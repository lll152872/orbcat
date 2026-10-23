//! **MCP 端到端** —— 验证 pi 真的能连 MCP server、真的能调到 MCP 工具。
//!
//! ## 为什么必须测这条
//!
//! MCP 是**唯一没被验证过的代码路径**：
//! - `sync_mcp()` 把 orbcat 的 `mcp.json` 转成 pi 的格式
//! - 白名单加 `mcp__*` 让工具直接声明给模型
//! - `pi_host_gate` 里对 `mcp__*` 走 grants / 确认卡
//!
//! 上面三条都"看起来对"，但**没有一次「pi 连上 server → 列出工具 →
//! 模型调工具 → 工具真的执行」的实测**。代码路径没跑过 ≠ 能跑。
//!
//! ## 怎么测
//!
//! 起一个**本地假 MCP server**（`tests/fake_mcp_server.cjs`，JSON-RPC over stdio），
//! 再起**假模型**（`tests/fake_model.rs`）驱动它去调 MCP 工具：
//!
//! ```text
//! 假模型（第一轮）→ 回 tool_calls: mcp__orbcat-test__get_marker
//!                          ↓
//!   pi 的 MCP client（真）→ 真的连上假 server（真 stdio JSON-RPC）
//!                          ↓
//!   假 server 执行 → 回 "MCP-MARKER-42" + 往 trace 文件留痕
//!                          ↓
//! 假模型（第二轮）→ 看到结果 → 给最终答复
//! ```
//!
//! ## 断言什么
//!
//! 1. **假 server 真的被连上了**（trace 里有 `initialize`）
//! 2. **`tools/list` 真的被问了**（trace 里有 `tools/list`）
//! 3. **工具真的被调了、参数真的是那个**（trace 里有 `tools/call` + name）
//! 4. **pi 侧看到了工具结果**（`MCP-MARKER-42` 出现在 steps 里）
//! 5. **`mcp.json` 格式对**（pi 真的能读）

#[path = "fake_model.rs"]
mod fake_model;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::agent::ProgressFn;
use orbcat_lib::pi_runtime::PiRuntime;
use serde_json::json;

fn runtime_dir() -> Option<PathBuf> {
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..7 {
        p = p.parent()?.to_path_buf();
        let r = p.join("runtime");
        if r.join("node").join("node.exe").is_file()
            && r.join("pi").join("dist").join("bundle").join("cli.js").is_file()
        {
            return Some(r);
        }
    }
    None
}

