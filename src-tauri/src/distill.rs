//! 记忆蒸馏的**搬运层**：解析 `memory/MEMORY.md`（中转站）+ 把条目迁到目标文件
//!
//! ## 四个落点（2026-10-15 起）
//!
//! | target | 落点 | 装什么 |
//! |---|---|---|
//! | `user` | `agent-data/USER.md` | **只写「人」**：身份、能力、性格、习惯、雷区、设备 |
//! | `soul` | `agent-data/SOUL.md` | 人格、语气、表达纪律 |
//! | `identity` | `agent-data/IDENTITY.md` | 名字、互相怎么称呼 |
//! | `project` | `agent-data/projects/<名>/MEMORY.md` | **只在本项目成立**：路径、术语、进展、未决项 |
//!
//! 加 `project` 之前没有第四个落点，于是「只在本项目成立」的事实只有两条路：
//! 硬塞进 `USER.md`（被它的边界自锁明令禁止）或留在中转站 —— 每次蒸馏都只能 `stay`。
//! 项目记忆本来就在 `agent-data/projects/` 下、`remember(scope:"project")` 也一直在写它，
//! 蒸馏却够不着：这是搬运层少了一个去向，不是数据没地方放。
//!
//! ## 定位（2026-10-03 定稿）
//!
//! 谁来做判断：**agent 本身**，在一条普通的「记忆蒸馏」会话里 —— 它读中转站、
//! 逐条问用户、拿到答复后调 [`apply_entry`]（工具名 `distill_move`）落盘。
//! 思考过程、工具调用时间线全走主对话那套 UI（用户 2026-10-03 明确要求：
//! 「就是常规聊天」）。
//!
//! 所以本模块**不含任何 LLM 调用**：没有 prompt、没有 JSON 协议解析。
//! 只做两件 deterministic 的事：
//! 1. [`split_entries`]：把中转站拆成条目（带行区间，供安全摘除）；
//! 2. [`apply_entry`]：备份 → 写目标文件 → 从中转站移除该条目。
//!
//! ## 备份策略
//!
//! 每次迁移前把 MEMORY.md 原文写到 `memory/MEMORY.md.bak-<日期>`。
//! **按天覆盖**而不是按次带时间戳：agent 一次蒸馏会连着迁十几条，
//! 按次命名会在目录里堆十几个几乎一样的备份，用户根本不看；
//! 按天一份 = 那天随时能回到蒸馏前状态，隔天自动换新。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 合法去向（`duplicate` = 目标已有同文，只从中转站删）
///
/// ⚠️ `project`（2026-10-15 加）：项目级事实的落点。此前蒸馏只往三份全局
/// persona 文件搬，于是「只在本项目成立」的事实只能被硬塞进 USER.md 或
/// 留在中转站 —— 而 `USER.md` 的边界自锁明令禁止写项目内容，于是这类条目
/// 每次蒸馏都只能 `stay`。现在有正经落点了。
pub const TARGETS: [&str; 6] = [
    "user",
    "soul",
    "identity",
    "project",
    "stay",
    "duplicate",
];

/// 一次迁移的请求（agent 调 `distill_move` 时的参数）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillProposal {
    /// 条目正文（不含行首 `- `）。从中转站移除时按它匹配
    pub text: String,
    /// user | soul | identity | project | duplicate（stay = 什么都不做）
    pub target: String,
    /// 归到目标文件的哪个小节（如 `## 基本`）；空 = 追加到末尾
    #[serde(default)]
    pub section: String,
    /// `target="project"` 时指定项目名；留空 = 用顶栏当前激活的项目。
    ///
    /// 留空是**有意的**：与 `remember(scope:"project")` 同一条规则 ——
    /// 用户已经用顶栏表达了"我现在在哪个项目里"，不该再让模型复述一遍名字。
    /// 两者都没有时 [`resolve_project`] 会报错并列出可选项目。
    #[serde(default)]
    pub project: String,
    /// 迁移理由（回显给用户看，便于 agent 在正文里交代）
    #[serde(default)]
    pub reason: String,
}

