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

/// 整句补全提示（LLM 异步返回后回传给引擎，Tab 提交时校验使用）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionHint {
    /// 发起请求时的编码串（如拼音 "wo"），用于校验结果是否仍匹配当前输入
    pub code: String,
    /// 发起请求时选中的候选索引
    pub selected_index: usize,
    /// 完整补全文本（不含选中词本身）
    pub text: String,
}

impl CompletionHint {
    /// 校验该补全是否仍适用于当前输入状态
    pub fn matches(&self, code: &str, selected_index: usize) -> bool {
        self.code == code && self.selected_index == selected_index
    }
}

/// 输入上下文（如当前应用、光标位置等）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InputContext {
    pub caret_x: i32,
    pub caret_y: i32,
    pub caret_h: i32,
    /// 光标前的文本（当前输入位置之前的已上屏内容），用于整句补全的上下文。
    /// 平台层尽力读取，读取失败或不可用时为 None。
    #[serde(default)]
    pub preceding_text: Option<String>,
    /// 光标后的文本（当前输入位置之后的内容），可选项，多数场景为 None。
    #[serde(default)]
    pub following_text: Option<String>,
}

impl InputContext {
    /// 便捷构造：仅定位信息，无周围文本
    pub fn caret(caret_x: i32, caret_y: i32, caret_h: i32) -> Self {
        Self {
            caret_x,
            caret_y,
            caret_h,
            preceding_text: None,
            following_text: None,
        }
    }
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
        /// 本次上屏是否为"临时英文"性质：包括临时英文模式（按住 Shift/CapsLock
        /// 输入的英文）的结束上屏，以及中文模式下原样上屏英文编码（Enter /
        /// 无候选 Space，上屏文本为键盘输入的 ASCII 串）。为 true 时，平台层
        /// 在英文语境下应锁定自动切换，避免上屏后立即被自动切换拉回英文/中文。
        temporary_english: bool,
    },
    /// 用户取消输入（Esc / cancel 绑定）：平台层须结束进行中的合成（清空
    /// 行内编码）并隐藏候选窗，同时消费该按键（不透传给宿主应用，避免
    /// 焦点转移导致候选窗被系统收起）。
    Cancelled,
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

/// 语境输入模式建议：根据光标周围文本推断。
///
/// [`suggest_input_mode`] 的返回值。英文/中文为强信号；数字与无信号
/// 都是"不动作"的中立结果，但语义不同——数字语境（[`ModeSuggestion::DigitsOnly`]）
/// 中英文输入都会出现，任何模式下都不触发自动切换；纯无信号
/// （[`ModeSuggestion::Neutral`]，空白文档/读取失败等）在英文模式下
/// 默认回切中文，避免自动切换产生的英文状态"粘住"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModeSuggestion {
    /// 英文强信号：光标周围最近的强信号字符是 ASCII 字母
    English,
    /// 中文强信号：光标周围最近的强信号字符是 CJK 字符或中文标点
    Chinese,
    /// 数字弱中性：扫描路径上出现过 ASCII 数字但无强信号。
    /// 数字不认定为英文，也不触发英→中默认回切——用户在英文模式下
    /// 输入 `123` 不应被切回中文。
    DigitsOnly,
    /// 无信号：空白/无上下文/文本读取失败。英文模式下默认回切中文
    /// （见 [`AutoModeSwitch::evaluate`]）。作为 Default 对应"无采样"
    /// 语境（与旧 Option<bool> 的 None 语义一致）。
    #[default]
    Neutral,
}

