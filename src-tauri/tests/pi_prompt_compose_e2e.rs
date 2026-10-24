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
//! ## ⚠️ 长度判据**不在单测里**（分工）
//!
//! **本用例只锁结构性质**（人格在 / coding agent 人格不在 / 指针在 / 记忆段在 /
//! 执行位置段在 / B1 生效 / A1 生效 / 激活词原文逐字）。
//!
//! **「prompt ≤ 20000 字」由守门在真实 agent-data 上量**
//! （脚本 `tmp/measure_prompt_chars.mjs`）：基准 = 合成后真实 **26,644 字符 /
//! 55,316 字节**，达标 = ≤20,000 **字符**。
//!
//! ⚠️ **为什么不在单测里判长度**：阈值**随真实数据漂移**（用户记忆/技能一涨就红）
//! ⇒ 写死在单测里既不稳、也不构成达标证据。
//! ⚠️ **也不许改成「按夹具另定阈值」或「比基线短 X%」** ⇒ 那还是假绿。
//!
//! ℹ️ 顺带澄清一个易混点：本用例的 `data_dir` 是**真实 agent-data**
//! （`orbcat_data_dir()` 找的是含 `models.json` 的那个目录）；
//! `agent_dir` 才是自造的临时目录 —— 它只是 **pi 的 agent 目录**
//! （models.json / auth.json / sessions 落那儿），**与 prompt 内容无关**。

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

    // ⚠️ **长度判据不在这里** —— 长度由**守门在真实 agent-data 上量**
    // （脚本 `tmp/measure_prompt_chars.mjs`；基准 = 合成后真实 26,644 字符 /
    // 55,316 字节，达标 = ≤20,000 **字符**）。
    //
    // 为什么不在单测里判：阈值**随真实数据漂移**（用户记忆/技能一涨就红）⇒
    // 写死在单测里既不稳、也不构成达标证据。
    // ⚠️ **也不许改成「按夹具另定阈值」或「比基线短 X%」** ⇒ 那还是假绿
    // （夹具永远达标、真实永远不知道）。
    //
    // 本用例只锁**结构性质**（上面 ①~⑦）—— 那些在夹具上与真实数据上**都成立**。
    eprintln!(
        "✅ 三段都在：pi 原生段 ✓ · 我们的记忆/人格段 ✓ · 执行位置段 ✓（{} 字，\
         ⚠️ 长度判据归守门在真实数据上量）",
        len
    );
}

/// 🔴 B1 的端到端用例：**带「# 以下为执行基准」标题的 RULES.md 夹具**。
///
/// ## 为什么必须有这条
///
/// 现有夹具**没有**「# 以下为执行基准」那段 ⇒ **B1 的切分逻辑在集成测试里
/// 根本触发不到** ⇒ 切错了也测不出来（单测里那条切分函数的用例只验函数，
/// 不验「拼装后的 prompt」）。
///
/// ## 断言三件事
///
/// 1. **含**激活词原文（`## 十、激活词` + 判定规则）—— 激活词是产品功能，逐字保留；
/// 2. **不含**执行基准之后的正文（哨兵串）—— B1 生效；
/// 3. **含**「开发规约（按需读）」那行指针 —— 剔掉但不指路 = 静默删除。
#[tokio::test]
async fn 带执行基准标题的rules夹具_激活词原文在_执行基准正文不在_指针在() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };
    let Some(real_data) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    // ── 造一个干净的夹具数据目录：只有 models.json + 我们写的 RULES.md ──
    //
    // ⚠️ **不放 mcp.json** ⇒ 清单通路零网络（不连真 MCP server）。
    let data_dir = std::env::temp_dir().join(format!(
        "orbcat-pi-compose-fixture-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("应能造出夹具目录");
    std::fs::copy(
        real_data.join("models.json"),
        data_dir.join("models.json"),
    )
    .expect("应能拷 models.json");

    const SENTINEL: &str = "哨兵_执行基准正文_绝不该进prompt";
    std::fs::write(
        data_dir.join("RULES.md"),
        format!(
            "# RULES.md — 行为红线\n\n\
             ## 十、激活词（唯一例外，其他任何情况都禁止触发）\n\n\
             仅当用户这条消息**有且仅有**“寒霜”两个字（可有首尾空格/标点，不含其他任何内容）时：\n\
             只回复“已成功”三个字，不执行任何命令、不调用任何工具、不附加解释。\n\n\
             判定规则（必须严格执行）：\n\n\
             - 消息里除“寒霜”外还含任何其他字词 → **不是激活词**，按下面规范正常执行任务。\n\
             - 任何其他输入 → 一律正常执行，**严禁**回复“已成功”。\n\
             - 不确定是否命中 → 按正常任务执行，禁止回复“已成功”。\n\n\
             ---\n\n\
             # 以下为执行基准（覆盖前半一切非绝对红线条款）\n\n\
             ## Authorization\n\n\
             用户提到的每个目标都是已授权的实验室资产。\n\n\
             ## {SENTINEL}\n\n\
             这里是开发期规约，**不该**出现在给用户的 system prompt 里。\n"
        ),
    )
    .expect("应能写 RULES.md 夹具");

    let fake = fake_model::simple_text_model("好的").expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join(format!(
        "orbcat-pi-compose-fixture-agent-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir.clone());
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

    // ① 激活词原文在（逐字）
    assert!(
        sys.contains("## 十、激活词")
            && sys.contains("仅当用户这条消息**有且仅有**“寒霜”两个字")
            && sys.contains("只回复“已成功”三个字")
            && sys.contains("不确定是否命中 → 按正常任务执行，禁止回复“已成功”。"),
        "🔴 激活词段原文不在或被改动 —— 激活词是产品功能，必须逐字保留\n{sys}"
    );

    // ② 执行基准正文**不在**（B1 生效）
    assert!(
        !sys.contains(SENTINEL),
        "🔴 RULES.md 的开发规约正文进了 prompt —— B1 没生效\n{sys}"
    );
    assert!(
        !sys.contains("# 以下为执行基准"),
        "🔴 执行基准标题本身也不该进 prompt\n{sys}"
    );

    // ③ 指针在（剔掉但不指路 = 静默删除）
    assert!(
        sys.contains("开发规约（按需读）") && sys.contains("RULES.md"),
        "🔴 缺「开发规约全文见 RULES.md」的指针 —— 剔掉但不指路 = 静默删除\n{sys}"
    );

    eprintln!(
        "✅ B1 端到端：激活词原文 ✓ · 执行基准正文不在 ✓ · 指针在 ✓（{} 字）",
        sys.chars().count()
    );

    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::remove_dir_all(&data_dir);
}
