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
/// 覆盖方式：`ORBCAT_MAX_ITERATIONS=100`。
/// 为什么做成可配：不同模型/任务对轮数需求差别很大（简单问答 3 轮够，
/// 长链路重构可能几十轮），编译期写死一个数字必然有一边不合适。
const DEFAULT_MAX_ITERATIONS: usize = 100;

/// 硬上限（默认值；可被环境变量覆盖）：插话可以**延长**预算，但不能无限延长。
///
/// 不做硬顶的话，"用户一直插话"会让基础轮数变成事实上的无上限 —— 既烧钱，
/// 也真的可能转不出来。到了这里仍然按"任务过大/模型陷入循环"报错。
///
/// 覆盖方式：`ORBCAT_HARD_ITERATIONS=250`。
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
                eprintln!("[orbcat] 环境变量 {name}={v:?} 非法（需正整数），回退默认 {default}");
                default
            }
        },
        Err(_) => default,
    }
}

/// 基础轮数上限（读环境变量，每次调用求值 —— 便于测试与运行时调整）
pub fn max_iterations() -> usize {
    env_usize("ORBCAT_MAX_ITERATIONS", DEFAULT_MAX_ITERATIONS)
}

/// 硬上限（读环境变量）
pub fn hard_iterations() -> usize {
    env_usize("ORBCAT_HARD_ITERATIONS", DEFAULT_HARD_ITERATIONS)
}

/// 每消费一批插话，给预算加多少轮
const STEER_BONUS: usize = 6;

/// 「输出被 max_tokens 截断」自动续写的最大次数（防死循环）
const MAX_LENGTH_CONTINUES: usize = 3;

/// 「工具调用被写成正文」时提醒重发的最大次数（防死循环）
const MAX_PSEUDO_NUDGES: usize = 2;

/// 「回合结束时没有可用正文」自动续跑的最大次数（防死循环）。
///
/// 抄 MiMo/WB 的 `INVALID_OUTPUT_CONTINUATION_LIMIT`（默认 2，env 可覆盖）：
/// 模型只吐了思考、或只回一句「已完成 / 收到」就收工 —— 那不是答案。
/// 判定为 invalid 后注入一条合成 user 消息让它**接着答**，而不是把空应答当结论交给用户。
///
/// 为什么必须有这一层（2026-10-01 定论）：提示词只能堵"态度问题"，
/// 真正确保"最后一定有一句正经回答"的是**运行时判定** —— 提示词是软约束，
/// 模型可以不理；这里是硬约束，它没有绕过的余地。
const MAX_INVALID_OUTPUT_CONTINUES: usize = 2;

/// 正文短于这个字符数、且命中「应答式收尾」特征时，判为无效输出。
///
/// 为什么要卡长度：长正文里出现「已完成」多半是在正常叙述（比如汇报进度），
/// 只有**极短的**正文才可能是"光说一声就收工"。这是防误伤的关键闸门。
const ACK_ONLY_MAX_CHARS: usize = 80;

/// 这句正文是不是「应答式收尾」——说了一声，但没给用户任何答案。
///
/// 覆盖两类：① 中文的「已完成 / 已回答 / 好的 / 收到」等纯确认；
/// ② 英文的 Done / Task complete / I've answered 等。
///
/// ⚠️ 只在**正文很短**（≤ [`ACK_ONLY_MAX_CHARS`]）时才认，见调用点。
fn is_acknowledgement_only(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    // 正文里带了代码块 / 列表 / 表格 —— 那是在交付内容，不是在打官腔
    if t.contains("```") || t.contains("| ") || t.lines().count() > 3 {
        return false;
    }
    // ★ 长度闸门（防误伤的关键）：长正文里出现「已完成」多半是在正常叙述
    //   （比如汇报进度）。只有**极短的**正文才可能是"光说一声就收工"。
    if t.chars().count() > ACK_ONLY_MAX_CHARS {
        return false;
    }
    // 去掉标点与空白后再比对，避免「已完成！」「Done.」这类尾巴影响判定
    let core: String = t
        .chars()
        .filter(|c| !c.is_whitespace() && !"，。！？、；：,.!?;:~～-—…「」“”\"'()（）".contains(*c))
        .collect::<String>()
        .to_lowercase();
    if core.is_empty() {
        return true;
    }

    const CN_ACKS: &[&str] = &[
        "已完成", "已回答", "已回复", "已处理", "已归档", "已记录", "已保存", "已更新",
        "已完成任务", "任务完成", "完成了", "好了", "好的", "收到", "明白", "了解",
        "没问题", "可以了", "搞定了", "处理完毕", "操作完成", "如上", "见上", "同上",
    ];
    const EN_ACKS: &[&str] = &[
        "done", "taskcomplete", "completed", "answered", "finished", "alldone",
        "acknowledged", "noted", "gotit", "okay", "ok",
    ];

    CN_ACKS.contains(&core.as_str())
        || EN_ACKS.contains(&core.as_str())
        // 「已完成。」这种带主语的变体：核心串以确认词开头且整体极短
        || (core.chars().count() <= 12
            && (CN_ACKS.iter().any(|a| core.starts_with(a))
                || EN_ACKS.iter().any(|a| core.starts_with(a))))
}

/// 「把答案指向别处」类正文的长度闸门。
///
/// 比 [`ACK_ONLY_MAX_CHARS`] 宽得多：这类收尾往往带一两句总结，不止一句确认语
/// （2026-10-02 那次实测是 183 字符）。但真正完整的答案通常远超这个长度，
/// 所以仍然卡一个上限来防误伤。
const DEFERRAL_MAX_CHARS: usize = 600;

/// 这句正文是不是「把答案指向别处」——没说答案，只说「答案在别处 / 我已经答过了」。
///
/// 这是用户报的**原始问题**（「最后说已回答」）。2026-10-02 拿到实测复现：
/// 会话 `s1790916789470-0` 第 6 条，模型跑完 **41 步**分析后，最后只写了 183 字符：
///
/// > 结论已给出（缓存命中与未命中分开计价…），完整表格和铁证数据见上一条回复。
///
/// 而这一轮它**从未写过任何表格或结论** —— 那条 20819 字的分析全在 reasoning 里。
/// 它在第 39 步的思考里明说：
/// `The final answer was already given in my previous message with the full tables
/// and evidence.` —— **它把自己的思考当成了已交付的回答**，于是指向一个不存在的回复。
/// 用户当场追问「结论给到哪里了」。
///
/// ⚠️ 这正是 WB 提示词那句 `Assume users can't see most tool calls or thinking —
/// only your text output` 要防的事：模型以为思考过程会显示给用户。
///
/// 与 [`is_acknowledgement_only`] 的分工：那个管「太短的空话」，这个管
/// 「篇幅够长但在指路」。两者都会被判为无效输出。
fn is_deferral_only(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        // 空正文交给 is_acknowledgement_only 管，这里不重复判
        return false;
    }
    // 自己交付了内容（代码块 / 表格）——「见上表」指的就是本条消息里的表，合法
    if t.contains("```") || t.contains("| ") {
        return false;
    }
    // 很长的正文一般认为自带答案，不判（防误伤）
    if t.chars().count() > DEFERRAL_MAX_CHARS {
        return false;
    }

    // 指向别处 / 声称已答过的说法。命中任意一条即认为是「指路而非作答」。
    const DEFERRAL_MARKERS: &[&str] = &[
        // ① 声称「我已经答过了」
        "结论已给出", "结论已经给出", "已给出结论", "答案已给出", "答案已经给出",
        "已给出答案", "已回答", "已经回答", "已经答复",
        // ② 指向别的消息（注意：**不**收录「上一轮」这种正常对比说法）
        "见上一条", "见上条", "上一条回复", "上条回复", "上一条消息", "上条消息",
        "前一条回复", "之前的回复", "前面的回复", "上一条回答", "上条回答",
        "见上文", "详见上文", "参见上文", "见前文", "如上所述",
        "前面已说明", "之前已说明", "上文已说明", "前面说过", "之前提过",
        // ③ 英文
        "already answered", "as above", "see above", "given above",
        "mentioned above", "previous message", "see my previous", "stated above",
    ];

    let lower = t.to_lowercase();
    DEFERRAL_MARKERS.iter().any(|m| lower.contains(m))
}

/// 「把中途正文提升为最终答复」要求的最短长度。
///
/// 太短的多半是「我先看一下文件」这种进度话，拿它当结论反而更差 ——
/// 那种情况退回注入提醒，让模型重写。
const MIDTURN_PROMOTE_MIN_CHARS: usize = 200;