/// 落盘结果（工具返回值 / 测试断言用）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillReport {
    /// 写入目标文件的条数（duplicate/stay 不算）
    pub moved: usize,
    /// 从中转站移除的条数
    pub removed: usize,
    /// 备份文件名（`memory/` 下）
    pub backup: String,
    /// 实际写入的文件（相对 data_dir）；空 = 没写任何文件
    ///
    /// 为什么加这个字段：`target="project"` 时"目标文件"不是 `<TARGET>.md`
    /// 而是 `projects/<名>/MEMORY.md`，工具返回值要**逐字说清改的是哪个文件**
    /// —— 模型会把它原样转述给用户，说含糊了用户就不知道自己的记忆去哪了。
    pub written: String,
    /// 跳过的条目（目标已有同文等原因）
    pub skipped: Vec<String>,
}

// ---------------------------------------------------------------------------
// MEMORY.md 条目解析
// ---------------------------------------------------------------------------

/// MEMORY.md 里的一个条目：正文 + 覆盖的行区间（`[line_start, line_end)`，0 起）
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub text: String,
    pub line_start: usize,
    pub line_end: usize,
}

/// 把 MEMORY.md 拆成条目。
///
/// 规则：顶层 `- ` 开头的行开新条目；其后**缩进的**非空行算该条目的续行；
/// 标题（`#`）、引用（`>`）、分隔线（`---`）、空行都不属于任何条目。
/// 这样移除条目时可以按行区间整段摘掉，标题/表头原样保留。
pub fn split_entries(md: &str) -> Vec<Entry> {
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<Entry> = Vec::new();
    let mut cur: Option<Entry> = None;

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim_end();
        if let Some(rest) = line.strip_prefix("- ") {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            cur = Some(Entry {
                text: rest.trim().to_string(),
                line_start: i,
                line_end: i + 1,
            });
        } else if !line.trim().is_empty()
            && line.starts_with(char::is_whitespace)
            && cur.is_some()
        {
            // 缩进续行归当前条目
            let e = cur.as_mut().expect("cur.is_some() 已检查");
            e.text.push('\n');
            e.text.push_str(line.trim());
            e.line_end = i + 1;
        } else if !line.trim().is_empty() && cur.is_some() {
            // 顶层非 bullet 行（标题/引用/分隔线）= 条目结束
            if let Some(e) = cur.take() {
                out.push(e);
            }
        }
    }
    if let Some(e) = cur.take() {
        out.push(e);
    }
    out
}

/// 条目归一化键：用于「agent 传来的正文 ↔ 磁盘条目」匹配（容忍空白差异）
fn norm_key(s: &str) -> String {
    s.lines().map(str::trim).collect::<Vec<_>>().join("\n")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…（截断）")
    }
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

/// 今天的日期串（`YYYYMMDD`）—— 备份按天命名
fn today_compact() -> String {
    ts_compact_pub()
}

/// [`today_compact`] 的跨模块可见版本（`pmem` 的备份命名要用同一套）。
pub(crate) fn ts_compact_pub() -> String {
    ts_compact()
}

