//! 输入法进程间通信（IPC）协议
//!
//! 用于 Windows TSF DLL 与 daemon 之间的跨进程通信。
//! 基于 Named Pipe（`\\.\pipe\...`）+ 4 字节长度前缀 + MessagePack 帧序列化：
//! 无端口占用/防火墙问题，访问控制由 Windows 对象权限（仅限当前用户 SID 的
//! DACL）保证。协议内容与旧 TCP 版本一致。

use black_hole_shared::{
    Candidate, InputContext, KeyEvent, SchemeId, SchemeResult, Theme, UiCommand,
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::time::Duration;
use tracing::warn;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, GetTokenInformation, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

/// TSF DLL → Daemon 的请求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequest {
    /// 按键处理请求。`context` 携带该键按下时光标周围文本（整句补全上下文），
    /// 由 TSF 端与按键合并为单次请求发送，省去独立 SetContext 的一轮写+解析。
    KeyEvent {
        key: KeyEvent,
        #[serde(default)]
        context: Option<InputContext>,
    },
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

/// 单帧最大长度：64 KiB（候选列表整句补全场景下绰绰有余，同时防止
/// 长度字段被破坏时 read_exact 尝试分配超大缓冲）。
pub(crate) const MAX_FRAME_SIZE: u32 = 64 * 1024;

// ---------------------------------------------------------------------------
// 帧协议：4 字节小端长度前缀 + MessagePack 载荷
// ---------------------------------------------------------------------------

/// 编码一帧：4 字节小端长度前缀 + MessagePack 载荷拼入同一缓冲，
/// 写入方单次 write_all（rmp-serde 1.3 未导出 to_writer，故先 to_vec 再拼接）。
fn encode_frame<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, io::Error> {
    let payload =
        rmp_serde::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// 写入一帧：4 字节小端长度 + 载荷，单次 write_all。
fn write_frame<W: Write>(writer: &mut W, value: &impl serde::Serialize) -> Result<(), io::Error> {
    let frame = encode_frame(value)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// 读取一帧。`buf` 由调用方复用（热路径避免每键重新分配），
/// 返回的切片在下次调用前有效。
fn read_frame<'a, R: Read>(reader: &mut R, buf: &'a mut Vec<u8>) -> Result<&'a [u8], io::Error> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {} exceeds max {}", len, MAX_FRAME_SIZE),
        ));
    }
    buf.clear();
    buf.resize(len as usize, 0);
    reader.read_exact(buf)?;
    Ok(buf.as_slice())
}

/// IPC 通信辅助函数：发送请求（帧协议，MessagePack 载荷）
pub fn send_request<W: Write>(writer: &mut W, request: &IpcRequest) -> Result<(), io::Error> {
    write_frame(writer, request)
}

/// 编码一帧到调用方复用的缓冲（热路径零分配：clear + 复用容量），
/// 再单次 write_all。语义与 [`write_frame`] 一致。
fn write_frame_buf<W: Write>(
    writer: &mut W,
    value: &impl serde::Serialize,
    buf: &mut Vec<u8>,
) -> Result<(), io::Error> {
    buf.clear();
    // 预留 4 字节长度占位，直接序列化进同一缓冲后回填
    buf.extend_from_slice(&[0u8; 4]);
    value
        .serialize(&mut rmp_serde::Serializer::new(&mut *buf))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(buf.len() - 4)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    buf[..4].copy_from_slice(&len.to_le_bytes());
    writer.write_all(buf)?;
    writer.flush()?;
    Ok(())
}

/// 发送请求到复用缓冲版 [`send_request`]：编码走调用方提供的 `buf`，
/// 按键热路径不再每帧分配。`buf` 内容在调用间被覆盖复用。
pub fn send_request_buf<W: Write>(
    writer: &mut W,
    request: &IpcRequest,
    buf: &mut Vec<u8>,
) -> Result<(), io::Error> {
    write_frame_buf(writer, request, buf)
}

/// 发送响应的复用缓冲版（daemon 端每连接循环用）。
pub fn send_response_buf<W: Write>(
    writer: &mut W,
    response: &IpcResponse,
    buf: &mut Vec<u8>,
) -> Result<(), io::Error> {
    write_frame_buf(writer, response, buf)
}

/// IPC 通信辅助函数：发送响应（daemon 端用；与 send_request 同一帧格式）
pub fn send_response<W: Write>(writer: &mut W, response: &IpcResponse) -> Result<(), io::Error> {
    write_frame(writer, response)
}

