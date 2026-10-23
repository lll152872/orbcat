//! 工具桥 —— 把 orbcat 的 34 个工具暴露给 pi 的 extension 调。
//!
//! ## 为什么需要这个
//!
//! pi 只有 **8 个内置工具**（`read`/`bash`/`edit`/`write`/`grep`/`find`/`ls`/`powershell`），
//! 而 orbcat 有 **34 个** —— 其中一半是 Windows 深度集成
//! （截屏、前台窗口、图像发送、表情包、life/pmem、记忆蒸馏、跨会话检索…），
//! 这些在 TS 里**根本拿不到**。
//!
//! 所以：工具**留在 Rust**（一行代码不重写），pi 通过 HTTP 回调来调。
//!
//! ## 为什么是 HTTP 而不是 stdio
//!
//! pi 的 stdin/stdout **专供 RPC 协议**（`docs/rpc.md:62`：stdout 专供协议，
//! 诊断走 stderr）。往里塞别的协议会破坏帧边界 —— 而 pi 的帧是严格 LF JSONL，
//! 塞多路复用逻辑进去等于自找麻烦。
//!
//! 所以另开一个 127.0.0.1 的临时端口。实测往返 **6 ms**（`tmp/probe8_rust_cb.py`）。
//!
//! ## 零新增依赖
//!
//! 用 `std::net::TcpListener` 手写最小 HTTP/1.1：
//! - 只支持 `POST /tool`（唯一需要的方法）
//! - 只服务 `127.0.0.1`（不对外暴露）
//! - 单请求单响应，不 keep-alive
//!
//! 为什么不引 `axum`/`tiny_http`：为了一个 POST 接口引一整个 Web 框架不值当，
//! 而项目本来就有「零额外依赖取向」（见 `allmemory/MEMORY.md`）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

/// 一次工具调用的请求体。
///
/// ```json
/// {"tool": "read_file", "args": {"path": "D:/x.md"}}
/// ```
#[derive(Debug, Clone, Deserialize)]
struct ToolRequest {
    /// ⚠️ **不能是必需字段** —— `/prompt` 端点的请求体没有 `tool`
    /// （只有 `{"message": "..."}`）。之前这里是必需的 → 反序列化直接失败 →
    /// 桥回 `ok:false` → extension 拿不到 prompt → **人格与记忆全丢**，
    /// 而且症状是"pi 用它自己的默认 prompt"，根因完全看不出来（2026-10-22 实测）。
    #[serde(default)]
    tool: String,
    #[serde(default)]
    args: Value,
    /// `/prompt` 端点专用：用户这一轮说的话（**顶层**字段，不是 `args` 里）。
    #[serde(default)]
    message: String,
}

/// 工具调用的处理器：由上层（`tools::execute` 那条路）注入。
///
/// 参数：`(工具名, 参数)` → `(是否出错, 结果 JSON)`。
pub type ToolHandler = Arc<dyn Fn(&str, &Value) -> Result<Value, String> + Send + Sync>;

/// 权限裁决器：`(工具名, 参数)` → `(允许?, 理由)`。
///
/// 裁决逻辑在 Rust 侧（`permission.rs` / `command_policy.rs`），
/// 这里只负责把它暴露给 pi 的 `tool_call` hook。
pub type GateHandler = Arc<dyn Fn(&str, &Value) -> Result<(bool, String), String> + Send + Sync>;

/// system prompt 提供者：`(用户这一轮说的话)` → `完整 system prompt`。
///
/// pi 的 extension 在 `before_agent_start` 时向宿主要 prompt ——
/// 这样 orbcat 的**四文件记忆**（`RULES.md`/`SOUL.md`/`IDENTITY.md`/`USER.md`
/// + `memory/MEMORY.md`）+ 项目记忆 + 记忆使用规则能被 pi 用上。
///
/// ⚠️ **必须由宿主提供**：pi 自己的默认 prompt 是「coding agent」那一套，
/// 与 orbcat 的「私人助手」定位、人格、记忆体系**完全不同**。
/// 不接这条 → pi 内核下的助手不知道你是谁、也不记得任何事。
///
/// ## 为什么是**两份**而不是一整段
///
/// pi 的 system prompt 是**分段构造**的（上游 `buildSystemPromptSections`）：
/// `preamble`（默认 coding agent 人格）+ `tools` + `rules` + `docs` +
/// `project_context` + `skills` + 各 extension 写进 `sections` 的段。
///
/// 我们要的是：
/// - `persona` → 上游的 **`customPrompt`**（替换掉 coding agent 人格 ⇒ 我们的占主导）；
/// - `context` → 上游的 **`sections["orbcat_context"]`**（追加段）；
/// - pi 原生那些段**全部保留** ⇒ 变成「工具用法 / 环境信息」的补充资料。
///
/// ⚠️ 旧的 extension（只认 `prompt` 字段）仍能工作 ⇒ `/prompt` 端点
/// **同时**返回 `prompt`（= 两份拼起来）作兼容。
pub type PromptHandler = Arc<dyn Fn(&str) -> crate::agent::PromptParts + Send + Sync>;

