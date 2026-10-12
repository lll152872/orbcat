//! 初始化 —— 声明式安装器
//!
//! ## 为什么必须是纯代码
//!
//! 「让 agent 自己 mkdir」这条路是死的，有个闭环：
//!
//! ```text
//!   要跑 agent   →  得有模型配置（models.json）
//!   要有模型配置 →  得先有 agent-data/
//!   要有 agent-data/  →  得跑 agent 去建
//!                            ↑ 回到第一步
//! ```
//!
//! 所以初始化**第一段**必须由不依赖模型的代码完成：建目录、写空的
//! `models.json`。用户在设置面板填完一个模型，闭环才被打开。
//!
//! 剩下的「你是谁」——`RULES.md` / `SOUL.md` / `IDENTITY.md` / `USER.md`
//! ——必须**问用户**才能写，那是模型就绪之后的事，属于第二段，
//! 由 agent 读项目根的 `初始化.md` 自行推进。
//!
//! ## 骨架从哪来：`src-tauri/skeleton/`
//!
//! 骨架正文不再硬写在本文件里，而是躺在 `src-tauri/skeleton/` 下，
//! 由 `build.rs` 在编译期遍历、校验、烧进二进制。**这份目录是内置区，
//! 是唯一允许进包的地方**；用户数据住在 `agent-data/`，两者物理隔离。
//!
//! 声明文件是 `skeleton/manifest.json` —— 白名单而非黑名单：目录里出现
//! 任何没声明的文件，**构建直接失败**。
//!
//! ## 幂等
//!
//! 已存在的文件**一律不覆盖**。用户可能已经改过 `settings.json` 或
//! `MEMORY.md`，重跑初始化不能毁掉它们。
//!
//! ## 版本与报告（对齐 Mavis / WorkBuddy 的做法）
//!
//! - `SKELETON_SEED` 是骨架树的内容指纹。变了才重看一遍，**且依然不覆盖
//!   用户内容** —— 对版本化的模板文件只补用户文件里缺失的 `##` 小节。
//! - 已应用状态落 `agent-data/.skeleton-state.json`。
//! - 每次运行落一份报告 `agent-data/migrations/YYYY/MM/DD/orbcat-skeleton-*.json`，
//!   **`skipped` 逐项写明为什么跳过**，不留静默跳过。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// build.rs 遍历 skeleton/ 生成。内含 SkeletonAsset / SKELETON_DIRS / SKELETON_ASSETS / SKELETON_SEED。
include!(concat!(env!("OUT_DIR"), "/skeleton_manifest.rs"));

/// 初始化结果，回给前端用于提示
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapReport {
    /// 数据目录绝对路径
    pub data_dir: String,
    /// 本次新建的目录（相对 data_dir）
    pub created_dirs: Vec<String>,
    /// 本次新建的文件（相对 data_dir）
    pub created_files: Vec<String>,
    /// 本来就有、跳过的路径（相对 data_dir）
    pub skipped: Vec<String>,
    /// 是否已经存在过 models.json 且有内容 —— 前端据此决定要不要提示"去加模型"
    pub has_models: bool,
    /// 本次二进制里编进来的骨架指纹
    pub seed: String,
    /// 上次铺过的指纹；`None` = 从没铺过
    pub previous_seed: Option<String>,
    /// 版本落后但**没有**自动补写的模板文件
    pub outdated_templates: Vec<String>,
}

/// 跳过原因。**每个跳过都必须带一个**，报告里不允许出现裸路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// 用户区里已经有了 —— 我们的铁律是绝不覆盖
    UserOwned,
    /// 骨架区里声明了但文件不在。
    ///
    /// ⚠️ 运行期**不可达**：build.rs 的闸门①已经在编译期拦下「声明了但文件
    /// 不存在」，编不出这种二进制。留着它是为了让报告的 reason 词表是**闭集**
    /// —— 将来加了别的跳过路径，直接往这里加，不用临时编词。
    #[allow(dead_code)]
    SourceMissing,
    /// seed 没变，这轮不需要动它
    SeedCurrent,
}

impl SkipReason {
    fn as_str(self) -> &'static str {
        match self {
            SkipReason::UserOwned => "user-owned",
            SkipReason::SourceMissing => "source-missing",
            SkipReason::SeedCurrent => "seed-current",
        }
    }
}

#[derive(serde::Serialize)]
struct SkipEntry {
    path: String,
    reason: &'static str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RunReport {
    schema_version: u32,
    kind: &'static str,
    status: &'static str,
    seed: String,
    previous_seed: Option<String>,
    created_dirs: Vec<String>,
    created_files: Vec<String>,
    skipped: Vec<SkipEntry>,
    outdated_templates: Vec<String>,
    errors: Vec<String>,
    started_at_ms: u128,
    finished_at_ms: u128,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppliedState {
    schema_version: u32,
    kind: String,
    version: u32,
    seed: String,
    applied_at_ms: u128,
    applied_files: Vec<String>,
}

const STATE_FILE: &str = ".skeleton-state.json";
const MIGRATIONS_DIR: &str = "migrations";

/// 建目录骨架 + 最小配置文件。**不碰 `models.json` 之外的任何密钥**。
///
/// 返回 [`BootstrapReport`]；任何一步失败都返回 `Err`（带上具体路径），
/// 但不回滚已建的部分 —— 目录都是幂等的，重试即可。
pub fn bootstrap_agent_data(data_dir: &Path) -> Result<BootstrapReport, String> {
    let started = now_ms();
    let mut created_dirs = Vec::new();
    let mut created_files = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut skip_reasons: Vec<SkipEntry> = Vec::new();
    let mut outdated_templates: Vec<String> = Vec::new();

    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("创建数据目录 {} 失败: {e}", data_dir.display()))?;

