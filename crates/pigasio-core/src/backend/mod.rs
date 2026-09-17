//! 音频后端抽象。
//!
//! 引擎不直接依赖任何具体的音频 API:它只认这里的 [`Backend`] /
//! [`DeviceHandle`] / [`StreamHandle`] 三个 trait。现有两个实现:
//!
//! * cpal(见 [`cpal_backend`])—— 默认,行为与改动前完全一致;
//! * 直写 WASAPI(见 `wasapi`)—— 用 `PIGASIO_BACKEND=wasapi` 启用,
//!   或者 `=auto` 让它自动挑。目前还只是"行为对齐 cpal"(默认 period),
//!   切过去没有收益,所以默认不启用 —— 它的意义在于阶段 3 能把 period
//!   降下来,见 `docs/low-latency-wasapi.md`。
//!
//! # 这一层为什么存在
//!
//! 不是为了"可插拔"这个名头,而是因为 **cpal 拿不到 WASAPI 的低延迟共享
//! 模式**:它只用旧接口 `IAudioClient::Initialize`,而把 period 降到设备
//! 允许的最小值需要 `IAudioClient3::InitializeSharedAudioStream`。cpal 的
//! `Device` 内部虽然持有 `IMMDevice`,却没有公开访问器,所以没法在它之上
//! 打补丁 —— 只能绕开它另写一个后端。这一层就是为那个后端留的位置。
//!
//! # 回调约定
//!
//! 设备原生格式与引擎内部格式(`f32`)之间的转换**由后端负责**:回调收到的
//! 永远是交错的 `f32`。这样引擎侧不必为每种样本格式各写一份泛型代码,
//! 新增后端也不用碰引擎。

use std::sync::Arc;

use crate::error::{Result, StreamKind};

mod cpal_backend;

#[cfg(windows)]
mod wasapi;

pub use cpal_backend::CpalBackend;

#[cfg(windows)]
pub use wasapi::WasapiBackend;

/// 设备端单个样本的格式。
///
/// 引擎内部一律用 `f32`,这个枚举只说明设备原生是什么 —— 转换在后端完成。
/// cpal 后端需要它来分派泛型的 `build_*_stream`;WASAPI 共享模式固定是 f32。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceSampleFormat {
    F32,
    I16,
    I32,
    U16,
}

/// 协商好的流格式。
#[derive(Debug, Clone)]
pub struct StreamFormat {
    /// 实际使用的采样率。设备不支持请求值时,后端会夹到最接近的支持范围。
    pub sample_rate: u32,
    /// 设备提供的通道数 —— 回调里交错数据的步长。
    ///
    /// 这是**设备**的通道数,不是配置里选用的通道数:配置可能只要其中两个,
    /// 那种筛选由引擎在回调里做。
    pub channels: usize,
    /// 设备原生的样本格式。引擎不关心它,只是转交给后端的 `open_*`。
    pub sample_format: DeviceSampleFormat,
}

/// 打开流时的请求。具体格式由后端协商。
#[derive(Debug, Clone, Copy)]
pub struct StreamRequest {
    /// 期望的采样率。设备不支持时后端可以向下调整。
    pub sample_rate: u32,
}

/// 输入回调:后端把设备数据转成 `f32` 交错后交过来。
pub type InputCallback = Box<dyn FnMut(&[f32]) + Send>;
/// 输出回调:后端从这块缓冲取数据送去设备。
pub type OutputCallback = Box<dyn FnMut(&mut [f32]) + Send>;
/// 错误回调。参数是给用户看的说明。
pub type ErrorCallback = Box<dyn FnMut(String) + Send>;

/// 一个已经打开的流。
///
/// **故意不加 `Send`**:底层的流句柄(比如 `cpal::Stream`)本来就不可跨线程
/// 移动 —— 它必须在创建自己的那条线程里启动、暂停、销毁。引擎因此把流的
/// 整个生命周期关在一个专用线程内(见 `engine::StreamHost`),从不把它传出去。
/// 这里要是硬加 `Send`,反而是在掩盖这个约束。
pub trait StreamHandle {
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
}

