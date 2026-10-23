//! pi 内核集成测试 —— 真起 pi 子进程 + 真调工具桥。
//!
//! **为什么这个测试重要**：pi 的行为（协议帧、工具注册、权限往返）只有真跑才知道。
//! 单元测试能验「解析对不对」，但验不了「pi 真的认这个 extension」。
//!
//! ## 跳过条件
//!
//! 需要 `runtime/` 已构建（`node tools/build-runtime-assets.mjs`）。
//! 没有就 skip —— CI 上不该因此变红。

use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::pi::{self, PiClient, PiHostReply, PiHostRequest};
use orbcat_lib::pi_bridge::{GateHandler, PromptHandler, ToolBridge, ToolHandler};
use serde_json::{json, Value};

#[path = "fake_model.rs"]
mod fake_model;

fn runtime_dir() -> Option<PathBuf> {
    // 测试二进制在 target/debug/deps/ —— 往上找 runtime/
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..6 {
        p = p.parent()?.to_path_buf();
        let r = p.join("runtime");
        if r.join("node").join("node.exe").is_file() {
            return Some(r);
        }
        // target/debug/runtime 也算
        let r2 = p.join("runtime");
        if r2.join("pi").join("dist").join("bundle").join("cli.js").is_file() {
            return Some(r2);
        }
    }
    None
}


/// 从 orbcat 的 `models.json` 生成 pi 的 `models.json`（**调试真实模型时用**）。

/// ⚠️ 2026-10-23：测试主路已改用**假模型**（`write_fake_models`），
/// 这个函数留着给「需要跑真实模型做手工对照」用。
///
/// 复用产品代码那条路（`pi_runtime.rs::sync_models` 的逻辑）——
/// 测试和生产走同一条路，才能测出真问题。
#[allow(dead_code)]
fn write_pi_models(agent_dir: &std::path::Path, orbcat_data: &std::path::Path) -> Option<(String, String)> {
    let src = orbcat_data.join("models.json");
    let raw = std::fs::read_to_string(&src).ok()?;
    let list: Vec<Value> = serde_json::from_str(&raw).ok()?;
    let mut providers = serde_json::Map::new();
    let mut first: Option<(String, String)> = None;
    for (i, m) in list.iter().enumerate() {
        let url = m.get("url")?.as_str()?;
        if !url.starts_with("http") { continue; }
        // ⚠️ apiKey 不能是空串（pi schema: minLength 1）——
        // 一个空的会让**整个 models.json 校验失败**，所有 provider 都不加载。
        let raw_key = m.get("apiKey").and_then(|x| x.as_str()).unwrap_or("");
        let key = if raw_key.is_empty() { "dummy" } else { raw_key };
        let id = m.get("id")?.as_str()?;
        let pid = format!("orb{}", providers.len() + 1);
        providers.insert(
            pid.clone(),
            serde_json::json!({
                "baseUrl": url,
                "api": "openai-completions",
                "apiKey": key,
                "models": [{
                    "id": id,
                    "name": id,
                    "contextWindow": m.get("maxInputTokens").cloned().unwrap_or(json!(128000)),
                    // ⚠️ maxTokens 必须是 number（schema: exclusiveMinimum 0）——
                    // orbcat 那边是 null，直接透传会让整个 models.json 校验失败。
                    "maxTokens": match m.get("maxOutputTokens").and_then(|x| x.as_u64()) {
                        Some(v) => json!(v),
                        None => json!(16384),
                    },
                    "input": ["text"],
                    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
                }]
            }),
        );
        if first.is_none() { first = Some((pid, id.to_string())); }
        let _ = i;
    }
    if providers.is_empty() { return None; }
    let _ = std::fs::create_dir_all(agent_dir);
    let doc = serde_json::json!({
        "$schema": "https://pi.dev/schemas/models.schema.json",
        "providers": providers
    });
    std::fs::write(agent_dir.join("models.json"), doc.to_string()).ok()?;
    first
}

/// 给 pi 写一份指向**假模型**的 models.json（CI 里也能跑）。

/// 形状与产品代码 `sync_models` 产出的完全一致（provider → baseUrl/api/apiKey/models），
/// 差别只在 baseUrl 指向假服务器。
fn write_fake_models(agent_dir: &std::path::Path, base_url: &str) -> (String, String) {
    let _ = std::fs::create_dir_all(agent_dir);
    let doc = json!({
        "providers": {
            "orb1": {
                "name": "fake",
                "baseUrl": base_url,
                "api": "openai-completions",
                "apiKey": "dummy",
                "models": [{
                    "id": "fake-model",
                    "name": "fake-model",
                    "contextWindow": 128000,
                    "maxTokens": 16384,
                    "input": ["text"],
                    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
                }]
            }
        }
    });
    std::fs::write(agent_dir.join("models.json"), doc.to_string())
        .expect("应能写 models.json");
    ("orb1".to_string(), "fake-model".to_string())
}

