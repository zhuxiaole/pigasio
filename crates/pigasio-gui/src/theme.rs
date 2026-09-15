//! Fluent 风格主题。
//!
//! egui 默认的观感是"深灰蓝 + 方角 + 紧凑",和 Windows 11 的设计语言
//! 差得比较远。这个模块把它调成 Fluent 的样子:
//!
//! * **圆角**:控件 4px、卡片 8px(Fluent 的 corners 令牌)
//! * **分层配色**:窗口底色 / 卡片 / 输入框三层,靠明度而不是边框来分层
//! * **系统强调色**:直接从注册表读用户在"个性化"里选的强调色
//! * **跟随系统深色模式**
//! * **Segoe UI 字体**,而不是 egui 自带的 Ubuntu-Light
//!
//! # 做不到的部分
//!
//! Fluent 最具标志性的 **Mica 材质**(窗口背景取样桌面壁纸并模糊)需要
//! `DwmSetWindowAttribute` 配合透明窗口。egui 的 glow 渲染器默认不透明,
//! 走这条路要改动窗口创建流程,风险大于收益,所以这里只是用纯色近似 ——
//! 浅色用 `#F3F3F3`、深色用 `#202020`,这正是 Mica 在纯色桌面上的
//! 平均结果。

use eframe::egui::{self, Color32, Rounding, Stroke};

/// 控件圆角。Fluent 的 `cornerRadius.small`。
const CORNER_CONTROL: f32 = 4.0;
/// 卡片/窗口圆角。Fluent 的 `cornerRadius.large`。
const CORNER_CARD: f32 = 8.0;
/// 内容卡片的内边距。
///
/// [`content_frame`] 和 [`panel_card`] 共用 —— 后者要手工算卡片矩形,
/// 这个值必须和 frame 的 `inner_margin` 保持一致,否则卡片会歪。
const CARD_PADDING: f32 = 10.0;

/// 主题模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// 跟随 Windows 的"应用模式"设置。
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    /// 映射到 egui 的主题偏好。
    ///
    /// `System` 直接交给 egui —— 它每帧从 winit 拿系统主题,比自己读注册表
    /// 更及时(用户在"设置"里改了颜色,不用重开窗口就能跟上)。
    pub fn to_preference(self) -> egui::ThemePreference {
        match self {
            ThemeMode::System => egui::ThemePreference::System,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::Dark => egui::ThemePreference::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "跟随系统",
            ThemeMode::Light => "浅色",
            ThemeMode::Dark => "深色",
        }
    }
}

// ---------------------------------------------------------------------------
// 调色板
// ---------------------------------------------------------------------------

/// Fluent 2 的浅色令牌。数值取自 Windows 11 浅色主题的实际取值。
mod light {
    use eframe::egui::Color32;

    /// 窗口底色(Mica 的纯色近似)。
    pub const LAYER: Color32 = Color32::from_rgb(0xF3, 0xF3, 0xF3);
    /// 卡片/浮层。
    pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
    /// 输入框等需要"凹陷"感的控件。
    pub const CONTROL: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
    /// 按钮的静息底色。
    pub const BUTTON: Color32 = Color32::from_rgb(0xFD, 0xFD, 0xFD);
    /// 悬停。
    pub const HOVER: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF5);
    /// 按下。
    pub const PRESSED: Color32 = Color32::from_rgb(0xEC, 0xEC, 0xEC);
    /// 分隔线与控件描边。
    pub const STROKE: Color32 = Color32::from_rgb(0xE0, 0xE0, 0xE0);
    /// 滑条底槽。
    ///
    /// 它和输入框共用 `widgets.inactive.bg_fill`,但两者要的东西不一样:
    /// 输入框是"凹进去一点点"(几乎和白卡片同色就行),底槽得**看得出来**。
    /// 见 [`slider`]。
    pub const RAIL: Color32 = Color32::from_rgb(0xC5, 0xC5, 0xC5);
    /// 主要文字。
    pub const TEXT: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
    /// 次要文字(说明、提示)。
    pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x61, 0x61, 0x61);
}

/// Fluent 2 的深色令牌。
mod dark {
    use eframe::egui::Color32;

