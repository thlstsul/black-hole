//! Linux IBus Engine DBus 服务实现
//!
//! 基于 zbus 实现 `org.freedesktop.IBus.Engine` 接口，通过 daemon 提供的
//! channel 与引擎线程通信处理按键，并通过 `black_hole_ui` 显示候选窗口。

use black_hole_shared::{
    EngineCommand, InputContext, InputModeSwitch, KeyEvent, KeyState, Modifiers, SchemeResult,
    UiCommand,
};
use std::future::pending;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};
use tracing::info;
use zbus::zvariant::{Array, Dict, StructureBuilder, Type, Value};
use zbus::{Connection, Error, interface};

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
    ) -> Result<(), Error> {
        let conn = Connection::session().await?;

        let ibus_engine = IbusEngine {
            engine_tx: Mutex::new(engine_tx),
            platform_rx: Mutex::new(platform_rx),
            ui_tx: Mutex::new(ui_tx),
            context: Mutex::new(InputContext::default()),
            mode_switch: Mutex::new(InputModeSwitch::default()),
            last_code: Mutex::new(None),
            conn: conn.clone(),
        };

        conn.object_server()
            .at("/org/freedesktop/IBus/Engine/Black-Hole", ibus_engine)
            .await?;

        // 保持连接活跃
        pending::<()>().await;
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
    /// 中英文模式切换状态机（Ctrl 键触发）
    mode_switch: Mutex<InputModeSwitch>,
    /// 最近一次 composition 的编码（切英文模式时上屏保留）
    last_code: Mutex<Option<String>>,
    conn: Connection,
}

impl IbusEngine {
    /// 模式切换后的收尾：切英文模式时先上屏保留输入框内容，
    /// 然后重置引擎并向面板广播新的 InputMode 属性。
    async fn apply_mode_change(&self, english: bool) {
        // 切换到英文模式时，把输入框中的编码上屏，避免输入丢失。
        if english {
            let code = self.last_code.lock().unwrap().take();
            if let Some(code) = code.filter(|c| !c.is_empty()) {
                let ibus_text = (code.as_str(), Vec::<(u32, u32, u32, u32)>::new());
                let variant = Value::from(ibus_text);
                let _ = self
                    .conn
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/IBus/Engine/Black-Hole",
                        "org.freedesktop.IBus.Engine",
                        "CommitText",
                        &variant,
                    )
                    .await;
            }
        } else {
            *self.last_code.lock().unwrap() = None;
        }
        // 取消未完成的输入并重置引擎（幂等）。
        // daemon 处理 Reset 时会自行隐藏候选窗。
        let sent = {
            let engine_tx = self.engine_tx.lock().unwrap();
            engine_tx.send(EngineCommand::Reset).is_ok()
        };
        if sent {
            let platform_rx = self.platform_rx.lock().unwrap();
            let _ = platform_rx.recv();
        }
        // 更新 InputMode 属性，供面板/桌面 shell 感知中英文状态。
        let _ = self
            .conn
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/IBus/Engine/Black-Hole",
                "org.freedesktop.IBus.Engine",
                "UpdateProperty",
                &input_mode_property(english),
            )
            .await;
    }
}

