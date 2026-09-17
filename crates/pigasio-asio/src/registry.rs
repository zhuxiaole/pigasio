//! COM 组件的注册与注销。
//!
//! 要出现在 ASIO 宿主的驱动列表里,必须写两组注册表项:
//!
//! 1. **`HKCR\CLSID\{...}`** —— 让 `CoCreateInstance` 能找到我们的 DLL。
//!    `InprocServer32` 指向 DLL 的完整路径,`ThreadingModel` 必须是
//!    `Both`(宿主可能在任意线程创建驱动对象)。
//! 2. **`HKLM\SOFTWARE\ASIO\<驱动名>`** —— 这是 ASIO 自己的约定,
//!    宿主靠枚举这个键来列出所有已安装的驱动。
//!
//! 第二组键在 `HKLM` 下,写入需要管理员权限 —— 所以 `regsvr32` 必须
//! 以管理员身份运行。FlexASIO 的安装包也是这么做的。

use crate::abi::CLSID_PIGASIO;

/// 驱动在 ASIO 宿主列表里显示的名字。也是 `HKLM\SOFTWARE\ASIO\` 下的子键名。
pub const ASIO_DRIVER_KEY: &str = "PigASIO";

/// 注册结果。`Err` 里是给用户看的说明。
pub type RegResult = core::result::Result<(), String>;

/// 把 DLL 注册成 COM 组件和 ASIO 驱动。
pub fn register_server(dll_path: &std::path::Path) -> RegResult {
    platform::register_server(dll_path)
}

/// 注销。会删掉本驱动写入的所有键。
pub fn unregister_server() -> RegResult {
    platform::unregister_server()
}

