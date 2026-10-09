//! 备份与归档（回收站）的**可还原**管理 + 保留策略。
//!
//! ## 为什么需要这个模块（2026-09-30）
//!
//! 在此之前，两个目录都只进不出：
//!
//! 1. `agent-data/backups/` —— `write_file` / `edit_file` 每次写改前 `copy` 一份。
//!    文件名是 `{时间戳}__{文件名}`，**不含原路径**。实测 210 个文件 / 17.4 MB，
//!    而要想还原，用户得自己猜"这个 `README.md` 原来在哪个目录"。
//!    这是**备份机制的根本缺陷**：备份存的是内容，丢的是来路。
//!
//! 2. `agent-data/.trash/` —— `delete_file` 归档到这里，`trash.jsonl` 里
//!    记了原路径。实测 59 个文件 / 85.3 MB（其中 6 个 PDF 占 82 MB）。
//!    记录是全的，但**没有还原入口** —— `delete_file` 的返回文本只能告诉模型
//!    "把归档路径 move 回原路径即可"，等于让用户手工搬文件。
//!
//! ## 本模块做三件事
//!
//! - **记**：备份时写 `backups.jsonl`（原路径 / 时间 / 大小），与 `trash.jsonl`
//!   同构。老备份没有 id 就从文件名反解（`legacy` 标记），**不假装知道**它的原路径。
//! - **还**：`restore_backup` 把备份复制回原路径（**先给现状再备份一次**，
//!   所以"还原"本身也可撤销）；`restore_trash` 把归档 move 回原路径。
//! - **清**：`prune` 按天数 + 条数上限清理，**默认保守**（90 天 / 各 500 条），
//!   且只在用户显式调用时执行 —— 不做后台自动删除。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 默认保留天数。**刻意给得宽**：备份/归档是"后悔药"，宁可占点磁盘，
/// 也别在用户真想找回时发现已经清掉了。真正占地方的是超大文件，
/// 那种情况用户会在设置页看到用量提示后自己点清理。
pub const DEFAULT_KEEP_DAYS: u64 = 90;

/// 默认单目录保留条数上限（备份与归档各自计算）。
pub const DEFAULT_KEEP_ITEMS: usize = 500;

/// 一次备份的来路记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRec {
    /// 备份文件 id（= 文件名）
    pub id: String,
    /// 备份时间（epoch 毫秒）
    pub at: u64,
    /// **原文件路径** —— 这是老备份缺的那一项，也是能还原的前提
    pub original: String,
    /// 备份副本的绝对路径
    pub backup: String,
    /// 字节数
    #[serde(default)]
    pub size: u64,
    /// 老备份（写记录之前产生的）：原路径未知，只能按文件名提示
    #[serde(default)]
    pub legacy: bool,
}

/// 一条归档（回收站）记录 —— 与 `tools.rs::delete_file` 写入的
/// `trash.jsonl` **同一格式**，这里只是把它读回来。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashRec {
    #[serde(default)]
    pub at: u64,
    /// 原路径
    pub original: String,
    /// 归档后的路径
    pub trashed: String,
    /// `file` | `dir`
    #[serde(default)]
    pub kind: String,
    /// 目录归档时包含的文件数
    #[serde(default)]
    pub count: usize,
    /// 归档物是否还在（用户可能手工搬走了）—— **由 list 时探测**，不落盘
    #[serde(default)]
    pub present: bool,
    /// 归档物字节数 —— 同样 list 时探测
    #[serde(default)]
    pub size: u64,
}

/// 保留策略。
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub keep_days: u64,
    pub keep_items: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
            keep_items: DEFAULT_KEEP_ITEMS,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

pub fn trash_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(".trash")
}

fn backups_manifest(data_dir: &Path) -> PathBuf {
    backups_dir(data_dir).join("backups.jsonl")
}

fn trash_manifest(data_dir: &Path) -> PathBuf {
    trash_dir(data_dir).join("trash.jsonl")
}

/// 读一个 JSONL 清单；坏行跳过（用户可能用记事本改过它）。
fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<T>(l).ok())
        .collect()
}

