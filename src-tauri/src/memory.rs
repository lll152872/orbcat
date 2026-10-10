//! 记忆引擎
//!
//! ## 文件族（全在 agent-data/ 下，人类可读可改，git 可管）
//!
//! ```text
//! agent-data/
//! ├── RULES.md      行为红线（最高优先级，进 system prompt 最前）
//! ├── SOUL.md       人格 / 语气
//! ├── IDENTITY.md   称呼约定
//! ├── USER.md       用户画像（模型可建议，审批后写入）
//! ├── MEMORY.md     长期记忆（审批后的候选都追加到这里）
//! ├── daily/
//! │   └── 2026-09-17.md   每日流水（append-only）
//! └── pending/
//!     ├── candidates.jsonl   待审批的记忆候选
//!     └── rejected.jsonl     驳回记录（留档，方便复盘模型提了什么）
//! ```
//!
//! ## 流转
//!
//! ```text
//! 模型调用 remember 工具 ──→ candidates.jsonl（待审批）
//!                                  │
//!                  用户在面板里批准 ──→ MEMORY.md（进长期记忆，下次对话生效）
//!                                  └─→ rejected.jsonl（驳回留档）
//! ```
//!
//! **为什么要有审批**：模型自己"觉得该记"的东西不一定对/不该记，
//! 让用户把关一道，符合"可逆优先"原则 —— 没批准的永远没进记忆。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 一条待审批的记忆候选
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// 唯一 id：时间戳 + 内容哈希前缀
    pub id: String,
    /// 记忆内容
    pub content: String,
    /// 来源：`model`（模型主动提交）| `user`（用户手写）
    pub source: String,
    /// 提交时间（本地时区，ISO 风格）
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

fn now_string() -> String {
    // 不引入 chrono，用系统时间戳 + 简单格式化
    // （本地时区由 Shell/系统决定，这里只用于展示，精度到秒即可）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 用一个极简的 UTC 格式化（本地时区差异对日记影响可忽略）
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 + days —— 手写闰年太啰嗦，这里直接用标准库的 debug 输出兜底
    let _ = days;
    format!("ts{secs:08}T{h:02}:{mi:02}:{s:02}Z")
}

fn today_file_name() -> String {
    // 本地日期由调用方（Rust 侧无 chrono）……同样用时间戳生成，
    // 86400 秒一天，从 1970-01-01（周四）推算 —— 但这里我们更愿意相信系统：
    // 用 cmd 的 %DATE% 不可靠，干脆用时间戳日期（UTC）。
    // 若与本地日期有偏差，只影响日记文件名，不影响正确性。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // 简化的公历换算（civil_from_days 算法，Howard Hinnant 版）
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}")
}

