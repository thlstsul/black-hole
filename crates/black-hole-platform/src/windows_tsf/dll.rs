use super::service::BlackHoleTextService;
use super::{CLSID_BLACKHOLE_TIP, GLOBAL_REF_COUNT, dll_add_ref, dll_release, set_dll_instance};
#[cfg(debug_assertions)]
use std::env;
use std::ffi::c_void;
#[cfg(debug_assertions)]
use std::fs;
#[cfg(debug_assertions)]
use std::process;
use std::ptr;
#[cfg(debug_assertions)]
use std::sync::Once;
#[cfg(debug_assertions)]
use tracing::Level;
#[cfg(debug_assertions)]
use tracing_appender::non_blocking;
#[cfg(debug_assertions)]
use tracing_subscriber::fmt;
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, HINSTANCE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement};

// ---------------------------------------------------------------------------
// COM object: BlackHoleClassFactory
// ---------------------------------------------------------------------------

#[implement(IClassFactory)]
pub struct BlackHoleClassFactory;

impl IClassFactory_Impl for BlackHoleClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if ppv.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *ppv = ptr::null_mut();
        }

        let service = BlackHoleTextService::new();
        let unknown: IUnknown = service.into();
        unsafe {
            unknown.query(riid, ppv).ok()?;
        }
        dll_add_ref();
        Ok(())
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        if flock.as_bool() {
            dll_add_ref();
        } else {
            dll_release();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DLL exports
// ---------------------------------------------------------------------------

const DLL_PROCESS_ATTACH: u32 = 1;

#[cfg(debug_assertions)]
static INIT_LOGS: Once = Once::new();

/// Initialize tracing subscriber to write DLL logs to a file.
/// Each process gets its own log file under `%TEMP%`.
/// Only active in debug builds.
#[cfg(debug_assertions)]
fn init_dll_logging() {
    INIT_LOGS.call_once(|| {
        let temp_dir = env::temp_dir();
        let pid = process::id();
        let log_path = temp_dir.join(format!("black_hole_tsf_{}.log", pid));

        let file = match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => f,
            Err(_) => return,
        };

        let (non_blocking, _guard) = non_blocking(file);
        let _ = Box::leak(Box::new(_guard));

        let _ = fmt()
            .with_writer(non_blocking)
            .with_max_level(Level::DEBUG)
            .with_ansi(false)
            .with_thread_ids(true)
            .with_line_number(true)
            .with_target(true)
            .with_level(true)
            .try_init();
    });
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(instance: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        set_dll_instance(instance);
        #[cfg(debug_assertions)]
        init_dll_logging();
    }
    BOOL(1)
}

#[unsafe(no_mangle)]
extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = ptr::null_mut();
    }

    if rclsid.is_null() || unsafe { *rclsid } != CLSID_BLACKHOLE_TIP {
        return CLASS_E_CLASSNOTAVAILABLE;
    }

    let factory: IClassFactory = BlackHoleClassFactory.into();
    let result = unsafe { factory.query(riid, ppv) };
    if result.is_ok() {
        dll_add_ref();
    }
    result
}

#[unsafe(no_mangle)]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    let count = *GLOBAL_REF_COUNT.lock().unwrap();
    if count == 0 { S_OK } else { S_FALSE }
}
