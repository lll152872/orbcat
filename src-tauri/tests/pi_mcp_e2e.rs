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
/// 本进程内**唯一**的临时目录名（pid + 进程内原子序号）。
///
/// ## 🔴 为什么 pid 不够、为什么固定名更糟
///
/// - 同一个 test target 里的多个 `#[tokio::test]` 是**同一个进程** ⇒ **同一个 pid**
///   ⇒ 只带 pid 命名 ⇒ 两个用例拿到**同一个目录** ⇒ tokio 默认并发 ⇒
///   A 的 `remove_dir_all` 把 B 正在用的目录删了 ⇒ B 静默跳过；反向 B 删了 A
///   ⇒ A 的 pi 起不来 ⇒ **模型看到的工具 0 个**。
/// - **完全不带 pid 的固定名更危险** ⇒ `cargo test --tests` 会**并行跑多个 target**
///   （不同进程、不同 pid）⇒ 固定名**跨 target 也会互删**
///   ⇒ 这正是「全跑红、逐 target 串行全绿」的真正原因。
///
/// ⇒ 统一用「pid + 进程内序号」⇒ 每次调用一个独立目录，彻底消除互删。
fn unique_tmp(tag: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "orbcat-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // ⚠️ **顺手把目录建出来** —— 以前这些路径直接用 `temp_dir()`（**一定存在**），
    // 改成唯一目录后不建 ⇒ pi 起来时写 models.json 失败 ⇒ 「pi 应能起」直接挂。
    //
    // ⚠️ 造不出来要 **panic**，不许静默跳过 —— 静默跳过 = 测试绿着但什么都没验（假绿）。
    std::fs::create_dir_all(&d).unwrap_or_else(|e| {
        panic!("应能造出临时目录 {}：{e}", d.display());
    });
    d
}


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
    // 主路径：测试 exe 在 target/debug/deps/ → 往上找到 src-tauri/
    if let Ok(mut p) = std::env::current_exe() {
        for _ in 0..6 {
            p = p.parent()?.to_path_buf();
            let cand = p.join("tests").join("fake_mcp_server.cjs");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // 兜底：从 agent-data 往上找仓库根的 src-tauri/tests/
    // ⚠️ 用**独立 target** 跑时 exe 不在仓库里 ⇒ 主路径找不到 ⇒ 用例会**静默跳过**
    //    （绿着但什么都没验 = 假绿）。
    let data = orbcat_data_dir()?;
    for up in 0..4 {
        let mut cand = data.clone();
        for _ in 0..up {
            cand = cand.parent()?.to_path_buf();
        }
        let c = cand.join("src-tauri").join("tests").join("fake_mcp_server.cjs");
        if c.is_file() {
            return Some(c);
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
///
/// ## ⚠️ 造不出来要 **panic**，不许静默跳过
///
/// 以前是 `.ok()?` ⇒ 造目录失败就悄悄 `return` ⇒ **测试绿着但什么都没验**
/// （假绿）。⇒ 失败就 panic 并带清晰消息。
/// ⚠️ 但「runtime/ 未构建」那类**环境性跳过保留**（那是设计如此，不是缺陷）。
fn clean_data_dir(real: &Path) -> PathBuf {
    let d = unique_tmp("pi-mcp-data");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap_or_else(|e| {
        panic!("应能造出临时数据目录 {}：{e}", d.display());
    });
    std::fs::copy(real.join("models.json"), d.join("models.json")).unwrap_or_else(|e| {
        panic!("应能拷 models.json 到 {}：{e}", d.display());
    });
    d
}

/// 从假模型的**请求历史**里取出模型实际看到的工具名。
///
/// 这是端到端证据：请求体里的 `tools` 数组就是 provider 收到的声明。
///
/// ## 🔴 工具名有**两种**形态，都要认
///
/// 真实请求体里是 OpenAI function-calling：
/// ```json
/// [{"type":"function","function":{"name":"orb_read_file","description":"…","parameters":{…}}}]
/// ```
/// ⇒ 工具名在 **`t.function.name`**，`t.name` 是 `undefined`。
/// 而 Anthropic 风格是扁平的 `{"name": …}`。
/// ⇒ 只认 `t.name` ⇒ **一个都取不到** ⇒ 「模型看到的工具 0 个」（守门 2026-10-24 真机定位）。
fn model_tool_names(fake: &fake_model::FakeModel) -> Vec<String> {
    let hist = fake.request_history();
    hist.iter()
        .filter_map(|r| r.get("tools").and_then(|t| t.as_array()))
        .flatten()
        .filter_map(|t| {
            t.get("name")
                .or_else(|| t.get("function").and_then(|f| f.get("name")))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
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
    let data_dir = clean_data_dir(&real_data);

    // ── trace 文件：假 MCP server 每次调用都往里写一行 ──
    let trace_path = unique_tmp("mcp-trace");
    let _ = std::fs::remove_file(&trace_path);

    let fake = fake_model::simple_text_model("好的。").expect("假模型应能起");

    let agent_dir = unique_tmp("pi-mcp-agent");
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
    //
    // ⚠️⚠️ **但这条在「没有宿主注入」的用例里是弱断言** ——
    // 本用例**没有**注入宿主 MCP 注册表 ⇒ pi 侧本来就没有任何 MCP 工具
    // ⇒ **天然没有 `mcp__` 裸名** ⇒ 这条**只能证明「没放行原生命名空间」**，
    // **不能证明「MCP 工具确实进来了」**。
    // ⇒ 「MCP 工具经宿主桥进 pi 工具表」由 crate 内单测
    // `注入共享注册表后_清单里有mcp工具且名字是mcp前缀`（清单侧 + 白名单侧）
    // + 守门真机证据（pi 侧真的注册了 34 个工具、含 `orb_mcp__*`）共同承担。
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
    let data_dir = clean_data_dir(&real_data);

    let fake = fake_model::simple_text_model("好的。").expect("假模型应能起");
    let agent_dir = unique_tmp("pi-mcp-bad-agent");
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

/// 🔴 回归用例：**同一 target 内的两次调用必须拿到不同目录**。
///
/// 背景（守门 2026-10-24 实测）：目录名只带 pid ⇒ 同一 target 的多个
/// `#[tokio::test]` 是同一进程 ⇒ 同一 pid ⇒ 共用同一目录 ⇒ 并发时互相
/// `remove_dir_all` ⇒ 一个用例静默跳过、另一个的 pi 起不来（工具 0 个）。
/// 而**完全不带 pid 的固定名**还会跨 target 互删（cargo 并行跑多个 target）。
///
/// 这条用例**锁住「唯一」这个性质本身**，避免以后有人把序号去掉。
#[test]
fn 同一target内两次调用拿到不同目录() {
    let a = unique_tmp("pi-mcp-uniq");
    let b = unique_tmp("pi-mcp-uniq");
    assert_ne!(a, b, "两次调用必须拿到不同目录（否则同 target 并发会互删）");
    assert_ne!(
        a.file_name(),
        b.file_name(),
        "目录名必须带进程内唯一序号"
    );
}

/// 🔴🔴 **端到端守卫用例**：**MCP 工具经宿主桥进 pi 的工具表**。
///
/// ## 这条锁的是什么
///
/// 「MCP 单通道」是**安全红线**：
/// 模型看到的 MCP 工具必须只有 `orb_mcp__<server>__<tool>`（宿主桥那一路），
/// **不能**有 `mcp__<server>__<tool>` 裸名（pi 原生 MCP 那一路 ⇒ 绕过宿主权限闸）。
///
/// 之前的覆盖只有两层：
/// - crate 内单测（清单侧 + 白名单侧）—— **没到「模型实际收到的 tools 数组」**；
/// - 守门真机证据 —— **不是仓库里的守卫用例**。
///
/// ⇒ **红线的端到端层没有守卫** ⇒ 这条补上。
///
/// ## 走的是哪条路
///
/// 1. `orbcat_lib::test_support::mcp_registry_with_stdio_server(...)`
///    —— **真连**一个 stdio 假 MCP server，造出「已连接、有工具」的**宿主**注册表；
/// 2. `PiRuntime::set_host_mcp_perm(...)` —— 注入宿主注册表 + 授权表；
/// 3. 跑一轮 ⇒ pi 的工具表由宿主那份清单生成 ⇒ 模型收到的 `tools` 数组里
///    **必须出现** `orb_mcp__<server>__<tool>`。
///
/// ## ⚠️ 断言不能写成「无裸名就绿」
///
/// 「没有 `mcp__` 裸名」在**没注入**的用例里是**弱断言**（本来就没有 MCP 工具）
/// ⇒ 绿了也说明不了什么。
/// ⇒ 本条**同时**断言「`orb_mcp__*` 确实在表里」—— **这才是这条用例存在的意义**。
#[tokio::test]
async fn 宿主注入注册表后_模型看得到orb_mcp工具且没有mcp裸名() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(real_data) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };
    let Some(script) = fake_mcp_script() else {
        eprintln!("跳过：找不到 tests/fake_mcp_server.cjs");
        return;
    };

    // ── trace：假 server 每次调用都往里写一行（这里只是让它能起）──
    let trace_path = unique_tmp("pi-mcp-inj-trace").with_extension("jsonl");
    let _ = std::fs::remove_file(&trace_path);
    let mut env = BTreeMap::new();
    env.insert(
        "MCP_TRACE_FILE".to_string(),
        trace_path.display().to_string(),
    );

    // ── ① 造宿主的 MCP 注册表（**真连** stdio 假 server）──
    let node = runtime.join("node").join("node.exe");
    let reg = orbcat_lib::test_support::mcp_registry_with_stdio_server(
        "fakemcp",
        &node.display().to_string(),
        vec![script.display().to_string()],
        env,
    )
    .await
    .expect("应能造出宿主 MCP 注册表");
    assert!(
        !reg.active_tools().is_empty(),
        "🔴 宿主注册表里应该有工具 —— 连不上假 server 就测不到这条红线"
    );

    let fake = fake_model::simple_text_model("好的").expect("假模型应能起");

    let agent_dir = unique_tmp("pi-mcp-inj-agent");
    let rt = PiRuntime::with_agent_dir(real_data.clone(), runtime.clone(), agent_dir.clone());
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));
    // ── ② 注入宿主注册表 + 授权表 ──
    //
    // ⚠️ **必须在 run 之前** —— pi 的 `--tools` 白名单是**起进程时**构造的
    //    （白名单里要列全 `orb_mcp__*`，否则模型看不到那些工具）。
    rt.set_host_mcp_perm(
        Arc::new(tokio::sync::Mutex::new(reg)),
        Arc::new(orbcat_lib::perm_request::PermHub::new()),
    );

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

    // ── ③ 断言：模型实际收到的 tools 数组里**有** orb_mcp__*、**无** mcp__ 裸名 ──
    let names = model_tool_names(&fake);
    eprintln!("=== 模型看到的工具（{} 个）===", names.len());
    let orb_mcp: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("orb_mcp__"))
        .collect();
    assert!(
        !orb_mcp.is_empty(),
        "🔴 模型看到的工具表里**没有** `orb_mcp__*` —— MCP 工具没经宿主桥进 pi 工具表\n\
         全部工具名：{names:?}"
    );
    let bare: Vec<&String> = names.iter().filter(|n| n.starts_with("mcp__")).collect();
    assert!(
        bare.is_empty(),
        "🔴 模型看到了 MCP 裸名 {bare:?} —— pi 原生 MCP 还在放行（单通道失效，闸门可绕）\n\
         全部工具名：{names:?}"
    );

    eprintln!(
        "✅ 端到端：orb_mcp__* {} 个在表里 ✓ · mcp__ 裸名 0 条 ✓",
        orb_mcp.len()
    );

    let _ = std::fs::remove_file(&trace_path);
    let _ = std::fs::remove_dir_all(&agent_dir);
}
