//! Agent loop
//!
//! 循环体：
//! ```text
//!   LLM 调用
//!     ├─ 返回纯文本        → 结束，这就是答案
//!     └─ 返回 tool_calls   → 逐个执行 → 结果回灌 → 再调用
//! ```
//!
//! 有最大轮数兜底，防止模型陷入"调用-失败-再调用"的死循环。

use std::path::Path;

use serde::Serialize;

use crate::config::ModelConfig;
use crate::llm::{self, ChatMessage};
use crate::permission::PermissionGate;
use crate::tools;

/// 最大循环轮数
const MAX_ITERATIONS: usize = 12;

/// 单条工具结果回灌给模型时的截断长度
const TOOL_RESULT_LIMIT: usize = 24 * 1024;

// ---------------------------------------------------------------------------
// 进度上报
// ---------------------------------------------------------------------------

/// Agent 执行过程中的关键节点。
///
/// 为什么要这个：agent loop 一轮可能跑 30-60 秒（多次模型调用 + 工具执行），
/// 中间是**完全黑盒**的 —— 用户只看到空白，然后答案突然出现。
/// 把这些节点推给前端，就能实时显示「正在思考 / 调用了什么工具 / 工具返回了什么」。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Progress {
    /// 开始第 n 轮模型调用（模型在"思考"）
    Thinking { iteration: usize },
    /// **流式增量**：正文或思维链的一小段
    Delta {
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    /// 模型决定调用某个工具
    ToolCall { name: String, args: String },
    /// 工具返回了
    ToolResult { name: String, preview: String },
    /// 工具执行失败
    ToolError { name: String, message: String },
    /// 准备输出最终答案（流式下已经边收边发了，这里只作为收尾信号）
    Answering,
    /// 本轮流了正文出来，但模型最终决定去调工具 —— 前端把那半截话淡化掉
    DiscardStream { reason: String },
}

/// 进度回调。用 Arc<dyn Fn> 而不是泛型，避免把泛型参数污染到整个 agent loop。
pub type ProgressFn = std::sync::Arc<dyn Fn(Progress) + Send + Sync>;