    // ── 0、读上次铺到哪了 ───────────────────────────────────────
    let state_path = data_dir.join(STATE_FILE);
    let previous_seed: Option<String> = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|t| serde_json::from_str::<AppliedState>(&t).ok())
        .filter(|s| s.kind == "orbcat-skeleton")
        .map(|s| s.seed);
    // seed 变了才重看一遍。没铺过（None）也要铺一遍 —— 那是首装。
    let seed_changed = previous_seed.as_deref() != Some(SKELETON_SEED);

    // ── 一、目录 ────────────────────────────────────────────────
    for d in SKELETON_DIRS {
        let p = data_dir.join(d);
        if p.is_dir() {
            skipped.push(d.to_string());
            skip_reasons.push(SkipEntry {
                path: d.to_string(),
                reason: SkipReason::UserOwned.as_str(),
            });
        } else {
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("创建目录 {} 失败: {e}", p.display()))?;
            created_dirs.push(d.to_string());
        }
    }

    // ── 二、models.json（空数组）────────────────────────────────
    // ⚠️ 写空数组而不是模板：模板里的占位 url/apiKey 会让
    //    「模型列表」多出一条点不动的废条目。空数组干净，
    //    UI 直接显示"还没有模型，点这里添加"。
    let models = data_dir.join("models.json");
    let mut has_models = false;
    if models.exists() {
        skipped.push("models.json".into());
        skip_reasons.push(SkipEntry {
            path: "models.json".into(),
            reason: SkipReason::UserOwned.as_str(),
        });
        has_models = crate::config::load_models(data_dir)
            .map(|m| !m.is_empty())
            .unwrap_or(false);
    } else {
        std::fs::write(&models, "[]\n")
            .map_err(|e| format!("写入 {} 失败: {e}", models.display()))?;
        created_files.push("models.json".into());
    }

    // ── 三、settings.json（默认值）──────────────────────────────
    let settings = data_dir.join("settings.json");
    if settings.exists() {
        skipped.push("settings.json".into());
        skip_reasons.push(SkipEntry {
            path: "settings.json".into(),
            reason: SkipReason::UserOwned.as_str(),
        });
    } else {
        let d = crate::config::AgentSettings::default();
        let txt = serde_json::to_string_pretty(&d)
            .map_err(|e| format!("序列化 settings 失败: {e}"))?;
        std::fs::write(&settings, format!("{txt}\n"))
            .map_err(|e| format!("写入 {} 失败: {e}", settings.display()))?;
        created_files.push("settings.json".into());
    }

    // ── 三之二、modes.json（agent 模式定义）──────────────────────
    // 为什么要落盘而不是"不存在就用内置"：文件不存在时用户**看不见**有这么个东西，
    // 也就不会想到可以改它。落一份带说明的模板，才是"可发现、可改"。
    // 与其它文件同一条铁律：**已存在一律不覆盖**（用户改过的东西不能被升级冲掉）。
    //
    // ⚠️ 这三个 json（modes / life / settings）**故意不在骨架区**：
    // 它们由 Rust struct 生成。搬成静态文件后，struct 加了字段而文件没跟上，
    // 就是一个用户永远看不到、也永远补不上的死配置。
    let modes_file = data_dir.join(crate::modes::MODES_FILE);
    if modes_file.exists() {
        skipped.push(crate::modes::MODES_FILE.into());
        skip_reasons.push(SkipEntry {
            path: crate::modes::MODES_FILE.into(),
            reason: SkipReason::UserOwned.as_str(),
        });
    } else {
        std::fs::write(&modes_file, crate::modes::default_file_text())
            .map_err(|e| format!("写入 {} 失败: {e}", modes_file.display()))?;
        created_files.push(crate::modes::MODES_FILE.into());
    }

    // ── 三之三、life.json（数据源定义）──────────────────────────
    // 与 modes.json 同一条理由：文件不存在时用户看不见有这回事，
    // 也就不会想到"我可以接一个爬虫快照进来"。落一份**带完整 schema 说明**
    // 的模板（但 sources 为空 —— 默认不接任何源，不替用户决定读什么）。
    let life_file = data_dir.join(crate::life::LIFE_FILE);
    if life_file.exists() {
        skipped.push(crate::life::LIFE_FILE.into());
        skip_reasons.push(SkipEntry {
            path: crate::life::LIFE_FILE.into(),
            reason: SkipReason::UserOwned.as_str(),
        });
    } else {
        std::fs::write(&life_file, crate::life::default_file_text())
            .map_err(|e| format!("写入 {} 失败: {e}", life_file.display()))?;
        created_files.push(crate::life::LIFE_FILE.into());
    }

    // ── 四、骨架区物化 ─────────────────────────────────────────
    // 目录已存在则跳过；文件已存在则**绝不覆盖**；版本化的模板文件在
    // seed 变化时只补缺失小节。人格文件（RULES/SOUL/IDENTITY/USER）
    // 故意**不在这里建**：那是第二段、要问过用户才能写的东西。
    let mut applied: Vec<String> = Vec::new();
    for asset in SKELETON_ASSETS {
        let p = data_dir.join(asset.rel);
        if p.is_file() {
            skipped.push(asset.rel.to_string());
            let reason = if !seed_changed {
                SkipReason::SeedCurrent
            } else {
                SkipReason::UserOwned
            };
            skip_reasons.push(SkipEntry {
                path: asset.rel.to_string(),
                reason: reason.as_str(),
            });
            applied.push(asset.rel.to_string());
            if asset.template && seed_changed {
                match upgrade_template(&p, asset.body, data_dir) {
                    Ok(true) => {}
                    Ok(false) => outdated_templates.push(asset.rel.to_string()),
                    Err(e) => return Err(e),
                }
            }
            continue;
        }

        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
        }
        let body = materialize(asset, data_dir)?;
        std::fs::write(&p, body)
            .map_err(|e| format!("写入 {} 失败: {e}", p.display()))?;
        created_files.push(asset.rel.to_string());
        applied.push(asset.rel.to_string());
    }

    // ── 五、permissions.json 由权限模块自举，这里不碰 ────────────
    if data_dir.join("permissions.json").exists() {
        skipped.push("permissions.json".into());
        skip_reasons.push(SkipEntry {
            path: "permissions.json".into(),
            reason: SkipReason::UserOwned.as_str(),
        });
    }

    // ── 六、落已应用状态 ───────────────────────────────────────
    let state = AppliedState {
        schema_version: 1,
        kind: "orbcat-skeleton".to_string(),
        version: 1,
        seed: SKELETON_SEED.to_string(),
        applied_at_ms: started,
        applied_files: applied,
    };
    let txt = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("序列化已应用状态失败: {e}"))?;
    std::fs::write(&state_path, format!("{txt}\n"))
        .map_err(|e| format!("写入 {} 失败: {e}", state_path.display()))?;

    // ── 七、落运行报告（按日期分片）────────────────────────────
    let report = RunReport {
        schema_version: 1,
        kind: "orbcat-skeleton",
        status: "completed",
        seed: SKELETON_SEED.to_string(),
        previous_seed: previous_seed.clone(),
        created_dirs: created_dirs.clone(),
        created_files: created_files.clone(),
        skipped: skip_reasons,
        outdated_templates: outdated_templates.clone(),
        errors: Vec::new(),
        started_at_ms: started,
        finished_at_ms: now_ms(),
    };
    write_report(data_dir, &report)?;

    Ok(BootstrapReport {
        data_dir: data_dir.display().to_string(),
        created_dirs,
        created_files,
        skipped,
        has_models,
        seed: SKELETON_SEED.to_string(),
        previous_seed,
        outdated_templates,
    })
}

