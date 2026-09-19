//! float-agent — Tauri 后端入口

mod agent;
mod bootstrap;
mod capture;
mod config;
mod context;
mod history;
mod images;
mod llm;
mod mcp;
mod memory;
mod permission;
mod sessions;
mod skills;
mod tools;

#[cfg(windows)]
mod win32;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use permission::{Access, PermissionGate};
use tauri::{Emitter, Manager, State};

// ---------------------------------------------------------------------------
// 窗口布局常量（逻辑像素）
// ---------------------------------------------------------------------------

/// 胶囊态边长
const ORB_LOGICAL: f64 = 56.0;
/// 面板态尺寸
const PANEL_W_LOGICAL: f64 = 420.0;
const PANEL_H_LOGICAL: f64 = 600.0;
/// 右键菜单态尺寸（只够放一个小菜单卡片；4 个菜单项）
const MENU_W_LOGICAL: f64 = 196.0;
const MENU_H_LOGICAL: f64 = 190.0;
/// 距屏幕边缘留白
const MARGIN_LOGICAL: f64 = 24.0;
/// 胶囊在屏幕高度的这个比例处
const ORB_Y_RATIO: f64 = 0.40;

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
        eprintln!("[float-agent] 权限规则已从文件热加载（{} 条）", f.rules.len());
    }
}

// ---------------------------------------------------------------------------
// 数据目录解析
// ---------------------------------------------------------------------------

/// 解析 agent-data 目录。
///
/// 优先级：
///   1. 环境变量 `FLOAT_AGENT_DATA_DIR`
///   2. 源码树里的 `<项目根>/agent-data`（开发期）
///   3. 系统应用数据目录（发布版）
///
/// 注：`agent-data/` 是**运行时数据**（API key、会话、截图、日记），
/// 已在 `.gitignore` 里排除 —— 仓库里只保留 `初始化.md`，
/// 新克隆的用户让 agent 读它即可自建整套结构。
fn resolve_data_dir(app: &tauri::AppHandle) -> PathBuf {
    if let Ok(p) = std::env::var("FLOAT_AGENT_DATA_DIR") {
        let p = PathBuf::from(p);
        if p.exists() {
            return p;
        }
    }

    // 开发期：CARGO_MANIFEST_DIR = <项目根>/float-agent/src-tauri
    // 往上**一级**即 float-agent/，故 agent-data 与 src-tauri 平级。
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
        _ => (orb_x, orb_y, orb_px, orb_px),
    }
}

