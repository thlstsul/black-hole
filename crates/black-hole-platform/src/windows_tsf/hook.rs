//! WH_KEYBOARD_LL 全局键盘钩子：检测单独 Ctrl 键的按下/松开并切换中英文模式。
//!
//! Chrome 等应用不把修饰键（以及部分组合键）的按下/松开事件转发给 TSF 的
//! `ITfKeyEventSink`，导致基于 `ITfKeystrokeMgr` 的 Ctrl 切换在 Chrome 中失效。
//! 这里改用系统级低层键盘钩子直接读取物理按键状态，绕过应用的 TSF 转发行为。
//!
//! 语义与 TSF 路径一致：按下 Ctrl 标记切换候选；按住 Ctrl 期间按下其他键
//! （如 Ctrl+C、Ctrl+Shift）取消候选；单独松开 Ctrl 时切换中英文模式。
//! 本钩子只观察按键、从不消费按键本身，保证 Ctrl+C 等组合键正常工作。
//!
//! 钩子回调运行在安装钩子的线程（Activate 所在的应用 UI 线程，自带消息循环）上，
//! 按"实例级前台焦点标志 → 前台窗口所属线程"的顺序在 `ACTIVE_SERVICES`
//! 注册表中定位**当前真正持有输入焦点**的那个服务实例并切换。
//!
//! WH_KEYBOARD_LL 是全局钩子：每个装有本输入法的进程都会被系统投递每一次
//! 按键事件。因此**本进程不在前台时绝不能切换自己的实例**（否则同一次 Ctrl
//! 会被前后台两个实例各切一次，两边模式相位不同时徽标就与正在输入的实例
//! 相反，后台实例的 Show 还会抢走 daemon 的"聚焦连接"）。选择规则见
//! [`pick_toggle_target`]。

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Arc, Mutex, Weak};

use windows::Win32::System::Threading::GetCurrentThreadId;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LCONTROL, VK_RCONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, HHOOK, KBDLLHOOKSTRUCT,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
};

use super::service::{apply_input_mode_toggle, caret_context_for_show};
use super::{ServiceInner, locked};
use super::{
    gesture_consume_release, gesture_ctrl_pressed, gesture_other_key_pressed, send_ui_command_inner,
};
use black_hole_shared::UiCommand;
use tracing::{debug, warn};

/// 当前进程内所有已激活的文本服务实例（线程 id → 弱引用）。
/// 钩子回调据此定位持有输入焦点的服务实例（见 [`pick_toggle_target`]）。
static ACTIVE_SERVICES: Mutex<Option<HashMap<u32, Weak<Mutex<ServiceInner>>>>> = Mutex::new(None);
/// 已安装的钩子句柄（进程内只安装一次），用 usize 存储避免裸指针的 Send/Sync 问题。
static HOOK: Mutex<Option<usize>> = Mutex::new(None);
/// 当前进程内 Ctrl 是否处于按下状态，用于过滤按住时的自动重复按下事件。
static CTRL_DOWN: Mutex<bool> = Mutex::new(false);
/// 进程内持有 TSF 键盘前台焦点的线程集合，以及最近一次获得焦点的线程。
///
/// 由 service.rs 的 `ITfKeyEventSink::OnSetFocus` 维护（收到焦点时加入集合，
/// 失去焦点时**仅移除本线程**），钩子回调据此选择切换目标（见
/// [`pick_toggle_target`]）。
///
/// 用"集合 + 最近者"而不是单值：进程内可能有多个文本服务实例（多窗口/
/// 多线程），别的实例的失焦回调不得抹掉当前聚焦实例的记录。这份状态只被
/// 焦点回调与钩子回调短暂持锁读写，**绝不与 `ServiceInner` 的锁嵌套**——
/// 低层键盘钩子回调里若等待按键路径持有的 `ServiceInner` 锁（按键路径会
/// 持锁跨同步 IPC），会拖慢甚至卡住宿主输入。
#[derive(Default)]
struct FocusState {
    /// 最近一次获得 TSF 键盘前台焦点的线程 id
    latest: Option<u32>,
    /// 当前自称持有 TSF 键盘前台焦点的线程集合（多实例进程的兜底依据）
    claimed: HashSet<u32>,
}