fn ts_compact() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (yoe * 365 + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

fn long_term_path(root: &Path) -> PathBuf {
    root.join("memory").join("MEMORY.md")
}

fn target_path(root: &Path, target: &str) -> PathBuf {
    root.join(format!("{}.md", target.to_uppercase()))
}

/// `target="project"` 的落点：`projects/<名>/MEMORY.md`。
///
/// ⚠️ 必须与 `pmem::project_memory_md` / `pmem::append_project_memory` /
/// `memory::MemoryStore::match_project_memory` **同一条路径** —— 那几处是
/// 项目记忆的真源，这里若自己拼一套，蒸馏写进去的东西模型永远读不到。
fn project_memory_path(root: &Path, name: &str) -> PathBuf {
    root.join("projects").join(name).join("MEMORY.md")
}

/// 解析 `target="project"` 的项目名，返回**已登记**的项目名。
///
/// ## 为什么必须查 `pmem.sqlite` 而不是直接拿名字拼路径
///
/// 项目名是**模型给的字符串**，而落点是 `data_dir/projects/<名>/MEMORY.md`。
/// 不校验就等于把路径拼装权交给模型：`project: "../../evil"` 会在
/// `agent-data` 外面凭空造出目录并往里写文件。名字只允许**精确等于某个已登记
/// 的项目**，从根上堵死穿越（顺带挡掉 `"a/b"`、`".."`、`""`）。
///
/// 校验过不了就报错并**列出可选项目** —— 模型拿到清单后能直接改对，不必再来回试。
fn resolve_project(root: &Path, want: &str) -> Result<String, String> {
    let known = crate::pmem::list_projects(root).unwrap_or_default();
    let list = if known.is_empty() {
        "（一个都没有 —— 用户还没建过项目）".to_string()
    } else {
        known
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join("、")
    };

    let want = want.trim();
    if want.is_empty() {
        // 留空 = 用顶栏激活的项目，与 `remember(scope:"project")` 同一条规则
        let active = crate::config::load_settings(root)
            .active_project_id
            .and_then(|pid| known.iter().find(|p| p.id == pid).map(|p| p.name.clone()));
        return active.ok_or_else(|| {
            format!(
                "target=\"project\" 但没有指定项目名，顶栏也没有激活项目。\n\
                 二选一：\n\
                 1. 传 project=\"<项目名>\" 指明落到哪个项目\n\
                 2. 让用户在顶栏「项目 ▾」里切到目标项目，再调一次（不带 project）\n\
                 现有项目：{list}"
            )
        });
    }

    known
        .iter()
        .find(|p| p.name == want)
        .map(|p| p.name.clone())
        .ok_or_else(|| format!("没有名为「{want}」的项目。现有项目：{list}"))
}

/// 条目 → 目标文件里的块（首行带 `- `，续行缩进两格）
fn bullet_block(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim().to_string();
    let mut out = format!("- {first}");
    for l in lines {
        let l = l.trim();
        if !l.is_empty() {
            out.push_str(&format!("\n  {l}"));
        }
    }
    out
}

/// 把条目插进目标文件：`section` 命中现有小节 → 插小节头之后；
/// `section` 非空但没命中 → 文件末尾新建小节；空 → 直接追加末尾。
/// 目标已有同文 → 返回 false（调用方计为 duplicate/skip）。
fn insert_into_target(file: &str, section: &str, text: &str) -> Option<String> {
    let bullet_lines: Vec<String> = bullet_block(text).lines().map(String::from).collect();
    // 判重：目标文件里已有同文 bullet（按行归一比较）
    let existing: Vec<String> = file.lines().map(|l| l.trim().to_string()).collect();
    for b in &bullet_lines {
        let key = b.trim_start_matches("- ").trim();
        if !key.is_empty() && existing.iter().any(|l| l == key || l == &format!("- {key}")) {
            return None;
        }
    }

    let trimmed = file.trim_end().to_string();
    let sec = section.trim();
    if sec.is_empty() {
        let mut out = trimmed;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&bullet_lines.join("\n"));
        return Some(out);
    }

    let normalized = if sec.starts_with('#') {
        sec.to_string()
    } else {
        format!("## {sec}")
    };
    let lines: Vec<String> = trimmed.lines().map(String::from).collect();
    let mut hit: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == normalized {
            hit = Some(i);
            break;
        }
    }
    let mut out: Vec<String> = lines;
    match hit {
        Some(i) => {
            let at = i + 1;
            for (off, b) in bullet_lines.iter().enumerate() {
                out.insert(at + off, b.clone());
            }
        }
        None => {
            out.push(String::new());
            out.push(normalized);
            out.push(String::new());
            for b in &bullet_lines {
                out.push(b.clone());
            }
        }
    }
    Some(out.join("\n") + "\n")
}

/// 重写 MEMORY.md：按行区间摘掉被移除的条目（标题/引用/空行原样保留）
fn rewrite_memory(md: &str, remove: &[Entry]) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut dead = vec![false; lines.len()];
    for e in remove {
        for i in e.line_start..e.line_end.min(lines.len()) {
            dead[i] = true;
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut blanks = 0;
    for (i, l) in lines.iter().enumerate() {
        if dead[i] {
            continue;
        }
        if l.trim().is_empty() {
            blanks += 1;
            if blanks >= 2 {
                continue; // 连续空行压成一行，移除条目后不留洞
            }
        } else {
            blanks = 0;
        }
        out.push((*l).to_string());
    }
    let mut s = out.join("\n");
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n'); // 与原文件保持一致：结尾带一个换行
    }
    s
}