#[interface(name = "org.freedesktop.IBus.Engine")]
impl IbusEngine {
    /// 处理按键事件
    /// 返回 true 表示按键被消费，false 表示转发给应用
    async fn process_key_event(&self, keyval: u32, _keycode: u32, state: u32) -> bool {
        const RELEASE_MASK: u32 = 1 << 30;
        const CONTROL_MASK: u32 = 1 << 2;
        // XK_Control_L / XK_Control_R
        const CONTROL_L: u32 = 0xFFE3;
        const CONTROL_R: u32 = 0xFFE4;

        // Ctrl 键：按下标记候选，松开切换中英文模式；不消费按键本身。
        if keyval == CONTROL_L || keyval == CONTROL_R {
            let toggled = {
                let mut mode = self.mode_switch.lock().unwrap();
                if state & RELEASE_MASK != 0 {
                    mode.ctrl_released()
                } else {
                    mode.ctrl_pressed();
                    None
                }
            };
            if let Some(english) = toggled {
                info!(
                    "Input mode toggled: {}",
                    if english { "英文" } else { "中文" }
                );
                self.apply_mode_change(english).await;
            }
            return false;
        }

        // 按住 Ctrl 期间按下其他键（如 Ctrl+C），取消切换候选。
        if state & CONTROL_MASK != 0 {
            self.mode_switch.lock().unwrap().other_key_pressed(true);
        }

        // 英文模式下不消费任何按键。
        if self.mode_switch.lock().unwrap().is_english() {
            return false;
        }

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
                // 已上屏，清空记录的编码，避免切换模式时重复上屏
                *self.last_code.lock().unwrap() = None;
                // 发送 CommitText DBus 信号
                let ibus_text = (text.as_str(), Vec::<(u32, u32, u32, u32)>::new());
                let variant = Value::from(ibus_text);

                let _ = self
                    .conn
                    .emit_signal(
                        None::<&str>,
                        "/org/freedesktop/IBus/Engine/Black-Hole",
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
                // 记录当前编码，供切换英文模式时上屏保留
                *self.last_code.lock().unwrap() = Some(code.clone());
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
        // 引擎已重置，清空记录的编码
        *self.last_code.lock().unwrap() = None;
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

    async fn enable(&self) {
        // 注册 InputMode 属性，使面板/桌面 shell 能显示并跟踪中英文状态。
        let english = self.mode_switch.lock().unwrap().is_english();
        let _ = self
            .conn
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/IBus/Engine/Black-Hole",
                "org.freedesktop.IBus.Engine",
                "RegisterProperties",
                &input_mode_prop_list(english),
            )
            .await;
    }
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
    async fn property_activate(&self, prop_name: String, prop_state: i32) {
        if prop_name != INPUT_MODE_PROP_KEY {
            return;
        }
        // 面板点击 InputMode 属性：CHECKED=中文，UNCHECKED=英文
        let toggled = self
            .mode_switch
            .lock()
            .unwrap()
            .set_english(prop_state == PROP_STATE_UNCHECKED as i32);
        if let Some(english) = toggled {
            info!(
                "Input mode toggled via property: {}",
                if english { "英文" } else { "中文" }
            );
            self.apply_mode_change(english).await;
        }
    }
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

// ---------------------------------------------------------------------------
// IBus 属性序列化（与 libibus 的 GVariant 格式逐字段一致）
//
// libibus 将 IBusSerializable 序列化为 tuple：首元素为类型名（如
// "IBusProperty"），随后是 attachments（a{sv}，恒为空），再按各子类
// 定义顺序追加字段。RegisterProperties / UpdateProperty 信号参数均为
// 包裹该 tuple 的 variant（见 ibus 源码 ibusserializable.c /
// ibusproperty.c / ibustext.c / ibusattrlist.c / ibusproplist.c /
// ibusengine.c）。
// ---------------------------------------------------------------------------

/// IBus 属性类型/状态常量（见 ibus 源码 ibustypes.h）
const PROP_TYPE_TOGGLE: u32 = 1;
const PROP_STATE_UNCHECKED: u32 = 0;
const PROP_STATE_CHECKED: u32 = 1;

/// 供面板/桌面 shell 感知中英文模式的属性 key
const INPUT_MODE_PROP_KEY: &str = "InputMode";

/// 空 attachments（a{sv}）
fn empty_attachments() -> Value<'static> {
    Value::Dict(Dict::new(<&str>::SIGNATURE, <Value>::SIGNATURE))
}

/// 序列化空的 IBusAttrList / IBusPropList：(s 类型名, a{sv}, av)
fn empty_named_container(class_name: &'static str) -> Value<'static> {
    Value::Structure(
        StructureBuilder::new()
            .add_field(class_name)
            .append_field(empty_attachments())
            .append_field(Value::Array(Array::new(<Value>::SIGNATURE)))
            .build()
            .expect("non-empty structure"),
    )
}

/// 序列化 IBusText：(s "IBusText", a{sv}, s text, v IBusAttrList)
fn ibus_text(text: &'static str) -> Value<'static> {
    Value::Structure(
        StructureBuilder::new()
            .add_field("IBusText")
            .append_field(empty_attachments())
            .add_field(text)
            .append_field(Value::Value(Box::new(empty_named_container(
                "IBusAttrList",
            ))))
            .build()
            .expect("non-empty structure"),
    )
}

/// 序列化 InputMode 属性并以 variant 包裹（UpdateProperty 信号参数）。
///
/// IBusProperty 字段顺序：key(s) type(u) label(v) icon(s) tooltip(v)
/// sensitive(b) visible(b) state(u) sub_props(v) symbol(v)。
/// 约定：中文=CHECKED（label "中"），英文=UNCHECKED（label "英"）。
fn input_mode_property(english: bool) -> Value<'static> {
    let (label, tooltip, state) = if english {
        ("英", "英文输入模式", PROP_STATE_UNCHECKED)
    } else {
        ("中", "中文输入模式", PROP_STATE_CHECKED)
    };
    let prop = StructureBuilder::new()
        .add_field("IBusProperty")
        .append_field(empty_attachments())
        .add_field(INPUT_MODE_PROP_KEY)
        .add_field(PROP_TYPE_TOGGLE)
        .append_field(Value::Value(Box::new(ibus_text(label))))
        .add_field("")
        .append_field(Value::Value(Box::new(ibus_text(tooltip))))
        .add_field(true)
        .add_field(true)
        .add_field(state)
        .append_field(Value::Value(Box::new(empty_named_container(
            "IBusPropList",
        ))))
        .append_field(Value::Value(Box::new(ibus_text(label))))
        .build()
        .expect("non-empty structure");
    Value::Value(Box::new(Value::Structure(prop)))
}

/// 序列化仅含 InputMode 属性的 IBusPropList 并以 variant 包裹
/// （RegisterProperties 信号参数）。列表元素同样是 variant 包裹的属性。
fn input_mode_prop_list(english: bool) -> Value<'static> {
    let mut props = Array::new(<Value>::SIGNATURE);
    props
        .append(input_mode_property(english))
        .expect("element signature matches");
    let list = StructureBuilder::new()
        .add_field("IBusPropList")
        .append_field(empty_attachments())
        .append_field(Value::Array(props))
        .build()
        .expect("non-empty structure");
    Value::Value(Box::new(Value::Structure(list)))
}