/// 应用某个形态：设置 bounds + 切换窗口扩展样式。
#[cfg(windows)]
fn apply_mode(win: &tauri::WebviewWindow, mode: &str) -> Result<(), String> {
    // ---- 诊断开关 ----
    // 设了 FLOAT_AGENT_NO_WINDOW_MODE 就跳过所有窗口形态调整，
    // 用来把它与"渲染问题"隔离开做二分排查。
    if std::env::var("FLOAT_AGENT_NO_WINDOW_MODE").is_ok() {
        eprintln!("[float-agent] apply_mode 已被诊断开关跳过: {mode}");
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
        let _ = win.set_focus();
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
        "[float-agent] 权限规则已更新并保存（{} 条 → {}）",
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
        "[float-agent] 测试模型 {} @ {}（key {}）",
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

/// 新增（或覆盖）一个模型。**apiKey 只落本地文件**（agent-data/models.json，
/// 已在 .gitignore），不进日志、不回传前端。
#[tauri::command]
fn models_add(
    state: State<'_, AppState>,
    id: String,
    name: String,
    url: String,
    api_key: String,
    supports_tool_call: bool,
    supports_images: bool,
    headers: Option<String>,
) -> Result<String, String> {
    let id = id.trim().to_string();
    let url = url.trim().trim_end_matches('/').to_string();
    if api_key.trim().is_empty() {
        return Err("API key 不能为空".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("URL 必须以 http:// 或 https:// 开头".into());
    }

    // 额外请求头：表单给了就用表单的；没给则**保留原有配置**（避免编辑模型时把已有的头冲掉）
    let extra_headers = match headers {
        Some(t) => parse_headers(&t)?,
        None => config::find_model(&state.data_dir, &id)
            .map(|m| m.headers)
            .unwrap_or_default(),
    };

    let m = config::ModelConfig {
        id: id.clone(),
        name: if name.trim().is_empty() { id.clone() } else { name.trim().to_string() },
        vendor: "Custom".into(),
        url,
        api_key: api_key.trim().to_string(),
        supports_tool_call,
        supports_images,
        max_input_tokens: None,
        max_output_tokens: None,
        headers: extra_headers,
    };

    config::add_model(&state.data_dir, m)?;
    eprintln!("[float-agent] 已添加模型 {id}");
    Ok(id)
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

/// 通用设置读写（失焦收起等开关）
#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> config::AgentSettings {
    config::load_settings(&state.data_dir)
}

#[tauri::command]
fn set_blur_collapse(state: State<'_, AppState>, enable: bool) -> Result<(), String> {
    let mut s = config::load_settings(&state.data_dir);
    s.blur_collapse = enable;
    config::save_settings(&state.data_dir, &s)
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
    images::thumb_data_url(&path, max_edge.unwrap_or(320))
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
    eprintln!("[float-agent] 用户要求退出");
    app.exit(0);
}

#[cfg(windows)]
mod autostart {
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "float-agent";

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

    /// `.../target/debug/float-agent.exe` → `.../target/release/float-agent.exe`（存在才返回）
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
            eprintln!("[float-agent] 开机自启已改指向 release exe（无控制台黑框）");
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
        eprintln!("[float-agent] 开机自启已{}", if enable { "开启" } else { "关闭" });
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
    active_tokens: usize,
    budget_tokens: usize,
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
                budget_tokens: 0,
                groups: Vec::new(),
            })
        }
    };
    Ok(McpStatus {
        connected: reg.connected,
        error: reg.error.clone(),
        url: reg.url(),
        active_tokens: reg.active_tokens(),
        budget_tokens: reg.budget_tokens(),
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

/// 手动重新拉取 MCP 工具清单（网关刚起来时用得上）
#[tauri::command]
async fn mcp_refresh(state: State<'_, AppState>) -> Result<(), String> {
    // 与 setup 后台任务同款：锁外网络 IO，锁内落账
    let client = state.mcp.lock().await.client_clone();
    let snap = mcp::ToolRegistry::fetch_snapshot(&client).await;
    state.mcp.lock().await.apply_snapshot(snap);
    Ok(())
}

/// 设置/更换/禁用 MCP 网关地址（写 `mcp.json`，立即生效于下次拉取）。
///
/// `url` 传空 = 显式禁用。MCP 网关是用户自己起的进程，**没有内置默认地址**，
/// 不配就不启用。
#[tauri::command]
async fn mcp_set_url(state: State<'_, AppState>, url: String) -> Result<(), String> {
    let u = url.trim().to_string();
    if u.is_empty() {
        config::save_mcp_url(&state.data_dir, None)?;
        state.mcp.lock().await.set_url("");
        eprintln!("[float-agent] MCP 已禁用");
        return Ok(());
    }
    if !u.starts_with("http://") && !u.starts_with("https://") {
        return Err("地址必须以 http:// 或 https:// 开头".into());
    }
    config::save_mcp_url(&state.data_dir, Some(&u))?;
    let mut reg = state.mcp.lock().await;
    reg.set_url(&u);
    drop(reg);
    eprintln!("[float-agent] MCP 网关已设为 {u}，重新拉取工具清单");
    // 换完顺手拉一次，用户立刻能看到成没成（网关没起来会显示连接失败）
    let client = state.mcp.lock().await.client_clone();
    let snap = mcp::ToolRegistry::fetch_snapshot(&client).await;
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
        "[float-agent] chat: 模型={} 文字={} 字 图片={} 张",
        cfg.id,
        input.chars().count(),
        imgs.len()
    );

    // 新对话开始：清掉可能残留的取消请求
    CHAT_CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);

    // 当前会话 id —— 回灌历史要用。
    // ⚠️ 时序：必须在这里取（`append_turn` 落盘**之前**），
    //    否则 agent 会把本轮问题也当成历史读回来，重复一遍。
    let session_id = sessions::current_id(&data_dir)
        .unwrap_or_else(|| sessions::ensure_current(&data_dir).id);

    // imgs 会被 move 进 agent::run，这里留一份给会话落盘
    let imgs_for_store = imgs.clone();

    // 采集一次前台上下文（用户在用 VSCode/Typora 看哪个目录）——注入本轮 system prompt。
    // 采集失败不影响对话，顶多少一段上下文。
    let fg = context::current();
    if let Some(f) = &fg {
        eprintln!("[float-agent] 前台上下文: {}", f.summary());
    }

    // 进度回调用事件推给前端 —— 否则 agent loop 的 30-60 秒是黑盒
    let progress: agent::ProgressFn = {
        let win = window.clone();
        std::sync::Arc::new(move |p: agent::Progress| {
            let _ = win.emit("agent-progress", &p);
        })
    };

    let run = agent::run(
        &cfg,
        &gate,
        &data_dir,
        &state.mcp,
        &input,
        imgs,
        progress,
        fg.as_ref(),
        &CHAT_CANCEL,
        &session_id,
    )
    .await?;

    // 对话落日记（append-only）+ 会话持久化。失败不影响主流程。
    if let Err(e) = state
        .memory
        .append_daily(&input, &run.answer, run.steps.iter().filter(|s| s.kind == "tool_call").count())
    {
        eprintln!("[float-agent] 写日记失败: {e}");
    }
    if let Err(e) = sessions::append_turn(
        &state.data_dir,
        &input,
        &imgs_for_store,
        &run.answer,
        &sessions::steps_from_agent(&run.steps),
    ) {
        eprintln!("[float-agent] 写会话失败: {e}");
    }

    Ok(run)
}

