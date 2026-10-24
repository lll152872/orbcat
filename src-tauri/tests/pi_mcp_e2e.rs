//! **MCP 端到端（单通道）** —— 验证「MCP 走宿主桥、pi 不再自己连 MCP server」。
//!
//! ## 架构背景（2026-10-24 用户拍板）
//!
//! 「走宿主桥，orbcat 本身作为 mcp 网管给 pi 提供 mcp」⇒
//! ① **orbcat 是唯一 MCP 网关**，pi 不再自己连 MCP server；
//! ② **单通道** —— pi 的原生 MCP 必须关掉
//!    （否则模型有两条路能调 MCP，宿主权限闸形同虚设）；
//! ③ MCP 工具经宿主的桥给 pi ⇒ 每次调用都过 orbcat 的权限闸。
//!
//! ## 这条锁的是什么
//!
//! **关原生**这件事有两个坑，少做一个都不算关掉：
//!
//! - **坑 1**：光「不调 `sync_mcp()`」**不够** —— pi 的 agent 目录里可能还留着
//!   上一轮写的 `mcp.json` ⇒ pi 照样读它、照样注册原生 MCP 工具。
//!   ⇒ 必须**显式删掉那个文件**（`disable_native_mcp`）。
//! - **坑 2**：白名单里若还留着 `mcp__*`（裸名或通配），pi 仍会放行原生那条路。
//!   ⇒ 白名单里**只准有 `orb_mcp__*`**（桥侧加 `orb_` 前缀后的名字）。
//!
//! ## 断言什么
//!
//! 1. **模型实际看到的工具表里没有任何 `mcp__` 裸名**（判据 4，闸门不可绕）
//! 2. pi 的 agent 目录里**没有** `mcp.json`
//! 3. 注入的假 MCP server **一次都没被连过**（没有 initialize / tools/list）
//! 4. orbcat 私有工具（`orb_read_file`）照旧在工具表里（判据 6）
//! 5. MCP 不可用时**不连累**主链路（pi 照常起、照常答）
//!
//! ## ⚠️ 还没覆盖的（要后端的执行侧通路就绪）
//!
//! 「模型调 MCP 工具 ⇒ 宿主权限请求 > 0 条 / 批准后真执行 / 驳回不执行 /
//! 宿主禁用后在跑的 pi 立刻调不动」—— 这四条**依赖宿主桥能把 MCP 工具
//! 送进 `execute` 的网关分支**（`tools.rs` 的 `execute_bridge_full`，
//! 后端在做）。在那之前，这里只锁**清单侧与通道侧**。

