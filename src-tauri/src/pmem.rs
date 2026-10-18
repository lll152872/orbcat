//! 项目索引（仿 WorkBuddy：SQLite **只管索引**，长期记忆仍写 md）
//!
//! ## 分工
//! - `pmem.sqlite`：projects 表 + session→project 绑定（可查询、可切换）
//! - `projects/<name>/MEMORY.md`：项目长期记忆（人可直接改）
//!
//! ## 为什么不用 SQLite 存记忆正文
//! 记忆要人用记事本改；索引要按项目/会话查。两件事分开。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    /// 绑定的工作目录（可空；命中前台/工作目录时可自动建议）
    pub root_path: String,
    pub created_at: String,
}

fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("pmem.sqlite")
}

fn now_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rem = secs % 86400;
    format!(
        "ts{secs:08}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 项目名必须是**目录名的单段**：不能带路径分隔符、不能是 `.` / `..`。
///
/// 为什么这是硬校验：项目名会直接拼进 `projects/<名>/MEMORY.md`（本文件多处，
/// 外加 `distill::project_memory_path` 与 `memory::match_project_memory`）。
/// 名字一旦能带分隔符，`projects/../../evil/MEMORY.md` 就会写到
/// `agent-data` **外面**去 —— 而项目名是用户/模型给的字符串。
///
/// `distill::resolve_project` 只允许名字**精确等于已登记项目**，是第二道闸；
/// 这里是第一道 —— 闸门要设在数据入口，不设在读出口。
fn check_name(name: &str) -> Result<&str, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("项目名不能为空".into());
    }
    if n == "." || n == ".." || n.contains('/') || n.contains('\\') || n.contains('\0') {
        return Err(format!(
            "项目名「{n}」不合法：它会变成 projects/ 下的一个目录，不能包含路径分隔符 \
             （/ \\）、不能是 . 或 .."
        ));
    }
    Ok(n)
}

fn open(data_dir: &Path) -> Result<rusqlite::Connection, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("建数据目录失败: {e}"))?;
    let conn = rusqlite::Connection::open(db_path(data_dir))
        .map_err(|e| format!("打开 pmem.sqlite 失败: {e}"))?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS projects (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            root_path TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS session_project (
            session_id TEXT PRIMARY KEY,
            project_id INTEGER,
            bound_at TEXT NOT NULL
        );
        ",
    )
    .map_err(|e| format!("初始化 pmem 表失败: {e}"))?;
    Ok(conn)
}

