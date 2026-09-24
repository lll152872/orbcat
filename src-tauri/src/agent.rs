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
use crate::llm::{self, ChatMessage, TokenUsage};
use crate::permission::PermissionGate;
use crate::tools;

/// 基础循环轮数上限（默认值；可被环境变量覆盖）
///
/// 覆盖方式：`FLOAT_AGENT_MAX_ITERATIONS=100`。
/// 为什么做成可配：不同模型/任务对轮数需求差别很大（简单问答 3 轮够，
/// 长链路重构可能几十轮），编译期写死一个数字必然有一边不合适。
const DEFAULT_MAX_ITERATIONS: usize = 100;

/// 硬上限（默认值；可被环境变量覆盖）：插话可以**延长**预算，但不能无限延长。
///
/// 不做硬顶的话，"用户一直插话"会让基础轮数变成事实上的无上限 —— 既烧钱，
/// 也真的可能转不出来。到了这里仍然按"任务过大/模型陷入循环"报错。
///
/// 覆盖方式：`FLOAT_AGENT_HARD_ITERATIONS=250`。
const DEFAULT_HARD_ITERATIONS: usize = 250;

/// 读一个正整数环境变量；缺失/非法/为 0 时回退默认值。
///
/// 为什么静默回退而不是报错：这两个值是"安全阀"参数，配错了不应该让
/// 整个 agent 起不来 —— 回退到默认值并打一行日志，用户能跑就行。
fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("[float-agent] 环境变量 {name}={v:?} 非法（需正整数），回退默认 {default}");
                default
            }
        },
        Err(_) => default,
    }
}

/// 基础轮数上限（读环境变量，每次调用求值 —— 便于测试与运行时调整）
pub fn max_iterations() -> usize {
    env_usize("FLOAT_AGENT_MAX_ITERATIONS", DEFAULT_MAX_ITERATIONS)
}

/// 硬上限（读环境变量）
pub fn hard_iterations() -> usize {
    env_usize("FLOAT_AGENT_HARD_ITERATIONS", DEFAULT_HARD_ITERATIONS)
}

/// 每消费一批插话，给预算加多少轮
const STEER_BONUS: usize = 6;

/// 插话队列上限（满了就让用户等当前轮结束）
const MAX_INBOX: usize = 8;

/// 单条工具结果回灌给模型时的截断长度
const TOOL_RESULT_LIMIT: usize = 24 * 1024;

/// 上下文预算的安全水位：用量超过 `maxInputTokens × 这个比例` 就开始裁剪。
///
/// 为什么是 0.75 而不是 1.0：① 估算本身有误差（中英混排、代码块的
/// char→token 比并不恒定）；② 下一轮还要再塞工具结果；③ 留出输出空间。
/// 宁可早裁，不可撞 413 / context_length_exceeded —— 撞上去就是整轮报废。
const CONTEXT_SAFE_RATIO: f64 = 0.75;

/// 模型没配 `maxInputTokens` 时的保守兜底窗口。
///
/// 为什么必须有兜底：配置里大量模型这个字段是 `null`（用户懒得填），
/// 没兜底 = 没有预算保护 = 拉高轮数必炸。取 128k 是因为它是当前主流
/// 模型的**下界**（多数在 256k~1M，但保守取值保证不会误判为"还有空间"）。
const FALLBACK_CONTEXT_WINDOW: usize = 128 * 1024;

/// 估算整个 messages 数组的 token 数（含工具定义之外的纯消息部分）。
///
/// 注意**不含**工具 schema —— 那部分是固定的、每轮都一样，且已在
/// `tools_to_openai` 里控制。这里聚焦"会随轮数增长的部分"。
fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    let mut total = 0usize;
    for m in messages {
        if let Some(c) = &m.content {
            total += llm::estimate_tokens(&c.as_plain());
        }
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                total += llm::estimate_tokens(&tc.function.name);
                total += llm::estimate_tokens(&tc.function.arguments);
                total += 8; // 结构性开销
            }
        }
        total += 4; // 每条消息的角色标记等固定开销
    }
    total
}

/// 从 messages 里**裁掉最旧的工具结果**，给后面的轮次腾空间。
///
/// 为什么裁工具结果而不是别的：工具结果通常是上下文里**最大块且最可替代**的内容
/// （几 KB 的文件内容、命令输出），而模型后续真正依赖的是"做过什么"而非
/// "原始输出全文"。system / user / assistant 都保留 —— 那些是对话骨架。
///
/// 返回裁掉了多少条。
fn trim_old_tool_results(messages: &mut [ChatMessage], keep_recent: usize) -> usize {
    // 找出所有 tool 消息的下标
    let tool_idx: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .map(|(i, _)| i)
        .collect();
    if tool_idx.len() <= keep_recent {
        return 0;
    }
    let mut trimmed = 0usize;
    for &i in &tool_idx[..tool_idx.len() - keep_recent] {
        // 只裁"还没被裁过"的（占位串幂等，避免反复覆盖变短）
        let is_placeholder = messages[i]
            .content
            .as_ref()
            .map(|c| c.as_plain().starts_with("[已省略"))
            .unwrap_or(false);
        if is_placeholder {
            continue;
        }
        messages[i].content = Some(crate::llm::MessageContent::text(
            "[已省略：本条工具结果因上下文预算被裁剪，需要时可重新调用该工具]",
        ));
        trimmed += 1;
    }
    trimmed
}

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
    /// 请求链路状态（连接中 / 等首包 / 限流重试）。**不是**模型思维链 ——
    /// 用来消掉「界面一直显示思考，其实 HTTP 还没首字节」的黑盒感。
    ///
    /// `retry`：只有限流退避这类状态才进对话时间线（前端据此决定要不要落一条）。
    /// 过场状态（正在请求 / 等首包）每轮都有，只刷标题、不入时间线。
    Status {
        text: String,
        #[serde(default)]
        retry: bool,
    },
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
    /// 执行中「插话」的用户消息**已被消费**（前端把「排队中」改成「已送达」）
    Steer { id: String, text: String },
}

