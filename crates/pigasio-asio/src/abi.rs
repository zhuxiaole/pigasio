//! ASIO 的二进制接口定义。
//!
//! 这些类型是 Steinberg ASIO SDK 2.3 里 `common/asiosys.h`、`common/asio.h`
//! 和 `common/iasiodrv.h` 的 Rust 翻译。之所以自己写一遍而不是去包含 C
//! 头文件,是因为 Rust 侧只需要**确定的内存布局**,不需要 SDK 的任何实现,
//! 这样项目本身不必再分发 SDK 的源码。
//!
//! # 几个容易踩的坑
//!
//! * **`long` 在 Windows 上是 32 位**,即使是 64 位进程也一样(LLP64 模型)。
//!   所以 `ASIOBool`、`ASIOError`、`ASIOSampleType`、`ASIOChannelInfo` 里
//!   的那些字段全是 `i32`。
//! * **`ASIOSamples` / `ASIOTimeStamp` 是结构体而不是 64 位整数**。
//!   `asiosys.h` 在 Windows 分支把 `NATIVE_INT64` 定义为 0,于是这两个
//!   类型走的是 `{ unsigned long hi; unsigned long lo; }` 那条分支。
//! * **`ASIOChannelInfo` 的字段顺序是 2.3 才变成现在这样的**:
//!   `isActive` 紧跟在 `isInput` 后面,`channelGroup` 在它之后。老资料里
//!   常见的 `channel, isInput, channelGroup, type, isActive, name` 顺序是
//!   ASIO 2.2 的,照那个写会写坏宿主的栈。
//! * **调用约定**:ASIO 接口在 32 位下用的是 MSVC 的 `__thiscall` + 导出
//!   函数的 `__stdcall` 混合体。Rust 稳定版无法表达 `__thiscall`,所以
//!   PigASIO 只构建 `x86_64-pc-windows-msvc`。64 位下所有调用约定统一,
//!   `extern "system"` 就是正确答案。

#![allow(non_camel_case_types)]

use core::ffi::c_void;

// ---------------------------------------------------------------------------
// 基础类型
// ---------------------------------------------------------------------------

/// 64 位采样计数。Windows 上 `NATIVE_INT64 == 0`,所以是高低位两个 u32。
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ASIOSamples {
    pub hi: u32,
    pub lo: u32,
}

impl ASIOSamples {
    pub fn from_u64(v: u64) -> Self {
        ASIOSamples {
            hi: (v >> 32) as u32,
            lo: (v & 0xffff_ffff) as u32,
        }
    }

    pub fn to_u64(self) -> u64 {
        ((self.hi as u64) << 32) | self.lo as u64
    }
}

/// 64 位时间戳,单位纳秒。结构与 [`ASIOSamples`] 相同。
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ASIOTimeStamp {
    pub hi: u32,
    pub lo: u32,
}

impl ASIOTimeStamp {
    pub fn from_u64(v: u64) -> Self {
        ASIOTimeStamp {
            hi: (v >> 32) as u32,
            lo: (v & 0xffff_ffff) as u32,
        }
    }
}

/// 采样率,IEEE 754 双精度。
pub type ASIOSampleRate = f64;

/// 布尔值。ASIO 用 `long` 表示,取值 [`ASIO_FALSE`] / [`ASIO_TRUE`]。
pub type ASIOBool = i32;

pub const ASIO_FALSE: ASIOBool = 0;
pub const ASIO_TRUE: ASIOBool = 1;

/// 错误码,见 [`ase`]。
pub type ASIOError = i32;

/// 采样格式编码,见 [`sample_type`]。
pub type ASIOSampleType = i32;

/// ASIO 的错误码(`asio.h` 的 `Error codes` 段)。
pub mod ase {
    use super::ASIOError;