/// 什么都不做的回调（测试用）
pub fn no_progress() -> ProgressFn {
    std::sync::Arc::new(|_| {})
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStep {
    /// `assistant` | `tool_call` | `tool_result`
    pub kind: String,
    pub name: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRun {
    pub answer: String,
    pub steps: Vec<AgentStep>,
    /// 实际用了几轮
    pub iterations: usize,
}

/// 跑一轮完整对话。
///
/// - `images` 用户附带的图片（`data:image/png;base64,...`），可为空
/// - `gate` 由调用方 clone 后传入 —— 因为 `MutexGuard` 不能跨 await，
///   而 `PermissionGate` 本身是 `Clone` 的
/// - `mcp` MCP 工具注册表；工具列表**每轮重建**，这样模型中途
///   `load_tool_group` 加载的新组能在下一轮立即出现
/// - `on_progress` 进度回调，用来把「在思考 / 调什么工具」实时推给前端
/// - `session_id` 当前会话 id；用来回灌历史（`history::build_history`）。
///   传空串则不回灌（无会话上下文，等价于旧行为）
#[allow(clippy::too_many_arguments)]
pub async fn run(
    cfg: &ModelConfig,
    gate: &PermissionGate,
    data_dir: &Path,
    mcp: &tokio::sync::Mutex<crate::mcp::ToolRegistry>,
    user_input: &str,
    images: Vec<String>,
    on_progress: ProgressFn,
    foreground: Option<&crate::context::ForegroundContext>,
    cancel: &std::sync::atomic::AtomicBool,
    session_id: &str,
) -> Result<AgentRun, String> {
    use std::sync::atomic::Ordering;

    let mut system_prompt = build_system_prompt(data_dir);
    // 前台上下文是**每轮都变**的动态内容 → 必须 append 在最后，
    // 绝不能插到 system prompt 中间（那会让它后面全部缓存失效）。
    // 详见 build_system_prompt 的「前缀稳定性」文档。
    if let Some(fg) = foreground {
        system_prompt.push_str(&format!(
            "\n\n---\n\n# 用户当前正在看什么（实时采集）\n\n\
             {}\n\
             若用户说「这个」「这里」「我看的这个目录」，优先结合上述上下文理解；\
             需要目录内容时用文件工具去读（仍受权限网关管辖）。\
             该信息可能过期，拿不准时调用 foreground_context 工具重新获取。",
            fg.summary()
        ));
    }

    let first_user = build_user_message(cfg, data_dir, user_input, &images);

    // ---- 多轮上下文回灌（用户 2026-09-19 定案）----
    // 规则：(最近 6h 内的消息) ∪ (最近 10 条)，见 history.rs 文档。
    //
    // ⚠️ 时序关键：必须在 lib.rs::chat 调 `sessions::append_turn` **之前**读，
    //    否则当前这轮会被算进历史，模型看到自己的问题重复出现。
    let history = if session_id.is_empty() {
        Vec::new()
    } else {
        crate::history::build_history(data_dir, session_id, now_ms())
    };
    let history_len = history.len();

    let mut messages = vec![ChatMessage::system(system_prompt)];
    messages.extend(history);
    messages.push(first_user);

    eprintln!(
        "[float-agent] 上下文回灌: 历史 {history_len} 条 + 本轮输入 1 条（会话 {session_id}）"
    );

    let mut steps: Vec<AgentStep> = Vec::new();

    for iter in 1..=MAX_ITERATIONS {
        // 取消检查点 ①：每轮开始。用户点「停止」后最迟在当前 LLM 调用
        // 返回后的下一轮退出（流式内部无法中断，这是 v1 的延迟上界）。
        if cancel.load(Ordering::Relaxed) {
            return Err("已停止".into());
        }
        // 每轮重建工具列表 —— 模型可能刚 load 了新组
        let tools_arg = if cfg.supports_tool_call {
            let specs = {
                let reg = mcp.lock().await;
                tools::tool_specs(Some(&reg))
            };
            Some(llm::tools_to_openai(&specs))
        } else {
            None
        };

        on_progress(Progress::Thinking { iteration: iter });

        // 流式：正文边收边推给前端（方案 C）
        // 用 Arc<AtomicBool> 而非裸引用 —— 回调是 'static 约束的 trait object，
        // 借局部变量的闭包活不够长
        let streamed_this_round = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delta_cb = {
            let cb = on_progress.clone();
            let flag = streamed_this_round.clone();
            move |text: Option<String>, reasoning: Option<String>| {
                if text.is_some() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                cb(Progress::Delta { text, reasoning });
            }
        };

        let out = llm::chat_stream(cfg, messages.clone(), tools_arg, &delta_cb, cancel).await?;
        let calls: Vec<llm::ToolCall> = out.tool_calls().to_vec();

        // --- 没有工具调用：这就是最终答案（流式内容已经在界面上）---
        if calls.is_empty() {
            let text = out.text();
            on_progress(Progress::Answering);
            steps.push(AgentStep {
                kind: "assistant".into(),
                name: None,
                detail: text.clone(),
            });
            return Ok(AgentRun {
                answer: text,
                steps,
                iterations: iter,
            });
        }

        // --- 有工具调用：本轮若已流过正文，告诉前端把那半截话淡化掉 ---
        // （方案 C 的代价：模型偶尔会先吐半句再决定调工具）
        if streamed_this_round.load(std::sync::atomic::Ordering::Relaxed) {
            on_progress(Progress::DiscardStream {
                reason: "模型中途决定调用工具，以下内容不是最终答复".into(),
            });
        }

        // --- 有工具调用：先把 assistant 这条（含 tool_calls）加进历史 ---
        messages.push(out.message.clone());

        // 截图类工具产生的图片先收集起来，等所有 tool 结果回灌完再统一插入 ——
        // 避免把 user 消息夹在 assistant(tool_calls) 与 tool 之间，那会让部分厂商 API 报错
        let mut pending_images: Vec<String> = Vec::new();

        for call in calls {
            let args: serde_json::Value =
                serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);

            on_progress(Progress::ToolCall {
                name: call.function.name.clone(),
                args: summarize_args(&call.function.arguments),
            });

            steps.push(AgentStep {
                kind: "tool_call".into(),
                name: Some(call.function.name.clone()),
                detail: call.function.arguments.clone(),
            });

            let output = {
                let ctx = tools::ToolCtx {
                    gate,
                    data_dir,
                    mcp,
                    session_id,
                };
                match tools::execute(&call.function.name, &args, &ctx).await {
                    Ok(o) => o,
                    Err(e) => {
                        on_progress(Progress::ToolError {
                            name: call.function.name.clone(),
                            message: e.chars().take(200).collect(),
                        });
                        tools::ToolOutput::text(format!("工具执行失败：{e}"))
                    }
                }
            };

            let preview: String = output.text.chars().take(400).collect();
            if !output.text.starts_with("工具执行失败") {
                on_progress(Progress::ToolResult {
                    name: call.function.name.clone(),
                    preview: preview.clone(),
                });
            }
            steps.push(AgentStep {
                kind: "tool_result".into(),
                name: Some(call.function.name.clone()),
                detail: preview,
            });

            // 回灌时截断，避免撑爆上下文
            let for_model: String = output.text.chars().take(TOOL_RESULT_LIMIT).collect();
            messages.push(ChatMessage::tool_result(call.id.clone(), for_model));

            if let Some(img) = output.image {
                pending_images.push(img);
            }
        }

        // 截图产生的画面：作为 user 消息附进去，模型下一轮就能"看到"
        if !pending_images.is_empty() {
            messages.push(build_user_message(
                cfg,
                data_dir,
                "（以上是刚截取的屏幕画面）",
                &pending_images,
            ));
        }

        // 取消检查点 ②：工具都执行完、还没发下一轮 LLM 请求 —— 在这里停最省
        if cancel.load(Ordering::Relaxed) {
            return Err("已停止".into());
        }
    }

    Err(format!(
        "已连续调用工具 {MAX_ITERATIONS} 轮仍未给出结论，已中止（可能是任务过大或模型陷入循环）"
    ))
}

/// 组装一条带图（或带图占位）的 user 消息。
///
/// **图片一律以文件路径传入**（落盘由 `images::save_all` 在入口完成），
/// 这里按模型能力决定怎么递送：
/// - `supports_images = true` → 读文件转 base64，多部分 content（模型真能看到图）
/// - `supports_images = false` → 只把路径写成占位文本，并明确告知模型"你看不到图"
///
/// 为什么不能无脑发 base64：纯文本模型收到图像 part 要么 400，要么把 base64
/// 当文本 token 吃进去（爆 token 且回答跑偏），更糟的是**静默忽略**——模型会
/// 对着空气编造内容。显式告知它"有图但看不到"，才能得到正确的引导式回答。
fn build_user_message(
    cfg: &ModelConfig,
    data_dir: &Path,
    text: &str,
    images: &[String],
) -> ChatMessage {
    if images.is_empty() {
        return ChatMessage::user(text);
    }

    if cfg.supports_images {
        // 转 base64；个别图读失败/过大就跳过它，但把情况告诉模型
        let mut urls: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for p in images {
            match crate::images::to_data_url(p) {
                Ok(u) => urls.push(u),
                Err(e) => {
                    eprintln!("[float-agent] 图片内联失败: {e}");
                    failed.push(p.clone());
                }
            }
        }
        if urls.is_empty() {
            // 全失败 → 退化成占位，别发一条空的多部分消息
            return ChatMessage::user(format!(
                "{text}{}",
                crate::images::placeholder_note(images, data_dir)
            ));
        }
        let mut body = text.to_string();
        if !failed.is_empty() {
            body.push_str(&format!(
                "\n\n（有 {} 张图因过大或读取失败未能附上）",
                failed.len()
            ));
        }
        return ChatMessage::user_with_images(body, urls);
    }

    ChatMessage::user(format!(
        "{text}{}",
        crate::images::placeholder_note(images, data_dir)
    ))
}

/// 把工具参数压成一行短摘要，供进度条展示
fn summarize_args(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.chars().take(80).collect();
    };
    let Some(obj) = v.as_object() else {
        return raw.chars().take(80).collect();
    };
    // 优先展示这些有信息量的键
    for key in ["path", "pattern", "name", "url", "cmd", "command", "sql", "content"] {
        if let Some(val) = obj.get(key).and_then(|x| x.as_str()) {
            return format!("{key}={}", val.chars().take(60).collect::<String>());
        }
    }
    if obj.is_empty() {
        "（无参数）".into()
    } else {
        raw.chars().take(80).collect()
    }
}

