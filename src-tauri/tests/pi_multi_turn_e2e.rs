//! **多轮回归测试** —— 同一个会话连发两轮，第二轮不能炸，且必须带上第一轮的上下文。
//!
//! ## 这条测的是什么（P0，2026-10-23 22:16 守门 CDP 实测发现）
//!
//! pi 的事件通道以前是「**单 receiver + take 移走语义**」
//! （`pi.rs` 的 `events_rx: Mutex<Option<Receiver>>` + `take_events()`），
//! 全仓唯一调用点取走后就**再没有任何地方归还** ⇒
//! **同一个 pi 进程（= 同一个会话）的第二轮必然取不到通道**
//! ⇒ 硬错「事件通道已被占用」。
//!
//! ⇒ pi 是默认路径（`main.ts` 里 `usePiKernel = true`）⇒
//! **用户发第一条有回复、发第二条就报错**。
//!
//! ## 为什么必须写成「两轮」而不是「一轮」
//!
//! 现有那批 e2e 里「假模型被调了 2 次」是**同一轮内**的
//! （tool_call + 最终答复）⇒ **单轮全绿完全掩盖了多轮必炸**。
//! 这条用例就是防它再犯的锁。
//!
//! ## 判据（两条都要）
//!
//! 1. **第二轮不报错**（旧实现直接返回 `Err("事件通道已被占用")`）；
//! 2. **第二轮的 provider 请求里带第一轮的上下文** ——
//!    断言请求历史里第 3 条含**第一轮产生的 tool 结果**（role == "tool"），
//!    且消息条数比第 2 条多（历史累积，跨 run 延续）。
//!
//! ⚠️ 假模型**必须说 SSE**（`text/event-stream` + `data: {...}` + `[DONE]`，
//! chunk 带 `choices[0].finish_reason`）—— 否则 pi 抛
//! "Stream ended without finish_reason" ⇒ 假红。`fake_model.rs` 已经是对的。

#[path = "fake_model.rs"]
mod fake_model;

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

/// 🔴 核心用例：**同一个会话连发两轮**，第二轮必须成功且带上第一轮的上下文。
#[tokio::test]
async fn 同会话连发两轮_第二轮不炸且带上第一轮上下文() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    // 第一轮的唯一标记：用来确认第二轮的请求里带了第一轮的历史
    const FIRST_MARK: &str = "第一轮唯一标记-MULTITURN-7f3a";

    // ── 目标文件（第一轮让模型去读它 ⇒ 会产生一条 tool 结果）──
    let work = std::env::temp_dir().join("orbcat");
    let _ = std::fs::create_dir_all(&work);
    let target = work.join("pi-multiturn-target.txt");
    std::fs::write(&target, "MAGIC-MULTITURN-42").expect("应能写目标文件");

    // 假模型：第一轮回 tool_calls（调 orb_read_file），之后回最终答复
    let fake = fake_model::FakeModel::start(
        "read_file",
        json!({ "path": target.display().to_string() }),
        "两轮都跑完了",
    )
    .expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join("orbcat-pi-multiturn-agent");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir, runtime, agent_dir);
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));

    let progress: ProgressFn = Arc::new(|_p| {});

    // ── 第一轮 ──
    let r1 = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run(FIRST_MARK, vec![], progress.clone()),
    )
    .await;
    let r1 = match r1 {
        Err(_) => panic!("第一轮超时"),
        Ok(Err(e)) => panic!("第一轮报错: {e}"),
        Ok(Ok(r)) => r,
    };
    assert!(
        r1.answer.contains("两轮都跑完了"),
        "第一轮答复不对: {}",
        r1.answer
    );

    // ── 第二轮（**同一个 rt = 同一个 pi 进程 = 同一个会话**）──
    //
    // ⚠️ 旧实现在这里直接返回 `Err("事件通道已被占用")` ⇒ 用户第二条消息就报错。
    let r2 = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run("第二轮的问题", vec![], progress),
    )
    .await;
    let r2 = match r2 {
        Err(_) => panic!("第二轮超时"),
        Ok(Err(e)) => panic!(
            "🔴 第二轮报错（这就是「同一会话第二条消息必炸」那个 P0）：{e}"
        ),
        Ok(Ok(r)) => r,
    };
    assert!(
        r2.answer.contains("两轮都跑完了"),
        "第二轮答复不对: {}",
        r2.answer
    );

    // ── 判据 2：第二轮的 provider 请求必须带上第一轮的上下文 ──
    //
    // 假模型的请求历史：
    //   [0] 第一轮 · tool_call 之前
    //   [1] 第一轮 · 工具结果回来之后（同一轮内的第二次请求）
    //   [2] 第二轮 · 第三次请求 ← **这条必须带第一轮的历史**
    let hist = fake.request_history();
    eprintln!("=== 假模型请求数 = {} ===", hist.len());
    for (i, h) in hist.iter().enumerate() {
        let n = h.get("messages").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0);
        eprintln!("  请求 {}: {} 条消息", i + 1, n);
    }
    assert!(
        hist.len() >= 3,
        "假模型应至少被调 3 次（第一轮 tool_call + 答复、第二轮答复），实际 {} 次",
        hist.len()
    );

    let msgs3 = hist[2]
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("第三次请求应有 messages");
    let has_tool_result = msgs3.iter().any(|m| {
        m.get("role").and_then(|r| r.as_str()) == Some("tool")
    });
    assert!(
        has_tool_result,
        "🔴 第二轮的 provider 请求里**没有**第一轮的工具结果 ⇒ 上下文没延续。\
         第三条请求的消息: {:?}",
        msgs3
    );

    let msgs2 = hist[1]
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("第二次请求应有 messages");
    assert!(
        msgs3.len() > msgs2.len(),
        "第二轮的消息条数应比第一轮内的第二条多（历史累积）: {} vs {}",
        msgs3.len(),
        msgs2.len()
    );

    eprintln!(
        "✅ 同会话两轮都跑通；第二轮带了第一轮的 {} 条消息",
        msgs3.len()
    );
}
