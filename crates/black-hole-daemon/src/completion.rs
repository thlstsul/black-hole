//! LLM 整句补全：独立 worker 线程发起 OpenAI 兼容 HTTP 请求，
//! 结果通过两条旁路通道回传：
//! - `EngineCommand::UpdateCompletion`：引擎侧持有 hint，Tab 提交时校验后拼入；
//! - `UiCommand::Completion`：候选窗实时显示 ghost text。
//!
//! 引擎线程只负责往本模块的通道投递请求，绝不等待 LLM 响应，
//! 保证按键管线零阻塞；LLM 未就绪/失败/超时时行为与"无补全"等价。

use black_hole_shared::{CompletionHint, EngineCommand, LlmCompletionSettings, UiCommand};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tracing::{debug, error, warn};

/// 补全请求：由引擎线程在每次 Composing 结果产生时投递
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// 发起请求时的编码串（如拼音 "wo"）
    pub code: String,
    /// 发起请求时选中的候选索引
    pub selected_index: usize,
    /// 选中的候选文本（作为 LLM 续写种子）
    pub selected_text: String,
    /// 光标前的文本（当前输入位置之前的已上屏内容），供 LLM 理解句意
    pub preceding_text: Option<String>,
    /// 光标后的文本（可选项，多数场景为 None）
    pub following_text: Option<String>,
}

/// LLM 客户端抽象：便于单测注入 fake 实现，不依赖网络。
/// 需 `Send + Sync`：每次补全请求在独立线程执行，多个线程共享同一客户端。
pub trait LlmClient: Send + Sync {
    /// 请求一次补全；`Ok(None)` 表示模型返回了空结果（视为无补全）。
    /// 返回可 `abort` 的 future：调用方取消任务即真正中断进行中的网络请求
    /// （reqwest future drop 即关闭连接）。
    fn complete(
        &self,
        settings: LlmCompletionSettings,
        req: CompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send + '_>>;
}

/// 基于 reqwest 的 OpenAI 兼容 `/v1/chat/completions` 客户端
///
/// 同一协议同时覆盖本地（Ollama / llama.cpp / LM Studio）与云端服务。
/// 使用 async + 可 abort 的 future：终止补全时取消任务即关闭底层连接，
/// 网络请求本身不再发出/不再等待。
pub struct HttpLlmClient;

impl LlmClient for HttpLlmClient {
    fn complete(
        &self,
        settings: LlmCompletionSettings,
        req: CompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send + '_>> {
        Box::pin(async move {
            // DeepSeek V4 系列默认开启思考模式：会先消耗 token 推理、content 常为空。
            // 整句补全无需推理，固定关闭思考（thinking.disabled），max_tokens 全部用于生成补全。
            let body = serde_json::json!({
                "model": settings.model,
                "max_tokens": settings.max_tokens,
                "temperature": settings.temperature,
                "thinking": {"type": "disabled"},
                "messages": [
                    {"role": "system", "content": "你是中文输入法的整句补全助手。\n请续写光标处【】内词语之后的内容，输出一句自然通顺的中文，只输出续写部分，不超过 20 字，可含句末标点。"},
                    {"role": "user", "content": build_prompt(&req)},
                ],
            });

            // 分阶段超时：连接阶段短超时快速失败（DNS/建连），读取阶段用完整
            // 配置超时——云端 LLM（如 DeepSeek）推理耗时可能数秒。reqwest 的
            // `timeout` 是整体请求超时（含连接+读取），`connect_timeout` 单独
            // 限制建连。连接超时不宜过短：国内直连 api.deepseek.com 实测建连
            // 可耗时数秒，压到 5s 会在连接阶段就误报超时。
            let read_timeout = Duration::from_millis(settings.timeout_ms);
            let connect_timeout = read_timeout.min(Duration::from_secs(15));
            let client = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .timeout(read_timeout)
                .build()
                .map_err(|e| format!("HTTP 客户端构建失败: {}", e))?;

            let mut request = client.post(&settings.endpoint).json(&body);
            if !settings.api_key.is_empty() {
                request = request.bearer_auth(&settings.api_key);
            }

            let response = request
                .send()
                .await
                .map_err(|e| format!("LLM 请求失败: {}", describe_llm_error(&e, &settings)))?;
            let value: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("LLM 响应解析失败: {}", e))?;

            // 提取补全文本：只取标准 content（兼容旧式 choices[0].text），
            // 绝不回退到 reasoning_content——那是推理思考过程，不是补全结果。
            let content = ["/choices/0/message/content", "/choices/0/text"]
                .iter()
                .find_map(|path| value.pointer(path).and_then(|c| c.as_str()))
                .unwrap_or_default();

            // finish_reason=length 且 content 为空：推理模型 token 被思考过程耗尽，
            // 最终回答未生成，日志给出明确的排查方向。
            let finish_reason = value
                .pointer("/choices/0/finish_reason")
                .and_then(|c| c.as_str())
                .unwrap_or_default();
            if content.is_empty() && finish_reason == "length" {
                warn!(
                    "LLM 响应因 max_tokens={} 不足被截断（finish_reason=length），content 为空；\
                 推理模型会先消耗 token 思考，请把设置面板的“最大 Tokens”调大（如 256~512）",
                    settings.max_tokens
                );
            }
            Ok(normalize_completion(content, &req.selected_text))
        })
    }
}

