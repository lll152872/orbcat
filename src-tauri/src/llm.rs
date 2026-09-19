//! OpenAI 兼容的 chat completions 客户端
//!
//! 支持：
//!   - tool calls（函数调用）
//!   - **多模态输入**（文本 + 图片，走 OpenAI 的 `content` 数组格式）
//!
//! 所有配置的模型（sensenova / glm / openrouter / ...）都走同一套 OpenAI 兼容协议，
//! 所以这里不区分厂商。
//!
//! ⚠️ API key 只用于请求头，**绝不**进入日志或错误信息。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::ModelConfig;

// ---------------------------------------------------------------------------
// 消息内容（支持多模态）
// ---------------------------------------------------------------------------

/// 消息内容：纯文本，或多部分（文本 + 图片）
///
/// 用 `#[serde(untagged)]` 让它序列化成 OpenAI 期望的两种形态之一：
///   - 纯文本：`"你好"`
///   - 多模态：`[{"type":"text",...},{"type":"image_url",...}]`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    /// 可以是 `https://...` 或 `data:image/png;base64,...`
    pub url: String,
}

impl MessageContent {
    pub fn text(s: impl Into<String>) -> Self {
        MessageContent::Text(s.into())
    }

    /// 拼出可读文本（用于日志 / 工具结果展示）
    pub fn as_plain(&self) -> String {
        match self {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.clone()),
                    ContentPart::ImageUrl { .. } => Some("[图片]".to_string()),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(MessageContent::text(content)),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }

    /// 手工构造 assistant 消息（目前 agent loop 直接复用响应对象，此处留作 API）
    #[allow(dead_code)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain("assistant", content)
    }

    /// **多模态**：一段文本 + 若干张图片（data URL 或 http URL）
    pub fn user_with_images(text: impl Into<String>, image_urls: Vec<String>) -> Self {
        let mut parts = vec![ContentPart::Text { text: text.into() }];
        for url in image_urls {
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl { url },
            });
        }
        Self {
            role: "user".into(),
            content: Some(MessageContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// 工具执行结果回灌
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(MessageContent::text(content)),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn default_tool_type() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON 字符串形式的参数（模型给的，需要自己解析）
    #[serde(default)]
    pub arguments: String,
}

// ---------------------------------------------------------------------------
// 请求 / 响应
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// 一次调用的结果
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
    /// 思维链（部分模型会给；流式下逐块累加）
    pub reasoning: Option<String>,
}

