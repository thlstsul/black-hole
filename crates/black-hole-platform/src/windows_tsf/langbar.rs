//! Windows TSF 语言栏项（Language Bar Item）实现
//!
//! 提供位于任务栏输入法指示器左侧的专属托盘按钮：
//! - 仅在 Black-Hole IME 激活时显示
//! - 不可移动（语言栏项固定位置）
//! - 点击弹出菜单：设置、输入方案、主题、退出

use super::service::apply_input_mode_toggle;
use super::{CLSID_BLACKHOLE_TIP, ServiceInner, send_ui_command_inner};
use crate::auto_start::is_auto_start;
use black_hole_shared::{SchemeId, Theme, UiCommand};
use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::slice;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_UNEXPECTED, FreeLibrary, HMODULE, LPARAM, POINT, RECT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC, CreateDIBSection,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject,
    DrawTextW, GetDC, GetStockObject, HBITMAP, HGDIOBJ, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows::Win32::UI::TextServices::{
    GUID_LBI_INPUTMODE, ITfLangBarItem, ITfLangBarItem_Impl, ITfLangBarItemButton,
    ITfLangBarItemButton_Impl, ITfLangBarItemSink, ITfMenu, ITfSource, ITfSource_Impl,
    TF_LANGBARITEMINFO, TF_LBI_CLK_LEFT, TF_LBI_CLK_RIGHT, TF_LBI_ICON, TF_LBI_STYLE_BTN_MENU,
    TF_LBI_STYLE_SHOWNINTRAY, TF_LBI_STYLE_TEXTCOLORICON, TF_LBI_TEXT, TF_LBMENUF_CHECKED,
    TF_LBMENUF_SEPARATOR, TF_LBMENUF_SUBMENU, TfLBIClick,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DestroyMenu, DestroyWindow,
    HICON, ICONINFO, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, PostMessageW,
    SetForegroundWindow, TPM_LEFTBUTTON, TPM_NONOTIFY, TPM_RETURNCMD, TrackPopupMenu,
    WINDOW_EX_STYLE, WM_NULL, WS_POPUP,
};
use windows_core::{
    BOOL, BSTR, GUID, IUnknown, Interface, PCSTR, PCWSTR, Ref, Result, implement, w,
};

// ---------------------------------------------------------------------------
// 菜单命令 ID
// ---------------------------------------------------------------------------

const MENU_ID_SETTINGS: u32 = 1;
const MENU_ID_PINYIN: u32 = 2;
const MENU_ID_SHUANGPIN: u32 = 3;
const MENU_ID_LIGHT: u32 = 4;
const MENU_ID_DARK: u32 = 5;
const MENU_ID_SYSTEM: u32 = 6;
const MENU_ID_EXIT: u32 = 7;
const MENU_ID_AUTO_START: u32 = 9;
const MENU_ID_AUTO_SWITCH: u32 = 10;

// ---------------------------------------------------------------------------
// 菜单暗色主题支持
// ---------------------------------------------------------------------------

const PREFERRED_APP_MODE_FORCE_DARK: i32 = 2;
const PREFERRED_APP_MODE_FORCE_LIGHT: i32 = 3;

type SetPreferredAppModeFn = unsafe extern "system" fn(i32) -> i32;
type FlushMenuThemesFn = unsafe extern "system" fn();

/// 通过 `uxtheme.dll` 的未公开导出函数设置进程级菜单主题偏好，
/// 并在 Drop 时恢复到之前的状态。
struct PreferredAppModeGuard {
    _module: HMODULE,
    set: SetPreferredAppModeFn,
    flush: FlushMenuThemesFn,
    previous: i32,
}

