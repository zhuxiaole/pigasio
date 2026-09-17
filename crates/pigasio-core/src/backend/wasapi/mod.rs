//! 直接调用 WASAPI 的后端。
//!
//! # 为什么另写一个后端
//!
//! cpal 0.15 只用旧接口 `IAudioClient::Initialize`,共享模式的 period 由
//! audio engine 决定(通常 10 ms)。要把 period 降到设备允许的最小值,得用
//! `IAudioClient3::InitializeSharedAudioStream` —— 那个能力 cpal 没暴露,它的
//! `Device` 也不给 `IMMDevice`,所以只能绕开它。
//!
//! 当前阶段(见 `docs/low-latency-wasapi.md`)**行为对齐 cpal**:共享模式 +
//! 默认 period + 事件驱动的流循环。接入 `IAudioClient3` 是阶段 3。
//!
//! # 共享模式下的"格式协商"
//!
//! 共享模式不能随便挑格式:必须用系统混音格式(`GetMixFormat`),否则只会
//! 拿到 `S_FALSE` 和一个"最接近的替代格式"。所以这里的协商就是读混音格式。
//! 采样率由播放设备决定,和 ASIO 配置不一致时由引擎的重采样器兜住 ——
//! 这一点与 cpal 后端的行为一致(见 `cpal_backend::CpalDevice::negotiate`)。
//!
//! # 线程与 COM
//!
//! 用到 COM 的每个线程都要 `CoInitializeEx`。设备枚举发生在调用方线程
//! (CLI / 控制面板 / 引擎初始化),流的事件循环跑在 `open_*` 里起的那条
//! 线程上,两边都按需初始化。

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioClient, IAudioClient3, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::error::{Error, Result, StreamKind};

use super::{
    Backend, DeviceHandle, DeviceInfo, DeviceSampleFormat, ErrorCallback, InputCallback,
    OutputCallback, StreamFormat, StreamHandle, StreamRequest,
};

mod stream;

/// 基于 WASAPI 的后端。
pub struct WasapiBackend;

impl WasapiBackend {
    pub fn new() -> Self {
        WasapiBackend
    }

    /// 这台机器上能不能用(COM 起得来、且至少有一个输出端点)。
    ///
    /// 自动选择后端时用它试一下:失败就退回 cpal,而不是让驱动整个加载不上。
    pub fn available(&self) -> bool {
        self.enumerate(StreamKind::Output)
            .map(|devices| !devices.is_empty())
            .unwrap_or(false)
    }
}

impl Default for WasapiBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for WasapiBackend {
    fn name(&self) -> &'static str {
        "wasapi"
    }

    fn enumerate(&self, kind: StreamKind) -> Result<Vec<DeviceInfo>> {
        ensure_com()?;
        let enumerator = new_enumerator()?;
        let collection = unsafe { enumerator.EnumAudioEndpoints(data_flow(kind), DEVICE_STATE_ACTIVE) }
            .map_err(|e| Error::Backend(format!("枚举{}设备失败:{e}", kind.as_str())))?;
        let count = unsafe { collection.GetCount() }
            .map_err(|e| Error::Backend(format!("读取设备数量失败:{e}")))?;

        let mut out = Vec::new();
        for index in 0..count {
            let device = match unsafe { collection.Item(index) } {
                Ok(device) => device,
                Err(e) => {
                    log::debug!("跳过第 {index} 个{}端点:{e}", kind.as_str());
                    continue;
                }
            };
            let name = match friendly_name(&device) {
                Ok(name) => name,
                Err(e) => {
                    log::warn!("跳过读不出名字的{}设备:{e}", kind.as_str());
                    continue;
                }
            };
            // 读不出混音格式,说明这个端点在当前方向用不了。
            let (max_channels, default_sample_rate) = match mix_format(&device) {
                Ok(info) => info,
                Err(e) => {
                    log::debug!("设备 “{name}” 无法作为{}打开:{e}", kind.as_str());
                    continue;
                }
            };
            out.push(DeviceInfo {
                name: name.clone(),
                max_channels,
                default_sample_rate,
                handle: Arc::new(WasapiDevice { name, device }),
            });
        }
        Ok(out)
    }

    fn default_device(&self, kind: StreamKind) -> Result<DeviceInfo> {
        ensure_com()?;
        let enumerator = new_enumerator()?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(data_flow(kind), eConsole) }
            .map_err(|e| Error::Backend(format!("取默认{}设备失败:{e}", kind.as_str())))?;
        let name = friendly_name(&device)?;
        let (max_channels, default_sample_rate) = mix_format(&device)?;
        Ok(DeviceInfo {
            name: name.clone(),
            max_channels,
            default_sample_rate,
            handle: Arc::new(WasapiDevice { name, device }),
        })
    }
}