fn append_jsonl<T: Serialize>(path: &Path, rec: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let line = serde_json::to_string(rec).map_err(|e| format!("序列化失败: {e}"))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("打开清单失败: {e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("写清单失败: {e}"))
}

/// 从老备份文件名反解时间戳：`{毫秒}__{文件名}` / `{毫秒}_{n}__{文件名}`。
///
/// 返回 `(时间戳, 展示用文件名)`。反解不出来就给 `(0, 整名)` ——
/// **宁可显示得丑，也不假装知道**。
fn parse_legacy_name(name: &str) -> (u64, String) {
    let Some((head, tail)) = name.split_once("__") else {
        return (0, name.to_string());
    };
    let stamp = head.split('_').next().unwrap_or(head);
    match stamp.parse::<u64>() {
        Ok(v) => (v, tail.to_string()),
        Err(_) => (0, name.to_string()),
    }
}

/// 备份一个文件（写/改之前调用）。
///
/// 与老实现的三点差别：
/// 1. 写 `backups.jsonl` 记录**原路径**，还原才有依据；
/// 2. 返回记录本身（调用方拿去拼提示文本）；
/// 3. 文件不存在时返回 `None`，不建空备份。
pub fn backup_file(data_dir: &Path, path: &Path) -> Result<Option<BackupRec>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let dir = backups_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建备份目录失败: {e}"))?;

    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let at = now_ms();
    let mut id = format!("{at}__{stem}");
    let mut n = 1u32;
    while dir.join(&id).exists() {
        id = format!("{at}_{n}__{stem}");
        n += 1;
    }
    let dest = dir.join(&id);
    std::fs::copy(path, &dest).map_err(|e| format!("备份失败: {e}"))?;
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);

    let rec = BackupRec {
        id,
        at,
        original: path.display().to_string(),
        backup: dest.display().to_string(),
        size,
        legacy: false,
    };
    // ⚠️ 清单写失败**不算备份失败**：副本已经在盘上了，凭文件名也能人工还原。
    //    为一条日志把写操作整个否掉，是拿"可审计"换"可用"，不值。
    if let Err(e) = append_jsonl(&backups_manifest(data_dir), &rec) {
        eprintln!("[orbcat] ⚠️ 备份清单写入失败（副本已存在，不影响还原能力）: {e}");
    }
    Ok(Some(rec))
}

/// 列出所有备份，**新→旧**。同时把磁盘上没进过清单的老备份补进来
/// （`legacy = true`，原路径未知）。
pub fn list_backups(data_dir: &Path) -> Vec<BackupRec> {
    let dir = backups_dir(data_dir);
    let mut recs = read_jsonl::<BackupRec>(&backups_manifest(data_dir));
    let known: std::collections::HashSet<String> = recs.iter().map(|r| r.id.clone()).collect();

    // 补老备份 / 手工丢进目录的副本
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".jsonl") || known.contains(&name) {
                continue;
            }
            if !e.path().is_file() {
                continue;
            }
            let (at, _stem) = parse_legacy_name(&name);
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            recs.push(BackupRec {
                id: name.clone(),
                at,
                // 老备份的真原路径**无从得知**，留空由前端显示"原路径未知"
                original: String::new(),
                backup: e.path().display().to_string(),
                size,
                legacy: true,
            });
        }
    }

    // 清单里记的副本可能已被用户手工删掉 → 标记出来（前端灰显，不给还原按钮）
    recs.retain(|r| Path::new(&r.backup).exists() || Path::new(&backups_dir(data_dir).join(&r.id)).exists());
    recs.sort_by_key(|r| std::cmp::Reverse(r.at));
    recs
}