/// 执行中「插话」进来的一条用户消息。
///
/// 语义：**排队**。它不打断正在跑的 LLM 调用/工具，而是在下一个轮边界被
/// 追加进 `messages`，作为一条普通 user 消息继续同一段上下文。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteeredMsg {
    /// 前端用来把「排队中」改成「已送达」
    pub id: String,
    pub text: String,
    pub images: Vec<String>,
}

/// 插话回执：告诉前端排到第几位
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerAck {
    pub id: String,
    /// 入队后队列长度（含本条）
    pub queued: usize,
}

/// 生成插话 id（前端据此定位气泡）
pub fn new_steer_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("st{ms}-{n}")
}

// ---------------------------------------------------------------------------
// 插话收件箱（RunHub）
// ---------------------------------------------------------------------------

/// 执行中「插话」的收件箱。**全局单例**（同一时刻只有一个 run）。
///
/// ## 为什么是全局单例而不是挂在会话上
/// 做成"会话维度"的话，用户切一下会话，队列里的消息就跟着跑了 —— 而正在跑的
/// run 还在原会话上，消息永远送不到。Inbox 属于"当前这次执行"，不属于会话。
///
/// ## 为什么用 `Mutex<VecDeque>` 而不是 mpsc 通道
/// loop 只在**轮边界**取一次（一轮最多 24 次），不需要"推一下就立刻唤醒"。
/// 队列还有个好处：能知道排在第几位，满没满，能一次性 drain 出来。
pub struct RunHub {
    /// 是否有正在跑的一轮对话。`chat_steer` 靠它区分"插话"和"新的一轮"
    active: std::sync::atomic::AtomicBool,
    inbox: tokio::sync::Mutex<std::collections::VecDeque<SteeredMsg>>,
}

impl Default for RunHub {
    fn default() -> Self {
        Self::new()
    }
}