#[path = "fake_model.rs"]
mod fake_model;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
fn read_trace(path: &Path) -> Vec<serde_json::Value> {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    txt.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// 造一个**干净**的数据目录：只有 `models.json`，**没有 `mcp.json`**。
///
/// ## 为什么
///
/// 清单通路会连真实的 MCP server（`resolve_mcp_servers` 读 `data_dir/mcp.json`）
/// ⇒ 测试要**零网络**、零子进程，就必须让那个目录里没有 `mcp.json`。
fn clean_data_dir(real: &Path) -> Option<PathBuf> {
    let d = std::env::temp_dir().join(format!("orbcat-pi-mcp-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).ok()?;
    std::fs::copy(real.join("models.json"), d.join("models.json")).ok()?;
    Some(d)
}

/// 从假模型的**请求历史**里取出模型实际看到的工具名。
///
/// 这是端到端证据：请求体里的 `tools` 数组就是 provider 收到的声明。
fn model_tool_names(fake: &fake_model::FakeModel) -> Vec<String> {
    let hist = fake.request_history();
    hist.iter()
        .filter_map(|r| r.get("tools").and_then(|t| t.as_array()))
        .flatten()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

/// 🔴 单通道的核心用例：pi **不再自己连** MCP server。
#[tokio::test]
async fn pi不再自己连mcp_server_模型只看得到orb前缀的工具() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(real_data) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };
    let Some(script) = fake_mcp_script() else {
        eprintln!("跳过：找不到 tests/fake_mcp_server.cjs");
        return;
    };
    let Some(data_dir) = clean_data_dir(&real_data) else {
        eprintln!("跳过：造不出干净的数据目录");
        return;
    };

    // ── trace 文件：假 MCP server 每次调用都往里写一行 ──
    let trace_path = std::env::temp_dir().join(format!("orbcat-mcp-trace-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&trace_path);

    let fake = fake_model::simple_text_model("好的。").expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join(format!("orbcat-pi-mcp-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    // 假 server 需要内置 node 跑
    let node = runtime.join("node").join("node.exe");

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir.clone());
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));
    // 注入假 MCP server —— 只用来证明「pi 没有去连它」
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
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run("你好。", vec![], progress),
    )
    .await;
    match r {
        Err(_) => panic!("90 秒超时"),
        Ok(Err(e)) => panic!("链路报错: {e}"),
        Ok(Ok(_)) => {}
    }

    // ── 判据 4：模型实际看到的工具表里**没有任何 `mcp__` 裸名** ──
    //
    // ⚠️ 这是**最硬**的一条：它是「模型实际收到的东西」，
    // 不是我们自己的变量。
    let names = model_tool_names(&fake);
    eprintln!("=== 模型看到的工具（{} 个）===", names.len());
    let bare: Vec<&String> = names.iter().filter(|n| n.starts_with("mcp__")).collect();
    assert!(
        bare.is_empty(),
        "🔴 模型看到了 MCP 裸名 {bare:?} —— pi 原生 MCP 还在放行（单通道失效，闸门可绕）\n\
         全部工具名：{names:?}"
    );

    // ── 判据 6：orbcat 私有工具照旧在工具表里 ──
    assert!(
        names.iter().any(|n| n == "orb_read_file"),
        "orb_read_file 应在工具表里：{names:?}"
    );

    // ── 坑 1：pi 的 agent 目录里**没有** mcp.json ──
    let mcp_json = agent_dir.join("mcp.json");
    assert!(
        !mcp_json.exists(),
        "🔴 pi 的 agent 目录里还有 mcp.json —— 原生 MCP 通道还开着（pi 照样会读它）"
    );

    // ── 坑 2：注入的假 server **一次都没被连过** ──
    let trace = read_trace(&trace_path);
    let kinds: Vec<String> = trace
        .iter()
        .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(|s| s.to_string()))
        .collect();
    assert!(
        !kinds.iter().any(|k| k == "initialize"),
        "🔴 pi 仍然自己去连 MCP server（原生通道没关掉）：{kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "tools/list"),
        "🔴 pi 仍然自己去列 MCP 工具：{kinds:?}"
    );

    let _ = std::fs::remove_file(&trace_path);
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 对照测试：MCP **不可用**时，不该把整轮拖死。
///
/// 现实场景：用户在设置里配了一个 server，但它挂了 / 路径错了。
///
/// ⚠️ 单通道之后，这条路的形态变了：
/// **清单拉不到 ⇒ 清单为空 ⇒ pi 照常起（只是这轮没有 MCP 工具）**。
/// 关键不是「报错」，而是**主链路不被连累**。
#[tokio::test]
async fn mcp不可用时不卡死() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(real_data) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };
    let Some(data_dir) = clean_data_dir(&real_data) else {
        eprintln!("跳过：造不出干净的数据目录");
        return;
    };

    let fake = fake_model::simple_text_model("好的。").expect("假模型应能起");
    let agent_dir = std::env::temp_dir().join(format!("orbcat-pi-mcp-bad-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir.clone());
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
        Err(_) => panic!("90 秒超时 —— MCP 不可用不该把整轮拖死"),
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

    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::remove_dir_all(&data_dir);
}
