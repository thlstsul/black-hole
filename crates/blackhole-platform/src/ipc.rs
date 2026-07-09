//! 输入法进程间通信（IPC）协议
//!
//! 用于 Windows TSF DLL 与 daemon 之间的跨进程通信。
//! 基于 TCP localhost socket + JSON 序列化，轻量且无需额外依赖。

use blackhole_shared::{InputContext, KeyEvent, SchemeId, SchemeResult, Theme, UiCommand};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// TSF DLL → Daemon 的请求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequest {
    KeyEvent(KeyEvent),
    SetContext(InputContext),
    Reset,
    /// TSF DLL 通知 daemon 执行 UI 命令（候选窗、设置、主题、退出等）。
    UiCommand(UiCommand),
    /// TSF DLL 连接后向 daemon 查询当前设置（scheme、theme），
    /// 以便托盘菜单勾选正确的选项。
    GetSettings,
}

/// Daemon → TSF DLL 的响应
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    Composing {
        code: String,
        candidates: Vec<blackhole_shared::Candidate>,
        selected_index: usize,
        expanded: bool,
    },
    Committed {
        text: String,
    },
    Ignored,
    /// 响应 GetSettings 请求，返回 daemon 当前加载的设置。
    Settings {
        scheme_id: SchemeId,
        theme: Theme,
    },
}

impl From<SchemeResult> for IpcResponse {
    fn from(result: SchemeResult) -> Self {
        match result {
            SchemeResult::Composing {
                code,
                candidates,
                selected_index,
                expanded,
            } => IpcResponse::Composing {
                code,
                candidates,
                selected_index,
                expanded,
            },
            SchemeResult::Committed { text } => IpcResponse::Committed { text },
            SchemeResult::Ignored => IpcResponse::Ignored,
        }
    }
}

impl From<IpcResponse> for SchemeResult {
    fn from(response: IpcResponse) -> Self {
        match response {
            IpcResponse::Composing {
                code,
                candidates,
                selected_index,
                expanded,
            } => SchemeResult::Composing {
                code,
                candidates,
                selected_index,
                expanded,
            },
            IpcResponse::Committed { text } => SchemeResult::Committed { text },
            IpcResponse::Ignored => SchemeResult::Ignored,
            // Settings is only handled directly in sync_settings_from_daemon,
            // never converted to SchemeResult.
            IpcResponse::Settings { .. } => {
                unreachable!("IpcResponse::Settings should not be converted to SchemeResult")
            }
        }
    }
}

/// IPC 通信辅助函数：发送请求到 stream
pub fn send_request<W: Write>(writer: &mut W, request: &IpcRequest) -> Result<(), std::io::Error> {
    let json = serde_json::to_string(request)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writeln!(writer, "{}", json)?;
    writer.flush()?;
    Ok(())
}

/// IPC 通信辅助函数：从 stream 读取响应
pub fn read_response<R: BufRead>(reader: &mut R) -> Result<IpcResponse, std::io::Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: IpcResponse = serde_json::from_str(line.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(response)
}

/// Daemon 端 IPC 服务器地址
pub const IPC_SERVER_ADDR: &str = "127.0.0.1:52719";