    pub const OK: ASIOError = 0;
    /// `ASIOFuture()` 专用的成功返回值。
    pub const SUCCESS: ASIOError = 0x3f48_47a0;
    /// 硬件输入或输出不存在/不可用。
    pub const NOT_PRESENT: ASIOError = -1000;
    /// 硬件故障。
    pub const HW_MALFUNCTION: ASIOError = -999;
    /// 参数无效。
    pub const INVALID_PARAMETER: ASIOError = -998;
    /// 硬件状态不对,或调用时机不对。
    pub const INVALID_MODE: ASIOError = -997;
    /// 查询采样位置时硬件没有在推进。
    pub const SP_NOT_ADVANCING: ASIOError = -996;
    /// 采样时钟或采样率无法确定。
    pub const NO_CLOCK: ASIOError = -995;
    /// 内存不足。
    pub const NO_MEMORY: ASIOError = -994;
}

/// 采样格式编码(`asio.h` 的 `Sample Types` 段)。
///
/// 只列出小端序(Little Endian)的那一组 —— 那是 x86/ARM 上实际会用到的,
/// 也是 FlexASIO 与 PigASIO 实际暴露的。
pub mod sample_type {
    use super::ASIOSampleType;

    pub const INT16_LSB: ASIOSampleType = 16;
    pub const INT24_LSB: ASIOSampleType = 17;
    pub const INT32_LSB: ASIOSampleType = 18;
    pub const FLOAT32_LSB: ASIOSampleType = 19;
    pub const FLOAT64_LSB: ASIOSampleType = 20;

