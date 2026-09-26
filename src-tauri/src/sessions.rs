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
//!     current.txt          ← 当前会话 id（一行文本）
//!     <id>.json            ← 一个会话的全部消息
//!     <id>.grants.json     ← 该会话「整个任务」档的临时授权（无授权时不存在）
//! ```
//!
//! 授权为什么放在这：**跟着会话一起生、一起死**。会话删掉 → 授权文件一并无了，
//! 不需要额外的撤销逻辑，也不会留下孤儿授权（同「目录删了记忆自然没了」的思路）。
//! 单会话消息数有上限（超出从头截断），避免文件无限长大。

use crate::permission;
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

/// 时间线条目单条的落盘上限（字符）。
///
/// 为什么不像以前那样**整组折叠成一行**（见 [`cap_steps`]）：折叠把"做了几步、
/// 哪一步的思考对应哪个工具"全部抹平了，用户回看时只剩
/// `[已执行：read_file、grep（共 12 次工具调用）]` —— 等于"做过的事又消失一次"
/// （2026-09-22 用户报的第二个问题）。现在保留条目与顺序，只在**单条**上截断。
///
/// ⚠️ 只作用于工具类条目（参数 / 结果）。**思考条目不受任何上限约束** ——
/// 见 [`cap_steps`] 的说明。
const STEP_DETAIL_LIMIT: usize = 300;

/// 单轮落盘的时间线条目数上限（超出只留最近的，前面补一条省略说明）。
///
/// 会话是**全量 JSON 读写**，单文件体积要控制。但上限不能压到会丢掉思考：
/// 2026-09-26 实测用户一条长任务里单轮就产生 61 步、被砍掉 **199 步**更早的
/// 步骤（`omitted` 标记实锤）—— 症状③「早期轮次思考块消失」就是这么来的。
///
/// 轮数硬顶 250 × 每轮约 3 条 ≈ 750 步，取值要高于它才不会误砍。
/// 并且超限时**只丢最早的普通步骤，思考条目永不丢**（见 [`cap_steps`]）。
const STEPS_LIMIT: usize = 1200;

/// 单轮落盘的链路状态条目上限（限流退避通知这类，只留最后几条）
const STATUS_STEPS_LIMIT: usize = 8;

/// 按字符数（不是字节）截断，超限时追加截断说明。
fn cap_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    format!("{}……（已截断）", s.chars().take(limit).collect::<String>())
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 主会话的固定标题。主会话不可删、不参与「首条消息改名」，
/// 名字必须一眼能认出，否则它在列表里和普通任务会话无从区分。
const MAIN_TITLE: &str = "主聊天";

/// 会话类型。整库**有且仅有一条** `Main`；`Fork` 从主会话分叉出来，其余都是 `Task`。
pub mod kind {
    pub const MAIN: &str = "main";
    pub const TASK: &str = "task";
    /// 从**主聊天**分叉出来的分支会话。只能由主聊天分叉（不能分叉分支/任务）。
    pub const FORK: &str = "fork";
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
    /// 本轮的思考过程（思维链）。**老数据没有这个字段 → None**。
    ///
    /// 只存**不用** —— 不回灌给模型（见 `history.rs`）。存它是为了用户回看，
    /// 不是为了给后续轮次当上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 本轮被用户中断，`text` 是**半截**正文。
    ///
    /// 用独立字段而不是在 text 末尾追加"（已中断）"：后者会污染复制、
    /// 搜索和历史回灌的字节，且一旦落盘就再也分不清哪部分是模型原话。
    #[serde(default, skip_serializing_if = "is_false")]
    pub interrupted: bool,
    /// **未定稿的占位消息**（2026-09-26 加的"退出不丢轮"机制）。
    ///
    /// 一轮对话开跑时，先落一条 `partial = true` 的助手占位消息；轮内每完成
    /// 一段就增量更新它；轮结束由 `finish_turn` 定稿（清掉这个标记）。
    /// 进程中途退出时，磁盘上留下的就是"提问 + 已经产出的部分"，
    /// 而不是以前的**整轮蒸发**。
    ///
    /// 老数据没有这个字段 → 默认 `false`（= 已定稿），行为不变。
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    /// 执行中插话的 id。有值时 UI **不在对话流铺气泡**，
    /// 而是在 assistant.steps 的 `kind=steer` 时间线里按位置画（真实交错）。
    /// 历史回灌仍按 user 角色进模型上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer_id: Option<String>,
    /// 生成本条回答的模型 id（供「每天每模型」用量统计）。老数据没有 → None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本轮问答的 token 用量合计（含中途调工具的各轮）。
    /// 服务端没返回 usage 时是 None —— 这种条目不进用量统计，也不显示角标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::llm::TokenUsage>,
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
    /// `main` | `task` | `fork`，见 [`kind`]。老数据没有这个字段 → 默认 `task`，
    /// 由 [`unique_main`] 在首次加载时自愈出一条 main，无需迁移脚本。
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 分叉来源的会话 id（只有 `kind == "fork"` 时有）。
    /// **只作审计与展示** —— 不做回流/合并/同步（分叉不是 git branch）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// 分叉点：源会话里的第几条消息（下标）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_at: Option<usize>,
    /// **上下文摘要**（compact 产物）。覆盖 `summary_upto` 之前的所有消息。
    ///
    /// 为什么必须**落盘**而不是每次读取时现算：prompt cache 按 token 前缀
    /// 逐字节比对，摘要若每轮重新生成（模型输出天然有随机性），前缀就会抖动，
    /// 其后全部 token 按未命中价重付。落盘一次 = 写进去什么样，后面读出来就什么样。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 摘要覆盖到第几条消息（下标，不含）。`summary_upto = n` 表示
    /// `messages[0..n]` 已被 `summary` 概括，回灌时从 `messages[n..]` 取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_upto: Option<usize>,
}

fn default_kind() -> String {
    kind::TASK.to_string()
}

impl Session {
    pub fn is_main(&self) -> bool {
        self.kind == kind::MAIN
    }

    /// 能不能被分叉 —— **只有主聊天可以**。
    ///
    /// 用户定案：主聊天是"关系的主线"，从它上面挑一个上下文窗口开一条新线才有意义；
    /// 任务会话本来就是一次性的，分叉它只会制造一堆没人认领的碎片。
    pub fn can_fork(&self) -> bool {
        self.is_main()
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

/// 某会话的临时授权文件：`sessions/<id>.grants.json`
///
/// 与 `<id>.json` **平级**（不是子目录）—— 会话本来就是扁平存储，
/// 不为了一个授权文件引入目录层级。
fn grants_file(data_dir: &Path, id: &str) -> PathBuf {
    sessions_dir(data_dir).join(format!("{id}.grants.json"))
}

/// 判断某个文件是不是授权文件（`list()` 要靠它把授权和会话分开）
fn is_grants_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|x| x.to_str())
        .is_some_and(|n| n.ends_with(".grants.json"))
}

