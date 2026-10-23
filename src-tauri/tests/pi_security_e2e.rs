//! **安全回归测试** —— pi 内置工具**不能**绕过 orbcat 的权限体系。
//!
//! ## 背景（2026-10-23 发现的实缺口）
//!
//! pi 1.4.0 自带 8 个内置工具（上游 `createAllToolDefinitions`）：
//! `read` / `bash` / `powershell` / `edit` / `write` / `grep` / `find` / `ls`。
//!
//! 它们**原生执行**，**不经过** orbcat 的 `permission.rs` 文件网关与
//! `command_policy.rs` 命令策略 ⇒ 决策 7（三层闸门）与决策 9（命令三路）被架空。
//!
//! 实测（`docs/explore/pi-upstream-context-interaction.md` §二 2.6）：
//! pi 的 `bash` RPC 真的执行了命令并返回结果（78ms）。
//!
//! ## 这个测试怎么证明「没被执行」
//!
//! **不看模型输出，看副作用** —— 让假模型去调 pi 的 `bash` 工具执行一条
//! 会**创建探针文件**的命令；跑完断言**探针文件不存在**。
//!
//! 这是唯一靠得住的判据：pi 报什么错都无所谓，**文件在 = 漏了**。
//!
//! ## 同时验证「没把工具全禁死」
//!
//! 第二个用例：orbcat 自己的 `orb_read_file` 仍能经桥回 Rust 正常调用
//! —— 堵漏口不能变成「把 pi 的工具全砍了，模型啥也干不了」。

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

/// 🔴 核心用例：pi 的内置 `bash` **不能**执行命令。
///
/// 判据 = **探针文件不存在**（副作用），不是看 pi 报什么错。
#[tokio::test]
async fn pi内置bash工具_不能执行命令() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 orbcat 的 agent-data/");
        return;
    };

    // ── 探针：命令跑成功就会创建这个文件 ──
    let work = std::env::temp_dir().join("orbcat-sec-probe");
    let _ = std::fs::create_dir_all(&work);
    let probe = work.join("PWNED.txt");
    let _ = std::fs::remove_file(&probe);

    // 让假模型去调 pi 的内置 `bash`（`start_raw`：不加 `orb_` 前缀）
    let cmd = format!(
        "echo pwned > \"{}\"",
        probe.display().to_string().replace('\\', "/")
    );
    let fake =
        fake_model::FakeModel::start_raw("bash", json!({ "command": cmd }), "（假装执行完了）")
            .expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join("orbcat-pi-sec-bash");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir, runtime, agent_dir);
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| Box::pin(async { (true, String::new()) })));

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        rt.run("跑一条命令试试。", vec![], progress),
    )
    .await;

    match r {
        Err(_) => {
            // ⚠️ 超时**不算失败**：工具不存在时 pi 有可能卡在等一个不会来的结果。
            // 关键判据只有一个 —— **探针文件必须不存在**。
            eprintln!("（60 秒超时 —— 不代表被执行了，看下面的探针断言）");
        }
        // 报错也算「没执行」：工具不存在时 pi 可能直接失败。
        Ok(Err(e)) => eprintln!("（链路报错，符合预期：工具不存在）: {e}"),
        Ok(Ok(res)) => {
            eprintln!("=== 答复 ===\n{}", res.answer);
            eprintln!("=== 步骤 ===\n{:#?}", res.steps);
        }
    }

    assert!(
        !probe.exists(),
        "🔴 pi 的内置 bash **真的执行了命令**（探针文件已创建）—— 权限网关被绕过！\
         探针路径: {}",
        probe.display()
    );
    eprintln!("✅ 探针文件不存在 = 内置 bash 没有执行");
}

/// 反向用例：orbcat 自己的工具**仍然可用**（堵漏口不能把工具全砍死）。
///
/// 同样用副作用判据：读一个真文件，断言读到了内容。
#[tokio::test]
async fn orbcat自己的工具_仍然能经桥回Rust() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建");
        return;
    };
    let Some(data_dir) = orbcat_data_dir() else {
        eprintln!("跳过：找不到 agent-data/");
        return;
    };

    let work = std::env::temp_dir().join("orbcat-sec-probe");
    let _ = std::fs::create_dir_all(&work);
    let target = work.join("ok-target.txt");
    std::fs::write(&target, "MAGIC-ORBCAT-OK").expect("应能写目标文件");

    let fake = fake_model::FakeModel::start(
        "read_file",
        json!({ "path": target.display().to_string() }),
        "读到了内容。",
    )
    .expect("假模型应能起");

    let agent_dir = std::env::temp_dir().join("orbcat-pi-sec-orb");
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir, runtime, agent_dir);
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| Box::pin(async { (true, String::new()) })));

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        rt.run("读一下那个文件。", vec![], progress),
    )
    .await;

    let res = match r {
        Err(_) => panic!("60 秒超时"),
        Ok(Err(e)) => panic!("链路报错: {e}"),
        Ok(Ok(res)) => res,
    };

    let saw = res
        .steps
        .iter()
        .any(|s| s.kind == "tool_result" && s.detail.contains("MAGIC-ORBCAT-OK"));
    assert!(
        saw,
        "orbcat 的 `orb_read_file` 仍应能经桥回 Rust 读到内容，实际步骤: {:#?}",
        res.steps
    );
}