/// 根据光标周围文本推断目标输入模式。
///
/// 前文从末尾向前逐字符扫描，跳过 Unicode 空白、ASCII 标点与 ASCII 数字
/// （数字中性：中英文语境都会出现，不构成切换信号），命中的第一个强信号
/// 字符决定结果：ASCII 字母 → [`ModeSuggestion::English`]；CJK 字符或中文
/// 标点 → [`ModeSuggestion::Chinese`]；其它字符（如 emoji、其它文字）视为
/// 中性继续向前。前文无强信号时，再对后文从头向后做同样扫描；均无强信号
/// 时，任一侧扫描路径上出现过数字则返回 [`ModeSuggestion::DigitsOnly`]，
/// 否则返回 [`ModeSuggestion::Neutral`]。
pub fn suggest_input_mode(preceding: Option<&str>, following: Option<&str>) -> ModeSuggestion {
    let preceding_result = preceding
        .map(|text| scan_mode_signal(text.chars().rev()))
        .unwrap_or(ModeSuggestion::Neutral);
    match preceding_result {
        ModeSuggestion::English | ModeSuggestion::Chinese => preceding_result,
        // 前文无强信号：后文从头扫描，强信号优先于前文的数字弱中性
        _ => {
            let following_result = following
                .map(|text| scan_mode_signal(text.chars()))
                .unwrap_or(ModeSuggestion::Neutral);
            match following_result {
                ModeSuggestion::English | ModeSuggestion::Chinese => following_result,
                _ => {
                    if preceding_result == ModeSuggestion::DigitsOnly
                        || following_result == ModeSuggestion::DigitsOnly
                    {
                        ModeSuggestion::DigitsOnly
                    } else {
                        ModeSuggestion::Neutral
                    }
                }
            }
        }
    }
}

/// 逐字符扫描强信号：ASCII 字母 → 英文；CJK 字符/中文标点 → 中文；
/// 空白与 ASCII 标点跳过；ASCII 数字记录为弱中性后继续扫描。
/// 扫描结束仍无强信号时：路径上出现过数字 → DigitsOnly，否则 Neutral。
fn scan_mode_signal(chars: impl Iterator<Item = char>) -> ModeSuggestion {
    let mut saw_digit = false;
    for ch in chars {
        if ch.is_whitespace() || ch.is_ascii_punctuation() {
            continue;
        }
        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if ch.is_ascii_alphabetic() {
            return ModeSuggestion::English;
        }
        if is_cjk_or_zh_punct(ch) {
            return ModeSuggestion::Chinese;
        }
        // 中性字符：继续扫描
    }
    if saw_digit {
        ModeSuggestion::DigitsOnly
    } else {
        ModeSuggestion::Neutral
    }
}

/// 是否 CJK 字符或中文标点（全角字符）
fn is_cjk_or_zh_punct(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}'      // CJK 统一表意文字
        | '\u{3400}'..='\u{4DBF}'    // CJK 统一表意文字扩展 A
        | '\u{F900}'..='\u{FAFF}'    // CJK 兼容表意文字
        | '\u{3000}'..='\u{303F}'    // CJK 符号和标点
        | '\u{FF00}'..='\u{FFEF}') // 全角字符
}

/// 根据光标周围文本自动切换中英模式的状态机。
///
/// 配合 [`ModeSuggestion`] 使用：平台层在语境变化时调用 `evaluate`，
/// 返回 Some(target) 表示应自动切换；用户手动切换后调用 `lock_manual`
/// 并传入手动切换时刻评估的当前语境建议作为锁定基线，锁定期间同语境的
/// 建议不再撤销用户选择，语境变化后自动解锁、恢复自动切换。
/// 临时英文/原样上屏英文编码后调用 `suppress_next_zh_to_en`，仅抑制
/// 紧随其后的一次中→英自动切换（单次抑制，不产生持久锁定）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AutoModeSwitch {
    /// None=未锁定；Some(b)=已锁定，b 为锁定时语境基线
    locked_baseline: Option<ModeSuggestion>,
    /// 单次抑制标志：置位后下一次"中文模式 + 英文建议"的中→英评估被吞掉
    /// 一次并自动复位
    suppress_zh_to_en_once: bool,
}