/// 构造 LLM 提示词：优先利用光标前文作为句意上下文，再要求续写选中词。
/// 前文为空时退化为仅基于选中词的提示。
fn build_prompt(req: &CompletionRequest) -> String {
    let context = match (&req.preceding_text, &req.following_text) {
        (Some(pre), Some(post)) => format!("{}【{}】{}", pre, req.selected_text, post),
        (Some(pre), None) => format!("{}【{}】", pre, req.selected_text),
        (None, Some(post)) => format!("【{}】{}", req.selected_text, post),
        (None, None) => format!("【{}】", req.selected_text),
    };
    format!("光标处文本是：{}", context)
}

/// 清理 LLM 返回：去首尾空白与前导标点；若结果以选中词开头（模型重复了种子）
/// 则剥离该前缀；空结果返回 None。句末标点保留。
fn normalize_completion(text: &str, seed: &str) -> Option<String> {
    let trim_chars = |c: char| "，。！？,.!?；：、\"'“”‘’（）() \t\n\r".contains(c);
    let mut s = text.trim().trim_start_matches(trim_chars).to_string();
    if let Some(rest) = s.strip_prefix(seed) {
        s = rest.trim().trim_start_matches(trim_chars).to_string();
    }
    if s.is_empty() { None } else { Some(s) }
}

/// 将 reqwest 错误转换为可读的排查提示：区分 HTTP 状态错误与网络/超时错误，
/// 并对常见的模型名/API Key 配置错误给出提示。
fn describe_llm_error(e: &reqwest::Error, settings: &LlmCompletionSettings) -> String {
    if e.is_timeout() {
        format!(
            "请求超时（已等待 {}ms；若直连云端 API 请检查网络/代理，或调大超时设置）",
            settings.timeout_ms
        )
    } else if let Some(status) = e.status() {
        format!(
            "HTTP {}（请检查模型名 \"{}\" 与 API Key 是否正确）",
            status.as_u16(),
            settings.model
        )
    } else if e.is_connect() {
        format!(
            "无法连接端点 {}（请检查地址是否可达/需代理）",
            settings.endpoint
        )
    } else {
        e.to_string()
    }
}

