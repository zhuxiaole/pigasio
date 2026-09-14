//! `IASIO` 接口的实现。
//!
//! # ASIO 不是规矩的 COM
//!
//! 宿主会:
//!
//! 1. 从注册表 `HKLM\SOFTWARE\ASIO\<名字>\CLSID` 读到我们的 CLSID;
//! 2. 调 `CoCreateInstance(CLSID, ..., riid = CLSID, ...)` —— **把 CLSID
//!    当 IID 用**;
//! 3. 把拿到的 `void*` 直接盲转成 `IASIO*`,从不调用 `QueryInterface`。
//!
//! 所以驱动对象必须满足两个苛刻条件:第一个字段是 `IASIO` 的虚表指针
//! (否则宿主取到的函数地址全部错位),并且 `QueryInterface` 收到自己的
//! CLSID 时要返回自身。
//!
//! # 死锁红线
//!
//! `state` 的锁会被音频回调在 `bufferSwitch` 期间持有。宿主完全可能在
//! 自己的 `bufferSwitch` 处理里回头调用 `getSamplePosition()` 或
//! `outputReady()` —— 这两个接口因此**绝不能**去碰那把锁。
//! 它们读的是 `DriverObject` 上的原子字段,由音频回调直接维护。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use pigasio_core::engine::BufferSwitchCallback;
use pigasio_core::{Engine, Error as CoreError, StreamKind};

use crate::abi::*;

// ---------------------------------------------------------------------------
// 虚表
// ---------------------------------------------------------------------------

/// 驱动对象的虚表。整个驱动只有一个实现,所以虚表是静态的,
/// 所有实例共享同一份,`DriverObject` 里只存一个指针。
static IASIO_VTBL: IAsioVtbl = IAsioVtbl {
    query_interface: vt_query_interface,
    add_ref: vt_add_ref,
    release: vt_release,
    init: vt_init,
    get_driver_name: vt_get_driver_name,
    get_driver_version: vt_get_driver_version,
    get_error_message: vt_get_error_message,
    start: vt_start,
    stop: vt_stop,
    get_channels: vt_get_channels,
    get_latencies: vt_get_latencies,
    get_buffer_size: vt_get_buffer_size,
    can_sample_rate: vt_can_sample_rate,
    get_sample_rate: vt_get_sample_rate,
    set_sample_rate: vt_set_sample_rate,
    get_clock_sources: vt_get_clock_sources,
    set_clock_source: vt_set_clock_source,
    get_sample_position: vt_get_sample_position,
    get_channel_info: vt_get_channel_info,
    create_buffers: vt_create_buffers,
    dispose_buffers: vt_dispose_buffers,
    control_panel: vt_control_panel,
    future: vt_future,
    output_ready: vt_output_ready,
};

/// 驱动对象的 COM 布局。
///
/// `vtbl` **必须**是第一个字段 —— 宿主把对象首地址当 `IASIO*` 用,
/// 而 `IASIO*` 解引用读到的第一个机器字就是虚表指针。
#[repr(C)]
pub struct DriverObject {
    vtbl: *const IAsioVtbl,
    ref_count: AtomicU32,

    // ---- 以下三个字段专门服务于「不能加锁」的接口 ----
    /// 已处理的采样帧数,由音频回调直接累加。
    sample_position: Arc<AtomicU64>,
    /// 当前采样率(Hz)。0 表示尚未初始化。
    sample_rate: AtomicU32,
    /// 是否正在运行。
    running: AtomicBool,

    state: Mutex<DriverState>,
}

// 安全性论证:
// * `vtbl` 指向 `static IASIO_VTBL`,永不失效,内容不可变;
// * 引用计数与三个快照字段都是原子量;
// * `state` 由 `Mutex` 保护。
// COM 的 `ThreadingModel` 注册为 `Both`,对象必须能从任意线程访问。
unsafe impl Send for DriverObject {}
unsafe impl Sync for DriverObject {}

impl DriverObject {
    /// 创建一个引用计数为 1 的新对象。
    pub fn create() -> *mut DriverObject {
        let obj = Box::new(DriverObject {
            vtbl: &IASIO_VTBL,
            ref_count: AtomicU32::new(1),
            sample_position: Arc::new(AtomicU64::new(0)),
            sample_rate: AtomicU32::new(0),
            running: AtomicBool::new(false),
            state: Mutex::new(DriverState::new()),
        });
        Box::into_raw(obj)
    }

    /// 采样位置的无锁句柄,交给音频回调去累加。
    fn position_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.sample_position)
    }
}