/// orbcat 的数据目录（测试从这里读 models.json）。
#[allow(dead_code)]
fn orbcat_data_dir() -> PathBuf {
    // 仓库根的 orbcat/agent-data/
    let mut p = std::env::current_exe().ok().unwrap_or_default();
    for _ in 0..6 {
        p = p.parent().unwrap_or(&p).to_path_buf();
        let d = p.join("agent-data");
        if d.join("models.json").is_file() {
            return d;
        }
        let d2 = p.join("orbcat").join("agent-data");
        if d2.join("models.json").is_file() {
            return d2;
        }
    }
    PathBuf::from("agent-data")
}

/// 起一个桥，返回（端口，调用计数）。
fn start_bridge() -> (Arc<ToolBridge>, Arc<std::sync::atomic::AtomicU64>) {
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let c2 = calls.clone();
    let handler: ToolHandler = Arc::new(move |name, args| {
        c2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({
            "content": [{"type": "text", "text": format!("{name} called with {args}")}],
            "details": {}
        }))
    });
    let gate: GateHandler = Arc::new(|_n, _a| Ok((true, String::new())));
    let prompt: PromptHandler = Arc::new(|u: &str| format!("PROMPT[{u}]"));
    let b = ToolBridge::start(handler, gate, prompt).expect("桥应能起");
    (b, calls)
}

