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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// 平台适配层 → 引擎的命令
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineCommand {
    Key(KeyEvent),
    SetContext(InputContext),
    SelectCandidate(usize),
    SwitchScheme(SchemeId),
    Reset,
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
    SetTheme(Theme),
    SwitchScheme(SchemeId),
    Exit,
}

/// 应用设置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub theme: Theme,
    pub default_scheme: SchemeId,
    pub candidate_window: CandidateWindowSettings,
    pub key_bindings: KeyBindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            default_scheme: SchemeId::Pinyin,
            candidate_window: CandidateWindowSettings::default(),
            key_bindings: KeyBindings::default(),
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