/// 还原一个备份到指定路径。
///
/// `target` 为空则回原路径。**还原前会把目标的现状再备份一次** ——
/// 否则"还原"就成了不可撤销的覆盖，与项目"可逆操作优先"的底线相悖。
pub fn restore_backup(
    data_dir: &Path,
    id: &str,
    target: Option<&str>,
) -> Result<String, String> {
    let rec = list_backups(data_dir)
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| format!("找不到备份「{id}」"))?;

    let dst = match target.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => PathBuf::from(t),
        None => {
            if rec.original.trim().is_empty() {
                return Err(format!(
                    "备份「{id}」没有记录原路径（是加清单之前的老备份）。\
                     请手动指定还原目标路径。"
                ));
            }
            PathBuf::from(&rec.original)
        }
    };

    let src = PathBuf::from(&rec.backup);
    if !src.exists() {
        // 清单里记的是 id，副本可能在目录里（id == 文件名）
        let alt = backups_dir(data_dir).join(&rec.id);
        if !alt.exists() {
            return Err(format!("备份副本已不存在：{}", rec.backup));
        }
        return copy_over(data_dir, &alt, &dst);
    }
    copy_over(data_dir, &src, &dst)
}

/// 复制覆盖 + 目标现状先备份。
fn copy_over(data_dir: &Path, src: &Path, dst: &Path) -> Result<String, String> {
    let safety = if dst.exists() {
        match backup_file(data_dir, dst)? {
            Some(r) => format!("\n（{} 的**现状**也已备份：{}）", dst.display(), r.id),
            None => String::new(),
        }
    } else {
        String::new()
    };
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    std::fs::copy(src, dst).map_err(|e| format!("还原失败: {e}"))?;
    Ok(format!(
        "已还原 {} ← {}（{} 字节）{safety}",
        dst.display(),
        src.display(),
        std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0)
    ))
}

/// 列出回收站内容，**新→旧**；探测每个归档物是否还在盘上。
pub fn list_trash(data_dir: &Path) -> Vec<TrashRec> {
    let mut recs = read_jsonl::<TrashRec>(&trash_manifest(data_dir));
    for r in recs.iter_mut() {
        let p = PathBuf::from(&r.trashed);
        r.present = p.exists();
        if r.present && r.size == 0 {
            r.size = dir_size(&p);
        }
    }
    recs.sort_by_key(|r| std::cmp::Reverse(r.at));
    recs
}

