//! Linux IBus Engine DBus 服务实现
//!
//! 基于 zbus 实现 `org.freedesktop.IBus.Engine` 接口，通过 daemon 提供的
//! channel 与引擎线程通信处理按键，并通过 `black_hole_ui` 显示候选窗口。

use black_hole_shared::{
    AutoModeSwitch, EngineCommand, InputContext, InputModeSwitch, KeyEvent, KeyState,
    ModeSuggestion, Modifiers, RuntimeSettings, SchemeResult, UiCommand, suggest_input_mode,
};
use std::future::pending;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use tracing::info;
use zbus::zvariant::{Array, Dict, OwnedValue, Structure, StructureBuilder, Type, Value};
use zbus::{Connection, Error, interface};

use super::{PlatformError, PlatformIme};

pub mod auto_register;

const ENGINE_PATH: &str = "/org/freedesktop/IBus/Engine/Black-Hole";
const ENGINE_IFACE: &str = "org.freedesktop.IBus.Engine";

/// Mutex poison 恢复加锁：panic 后继续使用内部值
fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Linux IBus 输入法平台实现
pub struct LinuxIbusIme {
    /// daemon 共享的运行时设置（方案/主题/中英模式/自动切换开关），
    /// auto_switch 字段为"根据光标周围文本自动切换中英模式"开关，支持热更新
    current_settings: Arc<Mutex<RuntimeSettings>>,
}

impl LinuxIbusIme {
    pub fn new(current_settings: Arc<Mutex<RuntimeSettings>>) -> Self {
        Self { current_settings }
    }
}

impl PlatformIme for LinuxIbusIme {
    fn run(
        &mut self,
        engine_tx: Sender<EngineCommand>,
        platform_rx: Receiver<SchemeResult>,
        ui_tx: Sender<UiCommand>,
    ) -> Result<(), PlatformError> {
        // IBus Engine DBus 服务，通过 daemon 提供的 channel 与引擎线程通信
        pollster::block_on(self.run_async(engine_tx, platform_rx, ui_tx))
            .map_err(|e| PlatformError::Dbus(e.to_string()))?;

        Ok(())
    }
}

impl LinuxIbusIme {
    async fn run_async(
        &self,
        engine_tx: Sender<EngineCommand>,
        platform_rx: Receiver<SchemeResult>,
        ui_tx: Sender<UiCommand>,
    ) -> Result<(), Error> {
        let conn = Connection::session().await?;

        let ibus_engine = IbusEngine {
            engine_tx: Mutex::new(engine_tx),
            platform_rx: Mutex::new(platform_rx),
            ui_tx: Mutex::new(ui_tx),
            context: Mutex::new(InputContext::default()),
            mode_switch: Mutex::new(InputModeSwitch::default()),
            auto_mode: Mutex::new(AutoModeSwitch::default()),
            last_code: Mutex::new(None),
            caret_reported: AtomicBool::new(false),
            badge_visible: AtomicBool::new(false),
            current_settings: Arc::clone(&self.current_settings),
            conn: conn.clone(),
        };

        conn.object_server()
            .at("/org/freedesktop/IBus/Engine/Black-Hole", ibus_engine)
            .await?;

        // 保持连接活跃
        pending::<()>().await;
        Ok(())
    }
}

/// IBus Engine DBus 对象
///
/// 通过 channel 与 daemon 中的引擎线程通信。
struct IbusEngine {
    engine_tx: Mutex<Sender<EngineCommand>>,
    platform_rx: Mutex<Receiver<SchemeResult>>,
    ui_tx: Mutex<Sender<UiCommand>>,
    context: Mutex<InputContext>,
    /// 中英文模式切换状态机（Ctrl 键触发）
    mode_switch: Mutex<InputModeSwitch>,
    /// 根据光标周围文本自动切换中英模式的状态机
    /// （用户手动切换后锁定语境基线，语境变化后自动解锁）
    auto_mode: Mutex<AutoModeSwitch>,
    /// 最近一次 composition 的编码（切英文模式时上屏保留）
    last_code: Mutex<Option<String>>,
    /// 本焦点会话内光标坐标是否已上报（set_cursor_location）：模式切换
    /// Show 的"尚无光标"判定与 UpdatePosition 去重依据
    caret_reported: AtomicBool,
    /// 徽标按发送端视角是否可见：Show 置真、Hide 置假，英文直输 Hide 的
    /// 源侧去重依据（已隐藏时不再逐键发 IPC）
    badge_visible: AtomicBool,
    /// daemon 共享的运行时设置（方案/主题/中英模式/自动切换开关），
    /// auto_switch 字段为自动切换开关，每次评估时读取
    current_settings: Arc<Mutex<RuntimeSettings>>,
    conn: Connection,
}