pub fn list_projects(data_dir: &Path) -> Result<Vec<ProjectRow>, String> {
    let conn = open(data_dir)?;
    let mut stmt = conn
        .prepare("SELECT id, name, root_path, created_at FROM projects ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                name: r.get(1)?,
                root_path: r.get(2)?,
                created_at: r.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

pub fn create_project(data_dir: &Path, name: &str, root_path: &str) -> Result<ProjectRow, String> {
    let name = check_name(name)?;
    let conn = open(data_dir)?;
    let ts = now_ts();
    conn.execute(
        "INSERT INTO projects (name, root_path, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![name, root_path.trim(), ts],
    )
    .map_err(|e| format!("创建项目失败: {e}"))?;
    // 同步建项目记忆文件（md 是记忆真源）
    let mem_dir = data_dir.join("projects").join(name);
    std::fs::create_dir_all(&mem_dir).map_err(|e| format!("建项目目录失败: {e}"))?;
    let mem = mem_dir.join("MEMORY.md");
    if !mem.exists() {
        let body = format!(
            "# 项目记忆 — {name}\n\n> 跨会话沉淀；主对话切换到本项目后由 system prompt 注入。\n> 用 `remember` 提交候选，或直接改本文件。\n"
        );
        std::fs::write(&mem, body).map_err(|e| format!("写 MEMORY.md 失败: {e}"))?;
    }
    // source.ref：绑定路径时写入，供旧路径匹配逻辑兼容
    if !root_path.trim().is_empty() {
        let _ = std::fs::write(mem_dir.join("source.ref"), root_path.trim());
    }
    let id = conn.last_insert_rowid();
    Ok(ProjectRow {
        id,
        name: name.to_string(),
        root_path: root_path.trim().to_string(),
        created_at: ts,
    })
}

pub fn delete_project(data_dir: &Path, id: i64) -> Result<(), String> {
    let conn = open(data_dir)?;
    conn.execute("DELETE FROM projects WHERE id = ?1", [id])
        .map_err(|e| format!("删除项目失败: {e}"))?;
    conn.execute(
        "DELETE FROM session_project WHERE project_id = ?1",
        [id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 会话绑定到某项目（`project_id` 为 NULL = 主对话/无项目）
pub fn bind_session(data_dir: &Path, session_id: &str, project_id: Option<i64>) -> Result<(), String> {
    let conn = open(data_dir)?;
    conn.execute(
        "INSERT INTO session_project (session_id, project_id, bound_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET project_id = excluded.project_id, bound_at = excluded.bound_at",
        rusqlite::params![session_id, project_id, now_ts()],
    )
    .map_err(|e| format!("绑定会话失败: {e}"))?;
    Ok(())
}

/// 查一个会话绑定到哪个项目（`None` = 主对话/未绑定）。
///
/// ⚠️ **仅供测试**：UI 展示走的是 [`session_map`]（一次性
/// 拿全量映射，避免每行一次 SQLite 往返）。这个单条查询留着给单测断言绑定结果，
/// 生产路径没有调用者。
#[cfg(test)]
pub fn session_project(data_dir: &Path, session_id: &str) -> Result<Option<i64>, String> {
    let conn = open(data_dir)?;
    let mut stmt = conn
        .prepare("SELECT project_id FROM session_project WHERE session_id = ?1")
        .map_err(|e| e.to_string())?;
    let v: Option<Option<i64>> = stmt
        .query_row([session_id], |r| r.get(0))
        .map_err(|e| e.to_string())
        .ok();
    Ok(v.flatten())
}

/// 全量 session_id → 项目名（null = 主对话）。会话列表标签用。
pub fn session_map(
    data_dir: &Path,
) -> Result<std::collections::HashMap<String, Option<String>>, String> {
    let conn = open(data_dir)?;
    let mut names = std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, name FROM projects")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (id, name) = row.map_err(|e| e.to_string())?;
            names.insert(id, name);
        }
    }
    let mut stmt = conn
        .prepare("SELECT session_id, project_id FROM session_project")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (sid, pid) = row.map_err(|e| e.to_string())?;
        out.insert(sid, pid.and_then(|p| names.get(&p).cloned()));
    }
    Ok(out)
}

/// 重命名项目（显示名唯一）
pub fn rename_project(data_dir: &Path, id: i64, new_name: &str) -> Result<(), String> {
    // 与 create_project 同一条闸：重命名会 `fs::rename` 整个目录
    let new_name = check_name(new_name)?;
    let conn = open(data_dir)?;
    let old: String = conn
        .query_row("SELECT name FROM projects WHERE id = ?1", [id], |r| r.get(0))
        .map_err(|_| format!("找不到项目 id={id}"))?;
    if old == new_name {
        return Ok(());
    }
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE name = ?1 AND id != ?2",
            rusqlite::params![new_name, id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if n > 0 {
        return Err(format!("项目名「{new_name}」已存在"));
    }
    conn.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2",
        rusqlite::params![new_name, id],
    )
    .map_err(|e| format!("重命名失败: {e}"))?;
    // 同步目录/MEMORY.md
    let old_dir = data_dir.join("projects").join(&old);
    let new_dir = data_dir.join("projects").join(new_name);
    if old_dir.exists() && !new_dir.exists() {
        let _ = std::fs::rename(&old_dir, &new_dir);
    }
    Ok(())
}

/// 清空项目 MEMORY.md（项目保留）
pub fn clear_project_memory(data_dir: &Path, name: &str) -> Result<(), String> {
    let p = data_dir.join("projects").join(name).join("MEMORY.md");
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, format!("# 项目记忆 — {name}\n\n"))
        .map_err(|e| format!("清空记忆失败: {e}"))
}

/// 读项目 MEMORY.md 全文（注入 system prompt 用）
pub fn project_memory_md(data_dir: &Path, name: &str) -> Option<String> {
    let p = data_dir.join("projects").join(name).join("MEMORY.md");
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 项目名 → `projects/<名>/MEMORY.md`。
///
/// ⚠️ **名字必须先过 `valid_project_name`** —— 项目名来自 UI 与工具调用，
/// 会被拼进路径，空/隐藏/含分隔符/含 `..` 的一律拒绝。
fn project_memory_path(data_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    valid_project_name(name)?;
    Ok(data_dir.join("projects").join(name).join("MEMORY.md"))
}

/// 追加一条项目记忆到 md（有项目时 `remember` 可直接写，不进全局 pending）
pub fn append_project_memory(
    data_dir: &Path,
    name: &str,
    content: &str,
    source: &str,
) -> Result<(), String> {
    let content = content.trim();
    if content.is_empty() {
        return Err("记忆内容不能为空".into());
    }
    let p = project_memory_path(data_dir, name)?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建项目目录失败: {e}"))?;
    }
    if !p.exists() {
        std::fs::write(&p, format!("# 项目记忆 — {}\n\n", name.trim()))
            .map_err(|e| format!("写 MEMORY.md 失败: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&p)
        .map_err(|e| format!("打开 MEMORY.md 失败: {e}"))?;
    use std::io::Write as _;
    writeln!(f, "- {content}（{} · {source}）", now_ts())
        .map_err(|e| format!("追加记忆失败: {e}"))?;
    Ok(())
}

/// 项目名合法性（UI 与工具共用的唯一口径）。
pub fn valid_project_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return Err("非法项目名".into());
    }
    Ok(())
}

/// 读项目 MEMORY.md 全文（UI 编辑器 / 工具校对用）。不存在当空串。
pub fn read_project_memory_file(data_dir: &Path, name: &str) -> Result<String, String> {
    let p = project_memory_path(data_dir, name)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("读项目记忆失败: {e}")),
    }
}

