//! PigASIO 控制面板。
//!
//! 这是一个独立的 GUI 程序,和驱动 DLL 放在同一目录。ASIO 宿主调用
//! `controlPanel()` 时,驱动会把它拉起来。
//!
//! 之所以不做成 DLL 里的对话框,是因为 GUI 框架塞进音频宿主进程只会
//! 互相拖累 —— 界面掉一帧就可能让宿主爆一次音。
//!
//! # 功能
//!
//! * 增删输入/输出设备,每一路都可以独立选择设备和通道;
//! * 编辑引擎参数(采样率、缓冲区、重采样质量、漂移补偿);
//! * **试运行** —— 在界面里直接启动引擎,实时看到各流的缓冲水位和
//!   漂移补偿量,不用打开 DAW 就能确认多设备是否同步;
//! * 读写 `PigASIO.toml`。

// 发布版链接成 Windows 子系统,双击时不会先弹出一个控制台黑框。
// debug 版保留控制台,开发时能看到日志。
//
// 代价是发布版没有 stdout/stderr —— 所以日志必须落到文件,
// 见下面的 `init_logging`。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;
use pigasio_core::config::{
    AsioSampleType, ChannelSelection, Config, DeviceRef, EngineConfig, ResampleQuality,
    StreamConfig, WasapiOptions,
};
use pigasio_core::{AsioBufferSet, Engine, Result as CoreResult, StreamKind, StreamStatusSnapshot};

mod theme;
use theme::ThemeMode;

fn main() -> eframe::Result<()> {
    init_logging();

    let config_path = parse_config_arg();
    let prefs = load_ui_prefs();

    // 主题的优先级:命令行 > 上次的选择 > 跟随系统。
    // `--theme light|dark` 用来强制指定,方便截图和排查。
    let theme_mode = parse_theme_arg()
        .or_else(|| prefs.theme.as_deref().and_then(ThemeMode::parse))
        .unwrap_or_default();

    let restore = WindowRestore {
        maximized: prefs.maximized == Some(true),
        pos: prefs.window_pos.map(|[x, y]| egui::pos2(x, y)),
        size: prefs.window_size.map(|[w, h]| egui::vec2(w, h)),
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_min_inner_size([760.0, 520.0])
        .with_icon(window_icon());
    if restore.maximized {
        // 最大化时位置和尺寸都不给。真正的"一出生就是最大化"由
        // `preset_maximized` 在窗口显示之前设好 —— 这里给了反而会被 Windows
        // 用来先摆一个窗口化窗口。
        //
        // 代价是取消最大化之后窗口回到系统默认大小:所以把用户上次摆的位置
        // 和尺寸留着,等他取消最大化再补回去,见 `App::pending_pos`。
    } else {
        viewport = viewport.with_inner_size(restore.size.unwrap_or(egui::vec2(1000.0, 780.0)));
        if let Some([x, y]) = prefs.window_pos {
            viewport = viewport.with_position([x, y]);
        }
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            install_ui_font(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx, theme_mode);
            Ok(Box::new(App::new(cc, config_path, theme_mode, restore)))
        }),
    );

    // 发布版没有控制台,启动失败时用户只会看到"双击没反应"。
    // 这种情况必须主动弹个对话框把原因说清楚。
    if let Err(e) = &result {
        report_fatal(&format!("控制面板启动失败:{e}"));
    }
    result
}

/// 初始化日志。
///
/// 发布版是 Windows 子系统,没有控制台可看,所以优先写文件 ——
/// 沿用驱动那套规则:用户目录下存在 `PigASIO.log` 就写进去。
/// 没有开文件日志时退回 stderr,从命令行启动时仍然能看到输出。
fn init_logging() {
    // 注意别写成 `use pigasio_core::log::{self, ...}` —— 那会把 `log`
    // 这个名字遮蔽成核心库的日志模块,`log::info!` 之类的宏就找不到了。
    use log::LevelFilter;
    use pigasio_core::log::LogStatus;

    match pigasio_core::log::init() {
        LogStatus::Enabled(path) => log::info!("控制面板启动,日志写入 {}", path.display()),
        LogStatus::Failed(path, reason) => {
            eprintln!("无法写入日志 {}:{reason}", path.display());
            pigasio_core::log::init_stderr(LevelFilter::Info);
        }
        LogStatus::Disabled => {
            // 没开文件日志。从命令行启动时 stderr 可见,双击时无人可见
            // —— 这是刻意的默认:安静。
            pigasio_core::log::init_stderr(LevelFilter::Info);
        }
    }
}

/// 报告一个致命错误。
///
/// 发布版没有控制台,只往 stderr 写的话,用户看到的就是"双击没反应"。
/// 所以这里弹一个系统对话框,至少把原因摆到面前。
fn report_fatal(message: &str) {
    log::error!("{message}");

    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

        fn wide(s: &str) -> Vec<u16> {
            OsStr::new(s)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        }

        let text = wide(message);
        let title = wide("PigASIO 控制面板");
        // SAFETY: 两个指针都指向以 0 结尾的合法 UTF-16 缓冲。
        unsafe {
            MessageBoxW(
                core::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    #[cfg(not(windows))]
    {
        let _ = message;
    }
}

/// 字体表里给中文字体用的键名。
/// 拉丁字体在字体表里的键名。
const LATIN_FONT_KEY: &str = "pigasio-latin";

/// 中文字体在字体表里的键名。
const CJK_FONT_KEY: &str = "pigasio-cjk";

/// 让界面能显示中文。
///
/// egui 自带的字体是 Hack 和 Ubuntu-Light,都不含 CJK 字形 —— 不额外
/// 加载的话,界面上每一个汉字都会渲染成一个"方框"(缺字形时的占位符)。
///
/// 这里把系统中文字体**追加**到字体列表末尾,而不是替换掉原有字体。
/// egui 按字符逐个在列表里找字形:拉丁字母会命中前面的 Hack/Ubuntu,
/// 只有汉字才会落到中文字体上。这样英文的排版观感保持不变,中文也能
/// 正常显示。
///
/// 用户可以用环境变量 `PIGASIO_FONT` 指定自己的字体文件(路径后面可以
/// 跟 `#序号` 来指定 TTC 里的第几个字体面)。
fn install_ui_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 字体列表是**按字符逐个**查找的:排在前面的先命中。所以把 Segoe UI
    // 放前面(拉丁字母、数字、标点),中文字体放后面兜底 —— 这正是
    // Fluent 的处理方式,英文和数字的观感才和系统一致。
    let mut installed = Vec::new();

    // 1. Latin:Segoe UI,Windows 11 的界面字体。
    if let Some((bytes, index, source)) = load_latin_font() {
        let mut data = egui::FontData::from_owned(bytes);
        data.index = index;
        fonts.font_data.insert(LATIN_FONT_KEY.to_owned(), data);
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(LATIN_FONT_KEY.to_owned());
        }
        installed.push(source);
    }

    // 2. CJK:雅黑一类。放在后面,只接管拉丁字体没有的字形。
    match load_cjk_font() {
        Some((bytes, index, source)) => {
            let mut data = egui::FontData::from_owned(bytes);
            data.index = index;
            fonts.font_data.insert(CJK_FONT_KEY.to_owned(), data);
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push(CJK_FONT_KEY.to_owned());
            }
            installed.push(source);
        }
        None => log::warn!(
            "系统里找不到可用的中文字体,界面上的中文会显示成方框。\
             可以用环境变量 PIGASIO_FONT 指定一个字体文件。"
        ),
    }

    ctx.set_fonts(fonts);
    if !installed.is_empty() {
        let names: Vec<_> = installed.iter().map(|p| p.display().to_string()).collect();
        log::info!("界面字体:{}", names.join(" + "));
    }
}

/// 拉丁字体的候选,按优先级。
///
/// 用静态的 `segoeui.ttf` 而不是 `SegUIVar.ttf`(Segoe UI Variable):
/// epaint 加载字体失败时会直接 **panic**,而可变字体能否被 ab_glyph
/// 正常解析并不确定。静态版观感几乎一致,没有这个风险。
fn load_latin_font() -> Option<(Vec<u8>, u32, PathBuf)> {
    if let Some(spec) = std::env::var_os("PIGASIO_LATIN_FONT") {
        let spec = spec.to_string_lossy().to_string();
        if let Some(found) = read_font_spec(&spec) {
            return Some(found);
        }
    }

    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    for name in ["Fonts/segoeui.ttf", "Fonts/SegoeUI.ttf"] {
        let path = PathBuf::from(&windir).join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            return Some((bytes, 0, path));
        }
    }
    None
}

/// 中文字体的候选,按"做界面好不好看"排序。
fn load_cjk_font() -> Option<(Vec<u8>, u32, PathBuf)> {
    if let Some(spec) = std::env::var_os("PIGASIO_FONT") {
        let spec = spec.to_string_lossy().to_string();
        if let Some(found) = read_font_spec(&spec) {
            return Some(found);
        }
    }

    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    for (relative, index) in [
        ("Fonts/msyh.ttc", 0u32), // 微软雅黑
        ("Fonts/msyhl.ttc", 0),   // 微软雅黑 Light
        ("Fonts/Deng.ttf", 0),    // 等线
        ("Fonts/simhei.ttf", 0),  // 黑体
        ("Fonts/msjh.ttc", 0),    // 微软正黑(繁体系统)
        ("Fonts/simsun.ttc", 0),  // 宋体
    ] {
        let path = PathBuf::from(&windir).join(relative);
        if let Ok(bytes) = std::fs::read(&path) {
            return Some((bytes, index, path));
        }
    }
    None
}

/// 解析 `路径` 或 `路径#序号` 形式的字体指定,并读出字节。
fn read_font_spec(spec: &str) -> Option<(Vec<u8>, u32, PathBuf)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let (path, index) = match spec.rsplit_once('#') {
        Some((p, n)) => (p, n.parse().unwrap_or(0)),
        None => (spec, 0),
    };
    let path = PathBuf::from(path);
    match std::fs::read(&path) {
        Ok(bytes) => Some((bytes, index, path)),
        Err(e) => {
            log::warn!("字体 {} 读不出来:{e}", path.display());
            None
        }
    }
}