impl PreferredAppModeGuard {
    /// 尝试将当前进程的应用主题模式设置为 `mode`。
    fn set(mode: i32) -> Option<Self> {
        unsafe {
            let module = LoadLibraryW(w!("uxtheme.dll")).ok()?;
            let set = GetProcAddress(module, PCSTR(135 as *const u8))?;
            let flush = GetProcAddress(module, PCSTR(136 as *const u8))?;
            let set: SetPreferredAppModeFn = mem::transmute(set);
            let flush: FlushMenuThemesFn = mem::transmute(flush);
            let previous = set(mode);
            flush();
            debug!("SetPreferredAppMode mode={} previous={}", mode, previous);
            Some(Self {
                _module: module,
                set,
                flush,
                previous,
            })
        }
    }
}

impl Drop for PreferredAppModeGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.set)(self.previous);
            (self.flush)();
            let _ = FreeLibrary(self._module);
            debug!("Restored PreferredAppMode to {}", self.previous);
        }
    }
}

fn preferred_app_mode_for_theme(theme: Theme) -> i32 {
    match theme {
        Theme::Light => PREFERRED_APP_MODE_FORCE_LIGHT,
        Theme::Dark => PREFERRED_APP_MODE_FORCE_DARK,
        Theme::System => {
            if system_uses_dark_mode() {
                PREFERRED_APP_MODE_FORCE_DARK
            } else {
                PREFERRED_APP_MODE_FORCE_LIGHT
            }
        }
    }
}

/// 读取注册表判断系统是否处于暗色模式。
fn system_uses_dark_mode() -> bool {
    unsafe {
        let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let value: Vec<u16> = "AppsUseLightTheme".encode_utf16().chain(Some(0)).collect();
        let mut hkey = mem::zeroed();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }

        let mut data: u32 = 0;
        let mut size = mem::size_of::<u32>() as u32;
        let mut ty = REG_VALUE_TYPE(0);
        let is_dark = RegQueryValueExW(
            hkey,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut ty),
            Some(&mut data as *mut _ as *mut u8),
            Some(&mut size),
        )
        .is_ok()
            && ty == REG_DWORD
            && data == 0;

        let _ = RegCloseKey(hkey);
        is_dark
    }
}

// ---------------------------------------------------------------------------
// COM 对象：BlackHoleLangBarItem
// ---------------------------------------------------------------------------

#[implement(ITfLangBarItem, ITfLangBarItemButton, ITfSource)]
pub(crate) struct BlackHoleLangBarItem {
    inner: Arc<Mutex<ServiceInner>>,
}

impl BlackHoleLangBarItem {
    pub(crate) fn new(inner: Arc<Mutex<ServiceInner>>) -> Self {
        Self { inner }
    }

    fn send_ui_command(&self, cmd: UiCommand) {
        // 委托公共函数:连接缺失/断开时自动重连,失败记录日志
        send_ui_command_inner(&self.inner, cmd);
    }

    fn current_scheme(&self) -> SchemeId {
        let inner = self.inner.lock().unwrap();
        inner.current_scheme
    }

    fn current_theme(&self) -> Theme {
        let inner = self.inner.lock().unwrap();
        inner.current_theme
    }

    fn is_english_mode(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.mode_switch.is_english()
    }

    fn is_auto_switch(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.auto_switch
    }

    /// 切换"根据光标周围文本自动切换中英"开关：本进程立即生效，
    /// 持久化与其它进程同步由 daemon 统一处理（焦点同步时拉取）。
    fn toggle_auto_switch(&self) {
        let next = {
            let mut inner = self.inner.lock().unwrap();
            inner.auto_switch = !inner.auto_switch;
            inner.auto_switch
        };
        self.send_ui_command(UiCommand::SetAutoSwitch(next));
    }

    /// 切换中英文模式（不依赖 Ctrl 键事件，Chrome 等应用不把单独修饰键
    /// 转发给 TSF，因此通过点击语言栏提供显式入口）。
    fn toggle_input_mode(&self) {
        let toggled = {
            let mut inner = self.inner.lock().unwrap();
            let next = !inner.mode_switch.is_english();
            inner.mode_switch.set_english(next)
        };
        if let Some(english) = toggled {
            apply_input_mode_toggle(&self.inner, english);
            // 上报 daemon 持久化并更新全局状态，供其它进程（管理员/普通）同步
            self.send_ui_command(UiCommand::SetInputMode(english));
        }
    }

