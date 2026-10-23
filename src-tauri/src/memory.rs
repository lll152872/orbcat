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
    /// 来源：`model`（模型主动提交）| `auto`（轮末自动蒸馏）| `user`（用户手写）
    pub source: String,
    /// 提交时间（本地时区，ISO 风格）
    pub created_at: String,
    /// **触发轮次**：这条记忆是从哪一句用户提问里诞生的（截断后的用户原话）。
    ///
    /// 2026-10-23 加（设计稿 06「触发轮次」）：审批页要能回答
    /// 「这条到底是从哪句话里冒出来的」—— 没有它就只剩一句孤零零的结论，
    /// 用户批不批全靠猜。
    ///
    /// 老数据没这个字段 → `serde(default)` 读成 `None`，前端回退到占位文案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
    /// **驳回理由**（只写进 `rejected.jsonl`，不进 `candidates.jsonl`）。
    ///
    /// 2026-10-15 加：以前驳回只把条目搬走、不留理由，于是过几天问
    /// 「这条当初为什么被打回」只能靠翻聊天记录 —— 而 rejected.jsonl
    /// 本来就是**留档**，留内容不��理由等于留一半。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
    /// 驳回时间（同上格式），用来分清「提交于何时 / 否于何时」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_at: Option<String>,
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