/// 驱动通过 `--config <路径>` 把当前生效的配置文件告诉控制面板。
fn parse_config_arg() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" {
            if let Some(p) = args.get(i + 1) {
                return Some(PathBuf::from(p));
            }
        }
        i += 1;
    }
    None
}

/// 解析 `--theme light|dark|system`。
///
/// 默认跟随系统。强制指定主要用于截图和对比排查 —— 界面里也有切换按钮。
fn parse_theme_arg() -> Option<ThemeMode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--theme" {
            return match args.get(i + 1).map(String::as_str) {
                Some(value) => match ThemeMode::parse(value) {
                    Some(mode) => Some(mode),
                    None => {
                        log::warn!("--theme 的值 “{value}” 无法识别,改用记住的主题");
                        None
                    }
                },
                None => {
                    log::warn!("--theme 后面缺少参数");
                    None
                }
            };
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// 界面偏好
// ---------------------------------------------------------------------------

/// 设备下拉框里表示"不打开设备"的那一项。
///
/// `from_config` 往里写、`to_config` 往外读,两个方向必须用同一个字符串。
const NO_DEVICE: &str = "(禁用)";

/// 界面上展示的版本号。
///
/// `CARGO_PKG_VERSION` 是 cargo 在编译期塞进来的(取自 workspace 的
/// `version`),不用手工维护。
const VERSION_LABEL: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// 窗口标题。带上版本号,任务栏和 Alt+Tab 里也看得见。
///
/// 它同时还是"按标题找窗口"的依据(`find_main_window`),所以只能有这一个
/// 出处 —— 两处各写一遍字符串,改了名字就会静默找不到窗口。
///
/// 版本号这里得原样再写一遍 `env!`:`concat!` 只吃字面量,没法引用
/// [`VERSION_LABEL`]。
const WINDOW_TITLE: &str = concat!("PigASIO 控制面板 v", env!("CARGO_PKG_VERSION"));

/// 找本程序的主窗口。
///
/// eframe 没把窗口句柄交出来 —— `CreationContext` 里那个字段是 crate 私有的
/// —— 只能按标题找。同一个进程里标题是唯一的,够用。
#[cfg(windows)]
fn find_main_window() -> windows_sys::Win32::Foundation::HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;

    let title: Vec<u16> = WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 标题以 NUL 结尾。找不到窗口就是空指针,由调用方判断。
    unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) }
}

/// 让标题栏跟着主题走。
///
/// Windows 的标题栏默认由系统设置说了算,应用插不上手 —— 深色界面顶着一
/// 条白标题栏很割裂。`DWMWA_USE_IMMERSIVE_DARK_MODE` 就是给应用改这个用的
/// (Windows 10 1809 起支持)。
#[cfg(windows)]
fn apply_titlebar(dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

    let hwnd = find_main_window();
    if hwnd.is_null() {
        return;
    }
    let value: i32 = i32::from(dark);
    // SAFETY: hwnd 有效;value 是本地的 i32,长度按 API 要求给出。
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&value as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn apply_titlebar(_dark: bool) {}

/// 运行时窗口图标 —— 任务栏和 Alt+Tab 上显示的那个。
///
/// exe 里嵌的图标(见 `build.rs`)管的是"文件在资源管理器里什么样",窗口这边
/// 得单独设,不然任务栏显示的还是默认图标。
///
/// 数据是构建前用 ImageMagick 转好的裸 RGBA(`assets/pigasio-64.rgba`),
/// 这样不必为了解一张 PNG 把图片解码库拉进来。
fn window_icon() -> egui::IconData {
    /// 图标边长(像素)。
    const SIZE: usize = 64;
    const RGBA: &[u8] = include_bytes!("../../../assets/pigasio-64.rgba");
    debug_assert_eq!(RGBA.len(), SIZE * SIZE * 4, "图标数据的尺寸对不上");

    egui::IconData {
        rgba: RGBA.to_vec(),
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// 把窗口预设成"下次显示就是最大化"。
///
/// 窗口建出来之后、渲染出第一帧之前一直是隐藏的(eframe 自己就这么建,防
/// 白闪),趁这个空档把它的显示状态设成最大化 —— 等它真显出来的时候就已经
/// 是最大化尺寸了,不会先闪一下小窗口。
///
/// 为什么不用 egui 那套:
///
/// * `ViewportBuilder::with_maximized` 交到 Windows 手上会被 `with_inner_size`
///   搅乱 —— 位置进了最大化的位置,尺寸却留在窗口大小;
/// * 改成第一帧发 `ViewportCommand::Maximized` 吧,那条命令要等这一帧跑完
///   才处理,而窗口紧接着就显示了。实测会先以窗口化尺寸露一下脸,过几百
///   毫秒才跳成最大化 —— 用户看得见。
///
/// `SetWindowPlacement` 只改状态、不显示窗口,所以不会把窗口提前暴露出来
/// (那样会白屏)。等 eframe 自己 `set_visible(true)` 时,窗口直接以最大化的
/// 形态出现。
#[cfg(windows)]
fn preset_maximized() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowPlacement, SetWindowPlacement, SW_SHOWMAXIMIZED, WINDOWPLACEMENT,
    };

    let hwnd = find_main_window();
    if hwnd.is_null() {
        log::warn!("拿不到窗口句柄,最大化要等第一帧才生效(可能闪一下)");
        return;
    }

    // SAFETY: 全零的 WINDOWPLACEMENT 是合法的初始值,length 随后按 API 要求填。
    let mut placement: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    // SAFETY: hwnd 有效;placement 是本地结构,length 已填好。
    unsafe {
        if GetWindowPlacement(hwnd, &mut placement) == 0 {
            return;
        }
        placement.showCmd = SW_SHOWMAXIMIZED as u32;
        SetWindowPlacement(hwnd, &placement);
    }
}

#[cfg(not(windows))]
fn preset_maximized() {}

/// 启动时要恢复的窗口状态。
#[derive(Clone, Copy)]
struct WindowRestore {
    /// 上次关窗时是不是最大化。
    maximized: bool,
    /// 上次窗口化时窗口的位置和大小。恢复最大化时它们同样有用 —— 用户一
    /// 取消最大化,就得靠它们把窗口摆回原样。
    pos: Option<egui::Pos2>,
    size: Option<egui::Vec2>,
}

/// 界面偏好的文件名。和引擎配置放同一个目录(用户主目录)。
const UI_PREFS_FILE_NAME: &str = "PigASIO-ui.toml";

/// 界面偏好。
///
/// 单独存一份,不塞进 `PigASIO.toml`:
///
/// * 那份配置驱动也要读,而它的解析器是 `#[serde(deny_unknown_fields)]` 的,
///   多一个 `[ui]` 段会让驱动直接报错;
/// * 主题本来就是"这台机器上的这个人"的偏好,不该跟着配置文件跑 ——
///   用户完全可以把配置另存到项目目录里去。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct UiPrefs {
    /// 主题模式,[`ThemeMode::as_str`] 的那个名字。
    theme: Option<String>,
    /// 上次关窗时窗口左上角的位置(逻辑点)。
    ///
    /// 要是拔掉了外接显示器,这个值可能指向一块已经不存在的屏幕,窗口就会
    /// "开了但看不见" —— 删掉这个文件即可回到系统默认位置。
    window_pos: Option<[f32; 2]>,
    /// 上次关窗时的窗口大小(逻辑点,客户区)。
    ///
    /// **最大化期间不更新它**:那时候量到的尺寸是整个屏幕,存下来用户下次
    /// 一取消最大化,就只能看到一个占满屏幕的窗口了。
    window_size: Option<[f32; 2]>,
    /// 上次关窗时窗口是不是最大化。
    maximized: Option<bool>,
}

/// 界面偏好存在哪。跟着用户目录走,不受 `--config` 影响。
fn ui_prefs_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(UI_PREFS_FILE_NAME))
}

/// 读界面偏好。文件不存在(第一次运行)或者读坏了都退回默认值 ——
/// 界面偏好不该拦住程序启动。
fn load_ui_prefs() -> UiPrefs {
    let Some(path) = ui_prefs_path() else {
        return UiPrefs::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            log::warn!("{} 读不出来,按默认偏好看待:{e}", path.display());
            UiPrefs::default()
        }),
        Err(_) => UiPrefs::default(),
    }
}

/// 改一项界面偏好,其余保持不变。
///
/// 两个字段是各自独立地变的(切主题、挪窗口),各自直接构造一份 `UiPrefs`
/// 写回去的话,后写的那个会把先写的那个冲掉。
fn update_ui_prefs(change: impl FnOnce(&mut UiPrefs)) {
    let mut prefs = load_ui_prefs();
    change(&mut prefs);
    save_ui_prefs(&prefs);
}

/// 写界面偏好。写失败只记一条日志:主题已经生效了,不值得为它弹个错。
fn save_ui_prefs(prefs: &UiPrefs) {
    let Some(path) = ui_prefs_path() else {
        return;
    };
    match toml::to_string(prefs) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                log::warn!("{} 写不进去:{e}", path.display());
            }
        }
        Err(e) => log::warn!("界面偏好序列化失败:{e}"),
    }
}

// ---------------------------------------------------------------------------
// 可编辑的配置
// ---------------------------------------------------------------------------

/// 通道选择方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelMode {
    /// 取设备的前 N 个通道。
    Count,
    /// 手工列出通道号。
    List,
}

/// 界面上一条设备条目。与 [`StreamConfig`] 一一对应,但把“通道”拆成了
/// 两种输入方式,方便操作。
#[derive(Debug, Clone)]
struct StreamEdit {
    /// 用户选中的设备名(空字符串表示使用系统默认设备)。
    device: String,
    use_default_device: bool,
    channel_mode: ChannelMode,
    channel_count: usize,
    /// 指定通道时用户输入的原文,例如 `0, 3`。
    channels_text: String,
    gain_db: f32,
    latency_ms: Option<f32>,
    wasapi_exclusive: bool,
    clock_master: bool,
}

impl Default for StreamEdit {
    fn default() -> Self {
        StreamEdit {
            device: String::new(),
            use_default_device: true,
            channel_mode: ChannelMode::Count,
            channel_count: 2,
            channels_text: "0, 1".into(),
            gain_db: 0.0,
            latency_ms: None,
            wasapi_exclusive: false,
            clock_master: false,
        }
    }
}