/// 改项目记忆前按天备份（同 [`crate::distill`] 的 `MEMORY.md.bak-<日期>` 命名），
/// 返回备份文件名。同一天再改会覆盖它 —— 想回到更早得自己另存。
fn backup_project_memory(p: &Path) -> Result<String, String> {
    let stamp = crate::distill::ts_compact_pub();
    let name = format!("MEMORY.md.bak-{stamp}");
    let bak = p.with_file_name(&name);
    let prev = std::fs::read_to_string(p).unwrap_or_default();
    std::fs::write(&bak, prev).map_err(|e| format!("备份项目记忆失败: {e}"))?;
    Ok(name)
}

/// **用户手工编辑**项目 MEMORY.md（整份覆盖）。
///
/// 返回备份文件名 —— 按天覆盖，所以同一天改多次只能回到当天第一次的状态。
/// 要更稳就自己拷一份 `MEMORY.md.bak-<日期>`。
pub fn write_project_memory_file(
    data_dir: &Path,
    name: &str,
    text: &str,
) -> Result<String, String> {
    let p = project_memory_path(data_dir, name)?;
    let bak = if p.exists() {
        Some(backup_project_memory(&p)?)
    } else {
        None
    };
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建项目目录失败: {e}"))?;
    }
    let body = if text.trim().is_empty() {
        format!("# 项目记忆 — {}\n\n", name.trim())
    } else {
        let t = text.trim_end();
        if t.ends_with('\n') {
            t.to_string()
        } else {
            format!("{t}\n")
        }
    };
    std::fs::write(&p, body).map_err(|e| format!("写项目记忆失败: {e}"))?;
    Ok(bak.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fa-pmem-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn create_list_bind() {
        let d = tmp("a");
        let p = create_project(&d, "M3FM", "C:\\work\\my-paper").unwrap();
        assert_eq!(p.name, "M3FM");
        assert!(d.join("projects/M3FM/MEMORY.md").exists());
        let all = list_projects(&d).unwrap();
        assert_eq!(all.len(), 1);
        bind_session(&d, "s1", Some(p.id)).unwrap();
        assert_eq!(session_project(&d, "s1").unwrap(), Some(p.id));
        bind_session(&d, "s1", None).unwrap();
        assert_eq!(session_project(&d, "s1").unwrap(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn append_memory_to_md() {
        let d = tmp("b");
        create_project(&d, "论文", "").unwrap();
        append_project_memory(&d, "论文", "Fig.2 是任务表", "model").unwrap();
        let t = project_memory_md(&d, "论文").unwrap();
        assert!(t.contains("Fig.2 是任务表"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 项目名会直接拼进 `projects/<名>/MEMORY.md`，而它是**外部给的字符串**
    /// （用户在 UI 里输入、模型在 distill_move 里传）。带分隔符就必须挡住，
    /// 否则一行调用就能把文件写到 data_dir 外面。
    #[test]
    fn project_name_cannot_contain_path_separators() {
        let d = tmp("evil");
        for bad in ["..", ".", "../evil", "a/b", "a\\b", "  "] {
            assert!(
                create_project(&d, bad, "").is_err(),
                "「{bad}」必须被拒绝"
            );
        }
        assert!(
            !d.parent().unwrap().join("evil").exists(),
            "不能在 projects/ 外面造目录"
        );
        // 正常名仍然能建
        let p = create_project(&d, "M3FM 论文", "C:\\work").unwrap();
        assert_eq!(p.name, "M3FM 论文", "首尾空格要去掉，中间的要保留");
        assert!(d.join("projects/M3FM 论文/MEMORY.md").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_rejects_path_separators_too() {
        let d = tmp("evil2");
        let p = create_project(&d, "论文", "").unwrap();
        assert!(rename_project(&d, p.id, "../跑了").is_err());
        assert_eq!(list_projects(&d).unwrap()[0].name, "论文", "拒绝后不该改名");
        let _ = std::fs::remove_dir_all(&d);
    }
}
