//! WASAPI 的事件驱动流。
//!
//! 每个流一条线程,循环等 `IAudioClient` 的事件、搬一块数据。线程在
//! [`open_input`] / [`open_output`] 里就起,一直活到流被销毁 —— 这样
//! `play` / `pause` 只控制 `IAudioClient` 的启停,不必反复创建线程。
//!
//! 线程体写成**独立函数**(而不是直接塞进 `spawn` 的闭包):Rust 2021 的闭包
//! 按字段捕获,而 `Sendable<T>` 只有整体才是 `Send` —— 闭包里一旦出现
//! `client.0` 这样的字段访问,捕获的就成了里面那个不是 `Send` 的 COM 接口。
//! 把 `RenderContext` / `CaptureContext` 整个按值传进函数,捕获才落在正确的
//! 类型上。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use crate::backend::{ErrorCallback, InputCallback, OutputCallback, StreamFormat, StreamHandle};
use crate::error::{Error, Result};

use super::ensure_com;

/// 等事件时的超时(毫秒)。
///
/// 用超时而不是无限等,是为了让线程有机会看到 `shutdown` 标志。200 ms 足够
/// 短,不至于让销毁流时卡顿。
const WAIT_TIMEOUT_MS: u32 = 200;

/// 一个内核事件句柄。
///
/// `HANDLE` 是裸指针,本身不是 `Send`;但事件是内核对象,跨线程使用是安全的
/// (它只是个不透明的标量),所以手动声明。
#[derive(Clone, Copy)]
struct OwnedEvent(HANDLE);

unsafe impl Send for OwnedEvent {}

/// 把一个 WASAPI 接口标成可以送进线程。
///
/// 这些 COM 接口(WASAPI 的音频客户端、渲染/采集客户端)都是 agile 的:可以在
/// MTA 下的任意线程使用。但 windows crate 一律不给它们实现 `Send`,而
/// `unsafe impl Send for IAudioClient` 又做不到(那是外部类型),所以包一层
/// newtype。
struct Sendable<T>(T);

// SAFETY: 只用于本模块里那几个 agile 的 WASAPI 接口,而且每个接口同一时刻
// 只有一条线程在碰它 —— 音频客户端留在创建/启停的那条线程,渲染/采集客户端
// 只被回调线程使用。
unsafe impl<T> Send for Sendable<T> {}

/// WASAPI 流。
struct WasapiStream {
    client: IAudioClient,
    event: OwnedEvent,
    /// `IAudioClient` 是否已经 Start。回调线程据此跳过不该处理的块。
    running: Arc<AtomicBool>,
    /// 线程该退出了。
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StreamHandle for WasapiStream {
    fn play(&self) -> Result<()> {
        unsafe { self.client.Start() }
            .map_err(|e| Error::Backend(format!("启动音频流失败:{e}")))?;
        self.running.store(true, Ordering::Release);
        Ok(())
    }

    fn pause(&self) -> Result<()> {
        // 先让回调停下来再停 client:反过来的话,最后一块可能已经被取走
        // 却没被处理。
        self.running.store(false, Ordering::Release);
        unsafe { self.client.Stop() }
            .map_err(|e| Error::Backend(format!("暂停音频流失败:{e}")))?;
        Ok(())
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        // 线程可能正阻塞在 WaitForSingleObject 上,叫醒它。
        self.shutdown.store(true, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.event.0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // 关事件必须在 join 之后 —— 线程还在用它。
        unsafe {
            let _ = CloseHandle(self.event.0);
        }
    }
}

/// 初始化好的音频客户端。
struct ClientSetup {
    client: IAudioClient,
    event: OwnedEvent,
    /// 设备缓冲的帧数(整块)。
    buffer_frames: u32,
}

/// 激活并初始化一个共享模式的音频客户端。
///
/// 格式直接用系统的混音格式 —— 共享模式只能用它(见模块文档)。
/// `period_frames` 是 0 就按系统默认周期,否则尽量按它缩短。
fn init_client(
    device: &IMMDevice,
    device_name: &str,
    period_frames: usize,
) -> Result<ClientSetup> {
    ensure_com()?;

    let client: IAudioClient =
        unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| Error::DeviceOpen {
            name: device_name.to_string(),
            reason: format!("激活音频客户端失败:{e}"),
        })?;

    let mix = unsafe { client.GetMixFormat() }.map_err(|e| Error::DeviceOpen {
        name: device_name.to_string(),
        reason: format!("读取混音格式失败:{e}"),
    })?;

    let initialized = initialize_stream(&client, mix, period_frames);
    unsafe { CoTaskMemFree(Some(mix as *const _)) };
    initialized.map_err(|e| Error::DeviceOpen {
        name: device_name.to_string(),
        reason: format!("初始化音频客户端失败:{e}"),
    })?;

    let buffer_frames = unsafe { client.GetBufferSize() }.map_err(|e| Error::DeviceOpen {
        name: device_name.to_string(),
        reason: format!("读取设备缓冲大小失败:{e}"),
    })?;

    // 手动重置的事件:系统每次有数据就 SetEvent,我们等它。
    let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| Error::DeviceOpen {
        name: device_name.to_string(),
        reason: format!("创建事件失败:{e}"),
    })?;
    let event = OwnedEvent(event);
    unsafe { client.SetEventHandle(event.0) }.map_err(|e| Error::DeviceOpen {
        name: device_name.to_string(),
        reason: format!("绑定事件失败:{e}"),
    })?;