impl StreamEdit {
    /// 这条流会不会真的去开一个设备。
    ///
    /// 系统默认设备算数;`(禁用)` 和空串不算 —— 口径和 [`Self::to_config`]
    /// 必须一致,不然界面上"看着有设备"的流在引擎那边是空的,时钟基准就会
    /// 落到一个不存在的地方。
    fn opens_device(&self) -> bool {
        self.use_default_device || (!self.device.is_empty() && self.device != NO_DEVICE)
    }

    /// 从配置里的 `StreamConfig` 还原出可编辑形式。
    fn from_config(cfg: &StreamConfig) -> Self {
        let (device, use_default_device) = match &cfg.device {
            DeviceRef::Default => (String::new(), true),
            DeviceRef::None => (NO_DEVICE.to_string(), false),
            DeviceRef::Substring(s) => (s.clone(), false),
            DeviceRef::Regex(r) => (format!("/{r}/"), false),
        };
        let (channel_mode, channel_count, channels_text) = match &cfg.channels {
            ChannelSelection::Count(n) => (ChannelMode::Count, *n, "0, 1".to_string()),
            ChannelSelection::List(v) => (
                ChannelMode::List,
                v.len(),
                v.iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        };
        StreamEdit {
            device,
            use_default_device,
            channel_mode,
            channel_count,
            channels_text,
            gain_db: cfg.gain_db,
            latency_ms: cfg.latency_seconds.map(|s| (s * 1000.0) as f32),
            wasapi_exclusive: cfg.wasapi.exclusive,
            clock_master: cfg.clock_master,
        }
    }

    /// 生成写回配置用的 `StreamConfig`。
    fn to_config(&self) -> StreamConfig {
        let device = if self.use_default_device {
            DeviceRef::Default
        } else if self.device == NO_DEVICE || self.device.is_empty() {
            DeviceRef::None
        } else {
            // 用完整设备名做子串匹配:这是最不容易因为驱动更新而失配的做法。
            DeviceRef::Substring(self.device.clone())
        };

        let channels = match self.channel_mode {
            ChannelMode::Count => ChannelSelection::Count(self.channel_count.max(1)),
            ChannelMode::List => {
                let mut list: Vec<usize> = self
                    .channels_text
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                list.sort_unstable();
                list.dedup();
                if list.is_empty() {
                    list.push(0);
                }
                ChannelSelection::List(list)
            }
        };

        StreamConfig {
            device,
            channels,
            latency_seconds: self.latency_ms.map(|ms| (ms / 1000.0) as f64),
            gain_db: self.gain_db,
            wasapi: WasapiOptions {
                exclusive: self.wasapi_exclusive,
                auto_convert: true,
            },
            clock_master: self.clock_master,
        }
    }
}

// ---------------------------------------------------------------------------
// 试运行
// ---------------------------------------------------------------------------

/// 界面内嵌的引擎实例。
struct Runner {
    engine: Engine,
    started: Instant,
    /// 输入峰值电平,由音频回调写入,给界面的电平表用。
    peak: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl Runner {
    fn start(config: Config) -> CoreResult<Self> {
        let mut engine = Engine::new(config)?;
        let out_channels = engine.output_channel_count();
        let in_channels = engine.input_channel_count();
        let chunk = engine.buffer_size();
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let peak_for_cb = std::sync::Arc::clone(&peak);

        let callback = Box::new(move |buffers: &mut AsioBufferSet, index: usize| {
            // 试运行不制造噪音:输出全静音,只统计输入电平。
            let mut max = 0.0f32;
            for ch in 0..in_channels {
                for &v in buffers.input_plane(ch, index) {
                    if v.is_finite() {
                        max = max.max(v.abs());
                    }
                }
            }
            peak_for_cb.store(max.to_bits(), std::sync::atomic::Ordering::Relaxed);
            for ch in 0..out_channels {
                buffers.output_plane_mut(ch, index).fill(0.0);
            }
        });

        engine.prepare(chunk, callback)?;
        engine.start()?;
        Ok(Runner {
            engine,
            started: Instant::now(),
            peak,
        })
    }

    fn peak_level(&self) -> f32 {
        f32::from_bits(self.peak.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn elapsed(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.engine.stop();
    }
}

/// 引擎正在启动或停止。
///
/// 这两件事都要跟 ASIO 驱动打交道 —— 打开设备、销毁缓冲、join 音频线程,
/// 慢起来好几秒 —— 所以都丢到后台线程上跑,结果从 channel 收回来。
enum RunnerJob {
    Starting(std::sync::mpsc::Receiver<CoreResult<Runner>>),
    Stopping(std::sync::mpsc::Receiver<()>),
}

impl RunnerJob {
    /// 工具栏按钮上的文字。
    fn button_label(&self) -> &'static str {
        match self {
            RunnerJob::Starting(_) => "正在启动…",
            RunnerJob::Stopping(_) => "正在停止…",
        }
    }

    /// 实时状态区里的说明。
    fn hint(&self) -> &'static str {
        match self {
            RunnerJob::Starting(_) => "正在打开 ASIO 设备…多设备时这一步要几秒。",
            RunnerJob::Stopping(_) => "正在关闭 ASIO 设备、释放缓冲。",
        }
    }
}

/// 后台枚举设备的结果。
///
/// 两个方向分开报错:一块设备出问题不该让另一边也空着。
struct DeviceLists {
    inputs: CoreResult<Vec<String>>,
    outputs: CoreResult<Vec<String>>,
}

// ---------------------------------------------------------------------------
// 应用
// ---------------------------------------------------------------------------

struct App {
    // ---- 可编辑的配置 ----
    sample_rate: u32,
    buffer_size: u32,
    asio_sample_type: AsioSampleType,
    resample_quality: ResampleQuality,
    drift_correction: bool,
    max_drift_ppm: f64,
    watermark_ms: f64,
    use_non_ascii_channel_names: bool,
    inputs: Vec<StreamEdit>,
    outputs: Vec<StreamEdit>,

    // ---- 环境 ----
    config_path: Option<PathBuf>,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    message: String,
    message_is_error: bool,

    // ---- 试运行 ----
    runner: Option<Runner>,
    /// 引擎正在启动或停止。见 [`RunnerJob`]。
    runner_job: Option<RunnerJob>,
    /// 正在后台线程里枚举的设备。
    ///
    /// 枚举要加载驱动、问它要通道表,同样不能压在 UI 线程上 —— 窗口刚弹
    /// 出来时那一次也在其中。
    devices_job: Option<std::sync::mpsc::Receiver<DeviceLists>>,
    /// 上一次成功读到的各流统计。
    ///
    /// [`Engine::status`] 是用 `try_lock` 拿状态的:音频回调正忙时它会返回
    /// **空**的统计。照它直接渲染,表格就会在"几行数据"和"一行提示"之间
    /// 反复跳高度 —— 而 egui 在内容变矮时会把滚动位置夹回顶部,表现正是
    /// "刷新一下又滑回顶上了"。所以留一份上次的结果兜着。
    last_stats: Vec<StreamStatusSnapshot>,
    /// 窗口左上角,每帧跟着实际位置更新,退出时写进界面偏好。
    window_pos: Option<egui::Pos2>,
    /// 窗口客户区大小,同上。
    window_size: Option<egui::Vec2>,
    /// 窗口是不是最大化,同上。
    maximized: bool,
    /// 上一次给标题栏用的深浅。
    ///
    /// 标题栏不是每帧都设一遍,而是跟着主题走 —— 只在它变了的时候调一次。
    /// 放在这里还能捎带处理"跟随系统"模式下用户在 Windows 设置里改颜色的
    /// 情况:egui 自己会跟上,标题栏得我们替它跟。
    titlebar_dark: Option<bool>,
    /// 启动时要不要恢复最大化。
    ///
    /// 只用来给第一帧发一次命令,发完就置 false。见 `main` 里那段说明 ——
    /// 光靠 `ViewportBuilder::with_maximized` 不够稳。
    restore_maximized: bool,
    /// 启动即最大化时,用户上次摆的窗口位置和大小。
    ///
    /// 恢复最大化时我们没给窗口位置和尺寸(给了会先闪一下小窗口),所以窗口
    /// 还原时得拿它们摆回原样;补完就清掉,免得反复覆盖用户后来的调整。
    /// 只有"启动即最大化"才留 —— 窗口化启动的用户自己最大化再还原时,
    /// 不该被这两个陈旧的值拽回去。
    pending_pos: Option<egui::Pos2>,
    pending_size: Option<egui::Vec2>,
    /// 当前主题模式。切换时会立即重新应用样式。
    theme_mode: ThemeMode,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        config_path: Option<PathBuf>,
        initial_theme: ThemeMode,
        restore: WindowRestore,
    ) -> Self {
        // 趁窗口还没露脸,先把最大化状态设好 —— 见 `preset_maximized`。
        if restore.maximized {
            preset_maximized();
        }

        // 主题在进入这里之前已经应用过了,这里只把模式记下来供界面切换用。
        let _ = cc;
        let mut app = App {
            sample_rate: 48_000,
            buffer_size: 1024,
            asio_sample_type: AsioSampleType::Float32,
            resample_quality: ResampleQuality::Sinc,
            drift_correction: true,
            max_drift_ppm: 500.0,
            watermark_ms: 30.0,
            use_non_ascii_channel_names: true,
            inputs: vec![StreamEdit::default()],
            outputs: vec![StreamEdit::default()],
            config_path: None,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            message: String::new(),
            message_is_error: false,
            runner: None,
            runner_job: None,
            devices_job: None,
            last_stats: Vec::new(),
            window_pos: None,
            window_size: None,
            // 恢复最大化时先当它已经最大化了:要等 `ViewportInfo` 回来才知道
            // 真实状态,而那时已经该判断"用户是不是刚取消了最大化"。
            maximized: restore.maximized,
            titlebar_dark: None,
            restore_maximized: restore.maximized,
            pending_pos: if restore.maximized { restore.pos } else { None },
            pending_size: if restore.maximized {
                restore.size
            } else {
                None
            },
            theme_mode: initial_theme,
        };

        app.refresh_devices();

        // 优先用驱动传进来的路径;没有就按标准顺序找。
        let path = config_path.or_else(|| {
            let cwd = std::env::current_dir().ok();
            pigasio_core::config::find_config_file(cwd.as_deref())
        });
        if let Some(p) = path {
            app.load_from(&p);
        } else {
            app.message = "未找到配置文件,当前显示的是默认设置。".into();
        }

        app
    }

