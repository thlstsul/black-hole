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
use black_hole_platform::windows_tsf::auto_register::{
    enable_keyboard, is_profile_enabled, is_registered, register_ime,
};
#[cfg(target_os = "linux")]
use black_hole_platform::{LinuxIbusIme, PlatformError as LinuxPlatformError};
#[cfg(target_os = "windows")]
use black_hole_platform::{PlatformError as WindowsPlatformError, WindowsTsfIme};
use black_hole_shared::{
    EngineCommand, InputContext, LlmCompletionSettings, RuntimeSettings, SchemeId, SchemeResult,
    Settings, Theme, UiCommand,
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

        // 运行时方案/主题/中英模式/自动切换开关状态，平台线程通过 IPC GetSettings 读取，dispatch 时同步更新
        // 中英模式是运行时状态而非设置项：daemon 启动播种为中文（false），
        // 会话内由 SetInputMode/SetInputModeTransient 更新共享状态供跨进程
        // 同步，不持久化 settings.json，重启后回到中文。连接时各平台实例
        // 直接应用 daemon 当前模式（含会话内已切换的英文）：英文模式下
        // 自动切换已由 OnTestKeyDown 补设 context 正常触发，无需再以
        // "连接默认中文"兜底。
        // auto_switch 是持久化设置项，须在启动时从 settings 恢复，否则用户开启的
        // 自动切换在 daemon 重启后静默失效（直到下次手动切换或热更新才恢复）。
        let current_settings: Arc<Mutex<RuntimeSettings>> = Arc::new(Mutex::new(
            Self::seed_runtime_settings(settings_mgr.settings(), default_scheme, default_theme),
        ));

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
                Arc::new(completion::HttpLlmClient::new().expect("HTTP 客户端构建失败")),
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
        if !is_registered() {
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
            if let Err(e) = register_ime(&dll_path) {
                warn!("Auto-registration failed: {}", e);
                return;
            }
            info!("Auto-registration succeeded");
        } else {
            info!("IME already registered");
        }

        // 已注册但未启用（未添加到输入法列表）时自动启用，等效于设置中"添加键盘"。
        // 用户在设置中手动移除键盘后，此调用会重新启用；注册后首次运行也会走到这里。
        if is_profile_enabled() {
            info!("IME keyboard already enabled");
            return;
        }

        info!("IME keyboard not enabled, attempting to add to input method list...");
        match enable_keyboard() {
            Ok(()) => info!("Keyboard enabled successfully"),
            Err(e) => warn!("Failed to enable keyboard: {}", e),
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
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            error!(msg, "Engine thread panicked");
            // 尽量通知 UI 隐藏候选窗，避免界面残留
            let _ = ui_tx.send(UiCommand::HideCandidates);
        }
    }

    /// 依据持久化设置初始化运行时共享状态。
    ///
    /// `auto_switch` 是持久化设置项（settings.json 的 auto_switch_mode），
    /// 启动时必须恢复，否则用户开启的自动切换在 daemon 重启后静默失效；
    /// `english` 是运行时状态而非设置项，启动播种为中文（false），
    /// 会话内由 SetInputMode/SetInputModeTransient 更新。
    fn seed_runtime_settings(
        settings: &Settings,
        default_scheme: SchemeId,
        default_theme: Theme,
    ) -> RuntimeSettings {
        let mut runtime = RuntimeSettings::new(default_scheme, default_theme);
        runtime.auto_switch = settings.auto_switch_mode;
        runtime
    }

    /// 创建一个以"磁盘最新可用值"为基线的 SettingsManager。
    ///
    /// `SettingsManager::new()` 在读取/解析失败（如设置面板正在写入的
    /// 半写文件）时会回退到默认值；直接以默认值为基线再保存会把面板的
    /// 真实设置覆盖掉。因此仅当磁盘加载确实失败（`load_ok() == false`）
    /// 时才改用 `last_applied` 内存基线，保证 dispatch 的读-改-写只修改
    /// 目标字段、不丢失其它字段；合法默认值文件与解析失败不再混淆。
    fn settings_manager_with_latest(last_applied: &Settings) -> SettingsManager {
        Self::apply_last_applied_fallback(SettingsManager::new(), last_applied)
    }

    /// 加载失败时以 `last_applied` 覆盖默认值基线（核心决策，独立可测）。
    ///
    /// `load_ok() == false`（读取/解析失败，如半写文件）时，把默认值回退
    /// 替换为 daemon 内存中最后一次生效的 `last_applied`，防止 dispatch
    /// 以默认值为基线再保存而静默覆盖用户真实设置；`load_ok() == true`
    /// （含文件不存在按默认值处理）时保持磁盘内容不变。
    /// 独立成纯函数：配合 `SettingsManager::from_config_path` 注入临时路径，
    /// 可直接验证损坏/有效文件的回退决策。
    fn apply_last_applied_fallback(
        mut mgr: SettingsManager,
        last_applied: &Settings,
    ) -> SettingsManager {
        if !mgr.load_ok() {
            warn!("settings.json unreadable, using last applied settings as baseline");
            *mgr.settings_mut() = last_applied.clone();
        }
        mgr
    }

    /// 以磁盘最新可用值为基线修改设置，保存并同步 last_applied。
    fn modify_and_save_settings<F>(last_applied: &Arc<Mutex<Settings>>, mutate: F)
    where
        F: FnOnce(&mut Settings),
    {
        let last = last_applied.lock().unwrap().clone();
        let mut settings_mgr = Self::settings_manager_with_latest(&last);
        mutate(settings_mgr.settings_mut());
        *last_applied.lock().unwrap() = settings_mgr.settings().clone();
        settings_mgr.save();
    }

    /// 分发来自平台层/UI 层的命令到对应处理者
    fn dispatch_ui_command(
        cmd: UiCommand,
        engine_tx: &mpsc::Sender<EngineCommand>,
        ui_render_tx: &mpsc::Sender<UiCommand>,
        current_settings: &Arc<Mutex<RuntimeSettings>>,
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
                // 先克隆基线再读盘：settings_manager_with_latest 内部会加载
                // settings.json，避免持 last_applied 锁跨磁盘 I/O（watcher
                // 线程每次文件变更也要获取该锁，缩短锁竞争窗口）。
                Self::modify_and_save_settings(last_applied, |s| s.default_scheme = scheme_id);
                // 同步共享状态，使平台线程（IPC GetSettings）返回最新值
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.scheme_id = scheme_id;
                }
            }
            UiCommand::SetTheme(theme) => {
                info!("UI dispatch: set theme to {:?}", theme);
                Self::modify_and_save_settings(last_applied, |s| s.theme = theme);
                // 同步共享状态
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.theme = theme;
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
                Self::modify_and_save_settings(last_applied, |s| s.auto_start = enabled);
            }
            UiCommand::SetInputMode(english) | UiCommand::SetInputModeTransient(english) => {
                // 中英模式是运行时状态而非设置项：手动切换（SetInputMode）与自动切换
                // 上报（SetInputModeTransient）处理一致——仅更新共享状态，供平台线程
                // （IPC GetSettings）与其它进程 TSF 实例同步，不持久化 settings.json。
                // 手动切换的详细日志已由平台层（Ctrl/语言栏路径）记录，此处用 debug。
                debug!("UI dispatch: set input mode english={}", english);
                let mut cur = current_settings.lock().unwrap();
                cur.english = english;
            }
            UiCommand::SetAutoSwitch(enabled) => {
                info!("UI dispatch: set auto switch mode to {}", enabled);
                // 持久化到设置，使重启后保持本次选择
                Self::modify_and_save_settings(last_applied, |s| s.auto_switch_mode = enabled);
                // 同步共享状态，使平台线程（IPC GetSettings）返回最新值
                {
                    let mut cur = current_settings.lock().unwrap();
                    cur.auto_switch = enabled;
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
    /// - 自动切换中英模式开关：同步共享状态，供平台层每次评估时读取
    /// - LLM 补全：更新补全 worker 的配置
    fn apply_settings_hot(
        new: &Settings,
        old: &Settings,
        engine_tx: &mpsc::Sender<EngineCommand>,
        ui_render_tx: &mpsc::Sender<UiCommand>,
        current_settings: &Arc<Mutex<RuntimeSettings>>,
        completion_config: &Arc<Mutex<LlmCompletionSettings>>,
    ) {
        if new.theme != old.theme {
            info!("Hot applying theme: {:?}", new.theme);
            {
                let mut cur = current_settings.lock().unwrap();
                cur.theme = new.theme;
            }
            let _ = ui_render_tx.send(UiCommand::SetTheme(new.theme));
        }

        if new.default_scheme != old.default_scheme {
            info!("Hot applying scheme: {:?}", new.default_scheme);
            let _ = engine_tx.send(EngineCommand::SwitchScheme(new.default_scheme));
            {
                let mut cur = current_settings.lock().unwrap();
                cur.scheme_id = new.default_scheme;
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

        if new.auto_switch_mode != old.auto_switch_mode {
            info!("Hot applying auto switch mode: {}", new.auto_switch_mode);
            {
                let mut cur = current_settings.lock().unwrap();
                cur.auto_switch = new.auto_switch_mode;
            }
        }

        if new.llm_completion != old.llm_completion {
            info!("Hot applying LLM completion settings");
            *completion_config.lock().unwrap() = new.llm_completion.clone();
        }
    }

    /// 处理上屏/取消结果：通知 UI 并递增补全代际号以废弃在途请求。
    fn handle_commit_result(
        result: &SchemeResult,
        ui_tx: &mpsc::Sender<UiCommand>,
        completion_generation: &Arc<AtomicU64>,
    ) {
        if let SchemeResult::Committed { text, .. } = result {
            let _ = ui_tx.send(UiCommand::CommitText(text.clone()));
            // 选中/上屏：递增代际号，终止所有在途补全请求
            completion_generation.fetch_add(1, Ordering::SeqCst);
        } else if matches!(result, SchemeResult::Cancelled) {
            // 取消输入：隐藏候选窗，并递增代际号终止所有在途补全请求
            // （编码已重置，旧补全结果已无意义）
            let _ = ui_tx.send(UiCommand::HideCandidates);
            completion_generation.fetch_add(1, Ordering::SeqCst);
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
            EngineCommand::Shutdown => None,
            EngineCommand::SwitchScheme(_)
            | EngineCommand::UpdateKeyBindings(_)
            | EngineCommand::UpdateCompletion(_) => {
                let mut engine = engine.lock().unwrap();
                engine.process(&cmd, ctx);
                None
            }
            EngineCommand::Reset => {
                let mut engine = engine.lock().unwrap();
                let result = engine.process(&cmd, ctx);
                let _ = ui_tx.send(UiCommand::HideCandidates);
                Some(result)
            }
            _ => {
                let mut engine = engine.lock().unwrap();
                let result = engine.process(&cmd, ctx);
                Self::handle_commit_result(&result, ui_tx, completion_generation);
                maybe_request_completion(&result, ctx, completion_tx);
                Some(result)
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
    current_settings: Arc<Mutex<RuntimeSettings>>,
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
    current_settings: Arc<Mutex<RuntimeSettings>>,
) -> Result<(), LinuxPlatformError> {
    let mut platform = LinuxIbusIme::new(current_settings);
    platform.run(engine_tx, platform_rx, ui_tx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn seed_runtime_settings_restores_auto_switch() {
        // auto_switch 是持久化设置项：daemon 重启后须从 settings 恢复，
        // 否则用户开启的自动切换静默失效
        let settings = Settings {
            auto_switch_mode: true,
            ..Settings::default()
        };
        let runtime = App::seed_runtime_settings(&settings, SchemeId::Pinyin, Theme::Dark);
        assert!(runtime.auto_switch);
        assert_eq!(runtime.scheme_id, SchemeId::Pinyin);
        assert_eq!(runtime.theme, Theme::Dark);
    }

    #[test]
    fn seed_runtime_settings_auto_switch_disabled_by_default() {
        // 未开启自动切换时保持关闭
        let settings = Settings::default();
        let runtime = App::seed_runtime_settings(&settings, SchemeId::Pinyin, Theme::Dark);
        assert!(!runtime.auto_switch);
    }

    #[test]
    fn seed_runtime_settings_english_always_chinese() {
        // english 是运行时状态而非设置项：启动播种恒为中文（false），
        // 不随任何持久化值恢复
        let settings = Settings::default();
        let runtime = App::seed_runtime_settings(&settings, SchemeId::Shuangpin, Theme::Light);
        assert!(!runtime.english);
        assert_eq!(runtime.scheme_id, SchemeId::Shuangpin);
    }

    /// 每个测试使用独立的临时配置目录，避免并行测试互相干扰
    fn temp_config_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "black-hole-daemon-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[test]
    fn apply_last_applied_fallback_uses_baseline_on_corrupt_file() {
        // 数据保护核心：settings.json 损坏（半写文件）时，dispatch 必须以
        // last_applied 为基线而非默认值——否则托盘操作以默认值再保存会
        // 静默覆盖用户真实设置
        let path = temp_config_path("corrupt");
        fs::write(&path, "{not valid json").unwrap();

        let last_applied = Settings {
            default_scheme: SchemeId::Shuangpin,
            auto_switch_mode: true,
            ..Settings::default()
        };

        let mgr = App::apply_last_applied_fallback(
            SettingsManager::from_config_path(path),
            &last_applied,
        );
        assert_eq!(
            mgr.settings(),
            &last_applied,
            "损坏文件应以 last_applied 为基线"
        );
    }

    #[test]
    fn apply_last_applied_fallback_keeps_valid_disk_content() {
        // 有效文件：保持磁盘内容不变，不得用 last_applied 覆盖
        let path = temp_config_path("valid");
        let disk = Settings {
            default_scheme: SchemeId::Shuangpin,
            auto_switch_mode: true,
            ..Settings::default()
        };
        fs::write(&path, serde_json::to_string(&disk).unwrap()).unwrap();

        // last_applied 故意与磁盘内容不同，验证有效文件路径不应用基线
        let last_applied = Settings::default();
        let mgr = App::apply_last_applied_fallback(
            SettingsManager::from_config_path(path),
            &last_applied,
        );
        assert_eq!(mgr.settings(), &disk, "有效文件应保留磁盘内容");
    }

    #[test]
    fn apply_last_applied_fallback_keeps_disk_on_missing_file() {
        // 文件不存在（首次运行）：默认值合法，视为加载成功（load_ok=true），
        // 不得误判为失败而用 last_applied 覆盖
        let path = temp_config_path("missing");
        // 不创建文件
        let last_applied = Settings {
            default_scheme: SchemeId::Shuangpin,
            ..Settings::default()
        };
        let mgr = App::apply_last_applied_fallback(
            SettingsManager::from_config_path(path),
            &last_applied,
        );
        assert_eq!(
            mgr.settings(),
            &Settings::default(),
            "文件不存在应保留默认值（不应用 last_applied）"
        );
    }
}
