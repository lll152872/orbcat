//! **图片通道端到端测试** —— pi 内核下带图对话，图片**到底有没有送到模型**。
//!
//! ## 这个测试拦的是什么
//!
//! 2026-10-23 活体实测（随包 node 直接驱动 pi 的 RPC，假模型吃 OpenAI 兼容 SSE）查出：
//!
//! - **prompt 通道**：orbcat 送的是 **data URL 字符串**（`data:image/png;base64,AAA`），
//!   而 pi 的 `_normalizePromptImages` 只认 `{type:"image", data:<纯 base64>, mimeType}`
//!   ⇒ 取 `image.data` 得到 `undefined` ⇒ `Buffer.from(undefined,"base64")` 抛错
//!   ⇒ **整条 prompt 被拒**，错误 `The first argument must be of type string or an
//!   instance of Buffer... Received undefined`，**模型收到 0 次**。
//! - **steer 通道**：上游 `_queueSteer` **不归一化** ⇒ 形状错了**不报错**，
//!   模型收到 `data:undefined;base64,undefined` —— **静默坏图**，最难查的那种。
//!
//! ⇒ 这是**切 pi 默认前必须修掉的红线**：用户的核心场景就是带图对话。
//!
//! ## 判据：副作用，不是模型输出
//!
//! 断言落在「**假模型进程落盘的文件**」上 —— `fake_model::set_trace_file()`
//! 让假模型把每个请求体追加写进 trace 文件，测试读文件。
//!
//! 为什么不用「看模型说了什么」：
//! 模型的文字输出是**被测系统自己写出来的**，用它当判据 = 让被测者给自己打分。
//! 落盘文件是**外部进程留下的痕迹**，是独立证据。
//!
//! （同一个思路：`fake_mcp_server.cjs` 的 `MCP_TRACE_FILE`；
//! 测「pi 内置 bash 绕权限」也是用**探针文件**判成败。）
//!
//! ## 断言什么
//!
//! 1. 带图的一轮**不报错**（修复前这里直接 `Received undefined`）
//! 2. trace 文件里有**我们发的那串 base64**（图原样到了模型）
//! 3. trace 文件里**没有 `undefined`**（不是静默坏图）
//! 4. 不带图的对照轮照常跑通、请求里没有图片段（证明断言不会误判）

#[path = "fake_model.rs"]
mod fake_model;

use std::path::PathBuf;
use std::sync::Arc;

use orbcat_lib::agent::ProgressFn;
use orbcat_lib::pi_runtime::PiRuntime;
use serde_json::json;

/// 1x1 PNG 的 base64（**不带** `data:` 前缀）。
const PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// orbcat 前端给的就是这个形状（`pendingImages.push(dataUrl)`）。
fn data_url() -> String {
    format!("data:image/png;base64,{PNG_B64}")
}

/// 找随包的 runtime/（node + pi bundle）。
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

/// 找 orbcat 的数据目录（里面有 models.json）。
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

/// 起一套「假模型 + pi 运行时」，返回（假模型、运行时、trace 文件路径）。
async fn setup(
    tag: &str,
    delay_ms: u64,
) -> Option<(Arc<fake_model::FakeModel>, Arc<PiRuntime>, PathBuf, PathBuf)> {
    let runtime = runtime_dir()?;
    let data_dir = orbcat_data_dir()?;

    let work = std::env::temp_dir().join("orbcat");
    let _ = std::fs::create_dir_all(&work);
    // 假模型第一轮要调 orb_read_file ⇒ 得有个真文件可读
    let target = work.join(format!("pi-image-target-{tag}.txt"));
    std::fs::write(&target, "MAGIC-ORBCAT-IMAGE").expect("应能写目标文件");

    // 副作用判据的载体：假模型把请求体写这儿
    let trace = work.join(format!("pi-image-trace-{tag}.jsonl"));
    let _ = std::fs::remove_file(&trace);

    let fake = fake_model::FakeModel::start(
        "read_file",
        json!({ "path": target.display().to_string() }),
        "看到图了",
    )
    .expect("假模型应能起");
    fake.set_trace_file(&trace);
    if delay_ms > 0 {
        fake.set_delay_ms(delay_ms);
    }

    let agent_dir = std::env::temp_dir().join(format!("orbcat-pi-image-agent-{tag}"));
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::create_dir_all(&agent_dir);

    let rt = PiRuntime::with_agent_dir(data_dir.clone(), runtime.clone(), agent_dir.clone());
    // ⚠️ **必须把 provider 指到假模型** —— 否则 pi 会去连 orbcat models.json 里的
    // **真模型**（要真 API key、会重试、会超时），测试就成了「测网络」。
    // （踩过一次：漏了这行，pi 去找真模型，trace 文件全空、镜像测试卡 120s。）
    //
    // ⚠️ **必须声明支持图片** —— pi 按 `model.input` 能力决定要不要把图发给模型
    // （上游 `openai-completions` chunk：`hasImages && model.input.includes("image")`）
    // ⇒ 声明只有 `["text"]` 的话，**图会被静默丢掉**（踩过一次：trace 里有请求，
    // 但里面没有 base64）。
    rt.set_model_override_with_images("fake-model", fake.base_url())
        .await;
    // 工具一律放行（这个测试不测权限）
    rt.set_gate(Arc::new(|_t, _a, _sid| {
        Box::pin(async { (true, String::new()) })
    }));
    Some((fake, rt, trace, target))
}