static FOCUS: Mutex<Option<FocusState>> = Mutex::new(None);

/// 记录某线程获得 TSF 键盘前台焦点（ITfKeyEventSink::OnSetFocus 收到
/// 前台焦点时调用）。
pub(crate) fn set_foreground_thread(thread_id: u32) {
    debug!("hook: set_foreground_thread thread_id={}", thread_id);
    if let Ok(mut guard) = FOCUS.lock() {
        let state = guard.get_or_insert_with(FocusState::default);
        state.claimed.insert(thread_id);
        state.latest = Some(thread_id);
    }
}

/// 移除某线程的 TSF 键盘前台焦点记录（OnSetFocus 失去前台焦点时调用）。
///
/// 只移除本线程：进程内其它实例（窗口/线程）的失焦回调不得抹掉当前聚焦
/// 实例的记录——记录被抹掉后钩子路径会退回"前台窗口线程 / 唯一实例"的
/// 近似选择，可能选中与 TSF 按键路径不同的实例，同一次 Ctrl 被两个实例
/// 各切一次（模式相位不同的两个实例各自上报模式/Show，徽标就与真正在输入
/// 的实例相反）。
pub(crate) fn clear_foreground_thread(thread_id: u32) {
    if let Ok(mut guard) = FOCUS.lock()
        && let Some(state) = guard.as_mut()
    {
        state.claimed.remove(&thread_id);
        if state.latest == Some(thread_id) {
            // 兜底：仍有其它实例自称前台时改指向它（正常情况下集合至多一个）
            state.latest = state.claimed.iter().copied().next();
        }
        debug!(
            "hook: clear_foreground_thread thread_id={} latest={:?} claimed={:?}",
            thread_id, state.latest, state.claimed
        );
    }
}

/// 读取焦点状态快照（`(最近聚焦线程, 自称前台线程集合)`）。
/// 仅持 [`FOCUS`] 自身的锁，不触碰任何服务实例的锁。
fn focus_snapshot() -> (Option<u32>, HashSet<u32>) {
    match FOCUS.lock() {
        Ok(guard) => guard
            .as_ref()
            .map(|state| (state.latest, state.claimed.clone()))
            .unwrap_or_default(),
        Err(_) => (None, HashSet::new()),
    }
}

/// 注册一个已激活的服务实例（Activate 时调用），并确保钩子已安装。
pub(crate) fn register_service(thread_id: u32, inner: Arc<Mutex<ServiceInner>>) {
    debug!("hook: register_service thread_id={}", thread_id);
    if let Ok(mut guard) = ACTIVE_SERVICES.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(thread_id, Arc::downgrade(&inner));
    }
    install_hook();
}

/// 注销服务实例（Deactivate 时调用）；若无剩余实例则卸载钩子。
pub(crate) fn unregister_service(thread_id: u32) {
    let empty = {
        let mut guard = locked(&ACTIVE_SERVICES);
        if let Some(map) = guard.as_mut() {
            map.remove(&thread_id);
            map.is_empty()
        } else {
            true
        }
    };
    if empty
        && let Ok(mut hook_guard) = HOOK.lock()
        && let Some(raw) = hook_guard.take()
    {
        let hook = HHOOK(raw as *mut c_void);
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
    }
}

/// 安装 WH_KEYBOARD_LL 钩子（进程内幂等）。
fn install_hook() {
    let mut guard = match HOOK.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if guard.is_some() {
        return;
    }
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            Option::<HINSTANCE>::None,
            0,
        )
    };
    match hook {
        Ok(h) => {
            debug!("WH_KEYBOARD_LL hook installed");
            *guard = Some(h.0 as usize);
        }
        Err(e) => warn!("Failed to install WH_KEYBOARD_LL hook: {}", e),
    }
}