#[tokio::test]
async fn 桥能收到工具调用并返回结果() {
    let (bridge, calls) = start_bridge();
    let port = bridge.port();
    assert!(port > 0);

    // 用最简 HTTP 客户端打一发
    let body = json!({"tool": "read_file", "args": {"path": "D:/x.md"}}).to_string();
    let req = format!(
        "POST /tool HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
    s.write_all(req.as_bytes()).expect("写应成功");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("读应成功");

    assert!(resp.contains("200 OK"), "应 200");
    assert!(resp.contains("read_file"), "应回显工具名");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn gate端点能裁决() {
    let handler: ToolHandler = Arc::new(|_n, _a| Ok(json!({})));
    let gate: GateHandler = Arc::new(|_n, _a| Ok((false, "被拒绝了".to_string())));
    let prompt: PromptHandler = Arc::new(|u: &str| format!("PROMPT[{u}]"));
    let b = ToolBridge::start(handler, gate, prompt).expect("桥应能起");
    let port = b.port();

    let body = json!({"tool": "write", "args": {}}).to_string();
    let req = format!(
        "POST /gate HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
    s.write_all(req.as_bytes()).expect("写应成功");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("读应成功");

    assert!(resp.contains("\"allowed\":false"), "应拒绝，实际: {}", &resp[..resp.len().min(300)]);
    assert!(resp.contains("被拒绝了"));
}

/// `/prompt` 端点能拿到 system prompt。
///
/// ## 回归测试（2026-10-22）
///
/// 早期 `ToolRequest.tool` 是**必需**字段 → `/prompt` 的请求体
/// （只有 `{"message": "..."}`，没有 `tool`）反序列化直接失败 →
/// 桥回 `{"ok":false}` → extension 拿不到 prompt →
/// **pi 静默退回它自己的默认 prompt，orbcat 的人格与记忆全丢**。
///
/// 症状完全看不出根因（表现为"模型突然变得不认识我了"），
/// 所以这条测试必须守住。
#[tokio::test]
async fn prompt端点能拿到宿主拼的prompt() {
    let handler: ToolHandler = Arc::new(|_n, _a| Ok(json!({})));
    let gate: GateHandler = Arc::new(|_n, _a| Ok((true, String::new())));
    let prompt: PromptHandler = Arc::new(|u: &str| format!("HOST-PROMPT[{u}]"));
    let b = ToolBridge::start(handler, gate, prompt).expect("桥应能起");
    let port = b.port();

    // ⚠️ 请求体**故意不带** `tool` 字段 —— 这正是之前挂掉的原因
    let body = json!({"message": "我叫什么名字"}).to_string();
    let req = format!(
        "POST /prompt HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
    s.write_all(req.as_bytes()).expect("写应成功");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("读应成功");

    assert!(resp.contains("\"ok\":true"), "应成功，实际: {}", &resp[..resp.len().min(300)]);
    assert!(
        resp.contains("HOST-PROMPT[我叫什么名字]"),
        "应回显宿主拼的 prompt，实际: {}",
        &resp[..resp.len().min(300)]
    );
}

/// 真起 pi，跑一次 `get_state`。
#[tokio::test]
async fn pi子进程能起来并回应命令() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };
    let entry = runtime.join("pi").join("dist").join("bundle").join("cli.js");
    if !entry.is_file() {
        eprintln!("跳过：pi bundle 不存在");
        return;
    }

    let (bridge, _calls) = start_bridge();
    let mut cfg = pi::PiConfig::new(
        runtime.clone(),
        std::env::temp_dir().join("orbcat-pi-test-agent"),
        std::env::temp_dir(),
    );
    cfg.bridge_port = bridge.port();
    cfg.tool_specs = json!({});
    cfg.offline = true;

    let host: pi::PiHostFn = Arc::new(|_req: PiHostRequest| Some(PiHostReply::Confirm(true)));
    let client = PiClient::spawn(cfg, host).await.expect("pi 应能起");

    // 发一条 get_state，等响应
    let data = client
        .command(json!({"type": "get_state"}), 20_000)
        .await
        .expect("get_state 应成功");
    assert!(data.get("sessionId").is_some(), "应有 sessionId，实际: {data}");

    client.shutdown().await;
}

/// pi 能通过桥调到 Rust 的工具。
///
/// 这是**整个方案成立的核心验证**：
/// pi（TS 侧）→ extension → HTTP → Rust → 工具执行 → 结果回到模型。
#[tokio::test]
async fn pi能通过桥调到rust工具() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let entry = runtime.join("pi").join("dist").join("bundle").join("cli.js");
    let ext = runtime.join("pi").join("extensions").join("orbcat-tools.ts");
    if !entry.is_file() || !ext.is_file() {
        eprintln!("跳过：pi bundle 或 extension 不存在");
        return;
    }
    if !runtime.join("node").join("node.exe").is_file() {
        eprintln!("跳过：内置 node 不存在");
        return;
    }

    // ✅ **用假模型**（`fake_model.rs`）—— 2026-10-23 改：
    //
    // 原来这条**必须配真实模型**（注释原话：「否则 pi 的 prompt 直接返回
    // 'No API key found for the selected model'，模型根本不会启动」）
    // ⇒ 真模型的 key 不在 CI 里 ⇒ **CI 必红** ⇒ 只能给它加 `-- --skip`
    // ⇒ **整条集成测试在 CI 里被静默默跳过**（最坏的一种假绿：
    // 闸门「绿」了，但核心链路一条没跑）。
    //
    // 现在改成假模型：真 HTTP + 真 SSE + 真模型协议 + 真 extension，
    // 只有「模型怎么想」是写死的 ⇒ **CI 里也能真跑**（skip 可以删了）。
    let agent_dir = std::env::temp_dir().join("orbcat-pi-test-agent2");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let fake = fake_model::FakeModel::start(
        "probe_tool",
        json!({ "x": "hello" }),
        "调用完成",
    )
    .expect("假模型应能起");
    let (provider, model) = write_fake_models(&agent_dir, &fake.base_url());

    let (bridge, calls) = start_bridge();
    let mut cfg = pi::PiConfig::new(
        runtime.clone(),
        agent_dir,
        std::env::temp_dir(),
    );
    cfg.provider = Some(provider);
    cfg.model = Some(model);
    cfg.bridge_port = bridge.port();
    // 给 extension 一个假工具（名字带 orb_ 前缀，与真实路径一致）
    cfg.tool_specs = json!({
        "probe_tool": {
            "description": "探针工具",
            "parameters": {
                "type": "object",
                "properties": {"x": {"type": "string"}},
                "required": [],
                "additionalProperties": false
            }
        }
    });
    cfg.extra_args.push("--extension".into());
    cfg.extra_args.push(ext.to_string_lossy().to_string());
    cfg.tools = vec!["orb_probe_tool".into()];
    cfg.offline = true;

    let host: pi::PiHostFn = Arc::new(|_req: PiHostRequest| Some(PiHostReply::Confirm(true)));
    let client = PiClient::spawn(cfg, host).await.expect("pi 应能起");

    // 订阅事件（**每轮一个独立通道** —— 旧的 take 语义会导致第二轮取不到）
    let (sub_id, mut rx) = client.subscribe_events();

    // 发一条 prompt，让模型去调那个工具
    let _ = client.prompt("只调用一次 orb_probe_tool，x 填 hello。不要做别的事。", vec![]).await;

    // 等 settled
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut saw_tool = false;
    while std::time::Instant::now() < deadline {
        let Ok(Some(ev)) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
        else {
            continue;
        };
        if pi::is_settled(&ev) {
            break;
        }
        eprintln!("EV: {}", ev.to_string().chars().take(300).collect::<String>());
        if let Some((_id, name, _args)) = pi::tool_call(&ev) {
            eprintln!("  -> tool_call {name}");
            if name == "orb_probe_tool" {
                saw_tool = true;
            }
        }
    }

    // 退订（与 `PiRuntime::run_inner` 同口径：不退订订阅者列表会一直涨）
    client.unsubscribe_events(sub_id);

    assert!(saw_tool, "模型应该发起了 orb_probe_tool 调用");
    assert!(
        calls.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "桥应该被调用过（说明 extension → HTTP → Rust 通了）"
    );

    client.shutdown().await;
}