    /// 刷新设备列表。
    ///
    /// 枚举要加载驱动、问它要通道表,压在 UI 线程上的话窗口刚弹出来那一下
    /// 就会僵住(启动时那次也走这里),所以丢给后台线程,结果由
    /// [`Self::poll_devices`] 收。
    fn refresh_devices(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let names = |kind| {
                pigasio_core::devices::enumerate(kind)
                    .map(|list| list.into_iter().map(|d| d.name).collect::<Vec<_>>())
            };
            let _ = tx.send(DeviceLists {
                inputs: names(StreamKind::Input),
                outputs: names(StreamKind::Output),
            });
        });
        self.devices_job = Some(rx);
    }

    fn set_error(&mut self, message: String) {
        self.message = message;
        self.message_is_error = true;
    }

    fn set_info(&mut self, message: String) {
        self.message = message;
        self.message_is_error = false;
    }

    fn load_from(&mut self, path: &std::path::Path) {
        match Config::from_file(path) {
            Ok(config) => {
                self.apply_config(&config);
                self.config_path = Some(path.to_path_buf());
                self.set_info(format!("已载入 {}", path.display()));
            }
            Err(e) => self.set_error(format!("载入失败:{e}")),
        }
    }

    fn apply_config(&mut self, config: &Config) {
        self.sample_rate = config.sample_rate;
        self.buffer_size = config.buffer_size_samples;
        self.asio_sample_type = config.asio_sample_type;
        self.resample_quality = config.engine.resample_quality;
        self.drift_correction = config.engine.drift_correction;
        self.max_drift_ppm = config.engine.max_drift_ppm;
        self.use_non_ascii_channel_names = config.engine.use_non_ascii_channel_names;
        self.watermark_ms = config.engine.watermark_ms;
        self.inputs = config.inputs.iter().map(StreamEdit::from_config).collect();
        self.outputs = config.outputs.iter().map(StreamEdit::from_config).collect();
    }

    /// 把界面上的编辑内容组装成一份配置。
    fn build_config(&self) -> Config {
        Config {
            sample_rate: self.sample_rate,
            buffer_size_samples: self.buffer_size,
            asio_sample_type: self.asio_sample_type,
            inputs: self.inputs.iter().map(StreamEdit::to_config).collect(),
            outputs: self.outputs.iter().map(StreamEdit::to_config).collect(),
            engine: EngineConfig {
                resample_quality: self.resample_quality,
                drift_correction: self.drift_correction,
                max_drift_ppm: self.max_drift_ppm,
                use_non_ascii_channel_names: self.use_non_ascii_channel_names,
                watermark_ms: self.watermark_ms,
            },
        }
    }

    /// 让「时钟主设备」的勾选状态和实际生效的基准一致。
    ///
    /// 引擎在没人显式指定时会自己挑一条(优先第一个输出设备),可界面上什么
    /// 都不显示 —— 用户不知道基准落在谁身上。这里把那个结果落进配置,单选框
    /// 于是就是勾上的。
    ///
    /// 幂等:已经有显式指定就什么都不做。基准是**必须**存在的(没它引擎跑不
    /// 起来),所以"取消勾选"的实际含义是"交回自动挑选",而不是"不要基准"。
    fn sync_clock_master(&mut self) {
        if self.inputs.iter().any(|s| s.clock_master) || self.outputs.iter().any(|s| s.clock_master)
        {
            return;
        }
        // 规矩和 `Config::clock_master()` 保持一致:优先第一个输出设备。
        if let Some(stream) = self.outputs.iter_mut().find(|s| s.opens_device()) {
            stream.clock_master = true;
        } else if let Some(stream) = self.inputs.iter_mut().find(|s| s.opens_device()) {
            stream.clock_master = true;
        }
    }

    fn save_to(&mut self, path: &std::path::Path) {
        let config = self.build_config();
        if let Err(e) = config.validate() {
            self.set_error(format!("配置有误,未保存:{e}"));
            return;
        }
        match write_config(path, &config) {
            Ok(()) => {
                self.config_path = Some(path.to_path_buf());
                self.set_info(format!("已保存到 {}", path.display()));
            }
            Err(e) => self.set_error(format!("保存失败:{e}")),
        }
    }

    fn toggle_runner(&mut self) {
        // 启动/停止还没回来。按钮这时是灰的,这里再兜一道。
        if self.runner_job.is_some() {
            return;
        }

        if let Some(runner) = self.runner.take() {
            // 释放也不能占着 UI 线程:Runner 析构会层层落到 `StreamHost`
            // 的 `join()` 上,而那要等音频线程把每块设备的缓冲都销毁完。
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                drop(runner);
                let _ = tx.send(());
            });
            self.runner_job = Some(RunnerJob::Stopping(rx));
            // 统计是跟着这一次运行走的,别留给下一次。
            self.last_stats.clear();
            self.set_info("正在停止引擎…".into());
            return;
        }

        let config = self.build_config();
        if let Err(e) = config.validate() {
            self.set_error(format!("无法试运行:{e}"));
            return;
        }

        // 打开设备、分配缓冲、启动流都要跟 ASIO 驱动打交道,慢起来好几秒。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // 收端没了(比如窗口已经关了)就让 Runner 就地析构 ——
            // 它的 Drop 会去停引擎。
            let _ = tx.send(Runner::start(config));
        });
        self.runner_job = Some(RunnerJob::Starting(rx));
        // 上一次试运行留下的统计不能给这一次用。
        self.last_stats.clear();
        self.set_info("正在启动引擎…".into());
    }

    /// 收后台线程送回来的启动/停止结果。每帧调一次。
    fn poll_runner_job(&mut self, ctx: &egui::Context) {
        use std::sync::mpsc::TryRecvError;

        // 先把 job 取出来,下面才好改 `self` 的其它字段。
        let Some(job) = self.runner_job.take() else {
            return;
        };
        let mut pending = true;

        match &job {
            RunnerJob::Starting(rx) => match rx.try_recv() {
                Ok(Ok(runner)) => {
                    pending = false;
                    self.runner = Some(runner);
                    self.set_info("试运行中。输出为静音,只统计缓冲状态,不会发出声音。".into());
                }
                Ok(Err(e)) => {
                    pending = false;
                    self.set_error(format!("试运行失败:{e}"));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    // 线程 panic 了才会走到这里。
                    pending = false;
                    self.set_error("启动线程异常退出。".into());
                }
            },
            RunnerJob::Stopping(rx) => match rx.try_recv() {
                // 线程提前退出也别卡在"正在停止"上:设备已经跟着进程走了。
                Ok(()) | Err(TryRecvError::Disconnected) => {
                    pending = false;
                    self.set_info("试运行已停止。".into());
                }
                Err(TryRecvError::Empty) => {}
            },
        }

        if pending {
            self.runner_job = Some(job);
            // 这中间没有输入事件,egui 默认不会重绘,不主动要一帧的话界面
            // 就停在「正在启动…」上了。
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 收后台线程送回来的设备列表。每帧调一次。
    fn poll_devices(&mut self, ctx: &egui::Context) {
        use std::sync::mpsc::TryRecvError;

        let Some(rx) = self.devices_job.as_ref() else {
            return;
        };
        let received = rx.try_recv();

        match received {
            Ok(lists) => {
                self.devices_job = None;
                match lists.inputs {
                    Ok(names) => self.input_devices = names,
                    Err(e) => {
                        self.input_devices.clear();
                        self.set_error(format!("枚举输入设备失败:{e}"));
                    }
                }
                match lists.outputs {
                    Ok(names) => self.output_devices = names,
                    Err(e) => {
                        self.output_devices.clear();
                        self.set_error(format!("枚举输出设备失败:{e}"));
                    }
                }
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                self.devices_job = None;
                self.set_error("枚举设备的线程异常退出。".into());
            }
        }
    }
}

/// 把配置序列化成带注释的 TOML。
///
/// 手写而不是直接 `toml::to_string`,是为了保留解释性注释 ——
/// 用户最终是靠读这个文件来理解多设备配置的。
fn write_config(path: &std::path::Path, config: &Config) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut out = String::new();

    out.push_str("# PigASIO 配置文件 —— 由控制面板生成\n");
    out.push_str("# 每个 [[input]] / [[output]] 对应一块独立设备,通道按书写顺序\n");
    out.push_str("# 拼成 ASIO 的通道列表。\n\n");
    let _ = writeln!(out, "sample_rate = {}", config.sample_rate);
    let _ = writeln!(out, "buffer_size_samples = {}", config.buffer_size_samples);
    let _ = writeln!(out, "asio_sample_type = \"{}\"", config.asio_sample_type);

    out.push_str("\n[engine]\n");
    let quality = match config.engine.resample_quality {
        ResampleQuality::None => "none",
        ResampleQuality::Fast => "fast",
        ResampleQuality::Sinc => "sinc",
    };
    let _ = writeln!(out, "resample_quality = \"{quality}\"");
    let _ = writeln!(out, "drift_correction = {}", config.engine.drift_correction);
    let _ = writeln!(out, "max_drift_ppm = {}", config.engine.max_drift_ppm);
    let _ = writeln!(
        out,
        "use_non_ascii_channel_names = {}",
        config.engine.use_non_ascii_channel_names
    );
    let _ = writeln!(out, "watermark_ms = {}", config.engine.watermark_ms);

    for (label, streams) in [("output", &config.outputs), ("input", &config.inputs)] {
        for (i, s) in streams.iter().enumerate() {
            out.push_str(&format!("\n# {label} #{}\n[[{label}]]\n", i + 1));
            match &s.device {
                DeviceRef::Default => out.push_str("device = \"default\"\n"),
                DeviceRef::None => out.push_str("device = \"none\"\n"),
                DeviceRef::Substring(name) => {
                    let _ = writeln!(out, "device = {}", toml_string(name));
                }
                DeviceRef::Regex(r) => {
                    let _ = writeln!(out, "device_regex = {}", toml_string(r));
                }
            }
            match &s.channels {
                ChannelSelection::Count(n) => {
                    let _ = writeln!(out, "channel_count = {n}");
                }
                ChannelSelection::List(v) => {
                    let list = v
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(out, "channels = [{list}]");
                }
            }
            if s.gain_db != 0.0 {
                let _ = writeln!(out, "gain_db = {}", s.gain_db);
            }
            if let Some(lat) = s.latency_seconds {
                let _ = writeln!(out, "latency = {lat}");
            }
            if s.clock_master {
                out.push_str("clock_master = true\n");
            }
            if s.wasapi.exclusive {
                let _ = writeln!(out, "\n[{label}.wasapi]\nexclusive = true");
            }
        }
    }

    std::fs::write(path, out)
}