impl IbusEngine {
    async fn emit_engine_signal(&self, signal: &str, body: &(impl Type + serde::Serialize)) {
        let _ = self
            .conn
            .emit_signal(None::<&str>, ENGINE_PATH, ENGINE_IFACE, signal, body)
            .await;
    }

    /// 模式切换后的收尾：切英文模式时先上屏保留输入框内容，
    /// 然后重置引擎并向面板广播新的 InputMode 属性。
    async fn apply_mode_change(&self, english: bool, report: UiCommand) {
        // 上报 daemon 更新共享状态（手动/自动切换路径各传对应变体），否则
        // 徽标与其它进程同步的模式脱节
        let _ = locked(&self.ui_tx).send(report);
        // 切换到英文模式时，把输入框中的编码上屏，避免输入丢失。
        if english {
            let code = locked(&self.last_code).take();
            if let Some(code) = code.filter(|c| !c.is_empty()) {
                let variant = Value::from((code.as_str(), Vec::<(u32, u32, u32, u32)>::new()));
                self.emit_engine_signal("CommitText", &variant).await;
            }
        } else {
            *locked(&self.last_code) = None;
        }
        // 取消未完成的输入并重置引擎（幂等）；daemon 处理 Reset 时会自行
        // 隐藏候选窗——其 UI 命令同样会清掉徽标显隐，显式重显必须排在其后
        let sent = {
            let engine_tx = locked(&self.engine_tx);
            engine_tx.send(EngineCommand::Reset).is_ok()
        };
        if sent {
            let _ = locked(&self.platform_rx).recv();
            // recv() 即 Reset 处理完成的屏障：daemon 在同一守护线程上先向
            // UI 队列入队 Reset 的 HideCandidates、再回执本结果；下方 Show
            // 在收到回执后才入队同一 UI 队列。MPSC 保序，HideCandidates
            // 必然先于 Show 应用，无需额外同步
        }
        // 更新 InputMode 属性，供面板/桌面 shell 感知中英文状态。
        self.emit_engine_signal("UpdateProperty", &input_mode_property(english))
            .await;
        // 显式重显徽标（最后一条徽标相关命令）：UI 侧 SetInputMode 仅同步
        // 字样不改显隐（跨进程同步不得基于过期光标重显），切换方（必然
        // 持有焦点）用自身光标位置 Show；尚无光标上报时传 None，徽标屏外
        // 停放待光标就位。排在 Reset 回程之后，否则刚显示的徽标立即被
        // 同序应用的重置隐藏（评审 P2 修复）
        let context = self.caret_reported.load(Ordering::Relaxed).then(|| {
            // 只携带光标三元组：完整 InputContext 的 preceding_text（周围
            // 文本缓存）对 UI 无用，整体 clone 徒增 IPC 载荷
            let ctx = locked(&self.context);
            InputContext::caret(ctx.caret_x, ctx.caret_y, ctx.caret_h)
        });
        // 同一焦点会话内重显：未取到光标时沿用 UI 已记录的位置
        // （invalidate_caret=false），否则切换中英时徽标会被清成"位置未知"
        // 而屏外停放、用户看不到任何反馈
        let _ = locked(&self.ui_tx).send(UiCommand::ShowModeIndicator {
            context,
            invalidate_caret: false,
        });
        self.badge_visible.store(true, Ordering::Relaxed);
    }