/// 迁移**一条**（`distill_move` 工具的实现体）。
///
/// 流程：解析去向 → 备份（按天）→ 写目标文件 → 从中转站移除。
/// `target = duplicate` 表示「目标已有同文」：不写文件，只从中转站删。
/// `target = stay`：什么都不做（agent 用它表达「这条留在中转站」）。
/// `target = project`：写 `projects/<名>/MEMORY.md`（名字见 [`resolve_project`]）。
///
/// ⚠️ **项目名必须先解析成功再动任何文件** —— 报错时磁盘保持原样，
/// 与「传错正文一个字都不动」的纪律一致。
pub fn apply_entry(root: &Path, p: &DistillProposal) -> Result<DistillReport, String> {
    let text = p.text.trim();
    if text.is_empty() {
        return Err("条目内容不能为空".into());
    }
    if !TARGETS.contains(&p.target.as_str()) {
        return Err(format!(
            "未知去向 {}（只能是 user / soul / identity / project / stay / duplicate）",
            p.target
        ));
    }
    let empty = |backup: String| DistillReport {
        moved: 0,
        removed: 0,
        backup,
        written: String::new(),
        skipped: Vec::new(),
    };

    if p.target == "stay" {
        return Ok(empty(String::new()));
    }

    // 项目名先解析（可能报错）—— 放在备份之前，保证失败时磁盘零改动
    let project = if p.target == "project" {
        Some(resolve_project(root, &p.project)?)
    } else {
        None
    };

    let mem_path = long_term_path(root);
    let md = std::fs::read_to_string(&mem_path).map_err(|e| format!("读 MEMORY.md 失败: {e}"))?;

    // 条目必须真的在中转站里 —— agent 传错字时不能静默改文件
    let entries = split_entries(&md);
    let key = norm_key(text);
    let Some(hit) = entries.iter().find(|e| norm_key(&e.text) == key) else {
        return Err("中转站里没有这条（正文与 MEMORY.md 不一致，可能已迁移过）".into());
    };
    let hit = hit.clone();

    // 1) 备份（按天覆盖：一天内反复迁移只留一份「当天蒸馏前」）
    let backup_name = format!("MEMORY.md.bak-{}", today_compact());
    std::fs::create_dir_all(root.join("memory")).map_err(|e| e.to_string())?;
    std::fs::write(root.join("memory").join(&backup_name), &md)
        .map_err(|e| format!("写备份失败: {e}"))?;

    // 2) 写目标文件（duplicate 跳过写）
    let mut moved = 0usize;
    let mut written = String::new();
    let mut skipped: Vec<String> = Vec::new();
    if let Some(name) = &project {
        // 项目侧也要能反悔：落盘前先把它自己按天备份一份
        let path = project_memory_path(root, name);
        let cur = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let pbak = path.with_file_name(format!("MEMORY.md.bak-{}", today_compact()));
        std::fs::write(&pbak, &cur).map_err(|e| format!("写项目记忆备份失败: {e}"))?;
        match insert_into_target(&cur, &p.section, text) {
            Some(next) => {
                std::fs::write(&path, next).map_err(|e| format!("写项目记忆失败: {e}"))?;
                moved = 1;
                written = format!("projects/{name}/MEMORY.md");
            }
            None => skipped.push(format!("项目「{name}」的记忆已有同文：{}", clip(text, 60))),
        }
    } else if matches!(p.target.as_str(), "user" | "soul" | "identity") {
        let file = std::fs::read_to_string(target_path(root, &p.target)).unwrap_or_default();
        match insert_into_target(&file, &p.section, text) {
            Some(next) => {
                std::fs::write(target_path(root, &p.target), next)
                    .map_err(|e| format!("写 {} 失败: {e}", p.target))?;
                moved = 1;
                written = format!("{}.md", p.target.to_uppercase());
            }
            None => skipped.push(format!("目标文件已有同文：{}", clip(text, 60))),
        }
    }

    // 3) 从中转站移除
    let next_md = rewrite_memory(&md, &[hit]);
    std::fs::write(&mem_path, next_md).map_err(|e| format!("重写 MEMORY.md 失败: {e}"))?;

    Ok(DistillReport {
        moved,
        removed: 1,
        backup: backup_name,
        written,
        skipped,
    })
}

