use std::io::{self, ErrorKind};
use std::mem;
use std::path::Path;
use std::ptr;
use tracing::debug;
use windows::Win32::Foundation::{CloseHandle, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::TextServices::{
    CLSID_TF_InputProcessorProfiles, ITfInputProcessorProfileMgr, ITfInputProcessorProfiles,
    TF_IPP_FLAG_ENABLED, TF_PROFILETYPE_INPUTPROCESSOR,
};
use windows_core::{IUnknown, PCWSTR};

use super::registry::DEFAULT_LANGID;
use super::{CLSID_BLACKHOLE_TIP, GUID_PROFILE_BLACKHOLE};

/// 输入法注册的语言 ID（与 registry.rs 中 PROFILE_LANGIDS 同源）。
const LANGID: u16 = DEFAULT_LANGID;

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
        let mut hkey = mem::zeroed();
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
pub fn register_ime(dll_path: &Path) -> io::Result<()> {
    let dll_path_str = dll_path.to_string_lossy();
    let params = format!("/s \"{}\"", dll_path_str);

    let file = null_terminated_wide("regsvr32.exe");
    let verb = null_terminated_wide("runas");
    let parameters = null_terminated_wide(&params);

    let mut sei: SHELLEXECUTEINFOW = unsafe { mem::zeroed() };
    sei.cbSize = mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.hwnd = HWND(ptr::null_mut());
    sei.lpVerb = PCWSTR(verb.as_ptr());
    sei.lpFile = PCWSTR(file.as_ptr());
    sei.lpParameters = PCWSTR(parameters.as_ptr());
    sei.nShow = 0;

    unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| {
        io::Error::other(format!(
            "Failed to launch regsvr32 with elevated privileges: {}",
            e
        ))
    })?;

    // hProcess is only valid when SEE_MASK_NOCLOSEPROCESS is set
    let h_process = sei.hProcess;
    if h_process.is_invalid() {
        return Err(io::Error::other(
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
        return Err(io::Error::new(
            ErrorKind::TimedOut,
            "regsvr32 did not complete within 30 seconds",
        ));
    };

    let _ = unsafe { CloseHandle(h_process) };

    if exit_code != 0 {
        return Err(io::Error::other(format!(
            "regsvr32 exited with code {}",
            exit_code
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Enable keyboard (add to input method list)
// ---------------------------------------------------------------------------

/// 检查输入法语言配置文件是否已启用（即已添加到输入法列表）。
///
/// 通过 `ITfInputProcessorProfileMgr::GetProfile` 查询：返回 `Ok` 且
/// `dwFlags` 含 `TF_IPP_FLAG_ENABLED` 时视为已启用。仅注册（regsvr32）不会
/// 自动启用配置文件，需要此检查判断用户是否已手动添加键盘。
pub fn is_profile_enabled() -> bool {
    unsafe {
        // TSF COM 对象在未初始化 COM 的进程中也可按进程内方式创建；
        // RPC_E_CHANGED_MODE 表示线程已用其他并发模型初始化过 COM，仍可直接使用，
        // 但此时不能调用 CoUninitialize（本次调用未添加引用）。
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let com_initialized = hr.is_ok();

        let enabled = CoCreateInstance::<Option<&IUnknown>, ITfInputProcessorProfileMgr>(
            &CLSID_TF_InputProcessorProfiles,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .map(|profile_mgr| {
            let mut profile = mem::zeroed();
            match profile_mgr.GetProfile(
                TF_PROFILETYPE_INPUTPROCESSOR,
                LANGID,
                &CLSID_BLACKHOLE_TIP,
                &GUID_PROFILE_BLACKHOLE,
                HKL(ptr::null_mut()),
                &mut profile,
            ) {
                Ok(()) => profile.dwFlags & TF_IPP_FLAG_ENABLED != 0,
                Err(e) => {
                    debug!("GetProfile failed, treating as not enabled: {}", e);
                    false
                }
            }
        })
        .inspect_err(|e| {
            debug!("Failed to create TSF profile manager instance: {}", e);
        })
        .unwrap_or(false);

        if com_initialized {
            CoUninitialize();
        }
        enabled
    }
}

/// 将输入法启用到当前用户输入法列表（等效于设置中"添加键盘"）。
///
/// `ITfInputProcessorProfiles::EnableLanguageProfile` 是按当前用户（HKCU）
/// 生成的启用状态写入，无需管理员权限。对已启用的配置文件重复调用是幂等的。
///
/// # Errors
///
/// - COM 初始化或接口创建失败时返回错误
/// - TSF 拒绝启用该配置文件时返回错误
pub fn enable_keyboard() -> io::Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let com_initialized = hr.is_ok();

        let result = CoCreateInstance::<Option<&IUnknown>, ITfInputProcessorProfiles>(
            &CLSID_TF_InputProcessorProfiles,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(|e| io::Error::other(format!("Failed to create TSF profile instance: {}", e)))
        .and_then(|profiles| {
            profiles
                .EnableLanguageProfile(&CLSID_BLACKHOLE_TIP, LANGID, &GUID_PROFILE_BLACKHOLE, true)
                .map_err(|e| io::Error::other(format!("EnableLanguageProfile failed: {}", e)))
        });

        if com_initialized {
            CoUninitialize();
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn null_terminated_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