fn dir_size(p: &Path) -> u64 {
    if p.is_file() {
        return std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(path),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// 从回收站还原一项：`move` 回原路径。
///
/// 为什么用 `rename` 而不是复制：归档本身就是 `rename` 过去的，
/// 原路 `rename` 回来最省事也最不容易出事（同盘必成功、无副本残留）。
pub fn restore_trash(data_dir: &Path, trashed: &str, target: Option<&str>) -> Result<String, String> {
    let recs = list_trash(data_dir);
    let rec = recs
        .iter()
        .find(|r| r.trashed == trashed)
        .ok_or_else(|| format!("回收站清单里没有「{trashed}」"))?;
    if !rec.present {
        return Err(format!("归档物已不在原处（可能被手工移走）：{}", rec.trashed));
    }

    let src = PathBuf::from(&rec.trashed);
    let dst = match target.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => PathBuf::from(t),
        None => PathBuf::from(&rec.original),
    };
    if dst.exists() {
        return Err(format!(
            "目标已存在，拒绝覆盖：{}\n\
             请先把目标移走或改名（归档还原不做覆盖，避免盖掉更新的内容）。",
            dst.display()
        ));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    std::fs::rename(&src, &dst).map_err(|e| format!("还原失败: {e}"))?;
    Ok(format!("已还原 {} → {}", src.display(), dst.display()))
}

/// 清理结果：删了哪些、省了多少字节。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PruneOutcome {
    pub backups_removed: usize,
    pub trash_removed: usize,
    pub bytes_freed: u64,
}

/// 按保留策略清理备份与归档。
///
/// **只在用户显式点击时调用**（设置页「清理」）—— 不做后台自动删除：
/// 这两个目录是后悔药，"程序偷偷清掉"本身就是新的黑箱。
/// 超期且超量的才删，两个条件同时满足才动（任一不满足就保留）。
pub fn prune(data_dir: &Path, keep: Retention) -> Result<PruneOutcome, String> {
    let mut out = PruneOutcome::default();
    let cutoff = now_ms().saturating_sub(keep.keep_days * 86_400_000);

    // ---- 备份 ----
    let mut recs = list_backups(data_dir);
    // list 已是新→旧；保留前 keep_items 条，其余里超过 keep_days 的删掉
    for r in recs.iter().skip(keep.keep_items) {
        if r.at > cutoff && r.at != 0 {
            continue; // 还没超期
        }
        let p = PathBuf::from(&r.backup);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(&p).is_ok() {
            out.backups_removed += 1;
            out.bytes_freed += size;
        }
    }
    prune_manifest(data_dir)?;
    recs.clear();

    // ---- 归档 ----
    let trecs = list_trash(data_dir);
    for r in trecs.iter().skip(keep.keep_items) {
        if r.at > cutoff && r.at != 0 {
            continue;
        }
        let p = PathBuf::from(&r.trashed);
        if !p.exists() {
            continue;
        }
        let size = dir_size(&p);
        let done = if p.is_dir() {
            std::fs::remove_dir_all(&p).is_ok()
        } else {
            std::fs::remove_file(&p).is_ok()
        };
        if done {
            out.trash_removed += 1;
            out.bytes_freed += size;
        }
    }
    // 归档清单没法"删中间行"（同备份），用存在的项重写
    rewrite_trash_manifest(data_dir)?;

    Ok(out)
}

/// 备份清单重写：只留磁盘上还存在的条目（清理后必须重写，否则清单越滚越大）。
fn prune_manifest(data_dir: &Path) -> Result<(), String> {
    let dir = backups_dir(data_dir);
    let recs = read_jsonl::<BackupRec>(&backups_manifest(data_dir));
    let alive: Vec<BackupRec> = recs
        .into_iter()
        .filter(|r| dir.join(&r.id).exists())
        .collect();
    write_manifest(&backups_manifest(data_dir), &alive)
}

fn rewrite_trash_manifest(data_dir: &Path) -> Result<(), String> {
    let recs = read_jsonl::<TrashRec>(&trash_manifest(data_dir));
    let alive: Vec<TrashRec> = recs
        .into_iter()
        .filter(|r| Path::new(&r.trashed).exists())
        .collect();
    write_manifest(&trash_manifest(data_dir), &alive)
}

fn write_manifest<T: Serialize>(path: &Path, recs: &[T]) -> Result<(), String> {
    // ⚠️ 必须先建目录：`prune` 会把清单重写成"只剩活着的条目"，
    //    而全新机器上目录可能还不存在（没有备份也没删过文件时），
    //    直接 fs::write 会报 os error 3（找不到路径）。
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let mut buf = String::new();
    for r in recs {
        if let Ok(l) = serde_json::to_string(r) {
            buf.push_str(&l);
            buf.push('\n');
        }
    }
    // 先写临时文件再 rename：清单被写坏等于"还原入口全丢"，
    // 不值得为省一次 rename 冒半截文件的险。
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, buf).map_err(|e| format!("写清单失败: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换清单失败: {e}"))
}

/// 两个目录的磁盘占用（设置页显示用）。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryStats {
    pub backups_count: usize,
    pub backups_bytes: u64,
    pub trash_count: usize,
    pub trash_bytes: u64,
    /// agent 记录过、但盘上已不存在的老备份数（清单冗余）
    pub stale_records: usize,
}

pub fn stats(data_dir: &Path) -> RecoveryStats {
    let backups = list_backups(data_dir);
    let trash = list_trash(data_dir);
    RecoveryStats {
        backups_count: backups.len(),
        backups_bytes: backups.iter().map(|r| r.size).sum(),
        trash_count: trash.iter().filter(|r| r.present).count(),
        trash_bytes: trash.iter().map(|r| r.size).sum(),
        stale_records: 0,
    }
}

/// 设置页一次请求要的全部内容。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryState {
    pub stats: RecoveryStats,
    pub backups: Vec<BackupRec>,
    pub trash: Vec<TrashRec>,
    /// 默认保留策略（前端把输入框的初值填成它，避免前后端各写一份默认值）
    pub default_keep_days: u64,
    pub default_keep_items: usize,
}