/// 判断虚拟键是否为 Ctrl（含左右键）。
fn is_ctrl_vk(vk: u32) -> bool {
    vk == VK_CONTROL.0 as u32 || vk == VK_LCONTROL.0 as u32 || vk == VK_RCONTROL.0 as u32
}

/// 低层键盘钩子回调：观察 Ctrl 按下/松开与组合键，切换中英文模式。
/// 从不消费任何按键（始终转发给下一个钩子）。
unsafe extern "system" fn keyboard_hook_proc(
    ncode: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // ncode < 0 时必须无条件转发给下一个钩子。
    if ncode >= 0 {
        // SAFETY: 低层钩子回调的 lparam 保证指向有效的 KBDLLHOOKSTRUCT。
        let msg = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let vk = msg.vkCode;
        let event = wparam.0 as u32;
        // 诊断日志：确认钩子回调是否被系统投递到本进程
        // （若设置面板内按 Ctrl 时这里没有任何输出，说明回调未被投递）。
        debug!(
            "hook: proc ncode={} vk=0x{:04X} event=0x{:08X}",
            ncode, vk, event
        );

        if is_ctrl_vk(vk) && (event == WM_KEYDOWN || event == WM_KEYUP) {
            let down = event == WM_KEYDOWN;
            let mut ctrl_down = locked(&CTRL_DOWN);
            if down && !*ctrl_down {
                // 首次按下（自动重复的按下不再重复标记候选）。
                *ctrl_down = true;
                drop(ctrl_down);
                on_ctrl_pressed();
            } else if !down && *ctrl_down {
                *ctrl_down = false;
                drop(ctrl_down);
                on_ctrl_released();
            }
        } else if event == WM_KEYDOWN {
            // 按住 Ctrl 期间按下其他键（如 Ctrl+C、Ctrl+Shift），取消切换候选。
            // 不能用 GetAsyncKeyState 判定 Ctrl 是否按住：低层钩子回调在该
            // 按键事件注入输入流之前执行，异步键状态尚未更新，快速连击时
            // 会读到陈旧的"未按下"而漏掉取消（见 on_ctrl_pressed 注释）。
            // 改用回调自身按 KEYDOWN/KEYUP 维护的 CTRL_DOWN 跟踪。
            if *locked(&CTRL_DOWN) {
                on_other_key_pressed();
            }
        }
    }
    unsafe { CallNextHookEx(None, ncode, wparam, lparam) }
}

/// 前台窗口必须属于本进程，返回其所属线程 id；否则返回 None。
/// 其它进程的前台窗口事件也会触发本钩子，必须据此过滤。
fn foreground_thread_id() -> Option<u32> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != GetCurrentProcessId() {
            debug!(
                "hook: 前台窗口属于其他进程 pid={} (本进程 pid={})，跳过",
                pid,
                GetCurrentProcessId()
            );
            return None;
        }
        Some(tid)
    }
}