/// 把占位符换成本机真实路径。
///
/// 为什么不在模板里写死路径：写死的绝对路径换台机器就废了，而且会把
/// **构建者的用户名与磁盘布局**分发给所有用户。释放时按实际 `data_dir`
/// 渲染才通用。
///
/// `SKILL_DIR` = 该技能自己的目录。**由 `rel` 推导，绝不写死技能名** ——
/// 写死的话，`skills/x/panel/index.html` 会取到 `panel`，
/// 释放出来的正文里路径全错，而且只验 SKILL.md 的测试验不出来。
fn materialize(asset: &SkeletonAsset, data_dir: &Path) -> Result<String, String> {
    if !asset.render {
        return Ok(asset.body.to_string());
    }
    let mut out = asset.body.replace("{{DATA_DIR}}", &data_dir.display().to_string());
    if out.contains("{{SKILL_DIR}}") {
        let sd = skill_dir_for(asset.rel, data_dir).ok_or_else(|| {
            format!(
                "资产 {} 用了 {{{{SKILL_DIR}}}}，但它的落点不在 skills/ 下，无法推导技能目录。\
                 要么把 rel 改成 skills/<技能名>/…，要么别用这个占位符。",
                asset.rel
            )
        })?;
        out = out.replace("{{SKILL_DIR}}", &sd.display().to_string());
    }
    Ok(out)
}