/// 正在运行的桥服务。
pub struct ToolBridge {
    /// 端口（0 = 没起）。
    port: u16,
    stop: Arc<AtomicBool>,
    /// 处理过的调用数（诊断用）。
    calls: Arc<AtomicU64>,
}

impl ToolBridge {
    /// 起一个桥服务，返回它和端口。
    ///
    /// 端口是**临时端口**（绑 `:0` 让 OS 挑）—— 多实例并存也不会撞。
    pub fn start(
        handler: ToolHandler,
        gate: GateHandler,
        prompt: PromptHandler,
    ) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicU64::new(0));

        let stop_c = stop.clone();
        let calls_c = calls.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop_c.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let h = handler.clone();
                let g = gate.clone();
                let pr = prompt.clone();
                let c = calls_c.clone();
                // 每个请求一个线程 —— 工具执行可能很慢（截屏、跑命令），
                // 串行会让并发工具调用排队。pi 的工具调用本来就是并行的。
                std::thread::spawn(move || {
                    let _ = handle_conn(stream, h, g, pr, c);
                });
            }
        });

        Ok(Arc::new(Self {
            port,
            stop,
            calls,
        }))
    }

    /// 端口（写进 extension 的环境变量）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 已处理的调用数。
    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// 停掉服务。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // 靠 listener 被 drop 退出循环 —— 这里只能置标志，
        // 真正的关闭靠进程退出（桥服务与 app 同生命周期）。
    }
}

impl Drop for ToolBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 处理一个 HTTP 连接：读请求 → 调工具 → 回响应。
fn handle_conn(
    mut stream: TcpStream,
    handler: ToolHandler,
    gate: GateHandler,
    prompt: PromptHandler,
    calls: Arc<AtomicU64>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(120)))?;

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
        if head.len() > 64 * 1024 {
            // 头太大 —— 不是我们的请求
            let _ = write_json(&mut stream, &json!({"ok": false, "error": "请求头过大"}));
            return Ok(());
        }
    }
    let head_str = String::from_utf8_lossy(&head).to_string();

    // Content-Length
    let mut content_length = 0usize;
    for line in head_str.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // ── 读请求体 ──
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body)?;
    }

    // ── 路由 ──
    //  /tool  → 执行工具
    //  /gate  → 权限裁决（给 orbcat-gate.ts 用）
    let path = head_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let is_gate = path.starts_with("/gate");
    let is_prompt = path.starts_with("/prompt");

    // ── 解析并执行 ──
    let req: ToolRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_json(
                &mut stream,
                &json!({"ok": false, "error": format!("请求体解析失败: {e}")}),
            );
            return Ok(());
        }
    };
    calls.fetch_add(1, Ordering::Relaxed);

    let resp = if is_prompt {
        // system prompt：宿主按「用户这一轮说的话」拼一份（含技能命中、项目上下文）
        //
        // ⚠️ `message` 在**顶层**（extension 发的是 `{"message": "..."}`），
        // 兼容 `args.message`（早期写法）。
        let user_text = if !req.message.is_empty() {
            req.message.as_str()
        } else {
            req.args
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("")
        };
        let parts = prompt(user_text);
        // ⚠️ `prompt` 字段保留（= 两份拼起来）⇒ 旧 extension 仍能用（退化成整段替换）。
        let full = format!("{}\n\n---\n\n{}", parts.persona, parts.context);
        json!({
            "ok": true,
            "prompt": full,
            "persona": parts.persona,
            "context": parts.context,
        })
    } else if is_gate {
        // 裁决：调用方给的是"允许吗"的函数
        match gate(&req.tool, &req.args) {
            Ok((allowed, reason)) => json!({"ok": true, "allowed": allowed, "reason": reason}),
            Err(e) => json!({"ok": false, "error": e}),
        }
    } else {
        match handler(&req.tool, &req.args) {
            Ok(v) => json!({"ok": true, "result": v}),
            Err(e) => json!({"ok": false, "error": e}),
        }
    };
    write_json(&mut stream, &resp)
}

