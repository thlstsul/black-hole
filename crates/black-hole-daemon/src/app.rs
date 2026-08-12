use crate::args::DaemonArgs;
use crate::completion::{self, CompletionRequest};
use black_hole_engine::{Engine, EngineBuilder};
use black_hole_platform::PlatformIme;
use black_hole_platform::auto_start::set_auto_start;
#[cfg(target_os = "linux")]
use black_hole_platform::linux_ibus::auto_register::{
    is_registered as is_registered_linux, register_ime as register_ime_linux,
};
#[cfg(target_os = "windows")]
use black_hole_platform::windows_tsf::auto_register::{is_registered, register_ime};
#[cfg(target_os = "linux")]
use black_hole_platform::{LinuxIbusIme, PlatformError as LinuxPlatformError};
#[cfg(target_os = "windows")]
use black_hole_platform::{PlatformError as WindowsPlatformError, WindowsTsfIme};
use black_hole_shared::{
    EngineCommand, InputContext, LlmCompletionSettings, SchemeId, SchemeResult, Settings, Theme,
    UiCommand,
};
use black_hole_ui::{SettingsManager, run_candidate_window, run_settings_panel};
use clap::Parser;
use notify::Watcher;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::mem::discriminant;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use tracing_appender::non_blocking;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;
use tracing_subscriber::util::SubscriberInitExt;

/// 应用生命周期管理器
///
/// 负责初始化、线程编排、信号处理和优雅退出。
pub struct App;