    Ok(ClientSetup {
        client,
        event,
        buffer_frames,
    })
}

/// 初始化共享模式的流。
///
/// 优先走 `IAudioClient3::InitializeSharedAudioStream` —— 这是唯一能把共享模式
/// 的 period 压下来的路子(见 `docs/low-latency-wasapi.md`)。拿不到这个接口、
/// 或者指定周期被拒绝,就退回普通的 `Initialize`,让系统按默认周期来。
///
/// 两条路径都用事件驱动(`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`)。
fn initialize_stream(
    client: &IAudioClient,
    format: *const windows::Win32::Media::Audio::WAVEFORMATEX,
    period_frames: usize,
) -> windows::core::Result<()> {
    const FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

    if period_frames > 0 {
        if let Ok(modern) = client.cast::<IAudioClient3>() {
            // SAFETY: 调用方保证 `format` 在这次调用期间有效。
            let result = unsafe {
                modern.InitializeSharedAudioStream(FLAGS, period_frames as u32, format, None)
            };
            match result {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!(
                    "按 {period_frames} 帧初始化共享模式失败({e}),退回系统默认 period"
                ),
            }
        }
    }

    // 共享模式下缓冲时长由 audio engine 决定,传 0 表示"用默认"。
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, FLAGS, 0, 0, format, None) }
}

/// 渲染线程要的一切。整体按值交给线程体,理由见模块文档。
struct RenderContext {
    client: Sendable<IAudioClient>,
    render: Sendable<IAudioRenderClient>,
    event: OwnedEvent,
    running: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    on_data: OutputCallback,
    on_error: ErrorCallback,
    channels: usize,
    buffer_frames: u32,
}

/// 采集线程要的一切。
struct CaptureContext {
    client: Sendable<IAudioClient>,
    capture: Sendable<IAudioCaptureClient>,
    event: OwnedEvent,
    running: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    on_data: InputCallback,
    on_error: ErrorCallback,
    channels: usize,
}

/// 输出流:从引擎要数据,写进设备的渲染缓冲。
pub(super) fn open_output(
    device: &IMMDevice,
    device_name: &str,
    format: &StreamFormat,
    on_data: OutputCallback,
    on_error: ErrorCallback,
) -> Result<Box<dyn StreamHandle>> {
    let setup = init_client(device, device_name, format.period_frames)?;
    let render: IAudioRenderClient = unsafe { setup.client.GetService() }
        .map_err(|e| Error::Backend(format!("取渲染客户端失败:{e}")))?;

    let running = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));

    let context = RenderContext {
        client: Sendable(setup.client.clone()),
        render: Sendable(render),
        event: setup.event,
        running: Arc::clone(&running),
        shutdown: Arc::clone(&shutdown),
        on_data,
        on_error,
        channels: format.channels.max(1),
        buffer_frames: setup.buffer_frames,
    };

    let thread = std::thread::Builder::new()
        .name("pigasio-wasapi-render".into())
        .spawn(move || render_loop(context))
        .map_err(|e| Error::Backend(format!("创建渲染线程失败:{e}")))?;

    Ok(Box::new(WasapiStream {
        client: setup.client,
        event: setup.event,
        running,
        shutdown,
        thread: Some(thread),
    }))
}

/// 输入流:从设备的采集缓冲取数据,交给引擎。
pub(super) fn open_input(
    device: &IMMDevice,
    device_name: &str,
    format: &StreamFormat,
    on_data: InputCallback,
    on_error: ErrorCallback,
) -> Result<Box<dyn StreamHandle>> {
    let setup = init_client(device, device_name, format.period_frames)?;
    let capture: IAudioCaptureClient = unsafe { setup.client.GetService() }
        .map_err(|e| Error::Backend(format!("取采集客户端失败:{e}")))?;

    let running = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));

    let context = CaptureContext {
        client: Sendable(setup.client.clone()),
        capture: Sendable(capture),
        event: setup.event,
        running: Arc::clone(&running),
        shutdown: Arc::clone(&shutdown),
        on_data,
        on_error,
        channels: format.channels.max(1),
    };

    let thread = std::thread::Builder::new()
        .name("pigasio-wasapi-capture".into())
        .spawn(move || capture_loop(context))
        .map_err(|e| Error::Backend(format!("创建采集线程失败:{e}")))?;

    Ok(Box::new(WasapiStream {
        client: setup.client,
        event: setup.event,
        running,
        shutdown,
        thread: Some(thread),
    }))
}

