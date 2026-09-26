//! 多轮上下文回灌 —— 把磁盘上的会话历史转成发给模型的消息。
//!
//! ## 为什么单独一份
//! `sessions.rs` 只管**怎么存**（JSON 落盘、消息上限、工具结果折叠），
//! 这里只管 **发什么给模型**（时间窗、轮数兜底）。两者正交：
//! 存的是预算内的原文，发的是预算内的子集。
//!
//! ⚠️ **这里不做任何"读取时加工"**。工具结果折叠已在落盘时完成
//! （`sessions::append_turn`）—— 因为 prompt cache 按 token 前缀逐字节比对，
//! 每轮读出来的历史必须字节稳定。唯一的例外是存量老数据（折叠上线前
//! 落盘的原文 steps），见 [`to_chat_message`] 里的兜底。
//!
//! ## 选取规则（用户 2026-09-19 定案）
//! ```text
//!   带入 = (最近 WINDOW_HOURS 小时内的消息) ∪ (最近 FALLBACK_TURNS 轮)
//! ```
//! - **时间窗**给大方向：长时间没聊，之前的话题自然淡出
//! - **轮数兜底**防断片：隔夜回来（时间窗外）至少还知道刚在说什么
//!
//! 用并集而不是二选一，是因为单靠时间窗会出现「昨天 22:00 聊到 23:50，
//! 今天 15:00 回来一个字的上下文都不带」——那正是用户要避免的。
//!
//! ## 更早的内容
//! 被窗口挡掉的部分**不是丢弃**，而是留给模型自己用 [`crate::tools`] 的
//! `recall_turns` 工具去查（原文永久留在 `sessions/<id>.json`）。
//! 但模型得先知道"有东西可查"，所以 system prompt 尾部会追加一句告知
//! （见 `agent::build_system_prompt` 的调用方）。

use crate::config::ModelConfig;
use crate::llm::ChatMessage;
use crate::sessions::{self, StoredMessage};

/// 摘要消息的固定头（学 WorkBuddy 的 `generateSummaryHeader`）。
///
/// 为什么要有固定头：① 模型看到这段会明白"下面是压缩过的历史，不是用户原话"，
/// 不会把摘要当成用户刚说的话去回应；② 逐字节固定 → prompt cache 友好。
const SUMMARY_HEADER: &str = "Summary of the conversation so far:\n\
The conversation is between an AI agent and a user.\n\
Use this to get up to speed, and continue helping the user as the AI agent.\n\
Some contents may be omitted.\n\
（以下是本会话较早内容的压缩摘要，供你恢复上下文；不是用户刚刚说的话。）";

/// 时间窗：带多少小时内的对话
pub const WINDOW_HOURS: u64 = 6;

/// 轮数兜底：至少带最近这么多条消息（**消息条数**，不是对话轮次）
///
/// 用户口径是"前 10 条"，按消息条数算 —— 一问一答算 2 条。
pub const FALLBACK_TURNS: usize = 10;

/// 一条历史消息 + 它是否属于"当前活跃窗口"
struct Picked {
    msg: StoredMessage,
    /// 由轮数兜底（而非时间窗）选中的老消息。
    ///
    /// 保留这个标记是为了让它可被观测/调试：折叠本身已在落盘时完成，
    /// 不再区分力度（见 `sessions::append_turn` 的说明）。
    #[allow(dead_code)]
    fallback: bool,
}

/// 组装要回灌的历史消息（不含 system、不含当前这轮的输入）。
///
/// 返回的 `ChatMessage` 顺序与磁盘一致（旧 → 新），可直接插在
/// `[system]` 之后、当前用户输入之前。
///
/// **compact 摘要**：若会话带 `summary` / `summary_upto`，则
/// `messages[0..summary_upto]` 不再逐条回灌，改为在最前面插一条
/// 摘要消息（[`SUMMARY_HEADER`] + 摘要正文）。摘要之后的区间照常按
/// 时间窗 ∪ 轮数兜底选取。
pub fn build_history(data_dir: &std::path::Path, session_id: &str, now_ms: u64) -> Vec<ChatMessage> {
    let Some(s) = sessions::load(data_dir, session_id) else {
        return Vec::new();
    };

    // 被摘要覆盖的消息（messages[0..upto]）不再逐条回灌：
    // 直接对**未摘要区间**套用时间窗 ∪ 轮数兜底，规则与原来一致。
    let upto = s.summary_upto.unwrap_or(0).min(s.messages.len());
    let picked = pick(&s.messages[upto..], now_ms);

    let mut out: Vec<ChatMessage> = Vec::new();
    // 摘要放在最前（它在语义上"概括了更早的内容"，顺序上必须在所有存活消息之前）
    if let Some(sum) = s.summary.as_deref() {
        if !sum.trim().is_empty() {
            out.push(ChatMessage::system(format!("{SUMMARY_HEADER}\n\n{sum}")));
        }
    }
    out.extend(picked.into_iter().filter_map(|p| {
        // ⚠️ 空的助手消息不回灌（2026-09-26）。来源：「退出不丢轮」机制在开跑时
        // 落下的 `partial` 占位 —— 进程若在产出任何内容前被关掉，磁盘上就留着
        // 一条 `text=""`、`steps=[]` 的助手消息。把它喂给模型，部分厂商 API 会
        // 直接因"空 content"报 400。有 steps 的占位照常回灌（那是真实轨迹）。
        let empty_assistant =
            p.msg.role == "assistant" && p.msg.text.trim().is_empty() && p.msg.steps.is_empty();
        if empty_assistant {
            return None;
        }
        Some(to_chat_message(p))
    }));
    out
}