    pub const LAYER: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
    pub const CARD: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2B);
    pub const CONTROL: Color32 = Color32::from_rgb(0x2D, 0x2D, 0x2D);
    pub const BUTTON: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);
    pub const HOVER: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x3A);
    pub const PRESSED: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);
    pub const STROKE: Color32 = Color32::from_rgb(0x3D, 0x3D, 0x3D);
    /// 滑条底槽。见浅色那边的说明。
    pub const RAIL: Color32 = Color32::from_rgb(0x5A, 0x5A, 0x5A);
    pub const TEXT: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
    pub const TEXT_WEAK: Color32 = Color32::from_rgb(0xC5, 0xC5, 0xC5);
}

/// 把主题应用到 egui 上下文。
///
/// # 为什么要把两套 style 都设一遍
///
/// egui 0.29 内部维护 **light / dark 两套独立的 style**,`ctx.style()`
/// 返回哪一套取决于它当前的主题解析结果:
///
/// ```text
/// ThemePreference::System => system_theme.unwrap_or(fallback_theme)
/// ```
///
/// `system_theme` 是**每帧**从 winit 传来的。于是在启动的第一帧它还是
/// `None`,会退回 `fallback_theme`(Dark)—— 这时如果按"读当前那套、
/// 改完写回"来做,主题就全写进了 dark_style,而界面稍后用 light_style
/// 渲染,等于什么都没改。
///
/// 表现出来就是:刚打开时看着还行,一旦手动切主题,之前那次没生效的
/// 样式(间距、内边距)突然全部落下,界面像被"放大"了一截。
///
/// 所以这里显式指定 `ThemePreference`,并且对两套 style 分别设置。
pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    // 1. 告诉 egui 用哪个偏好,让它的解析结果和我们的选择一致。
    ctx.set_theme(mode.to_preference());

    // 2. 两套都装上,这样系统主题变化时也是我们的样式。
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let style = build_style(theme);
        ctx.set_style_of(theme, style);
    }
}

