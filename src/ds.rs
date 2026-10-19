//! egui_sauge 设计系统接入层 —— 与 wrepl_GUI 完全同一套视觉。
//!
//! 界面风格（配色 Palette、圆角 RADIUS、间距 SPACING、九档字号层级、
//! Phosphor 图标字体、细滚动条）全部由 `egui_sauge` 提供，本模块只负责
//! **按正确顺序**把它装到 `egui::Context` 上。
//!
//! ## 初始化顺序（不能动，wrepl 侧踩过坑）
//!
//! 1. [`egui_sauge::install_fonts`] —— 内部是
//!    `FontDefinitions::default() + Phosphor`，**无条件覆盖**整个字体表，
//!    同时写入九档字号层级。先注册中文字体会被这一步整个冲掉 → 中文变豆腐块。
//! 2. 自己从头拼一份「egui 默认 + Phosphor + 中文」，再 `set_fonts` 覆盖第 1 步那份。
//!    `Proportional` 顺序必须是 `["cjk", "phosphor", …egui 默认…]`：
//!    Phosphor 的 cmap 覆盖 a–z 却不含 A–Z/数字，放前面会把小写字母吞成空白。
//! 3. [`apply`] —— 主题（palette + density + egui 自身的 theme 偏好）。

use eframe::egui;
use egui_sauge::{Density, Locale, Palette};

/// 中文字体在字体表里的键名。
const CJK_KEY: &str = "cjk";
/// Phosphor 图标字体在字体表里的键名。
const PHOSPHOR_KEY: &str = "phosphor";

/// 候选中文字体：按「更现代」排，前面的优先；末尾是 Windows 保底。
///
/// 只读系统已装字体、**不打包进产物** —— 零体积增长、无字体授权问题。
/// `.ttc` 是字体集合，靠 `FontData::index` 选第几张字面。
const CJK_CANDIDATES: &[(&str, u32)] = &[
    // ── 更现代的中文黑体（装了才生效，均为免费商用）──
    ("C:/Windows/Fonts/MiSans-Regular.ttf", 0),
    ("C:/Windows/Fonts/MiSans-Regular.otf", 0),
    ("C:/Windows/Fonts/HarmonyOS_Sans_SC_Regular.ttf", 0),
    ("C:/Windows/Fonts/SourceHanSansSC-Regular.otf", 0),
    ("C:/Windows/Fonts/SourceHanSansCN-Regular.otf", 0),
    ("C:/Windows/Fonts/NotoSansSC-Regular.otf", 0),
    ("C:/Windows/Fonts/AlibabaPuHuiTi-3-55-Regular.ttf", 0),
    ("C:/Windows/Fonts/OPPOSans-R.ttf", 0),
    // ── Windows 保底 ──
    ("C:/Windows/Fonts/msyh.ttc", 0),
    ("C:/Windows/Fonts/msyhbd.ttc", 0),
    ("C:/Windows/Fonts/simhei.ttf", 0),
    ("C:/Windows/Fonts/simsun.ttc", 0),
    ("C:/Windows/Fonts/Deng.ttf", 0),
];

/// 装字体与字号层级。开机一次（`run_native` 的创建闭包）。
pub fn install(ctx: &egui::Context) {
    // ── 1. 设计系统那一步：Phosphor 图标字体 + 九档字号层级 ───────────────
    egui_sauge::install_fonts(ctx);

    // ── 2. 自己拼一份含中文的字体表，覆盖掉第 1 步那份 ────────────────────
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    for (path, idx) in CJK_CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fd = egui::FontData::from_owned(bytes);
        fd.index = *idx;
        fonts.font_data.insert(CJK_KEY.to_owned(), fd.into());

        // Proportional 目标顺序：["cjk", "phosphor", …egui 自带…]
        // 中文字体必须在最前，先接走拉丁字母，否则 Phosphor 会把 a–z 吞成空白。
        // 图标字形在私用区（U+E000–F8FF），雅黑 cmap 里没有私用区码位，图标仍落到 phosphor。
        let prop = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        prop.retain(|k| k != CJK_KEY && k != PHOSPHOR_KEY);
        prop.insert(0, CJK_KEY.to_owned());
        prop.insert(1, PHOSPHOR_KEY.to_owned());

        // Monospace 只补一份中文兜底（日志里的中文），排末尾
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(CJK_KEY.to_owned());
        break;
    }
    ctx.set_fonts(fonts);

    // ── 3. 语言：只影响设计系统自带的那几串文案，固定 En 保证确定性 ────────
    egui_sauge::set_locale(ctx, Locale::En);
}

/// 应用设计系统主题。`light` = 浅色还是深色 palette。
///
/// 密度固定 [`Density::Comfortable`]（本工具表单字段不多，Compact 会压得过紧）。
pub fn apply(ctx: &egui::Context, light: bool) {
    // 让 egui 自身的 theme 偏好跟着走：内置控件（滚动条、文本选中）都读它。
    ctx.set_theme(if light {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
    let palette = if light {
        Palette::light()
    } else {
        Palette::dark()
    };
    egui_sauge::apply_theme_with(ctx, &palette, Density::Comfortable);

    // 滚动条改成常显的细条：egui 默认 floating 的休眠透明度是 0，
    // 内滚动条"其实有、看不见"，会让人以为内容没被截断。
    // 必须放在 apply_theme_with **之后**：那一步会重写 style。
    ctx.all_styles_mut(|style| {
        let sc = &mut style.spacing.scroll;
        sc.floating = true;
        sc.floating_width = 4.0;
        sc.floating_allocated_width = 4.0;
        sc.dormant_handle_opacity = 0.45;
        sc.dormant_background_opacity = 0.0;
        sc.active_handle_opacity = 0.65;
        sc.interact_handle_opacity = 0.9;
    });
}

/// 窗口清屏色，给 `eframe::App::clear_color` 用（线性化前的 0–1 分量）。
///
/// eframe 默认返回全透明，落到窗口上就是纯黑；面板之间一旦有一道没刷到的缝，
/// 缝里露出的就是那个黑。设成主题背景色后，这类缝即便还在也与两侧同色。
pub fn window_bg(light: bool) -> [f32; 4] {
    let c = if light {
        Palette::light()
    } else {
        Palette::dark()
    }
    .bg_app;
    let [r, g, b, _] = c.to_array();
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}