/// 本轮里模型中途写过、后来因为「接着调了工具」而被归档的最后一段**实质**正文。
///
/// 为什么需要它（**根因**，2026-10-02 定位）：
/// 模型写正文 + 同一轮接着调工具时，`DiscardStream` 只让**前端**把那段正文
/// 淡化归档（标注「模型中途决定调用工具，以下内容不是最终答复」），
/// 但它**没有从 `messages` 里删掉** —— 模型自己的历史里那段话完好无损。
/// 于是两边对「用户看到了什么」的认知不一致：
/// 模型以为「结论我已经给过了」，收尾就只写「完整表格和铁证数据见上一条回复」；
/// 而用户只在折叠的「过程」区里见过一句被标注成"不是最终答复"的话，
/// 根本不会去那里翻。用户当场追问「结论给到哪里了」。
///
/// 既然那段正文还在手上，与其再烧一轮让模型重写（还可能再写一次「见上一条」），
/// 不如直接把它提升为最终答复 —— 结论必须落地。
///
/// 只在**本轮**范围内找：倒着扫，遇到 user 消息就停（那是本轮起点），
/// 免得把上一轮的旧结论捞回来当答案。
fn last_substantive_midturn_text(messages: &[ChatMessage]) -> Option<String> {
    for m in messages.iter().rev() {
        if m.role == "user" {
            break;
        }
        if m.role != "assistant" {
            continue;
        }
        let Some(content) = m.content.as_ref() else {
            continue;
        };
        let plain = content.as_plain();
        let t = plain.trim();
        if t.chars().count() < MIDTURN_PROMOTE_MIN_CHARS {
            continue;
        }
        // 指路 / 应答本身不是答案，不能拿来提升
        if is_acknowledgement_only(t) || is_deferral_only(t) {
            continue;
        }
        return Some(t.to_string());
    }
    None
}

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
/// ## ⚠️ `rename_all_fields` 不能省（2026-09-26 修 bug）
///
/// serde 对**枚举**的 `rename_all` 只重命名**变体名**（`ToolTick` → `toolTick`），
/// **不重命名字段**。本枚举里 `elapsed_secs` 是唯一的多词字段，只写 `rename_all`
/// 时它序列化出去仍是 `elapsed_secs`，而前端读的是 `p.elapsedSecs` → 永远
/// `undefined ?? 0` → 长命令心跳永远显示「已运行 0s」（用户截图实报）。
/// `rename_all_fields` 才是"重命名所有变体的字段"的那个属性。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
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
    /// 工具执行**还在跑**（长命令每 2s 一次心跳）—— 前端据此刷"已运行 N 秒 + 输出尾巴"，
    /// 消掉「界面卡在 run_com 步骤不动」的假死感（2026-09-25）。
    ToolTick {
        name: String,
        elapsed_secs: u64,
        /// 输出尾巴（单行，可能为空）
        tail: String,
    },
    /// 工具执行失败
    ToolError { name: String, message: String },
    /// 准备输出最终答案（流式下已经边收边发了，这里只作为收尾信号）
    Answering,
    /// 本轮流了正文出来，但模型最终决定去调工具 —— 前端把那半截话淡化掉
    DiscardStream { reason: String },
    /// 执行中「插话」的用户消息**已被消费**（前端把「排队中」改成「已送达」）
    ///
    /// `images` 是插话附带的图片（文件路径）。为什么一并推给前端：插话的图
    /// 在对话流里不铺独立气泡（见 `steer_id` 的设计），只在**时间线的
    /// `kind=steer` 条目**上画 —— 少了它，用户在"插话里带图"时只能看到文字
    /// （2026-09-26 用户报的 bug1）。
    Steer {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
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

// ---------------------------------------------------------------------------
// 多会话并行 run：按 session_id 分槽的运行时注册表（2026-09-26 用户拍板）
// ---------------------------------------------------------------------------

/// 一轮执行的运行时句柄。**每个会话一个**，互不干扰。
///
/// 为什么不是全局单例：用户要"切到别的会话继续干活"。全局单例时
/// `chat_steer` 只有一个收件箱、`cancel` 只有一个标志 —— 停 A 会把 B 一起停掉，
/// 插话也会串到另一个会话去。见 [`RunRegistry`]。
pub struct RunSlot {
    pub hub: std::sync::Arc<RunHub>,
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Clone for RunSlot {
    fn clone(&self) -> Self {
        Self {
            hub: self.hub.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

/// 并发 run 注册表：`session_id → RunSlot`。多个会话可以同时跑各自的 agent loop。
#[derive(Default)]
pub struct RunRegistry {
    slots: std::sync::Mutex<std::collections::HashMap<String, RunSlot>>,
}

impl RunRegistry {
    /// 开跑前登记：拿到本会话专属的插话收件箱 + 取消标志。
    ///
    /// 同一会话重复 `begin`（理论上不该发生）时**复用同一个槽**，不覆盖 ——
    /// 否则上一个 run 的插话和取消就都失效了。
    ///
    /// ⚠️ 必须调 `hub.begin()`：它负责把 `active` 置位并清空残留收件箱。
    ///    漏了它 `RunHub::push` 会一直返回"当前没有进行中的对话"（插话全被拒）。
    pub fn begin(&self, session_id: &str) -> RunSlot {
        let mut g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = g
            .entry(session_id.to_string())
            .or_insert_with(|| RunSlot {
                hub: std::sync::Arc::new(RunHub::new()),
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
            .clone();
        // 复用槽位时也要重置状态：清掉上一轮残留 + 打开收件箱
        slot.hub.begin();
        slot.cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);
        slot
    }

    /// 收尾：取走残留插话（调用方回填给前端）并移除槽位。
    /// 无论成功、失败、中断都要调。
    pub fn end(&self, session_id: &str) -> Vec<SteeredMsg> {
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        match slot {
            Some(s) => s.hub.end(),
            None => Vec::new(),
        }
    }

    pub fn is_active(&self, session_id: &str) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(session_id)
    }

    /// 是否有**任意**会话在跑（重载授权这类全局动作要靠它避让）
    pub fn any_active(&self) -> bool {
        !self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// 当前所有在跑的会话 id（`chat_cancel` 不带 sid 时全停用）
    pub fn active_ids(&self) -> Vec<String> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    pub fn active_count(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 插话路由到**目标会话**的收件箱
    pub async fn push(&self, session_id: &str, m: SteeredMsg) -> Result<SteerAck, String> {
        let hub = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|s| s.hub.clone())
            .ok_or_else(|| "该会话没有进行中的对话".to_string())?;
        hub.push(m).await
    }

    /// 停掉**目标会话**的 run；返回是否确实停到了一个。
    pub fn cancel(&self, session_id: &str) -> bool {
        let g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(session_id) {
            Some(s) => {
                s.cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

/// 进度回调。用 Arc<dyn Fn> 而不是泛型，避免把泛型参数污染到整个 agent loop。
pub type ProgressFn = std::sync::Arc<dyn Fn(Progress) + Send + Sync>;

/// **轮内增量落盘**的回调（2026-09-26 加，治「退出后整轮蒸发」）。
///
/// 参数：`(目前已产出的步骤, 目前流出的正文, 目前的思考过程)`。
/// agent loop 在每个轮边界、以及每个工具跑完后调用；`lib.rs` 收到后写进会话里
/// 那条 `partial` 助手占位（见 `sessions::checkpoint_turn`）。
///
/// 为什么不用 `ProgressFn` 顺手做：`Progress` 是**面向 UI 的增量事件**
/// （单个 delta / 单条工具），要拿它还原"当前完整步骤"得再养一份影子状态，
/// 容易漂移。这里直接给完整快照 —— 写进磁盘的就是最终形态。
///
/// ⚠️ 实现方必须**快速返回**：它在 loop 里被串行调用，慢了会拖慢整个 agent。
pub type CheckpointFn = std::sync::Arc<dyn Fn(&[AgentStep], &str, &str) + Send + Sync>;

/// 触发一次轮内增量落盘（没配回调就什么都不做）。
///
/// 位置刻意选在**轮边界**与**每个工具跑完后** —— 这两处正好覆盖
/// "长命令卡住时用户关掉进程"的场景：磁盘上至少留下"问了什么 + 跑过哪几步"。
fn checkpoint(cb: &Option<CheckpointFn>, steps: &[AgentStep], answer: &str, reasoning: &str) {
    if let Some(f) = cb.as_ref() {
        f(steps, answer, reasoning);
    }
}

/// 流式正文的落盘节流间隔（毫秒）。
///
/// ## 为什么需要"流式落盘"（2026-09-30 补的缺口）
///
/// 已有的 [`checkpoint`] 只在**轮边界**与**工具跑完后**触发。但一轮模型调用
/// 可能流式吐几十秒到几分钟（长思考 + 长输出实测很常见），这段时间里
/// **正文一个字都没落盘** —— 分片只进了内存累加器 `round_text`。
/// 用户在这期间关掉进程（或进程崩溃），那段正文就真的没了：
/// 磁盘上只有一条空的 `partial` 占位。
///
/// ## 为什么是节流而不是每个分片都写
///
/// 分片来得很密（几十毫秒一片），每片都 `write_session` 就是**每秒钟重写
/// 整个会话 JSON 好几次** —— 会话文件可达几百 KB，那是拿磁盘寿命和 UI 卡顿
/// 换"最多少丢半秒的字"。3 秒是个折中：最坏丢 3 秒的打字量，代价可忽略。
const CHECKPOINT_THROTTLE_MS: u64 = 3_000;

/// 从流式回调里做节流落盘。
///
/// 抽成独立函数是为了让"回调里的落盘"只有一处 —— 回调会被 `llm.rs` 在
/// `.await` 内部调用，逻辑越少越不容易踩到跨 await 持锁那类坑。
///
/// `last` 是上次落盘的时刻（epoch 毫秒），`0` 表示"本轮还没落过"。
fn checkpoint_from_stream(
    cb: &Option<CheckpointFn>,
    last: &std::sync::atomic::AtomicU64,
    steps: &[AgentStep],
    text: &str,
    reasoning: &str,
) {
    use std::sync::atomic::Ordering;
    if cb.is_none() {
        return;
    }
    // 空文本不写：那只是"思考分片来了"，没有新增正文可保
    if text.is_empty() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = last.load(Ordering::Relaxed);
    if prev != 0 && now.saturating_sub(prev) < CHECKPOINT_THROTTLE_MS {
        return;
    }
    // 先记时刻再落盘：落盘慢（写整个会话文件）时不让回调排队等
    last.store(now, Ordering::Relaxed);
    checkpoint(cb, steps, text, reasoning);
}

/// 取本轮已流出正文的**副本**（不清空累加器 —— 后面还要用它当半截正文返回）。
fn peek_text(acc: &std::sync::Mutex<String>) -> String {
    acc.lock().map(|g| g.clone()).unwrap_or_default()
}

/// 这段中途正文能不能落成时间线条目 —— 能就给 `Some(原文)`，纯空白给 `None`。
///
/// ## 为什么要落盘（2026-10-02 修）
///
/// 模型写了正文又接着调工具时，此前只发一条 `Progress::DiscardStream` 让前端
/// 把那半截话"淡化归档" —— **后端从没把它写进 `steps`**。于是在前端它只活在
/// 运行中气泡的内存里（`archiveStreamToItems` 本地 push 的 `text` 条目），
/// 一轮跑完、前端按磁盘重建消息时**凭空消失**（用户报的"我的中途正文呢"）。
///
/// 落成 `kind = "text"` 之后，回看时它还在「过程」折叠外壳里 —— 正好符合
/// 用户定的「中途正文在中途展开、其余时候收起」。
///
/// 返回**原文**（只做"空不空"的判断，不 trim 内容）：正文里的缩进/换行是模型
/// 排版的一部分，这里不该动它。
fn midturn_detail(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
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
        images: Vec::new(),
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
            images: Vec::new(),
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
    /// 图片文件路径。目前只有 `kind == "steer"` 会用到（插话里带的图）——
    /// 插话不铺独立气泡，图只能画在时间线这条 steer 条目上。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
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
    /// **最后一轮**模型调用实际发出的 prompt token（服务端 `usage.prompt`）。
    ///
    /// 与 `usage.prompt`（整个 run 求和，几百万很常见）不是一回事：
    /// 这一个数代表"此刻上下文有多大"，是下一轮预算与 compact 水位的真锚点。
    /// 服务端不给 usage → None（调用方保留上一轮的值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_tokens: Option<u32>,
    /// 本轮「真实 prompt ÷ 本地估算」倍率，供下一轮自校准。None = 未校准。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_scale: Option<f64>,
}

/// 一轮对话的**校准累加器**：把"本地估算 vs 服务端真实值"的偏差记下来，
/// 供下一轮（乃至下一次进程启动）把预算算准。
///
/// ## 为什么必须有这个（2026-09-30）
///
/// 上下文预算此前只靠 `estimate_messages_tokens`，而实测**低估 5~144 倍**
/// （工具 schema、MCP 已加载组、reasoning 原文都没进估算）。后果：
/// 配了 `maxInputTokens: 1000000` 的模型水位是 60 万，而估算值永远够不着
/// —— 实测 30 个真实会话里自动 compact **一次都没触发过**。
///
/// 现在用服务端返回的 `usage.prompt`（真值）反推一个倍率：
/// `倍率 = 真实 prompt ÷ 本地估算`。下一轮把估算乘以它，量级就对了。
#[derive(Debug, Clone, Copy, Default)]
struct Calibration {
    /// 最后一个模型调用实际发出的 prompt token
    last_prompt: Option<u32>,
    /// 本轮最后一次可用的「真实 ÷ 估算」倍率
    scale: Option<f64>,
}

/// 倍率的允许区间。
///
/// 为什么要有上下界：单次调用的估算误差可能极端（比如历史几乎全是
/// JSON 工具输出时，`char/4` 的经验公式会偏得离谱）。倍率是要**存进会话
/// 文件、长期复用**的，一个荒谬值会让后续所有轮次的预算全错 ——
/// 与其相信离群点，不如夹到保守区间里。
const TOKEN_SCALE_MIN: f64 = 1.0;
const TOKEN_SCALE_MAX: f64 = 20.0;

impl Calibration {
    /// 用一次模型调用的结果更新校准。
    ///
    /// `estimated` = 发出这次请求**之前**本地估算的 messages token 数
    /// （不含 system prompt / 工具 schema —— 那部分由调用方从真实值里减掉）。
    fn observe(&mut self, real_prompt: u32, estimated_overhead: usize, estimated_msgs: usize) {
        if real_prompt == 0 {
            return;
        }
        self.last_prompt = Some(real_prompt);
        // 只有"真实值明显大于固定开销"时倍率才有意义 —— 否则分母趋零，
        // 算出来的倍率会被 system prompt 那点固定成本主导（虚高）。
        let real_msgs = real_prompt as f64 - estimated_overhead as f64;
        if real_msgs <= 0.0 || estimated_msgs < 256 {
            return;
        }
        let s = real_msgs / estimated_msgs as f64;
        if s.is_finite() && s > 0.0 {
            self.scale = Some(s.clamp(TOKEN_SCALE_MIN, TOKEN_SCALE_MAX));
        }
    }

    /// 把估算值按已校准的倍率放大（未校准时原样返回）。
    fn apply(&self, est: usize) -> usize {
        match self.scale {
            Some(s) if s.is_finite() && s > 1.0 => ((est as f64) * s) as usize,
            _ => est,
        }
    }
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
    // 轮内增量落盘（可选）。`None` = 不增量写盘（测试）。
    on_checkpoint: Option<CheckpointFn>,
    foreground: Option<&crate::context::ForegroundContext>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_id: &str,
    // 执行中插话的收件箱
    hub: &RunHub,
    // 「申请权限」的授权表 + 待办通道，透传给工具层
    perm: &crate::perm_request::PermHub,
) -> Result<AgentRun, RunAbort> {
    use std::sync::atomic::Ordering;

    // PTC 模式下，system prompt 里要带上**程序内可用的工具声明**。
    // 它必须与模型看到的工具集同源（`tools::ptc_sdk_tools` 复用 `all_tool_specs`），
    // 而本函数是同步的、拿不到注册表 —— 所以在能拿到的这里算好传进去。
    // 非 PTC 模式不花这份开销（连注册表锁都不抢）。
    //
    // ⚠️ **已知局限（有意为之）**：这份声明在**一次 run 内是快照**，
    //    而下面的 `tools_arg` 是每轮重建的。两者不一致的场景很窄 ——
    //    PTC 下模型**不能**直接调 `load_tool_group`（工具已坍缩成 run_code），
    //    只有在**程序内部**主动 load 组时，本次 run 后续的 SDK 才会落后一轮。
    //    即便如此工具**仍能调用**（子调用走 `execute`，MCP 分支按活跃集动态解析），
    //    只是声明里少了那个名字。
    //
    //    为什么不每轮重算：system prompt 一旦重建就**打掉整段前缀缓存**
    //    （见本文件「前缀稳定性」的说明），而"程序里 load 一个新组"是低频动作。
    //    用户下次发消息（新 run）即恢复一致。
    let ptc_sdk: Option<String> = if crate::ptc::mode_active(data_dir, Some(session_id)) {
        let reg = mcp.lock().await;
        let ready = crate::web_search_ready_for(data_dir);
        let tools = tools::ptc_sdk_tools(Some(&reg), ready, data_dir, Some(session_id));
        Some(crate::ptc::sdk_declarations(&tools))
    } else {
        None
    };

    // 传入本轮用户输入 → 命中技能的正文会作为最尾动态段自动加载进来（方案 B）
    let mut system_prompt = build_system_prompt_full(
        data_dir,
        user_input,
        ptc_sdk.as_deref(),
        Some(session_id),
    );

    // ── 「记忆蒸馏」会话角色（2026-10-03）────────────────────────────────
    //
    // 蒸馏就是**一条普通聊天**：这个会话由 `distill_session_open` 开出来，
    // 带 `sessions/<id>.distill` 标记；命中时追加这段角色说明 + 注入
    // `distill_move` 工具，模型就在常规循环里读中转站、问用户、搬条目 ——
    // 思考与工具调用全走标准时间线（用户要求：「就是常规聊天」）。
    //
    // 为什么放**动态区**而不是 modes.json：模式是全局的（一改全改），
    // 而蒸馏必须是**独立会话**，不能把用户的主聊天也变成蒸馏助手。
    let distill = crate::sessions::is_distill(data_dir, session_id);
    if distill {
        system_prompt.push_str(
            "\n\n---\n\n# 你的角色：记忆蒸馏助手（本会话专用）\n\n\
             长期记忆有一个**中转站** `agent-data/memory/MEMORY.md`：审批通过的记忆先进那里，\
             由你逐条分流到三个归属文件，之后中转站只留真正「无处可去」的条目。\n\n\
             ## 三个目标文件与边界\n\
             - `USER.md`（用户画像）：**只写「人」** —— 身份、能力、性格、习惯、偏好、雷区、设备、\
             沟通与授权习惯。项目名、项目路径、项目进展、代码细节**禁止**入内。\n\
             - `SOUL.md`（人格/语气）：你应表现出的人格、语气、表达纪律。\n\
             - `IDENTITY.md`（称呼）：名字、互相怎么称呼、身份定位。\n\
             - 与目标文件现有内容重复 → `duplicate`（不写文件，只从中转站删）。\n\
             - 项目/环境/工具事实、无处可放 → `stay`（**保持不动**，别硬塞进上面三个）。\n\n\
             ## 工作方式（重要）\n\
             1. 先用 `read_file` 读 `agent-data/memory/MEMORY.md`（必要时也读三个目标文件，避免重复搬运）。\n\
             2. **先给结论、再动手**：把逐条建议（每条引用原文）摆出来，让用户过目。\n\
             3. **拿不准就问**，一次最多问 3 条，问清再动；用户答复后再改判。\
                         别为了「显得有进展」而硬判。\n\
             4. 用户确认后（或你判定明确无需确认时），用 `distill_move` **一条一条**迁移。\n\
             5. 全部处理完，给一份简短小结：迁了几条、各去了哪、哪些留在中转站、备份在哪。\n\n\
             ## 硬约束\n\
             - `distill_move` 的 `text` 必须与 MEMORY.md 里的条目逐字一致（不含行首 `- `）——\
             它靠这个匹配并摘除条目。\n\
             - 未经用户明确同意，不要迁移「拿不准」的条目。\n\
             - 项目记忆（`agent-data/projects/`）**不在这轮范围内**，除非用户要求。",
        );
    }

    // ── 数据源摘要（认知面注入）─────────────────────────────────────────
    //
    // 用户 2026-10 拍板「一行摘要常驻 + 详情按需」：这里只放每个源**一行**
    // （条数 + 新鲜度），细节要模型调 `life_items`。所以这段很小（几十 token）。
    //
    // 为什么放**动态区最尾**（而不是 `build_system_prompt_full` 内部）：
    // 它是 async 的（`command` 类源要起进程），而那个函数是同步的。
    // 与 PTC SDK 从外面传进来是同一个理由。
    //
    // 为什么要缓存：本函数每轮都会走一次，不缓存等于每轮付一次进程启动的钱
    // （见 `life::cached_summary` 的 TTL 说明）。
    let life_summary = crate::life::cached_summary(data_dir, data_dir).await;
    if !life_summary.is_empty() {
        system_prompt.push_str(&format!("\n\n---\n\n{life_summary}"));
    }

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

    // ---- 多轮上下文回灌（2026-10-07 定案：全量注入）----
    // 未压缩区间全量带回，选取规则见 history.rs 文档；预算由自动 compact
    // （0.6 水位）+ 循环内 75% 硬裁剪兜底。
    //
    // ⚠️ 时序关键：必须在 lib.rs::chat 调 `sessions::append_turn` **之前**读，
    //    否则当前这轮会被算进历史，模型看到自己的问题重复出现。
    let mut history = if session_id.is_empty() {
        Vec::new()
    } else {
        crate::history::build_history(data_dir, session_id)
    };

    // ---- 上下文预算：拿"真值优先"的口径定水位 ----
    //
    // ⚠️ 2026-09-30 重做（旧实现让 compact 永远触发不了）：
    //
    // 旧口径 = `估算(history)` vs `窗口 × 60%`。两个致命问题：
    //   ① 估算只算 history，**不含** system prompt 与工具 schema，
    //      而这两样本来就在每次请求里（56 个 MCP 工具约 14k token）；
    //   ② 本地 `char/4` 估算对 JSON / 工具输出 / 英文代码严重低估
    //      （实测对服务端真实 prompt 低估 5~144 倍）。
    // 结果：配 1M 窗口的模型水位是 60 万，而估算值几千到几万，
    // **永远够不着** —— 实测 30 个真实会话里带 summary 的是 0 个。
    //
    // 新口径 = 「上一轮服务端返回的真实 prompt」优先，估算只作兜底。
    //   真值存在会话里（`last_prompt_tokens` / `token_scale`），跨轮、跨重启都有效。
    let context_window = cfg
        .max_input_tokens
        .map(|n| n as usize)
        .unwrap_or(FALLBACK_CONTEXT_WINDOW);
    let (real_last_prompt, token_scale) = if session_id.is_empty() {
        (None, None)
    } else {
        crate::sessions::context_calibration(data_dir, session_id)
    };
    let mut calib = Calibration {
        last_prompt: real_last_prompt,
        scale: token_scale,
    };
    if let Some(p) = real_last_prompt {
        eprintln!(
            "[orbcat] 上下文真值: 上一轮实际 prompt {p} tok（本地倍率 {:?}），窗口 {context_window}",
            token_scale
        );
    }

    // ---- 自动 compact：历史太大先压缩，再回灌 ----
    //
    // 为什么在"发请求前"而不是"撞到上限时"：撞上限意味着这次请求已经废了
    // （413 / context_length_exceeded），而压缩要额外发一次模型请求 —— 必须在
    // 还有余量时做。摘要**落盘**，所以只在真正超水位时压一次，之后每轮直接读。
    let compact_trigger = ((context_window as f64) * crate::history::COMPACT_TRIGGER_RATIO) as usize;
    if !session_id.is_empty() {
        // 判定量 = 上一轮**真实** prompt（有的话）。它就是"现在上下文有多大"，
        // 比任何本地估算都可信；没有真值（新会话 / 网关不透传 usage）才退回估算，
        // 且此时按已校准的倍率放大，避免再次系统性低估。
        let (measured, source) = match real_last_prompt {
            Some(p) => (p as usize, "服务端实测"),
            None => (
                calib.apply(estimate_messages_tokens(&history)),
                "本地估算（未校准）",
            ),
        };
        if measured > compact_trigger {
            eprintln!(
                "[orbcat] 上下文 {measured} tok（{source}）超过 compact 水位 {compact_trigger}，触发自动压缩"
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
                        "[orbcat] 自动 compact 成功：压 {} 条 → {} 字，保留 {} 条",
                        o.summarized, o.summary_chars, o.kept
                    );
                    history = crate::history::build_history(data_dir, session_id);
                    // 压完上下文变小了，**真值立刻失效** —— 但拿不到新的真值
                    // （这一轮还没发），所以退回估算并保留倍率，别让旧真值
                    // 在下一轮又把水位判超、反复压缩。
                    calib.last_prompt = None;
                }
                Err(e) => {
                    // 压缩失败不该挡住对话 —— 后面还有循环内的裁剪兜底
                    eprintln!("[orbcat] 自动 compact 跳过（{e}），改用循环内裁剪兜底");
                }
            }
        }
    }
    let history_len = history.len();

    // 注意 `clone()`：`system_prompt` 在循环里还要用来算**每轮**的固定开销
    // （工具组会被模型 `load_tool_group` 动态改变，开销不是常量），所以不能把它
    // move 进 messages。一轮一次 clone 的代价可以忽略（这是纯内存拷贝）。
    let mut messages = vec![ChatMessage::system(system_prompt.clone())];
    messages.extend(history);
    messages.push(first_user);

    eprintln!(
        "[orbcat] 上下文回灌: 历史 {history_len} 条 + 本轮输入 1 条（会话 {session_id}）"
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
    // "前面为什么这么绕"。落盘时**原文全存**（2026-09-26 起不截，见 `sessions::cap_steps`）。
    // ⚠️ 这是**旧字段**（兼容用）：新数据以 `steps` 里的 `reasoning` 条目为准。
    let mut reason_total = String::new();

    // 执行中被消费掉的插话（按送达顺序，落盘要按这个顺序写）
    let mut steers: Vec<SteeredMsg> = Vec::new();

    // 本轮问答的 token 用量合计 —— 每轮 chat_stream 成功后累加。
    // 服务端不给 usage（未开 include_usage / 网关不透传）时保持全 0。
    let mut usage_total = TokenUsage::default();

    // 注：上下文预算的校准累加器 `calib` 已在上面（compact 判定处）初始化 ——
    // 它同时是"上一轮真值"的载体与"本轮新真值"的收集器，只有一份。

    // 软预算：初始 max_iterations()，每消费一批插话 +STEER_BONUS，最多到 hard_iterations()。
    // 为什么不做成"插话不占轮数"：那等于取消上限。
    let max_iters = max_iterations();
    let hard_iters = hard_iterations().max(max_iters);
    let mut budget = max_iters;
    let mut iter = 0usize;

    // 「假象终止」纠正的三种计数（各自有上限，防死循环）
    let mut length_continues = 0usize; // finish_reason=length 自动续写
    let mut pseudo_nudges = 0usize; // 伪工具调用解析回真调用
    let mut invalid_continues = 0usize; // 回合结束无可用正文 → 注入提醒续跑

    // 上下文预算：模型配了就用，没配走保守兜底。乘安全水位。
    // （`context_window` 已在自动 compact 那一段算过，这里直接复用）
    //
    // ⚠️ 与 compact 水位的关系（2026-09-30 统一口径）：`COMPACT_TRIGGER_RATIO`（0.6）
    // 低于这里的 `CONTEXT_SAFE_RATIO`（0.75）是**刻意**的 —— compact 在 60% 就把
    // 历史压掉，循环内的硬裁剪（75%）只是压缩失败时的最后一道闸，正常轮不到它。
    let context_budget = ((context_window as f64) * CONTEXT_SAFE_RATIO) as usize;
    // 裁剪时至少保留最近这么多条工具结果原文（它们是模型当前最可能依赖的）
    const KEEP_RECENT_TOOL_RESULTS: usize = 4;
    eprintln!(
        "[orbcat] 上下文预算: 窗口 {} tok（{}）× {:.0}% = {} tok{}",
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
            return Ok(interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total, calib));
        }
        iter += 1;

        // ---- 轮边界：消费「插话」----
        // 放在这里同时覆盖两个时机：上一轮 LLM 结束后、以及工具都跑完之后
        // （两者都回到循环顶部）。插话就是普通 user 消息 —— 同一个 loop、
        // 同一个权限网关，所以「越权」这条红线自动同样生效，不需要额外校验。
        for m in hub.drain().await {
            messages.push(build_user_message(cfg, data_dir, &m.text, &m.images));
            // 插进 steps 时间线：重载后过程里能看到「跑到这一步时用户补了一句」。
            // 图片一并挂上 —— 插话不铺独立气泡，这里是它唯一的展示位置。
            steps.push(AgentStep {
                kind: "steer".into(),
                name: Some(m.id.clone()),
                detail: m.text.clone(),
                images: m.images.clone(),
            });
            on_progress(Progress::Steer {
                id: m.id.clone(),
                text: m.text.clone(),
                images: m.images.clone(),
            });
            eprintln!("[orbcat] 插话已送达（第 {iter} 轮）: {}", m.text);
            steers.push(m);
            budget = (budget + STEER_BONUS).min(hard_iters);
        }

        // 每轮重建工具列表 —— 模型可能刚 load 了新组
        let tools_arg = if cfg.supports_tool_call {
            let specs = {
                let reg = mcp.lock().await;
                // ⚠️ 用 `crate::web_search_ready_for` 而**不是** `search::is_ready`：
                // 项目可以声明 `slots.search`，声明了就必须是那个后端才暴露
                // `web_search`（否则会把查询发给用户没预期的服务商）。
                let ready = crate::web_search_ready_for(data_dir);
                // 蒸馏会话：额外注入 `distill_move`（普通聊天不该看见它）
                if distill {
                    tools::tool_specs_for_distill(
                        Some(&reg),
                        ready,
                        data_dir,
                        Some(session_id),
                    )
                } else {
                    tools::tool_specs(Some(&reg), ready, data_dir, Some(session_id))
                }
            };
            Some(llm::tools_to_openai(&specs))
        } else {
            None
        };
        // 工具 schema 的文本形态，**只用于估算开销**（不参与请求）。
        // 为什么单独留一份：工具列表每轮都可能变（模型刚 load 了新组），
        // 而它是固定开销里最大的一块（56 个 MCP 工具约 14k token）。
        let tools_arg_text = tools_arg
            .as_ref()
            .map(|t| serde_json::to_string(t).unwrap_or_default())
            .unwrap_or_default();

        // ---- 上下文预算检查（每轮 LLM 调用前）----
        //
        // 为什么必须有这一步：`messages` 在循环里**只增不减**（每轮 push
        // assistant + N 条 tool 结果）。轮数上限拉到 100/250 后，不检查就会
        // 一路顶穿模型窗口 → 413 / context_length_exceeded → 整轮报废。
        //
        // ⚠️ 2026-09-30 三处修正（此前这条闸门形同虚设）：
        //   1. **算上固定开销**：system prompt 与工具 schema 此前完全没进估算，
        //      而它们本来就在每次请求里（56 个 MCP 工具约 14k token）。
        //   2. **乘校准倍率**：本地 `char/4` 估算实测低估 5~144 倍，
        //      用服务端返回的真实 prompt 反推倍率后，估算才有量级意义。
        //   3. **与 compact 同一口径**：`context_budget` 现在取
        //      「预算 − 开销」而不是整个窗口的 75%，否则历史把预算吃满时
        //      循环还认为"没超"。与开跑前的 auto-compact 水位同源。
        let est_msgs = estimate_messages_tokens(&messages);
        // 固定开销 = system prompt（已含前台段与项目记忆段）+ 工具 schema。
        // 这两样**本来就在每次请求里**，却完全没进过预算估算 —— 正是它们
        // 让本地估算与服务端真实 prompt 差了 5~144 倍。
        let est_overhead =
            llm::estimate_tokens(&system_prompt) + llm::estimate_tokens(&tools_arg_text);
        let est_msgs_scaled = calib.apply(est_msgs);
        let est = est_msgs_scaled + est_overhead;
        if est > context_budget {
            let trimmed = trim_old_tool_results(&mut messages, KEEP_RECENT_TOOL_RESULTS);
            let after = calib.apply(estimate_messages_tokens(&messages)) + est_overhead;
            eprintln!(
                "[orbcat] 上下文预算超限（第 {iter} 轮）：估算 {est}（消息 {est_msgs}×{:?} + 开销 \
                 {est_overhead}）> 预算 {context_budget}，裁掉 {trimmed} 条旧工具结果 → {after} tok",
                calib.scale
            );
            if after > context_budget {
                // 裁剪救不回来 → 降级返回部分结果（不硬发必然失败的请求）
                eprintln!(
                    "[orbcat] 裁剪后仍超预算（{after} > {context_budget}），降级返回部分结果"
                );
                let mut r = interrupted_run(
                    steps,
                    steers,
                    reason_total,
                    String::new(),
                    iter,
                    usage_total,
                    calib,
                );
                r.stop_reason = Some(format!(
                    "上下文已接近模型上限（估算 {after} / 预算 {context_budget} tok），\
                     继续执行会被模型拒绝。以下是已产出的部分结果；\
                     可清空部分历史或换更大窗口的模型后重试"
                ));
                return Ok(r);
            }
        }

        // 校准要用的「发出这次请求之前，本地估了多少」快照。
        // ⚠️ 必须在 push 了本轮的 assistant / tool 结果**之前**取 ——
        //    服务端返回的 prompt 对应的正是"发出去时的那份 messages"。
        let est_msgs_before_round = est_msgs;

        on_progress(Progress::Thinking { iteration: iter });

        // 流式：正文边收边推给前端（方案 C）
        // 用 Arc<AtomicBool> 而非裸引用 —— 回调是 'static 约束的 trait object，
        // 借局部变量的闭包活不够长
        let streamed_this_round = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 本轮的两个累积器。为什么单独攒一份：`out` 只在调用**成功**返回时才有，
        // 被中断那轮的正文与思考就丢了；而中断恰恰是用户最想回看的时候。
        let round_reason = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let round_text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

        // 流式落盘的三件套（2026-09-30）：
        //   - `steps_so_far`：回调里不能碰 `steps`（它在本轮后面还会被可变借用），
        //     所以每轮开头 take 一份只读快照；
        //   - `ckpt_cb`：checkpoint 回调本身（`Option<Arc<dyn Fn>>`，clone 即共享）；
        //   - `last_ckpt_ms`：节流时钟，原子量让回调无需加锁。
        let steps_so_far = steps.clone();
        let ckpt_cb = on_checkpoint.clone();
        let last_ckpt_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        let delta_cb = {
            let cb = on_progress.clone();
            let flag = streamed_this_round.clone();
            let acc_r = round_reason.clone();
            let acc_t = round_text.clone();
            let steps_for_ckpt = steps_so_far;
            let cb_ckpt = ckpt_cb;
            let ckpt_clock = last_ckpt_ms;
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

                // 流式落盘（节流）：正文边收边存，进程被杀不至于丢掉整段回答。
                // 用 `peek` 而不是 `take` —— 累加器后面还要用（作半截正文返回）。
                checkpoint_from_stream(
                    &cb_ckpt,
                    &ckpt_clock,
                    &steps_for_ckpt,
                    &peek_text(&acc_t),
                    &peek_text(&acc_r),
                );

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

        let mut out = match llm::chat_stream(
            cfg,
            messages.clone(),
            tools_arg,
            &delta_cb,
            status_cb.as_ref(),
            discard_cb.as_ref(),
            &cancel,
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
                    return Ok(interrupted_run(steps, steers, reason_total, half, iter, usage_total, calib));
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
                    calib,
                );
                run.stop_reason = Some(e.clone());
                run.steps.push(AgentStep {
                    kind: "error".into(),
                    name: None,
                    detail: e.clone(),
                    images: Vec::new(),
                });
                return Err(RunAbort { message: e, run });
            }
        };
        drain_status_steps(&status_log, &mut steps);
        absorb_reason(&mut reason_total, iter, &round_reason, &mut steps);
        // 轮内增量落盘：本轮思考/工具已进 steps，先写一份到磁盘
        // （进程这会儿被关掉也不至于整轮蒸发）
        checkpoint(
            &on_checkpoint,
            &steps,
            &peek_text(&round_text),
            &reason_total,
        );
        // 本轮的 token 用量并入合计（无论这轮是出答案还是调工具）
        if let Some(u) = out.usage {
            usage_total.add(&u);
            // ⚠️ 与上面的求和**不同口径**（2026-09-30）：
            //   `usage_total.prompt` 是整个 run 各轮求和（几百万很正常），
            //   而 `u.prompt` 是**这一轮**实际发出去的上下文大小 ——
            //   只有它代表"现在上下文有多大"，是预算与 compact 水位的真锚点。
            //   所以校准吃的是这一个数，不是求和。
            calib.observe(u.prompt, est_overhead, est_msgs_before_round);

            // 每轮 cache 命中率（2026-09-30）：prompt 的大头走命中价，
            // **全价部分 = prompt − cache_hit**。命中率掉下来时成本成倍上涨
            // 而总量看不出变化，所以必须**逐轮留痕**才追得到回归点。
            // 定位手段：这条日志配合"改了前缀稳定性 / 换了模型 / 动了技能与
            // MCP 组"的时间点，能直接指出是哪次改动打掉了缓存。
            if u.prompt > 0 {
                let hit = u.cache_hit.min(u.prompt);
                let rate = (hit as f64) / (u.prompt as f64) * 100.0;
                eprintln!(
                    "[orbcat] 第 {iter} 轮 cache: {rate:.1}% 命中（命中 {hit} / 输入 {}，\
                     全价 {} tok）",
                    u.prompt,
                    u.prompt - hit
                );
            }
        }
        let mut calls: Vec<llm::ToolCall> = out.tool_calls().to_vec();

        // --- 没有工具调用：先分辨"真的答完了"还是"假象"（2026-09-25 用户报提前停止）---
        //
        // 两种假象必须先排掉，否则都会被当成最终答案收工：
        // ① 输出被 max_tokens 掐断（finish_reason=length）——半截正文不是答案；
        // ② 模型把工具调用**写成正文**（模仿历史回灌的 «steps…调用 X…» 格式，
        //    mimo 实测会这样）—— 没有真 tool_calls，但意图是"继续干活"。
        if calls.is_empty() {
            let text = out.text();

            // ② 伪工具调用：正文里有 «steps /「调用 x / 参数: {…}」→ 解析回真调用
            if smells_like_pseudo(&text) {
                if let Some(pseudo) = extract_pseudo_calls(&text) {
                    if pseudo_nudges < MAX_PSEUDO_NUDGES {
                        pseudo_nudges += 1;
                        eprintln!(
                            "[orbcat] 检测到 {} 个写进正文的伪工具调用（第 {} 次），解析回真 tool_calls 继续执行",
                            pseudo.len(),
                            pseudo_nudges
                        );
                        on_progress(Progress::Status {
                            text: "模型把工具调用写成了文字，已自动纠正为真实调用…".into(),
                            retry: true,
                        });
                        steps.push(AgentStep {
                            kind: "status".into(),
                            name: None,
                            detail: format!(
                                "⚠️ 模型把 {} 个工具调用写成了正文（未真正执行），已解析回真实调用继续",
                                pseudo.len()
                            ),
                            images: Vec::new(),
                        });
                        // 正文那半截是"伪日志"，不作为最终答案。
                        // assistant 这条**不在这 push** —— 下面"有工具调用"分支会统一
                        // push out.message，这里只把 tool_calls 修正进它，避免推两条。
                        out.message.tool_calls = Some(pseudo.clone());
                        calls = pseudo;
                        // 走下面的工具执行分支（与真 tool_calls 同一条路）
                    }
                }
            }

            // ① 截断续写：还有下半截要生成
            if calls.is_empty() && out.finish_reason.as_deref() == Some("length") {
                if length_continues < MAX_LENGTH_CONTINUES {
                    length_continues += 1;
                    eprintln!(
                        "[orbcat] 输出被 max_tokens 截断（第 {length_continues}/{MAX_LENGTH_CONTINUES} 次），自动续写"
                    );
                    messages.push(ChatMessage::assistant(text));
                    messages.push(ChatMessage::user(
                        "（上一条输出被长度上限截断了。请**从中断处直接继续**，不要重复已输出的内容；\
                         若刚才正要调用工具，请直接调用工具而不是描述它。）",
                    ));
                    on_progress(Progress::Status {
                        text: "输出被长度上限截断，自动续写中…".into(),
                        retry: true,
                    });
                    continue;
                }
                // 续写次数用尽：把半截当答案，但明说被截断
                let mut r = interrupted_run(
                    steps,
                    steers,
                    reason_total,
                    text,
                    iter,
                    usage_total,
                    calib,
                );
                r.stop_reason = Some(
                    "输出连续被长度上限截断（已自动续写多次仍被截）。\
                     可在模型配置里调大 maxOutputTokens 后继续。"
                        .into(),
                );
                return Ok(r);
            }

            // ③ 空应答 / 纯确认收尾（2026-10-01 新增，抄 MiMo/WB 的 autoContinueInvalidOutput）——
            //    模型没调工具、正文是空的、或者只回一句「已完成 / Done」。
            //    这不是答案：用户问的是问题，不是"你做完了没"。
            //    提示词里已经写了「不要只回一句已完成」，但**软约束挡不住**，
            //    所以这里做硬判定：注入一条合成 user 消息让它接着答。
            if calls.is_empty() && (is_acknowledgement_only(&text) || is_deferral_only(&text)) {
                // 「指向别处」（文中说"答案见上一条"）与「空话应答」要分开处理：
                // 前者模型其实**有**答案，只是以为已经交付过了。
                let deferral = is_deferral_only(&text);

                // ★ 指路式收尾：本轮中途其实写过实质正文（`messages` 里还留着）
                //   → 直接提升为最终答复。不再烧一轮，也就不可能再写一次「见上一条回复」。
                if deferral {
                    if let Some(promoted) = last_substantive_midturn_text(&messages) {
                        eprintln!(
                            "[orbcat] 收尾只写了「见上一条回复」之类，已把本轮中途写的正文\
                             （{} 字符）提升为最终答复",
                            promoted.chars().count()
                        );
                        // ⚠️ 去重（2026-10-02）：这段正文在它当年那轮已经作为 `text`
                        //    条目落过盘了（见上面工具调用分支的 push）。删掉它，
                        //    否则同一段话会既在「过程」里、又当答案显示一遍。
                        if let Some(pos) = steps
                            .iter()
                            .rposition(|s| s.kind == "text" && s.detail.trim() == promoted.trim())
                        {
                            steps.remove(pos);
                        }
                        steps.push(AgentStep {
                            kind: "status".into(),
                            name: None,
                            detail: "⚠️ 模型收尾只写了「见上一条回复」，\
                                     已把它本轮中途写好的结论提升为最终答复"
                                .into(),
                            images: Vec::new(),
                        });
                        on_progress(Progress::Answering);
                        steps.push(AgentStep {
                            kind: "assistant".into(),
                            name: None,
                            detail: promoted.clone(),
                            images: Vec::new(),
                        });
                        return Ok(AgentRun {
                            answer: promoted,
                            steps,
                            iterations: iter,
                            reasoning: (!reason_total.is_empty()).then_some(reason_total),
                            interrupted: false,
                            steers,
                            pending_steers: Vec::new(),
                            stop_reason: None,
                            usage: usage_total,
                            last_prompt_tokens: calib.last_prompt,
                            token_scale: calib.scale,
                        });
                    }
                }

                if invalid_continues < MAX_INVALID_OUTPUT_CONTINUES {
                    invalid_continues += 1;
                    eprintln!(
                        "[orbcat] 回合结束但没有可用正文（{}，第 {invalid_continues}/{MAX_INVALID_OUTPUT_CONTINUES} 次），\
                         注入提醒继续作答",
                        if deferral { "把答案指向了别处" } else { "空内容或纯确认语" }
                    );
                    // 空正文时不要把空的 assistant 消息塞进历史（部分厂商 API 会报错）
                    if !text.trim().is_empty() {
                        messages.push(ChatMessage::assistant(text.clone()));
                    }
                    messages.push(ChatMessage::user(if deferral {
                        "（你上一轮**没有把答案写出来**，只是把答案指向了别处 —— \
                         比如「结论已给出」「完整数据见上一条回复」。\n\
                         用户看不到你的思考过程（reasoning 不会显示），也**没有**你指的那条「上一条回复」——\
                         那里什么都没有。你在思考里算出来的结论，用户一个字都没看到。）\n\
                         现在**把结论完整写在这一条消息里**：\n\
                         1. 第一句直接给答案（用户问的那件事）；\n\
                         2. 关键数据用 markdown 表格或列表**列出来**，不要再用「见上表」指路；\n\
                         3. 依据（文件:行号 / 命令输出）。\n\
                         禁止再写「见上文」「如上」「已给出」这类指路的话。"
                    } else {
                        "（你上一轮没有给出可用回答 —— 只有思考过程，或只是一句确认/空内容。）\n\
                         现在**直接回答用户的问题**：给结论、给依据、给下一步。\n\
                         不要只回「已完成 / 好的 / 收到」这类应答；如果确实还有活要干，就直接调用工具。"
                    }));
                    on_progress(Progress::Status {
                        text: if deferral {
                            "模型把答案指向了别处，已要求它把结论写出来…".into()
                        } else {
                            "模型只给了应答式收尾，已要求它正面作答…".into()
                        },
                        retry: true,
                    });
                    steps.push(AgentStep {
                        kind: "status".into(),
                        name: None,
                        detail: if deferral {
                            "⚠️ 回合结束但没写答案（只说了「见上一条回复」之类），已自动要求它把结论写全"
                                .into()
                        } else {
                            "⚠️ 回合结束但没有可用正文（空内容或纯确认），已自动要求正面作答".into()
                        },
                        images: Vec::new(),
                    });
                    continue;
                }
                // 续跑次数用尽：明说没答上来，别把空应答伪装成结论
                eprintln!("[orbcat] 连续 {invalid_continues} 次无可用正文，降级返回并说明");
                let mut r = interrupted_run(
                    steps,
                    steers,
                    reason_total,
                    text,
                    iter,
                    usage_total,
                    calib,
                );
                r.stop_reason = Some(
                    "模型连续多轮没有给出可用回答（只输出思考或确认语）。\
                     可以换个说法再问一次，或换一个模型。"
                        .into(),
                );
                return Ok(r);
            }

            // 真的答完了：这就是最终答案（流式内容已经在界面上）
            if calls.is_empty() {
                on_progress(Progress::Answering);
                steps.push(AgentStep {
                    kind: "assistant".into(),
                    name: None,
                    detail: text.clone(),
                    images: Vec::new(),
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
                    last_prompt_tokens: calib.last_prompt,
                    token_scale: calib.scale,
                });
            }
        }

        // --- 有工具调用：本轮若已流过正文，告诉前端把那半截话淡化掉 ---
        // （方案 C 的代价：模型偶尔会先吐半句再决定调工具）
        //
        // ⚠️ 只作废**正文**。reasoning 保留 —— 它是"为什么调这个工具"的推演，
        //    正是用户想看的；作废它等于把最有价值的部分删掉。
        if streamed_this_round.load(std::sync::atomic::Ordering::Relaxed) {
            // ★ 先把这段中途正文**落进时间线**（2026-10-02 修）。
            //   以前只发 DiscardStream 给前端"淡化"，后端不落盘 → 一轮跑完
            //   前端按磁盘重建消息，这段正文就没了（用户："我的中途正文呢"）。
            //   位置必须在下面 `for call in calls` **之前**，时间线里才会
            //   "正文 → 工具行"地排；每轮的 `round_text` 是新的，所以多轮
            //   "正文→调工具"会各自留一条。
            //
            // ⚠️ 用 `peek_text` 而不是 take：末尾 1795 的 checkpoint 与
            //    「被停止」时 1811 的半截正文都还要读它，语义一个字都不能变。
            //
            // ⚠️ 另一个 DiscardStream 源（`discard_cb`，断流重试）**不落**：
            //    那是流到一半断掉、正在重试的半截废稿，落了会出现"半句 + 重试完整句"。
            //    错误路径（interrupted_run）同理，正文由它自己带走，这里不补。
            if let Some(detail) = midturn_detail(&peek_text(&round_text)) {
                steps.push(AgentStep {
                    kind: "text".into(),
                    name: None,
                    detail,
                    images: Vec::new(),
                });
            }
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
                images: Vec::new(),
            });

            let output = {
                // 长命令心跳：每 2s 把「已运行 N 秒 + 输出尾巴」推给前端（工具名绑进闭包）
                let tick: crate::shell::TickFn = {
                    let cb = on_progress.clone();
                    let name = call.function.name.clone();
                    std::sync::Arc::new(move |elapsed_secs: u64, tail: String| {
                        cb(Progress::ToolTick {
                            name: name.clone(),
                            elapsed_secs,
                            tail: tail.clone(),
                        });
                    })
                };
                // 取消探针：让 run_command 在**执行期间**也能被「停止」掐断
                // （2026-09-26 修：以前 cancel 只在轮边界读，命令跑 600s 就得等 600s）。
                // 现在 `cancel` 是**每 run 一个** `Arc<AtomicBool>`（多会话并行 run），
                // 闭包直接捕获它的 clone —— 不再依赖 `'static` 全局，停 A 不会误停 B。
                let cancel_flag = cancel.clone();
                let cancel_probe: crate::shell::CancelFn = std::sync::Arc::new(move || {
                    cancel_flag.load(std::sync::atomic::Ordering::Relaxed)
                });
                let ctx = tools::ToolCtx {
                    gate,
                    data_dir,
                    mcp,
                    session_id,
                    perm,
                    tick: Some(tick),
                    cancel: Some(cancel_probe),
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
                images: Vec::new(),
            });

            // 每个工具跑完就落一次盘：长命令是"用户等不及直接关进程"的高发区，
            // 这里落完，磁盘上至少能看到"跑到哪个工具、参数是什么"。
            checkpoint(
                &on_checkpoint,
                &steps,
                &peek_text(&round_text),
                &reason_total,
            );

            // 回灌时截断，避免撑爆上下文
            let for_model: String = output.text.chars().take(TOOL_RESULT_LIMIT).collect();
            messages.push(ChatMessage::tool_result(call.id.clone(), for_model));

            if let Some(img) = output.image {
                pending_images.push(img);
            }

            // 工具被「停止」掐断 → 剩余工具不再执行，直接返回部分结果。
            // 为什么同一批 tool_calls 里剩下的也要跳过：命令已经证明是长任务，
            // 继续跑下一个既违背用户"停下"的意图，也是纯浪费。
            if cancel.load(Ordering::Relaxed) {
                let half = take_text(&round_text);
                return Ok(interrupted_run(
                    steps,
                    steers,
                    reason_total,
                    half,
                    iter,
                    usage_total,
                    calib,
                ));
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
            return Ok(interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total, calib));
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
        "[orbcat] 已连续调用工具 {iter} 轮仍未给出结论，降级返回部分结果（预算 {budget}，硬顶 {hard_iters}）"
    );
    let mut r = interrupted_run(steps, steers, reason_total, String::new(), iter, usage_total, calib);
    // 借用 stop_reason 说明是"轮数用尽"而非"用户停止"，前端据此显示不同提示
    r.stop_reason = Some(format!(
        "已达轮数上限（{iter} 轮），以下是已产出的部分结果。可能是任务过大或模型陷入循环。\
         你可以：① 直接说「继续」接着跑；② 先压缩上下文再说「继续」；③ 停止。\
         也可调大 ORBCAT_MAX_ITERATIONS / ORBCAT_HARD_ITERATIONS"
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
    // 本轮拿到的校准值（真实 prompt / 倍率）。`Copy`，各条返回路径各拿一份。
    calib: Calibration,
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
        last_prompt_tokens: calib.last_prompt,
        token_scale: calib.scale,
    }
}

/// 取走某一轮累积的正文（空则返回空串）
fn take_text(acc: &std::sync::Mutex<String>) -> String {
    acc.lock().map(|g| g.clone()).unwrap_or_default()
}

/// 从正文里解析**写成文字的伪工具调用**，还原成真 `tool_calls`。
///
/// ## 为什么需要（2026-09-25 用户报「mimo 老是提前停止」后定案）
///
/// 历史回灌把工具轨迹以 `«steps … 调用 X / 参数: {…} …»` 文本形式拼进
/// assistant 消息（见 `history::fold_steps_to_line`）。部分模型（mimo 实测）
/// 会**模仿这个格式**：它把下一步要调的工具写成正文「调用 run_command
/// 参数: {...}」，然后就此收工 —— 没有真 `tool_calls`，agent loop 判定
/// "没有调用 = 答完了"，任务停在半路。
///
/// 解析规则（对齐 fold_steps_to_line 的输出格式）：
/// - `调用 <name>` 行（行首允许缩进）→ 工具名
/// - 其后的 `参数: <json>` 行 → 参数（必须是合法 JSON 对象才收）
/// - 只认**成对**出现的；单有名字没参数的忽略（可能是正文提到"调用"一词）
///
/// 返回 `None` = 正文里没有伪调用（正常回答）。解析出 0 个有效对也是 `None`。
pub fn extract_pseudo_calls(text: &str) -> Option<Vec<llm::ToolCall>> {
    let mut out: Vec<llm::ToolCall> = Vec::new();
    let mut pending_name: Option<String> = None;

    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("调用 ") {
            let name = rest.trim();
            // 工具名形态校验：字母/数字/下划线/连字符，别把正文句子当名字
            if !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                pending_name = Some(name.to_string());
            } else {
                pending_name = None;
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("参数:") {
            if let Some(name) = pending_name.take() {
                let raw = rest.trim();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
                    if v.is_object() {
                        out.push(llm::ToolCall {
                            id: format!("pseudo_{}", out.len()),
                            kind: "function".into(),
                            function: llm::FunctionCall {
                                name,
                                arguments: raw.to_string(),
                            },
                        });
                    }
                }
            }
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 正文里有没有伪调用的"气味"（«steps 轨迹块 / 调用+参数 行）。
/// 先便宜地嗅一下，命中才跑 [`extract_pseudo_calls`]。
fn smells_like_pseudo(text: &str) -> bool {
    text.contains("«steps") || (text.contains("\n调用 ") && text.contains("参数:"))
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
                    eprintln!("[orbcat] 图片内联失败: {e}");
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
///   MCP 机制（纯规则文字，不含组状态、不含工具清单）
///   对话历史机制（纯规则文字，不含任何动态值）
/// ── 动态尾部（按变化频率递增）──────────────
///   外部工具索引     拉到新工具清单才变 → 动态区里最不易变，排最前
///   技能清单        加技能才变
///   MCP 已加载组     加载/卸载组才变
///   工作目录        进程内恒定，但仍是"求值"得来，放最后
///   自动加载技能     用户本轮消息命中关键词才出现 → 每轮都可能变
///   （前台上下文由 agent::run 追加，每轮都变 → 绝对最尾）
/// ```
///
/// ⚠️ **外部工具索引（能力索引）为什么必须排在动态区最前**（2026-10 加）：
/// 它列的是"当前 MCP 有哪些工具、各能干什么"，改一次 server 配置或拉一次
/// 快照就会变 —— 所以它**不能**进稳定前缀：那样每次配置变动都会让它后面的
/// 全部内容（对话历史规则、技能清单…）按未命中价重付。
/// 但在动态区里它又是**最不易变**的（时间每分钟变、自动加载技能每轮变），
/// 于是它排动态区第一位、其余动态内容跟在它后面：
/// 索引变了只失效它自己那一小段，别的一律继续吃缓存。
///
/// 索引内容**只有工具名 + 一句话用途**，绝不含 schema —— 56 个工具的 schema
/// 约 14.4k tokens，常驻它等于把懒加载整个取消掉。实现见
/// `mcp::ToolRegistry::capability_index`。
///
/// 组装 prompt（不含本轮用户消息）—— 等价于 `build_system_prompt_full(dir, "", None)`。
///
/// ⚠️ **仅供测试**（2026-09-30 标注，2026-10 更新）：生产路径走
/// [`build_system_prompt_full`]，因为它要把本轮用户消息传进去做**技能自动命中**
/// （命中的技能正文会拼到动态区最尾），并且要带上 PTC 的程序内工具声明。
/// 这个不带消息的版本只给"前缀稳定性"那批单测用 —— 它们只关心
/// 稳定区/动态区的切分，不关心技能命中与 PTC。
#[cfg(test)]
pub fn build_system_prompt(data_dir: &Path) -> String {
    // 测试入口不带会话 → 走全局默认模式（`effective_mode` 的回退分支）
    build_system_prompt_full(data_dir, "", None, None)
}

/// 组装 system prompt；`user_message` 的关键词会**自动加载**命中的技能正文，
/// `ptc_sdk` 是 PTC 模式下的**程序内工具声明**（`None` = 非 PTC / 不需要）。
///
/// 技能机制见 `skills::autoload`：命中的技能全文作为**最尾**的动态段追加，
/// 模型直接照做，无需再调 `load_skill`。
///
/// ## 为什么 PTC 的 SDK 要从外面传进来，而不是在这里现算
///
/// PTC 的程序内工具声明必须与**模型看到的工具集逐项同源**
/// （`tools::ptc_sdk_tools` 直接复用 `all_tool_specs`）。而本函数是**同步**的，
/// 拿不到 MCP 注册表（那要 `.await` 抢异步锁 —— 见 `mcp.rs` 里能力索引
/// 为什么走落盘缓存的那段说明）。
///
/// 所以由调用方（`run()`，它本来就是 async 且能拿到注册表）算好传进来。
/// 不走"落盘缓存"那条路：SDK 与工具集必须严格同步，多一份缓存就多一个
/// 能悄悄过期的地方，而这里根本不需要 —— 调用点就在同一个函数里。
/// 模式 → 「发图」那段系统提示（2026-10-08，同日改为**工具驱动**）。
///
/// ## 为什么改成工具（用户 2026-10-08 提出）
///
/// 原先教模型自己写 `![说明](路径)`。问题在**约束力**：那是提示词措辞，
/// 模型理论上可以不听。而 `send_image` / `send_meme` 是**工具** ——
/// 工作模式下 `send_meme` 压根不在工具表里（见 `tools::all_tool_specs`），
/// 模型想发表情包也调不到。**硬闸门比措辞可靠**，这一点用户说得对。
///
/// ## 但图仍然显示在**正文**里
///
/// 工具只负责"挑哪张 + 校验存在性"，返回一段 markdown，由模型贴进正文，
/// 前端 `mdToHtml` 渲染成图。为什么不直接渲染工具结果：工具结果折叠在
/// 「过程」里（`details.tl-tool`），跑完还整轮收起 —— 用户**根本看不到图**。
/// 所以"工具决定发什么、正文决定图在哪"是刻意的分工，不是重复。
///
/// 两版措辞的差别就是 `modes::ImageReplies` 的全部语义。
fn image_replies_prompt(mode: &crate::modes::Mode) -> String {
    let how = "\
**怎么发：用工具，不要自己手写路径。**

- `send_image(path[, caption])` —— 贴一张现成的图（截图、图表、网图）
- `send_meme([query])` —— 从你的表情包库挑一张

两个工具都会返回一段 markdown。**你必须把它原样复制到回复正文里** ——
工具返回值本身折叠在「过程」里，用户看不到；只有贴进正文才会显示成图。

手写 `![说明](路径)` 也渲染得出来，但**不经工具就没有存在性校验**，
写错只会给用户一个「图（读不出来）」。所以能走工具就走工具。";

    if mode.freestyle_images() {
        format!(
            "# 发图\n\n\
             现在是**闲聊场景**，可以主动发图：情绪到了就发一张表情包 / 梗图，\
             不用等用户开口要。一条回复最多一张，别刷屏 —— 发多了很吵。\n\n{how}"
        )
    } else {
        format!(
            "# 发图\n\n\
             你可以把**工作产物**贴进回复 —— 比如 `capture_screen` 截完屏、\
             或生成了一张图表之后，让用户直接看到，比描述一遍强。\n\
             ⚠️ 这个模式**没有 `send_meme`**（发表情包的工具不存在），\
             也**不要**发梗图、表情包之类与当前任务无关的图。\n\n{how}"
        )
    }
}

pub fn build_system_prompt_full(
    data_dir: &Path,
    user_message: &str,
    ptc_sdk: Option<&str>,
    session_id: Option<&str>,
) -> String {
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

    // ── 项目级规则片段（拼装项目 `promptParts.rules`）────────────────────
    //
    // 位置：**紧跟在全局 RULES 之后**（spec S2.3 的要求）。放这里而不是
    // 追加到最尾，是因为它在语义上是"这个项目的红线"，与全局红线同族；
    // 但它是**追加**而非替换 —— 全局红线永远先出现、永远有效。
    if let Some(r) = crate::project::project_rules_md(data_dir) {
        stable.push(format!(
            "# 项目规则（当前项目追加，不得与上面的行为红线冲突）\n\n{r}"
        ));
    }

    // ── 拼装项目：项目记忆片段（`promptParts.memory`，默认 MEMORY.md）────
    //
    // ⚠️ 与下面的「激活项目（active_project_id）」是**两条不同的线**：
    //   - 这条：拼装包声明的记忆文件（project.rs）
    //   - 下面那条：项目记忆的会话绑定（pmem.rs）
    // 两者可以同时存在。各自读各自的文件，互不覆盖。
    if let Some(pm) = crate::project::project_memory_part(data_dir) {
        if let Some(a) = crate::project::active(data_dir) {
            stable.push(format!(
                "# 拼装项目记忆（{}）\n\n{pm}\n\n\
                 当前处于拼装项目「{}」。这套配置由 `projects/{}/project.json` 定义\
                 （技能范围 / 权限预设 / 脚本工具）。",
                a.bundle.name, a.id, a.id
            ));
        }
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
         `projects/<名>/MEMORY.md` 是项目记忆（顶栏切换项目后整段注入）。\
         另有轮末自动蒸馏在后台补充：项目记忆由它直写，全局候选由它代提 ——\
         但你在对话中**当场**判断值得记的，仍然要自己调 `remember`，别全推给后台。\n\n\
         调 `remember` 的时机（**当场就调，不要攒**）：\n\
         - 用户说「以后 / 记住 / 下次 / 我的习惯是 / 以后都…」—— 立即 remember\n\
         - 用户给出项目路径、目录结构、命名约定等稳定事实 —— 当场 remember\n\
         - 用户确认了一项决策/约定（「就用 X」「以后按 Y 来」）—— 当场 remember\n\
         - 你踩了坑并找到解法 —— 修完当场 remember（错因 + 规避方式）\n\n\
         落点规则：**主对话**进全局候选（有审批关，宁多勿漏 —— 提错的\
         用户会驳掉，漏记的永远丢了）；**已切换项目**直接写项目 MEMORY.md\
         （无审批，必须准：只写确认过的事实，不写猜测）。\n\n\
         不该记：临时状态、一次路径的临时拼接、编造的猜测、尚未确认的推断。\n\n\
         记不清某事时：先查长期记忆与项目记忆，再调 `recall_turns`，仍没有再问用户 —— 不要瞎编。"
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
         若工具不存在，说明未配置搜索 —— 不要编造「已搜索」，直接说明无法联网检索。\n\n\
         **语言（2026-10-08 加，别删）**：检索到的资料经常是英文 —— 那只说明**资料**是\n\
         英文，你的回答必须仍是**用户正在用的语言**（默认中文）。术语/报错原文可以\n\
         保留英文，但要说中文；**绝不要**因为查到的是英文内容就把整段回答切成英文。\n\
         查询词也优先中文，除非查英文资料确实更准。"
            .to_string(),
    );

    // ── 场景能力：正文发图 + 场景上下文（2026-10-08）─────────────────────
    //
    // 这两段是**模式**对 prompt 的唯一影响面。边界见 `modes.rs` 顶部：
    // 模式可以决定"注入什么内容"，但不可以决定"拦不拦"（权限/轮数仍归各自闸门）。
    //
    // 为什么放稳定区：切模式是低频操作（一次切换换一次缓存），而这两段
    // 属于"这个模式下模型能做什么"，跟人格/红线一样跨轮稳定 —— 每轮都变
    // 的东西（前台窗口、数据源摘要）才必须留在动态区最尾。
    // 模式**跟着会话走**（2026-10-08 用户定案）：同一个进程里两个会话可以跑不同模式，
    // 所以这里必须按 `session_id` 取，不能读全局的 `settings.activeMode`。
    let mode = crate::sessions::effective_mode(data_dir, session_id);
    stable.push(image_replies_prompt(&mode));

    // 表情包库摘要 —— **只在能随便发图的模式下注入**（2026-10-08）。
    // 工作模式不注：那儿本来就不该发表情包，把库摆出来等于诱导。
    // 库为空时 `prompt_section` 返回空串，这里自然跳过，不会塞一段废话进 prompt。
    if mode.freestyle_images() {
        let memes = crate::memes::prompt_section(data_dir);
        if !memes.is_empty() {
            stable.push(memes);
        }
    }

    // 聊天式呈现：教模型**怎么分段**（前端按段落切成气泡，见 `bubble.ts`）。
    //
    // 为什么这段跟着 `chatty` 而不是只写在 `promptExtra` 里：用户自定义的聊天模式
    // 很可能忘了写分段约定，那切分就退化成"一整段一个泡"，白做。机制说明归代码，
    // 语气归 `promptExtra` —— 跟「发图怎么写」和「发图的语气」是同一种分工。
    if mode.chatty {
        stable.push(
            "# 闲聊的说话方式\n\n\
             你现在是**聊天式输出**：界面会按你的**分段**显示成一条条消息。\n\n\
             - 一次说一个念头，**一段就是一条消息**\n\
             - 短句、口语；别写小标题、别分点罗列（那是工作模式的样子）\n\
             - 想说三句就分三段；内容少就只发一条，**别为了凑条数硬拆**\n\
             - 最多 5 条 —— 超了界面会自己合并，但最好你自己控制住\n\n\
             ⚠️ 代码块、列表、表格不会被拆开，会整块占一条消息。"
                .to_string(),
        );
    }

    // 场景上下文：模式自己写的一两句话（`promptExtra`）。
    // 标题里带模式名，跟上面「项目规则」「拼装项目记忆」一样**自报家门** ——
    // 否则 prompt 里好几段注入，读的人分不清谁塞的。
    if !mode.prompt_extra.trim().is_empty() {
        stable.push(format!(
            "# 当前场景（{}）\n\n{}",
            mode.name,
            mode.prompt_extra.trim()
        ));
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

    // ── 稳定区：工具调用格式红线（2026-09-25 mimo 提前停止的根因之一）────
    // 历史回灌把工具轨迹拼成 «steps … 调用 X / 参数: …» 文本（history.rs），
    // 部分模型会把它当成"输出格式"模仿 —— 调用写成文字、没有 tool_calls，
    // agent 只能判定"答完了"。必须显式禁止。
    stable.push(
        "# 工具调用的格式（违反 = 调用不会被执行）\n\n\
         调用工具**只能**通过 tool_calls（function calling）发出。\n\
         **绝对不要**在正文里写 `«steps…»`、`调用 xxx`、`参数: {...}`、`·think` \
         这类文字冒充工具调用 —— 那是历史记录的折叠格式，只供你回看，\
         写进正文不会执行任何工具，只会让任务停在半路。\n\
         正文里只写给用户看的话；要干活，就发 tool_calls。"
            .to_string(),
    );

    // ── 稳定区：收尾纪律（2026-10-01，抄 WB 的正面钉死 + 堵出口）────────
    //
    // 用户的原问题：「怎么保证在最后的时候能稳定回答用户的问题，
    // 而不是最后说已回答」。
    //
    // 答案分两层，**这一层只是第二道防线**：
    //   ① 运行时硬判定 —— 回合结束若没有可用正文（空内容 / 纯确认语），
    //      代码会注入提醒让你接着答（见 `MAX_INVALID_OUTPUT_CONTINUES`）。
    //      那是硬约束，你绕不过去。
    //   ② 就是下面这段提示词 —— 软约束，目的是**少触发**①，
    //      因为每触发一次就多烧一轮 token。
    //
    // 抄自 WB/MiMo 的 system prompt 原文（`tmp/wb_sysprompt_extract.txt`
    // 第 75/78/131 行，2026-10-01 从 asar 提取实证）：
    //   - "End-of-turn summary: What changed and what's next. Nothing else."
    //   - "Do not end a turn with only an acknowledgement or a future-tense promise"
    //   - "Assume users can't see most tool calls or thinking — only your text output."
    stable.push(
        "# 收尾纪律（回合最后一条消息必须能独立回答用户）\n\n\
         **你的思考过程用户完全看不到** —— reasoning 不会显示在对话里，工具调用也被折叠。\
         用户只看到你写的**正文**。\n\
         所以：只在思考里算出来的结论，不写进正文 = 用户一个字都看不到。\n\
         干完活**必须在正文里说清楚结果**，不能只发一句「已完成」。\n\n\
         ⚠️ **你在一轮中途写下的正文，不算最终答复** —— 只要你接着调了工具，\
         系统就会把那段正文归档进折叠的「过程」里，并标注\
         「模型中途决定调用工具，以下内容不是最终答复」。\n\
         所以**不要把结论只留在中途的正文里**：最后一条消息必须把结论重写一遍。\n\
         用户不会去折叠的过程里翻你之前写过什么。\n\n\
         ## 收尾模板\n\
         最后一条正文按这个结构写（简洁，不要套话）：\n\
         1. **结论** —— 直接回答用户问的那件事，第一句就给答案；\n\
         2. **依据** —— 关键证据（文件:行号、命令输出、数据），只列支撑结论的那几条；\n\
         3. **下一步** —— 需要用户拍板的选项，或还剩什么没做。\n\n\
         ## 禁止的收尾（违反 = 会被代码打回重答，白烧一轮）\n\
         - ❌ 只回「已完成 / 已回答 / 好的 / 收到 / Done」这类确认语；\n\
         - ❌ **把答案指向别处**：「结论已给出」「完整数据见上一条回复」「如上」「详见上文」——\
         用户找不到你说的那条「上一条回复」，那里什么都没有；答案必须写在这条消息里；\n\
         - ❌ 只描述「我打算怎么做」（将来时承诺），却没真做；\n\
         - ❌ 把过程当结论（用户问的是结果，不是你的步骤流水账）；\n\
         - ❌ 正文留空，把内容全塞在工具调用里。\n\n\
         如果确实还没干完，就**继续调工具**；如果卡住了，\n\
         就直说卡在哪、需要用户提供什么 —— 但不要用空话收尾。"
            .to_string(),
    );

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
           ⚠️ `grep_files` 的用法要点（踩过坑，2026-10-01）：\n\
           - `path` **可以传目录，也可以直接传单个文件** —— 搜一个文件就传它本身，别传父目录；\n\
           - 只做**字面量子串**匹配：**不要写正则**（`a|b`、`.*`、`\\d`、`^x` 都会直接报错）；\
           多个词请**分开搜几次**；\n\
           - 默认**大小写不敏感**，一般不用管大小写；\n\
           - 返回里的「命中 N 行」**只数真正的命中行**（不含 context 行）；\n\
           - ⚠️ 若返回里出现「⚠️ 注意：…未读」，说明**有文件被跳过了**（超 2MB / 非文本 / 权限）——\n\
           这时「无匹配」**不等于真的没有**，答案可能就在被跳过的文件里。\n\
           真需要正则/搜二进制时，才用 `run_command` 跑 `rg`（属于「确实没有对应内置工具」的场景）。\n\
         - **改文件** 优先 `edit_file`（精确字符串局部替换），不要整文件覆盖；\
         写新文件/全文重写才用 `write_file`（写前会备份）；追加用 `append_file`。\n\
         - **复制 / 移动 / 建目录** → `copy_file` / `move_file` / `mkdir`，不要 `Copy-Item`/`Move-Item`/`New-Item`。\n\
         - **git 只读**（status/diff/log/show/branch）→ 用 `git` 工具。\n\
         - **看时间** → 用 `current_time` 工具（或直接用下方动态信息里的「当前时间」），\
         **禁止**为了看时间跑 `Get-Date`。\n\
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

    // ── 稳定区：MCP 机制（**纯规则文字**，组状态与工具清单挪到尾部）────
    // 关键：56 个外部工具的 schema 有 ~14k tokens，全量注入会让 prompt 变得又长又慢，
    // 还会让模型挑错工具。所以只常驻内置工具，外部工具按组懒加载。
    //
    // 注意这段是**纯规则**：一行都不含"当前有哪些组 / 哪些工具"——
    // 那些是运行时数据，在动态尾部（外部工具索引 + 组状态）。
    stable.push(
        "# 外部工具（MCP）的用法\n\n\
         你还能用一批**外部工具**（操作网页、远程服务器、数据库、桌面应用等）。\n\n\
         **默认情况下这些工具已经直接给你了** —— 它们就写在你的工具列表里，\
         名字形如 `mcp__<组名>__<工具名>`，**直接调用即可，不需要先加载**。\n\n\
         只有在两种情况下才需要走「先查再加载」：\n\
         1. 你要用的工具**不在**当前工具列表里（用户没勾选该组 / 被停用了）。\n\
         2. 用户明确让你去看有哪些组可用。\n\
         这时：`search_tools` 或 `list_tool_groups` 定位到组，再 `load_tool_group` 加载，\
         该组工具会在**下一轮**出现，然后直接调用。\n\n\
         规则：\n\
         - 当用户的需求涉及「打开网页 / 抓取某站 / 连服务器 / 查数据库 / 操作某个桌面软件」时，\
         先在你现有的工具列表里找对应的 `mcp__*` 工具直接用。\n\
         - **不确定有没有某个能力时，先查、不要猜、也不要直接说「我做不到」**：\
         用 `search_tools` 按意图搜（关键词如「提交 issue」「截图」「数据库查询」），\
         它只返回名字 + 一句话用途、不返回参数 schema，所以很便宜，可以放心多搜几次。\
         下方「外部工具索引」段列着全部工具的名字与用途，也可以直接看那里。\n\
         - **不要在没查过的情况下断言「没有这个工具」**；查完确实没有，\
         再向用户说明缺什么（需要哪个组 / 哪个 MCP server）。\n\
         - `unload_tool_group` 只在你确认某组确实拖慢上下文、且用户不再需要时才用。"
            .to_string(),
    );

    // ── 稳定区：对话历史机制（**纯规则文字**）──────────────────────────
    // 2026-10-07 起历史是**全量回灌**（未压缩区间一条不丢）；被 compact
    // 摘要覆盖的更早部分不在逐条上下文里（只有摘要）。模型仍要知道"有原文
    // 可查"，否则会：① 对着"上次那个方案"瞎编 ② 反问用户"哪个方案？"
    // 跨会话检索（scope=all）同理：**不说它就不知道能跨会话**，会直接回
    // "本会话没聊过"。而这个用户每天都在多个会话里解决同类问题
    // （编译加速、代理配置、MCP 绑定这些坑都踩过不止一次），
    // 不说清楚就等于让他把同一个坑再踩一遍。
    // ⚠️ 下面**没有任何插值** —— 这段在稳定前缀里，一个每轮都变的值会让
    //    其后全部 token 按未命中价重付。
    stable.push(format!(
        "# 对话历史\n\n\
         本会话的历史会**全量**带入你的上下文；被压缩摘要覆盖的更早部分，\
         你只看得到摘要，原文不在其中。\n\n\
         当用户提到「上次 / 之前 / 刚才那个 / 我们之前定的」而你找不到依据时：\n\
         **不要猜，也不要反问「哪个？」** —— 调 `recall_turns` 工具去查原文。\n\n\
         `recall_turns` 用法：\n\
         - `query`：关键词（如「跳页」「构建报错」）。留空则返回最近的记录。\n\
         - `hours`：只看最近 N 小时（可选）。\n\
         - `limit`：最多返回几条，默认 10。\n\
         - `scope`：检索范围。默认 `session` = 只查当前会话；\
         `all` = **连同其他历史会话一起查**。\n\n\
         ## 什么时候必须用 `scope=all`\n\
         用户说的是「以前搞过 / 上次（不一定是这个会话）怎么解决的 / 之前配过」\
         这类**跨会话**的事，而当前会话里查不到时 —— 用 `scope=all` 再查一次。\n\
         你经常在多个会话里解同类问题（编译加速、代理配置、MCP 绑定这类坑\
         不止踩过一次），换个会话就不知道上次怎么解，等于让用户重新付一遍学费。\n\n\
         `scope=all` 的结果每条都带**来源标识**（会话标题 · 时间）。\
         那些内容**不属于当前会话**：\n\
         - 引用时要说明「这是在会话《某标题》里做的」，不要当成当前会话里说过的话；\n\
         - 也不要把它当成既定事实 —— 别的会话的背景可能不同，\
           必要时说明差异或跟用户确认。\n\n\
         查完基于结果回答，并说明「这是从更早的对话里找到的」\
         （跨会话时再补一句是**哪个**会话）。",
    ));

    // ── 动态尾部 ①：外部工具索引（**能力发现**，拉到新工具清单才变）──────
    //
    // 为什么在这里、而不是稳定前缀：这段列的是"当前 MCP 有哪些工具"，
    // 换一次 server 配置 / 拉一次快照就变 —— 属运行时数据。放稳定区会让
    // 它**后面**的全部内容（对话历史规则、技能清单…）跟着失效。
    //
    // 为什么排在动态区**最前**：在"会变的内容"里它变的最少（拉快照才变，
    // 而当前时间每分钟变、自动加载技能每轮变）。按变化频率从低到高排，
    // 它变了只失效自己那一段 + 它后面的（本来就要重算），代价最小。
    //
    // 为什么不是"照旧只给组名"：见 mcp.rs 顶部「能力发现索引」——
    // 只给组名时模型无法知道 56 个工具叫什么，只能猜组名 + load 试探。
    // 这份索引是"名字 + 一句话用途"：实测 56 工具 / 4 组时索引本体约 826 tokens，
    // 连同本段规则文字共给 system prompt 增加约 1056 tokens —— 完整 schema 是
    // ~14.4k，也就是 **7% 的价钱换来"知道有什么工具"**。
    // （数字由 `tool_index_constant_cost_stays_small` 与 `index_stays_compact`
    //   两条测试钉住，谁把它写肥了会当场炸。）
    //
    // 读的是落盘缓存：组装 prompt 不该去抢 MCP 注册表的异步锁（`build_system_prompt`
    // 是同步函数、被大量单测直接调用），缓存由 `apply_snapshot` 在拉取后刷新。
    {
        let index = crate::mcp::ToolRegistry::read_cached_index(data_dir);
        if !index.is_empty() {
            dynamic.push(format!(
                "# 外部工具索引（能力发现）\n\n\
                 下面是当前可用的**全部外部工具**：组名 → 工具名（短名）+ 一句话用途。\
                 这里**只有名字和用途，没有参数细节**，所以它很便宜 —— \
                 拿它判断「该用哪一组、有没有这个能力」，别凭印象猜。\n\n\
                 用法：\n\
                 - 要做事但不确定有没有现成工具 → 先 `search_tools`（给意图关键词，如\
                 `提交 issue`、`截图`、`数据库查询`）。它只回几十行、无副作用，可以放心多搜几次。\n\
                 - 确定用哪一组 → `load_tool_group` 加载该组，拿到参数 schema 后直接调用。\n\
                 - 想知道各组的 token 成本 → `list_tool_groups`。\n\
                 - 工具名形如 `mcp__<组名>__<工具名>`（下表的短名要配上组名前缀）。\n\n\
                 {index}"
            ));
        }
    }

    // ── 动态尾部 ①′：MCP server 的使用说明（instructions）─────────────────
    // MCP 握手时 server 可以带 `instructions` 字段（如 cordis-mcp-bridge 用它送
    // 插件技能剧本摘要）。支持它的客户端应拼进系统提示 —— 本段就是那个拼装点。
    // 读的是落盘缓存（`apply_snapshot` → `persist_instructions` 刷新），与上面的
    // 索引同款纪律：同步读、读不到退空、server 掉线时缓存会被清空。
    {
        let instructions = crate::mcp::ToolRegistry::read_cached_instructions(data_dir);
        if !instructions.is_empty() {
            dynamic.push(format!(
                "# 外部工具的使用说明（来自 MCP server）\n\n\
                 以下 MCP server 在握手时附带了使用说明（通常是如何正确使用它的工具、\
                 以及调用前要先读哪些技能全文）。**照做**：\n\n{instructions}"
            ));
        }
    }

    // ── 动态尾部 ②：技能清单（加技能才变）──────────────────────────────
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

    // ── 动态尾部 ③：MCP 已加载组状态（加载/卸载组才变）─────────────────
    // 注：组状态本身由 `tool_specs()` 每轮实时反映在工具定义里，
    // 这里只是一个文字提示，告诉模型"当前手上有什么"。
    //
    // ⚠️ **当前时间必须在动态区**（2026-09-30 加）：放稳定前缀会让**整段
    //    后续内容每分钟失效一次**（prompt cache 按前缀逐字节比对）——
    //    那是拿"每轮最贵的成本项"去换一条时间。放尾部则只失效尾部这一小段。
    //    这是有意的取舍：模型没有时间就无法给日记/文档打正确日期、无法做
    //    「上周/三天前」的推理（此前它只能靠跑一次 Get-Date 猜，多数时候直接跳过）。
    dynamic.push(format!(
        "# 动态信息\n\n\
         ## 当前时间\n{}\n\n\
         ## 当前工作目录\n{}\n\n\
         ## MCP 工具组\n\
         当前可用的组已直接反映在工具列表里（`mcp__*`），\
         全部工具的名字与用途在上方「外部工具索引」段（含每组的已加载/未加载标记）。\
         按意图找工具用 `search_tools`，看各组的 token 成本用 `list_tool_groups`。",
        // 精确到分钟：够写字、算"几小时前"，也够判断跨没跨零点
        clock_line(now_ms()),
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into())
    ));

    // ── 动态尾部 ④：本轮自动加载的技能（方案 B）─────────────────────────
    // 按用户本轮消息的关键词自动命中 → 命中技能全文直接进 prompt，模型无需
    // 再调 `load_skill`。这是**每轮都可能变**的内容 → 必须排在所有低频内容之后。
    let autoloaded = crate::skills::autoload(data_dir, user_message);
    if !autoloaded.is_empty() {
        dynamic.push(autoloaded);
    }

    // ── 动态尾部 ⑤：当前模式的**行为指令**（切模式才变）─────────────────
    //
    // ⚠️ 为什么这一段必须有（2026-10 用户指出「PTC 不该是一次调用多个工具吗」）：
    //
    // agent loop **本来就支持**一轮发多个 tool_calls（`for call in calls` 逐个执行、
    // 结果全部回灌），但 prompt 里从来没有一句话告诉模型"你可以一次发多个"。
    // 于是模型永远一个工具一轮 —— 能力在，指令缺。
    //
    // 2026-10 修正：**只让模型"一次发多个"是不够的**。对照 DSH 的 PTC 实现
    // （`dsh-agent-tool-presentation` + `dsh-tools` 的 ptc mode）后确认，
    // 真正的 PTC 是三件事：
    //   ① 工具**不再逐个进工具列表**，坍缩成 `run_code` 一个传输工具
    //      （见 `tools::tool_specs`）；
    //   ② 其余工具以**程序内 SDK 声明**提供（`ptc_sdk`，由 `run()` 算好传进来）；
    //   ③ 控制流（循环/条件/并发/错误处理）搬进程序，中间结果**不进上下文**。
    // 旧版只做了"多给工具 + 让模型自己枚举并行调用"，那是标准模式 + 更多工具，
    // 所以模式之间看不出区别。
    //
    // 放动态区（不放稳定前缀）：切模式才变，而切模式是低频操作；
    // 放稳定区会让**每次切模式**都把后面所有内容的前缀缓存打掉。
    //
    // ⚠️ 2026-10-08 修：这里原来读的是全局 `settings.activeMode`（`load_active_mode`），
    //    模式跟会话走之后那是**另一个会话的模式** —— 表现是"闲聊会话拿到了 PTC 的行为段"
    //    （或反过来）。直接用函数上方按 `session_id` 取好的那个 `mode`。
    if mode.is_ptc() {
        // PTC：给真正的程序化契约（含程序内工具声明）
        let decls = ptc_sdk.unwrap_or(
            "declare const tools: Record<string, (args: unknown) => Promise<unknown>>;",
        );
        dynamic.push(crate::ptc::behavior_section(decls));
    } else {
        // 标准 / 闲聊 / 创造 / 用户自定义：也允许批量，但不强推
        dynamic.push(
            "# 当前模式：按需（逐个为主）\n\n\
             默认一个工具一轮。但如果多个调用之间**确实没有依赖**，\
             也可以在同一条回复里一次发多个（系统会逐个执行、结果全部回灌），\
             能省轮次。有依赖时必须分轮。\n\
             需要「把工具调用写成程序、批量跑并只把结论交回来」那种工作方式时，\
             让用户**新建一个 PTC 模式的会话**（工具栏「＋」→ 选 PTC 模式）——\
             模式在**建会话时**定，建完不能改。"
                .to_string(),
        );
    }

    // ── 动态尾部 ⑥：待审批记忆候选提醒（审批一次 / 蒸馏一条就变）────────
    //
    // 闭环的关键一环：轮末自动蒸馏会持续往候选队列里塞条目（主对话路径），
    // 但批不批在用户。候选堆着没人知道 = 等于没记。放动态区**最末**：
    // 它是全 prompt 变化最频繁的段之一（每批一条就变），放前面会把后面
    // 全部内容的前缀缓存打掉。
    {
        let n = crate::memory::MemoryStore::new(data_dir)
            .list_pending()
            .map(|v| v.len())
            .unwrap_or(0);
        if n > 0 {
            dynamic.push(format!(
                "# 待处理记忆候选（{n} 条）\n\n\
                 记忆候选队列里有 {n} 条待用户审批的条目。在本轮回答的**收尾处**\
                 用一句话提醒用户：「有 {n} 条记忆候选待处理，可在 设置 › 记忆 里批量批/驳」。\
                 只提一次、一句话，不要展开内容也不要反复催。"
            ));
        }
    }

    stable.extend(dynamic);
    stable.join("\n\n---\n\n")
}

/// 当前 epoch 毫秒。会话与历史模块共用同一口径。
///
/// 历史（2026-10-08 删）：这里原来还有一个 `load_active_mode()` ——
/// 读 `settings.json` 的 `activeMode` 当"当前模式"。模式跟会话走之后
/// 它是**错的**（那是另一个会话的模式），唯一读入口是
/// `sessions::effective_mode(data_dir, session_id)`。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 给模型看的一行「现在是什么时候」，形如 `2026-09-30 14:05（周三）`。
///
/// 复用 `history::fmt_local` / `weekday_cn`，不另写一套时间格式化 ——
/// 两处各写一份，迟早出现"历史里是 09-30、当前时间是 09-29"这种自相矛盾。
fn clock_line(ms: u64) -> String {
    format!(
        "{}（{}）",
        crate::history::fmt_local(ms),
        crate::history::weekday_cn(ms)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- 中途正文落成时间线条目 ----------------

    /// 有内容 → 落一条（返回原文，不 trim）；纯空白 → 不落。
    ///
    /// 回归测试对象：2026-10-02 用户报「我的中途正文呢」—— 模型中途写正文又接着
    /// 调工具时，那段正文以前只发给前端"淡化"，后端不落盘，一轮跑完就消失。
    #[test]
    fn midturn_detail_keeps_real_text_and_drops_blank() {
        assert_eq!(
            midturn_detail("结论：0 扣费的请求走的是自定义模型。"),
            Some("结论：0 扣费的请求走的是自定义模型。".to_string())
        );
        // 原文不动（缩进/换行是模型排版的一部分）
        let multi = "第一行\n  缩进的第二行\n";
        assert_eq!(midturn_detail(multi), Some(multi.to_string()));
        // 空 / 纯空白 → 不落条目（否则时间线上会多一条空行）
        assert_eq!(midturn_detail(""), None);
        assert_eq!(midturn_detail("   \n\t  "), None);
    }

    /// 提升为最终答复时要**删掉同内容的 text 条目**，否则同一段话会出现两次
    /// （一次在「过程」里，一次当答案）。
    #[test]
    fn promoted_midturn_text_is_not_shown_twice() {
        let promoted = "结论：dpsk 确实涨价了约 1.5 倍。";
        let mut steps = vec![
            AgentStep {
                kind: "reasoning".into(),
                name: None,
                detail: "先算一下".into(),
                images: Vec::new(),
            },
            AgentStep {
                kind: "text".into(),
                name: None,
                detail: format!("  {promoted}\n"), // 落盘时可能带空白
                images: Vec::new(),
            },
            AgentStep {
                kind: "tool_call".into(),
                name: Some("read_file".into()),
                detail: "{}".into(),
                images: Vec::new(),
            },
        ];
        // 复刻 1571 分支里的去重逻辑
        if let Some(pos) = steps
            .iter()
            .rposition(|s| s.kind == "text" && s.detail.trim() == promoted.trim())
        {
            steps.remove(pos);
        }
        assert!(
            !steps.iter().any(|s| s.kind == "text"),
            "同内容的中途正文条目应被删掉"
        );
        assert_eq!(steps.len(), 2, "只删那一条，别的条目不能动");
        assert_eq!(steps[0].kind, "reasoning");
        assert_eq!(steps[1].kind, "tool_call");
    }

    // ---------------- 进度事件的字段名契约 ----------------

    /// `Progress` 的字段**必须是 camelCase** —— 前端按 camelCase 读。
    ///
    /// 回归测试对象：2026-09-26 用户报「长命令一直显示已运行 0s」。
    /// 根因是 serde 对**枚举**的 `rename_all` 只重命名变体、不重命名字段，
    /// `elapsed_secs` 出去还是 snake_case，前端 `p.elapsedSecs ?? 0` 永远取到 0。
    /// 修法是给枚举加 `rename_all_fields = "camelCase"`（见 Progress 定义）。
    #[test]
    fn progress_fields_serialize_as_camel_case() {
        let tick = Progress::ToolTick {
            name: "run_command".into(),
            elapsed_secs: 7,
            tail: "x".into(),
        };
        let j = serde_json::to_string(&tick).unwrap();
        assert!(
            j.contains("\"elapsedSecs\":7"),
            "心跳秒数必须是 camelCase（前端读 p.elapsedSecs）: {j}"
        );
        assert!(
            !j.contains("elapsed_secs"),
            "不能漏出 snake_case 字段名: {j}"
        );
        // 变体名也仍是 camelCase（原有的 rename_all 行为不能回退）
        assert!(j.contains("\"kind\":\"toolTick\""), "变体名: {j}");
    }

    /// 插话事件必须带上图片路径 —— 插话不铺气泡，图只能画在时间线 steer 条目上。
    #[test]
    fn progress_steer_carries_images() {
        let p = Progress::Steer {
            id: "st1".into(),
            text: "看这张图".into(),
            images: vec!["D:\\x\\a.png".into()],
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains("\"kind\":\"steer\""), "{j}");
        assert!(j.contains("\"images\":[\"D:\\\\x\\\\a.png\"]"), "图路径要带上: {j}");

        // 没图时不写空数组（省带宽，前端按 undefined 处理）
        let empty = Progress::Steer {
            id: "st2".into(),
            text: "只说话".into(),
            images: Vec::new(),
        };
        let j2 = serde_json::to_string(&empty).unwrap();
        assert!(!j2.contains("images"), "空图不该出现字段: {j2}");
    }

    /// 多会话并行 run：**停 A 绝不能停 B**（每 run 独立的取消标志）。
    ///
    /// 2026-09-26 用户拍板。以前是全局单个 `CHAT_CANCEL`，`chat_cancel` 一按
    /// 所有会话全停。
    #[test]
    fn run_registry_isolates_cancel_per_session() {
        let reg = RunRegistry::default();
        let a = reg.begin("A");
        let b = reg.begin("B");
        assert!(reg.is_active("A") && reg.is_active("B"), "两个会话都该在跑");
        assert_eq!(reg.active_count(), 2);

        // 停 A：只置 A 的标志
        assert!(reg.cancel("A"), "A 在跑，应停到");
        assert!(
            a.cancel.load(std::sync::atomic::Ordering::Relaxed),
            "A 应被取消"
        );
        assert!(
            !b.cancel.load(std::sync::atomic::Ordering::Relaxed),
            "B 不该被误停"
        );

        // 收尾 A：B 还在；A 的槽位要摘掉
        reg.end("A");
        assert!(!reg.is_active("A"), "A 的槽位应已摘掉");
        assert!(reg.is_active("B"), "B 不受影响");
        assert_eq!(reg.active_count(), 1);

        reg.end("B");
        assert_eq!(reg.active_count(), 0);
    }

    /// 插话必须**按会话路由**：B 会话里打的字绝不能插进 A 的对话。
    #[tokio::test]
    async fn run_registry_routes_steers_per_session() {
        let reg = RunRegistry::default();
        let _a = reg.begin("A");
        let _b = reg.begin("B");

        let ack = reg
            .push(
                "B",
                SteeredMsg {
                    id: "s1".into(),
                    text: "只给 B".into(),
                    images: Vec::new(),
                },
            )
            .await;
        assert!(ack.is_ok(), "B 在跑，插话应成功: {ack:?}");

        let got_a = reg.end("A");
        assert!(got_a.is_empty(), "A 不该收到 B 的插话: {:?}", got_a);
        let got_b = reg.end("B");
        assert_eq!(got_b.len(), 1, "B 应收到 1 条");
        assert_eq!(got_b[0].text, "只给 B");

        // 已收尾的会话再插话 → 必须报错，不能塞进没人消费的队列
        let again = reg
            .push(
                "A",
                SteeredMsg {
                    id: "s2".into(),
                    text: "迟到".into(),
                    images: Vec::new(),
                },
            )
            .await;
        assert!(again.is_err(), "A 已收尾，插话应报错");
    }

    /// 排障：导出真实 system prompt ----------------

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
            images: Vec::new(),
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
        let tmp = std::env::temp_dir().join("orbcat_prompt_test");
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
        let tmp = std::env::temp_dir().join("orbcat_prompt_empty");
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
        let tmp = std::env::temp_dir().join("orbcat_prompt_order");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULES_BODY").unwrap();
        std::fs::write(tmp.join("SOUL.md"), "SOUL_BODY").unwrap();
        std::fs::write(tmp.join("USER.md"), "USER_BODY").unwrap();
        // 造一份假的 MCP 能力索引缓存：这样"外部工具索引"段也会出现，
        // 它必须同样落在动态区（拉快照就会变，不是规则文字）。
        std::fs::write(
            crate::mcp::ToolRegistry::index_cache_path_in(&tmp),
            "**demo**（2 个，已加载）：\n- alpha — 做甲事\n- beta — 做乙事\n",
        )
        .unwrap();

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
        // （`## 当前时间` 也在动态区 —— 它每分钟都会变，放稳定区等于每分钟
        //   让后面全部内容按未命中价重付，见 build_system_prompt 的注释）
        //
        // `# 外部工具索引` 必须在这里：它列的是"当前有哪些工具"，
        // 改 server 配置 / 拉快照就变 —— 绝不能混进稳定前缀。
        let dynamic_anchors = [
            "# 外部工具索引",
            "# 技能（Skills）",
            "# 动态信息",
            "## 当前时间",
        ];

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

    /// 稳定前缀必须**逐字节稳定**：同一份输入调两次，**前缀**完全相同。
    ///
    /// ⚠️ 2026-09-30 改口径：以前断言的是"整个 prompt 两次完全一致"，
    /// 但动态区现在含「当前时间」（精确到分钟）。如果两次调用正好跨过一分钟
    /// 边界，这条断言会**偶发失败** —— 那是测试本身不可靠，不是实现错了。
    ///
    /// 真正与 prompt cache 有关的只有**前缀**（缓存按前缀逐字节比对），
    /// 所以这里只比较"第一个动态段之前"的部分：它必须完全稳定。
    /// 动态区之后可以有变化（当前时间、已加载技能、MCP 组状态）。
    #[test]
    fn system_prompt_prefix_is_byte_stable_across_calls() {
        let tmp = std::env::temp_dir().join("orbcat_prompt_stable");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "R").unwrap();

        let a = build_system_prompt(&tmp);
        let b = build_system_prompt(&tmp);

        // 第一个动态锚点：取三者的最早出现位置
        let cut = ["# 技能（Skills）", "# 动态信息", "## 当前时间"]
            .iter()
            .filter_map(|x| a.find(x))
            .min()
            .expect("应有动态段");
        assert!(cut > 0, "动态段不应在最开头");
        assert_eq!(
            &a[..cut],
            &b[..cut],
            "稳定前缀必须逐字节相同（缓存只认前缀）"
        );

        // 反向保证：动态区确实落在前缀之后，别把时间混进稳定区
        let t = a.find("## 当前时间").expect("应有当前时间");
        assert!(t >= cut, "当前时间必须在动态区");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 🔴 能力索引必须落在动态区，且**改动索引不能动到稳定前缀**。
    ///
    /// 这条是本任务加索引时最关键的回归：如果哪天有人图省事把索引拼进稳定区
    /// （比如塞进「外部工具（MCP）的用法」那段），每次换 MCP 配置都会让它
    /// 后面的对话历史规则、技能清单全部按未命中价重付 —— 而且**不会有任何报错**，
    /// 只会慢慢变贵。所以这里直接对比"索引变了"前后两个 prompt 的前缀。
    #[test]
    fn tool_index_change_does_not_invalidate_stable_prefix() {
        let tmp = std::env::temp_dir().join("orbcat_prompt_tool_index");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULES_BODY_IDX").unwrap();
        std::fs::write(tmp.join("SOUL.md"), "SOUL_BODY_IDX").unwrap();

        // ① 没有索引（MCP 未配置）
        let without = build_system_prompt(&tmp);
        assert!(
            !without.contains("# 外部工具索引"),
            "没有索引缓存时不该凭空造一段"
        );

        // ② 有一份索引
        std::fs::write(
            crate::mcp::ToolRegistry::index_cache_path_in(&tmp),
            "**ssh**（2 个，已加载）：\n- run-command — 在远程主机执行命令\n- upload-file — 上传文件\n",
        )
        .unwrap();
        let with = build_system_prompt(&tmp);
        assert!(with.contains("# 外部工具索引"));
        assert!(with.contains("run-command"));

        // ③ 换一份索引（模拟改了 MCP server 配置）
        std::fs::write(
            crate::mcp::ToolRegistry::index_cache_path_in(&tmp),
            "**mysql**（1 个，未加载）：\n- execute_query — 执行只读 SQL\n",
        )
        .unwrap();
        let changed = build_system_prompt(&tmp);
        assert!(changed.contains("execute_query"));
        assert!(!changed.contains("run-command"), "旧索引不该残留");

        // 🔴 核心断言：三种情况下**第一个动态段之前**的部分逐字节相同。
        // （改动索引只允许影响索引自己那一段及其之后 —— 绝不能前移。）
        let cut = |s: &str| {
            [
                "# 外部工具索引",
                "# 技能（Skills）",
                "# 动态信息",
                "## 当前时间",
            ]
            .iter()
            .filter_map(|x| s.find(x))
            .min()
            .expect("应有动态段")
        };
        assert_eq!(
            &without[..cut(&without)],
            &with[..cut(&with)],
            "加索引不能改变稳定前缀"
        );
        assert_eq!(
            &with[..cut(&with)],
            &changed[..cut(&changed)],
            "换索引不能改变稳定前缀（否则后面全部缓存失效）"
        );

        // 顺序：索引是最早的动态段 → 它排在最前（比技能清单更不易变）
        assert!(
            with.find("# 外部工具索引").unwrap() < with.find("# 技能（Skills）").unwrap(),
            "索引应排在技能清单之前（动态区按变化频率从低到高）"
        );
        // 但仍在静态区之后：稳定规则文字的锚点在它前面
        assert!(
            with.find("外部工具（MCP）的用法").unwrap() < with.find("# 外部工具索引").unwrap(),
            "索引是运行时数据，必须在稳定规则文字之后"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 🔴 常驻成本必须可量化：端到端量一次"56 个工具 / 4 组"的真实索引
    /// 会给 system prompt **增加多少 token**。
    ///
    /// 为什么这条测试重要：本任务的硬约束就是"可发现，但绝不把上下文顶爆"。
    /// 索引是**每轮都常驻**的内容，所以它的成本必须被一个具体数字钉住 ——
    /// 对照组是同一段 prompt 在有/无索引两种情况下的 `estimate_tokens` 之差，
    /// 而不是估算某个片段的字数（那会把分隔符、标题、规则文字全漏掉）。
    ///
    /// 上限取 1.5k：真实测量约 1.1k，是完整 schema（~14.4k）的 ~8%。
    /// 如果哪天有人往索引里塞 schema 或参数列表，这条会先炸。
    /// PTC 模式必须给出**行为指令**（不只"全给工具"）。
    ///
    /// 背景（2026-10 用户指出）：agent loop 本来就支持一轮发多个 tool_calls
    /// （`for call in calls` 逐个执行、结果全回灌），但 prompt 里从来没告诉模型
    /// 这件事 —— 于是模型永远一个工具一轮，"PTC"退化成"看得见更多工具"而已。
    ///
    /// 这条测试钉住三件事：
    ///   1. PTC 下 prompt 里给出**程序化**契约（写代码、并发、什么回到上下文）；
    ///   2. 同时说清**中间结果不进上下文**这条最关键的红线；
    ///   3. 非 PTC 模式下不出现这段文字（否则等于把 PTC 变成默认）。
    ///
    /// ⚠️ 2026-10 重写：旧版只断言"一次发多个 tool_calls"。
    /// 那条**不够** —— 让模型自己枚举并行调用只是 native + 更多工具，
    /// 真正的 PTC 要把控制流搬进程序（见 `ptc.rs` 顶部文档）。
    #[test]
    fn ptc_mode_prompt_carries_programmatic_contract() {
        let tmp = std::env::temp_dir().join(format!(
            "orbcat_ptc_mode_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // 默认（标准模式）：不该出现 PTC 的程序化契约
        let standard = build_system_prompt(&tmp);
        assert!(
            !standard.contains("当前模式：PTC"),
            "默认模式不该带 PTC 指令段"
        );
        assert!(
            standard.contains("当前模式：按需"),
            "非 PTC 也要有模式说明，否则模型不知道自己处于哪个模式"
        );

        // 「切到 PTC」现在是**换一个 PTC 会话**（模式跟会话走，2026-10-08）。
        // ⚠️ 不能再用 `settings.activeMode` —— 那个字段已删，而且它现在
        //    根本不影响任何一条会话的模式。
        let sid = crate::sessions::new_session(&tmp, "ptc").id;

        // 注意：这里**不传 SDK**（生产路径 `run()` 才传）。所以验的是
        // 那段契约文字本身；SDK 声明的渲染由 `ptc::` 的单测覆盖。
        let ptc = build_system_prompt_full(&tmp, "", None, Some(&sid));
        assert!(ptc.contains("当前模式：PTC"), "PTC 模式应注入程序化契约");
        assert!(
            ptc.contains("run_code"),
            "必须点明唯一可直接调用的工具是 run_code"
        );
        assert!(
            ptc.contains("tools.<名字>(args)"),
            "必须教程序里怎么调工具"
        );
        assert!(
            ptc.contains("Promise.all"),
            "必须给出并发的写法 —— 这是 PTC 相对 native 的核心卖点之一"
        );
        assert!(
            ptc.contains("只有 `return` 的返回值和你 `console.log` 的内容会回到上下文"),
            "「中间结果不进上下文」是 PTC 最关键的一条，丢了它就退化成 native"
        );
        assert!(
            ptc.contains("ToolCallError"),
            "必须说明失败怎么接（try/catch），否则一个失败就废掉整批"
        );
        assert!(
            ptc.contains("declare const tools"),
            "必须带程序内工具声明（缺 SDK 时给兜底声明，而不是什么都不给）"
        );
        assert!(
            !ptc.contains("当前模式：按需"),
            "两种模式的指令段不能同时出现"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PTC 下**工具列表必须坍缩成只有 `run_code`**。
    ///
    /// 这是"通告面 = 可调用面"：把工具全列出来、却不允许直接调，
    /// 模型会照列表发一个 native 调用，拿到"未启用"的报错，
    /// 然后以为整个部署坏了（DSH 的 executor-collapse note 记的正是这个坑）。
    #[test]
    fn ptc_collapses_tool_list_to_run_code() {
        let tmp = std::env::temp_dir().join(format!(
            "orbcat_ptc_collapse_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // 标准模式：完整工具列表（含 read_file 等）
        let std_sid = crate::sessions::new_session(&tmp, "standard").id;
        let full = crate::tools::tool_specs(None, false, &tmp, Some(&std_sid));
        assert!(
            full.iter().any(|s| s.name == "read_file"),
            "标准模式该有 read_file"
        );
        assert!(
            !full.iter().any(|s| s.name == crate::ptc::RUN_CODE),
            "标准模式不该有 run_code"
        );

        // 换一条 PTC 会话：只剩 run_code（模式跟会话走，2026-10-08）
        let ptc_sid = crate::sessions::new_session(&tmp, "ptc").id;

        let collapsed = crate::tools::tool_specs(None, false, &tmp, Some(&ptc_sid));
        assert_eq!(collapsed.len(), 1, "PTC 下只该剩一个工具");
        assert_eq!(collapsed[0].name, crate::ptc::RUN_CODE);

        // ⚠️ 但 SDK 素材必须是**完整**的（模型写程序时要能调到所有工具）
        let sdk = crate::tools::ptc_sdk_tools(None, false, &tmp, Some(&ptc_sid));
        assert!(
            sdk.iter().any(|(n, _, _)| n == "read_file"),
            "SDK 声明里必须有 read_file —— 坍缩的只是呈现形态，不是能力"
        );
        assert!(
            !sdk.iter().any(|(n, _, _)| n == crate::ptc::RUN_CODE),
            "SDK 里不该有 run_code（它不能递归调用自己）"
        );

        // ⭐ 这条是"模式跟会话走"的直接后果，也是本测试存在的最强理由：
        //    **两条会话并存、模式互不干扰** —— PTC 会话坍缩的同时，
        //    标准会话的工具面必须原样完整。用全局模式做不到这一点。
        let full_again = crate::tools::tool_specs(None, false, &tmp, Some(&std_sid));
        assert!(
            full_again.iter().any(|s| s.name == "read_file"),
            "PTC 会话的存在不该把标准会话的工具一起坍缩掉"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 模式不同只影响**动态区**，不该把稳定前缀打掉（prompt cache 成本）。
    ///
    /// 2026-10-08：模式跟会话走后测法变成"两条不同模式的会话各取一次 prompt"
    /// —— 语义没变（稳定前缀必须逐字节一致），但更贴近真实用法。
    #[test]
    fn mode_switch_only_touches_dynamic_tail() {
        let tmp = std::env::temp_dir().join(format!(
            "orbcat_mode_prefix_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULES_STABLE_MARKER").unwrap();

        // ⚠️ 2026-10-08：模式跟会话走后，"切模式"= **换一条会话**。
        //    这里用两条不同模式的会话来验同一件事，而且比原来更严格：
        //    它验的是"两个会话来来回回取 prompt，稳定前缀都不许动"。
        let sid_std = crate::sessions::new_session(&tmp, "standard").id;
        let sid_ptc = crate::sessions::new_session(&tmp, "ptc").id;

        let a = build_system_prompt_full(&tmp, "", None, Some(&sid_std));
        let b = build_system_prompt_full(&tmp, "", None, Some(&sid_ptc));

        assert_ne!(a, b, "换模式必须真的改变 prompt");
        // 稳定段（红线）在两种模式下都必须原样在，且位置不变
        let ia = a.find("RULES_STABLE_MARKER").unwrap();
        let ib = b.find("RULES_STABLE_MARKER").unwrap();
        assert_eq!(ia, ib, "换模式不该挪动稳定前缀（会打掉 prompt cache）");
        assert_eq!(
            &a[..ia],
            &b[..ib],
            "稳定前缀必须逐字节一致（前缀缓存按字节比对）"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tool_index_constant_cost_stays_small() {
        let tmp = std::env::temp_dir().join("orbcat_prompt_tool_index_cost");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("memory")).unwrap();
        std::fs::write(tmp.join("RULES.md"), "RULES_BODY_COST").unwrap();

        // 对照组：没有索引
        let without = build_system_prompt(&tmp);
        let base = crate::llm::estimate_tokens(&without);

        // 实验组：塞一份与真实网关同形的 56 工具索引
        std::fs::write(
            crate::mcp::ToolRegistry::index_cache_path_in(&tmp),
            crate::mcp::ToolRegistry::test_fixture_index(),
        )
        .unwrap();
        let with = build_system_prompt(&tmp);
        let total = crate::llm::estimate_tokens(&with);
        let cost = total - base;
        eprintln!(
            "[orbcat][test] 能力索引常驻成本：{cost} tokens\
             （prompt {base} → {total}，完整 schema 约 14400）"
        );

        assert!(with.contains("# 外部工具索引"), "应注入索引段");
        assert!(
            (700..1500).contains(&cost),
            "索引常驻成本应在 0.7k~1.5k tokens（真实约 1.1k，完整 schema 的 ~8%），\
             实际 {cost} —— 太大就是变相全量注入，太小说明索引没真的进去"
        );

        // 上面那条断言的另一半：索引段本身**不含**任何 schema 字段名
        let idx_at = with.find("# 外部工具索引").unwrap();
        let skills_at = with.find("# 技能（Skills）").unwrap();
        let section = &with[idx_at..skills_at];
        assert!(!section.contains("properties"), "索引段不得携带 schema");
        assert!(
            !section.contains("inputSchema") && !section.contains("required"),
            "索引段不得携带 schema 字段"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 方案 B：用户消息命中技能关键词时，正文应自动出现在 prompt 的动态尾部，
    /// 且不得破坏"静态在前、动态在后"的前缀稳定性。
    #[test]
    fn system_prompt_autoloads_matched_skill() {
        let tmp = std::env::temp_dir().join("orbcat_prompt_autoload");
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
        let hit = build_system_prompt_full(&tmp, "帮我重新构建一下", None, None);
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
            // 联调不增量落盘（None = 不写盘）
            None,
            None,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    // ---- 伪工具调用解析（mimo 提前停止的兜底）----

    #[test]
    fn pseudo_calls_parsed_from_folded_text() {
        // 真实案例形态（2026-09-25 会话 s1790306950716）：mimo 把工具调用
        // 模仿成历史回灌的 «steps 折叠格式写进正文
        let text = "继续验证反代。跑最终测试。\n«steps\n·think Let me check\n\
                    调用 run_command\n  参数: {\"command\": \"echo hi\"}\n\
                    run_command 返回:\n$ echo hi\n»";
        let calls = super::extract_pseudo_calls(text).expect("应解析出伪调用");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "run_command");
        assert!(calls[0].function.arguments.contains("echo hi"));
        assert!(super::smells_like_pseudo(text));
    }

    #[test]
    fn pseudo_calls_multiple_and_invalid_json_skipped() {
        let text = "调用 read_file\n  参数: {\"path\": \"a.txt\"}\n\
                    调用 write_file\n  参数: {不是合法json}\n\
                    调用 list_dir\n  参数: {\"path\": \"d\"}";
        let calls = super::extract_pseudo_calls(text).expect("应解析出 2 个");
        assert_eq!(calls.len(), 2, "非法 JSON 的那条应被跳过");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[1].function.name, "list_dir");
    }

    #[test]
    fn plain_answer_is_not_pseudo() {
        // 正常回答里提到"调用"一词不算；没有「参数:」配对不收
        let text = "这个函数会调用 read_file 来读取文件。";
        assert!(super::extract_pseudo_calls(text).is_none());
        assert!(!super::smells_like_pseudo(text));
    }

    // ---------------- 空应答收尾的运行时判定（2026-10-01） ----------------
    //
    // 抄 MiMo/WB 的 autoContinueInvalidOutput：回合结束但没有可用正文时，
    // 必须注入提醒让它接着答，而不是把「已完成」当成结论交给用户。
    // 提示词是软约束（模型可以不理），这一层是硬约束。

    #[test]
    fn empty_and_acknowledgement_only_are_invalid_output() {
        // 空正文 —— 最典型的 invalid
        assert!(super::is_acknowledgement_only(""));
        assert!(super::is_acknowledgement_only("   \n  "));

        // 纯确认语：用户问的是问题，不是"你做完了没"
        for t in [
            "已完成",
            "已完成。",
            "已回答",
            "好的",
            "收到！",
            "任务完成",
            "搞定了",
            "Done",
            "Done.",
            "Task complete",
            "OK",
        ] {
            assert!(
                super::is_acknowledgement_only(t),
                "{t:?} 应判为应答式收尾（无可用正文）"
            );
        }
    }

    #[test]
    fn real_answers_are_never_treated_as_acknowledgement() {
        // ★ 防误伤：正常回答绝不能触发续跑，否则会把好答案重问一遍
        for t in [
            "WB 保证最后稳定回答靠三层机制：①运行时判定……",
            "文件在第 42 行，内容如下：",
            "这个问题的原因是 max_tokens 设置过小，建议调到 8192。",
            // 长正文里出现"已完成"是在叙述，不是在打官腔
            "我已经完成了对 agent.rs 的检查，发现主循环缺少运行时兜底判定。\
             具体来说，第 1388 行的『真的答完了』分支会把空正文当成最终答案接受，\
             这跟 MiMo 的 autoContinueInvalidOutput 机制相比少了一层保护。",
            // 带代码块 / 表格 / 多行 = 在交付内容
            "```rust\nfn main() {}\n```",
            "| 项 | 值 |\n|---|---|\n| a | b |",
            "第一行\n第二行\n第三行\n第四行",
        ] {
            assert!(
                !super::is_acknowledgement_only(t),
                "{t:?} 是正常回答，不能判为应答式收尾"
            );
        }
    }

    #[test]
    fn acknowledgement_length_gate_blocks_false_positive() {
        // 关键闸门：超过 ACK_ONLY_MAX_CHARS 的正文一律不判为应答式，
        // 哪怕它以"已完成"开头 —— 那多半是在正常汇报。
        let long = format!("已完成{}", "补充说明".repeat(30));
        assert!(long.chars().count() > super::ACK_ONLY_MAX_CHARS);
        assert!(!super::is_acknowledgement_only(&long));
    }

    // ------------- 「把答案指向别处」的运行时判定（2026-10-02 实测复现） -------------
    //
    // 用户报的**原始问题**就是这个：AI 最后不说结论，只说「结论已给出 / 见上一条回复」。
    // 上一版只防了「太短的空话」（ACK_ONLY_MAX_CHARS = 80），
    // 而真实失败是 183 字符的指路式收尾 —— 正好从闸门缝里漏过去。

    /// 会话 `s1790916789470-0` 第 6 条的**逐字原文**（用户当场追问「结论给到哪里了」）。
    const REAL_DEFERRAL_TEXT: &str = "结论已给出（缓存命中与未命中分开计价，1M token 命中 50% vs 90% 积分差 1.8~4.2 倍，模型间差异明显），完整表格和铁证数据见上一条回复。\n\n公式修正已写入项目记忆，作废了之前「0.06x→27.8万 token/积分」的旧条目——那个只是 94% 命中率下的混合价，真实范围是 0% 命中≈9万 token/积分 到 95% 命中≈33万。";

    #[test]
    fn deferral_to_an_earlier_message_is_invalid_output() {
        // ★ 这条断言就是用户报的那个 bug。它 183 字符，比 ACK_ONLY_MAX_CHARS 长，
        //   所以 is_acknowledgement_only 放过它 —— 必须由 is_deferral_only 拦住。
        assert!(
            !super::is_acknowledgement_only(REAL_DEFERRAL_TEXT),
            "前提：这条确实能穿过 80 字符闸门（否则这个测试就失去意义了）"
        );
        assert!(
            super::is_deferral_only(REAL_DEFERRAL_TEXT),
            "★ 实测原文必须判为「把答案指向别处」——这正是用户报的原始问题"
        );

        // 同类指路说法
        for t in [
            "结论已给出，详见上一条回复。",
            "分析完成，完整数据见上条消息。",
            "如上所述，不再重复。",
            "详见上文表格。",
            "The full table was given in my previous message.",
            "As above, credits differ.",
        ] {
            assert!(super::is_deferral_only(t), "{t:?} 是指路式收尾，应判为无效");
        }
    }

    #[test]
    fn real_answers_are_never_treated_as_deferral() {
        // ★ 防误伤：真答案不能被这条规则打回重答。
        //   下面这坨是同一会话第 3 条的**逐字原文**（一条正常的、带结论的回答）。
        let real_good = "**0.06 倍率下，1 积分 ≈ 27.8 万 token**（与上一轮实测的 27.75 万完全吻合）。\n\n反推出的 WB 积分公式：`积分 = token × 60 × 模型倍率 / 1,000,000`，即 1.0x 倍率 = 60 积分/百万 token。截图里各模型的换算：Hy3 免费、GLM-5.3-Flash 0.06x→27.8万 token/积分、Deepseek-V4.1-Flash 0.11x→15.2万、MiniMax-M3 0.25x→6.7万、公式已记入项目记忆。";
        assert!(
            !super::is_deferral_only(real_good),
            "★ 同会话里的正常回答不能被误判（否则会把好答案重问一遍）"
        );

        // 自己带了表格 / 代码块 = 在交付内容，「见上表」指的就是本条消息，合法
        let with_table = "结论：缓存命中影响积分。\n\n| 命中率 | token/积分 |\n|---|---|\n| 0% | 9万 |\n| 95% | 33万 |";
        assert!(!super::is_deferral_only(with_table));
        let with_code = "改好了，关键改动：\n\n```rust\nconst MAX: usize = 2;\n```";
        assert!(!super::is_deferral_only(with_code));

        // 很长的正文自带答案，里面出现「如上」是正常叙述
        let long = format!(
            "完整分析如下。{}如上，结论是缓存命中越多越省积分。",
            "数据表明该字段确实存在，".repeat(60)
        );
        assert!(long.chars().count() > super::DEFERRAL_MAX_CHARS);
        assert!(!super::is_deferral_only(&long));

        // 空正文由 is_acknowledgement_only 负责，这里不重复判
        assert!(!super::is_deferral_only(""));
        assert!(!super::is_deferral_only("   "));
    }

    /// 复刻失败那轮的真实消息序列里，一段「中途写下、又被归档」的实质正文。
    fn midturn_table() -> String {
        format!(
            "缓存命中确实影响积分，实测如下：\n\n\
             | 命中率 | token/积分 |\n|---|---|\n| 0% | 9万 |\n| 95% | 33万 |\n\n{}",
            "依据：逐请求累加与面板 summary 对得上。".repeat(8)
        )
    }

    #[test]
    fn midturn_text_is_promoted_when_the_turn_ends_by_pointing_elsewhere() {
        use crate::llm::ChatMessage;
        let table = midturn_table();
        assert!(table.chars().count() >= super::MIDTURN_PROMOTE_MIN_CHARS);

        // user(问题) → assistant(中途写了整张表) → tool → assistant(收尾只指路)
        let msgs = vec![
            ChatMessage::system("…"),
            ChatMessage::user("这个有没有缓存干扰啊"),
            ChatMessage::assistant(table.clone()),
            ChatMessage::tool_result("call_1", "ok"),
            ChatMessage::assistant(REAL_DEFERRAL_TEXT),
        ];
        assert_eq!(
            super::last_substantive_midturn_text(&msgs).as_deref(),
            Some(table.as_str()),
            "★ 收尾只指路时，必须能把本轮中途写好的正文捞回来当答案"
        );

        // ★ 不能越过本轮起点，把**上一轮**的旧结论捞回来当答案
        let cross_turn = vec![
            ChatMessage::system("…"),
            ChatMessage::user("上一个问题"),
            ChatMessage::assistant(table.clone()),
            ChatMessage::user("新问题"),
            ChatMessage::assistant("结论已给出，见上一条回复。"),
        ];
        assert_eq!(
            super::last_substantive_midturn_text(&cross_turn),
            None,
            "★ 上一轮的答案不能当本轮的答案"
        );

        // 中途只写了句进度话（太短）→ 不提升，退回注入提醒让模型重写
        let only_progress = vec![
            ChatMessage::system("…"),
            ChatMessage::user("问题"),
            ChatMessage::assistant("我先看一下文件。"),
            ChatMessage::assistant(REAL_DEFERRAL_TEXT),
        ];
        assert_eq!(super::last_substantive_midturn_text(&only_progress), None);

        // 中途那段本身也是指路 → 不能拿来提升（否则等于把「见上一条」当答案）
        let deferral_mid = vec![
            ChatMessage::system("…"),
            ChatMessage::user("问题"),
            ChatMessage::assistant(format!("分析完成，完整数据见上一条回复。{}", "详".repeat(220))),
            ChatMessage::assistant(REAL_DEFERRAL_TEXT),
        ];
        assert_eq!(super::last_substantive_midturn_text(&deferral_mid), None);
    }

    // ---------------- 流式落盘节流（2026-09-30） ----------------

    /// 数调用次数的 checkpoint 回调工厂
    fn counting_ckpt() -> (
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        Option<CheckpointFn>,
    ) {
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        let cb: CheckpointFn = std::sync::Arc::new(move |_s: &[AgentStep], _a: &str, _r: &str| {
            n2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        (n, Some(cb))
    }

    /// 空正文不落盘：思考分片来了但没有新正文时，写一次盘纯属浪费
    #[test]
    fn stream_checkpoint_skips_empty_text() {
        let (calls, cb) = counting_ckpt();
        let clock = std::sync::atomic::AtomicU64::new(0);
        checkpoint_from_stream(&cb, &clock, &[], "", "想了一下");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// 节流：同一轮里连续多次调用只落一次盘（相邻两份会落两次）
    ///
    /// 为什么这条重要：`checkpoint` 会 `write_session` **整个会话 JSON**，
    /// 而流式分片是几十毫秒一片 —— 不节流就是每秒重写会话好几次。
    #[test]
    fn stream_checkpoint_throttles_bursts() {
        let (calls, cb) = counting_ckpt();
        let clock = std::sync::atomic::AtomicU64::new(0);

        // 第一片：`last == 0` 视为"本轮还没落过" → 立刻落
        checkpoint_from_stream(&cb, &clock, &[], "第一段", "");
        // 紧接着的三片会被节流挡掉（间隔远小于 CHECKPOINT_THROTTLE_MS）
        checkpoint_from_stream(&cb, &clock, &[], "第二段", "");
        checkpoint_from_stream(&cb, &clock, &[], "第三段", "");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "突发分片只该落一次盘"
        );

        // 把时钟拨回 0：等于"距上次落盘已经很久" → 又能落一次
        clock.store(0, std::sync::atomic::Ordering::Relaxed);
        checkpoint_from_stream(&cb, &clock, &[], "第四段", "");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "过了节流窗口应再落一次"
        );
    }

    /// 没配回调（如单测里的 `run(..., None, ...)`）时不能 panic
    #[test]
    fn stream_checkpoint_noop_without_callback() {
        let clock = std::sync::atomic::AtomicU64::new(0);
        checkpoint_from_stream(&None, &clock, &[], "正文", "");
        assert_eq!(clock.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    // ---------------- 校准（上下文预算的真锚点） ----------------

    /// 校准倍率：真实 prompt 明显大于固定开销时，按「(真实−开销) ÷ 估算」算
    #[test]
    fn calibration_computes_scale_from_real_prompt() {
        let mut c = Calibration::default();
        // 真实 10000，其中 2000 是 system+工具 schema 的开销；本地估算消息 1000
        // → 倍率 = (10000-2000)/1000 = 8
        c.observe(10_000, 2_000, 1_000);
        assert_eq!(c.last_prompt, Some(10_000));
        assert_eq!(c.scale, Some(8.0));
        assert_eq!(c.apply(1_000), 8_000, "估算应按倍率放大");
    }

    /// 真实值不比开销大时不更新倍率 —— 否则分母趋零会算出虚高的荒谬倍率
    #[test]
    fn calibration_skips_when_real_is_not_larger_than_overhead() {
        let mut c = Calibration::default();
        c.observe(500, 20_000, 1_000);
        assert_eq!(c.last_prompt, Some(500), "真实值本身仍要记住");
        assert_eq!(c.scale, None, "但倍率不该更新");
    }

    /// 倍率被夹在保守区间里：单次调用的离群值不该污染后面所有轮次
    #[test]
    fn calibration_clamps_extreme_scale() {
        let mut c = Calibration::default();
        // 算出 1000 倍的荒谬值
        c.observe(1_000_000, 0, 1_000);
        assert_eq!(c.scale, Some(TOKEN_SCALE_MAX));
    }

    /// 倍率为 1（或更小）时 `apply` 不该把估算**变小** —— 低估是我们要治的病
    #[test]
    fn apply_never_shrinks_estimate() {
        let c = Calibration {
            last_prompt: None,
            scale: Some(0.5),
        };
        assert_eq!(c.apply(1_000), 1_000, "小于 1 的倍率不生效");
    }

    /// 服务端不给 usage（prompt = 0）时保持原状，不把已校准的会话抹掉
    #[test]
    fn calibration_ignores_zero_usage() {
        let mut c = Calibration {
            last_prompt: Some(123),
            scale: Some(4.0),
        };
        c.observe(0, 0, 999);
        assert_eq!(c.last_prompt, Some(123));
        assert_eq!(c.scale, Some(4.0));
    }

    /// 🔴 回归：**旧闸门会放行一个已经超水位的上下文**（这是本项要修的 bug）
    ///
    /// 场景照着实测数据构造：模型配了 `maxInputTokens = 1_000_000`（水位 60 万），
    /// 而本地估算对真实 prompt 低估约百倍 —— 实测某个会话的真实 prompt 求和
    /// 2340 万、落盘消息才几百 KB，本地估算与服务端真实值差 4.9~144 倍。
    ///
    /// 旧实现只拿「历史估算」去比水位，估算值永远够不着 → 闸门形同虚设，
    /// 上下文一路裸奔（这正是 30 个真实会话里 compact **一次都没触发**的原因）。
    #[test]
    fn calibrated_budget_catches_what_old_gate_missed() {
        // 一条"看起来不大"的消息：36 万 ASCII 字符 ≈ 9 万 tok 的本地估算
        let big = "x".repeat(360_000);
        let messages = vec![ChatMessage::user(&big)];
        let est_msgs = estimate_messages_tokens(&messages);
        assert!(
            (89_000..91_000).contains(&est_msgs),
            "本地估算应在 9 万上下，实际 {est_msgs}"
        );

        let context_window = 1_000_000usize;
        let compact_trigger =
            ((context_window as f64) * crate::history::COMPACT_TRIGGER_RATIO) as usize;
        assert_eq!(compact_trigger, 600_000);

        // ① 旧口径（未校准）：估算 9 万 << 水位 60 万 → **判不超，闸门放行**
        let uncalibrated = Calibration::default();
        assert!(
            uncalibrated.apply(est_msgs) < compact_trigger,
            "这正是 bug 现场：未校准时估算远低于水位，所以 compact 从不触发"
        );

        // ② 新口径：服务端实测这个上下文其实是 95 万 tok、固定开销 5 万。
        //    倍率 = (950000-50000)/90000 = 10 → 校准后判超，闸门拦下
        let mut cal = Calibration::default();
        let real = 950_000u32;
        let overhead = 50_000usize;
        cal.observe(real, overhead, est_msgs);
        let scale = cal.scale.expect("应算出倍率");
        assert!(
            (9.8..10.2).contains(&scale),
            "倍率应约为 (950000-50000)/估算 = 10，实际 {scale}"
        );
        let measured = cal.apply(est_msgs) + overhead;
        assert!(
            (940_000..960_000).contains(&measured),
            "校准后应能反推出真实量级（约 95 万），实际 {measured}"
        );
        assert!(
            measured > compact_trigger,
            "真实 95 万 > 水位 60 万 → **必须判超**（旧实现这里会放行、上下文裸奔）"
        );

        // ③ 光有真值、倍率没算出来的情况（分母太小等）也得拦住：
        //    `apply` 在倍率为 None 时原样返回，但上游用的是**真实 prompt**，
        //    所以带真值的会话永远不依赖估算。
        let mut real_only = Calibration::default();
        real_only.observe(real, overhead, 0); // estimated_msgs = 0 → 不更新倍率
        assert_eq!(real_only.last_prompt, Some(real));
        assert_eq!(real_only.scale, None);
        assert!(
            real_only.last_prompt.unwrap() as usize > compact_trigger,
            "真值在手时判定量就是它，与估算无关"
        );
    }
}