/// 按给定的明暗构建一份完整的 Style。
///
/// 基础取自 **该主题自己的默认 style**(`Theme::default_style()`),而不是
/// `ctx.style()` —— 后者返回的是"egui 此刻认为该用哪一套",在 System
/// 模式下第一帧还没有 `system_theme`,会给你 fallback 那一套,于是
/// light 的主题被建在了 dark 的底子上。
fn build_style(theme: egui::Theme) -> egui::Style {
    let is_dark = theme == egui::Theme::Dark;
    let accent = accent_color(is_dark);
    // 强调色上的文字:Windows 会根据强调色的明度自动选黑或白。
    let on_accent = if is_dark {
        Color32::from_rgb(0x00, 0x00, 0x00)
    } else {
        Color32::WHITE
    };

    let mut style = theme.default_style();
    let v = &mut style.visuals;

    v.dark_mode = is_dark;

    if is_dark {
        v.panel_fill = dark::LAYER;
        v.window_fill = dark::CARD;
        v.extreme_bg_color = dark::CONTROL;
        // 表格斑马纹、滑轨底槽这类"很淡"的背景。
        v.faint_bg_color = Color32::from_rgb(0x27, 0x27, 0x27);
        // 不画面板/窗口的边框线:Fluent 的分区靠底色明度差,
        // 一条 1px 的边线只会让界面显得毛糙。(egui 的 panel 分隔线
        // 也用这个值,置空后连分隔线一并不见了。)
        v.window_stroke = Stroke::NONE;
        v.hyperlink_color = accent;
        v.warn_fg_color = Color32::from_rgb(0xF7, 0x9B, 0x3C);
        v.error_fg_color = Color32::from_rgb(0xFF, 0x99, 0xA4);
        // 刻意**不**设 override_text_color:它会盖掉所有文字颜色,
        // 包括选中按钮该用的白色 —— 变成"深蓝底 + 近黑字",根本看不清。
        // 文字颜色交给下面各控件的 fg_stroke 分别控制。
    } else {
        v.panel_fill = light::LAYER;
        v.window_fill = light::CARD;
        v.extreme_bg_color = light::CONTROL;
        v.faint_bg_color = Color32::from_rgb(0xF9, 0xF9, 0xF9);
        v.window_stroke = Stroke::NONE;
        v.hyperlink_color = accent;
        v.warn_fg_color = Color32::from_rgb(0x9D, 0x5D, 0x00);
        v.error_fg_color = Color32::from_rgb(0xC4, 0x2B, 0x1C);
    }

    // 选中态:强调色填充 + 反色文字。
    v.selection.bg_fill = accent;
    v.selection.stroke = Stroke::new(1.0_f32, on_accent);

    // 圆角。这是 Fluent 观感里最直观的一环。
    v.window_rounding = Rounding::same(CORNER_CARD);
    v.menu_rounding = Rounding::same(CORNER_CARD);
    // 浮层阴影调淡一点,Windows 11 的阴影比 egui 默认克制。
    v.window_shadow = egui::epaint::Shadow {
        offset: egui::vec2(0.0, 4.0),
        blur: 16.0,
        spread: 0.0,
        color: Color32::from_black_alpha(if is_dark { 96 } else { 40 }),
    };
    v.popup_shadow = v.window_shadow;

    // 各类控件的外观。分四种状态:Fluent 用"叠加一层薄薄的明度"
    // 来表示 hover/press,而不是换一整套颜色。
    let widgets = &mut v.widgets;

    widgets.noninteractive.bg_fill = if is_dark { dark::CARD } else { light::CARD };
    widgets.noninteractive.weak_bg_fill = if is_dark { dark::CARD } else { light::CARD };
    widgets.noninteractive.bg_stroke = Stroke::NONE;
    widgets.noninteractive.fg_stroke = Stroke::new(
        1.0_f32,
        if is_dark {
            dark::TEXT_WEAK
        } else {
            light::TEXT_WEAK
        },
    );
    widgets.noninteractive.rounding = Rounding::same(CORNER_CONTROL);
    widgets.noninteractive.expansion = 0.0;

    widgets.inactive.bg_fill = if is_dark {
        dark::CONTROL
    } else {
        light::CONTROL
    };
    widgets.inactive.weak_bg_fill = if is_dark { dark::BUTTON } else { light::BUTTON };
    widgets.inactive.bg_stroke =
        Stroke::new(1.0_f32, if is_dark { dark::STROKE } else { light::STROKE });
    widgets.inactive.fg_stroke =
        Stroke::new(1.0_f32, if is_dark { dark::TEXT } else { light::TEXT });
    widgets.inactive.rounding = Rounding::same(CORNER_CONTROL);
    widgets.inactive.expansion = 0.0;

    widgets.hovered.bg_fill = if is_dark { dark::HOVER } else { light::HOVER };
    widgets.hovered.weak_bg_fill = if is_dark { dark::HOVER } else { light::HOVER };
    widgets.hovered.bg_stroke =
        Stroke::new(1.0_f32, if is_dark { dark::STROKE } else { light::STROKE });
    widgets.hovered.fg_stroke =
        Stroke::new(1.0_f32, if is_dark { dark::TEXT } else { light::TEXT });
    widgets.hovered.rounding = Rounding::same(CORNER_CONTROL);
    widgets.hovered.expansion = 0.0;

    widgets.active.bg_fill = if is_dark {
        dark::PRESSED
    } else {
        light::PRESSED
    };
    widgets.active.weak_bg_fill = if is_dark {
        dark::PRESSED
    } else {
        light::PRESSED
    };
    widgets.active.bg_stroke =
        Stroke::new(1.0_f32, if is_dark { dark::STROKE } else { light::STROKE });
    widgets.active.fg_stroke = Stroke::new(1.0_f32, if is_dark { dark::TEXT } else { light::TEXT });
    widgets.active.rounding = Rounding::same(CORNER_CONTROL);
    widgets.active.expansion = 0.0;

    widgets.open.bg_fill = if is_dark { dark::HOVER } else { light::HOVER };
    widgets.open.weak_bg_fill = if is_dark { dark::HOVER } else { light::HOVER };
    widgets.open.bg_stroke =
        Stroke::new(1.0_f32, if is_dark { dark::STROKE } else { light::STROKE });
    widgets.open.fg_stroke = Stroke::new(1.0_f32, if is_dark { dark::TEXT } else { light::TEXT });
    widgets.open.rounding = Rounding::same(CORNER_CONTROL);
    widgets.open.expansion = 0.0;

    // 间距:Windows 11 的控件比 egui 默认略高一点,点击目标更舒服。
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 4.0);
    s.button_padding = egui::vec2(8.0, 2.0);
    s.window_margin = egui::Margin::same(12.0);
    s.menu_margin = egui::Margin::same(6.0);
    s.interact_size.y = 20.0;
    s.slider_width = 130.0;
    s.slider_rail_height = 5.0;

    style
}

// ---------------------------------------------------------------------------
// 容器样式
// ---------------------------------------------------------------------------