    fn set_scheme(&self, scheme: SchemeId) {
        let sink = {
            let mut inner = self.inner.lock().unwrap();
            inner.current_scheme = scheme;
            inner.langbar_item_sink.clone()
        };
        if let Some(sink) = sink {
            let _ = unsafe { sink.OnUpdate(TF_LBI_ICON) };
        }
    }

    fn set_theme(&self, theme: Theme) {
        let sink = {
            let mut inner = self.inner.lock().unwrap();
            inner.current_theme = theme;
            inner.langbar_item_sink.clone()
        };
        if let Some(sink) = sink {
            let _ = unsafe { sink.OnUpdate(TF_LBI_ICON | TF_LBI_TEXT) };
        }
    }

    /// 在点击位置显示自定义右键菜单。
    /// Windows 8+ 的 GUID_LBI_INPUTMODE 项触发 OnClick 而不是 InitMenu，
    /// 因此需要自行创建并跟踪 Win32 弹出菜单。
    fn show_context_menu(&self, pt: &POINT) {
        unsafe {
            let scheme = self.current_scheme();
            let theme = self.current_theme();

            let root = match CreatePopupMenu() {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to create popup menu: {}", e);
                    return;
                }
            };

            let _ = AppendMenuW(root, MF_STRING, MENU_ID_SETTINGS as usize, w!("设置"));
            let _ = AppendMenuW(root, MF_SEPARATOR, 0, PCWSTR::null());

            // 输入方案子菜单
            let scheme_menu = match CreatePopupMenu() {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to create scheme submenu: {}", e);
                    let _ = DestroyMenu(root);
                    return;
                }
            };
            let _ = AppendMenuW(
                scheme_menu,
                MF_STRING
                    | if scheme == SchemeId::Pinyin {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_PINYIN as usize,
                w!("拼音"),
            );
            let _ = AppendMenuW(
                scheme_menu,
                MF_STRING
                    | if scheme == SchemeId::Shuangpin {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_SHUANGPIN as usize,
                w!("小鹤双拼"),
            );
            let _ = AppendMenuW(root, MF_POPUP, scheme_menu.0 as usize, w!("输入方案"));

            // 主题子菜单
            let theme_menu = match CreatePopupMenu() {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to create theme submenu: {}", e);
                    let _ = DestroyMenu(root);
                    let _ = DestroyMenu(scheme_menu);
                    return;
                }
            };
            let _ = AppendMenuW(
                theme_menu,
                MF_STRING
                    | if theme == Theme::Light {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_LIGHT as usize,
                w!("浅色"),
            );
            let _ = AppendMenuW(
                theme_menu,
                MF_STRING
                    | if theme == Theme::Dark {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_DARK as usize,
                w!("深色"),
            );
            let _ = AppendMenuW(
                theme_menu,
                MF_STRING
                    | if theme == Theme::System {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_SYSTEM as usize,
                w!("跟随系统"),
            );
            let _ = AppendMenuW(root, MF_POPUP, theme_menu.0 as usize, w!("主题"));

            let _ = AppendMenuW(root, MF_SEPARATOR, 0, PCWSTR::null());

            // 根据光标周围文本自动切换中英：勾选状态反映本进程同步到的设置
            let _ = AppendMenuW(
                root,
                MF_STRING
                    | if self.is_auto_switch() {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_AUTO_SWITCH as usize,
                w!("自动切换中英"),
            );

            // 开机自启动：勾选状态反映系统当前配置（读注册表，无副作用）
            let _ = AppendMenuW(
                root,
                MF_STRING
                    | if is_auto_start() {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_ID_AUTO_START as usize,
                w!("开机自启动"),
            );

            let _ = AppendMenuW(root, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(root, MF_STRING, MENU_ID_EXIT as usize, w!("退出"));

            // 不使用 GetForegroundWindow() 作为菜单宿主：点击任务栏输入指示器时
            // 前台窗口可能属于 shell（其它进程/线程），TrackPopupMenu 要求宿主窗口
            // 属于调用线程且为前台窗口或其子窗口，否则菜单无法弹出（Chrome 常见）。
            // 因此创建当前线程所有的隐藏窗口作为菜单宿主。
            let host = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    warn!("Failed to create menu host window: {}", e);
                    let _ = DestroyMenu(root);
                    return;
                }
            };

            // 设置进程级菜单主题偏好，菜单关闭后自动恢复。
            let mode = preferred_app_mode_for_theme(theme);
            let theme_guard = PreferredAppModeGuard::set(mode);
            if theme_guard.is_none() {
                warn!("Failed to set preferred app mode for menu theme {}", mode);
            }

            // 经典 workaround：TrackPopupMenu 要求宿主为前台窗口或其子窗口，
            // 先把宿主置为前台，菜单关闭后发 WM_NULL 让原前台窗口恢复。
            let _ = SetForegroundWindow(host);

            let cmd = TrackPopupMenu(
                root,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTBUTTON,
                pt.x,
                pt.y,
                None,
                host,
                None,
            );

            let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyWindow(host);
            let _ = DestroyMenu(root);

            let cmd_id = cmd.0 as u32;
            if cmd_id == 0 {
                debug!("LangBarItem menu dismissed without selection");
                return;
            }
            debug!("LangBarItem menu selected: cmd={}", cmd_id);

            match cmd_id {
                MENU_ID_SETTINGS => self.send_ui_command(UiCommand::ShowSettings),
                MENU_ID_PINYIN => {
                    self.set_scheme(SchemeId::Pinyin);
                    self.send_ui_command(UiCommand::SwitchScheme(SchemeId::Pinyin));
                }
                MENU_ID_SHUANGPIN => {
                    self.set_scheme(SchemeId::Shuangpin);
                    self.send_ui_command(UiCommand::SwitchScheme(SchemeId::Shuangpin));
                }
                MENU_ID_LIGHT => {
                    self.set_theme(Theme::Light);
                    self.send_ui_command(UiCommand::SetTheme(Theme::Light));
                }
                MENU_ID_DARK => {
                    self.set_theme(Theme::Dark);
                    self.send_ui_command(UiCommand::SetTheme(Theme::Dark));
                }
                MENU_ID_SYSTEM => {
                    self.set_theme(Theme::System);
                    self.send_ui_command(UiCommand::SetTheme(Theme::System));
                }
                MENU_ID_AUTO_START => {
                    // 切换自启动状态；持久化与平台写入由 daemon 统一处理
                    let next = !is_auto_start();
                    self.send_ui_command(UiCommand::SetAutoStart(next));
                }
                MENU_ID_AUTO_SWITCH => self.toggle_auto_switch(),
                MENU_ID_EXIT => self.send_ui_command(UiCommand::Exit),
                _ => {}
            }
        }
    }
}

impl ITfLangBarItem_Impl for BlackHoleLangBarItem_Impl {
    fn GetInfo(&self, pinfo: *mut TF_LANGBARITEMINFO) -> Result<()> {
        debug!("LangBarItem::GetInfo called");
        unsafe {
            let info = &mut *pinfo;
            info.clsidService = CLSID_BLACKHOLE_TIP;
            // Windows 8+ 的输入指示器只显示 GUID 为 GUID_LBI_INPUTMODE 的项。
            info.guidItem = GUID_LBI_INPUTMODE;
            info.dwStyle =
                TF_LBI_STYLE_BTN_MENU | TF_LBI_STYLE_SHOWNINTRAY | TF_LBI_STYLE_TEXTCOLORICON;
            info.ulSort = 100;
            info.szDescription = [0; 32];
            let desc: Vec<u16> = "Black-Hole".encode_utf16().collect();
            for (i, &c) in desc.iter().enumerate().take(32) {
                info.szDescription[i] = c;
            }
        }
        Ok(())
    }

    fn GetStatus(&self) -> Result<u32> {
        debug!("LangBarItem::GetStatus called");
        Ok(0)
    }

    fn Show(&self, fshow: BOOL) -> Result<()> {
        debug!("LangBarItem::Show called: fshow={}", fshow.0);
        Ok(())
    }

    fn GetTooltipString(&self) -> Result<BSTR> {
        debug!("LangBarItem::GetTooltipString called");
        Ok(BSTR::from("黑洞输入法"))
    }
}

impl ITfLangBarItemButton_Impl for BlackHoleLangBarItem_Impl {
    fn OnClick(&self, click: TfLBIClick, pt: &POINT, _prcarea: *const RECT) -> Result<()> {
        debug!("LangBarItem::OnClick called: click={:?}", click.0);
        // Windows 8+ 对 GUID_LBI_INPUTMODE 项通常走 OnClick 而不是 InitMenu。
        // 左键点击直接切换中英模式，右键弹出上下文菜单。
        if click == TF_LBI_CLK_LEFT {
            self.toggle_input_mode();
        } else if click == TF_LBI_CLK_RIGHT {
            self.show_context_menu(pt);
        }
        Ok(())
    }

    fn InitMenu(&self, pmenu: Ref<'_, ITfMenu>) -> Result<()> {
        debug!("LangBarItem::InitMenu called");
        let menu = pmenu.to_owned().ok_or(E_UNEXPECTED)?;

        add_menu_item(&menu, MENU_ID_SETTINGS, 0, "设置")?;
        add_menu_separator(&menu)?;

        let scheme_menu = add_submenu(&menu, 0, "输入方案")?;
        let scheme = self.current_scheme();
        add_menu_item(
            &scheme_menu,
            MENU_ID_PINYIN,
            if scheme == SchemeId::Pinyin {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "拼音",
        )?;
        add_menu_item(
            &scheme_menu,
            MENU_ID_SHUANGPIN,
            if scheme == SchemeId::Shuangpin {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "小鹤双拼",
        )?;

        let theme_menu = add_submenu(&menu, 0, "主题")?;
        let theme = self.current_theme();
        add_menu_item(
            &theme_menu,
            MENU_ID_LIGHT,
            if theme == Theme::Light {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "浅色",
        )?;
        add_menu_item(
            &theme_menu,
            MENU_ID_DARK,
            if theme == Theme::Dark {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "深色",
        )?;
        add_menu_item(
            &theme_menu,
            MENU_ID_SYSTEM,
            if theme == Theme::System {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "跟随系统",
        )?;

        add_menu_separator(&menu)?;

        // 根据光标周围文本自动切换中英：勾选状态反映本进程同步到的设置
        add_menu_item(
            &menu,
            MENU_ID_AUTO_SWITCH,
            if self.is_auto_switch() {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "自动切换中英",
        )?;

        // 开机自启动：勾选状态反映系统当前配置（读注册表，无副作用）
        add_menu_item(
            &menu,
            MENU_ID_AUTO_START,
            if is_auto_start() {
                TF_LBMENUF_CHECKED
            } else {
                0
            },
            "开机自启动",
        )?;

        add_menu_separator(&menu)?;
        add_menu_item(&menu, MENU_ID_EXIT, 0, "退出")?;

        Ok(())
    }

    fn OnMenuSelect(&self, wid: u32) -> Result<()> {
        debug!("LangBarItem::OnMenuSelect called: wid={}", wid);
        match wid {
            MENU_ID_SETTINGS => self.send_ui_command(UiCommand::ShowSettings),
            MENU_ID_PINYIN => {
                self.set_scheme(SchemeId::Pinyin);
                self.send_ui_command(UiCommand::SwitchScheme(SchemeId::Pinyin));
            }
            MENU_ID_SHUANGPIN => {
                self.set_scheme(SchemeId::Shuangpin);
                self.send_ui_command(UiCommand::SwitchScheme(SchemeId::Shuangpin));
            }
            MENU_ID_LIGHT => {
                self.set_theme(Theme::Light);
                self.send_ui_command(UiCommand::SetTheme(Theme::Light));
            }
            MENU_ID_DARK => {
                self.set_theme(Theme::Dark);
                self.send_ui_command(UiCommand::SetTheme(Theme::Dark));
            }
            MENU_ID_SYSTEM => {
                self.set_theme(Theme::System);
                self.send_ui_command(UiCommand::SetTheme(Theme::System));
            }
            MENU_ID_AUTO_START => {
                // 切换自启动状态；持久化与平台写入由 daemon 统一处理
                let next = !is_auto_start();
                self.send_ui_command(UiCommand::SetAutoStart(next));
            }
            MENU_ID_AUTO_SWITCH => self.toggle_auto_switch(),
            MENU_ID_EXIT => self.send_ui_command(UiCommand::Exit),
            _ => {}
        }
        Ok(())
    }

    fn GetIcon(&self) -> Result<HICON> {
        debug!("LangBarItem::GetIcon called");
        render_scheme_icon(
            self.current_scheme(),
            self.current_theme(),
            self.is_english_mode(),
        )
    }

    fn GetText(&self) -> Result<BSTR> {
        debug!("LangBarItem::GetText called");
        // 菜单按钮通常只显示图标；文本留空避免占用空间。
        Ok(BSTR::new())
    }
}

impl ITfSource_Impl for BlackHoleLangBarItem_Impl {
    fn AdviseSink(&self, riid: *const GUID, punk: Ref<'_, IUnknown>) -> Result<u32> {
        let riid_safe = unsafe { riid.as_ref().copied() };
        debug!("LangBarItem::AdviseSink called: riid={:?}", riid_safe);

        let sink_iid = <ITfLangBarItemSink as Interface>::IID;
        if let Some(req_riid) = riid_safe
            && req_riid == sink_iid
        {
            let unknown = punk.to_owned().ok_or(E_UNEXPECTED)?;
            let sink: ITfLangBarItemSink = unknown.cast()?;
            let mut inner = self.inner.lock().unwrap();
            inner.langbar_item_sink = Some(sink.clone());
            inner.langbar_item_sink_cookie = 1;
            debug!("LangBarItem sink installed");
            // 通知语言栏管理器立即刷新该项的图标和文本。
            // Windows 在项未完全就绪时可能返回 E_FAIL，不影响后续渲染。
            match unsafe { sink.OnUpdate(TF_LBI_ICON | TF_LBI_TEXT) } {
                Ok(()) => debug!("LangBarItem OnUpdate sent successfully"),
                Err(e) => debug!("LangBarItem OnUpdate ignored: {}", e),
            }
            return Ok(1);
        }

        Ok(0)
    }

    fn UnadviseSink(&self, dwcookie: u32) -> Result<()> {
        debug!("LangBarItem::UnadviseSink called: dwcookie={}", dwcookie);
        if dwcookie == 1 {
            let mut inner = self.inner.lock().unwrap();
            inner.langbar_item_sink = None;
            inner.langbar_item_sink_cookie = 0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 菜单辅助函数
// ---------------------------------------------------------------------------

fn add_menu_item(menu: &ITfMenu, id: u32, flags: u32, text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        menu.AddMenuItem(
            id,
            flags,
            HBITMAP(ptr::null_mut()),
            HBITMAP(ptr::null_mut()),
            &wide,
            ptr::null_mut(),
        )
    }
}

fn add_menu_separator(menu: &ITfMenu) -> Result<()> {
    add_menu_item(menu, 0, TF_LBMENUF_SEPARATOR, "")
}

fn add_submenu(menu: &ITfMenu, flags: u32, text: &str) -> Result<ITfMenu> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let mut submenu: Option<ITfMenu> = None;
    unsafe {
        menu.AddMenuItem(
            0,
            flags | TF_LBMENUF_SUBMENU,
            HBITMAP(ptr::null_mut()),
            HBITMAP(ptr::null_mut()),
            &wide,
            &mut submenu,
        )?;
    }
    submenu.ok_or(E_FAIL.into())
}

// ---------------------------------------------------------------------------
// 图标辅助函数
// ---------------------------------------------------------------------------

fn render_scheme_icon(scheme: SchemeId, theme: Theme, english: bool) -> Result<HICON> {
    const SIZE: i32 = 16;
    const PX_COUNT: usize = (SIZE * SIZE) as usize;

    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(E_UNEXPECTED.into());
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let _ = ReleaseDC(None, screen_dc);
        if mem_dc.is_invalid() {
            return Err(E_UNEXPECTED.into());
        }

        // 32bpp 自顶向下 DIB。
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE, // 负值表示自顶向下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default(); 1],
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let color_bmp = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        if color_bmp.is_invalid() {
            let _ = DeleteDC(mem_dc);
            return Err(E_UNEXPECTED.into());
        }

        // DIB 初始内容未定义，先清零以获得透明背景。
        ptr::write_bytes(bits, 0, PX_COUNT * 4);

        let old_bmp = SelectObject(mem_dc, HGDIOBJ(color_bmp.0));

        // 文字：白色、居中（白色仅作为亮度参考，后续会根据主题着色）。
        let text = if english {
            "英"
        } else {
            match scheme {
                SchemeId::Pinyin => "全",
                SchemeId::Shuangpin => "双",
            }
        };
        let mut wide: Vec<u16> = text.encode_utf16().collect();

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let old_font = SelectObject(mem_dc, font);
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(0x00FFFFFF));
        let text_rect = RECT {
            left: 0,
            top: 0,
            right: SIZE,
            bottom: SIZE,
        };
        let mut text_rect_draw = text_rect;
        DrawTextW(
            mem_dc,
            &mut wide[..],
            &mut text_rect_draw,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(mem_dc, old_font);

        // 恢复 DC 中的位图。
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteDC(mem_dc);

        // 根据主题确定文字颜色（0x00BBGGRR）。
        let text_color = match theme {
            Theme::Light => COLORREF(0x00000000),                // 黑色
            Theme::Dark | Theme::System => COLORREF(0x00FFFFFF), // 白色；System 暂按深色处理
        };
        let text_argb = u32::from_le_bytes([
            0xFF,
            (text_color.0 >> 16) as u8,
            (text_color.0 >> 8) as u8,
            text_color.0 as u8,
        ]);

        // 后处理 alpha：GDI 不写入 alpha，手动把非零亮度像素设为文字颜色并完全不透明。
        let pixels = slice::from_raw_parts_mut(bits as *mut u32, PX_COUNT);
        for px in pixels.iter_mut() {
            let b = (*px & 0xFF) as u8;
            let g = ((*px >> 8) & 0xFF) as u8;
            let r = ((*px >> 16) & 0xFF) as u8;
            if r > 0 || g > 0 || b > 0 {
                *px = text_argb;
            } else {
                *px = 0; // 完全透明
            }
        }

        // 创建 1bpp 掩码位图：全黑表示完全不透明。
        let mask_bmp = CreateBitmap(SIZE, SIZE, 1, 1, None);
        if mask_bmp.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(color_bmp.0));
            return Err(E_UNEXPECTED.into());
        }

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: color_bmp,
        };

        let icon = CreateIconIndirect(&icon_info);
        let _ = DeleteObject(HGDIOBJ(color_bmp.0));
        let _ = DeleteObject(HGDIOBJ(mask_bmp.0));
        icon
    }
}
