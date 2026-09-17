//! PigASIO 的配置解析。
//!
//! 与 FlexASIO 最大的不同:这里的 `[[input]]` 和 `[[output]]` 是**数组**,
//! 可以出现任意多次,每个条目对应一个独立打开的音频设备。引擎会把所有
//! 输入设备的通道拼成 ASIO 的输入通道列表,输出同理。
//!
//! 一个最小配置长这样:
//!
//! ```toml
//! sample_rate = 48000
//! buffer_size_samples = 512
//!
//! [[output]]
//! device = "Speakers (Realtek Audio)"
//!
//! [[output]]
//! device = "Digital Audio (S/PDIF)"
//! channels = [0, 1]
//!
//! [[input]]
//! device = "default"
//! channel_count = 2
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result, StreamKind};

/// 配置文件的默认文件名。
pub const CONFIG_FILE_NAME: &str = "PigASIO.toml";

/// 单个设备最多能选多少通道。
///
/// 真实声卡最多几十路(MADI 常见 64),256 已经很宽裕 —— 这个上界挡的是
/// **畸形配置**,不是硬件,理由见 [`Config::validate`]。
const MAX_CHANNELS_PER_STREAM: usize = 256;

/// 单个设备的增益上下限,单位 dB。
///
/// 120 dB 是 10^6 倍,远超任何实际用途,同时保证 `10^(x / 20)` 不会溢出
/// 成 `inf`(f32 要到约 ±770 dB 才会溢出)。理由见 [`Config::validate`]。
const MAX_GAIN_DB: f32 = 120.0;

/// 一帧里单个通道的采样类型。决定 ASIO 侧 `getChannelInfo()` 返回的格式。
///
/// # 目前只有 `Float32` 可用
///
/// 引擎内部以 `f32` 处理,而 ASIO 缓冲区(`AsioBufferSet`)也全是 `f32`
/// —— `createBuffers` 直接把 f32 指针交给宿主,**没有任何 f32↔整数转换**。
/// 所以 `Int32` / `Int24` / `Int16` 只是预留的枚举值:一旦放行,
/// `getChannelInfo()` 会把整数格式报给宿主,而宿主按那个字节宽度去解释
/// 一块 f32 内存,数值全错且不报任何错 —— 只在听感上是噪声或静音。
/// [`Config::validate`] 因此会拒绝它们,实现转换之前不要再放开。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AsioSampleType {
    #[default]
    Float32,
    /// 尚未实现,`validate()` 会拒绝。
    Int32,
    /// 尚未实现,`validate()` 会拒绝。
    Int24,
    /// 尚未实现,`validate()` 会拒绝。
    Int16,
}

impl AsioSampleType {
    /// 单个采样占用的字节数。
    pub fn size_of(self) -> usize {
        match self {
            AsioSampleType::Float32 | AsioSampleType::Int32 => 4,
            AsioSampleType::Int24 => 3,
            AsioSampleType::Int16 => 2,
        }
    }

    /// 对应的 ASIO `ASIOSampleType` 常量值,详见 asio.h。
    pub fn asio_code(self) -> i32 {
        match self {
            AsioSampleType::Int16 => 16,   // ASIOSTInt16LSB
            AsioSampleType::Int24 => 17,   // ASIOSTInt24LSB
            AsioSampleType::Int32 => 18,   // ASIOSTInt32LSB
            AsioSampleType::Float32 => 19, // ASIOSTFloat32LSB
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().replace(['_', '-'], "").as_str() {
            "float32" | "f32" => Ok(AsioSampleType::Float32),
            "int32" | "i32" => Ok(AsioSampleType::Int32),
            "int24" | "i24" => Ok(AsioSampleType::Int24),
            "int16" | "i16" => Ok(AsioSampleType::Int16),
            other => Err(Error::Config(format!(
                "未知的采样类型 “{other}”;目前只支持 float32"
            ))),
        }
    }
}

impl fmt::Display for AsioSampleType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            AsioSampleType::Float32 => "float32",
            AsioSampleType::Int32 => "int32",
            AsioSampleType::Int24 => "int24",
            AsioSampleType::Int16 => "int16",
        };
        f.write_str(s)
    }
}

/// 重采样质量。多设备之间必然存在时钟漂移,漂移补偿需要变速重采样。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResampleQuality {
    /// 不做任何重采样。仅当所有设备与 ASIO 采样率完全一致、且关闭漂移补偿时有效。
    /// 延迟最低,但设备时钟不一致时会周期性地爆音。
    None,
    /// 多项式插值,CPU 占用低,音质一般。
    Fast,
    /// 窗化 sinc 插值,默认选项,音质与开销平衡良好。
    #[default]
    Sinc,
}

