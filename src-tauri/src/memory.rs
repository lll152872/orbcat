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
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store(tag: &str) -> (MemoryStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("float_agent_mem_{tag}"));
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
}