/// 写一个 HTTP 响应（JSON 体）。
fn write_json(stream: &mut TcpStream, v: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// 工具清单（给 extension 用）。
///
/// pi 的 extension 需要**自己声明**每个工具的参数 schema（TypeBox）。
/// 这里把 orbcat 的 `ToolSpec` 导出成 JSON，由 extension 加载。
pub fn specs_to_json(specs: &[crate::tools::ToolSpec]) -> Value {
    let mut map = HashMap::new();
    for s in specs {
        map.insert(
            s.name.clone(),
            json!({
                "description": s.description,
                "parameters": s.parameters,
            }),
        );
    }
    Value::Object(map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认 prompt 提供者（测试用）。
    fn dummy_prompt() -> PromptHandler {
        Arc::new(|user: &str| crate::agent::PromptParts {
            persona: format!("PROMPT[{}]", user),
            context: String::new(),
        })
    }

    #[test]
    fn 桥能起并且端口有效() {
        let h: ToolHandler = Arc::new(|_n, _a| Ok(json!({"content": []})));
        let g: GateHandler = Arc::new(|_n, _a| Ok((true, String::new())));
        let bridge = ToolBridge::start(h, g, dummy_prompt()).expect("应能起");
        assert!(bridge.port() > 0, "端口必须有效");
    }

    /// 回归测试：`/prompt` 的请求体**没有** `tool` 字段。
    ///
    /// 之前 `ToolRequest.tool` 是必需字段 → 反序列化失败 →
    /// 桥回 `{"ok":false}` → extension 拿不到 prompt →
    /// **pi 用它自己的默认 prompt，人格与记忆全丢**（2026-10-22 实测踩到）。
    #[test]
    fn prompt端点能吃没有tool字段的请求体() {
        let h: ToolHandler = Arc::new(|_n, _a| Ok(json!({})));
        let g: GateHandler = Arc::new(|_n, _a| Ok((true, String::new())));
        let bridge = ToolBridge::start(h, g, dummy_prompt()).expect("应能起");
        let port = bridge.port();

        let body = json!({"message": "你好"}).to_string();
        let req = format!(
            "POST /prompt HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
        stream.write_all(req.as_bytes()).expect("写应成功");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("读应成功");

        assert!(resp.contains("\"ok\":true"), "应成功，实际: {resp}");
        assert!(resp.contains("PROMPT[你好]"), "应回显 prompt，实际: {resp}");
    }

    #[test]
    fn 工具调用能往返() {
        // 起一个假工具
        let h: ToolHandler = Arc::new(|name, args| {
            Ok(json!({
                "content": [{"type": "text", "text": format!("{name}:{}", args)}],
                "details": {}
            }))
        });
        let g: GateHandler = Arc::new(|_n, _a| Ok((true, String::new())));
        let bridge = ToolBridge::start(h, g, dummy_prompt()).expect("应能起");
        let port = bridge.port();

        // 用极简 HTTP 客户端打一发
        let body = json!({"tool": "read_file", "args": {"path": "D:/x.md"}}).to_string();
        let req = format!(
            "POST /tool HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
        stream.write_all(req.as_bytes()).expect("写应成功");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("读应成功");

        assert!(resp.contains("200 OK"), "应返回 200，实际: {}", &resp[..resp.len().min(200)]);
        assert!(resp.contains("read_file"), "应回显工具名");
        assert_eq!(bridge.call_count(), 1);
    }

    #[test]
    fn 工具报错时返回ok_false() {
        let h: ToolHandler = Arc::new(|_n, _a| Err("权限被拒".into()));
        let g: GateHandler = Arc::new(|_n, _a| Ok((true, String::new())));
        let bridge = ToolBridge::start(h, g, dummy_prompt()).expect("应能起");
        let port = bridge.port();
        let body = json!({"tool": "bash", "args": {}}).to_string();
        let req = format!(
            "POST /tool HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
        stream.write_all(req.as_bytes()).expect("写应成功");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("读应成功");
        assert!(resp.contains("\"ok\":false"), "错误应返回 ok:false");
        assert!(resp.contains("权限被拒"));
    }
}
