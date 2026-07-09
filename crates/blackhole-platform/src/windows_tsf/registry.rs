use super::{
    CLSID_BLACKHOLE_TIP, CLSID_TF_INPUTPROCESSORPROFILES, GUID_PROFILE_BLACKHOLE, TIP_DISPLAY_NAME,
    TIP_PROFILE_NAME, get_dll_instance,
};
use windows::Win32::Foundation::{E_FAIL, HMODULE, S_OK};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, REG_DWORD, REG_SZ, RegCloseKey, RegCreateKeyW,
    RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::UI::TextServices::{
    CLSID_TF_CategoryMgr, CLSID_TF_InputProcessorProfiles, GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
    GUID_TFCAT_TIP_KEYBOARD, GUID_TFCAT_TIPCAP_COMLESS, GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
    GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT, GUID_TFCAT_TIPCAP_SECUREMODE,
    GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT, GUID_TFCAT_TIPCAP_UIELEMENTENABLED, ITfCategoryMgr,
    ITfInputProcessorProfileMgr, ITfInputProcessorProfiles,
};
use windows_core::{GUID, HRESULT, PCWSTR, w};

// ---------------------------------------------------------------------------
// Registry helpers
// ---------------------------------------------------------------------------

unsafe fn set_reg_value(hkey: HKEY, name: PCWSTR, value: PCWSTR) {
    let data = unsafe { value.as_wide() };
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
    let _ = unsafe { RegSetValueExW(hkey, name, Some(0), REG_SZ, Some(bytes)) };
}

unsafe fn set_reg_dword(hkey: HKEY, name: PCWSTR, value: u32) {
    let bytes = value.to_le_bytes();
    let _ = unsafe { RegSetValueExW(hkey, name, Some(0), REG_DWORD, Some(&bytes)) };
}

unsafe fn create_key(root: HKEY, path: PCWSTR) -> windows_core::Result<HKEY> {
    let mut hkey = unsafe { std::mem::zeroed() };
    let result = unsafe { RegCreateKeyW(root, path, &mut hkey) };
    if result.is_err() {
        return Err(result.into());
    }
    Ok(hkey)
}

unsafe fn register_language_profile(
    root: HKEY,
    clsid: &GUID,
    profile: &GUID,
    langid: &str,
) -> windows_core::Result<()> {
    let lp_path = format!(
        "SOFTWARE\\Microsoft\\CTF\\TIP\\{{{:?}}}\\LanguageProfile\\{}\\{{{:?}}}",
        clsid, langid, profile
    );
    let lp_path_w: Vec<u16> = lp_path.encode_utf16().chain(Some(0)).collect();
    let hkey = unsafe { create_key(root, PCWSTR(lp_path_w.as_ptr()))? };
    unsafe { set_reg_value(hkey, PCWSTR(std::ptr::null()), TIP_PROFILE_NAME) };
    unsafe { set_reg_dword(hkey, w!("Enable"), 1) };
    let _ = unsafe { RegCloseKey(hkey) };
    Ok(())
}

/// TSF categories to register (matches SampleIME).
const SUPPORT_CATEGORIES: &[GUID] = &[
    GUID_TFCAT_TIP_KEYBOARD,
    GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
    GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
    GUID_TFCAT_TIPCAP_SECUREMODE,
    GUID_TFCAT_TIPCAP_COMLESS,
    GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
    GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
    GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
];

/// Language IDs for which we register a profile.
const PROFILE_LANGIDS: &[(u16, &str)] = &[(0x0804, "0804")];

