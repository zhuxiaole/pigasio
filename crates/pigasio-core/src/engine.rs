//! 多设备音频引擎。
//!
//! # 职责
//!
//! 把「N 个输入设备 + M 个输出设备」抽象成 ASIO 宿主眼中的「一个设备,
//! 有 N 路输入、M 路输出」:
//!
//! 1. 按配置为每个设备打开一条独立的流;
//! 2. 用环形缓冲区把设备时钟域和 ASIO 时钟域解耦;
//! 3. 选一个「时钟主设备」,它的回调充当整个驱动的时间基准;
//! 4. 其余设备通过变速重采样,把自身时钟软锁到主设备上。
//!
//! # 线程模型
//!
//! ```text
//!   时钟主设备回调线程 ──┐
//!                       │ try_lock
//!                       ▼
//!                 ┌───────────┐
//!                 │ AudioCore │  持有所有流的 ring 端 + ASIO 缓冲 + 宿主回调
//!                 └───────────┘
//!                       │ 调用
//!                       ▼
//!                 host.bufferSwitch(index)
//!
//!   从设备回调线程 ──▶ 只碰自己那条流的 ring 的另一端(完全无锁)
//!
//!   宿主主线程 ──▶ start/stop,也通过 AudioCore 的锁同步
//! ```
//!
//! # 加锁的边界
//!
//! `AudioCore` 的锁只在两个地方拿:时钟主设备的回调,以及 `start`/`stop`
//! 这类控制接口。后者在音频运行期间几乎不发生,所以 `try_lock` 基本总是
//! 立即成功。
//!
//! **有一条硬性约束**:任何可能在 `bufferSwitch` 内部被宿主调用的 ASIO
//! 接口(`getSamplePosition`、`outputReady`、`getChannelInfo` 等)**绝不能**
//! 去拿这把锁 —— 宿主完全可能在自己的 `bufferSwitch` 处理里回头调用它们,
//! 而 `parking_lot::Mutex` 不可重入,那样会直接死锁。这些接口改用原子变量。
//!
//! # 为什么设备流要放在专用线程上
//!
//! 底层的流句柄不是 `Send`(比如 `cpal::Stream` 内部按平台持有不可跨线程的
//! 句柄),所以 [`crate::backend::StreamHandle`] 也刻意不带 `Send` 约束。
//! 而引擎必须能被宿主的任意线程访问。于是这里把「创建/启动/停止/销毁设备流」
//! 全部收拢到一个专用线程,引擎本体只保留一个命令通道,从而对宿主表现为
//! 完全线程安全。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;

use crate::backend::{
    DeviceHandle, ErrorCallback, InputCallback, OutputCallback, StreamFormat, StreamHandle,
    StreamRequest,
};
use crate::channel_name::ChannelNames;
use crate::config::{Config, ResampleQuality, StreamConfig as PigStreamConfig};
use crate::devices::{self, DeviceInfo};
use crate::drift::DriftController;
use crate::error::{Error, Result, StreamKind};
use crate::resample::{allocate_planes, FixedInResampler, FixedOutResampler, ResamplerSpec};
use crate::ring::{self, FrameReader, FrameWriter, RingStats, RingStatsSnapshot};

/// 环形缓冲容量相对于目标水位的倍数。
const RING_CAPACITY_FACTOR: f64 = 6.0;
/// 环形缓冲至少容纳的时长(秒)。这是**容量**的下限,不是水位的 —— 水位
/// 本身没有下限,用户填多少就是多少(见 `target_watermark_frames`)。
const RING_MIN_SECONDS: f64 = 0.25;
/// 一次时钟推进最多连续处理的 ASIO 缓冲区个数。
///
/// 注意它**不是**"每轮该处理多少"—— 那是设备块说了算的。比如设备块 1056 帧
/// 配 128 帧的 ASIO 缓冲,一轮就该处理 8 个。这个常量只是道**安全阀**,防的是
/// `accumulated` 因为异常暴涨时在这里空转太久。
///
/// 早先取的 4 是个错误:设备块一旦超过 4 个 ASIO 缓冲(512 帧),引擎每轮就
/// 跟不上了 —— 消费只有生产的一半,环形缓冲一路积到容量上限(250 ms),而漂移
/// 补偿那 ±0.5% 的修正量对这种量级的缺口杯水车薪。实测的 1056 帧设备块正好
/// 踩在这个坑上。
const MAX_BUFFERS_PER_ADVANCE: usize = 64;

/// 查状态时愿意为 `core` 锁等多久。
///
/// 见 [`Engine::status`]:等一小会儿比一次不等更实际。20 ms 远小于控制面板
/// 200 ms 的刷新周期,不会把界面拖慢。
const STATUS_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(20);

/// 宿主提供的缓冲区交换回调。
///
/// 引擎在输入数据就绪后调用它,调用方应当在回调里读输入、写输出。
/// 第一个参数是双缓冲本体,第二个是 double buffer 的下标(0 或 1)。
pub type BufferSwitchCallback = Box<dyn FnMut(&mut AsioBufferSet, usize) + Send>;

// ---------------------------------------------------------------------------
// ASIO 缓冲区
// ---------------------------------------------------------------------------

/// 交给 ASIO 宿主的双缓冲。
///
/// 每个通道是一块**连续**内存,前半段是 index 0,后半段是 index 1。
/// 连续性很重要:宿主会长期持有这些指针,任何重新分配都会让它手里的
/// 指针失效,进而直接踩坏宿主进程的内存。
pub struct AsioBufferSet {
    buffer_size: usize,
    input_channels: usize,
    output_channels: usize,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

impl AsioBufferSet {
    pub fn new(input_channels: usize, output_channels: usize, buffer_size: usize) -> Self {
        let span = buffer_size * 2;
        AsioBufferSet {
            buffer_size,
            input_channels,
            output_channels,
            inputs: (0..input_channels).map(|_| vec![0.0; span]).collect(),
            outputs: (0..output_channels).map(|_| vec![0.0; span]).collect(),
        }
    }

    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    /// 输入缓冲在 `index` 处的起始地址,写进 `ASIOBufferInfo::buffers[index]`。
    pub fn input_ptr(&mut self, channel: usize, index: usize) -> *mut f32 {
        let off = index * self.buffer_size;
        self.inputs[channel][off..].as_mut_ptr()
    }

    /// 输出缓冲在 `index` 处的起始地址。
    pub fn output_ptr(&mut self, channel: usize, index: usize) -> *mut f32 {
        let off = index * self.buffer_size;
        self.outputs[channel][off..].as_mut_ptr()
    }

    pub fn input_plane_mut(&mut self, channel: usize, index: usize) -> &mut [f32] {
        let off = index * self.buffer_size;
        let size = self.buffer_size;
        &mut self.inputs[channel][off..off + size]
    }

    /// 只读地取得一个输入通道的当前缓冲。
    pub fn input_plane(&self, channel: usize, index: usize) -> &[f32] {
        let off = index * self.buffer_size;
        &self.inputs[channel][off..off + self.buffer_size]
    }

    pub fn output_plane(&self, channel: usize, index: usize) -> &[f32] {
        let off = index * self.buffer_size;
        &self.outputs[channel][off..off + self.buffer_size]
    }

    /// 可变地取得一个输出通道的当前缓冲。
    pub fn output_plane_mut(&mut self, channel: usize, index: usize) -> &mut [f32] {
        let off = index * self.buffer_size;
        let size = self.buffer_size;
        &mut self.outputs[channel][off..off + size]
    }
    /// 同时取得一个输入通道和一个输出通道。
    ///
    /// 单独调用 [`Self::input_plane`] 和 [`Self::output_plane_mut`] 会被
    /// 借用检查器拦住(一个是 `&self`,一个是 `&mut self`),而直通测试
    /// 恰恰需要同时拿到两者。输入和输出是两块独立的存储,所以这里可以
    /// 安全地一起借出去。
    pub fn input_and_output_mut(
        &mut self,
        input_channel: usize,
        output_channel: usize,
        index: usize,
    ) -> (&[f32], &mut [f32]) {
        let off = index * self.buffer_size;
        let size = self.buffer_size;
        let input = &self.inputs[input_channel][off..off + size];
        let output = &mut self.outputs[output_channel][off..off + size];
        (input, output)
    }
}

// ---------------------------------------------------------------------------
// 流运行时状态(只在时钟线程访问)
// ---------------------------------------------------------------------------

/// 输入流:设备 → ASIO。
struct InputStreamRuntime {
    device_name: String,
    /// 设备回调是生产者,这里是消费者。
    reader: FrameReader,
    /// 设备时钟域 → ASIO 时钟域。
    resampler: FixedOutResampler,
    drift: DriftController,
    /// 重采样器输入,分离通道,长度 = 输入帧数上界。
    resample_in: Vec<Vec<f32>>,
    /// 重采样器输出,分离通道,长度 = ASIO 缓冲区大小。
    resample_out: Vec<Vec<f32>>,
    channels: usize,
    /// 本流第 i 个通道对应 ASIO 的第几个输入通道。
    asio_channel_map: Vec<usize>,
    gain: f32,
    stats: Arc<RingStats>,
}

impl InputStreamRuntime {
    /// 采集一轮数据填进 ASIO 输入缓冲。
    fn fill(&mut self, buffers: &mut AsioBufferSet, index: usize, chunk: usize) {
        let need = self.resampler.input_frames_next();
        let got = self.reader.read_into_planar(&mut self.resample_in, need);
        // 记下真正从 ring 取走多少 —— 和写入量一比就知道是哪一侧不匹配。
        self.stats
            .read_frames
            .fetch_add(got as u64, Ordering::Relaxed);

        // 数据不够就补静音。这多发生在启动的头几个缓冲区,或者设备侧
        // 跟不上时。补零而不是重复旧数据,免得产生可听的回音。
        if got < need {
            let end = need.min(self.resample_in.first().map(|p| p.len()).unwrap_or(0));
            for plane in self.resample_in.iter_mut() {
                plane[got.min(end)..end].fill(0.0);
            }
        }

        if self.gain != 1.0 {
            let end = need.min(self.resample_in.first().map(|p| p.len()).unwrap_or(0));
            for plane in self.resample_in.iter_mut() {
                for s in plane[..end].iter_mut() {
                    *s *= self.gain;
                }
            }
        }

        let produced = match self
            .resampler
            .process_into(&self.resample_in, &mut self.resample_out)
        {
            Ok(outcome) => outcome.frames_out,
            Err(_) => {
                // 实时线程只记数,不打印 —— 见 `RingStats::resample_failures`。
                self.stats.resample_failures.fetch_add(1, Ordering::Relaxed);
                0
            }
        };

        let produced = produced.min(chunk);
        for ch in 0..self.channels {
            let dst = buffers.input_plane_mut(self.asio_channel_map[ch], index);
            let n = produced.min(self.resample_out[ch].len()).min(dst.len());
            dst[..n].copy_from_slice(&self.resample_out[ch][..n]);
            let tail = chunk.min(dst.len());
            dst[n..tail].fill(0.0);
        }
    }