/// 一道"内容区"的边框。
///
/// 颜色直接复用 `ui.visuals().window_fill` —— 那是 egui 已经按当前主题
/// 设好的卡片色。早先这里自己判断 `dark_mode` 再挑常量,一旦判断和实际
/// 主题不一致(比如首帧 `system_theme` 还没到位),就会在浅色界面上套一个
/// 深色的边,看起来像个突兀的黑框。
///
/// **不加描边**:Fluent 的层次靠**明度差**建立,靠描边只会把界面切得
/// 七零八落。这里用「窗口底色 → 卡片色 → 浅一档」三层来分区。
pub fn content_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::none()
        .fill(ui.visuals().window_fill)
        .rounding(Rounding::same(CORNER_CARD))
        .inner_margin(egui::Margin::same(CARD_PADDING))
}

/// 宽度由**外层**决定的内容卡片。
///
/// 和 [`content_frame`] 长得一样,区别在宽度归谁说了算:
///
/// * `content_frame` 走 `Frame::show`,卡片宽度跟着内容走;
/// * 这里先把卡片矩形算好,再让内容画进去,内容再宽也撑不大它。
///
/// # 为什么需要这个
///
/// egui 的容器全是"内容决定尺寸":子控件一旦溢出,父容器就跟着变宽。
/// 引擎设置那两栏用的是 `Ui::columns`,而它收尾时会按「**最宽**的那一列
/// × 列数」重算总宽:
///
/// ```text
/// total_required_width = spacing + max_column_width * num_columns
/// ```
///
/// 左栏的缓冲区按钮组刚好比列宽宽了 20px,于是 `max_column_width` 从
/// 474 变成 494,总宽算成 `8 + 494 × 2 = 996` —— 比可用宽度 956 还大。
/// 父 `Ui` 被这一下撑到 1018(超出窗口),卡片跟着一路顶到窗口右缘,
/// **右边距就这么没了**,而左边距还好端端的。
///
/// 面板宽度本来就是固定的,卡片理应跟着固定。所以这里把宽度锁死,只让
/// 内容决定高度(面板自身的高度正是这么来的)。
pub fn panel_card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let outer = ui.available_rect_before_wrap();
    let inner = outer.shrink(CARD_PADDING);

    // 先占一个绘制槽位,等内容跑完再回填卡片背景。
    //
    // 顺序不能反:画家按调用先后落笔,背景要是后画,就会把内容盖个严实
    // ——表现出来是"卡片一片空白"。`Frame::show` 内部也正是这么做的
    // (`add(Shape::Noop)` 占位,收尾时 `set` 回填)。
    let background = ui.painter().add(egui::Shape::Noop);

    let mut content_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let ret = add_contents(&mut content_ui);

    // 高度完全由内容决定 —— 自适应高度的面板正是这么算出自己该多高的。
    //
    // 这里**不能**拿 `inner` 的高度去封顶:面板第一帧拿到的还是它上一帧
    // 的高度,而自适应面板初次只有一个控件那么高,一封顶卡片就被压没了。
    let height = content_ui.min_rect().height();
    let card = egui::Rect::from_min_size(
        outer.min,
        egui::vec2(outer.width(), height + CARD_PADDING * 2.0),
    );

    ui.painter().set(background, content_frame(ui).paint(card));
    ui.allocate_rect(card, egui::Sense::hover());
    ret
}

/// 设备卡片的边框。
///
/// 底色取窗口底色(`panel_fill`):它嵌在白色内容区里,用浅一档的灰才看
/// 得出边界;反过来就成了"白卡片上贴白卡片",白费功夫。
pub fn card_frame(ui: &egui::Ui, contained: bool) -> egui::Frame {
    let frame = egui::Frame::none()
        .rounding(Rounding::same(CORNER_CONTROL + 2.0))
        .inner_margin(egui::Margin::same(8.0));
    if contained {
        frame.fill(ui.visuals().panel_fill)
    } else {
        frame
    }
}

// ---------------------------------------------------------------------------
// 读取系统设置
// ---------------------------------------------------------------------------

/// 取系统强调色。
///
/// `HKCU\Software\Microsoft\Windows\DWM\AccentColor` 存的是 **ABGR**
/// 打包成的 DWORD(不是常见的 ARGB),所以要手工拆字节。
///
/// 深色模式下 Fluent 会把强调色提亮,否则在深底上对比度不够 —— 这里
/// 用同样的思路:混合一部分白色。
fn accent_color(is_dark: bool) -> Color32 {
    /// Windows 默认强调色 `#0078D4`,也是系统的出厂值。
    const DEFAULT_ACCENT: Color32 = Color32::from_rgb(0x00, 0x78, 0xD4);

    let base = read_dword(r"Software\Microsoft\Windows\DWM", "AccentColor")
        .map(|v| {
            // ABGR:低字节是 R。
            Color32::from_rgb(
                (v & 0xFF) as u8,
                ((v >> 8) & 0xFF) as u8,
                ((v >> 16) & 0xFF) as u8,
            )
        })
        .unwrap_or(DEFAULT_ACCENT);

    if is_dark {
        // 往白色方向混 45%,得到 Fluent 深色主题那种浅蓝。
        mix(base, Color32::WHITE, 0.45)
    } else {
        base
    }
}