/// 当前进程是否以管理员权限(elevated)运行。
///
/// `None` 表示查不出来(拿不到自己的进程令牌)—— 调用方这时应当**放行**,
/// 让真正的注册 / 注销去报错,而不是凭一次失败的探测就把用户拦住。
///
/// 注册要写 `HKLM\SOFTWARE\ASIO`,没有管理员权限必然失败,错误码是
/// `ERROR_ACCESS_DENIED`(5)。先问一句能给用户更直接的提示,省得他对着
/// 一个错误码猜。
pub fn is_elevated() -> Option<bool> {
    platform::is_elevated()
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT,
        HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    /// 把 Rust 字符串转成以 0 结尾的 UTF-16。
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// 打开(必要时创建)一个子键。
    unsafe fn create_key(root: HKEY, subkey: &str) -> Result<HKEY, String> {
        let path = wide(subkey);
        let mut handle: HKEY = core::ptr::null_mut();
        let mut disposition = 0u32;
        let status = RegCreateKeyExW(
            root,
            path.as_ptr(),
            0,
            core::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            core::ptr::null(),
            &mut handle,
            &mut disposition,
        );
        if status != ERROR_SUCCESS {
            return Err(format!(
                "创建注册表键 HK…\\{subkey} 失败(错误码 {status});\
                 注册 ASIO 驱动需要写 HKLM,请以管理员身份重新运行本命令"
            ));
        }
        Ok(handle)
    }

    /// 写一个字符串值。`name = None` 表示写默认值。
    unsafe fn set_string(key: HKEY, name: Option<&str>, value: &str) -> Result<(), String> {
        let name_wide = name.map(wide);
        let value_wide = wide(value);
        let status = RegSetValueExW(
            key,
            name_wide
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(core::ptr::null()),
            0,
            REG_SZ,
            value_wide.as_ptr() as *const u8,
            (value_wide.len() * 2) as u32,
        );
        if status != ERROR_SUCCESS {
            return Err(format!("写注册表值失败(错误码 {status})"));
        }
        Ok(())
    }

    /// 一个 RAII 包装,保证句柄一定被关掉。
    struct Key(HKEY);

    impl Key {
        unsafe fn create(root: HKEY, subkey: &str) -> Result<Self, String> {
            create_key(root, subkey).map(Key)
        }

        unsafe fn set(&self, name: Option<&str>, value: &str) -> Result<(), String> {
            set_string(self.0, name, value)
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    pub(super) fn register_server(dll_path: &std::path::Path) -> RegResult {
        let clsid = CLSID_PIGASIO.to_registry_string();
        let dll = dll_path
            .to_str()
            .ok_or_else(|| format!("DLL 路径包含非 Unicode 字符:{}", dll_path.display()))?;

        log::info!("注册 PigASIO 驱动:DLL = {dll},CLSID = {clsid}");

        unsafe {
            // ---- HKCR\CLSID\{...} ----
            let clsid_key = Key::create(HKEY_CLASSES_ROOT, &format!("CLSID\\{clsid}"))?;
            clsid_key.set(None, "PigASIO —— 多设备 ASIO 驱动")?;

            let inproc_key = Key::create(
                HKEY_CLASSES_ROOT,
                &format!("CLSID\\{clsid}\\InprocServer32"),
            )?;
            inproc_key.set(None, dll)?;
            // 宿主可能从任意线程创建驱动对象,所以必须是 Both。
            inproc_key.set(Some("ThreadingModel"), "Both")?;

            // ---- HKCR\PigASIO.PigASIO.1(ProgID)----
            let progid_key = Key::create(HKEY_CLASSES_ROOT, "PigASIO.PigASIO.1")?;
            progid_key.set(None, ASIO_DRIVER_KEY)?;
            let progid_clsid_key = Key::create(HKEY_CLASSES_ROOT, "PigASIO.PigASIO.1\\CLSID")?;
            progid_clsid_key.set(None, &clsid)?;

            // ---- HKLM\SOFTWARE\ASIO\PigASIO(ASIO 宿主靠这个枚举驱动)----
            let asio_key = Key::create(
                HKEY_LOCAL_MACHINE,
                &format!("SOFTWARE\\ASIO\\{ASIO_DRIVER_KEY}"),
            )?;
            asio_key.set(Some("CLSID"), &clsid)?;
            asio_key.set(Some("Description"), "PigASIO —— 多设备 ASIO 驱动")?;
        }

        log::info!("注册完成");
        Ok(())
    }

    pub(super) fn unregister_server() -> RegResult {
        let clsid = CLSID_PIGASIO.to_registry_string();
        log::info!("注销 PigASIO 驱动(CLSID = {clsid})");

        let mut failures: Vec<String> = Vec::new();
        unsafe {
            for (root, path) in [
                (HKEY_CLASSES_ROOT, format!("CLSID\\{clsid}")),
                (HKEY_CLASSES_ROOT, "PigASIO.PigASIO.1".to_string()),
                (
                    HKEY_LOCAL_MACHINE,
                    format!("SOFTWARE\\ASIO\\{ASIO_DRIVER_KEY}"),
                ),
            ] {
                let wide_path = wide(&path);
                let status = RegDeleteTreeW(root, wide_path.as_ptr());
                // "不存在"正是卸载想要的终态,不算失败 —— 重复卸载、
                // 或者注册本来就只成功了一半,都会走到这里。
                if status != ERROR_SUCCESS
                    && status != ERROR_FILE_NOT_FOUND
                    && status != ERROR_PATH_NOT_FOUND
                {
                    log::warn!("删除注册表键 {path} 失败(错误码 {status})");
                    failures.push(format!("{path}(错误码 {status})"));
                }
            }
        }

        // 这里以前把失败全吞了、恒返回 Ok,于是没有管理员权限时 CLI 会打印
        // "注销完成"、退出码 0,而 HKLM\SOFTWARE\ASIO\PigASIO 还在原地 ——
        // 宿主列表里驱动依旧在,用户却以为已经卸掉了。
        if failures.is_empty() {
            log::info!("注销完成");
            Ok(())
        } else {
            Err(format!(
                "以下注册表项删除失败:{}\
                 删除 HKLM 下的项需要管理员权限,请以管理员身份重新运行本命令",
                failures.join("、")
            ))
        }
    }

    pub(super) fn is_elevated() -> Option<bool> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token: HANDLE = core::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut returned = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                core::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            let _ = CloseHandle(token);
            if ok == 0 {
                None
            } else {
                Some(elevation.TokenIsElevated != 0)
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn register_server(_dll_path: &std::path::Path) -> RegResult {
        Err("PigASIO 的 ASIO 驱动只能在 Windows 上注册".into())
    }

    pub(super) fn unregister_server() -> RegResult {
        Err("PigASIO 的 ASIO 驱动只能在 Windows 上注册".into())
    }

    pub(super) fn is_elevated() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 注册表键名符合_asio_约定() {
        // ASIO 宿主枚举 HKLM\SOFTWARE\ASIO 的子键,并把子键名当作驱动名显示。
        assert_eq!(ASIO_DRIVER_KEY, "PigASIO");
    }

    #[test]
    fn clsid_字符串格式正确() {
        let s = CLSID_PIGASIO.to_registry_string();
        assert!(s.starts_with('{'));
        assert!(s.ends_with('}'));
        // 8-4-4-4-12
        let inner = &s[1..s.len() - 1];
        let parts: Vec<_> = inner.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
    }

    #[cfg(windows)]
    #[test]
    fn 能查到自己的提升状态() {
        // 当前进程的令牌一定打得开,所以这里不该是 None —— 是 None 就说明
        // `GetTokenInformation` 那一串调用写错了。至于具体是 true 还是
        // false 取决于跑测试的人是不是用了管理员终端,两种都合法。
        assert!(
            is_elevated().is_some(),
            "查不到本进程的提升状态,权限探测实现有问题"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn 非_windows_上查不到提升状态() {
        // 桩实现:CLI 看到 None 会放行,由后续的注册调用去报平台错误。
        assert_eq!(is_elevated(), None);
    }
}
