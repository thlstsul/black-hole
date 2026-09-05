//! 输入法进程间通信（IPC）协议
//!
//! 用于 Windows TSF DLL 与 daemon 之间的跨进程通信。
//! 基于 TCP localhost socket + JSON 序列化，轻量且无需额外依赖。

use black_hole_shared::{
    Candidate, InputContext, KeyEvent, SchemeId, SchemeResult, Theme, UiCommand,
};
use serde::{Deserialize, Serialize};
use serde_json::{from_str, to_string};
use std::io::{self, BufRead, Write};

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
        candidates: Vec<Candidate>,
        selected_index: usize,
        expanded: bool,
    },
    Committed {
        text: String,
        /// 本次上屏是否为"临时英文模式"结束上屏（见 SchemeResult::Committed）。
        /// 加 #[serde(default)] 做跨版本兼容：旧版本 daemon 序列化的 Committed
        /// JSON 不含此字段时默认 false（非临时英文），与同枚举 Settings.auto_switch
        /// 的约定一致，避免版本错配时 read_response 反序列化失败丢键。
        #[serde(default)]
        temporary_english: bool,
    },
    Ignored,
    /// 用户取消输入（Esc / cancel 绑定），见 SchemeResult::Cancelled。
    Cancelled,
    /// 响应 GetSettings 请求，返回 daemon 当前加载的设置。
    Settings {
        scheme_id: SchemeId,
        theme: Theme,
        /// 全局中英文输入模式：true=英文，false=中文
        english: bool,
        /// 是否启用"根据光标周围文本自动切换中英模式"
        #[serde(default)]
        auto_switch: bool,
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
            SchemeResult::Committed {
                text,
                temporary_english,
            } => IpcResponse::Committed {
                text,
                temporary_english,
            },
            SchemeResult::Cancelled => IpcResponse::Cancelled,
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
            IpcResponse::Committed {
                text,
                temporary_english,
            } => SchemeResult::Committed {
                text,
                temporary_english,
            },
            IpcResponse::Cancelled => SchemeResult::Cancelled,
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
pub fn send_request<W: Write>(writer: &mut W, request: &IpcRequest) -> Result<(), io::Error> {
    let json = to_string(request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(writer, "{}", json)?;
    writer.flush()?;
    Ok(())
}

/// IPC 通信辅助函数：从 stream 读取响应
pub fn read_response<R: BufRead>(reader: &mut R) -> Result<IpcResponse, io::Error> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: IpcResponse =
        from_str(line.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(response)
}

/// Daemon 端 IPC 服务器地址
pub const IPC_SERVER_ADDR: &str = "127.0.0.1:52719";

#[cfg(test)]
mod tests {
    use super::*;

    /// Committed 经 IPC 往返须保留 temporary_english（跨 daemon↔TSF 的关键
    /// 字段，静默丢失会使临时英文上屏后的自动切换锁定失效）。
    #[test]
    fn committed_round_trip_preserves_temporary_english() {
        for temp in [false, true] {
            let result = SchemeResult::Committed {
                text: "hello ".to_string(),
                temporary_english: temp,
            };
            let response = IpcResponse::from(result);
            assert_eq!(
                IpcResponse::Committed {
                    text: "hello ".to_string(),
                    temporary_english: temp,
                },
                response
            );
            // 经 JSON 序列化/反序列化（与 read_response 同一通道）后仍保留
            let json = to_string(&response).unwrap();
            let back: IpcResponse = from_str(&json).unwrap();
            let restored = SchemeResult::from(back);
            assert_eq!(
                restored,
                SchemeResult::Committed {
                    text: "hello ".to_string(),
                    temporary_english: temp,
                }
            );
        }
    }

    /// 旧版本 daemon 的 Committed JSON 不含 temporary_english 字段时，
    /// #[serde(default)] 应兜底为 false 而非反序列化失败。
    #[test]
    fn committed_missing_temporary_english_defaults_to_false() {
        let legacy = r#"{"Committed":{"text":"abc"}}"#;
        let response: IpcResponse = from_str(legacy).unwrap();
        assert_eq!(
            response,
            IpcResponse::Committed {
                text: "abc".to_string(),
                temporary_english: false,
            }
        );
    }
}