/// 挑出要回灌的消息（时间窗 ∪ 轮数兜底）
fn pick(messages: &[StoredMessage], now_ms: u64) -> Vec<Picked> {
    if messages.is_empty() {
        return Vec::new();
    }

    let cutoff = now_ms.saturating_sub(WINDOW_HOURS * 60 * 60 * 1000);

    // 从最新往回找"轮数兜底"的起点。一条"轮"= 一条用户消息，
    // 但我们的上限按**消息条数**算（用户口径"前 10 条"），
    // 所以直接取最后 FALLBACK_TURNS 条。
    let fallback_start = messages.len().saturating_sub(FALLBACK_TURNS);

    messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let in_window = m.at >= cutoff;
            let in_fallback = i >= fallback_start;
            if !in_window && !in_fallback {
                return None;
            }
            Some(Picked {
                msg: m.clone(),
                // 靠兜底进来的（时间窗外）→ 标成 fallback，折叠更狠
                fallback: !in_window,
            })
        })
        .collect()
}

/// **下一轮会喂给模型的那段上下文**（就是 [`pick`] 选出来的子集，按原顺序）。
///
/// 单独暴露出来是给 `sessions::fork` 用的：分叉的语义是"把下一轮的上下文
/// 窗口截出来开一条新线"，所以它必须和回灌**共用同一套规则** —— 两边各写一份
/// 迟早会不一致（改了窗口参数只改一处，分叉出来的上下文就和真实对话不符了）。
///
/// 注意：传入的切片通常是某个前缀（分叉点之前的消息），选出来的最后一条
/// 一定落在最近 [`FALLBACK_TURNS`] 条之内，所以**分叉点自己必然在里面**。
pub fn pick_window(messages: &[StoredMessage], now_ms: u64) -> Vec<StoredMessage> {
    pick(messages, now_ms).into_iter().map(|p| p.msg).collect()
}

/// `StoredMessage` → `ChatMessage`
fn to_chat_message(p: Picked) -> ChatMessage {
    let m = p.msg;

    // 有图的历史消息：**不带图本体**。
    // 历史里的图（截图等）重发代价极高（一张 base64 几百 KB，
    // 而且模型看到的往往是当时的瞬时画面，早过期了）。
    // 只留一行文字说明"当时附过图"，需要时它能用 read_file 去看。
    let mut text = m.text.clone();
    if !m.images.is_empty() {
        text.push_str(&format!(
            "\n（本条曾附带 {} 张图片，图片不在当前上下文中，需要时用 read_file 读取：{}）",
            m.images.len(),
            m.images.join("、")
        ));
    }

    // 工具步骤回灌（2026-09-23 修正）：
    //
    // 落盘已走 `sessions::cap_steps`（**保序多条**，含 tool_result），不再是
    // 一行摘要。这里直接交给 [`fold_steps_to_line`]：
    // - 已折叠老格式（1 条 `[已执行…`）→ 原样返回
    // - 结构化多条 steps → **近 K 条原文/截断**（含 tool_result），更旧折一行
    //
    // ⚠️ 不要再 `sessions::fold_steps` 预折叠：那会把 tool_result 全部丢掉，
    // 模型下一轮「失忆」重跑 Get-Content（STATUS 2026-09-22 根因）。
    //
    // 每轮读到的历史对**同一会话前缀**保持稳定（fold 规则只依赖 steps 本身），
    // 有利于 prompt cache；不要在读取路径引入时间戳/随机量。
    let folded = fold_steps_to_line(&m.steps);
    if !folded.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        // 明确圈出轨迹块：降低模型把工具日志续写进正文的概率
        text.push_str("«steps\n");
        text.push_str(&folded);
        text.push_str("\n»");
    }

    match m.role.as_str() {
        "assistant" => ChatMessage::assistant(text),
        _ => ChatMessage::user(text),
    }
}

/// 是否「旧折叠格式」：只有一条 `tool_call`，且 `detail` 以 `[已执行` 开头。
///
/// 新数据是 `cap_steps` 保序多条（含 tool_result），**不是** legacy。
/// 仅用于识别历史里真正被旧 `fold_steps` 压成一行的记录。
fn is_already_folded_line(steps: &[sessions::StoredStep]) -> bool {
    if steps.len() != 1 {
        return false;
    }
    let s = &steps[0];
    s.kind == "tool_call" && s.detail.trim_start().starts_with("[已执行")
}