impl RunHub {
    pub fn new() -> Self {
        Self {
            active: std::sync::atomic::AtomicBool::new(false),
            inbox: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 开跑前调用：置位 + **清空队列**。
    ///
    /// 清空是必须的：上一轮结束时若还有残留没被取走（理论上 `end` 会取走，
    /// 但异常路径谁也不敢保证），那些消息绝不能漏进新的一轮 —— 否则用户会
    /// 看到自己几轮前说过的话被当成新指令执行。
    pub fn begin(&self) {
        if let Ok(mut q) = self.inbox.try_lock() {
            q.clear();
        }
        self.active
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 收尾（**无论成功、失败、中断都要调**）：置位关闭 + 取走残留。
    ///
    /// 取走的残留由调用方回填给前端（还原到输入框），不让用户白打一遍字。
    pub fn end(&self) -> Vec<SteeredMsg> {
        self.active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.inbox
            .try_lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// 入队一条插话。没有进行中的对话 → Err（前端据此改走普通发送）。
    pub async fn push(&self, m: SteeredMsg) -> Result<SteerAck, String> {
        if !self.is_active() {
            return Err("当前没有进行中的对话".into());
        }
        let mut q = self.inbox.lock().await;
        if q.len() >= MAX_INBOX {
            return Err(format!("插话队列已满（最多 {MAX_INBOX} 条），等当前轮结束后再发"));
        }
        let id = m.id.clone();
        q.push_back(m);
        Ok(SteerAck {
            id,
            queued: q.len(),
        })
    }

    /// loop 在轮边界取走本轮要消费的全部插话
    pub async fn drain(&self) -> Vec<SteeredMsg> {
        self.inbox.lock().await.drain(..).collect()
    }
}

/// 进度回调。用 Arc<dyn Fn> 而不是泛型，避免把泛型参数污染到整个 agent loop。
pub type ProgressFn = std::sync::Arc<dyn Fn(Progress) + Send + Sync>;

/// 什么都不做的回调（测试用）
pub fn no_progress() -> ProgressFn {
    std::sync::Arc::new(|_| {})
}

/// 把某一轮的思维链并入**时间线**与累计串。
///
/// 空轮直接跳过（多数轮没有 reasoning，不该在界面上留一堆空的思考条目）。
///
/// ## 为什么既要 `steps` 又要 `total`（2026-09-22 用户拍板）
///
/// - `steps`（kind = `reasoning`）是**规范来源**：思考作为独立条目按位置插进去，
///   此刻正落在本轮工具调用**之前**，渲染出来就是「思考 → 调工具 → 思考 → 调工具」
///   的交错节奏。以前只有下面那个累计串，所有思考必然堆成一块，工具另起一处。
/// - `total` 是**兼容用的旧字段**（`AgentRun.reasoning` / `StoredMessage.reasoning`）：
///   老会话数据与老前端读它，多轮时按「【第 N 轮】」分段。新数据以 steps 为准。
fn absorb_reason(
    total: &mut String,
    iter: usize,
    round: &std::sync::Mutex<String>,
    steps: &mut Vec<AgentStep>,
) {
    let txt = match round.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    if txt.trim().is_empty() {
        return;
    }
    // 时间线条目：位置就是"此刻"，即本轮的思考在它触发的工具调用之前
    steps.push(AgentStep {
        kind: "reasoning".into(),
        name: None,
        detail: txt.clone(),
    });
    if !total.is_empty() {
        total.push_str("\n\n");
    }
    total.push_str(&format!("【第 {iter} 轮】\n{txt}"));
}

/// 单次 run 里最多记多少条链路状态（限流重试通知这类），多了纯噪音。
const MAX_STATUS_NOTES: usize = 16;

/// 把这一轮收到的链路状态（连接中 / 限流退避重试）落成时间线条目。
///
/// 为什么要落进 steps：撞 429 时用户最想看到的恰恰是"它在等什么、重试了几次"。
/// 这些状态以前只写进进行中气泡的标题（内存态），一旦失败或重载就没了。
fn drain_status_steps(log: &std::sync::Mutex<Vec<String>>, steps: &mut Vec<AgentStep>) {
    let mut g = match log.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    for text in g.drain(..) {
        steps.push(AgentStep {
            kind: "status".into(),
            name: None,
            detail: text,
        });
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStep {
    /// `assistant` | `tool_call` | `tool_result` | `steer` | …
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
    /// 本轮的思考过程（多轮时按「【第 N 轮】」分段）。**只给用户看，不回灌模型**
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 被用户「停止」打断 —— `answer` 是**半截**正文（已产出的部分）
    #[serde(default)]
    pub interrupted: bool,
    /// 执行中被消费掉的插话（按送达顺序，落盘要按这个顺序写）
    #[serde(default)]
    pub steers: Vec<SteeredMsg>,
    /// 中断时队列里没来得及消费的插话文本（前端还原到输入框，避免白打字）
    #[serde(default)]
    pub pending_steers: Vec<String>,
    /// 中断原因（目前只有 `已停止`；留字段是为了以后区分超时/错误）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// 本轮问答**所有模型调用**的 token 用量合计（含中途调工具的轮次）。
    ///
    /// 为什么是求和而不是只算最后那轮：用户问「这次问答花了多少 token」，
    /// 想知道的是"这一问一答总共烧了多少"，工具轮的开销同样是这一问引起的，
    /// 拆开报反而失真。全 0（服务端没返回 usage）时前端不显示角标。
    #[serde(default)]
    pub usage: TokenUsage,
}

/// 一轮对话**失败**时带出的结果：错误文案 + 已经产出的部分结果。
///
/// ## 为什么错误要携带 `run`（2026-09-22 用户报 bug 后定案）
///
/// 以前错误直接 `Err(String)` 抛给 `lib.rs`，而 `lib.rs` 是在**落盘之前**
/// `result?` 的 —— 于是撞 429（或任何 API 错误）时，这一轮的提问、思考、
/// 工具步骤、半截正文**一个字都不写**，用户屏幕上只剩一条红色错误横幅，
/// 连自己刚问的那句话都找不回来。
///
/// 现在错误和「停止」「轮数用尽」走同一套语义：**部分结果照样落盘**，
/// 差别只是 `interrupted = true` + 尾部多一条 `kind = "error"` 的时间线条目，
/// 并且最终还是 `Err` 返回（前端照旧显示红条，不把真错误伪装成正常结束）。
#[derive(Debug)]
pub struct RunAbort {
    /// 给用户看的错误文案（前端红条显示的就是它）
    pub message: String,
    /// 必须落盘的部分结果（`interrupted = true`，尾部含 error 条目）
    pub run: AgentRun,
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
/// - `hub` 执行中「插话」的收件箱：loop 在**轮边界**取走并当作追加的 user 消息
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
    // 执行中插话的收件箱
    hub: &RunHub,
    // 「申请权限」的授权表 + 待办通道，透传给工具层
    perm: &crate::perm_request::PermHub,
) -> Result<AgentRun, RunAbort> {
    use std::sync::atomic::Ordering;

    // 传入本轮用户输入 → 命中技能的正文会作为最尾动态段自动加载进来（方案 B）
    let mut system_prompt = build_system_prompt_for(data_dir, user_input);
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

        // 项目记忆：前台目录/文件命中 projects/*/source.ref 时附上（最长前缀）
        let candidates: Vec<&Path> = [fg.dir.as_deref(), fg.file.as_deref()]
            .into_iter()
            .flatten()
            .map(Path::new)
            .collect();
        if let Some((name, mem)) =
            crate::memory::MemoryStore::new(data_dir).match_project_memory(&candidates)
        {
            system_prompt.push_str(&format!(
                "\n\n---\n\n# 项目记忆（{name}）\n\n\
                 当前前台路径命中该项目。下列约定只在本项目内成立，优先遵守：\n\n{mem}\n\n\
                 若要沉淀新的项目级事实，用 `remember` 提交候选（或引导用户写进 \
                 `agent-data/projects/{name}/MEMORY.md`）。"
            ));
        }
    }

    let first_user = build_user_message(cfg, data_dir, user_input, &images);

    // ---- 多轮上下文回灌（用户 2026-09-19 定案）----
    // 规则：(最近 6h 内的消息) ∪ (最近 10 条)，见 history.rs 文档。
    //
    // ⚠️ 时序关键：必须在 lib.rs::chat 调 `sessions::append_turn` **之前**读，
    //    否则当前这轮会被算进历史，模型看到自己的问题重复出现。
    let mut history = if session_id.is_empty() {
        Vec::new()
    } else {
        crate::history::build_history(data_dir, session_id, now_ms())
    };

    // ---- 自动 compact：历史太大先压缩，再回灌 ----
    //
    // 为什么在"发请求前"而不是"撞到上限时"：撞上限意味着这次请求已经废了
    // （413 / context_length_exceeded），而压缩要额外发一次模型请求 —— 必须在
    // 还有余量时做。摘要**落盘**，所以只在真正超水位时压一次，之后每轮直接读。
    let context_window = cfg
        .max_input_tokens
        .map(|n| n as usize)
        .unwrap_or(FALLBACK_CONTEXT_WINDOW);
    if !session_id.is_empty() {
        let est = estimate_messages_tokens(&history);
        let trigger = ((context_window as f64) * crate::history::COMPACT_TRIGGER_RATIO) as usize;
        if est > trigger {
            eprintln!(
                "[float-agent] 历史估算 {est} tok 超过 compact 水位 {trigger}，触发自动压缩"
            );
            match crate::history::compact(
                cfg,
                data_dir,
                session_id,
                crate::history::COMPACT_KEEP_RECENT,
                false,
                now_ms(),
            )
            .await
            {
                Ok(o) => {
                    eprintln!(
                        "[float-agent] 自动 compact 成功：压 {} 条 → {} 字，保留 {} 条",
                        o.summarized, o.summary_chars, o.kept
                    );
                    history = crate::history::build_history(data_dir, session_id, now_ms());
                }
                Err(e) => {
                    // 压缩失败不该挡住对话 —— 后面还有循环内的裁剪兜底
                    eprintln!("[float-agent] 自动 compact 跳过（{e}），改用循环内裁剪兜底");
                }
            }
        }
    }
    let history_len = history.len();

    let mut messages = vec![ChatMessage::system(system_prompt)];
    messages.extend(history);
    messages.push(first_user);

    eprintln!(
        "[float-agent] 上下文回灌: 历史 {history_len} 条 + 本轮输入 1 条（会话 {session_id}）"
    );

    let mut steps: Vec<AgentStep> = Vec::new();

    // 链路状态通知（连接中 / 限流退避重试）的暂存区。
    //
    // 回调是 `'static` 的 trait object，借不到旁边的 `steps`，所以先用
    // `Arc<Mutex<Vec>>` 攒着，在本轮 LLM 调用返回后一次性按位置并入 steps。
    let status_log: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // 跨轮累计的思考过程（多轮时按「【第 N 轮】」分段）。
    //
    // 为什么要累加：模型每轮都会重新推演一遍，只留最后一轮会让用户看不懂
    // "前面为什么这么绕"。落盘时按上限截断（见 `sessions::REASONING_LIMIT`）。
    // ⚠️ 这是**旧字段**（兼容用）：新数据以 `steps` 里的 `reasoning` 条目为准。
    let mut reason_total = String::new();

    // 执行中被消费掉的插话（按送达顺序，落盘要按这个顺序写）
    let mut steers: Vec<SteeredMsg> = Vec::new();

    // 本轮问答的 token 用量合计 —— 每轮 chat_stream 成功后累加。
    // 服务端不给 usage（未开 include_usage / 网关不透传）时保持全 0。
    let mut usage_total = TokenUsage::default();

    // 软预算：初始 max_iterations()，每消费一批插话 +STEER_BONUS，最多到 hard_iterations()。
    // 为什么不做成"插话不占轮数"：那等于取消上限。
    let max_iters = max_iterations();
    let hard_iters = hard_iterations().max(max_iters);
    let mut budget = max_iters;
    let mut iter = 0usize;

    // 上下文预算：模型配了就用，没配走保守兜底。乘安全水位。
    // （`context_window` 已在自动 compact 那一段算过，这里直接复用）
    let context_budget = ((context_window as f64) * CONTEXT_SAFE_RATIO) as usize;
    // 裁剪时至少保留最近这么多条工具结果原文（它们是模型当前最可能依赖的）
    const KEEP_RECENT_TOOL_RESULTS: usize = 4;
    eprintln!(
        "[float-agent] 上下文预算: 窗口 {} tok（{}）× {:.0}% = {} tok{}",
        context_window,
        if cfg.max_input_tokens.is_some() { "模型配置" } else { "兜底默认" },
        CONTEXT_SAFE_RATIO * 100.0,
        context_budget,
        if cfg.max_input_tokens.is_some() {
            "".to_string()
        } else {
            format!("；建议给模型 {} 填 maxInputTokens", cfg.id)
        }
    );

    while iter < budget.min(hard_iters) {
        // 取消检查点 ①：每轮开始。
        //
        // ⚠️ 这里是 `Ok(部分结果)` 而不是 `Err("已停止")`：用户点「停止」时想要的
        //    不是"这一轮凭空消失"，而是"停在这儿，我看到的东西留下"。返回 Err 会让
        //    `lib.rs` 提前 `?` 掉，用户连自己那条提问都看不到。
        if cancel.load(Ordering::Relaxed) {
            return Ok(interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total));
        }
        iter += 1;

        // ---- 轮边界：消费「插话」----
        // 放在这里同时覆盖两个时机：上一轮 LLM 结束后、以及工具都跑完之后
        // （两者都回到循环顶部）。插话就是普通 user 消息 —— 同一个 loop、
        // 同一个权限网关，所以「越权」这条红线自动同样生效，不需要额外校验。
        for m in hub.drain().await {
            messages.push(build_user_message(cfg, data_dir, &m.text, &m.images));
            // 插进 steps 时间线：重载后过程里能看到「跑到这一步时用户补了一句」
            steps.push(AgentStep {
                kind: "steer".into(),
                name: Some(m.id.clone()),
                detail: m.text.clone(),
            });
            on_progress(Progress::Steer {
                id: m.id.clone(),
                text: m.text.clone(),
            });
            eprintln!("[float-agent] 插话已送达（第 {iter} 轮）: {}", m.text);
            steers.push(m);
            budget = (budget + STEER_BONUS).min(hard_iters);
        }

        // ---- 上下文预算检查（每轮 LLM 调用前）----
        //
        // 为什么必须有这一步：`messages` 在循环里**只增不减**（每轮 push
        // assistant + N 条 tool 结果）。轮数上限拉到 100/250 后，不检查就会
        // 一路顶穿模型窗口 → 413 / context_length_exceeded → 整轮报废。
        //
        // 策略是**渐进裁剪**而不是直接中止：
        //   1) 先裁最旧的工具结果为占位串（信息密度最低、最可替代）
        //   2) 裁完还超 → 说明不是工具结果撑的（可能是超长对话骨架），
        //      此时才降级返回部分结果，别硬发一个必然失败的请求
        let est = estimate_messages_tokens(&messages);
        if est > context_budget {
            let trimmed = trim_old_tool_results(&mut messages, KEEP_RECENT_TOOL_RESULTS);
            let after = estimate_messages_tokens(&messages);
            eprintln!(
                "[float-agent] 上下文预算超限（第 {iter} 轮）：估算 {est} > 预算 {context_budget}，\
                 裁掉 {trimmed} 条旧工具结果 → {after} tok"
            );
            if after > context_budget {
                // 裁剪救不回来 → 降级返回部分结果（不硬发必然失败的请求）
                eprintln!(
                    "[float-agent] 裁剪后仍超预算（{after} > {context_budget}），降级返回部分结果"
                );
                let mut r =
                    interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total);
                r.stop_reason = Some(format!(
                    "上下文已接近模型上限（估算 {after} / 预算 {context_budget} tok），\
                     继续执行会被模型拒绝。以下是已产出的部分结果；\
                     可清空部分历史或换更大窗口的模型后重试"
                ));
                return Ok(r);
            }
        }

        // 每轮重建工具列表 —— 模型可能刚 load 了新组
        let tools_arg = if cfg.supports_tool_call {
            let specs = {
                let reg = mcp.lock().await;
                let ready = crate::search::is_ready(data_dir);
                tools::tool_specs(Some(&reg), ready)
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
        // 本轮的两个累积器。为什么单独攒一份：`out` 只在调用**成功**返回时才有，
        // 被中断那轮的正文与思考就丢了；而中断恰恰是用户最想回看的时候。
        let round_reason = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let round_text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let delta_cb = {
            let cb = on_progress.clone();
            let flag = streamed_this_round.clone();
            let acc_r = round_reason.clone();
            let acc_t = round_text.clone();
            move |text: Option<String>, reasoning: Option<String>| {
                if let Some(t) = text.as_deref() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    if !t.is_empty() {
                        // ⚠️ 锁只活在这一行内，绝不跨 await —— 这个闭包会被 llm
                        //    在 `.await` 内部调用，把 guard 带出去就是死锁。
                        if let Ok(mut g) = acc_t.lock() {
                            g.push_str(t);
                        }
                    }
                }
                if let Some(r) = reasoning.as_deref() {
                    if !r.is_empty() {
                        if let Ok(mut g) = acc_r.lock() {
                            g.push_str(r);
                        }
                    }
                }
                cb(Progress::Delta { text, reasoning });
            }
        };

        // 链路状态：连接中 / 等首包 / 限流重试 —— 与思维链区分开，消掉黑盒。
        //
        // 同时落进 `status_log`：这些是**内存态的通知**，以前只出现在进行中气泡的
        // 标题上，撞 429 失败后连"重试了几次"都看不到。攒起来在本轮结束时
        // 按位置插进时间线（见 `drain_status_steps`）。
        let status_cb = {
            let cb = on_progress.clone();
            let log = status_log.clone();
            std::sync::Arc::new(move |text: String, retry: bool| {
                // 只有"值得留在时间线"的状态（限流退避）才攒起来落盘；
                // 「正在请求 / 等首包」是每轮的过场，记下来只会刷屏
                if retry {
                    if let Ok(mut g) = log.lock() {
                        if g.len() < MAX_STATUS_NOTES {
                            g.push(text.clone());
                        }
                    }
                }
                cb(Progress::Status { text, retry });
            }) as std::sync::Arc<llm::StatusFn>
        };

        // 断流重试前作废半截正文 —— 与工具调用 DiscardStream 同语义（reasoning 保留）
        let discard_cb = {
            let cb = on_progress.clone();
            let flag = streamed_this_round.clone();
            let acc_t = round_text.clone();
            std::sync::Arc::new(move |reason: String| {
                flag.store(false, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut g) = acc_t.lock() {
                    g.clear();
                }
                cb(Progress::DiscardStream { reason });
            }) as std::sync::Arc<llm::DiscardFn>
        };

        let out = match llm::chat_stream(
            cfg,
            messages.clone(),
            tools_arg,
            &delta_cb,
            status_cb.as_ref(),
            discard_cb.as_ref(),
            cancel,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                // 这一轮收到的链路状态（多半是"N 次限流重试"）先落进时间线，
                // 位置在本轮思考/工具之前 —— 用户要看的正是"卡在哪一步"
                drain_status_steps(&status_log, &mut steps);
                absorb_reason(&mut reason_total, iter, &round_reason, &mut steps);
                // 被「停止」打断 → 把这一轮已经流出来的半截正文带上
                if cancel.load(Ordering::Relaxed) {
                    let half = take_text(&round_text);
                    return Ok(interrupted_run(steps, steers, reason_total, half, iter, usage_total));
                }
                // ---- 真错误：**带部分结果**返回，而不是把整轮丢掉 ----
                //
                // 语义与「停止」一致（部分结果照样落盘、同样跳过日记），只是
                // stop_reason 写真实错误、并追加一条 error 时间线条目。
                // 仍然返回 Err：真错误不该被伪装成正常结束（前端要显红条）。
                let mut run = interrupted_run(
                    steps,
                    steers,
                    reason_total,
                    take_text(&round_text),
                    iter,
                    usage_total,
                );
                run.stop_reason = Some(e.clone());
                run.steps.push(AgentStep {
                    kind: "error".into(),
                    name: None,
                    detail: e.clone(),
                });
                return Err(RunAbort { message: e, run });
            }
        };
        drain_status_steps(&status_log, &mut steps);
        absorb_reason(&mut reason_total, iter, &round_reason, &mut steps);
        // 本轮的 token 用量并入合计（无论这轮是出答案还是调工具）
        if let Some(u) = out.usage {
            usage_total.add(&u);
        }
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
                reasoning: (!reason_total.is_empty()).then_some(reason_total),
                interrupted: false,
                steers,
                pending_steers: Vec::new(),
                stop_reason: None,
                usage: usage_total,
            });
        }

        // --- 有工具调用：本轮若已流过正文，告诉前端把那半截话淡化掉 ---
        // （方案 C 的代价：模型偶尔会先吐半句再决定调工具）
        //
        // ⚠️ 只作废**正文**。reasoning 保留 —— 它是"为什么调这个工具"的推演，
        //    正是用户想看的；作废它等于把最有价值的部分删掉。
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
                    perm,
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

        // 工具附带的画面（截图 / view_image）：作为 user 消息附进去，模型下一轮就能"看到"
        if !pending_images.is_empty() {
            messages.push(build_user_message(
                cfg,
                data_dir,
                "（以上是刚获取的图片画面）",
                &pending_images,
            ));
        }

        // 取消检查点 ②：工具都执行完、还没发下一轮 LLM 请求 —— 在这里停最省。
        // 同上：返回**部分结果**而不是 Err，让已完成的工具调用留在会话里。
        if cancel.load(Ordering::Relaxed) {
            return Ok(interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total));
        }
    }

    // 轮数用尽仍未收敛 —— **降级返回部分结果**，而不是 Err。
    //
    // 为什么不再直接报错（对齐 WorkBuddy 的 "returning partial output as completed"）：
    // 用户点一次对话，前面已经烧掉的 token、已经跑完的工具调用、已经流出的正文
    // 都是真实产出。一个 Err 会让 `lib.rs` 提前 `?` 掉，用户连自己那条提问都看不到，
    // 前面几十轮的活全部作废 —— 体验比"给半截"差得多。
    //
    // 走 `interrupted_run` 的好处：`interrupted=true` 让 lib.rs 跳过日记（半截问答
    // 不该写进流水账），但**会话 JSON 照样落盘**，用户能看到提问与已产出内容。
    eprintln!(
        "[float-agent] 已连续调用工具 {iter} 轮仍未给出结论，降级返回部分结果（预算 {budget}，硬顶 {hard_iters}）"
    );
    let mut r = interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total);
    // 借用 stop_reason 说明是"轮数用尽"而非"用户停止"，前端据此显示不同提示
    r.stop_reason = Some(format!(
        "已达轮数上限（{iter} 轮），以下是已产出的部分结果。可能是任务过大或模型陷入循环。\
         你可以：① 直接说「继续」接着跑；② 先压缩上下文再说「继续」；③ 停止。\
         也可调大 FLOAT_AGENT_MAX_ITERATIONS / FLOAT_AGENT_HARD_ITERATIONS"
    ));
    Ok(r)
}