/// 把字符串转成合法的 TOML 基本字符串字面量。
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// 界面
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 恢复最大化。只发一次:发完就清掉,否则用户手动取消最大化之后
        // 又会被按回去。
        if self.restore_maximized {
            self.restore_maximized = false;
            // 补一次命令 —— 创建时那个 `with_maximized` 在 Windows 上会被
            // `with_inner_size` 搅乱,得在这里坐实。
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }

        // 标题栏跟着主题走。做成"变了才设"而不是切换时设一次:跟随系统模式
        // 下,用户在 Windows 设置里改了颜色,egui 自己会跟上,标题栏不会。
        let dark = ctx.style().visuals.dark_mode;
        if self.titlebar_dark != Some(dark) {
            self.titlebar_dark = Some(dark);
            apply_titlebar(dark);
        }

        // 没人勾「时钟主设备」时,把引擎实际会用的那条标出来 —— 否则用户从
        // 界面上看不出基准落在谁身上。幂等,放每帧开头最省心。
        self.sync_clock_master();

        // 先收后台线程的结果。
        self.poll_runner_job(ctx);
        self.poll_devices(ctx);

        // 跟着窗口走。拖动/缩放时每帧都在变,所以这里只更新内存,落盘交给
        // `on_exit` —— 没必要为一次拖动写几十遍文件。
        let was_maximized = self.maximized;
        let mut now_maximized = was_maximized;
        ctx.input(|i| {
            let vp = i.viewport();
            if let Some(rect) = vp.outer_rect {
                self.window_pos = Some(rect.min);
            }
            if let Some(size) = vp.inner_rect.map(|rect| rect.size()) {
                self.window_size = Some(size);
            }
            if let Some(maximized) = vp.maximized {
                now_maximized = maximized;
            }
        });
        self.maximized = now_maximized;

        // 刚把窗口从最大化还原回来。因为启动时没给位置和尺寸(给了会先闪
        // 一下小窗口),这会儿得把用户上次摆的样子补回去。
        if was_maximized && !now_maximized {
            if let Some(pos) = self.pending_pos.take() {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
            if let Some(size) = self.pending_size.take() {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
        }

        // 试运行时周期性重绘以刷新统计;平时按需重绘即可。
        if self.runner.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("PigASIO");
                ui.label("多设备 ASIO 驱动");
                // 版本号退到次要位置:它是"想确认时才看一眼"的信息。
                ui.label(egui::RichText::new(VERSION_LABEL).small().weak());
                ui.separator();

                if ui
                    .add_enabled(self.devices_job.is_none(), egui::Button::new("刷新设备"))
                    .clicked()
                {
                    self.refresh_devices();
                }
                if ui.button("重新载入").clicked() {
                    if let Some(p) = self.config_path.clone() {
                        self.load_from(&p);
                    }
                }

                // 主题切换。三个并排的可选项而不是下拉框 —— 当前用的是
                // 哪个一眼就能看出来,而主题这种"随时想换一下"的东西
                // 不值得为它多一次点击。
                ui.separator();
                for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
                    if ui
                        .selectable_label(self.theme_mode == mode, mode.label())
                        .clicked()
                        && self.theme_mode != mode
                    {
                        self.theme_mode = mode;
                        theme::apply(ctx, mode);
                        // 切换即记住。主题是随手改一下的东西,不该还要求用户
                        // 再去点一次「保存」—— 那个按钮存的是引擎配置。
                        update_ui_prefs(|prefs| prefs.theme = Some(mode.as_str().to_string()));
                    }
                }
                if ui.button("保存").clicked() {
                    if let Some(p) = self.config_path.clone() {
                        self.save_to(&p);
                    } else {
                        // 没有路径就落到用户目录,这是驱动默认会查找的位置。
                        if let Some(home) =
                            std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
                        {
                            let p =
                                PathBuf::from(home).join(pigasio_core::config::CONFIG_FILE_NAME);
                            self.save_to(&p);
                        } else {
                            self.set_error("无法确定用户目录,请先用「另存为」指定路径".into());
                        }
                    }
                }

                ui.separator();
                let busy = self.runner_job.is_some();
                let label = if let Some(job) = self.runner_job.as_ref() {
                    job.button_label()
                } else if self.runner.is_some() {
                    "■ 停止试运行"
                } else {
                    "▶ 试运行"
                };
                // 启动/停止途中把按钮禁掉:这时候再点没有意义,而且会让人以为
                // 界面卡死了 —— 其实它只是在等后台线程。
                if ui
                    .add_enabled(!busy, egui::Button::new(label))
                    .on_hover_text("在面板内启动引擎,实时查看各流的缓冲状态;输出是静音的")
                    .clicked()
                {
                    self.toggle_runner();
                }
            });

            ui.horizontal(|ui| {
                ui.label("配置文件:");
                let path_label = match self.config_path.as_ref() {
                    // 宿主目录下的完整路径往往很长,不截断会把这一行撑出
                    // 窗口(egui 的 Label 默认不折行)。中间省略,保留开头
                    // 的盘符和结尾的文件名 —— 这两头才是有信息量的部分。
                    Some(p) => elide_middle(&p.display().to_string(), 72),
                    None => "(未保存 —— 点「保存」写入用户目录)".to_string(),
                };
                ui.label(egui::RichText::new(path_label).weak())
                    .on_hover_text(
                        self.config_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    );
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            if self.message_is_error {
                ui.colored_label(egui::Color32::from_rgb(220, 90, 90), &self.message);
            } else if !self.message.is_empty() {
                ui.label(&self.message);
            } else {
                ui.label(" ");
            }
        });

        // 布局分四层,每一层都有自己独立的滚动区,整个窗口不再整页滚动:
        //
        //   ┌ 工具栏 ────────────────────────────────┐
        //   ├ 引擎设置(固定高度)─────────────────────┤
        //   ├ 输入设备 │ 输出设备(左右两列,各自滚动)──┤
        //   ├ 实时状态(固定高度,可拖拽调整)──────────┤
        //   └ 状态栏 ────────────────────────────────┘
        //
        // 这么分是因为这是个工具面板:设备列表可能很长,但引擎参数和实时
        // 状态是随时要瞄一眼的,不该被列表顶出视野。

        // 引擎设置钉在顶部。
        egui::TopBottomPanel::top("engine_settings_panel")
            .resizable(false)
            .show_separator_line(false)
            .frame(theme::panel_frame(ctx))
            .show(ctx, |ui| {
                // 用 panel_card 而不是 content_frame:卡片宽度必须由面板
                // 决定。里面的两栏 Grid 一旦比列宽宽一点点,`Frame::show`
                // 就会跟着内容把卡片撑到窗口右缘,右边距随即消失。
                theme::panel_card(ui, |ui| {
                    self.draw_engine_settings(ui);
                });
            });

        // 实时状态钉在底部,高度可以拖 —— 试运行时信息量大,值得给它
        // 更多空间;平时又可以压扁让设备列表占满。
        egui::TopBottomPanel::bottom("runner_panel")
            .resizable(true)
            .default_height(180.0)
            .min_height(90.0)
            .max_height(400.0)
            .show_separator_line(false)
            .frame(theme::panel_frame(ctx))
            .show(ctx, |ui| {
                theme::panel_card(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("runner_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.draw_runner_status(ui);
                        });
                });
            });

        // 中间留给设备,左右各占一半。两边各有自己的滚动条,
        // 所以加设备加到几十个也不会把别的区域挤走。
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    // 铺上窗口底色。
                    //
                    // 千万别写 `Color32::TRANSPARENT` —— 这层窗口底色本来
                    // 就是这个 CentralPanel 画的,置成透明等于"不画",底下
                    // 没有任何层接手,glow 就用黑色 clear 那块区域,于是
                    // 内边距变成一圈纯黑的框。这正是之前"黑边框"的真凶:
                    // 它不是描边,是**没有背景色的空隙**。
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                let total = ui.available_width();
                let gap = ui.spacing().item_spacing.x;
                let column = ((total - gap) / 2.0).max(220.0);

                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(column, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            theme::content_frame(ui).show(ui, |ui| {
                                // 撑满这一栏并限住上界。
                                //
                                // `Frame::show` 的宽度**由内容决定**:不撑的话
                                // 卡片会比这一栏窄,和上下的引擎设置区对不齐;
                                // 不封顶的话,某个控件比这栏还宽时会把整栏反过
                                // 来撑宽 —— egui 的容器都是"内容决定尺寸"。
                                // 这一栏的内容都做了 wrap,不会溢出。
                                let w = ui.available_width();
                                ui.set_min_width(w);
                                ui.set_max_width(w);
                                egui::ScrollArea::vertical()
                                    .id_salt("inputs_scroll")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        self.draw_streams(ui, StreamKind::Input);
                                    });
                            });
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(column, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            theme::content_frame(ui).show(ui, |ui| {
                                // 撑满这一栏并限住上界。
                                //
                                // `Frame::show` 的宽度**由内容决定**:不撑的话
                                // 卡片会比这一栏窄,和上下的引擎设置区对不齐;
                                // 不封顶的话,某个控件比这栏还宽时会把整栏反过
                                // 来撑宽 —— egui 的容器都是"内容决定尺寸"。
                                // 这一栏的内容都做了 wrap,不会溢出。
                                let w = ui.available_width();
                                ui.set_min_width(w);
                                ui.set_max_width(w);
                                egui::ScrollArea::vertical()
                                    .id_salt("outputs_scroll")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        self.draw_streams(ui, StreamKind::Output);
                                    });
                            });
                        },
                    );
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 窗口位置/大小只在这里落盘 —— 拖动缩放时它们每帧都在变,跟着写
        // 文件没必要。
        let (pos, size, maximized) = (self.window_pos, self.window_size, self.maximized);
        update_ui_prefs(|prefs| {
            // 最大化时量到的位置和尺寸都是"铺满屏幕"的那一套(位置往往还带
            // 负偏移),不是用户摆放的样子。存下去的话,下次一取消最大化就
            // 只能看到一个占满屏幕、还偏到屏幕外的窗口。
            if !maximized {
                if let Some(pos) = pos {
                    prefs.window_pos = Some([pos.x, pos.y]);
                }
                if let Some(size) = size {
                    prefs.window_size = Some([size.x, size.y]);
                }
            }
            prefs.maximized = Some(maximized);
        });
    }
}