/// WASAPI 的设备句柄。
struct WasapiDevice {
    name: String,
    /// `IMMDevice` 是引用计数的 COM 接口,克隆代价低。
    device: IMMDevice,
}

// `IMMDevice` 在 windows crate 里没被标成 `Send + Sync`(它内部是裸指针),
// 但它是 **agile** 的 COM 对象:无论所在线程是 MTA 还是 STA,都可以自由地
// 跨线程使用它,不需要 marshal。后端的 `DeviceHandle` 要求 `Send + Sync`,
// 因为 `DeviceInfo` 会被克隆、跨线程传递(引擎在宿主线程解析设备,在专用
// 线程打开流)。
unsafe impl Send for WasapiDevice {}
unsafe impl Sync for WasapiDevice {}

impl WasapiDevice {
    fn activate_client(&self) -> Result<IAudioClient> {
        ensure_com()?;
        unsafe { self.device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| Error::DeviceOpen {
                name: self.name.clone(),
                reason: format!("激活音频客户端失败:{e}"),
            })
    }
}

impl DeviceHandle for WasapiDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn negotiate(&self, _kind: StreamKind, request: &StreamRequest) -> Result<StreamFormat> {
        let client = self.activate_client()?;
        let format = unsafe { client.GetMixFormat() }.map_err(|e| Error::DeviceOpen {
            name: self.name.clone(),
            reason: format!("读取混音格式失败:{e}"),
        })?;
        // 解析和 period 查询都在释放之前做完 —— 两者都要用那块内存。
        let parsed = unsafe { read_mix_format(format) };
        let period_frames = unsafe { choose_period(&client, format, request.period_frames) };
        unsafe { CoTaskMemFree(Some(format as *const _)) };
        let (channels, sample_rate, sample_format) = parsed.map_err(|e| Error::DeviceOpen {
            name: self.name.clone(),
            reason: e.to_string(),
        })?;

        if sample_rate != request.sample_rate {
            // 共享模式的采样率由播放设备定,改不了 —— 交给重采样器。
            log::warn!(
                "设备 “{}” 的共享模式采样率是 {sample_rate} Hz(配置为 {} Hz),由重采样兜住",
                self.name,
                request.sample_rate
            );
        }
        Ok(StreamFormat {
            sample_rate,
            channels,
            sample_format,
            period_frames,
        })
    }

    fn open_input(
        &self,
        format: &StreamFormat,
        on_data: InputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>> {
        stream::open_input(&self.device, &self.name, format, on_data, on_error)
    }

    fn open_output(
        &self,
        format: &StreamFormat,
        on_data: OutputCallback,
        on_error: ErrorCallback,
    ) -> Result<Box<dyn StreamHandle>> {
        stream::open_output(&self.device, &self.name, format, on_data, on_error)
    }
}

// ---------------------------------------------------------------------------
// COM 辅助
// ---------------------------------------------------------------------------

/// 确保**当前线程**初始化过 COM。
///
/// WASAPI 全是 COM,每个用到它的线程都得先 `CoInitializeEx`。这里用
/// thread-local 记住结果,避免重复调用 —— 重复调用虽然只会返回 `S_FALSE`,
/// 但每块缓冲都问一次是没必要的。
///
/// **注意不能调用 `CoUninitialize`**:设备和流句柄可能活到线程之后,提前
/// 反初始化会让那些接口变成悬空的。让 COM 跟着线程一起结束是安全的。
pub(super) fn ensure_com() -> Result<()> {
    thread_local! {
        static COM: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }
    COM.with(|state| {
        if let Some(ok) = state.get() {
            return if ok {
                Ok(())
            } else {
                Err(Error::Backend("COM 初始化失败".into()))
            };
        }

        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // `RPC_E_CHANGED_MODE` **不是失败**:它的意思是"这个线程已经用别的
        // apartment 模式初始化过 COM 了"。COM 本身可用,只是模式不同 ——
        // 而 WASAPI 的接口都是 agile(free-threaded)的,在 STA 上照样能用。
        //
        // 控制面板的 UI 线程必然走到这条路:winit 创建窗口时会用
        // `OleInitialize` 把线程设成 STA。把它当失败处理的话,控制面板里
        // 永远枚举不出设备。
        let ok = result.is_ok() || result == RPC_E_CHANGED_MODE;
        state.set(Some(ok));
        if ok {
            Ok(())
        } else {
            Err(Error::Backend(format!(
                "COM 初始化失败(0x{:08X})",
                result.0 as u32
            )))
        }
    })
}

