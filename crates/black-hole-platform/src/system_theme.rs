// ---------------------------------------------------------------------------
// 系统明暗主题探测
//
// UI 侧解析 `Theme::System` 时需要知道系统当前是亮色还是暗色，语言栏按钮的
// 「跟随系统」也依赖同一判断，故由平台层统一提供，避免两处实现漂移。
// ---------------------------------------------------------------------------

/// 探测操作系统当前是否处于暗色模式。
///
/// - Windows：读取 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\
///   Personalize\AppsUseLightTheme`（即「设置 → 个性化 → 颜色 → 选择模式」）；
///   键不存在或类型不符时按亮色处理。
/// - 其他平台：以 `GTK_THEME` 是否含 `dark` 作启发式判断；多数环境下该变量
///   并未设置，因此通常回落到亮色。
pub fn system_uses_dark_mode() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows_dark_mode()
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("GTK_THEME").is_ok_and(|theme| theme.to_ascii_lowercase().contains("dark"))
    }
}

#[cfg(target_os = "windows")]
fn windows_dark_mode() -> bool {
    use std::mem;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows_core::PCWSTR;

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
