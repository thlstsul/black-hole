use std::path::Path;
use windows::Win32::Foundation::{CloseHandle, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_core::PCWSTR;

use super::CLSID_BLACKHOLE_TIP;

// ---------------------------------------------------------------------------
// Registration check
// ---------------------------------------------------------------------------

/// 检查输入法是否已在注册表中注册。
///
/// 通过检测 `HKEY_CLASSES_ROOT\CLSID\{CLSID_BLACKHOLE_TIP}` 是否存在来判断。
pub fn is_registered() -> bool {
    unsafe {
        let clsid_path = format!("CLSID\\{{{:?}}}", CLSID_BLACKHOLE_TIP);
        let clsid_path_w: Vec<u16> = clsid_path.encode_utf16().chain(Some(0)).collect();
        let mut hkey = std::mem::zeroed();
        let result = RegOpenKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(clsid_path_w.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        );
        if result.is_ok() {
            let _ = RegCloseKey(hkey);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Auto registration
// ---------------------------------------------------------------------------

/// 以管理员权限自动注册 IME DLL。
///
/// 使用 `ShellExecuteExW` + `"runas"` 动词启动 `regsvr32.exe /s <dll_path>`，
/// 并等待进程结束获取退出码。
///
/// # Errors
///
/// - 当 DLL 路径包含非法字符或参数构造失败时返回错误
/// - 当无法启动提权进程（如用户拒绝 UAC）时返回错误
/// - 当 `regsvr32` 返回非零退出码时返回错误
pub fn register_ime(dll_path: &Path) -> std::io::Result<()> {
    let dll_path_str = dll_path.to_string_lossy();
    let params = format!("/s \"{}\"", dll_path_str);

    let file = null_terminated_wide("regsvr32.exe");
    let verb = null_terminated_wide("runas");
    let parameters = null_terminated_wide(&params);

    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.hwnd = HWND(std::ptr::null_mut());
    sei.lpVerb = PCWSTR(verb.as_ptr());
    sei.lpFile = PCWSTR(file.as_ptr());
    sei.lpParameters = PCWSTR(parameters.as_ptr());
    sei.nShow = 0;

    unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| {
        std::io::Error::other(format!(
            "Failed to launch regsvr32 with elevated privileges: {}",
            e
        ))
    })?;

    // hProcess is only valid when SEE_MASK_NOCLOSEPROCESS is set
    let h_process = sei.hProcess;
    if h_process.is_invalid() {
        return Err(std::io::Error::other(
            "ShellExecuteExW did not return a process handle",
        ));
    }

    // Wait up to 30 seconds for regsvr32 to finish
    let wait_result = unsafe { WaitForSingleObject(h_process, 30_000) };
    let exit_code = if wait_result == WAIT_OBJECT_0 {
        let mut code: u32 = 0;
        let _ = unsafe { GetExitCodeProcess(h_process, &mut code) };
        code
    } else {
        let _ = unsafe { CloseHandle(h_process) };
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "regsvr32 did not complete within 30 seconds",
        ));
    };

    let _ = unsafe { CloseHandle(h_process) };

    if exit_code != 0 {
        return Err(std::io::Error::other(format!(
            "regsvr32 exited with code {}",
            exit_code
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn null_terminated_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