impl App {
    /// 启动 daemon，阻塞直到平台服务结束或收到退出信号
    pub fn run() -> Result<(), Box<dyn Error>> {
        let args = DaemonArgs::parse();

        // 设置面板模式：由守护进程以独立进程方式唤起。
        // winit 0.30 每个进程只允许创建一个事件循环，候选窗已占用守护进程
        // 进程内的那个，因此设置面板必须在独立进程中运行。
        if args.settings_panel {
            let settings_mgr = SettingsManager::new();
            run_settings_panel(settings_mgr);
            return Ok(());
        }

        let _tracing_guard = Self::init_tracing();
        info!("Black-Hole IME daemon starting...");

        #[cfg(target_os = "windows")]
        Self::ensure_ime_registered();

        #[cfg(target_os = "linux")]
        if !args.ibus {
            Self::ensure_ime_registered();
        }
        let settings_mgr = SettingsManager::new();
        let settings_scheme = settings_mgr.settings().default_scheme;
        let default_scheme = args.scheme.unwrap_or(settings_scheme);

        // 按设置同步开机自启动状态（如安装目录变化后重新写入 Run 键 / autostart 文件）
        let auto_start = settings_mgr.settings().auto_start;
        if let Err(e) = set_auto_start(auto_start) {
            warn!("Failed to sync auto start ({}): {}", auto_start, e);
        }

        info!(
            "resolved scheme: CLI={:?}, settings={:?}, effective={:?}",
            args.scheme, settings_scheme, default_scheme,
        );

        let dict_path = args.dict_path.or_else(|| {
            env::current_exe()
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
                .key_bindings(settings_mgr.settings().key_bindings.clone())
                .build(),
        ));

        let (ui_tx, ui_rx) = mpsc::channel::<UiCommand>();
        let (engine_tx, engine_rx) = mpsc::channel::<EngineCommand>();
        let (platform_tx, platform_rx) = mpsc::channel::<SchemeResult>();

        // LLM 整句补全：引擎线程投递请求 → 独立 worker 异步调用 LLM，
        // 结果经 EngineCommand::UpdateCompletion / UiCommand::Completion 双通道回传。
        let (completion_tx, completion_rx) = mpsc::channel::<CompletionRequest>();
        let completion_config: Arc<Mutex<LlmCompletionSettings>> =
            Arc::new(Mutex::new(settings_mgr.settings().llm_completion.clone()));
        // 补全请求代际号（daemon 与 worker 共享）：worker 发起请求时递增以作废
        // 在途旧请求；引擎侧收到 Committed（选中/上屏）时也递增，终止所有
        // 未返回的补全请求，避免选中后旧结果再覆盖。
        let completion_generation = Arc::new(AtomicU64::new(0));

        Self::setup_signal_handler(ui_tx.clone());

        let default_theme = settings_mgr.settings().theme;

        // 运行时方案/主题/中英模式状态，平台线程通过 IPC GetSettings 读取，dispatch 时同步更新
        let current_settings: Arc<Mutex<(SchemeId, Theme, bool)>> = Arc::new(Mutex::new((
            default_scheme,
            default_theme,
            settings_mgr.settings().english_mode,
        )));

        // 最后已生效设置：dispatch 持久化时同步更新，watcher 据此跳过 daemon 自身写入，
        // 避免设置面板/托盘改一项设置触发两次热应用
        let last_applied_settings: Arc<Mutex<Settings>> =
            Arc::new(Mutex::new(settings_mgr.settings().clone()));

        // UI 渲染线程（只处理候选窗相关命令）
        let (ui_render_tx, ui_render_rx) = mpsc::channel::<UiCommand>();
        let initial_cw = settings_mgr.settings().candidate_window.clone();
        let ui_handle = thread::spawn(move || {
            run_candidate_window(ui_render_rx, default_theme, initial_cw);
        });

        // 引擎线程（带 panic 恢复）
        let engine_clone = Arc::clone(&engine);
        let ui_tx_clone = ui_tx.clone();
        let engine_generation = Arc::clone(&completion_generation);
        let engine_handle = thread::spawn(move || {
            Self::run_engine_thread(
                engine_clone,
                engine_rx,
                platform_tx,
                ui_tx_clone,
                completion_tx,
                engine_generation,
            );
        });

        // LLM 补全 worker 线程：独立异步调用，绝不阻塞引擎按键管线
        let completion_stop = Arc::new(AtomicBool::new(false));
        let completion_engine_tx = engine_tx.clone();
        let completion_ui_render_tx = ui_render_tx.clone();
        let completion_config_worker = Arc::clone(&completion_config);
        let completion_generation_worker = Arc::clone(&completion_generation);
        let completion_stop_worker = Arc::clone(&completion_stop);
        let completion_handle = thread::spawn(move || {
            completion::run_completion_worker(
                completion_rx,
                completion_engine_tx,
                completion_ui_render_tx,
                completion_config_worker,
                Arc::new(completion::HttpLlmClient),
                completion_generation_worker,
                completion_stop_worker,
            );
        });

        // 为 UI 命令分发和平台线程预先 clone 通道 / 共享状态
        let engine_tx_for_ui_dispatch = engine_tx.clone();
        let ui_render_tx_for_shutdown = ui_render_tx.clone();
        let ui_tx_for_platform = ui_tx.clone();
        let current_for_dispatch = Arc::clone(&current_settings);
        let last_applied_for_dispatch = Arc::clone(&last_applied_settings);

        // 平台线程（后台运行，避免阻塞主线程）
        let _platform_handle = thread::spawn(move || {
            if let Err(e) =
                run_platform(engine_tx, platform_rx, ui_tx_for_platform, current_settings)
            {
                error!("Platform IME error: {}", e);
            }
        });

        // 设置热更新线程：阻塞在 watch_rx 上，独立于主循环处理 settings.json 变更，
        // 设置面板保存后实时热应用。主循环只需专注分发 UI 命令。
        let watch_config_path = settings_mgr.config_path().clone();
        let watch_engine_tx = engine_tx_for_ui_dispatch.clone();
        let watch_ui_render_tx = ui_render_tx.clone();
        let watch_current = Arc::clone(&current_for_dispatch);
        let watch_last_applied = Arc::clone(&last_applied_settings);
        let watch_completion_config = Arc::clone(&completion_config);
        // 停止信号：graceful_shutdown 置位后，watch 线程在 recv_timeout 超时醒来退出
        let watch_stop = Arc::new(AtomicBool::new(false));
        let watch_stop_flag = Arc::clone(&watch_stop);
        let watch_handle = thread::spawn(move || {
            let settings_mgr = SettingsManager::new();
            let (watch_tx, watch_rx) = mpsc::channel::<notify::Result<notify::Event>>();
            let mut watcher = match notify::recommended_watcher(watch_tx) {
                Ok(w) => w,
                Err(e) => {
                    warn!("Failed to create settings watcher: {}", e);
                    return;
                }
            };
            if let Some(config_dir) = watch_config_path.parent() {
                // 目录可能尚不存在（首次运行未保存过设置），先创建再监听；
                // 监听目录而非文件本身，避免编辑器原子保存（rename 替换）时丢失事件。
                if let Err(e) = fs::create_dir_all(config_dir) {
                    warn!("Failed to create config dir {:?}: {}", config_dir, e);
                }
                if let Err(e) = watcher.watch(config_dir, notify::RecursiveMode::NonRecursive) {
                    warn!("Failed to watch config dir {:?}: {}", config_dir, e);
                }
            }
            // 外层 Ok 表示通道正常；notify 的错误事件（如监视目录被删除重建）
            // 通过 Ok(Err) 送达，记录日志后继续，避免热更新循环永久退出。
            // 用 recv_timeout 周期性检查停止信号，使 graceful_shutdown 能 join 本线程。
            while !watch_stop_flag.load(Ordering::Relaxed) {
                match watch_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(Ok(event)) => {
                        if event.paths.iter().any(|p| p == &watch_config_path)
                            && let Some(new_settings) = settings_mgr.try_reload()
                        {
                            // 以共享基线比较：daemon 自身保存（dispatch）会同步该基线，
                            // 因此可跳过自身写入引发的重复热应用
                            let last_applied = watch_last_applied.lock().unwrap().clone();
                            if new_settings != last_applied {
                                info!("Settings changed on disk, applying hot update");
                                Self::apply_settings_hot(
                                    &new_settings,
                                    &last_applied,
                                    &watch_engine_tx,
                                    &watch_ui_render_tx,
                                    &watch_current,
                                    &watch_completion_config,
                                );
                                *watch_last_applied.lock().unwrap() = new_settings;
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("Settings watcher error event, continuing: {:?}", e);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        // 主循环：阻塞在 ui_rx.recv() 上分发 UI 命令，等待退出事件
        // （Ctrl+C 或语言栏 Exit），退出信号统一经 UiCommand::Exit 事件传播。
        while let Ok(cmd) = ui_rx.recv() {
            if cmd == UiCommand::Exit {
                info!("UI dispatch: exit requested");
                break;
            }
            Self::dispatch_ui_command(
                cmd,
                &engine_tx_for_ui_dispatch,
                &ui_render_tx,
                &current_for_dispatch,
                &last_applied_for_dispatch,
            );
        }

        Self::graceful_shutdown(ShutdownParts {
            engine_tx: engine_tx_for_ui_dispatch,
            ui_tx: ui_render_tx_for_shutdown,
            ui_handle,
            engine_handle,
            watch_handle,
            watch_stop,
            completion_handle,
            completion_stop,
        });

        info!("Black-Hole IME daemon exited.");
        Ok(())
    }

    // ------------------------------------------------------------------
    // 初始化
    // ------------------------------------------------------------------

    /// 初始化 tracing 日志系统
    ///
    /// 返回的 guard 必须被持有，否则非阻塞文件 appender 会被提前刷新关闭。
    fn init_tracing() -> WorkerGuard {
        let log_dir = env::temp_dir();
        let file_appender = rolling::never(log_dir, "black-hole-daemon.log");
        let (non_blocking, guard) = non_blocking(file_appender);

        registry()
            .with(EnvFilter::from_default_env())
            .with(fmt::layer().with_writer(io::stdout))
            .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
            .init();

        guard
    }

    // ------------------------------------------------------------------
    // 输入法自动注册（Windows 专用）
    // ------------------------------------------------------------------

    #[cfg(target_os = "windows")]
    fn ensure_ime_registered() {
        if is_registered() {
            info!("IME already registered");
            return;
        }

        let dll_path = match env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("black_hole_platform.dll")))
        {
            Some(p) => p,
            None => {
                warn!("Could not determine DLL path for auto-registration");
                return;
            }
        };

        if !dll_path.exists() {
            warn!("Platform DLL not found at: {}", dll_path.display());
            return;
        }

        info!("IME not registered, attempting auto-registration...");
        match register_ime(&dll_path) {
            Ok(()) => info!("Auto-registration succeeded"),
            Err(e) => warn!("Auto-registration failed: {}", e),
        }
    }

    // ------------------------------------------------------------------
    // 输入法自动注册（Linux 专用）
    // ------------------------------------------------------------------

    #[cfg(target_os = "linux")]
    fn ensure_ime_registered() {
        if is_registered_linux() {
            info!("IBus component already registered");
            return;
        }

        info!("IBus component not registered, attempting auto-registration...");
        match register_ime_linux() {
            Ok(()) => info!("Auto-registration succeeded"),
            Err(e) => warn!("Auto-registration failed: {}", e),
        }
    }

    // ------------------------------------------------------------------
    // 信号处理
    // ------------------------------------------------------------------

    fn setup_signal_handler(ui_tx: mpsc::Sender<UiCommand>) {
        let result = ctrlc::set_handler(move || {
            info!("Received interrupt signal, requesting shutdown...");
            // ctrlc 回调运行在专用线程上，可安全发送事件
            let _ = ui_tx.send(UiCommand::Exit);
        });
        if let Err(e) = result {
            warn!("Failed to register Ctrl+C handler: {}", e);
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
        completion_tx: mpsc::Sender<CompletionRequest>,
        completion_generation: Arc<AtomicU64>,
    ) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut ctx = InputContext::default();
            while let Ok(cmd) = engine_rx.recv() {
                if cmd == EngineCommand::Shutdown {
                    info!("Engine thread received shutdown request");
                    break;
                }

                debug!("engine_thread start: cmd={:?}", discriminant(&cmd));

                let result = Self::process_engine_command(
                    &engine,
                    &mut ctx,
                    cmd,
                    &ui_tx,
                    &completion_tx,
                    &completion_generation,
                );

                if let Some(r) = result
                    && platform_tx.send(r).is_err()
                {
                    warn!("Platform receiver dropped, exiting engine thread");
                    break;
                }

                debug!("engine_thread end");
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
            error!(msg, "Engine thread panicked");
            // 尽量通知 UI 隐藏候选窗，避免界面残留
            let _ = ui_tx.send(UiCommand::HideCandidates);
        }
    }

    /// 分发来自平台层/UI 层的命令到对应处理者
    fn dispatch_ui_command(
        cmd: UiCommand,
        engine_tx: &mpsc::Sender<EngineCommand>,
        ui_render_tx: &mpsc::Sender<UiCommand>,
        current_settings: &Arc<Mutex<(SchemeId, Theme, bool)>>,
        last_applied: &Arc<Mutex<Settings>>,
    ) {
        match cmd {
            UiCommand::ShowSettings => {
                info!("UI dispatch: open settings");
                // winit 0.30 每进程仅允许一个事件循环，候选窗已占用 daemon 进程内的，
                // 设置面板须以独立进程（--settings-panel）方式运行。
                if let Ok(exe) = env::current_exe() {
                    if let Err(e) = Command::new(exe).arg("--settings-panel").spawn() {
                        error!("Failed to spawn settings panel: {}", e);
                    }
                } else {
                    error!("Failed to resolve current exe for settings panel");
                }
            }
            UiCommand::SwitchScheme(scheme_id) => {
                info!("UI dispatch: switch to {:?}", scheme_id);
                let _ = engine_tx.send(EngineCommand::SwitchScheme(scheme_id));
                // 持久化到设置，使重启后保持本次选择
                let mut settings_mgr = SettingsManager::new();
                settings_mgr.settings_mut().default_scheme = scheme_id;
                // 先同步"最后已生效设置"基线，再落盘，watcher 可识别本次为自身写入而跳过
                *last_applied.lock().unwrap() = settings_mgr.settings().clone();
                settings_mgr.save();
                // 同步共享状态，使平台线程（IPC GetSettings）返回最新值
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.0 = scheme_id;
                }
            }
            UiCommand::SetTheme(theme) => {
                info!("UI dispatch: set theme to {:?}", theme);
                let mut settings_mgr = SettingsManager::new();
                settings_mgr.settings_mut().theme = theme;
                *last_applied.lock().unwrap() = settings_mgr.settings().clone();
                settings_mgr.save();
                // 同步共享状态
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.1 = theme;
                }
                let _ = ui_render_tx.send(UiCommand::SetTheme(theme));
            }
            UiCommand::SetAutoStart(enabled) => {
                info!("UI dispatch: set auto start to {}", enabled);
                // 平台写入（注册表 Run 键 / XDG autostart 文件）
                if let Err(e) = set_auto_start(enabled) {
                    warn!("Failed to set auto start ({}): {}", enabled, e);
                }
                // 持久化到设置，使重启后保持本次选择
                let mut settings_mgr = SettingsManager::new();
                settings_mgr.settings_mut().auto_start = enabled;
                *last_applied.lock().unwrap() = settings_mgr.settings().clone();
                settings_mgr.save();
            }
            UiCommand::SetInputMode(english) => {
                info!("UI dispatch: set input mode english={}", english);
                // 持久化到设置，使重启后保持本次选择
                let mut settings_mgr = SettingsManager::new();
                settings_mgr.settings_mut().english_mode = english;
                *last_applied.lock().unwrap() = settings_mgr.settings().clone();
                settings_mgr.save();
                // 同步共享状态，使平台线程（IPC GetSettings）返回最新值
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.2 = english;
                }
            }
            UiCommand::Exit => {
                // 主循环在分发前已拦截 Exit 并结束事件循环，正常不会到达此处；
                // 保留分支以穷尽匹配，并防止误转发到候选窗线程。
                info!("UI dispatch: exit requested");
            }
            other => {
                // 候选窗等命令转发给 UI 渲染线程
                let _ = ui_render_tx.send(other);
            }
        }
    }

    /// 热应用磁盘上的新设置（设置面板保存后由主循环轮询触发）。
    ///
    /// 只对发生变化的部分生效，避免无谓的命令/重建开销：
    /// - 主题：同步共享状态并通知候选窗线程
    /// - 默认方案：通知引擎线程切换方案
    /// - 自启动：平台写入（注册表 Run 键 / XDG autostart 文件）
    /// - 候选窗参数：通知候选窗线程热更新字号/最大候选数
    /// - 按键绑定：通知引擎线程热更新按键映射
    /// - 中英模式：同步共享状态，供各进程 TSF 实例获得焦点时同步
    fn apply_settings_hot(
        new: &Settings,
        old: &Settings,
        engine_tx: &mpsc::Sender<EngineCommand>,
        ui_render_tx: &mpsc::Sender<UiCommand>,
        current_settings: &Arc<Mutex<(SchemeId, Theme, bool)>>,
        completion_config: &Arc<Mutex<LlmCompletionSettings>>,
    ) {
        if new.theme != old.theme {
            info!("Hot applying theme: {:?}", new.theme);
            {
                let mut cur = current_settings.lock().unwrap();
                cur.1 = new.theme;
            }
            let _ = ui_render_tx.send(UiCommand::SetTheme(new.theme));
        }

        if new.default_scheme != old.default_scheme {
            info!("Hot applying scheme: {:?}", new.default_scheme);
            let _ = engine_tx.send(EngineCommand::SwitchScheme(new.default_scheme));
            {
                let mut cur = current_settings.lock().unwrap();
                cur.0 = new.default_scheme;
            }
        }

        if new.auto_start != old.auto_start {
            info!("Hot applying auto start: {}", new.auto_start);
            if let Err(e) = set_auto_start(new.auto_start) {
                warn!("Failed to set auto start ({}): {}", new.auto_start, e);
            }
        }

        if new.candidate_window != old.candidate_window {
            info!("Hot applying candidate window settings");
            let _ = ui_render_tx.send(UiCommand::SetCandidateWindowSettings(
                new.candidate_window.clone(),
            ));
        }

        if new.key_bindings != old.key_bindings {
            info!("Hot applying key bindings");
            let _ = engine_tx.send(EngineCommand::UpdateKeyBindings(new.key_bindings.clone()));
            // 同步"整句上屏"实际绑定给候选窗，首行提示显示真实按键
            let _ = ui_render_tx.send(UiCommand::SetCommitSentenceKey(
                new.key_bindings.commit_sentence.clone(),
            ));
        }

        if new.english_mode != old.english_mode {
            info!("Hot applying input mode english={}", new.english_mode);
            {
                let mut cur = current_settings.lock().unwrap();
                cur.2 = new.english_mode;
            }
        }

        if new.llm_completion != old.llm_completion {
            info!("Hot applying LLM completion settings");
            *completion_config.lock().unwrap() = new.llm_completion.clone();
        }
    }

    /// 处理单个引擎命令，返回需要发送给平台层的结果
    fn process_engine_command(
        engine: &Arc<Mutex<Engine>>,
        ctx: &mut InputContext,
        cmd: EngineCommand,
        ui_tx: &mpsc::Sender<UiCommand>,
        completion_tx: &mpsc::Sender<CompletionRequest>,
        completion_generation: &Arc<AtomicU64>,
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
                    // 选中/上屏：递增代际号，终止所有在途补全请求
                    completion_generation.fetch_add(1, Ordering::SeqCst);
                }
                maybe_request_completion(&result, ctx, completion_tx);
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
                    // 选中候选上屏：递增代际号，终止所有在途补全请求
                    completion_generation.fetch_add(1, Ordering::SeqCst);
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
            EngineCommand::UpdateKeyBindings(bindings) => {
                let mut engine = engine.lock().unwrap();
                engine.process(&EngineCommand::UpdateKeyBindings(bindings), ctx);
                // 与 SwitchScheme 同理：由 daemon 主循环触发，无需响应。
                None
            }
            EngineCommand::UpdateCompletion(completion) => {
                let mut engine = engine.lock().unwrap();
                engine.process(&EngineCommand::UpdateCompletion(completion), ctx);
                // 由补全 worker 线程触发，无需响应。
                None
            }
            EngineCommand::Shutdown => {
                // 引擎线程循环已拦截 Shutdown 并退出，正常不会到达此处；
                // 保留分支以穷尽匹配。
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // 优雅退出
    // ------------------------------------------------------------------

    fn graceful_shutdown(parts: ShutdownParts) {
        info!("Shutting down daemon...");
        let ShutdownParts {
            engine_tx,
            ui_tx,
            ui_handle,
            engine_handle,
            watch_handle,
            watch_stop,
            completion_handle,
            completion_stop,
        } = parts;

        // 停止设置热更新线程：置位停止信号，其 recv_timeout 循环会退出
        watch_stop.store(true, Ordering::Relaxed);

        // 停止 LLM 补全 worker 线程：置位停止信号，其 recv_timeout 循环会退出
        completion_stop.store(true, Ordering::Relaxed);

        // 通知引擎线程退出：发送 Shutdown 唤醒其阻塞的 recv，使其及时结束
        if engine_tx.send(EngineCommand::Shutdown).is_err() {
            warn!("Engine channel closed, engine thread may have already exited");
        }

        // 通知 UI 退出
        if ui_tx.send(UiCommand::Exit).is_err() {
            warn!("UI channel closed, UI thread may have already exited");
        }

        // 等待设置热更新线程结束（5 秒超时）
        if !Self::join_with_timeout(watch_handle, "SettingsWatcher", Duration::from_secs(5)) {
            warn!("Settings watcher thread did not exit within timeout");
        }

        // 等待 LLM 补全 worker 线程结束（5 秒超时）
        if !Self::join_with_timeout(
            completion_handle,
            "CompletionWorker",
            Duration::from_secs(5),
        ) {
            warn!("Completion worker thread did not exit within timeout");
        }

        // 等待引擎线程结束（5 秒超时）
        if !Self::join_with_timeout(engine_handle, "Engine", Duration::from_secs(5)) {
            warn!("Engine thread did not exit within timeout");
        }

        // 等待 UI 线程结束（5 秒超时）
        if !Self::join_with_timeout(ui_handle, "UI", Duration::from_secs(5)) {
            warn!("UI thread did not exit within timeout");
        }
    }

    /// 等待线程结束，带超时
    fn join_with_timeout(handle: thread::JoinHandle<()>, name: &str, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = handle.join();
            let _ = tx.send(result);
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(())) => true,
            Ok(Err(_)) => {
                warn!("{} thread panicked during shutdown", name);
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

/// 优雅退出所需的线程句柄与通知通道（聚合参数，避免过长函数签名）
struct ShutdownParts {
    engine_tx: mpsc::Sender<EngineCommand>,
    ui_tx: mpsc::Sender<UiCommand>,
    ui_handle: thread::JoinHandle<()>,
    engine_handle: thread::JoinHandle<()>,
    watch_handle: thread::JoinHandle<()>,
    watch_stop: Arc<AtomicBool>,
    completion_handle: thread::JoinHandle<()>,
    completion_stop: Arc<AtomicBool>,
}

/// Composing 结果且存在选中候选时，向 LLM 补全 worker 投递请求。
///
/// 仅投递、绝不等待：LLM 结果经异步双通道回传，未就绪时 Tab 回退为
/// 仅提交选中词，与无补全行为一致。含首选（index 0）。
fn maybe_request_completion(
    result: &SchemeResult,
    ctx: &InputContext,
    completion_tx: &mpsc::Sender<CompletionRequest>,
) {
    if let SchemeResult::Composing {
        code,
        candidates,
        selected_index,
        ..
    } = result
        && let Some(selected) = candidates.get(*selected_index)
    {
        let _ = completion_tx.send(CompletionRequest {
            code: code.clone(),
            selected_index: *selected_index,
            selected_text: selected.text.clone(),
            preceding_text: ctx.preceding_text.clone(),
            following_text: ctx.following_text.clone(),
        });
    }
}

#[cfg(target_os = "windows")]
fn run_platform(
    engine_tx: mpsc::Sender<EngineCommand>,
    platform_rx: mpsc::Receiver<SchemeResult>,
    ui_tx: mpsc::Sender<UiCommand>,
    current_settings: Arc<Mutex<(SchemeId, Theme, bool)>>,
) -> Result<(), WindowsPlatformError> {
    let mut platform = WindowsTsfIme::new(current_settings);
    platform.run(engine_tx, platform_rx, ui_tx)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_platform(
    engine_tx: mpsc::Sender<EngineCommand>,
    platform_rx: mpsc::Receiver<SchemeResult>,
    ui_tx: mpsc::Sender<UiCommand>,
    _current_settings: Arc<Mutex<(SchemeId, Theme, bool)>>,
) -> Result<(), LinuxPlatformError> {
    let mut platform = LinuxIbusIme::new();
    platform.run(engine_tx, platform_rx, ui_tx)?;
    Ok(())
}
