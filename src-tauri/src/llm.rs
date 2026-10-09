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

/// 状态文案用的模型标签：就是接口 model id（显示名已废弃，请求发的就是这个 id）。
pub fn model_status_label(cfg: &ModelConfig) -> String {
    cfg.id.clone()
}

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
// token 用量
// ---------------------------------------------------------------------------

/// 一次模型调用的 token 用量（OpenAI 兼容 `usage` 字段的归一化结果）。
///
/// 各厂商字段名不统一：多数用 `prompt_tokens` / `completion_tokens`，
/// 少数网关用 `input_tokens` / `output_tokens`；思维链 token 放在
/// `completion_tokens_details.reasoning_tokens`（也有放 prompt 侧的）。
/// 这里全部收进一个统一结构，上层只认这个。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 输入（prompt）token
    #[serde(default)]
    pub prompt: u32,
    /// 输出（completion）token
    #[serde(default)]
    pub completion: u32,
    /// 思维链 token（部分模型单独给；不给就是 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u32>,
    /// 缓存命中（读缓存）token —— DeepSeek `prompt_cache_hit_tokens` /
    /// OpenAI `prompt_tokens_details.cached_tokens` / Claude `cache_read_input_tokens`
    #[serde(default)]
    pub cache_hit: u32,
    /// 缓存写入 token —— Claude `cache_creation_input_tokens` 等
    #[serde(default)]
    pub cache_write: u32,
    /// 总计。厂商没给时按 prompt + completion 补
    #[serde(default)]
    pub total: u32,
}

impl TokenUsage {
    /// 全为 0 = 没拿到用量（不算一次有效记账）
    pub fn is_zero(&self) -> bool {
        self.prompt == 0 && self.completion == 0 && self.total == 0
    }

    /// 累加（多轮 agent loop 求和）
    pub fn add(&mut self, o: &TokenUsage) {
        self.prompt += o.prompt;
        self.completion += o.completion;
        self.total += o.total;
        self.cache_hit += o.cache_hit;
        self.cache_write += o.cache_write;
        if let Some(r) = o.reasoning {
            self.reasoning = Some(self.reasoning.unwrap_or(0) + r);
        }
    }
}

/// `usage` 原始结构（兼容 prompt/completion 与 input/output 两套命名）
#[derive(Debug, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    /// DeepSeek 风格顶层缓存命中
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    /// Claude 风格顶层缓存读/写
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<RawUsageDetails>,
    #[serde(default)]
    completion_tokens_details: Option<RawUsageDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct RawUsageDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
    /// OpenAI 风格缓存命中
    #[serde(default)]
    cached_tokens: Option<u32>,
    /// 另一些网关的缓存写入别名
    #[serde(default)]
    cache_write_tokens: Option<u32>,
    #[serde(default)]
    cached_creation_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_tokens: Option<u32>,
}