impl ResampleQuality {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(ResampleQuality::None),
            "fast" | "poly" => Ok(ResampleQuality::Fast),
            "sinc" | "high" => Ok(ResampleQuality::Sinc),
            other => Err(Error::Config(format!(
                "未知的重采样质量 “{other}”,可选:none / fast / sinc"
            ))),
        }
    }
}

/// 设备引用方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceRef {
    /// 使用系统默认设备(Windows 上是默认的输入/输出端点)。
    Default,
    /// 显式声明不使用该方向的设备。
    None,
    /// 名字包含该子串的设备,大小写不敏感。
    ///
    /// 这是唯一的「按名字找设备」方式 —— 正则匹配已经移除。原因见下方
    /// `RawStream::finish` 的说明。
    Substring(String),
}

impl fmt::Display for DeviceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceRef::Default => f.write_str("default"),
            DeviceRef::None => f.write_str("none"),
            DeviceRef::Substring(s) => write!(f, "{s}"),
        }
    }
}

impl DeviceRef {
    /// 该引用是否表示“不需要设备”。
    pub fn is_none(&self) -> bool {
        matches!(self, DeviceRef::None)
    }
}

/// 从一个设备上选取哪些通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelSelection {
    /// 取设备的前 N 个通道。
    Count(usize),
    /// 取指定的通道索引(可以在设备上不连续,便于挑选特定物理接口)。
    List(Vec<usize>),
}

impl ChannelSelection {
    /// 展开成具体的通道索引列表。
    pub fn expand(&self) -> Vec<usize> {
        match self {
            ChannelSelection::Count(n) => (0..*n).collect(),
            ChannelSelection::List(v) => v.clone(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            ChannelSelection::Count(n) => *n,
            ChannelSelection::List(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// WASAPI 相关的后端选项。当前 cpal 后端只能提供共享模式,
/// 独占模式在 `docs/backends.md` 中说明了扩展方式。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WasapiOptions {
    /// 是否请求独占模式。
    pub exclusive: bool,
    /// 共享模式下是否允许 Windows 音频引擎做采样率转换。
    pub auto_convert: bool,
}

/// 单个输入或输出设备的配置。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamConfig {
    /// 设备引用。
    pub device: DeviceRef,
    /// 选用哪些通道。
    pub channels: ChannelSelection,
    /// 建议延迟(秒)。传给后端作为缓冲区的期望值。
    pub latency_seconds: Option<f64>,
    /// 应用到该设备所有通道的增益,单位分贝。
    pub gain_db: f32,
    /// WASAPI 选项。
    pub wasapi: WasapiOptions,
    /// 该流是否参与时钟主设备选举。
    pub clock_master: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            device: DeviceRef::Default,
            channels: ChannelSelection::Count(2),
            latency_seconds: None,
            gain_db: 0.0,
            wasapi: WasapiOptions::default(),
            clock_master: false,
        }
    }
}

impl StreamConfig {
    /// 把分贝增益换算成线性倍数。0 dB 返回 1.0。
    pub fn linear_gain(&self) -> f32 {
        if self.gain_db == 0.0 {
            1.0
        } else {
            10f32.powf(self.gain_db / 20.0)
        }
    }
}

/// 引擎行为选项。
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// 重采样质量。
    pub resample_quality: ResampleQuality,
    /// 是否启用时钟漂移补偿。关闭后各设备按标称速率运行,
    /// 一旦硬件时钟有偏差就会累积并最终溢出或欠载。
    pub drift_correction: bool,
    /// 漂移补偿允许的最大修正量,单位 ppm(百万分之一)。
    /// 这个值同时决定了重采样器可以工作的变速范围。
    pub max_drift_ppm: f64,
    /// 每个流环形缓冲区的目标水位,**单位毫秒**。
    ///
    /// 它决定了启动时要预热多少数据,以及稳态下维持多少缓冲。这一项
    /// **直接加在延迟上**:水位是多少毫秒,音频路径上就多出多少等待。
    ///
    /// # 为什么单位是时间而不是「几个缓冲区」
    ///
    /// 水位真正要保证的是**撑得住多久** —— 具体说,是撑得过设备回调的
    /// 抖动。而设备回调周期是由系统音频引擎定的,和 ASIO 缓冲区大小没有
    /// 关系(WASAPI 共享模式下普遍是 10 ms 一块)。
    ///
    /// 早先这里是「ASIO 缓冲区的倍数」,于是踩了个坑:用户为了降延迟把
    /// `buffer_size_samples` 从 1024 调到 128,水位从 64 ms 缩到 8 ms ——
    /// 而 8 ms 比一块设备的回调周期还短,**本来是为了降延迟,结果反而开始
    /// 欠载**。倍数看着没变,绝对时间缩了 8 倍,界面上又只有「3.00 个
    /// 缓冲区」,看不出 8 ms 意味着什么。
    ///
    /// 换成毫秒之后,这个量不再随缓冲区大小漂移:调小缓冲区就是纯粹地
    /// 降延迟,不会顺手把安全余量也抽走。
    ///
    /// 默认 30.0 ms 是个**保守起点**(约等于 3 块 10 ms 的设备回调),
    /// 不是下限:引擎不会背着你抬高它。欠载会在实时状态里以「欠载 N 帧」
    /// 的形式立刻显出来,那时候再往回调一点即可。
    pub watermark_ms: f64,
    /// 通道名里是否允许非 ASCII 字符(比如中文设备名)。
    ///
    /// ASIO 的通道名是 `char[32]`,协议从没规定过编码。PigASIO 按
    /// **UTF-8** 写入 —— 现在的宿主(Cantabile、REAPER、Ableton 等)普遍
    /// 按 UTF-8 解释,中文 Windows 上能正常显示中文设备名。
    ///
    /// 早先这里写的是系统 ANSI 代码页(中文 Windows 上是 GBK),结果在按
    /// UTF-8 读的宿主里中文全是乱码,所以改成了 UTF-8。
    ///
    /// 反过来,要是碰上只认老式 ANSI 代码页的宿主,把这一项设为 `false`:
    /// 通道名会退化成 `OUT 1 (dev2)` 这样的纯 ASCII 形式,用设备序号代替
    /// 设备名,任何编码下都不会出错。
    pub use_non_ascii_channel_names: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            resample_quality: ResampleQuality::Sinc,
            drift_correction: true,
            max_drift_ppm: 500.0,
            watermark_ms: 30.0,
            use_non_ascii_channel_names: true,
        }
    }
}

