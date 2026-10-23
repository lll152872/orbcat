//! 验证 **system prompt 被 orbcat 接管** —— pi 内核下助手的人格与记忆。
//!
//! 这条链路：
//! ```
//! pi 的 before_agent_start
//!   → orbcat-prompt.ts 向宿主 POST /prompt
//!   → 宿主调 agent::build_system_prompt_full()（与自研内核同源）
//!   → extension 返回 { systemPrompt } → pi 完全替换
//! ```
//!
//! 验收标准：模型能答出「用户叫什么」和「人格设定是什么」——
//! 这两个答案只可能来自 orbcat 的 `USER.md` / `SOUL.md`。

use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::pi_runtime::PiRuntime;
use serde_json::{json, Value};

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

/// 把 orbcat 的模型清单同步成 pi 的（格式转换 + schema 合规）。
fn sync_models(agent_dir: &std::path::Path, data_dir: &std::path::Path) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(data_dir.join("models.json")).ok()?;
    let list: Vec<Value> = serde_json::from_str(&raw).ok()?;
    let mut providers = serde_json::Map::new();
    let mut first: Option<(String, String)> = None;
    for m in &list {
        let Some(url) = m.get("url").and_then(|x| x.as_str()) else { continue };
        if !url.starts_with("http") { continue; }
        let Some(id) = m.get("id").and_then(|x| x.as_str()) else { continue; };
        let raw_key = m.get("apiKey").and_then(|x| x.as_str()).unwrap_or("");
        let key = if raw_key.is_empty() { "dummy" } else { raw_key };
        let pid = format!("orb{}", providers.len() + 1);
        providers.insert(
            pid.clone(),
            json!({
                "baseUrl": url, "api": "openai-completions", "apiKey": key,
                "models": [{
                    "id": id, "name": id,
                    "contextWindow": m.get("maxInputTokens").cloned().unwrap_or(json!(128000)),
                    "maxTokens": match m.get("maxOutputTokens").and_then(|x| x.as_u64()) {
                        Some(v) => json!(v), None => json!(16384),
                    },
                    "input": ["text"],
                    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
                }]
            }),
        );
        if first.is_none() { first = Some((pid, id.to_string())); }
    }
    let (pid, mid) = first?;
    let _ = std::fs::create_dir_all(agent_dir);
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"$schema":"https://pi.dev/schemas/models.schema.json","providers":providers}).to_string(),
    ).ok()?;
    Some((pid, mid))
}

/// pi 内核下，模型能答出 orbcat 记忆里的内容。
#[tokio::test]
async fn pi内核继承orbcat的人格与记忆() {
    // ⚠️ 这类测试**打真模型**（慢、会 429、不确定）。
    // 默认跳过 —— 日常用 `tests/pi_mock_e2e.rs`（假模型，17 秒跑完）。
    // 真要验真模型时：`ORBCAT_REAL_MODEL=1 cargo test --test pi_prompt`
    if std::env::var("ORBCAT_REAL_MODEL").is_err() {
        eprintln!("跳过：这是打真模型的测试（设 ORBCAT_REAL_MODEL=1 才跑）");
        return;
    }
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    let agent_dir = std::env::temp_dir().join("orbcat-pi-prompt-agent");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);
    let Some((_p, _m)) = sync_models(&agent_dir, &data_dir) else {
        eprintln!("跳过：没有可用模型");
        return;
    };

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir);
    // gate：一律放行（这个测试只验 prompt 接管，不碰权限）
    rt.set_gate(std::sync::Arc::new(|_t, _a| {
        Box::pin(async { (true, String::new()) })
    }));

    let progress: orbcat_lib::agent::ProgressFn = Arc::new(|_p| {});
    let result = rt
        .run(
            "用一句话回答两件事：1) 我叫什么 2) 你的人格设定是什么。不要调工具。",
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

    // orbcat 的 USER.md 里写的是「用户」，SOUL.md 里是「直言不讳」那套
    let a = result.answer.clone();
    let ok = a.contains("用户") || a.contains("直言不讳") || a.contains("私人助手");
    assert!(
        ok,
        "答复里应出现 orbcat 记忆里的内容（用户 / 人格），实际: {a}"
    );
}
