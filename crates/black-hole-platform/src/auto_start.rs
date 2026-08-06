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
mod windows_impl;

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
mod linux_impl;

#[cfg(target_os = "linux")]
fn linux_set_auto_start(enabled: bool) -> Result<(), PlatformError> {
    linux_impl::set(enabled)
}

#[cfg(target_os = "linux")]
fn linux_is_auto_start() -> bool {
    linux_impl::is_set()
}