/// IPC 通信辅助函数：读取请求（daemon 端用）。`buf` 复用避免每请求分配。
pub fn read_request<R: Read>(reader: &mut R, buf: &mut Vec<u8>) -> Result<IpcRequest, io::Error> {
    let payload = read_frame(reader, buf)?;
    // to_vec 产出的数据自带完整边界，&[u8] 切片解码即可
    let request: IpcRequest = rmp_serde::from_slice(payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(request)
}

/// IPC 通信辅助函数：读取响应（帧协议，MessagePack 载荷）。`buf` 复用避免每键分配。
pub fn read_response<R: Read>(reader: &mut R, buf: &mut Vec<u8>) -> Result<IpcResponse, io::Error> {
    let payload = read_frame(reader, buf)?;
    let response: IpcResponse = rmp_serde::from_slice(payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(response)
}

// ---------------------------------------------------------------------------
// Named Pipe 传输层（替代原 TCP localhost socket）
// ---------------------------------------------------------------------------

/// Daemon 端 Named Pipe 路径（无端口占用/防火墙问题，权限随默认 DACL）。
pub const IPC_PIPE_NAME: &str = r"\\.\pipe\black-hole-ime";

type SecAttrsTuple = (Box<SECURITY_ATTRIBUTES>, Box<SECURITY_DESCRIPTOR>, Vec<u8>);

/// 包裹含裸指针（*mut ACL）的缓存值，使其可放入 OnceLock：
/// 不变量——初始化后整体只读，内部指针仅由本进程的 CreateNamedPipeW
/// 内核调用读取，指向的分配随进程存活，故跨线程共享安全。
struct SecAttrsValue(SecAttrsTuple);
// SAFETY: 见类型注释；SECURITY_DESCRIPTOR 因内含 *mut ACL 本身 !Send/!Sync，
// 只读共享 + 指针进程生命周期内恒定有效，满足 Send/Sync 语义。
unsafe impl Send for SecAttrsValue {}
unsafe impl Sync for SecAttrsValue {}

struct SecAttrsCache(std::sync::OnceLock<Result<SecAttrsValue, String>>);

/// 构造仅授予当前用户访问权限的 SECURITY_ATTRIBUTES。
/// 默认 DACL 会允许同会话任意进程连接管道并注入按键/UI 命令，
/// 显式限定当前用户 SID 收紧攻击面。失败时退回默认安全属性（返回 None）。
///
/// 结果按进程缓存（OnceLock）：SA 内含指向 SD/ACL 的裸指针，缓存同时
/// 保证其地址在进程内恒定有效；且 accept() 每连接都会调用本函数，
/// 不缓存则每次泄漏一个 token 句柄和一份 ACL 分配。
fn restricted_security_attributes() -> io::Result<&'static SecAttrsTuple> {
    static SEC_ATTRS: SecAttrsCache = SecAttrsCache(std::sync::OnceLock::new());
    SEC_ATTRS
        .0
        .get_or_init(build_security_attributes)
        .as_ref()
        .map(|v| &v.0)
        .map_err(|e| io::Error::other(e.clone()))
}

fn build_security_attributes() -> Result<SecAttrsValue, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| e.to_string())?;
        // 进程令牌句柄随进程存活，无需关闭（daemon 生命周期内有效）。

        // 查询 TOKEN_USER 所需缓冲大小，再取实际数据
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut token_user_buf = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(token_user_buf.as_mut_ptr() as _),
            needed,
            &mut needed,
        )
        .map_err(|e| e.to_string())?;
        let token_user = &*(token_user_buf.as_ptr() as *const TOKEN_USER);
        let sid_ptr = token_user.User.Sid;

        // EXPLICIT_ACCESS：当前用户 SID，GENERIC_ALL
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: Default::default(),
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: windows::core::PWSTR(sid_ptr.0 as *mut u16),
        };
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            grfAccessMode: SET_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee,
        };

        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let err = SetEntriesInAclW(Some(&[ea]), None, &mut new_acl);
        if !err.is_ok() || new_acl.is_null() {
            return Err(format!("SetEntriesInAclW failed: {}", err.0));
        }

        // SECURITY_DESCRIPTOR：显式 DACL = 仅上述 ACE
        let mut sd = Box::new(SECURITY_DESCRIPTOR::default());
        InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(&mut *sd as *mut _ as _),
            // SECURITY_DESCRIPTOR_REVISION = 1（windows crate 未导出该常量）
            1u32,
        )
        .map_err(|e| e.to_string())?;
        SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(&mut *sd as *mut _ as _),
            true,
            Some(new_acl),
            false,
        )
        .map_err(|e| e.to_string())?;

        let sa = Box::new(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: &mut *sd as *mut _ as _,
            bInheritHandle: false.into(),
        });

        Ok(SecAttrsValue((sa, sd, token_user_buf)))
    }
}

/// 双工字节模式 Named Pipe 流。客户端与服务端共用，
/// [`Read`]/[`Write`] 委托给内部 [`File`]（管道句柄与文件句柄同构）。
pub struct IpcStream {
    file: File,
}