fn orbcat_data_dir() -> Option<PathBuf> {
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..7 {
        p = p.parent()?.to_path_buf();
        for cand in [p.join("agent-data"), p.join("orbcat").join("agent-data")] {
            if cand.join("models.json").is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 找到 `fake_mcp_server.cjs`（源码在 `src-tauri/tests/` 下）。
fn fake_mcp_script() -> Option<PathBuf> {
    // 测试 exe 在 target/debug/deps/ → 往上找到 src-tauri/
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..6 {
        p = p.parent()?.to_path_buf();
        let cand = p.join("tests").join("fake_mcp_server.cjs");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 读 trace 文件里某一类记录。
fn read_trace(path: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    txt.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// MCP 全链路：pi 连上 server → 列出工具 → 模型调用 → 工具真的执行。
#[tokio::test]
async fn pi能连上mcp并真的调到mcp工具() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };
    let Some(script) = fake_mcp_script() else {
        eprintln!("跳过：找不到 tests/fake_mcp_server.cjs");
        return;
    };

    // ── trace 文件：假 MCP server 每次调用都往里写一行 ──
    let trace_path = std::env::temp_dir().join("orbcat-mcp-trace.jsonl");
    let _ = std::fs::remove_file(&trace_path);

    // ── 假模型：第一轮让模型调 **MCP 工具**，第二轮给答复 ──
    //
    // ⚠️ 工具全名是 `mcp__<server>__<tool>`：
    //   server 名 = `orbcat-test`（`-` 在命名空间里会换 `_`）
    //   工具名   = `get_marker`
    // 所以模型看到的是 `mcp__orbcat-test__get_marker`。
    //
    // 注意：`FakeModel::start` 会自己加 `orb_` 前缀，
    // 所以这里传的 tool_name 需要**绕过**那个前缀 —— 用 `start_raw()`。
    let fake = fake_model::FakeModel::start_raw(
        "mcp__orbcat_test__get_marker",
        json!({}),
        "MCP 工具返回了 MCP-MARKER-42。",
    )
    .expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join("orbcat-pi-mcp-agent");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    // 假 server 需要内置 node 跑（用户机器上没有裸 node 也能跑）
    let node = runtime.join("node").join("node.exe");

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir.clone());
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));
    // 注入假 MCP server（不写用户的 agent-data/mcp.json）
    let mut env = BTreeMap::new();
    env.insert(
        "MCP_TRACE_FILE".to_string(),
        trace_path.display().to_string(),
    );
    rt.add_mcp_server_for_test(
        "orbcat-test",
        node.display().to_string(),
        vec![script.display().to_string()],
        env,
    )
    .await;

    let progress: ProgressFn = Arc::new(|_p| {});

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run("调用 get_marker 工具，告诉我它返回了什么。", vec![], progress),
    )
    .await;

    // ── 先看 pi 的 mcp.json 写成什么样（不管成败都能查）──
    let mcp_json = agent_dir.join("mcp.json");
    let mcp_txt = std::fs::read_to_string(&mcp_json).unwrap_or_default();
    eprintln!("=== pi 的 mcp.json ===\n{mcp_txt}");

    let trace = read_trace(&trace_path);
    eprintln!("=== MCP trace（{} 条）===", trace.len());
    for t in &trace {
        eprintln!("{}", t);
    }

    let result = match result {
        Err(_) => panic!(
            "90 秒超时 —— 假模型 + 假 MCP server 都是本地的，超时说明链路卡住。\n\
             trace: {trace:?}"
        ),
        Ok(Err(e)) => panic!("链路报错: {e}\n trace: {trace:?}"),
        Ok(Ok(r)) => r,
    };

    eprintln!("=== 最终答复 ===\n{}", result.answer);
    eprintln!("=== 步骤 ===\n{:#?}", result.steps);

    // ── 验收 1：pi 的 mcp.json 里有注入的 server，且 exposure=direct ──
    assert!(
        mcp_txt.contains("orbcat-test") || mcp_txt.contains("orbcat_test"),
        "pi 的 mcp.json 里应有注入的 server，实际:\n{mcp_txt}"
    );
    assert!(
        mcp_txt.contains("\"direct\""),
        "exposure 必须是 direct（否则工具不直接给模型），实际:\n{mcp_txt}"
    );

    // ── 验收 2：假 server 真被**连上**了（initialize）──
    let kinds: Vec<String> = trace
        .iter()
        .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(|s| s.to_string()))
        .collect();
    assert!(
        kinds.iter().any(|k| k == "initialize"),
        "MCP server 应该收到 initialize（说明 pi 真的连上了），实际 trace kinds: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "tools/list"),
        "MCP server 应该收到 tools/list（说明 pi 真的列了工具），实际: {kinds:?}"
    );

    // ── 验收 3：工具**真的被调了**，名字对 ──
    let call = trace.iter().find(|v| {
        v.get("kind").and_then(|k| k.as_str()) == Some("tools/call")
    });
    let Some(call) = call else {
        panic!("MCP 工具应该被调用过（trace 里没有 tools/call），实际 trace: {trace:?}");
    };
    let called_name = call
        .get("payload")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    assert_eq!(
        called_name, "get_marker",
        "调用的应该是 get_marker，实际: {called_name}"
    );

    // ── 验收 4：工具结果回到了 pi（模型看到了 MCP-MARKER-42）──
    let saw_marker = result
        .steps
        .iter()
        .any(|s| s.detail.contains("MCP-MARKER-42"));
    assert!(
        saw_marker,
        "步骤里应出现 MCP 工具返回的 MCP-MARKER-42，实际: {:#?}",
        result.steps
    );
}

/// 对照测试：MCP server **连不上**时，不该把整轮拖死。
///
/// 现实场景：用户在设置里配了一个 server 但其进程没起（或路径错了）。
/// 这时 pi 应该报错但**不卡**，而 orbcat 应该能把错误传出来。
#[tokio::test]
async fn mcp_server连不上时不卡死() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };

    let fake = fake_model::simple_text_model("好的。").expect("假模型应能起");
    let agent_dir = std::env::temp_dir().join("orbcat-pi-mcp-bad-agent");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir);
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));
    // 注入一个**不存在的**命令 —— 连不上
    rt.add_mcp_server_for_test(
        "broken-server",
        r"C:\__definitely_not_here__\nope.exe".to_string(),
        vec![],
        BTreeMap::new(),
    )
    .await;

    let progress: ProgressFn = Arc::new(|_p| {});
    let started = std::time::Instant::now();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run("你好。", vec![], progress),
    )
    .await;
    let elapsed = started.elapsed();

    eprintln!("=== 耗时 {:?} ===", elapsed);

    match r {
        Err(_) => panic!("90 秒超时 —— MCP 连不上不该把整轮拖死"),
        Ok(Err(e)) => {
            // 报错可以接受（连接失败是真实错误），关键是要**快速**报错
            eprintln!("链路报错（可接受）: {e}");
            assert!(
                elapsed < std::time::Duration::from_secs(80),
                "应该快速失败，实际耗时 {elapsed:?}"
            );
        }
        Ok(Ok(run)) => {
            // 更理想：MCP 挂了但对话照常 —— 只说明情况
            eprintln!("对话照常完成（MCP 故障没影响主链路）: {}", run.answer);
        }
    }
}
