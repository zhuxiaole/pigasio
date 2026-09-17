//! PigASIO 的 ASIO 驱动 DLL。
//!
//! 这个 crate 负责把 [`pigasio_core`] 的多设备引擎包装成一个符合
//! Steinberg ASIO 2.3 规范的进程内 COM 服务器。
//!
//! # 构建与安装
//!
//! ```text
//! cargo build -p pigasio-asio --release
//! # 产物:crates/pigasio-asio/../../target/release/pigasio_asio.dll
//! ```
//!
//! 把 DLL 放到一个固定位置,然后以**管理员身份**注册:
//!
//! ```text
//! regsvr32 pigasio_asio.dll      # 注册
//! regsvr32 /u pigasio_asio.dll   # 注销
//! ```
//!
//! 注册后重启宿主软件,PigASIO 就会出现在它的 ASIO 驱动列表里。
//!
//! # 为什么只支持 64 位
//!
//! ASIO 在 32 位下用 MSVC 的 `__thiscall` 调用接口方法,而 Rust 稳定版
//! 无法表达这个调用约定。64 位下所有调用约定统一,`extern "system"`
//! 就是正确答案。现代 ASIO 宿主几乎都是 64 位的。

pub mod abi;
pub mod driver;
pub mod factory;
pub mod registry;

use std::path::{Path, PathBuf};
use std::sync::Once;

use abi::*;

/// 保证日志系统只初始化一次。
static LOG_INIT: Once = Once::new();

/// 按需初始化日志。
///
/// 沿用 FlexASIO 的思路:用户目录下存在 `PigASIO.log` 就自动开启日志。
/// 驱动跑在宿主进程里,没有控制台可用,"文件存在即开启"是最省事的开关。
fn ensure_initialized() {
    LOG_INIT.call_once(|| {
        match pigasio_core::log::init() {
            pigasio_core::log::LogStatus::Enabled(path) => {
                log::info!(
                    "==== PigASIO {} 日志已开启 ====",
                    pigasio_core::DRIVER_VERSION
                );
                log::info!("日志文件:{}", path.display());
            }
            pigasio_core::log::LogStatus::Failed(path, reason) => {
                // 没法写日志,但也不能因此让驱动起不来。
                // 这时候唯一能做的就是把原因留在调试器输出里。
                output_debug_string(&format!(
                    "PigASIO: 无法写入日志文件 {}:{reason}",
                    path.display()
                ));
            }
            pigasio_core::log::LogStatus::Disabled => {}
        }
    });
}

/// 往调试器输出一行。没有调试器时是空操作。
fn output_debug_string(message: &str) {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;

        let wide: Vec<u16> = OsStr::new(message)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: wide 是以 0 结尾的合法 UTF-16 缓冲。
        unsafe { OutputDebugStringW(wide.as_ptr()) };
    }
    #[cfg(not(windows))]
    {
        let _ = message;
    }
}

// ---------------------------------------------------------------------------
// 路径辅助
// ---------------------------------------------------------------------------