    /// 根据光标周围文本自动评估并切换中英模式。
    ///
    /// 仅在自动切换开关开启（daemon 共享状态元组第四元，支持热更新）且
    /// 当前非合成态（无未上屏编码）时评估；命中建议时直接设置模式并复用
    /// `apply_mode_change` 完成上屏保留/重置引擎/更新属性收尾。
    async fn maybe_auto_switch_mode(&self) {
        if !locked(&self.current_settings).auto_switch {
            return;
        }
        // 合成中（输入框尚有编码）不评估，避免打断正在进行的输入
        if locked(&self.last_code)
            .as_ref()
            .is_some_and(|c| !c.is_empty())
        {
            return;
        }
        let (preceding, following) = {
            let ctx = locked(&self.context);
            (ctx.preceding_text.clone(), ctx.following_text.clone())
        };
        let suggestion = suggest_input_mode(preceding.as_deref(), following.as_deref());
        let target = {
            let current = locked(&self.mode_switch).is_english();
            locked(&self.auto_mode).evaluate(suggestion, current)
        };
        if let Some(english) = target {
            // evaluate 已确认目标与当前不同，set_english 必然产生切换
            locked(&self.mode_switch).set_english(english);
            info!(
                "Input mode auto switched: {}",
                if english { "英文" } else { "中文" }
            );
            self.apply_mode_change(english, UiCommand::SetInputModeTransient(english))
                .await;
        }
    }

    /// 以缓存的光标周围文本评估当前语境建议（手动切换时作为锁定基线，
    /// 必须即时采样：陈旧基线会导致下个按键即被自动切换撤销）
    fn current_context_suggestion(&self) -> ModeSuggestion {
        let ctx = locked(&self.context);
        suggest_input_mode(ctx.preceding_text.as_deref(), ctx.following_text.as_deref())
    }
}

