//! orbcat — Tauri 后端入口

mod agent;
/// 测试钩子 / 集成用例（不进产品路径）
mod test_hooks;
mod bootstrap;
mod capture;
mod command_policy;
mod config;
mod context;
mod fetch;
mod history;
mod images;
mod llm;
mod mcp;
mod memory;
mod perm_request;
mod permission;
mod pmem;
mod sessions;
mod shell;
mod skills;
mod tools;
mod search;

#[cfg(windows)]
mod win32;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use permission::{Access, GrantTier, PermissionGate};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, State};

// ---------------------------------------------------------------------------
// 窗口布局常量（逻辑像素）
// ---------------------------------------------------------------------------

/// 胶囊态边长
const ORB_LOGICAL: f64 = 56.0;
/// 面板态尺寸
const PANEL_W_LOGICAL: f64 = 420.0;
const PANEL_H_LOGICAL: f64 = 600.0;
/// 右键菜单态尺寸（只够放一个小菜单卡片）
const MENU_W_LOGICAL: f64 = 196.0;
/// 右键菜单高度 —— 设置 / 记忆 / 对话 / 退出（4 项）
const MENU_H_LOGICAL: f64 = 172.0;
/// 距屏幕边缘留白
const MARGIN_LOGICAL: f64 = 24.0;
/// 胶囊在屏幕高度的这个比例处
const ORB_Y_RATIO: f64 = 0.40;

/// 胶囊态窗口相对球体的额外留白（逻辑像素，每边）。
///
/// ⚠️ 作用：球外面还有三个「出球」的视觉效果 ——
///   `.orb::after` 呼吸光环（`inset: -6px`，即球外 6px）
///   `.orb:hover` 的 4px 高亮环
///   呼吸动画 `transform: scale(1.035)`（球本体会放大）
/// 窗口若紧贴球（= 球径 56），这些效果**全部伸出窗口矩形被 DWM 硬裁**，
/// 表现为球四周出现一圈直角方框 —— 用户报的「外面的黑框」就是这么来的。
/// 给窗口每边留 10px，三个效果（最多 ~6.7px）都落在窗内，硬裁消失。
const ORB_PAD_LOGICAL: f64 = 10.0;

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

struct AppState {
    /// 文件权限网关 —— **所有**文件操作都必须先过它
    gate: Mutex<PermissionGate>,
    /// 上次加载 permissions.json 时的 mtime（热加载检测用）
    perm_mtime: Mutex<Option<std::time::SystemTime>>,
    /// agent 数据目录（记忆文件族 + 备份 + 设置）
    data_dir: PathBuf,
    /// 记忆引擎（日记 / 候选审批 / 长期记忆）
    memory: memory::MemoryStore,
    /// MCP 工具注册表（懒加载 + 预算守卫，见 mcp.rs）。
    /// 用 Arc 是为了能在 setup 里 spawn 一个后台任务去拉工具清单。
    mcp: std::sync::Arc<tokio::sync::Mutex<mcp::ToolRegistry>>,
    /// 「申请权限」的授权表 + 待办通道。
    /// 用 Arc 是因为工具层要以 `&PermHub` 借它（见 `tools::ToolCtx`），
    /// 而 Tauri 的 `State` 只能给出借用，不能把所有权传进去。
    perm: std::sync::Arc<perm_request::PermHub>,
    /// 执行中「插话」的收件箱 + 取消标志，**按会话分槽**（2026-09-26 用户拍板：
    /// 多会话并行 run）。`chat` 开跑前 `begin(session_id)` 拿槽、收尾 `end()` 归还；
    /// `chat_steer` / `chat_cancel` 都带 `session_id` 路由到对应槽。
    runs: agent::RunRegistry,
}

/// 换会话时重装临时授权（**仅文件授权**）。
///
/// **必须整体替换，不能只清 `Task` 档**：`Once`/`Turn` 只活在内存里，
/// 而"换会话"本身就意味着上下文全变了 —— 它们跨过去毫无意义。
/// （`Once` 是"下一次工具调用"，换个会话还算数就很荒唐。）
///
/// ⚠️ **命令授权不走这里**（2026-09-22 用户拍板）：它是**永久**的，
/// 落 `command_grants.json`，跨会话、跨重启都保留 —— 换会话顺手清掉就白永久了。
fn reload_grants_for_session(state: &AppState, id: &str) {
    let loaded = sessions::load_grants(&state.data_dir, id);
    match state.perm.grants().lock() {
        Ok(mut store) => store.replace(loaded),
        Err(e) => eprintln!("[orbcat] ⚠️ 切换会话时重装授权失败: {e}"),
    }
    // 上个会话的"最近一次拒绝"不能继续用来解释本会话的申请；
    // 「防骚扰」的被拒记录同理 —— 上个任务拒过的东西，不该管到这个任务。
    state.perm.clear_denial();
    state.perm.clear_denied();
}

impl AppState {
    /// 权限热加载：permissions.json 被外部改过（手工编辑 / 别的入口删了规则）
    /// 就重新读进来。没有这一步的话「删了规则 UI 还显示旧值」，用户会觉得删不掉。
    fn sync_perm_file(&self) {
        let path = permission::rules_path(&self.data_dir);
        let Ok(md) = std::fs::metadata(&path) else { return };
        let Ok(mtime) = md.modified() else { return };

        let changed = match *self.perm_mtime.lock().unwrap() {
            Some(t) => t != mtime,
            None => true,
        };
        if !changed {
            return;
        }

        let Ok(txt) = std::fs::read_to_string(&path) else { return };
        #[derive(serde::Deserialize)]
        struct RulesFile {
            #[serde(default)]
            rules: Vec<permission::Rule>,
        }
        let Ok(f) = serde_json::from_str::<RulesFile>(&txt) else {
            return; // 解析失败不动内存里的规则（下次有效写入再同步）
        };
        let mut gate = self.gate.lock().unwrap();
        gate.replace_rules(&f.rules);
        drop(gate);
        *self.perm_mtime.lock().unwrap() = Some(mtime);
        eprintln!("[orbcat] 权限规则已从文件热加载（{} 条）", f.rules.len());
    }
}

// ---------------------------------------------------------------------------
// 数据目录解析
// ---------------------------------------------------------------------------

/// 解析 agent-data 目录。
///
/// 优先级：
///   1. 环境变量 `ORBCAT_DATA_DIR`
///   2. 源码树里的 `<项目根>/agent-data`（开发期）
///   3. 系统应用数据目录（发布版）
///
/// 注：`agent-data/` 是**运行时数据**（API key、会话、截图、日记），
/// 已在 `.gitignore` 里排除 —— 仓库里只保留 `初始化.md`，
/// 新克隆的用户让 agent 读它即可自建整套结构。
fn resolve_data_dir(app: &tauri::AppHandle) -> PathBuf {
    if let Ok(p) = std::env::var("ORBCAT_DATA_DIR") {
        let p = PathBuf::from(p);
        if p.exists() {
            return p;
        }
    }

    // 开发期：CARGO_MANIFEST_DIR = <项目根>/orbcat/src-tauri
    // 往上**一级**即 orbcat/，故 agent-data 与 src-tauri 平级。
    // ⚠️ 这是编译期常量 —— 改动后必须重新构建，否则跑的还是旧路径。
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-data");
    if dev.exists() {
        return dev;
    }

    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("./agent-data"))
}

// ---------------------------------------------------------------------------
// 窗口几何计算
// ---------------------------------------------------------------------------

/// 计算目标窗口 bounds，返回**物理像素** `(x, y, w, h)`。
#[cfg(windows)]
fn compute_bounds(
    mode: &str,
    monitor_pos: (i32, i32),
    monitor_size: (u32, u32),
    scale: f64,
) -> (i32, i32, i32, i32) {
    let (mx, my) = monitor_pos;
    let (mw, mh) = (monitor_size.0 as i32, monitor_size.1 as i32);

    let margin = (MARGIN_LOGICAL * scale).round() as i32;
    let orb_px = (ORB_LOGICAL * scale).round() as i32;

    let orb_x = mx + mw - orb_px - margin;
    let orb_y = my + (mh as f64 * ORB_Y_RATIO).round() as i32;

    match mode {
        "panel" => {
            let pw = (PANEL_W_LOGICAL * scale).round() as i32;
            let ph = (PANEL_H_LOGICAL * scale).round() as i32;
            let px = mx + mw - pw - margin;

            let mut py = orb_y - ph / 3;
            if py + ph > my + mh - margin {
                py = my + mh - ph - margin;
            }
            if py < my + margin {
                py = my + margin;
            }
            (px, py, pw, ph)
        }
        // 菜单：小卡片，右边缘与胶囊右边缘对齐（球在屏幕右侧，向左展开才不会出界）
        "menu" => {
            let mw_menu = (MENU_W_LOGICAL * scale).round() as i32;
            let mh_menu = (MENU_H_LOGICAL * scale).round() as i32;
            let mx_menu = orb_x + orb_px - mw_menu;
            let my_menu = orb_y;
            (mx_menu, my_menu, mw_menu, mh_menu)
        }
        // 胶囊态：窗口比球**大一圈**（每边 ORB_PAD_LOGICAL）。
        //   orb_x / orb_y 仍按球径算 —— 球的右边缘与纵向位置完全不变，
        //   只是窗口往左上各扩 pad，让球外光环/悬停环/呼吸放大有落脚处（见常量注释）。
        //   球自身仍是 ORB_LOGICAL，在 #app 里 flex 居中，不受窗口变大影响。
        _ => {
            let pad = (ORB_PAD_LOGICAL * scale).round() as i32;
            (
                orb_x - pad,
                orb_y - pad,
                orb_px + pad * 2,
                orb_px + pad * 2,
            )
        }
    }
}