impl App {
    fn draw_engine_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("引擎设置");
        let sample_rate = self.sample_rate;

        // 两栏并排。
        //
        // 这些参数是"设一次就不动"的东西,竖着堆 7 行要把 160px 的垂直
        // 空间吃掉 —— 而那正是下面设备列表最需要的。分两栏后压到 4 行。
        //
        // 这里敢用 `ui.columns`:它收尾时会按「**最宽**的那一列 × 列数」
        // 重算总宽 —— 子控件溢出多少,它就把父 `Ui` 撑宽多少,卡片会被
        // 一路顶到窗口右缘。早先左栏那 5 个带毫秒的按钮就踩过这个坑。
        // 现在两栏用的都是宽度可控的下拉框,而且卡片宽度已经由
        // `theme::panel_card` 锁死,真溢出了也撑不大。
        ui.columns(2, |cols| {
            self.engine_settings_left(&mut cols[0], sample_rate);
            self.engine_settings_right(&mut cols[1]);
        });

        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "采样率和缓冲区大小由所有设备共用。设备不支持该采样率时,引擎会自动\
                 重采样,所以不同声卡可以混用。",
            )
            .small()
            .weak(),
        );
        let _ = self.asio_sample_type;
    }

    /// 引擎设置左栏:采样率、缓冲区、重采样质量。
    fn engine_settings_left(&mut self, ui: &mut egui::Ui, sample_rate: u32) {
        /// 下拉框宽度。三行取同一个值才对齐。
        ///
        /// `ComboBox::width` 是**最小**宽度:文字比它窄就按这个值,所以只要
        /// 它不小于最长的那项("none(不重采样)"、"2048 (43ms)"),三个框就
        /// 一样宽。
        const COMBO: f32 = 190.0;

        egui::Grid::new("engine_settings_left")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("采样率");
                egui::ComboBox::from_id_salt("sample_rate")
                    .selected_text(self.sample_rate.to_string())
                    .width(COMBO)
                    .show_ui(ui, |ui| {
                        for rate in [44_100u32, 48_000, 88_200, 96_000] {
                            ui.selectable_value(&mut self.sample_rate, rate, rate.to_string());
                        }
                    });
                ui.end_row();

                ui.label("缓冲区");
                egui::ComboBox::from_id_salt("buffer_size")
                    .selected_text(buffer_label(self.buffer_size, sample_rate))
                    .width(COMBO)
                    .show_ui(ui, |ui| {
                        for size in BUFFER_SIZES {
                            ui.selectable_value(
                                &mut self.buffer_size,
                                size,
                                buffer_label(size, sample_rate),
                            );
                        }
                    });
                ui.end_row();

                ui.label("重采样质量");
                egui::ComboBox::from_id_salt("resample_quality")
                    .selected_text(resample_label(self.resample_quality))
                    .width(COMBO)
                    .show_ui(ui, |ui| {
                        for quality in [
                            ResampleQuality::Sinc,
                            ResampleQuality::Fast,
                            ResampleQuality::None,
                        ] {
                            ui.selectable_value(
                                &mut self.resample_quality,
                                quality,
                                resample_label(quality),
                            )
                            .on_hover_text(match quality {
                                ResampleQuality::Sinc => "窗化 sinc 插值,音质和开销平衡得最好。",
                                ResampleQuality::Fast => "多项式插值,省 CPU,音质一般。",
                                ResampleQuality::None => {
                                    "要求每块设备都直接支持引擎采样率,并且关掉时钟\
                                     漂移补偿;否则两块声卡的晶振差会周期性地爆音。\
                                     换来的是最低延迟。"
                                }
                            });
                        }
                    });
                ui.end_row();
            });
    }

    /// 引擎设置右栏:时钟、通道名、漂移补偿。
    fn engine_settings_right(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("engine_settings_right")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("时钟漂移补偿");
                ui.checkbox(&mut self.drift_correction, "启用");
                ui.end_row();

                ui.label("通道名带设备名");
                ui.checkbox(&mut self.use_non_ascii_channel_names, "允许中文")
                    .on_hover_text(
                        "ASIO 的通道名是 char[32],协议没规定编码,各家宿主的解释方式\
                             不一样。勾选后按 UTF-8 写入 —— Cantabile、REAPER 这类现代宿主\
                             都认,能显示中文设备名。要是碰上只认老式 ANSI 代码页的宿主,\
                             中文会变乱码,那种情况取消勾选,通道名就退化成 OUT 1 (dev2)\
                             这样的纯 ASCII 形式。",
                    );
                ui.end_row();

                ui.label("最大漂移补偿");
                theme::slider(
                    ui,
                    egui::Slider::new(&mut self.max_drift_ppm, 10.0..=5000.0)
                        .suffix(" ppm")
                        .logarithmic(true),
                )
                .on_hover_text(
                    "稳态下允许的重采样比率偏移。设得太小可能压不住两块声卡之间的\
                         晶振差异。",
                );
                ui.end_row();

                ui.label("缓冲目标水位");
                // 闭包里要可变借用 `self.watermark_ms`,所以采样率得先拷出来。
                let sample_rate = self.sample_rate.max(1) as f64;
                theme::slider(
                    ui,
                    egui::Slider::new(&mut self.watermark_ms, 1.0..=200.0)
                        .logarithmic(true)
                        .custom_formatter(move |v, _| {
                            format!("{v:.0} ms ≈ {:.0} 帧", v / 1000.0 * sample_rate)
                        }),
                )
                .on_hover_text(
                    "每个流的环形缓冲要维持多少数据,**单位是时间**。这一项直接加在\
                         延迟上:水位有多少毫秒,音频路径上就多出多少等待。\n\n\
                         填的是绝对时间,所以调小缓冲区不会连带把它缩掉 —— 调小缓冲区\
                         就是纯粹地降延迟。\n\n\
                         多厚才够?至少要盖过一块设备的回调周期。WASAPI 共享模式下\
                         设备普遍 10 ms 一块,所以低于 10 ms 基本一定会欠载;30 ms 是个\
                         稳妥的起点。",
                );
                ui.end_row();
            });

        // 拿实测到的设备块跟水位比一比。这是唯一能在**跑起来之后**发现「水位比
        // 设备回调周期还短」的地方 —— 这种配置必然欠载,而且原因很不直观:水位
        // 用自己的数字看着挺正常,实际却薄于设备一次回调的间隔。
        //
        // 没运行过就没有实测值,那就只留上面滑动条提示里的经验值(10 ms 一块)。
        // 放在 Grid 外面是为了让它按栏宽折行,不至于把表格撑宽。
        let block_frames = self
            .last_stats
            .iter()
            .map(|s| s.device_frames)
            .max()
            .unwrap_or(0);
        if block_frames > 0 {
            let sample_rate = self.sample_rate.max(1) as f64;
            let block_ms = block_frames as f64 / sample_rate * 1000.0;
            ui.add_space(6.0);
            if self.watermark_ms < block_ms {
                let label = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "⚠ 水位 {:.0} ms 比实测设备块 {block_ms:.0} ms({block_frames} 帧)\
                             还薄 —— 余量不足一块设备缓冲,随时可能欠载,建议至少调到 {:.0} ms",
                            self.watermark_ms,
                            (block_ms * 1.5).ceil(),
                        ))
                        .color(egui::Color32::from_rgb(220, 160, 60)),
                    )
                    .wrap(),
                );
                label.on_hover_text(
                    "设备块是每块声卡回调一次实际送来(或取走)的帧数,由系统音频引擎\
                     决定,和 ASIO 缓冲区大小无关。两块设备回调之间环形缓冲没有新数据\
                     补充,所以水位得比这个时间长,否则宿主来取数据时缓冲里是空的。",
                );
            } else {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "实测最大设备块 {block_ms:.0} ms({block_frames} 帧),\
                             水位比它厚 {:.1} 倍",
                            self.watermark_ms / block_ms
                        ))
                        .weak(),
                    )
                    .wrap(),
                );
            }
        }
    }

    fn draw_streams(&mut self, ui: &mut egui::Ui, kind: StreamKind) {
        let is_input = kind == StreamKind::Input;
        let device_names = if is_input {
            self.input_devices.clone()
        } else {
            self.output_devices.clone()
        };
        let streams = if is_input {
            &mut self.inputs
        } else {
            &mut self.outputs
        };
        let mut remove: Option<usize> = None;

        ui.horizontal(|ui| {
            ui.heading(format!("{}设备", kind.as_str()));
            ui.label(egui::RichText::new(format!("共 {} 路", streams.len())).weak());
            if ui.button("+ 添加").clicked() {
                streams.push(StreamEdit {
                    clock_master: false,
                    ..StreamEdit::default()
                });
            }
        });

        // 逐条累加,算出每条流占用的 ASIO 通道区间。这是理解多设备
        // 通道映射最直观的方式,所以直接标在界面上。
        let mut asio_offset = 0usize;
        // 「时钟主设备」是单选语义,但选中时不能立刻去改别的条目 ——
        // 那会在遍历 `streams` 的过程中再次可变借用它。所以先记下来,
        // 等循环结束再统一处理。
        let mut new_clock_master: Option<usize> = None;

        for (i, stream) in streams.iter_mut().enumerate() {
            let channel_count = match stream.channel_mode {
                ChannelMode::Count => stream.channel_count.max(1),
                ChannelMode::List => stream
                    .channels_text
                    .split(',')
                    .filter_map(|s| s.trim().parse::<usize>().ok())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    .max(1),
            };
            let start = asio_offset + 1;
            let end = asio_offset + channel_count;
            let range_text = if channel_count == 1 {
                format!("{start}")
            } else {
                format!("{start}–{end}")
            };
            asio_offset += channel_count;

            theme::card_frame(ui, true).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("#{}", i + 1));

                    let current = if stream.use_default_device {
                        "(系统默认设备)".to_string()
                    } else {
                        stream.device.clone()
                    };
                    // 先把"删除"按钮从右边占掉,剩下的宽度才归下拉框 ——
                    // 反过来的话,下拉框会吃掉整行,把按钮挤到重叠。
                    let button_w = 46.0;
                    let combo_width = (ui.available_width() - button_w - 12.0).max(110.0);
                    let combo = egui::ComboBox::from_id_salt((is_input, i))
                        // 交给 egui 按**实际宽度**截断。早先这里是自己按字符数
                        // 截的(`elide(&current, 22)`),而一个汉字顶两个字符宽,
                        // 设备名于是老早被切掉、右边却还空着一大截。
                        .selected_text(current.as_str())
                        .truncate()
                        .width(combo_width)
                        .show_ui(ui, |ui| {
                            ui.set_min_width(combo_width);
                            if ui
                                .selectable_label(stream.use_default_device, "(系统默认设备)")
                                .clicked()
                            {
                                stream.use_default_device = true;
                            }
                            for name in &device_names {
                                let selected = !stream.use_default_device && &stream.device == name;
                                if ui.selectable_label(selected, name).clicked() {
                                    stream.device = name.clone();
                                    stream.use_default_device = false;
                                }
                            }
                        });
                    let _ = combo;

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("删除").clicked() {
                            remove = Some(i);
                        }
                    });
                });

                // ASIO 通道映射单独占一行。放在下拉框右边时会被挤成
                // 一两个字,不如让它自己一行说清楚。
                ui.label(
                    egui::RichText::new(format!("→ ASIO {} 通道 {range_text}", kind.as_str()))
                        .small()
                        .weak(),
                );

                // 用 horizontal_wrapped:分栏之后每列只有半屏宽,
                // 通道/增益/时钟主设备这一行放不下时会自动折到下一行。
                ui.horizontal_wrapped(|ui| {
                    ui.label("通道");
                    ui.selectable_value(&mut stream.channel_mode, ChannelMode::Count, "前 N 个");
                    ui.selectable_value(&mut stream.channel_mode, ChannelMode::List, "指定");
                    match stream.channel_mode {
                        ChannelMode::Count => {
                            ui.add(egui::DragValue::new(&mut stream.channel_count).range(1..=32));
                            ui.label("个");
                        }
                        ChannelMode::List => {
                            ui.add(
                                egui::TextEdit::singleline(&mut stream.channels_text)
                                    .desired_width(110.0)
                                    .hint_text("0, 1"),
                            );
                        }
                    }

                    ui.separator();
                    ui.label("增益");
                    ui.add(
                        egui::DragValue::new(&mut stream.gain_db)
                            .range(-40.0..=40.0)
                            .speed(0.1)
                            .suffix(" dB"),
                    );

                    ui.separator();
                    let mut master = stream.clock_master;
                    if ui
                        .checkbox(&mut master, "时钟主设备")
                        .on_hover_text(
                            "多设备之间必须有一个时间基准,其余设备通过重采样跟随它。\
                             整个配置里只能有一个。",
                        )
                        .changed()
                    {
                        stream.clock_master = master;
                        if master {
                            new_clock_master = Some(i);
                        }
                    }
                });
            });
            ui.add_space(4.0);
        }

        // 单选语义:选中了新的主设备就把其他的清掉。放在循环外做,
        // 避免在遍历 `streams` 时再次可变借用它。
        if let Some(chosen) = new_clock_master {
            for (j, other) in streams.iter_mut().enumerate() {
                if j != chosen {
                    other.clock_master = false;
                }
            }
        }

        if let Some(i) = remove {
            streams.remove(i);
        }
        if streams.is_empty() {
            ui.label(
                egui::RichText::new(format!(
                    "没有{}设备。宿主将看不到任何{}通道。",
                    kind.as_str(),
                    kind.as_str()
                ))
                .weak(),
            );
        }
    }

    fn draw_runner_status(&mut self, ui: &mut egui::Ui) {
        let Some(runner) = self.runner.as_ref() else {
            ui.heading("实时状态");
            if let Some(job) = self.runner_job.as_ref() {
                ui.label(job.hint());
            } else {
                ui.label(
                    egui::RichText::new(
                        "点上面的「试运行」启动引擎,就能在这里看到各流的缓冲水位和\
                         漂移补偿量。试运行不会发出声音,适合在打开 DAW 之前先确认\
                         多设备是否同步。",
                    )
                    .weak(),
                );
            }
            return;
        };

        let status = runner.engine.status();
        let peak = runner.peak_level();
        let elapsed = runner.elapsed();

        // 这次没读到就沿用上一次的,别让表格忽高忽低(见 `last_stats`)。
        if !status.stream_stats.is_empty() {
            self.last_stats = status.stream_stats.clone();
        }
        let stats: &[StreamStatusSnapshot] = if status.stream_stats.is_empty() {
            &self.last_stats
        } else {
            &status.stream_stats
        };

        ui.horizontal(|ui| {
            ui.heading("实时状态");
            ui.label(format!("已运行 {elapsed:.1} 秒"));
            ui.separator();
            ui.label(format!(
                "{} Hz · {} 帧 · {} 路输入 / {} 路输出",
                status.sample_rate,
                status.buffer_size,
                status.input_channels,
                status.output_channels
            ));
            // 引擎级的丢帧:不属于哪条流,所以不放进下面那张表,单列在这儿。
            // 正常恒为 0,一旦出现就说明时钟推进的一轮里没处理完。
            if status.dropped_frames > 0 {
                ui.separator();
                ui.colored_label(
                    egui::Color32::from_rgb(220, 160, 60),
                    format!("引擎丢帧 {} 帧", status.dropped_frames),
                )
                .on_hover_text(
                    "这不是某条流的欠载,而是引擎自己没跟上:时钟推进一轮要处理的\
                     缓冲数超过了上限,多余的累计计数只能丢掉。逐流的欠载/溢出\
                     量的是每条流自己的环形缓冲,这个量的是时钟推进本身。\n\n\
                     出现它通常意味着 CPU 吃紧 —— 试试调大「缓冲区」,让每秒的\
                     回调次数少一些。",
                );
            }
        });

        if status.input_channels > 0 {
            // 数值放在条**外面**。
            //
            // egui 给条内文字取的颜色是 `selection.stroke.color`,而本主题按
            // Fluent 的规则把它设成了"强调色上的反色" —— 浅色主题里就是白。
            // 文字又是左对齐的,进度小的时候整段都落在**未填充**的白底上,
            // 于是白字白底,什么都看不见;深色主题同理(黑字压深底)。放外面
            // 就和填充多少无关了,进度再小也读得出数。
            ui.horizontal(|ui| {
                theme::progress_bar(ui, peak.clamp(0.0, 1.0), 260.0);
                ui.label(format!("输入峰值 {peak:.3}"));
            });
        }

        ui.add_space(4.0);
        egui::Grid::new("runner_streams")
            .num_columns(7)
            .striped(true)
            .spacing([14.0, 4.0])
            .show(ui, |ui| {
                ui.strong("方向");
                ui.strong("设备");
                ui.strong("水位(帧)");
                // 设备侧的真实块大小,逐流列出 —— 汇总成一个数看不出是谁慢。
                ui.strong("块(帧)").on_hover_text(
                    "这块声卡的回调实际每块送来多少帧。它是量出来的,由系统\
                         音频引擎决定,和我们报给 ASIO 宿主的「缓冲区」不是一回事。\
                         整条链路的水位得能装下最大的一块,所以这个数最大的那条\
                         设备决定了延迟下限。",
                );
                ui.strong("漂移(ppm)");
                // 累计写进 ring / 读走的帧数。存量(水位)只能说明"满了",
                // 这两个才能指出是生产太快还是消费太慢 —— 盯着看几秒,跑在前
                // 面的那一侧就是问题所在。
                ui.strong("写/读(k帧)").on_hover_text(
                    "累计写进环形缓冲、和从环形缓冲读走的帧数(单位千帧)。\
                         正常时两者同步增长;若「写」持续跑在前面,说明生产快于消费,\
                         缓冲会一路积到容量上限。\n\n\
                         两侧分别由持有那一半端的线程记数:输入方向是设备写、ASIO 读,\
                         输出方向反过来。",
                );
                ui.strong("状态");
                ui.end_row();

                for s in stats {
                    let snap = &s.stats;
                    ui.label(s.kind.as_str());
                    // 标出时钟基准:它不做重采样,其余流都跟着它走。
                    ui.horizontal(|ui| {
                        ui.label(elide(&s.device_name, 34));
                        if s.is_clock_master {
                            ui.label(egui::RichText::new("· 主时钟").small().weak());
                        }
                    });
                    ui.label(snap.queued_frames.to_string());
                    if s.device_frames > 0 {
                        ui.label(s.device_frames.to_string());
                    } else {
                        ui.label("-");
                    }
                    ui.label(format!("{:+.0}", snap.drift_ppm));
                    ui.label(format!(
                        "{:.0}k/{:.0}k",
                        snap.written_frames as f64 / 1000.0,
                        snap.read_frames as f64 / 1000.0
                    ));
                    if snap.is_healthy() {
                        ui.colored_label(egui::Color32::from_rgb(90, 180, 90), "正常");
                    } else {
                        // 欠载几乎总是水位不够厚,而水位现在完全由用户说了算,
                        // 所以直接把调整方向写在这儿 —— 不然只能对着数字猜。
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 60),
                            format!(
                                "欠载 {} / 溢出 {}",
                                snap.underflow_frames, snap.overflow_frames
                            ),
                        )
                        .on_hover_text(
                            "欠载是环形缓冲被读空了 —— 水位不够厚。可以试着把\
                                 「缓冲目标水位」调大;溢出的方向相反,那是水位偏大\
                                 或设备时钟偏慢。",
                        );
                    }
                    ui.end_row();
                }
            });

        if stats.is_empty() {
            ui.label(egui::RichText::new("暂时读不到统计(音频回调正忙),稍后会自动刷新。").weak());
        }
    }
}

