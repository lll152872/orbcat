//! **L1 项目脚本工具 e2e** —— 「配个项目工具 → pi 调到它」整条链真跑通。
//!
//! ## 补的是什么缺口
//!
//! 代码路径早就是通的（`tools::bridge_specs_for_dir` 收集项目工具 →
//! `runtime-ext/orbcat-tools.ts` 注册成 `orb_<name>` → 桥回 Rust 的
//! `tools::execute_bridge` → `execute` 的 `other` 分支 → `run_project_tool`）。
//!
//! 但**没有一条测试**证明「真的配了项目工具 → pi 真的调到了它 → 脚本真的跑了」。
//! 风险很具体：项目 bundle 的 `commandTemplate` 展开、argv 级占位符替换、
//! 或者 `orb_` 前缀对不上 —— 任何一环错了，代码路径「看着通」但实际是死的。
//!
//! ## 判据：看副作用，不看模型输出
//!
//! 脚本会**写一个标记文件**。跑完断言：
//! - 标记文件**存在** ⇒ 脚本真的被执行了（不是「参数展开失败但静默返回」）
//! - 工具结果里含脚本的输出 ⇒ 结果真的回传到了模型
//!
//! ## 为什么用临时数据目录
//!
//! 用户的 `agent-data/` 是真实数据（模型 key、记忆、项目）。测试**不许**往里写。
//! 所以这里建一个临时 `data_dir`，只放**测试自己要的**东西：
//! - `models.json` —— 指向**假模型**（不碰真模型，也不泄漏 key）
//! - `settings.json` —— 激活项目 + `execTrust: full`（免问，否则弹卡等到超时）
//! - `projects/p/project.json` + 脚本 —— 本测试的主角
//!
//! ## 为什么要自己写 `settings.json`
//!
//! `config` / `project` 都是**私有模块**（`mod config;` / `mod project;`），
//! 集成测试（`tests/*.rs`）这个外部 crate 调不到。
//! 但它们读的就是这两个普通文件 ⇒ 直接按 serde 的 camelCase 口径手写即可：
//! `AgentSettings` 是 `rename_all = "camelCase"`、`ExecTrust` 也是。

#[path = "fake_model.rs"]
mod fake_model;

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