fn new_enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| Error::Backend(format!("创建设备枚举器失败:{e}")))
}

fn data_flow(kind: StreamKind) -> windows::Win32::Media::Audio::EDataFlow {
    match kind {
        StreamKind::Input => eCapture,
        StreamKind::Output => eRender,
    }
}

/// 读设备名(`PKEY_Device_FriendlyName`)。
fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device
            .OpenPropertyStore(STGM_READ)
            .map_err(|e| Error::Backend(format!("打开设备属性失败:{e}")))?;
        let value = store
            .GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const _)
            .map_err(|e| Error::Backend(format!("读取设备名失败:{e}")))?;
        // PROPVARIANT 是个联合体,这里要的是里面的 LPWSTR。
        let inner = &value.as_raw().Anonymous.Anonymous;
        if inner.vt != VT_LPWSTR.0 {
            return Err(Error::Backend("设备名不是字符串".into()));
        }
        let ptr = *(&inner.Anonymous as *const _ as *const *const u16);
        Ok(utf16_ptr_to_string(ptr))
    }
}

/// 从以 0 结尾的 UTF-16 指针读出字符串。
///
/// # Safety
/// `ptr` 必须指向一段以 0 结尾的、长度不超过实际分配的 UTF-16 数据。
unsafe fn utf16_ptr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    OsString::from_wide(std::slice::from_raw_parts(ptr, len))
        .to_string_lossy()
        .into_owned()
}

/// 读混音格式的通道数与采样率(设备枚举用)。
fn mix_format(device: &IMMDevice) -> Result<(usize, u32)> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| Error::Backend(format!("激活音频客户端失败:{e}")))?;
    let format = unsafe { client.GetMixFormat() }
        .map_err(|e| Error::Backend(format!("读取混音格式失败:{e}")))?;
    // `WAVEFORMATEX` 在 windows crate 里是 packed 的,直接按引用读字段会被
    // 判为未对齐访问 —— 复制成局部值再读。
    let base = unsafe { std::ptr::read_unaligned(format) };
    let info = (base.nChannels as usize, base.nSamplesPerSec);
    unsafe { CoTaskMemFree(Some(format as *const _)) };
    Ok(info)
}

/// 解析混音格式:通道数、采样率、样本类型。
///
/// # Safety
/// `format` 必须来自 `IAudioClient::GetMixFormat`,且仍在有效期内。
unsafe fn read_mix_format(
    format: *const windows::Win32::Media::Audio::WAVEFORMATEX,
) -> Result<(usize, u32, DeviceSampleFormat)> {
    // packed 结构:要用的字段先**复制**到局部变量。直接拿字段去比较或格式化
    // 会创建对未对齐字段的引用,Rust 判为 UB(E0793)。
    let base: windows::Win32::Media::Audio::WAVEFORMATEX = std::ptr::read_unaligned(format);
    let channels = base.nChannels as usize;
    let sample_rate = base.nSamplesPerSec;
    let format_tag = base.wFormatTag as u32;
    let bits_per_sample = base.wBitsPerSample;

    // 共享模式的混音格式在 Windows 上几乎总是 32 位 float。不是的话我们没法
    // 直接当 f32 用 —— 与其悄悄错位,不如明确报出来。
    let is_float = match format_tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            let ext: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE =
                std::ptr::read_unaligned(format as *const _);
            let sub_format = ext.SubFormat;
            sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    };
    if !is_float || bits_per_sample != 32 {
        return Err(Error::Backend(format!(
            "共享模式混音格式不是 32 位 float(格式标签 {format_tag}、每样本 {bits_per_sample} 位);\
             PigASIO 的 WASAPI 后端暂时只支持 float32"
        )));
    }

    Ok((channels, sample_rate, DeviceSampleFormat::F32))
}