/// 应用某个形态：设置 bounds + 切换窗口扩展样式。
#[cfg(windows)]
fn apply_mode(win: &tauri::WebviewWindow, mode: &str) -> Result<(), String> {
    // ---- 诊断开关 ----
    // 设了 ORBCAT_NO_WINDOW_MODE 就跳过所有窗口形态调整，
    // 用来把它与"渲染问题"隔离开做二分排查。
    if std::env::var("ORBCAT_NO_WINDOW_MODE").is_ok() {
        eprintln!("[orbcat] apply_mode 已被诊断开关跳过: {mode}");
        return Ok(());
    }

    let scale = win
        .scale_factor()
        .map_err(|e| format!("获取缩放比失败: {e}"))?;

    let hwnd = win.hwnd().map_err(|e| format!("获取窗口句柄失败: {e}"))?;
    let ptr = hwnd.0 as *mut c_void;

    let (mx, my, mw, mh) = match win.current_monitor() {
        Ok(Some(m)) => (m.position().x, m.position().y, m.size().width, m.size().height),
        _ => (0, 0, 1920, 1080),
    };

    let (x, y, w, h) = compute_bounds(mode, (mx, my), (mw, mh), scale);

    // 关键：用 SetWindowPos 而非 Tauri 的 set_size —— 见 win32::set_bounds 注释
    win32::set_bounds(ptr, x, y, w, h);

    if mode == "orb" {
        win32::set_no_activate(ptr);
    } else {
        win32::clear_no_activate(ptr);
        // 先走 Tauri 的正常路径，再用 `force_foreground` 兜底绕前台锁。
        // ⚠️ 只加在**面板态**：球态/菜单态抢焦点没意义，面板态不抢则输入框永远
        //    拿不到 OS 级焦点 —— 自动化工具（windows-mcp 等）打的字会全落到别的
        //    应用里。详见 `win32::force_foreground` 的注释。
        let _ = win.set_focus();
        if mode == "panel" {
            win32::force_foreground(ptr);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 权限网关
// ---------------------------------------------------------------------------

fn parse_access(s: &str) -> Result<Access, String> {
    Access::parse(s).ok_or_else(|| {
        format!("未知权限级别「{s}」，可用值：deny / read / readwrite / full")
    })
}

/// 保存规则后刷新 mtime 戳，避免下一次 sync 又把刚写的文件重读一遍
fn stamp_perm_mtime(state: &AppState) {
    let path = permission::rules_path(&state.data_dir);
    if let Ok(t) = std::fs::metadata(&path).and_then(|m| m.modified()) {
        if let Ok(mut g) = state.perm_mtime.lock() {
            *g = Some(t);
        }
    }
}

#[allow(dead_code)]
fn access_name(a: Access) -> &'static str {
    a.as_str()
}

#[tauri::command]
fn perm_check(state: State<'_, AppState>, path: String, need: String) -> Result<String, String> {
    state.sync_perm_file();
    let access = parse_access(&need)?;
    let gate = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
    gate.check(&path, access)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

/// 当前全部规则（`Rule` 本身可序列化，直接返回，前端拿到的是
/// `{prefix, access, label}`，access 为 `deny|read|readwrite|full`）
#[tauri::command]
fn perm_rules(state: State<'_, AppState>) -> Result<Vec<permission::Rule>, String> {
    // 关键：先热加载。早先这里直接返回内存值，用户在文件里删了规则、
    // 或从别的入口改了 permissions.json，UI 一直显示旧的 —— 看起来就是「删不掉」。
    state.sync_perm_file();
    let gate = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
    Ok(gate.export_rules())
}

#[tauri::command]
fn perm_add_rule(
    state: State<'_, AppState>,
    prefix: String,
    access: String,
    label: String,
) -> Result<(), String> {
    state.sync_perm_file();
    let a = parse_access(&access)?;
    let mut gate = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
    gate.add_rule(&prefix, a, label);
    permission::save_gate(&state.data_dir, &gate)?;
    drop(gate);
    stamp_perm_mtime(&state);
    Ok(())
}

#[tauri::command]
fn perm_revoke_label(state: State<'_, AppState>, label: String) -> Result<usize, String> {
    state.sync_perm_file();
    let mut gate = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
    let n = gate.remove_rules_by_label(&label);
    if n > 0 {
        permission::save_gate(&state.data_dir, &gate)?;
        drop(gate);
        stamp_perm_mtime(&state);
    }
    Ok(n)
}

/// 整体替换全部规则（设置界面用）。
///
/// **同时落盘** —— 早先规则只存在内存里，改了重启就丢，那是设计错误。
#[tauri::command]
fn perm_set_rules(state: State<'_, AppState>, rules: Vec<permission::Rule>) -> Result<(), String> {
    let mut gate = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
    gate.replace_rules(&rules);
    let path = permission::save_gate(&state.data_dir, &gate)?;
    drop(gate);
    stamp_perm_mtime(&state);
    eprintln!(
        "[orbcat] 权限规则已更新并保存（{} 条 → {}）",
        rules.len(),
        path.display()
    );
    Ok(())
}

/// 删除一条规则（按前缀）
#[tauri::command]
fn perm_remove_rule(state: State<'_, AppState>, prefix: String) -> Result<(), String> {
    state.sync_perm_file();
    let mut gate = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
    if !gate.remove_rule(&prefix) {
        return Err(format!("找不到规则：{prefix}"));
    }
    permission::save_gate(&state.data_dir, &gate)?;
    drop(gate);
    stamp_perm_mtime(&state);
    Ok(())
}

/// 把规则文件路径告诉前端（方便用户手工编辑）
#[tauri::command]
fn perm_rules_path(state: State<'_, AppState>) -> String {
    permission::rules_path(&state.data_dir)
        .display()
        .to_string()
}

// --- 命令策略（决策 9.1）---

/// 当前命令策略（设置页展示：白名单 / 硬阻断）
#[tauri::command]
fn perm_cmd_policy(state: State<'_, AppState>) -> command_policy::CommandPolicy {
    command_policy::load_policy(&state.data_dir)
}

/// 命令策略文件路径（方便用户手工编辑）
#[tauri::command]
fn perm_cmd_policy_path(state: State<'_, AppState>) -> String {
    command_policy::policy_path(&state.data_dir)
        .display()
        .to_string()
}

/// 当前生效的命令授权（「记住这类/记住这条」是**永久**档，跨重启存活；设置页展示）
#[tauri::command]
fn perm_cmd_grants_list(state: State<'_, AppState>) -> Result<Vec<command_policy::CmdGrant>, String> {
    let store = state
        .perm
        .cmd_grants()
        .lock()
        .map_err(|e| format!("锁失败: {e}"))?;
    Ok(store.grants().to_vec())
}

/// 撤销一条永久命令授权（设置页「撤销」）。返回撤销条数，并同步落盘。
#[tauri::command]
fn perm_cmd_grant_revoke(state: State<'_, AppState>, fingerprint: String) -> Result<usize, String> {
    let fp = fingerprint.trim();
    if fp.is_empty() {
        return Err("fingerprint 不能为空".into());
    }
    Ok(state.perm.revoke_cmd_grant(fp))
}

/// 清空全部永久命令授权（设置页「全部撤销」）。返回清掉的条数，并同步落盘。
#[tauri::command]
fn perm_cmd_grants_clear(state: State<'_, AppState>) -> Result<usize, String> {
    Ok(state.perm.clear_cmd_all())
}

/// MCP「总是允许」名单
#[tauri::command]
fn mcp_grants_list(state: State<'_, AppState>) -> Vec<String> {
    mcp::load_mcp_grants(&state.data_dir)
}

#[tauri::command]
fn mcp_grant_revoke(state: State<'_, AppState>, tool: String) -> Result<usize, String> {
    mcp::revoke_mcp_tool(&state.data_dir, &tool)
}

#[tauri::command]
fn mcp_grants_clear(state: State<'_, AppState>) -> Result<usize, String> {
    mcp::clear_mcp_grants(&state.data_dir)
}

/// 覆盖保存命令策略（设置页编辑白名单 / 硬阻断列表）。
/// 入参去空白、去空行、去重后整体写入 `command_policy.json`。
#[tauri::command]
fn perm_cmd_policy_set(
    state: State<'_, AppState>,
    allow: Vec<String>,
    hard_block: Vec<String>,
) -> Result<command_policy::CommandPolicy, String> {
    let clean = |xs: Vec<String>| -> Vec<String> {
        let mut v: Vec<String> = xs
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let policy = command_policy::CommandPolicy {
        allow: clean(allow),
        hard_block: clean(hard_block),
    };
    command_policy::save_policy(&state.data_dir, &policy)?;
    Ok(policy)
}

/// 设置页「试一下」：对一条命令做策略判定（不执行、不申请）。
#[derive(serde::Serialize)]
struct CmdVerdictView {
    /// allow | ask | hard-block
    verdict: String,
    reason: String,
    risk: String,
    normalized: String,
}

#[tauri::command]
fn perm_cmd_test(state: State<'_, AppState>, command: String) -> Result<CmdVerdictView, String> {
    let policy = command_policy::load_policy(&state.data_dir);
    let norm = command_policy::normalize(&command);
    let (verdict, reason, risk) = match command_policy::evaluate(&policy, &command) {
        command_policy::CmdVerdict::HardBlock { reason } => {
            ("hard-block", reason, "high".to_string())
        }
        command_policy::CmdVerdict::Allow { reason } => ("allow", reason, "low".to_string()),
        command_policy::CmdVerdict::Ask { reason, risk } => {
            ("ask", reason, risk.as_str().to_string())
        }
    };
    Ok(CmdVerdictView {
        verdict: verdict.to_string(),
        reason,
        risk,
        normalized: norm.full(),
    })
}

/// 审计尾部（默认 80 条，最多 500）。文件不存在 = 空列表。
#[tauri::command]
fn perm_cmd_audit_tail(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<command_policy::AuditRecord>, String> {
    let path = command_policy::audit_path(&state.data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let txt = std::fs::read_to_string(&path).map_err(|e| format!("读审计失败: {e}"))?;
    let mut out: Vec<command_policy::AuditRecord> = Vec::new();
    for line in txt.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<command_policy::AuditRecord>(line) {
            out.push(r);
        }
    }
    let lim = limit.unwrap_or(80).clamp(1, 500);
    let start = out.len().saturating_sub(lim);
    Ok(out.split_off(start))
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 模型
// ---------------------------------------------------------------------------

/// 列出可用模型（**不含 apiKey**）
#[tauri::command]
fn list_models(state: State<'_, AppState>) -> Result<Vec<config::ModelView>, String> {
    let models = config::load_models(&state.data_dir)?;
    Ok(models.iter().map(config::ModelView::from).collect())
}

/// 当前选中的模型 id
#[tauri::command]
fn get_selected_model(state: State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(config::load_settings(&state.data_dir).selected_model)
}

#[tauri::command]
fn set_selected_model(state: State<'_, AppState>, id: String) -> Result<(), String> {
    // 先确认这个 id 真的存在
    let _ = config::find_model(&state.data_dir, &id)?;
    let mut s = config::load_settings(&state.data_dir);
    s.selected_model = Some(id);
    config::save_settings(&state.data_dir, &s)
}

/// 连通性自检：确认 key / url / 模型名都对
#[tauri::command]
async fn test_model(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let cfg = config::find_model(&state.data_dir, &id)?;
    eprintln!(
        "[orbcat] 测试模型 {} @ {}（key {}）",
        cfg.id,
        cfg.url,
        cfg.redacted_key()
    );
    llm::ping(&cfg).await
}

/// 解析「Key: Value」逐行的额外请求头文本
fn parse_headers(text: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut map = std::collections::BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(format!("第 {} 行不是 `Key: Value` 格式：{line}", i + 1));
        };
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() {
            return Err(format!("第 {} 行的头名字为空", i + 1));
        }
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

/// 新增（或按 **显示名** 覆盖）一个模型。
///
/// - **显示名唯一**；**接口 model（`id`）允许重复**（官方 / 火山可同时调 `deepseek-v4.1-flash`）
/// - 同显示名 = 更新这一条；不再因为接口 id 相同而拦截
/// - Key 可空（Ollama / 本地）
#[tauri::command]
fn models_add(
    state: State<'_, AppState>,
    id: String,
    name: String,
    url: String,
    api_key: Option<String>,
    supports_tool_call: bool,
    supports_images: bool,
    headers: Option<String>,
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    overwrite: Option<bool>,
) -> Result<String, String> {
    let _ = overwrite; // 同显示名即更新，不再需要「覆盖」确认
    let id = id.trim().to_string();
    let display = if name.trim().is_empty() {
        id.clone()
    } else {
        name.trim().to_string()
    };
    if display.is_empty() {
        return Err("显示名不能为空".into());
    }
    if id.is_empty() {
        return Err("接口模型 ID 不能为空".into());
    }
    let url = config::normalize_base_url(&url)?;

    // 按显示名找旧配置（保留 vendor / max / key 兜底）
    let existing = config::find_model(&state.data_dir, &display).ok();

    let key_in = api_key.unwrap_or_default().trim().to_string();
    let api_key = if key_in.is_empty() {
        existing
            .as_ref()
            .map(|m| m.api_key.clone())
            .unwrap_or_default()
    } else {
        key_in
    };

    let vendor = existing_vendor_or_custom(&existing);
    let max_in = max_input_tokens.or_else(|| existing.as_ref().and_then(|m| m.max_input_tokens));
    let max_out =
        max_output_tokens.or_else(|| existing.as_ref().and_then(|m| m.max_output_tokens));

    let extra_headers = match headers {
        Some(t) => parse_headers(&t)?,
        None => existing.as_ref().map(|m| m.headers.clone()).unwrap_or_default(),
    };

    let m = config::ModelConfig {
        id,
        name: display.clone(),
        vendor,
        url,
        api_key,
        supports_tool_call,
        supports_images,
        max_input_tokens: max_in,
        max_output_tokens: max_out,
        headers: extra_headers,
    };

    config::add_model(&state.data_dir, m)?;
    eprintln!("[orbcat] 已保存模型（显示名={display}）");
    Ok(display)
}

fn existing_vendor_or_custom(existing: &Option<config::ModelConfig>) -> String {
    match existing {
        Some(m) if !m.vendor.trim().is_empty() => m.vendor.trim().to_string(),
        _ => "Custom".into(),
    }
}

/// 编辑已有模型（按 **显示名** 定位）。
///
/// - `name`：显示名（唯一键）
/// - `new_id`：发给提供商的 `model` 字段，**允许与别的条目相同**
/// - `apiKey` 空 = 保持原 Key；`clear_key` = 清空
#[tauri::command]
fn models_edit(
    state: State<'_, AppState>,
    id: String,
    new_id: Option<String>,
    name: Option<String>,
    url: Option<String>,
    api_key: Option<String>,
    clear_key: Option<bool>,
    supports_tool_call: Option<bool>,
    supports_images: Option<bool>,
    headers: Option<String>,
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
) -> Result<(), String> {
    let key = id.trim().to_string();
    let extra_headers = match headers {
        Some(t) => Some(parse_headers(&t)?),
        None => None,
    };

    // 显示名变更（唯一）；接口 id 单独改、不查重
    let new_name = name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let rename_to = new_name.filter(|n| n != &key);

    if let Some(nn) = &rename_to {
        config::rename_model(&state.data_dir, &key, nn)?;
    }

    let target = rename_to.clone().unwrap_or_else(|| key.clone());
    config::update_model(
        &state.data_dir,
        &target,
        None,
        url,
        api_key,
        clear_key.unwrap_or(false),
        supports_tool_call,
        supports_images,
        extra_headers,
        Some(max_input_tokens),
        Some(max_output_tokens),
        new_id.filter(|s| !s.trim().is_empty()),
    )?;

    if let Some(nn) = &rename_to {
        let mut s = config::load_settings(&state.data_dir);
        if s.selected_model.as_deref() == Some(key.as_str()) {
            s.selected_model = Some(nn.clone());
            config::save_settings(&state.data_dir, &s)?;
        }
    }

    eprintln!(
        "[orbcat] 已编辑模型（显示名 {}{}）",
        key,
        rename_to
            .as_ref()
            .map(|n| format!(" → {n}"))
            .unwrap_or_default()
    );
    Ok(())
}

/// 编辑表单回填（**不含完整 Key**，只有掩码预览）
#[tauri::command]
fn models_get_edit(state: State<'_, AppState>, id: String) -> Result<config::ModelEditView, String> {
    let m = config::find_model(&state.data_dir, id.trim())?;
    Ok(config::ModelEditView::from(&m))
}

/// 手动填 URL + Key，拉取 OpenAI 兼容 `GET {url}/models`。
/// 返回 id 列表（不含任何密钥）。
#[tauri::command]
async fn models_fetch_remote(
    url: String,
    api_key: String,
    headers: Option<String>,
) -> Result<Vec<llm::RemoteModelInfo>, String> {
    let url = url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("URL 必须以 http:// 或 https:// 开头".into());
    }
    let hs = match headers {
        Some(t) if !t.trim().is_empty() => {
            let map = parse_headers(&t)?;
            map.into_iter().collect::<Vec<_>>()
        }
        _ => Vec::new(),
    };
    // Key 可空：Ollama / 本地网关不需要鉴权（预设下 Key 输入框必须可写，前端负责）
    eprintln!("[orbcat] 拉取模型列表 @ {}", config::models_endpoint_from_base(&url));
    llm::fetch_models_list(&url, &api_key, &hs).await
}

/// 用**已配置模型**的 url/key/headers 拉取列表（设置页省得再手填 Key）。
#[tauri::command]
async fn models_fetch_remote_using(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<llm::RemoteModelInfo>, String> {
    let cfg = config::find_model(&state.data_dir, id.trim())?;
    eprintln!(
        "[orbcat] 用已配置模型 {} 拉取列表（key {}）",
        cfg.id,
        cfg.redacted_key()
    );
    llm::fetch_models_from_cfg(&cfg).await
}

/// 批量导入远端列表里的 id。
///
/// - `source_id` 有值 → 复用该模型的 url/key/headers（推荐）
/// - 否则用传入的 url / api_key / headers（手动拉取那条路径）
///
/// 已存在的 id **跳过不覆盖**。返回 `{added, skipped}`。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportModelsResult {
    added: Vec<String>,
    skipped: Vec<String>,
}

#[tauri::command]
fn models_import_remote(
    state: State<'_, AppState>,
    ids: Vec<String>,
    source_id: Option<String>,
    url: Option<String>,
    api_key: Option<String>,
    headers: Option<String>,
    supports_tool_call: bool,
    supports_images: bool,
    // 模型 id → 上下文窗口（来自 `/models` 返回的可选字段）。
    // 前端从拉取结果里带上；网关不提供时为空。
    context_lengths: Option<std::collections::HashMap<String, u64>>,
) -> Result<ImportModelsResult, String> {
    let source = if let Some(sid) = source_id.filter(|s| !s.trim().is_empty()) {
        config::find_model(&state.data_dir, sid.trim())?
    } else {
        let url = url.unwrap_or_default();
        let url = url.trim().trim_end_matches('/').to_string();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("URL 必须以 http:// 或 https:// 开头".into());
        }
        let key = api_key.unwrap_or_default().trim().to_string();
        if key.is_empty() {
            return Err("API key 不能为空".into());
        }
        let hs = match headers {
            Some(t) if !t.trim().is_empty() => parse_headers(&t)?,
            _ => Default::default(),
        };
        config::ModelConfig {
            id: "__import__".into(),
            name: String::new(),
            vendor: "Custom".into(),
            url,
            api_key: key,
            supports_tool_call,
            supports_images,
            max_input_tokens: None,
            max_output_tokens: None,
            headers: hs,
        }
    };

    let ctx = context_lengths.unwrap_or_default();
    let (added, skipped) = config::import_models_from_source(
        &state.data_dir,
        &source,
        &ids,
        supports_tool_call,
        supports_images,
        &ctx,
    )?;
    eprintln!(
        "[orbcat] 批量导入模型：新增 {}，跳过 {}（带上下文窗口 {} 个）",
        added.len(),
        skipped.len(),
        ctx.len()
    );
    Ok(ImportModelsResult { added, skipped })
}

/// 删除一个模型。若删的是当前选中项，顺带清除选中状态。
#[tauri::command]
fn models_remove(state: State<'_, AppState>, id: String) -> Result<(), String> {
    config::remove_model(&state.data_dir, &id)?;

    let mut s = config::load_settings(&state.data_dir);
    if s.selected_model.as_deref() == Some(id.as_str()) {
        s.selected_model = None;
        config::save_settings(&state.data_dir, &s)?;
    }
    Ok(())
}

/// 读长期记忆全文（memory/MEMORY.md，与 approve 写入路径、prompt 读取路径一致）
#[tauri::command]
fn mem_read(state: State<'_, AppState>) -> Result<String, String> {
    let p = state.memory.root().join("memory").join("MEMORY.md");
    if !p.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&p).map_err(|e| format!("读取失败: {e}"))
}

/// 一次读回全部记忆文件族 —— 记忆面板要用「四大记忆」的完整视图。
///
/// 返回 `{ rules, soul, identity, user, memory }`（缺文件为空串）。
#[derive(serde::Serialize)]
struct MemoryFiles {
    rules: String,
    soul: String,
    identity: String,
    user: String,
    memory: String,
}

#[tauri::command]
fn mem_files(state: State<'_, AppState>) -> MemoryFiles {
    let read = |name: &str| -> String {
        std::fs::read_to_string(state.memory.root().join(name)).unwrap_or_default()
    };
    MemoryFiles {
        rules: read("RULES.md"),
        soul: read("SOUL.md"),
        identity: read("IDENTITY.md"),
        user: read("USER.md"),
        memory: read("memory/MEMORY.md"),
    }
}

/// 项目记忆清单（设置 › 记忆 展示懒检测状态）
#[derive(serde::Serialize)]
struct ProjectMemoryView {
    name: String,
    source: String,
    source_alive: bool,
    has_memory: bool,
}

#[tauri::command]
fn mem_projects(state: State<'_, AppState>) -> Vec<ProjectMemoryView> {
    state
        .memory
        .list_projects()
        .into_iter()
        .map(|(name, source, source_alive, has_memory)| ProjectMemoryView {
            name,
            source,
            source_alive,
            has_memory,
        })
        .collect()
}

/// 通用设置读写（失焦收起等开关）
#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> config::AgentSettings {
    config::load_settings(&state.data_dir)
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 网页搜索（Tavily / Exa / Brave）
// ---------------------------------------------------------------------------

#[tauri::command]
fn search_status(state: State<'_, AppState>) -> search::SearchStatus {
    search::status(&state.data_dir)
}

/// 设置搜索后端。`provider`: ""|off|tavily|exa|brave；`api_key` Some 才覆盖。
#[tauri::command]
fn search_set(
    state: State<'_, AppState>,
    provider: String,
    api_key: Option<String>,
) -> Result<search::SearchStatus, String> {
    let r = search::set_config(&state.data_dir, &provider, api_key.as_deref())?;
    eprintln!(
        "[orbcat] 搜索配置: provider={} ready={} key={}",
        r.provider,
        r.ready,
        if r.has_key { r.key_preview.as_str() } else { "(无)" }
    );
    Ok(r)
}

/// 连通性自检（需要已配置）
#[tauri::command]
async fn search_test(state: State<'_, AppState>) -> Result<String, String> {
    let cfg = search::load_config(&state.data_dir)?;
    search::ping(&cfg).await
}

#[tauri::command]
fn set_blur_collapse(state: State<'_, AppState>, enable: bool) -> Result<(), String> {
    let mut s = config::load_settings(&state.data_dir);
    s.blur_collapse = enable;
    config::save_settings(&state.data_dir, &s)
}

/// 设置执行权限档位（`ask` / `smart` / `full`）。
/// 管的是 `run_command` / MCP 弹卡频率 —— 文件权限不受影响。
#[tauri::command]
fn set_exec_trust(state: State<'_, AppState>, mode: String) -> Result<String, String> {
    let t = config::ExecTrust::parse(&mode);
    let mut s = config::load_settings(&state.data_dir);
    s.exec_trust = t;
    config::save_settings(&state.data_dir, &s)?;
    eprintln!("[orbcat] 执行权限档位: {}", t.as_str());
    Ok(t.as_str().to_string())
}

/// 采集一次前台应用上下文（模型工具用；返回缓存的「非自身前台」）
#[tauri::command]
fn foreground_context() -> Option<context::ForegroundContext> {
    context::cached()
}

/// 最近的非自身前台应用（新的在前）—— 面板指示条展示多个用
#[tauri::command]
fn foreground_history() -> Vec<context::ForegroundContext> {
    let v = context::recent_foregrounds();
    if !v.is_empty() {
        return v;
    }
    // 缓存空（刚启动 / 一直被自身占前台）→ 现场采一次兜底
    context::current_excluding_self().into_iter().collect()
}

/// 图片缩略图（历史消息里的图渲染用）—— 返回 data URL。
/// 走命令而不是 asset protocol：免掉 tauri.conf 的 scope 配置与权限坑，
/// 缩到 320px 后单张几十 KB，IPC 压力可忽略。
#[tauri::command]
fn image_thumb(path: String, max_edge: Option<u32>) -> Result<String, String> {
    let r = images::thumb_data_url(&path, max_edge.unwrap_or(320));
    if let Err(e) = &r {
        // 留痕：会话里引用的图片文件可能已被移动/删除（换了数据目录就会有这种历史路径）。
        // 前端会把这类图渲染成"图"占位，**只请求一次**；如果这里刷屏，
        // 说明前端的失败缓存又退化成"每次都重试"了。
        eprintln!("[orbcat] 缩略图读不出来 {path}: {e}");
    }
    r
}

/// 技能清单（前端展示用）：name + description + 文件路径
#[tauri::command]
fn skills_list(state: State<'_, AppState>) -> Vec<serde_json::Value> {
    skills::discover(&state.data_dir)
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "path": s.path.to_string_lossy(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 应用生命周期 / 开机自启
// ---------------------------------------------------------------------------

/// 退出应用。
///
/// 为什么需要它：窗口设了 `skipTaskbar: true`（悬浮球不该占任务栏），
/// 结果就是**用户没有任何正常途径关掉它** —— 只能去任务管理器杀进程。
#[tauri::command]
fn app_quit(app: tauri::AppHandle) {
    eprintln!("[orbcat] 用户要求退出");
    app.exit(0);
}

#[cfg(windows)]
mod autostart {
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "orbcat";

    /// 当前可执行文件路径（包在引号里，防路径含空格）。
    ///
    /// ⚠️ 开机自启**优先指向 release exe**：debug 构建带控制台窗口
    /// （`windows_subsystem="windows"` 只在 release 生效），开机弹个黑框很难看。
    /// 若能从当前 exe 路径推导出同项目的 release exe（.../debug/xxx.exe →
    /// .../release/xxx.exe）且它存在，就用 release；否则退回当前 exe。
    fn exe_cmd() -> Result<String, String> {
        let exe = std::env::current_exe().map_err(|e| format!("取 exe 路径失败: {e}"))?;
        let release = release_sibling(&exe);
        let chosen = release.unwrap_or(exe);
        Ok(format!("\"{}\"", chosen.display()))
    }

    /// `.../target/debug/orbcat.exe` → `.../target/release/orbcat.exe`（存在才返回）
    fn release_sibling(exe: &std::path::Path) -> Option<std::path::PathBuf> {
        let s = exe.to_string_lossy().replace("\\", "/");
        if !s.contains("/debug/") {
            return None;
        }
        let cand = std::path::PathBuf::from(s.replace("/debug/", "/release/"));
        if cand.exists() {
            Some(cand)
        } else {
            None
        }
    }

    /// 是否已设置开机自启
    ///
    /// ⚠️ 用注册表 API 而不是 `reg query` 子进程：后者在 GUI 进程里同步等
    /// console 子进程，曾被观察为设置页"加载中"卡死元凶之一。
    pub fn is_enabled() -> bool {
        crate::win32::read_hkcu_sz(RUN_KEY, VALUE_NAME).is_some()
    }

    /// 启动时校准：若注册表里的自启项指向 debug exe，而 release exe 已存在，
    /// 就改写指向 release —— 否则开机永远弹黑框（debug 构建带控制台）。
    pub fn sync_to_release() {
        let Some(cur) = crate::win32::read_hkcu_sz(RUN_KEY, VALUE_NAME) else {
            return; // 没设自启，不用管
        };
        if !cur.to_lowercase().contains("debug") {
            return; // 已经指向别处（release / 安装版）
        }
        if release_sibling_path().is_none() {
            return; // release 还没构建，等构建后再说
        }
        if set(true).is_ok() {
            eprintln!("[orbcat] 开机自启已改指向 release exe（无控制台黑框）");
        }
    }

    fn release_sibling_path() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        release_sibling(&exe)
    }

    /// 设置/取消开机自启。
    ///
    /// ⚠️ 全程注册表 API，不用 `reg.exe` 子进程 —— GUI 进程（无控制台）同步等待
    /// console 子进程会挂死，且本函数还在 setup 启动路径上：一旦卡住，
    /// 后面的 manage / spawn_watcher 都不执行（表现为面板指示条永远空白）。
    pub fn set(enable: bool) -> Result<(), String> {
        if enable {
            let cmd = exe_cmd()?;
            crate::win32::write_hkcu_sz(RUN_KEY, VALUE_NAME, &cmd)
        } else {
            crate::win32::delete_hkcu_value(RUN_KEY, VALUE_NAME)
        }
    }
}

/// 查询开机自启状态
#[tauri::command]
fn autostart_status() -> bool {
    #[cfg(windows)]
    {
        autostart::is_enabled()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 设置开机自启
#[tauri::command]
fn autostart_set(enable: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        autostart::set(enable)?;
        eprintln!("[orbcat] 开机自启已{}", if enable { "开启" } else { "关闭" });
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enable;
        Err("当前平台未实现".into())
    }
}

/// 当前 exe 路径（设置页展示用，方便用户做快捷方式）
#[tauri::command]
fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into())
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— MCP
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct McpGroupView {
    name: String,
    summary: String,
    tool_count: usize,
    tokens: usize,
    active: bool,
    has_destructive: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct McpStatus {
    connected: bool,
    error: Option<String>,
    /// 当前网关地址（空串 = 未配置）。设置页的输入框显示它。
    url: String,
    /// 活跃工具 schema 的粗略 token 估算（仅展示）
    active_tokens: usize,
    groups: Vec<McpGroupView>,
}

#[tauri::command]
async fn mcp_status(state: State<'_, AppState>) -> Result<McpStatus, String> {
    // 保险丝：锁最多等 2 秒。就算未来再有谁持锁做慢事，设置页也只是
    // 显示「未连接 + 原因」，绝不能跟着卡死。
    let reg = match tokio::time::timeout(std::time::Duration::from_secs(2), state.mcp.lock()).await
    {
        Ok(g) => g,
        Err(_) => {
            return Ok(McpStatus {
                connected: false,
                error: Some("MCP 状态查询超时（后台通信僵持中，稍后自动恢复）".into()),
                url: String::new(),
                active_tokens: 0,
                groups: Vec::new(),
            })
        }
    };
    Ok(McpStatus {
        connected: reg.connected,
        error: reg.error.clone(),
        url: reg.url(),
        active_tokens: reg.active_tokens(),
        groups: reg
            .groups()
            .iter()
            .map(|g| McpGroupView {
                name: g.name.clone(),
                summary: g.summary.clone(),
                tool_count: g.tools.len(),
                tokens: g.total_tokens(),
                active: reg.is_active(&g.name),
                has_destructive: g.tools.iter().any(|t| t.is_destructive()),
            })
            .collect(),
    })
}

/// 手动重新拉取全部 MCP server 的工具清单
#[tauri::command]
async fn mcp_refresh(state: State<'_, AppState>) -> Result<(), String> {
    let clients = state.mcp.lock().await.clients_clone();
    let snap = mcp::ToolRegistry::fetch_snapshot(&clients).await;
    state.mcp.lock().await.apply_snapshot(snap);
    Ok(())
}

/// 读取全部 MCP server 配置（含禁用）
#[tauri::command]
fn mcp_servers_list(state: State<'_, AppState>) -> Vec<config::McpServerCfg> {
    config::load_mcp_servers(&state.data_dir)
}

/// 整表保存 MCP server 配置并立即重连
#[tauri::command]
async fn mcp_servers_save(
    state: State<'_, AppState>,
    servers: Vec<config::McpServerCfg>,
) -> Result<(), String> {
    // 规整：id 空则生成；url 去空白
    let mut cleaned: Vec<config::McpServerCfg> = Vec::new();
    for (i, mut s) in servers.into_iter().enumerate() {
        s.url = s.url.trim().to_string();
        if s.id.trim().is_empty() {
            s.id = format!("s{}", i + 1);
        }
        if s.label.trim().is_empty() {
            s.label = s.id.clone();
        }
        cleaned.push(s);
    }
    config::save_mcp_servers(&state.data_dir, &cleaned)?;
    {
        let mut reg = state.mcp.lock().await;
        reg.set_servers(&cleaned);
    }
    let clients = state.mcp.lock().await.clients_clone();
    let snap = mcp::ToolRegistry::fetch_snapshot(&clients).await;
    state.mcp.lock().await.apply_snapshot(snap);
    Ok(())
}

/// 设置/更换/禁用 MCP 网关地址（兼容旧单 URL 入口）。
#[tauri::command]
async fn mcp_set_url(state: State<'_, AppState>, url: String) -> Result<(), String> {
    let u = url.trim().to_string();
    if u.is_empty() {
        config::save_mcp_url(&state.data_dir, None)?;
        state.mcp.lock().await.set_url("");
        return Ok(());
    }
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err("MCP 地址必须是 http/https URL".into());
    }
    config::save_mcp_url(&state.data_dir, Some(&u))?;
    let mut reg = state.mcp.lock().await;
    reg.set_url(&u);
    drop(reg);
    let clients = state.mcp.lock().await.clients_clone();
    let snap = mcp::ToolRegistry::fetch_snapshot(&clients).await;
    state.mcp.lock().await.apply_snapshot(snap);
    Ok(())
}

/// 手动加载/卸载一个组（前端也能操作，和模型调的元工具等效）
#[tauri::command]
async fn mcp_load_group(state: State<'_, AppState>, name: String) -> Result<String, String> {
    let mut reg = state.mcp.lock().await;
    reg.load(&name)
}

#[tauri::command]
async fn mcp_unload_group(state: State<'_, AppState>, name: String) -> Result<String, String> {
    let mut reg = state.mcp.lock().await;
    reg.unload(&name)
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— agent
// ---------------------------------------------------------------------------

/// 跑一轮 agent 对话。
///
/// - `input`      用户文字
/// - `images`     用户附带的图片（`data:image/png;base64,...`），可为空
///
/// ⚠️ 前端传上来的 base64 可能很大（一张 1568px 的 PNG 约 2-4MB），
///    所以这里只做透传，不在日志里打印内容。
#[tauri::command]
async fn chat(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    input: String,
    images: Option<Vec<String>>,
) -> Result<agent::AgentRun, String> {
    let model_id = config::load_settings(&state.data_dir)
        .selected_model
        .ok_or_else(|| "尚未选择模型，请先在设置里选一个".to_string())?;

    let cfg = config::find_model(&state.data_dir, &model_id)?;

    // ⚠️ MutexGuard 不能跨 await，所以先把 gate clone 出来
    let gate: PermissionGate = {
        let g = state.gate.lock().map_err(|e| format!("锁失败: {e}"))?;
        g.clone()
    };
    let data_dir = state.data_dir.clone();

    // 图片一律先落盘（data URL → agent-data/images/<hash>.<ext>，内容去重）。
    // agent 与会话从此只认**文件路径**，是否转 base64 由模型能力决定。
    let imgs = images::save_all(&state.data_dir, &images.unwrap_or_default());
    eprintln!(
        "[orbcat] chat: 模型={} 文字={} 字 图片={} 张",
        cfg.id,
        input.chars().count(),
        imgs.len()
    );

    // 当前会话 id —— 回灌历史要用。
    // ⚠️ 时序：必须在这里取（`append_turn` 落盘**之前**），
    //    否则 agent 会把本轮问题也当成历史读回来，重复一遍。
    let session_id = sessions::current_id(&data_dir)
        .unwrap_or_else(|| sessions::ensure_current(&data_dir).id);

    // 新的一轮：登记到**本会话专属**的槽位（多会话并行 run）。
    // 取消标志是每 run 一个 `Arc<AtomicBool>`，停 A 不会停 B；
    // 插话收件箱同理，`chat_steer` 只会塞进目标会话那个。
    let slot = state.runs.begin(&session_id);

    // imgs 会被 move 进 agent::run，这里留一份给会话落盘
    let imgs_for_store = imgs.clone();

    // 采集一次前台上下文（用户在用 VSCode/Typora 看哪个目录）——注入本轮 system prompt。
    // 采集失败不影响对话，顶多少一段上下文。
    let fg = context::current();
    if let Some(f) = &fg {
        eprintln!("[orbcat] 前台上下文: {}", f.summary());
    }

    // 进度回调用事件推给前端 —— 否则 agent loop 的 30-60 秒是黑盒。
    // ⚠️ 事件**带 session id**（信封 `{sessionId, p}`）：并行 run 时前端要能分清
    //    这条进度是谁的，否则 B 会话的进度会画进 A 会话的时间线。
    let progress: agent::ProgressFn = {
        let win = window.clone();
        let sid = session_id.clone();
        std::sync::Arc::new(move |p: agent::Progress| {
            let _ = win.emit(
                "agent-progress",
                &serde_json::json!({ "sessionId": sid, "p": p }),
            );
        })
    };

    // 轮内增量落盘（2026-09-26 治「退出后整轮清空」）：
    // 开跑先把 [提问 + partial 占位] 写进会话，然后 agent loop 每到一个轮边界 /
    // 工具跑完就回调这里，把"目前已产出"覆盖进那条占位。
    // 进程中途被关掉时，磁盘上留下的是"问了什么 + 已经跑到哪一步"，而不是空文件。
    //
    // ⚠️ 顺序必须在 `agent::run` **之前**：占位要在 loop 跑起来前就存在，
    //    否则第一段产出的 checkpoint 找不到占位（`checkpoint_turn` 会静默跳过）。
    if let Err(e) = sessions::begin_turn_in(&data_dir, Some(&session_id), &input, &imgs_for_store) {
        eprintln!("[orbcat] ⚠️ 开跑落盘失败（不影响对话）: {e}");
    }
    let on_checkpoint: agent::CheckpointFn = {
        let dd = data_dir.clone();
        // ⚠️ 会话 id 跟着跑：用户中途切走时，"当前会话"已经变了，
        //    但这一轮的增量必须落回**开跑时**那个会话（切走会话、后台继续跑）。
        let sid = session_id.clone();
        std::sync::Arc::new(move |steps: &[agent::AgentStep], answer: &str, reasoning: &str| {
            let stored = sessions::steps_from_agent(steps);
            let r = if reasoning.is_empty() { None } else { Some(reasoning) };
            if let Err(e) = sessions::checkpoint_turn_in(&dd, Some(&sid), &stored, answer, r) {
                eprintln!("[orbcat] ⚠️ 轮内落盘失败（不影响对话）: {e}");
            }
        })
    };

    // 新一轮开始 —— 上一轮记下的"最近一次拒绝"不能再用，
    // 否则它会拿去校验这一轮的申请（`request_access` 靠它判断"申请范围是否包含被拒路径"）。
    state.perm.clear_denial();

    let result = agent::run(
        &cfg,
        &gate,
        &data_dir,
        &state.mcp,
        &input,
        imgs,
        progress,
        Some(on_checkpoint),
        fg.as_ref(),
        slot.cancel.clone(),
        &session_id,
        &slot.hub,
        &state.perm,
    )
    .await;

    // 关闭本会话的插话收件箱并取走残留。
    // ⚠️ 必须在 `match result` **之前**：否则失败/中断路径上槽位会一直挂着，
    //    下一次 `chat_steer` 会把消息塞进一个永远没人消费的队列。
    let leftover = state.runs.end(&session_id);

    // 一轮结束 → 清掉「本轮」档授权。
    // ⚠️ 必须放在 `match result` **之前**：出错提前返回时同样要清，
    //    否则失败那一轮批的"本轮"授权会活到下一次对话，"一轮"语义当场破掉。
    // ⚠️ 并行 run 时**不能无条件清**：grant 表是全局内存的，A 收尾会把 B 刚批的
    //    「本轮」授权一起清掉。所以只有"没有别的会话在跑"时才清；还有别的 run 时
    //    留到它最后一个收尾时清（略滞后，可接受）。
    if state.runs.active_count() == 0 {
        if let Ok(mut store) = state.perm.grants().lock() {
            let n = store.clear_tier(GrantTier::Turn);
            if n > 0 {
                eprintln!("[orbcat] 一轮结束，清掉 {n} 条「本轮」授权");
            }
        }
    }
    // ⚠️ 命令授权**不参与**这里：永久档跨会话存活，靠设置页「撤销」才失效。

    // ⚠️ **成功与失败都要走落盘**（2026-09-22 修 bug）。
    //
    // 这行以前是 `let mut run = result?;` —— 于是撞 429（或任何 API 错误）时
    // 整轮一个字都不写：提问、思考、工具步骤、半截正文全部蒸发，屏幕上只剩
    // 一条红色横幅。现在 `agent::run` 的 Err 里带着部分结果（见 `agent::RunAbort`），
    // 先落盘、再把错误文案抛给前端（真错误不伪装成正常结束）。

    let mut run = match result {
        Ok(run) => run,
        Err(ab) => {
            let agent::RunAbort { message, mut run } = ab;
            run.pending_steers = leftover.into_iter().map(|m| m.text).collect();
            eprintln!(
                "[orbcat] 本轮失败（{}），已落盘部分结果：{} 条步骤 / {} 字正文",
                message.chars().take(60).collect::<String>(),
                run.steps.len(),
                run.answer.chars().count()
            );
            persist_run(&state, &session_id, &cfg.id, &input, &imgs_for_store, &run);
            return Err(message);
        }
    };

    // 没抓到思维链就留一行痕迹。
    // 为什么值得专门打日志：字段名各家不同（reasoning_content / reasoning / thinking），
    // 漏认一个不会报错，只会"思考过程那一块永远不出现" —— 没有这行日志，
    // 只能靠猜是模型不支持还是解析漏了。
    if run.reasoning.is_none() {
        eprintln!(
            "[orbcat] 本轮未收到思维链（模型 {} 可能不支持思考，或字段名未覆盖）",
            cfg.id
        );
    }

    // 没来得及送达的插话回填给前端（还原到输入框），别让用户白打一遍字
    run.pending_steers = leftover.into_iter().map(|m| m.text).collect();
    if !run.pending_steers.is_empty() {
        eprintln!(
            "[orbcat] {} 条插话未送达，已回填输入框",
            run.pending_steers.len()
        );
    }

    persist_run(&state, &session_id, &cfg.id, &input, &imgs_for_store, &run);

    Ok(run)
}

/// 一轮结束后的持久化：日记（append-only）+ 会话（JSON）。
///
/// ## 为什么抽成独立函数（2026-09-22）
///
/// 成功、被「停止」、轮数用尽、**API 错误**四条路径都要落盘，差别只在
/// `TurnRecord::interrupted`。以前这段内联在 `chat` 里、且位于 `result?` 之后，
/// 错误路径根本走不到 —— 这是"整轮消失"的根因（用户 2026-09-22 报的 bug）。
///
/// ## 日记为什么不写半截轮
///
/// `interrupted` 的轮（停止 / 轮数用尽 / 出错）**不写日记**：日记是"聊过什么"的
/// 流水账，半截问答写进去，以后翻日记的人会以为当时聊完了。
/// **会话 JSON 照样落盘** —— 用户要看到自己的提问和已产出的部分。
///
/// 落盘失败只打日志、不影响主流程（用户已经看到回答了）。
fn persist_run(
    app: &AppState,
    session_id: &str,
    model_id: &str,
    input: &str,
    images: &[String],
    run: &agent::AgentRun,
) {
    if !run.interrupted {
        if let Err(e) = app.memory.append_daily(
            input,
            &sessions::strip_step_folds(&run.answer),
            run.steps.iter().filter(|s| s.kind == "tool_call").count(),
        ) {
            eprintln!("[orbcat] 写日记失败: {e}");
        }
    }
    let steps = sessions::steps_from_agent(&run.steps);
    // ⚠️ 显式传 session_id：用户可能已经切走，"当前会话"不再是这一轮的目标。
    if let Err(e) = sessions::append_run_in(
        &app.data_dir,
        Some(session_id),
        &sessions::TurnRecord {
            user_text: input,
            images,
            steers: &run.steers,
            answer: &run.answer,
            reasoning: run.reasoning.as_deref(),
            steps: &steps,
            interrupted: run.interrupted,
            model: model_id,
            usage: run.usage,
        },
    ) {
        eprintln!("[orbcat] 写会话失败: {e}");
    }
}

/// 拉取 token 用量报表（跨**全部会话**，每条带用量的问答一条记录）。
///
/// 前端拿这份扁平记录自己按「本地日期 × 模型」聚合 —— 时区只有前端知道。
#[tauri::command]
async fn usage_report(state: State<'_, AppState>) -> Result<Vec<sessions::UsageRecord>, String> {
    Ok(sessions::usage_report(&state.data_dir))
}

/// 「正在思考」中的取消标志。chat 每轮循环检查，置位后尽快返回"已停止"。
static CHAT_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 取取消标志的 `'static` 引用，给工具层做「执行期取消探针」用
/// （见 `shell::CancelFn`）—— 命令跑到一半也能被「停止」掐断，而不是等到轮边界。
pub fn chat_cancel_flag() -> &'static std::sync::atomic::AtomicBool {
    &CHAT_CANCEL
}

/// 前端「停止」按钮：只停**指定会话**的那一轮（多会话并行 run）。
///
/// 不传 `session_id` = 兼容旧调用，停掉**所有**在跑的会话。
#[tauri::command]
fn chat_cancel(state: State<'_, AppState>, session_id: Option<String>) {
    let ids: Vec<String> = match session_id {
        Some(s) if !s.is_empty() => vec![s],
        _ => state.runs.active_ids(),
    };
    for id in &ids {
        // 只置**本会话**的取消标志 —— 停 A 绝不能连 B 一起停
        state.runs.cancel(id);
        // 权限申请是**阻塞**在 ask* 上的：只置 cancel 标志叫不醒它，
        // 用户会看到「点了停止还一直转」。一并作废该会话的待办，立刻放行它的 agent loop。
        state
            .perm
            .cancel_all_in(id, "用户已停止本轮，此申请作废（不是对请求本身的拒绝）。");
    }
    eprintln!("[orbcat] 收到停止请求：{}", ids.join(", "));
}

/// **执行中插话**：把消息塞进收件箱，等当前轮（LLM 调用 + 工具）结束后
/// 作为追加的 user 消息送入。
///
/// 为什么是"排队"而不是"立刻打断"：
/// - 立刻打断会丢掉当前这轮已经流出的正文和正在跑的工具 —— 用户只是想说
///   一句"顺便也看看 X"，不值得为此作废已经烧掉的 token。
/// - 排队同样保住了「上下文延续」：它就是一条普通的历史消息，模型下一轮
///   自然看得到，跟直接追问没区别。
///
/// 权限说明：这条消息**不在这里**做任何校验。它进的是同一个 `messages`、
/// 同一个 agent loop、同一个权限网关 —— 越权与否在执行工具那一刻判定。
#[tauri::command]
async fn chat_steer(
    state: State<'_, AppState>,
    input: String,
    images: Option<Vec<String>>,
    session_id: Option<String>,
) -> Result<agent::SteerAck, String> {
    // 显式指定目标会话；不传则退回"当前会话"（旧调用兼容）。
    // ⚠️ 并行 run 下必须显式路由 —— 否则 B 会话里打的字会插进 A 的对话里。
    let sid = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => sessions::current_id(&state.data_dir).unwrap_or_default(),
    };
    if sid.is_empty() {
        return Err("当前没有进行中的对话".into());
    }
    let text = input.trim().to_string();
    // 图片同样先落盘（与 chat 一致：agent 只认文件路径）
    let imgs = images::save_all(&state.data_dir, &images.unwrap_or_default());
    if text.is_empty() && imgs.is_empty() {
        return Err("插话内容为空".into());
    }
    state
        .runs
        .push(
            &sid,
            agent::SteeredMsg {
                id: agent::new_steer_id(),
                text,
                images: imgs,
            },
        )
        .await
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 会话
// ---------------------------------------------------------------------------

/// 当前会话全量 + 会话列表（前端展开面板时拉一次即可恢复历史）
#[tauri::command]
fn session_state(state: State<'_, AppState>) -> serde_json::Value {
    let cur = sessions::ensure_current(&state.data_dir);
    serde_json::json!({
        "session": cur,
        "list": sessions::list(&state.data_dir),
    })
}

/// 启动自检 —— 前端首次拉一次，决定是否提示「初始化」。
///
/// 为什么需要它：`agent-data/` 不存在时，人格文件、记忆、技能**全都没有**，
/// 连 `skills/` 目录都读不到（技能清单是空的）。所以"是否需要初始化"
/// 这件事**不能靠技能或记忆告诉模型**，必须由代码直接检测并告诉用户。
///
/// 判定口径**只看 `agent-data` 目录本身是否存在**，不看里面有没有文件：
/// 目录在 = 用户有数据（哪怕某人删空了配置文件，那也是他的选择，不该被当成新装）。
#[tauri::command]
fn boot_state(state: State<'_, AppState>) -> serde_json::Value {
    let needs_init = !state.data_dir.exists();
    // 没有模型就聊不了 —— 前端据此把「去设置加模型」做成首要引导
    let has_models = config::load_models(&state.data_dir)
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    serde_json::json!({
        "needsInit": needs_init,
        "hasModels": has_models,
        "dataDir": state.data_dir.display().to_string(),
    })
}

/// **初始化**：纯 Rust 建出 `agent-data/` 骨架（目录 + 空 `models.json`）。
///
/// 刻意不依赖模型 —— 见 [`bootstrap`] 模块头部的死锁说明。
/// 幂等：已存在的文件一个都不动，重复点不会毁数据。
#[tauri::command]
fn bootstrap_data(state: State<'_, AppState>) -> Result<bootstrap::BootstrapReport, String> {
    let r = bootstrap::bootstrap_agent_data(&state.data_dir)?;
    eprintln!(
        "[orbcat] 初始化完成: 建目录 {} 个 / 建文件 {} 个 / 跳过 {} 个",
        r.created_dirs.len(),
        r.created_files.len(),
        r.skipped.len()
    );
    Ok(r)
}

/// 新建会话（旧的自动保留在列表里）
///
/// 多会话并行 run 下**放行**：新建不碰别的会话，落盘各自带着自己的 `session_id`。
/// 仍然**禁止**的只剩"改动某个会话本身"的动作（删除/截断/分叉**该会话**），见各自命令。
#[tauri::command]
fn session_new(state: State<'_, AppState>) -> Result<sessions::Session, String> {
    let s = sessions::new_session(&state.data_dir);
    eprintln!("[orbcat] 新会话 {}", s.id);
    // 新会话没有历史授权 —— 顺手清掉上一段残留的（尤其 `Once`/`Turn`）。
    // ⚠️ 但**有 run 在跑时跳过**：`reload_grants_for_session` 会把内存授权换成新会话的，
    //    正在跑的任务会当场丢权限（工具调用全被拒）。
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &s.id);
    }
    Ok(s)
}

/// 切换到指定会话
///
/// **允许在别的会话跑着的时候切走，也允许切到另一个正在跑的会话**（多会话并行 run）。
/// 落盘路径（`*_in` 系列）带着开跑时的 `session_id`，回答不会写错会话。
///
/// ⚠️ 但**有任意 run 在跑时不重载授权**：`reload_grants_for_session` 会把内存里那份
/// 换成"目标会话"的，正在跑的任务会因此**当场丢权限**（它的工具调用全被拒）。
#[tauri::command]
fn session_switch(state: State<'_, AppState>, id: String) -> Result<sessions::Session, String> {
    // 换会话 = 上一段「整个任务」结束。不清的话，A 任务批的授权会漏到 B 任务里去；
    // 同时把新会话自己落盘的那份装回来。
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &id);
    }
    sessions::switch(&state.data_dir, &id)
}

