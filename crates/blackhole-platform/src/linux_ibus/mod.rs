//! Linux IBus Engine DBus 服务实现
//!
//! 基于 zbus 实现 `org.freedesktop.IBus.Engine` 接口，通过 daemon 提供的
//! channel 与引擎线程通信处理按键，并通过 `blackhole_ui` 显示候选窗口。

use blackhole_shared::{
    EngineCommand, InputContext, KeyEvent, KeyState, Modifiers, SchemeResult, UiCommand,
};
use std::sync::mpsc::{Receiver, Sender};
use zbus::{Connection, interface};

use super::{PlatformError, PlatformIme};

pub mod auto_register;

/// Linux IBus 输入法平台实现
pub struct LinuxIbusIme;

impl LinuxIbusIme {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformIme for LinuxIbusIme {
    fn run(
        &mut self,
        engine_tx: Sender<EngineCommand>,
        platform_rx: Receiver<SchemeResult>,
        ui_tx: Sender<UiCommand>,
    ) -> Result<(), PlatformError> {
        // IBus Engine DBus 服务，通过 daemon 提供的 channel 与引擎线程通信
        pollster::block_on(self.run_async(engine_tx, platform_rx, ui_tx))
            .map_err(|e| PlatformError::Dbus(e.to_string()))?;

        Ok(())
    }
}

impl LinuxIbusIme {
    async fn run_async(
        &self,
        engine_tx: Sender<EngineCommand>,
        platform_rx: Receiver<SchemeResult>,
        ui_tx: Sender<UiCommand>,
    ) -> Result<(), zbus::Error> {
        let conn = Connection::session().await?;

        let ibus_engine = IbusEngine {
            engine_tx: Mutex::new(engine_tx),
            platform_rx: Mutex::new(platform_rx),
            ui_tx: Mutex::new(ui_tx),
            context: Mutex::new(InputContext::default()),
            conn: conn.clone(),
        };

        conn.object_server()
            .at("/org/freedesktop/IBus/Engine/Blackhole", ibus_engine)
            .await?;

        // 保持连接活跃
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// IBus Engine DBus 对象
///
/// 通过 channel 与 daemon 中的引擎线程通信。
struct IbusEngine {
    engine_tx: Mutex<Sender<EngineCommand>>,
    platform_rx: Mutex<Receiver<SchemeResult>>,
    ui_tx: Mutex<Sender<UiCommand>>,
    context: Mutex<InputContext>,
    conn: Connection,
}

#[interface(name = "org.freedesktop.IBus.Engine")]
impl IbusEngine {
    /// 处理按键事件
    /// 返回 true 表示按键被消费，false 表示转发给应用
    async fn process_key_event(&self, keyval: u32, _keycode: u32, state: u32) -> bool {
        let key = convert_keyval(keyval, state);
        let Some(key) = key else { return false };

        // 发送 Key 命令到引擎线程
        {
            let engine_tx = self.engine_tx.lock().unwrap();
            if engine_tx.send(EngineCommand::Key(key)).is_err() {
                return false;
            }
        }

        // 从引擎线程接收处理结果
        let result = {
            let platform_rx = self.platform_rx.lock().unwrap();
            match platform_rx.recv() {
                Ok(r) => r,
                Err(_) => return false,
            }
        };

        match result {
            SchemeResult::Committed { text } => {
                // 发送 CommitText DBus 信号
                let ibus_text = (text.as_str(), Vec::<(u32, u32, u32, u32)>::new());
                let variant = zbus::zvariant::Value::from(ibus_text);

                let _ = self
                    .conn
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/IBus/Engine/Blackhole",
                        "org.freedesktop.IBus.Engine",
                        "CommitText",
                        &variant,
                    )
                    .await;

                true
            }
            SchemeResult::Composing {
                code,
                candidates,
                selected_index,
                expanded,
            } => {
                // 通过 channel 发送 UI 更新
                let ui_tx = self.ui_tx.lock().unwrap();
                let ctx = self.context.lock().unwrap().clone();
                let _ = ui_tx.send(UiCommand::ShowCandidates {
                    code,
                    candidates,
                    selected_index,
                    expanded,
                    context: ctx,
                });
                true
            }
            SchemeResult::Ignored => false,
        }
    }

    /// 更新光标位置
    async fn set_cursor_location(&self, x: i32, y: i32, _w: i32, h: i32) {
        let engine_tx = self.engine_tx.lock().unwrap();
        let ctx = InputContext {
            caret_x: x,
            caret_y: y,
            caret_h: h,
        };
        *self.context.lock().unwrap() = ctx.clone();
        let _ = engine_tx.send(EngineCommand::SetContext(ctx));
    }

    async fn focus_in(&self) {}
    async fn focus_out(&self) {}

    async fn reset(&self) {
        // 发送 Reset 命令到引擎线程并读取响应
        {
            let engine_tx = self.engine_tx.lock().unwrap();
            if engine_tx.send(EngineCommand::Reset).is_err() {
                return;
            }
        }
        let platform_rx = self.platform_rx.lock().unwrap();
        let _ = platform_rx.recv();
    }

    async fn enable(&self) {}
    async fn disable(&self) {}
    async fn set_capabilities(&self, _caps: u32) {}
    async fn page_up(&self) -> bool {
        false
    }
    async fn page_down(&self) -> bool {
        false
    }
    async fn cursor_up(&self) -> bool {
        false
    }
    async fn cursor_down(&self) -> bool {
        false
    }
    async fn property_activate(&self, _prop_name: String, _prop_state: i32) {}
    async fn candidate_clicked(&self, _index: u32, _button: u32, _state: u32) {}
}

/// 将 IBus keyval + state 转换为内部 KeyEvent
fn convert_keyval(keyval: u32, state: u32) -> Option<KeyEvent> {
    let key = match keyval {
        0xFF08 => "Backspace".to_string(),
        0xFF0D => "Enter".to_string(),
        0xFF1B => "Escape".to_string(),
        0x0020 => "Space".to_string(),
        0xFF52 => "ArrowUp".to_string(),
        0xFF54 => "ArrowDown".to_string(),
        0xFF51 => "ArrowLeft".to_string(),
        0xFF53 => "ArrowRight".to_string(),
        0x30..=0x39 => ((keyval as u8 - 0x30 + b'0') as char).to_string(),
        0x41..=0x5A => ((keyval as u8 - 0x41 + b'a') as char).to_string(),
        0x61..=0x7A => ((keyval as u8 - 0x61 + b'a') as char).to_string(),
        0x3B => ";".to_string(),
        0x21..=0x2F | 0x3A..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E => (keyval as u8 as char).to_string(),
        _ => return None,
    };

    let shift = (state & 1) != 0;
    let ctrl = (state & 4) != 0;
    let alt = (state & 8) != 0;
    let meta = (state & 0x10000000) != 0;

    Some(KeyEvent {
        key,
        modifiers: Modifiers {
            shift,
            ctrl,
            alt,
            meta,
            capslock: false,
        },
        state: KeyState::Press,
    })
}
