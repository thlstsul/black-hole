use super::commit::apply_result;
use super::{IpcConnection, ServiceInner};
use black_hole_shared::{KeyEvent, KeyState, Modifiers};
use crate::ipc::{IPC_SERVER_ADDR, IpcRequest, read_response, send_request};
use std::io::BufReader;
use std::net::TcpStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{error, warn};
use windows::Win32::Foundation::{E_UNEXPECTED, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_SPACE, VK_UP, VIRTUAL_KEY,
};
use windows::Win32::UI::TextServices::{ITfEditSession, ITfEditSession_Impl};
use windows_core::{BOOL, Error, Result, implement};

// External Win32 functions not provided by the windows crate
unsafe extern "system" {
    fn MapVirtualKeyW(uCode: u32, uMapType: u32) -> u32;
    fn GetKeyboardState(lpKeyState: *mut u8) -> BOOL;
    fn ToUnicode(
        wVirtKey: u32,
        wScanCode: u32,
        lpKeyState: *const u8,
        pwszBuff: *mut u16,
        cchBuff: i32,
        wFlags: u32,
    ) -> i32;
}

/// Convert a Win32 virtual-key code into our internal `KeyEvent` representation.
pub(crate) fn virtual_key_to_key_event(
    vk: VIRTUAL_KEY,
    _wparam: WPARAM,
    _lparam: LPARAM,
    state: KeyState,
) -> Option<KeyEvent> {
    let vk_val = vk.0 as u32;

    let scan_code = unsafe { MapVirtualKeyW(vk_val, 0) };
    let mut kbd_state = [0u8; 256];
    let mut wch = [0u16; 8];
    let key_char = if unsafe { GetKeyboardState(kbd_state.as_mut_ptr()) }.as_bool() {
        let len = unsafe {
            ToUnicode(
                vk_val,
                scan_code,
                kbd_state.as_ptr(),
                wch.as_mut_ptr(),
                wch.len() as i32,
                0,
            )
        };
        if len > 0 {
            let slice = &wch[..len as usize];
            char::decode_utf16(slice.iter().copied())
                .filter_map(|r| r.ok())
                .next()
        } else {
            None
        }
    } else {
        None
    };

    let key = match vk {
        VK_BACK => "Backspace".to_string(),
        VK_ESCAPE => "Escape".to_string(),
        VK_RETURN => "Enter".to_string(),
        VK_SPACE => "Space".to_string(),
        VK_LEFT => "ArrowLeft".to_string(),
        VK_RIGHT => "ArrowRight".to_string(),
        VK_UP => "ArrowUp".to_string(),
        VK_DOWN => "ArrowDown".to_string(),
        _ => {
            if let Some(ch) = key_char {
                if !ch.is_ascii_alphanumeric() && !ch.is_ascii_punctuation() {
                    return None;
                }
                ch.to_string()
            } else if (0x30..=0x39).contains(&vk_val) {
                ((vk_val as u8 - 0x30 + b'0') as char).to_string()
            } else if (0x41..=0x5A).contains(&vk_val) {
                ((vk_val as u8 - 0x41 + b'a') as char).to_string()
            } else {
                return None;
            }
        }
    };

    let shift =
        unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) }
            < 0;
    let ctrl = unsafe {
        GetAsyncKeyState(VK_CONTROL.0 as i32)
    } < 0;
    let alt = unsafe { GetAsyncKeyState(0x12i32) } < 0;
    let capslock = (kbd_state[0x14] & 0x01) != 0;

    Some(KeyEvent {
        key,
        modifiers: Modifiers {
            shift,
            ctrl,
            alt,
            meta: false,
            capslock,
        },
        state,
    })
}

// ---------------------------------------------------------------------------
// EditSession for key handling
// ---------------------------------------------------------------------------

#[implement(ITfEditSession)]
pub(crate) struct KeyHandlerEditSession {
    pub(crate) service: Arc<Mutex<ServiceInner>>,
    pub(crate) key_event: KeyEvent,
}

impl ITfEditSession_Impl for KeyHandlerEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let service = self.service.clone();
        let key_event = self.key_event.clone();

        match handle_key_event_with_reconnect(&service, ec, key_event) {
            Ok(()) => Ok(()),
            Err(e) => {
                error!("DoEditSession: failed with error: {:?}", e);
                Err(e)
            }
        }
    }
}

/// Handle key event with automatic IPC reconnection support.
pub(crate) fn handle_key_event_with_reconnect(
    service: &Arc<Mutex<ServiceInner>>,
    ec: u32,
    key_event: KeyEvent,
) -> Result<()> {
    let result = handle_key_event_internal(service, ec, &key_event);

    if result.is_err() {
        warn!("IPC operation failed, clearing connection and retrying");

        {
            let mut inner = service.lock().unwrap();
            inner.ipc_conn = None;
        }

        let max_retries = 2;
        let mut retry_delay_ms = 300;

        for attempt in 1..=max_retries {
            match TcpStream::connect(IPC_SERVER_ADDR) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    let reader = BufReader::new(stream.try_clone().map_err(|_| E_UNEXPECTED)?);

                    {
                        let mut inner = service.lock().unwrap();
                        inner.ipc_conn = Some(IpcConnection {
                            writer: stream,
                            reader,
                        });
                    }

                    let retry_result = handle_key_event_internal(service, ec, &key_event);
                    if retry_result.is_ok() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    warn!("reconnection attempt {} failed: {}", attempt, e);
                }
            }

            if attempt < max_retries {
                thread::sleep(Duration::from_millis(retry_delay_ms));
                retry_delay_ms = (retry_delay_ms * 2).min(2000);
            }
        }

        error!("all reconnection attempts failed");
    }

    result
}

/// Internal key event handling logic (assumes connection exists).
fn handle_key_event_internal(
    service: &Arc<Mutex<ServiceInner>>,
    ec: u32,
    key_event: &KeyEvent,
) -> Result<()> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut inner = service.lock().unwrap();
        let ctx = inner.context.clone().ok_or(E_UNEXPECTED)?;
        let conn = inner.ipc_conn.as_mut().ok_or(E_UNEXPECTED)?;

        let request = IpcRequest::KeyEvent(key_event.clone());
        send_request(&mut conn.writer, &request).map_err(|_| E_UNEXPECTED)?;

        let response = read_response(&mut conn.reader).map_err(|_| E_UNEXPECTED)?;

        drop(inner);
        apply_result(service.clone(), ec, &ctx, &response.into())?;
        Ok::<(), Error>(())
    }));

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(E_UNEXPECTED.into()),
    }
}