/// **从主聊天的某条消息处分叉**出一条新会话（并自动切过去）。
///
/// 只有主聊天能被分叉；任务会话与分叉出来的会话都会被后端拒绝（前端也不渲染入口，
/// 但这里才是硬保护）。
///
/// ⚠️ 必须整体重装授权：主聊天内存里的 `Task` 档授权不能漏给分叉会话
///    （分叉会话要授权，得在它自己里面重新批 —— 批了也只活在它自己的
///    `<id>.grants.json` 里，删掉分叉就一并没有）。
#[tauri::command]
fn session_fork(state: State<'_, AppState>, upto: usize) -> Result<sessions::Session, String> {
    let cur = sessions::ensure_current(&state.data_dir);
    // 只拦"**这条会话**正在跑" —— 其它会话在跑不影响分叉
    if state.runs.is_active(&cur.id) {
        return Err("该会话正在运行，先停止或等它结束再分叉".into());
    }
    let s = sessions::fork(&state.data_dir, &cur, upto)?;
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &s.id);
    }
    eprintln!(
        "[orbcat] 分叉 {} → {}（截至第 {upto} 条）",
        cur.id, s.id
    );
    Ok(s)
}

/// **从当前会话的第 `upto` 条消息起删到末尾**（含 `upto` 自己）。
///
/// 语义：对话流里每条消息的「删除」= 从这条开始后面全不要了。**不可恢复**，
/// 前端必须二次确认。删完直接覆盖写盘，所以下一轮喂给模型的上下文立刻变短
/// （`history::pick_window` 读的就是这份 messages）。
///
/// ⚠️ **该会话**正在跑时禁止：`chat` 跑的时候正在用这份 messages，中途截断会让
///    `append_run_in` 把回答落进一条逻辑上已经不存在的对话里/或重复落盘。
///    只拦这一条会话 —— 其它会话在跑不影响。
#[tauri::command]
fn session_truncate(state: State<'_, AppState>, upto: usize) -> Result<sessions::Session, String> {
    let cur = sessions::ensure_current(&state.data_dir);
    if state.runs.is_active(&cur.id) {
        return Err("该会话正在运行，先停止或等它结束再删除消息".into());
    }
    let s = sessions::truncate(&state.data_dir, &cur.id, upto)?;
    eprintln!(
        "[orbcat] 删除消息：{} 从第 {} 条起截断，剩 {} 条",
        s.id,
        upto,
        s.messages.len()
    );
    Ok(s)
}

