use crate::args::DaemonArgs;
use blackhole_engine::{Engine, EngineBuilder};
use blackhole_platform::PlatformIme;
use blackhole_shared::{EngineCommand, InputContext, SchemeId, SchemeResult, Theme, UiCommand};
use blackhole_ui::SettingsManager;
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// 全局退出标志，由信号处理器设置
static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

/// 应用生命周期管理器
///
/// 负责初始化、线程编排、信号处理和优雅退出。
pub struct App;

impl App {
    /// 启动 daemon，阻塞直到平台服务结束或收到退出信号
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let _tracing_guard = Self::init_tracing();
        tracing::info!("Blackhole IME daemon starting...");

        let args = DaemonArgs::parse();

        #[cfg(target_os = "windows")]
        Self::ensure_ime_registered();

        #[cfg(target_os = "linux")]
        if !args.ibus {
            Self::ensure_ime_registered();
        }
        let settings_mgr = SettingsManager::new();
        let settings_scheme = settings_mgr.settings().default_scheme;
        let default_scheme = args.scheme.unwrap_or(settings_scheme);

        tracing::info!(
            "resolved scheme: CLI={:?}, settings={:?}, effective={:?}",
            args.scheme,
            settings_scheme,
            default_scheme,
        );

        let dict_path = args.dict_path.or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()
                .map(|p| p.join("dicts").join("rime_ice.dict.yaml"))
                .filter(|p| p.exists())
                .map(|p| p.to_string_lossy().to_string())
        });

        let engine = Arc::new(Mutex::new(
            EngineBuilder::new()
                .scheme(default_scheme)
                .dictionary_opt(dict_path)
                .build(),
        ));

        let (ui_tx, ui_rx) = mpsc::channel::<UiCommand>();
        let (engine_tx, engine_rx) = mpsc::channel::<EngineCommand>();
        let (platform_tx, platform_rx) = mpsc::channel::<SchemeResult>();

        Self::setup_signal_handler();

        let default_theme = settings_mgr.settings().theme;

        // 运行时方案/主题状态，平台线程通过 IPC GetSettings 读取，dispatch 时同步更新
        let current_settings: Arc<Mutex<(SchemeId, Theme)>> =
            Arc::new(Mutex::new((default_scheme, default_theme)));

        // UI 渲染线程（只处理候选窗相关命令）
        let (ui_render_tx, ui_render_rx) = mpsc::channel::<UiCommand>();
        let ui_handle = std::thread::spawn(move || {
            blackhole_ui::run_candidate_window(ui_render_rx, default_theme);
        });

        // 引擎线程（带 panic 恢复）
        let engine_clone = Arc::clone(&engine);
        let ui_tx_clone = ui_tx.clone();
        let engine_handle = std::thread::spawn(move || {
            Self::run_engine_thread(engine_clone, engine_rx, platform_tx, ui_tx_clone);
        });

        // 为 UI 命令分发和平台线程预先 clone 通道 / 共享状态
        let engine_tx_for_ui_dispatch = engine_tx.clone();
        let ui_render_tx_for_shutdown = ui_render_tx.clone();
        let ui_tx_for_platform = ui_tx.clone();
        let current_for_dispatch = Arc::clone(&current_settings);

        // 平台线程（后台运行，避免阻塞主线程）
        let _platform_handle = std::thread::spawn(move || {
            if let Err(e) =
                run_platform(engine_tx, platform_rx, ui_tx_for_platform, current_settings)
            {
                tracing::error!("Platform IME error: {}", e);
            }
        });

        // 主线程：分发 UI 命令并等待退出信号（Ctrl+C 或语言栏 Exit）
        while !SHOULD_EXIT.load(Ordering::Relaxed) {
            while let Ok(cmd) = ui_rx.try_recv() {
                Self::dispatch_ui_command(
                    cmd,
                    &engine_tx_for_ui_dispatch,
                    &ui_render_tx,
                    &current_for_dispatch,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Self::graceful_shutdown(ui_render_tx_for_shutdown, ui_handle, engine_handle);

        tracing::info!("Blackhole IME daemon exited.");
        Ok(())
    }

    // ------------------------------------------------------------------
    // 初始化
    // ------------------------------------------------------------------

    /// 初始化 tracing 日志系统
    ///
    /// 返回的 guard 必须被持有，否则非阻塞文件 appender 会被提前刷新关闭。
    fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
        let log_dir = std::env::temp_dir();
        let file_appender = tracing_appender::rolling::never(log_dir, "blackhole-daemon.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false),
            )
            .init();

        guard
    }

    // ------------------------------------------------------------------
    // 输入法自动注册（Windows 专用）
    // ------------------------------------------------------------------

    #[cfg(target_os = "windows")]
    fn ensure_ime_registered() {
        if blackhole_platform::windows_tsf::auto_register::is_registered() {
            tracing::info!("IME already registered");
            return;
        }

        let dll_path = match std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("blackhole_platform.dll")))
        {
            Some(p) => p,
            None => {
                tracing::warn!("Could not determine DLL path for auto-registration");
                return;
            }
        };

        if !dll_path.exists() {
            tracing::warn!("Platform DLL not found at: {}", dll_path.display());
            return;
        }

        tracing::info!("IME not registered, attempting auto-registration...");
        match blackhole_platform::windows_tsf::auto_register::register_ime(&dll_path) {
            Ok(()) => tracing::info!("Auto-registration succeeded"),
            Err(e) => tracing::warn!("Auto-registration failed: {}", e),
        }
    }

    // ------------------------------------------------------------------
    // 输入法自动注册（Linux 专用）
    // ------------------------------------------------------------------

    #[cfg(target_os = "linux")]
    fn ensure_ime_registered() {
        if blackhole_platform::linux_ibus::auto_register::is_registered() {
            tracing::info!("IBus component already registered");
            return;
        }

        tracing::info!("IBus component not registered, attempting auto-registration...");
        match blackhole_platform::linux_ibus::auto_register::register_ime() {
            Ok(()) => tracing::info!("Auto-registration succeeded"),
            Err(e) => tracing::warn!("Auto-registration failed: {}", e),
        }
    }

    // ------------------------------------------------------------------
    // 信号处理
    // ------------------------------------------------------------------

    fn setup_signal_handler() {
        let result = ctrlc::set_handler(|| {
            tracing::info!("Received interrupt signal, requesting shutdown...");
            SHOULD_EXIT.store(true, Ordering::SeqCst);
        });
        if let Err(e) = result {
            tracing::warn!("Failed to register Ctrl+C handler: {}", e);
        }
    }

    // ------------------------------------------------------------------
    // 引擎线程
    // ------------------------------------------------------------------

    fn run_engine_thread(
        engine: Arc<Mutex<Engine>>,
        engine_rx: mpsc::Receiver<EngineCommand>,
        platform_tx: mpsc::Sender<SchemeResult>,
        ui_tx: mpsc::Sender<UiCommand>,
    ) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ctx = InputContext::default();
            while let Ok(cmd) = engine_rx.recv() {
                if SHOULD_EXIT.load(Ordering::Relaxed) {
                    tracing::info!("Engine thread received exit request");
                    break;
                }

                tracing::debug!(
                    "engine_thread start: cmd={:?}",
                    std::mem::discriminant(&cmd)
                );

                let result = Self::process_engine_command(&engine, &mut ctx, cmd, &ui_tx);

                if let Some(r) = result
                    && platform_tx.send(r).is_err()
                {
                    tracing::warn!("Platform receiver dropped, exiting engine thread");
                    break;
                }

                tracing::debug!("engine_thread end");
            }
        }));

        if let Err(e) = result {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            tracing::error!(msg, "Engine thread panicked");
            // 尽量通知 UI 隐藏候选窗，避免界面残留
            let _ = ui_tx.send(UiCommand::HideCandidates);
        }
    }

    /// 分发来自平台层/UI 层的命令到对应处理者
    fn dispatch_ui_command(
        cmd: UiCommand,
        engine_tx: &mpsc::Sender<EngineCommand>,
        ui_render_tx: &mpsc::Sender<UiCommand>,
        current_settings: &Arc<Mutex<(SchemeId, Theme)>>,
    ) {
        match cmd {
            UiCommand::ShowSettings => {
                tracing::info!("UI dispatch: open settings");
                let settings_mgr = SettingsManager::new();
                std::thread::spawn(move || {
                    blackhole_ui::run_settings_panel(settings_mgr);
                });
            }
            UiCommand::SwitchScheme(scheme_id) => {
                tracing::info!("UI dispatch: switch to {:?}", scheme_id);
                let _ = engine_tx.send(EngineCommand::SwitchScheme(scheme_id));
                // 持久化到设置，使重启后保持本次选择
                let mut settings_mgr = SettingsManager::new();
                settings_mgr.settings_mut().default_scheme = scheme_id;
                settings_mgr.save();
                // 同步共享状态，使平台线程（IPC GetSettings）返回最新值
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.0 = scheme_id;
                }
            }
            UiCommand::SetTheme(theme) => {
                tracing::info!("UI dispatch: set theme to {:?}", theme);
                let mut settings_mgr = SettingsManager::new();
                settings_mgr.settings_mut().theme = theme;
                settings_mgr.save();
                // 同步共享状态
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.1 = theme;
                }
                let _ = ui_render_tx.send(UiCommand::SetTheme(theme));
            }
            UiCommand::Exit => {
                tracing::info!("UI dispatch: exit requested");
                SHOULD_EXIT.store(true, Ordering::SeqCst);
            }
            other => {
                // 候选窗等命令转发给 UI 渲染线程
                let _ = ui_render_tx.send(other);
            }
        }
    }

    /// 处理单个引擎命令，返回需要发送给平台层的结果
    fn process_engine_command(
        engine: &Arc<Mutex<Engine>>,
        ctx: &mut InputContext,
        cmd: EngineCommand,
        ui_tx: &mpsc::Sender<UiCommand>,
    ) -> Option<SchemeResult> {
        match cmd {
            EngineCommand::SetContext(new_ctx) => {
                *ctx = new_ctx;
                None
            }
            EngineCommand::Key(key) => {
                let mut engine = engine.lock().unwrap();
                let result = engine.process(&EngineCommand::Key(key), ctx);
                if let SchemeResult::Committed { ref text } = result {
                    let _ = ui_tx.send(UiCommand::CommitText(text.clone()));
                }
                Some(result)
            }
            EngineCommand::Reset => {
                let mut engine = engine.lock().unwrap();
                let result = engine.process(&EngineCommand::Reset, ctx);
                let _ = ui_tx.send(UiCommand::HideCandidates);
                Some(result)
            }
            EngineCommand::SelectCandidate(idx) => {
                let mut engine = engine.lock().unwrap();
                let result = engine.process(&EngineCommand::SelectCandidate(idx), ctx);
                if let SchemeResult::Committed { ref text } = result {
                    let _ = ui_tx.send(UiCommand::CommitText(text.clone()));
                }
                Some(result)
            }
            EngineCommand::SwitchScheme(id) => {
                let mut engine = engine.lock().unwrap();
                engine.process(&EngineCommand::SwitchScheme(id), ctx);
                // SwitchScheme 来自托盘而非 IPC 客户端，没有人等待 platform_rx 上的响应，
                // 发送 Ignored 会污染共享响应通道，导致后续按键请求/响应错位。
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // 优雅退出
    // ------------------------------------------------------------------

    fn graceful_shutdown(
        ui_tx: mpsc::Sender<UiCommand>,
        ui_handle: std::thread::JoinHandle<()>,
        engine_handle: std::thread::JoinHandle<()>,
    ) {
        tracing::info!("Shutting down daemon...");

        // 通知 UI 退出
        if ui_tx.send(UiCommand::Exit).is_err() {
            tracing::warn!("UI channel closed, UI thread may have already exited");
        }

        // 等待引擎线程结束（5 秒超时）
        if !Self::join_with_timeout(engine_handle, "Engine", std::time::Duration::from_secs(5)) {
            tracing::warn!("Engine thread did not exit within timeout");
        }

        // 等待 UI 线程结束（5 秒超时）
        if !Self::join_with_timeout(ui_handle, "UI", std::time::Duration::from_secs(5)) {
            tracing::warn!("UI thread did not exit within timeout");
        }
    }

    /// 等待线程结束，带超时
    fn join_with_timeout(
        handle: std::thread::JoinHandle<()>,
        name: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = handle.join();
            let _ = tx.send(result);
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(())) => true,
            Ok(Err(_)) => {
                tracing::warn!("{} thread panicked during shutdown", name);
                true // 虽然 panic 了，但至少 join 返回了
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => false,
        }
    }
}

// ------------------------------------------------------------------
// 平台适配
// ------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn run_platform(
    engine_tx: mpsc::Sender<EngineCommand>,
    platform_rx: mpsc::Receiver<SchemeResult>,
    ui_tx: mpsc::Sender<UiCommand>,
    current_settings: Arc<Mutex<(SchemeId, Theme)>>,
) -> Result<(), blackhole_platform::PlatformError> {
    let mut platform = blackhole_platform::WindowsTsfIme::new(current_settings);
    platform.run(engine_tx, platform_rx, ui_tx)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_platform(
    engine_tx: mpsc::Sender<EngineCommand>,
    platform_rx: mpsc::Receiver<SchemeResult>,
    ui_tx: mpsc::Sender<UiCommand>,
    _current_settings: Arc<Mutex<(SchemeId, Theme)>>,
) -> Result<(), blackhole_platform::PlatformError> {
    let mut platform = blackhole_platform::LinuxIbusIme::new();
    platform.run(engine_tx, platform_rx, ui_tx)?;
    Ok(())
}