/// 渲染线程体。
fn render_loop(context: RenderContext) {
    let RenderContext {
        client,
        render,
        event,
        running,
        shutdown,
        mut on_data,
        mut on_error,
        channels,
        buffer_frames,
    } = context;
    let client = &client.0;
    let render = &render.0;
    let mut reported = false;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let waited = unsafe { WaitForSingleObject(event.0, WAIT_TIMEOUT_MS) };
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        if waited != WAIT_OBJECT_0 || !running.load(Ordering::Acquire) {
            continue;
        }

        // 设备缓冲里已经排了多少帧 —— 只补空出来的部分。
        let padding = match unsafe { client.GetCurrentPadding() } {
            Ok(padding) => padding,
            Err(e) => {
                report_once(&mut reported, &mut on_error, || {
                    format!("读取缓冲占用失败:{e}")
                });
                continue;
            }
        };
        let available = buffer_frames.saturating_sub(padding);
        if available == 0 {
            continue;
        }

        let data = match unsafe { render.GetBuffer(available) } {
            Ok(data) => data,
            Err(e) => {
                report_once(&mut reported, &mut on_error, || {
                    format!("取渲染缓冲失败:{e}")
                });
                continue;
            }
        };
        if !data.is_null() {
            // 共享模式的混音格式是 f32,`negotiate` 已经确认过了。
            let samples = unsafe {
                std::slice::from_raw_parts_mut(data as *mut f32, available as usize * channels)
            };
            on_data(samples);
        }
        if let Err(e) = unsafe { render.ReleaseBuffer(available, 0) } {
            report_once(&mut reported, &mut on_error, || {
                format!("提交渲染缓冲失败:{e}")
            });
        }
    }
}

/// 采集线程体。
fn capture_loop(context: CaptureContext) {
    let CaptureContext {
        client,
        capture,
        event,
        running,
        shutdown,
        mut on_data,
        mut on_error,
        channels,
    } = context;
    let _ = &client; // 采集侧只需要 capture 客户端
    let capture = &capture.0;
    let mut reported = false;
    // 设备报"静音"时用的填充缓冲。
    let mut silence: Vec<f32> = Vec::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let waited = unsafe { WaitForSingleObject(event.0, WAIT_TIMEOUT_MS) };
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        if waited != WAIT_OBJECT_0 || !running.load(Ordering::Acquire) {
            continue;
        }

        // 一次通知可能攒了好几包,全取出来。
        loop {
            // `Ok(0)` 表示取干净了。
            let mut packet = match unsafe { capture.GetNextPacketSize() } {
                Ok(0) => break,
                Ok(packet) => packet,
                Err(e) => {
                    report_once(&mut reported, &mut on_error, || {
                        format!("读取采集包大小失败:{e}")
                    });
                    break;
                }
            };

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut flags = 0u32;
            if let Err(e) =
                unsafe { capture.GetBuffer(&mut data, &mut packet, &mut flags, None, None) }
            {
                report_once(&mut reported, &mut on_error, || {
                    format!("取采集缓冲失败:{e}")
                });
                break;
            }
            let count = packet as usize * channels;

            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                // 标了静音时缓冲内容未定义,必须自己填零 —— 直接读会把上一轮
                // 的残留当成音频送出去。
                silence.clear();
                silence.resize(count, 0.0);
                on_data(&silence);
            } else if !data.is_null() {
                let samples = unsafe { std::slice::from_raw_parts(data as *const f32, count) };
                on_data(samples);
            }

            if let Err(e) = unsafe { capture.ReleaseBuffer(packet) } {
                report_once(&mut reported, &mut on_error, || {
                    format!("释放采集缓冲失败:{e}")
                });
                break;
            }
        }
    }
}

/// 报一次错误,之后静默。
///
/// 这个回调跑在音频线程上,而引擎收到错误后要写日志 —— 落盘是阻塞的。
/// 一次故障可能连着触发几千次,不能让它把日志刷爆、把实时线程拖垮。
fn report_once(reported: &mut bool, on_error: &mut ErrorCallback, message: impl FnOnce() -> String) {
    if !*reported {
        *reported = true;
        on_error(message());
    }
}