#[interface(name = "org.freedesktop.IBus.Engine")]
impl IbusEngine {
    /// 处理按键事件
    /// 返回 true 表示按键被消费，false 表示转发给应用
    async fn process_key_event(&self, keyval: u32, _keycode: u32, state: u32) -> bool {
        const RELEASE_MASK: u32 = 1 << 30;
        const CONTROL_MASK: u32 = 1 << 2;
        // XK_Control_L / XK_Control_R
        const CONTROL_L: u32 = 0xFFE3;
        const CONTROL_R: u32 = 0xFFE4;

        // Ctrl 键：按下标记候选，松开切换中英文模式；不消费按键本身。
        if keyval == CONTROL_L || keyval == CONTROL_R {
            let toggled = {
                let mut mode = locked(&self.mode_switch);
                if state & RELEASE_MASK != 0 {
                    mode.ctrl_released()
                } else {
                    mode.ctrl_pressed();
                    None
                }
            };
            if let Some(english) = toggled {
                info!(
                    "Input mode toggled: {}",
                    if english { "英文" } else { "中文" }
                );
                // 用户手动切换：以即时采样的当前语境建议为锁定基线，
                // 同语境的自动建议不再撤销本次选择
                let suggestion = self.current_context_suggestion();
                locked(&self.auto_mode).lock_manual(suggestion);
                self.apply_mode_change(english, UiCommand::SetInputMode(english))
                    .await;
            }
            return false;
        }

        // 按住 Ctrl 期间按下其他键（如 Ctrl+C），取消切换候选。
        if state & CONTROL_MASK != 0 {
            locked(&self.mode_switch).other_key_pressed(true);
        }

        // 根据光标周围文本自动切换中英模式（开关开启且非合成态时评估一次）
        self.maybe_auto_switch_mode().await;

        // 英文模式下不消费任何按键；可打印字符直输属于"开始输入"，
        // 隐藏中英模式徽标（方向键/修饰键等非输入键不改变显隐）。
        // 判定覆盖 ASCII 与 Latin-1（该区间 keysym 与 Unicode 码点同值，
        // 含 KP 小键盘字符键）；keysym 高段（功能键 0xFFxx 等）不视为可打印
        if locked(&self.mode_switch).is_english() {
            // 可打印判定：Latin-1 段 keysym 与码点同值；非 Latin 文字的
            // legacy keysym 段取西里尔（0x6A1-0x6FF）与希腊（0x7A1-0x7F9）
            // 两个已分配区间（0x700-0x7A0 为未分配空洞，不得纳入）；
            // 0x1000000 段为 Unicode 直接编码（keysym = 0x1000000 + 码点），
            // 上界取 0x1110000 以覆盖到 U+10FFFF（合法标量终点）。
            // 另须排除 XF86 媒体/浏览器键段（0x1008FF00-0x1008FFFF，如音量/
            // 播放键）：其差值为 0x8FF00-0x8FFFF，属合法标量（Plane 8 未分配
            // 区），from_u32 判定会放行，不排除就会把媒体键当成字符键。
            // keysym 高段功能键（0xFFxx）不视为可打印
            let printable = ((0x20..=0xFF).contains(&keyval)
                && char::from_u32(keyval).is_some_and(|c| !c.is_control()))
                || (0x6A1..=0x6FF).contains(&keyval)
                || (0x7A1..=0x7F9).contains(&keyval)
                || ((0x1000000..0x1110000).contains(&keyval)
                    && !(0x1008FF00..=0x1008FFFF).contains(&keyval)
                    && char::from_u32(keyval - 0x1000000).is_some_and(|c| !c.is_control()));
            if printable && self.badge_visible.swap(false, Ordering::Relaxed) {
                // 英文直输隐藏：当前焦点仍在输入，保留光标位置；
                // 已隐藏时不再逐键发 IPC（swap 顺带置假完成源侧去重）
                let _ = locked(&self.ui_tx).send(UiCommand::HideModeIndicator {
                    invalidate_caret: false,
                });
            }
            return false;
        }

        let Some(key) = convert_keyval(keyval, state) else {
            return false;
        };

        // 发送 Key 命令到引擎线程
        if locked(&self.engine_tx)
            .send(EngineCommand::Key(key))
            .is_err()
        {
            return false;
        }

        // 从引擎线程接收处理结果
        let result = match locked(&self.platform_rx).recv() {
            Ok(r) => r,
            Err(_) => return false,
        };

        match result {
            SchemeResult::Committed {
                text,
                temporary_english,
            } => {
                // 已上屏，清空记录的编码，避免切换模式时重复上屏
                *locked(&self.last_code) = None;
                // 临时英文/原样上屏英文编码结束上屏后，抑制紧随其后的一次中→英
                // 自动切换：上屏文本立即使语境变为英文，若无抑制，用户敲下的
                // 下一个字母会在合成开始前被中→英自动切换拉进英文模式直输
                // （与 Windows 侧一致）。用单次抑制而非持久锁定——持久锁定在
                // 英文语境（如 IDE）持续存在时会吞掉后续所有自动切换。
                if temporary_english && locked(&self.current_settings).auto_switch {
                    locked(&self.auto_mode).suppress_next_zh_to_en();
                }
                let variant = Value::from((text.as_str(), Vec::<(u32, u32, u32, u32)>::new()));
                self.emit_engine_signal("CommitText", &variant).await;
                true
            }
            SchemeResult::Composing {
                code,
                candidates,
                selected_index,
                expanded,
            } => {
                // 记录当前编码，供切换英文模式时上屏保留
                *locked(&self.last_code) = Some(code.clone());
                let ui_tx = locked(&self.ui_tx);
                let ctx = locked(&self.context).clone();
                let _ = ui_tx.send(UiCommand::ShowCandidates {
                    code,
                    candidates,
                    selected_index,
                    expanded,
                    context: ctx,
                });
                true
            }
            SchemeResult::Cancelled => {
                // 取消输入（Esc / cancel 绑定）：消费按键，避免 IBus 把 Esc
                // 转发给宿主应用。daemon 处理 Cancelled 时已发 HideCandidates。
                *locked(&self.last_code) = None;
                true
            }
            SchemeResult::Ignored => false,
        }
    }