/// 后端持有的设备句柄。
pub trait DeviceHandle: Send + Sync {
    /// 设备在系统里显示的名字。报错时要用。
    fn name(&self) -> &str;

    /// 协商出一个实际可用的格式。纯查询,不打开流。
    fn negotiate(&self, kind: StreamKind, request: &StreamRequest) -> Result<StreamFormat>;

    fn open_input(
        &self,
        format: &StreamFormat,
        on_data: InputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>>;

    fn open_output(
        &self,
        format: &StreamFormat,
        on_data: OutputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>>;
}

/// 一次枚举得到的设备信息。
///
/// `handle` 是引用计数的句柄,所以克隆整条记录很便宜。
#[derive(Clone)]
pub struct DeviceInfo {
    /// 设备在系统里显示的名字。
    pub name: String,
    /// 该设备在查询方向上提供的最大通道数。
    pub max_channels: usize,
    /// 设备偏好的采样率(展示与诊断用)。
    pub default_sample_rate: u32,
    /// 打开设备用的句柄。
    pub handle: Arc<dyn DeviceHandle>,
}

impl std::fmt::Debug for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceInfo")
            .field("name", &self.name)
            .field("max_channels", &self.max_channels)
            .field("default_sample_rate", &self.default_sample_rate)
            .finish_non_exhaustive()
    }
}

/// 一个音频后端。
pub trait Backend: Send + Sync {
    /// 后端的名字,用于日志与诊断。
    fn name(&self) -> &'static str;

    fn enumerate(&self, kind: StreamKind) -> Result<Vec<DeviceInfo>>;

    fn default_device(&self, kind: StreamKind) -> Result<DeviceInfo>;
}

/// 当前使用的后端。
///
/// 选择规则:
/// * `PIGASIO_BACKEND=cpal` 或 `=wasapi` —— 显式指定;
/// * `PIGASIO_BACKEND=auto` —— 优先 WASAPI,它在这台机器上不可用时退回 cpal;
/// * 不设置 —— **先用 cpal**。
///
/// 默认之所以还是 cpal:WASAPI 后端目前只做到"行为对齐 cpal"(默认 period),
/// 切过去没有任何收益,却要承担一套新代码的风险。等阶段 3 把低延迟 period
/// 接上、真机跑稳之后再改默认。见 `docs/low-latency-wasapi.md`。
pub fn current() -> &'static dyn Backend {
    static BACKEND: std::sync::OnceLock<Box<dyn Backend>> = std::sync::OnceLock::new();
    // 注意要多解一层:`&Box<dyn Backend>` 不是 `&dyn Backend`(标准库没有给
    // `Box<dyn Trait>` 实现 `Trait`)。
    &**BACKEND.get_or_init(select_backend)
}

/// 按上面的规则挑一个后端。
#[cfg(windows)]
fn select_backend() -> Box<dyn Backend> {
    // 顺手容忍大小写和首尾空格 —— 环境变量本来就容易写歪,没必要在这种
    // 地方挑刺,写错了还给个明确的警告。
    let requested = std::env::var("PIGASIO_BACKEND")
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();

    let backend: Box<dyn Backend> = match requested.as_str() {
        "" | "cpal" => Box::new(CpalBackend::new()),
        "wasapi" => Box::new(WasapiBackend::new()),
        "auto" => {
            let wasapi = WasapiBackend::new();
            if wasapi.available() {
                Box::new(wasapi)
            } else {
                log::warn!("WASAPI 后端在这台机器上不可用,退回 cpal");
                Box::new(CpalBackend::new())
            }
        }
        other => {
            log::warn!(
                "PIGASIO_BACKEND 的值 “{other}” 无法识别(可选 cpal / wasapi / auto),按 cpal 处理"
            );
            Box::new(CpalBackend::new())
        }
    };
    // 无论走哪条分支都记一条 —— 排查"到底用的哪个后端"时,这条就是答案。
    log::info!("音频后端:{}", backend.name());
    backend
}

#[cfg(not(windows))]
fn select_backend() -> Box<dyn Backend> {
    let backend: Box<dyn Backend> = Box::new(CpalBackend::new());
    log::info!("音频后端:{}", backend.name());
    backend
}