/// 删除**一条**（`memory_delete` 工具的实现体，2026-10-10 加）。
///
/// 与 [`apply_entry`] 的区别只有一个：**不写任何目标文件**，只把这条从中转站摘掉。
/// 用于「这条已经过时」或「当初就记错了」—— 记忆是滚动累积的，一条错的条目会一直
/// 影响以后的每一次回答，光加不减迟早把长期记忆污染成一堆假约束。
///
/// 备份与匹配规则**完全复用** [`apply_entry`]：按天备份（`MEMORY.md.bak-<日期>`）、
/// 正文逐字匹配；传错字直接报错、**一个字都不动文件**。
///
/// 为什么不给主对话也开这个工具（2026-10-10 定）：删除是破坏性的，而主对话里
/// 没有「逐条摆给用户过目」这个环节 —— 那正是蒸馏会话存在的意义。
/// 蒸馏会话每次动手前都会先把建议和原文摆出来，拿不准就问。
pub fn delete_entry(root: &Path, text: &str) -> Result<DistillReport, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("条目内容不能为空".into());
    }
    let mem_path = long_term_path(root);
    let md = std::fs::read_to_string(&mem_path).map_err(|e| format!("读 MEMORY.md 失败: {e}"))?;

    // 必须真的在中转站里 —— 传错字时不能静默改文件
    let entries = split_entries(&md);
    let key = norm_key(text);
    let Some(hit) = entries.iter().find(|e| norm_key(&e.text) == key) else {
        return Err("中转站里没有这条（正文与 MEMORY.md 不一致，可能已经处理过了）".into());
    };
    let hit = hit.clone();

    // 备份（按天覆盖，与 apply_entry 同策略）
    let backup_name = format!("MEMORY.md.bak-{}", today_compact());
    std::fs::create_dir_all(root.join("memory")).map_err(|e| e.to_string())?;
    std::fs::write(root.join("memory").join(&backup_name), &md)
        .map_err(|e| format!("写备份失败: {e}"))?;

    let next_md = rewrite_memory(&md, &[hit]);
    std::fs::write(&mem_path, next_md).map_err(|e| format!("重写 MEMORY.md 失败: {e}"))?;

    Ok(DistillReport {
        moved: 0,
        removed: 1,
        backup: backup_name,
        written: String::new(),
        skipped: Vec::new(),
    })
}
/// 批量迁移（测试与将来「一键应用」用；agent 走 [`apply_entry`] 逐条）
#[cfg(test)]
fn apply_batch(root: &Path, approved: &[DistillProposal]) -> Result<DistillReport, String> {
    let mut moved = 0;
    let mut removed = 0;
    let mut backup = String::new();
    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for p in approved {
        let r = apply_entry(root, p)?;
        moved += r.moved;
        removed += r.removed;
        skipped.extend(r.skipped);
        if !r.written.is_empty() && !written.contains(&r.written) {
            written.push(r.written);
        }
        if !r.backup.is_empty() {
            backup = r.backup;
        }
    }
    Ok(DistillReport {
        moved,
        removed,
        backup,
        written: written.join("、"),
        skipped,
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# MEMORY.md — 长期记忆\n\n> 说明行\n\n## 用户确认过的约定\n\n- 称呼约定：用户叫我「你」。（ts1）\n- 交付风格：先结论后细节。（ts2）\n\n## 项目\n\n- 洛谷代码放 D:\\allc++\\luogu\\，文件名 P<题号>.cpp。（ts3）\n\n- 多行条目第一行\n  续行 A\n  续行 B（ts4）\n\n_最后更新：_\n";

    fn seed(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbcat_distill2_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(dir.join("memory").join("MEMORY.md"), SAMPLE).unwrap();
        dir
    }

    fn prop(text: &str, target: &str, section: &str) -> DistillProposal {
        DistillProposal {
            text: text.into(),
            target: target.into(),
            section: section.into(),
            project: String::new(),
            reason: String::new(),
        }
    }

    /// 带项目名的 `distill_move` 参数（`target="project"` 专用）
    fn prop_proj(text: &str, project: &str, section: &str) -> DistillProposal {
        DistillProposal {
            project: project.into(),
            ..prop(text, "project", section)
        }
    }

    #[test]
    fn split_entries_excludes_headers_and_quotes() {
        let es = split_entries(SAMPLE);
        let texts: Vec<&str> = es.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts.len(), 4, "应解析出 4 条条目: {texts:?}");
        assert!(texts[0].starts_with("称呼约定"));
        assert!(texts[2].contains("D:\\allc++\\luogu"));
        assert_eq!(texts[3], "多行条目第一行\n续行 A\n续行 B（ts4）", "续行归并");
        assert!(!texts.iter().any(|t| t.contains("长期记忆")));
        assert!(!texts.iter().any(|t| t.contains("最后更新")));
    }

    #[test]
    fn apply_entry_moves_into_existing_section_and_removes() {
        let dir = seed("move");
        std::fs::write(
            dir.join("IDENTITY.md"),
            "# IDENTITY.md — 我是谁\n\n## 称呼约定\n\n- （已有内容）\n",
        )
        .unwrap();

        let r = apply_entry(&dir, &prop("称呼约定：用户叫我「你」。（ts1）", "identity", "## 称呼约定"))
            .unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(r.removed, 1);
        assert!(r.backup.starts_with("MEMORY.md.bak-"), "备份按天命名: {}", r.backup);

        let id = std::fs::read_to_string(dir.join("IDENTITY.md")).unwrap();
        assert!(id.contains("称呼约定：用户叫我「你」"), "应插进现有小节: {id}");

        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(!mem.contains("称呼约定"), "已迁移的要从中转站消失: {mem}");
        assert!(mem.contains("## 用户确认过的约定"), "标题保留");
        assert!(mem.contains("多行条目第一行"), "别的条目不动");
        assert!(mem.contains("交付风格"));

        // 备份里是原文
        let bak = std::fs::read_to_string(dir.join("memory").join(&r.backup)).unwrap();
        assert!(bak.contains("称呼约定"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_entry_creates_section_and_dedupes() {
        let dir = seed("section");
        // 目标没有该小节 → 新建
        let r = apply_entry(
            &dir,
            &prop(
                "洛谷代码放 D:\\allc++\\luogu\\，文件名 P<题号>.cpp。（ts3）",
                "user",
                "存放习惯",
            ),
        )
        .unwrap();
        assert_eq!(r.moved, 1);
        let user = std::fs::read_to_string(dir.join("USER.md")).unwrap();
        assert!(user.contains("## 存放习惯"), "没命中应新建小节: {user}");

        // 再迁一次同文 → duplicate 语义：目标不重复写，但中转站已无此条（会报找不到）
        let again = apply_entry(
            &dir,
            &prop(
                "洛谷代码放 D:\\allc++\\luogu\\，文件名 P<题号>.cpp。（ts3）",
                "user",
                "存放习惯",
            ),
        );
        assert!(again.is_err(), "中转站已无此条，应报错而不是重复写");
        let user2 = std::fs::read_to_string(dir.join("USER.md")).unwrap();
        assert_eq!(
            user2.matches("洛谷代码").count(),
            1,
            "目标文件不该被写第二份: {user2}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_target_only_removes_from_transit() {
        let dir = seed("dup");
        let r = apply_entry(
            &dir,
            &prop("交付风格：先结论后细节。（ts2）", "duplicate", ""),
        )
        .unwrap();
        assert_eq!(r.moved, 0, "duplicate 不写文件");
        assert_eq!(r.removed, 1);
        assert!(!dir.join("USER.md").exists(), "duplicate 不该建目标文件");
        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(!mem.contains("交付风格"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stay_and_unknown_target_are_handled() {
        let dir = seed("stay");
        let r = apply_entry(&dir, &prop("交付风格：先结论后细节。（ts2）", "stay", "")).unwrap();
        assert_eq!((r.moved, r.removed), (0, 0));
        assert!(r.backup.is_empty(), "stay 不该备份");

        let bad = apply_entry(&dir, &prop("x", "不知道", ""));
        assert!(bad.is_err(), "未知去向要报错");

        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(mem.contains("交付风格"), "中转站应原样: {mem}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_text_is_rejected_without_touching_files() {
        let dir = seed("unknown");
        let before = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        let r = apply_entry(&dir, &prop("这条根本不在中转站里", "user", ""));
        assert!(r.is_err());
        let after = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert_eq!(before, after, "报错时不能动文件");
        assert!(!dir.join("USER.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_moves_several_and_keeps_one_backup_per_day() {
        let dir = seed("batch");
        let r = apply_batch(
            &dir,
            &[
                prop("称呼约定：用户叫我「你」。（ts1）", "identity", "## 称呼约定"),
                prop("交付风格：先结论后细节。（ts2）", "duplicate", ""),
                prop(
                    "洛谷代码放 D:\\allc++\\luogu\\，文件名 P<题号>.cpp。（ts3）",
                    "user",
                    "存放习惯",
                ),
            ],
        )
        .unwrap();
        assert_eq!(r.moved, 2);
        assert_eq!(r.removed, 3);
        let baks: Vec<_> = std::fs::read_dir(dir.join("memory"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("MEMORY.md.bak-"))
            .collect();
        assert_eq!(baks.len(), 1, "同一天只留一份备份，实际 {}", baks.len());
        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(!mem.contains("称呼约定") && !mem.contains("交付风格") && !mem.contains("洛谷"));
        assert!(mem.contains("多行条目第一行"), "未点名的条目保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- 删除（memory_delete，2026-10-10） ----------------

    #[test]
    fn delete_entry_removes_it_and_keeps_a_backup() {
        let dir = seed("del");
        let r = delete_entry(&dir, "交付风格：先结论后细节。（ts2）").unwrap();
        assert_eq!((r.moved, r.removed), (0, 1), "删除不写任何目标文件");
        assert!(r.backup.starts_with("MEMORY.md.bak-"), "备份按天命名: {}", r.backup);

        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(!mem.contains("交付风格"), "该删的要从文件里消失: {mem}");
        // 标题、说明行、别的条目一个都不能少
        assert!(mem.contains("## 用户确认过的约定"), "标题保留");
        assert!(mem.contains("称呼约定"), "别的条目不动");
        assert!(mem.contains("多行条目第一行"), "多行条目不动");

        // 备份里是原文 —— 删错了能从这儿捞回来
        let bak = std::fs::read_to_string(dir.join("memory").join(&r.backup)).unwrap();
        assert!(bak.contains("交付风格"), "备份必须是删除前的内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_entry_rejects_unknown_text_without_touching_files() {
        let dir = seed("del-err");
        let before = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        let r = delete_entry(&dir, "这条根本不在中转站里");
        assert!(r.is_err(), "传错字必须报错");
        let after = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert_eq!(before, after, "报错时不能动文件");
        // 也不该凭空造出备份
        let baks: Vec<_> = std::fs::read_dir(dir.join("memory"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert!(baks.is_empty(), "报错时不该写备份");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- project 去向（2026-10-15） ----------------

    const TS3: &str = "洛谷代码放 D:\\allc++\\luogu\\，文件名 P<题号>.cpp。（ts3）";

    /// 核心行为：条目写进 `projects/<名>/MEMORY.md`，并从中转站消失。
    ///
    /// 落点必须与 `pmem::project_memory_md` **同一条路径** —— 蒸馏写进去的
    /// 东西，主对话得能在 system prompt 里读出来才算数。
    #[test]
    fn project_target_writes_into_projects_memory_md() {
        let dir = seed("proj");
        crate::pmem::create_project(&dir, "M3FM", "").unwrap();

        let r = apply_entry(&dir, &prop_proj(TS3, "M3FM", "")).unwrap();
        assert_eq!((r.moved, r.removed), (1, 1));
        assert_eq!(r.written, "projects/M3FM/MEMORY.md", "要逐字说清改的是哪个文件");

        let pm = std::fs::read_to_string(dir.join("projects/M3FM/MEMORY.md")).unwrap();
        assert!(pm.contains(TS3), "条目应进项目记忆: {pm}");
        // 中转站那边必须少掉这条
        let mem = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(!mem.contains("洛谷"), "中转站不该留着: {mem}");
        assert!(mem.contains("称呼约定"), "别的条目不动");
        // 也不能凭空写进 USER.md
        assert!(
            !dir.join("USER.md").exists(),
            "project 去向不碰三份 persona 文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 项目记忆是**另一个文件** → 它自己也得有一份备份，
    /// 否则动错了只能从中转站那份备份里捞，而那份里根本没有项目内容。
    #[test]
    fn project_target_backs_up_the_project_file_too() {
        let dir = seed("proj-bak");
        crate::pmem::create_project(&dir, "论文", "").unwrap();
        let r = apply_entry(&dir, &prop_proj(TS3, "论文", "")).unwrap();
        assert_eq!(r.written, "projects/论文/MEMORY.md");

        let baks: Vec<_> = std::fs::read_dir(dir.join("projects/论文"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .collect();
        assert_eq!(baks.len(), 1, "项目侧按天备份，实际 {}", baks.len());
        let bak = std::fs::read_to_string(baks[0].path()).expect("备份应可读");
        assert!(!bak.contains("洛谷"), "备份是**写入前**的内容: {bak}");
        assert!(bak.contains("项目记忆"), "备份里是原文件全文: {bak}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 项目名写错 → 报错 + 列出可选项目 + **磁盘零改动**。
    ///
    /// 报错必须带清单：模型拿到名字才能一次改对，不用来回试。
    #[test]
    fn unknown_project_name_is_rejected_and_lists_options() {
        let dir = seed("proj-err");
        crate::pmem::create_project(&dir, "M3FM", "").unwrap();
        crate::pmem::create_project(&dir, "论文", "").unwrap();
        let before = std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap();

        let r = apply_entry(&dir, &prop_proj(TS3, "不存在的项目", ""));
        let e = r.unwrap_err();
        assert!(e.contains("M3FM") && e.contains("论文"), "报错要列出可选项目: {e}");
        assert!(e.contains("不存在的项目"), "报错要点名收到的名字: {e}");

        assert_eq!(
            before,
            std::fs::read_to_string(dir.join("memory").join("MEMORY.md")).unwrap(),
            "报错时中转站不能动"
        );
        assert!(!dir.join("projects/不存在的项目").exists(), "不能凭空造目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **路径穿越必须被挡住**：项目名会拼进 `projects/<名>/MEMORY.md`，
    /// 而它是模型给的字符串。`../..` 那种名字若被接受，一行 `distill_move`
    /// 就能把文件写到 agent-data 外面去。
    #[test]
    fn project_name_cannot_escape_the_projects_dir() {
        for evil in ["..", "../evil", "..\\..\\evil", "a/b", "."] {
            let dir = seed("proj-evil");
            crate::pmem::create_project(&dir, "正常项目", "").unwrap();
            let r = apply_entry(&dir, &prop_proj(TS3, evil, ""));
            assert!(r.is_err(), "项目名「{evil}」必须被拒绝");
            assert!(
                !dir.parent().unwrap().join("evil").exists(),
                "不能写到 projects/ 外面去（{evil}）"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// `project` 留空 = 用顶栏激活的项目（与 `remember(scope:"project")` 同规则）。
    ///
    /// 用户已经用顶栏表达了"我现在在哪个项目里"，不该再逼模型复述一遍名字。
    #[test]
    fn empty_project_falls_back_to_the_active_project() {
        let dir = seed("proj-active");
        let a = crate::pmem::create_project(&dir, "激活的", "").unwrap();
        let b = crate::pmem::create_project(&dir, "没激活的", "").unwrap();

        let mut s = crate::config::load_settings(&dir);
        s.active_project_id = Some(a.id);
        crate::config::save_settings(&dir, &s).unwrap();

        let r = apply_entry(&dir, &prop_proj(TS3, "", "")).unwrap();
        assert_eq!(r.written, format!("projects/{}/MEMORY.md", a.name));
        assert!(
            std::fs::read_to_string(dir.join("projects").join(&b.name).join("MEMORY.md"))
                .unwrap()
                .contains("# 项目记忆"),
            "没激活的项目不该被写"
        );
        assert!(std::fs::read_to_string(dir.join("projects").join(&a.name).join("MEMORY.md"))
            .unwrap()
            .contains(TS3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两个都没有时，报错要说清**两条出路**（指定项目名 / 先切顶栏），
    /// 而不是只丢一句"没有项目"。
    #[test]
    fn no_project_at_all_reports_both_ways_out() {
        let dir = seed("proj-none");
        let r = apply_entry(&dir, &prop_proj(TS3, "", ""));
        let e = r.unwrap_err();
        assert!(e.contains("project="), "要告诉模型可以指定名字: {e}");
        assert!(e.contains("顶栏"), "要告诉用户可以切顶栏: {e}");
        assert!(e.contains("一个都没有"), "要说清现状: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 判重：同一条迁两次，第二次必须报错（中转站已无此条），
    /// 且项目记忆里**不能**出现两份。
    #[test]
    fn project_target_dedupes_against_existing_project_memory() {
        let dir = seed("proj-dup");
        crate::pmem::create_project(&dir, "M3FM", "").unwrap();
        // 先用 remember 通道写一条同文的（append_project_memory 会加时间戳后缀，
        // 所以这里直接手写同文 bullet，模拟"已经记过了"）
        let pm = dir.join("projects/M3FM/MEMORY.md");
        let cur = std::fs::read_to_string(&pm).unwrap();
        std::fs::write(&pm, format!("{cur}\n- {TS3}\n")).unwrap();

        let r = apply_entry(&dir, &prop_proj(TS3, "M3FM", "")).unwrap();
        assert_eq!((r.moved, r.removed), (0, 1), "不该重写，但要从中转站摘掉");
        assert!(!r.skipped.is_empty(), "要报告跳过了: {:?}", r.skipped);
        let after = std::fs::read_to_string(&pm).unwrap();
        assert_eq!(
            after.matches("洛谷").count(),
            1,
            "项目记忆里只能有一份: {after}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
