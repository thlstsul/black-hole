use black_hole_shared::{EngineCommand, SchemeResult, UiCommand};
use std::sync::mpsc::{Receiver, Sender};

/// 平台输入法抽象 trait
///
/// 平台层负责将系统原生输入事件转换为 `EngineCommand` 发送给引擎线程，
/// 并从 `platform_rx` 接收引擎处理结果以执行平台特定的文本操作
/// （如 IBus CommitText、TSF composition 更新等）。
/// UI 相关的结果（候选窗口显示/隐藏）由引擎线程直接通过 `ui_tx` 发送给 UI 线程，
/// 平台层不直接参与 UI 渲染。
pub trait PlatformIme: Send {
    /// 启动平台输入法服务，阻塞直到服务结束。
    fn run(
        &mut self,
        engine_tx: Sender<EngineCommand>,
        platform_rx: Receiver<SchemeResult>,
        ui_tx: Sender<UiCommand>,
    ) -> Result<(), PlatformError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("platform not supported")]
    Unsupported,
    #[error("COM initialization failed: {0}")]
    ComInit(String),
    #[error("DBus error: {0}")]
    Dbus(String),
    #[error("unknown error: {0}")]
    Other(String),
}

#[cfg(target_os = "windows")]
pub mod ipc;

#[cfg(target_os = "windows")]
pub mod windows_tsf;

#[cfg(target_os = "windows")]
pub use windows_tsf::WindowsTsfIme;

#[cfg(target_os = "linux")]
pub mod linux_ibus;

#[cfg(target_os = "linux")]
pub use linux_ibus::LinuxIbusIme;