// ---------------------------------------------------------------------------
// DllRegisterServer
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
extern "system" fn DllRegisterServer() -> HRESULT {
    unsafe {
        // 1. COM registration under HKEY_CLASSES_ROOT
        let clsid_path = format!("CLSID\\{{{:?}}}", CLSID_BLACKHOLE_TIP);
        let clsid_path_w: Vec<u16> = clsid_path.encode_utf16().chain(Some(0)).collect();
        let hkey = match create_key(HKEY_CLASSES_ROOT, PCWSTR(clsid_path_w.as_ptr())) {
            Ok(h) => h,
            Err(_) => return E_FAIL,
        };
        set_reg_value(hkey, PCWSTR(std::ptr::null()), TIP_DISPLAY_NAME);
        let _ = RegCloseKey(hkey);

        // InprocServer32
        let server_path = format!("CLSID\\{{{:?}}}\\InprocServer32", CLSID_BLACKHOLE_TIP);
        let server_path_w: Vec<u16> = server_path.encode_utf16().chain(Some(0)).collect();
        let hkey = match create_key(HKEY_CLASSES_ROOT, PCWSTR(server_path_w.as_ptr())) {
            Ok(h) => h,
            Err(_) => return E_FAIL,
        };
        let mut dll_path = [0u16; 512];
        let instance = get_dll_instance().unwrap_or_default();
        let _len = GetModuleFileNameW(Some(HMODULE(instance.0)), &mut dll_path);
        let dll_path_pcw = PCWSTR(dll_path.as_ptr());
        set_reg_value(hkey, PCWSTR(std::ptr::null()), dll_path_pcw);
        set_reg_value(hkey, w!("ThreadingModel"), w!("Apartment"));
        let _ = RegCloseKey(hkey);

        // Category registration
        let cat_path = format!(
            "CLSID\\{{{:?}}}\\Implemented Categories\\{{{:?}}}",
            CLSID_BLACKHOLE_TIP, GUID_TFCAT_TIP_KEYBOARD
        );
        let cat_path_w: Vec<u16> = cat_path.encode_utf16().chain(Some(0)).collect();
        let hkey = match create_key(HKEY_CLASSES_ROOT, PCWSTR(cat_path_w.as_ptr())) {
            Ok(h) => h,
            Err(_) => return E_FAIL,
        };
        let _ = RegCloseKey(hkey);

        // 2. TSF/CTF registration under HKEY_LOCAL_MACHINE (64-bit view)
        let ctf_tip_path = format!(
            "SOFTWARE\\Microsoft\\CTF\\TIP\\{{{:?}}}",
            CLSID_BLACKHOLE_TIP
        );
        let ctf_tip_path_w: Vec<u16> = ctf_tip_path.encode_utf16().chain(Some(0)).collect();
        let hkey = match create_key(HKEY_LOCAL_MACHINE, PCWSTR(ctf_tip_path_w.as_ptr())) {
            Ok(h) => h,
            Err(_) => return E_FAIL,
        };
        set_reg_value(hkey, PCWSTR(std::ptr::null()), TIP_DISPLAY_NAME);
        let _ = RegCloseKey(hkey);

        for (_, langid_str) in PROFILE_LANGIDS {
            if register_language_profile(
                HKEY_LOCAL_MACHINE,
                &CLSID_BLACKHOLE_TIP,
                &GUID_PROFILE_BLACKHOLE,
                langid_str,
            )
            .is_err()
            {
                return E_FAIL;
            }
        }

        // 3. TSF/CTF registration under WOW6432Node (for 32-bit TSF clients)
        let ctf_tip_path_wow = format!(
            "SOFTWARE\\WOW6432Node\\Microsoft\\CTF\\TIP\\{{{:?}}}",
            CLSID_BLACKHOLE_TIP
        );
        let ctf_tip_path_wow_w: Vec<u16> = ctf_tip_path_wow.encode_utf16().chain(Some(0)).collect();
        if let Ok(hkey) = create_key(HKEY_LOCAL_MACHINE, PCWSTR(ctf_tip_path_wow_w.as_ptr())) {
            set_reg_value(hkey, PCWSTR(std::ptr::null()), TIP_DISPLAY_NAME);
            let _ = RegCloseKey(hkey);

            for (_, langid_str) in PROFILE_LANGIDS {
                let _ = register_language_profile(
                    HKEY_LOCAL_MACHINE,
                    &CLSID_BLACKHOLE_TIP,
                    &GUID_PROFILE_BLACKHOLE,
                    langid_str,
                );
            }
        }

        // 4. Register profiles and categories via TSF APIs
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let com_initialized = hr.is_ok();

        if com_initialized || hr.0 == 0x80010106u32 as i32 {
            if let Ok(profile_mgr) = CoCreateInstance::<
                Option<&windows_core::IUnknown>,
                ITfInputProcessorProfileMgr,
            >(
                &CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER
            ) {
                let desc: Vec<u16> = "Blackhole IME".encode_utf16().collect();
                let icon_file: Vec<u16> = {
                    let mut path = [0u16; 512];
                    let instance = get_dll_instance().unwrap_or_default();
                    let _len = GetModuleFileNameW(Some(HMODULE(instance.0)), &mut path);
                    let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                    path[..len].to_vec()
                };

                for (langid, _) in PROFILE_LANGIDS {
                    let _ = profile_mgr.RegisterProfile(
                        &CLSID_BLACKHOLE_TIP,
                        *langid,
                        &GUID_PROFILE_BLACKHOLE,
                        &desc,
                        &icon_file,
                        0,
                        windows::Win32::UI::Input::KeyboardAndMouse::HKL(std::ptr::null_mut()),
                        0,
                        true,
                        0,
                    );
                }
            }

            if let Ok(cat_mgr) = CoCreateInstance::<Option<&windows_core::IUnknown>, ITfCategoryMgr>(
                &CLSID_TF_CategoryMgr,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                for cat in SUPPORT_CATEGORIES {
                    let _ =
                        cat_mgr.RegisterCategory(&CLSID_BLACKHOLE_TIP, cat, &CLSID_BLACKHOLE_TIP);
                }
            }

            if let Ok(profiles) = CoCreateInstance::<
                Option<&windows_core::IUnknown>,
                ITfInputProcessorProfiles,
            >(
                &CLSID_TF_INPUTPROCESSORPROFILES, None, CLSCTX_INPROC_SERVER
            ) {
                let _ = profiles.Register(&CLSID_BLACKHOLE_TIP);
            }

            if com_initialized {
                CoUninitialize();
            }
        }
    }
    S_OK
}