pub fn build_state(data_dir: &Path) -> RecoveryState {
    RecoveryState {
        stats: stats(data_dir),
        backups: list_backups(data_dir),
        trash: list_trash(data_dir),
        default_keep_days: DEFAULT_KEEP_DAYS,
        default_keep_items: DEFAULT_KEEP_ITEMS,
    }
}

// ---------------------------------------------------------------------------
// 诊断包
// ---------------------------------------------------------------------------

/// 诊断包里日志尾部最多带多少字节。
///
/// 为什么截断：`diag.log` 与 `audit-commands.jsonl` 都是**追加写、无轮转**
/// （实测 audit 已 2000+ 条 / 1.85 MB）。诊断包是拿去贴给人和 agent 看的，
/// 塞几 MB 进去没人读得完，也会把要反馈的对话本身挤爆。尾部才是现场。
const DIAG_TAIL_BYTES: usize = 24 * 1024;

/// 读文件尾部（按字节，**不切断 UTF-8 字符**）。
///
/// 为什么不能直接切字节：日志里全是中文，切在字符中间会得到非法 UTF-8，
/// 贴到别处就是一堆 `�`。所以从切点往后找到第一个字符边界。
fn read_tail(path: &Path, max: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    if bytes.len() <= max {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let mut start = bytes.len() - max;
    // UTF-8 续字节是 10xxxxxx；跳到下一个起始字节
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// 生成一份**可粘贴**的诊断包（纯文本）。
///
/// ## 为什么需要（2026-09-30）
///
/// 这个项目出问题时，用户唯一的通道就是**截图一条红色横幅**（工作日志里
/// 反复出现"用户发截图"）。而真正的现场分散在四个地方：
/// `diag.log`（前端启动/异常）、`audit-commands.jsonl`（跑了什么命令）、
/// 会话里的 `kind = "error"` 时间线条目（模型侧错误）、以及各目录的体积
/// （磁盘长胖时的第一手证据）。人工翻这四处等于劝退。
///
/// ## 硬约束
///
/// **必须脱敏**：模型配置里的 api key 一律走 `config::redact_key_str`
/// （项目里"日志不打印明文 key"的唯一收口）。诊断包是要被人转发的，
/// 带明文 key 等于把密钥拱手送出去。
pub fn build_diagnostic(data_dir: &Path) -> String {
    let mut out = String::new();
    let now = now_ms();

    out.push_str("orbcat 诊断包\n");
    out.push_str(&format!("生成时间（epoch ms）: {now}\n"));
    out.push_str(&format!("agent-data: {}\n", data_dir.display()));

    // ---- 体积 ----
    let s = stats(data_dir);
    out.push_str("\n## 目录体积\n");
    out.push_str(&format!(
        "backups: {} 份 / {} 字节\ntrash: {} 项 / {} 字节\n",
        s.backups_count, s.backups_bytes, s.trash_count, s.trash_bytes
    ));
    for name in ["sessions", "images", "skills", "daily", "projects", "tmp"] {
        let p = data_dir.join(name);
        if p.exists() {
            out.push_str(&format!("{name}: {} 字节\n", dir_size(&p)));
        }
    }

    // ---- 模型配置（脱敏）----
    out.push_str("\n## 模型（key 已脱敏）\n");
    // ⚠️ `load_models` 现在返回 `Result`（2026-09-30 改的：文件存在但 JSON 坏掉
    //    时要能区分"没配模型"和"配置读坏了"）。诊断包里把错误**原样写进去** ——
    //    这正是排查时最想知道的事，比一句"未配置模型"有用得多。
    match crate::config::load_models(data_dir) {
        Err(e) => out.push_str(&format!("（模型配置读取失败：{e}）\n")),
        Ok(models) if models.is_empty() => out.push_str("（未配置模型）\n"),
        Ok(models) => {
            for m in models {
                let key = crate::config::redact_key_str(&m.api_key);
                out.push_str(&format!(
                    "- {} | url={} | key={} | maxIn={:?} | tool={} | img={}\n",
                    m.id,
                    m.url,
                    key,
                    m.max_input_tokens,
                    m.supports_tool_call,
                    m.supports_images,
                ));
            }
        }
    }

    // ---- 会话侧错误 ----
    out.push_str("\n## 最近会话里的错误条目\n");
    let mut errs = 0usize;
    for meta in crate::sessions::list(data_dir).into_iter().take(5) {
        let Some(sess) = crate::sessions::load(data_dir, &meta.id) else {
            continue;
        };
        for m in sess.messages.iter().rev().take(20) {
            for st in m.steps.iter().filter(|s| s.kind == "error").take(3) {
                errs += 1;
                out.push_str(&format!(
                    "- [{}] {}: {}\n",
                    crate::history::fmt_local(m.at),
                    sess.title,
                    st.detail.chars().take(300).collect::<String>()
                ));
            }
        }
    }
    if errs == 0 {
        out.push_str("（最近 5 个会话里没有 error 条目 —— 好消息）\n");
    }

    // ---- 日志尾部 ----
    for (label, path) in [
        ("diag.log", data_dir.join("diag.log")),
        ("audit-commands.jsonl", data_dir.join("audit-commands.jsonl")),
    ] {
        out.push_str(&format!("\n## {label}（尾部最多 {DIAG_TAIL_BYTES} 字节）\n"));
        let tail = read_tail(&path, DIAG_TAIL_BYTES);
        if tail.trim().is_empty() {
            out.push_str("（空 / 不存在）\n");
        } else {
            out.push_str(&tail);
            if !tail.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    out
}

/// 生成诊断包并落盘到 `agent-data/diagnostics/diag-<时间戳>.txt`，返回路径。
///
/// 为什么落盘而不只返回文本：文本可以更长、更好读，也让用户能把整个文件
/// 拖进对话里（比粘贴一屏截断的内容完整得多）。
pub fn write_diagnostic(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("diagnostics");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建诊断目录失败: {e}"))?;
    let path = dir.join(format!("diag-{}.txt", now_ms()));
    std::fs::write(&path, build_diagnostic(data_dir)).map_err(|e| format!("写诊断包失败: {e}"))?;
    Ok(path)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orbcat_recovery_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 备份必须记下**原路径** —— 这是老实现缺的那一项，也是能还原的前提
    #[test]
    fn backup_records_original_path() {
        let d = tmp("orig");
        let f = d.join("hello.txt");
        std::fs::write(&f, "v1").unwrap();

        let rec = backup_file(&d, &f).unwrap().expect("应产生备份");
        assert_eq!(rec.original, f.display().to_string());
        assert!(!rec.legacy);
        assert!(Path::new(&rec.backup).exists());

        // 从清单读回来，原路径仍在
        let listed = list_backups(&d);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].original, f.display().to_string());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 空文件不备份（没有内容可保），返回 None 而不是造一个空副本
    #[test]
    fn no_backup_for_missing_file() {
        let d = tmp("missing");
        assert!(backup_file(&d, &d.join("nope.txt")).unwrap().is_none());
        assert!(list_backups(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 还原：内容回到原路径，且**被覆盖的现状也被备份**（还原本身可撤销）
    #[test]
    fn restore_brings_content_back_and_backs_up_current() {
        let d = tmp("restore");
        let f = d.join("doc.md");
        std::fs::write(&f, "原始内容").unwrap();
        let rec = backup_file(&d, &f).unwrap().unwrap();

        std::fs::write(&f, "被改坏的内容").unwrap();
        let msg = restore_backup(&d, &rec.id, None).unwrap();
        assert!(msg.contains("已还原"), "{msg}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "原始内容");

        // 现状（被改坏的版本）也应进了备份 —— 否则还原不可撤销
        let all = list_backups(&d);
        assert!(
            all.iter().any(|r| {
                std::fs::read_to_string(&r.backup).map(|c| c == "被改坏的内容").unwrap_or(false)
            }),
            "还原前应先备份目标现状"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 老备份（无清单、文件名只有时间戳）：能列出来，但**明确标记原路径未知**，
    /// 且不带 target 还原时必须报错 —— 不能瞎猜一个路径把文件写错地方
    #[test]
    fn legacy_backup_is_listed_but_refuses_guess() {
        let d = tmp("legacy");
        let dir = backups_dir(&d);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1700000000000__old.txt"), "旧").unwrap();

        let listed = list_backups(&d);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].legacy, "无清单的备份应标记为 legacy");
        assert!(listed[0].original.is_empty(), "老备份不知道原路径");
        assert_eq!(listed[0].at, 1_700_000_000_000);

        let err = restore_backup(&d, &listed[0].id, None).unwrap_err();
        assert!(err.contains("原路径未知") || err.contains("没有记录原路径"), "{err}");

        // 显式给目标就能还原
        let target = d.join("restored.txt");
        restore_backup(&d, &listed[0].id, Some(&target.display().to_string())).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "旧");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 回收站：列出 → 还原 → 归档物消失、原路径内容回来
    #[test]
    fn trash_roundtrip() {
        let d = tmp("trash");
        let trash = trash_dir(&d);
        std::fs::create_dir_all(&trash).unwrap();
        let original = d.join("gone.txt");
        let archived = trash.join("1700000000001__gone.txt");
        std::fs::write(&archived, "内容").unwrap();
        append_jsonl(
            &trash_manifest(&d),
            &TrashRec {
                at: 1_700_000_000_001,
                original: original.display().to_string(),
                trashed: archived.display().to_string(),
                kind: "file".into(),
                count: 1,
                present: false,
                size: 0,
            },
        )
        .unwrap();

        let listed = list_trash(&d);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].present, "归档物在盘上，应探测为 present");
        assert!(listed[0].size > 0, "应探测出字节数");

        restore_trash(&d, &archived.display().to_string(), None).unwrap();
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "内容");
        assert!(!archived.exists(), "归档物应已移走");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 还原归档**拒绝覆盖**已存在的目标（避免盖掉更新的内容）
    #[test]
    fn trash_restore_refuses_to_overwrite() {
        let d = tmp("trash_clash");
        let trash = trash_dir(&d);
        std::fs::create_dir_all(&trash).unwrap();
        let original = d.join("clash.txt");
        std::fs::write(&original, "新的").unwrap();
        let archived = trash.join("1700000000002__clash.txt");
        std::fs::write(&archived, "旧的").unwrap();
        append_jsonl(
            &trash_manifest(&d),
            &TrashRec {
                at: 1_700_000_000_002,
                original: original.display().to_string(),
                trashed: archived.display().to_string(),
                kind: "file".into(),
                count: 1,
                present: false,
                size: 0,
            },
        )
        .unwrap();

        let err = restore_trash(&d, &archived.display().to_string(), None).unwrap_err();
        assert!(err.contains("拒绝覆盖"), "{err}");
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "新的", "原内容不能被改");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 保留策略：超期 + 超量**两个条件都满足**才删；未超期的一律留
    #[test]
    fn prune_keeps_recent_and_respects_both_conditions() {
        let d = tmp("prune");
        let dir = backups_dir(&d);
        std::fs::create_dir_all(&dir).unwrap();
        let now = now_ms();

        // 3 条新备份（未超期）+ 2 条很老的
        for (i, at) in [now, now - 1000, now - 2000, 1_600_000_000_000, 1_600_000_000_001]
            .iter()
            .enumerate()
        {
            let id = format!("{at}__f{i}.txt");
            std::fs::write(dir.join(&id), "x").unwrap();
            append_jsonl(
                &backups_manifest(&d),
                &BackupRec {
                    id,
                    at: *at,
                    original: d.join(format!("f{i}.txt")).display().to_string(),
                    backup: dir.join(format!("{at}__f{i}.txt")).display().to_string(),
                    size: 1,
                    legacy: false,
                },
            )
            .unwrap();
        }

        // 只留 2 条、90 天 → 5 条里：前 2 条（最新）无论如何都留；
        // 其余 3 条里，只有"超期"的老备份会被删（那两条 2020 年的）
        let out = prune(
            &d,
            Retention {
                keep_days: 90,
                keep_items: 2,
            },
        )
        .unwrap();
        assert_eq!(out.backups_removed, 2, "只该删两条超期的老备份");
        let left = list_backups(&d);
        assert_eq!(left.len(), 3, "未超期的三条必须留下");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 清理后清单必须重写，只留活着的条目（否则清单无限增长）
    #[test]
    fn prune_rewrites_manifest() {
        let d = tmp("prune_manifest");
        let dir = backups_dir(&d);
        std::fs::create_dir_all(&dir).unwrap();
        // 造一条"清单里有、盘上已无"的冗余记录
        append_jsonl(
            &backups_manifest(&d),
            &BackupRec {
                id: "1700000000000__ghost.txt".into(),
                at: 1_700_000_000_000,
                original: d.join("ghost.txt").display().to_string(),
                backup: dir.join("1700000000000__ghost.txt").display().to_string(),
                size: 9,
                legacy: false,
            },
        )
        .unwrap();
        prune(&d, Retention::default()).unwrap();
        let text = std::fs::read_to_string(backups_manifest(&d)).unwrap_or_default();
        assert!(
            !text.contains("ghost"),
            "清理后清单不该再留着已经不存在的条目：{text}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// stats 只统计真实存在的项
    #[test]
    fn stats_counts_what_is_on_disk() {
        let d = tmp("stats");
        let f = d.join("a.txt");
        std::fs::write(&f, "12345").unwrap();
        backup_file(&d, &f).unwrap().unwrap();
        let s = stats(&d);
        assert_eq!(s.backups_count, 1);
        assert_eq!(s.backups_bytes, 5);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---------------- 诊断包 ----------------

    /// 🔴 **诊断包必须脱敏**：它是给人转发用的，带明文 key 等于送密钥
    #[test]
    fn diagnostic_redacts_api_keys() {
        let d = tmp("diag_redact");
        std::fs::write(
            d.join("models.json"),
            r#"[{"id":"m1","name":"M1","url":"https://x/v1","apiKey":"sk-SECRETSECRETSECRET1234","supportsToolCall":true,"supportsImages":false,"maxInputTokens":null,"maxOutputTokens":null}]"#,
        )
        .unwrap();
        std::fs::write(d.join("diag.log"), "启动正常\n").unwrap();

        let text = build_diagnostic(&d);
        assert!(
            !text.contains("SECRETSECRETSECRET"),
            "诊断包泄漏了 api key：{text}"
        );
        assert!(text.contains("sk-SEC"), "应保留可辨认的头几位：{text}");
        assert!(text.contains("启动正常"), "应带上日志尾部");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 日志尾部按**字符边界**截断：中文日志被切在字节中间会产生乱码
    #[test]
    fn diagnostic_tail_does_not_split_utf8() {
        let d = tmp("diag_utf8");
        // 造一段远超上限的中文日志
        let long = "中文日志行，测十次。".repeat(5000);
        std::fs::write(d.join("diag.log"), &long).unwrap();

        let text = build_diagnostic(&d);
        assert!(!text.contains('\u{FFFD}'), "尾部截断不该产生替换字符");
        // 截出来的中文内容应仍是合法 UTF-8（能正常 contains 匹配）
        assert!(text.contains("中文日志行"), "应保留尾部内容");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 空目录也能生成诊断包（不能因为缺文件就失败 —— 出问题时数据目录
    /// 恰恰可能是半损坏状态）
    #[test]
    fn diagnostic_works_on_empty_data_dir() {
        let d = tmp("diag_empty");
        let text = build_diagnostic(&d);
        assert!(text.contains("orbcat 诊断包"));
        assert!(text.contains("## 目录体积"));
        let path = write_diagnostic(&d).unwrap();
        assert!(path.exists(), "应把诊断包落盘");
        let _ = std::fs::remove_dir_all(&d);
    }
}