impl RawUsage {
    /// 归一化成 [`TokenUsage`]；一个数字都没有 → None
    fn to_usage(&self) -> Option<TokenUsage> {
        let prompt = self.prompt_tokens.or(self.input_tokens).unwrap_or(0);
        let completion = self.completion_tokens.or(self.output_tokens).unwrap_or(0);
        let total = self.total_tokens.unwrap_or(prompt + completion);
        if prompt == 0 && completion == 0 && total == 0 {
            return None;
        }
        let reasoning = self
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .or_else(|| {
                self.prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.reasoning_tokens)
            });
        let ptd = self.prompt_tokens_details.as_ref();
        let cache_hit = self
            .prompt_cache_hit_tokens
            .or(self.cache_read_input_tokens)
            .or_else(|| ptd.and_then(|d| d.cached_tokens))
            .unwrap_or(0);
        let cache_write = self
            .cache_creation_input_tokens
            .or_else(|| ptd.and_then(|d| d.cache_write_tokens))
            .or_else(|| ptd.and_then(|d| d.cached_creation_tokens))
            .or_else(|| ptd.and_then(|d| d.cache_creation_tokens))
            .unwrap_or(0);
        Some(TokenUsage {
            prompt,
            completion,
            reasoning,
            cache_hit,
            cache_write,
            total,
        })
    }
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
    /// 流式下要求服务端在最后一片带回 `usage`（OpenAI `stream_options.include_usage`）。
    /// 不设这个，流式响应里**不会**有 usage，用量就记不到账。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<Value>,
    /// 生成上限（模型配置 `maxOutputTokens`）。
    ///
    /// 为什么必须上行（2026-09-25 用户报「mimo 老是提前停止」后补）：
    /// 这个字段以前只是配置里的摆设 —— 从没进过请求体，服务端就用它自己的
    /// 默认值（很多网关只有 4k）。一旦回答长一点，就被 `finish_reason=length`
    /// 掐断在半截，agent 却把半截当成答完 → 表现就是"提前停止"。
    /// 未配置则不发（`skip_serializing_if`），走服务端默认。
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// 一次调用的结果
///
/// ⚠️ 2026-09-30 删掉了 `reasoning: Option<String>` 字段。
/// 原因不是"没人用"，而是**它有第二份来源且那份才是真的**：
/// 思维链在流式路径上由 `on_delta` 回调**逐块累加**，agent loop 拿它拼
/// `round_reason`（再经 `absorb_reason` 进步骤时间线）。这里再存一份，
/// 只会让读代码的人以为"agent 用这个字段"，从而在排查思维链丢失时看错地方。
/// `chat()`（非流式）以前会返回它，但 agent 只走流式路径。
///
/// 流式内部的局部 `reasoning` 变量仍在用（见"流提前中断"错误信息里的字数统计），
/// 只是不再往结构体里塞。
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
    /// 本次调用的 token 用量（服务端没给就是 None）
    pub usage: Option<TokenUsage>,
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
            Err(ChatError::Retryable(reason, msg)) => {
                last_err = msg;
                if attempt == MAX_RETRIES {
                    break;
                }
                eprintln!(
                    "[orbcat] {reason} 临时故障，{:?} 后重试（第 {attempt}/{MAX_RETRIES} 次）",
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
#[derive(Debug)]
enum ChatError {
    /// (原因标签, 原始信息) —— 标签进状态文案，如 "HTTP 429" / "流中断"
    ///
    /// 流式读到一半 `error decoding response body` 这类**传输层**故障也走这里：
    /// 它既不是 429，也不是请求写错，整轮直接判死太粗暴（2026-09-24 用户实撞）。
    Retryable(String, String),
    Fatal(String),
}

// ---------------------------------------------------------------------------
// 流式（SSE）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// 最后一片（choices 为空）会带 usage，前提是请求设了
    /// `stream_options.include_usage`。所以**必须在取 choice 之前**读它。
    #[serde(default)]
    usage: Option<RawUsage>,
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
    /// 思维链。**各家字段名不统一**（2026-09-20 实测）：
    /// - `reasoning_content` —— spark-x2.5-4b、DeepSeek 系
    /// - `reasoning`         —— sensenova-6.8-flash-lite
    /// - `thinking`          —— 部分 Claude 兼容网关
    ///
    /// 只认一个的后果很具体：换个模型就"看不到思考过程"，而且**不报错**，
    /// 看起来像功能坏了。所以三个都收，谁有内容用谁。
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

impl Delta {
    /// 取这一片的思维链文本（兼容三种字段名）
    fn reasoning_piece(&self) -> Option<&str> {
        [
            self.reasoning_content.as_deref(),
            self.reasoning.as_deref(),
            self.thinking.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
    }
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
    /// 工具名（流式分片里逐段给全名；归并逻辑以 index 分组、首片取名）
    #[serde(default)]
    name: Option<String>,
    /// 工具参数分片。**形态不统一**（2026-09-25）：
    /// 绝大多数网关按 OpenAI 规范给**字符串**分片（多片拼接成 JSON 串）；
    /// 个别实现直接给**整个 JSON 对象**（一次给完）。
    /// 只收 String 的后果：`serde_json::from_str::<StreamChunk>` 反序列化失败，
    /// 整个 chunk 被静默 `continue` —— 工具调用凭空消失，模型"调了个寂寞"，
    /// agent 拿到空 tool_calls 就当答完收工。所以对象形态也要收。
    #[serde(default)]
    arguments: Option<Value>,
}

/// 流式增量回调：`(正文增量, 思维链增量)`
pub type DeltaFn = dyn Fn(Option<String>, Option<String>) + Send + Sync;

/// 流式重试前调用：上一尝试已推送的半截正文应作废（reason 给前端归档标签）。
///
/// 为什么要：断流重试会从头发第二遍，不作废就会把两截拼成重复/错乱正文。
/// 与工具调用的 `DiscardStream` 同语义 —— **只作废正文**，reasoning 保留。
pub type DiscardFn = dyn Fn(String) + Send + Sync;

/// 请求链路状态回调 `(文案, 是否值得留在时间线上)`。
/// 用来区分「模型在思考」和「HTTP 还没首字节」—— 后者以前是纯黑盒。
///
/// 第二个参数 `retry`：**只有**限流退避重试这类状态才该进对话时间线
/// （用户撞 429 时最想知道"重试了几次、为什么"）。「正在请求 / 等首包」
/// 是每个请求都会有的过场，只配刷在标题上，不该把时间线刷满。
pub type StatusFn = dyn Fn(String, bool) + Send + Sync;

/// 配额/余额类错误：状态码是 429，但**重试没有意义**。
///
/// 典型：火山方舟 5 小时用量配额耗尽（body 里 `"code":"AccountQuotaExceeded"`，
/// 原文还带恢复时间）；OpenAI 的 `insufficient_quota` 同理。
///
/// 为什么要单独识别（2026-09-22 用户实际撞上）：`is_retryable` 把所有 429 一律
/// 当"临时限流"退避重试，用户于是白等四轮退避，还以为只是被限流了。
fn is_quota_exhausted(status: u16, body: &str) -> bool {
    if status != 429 && status != 402 && status != 403 {
        return false;
    }
    let b = body.to_ascii_lowercase();
    b.contains("accountquotaexceeded")
        || b.contains("insufficient_quota")
        || b.contains("quota exceeded")
        || b.contains("exceeded your current quota")
        || b.contains("insufficient balance")
        || b.contains("balance_insufficient")
}

/// HTTP 非 2xx 的三路分类。
///
/// - **配额/余额耗尽** → Fatal：不是临时故障，重试只会白等
/// - 其余 429 / 5xx → Retryable：退避重试基本能撑过去
/// - 401/403/400/404… → Fatal：key 不对 / 模型名写错，重试也没用
fn classify_http_error(code: u16, body: &str, msg: String) -> ChatError {
    if is_quota_exhausted(code, body) {
        return ChatError::Fatal(format!(
            "{msg}\n\n（这是**配额/余额已耗尽**，不是临时限流 —— 重试没有意义。\
             服务端原文里有恢复时间或额度说明；可以照它等，或在设置里换一个还有\
             额度的模型重发。）"
        ));
    }
    if is_retryable(code) {
        ChatError::Retryable(format!("HTTP {code}"), msg)
    } else {
        ChatError::Fatal(msg)
    }
}

/// 把 reqwest 错误链摊平：底层原因（DNS/超时/代理/TLS）比顶层文案有用得多。
fn reqwest_err_chain(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    parts.join(" ← ")
}

/// 流式 chunk 读失败 → **可重试**（不是 429，也不是请求写错）。
///
/// 典型：`error decoding response body`（代理/网关掐流、解压坏包）、连接被复位。
/// 以前包成 Fatal，用户表现为「长回复经常一有就红、从不重试」。
fn stream_read_fail(err_chain: String) -> ChatError {
    ChatError::Retryable("流中断".into(), format!("读取流失败: {err_chain}"))
}

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
    on_status: &StatusFn,
    cancel: &std::sync::atomic::AtomicBool,
    // 本次尝试是否已推送正文增量（重试前决定要不要 `on_discard`）
    emitted: &std::sync::atomic::AtomicBool,
) -> Result<ChatOutcome, ChatError> {
    use futures_util::StreamExt;
    use std::sync::atomic::Ordering;

    // connect 超时单独收紧：连不上应在十几秒内报错，而不是干等总超时。
    //
    // ⚠️ 总超时从 180s 放宽到 600s（2026-09-25）：reqwest 的 `timeout` 覆盖
    // **整个请求含流式读取**。mimo 这类"长思考 + 长输出"的模型，一次调用
    // 跑三四分钟很常见 —— 180s 会把流拦腰掐断，看起来就像"提前停止/反复重试"。
    // 打断长流靠用户点「停止」（cancel 每个 chunk 都检查），不靠总超时。
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| ChatError::Fatal(format!("创建 HTTP 客户端失败: {e}")))?;

    let body = ChatRequest {
        model: cfg.id.clone(),
        messages,
        tools,
        tool_choice: Some("auto".into()),
        stream: true,
        // 要求末片带 usage —— 否则流式拿不到 token 用量，账记不了
        stream_options: Some(json!({ "include_usage": true })),
        max_tokens: cfg.max_output_tokens,
    };

    let endpoint = cfg.chat_endpoint();
    on_status(
        format!(
            "正在请求 {} …（已发出，等服务端响应）",
            model_status_label(cfg)
        ),
        false,
    );

    let mut req = client
        .post(&endpoint)
        .bearer_auth(&cfg.api_key)
        // 自定义头（含按需自动补的 x-opencode-session 等）
        .json(&body);
    for (k, v) in cfg.effective_headers() {
        req = req.header(k, v);
    }

    let resp = tokio::select! {
        r = req.send() => r.map_err(|e| {
            ChatError::Fatal(format!(
                "请求 {endpoint} 失败: {}\n\
                 （常见原因：网络/代理不通、DNS、TLS；若只有主对话失败、短消息正常，\
                 多半是请求体过大——部分网关对约 32KB+ 的 body 会挂住直到超时。\
                 可新开会话或精简人格/规则文件后重试）",
                reqwest_err_chain(&e)
            ))
        })?,
        // 等待响应（首字节延迟）也可被取消 —— 否则用户点了停止还要干等
        _ = wait_cancel(cancel) => return Err(ChatError::Fatal("已停止".into())),
    };

    on_status(
        format!(
            "已连上 {}，等待模型输出首包…（轻量模型+长提示词可能要十几秒）",
            model_status_label(cfg)
        ),
        false,
    );

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        let brief: String = text.chars().take(400).collect();
        let msg = format!("HTTP {status}（{endpoint}）：{brief}");
        return Err(classify_http_error(code, &text, msg));
    }

    // ---- 逐块解析 SSE ----
    let mut stream = resp.bytes_stream();
    let mut buf = String::new(); // 半行缓冲：chunk 边界可能切断一行
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason: Option<String> = None;
    // 是否收到过流终止标记（[DONE]）—— 用来区分"正常结束"与"被掐断"
    let mut saw_done = false;
    // 流式用量：只在**末片**出现（那一片的 choices 是空的）
    let mut usage: Option<TokenUsage> = None;
    // index → (id, name, arguments)
    let mut partial: Vec<(String, String, String)> = Vec::new();

    while let Some(chunk) = stream.next().await {
        // 取消检查：做到「流粒度」——用户点停止后下一个 chunk（几十毫秒）就退出，
        // 不必等整轮模型输出完。这是「点停止没反应」的关键修复点。
        if cancel.load(Ordering::Relaxed) {
            return Err(ChatError::Fatal("已停止".into()));
        }
        let bytes = chunk.map_err(|e| stream_read_fail(reqwest_err_chain(&e)))?;
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
                saw_done = true;
                break;
            }

            let Ok(ch) = serde_json::from_str::<StreamChunk>(payload) else {
                // 以前这里静默 continue —— 服务端换了字段形态时工具调用会"无声消失"。
                // 打一行日志留证据；注释行（`: ping` 之类）很常见，不用每次都刷屏。
                if payload.len() > 1 && !payload.starts_with(':') {
                    eprintln!(
                        "[orbcat] SSE chunk 解析失败（忽略）：{}",
                        payload.chars().take(160).collect::<String>()
                    );
                }
                continue;
            };
            // ⚠️ 用量必须**在取 choice 之前**读 —— 末片的 choices 是空数组，
            //    一旦先走下面的 `else { continue }`，usage 就永远丢了。
            if let Some(u) = ch.usage.as_ref().and_then(RawUsage::to_usage) {
                usage = Some(u);
            }
            let Some(choice) = ch.choices.into_iter().next() else {
                continue;
            };

            if let Some(fr) = choice.finish_reason {
                finish_reason = Some(fr);
            }

            let d = choice.delta;

            // 思维链增量（字段名见 `Delta` 的说明）
            if let Some(r) = d.reasoning_piece() {
                reasoning.push_str(r);
                on_delta(None, Some(r.to_string()));
            }

            // 正文增量
            if let Some(c) = d.content.filter(|s| !s.is_empty()) {
                content.push_str(&c);
                emitted.store(true, Ordering::Relaxed);
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
                        // 参数可能是字符串分片（主流）或一次性整对象（个别网关）
                        match f.arguments {
                            None | Some(Value::Null) => {}
                            Some(Value::String(s)) => slot.2.push_str(&s),
                            Some(other) => slot.2.push_str(&other.to_string()),
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

    // ---- 流"干净地"结束了 ≠ 模型答完了 ----
    //
    // 2026-09-25 用户报「mimo 老是提前停止」后的核心修复：
    // 网络/代理/网关掐流时，SSE 连接会**提前正常关闭** —— 既没有 [DONE]，
    // 也没有 finish_reason，但 `while let Some(chunk)` 照样"正常"退出。
    // 旧代码把这种情况当成完整回答返回，agent 拿到半截正文就收工 → 提前停止。
    //
    // 判据：终止标记 `[DONE]` 和 `finish_reason` **一个都没有**才判截断。
    // （有些网关不发 [DONE] 但会发 finish_reason，反之亦然 —— 只要有其一就算完整。）
    if !saw_done && finish_reason.is_none() {
        return Err(ChatError::Retryable(
            "流提前中断".into(),
            format!(
                "SSE 流在没有 [DONE]/finish_reason 的情况下提前关闭（多半是网络/代理/网关掐断）。\
                 已收正文 {} 字、思维链 {} 字、工具调用 {} 个 —— 这些是半截数据，作废后自动重试。",
                content.chars().count(),
                reasoning.chars().count(),
                tool_calls.len()
            ),
        ));
    }

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
        usage,
    })
}