impl AutoModeSwitch {
    /// 评估一次建议；返回 Some(target) 表示应自动切换到 target（true=英文）。
    pub fn evaluate(&mut self, suggestion: ModeSuggestion, current_english: bool) -> Option<bool> {
        // 已锁定：语境未变（建议等于基线）则保持锁定不动作（抑制标志不被
        // 无效评估消费）；语境已变则解除锁定，继续向下正常评估。
        if let Some(baseline) = self.locked_baseline {
            if suggestion == baseline {
                return None;
            }
            self.locked_baseline = None;
        }
        // 单次抑制：临时英文/原样上屏英文编码后，上屏文本本身使语境变为
        // 英文，紧随其后的首次中→英评估应被吞掉（否则用户刚起的拼音合成
        // 会在首键就被拉进英文模式直输）。仅在真正吞掉一次中→英切换时
        // 消费复位；语境明确变为中文时提前解除（抑制已无意义）。
        if self.suppress_zh_to_en_once {
            match suggestion {
                ModeSuggestion::English if !current_english => {
                    self.suppress_zh_to_en_once = false;
                    return None;
                }
                ModeSuggestion::Chinese => self.suppress_zh_to_en_once = false,
                _ => {}
            }
        }
        match suggestion {
            ModeSuggestion::English if !current_english => Some(true),
            ModeSuggestion::Chinese if current_english => Some(false),
            // 纯无信号（空白文档、新文件、文本读取失败等中立语境）：当前为
            // 英文时默认回到中文，避免自动切换产生的英文状态在中立语境
            // "粘住"（表现为新开文档/切换进程后默认英文）。手动 Ctrl 切到
            // 英文由 lock_manual 锁定保护；数字语境（DigitsOnly）是弱中性，
            // 中英文输入都会出现，不受此规则影响——英文模式下输入 `123`
            // 不应被切回中文。
            ModeSuggestion::Neutral if current_english => Some(false),
            _ => None,
        }
    }

    /// 用户手动切换后调用：以手动切换时刻评估的当前语境建议作为锁定基线。
    /// 必须即时采样传入（而非复用旧评估结果），否则用户移动到其它语境后
    /// 手动切换会以陈旧基线锁定，下个按键即被自动切换撤销。
    /// 手动锁定覆盖单次抑制（用户明确的选择优先于上屏抑制）。
    pub fn lock_manual(&mut self, current_suggestion: ModeSuggestion) {
        self.locked_baseline = Some(current_suggestion);
        self.suppress_zh_to_en_once = false;
    }