/// 驱动的可变状态。除原子快照之外的字段都在这把锁后面。
struct DriverState {
    /// 引擎在 `init()` 时建立,`release` 或 `dispose()` 时丢弃。
    engine: Option<Engine>,
    /// 宿主提供的回调表。
    callbacks: ASIOCallbacks,
    /// 宿主传给 `init()` 的窗口句柄。只存不用,所以保存成整数,
    /// 免得让整个结构体变成 `!Send`。
    sys_handle: usize,
    /// 最近一次失败的描述,`getErrorMessage()` 返回它。
    last_error: String,
    /// 已经创建过缓冲区。
    prepared: bool,
    /// 每个通道是否被 `createBuffers()` 启用过。
    active_inputs: Vec<bool>,
    active_outputs: Vec<bool>,
    /// 实际生效的配置文件路径。
    config_path: Option<std::path::PathBuf>,
}

impl DriverState {
    fn new() -> Self {
        DriverState {
            engine: None,
            callbacks: ASIOCallbacks::default(),
            sys_handle: 0,
            last_error: String::new(),
            prepared: false,
            active_inputs: Vec::new(),
            active_outputs: Vec::new(),
            config_path: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把 `this` 还原成对象引用。
///
/// # Safety
/// `this` 必须来自本模块的 `DriverObject::create()`,且在调用期间仍然存活
/// (由 COM 引用计数保证)。
unsafe fn object<'a>(this: *mut core::ffi::c_void) -> &'a DriverObject {
    &*(this as *const DriverObject)
}

/// 捕获 panic。
///
/// 驱动跑在宿主进程里,一个漏出去的 panic 会让整个 DAW 消失,用户刚录的
/// 东西一起没了。宁可返回一个错误让宿主提示“驱动有问题”,也不要同归于尽。
fn guard<T>(context: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => {
            log::error!("PigASIO 在 {context} 中 panic,已拦截");
            fallback
        }
    }
}

/// 业务失败的简写:(ASIO 错误码, 给用户看的说明)。
type Failure = (ASIOError, String);

fn fail(code: ASIOError, message: impl Into<String>) -> Failure {
    (code, message.into())
}

/// 统一的进入/退出包装:记录日志,并把错误写进 `last_error`
/// 以便 `getErrorMessage()` 能把它交给宿主显示。
fn enter<F>(context: &str, state: &mut DriverState, f: F) -> ASIOError
where
    F: FnOnce(&mut DriverState) -> core::result::Result<(), Failure>,
{
    log::debug!("--- 进入 {context}");
    // 用 `&mut *state` 重新借一次,这样闭包用完后 state 还能继续用。
    match f(&mut *state) {
        Ok(()) => {
            log::debug!("--- 离开 {context} [OK]");
            ase::OK
        }
        Err((code, message)) => {
            log::warn!("--- 离开 {context} 失败({code}):{message}");
            state.last_error = message;
            code
        }
    }
}

/// 把核心库的错误映射成 ASIO 错误码。
fn map_core_error(e: &CoreError) -> ASIOError {
    match e {
        CoreError::Config(_) => ase::INVALID_PARAMETER,
        CoreError::DeviceNotFound { .. } => ase::NOT_PRESENT,
        CoreError::DeviceOpen { .. } => ase::HW_MALFUNCTION,
        CoreError::ChannelOutOfRange { .. } => ase::INVALID_PARAMETER,
        CoreError::AlreadyRunning | CoreError::NotRunning => ase::INVALID_MODE,
        CoreError::Resampler(_) => ase::NO_MEMORY,
        _ => ase::HW_MALFUNCTION,
    }
}

/// 往宿主提供的定长字符缓冲里写字符串。
///
/// ASIO 的字符串字段没有长度参数,长度由协议定死(驱动名 32 字节、
/// 错误信息 124 字节),必须自己截断并补 0。
///
/// # Safety
/// `dst` 必须指向至少 `capacity` 字节的可写内存。
unsafe fn write_c_string(dst: *mut u8, capacity: usize, s: &str) {
    if dst.is_null() || capacity == 0 {
        return;
    }
    // 编码成宿主会按**系统 ANSI 代码页**解释的字节(不是 UTF-8),
    // 并留一个字节给结尾的 0。驱动名和错误信息都会走这里,而错误信息
    // 是中文的 —— 用 UTF-8 写进去宿主只会显示乱码。
    let encoded = crate::abi::encode_for_asio(s, capacity - 1);
    core::ptr::copy_nonoverlapping(encoded.as_ptr(), dst, encoded.len());
    *dst.add(encoded.len()) = 0;
}

// ---------------------------------------------------------------------------
// IUnknown
// ---------------------------------------------------------------------------

unsafe extern "system" fn vt_query_interface(
    this: *mut core::ffi::c_void,
    riid: *const Guid,
    ppv: *mut *mut core::ffi::c_void,
) -> i32 {
    guard("QueryInterface", E_FAIL, || {
        if ppv.is_null() {
            return E_POINTER;
        }
        *ppv = core::ptr::null_mut();
        if riid.is_null() {
            return E_INVALIDARG;
        }
        let iid = *riid;

        // 驱动只实现 IASIO 这一个接口,而 IASIO 连 IID 都没有,
        // 所以除了 IUnknown,就只认自己的 CLSID。
        if iid == IID_IUNKNOWN || iid == CLSID_PIGASIO {
            object(this).ref_count.fetch_add(1, Ordering::Relaxed);
            *ppv = this;
            S_OK
        } else {
            log::debug!("QueryInterface 收到未实现的 IID {iid:?}");
            E_NOINTERFACE
        }
    })
}

unsafe extern "system" fn vt_add_ref(this: *mut core::ffi::c_void) -> u32 {
    guard("AddRef", 1, || {
        object(this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
    })
}

unsafe extern "system" fn vt_release(this: *mut core::ffi::c_void) -> u32 {
    guard("Release", 0, || {
        let obj = object(this);
        let previous = obj.ref_count.fetch_sub(1, Ordering::AcqRel);
        let remaining = previous.saturating_sub(1);
        if remaining == 0 {
            log::info!("PigASIO 驱动对象已释放");
            // 在这里关掉设备流、结束音频线程。
            {
                let mut state = obj.state.lock();
                if let Some(mut engine) = state.engine.take() {
                    let _ = engine.dispose();
                }
            }
            drop(Box::from_raw(this as *mut DriverObject));
        }
        remaining
    })
}

// ---------------------------------------------------------------------------
// IASIO
// ---------------------------------------------------------------------------

unsafe extern "system" fn vt_init(
    this: *mut core::ffi::c_void,
    sys_handle: *mut core::ffi::c_void,
) -> ASIOBool {
    guard("init", ASIO_FALSE, || {
        let obj = object(this);
        let mut state = obj.state.lock();

        if state.engine.is_some() {
            state.last_error = "init() 被调用了两次".into();
            log::warn!("{}", state.last_error);
            return ASIO_FALSE;
        }
        state.sys_handle = sys_handle as usize;

        let code = enter("init", &mut state, |st| {
            // 配置来源优先级:环境变量 > 宿主 exe 同目录 > 用户目录 > 内置默认值。
            let host_dir = crate::host_executable_dir();
            let (config, path) = pigasio_core::config::load(host_dir.as_deref())
                .map_err(|e| fail(map_core_error(&e), e.to_string()))?;
            st.config_path = path;

            log::info!(
                "配置:{} Hz,{} 帧缓冲,{} 路输入 / {} 路输出",
                config.sample_rate,
                config.buffer_size_samples,
                config.inputs.len(),
                config.outputs.len()
            );

            let engine = Engine::new(config).map_err(|e| fail(map_core_error(&e), e.to_string()))?;
            log::info!(
                "引擎就绪:{} 个 ASIO 输入通道,{} 个 ASIO 输出通道",
                engine.input_channel_count(),
                engine.output_channel_count()
            );

            // 发布无锁快照,给 getSamplePosition / outputReady 用。
            obj.sample_rate.store(engine.sample_rate(), Ordering::Relaxed);
            obj.sample_position
                .store(0, Ordering::Relaxed);

            st.active_inputs = vec![false; engine.input_channel_count()];
            st.active_outputs = vec![false; engine.output_channel_count()];
            st.engine = Some(engine);
            Ok(())
        });

        if code == ase::OK {
            ASIO_TRUE
        } else {
            ASIO_FALSE
        }
    })
}

unsafe extern "system" fn vt_get_driver_name(_this: *mut core::ffi::c_void, name: *mut u8) {
    guard("getDriverName", (), || {
        // 宿主给的是 32 字节,ASIO 对超长没有定义行为,所以自己截断。
        unsafe { write_c_string(name, 32, pigasio_core::DRIVER_NAME) };
    });
}

unsafe extern "system" fn vt_get_driver_version(_this: *mut core::ffi::c_void) -> i32 {
    // ASIO 没规定版本号格式。这里报告 ASIO 2.3,让宿主启用 2.x 特性
    // (比如 outputReady),同时把驱动自己的版本放在低 16 位。
    (2 << 16) | 3
}

unsafe extern "system" fn vt_get_error_message(this: *mut core::ffi::c_void, string: *mut u8) {
    guard("getErrorMessage", (), || {
        let message = {
            let obj = object(this);
            let state = obj.state.lock();
            state.last_error.clone()
        };
        // ASIODriverInfo::errorMessage 是 124 字节,留一个给结尾的 0。
        unsafe { write_c_string(string, 124, &message) };
    });
}

unsafe extern "system" fn vt_start(this: *mut core::ffi::c_void) -> ASIOError {
    guard("start", ase::HW_MALFUNCTION, || {
        let obj = object(this);
        let mut state = obj.state.lock();

        if !state.prepared {
            state.last_error = "start() 之前必须先调用 createBuffers()".into();
            return ase::INVALID_MODE;
        }

        let code = enter("start", &mut state, |st| {
            let engine = st
                .engine
                .as_mut()
                .ok_or_else(|| fail(ase::INVALID_MODE, "引擎未初始化"))?;
            engine
                .start()
                .map_err(|e| fail(map_core_error(&e), e.to_string()))
        });

        if code == ase::OK {
            obj.sample_position.store(0, Ordering::Relaxed);
            obj.running.store(true, Ordering::Release);
        }
        code
    })
}

unsafe extern "system" fn vt_stop(this: *mut core::ffi::c_void) -> ASIOError {
    guard("stop", ase::HW_MALFUNCTION, || {
        let obj = object(this);
        // 先摘掉运行标志:`stop()` 返回之后绝不能再有 bufferSwitch 飞向宿主,
        // 这是 ASIO 规范的硬性要求。引擎内部的回调入口由 `Engine::stop()`
        // 负责关闭,两边都关才能覆盖所有时序。
        obj.running.store(false, Ordering::Release);

        let mut state = obj.state.lock();
        enter("stop", &mut state, |st| {
            let engine = st
                .engine
                .as_mut()
                .ok_or_else(|| fail(ase::INVALID_MODE, "引擎未初始化"))?;
            engine
                .stop()
                .map_err(|e| fail(map_core_error(&e), e.to_string()))
        })
    })
}

unsafe extern "system" fn vt_get_channels(
    this: *mut core::ffi::c_void,
    num_input: *mut i32,
    num_output: *mut i32,
) -> ASIOError {
    guard("getChannels", ase::HW_MALFUNCTION, || {
        if num_input.is_null() || num_output.is_null() {
            return E_INVALIDARG;
        }
        let obj = object(this);
        let state = obj.state.lock();
        let Some(engine) = state.engine.as_ref() else {
            return ase::INVALID_MODE;
        };
        unsafe {
            *num_input = engine.input_channel_count() as i32;
            *num_output = engine.output_channel_count() as i32;
            log::debug!("getChannels() -> {} 输入 / {} 输出", *num_input, *num_output);
        }
        ase::OK
    })
}

unsafe extern "system" fn vt_get_latencies(
    this: *mut core::ffi::c_void,
    input_latency: *mut i32,
    output_latency: *mut i32,
) -> ASIOError {
    guard("getLatencies", ase::HW_MALFUNCTION, || {
        if input_latency.is_null() || output_latency.is_null() {
            return E_INVALIDARG;
        }
        let obj = object(this);
        let state = obj.state.lock();
        let Some(engine) = state.engine.as_ref() else {
            return ase::INVALID_MODE;
        };
        // ASIO 的延迟定义是「bufferSwitch 到声音真正进出」的时间。
        //
        // 多设备驱动里这个值由环形缓冲维持的水位决定 —— 数据要先在
        // 环形缓冲里排队,然后才被重采样送进设备(输出方向)或者被宿主
        // 读到(输入方向)。所以延迟约等于 `buffer_size * buffer_watermark`
        // 帧,而不是一个缓冲区。
        //
        // 报大了只是让宿主的对齐补偿多留一点余量;报小了才会让录音对不齐,
        // 所以这里不做任何"乐观"的缩减。
        let frames = (engine.buffer_size() as f64 * engine.config().engine.buffer_watermark)
            .round() as i32;
        unsafe {
            *input_latency = frames;
            *output_latency = frames;
        }
        ase::OK
    })
}

unsafe extern "system" fn vt_get_buffer_size(
    this: *mut core::ffi::c_void,
    min_size: *mut i32,
    max_size: *mut i32,
    preferred_size: *mut i32,
    granularity: *mut i32,
) -> ASIOError {
    guard("getBufferSize", ase::HW_MALFUNCTION, || {
        if min_size.is_null()
            || max_size.is_null()
            || preferred_size.is_null()
            || granularity.is_null()
        {
            return E_INVALIDARG;
        }
        let obj = object(this);
        let state = obj.state.lock();
        let Some(engine) = state.engine.as_ref() else {
            return ase::INVALID_MODE;
        };
        let (min, max, preferred, gran) = engine.buffer_size_range();
        unsafe {
            *min_size = min;
            *max_size = max;
            *preferred_size = preferred;
            *granularity = gran;
        }
        log::debug!("getBufferSize() -> {min}..{max}(首选 {preferred})");
        ase::OK
    })
}

unsafe extern "system" fn vt_can_sample_rate(
    this: *mut core::ffi::c_void,
    sample_rate: ASIOSampleRate,
) -> ASIOError {
    guard("canSampleRate", ase::HW_MALFUNCTION, || {
        let obj = object(this);
        let current = obj.sample_rate.load(Ordering::Relaxed);
        if current == 0 {
            return ase::INVALID_MODE;
        }
        // 采样率由配置文件决定,不支持宿主动态切换:设备是按配置里的
        // 采样率打开的,嘴上答应切到 96 kHz 而实际还在 48 kHz 跑,
        // 会让宿主的录音对齐整个错掉。
        if (sample_rate - current as f64).abs() < 0.5 {
            ase::OK
        } else {
            log::debug!("canSampleRate({sample_rate}) -> 不支持,配置为 {current} Hz");
            ase::NO_CLOCK
        }
    })
}

unsafe extern "system" fn vt_get_sample_rate(
    this: *mut core::ffi::c_void,
    sample_rate: *mut ASIOSampleRate,
) -> ASIOError {
    guard("getSampleRate", ase::HW_MALFUNCTION, || {
        if sample_rate.is_null() {
            return E_INVALIDARG;
        }
        let obj = object(this);
        let current = obj.sample_rate.load(Ordering::Relaxed);
        if current == 0 {
            return ase::INVALID_MODE;
        }
        unsafe { *sample_rate = current as f64 };
        ase::OK
    })
}

unsafe extern "system" fn vt_set_sample_rate(
    this: *mut core::ffi::c_void,
    sample_rate: ASIOSampleRate,
) -> ASIOError {
    guard("setSampleRate", ase::HW_MALFUNCTION, || {
        let obj = object(this);
        let current = obj.sample_rate.load(Ordering::Relaxed);
        if current == 0 {
            return ase::INVALID_MODE;
        }
        if (sample_rate - current as f64).abs() < 0.5 {
            return ase::OK;
        }
        let message = format!(
            "PigASIO 不支持动态切换采样率(请求 {sample_rate} Hz,当前 {current} Hz);\
             请修改 {config} 里的 sample_rate 后重启宿主",
            config = pigasio_core::config::CONFIG_FILE_NAME
        );
        log::warn!("{message}");
        obj.state.lock().last_error = message;
        ase::NO_CLOCK
    })
}

unsafe extern "system" fn vt_get_clock_sources(
    this: *mut core::ffi::c_void,
    clocks: *mut ASIOClockSource,
    num_sources: *mut i32,
) -> ASIOError {
    guard("getClockSources", ase::HW_MALFUNCTION, || {
        if clocks.is_null() || num_sources.is_null() || unsafe { *num_sources } < 1 {
            return E_INVALIDARG;
        }
        let _ = object(this);
        // 多设备场景下时间基准是「时钟主设备」的硬件时钟,宿主没法直接
        // 切换它 —— 想换就在配置里改 clock_master。
        let mut source = ASIOClockSource {
            index: 0,
            associated_channel: -1,
            associated_group: -1,
            is_current_source: ASIO_TRUE,
            ..Default::default()
        };
        let name = b"PigASIO Clock Master";
        source.name[..name.len()].copy_from_slice(name);
        unsafe {
            *clocks = source;
            *num_sources = 1;
        }
        ase::OK
    })
}

unsafe extern "system" fn vt_set_clock_source(
    _this: *mut core::ffi::c_void,
    reference: i32,
) -> ASIOError {
    guard("setClockSource", ase::HW_MALFUNCTION, || {
        if reference == 0 {
            ase::OK
        } else {
            log::warn!("setClockSource({reference}) 超出范围");
            ase::INVALID_PARAMETER
        }
    })
}

unsafe extern "system" fn vt_get_sample_position(
    this: *mut core::ffi::c_void,
    s_pos: *mut ASIOSamples,
    t_stamp: *mut ASIOTimeStamp,
) -> ASIOError {
    guard("getSamplePosition", ase::HW_MALFUNCTION, || {
        if s_pos.is_null() || t_stamp.is_null() {
            return E_INVALIDARG;
        }
        let obj = object(this);
        // 这个函数可能从宿主的 bufferSwitch 处理里被调用,而那时主锁
        // 正被音频回调持有。所以只读原子量,绝不加锁。
        let rate = obj.sample_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return ase::INVALID_MODE;
        }
        let position = obj.sample_position.load(Ordering::Relaxed);
        // 时间戳按 ASIO 约定是纳秒。用采样率换算而不是读系统时钟,
        // 这样即使系统时间被调整,两者也不会互相打架。
        let nanos = (position as f64 / rate as f64 * 1e9) as u64;
        unsafe {
            *s_pos = ASIOSamples::from_u64(position);
            *t_stamp = ASIOTimeStamp::from_u64(nanos);
        }
        ase::OK
    })
}

unsafe extern "system" fn vt_get_channel_info(
    this: *mut core::ffi::c_void,
    info: *mut ASIOChannelInfo,
) -> ASIOError {
    guard("getChannelInfo", ase::HW_MALFUNCTION, || {
        if info.is_null() {
            return E_INVALIDARG;
        }
        let obj = object(this);
        let state = obj.state.lock();
        let Some(engine) = state.engine.as_ref() else {
            return ase::INVALID_MODE;
        };

        let target = unsafe { &mut *info };
        let chan = target.channel;
        let is_input = target.is_input != 0;
        if chan < 0 {
            return ase::INVALID_PARAMETER;
        }
        let chan = chan as usize;

        let (limit, active) = if is_input {
            (engine.input_channel_count(), &state.active_inputs)
        } else {
            (engine.output_channel_count(), &state.active_outputs)
        };
        if chan >= limit {
            log::warn!("getChannelInfo 请求了不存在的通道 {chan}(上限 {limit})");
            return ase::INVALID_PARAMETER;
        }

        target.is_active = if active.get(chan).copied().unwrap_or(false) {
            ASIO_TRUE
        } else {
            ASIO_FALSE
        };
        target.channel_group = 0;
        target.r#type = engine.config().asio_sample_type.asio_code();

        // 名字在引擎构造时就全部算好并保证了同方向内不重名
        // (见 pigasio_core::channel_name),这里只是查表。
        let kind = if is_input {
            StreamKind::Input
        } else {
            StreamKind::Output
        };
        if let Some(name) = engine.channel_name(kind, chan) {
            target.set_name(name);
            log::debug!("getChannelInfo -> {name}(通道 {chan})");
        } else {
            // 走不到这里:上面的越界检查已经挡过了。
            log::warn!("getChannelInfo 取不到通道 {chan} 的名字");
        }
        ase::OK
    })
}

unsafe extern "system" fn vt_create_buffers(
    this: *mut core::ffi::c_void,
    buffer_infos: *mut ASIOBufferInfo,
    num_channels: i32,
    buffer_size: i32,
    callbacks: *mut ASIOCallbacks,
) -> ASIOError {
    guard("createBuffers", ase::HW_MALFUNCTION, || {
        if buffer_infos.is_null() || callbacks.is_null() || num_channels <= 0 || buffer_size <= 0 {
            return E_INVALIDARG;
        }
        let obj = object(this);
        let mut state = obj.state.lock();
        let position = obj.position_handle();
        let chunk = buffer_size as usize;

        enter("createBuffers", &mut state, |st| {
            if st.prepared {
                return Err(fail(ase::INVALID_MODE, "缓冲区已经创建过了"));
            }
            st.callbacks = unsafe { *callbacks };

            // 宿主可能在自己的缓冲处理里回头调用 getSamplePosition,
            // 所以采样位置由回调直接累加到原子量上,不走引擎的锁。
            let host_callbacks = st.callbacks;
            let switch_callback: BufferSwitchCallback = Box::new(move |_buffers: &mut pigasio_core::AsioBufferSet, index: usize| {
                if let Some(f) = host_callbacks.buffer_switch {
                    // SAFETY: 宿主在 createBuffers 时提供了这个指针,
                    // 并保证它在 disposeBuffers 之前一直有效。
                    unsafe { f(index as i32, ASIO_FALSE) };
                }
                position.fetch_add(chunk as u64, Ordering::Relaxed);
            });

            let engine = st
                .engine
                .as_mut()
                .ok_or_else(|| fail(ase::INVALID_MODE, "引擎未初始化"))?;

            engine
                .prepare(chunk, switch_callback)
                .map_err(|e| fail(map_core_error(&e), e.to_string()))?;

            // 把内部缓冲的地址交给宿主。这些指针在 disposeBuffers 之前
            // 一直有效 —— 引擎内部不会再重新分配它们。
            let mut active_inputs = vec![false; engine.input_channel_count()];
            let mut active_outputs = vec![false; engine.output_channel_count()];

            for i in 0..num_channels as isize {
                let info = unsafe { &mut *buffer_infos.offset(i) };
                let is_input = info.is_input != 0;
                let chan = info.channel_num;
                if chan < 0 {
                    return Err(fail(
                        ase::INVALID_PARAMETER,
                        format!("第 {i} 个缓冲的通道号为负数:{chan}"),
                    ));
                }
                let chan = chan as usize;
                let limit = if is_input {
                    active_inputs.len()
                } else {
                    active_outputs.len()
                };
                if chan >= limit {
                    return Err(fail(
                        ase::INVALID_PARAMETER,
                        format!(
                            "{}通道 {chan} 越界,当前只配置了 {limit} 个通道",
                            if is_input { "输入" } else { "输出" }
                        ),
                    ));
                }

                info.buffers[0] =
                    engine.buffer_ptr(is_input, chan, 0) as *mut core::ffi::c_void;
                info.buffers[1] =
                    engine.buffer_ptr(is_input, chan, 1) as *mut core::ffi::c_void;

                if is_input {
                    active_inputs[chan] = true;
                } else {
                    active_outputs[chan] = true;
                }
            }

            st.active_inputs = active_inputs;
            st.active_outputs = active_outputs;
            st.prepared = true;
            log::info!("已创建 {num_channels} 个通道的缓冲区,每块 {buffer_size} 帧");
            Ok(())
        })
    })
}

unsafe extern "system" fn vt_dispose_buffers(this: *mut core::ffi::c_void) -> ASIOError {
    guard("disposeBuffers", ase::HW_MALFUNCTION, || {
        let obj = object(this);
        obj.running.store(false, Ordering::Release);
        let mut state = obj.state.lock();

        enter("disposeBuffers", &mut state, |st| {
            if !st.prepared {
                return Err(fail(ase::INVALID_MODE, "缓冲区尚未创建"));
            }
            let engine = st
                .engine
                .as_mut()
                .ok_or_else(|| fail(ase::INVALID_MODE, "引擎未初始化"))?;
            engine
                .dispose()
                .map_err(|e| fail(map_core_error(&e), e.to_string()))?;

            st.prepared = false;
            st.active_inputs.iter_mut().for_each(|v| *v = false);
            st.active_outputs.iter_mut().for_each(|v| *v = false);
            st.callbacks = ASIOCallbacks::default();
            Ok(())
        })
    })
}

unsafe extern "system" fn vt_control_panel(this: *mut core::ffi::c_void) -> ASIOError {
    guard("controlPanel", ase::HW_MALFUNCTION, || {
        let config_path = {
            let obj = object(this);
            let state = obj.state.lock();
            state.config_path.clone()
        };
        log::info!("正在打开控制面板(配置文件 {config_path:?})");
        match crate::launch_control_panel(config_path.as_deref()) {
            Ok(()) => ase::OK,
            Err(e) => {
                log::error!("无法启动控制面板:{e}");
                object(this).state.lock().last_error = format!("无法启动控制面板:{e}");
                ase::HW_MALFUNCTION
            }
        }
    })
}

unsafe extern "system" fn vt_future(
    _this: *mut core::ffi::c_void,
    selector: i32,
    _opt: *mut core::ffi::c_void,
) -> ASIOError {
    guard("future", ase::NOT_PRESENT, || {
        // ASIO 用 future() 承载各种扩展。宿主看到 ASE_NotPresent 就知道
        // 该扩展不可用,这比返回错误码更符合协议。
        log::debug!("future(selector = {selector}) -> 不支持");
        ase::NOT_PRESENT
    })
}

unsafe extern "system" fn vt_output_ready(this: *mut core::ffi::c_void) -> ASIOError {
    guard("outputReady", ase::HW_MALFUNCTION, || {
        let obj = object(this);
        // 同 getSamplePosition:可能从宿主的 bufferSwitch 处理里被调用,
        // 所以只读原子标志。
        //
        // PigASIO 在 bufferSwitch 返回后立刻就把输出送向设备了,不需要
        // 宿主额外的「我准备好了」通知。这里只回报状态,告诉宿主这条路
        // 是通的 —— 部分宿主会据此少留一个缓冲区的延迟。
        if obj.running.load(Ordering::Acquire) {
            ase::OK
        } else {
            ase::INVALID_MODE
        }
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // 通道名的生成(含"避免重名")由 `pigasio_core::channel_name` 负责,
    // 测试也在那边 —— 那里能构造假的设备列表,不需要真实硬件。

    #[test]
    fn 写字符串会截断并补零() {
        let mut buf = [0xAAu8; 8];
        unsafe { write_c_string(buf.as_mut_ptr(), 8, "abcdefghij") };
        assert_eq!(&buf[..7], b"abcdefg");
        assert_eq!(buf[7], 0);
    }

    #[test]
    fn 写字符串不会切断多字节字符() {
        let mut buf = [0u8; 8];
        unsafe { write_c_string(buf.as_mut_ptr(), 8, "中文测试") };
        let len = buf.iter().position(|&b| b == 0).expect("缺少结尾的 0");
        assert!(len <= 7, "写入 {len} 字节,超过 capacity-1");

        // 关键断言:写入的字节必须能被系统代码页**完整解码**。
        // 如果从多字节字符中间截断,这里会解出非法序列。
        let decoded = decode_ansi(&buf[..len]).expect("截断产生了非法序列");
        assert!(
            "中文测试".starts_with(&decoded),
            "解码结果 “{decoded}” 不是原串的前缀"
        );
        // 而且至少放下了一个字 —— 否则测试本身没有意义。
        assert!(!decoded.is_empty());
    }

    /// 用系统 ANSI 代码页把字节解回字符串,仅供测试验证编码结果。
    ///
    /// 刻意不写死 GBK:这个测试要在任何语言的 Windows 上都能跑。
    fn decode_ansi(bytes: &[u8]) -> Option<String> {
        use windows_sys::Win32::Globalization::{
            MultiByteToWideChar, CP_ACP, MB_ERR_INVALID_CHARS,
        };

        if bytes.is_empty() {
            return Some(String::new());
        }
        let len = i32::try_from(bytes.len()).ok()?;
        // SAFETY: bytes 是合法切片;先问长度再分配。
        let needed = unsafe {
            MultiByteToWideChar(
                CP_ACP,
                MB_ERR_INVALID_CHARS,
                bytes.as_ptr(),
                len,
                core::ptr::null_mut(),
                0,
            )
        };
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        // SAFETY: wide 有 needed 个 u16 的容量。
        let written = unsafe {
            MultiByteToWideChar(
                CP_ACP,
                MB_ERR_INVALID_CHARS,
                bytes.as_ptr(),
                len,
                wide.as_mut_ptr(),
                needed,
            )
        };
        if written <= 0 {
            return None;
        }
        wide.truncate(written as usize);
        String::from_utf16(&wide).ok()
    }

    #[test]
    fn 通道名不会在字符中间截断() {
        let mut info = ASIOChannelInfo::default();
        // 塞满中文,确保一定触发截断。
        info.set_name(&"设".repeat(40));
        let len = info.name.iter().position(|&b| b == 0).unwrap();
        assert!(len <= 31);

        // 单个字符占多少字节由系统代码页决定(GBK 是 2,UTF-8 是 3)。
        // 无论哪种,总长度都必须是它的整数倍 —— 否则就是从字符中间切断了。
        let per_char = encode_for_asio("设", 64).len();
        assert!(per_char > 0);
        assert_eq!(
            len % per_char,
            0,
            "在字符中间截断了:{len} 字节,单字 {per_char} 字节"
        );
    }

    #[test]
    fn 通道名里的_ascii_在任何代码页下都原样保留() {
        let mut info = ASIOChannelInfo::default();
        let name = "OUT 1 (Realtek)";
        info.set_name(name);
        assert_eq!(&info.name[..name.len()], name.as_bytes());
        assert_eq!(info.name[name.len()], 0);
    }

    #[test]
    fn 中文按系统代码页编码而不是_utf8() {
        // 这是"机架里通道名是乱码"那个 bug 的回归测试。
        //
        // ASIO 的 char[] 被宿主按系统 ANSI 代码页解释,而 Rust 字符串是
        // UTF-8。如果不做转换,"中" 会写成 3 个字节 E4 B8 AD,宿主按
        // GBK 读出来是"涓"—— 典型的乱码。
        let encoded = crate::abi::encode_for_asio("中", 8);

        // 目标代码页表示不了这个字时会退化成 '?',那种情况下没什么可验证的。
        if encoded == b"?" {
            return;
        }

        assert_ne!(
            encoded.as_slice(),
            "中".as_bytes(),
            "写进去的仍是 UTF-8 —— 宿主按系统代码页解释,中文必然乱码"
        );
        // 必须能被同一个代码页完整解回来。
        assert_eq!(
            decode_ansi(&encoded).as_deref(),
            Some("中"),
            "编码结果无法用系统代码页解回,字节是 {encoded:?}"
        );
    }

    #[test]
    fn 核心错误映射到合理的_asio_错误码() {
        assert_eq!(
            map_core_error(&CoreError::Config("x".into())),
            ase::INVALID_PARAMETER
        );
        assert_eq!(
            map_core_error(&CoreError::DeviceNotFound {
                kind: StreamKind::Input,
                spec: "x".into(),
                available: vec![],
            }),
            ase::NOT_PRESENT
        );
        assert_eq!(map_core_error(&CoreError::AlreadyRunning), ase::INVALID_MODE);
    }
}
