// ---------------------------------------------------------------------------
// daisyUI 默认主题调色板（light / dark）
//
// 取色自 daisyUI 5 内置的 `light` / `dark` 主题，即其 CSS 变量 `--color-*`
// 的 sRGB 值。换算链路：OKLCH → OKLab → linear sRGB（标准 Oklab 矩阵）
// → sRGB gamma（IEC 61966-2-1）；部分色值在注释中附了 daisyUI 的 OKLCH
// 原始定义，便于日后与升级后的定义逐项比对。
//
// 候选窗与设置面板共用同一套调色板，使两个窗口在切换主题时观感一致。
// ---------------------------------------------------------------------------

use std::sync::Mutex;
use std::time::{Duration, Instant};

use black_hole_platform::system_uses_dark_mode;
use black_hole_shared::Theme;
use eframe::egui::{Color32, Context, Stroke, Visuals};

/// daisyUI 主题变量（`--color-*`）到 egui 颜色的映射。
///
/// 字段名与 CSS 变量同名（`--color-base-100` → `base_100`），便于与 daisyUI
/// 主题定义逐项对照。
#[derive(Clone, Copy)]
pub struct Palette {
    /// `base-100`：卡片/浮层底色
    pub base_100: Color32,
    /// `base-200`：次级底色（页面背景、悬停态控件、faint 背景）
    pub base_200: Color32,
    /// `base-300`：更深一级的底色（按下态控件）
    pub base_300: Color32,
    /// `base-content`：正文文字
    pub base_content: Color32,
    /// 正文弱化色（`base-content` 60% 不透明度）：序号、提示等次要文字
    pub base_content_weak: Color32,
    /// 边框色：daisyUI v5 以 `base-content` 20% 不透明度取代固定的 base-300
    pub border: Color32,
    /// `primary`：选中态底色
    pub primary: Color32,
    /// `primary-content`：选中态上的文字
    pub primary_content: Color32,
    /// 成功提示文字色（浅色主题取 `success-content`，保证白底可读）
    pub success_text: Color32,
    /// 失败提示文字色（浅色主题取 `error-content`，保证白底可读）
    pub error_text: Color32,
    /// 是否为深色配色：`visuals` 据此挑选 egui 内置样式作为基底，
    /// 调用方也用它作为样式缓存键（`Theme::System` 解析结果变化时需重建样式）
    pub dark: bool,
}

/// 按主题取调色板；`System` 由平台探针解析为亮色或暗色。
pub fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::Light => light(),
        Theme::Dark => dark(),
        Theme::System => {
            if system_dark() {
                dark()
            } else {
                light()
            }
        }
    }
}

/// 系统暗色状态的进程内缓存。
///
/// 候选窗每帧都要取调色板，逐帧读注册表过于昂贵；1 秒 TTL 既避开系统调用，
/// 又能让用户在系统里切换亮/暗后很快反映到界面上。
static SYSTEM_DARK: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// 带 TTL 缓存的 [`system_uses_dark_mode`]。
fn system_dark() -> bool {
    const TTL: Duration = Duration::from_secs(1);
    let mut cache = SYSTEM_DARK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match *cache {
        // 缓存仍然新鲜
        Some((updated_at, dark)) if updated_at.elapsed() < TTL => dark,
        _ => {
            let dark = system_uses_dark_mode();
            *cache = Some((Instant::now(), dark));
            dark
        }
    }
}

/// daisyUI `light` 主题
fn light() -> Palette {
    // base-content: oklch(21% 0.006 285.885)
    let base_content = Color32::from_rgb(24, 24, 27);
    Palette {
        base_100: Color32::from_rgb(255, 255, 255),
        base_200: Color32::from_rgb(248, 248, 248),
        base_300: Color32::from_rgb(238, 238, 238),
        base_content,
        base_content_weak: base_content.gamma_multiply(0.6),
        border: base_content.gamma_multiply(0.2),
        // primary: oklch(45% 0.24 277.023)
        primary: Color32::from_rgb(66, 42, 213),
        // primary-content: oklch(93% 0.034 272.788)
        primary_content: Color32::from_rgb(224, 231, 255),
        // success-content: oklch(37% 0.077 168.94)；白底上 success 本体过亮
        success_text: Color32::from_rgb(0, 76, 57),
        // error-content: oklch(27% 0.105 12.094)
        error_text: Color32::from_rgb(77, 2, 24),
        dark: false,
    }
}

