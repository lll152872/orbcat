//! 会话持久化 —— 面板里的对话历史，重启后还在
//!
//! ## 为什么单独一份，不靠日记
//! `daily/YYYY-MM-DD.md` 是**给人读的流水账**（一次对话一行摘要），
//! 而会话要的是**能原样恢复的对话**：谁说了什么、模型调了哪些工具、
//! 附了哪张图。两者用途不同，各自落盘最省心。
//!
//! ## 布局
//! ```text
//!   agent-data/sessions/
//!     current.txt        ← 当前会话 id（一行文本）
//!     <id>.json          ← 一个会话的全部消息
//! ```
//! 单会话消息数有上限（超出从头截断），避免文件无限长大。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 单个会话最多保留的消息条数（超出截掉最旧的）
///
/// 主会话（`kind == "main"`）**同样适用**：它是问答场景，400 条足够，
/// 且能避免单文件无限膨胀（当前是全量 JSON 读写，不是 JSONL 追加）。
/// 「丢掉 400 条之前的内容」是已确认可接受的行为。
const MAX_MESSAGES: usize = 400;

/// 折叠后单条工具结果的展示上限（字符）
const FOLDED_DETAIL_LIMIT: usize = 120;

/// 主会话的固定标题。主会话不可删、不参与「首条消息改名」，
/// 名字必须一眼能认出，否则它在列表里和普通任务会话无从区分。
const MAIN_TITLE: &str = "主聊天";

/// 会话类型。整库**有且仅有一条** `Main`，其余都是 `Task`。
pub mod kind {
    pub const MAIN: &str = "main";
    pub const TASK: &str = "task";
}

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredStep {
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    /// `user` | `assistant`
    pub role: String,
    #[serde(default)]
    pub text: String,
    /// **图片文件路径**（不是 base64）。图片本体在 `agent-data/images/`，
    /// 发送时按模型能力决定转 base64 还是只发路径占位，见 `images.rs`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// assistant 消息的执行步骤
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StoredStep>,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    /// 标题 = 首条用户消息的前若干字（列表里好认）。
    /// 主会话固定为 [`MAIN_TITLE`]，不参与自动改名。
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<StoredMessage>,
    /// `main` | `task`，见 [`kind`]。老数据没有这个字段 → 默认 `task`，
    /// 由 [`unique_main`] 在首次加载时自愈出一条 main，无需迁移脚本。
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    kind::TASK.to_string()
}

impl Session {
    pub fn is_main(&self) -> bool {
        self.kind == kind::MAIN
    }
}

/// 列表用的轻量元信息（不带消息体）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub count: usize,
    pub current: bool,
    pub kind: String,
}

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

fn current_file(data_dir: &Path) -> PathBuf {
    sessions_dir(data_dir).join("current.txt")
}

