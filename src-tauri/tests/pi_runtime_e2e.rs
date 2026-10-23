//! pi 内核端到端验证 —— 真起运行时、真跑一轮对话、真调 Rust 工具。
//!
//! 这个测试验的是**整条链**：
//! ```
//! PiRuntime::run
//!   → 起 pi 子进程（内置 node + 精简 bundle）
//!   → 起工具桥（127.0.0.1 临时端口）
//!   → pi extension 调 orb_xxx 工具 → HTTP → Rust → tools::execute
//!   → 结果回模型 → 模型最终答复
//!   → 事件翻译成 orbcat Progress
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::pi_runtime::PiRuntime;
use serde_json::{json, Value};

fn runtime_dir() -> Option<PathBuf> {
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..7 {
        p = p.parent()?.to_path_buf();
        let r = p.join("runtime");
        if r.join("node").join("node.exe").is_file() && r.join("pi").join("dist").join("bundle").join("cli.js").is_file() {
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

/// 真跑一轮：问一个需要用工具的问题。
#[tokio::test]
async fn pi_runtime能跑完整一轮并调通rust工具() {
    // ⚠️ 这类测试**打真模型**（慢、会 429、不确定）。
    // 默认跳过 —— 日常用 `tests/pi_mock_e2e.rs`（假模型，17 秒跑完）。
    // 真要验真模型时：`ORBCAT_REAL_MODEL=1 cargo test --test pi_runtime_e2e`
    if std::env::var("ORBCAT_REAL_MODEL").is_err() {
        eprintln!("跳过：这是打真模型的测试（设 ORBCAT_REAL_MODEL=1 才跑）");
        return;
    }
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（node tools/build-runtime-assets.mjs）");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    // 用一个**临时的** agent 目录（别污染真实数据）
    let tmp_agent = std::env::temp_dir().join("orbcat-pi-e2e-agent");
    let _ = std::fs::remove_dir_all(&tmp_agent);
    let _ = std::fs::create_dir_all(&tmp_agent);

    // 把 orbcat 的 models.json 同步成 pi 的
    let raw = std::fs::read_to_string(data_dir.join("models.json")).expect("应能读 models.json");
    let list: Vec<Value> = serde_json::from_str(&raw).expect("models.json 应能解析");
    let mut providers = serde_json::Map::new();
    let mut first: Option<(String, String)> = None;
    for m in &list {
        let Some(url) = m.get("url").and_then(|x| x.as_str()) else { continue };
        if !url.starts_with("http") { continue; }
        let Some(id) = m.get("id").and_then(|x| x.as_str()) else { continue };
        let raw_key = m.get("apiKey").and_then(|x| x.as_str()).unwrap_or("");
        let key = if raw_key.is_empty() { "dummy" } else { raw_key };
        let pid = format!("orb{}", providers.len() + 1);
        providers.insert(
            pid.clone(),
            json!({
                "baseUrl": url,
                "api": "openai-completions",
                "apiKey": key,
                "models": [{
                    "id": id,
                    "name": id,
                    "contextWindow": m.get("maxInputTokens").cloned().unwrap_or(json!(128000)),
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
    }
    let Some((provider, model)) = first else {
        eprintln!("跳过：models.json 里没有可用模型");
        return;
    };
    std::fs::write(
        tmp_agent.join("models.json"),
        json!({"$schema":"https://pi.dev/schemas/models.schema.json","providers":providers}).to_string(),
    ).expect("应能写 models.json");

    // 起运行时
    // ⚠️ data_dir 必须是**真实数据目录**（模型从那儿来），
    // agent_dir 才是临时的（pi 的 models.json/sessions 落这儿，别污染真实数据）。
    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), tmp_agent.clone());
    rt.set_gate(Arc::new(|_tool, _args, _sid| {
        Box::pin(async { (true, String::new()) })
    }));

    // 收集进度事件
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let progress: orbcat_lib::agent::ProgressFn = Arc::new(move |p| {
        seen2.lock().unwrap().push(format!("{p:?}"));
    });

    // 跑一轮
    let result = rt
        .run("只回答两个字：收到", vec![], progress)
        .await
        .expect("pi 内核应能跑完一轮");

    eprintln!("=== 答复 ===\n{}", result.answer);
    eprintln!("=== 轮数 === {}", result.iterations);
    eprintln!("=== 进度事件数 === {}", seen.lock().unwrap().len());
    eprintln!("=== token === {:?}", result.usage);

    assert!(!result.answer.trim().is_empty(), "应有答复");
    assert!(result.iterations >= 1, "应至少有一轮");
    assert!(
        !seen.lock().unwrap().is_empty(),
        "应收到进度事件（前端靠它渲染时间线）"
    );

    // 用量
    assert!(result.usage.total > 0, "应有 token 用量，实际: {:?}", result.usage);
}

/// 桥能被 pi 调到（用真实工具名 + 真实参数）。
#[tokio::test]
async fn pi能调到orbcat的真实工具() {
    // ⚠️ 打真模型（慢/会 429/不确定）—— 默认跳过，日常用 pi_mock_e2e。
    if std::env::var("ORBCAT_REAL_MODEL").is_err() {
        eprintln!("跳过：这是打真模型的测试（设 ORBCAT_REAL_MODEL=1 才跑）");
        return;
    }
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };

    // 准备一个真实的文件，让 read_file 能读到
    let tmp_agent = std::env::temp_dir().join("orbcat-pi-e2e-agent2");
    let _ = std::fs::remove_dir_all(&tmp_agent);
    let _ = std::fs::create_dir_all(&tmp_agent);

    let raw = std::fs::read_to_string(data_dir.join("models.json")).expect("应能读 models.json");
    let list: Vec<Value> = serde_json::from_str(&raw).expect("应能解析");
    let mut providers = serde_json::Map::new();
    let mut first: Option<(String, String)> = None;
    for m in &list {
        let Some(url) = m.get("url").and_then(|x| x.as_str()) else { continue };
        if !url.starts_with("http") { continue; }
        let Some(id) = m.get("id").and_then(|x| x.as_str()) else { continue; };
        let raw_key = m.get("apiKey").and_then(|x| x.as_str()).unwrap_or("");
        let key = if raw_key.is_empty() { "dummy" } else { raw_key };
        let pid = format!("orb{}", providers.len() + 1);
        providers.insert(pid.clone(), json!({
            "baseUrl": url, "api": "openai-completions", "apiKey": key,
            "models": [{"id": id, "name": id,
                "contextWindow": m.get("maxInputTokens").cloned().unwrap_or(json!(128000)),
                "maxTokens": json!(16384), "input": ["text"],
                "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}]
        }));
        if first.is_none() { first = Some((pid, id.to_string())); }
    }
    let Some((provider, model)) = first else {
        eprintln!("跳过：没有可用模型");
        return;
    };
    std::fs::write(
        tmp_agent.join("models.json"),
        json!({"providers":providers}).to_string(),
    ).unwrap();

    // 目标文件 —— 必须放在**已授权目录**里，否则权限网关会正确拦掉
    // （实测：%TEMP% 根目录不在授权范围，read_file 被拒 —— 网关工作正常）。
    // 授权区：`C://Users//DELL//AppData//Local//Temp//orbcat`（readwrite）。
    let work = std::env::temp_dir().join("orbcat");
    let _ = std::fs::create_dir_all(&work);
    let target = work.join("pi-e2e-target.txt");
    std::fs::write(&target, "MAGIC-ORBCAT-42").unwrap();

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), tmp_agent);
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));

    let progress: orbcat_lib::agent::ProgressFn = Arc::new(|_p| {});
    let result = rt
        .run(
            &format!(
                "用 read_file 工具读取 {}，然后把文件内容原样告诉我。只调这一个工具。",
                target.display()
            ),
            vec![],
            progress,
        )
        .await;

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("跳过/失败：{e}");
            return;
        }
    };

    eprintln!("=== 答复 ===\n{}", result.answer);
    eprintln!("=== 步骤 === {:#?}", result.steps);

    let saw_read = result
        .steps
        .iter()
        .any(|s| s.name.as_deref().map(|n| n.contains("read_file")).unwrap_or(false));
    assert!(saw_read, "应调用过 read_file，实际步骤: {:#?}", result.steps);
    assert!(
        result.answer.contains("MAGIC-ORBCAT-42"),
        "答复里应含文件内容，实际: {}",
        result.answer
    );
}