/// 流式 chat（带限流退避重试）
///
/// `on_discard`：重试前若上一尝试已吐过正文，先作废那半截，避免与新一发拼接。
pub async fn chat_stream(
    cfg: &ModelConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<Value>>,
    on_delta: &DeltaFn,
    on_status: &StatusFn,
    on_discard: &DiscardFn,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ChatOutcome, String> {
    let mut backoff = FIRST_BACKOFF;
    let mut last_err = String::new();
    // 本次尝试是否已推送正文增量 —— 决定重试前要不要 on_discard
    let emitted = std::sync::atomic::AtomicBool::new(false);

    for attempt in 1..=MAX_RETRIES {
        // 取消后不再重试（否则用户点了停止，退避完又发一轮）
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("已停止".into());
        }
        emitted.store(false, std::sync::atomic::Ordering::Relaxed);
        match chat_stream_once(
            cfg,
            messages.clone(),
            tools.clone(),
            on_delta,
            on_status,
            cancel,
            &emitted,
        )
        .await
        {
            Ok(out) => return Ok(out),
            Err(ChatError::Retryable(reason, msg)) => {
                last_err = msg;
                if attempt == MAX_RETRIES {
                    break;
                }
                // 上一发已经吐了半截正文 → 作废，否则重试会拼接出重复内容
                if emitted.load(std::sync::atomic::Ordering::Relaxed) {
                    on_discard(format!("{reason}，已丢弃半截输出后重试"));
                }
                let wait = format!("{:?}", backoff);
                eprintln!(
                    "[orbcat] {reason} 临时故障，{wait} 后重试（第 {attempt}/{MAX_RETRIES} 次）"
                );
                // 静默退避是「一直思考、界面无动静」的主因之一 —— 必须告诉用户在等什么
                // 这个**值得**进时间线：撞限流/断流时用户最想看"它在等什么、重试到第几次"
                on_status(
                    format!("{reason}，{wait} 后自动重试（第 {attempt}/{MAX_RETRIES} 次）…"),
                    true,
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
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| ChatError::Fatal(format!("创建 HTTP 客户端失败: {e}")))?;

    let body = ChatRequest {
        model: cfg.id.clone(),
        messages,
        tools,
        tool_choice: Some("auto".into()),
        stream: false,
        stream_options: None,
        max_tokens: cfg.max_output_tokens,
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
        .map_err(|e| {
            ChatError::Fatal(format!(
                "请求 {endpoint} 失败: {}\n（常见原因：网络/代理不通、DNS、TLS、或服务端拒绝连接）",
                reqwest_err_chain(&e)
            ))
        })?;

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        let brief: String = text.chars().take(400).collect();
        let msg = format!("HTTP {status}（{endpoint}）：{brief}");
        return Err(classify_http_error(code, &text, msg));
    }

    let parsed: ChatResponse = resp
        .json()
        .await
        .map_err(|e| ChatError::Fatal(format!("解析响应失败: {e}")))?;

    // 用量要在 move choices 之前取
    let usage = parsed.usage.as_ref().and_then(RawUsage::to_usage);

    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ChatError::Fatal("响应里没有 choices".to_string()))?;

    Ok(ChatOutcome {
        message: choice.message,
        finish_reason: choice.finish_reason,
        usage,
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

// ---------------------------------------------------------------------------
// 远端模型列表（GET /models）
// ---------------------------------------------------------------------------

/// 远端 `/models` 列表里的一项（只给前端展示，不含任何密钥）
///
/// `context_length` / `max_output_length` 是**尽力而为**的可选字段：
/// 部分网关（如 SenseNova）会在 `/models` 里返回它们，导入模型时就能
/// 自动填上 `maxInputTokens`，省得用户去翻文档手填。
/// 不返回的网关（如火山方舟 `/models` 直接 404）只能手填 —— 这里保持 `None`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteModelInfo {
    pub id: String,
    #[serde(default)]
    pub owned_by: String,
    /// 上下文窗口（token）。缺失 = 该网关没提供，需手填
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// 最大输出（token）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_length: Option<u64>,
}

/// 从对象里按多个候选键名取一个正整数。
///
/// 各家网关字段名不统一（`context_length` / `context_window` /
/// `max_input_tokens` / `max_context_length` 都见过），逐个试。
/// 接受数字或数字字符串（部分网关给的是 `"1048576"`）。
fn pick_u64(obj: &Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        let Some(v) = obj.get(*k) else { continue };
        if let Some(n) = v.as_u64() {
            if n > 0 {
                return Some(n);
            }
        }
        if let Some(s) = v.as_str() {
            if let Ok(n) = s.trim().parse::<u64>() {
                if n > 0 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// 解析 OpenAI 兼容的 `GET /models` 响应体。
///
/// 兼容常见形态：
///   - 标准：`{object:"list", data:[{id, owned_by}]}`
///   - 部分网关把数组放在 `models` 字段
///   - 裸数组：`["id1", ...]` 或 `[{id}, ...]`
///   - id 字段名可能是 `id` / `name` / `model`
pub fn parse_models_payload(v: &Value) -> Result<Vec<RemoteModelInfo>, String> {
    let arr = if let Some(d) = v.get("data").and_then(|x| x.as_array()) {
        d.as_slice()
    } else if let Some(d) = v.get("models").and_then(|x| x.as_array()) {
        d.as_slice()
    } else if let Some(d) = v.as_array() {
        d.as_slice()
    } else {
        return Err("响应里找不到模型数组（期望 data / models / 顶层数组）".into());
    };

    let mut out = Vec::new();
    for item in arr {
        let info = match item {
            Value::String(s) => {
                let id = s.trim().to_string();
                if id.is_empty() {
                    continue;
                }
                RemoteModelInfo {
                    id,
                    owned_by: String::new(),
                    context_length: None,
                    max_output_length: None,
                }
            }
            Value::Object(_) => {
                let id = item
                    .get("id")
                    .or_else(|| item.get("name"))
                    .or_else(|| item.get("model"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let owned_by = item
                    .get("owned_by")
                    .or_else(|| item.get("ownedBy"))
                    .or_else(|| item.get("vendor"))
                    .or_else(|| item.get("organization"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                // 上下文窗口：各家字段名不一，尽力而为
                let context_length = pick_u64(
                    item,
                    &[
                        "context_length",
                        "contextLength",
                        "context_window",
                        "contextWindow",
                        "max_context_length",
                        "max_input_tokens",
                        "maxInputTokens",
                        "max_tokens",
                    ],
                );
                let max_output_length = pick_u64(
                    item,
                    &[
                        "max_output_length",
                        "maxOutputLength",
                        "max_output_tokens",
                        "maxOutputTokens",
                        "output_length",
                    ],
                );
                RemoteModelInfo {
                    id,
                    owned_by,
                    context_length,
                    max_output_length,
                }
            }
            _ => continue,
        };
        out.push(info);
    }

    if out.is_empty() {
        return Err("接口返回了空模型列表".into());
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

/// 用显式 url / key / 额外头拉取远端模型列表。
pub async fn fetch_models_list(
    url: &str,
    api_key: &str,
    headers: &[(String, String)],
) -> Result<Vec<RemoteModelInfo>, String> {
    let endpoint = crate::config::models_endpoint_from_base(url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let mut req = client.get(&endpoint);
    let key = api_key.trim();
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }

    let resp = req.send().await.map_err(|e| {
        format!(
            "请求 {endpoint} 失败: {}\n（常见原因：网络/代理不通、DNS、TLS、或服务端拒绝连接）",
            reqwest_err_chain(&e)
        )
    })?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let brief: String = text.chars().take(400).collect();
        return Err(format!("HTTP {status}（{endpoint}）：{brief}"));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析响应失败: {e}"))?;
    parse_models_payload(&body)
}

/// 用**已配置模型**的 url / key / headers 拉列表（设置页里省得再手填 Key）。
pub async fn fetch_models_from_cfg(cfg: &ModelConfig) -> Result<Vec<RemoteModelInfo>, String> {
    fetch_models_list(&cfg.url, &cfg.api_key, &cfg.effective_headers()).await
}

/// 估算一段文本的 token 数（CJK 与 ASCII 分别计权）。
///
/// 为什么手写而不是引 tiktoken：① 引一个带 BPE 词表的 crate 体积很大，
/// 与项目"零额外依赖"取向冲突；② agent loop 与 compact 只需要**量级正确**
/// 来判断"是否接近预算"，不需要精确计费（真实用量以服务端返回的 usage 为准）。
///
/// 经验公式：
/// - CJK 字符 ≈ 1 token/字（UTF-8 3 字节，tokenizer 通常 1 字 1~2 token，取下界）
/// - 其他字符（ASCII/代码/标点）≈ 1 token / 4 字符
/// - `div_ceil` 保证纯 ASCII 短文本也不会算成 0
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        // CJK 统一表意文字 + 常用中日韩标点/假名/谚文
        if matches!(c as u32,
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF |
            0x3000..=0x303F | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
        {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
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

    /// 思维链字段名兼容（2026-09-20 实测）：
    /// spark-x2.5-4b 用 `reasoning_content`，sensenova-6.8-flash-lite 用 `reasoning`。
    /// 只认前者时，换成 sensenova 就"看不到思考过程"且**不报错** —— 锁一条测试防回归。
    #[test]
    fn delta_accepts_three_reasoning_field_names() {
        let real_sensenova = r#"{"reasoning":"这是一个","content":""}"#;
        let d: Delta = serde_json::from_str(real_sensenova).unwrap();
        assert_eq!(d.reasoning_piece(), Some("这是一个"), "sensenova 用 reasoning");

        let real_spark = r#"{"content":"","reasoning_content":null,"role":"assistant"}"#;
        let d: Delta = serde_json::from_str(real_spark).unwrap();
        assert_eq!(d.reasoning_piece(), None, "null / 空串不算内容");

        let spark2 = r#"{"reasoning_content":"我们需要"}"#;
        let d: Delta = serde_json::from_str(spark2).unwrap();
        assert_eq!(d.reasoning_piece(), Some("我们需要"), "spark 用 reasoning_content");

        let thinking = r#"{"thinking":"hmm"}"#;
        let d: Delta = serde_json::from_str(thinking).unwrap();
        assert_eq!(d.reasoning_piece(), Some("hmm"), "部分网关用 thinking");

        let plain = r#"{"content":"你好"}"#;
        let d: Delta = serde_json::from_str(plain).unwrap();
        assert_eq!(d.reasoning_piece(), None, "没有思维链字段时不该编造");
    }

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
    fn quota_errors_fail_fast_instead_of_retrying() {
        // 火山方舟 5 小时配额耗尽：状态码 429，但重试注定失败
        let fangzhou = r#"{"error":{"code":"AccountQuotaExceeded",
            "message":"You have exceeded the 5-hour usage quota. It will reset at 2026-09-22 15:02:08 +0800 CST"}}"#;
        assert!(is_quota_exhausted(429, fangzhou), "方舟配额耗尽要认出来");
        assert!(!is_retryable(429) || is_quota_exhausted(429, fangzhou));
        match classify_http_error(429, fangzhou, "HTTP 429".into()) {
            ChatError::Fatal(m) => assert!(m.contains("配额/余额已耗尽"), "要说清重试没用: {m}"),
            other => panic!("配额类 429 必须是 Fatal，实际 {other:?}"),
        }

        // OpenAI 的 insufficient_quota
        assert!(is_quota_exhausted(429, r#"{"error":{"code":"insufficient_quota"}}"#));
        assert!(is_quota_exhausted(402, "insufficient balance"));

        // 真正的临时限流 → 仍然重试
        let transient = r#"{"error":{"message":"Rate limit reached for requests"}}"#;
        assert!(!is_quota_exhausted(429, transient), "限流不等于配额耗尽");
        assert!(matches!(
            classify_http_error(429, transient, "HTTP 429".into()),
            ChatError::Retryable(reason, _) if reason.contains("429")
        ));

        // 别误判：200 里的 "quota exceeded" 字样不该被当错误分类（状态码先卡住）
        assert!(!is_quota_exhausted(200, "quota exceeded"));
    }

    #[test]
    fn stream_decode_error_is_retryable_not_fatal() {
        // 用户实撞：火山 glm 流式读到一半 `error decoding response body`
        // 以前包成 Fatal → 整轮直接红、从不重试
        let e = stream_read_fail("error decoding response body".into());
        match e {
            ChatError::Retryable(reason, msg) => {
                assert!(reason.contains("流中断"), "标签要能看出是断流: {reason}");
                assert!(msg.contains("读取流失败"), "原始错误要保留: {msg}");
                assert!(msg.contains("error decoding response body"));
            }
            other => panic!("流读失败必须可重试，实际 {other:?}"),
        }

        // 对照：已停止 / 配额耗尽 仍然是 Fatal，不能被误重试
        assert!(matches!(ChatError::Fatal("已停止".into()), ChatError::Fatal(_)));
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
            usage: parsed.usage.as_ref().and_then(RawUsage::to_usage),
        };
        assert_eq!(out.tool_calls().len(), 1);
        assert_eq!(out.tool_calls()[0].function.name, "read_file");
        assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// usage 归一化 —— **字段名兼容是静默失效重灾区**：
    /// 标准 OpenAI 用 prompt/completion_tokens，部分网关用 input/output_tokens，
    /// 认错一套不会报错，只会"用量永远是 0"。
    #[test]
    fn raw_usage_normalizes_field_variants() {
        // 标准 OpenAI
        let std: RawUsage =
            serde_json::from_str(r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#)
                .unwrap();
        let u = std.to_usage().unwrap();
        assert_eq!((u.prompt, u.completion, u.total), (10, 5, 15));
        assert_eq!(u.reasoning, None);

        // 别名 + 缺 total → 用 prompt+completion 补
        let alias: RawUsage =
            serde_json::from_str(r#"{"input_tokens":7,"output_tokens":3}"#).unwrap();
        let u = alias.to_usage().unwrap();
        assert_eq!((u.prompt, u.completion, u.total), (7, 3, 10));

        // 推理 token 在 completion_tokens_details.reasoning_tokens
        let reason: RawUsage = serde_json::from_str(
            r#"{"prompt_tokens":2,"completion_tokens":9,"total_tokens":11,"completion_tokens_details":{"reasoning_tokens":4}}"#,
        )
        .unwrap();
        assert_eq!(reason.to_usage().unwrap().reasoning, Some(4));

        // 一个数字都没有 → None（不算一次有效记账）
        let empty: RawUsage = serde_json::from_str(r#"{}"#).unwrap();
        assert!(empty.to_usage().is_none());
    }

    /// 求和：多轮 agent loop 累加；reasoning 逐轮叠加
    #[test]
    fn usage_add_sums_all_fields() {
        let mut a = TokenUsage {
            prompt: 1,
            completion: 2,
            reasoning: Some(3),
            cache_hit: 4,
            cache_write: 5,
            total: 6,
        };
        a.add(&TokenUsage {
            prompt: 10,
            completion: 20,
            reasoning: Some(30),
            cache_hit: 40,
            cache_write: 50,
            total: 60,
        });
        assert_eq!(a.prompt, 11);
        assert_eq!(a.completion, 22);
        assert_eq!(a.reasoning, Some(33));
        assert_eq!(a.cache_hit, 44);
        assert_eq!(a.cache_write, 55);
        assert_eq!(a.total, 66);
    }

    #[test]
    fn raw_usage_parses_cache_fields() {
        // DeepSeek 顶层
        let ds: RawUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,"prompt_cache_hit_tokens":60}"#,
        )
        .unwrap();
        let u = ds.to_usage().unwrap();
        assert_eq!(u.cache_hit, 60);
        assert_eq!(u.cache_write, 0);

        // OpenAI details.cached_tokens
        let oai: RawUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,"prompt_tokens_details":{"cached_tokens":40}}"#,
        )
        .unwrap();
        assert_eq!(oai.to_usage().unwrap().cache_hit, 40);

        // Claude 顶层 cache_read / cache_creation
        let cl: RawUsage = serde_json::from_str(
            r#"{"input_tokens":100,"output_tokens":10,"total_tokens":110,"cache_read_input_tokens":70,"cache_creation_input_tokens":15}"#,
        )
        .unwrap();
        let cu = cl.to_usage().unwrap();
        assert_eq!(cu.cache_hit, 70);
        assert_eq!(cu.cache_write, 15);
    }

    /// 解析 OpenAI 标准 `/v1/models` 响应，并按 id 排序。
    ///
    /// ⚠️ 2026-09-30：这个函数**一直漏写 `#[test]`**，于是它躺在测试模块里
    /// 从来没被执行过（rustc 的 `dead_code` 警告把它暴露出来了）。
    /// 补上属性后它才真正开始守住"标准响应能被解析"这条不变量。
    #[test]
    fn parse_models_openai_standard() {
        let v: Value = serde_json::from_str(
            r#"{"object":"list","data":[
                {"id":"gpt-4o","object":"model","owned_by":"openai"},
                {"id":"deepseek-v4.1-flash","object":"model","owned_by":"deepseek"}
            ]}"#,
        )
        .unwrap();
        let list = parse_models_payload(&v).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "deepseek-v4.1-flash");
        assert_eq!(list[0].owned_by, "deepseek");
        assert_eq!(list[1].id, "gpt-4o");
    }

    #[test]
    fn parse_models_accepts_string_array_and_name_field() {
        // 裸字符串数组
        let v: Value = serde_json::from_str(r#"["m-b","m-a"]"#).unwrap();
        let list = parse_models_payload(&v).unwrap();
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["m-a", "m-b"]
        );

        // 个别网关用 name 而不是 id
        let v: Value = serde_json::from_str(r#"{"data":[{"name":"glm-4.6","organization":"zhipu"}]}"#)
            .unwrap();
        let list = parse_models_payload(&v).unwrap();
        assert_eq!(list[0].id, "glm-4.6");
        assert_eq!(list[0].owned_by, "zhipu");
    }

    #[test]
    fn parse_models_rejects_empty_or_unknown_shape() {
        let v: Value = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(parse_models_payload(&v).is_err());

        let v: Value = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(parse_models_payload(&v).is_err());
    }

    #[test]
    fn models_endpoint_joins_like_chat() {
        assert_eq!(
            crate::config::models_endpoint_from_base("https://api.example.com/v1/"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            crate::config::models_endpoint_from_base("http://127.0.0.1:8787/v1"),
            "http://127.0.0.1:8787/v1/models"
        );
    }

    /// 状态文案 = 接口 model id（显示名已废弃，请求发的就是这个 id）
    #[test]
    fn status_label_returns_api_model_id() {
        let cfg = ModelConfig {
            id: "deepseek-v4.1-flash".into(),
            url: "https://ark.example.com/v3".into(),
            api_key: "k".into(),
            ..Default::default()
        };
        assert_eq!(model_status_label(&cfg), "deepseek-v4.1-flash");
    }
}