/// 当前 epoch 毫秒（会话/summary 时间戳共用口径）。
pub fn now_ms() -> u64 {
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
    let mut s = match serde_json::from_str::<Session>(&txt) {
        Ok(s) => s,
        Err(e) => {
            // 静默跳过会让会话"凭空消失"，而且 `unique_main` 可能顺势把**另一条**
            // 会话升成主会话 —— 到时候完全看不出原因。留一行痕迹。
            eprintln!("[sessions] ⚠️ {} 解析失败，本次跳过: {e}", path.display());
            return None;
        }
    };
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

/// 落盘一个会话（对外入口）。
///
/// 给 compact 摘要写入用：`history::compact` 生成摘要后需要把 `summary` /
/// `summary_upto` 写回会话文件，而底层 `write_session` 是私有的。
pub fn save(data_dir: &Path, s: &Session) -> Result<(), String> {
    write_session(data_dir, s)
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
        forked_from: None,
        fork_at: None,
        summary: None,
        summary_upto: None,
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
        // ⚠️ FORK 必须显式保留。这里原来对所有非 main 一律写成 TASK，
        //    而 `ensure_current` **每次**都会调 `unique_main` 并写回磁盘 ——
        //    照旧逻辑，分叉会话在下一次任何会话操作后就会被降级成普通任务会话
        //    （"不可再分叉"的约束随之失效）。这是本项目最容易踩的一个坑。
        let want_kind = if s.id == main_id {
            kind::MAIN
        } else if s.kind == kind::FORK {
            kind::FORK
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
        forked_from: None,
        fork_at: None,
        summary: None,
        summary_upto: None,
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

// ---------------------------------------------------------------------------
// 临时授权（「整个任务」档）
// ---------------------------------------------------------------------------

// ⚠️ 这三个是「申请权限」的落盘通道，已实现且有单测覆盖，
//    但**工具层还没接线**（见 docs/02-PERMISSION-REQUEST.md 第 5 项）。
//    接线后 allow 应全部删掉。
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct GrantsFile {
    #[serde(default)]
    grants: Vec<permission::Grant>,
}

/// 读某会话的临时授权。**文件不存在 = 空表，不是错误**。
#[allow(dead_code)]
pub fn load_grants(data_dir: &Path, id: &str) -> Vec<permission::Grant> {
    let path = grants_file(data_dir, id);
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<GrantsFile>(&txt) {
        Ok(f) => f.grants,
        Err(e) => {
            // 坏文件按空表处理。方向是**保守的**：空表 = 回到纯规则判定 = 更严，
            // 不会因为文件损坏而多放行什么。
            eprintln!(
                "[orbcat] ⚠️ {} 解析失败（{e}），本次按「无临时授权」处理",
                path.display()
            );
            Vec::new()
        }
    }
}

/// 写某会话的临时授权。**空表 → 删文件**，不留空壳。
#[allow(dead_code)]
pub fn save_grants(data_dir: &Path, id: &str, grants: &[permission::Grant]) -> Result<(), String> {
    let path = grants_file(data_dir, id);

    if grants.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("清理 {} 失败: {e}", path.display())),
        };
    }

    let dir = sessions_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建会话目录失败: {e}"))?;
    let file = GrantsFile {
        grants: grants.to_vec(),
    };
    let txt = serde_json::to_string_pretty(&file).map_err(|e| format!("序列化授权失败: {e}"))?;
    std::fs::write(&path, txt).map_err(|e| format!("写授权失败: {e}"))
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
        // `sessions/<id>.grants.json` 是临时授权，不是会话。
        // 不跳也能活（解析 `Session` 必失败会被跳过），但那是靠"碰巧"不是靠"明确"。
        if is_grants_file(&p) {
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

/// 改会话标题（主聊天标题固定，不可改）
pub fn set_title(data_dir: &Path, id: &str, title: &str) -> Result<(), String> {
    let mut s = load(data_dir, id).ok_or_else(|| format!("会话 {id} 不存在"))?;
    if s.is_main() {
        return Err("主聊天标题固定，不能改".into());
    }
    let t = title.trim();
    if t.is_empty() {
        return Err("标题不能为空".into());
    }
    s.title = t.to_string();
    write_session(data_dir, &s)
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
    // 授权跟着会话一起走 —— 会话都没了，「整个任务」档的授权不该留成孤儿。
    // 失败不算错（本来就可能不存在），所以忽略返回值。
    let _ = std::fs::remove_file(grants_file(data_dir, id));
    if current_id(data_dir).as_deref() == Some(id) {
        let _ = std::fs::remove_file(current_file(data_dir));
        let _ = ensure_current(data_dir);
    }
    Ok(())
}

/// 从第 `upto` 条消息起**删到末尾**（含 `upto` 自己），并把结果写回磁盘。
///
/// 语义（用户 2026-09-20 要求）：在对话流里点某条消息上的「删除」，
/// 就是「从这条开始，后面全不要了」—— 包括这条。也就是把 `messages` 截成
/// `messages[..upto]`。这是**不可恢复**的，前端必须二次确认。
///
/// ## 为什么不删图片文件
/// `messages[i].images` 存的是 `agent-data/images/` 里的**路径**。同一个路径
/// 可能被分叉会话共享，也可能是用户手动引用过的文件 —— 删消息时顺手删文件
/// 迟早会删掉别人还在用的东西。所以这里只切会话数据，图片留在盘上（宁可留垃圾，
/// 不删不确定归属的文件）。
///
/// ## 为什么不动 `updated_at`
/// `list()` 按 `updated_at` 倒排。删几条旧消息不该让会话在列表里"跳"到最前面，
/// 保持时间戳原样，用户的列表顺序不会被这个操作打乱。
///
/// 返回截断后的会话。
pub fn truncate(data_dir: &Path, id: &str, upto: usize) -> Result<Session, String> {
    let path = session_file(data_dir, id);
    let mut s = read_session(&path).ok_or_else(|| format!("会话 {id} 不存在或已损坏"))?;
    if s.messages.is_empty() {
        return Err("这个会话还没有消息可删".into());
    }
    // 下标越界就夹到末尾（前端手里的下标可能因为历史截断而偏大）——
    // 夹到末尾等于"删掉最后一条"，仍然符合"从这条起往后删"的直觉，不会误删更多。
    let idx = upto.min(s.messages.len() - 1);
    s.messages.truncate(idx);
    // 摘要失效检查：`summary` 概括的是 `messages[0..summary_upto]`。
    // 若截断落进了这个区间（idx < summary_upto），被概括的消息已被删掉一部分，
    // 摘要不再对应任何真实内容 → 必须一并丢弃，否则回灌会带上一段"凭空捏造"的上下文。
    if s.summary_upto.map_or(false, |u| u > idx) {
        s.summary = None;
        s.summary_upto = None;
    }
    write_session(data_dir, &s)?;
    Ok(s)
}

/// 从**主聊天**的某条消息处分叉出一条新会话。
///
/// ## 语义（用户 2026-09-20 修正）
/// **分叉的上下文 = 下一轮实际会喂给模型的那段窗口**，不是"到那条为止的全部历史"。
/// 也就是说：把主聊天切成 `messages[..=upto]` 这个前缀，再套用 [`crate::history::pick_window`]
/// 的同一套规则（最近 [`crate::history::WINDOW_HOURS`] 小时 ∪ 最近
/// [`crate::history::FALLBACK_TURNS`] 条），**只把选出来的那几条复制进新会话**。
///
/// 为什么必须和回灌共用规则：分叉的意义是"从这里接着聊"，而接着聊时模型真正
/// 看到的就只有那个窗口。若把 400 条历史原样搬过去，新会话一开场的上下文预算
/// 就白占一大截，而且和"从这条消息继续"的真实视图对不上。
///
/// 因为最后一条必然落在最近 `FALLBACK_TURNS` 条内，**分叉点自己一定在窗口里**。
///
/// ## 三条硬约束
/// - **只有主聊天能分叉**（`can_fork`）。任务会话分叉只会产出没人认领的碎片。
/// - 分叉点必须是 **assistant 完整回复**（不是 user）：挂在 user 上时新会话
///   末条是未回答的提问，下一轮回灌再接用户输入会拼成 user→user，轮次结构
///   是坏的。完整一轮的边界在 assistant 之后。要防的是「模型说了半句」，
///   不是「assistant 之后」——流式未完成的消息本来就不该给出分叉入口。
/// - **不继承源会话的 `.grants.json`**：新 id 天然没有那个文件。授权跟着会话
///   生死是既定约定，分叉会话要授权就得在它自己里面重新批（批了也只活在它自己
///   的 grants 文件里，删掉分叉就一并没有）。
///
/// 返回新会话，并把它设为当前会话。
pub fn fork(data_dir: &Path, src: &Session, upto: usize) -> Result<Session, String> {
    if !src.can_fork() {
        return Err("只有主聊天可以被分叉".into());
    }
    if src.messages.is_empty() {
        return Err("主聊天还没有消息，没有可分叉的点".into());
    }
    // 下标越界就夹到末尾 —— 前端手里的下标可能因为 400 条截断而偏大
    let idx = upto.min(src.messages.len() - 1);
    if src.messages[idx].role != "assistant" {
        return Err("分叉点必须选在一条 AI 回复上".into());
    }

    let now = now_ms();
    // 前缀 → 套用回灌同一套窗口规则（"下一轮塞什么，分叉的上下文就是什么"）
    let window = crate::history::pick_window(&src.messages[..=idx], now);
    if window.is_empty() {
        return Err("分叉点之前没有落在上下文窗口里的消息".into());
    }

    // 标题优先取分叉点前最近一条用户提问 —— 会话列表里比 assistant 正文好认
    let head: String = src.messages[..=idx]
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text.trim().chars().take(18).collect::<String>())
        .unwrap_or_default();
    let s = Session {
        id: new_id(),
        // 「⑂」前缀让它在会话列表里一眼能认出来
        title: if head.is_empty() {
            "⑂ 分支会话".into()
        } else {
            format!("⑂ {head}")
        },
        created_at: now,
        updated_at: now,
        messages: window,
        kind: kind::FORK.into(),
        forked_from: Some(src.id.clone()),
        fork_at: Some(idx),
        // 分叉复制的是一个"窗口"，下标与原会话不同 —— 原摘要的 summary_upto
        // 在新会话里对不上，直接不带（新会话从头开始积累自己的摘要）。
        summary: None,
        summary_upto: None,
    };
    write_session(data_dir, &s)?;
    set_current(data_dir, &s.id);
    Ok(s)
}

/// 一次 run 要落盘的全部内容。
///
/// 参数多到不该用位置参数传（`append_turn` 当年 5 个已经够呛，加上思考过程、
/// 插话、中断标记就要 8 个），所以聚成结构体。
pub struct TurnRecord<'a> {
    /// 本轮用户消息
    pub user_text: &'a str,
    pub images: &'a [String],
    /// 执行中「插话」进来的用户消息（按送达顺序）
    pub steers: &'a [crate::agent::SteeredMsg],
    /// 助手回答（正常情况下是完整答复；`interrupted` 时是半截）
    pub answer: &'a str,
    pub reasoning: Option<&'a str>,
    pub steps: &'a [StoredStep],
    pub interrupted: bool,
    /// 本轮用的模型 id —— 用量统计要按模型分组
    pub model: &'a str,
    /// 本轮问答的 token 用量合计（全 0 则落盘时不写）
    pub usage: crate::llm::TokenUsage,
}