/// LLM 补全 worker 线程主循环
///
/// - 阻塞在 `recv_timeout` 上，周期性检查停止信号以便优雅退出；
/// - 收到请求后先等一个**去抖窗口**（输入停顿期），期间持续收集最新请求、
///   只服务停顿后的最后一条（用户快速连打/翻候选时合并为一次 LLM 调用）；
/// - 与上次实际处理相同的 (code, selected_index) 直接跳过，避免方向键导航
///   等未改变输入状态的操作重复触发网络请求；
/// - 每次有效请求在 **tokio 任务**中执行 async LLM 调用，并持有在途任务的
///   `JoinHandle`：**新请求发起、或引擎侧上屏（`latest_seq` 递增）时
///   `abort()` 该任务**——reqwest future 被 drop 即关闭底层连接，
///   网络请求本身真正终止，而非"发出后丢弃结果"；
/// - 请求结果经 channel 回传，主循环按 `seq` 校验：过期结果直接丢弃，
///   不发引擎/UI 双通道，避免过期补全覆盖新结果。
pub fn run_completion_worker(
    rx: mpsc::Receiver<CompletionRequest>,
    engine_tx: mpsc::Sender<EngineCommand>,
    ui_render_tx: mpsc::Sender<UiCommand>,
    config: Arc<Mutex<LlmCompletionSettings>>,
    client: Arc<dyn LlmClient>,
    latest_seq: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    // 去抖窗口：用户输入停顿超过该时长才真正发起 LLM 请求
    const DEBOUNCE: Duration = Duration::from_millis(250);
    let mut last_key: Option<(String, usize)> = None;
    // 请求结果回传通道：tokio 任务完成后送回主循环处理
    let (result_tx, result_rx) =
        mpsc::channel::<(u64, CompletionRequest, Result<Option<String>, String>)>();
    // 当前在途请求任务句柄：新请求发起/上屏时 abort 以真正终止网络请求
    let mut inflight: Option<tokio::task::JoinHandle<()>> = None;
    // 上次观察到的代际号：引擎 Committed 递增时据此 abort 在途请求
    let mut last_generation = latest_seq.load(Ordering::SeqCst);

    // tokio runtime：驱动 async 请求；abort JoinHandle 即取消 reqwest future、关闭连接
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(
                "Failed to create tokio runtime for completion worker: {}",
                e
            );
            return;
        }
    };

    while !stop.load(Ordering::Relaxed) {
        // 先处理已返回的结果（被 abort 的任务不会送达结果）
        while let Ok((seq, req, result)) = result_rx.try_recv() {
            if latest_seq.load(Ordering::SeqCst) != seq {
                debug!(
                    "completion worker: stale result dropped (seq={}, code='{}')",
                    seq, req.code
                );
                continue;
            }
            deliver_result(&engine_tx, &ui_render_tx, req, result);
        }

        // 代际号变化（引擎上屏 Committed 递增）：abort 在途请求，网络请求真正终止
        let current_generation = latest_seq.load(Ordering::SeqCst);
        if current_generation != last_generation {
            if let Some(handle) = inflight.take() {
                handle.abort();
                debug!("completion worker: aborted in-flight request (generation changed)");
            }
            last_generation = current_generation;
        }

        let mut req = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // 快照出队时的代际号：去抖结束、spawn 前再比对一次，期间若已发生
        // 上屏（引擎 Committed 递增）则丢弃本请求，防止"上屏前排队、上屏后
        // 处理"的请求拿到新序号后仍产出可交付的过期补全。
        let generation_at_dequeue = latest_seq.load(Ordering::SeqCst);
        // 去抖：等待输入停顿窗口，期间持续收走更新的请求，只保留最后一条
        let deadline = Instant::now() + DEBOUNCE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(newer) => req = newer,
                Err(mpsc::RecvTimeoutError::Timeout)
                | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // 去抖期间代际号已变化（发生上屏/新请求）：本请求作废，不再发起
        if latest_seq.load(Ordering::SeqCst) != generation_at_dequeue {
            debug!(
                "completion worker: request invalidated during debounce (code='{}')",
                req.code
            );
            continue;
        }

        // 与上次相同输入状态：跳过（方向键导航/重复输入不重复请求）
        let key = (req.code.clone(), req.selected_index);
        if last_key.as_ref() == Some(&key) {
            continue;
        }
        last_key = Some(key);

        debug!(
            "completion worker: request code='{}' selected_index={} text='{}'",
            req.code, req.selected_index, req.selected_text
        );

        let settings = config.lock().unwrap().clone();
        if !settings.enabled {
            // 未启用：清空两侧旧补全，避免残留 ghost text
            let _ = engine_tx.send(EngineCommand::UpdateCompletion(None));
            let _ = ui_render_tx.send(UiCommand::Completion {
                code: req.code.clone(),
                selected_index: req.selected_index,
                text: None,
            });
            continue;
        }

        // 发起新请求：先 abort 在途旧请求（网络请求真正终止），再 spawn 新任务
        if let Some(handle) = inflight.take() {
            handle.abort();
            debug!("completion worker: aborted in-flight request (new request)");
        }
        let seq = latest_seq.fetch_add(1, Ordering::SeqCst) + 1;
        last_generation = seq;

        let client = Arc::clone(&client);
        let result_tx = result_tx.clone();
        let handle = rt.spawn(async move {
            let result = client.complete(settings, req.clone()).await;
            let _ = result_tx.send((seq, req, result));
        });
        inflight = Some(handle);
    }

    // 退出：abort 在途请求，避免残留网络任务
    if let Some(handle) = inflight.take() {
        handle.abort();
    }
}

/// 将请求结果经引擎/UI 双通道回传（调用方已确保 seq 未过期）
fn deliver_result(
    engine_tx: &mpsc::Sender<EngineCommand>,
    ui_render_tx: &mpsc::Sender<UiCommand>,
    req: CompletionRequest,
    result: Result<Option<String>, String>,
) {
    match result {
        Ok(Some(text)) => {
            debug!("completion worker: result='{}'", text);
            let hint = CompletionHint {
                code: req.code.clone(),
                selected_index: req.selected_index,
                text: text.clone(),
            };
            let _ = engine_tx.send(EngineCommand::UpdateCompletion(Some(hint)));
            let _ = ui_render_tx.send(UiCommand::Completion {
                code: req.code.clone(),
                selected_index: req.selected_index,
                text: Some(text),
            });
        }
        Ok(None) => {
            // 模型返回空：视为无补全
            debug!(
                "completion worker: empty result for code='{}' text='{}'",
                req.code, req.selected_text
            );
            let _ = engine_tx.send(EngineCommand::UpdateCompletion(None));
            let _ = ui_render_tx.send(UiCommand::Completion {
                code: req.code.clone(),
                selected_index: req.selected_index,
                text: None,
            });
        }
        Err(e) => {
            warn!("LLM completion failed: {}", e);
            let _ = engine_tx.send(EngineCommand::UpdateCompletion(None));
            let _ = ui_render_tx.send(UiCommand::Completion {
                code: req.code.clone(),
                selected_index: req.selected_index,
                text: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_completion_plain() {
        assert_eq!(
            normalize_completion(" 是中国人。", "我"),
            Some("是中国人。".to_string())
        );
    }

    #[test]
    fn test_normalize_completion_repeats_seed() {
        assert_eq!(
            normalize_completion("我是中国人", "我"),
            Some("是中国人".to_string())
        );
    }

    #[test]
    fn test_normalize_completion_empty() {
        assert_eq!(normalize_completion("", "我"), None);
        assert_eq!(normalize_completion("，，，", "我"), None);
    }
}