    /// 用当前水位推进漂移控制。
    fn update_drift(&mut self, dt_seconds: f64) {
        let queued = self.reader.available_frames();
        // 直通流没有可调的比率(见 `stream_quality`)。它的水位靠「生产和
        // 消费同源」保持稳定,这里只上报水位 —— 否则界面上会挂着一个永远
        // 不会生效的漂移修正量。
        if self.resampler.is_passthrough() {
            self.stats
                .queued_frames
                .store(queued as u64, Ordering::Relaxed);
            self.stats
                .underflow_frames
                .store(self.reader.underflow_frames(), Ordering::Relaxed);
            return;
        }
        let adjust = self.drift.update(queued, dt_seconds);
        self.resampler.set_relative_ratio(1.0 + adjust, true);
        self.stats
            .drift_ppm
            .store((adjust * 1e6) as i64, Ordering::Relaxed);
        self.stats
            .queued_frames
            .store(queued as u64, Ordering::Relaxed);
        self.stats
            .underflow_frames
            .store(self.reader.underflow_frames(), Ordering::Relaxed);
    }
}

/// 输出流:ASIO → 设备。
struct OutputStreamRuntime {
    device_name: String,
    /// 这里是生产者,设备回调是消费者。
    writer: FrameWriter,
    /// ASIO 时钟域 → 设备时钟域。
    resampler: FixedInResampler,
    drift: DriftController,
    /// 从 ASIO 输出缓冲取出的分离通道数据,长度 = ASIO 缓冲区大小。
    resample_in: Vec<Vec<f32>>,
    /// 重采样结果,分离通道,长度 = 输出帧数上界。
    resample_out: Vec<Vec<f32>>,
    /// 交织后的临时缓冲。
    staging: Vec<f32>,
    channels: usize,
    /// 本流第 i 个通道取自 ASIO 的第几个输出通道。
    asio_channel_map: Vec<usize>,
    gain: f32,
    stats: Arc<RingStats>,
}

impl OutputStreamRuntime {
    /// 从 ASIO 输出缓冲取一轮数据,重采样后送进设备 ring。
    fn drain(&mut self, buffers: &AsioBufferSet, index: usize, chunk: usize) {
        for ch in 0..self.channels {
            let src = buffers.output_plane(self.asio_channel_map[ch], index);
            let plane = &mut self.resample_in[ch];
            let n = chunk.min(src.len()).min(plane.len());
            plane[..n].copy_from_slice(&src[..n]);
            plane[n..].fill(0.0);
        }

        let frames = match self
            .resampler
            .process_into(&self.resample_in, &mut self.resample_out)
        {
            Ok(outcome) => outcome.frames_out,
            Err(_) => {
                // 同输入侧:实时线程只记数,不打印。
                self.stats.resample_failures.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        let usable = frames.min(self.staging.len() / self.channels);
        for f in 0..usable {
            let base = f * self.channels;
            for ch in 0..self.channels {
                let v = self.resample_out[ch].get(f).copied().unwrap_or(0.0);
                self.staging[base + ch] = if self.gain == 1.0 { v } else { v * self.gain };
            }
        }

        // 写不进去的部分由 `write_interleaved` 自己记进生产者的
        // `overflow_frames`,`update_drift` 会把它上报到共享统计里。这里**不**
        // 再 `fetch_add` 一次 —— 那既是重复计数,又会被随后的 `store` 覆盖,
        // 白白让这个数变得无法解释。
        let written = self.writer.write_interleaved(&self.staging, usable);
        // 流量计数和输入侧的 `fill()` 对称:谁从 ring 里取走/送进去,就由谁记。
        // 少这一行,输出流的「写」永远是 0 —— 和之前欠载计数器只上报一半
        // 是同一类问题。
        self.stats
            .written_frames
            .fetch_add(written as u64, Ordering::Relaxed);
    }

    /// 用静音把环形缓冲垫到目标水位。
    ///
    /// 输入流启动前有 [`Engine::wait_for_input_prime`] 替它等设备灌到目标水位,
    /// 输出流原本什么都没有 —— 设备一开始播放就对着一个近乎全空的 ring,每块
    /// 回调都取不满,启动后的头几秒一直在欠载。
    ///
    /// 更麻烦的是之后:ring 只能靠漂移补偿那个 ±0.5% 的偏置慢慢涨。水位配得越大
    /// 涨得越久 —— 200 ms 要靠 0.5% 灌满得四十多秒,这段时间水位始终远低于目标,
    /// 用户看到的就成「都配到 200 ms 了还是欠载」。水位明明是用来买余量的,
    /// 调大反而更糟,因为补上余量的速度没变。
    ///
    /// 垫静音没有副作用:这些静音本来就会送到设备上播放,只是把「慢慢涨上去」
    /// 换成「一开始就停在目标水位」,稳态延迟不变。启动初期设备多播几十毫秒静音,
    /// 正是 `getLatencies` 报给宿主的那个值。
    fn prime_to_target(&mut self) {
        let target = self.drift.target_frames();
        let have = self.writer.queued_frames();
        if have >= target {
            return;
        }
        // 只走这一次,不在音频线程上,分配是安全的。
        let silence = vec![0.0f32; (target - have) * self.channels];
        self.writer.write_interleaved(&silence, target - have);
    }

    fn update_drift(&mut self, dt_seconds: f64) {
        let queued = self.writer.queued_frames();
        // 直通流:同输入侧,只上报水位。
        if self.resampler.is_passthrough() {
            self.stats
                .queued_frames
                .store(queued as u64, Ordering::Relaxed);
            self.stats
                .overflow_frames
                .store(self.writer.overflow_frames(), Ordering::Relaxed);
            return;
        }
        let adjust = self.drift.update(queued, dt_seconds);
        self.resampler.set_relative_ratio(1.0 + adjust, true);
        self.stats
            .drift_ppm
            .store((adjust * 1e6) as i64, Ordering::Relaxed);
        self.stats
            .queued_frames
            .store(queued as u64, Ordering::Relaxed);
        self.stats
            .overflow_frames
            .store(self.writer.overflow_frames(), Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// 时钟核心
// ---------------------------------------------------------------------------

/// 由时钟主设备回调驱动的核心状态。
struct AudioCore {
    buffers: AsioBufferSet,
    inputs: Vec<InputStreamRuntime>,
    outputs: Vec<OutputStreamRuntime>,
    callback: BufferSwitchCallback,
    /// 下一个要交给宿主的 double buffer 下标。
    buffer_index: usize,
    /// 还没凑够一个 ASIO 缓冲区的帧数。
    accumulated: usize,
    /// 采样率,计算漂移控制的时间步长用。
    sample_rate: f64,
    /// 控制接口和回调之间的运行开关。放在核心里是因为它必须和
    /// `advance()` 的检查原子地发生。
    running: bool,
    /// 已处理帧数。与 `Engine` 共享,`getSamplePosition()` 读它时不加锁。
    samples_processed: Arc<AtomicU64>,
    /// 引擎跟不上时丢掉的帧数(**累计**),与 `Engine` 共享。
    ///
    /// 全引擎级的:丢的是"时钟推进的计数",不属于哪一条流,所以不进
    /// `RingStats`,而是由 [`EngineStatus::dropped_frames`] 带出去。做成原子
    /// 而不是核心里一个普通字段,是因为 `Engine::status()` 拿不到锁时也要能
    /// 读到一个可信的值 —— 否则界面上的数字会在真值和 0 之间跳。
    ///
    /// **绝不在这里打印。** `log::warn!` 最终要落盘,而落盘是阻塞操作 ——
    /// 调用它的却是实时音频线程,一次磁盘 I/O 就够让缓冲区欠载。这条路径
    /// 只累加,由 `Engine::status()` 那条非实时路径落盘。
    dropped_frames: Arc<AtomicU64>,
    /// 上次落盘之后又攒了多少丢帧,供日志报"本次新增"。
    dropped_since_log: u64,
}

impl AudioCore {
    fn buffer_size(&self) -> usize {
        self.buffers.buffer_size()
    }

    /// 时钟推进 `frames` 帧,必要时执行若干次缓冲区交换。
    ///
    /// 设备回调的块大小与 ASIO 缓冲区大小不一定相等,所以需要累积到
    /// 整数个缓冲区再通知宿主,保证 ASIO 侧的块边界始终整齐。
    fn advance(&mut self, frames: usize) {
        if !self.running {
            return;
        }
        let size = self.buffer_size();
        if size == 0 {
            return;
        }
        self.accumulated += frames;

        let mut budget = MAX_BUFFERS_PER_ADVANCE;
        while self.accumulated >= size && budget > 0 {
            self.accumulated -= size;
            budget -= 1;
            self.process_one_buffer();
        }

        if self.accumulated >= size {
            // 正常情况走不到这里。真走到了说明累积异常,丢弃多余计数,
            // 免得延迟无限增长。
            //
            // 只记数、**不打印** —— 这里跑在实时音频线程上,而打印要落盘。
            // 攒着交给 `Engine::status()` 那条非实时路径去说。
            let (dropped, kept) = drop_excess_accumulated(self.accumulated, size);
            self.accumulated = kept;
            let dropped = dropped as u64;
            self.dropped_frames.fetch_add(dropped, Ordering::Relaxed);
            self.dropped_since_log = self.dropped_since_log.saturating_add(dropped);
        }
    }

    /// 取走音频线程攒下的告警,**返回待打印的文字,不自己打印**。
    ///
    /// 调用方(`Engine::status`)必须拿着返回的字符串**出了锁再**落盘。这里
    /// 早先是直接 `log::warn!` 的,而它跑在 `try_lock_for` 的守卫里 ——
    /// 于是控制面板每 200 ms 查一次状态,一旦恰好有告警要打,锁就被按住
    /// 一次磁盘 I/O 的时间,而音频线程正靠这把锁推进时钟:轻则时钟停一拍,
    /// 重则设备回调抢不到锁、整块送静音(见输出回调里对 `try_lock` 的处理)。
    ///
    /// `format!` 的分配留在锁内是有意的:那是微秒级,而且告警本来就是低频
    /// 事件,换来的是不必把计数器搬来搬去。
    ///
    /// **只能从非实时路径调用**(目前是 `Engine::status`)。读一次清零一次,
    /// 所以同一条告警不会反复刷屏。
    fn take_audio_warnings(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.dropped_since_log > 0 {
            out.push(format!(
                "时钟推进落后,本次丢弃 {} 帧(累计 {} 帧;设备块与 ASIO 缓冲区不匹配?)",
                self.dropped_since_log,
                self.dropped_frames.load(Ordering::Relaxed),
            ));
            self.dropped_since_log = 0;
        }
        // 输入和输出是不同的类型,chain 不起来,分两趟走。
        for stream in self.inputs.iter() {
            if let Some(msg) = take_resample_failures(&stream.device_name, &stream.stats) {
                out.push(msg);
            }
        }
        for stream in self.outputs.iter() {
            if let Some(msg) = take_resample_failures(&stream.device_name, &stream.stats) {
                out.push(msg);
            }
        }
        out
    }

    /// 一个完整的 ASIO 缓冲区周期:填输入 → 叫宿主 → 收输出 → 调漂移。
    fn process_one_buffer(&mut self) {
        let index = self.buffer_index;
        self.buffer_index ^= 1;
        let chunk = self.buffer_size();
        let dt = chunk as f64 / self.sample_rate.max(1.0);

        // 解构出各字段的独立借用,否则同时借用 buffers / inputs / callback
        // 会被借用检查器拒绝。
        let AudioCore {
            buffers,
            inputs,
            outputs,
            callback,
            samples_processed,
            ..
        } = self;

        for stream in inputs.iter_mut() {
            stream.fill(buffers, index, chunk);
        }

        // 宿主在这里读输入、写输出。注意宿主可能在自己的处理里回头调用
        // 本驱动的 getSamplePosition() —— 那些接口只读原子变量,不会
        // 碰到这把锁。
        callback(buffers, index);

        for stream in outputs.iter_mut() {
            stream.drain(buffers, index, chunk);
        }

        for stream in inputs.iter_mut() {
            stream.update_drift(dt);
        }
        for stream in outputs.iter_mut() {
            stream.update_drift(dt);
        }

        samples_processed.fetch_add(chunk as u64, Ordering::Relaxed);
    }

    /// 启动前把输出环形缓冲垫到目标水位,再把宿主已经填好的输出缓冲
    /// (ASIO 约定是 index 1)推到队尾。
    fn prime_outputs(&mut self) {
        let chunk = self.buffer_size();
        let AudioCore {
            buffers, outputs, ..
        } = self;
        for stream in outputs.iter_mut() {
            // 顺序很重要:先垫静音、再推宿主的音频。反过来的话宿主那一块
            // 会被设备立刻取走,ring 又回到近乎全空的状态。
            stream.prime_to_target();
            stream.drain(buffers, 1, chunk);
        }
    }

    fn status_rows(&self) -> Vec<StreamStatusSnapshot> {
        let mut rows = Vec::new();
        for (index, s) in self.inputs.iter().enumerate() {
            let snap = s.stats.snapshot();
            rows.push(StreamStatusSnapshot {
                kind: StreamKind::Input,
                index,
                device_name: s.device_name.clone(),
                channel_count: s.channels,
                // 谁当基准是 `Engine` 的事,这里先留空,由 `Engine::status` 填。
                is_clock_master: false,
                // 设备块大小由 `Engine::status` 回填 —— 音频核心看不到那个。
                device_frames: 0,
                drift_ppm: snap.drift_ppm,
                had_glitch: !snap.is_healthy(),
                stats: snap,
            });
        }
        for (index, s) in self.outputs.iter().enumerate() {
            let snap = s.stats.snapshot();
            rows.push(StreamStatusSnapshot {
                kind: StreamKind::Output,
                index,
                device_name: s.device_name.clone(),
                channel_count: s.channels,
                is_clock_master: false,
                // 设备块大小由 `Engine::status` 回填 —— 音频核心看不到那个。
                device_frames: 0,
                drift_ppm: snap.drift_ppm,
                had_glitch: !snap.is_healthy(),
                stats: snap,
            });
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// 设备流构建
// ---------------------------------------------------------------------------

/// 构建一条音频流所需的全部材料。
///
/// 这些都会被移动到专用线程上 —— 流必须在它被创建的那个线程里启动和
/// 销毁,所以准备工作也只能在线程内部完成。
struct StreamSpec {
    kind: StreamKind,
    /// 设备句柄。用 `Arc` 是因为 `DeviceInfo` 是可克隆的,而句柄本身由
    /// 后端持有引用计数。
    device: Arc<dyn DeviceHandle>,
    device_name: String,
    /// 后端协商好的格式。设备原生格式到 f32 的转换在后端做,这里只关心
    /// 采样率和通道数。
    format: StreamFormat,
    /// ring 的第 i 个通道取自设备的第 `ch_map[i]` 个通道。
    ch_map: Vec<usize>,
    /// 输入流:设备回调持生产者。
    input_writer: Option<FrameWriter>,
    /// 输出流:设备回调持消费者。
    output_reader: Option<FrameReader>,
    /// 只有时钟主设备才需要它来推进整个引擎。
    master_core: Option<Arc<Mutex<AudioCore>>>,
    err_flag: Arc<AtomicBool>,
    /// 报给实时优先级提升用的「每块缓冲多少帧」。
    ///
    /// MMCSS 拿它和采样率一起推算这条线程该怎么被调度。设备实际的缓冲
    /// 大小问不出来(共享模式下由系统定),所以拿引擎的 chunk 当代表 ——
    /// 同一个数量级就够它做判断了。
    frames_per_buffer: u32,
    /// 回调往里写自己实际收到的每块帧数。
    ///
    /// 每条流一个,和流一一对应 —— `Engine` 那边按顺序收集它们,
    /// 见 `Engine::device_frames_per_stream`。
    device_frames: Arc<AtomicUsize>,
    /// ring 的统计。设备回调往里记写入量,消费侧记读取量 —— 见 `RingStats`。
    stats: Arc<RingStats>,
}

impl StreamSpec {
    fn build(self) -> Result<Box<dyn StreamHandle>> {
        match self.kind {
            StreamKind::Input => build_input(self),
            StreamKind::Output => build_output(self),
        }
    }
}

/// 把**当前线程**提升到实时音频优先级。
///
/// 只在音频回调的第一次执行时调用。之所以非得在这里做:`audio_thread_priority`
/// 提的是"当前线程",而音频回调跑在**后端自己创建的线程**上 —— 别的线程替不了
/// 它,只有在回调内部才有机会。
///
/// 提不上去不影响音频能不能跑,只是这条线程更容易被系统调度器抢走,表现就是
/// 缓冲再大也照样爆音。所以失败只记一条日志(系统不支持、权限不够等),不往上
/// 报错。
///
/// # 返回的句柄为什么就地泄漏
///
/// 提升成功会给一个 `RtPriorityHandle`,但这里**不保存**它,直接
/// `mem::forget`:
///
/// * 它的 `Drop` 会调 MMCSS 的 `AvRevertMmThreadCharacteristics`,而那个
///   调用必须发生在当初提升的那条线程上。闭包析构时未必还在音频线程(流
///   销毁可能由别的线程发起),跨线程还回去是错上加错。
/// * 而且 Windows 上这个句柄内部含裸指针,本身就不是 `Send`,根本塞不进
///   后端要求 `Send` 的回调闭包 —— 想保存也保存不了。
///
/// 音频线程的寿命和流绑定:线程一结束,系统会把 MMCSS 特性连同线程一起回收,
/// 用不着我们显式还。
fn promote_audio_thread(frames_per_buffer: u32, sample_rate: u32) {
    // 参数为 0 会被库判为无效,兜一下。
    let frames = frames_per_buffer.max(1);
    match audio_thread_priority::promote_current_thread_to_real_time(frames, sample_rate) {
        Ok(handle) => {
            log::info!("音频线程已提升到实时优先级({frames} 帧 @ {sample_rate} Hz)");
            // 句柄**故意不析构**,理由见函数文档。`ManuallyDrop` 比
            // `mem::forget` 更直白地表达这个意图,而且在 Windows 上句柄
            // 根本没有 `Drop`(那边忘了等于没忘),只有 Linux/macOS 才需要
            // 靠这一手把提升状态留住。
            let _keep_alive = std::mem::ManuallyDrop::new(handle);
        }
        Err(e) => log::warn!("音频线程提不上实时优先级,按普通优先级继续:{e}"),
    }
}

/// 输入方向:设备回调把采集数据搬进 ring。
///
/// 设备原生格式到 `f32` 的转换已经在后端做完,所以这里没有泛型。
fn build_input(spec: StreamSpec) -> Result<Box<dyn StreamHandle>> {
    let StreamSpec {
        device,
        device_name,
        format,
        ch_map,
        input_writer,
        master_core,
        stats,
        err_flag,
        frames_per_buffer,
        device_frames,
        ..
    } = spec;

    let sample_rate = format.sample_rate;
    let device_channels = format.channels;
    let mut writer =
        input_writer.ok_or_else(|| Error::Internal("输入流缺少 ring 生产者".into()))?;
    let err_name = device_name.clone();

    // 音频回调跑在后端建出来的线程上,实时优先级只能由**那条线程自己**申请
    // —— 参见 `promote_audio_thread`。这里只需要一个"做过了"的开关。
    let rt_tried = Arc::new(AtomicBool::new(false));

    let on_data: InputCallback = Box::new(move |data: &[f32]| {
        // 快速路径:提过了就直接过去。用原子标志而不是加锁 —— 这是音频线程,
        // 每块缓冲都要跑一遍,不该为只做一次的活去争锁。
        if !rt_tried.swap(true, Ordering::Relaxed) {
            promote_audio_thread(frames_per_buffer, sample_rate);
        }

        let device_channels = device_channels.max(1);
        let frames = data.len() / device_channels;
        // 量下设备实际每块多少帧 —— 这是唯一能拿到真实缓冲大小的办法
        // (见 `Engine::device_frames_per_stream`)。`store` 无锁,放在音频
        // 线程里是安全的。
        device_frames.store(frames, Ordering::Relaxed);
        let total = frames * device_channels;
        if total == 0 {
            return;
        }

        // 记下真正写进去多少(ring 满时会少于 `frames`)—— 和消费量一比就
        // 知道积压在哪一侧。
        let written = writer.write_selected(&data[..total], device_channels, &ch_map, frames);
        stats
            .written_frames
            .fetch_add(written as u64, Ordering::Relaxed);
        // 这一侧的溢出只能在这里上报。ring 的生产者半端就在本回调手里,
        // 它的计数除了这里没有第二个人能读走 —— `InputStreamRuntime` 拿到的是
        // 消费者半端,只能看到欠载。少这一行,输入侧的溢出就永远不会出现在
        // 界面上。
        stats
            .overflow_frames
            .store(writer.overflow_frames(), Ordering::Relaxed);

        // 如果这条流就是时钟主设备,它同时负责推进整个引擎。
        if let Some(core) = master_core.as_ref() {
            if let Some(mut core) = core.try_lock() {
                core.advance(frames);
            }
        }
    });

    let on_error: ErrorCallback = Box::new(move |e: String| {
        err_flag.store(true, Ordering::Release);
        log::error!("输入设备 “{err_name}” 报错:{e}");
    });

    device.open_input(&format, on_data, on_error)
}

/// 输出方向:从 ring 取数据交给设备播放。
fn build_output(spec: StreamSpec) -> Result<Box<dyn StreamHandle>> {
    let StreamSpec {
        device,
        device_name,
        format,
        ch_map,
        output_reader,
        master_core,
        stats,
        err_flag,
        frames_per_buffer,
        device_frames,
        ..
    } = spec;

    let sample_rate = format.sample_rate;
    let device_channels = format.channels;
    let mut reader =
        output_reader.ok_or_else(|| Error::Internal("输出流缺少 ring 消费者".into()))?;
    let mut scratch: Vec<f32> = Vec::new();
    let err_name = device_name.clone();

    // 同输入侧:音频线程的实时优先级只能在回调内部申请。
    let rt_tried = Arc::new(AtomicBool::new(false));

    let on_data: OutputCallback = Box::new(move |data: &mut [f32]| {
        if !rt_tried.swap(true, Ordering::Relaxed) {
            promote_audio_thread(frames_per_buffer, sample_rate);
        }

        let device_channels = device_channels.max(1);
        let frames = data.len() / device_channels;
        if frames == 0 {
            return;
        }
        // 同输入侧:量出设备实际的块大小。
        device_frames.store(frames, Ordering::Relaxed);
        let ring_channels = ch_map.len().max(1);
        let ring_samples = frames * ring_channels;
        if scratch.len() < ring_samples {
            scratch.resize(ring_samples, 0.0);
        }

        // 先把本流自己的 ring 读出来 —— **不需要锁**。
        //
        // reader 半端由本回调独占;生产者半端只被 `advance()` 里的 `drain()`
        // 碰,而时钟推进就发生在这个回调里(`master_core` 那一支),所以两边
        // 其实是同一个线程。唯一的例外是启动时 `prime_outputs()` 从控制线程
        // 写过一次,但那发生在输出设备启动之前,不会有回调与它并发。
        //
        // 这里曾经整个包在 `try_lock` 里。代价是抢不到锁时这一整块只能送静音,
        // 而且 `read_interleaved` 根本没被调用 —— 欠载计数也不会增加,丢得
        // 无声无息。现在读和锁解耦,设备永远能拿到 ring 里的数据,计数也永远准。
        let got = reader.read_interleaved(&mut scratch[..ring_samples], frames);

        // 这一侧的欠载只能在这里上报。ring 的消费者半端就在本回调手里,
        // 它的计数除了这里没有第二个人能读走 —— `OutputStreamRuntime` 拿到的是
        // 生产者半端,只能看到溢出。少这一行,输出侧的欠载就永远不会出现在
        // 界面上:水位掉到不足一块设备缓冲时,界面照样显示「正常」。
        stats
            .underflow_frames
            .store(reader.underflow_frames(), Ordering::Relaxed);
        // 流量计数同样和输入侧对称:从 ring 里取走多少就在这里记多少。
        stats.read_frames.fetch_add(got as u64, Ordering::Relaxed);

        // 时钟推进要碰所有流的另一半端,这才需要锁。抢不到就跳过这一拍:
        // 数据没丢(上面已经读到手了),只是引擎时钟停一拍,下一次回调会把
        // 欠的那部分一起补上。
        if let Some(core) = master_core.as_ref() {
            if let Some(mut core) = core.try_lock() {
                core.advance(frames);
            }
        }

        // 填数据放在锁外,尽量缩短持锁时间。
        for s in data.iter_mut() {
            *s = 0.0;
        }
        let usable = got.min(frames);
        for f in 0..usable {
            let base = f * ring_channels;
            for (slot, &dev_ch) in ch_map.iter().enumerate() {
                if dev_ch < device_channels && base + slot < scratch.len() {
                    data[f * device_channels + dev_ch] = scratch[base + slot];
                }
            }
        }
    });

    let on_error: ErrorCallback = Box::new(move |e: String| {
        err_flag.store(true, Ordering::Release);
        log::error!("输出设备 “{err_name}” 报错:{e}");
    });

    device.open_output(&format, on_data, on_error)
}

// ---------------------------------------------------------------------------
// 设备流专用线程
// ---------------------------------------------------------------------------

/// 要启动哪一组流。
///
/// 分组是因为启动顺序有讲究:输入流要先跑起来、把环形缓冲填上一批数据,
/// 输出流才好开始播放,否则开场那一小段会是静音。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamGroup {
    Inputs,
    Outputs,
}

enum StreamCommand {
    Start(StreamGroup, mpsc::Sender<Result<()>>),
    Stop(mpsc::Sender<Result<()>>),
    Shutdown,
}

/// 持有设备流的线程句柄。
struct StreamHost {
    commands: mpsc::Sender<StreamCommand>,
    join: Option<JoinHandle<()>>,
}

impl StreamHost {
    /// 在线程上创建所有流,并把创建结果同步传回。
    fn spawn(specs: Vec<StreamSpec>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<StreamCommand>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let join = std::thread::Builder::new()
            .name("pigasio-streams".into())
            .spawn(move || {
                // 流必须在创建它的线程里启动和销毁,这是这个专用线程
                // 存在的唯一理由。
                let mut inputs: Vec<Box<dyn StreamHandle>> = Vec::new();
                let mut outputs: Vec<Box<dyn StreamHandle>> = Vec::new();
                let mut failure: Option<Error> = None;

                for spec in specs {
                    let name = spec.device_name.clone();
                    let kind = spec.kind;
                    match spec.build() {
                        Ok(stream) => {
                            log::info!("已打开{}设备流:{name}", kind.as_str());
                            if kind == StreamKind::Input {
                                inputs.push(stream);
                            } else {
                                outputs.push(stream);
                            }
                        }
                        Err(e) => {
                            log::error!("打开设备流 “{name}” 失败:{e}");
                            failure = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = failure {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }

                fn play_group(list: &[Box<dyn StreamHandle>]) -> Result<()> {
                    for s in list {
                        s.play()?;
                    }
                    Ok(())
                }

                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        StreamCommand::Start(group, reply) => {
                            let mut result = Ok(());
                            if group == StreamGroup::Inputs {
                                result = play_group(&inputs);
                            }
                            if result.is_ok() && group == StreamGroup::Outputs {
                                result = play_group(&outputs);
                            }
                            let _ = reply.send(result);
                        }
                        StreamCommand::Stop(reply) => {
                            // 停止顺序与启动相反:先掐输出,再掐输入。
                            for s in outputs.iter().chain(inputs.iter()) {
                                if let Err(e) = s.pause() {
                                    log::warn!("暂停音频流失败:{e}");
                                }
                            }
                            let _ = reply.send(Ok(()));
                        }
                        StreamCommand::Shutdown => break,
                    }
                }

                // 流在这里被销毁,与创建它们处于同一线程。
                drop(inputs);
                drop(outputs);
            })
            .map_err(|e| Error::Platform(format!("创建音频流线程失败:{e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(StreamHost {
                commands: cmd_tx,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => Err(Error::Internal("音频流线程意外退出".into())),
        }
    }

    fn start(&self, group: StreamGroup) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.commands
            .send(StreamCommand::Start(group, tx))
            .map_err(|_| Error::Internal("音频流线程已退出".into()))?;
        rx.recv()
            .map_err(|_| Error::Internal("音频流线程未回复启动结果".into()))?
    }

    fn stop(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.commands
            .send(StreamCommand::Stop(tx))
            .map_err(|_| Error::Internal("音频流线程已退出".into()))?;
        rx.recv()
            .map_err(|_| Error::Internal("音频流线程未回复停止结果".into()))?
    }
}

impl Drop for StreamHost {
    fn drop(&mut self) {
        let _ = self.commands.send(StreamCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

/// 每条流的静态描述,供状态查询使用。
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub kind: StreamKind,
    pub device_name: String,
    pub is_clock_master: bool,
    /// 本流第 0 个通道在 ASIO 通道列表里的下标。
    pub asio_channel_offset: usize,
    pub channel_count: usize,
    /// 本流第 i 个通道对应设备的第几个通道。
    pub device_channel_map: Vec<usize>,
}

/// 单条流的运行统计。
#[derive(Debug, Clone)]
pub struct StreamStatusSnapshot {
    pub kind: StreamKind,
    /// 这个方向上的第几条流(从 0 起),和配置里的顺序一致。
    ///
    /// 用来跟 `Engine::clock_master` 对上号 —— 那边就是这个索引口径。
    pub index: usize,
    pub device_name: String,
    pub channel_count: usize,
    /// 是不是时钟基准。由 [`Engine::status`] 填 —— 音频核心只管缓冲,
    /// 不知道这件事。
    pub is_clock_master: bool,
    /// 这条流的设备回调**实际**每块送来多少帧。由 [`Engine::status`] 填。
    ///
    /// WASAPI 共享模式下缓冲由系统音频引擎决定,而我们给 cpal 的本来就是
    /// `Default` —— 所以这个数只能在回调里量,和报给宿主的 `buffer_size`
    /// 不是一回事。逐流分开记才能定位到是哪块设备。0 表示还没量到。
    pub device_frames: usize,
    /// 当前漂移补偿量,ppm。
    pub drift_ppm: f64,
    pub had_glitch: bool,
    pub stats: RingStatsSnapshot,
}

/// 引擎状态快照。
#[derive(Debug, Clone)]
pub struct EngineStatus {
    pub sample_rate: u32,
    pub buffer_size: usize,
    pub input_channels: usize,
    pub output_channels: usize,
    pub running: bool,
    pub sample_position: u64,
    /// 引擎跟不上、被丢掉的**累计帧数**。全引擎级,不属于哪一条流。
    ///
    /// 它和逐流的「欠载 / 溢出」是两码事:那两个量的是某条流自己的环形缓冲,
    /// 这个量的是时钟推进本身 —— `MAX_BUFFERS_PER_ADVANCE` 那一轮没处理完、
    /// 只能把多余累积丢掉的部分。正常情况恒为 0。
    pub dropped_frames: u64,
    pub streams: Vec<StreamInfo>,
    pub stream_stats: Vec<StreamStatusSnapshot>,
}

/// 多设备 ASIO 引擎。
pub struct Engine {
    config: Config,
    clock_master: (StreamKind, usize),
    input_devices: Vec<DeviceInfo>,
    output_devices: Vec<DeviceInfo>,
    core: Arc<Mutex<AudioCore>>,
    host: Option<StreamHost>,
    buffer_size: usize,
    input_channel_count: usize,
    output_channel_count: usize,
    sample_rate: u32,
    running: Arc<AtomicBool>,
    /// `getSamplePosition()` 会用到,由音频回调更新,读取不加锁。
    samples_processed: Arc<AtomicU64>,
    /// 引擎跟不上时丢掉的累计帧数,由音频回调更新。见 `AudioCore::dropped_frames`。
    dropped_frames: Arc<AtomicU64>,
    stream_infos: Vec<StreamInfo>,
    /// 每个 ASIO 通道的显示名,构造时一次算好。
    channel_names: ChannelNames,
    stream_error: Arc<AtomicBool>,
    prepared: Arc<AtomicBool>,
    /// 各流重采样器引入的延迟(帧),取最大值。`prepare()` 之后才有意义。
    ///
    /// 它直接加在音频路径上,`getLatencies()` 必须算进去。构造时是 0 ——
    /// 重采样器要等 `prepare()` 才建出来。
    resampler_delay_frames: usize,
    /// 每条流各自的设备块大小,顺序同 `stream_infos`(先输入后输出)。
    ///
    /// 这是量出来的,不是我们请求的:WASAPI 共享模式下缓冲由系统音频引擎
    /// 决定,而我们给 cpal 的本来就是 `BufferSize::Default`。cpal 0.15 也没
    /// 提供查询接口(`StreamTrait` 只有 `play`/`pause`),唯一可靠的办法就是
    /// 在回调里量 —— 每收到一块,`data.len() / channels` 就是那一块的帧数。
    ///
    /// **按流分开存**而不是汇总成一个数,是因为不同设备可能各有一套周期
    /// (虚拟声卡常见 20 ms 上下,物理声卡可能是 10 ms)。汇总成一个最大值
    /// 只能看出"最慢的那块有多慢",定位不到是谁。
    device_frames_per_stream: Vec<Arc<AtomicUsize>>,
}

impl Engine {
    /// 解析配置、确定设备,但**不打开**任何流。
    ///
    /// 对应 ASIO 的 `ASIOInit()`:尽早发现配置错误,比等到
    /// `createBuffers()` 再报错友好得多。
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        // 后端必须在打开任何设备之前定下来 —— 设备枚举走的就是它。控制面板
        // 改了这一项之后,下一次建引擎就会换过去,不必重启。
        crate::backend::select(config.engine.backend);

        let mut input_devices = Vec::new();
        for (i, cfg) in config.active_inputs() {
            let info = devices::resolve(&cfg.device, StreamKind::Input)
                .map_err(|e| Error::Config(format!("第 {} 个输入设备:{e}", i + 1)))?;
            devices::check_channels(&info.name, info.max_channels, cfg)?;
            log::info!(
                "输入设备 #{}: {} ({} 通道,设备默认 {} Hz)",
                i + 1,
                info.name,
                info.max_channels,
                info.default_sample_rate
            );
            input_devices.push(info);
        }

        let mut output_devices = Vec::new();
        for (i, cfg) in config.active_outputs() {
            let info = devices::resolve(&cfg.device, StreamKind::Output)
                .map_err(|e| Error::Config(format!("第 {} 个输出设备:{e}", i + 1)))?;
            devices::check_channels(&info.name, info.max_channels, cfg)?;
            log::info!(
                "输出设备 #{}: {} ({} 通道,设备默认 {} Hz)",
                i + 1,
                info.name,
                info.max_channels,
                info.default_sample_rate
            );
            output_devices.push(info);
        }

        check_duplicates_for(&input_devices, &config, StreamKind::Input)?;
        check_duplicates_for(&output_devices, &config, StreamKind::Output)?;

        let clock_master = config
            .clock_master()
            .ok_or_else(|| Error::Config("没有可用的设备,无法确定时钟主设备".into()))?;
        log::info!(
            "时钟主设备:{} #{} —— 其余设备通过重采样跟随它",
            clock_master.0.as_str(),
            clock_master.1 + 1
        );

        // ASIO 的通道是扁平编号的:第 0 个输入设备的通道排在最前,
        // 接着是第 1 个,依次类推。
        let mut stream_infos = Vec::new();
        let mut offset = 0usize;
        for (i, cfg) in config.active_inputs() {
            let mapped = cfg.channels.expand();
            let count = mapped.len();
            stream_infos.push(StreamInfo {
                kind: StreamKind::Input,
                device_name: input_devices[i].name.clone(),
                is_clock_master: clock_master == (StreamKind::Input, i),
                asio_channel_offset: offset,
                channel_count: count,
                device_channel_map: mapped,
            });
            offset += count;
        }
        let mut offset = 0usize;
        for (i, cfg) in config.active_outputs() {
            let mapped = cfg.channels.expand();
            let count = mapped.len();
            stream_infos.push(StreamInfo {
                kind: StreamKind::Output,
                device_name: output_devices[i].name.clone(),
                is_clock_master: clock_master == (StreamKind::Output, i),
                asio_channel_offset: offset,
                channel_count: count,
                device_channel_map: mapped,
            });
            offset += count;
        }

        let input_channel_count = config.total_input_channels();
        let output_channel_count = config.total_output_channels();
        let buffer_size = config.buffer_size_samples as usize;
        // 通道名需要看到全部设备的全局视图才能避免重名,所以在这里
        // 一次算好,而不是每次 getChannelInfo() 现算。
        let channel_names = ChannelNames::build(&config, &stream_infos);

        let samples_processed = Arc::new(AtomicU64::new(0));
        let dropped_frames = Arc::new(AtomicU64::new(0));

        Ok(Engine {
            sample_rate: config.sample_rate,
            core: Arc::new(Mutex::new(AudioCore {
                buffers: AsioBufferSet::new(input_channel_count, output_channel_count, buffer_size),
                inputs: Vec::new(),
                outputs: Vec::new(),
                callback: Box::new(|_, _| {}),
                buffer_index: 0,
                accumulated: 0,
                sample_rate: config.sample_rate as f64,
                running: false,
                samples_processed: Arc::clone(&samples_processed),
                dropped_frames: Arc::clone(&dropped_frames),
                dropped_since_log: 0,
            })),
            host: None,
            buffer_size,
            input_channel_count,
            output_channel_count,
            running: Arc::new(AtomicBool::new(false)),
            samples_processed,
            dropped_frames,
            stream_error: Arc::new(AtomicBool::new(false)),
            prepared: Arc::new(AtomicBool::new(false)),
            resampler_delay_frames: 0,
            device_frames_per_stream: Vec::new(),
            stream_infos,
            channel_names,
            config,
            clock_master,
            input_devices,
            output_devices,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn input_channel_count(&self) -> usize {
        self.input_channel_count
    }

    pub fn output_channel_count(&self) -> usize {
        self.output_channel_count
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// 每条流的静态描述,按「先输入后输出、与配置同序」排列。
    ///
    /// 驱动层用它来拼通道名(比如 `IN 1 (Speakers)`)。这些信息是静态的,
    /// 所以可以直接返回引用,不需要加锁。
    pub fn stream_infos(&self) -> &[StreamInfo] {
        &self.stream_infos
    }

    /// 某个 ASIO 通道给宿主显示的通道名,例如 `OUT 1 (扬声器 Realtek)`。
    ///
    /// 名字在构造时就全部算好了 —— 保证同方向内不会重名(见
    /// [`crate::channel_name`]),所以这里只是查表。
    pub fn channel_name(&self, kind: StreamKind, channel: usize) -> Option<&str> {
        self.channel_names.get(kind, channel)
    }

    /// 全部通道名。用于诊断输出。
    pub fn channel_names(&self) -> &ChannelNames {
        &self.channel_names
    }

    /// 缓冲区是否已经准备好(对应 ASIO 的 createBuffers 之后)。
    pub fn is_prepared(&self) -> bool {
        self.prepared.load(Ordering::Acquire)
    }

    /// 是否有设备流报错(设备被拔出、被独占占用等)。
    pub fn has_stream_error(&self) -> bool {
        self.stream_error.load(Ordering::Acquire)
    }

    /// ASIO 宿主可选的缓冲区大小范围。
    ///
    /// 对外只承诺一个值。好处是内部所有缓冲和重采样器都能按固定块长
    /// 预分配,运行期完全不需要分配内存。
    pub fn buffer_size_range(&self) -> (i32, i32, i32, i32) {
        let n = self.buffer_size as i32;
        (n, n, n, 0)
    }

    /// 各流重采样器引入的延迟(帧),取最大值。
    ///
    /// 这个值直接加在音频路径上,`getLatencies()` 要把它和水位、缓冲区一起
    /// 报给宿主。`prepare()` 之前恒为 0 —— 那时候重采样器还没建出来。
    pub fn resampler_delay_frames(&self) -> usize {
        self.resampler_delay_frames
    }

    /// 取得 ASIO 缓冲区的地址,用于 `ASIOCreateBuffers()`。
    ///
    /// 这些指针在流运行期间保持有效 —— 内部缓冲一旦建立就不再重新分配。
    pub fn buffer_ptr(&self, is_input: bool, channel: usize, index: usize) -> *mut f32 {
        let mut core = self.core.lock();
        if is_input {
            core.buffers.input_ptr(channel, index)
        } else {
            core.buffers.output_ptr(channel, index)
        }
    }

    /// 打开所有设备流并建立内部管线。
    ///
    /// `buffer_size` 由宿主决定。我们在 `getBufferSize()` 里只报告一个值,
    /// 所以正常情况下宿主的请求与配置一致;不一致时以宿主为准,
    /// 免得某些固执的宿主直接拒绝加载驱动。
    pub fn prepare(&mut self, buffer_size: usize, callback: BufferSwitchCallback) -> Result<()> {
        if self.prepared.load(Ordering::Acquire) {
            return Err(Error::Config("缓冲区已经创建过了".into()));
        }
        if buffer_size == 0 {
            return Err(Error::Config("缓冲区大小不能为 0".into()));
        }
        if buffer_size != self.buffer_size {
            log::info!(
                "宿主请求 {} 帧的缓冲区(配置为 {}),以宿主为准",
                buffer_size,
                self.buffer_size
            );
            self.buffer_size = buffer_size;
        }

        let chunk = self.buffer_size;
        let sample_rate = self.sample_rate;
        let engine_cfg = self.config.engine.clone();
        let mut specs = Vec::new();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut input_offset = 0usize;
        // 每条流各自的设备块大小。顺序跟 `stream_infos` 一致(先输入后输出),
        // `status()` 才能按同样的下标对回去。
        let mut device_frames_per_stream: Vec<Arc<AtomicUsize>> = Vec::new();

        // ---- 输入流 ----
        for (i, cfg) in self.config.active_inputs() {
            let info = &self.input_devices[i];
            let mapped = cfg.channels.expand();
            let ring_channels = mapped.len();
            let format = info.handle.negotiate(
                StreamKind::Input,
                &StreamRequest {
                    sample_rate,
                    period_frames: engine_cfg.period_frames,
                },
            )?;

            let capacity = ring_capacity(chunk, engine_cfg.watermark_ms, sample_rate);
            let (writer, reader) = ring::ring_buffer(ring_channels, capacity);
            let stats = RingStats::new();

            // 标称比率 = 输出率 / 输入率。这里的“输入”是设备侧,
            // “输出”是 ASIO 侧。
            let nominal = sample_rate as f64 / format.sample_rate.max(1) as f64;
            let is_master = self.clock_master == (StreamKind::Input, i);
            let resampler = FixedOutResampler::new(&ResamplerSpec {
                quality: stream_quality(
                    engine_cfg.resample_quality,
                    nominal,
                    is_master,
                    &info.name,
                ),
                channels: ring_channels,
                chunk_size: chunk,
                nominal_ratio: nominal,
                max_relative: ResamplerSpec::max_relative_for(engine_cfg.max_drift_ppm),
            })?;

            let max_in = resampler.max_input_frames();
            inputs.push(InputStreamRuntime {
                device_name: info.name.clone(),
                reader,
                resampler,
                drift: DriftController::new(
                    target_watermark_frames(sample_rate, engine_cfg.watermark_ms),
                    engine_cfg.max_drift_ppm,
                    engine_cfg.drift_correction,
                ),
                resample_in: allocate_planes(ring_channels, max_in),
                resample_out: allocate_planes(ring_channels, chunk),
                channels: ring_channels,
                asio_channel_map: (input_offset..input_offset + ring_channels).collect(),
                gain: cfg.linear_gain(),
                stats: Arc::clone(&stats),
            });

            specs.push(StreamSpec {
                kind: StreamKind::Input,
                device: Arc::clone(&info.handle),
                device_name: info.name.clone(),
                format,
                ch_map: mapped,
                input_writer: Some(writer),
                output_reader: None,
                master_core: is_master.then(|| Arc::clone(&self.core)),
                err_flag: Arc::clone(&self.stream_error),
                frames_per_buffer: chunk as u32,
                stats: Arc::clone(&stats),
                device_frames: {
                    let slot = Arc::new(AtomicUsize::new(0));
                    device_frames_per_stream.push(Arc::clone(&slot));
                    slot
                },
            });
            input_offset += ring_channels;
        }

        // ---- 输出流 ----
        let mut output_offset = 0usize;
        for (i, cfg) in self.config.active_outputs() {
            let info = &self.output_devices[i];
            let mapped = cfg.channels.expand();
            let ring_channels = mapped.len();
            let format = info.handle.negotiate(
                StreamKind::Output,
                &StreamRequest {
                    sample_rate,
                    period_frames: engine_cfg.period_frames,
                },
            )?;

            let capacity = ring_capacity(chunk, engine_cfg.watermark_ms, sample_rate);
            let (writer, reader) = ring::ring_buffer(ring_channels, capacity);
            let stats = RingStats::new();

            // 标称比率 = 输出率 / 输入率。这里的“输入”是 ASIO 侧,
            // “输出”是设备侧 —— 与输入流的方向恰好相反。
            let nominal = format.sample_rate.max(1) as f64 / sample_rate as f64;
            let is_master = self.clock_master == (StreamKind::Output, i);
            let resampler = FixedInResampler::new(&ResamplerSpec {
                quality: stream_quality(
                    engine_cfg.resample_quality,
                    nominal,
                    is_master,
                    &info.name,
                ),
                channels: ring_channels,
                chunk_size: chunk,
                nominal_ratio: nominal,
                max_relative: ResamplerSpec::max_relative_for(engine_cfg.max_drift_ppm),
            })?;

            let max_out = resampler.max_output_frames();
            outputs.push(OutputStreamRuntime {
                device_name: info.name.clone(),
                writer,
                resampler,
                drift: DriftController::new(
                    target_watermark_frames(sample_rate, engine_cfg.watermark_ms),
                    engine_cfg.max_drift_ppm,
                    engine_cfg.drift_correction,
                ),
                resample_in: allocate_planes(ring_channels, chunk),
                resample_out: allocate_planes(ring_channels, max_out),
                staging: vec![0.0; max_out * ring_channels],
                channels: ring_channels,
                asio_channel_map: (output_offset..output_offset + ring_channels).collect(),
                gain: cfg.linear_gain(),
                stats: Arc::clone(&stats),
            });

            specs.push(StreamSpec {
                kind: StreamKind::Output,
                device: Arc::clone(&info.handle),
                device_name: info.name.clone(),
                format,
                ch_map: mapped,
                input_writer: None,
                output_reader: Some(reader),
                master_core: is_master.then(|| Arc::clone(&self.core)),
                err_flag: Arc::clone(&self.stream_error),
                frames_per_buffer: chunk as u32,
                stats: Arc::clone(&stats),
                device_frames: {
                    let slot = Arc::new(AtomicUsize::new(0));
                    device_frames_per_stream.push(Arc::clone(&slot));
                    slot
                },
            });
            output_offset += ring_channels;
        }

        // 重采样滤波器的 group delay 直接加在音频路径上,`getLatencies()`
        // 得把它算进去 —— 报小了,宿主的录音对齐补偿就不够。各流可能不同
        // (直通的流是 0),取最大的那个。
        self.resampler_delay_frames = inputs
            .iter()
            .map(|s| s.resampler.output_delay_frames())
            .chain(outputs.iter().map(|s| s.resampler.output_delay_frames()))
            .max()
            .unwrap_or(0);

        // 把运行时状态和宿主回调装进核心。
        {
            let mut core = self.core.lock();
            core.inputs = inputs;
            core.outputs = outputs;
            core.callback = callback;
            core.buffers =
                AsioBufferSet::new(self.input_channel_count, self.output_channel_count, chunk);
            core.buffer_index = 0;
            core.accumulated = 0;
            core.sample_rate = sample_rate as f64;
            core.running = false;
        }
        self.samples_processed.store(0, Ordering::Relaxed);

        // 流在这个专用线程上创建并驻留。
        self.host = Some(StreamHost::spawn(specs)?);
        self.device_frames_per_stream = device_frames_per_stream;
        self.prepared.store(true, Ordering::Release);
        log::info!(
            "已准备 {} 路输入 / {} 路输出,ASIO 缓冲区 {} 帧 @ {} Hz",
            self.input_channel_count,
            self.output_channel_count,
            chunk,
            sample_rate
        );
        Ok(())
    }

    /// 启动所有设备流。
    ///
    /// 启动顺序是精心安排的,目的是避免开场时的欠载:
    ///
    /// 1. 先把宿主预先填好的输出缓冲(ASIO 约定是 index 1)推进输出环形缓冲;
    /// 2. 只启动**输入**流,让它们开始采集;
    /// 3. 等输入缓冲攒够数据(最多几百毫秒);
    /// 4. 启动**输出**流 —— 此时输出环形缓冲里已经有第 1 步放进去的数据;
    /// 5. 打开回调闸门,开始正常的缓冲区交换。
    ///
    /// 如果第 1 步之后就直接启动全部流,输出设备会在采集设备还没送来
    /// 任何数据时就开始跑,而宿主第一次要读输入缓冲时那里是空的。
    pub fn start(&mut self) -> Result<()> {
        if !self.prepared.load(Ordering::Acquire) {
            return Err(Error::NotRunning);
        }
        if self.running.load(Ordering::Acquire) {
            return Err(Error::AlreadyRunning);
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| Error::Internal("设备流尚未建立".into()))?;

        // 1. 预填充输出,并把状态归零。此时 `running` 仍是 false,
        //    所以即使主设备回调已经开始跑,`advance()` 也会直接返回。
        {
            let mut core = self.core.lock();
            core.buffer_index = 0;
            core.accumulated = 0;
            core.prime_outputs();
        }
        self.samples_processed.store(0, Ordering::Relaxed);

        host.start(StreamGroup::Inputs)?;

        // 3. 等输入缓冲攒够一个缓冲区的数据。
        self.wait_for_input_prime();

        // 4. 打开闸门。
        //
        // **必须在启动输出设备之前**。设备一旦 Start 就立刻开始从环形缓冲取
        // 数据,而这时里面只有预填充的那一点。闸门要是还没开,`advance()` 会
        // 直接返回、`drain()` 不执行 —— 缓冲只出不进,开头那一段必然欠载。
        // 实测:把这一步提前之后,启动期的欠载从几百帧降到 0。
        //
        // 提前开闸门是安全的,两个方向都过一遍:
        // * 输出方向此刻还没有任何回调 —— 流还没 Start,自然没有事件。
        // * 输入方向虽然早在第 2 步就启动了,但只有**输入作为时钟主设备**时
        //   它的回调才会推进引擎。那种情况下 `drain()` 会把数据写进输出环形
        //   缓冲,而那些数据只是先排队等设备启动(容量是水位的 6 倍,几毫秒的
        //   积压远够放),不会溢出。
        {
            let mut core = self.core.lock();
            core.buffer_index = 0;
            core.accumulated = 0;
            core.running = true;
        }

        // 5. 让输出设备开始播放。
        host.start(StreamGroup::Outputs)?;

        self.running.store(true, Ordering::Release);
        log::info!("引擎已启动");
        Ok(())
    }

    /// 等待各输入流的环形缓冲攒到各自的目标水位。
    ///
    /// 门槛不能只用 ASIO 缓冲区大小,也不能只用重采样器"下一次需要多少":
    /// 那样预热刚够第一次 `fill()` 消耗,环形缓冲立刻见底,等设备送来
    /// 下一批数据之前就欠载了。直接按引擎认定的目标水位预热,启动后
    /// 就有一整个缓冲余量,后续由漂移控制器维持。
    ///
    /// 超时取 300 ms。这个值是权衡的结果:太短压不住慢设备的启动延迟,
    /// 太长会让宿主的 `ASIOStart()` 明显卡顿。等待期间输出流还没启动,
    /// 所以环形缓冲不会被消耗,等一等是安全的。
    fn wait_for_input_prime(&self) {
        if self.input_channel_count == 0 {
            return;
        }
        let timeout = std::time::Duration::from_millis(300);
        let deadline = std::time::Instant::now() + timeout;

        loop {
            let ready = match self.core.try_lock() {
                Some(core) => core.inputs.iter().all(|s| {
                    let want = s.drift.target_frames().max(s.resampler.input_frames_next());
                    s.reader.available_frames() >= want
                }),
                // 拿不到锁说明有回调正在跑,那种情况下也没法更精确了。
                None => true,
            };
            if ready {
                log::debug!("输入流已预热完成");
                return;
            }
            if std::time::Instant::now() >= deadline {
                let pending = self
                    .core
                    .try_lock()
                    .map(|core| {
                        core.inputs
                            .iter()
                            .map(|s| {
                                (
                                    s.device_name.clone(),
                                    s.reader.available_frames(),
                                    s.drift.target_frames(),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for (name, have, want) in pending {
                    log::debug!("输入流 “{name}” 预热不足:{have} / {want} 帧,先启动");
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// 停止所有设备流。
    pub fn stop(&mut self) -> Result<()> {
        // 先掐掉回调入口,保证 `stop()` 返回后不会再有 bufferSwitch 飞出去
        // —— ASIO 规范对此有明确要求。
        {
            let mut core = self.core.lock();
            core.running = false;
            core.accumulated = 0;
        }
        self.running.store(false, Ordering::Release);

        if let Some(host) = self.host.as_ref() {
            host.stop()?;
        }
        log::info!(
            "引擎已停止,已处理 {} 帧",
            self.samples_processed.load(Ordering::Relaxed)
        );
        Ok(())
    }

    /// 关闭设备流并释放资源。
    pub fn dispose(&mut self) -> Result<()> {
        let _ = self.stop();
        // `StreamHost` 的 Drop 会结束线程,并在那里销毁所有流。
        self.host = None;
        {
            let mut core = self.core.lock();
            core.inputs.clear();
            core.outputs.clear();
            core.callback = Box::new(|_, _| {});
        }
        self.prepared.store(false, Ordering::Release);
        log::info!("引擎资源已释放");
        Ok(())
    }

    /// 已处理的采样帧数,对应 ASIO 的 sample position。
    ///
    /// 只读原子变量,不加锁 —— 宿主可能在 `bufferSwitch` 内部调用它。
    pub fn sample_position(&self) -> u64 {
        self.samples_processed.load(Ordering::Relaxed)
    }

    /// 状态快照,控制面板和日志用。
    ///
    /// 锁只等 [`STATUS_LOCK_TIMEOUT`] 那么久,不无限期阻塞 —— 这个函数是从
    /// GUI 线程调的,拿不到就退化成只返回静态信息。
    ///
    /// 早先这里用的是 `try_lock`(一次不等)。本意是"绝不阻塞",实测下来
    /// 却走到了另一个极端:实时状态连续三十秒都显示"暂时读不到统计(音频
    /// 回调正忙)"。音频线程持锁虽然只有几毫秒,但它每 22 ms 就来一轮,
    /// 撞上的概率高得离谱。等一小会儿是划算的 —— 界面晚 20 ms 拿到数据,
    /// 总好过一直拿不到。
    pub fn status(&self) -> EngineStatus {
        let (mut stream_stats, warnings, live) = match self.core.try_lock_for(STATUS_LOCK_TIMEOUT) {
            Some(mut core) => {
                let stats = core.status_rows();
                // 只**取数**,不打印 —— 见下。
                let warnings = core.take_audio_warnings();
                (stats, warnings, true)
            }
            None => (Vec::new(), Vec::new(), false),
        };
        // 落盘放在锁外。`log::warn!` 是阻塞的,而音频线程正靠这把锁推进时钟
        // —— 在锁内打印等于让磁盘 I/O 去和实时线程抢锁。上面那句
        // `try_lock_for(20ms)` 之所以要等 20 ms,也是同一件事的另一面。
        for warning in &warnings {
            log::warn!("{warning}");
        }
        if !live {
            log::debug!("状态查询时音频核心正忙,本次只返回静态信息");
        }
        // 谁在当时钟基准只有 `Engine` 知道(音频核心只管缓冲),在这里补上。
        for stat in &mut stream_stats {
            stat.is_clock_master = self.clock_master == (stat.kind, stat.index);
        }
        // 设备块大小也一样 —— 那是回调量出来的,存在 Engine 这边。
        // `status_rows` 的顺序是先输入后输出,和 `device_frames_per_stream`
        // 的收集顺序一致,所以能按位置对回去。
        for (stat, frames) in stream_stats
            .iter_mut()
            .zip(self.device_frames_per_stream.iter())
        {
            stat.device_frames = frames.load(Ordering::Relaxed);
        }
        EngineStatus {
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            input_channels: self.input_channel_count,
            output_channels: self.output_channel_count,
            running: self.running.load(Ordering::Acquire),
            sample_position: self.sample_position(),
            // 原子读,和上面那笔锁无关 —— 拿不到锁时也能给出真实值,
            // 界面上的丢帧数不会在真值和 0 之间跳。
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            streams: self.stream_infos.clone(),
            stream_stats,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 检查同一方向上是否有设备被重复配置且通道重叠。
fn check_duplicates_for(resolved: &[DeviceInfo], config: &Config, kind: StreamKind) -> Result<()> {
    let refs: Vec<(&str, &PigStreamConfig)> = resolved
        .iter()
        .map(|d| d.name.as_str())
        .zip(match kind {
            StreamKind::Input => config.active_inputs().map(|(_, c)| c).collect::<Vec<_>>(),
            StreamKind::Output => config.active_outputs().map(|(_, c)| c).collect::<Vec<_>>(),
        })
        .collect();
    devices::check_duplicates(&refs, kind)
}

/// 计算环形缓冲的容量。
///
/// `watermark_ms` 是目标水位(毫秒)。容量取三者中最大的一个:水位的若干倍
/// (水位附近要有调节余地,漂移补偿靠的就是在这里加减)、至少 0.25 秒、以及
/// 至少四个 ASIO 缓冲区(一次 `advance` 至少要能整块进出)。
fn ring_capacity(chunk: usize, watermark_ms: f64, sample_rate: u32) -> usize {
    let by_watermark =
        (sample_rate as f64 * watermark_ms / 1000.0 * RING_CAPACITY_FACTOR).ceil() as usize;
    let by_time = (sample_rate as f64 * RING_MIN_SECONDS).ceil() as usize;
    by_watermark.max(by_time).max(chunk * 4).max(1)
}

/// 把配置里的目标水位(毫秒)换算成帧数。
///
/// 单位是时间而不是「几个缓冲区」,所以这个换算是**唯一**的:帧数 = 采样率
/// × 毫秒数,和 `buffer_size_samples` 无关。
///
/// 早先这里算的是 `chunk × watermark`。那个写法有个坑:用户为了降延迟把缓冲区
/// 从 1024 调到 128,水位从 64 ms 缩到 8 ms —— 而 8 ms 比一块设备的回调周期
/// (WASAPI 共享模式下普遍 10 ms)还短,**本来是为了降延迟,结果反而开始欠载**。
/// 界面上只显示「3.00 个缓冲区」,看不出那是 8 ms,于是这个坑很难爬出来。
///
/// # 为什么不再兜一个绝对时间下限
///
/// 这里一度有个 30 ms 的硬下限,正是为了挡住上面那种情况。但那等于把用户的
/// 选择悄悄盖掉 —— 用户设 1,实际跑 30 ms,无论界面还是日志都看不出来,
/// 想压延迟就会撞上一堵看不见的墙。
///
/// 把单位本身改成时间之后,下限就没有存在的意义了:用户填的数字**就是**
/// 绝对时间,不再会随缓冲区大小缩水。真正该做的是让这个数字在界面上可读、
/// 并且和水位不够时的反馈(实时状态里的「欠载 / 溢出」两列)对得上。
///
/// 唯一绕不开的是设备回调的抖动:抖动越大,水位就得越厚。但那取决于用户的
/// 声卡和机器,该让他自己试,而不是由代码猜一个数。
fn target_watermark_frames(sample_rate: u32, watermark_ms: f64) -> usize {
    ((sample_rate as f64 * watermark_ms / 1000.0).round() as usize).max(1)
}

/// 决定某条流实际用哪种重采样质量。
///
/// **时钟主设备**在设备采样率与 ASIO 一致时完全不需要重采样:ASIO 的时钟
/// 就是它的时钟,生产(设备回调写进 ring)和消费(`advance` 攒够一块后搬运)
/// 都由它驱动,长期速率必然相等,不存在漂移可言。这条路径上的 sinc 滤波器
/// 于是只剩一个副作用 —— 它的 group delay 直接加在音频上。直通掉它。
///
/// **从设备不行**:哪怕采样率相同,它也得靠变速重采样把时钟软锁到主设备上。
///
/// `nominal_ratio` 由两个整数相除得到,两边相等时是精确的 1.0,可以直接比。
fn stream_quality(
    configured: ResampleQuality,
    nominal_ratio: f64,
    is_master: bool,
    device_name: &str,
) -> ResampleQuality {
    if is_master && nominal_ratio == 1.0 {
        if configured != ResampleQuality::None {
            log::info!(
                "时钟主设备 “{device_name}” 与 ASIO 采样率一致,直通不做重采样\
                 (省掉滤波器的 group delay)"
            );
        }
        ResampleQuality::None
    } else {
        configured
    }
}

/// 一轮 `advance` 处理不完时,把超出的累计帧数丢掉,返回 (丢掉的帧数, 保留的余数)。
///
/// 单位必须是**帧**。`accumulated` 是帧,除以缓冲区大小得到的是"多少个
/// ASIO 缓冲区",这两个数差着一个 `buffer_size` 的因子 —— 早先直接把那个商
/// 当帧数记了,于是日志写成"丢弃累计 3 帧",而实际丢掉的是 3 × 1024 = 3072 帧。
///
/// 余数(`< size` 的那部分)不算丢:它下次还能凑成整块,留着才是对的。
fn drop_excess_accumulated(accumulated: usize, size: usize) -> (usize, usize) {
    debug_assert!(size > 0, "缓冲区大小不可能为 0,除之前先确认");
    let buffers = accumulated / size;
    (buffers * size, accumulated % size)
}

/// 取走某条流攒下的重采样失败计数,拼成待打印的文字。
///
/// 和 [`AudioCore::take_audio_warnings`] 一样,只取数不打印 —— 调用方要拿着
/// 返回的字符串出锁之后再落盘。只能从非实时路径调用。
fn take_resample_failures(device_name: &str, stats: &RingStats) -> Option<String> {
    let failures = stats.resample_failures.swap(0, Ordering::Relaxed);
    (failures > 0).then(|| format!("设备 “{device_name}” 重采样失败 {failures} 次"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 环形缓冲容量随水位与时间增长() {
        let c = ring_capacity(64, 20.0, 48_000);
        assert!(c >= (48_000.0 * RING_MIN_SECONDS) as usize);

        let c = ring_capacity(8192, 20.0, 48_000);
        assert!(c >= 8192 * 4);
    }

    #[test]
    fn 目标水位按毫秒换算且不随缓冲区大小漂移() {
        // 单位是时间,所以帧数只跟采样率和毫秒数有关 —— 缓冲区大小不再参与。
        // 这是这个函数最重要的性质:以前算的是 `chunk × 倍数`,用户为了降延迟
        // 把缓冲区从 1024 调到 128,水位会跟着从 64 ms 缩到 8 ms,比一块设备的
        // 回调周期还短,于是「降延迟」反而变成「欠载」。
        assert_eq!(target_watermark_frames(48_000, 30.0), 1440);
        assert_eq!(target_watermark_frames(48_000, 10.0), 480);
        assert_eq!(target_watermark_frames(44_100, 10.0), 441);

        // 同一毫秒数在任何采样率下都是同一段时间(±1 帧取整误差)。
        for ms in [5.0, 8.0, 30.0, 64.0] {
            let frames = target_watermark_frames(48_000, ms);
            let back = frames as f64 / 48_000.0 * 1000.0;
            assert!((back - ms).abs() < 0.1, "{ms} ms 往返成了 {back} ms");
        }

        // 极端值兜底:至少留一帧,不能让下游一帧数据都拿不到。
        assert_eq!(target_watermark_frames(48_000, 0.0), 1);
        assert_eq!(target_watermark_frames(0, 30.0), 1);
    }

    #[test]
    fn 丢帧计数按帧而不是按缓冲区个数() {
        // 1024 帧的缓冲区、累计 3072 帧:丢掉 3 整块 = 3072 帧,不留余数。
        assert_eq!(drop_excess_accumulated(3072, 1024), (3072, 0));
        // 早先的写法会把「3 个缓冲区」当成「3 帧」记下来 —— 差 1024 倍。
        assert_ne!(drop_excess_accumulated(3072, 1024).0, 3);

        // 不足一块的余数不算丢:下次还能凑成整块。
        assert_eq!(drop_excess_accumulated(2500, 1024), (2048, 452));
        assert_eq!(drop_excess_accumulated(1023, 1024), (0, 1023));
        // 刚好一块:整块都要丢,余数为 0(边界不能落到"保留一整块"那边去)。
        assert_eq!(drop_excess_accumulated(1024, 1024), (1024, 0));
    }

    #[test]
    fn asio_缓冲指针按块偏移且互不重叠() {
        let mut b = AsioBufferSet::new(2, 2, 4);
        let p0 = b.input_ptr(0, 0);
        let p1 = b.input_ptr(0, 1);
        assert_ne!(p0, p1);
        // index 1 正好在 index 0 之后 buffer_size 个 f32。
        assert_eq!(unsafe { p1.offset_from(p0) }, 4);

        // 不同通道的缓冲互不重叠。
        let mut b2 = AsioBufferSet::new(2, 2, 4);
        let a = b2.input_ptr(0, 0);
        let c = b2.input_ptr(1, 0);
        assert_ne!(a, c);
        assert!(unsafe { a.offset_from(c) }.abs() >= 8);
    }

    #[test]
    fn 时钟主设备在同采样率下走直通() {
        // 主设备定义 ASIO 的时钟,不存在漂移可言 —— 直通能省掉 sinc 滤波器的
        // group delay。
        assert_eq!(
            stream_quality(ResampleQuality::Sinc, 1.0, true, "主设备"),
            ResampleQuality::None
        );
        // 从设备即使采样率相同也要变速(把时钟软锁到主设备),不能直通。
        assert_eq!(
            stream_quality(ResampleQuality::Sinc, 1.0, false, "从设备"),
            ResampleQuality::Sinc
        );
        // 主设备但设备采样率与 ASIO 不同:仍要做真正的采样率转换。
        assert_eq!(
            stream_quality(ResampleQuality::Sinc, 0.5, true, "主设备"),
            ResampleQuality::Sinc
        );
        assert_eq!(
            stream_quality(ResampleQuality::Sinc, 2.0, true, "主设备"),
            ResampleQuality::Sinc
        );
        // 用户显式设的档位该保留的保留、该直通的直通。
        assert_eq!(
            stream_quality(ResampleQuality::None, 1.0, false, "从设备"),
            ResampleQuality::None
        );
        assert_eq!(
            stream_quality(ResampleQuality::Fast, 0.5, false, "从设备"),
            ResampleQuality::Fast
        );
    }
}