/// 完整配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// ASIO 采样率。所有设备都会尽量以此速率打开。
    pub sample_rate: u32,
    /// ASIO 缓冲区大小,单位是采样帧。
    pub buffer_size_samples: u32,
    /// ASIO 侧暴露给宿主的采样类型。
    pub asio_sample_type: AsioSampleType,
    /// 输入设备列表,按配置顺序拼接成 ASIO 输入通道。
    pub inputs: Vec<StreamConfig>,
    /// 输出设备列表,按配置顺序拼接成 ASIO 输出通道。
    pub outputs: Vec<StreamConfig>,
    /// 引擎选项。
    pub engine: EngineConfig,
}

impl Default for Config {
    /// 没有任何配置文件时的默认行为,与 FlexASIO 的开箱即用体验对齐:
    /// 默认播放设备 + 默认录音设备,各取前两个通道。
    fn default() -> Self {
        Config {
            sample_rate: 48_000,
            buffer_size_samples: 1024,
            asio_sample_type: AsioSampleType::Float32,
            inputs: vec![StreamConfig {
                channels: ChannelSelection::Count(2),
                ..StreamConfig::default()
            }],
            outputs: vec![StreamConfig {
                channels: ChannelSelection::Count(2),
                ..StreamConfig::default()
            }],
            engine: EngineConfig::default(),
        }
    }
}