impl IpcStream {
    /// 客户端连接到指定管道。管道忙（实例被占满）时用 WaitNamedPipeW 等待重试；
    /// 管道尚未创建（daemon 正在创建首个实例的竞态窗口）时短暂重试。
    pub fn connect(pipe_name: &str) -> io::Result<Self> {
        let wide = to_wide(pipe_name);
        let access = (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0;
        // 竞态窗口（daemon accept 后立刻重建实例）通常几次轻量重试即可跨过；
        // 极端情况（daemon 冷启动慢，如杀毒扫描 DLL）切换 10ms 长间隔，
        // 总等待窗口约 200ms，仍远小于一次按键超时。
        let mut not_found_retries: u32 = 0;
        const FAST_RETRIES: u32 = 10;
        const SLOW_RETRIES: u32 = 10;
        loop {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    access,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            };
            match handle {
                Ok(h) => {
                    return Ok(Self {
                        file: unsafe { File::from_raw_handle(h.0 as _) },
                    });
                }
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    // 管道实例占满：等待 daemon 释放一个实例后重试
                    let ok = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), 1000u32) }.as_bool();
                    if !ok {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "WaitNamedPipe timed out",
                        ));
                    }
                }
                Err(e)
                    if e.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                        && not_found_retries < FAST_RETRIES + SLOW_RETRIES =>
                {
                    not_found_retries += 1;
                    let ms = if not_found_retries <= FAST_RETRIES {
                        // 快速阶段：2ms×n 退避（合计约 110ms）
                        2u64.saturating_mul(not_found_retries as u64)
                    } else {
                        // 长间隔阶段：10ms 固定（再补约 100ms）
                        10
                    };
                    std::thread::sleep(Duration::from_millis(ms));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// 复制底层句柄（服务端用于拆分 reader/writer，与原 TcpStream::try_clone 等价）。
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }
}

impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Daemon 端管道监听器。每次 [`IpcListener::accept`] 创建一个双工管道实例并
/// 阻塞等待客户端连接（实例数不限，等价原 TCP per-connection 线程模型）。
pub struct IpcListener {
    pipe_name: Vec<u16>,
    /// bind() 创建的首个实例：既做名称冲突探测（fail-fast），也在
    /// 第一次 accept 时复用，避免"创建→关闭→重建"窗口吞掉客户端连接。
    /// Cell：accept(&self) 内取走一次性所有权。
    probe: std::cell::Cell<Option<HANDLE>>,
}

// HANDLE 是裸句柄（非线程亲和资源，内核对象可跨线程使用），probe 仅在
// accept 内被取走一次；语义与原 TcpListener（自动 Send）一致。
unsafe impl Send for IpcListener {}

impl Drop for IpcListener {
    fn drop(&mut self) {
        // probe 尚未被 accept() 取走时，首个管道实例句柄需手动关闭，
        // 否则 \\.\pipe\black-hole-ime 在进程存活期间一直被占用。
        if let Some(handle) = self.probe.take().filter(|h| !h.is_invalid()) {
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

impl IpcListener {
    /// 绑定指定管道名。以 FILE_FLAG_FIRST_PIPE_INSTANCE 创建首个实例：
    /// 若同名管道已存在（如另一个 daemon 在运行），CreateNamedPipeW 返回
    /// ERROR_ACCESS_DENIED，启动期即报错（等价旧 TcpListener::bind 的 fail-fast）。
    /// 该实例保留在 listener 内，由第一次 accept() 直接服务，而非关闭——
    /// 关闭会造成短暂窗口：客户端连上一个随即销毁的实例，或遭遇 ERROR_FILE_NOT_FOUND。
    pub fn bind(pipe_name: &str) -> io::Result<Self> {
        let listener = Self {
            pipe_name: to_wide(pipe_name),
            probe: std::cell::Cell::new(None),
        };
        let probe = listener.create_instance(true)?;
        listener.probe.set(Some(probe));
        Ok(listener)
    }

    /// 创建一个双工字节模式管道实例。
    /// 使用仅限当前用户的 DACL（见 restricted_security_attributes），
    /// 失败时退回默认安全属性并记录日志（安全收紧失败不应阻断输入法启动）。
    ///
    /// `first_instance`：传 true 时带 FILE_FLAG_FIRST_PIPE_INSTANCE，
    /// 同名管道已存在即失败（bind 探测用）；普通实例传 false。
    fn create_instance(&self, first_instance: bool) -> io::Result<HANDLE> {
        let name = PCWSTR(self.pipe_name.as_ptr());
        let first_flag = if first_instance {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(0)
        };
        let open_mode = PIPE_ACCESS_DUPLEX | first_flag;
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT;
        // SA 按进程缓存于 OnceLock（地址恒定有效），此处仅需借用其指针。
        let sa_ptr: *mut SECURITY_ATTRIBUTES = match restricted_security_attributes() {
            Ok(sa) => sa.0.as_ref() as *const SECURITY_ATTRIBUTES as *mut SECURITY_ATTRIBUTES,
            Err(e) => {
                warn!(
                    "restricted_security_attributes failed, using default DACL: {}",
                    e
                );
                std::ptr::null_mut()
            }
        };
        let handle: HANDLE = unsafe {
            CreateNamedPipeW(
                name,
                open_mode,
                pipe_mode,
                PIPE_UNLIMITED_INSTANCES,
                // 64 KiB：整句补全的候选列表帧较大，大缓冲避免写入方阻塞
                64 * 1024,
                64 * 1024,
                0,
                Some(sa_ptr as *const SECURITY_ATTRIBUTES),
            )
        };
        if handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }

    /// 创建一个管道实例并阻塞等待客户端连接。PIPE_UNLIMITED_INSTANCES
    /// 允许多个客户端（多个宿主进程中的 TSF DLL）并发连接。
    pub fn accept(&self) -> io::Result<IpcStream> {
        // 优先复用 bind() 保留的首个实例（Handle 为裸句柄，Drop 不会自动关闭）
        let handle = match self.probe.take() {
            Some(h) => h,
            None => self.create_instance(false)?,
        };

        // 阻塞等待客户端连接（默认同步管道即等待语义）。
        // 失败路径必须关闭句柄，否则管道实例及其 64 KiB 缓冲泄漏。
        let stream = IpcStream {
            file: unsafe { File::from_raw_handle(handle.0 as _) },
        };
        if let Err(e) = unsafe { ConnectNamedPipe(handle, None) } {
            // ERROR_PIPE_CONNECTED：客户端在 ConnectNamedPipe 调用前已连上，属正常
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, e));
            }
        }

        Ok(stream)
    }
}