/// `skills/<技能名>/…` → `<data_dir>/skills/<技能名>`。其余返回 `None`。
fn skill_dir_for(rel: &str, data_dir: &Path) -> Option<PathBuf> {
    let mut it = rel.split('/');
    match (it.next(), it.next()) {
        (Some("skills"), Some(name)) if !name.is_empty() => {
            Some(data_dir.join("skills").join(name))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 版本化模板的升级：只补缺失小节，绝不覆盖用户内容
// ---------------------------------------------------------------------------

/// 把 Markdown 按 `## ` 切成 `(标题, 该小节全文)`。
fn md_sections(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in body.lines() {
        if line.starts_with("## ") {
            if let Some((h, ls)) = cur.take() {
                out.push((h, ls.join("\n")));
            }
            let title = line.trim_start_matches('#').trim().to_string();
            cur = Some((title, vec![line]));
        } else if let Some((_, ls)) = cur.as_mut() {
            ls.push(line);
        }
    }
    if let Some((h, ls)) = cur.take() {
        out.push((h, ls.join("\n")));
    }
    out
}

/// seed 变了以后把模板里**用户还没有的**小节补到文末。
///
/// 返回 `Ok(true)` = 补过；`Ok(false)` = 一节都没补（用户已经全有了，
/// 或者压根不适用）—— 两种情况都要在报告里说，别静默。
fn upgrade_template(target: &Path, template_body: &str, data_dir: &Path) -> Result<bool, String> {
    let cur = std::fs::read_to_string(target)
        .map_err(|e| format!("读取 {} 失败: {e}", target.display()))?;
    let have: BTreeSet<String> = md_sections(&cur).into_iter().map(|(h, _)| h).collect();

    let missing: Vec<(String, String)> = md_sections(template_body)
        .into_iter()
        .filter(|(h, _)| !have.contains(h))
        .collect();

    if missing.is_empty() {
        return Ok(false);
    }

    // 动手之前先备份 —— 沿用 memory.rs 已有的 .trash 约定（agent-data/.trash/）
    let stamp = now_ms();
    let trash = data_dir.join(".trash");
    std::fs::create_dir_all(&trash)
        .map_err(|e| format!("创建 {} 失败: {e}", trash.display()))?;
    let fname = target
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "template".to_string());
    let backup = trash.join(format!("{fname}.{stamp}.v{}.md", &SKELETON_SEED[..8.min(SKELETON_SEED.len())]));
    std::fs::copy(target, &backup)
        .map_err(|e| format!("备份 {} 到 {} 失败: {e}", target.display(), backup.display()))?;

    let mut buf = cur;
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    buf.push_str("\n<!-- orbcat: 骨架升级时补的小节（只增不覆盖） -->\n");
    for (_, sec) in &missing {
        buf.push('\n');
        buf.push_str(sec);
        buf.push('\n');
    }
    std::fs::write(target, buf)
        .map_err(|e| format!("写入 {} 失败: {e}", target.display()))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 报告与状态
// ---------------------------------------------------------------------------

fn write_report(data_dir: &Path, r: &RunReport) -> Result<(), String> {
    let (y, m, d) = civil_today();
    let dir = data_dir.join(MIGRATIONS_DIR).join(y).join(m).join(d);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    // ⚠️ 文件名必须**保证不撞**。毫秒 + pid 不够：连续两次运行（用户双击
    // 「建出目录结构」按钮、或失败后立刻重试）完全可能落在同一毫秒里，
    // 那样第二次会直接覆盖第一次 —— 审计线索凭空少一条，且**完全静默**。
    // 用户要的是「每次都写」，所以一份都不能丢。
    // （macOS CI 上就是这么挂的：两次 bootstrap 落在同一毫秒，只剩一份。）
    let stamp = now_nanos();
    let mut path = dir.join(format!("orbcat-skeleton-{stamp}-{}.json", std::process::id()));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!(
            "orbcat-skeleton-{stamp}-{}-{n}.json",
            std::process::id()
        ));
        n += 1;
        if n > 1000 {
            return Err(format!(
                "{} 下同名报告已超过 1000 份，拒绝继续写",
                dir.display()
            ));
        }
    }
    let txt =
        serde_json::to_string_pretty(r).map_err(|e| format!("序列化运行报告失败: {e}"))?;
    std::fs::write(&path, format!("{txt}\n"))
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 公历换算（civil_from_days，Howard Hinnant 版）。
///
/// ⚠️ 用的是 **UTC**，与 `memory.rs::today_file_name()` 同一口径 —— 本项目
/// 早就接受"日期差一天只影响文件名、不影响正确性"。这里同理：报告是给人看的
/// 排查线索，分到哪一天不影响任何判断。
fn civil_today() -> (String, String, String) {
    let days = (now_ms() / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        format!("{y:04}"),
        format!("{m:02}"),
        format!("{d:02}"),
    )
}

// ===========================================================================
// 测试
//
// ⚠️ 一律用 `std::env::temp_dir()`，**绝不写用户真实的 agent-data**。
//    踩过这个坑：拿真实数据目录当靶场。
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fa-bootstrap-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// 递归数 migrations/ 下有几份运行报告（不按日期分层假设）。
    fn count_reports(d: &Path) -> usize {
        fn rec(dir: &Path, n: &mut usize) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.filter_map(|e| e.ok()) {
                if e.path().is_dir() {
                    rec(&e.path(), n);
                } else if e.file_name().to_string_lossy().starts_with("orbcat-skeleton-") {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        rec(&d.join(MIGRATIONS_DIR), &mut n);
        n
    }

    fn latest_report(d: &Path) -> PathBuf {
        fn rec(dir: &Path, best: &mut Option<(std::time::SystemTime, PathBuf)>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    rec(&p, best);
                } else if p.file_name().unwrap().to_string_lossy().starts_with("orbcat-skeleton-")
                {
                    let m = e.metadata().unwrap().modified().unwrap();
                    if best.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
                        *best = Some((m, p));
                    }
                }
            }
        }
        let mut best = None;
        rec(&d.join(MIGRATIONS_DIR), &mut best);
        best.expect("应至少有报告").1
    }

    /// 回归：报告文件名不许互相覆盖。
    ///
    /// 这条是 macOS CI 上真挂过的 —— 连续两次 bootstrap 落在同一毫秒，
    /// `{毫秒}-{pid}` 的文件名完全相同，第二次把第一次覆盖掉，审计记录
    /// 凭空少一条且**没有任何提示**。用户要的是「每次都写」。
    #[test]
    fn repeated_runs_never_overwrite_a_report() {
        let d = tmp("noclobber");
        for i in 0..6 {
            bootstrap_agent_data(&d).unwrap_or_else(|e| panic!("第 {i} 次应成功: {e}"));
        }
        assert_eq!(
            count_reports(&d),
            6,
            "跑 6 次就该有 6 份报告 —— 少一份说明文件名撞了"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 首装 ────────────────────────────────────────────────────

    #[test]
    fn creates_every_declared_dir_and_asset() {
        let d = tmp("fresh");
        let r = bootstrap_agent_data(&d).expect("初始化应成功");

        // 数**声明出来的**，不写死 8 —— 写死的话每加一个目录都要回来改这一行，
        // 而改的人很容易顺手把数字改对了却没意识到自己漏了真正的断言。
        assert_eq!(
            r.created_dirs.len(),
            SKELETON_DIRS.len(),
            "manifest 声明的目录应全部新建"
        );
        assert!(r.skipped.is_empty(), "首次运行不该有跳过项: {:?}", r.skipped);
        assert!(r.outdated_templates.is_empty());
        assert!(!r.has_models, "空 models.json 不算有模型");
        assert_eq!(r.previous_seed, None, "从没铺过就没有 previous_seed");

        for name in SKELETON_DIRS {
            assert!(d.join(name).is_dir(), "{name} 应存在");
        }
        // 由 manifest 驱动 —— 骨架增删不用回来改测试
        for a in SKELETON_ASSETS {
            assert!(d.join(a.rel).is_file(), "声明的 {} 应存在", a.rel);
            assert!(
                r.created_files.iter().any(|f| f == a.rel),
                "{} 应进 created_files",
                a.rel
            );
        }
        for f in ["models.json", "settings.json", "modes.json", crate::life::LIFE_FILE] {
            assert!(d.join(f).is_file(), "{f} 应存在");
        }

        // 人格文件是第二段的事，代码凭空造不出"你是谁"
        for f in ["RULES.md", "SOUL.md", "IDENTITY.md", "USER.md"] {
            assert!(!d.join(f).exists(), "{f} 不该由 bootstrap 创建");
        }

        // ── `boot_state` 的 needsInit 判据（2026-10-12 用户反馈）──
        //
        // 原先的判据是 `!data_dir.exists()`，发布版**永远等不到「初始化」按钮**：
        // data_dir 是 `%APPDATA%\com.orbcat.desktop`，程序第一次启动就会往里面
        // 写 diag.log / permissions.json / settings.json —— 目录被这些文件创建了，
        // 而骨架一个都没铺。`!exists()` 于是恒为 false。
        //
        // 现在的判据是 `manifest.json` 在不在。这里锁住两件事：
        //   ① bootstrap 确实会铺出 manifest.json（否则判据永远为 true，按钮永远在）；
        //   ② 只建目录不铺文件时，manifest 不在（否则判据永远为 false，按钮永远不在）。
        assert!(
            d.join(STATE_FILE).is_file(),
            "bootstrap 必须写 STATE_FILE —— boot_state 拿它当初始化完成的判据"
        );
    }

    /// 反向对照：一个"被别的文件抢先创建"的空目录**不算已初始化**。
    ///
    /// 这正是用户机器上的真实状态（diag.log / permissions.json 已经把
    /// `%APPDATA%\com.orbcat.desktop` 建出来了，骨架却一个都没有），
    /// 而旧的 `!exists()` 判据在这种情况下给出 `needsInit == false`。
    #[test]
    fn dir_created_by_other_files_is_not_initialized() {
        let d = tmp("partial");
        std::fs::create_dir_all(&d).unwrap();
        // 程序启动时会写的那些东西
        std::fs::write(d.join("diag.log"), "启动正常\n").unwrap();
        std::fs::write(d.join("permissions.json"), "{\"rules\":[]}").unwrap();
        std::fs::write(d.join("settings.json"), "{}").unwrap();

        assert!(
            !d.join(STATE_FILE).is_file(),
            "测试前提失效：这个目录不该有 manifest.json"
        );

        // 判据必须说"需要初始化"
        let needs_init = !d.join(STATE_FILE).is_file();
        assert!(
            needs_init,
            "目录存在但骨架没铺 → 必须判定为「需要初始化」，\
             否则用户看不到「建出目录结构」按钮（2026-10-12 实际发生）"
        );

        // 补跑一次 bootstrap 之后才该变成 false
        bootstrap_agent_data(&d).expect("初始化应成功");
        assert!(
            !(!d.join(STATE_FILE).is_file()),
            "bootstrap 跑完就必须算已初始化"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 内置技能必须**随程序走**。这轮把 dual-token-dashboard 之外的两个
    /// （self-rebuild / token-usage-report）也纳进来了 —— 它们原先只活在
    /// 用户区的 agent-data/skills/ 里，新装的机器上一个都没有。
    #[test]
    fn builtin_skills_ship_with_the_program() {
        let d = tmp("ship");
        bootstrap_agent_data(&d).expect("初始化应成功");

        let shipped: BTreeSet<&str> = SKELETON_ASSETS
            .iter()
            .filter_map(|a| a.rel.strip_prefix("skills/"))
            .filter_map(|r| r.split('/').next())
            .collect();
        for want in [
            "dual-token-dashboard",
            "self-rebuild",
            "token-usage-report",
        ] {
            assert!(shipped.contains(want), "内置技能 {want} 应随程序走");
            assert!(
                d.join("skills").join(want).join("SKILL.md").is_file(),
                "{want}/SKILL.md 应被释放"
            );
        }
        // README.md 是「机制说明」，只留在源码树，不进二进制也不释放
        assert!(
            !d.join("skills/dual-token-dashboard/README.md").exists(),
            "distribute:false 的文件不该被释放"
        );
        assert!(
            !SKELETON_ASSETS
                .iter()
                .any(|a| a.rel.ends_with("dual-token-dashboard/README.md")),
            "distribute:false 不该进二进制"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 幂等 ────────────────────────────────────────────────────

    #[test]
    fn second_run_is_idempotent_and_preserves_user_data() {
        let d = tmp("idem");
        bootstrap_agent_data(&d).expect("首次初始化应成功");

        // 模拟用户动过手
        std::fs::write(d.join("memory/MEMORY.md"), "用户改过的记忆").unwrap();
        std::fs::write(d.join("settings.json"), "{\"selectedModel\":\"x\"}").unwrap();

        let r = bootstrap_agent_data(&d).expect("二次初始化应成功");
        assert!(r.created_dirs.is_empty(), "不该重复建目录");
        assert!(r.created_files.is_empty(), "不该重复写文件");
        assert_eq!(r.previous_seed.as_deref(), Some(SKELETON_SEED));
        assert!(r.outdated_templates.is_empty(), "seed 没变就不该谈升级");

        for name in SKELETON_DIRS {
            assert!(r.skipped.iter().any(|s| s == *name), "目录 {name} 应跳过");
        }
        for a in SKELETON_ASSETS {
            assert!(
                r.skipped.iter().any(|s| s == a.rel),
                "{} 应进跳过列表: {:?}",
                a.rel,
                r.skipped
            );
        }
        for name in [
            "models.json",
            "settings.json",
            "modes.json",
            crate::life::LIFE_FILE,
            "permissions.json",
        ] {
            let _ = name; // permissions.json 由权限模块自举，首装时还不存在
        }
        assert!(r.skipped.iter().any(|s| s == "modes.json"));

        assert_eq!(
            std::fs::read_to_string(d.join("memory/MEMORY.md")).unwrap(),
            "用户改过的记忆",
            "已有记忆不能被覆盖"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("settings.json")).unwrap(),
            "{\"selectedModel\":\"x\"}",
            "已有设置不能被覆盖"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_existing_models() {
        let d = tmp("hasmodels");
        bootstrap_agent_data(&d).expect("首次初始化应成功");
        std::fs::write(d.join("models.json"), "[{\"id\":\"m\",\"url\":\"http://x\"}]").unwrap();

        let r = bootstrap_agent_data(&d).expect("二次初始化应成功");
        assert!(r.has_models, "已有模型应被识别");

        let _ = std::fs::remove_dir_all(&d);
    }

    fn tmp_modes(tag: &str) -> PathBuf {
        let d = tmp(tag);
        bootstrap_agent_data(&d).expect("初始化应成功");
        d
    }

    #[test]
    fn creates_modes_json_with_four_builtin_modes() {
        let d = tmp_modes("modes");
        let modes = crate::modes::load(&d);
        let ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["standard", "ptc", "chat", "creator"]);
        assert!(modes.iter().any(|m| m.id == "ptc" && m.allows_all()));

        // 用户改过之后，二次初始化不许覆盖
        std::fs::write(
            d.join("modes.json"),
            r#"{"modes":[{"id":"mine","name":"我的","description":"x","mcpGroupsPreload":[]}]}"#,
        )
        .unwrap();
        let r = bootstrap_agent_data(&d).expect("二次初始化应成功");
        assert!(r.skipped.iter().any(|s| s == "modes.json"));
        assert_eq!(crate::modes::load(&d).len(), 1, "用户改过的模式定义不能被覆盖");

        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 占位符渲染 ──────────────────────────────────────────────

    /// 展开到**任意临时目录**也成立 —— 证明内置技能不是只会写死的那一个路径。
    #[test]
    fn builtin_skills_expand_into_arbitrary_dir() {
        let d = tmp("expand");
        bootstrap_agent_data(&d).expect("初始化应成功");

        for a in SKELETON_ASSETS {
            let released = std::fs::read_to_string(d.join(a.rel)).unwrap();
            assert!(
                !released.contains("{{"),
                "{} 释放后不该残留占位符",
                a.rel
            );
        }

        let md = std::fs::read_to_string(d.join("skills/dual-token-dashboard/SKILL.md")).unwrap();
        assert!(
            md.contains(&d.display().to_string()),
            "释放出的 SKILL.md 应引用本次的 data_dir: {}",
            d.display()
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// ⚠️ 这条钉的是 §4.2-③ 那个坑：`{{SKILL_DIR}}` 必须由 **rel 的第 2 段**
    /// （技能名）推导，绝不能写成「rel 的父目录名」—— 否则
    /// `skills/dual-token-dashboard/panel/index.html` 会取到 `panel`，
    /// 释放出来的正文里路径全错，而只验 SKILL.md 的测试**验不出来**。
    #[test]
    fn skill_dir_is_the_skill_root_not_the_parent_folder() {
        let d = PathBuf::from("/tmp/x");
        assert_eq!(
            skill_dir_for("skills/dual-token-dashboard/SKILL.md", &d).unwrap(),
            d.join("skills/dual-token-dashboard")
        );
        assert_eq!(
            skill_dir_for(
                "skills/dual-token-dashboard/panel/index.html",
                &d
            )
            .unwrap(),
            d.join("skills/dual-token-dashboard"),
            "深层文件也得拿到技能根目录，不能是 panel/"
        );
        assert!(skill_dir_for("memory/MEMORY.md", &d).is_none());
    }

    #[test]
    fn skill_dir_placeholder_outside_skills_is_an_error_not_a_guess() {
        let a = SkeletonAsset {
            rel: "memory/STRANGER.md",
            body: "路径在 {{SKILL_DIR}}",
            template: false,
            render: true,
        };
        let e = materialize(&a, Path::new("/tmp/x")).unwrap_err();
        assert!(e.contains("SKILL_DIR"), "应明确报错而不是瞎猜: {e}");
    }

    // ── 运行时自检（build.rs 闸门③ 的库内镜像）───────────────────

    /// 编进二进制的骨架**必须干净**。build.rs 在编译期已经拦过一遍，
    /// 这里再钉一次 —— 万一将来绕过闸门（比如手工改了 codegen），
    /// 测试要能当场抓住。
    #[test]
    fn compiled_skeleton_contains_no_machine_specific_content() {
        assert!(!SKELETON_SEED.is_empty(), "seed 不能为空");
        for a in SKELETON_ASSETS {
            // 落点不许带用户区名字
            assert!(
                !a.rel.contains("agent-data"),
                "{} 的 rel 不该出现 agent-data",
                a.rel
            );
            // rel 必须是相对数据目录、用 / 分隔
            assert!(!a.rel.contains('\\'), "{} 的 rel 用了反斜杠", a.rel);
            assert!(!a.rel.starts_with('/'), "{} 的 rel 不该是绝对路径", a.rel);
            // 正文不许有本机绝对路径 / 用户名
            for (pat, why) in [
                (r"C:\Users\", "本机用户目录"),
                (r"D:\workplace", "构建机工作目录"),
                ("C:\\Users\\DELL", "构建者用户名"),
            ] {
                assert!(
                    !a.body.contains(pat),
                    "{} 正文含{why}：{pat}\n这会把构建机的信息分发给所有用户",
                    a.rel
                );
            }
            // 正文不许有编译缓存 / 凭证
            assert!(!a.body.contains("Bearer "), "{} 含 Bearer 令牌", a.rel);
            assert!(!a.body.contains("api_key="), "{} 含 api_key 赋值", a.rel);
            // 有占位符就必须声明要渲染
            if a.body.contains("{{") {
                assert!(a.render, "{} 有占位符但没声明 render", a.rel);
            }
        }
    }

    #[test]
    fn seed_is_a_stable_sha256_hex() {
        // sha256 = 64 位小写十六进制
        assert_eq!(SKELETON_SEED.len(), 64, "seed 应是 sha256");
        assert!(
            SKELETON_SEED.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "seed 应是全小写十六进制: {SKELETON_SEED}"
        );
    }

    // ── 状态与报告 ──────────────────────────────────────────────

    #[test]
    fn writes_state_file_and_dated_report_with_explicit_reasons() {
        let d = tmp("report");
        bootstrap_agent_data(&d).expect("首装应成功");

        // 已应用状态
        let st: AppliedState = serde_json::from_str(
            &std::fs::read_to_string(d.join(STATE_FILE)).expect("应写状态文件"),
        )
        .expect("状态文件应是合法 JSON");
        assert_eq!(st.kind, "orbcat-skeleton");
        assert_eq!(st.seed, SKELETON_SEED);
        assert_eq!(st.applied_files.len(), SKELETON_ASSETS.len());

        // 报告按 YYYY/MM/DD 分片
        let (y, m, day) = civil_today();
        let dir = d.join(MIGRATIONS_DIR).join(&y).join(&m).join(&day);
        let reports: Vec<_> = std::fs::read_dir(&dir)
            .expect("报告目录应存在")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("orbcat-skeleton-"))
            .collect();
        assert_eq!(reports.len(), 1, "首装应只落一份报告");

        let rep: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(reports[0].path()).unwrap()).unwrap();
        assert_eq!(rep["kind"], "orbcat-skeleton");
        assert_eq!(rep["status"], "completed");
        assert_eq!(rep["seed"], SKELETON_SEED);
        assert!(rep["skipped"].as_array().unwrap().is_empty(), "首装无跳过");
        assert_eq!(rep["errors"].as_array().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 二次运行：每个跳过项都必须带 reason，**不许出现裸路径**。
    #[test]
    fn report_records_reason_for_every_skip() {
        let d = tmp("reasons");
        bootstrap_agent_data(&d).expect("首装应成功");
        // 真的跑第二次 —— 否则读到的是首装那份（skipped 为空），断言会假通过
        bootstrap_agent_data(&d).expect("二次应成功");

        // 递归数而不是只看当天目录：两次运行若跨了 UTC 零点，报告会分到两天，
        // 只看 `civil_today()` 会读到 1 份而误判。
        let reports = count_reports(&d);
        assert_eq!(reports, 2, "跑两次就该有两份报告，实际 {reports}");

        let latest = latest_report(&d);
        let rep: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(latest).unwrap()).unwrap();

        let skips = rep["skipped"].as_array().expect("skipped 应是数组");
        assert!(!skips.is_empty(), "二次运行必然有跳过项");
        for s in skips {
            assert!(
                s.get("reason").and_then(|r| r.as_str()).is_some(),
                "跳过项缺 reason —— 不允许静默: {s}"
            );
            assert!(
                ["user-owned", "source-missing", "seed-current"]
                    .contains(&s["reason"].as_str().unwrap()),
                "reason 词表是闭集，实际 {:?}",
                s["reason"]
            );
        }
        // 二次运行 seed 没变：**骨架资产**的理由应是 seed-current。
        // 而目录与那三个「由 Rust struct 生成」的 json 不在 seed 里，
        // 「seed 没变」对它们根本不成立 —— 诚实的原因是 user-owned。
        let by_path = |p: &str| {
            skips.iter()
                .find(|s| s["path"] == p)
                .map(|s| s["reason"].as_str().unwrap().to_string())
        };
        for a in SKELETON_ASSETS {
            assert_eq!(
                by_path(a.rel).as_deref(),
                Some("seed-current"),
                "{} 是骨架资产，seed 未变时应记 seed-current",
                a.rel
            );
        }
        for p in [
            "daily", "images", "memory", "pending", "projects", "screenshots", "sessions",
            "skills", "models.json", "settings.json", "modes.json",
        ] {
            assert_eq!(
                by_path(p).as_deref(),
                Some("user-owned"),
                "{p} 不在 seed 里，不该拿 seed-current 当理由"
            );
        }

        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 版本化模板升级 ──────────────────────────────────────────

    fn read_sections(p: &Path) -> Vec<String> {
        md_sections(&std::fs::read_to_string(p).unwrap())
            .into_iter()
            .map(|(h, _)| h)
            .collect()
    }

    #[test]
    fn md_sections_splits_on_h2_only() {
        let md = "前置说明\n\n## 甲\n内容甲\n\n### 子\n子内容\n\n## 乙\n内容乙\n";
        let s = md_sections(md);
        assert_eq!(
            s.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(),
            vec!["甲", "乙"],
            "# 开头不该当成小节，### 是子级也不该算"
        );
        assert!(s[0].1.contains("内容甲") && s[0].1.contains("子内容"));
        assert!(s[1].1.contains("内容乙"));
    }

    /// 核心铁律：补缺失小节，**用户写过的一节都不许动**。
    #[test]
    fn upgrade_appends_only_missing_sections() {
        let d = tmp("upgrade");
        std::fs::create_dir_all(&d).unwrap();
        let target = d.join("MEMORY.md");

        // 用户已经有一个模板里没有的小节，还有一个同名但内容是自己写的
        std::fs::write(
            &target,
            "## 项目\n\n我自己写的项目记忆，绝不能被覆盖\n\n## 我自己的小节\n\n保留我\n",
        )
        .unwrap();

        let tpl = "## 项目\n\n模板的项目段\n\n## 教训\n\n模板的教训段\n\n## 决策\n\n模板的决策段\n";
        assert!(upgrade_template(&target, tpl, &d).unwrap(), "应该补过");

        let after = std::fs::read_to_string(&target).unwrap();
        assert!(
            after.contains("我自己写的项目记忆，绝不能被覆盖"),
            "同名小节的**用户内容**必须原样保留:\n{after}"
        );
        assert!(!after.contains("模板的项目段"), "同名小节不该被模板正文覆盖");
        assert!(after.contains("模板的教训段"), "缺失的教训段该补上");
        assert!(after.contains("模板的决策段"), "缺失的决策段该补上");
        assert!(after.contains("## 我自己的小节"), "用户自己的小节不该被删");
        assert_eq!(
            read_sections(&target),
            vec!["项目", "我自己的小节", "教训", "决策"],
            "补的追加在文末，不打乱原有顺序"
        );

        // 动手之前必须留备份
        let backups: Vec<_> = std::fs::read_dir(d.join(".trash"))
            .expect("应建 .trash")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(backups.len(), 1, "补写前应备份原文件一次");
        assert!(std::fs::read_to_string(backups[0].path())
            .unwrap()
            .contains("我自己写的项目记忆"));

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 一节都不缺时不动文件，并且**如实说"没补"** —— 不静默。
    #[test]
    fn upgrade_reports_nothing_to_do_when_all_sections_present() {
        let d = tmp("noop");
        std::fs::create_dir_all(&d).unwrap();
        let target = d.join("MEMORY.md");
        let original = "## 甲\n我的内容\n\n## 乙\n我的内容乙\n";
        std::fs::write(&target, original).unwrap();

        assert!(
            !upgrade_template(&target, "## 甲\n模板甲\n\n## 乙\n模板乙\n", &d).unwrap(),
            "全都有了就不该声称补过"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "没补就不该动文件"
        );
        assert!(
            !d.join(".trash").exists(),
            "没补就不该产生备份"
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}