/// 钩子路径的切换目标选择（纯函数，便于单测）：返回应响应本次物理 Ctrl 的
/// 实例线程 id；`None` = 本进程不是当前输入前台，钩子不得动作。
///
/// 钩子是**进程级**观察者：每个装有本输入法的进程都安装一份 WH_KEYBOARD_LL，
/// 系统对每次按键都会投递到所有进程的钩子。若不判断本进程是否在前台，后台
/// 进程的钩子也会把自己的实例切换一次并向 daemon 上报模式/Show 徽标——同一次
/// Ctrl 被两个实例（甚至两个不同进程）各切一次，两边模式相位不同时徽标就与
/// 真正在输入的实例相反（取反）；后台实例的 Show 还会抢走 daemon 的
/// "聚焦连接"（此后前台窗口的 Hide/位置更新被仲裁丢弃）。
///
/// 输入：
/// - `focused_tid`：`OnSetFocus` 记录的 TSF 前台线程（进程级记录，可能被
///   迟到/别实例的回调覆盖或清除）；
/// - `focused_claimed`：`focused_tid` 对应的实例自身仍自称持有 TSF 键盘焦点
///   （实例级标志，见 [`ServiceInner::foreground`]）；
/// - `win_tid`：前台窗口所属线程，仅当该窗口属于本进程时为 `Some`；
/// - `instances`：(线程 id, 该实例是否自称前台)。
///
/// 选择优先级（规则 1 要求前台窗口属于本进程；后台残留认领的误切换由
/// on_ctrl_released 的同线程 GetFocus 权威校验兜底）：
/// 1. 进程级焦点记录 + 该实例在自称前台集合内：信息最完整（同进程多实例）；
/// 2. 恰好一个实例自称前台：进程级记录被清除/覆盖时的兜底，同时覆盖
///    UWP 等前台窗口属于宿主进程（win_tid=None）的场景；
///    多个实例同时自称前台（迟到回调）时不可信，继续向下判断；
/// 3. 前台窗口属于本进程：按窗口线程定位；该线程未注册时回退唯一实例
///    （激活线程与窗口线程不一致，如设置面板等无独立消息循环的宿主）；
/// 4. 其余：本进程不在前台 —— 后台钩子只观察按键，不切换任何实例。
fn pick_toggle_target(
    focused_tid: Option<u32>,
    focused_claimed: bool,
    win_tid: Option<u32>,
    instances: &[(u32, bool)],
) -> Option<u32> {
    // 1) 进程级焦点记录与该实例的焦点标志一致，且前台窗口属于本进程。
    // 前台窗口归属是硬门控：TSF 焦点认领可能残留（OnSetFocus(false) 迟到/
    // 丢失），后台进程凭残留认领通过规则 1 会对同一次物理 Ctrl 也切换自己的
    // 实例——两进程各自上报相反模式，daemon "最后 Show 胜出"使徽标显示非
    // 聚焦实例的字样（徽标反转）。经典 TSF 下 IME 承载进程与前台窗口所属
    // 进程一致（Chrome/WebView2/Zed 均然），该门控不影响正常路径。
    if win_tid.is_some()
        && let Some(tid) = focused_tid
        && focused_claimed
        && instances.iter().any(|(t, _)| *t == tid)
    {
        return Some(tid);
    }

    // 2) 唯一自称持有 TSF 键盘焦点的实例（不要求前台窗口属于本进程：
    // UWP 等应用前台窗口在宿主进程 ApplicationFrameHost，而 IME 承载在
    // 应用进程，win_tid 必为 None）。残留认领的后台误切换由
    // on_ctrl_released 的同线程 GetFocus 权威校验兜底，不在此猜测。
    let mut claimed = instances
        .iter()
        .filter(|(_, foreground)| *foreground)
        .map(|(tid, _)| *tid);
    if let Some(tid) = claimed.next()
        && claimed.next().is_none()
    {
        return Some(tid);
    }

    // 3) 前台窗口属于本进程：窗口线程 → 唯一实例兜底
    if let Some(tid) = win_tid {
        if instances.iter().any(|(t, _)| *t == tid) {
            return Some(tid);
        }
        if let [only] = instances {
            return Some(only.0);
        }
    }

    // 4) 本进程不在前台：不动作（后台钩子绝不能切换自己的实例）
    None
}