/// 按比例把 `a` 往 `b` 混合,`t = 0` 得到 `a`,`t = 1` 得到 `b`。
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

/// 读一个 REG_DWORD。
#[cfg(windows)]
fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    };

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let subkey_w = wide(subkey);
    let value_w = wide(value);

    let mut key: HKEY = core::ptr::null_mut();
    // SAFETY: 两个宽字符串都以 0 结尾;key 是合法的输出位置。
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey_w.as_ptr(), 0, KEY_READ, &mut key) };
    if status != 0 {
        return None;
    }

    let mut data: u32 = 0;
    let mut size = core::mem::size_of::<u32>() as u32;
    let mut kind: u32 = 0;
    // SAFETY: data/size/kind 都是本地的合法输出缓冲。
    let status = unsafe {
        RegQueryValueExW(
            key,
            value_w.as_ptr(),
            core::ptr::null(),
            &mut kind,
            &mut data as *mut u32 as *mut u8,
            &mut size,
        )
    };
    // SAFETY: key 由上面成功打开,必须关闭。
    unsafe { RegCloseKey(key) };

    if status == 0 && kind == REG_DWORD && size == core::mem::size_of::<u32>() as u32 {
        Some(data)
    } else {
        None
    }
}

#[cfg(not(windows))]
fn read_dword(_subkey: &str, _value: &str) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 颜色混合按预期工作() {
        let black = Color32::from_rgb(0, 0, 0);
        let white = Color32::from_rgb(255, 255, 255);
        assert_eq!(mix(black, white, 0.0), black);
        assert_eq!(mix(black, white, 1.0), white);
        // 50% 应当是中灰。
        let half = mix(black, white, 0.5);
        assert_eq!(half.r(), 128);
        // 越界值被夹住,不会 panic。
        assert_eq!(mix(black, white, 2.0), white);
        assert_eq!(mix(black, white, -1.0), black);
    }

    #[test]
    fn 深色模式的强调色更亮() {
        let light = accent_color(false);
        let dark = accent_color(true);
        let sum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert!(
            sum(dark) > sum(light),
            "深色强调色({dark:?})应当比浅色的({light:?})亮"
        );
    }

    #[test]
    fn 主题模式解析() {
        assert_eq!(
            ThemeMode::Light.to_preference(),
            egui::ThemePreference::Light
        );
        assert_eq!(ThemeMode::Dark.to_preference(), egui::ThemePreference::Dark);
        // System 交给 egui 自己解析,这里只验证映射正确。
        assert_eq!(
            ThemeMode::System.to_preference(),
            egui::ThemePreference::System
        );
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    /// "点浅色后文字变大" 那个 bug 的回归测试。
    ///
    /// 主题应用必须**幂等**:反复应用同一个模式,字号和间距都不能累积变化。
    #[test]
    fn 重复应用主题不会改变字号() {
        let ctx = egui::Context::default();

        let snapshot = |ctx: &egui::Context| {
            let s = ctx.style();
            let mut out: Vec<(String, f32)> = s
                .text_styles
                .iter()
                .map(|(name, font)| (format!("{name:?}"), font.size))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        };

        apply(&ctx, ThemeMode::Light);
        let first = snapshot(&ctx);

        // 再应用几次,字号必须完全不变。
        apply(&ctx, ThemeMode::Light);
        apply(&ctx, ThemeMode::Light);
        let third = snapshot(&ctx);

        assert_eq!(first, third, "重复应用主题改变了字号");
    }

    /// 间距同理 —— 累积增长会让界面越点越"胖"。
    #[test]
    fn 重复应用主题不会累积间距() {
        let ctx = egui::Context::default();

        apply(&ctx, ThemeMode::Light);
        let first = (
            ctx.style().spacing.item_spacing,
            ctx.style().spacing.button_padding,
        );

        for _ in 0..3 {
            apply(&ctx, ThemeMode::Light);
        }
        let after = (
            ctx.style().spacing.item_spacing,
            ctx.style().spacing.button_padding,
        );

        assert_eq!(first, after, "重复应用主题累积了间距");
    }

    /// 切换模式不应该动到 egui 的缩放设置 —— 那个会整体放大界面。
    #[test]
    fn 切换主题不影响缩放() {
        let ctx = egui::Context::default();
        let before = ctx.pixels_per_point();

        apply(&ctx, ThemeMode::Light);
        apply(&ctx, ThemeMode::Dark);
        apply(&ctx, ThemeMode::System);

        assert_eq!(before, ctx.pixels_per_point(), "切换主题改变了缩放");
    }
}