/// 已折叠的 steps → 拼进历史文本的那一行（保留近期原文/截断，更旧折一行）
///
/// **为什么必须保留近期 tool_result**（2026-09-23 用户点名）：
/// 早先实现只 `find` 首条 `tool_call` detail，`tool_result` 全部丢弃 ——
/// 跨轮/插话后模型「失忆」，只能重跑 `Get-Content`/`Select-String` 打转。
/// 现在与 `sessions::cap_steps` 对齐：**落盘给模型看的也不是一行摘要**。
fn fold_steps_to_line(steps: &[sessions::StoredStep]) -> String {
    const KEEP_RECENT_STEPS: usize = 12;
    const TOOL_RESULT_LIMIT: usize = 4 * 1024; // 按**字符**计，防 UTF-8 边界 panic

    if steps.is_empty() {
        return String::new();
    }

    // 旧折叠格式：单条 tool_call 且以 [已执行 开头 → 原样返回
    if is_already_folded_line(steps) {
        return steps[0].detail.clone();
    }

    let n = steps.len();
    let split = n.saturating_sub(KEEP_RECENT_STEPS);
    let mut parts: Vec<String> = Vec::new();

    if split > 0 {
        let older = &steps[..split];
        let calls = older.iter().filter(|s| s.kind == "tool_call").count();
        let results = older.iter().filter(|s| s.kind == "tool_result").count();
        let last_name = older
            .iter()
            .rev()
            .find(|s| s.kind == "tool_call" || s.kind == "tool_result")
            .and_then(|s| s.name.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("?");
        parts.push(format!(
            "[已执行：更早 steps 已折叠 tool_call×{calls} tool_result×{results}；最后: {last_name}]"
        ));
    }

    for s in &steps[split..] {
        match s.kind.as_str() {
            "tool_call" => {
                let args: String = s.detail.chars().take(120).collect();
                parts.push(format!("调用 {}", s.name.as_deref().unwrap_or("?")));
                if !args.is_empty() {
                    parts.push(format!("  参数: {args}"));
                }
            }
            "tool_result" => {
                let mut d = s.detail.clone();
                if d.chars().count() > TOOL_RESULT_LIMIT {
                    // 按字符截断，避免 String::truncate 在 UTF-8 边界 panic
                    let cut: String = d.chars().take(TOOL_RESULT_LIMIT).collect();
                    d = cut;
                    d.push_str("…[截断]");
                }
                parts.push(format!(
                    "{} 返回:\n{}",
                    s.name.as_deref().unwrap_or("?"),
                    d
                ));
            }
            "tool_error" | "error" => {
                parts.push(format!(
                    "{} {}: {}",
                    if s.kind == "error" { "错误" } else { "工具失败" },
                    s.name.as_deref().unwrap_or(""),
                    s.detail
                ));
            }
            "reasoning" => {
                // 思考已单独有 reasoning 字段；这里只留极短摘要，避免与 reason_total 重复
                // ⚠️ 前缀刻意不用「[思考]」这种会被模型当成正文续写的格式（2026-09-24
                //    用户截图：商汤把 fold 尾巴回显进 answer.text）。改用轨迹块标记。
                let head: String = s.detail.trim().chars().take(80).collect();
                if !head.is_empty() {
                    parts.push(format!("·think {head}"));
                }
            }
            "status" | "omitted" => {
                let t: String = s.detail.chars().take(120).collect();
                if !t.is_empty() {
                    parts.push(format!("·status {t}"));
                }
            }
            // 多轮流式正文（按轮收进时间线的中间话）
            "text" | "stream" => {
                let t: String = s.detail.trim().chars().take(200).collect();
                if !t.is_empty() {
                    parts.push(format!("·text {t}"));
                }
            }
            // 插话已作为独立 user 消息落盘回灌；steps 里只服务 UI 时间线，这里跳过防重复
            "steer" => {}
            _ => {}
        }
    }

    parts.join("\n")
}

// ---------------------------------------------------------------------------
// recall_turns 工具用：按关键词搜当前会话的历史原文
// ---------------------------------------------------------------------------

/// 一条搜索结果
pub struct Hit {
    /// `user` | `assistant`
    pub role: String,
    /// 人类可读时间（本地时间 `MM-DD HH:MM`）
    pub when: String,
    pub text: String,
}

/// 在当前会话里搜历史原文（**带 steps 的完整记录**，不是折叠版）。
///
/// - `query` 为空 → 只按时间倒序返回最近 `limit` 条（用于"翻翻刚才聊了啥"）
/// - `query` 非空 → 大小写不敏感的子串匹配（`text` 字段）
/// - `hours` 给定时只在最近 N 小时内搜
pub fn search(
    data_dir: &std::path::Path,
    session_id: &str,
    query: &str,
    limit: usize,
    hours: Option<u64>,
    now_ms: u64,
) -> Vec<Hit> {
    let Some(s) = sessions::load(data_dir, session_id) else {
        return Vec::new();
    };

    let cutoff = hours.map(|h| now_ms.saturating_sub(h * 60 * 60 * 1000));
    let needle = query.trim().to_lowercase();

    let mut hits: Vec<Hit> = s
        .messages
        .iter()
        .filter(|m| {
            if let Some(c) = cutoff {
                if m.at < c {
                    return false;
                }
            }
            if needle.is_empty() {
                return true;
            }
            m.text.to_lowercase().contains(&needle)
        })
        .map(|m| Hit {
            role: m.role.clone(),
            when: fmt_local(m.at),
            // 单条结果仍然截断 —— 模型缺的是线索，不是全文；
            // 真要全文它有 read_file（但会话 JSON 不建议直接读）
            text: m.text.chars().take(600).collect(),
        })
        .collect();

    // 最近的在最后 → 截断时优先丢远的
    if hits.len() > limit {
        let cut = hits.len() - limit;
        hits.drain(..cut);
    }
    hits
}

/// epoch 毫秒 → 本地 `MM-DD HH:MM`
///
/// 手写而不用 chrono：只为格式化一个时间戳引一个 crate 不划算，
/// 且 `libc::localtime_r` 在 Windows 上不可用。这里走 `std::time` 的
/// 「UTC 秒 + 固定偏移」近似 —— 精确到分钟对"回忆昨天聊了什么"足够。
fn fmt_local(ms: u64) -> String {
    // 从 epoch 天数算年月日（不引第三方库的民用历法算法）
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;

    let (y, mo, d) = civil_from_days(days);
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    // 本机为 UTC+8；不追求跨时区正确性，显式标注避免误读
    let h = (h + 8) % 24;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// Howard Hinnant 的 `civil_from_days`（days since 1970-01-01 → 年月日）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// compact —— 上下文压缩（打分保原文 + 模型摘要兜底）
// ---------------------------------------------------------------------------

/// 摘要时至少保留的最近消息条数（原文，不压）。
///
/// 为什么必须有下界：最近几条是模型最可能直接依赖的（当前话题、刚做的决定），
/// 把它们也压成摘要会让模型"接不上话"。保守取 6（约 3 轮问答）。
pub const COMPACT_KEEP_RECENT: usize = 6;

/// 打分时进入"原文附录"的条数上限（高分者以原文形式保留在摘要里）。
const COMPACT_QUOTE_TOP: usize = 5;

/// 触发 compact 的水位（占上下文预算的比例）。
///
/// 为什么低于 agent loop 的 0.75：compact 本身要发一次模型请求（摘要），
/// 留出余量避免"刚压缩完又超"。0.6 是"有压力但还宽裕"的位置。
pub const COMPACT_TRIGGER_RATIO: f64 = 0.6;

/// 单条消息的打分：分越高越值得保留原文。
///
/// 学 WorkBuddy 的 `calculateItemScore`，按本项目场景裁剪：
/// - 位置分：越新越高（`index/len * 50`）
/// - 含错误/重要等关键词：每个 +15
/// - 内容长：说明信息量大，`min(len/100, 20)`
/// - 工具调用记录：+30（"做过什么"是任务连续性的关键）
fn score_message(m: &StoredMessage, index: usize, total: usize) -> i64 {
    let mut s = 0i64;
    if total > 0 {
        s += (index as i64) * 50 / total as i64;
    }
    if !m.steps.is_empty() {
        s += 30;
    }
    let len = m.text.chars().count();
    if len > 100 {
        s += ((len / 100).min(20)) as i64;
    }
    let low = m.text.to_lowercase();
    for k in [
        "错误", "失败", "error", "重要", "注意", "警告", "warning",
        "决定", "约定", "bug", "修复", "不能", "必须",
    ] {
        if low.contains(k) {
            s += 15;
        }
    }
    s
}

/// 把一段消息压成纯文本记录（喂给摘要模型）。
fn render_transcript(msgs: &[&StoredMessage]) -> String {
    let mut out = String::new();
    for m in msgs {
        let who = if m.role == "user" { "用户" } else { "助手" };
        // 单条截到 2000 字：摘要看的是"发生了什么"，不需要全文
        let text: String = m.text.chars().take(2000).collect();
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("[{who}] {text}\n"));
    }
    out
}

/// 调模型生成摘要（失败返回 Err，调用方决定是否降级）。
async fn generate_summary(cfg: &ModelConfig, msgs: &[&StoredMessage]) -> Result<String, String> {
    let transcript = render_transcript(msgs);
    if transcript.trim().is_empty() {
        return Err("待压缩区间没有有效文本".into());
    }
    let prompt = format!(
        "请把下面这段「AI 助手与用户的历史对话」压缩成一份结构化摘要，\
供后续轮次恢复上下文使用。\n\n\
要求：\n\
1. 保留：用户的目标与要求、已达成的决定与约定、改过/看过哪些文件、\
遇到的错误与解决办法、未完成的待办。\n\
2. 省略：寒暄、重复确认、工具的原始输出全文。\n\
3. 用简洁的中文分条列出，不要写成「用户说/助手说」这类流水话术。\n\
4. 只输出摘要正文，不要加开场白或结尾客套。\n\n\
--- 对话记录开始 ---\n{transcript}\n--- 对话记录结束 ---"
    );

    let messages = vec![ChatMessage::user(prompt)];
    let out = crate::llm::chat(cfg, messages, None).await?;
    let text = out.text();
    if text.trim().is_empty() {
        return Err("模型返回了空摘要".into());
    }
    Ok(text.trim().to_string())
}

/// compact 的结果摘要（供日志/前端提示）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactOutcome {
    /// 本次被摘要覆盖的消息数
    pub summarized: usize,
    /// 保留原文的消息数
    pub kept: usize,
    /// 摘要字数
    pub summary_chars: usize,
}