/// 取得当前 DLL 的完整路径。
#[cfg(windows)]
pub fn module_path() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    // 用模块内某个函数的地址反查本模块句柄,避免依赖调用方传进来的 hModule
    // —— `DllRegisterServer` 虽然是 regsvr32 调用的,但我们拿不到它的 hModule。
    let mut module = core::ptr::null_mut();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            module_path as *const u16,
            &mut module,
        )
    };
    if ok == 0 {
        return None;
    }

    let mut buffer = vec![0u16; 32768];
    let len = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize >= buffer.len() {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
pub fn module_path() -> Option<PathBuf> {
    None
}

/// 宿主可执行文件所在目录。
///
/// 配置文件查找的第一个位置就是这里 —— 把 `PigASIO.toml` 放在 DAW 的
/// 安装目录下,就能只对这一款软件生效,而不影响其他宿主。
#[cfg(windows)]
pub fn host_executable_dir() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;

    // 传 null 句柄得到的是**宿主进程**的 exe 路径,而不是本 DLL 的。
    let mut buffer = vec![0u16; 32768];
    let len = unsafe {
        GetModuleFileNameW(
            core::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if len == 0 {
        return None;
    }
    buffer.truncate(len as usize);
    let path = PathBuf::from(OsString::from_wide(&buffer));
    path.parent().map(Path::to_path_buf)
}

#[cfg(not(windows))]
pub fn host_executable_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// 启动控制面板程序。
///
/// 控制面板是一个独立的可执行文件,与 DLL 放在同一目录。之所以不让
/// 控制面板跑在宿主进程里,是因为 GUI 框架和音频驱动塞进同一个进程
/// 只会互相拖累:一个界面卡顿就可能让宿主掉音。
pub fn launch_control_panel(config_path: Option<&Path>) -> Result<(), String> {
    let dll_dir = module_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| "无法确定 PigASIO 的安装目录".to_string())?;

    let candidates = ["pigasio-gui.exe", "pigasio-gui"];
    let exe = candidates
        .iter()
        .map(|name| dll_dir.join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            format!(
                "在 {} 里找不到 pigasio-gui.exe;请先构建控制面板,或者直接编辑 PigASIO.toml",
                dll_dir.display()
            )
        })?;

    let mut command = std::process::Command::new(&exe);
    if let Some(config) = config_path {
        command.arg("--config").arg(config);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {} 失败:{e}", exe.display()))
}

// ---------------------------------------------------------------------------
// DLL 导出
// ---------------------------------------------------------------------------

/// `DllGetClassObject` —— COM 用它取得类工厂。
///
/// # Safety
/// 由 COM 运行时调用,三个指针都遵循 COM 的约定。
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const Guid,
    riid: *const Guid,
    ppv: *mut *mut core::ffi::c_void,
) -> i32 {
    ensure_initialized();
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = core::ptr::null_mut();

    if rclsid.is_null() || *rclsid != CLSID_PIGASIO {
        let got = if rclsid.is_null() {
            "null".to_string()
        } else {
            (*rclsid).to_registry_string().to_string()
        };
        log::warn!("DllGetClassObject 收到未知 CLSID {got}");
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    if riid.is_null() {
        // COM 规范:除输出参数以外的空指针一律返回 E_POINTER。
        return E_POINTER;
    }
    if *riid != IID_ICLASS_FACTORY && *riid != IID_IUNKNOWN {
        return E_NOINTERFACE;
    }

    *ppv = factory::class_factory();
    S_OK
}

/// `DllCanUnloadNow` —— 恒返回 `S_FALSE`(不卸载)。
///
/// 让驱动 DLL 在宿主运行期间卸载没有好处,只会在宿主里留下悬空的
/// 函数指针。FlexASIO 同样选择常驻。
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    factory::can_unload_now()
}

/// `DllRegisterServer` —— `regsvr32` 会调用它。
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> i32 {
    ensure_initialized();
    let Some(path) = module_path() else {
        log::error!("无法确定 DLL 自身路径,注册失败");
        output_debug_string("PigASIO: 无法确定 DLL 自身的路径");
        return E_FAIL;
    };

    match registry::register_server(&path) {
        Ok(()) => {
            log::info!("PigASIO 注册成功");
            S_OK
        }
        Err(e) => {
            log::error!("注册失败:{e}");
            output_debug_string(&format!("PigASIO 注册失败:{e}"));
            E_FAIL
        }
    }
}

/// `DllUnregisterServer` —— `regsvr32 /u` 会调用它。
#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> i32 {
    ensure_initialized();
    match registry::unregister_server() {
        Ok(()) => {
            log::info!("PigASIO 注销成功");
            S_OK
        }
        Err(e) => {
            log::error!("注销失败:{e}");
            output_debug_string(&format!("PigASIO 注销失败:{e}"));
            E_FAIL
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // 直接用 `abi::` 前缀引用,避免与外层通配导入重复。

    #[test]
    fn dll_导出在错误的_clsid_上会拒绝() {
        let wrong = Guid::new(0xDEAD_BEEF, 0, 0, [0; 8]);
        let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
        let hr = unsafe { DllGetClassObject(&wrong, &IID_ICLASS_FACTORY, &mut out) };
        assert_eq!(hr, CLASS_E_CLASSNOTAVAILABLE);
        assert!(out.is_null());
    }

    #[test]
    fn dll_导出能给出类工厂() {
        let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
        let hr = unsafe { DllGetClassObject(&CLSID_PIGASIO, &IID_ICLASS_FACTORY, &mut out) };
        assert_eq!(hr, S_OK);
        assert!(!out.is_null());
    }

    #[test]
    fn dll_不会被卸载() {
        // 驱动常驻宿主进程;允许卸载只会留下悬空指针。
        assert_eq!(DllCanUnloadNow(), S_FALSE);
    }

    #[test]
    fn 宿主目录是当前可执行文件所在目录() {
        // 测试进程的 exe 在 target/debug/deps 下,父目录一定存在。
        let dir = host_executable_dir();
        assert!(dir.is_some(), "应当能取到宿主 exe 目录");
        assert!(dir.unwrap().is_dir());
    }
}