/// 造一个临时数据目录，带一个 L1 项目脚本工具。
///
/// 返回 `(data_dir, 标记文件路径)`。
fn setup_l1_project(fake_base_url: &str, tool_name: &str) -> (PathBuf, PathBuf) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_nanos())
        .unwrap_or(0);
    let d = unique_tmp("l1e2e");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("应能建临时数据目录");

    // ── models.json：只指向假模型（不碰真模型、不写真 key）──
    std::fs::write(
        d.join("models.json"),
        serde_json::to_string_pretty(&json!([{
            "id": "fake-model",
            "vendor": "test",
            "url": fake_base_url,
            "apiKey": "dummy",
            "supportsToolCall": true,
            "supportsImages": false,
            "maxInputTokens": 128000,
            "maxOutputTokens": 16384
        }]))
        .unwrap(),
    )
    .expect("应能写 models.json");

    // ── settings.json：激活项目 + 免问 ──
    // `execTrust: full` ⇒ 免弹卡（硬阻断仍生效）。
    std::fs::write(
        d.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "activeBundle": "p",
            "execTrust": "full"
        }))
        .unwrap(),
    )
    .expect("应能写 settings.json");

    // ── 项目包 ──
    let bundle = d.join("projects").join("p");
    std::fs::create_dir_all(bundle.join("tools")).expect("应能建项目目录");

    // 标记文件：脚本执行后会写它
    let marker = unique_tmp("l1-marker").with_extension("txt");
    let _ = std::fs::remove_file(&marker);

    // 脚本：把参数写到标记文件 + 回显
    let script = format!(
        r#"param([string]$msg)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Set-Content -Path '{}' -Value ("RAN:" + $msg) -Encoding UTF8
Write-Output ("SCRIPT-OK:" + $msg)
"#,
        marker.to_string_lossy().replace('\'', "''")
    );
    std::fs::write(bundle.join("tools").join("probe.ps1"), script).expect("应能写脚本");

    // bundle 清单
    // `commandTemplate` 是 **argv 数组**（不是整串）—— 数组形态下占位符替换
    // 不会引入 shell 语义。`-File` 后面跟脚本路径，**不含** `{param.*}` ⇒
    // 不会触发 bundle 校验里的「内联解释器注入」检查。
    let template = json!([
        interpreter(),
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "{project}/tools/probe.ps1",
        "-msg",
        "{param.msg}"
    ]);
    let manifest = json!({
        "schemaVersion": 1,
        "name": "L1 e2e 测试项目",
        "tools": [{
            "name": tool_name,
            "description": "写一个标记文件并回显参数",
            "params": {"type":"object","properties":{"msg":{"type":"string"}}},
            "commandTemplate": template
        }]
    });
    std::fs::write(
        bundle.join("project.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("应能写 project.json");

    (d, marker)
}

/// 找一个能用的 PowerShell 解释器。
///
/// 优先 pwsh（PS7），没有就退到 powershell.exe（PS 5.1，Windows 自带）。
/// 与 `shell::resolve_interpreter()` 同口径，但那个是私有模块，测试调不到。
fn interpreter() -> String {
    let pwsh = Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe");
    if pwsh.is_file() {
        return pwsh.to_string_lossy().to_string();
    }
    "powershell.exe".to_string()
}

/// 端到端：配好项目工具 → pi 调到它 → 脚本真的跑了 → 结果回到模型。
#[tokio::test]
async fn 项目脚本工具_pi能调到并真的执行() {
    let Some(runtime) = runtime_dir() else {
        eprintln!("跳过：runtime/ 未构建（先跑 node tools/build-runtime-assets.mjs）");
        return;
    };

    let tool_name = "l1_probe";
    let msg = "hello-from-pi";

    // ── 假模型：第一轮让模型调 `orb_l1_probe`，第二轮给答复 ──
    let fake = fake_model::FakeModel::start(
        tool_name,
        json!({ "msg": msg }),
        "跑完了。",
    )
    .expect("假模型应能起");

    let (data_dir, marker) = setup_l1_project(&fake.base_url(), tool_name);

    let agent_dir = std::env::temp_dir().join(format!(
        "orbcat-l1e2e-agent-{}",
        data_dir.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime, agent_dir);
    // 显式指定假模型（否则会挑 models.json 里的第一个 = 假模型那条，但
    // 显式指定更稳，不依赖「只有一个 provider」这个前提）
    rt.set_model_override("fake-model", fake.base_url()).await;
    rt.set_gate(Arc::new(|_t, _a, _sid| Box::pin(async { (true, String::new()) })));

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        rt.run("跑一下那个项目工具。", vec![], progress),
    )
    .await;

    let res = match r {
        Err(_) => panic!("90 秒超时 —— 链路卡住了"),
        Ok(Err(e)) => {
            eprintln!("链路报错: {e}");
            eprintln!("=== 步骤 ===");
            panic!("链路报错: {e}")
        }
        Ok(Ok(res)) => res,
    };

    eprintln!("=== 最终答复 ===\n{}", res.answer);
    eprintln!("=== 步骤 ===\n{:#?}", res.steps);

    // ── 验收 1：假模型被调了 2 次（第一次给工具调用，第二次给答复）──
    assert_eq!(
        fake.call_count(),
        2,
        "假模型应被调 2 次，实际 {} 次",
        fake.call_count()
    );

    // ── 验收 2：脚本**真的执行了**（副作用判据）──
    assert!(
        marker.exists(),
        "🔴 项目脚本工具没有执行 —— 标记文件没生成。\
         data_dir = {}",
        data_dir.display()
    );
    let content = std::fs::read_to_string(&marker).expect("应能读标记文件");
    assert!(
        content.contains(msg),
        "标记文件内容应含参数「{msg}」，实际: {content:?}"
    );

    // ── 验收 3：工具结果回到了模型这一侧 ──
    let saw = res
        .steps
        .iter()
        .any(|s| s.kind == "tool_result" && s.detail.contains("SCRIPT-OK"));
    assert!(
        saw,
        "工具结果应含脚本回显「SCRIPT-OK」（说明结果真的传回来了），实际步骤: {:#?}",
        res.steps
    );

    // ── 清理 ──
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_dir_all(&data_dir);
}