/// 组装「被中断」的部分结果。
///
/// 三种情况共用它：用户点「停止」、轮数用尽降级、以及 **API 错误**（后者由
/// [`RunAbort`] 包着返回，`stop_reason` 换成真实错误文案并追加 error 条目）。
///
/// - `answer` 是已流出的半截正文（可能为空 —— 那种情况前端只显示进度与角标）
/// - `reasoning` 包含被中断那一轮的思考（[`absorb_reason`] 已在返回前并入）
/// - `pending_steers` 留空：没送达的插话由 `lib.rs` 从收件箱取走后再回填
fn interrupted_run(
    steps: Vec<AgentStep>,
    steers: Vec<SteeredMsg>,
    reasoning: String,
    answer: String,
    iterations: usize,
    usage: TokenUsage,
) -> AgentRun {
    AgentRun {
        answer,
        steps,
        iterations,
        reasoning: (!reasoning.trim().is_empty()).then_some(reasoning),
        interrupted: true,
        steers,
        pending_steers: Vec::new(),
        stop_reason: Some("已停止".into()),
        usage,
    }
}

/// 取走某一轮累积的正文（空则返回空串）
fn take_text(acc: &std::sync::Mutex<String>) -> String {
    acc.lock().map(|g| g.clone()).unwrap_or_default()
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
    summarize_args_pub(raw)
}