/// 一轮**开跑时**立刻落盘：用户提问 + 一条 `partial` 助手占位。
///
/// ## 为什么需要（2026-09-26 用户报「退出后整轮清空」）
///
/// 以前整轮只在 `agent::run` 返回之后才走 [`append_run`]。用户在长命令
/// 执行期等不及直接关掉进程时，那个 future 被直接丢弃，`append_run` 根本没
/// 执行 —— 提问、思考、工具步骤、正文**一个字都没落盘**。
/// 实证：`s1790400662188-0.json` 13:31 建、`messages: []`，而 grants 里
/// 13:34 批的 `D:/mimo-proxy` 授权正是卡死那轮留下的。
///
/// 现在改成：开跑先落提问与占位 → 轮内 [`checkpoint_turn`] 增量更新 →
/// 结束 [`append_run`] 定稿。哪怕进程被杀，磁盘上至少留下"问了什么 +
/// 已经产出到哪一步"。
pub fn begin_turn(data_dir: &Path, user_text: &str, images: &[String]) -> Result<(), String> {
    begin_turn_in(data_dir, None, user_text, images)
}

/// 解析"这一轮该写进哪个会话"。
///
/// 显式 `session_id` 优先 —— 支持**切走会话、后台继续跑**（2026-09-26 用户拍板）：
/// 用户中途切走时 `current.txt` 已经变了，但这一轮的结果必须落回**开跑时的那个**
/// 会话（`chat` 开跑前就把 id 定死了）。id 为空 / 指向已不存在的会话时退回当前会话。
fn target_session(data_dir: &Path, session_id: Option<&str>) -> Session {
    if let Some(id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(s) = load(data_dir, id) {
            return s;
        }
    }
    ensure_current(data_dir)
}

/// 同 [`begin_turn`]，但**显式指定落盘会话**（切走会话、后台继续跑用）。
pub fn begin_turn_in(
    data_dir: &Path,
    session_id: Option<&str>,
    user_text: &str,
    images: &[String],
) -> Result<(), String> {
    let mut s = target_session(data_dir, session_id);
    let now = now_ms();

    s.messages.push(StoredMessage {
        role: "user".into(),
        text: user_text.to_string(),
        images: images.to_vec(),
        steps: Vec::new(),
        reasoning: None,
        interrupted: false,
        partial: false,
        steer_id: None,
        model: None,
        usage: None,
        at: now,
    });
    // 助手占位：`partial = true` 是"还没定稿"的标记，定稿时被 append_run 换掉。
    s.messages.push(StoredMessage {
        role: "assistant".into(),
        text: String::new(),
        images: Vec::new(),
        steps: Vec::new(),
        reasoning: None,
        interrupted: false,
        partial: true,
        steer_id: None,
        model: None,
        usage: None,
        at: now,
    });

    // 标题规则与 append_run 保持一致（首次有用户消息时定下来）
    if !s.is_main() && s.title == "新会话" {
        let t: String = user_text.trim().chars().take(24).collect();
        if !t.is_empty() {
            s.title = t;
        }
    }
    s.updated_at = now;
    write_session(data_dir, &s)
}

/// 轮内**增量更新**那条 `partial` 助手占位（提问已由 [`begin_turn`] 落盘）。
///
/// 由 agent 循环在轮边界 / 每个工具跑完后回调触发。找不到占位就静默返回 ——
/// 说明这轮没走 `begin_turn`（老路径 / 分叉会话），不要在这里硬造一条消息。
pub fn checkpoint_turn(
    data_dir: &Path,
    steps: &[StoredStep],
    answer: &str,
    reasoning: Option<&str>,
) -> Result<(), String> {
    checkpoint_turn_in(data_dir, None, steps, answer, reasoning)
}

/// 同 [`checkpoint_turn`]，但**显式指定落盘会话**（切走会话、后台继续跑用）。
pub fn checkpoint_turn_in(
    data_dir: &Path,
    session_id: Option<&str>,
    steps: &[StoredStep],
    answer: &str,
    reasoning: Option<&str>,
) -> Result<(), String> {
    let mut s = target_session(data_dir, session_id);
    let Some(last) = s.messages.last_mut() else {
        return Ok(());
    };
    if !(last.partial && last.role == "assistant") {
        return Ok(());
    }
    last.steps = cap_steps(steps);
    if !answer.trim().is_empty() {
        last.text = strip_step_folds(answer);
    }
    if let Some(r) = reasoning {
        if !r.is_empty() {
            last.reasoning = Some(r.to_string());
        }
    }
    s.updated_at = now_ms();
    write_session(data_dir, &s)
}