impl Config {
    /// 从 TOML 文本解析并校验。
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|e| Error::Config(format!("解析 TOML 失败:{e}")))?;
        raw.finish()
    }

    /// 从文件解析。
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("读取配置文件 {} 失败:{e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// 输入通道总数 —— 也就是 ASIO `getChannels()` 返回的输入通道数。
    pub fn total_input_channels(&self) -> usize {
        self.inputs
            .iter()
            .filter(|s| !s.device.is_none())
            .map(|s| s.channels.len())
            .sum()
    }

    /// 输出通道总数。
    pub fn total_output_channels(&self) -> usize {
        self.outputs
            .iter()
            .filter(|s| !s.device.is_none())
            .map(|s| s.channels.len())
            .sum()
    }

    /// 生效的输入流(过滤掉 `device = "none"` 的条目)。
    pub fn active_inputs(&self) -> impl Iterator<Item = (usize, &StreamConfig)> {
        self.inputs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.device.is_none())
    }

    /// 生效的输出流。
    pub fn active_outputs(&self) -> impl Iterator<Item = (usize, &StreamConfig)> {
        self.outputs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.device.is_none())
    }

    /// 全量校验。返回第一个发现的问题。
    pub fn validate(&self) -> Result<()> {
        if self.sample_rate < 8_000 || self.sample_rate > 768_000 {
            return Err(Error::Config(format!(
                "sample_rate = {} 超出合理范围(8000..=768000)",
                self.sample_rate
            )));
        }
        if self.buffer_size_samples < 16 || self.buffer_size_samples > 65_536 {
            return Err(Error::Config(format!(
                "buffer_size_samples = {} 超出合理范围(16..=65536)",
                self.buffer_size_samples
            )));
        }
        if !self.buffer_size_samples.is_power_of_two() {
            // 不是硬性要求,但 ASIO 宿主普遍偏好 2 的幂;
            // 有些宿主会直接拒绝非 2 的幂的 buffer size。
            log::warn!(
                "buffer_size_samples = {} 不是 2 的幂,部分宿主可能拒绝该设置",
                self.buffer_size_samples
            );
        }
        // ASIO 缓冲区内部是 f32,而驱动没有任何 f32↔整数转换(见
        // `AsioSampleType` 的说明)。放行整数格式会让宿主按错误的字节宽度
        // 去解释那块内存:数值全错,却不出任何错 —— 只在听感上表现为噪声
        // 或静音。宁可在这里明确拒绝,也不要静默损坏音频。
        if self.asio_sample_type != AsioSampleType::Float32 {
            return Err(Error::Config(format!(
                "asio_sample_type = \"{}\" 暂不支持:ASIO 侧目前只暴露 float32",
                self.asio_sample_type
            )));
        }
        // `is_finite` 顺手挡掉 NaN 和 inf —— 只写区间比较的话 NaN 会溜过去
        // (NaN 和任何数比都是 false)。
        let watermark_ms = self.engine.watermark_ms;
        if !watermark_ms.is_finite() || watermark_ms <= 0.0 || watermark_ms > 2000.0 {
            return Err(Error::Config(format!(
                "engine.watermark_ms = {} 超出合理范围(0, 2000] 毫秒",
                self.engine.watermark_ms
            )));
        }
        if self.engine.max_drift_ppm <= 0.0 || self.engine.max_drift_ppm > 100_000.0 {
            return Err(Error::Config(
                "engine.max_drift_ppm 必须落在 (0, 100000] 区间".into(),
            ));
        }

        if self.total_input_channels() == 0 && self.total_output_channels() == 0 {
            return Err(Error::Config(
                "配置里没有任何可用的输入或输出设备;至少要有一个 [[input]] 或 [[output]]".into(),
            ));
        }

        for (kind, streams) in [
            (StreamKind::Input, &self.inputs),
            (StreamKind::Output, &self.outputs),
        ] {
            for (i, s) in streams.iter().enumerate() {
                if s.device.is_none() {
                    continue;
                }
                let label = format!("第 {} 个{kind}设备", i + 1);
                if s.channels.is_empty() {
                    return Err(Error::Config(format!(
                        "{label} 的通道选择为空;请用 channels = [0, 1] 或 channel_count = 2"
                    )));
                }
                // 通道数上限。挡的是畸形配置:`ChannelSelection::Count(n)` 会
                // 在下面展开成 `(0..n)` 的 Vec,若 n 写成 10^12,光是校验就会
                // 申请 8 TB 内存、把宿主进程直接 abort —— 驱动跑在宿主里,那
                // 等于连用户没保存的工程一起带走。`len()` 不分配,所以这道
                // 检查本身是安全的,必须发生在 `expand()` 之前。
                if s.channels.len() > MAX_CHANNELS_PER_STREAM {
                    return Err(Error::Config(format!(
                        "{label} 选了 {} 个通道,超过单设备上限 {MAX_CHANNELS_PER_STREAM}",
                        s.channels.len()
                    )));
                }
                let mut seen = std::collections::HashSet::new();
                for ch in s.channels.expand() {
                    if !seen.insert(ch) {
                        return Err(Error::Config(format!("{label} 的通道 {ch} 被重复选择")));
                    }
                }
                if let Some(lat) = s.latency_seconds {
                    if !(0.0..=1.0).contains(&lat) {
                        return Err(Error::Config(format!(
                            "{label} 的 latency = {lat} 超出范围(0.0..=1.0 秒)"
                        )));
                    }
                }
                // 既要有限,也要在范围内。上界挡的是 `10^(gain_db / 20)` 的
                // 溢出:`gain_db = 1000` 是有限值,但线性增益算出来是 `+inf`,
                // 乘到音频上会得到 `inf`,而静音样本 `0.0 * inf` 是 `NaN` ——
                // 两个都不是能听的东西,而且不出任何错。
                if !s.gain_db.is_finite() || s.gain_db.abs() > MAX_GAIN_DB {
                    return Err(Error::Config(format!(
                        "{label} 的 gain_db = {} 超出范围(±{MAX_GAIN_DB} dB)",
                        s.gain_db
                    )));
                }
            }
        }

        // 时钟主设备最多只能有一个,否则引擎无法确定时间基准。
        let masters: Vec<_> = self
            .inputs
            .iter()
            .enumerate()
            .map(|(i, s)| (StreamKind::Input, i, s))
            .chain(
                self.outputs
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (StreamKind::Output, i, s)),
            )
            .filter(|(_, _, s)| s.clock_master && !s.device.is_none())
            .collect();
        if masters.len() > 1 {
            return Err(Error::Config(format!(
                "有 {} 个设备被标记为 clock_master = true,只能有一个",
                masters.len()
            )));
        }

        Ok(())
    }

    /// 选出时钟主设备。若配置没有显式指定,则优先取第一个输出设备
    /// —— 这与绝大多数宿主的直觉一致(播放设备是时间基准)。
    /// 没有输出设备时退回第一个输入设备。
    pub fn clock_master(&self) -> Option<(StreamKind, usize)> {
        for (i, s) in self.outputs.iter().enumerate() {
            if s.clock_master && !s.device.is_none() {
                return Some((StreamKind::Output, i));
            }
        }
        for (i, s) in self.inputs.iter().enumerate() {
            if s.clock_master && !s.device.is_none() {
                return Some((StreamKind::Input, i));
            }
        }
        self.active_outputs()
            .next()
            .map(|(i, _)| (StreamKind::Output, i))
            .or_else(|| {
                self.active_inputs()
                    .next()
                    .map(|(i, _)| (StreamKind::Input, i))
            })
    }
}