/// 「正在思考」中的取消标志。chat 每轮循环检查，置位后尽快返回"已停止"。
static CHAT_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 前端「停止」按钮：请求中断当前对话轮
#[tauri::command]
fn chat_cancel() {
    CHAT_CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[float-agent] 收到停止请求");
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
        "[float-agent] 初始化完成: 建目录 {} 个 / 建文件 {} 个 / 跳过 {} 个",
        r.created_dirs.len(),
        r.created_files.len(),
        r.skipped.len()
    );
    Ok(r)
}

/// 新建会话（旧的自动保留在列表里）
#[tauri::command]
fn session_new(state: State<'_, AppState>) -> sessions::Session {
    let s = sessions::new_session(&state.data_dir);
    eprintln!("[float-agent] 新会话 {}", s.id);
    s
}

/// 切换到指定会话
#[tauri::command]
fn session_switch(state: State<'_, AppState>, id: String) -> Result<sessions::Session, String> {
    sessions::switch(&state.data_dir, &id)
}

/// 删除会话
#[tauri::command]
fn session_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    sessions::delete(&state.data_dir, &id)
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

            eprintln!("[float-agent] agent-data 目录: {}", data_dir.display());

            // 开机自启校准：若之前的自启项指向 debug exe，release 已存在则改指 release
            #[cfg(windows)]
            autostart::sync_to_release();

            // 前台应用轮询线程（记录「用户上一个在看的应用」，对话时注入上下文）
            context::spawn_watcher();

            let gate = permission::load_gate(&data_dir, &app_dir);
            for r in gate.rules() {
                eprintln!(
                    "[float-agent] 权限规则: {} => {:?} ({})",
                    r.prefix.display(),
                    r.access,
                    r.label
                );
            }

            // 模型配置来源（便于排查）
            eprintln!(
                "[float-agent] 模型配置: {}",
                config::models_json_path(&data_dir).display()
            );

            // --- MCP 注册表（懒加载）---
            // 拉取在后台进行：网关没起来也不能拖慢应用启动。
            // 没配 mcp.json 就是 None（不启用）—— 没有默认网关地址。
            let mcp_url = config::resolve_mcp_url(&data_dir);
            let registry = match &mcp_url {
                Some(u) => {
                    eprintln!("[float-agent] MCP 网关: {u}（后台拉取工具清单中…）");
                    mcp::ToolRegistry::new(u.clone())
                }
                None => {
                    eprintln!("[float-agent] MCP 未配置（设置页里填网关地址才启用）");
                    mcp::ToolRegistry::new("")
                }
            };

            let shared = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
            if mcp_url.is_some() {
                let bg = shared.clone();
                tauri::async_runtime::spawn(async move {
                    // 锁外做网络 IO（网关僵持时最长 ~40s），锁内只落内存账。
                    // 之前持锁 refresh 会把 mcp_status/工具列表全部堵在锁上
                    // —— 表现就是「点开设置，加载中卡半天」。
                    let client = bg.lock().await.client_clone();
                    let snap = mcp::ToolRegistry::fetch_snapshot(&client).await;
                    bg.lock().await.apply_snapshot(snap);
                });
            }

            app.manage(AppState {
                gate: Mutex::new(gate),
                perm_mtime: Mutex::new(None),
                data_dir: data_dir.clone(),
                memory: memory::MemoryStore::new(data_dir),
                mcp: shared,
            });

            // --- 窗口初始形态 ---
            #[cfg(windows)]
            if let Some(win) = app.get_webview_window("orb") {
                if let Err(e) = apply_mode(&win, "orb") {
                    eprintln!("[float-agent] 初始化窗口形态失败: {e}");
                }
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
            list_models,
            get_selected_model,
            set_selected_model,
            models_add,
            models_remove,
            test_model,
            chat,
            capture_screen,
            mem_pending,
            mem_approve,
            mem_reject,
            mem_propose,
            mem_read,
            mem_files,
            get_settings,
            set_blur_collapse,
            foreground_context,
            foreground_history,
            image_thumb,
            skills_list,
            chat_cancel,
            session_state,
            boot_state,
            bootstrap_data,
            session_new,
            session_switch,
            session_delete,
            mcp_status,
            mcp_refresh,
            mcp_set_url,
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