/// 获取应响应本次物理 Ctrl 的服务实例及其注册线程 id（弱引用升级失败则
/// 返回 None）。选择规则见 [`pick_toggle_target`]：只有本进程是当前输入前台
/// （或本进程内有实例自称持有 TSF 键盘焦点）时才返回目标实例。
///
/// 全程**不加任何 `ServiceInner` 锁**（焦点集合来自 [`focus_snapshot`]，
/// 实例只做弱引用升级）：本函数在低层键盘钩子回调里运行，若等待按键路径
/// 持有的服务锁（其持锁期间会做同步 IPC），会拖慢甚至卡住宿主输入。
/// 焦点真伪的权威校验由调用方在拿到实例锁后进行（见 [`on_ctrl_released`]）。
fn foreground_service() -> Option<(u32, Arc<Mutex<ServiceInner>>)> {
    let (focused_tid, claimed_tids) = focus_snapshot();
    // 仅当前台窗口属于本进程时为 Some（其它进程的前台窗口事件也会触发本钩子）
    let win_tid = foreground_thread_id();
    let instances: Vec<(u32, Arc<Mutex<ServiceInner>>)> = {
        let guard = ACTIVE_SERVICES.lock().ok()?;
        let map = guard.as_ref()?;
        map.iter()
            .filter_map(|(tid, weak)| weak.upgrade().map(|inner| (*tid, inner)))
            .collect()
    };
    if instances.is_empty() {
        return None;
    }
    let claims: Vec<(u32, bool)> = instances
        .iter()
        .map(|(tid, _)| (*tid, claimed_tids.contains(tid)))
        .collect();
    let focused_claimed = focused_tid.is_some_and(|tid| claimed_tids.contains(&tid));
    let Some(target) = pick_toggle_target(focused_tid, focused_claimed, win_tid, &claims) else {
        debug!(
            "hook: 本进程不在输入前台，忽略按键（tsf={:?} win={:?} claims={:?}）",
            focused_tid, win_tid, claims
        );
        return None;
    };
    // 现场可查：下次再出现"徽标取反"时据此判断是第几级选择命中、是否选错实例
    debug!(
        "hook: toggle target tid={} (tsf={:?} win={:?} claims={:?})",
        target, focused_tid, win_tid, claims
    );
    instances.into_iter().find(|(tid, _)| *tid == target)
}

fn on_ctrl_pressed() {
    // 只武装进程级手势，不触碰任何实例：与 TSF 路径共享同一份 pending，
    // 一次手势在进程内只可能被消费一次（见 CTRL_GESTURE_PENDING）。
    // 注意不能用 GetAsyncKeyState 校验"物理键仍按着"：低层钩子回调在该
    // 按键事件注入输入流之前执行，异步键状态尚未更新，按下的 Ctrl 会读到
    // "未按下"，导致手势永远不武装、切换失效。回调队列内按下先于松开派发
    // （同一线程按序处理），不存在"迟到按下重新武装已消费手势"的乱序。
    gesture_ctrl_pressed();
}

fn on_other_key_pressed() {
    // 按住 Ctrl 期间按下其他键（Ctrl+C、Ctrl+Shift）：取消切换候选。
    // 只取消进程级手势，不触碰任何实例。"Ctrl 是否按住"由调用方依据
    // 钩子回调自身的 CTRL_DOWN 跟踪判定（GetAsyncKeyState 在低层钩子
    // 回调里不可靠，见 on_ctrl_pressed 注释），此处无条件取消。
    gesture_other_key_pressed(true);
}