// ---------------------------------------------------------------------------
// TOML 反序列化的中间表示
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    sample_rate: Option<u32>,
    buffer_size_samples: Option<u32>,
    asio_sample_type: Option<String>,
    engine: Option<RawEngine>,

    #[serde(default)]
    input: Vec<RawStream>,
    #[serde(default)]
    output: Vec<RawStream>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEngine {
    resample_quality: Option<String>,
    drift_correction: Option<bool>,
    max_drift_ppm: Option<f64>,
    use_non_ascii_channel_names: Option<bool>,
    /// 目标水位,单位毫秒。
    watermark_ms: Option<f64>,
    /// 旧字段:目标水位,单位是 ASIO 缓冲区的倍数。
    ///
    /// 被 [`watermark_ms`](Self::watermark_ms) 取代。这个单位会随
    /// `buffer_size_samples` 一起缩放,调小缓冲区时会把安全余量一起抽走,
    /// 是个坑(见 `EngineConfig::watermark_ms` 的说明)。仍然接受,是为了
    /// 让老配置文件不报错 —— 解析时按当时的缓冲区大小和采样率折算成毫秒,
    /// 保持**行为不变**,同时打一条日志请用户改成新字段。
    buffer_watermark: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStream {
    device: Option<String>,

    channels: Option<RawChannels>,
    channel_count: Option<usize>,

    latency: Option<f64>,
    latency_ms: Option<f64>,
    gain_db: Option<f32>,
    clock_master: Option<bool>,

    wasapi: Option<RawWasapi>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWasapi {
    exclusive: Option<bool>,
    auto_convert: Option<bool>,
}

/// `channels = 2` 与 `channels = [0, 3]` 都合法。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawChannels {
    Count(usize),
    List(Vec<usize>),
}

impl RawConfig {
    fn finish(self) -> Result<Config> {
        let default = Config::default();

        // 没写 `[[input]]` / `[[output]]` 时沿用默认设备,
        // 但一旦用户显式写了任何一个,就完全以用户写的为准。
        let inputs = if self.input.is_empty() {
            if self.output.is_empty() {
                default.inputs.clone()
            } else {
                // 用户只配了输出,说明他不想要输入。
                Vec::new()
            }
        } else {
            self.input
                .iter()
                .map(|s| s.clone().finish(StreamKind::Input))
                .collect::<Result<Vec<_>>>()?
        };

        let outputs = if self.output.is_empty() {
            if self.input.is_empty() {
                default.outputs.clone()
            } else {
                Vec::new()
            }
        } else {
            self.output
                .iter()
                .map(|s| s.clone().finish(StreamKind::Output))
                .collect::<Result<Vec<_>>>()?
        };

        // 先定下采样率和缓冲区大小 —— 旧版 `buffer_watermark` 折算成毫秒
        // 要用到这两个值。
        let sample_rate = self.sample_rate.unwrap_or(default.sample_rate);
        let buffer_size_samples = self
            .buffer_size_samples
            .unwrap_or(default.buffer_size_samples);

        let engine = match self.engine {
            None => EngineConfig::default(),
            Some(e) => EngineConfig {
                resample_quality: match e.resample_quality {
                    Some(s) => ResampleQuality::parse(&s)?,
                    None => EngineConfig::default().resample_quality,
                },
                drift_correction: e
                    .drift_correction
                    .unwrap_or(EngineConfig::default().drift_correction),
                max_drift_ppm: e
                    .max_drift_ppm
                    .unwrap_or(EngineConfig::default().max_drift_ppm),
                watermark_ms: match (e.watermark_ms, e.buffer_watermark) {
                    (Some(_), Some(_)) => {
                        return Err(Error::Config(
                            "engine.watermark_ms 和 engine.buffer_watermark 不能同时写;\
                             buffer_watermark 是旧字段,请改用 watermark_ms"
                                .into(),
                        ))
                    }
                    (Some(ms), None) => ms,
                    (None, Some(buffers)) => {
                        let ms = buffers * buffer_size_samples as f64 / sample_rate.max(1) as f64
                            * 1000.0;
                        log::warn!(
                            "engine.buffer_watermark = {buffers} 是旧写法,已按当前配置\
                             (缓冲区 {buffer_size_samples} 帧 @ {sample_rate} Hz)折算成 \
                             {ms:.1} ms。请把这一行改成 `watermark_ms = {ms:.1}` —— \
                             「几个缓冲区」这个单位会随 buffer_size_samples 一起缩放,\
                             调小缓冲区时会把安全余量一起抽走。"
                        );
                        ms
                    }
                    (None, None) => EngineConfig::default().watermark_ms,
                },
                use_non_ascii_channel_names: e
                    .use_non_ascii_channel_names
                    .unwrap_or(EngineConfig::default().use_non_ascii_channel_names),
            },
        };

        let config = Config {
            sample_rate,
            buffer_size_samples,
            asio_sample_type: match self.asio_sample_type {
                Some(s) => AsioSampleType::parse(&s)?,
                None => default.asio_sample_type,
            },
            inputs,
            outputs,
            engine,
        };

        config.validate()?;
        Ok(config)
    }
}

impl RawStream {
    fn clone(&self) -> Self {
        RawStream {
            device: self.device.clone(),
            channels: self.channels.as_ref().map(|c| match c {
                RawChannels::Count(n) => RawChannels::Count(*n),
                RawChannels::List(v) => RawChannels::List(v.clone()),
            }),
            channel_count: self.channel_count,
            latency: self.latency,
            latency_ms: self.latency_ms,
            gain_db: self.gain_db,
            clock_master: self.clock_master,
            wasapi: self.wasapi.as_ref().map(|w| RawWasapi {
                exclusive: w.exclusive,
                auto_convert: w.auto_convert,
            }),
        }
    }

    fn finish(self, kind: StreamKind) -> Result<StreamConfig> {
        // `device` 是唯一的设备选择字段。早先还有一个 `device_regex`
        // 与它二选一,但控制面板只认识字符串:载入时把正则渲染成
        // `"^麦克风"`,保存时又当成普通子串写回去,匹配语义被悄悄换掉,
        // 用户直到设备打不开才发现。与其维护一个两边对不齐的功能,
        // 不如只留子串匹配 —— 多设备场景里它够用,而且行为可预期。
        let device = match self.device.as_deref() {
            Some(name) => match name.trim().to_ascii_lowercase().as_str() {
                "default" | "" => DeviceRef::Default,
                "none" | "null" | "disabled" => DeviceRef::None,
                _ => DeviceRef::Substring(name.to_string()),
            },
            None => DeviceRef::Default,
        };

        let channels = match (self.channels, self.channel_count) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(
                    "同一个设备条目里不能同时写 channels 和 channel_count".into(),
                ))
            }
            (Some(RawChannels::Count(n)), None) => ChannelSelection::Count(n),
            (Some(RawChannels::List(v)), None) => ChannelSelection::List(v),
            (None, Some(n)) => ChannelSelection::Count(n),
            (None, None) => ChannelSelection::Count(2),
        };

        let latency_seconds = match (self.latency, self.latency_ms) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(
                    "同一个设备条目里不能同时写 latency 和 latency_ms".into(),
                ))
            }
            (Some(sec), None) => Some(sec),
            (None, Some(ms)) => Some(ms / 1000.0),
            (None, None) => None,
        };

        let _ = kind; // 目前两个方向的解析规则一致,保留参数便于将来分化。

        Ok(StreamConfig {
            device,
            channels,
            latency_seconds,
            gain_db: self.gain_db.unwrap_or(0.0),
            wasapi: self
                .wasapi
                .map_or_else(WasapiOptions::default, |w| WasapiOptions {
                    exclusive: w.exclusive.unwrap_or(false),
                    auto_convert: w.auto_convert.unwrap_or(true),
                }),
            clock_master: self.clock_master.unwrap_or(false),
        })
    }
}