/// 缓冲区大小可选值,单位采样帧。
///
/// 只放 2 的幂 —— ASIO 宿主普遍偏好这个,有些会直接拒绝别的值(引擎在配置
/// 校验里对非 2 的幂也只会 warn)。
///
/// 下到 16:延迟虽然由水位主导(见 `watermark_ms`),但缓冲区仍是唯一能压到
/// 亚毫秒的那一项,而且实测 16 帧在本机跑得稳。往上到 2048 就够覆盖"宿主里
/// 挂了一堆插件、回调来不及跑"的场景了,再大纯属徒增延迟。
/// 注意宿主会按 `getBufferSize()` 报的值来建缓冲,而驱动只报配置里这一个值,
/// 所以这份列表就是宿主能用的全部选项。
const BUFFER_SIZES: [u32; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

/// 缓冲区选项的文字:大小 + 它在当前采样率下的延迟。
///
/// 延迟才是选缓冲区时真正要权衡的东西,所以直接写进选项里。必须带一位小数 ——
/// 16 和 64 帧都取整到 0 位的话会一起显示成 "0ms"/"1ms",反而看不出区别。
fn buffer_label(size: u32, sample_rate: u32) -> String {
    let latency = size as f32 / sample_rate.max(1) as f32 * 1000.0;
    format!("{size} ({latency:.1}ms)")
}

/// 重采样质量的显示文字。
fn resample_label(quality: ResampleQuality) -> &'static str {
    match quality {
        ResampleQuality::Sinc => "sinc(最好)",
        ResampleQuality::Fast => "fast(省 CPU)",
        ResampleQuality::None => "none(不重采样)",
    }
}