/// UTF-8 → NUL 结尾 UTF-16
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use black_hole_shared::{KeyState, Modifiers};

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
            // 经 MessagePack 序列化/反序列化（与 read_response 同一通道）后仍保留
            let payload = rmp_serde::to_vec(&response).unwrap();
            let back: IpcResponse = rmp_serde::from_slice(&payload).unwrap();
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

    /// 旧版本 daemon 的 Committed MessagePack 不含 temporary_english 字段时，
    /// #[serde(default)] 应兜底为 false 而非反序列化失败。
    /// （用 serde_json 构造再转 MessagePack 不可行，直接手写最小 msgp 结构：
    /// fixmap{1}，key="Committed"（fixstr len 9），value=fixmap{1}，key="text"
    /// （fixstr len 4），value="abc"（fixstr len 3）。）
    #[test]
    fn committed_missing_temporary_english_defaults_to_false() {
        let legacy: Vec<u8> = vec![
            0x81, 0xA9, b'C', b'o', b'm', b'm', b'i', b't', b't', b'e',
            b'd', // fixmap{1}, "Committed"
            0x81, 0xA4, b't', b'e', b'x', b't', 0xA3, b'a', b'b',
            b'c', // fixmap{1}, "text", "abc"
        ];
        let response: IpcResponse = rmp_serde::from_slice(&legacy).unwrap();
        assert_eq!(
            response,
            IpcResponse::Committed {
                text: "abc".to_string(),
                temporary_english: false,
            }
        );
    }

    /// 帧协议往返：IpcRequest 经 send/read 同路径编解码后保持一致。
    #[test]
    fn frame_round_trip_request_and_response() {
        let request = IpcRequest::KeyEvent {
            key: KeyEvent {
                key: "a".to_string(),
                modifiers: Modifiers {
                    shift: false,
                    ctrl: false,
                    alt: false,
                    meta: false,
                    capslock: false,
                },
                state: KeyState::Press,
            },
            context: None,
        };
        let mut wire = Vec::new();
        send_request(&mut wire, &request).unwrap();
        let mut buf = Vec::new();
        let decoded = read_request(&mut wire.as_slice(), &mut buf).unwrap();
        assert_eq!(request, decoded);

        let response = IpcResponse::Composing {
            code: "ni".to_string(),
            candidates: vec![
                Candidate {
                    text: "你".to_string(),
                    comment: None,
                    score: 100,
                },
                Candidate {
                    text: "呢".to_string(),
                    comment: None,
                    score: 90,
                },
            ],
            selected_index: 0,
            expanded: false,
        };
        let mut wire = Vec::new();
        send_response(&mut wire, &response).unwrap();
        let mut buf = Vec::new();
        let decoded = read_response(&mut wire.as_slice(), &mut buf).unwrap();
        assert_eq!(response, decoded);
    }
}