    /// 更新光标位置
    async fn set_cursor_location(&self, x: i32, y: i32, _w: i32, h: i32) {
        // 首次上报（本焦点会话）与坐标变化均视为位置更新；其余为应用重复
        // 上报同一位置，跳过 UI 通知（逐键 IPC + 强制重绘是纯开销）
        let mut changed = !self.caret_reported.swap(true, Ordering::Relaxed);
        // 合并更新：保留 set_surrounding_text 缓存的周围文本，仅刷新光标坐标；
        // 整体覆盖会清掉文本缓存，使自动切换中英模式评估失去依据
        let mut ctx = locked(&self.context).clone();
        changed |= ctx.caret_x != x || ctx.caret_y != y || ctx.caret_h != h;
        ctx.caret_x = x;
        ctx.caret_y = y;
        ctx.caret_h = h;
        *locked(&self.context) = ctx.clone();
        let _ = locked(&self.engine_tx).send(EngineCommand::SetContext(ctx));
        if changed {
            // 同步通知 UI（含中英模式徽标）光标已移动，使其持续跟随。
            // 通道为 MPSC（进程内直发 UI 渲染循环），不涉及 Windows 命名
            // 管道的字节合并问题，重复位置跳过仅是逐键开销优化
            let _ = locked(&self.ui_tx).send(UiCommand::UpdatePosition {
                context: InputContext::caret(x, y, h),
            });
        }
    }