/// 对会话做一次 compact：把较早的消息压成摘要写回会话文件。
///
/// ## 为什么必须是**持久化**而不是读取时现压
/// prompt cache 按 token 前缀逐字节比对。摘要若每轮重新生成，模型输出的
/// 随机性会让前缀抖动，其后全部 token 按未命中价重付 —— 每一轮都多花钱。
/// 落盘一次 = 写进去什么样，后面读出来就什么样，天然稳定。
///
/// ## 流程
/// 1. 取"未摘要区间" `messages[start..]`（`start` = 上次的 `summary_upto`）
/// 2. 只压其中较早的部分，尾部 [`COMPACT_KEEP_RECENT`] 条保留原文
/// 3. 对**压缩区**打分，高分者以原文附录形式附在摘要后（尽量少丢关键信息）
/// 4. 生成摘要 + 写入 `summary` / `summary_upto` 并落盘
pub async fn compact(
    cfg: &ModelConfig,
    data_dir: &std::path::Path,
    session_id: &str,
    keep_recent: usize,
    force: bool,
    now_ms: u64,
) -> Result<CompactOutcome, String> {
    let mut s = sessions::load(data_dir, session_id)
        .ok_or_else(|| format!("会话 {session_id} 不存在"))?;

    let start = s.summary_upto.unwrap_or(0).min(s.messages.len());
    let pending = &s.messages[start..];
    if pending.len() <= keep_recent {
        return Err(format!(
            "消息太少（{} 条），无需压缩（保留线 {} 条）",
            pending.len(),
            keep_recent
        ));
    }

    // 触发水位检查（force 时跳过）
    if !force {
        // 用模型窗口估算：超 COMPACT_TRIGGER_RATIO 才真压
        let window = cfg
            .max_input_tokens
            .map(|n| n as usize)
            .unwrap_or(128 * 1024);
        let budget = ((window as f64) * COMPACT_TRIGGER_RATIO) as usize;
        let est: usize = pending
            .iter()
            .map(|m| crate::llm::estimate_tokens(&m.text) + 4)
            .sum();
        if est <= budget {
            return Err(format!("未达压缩水位（估算 {est} ≤ {budget}），跳过"));
        }
    }

    let split = pending.len() - keep_recent;
    let region: Vec<&StoredMessage> = pending[..split].iter().collect();

    // 打分挑出高分者 → 原文附录（尽量别把关键信息压没）
    let total = region.len();
    let mut scored: Vec<(usize, i64)> = region
        .iter()
        .enumerate()
        .map(|(i, m)| (i, score_message(m, i, total)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    let mut top: Vec<usize> = scored
        .iter()
        .take(COMPACT_QUOTE_TOP)
        .map(|(i, _)| *i)
        .collect();
    top.sort_unstable(); // 恢复时间顺序

    let mut summary = generate_summary(cfg, &region).await?;
    if !top.is_empty() {
        summary.push_str("\n\n【重要历史片段（原文保留）】\n");
        for i in top {
            let m = region[i];
            let who = if m.role == "user" { "用户" } else { "助手" };
            let text: String = m.text.chars().take(300).collect();
            summary.push_str(&format!("- [{who}] {text}\n"));
        }
    }

    let summary_chars = summary.chars().count();
    s.summary = Some(summary);
    s.summary_upto = Some(start + split);
    s.updated_at = now_ms;
    sessions::save(data_dir, &s)?;

    eprintln!(
        "[orbcat] compact 完成：摘要 {} 条消息 → {} 字，保留原文 {} 条",
        split, summary_chars, keep_recent
    );
    Ok(CompactOutcome {
        summarized: split,
        kept: keep_recent,
        summary_chars,
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::StoredStep;

    fn msg(role: &str, text: &str, at: u64) -> StoredMessage {
        StoredMessage {
            role: role.into(),
            text: text.into(),
            images: Vec::new(),
            steps: Vec::new(),
            reasoning: None,
            interrupted: false,
            partial: false,
            steer_id: None,
            model: None,
            usage: None,
            at,
        }
    }

    const HOUR: u64 = 60 * 60 * 1000;

    #[test]
    fn empty_history_yields_nothing() {
        assert!(pick(&[], 100 * HOUR).is_empty());
    }

    #[test]
    fn recent_messages_all_included() {
        let now = 100 * HOUR;
        let msgs = vec![
            msg("user", "a", now - HOUR),
            msg("assistant", "b", now - HOUR + 1000),
        ];
        let picked = pick(&msgs, now);
        assert_eq!(picked.len(), 2, "1 小时前的内容在窗口内，应全带");
        assert!(picked.iter().all(|p| !p.fallback));
    }

    #[test]
    fn window_excludes_old_but_fallback_keeps_recent_n() {
        let now = 100 * HOUR;
        // 20 条很老的消息（隔夜），时间窗全挡掉
        let mut msgs: Vec<StoredMessage> = (0..20)
            .map(|i| msg("user", &format!("老{i}"), now - 20 * HOUR + i))
            .collect();
        // 一条刚刚的
        msgs.push(msg("user", "刚说的", now));

        let picked = pick(&msgs, now);
        // 时间窗带 1 条（刚说的），兜底带最后 10 条
        assert_eq!(picked.len(), FALLBACK_TURNS, "应为兜底条数 FALLBACK_TURNS");
        assert_eq!(picked.last().unwrap().msg.text, "刚说的");
        // 靠兜底进来的应被标记
        assert!(picked[0].fallback, "时间窗外的应标记 fallback");
    }

    #[test]
    fn overnight_window_union_does_not_lose_current_topic() {
        // 用户场景：昨天 22:00-23:50 聊，今天 15:00 回来
        let now = 100 * HOUR;
        let mut msgs: Vec<StoredMessage> = (0..6)
            .map(|i| msg("user", &format!("昨天{i}"), now - 17 * HOUR + i))
            .collect();
        msgs.push(msg("assistant", "今天刚问的", now - 60_000));

        let picked = pick(&msgs, now);
        assert!(
            picked.iter().any(|p| p.msg.text == "今天刚问的"),
            "必须带上今天的消息，否则话题断片"
        );
        assert!(
            picked.iter().any(|p| p.msg.text.starts_with("昨天")),
            "兜底应把隔夜的话题拉回来"
        );
    }

    /// 折叠后的 steps 直接读出来就用，不再二次加工
    #[test]
    fn folded_steps_are_read_verbatim() {
        let mut m = msg("assistant", "查了", 100);
        m.steps = vec![StoredStep {
            kind: "tool_call".into(),
            name: Some("read_file".into()),
            detail: "[已执行：read_file、grep_files（共 2 次工具调用）]".into(),
        }];
        assert!(is_already_folded_line(&m.steps), "这形状就是折叠版");

        let cm = to_chat_message(Picked { msg: m, fallback: false });
        let json = serde_json::to_string(&cm).unwrap();
        assert!(
            json.contains("[已执行：read_file、grep_files（共 2 次工具调用）]"),
            "折叠行应原样读出: {json}"
        );
    }

    /// P0：结构化 steps 的 tool_result 必须进下一轮（不再被 fold 成一行）
    #[test]
    fn structured_steps_keep_recent_tool_results() {
        let mut m = msg("assistant", "查了", 100);
        m.steps = vec![
            StoredStep {
                kind: "tool_call".into(),
                name: Some("read_file".into()),
                detail: r#"{"path":"D:\\a.md"}"#.into(),
            },
            StoredStep {
                kind: "tool_result".into(),
                name: Some("read_file".into()),
                detail: "FILE_BODY_MARKER\nline2".into(),
            },
            StoredStep {
                kind: "tool_call".into(),
                name: Some("grep_files".into()),
                detail: r#"{"pattern":"TODO"}"#.into(),
            },
            StoredStep {
                kind: "tool_result".into(),
                name: Some("grep_files".into()),
                detail: "TODO_HIT_MARKER".into(),
            },
        ];
        let cm = to_chat_message(Picked { msg: m, fallback: false });
        let json = serde_json::to_string(&cm).unwrap();
        assert!(
            json.contains("FILE_BODY_MARKER"),
            "近条 tool_result 必须回灌: {json}"
        );
        assert!(
            json.contains("TODO_HIT_MARKER"),
            "近条 tool_result 必须回灌: {json}"
        );
        assert!(
            !json.contains("2 次工具调用"),
            "不应再被压成一行摘要: {json}"
        );
    }

    /// 更旧的 steps 折成一行摘要，近条仍保留
    #[test]
    fn older_steps_fold_to_summary_line() {
        let mut steps = Vec::new();
        for i in 0..20 {
            steps.push(StoredStep {
                kind: "tool_call".into(),
                name: Some(format!("t{i}")),
                detail: format!("call-{i}"),
            });
            steps.push(StoredStep {
                kind: "tool_result".into(),
                name: Some(format!("t{i}")),
                detail: format!("RESULT_{i}_MARKER"),
            });
        }
        let line = fold_steps_to_line(&steps);
        assert!(
            line.contains("更早 steps 已折叠"),
            "超 K 条应有折叠摘要: {line}"
        );
        assert!(
            line.contains("RESULT_19_MARKER") || line.contains("RESULT_18_MARKER"),
            "近条结果应保留: {line}"
        );
        assert!(
            !line.contains("RESULT_0_MARKER"),
            "很旧的结果应被折叠掉: {line}"
        );
    }

    /// 只有 thought、没有工具调用的 steps → 不产生折叠行
    #[test]
    fn thought_only_steps_produce_no_line() {
        let mut m = msg("assistant", "我想了想", 100);
        m.steps = vec![StoredStep {
            kind: "thought".into(),
            name: None,
            detail: "思考内容".into(),
        }];
        let cm = to_chat_message(Picked { msg: m, fallback: false });
        let json = serde_json::to_string(&cm).unwrap();
        assert!(!json.contains("已执行"), "无工具调用不应有折叠行: {json}");
        assert!(json.contains("我想了想"));
    }

    #[test]
    fn images_become_path_note_not_base64() {
        let mut m = msg("user", "看这个", 100);
        m.images = vec!["D:\\x\\a.png".into(), "D:\\x\\b.png".into()];
        let p = Picked { msg: m, fallback: false };
        let cm = to_chat_message(p);

        // ChatMessage 的 content 是 MessageContent，序列化出来看
        let json = serde_json::to_string(&cm).unwrap();
        assert!(json.contains("2 张图片"), "应说明图片数量: {json}");
        assert!(json.contains("a.png"), "应保留路径");
        assert!(!json.contains("base64"), "绝不能带 base64 本体");
    }

    #[test]
    fn search_finds_keyword_in_current_session() {
        let d = std::env::temp_dir().join("orbcat_history_search");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // 造一个会话：一条聊 venera，一条聊 apidash
        let s = sessions::new_session(&d);
        sessions::append_turn(&d, "改一下 venera 的跳页 bug", &[], "好的", &[]).unwrap();
        sessions::append_turn(&d, "另外 apidash 的构建也看看", &[], "行", &[]).unwrap();

        // 关键词命中
        let hits = search(&d, &s.id, "venera", 10, None, now);
        assert_eq!(hits.len(), 1, "应只命中 venera 那条");
        assert!(hits[0].text.contains("venera"));
        assert_eq!(hits[0].role, "user");
        assert!(!hits[0].when.is_empty(), "应带人类可读时间");

        // 大小写不敏感
        let hits = search(&d, &s.id, "APIDASH", 10, None, now);
        assert_eq!(hits.len(), 1, "关键词匹配应大小写不敏感");

        // 空 query → 返回最近 N 条
        // 注意：append_turn 一次追加 user + assistant **两条**，
        // 聊两轮 = 4 条消息：[u1, a1, u2, a2]
        let hits = search(&d, &s.id, "", 4, None, now);
        assert_eq!(hits.len(), 4, "两轮对话应有 4 条消息");
        assert_eq!(hits[2].text, "另外 apidash 的构建也看看");

        // limit 生效：截断时丢掉**最远的**，保留最近的
        let hits = search(&d, &s.id, "", 2, None, now);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "另外 apidash 的构建也看看");
        assert_eq!(hits[1].text, "行", "最后一条应是最后那轮的助手回复");

        // hours 窗口：近 3 小时内的记录应全在
        let hits = search(&d, &s.id, "", 10, Some(3), now);
        assert_eq!(hits.len(), 4);

        // 会话不存在 → 空
        assert!(search(&d, "no-such-id", "x", 10, None, now).is_empty());

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn build_history_returns_usable_chat_messages() {
        let d = std::env::temp_dir().join("orbcat_history_build");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let s = sessions::new_session(&d);
        sessions::append_turn(&d, "第一问", &[], "第一答", &[]).unwrap();

        let msgs = build_history(&d, &s.id, now);
        assert_eq!(msgs.len(), 2, "一问一答应回灌 2 条");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");

        // 不存在的会话 → 空（不能 panic）
        assert!(build_history(&d, "nope", now).is_empty());

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fmt_local_handles_known_timestamp() {
        // 1789950480000 ms = 2026-09-21 00:28 UTC = 2026-09-21 08:28 本地（UTC+8）
        // （该值用 PowerShell `[DateTimeOffset]::FromUnixTimeMilliseconds` 核对过，
        //  别手算 —— 手算极易错年份）
        let s = fmt_local(1_789_950_480_000);
        assert_eq!(s, "2026-09-21 08:28", "本地时间格式化不对");
    }

    #[test]
    fn fmt_local_epoch_zero_is_utc_plus_8() {
        // 0 ms = 1970-01-01 00:00 UTC = 1970-01-01 08:00 本地
        assert_eq!(fmt_local(0), "1970-01-01 08:00");
    }

    // ---- compact 相关 ----

    #[test]
    fn score_prefers_newer_and_error_keywords() {
        let old = msg("user", "普通内容", 100);
        let new = msg("user", "普通内容", 200);
        assert!(
            score_message(&new, 9, 10) > score_message(&old, 0, 10),
            "越新的消息分数应更高（位置分）"
        );

        let with_err = msg("assistant", "这里报错了：连接失败", 100);
        let plain = msg("assistant", "一切正常", 100);
        assert!(
            score_message(&with_err, 5, 10) > score_message(&plain, 5, 10),
            "含错误关键词的消息应更高分"
        );
    }

    #[test]
    fn score_rewards_tool_usage() {
        let mut with_tools = msg("assistant", "查了", 100);
        with_tools.steps = vec![StoredStep {
            kind: "tool_call".into(),
            name: Some("read_file".into()),
            detail: "[已执行：read_file（共 1 次工具调用）]".into(),
        }];
        let without = msg("assistant", "查了", 100);
        assert!(
            score_message(&with_tools, 5, 10) > score_message(&without, 5, 10),
            "有工具调用记录的消息应更高分"
        );
    }

    /// 摘要生效：`summary_upto` 之前的消息不再逐条回灌，且摘要插在最前面
    #[test]
    fn summary_replaces_covered_messages() {
        let d = std::env::temp_dir().join("orbcat_history_summary");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let s = sessions::new_session(&d);
        sessions::append_turn(&d, "第一问", &[], "第一答", &[]).unwrap();
        sessions::append_turn(&d, "第二问", &[], "第二答", &[]).unwrap();

        // 手工写回带摘要的会话：前 2 条（第一问/第一答）被摘要覆盖
        let mut sess = sessions::load(&d, &s.id).unwrap();
        sess.summary = Some("用户先问了第一件事，已答复。".into());
        sess.summary_upto = Some(2);
        sessions::save(&d, &sess).unwrap();

        let msgs = build_history(&d, &s.id, now);
        // 期望：1 条摘要（system）+ 摘要之后的消息（第二问/第二答，若在窗口内）
        let first = serde_json::to_string(&msgs[0]).unwrap();
        assert!(first.contains("summary") || first.contains("Summary"), "首条应是摘要: {first}");
        assert!(first.contains("第一件事"), "摘要正文应带上: {first}");

        let all = serde_json::to_string(&msgs).unwrap();
        assert!(!all.contains("第一问"), "被摘要覆盖的消息不应再逐条出现");
        assert!(all.contains("第二问"), "摘要之后的消息应正常回灌");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 截断落进摘要区 → 摘要必须失效（否则回灌一段对不上的上下文）
    #[test]
    fn truncate_inside_summary_invalidates_it() {
        let d = std::env::temp_dir().join("orbcat_trunc_summary");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let s = sessions::new_session(&d);
        sessions::append_turn(&d, "q1", &[], "a1", &[]).unwrap();
        sessions::append_turn(&d, "q2", &[], "a2", &[]).unwrap();

        let mut sess = sessions::load(&d, &s.id).unwrap();
        sess.summary = Some("旧摘要".into());
        sess.summary_upto = Some(3);
        sessions::save(&d, &sess).unwrap();

        // 截到下标 1（只剩 1 条）→ 落在摘要覆盖区 [0,3) 内 → 摘要失效
        let after = sessions::truncate(&d, &s.id, 1).unwrap();
        assert!(after.summary.is_none(), "截断后摘要应被清空");
        assert!(after.summary_upto.is_none(), "summary_upto 应被清空");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 截断发生在摘要区**之后** → 摘要仍然有效
    #[test]
    fn truncate_after_summary_keeps_it() {
        let d = std::env::temp_dir().join("orbcat_trunc_keep");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let s = sessions::new_session(&d);
        sessions::append_turn(&d, "q1", &[], "a1", &[]).unwrap();
        sessions::append_turn(&d, "q2", &[], "a2", &[]).unwrap();

        let mut sess = sessions::load(&d, &s.id).unwrap();
        sess.summary = Some("摘要".into());
        sess.summary_upto = Some(2); // 只覆盖前 2 条
        sessions::save(&d, &sess).unwrap();

        // 截到下标 3（保留 3 条）→ 摘要区 [0,2) 完好 → 摘要保留
        let after = sessions::truncate(&d, &s.id, 3).unwrap();
        assert_eq!(after.summary.as_deref(), Some("摘要"), "摘要区未被触及，应保留");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn token_estimate_cjk_vs_ascii() {
        // 4 个 ASCII 字符 ≈ 1 token；CJK 1 字 ≈ 1 token
        assert_eq!(crate::llm::estimate_tokens("abcd"), 1);
        assert_eq!(crate::llm::estimate_tokens("中文四个字"), 5);
        // 空串不 panic
        assert_eq!(crate::llm::estimate_tokens(""), 0);
    }
}