/// 读 trace 文件（副作用判据的读法）。
fn trace_body(trace: &PathBuf) -> String {
    std::fs::read_to_string(trace).unwrap_or_default()
}

/// 🔴 红线测试：**带图的一轮必须跑通，且图原样送到模型**。
#[tokio::test]
async fn 带图对话_图原样送到模型_且不是undefined() {
    let Some((_fake, rt, trace, _target)) = setup("prompt", 0).await else {
        eprintln!("跳过：runtime/ 或 agent-data/ 不在（先构建 runtime）");
        return;
    };

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        rt.run("看这张图", vec![data_url()], progress),
    )
    .await;

    match r {
        Err(_) => panic!("带图对话超时（120s）—— 正常应该几秒"),
        // 修复前就是这里炸：pi 回 "The first argument must be of type string...
        // Received undefined"（整个 prompt 被拒，模型一次都没被调用）
        Ok(Err(e)) => panic!("带图对话报错: {e}"),
        Ok(Ok(_)) => {}
    }

    // ── 副作用判据：读假模型落盘的请求体 ──
    let body = trace_body(&trace);
    assert!(
        !body.is_empty(),
        "假模型应留下请求体落盘文件（trace）—— 模型一次都没被调用？"
    );
    assert!(
        body.contains(PNG_B64),
        "图片 base64 应原样出现在模型请求里。\ntrace: {}",
        &body[..body.len().min(600)]
    );
    assert!(
        !body.contains("data:undefined") && !body.contains("base64,undefined"),
        "不能出现 `data:undefined` —— 那是 pi 收到坏形状后静默造出来的坏图"
    );
    assert!(
        body.contains("image_url"),
        "模型请求里应带 image_url 段（OpenAI 兼容多模态格式）"
    );
}

/// 对照组：**不带图**照常跑通，且请求里没有图片段。
///
/// 作用：证明上面的断言不是「碰巧成立」——没有图时本来就不该有图。
#[tokio::test]
async fn 不带图的对话_照常跑通_且请求里没有图片() {
    let Some((_fake, rt, trace, _target)) = setup("plain", 0).await else {
        eprintln!("跳过：runtime/ 或 agent-data/ 不在（先构建 runtime）");
        return;
    };

    let progress: ProgressFn = Arc::new(|_p| {});
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        rt.run("你好。", vec![], progress),
    )
    .await;

    match r {
        Err(_) => panic!("不带图的对话超时（120s）"),
        Ok(Err(e)) => panic!("不带图的对话报错: {e}"),
        Ok(Ok(_)) => {}
    }

    let body = trace_body(&trace);
    assert!(!body.is_empty(), "假模型应留下请求体落盘文件");
    assert!(
        !body.contains("image_url"),
        "不带图就不该有 image_url 段"
    );
}

/// **插话带图** —— 图也要原样送到模型。
///
/// 判据同样是落盘文件：插话排到下一轮，那一轮的请求体里应出现图片。
#[tokio::test]
async fn steer带图_图也原样送到模型() {
    // 假模型慢一点 ⇒ agent 处于 streaming ⇒ 插话能插进去
    let Some((_fake, rt, trace, _target)) = setup("steer", 2500).await else {
        eprintln!("跳过：runtime/ 或 agent-data/ 不在（先构建 runtime）");
        return;
    };

    let rt2 = rt.clone();
    let runner = tokio::spawn(async move {
        let progress: ProgressFn = Arc::new(|_p| {});
        rt2.run("先说点什么", vec![], progress).await
    });

    // 等 agent 真的开跑
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let st = rt
        .steer("插话带图", vec![data_url()])
        .await;
    assert!(st.is_ok(), "插话应成功: {:?}", st.err());

    let _ = tokio::time::timeout(std::time::Duration::from_secs(120), runner).await;

    let body = trace_body(&trace);
    assert!(
        body.contains(PNG_B64),
        "插话里的图也应原样送到模型。\ntrace: {}",
        &body[..body.len().min(600)]
    );
    assert!(
        !body.contains("data:undefined") && !body.contains("base64,undefined"),
        "不能出现 `data:undefined` —— 静默坏图"
    );
}