// ---------------------------------------------------------------------------
// system prompt 组装
// ---------------------------------------------------------------------------

/// 从 `agent-data/` 组装 system prompt。
///
/// ## 顺序 = 前缀稳定性（prompt cache 纪律）
///
/// 服务商的 prompt cache 按 **token 前缀逐字节比对**：任何一处变动，
/// 它**后面**的全部 token 都要按未命中价重付。所以这里的排版规则是：
///
/// 1. **静态的在前** —— 除非改代码，否则永不变化
/// 2. **动态的在后** —— 按"变化频率"从低到高排列
/// 3. **规则文字与它的数据拆开** —— 规则是静态的（前移），
///    数据是动态的（后移）。早先两者混写，导致"加一个技能"
///    会连带让后面的 MCP 说明、对话历史说明整段失效。
///
/// ```text
/// ── 稳定前缀（改代码才动）──────────────────
///   RULES.md        行为红线（最高优先级）
///   SOUL.md         人格 / 语气
///   IDENTITY.md     称呼约定
///   USER.md         用户画像
///   MEMORY.md       长期记忆
///   运行环境（纯规则文字，不含工作目录值）
///   MCP 机制（纯规则文字，不含组状态）
///   对话历史机制（纯规则文字，不含任何动态值）
/// ── 动态尾部（按变化频率递增）──────────────
///   技能清单        加技能才变
///   MCP 已加载组     加载/卸载组才变
///   工作目录        进程内恒定，但仍是"求值"得来，放最后
///   （前台上下文由 agent::run 追加，每轮都变 → 绝对最尾）
/// ```
pub fn build_system_prompt(data_dir: &Path) -> String {
    let mut stable: Vec<String> = Vec::new();
    let mut dynamic: Vec<String> = Vec::new();

    let read = |name: &str| -> Option<String> {
        let p = data_dir.join(name);
        std::fs::read_to_string(&p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if let Some(r) = read("RULES.md") {
        stable.push(format!(
            "# ⚠️ 行为红线（最高优先级，任何情况下不得违反）\n\n{r}"
        ));
    }
    if let Some(s) = read("SOUL.md") {
        stable.push(format!("# 你的人格与语气\n\n{s}"));
    }
    if let Some(i) = read("IDENTITY.md") {
        stable.push(format!("# 你的身份\n\n{i}"));
    }
    if let Some(u) = read("USER.md") {
        stable.push(format!("# 关于用户\n\n{u}"));
    }
    if let Some(m) = read("memory/MEMORY.md") {
        stable.push(format!("# 长期记忆\n\n{m}"));
    }

    // ── 未初始化 / 人格缺失 ─────────────────────────────────────────────
    // agent-data 不存在 = 全新克隆；目录建好了但四个 persona 文件不全 =
    // 初始化第二段没做完。两种情况模型都是"没有人格的"，必须告诉它该干什么。
    //
    // ⚠️ 关键设计：**建目录这件事交给 Rust 代码（bootstrap_data 命令），
    //    不交给模型**。「让 agent 自己 mkdir」有个死锁：要跑 agent 得先有
    //    模型配置，而模型配置就存在 agent-data 里 —— 没目录就没配置，
    //    没配置就跑不了 agent，跑不了 agent 就建不了目录。闭环。
    //    所以初始化拆成两段：
    //      ① 纯 Rust：建目录 + 写空 models.json（不碰模型），
    //         用户在设置面板加完模型，闭环才打开；
    //      ② 模型就绪后的对话：agent 读 `初始化.md`，
    //         补齐 RULES/SOUL/IDENTITY/USER —— 这些必须问用户才写得出。
    //    下面这段 prompt 负责第二段。
    //
    // 前缀稳定性：这个条件在一次安装内最多翻转四次（每建一个 persona
    // 文件翻转一次缓存）—— 一次性成本，可接受。
    let missing_persona = ["RULES.md", "SOUL.md", "IDENTITY.md", "USER.md"]
        .iter()
        .any(|f| !data_dir.join(f).exists());
    if missing_persona {
        let dir_note = if !data_dir.exists() {
            "注意：数据目录本身还不存在。这种情况你**什么都做不了**\n（没有模型你也收不到消息）。界面上会有「建出目录结构」按钮，\n引导用户去点它 —— **不要试图自己建目录**。"
        } else {
            "数据目录已存在，缺的是下面这几个定义「你是谁」的文件。"
        };
        stable.push(format!(
            "# 初始化未完成\n\n\
             你的人格文件不全：`RULES.md` / `SOUL.md` / `IDENTITY.md` /\n\
             `USER.md` —— 这四个定义你是谁，现在缺其中一些。\n\
             {dir_note}\n\n\
             当用户说「**初始化**」或明显想把你配置起来时：\n\
             1. 读项目根的 `初始化.md`（和 `agent-data/` 同级），\n\
                它写清了每个文件的用途与格式。\n\
             2. **先问用户再动笔** —— 名字、语气、红线、用户画像都是\n\
                用户的事。这四个文件是定义「用户想要你成为谁」，\n\
                你编一份让用户来批改，不如一开始就问。\n\
                一次问不完就问几轮，宁可慢也不要猜。\n\
             3. 写完后告诉用户改了哪些文件。\n\n\
             ⚠️ **不要碰 `models.json`** —— 模型由用户在设置面板里添加，\n\
             你直接改那个文件可能把用户的密钥写坏。\n\n\
             **在人格文件补齐之前，不要假装有记忆或人格。**"
        ));
    }

    // ── 稳定区：运行环境（**纯规则文字**，工作目录值挪到尾部）──────────
    stable.push(
        "# 运行环境\n\n\
         你有文件工具（read_file / list_dir / glob_files / grep_files / write_file）。\n\
         **所有文件操作都受权限网关管辖**，越权会被拒绝。\n\
         遇到权限被拒时，直接告诉用户是哪条规则挡住的，不要反复重试。\n\
         当前工作目录在下方「动态信息」区给出。"
            .to_string(),
    );

    // ── 稳定区：MCP 机制（**纯规则文字**，组状态挪到尾部）──────────────
    // 关键：56 个外部工具的 schema 有 ~14k tokens，全量注入会让 prompt 变得又长又慢，
    // 还会让模型挑错工具。所以只常驻内置工具，外部工具按组懒加载。
    stable.push(
        "# 外部工具（MCP）的用法\n\n\
         你还能用一批**外部工具**（操作网页、远程服务器、数据库、桌面应用等），\
         但它们**不会默认出现在你的工具列表里** —— 因为全部装载会占用大量上下文。\n\n\
         正确用法是**两步**：\n\
         1. 先调用 `list_tool_groups`，看看有哪些工具组、各自是干什么的、多少工具。\n\
         2. 再对你需要的那一组调用 `load_tool_group`，加载后该组的工具会在**下一轮**可用。\n\n\
         规则：\n\
         - 一次只加载**当前任务真正需要**的组，不要图省事全加载（会超预算被拒）。\n\
         - 任务做完后可以用 `unload_tool_group` 释放空间。\n\
         - 当用户的需求涉及「打开网页 / 抓取某站 / 连服务器 / 查数据库 / 操作某个桌面软件」时，\
         先走上面两步。\n\
         - 加载组只拿到工具**名字**；具体参数看下一轮的工具定义。\n\
         已加载的组在下方「动态信息」区给出。"
            .to_string(),
    );

    // ── 稳定区：对话历史机制（**纯规则文字**）──────────────────────────
    // 这段很关键：history.rs 只回灌「最近 6h ∪ 最近 10 条」，
    // 更早的内容不在上下文里。如果模型不知道"有东西可查"，它会：
    //   ① 对着"上次那个方案"瞎编  ② 反问用户"哪个方案？"
    // 所以必须显式告知 —— 否则 recall_turns 工具等于摆设。
    stable.push(format!(
        "# 对话历史\n\n\
         你能看到本会话**最近**的对话（最近 {window} 小时内，或最近 {turns} 条），\
         更早的内容**不在你的上下文里**。\n\n\
         当用户提到「上次 / 之前 / 刚才那个 / 我们之前定的」而你找不到依据时：\n\
         **不要猜，也不要反问「哪个？」** —— 调 `recall_turns` 工具去查。\n\n\
         `recall_turns` 用法：\n\
         - `query`：关键词（如「跳页」「构建报错」）。留空则返回最近的记录。\n\
         - `hours`：只看最近 N 小时（可选）。\n\
         - `limit`：最多返回几条，默认 10。\n\n\
         查完基于结果回答，并说明「这是从更早的对话里找到的」。",
        window = crate::history::WINDOW_HOURS,
        turns = crate::history::FALLBACK_TURNS,
    ));

    // ── 动态尾部 ①：技能清单（加技能才变）──────────────────────────────
    dynamic.push(format!(
        "# 技能（Skills）\n\n\
         你有一个本地技能库，存放用户或你自己沉淀的**可复用操作流程**。\n\
         技能清单：\n\n{}\n\n\
         使用规则：\n\
         - 接到任务先看清单：有匹配的技能就**先调 load_skill 读全文**，严格按步骤执行，不要凭清单里那行描述猜细节。\n\
         - 用户明确说「保存到技能 / 记成技能 / 以后照这个做」，或你刚完成一套\
         今后会反复用到的多步骤流程（改过坑、验证过的），就调 save_skill 沉淀。\n\
         - save_skill 的 description 要写清「什么时候该用它」（触发场景），正文写具体步骤。\n\
         - 技能执行中发现步骤过时或踩了新坑，用 save_skill（overwrite=true）修正它。",
        crate::skills::catalog(data_dir)
    ));

    // ── 动态尾部 ②：MCP 已加载组状态（加载/卸载组才变）─────────────────
    // 注：组状态本身由 `tool_specs()` 每轮实时反映在工具定义里，
    // 这里只是一个文字提示，告诉模型"当前手上有什么"。
    dynamic.push(format!(
        "# 动态信息\n\n\
         ## 当前工作目录\n{}\n\n\
         ## 已加载的 MCP 工具组\n\
         用 `list_tool_groups` 可查看全部可用组。",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into())
    ));

    stable.extend(dynamic);
    stable.join("\n\n---\n\n")
}

/// 当前 epoch 毫秒。会话与历史模块共用同一口径。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_includes_rules_first() {
        let tmp = std::env::temp_dir().join("float_agent_prompt_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULE_MARKER_XYZ").unwrap();
        std::fs::write(tmp.join("USER.md"), "USER_MARKER_ABC").unwrap();

        let p = build_system_prompt(&tmp);
        let rules_at = p.find("RULE_MARKER_XYZ").expect("RULES 应在 prompt 里");
        let user_at = p.find("USER_MARKER_ABC").expect("USER 应在 prompt 里");
        assert!(rules_at < user_at, "RULES 必须排在 USER 前面");
        assert!(p.contains("行为红线"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn system_prompt_without_files_still_works() {
        let tmp = std::env::temp_dir().join("float_agent_prompt_empty");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let p = build_system_prompt(&tmp);
        assert!(p.contains("运行环境"));
    }

    /// 🔴 前缀稳定性：所有**会变的内容**必须排在所有**不变的内容**之后
    ///
    /// 为什么这个测试重要：prompt cache 按 token 前缀逐字节比对，
    /// 任何一处变动都会让它**后面**的全部 token 按未命中价重付。
    /// 所以「动态内容的位置」是成本正确性问题，不是代码风格问题。
    #[test]
    fn dynamic_content_must_trail_static_content() {
        let tmp = std::env::temp_dir().join("float_agent_prompt_order");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULES_BODY").unwrap();
        std::fs::write(tmp.join("SOUL.md"), "SOUL_BODY").unwrap();
        std::fs::write(tmp.join("USER.md"), "USER_BODY").unwrap();

        let p = build_system_prompt(&tmp);

        // 静态锚点：这些是"改代码才变"的规则文字
        let static_anchors = [
            "RULES_BODY",
            "SOUL_BODY",
            "USER_BODY",
            "运行环境",
            "外部工具（MCP）的用法",
            "# 对话历史", // 带 # —— 否则可能命中动态区里的指引句
        ];
        // 动态锚点：这些是"运行时数据"，会随用户操作变化
        let dynamic_anchors = ["# 技能（Skills）", "# 动态信息"];

        let last_static = static_anchors
            .iter()
            .map(|a| (a, p.find(a).unwrap_or_else(|| panic!("静态段 {a} 缺失"))))
            .max_by_key(|(_, at)| *at)
            .unwrap();
        let first_dynamic = dynamic_anchors
            .iter()
            .map(|a| (a, p.find(a).unwrap_or_else(|| panic!("动态段 {a} 缺失"))))
            .min_by_key(|(_, at)| *at)
            .unwrap();

        assert!(
            last_static.1 < first_dynamic.1,
            "动态内容必须全部排在静态内容之后（否则加技能会让后面整段缓存失效）\n\
             最后一个静态段「{}」位置={}，第一个动态段「{}」位置={}",
            last_static.0,
            last_static.1,
            first_dynamic.0,
            first_dynamic.1
        );

        // 工作目录的**值**必须在动态区。
        // 注意不能用 "当前工作目录" 当锚点 —— 稳定区的运行环境段里
        // 有一句"当前工作目录在下方...给出"做指引，会先命中那一句。
        // 所以用动态区独有的 `## 当前工作目录` 标题当锚点。
        let cwd_at = p
            .find("## 当前工作目录")
            .expect("动态区应有工作目录标题");
        assert!(
            cwd_at > first_dynamic.1,
            "工作目录值应在动态区，不能在稳定前缀里"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 稳定前缀必须**逐字节稳定**：同一份输入调两次结果完全相同
    ///
    /// 注意 `current_dir()` 在进程内恒定，所以整个 prompt 两次应完全一致。
    #[test]
    fn system_prompt_is_byte_stable_across_calls() {
        let tmp = std::env::temp_dir().join("float_agent_prompt_stable");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "R").unwrap();

        let a = build_system_prompt(&tmp);
        let b = build_system_prompt(&tmp);
        assert_eq!(a, b, "同一份输入必须产出逐字节相同的 prompt");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 🔴 真实网络调用 —— 验证完整 agent loop（LLM → tool_call → 执行 → 回灌 → 答案）
    ///
    /// 默认 `#[ignore]`，手动跑：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture real_agent_loop
    /// ```
    #[tokio::test]
    #[ignore = "需要网络 + 有效 API key"]
    async fn real_agent_loop() {
        use crate::config;
        use crate::mcp;
        use crate::permission;

        let data_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-data");

        let cfg = config::find_model(&data_dir, "sensenova-6.8-flash-lite")
            .expect("读不到 sensenova 模型配置（检查 ~/.workbuddy/models.json）");

        let app_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let gate = permission::bootstrap_gate(&data_dir, &app_dir);

        // 测试里不连 MCP，只看内置工具
        let mcp_reg = tokio::sync::Mutex::new(mcp::ToolRegistry::new(""));

        println!("=== 模型 {} @ {} ===", cfg.id, cfg.url);

        let run = run(
            &cfg,
            &gate,
            &data_dir,
            &mcp_reg,
            "看看 D:\\myword 目录下有哪些内容，简要列一下",
            vec![],
            std::sync::Arc::new(|p| eprintln!("[progress] {p:?}")),
            None,
            &std::sync::atomic::AtomicBool::new(false),
            // 传当前会话 id → 顺带验证历史回灌
            &crate::sessions::ensure_current(&data_dir).id,
        )
        .await
        .expect("agent loop 失败");

        println!("\n=== 执行步骤（{} 轮）===", run.iterations);
        for s in &run.steps {
            let name = s.name.clone().unwrap_or_default();
            let detail: String = s.detail.chars().take(160).collect();
            println!("[{}] {} {}", s.kind, name, detail);
        }

        println!("\n=== 最终答案 ===");
        println!("{}", run.answer);

        assert!(!run.answer.is_empty(), "答案不能为空");
        assert!(
            run.steps.iter().any(|s| s.kind == "tool_call"),
            "应该至少发起过一次工具调用"
        );
    }
}