/// daisyUI `dark` 主题
fn dark() -> Palette {
    // base-content: oklch(97.807% 0.029 256.847)
    let base_content = Color32::from_rgb(236, 249, 255);
    Palette {
        base_100: Color32::from_rgb(29, 35, 42),
        base_200: Color32::from_rgb(25, 30, 36),
        base_300: Color32::from_rgb(21, 25, 30),
        base_content,
        base_content_weak: base_content.gamma_multiply(0.6),
        border: base_content.gamma_multiply(0.2),
        // primary: oklch(58% 0.233 277.117)
        primary: Color32::from_rgb(96, 93, 255),
        // primary-content: oklch(96% 0.018 272.314)
        primary_content: Color32::from_rgb(237, 241, 254),
        // 深色底上使用 success / error 本体即可读
        success_text: Color32::from_rgb(0, 211, 144),
        error_text: Color32::from_rgb(255, 98, 125),
        dark: true,
    }
}

/// 由调色板派生 egui 全局样式（设置面板使用）。
///
/// 层次约定沿用 daisyUI：`base-100` 用于卡片/浮层/控件，`base-200` 用于页面
/// 与悬停，`base-300` 用于按下，选中态为 `primary` + `primary-content`。
///
/// 设置面板的根 `Ui` 之上没有 `CentralPanel`，页面底色取自 `window_fill`
/// （= `base-100`）并由 `App::clear_color` 铺满；`panel_fill`（= `base-200`）
/// 只有将来改用 `CentralPanel` 时才会生效。
pub fn visuals(p: Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.panel_fill = p.base_200;
    v.window_fill = p.base_100;
    v.window_stroke = Stroke::new(1.0, p.border);
    // 输入类控件（文本输入、拖拽数值）与滚动区底色
    v.extreme_bg_color = p.base_100;
    v.text_edit_bg_color = Some(p.base_100);
    v.faint_bg_color = p.base_200;
    v.code_bg_color = p.base_200;
    v.hyperlink_color = p.primary;
    v.error_fg_color = p.error_text;
    v.weak_text_color = Some(p.base_content_weak);

    // 选中态：daisyUI 的 primary（`selectable_value`、文本选区共用）
    v.selection.bg_fill = p.primary;
    v.selection.stroke = Stroke::new(1.0, p.primary_content);

    // 非交互态 = 卡片：base-100 底 + 边框区分层次
    v.widgets.noninteractive.bg_fill = p.base_100;
    v.widgets.noninteractive.weak_bg_fill = p.base_100;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.base_content);

    // 静止态 = 默认按钮/复选框：base-100 底 + 边框
    v.widgets.inactive.bg_fill = p.base_100;
    v.widgets.inactive.weak_bg_fill = p.base_100;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.base_content);

    // 悬停 / 按下：逐级加深，与 daisyUI 默认按钮一致
    v.widgets.hovered.bg_fill = p.base_200;
    v.widgets.hovered.weak_bg_fill = p.base_200;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.base_content);

    v.widgets.active.bg_fill = p.base_300;
    v.widgets.active.weak_bg_fill = p.base_300;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.primary);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.base_content);

    // 展开/聚焦态：边框转 primary，对应 daisyUI 输入框的 focus 样式
    v.widgets.open.bg_fill = p.base_200;
    v.widgets.open.weak_bg_fill = p.base_200;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.primary);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.base_content);

    v
}

/// 仅当主题（含 `System` 解析出的明暗）相对上次应用发生变化时才
/// `set_visuals`：候选窗每次按键都会重绘，逐帧重设会白白丢弃样式缓存。
/// `cache` 由调用方持有，初始传 `None`。
pub fn sync_visuals(ctx: &Context, theme: Theme, cache: &mut Option<(Theme, bool)>) {
    let palette = palette(theme);
    let key = (theme, palette.dark);
    if *cache != Some(key) {
        ctx.set_visuals(visuals(palette));
        *cache = Some(key);
    }
}