/// 挑一个共享模式的 period(帧)。
///
/// 拿不到 `IAudioClient3`(Windows 10 之前,或者驱动不支持)就返回 0 —— 那时
/// period 由 audio engine 决定,我们插不上手,只能照旧走默认的 `Initialize`。
///
/// # Safety
/// `format` 必须来自 `IAudioClient::GetMixFormat`,且仍在有效期内。
unsafe fn choose_period(
    client: &IAudioClient,
    format: *const windows::Win32::Media::Audio::WAVEFORMATEX,
    requested: Option<usize>,
) -> usize {
    let Ok(modern) = client.cast::<IAudioClient3>() else {
        log::debug!("这台设备不支持 IAudioClient3,共享模式 period 由系统决定");
        return 0;
    };

    let mut default_period = 0u32;
    let mut fundamental = 0u32;
    let mut min_period = 0u32;
    let mut max_period = 0u32;
    if modern
        .GetSharedModeEnginePeriod(
            format,
            &mut default_period,
            &mut fundamental,
            &mut min_period,
            &mut max_period,
        )
        .is_err()
    {
        log::debug!("查不到共享模式的 period 范围,按默认处理");
        return 0;
    }
    if fundamental == 0 || min_period == 0 {
        return 0;
    }

    let target = match requested {
        // 用户点名了周期:`InitializeSharedAudioStream` 要求它是基本单位的
        // 整数倍,不对齐就直接返回 E_INVALIDARG,所以向上取整;再夹进设备
        // 报出的范围。
        Some(frames) => align_up(frames, fundamental as usize)
            .clamp(min_period as usize, max_period.max(min_period) as usize),
        // 没点名就**保持设备默认的 period**。
        //
        // 这里一度取的是最小值 —— "默认就把延迟压到最低"听起来很合理,但实测
        // Virtual Audio Cable 在它的最小 period(48 帧)下产出速率比标称高约
        // 3%(事件频率 1034/秒而非 1000),远超漂移补偿的能力(默认 500 ppm
        // = 0.05%),环形缓冲会一路积压到溢出。降周期是要承担风险的,应该由
        // 用户主动要,不该是默认行为。
        None => default_period as usize,
    };

    // 目标就是设备默认值时,显式设置和用默认没有区别 —— 那就别折腾,退回普通
    // 的 `Initialize`。少一次可能被驱动实现歪掉的调用。
    if requested.is_none() && target <= default_period as usize {
        log::debug!(
            "共享模式默认 period 是 {default_period} 帧(可选 {min_period}..{max_period}),\
             保持默认"
        );
        return 0;
    }

    log::info!(
        "共享模式 period:可选 {min_period}..{max_period} 帧(默认 {default_period}、\
         基本单位 {fundamental}),选用 {target} 帧"
    );
    target
}

/// 把 `value` 向上对齐到 `unit` 的整数倍。
fn align_up(value: usize, unit: usize) -> usize {
    if unit == 0 {
        return value;
    }
    value.div_ceil(unit) * unit
}

#[cfg(test)]
mod tests {
    use super::align_up;

    #[test]
    fn 向上对齐到基本单位的整数倍() {
        // 128 是 Windows 上常见的 fundamental period(48 kHz 下约 2.67 ms)。
        assert_eq!(align_up(128, 128), 128);
        assert_eq!(align_up(129, 128), 256);
        assert_eq!(align_up(1, 128), 128);
        assert_eq!(align_up(0, 128), 0);
        // 基本单位为 0 时不该除零,原样返回。
        assert_eq!(align_up(300, 0), 300);
    }

    #[test]
    fn 线程已经是_sta_时_com_依然可用() {
        // 控制面板的 UI 线程就是这个状态:winit 创建窗口时用 `OleInitialize`
        // 把它设成了 STA,我们再请求 MTA 会拿到 `RPC_E_CHANGED_MODE`。
        //
        // 曾经把这个返回值当失败,后果是**控制面板里永远枚举不出设备**,而
        // 命令行工具一切正常(它的主线程没被别人初始化过)—— 这种"只在 GUI
        // 里坏"的 bug 光看 CLI 是发现不了的。
        let init = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        // 测试线程可能已经是别的模式了(线程池复用),那不碍事 —— 要验证的是
        // `ensure_com()` 不该因为模式不匹配而失败。
        let _ = init;

        assert!(
            super::ensure_com().is_ok(),
            "线程已经在 STA 上初始化过 COM 时,ensure_com 不该报失败"
        );
    }
}