/// 对当前会话做一次上下文压缩（compact）。
///
/// `force=true` 时跳过"水位检查"强制压缩（手动触发用）。
/// 摘要会**落盘**到会话文件（`summary` / `summary_upto`），之后每轮回灌
/// 直接读它 —— 保证 prompt cache 前缀稳定。
#[tauri::command]
async fn session_compact(
    state: State<'_, AppState>,
    force: Option<bool>,
) -> Result<history::CompactOutcome, String> {
    let cur = sessions::ensure_current(&state.data_dir);
    // 只拦"**这条会话**正在跑" —— 压缩会改写 summary/summary_upto，
    // 跑到一半的那轮 checkpoint 会写进一份对不上的上下文。
    if state.runs.is_active(&cur.id) {
        return Err("该会话正在运行，先停止或等它结束再压缩上下文".into());
    }
    let model_id = config::load_settings(&state.data_dir)
        .selected_model
        .ok_or_else(|| "尚未选择模型，无法生成摘要".to_string())?;
    let cfg = config::find_model(&state.data_dir, &model_id)?;
    history::compact(
        &cfg,
        &state.data_dir,
        &cur.id,
        history::COMPACT_KEEP_RECENT,
        force.unwrap_or(true),
        sessions::now_ms(),
    )
    .await
}