    /// 应用推送的光标周围文本（IBus SetSurroundingText，IBus 1.5+）。
    ///
    /// `text` 为 variant 包裹的 IBusText（序列化结构
    /// `(s "IBusText", a{sv} attachments, s text, v attrs)`，纯文本在第 3 个字段），
    /// `cursor_pos` 为光标在文本中的字符索引。按光标位置切出前文/后文，
    /// 合并光标坐标后通过 SetContext 同步给 daemon（供整句补全提供上下文）。
    /// 解析失败或文本为空时静默跳过，不影响输入管线。
    async fn set_surrounding_text(&self, text: OwnedValue, cursor_pos: u32, _anchor_pos: u32) {
        const MAX_PRECEDING_CHARS: usize = 32;
        const MAX_FOLLOWING_CHARS: usize = 16;

        // IBusText 序列化为 (s "IBusText", a{sv} attachments, s text, v attrs)，
        // 纯文本位于 index 2。
        let Ok(structure) = text.downcast_ref::<&Structure>() else {
            return;
        };
        let fields = structure.fields();
        let Some(Value::Str(text_str)) = fields.get(2) else {
            return;
        };
        if text_str.is_empty() {
            return;
        }

        // cursor_pos 是字符索引，换算成字节偏移后切分前文/后文
        let cursor_byte = text_str
            .char_indices()
            .nth(cursor_pos as usize)
            .map(|(i, _)| i)
            .unwrap_or(text_str.len());
        let (pre, post) = text_str.split_at(cursor_byte);
        // 前文取光标前最后 MAX_PRECEDING_CHARS 个字符，后文取光标后前 MAX_FOLLOWING_CHARS 个字符
        let preceding: String = pre
            .chars()
            .rev()
            .take(MAX_PRECEDING_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let following: String = post.chars().take(MAX_FOLLOWING_CHARS).collect();

        // 保留 set_cursor_location 写入的光标坐标，仅更新文本字段；
        // 同时缓存到 self.context，供自动切换中英模式评估时读取周围文本
        let mut ctx = locked(&self.context).clone();
        ctx.preceding_text = (!preceding.is_empty()).then_some(preceding);
        ctx.following_text = (!following.is_empty()).then_some(following);
        *locked(&self.context) = ctx.clone();
        let _ = locked(&self.engine_tx).send(EngineCommand::SetContext(ctx));
    }

    /// 声明引擎需要周围文本（IBus 1.5.27+ 读取此属性，焦点变化时自动推送
    /// SetSurroundingText）。
    #[zbus(property, name = "active-surrounding-text")]
    async fn active_surrounding_text(&self) -> bool {
        true
    }

    async fn focus_in(&self) {
        // 新焦点窗口的光标位置未知：重置上报标记（否则模式切换 Show 会
        // 携带上一窗口的过期坐标重显）；聚焦输入区即显示中英模式徽标
        // （输入前可见），光标坐标随后由 set_cursor_location 上报，
        // 未到前徽标屏外停放
        self.caret_reported.store(false, Ordering::Relaxed);
        // 焦点会话已变更：作废旧窗口的光标位置，屏外停放待本次会话上报
        let _ = locked(&self.ui_tx).send(UiCommand::ShowModeIndicator {
            context: None,
            invalidate_caret: true,
        });
        self.badge_visible.store(true, Ordering::Relaxed);
    }

    async fn focus_out(&self) {
        // 焦点离开输入区：隐藏徽标并作废光标位置（焦点已失效）；
        // 重置上报标记，下一焦点会话重新等待光标就位
        self.caret_reported.store(false, Ordering::Relaxed);
        let _ = locked(&self.ui_tx).send(UiCommand::HideModeIndicator {
            invalidate_caret: true,
        });
        self.badge_visible.store(false, Ordering::Relaxed);
    }

    async fn reset(&self) {
        // 引擎已重置，清空记录的编码
        *locked(&self.last_code) = None;
        // 发送 Reset 命令到引擎线程并读取响应
        if locked(&self.engine_tx).send(EngineCommand::Reset).is_err() {
            return;
        }
        let _ = locked(&self.platform_rx).recv();
    }

    async fn enable(&self) {
        // 声明需要周围文本：IBus 1.5.27+ 的 daemon 读取 active-surrounding-text
        // 属性后，会在每次焦点切换时自动推送 SetSurroundingText。
        // 旧版 IBus 无此属性，改用 RequireSurroundingText 信号通知 daemon。
        self.emit_engine_signal("RequireSurroundingText", &()).await;
        // 注册 InputMode 属性，使面板/桌面 shell 能显示并跟踪中英文状态。
        let english = locked(&self.mode_switch).is_english();
        self.emit_engine_signal("RegisterProperties", &input_mode_prop_list(english))
            .await;
    }
    async fn disable(&self) {}
    async fn set_capabilities(&self, _caps: u32) {}
    async fn page_up(&self) -> bool {
        false
    }
    async fn page_down(&self) -> bool {
        false
    }
    async fn cursor_up(&self) -> bool {
        false
    }
    async fn cursor_down(&self) -> bool {
        false
    }
    async fn property_activate(&self, prop_name: String, prop_state: i32) {
        if prop_name != INPUT_MODE_PROP_KEY {
            return;
        }
        // 面板点击 InputMode 属性：CHECKED=中文，UNCHECKED=英文
        let toggled =
            locked(&self.mode_switch).set_english(prop_state == PROP_STATE_UNCHECKED as i32);
        if let Some(english) = toggled {
            info!(
                "Input mode toggled via property: {}",
                if english { "英文" } else { "中文" }
            );
            // 用户手动切换：以即时采样的当前语境建议为锁定基线，
            // 同语境的自动建议不再撤销本次选择
            let suggestion = self.current_context_suggestion();
            locked(&self.auto_mode).lock_manual(suggestion);
            self.apply_mode_change(english, UiCommand::SetInputMode(english))
                .await;
        }
    }
    async fn candidate_clicked(&self, _index: u32, _button: u32, _state: u32) {}
}

/// 将 IBus keyval + state 转换为内部 KeyEvent
fn convert_keyval(keyval: u32, state: u32) -> Option<KeyEvent> {
    let key = match keyval {
        0xFF08 => "Backspace".to_string(),
        0xFF0D => "Enter".to_string(),
        0xFF1B => "Escape".to_string(),
        0xFF09 => "Tab".to_string(),
        0x0020 => "Space".to_string(),
        0xFF52 => "ArrowUp".to_string(),
        0xFF54 => "ArrowDown".to_string(),
        0xFF51 => "ArrowLeft".to_string(),
        0xFF53 => "ArrowRight".to_string(),
        0x30..=0x39 => ((keyval as u8 - 0x30 + b'0') as char).to_string(),
        0x41..=0x5A => ((keyval as u8 - 0x41 + b'a') as char).to_string(),
        0x61..=0x7A => ((keyval as u8 - 0x61 + b'a') as char).to_string(),
        0x3B => ";".to_string(),
        0x21..=0x2F | 0x3A..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E => (keyval as u8 as char).to_string(),
        _ => return None,
    };

    let shift = (state & 1) != 0;
    let ctrl = (state & 4) != 0;
    let alt = (state & 8) != 0;
    let meta = (state & 0x10000000) != 0;

    Some(KeyEvent {
        key,
        modifiers: Modifiers {
            shift,
            ctrl,
            alt,
            meta,
            capslock: false,
        },
        state: KeyState::Press,
    })
}

// ---------------------------------------------------------------------------
// IBus 属性序列化（与 libibus 的 GVariant 格式逐字段一致）
//
// libibus 将 IBusSerializable 序列化为 tuple：首元素为类型名（如
// "IBusProperty"），随后是 attachments（a{sv}，恒为空），再按各子类
// 定义顺序追加字段。RegisterProperties / UpdateProperty 信号参数均为
// 包裹该 tuple 的 variant（见 ibus 源码 ibusserializable.c /
// ibusproperty.c / ibustext.c / ibusattrlist.c / ibusproplist.c /
// ibusengine.c）。
// ---------------------------------------------------------------------------

/// IBus 属性类型/状态常量（见 ibus 源码 ibustypes.h）
const PROP_TYPE_TOGGLE: u32 = 1;
const PROP_STATE_UNCHECKED: u32 = 0;
const PROP_STATE_CHECKED: u32 = 1;

/// 供面板/桌面 shell 感知中英文模式的属性 key
const INPUT_MODE_PROP_KEY: &str = "InputMode";

/// 空 attachments（a{sv}）
fn empty_attachments() -> Value<'static> {
    Value::Dict(Dict::new(<&str>::SIGNATURE, <Value>::SIGNATURE))
}

