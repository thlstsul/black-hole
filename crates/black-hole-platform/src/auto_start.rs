//! 开机自启动管理（平台相关）
//!
//! - Windows：写/删 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 Run 键值
//! - Linux：写/删 `~/.config/autostart/black-hole-ime.desktop`（XDG autostart）

use crate::PlatformError;

/// 设置当前程序是否开机自启动
pub fn set_auto_start(enabled: bool) -> Result<(), PlatformError> {
    #[cfg(target_os = "windows")]
    {
        windows_set_auto_start(enabled)
    }
    #[cfg(target_os = "linux")]
    {
        linux_set_auto_start(enabled)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = enabled;
        Err(PlatformError::Unsupported)
    }
}

/// 查询当前程序是否已开机自启动
pub fn is_auto_start() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows_is_auto_start()
    }
    #[cfg(target_os = "linux")]
    {
        linux_is_auto_start()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// Windows：Run 注册表键
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_core::PCWSTR;

    /// 当前用户 Run 键路径
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    /// 自启动注册表值名称
    const RUN_VALUE_NAME: &str = "BlackHoleIME";

    /// 打开 Run 键（不存在时创建）
    unsafe fn open_run_key(create: bool) -> Result<windows::Win32::System::Registry::HKEY, PlatformError> {
        let key_w: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
        let mut hkey = unsafe { std::mem::zeroed() };
        let result = unsafe {
            if create {
                // RegCreateKeyW 等价于带 KEY_ALL_ACCESS 的打开/创建
                windows::Win32::System::Registry::RegCreateKeyW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key_w.as_ptr()),
                    &mut hkey,
                )
            } else {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key_w.as_ptr()),
                    Some(0),
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    &mut hkey,
                )
            }
        };
        if result.0 != 0 {
            return Err(PlatformError::Other(format!(
                "failed to open Run registry key, error 0x{:08X}",
                result.0
            )));
        }
        Ok(hkey)
    }

    fn current_exe_quoted() -> Result<Vec<u16>, PlatformError> {
        let exe = std::env::current_exe()
            .map_err(|e| PlatformError::Other(format!("cannot locate current exe: {e}")))?;
        // Run 键中路径带引号，避免路径含空格时被拆成参数
        let quoted = format!("\"{}\"", exe.to_string_lossy());
        Ok(quoted.encode_utf16().chain(Some(0)).collect())
    }

    /// 自启动值名称的宽字符串（含结尾 NUL）
    fn value_name_w() -> Vec<u16> {
        RUN_VALUE_NAME.encode_utf16().chain(Some(0)).collect()
    }

    pub(super) fn set(enabled: bool) -> Result<(), PlatformError> {
        unsafe {
            if enabled {
                let value_w = current_exe_quoted()?;
                let hkey = open_run_key(true)?;
                let bytes = std::slice::from_raw_parts(
                    value_w.as_ptr() as *const u8,
                    value_w.len() * 2,
                );
                let name_w = value_name_w();
                let result = RegSetValueExW(
                    hkey,
                    PCWSTR(name_w.as_ptr()),
                    Some(0),
                    REG_SZ,
                    Some(bytes),
                );
                let _ = RegCloseKey(hkey);
                if result.0 != 0 {
                    return Err(PlatformError::Other(format!(
                        "failed to write Run registry value, error 0x{:08X}",
                        result.0
                    )));
                }
                Ok(())
            } else {
                let hkey = open_run_key(false)?;
                let name_w = value_name_w();
                let result = RegDeleteValueW(hkey, PCWSTR(name_w.as_ptr()));
                let _ = RegCloseKey(hkey);
                // 值不存在（ERROR_FILE_NOT_FOUND = 2）视为已处于关闭状态
                if result.0 != 0 && result.0 != 2 {
                    return Err(PlatformError::Other(format!(
                        "failed to delete Run registry value, error 0x{:08X}",
                        result.0
                    )));
                }
                Ok(())
            }
        }
    }

    pub(super) fn is_set() -> bool {
        unsafe {
            let Ok(hkey) = open_run_key(false) else {
                return false;
            };
            let mut data = [0u8; 8];
            let mut size = data.len() as u32;
            let name_w = value_name_w();
            let result = RegQueryValueExW(
                hkey,
                PCWSTR(name_w.as_ptr()),
                None,
                None,
                Some(data.as_mut_ptr()),
                Some(&mut size),
            );
            let _ = RegCloseKey(hkey);
            result.0 == 0
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_set_auto_start(enabled: bool) -> Result<(), PlatformError> {
    windows_impl::set(enabled)
}

#[cfg(target_os = "windows")]
fn windows_is_auto_start() -> bool {
    windows_impl::is_set()
}

// ---------------------------------------------------------------------------
// Linux：XDG autostart desktop 文件
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn autostart_file() -> PathBuf {
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".config"))
                    .unwrap_or_default()
            });
        config_dir.join("autostart").join("black-hole-ime.desktop")
    }

    pub(super) fn set(enabled: bool) -> Result<(), PlatformError> {
        let path = autostart_file();
        if enabled {
            let exe = std::env::current_exe()
                .map_err(|e| PlatformError::Other(format!("cannot locate current exe: {e}")))?;
            let content = format!(
                "[Desktop Entry]\nType=Application\nName=Black-Hole IME\nComment=Black-Hole IME\nExec={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe.to_string_lossy()
            );
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    PlatformError::Other(format!("failed to create autostart dir: {e}"))
                })?;
            }
            fs::write(&path, content)
                .map_err(|e| PlatformError::Other(format!("failed to write autostart file: {e}")))?;
            Ok(())
        } else {
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(PlatformError::Other(format!(
                    "failed to remove autostart file: {e}"
                ))),
            }
        }
    }

    pub(super) fn is_set() -> bool {
        autostart_file().exists()
    }
}

#[cfg(target_os = "linux")]
fn linux_set_auto_start(enabled: bool) -> Result<(), PlatformError> {
    linux_impl::set(enabled)
}

#[cfg(target_os = "linux")]
fn linux_is_auto_start() -> bool {
    linux_impl::is_set()
}