/// 往当前会话追加一轮完整对话（用户消息 + 插话 + 助手结果）。
///
/// ## 落盘顺序的取舍
/// 插话消息统一排在**本轮用户消息之后、助手结果之前**，而不是精确插在
/// "当时已流出的正文"之间。理由：模型自己看到的顺序在内存 `messages` 里是
/// 精确的；落盘只需保证"用户说了什么、答案是什么、能回看"。要做到 token 级
/// 精确就得存多个 assistant 片段，存储模型会复杂一个量级，收益却只是"回看时
/// 位置更准"——不值。
///
/// ## 落盘的其它约定
/// - 工具步骤在这里**只限长、不折叠**（见 [`cap_steps`]）：保留
///   `reasoning / tool_call / tool_result / status / error` 的顺序，界面才能
///   还原成交错时间线。喂给模型的那份仍走 [`fold_steps`]（`history.rs` 读时）。
/// - `reasoning` **原文全存**（2026-09-26 起不截）。这是**旧字段**，
///   新数据以 `steps` 里的 `reasoning` 条目为准；保留写入是为了老前端 / 回滚安全。
/// - `interrupted` 的中断轮**照样落盘** —— 用户要看到自己那条消息和已产出的部分。
pub fn append_run(data_dir: &Path, rec: &TurnRecord<'_>) -> Result<(), String> {
    append_run_in(data_dir, None, rec)
}