/// 画一条滑条,底槽用看得见的灰。
///
/// egui 的滑条底槽固定取 `widgets.inactive.bg_fill`(见 `slider.rs` 里
/// `rect_filled(rail_rect, .., widget_visuals.inactive.bg_fill)`),而本主题
/// 把这个值设成了卡片同色 —— 浅色下轨道和卡片都是纯白,**整条轨道就没了**,
/// 界面上只剩一个孤零零的把手。
///
/// 不能直接把 `widgets.inactive.bg_fill` 全局改成灰:复选框、单选框的方框
/// 也取这个值,它们该是白底加描边。所以这里用一层 [`egui::Ui::scope`] 把
/// 颜色临时换掉,只作用于这一条滑条。
///
/// 代价是**把手也会跟着变灰** —— 滑条把轨道和把手绑在同一个颜色令牌上
/// (`CircleShape { fill: visuals.bg_fill, .. }`),没给把手留单独的入口。
/// 好在把手还有 `fg_stroke` 那圈描边,不至于糊进轨道里。想让把手白回去,
/// 只能自己复刻一遍滑条的绘制。
pub fn slider<'a>(ui: &mut egui::Ui, slider: egui::Slider<'a>) -> egui::Response {
    let rail = if ui.visuals().dark_mode {
        dark::RAIL
    } else {
        light::RAIL
    };
    ui.scope(|ui| {
        ui.visuals_mut().widgets.inactive.bg_fill = rail;
        ui.add(slider)
    })
    .inner
}

/// 画一条电平表(进度条),底槽用看得见的灰。
///
/// 和 [`slider`] 是同一个毛病:egui 把进度条的底槽画成 `extreme_bg_color`
/// (`progress_bar.rs` 里 `rect(outer_rect, .., visuals.extreme_bg_color, ..)`),
/// 而本主题把它设成了卡片同色 —— 浅色下两者都是纯白,底槽整个隐形。电平低
/// 的时候(试运行不发声,输入只有底噪)就只剩填充部分那么一个小圆点,看着
/// 完全不像个条形控件。同理,底槽也不能全局改:多行文本框用的也是这个色。
pub fn progress_bar(ui: &mut egui::Ui, progress: f32, width: f32) -> egui::Response {
    let rail = if ui.visuals().dark_mode {
        dark::RAIL
    } else {
        light::RAIL
    };
    ui.scope(|ui| {
        ui.visuals_mut().extreme_bg_color = rail;
        ui.add(egui::ProgressBar::new(progress).desired_width(width))
    })
    .inner
}

/// 用于**面板自身**的边框(`TopBottomPanel::frame` / `SidePanel::frame`)。
///
/// 和 [`content_frame`] 长得一样,但用途不同,这个区别很致命:
///
/// 内边距**必须**交给面板的 frame,不能用 `ui.add_space()` 在内容里加。
/// 因为 egui 是用「内容的实际矩形」来记录面板高度并传给下一帧的
/// (`PanelState`) —— 内容里多 4px 空隙,下一帧的面板就高 8px(上下各一次),
/// 再下一帧再高 8px,一路涨到撞上窗口边界才停。这个正反馈会表现为
/// "面板越用越大、把别的区域挤没"。
///
/// 面板 frame 的 `inner_margin` 则是从面板高度里扣的,不会累加。
pub fn panel_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::side_top_panel(&ctx.style())
        // 底色用**窗口底色**,不是卡片色。
        //
        // 这样面板左右各留出一圈窗口底色,和中央那两栏设备区(由
        // CentralPanel 的 inner_margin 让出来的)在视觉上对齐。面板里的
        // `content_frame` 才是那块白卡片。反过来把面板刷成卡片色的话,
        // 白色会一直铺到窗口边缘,和中间两栏对不齐。
        .fill(ctx.style().visuals.panel_fill)
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
}
