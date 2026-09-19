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

use crate::llm::ChatMessage;
use crate::sessions::{self, StoredMessage};

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
pub fn build_history(data_dir: &std::path::Path, session_id: &str, now_ms: u64) -> Vec<ChatMessage> {
    let Some(s) = sessions::load(data_dir, session_id) else {
        return Vec::new();
    };
    let picked = pick(&s.messages, now_ms);
    picked.into_iter().map(to_chat_message).collect()
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

    // 工具调用结果：**落盘时就已折叠**（见 `sessions::append_turn`），
    // 这里直接拼，不再加工 —— 每轮读到的历史必须字节稳定，
    // 任何"读取时加工"都会让 prompt cache 前缀失效。
    //
    // 唯一的例外是**存量老数据**：折叠机制上线前落盘的 steps 是原文
    // （一条 detail 可能几 KB）。这些消息在本机读出来仍是原文，
    // 会对缓存不友好。但老数据的折叠结果一旦写回磁盘就定死，
    // 反而会被"重折一次"改掉 —— 所以这里**读时兜底折叠**，
    // 只对明显是原文的（detail 很长）动手，且不写回磁盘：
    // 老消息会随 400 条上限自然淘汰，不影响新数据的稳定性。
    let steps = if is_legacy_unfolded(&m.steps) {
        crate::sessions::fold_steps(&m.steps)
    } else {
        m.steps.clone()
    };

    let folded = fold_steps_to_line(&steps);
    if !folded.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&folded);
    }

    match m.role.as_str() {
        "assistant" => ChatMessage::assistant(text),
        _ => ChatMessage::user(text),
    }
}

/// 判断这组 steps 是否是**折叠前**的存量数据。
///
/// 折叠后的记录形状固定：只有 1 条，`kind == "tool_call"`，且 `detail`
/// 以 `[已执行` 开头。除此之外都是原文（多条记录、或含 `tool_result`、
/// 或 detail 是真实输出）。
///
/// 依赖 [`crate::sessions::fold_steps`] 的**幂等性**：折叠结果再过一遍
/// 折不出别的样子，所以这个判定不会把新数据误判成老数据。
fn is_legacy_unfolded(steps: &[sessions::StoredStep]) -> bool {
    if steps.is_empty() {
        return false;
    }
    if steps.len() != 1 {
        return true;
    }
    let s = &steps[0];
    s.kind != "tool_call" || !s.detail.trim_start().starts_with("[已执行")
}

/// 已折叠的 steps → 拼进历史文本的那一行
fn fold_steps_to_line(steps: &[sessions::StoredStep]) -> String {
    steps
        .iter()
        .find(|s| s.kind == "tool_call")
        .map(|s| s.detail.clone())
        .unwrap_or_default()
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
        assert_eq!(picked.len(), 10, "应为兜底的 10 条");
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
        assert!(!is_legacy_unfolded(&m.steps), "这形状就是折叠版");

        let cm = to_chat_message(Picked { msg: m, fallback: false });
        let json = serde_json::to_string(&cm).unwrap();
        assert!(
            json.contains("[已执行：read_file、grep_files（共 2 次工具调用）]"),
            "折叠行应原样读出: {json}"
        );
    }

    /// 存量老数据（折叠机制上线前的原文 steps）读时兜底折叠
    #[test]
    fn legacy_unfolded_steps_are_folded_on_read() {
        let mut m = msg("assistant", "查了", 100);
        m.steps = vec![
            StoredStep {
                kind: "tool_call".into(),
                name: Some("read_file".into()),
                detail: "x".repeat(5000),
            },
            StoredStep {
                kind: "tool_result".into(),
                name: Some("read_file".into()),
                detail: "y".repeat(5000),
            },
            StoredStep {
                kind: "tool_call".into(),
                name: Some("grep_files".into()),
                detail: "z".repeat(5000),
            },
        ];
        let cm = to_chat_message(Picked { msg: m, fallback: false });
        let json = serde_json::to_string(&cm).unwrap();
        assert!(json.contains("2 次工具调用"), "老数据应被兜底折叠: {json}");
        assert!(!json.contains("xxxx"), "原文不该出现在回灌内容里");
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
        let d = std::env::temp_dir().join("float_agent_history_search");
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
        let d = std::env::temp_dir().join("float_agent_history_build");
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
}
