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

use parking_lot::RwLock;

use crate::error::{Result, StreamKind};

mod cpal_backend;

#[cfg(windows)]
mod wasapi;

pub use cpal_backend::CpalBackend;

#[cfg(windows)]
pub use wasapi::WasapiBackend;

/// 用哪个音频后端。
///
/// 只是"挑哪个实现"的名字,所以放在这里而不是配置模块里 —— 配置层引用它,
/// 反过来不成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// cpal(经它的 WASAPI 后端)。
    ///
    /// **这是默认值。** 直写 WASAPI 那个后端虽然已在真机上验证与它指标相当,
    /// 但还没在真实宿主里长期跑过;等低延迟 period 接上之后再考虑改默认。
    #[default]
    Cpal,
    /// 直写 WASAPI。将来低延迟 period 的收益只有它拿得到。
    Wasapi,
    /// 自动:优先 WASAPI,这台机器上不可用时退回 cpal。
    Auto,
}

impl BackendKind {
    /// 解析配置或环境变量里的写法。大小写和首尾空格都容忍 —— 手写的
    /// 环境变量写歪是常事。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(BackendKind::Auto),
            "cpal" => Some(BackendKind::Cpal),
            "wasapi" => Some(BackendKind::Wasapi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Auto => "auto",
            BackendKind::Cpal => "cpal",
            BackendKind::Wasapi => "wasapi",
        }
    }
}

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

/// 当前后端,以及它是按哪个选择造出来的。
///
/// 用 `RwLock + Arc` 而不是 `OnceLock`:控制面板改了配置之后,下一次枚举设备
/// 或打开流就该用新的后端,不必重启整个程序。读端拿的是 `Arc`,所以锁只会
/// 在取指针的那一瞬间被持有 —— 不会挡在音频路径上。
static BACKEND: RwLock<Option<(BackendKind, Arc<dyn Backend>)>> = RwLock::new(None);

/// 取当前后端。没人调过 [`select`] 时按环境变量或默认值确定。
pub fn current() -> Arc<dyn Backend> {
    if let Some((_, backend)) = BACKEND.read().as_ref() {
        return Arc::clone(backend);
    }

    let kind = env_override().unwrap_or_default();
    let backend = build(kind);

    let mut slot = BACKEND.write();
    // 两个线程可能同时走到这里 —— 后到的用先到的那份,免得凭空多造一个后端
    // (那会重复打日志,也让"当前后端是谁"变得含糊)。
    if let Some((_, existing)) = slot.as_ref() {
        return Arc::clone(existing);
    }
    *slot = Some((kind, Arc::clone(&backend)));
    backend
}

/// 按配置选定后端。
///
/// 环境变量 `PIGASIO_BACKEND` 优先级最高 —— 临时覆盖配置排查问题,不必去改
/// 配置文件。选择没变时直接返回:这个函数每次应用配置都会被调用。
pub fn select(preferred: BackendKind) {
    let kind = env_override().unwrap_or(preferred);

    let mut slot = BACKEND.write();
    if matches!(slot.as_ref(), Some((current, _)) if *current == kind) {
        return;
    }
    let backend = build(kind);
    *slot = Some((kind, Arc::clone(&backend)));
}

/// 环境变量里的覆盖值。
fn env_override() -> Option<BackendKind> {
    let raw = std::env::var("PIGASIO_BACKEND").ok()?;
    match BackendKind::parse(&raw) {
        Some(kind) => Some(kind),
        None => {
            log::warn!(
                "PIGASIO_BACKEND 的值 “{raw}” 无法识别(可选 auto / cpal / wasapi),已忽略"
            );
            None
        }
    }
}

/// 按种类造一个后端。无论走哪条分支都会记一条日志 —— 排查"到底用的哪个
/// 后端"时,这条就是答案。
fn build(kind: BackendKind) -> Arc<dyn Backend> {
    let backend: Arc<dyn Backend> = match kind {
        BackendKind::Cpal => Arc::new(CpalBackend::new()),
        #[cfg(windows)]
        BackendKind::Wasapi => Arc::new(WasapiBackend::new()),
        #[cfg(not(windows))]
        BackendKind::Wasapi => {
            log::warn!("这个系统上没有 WASAPI 后端,改用 cpal");
            Arc::new(CpalBackend::new())
        }
        BackendKind::Auto => {
            #[cfg(windows)]
            {
                let wasapi = WasapiBackend::new();
                if wasapi.available() {
                    Arc::new(wasapi)
                } else {
                    log::warn!("WASAPI 后端在这台机器上不可用,退回 cpal");
                    Arc::new(CpalBackend::new())
                }
            }
            #[cfg(not(windows))]
            {
                Arc::new(CpalBackend::new())
            }
        }
    };
    log::info!("音频后端:{}", backend.name());
    backend
}
