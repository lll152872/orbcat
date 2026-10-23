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
//!
//! ## 长度上限从哪来
//!
//! `≤ 20000 字` 是 Chief 拍板的瘦身目标区间（15000–20000）的上沿。
//! **本用例量的是真实 agent-data**（`orbcat_data_dir()`）⇒ 数字即真机数字，
//! 不是夹具数字。阈值留了余量：2026-10-24 实测瘦身后 ≈19.9k 字，
//! 原始 40706 字。记忆/技能变多时这条会红 —— 那是**该重新瘦身的信号**，不是误报。

#[path = "fake_model.rs"]
mod fake_model;

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::agent::ProgressFn;
use orbcat_lib::pi_runtime::PiRuntime;

/// 内容 hash（8 位 hex）—— 给 dump 文件名做唯一标识。
fn short_hash(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
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

    // ⚠️ agent_dir 必须**唯一**：pi 的 agent 目录里会有 mcp.json / settings 等，
    // 两个会话共用同一个目录会互相串（2026-10-24 实测踩到：另一条线跑测试时
    // 覆盖了本线的 pi 配置）。
    let agent_dir = std::env::temp_dir().join(format!(
        "orbcat-pi-compose-agent-{}",
        std::process::id()
    ));
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

    let len = sys.chars().count();
    eprintln!("=== 合成后 system prompt 长度 = {} 字 ===", len);

    // ── dump 全文到文件（stderr 会被记忆撑爆）──
    //
    // ⚠️ **固定名会被别的会话 / 别的分支覆盖**（2026-10-24 实测：守门跑发布线时
    // 覆盖了本分支的产物，字数结论直接对不上）⇒
    // ① 另写一份**按内容 hash 命名**的：同内容幂等可覆盖、不同内容互不踩；
    // ② **固定名那份保留**（守门 / 验收脚本在引用，删了会打断别人），
    //    但注明它只是「最近一次跑的」，**以 hash 名为准**。
    let fixed_dump = std::env::temp_dir().join("orbcat-pi-prompt-dump.txt");
    let _ = std::fs::write(&fixed_dump, &sys);
    let hashed_dump = std::env::temp_dir()
        .join(format!("orbcat-pi-prompt-dump-{}.txt", short_hash(&sys)));
    let _ = std::fs::write(&hashed_dump, &sys);
    eprintln!(
        "=== 全文 dump（**以这份为准**）: {} ===",
        hashed_dump.display()
    );
    eprintln!(
        "=== 全文 dump（固定名 = 最近一次跑的，可能被别的会话覆盖）: {} ===",
        fixed_dump.display()
    );

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

    // ── 判据 2：模型知道有工具可用、且知道**去哪查** ──
    //
    // ⚠️ **不能断言「外部工具索引」那段字面在** —— A1 就是「整段移除它」
    // ⇒ 那样断言必然红。判据的本质是「模型知道有工具、也知道去哪查」。
    let tools_doc = sys.contains("外部工具（按需查）")
        || sys.contains("search_tools")
        || sys.contains("MCP 工具组");
    assert!(
        tools_doc,
        "🔴 工具说明丢了 —— 模型不知道有哪些工具可用、也不知道去哪查"
    );
    assert!(
        sys.contains("search_tools"),
        "🔴 缺「工具清单用 `search_tools` 现查」的指针 —— 剔掉索引但不指路 = 静默删除"
    );

    // ── 判据 3：我们的记忆/上下文段在 ──
    let memory = sys.contains("# 长期记忆") || sys.contains("# 动态信息");
    assert!(memory, "🔴 我们的记忆/上下文段不在 —— 合成失败");

    // ── 判据 4：执行位置段在 ──
    let exec_loc = sys.contains("# 当前执行位置");
    assert!(
        exec_loc,
        "🔴 「当前执行位置」段不在 —— 模型不知道自己在哪种 shell 里，\
         会照旧写另一种 shell 的命令"
    );

    // ── 判据 5（B1）：RULES.md 的开发规约正文**不在**，但**指针在** ──
    //
    // 真实 agent-data 的 RULES.md 里确实有 `# 以下为执行基准` 标题 ⇒
    // 这条断言走的是真切分路径（不是"本来就没有"的假绿）。
    assert!(
        !sys.contains("# 以下为执行基准"),
        "🔴 RULES.md 的开发规约正文还在 prompt 里 —— B1 没生效"
    );
    assert!(
        sys.contains("开发规约（按需读）") && sys.contains("RULES.md"),
        "🔴 缺「开发规约全文见 RULES.md」的指针 —— 剔掉但不指路 = 静默删除"
    );

    // ── 判据 6（A1）：没有逐条 MCP 工具清单常驻 ──
    //
    // ⚠️ **不能**用 `contains("外部工具索引")` 判断 —— 指针文案里也可能出现这几个字。
    // 真正的判据是「**没有几十个具体的 mcp__<组>__<工具> 名字连着出现**」。
    // 占位写法（`mcp__<组名>__<工具名>` / `mcp__*`）不算具体工具名。
    let concrete: Vec<&str> = sys
        .match_indices("mcp__")
        .map(|(i, _)| {
            let rest = &sys[i + 5..];
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(rest.len());
            &sys[i..i + 5 + end]
        })
        // 具体工具名形如 `mcp__<组>__<工具>` ⇒ **两个** `__` 才算；
            // `mcp__*` / `mcp__<组名>__<工具名>`（占位写法）只有一个，不算。
            .filter(|n| n.matches("__").count() >= 2)
        .collect();
    assert!(
        concrete.is_empty(),
        "🔴 prompt 里出现了 {} 个具体 MCP 工具名（{:?}）—— A1 没生效：\
         工具清单不该整段常驻，应只留「用 search_tools 现查」的指针",
        concrete.len(),
        &concrete[..concrete.len().min(5)]
    );

    // ── 判据 7：激活词段**原文一字不差** ──
    //
    // 激活词是**产品功能**（用户发「寒霜」触发特定行为）⇒ 瘦身时逐字保留。
    assert!(
        sys.contains("仅当用户这条消息**有且仅有**“寒霜”两个字")
            && sys.contains("只回复“已成功”三个字")
            && sys.contains("## 十、激活词"),
        "🔴 激活词段原文不在或被改动 —— 激活词是产品功能，必须逐字保留"
    );

    // ── 判据 8：长度上限 ──
    assert!(
        len <= 20_000,
        "🔴 合成后 prompt = {len} 字，超过 20000 字上限（瘦身目标区间 15000-20000）"
    );

    eprintln!(
        "✅ 三段都在：pi 原生段 ✓ · 我们的记忆/人格段 ✓ · 执行位置段 ✓（{len} 字）"
    );
}