    /// 该格式每个采样占多少字节。未知格式返回 `None`。
    pub fn size_of(t: ASIOSampleType) -> Option<usize> {
        match t {
            INT16_LSB => Some(2),
            INT24_LSB => Some(3),
            INT32_LSB | FLOAT32_LSB => Some(4),
            FLOAT64_LSB => Some(8),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 面向宿主的结构体
// ---------------------------------------------------------------------------

/// `ASIOBufferInfo` —— `ASIOCreateBuffers()` 的输入/输出。
///
/// * 输入时:`is_input` 指明方向,`channel_num` 指明通道号。
/// * 输出时:驱动把 double buffer 的两个地址写进 `buffers[0..2]`。
///
/// # 对齐:只能按未对齐方式访问
///
/// SDK 的 `asio.h` 全局带 `#pragma pack(4)`,所以 C 侧这个结构体的 `alignof`
/// 是 4;而它含指针,在 Rust 里 `align_of == 8`。宿主传进来的数组因此可能
/// 只按 4 字节对齐,对元素取 `&mut` 引用属于 UB。
///
/// 所以驱动侧一律用 `read_unaligned` / `write_unaligned` 按值读写、不取
/// 引用 —— 见 `driver::vt_create_buffers`。[`ASIOCallbacks`] 同理。本文件
/// 其余面向宿主的结构体(如 [`ASIOChannelInfo`]、[`ASIOClockSource`])不含
/// 指针,两侧对齐都是 4,可以直接取引用。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOBufferInfo {
    pub is_input: ASIOBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

impl Default for ASIOBufferInfo {
    fn default() -> Self {
        ASIOBufferInfo {
            is_input: ASIO_FALSE,
            channel_num: 0,
            buffers: [core::ptr::null_mut(); 2],
        }
    }
}

/// `ASIOChannelInfo` —— `ASIOGetChannelInfo()` 的输入/输出。
///
/// 字段顺序必须与 SDK 2.3 一致,理由见本模块顶部的说明。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOChannelInfo {
    /// 输入:通道号。
    pub channel: i32,
    /// 输入:是否输入通道。
    pub is_input: ASIOBool,
    /// 输出:该通道当前是否活跃。
    pub is_active: ASIOBool,
    /// 输出:通道组。
    pub channel_group: i32,
    /// 输出:采样格式。
    pub r#type: ASIOSampleType,
    /// 输出:通道名,最多 31 个字符 + 结尾的 0。
    pub name: [u8; 32],
}

impl Default for ASIOChannelInfo {
    fn default() -> Self {
        ASIOChannelInfo {
            channel: 0,
            is_input: ASIO_FALSE,
            is_active: ASIO_FALSE,
            channel_group: 0,
            r#type: sample_type::FLOAT32_LSB,
            name: [0; 32],
        }
    }
}

impl ASIOChannelInfo {
    /// 写入通道名。
    ///
    /// 编码交给 [`encode_for_asio`] —— ASIO 是个 1996 年的纯 C 接口,从没
    /// 规定过 `char[]` 用什么编码,只能挑一个宿主普遍认的,见那里的说明。
    pub fn set_name(&mut self, name: &str) {
        let encoded = encode_for_asio(name, self.name.len() - 1);
        self.name[..encoded.len()].copy_from_slice(&encoded);
        for b in self.name[encoded.len()..].iter_mut() {
            *b = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// 字符串编码
// ---------------------------------------------------------------------------

/// 把 Rust 字符串编码成 ASIO 期望的字节序列,最多 `max_bytes` 个字节。
///
/// ASIO 里通道名、驱动名、错误信息都是 `char xxx[N]`,而这个协议**从来没
/// 规定过编码** —— 于是各家宿主的解释方式并不一致:
///
/// * 老宿主沿用系统 ANSI 代码页(中文 Windows 上是 GBK);
/// * 现在的宿主(Cantabile、REAPER、Ableton……)普遍按 **UTF-8** 处理。
///
/// 早先这里用 `WideCharToMultiByte(CP_ACP)` 写 GBK 字节,结果中文在
/// Cantabile 里全是乱码 —— 它按 UTF-8 去解 GBK 的字节,自然对不上。现在
/// 统一写 UTF-8,这也是 FlexASIO 等现代驱动的做法。代价是只认 ANSI 的老
/// 宿主会显示不正常,那种情况可以把配置里的 `use_non_ascii_channel_names`
/// 关掉,退化成纯 ASCII 名字。
///
/// 截断按**字符**进行,不会把一个多字节字符切成两半。
pub fn encode_for_asio(s: &str, max_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(max_bytes);
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let part = ch.encode_utf8(&mut buf).as_bytes();
        if out.len() + part.len() > max_bytes {
            break;
        }
        out.extend_from_slice(part);
    }
    out
}

/// `ASIOClockSource` —— `ASIOGetClockSources()` 用。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: ASIOBool,
    pub name: [u8; 32],
}

impl Default for ASIOClockSource {
    fn default() -> Self {
        ASIOClockSource {
            index: 0,
            associated_channel: -1,
            associated_group: -1,
            is_current_source: ASIO_FALSE,
            name: [0; 32],
        }
    }
}

/// `ASIOCallbacks` —— 宿主交给驱动的回调表。
///
/// 全部是函数指针,所以整个结构体是 `Copy + Send + Sync`。
///
/// 和 [`ASIOBufferInfo`] 一样,它在 `#pragma pack(4)` 下只有 4 字节对齐,
/// 而 Rust 这边是 8 —— 所以读取时要用 `read_unaligned`,不能直接解引用。
///
/// PigASIO 只使用 `bufferSwitch`。`bufferSwitchTimeInfo` 需要构造
/// `ASIOTime`,而那个结构体在 `#pragma pack(4)` 下的布局与 Rust 的
/// 自然对齐规则不同(size 差 4 字节),很容易写出内存错位。既然 ASIO 的
/// 时间信息对宿主来说是可选的,这里就干脆不声明支持(`asioMessage` 对
/// `kAsioSupportsTimeInfo` 一律回 0),宿主会自动退回 `bufferSwitch`。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ASIOCallbacks {
    /// 缓冲区交换通知。宿主在这里读输入缓冲、写输出缓冲。
    pub buffer_switch:
        Option<unsafe extern "system" fn(double_buffer_index: i32, direct_process: ASIOBool)>,
    /// 采样率变化通知。
    pub sample_rate_did_change: Option<unsafe extern "system" fn(s_rate: ASIOSampleRate)>,
    /// 通用消息通道,见 [`asio_message`]。
    pub asio_message: Option<
        unsafe extern "system" fn(
            selector: i32,
            value: i32,
            message: *mut c_void,
            opt: *mut f64,
        ) -> i32,
    >,
    /// 带时间信息的缓冲区交换。PigASIO 不会调用它,但结构体里必须留着
    /// 这个字段,否则宿主按 ASIO 2.3 的布局读取时会错位。
    pub buffer_switch_time_info: Option<
        unsafe extern "system" fn(
            params: *mut c_void,
            double_buffer_index: i32,
            direct_process: ASIOBool,
        ) -> *mut c_void,
    >,
}

/// `asioMessage()` 的 selector(`asio.h` 的 `asioMessage selectors` 段)。
pub mod asio_message {
    pub const SELECTOR_SUPPORTED: i32 = 1;
    pub const ENGINE_VERSION: i32 = 2;
    pub const RESET_REQUEST: i32 = 3;
    pub const BUFFER_SIZE_CHANGE: i32 = 4;
    pub const RESYNC_REQUEST: i32 = 5;
    pub const LATENCIES_CHANGED: i32 = 6;
    pub const SUPPORTS_TIME_INFO: i32 = 7;
    pub const SUPPORTS_TIME_CODE: i32 = 8;
    pub const MMC_COMMAND: i32 = 9;
    pub const SUPPORTS_INPUT_MONITOR: i32 = 10;
    pub const SUPPORTS_INPUT_GAIN: i32 = 11;
    pub const SUPPORTS_INPUT_METER: i32 = 12;
    pub const SUPPORTS_OUTPUT_GAIN: i32 = 13;
    pub const SUPPORTS_OUTPUT_METER: i32 = 14;
    pub const OVERLOAD: i32 = 15;
    pub const NUM_MESSAGE_SELECTORS: i32 = 16;
}

// ---------------------------------------------------------------------------
// IASIO 接口
// ---------------------------------------------------------------------------

/// `IASIO` 的虚函数表。
///
/// **字段顺序必须与 `common/iasiodrv.h` 里的声明顺序逐字对应。**
/// ASIO 不使用规范的 COM:`CoCreateInstance()` 时把 CLSID 当 IID 传,
/// 拿到指针后直接盲转成 `IASIO*`。也就是说宿主是按固定偏移去取函数指针的,
/// 这里顺序错了不会编译失败,而是直接跳到错误的函数上。
///
/// 前三个是 `IUnknown` 的方法 —— `IASIO` 继承自 `IUnknown`,
/// 所以虚表以它们开头。
#[repr(C)]
pub struct IAsioVtbl {
    // ---- IUnknown ----
    pub query_interface: unsafe extern "system" fn(
        this: *mut c_void,
        riid: *const Guid,
        ppv_object: *mut *mut c_void,
    ) -> i32,
    pub add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub release: unsafe extern "system" fn(this: *mut c_void) -> u32,

    // ---- IASIO ----
    pub init: unsafe extern "system" fn(this: *mut c_void, sys_handle: *mut c_void) -> ASIOBool,
    pub get_driver_name: unsafe extern "system" fn(this: *mut c_void, name: *mut u8),
    pub get_driver_version: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub get_error_message: unsafe extern "system" fn(this: *mut c_void, string: *mut u8),
    pub start: unsafe extern "system" fn(this: *mut c_void) -> ASIOError,
    pub stop: unsafe extern "system" fn(this: *mut c_void) -> ASIOError,
    pub get_channels: unsafe extern "system" fn(
        this: *mut c_void,
        num_input: *mut i32,
        num_output: *mut i32,
    ) -> ASIOError,
    pub get_latencies: unsafe extern "system" fn(
        this: *mut c_void,
        input_latency: *mut i32,
        output_latency: *mut i32,
    ) -> ASIOError,
    pub get_buffer_size: unsafe extern "system" fn(
        this: *mut c_void,
        min_size: *mut i32,
        max_size: *mut i32,
        preferred_size: *mut i32,
        granularity: *mut i32,
    ) -> ASIOError,
    pub can_sample_rate:
        unsafe extern "system" fn(this: *mut c_void, sample_rate: ASIOSampleRate) -> ASIOError,
    pub get_sample_rate:
        unsafe extern "system" fn(this: *mut c_void, sample_rate: *mut ASIOSampleRate) -> ASIOError,
    pub set_sample_rate:
        unsafe extern "system" fn(this: *mut c_void, sample_rate: ASIOSampleRate) -> ASIOError,
    pub get_clock_sources: unsafe extern "system" fn(
        this: *mut c_void,
        clocks: *mut ASIOClockSource,
        num_sources: *mut i32,
    ) -> ASIOError,
    pub set_clock_source: unsafe extern "system" fn(this: *mut c_void, reference: i32) -> ASIOError,
    pub get_sample_position: unsafe extern "system" fn(
        this: *mut c_void,
        s_pos: *mut ASIOSamples,
        t_stamp: *mut ASIOTimeStamp,
    ) -> ASIOError,
    pub get_channel_info:
        unsafe extern "system" fn(this: *mut c_void, info: *mut ASIOChannelInfo) -> ASIOError,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut c_void,
        buffer_infos: *mut ASIOBufferInfo,
        num_channels: i32,
        buffer_size: i32,
        callbacks: *mut ASIOCallbacks,
    ) -> ASIOError,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut c_void) -> ASIOError,
    pub control_panel: unsafe extern "system" fn(this: *mut c_void) -> ASIOError,
    pub future:
        unsafe extern "system" fn(this: *mut c_void, selector: i32, opt: *mut c_void) -> ASIOError,
    pub output_ready: unsafe extern "system" fn(this: *mut c_void) -> ASIOError,
}

