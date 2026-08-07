use serde::{Deserialize, Serialize};

pub mod candidate_layout;

/// 按键事件，由平台适配层解析后发送给引擎
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: String,
    pub modifiers: Modifiers,
    pub state: KeyState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
    #[serde(default)]
    pub capslock: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyState {
    Press,
    Release,
}

/// 单个候选词
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub text: String,
    pub comment: Option<String>,
    pub score: i64,
}

/// 输入上下文（如当前应用、光标位置等）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InputContext {
    pub caret_x: i32,
    pub caret_y: i32,
    pub caret_h: i32,
}

/// 输入方案标识
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SchemeId {
    Pinyin,
    Shuangpin,
}

/// 引擎返回给 UI 的结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemeResult {
    Composing {
        /// 当前编码串（如拼音 "zhongwen"）
        code: String,
        /// 候选列表
        candidates: Vec<Candidate>,
        /// 当前选中的候选索引
        selected_index: usize,
        /// 候选窗是否展开为完整列表
        expanded: bool,
    },
    Committed {
        text: String,
    },
    Ignored,
}

impl SchemeResult {
    /// 获取当前编码（如果有）
    pub fn code(&self) -> Option<&str> {
        match self {
            SchemeResult::Composing { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// 中英文输入模式切换状态机（Ctrl 键触发）
///
/// 按下 Ctrl 时标记切换候选；按住 Ctrl 期间按下任意其他键则取消候选；
/// 松开 Ctrl 时若候选仍有效则切换模式。由平台层持有并驱动，
/// 模式状态保留在平台层（不经过引擎），英文模式下平台层直接放行按键。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputModeSwitch {
    english: bool,
    ctrl_pending: bool,
}

impl InputModeSwitch {
    /// 当前是否为英文输入模式
    pub fn is_english(&self) -> bool {
        self.english
    }

    /// Ctrl 键按下
    pub fn ctrl_pressed(&mut self) {
        self.ctrl_pending = true;
    }

    /// 其他键按下；`ctrl_held` 表示 Ctrl 仍处于按住状态
    pub fn other_key_pressed(&mut self, ctrl_held: bool) {
        if ctrl_held {
            self.ctrl_pending = false;
        }
    }

    /// Ctrl 键松开；返回 `Some(english)` 表示发生了模式切换
    pub fn ctrl_released(&mut self) -> Option<bool> {
        if self.ctrl_pending {
            self.ctrl_pending = false;
            self.english = !self.english;
            Some(self.english)
        } else {
            None
        }
    }

    /// 直接设置模式（如系统面板点击属性触发）；
    /// 返回 `Some(english)` 表示发生了模式切换
    pub fn set_english(&mut self, english: bool) -> Option<bool> {
        self.ctrl_pending = false;
        if self.english != english {
            self.english = english;
            Some(english)
        } else {
            None
        }
    }
}

/// 平台适配层 → 引擎的命令
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineCommand {
    Key(KeyEvent),
    SetContext(InputContext),
    SelectCandidate(usize),
    SwitchScheme(SchemeId),
    /// 运行时热更新按键绑定（设置面板实时生效）
    UpdateKeyBindings(KeyBindings),
    Reset,
    /// daemon → 引擎线程：退出信号（唤醒阻塞的 recv，使引擎线程及时结束）
    Shutdown,
}

/// 引擎 → UI 的命令
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiCommand {
    ShowCandidates {
        code: String,
        candidates: Vec<Candidate>,
        selected_index: usize,
        context: InputContext,
        expanded: bool,
    },
    UpdatePosition {
        context: InputContext,
    },
    HideCandidates,
    CommitText(String),
    UpdateStatus(String),
    ShowSettings,
    SetAutoStart(bool),
    SetTheme(Theme),
    SwitchScheme(SchemeId),
    /// 中英文输入模式切换：TSF 实例切换后上报 daemon，
    /// daemon 持久化并更新共享状态，供其它进程同步。
    SetInputMode(bool),
    /// daemon → 候选窗线程：热更新候选窗参数（字号、最大候选数等）
    SetCandidateWindowSettings(CandidateWindowSettings),
    Exit,
}

/// 应用设置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub theme: Theme,
    pub default_scheme: SchemeId,
    pub candidate_window: CandidateWindowSettings,
    pub key_bindings: KeyBindings,
    /// 是否开机自启动（登录时自动运行守护进程）
    #[serde(default)]
    pub auto_start: bool,
    /// 中英文输入模式：true=英文，false=中文。
    /// 由 daemon 全局持有并持久化，各进程 TSF 实例启动/获得焦点时同步。
    #[serde(default)]
    pub english_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            default_scheme: SchemeId::Pinyin,
            candidate_window: CandidateWindowSettings::default(),
            key_bindings: KeyBindings::default(),
            auto_start: false,
            english_mode: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Light,
    Dark,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateWindowSettings {
    pub font_size: u32,
    pub max_candidates: usize,
    pub width: u32,
    pub item_height: u32,
}

impl Default for CandidateWindowSettings {
    fn default() -> Self {
        Self {
            font_size: 14,
            max_candidates: 9,
            width: 320,
            item_height: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBindings {
    pub next_candidate: String,
    pub prev_candidate: String,
    pub commit: String,
    pub cancel: String,
    pub switch_scheme: String,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            next_candidate: "ArrowDown".to_string(),
            prev_candidate: "ArrowUp".to_string(),
            commit: "Space".to_string(),
            cancel: "Escape".to_string(),
            switch_scheme: "Ctrl+Shift+F12".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_press_release_toggles_mode() {
        let mut sw = InputModeSwitch::default();
        assert!(!sw.is_english());

        sw.ctrl_pressed();
        assert_eq!(sw.ctrl_released(), Some(true));
        assert!(sw.is_english());

        sw.ctrl_pressed();
        assert_eq!(sw.ctrl_released(), Some(false));
        assert!(!sw.is_english());
    }

    #[test]
    fn ctrl_combo_does_not_toggle() {
        let mut sw = InputModeSwitch::default();

        // Ctrl+C：按住 Ctrl 期间按下其他键，松开 Ctrl 不应切换
        sw.ctrl_pressed();
        sw.other_key_pressed(true);
        assert_eq!(sw.ctrl_released(), None);
        assert!(!sw.is_english());

        // Ctrl+Shift（系统切换布局快捷键）同样不应触发切换
        sw.ctrl_pressed();
        sw.other_key_pressed(true);
        assert_eq!(sw.ctrl_released(), None);
        assert!(!sw.is_english());
    }

    #[test]
    fn other_key_without_ctrl_does_not_affect_pending_toggle() {
        let mut sw = InputModeSwitch::default();

        // 未按住 Ctrl 时的普通按键不影响状态
        sw.other_key_pressed(false);
        sw.ctrl_pressed();
        assert_eq!(sw.ctrl_released(), Some(true));
    }

    #[test]
    fn release_without_press_does_not_toggle() {
        let mut sw = InputModeSwitch::default();
        assert_eq!(sw.ctrl_released(), None);
        assert!(!sw.is_english());
    }

    #[test]
    fn set_english_reports_only_actual_changes() {
        let mut sw = InputModeSwitch::default();

        // 与当前模式相同则不产生切换
        assert_eq!(sw.set_english(false), None);
        // 切换到英文并清除 Ctrl 候选状态
        sw.ctrl_pressed();
        assert_eq!(sw.set_english(true), Some(true));
        assert!(sw.is_english());
        assert_eq!(sw.ctrl_released(), None);
        // 切回中文
        assert_eq!(sw.set_english(false), Some(false));
        assert!(!sw.is_english());
    }
}