/// 同 [`append_run`]，但**显式指定落盘会话**（切走会话、后台继续跑用）。
///
/// ⚠️ 为什么要显式指定：切走会话后台继续跑时，`current.txt` 在跑的中途就变了；
/// 仍按"当前会话"落盘的话，回答会写进**另一个**会话（见 `session_new` 的旧注释）。
pub fn append_run_in(
    data_dir: &Path,
    session_id: Option<&str>,
    rec: &TurnRecord<'_>,
) -> Result<(), String> {
    let mut s = target_session(data_dir, session_id);
    let now = now_ms();

    // 插话（执行中追加的指令）—— 仍是 user 消息（历史回灌要按角色），
    // 但带 steer_id：UI 不单独铺气泡，改画在 steps 时间线里（与真实进度交错）。
    // 「退出不丢轮」（2026-09-26）：开跑时 `begin_turn` 已经落了 [用户提问 +
    // partial 助手占位]。这里若发现队尾是那条占位，就**就地定稿**：
    //   - 用户消息**不再重复 push**（它已经在盘上）
    //   - 把占位替换成真正的助手消息（补全 steps / 正文 / 用量）
    // 插话仍按原有顺序插在助手结果之前。
    let has_placeholder = matches!(
        s.messages.last(),
        Some(m) if m.partial && m.role == "assistant"
    );
    if !has_placeholder {
        // 老路径 / 直接调用（测试、分叉）：自己补用户消息
        s.messages.push(StoredMessage {
            role: "user".into(),
            text: rec.user_text.to_string(),
            images: rec.images.to_vec(),
            steps: Vec::new(),
            reasoning: None,
            interrupted: false,
            partial: false,
            steer_id: None,
            model: None,
            usage: None,
            at: now,
        });
    } else {
        // 定稿：先摘掉占位（插话要插在它前面），最后再 push 真正的助手消息
        s.messages.pop();
    }

    for m in rec.steers {
        s.messages.push(StoredMessage {
            role: "user".into(),
            text: m.text.clone(),
            images: m.images.clone(),
            steps: Vec::new(),
            reasoning: None,
            interrupted: false,
            partial: false,
            steer_id: Some(m.id.clone()),
            model: None,
            usage: None,
            at: now,
        });
    }

    s.messages.push(StoredMessage {
        role: "assistant".into(),
        text: strip_step_folds(rec.answer),
        images: Vec::new(),
        steps: cap_steps(rec.steps),
        // 思考过程**原文全存**（2026-09-26 起不截）：回看时要能逐字复现它当时怎么想的
        reasoning: rec.reasoning.filter(|r| !r.is_empty()).map(str::to_string),
        interrupted: rec.interrupted,
        // 定稿：清掉 partial 标记
        partial: false,
        steer_id: None,
        // 用量与模型只落在 assistant 这条上 —— 统计的就是"每次问答花了多少"
        model: Some(rec.model.to_string()),
        usage: (!rec.usage.is_zero()).then_some(rec.usage),
        at: now,
    });

    // 标题：首次有用户消息时定下来。
    // **主会话不参与** —— 它的名字固定为「主聊天」，被首条消息劫持后就认不出来了。
    if !s.is_main() && s.title == "新会话" {
        let t: String = rec.user_text.trim().chars().take(24).collect();
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
///
/// ⚠️ 保留旧签名（8+ 处单测在用）。新增能力走 [`append_run`]。
///    lib.rs 已全部改走 `append_run`，所以这个壳目前只有测试在用。
#[allow(dead_code)]
pub fn append_turn(
    data_dir: &Path,
    user_text: &str,
    images: &[String],
    answer: &str,
    steps: &[StoredStep],
) -> Result<(), String> {
    append_run(
        data_dir,
        &TurnRecord {
            user_text,
            images,
            steers: &[],
            answer,
            reasoning: None,
            steps,
            interrupted: false,
            // 旧壳不记用量（usage 全 0 时不落盘），模型留空
            model: "",
            usage: crate::llm::TokenUsage::default(),
        },
    )
}

/// 把 Rust 侧 AgentStep 转成可存储的步骤
/// 剥掉正文里混入的「工具轨迹 / 思考摘要」尾巴。
///
/// 根因（2026-09-24）：`history::fold_steps_to_line` 把 steps 拼进发给模型的
/// assistant 文本后，部分模型（商汤等）会把 `«steps…»` / `调用 …` / `·think …`
/// / 旧版 `[思考] …` 回显进 `answer`。落盘前剥掉，界面才不会把思考画成正文。
pub fn strip_step_folds(text: &str) -> String {
    let t = text;
    let mut cut = t.len();
    let markers = [
        "\n«steps\n",
        "\n«steps",
        "\n[思考] ",
        "\n[状态] ",
        "\n[中间正文] ",
        "\n[已执行",
        "\n·think ",
        "\n·status ",
        "\n·text ",
        "\n调用 ",
        "\n  参数: ",
    ];
    for m in markers {
        if let Some(i) = t.find(m) {
            // 前面要有像样的正文，避免把「一上来就是工具」的合法内容砍掉
            if i > 0 && i < cut {
                cut = i;
            }
        }
    }
    let mut out = t[..cut].trim_end().to_string();
    // 整段就是轨迹（没有任何正文）→ 空串
    if out.is_empty() && cut < t.len() {
        return String::new();
    }
    // 若正文里还嵌着完整轨迹块，再整块删掉
    while let (Some(a), Some(b)) = (out.find("«steps\n"), out.find("\n»")) {
        if b > a {
            out.replace_range(a..=b.min(out.len() - 1), "");
        } else {
            break;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod strip_folds_tests {
    use super::strip_step_folds;

    #[test]
    fn strips_legacy_think_tail() {
        let t = "直接说：不是论文术语。\n[思考] 用户在问临床故事…";
        assert_eq!(strip_step_folds(t), "直接说：不是论文术语。");
    }

    #[test]
    fn strips_tool_log_tail() {
        let t = "回答正文\n调用 read_file\n  参数: {\"path\": \"x\"}\nread_file 返回:\n130|\n·think 用户在问";
        assert_eq!(strip_step_folds(t), "回答正文");
    }

    #[test]
    fn strips_steps_block() {
        let t = "正文\n«steps\n调用 read_file\n  参数: {}\n»\n";
        assert!(strip_step_folds(t).starts_with("正文"));
        assert!(!strip_step_folds(t).contains("«steps"));
    }

    #[test]
    fn keeps_clean_answer() {
        let t = "# 标题\n\n这是一段正常回答。";
        assert_eq!(strip_step_folds(t), t);
    }
}

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

/// 落盘前给时间线条目"限长"，**但保留条目与顺序**。
///
/// ## 与 `history::fold_steps_to_line` 的分工（2026-09-23 更新）
///
/// - 喂给**模型**的历史 → `history::fold_steps_to_line`：近 K 条 steps **原文/截断**
///   （含 tool_result），更旧折一行摘要。**不要**再把 tool_result 压成一行。
/// - 存给**人**看的会话 → 本函数 [`cap_steps`]：保留 `reasoning / tool_call /
///   tool_result / status / error` 的原始顺序，只截断单条 `detail`。这样界面能把
///   「思考 → 调工具 → 思考 → 调工具」还原成交错时间线。
///
/// 原来的做法是落盘时就折叠成一整行，结果是**回看时步骤全没了** —— 用户
/// 感知到的"它做的事消失了"其实有第二次（第一次是错误路径整轮不落盘，见
/// `agent.rs::RunAbort`）。
///
/// ## 保留了什么
/// - 顺序：不做任何重排、不合并相邻同种条目
/// - 种类：reasoning / tool_call / tool_result / status / error / assistant 全留
/// - 思考条目：**原文全留**（见下）
/// - 其余条目的 `detail`：截到 [`STEP_DETAIL_LIMIT`]，末尾加"（已截断）"
///
/// ## 思考过程为什么完全不截（2026-09-26 用户拍板）
///
/// 以前有 `REASONING_LIMIT = 8KB` 的**总预算**，且预算是按**字节**算的 ——
/// 中文一个字 3 字节，实际只装得下约 2700 字；更糟的是预算一旦耗尽，
/// **后续思考条目整条不落盘**（`continue`）。用户报的三个症状都由它引起：
/// 展开后内容比标注字数少、早期轮次的思考块整个消失。
///
/// 现在的取舍：思维链是"它当时怎么想的"，正是回看时最想看的东西，
/// 不设任何上限。会话文件会变大（用户已确认接受）。
///
/// ## 砍掉了什么
/// - status 条目只留最后 [`STATUS_STEPS_LIMIT`] 条（限流退避通知会刷屏）
/// - 总条数超 [`STEPS_LIMIT`] → 只留最近的，最前面补一条 `omitted` 说明
pub fn cap_steps(steps: &[StoredStep]) -> Vec<StoredStep> {
    let total_status = steps.iter().filter(|s| s.kind == "status").count();
    let drop_status = total_status.saturating_sub(STATUS_STEPS_LIMIT);
    let mut seen_status = 0usize;
    let mut out: Vec<StoredStep> = Vec::with_capacity(steps.len());
    for s in steps {
        if s.kind == "status" {
            seen_status += 1;
            if seen_status <= drop_status {
                continue;
            }
        }
        let detail = if s.kind == "reasoning" || s.kind == "thought" {
            // 思考条目：**原文全留**，不截、不丢（见函数文档）
            s.detail.clone()
        } else if s.kind == "text" || s.kind == "stream" {
            // 多轮流式正文按轮收进时间线 —— 回看要比 tool 参数更长一点
            cap_chars(&s.detail, 2000)
        } else {
            cap_chars(&s.detail, STEP_DETAIL_LIMIT)
        };
        out.push(StoredStep {
            kind: s.kind.clone(),
            name: s.name.clone(),
            detail,
        });
    }

    if out.len() > STEPS_LIMIT {
        // 超限时**思考条目永不丢** —— 只丢最早的普通步骤（tool_call / tool_result /
        // status / error / text）。症状③「早期思考块消失」正是被这里的无差别
        // `drain(..cut)` 砍掉的：用户一轮有 200+ 步时，最早那批思考全没了。
        //
        // 若可丢的普通步骤不够（思考本身就超限）→ 条数会略微超出 STEPS_LIMIT。
        // 这是**刻意的**：宁可条数超一点，也不丢思考。
        let must_drop = out.len().saturating_sub(STEPS_LIMIT);
        let mut keep: Vec<StoredStep> = Vec::with_capacity(STEPS_LIMIT + 1);
        let mut dropped = 0usize;
        for s in out.iter() {
            if dropped < must_drop && !is_keep_always(&s.kind) {
                dropped += 1;
                continue;
            }
            keep.push(s.clone());
        }
        if dropped > 0 {
            keep.insert(
                0,
                StoredStep {
                    kind: "omitted".into(),
                    name: None,
                    detail: format!("（本轮还有 {dropped} 步更早的非思考步骤，已省略）"),
                },
            );
        }
        out = keep;
    }
    out
}

/// 这些 kind 的条目**任何情况下都不丢**（超限时优先丢别的）。
///
/// 为什么只有思考：它是"它当时怎么想的"，回看时最想看、也最难从别处恢复
/// （工具调用可以从工具结果反推，思考不行）。
fn is_keep_always(kind: &str) -> bool {
    kind == "reasoning" || kind == "thought"
}

/// 把 assistant 消息的 steps 压成一条摘要（历史回灌用）。
///
/// 输出形如 `[已执行：read_file、grep_files（共 2 次工具调用）]`。
/// **保留原始 `kind` 字段**（`tool_call`），`detail` 换成摘要 ——
/// 这样下游看到的是"一条 steps 记录"，不必区分折叠前后。
///
/// ⚠️ 用途已收窄（2026-09-22）：**只服务于"喂给模型的历史"**（`history.rs`），
///    不再用于落盘。落盘改走 [`cap_steps`]（保留条目与顺序）。
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
// 用量统计
// ---------------------------------------------------------------------------

/// 一条用量记录（每条带用量的 assistant 消息一条）。
///
/// 不在这里按日期聚合：只有前端能拿到系统的**本地时区**，
/// Rust 侧硬按 UTC 算「今天」会把跨零点的对话算到隔壁日期。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    /// 消息时间戳（epoch ms）
    pub at: u64,
    /// 模型 id（老数据可能为空串）
    pub model: String,
    /// 所属会话 id
    pub session: String,
    pub usage: crate::llm::TokenUsage,
}

/// 扫描**全部**会话，收集所有带 token 用量的 assistant 消息。
///
/// 为什么在 Rust 侧读盘：用量是跨会话的全局账，前端只看得到当前会话，
/// 拿不到别的会话的历史。
pub fn usage_report(data_dir: &Path) -> Vec<UsageRecord> {
    let mut out: Vec<UsageRecord> = Vec::new();
    for meta in list(data_dir) {
        let Some(s) = load(data_dir, &meta.id) else {
            continue;
        };
        for m in &s.messages {
            if m.role != "assistant" {
                continue;
            }
            // Option<TokenUsage> 是 Copy，这里直接拷贝出来，不用借引用
            let Some(u) = m.usage else {
                continue;
            };
            if u.is_zero() {
                continue;
            }
            out.push(UsageRecord {
                at: m.at,
                model: m.model.clone().unwrap_or_default(),
                session: s.id.clone(),
                usage: u,
            });
        }
    }
    out.sort_by_key(|r| r.at);
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("orbcat_sess_{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 「退出不丢轮」：开跑即落盘 —— 提问与 partial 占位必须在 agent 跑之前就存在。
    ///
    /// 这是 2026-09-26 用户报「退出后整轮清空」的回归测试（实证：卡死那轮的
    /// `s1790400662188-0.json` 是空的 `messages: []`）。
    #[test]
    fn begin_turn_persists_question_and_placeholder() {
        let d = tmp("begin_turn");
        begin_turn(&d, "帮我跑个长命令", &[]).unwrap();

        // 模拟"进程在产出任何内容前被杀" → 直接重新读盘
        let s = ensure_current(&d);
        assert_eq!(s.messages.len(), 2, "应落 [提问, partial 占位] 两条");
        assert_eq!(s.messages[0].role, "user");
        assert_eq!(s.messages[0].text, "帮我跑个长命令");
        assert!(!s.messages[0].partial, "用户消息不是占位");
        assert_eq!(s.messages[1].role, "assistant");
        assert!(s.messages[1].partial, "助手占位必须带 partial 标记");
        assert!(
            s.messages[1].text.is_empty() && s.messages[1].steps.is_empty(),
            "占位开始时没有内容"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 轮内 checkpoint：把已产出的步骤/思考写进那条占位，且**不新增消息**。
    #[test]
    fn checkpoint_turn_updates_placeholder_in_place() {
        let d = tmp("checkpoint");
        begin_turn(&d, "问题", &[]).unwrap();

        let steps = vec![
            StoredStep {
                kind: "reasoning".into(),
                name: None,
                detail: "先看看情况".into(),
            },
            StoredStep {
                kind: "tool_call".into(),
                name: Some("run_command".into()),
                detail: "{\"command\":\"ping\"}".into(),
            },
        ];
        checkpoint_turn(&d, &steps, "半截正文", Some("先看看情况")).unwrap();

        let s = ensure_current(&d);
        assert_eq!(s.messages.len(), 2, "checkpoint 不能新增消息");
        let last = s.messages.last().unwrap();
        assert!(last.partial, "还没定稿");
        assert_eq!(last.steps.len(), 2, "步骤已增量落盘");
        assert_eq!(last.reasoning.as_deref(), Some("先看看情况"));
        assert_eq!(last.text, "半截正文");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 定稿：`append_run` 要把占位**换成**真正的助手消息，且**不重复**写用户消息。
    #[test]
    fn append_run_finalizes_placeholder_without_duplicating_user() {
        let d = tmp("finalize");
        begin_turn(&d, "问题", &[]).unwrap();
        let steps = vec![StoredStep {
            kind: "reasoning".into(),
            name: None,
            detail: "想了想".into(),
        }];
        checkpoint_turn(&d, &steps, "", Some("想了想")).unwrap();

        append_run(
            &d,
            &TurnRecord {
                user_text: "问题",
                images: &[],
                steers: &[],
                answer: "最终答案",
                reasoning: Some("想了想"),
                steps: &steps,
                interrupted: false,
                model: "test-model",
                usage: crate::llm::TokenUsage::default(),
            },
        )
        .unwrap();

        let s = ensure_current(&d);
        assert_eq!(s.messages.len(), 2, "定稿后仍只有 [提问, 助手] 两条，不能重复");
        assert_eq!(s.messages[0].role, "user");
        assert_eq!(s.messages[0].text, "问题");
        let a = &s.messages[1];
        assert_eq!(a.role, "assistant");
        assert!(!a.partial, "定稿后必须清掉 partial 标记");
        assert_eq!(a.text, "最终答案");
        assert_eq!(a.steps.len(), 1, "步骤保留");
        assert_eq!(a.model.as_deref(), Some("test-model"));

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 没走 `begin_turn` 的老路径（直接 append_run）仍要自己补用户消息。
    #[test]
    fn append_run_without_placeholder_still_writes_user() {
        let d = tmp("no_placeholder");
        let _ = ensure_current(&d);
        append_run(
            &d,
            &TurnRecord {
                user_text: "直接一轮",
                images: &[],
                steers: &[],
                answer: "答",
                reasoning: None,
                steps: &[],
                interrupted: false,
                model: "m",
                usage: crate::llm::TokenUsage::default(),
            },
        )
        .unwrap();

        let s = ensure_current(&d);
        assert_eq!(s.messages.len(), 2, "用户 + 助手");
        assert_eq!(s.messages[0].text, "直接一轮");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 切走会话、后台继续跑：落盘必须回**开跑的那个**会话，而不是"当前会话"。
    ///
    /// 2026-09-26 用户拍板「切走 = 后台继续跑」。回归点：`begin_turn` /
    /// `checkpoint_turn` / `append_run` 的老版本走 `ensure_current`，中途切走会把
    /// 这一轮的回答写进**另一个**会话。
    #[test]
    fn pinned_session_writes_survive_switch_away() {
        let d = tmp("pinned_switch");
        let a = ensure_current(&d); // A：开跑的那个会话
        let b = new_session(&d); // B：用户中途切过去的会话（new_session 会 set_current）

        // 开跑：显式钉在 A —— 此刻 current 其实已经是 B
        begin_turn_in(&d, Some(&a.id), "在 A 里问", &[]).unwrap();
        checkpoint_turn_in(&d, Some(&a.id), &[], "半截答案", None).unwrap();
        append_run_in(
            &d,
            Some(&a.id),
            &TurnRecord {
                user_text: "在 A 里问",
                images: &[],
                steers: &[],
                answer: "A 的最终答案",
                reasoning: None,
                steps: &[],
                interrupted: false,
                model: "m",
                usage: crate::llm::TokenUsage::default(),
            },
        )
        .unwrap();

        // A 拿到完整一轮；B 一个字都没多
        let sa = load(&d, &a.id).unwrap();
        assert_eq!(
            sa.messages.len(),
            2,
            "A 应是 用户+助手 两条（占位被定稿替换，不重复），实际 {}",
            sa.messages.len()
        );
        assert_eq!(sa.messages[0].text, "在 A 里问");
        assert_eq!(sa.messages[1].text, "A 的最终答案");
        assert!(!sa.messages[1].partial, "append_run_in 应定稿（清掉 partial）");

        let sb = load(&d, &b.id).unwrap();
        assert!(sb.messages.is_empty(), "B 不该被写进任何东西");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 不传 `session_id`（老路径 / 分叉等）→ 退回"当前会话"，行为与改动前一致。
    #[test]
    fn append_run_in_none_falls_back_to_current() {
        let d = tmp("pin_fallback");
        let cur = ensure_current(&d);
        append_run_in(
            &d,
            None,
            &TurnRecord {
                user_text: "老路径",
                images: &[],
                steers: &[],
                answer: "答",
                reasoning: None,
                steps: &[],
                interrupted: false,
                model: "m",
                usage: crate::llm::TokenUsage::default(),
            },
        )
        .unwrap();

        let s = load(&d, &cur.id).unwrap();
        assert_eq!(s.id, cur.id, "None 应写回当前会话");
        assert_eq!(s.messages.len(), 2);

        // 指到一个不存在的会话 id 也要退回当前会话，不能把内容弄丢
        append_run_in(
            &d,
            Some("s-not-exist"),
            &TurnRecord {
                user_text: "坏 id",
                images: &[],
                steers: &[],
                answer: "答2",
                reasoning: None,
                steps: &[],
                interrupted: false,
                model: "m",
                usage: crate::llm::TokenUsage::default(),
            },
        )
        .unwrap();
        let s2 = load(&d, &cur.id).unwrap();
        assert_eq!(s2.messages.len(), 4, "坏 id 应退回当前会话追加一轮");

        let _ = std::fs::remove_dir_all(&d);
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

    /// 分叉：只带**下一轮的上下文窗口**，且**不会被 unique_main 降级**。
    ///
    /// 这条回归针对一个真实存在的坑：`unique_main` 原来对所有非 main 一律写成
    /// task，而 `ensure_current` 每次调用都会跑它 —— 分叉会话在下一次任何会话
    /// 操作后就会变成普通任务会话。所以要断言**磁盘上**的 kind，不只是内存返回值。
    #[test]
    fn fork_copies_prefix_and_is_not_demoted_by_unique_main() {
        let d = tmp("fork_survives");
        let main = ensure_current(&d);
        append_turn(&d, "第一问", &[], "第一答", &[]).unwrap();
        append_turn(&d, "第二问", &[], "第二答", &[]).unwrap();

        let src = load(&d, &main.id).unwrap();
        assert_eq!(src.messages.len(), 4);

        // 下标 2 = "第二问"（user）→ 不能作为分叉点；下标 3 = "第二答"（assistant）
        let f = fork(&d, &src, 3).unwrap();
        assert_eq!(f.kind, kind::FORK);
        assert_eq!(f.messages.len(), 4, "4 条都在窗口里");
        assert_eq!(f.messages.last().unwrap().text, "第二答", "分叉点自己必须在窗口里");
        assert_eq!(f.forked_from.as_deref(), Some(main.id.as_str()));
        assert_eq!(f.fork_at, Some(3));
        assert!(f.title.starts_with('⑂'), "标题要能一眼认出是分支");
        // 分叉后自动切过去
        assert_eq!(current_id(&d).as_deref(), Some(f.id.as_str()));

        // 连调两次 unique_main（模拟后续任何会话操作）
        let _ = unique_main(&d);
        let _ = unique_main(&d);
        let on_disk = load(&d, &f.id).unwrap();
        assert_eq!(on_disk.kind, kind::FORK, "❌ 分叉会话被降级成任务会话了");
        assert!(
            list(&d).iter().any(|m| m.id == f.id && m.kind == kind::FORK),
            "列表里也要还是 fork"
        );
        // 分叉会话是可删的普通会话
        assert!(delete(&d, &f.id).is_ok());

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **分叉带的是"下一轮会喂给模型的窗口"，不是全部历史**（用户 2026-09-20 修正）。
    ///
    /// 造 30 条：前 20 条是"很久以前"（时间窗外），后 10 条是刚聊的。
    /// 分叉点取第 29 条（assistant 回复）→ 只应带最近 10 条，老消息一条都不进。
    #[test]
    fn fork_takes_only_the_injected_window() {
        let d = tmp("fork_window");
        let main = ensure_current(&d);
        let mut s = load(&d, &main.id).unwrap();
        let now = now_ms();
        for i in 0..30u64 {
            // 前 20 条 100 小时前（时间窗外、且超出最近 10 条）
            let at = if i < 20 { now - 100 * 3600 * 1000 } else { now - 60_000 };
            s.messages.push(StoredMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                text: format!("m{i}"),
                images: Vec::new(),
                steps: Vec::new(),
                reasoning: None,
                interrupted: false,
                partial: false,
                steer_id: None,
                model: None,
                usage: None,
                at,
            });
        }
        write_session(&d, &s).unwrap();

        let src = load(&d, &main.id).unwrap();
        assert_eq!(src.messages.len(), 30);
        // 下标 29 是 assistant（奇数）= 完整轮次边界
        let f = fork(&d, &src, 29).unwrap();
        assert_eq!(
            f.messages.len(),
            10,
            "只带最近 10 条（时间窗外的老消息不进分叉）"
        );
        assert_eq!(f.messages.first().unwrap().text, "m20", "窗口起点");
        assert_eq!(f.messages.last().unwrap().text, "m29", "分叉点收尾");
        assert!(
            f.messages.iter().all(|m| m.text != "m0" && m.text != "m10"),
            "窗外老消息一条都不该被复制"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 「删除这条及之后」：把 messages 截成 `[..upto]`，并**写回磁盘**（不是只改内存）。
    #[test]
    fn truncate_drops_this_and_everything_after() {
        let d = tmp("truncate");
        let main = ensure_current(&d);
        let _ = switch(&d, &main.id);
        for i in 0..5 {
            append_turn(&d, &format!("q{i}"), &[], &format!("a{i}"), &[]).unwrap();
        }
        assert_eq!(load(&d, &main.id).unwrap().messages.len(), 10, "5 问 5 答");

        // 从第 4 条（q2）起删 → 只剩 q0/a0/q1/a1
        let s = truncate(&d, &main.id, 4).unwrap();
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages.last().unwrap().text, "a1", "保留最后一条是 a1");

        // ⚠️ 必须是**落盘的**：重载后还是 4 条，否则重启就复活了
        let back = load(&d, &main.id).unwrap();
        assert_eq!(back.messages.len(), 4);
        assert!(
            back.messages.iter().all(|m| m.text != "q2" && m.text != "a4"),
            "被删的消息一条都不该留下"
        );

        // 下标越界 → 夹到末尾，等于只删最后一条（不能误删更多）
        let s2 = truncate(&d, &main.id, 999).unwrap();
        assert_eq!(s2.messages.len(), 3);
        // 删空：upto = 0
        let s3 = truncate(&d, &main.id, 0).unwrap();
        assert!(s3.messages.is_empty(), "upto=0 应清空整个会话");
        assert!(truncate(&d, &main.id, 0).is_err(), "空会话再删要报错，不能静默成功");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 分叉的准入规则：只有主聊天能分叉。
    #[test]
    fn fork_rejects_non_main() {
        let d = tmp("fork_reject");
        let main = unique_main(&d);
        let _ = switch(&d, &main.id);
        append_turn(&d, "问", &[], "答", &[]).unwrap();

        // ① 任务会话不能分叉
        let t = new_session(&d);
        assert!(fork(&d, &t, 0).is_err(), "任务会话不该能被分叉");

        // ② 分叉出来的会话也不能再分叉（"其他的不行"）
        //    下标 1 = assistant（问/答 之后），是唯一合法分叉点
        let _ = switch(&d, &main.id);
        let src = load(&d, &main.id).unwrap();
        let f = fork(&d, &src, 1).unwrap();
        assert!(fork(&d, &f, 1).is_err(), "分叉出来的会话不该能再分叉");

        // ③ 空的主聊天没有可分叉的点
        let empty = create_main(&d);
        assert!(fork(&d, &empty, 0).is_err(), "没有消息就没有分叉点");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 分叉点必须落在 assistant 完整回复上（落在 user 上，新会话末条是未回答的
    /// 提问，接着发一句会拼成 user→user）。
    #[test]
    fn fork_point_must_be_assistant_message_and_clamps() {
        let d = tmp("fork_point");
        let main = ensure_current(&d);
        append_turn(&d, "问", &[], "答", &[]).unwrap();
        let src = load(&d, &main.id).unwrap();

        // 下标 0 = user → 拒
        assert!(fork(&d, &src, 0).is_err(), "分叉点不能落在用户消息上");
        // 下标 1 = assistant → 合法
        assert!(fork(&d, &src, 1).is_ok());
        // 越界 → 夹到末尾（末尾是 assistant）→ 同样合法
        let _ = switch(&d, &main.id);
        let src = load(&d, &main.id).unwrap();
        assert!(fork(&d, &src, 999).is_ok(), "越界应夹到末尾的 assistant 后可用");

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

    // ---------------- 落盘时间线（cap_steps）----------------

    #[test]
    fn append_turn_keeps_timeline_order_with_capped_details() {
        let d = tmp("cap_store");
        let s = ensure_current(&d);
        let steps = vec![
            StoredStep {
                kind: "reasoning".into(),
                name: None,
                detail: "先看结构".into(),
            },
            StoredStep {
                kind: "tool_call".into(),
                name: Some("read_file".into()),
                detail: format!("{{\"path\":\"a.rs\",\"pad\":\"{}\"}}", "x".repeat(5000)),
            },
            StoredStep {
                kind: "tool_result".into(),
                name: Some("read_file".into()),
                detail: "y".repeat(5000),
            },
            StoredStep {
                kind: "reasoning".into(),
                name: None,
                detail: "再看调用点".into(),
            },
            StoredStep {
                kind: "tool_call".into(),
                name: Some("grep_files".into()),
                detail: "z".repeat(5000),
            },
        ];
        append_turn(&d, "看看", &[], "查了", &steps).unwrap();

        // 单条 detail 必须截断，否则一个 write_file 的整篇正文就进会话文件了
        let on_disk = std::fs::read_to_string(session_file(&d, &s.id)).unwrap();
        assert!(
            !on_disk.contains(&"x".repeat(600)),
            "单条 detail 超上限必须截断（会话是全量 JSON 读写）"
        );

        let after = load(&d, &s.id).unwrap();
        let st = &after.messages[1].steps;
        let kinds: Vec<&str> = st.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "reasoning",
                "tool_call",
                "tool_result",
                "reasoning",
                "tool_call"
            ],
            "落盘必须保留条目与顺序 —— 以前折成一行，回看时步骤就'消失'了"
        );
        assert!(
            st[1].detail.starts_with("{\"path\":\"a.rs\""),
            "参数头部要留着（前端靠它显示路径）: {}",
            st[1].detail
        );
        assert!(st[1].detail.ends_with("（已截断）"), "截断要有说明");
        assert_eq!(st[0].detail, "先看结构", "短条目不该被动");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cap_steps_keeps_only_last_statuses() {
        let mut steps: Vec<StoredStep> = (0..12)
            .map(|i| StoredStep {
                kind: "status".into(),
                name: None,
                detail: format!("限流重试 {i}"),
            })
            .collect();
        steps.push(StoredStep {
            kind: "tool_call".into(),
            name: Some("read_file".into()),
            detail: "{}".into(),
        });

        let out = cap_steps(&steps);
        let statuses: Vec<&str> = out
            .iter()
            .filter(|s| s.kind == "status")
            .map(|s| s.detail.as_str())
            .collect();
        assert_eq!(statuses.len(), STATUS_STEPS_LIMIT, "状态条目只留最后几条");
        assert_eq!(statuses[0], "限流重试 4", "留的是**最后**几条，不是最前几条");
        assert_eq!(out.last().unwrap().kind, "tool_call", "顺序与工具条目都不能丢");

        let empty = cap_steps(&[]);
        assert!(empty.is_empty());
    }

    #[test]
    fn cap_steps_marks_omitted_when_too_many() {
        let steps: Vec<StoredStep> = (0..STEPS_LIMIT + 10)
            .map(|i| StoredStep {
                kind: "tool_call".into(),
                name: Some(format!("tool{i}")),
                detail: "{}".into(),
            })
            .collect();

        let out = cap_steps(&steps);
        assert_eq!(out.len(), STEPS_LIMIT + 1, "超限时补一条省略说明");
        assert_eq!(out[0].kind, "omitted");
        assert!(out[0].detail.contains("10 步"), "要说明省了几步: {}", out[0].detail);
        // 留最近的那批 —— 最后一个必须是最后构造的那条（别写死下标，STEPS_LIMIT 会调）
        let last_name = format!("tool{}", STEPS_LIMIT + 9);
        assert_eq!(
            out.last().unwrap().name.as_deref(),
            Some(last_name.as_str()),
            "留最近的那批"
        );

        // 不超限时不该冒出 omitted
        let few = cap_steps(&steps[..3]);
        assert_eq!(few.len(), 3);
        assert!(few.iter().all(|s| s.kind == "tool_call"));
    }

    /// 思考条目**原文全留**：不截、不丢（2026-09-26 用户拍板"完全不截"）。
    ///
    /// 这是三个症状的回归测试：① 标注字数比真实少 ② 展开内容比标注少
    /// ③ 早期轮次思考块消失 —— 根因都是旧的 8KB 字节预算。
    #[test]
    fn cap_steps_keeps_reasoning_verbatim() {
        // 两条都很长的思考（远超声称的 8KB）+ 一条工具
        let long_a = "思考甲".repeat(4000); // 12000 字 ≈ 36KB，旧预算下必被截
        let long_b = "思考乙".repeat(4000);
        let steps = vec![
            StoredStep {
                kind: "reasoning".into(),
                name: None,
                detail: long_a.clone(),
            },
            StoredStep {
                kind: "reasoning".into(),
                name: None,
                detail: long_b.clone(),
            },
            StoredStep {
                kind: "tool_call".into(),
                name: Some("read_file".into()),
                detail: "{}".into(),
            },
        ];

        let out = cap_steps(&steps);
        let reasons: Vec<&StoredStep> =
            out.iter().filter(|s| s.kind == "reasoning").collect();
        assert_eq!(reasons.len(), 2, "两条思考都要落盘，一条都不能丢");
        assert_eq!(reasons[0].detail, long_a, "思考甲必须逐字一致（不截不丢）");
        assert_eq!(reasons[1].detail, long_b, "思考乙必须逐字一致（不截不丢）");
        assert_eq!(out.last().unwrap().kind, "tool_call", "工具条目照常保留");
    }

    /// 工具条目仍然限长（思考放开了，不代表别的也放开）。
    #[test]
    fn cap_steps_still_caps_tool_detail() {
        let steps = vec![StoredStep {
            kind: "tool_result".into(),
            name: Some("read_file".into()),
            detail: "x".repeat(STEP_DETAIL_LIMIT + 100),
        }];
        let out = cap_steps(&steps);
        assert!(
            out[0].detail.ends_with("（已截断）"),
            "工具结果仍按 STEP_DETAIL_LIMIT 截断: {}",
            out[0].detail
        );
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
                reasoning: None,
                interrupted: false,
                partial: false,
                steer_id: None,
                model: None,
                usage: None,
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

    // ---- 临时授权（「整个任务」档落盘）----

    #[test]
    fn grants_roundtrip_and_delete_cleans_up() {
        let d = tmp("grants_roundtrip");
        let s = new_session(&d);
        assert!(load_grants(&d, &s.id).is_empty(), "没写过就该是空表，不是错误");

        let gs = vec![permission::Grant::lasting(
            r"D:\work",
            permission::Access::ReadWrite,
            permission::GrantTier::Task,
            "整个任务批了写",
        )];
        save_grants(&d, &s.id, &gs).unwrap();

        let back = load_grants(&d, &s.id);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].tier, permission::GrantTier::Task);
        assert_eq!(back[0].access, permission::Access::ReadWrite);
        assert_eq!(back[0].prefix, PathBuf::from(r"D:\work"));

        // 授权文件不能被 list() 当成会话 —— 否则会话列表里会多出幽灵条目
        let ids: Vec<String> = list(&d).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![s.id.clone()], "list 必须跳过 *.grants.json；实际 {ids:?}");

        // 删除会话 → 授权一起走，不留孤儿
        delete(&d, &s.id).unwrap();
        assert!(
            !grants_file(&d, &s.id).exists(),
            "会话删了，「整个任务」档的授权不该留成孤儿"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_empty_grants_removes_file() {
        let d = tmp("grants_empty");
        let s = new_session(&d);

        save_grants(
            &d,
            &s.id,
            &[permission::Grant::once(
                r"D:\a",
                permission::Access::Read,
                "",
            )],
        )
        .unwrap();
        assert!(grants_file(&d, &s.id).exists());

        // 空表 → 删文件，不留空壳
        save_grants(&d, &s.id, &[]).unwrap();
        assert!(!grants_file(&d, &s.id).exists(), "空表应删文件不留空壳");

        // 文件本来就不存在时再存一次空表也不该报错（幂等）
        save_grants(&d, &s.id, &[]).unwrap();

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_grants_file_falls_back_to_empty() {
        // 坏文件按空表处理 —— 方向必须**保守**：无授权 = 回到纯规则判定 = 更严，
        // 绝不能因为文件损坏而多放行什么。
        let d = tmp("grants_corrupt");
        let s = new_session(&d);
        std::fs::create_dir_all(sessions_dir(&d)).unwrap();
        std::fs::write(grants_file(&d, &s.id), "{ 这不是 JSON").unwrap();

        assert!(load_grants(&d, &s.id).is_empty(), "坏文件应退化为空表");

        let _ = std::fs::remove_dir_all(&d);
    }
}