// ---------------------------------------------------------------------------
// COM 基础设施
// ---------------------------------------------------------------------------

/// COM 的 `GUID`/`CLSID`/`IID` 布局。
///
/// 字符串形式 `{3F8C2A91-7B4E-4D63-9E15-C0A7D2B84F31}` 的解析规则是
/// 前三段按**小端**读成整数,后两段按字节原样排列。
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// 从 `(u32, u16, u16, [u8; 8])` 构造。
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Guid {
            data1,
            data2,
            data3,
            data4,
        }
    }

    /// 格式化成注册表里使用的 `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` 形式。
    pub fn to_registry_string(self) -> String {
        format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7],
        )
    }
}

/// PigASIO 的 COM 组件 ID。
///
/// 这个值会写进 `HKCR\CLSID\...` 和 `HKLM\SOFTWARE\ASIO\PigASIO\CLSID`,
/// ASIO 宿主正是靠它来 `CoCreateInstance` 的。发布之后**不要改动** ——
/// 改了会让已有的宿主配置全部失效。
pub const CLSID_PIGASIO: Guid = Guid::new(
    0x3F8C_2A91,
    0x7B4E,
    0x4D63,
    [0x9E, 0x15, 0xC0, 0xA7, 0xD2, 0xB8, 0x4F, 0x31],
);