// ---------------------------------------------------------------------------
// 配置文件查找
// ---------------------------------------------------------------------------

/// 按优先级列出候选配置路径,并返回第一个实际存在的文件。
///
/// 顺序:
/// 1. 环境变量 `PIGASIO_CONFIG` 指定的路径(存在即用,不回退)
/// 2. `host_exe_dir/PigASIO.toml` —— 宿主软件自己的目录
/// 3. `~/PigASIO.toml` —— 用户目录,对所有宿主生效
///
/// `host_exe_dir` 由调用方提供:在驱动 DLL 里它是宿主进程的 exe 目录,
/// 在命令行工具里则是当前工作目录。
pub fn find_config_file(host_exe_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PIGASIO_CONFIG") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
        log::warn!(
            "PIGASIO_CONFIG 指向的 {} 不存在,继续查找其他位置",
            p.display()
        );
    }

    if let Some(dir) = host_exe_dir {
        let candidate = dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    let candidate = home.join(CONFIG_FILE_NAME);
    if candidate.is_file() {
        return Some(candidate);
    }

    None
}

/// 载入配置:找到文件就解析,找不到就用默认配置。
pub fn load(host_exe_dir: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
    match find_config_file(host_exe_dir) {
        Some(path) => {
            log::info!("使用配置文件:{}", path.display());
            let config = Config::from_file(&path)?;
            Ok((config, Some(path)))
        }
        None => {
            log::info!("未找到 PigASIO.toml,使用默认配置(默认输入/输出设备,各 2 通道)");
            let config = Config::default();
            config.validate()?;
            Ok((config, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认水位是三十毫秒() {
        assert_eq!(EngineConfig::default().watermark_ms, 30.0);
    }

    #[test]
    fn 水位只认毫秒不认缓冲区大小() {
        // 同样的 watermark_ms,配上不同的 buffer_size_samples,应该是同一段时间。
        // 这条是这次改单位的全部意义:以前调小缓冲区会把水位一起缩掉。
        let mut seen = Vec::new();
        for buffer in [128, 512, 1024] {
            let cfg = Config::from_toml_str(&format!(
                "buffer_size_samples = {buffer}\n[engine]\nwatermark_ms = 25.0\n"
            ))
            .unwrap();
            assert_eq!(cfg.engine.watermark_ms, 25.0);
            seen.push(cfg.buffer_size_samples);
        }
        assert_eq!(seen, vec![128, 512, 1024]);
    }

    #[test]
    fn 旧字段_buffer_watermark_按当时配置折算成毫秒() {
        // 缓冲区 128 帧、3 个缓冲区,折算出来是 8 ms —— 正是那个「比设备块
        // 还薄、必然欠载」的配置。折算的意义是**保持原行为**,不去猜用户
        // 本来想要多少,免得静默改掉他调好的延迟。
        let cfg = Config::from_toml_str(
            "sample_rate = 48000\nbuffer_size_samples = 128\n[engine]\nbuffer_watermark = 3.0\n",
        )
        .unwrap();
        assert_eq!(cfg.engine.watermark_ms, 8.0);

        // 默认缓冲区 1024 配 3.0 则是 64 ms。
        let cfg = Config::from_toml_str("[engine]\nbuffer_watermark = 3.0\n").unwrap();
        assert_eq!(cfg.engine.watermark_ms, 64.0);
    }

    #[test]
    fn 新旧水位字段不能同时出现() {
        let err = Config::from_toml_str(
            "sample_rate = 48000\n[engine]\nwatermark_ms = 30.0\nbuffer_watermark = 3.0\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("watermark_ms") && msg.contains("buffer_watermark"),
            "{msg}"
        );
    }

    #[test]
    fn 水位超出合理范围会被拒绝() {
        for bad in ["0.0", "-1.0", "5000.0", "nan"] {
            let text = format!("[engine]\nwatermark_ms = {bad}\n");
            match Config::from_toml_str(&text) {
                Ok(cfg) => panic!(
                    "watermark_ms = {bad} 本该被拒绝,却解析出 {}",
                    cfg.engine.watermark_ms
                ),
                Err(e) => assert!(e.to_string().contains("watermark_ms"), "{bad}: {e}"),
            }
        }
        // 边界值本身合法。
        assert!(Config::from_toml_str("[engine]\nwatermark_ms = 2000.0\n").is_ok());
        assert!(Config::from_toml_str("[engine]\nwatermark_ms = 1.0\n").is_ok());
    }

    #[test]
    fn 只接受_float32_采样类型() {
        // float32 是唯一真正实现的格式。
        assert!(Config::from_toml_str("asio_sample_type = \"float32\"\n").is_ok());
        // 不写就用默认值,也必须是 float32。
        assert_eq!(
            Config::from_toml_str("sample_rate = 48000\n")
                .unwrap()
                .asio_sample_type,
            AsioSampleType::Float32
        );

        // 整数格式尚未实现:引擎的 ASIO 缓冲区是 f32,而驱动没有任何
        // f32↔整数转换。放行它们会让 `getChannelInfo()` 把整数格式报给宿主,
        // 宿主于是按错误的字节宽度解释那块 f32 内存 —— 数值全错却不出任何
        // 错。必须在解析阶段就拒绝,不能等到用户听出噪声。
        for bad in ["int32", "int24", "int16", "i32", "i24", "i16"] {
            let text = format!("asio_sample_type = \"{bad}\"\n");
            match Config::from_toml_str(&text) {
                Ok(_) => panic!("asio_sample_type = {bad} 本该被拒绝,却解析成功了"),
                Err(e) => assert!(
                    e.to_string().contains("asio_sample_type"),
                    "{bad} 的错误信息应点明是哪一项:{e}"
                ),
            }
        }
    }

    #[test]
    fn 通道数超上限会被拒绝而不是撑爆内存() {
        // channel_count 写成一个天文数字。如果 validate() 先 expand 再检查,
        // 这里会申请 TB 级内存、直接 abort,测试根本跑不完 —— 能拿到 Err
        // 就说明检查发生在分配之前。
        let text = "[[output]]\ndevice = \"default\"\nchannel_count = 1000000000000\n";
        let err = Config::from_toml_str(text).unwrap_err();
        assert!(err.to_string().contains("上限"), "{err}");

        // channels 列表按长度受同样的约束。
        let list = (0..MAX_CHANNELS_PER_STREAM + 1)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!("[[output]]\ndevice = \"default\"\nchannels = [{list}]\n");
        let err = Config::from_toml_str(&text).unwrap_err();
        assert!(err.to_string().contains("上限"), "{err}");

        // 上限本身仍然合法。
        let text = format!(
            "[[output]]\ndevice = \"default\"\nchannel_count = {MAX_CHANNELS_PER_STREAM}\n"
        );
        assert!(Config::from_toml_str(&text).is_ok());
    }

    #[test]
    fn 增益超出范围会被拒绝() {
        // 有限但过大:`10^(1000/20)` 溢出成 inf,乘到音频上会得到 inf,而
        // 静音样本 `0.0 * inf` 是 NaN。nan/inf 本身也不是有限值,同样要拒。
        for bad in ["1000.0", "-1000.0", "121.0", "nan", "inf", "-inf"] {
            let text = format!("[[output]]\ndevice = \"default\"\ngain_db = {bad}\n");
            match Config::from_toml_str(&text) {
                Ok(_) => panic!("gain_db = {bad} 本该被拒绝,却解析成功了"),
                Err(e) => assert!(e.to_string().contains("gain_db"), "{bad}: {e}"),
            }
        }

        // 边界值本身合法,而且算出来的线性增益必须是有限的。
        for ok in ["0.0", "120.0", "-120.0", "6.0"] {
            let text = format!("[[output]]\ndevice = \"default\"\ngain_db = {ok}\n");
            let cfg = Config::from_toml_str(&text).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert!(cfg.outputs[0].linear_gain().is_finite(), "{ok} 的增益算出了非有限值");
        }
    }
}