/// 序列化空的 IBusAttrList / IBusPropList：(s 类型名, a{sv}, av)
fn empty_named_container(class_name: &'static str) -> Value<'static> {
    Value::Structure(
        StructureBuilder::new()
            .add_field(class_name)
            .append_field(empty_attachments())
            .append_field(Value::Array(Array::new(<Value>::SIGNATURE)))
            .build()
            .expect("non-empty structure"),
    )
}

/// 序列化 IBusText：(s "IBusText", a{sv}, s text, v IBusAttrList)
fn ibus_text(text: &'static str) -> Value<'static> {
    Value::Structure(
        StructureBuilder::new()
            .add_field("IBusText")
            .append_field(empty_attachments())
            .add_field(text)
            .append_field(Value::Value(Box::new(empty_named_container(
                "IBusAttrList",
            ))))
            .build()
            .expect("non-empty structure"),
    )
}

/// 序列化 InputMode 属性并以 variant 包裹（UpdateProperty 信号参数）。
///
/// IBusProperty 字段顺序：key(s) type(u) label(v) icon(s) tooltip(v)
/// sensitive(b) visible(b) state(u) sub_props(v) symbol(v)。
/// 约定：中文=CHECKED（label "中"），英文=UNCHECKED（label "英"）。
fn input_mode_property(english: bool) -> Value<'static> {
    let (label, tooltip, state) = if english {
        ("英", "英文输入模式", PROP_STATE_UNCHECKED)
    } else {
        ("中", "中文输入模式", PROP_STATE_CHECKED)
    };
    let prop = StructureBuilder::new()
        .add_field("IBusProperty")
        .append_field(empty_attachments())
        .add_field(INPUT_MODE_PROP_KEY)
        .add_field(PROP_TYPE_TOGGLE)
        .append_field(Value::Value(Box::new(ibus_text(label))))
        .add_field("")
        .append_field(Value::Value(Box::new(ibus_text(tooltip))))
        .add_field(true)
        .add_field(true)
        .add_field(state)
        .append_field(Value::Value(Box::new(empty_named_container(
            "IBusPropList",
        ))))
        .append_field(Value::Value(Box::new(ibus_text(label))))
        .build()
        .expect("non-empty structure");
    Value::Value(Box::new(Value::Structure(prop)))
}

/// 序列化仅含 InputMode 属性的 IBusPropList 并以 variant 包裹
/// （RegisterProperties 信号参数）。列表元素同样是 variant 包裹的属性。
fn input_mode_prop_list(english: bool) -> Value<'static> {
    let mut props = Array::new(<Value>::SIGNATURE);
    props
        .append(input_mode_property(english))
        .expect("element signature matches");
    let list = StructureBuilder::new()
        .add_field("IBusPropList")
        .append_field(empty_attachments())
        .append_field(Value::Array(props))
        .build()
        .expect("non-empty structure");
    Value::Value(Box::new(Value::Structure(list)))
}