fn session_file(data_dir: &Path, id: &str) -> PathBuf {
    sessions_dir(data_dir).join(format!("{id}.json"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 新会话 id：时间戳 + 进程内计数，避免同毫秒碰撞
fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("s{}-{}", now_ms(), n)
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

fn read_session(path: &Path) -> Option<Session> {
    let txt = std::fs::read_to_string(path).ok()?;
    let mut s = serde_json::from_str::<Session>(&txt).ok()?;
    sanitize_legacy_images(&mut s);
    Some(s)
}

/// 清掉旧格式里内联的 base64 图片。
///
/// 早先图片以 data URL 直接存进会话（一张截图 ≈ 270KB 文本，几十张就是几十 MB）。
/// 图片现在改为外置存路径，旧的内联图**用户已确认无用**，直接丢掉（消息文本保留），
/// 免得每次读会话都要解析几十 MB 的 base64。
fn sanitize_legacy_images(s: &mut Session) {
    for m in &mut s.messages {
        m.images.retain(|img| !img.starts_with("data:"));
    }
}

fn write_session(data_dir: &Path, s: &Session) -> Result<(), String> {
    let dir = sessions_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建会话目录失败: {e}"))?;
    let path = session_file(data_dir, &s.id);
    let txt = serde_json::to_string(s).map_err(|e| format!("序列化会话失败: {e}"))?;
    std::fs::write(&path, txt).map_err(|e| format!("写会话失败: {e}"))
}

/// 当前会话 id（没记录过返回 None）
pub fn current_id(data_dir: &Path) -> Option<String> {
    let v = std::fs::read_to_string(current_file(data_dir)).ok()?;
    let id = v.trim().to_string();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn set_current(data_dir: &Path, id: &str) {
    let dir = sessions_dir(data_dir);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(current_file(data_dir), id);
}

/// 建一个**任务会话**并设为当前。
///
/// 主会话不由这里创建 —— 它由 [`unique_main`] 保证存在，全库唯一。
pub fn new_session(data_dir: &Path) -> Session {
    let now = now_ms();
    let s = Session {
        id: new_id(),
        title: "新会话".into(),
        created_at: now,
        updated_at: now,
        messages: Vec::new(),
        kind: kind::TASK.into(),
    };
    let _ = write_session(data_dir, &s);
    set_current(data_dir, &s.id);
    s
}

/// 保证全库**有且仅有一条**主会话，返回它。
///
/// 三种情况：
/// - 已有唯一 main → 直接返回（顺手纠正标题）
/// - 有多个 main（异常数据）→ 保留 `created_at` 最小的，其余降级为 task
/// - 一条 main 都没有（含全部老数据）→ 把 `created_at` 最小的会话升为 main；
///   库是空的则新建一条
///
/// 主会话标题**强制**为 [`MAIN_TITLE`]（用户已确认：现有会话升上来时覆盖原名）。
pub fn unique_main(data_dir: &Path) -> Session {
    let Ok(entries) = std::fs::read_dir(sessions_dir(data_dir)) else {
        return create_main(data_dir);
    };

    let mut all: Vec<Session> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|p| read_session(&p))
        .collect();

    if all.is_empty() {
        return create_main(data_dir);
    }

    all.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    // 选定主会话：优先保留**已有的** main 里 created_at 最小的那条；
    // 一条 main 都没有 → 取 created_at 最小的会话（最早的那条 = 关系的起点）。
    // 索引在 `all` 上算好，之后统一按索引判定，逻辑单一不会互相干扰。
    let main_idx = all
        .iter()
        .position(|s| s.is_main())
        .unwrap_or(0);
    let main_id = all[main_idx].id.clone();

    for s in all.iter_mut() {
        let want_kind = if s.id == main_id {
            kind::MAIN
        } else {
            kind::TASK
        };
        let kind_changed = s.kind != want_kind;
        let title_changed = want_kind == kind::MAIN && s.title != MAIN_TITLE;
        if kind_changed || title_changed {
            s.kind = want_kind.to_string();
            if want_kind == kind::MAIN {
                s.title = MAIN_TITLE.to_string();
            }
            let _ = write_session(data_dir, s);
        }
    }

    all.into_iter()
        .find(|s| s.id == main_id)
        .unwrap_or_else(|| create_main(data_dir))
}

fn create_main(data_dir: &Path) -> Session {
    let now = now_ms();
    let s = Session {
        id: new_id(),
        title: MAIN_TITLE.into(),
        created_at: now,
        updated_at: now,
        messages: Vec::new(),
        kind: kind::MAIN.into(),
    };
    let _ = write_session(data_dir, &s);
    s
}

/// 取当前会话；没有/文件丢了就建一个
pub fn ensure_current(data_dir: &Path) -> Session {
    // 先保证主会话存在（首次加载会自愈），顺带拿到它作为兜底
    let main = unique_main(data_dir);
    if let Some(id) = current_id(data_dir) {
        if let Some(s) = read_session(&session_file(data_dir, &id)) {
            return s;
        }
    }
    // 兼容：current.txt 丢了但有历史会话 → 直接捡最近一个，而不是新开
    if let Some(latest) = list(data_dir).into_iter().next() {
        set_current(data_dir, &latest.id);
        if let Some(s) = read_session(&session_file(data_dir, &latest.id)) {
            return s;
        }
    }
    set_current(data_dir, &main.id);
    main
}

pub fn load(data_dir: &Path, id: &str) -> Option<Session> {
    read_session(&session_file(data_dir, id))
}

/// 会话列表（**主会话永远置顶**，其余按最近更新倒序）
pub fn list(data_dir: &Path) -> Vec<SessionMeta> {
    let cur = current_id(data_dir).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(sessions_dir(data_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<SessionMeta> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(s) = read_session(&p) else { continue };
        out.push(SessionMeta {
            current: s.id == cur,
            id: s.id,
            title: s.title,
            created_at: s.created_at,
            updated_at: s.updated_at,
            count: s.messages.len(),
            kind: s.kind,
        });
    }
    // 主会话置顶；同一层内按 updated_at 倒序。
    // 这是「主副结构」能被一眼看出来的关键 —— 否则主会话会沉到列表底下。
    out.sort_by(|a, b| {
        let a_main = a.kind == kind::MAIN;
        let b_main = b.kind == kind::MAIN;
        b_main
            .cmp(&a_main)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    out
}

/// 切到某个会话（不存在返回 Err）
pub fn switch(data_dir: &Path, id: &str) -> Result<Session, String> {
    let s = load(data_dir, id).ok_or_else(|| format!("会话 {id} 不存在"))?;
    set_current(data_dir, id);
    Ok(s)
}

/// 删除一个会话。
///
/// **主会话拒绝删除** —— 它「永久且不可删」是主副结构的前提，
/// 允许多个/允许删就退化成普通列表。这是后端硬保护，前端不渲染 ✕ 只是第一道。
/// 删掉的若是当前会话，自动切到最近一个（没有就新建）
pub fn delete(data_dir: &Path, id: &str) -> Result<(), String> {
    let path = session_file(data_dir, id);
    if !path.exists() {
        return Err(format!("会话 {id} 不存在"));
    }
    if let Some(s) = read_session(&path) {
        if s.is_main() {
            return Err("主聊天不可删除".into());
        }
    }
    std::fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))?;
    if current_id(data_dir).as_deref() == Some(id) {
        let _ = std::fs::remove_file(current_file(data_dir));
        let _ = ensure_current(data_dir);
    }
    Ok(())
}

/// 往当前会话追加一轮对话（用户消息 + 助手回答）。
///
/// ## 工具结果在这里就折叠掉（用户 2026-09-19 定案）
/// assistant 消息的 `steps` 体积最大（一轮 grep 可能几 KB），落盘前就压成
/// 一行摘要。为什么放在**落盘**而不是每轮组装时折：
///
/// - prompt cache 按 token 前缀逐字节比对，**每轮读到的历史必须字节稳定**。
///   若在 `history.rs` 里每轮现折，折叠逻辑一改（或 `harsh` 判定随顺序变化），
///   已发出的历史就会"变形"，其后全部 token 按未命中价重付。
/// - 落盘一次 → 写进去什么样，后面每轮读出来就什么样，天然稳定。
/// - 折叠是**纯函数**（只依赖 steps 本身），不依赖"现在几点"，所以不会
///   每次读盘得到不同结果。
///
/// 代价：原文不再保留，模型只能靠重调工具拿细节。但历史本就只回灌
/// 最近 6h ∪ 最近 10 条（见 `history.rs`），窗口外的细节本来也带不上，
/// 丢掉不影响实际能力。**`recall_turns` 工具查的是折叠后的文本**
/// ——这是刻意的：查历史是为了找回"聊过什么"，不是回放工具输出。
pub fn append_turn(
    data_dir: &Path,
    user_text: &str,
    images: &[String],
    answer: &str,
    steps: &[StoredStep],
) -> Result<(), String> {
    let mut s = ensure_current(data_dir);
    let now = now_ms();

    s.messages.push(StoredMessage {
        role: "user".into(),
        text: user_text.to_string(),
        images: images.to_vec(),
        steps: Vec::new(),
        at: now,
    });
    s.messages.push(StoredMessage {
        role: "assistant".into(),
        text: answer.to_string(),
        images: Vec::new(),
        steps: fold_steps(steps),
        at: now,
    });

    // 标题：首次有用户消息时定下来。
    // **主会话不参与** —— 它的名字固定为「主聊天」，被首条消息劫持后就认不出来了。
    if !s.is_main() && s.title == "新会话" {
        let t: String = user_text.trim().chars().take(24).collect();
        if !t.is_empty() {
            s.title = t;
        }
    }

    if s.messages.len() > MAX_MESSAGES {
        let cut = s.messages.len() - MAX_MESSAGES;
        // 主会话超限是**已确认可接受**的丢弃（问答场景，用户记不住 400 条之前的内容）。
        // 留一条 stderr 痕迹，免得以后误判成 bug。
        if s.is_main() {
            eprintln!(
                "[sessions] 主会话超过 {MAX_MESSAGES} 条，丢弃最旧的 {cut} 条（预期行为）"
            );
        }
        s.messages.drain(..cut);
    }
    s.updated_at = now;
    write_session(data_dir, &s)
}

/// 把 Rust 侧 AgentStep 转成可存储的步骤
pub fn steps_from_agent(steps: &[crate::agent::AgentStep]) -> Vec<StoredStep> {
    steps
        .iter()
        .map(|s| StoredStep {
            kind: s.kind.clone(),
            name: s.name.clone(),
            detail: s.detail.clone(),
        })
        .collect()
}

/// 把 assistant 消息的 steps 压成一条摘要（历史回灌用）。
///
/// 输出形如 `[已执行：read_file、grep_files（共 2 次工具调用）]`。
/// **保留原始 `kind` 字段**（`tool_call`），`detail` 换成摘要 ——
/// 这样下游看到的是"一条 steps 记录"，不必区分折叠前后。
///
/// 全部非 `tool_call`（例如只有 `thought`）→ 返回空 vec：这些步骤
/// 在历史上没有回灌价值，模型正文里已经说了它想了什么。
pub fn fold_steps(steps: &[StoredStep]) -> Vec<StoredStep> {
    let calls: Vec<&str> = steps
        .iter()
        .filter(|s| s.kind == "tool_call")
        .map(|s| s.name.as_deref().unwrap_or("?"))
        .collect();

    if calls.is_empty() {
        return Vec::new();
    }

    // 去重但保留顺序，最多列 8 个
    let mut uniq: Vec<&str> = Vec::new();
    for c in &calls {
        if !uniq.contains(c) {
            uniq.push(c);
        }
    }
    let shown: Vec<&str> = uniq.iter().take(8).copied().collect();
    let more = if uniq.len() > shown.len() {
        format!(" 等 {} 种", uniq.len())
    } else {
        String::new()
    };

    let line: String = format!(
        "[已执行：{}{}（共 {} 次工具调用）]",
        shown.join("、"),
        more,
        calls.len()
    )
    .chars()
    .take(FOLDED_DETAIL_LIMIT)
    .collect();

    vec![StoredStep {
        kind: "tool_call".into(),
        // `name` 必须带**第一个**工具名，不能留 None：
        // ① 幂等 —— 对已折叠记录再折一次，输出与第一次完全相同；
        // ② 可读 —— `recall_turns` / 前端看单条 step 时也能认出是什么工具。
        // detail 里已经是完整摘要，这里只是同一信息的结构化副本。
        name: Some(shown.first().copied().unwrap_or("?").to_string()),
        detail: line,
    }]
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("float_agent_sess_{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn ensure_current_creates_then_reuses() {
        let d = tmp("ensure");
        let a = ensure_current(&d);
        let b = ensure_current(&d);
        assert_eq!(a.id, b.id, "第二次应复用同一个会话");

        // current.txt 丢了 → 捡最近的，不新开
        let _ = std::fs::remove_file(super::current_file(&d));
        let c = ensure_current(&d);
        assert_eq!(c.id, a.id);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn append_turn_sets_title_and_roundtrips() {
        let d = tmp("append");
        // 全新库：ensure_current 会自愈出一条主会话，用它聊 → 标题**不应**被劫持
        append_turn(&d, "帮我看看这个目录", &[], "好的", &[]).unwrap();
        let s = ensure_current(&d);
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, "user");
        assert_eq!(s.messages[1].role, "assistant");
        assert!(s.is_main());
        assert_eq!(s.title, MAIN_TITLE, "主会话标题保持固定");

        // 重新从磁盘读（模拟重启）
        let again = load(&d, &s.id).unwrap();
        assert_eq!(again.messages.len(), 2);
        assert_eq!(again.messages[1].text, "好的");

        // 普通任务会话才用首条消息做标题
        let t = new_session(&d);
        append_turn(&d, "任务会话的首条", &[], "ok", &[]).unwrap();
        let t2 = load(&d, &t.id).unwrap();
        assert_eq!(t2.title, "任务会话的首条");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn messages_are_capped() {
        let d = tmp("cap");
        for i in 0..(MAX_MESSAGES / 2 + 5) {
            append_turn(&d, &format!("问{i}"), &[], "答", &[]).unwrap();
        }
        let s = ensure_current(&d);
        assert!(s.messages.len() <= MAX_MESSAGES, "消息数应有上限");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_inline_base64_images_are_dropped() {
        let d = tmp("legacy");
        let s = new_session(&d);
        // 手写一个带旧格式内联图的会话文件
        let legacy = serde_json::json!({
            "id": s.id,
            "title": "旧会话",
            "createdAt": 1,
            "updatedAt": 2,
            "messages": [
                { "role": "user", "text": "看看这个报错",
                  "images": ["data:image/png;base64,AAAA", "D:\\x\\keep.png"],
                  "at": 1 }
            ]
        });
        std::fs::write(
            session_file(&d, &s.id),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();

        let loaded = load(&d, &s.id).unwrap();
        let imgs = &loaded.messages[0].images;
        assert_eq!(imgs.len(), 1, "内联 base64 应被清掉: {imgs:?}");
        assert!(imgs[0].ends_with("keep.png"), "路径应保留: {imgs:?}");
        assert_eq!(loaded.messages[0].text, "看看这个报错", "文本不受影响");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn switch_and_delete_semantics() {
        let d = tmp("switch");
        // A 会是自愈出的主会话；再建 B 作为普通任务会话
        let a = ensure_current(&d);
        assert!(a.is_main());
        append_turn(&d, "会话A", &[], "ok", &[]).unwrap();

        let b = new_session(&d);
        append_turn(&d, "会话B", &[], "ok", &[]).unwrap();
        assert_eq!(current_id(&d).unwrap(), b.id);

        // 切回 A（主会话）
        let back = switch(&d, &a.id).unwrap();
        assert_eq!(back.title, MAIN_TITLE);
        assert_eq!(current_id(&d).unwrap(), a.id);

        // 主会话受保护，删不掉
        assert!(delete(&d, &a.id).is_err(), "主会话不可删除");

        // 普通会话可以删；删掉当前会话（B）后应自动切到主会话
        switch(&d, &b.id).unwrap();
        delete(&d, &b.id).unwrap();
        let cur = ensure_current(&d);
        assert_eq!(cur.id, a.id, "删当前会话后应切到剩下的主会话");

        let all = list(&d);
        assert_eq!(all.len(), 1);
        assert!(all[0].current);
        assert_eq!(all[0].kind, kind::MAIN);

        let _ = std::fs::remove_dir_all(&d);
    }

    // ---------------- 主会话（kind = main）----------------

    #[test]
    fn unique_main_migrates_old_data_without_kind_field() {
        let d = tmp("main_migrate");
        // 手写两条**没有 kind 字段**的老会话（模拟升级前的数据）
        // 注意：必须自己建 sessions/ 子目录，write_session 才会代劳
        std::fs::create_dir_all(sessions_dir(&d)).unwrap();
        for (id, title, created) in [("s1", "老会话一", 100u64), ("s2", "老会话二", 200u64)] {
            let legacy = serde_json::json!({
                "id": id, "title": title,
                "createdAt": created, "updatedAt": created,
                "messages": []
            });
            std::fs::write(
                session_file(&d, id),
                serde_json::to_string(&legacy).unwrap(),
            )
            .unwrap();
        }

        let m = unique_main(&d);
        assert_eq!(m.id, "s1", "created_at 最小的应升为主会话");
        assert_eq!(m.kind, kind::MAIN);
        assert_eq!(m.title, MAIN_TITLE, "主会话标题应被强制覆盖");

        // 再调一次必须幂等：仍只有 s1 是 main，且不产生新文件
        let m2 = unique_main(&d);
        assert_eq!(m2.id, "s1");
        let mains = list(&d)
            .into_iter()
            .filter(|x| x.kind == kind::MAIN)
            .count();
        assert_eq!(mains, 1, "全库有且仅有一条主会话");
        assert_eq!(list(&d).len(), 2, "不应凭空多出会话");

        // 留下的那条是 task 且标题未被覆盖
        let other = list(&d).into_iter().find(|x| x.id == "s2").unwrap();
        assert_eq!(other.kind, kind::TASK);
        assert_eq!(other.title, "老会话二");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unique_main_demotes_extra_mains() {
        let d = tmp("main_dedupe");
        std::fs::create_dir_all(sessions_dir(&d)).unwrap();
        for (id, k, created) in [("s1", kind::MAIN, 100u64), ("s2", kind::MAIN, 200u64)] {
            let s = serde_json::json!({
                "id": id, "title": "x", "kind": k,
                "createdAt": created, "updatedAt": created, "messages": []
            });
            std::fs::write(
                session_file(&d, id),
                serde_json::to_string(&s).unwrap(),
            )
            .unwrap();
        }

        let m = unique_main(&d);
        assert_eq!(m.id, "s1", "多个 main 时保留 created_at 最小的");
        assert_eq!(
            list(&d).into_iter().filter(|x| x.kind == kind::MAIN).count(),
            1,
            "多余的 main 应被降级为 task"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn main_is_pinned_on_top_and_title_not_hijacked() {
        let d = tmp("main_order");
        // 建三条会话；第一条 created_at 最小 = 主会话
        let main = ensure_current(&d);
        assert!(main.is_main(), "首次加载自愈出的会话应是主会话");

        // 主会话聊一条 → 标题必须保持「主聊天」
        append_turn(&d, "帮我改一下这个文档好吗", &[], "好", &[]).unwrap();
        let m = load(&d, &main.id).unwrap();
        assert_eq!(m.title, MAIN_TITLE, "主会话标题不应被首条消息劫持");

        // 再建两条任务会话，并让它们比主会话更新
        for i in 0..2 {
            let t = new_session(&d);
            append_turn(&d, &format!("任务{i}"), &[], "ok", &[]).unwrap();
            assert!(!t.is_main(), "new_session 建出的必须是任务会话");
        }

        let all = list(&d);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, main.id, "主会话必须排在列表第一位");
        assert_eq!(all[0].kind, kind::MAIN);
        // 其余按 updated_at 倒序
        assert!(all[1].updated_at >= all[2].updated_at);

        let _ = std::fs::remove_dir_all(&d);
    }

    // ---------------- 落盘折叠（fold_steps）----------------

    #[test]
    fn append_turn_stores_folded_steps() {
        let d = tmp("fold_store");
        let s = ensure_current(&d);
        let steps = vec![
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
        append_turn(&d, "看看", &[], "查了", &steps).unwrap();

        // 重新读盘：steps 必须已是折叠版，原文不可残留
        let on_disk = std::fs::read_to_string(session_file(&d, &s.id)).unwrap();
        assert!(
            !on_disk.contains("xxxx"),
            "原文 detail 不该落盘（单会话文件会膨胀）"
        );

        let after = load(&d, &s.id).unwrap();
        let st = &after.messages[1].steps;
        assert_eq!(st.len(), 1, "折叠后只留一条摘要");
        assert!(st[0].detail.contains("read_file"), "应列出工具名");
        assert!(st[0].detail.contains("grep_files"));
        assert!(st[0].detail.contains("2 次"), "应报调用次数: {}", st[0].detail);
        assert!(st[0].detail.len() <= FOLDED_DETAIL_LIMIT);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fold_steps_is_idempotent_for_downstream_reads() {
        // 折叠结果是**幂等**的：对已折叠的 steps 再折一次得到等价内容
        // （这保证"读时兜底折叠"对老新数据都安全，不会越折越不一样）
        let steps = vec![StoredStep {
            kind: "tool_call".into(),
            name: Some("read_file".into()),
            detail: "x".repeat(100),
        }];
        let once = fold_steps(&steps);
        let twice = fold_steps(&once);
        assert_eq!(once[0].detail, twice[0].detail, "折叠应幂等");
    }

    #[test]
    fn fold_steps_empty_without_tool_calls() {
        assert!(fold_steps(&[]).is_empty(), "空 steps 不应产生记录");
        let thoughts = vec![StoredStep {
            kind: "thought".into(),
            name: None,
            detail: "想了很久".into(),
        }];
        assert!(
            fold_steps(&thoughts).is_empty(),
            "只有 thought 没有工具调用时不该有折叠行"
        );
    }

    #[test]
    fn fold_steps_caps_name_list() {
        // 12 种不同工具 → 只列前 8 个，末尾补「等 N 种」
        let steps: Vec<StoredStep> = (0..12)
            .map(|i| StoredStep {
                kind: "tool_call".into(),
                name: Some(format!("tool{i}")),
                detail: String::new(),
            })
            .collect();
        let f = fold_steps(&steps);
        assert_eq!(f.len(), 1);
        assert!(f[0].detail.contains("等 12 种"), "应提示还有多少种: {}", f[0].detail);
        assert!(!f[0].detail.contains("tool11"), "只列前 8 个");
    }

    #[test]
    fn main_survives_message_cap_with_drain() {
        let d = tmp("main_cap");
        let main = ensure_current(&d);
        // 直接把消息堆到超过上限，省去几千次写盘
        let mut s = load(&d, &main.id).unwrap();
        s.messages = (0..(MAX_MESSAGES + 10))
            .map(|i| StoredMessage {
                role: "user".into(),
                text: format!("问{i}"),
                images: Vec::new(),
                steps: Vec::new(),
                at: i as u64,
            })
            .collect();
        write_session(&d, &s).unwrap();

        append_turn(&d, "触发截断", &[], "ok", &[]).unwrap();
        let after = load(&d, &main.id).unwrap();
        assert!(after.messages.len() <= MAX_MESSAGES, "主会话同样受 400 条上限约束");
        assert!(after.is_main(), "截断不应改变会话类型");

        let _ = std::fs::remove_dir_all(&d);
    }
}
