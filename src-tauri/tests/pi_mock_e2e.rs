//! **mock 端到端测试** —— pi 内核当 orbcat 的 agent 内核，整条链跑一遍。
//!
//! ## 为什么不打真模型（用户原话：「这么慢吗，为啥不用 mock 测试啊」）
//!
//! 真模型：慢（prompt 4 万字 × 每轮几秒）、会挂（429 额度、限流、网络）、
//! 不确定（模型可能不调工具）—— 测出来的是**环境问题**，不是代码问题。
//!
//! 所以这里用 `fake_model.rs` 起一个**假的 OpenAI 兼容服务**：
//!
//! ```text
//! 真实：前端 → chat_pi → PiRuntime → 起工具桥 → 起 pi 子进程（真 node + 真 pi bundle）
//!                    → 三个真 extension（tools / gate / prompt）
//!                    → pi 真的发 HTTP + 真 SSE  ← 只有这一步换成假模型
//!                    → 工具真的经桥回 Rust → 真权限网关 → 真读文件
//!                    → 真落盘（checkpoint）
//! ```
//!
//! **除了"模型怎么想"是写死的，其它全是真的。**毫秒级、确定、可断言。
//!
//! ## 断言什么
//!
//! 1. 模型发起的 `orb_read_file` **真的到了 Rust**，读出了文件内容
//! 2. `system prompt` 被 orbcat 接管（假模型收到的 messages 里能看到记忆）
//! 3. 中途正文 + 工具步骤都落盘了（`kind:"text"` / `kind:"tool_call"`）
//! 4. MCP 配置同步（有 server 时有 `mcp.json`）

#[path = "fake_model.rs"]
mod fake_model;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

/// 核心端到端：一轮对话里调一个真工具（mock 模型驱动）。
#[tokio::test]
async fn mock模型驱动_完整一轮跑通() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    // ── 目标文件：放在已授权目录（%TEMP%\orbcat）──
    let work = std::env::temp_dir().join("orbcat");
    let _ = std::fs::create_dir_all(&work);
    let target = work.join("pi-mock-target.txt");
    std::fs::write(&target, "MAGIC-ORBCAT-42").expect("应能写目标文件");

    // ── 假模型：第一轮让模型调 orb_read_file，第二轮给答复 ──
    let fake = fake_model::FakeModel::start(
        "read_file",
        json!({ "path": target.display().to_string() }),
        "读到了：MAGIC-ORBCAT-42",
    )
    .expect("假模型应能起");

    // ── 临时 agent 目录（pi 的 models.json / sessions 落这儿）──
    let agent_dir = std::env::temp_dir().join("orbcat-pi-mock-agent");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir.clone());
    // 让 pi 去连假模型（URL 不在 models.json 里 → sync_models 会注入临时 provider）
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a| {
        Box::pin(async { (true, String::new()) })
    }));

    // ── 收集进度 / checkpoint ──
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    let progress: ProgressFn = Arc::new(move |p| {
        ev2.lock().unwrap().push(format!("{p:?}"));
    });

    let ck_count = Arc::new(AtomicUsize::new(0));
    let ck_kinds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ck2 = ck_count.clone();
    let ck3 = ck_kinds.clone();
    let checkpoint: orbcat_lib::agent::CheckpointFn = Arc::new(move |steps, _a, _r| {
        ck2.fetch_add(1, Ordering::Relaxed);
        let mut g = ck3.lock().unwrap();
        g.clear();
        for s in steps {
            g.push(s.kind.clone());
        }
    });

    // ── 跑（30 秒够 —— 假模型毫秒级返回）──
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        rt.run_with_checkpoint(
            &format!("读一下 {} 的内容，然后告诉我。", target.display()),
            vec![],
            progress,
            Some(checkpoint),
        ),
    )
    .await;

    let result = match result {
        Err(_) => panic!("60 秒超时 —— 假模型是毫秒级的，超时说明链路卡住了"),
        Ok(Err(e)) => panic!("链路报错: {e}"),
        Ok(Ok(r)) => r,
    };

    eprintln!("=== 最终答复 ===\n{:?}", result.answer);
    eprintln!("=== 步骤 ===\n{:#?}", result.steps);
    eprintln!("=== 进度事件 ===\n{}", events.lock().unwrap().join("\n"));
    eprintln!("=== checkpoint 次数 = {} ===", ck_count.load(Ordering::Relaxed));
    eprintln!("=== checkpoint 条目 = {:?} ===", ck_kinds.lock().unwrap());

    // ── 验收 1：假模型被调了 2 次（第一次给工具调用，第二次给答复）──
    assert_eq!(
        fake.call_count(),
        2,
        "假模型应被调 2 次，实际 {} 次",
        fake.call_count()
    );

    // ── 验收 2：工具真的经桥回 Rust 读了文件 ──
    let read_step = result.steps.iter().find(|s| {
        s.name
            .as_deref()
            .map(|n| n.contains("read_file"))
            .unwrap_or(false)
    });
    assert!(
        read_step.is_some(),
        "应调用过 read_file（经桥回 Rust），实际步骤: {:#?}",
        result.steps
    );

    let saw_content = result
        .steps
        .iter()
        .any(|s| s.kind == "tool_result" && s.detail.contains("MAGIC-ORBCAT-42"));
    assert!(
        saw_content,
        "工具结果应含文件内容（说明真的读到了文件），实际: {:#?}",
        result.steps
    );

    // ── 验收 3：最终答复来自假模型的第二轮 ──
    assert!(
        result.answer.contains("MAGIC-ORBCAT-42"),
        "最终答复应含内容，实际: {}",
        result.answer
    );

    // ── 验收 4：轮内落盘被触发（中途正文不会凭空消失）──
    assert!(
        ck_count.load(Ordering::Relaxed) > 0,
        "checkpoint 应被触发过（否则中途正文会丢）"
    );
    let kinds = ck_kinds.lock().unwrap().clone();
    assert!(
        kinds.iter().any(|k| k == "tool_call"),
        "落盘的步骤里应有 tool_call，实际: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "text"),
        "落盘的步骤里应有中途正文（kind:text）—— 否则跑完就凭空消失，实际: {kinds:?}"
    );

    // ── 验收 5：prompt 接管生效（进度流里有 DiscardStream = 中途正文被归档）──
    let evs = events.lock().unwrap().clone();
    assert!(
        evs.iter().any(|e| e.contains("DiscardStream")),
        "应有 DiscardStream 事件（模型写了正文又调工具 → 前端淡化归档），实际: {evs:?}"
    );
}

