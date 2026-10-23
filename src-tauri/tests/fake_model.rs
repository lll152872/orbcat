//! 假模型服务 —— 用来做 pi 内核的端到端测试。
//!
//! ## 为什么不打真模型
//!
//! 真模型有三个问题：
//! 1. **慢** —— 一轮要几秒到几十秒，而且 prompt 有 4 万字
//! 2. **会挂** —— 额度用尽（429）、限流、网络，测出来的是环境问题不是代码问题
//! 3. **不确定** —— 模型可能不调工具、可能改调别的工具，断言写不牢
//!
//! 所以这里起一个**假的 OpenAI 兼容服务**：
//! - pi 真的连它（真 HTTP、真 SSE、真模型协议）
//! - extension / 工具桥 / 权限网关 / 落盘**全是真的**
//! - 只有"模型怎么想"这一步是写死的 → 毫秒级、确定、可断言
//!
//! ## 它怎么装成模型
//!
//! 按 OpenAI `/v1/chat/completions` 的**流式**协议回：
//!
//! 1. 第一次请求（对话里还没有工具结果）→ 回一个 `tool_calls` 增量，
//!    让 pi 去调 `orb_read_file`
//! 2. 第二次请求（对话里已有工具结果）→ 回一段最终正文
//!
//! 这样就把"模型决定调工具 → 工具执行 → 模型看结果给答复"
//! 这条完整回路真的跑一遍。
//!
//! ## 用法
//!
//! 测试里起它，然后把 pi 的 provider `baseUrl` 指到 `http://127.0.0.1:<port>/v1`。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

/// 假模型服务。
pub struct FakeModel {
    port: u16,
    /// 收到过几次 chat/completions 请求
    calls: Arc<AtomicUsize>,
    /// 第一次要模型调的工具名
    tool_name: String,
    /// 工具参数（JSON 字符串）
    tool_args: Value,
    /// 工具跑完后模型给的最终答复
    final_text: String,
    /// 工具名是否要自动加 `orb_` 前缀
    ///
    /// orbcat 的私有工具在 pi 侧是 `orb_read_file` 这种；
    /// 但 **MCP 工具**是 `mcp__<server>__<tool>`，加前缀就错了。
    prefix_orb: bool,
    /// 最后一次收到的请求体（测试用来检查 prompt 接管）
    last_req: Arc<std::sync::Mutex<Option<Value>>>,
}

impl FakeModel {
    /// 起一个假模型。
    ///
    /// - `tool_name`：第一轮让模型调的工具（**不带 `orb_` 前缀**，本函数自动加）
    /// - `tool_args`：工具参数
    /// - `final_text`：第二轮模型给的最终答复
    pub fn start(tool_name: impl Into<String>, tool_args: Value, final_text: impl Into<String>) -> std::io::Result<Arc<Self>> {
        Self::start_inner(tool_name, tool_args, final_text, true)
    }

    /// 同 [`FakeModel::start`]，但**不加 `orb_` 前缀**。
    ///
    /// MCP 工具名是 `mcp__<server>__<tool>` —— 加前缀就成
    /// `orb_mcp__...` 了，pi 里没有这个工具。
    pub fn start_raw(
        tool_name: impl Into<String>,
        tool_args: Value,
        final_text: impl Into<String>,
    ) -> std::io::Result<Arc<Self>> {
        Self::start_inner(tool_name, tool_args, final_text, false)
    }

    fn start_inner(
        tool_name: impl Into<String>,
        tool_args: Value,
        final_text: impl Into<String>,
        prefix_orb: bool,
    ) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let calls = Arc::new(AtomicUsize::new(0));

        let me = Arc::new(Self {
            port,
            calls: calls.clone(),
            tool_name: tool_name.into(),
            tool_args,
            final_text: final_text.into(),
            prefix_orb,
            last_req: Arc::new(std::sync::Mutex::new(None)),
        });

