//! 记忆蒸馏的**搬运层**：解析 `memory/MEMORY.md`（中转站）+ 把条目迁到目标文件
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
pub const TARGETS: [&str; 5] = ["user", "soul", "identity", "stay", "duplicate"];

/// 一次迁移的请求（agent 调 `distill_move` 时的参数）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillProposal {
    /// 条目正文（不含行首 `- `）。从中转站移除时按它匹配
    pub text: String,
    /// user | soul | identity | duplicate（stay = 什么都不做）
    pub target: String,
    /// 归到目标文件的哪个小节（如 `## 基本`）；空 = 追加到末尾
    #[serde(default)]
    pub section: String,
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
/// 流程：备份（按天）→ 写目标文件（user/soul/identity）→ 从中转站移除。
/// `target = duplicate` 表示「目标已有同文」：不写文件，只从中转站删。
/// `target = stay`：什么都不做（agent 用它表达「这条留在中转站」）。
pub fn apply_entry(root: &Path, p: &DistillProposal) -> Result<DistillReport, String> {
    let text = p.text.trim();
    if text.is_empty() {
        return Err("条目内容不能为空".into());
    }
    if !TARGETS.contains(&p.target.as_str()) {
        return Err(format!(
            "未知去向 {}（只能是 user / soul / identity / stay / duplicate）",
            p.target
        ));
    }
    let empty = |backup: String| DistillReport {
        moved: 0,
        removed: 0,
        backup,
        skipped: Vec::new(),
    };

    if p.target == "stay" {
        return Ok(empty(String::new()));
    }

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
    let mut skipped: Vec<String> = Vec::new();
    if matches!(p.target.as_str(), "user" | "soul" | "identity") {
        let file = std::fs::read_to_string(target_path(root, &p.target)).unwrap_or_default();
        match insert_into_target(&file, &p.section, text) {
            Some(next) => {
                std::fs::write(target_path(root, &p.target), next)
                    .map_err(|e| format!("写 {} 失败: {e}", p.target))?;
                moved = 1;
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
        skipped,
    })
}

/// 批量迁移（测试与将来「一键应用」用；agent 走 [`apply_entry`] 逐条）
#[cfg(test)]
fn apply_batch(root: &Path, approved: &[DistillProposal]) -> Result<DistillReport, String> {
    let mut moved = 0;
    let mut removed = 0;
    let mut backup = String::new();
    let mut skipped = Vec::new();
    for p in approved {
        let r = apply_entry(root, p)?;
        moved += r.moved;
        removed += r.removed;
        skipped.extend(r.skipped);
        if !r.backup.is_empty() {
            backup = r.backup;
        }
    }
    Ok(DistillReport {
        moved,
        removed,
        backup,
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
            reason: String::new(),
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
}