// ---------------------------------------------------------------------------
// DllUnregisterServer
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
extern "system" fn DllUnregisterServer() -> HRESULT {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let com_initialized = hr.is_ok();

        if com_initialized || hr.0 == 0x80010106u32 as i32 {
            if let Ok(profile_mgr) = CoCreateInstance::<
                Option<&windows_core::IUnknown>,
                ITfInputProcessorProfileMgr,
            >(
                &CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER
            ) {
                for (langid, _) in PROFILE_LANGIDS {
                    let _ = profile_mgr.UnregisterProfile(
                        &CLSID_BLACKHOLE_TIP,
                        *langid,
                        &GUID_PROFILE_BLACKHOLE,
                        0,
                    );
                }
            }

            if let Ok(cat_mgr) = CoCreateInstance::<Option<&windows_core::IUnknown>, ITfCategoryMgr>(
                &CLSID_TF_CategoryMgr,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                for cat in SUPPORT_CATEGORIES {
                    let _ =
                        cat_mgr.UnregisterCategory(&CLSID_BLACKHOLE_TIP, cat, &CLSID_BLACKHOLE_TIP);
                }
            }

            if com_initialized {
                CoUninitialize();
            }
        }

        // Remove COM registration
        let clsid_path = format!("CLSID\\{{{:?}}}", CLSID_BLACKHOLE_TIP);
        let clsid_path_w: Vec<u16> = clsid_path.encode_utf16().chain(Some(0)).collect();
        let _ = RegDeleteTreeW(HKEY_CLASSES_ROOT, PCWSTR(clsid_path_w.as_ptr()));

        // Remove TSF/CTF registration (64-bit)
        let ctf_tip_path = format!(
            "SOFTWARE\\Microsoft\\CTF\\TIP\\{{{:?}}}",
            CLSID_BLACKHOLE_TIP
        );
        let ctf_tip_path_w: Vec<u16> = ctf_tip_path.encode_utf16().chain(Some(0)).collect();
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(ctf_tip_path_w.as_ptr()));

        // Remove WOW6432Node registration
        let ctf_tip_path_wow = format!(
            "SOFTWARE\\WOW6432Node\\Microsoft\\CTF\\TIP\\{{{:?}}}",
            CLSID_BLACKHOLE_TIP
        );
        let ctf_tip_path_wow_w: Vec<u16> = ctf_tip_path_wow.encode_utf16().chain(Some(0)).collect();
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(ctf_tip_path_wow_w.as_ptr()));
    }
    S_OK
}