        let me2 = me.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let s = me2.clone();
                std::thread::spawn(move || {
                    let _ = s.handle(stream);
                });
            }
        });

        Ok(me)
    }

    /// 假模型的 base URL（填进 pi 的 provider config）。
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 收到过几次 chat/completions 请求。
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// 最后一次收到的请求体。
    pub fn last_request(&self) -> Option<Value> {
        self.last_req.lock().unwrap().clone()
    }

    /// 最后一次请求里的 **system 消息内容**。
    ///
    /// 用来验「prompt 接管有没有生效」—— 这是**直接证据**：
    /// 看模型到底收到了什么，而不是猜模型为什么那样答。
    pub fn last_system_prompt(&self) -> Option<String> {
        let req = self.last_request()?;
        let msgs = req.get("messages")?.as_array()?;
        let mut out = String::new();
        for m in msgs {
            if m.get("role").and_then(|x| x.as_str()) == Some("system") {
                let c = m.get("content")?;
                if let Some(s) = c.as_str() {
                    out.push_str(s);
                } else if let Some(arr) = c.as_array() {
                    for b in arr {
                        if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                            out.push_str(t);
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// 处理一个 HTTP 连接。
    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;

        // ── 读请求头 ──
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte)?;
            if n == 0 {
                return Ok(());
            }
            head.push(byte[0]);
            if head.len() >= 4 && &head[head.len() - 4..] == b"\r\n\r\n" {
                break;
            }
            if head.len() > 256 * 1024 {
                return Ok(());
            }
        }
        let head_str = String::from_utf8_lossy(&head).to_string();
        let path = head_str
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();

        let mut content_length = 0usize;
        for line in head_str.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            stream.read_exact(&mut body)?;
        }

        // ── 路由 ──
        if path.contains("/chat/completions") {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            return self.stream_chat(&mut stream, &req);
        }
        if path.contains("/models") {
            let body = json!({
                "object": "list",
                "data": [{"id": "fake-model", "object": "model"}]
            })
            .to_string();
            return write_json(&mut stream, &body);
        }
        // 其它路径：给个 200 空 JSON，别让客户端挂住
        write_json(&mut stream, "{}")
    }

    /// 按 OpenAI 流式协议回一个回复。
    fn stream_chat(&self, stream: &mut TcpStream, req: &Value) -> std::io::Result<()> {
        *self.last_req.lock().unwrap() = Some(req.clone());
        // 数一下对话里已经有多少条 tool 角色的消息 —— 有 = 工具已经跑过了
        let has_tool_result = req
            .get("messages")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
            })
            .unwrap_or(false);

        let head = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream; charset=utf-8\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: close\r\n\r\n";
        stream.write_all(head.as_bytes())?;

        let id = "chatcmpl-fake";
        let model = "fake-model";

        if !has_tool_result {
            // ── 第一轮：让模型调工具 ──
            let tool = if self.prefix_orb {
                format!("orb_{}", self.tool_name)
            } else {
                self.tool_name.clone()
            };
            let args = self.tool_args.to_string();

            // 先来一小段"思考/正文"，模拟真实模型
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "我先读一下那个文件。"}, "finish_reason": null}]
            }))?;
            // 工具调用（分两片发，模拟真实的增量拼装）
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0, "id": "call_fake_1", "type": "function",
                    "function": {"name": tool, "arguments": ""}
                }]}, "finish_reason": null}]
            }))?;
            // 参数分两半 —— 现实验证 pi 的增量拼装
            let (a1, a2) = split_json_str(&args);
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0, "function": {"arguments": a1}
                }]}, "finish_reason": null}]
            }))?;
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0, "function": {"arguments": a2}
                }]}, "finish_reason": null}]
            }))?;
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }))?;
        } else {
            // ── 第二轮：给最终答复 ──
            // 分片发正文，验证 pi 的 text_delta 累积
            let text = self.final_text.clone();
            let mid = text.len() / 2;
            let (t1, t2) = split_at_char(&text, mid);
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": t1}, "finish_reason": null}]
            }))?;
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {"content": t2}, "finish_reason": null}]
            }))?;
            emit(stream, &json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30}
            }))?;
        }

        stream.write_all(b"data: [DONE]\n\n")?;
        stream.flush()
    }
}

/// 发一条 SSE 数据。
fn emit(stream: &mut TcpStream, v: &Value) -> std::io::Result<()> {
    let line = format!("data: {}\n\n", v);
    stream.write_all(line.as_bytes())
}

/// 写一个普通 JSON 响应。
fn write_json(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// 把一个 JSON 字符串切成两半（**必须落在字符边界上**）。
fn split_json_str(s: &str) -> (String, String) {
    if s.len() < 2 {
        return (s.to_string(), String::new());
    }
    split_at_char(s, s.len() / 2)
}

/// 供测试用：一个永远回固定文本的极端简单模型（不调工具）。
pub fn simple_text_model(text: impl Into<String>) -> std::io::Result<Arc<FakeModel>> {
    FakeModel::start("__never_called__", json!({}), text)
}

/// 按**字符边界**安全地把字符串切成两半。
///
/// `str::floor_char_boundary` 是 nightly API —— 这里自己实现，
/// 避免为了一个切分动作引入 nightly 依赖。
fn split_at_char(s: &str, at: usize) -> (String, String) {
    let mut cut = at.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    (s[..cut].to_string(), s[cut..].to_string())
}
