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
//! 通过前台窗口所属线程在 `ACTIVE_SERVICES` 注册表中定位对应的服务实例并切换。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, Weak};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_RCONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, HHOOK, KBDLLHOOKSTRUCT,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
};

use super::ServiceInner;
use super::service::apply_input_mode_toggle;

/// 当前进程内所有已激活的文本服务实例（线程 id → 弱引用）。
/// 钩子回调据此定位前台窗口所属线程对应的服务。
static ACTIVE_SERVICES: Mutex<Option<HashMap<u32, Weak<Mutex<ServiceInner>>>>> = Mutex::new(None);
/// 已安装的钩子句柄（进程内只安装一次），用 usize 存储避免裸指针的 Send/Sync 问题。
static HOOK: Mutex<Option<usize>> = Mutex::new(None);
/// 当前进程内 Ctrl 是否处于按下状态，用于过滤按住时的自动重复按下事件。
static CTRL_DOWN: Mutex<bool> = Mutex::new(false);
/// 当前持有 TSF 输入焦点的线程 id（由 service.rs 的 OnSetFocus 维护）。
/// WebView2 等多进程应用中，IME 承载进程与宿主窗口进程不同，GetForegroundWindow
/// 无法反映真正的输入焦点，必须依赖 TSF 自身的焦点回调。
static FOREGROUND_TID: Mutex<Option<u32>> = Mutex::new(None);

/// 记录当前持有 TSF 输入焦点的线程（OnSetFocus(fforeground=true) 时调用）。
pub(crate) fn set_foreground_thread(thread_id: u32) {
    tracing::debug!("hook: set_foreground_thread thread_id={}", thread_id);
    if let Ok(mut guard) = FOREGROUND_TID.lock() {
        *guard = Some(thread_id);
    }
}

/// 清除 TSF 输入焦点记录（OnSetFocus(fforeground=false) 时调用）。
pub(crate) fn clear_foreground_thread() {
    tracing::debug!("hook: clear_foreground_thread");
    if let Ok(mut guard) = FOREGROUND_TID.lock() {
        *guard = None;
    }
}

/// 注册一个已激活的服务实例（Activate 时调用），并确保钩子已安装。
pub(crate) fn register_service(thread_id: u32, inner: Arc<Mutex<ServiceInner>>) {
    tracing::debug!("hook: register_service thread_id={}", thread_id);
    if let Ok(mut guard) = ACTIVE_SERVICES.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(thread_id, Arc::downgrade(&inner));
    }
    install_hook();
}

/// 注销服务实例（Deactivate 时调用）；若无剩余实例则卸载钩子。
pub(crate) fn unregister_service(thread_id: u32) {
    let empty = {
        let mut guard = ACTIVE_SERVICES.lock().unwrap();
        if let Some(map) = guard.as_mut() {
            map.remove(&thread_id);
            map.is_empty()
        } else {
            true
        }
    };
    if empty
        && let Ok(mut hook_guard) = HOOK.lock()
            && let Some(raw) = hook_guard.take() {
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
            tracing::debug!("WH_KEYBOARD_LL hook installed");
            *guard = Some(h.0 as usize);
        }
        Err(e) => tracing::warn!("Failed to install WH_KEYBOARD_LL hook: {}", e),
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

        if is_ctrl_vk(vk) && (event == WM_KEYDOWN || event == WM_KEYUP) {
            let down = event == WM_KEYDOWN;
            let mut ctrl_down = CTRL_DOWN.lock().unwrap();
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
            let ctrl_held = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
            if ctrl_held {
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
            tracing::debug!(
                "hook: 前台窗口属于其他进程 pid={} (本进程 pid={})，跳过",
                pid,
                GetCurrentProcessId()
            );
            return None;
        }
        Some(tid)
    }
}

/// 获取前台线程对应的服务实例（弱引用升级失败则返回 None）。
///
/// 优先使用 TSF `OnSetFocus` 记录的焦点线程 —— WebView2 等多进程应用中
/// IME 承载进程与宿主窗口进程不同，`GetForegroundWindow` 不可靠；
/// 其次按前台窗口所属线程匹配（传统单进程应用）；
/// 最后回退到唯一注册实例，兼容前台线程与激活线程不一致的情况。
fn foreground_service() -> Option<Arc<Mutex<ServiceInner>>> {
    let focused_tid = FOREGROUND_TID.lock().ok().and_then(|g| *g);
    let win_tid = foreground_thread_id();
    let guard = ACTIVE_SERVICES.lock().ok()?;
    let map = guard.as_ref()?;

    // 1) TSF 焦点线程（OnSetFocus 记录）优先。
    if let Some(tid) = focused_tid
        && let Some(weak) = map.get(&tid)
        && let Some(inner) = weak.upgrade()
    {
        return Some(inner);
    }

    // 2) 前台窗口所属线程（传统单进程应用场景）。
    if let Some(tid) = win_tid
        && let Some(weak) = map.get(&tid)
        && let Some(inner) = weak.upgrade()
    {
        return Some(inner);
    }

    // 3) 唯一实例兜底：未命中焦点线程但注册表中只有一个实例时回退使用。
    if map.len() == 1 {
        let weak = map.values().next()?;
        tracing::debug!("hook: 未命中焦点线程，回退唯一注册实例");
        return weak.upgrade();
    }

    tracing::debug!(
        "hook: 未命中焦点线程（tsf={:?} win={:?}）且注册实例数={}",
        focused_tid,
        win_tid,
        map.len()
    );
    None
}

fn on_ctrl_pressed() {
    let Some(inner) = foreground_service() else {
        return;
    };
    if let Ok(mut guard) = inner.lock() {
        guard.mode_switch.ctrl_pressed();
    }
}

fn on_other_key_pressed() {
    let Some(inner) = foreground_service() else {
        return;
    };
    if let Ok(mut guard) = inner.lock() {
        guard.mode_switch.other_key_pressed(true);
    }
}

fn on_ctrl_released() {
    let Some(inner) = foreground_service() else {
        return;
    };
    let toggled = {
        let mut guard = match inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.mode_switch.ctrl_released()
    };
    if let Some(english) = toggled {
        tracing::debug!(
            "Ctrl toggled via keyboard hook: {}",
            if english { "英文" } else { "中文" }
        );
        apply_input_mode_toggle(&inner, english);
    }
}