fn short_hash(s: &str) -> String {
    // FNV-1a 32bit，够用来去重 + 生成短 id
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }

    fn candidates_path(&self) -> PathBuf {
        self.pending_dir().join("candidates.jsonl")
    }

    fn rejected_path(&self) -> PathBuf {
        self.pending_dir().join("rejected.jsonl")
    }

    fn long_term_path(&self) -> PathBuf {
        // ⚠️ 必须与 agent.rs 的 build_system_prompt 读的路径一致
        // （memory/MEMORY.md）。早先这里是 root/MEMORY.md —— 审批通过的写这，
        // 进 prompt 的读那，**批准的记忆永远不生效**，面板也永远显示空。
        self.root.join("memory").join("MEMORY.md")
    }

    fn daily_dir(&self) -> PathBuf {
        self.root.join("daily")
    }

    // -- 日记 ----------------------------------------------------------------

    /// 往今天的日记里追加一条对话记录（append-only）。
    pub fn append_daily(
        &self,
        user_input: &str,
        answer: &str,
        tool_calls: usize,
    ) -> Result<PathBuf, String> {
        let dir = self.daily_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("建日记目录失败: {e}"))?;

        let path = dir.join(format!("{}.md", today_file_name()));

        let q: String = user_input.trim().chars().take(200).collect();
        let a: String = answer.trim().chars().take(300).collect();

        let entry = format!(
            "\n### {time} · {q}\n\n- 工具调用：{tool_calls} 次\n- 回答摘要：{a}\n",
            time = now_string(),
            q = if q.is_empty() { "（仅图片）" } else { &q },
        );

        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("打开日记失败: {e}"))?;
        f.write_all(entry.as_bytes())
            .map_err(|e| format!("写日记失败: {e}"))?;

        Ok(path)
    }

    // -- 候选流转 ------------------------------------------------------------

    /// 提交一条记忆候选（remember 工具 / 用户手写都会走这里）
    pub fn propose(&self, content: &str, source: &str) -> Result<Candidate, String> {
        let content = content.trim().to_string();
        if content.is_empty() {
            return Err("记忆内容不能为空".into());
        }

        let c = Candidate {
            id: format!("{}_{}", now_string(), short_hash(&content)),
            content,
            source: source.to_string(),
            created_at: now_string(),
        };

        std::fs::create_dir_all(self.pending_dir())
            .map_err(|e| format!("建 pending 目录失败: {e}"))?;

        let line = serde_json::to_string(&c).map_err(|e| format!("序列化失败: {e}"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.candidates_path())
            .map_err(|e| format!("打开候选文件失败: {e}"))?;
        writeln!(f, "{line}").map_err(|e| format!("写入候选失败: {e}"))?;

        Ok(c)
    }

    fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, String> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let txt = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
        let mut out = Vec::new();
        for (i, line) in txt.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            out.push(
                serde_json::from_str(line)
                    .map_err(|e| format!("第 {} 行解析失败: {e}", i + 1))?,
            );
        }
        Ok(out)
    }

    fn rewrite_jsonl<T: Serialize>(path: &Path, items: &[T]) -> Result<(), String> {
        let mut buf = String::new();
        for it in items {
            let line = serde_json::to_string(it).map_err(|e| format!("序列化失败: {e}"))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        std::fs::write(path, buf).map_err(|e| format!("写入失败: {e}"))
    }

    /// 列出全部待审批候选
    pub fn list_pending(&self) -> Result<Vec<Candidate>, String> {
        Self::read_jsonl(&self.candidates_path())
    }

    /// 批准一条：从候选队列移除，追加进 MEMORY.md
    pub fn approve(&self, id: &str) -> Result<(), String> {
        let mut all = Self::read_jsonl::<Candidate>(&self.candidates_path())?;
        let Some(pos) = all.iter().position(|c| c.id == id) else {
            return Err(format!("找不到候选 {id}"));
        };
        let c = all.remove(pos);

        // 追加进长期记忆（带来源与日期，方便日后人工整理）
        let lt = self.long_term_path();
        if let Some(parent) = lt.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建 memory 目录失败: {e}"))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lt)
            .map_err(|e| format!("打开 MEMORY.md 失败: {e}"))?;
        let entry = format!("\n- {}（{}）\n", c.content, c.created_at);
        f.write_all(entry.as_bytes())
            .map_err(|e| format!("写入 MEMORY.md 失败: {e}"))?;

        Self::rewrite_jsonl(&self.candidates_path(), &all)
    }

    /// 驳回一条：移到 rejected.jsonl 留档
    pub fn reject(&self, id: &str) -> Result<(), String> {
        let mut all = Self::read_jsonl::<Candidate>(&self.candidates_path())?;
        let Some(pos) = all.iter().position(|c| c.id == id) else {
            return Err(format!("找不到候选 {id}"));
        };
        let c = all.remove(pos);

        std::fs::create_dir_all(self.pending_dir())
            .map_err(|e| format!("建 pending 目录失败: {e}"))?;
        let line = serde_json::to_string(&c).map_err(|e| format!("序列化失败: {e}"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.rejected_path())
            .map_err(|e| format!("打开驳回文件失败: {e}"))?;
        writeln!(f, "{line}").map_err(|e| format!("写入驳回失败: {e}"))?;

        Self::rewrite_jsonl(&self.candidates_path(), &all)
    }

    // -- 项目记忆（决策 5 / 16：projects/<name>/）----------------------------

    fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// 扫 `projects/*/source.ref`，用**最长前缀**匹配给定路径，命中则读该项目 MEMORY.md。
    ///
    /// 返回 `(项目名, MEMORY.md 全文)`。source 目录不存在 / MEMORY 为空 → 不返回。
    /// 多个 source 同时前缀命中时，取**最长**那条（与文件权限同一 specificity 规则）。
    pub fn match_project_memory(&self, paths: &[&Path]) -> Option<(String, String)> {
        let dir = self.projects_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return None;
        };

        let norm = |p: &Path| -> PathBuf {
            let c = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            crate::permission::strip_verbatim(c)
        };

        let mut best: Option<(usize, String, String)> = None; // (source_len, name, memory)

        for ent in entries.flatten() {
            if !ent.path().is_dir() {
                continue;
            }
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "README.md" {
                continue;
            }
            let source_ref = ent.path().join("source.ref");
            let Ok(src) = std::fs::read_to_string(&source_ref) else {
                continue;
            };
            let src = src.trim();
            if src.is_empty() {
                continue;
            }
            let src_path = Path::new(src);
            // 懒检测：源路径已删 → 不注入（避免给模型灌过期项目记忆）
            if !src_path.exists() {
                continue;
            }
            let src_cmp = norm(src_path);

            let mem_path = ent.path().join("MEMORY.md");
            let Ok(mem) = std::fs::read_to_string(&mem_path) else {
                continue;
            };
            let mem = mem.trim().to_string();
            if mem.is_empty() {
                continue;
            }

            for p in paths {
                if p.as_os_str().is_empty() {
                    continue;
                }
                let pc = norm(p);
                if pc.starts_with(&src_cmp) {
                    let len = src_cmp.as_os_str().len();
                    if best.as_ref().map(|(l, _, _)| len > *l).unwrap_or(true) {
                        best = Some((len, name.clone(), mem.clone()));
                    }
                }
            }
        }

        best.map(|(_, name, mem)| (name, mem))
    }

    /// 列出全部项目：`(名字, source.ref 原文, 源路径是否仍存在, MEMORY 是否非空)`
    pub fn list_projects(&self) -> Vec<(String, String, bool, bool)> {
        let dir = self.projects_dir();
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for ent in entries.flatten() {
            if !ent.path().is_dir() {
                continue;
            }
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let src = std::fs::read_to_string(ent.path().join("source.ref"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let alive = !src.is_empty() && Path::new(&src).exists();
            let has_mem = std::fs::read_to_string(ent.path().join("MEMORY.md"))
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            out.push((name, src, alive, has_mem));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 改/清项目的 `source.ref`（面板「改路径」用）。
    ///
    /// `path` 为空 = **解绑**：删掉 source.ref（该项目从此显示「未绑定路径」，
    /// 不再参与前台路径注入 —— 与"路径死了不注入"同一懒检测语义）。
    pub fn set_project_source(&self, name: &str, path: &str) -> Result<(), String> {
        let name = name.trim();
        // 名字要能安全拼进路径：拒绝空、隐藏目录、路径分隔与穿越
        if name.is_empty()
            || name.starts_with('.')
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
        {
            return Err("非法项目名".into());
        }
        let dir = self.projects_dir().join(name);
        if !dir.is_dir() {
            return Err(format!("项目「{name}」不存在"));
        }
        let ref_path = dir.join("source.ref");
        let path = path.trim();
        if path.is_empty() {
            let _ = std::fs::remove_file(&ref_path);
        } else {
            std::fs::write(&ref_path, path).map_err(|e| format!("写 source.ref 失败: {e}"))?;
        }
        Ok(())
    }

    /// 读全局长期记忆 MEMORY.md 全文（自动蒸馏查重用；读不到 = 空串）。
    pub fn long_term_md(&self) -> Result<String, String> {
        let p = self.long_term_path();
        if !p.exists() {
            return Ok(String::new());
        }
        std::fs::read_to_string(&p).map_err(|e| format!("读 MEMORY.md 失败: {e}"))
    }

    /// 归档一个项目记忆目录（面板「归档」用）。
    ///
    /// ⚠️ **绝不硬删**（RULES.md 第二节绝对红线）：整个目录改名挪进
    /// `agent-data/.trash/projects-<名>-<ts>/`，来路可查、手工可还原。
    /// 返回归档后的路径。
    pub fn archive_project(&self, name: &str) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty()
            || name.starts_with('.')
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
        {
            return Err("非法项目名".into());
        }
        let dir = self.projects_dir().join(name);
        if !dir.is_dir() {
            return Err(format!("项目「{name}」不存在"));
        }
        let trash = self.root.join(".trash");
        std::fs::create_dir_all(&trash).map_err(|e| format!("建 .trash 失败: {e}"))?;
        let dest = trash.join(format!("projects-{}-{}", name, now_string()));
        std::fs::rename(&dir, &dest).map_err(|e| format!("归档失败: {e}"))?;
        Ok(dest.display().to_string())
    }
}

// ---------------------------------------------------------------------------
// 轮末自动蒸馏（2026-10-07 用户定案：不靠模型自觉调 remember）
// ---------------------------------------------------------------------------

/// 自动蒸馏单次写入的条数上限
const AUTO_DISTILL_MAX: usize = 3;
/// 送进提取请求的正文截断（按字符）
const AUTO_DISTILL_INPUT_CHARS: usize = 1500;
const AUTO_DISTILL_ANSWER_CHARS: usize = 2500;
/// 本轮「提问 + 回答」合计低于这个字数就不值得发提取请求（寒暄省一次调用）
pub const AUTO_DISTILL_MIN_CHARS: usize = 60;

/// 从一轮问答里自动提取记忆条目并落库，返回写入条数。
///
/// ## 路由（与 `remember` 工具同一条规则）
/// - 激活项目 → **直写** `projects/<名>/MEMORY.md`（source=auto）——
///   项目内 remember 本来就是直写，不新增审批面；
/// - 主对话 → 进**全局待审批候选**（source=auto）—— 全局记忆保留审批关。
///
/// ## 契约
/// - 调用方（lib.rs::chat 收尾）负责把它丢进后台任务：不阻塞本轮返回、
///   失败只打日志 —— 蒸馏挂了不能影响对话本身。
/// - 查重见 [`store_auto_entries`]：反复聊同一句约定不该每次都进一遍。
pub async fn auto_distill(
    cfg: &crate::config::ModelConfig,
    data_dir: &Path,
    user_text: &str,
    answer: &str,
) -> Result<usize, String> {
    let q: String = user_text.trim().chars().take(AUTO_DISTILL_INPUT_CHARS).collect();
    let a: String = answer.trim().chars().take(AUTO_DISTILL_ANSWER_CHARS).collect();
    if q.chars().count() + a.chars().count() < AUTO_DISTILL_MIN_CHARS {
        return Ok(0);
    }

    let prompt = format!(
        "从下面这轮「用户与 AI 助手」的对话中，提取值得跨会话长期记住的事实。\n\n\
         只提取这几类：用户明确说过的偏好/称呼/工作习惯；项目路径与结构等稳定事实；\
         用户确认过的决策与约定；踩过的坑与验证过的结论（要能复用）。\n\
         不要提取：寒暄闲聊、临时状态、一次性的具体操作、未经用户确认的推断。\n\
         每条不超过 120 字，写成独立成立的事实句（不要\"用户说\"这类话术）。\n\
         没有值得记的就输出空数组。\n\n\
         只输出一个 JSON 字符串数组，例如 [\"条目一\",\"条目二\"]，不要任何解释。\n\n\
         --- 对话开始 ---\n【用户】{q}\n\n【助手】{a}\n--- 对话结束 ---"
    );

    let messages = vec![crate::llm::ChatMessage::user(prompt)];
    let out = crate::llm::chat(cfg, messages, None).await?;
    let items = parse_json_array(&out.text())?;
    store_auto_entries(data_dir, &items)
}

/// 把提取出的条目按当前项目状态落库（查重后写入），返回写入条数。
///
/// 同文判定 = trim 后逐字相等 / 目标文件已包含 —— 够用且零依赖；
/// 误杀率可忽略（记忆条目本来就短且具体）。
fn store_auto_entries(data_dir: &Path, items: &[String]) -> Result<usize, String> {
    let items: Vec<&str> = items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .take(AUTO_DISTILL_MAX)
        .collect();
    if items.is_empty() {
        return Ok(0);
    }

    // 路由：激活项目 → 直写项目 MEMORY.md
    let settings = crate::config::load_settings(data_dir);
    if let Some(pid) = settings.active_project_id {
        if let Ok(all) = crate::pmem::list_projects(data_dir) {
            if let Some(p) = all.iter().find(|x| x.id == pid) {
                let mut existing =
                    crate::pmem::project_memory_md(data_dir, &p.name).unwrap_or_default();
                let mut written = 0usize;
                for it in &items {
                    if existing.contains(it) {
                        continue;
                    }
                    crate::pmem::append_project_memory(data_dir, &p.name, it, "auto")?;
                    // ⚠️ 同轮内的第二条同文也要拦住：existing 是查重基准，
                    // 写入后必须同步追加，否则上面的 contains 永远查不到刚写的。
                    existing.push('\n');
                    existing.push_str(it);
                    written += 1;
                }
                return Ok(written);
            }
        }
    }

    // 主对话 → 全局待审批候选（保留审批关）
    let store = MemoryStore::new(data_dir);
    let pending = store.list_pending().unwrap_or_default();
    let lt = store.long_term_md().unwrap_or_default();
    let mut written = 0usize;
    for it in &items {
        if pending.iter().any(|c| c.content.trim() == *it) {
            continue;
        }
        if lt.contains(it) {
            continue;
        }
        store.propose(it, "auto")?;
        written += 1;
    }
    Ok(written)
}

/// 从模型输出里抠 JSON 字符串数组（容忍 ```json 包裹 / 前后废话）。
fn parse_json_array(text: &str) -> Result<Vec<String>, String> {
    let s = text.trim();
    let start = s
        .find('[')
        .ok_or_else(|| "输出里没有 JSON 数组".to_string())?;
    let end = s
        .rfind(']')
        .ok_or_else(|| "输出里没有数组结束符".to_string())?;
    if end < start {
        return Err("JSON 数组区间不合法".into());
    }
    serde_json::from_str::<Vec<String>>(&s[start..=end])
        .map_err(|e| format!("解析 JSON 数组失败: {e}"))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store(tag: &str) -> (MemoryStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("orbcat_mem_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (MemoryStore::new(&dir), dir)
    }

    #[test]
    fn daily_appends_and_groups_by_day() {
        let (s, dir) = fresh_store("daily");
        s.append_daily("你好", "你好呀", 0).unwrap();
        s.append_daily("看看目录", "列出来了", 1).unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.join("daily")).unwrap().collect();
        assert_eq!(files.len(), 1, "同一天应只有一个日记文件");

        let txt = std::fs::read_to_string(dir.join("daily").join(format!("{}.md", today_file_name())))
            .unwrap();
        assert!(txt.contains("你好"));
        assert!(txt.contains("列出来了"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn propose_then_approve_moves_into_long_term() {
        let (s, dir) = fresh_store("approve");

        let c = s.propose("用户的常用项目在 D:\\myword", "model").unwrap();
        assert_eq!(s.list_pending().unwrap().len(), 1);

        s.approve(&c.id).unwrap();
        assert!(s.list_pending().unwrap().is_empty(), "批准后候选应清空");

        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(mem.contains("D:\\myword"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reject_keeps_archive() {
        let (s, dir) = fresh_store("reject");

        let c = s.propose("不该记的东西", "model").unwrap();
        s.reject(&c.id).unwrap();

        assert!(s.list_pending().unwrap().is_empty());
        let rej = std::fs::read_to_string(dir.join("pending").join("rejected.jsonl")).unwrap();
        assert!(rej.contains("不该记的东西"));
        assert!(!dir.join("memory").join("MEMORY.md").exists(), "驳回不应写入长期记忆");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_content_is_rejected() {
        let (s, dir) = fresh_store("empty");
        assert!(s.propose("   ", "model").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_unique_for_different_content() {
        let (s, dir) = fresh_store("ids");
        let a = s.propose("内容A", "model").unwrap();
        let b = s.propose("内容B", "model").unwrap();
        assert_ne!(a.id, b.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    // 这些用例硬编码了 Windows 路径（`D:\…`）与 Windows 的大小写不敏感语义，
    // 在 macOS/Linux 上 `D:\` 只是一个相对文件名，断言必然失败 —— 不是产品 bug。
    // 2026-10-10 由 macos-14 runner 抓出。
    #[test]
    fn project_memory_longest_prefix_wins() {
        let (s, dir) = fresh_store("proj");
        let root = std::env::temp_dir().join(format!("fa_proj_src_{}", std::process::id()));
        let deep = root.join("sub");
        std::fs::create_dir_all(&deep).unwrap();

        let mk = |name: &str, src: &Path, mem: &str| {
            let p = dir.join("projects").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("source.ref"), src.display().to_string()).unwrap();
            std::fs::write(p.join("MEMORY.md"), mem).unwrap();
        };
        mk("outer", &root, "外层项目记忆");
        mk("inner", &deep, "内层项目记忆");

        let hit = s
            .match_project_memory(&[&deep.join("file.txt")])
            .expect("应命中项目记忆");
        assert_eq!(hit.0, "inner");
        assert!(hit.1.contains("内层项目记忆"));

        let hit2 = s
            .match_project_memory(&[&root.join("other.txt")])
            .expect("应命中外层");
        assert_eq!(hit2.0, "outer");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn project_memory_missing_source_or_empty_mem_is_skipped() {
        let (s, dir) = fresh_store("proj_empty");
        let p = dir.join("projects").join("ghost");
        std::fs::create_dir_all(&p).unwrap();
        // source.ref 指向不存在路径 → 即使 MEMORY 非空也不注入（懒检测语义）
        std::fs::write(p.join("source.ref"), r"D:\definitely-not-here-xyz").unwrap();
        std::fs::write(p.join("MEMORY.md"), "不该出现").unwrap();
        assert!(s.match_project_memory(&[Path::new(r"D:\definitely-not-here-xyz\a")]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 轮末自动蒸馏 ----

    #[test]
    fn parse_json_array_tolerates_wrapping_and_noise() {
        assert_eq!(parse_json_array(r#"["a","b"]"#).unwrap(), vec!["a", "b"]);
        // ```json 包裹
        assert_eq!(
            parse_json_array("```json\n[\"条目\"]\n```").unwrap(),
            vec!["条目"]
        );
        // 前后有废话
        assert_eq!(
            parse_json_array("提取结果如下：\n[\"x\"]\n以上。").unwrap(),
            vec!["x"]
        );
        // 空数组
        assert_eq!(parse_json_array("[]").unwrap(), Vec::<String>::new());
        // 没有数组 → 报错（不 panic）
        assert!(parse_json_array("我觉得没什么好记的").is_err());
    }

    /// 激活项目 → 直写项目 MEMORY.md；已有同文跳过
    #[test]
    fn store_auto_entries_routes_to_active_project_with_dedup() {
        let (s, dir) = fresh_store("auto_proj");
        let p = crate::pmem::create_project(&dir, "项目甲", "").unwrap();
        let mut st = crate::config::load_settings(&dir);
        st.active_project_id = Some(p.id);
        crate::config::save_settings(&dir, &st).unwrap();

        let n = store_auto_entries(
            &dir,
            &["构建用 cargo build（dev）".to_string(), "构建用 cargo build（dev）".to_string()],
        )
        .unwrap();
        assert_eq!(n, 1, "同文两条只写一条: {n}");
        let mem = crate::pmem::project_memory_md(&dir, "项目甲").unwrap();
        assert!(mem.contains("cargo build"), "应直写项目记忆: {mem}");
        assert!(mem.contains("auto"), "来源应标 auto: {mem}");
        assert!(
            s.list_pending().unwrap().is_empty(),
            "有项目时不进全局候选"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 主对话 → 进全局待审批候选；与已有候选/长期记忆同文跳过
    #[test]
    fn store_auto_entries_routes_to_pending_in_main_chat() {
        let (s, dir) = fresh_store("auto_main");

        // 先塞一条候选 + 一条已在长期记忆
        s.propose("已有候选甲", "model").unwrap();
        s.approve(&s.list_pending().unwrap()[0].id).unwrap();
        // （approve 后 MEMORY.md 里已有「已有候选甲」）

        let n = store_auto_entries(
            &dir,
            &[
                "已有候选甲".to_string(),   // 与长期记忆同文 → 跳过
                "新事实乙".to_string(),     // 新 → 写入
            ],
        )
        .unwrap();
        assert_eq!(n, 1, "只应写入新的一条: {n}");
        let pending = s.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].content, "新事实乙");
        assert_eq!(pending[0].source, "auto", "来源应标 auto");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空条目 / 纯空白全部过滤
    #[test]
    fn store_auto_entries_ignores_blanks() {
        let (_s, dir) = fresh_store("auto_blank");
        let n = store_auto_entries(&dir, &["  ".to_string(), String::new()]).unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
