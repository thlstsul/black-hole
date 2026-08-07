#![cfg(target_os = "windows")]
//! Windows：Run 注册表键

use super::*;
use std::env;
use std::mem;
use std::slice;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyW,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_core::PCWSTR;

/// 当前用户 Run 键路径
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 自启动注册表值名称
const RUN_VALUE_NAME: &str = "BlackHoleIME";

/// 打开 Run 键（不存在时创建）
unsafe fn open_run_key(create: bool) -> Result<HKEY, PlatformError> {
    let key_w: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
    let mut hkey = unsafe { mem::zeroed() };
    let result = unsafe {
        if create {
            // RegCreateKeyW 等价于带 KEY_ALL_ACCESS 的打开/创建
            RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr()), &mut hkey)
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
    let exe = env::current_exe()
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
            let bytes = slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2);
            let name_w = value_name_w();
            let result =
                RegSetValueExW(hkey, PCWSTR(name_w.as_ptr()), Some(0), REG_SZ, Some(bytes));
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