/// 把过长的设备名截短,给界面省点横向空间。
fn elide(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// 从中间省略过长的路径,保留头尾两端。
///
/// 路径的信息量集中在盘符和文件名上,中间的目录层级通常不重要。
/// 中文界面下这一点尤其明显:一条 `E:\Projects\...\examples\多设备输出示例.toml`
/// 按字符数算并不长,但每个汉字都占两个字宽,从尾部截断会把最有用的
/// 文件名切掉。
fn elide_middle(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    // 留一个字符给省略号。
    let keep = max_chars.saturating_sub(1) / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s.chars().skip(total - keep).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 界面偏好往返保持各项() {
        let prefs = UiPrefs {
            theme: Some("dark".to_string()),
            window_pos: Some([120.0, 64.0]),
            window_size: Some([900.0, 600.0]),
            maximized: Some(true),
        };
        let text = toml::to_string(&prefs).expect("序列化界面偏好");
        let back: UiPrefs = toml::from_str(&text).expect("读回界面偏好");
        assert_eq!(back.theme.as_deref(), Some("dark"));
        assert_eq!(back.window_pos, Some([120.0, 64.0]));
        assert_eq!(back.window_size, Some([900.0, 600.0]));
        assert_eq!(back.maximized, Some(true));
    }

    /// 老版本没写过这个文件,或者文件被清空了 —— 两种都得能读。
    /// 字段是逐个加进来的,少几个也要照读不误。
    #[test]
    fn 界面偏好缺字段时退回默认() {
        let prefs: UiPrefs = toml::from_str("").expect("空文件也要能读");
        assert!(prefs.theme.is_none());
        assert!(prefs.window_pos.is_none());
        assert!(prefs.window_size.is_none());
        assert!(prefs.maximized.is_none());

        // 只写了主题的第一版文件。
        let v1: UiPrefs = toml::from_str("theme = \"light\"").expect("旧文件也要能读");
        assert_eq!(v1.theme.as_deref(), Some("light"));
        assert!(v1.window_pos.is_none());
        assert!(v1.window_size.is_none());

        // 加了窗口位置、还没有大小和最大化的第二版。
        let v2: UiPrefs =
            toml::from_str("theme = \"dark\"\nwindow_pos = [10.0, 20.0]").expect("旧文件也要能读");
        assert_eq!(v2.window_pos, Some([10.0, 20.0]));
        assert!(v2.window_size.is_none());
        assert!(v2.maximized.is_none());
    }

    #[test]
    fn 字符串转义为合法_toml() {
        assert_eq!(toml_string("扬声器 (Realtek)"), "\"扬声器 (Realtek)\"");
        assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string("a\\b"), "\"a\\\\b\"");
        // 控制字符必须转义,否则 TOML 解析会失败。
        assert_eq!(toml_string("a\u{1}b"), "\"a\\u0001b\"");
    }

    #[test]
    fn 通道文本解析容忍空格与重复() {
        let edit = StreamEdit {
            channel_mode: ChannelMode::List,
            channels_text: " 3, 1 , 3 ,2 ".into(),
            ..StreamEdit::default()
        };
        assert_eq!(edit.to_config().channels.expand(), vec![1, 2, 3]);
    }

    #[test]
    fn 非法通道文本退化为单通道() {
        let edit = StreamEdit {
            channel_mode: ChannelMode::List,
            channels_text: "abc".into(),
            ..StreamEdit::default()
        };
        assert_eq!(edit.to_config().channels.expand(), vec![0]);
    }

    #[test]
    fn 配置往返保持设备与通道() {
        let edit = StreamEdit {
            device: "Speakers (USB)".into(),
            use_default_device: false,
            channel_mode: ChannelMode::List,
            channels_text: "1, 0".into(),
            gain_db: 3.5,
            latency_ms: Some(20.0),
            wasapi_exclusive: false,
            clock_master: true,
            ..StreamEdit::default()
        };
        let cfg = edit.to_config();
        assert_eq!(cfg.device, DeviceRef::Substring("Speakers (USB)".into()));
        assert_eq!(cfg.channels.expand(), vec![0, 1]);
        assert_eq!(cfg.gain_db, 3.5);
        assert!((cfg.latency_seconds.unwrap() - 0.02).abs() < 1e-9);
        assert!(cfg.clock_master);

        let back = StreamEdit::from_config(&cfg);
        assert_eq!(back.device, "Speakers (USB)");
        assert_eq!(back.channels_text, "0, 1");
        assert!(back.clock_master);
    }

    #[test]
    fn 默认设备条目往返正确() {
        let edit = StreamEdit {
            use_default_device: true,
            ..StreamEdit::default()
        };
        let cfg = edit.to_config();
        assert_eq!(cfg.device, DeviceRef::Default);
        assert!(StreamEdit::from_config(&cfg).use_default_device);
    }

    #[test]
    fn 长名字会被省略() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn 缓冲区选项带上当前采样率下的延迟() {
        // 48000Hz 下 128 帧约 2.7ms。保留一位小数,否则 16/64/128 会挤成
        // 同一个整数,小缓冲区之间反而分不出来。
        assert_eq!(buffer_label(128, 48_000), "128 (2.7ms)");
        assert_eq!(buffer_label(16, 48_000), "16 (0.3ms)");
        assert_eq!(buffer_label(64, 48_000), "64 (1.3ms)");
        assert_eq!(buffer_label(2048, 48_000), "2048 (42.7ms)");
        // 采样率损坏成 0 时不能算出 NaN/Inf(配置里不该出现,兜个底)。
        assert!(buffer_label(256, 0).starts_with("256 ("));
    }

    #[test]
    fn 缓冲区选项全是二的幂且升序() {
        // 引擎对非 2 的幂只会 warn,宿主却可能直接拒绝加载 —— 界面不该给出
        // 这种选项。升序则是下拉框可读性的前提。
        for (i, pair) in BUFFER_SIZES.windows(2).enumerate() {
            assert!(pair[0] < pair[1], "第 {i} 项没有升序:{pair:?}");
            assert!(
                pair[0].is_power_of_two(),
                "{} 不是 2 的幂,部分宿主会拒绝",
                pair[0]
            );
        }
        assert!(BUFFER_SIZES[BUFFER_SIZES.len() - 1].is_power_of_two());
        // 小缓冲区是延迟的关键选项,别在重构里被删掉。
        assert!(BUFFER_SIZES.contains(&32), "缺少 32 帧选项");
        assert!(BUFFER_SIZES.contains(&64), "缺少 64 帧选项");
    }

    #[test]
    fn 重采样质量的文字三个变体都有() {
        assert_eq!(resample_label(ResampleQuality::Sinc), "sinc(最好)");
        assert_eq!(resample_label(ResampleQuality::Fast), "fast(省 CPU)");
        assert_eq!(resample_label(ResampleQuality::None), "none(不重采样)");
    }

    #[test]
    fn 长路径从中间省略并保留文件名() {
        let path = r"E:\Projects\Other\PigASIO\examples\多设备输出示例.toml";
        let short = elide_middle(path, 30);
        assert!(short.chars().count() <= 30);
        assert!(short.contains('…'));
        // 文件名必须留下来 —— 那才是用户真正关心的部分。
        assert!(short.ends_with("多设备输出示例.toml"), "实际:{short}");
        // 短路径原样返回。
        assert_eq!(elide_middle(r"C:\a.toml", 30), r"C:\a.toml");
    }

    #[test]
    fn 写出的配置能被解析回来() {
        let dir = std::env::temp_dir().join("pigasio-gui-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.toml");

        let config = Config {
            sample_rate: 48_000,
            buffer_size_samples: 512,
            asio_sample_type: AsioSampleType::Float32,
            inputs: vec![StreamConfig {
                device: DeviceRef::Substring("麦克风 (USB)".into()),
                channels: ChannelSelection::List(vec![0, 1]),
                ..StreamConfig::default()
            }],
            outputs: vec![
                StreamConfig {
                    device: DeviceRef::Default,
                    channels: ChannelSelection::Count(2),
                    ..StreamConfig::default()
                },
                StreamConfig {
                    device: DeviceRef::Substring("S/PDIF".into()),
                    channels: ChannelSelection::Count(2),
                    gain_db: -3.0,
                    clock_master: true,
                    ..StreamConfig::default()
                },
            ],
            engine: EngineConfig::default(),
        };

        write_config(&path, &config).unwrap();
        let parsed = Config::from_file(&path).unwrap();

        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.buffer_size_samples, 512);
        assert_eq!(parsed.inputs.len(), 1);
        assert_eq!(parsed.outputs.len(), 2);
        assert_eq!(parsed.total_input_channels(), 2);
        assert_eq!(parsed.total_output_channels(), 4);
        assert_eq!(parsed.outputs[1].gain_db, -3.0);
        assert!(parsed.outputs[1].clock_master);
        assert_eq!(parsed.clock_master(), Some((StreamKind::Output, 1)));

        let _ = std::fs::remove_file(&path);
    }
}