impl ChatOutcome {
    pub fn tool_calls(&self) -> &[ToolCall] {
        self.message.tool_calls.as_deref().unwrap_or(&[])
    }
    pub fn text(&self) -> String {
        self.message
            .content
            .as_ref()
            .map(MessageContent::as_plain)
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// 限流/临时故障时的最大重试次数
const MAX_RETRIES: usize = 4;
/// 首次重试等待时长（之后翻倍）
const FIRST_BACKOFF: Duration = Duration::from_secs(2);

/// 这个错误值得重试吗？
///
/// 值得：429（限流）、5xx（服务端临时故障）、连接类错误
/// 不值得：401/403（key 问题）、400（请求本身有问题）、404（模型名错）
fn is_retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// 发起一次 chat completion（**带限流退避重试**）
///
/// 为什么要重试：轻量模型（如 sensenova-flash-lite）的 TPM/RPM 配额不高，
/// agent loop 一轮要发多次请求，很容易撞 429。限流是**临时**的，
/// 直接放弃对用户很不友好 —— 退避重试基本都能撑过去。
pub async fn chat(
    cfg: &ModelConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<Value>>,
) -> Result<ChatOutcome, String> {
    let mut backoff = FIRST_BACKOFF;
    let mut last_err = String::new();

    for attempt in 1..=MAX_RETRIES {
        match chat_once(cfg, messages.clone(), tools.clone()).await {
            Ok(out) => return Ok(out),
            Err(ChatError::Retryable(status, msg)) => {
                last_err = msg;
                if attempt == MAX_RETRIES {
                    break;
                }
                eprintln!(
                    "[float-agent] {status} 限流/临时故障，{:?} 后重试（第 {attempt}/{MAX_RETRIES} 次）",
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            Err(ChatError::Fatal(msg)) => return Err(msg),
        }
    }

    Err(format!(
        "模型接口持续限流或无响应，已退避重试 {MAX_RETRIES} 次仍未成功。\n\
         原始错误：{last_err}\n\n\
         建议：稍等十几秒再发一次，或在设置里换一个模型（当前：{}）。",
        cfg.id
    ))
}

/// 区分可重试与不可重试的错误
enum ChatError {
    /// (HTTP 状态码, 原始信息)
    Retryable(u16, String),
    Fatal(String),
}

// ---------------------------------------------------------------------------
// 流式（SSE）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// 部分模型（含 sensenova）会单独给思维链
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    /// 流式下 tool_calls 是**分片**的，靠 index 归并
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// 流式增量回调：`(正文增量, 思维链增量)`
pub type DeltaFn = dyn Fn(Option<String>, Option<String>) + Send + Sync;

/// 等到取消标志置位（配合 `tokio::select!` 做可中断的网络等待）。
async fn wait_cancel(cancel: &std::sync::atomic::AtomicBool) {
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 流式发起 chat completion。
///
/// 用于 agent loop 的每一轮：正文增量实时推给用户，同时把分片的
/// `tool_calls` 拼回完整结构（拼法与增量正文互不影响）。
#[allow(clippy::too_many_lines)]
async fn chat_stream_once(
    cfg: &ModelConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<Value>>,
    on_delta: &DeltaFn,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ChatOutcome, ChatError> {
    use futures_util::StreamExt;
    use std::sync::atomic::Ordering;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| ChatError::Fatal(format!("创建 HTTP 客户端失败: {e}")))?;

    let body = ChatRequest {
        model: cfg.id.clone(),
        messages,
        tools,
        tool_choice: Some("auto".into()),
        stream: true,
    };

    let endpoint = cfg.chat_endpoint();

    let mut req = client
        .post(&endpoint)
        .bearer_auth(&cfg.api_key)
        // 自定义头（含按需自动补的 x-opencode-session 等）
        .json(&body);
    for (k, v) in cfg.effective_headers() {
        req = req.header(k, v);
    }

    let resp = tokio::select! {
        r = req.send() => r.map_err(|e| ChatError::Fatal(format!("请求 {endpoint} 失败: {e}")))?,
        // 等待响应（首字节延迟）也可被取消 —— 否则用户点了停止还要干等
        _ = wait_cancel(cancel) => return Err(ChatError::Fatal("已停止".into())),
    };

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        let brief: String = text.chars().take(400).collect();
        let msg = format!("HTTP {status}（{endpoint}）：{brief}");
        return Err(if is_retryable(code) {
            ChatError::Retryable(code, msg)
        } else {
            ChatError::Fatal(msg)
        });
    }

    // ---- 逐块解析 SSE ----
    let mut stream = resp.bytes_stream();
    let mut buf = String::new(); // 半行缓冲：chunk 边界可能切断一行
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason: Option<String> = None;
    // index → (id, name, arguments)
    let mut partial: Vec<(String, String, String)> = Vec::new();

    while let Some(chunk) = stream.next().await {
        // 取消检查：做到「流粒度」——用户点停止后下一个 chunk（几十毫秒）就退出，
        // 不必等整轮模型输出完。这是「点停止没反应」的关键修复点。
        if cancel.load(Ordering::Relaxed) {
            return Err(ChatError::Fatal("已停止".into()));
        }
        let bytes = chunk.map_err(|e| ChatError::Fatal(format!("读取流失败: {e}")))?;
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // 按行处理，最后一行可能不完整 → 留在 buf 里
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim().to_string();
            buf.drain(..=pos);

            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                break;
            }

            let Ok(ch) = serde_json::from_str::<StreamChunk>(payload) else {
                continue; // 个别实现会插入注释行，忽略即可
            };
            let Some(choice) = ch.choices.into_iter().next() else {
                continue;
            };

            if let Some(fr) = choice.finish_reason {
                finish_reason = Some(fr);
            }

            let d = choice.delta;

            // 思维链增量
            if let Some(r) = d.reasoning_content.filter(|s| !s.is_empty()) {
                reasoning.push_str(&r);
                on_delta(None, Some(r));
            }

            // 正文增量
            if let Some(c) = d.content.filter(|s| !s.is_empty()) {
                content.push_str(&c);
                on_delta(Some(c), None);
            }

            // 工具调用分片：按 index 归并
            if let Some(calls) = d.tool_calls {
                for tc in calls {
                    while partial.len() <= tc.index {
                        partial.push((String::new(), String::new(), String::new()));
                    }
                    let slot = &mut partial[tc.index];
                    if let Some(id) = tc.id {
                        if !id.is_empty() {
                            slot.0 = id;
                        }
                    }
                    if let Some(f) = tc.function {
                        if let Some(n) = f.name {
                            if !n.is_empty() {
                                slot.1.push_str(&n);
                            }
                        }
                        if let Some(a) = f.arguments {
                            slot.2.push_str(&a);
                        }
                    }
                }
            }
        }
    }

    let tool_calls: Vec<ToolCall> = partial
        .into_iter()
        .filter(|(_, name, _)| !name.is_empty())
        .enumerate()
        .map(|(i, (id, name, arguments))| ToolCall {
            id: if id.is_empty() {
                format!("call_{i}")
            } else {
                id
            },
            kind: "function".into(),
            function: FunctionCall { name, arguments },
        })
        .collect();

    let has_calls = !tool_calls.is_empty();
    Ok(ChatOutcome {
        message: ChatMessage {
            role: "assistant".into(),
            content: if content.is_empty() {
                None
            } else {
                Some(MessageContent::Text(content))
            },
            tool_calls: if has_calls { Some(tool_calls) } else { None },
            tool_call_id: None,
        },
        finish_reason,
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
    })
}

/// 流式 chat（带限流退避重试）
pub async fn chat_stream(
    cfg: &ModelConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<Value>>,
    on_delta: &DeltaFn,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ChatOutcome, String> {
    let mut backoff = FIRST_BACKOFF;
    let mut last_err = String::new();

    for attempt in 1..=MAX_RETRIES {
        // 取消后不再重试（否则用户点了停止，退避完又发一轮）
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("已停止".into());
        }
        match chat_stream_once(cfg, messages.clone(), tools.clone(), on_delta, cancel).await {
            Ok(out) => return Ok(out),
            Err(ChatError::Retryable(status, msg)) => {
                last_err = msg;
                if attempt == MAX_RETRIES {
                    break;
                }
                eprintln!(
                    "[float-agent] {status} 限流/临时故障，{:?} 后重试（第 {attempt}/{MAX_RETRIES} 次）",
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            Err(ChatError::Fatal(msg)) => return Err(msg),
        }
    }

    Err(format!(
        "模型接口持续限流或无响应，已退避重试 {MAX_RETRIES} 次仍未成功。\n\
         原始错误：{last_err}\n\n\
         建议：稍等十几秒再发一次，或在设置里换一个模型（当前：{}）。",
        cfg.id
    ))
}

async fn chat_once(
    cfg: &ModelConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<Value>>,
) -> Result<ChatOutcome, ChatError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| ChatError::Fatal(format!("创建 HTTP 客户端失败: {e}")))?;

    let body = ChatRequest {
        model: cfg.id.clone(),
        messages,
        tools,
        tool_choice: Some("auto".into()),
        stream: false,
    };

    let endpoint = cfg.chat_endpoint();

    let mut req = client
        .post(&endpoint)
        .bearer_auth(&cfg.api_key)
        // 自定义头（含按需自动补的 x-opencode-session 等）
        .json(&body);
    for (k, v) in cfg.effective_headers() {
        req = req.header(k, v);
    }

    let resp = req
        .send()
        .await
        // 注意：错误里只带 endpoint，不带 key
        .map_err(|e| ChatError::Fatal(format!("请求 {endpoint} 失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        let brief: String = text.chars().take(400).collect();
        let msg = format!("HTTP {status}（{endpoint}）：{brief}");
        return Err(if is_retryable(code) {
            ChatError::Retryable(code, msg)
        } else {
            ChatError::Fatal(msg)
        });
    }

    let parsed: ChatResponse = resp
        .json()
        .await
        .map_err(|e| ChatError::Fatal(format!("解析响应失败: {e}")))?;

    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ChatError::Fatal("响应里没有 choices".to_string()))?;

    Ok(ChatOutcome {
        message: choice.message,
        finish_reason: choice.finish_reason,
        reasoning: None,
    })
}

/// 连通性自检：发一条最短的问候，确认 key / url / 模型名都对
pub async fn ping(cfg: &ModelConfig) -> Result<String, String> {
    let msgs = vec![ChatMessage::user("ping")];
    let out = chat(cfg, msgs, None).await?;
    let t = out.text();
    if t.is_empty() {
        Ok("(空响应，但连接正常)".into())
    } else {
        Ok(t)
    }
}

/// 把工具定义转成 OpenAI 的 tools 数组格式
pub fn tools_to_openai(specs: &[crate::tools::ToolSpec]) -> Vec<Value> {
    specs
        .iter()
        .map(|s| {
            json!({
                "type": "function",
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.parameters,
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 取消标志置位后，wait_cancel 应在 ~100ms 内返回（轮询间隔）
    #[tokio::test]
    async fn wait_cancel_returns_after_flag_set() {
        let c = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c2.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let t0 = std::time::Instant::now();
        wait_cancel(&c).await;
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "取消响应过慢: {:?}",
            t0.elapsed()
        );
    }

    /// 已取消时 select! 必须立刻选取消分支，而不是等网络请求 ——
    /// 这是「点停止没反应」的核心语义
    #[tokio::test]
    async fn select_prefers_cancel_over_slow_request() {
        let c = std::sync::atomic::AtomicBool::new(true);
        let picked = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => "request",
            _ = wait_cancel(&c) => "cancelled",
        };
        assert_eq!(picked, "cancelled");
    }

    #[test]
    fn plain_text_serializes_as_string() {
        let m = ChatMessage::user("hi");
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"content\":\"hi\""), "纯文本应序列化成字符串: {j}");
    }

    #[test]
    fn multimodal_serializes_as_parts_array() {
        let m = ChatMessage::user_with_images("这是什么", vec!["data:image/png;base64,AAA".into()]);
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"type\":\"text\""), "{j}");
        assert!(j.contains("\"type\":\"image_url\""), "{j}");
        assert!(j.contains("data:image/png;base64,AAA"), "{j}");
    }

    #[test]
    fn multimodal_as_plain_is_readable() {
        let m = ChatMessage::user_with_images("看看这个", vec!["data:image/png;base64,AAA".into()]);
        let t = m.content.as_ref().unwrap().as_plain();
        assert!(t.contains("看看这个"));
        assert!(t.contains("[图片]"));
    }

    #[test]
    fn tool_result_serializes_without_null_tool_calls() {
        let t = ChatMessage::tool_result("c1", "ok");
        let j = serde_json::to_string(&t).unwrap();
        // 关键：tool_calls 字段必须被跳过，否则部分厂商 API 会报错
        assert!(!j.contains("tool_calls"), "不该出现 null 的 tool_calls: {j}");
        assert!(j.contains("\"tool_call_id\":\"c1\""));
    }

    #[test]
    fn retryable_classification() {
        // 限流与服务端临时故障 → 值得重试
        assert!(is_retryable(429), "429 限流必须重试");
        assert!(is_retryable(500));
        assert!(is_retryable(502));
        assert!(is_retryable(503));

        // 认证/请求本身的问题 → 重试没有意义
        assert!(!is_retryable(400));
        assert!(!is_retryable(401), "key 不对，重试也没用");
        assert!(!is_retryable(403));
        assert!(!is_retryable(404), "模型名写错，重试也没用");
    }

    #[test]
    fn parses_tool_call_response() {        let raw = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"D:\\\\a.txt\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let out = ChatOutcome {
            message: parsed.choices[0].message.clone(),
            finish_reason: parsed.choices[0].finish_reason.clone(),
            reasoning: None,
        };
        assert_eq!(out.tool_calls().len(), 1);
        assert_eq!(out.tool_calls()[0].function.name, "read_file");
        assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    }
}