/// 删除会话
#[tauri::command]
fn session_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    // 只拦"**这条会话**正在跑" —— 它的 run 还在往这个文件里写，
    // 删掉之后 `append_run_in` 会复活/写坏一份已被删的会话。
    // 其它会话在跑不影响删除。
    if state.runs.is_active(&id) {
        return Err("该会话正在运行，先停止或等它结束再删除".into());
    }
    sessions::delete(&state.data_dir, &id)?;
    // 会话文件连同它的 `.grants.json` 一起没了，内存里那份也得跟着走。
    // `delete` 内部在删当前会话时会自动 `ensure_current`，所以这里取到的一定是有效 id。
    let cur = sessions::current_id(&state.data_dir).unwrap_or_default();
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &cur);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 申请权限
// ---------------------------------------------------------------------------

/// 用户对一条权限申请的决定（前端卡片按钮 → 这里 → 唤醒阻塞中的 `request_access`）。
///
/// `reason` 是**拒绝理由**（可空）：它会原样回给模型，让模型知道该怎么调整，
/// 而不是盲目重试或干脆放弃。
#[tauri::command]
fn perm_request_decide(
    state: State<'_, AppState>,
    id: String,
    decision: String,
    reason: Option<String>,
) -> Result<(), String> {
    let d = perm_request::Decision::parse(&decision).ok_or_else(|| {
        format!("未知的决定：{decision}（只能是 approve / narrow / prefix / full / always / deny）")
    })?;
    // id 前缀路由：`pr*` = 文件级（pending），`pc*` / `pm*` / `pa*` = 命令 / MCP / ask_user（pending_cmds）。
    // ⚠️ `pm*` 必须走 decide_command —— 它登记在 pending_cmds 里；
    //    早先只认 `pc` 打头，MCP 申请被误送进 decide() 后报「不存在或已处理」，
    //    前端还把卡片删了，agent 一直阻塞到 3 分钟超时（用户看到的就是卡死）。
    // `pa*`（ask_user）同理；它的"决定"只有 approve/deny，**回答文本放在 reason 里**。
    if id.starts_with("pc") || id.starts_with("pm") || id.starts_with("pa") {
        state.perm.decide_command(&id, d, reason.unwrap_or_default())
    } else {
        state.perm.decide(&id, d, reason.unwrap_or_default())
    }
}