    /// 临时英文 / 中文模式下原样上屏英文编码（Enter / 无候选 Space）结束后
    /// 调用：仅抑制紧随其后的一次中→英自动切换。上屏的英文文本会使语境
    /// 立即变为英文，若无抑制，用户敲下的下一个字母会在合成开始前触发
    /// 中→英自动切换、被拉进英文模式直输。单次抑制而非持久锁定——持久
    /// 锁定在英文语境（如 IDE）持续存在时会吞掉后续所有自动切换。
    pub fn suppress_next_zh_to_en(&mut self) {
        self.suppress_zh_to_en_once = true;
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
    /// LLM 整句补全结果回传引擎（daemon worker 线程 → 引擎线程），
    /// 供 Tab 提交时校验后拼入上屏文本
    UpdateCompletion(Option<CompletionHint>),
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
    /// LLM 整句补全结果 → 候选窗：`code` 用于与当前编码串比对、
    /// `selected_index` 用于与当前选中项比对，避免异步结果错位显示；
    /// `None` 表示无补全（失败/超时/未启用）
    Completion {
        code: String,
        selected_index: usize,
        text: Option<String>,
    },
    UpdateStatus(String),
    ShowSettings,
    SetAutoStart(bool),
    SetTheme(Theme),
    SwitchScheme(SchemeId),
    /// 中英文输入模式切换：TSF 实例切换后上报 daemon。
    /// daemon 只更新共享状态供其它进程同步，不持久化 settings.json
    /// （中英模式是运行时状态，重启后回到中文）。连接时各平台实例
    /// 直接应用 daemon 当前模式，无"连接默认中文"门控。
    SetInputMode(bool),
    /// 自动切换中英模式上报：由"根据光标周围文本自动切换"逻辑触发，
    /// daemon 处理与 SetInputMode 一致（仅更新共享状态，不持久化）。
    SetInputModeTransient(bool),
    /// 开关"根据光标周围文本自动切换中英模式"：daemon 持久化并更新共享状态，
    /// 各进程 TSF 实例获得焦点时同步（与设置面板复选框等价）。
    SetAutoSwitch(bool),
    /// daemon → 候选窗线程：热更新候选窗参数（字号、最大候选数等）
    SetCandidateWindowSettings(CandidateWindowSettings),
    /// daemon → 候选窗线程：同步"整句上屏"实际绑定的按键名，
    /// 用于首行 Tab 提示显示真实绑定而非硬编码
    SetCommitSentenceKey(String),
    Exit,
}

/// 应用设置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub theme: Theme,
    pub default_scheme: SchemeId,
    pub candidate_window: CandidateWindowSettings,
    pub key_bindings: KeyBindings,
    /// 是否开机自启动（登录时自动运行守护进程）
    #[serde(default)]
    pub auto_start: bool,
    /// 是否根据光标周围文本自动切换中英模式（默认关闭，需用户显式开启）
    #[serde(default)]
    pub auto_switch_mode: bool,
    /// LLM 整句补全设置（默认关闭，需用户显式开启）
    #[serde(default)]
    pub llm_completion: LlmCompletionSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            default_scheme: SchemeId::Pinyin,
            candidate_window: CandidateWindowSettings::default(),
            key_bindings: KeyBindings::default(),
            auto_start: false,
            auto_switch_mode: false,
            llm_completion: LlmCompletionSettings::default(),
        }
    }
}

/// LLM 整句补全设置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCompletionSettings {
    /// 是否启用整句补全（默认关闭；内容会发送到 endpoint，需用户知情）
    #[serde(default)]
    pub enabled: bool,
    /// OpenAI 兼容补全端点，如 http://127.0.0.1:11434/v1/chat/completions
    #[serde(default = "default_llm_endpoint")]
    pub endpoint: String,
    /// 模型名，如 qwen2.5:1.5b
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// API Key（本地模型可为空，云端服务必填）
    #[serde(default)]
    pub api_key: String,
    /// 单次补全最大 token 数。推理模型（如 deepseek-reasoner / v4-flash）
    /// 会先消耗 token 做推理，32 常被推理耗尽导致 content 为空，默认放宽
    #[serde(default = "default_llm_max_tokens")]
    pub max_tokens: u32,
    /// 采样温度
    #[serde(default = "default_llm_temperature")]
    pub temperature: f32,
    /// 请求超时（毫秒）。云端 LLM（如 DeepSeek）推理耗时可能数秒，默认放宽到 15s
    #[serde(default = "default_llm_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_llm_endpoint() -> String {
    "http://127.0.0.1:11434/v1/chat/completions".to_string()
}

fn default_llm_model() -> String {
    "qwen2.5:1.5b".to_string()
}

fn default_llm_max_tokens() -> u32 {
    256
}

fn default_llm_temperature() -> f32 {
    0.7
}

fn default_llm_timeout_ms() -> u64 {
    15000
}

impl Default for LlmCompletionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_llm_endpoint(),
            model: default_llm_model(),
            api_key: String::new(),
            max_tokens: default_llm_max_tokens(),
            temperature: default_llm_temperature(),
            timeout_ms: default_llm_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Light,
    Dark,
    System,
}