fn on_ctrl_released() {
    // 先解析目标并完成焦点校验，**最后**才消费进程级手势：若在解析前就
    // 消费（先取票后验货），钩子侧任何早期放弃（本进程不在前台、焦点认领
    // 残留校验失败）都会把手势吃掉——TSF OnTestKeyUp 路径随后拿到 false
    // 提前返回，用户刻意按下的 Ctrl 切换被静默吞掉。改为验证通过后才
    // 消费：钩子早期放弃时手势仍处于武装状态，聚焦实例的 TSF 路径仍可
    // 接管（恢复旧实例级状态机的优雅降级）；反之若 TSF 路径先消费，本
    // 函数拿到 false 直接返回，一次手势在进程内仍恰好被一条路径消费。
    let Some((target_tid, inner)) = foreground_service() else {
        debug!("hook: 未找到前台服务实例，手势留给 TSF 路径处理");
        return;
    };
    // 焦点真伪权威校验：焦点认领可能残留（OnSetFocus(false) 迟到/丢失），
    // 后台进程/同进程僵尸实例凭残留认领被选中会对同一次物理 Ctrl 也切换
    // 自己——两实例相位错开后各自上报相反模式，徽标显示非聚焦实例的字样。
    // ITfThreadMgr::GetFocus 是 TSF 自己维护的焦点事实：仅当候选实例就在
    // 钩子线程（STA 所有者线程）上时可直接查询（跨线程 COM 调用有阻塞
    // 风险，低层钩子回调里禁止）。无 thread_mgr（未完成激活）时跳过校验。
    if target_tid == unsafe { GetCurrentThreadId() } {
        let thread_mgr = {
            let guard = match inner.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.thread_mgr.clone()
        };
        if let Some(tm) = thread_mgr {
            let has_focus = unsafe { tm.GetFocus() }.is_ok();
            if !has_focus {
                debug!("hook: 候选实例的 TSF 线程管理器当前无焦点（认领已残留），丢弃本次切换");
                return;
            }
        }
    }
    // 验证通过，此时才消费手势：拿到 false 说明 TSF 路径已先消费并完成
    // 切换，本路径不得再次翻转（一次手势恰好被一条路径消费）。
    if !gesture_consume_release() {
        debug!("hook: 手势已被 TSF 路径消费，跳过钩子侧切换");
        return;
    }
    // 消费手势成功，翻转**目标实例自身**的模式并走切换收尾
    let english = {
        let mut guard = match inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        // 翻转目标值本身即新模式：set_english 的返回值只表示"是否发生变化"
        // （此处必然变化），不能当作模式值使用——误用 Option::is_some 会让
        // 每次切换都上报英文（恒为 true），徽标字样永远显示"英"
        let english = !guard.mode_switch.is_english();
        guard.mode_switch.set_english(english);
        english
    };
    if let Ok(mut guard) = inner.lock() {
        // 一律延迟锁定：钩子回调无法请求 TSF 编辑会话采样，而采样缓存
        // 无法证明仍反映当前语境（last_caret_pos 只在布局事件后异步更新，
        // 上屏/点击后的窗口内门控两侧是同一个滞后快照，空真通过会锁进
        // 陈旧基线，下一键即被自动切换撤销——徽标取反）。延迟到下个评估
        // 点以本次新鲜读取的建议锁定，防护更强且无陈旧窗口。
        // 关闭自动切换时不置位：没有评估点会消费它，残留下次开启时可能
        // 被当作"本次手动切换的基线"（开关同步路径也会清除残留）。
        if guard.auto_switch {
            guard.pending_manual_lock = true;
        }
    }
    debug!(
        "Ctrl toggled via keyboard hook: {}",
        if english { "英文" } else { "中文" }
    );
    apply_input_mode_toggle(&inner, english);
    // 上报 daemon 持久化并更新全局状态，供其它进程（管理员/普通）同步；
    // 随后显式 Show 徽标（携带本实例焦点光标）：先字样后 Show，
    // Show 触发的重绘必然呈现新字样
    send_ui_command_inner(&inner, UiCommand::SetInputMode(english));
    let context = caret_context_for_show();
    // 同一焦点会话内重显：未取到光标时沿用 UI 已记录的位置
    // （invalidate_caret=false），否则切换中英时徽标会被清成"位置未知"
    // 而屏外停放、用户看不到任何反馈
    send_ui_command_inner(
        &inner,
        UiCommand::ShowModeIndicator {
            context,
            invalidate_caret: false,
            english: Some(english),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{
        clear_foreground_thread, focus_snapshot, pick_toggle_target, set_foreground_thread,
    };

    #[test]
    fn foreground_focus_set_keeps_other_threads() {
        // 焦点集合只能由记录所属线程移除：别的实例（线程）的失焦回调不得
        // 抹掉当前聚焦实例的记录，否则钩子路径会退回近似选择，可能选中与
        // TSF 按键路径不同的实例（同一次 Ctrl 被切两次）
        set_foreground_thread(11);
        clear_foreground_thread(22);
        let (latest, claimed) = focus_snapshot();
        assert_eq!(latest, Some(11), "非记录所属线程的失焦回调不得清除记录");
        assert!(claimed.contains(&11));

        // 两个实例同时声称前台（迟到回调）：记录最近者，集合保留两者，
        // 选择函数在歧义时不再猜测
        set_foreground_thread(12);
        let (latest, claimed) = focus_snapshot();
        assert_eq!(latest, Some(12));
        assert_eq!(claimed.len(), 2);
        assert_eq!(
            pick_toggle_target(Some(12), true, Some(12), &[(11, true), (12, true)]),
            Some(12),
            "多个自称前台时以最近记录者为准"
        );

        // 记录所属线程失焦：从集合移除，latest 改指向仍自称前台的实例
        clear_foreground_thread(12);
        let (latest, claimed) = focus_snapshot();
        assert_eq!(latest, Some(11));
        assert!(!claimed.contains(&12));
        clear_foreground_thread(11);
        let (latest, claimed) = focus_snapshot();
        assert_eq!(latest, None);
        assert!(claimed.is_empty());
    }

    #[test]
    fn focused_thread_with_claim_wins() {
        // 进程级记录 + 该实例自称前台：即使前台窗口线程指向别的实例
        // （多线程窗口），也以 TSF 焦点实例为准——与 TSF 按键路径同一实例，
        // 共享 ctrl_pending 自然去重
        assert_eq!(
            pick_toggle_target(Some(7), true, Some(8), &[(7, true), (8, false)]),
            Some(7)
        );
    }

    #[test]
    fn stale_focused_thread_is_not_trusted() {
        // 进程级记录指向某实例，但该实例已不自称前台（记录被迟到/别实例
        // 回调污染）：不得据此切换；此处另有唯一自称前台实例 → 选它
        assert_eq!(
            pick_toggle_target(Some(7), false, Some(8), &[(7, false), (8, true)]),
            Some(8)
        );
        // 无任何自称前台实例、前台窗口也不属于本进程 → 后台进程不动作
        assert_eq!(
            pick_toggle_target(Some(7), false, None, &[(7, false), (8, false)]),
            None
        );
    }

    #[test]
    fn background_process_never_toggles_own_instance() {
        // 后台进程（前台窗口属于其它进程 ⇒ win_tid=None，实例也早已失去
        // 焦点标志）即使只有一个注册实例也不得回退切换——否则同一次 Ctrl
        // 被前后台两个实例各切一次，相位不同时徽标取反
        assert_eq!(pick_toggle_target(None, false, None, &[(3, false)]), None);
        // 有进程级记录但实例不自称前台，同样拒绝
        assert_eq!(
            pick_toggle_target(Some(3), false, None, &[(3, false)]),
            None
        );
    }

    #[test]
    fn sole_claim_fallback_covers_out_of_process_host() {
        // UWP 等应用前台窗口在宿主进程（win_tid=None），但本进程唯一实例
        // 自称持有 TSF 键盘焦点 → 由它响应 Ctrl（后台残留认领的误切换由
        // on_ctrl_released 的同线程 GetFocus 权威校验兜底）
        assert_eq!(pick_toggle_target(None, false, None, &[(5, true)]), Some(5));
        // 进程级记录 + 该实例认领 + 前台窗口属于本进程：规则 1 命中
        assert_eq!(
            pick_toggle_target(Some(5), true, Some(5), &[(5, true)]),
            Some(5)
        );
        // 多个实例同时自称前台（迟到回调遗留）：不可信，不再猜测
        assert_eq!(
            pick_toggle_target(None, false, None, &[(5, true), (6, true)]),
            None
        );
    }

    #[test]
    fn foreground_window_fallbacks() {
        // 前台窗口属于本进程：按窗口线程定位实例
        assert_eq!(
            pick_toggle_target(None, false, Some(9), &[(9, false), (10, false)]),
            Some(9)
        );
        // 窗口线程未注册但进程内唯一实例（激活线程与窗口线程不一致）
        assert_eq!(
            pick_toggle_target(None, false, Some(9), &[(10, false)]),
            Some(10)
        );
        // 窗口线程未注册且实例多于一个：无法确定，宁可不切换
        assert_eq!(
            pick_toggle_target(None, false, Some(9), &[(10, false), (11, false)]),
            None
        );
        // 没有前台窗口信息（GetForegroundWindow 失败等）也不伪造目标
        assert_eq!(pick_toggle_target(None, false, None, &[]), None);
    }
}