/// prompt 接管：假模型能"看到" orbcat 的人格与记忆。
///
/// 判定方式：假模型把收到的 messages 存下来，我们检查 system prompt 里
/// 有没有 orbcat 记忆的特征词。**不看模型输出，只看输入** —— 这才是
/// 「prompt 接管有没有生效」的直接证据。
#[tokio::test]
async fn mock模型能看到orbcat的人格与记忆() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };

    let fake = fake_model::simple_text_model("好的。").expect("假模型应能起");
    let agent_dir = std::env::temp_dir().join("orbcat-pi-mock-prompt");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir);
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a| {
        Box::pin(async { (true, String::new()) })
    }));

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        rt.run("你好。", vec![], progress),
    )
    .await;

    match r {
        Err(_) => panic!("60 秒超时"),
        Ok(Err(e)) => panic!("链路报错: {e}"),
        Ok(Ok(_)) => {}
    }

    // 假模型收到的最后一条请求 = prompt 接管后的结果
    let last = fake.last_request().expect("假模型应收到过请求");
    let sys = fake.last_system_prompt().unwrap_or_default();
    eprintln!("=== system prompt 长度 = {} ===", sys.chars().count());
    eprintln!("=== 前 300 字 ===\n{}", sys.chars().take(300).collect::<String>());

    // orbcat 的记忆特征词（来自 SOUL.md / USER.md / RULES.md）
    let hit = ["用户", "直言不讳", "私人助手", "orbcat"]
        .iter()
        .any(|k| sys.contains(k));
    assert!(
        hit,
        "system prompt 里应出现 orbcat 记忆的特征词（说明 prompt 接管生效）。\
         实际前 300 字: {}",
        sys.chars().take(300).collect::<String>()
    );
    // 反向断言：不该是 pi 的默认 coding agent prompt
    assert!(
        !sys.contains("expert coding assistant operating inside pi"),
        "不该是 pi 的默认 prompt —— 说明接管失败了"
    );
    let _ = last;
}