/// `IID_IUnknown`,所有 COM 对象的默认接口。
pub const IID_IUNKNOWN: Guid = Guid::new(
    0x0000_0000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

/// `IID_IClassFactory`。
pub const IID_ICLASS_FACTORY: Guid = Guid::new(
    0x0000_0001,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

pub const S_OK: i32 = 0;
pub const S_FALSE: i32 = 1;
pub const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
pub const E_POINTER: i32 = 0x8000_4003u32 as i32;
pub const E_FAIL: i32 = 0x8000_4005u32 as i32;
pub const E_INVALIDARG: i32 = 0x8000_7000u32 as i32;
pub const CLASS_E_NOAGGREGATION: i32 = 0x8004_0110u32 as i32;
pub const CLASS_E_CLASSNOTAVAILABLE: i32 = 0x8004_0111u32 as i32;

/// `IClassFactory` 的虚函数表。
#[repr(C)]
pub struct IClassFactoryVtbl {
    pub query_interface: unsafe extern "system" fn(
        this: *mut c_void,
        riid: *const Guid,
        ppv_object: *mut *mut c_void,
    ) -> i32,
    pub add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub release: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub create_instance: unsafe extern "system" fn(
        this: *mut c_void,
        outer_unknown: *mut c_void,
        riid: *const Guid,
        ppv_object: *mut *mut c_void,
    ) -> i32,
    pub lock_server: unsafe extern "system" fn(this: *mut c_void, lock: i32) -> i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn 基础类型宽度符合_windows_llp64() {
        // long 在 Windows 上是 32 位,即使 64 位进程。
        assert_eq!(size_of::<ASIOBool>(), 4);
        assert_eq!(size_of::<ASIOError>(), 4);
        assert_eq!(size_of::<ASIOSampleType>(), 4);
        assert_eq!(size_of::<ASIOSampleRate>(), 8);

        // NATIVE_INT64 == 0,所以这两个是结构体。
        assert_eq!(size_of::<ASIOSamples>(), 8);
        assert_eq!(size_of::<ASIOTimeStamp>(), 8);
    }

    #[test]
    fn 结构体大小与官方_sdk_一致() {
        // 对着 asio.h 数出来的:
        // ASIOBufferInfo = 4 + 4 + 2*8 = 24
        assert_eq!(size_of::<ASIOBufferInfo>(), 24);
        // ASIOChannelInfo = 4 + 4 + 4 + 4 + 4 + 32 = 52
        assert_eq!(size_of::<ASIOChannelInfo>(), 52);
        // ASIOClockSource = 4 + 4 + 4 + 4 + 32 = 48
        assert_eq!(size_of::<ASIOClockSource>(), 48);
        // ASIOCallbacks = 4 个函数指针
        assert_eq!(size_of::<ASIOCallbacks>(), size_of::<*const c_void>() * 4);
        assert_eq!(align_of::<ASIOChannelInfo>(), 4);
    }

    #[test]
    fn iasio_虚表有_24_个入口() {
        // IUnknown 3 个 + IASIO 21 个。
        let slots = size_of::<IAsioVtbl>() / size_of::<*const c_void>();
        assert_eq!(slots, 24, "IASIO 虚表项数必须与 asio.h 一致");
    }

    #[test]
    fn 虚表字段顺序与头文件一致() {
        // 用字段偏移把顺序锁死。任何一次字段重排(比如把 `start` 挪到
        // `getChannels` 后面)都会让这个测试失败,而不是静静地在宿主里崩溃。
        use core::mem::offset_of;
        let ptr = size_of::<*const c_void>();

        // IUnknown
        assert_eq!(offset_of!(IAsioVtbl, query_interface), 0);
        assert_eq!(offset_of!(IAsioVtbl, add_ref), ptr);
        assert_eq!(offset_of!(IAsioVtbl, release), 2 * ptr);
        // IASIO,顺序照抄 iasiodrv.h
        assert_eq!(offset_of!(IAsioVtbl, init), 3 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_driver_name), 4 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_driver_version), 5 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_error_message), 6 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, start), 7 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, stop), 8 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_channels), 9 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_latencies), 10 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_buffer_size), 11 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, can_sample_rate), 12 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_sample_rate), 13 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, set_sample_rate), 14 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_clock_sources), 15 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, set_clock_source), 16 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_sample_position), 17 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, get_channel_info), 18 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, create_buffers), 19 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, dispose_buffers), 20 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, control_panel), 21 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, future), 22 * ptr);
        assert_eq!(offset_of!(IAsioVtbl, output_ready), 23 * ptr);
    }

    #[test]
    fn guid_格式化与_windows_一致() {
        assert_eq!(
            CLSID_PIGASIO.to_registry_string(),
            "{3F8C2A91-7B4E-4D63-9E15-C0A7D2B84F31}"
        );
    }

    #[test]
    fn 采样计数高低位拆分正确() {
        let v = 0x0000_0001_0000_0002u64;
        let s = ASIOSamples::from_u64(v);
        assert_eq!(s.hi, 1);
        assert_eq!(s.lo, 2);
        assert_eq!(s.to_u64(), v);
    }

    #[test]
    fn 通道名会被截断并补零() {
        let mut info = ASIOChannelInfo::default();
        info.set_name(&"x".repeat(100));
        assert_eq!(info.name[31], 0);
        assert_eq!(info.name[30], b'x');

        info.set_name("IN 1");
        assert_eq!(&info.name[..4], b"IN 1");
        assert_eq!(info.name[4], 0);
    }
}