/// daemon 运行时共享状态（非持久化设置项）：
/// 方案 / 主题 / 中英模式 / 自动切换开关。
/// 由 daemon 持有并在 dispatch / 热更新时同步更新，平台线程通过
/// IPC GetSettings 读取；中英模式启动播种为中文（重启后回到中文），
/// 会话内由 SetInputMode/SetInputModeTransient 更新、供跨进程同步。
/// 连接时平台实例直接应用 daemon 当前模式（含会话内英文），
/// 无"连接默认中文"门控。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub scheme_id: SchemeId,
    pub theme: Theme,
    /// true=英文，false=中文
    pub english: bool,
    /// "根据光标周围文本自动切换中英模式"开关
    pub auto_switch: bool,
}

impl RuntimeSettings {
    /// 以指定方案/主题构造，中英模式播种为中文（运行时状态，重启回中文）。
    pub fn new(scheme_id: SchemeId, theme: Theme) -> Self {
        Self {
            scheme_id,
            theme,
            english: false,
            auto_switch: false,
        }
    }
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
    /// 整句上屏（LLM 补全时 Tab 提交选中词+补全；无补全时回退为仅提交选中词）
    #[serde(default = "default_commit_sentence")]
    pub commit_sentence: String,
}

fn default_commit_sentence() -> String {
    "Tab".to_string()
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            next_candidate: "ArrowDown".to_string(),
            prev_candidate: "ArrowUp".to_string(),
            commit: "Space".to_string(),
            cancel: "Escape".to_string(),
            switch_scheme: "Ctrl+Shift+F12".to_string(),
            commit_sentence: default_commit_sentence(),
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

    // ------------------------------------------------------------------
    // 防双切协调逻辑：全局钩子（WH_KEYBOARD_LL）与 TSF 路径（OnTestKeyUp）
    // 共用同一 InputModeSwitch，配合 hook_toggled 抑制标志保证
    // "每次 Ctrl 松开恰好一次切换"。以下测试用局部变量模拟 hook_toggled
    // 标志的消费顺序（对应 hook.rs 的 on_ctrl_released 与 service.rs 的
    // OnTestKeyUp），钉住单切换不变量。
    // ------------------------------------------------------------------

    /// 模拟 hook.rs on_ctrl_released：消费候选，切换成功则置位抑制标志。
    fn hook_release(sw: &mut InputModeSwitch, hook_toggled: &mut bool) -> Option<bool> {
        let toggled = sw.ctrl_released();
        if toggled.is_some() {
            *hook_toggled = true;
        }
        toggled
    }

    /// 模拟 service.rs OnTestKeyUp：钩子已切换则跳过，否则由 TSF 路径切换。
    fn tsf_keyup(sw: &mut InputModeSwitch, hook_toggled: &mut bool) -> Option<bool> {
        if *hook_toggled {
            *hook_toggled = false;
            None
        } else {
            sw.ctrl_released()
        }
    }

    #[test]
    fn hook_consumes_then_tsf_skips_single_toggle() {
        let mut sw = InputModeSwitch::default();
        let mut hook_toggled = false;

        // Ctrl 按下：两条路径都标记候选（幂等）
        sw.ctrl_pressed();
        sw.ctrl_pressed();

        // Ctrl 松开：低层钩子先于 TSF 回调，钩子路径消费候选并置位标志
        assert_eq!(hook_release(&mut sw, &mut hook_toggled), Some(true));
        assert!(hook_toggled);

        // TSF OnTestKeyUp 看到 hook_toggled，跳过，不再切换
        assert_eq!(tsf_keyup(&mut sw, &mut hook_toggled), None);
        assert!(!hook_toggled);
        // 恰好一次切换（英文）
        assert!(sw.is_english());

        // 完整往返：再次 Ctrl 周期后应回到中文
        sw.ctrl_pressed();
        hook_toggled = false;
        assert_eq!(hook_release(&mut sw, &mut hook_toggled), Some(false));
        assert_eq!(tsf_keyup(&mut sw, &mut hook_toggled), None);
        assert!(!sw.is_english());
    }

    #[test]
    fn tsf_consumes_then_hook_sees_none_single_toggle() {
        let mut sw = InputModeSwitch::default();
        let mut hook_toggled = false;

        // Ctrl 按下
        sw.ctrl_pressed();

        // Ctrl 松开：TSF 路径先执行（标志为 false），由 TSF 消费并切换
        assert_eq!(tsf_keyup(&mut sw, &mut hook_toggled), Some(true));
        assert!(!hook_toggled);

        // 钩子随后执行：候选已被消费，返回 None，不置位标志、不重复切换
        assert_eq!(hook_release(&mut sw, &mut hook_toggled), None);
        assert!(!hook_toggled);
        assert!(sw.is_english());
    }

    #[test]
    fn hook_toggle_without_tsf_keyup_resets_on_next_press() {
        let mut sw = InputModeSwitch::default();
        let mut hook_toggled = false;

        // Chrome 等场景：TSF 收不到修饰键松开，仅钩子路径收到事件
        sw.ctrl_pressed();
        assert_eq!(hook_release(&mut sw, &mut hook_toggled), Some(true));
        // 无 TSF keyup 来消费标志，hook_toggled 残留为 true
        assert!(hook_toggled);

        // 新一轮 Ctrl 按下：OnTestKeyDown / on_ctrl_pressed 重置标志，
        // 避免残留标志抑制本次合法切换
        sw.ctrl_pressed();
        hook_toggled = false;

        // 新一轮 Ctrl 松开：TSF 路径正常切换回中文
        assert_eq!(tsf_keyup(&mut sw, &mut hook_toggled), Some(false));
        assert!(!sw.is_english());
    }

    #[test]
    fn stale_flag_does_not_suppress_next_toggle() {
        let mut sw = InputModeSwitch::default();
        // 模拟残留的陈旧标志（钩子切换后 TSF 从未收到 keyup 消费标志）
        let mut hook_toggled = true;

        // 新一轮 Ctrl 按下必须重置标志（service.rs OnTestKeyDown 与
        // hook.rs on_ctrl_pressed 都会执行此重置）
        sw.ctrl_pressed();
        assert!(hook_toggled); // 确认残留标志确实存在，随后被按下路径重置
        hook_toggled = false;

        // 松开时 TSF 路径应正常切换，而不是被陈旧标志吞掉
        assert_eq!(tsf_keyup(&mut sw, &mut hook_toggled), Some(true));
        assert!(sw.is_english());
    }

    // ------------------------------------------------------------------
    // 根据光标周围文本推断输入模式（suggest_input_mode）
    // ------------------------------------------------------------------

    #[test]
    fn suggest_preceding_chinese_returns_chinese() {
        assert_eq!(
            suggest_input_mode(Some("你好"), None),
            ModeSuggestion::Chinese
        );
    }

    #[test]
    fn suggest_preceding_english_trailing_space_returns_english() {
        // 末尾空格跳过，命中 'd' → 英文
        assert_eq!(
            suggest_input_mode(Some("hello world "), None),
            ModeSuggestion::English
        );
    }

    #[test]
    fn suggest_preceding_chinese_punct_returns_chinese() {
        // 中文标点（全角逗号）是中文信号
        assert_eq!(
            suggest_input_mode(Some("你好，"), None),
            ModeSuggestion::Chinese
        );
    }

    #[test]
    fn suggest_blank_preceding_falls_back_to_following() {
        // 前文仅空白无信号，扫描后文命中英文
        assert_eq!(
            suggest_input_mode(Some("   "), Some("abc")),
            ModeSuggestion::English
        );
    }

    #[test]
    fn suggest_no_signal_returns_none() {
        assert_eq!(suggest_input_mode(None, None), ModeSuggestion::Neutral);
        assert_eq!(
            suggest_input_mode(Some("  \t "), None),
            ModeSuggestion::Neutral
        );
        assert_eq!(
            suggest_input_mode(Some(" "), Some("  ")),
            ModeSuggestion::Neutral
        );
    }

    #[test]
    fn suggest_preceding_alnum_returns_english() {
        // 从末尾向前扫：数字中性跳过，命中 'c'（字母）→ 英文
        assert_eq!(
            suggest_input_mode(Some("abc123"), None),
            ModeSuggestion::English
        );
    }

    #[test]
    fn suggest_preceding_digits_only_is_neutral() {
        // 纯数字无强信号 → DigitsOnly（数字不认定为英文，不触发切换）
        assert_eq!(
            suggest_input_mode(Some("123"), None),
            ModeSuggestion::DigitsOnly
        );
        assert_eq!(
            suggest_input_mode(Some(" 42 "), None),
            ModeSuggestion::DigitsOnly
        );
    }

    #[test]
    fn suggest_digits_skip_to_chinese_signal() {
        // 数字跳过后命中 CJK → 中文（数字不吞掉中文信号）
        assert_eq!(
            suggest_input_mode(Some("章节3"), None),
            ModeSuggestion::Chinese
        );
    }

    #[test]
    fn suggest_digits_before_following_signal() {
        // 前文纯数字（弱中性）、后文命中字母：强信号优先于数字弱中性
        assert_eq!(
            suggest_input_mode(Some("123"), Some("abc")),
            ModeSuggestion::English
        );
        // 两侧都只有数字：DigitsOnly
        assert_eq!(
            suggest_input_mode(Some("12"), Some("34")),
            ModeSuggestion::DigitsOnly
        );
    }

    #[test]
    fn suggest_preceding_mixed_chinese_returns_chinese() {
        // 从末尾向前扫：空格跳过，命中 '章'（CJK）→ 中文
        assert_eq!(
            suggest_input_mode(Some("第 3 章 "), None),
            ModeSuggestion::Chinese
        );
    }

    #[test]
    fn suggest_neutral_chars_are_skipped() {
        // 末尾空格跳过、emoji 中性跳过，命中 'o' → 英文
        assert_eq!(
            suggest_input_mode(Some("hello 🎉 "), None),
            ModeSuggestion::English
        );
    }

    // ------------------------------------------------------------------
    // 自动切换状态机（AutoModeSwitch）
    // ------------------------------------------------------------------

    #[test]
    fn auto_unlocked_switches_on_different_suggestion() {
        let mut sw = AutoModeSwitch::default();
        // 建议与当前不同 → 自动切换
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, true), Some(false));
        // 建议与当前相同 → 不动作
        assert_eq!(sw.evaluate(ModeSuggestion::English, true), None);
        // 数字弱中性 / 无信号且当前为中文 → 不动作
        assert_eq!(sw.evaluate(ModeSuggestion::DigitsOnly, false), None);
        assert_eq!(sw.evaluate(ModeSuggestion::Neutral, false), None);
    }

    #[test]
    fn auto_digits_only_never_switches() {
        let mut sw = AutoModeSwitch::default();
        // 数字弱中性在英文模式下不触发英→中默认回切（用户输入 123 不被打断）
        assert_eq!(sw.evaluate(ModeSuggestion::DigitsOnly, true), None);
        // 中文模式下也不触发中→英切换（数字不认定为英文）
        assert_eq!(sw.evaluate(ModeSuggestion::DigitsOnly, false), None);
    }

    #[test]
    fn auto_manual_lock_blocks_same_context_and_unlocks_on_change() {
        let mut sw = AutoModeSwitch::default();

        // 用户在英文语境（建议=英文，当前已是英文）下手动切换
        assert_eq!(sw.evaluate(ModeSuggestion::English, true), None);
        sw.lock_manual(ModeSuggestion::English);

        // 锁定生效：同语境建议不再撤销用户选择（当前中文也不自动切回英文）
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), None);

        // 语境变化（建议变为中文）→ 解锁；建议与当前相同，不动作
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, false), None);

        // 已解锁：恢复自动切换
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));
    }

    #[test]
    fn auto_no_signal_defaults_back_to_chinese_when_english() {
        let mut sw = AutoModeSwitch::default();
        // 中立语境（无信号）且当前为英文 → 默认回到中文
        assert_eq!(sw.evaluate(ModeSuggestion::Neutral, true), Some(false));
        // 中立语境且当前为中文 → 保持中文不动作
        assert_eq!(sw.evaluate(ModeSuggestion::Neutral, false), None);
    }

    #[test]
    fn auto_suppress_only_first_zh_to_en_after_commit() {
        let mut sw = AutoModeSwitch::default();

        // 上屏英文编码后置位单次抑制：紧随其后的首次中→英评估被吞掉
        sw.suppress_next_zh_to_en();
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), None);

        // 抑制已消费（单次）：后续中→英自动切换恢复正常
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));

        // 再上屏一次：再次抑制一次，之后恢复
        sw.suppress_next_zh_to_en();
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), None);
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));
    }

    #[test]
    fn auto_suppress_cleared_on_manual_lock_and_chinese_context() {
        let mut sw = AutoModeSwitch::default();

        // 置位抑制后语境变为中文：抑制提前解除（已无意义）
        sw.suppress_next_zh_to_en();
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, false), None);
        // 解除后中→英自动切换不再受影响
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));

        // 置位抑制后用户手动切换：手动锁定覆盖抑制
        sw.suppress_next_zh_to_en();
        sw.lock_manual(ModeSuggestion::English);
        // 锁定生效（同语境建议不动作），且抑制已被清除：
        // 语境变化解锁后立即恢复自动切换
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), None);
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, false), None);
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));
    }

    #[test]
    fn auto_suppress_keeps_manual_lock_behavior() {
        let mut sw = AutoModeSwitch::default();

        // 手动锁定与单次抑制并存：手动锁定优先语义不受抑制影响
        sw.lock_manual(ModeSuggestion::Chinese);
        sw.suppress_next_zh_to_en();
        // 锁定期间同语境建议不动作（锁定优先于抑制评估，抑制不被消费）
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, true), None);
        // 语境变化 → 解锁；中文模式下该英文建议触发的中→英评估被单次抑制吞掉
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), None);
        // 抑制消费完毕：恢复正常自动切换
        assert_eq!(sw.evaluate(ModeSuggestion::English, false), Some(true));
    }

    #[test]
    fn auto_manual_english_lock_survives_no_signal() {
        let mut sw = AutoModeSwitch::default();
        // 用户在中立语境手动切到英文并锁定（基线 Neutral）
        sw.lock_manual(ModeSuggestion::Neutral);
        // 同语境（无信号）不再撤销手动选择
        assert_eq!(sw.evaluate(ModeSuggestion::Neutral, true), None);
        // 语境变化（出现中文信号）→ 解锁并切回中文
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, true), Some(false));
    }

    #[test]
    fn auto_manual_lock_uses_fresh_baseline_not_stale_suggestion() {
        let mut sw = AutoModeSwitch::default();

        // 此前在英文语境评估过（建议=英文），随后用户移动到中文语境
        assert_eq!(sw.evaluate(ModeSuggestion::English, true), None);
        // 手动切换为英文：以手动切换时刻采样的当前语境（中文建议）为锁定基线
        sw.lock_manual(ModeSuggestion::Chinese);

        // 同语境（中文建议）不再撤销用户选择：保持英文不自动切回中文
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, true), None);

        // 语境变化（建议变为英文）→ 解锁；建议与当前相同，不动作
        assert_eq!(sw.evaluate(ModeSuggestion::English, true), None);

        // 已解锁：恢复自动切换
        assert_eq!(sw.evaluate(ModeSuggestion::Chinese, true), Some(false));
    }
}