/// 给 tools / perm 层用的参数摘要（截断 JSON）
pub fn summarize_args_pub(raw: &str) -> String {
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
///   自动加载技能     用户本轮消息命中关键词才出现 → 每轮都可能变
///   （前台上下文由 agent::run 追加，每轮都变 → 绝对最尾）
/// ```
///
/// 组装 prompt（不含本轮用户消息）—— 等价于 `build_system_prompt_for(dir, "")`。
/// 保留此签名给不关心技能自动加载的调用方（测试等）。
pub fn build_system_prompt(data_dir: &Path) -> String {
    build_system_prompt_for(data_dir, "")
}

/// 组装 system prompt，并按 `user_message` 的关键词**自动加载**命中的技能正文。
///
/// 机制见 `skills::autoload`：命中的技能全文作为**最尾**的动态段追加，
/// 模型直接照做，无需再调 `load_skill`。
pub fn build_system_prompt_for(data_dir: &Path, user_message: &str) -> String {
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

    // 激活项目（顶栏切换）→ 注入项目 MEMORY.md（跳脱主对话）
    {
        let s = crate::config::load_settings(data_dir);
        if let Some(pid) = s.active_project_id {
            if let Ok(list) = crate::pmem::list_projects(data_dir) {
                if let Some(p) = list.into_iter().find(|x| x.id == pid) {
                    if let Some(pm) = crate::pmem::project_memory_md(data_dir, &p.name) {
                        stable.push(format!(
                            "# 项目记忆（{name}）\n\n{pm}\n\n\
                             当前处于项目「{name}」。\n\
                             **你必须主动维护这份项目记忆**：\n\
                             1. 结论、路径、术语、未决问题 → 本轮用 `remember` 写入（直接落盘）\n\
                             2. 发现过时/错误条目 → `remember` 写「更正：…」并点明作废哪条\n\
                             3. 不要写全局 MEMORY；主对话闲聊不进项目记忆\n\
                             4. 每得出可复用结论就记，不要攒到最后",
                            name = p.name
                        ));
                    } else {
                        stable.push(format!(
                            "# 项目（{name}）\n\n\
                             当前处于项目「{name}」，项目记忆为空。\n\
                             **必须用 `remember` 维护**（结论/路径/未决问题 → projects/{name}/MEMORY.md）。",
                            name = p.name
                        ));
                    }
                }
            }
        }
    }

    // ── 稳定区：记忆使用规则（纯规则文字，条目本身在动态区）────────────
    stable.push(
        "# 记忆怎么用\n\n\
         `memory/MEMORY.md` 是**已批准**的跨项目长期记忆；\
         `projects/<名>/MEMORY.md` 是项目记忆（顶栏切换项目后整段注入）。\n\n\
         当你从对话里学到**值得跨会话记住**的东西时，调用 `remember` —— \
         **主对话**进全局候选审批；**已切换项目**直接写项目 MEMORY.md，且结论/路径/未决问题**必须写**。\n\n\
         该记什么：\n\
         - 用户明确说过的偏好、雷区、称呼、工作习惯\n\
         - 踩过的坑与验证过的做法（要能复用，不是流水账）\n\
         - 用户确认过的决策/约定\n\
         不该记：临时状态、一次路径、编造的猜测、尚未确认的推断。\n\n\
         记不清某事时：先查长期记忆与项目记忆，再调 `recall_turns`，仍没有再问用户 —— 不要瞎编。\n\n\
         ## 做完后提议记忆\n\n\
         当一轮任务**完成**（或失败已收尾）时，若本轮产生了值得跨会话复用的内容，\
         **先**用 `remember` 提交候选，再结束回复：\n\
         - **做过什么**：可复用的结论/做法（不是流水账）\n\
         - **错因**：若失败或踩坑，写清原因与规避方式\n\
         不要每次闲聊都提；不要写未验证的猜测。用户会在设置›记忆里批/驳。\n\n\
         ## 解决不了时分发\n\n\
         任务明显超出本助手时：若技能清单里有 **dispatch-workbuddy**，先 `load_skill` 再按手册执行。\
         核心动作是调用 `dispatch_task`（target=`workbuddy`）——总结会话有效消息并附图写交接、尝试打开；\
         **第一轮由用户手动**把文档交给 WorkBuddy，这即算分发完成。不要假装 WB 已接手，也不要硬扛半吊子交付。"
            .to_string(),
    );

    // 网页搜索（仅说明规则；工具是否出现由 tool_specs 按配置决定）
    stable.push(
        "# 网页抓取 fetch_url\n\n\
         工具列表里始终有 `fetch_url`：给一个 http/https URL，返回可读正文（Markdown 子集）。\n\
         静态文档/博客用它；需要点击、登录、JS 渲染的页面改走 playwright MCP 组。\n\
         不知道 URL 时先 `web_search`。URL 会直接请求目标站点。\n\n\
         # 网页搜索 web_search\n\n\
         当工具列表里**出现** `web_search` 时，说明用户已在设置里配置了搜索后端（默认 Tavily）。\n\
         用于：公开事实、文档版本、知识截止之后的新闻等。\n\
         不用于：用户偏好/约定/本机路径/做过的事 —— 那些在长期记忆、项目记忆与 recall_turns。\n\
         隐私：**查询词会发送到所选搜索服务商**（Tavily/Exa/Brave），不要把密钥、隐私原文当 query 搜。\n\
         若工具不存在，说明未配置搜索 —— 不要编造「已搜索」，直接说明无法联网检索。"
            .to_string(),
    );

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
         你有文件工具（read_file / list_dir / glob_files / grep_files / write_file / edit_file / delete_file）\
         以及 move_file / copy_file / mkdir / file_info / append_file / git（只读），\
         命令工具 `run_command`（在用户机器上跑 **PowerShell** 命令），\
         以及 `fetch_url`（抓网页正文）。\n\
         **所有文件操作都受文件权限网关管辖**，越权会被拒绝。\n\
         ## 工具选择硬规则（禁止用 PowerShell 绕过）\n\
         - **读文件 / 搜内容 / 列目录** → 只用 `read_file` / `grep_files` / `glob_files` / `list_dir` / `file_info`。\n\
           **禁止**用 `run_command` 跑 `Get-Content` / `Select-String` / `findstr` / `Get-ChildItem` 去读搜文件。\n\
         - **改文件** 优先 `edit_file`（精确字符串局部替换），不要整文件覆盖；\
         写新文件/全文重写才用 `write_file`（写前会备份）；追加用 `append_file`。\n\
         - **复制 / 移动 / 建目录** → `copy_file` / `move_file` / `mkdir`，不要 `Copy-Item`/`Move-Item`/`New-Item`。\n\
         - **git 只读**（status/diff/log/show/branch）→ 用 `git` 工具。\n\
         - `run_command` **只用于**编译器、包管理器、自定义脚本、或确实没有对应内置工具的场景。\n\
         `delete_file` = **永远归档**到 `agent-data/.trash/`（需 full 权限），\
         **不会硬删**；用户点名硬删也只归档，物理删除请用户自行执行。\n\
         不要为了改/删文件去 `run_command` 绕过工具层 —— 危险命令仍会被硬阻断。\n\
         `run_command` 受**命令权限策略**管辖：\n\
         - 只读命令（`git status` / `ls` / `rg` 等）直接放行；\n\
         - 危险命令（递归删除 / 格式化 / 关机 / 改注册表等）被**硬性阻断** —— \
         连申请都不行，让用户手工做；\n\
         - 其余命令会弹**确认卡片**让用户当场拍板。用户拒绝时会给出理由，\
         **按理由调整**（换做法），不要原样重复同一条命令。\n\
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
         - 系统会按用户**本轮消息的关键词自动加载**命中的技能，\
         全文出现在**文末「本轮自动加载的技能」**区 —— 那些直接照做，**不要再调 load_skill**。\n\
         - 清单里**没被自动加载**、但你觉得确实匹配当前任务的，再调 load_skill 读全文，\
         严格按步骤执行，不要凭清单里那行描述猜细节。\n\
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

    // ── 动态尾部 ③：本轮自动加载的技能（方案 B）─────────────────────────
    // 按用户本轮消息的关键词自动命中 → 命中技能全文直接进 prompt，模型无需
    // 再调 `load_skill`。这是**每轮都可能变**的内容 → 必须排在所有低频内容之后。
    let autoloaded = crate::skills::autoload(data_dir, user_message);
    if !autoloaded.is_empty() {
        dynamic.push(autoloaded);
    }

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

    // ---------------- 排障：导出真实 system prompt ----------------

    /// 把**真实数据目录**下组装出来的 system prompt 原样打印出来。
    ///
    /// 为什么要这个：prompt 是分段拼的（RULES / SOUL / IDENTITY / USER / 记忆 /
    /// 技能 / 工作目录 / 自动加载技能 / 前台上下文），光看代码猜"到底发了什么"
    /// 很容易漏掉顺序、动态段和长度占比。排障时先看全文最省事 ——
    /// 比如"为什么它不知道本机没有 py 启动器"，翻开一看就知道 prompt 里压根
    /// 没有环境事实段。
    ///
    /// 用法：`cargo test --lib dump_real_system_prompt -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_real_system_prompt() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-data");
        let p = build_system_prompt(&dir);
        println!(
            "===== SYSTEM PROMPT BEGIN ({} 字符, 数据目录 {}) =====",
            p.chars().count(),
            dir.display()
        );
        println!("{p}");
        println!("===== SYSTEM PROMPT END =====");
    }

    // ---------------- 时间线：思考与工具必须交错 ----------------

    #[test]
    fn reasoning_steps_interleave_with_tools_in_order() {
        let mut total = String::new();
        let mut steps: Vec<AgentStep> = Vec::new();

        // 复现主循环的顺序：本轮思考 → 本轮工具 → 下一轮思考
        let r1 = std::sync::Mutex::new("第一轮：先看目录".to_string());
        absorb_reason(&mut total, 1, &r1, &mut steps);
        steps.push(AgentStep {
            kind: "tool_call".into(),
            name: Some("list_dir".into()),
            detail: "{}".into(),
        });

        let r2 = std::sync::Mutex::new("第二轮：再看 sessions.rs".to_string());
        absorb_reason(&mut total, 2, &r2, &mut steps);

        let kinds: Vec<&str> = steps.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["reasoning", "tool_call", "reasoning"],
            "思考条目必须按发生顺序与工具交错（以前只有一个累计串，必然堆成一块）"
        );
        assert_eq!(steps[0].detail, "第一轮：先看目录");
        // 旧字段继续维护：老会话/回滚安全
        assert!(total.contains("【第 1 轮】") && total.contains("【第 2 轮】"));
    }

    #[test]
    fn absorb_reason_skips_empty_rounds() {
        let mut total = String::new();
        let mut steps: Vec<AgentStep> = Vec::new();
        let empty = std::sync::Mutex::new("   \n ".to_string());
        absorb_reason(&mut total, 3, &empty, &mut steps);

        assert!(steps.is_empty(), "空思考不该留下空条目");
        assert!(total.is_empty(), "空思考也不该留下空的「【第 N 轮】」标题");
    }

    #[test]
    fn status_notes_land_in_timeline() {
        let log = std::sync::Mutex::new(vec![
            "接口限流/临时故障（HTTP 429），4s 后自动重试（第 1/4 次）…".to_string(),
        ]);
        let mut steps: Vec<AgentStep> = Vec::new();
        drain_status_steps(&log, &mut steps);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, "status");
        assert!(steps[0].detail.contains("429"), "重试原因要留下");
        assert!(log.lock().unwrap().is_empty(), "取走即清空，别重复落");
    }

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

    /// 方案 B：用户消息命中技能关键词时，正文应自动出现在 prompt 的动态尾部，
    /// 且不得破坏"静态在前、动态在后"的前缀稳定性。
    #[test]
    fn system_prompt_autoloads_matched_skill() {
        let tmp = std::env::temp_dir().join("float_agent_prompt_autoload");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULES_BODY_UNIQUE").unwrap();
        let sdir = tmp.join("skills").join("self-rebuild");
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(
            sdir.join("SKILL.md"),
            "---\nname: self-rebuild\ndescription: 重建时用。触发：用户说「重新构建」。\n---\n\n# 步骤\n先杀进程\n",
        )
        .unwrap();

        // 无消息 → 不自动加载（用自动段独有的 skill 小标题当标记）
        let plain = build_system_prompt(&tmp);
        assert!(!plain.contains("## 已加载技能："), "空消息不应自动加载");

        // 命中消息 → 正文进 prompt，且排在静态内容之后
        let hit = build_system_prompt_for(&tmp, "帮我重新构建一下");
        assert!(hit.contains("## 已加载技能：self-rebuild"), "{hit}");
        assert!(hit.contains("先杀进程"), "应载入正文:\n{hit}");
        let rules_at = hit.find("RULES_BODY_UNIQUE").unwrap();
        let auto_at = hit.find("## 已加载技能：").unwrap();
        assert!(rules_at < auto_at, "自动加载段必须在静态内容之后");

        // 两次求值仍字节稳定
        assert_eq!(plain, build_system_prompt(&tmp), "空消息路径须字节稳定");

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

        // 这个 ignored 测试没人接申请卡，撞墙只会走超时（fail-closed）。
        // 绑成变量而不是写临时值 —— 临时值跨 `.await` 容易踩借用生命周期的坑。
        let perm = crate::perm_request::PermHub::new();

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
            // 手工联调不走插话通道
            &RunHub::new(),
            &perm,
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