/// 纯词法规整：消解 `..` 与 `.`（不碰文件系统）。
///
/// 只在 [`MemoryStore::canonicalize_with_missing_tail`] 找不到任何存在祖先时兜底使用。
fn lexical_clean(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // 只有前一个也是普通组件时才回退，避免把 `..` 弹成根
                match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(c),
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
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

    /// **已批准留档**（`pending/approved.jsonl`）。
    ///
    /// 为什么单独留一份：审批页底部要显示「N / M」（待批 / 候选总数），
    /// 而「已批准多少条」从 MEMORY.md 里数不出来 —— 那个文件里还有
    /// 手写内容，还有蒸馏搬过去的条目，混在一起统计就是假账。
    /// 留一份只追加不删的台账，数字才真。
    fn approved_path(&self) -> PathBuf {
        self.pending_dir().join("approved.jsonl")
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
        self.propose_with_ref(content, source, None)
    }

    /// 提交一条记忆候选，并记下**触发轮次**（用户原话，截断过）。
    ///
    /// `turn_ref` 为空串视作「没有」—— 免得调用方为了「没有」还得构造 `None`。
    pub fn propose_with_ref(
        &self,
        content: &str,
        source: &str,
        turn_ref: Option<&str>,
    ) -> Result<Candidate, String> {
        let content = content.trim().to_string();
        if content.is_empty() {
            return Err("记忆内容不能为空".into());
        }

        let turn_ref = turn_ref
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(40).collect::<String>());

        let c = Candidate {
            id: format!("{}_{}", now_string(), short_hash(&content)),
            content,
            source: source.to_string(),
            created_at: now_string(),
            turn_ref,
            rejected_reason: None,
            rejected_at: None,
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

    /// 追加一条到「已批准留档」（`pending/approved.jsonl`）。
    fn log_approved(&self, c: &Candidate) -> Result<(), String> {
        std::fs::create_dir_all(self.pending_dir())
            .map_err(|e| format!("建 pending 目录失败: {e}"))?;
        let line = serde_json::to_string(c).map_err(|e| format!("序列化失败: {e}"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.approved_path())
            .map_err(|e| format!("打开批准留档失败: {e}"))?;
        writeln!(f, "{line}").map_err(|e| format!("写入批准留档失败: {e}"))
    }

    /// 审批页底部的统计：**（待批数, 已处理数）**。
    ///
    /// 「已处理」= 已批准 + 已驳回。两者都从**只追加的台账**数，
    /// 不从 MEMORY.md 反推 —— 那个文件里还有手写内容和蒸馏搬过去的条目，
    /// 混进来统计就是假账。
    pub fn stats(&self) -> Result<(usize, usize), String> {
        let pending = Self::read_jsonl::<Candidate>(&self.candidates_path())?.len();
        let approved = Self::read_jsonl::<Candidate>(&self.approved_path())?.len();
        let rejected = Self::read_jsonl::<Candidate>(&self.rejected_path())?.len();
        Ok((pending, approved + rejected))
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
        f.flush().map_err(|e| format!("落盘失败: {e}"))?;

        self.log_approved(&c)?;
        Self::rewrite_jsonl(&self.candidates_path(), &all)
    }

    /// **全部批准**：把当前所有候选一次性写进长期记忆，返回写入条数。
    ///
    /// 设计稿 06 标题条上的「全部批准」。为什么后端做而不是前端循环调
    /// `mem_approve`：前端循环会留下「批了 3 条、剩 2 条失败」的半截状态，
    /// 而这里一次读写、要么全进要么报错。
    pub fn approve_all(&self) -> Result<usize, String> {
        let mut all = Self::read_jsonl::<Candidate>(&self.candidates_path())?;
        if all.is_empty() {
            return Ok(0);
        }

        let lt = self.long_term_path();
        if let Some(parent) = lt.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建 memory 目录失败: {e}"))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lt)
            .map_err(|e| format!("打开 MEMORY.md 失败: {e}"))?;

        let n = all.len();
        let batch: Vec<Candidate> = all.drain(..).collect();
        for c in &batch {
            let entry = format!("\n- {}（{}）\n", c.content, c.created_at);
            f.write_all(entry.as_bytes())
                .map_err(|e| format!("写入 MEMORY.md 失败: {e}"))?;
        }
        f.flush().map_err(|e| format!("落盘失败: {e}"))?;

        for c in &batch {
            self.log_approved(c)?;
        }
        Self::rewrite_jsonl(&self.candidates_path(), &all)?;
        Ok(n)
    }

    /// **改写**一条候选的内容（仍然留在待审批队列里，等用户点「批准」）。
    ///
    /// 设计稿 06 的「改写」按钮：模型蒸馏出来的句子往往太长或太满，
    /// 用户想收紧措辞再决定要不要写进长期记忆。
    /// 改写**不自动批准** —— 改完还要用户自己点「批准」，两件事分开。
    pub fn rewrite(&self, id: &str, content: &str) -> Result<(), String> {
        let content = content.trim();
        if content.is_empty() {
            return Err("改写内容不能为空".into());
        }
        let mut all = Self::read_jsonl::<Candidate>(&self.candidates_path())?;
        let Some(pos) = all.iter().position(|c| c.id == id) else {
            return Err(format!("找不到候选 {id}"));
        };
        all[pos].content = content.to_string();
        Self::rewrite_jsonl(&self.candidates_path(), &all)
    }

    /// 驳回一条：移到 rejected.jsonl 留档（**连理由一起**）
    ///
    /// `reason` 为空串 = 用户没填 / 老调用方。仍然照常留档，只是没有理由可看。
    pub fn reject(&self, id: &str, reason: &str) -> Result<(), String> {
        let mut all = Self::read_jsonl::<Candidate>(&self.candidates_path())?;
        let Some(pos) = all.iter().position(|c| c.id == id) else {
            return Err(format!("找不到候选 {id}"));
        };
        let mut c = all.remove(pos);
        let reason = reason.trim();
        if !reason.is_empty() {
            c.rejected_reason = Some(reason.to_string());
        }
        c.rejected_at = Some(now_string());

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

    /// 把路径归一化成**唯一形态**，供跨路径比较（项目记忆前缀匹配）使用。
    ///
    /// ## 为什么不能直接用 `canonicalize()`
    ///
    /// `canonicalize()` 要求整条路径**都存在**。而项目记忆匹配的两侧天然不对称：
    /// - `source.ref` 指向的**目录**总是存在的 → canonicalize 成功
    /// - 待匹配的**文件**经常还不存在（用户说「这个项目」时文件可能没建）
    ///   → canonicalize 失败，原样返回
    ///
    /// 两侧走了不同分支，形态就可能不一致。在 Windows 上 `canonicalize()` 还会
    /// 调 `GetFinalPathNameByHandle` 把 **8.3 短名展开成真实长名**
    /// （`RUNNER~1` → `runneradmin`）并加 `\\?\` 前缀，于是「存在侧」被改写、
    /// 「不存在侧」保持原样 → 前缀匹配失败。
    ///
    /// 本地中文机器上两边用户名恰好一致，形态碰巧对上所以永远看不出来；
    /// CI 的英文 runner（`runneradmin` 走短名展开）立刻暴露。
    ///
    /// 真实影响：用户把项目放在 TEMP 等含 8.3 短名的目录下，或匹配一个尚未创建
    /// 的文件时，**项目记忆会静默失效** —— 不是测试问题。
    ///
    /// ## 做法
    ///
    /// 从末端往前找**第一个存在的祖先**，对它 canonicalize，再把剩余部分接回去。
    /// 这样无论整条路径存不存在，得到的都是同一种形态。
    fn canonicalize_with_missing_tail(p: &Path) -> PathBuf {
        // 常见情况：整条路径存在，直接 canonicalize
        if let Ok(c) = p.canonicalize() {
            return crate::permission::strip_verbatim(c);
        }

        // 剥掉尾部的不存在部分，找最长存在前缀
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut cur = p.to_path_buf();
        loop {
            let Some(parent) = cur.parent().map(|x| x.to_path_buf()) else {
                break;
            };
            let Some(name) = cur.file_name().map(|x| x.to_os_string()) else {
                break;
            };
            tail.push(name);
            match parent.canonicalize() {
                Ok(c) => {
                    let mut out = crate::permission::strip_verbatim(c);
                    // 尾部按逆序接回（收集时是从后往前）
                    for seg in tail.iter().rev() {
                        out.push(seg);
                    }
                    return out;
                }
                Err(_) => cur = parent,
            }
        }

        // 连根都不存在（罕见）：原样返回，至少做了 `..` 消解
        crate::permission::strip_verbatim(lexical_clean(p))
    }

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

        // ⚠️ 两侧必须归一化到**同一种形态**，否则前缀比较会失效。
        //
        // 诊断（见同文件 `diag_path_forms_in_ci`）在 CI 上给出了确切答案：
        //   query canon = Err(NotFound)   ← 待匹配的文件**不存在**，canonicalize 失败
        //   root canon  = Ok("\\\\?\\C:\\...\\runneradmin\\...\\fa_diag_76652")
        //
        // 也就是说**两侧的行为本来就不对称**：
        //   - `source.ref` 指向的目录**存在** → canonicalize 成功 → 拿到 8.3 短名展开后的
        //     真实长名（`RUNNER~1` → `runneradmin`）+ `\\?\` 前缀
        //   - 待匹配的**文件不存在**（项目记忆匹配的是「本该在这个目录下的文件」）
        //     → canonicalize 失败 → 原样返回
        // 本地中文机器上两边用户目录名恰好一致，形态碰巧对上所以永远通过；
        // CI 的 `runneradmin` 走短名展开，形态必然不同 → 前缀匹配失败。
        //
        // 这不只是测试问题：真实场景里用户匹配的项目文件常常还没创建，
        // 一样会掉进这个不对称分支，导致项目记忆静默失效。
        //
        // 解法：**统一走「解析存在的祖先 + 拼接剩余部分」**，
        // 不再依赖 canonicalize 整体成功。这样两侧无论存不存在都得到同一形态。
        let norm = |p: &Path| -> PathBuf { Self::canonicalize_with_missing_tail(p) };

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
                // ⚠️ 用 `path_has_prefix` 而不是 `Path::starts_with`：
                //   - 前者是**按组件**比较，`D:\myword` 不会误匹配 `D:\mywordx`
                //   - 前者在 Windows 上**大小写不敏感**（canonicalize 可能产出
                //     与 source.ref 不同大小写的形式）
                //   - 后者只比字符串前缀，上述两种情况都会判错
                if crate::permission::path_has_prefix(&pc, &src_cmp) {
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
// 轮末自动蒸馏（不靠模型自觉调 remember）
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

/// 条目归一化：剥掉时间戳尾注与首部的「XX：」标签，压掉空白与常见标点。
///
/// ## 为什么需要
///
/// 自动蒸馏产出的条目是**改写版**，不是原文照抄：
/// 候选写「论文笔记产出结构：论文笔记存放在 D:\myword\文章\论文\」，
/// 而 MEMORY.md 里的原文是「用户的论文/笔记产出统一放在 D:\myword\文章\ 下的
/// Obsidian 库，论文类笔记固定在 D:\myword\文章\论文\……（ts1791981574T12:39:34Z）」。
/// 原来的 `lt.contains(it)` 是**子串**匹配，这种改写根本挡不住
/// —— 实证：2026-10-15 蒸馏把 4 条刚迁走的记忆又原样提交回候选队列。
fn norm_for_dedup(s: &str) -> String {
    // 去掉行尾的（ts...）/（... · user）/（2026-09-23 ... 验证）这类来源标注
    let mut t = s.to_string();
    while let Some(p) = t.rfind('（') {
        let tail = &t[p..];
        let inner = tail.trim_start_matches('（');
        let looks_like_stamp = inner.starts_with("ts")
            || inner.starts_with("20")
            || inner.contains("·")
            || inner.contains("验证")
            || inner.contains("建立")
            || inner.contains("跑通");
        if tail.ends_with('）') && looks_like_stamp {
            t.truncate(p);
        } else {
            break;
        }
    }
    // 去掉首部「XX：」式标签（候选常带，原文不带）
    //
    // ⚠️ 必须用 `len_utf8()` 而不是 `p + 1`：全角冒号 `：` 是 U+FF1A，
    //    UTF-8 下占 **3 字节**，`p+1` 会落在字符内部 → panic（char boundary）。
    //    这条是 `norm_strips_stamp_prefix_and_punctuation` 当场抓出来的。
    if let Some(p) = t.find('：') {
        if p <= 12 {
            t = t[p + '：'.len_utf8()..].to_string();
        }
    }
    // 只留实义字符：小写化 + 去掉空白与中英文标点
    t.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 两条内容是否算「同一条」。
///
/// 双向子串 + 长度门槛：门槛挡掉「都在讲工具」这种泛泛的撞词
/// （8 个字以下的共性片段谁都命中，那会把真条目也误杀）。
fn same_memory(a: &str, b: &str) -> bool {
    let (na, nb) = (norm_for_dedup(a), norm_for_dedup(b));
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }
    let (short, long) = if na.len() <= nb.len() { (&na, &nb) } else { (&nb, &na) };
    short.chars().count() >= 8 && long.contains(short)
}

/// 把记忆的**全部落点**读成一个比对语料：逐条 bullet 切开。
///
/// ## 为什么不能只比 `memory/MEMORY.md`
///
/// 记忆有五个落点（USER / SOUL / IDENTITY / 中转站 / projects/*）。
/// 查重只扫中转站的话，一条**刚被蒸馏迁进 USER.md** 的记忆就从基准里消失了，
/// 下一轮自动蒸馏会把它原样再提一遍 —— 2026-10-15 的实证：候选队列里
/// 5 条有 4 条是刚迁走的同义内容。
fn dedup_corpus(store: &MemoryStore) -> Vec<String> {
    let mut corpus: Vec<String> = Vec::new();

    for name in ["USER.md", "SOUL.md", "IDENTITY.md", "memory/MEMORY.md"] {
        if let Ok(t) = std::fs::read_to_string(store.root().join(name)) {
            corpus.extend(split_bullets(&t));
        }
    }
    // projects/*/MEMORY.md —— 项目记忆也是正式落点
    if let Ok(rd) = std::fs::read_dir(store.root().join("projects")) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            if let Ok(t) = std::fs::read_to_string(e.path().join("MEMORY.md")) {
                corpus.extend(split_bullets(&t));
            }
        }
    }
    corpus
}

/// md 文本 → 逐条 bullet（`- ` 开头，缩进续行归并），对标 `distill::split_entries`。
///
/// 给 `tools::read_project_memory` 复用（项目记忆要能按条目读、不能只给一大坨）。
pub fn split_bullets_pub(md: &str) -> Vec<String> {
    split_bullets(md)
}

/// md 文本 → 逐条 bullet（`- ` 开头，缩进续行归并），对标 `distill::split_entries`。
fn split_bullets(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    for raw in md.lines() {
        let l = raw.trim_end();
        if let Some(rest) = l.strip_prefix("- ") {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            cur = Some(rest.trim().to_string());
        } else if !l.trim().is_empty() && l.starts_with(char::is_whitespace) {
            if let Some(c) = cur.as_mut() {
                c.push('\n');
                c.push_str(l.trim());
            }
        } else if !l.trim().is_empty() {
            if let Some(c) = cur.take() {
                out.push(c);
            }
        }
    }
    if let Some(c) = cur.take() {
        out.push(c);
    }
    out.retain(|s| !s.trim().is_empty());
    out
}

/// 把提取出的条目按当前项目状态落库（查重后写入），返回写入条数。
///
/// 同文判定见 [`same_memory`]：归一化后双向子串 + 长度门槛，
/// 比对范围见 [`dedup_corpus`]（**全部五个落点**，不只是中转站）。
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
                // 该项目已有的条目也进语料：全局记忆里写过同样的事就别再写一遍
                let corpus = dedup_corpus(&MemoryStore::new(data_dir));
                let mut written = 0usize;
                for it in &items {
                    if split_bullets(&existing)
                        .iter()
                        .any(|b| same_memory(b, it))
                        || corpus.iter().any(|c| same_memory(c, it))
                    {
                        continue;
                    }
                    crate::pmem::append_project_memory(data_dir, &p.name, it, "auto")?;
                    // ⚠️ 同轮内的第二条同文也要拦住：existing 是查重基准，
                    // 写入后必须同步追加，否则上面的 contains 永远查不到刚写的。
                    existing.push('\n');
                    existing.push_str(&format!("- {it}\n"));
                    written += 1;
                }
                return Ok(written);
            }
        }
    }

    // 主对话 → 全局待审批候选（保留审批关）
    let store = MemoryStore::new(data_dir);
    let pending = store.list_pending().unwrap_or_default();
    let corpus = dedup_corpus(&store);
    let mut written = 0usize;
    for it in &items {
        // 同轮内刚写进去的也要看得见，否则一批里两条同义会都进来
        let dup_in_batch = pending
            .iter()
            .any(|c| same_memory(&c.content, it) || same_memory(it, &c.content));
        if dup_in_batch || corpus.iter().any(|c| same_memory(c, it)) {
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
        // ⚠️ 目录名必须**每次调用唯一**。
        //
        // 原来只有 tag：`orbcat_mem_daily` / `orbcat_mem_proj` / …，且开头无条件
        // `remove_dir_all`。`cargo test` 默认**多线程并行**，多个测试共用同一进程时，
        // 只要 tag 撞上就会互相删掉对方刚写的文件 —— 症状随机：本地串行跑全绿，
        // CI 上并行度不同就偶发失败。`project_memory_longest_prefix_wins` 就是这么挂的。
        //
        // 修法：tag 后缀加纳秒时间戳 + 线程 id，同进程内也不重名。
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let thread = format!("{:?}", std::thread::current().id())
            .replace(|c: char| !c.is_ascii_alphanumeric(), "");
        let dir = std::env::temp_dir().join(format!(
            "orbcat_mem_{tag}_{}_{}_{}",
            std::process::id(),
            thread,
            uniq
        ));
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
        s.reject(&c.id, "实验事实，不是记忆").unwrap();

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

    /// **触发轮次**要记进候选里 —— 审批页要能回答「这条从哪句话来的」。
    #[test]
    fn turn_ref_is_recorded_on_propose() {
        let (s, dir) = fresh_store("turn_ref");

        let c = s
            .propose_with_ref("回答前先给结论", "model", Some("回答先给结论"))
            .unwrap();
        assert_eq!(c.turn_ref.as_deref(), Some("回答先给结论"));

        // 落盘后重读要还在（不是只在内存里）
        let pend = s.list_pending().unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].turn_ref.as_deref(), Some("回答先给结论"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没给触发轮次（或给了空白）→ 存 `None`，不能存空串。
    #[test]
    fn turn_ref_absent_or_blank_is_none() {
        let (s, dir) = fresh_store("turn_ref_blank");

        let a = s.propose_with_ref("甲", "model", None).unwrap();
        assert!(a.turn_ref.is_none(), "没给就该是 None");

        let b = s.propose_with_ref("乙", "model", Some("   ")).unwrap();
        assert!(b.turn_ref.is_none(), "空白该当没给");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老数据没有 `turn_ref` 字段 —— 反序列化不能炸（否则历史候选全读不出来）。
    #[test]
    fn legacy_candidate_without_turn_ref_still_parses() {
        let legacy = r#"{"id":"x_1","content":"老条目","source":"model","created_at":"ts"}"#;
        let c: Candidate = serde_json::from_str(legacy).unwrap();
        assert!(c.turn_ref.is_none());
        assert_eq!(c.content, "老条目");
    }

    /// **改写**只改内容，**不动审批状态** —— 改完还得用户自己点「批准」。
    #[test]
    fn rewrite_changes_content_but_stays_pending() {
        let (s, dir) = fresh_store("rewrite");

        let c = s.propose("回答前先给结论，可能大概也许", "model").unwrap();
        s.rewrite(&c.id, "回答前先给结论。").unwrap();

        let pend = s.list_pending().unwrap();
        assert_eq!(pend.len(), 1, "改写不该把它移出待批队列");
        assert_eq!(pend[0].content, "回答前先给结论。");
        assert!(
            !dir.join("memory").join("MEMORY.md").exists(),
            "改写不该直接写进长期记忆"
        );

        // 改完再批准 → 写进去的是**改写后**的内容
        s.approve(&c.id).unwrap();
        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(mem.contains("回答前先给结论。"));
        assert!(!mem.contains("可能大概也许"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 改写空内容 / 改写不存在的候选 → 报错，不静默吞掉。
    #[test]
    fn rewrite_rejects_blank_and_unknown_id() {
        let (s, dir) = fresh_store("rewrite_bad");

        let c = s.propose("原内容", "model").unwrap();
        assert!(s.rewrite(&c.id, "   ").is_err(), "空内容该报错");
        assert!(s.rewrite("不存在的id", "x").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **全部批准**：一次全写进长期记忆，队列清空。
    #[test]
    fn approve_all_writes_everything_and_empties_queue() {
        let (s, dir) = fresh_store("approve_all");

        s.propose("第一条", "model").unwrap();
        s.propose("第二条", "model").unwrap();
        assert_eq!(s.list_pending().unwrap().len(), 2);

        let n = s.approve_all().unwrap();
        assert_eq!(n, 2);
        assert!(s.list_pending().unwrap().is_empty(), "全部批准后队列应清空");

        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(mem.contains("第一条") && mem.contains("第二条"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没候选时「全部批准」不该报错（幂等），返回 0。
    #[test]
    fn approve_all_on_empty_queue_is_ok() {
        let (s, dir) = fresh_store("approve_all_empty");
        assert_eq!(s.approve_all().unwrap(), 0);
        assert!(!dir.join("memory").join("MEMORY.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 统计口径：**待批数**与**已处理数**（已批准 + 已驳回）分开算。
    #[test]
    fn stats_counts_pending_and_handled() {
        let (s, dir) = fresh_store("stats");

        let a = s.propose("甲", "model").unwrap();
        let b = s.propose("乙", "model").unwrap();
        let c = s.propose("丙", "model").unwrap();

        let (pending, handled) = s.stats().unwrap();
        assert_eq!(pending, 3);
        assert_eq!(handled, 0);

        s.approve(&a.id).unwrap();
        s.reject(&b.id, "不要").unwrap();

        let (pending, handled) = s.stats().unwrap();
        assert_eq!(pending, 1, "批一条驳一条，剩一条");
        assert_eq!(handled, 2, "已批准 1 + 已驳回 1");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = c;
    }

    /// 「全部批准」也要进已处理台账（不然统计会一直停在半路）。
    #[test]
    fn approve_all_is_counted_as_handled() {
        let (s, dir) = fresh_store("stats_all");

        s.propose("甲", "model").unwrap();
        s.propose("乙", "model").unwrap();
        s.approve_all().unwrap();

        let (pending, handled) = s.stats().unwrap();
        assert_eq!(pending, 0);
        assert_eq!(handled, 2);

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
    // 由 CI 的 macos runner 抓出。
    #[test]
    fn project_memory_longest_prefix_wins() {
        let (s, dir) = fresh_store("proj");
        // ⚠️ 目录名必须**每次唯一**。这里原本只用 `process::id()`，而同一进程内
        //    有多个测试都建 `fa_proj_src_<pid>` —— 并行执行时会互相覆盖/删除，
        //    导致 `canonicalize()` 拿到已被删的路径 → 匹配失败。
        //    由 GitHub Actions windows runner 抓出（本地串行跑看不出）。
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("fa_proj_src_{}_{}", std::process::id(), uniq));
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

    /// 路径形态不对称时，项目记忆必须照样命中（真实缺陷回归测试）
    ///
    /// ## 缺陷本身
    ///
    /// 旧实现对比较的两侧用同一条规则：
    ///
    /// ```text
    /// strip_verbatim(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
    /// ```
    ///
    /// 但**两侧能不能 canonicalize 成功是不对称的**：
    ///
    /// - `source.ref` 指向的目录**总是存在** → 成功。Windows 上 `canonicalize`
    ///   走 `GetFinalPathNameByHandle`，会**展开重解析点与 8.3 短名**：
    ///   `C:\Users\RUNNER~1\...` → `C:\Users\runneradmin\...`
    /// - 待匹配的**文件常常还不存在**（用户说「这个项目」时文件可能还没建）
    ///   → 失败 → 原样返回，**形态没被归一**
    ///
    /// 于是只要两侧指向同一个真实位置、但写法不同，前缀比较必然失败，
    /// 用户看到的现象是「项目记忆突然不生效」，而且**没有任何报错**。
    ///
    /// ## 为什么本机能复现
    ///
    /// GitHub `windows-latest` 的 TEMP 是 `C:\Users\RUNNER~1\...`（8.3 短名），
    /// canonicalize 必然把它展开。而本机 C: 盘 `NtfsDisable8dot3NameCreation=1`，
    /// 8.3 短名压根没生成，所以本地永远碰巧通过 —— 这也是前两轮盲修
    /// （先猜「目录竞态」、后猜「短名」）都改不对的原因。
    ///
    /// 这里改用**目录联接（junction）** 复现**同一类**不对称：
    /// `source.ref` 存联接路径（存在 → 被解析成 `real`），
    /// 而查询文件不存在（保持联接形态）。普通 Windows 本地就能稳定复现。
    #[cfg(windows)]
    #[test]
    fn project_memory_matches_across_path_forms() {
        let (s, dir) = fresh_store("proj_form");
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("fa_form_{}_{}", std::process::id(), uniq));
        let real = base.join("real");
        let deep = real.join("sub");
        std::fs::create_dir_all(&deep).unwrap();

        // 建不出联接（策略/权限）就跳过并如实打印，不假装通过。
        let link = base.join("link");
        if let Err(e) = std::os::windows::fs::symlink_dir(&real, &link) {
            eprintln!(
                "跳过：当前环境不允许创建目录联接（{e}）—— 本用例覆盖的正是同一类路径形态不对称"
            );
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let mk = |name: &str, src: &Path, mem: &str| {
            let p = dir.join("projects").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("source.ref"), src.display().to_string()).unwrap();
            std::fs::write(p.join("MEMORY.md"), mem).unwrap();
        };
        // 关键：`outer` 的 source.ref 存的是**联接路径**形态
        mk("outer", &link, "外层项目记忆");
        mk("inner", &deep, "内层项目记忆");

        // 查询文件**不存在** → 旧实现在这里保持联接形态，永远匹配不上
        let hit = s
            .match_project_memory(&[&link.join("sub").join("file.txt")])
            .expect("联接形态 + 未创建的文件，也应命中项目记忆");
        assert_eq!(hit.0, "inner", "最长前缀应选中 inner");
        assert!(hit.1.contains("内层项目记忆"));

        let hit2 = s
            .match_project_memory(&[&link.join("other.txt")])
            .expect("联接形态也应命中外层");
        assert_eq!(hit2.0, "outer");

        // ⚠️ 清理顺序：必须先 `remove_dir` 掉联接**本身**（不递归），
        //    否则 `remove_dir_all` 会顺着联接把 `real` 里的内容一起删掉。
        let _ = std::fs::remove_dir(&link);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&base);
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

    // ---------------- 查重范围与归一化（2026-10-15） ----------------

    /// **回归本体**：一条记忆被 `distill_move` 迁进 USER.md 之后，
    /// 下一轮自动蒸馏会把它**原样再提一遍**。
    ///
    /// 根因：查重只扫 `memory/MEMORY.md`。迁移完成后条目就不在中转站里了，
    /// `lt.contains(it)` 永远不命中 → 重新进候选队列。实证 2026-10-15：
    /// 候选队列 5 条里 4 条是刚迁走的同义内容。
    #[test]
    fn dedup_covers_user_md_not_just_the_transit_station() {
        let (s, dir) = fresh_store("dedup_user");
        std::fs::write(
            dir.join("USER.md"),
            "# USER.md\n\n## 偏好\n\n- 用户以「cc」指代 Claude Code\n",
        )
        .unwrap();

        let n = store_auto_entries(&dir, &["用户以「cc」指代 Claude Code。".to_string()]).unwrap();
        assert_eq!(n, 0, "已在 USER.md 里的条目不该再进候选队列");
        assert!(s.list_pending().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同样要覆盖 SOUL / IDENTITY / 项目记忆 —— 五个落点一个都不能漏。
    #[test]
    fn dedup_covers_soul_identity_and_project_memory() {
        let (s, dir) = fresh_store("dedup_all");
        std::fs::write(
            dir.join("SOUL.md"),
            "# SOUL\n\n## 风格\n\n- think 块内禁止出现解析标签\n",
        )
        .unwrap();
        std::fs::write(dir.join("IDENTITY.md"), "# IDENTITY\n\n- 名字：小 orb\n").unwrap();
        std::fs::create_dir_all(dir.join("projects/论文")).unwrap();
        std::fs::write(
            dir.join("projects/论文/MEMORY.md"),
            "# 项目记忆 — 论文\n\n- M3FM 的 Fig.1 b 面板 = CTViT\n",
        )
        .unwrap();

        for dup in [
            "think 块内禁止出现解析标签",
            "名字：小 orb",
            "M3FM 的 Fig.1 b 面板 = CTViT",
        ] {
            let n = store_auto_entries(&dir, &[dup.to_string()]).unwrap();
            assert_eq!(n, 0, "「{dup}」已在某个落点，不该重复提交");
        }
        assert!(s.list_pending().unwrap().is_empty());

        // 真的新条目还是要能进去 —— 别把查重写死了
        let n = store_auto_entries(&dir, &["用户习惯用 pnpm 装依赖".to_string()]).unwrap();
        assert_eq!(n, 1, "新条目必须还能进候选队列");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归一化要能吃下**改写版**，不是逐字相等。
    ///
    /// 自动蒸馏产出的从来是改写句：带 `XX：` 前缀、末尾挂时间戳、中间多个标点。
    /// 原来的子串匹配对这类一律失效 —— 这正是 2026-10-15 那批重复的成因。
    #[test]
    fn dedup_survives_rewritten_forms() {
        let (_s, dir) = fresh_store("dedup_norm");
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(
            dir.join("memory").join("MEMORY.md"),
            "# MEMORY.md\n\n- 用户的论文/笔记产出统一放在 D:\\myword\\文章\\ 下的 Obsidian 库，\
             论文类笔记固定在 D:\\myword\\文章\\论文\\，用 Typora 编辑。（ts1791981574T12:39:34Z）\n",
        )
        .unwrap();

        // 自动蒸馏的典型改写：**带标签前缀 + 尾部挂时间戳 + 标点不同**。
        // 这三类正是 `norm_for_dedup` 要吃下的（大幅改写认不出，见下一条测试）。
        let rewritten =
            "论文笔记产出：用户的论文/笔记产出统一放在 D:\\myword\\文章\\ 下的 Obsidian 库，\
             论文类笔记固定在 D:\\myword\\文章\\论文\\，用 Typora 编辑（ts1791981574T12:39:34Z）";
        let n = store_auto_entries(&dir, &[rewritten.to_string()]).unwrap();
        assert_eq!(n, 0, "带标签前缀/时间戳/标点差异的改写版要认得出是同一条: {rewritten}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 归一化的单点断言（把规则本身锁住，比混在集成用例里好定位）。
    #[test]
    fn norm_strips_stamp_prefix_and_punctuation() {
        assert_eq!(
            norm_for_dedup("条目正文。（ts1789895466T09:11:06Z）"),
            norm_for_dedup("条目正文")
        );
        assert_eq!(norm_for_dedup("标签：条目正文"), norm_for_dedup("条目正文"));
        assert_eq!(norm_for_dedup("条目，正文。"), norm_for_dedup("条目正文"));
        // 标点差异不应影响判定
        assert!(same_memory(
            "M3FM 的 Fig.1 b 面板 = CTViT。",
            "M3FM 的 Fig.1b 面板 = CTViT"
        ));
        // 不能把「都在讲工具」这种泛泛撞词判成同一条
        assert!(!same_memory("用工具查 token 用量", "用工具读 PDF 插图"));
    }

    /// **已知边界（刻意不做）**：真正的**大幅改写**认不出来。
    ///
    /// 例：「M3FM 的 Fig.1 b 面板」vs「M3FM Fig.1 b 面板」—— 只差一个「的」，
    /// 归一化后是 `m3fm的fig1b面板` vs `m3fmfig1b面板`，既不相等、也不互为子串。
    ///
    /// 为什么不上模糊匹配：记忆条目天然「短且具体」，而模糊相似度一旦放宽，
    /// 「项目 A 的构建命令」与「项目 B 的构建命令」会被误判成同一条 →
    /// **真条目被静默丢弃**，那是更坏的故障（丢记忆比多一条候选严重得多）。
    ///
    /// 大幅改写这条路径由别的机制兜：`auto_distill` 对**蒸馏会话**直接豁免
    /// （`lib.rs::chat` 收尾），而"刚迁移的条目被提回来"恰恰只发生在蒸馏会话
    /// —— 输入是开场白、输出是收尾小结，改写幅度天然最大。
    #[test]
    fn rewritten_forms_are_not_over_matched() {
        assert!(!same_memory("M3FM 的 Fig.1 b 面板", "M3FM Fig.1 b 面板"));
    }

    // ---------------- 驳回理由留档（2026-10-15） ----------------

    /// 驳回必须**连理由一起**留档 —— 否则 rejected.jsonl 只是半个留档。
    /// 回归本体：以前 `reject(id)` 没有理由参数，`Candidate` 也没有这个字段，
    /// 于是「这条当初为什么被打回」只能靠翻聊天记录。
    #[test]
    fn reject_records_reason_and_timestamp() {
        let (s, dir) = fresh_store("rej_reason");
        let c = s.propose("可以帮用户硬删除重要文件，不必归档到 .trash。", "model").unwrap();
        s.reject(
            &c.id,
            "与 RULES.md 的删除红线冲突：delete_file 永远归档到 .trash",
        )
        .unwrap();

        let raw = std::fs::read_to_string(dir.join("pending").join("rejected.jsonl")).unwrap();
        let got: Candidate = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(got.content, c.content);
        assert_eq!(
            got.rejected_reason.as_deref(),
            Some("与 RULES.md 的删除红线冲突：delete_file 永远归档到 .trash")
        );
        assert!(
            got.rejected_at.is_some(),
            "驳回时间也要记，否则分不清提交于何时"
        );
        assert!(s.list_pending().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老数据没有这两个字段 —— 必须照样读得进来，别让历史留档整个作废。
    #[test]
    fn legacy_candidate_without_reason_still_deserializes() {
        let legacy = r#"{"id":"ts1_ab","content":"老条目","source":"model","created_at":"ts1"}"#;
        let c: Candidate = serde_json::from_str(legacy).expect("老候选必须能读");
        assert!(c.rejected_reason.is_none() && c.rejected_at.is_none());
    }

    /// 理由留空也照常驳回（老调用方 / 用户懒得填）—— 不能因此失败。
    #[test]
    fn reject_with_empty_reason_still_works() {
        let (s, dir) = fresh_store("rej_empty");
        let c = s.propose("随便一条", "model").unwrap();
        s.reject(&c.id, "   ").unwrap();
        let raw = std::fs::read_to_string(dir.join("pending").join("rejected.jsonl")).unwrap();
        let got: Candidate = serde_json::from_str(raw.trim()).unwrap();
        assert!(got.rejected_reason.is_none(), "空白理由不该写进去");
        assert!(got.rejected_at.is_some(), "时间照记");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