/// 当前待用户拍板的申请（前端重绘 / 启动时补拉用）
#[tauri::command]
fn perm_requests_list(state: State<'_, AppState>) -> Vec<perm_request::PendingView> {
    state.perm.pending_views()
}

/// 当前生效的临时授权（设置页展示 + 手动撤销用）
#[tauri::command]
fn perm_grants_list(state: State<'_, AppState>) -> Result<Vec<permission::Grant>, String> {
    let store = state.perm.grants().lock().map_err(|e| format!("锁失败: {e}"))?;
    Ok(store.grants().to_vec())
}

/// 撤销临时授权（按前缀，可限定档位）。返回撤销条数。
#[tauri::command]
fn perm_grant_revoke(
    state: State<'_, AppState>,
    prefix: String,
    tier: Option<String>,
) -> Result<usize, String> {
    let t = match tier.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(
            permission::GrantTier::parse(s).ok_or_else(|| format!("未知档位：{s}"))?,
        ),
        None => None,
    };

    let n = {
        let mut store = state.perm.grants().lock().map_err(|e| format!("锁失败: {e}"))?;
        store.revoke(&prefix, t)
    };

    // 撤了 `Task` 档必须同步落盘 —— 否则重启后它又回来了，用户会觉得"撤销没生效"
    if n > 0 {
        let sid = sessions::current_id(&state.data_dir).unwrap_or_default();
        if let Err(e) = state.perm.persist_task_grants(&state.data_dir, &sid) {
            eprintln!("[orbcat] ⚠️ 撤销后同步授权文件失败: {e}");
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 记忆
// ---------------------------------------------------------------------------

#[tauri::command]
fn mem_pending(state: State<'_, AppState>) -> Result<Vec<memory::Candidate>, String> {
    state.memory.list_pending()
}

#[tauri::command]
fn mem_approve(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.memory.approve(&id)
}

#[tauri::command]
fn mem_reject(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.memory.reject(&id)
}

/// 用户手写一条记忆候选（和模型提交走同一审批通道）
#[tauri::command]
fn mem_propose(state: State<'_, AppState>, content: String) -> Result<memory::Candidate, String> {
    state.memory.propose(&content, "user")
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 项目索引（仿 WB：SQLite 索引 + md 记忆）
// ---------------------------------------------------------------------------

#[tauri::command]
fn pmem_list(state: State<'_, AppState>) -> Result<Vec<pmem::ProjectRow>, String> {
    pmem::list_projects(&state.data_dir)
}

#[tauri::command]
fn pmem_create(
    state: State<'_, AppState>,
    name: String,
    root_path: Option<String>,
) -> Result<pmem::ProjectRow, String> {
    pmem::create_project(&state.data_dir, &name, root_path.as_deref().unwrap_or(""))
}

#[tauri::command]
fn pmem_delete(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    pmem::delete_project(&state.data_dir, id)?;
    // 删的是当前激活项目 → 回到主对话
    let mut s = config::load_settings(&state.data_dir);
    if s.active_project_id == Some(id) {
        s.active_project_id = None;
        config::save_settings(&state.data_dir, &s)?;
    }
    Ok(())
}

/// 切换项目：`id` 为 `null` = 主对话（无项目记忆）
#[tauri::command]
fn pmem_set_active(state: State<'_, AppState>, id: Option<i64>) -> Result<(), String> {
    if let Some(pid) = id {
        let all = pmem::list_projects(&state.data_dir)?;
        if !all.iter().any(|p| p.id == pid) {
            return Err(format!("找不到项目 id={pid}"));
        }
    }
    let mut s = config::load_settings(&state.data_dir);
    s.active_project_id = id;
    config::save_settings(&state.data_dir, &s)?;
    Ok(())
}

#[tauri::command]
fn pmem_get_active(state: State<'_, AppState>) -> Result<Option<pmem::ProjectRow>, String> {
    let s = config::load_settings(&state.data_dir);
    let Some(pid) = s.active_project_id else {
        return Ok(None);
    };
    let all = pmem::list_projects(&state.data_dir)?;
    Ok(all.into_iter().find(|p| p.id == pid))
}

/// 往**当前激活项目**的 MEMORY.md 追加一条（项目记忆不走全局 pending）
#[tauri::command]
fn pmem_remember(state: State<'_, AppState>, content: String) -> Result<String, String> {
    let s = config::load_settings(&state.data_dir);
    let Some(pid) = s.active_project_id else {
        return Err("当前是主对话，没有激活项目".into());
    };
    let all = pmem::list_projects(&state.data_dir)?;
    let p = all
        .into_iter()
        .find(|x| x.id == pid)
        .ok_or_else(|| "激活项目已不存在".to_string())?;
    pmem::append_project_memory(&state.data_dir, &p.name, &content, "user")?;
    Ok(p.name)
}

/// 把会话绑到项目（null = 主对话）
#[tauri::command]
fn pmem_bind_session(
    state: State<'_, AppState>,
    session_id: String,
    project_id: Option<i64>,
) -> Result<(), String> {
    pmem::bind_session(&state.data_dir, &session_id, project_id)
}

/// session_id → 项目名（null = 主对话）
#[tauri::command]
fn pmem_session_map(
    state: State<'_, AppState>,
) -> Result<std::collections::HashMap<String, Option<String>>, String> {
    pmem::session_map(&state.data_dir)
}

#[tauri::command]
fn pmem_rename(state: State<'_, AppState>, id: i64, name: String) -> Result<(), String> {
    pmem::rename_project(&state.data_dir, id, &name)
}

/// 清空项目 MEMORY.md（项目保留）
#[tauri::command]
fn pmem_clear_memory(state: State<'_, AppState>, name: String) -> Result<(), String> {
    pmem::clear_project_memory(&state.data_dir, &name)
}

/// 改会话标题
#[tauri::command]
fn session_set_title(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    sessions::set_title(&state.data_dir, &id, &title)
}

/// 截取当前主屏幕，返回 `data:image/jpeg;base64,...`（供前端预览 + 随消息发送）
#[tauri::command]
fn capture_screen() -> Result<String, String> {
    capture::capture_screen_data_url(capture::ShotFormat::Jpeg)
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 窗口
// ---------------------------------------------------------------------------

#[tauri::command]
fn set_window_mode(window: tauri::WebviewWindow, mode: String) -> Result<(), String> {
    if mode != "orb" && mode != "panel" && mode != "menu" {
        return Err(format!("未知模式「{mode}」，应为 orb / panel / menu"));
    }

    #[cfg(windows)]
    apply_mode(&window, &mode)?;

    #[cfg(not(windows))]
    {
        let _ = (&window, &mode);
    }

    Ok(())
}

#[tauri::command]
fn raise_orb(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(windows)]
    {
        let hwnd = window.hwnd().map_err(|e| format!("获取窗口句柄失败: {e}"))?;
        win32::keep_topmost_without_activating(hwnd.0 as *mut c_void);
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 系统托盘
// ---------------------------------------------------------------------------

/// 注册托盘图标 + 右键「退出」。
///
/// 不改 skipTaskbar / TOOLWINDOW：悬浮球仍不该占任务栏或 Alt+Tab。
/// 托盘只补一个可见的后台入口和退出途径。
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&quit])?;

    let icon = match app.default_window_icon() {
        Some(ic) => ic.clone(),
        None => {
            eprintln!("[orbcat] 无默认窗口图标，跳过托盘");
            return Ok(());
        }
    };

    let _tray = TrayIconBuilder::with_id("orbcat-tray")
        .icon(icon)
        .tooltip("orbcat")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            if event.id.as_ref() == "quit" {
                eprintln!("[orbcat] 用户从托盘退出");
                app.exit(0);
            }
        })
        .build(app)?;

    eprintln!("[orbcat] 托盘图标已注册（右键退出）");
    Ok(())
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // --- 初始化权限网关 ---
            let data_dir = resolve_data_dir(app.handle());
            let app_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));

            eprintln!("[orbcat] agent-data 目录: {}", data_dir.display());

            // 开机自启校准：若之前的自启项指向 debug exe，release 已存在则改指 release
            #[cfg(windows)]
            autostart::sync_to_release();

            // 前台应用轮询线程（记录「用户上一个在看的应用」，对话时注入上下文）
            context::spawn_watcher();

            let gate = permission::load_gate(&data_dir, &app_dir);
            for r in gate.rules() {
                eprintln!(
                    "[orbcat] 权限规则: {} => {:?} ({})",
                    r.prefix.display(),
                    r.access,
                    r.label
                );
            }

            // 模型配置来源（便于排查）
            eprintln!(
                "[orbcat] 模型配置: {}",
                config::models_json_path(&data_dir).display()
            );

            // --- MCP 注册表（多 server，懒加载）---
            // 拉取在后台进行：server 没起来也不能拖慢应用启动。
            let servers = config::resolve_mcp_servers(&data_dir);
            let registry = if servers.is_empty() {
                eprintln!("[orbcat] MCP 未配置（设置页可添加多个 server）");
                mcp::ToolRegistry::new("")
            } else {
                eprintln!(
                    "[orbcat] MCP servers: {}（后台拉取工具清单中…）",
                    servers.len()
                );
                mcp::ToolRegistry::with_servers(&servers)
            };

            let shared = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
            if !servers.is_empty() {
                let bg = shared.clone();
                tauri::async_runtime::spawn(async move {
                    // 锁外做网络 IO，锁内只落内存账。
                    let clients = bg.lock().await.clients_clone();
                    let snap = mcp::ToolRegistry::fetch_snapshot(&clients).await;
                    bg.lock().await.apply_snapshot(snap);
                });
            }

            // 「申请权限」的通道。注入发事件的回调后，申请才会推到窗口变成一张卡片 ——
            // 不注入的话用户看不到，申请只能走 3 分钟超时（fail-closed）。
            //
            // 注入回调而不是把 `AppHandle` 传进 `PermHub`：那样 `perm_request`
            // 会和界面实现绑死，单元测试也得拖着 Tauri 运行时。
            let perm = std::sync::Arc::new(perm_request::PermHub::new());
            {
                let handle = app.handle().clone();
                perm.attach_emitter(Box::new(move |event, payload| {
                    if let Err(e) = handle.emit(event, payload) {
                        eprintln!("[orbcat] ⚠️ 推送 {event} 事件失败: {e}");
                    }
                }));
            }

            // 命令授权是**永久**的（2026-09-22 用户拍板）：注入数据目录 + 把上次
            // 落盘的永久档读回来。文件授权不在这里 —— 它按会话走
            // `reload_grants_for_session`。
            perm.attach_data_dir(data_dir.clone());
            let lasting_cmds = command_policy::load_grants(&data_dir);
            if !lasting_cmds.is_empty() {
                match perm.cmd_grants().lock() {
                    Ok(mut store) => {
                        let n = lasting_cmds.len();
                        store.replace(lasting_cmds);
                        eprintln!("[orbcat] 已载入 {n} 条永久命令授权");
                    }
                    Err(e) => {
                        eprintln!("[orbcat] ⚠️ 命令授权表锁失败，永久授权未载入: {e}")
                    }
                }
            }

            app.manage(AppState {
                gate: Mutex::new(gate),
                perm_mtime: Mutex::new(None),
                data_dir: data_dir.clone(),
                memory: memory::MemoryStore::new(data_dir),
                mcp: shared,
                perm,
                runs: agent::RunRegistry::default(),
            });

            // --- 窗口初始形态 ---
            #[cfg(windows)]
            if let Some(win) = app.get_webview_window("orb") {
                if let Err(e) = apply_mode(&win, "orb") {
                    eprintln!("[orbcat] 初始化窗口形态失败: {e}");
                }
            }

            // --- 系统托盘（右键退出）---
            // skipTaskbar + TOOLWINDOW 会让应用在任务栏/「应用」列表里隐身，
            // 托盘是用户能正常关掉它的入口。打开面板仍靠悬浮球。
            if let Err(e) = setup_tray(app) {
                eprintln!("[orbcat] 托盘初始化失败: {e}");
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            perm_check,
            perm_rules,
            perm_add_rule,
            perm_revoke_label,
            perm_set_rules,
            perm_remove_rule,
            perm_rules_path,
            perm_cmd_policy,
            perm_cmd_policy_path,
            perm_cmd_grants_list,
            perm_cmd_grant_revoke,
            perm_cmd_grants_clear,
            mcp_grants_list,
            mcp_grant_revoke,
            mcp_grants_clear,
            perm_cmd_policy_set,
            perm_cmd_test,
            perm_cmd_audit_tail,
            perm_request_decide,
            perm_requests_list,
            perm_grants_list,
            perm_grant_revoke,
            list_models,
            get_selected_model,
            set_selected_model,
            models_add,
            models_edit,
            models_get_edit,
            models_fetch_remote,
            models_fetch_remote_using,
            models_import_remote,
            models_remove,
            test_model,
            chat,
            capture_screen,
            mem_pending,
            mem_approve,
            mem_reject,
            mem_propose,
            pmem_list,
            pmem_create,
            pmem_delete,
            pmem_set_active,
            pmem_get_active,
            pmem_remember,
            pmem_bind_session,
            pmem_session_map,
            pmem_rename,
            pmem_clear_memory,
            mem_read,
            mem_files,
            mem_projects,
            get_settings,
            search_status,
            search_set,
            search_test,
            set_blur_collapse,
            set_exec_trust,
            foreground_context,
            foreground_history,
            image_thumb,
            skills_list,
            chat_cancel,
            chat_steer,
            session_state,
            boot_state,
            bootstrap_data,
            session_new,
            session_switch,
            session_delete,
            session_set_title,
            session_compact,
            session_fork,
            session_truncate,
            usage_report,
            mcp_status,
            mcp_refresh,
            mcp_set_url,
            mcp_servers_list,
            mcp_servers_save,
            mcp_load_group,
            mcp_unload_group,
            app_quit,
            autostart_status,
            autostart_set,
            exe_path,
            set_window_mode,
            raise_orb,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
