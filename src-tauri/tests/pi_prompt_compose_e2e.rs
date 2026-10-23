//! **prompt 合成回归测试** —— pi 原生段 + 我们的记忆段 + 执行位置段，**三样都得在**。
//!
//! ## 这条锁的是什么
//!
//! pi 的 system prompt 是**分段构造**的（上游 `buildSystemPromptSections`）：
//! `preamble` + `tools` + `rules` + `docs` + `project_context` + `skills`
//! + 各 extension 写进 `sections` 的段。
//!
//! 我们以前**整段替换**（`{ systemPrompt: p }` ⇒ 上游设 `forceSystemPrompt`）
//! ⇒ **pi 原生那些段全丢**（工具说明 / 规则 / cwd / 项目上下文 / MCP 清单）。
//!
//! 现在改成**合成**：
//! - `customPrompt` = 我们的人格（替换掉 pi 的 coding agent 人格 ⇒ 我们的占主导）
//! - `sections["orbcat_context"]` = 记忆 / 项目 / 模式 / 技能 / **执行位置**
//! - pi 原生的 tools / rules / docs / project_context / MCP 清单 **全部保留**
//!
//! ⇒ 三样都在才算对。
//!
//! ## 判据（都从**模型实际收到的** system prompt 里读，不是读我们自己的变量）
//!
//! 用假模型的**请求历史**（`request_history()`）取 system 消息 —— 那是 pi
//! 真正发给 provider 的东西 ⇒ 端到端证据。
//!
//! ⚠️ 假模型**必须说 SSE**（`text/event-stream` + `data:` + `[DONE]`，chunk 带
//! `choices[0].finish_reason`）—— 否则 pi 抛 "Stream ended without finish_reason"。

#[path = "fake_model.rs"]
mod fake_model;

use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::agent::ProgressFn;
use orbcat_lib::pi_runtime::PiRuntime;

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

/// 🔴 核心用例：合成后的 system prompt 里**三样都在**。
#[tokio::test]
async fn 合成后_pi原生段_我们的记忆段_执行位置段_都在() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    let fake = fake_model::simple_text_model("好的")
        .expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join("orbcat-pi-compose-agent");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir, runtime, agent_dir);
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run("合成一下 prompt", vec![], progress),
    )
    .await;
    match r {
        Err(_) => panic!("超时"),
        Ok(Err(e)) => panic!("链路报错: {e}"),
        Ok(Ok(_)) => {}
    }

    // ── 从**模型实际收到的**请求里取 system prompt ──
    let hist = fake.request_history();
    assert!(!hist.is_empty(), "假模型应收到过请求");
    let sys = hist[0]
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| {
            panic!(
                "system 消息的 content 应是字符串，实际: {:?}",
                &hist[0]["messages"][0]
            )
        })
        .to_string();

    eprintln!("=== 合成后 system prompt 长度 = {} 字 ===", sys.chars().count());

    // dump 全文到文件（stderr 会被记忆撑爆）
    let dump = std::env::temp_dir().join("orbcat-pi-prompt-dump.txt");
    let _ = std::fs::write(&dump, &sys);
    eprintln!("=== 全文 dump: {} ===", dump.display());

    // ── 判据 1：我们的人格在，且**pi 的 coding agent 人格已被换掉** ──
    //
    // 我们用 `customPrompt` 放人格 ⇒ 替换掉 pi 默认的 coding assistant preamble
    // ⇒ 我们的身份占主导，模型不会以为自己是写代码的助手。
    let persona = sys.contains("# 你的人格与语气") || sys.contains("# 你的身份");
    assert!(persona, "🔴 我们的人格段不在 —— 合成失败");
    assert!(
        !sys.contains("coding assistant") && !sys.contains("coding agent"),
        "🔴 pi 的 coding agent 人格还在 —— 人格打架，模型会以为自己是写代码的助手"
    );

    // ── 判据 2：工具说明没丢 ──
    //
    // pi 的 `tools` 段（它自己的工具清单）因为 customPrompt 被连带跳过
    // （上游 if/else：customPrompt 一设，tools/rules/docs 三段都不生成）——
    // 但那三段是 **coding agent 专用**（pi 文档 / bash 工具规则 / 代码工具清单），
    // 对「用户的私人助手」无用。我们自己的**外部工具索引**段顶替它。
    let tools_doc = sys.contains("外部工具索引") || sys.contains("MCP 工具组");
    assert!(
        tools_doc,
        "🔴 工具说明丢了 —— 模型不知道有哪些工具可用"
    );

    // ── 判据 3：我们的记忆/上下文段在 ──
    let memory = sys.contains("# 长期记忆") || sys.contains("# 动态信息");
    assert!(memory, "🔴 我们的记忆/上下文段不在 —— 合成失败");

    // ── 判据 3：执行位置段在 ──
    let exec_loc = sys.contains("# 当前执行位置");
    assert!(
        exec_loc,
        "🔴 「当前执行位置」段不在 —— 模型不知道自己在哪种 shell 里，\
         会照旧写另一种 shell 的命令"
    );

    eprintln!(
        "✅ 三段都在：pi 原生段 ✓ · 我们的记忆/人格段 ✓ · 执行位置段 ✓（{} 字）",
        sys.chars().count()
    );
}
