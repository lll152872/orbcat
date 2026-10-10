//! orbcat — Tauri 后端入口

mod agent;
/// 测试钩子 / 集成用例（**整个模块只在测试构建里编译**）
///
/// ⚠️ `#[cfg(test)]` 不能省（2026-09-30）：这个文件里每个 fn 都带 `#[test]`，
/// 而它对 `crate::llm` 的 import 只被那些测试函数用。非测试构建下整个文件
/// 只剩一行没用的 `use` → 报 `unused imports: ChatMessage and MessageContent`。
/// 加在 `mod` 上比在文件里逐条 `#[allow]` 干净：它本来就"不进产品路径"。
#[cfg(test)]
mod test_hooks;
mod bootstrap;
mod capture;
mod command_policy;
mod config;
mod context;
mod disk_assets;
mod distill;
mod fetch;
mod history;
mod images;
mod life;
mod llm;
mod mcp;
mod memes;
mod memory;
mod modes;
mod perm_request;
mod permission;
mod pmem;
mod project;
mod ptc;
mod recovery;
mod sessions;
mod shell;
mod single_instance;
mod skills;
mod tools;
mod search;

mod platform;
mod win32;
#[cfg(windows)]
mod win32desk;

/// 把 `modes.json` 的初始文本暴露给 `examples/seed_modes.rs`。
///
/// 为什么需要它：`modes.json` 正常由「初始化」写，但**已经初始化过的**数据目录
/// 不会重跑初始化 —— 升级后用户看不到这份文件，也就无从发现自己能改模式。
/// 一次性 example 补写那一次，用的必须是**同一个生成函数**（两处各写一份模板
/// 迟早不一致）。唯一的调用方是那个 example，生产路径不经过这里。
pub fn seed_modes_text_for_example() -> String {
    modes::default_file_text()
}

// ---------------------------------------------------------------------------
// CLI 冒烟口（给 `main.rs` 的 `--bgdesk-*` 用）
// ---------------------------------------------------------------------------
//
// 隐形桌面那套能力全在 Windows API 上，**单测里验不了**（无桌面环境必挂）。
// 这两个函数就是真机验证的入口，逻辑与 Tauri 命令走同一条路径
// （`win32desk::run_blocking` / `BackgroundDesk::windows`），不是另写一份。

/// `orbcat.exe --bgdesk-run "<命令行>"` —— 在隐形桌面上跑一条命令并把输出打到控制台。
///
/// 验证要点（真机跑一次就能看全）：
///   1. 输出能不能收回来（管道）
///   2. **用户屏幕上有没有闪窗口**（这是要人来确认的那一半）
///   3. 命令里若启动 GUI 程序，它落在隐形桌面上（用 `--bgdesk-windows` 查）
#[cfg(windows)]
pub fn cli_bgdesk_run(cmd: &str) {
    let (exe, _) = shell::resolve_interpreter();
    println!("== 隐形桌面执行 ==");
    println!("解释器: {exe}");
    println!("命令  : {cmd}");
    let started = std::time::Instant::now();
    match win32desk::run_blocking(&exe, cmd, None, std::time::Duration::from_secs(120)) {
        Ok(o) => {
            println!("pid   : {}", o.pid);
            println!("用时  : {:?}", started.elapsed());
            println!("超时  : {}", o.timed_out);
            println!("退出码: {:?}", o.exit_code);
            println!("--- stdout ---\n{}", o.stdout.trim_end());
            if !o.stderr.trim().is_empty() {
                println!("--- stderr ---\n{}", o.stderr.trim_end());
            }
        }
        Err(e) => {
            eprintln!("✗ 失败: {e}");
            std::process::exit(1);
        }
    }
}

/// `orbcat.exe --bgdesk-windows` —— 列出隐形桌面上的窗口（证明 GUI 程序落在那里）
#[cfg(windows)]
pub fn cli_bgdesk_windows() {
    match win32desk::shared_desk() {
        Ok(d) => {
            println!("== 隐形桌面「{}」上的窗口 ==", d.name());
            let wins = d.windows();
            if wins.is_empty() {
                println!("（空 —— 没有程序跑在那张桌面上）");
            }
            for w in wins {
                println!(
                    "  {:<44} {:>5}x{:<5} pid={} {}",
                    w.title,
                    w.width,
                    w.height,
                    w.pid,
                    if w.visible { "" } else { "(不可见)" }
                );
            }
        }
        Err(e) => {
            eprintln!("✗ 打不开隐形桌面: {e}");
            std::process::exit(1);
        }
    }
}

/// `orbcat.exe --bgdesk-shot [标题子串] [输出文件]` —— 抓隐形桌面上一个窗口的画面。
///
/// 不给标题就抓**面积最大**的那个（隐形桌面上混着一堆 IME / 工具提示的空窗口，
/// 按面积挑最稳）。抓完打印文件路径，可以直接看图确认"它到底渲染成什么样"。
#[cfg(windows)]
pub fn cli_bgdesk_shot(title: Option<String>, out: Option<String>) {
    let desk = match win32desk::shared_desk() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("✗ 打不开隐形桌面: {e}");
            std::process::exit(1);
        }
    };
    let mut wins: Vec<_> = desk
        .windows()
        .into_iter()
        .filter(|w| w.width > 0 && w.height > 0)
        .collect();
    if wins.is_empty() {
        eprintln!("✗ 隐形桌面上没有可抓的窗口（没有程序在跑？）");
        std::process::exit(1);
    }
    let want = title.unwrap_or_default();
    let target = if want.trim().is_empty() {
        wins.sort_by_key(|w| -(w.width * w.height));
        wins.remove(0)
    } else {
        match wins.into_iter().find(|w| w.title.contains(want.trim())) {
            Some(w) => w,
            None => {
                eprintln!("✗ 没有标题含「{want}」的窗口");
                std::process::exit(1);
            }
        }
    };
    println!("目标窗口: {} ({}x{}) pid={}", target.title, target.width, target.height, target.pid);
    match win32desk::capture_window(target.hwnd) {
        Ok(img) => {
            let path = out.unwrap_or_else(|| {
                std::env::temp_dir()
                    .join(format!("orbcat-bgdesk-shot-{}.png", target.pid))
                    .to_string_lossy()
                    .to_string()
            });
            match img.save(&path) {
                Ok(()) => println!("✅ 已保存 {path}（{}x{}）", img.width(), img.height()),
                Err(e) => {
                    eprintln!("✗ 保存失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("✗ 抓图失败: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
pub fn cli_bgdesk_shot(_title: Option<String>, _out: Option<String>) {
    eprintln!("隐形桌面只在 Windows 上有");
}

#[cfg(not(windows))]
pub fn cli_bgdesk_run(_cmd: &str) {
    eprintln!("隐形桌面只在 Windows 上有");
}

#[cfg(not(windows))]
pub fn cli_bgdesk_windows() {
    eprintln!("隐形桌面只在 Windows 上有");
}

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use permission::{Access, GrantTier, PermissionGate};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State};

// ---------------------------------------------------------------------------
// 窗口布局常量（逻辑像素）
// ---------------------------------------------------------------------------

/// 胶囊态边长
const ORB_LOGICAL: f64 = 56.0;
/// 面板态尺寸
///
/// ⚠️ 宽度从 420 → 460（2026-10 改）。原因：输入行挤了三颗状态 chip
/// （后台执行 / Agent 模式 / 执行权限）后，420 逻辑像素下输入框只剩约 373px，
/// 连 placeholder 那句话都放不下一行（用户截图报"太挤"）。
///
/// 460 是"够用且不过分"的折中：@1.5x = 690px，仍只占 1707px 屏宽的 40%，
/// 但输入框能回到约 430px。**没有做成可拖拽**（用户选了简单直接那条）。
const PANEL_W_LOGICAL: f64 = 460.0;
const PANEL_H_LOGICAL: f64 = 600.0;
/// 右键菜单态尺寸（只够放一个小菜单卡片）
const MENU_W_LOGICAL: f64 = 196.0;
/// 右键菜单高度 —— 设置 / 记忆 / 对话 / 睡眠 / 退出（5 项）
const MENU_H_LOGICAL: f64 = 214.0;
/// 距屏幕边缘留白
const MARGIN_LOGICAL: f64 = 24.0;
/// 胶囊在屏幕高度的这个比例处
const ORB_Y_RATIO: f64 = 0.40;

/// orb 窗口在 tauri.conf.json 里配的 WebView2 浏览器参数。
///
/// ⚠️ **必须与 `tauri.conf.json` 的 `additionalBrowserArgs` 保持一致**：
/// 这台机器的 WebView2 要靠禁用 GPU 合成才能建起 webview。运行时用
/// `WebviewWindowBuilder` 新建的窗口（models）如果漏了这串参数，
/// `build()` 会返回 `Ok`，但底层 webview 根本没起来 —— 之后任何 getter
/// 都报 `failed to receive message from webview`，页面也永远不加载
/// （2026-09-29 实测：日志里 build Ok 但没有任何 page_load 记录）。
const ORB_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --force-renderer-accessibility --disable-gpu-compositing --disable-gpu-rasterization --disable-gpu-vsync --disable-frame-rate-limit";

/// 「模型管理」窗口的预热延迟（毫秒）。
///
/// 取值依据：orb 的 webview 起来 + 前端 boot + MCP 首拉大约 1~2s；
/// 给到 3s 是"确保 orb 那套彻底稳定"与"用户可能马上就点"之间的折中 ——
/// 预热本身耗时约 1~8s，太晚预热就失去意义（用户已经点了）。
///
/// 用环境变量 `ORBCAT_MODELS_PREHEAT_MS` 可覆盖，见 [`spawn_models_preheat`]。
const MODELS_PREHEAT_DELAY_MS: u64 = 3_000;

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
        // ⚠️ 整表替换会把「激活项目目录」的动态规则一起冲掉，跟着重建。
        self.refresh_project_dir_rules();
    }

    /// 重建「激活项目目录」动态授权（用户 2026-10-07 定案）：
    /// **新建项目时绑定的目录，项目激活期间默认读写；没绑目录就没有。**
    ///
    /// 规则形态：label = `项目目录「名」`，权限 ReadWrite，**只活内存**
    /// （三条硬约束见 [`permission::PROJECT_DIR_LABEL_PREFIX`]：不落盘、
    /// 不进设置页列表、由本函数在时机点重建）。
    ///
    /// 调用点：启动、切项目、建/删/改名项目、改绑定路径、permissions.json
    /// 热加载之后。全部幂等 —— 先摘全部旧动态规则，再按当前激活项目挂新的。
    fn refresh_project_dir_rules(&self) {
        let want = active_project_dir(&self.data_dir);
        let mut gate = self.gate.lock().unwrap();
        let removed = gate.retain_rules(|r| !permission::is_dynamic_label(&r.label));
        if let Some((path, name)) = want {
            gate.add_rule(
                &path,
                permission::Access::ReadWrite,
                format!("{}{}」", permission::PROJECT_DIR_LABEL_PREFIX, name),
            );
            eprintln!(
                "[orbcat] 项目目录授权: {} => readwrite（项目「{name}」）",
                path.display()
            );
        }
        if removed > 0 {
            eprintln!("[orbcat] 已摘除 {removed} 条旧的项目目录授权");
        }
    }
}

/// 当前激活项目绑定的目录 → `(规范路径, 项目名)`。
///
/// 三个前置（任一不满足 = `None` = 没有额外授权，fail-closed）：
/// 项目存在、绑定了非空路径、该路径**真实存在且是目录** —— 绑一个
/// 不存在的路径不该产生授权（与项目记忆注入的「源路径已死不注入」同语义）。
fn active_project_dir(data_dir: &Path) -> Option<(PathBuf, String)> {
    let s = config::load_settings(data_dir);
    let pid = s.active_project_id?;
    let all = pmem::list_projects(data_dir).ok()?;
    let p = all.iter().find(|x| x.id == pid)?;
    let rp = p.root_path.trim();
    if rp.is_empty() || !Path::new(rp).is_dir() {
        return None;
    }
    Some((permission::best_effort_canonicalize(Path::new(rp)), p.name.clone()))
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
    // 动态规则（激活项目目录授权）不进设置页列表：它由项目切换自动重建，
    // 用户在设置页删不掉，摆出来只会让人困惑；项目面板里看绑定路径更直观。
    Ok(gate
        .export_rules()
        .into_iter()
        .filter(|r| !permission::is_dynamic_label(&r.label))
        .collect())
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

/// 列出可用模型（**不含 apiKey**）。
///
/// 顺带做一次性迁移：`settings.selected_model` 存的若已不是任何接口 id
/// （2026-10-02 起身份 = (Base URL, 接口 id)，旧显示名全部失效）→ 清空，
/// 让前端兜底自动选第一个。
#[tauri::command]
fn list_models(state: State<'_, AppState>) -> Result<Vec<config::ModelView>, String> {
    let models = config::load_models(&state.data_dir)?;
    let mut s = config::load_settings(&state.data_dir);
    if let Some(sel) = s.selected_model.clone() {
        if !models.iter().any(|m| m.id == sel) {
            s.selected_model = None;
            s.selected_model_url = None;
            config::save_settings(&state.data_dir, &s)?;
        }
    }
    Ok(models.iter().map(config::ModelView::from).collect())
}

/// 当前选中的模型（身份 = (Base URL, 接口 id)）。
///
/// `url` 为 None 表示旧数据只存了 id —— 前端按裸 id 匹配即可。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectedModelView {
    id: String,
    url: Option<String>,
}

#[tauri::command]
fn get_selected_model(state: State<'_, AppState>) -> Result<Option<SelectedModelView>, String> {
    let s = config::load_settings(&state.data_dir);
    Ok(s.selected_model.map(|id| SelectedModelView {
        id,
        url: s.selected_model_url.clone(),
    }))
}

/// 按 (url, id) 定位模型：url 有值先精确匹配，失配回落裸 id 兜底。
fn locate_model(
    state: &State<'_, AppState>,
    id: &str,
    url: &Option<String>,
) -> Result<config::ModelConfig, String> {
    match url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => config::find_model_by_url_id(&state.data_dir, u, id)
            .or_else(|_| config::find_model(&state.data_dir, id)),
        None => config::find_model(&state.data_dir, id),
    }
}

#[tauri::command]
fn set_selected_model(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    url: Option<String>,
) -> Result<(), String> {
    // 先确认这个 (url, id) 真的存在（url 失配回落裸 id 校验）
    let resolved = locate_model(&state, &id, &url)?;
    let mut s = config::load_settings(&state.data_dir);
    s.selected_model = Some(resolved.id);
    s.selected_model_url = Some(resolved.url);
    config::save_settings(&state.data_dir, &s)?;
    // 在模型管理窗口里切了「当前模型」→ 面板顶部的模型卡片也要跟上
    let _ = app.emit("models-changed", ());
    Ok(())
}

/// 连通性自检：确认 key / url / 模型名都对
#[tauri::command]
async fn test_model(
    state: State<'_, AppState>,
    id: String,
    url: Option<String>,
) -> Result<String, String> {
    let cfg = locate_model(&state, &id, &url)?;
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

/// 新增（或覆盖）一个模型。**身份 = (Base URL, 接口 id)**。
///
/// - 同组同接口 id = 更新这一条；跨组同名（glm-5.3-flash 进两个火山组）完全合法
/// - Key 可空（Ollama / 本地）；**留空时自动继承**：同组已有 Key → 直接用
#[tauri::command]
fn models_add(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    url: String,
    api_key: Option<String>,
    supports_tool_call: bool,
    supports_images: bool,
    headers: Option<String>,
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    overwrite: Option<bool>,
) -> Result<String, String> {
    let _ = overwrite; // 同组同 id 即更新，不再需要「覆盖」确认
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err("接口模型 ID 不能为空".into());
    }
    let url = config::normalize_base_url(&url)?;

    // 按 (url, id) 找旧配置（保留 vendor / max / key 兜底）
    let existing = config::load_models(&state.data_dir)
        .ok()
        .and_then(|all| {
            let want = config::url_key(&url);
            all.into_iter()
                .find(|m| m.id == id && config::url_key(&m.url) == want)
        });

    let key_in = api_key.unwrap_or_default().trim().to_string();
    // Key 留空时的继承顺序：本条旧 Key → 同组（同 Base URL）已有 Key。
    // 用户心智"一个提供商一个 Key"：组里加模型不该要求再粘一遍 Key。
    let api_key = if !key_in.is_empty() {
        key_in
    } else if existing
        .as_ref()
        .is_some_and(|m| !m.api_key.trim().is_empty())
    {
        existing.as_ref().unwrap().api_key.clone()
    } else {
        let all = config::load_models(&state.data_dir)?;
        config::group_key_for_url(&all, &url)
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
        id: id.clone(),
        vendor,
        url: url.clone(),
        api_key,
        supports_tool_call,
        supports_images,
        max_input_tokens: max_in,
        max_output_tokens: max_out,
        headers: extra_headers,
    };

    config::add_model(&state.data_dir, m)?;
    eprintln!("[orbcat] 已保存模型（id={id} @ {url}）");
    // 模型列表变了 → 面板顶部的模型卡片要跟着刷（独立窗口里改的，面板看不到 DOM）
    let _ = app.emit("models-changed", ());
    Ok(id)
}

fn existing_vendor_or_custom(existing: &Option<config::ModelConfig>) -> String {
    match existing {
        Some(m) if !m.vendor.trim().is_empty() => m.vendor.trim().to_string(),
        _ => "Custom".into(),
    }
}

/// 编辑已有模型（按 **(Base URL, 接口 id)** 定位）。
///
/// - `url`：**定位用**的旧组 Base URL（前端从被编辑那条带上）；None 回落裸 id
/// - `new_url`：表单里填的**新** Base URL —— 改了就是换组（跨组移动这条模型）
/// - `new_id`：改发给提供商的 `model` 字段；**组内查重**（跨组同名合法）
/// - `apiKey` 空 = 保持原 Key；`clear_key` = 清空
///
/// ⚠️ `url` 与 `new_url` 必须分开（2026-10-09 修）。它们以前是**同一个参数**
/// `url` 既定位又当新值传下去，而前端只发了旧值 → 表单里的 Base URL 压根没上线，
/// 改完保存**静默丢弃**，用户看不到任何提示。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn models_edit(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    new_id: Option<String>,
    url: Option<String>,
    new_url: Option<String>,
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

    let target = locate_model(&state, &key, &url)?;
    let old_id = target.id.clone();
    let old_url = target.url.clone();
    let nid_trimmed = new_id
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // 新 Base URL：空 / 与旧值等价（url_key 归一）都当"没改"
    let url_changed = match new_url.as_deref().map(str::trim) {
        Some(nu) if !nu.is_empty() && config::url_key(nu) != config::url_key(&old_url) => {
            Some(nu.to_string())
        }
        _ => None,
    };

    config::update_model(
        &state.data_dir,
        &old_url,
        &old_id,
        url_changed.clone(),
        api_key,
        clear_key.unwrap_or(false),
        supports_tool_call,
        supports_images,
        extra_headers,
        Some(max_input_tokens),
        Some(max_output_tokens),
        nid_trimmed.clone(),
    )?;

    // 当前选中项就是被编辑的这条 → id / url 任一变了都要同步，否则"当前模型"标记掉没
    if nid_trimmed.is_some() || url_changed.is_some() {
        let mut s = config::load_settings(&state.data_dir);
        let sel_matches = s.selected_model.as_deref() == Some(old_id.as_str())
            && s
                .selected_model_url
                .as_deref()
                .map(|u| config::url_key(u) == config::url_key(&old_url))
                .unwrap_or(true);
        if sel_matches {
            if let Some(nid) = nid_trimmed {
                s.selected_model = Some(nid);
            }
            if let Some(nu) = url_changed {
                s.selected_model_url = Some(nu);
            }
            config::save_settings(&state.data_dir, &s)?;
        }
    }

    eprintln!("[orbcat] 已编辑模型（{old_id} @ {old_url}）");
    let _ = app.emit("models-changed", ());
    Ok(())
}

/// 编辑表单回填（**不含完整 Key**，只有掩码预览）
#[tauri::command]
fn models_get_edit(
    state: State<'_, AppState>,
    id: String,
    url: Option<String>,
) -> Result<config::ModelEditView, String> {
    let m = locate_model(&state, id.trim(), &url)?;
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
    url: Option<String>,
) -> Result<Vec<llm::RemoteModelInfo>, String> {
    let cfg = locate_model(&state, id.trim(), &url)?;
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
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    ids: Vec<String>,
    source_id: Option<String>,
    source_url: Option<String>,
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
        // 按 (url, id) 精确找来源（同 id 可跨组共存）；url 缺失回落裸 id
        match source_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(su) => config::find_model_by_url_id(&state.data_dir, su, sid.trim())
                .or_else(|_| config::find_model(&state.data_dir, sid.trim()))?,
            None => config::find_model(&state.data_dir, sid.trim())?,
        }
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
    if !added.is_empty() {
        let _ = app.emit("models-changed", ());
    }
    Ok(ImportModelsResult { added, skipped })
}
/// 删除一个模型（按 **(Base URL, 接口 id)** 定位）。
/// 若删的是当前选中项，顺带清除选中状态。
#[tauri::command]
fn models_remove(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    url: Option<String>,
) -> Result<(), String> {
    let target = locate_model(&state, &id, &url)?;
    config::remove_model(&state.data_dir, &target.url, &target.id)?;

    let mut s = config::load_settings(&state.data_dir);
    let sel_matches = s.selected_model.as_deref() == Some(target.id.as_str())
        && s.selected_model_url
            .as_deref()
            .map(|u| config::url_key(u) == config::url_key(&target.url))
            .unwrap_or(true);
    if sel_matches {
        s.selected_model = None;
        s.selected_model_url = None;
        config::save_settings(&state.data_dir, &s)?;
    }
    let _ = app.emit("models-changed", ());
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

// ---------------------------------------------------------------------------
// Tauri 命令 —— 数据源（认知面注入，见 life.rs）
// ---------------------------------------------------------------------------

/// 一个数据源给前端看的形态：声明 + **实时读一次的结果**。
///
/// 为什么把"读一次"和声明一起返回：设置页要显示每个源的健康状况
/// （几条、多久没更新、读没读到）。分两次命令会让前端要自己对时序，
/// 而这里一次读完（源数量是个位数，成本可接受）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LifeSourceView {
    id: String,
    label: String,
    enabled: bool,
    kind: String,
    /// 文件路径或命令（原样给前端展示与编辑）
    target: String,
    items_path: String,
    stale_hours: Option<f64>,
    /// 配置是否完整（缺 path/command 的会在 UI 标出来）
    complete: bool,
    /// 本次读取结果
    ok: bool,
    error: String,
    updated_at: String,
    age_hours: Option<f64>,
    stale: bool,
    count: usize,
    /// 明细预览（前几条标题，让用户一眼确认接对了）
    preview: Vec<String>,
}

#[tauri::command]
async fn life_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let f = life::load(&state.data_dir);
    let cwd = std::env::current_dir().unwrap_or_else(|_| state.data_dir.clone());
    let mut views: Vec<LifeSourceView> = Vec::new();

    for s in &f.sources {
        // 未启用的源**不读**（尤其 command 类：那等于偷偷跑命令）。
        // 前端仍要看到它，所以给一份"未读取"的占位。
        let snap = if s.enabled {
            life::read_source(s, &cwd).await
        } else {
            life::Snapshot {
                id: s.id.clone(),
                label: s.name().to_string(),
                ok: false,
                error: String::new(), // 空 = "没读"（前端据此显示"已关闭"而不是"失败"）
                updated_at: String::new(),
                age_hours: None,
                stale: false,
                items: Vec::new(),
            }
        };
        views.push(LifeSourceView {
            id: s.id.clone(),
            label: s.name().to_string(),
            enabled: s.enabled,
            kind: match s.kind {
                life::Kind::File => "file".into(),
                life::Kind::Command => "command".into(),
            },
            target: match s.kind {
                life::Kind::File => s.path.clone(),
                life::Kind::Command => s.command.clone(),
            },
            items_path: s.items.clone(),
            stale_hours: s.stale_hours,
            complete: s.is_complete(),
            ok: snap.ok,
            error: snap.error,
            updated_at: snap.updated_at,
            age_hours: snap.age_hours,
            stale: snap.stale,
            count: snap.items.len(),
            preview: snap
                .items
                .iter()
                .take(5)
                .map(|i| {
                    if i.group.is_empty() {
                        i.title.clone()
                    } else {
                        format!("[{}] {}", i.group, i.title)
                    }
                })
                .collect(),
        });
    }

    Ok(serde_json::json!({
        "path": life::life_path(&state.data_dir).display().to_string(),
        "sources": views,
    }))
}

/// 切换一个数据源的启用状态（落盘）。
///
/// 为什么单独一条命令而不是让前端整份覆写：整份覆写要求前端把
/// `map` / `items` 等字段原样带回来，任何一个字段丢了都是**静默损坏**。
/// 这里只改一个布尔，其余字段保持磁盘上的原样。
#[tauri::command]
fn life_set_enabled(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let mut f = life::load(&state.data_dir);
    let Some(s) = f.sources.iter_mut().find(|s| s.id == id.trim()) else {
        return Err(format!("没有 id 为「{id}」的数据源"));
    };
    if enabled && !s.is_complete() {
        return Err(format!(
            "「{}」的配置不完整（缺 {}），补全后再打开。",
            s.name(),
            match s.kind {
                life::Kind::File => "path",
                life::Kind::Command => "command",
            }
        ));
    }
    s.enabled = enabled;
    life::save(&state.data_dir, &f)?;
    // 立刻失效摘要缓存，否则下次构建 prompt 还会用旧摘要
    life::invalidate_cache();
    eprintln!("[orbcat] 数据源「{id}」→ {}", if enabled { "启用" } else { "关闭" });
    Ok(())
}

/// 重新读一遍全部源（设置页的「刷新」按钮）。
#[tauri::command]
fn life_refresh(state: State<'_, AppState>) -> Result<(), String> {
    life::invalidate_cache();
    let _ = state;
    Ok(())
}

/// 在文件管理器里打开 `life.json`（用户要自己加源时）。
#[tauri::command]
fn life_config_path(state: State<'_, AppState>) -> String {
    life::life_path(&state.data_dir).display().to_string()
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

// ---------------------------------------------------------------------------
// macOS 前台上下文：渐进式授权
// ---------------------------------------------------------------------------

/// 前台上下文当前能采到哪一级。
///
/// 前端据此决定要不要显示「让它读我的窗口」这类授权引导。
/// Windows 上恒为 `full`（`GetForegroundWindow` 不需要任何授权），
/// 所以这段逻辑在 Windows 上是死代码，不会被触发。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextCapabilityInfo {
    level: &'static str,
    /// 人类可读的一句说明，直接用于 UI 文案
    detail: String,
    /// 是否可以主动弹窗请求授权（Windows 恒为 false）
    can_request: bool,
    /// 授权后是否还需要用户去系统设置手动勾选（macOS 的辅助功能是）
    needs_system_settings: bool,
}

#[tauri::command]
fn context_capability() -> ContextCapabilityInfo {
    #[cfg(target_os = "macos")]
    {
        use crate::platform::{context_capability as cap, ContextCapability};
        return match cap() {
            ContextCapability::Full => ContextCapabilityInfo {
                level: "full",
                detail: "已获辅助功能权限，能读到当前窗口的文件名".into(),
                can_request: false,
                needs_system_settings: false,
            },
            ContextCapability::AppOnly => ContextCapabilityInfo {
                level: "appOnly",
                detail: "只能读到你在用哪个 App。授权后我能知道你正在编辑哪个文件".into(),
                can_request: true,
                needs_system_settings: true,
            },
            ContextCapability::Unavailable => ContextCapabilityInfo {
                level: "unavailable",
                detail: "拿不到前台信息".into(),
                can_request: false,
                needs_system_settings: false,
            },
        };
    }
    #[cfg(not(target_os = "macos"))]
    ContextCapabilityInfo {
        level: "full",
        detail: "Windows 上无需任何系统权限即可读取前台窗口".into(),
        can_request: false,
        needs_system_settings: false,
    }
}

/// 请求辅助功能授权（**仅 macOS 有效**，会弹系统授权框）。
///
/// ⚠️ **只应在用户主动点击「让它自己看」时调用**，不要在启动时自动调。
/// 渐进解锁的完整流程见 `platform/macos.rs` 的 [`ensure_accessibility`]。
///
/// 返回值刻意区分「点了允许」和「真的生效了」：
/// macOS 的辅助功能授权分两步 —— 先弹窗点允许，**再去系统设置里勾选 orbcat**。
/// 所以这里返回的是「是否已生效」，前端应当继续轮询 [`context_capability`]
/// 直到 `level == "full"`，或用户主动放弃。
#[tauri::command]
fn request_context_permission() -> Result<ContextCapabilityInfo, String> {
    #[cfg(target_os = "macos")]
    {
        crate::platform::macos::ensure_accessibility()?;
        return Ok(context_capability());
    }
    #[cfg(not(target_os = "macos"))]
    Err("当前平台不需要授权即可读取前台窗口".to_string())
}

/// 打开「系统设置 → 隐私与安全性 → 辅助功能」。
///
/// 用户点了「允许」但 `context_capability()` 仍不是 `full` 时，多半是忘了手动勾选，
/// 这个命令把他直接送到那个面板。
#[tauri::command]
fn open_accessibility_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return crate::platform::macos::open_accessibility_settings();
    #[cfg(not(target_os = "macos"))]
    Err("当前平台没有该设置项".to_string())
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
// Tauri 命令 —— 拼装项目（project.rs）
// ---------------------------------------------------------------------------

/// 项目包列表 + 当前激活项。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectListView {
    /// 全部 bundle（含**无效**的 —— 设置页要标黄让用户能修）
    bundles: Vec<project::BundleInfo>,
    /// 当前激活的 id（`None` = 无项目）
    active: Option<String>,
    /// 当前项目的引用解析失败项（`(类别, id, 原因)`，设置页标黄）
    unresolved: Vec<(String, String, String)>,
    /// 当前项目的 permPreset 被丢弃的原因（fail-closed；`None` = 没丢）
    perm_error: Option<String>,
    /// 当前项目声明的脚本工具（设置页展示）
    tools: Vec<serde_json::Value>,
}

#[tauri::command]
fn project_list(state: State<'_, AppState>) -> ProjectListView {
    let active_id = config::load_settings(&state.data_dir).active_bundle;
    let bundles = project::list_bundles(&state.data_dir, active_id.as_deref());
    let active = project::active(&state.data_dir);
    ProjectListView {
        bundles,
        active: active_id,
        unresolved: project::unresolved_refs(&state.data_dir),
        perm_error: active.as_ref().and_then(|a| a.perm_error.clone()),
        tools: active
            .as_ref()
            .map(|a| {
                a.bundle
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "template": t.command_template,
                            "perm": t.perm,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// 激活 / 退出一个拼装项目（`id = null` → 退出项目）。
///
/// ⚠️ **只改 `settings.json`，不重启、不重连 MCP**：项目影响的是
/// **新轮次**的 prompt / 工具面 / 权限判定，这些都在每轮现算
/// （`tool_specs` / `effective_policy` / `effective_exec_trust` 都是读盘函数）。
/// 所以切项目是**即时生效、零重启**的。
#[tauri::command]
fn project_activate(
    state: State<'_, AppState>,
    id: Option<String>,
) -> Result<(), String> {
    let id = id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    // 激活前先校验：坏包不该被设成激活项（否则用户会看到一个
    // "激活了但什么也没生效"的诡异状态，且日志里才有原因）。
    if let Some(ref want) = id {
        project::load_bundle(&state.data_dir, want)
            .map_err(|e| format!("项目「{want}」装配失败，未激活：{e}"))?;
    }

    let mut s = config::load_settings(&state.data_dir);
    s.active_bundle = id.clone();
    config::save_settings(&state.data_dir, &s)?;

    match id {
        Some(i) => eprintln!("[orbcat] 已激活项目: {i}"),
        None => eprintln!("[orbcat] 已退出项目（回到无项目）"),
    }
    Ok(())
}

/// 保存一个项目包（设置页编辑 / 导入）。
///
/// `body` 是 `project.json` 的**文本**，在这里解析 + 校验后再落盘 ——
/// 不合法就拒绝写入（避免把坏文件留在磁盘上，下次启动才发现）。
#[tauri::command]
fn project_save(
    state: State<'_, AppState>,
    id: String,
    body: String,
) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("项目 id 不能为空".into());
    }
    // id 会当目录名用 —— 挡掉路径分隔符与上跳
    if id.contains(['/', '\\']) || id == "." || id == ".." {
        return Err(format!("项目 id「{id}」不合法（不能含路径分隔符）"));
    }

    // 先解析校验（不落盘），再写 —— 顺序不能反
    let bundle: project::ProjectBundle = serde_json::from_str(&body)
        .map_err(|e| format!("不是合法的项目包 JSON: {e}"))?;
    if bundle.schema_version != project::SCHEMA_VERSION {
        return Err(format!(
            "schemaVersion={} 不认识（本版本支持 {}）",
            bundle.schema_version,
            project::SCHEMA_VERSION
        ));
    }

    let dir = project::bundle_dir(&state.data_dir, id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建项目目录失败: {e}"))?;
    let path = dir.join(project::BUNDLE_FILE);
    std::fs::write(&path, &body).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;

    // 写回后立刻复验一次 —— 校验器与写盘必须一致，
    // 否则会出现"保存成功但激活时才报错"的割裂体验。
    project::load_bundle(&state.data_dir, id)
        .map_err(|e| format!("已写入但复验失败（请检查文件）: {e}"))?;
    eprintln!("[orbcat] 项目包已保存: {}", path.display());
    Ok(())
}

/// 删除一个项目包（**只删 `project.json`**，保留 MEMORY.md 等项目记忆）。
///
/// 为什么不整个目录删：目录里可能还有用户的项目记忆与脚本，
/// 那些不是 bundle 的产物，不该被"退出项目"顺手带走。
#[tauri::command]
fn project_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let id = id.trim();
    let path = project::bundle_dir(&state.data_dir, id).join(project::BUNDLE_FILE);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))?;
    }
    // 删的正好是激活项 → 顺手退出，避免留下一个指向不存在包的 active_bundle
    let mut s = config::load_settings(&state.data_dir);
    if s.active_bundle.as_deref() == Some(id) {
        s.active_bundle = None;
        config::save_settings(&state.data_dir, &s)?;
    }
    Ok(())
}

/// 读一个项目包的原始文本（设置页编辑用）
#[tauri::command]
fn project_read(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let path = project::bundle_dir(&state.data_dir, id.trim()).join(project::BUNDLE_FILE);
    std::fs::read_to_string(&path).map_err(|e| format!("读不到 {}: {e}", path.display()))
}

/// 搜索是否可用（`web_search` 工具是否暴露给模型）。
///
/// 抽成一个函数是因为有**两个**判据要一致地看：
/// 1. `search.json` 里配了后端且有 key（既有逻辑）
/// 2. 拼装项目的 `slots.search` 若声明了，必须**就是**当前配置的那个后端
///
/// 第 2 条是"槽位"语义：项目说"我要用 tavily"，而用户配的是 exa →
/// 该项目下**不暴露** `web_search`（而不是偷偷用 exa）。这与
/// `slot_model` 的 fail-open 不同 —— 模型可以回落到用户选的，
/// 但搜索后端回落到"另一个服务商"意味着**把查询发给用户没预期的服务商**，
/// 那是隐私问题，所以这里选择不暴露。
pub(crate) fn web_search_ready_for(data_dir: &std::path::Path) -> bool {
    if !search::is_ready(data_dir) {
        return false;
    }
    match project::active(data_dir).and_then(|a| a.bundle.slots.search) {
        Some(want) => {
            let want = want.trim().to_ascii_lowercase();
            if want.is_empty() {
                return true;
            }
            match search::load_config(data_dir) {
                Ok(cfg) => cfg.provider_norm() == want,
                Err(_) => false,
            }
        }
        None => true,
    }
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

/// 把「当前模式允许的组」算成注册表要的形式（**唯一**的入口）。
///
/// 为什么收成一个函数：`apply_snapshot_with_settings` 有 4 个调用点
/// （启动后台拉取、保存 server、改 URL、手动刷新、切模式），
/// 每个点各写一遍"读 modes.json + 展开哨兵"，迟早有一处忘掉 ——
/// 而漏掉的那一处会变成"切了模式但工具没变"这种最难查的 bug。
fn allowed_for_dir(state: &AppState) -> std::collections::HashSet<String> {
    allowed_for_dir_of(&state.data_dir, &state.mcp)
}

/// 同 [`allowed_for_dir`]，但接收裸参数（启动期还没有 `State` 时用）
///
/// ⚠️ 2026-10-08：模式**跟会话走**了，所以这里不能再读 `settings.activeMode`
/// —— 那现在是"新建会话的默认模式"，跟正在用的这个会话没关系。
/// 改成问 `sessions::effective_mode`（当前会话 → 它自带的模式）。
/// 漏改的表现：切到「闲聊」后重新拉一次 MCP 快照，工具组又满血复活。
fn allowed_for_dir_of(
    data_dir: &std::path::Path,
    mcp: &std::sync::Arc<tokio::sync::Mutex<mcp::ToolRegistry>>,
) -> std::collections::HashSet<String> {
    // 组名清单只能从注册表拿（modes.json 里写的是名字，不知道 MCP 当前有什么组）。
    // `try_lock`：所有调用点都是"锁外算好、锁内套用"，所以这里正常能拿到锁；
    // 万一拿不到也不 `.await` —— 那会在这条路径上引出一个死锁可能
    // （调用方可能正持锁）。退化结果 = 白名单里的组暂时全被当成未知，
    // 下次快照即恢复，比死锁好得多。
    let available: Vec<String> = match mcp.try_lock() {
        Ok(reg) => reg.groups().iter().map(|g| g.name.clone()).collect(),
        Err(_) => Vec::new(),
    };
    // ⚠️ 用 `current_or_main` 而不是 `current_id`：后者只看指针文件，
    //    指针缺失（真实发生过：`sessions/current` 不存在）会返回 None →
    //    兜到内置默认模式，而界面/agent 走 `ensure_current` 落到的是另一条会话
    //    → "界面在闲聊会话里、MCP 工具组却按标准模式算"。两边都说得通，极难排查。
    //    也刻意不用 `ensure_current`：那条路径会**写盘**（自愈/落指针），
    //    而这里跑在后台线程里，不该有副作用。
    let cur = crate::sessions::current_or_main(data_dir);
    let mode = crate::sessions::effective_mode(data_dir, Some(&cur.id));
    modes::allowed_groups_for(data_dir, Some(&mode.id), &available).as_registry_set()
}

/// 换了会话 → 它自带模式 → **MCP 活跃组必须跟着重算**（2026-10-08）。
///
/// 为什么非得显式做一次：`allowed_for_dir` 只在"新快照 / 改设置 / 手动刷新"三条
/// 路径上跑，切会话一条都不触发。少了这一步，从「标准」切到「闲聊」后工具组
/// 还是满的 —— 表现出来就是"切了闲聊它照样能调 MCP"，而工具表里
/// `send_meme` 之类按模式走的闸门又是对的，两处不一致最难查。
///
/// 与 `mcp_set_all_groups` / `resync_mode_groups` 用的是同一套（`allowed_for_dir` +
/// `restore_active_from_settings`），所以"模式只能收窄、用户关掉的组开不回来"
/// 这条语义自动成立。
async fn resync_mode_groups(state: &AppState) -> usize {
    let allowed = allowed_for_dir(state);
    let s = config::load_settings(&state.data_dir);
    let mut reg = state.mcp.lock().await;
    reg.restore_active_from_settings(&s, Some(allowed))
}

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
    let settings = config::load_settings(&state.data_dir);
    let allowed = allowed_for_dir(&state);
    state
        .mcp
        .lock()
        .await
        .apply_snapshot_with_settings(snap, &settings, Some(allowed));
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
    // 规整：id 空则生成；url / command 去空白；args / env 去空条目
    let mut cleaned: Vec<config::McpServerCfg> = Vec::new();
    for (i, mut s) in servers.into_iter().enumerate() {
        s.url = s.url.trim().to_string();
        s.command = s.command.trim().to_string();
        // args 里的空白项没有意义（空参数在多数 CLI 里是错误来源），就地剔除
        s.args = s
            .args
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        // env 的 key 去空白；key 空的条目直接丢（没法 setenv）
        s.env = s
            .env
            .into_iter()
            .filter_map(|(k, v)| {
                let k = k.trim().to_string();
                if k.is_empty() {
                    None
                } else {
                    Some((k, v))
                }
            })
            .collect();
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
    let settings = config::load_settings(&state.data_dir);
    let allowed = allowed_for_dir(&state);
    state
        .mcp
        .lock()
        .await
        .apply_snapshot_with_settings(snap, &settings, Some(allowed));
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
    let settings = config::load_settings(&state.data_dir);
    let allowed = allowed_for_dir(&state);
    state
        .mcp
        .lock()
        .await
        .apply_snapshot_with_settings(snap, &settings, Some(allowed));
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

/// **切换一个 MCP 组的启用状态并落盘**（决策：默认全给 + 开关有记忆）。
///
/// 为什么要落盘：以前组开关只活内存，重启后全部回到"未加载"，
/// 用户每次开机都得手动再点一遍。现在写进 `settings.json`，
/// `apply_snapshot_with_settings` 会在每次拉取后自动恢复。
#[tauri::command]
async fn mcp_set_group_enabled(
    state: State<'_, AppState>,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut reg = state.mcp.lock().await;
        if enabled {
            reg.load(&name)?;
        } else {
            reg.unload(&name)?;
        }
    }
    let mut s = config::load_settings(&state.data_dir);
    s.mcp_group_enabled.insert(name, enabled);
    config::save_settings(&state.data_dir, &s)
}

/// 读 MCP 组开关记忆 + 全局「默认全给」开关（MCP 设置页渲染用）
#[tauri::command]
fn mcp_group_settings(
    state: State<'_, AppState>,
) -> std::collections::BTreeMap<String, bool> {
    let s = config::load_settings(&state.data_dir);
    let mut out = s.mcp_group_enabled;
    // 用保留键把全局开关也带出去（组名不会叫这个）
    out.insert("__all__".into(), s.mcp_all_groups);
    out
}

/// 读「后台执行」开关状态 + 当前隐形桌面上的窗口（前端展示"它现在在干什么"）。
#[cfg(windows)]
#[tauri::command]
fn bg_desk_status() -> serde_json::Value {
    let on = win32desk::is_enabled();
    // 没开就不去建桌面（`shared_desk` 有惰性创建，这里不能白建一张）
    let (ok, err, desk_name, wins) = if on {
        match win32desk::shared_desk() {
            Ok(d) => (
                true,
                None,
                d.name().to_string(),
                d.windows()
                    .into_iter()
                    .map(|w| {
                        serde_json::json!({
                            "title": w.title,
                            "width": w.width,
                            "height": w.height,
                            "pid": w.pid,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
            Err(e) => (false, Some(e), String::new(), Vec::new()),
        }
    } else {
        (true, None, String::new(), Vec::new())
    };
    serde_json::json!({
        "enabled": on,
        "available": ok,
        "error": err,
        "deskName": desk_name,
        "windows": wins,
    })
}

/// 开关「后台执行」（命令跑在隐形桌面上，不闪窗口、不抢焦点）。
///
/// ⚠️ **进程级、不落盘**：这是一次性会话开关。落盘会让人下次开机莫名其妙
/// "命令都看不见了"，还得回来找这个开关 —— 那种惊讶不值得省一次点击。
#[cfg(windows)]
#[tauri::command]
fn bg_desk_set(enabled: bool) -> Result<bool, String> {
    if enabled {
        // 开之前先确认真的建得出来 —— 建不出来就立刻报错，
        // 别等用户下一条命令跑的时候才发现"后台模式没生效"
        win32desk::shared_desk()?;
    }
    win32desk::set_enabled(enabled);
    eprintln!("[orbcat] 后台执行（隐形桌面）: {}", if enabled { "开" } else { "关" });
    Ok(enabled)
}

/// 抓一张隐形桌面上某个窗口的画面（给用户"看一眼它干到哪了"）。
///
/// 参数 `title` 为空时抓**尺寸最大的那个窗口**（通常就是主窗口；
/// 隐形桌面上会混着一堆 IME / 工具提示的空窗口，按面积挑最稳）。
#[cfg(windows)]
#[tauri::command]
fn bg_desk_shot(title: Option<String>, out_dir: String) -> Result<String, String> {
    let desk = win32desk::shared_desk()?;
    let mut wins: Vec<_> = desk.windows().into_iter().filter(|w| w.width > 0 && w.height > 0).collect();
    if wins.is_empty() {
        return Err("隐形桌面上没有可抓的窗口（是不是没有程序在跑？）".into());
    }
    let want = title.unwrap_or_default();
    let target = if want.trim().is_empty() {
        wins.sort_by_key(|w| -(w.width * w.height));
        wins.remove(0)
    } else {
        wins.into_iter()
            .find(|w| w.title.contains(want.trim()))
            .ok_or_else(|| format!("隐形桌面上没有标题含「{want}」的窗口"))?
    };

    let img = win32desk::capture_window(target.hwnd)?;
    let bytes = crate::capture::encode(&img, crate::capture::ShotFormat::Jpeg)?;
    let dir = std::path::PathBuf::from(&out_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录 {} 失败: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("bgdesk-{stamp}.jpg"));
    std::fs::write(&path, bytes).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;
    eprintln!(
        "[orbcat] 隐形桌面截图：{} → {}",
        target.title,
        path.display()
    );
    Ok(path.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— Agent 模式（标准 / PTC / 闲聊 / 创造）
// ---------------------------------------------------------------------------

/// 一个模式给前端看的形态（见 `modes::Mode` 的说明）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ModeView {
    id: String,
    name: String,
    description: String,
    /// 预载哪些组：`all` 或组名数组（原样回给前端展示）
    mcp_groups_preload: serde_json::Value,
    /// 本模式允许的组数；`all` 时为 `null`（含义是"全部，含未来新增"）
    allowed_count: Option<usize>,
    /// 模式点名了、但当前 MCP 里不存在的组（前端**标灰**）
    unknown_groups: Vec<String>,
    /// 工具呈现方式：`native` / `ptc`（前端据此说明"这个模式怎么干活"）
    tool_presentation: String,
    /// 正文能不能发图：`work`（只贴产物图）/ `free`（含表情包）。
    ///
    /// ⚠️ 前端拿它**只做展示**（在模式行上标一句），不做拦截 ——
    /// 工作模式也要渲染产物图，拦了会连截图一起挡掉。真正的约束是
    /// `agent.rs` 按这个值注入的那段措辞（见 `modes::ImageReplies`）。
    image_replies: String,
    /// 这个模式注入的场景上下文（可能为空，空则前端不显示这一行）
    prompt_extra: String,
    /// 聊天式呈现（2026-10-08）：一条回复拆成多个气泡 + **不显示「过程」**。
    /// 前端据此决定走 `bubble.ts` 的切分。只有闲聊模式为 true。
    chatty: bool,
}

/// 当前注册表里的组名（锁最多等 2 秒 —— 与 `mcp_status` 同一条保险丝）
async fn registry_group_names(state: &AppState) -> Vec<String> {
    match tokio::time::timeout(std::time::Duration::from_secs(2), state.mcp.lock()).await {
        Ok(reg) => reg.groups().iter().map(|g| g.name.clone()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 读全部 agent 模式 + 当前生效模式 + 当前模式对每个 MCP 组的影响。
///
/// 前端只用这一条命令就能画完整个模式选择器：
/// 每个模式的说明与"允许的组"、以及每组在当前模式下的状态
/// （`allowed` 给模型 / `denied` 被本模式收起 / `unknown` 模式里写了但不存在）。
///
/// ## `session_id`（2026-10-08，模式改跟会话走）
///
/// 传了就按**那条会话**的模式回 `activeMode`；没传（启动早期 / 老前端）给内置默认。
/// 另外单给一个 `defaultMode` —— 那**不是**"当前模式"，而是**新建会话时的默认值**，
/// 新建菜单拿它标「默认」。它是**编译期常量** `modes::DEFAULT_MODE_ID`，
/// 不再来自 `settings.activeMode`（那个字段已删，理由见
/// `sessions::effective_mode` 的说明 —— 用户看到「闲聊」被标成「默认」就是它干的）。
#[tauri::command]
async fn modes_get(
    state: State<'_, AppState>,
    session_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let available = registry_group_names(&state).await;
    let modes = modes::load(&state.data_dir);

    let views: Vec<ModeView> = modes
        .iter()
        .map(|m| {
            let a = modes::allowed_groups_for_with(m, &available);
            ModeView {
                id: m.id.clone(),
                name: m.name.clone(),
                description: m.description.clone(),
                mcp_groups_preload: serde_json::to_value(&m.mcp_groups_preload)
                    .unwrap_or(serde_json::Value::Null),
                allowed_count: if a.all { None } else { Some(a.allowed.len()) },
                unknown_groups: a.unknown.iter().cloned().collect(),
                tool_presentation: if m.is_ptc() { "ptc" } else { "native" }.to_string(),
                image_replies: if m.freestyle_images() { "free" } else { "work" }.to_string(),
                prompt_extra: m.prompt_extra.clone(),
                chatty: m.chatty,
            }
        })
        .collect();

    // ⚠️ 走 `effective_mode` 这个唯一入口 —— 别在这儿再写一遍"读 settings + resolve"。
    let active = crate::sessions::effective_mode(&state.data_dir, session_id.as_deref());
    // 「新建会话的默认模式」= 内置默认那个模式**解析后**的 id。
    // 为什么不是直接回 `DEFAULT_MODE_ID` 常量：用户完全可能手编 modes.json 把
    // `standard` 删了 —— 那时常量就指向一个不存在的 id，新建菜单里**没有一项**
    // 会带上「默认」标（看似小，但那是"界面在撒谎"）。`resolve` 会把这种情况
    // 兜到文件里真实存在的某个模式上。
    let default_mode = modes::resolve(&state.data_dir, None);
    let active_allowed = modes::allowed_groups_for_with(&active, &available);
    let groups: Vec<serde_json::Value> = available
        .iter()
        .map(|g| {
            let st = if active_allowed.allows(g) { "allowed" } else { "denied" };
            serde_json::json!({ "name": g, "state": st })
        })
        .collect();

    Ok(serde_json::json!({
        "activeMode": active.id,
        "defaultMode": default_mode.id,
        "modes": views,
        "groups": groups,
        "unknownGroups": active_allowed.unknown.iter().cloned().collect::<Vec<_>>(),
        "mcpConnected": !available.is_empty(),
    }))
}

// 历史（2026-10-08 删）：这里原来有 `modes_set(id)` —— 改"当前 agent 模式"。
// 它的三级演变值得留着，免得有人又想加回来：
//   ① 全局"当前模式" → ② 模式跟会话走后降级成"新建会话的默认模式"
//   → ③ **彻底删掉**。
// 删它的触发点：用户看到「＋」菜单里**「闲聊」被标成「默认」**，而他要的是
// "标准是默认"。那个标读的就是 `modes_set` 写进 `settings.json` 的 `activeMode`
// （用户早先在设置页切过闲聊，值就留在文件里成了"默认"）。
// 教训：**界面上没有了改它的入口之后，任何"用户可配的默认值"都会变成一个
// 会撒谎的死字段**（手编也不生效）。所以默认值只能是编译期常量
// （`modes::DEFAULT_MODE_ID`），而"要哪个模式"只能在**建会话时**选。
// 另外 `modes::exists()` 也只剩测试在用 —— 留着无害（它是个合理的公共 API）。

/// 写 MCP 全局开关：默认全给 / 回到按需加载
#[tauri::command]
async fn mcp_set_all_groups(
    state: State<'_, AppState>,
    all: bool,
) -> Result<(), String> {
    let mut s = config::load_settings(&state.data_dir);
    s.mcp_all_groups = all;
    // 切成"全给"时清掉显式关闭记录，语义最干净：全部打开
    if all {
        s.mcp_group_enabled.clear();
    }
    config::save_settings(&state.data_dir, &s)?;
    let allowed = allowed_for_dir(&state);
    let mut reg = state.mcp.lock().await;
    reg.restore_active_from_settings(&s, Some(allowed));
    Ok(())
}

/// 读模型分组显示名（键 = Base URL）。前端渲染组头用。
#[tauri::command]
fn model_group_names(
    state: State<'_, AppState>,
) -> std::collections::BTreeMap<String, String> {
    config::load_settings(&state.data_dir).model_group_names
}

/// 保存模型分组的显示名（键 = Base URL）。空值表示恢复默认（host）。
#[tauri::command]
fn model_group_rename(
    state: State<'_, AppState>,
    url: String,
    name: String,
) -> Result<(), String> {
    let mut s = config::load_settings(&state.data_dir);
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("URL 不能为空".into());
    }
    let name = name.trim().to_string();
    if name.is_empty() {
        s.model_group_names.remove(&url);
    } else {
        s.model_group_names.insert(url, name);
    }
    config::save_settings(&state.data_dir, &s)
}

/// **整组更新 API Key**：把该 Base URL 下所有模型共用的 Key 换成新的。
///
/// 为什么做成组级：现在 Key 仍存 per-model，但用户心智里"一个提供商一个 Key"，
/// 让他在组菜单里改一次就全生效，比逐个模型点编辑再粘一次合理。
#[tauri::command]
fn models_set_group_key(
    state: State<'_, AppState>,
    url: String,
    api_key: String,
) -> Result<usize, String> {
    let url = config::normalize_base_url(&url)?;
    let key = api_key.trim().to_string();
    let mut all = config::load_models(&state.data_dir)?;
    let mut n = 0usize;
    for m in all.iter_mut() {
        if config::url_key(&m.url) == config::url_key(&url) {
            m.api_key = key.clone();
            n += 1;
        }
    }
    if n == 0 {
        return Err("这个 Base URL 下没有模型".into());
    }
    config::save_models(&state.data_dir, &all)?;
    Ok(n)
}

/// **删除整组**：删掉该 Base URL 下的所有模型。返回删除条数。
#[tauri::command]
fn models_remove_group(state: State<'_, AppState>, url: String) -> Result<usize, String> {
    let key = config::url_key(&url);
    let mut all = config::load_models(&state.data_dir)?;
    let before = all.len();
    all.retain(|m| config::url_key(&m.url) != key);
    let removed = before.saturating_sub(all.len());
    if removed == 0 {
        return Err("这个 Base URL 下没有模型".into());
    }
    config::save_models(&state.data_dir, &all)?;
    // 选中的模型被删了就清掉选中（组删光后该 id 也不存在了）
    let mut s = config::load_settings(&state.data_dir);
    if let Some(cur) = s.selected_model.clone() {
        if !all.iter().any(|m| m.id == cur) {
            s.selected_model = None;
            s.selected_model_url = None;
            config::save_settings(&state.data_dir, &s)?;
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// 「模型管理」独立窗口（模型编辑/分组只住在这里；面板只负责展示与选择）
// ---------------------------------------------------------------------------

/// 把 models 窗口当前尺寸记回 settings.json（逻辑像素，一位小数）。
fn persist_models_window_size(dir: &std::path::Path, win: &tauri::WebviewWindow) {
    let Ok(sz) = win.outer_size() else { return };
    let Ok(scale) = win.scale_factor() else { return };
    if scale <= 0.0 {
        return;
    }
    let w = sz.width as f64 / scale;
    let h = sz.height as f64 / scale;
    if w < 200.0 || h < 150.0 {
        return; // 最小化/异常尺寸不记
    }
    let mut s = config::load_settings(dir);
    s.models_window.w = (w * 10.0).round() / 10.0;
    s.models_window.h = (h * 10.0).round() / 10.0;
    if let Err(e) = config::save_settings(dir, &s) {
        eprintln!("[orbcat] 保存模型窗口尺寸失败: {e}");
    }
}

/// 诊断日志：追加写 `agent-data/diag.log`。
///
/// 专用于排查「窗口创建 / 前端启动」这类**看不到控制台**的问题：
/// 用户自己启动的 exe 没有 stderr 可看，把关键步骤落盘后直接读文件。
/// 正式功能不要依赖它，排查完可以留着（开销是一次 append）。
fn diag_write(dir: &std::path::Path, msg: &str) {
    use std::io::Write;
    let path = dir.join("diag.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// 前端打诊断点（models.ts / main.ts 启动、全局错误都往这里写）。
#[tauri::command]
fn diag_log(state: State<'_, AppState>, msg: String) -> Result<(), String> {
    diag_write(&state.data_dir, &format!("[ui] {msg}"));
    Ok(())
}

/// 打开「模型管理」独立窗口（**懒创建**）。
///
/// - 尺寸：settings.modelsWindow（用户拖出来的记忆值，默认 720×640）——
///   改尺寸**不需要重新编译**，拖边即可，关窗自动记。
/// - 位置：贴面板右侧弹；右侧放不下翻左侧；垂直与面板顶对齐再夹进屏幕。
/// - 已开着时只把它带到前台。
///
/// ⚠️ 为什么不在 tauri.conf.json 里声明这个窗口（2026-09-29 实测）：
/// 启动时同时创建两个 webview，第二个会与 orb 的 webview 争抢 WebView2 环境，
/// 留下一个**底层无效**的窗口对象 —— 之后任何 getter（scale_factor /
/// outer_position / current_monitor）都收不到事件循环的回复，前端拿到
/// `runtime error: failed to receive message from webview`。改成点击时再建，
/// 启动期只有一个 webview，问题消失。
#[tauri::command]
async fn models_window_open(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let dir = state.data_dir.clone();
    models_window_open_inner(app, dir).await
}

async fn models_window_open_inner(
    app: tauri::AppHandle,
    dir: std::path::PathBuf,
) -> Result<(), String> {
    models_window_inner(app, dir, false).await
}

/// 真实现。`preheat = true` 表示**只是预热**：建好隐藏窗口就返回，
/// 不摆位、不显示（见 [`spawn_models_preheat`]）。
async fn models_window_inner(
    app: tauri::AppHandle,
    dir: std::path::PathBuf,
    preheat: bool,
) -> Result<(), String> {
    let t0 = std::time::Instant::now();
    diag_write(
        &dir,
        &format!("open: 进入命令（preheat={preheat}）"),
    );

    // 已经建过 → 直接复用（关窗是 hide，不销毁）
    if let Some(win) = app.get_webview_window("models") {
        diag_write(
            &dir,
            &format!(
                "open: 复用已存在的 models 窗口（耗时 {} ms）",
                t0.elapsed().as_millis()
            ),
        );
        if preheat {
            // 预热路径：窗口已经在了，说明另有并发调用刚建好 —— 什么都不用做
            return Ok(());
        }
        let r = show_models_window(&app, &dir, &win);
        diag_write(&dir, &format!("open: 复用路径结果 = {r:?}"));
        return r;
    }

    let cfg = config::load_settings(&dir).models_window;
    let (w0, h0) = (cfg.w.max(480.0), cfg.h.max(360.0));
    diag_write(&dir, &format!("open: 创建窗口 {w0}x{h0}"));

    let app2 = app.clone();
    let dir2 = dir.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    app.run_on_main_thread(move || {
        let r = tauri::WebviewWindowBuilder::new(
            &app2,
            "models",
            tauri::WebviewUrl::App("models.html".into()),
        )
        .title("模型管理")
        .inner_size(w0, h0)
        .min_inner_size(480.0, 360.0)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(true)
        .shadow(false) // 与 orb 对齐：Windows 上 shadow 会和自绘边框打架
        // 关键：这台机器的 WebView2 必须禁用 GPU 合成才能建起 webview
        // （orb 窗口一直靠同一串参数，见 tauri.conf.json 的 additionalBrowserArgs）。
        // 少了它 build() 会返回 Ok，但底层 webview 根本没起来 —— 之后任何
        // getter 都报 failed to receive message from webview，且页面永不加载。
        .additional_browser_args(ORB_BROWSER_ARGS)
        .visible(false) // 先建后摆位，避免在 (0,0) 闪一下
        // 页面加载结果落盘：能区分「窗口建了但页面没加载」和「页面加载了但 JS 挂了」
        .on_page_load(move |_, payload| {
            let what = match payload.event() {
                tauri::webview::PageLoadEvent::Started => "Started",
                tauri::webview::PageLoadEvent::Finished => "Finished",
            };
            diag_write(&dir2, &format!("page_load: {what} {}", payload.url()));
        })
        .build()
        .map(|_| ())
        .map_err(|e| format!("创建模型管理窗口失败：{e}"));
        let _ = tx.send(r);
    })
    .map_err(|e| format!("调度创建窗口失败：{e}"))?;

    let r = rx.recv().map_err(|e| format!("等待窗口创建失败：{e}"))?;
    diag_write(&dir, &format!("open: build 结果 = {r:?}"));
    r?;

    let Some(win) = app.get_webview_window("models") else {
        return Err("窗口创建后仍取不到句柄".to_string());
    };

    // 预热路径：建好就收工，**不摆位不显示**。
    //
    // 为什么预热不顺便摆位：摆位要读 orb 的 `outer_position` / `current_monitor`，
    // 而预热发生在启动后几秒 —— 这时 orb 可能还在初始化，读到的位置是默认角落，
    // 摆过去反而错。摆位留给真正的 `open`（那时用户已经点了，位置才是用户要的）。
    if preheat {
        diag_write(
            &dir,
            &format!(
                "preheat: models 窗口已建好（隐藏），耗时 {} ms —— 首次点开将走复用路径",
                t0.elapsed().as_millis()
            ),
        );
        return Ok(());
    }

    let r = show_models_window(&app, &dir, &win);
    diag_write(
        &dir,
        &format!(
            "open: 新建后 show 结果 = {r:?}（总耗时 {} ms）",
            t0.elapsed().as_millis()
        ),
    );
    r
}

/// 启动后在后台**预热**「模型管理」窗口。
///
/// ## 为什么需要（2026-09-30）
///
/// 这个窗口是懒创建的，代价落在**用户第一次点击**上：实测冷启 8.4s
/// （WebView2 首次为该进程建渲染器 + 加载 models.html）。用户点了卡片之后
/// 盯着屏幕等八秒，主观上就是"功能坏了"。
///
/// ## 为什么要延迟，而不是启动时立刻建
///
/// 2026-09-29 实测过：启动时**同时**创建两个 webview，第二个会与 orb 的
/// webview 争抢 WebView2 环境，留下一个"底层无效"的窗口对象 ——
/// 之后任何 getter 都报 `failed to receive message from webview`。
/// 所以必须等 orb 那一套彻底起来之后再建。
///
/// 延迟时长用 `ORBCAT_MODELS_PREHEAT_MS` 覆盖（**测试与无头验证用**）：
/// - 设 `0` → 立刻预热（用来复现"两个 webview 抢环境"那个坑）
/// - 设负数 → 跳过预热
/// - 不设 → 默认 [`MODELS_PREHEAT_DELAY_MS`]
///
/// ⚠️ 预热**失败不算错**：只记一行日志。最坏情况退化成改动前的行为
/// （首次点击时懒创建），而不是让程序起不来。
fn spawn_models_preheat(app: tauri::AppHandle, dir: std::path::PathBuf) {
    let delay_ms = match std::env::var("ORBCAT_MODELS_PREHEAT_MS") {
        Ok(v) => match v.trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) => MODELS_PREHEAT_DELAY_MS as i64,
        },
        Err(_) => MODELS_PREHEAT_DELAY_MS as i64,
    };
    if delay_ms < 0 {
        diag_write(&dir, "preheat: 被 ORBCAT_MODELS_PREHEAT_MS<0 跳过");
        return;
    }

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms as u64));
        // 已经开过（用户手快）就不用预热了
        if app.get_webview_window("models").is_some() {
            diag_write(&dir, "preheat: 窗口已存在（用户已点开），跳过");
            return;
        }
        diag_write(&dir, &format!("preheat: 开始预热（延迟 {delay_ms} ms）"));
        let app2 = app.clone();
        let dir2 = dir.clone();
        // 复用与点击同一条创建路径（preheat=true 只建不显示）——
        // 走两条不同的创建代码是 bug 温床。
        let r = tauri::async_runtime::block_on(models_window_inner(app2, dir2, true));
        match r {
            Ok(()) => {}
            Err(e) => eprintln!("[orbcat] 模型窗口预热失败（不影响使用，首次点开仍会懒创建）: {e}"),
        }
    });
}

/// 定位并显示已存在的「模型管理」窗口。
fn show_models_window(
    app: &tauri::AppHandle,
    dir: &std::path::Path,
    win: &tauri::WebviewWindow,
) -> Result<(), String> {
    let cfg = config::load_settings(dir).models_window;
    let scale = win
        .scale_factor()
        .map_err(|e| format!("取缩放比失败：{e}"))?;
    let w = (cfg.w.clamp(480.0, 4096.0) * scale).round() as i32;
    let h = (cfg.h.clamp(360.0, 4096.0) * scale).round() as i32;

    let mut x: i32 = 0;
    let mut y: i32 = 0;
    if let Some(panel) = app.get_webview_window("orb") {
        let pp = panel.outer_position().map_err(|e| format!("取面板位置失败：{e}"))?;
        let ps = panel.outer_size().map_err(|e| format!("取面板尺寸失败：{e}"))?;
        let mon = panel.current_monitor().ok().flatten();
        let gap = (12.0 * scale).round() as i32;
        x = pp.x + ps.width as i32 + gap; // 默认：面板右侧
        y = pp.y; // 顶对齐
        if let Some(m) = mon {
            let (mx, my) = (m.position().x, m.position().y);
            let (mw, mh) = (m.size().width as i32, m.size().height as i32);
            if x + w > mx + mw {
                x = pp.x - w - gap; // 右侧放不下 → 翻到面板左侧
            }
            x = x.clamp(mx, (mx + mw - w).max(mx));
            y = y.clamp(my, (my + mh - h).max(my));
        }
    }

    let _ = win.set_size(tauri::PhysicalSize::new(w.max(1), h.max(1)));
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    let _ = win.unminimize();
    win.show().map_err(|e| format!("显示窗口失败：{e}"))?;
    let _ = win.set_focus();
    Ok(())
}

/// 隐藏「模型管理」窗口（不销毁，再开秒出），并把当前尺寸记回 settings。
#[tauri::command]
async fn models_window_close(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let dir = state.data_dir.clone();
    if let Some(win) = app.get_webview_window("models") {
        persist_models_window_size(&dir, &win);
        let _ = win.hide();
    }
    Ok(())
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
    // 模型选择：**项目槽位优先**（`project.rs::slot_model`）。
    //
    // fail-open 语义：槽位引用的 id 在当前 `models.json` 里不存在时
    // `slot_model` 返回 `None`，自动回落到用户选的模型 —— 分享来的 bundle
    // 引用了别人机器上的模型名是常态，不该因此整个项目不能用。
    let s = config::load_settings(&state.data_dir);
    let (model_id, model_url) = match project::slot_model(&state.data_dir) {
        Some(m) => (m, None),
        None => (
            s.selected_model
                .clone()
                .ok_or_else(|| "尚未选择模型，请先在设置里选一个".to_string())?,
            s.selected_model_url.clone(),
        ),
    };

    // 身份 = (Base URL, 接口 id)：带 url 精确定位；槽位/旧数据没 url 时裸 id 兜底
    let cfg = match model_url.as_deref() {
        Some(u) => config::find_model_by_url_id(&state.data_dir, u, &model_id)
            .or_else(|_| config::find_model(&state.data_dir, &model_id))?,
        None => config::find_model(&state.data_dir, &model_id)?,
    };

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

    // ---- 轮末自动蒸馏（2026-10-07 用户定案：不靠模型自觉调 remember）----
    //
    // 后台发一个小请求，从本轮问答里提取 0~3 条记忆条目：
    // 激活项目 → 直写项目 MEMORY.md；主对话 → 进全局待审批候选。
    // 三条纪律：
    //   1. **后台**：tauri::async_runtime::spawn，不阻塞本轮返回给前端；
    //   2. **失败无害**：只打日志，蒸馏挂了不影响对话；
    //   3. **省调用**：中断轮、太短的轮（合计 < 60 字，寒暄）不蒸馏。
    // Err 分支（本轮失败）不蒸馏 —— 半截产物提不出可靠记忆。
    if !run.interrupted {
        let merged_chars = input.chars().count() + run.answer.chars().count();
        if merged_chars >= memory::AUTO_DISTILL_MIN_CHARS {
            let dd = state.data_dir.clone();
            let cfg2 = cfg.clone();
            let q = input.clone();
            let a = sessions::strip_step_folds(&run.answer);
            tauri::async_runtime::spawn(async move {
                match memory::auto_distill(&cfg2, &dd, &q, &a).await {
                    Ok(n) if n > 0 => eprintln!("[orbcat] 轮末自动蒸馏: 写入 {n} 条记忆"),
                    Ok(_) => {}
                    Err(e) => eprintln!("[orbcat] 轮末自动蒸馏失败（不影响对话）: {e}"),
                }
            });
        }
    }

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
            // 上下文预算的真锚点（2026-09-30）：只有本轮真拿到服务端 usage 才有值，
            // 没有时是 None，`append_run_in` 会保留会话里上一轮的值。
            last_prompt_tokens: run.last_prompt_tokens,
            token_scale: run.token_scale,
        },
    ) {
        eprintln!("[orbcat] 写会话失败: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 备份与回收站（还原 + 保留策略）
// ---------------------------------------------------------------------------

/// 备份 + 归档 + 占用统计，一次拉全（设置页一个请求就够）。
#[tauri::command]
fn recovery_state(state: State<'_, AppState>) -> recovery::RecoveryState {
    recovery::build_state(&state.data_dir)
}

/// 还原一份备份到原路径（或指定路径）。
///
/// ⚠️ 这里**不做文件权限网关校验**，理由与 `perm_add_rule` 等设置类命令一致：
/// 这是**用户在界面上亲自点**的还原操作，且目标路径来自 agent 自己写的
/// `backups.jsonl`。若再走一遍 `Access::Full` 判定，用户在设置页点了还原却
/// 被"权限不足"挡住，只会让人以为是 bug —— 权限网关约束的是**模型**，不是用户。
#[tauri::command]
fn recovery_restore_backup(
    state: State<'_, AppState>,
    id: String,
    target: Option<String>,
) -> Result<String, String> {
    recovery::restore_backup(&state.data_dir, &id, target.as_deref())
}

/// 从回收站还原一项到原路径（或指定路径）。
#[tauri::command]
fn recovery_restore_trash(
    state: State<'_, AppState>,
    trashed: String,
    target: Option<String>,
) -> Result<String, String> {
    recovery::restore_trash(&state.data_dir, &trashed, target.as_deref())
}

/// 按保留策略清理（**只在用户点按钮时执行**，不做后台自动删除）。
#[tauri::command]
fn recovery_prune(
    state: State<'_, AppState>,
    keep_days: Option<u64>,
    keep_items: Option<usize>,
) -> Result<recovery::PruneOutcome, String> {
    let keep = recovery::Retention {
        keep_days: keep_days.unwrap_or(recovery::DEFAULT_KEEP_DAYS),
        keep_items: keep_items.unwrap_or(recovery::DEFAULT_KEEP_ITEMS),
    };
    recovery::prune(&state.data_dir, keep)
}

/// 生成诊断包并返回落盘路径。
///
/// 为什么值得有（2026-09-30）：出问题时用户唯一的通道一直是**截图一条红条**，
/// 而现场散在四处（`diag.log` 的前端异常、`audit-commands.jsonl` 跑了什么命令、
/// 会话里的 `kind = "error"` 条目、各目录体积）。让用户手工凑这四样等于劝退，
/// 所以做成一个按钮：生成 → 落盘 → 用户把文件拖进对话即可。
///
/// **key 一律脱敏**（走 `config::redact_key_str`，与全项目日志同一收口）——
/// 这份文件是要被转发的。
#[tauri::command]
fn recovery_diagnostic(state: State<'_, AppState>) -> Result<String, String> {
    recovery::write_diagnostic(&state.data_dir).map(|p| p.display().to_string())
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
///
/// `mode` = 用户在「新建会话」菜单里选的模式 id（2026-10-08）。
/// 会话**建的时候**就把模式钉死，之后不再改 —— 用户定案："一个会话模式不能变"。
/// 不传 → 内置默认 `modes::DEFAULT_MODE_ID`（`standard`）；传了无效 id →
/// 由 `modes::resolve` 兜到同一处。**没有"用户可配的默认模式"这回事**
/// （`settings.activeMode` 已删，理由见 `sessions::effective_mode` 的说明）。
#[tauri::command]
async fn session_new(
    state: State<'_, AppState>,
    mode: Option<String>,
) -> Result<sessions::Session, String> {
    let fallback = modes::DEFAULT_MODE_ID;
    let s = sessions::new_session(&state.data_dir, mode.as_deref().unwrap_or(fallback));
    eprintln!(
        "[orbcat] 新会话 {}（模式 {}）",
        s.id,
        s.mode.as_deref().unwrap_or(fallback)
    );
    // 新会话没有历史授权 —— 顺手清掉上一段残留的（尤其 `Once`/`Turn`）。
    // ⚠️ 但**有 run 在跑时跳过**：`reload_grants_for_session` 会把内存授权换成新会话的，
    //    正在跑的任务会当场丢权限（工具调用全被拒）。
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &s.id);
    }
    // 新会话可能选了另一个模式（`new_session` 已把它设为当前）→ MCP 活跃组跟着重算
    let n = resync_mode_groups(&state).await;
    eprintln!("[orbcat] 新会话后重算 MCP 活跃组：{n} 个");
    Ok(s)
}

/// 切换到指定会话
///
/// **允许在别的会话跑着的时候切走，也允许切到另一个正在跑的会话**（多会话并行 run）。
/// 落盘路径（`*_in` 系列）带着开跑时的 `session_id`，回答不会写错会话。
///
/// ⚠️ 但**有任意 run 在跑时不重载授权**：`reload_grants_for_session` 会把内存里那份
/// 换成"目标会话"的，正在跑的任务会因此**当场丢权限**（它的工具调用全被拒）。
///
/// ⚠️ 2026-10-08：模式跟会话走 → 切会话 = **换模式** → MCP 活跃组必须跟着重算
/// （见 [`resync_mode_groups`]）。这一步**刻意不加"有 run 在跑就跳过"的闸**：
/// 加了的话，你切到「闲聊」时它在后台跑着任务，闲聊会话就会**一直**带着满组的
/// MCP 工具（没有任何后续事件会补算这次），比"在跑的那条会话下一轮少几个工具"
/// 黏得多。已知残留：多会话并行且模式不同时，活跃集只跟当前会话走
/// —— 这与改动前（全局模式）同级，没有恶化。
#[tauri::command]
async fn session_switch(
    state: State<'_, AppState>,
    id: String,
) -> Result<sessions::Session, String> {
    // 换会话 = 上一段「整个任务」结束。不清的话，A 任务批的授权会漏到 B 任务里去；
    // 同时把新会话自己落盘的那份装回来。
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &id);
    }
    let s = sessions::switch(&state.data_dir, &id)?;
    let n = resync_mode_groups(&state).await;
    eprintln!(
        "[orbcat] 切到会话 {}（模式 {}，MCP 活跃组 {n} 个）",
        s.id,
        s.mode.as_deref().unwrap_or("-")
    );
    Ok(s)
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
    let s = config::load_settings(&state.data_dir);
    let model_id = s
        .selected_model
        .clone()
        .ok_or_else(|| "尚未选择模型，无法生成摘要".to_string())?;
    let cfg = match s.selected_model_url.as_deref() {
        Some(u) => config::find_model_by_url_id(&state.data_dir, u, &model_id)
            .or_else(|_| config::find_model(&state.data_dir, &model_id))?,
        None => config::find_model(&state.data_dir, &model_id)?,
    };
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
async fn session_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
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
    // 删掉的是当前会话时，`ensure_current` 可能把当前切到**另一个模式**的会话上
    // → MCP 活跃组跟着重算（与 `session_switch` 同一理由）。
    let n = resync_mode_groups(&state).await;
    eprintln!("[orbcat] 删除会话 {id} 后重算 MCP 活跃组：{n} 个");
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
// Tauri 命令 —— 记忆蒸馏（2026-10-03）
// ---------------------------------------------------------------------------

/// 开一条「记忆蒸馏」会话：新建会话 + 打 `sessions/<id>.distill` 标记 + 命名。
///
/// 之后就是**一条普通聊天**：`agent::run` 见到标记会追加蒸馏角色说明并注入
/// `distill_move` 工具，模型在标准循环里读中转站、问用户、搬条目，
/// 思考与工具调用全走主对话那套时间线（用户 2026-10-03 明确要求）。
#[tauri::command]
fn distill_session_open(state: State<'_, AppState>) -> Result<sessions::Session, String> {
    // 蒸馏会话**固定用标准模式**：它要的是完整工具集（`tool_specs_for_distill`
    // = 标准工具 + `distill_move`），换成闲聊模式反而拿不到改文件的工具。
    let s = sessions::new_session(&state.data_dir, crate::modes::DEFAULT_MODE_ID);
    sessions::set_distill(&state.data_dir, &s.id)?;
    let _ = sessions::set_title(&state.data_dir, &s.id, "记忆蒸馏");
    eprintln!("[orbcat] 蒸馏会话 {}", s.id);
    if !state.runs.any_active() {
        reload_grants_for_session(&state, &s.id);
    }
    Ok(sessions::load(&state.data_dir, &s.id).unwrap_or(s))
}

// ---------------------------------------------------------------------------
// Tauri 命令 —— 项目记忆维护（改路径 / 归档）
// ---------------------------------------------------------------------------

/// 改/清项目的 source.ref（`path` 为空 = 解绑，显示「未绑定路径」）
#[tauri::command]
fn mem_project_set_source(
    state: State<'_, AppState>,
    name: String,
    path: String,
) -> Result<(), String> {
    state.memory.set_project_source(&name, &path)?;
    // 绑定路径变了 → 项目目录授权跟着变（改的是激活项目时才实际生效）
    state.refresh_project_dir_rules();
    Ok(())
}

/// 归档一个项目记忆目录（挪进 `agent-data/.trash/`，不硬删）
#[tauri::command]
fn mem_project_archive(state: State<'_, AppState>, name: String) -> Result<String, String> {
    state.memory.archive_project(&name)
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
    // 删掉的项目可能挂着目录授权 → 重建（被删项不再挂）
    state.refresh_project_dir_rules();
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
    // 切项目 → 项目目录授权跟着换（旧项目摘掉、新项目挂上）
    state.refresh_project_dir_rules();
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
    pmem::rename_project(&state.data_dir, id, &name)?;
    // 规则 label 里带着项目名，改名后重建让标签对上（路径不变、纯标签修正）
    state.refresh_project_dir_rules();
    Ok(())
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
// 悬浮球界面状态（睡眠 / 唤醒）
// ---------------------------------------------------------------------------

/// 球的界面状态。
///
/// ## 睡眠 = **球从屏幕上完全消失**，但任务照跑（2026-09-30 用户重新定义）
///
/// 早先实现成"球留在屏幕上、变暗缩小"，同时**停掉前台采样**。用户明确纠正：
/// *"睡眠是球从页面上消失但是任务不停止啊"* —— 即睡眠是**纯视觉层面的隐身**：
///
/// 1. 球**真的隐藏**（窗口 `hide()`），不是变暗缩小 —— 用户要的是"别占地方"；
/// 2. **任务不受任何影响**：agent loop 跑在自己的 tokio 任务里，与球的可见性
///    毫无关系，睡眠/唤醒都不许碰它；
/// 3. **前台采样也不停** —— 它喂的是模型的上下文（"用户正在看什么"），
///    球看不见了不代表用户在看的窗口不存在。停掉它 = 睡着期间问 agent 问题，
///    agent 对眼前的东西一无所知。
///
/// 唤醒 = 让球重新出现（`show()`），**同样不碰任务**。
///
/// 怎么唤醒（球都没了，不能让用户找不到入口）：托盘菜单「唤醒」，
/// 或**再启动一次程序**（走 [`wake_and_open_panel`]，第二实例会把面板打开）。
///
/// 为什么不持久化：重启后必须回到「清醒可见」—— 否则用户重启一次
/// 就面对一个看不见的球，还得先学会怎么弄出来。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct OrbUi {
    sleeping: bool,
}

static ORB_UI: Mutex<OrbUi> = Mutex::new(OrbUi { sleeping: false });

fn orb_ui_snapshot() -> OrbUi {
    ORB_UI.lock().map(|g| *g).unwrap_or_default()
}

/// 执行一次球状态动作。**托盘菜单 / 球的右键菜单 / 第二实例启动**三条入口
/// 共用这一个函数，保证语义完全一致。
///
/// - `wake`  唤醒：让球重新出现（**不动任务**）
/// - `sleep` 睡眠：把球从屏幕上隐藏（**不动任务、不停采样**）
///
/// ⚠️ 两条动作都**不调 `apply_mode`**：窗口形态（球态/面板态）是另一条正交的
/// 状态。睡眠时隐藏一个球态窗口，唤醒时它原地回来 —— 重设 bounds 反而会把
/// 用户拖过的位置弹回默认角落。
///
/// ⚠️ 唤醒时窗口形态**保持**：球态就回球态，面板态就回面板态。
/// 正在跑任务时用户多半是在面板里，唤醒后应该接着看到进度，而不是被缩回球。
fn orb_ui_apply(app: &tauri::AppHandle, action: &str) -> Result<(), String> {
    let sleeping = match action {
        "wake" => false,
        "sleep" => true,
        other => {
            return Err(format!("未知球状态动作「{other}」，应为 wake / sleep"))
        }
    };

    let next = OrbUi { sleeping };
    if let Ok(mut g) = ORB_UI.lock() {
        *g = next;
    }

    // ⚠️ 这里**刻意没有任何"停采样"动作**（2026-09-30 删掉了旧的
    //    `context::set_paused(sleeping)`）。
    //
    // 旧实现把"界面隐身"错当成"省电模式"：睡着就跳过前台采样。但采样产出的
    // "用户正在看什么"是**每轮对话的上下文** —— 停掉它，等于睡着期间问 agent
    // 问题，agent 对眼前的东西一无所知。而球看不见了不代表用户在看的窗口不存在。
    // 详见 `context::spawn_watcher` 的说明（那套 PAUSED 开关已整体删除）。

    // 睡眠 = 真的隐藏；唤醒 = 真的显示。这是本函数唯一的实质动作。
    //
    // ⚠️ `hide()` 只隐藏窗口，**不销毁 WebView**：正在跑的流式渲染、输入框草稿、
    //    进行中气泡全在内存里，唤醒后原样还在。这正是"任务不停止"的实现基础——
    //    我们不碰 agent 的任何状态，只是让承载它的窗口看不见。
    if let Some(win) = app.get_webview_window("orb") {
        let r = if sleeping { win.hide() } else { win.show() };
        if let Err(e) = r {
            eprintln!(
                "[orbcat] ⚠️ {}悬浮球失败: {e}",
                if sleeping { "隐藏" } else { "显示" }
            );
        }
    }

    // 球消失了，用户可能不知道怎么找回来 —— 托盘是唯一线索，换成睡猫 + 换提示。
    //
    // 为什么必须有这个：睡眠把**唯一的常驻可见入口**藏掉了。托盘图标虽然一直在，
    // 但它和醒着时长得一样，用户没有视觉线索知道"球只是睡了、程序还在跑"。
    // 换图标 + 更新 tooltip 是最低成本的自解释（左键单击托盘 = 唤醒）。
    //
    // ⚠️ 失败不影响动作（气泡/图标掉了也要能睡）。
    apply_tray_visual(app, sleeping);

    // 前端据此更新本地状态（它的 `orbSleeping` 只影响 title/菜单措辞 —— 
    // 球本身已经不可见了）。失败不让整个动作失败：窗口该隐藏的已经隐藏了。
    if let Err(e) = app.emit("orb-ui", next) {
        eprintln!("[orbcat] ⚠️ 广播球状态失败: {e}");
    }

    eprintln!(
        "[orbcat] 球状态: sleeping={sleeping}（{}，任务不受影响）",
        if sleeping { "已从屏幕隐藏" } else { "已重新显示" }
    );
    Ok(())
}

/// 睡眠时把托盘图标换成**睡猫**，并更新提示文案；唤醒时换回来。
///
/// ## 为什么值得做（2026-09-30）
///
/// 睡眠把球从屏幕上隐藏了 —— 也就是把**唯一的常驻可见入口**藏掉了。
/// 托盘图标虽然还在，但它原本和醒着时长得一模一样，用户没有任何视觉线索
/// 知道"球只是睡了，程序还在跑"。
///
/// 项目里本来就有 `public/orb/orb-sleep.png`（靛蓝睡猫，第二十九阶段做的
/// 三帧资产之一），睡眠态直接复用它 —— 与球睡着时是同一只猫，语义自洽。
///
/// ## 为什么读文件而不是编译期嵌入
///
/// 前端资源已经外置（见 `disk_assets.rs`），`public/orb/*.png` 就在
/// `dist/orb/` 里、随 exe 同目录分发。再嵌一份进二进制等于同一张图存两份，
/// 而且会让改图必须重编 Rust —— 正是外置方案要避免的事。
///
/// 读不到**不算错误**：托盘保持原图标即可，睡眠/唤醒本身照常生效。
fn apply_tray_visual(app: &tauri::AppHandle, sleeping: bool) {
    let Some(tray) = app.tray_by_id("orbcat-tray") else {
        eprintln!(
            "[orbcat] 球{}：托盘图标不存在（用户可用「再启动一次」或托盘入口恢复）",
            if sleeping { "已睡眠" } else { "已唤醒" }
        );
        return;
    };

    let tip = if sleeping {
        "orbcat（睡眠中）— 左键单击唤醒"
    } else {
        "orbcat"
    };
    if let Err(e) = tray.set_tooltip(Some(tip)) {
        eprintln!("[orbcat] 更新托盘提示失败: {e}");
    }

    // 只在睡眠时换图标；唤醒时回落到窗口默认图标（与启动时那个一致）
    let icon = if sleeping {
        tray_icon_from_dist("orb-sleep.png").or_else(|| app.default_window_icon().cloned())
    } else {
        app.default_window_icon().cloned()
    };
    if let Some(ic) = icon {
        if let Err(e) = tray.set_icon(Some(ic)) {
            eprintln!("[orbcat] 更新托盘图标失败: {e}");
        }
    }
}

/// 从外置前端资源里取一张托盘用图标。
///
/// 路径口径与 `disk_assets::resolve_dist_dir` 的**首选位置**一致：
/// exe 同目录的 `dist/`。找不到 / 解不开就返回 `None`（调用方回落默认图标）。
///
/// ⚠️ 为什么用 `image` crate 自己解码，而不是 `tauri::image::Image::from_bytes`：
/// 后者要求 tauri 打开 `image-png` feature（默认**没开**，否则编译报
/// "no function from_bytes"）。而 `image` 本来就是本项目的依赖
/// （`images.rs` 的缩略图靠它），复用它等于零新增依赖；
/// 反过来为一张托盘图标给 tauri 加解码器是净增编译成本。
fn tray_icon_from_dist(file: &str) -> Option<tauri::image::Image<'static>> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("dist").join("orb").join(file);
    let bytes = std::fs::read(&path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(tauri::image::Image::new_owned(img.into_raw(), w, h))
}

#[tauri::command]
fn orb_ui_action(app: tauri::AppHandle, action: String) -> Result<(), String> {
    orb_ui_apply(&app, &action)
}

#[tauri::command]
fn orb_ui_state() -> OrbUi {
    orb_ui_snapshot()
}

/// 「用户又启动了一次」的**待消费**标记。
///
/// ## 为什么需要它（2026-09-30 修 bug）
///
/// 第二实例启动的正确语义是"**我要用它**"，所以应该把面板打开。但这里有个
/// 时序缺口：唤醒事件可能在**前端还没就绪**时到达（WebView 尚在加载、或
/// `applyMode` 里的 `await invoke("set_window_mode")` 还没回来）。
/// 如果只发一次 `emit`，那次事件就丢了 —— 而第二实例已经退出，
/// 用户看到的就是**完全没反应**。
///
/// 所以做成"标记 + 事件"双通道：事件是快路径（前端已在场时立刻展开），
/// 标记是兜底（前端 `boot` 时拉一次，迟到的信号照样生效）。
/// `take()` 语义保证只生效一次，不会在每次刷新时反复弹面板。
static OPEN_PANEL_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 「前端已经接手这次开面板」的确认标记。
///
/// 与 `OPEN_PANEL_PENDING` 成对：pending 是"后端想开"，这个是"前端真的接了"。
/// 自愈定时器只认后者 —— 绝不能拿 pending 反推：pending 的 `swap(false)`
/// 只表示"标记被读过一次"，读它的人可能是任何一次 boot，
/// 而且定时器若用 `swap(true)` 去探测，反而会把标记重新点亮、让前端再弹一次。
static PANEL_OPEN_HANDLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 取走「请打开面板」标记。前端 `boot` 时调一次；有则返回 true 并清掉。
///
/// ⚠️ 返回 true 时**同时**记下"前端已接手"（见 [`PANEL_OPEN_HANDLED`]），
/// 供自愈定时器判断前端到底在不在场。
#[tauri::command]
fn take_open_panel_request() -> bool {
    let pending = OPEN_PANEL_PENDING.swap(false, std::sync::atomic::Ordering::Relaxed);
    if pending {
        PANEL_OPEN_HANDLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pending
}

/// 前端**确认**它已经接手了「第二实例要求开面板」这件事。
///
/// 自愈定时器靠这个标记判断"前端到底在不在场"：不确认就退回球态。
/// 这样即使前端启动失败，用户看到的也只是"点击没反应"，而不是一个
/// 面板尺寸的窗口里画着球（那看起来像界面坏了）。
#[tauri::command]
fn confirm_panel_opened() {
    PANEL_OPEN_HANDLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 「用户又启动了一次 exe」→ 唤醒 + **展开面板**。
///
/// ## 为什么不是只唤醒（这是用户报的 bug 的根因）
///
/// 旧实现只调 `orb_ui_apply("wake")`。如果球当时**没在睡**，这就是个纯 no-op；
/// 就算在睡，也只是"变回普通球"。用户点桌面/开始菜单图标的意图是"我要用它"，
/// 而看到的可能只是屏幕角落一个 84px 的球动了一下 —— 主观上就是**点击无效**。
///
/// 新行为：唤醒 → 展开面板 → 抢前台焦点（面板不抢焦点的话输入框拿不到
/// OS 级焦点，打字会落到别的应用里，见 `win32::force_foreground`）。
fn wake_and_open_panel(app: &tauri::AppHandle) {
    if let Err(e) = orb_ui_apply(app, "wake") {
        eprintln!("[orbcat] ⚠️ 第二实例唤醒失败: {e}");
    }

    // 消费标记：这次由我们自己展开，前端 boot 时就不该再弹一次
    OPEN_PANEL_PENDING.store(false, std::sync::atomic::Ordering::Relaxed);
    // 复位"前端已接手" —— 这次是后端直接开的窗，等前端来认领
    PANEL_OPEN_HANDLED.store(false, std::sync::atomic::Ordering::Relaxed);

    let Some(win) = app.get_webview_window("orb") else {
        eprintln!("[orbcat] ⚠️ 找不到 orb 窗口，无法展开面板");
        return;
    };

    // 球可能被最小化过 —— 先确保它可见，否则 set_bounds 之后也看不到
    let _ = win.show();

    #[cfg(windows)]
    {
        if let Err(e) = apply_mode(&win, "panel") {
            eprintln!("[orbcat] ⚠️ 第二实例展开面板失败: {e}");
        }
    }

    // 前端据此切视图（与球菜单点「对话」同一条路径）
    if let Err(e) = app.emit("orb-open-panel", ()) {
        eprintln!("[orbcat] ⚠️ 广播 orb-open-panel 失败: {e}");
    }

    // ⚠️ **自愈兜底**（2026-09-30）：上面已经把窗口改成面板尺寸，而**换 DOM 是
    //    前端的事**。如果前端此刻还没加载出来（WebView 冷启、页面被 CSP 拦、
    //    或它自己崩了），`emit` 就没人接 —— 屏幕上会留一个面板大小的窗口，
    //    里面画着 84px 的球，看起来像"界面坏了"。
    //
    //    这里等一小会儿看前端有没有接手（它接到信号后会调
    //    `take_open_panel_request`，或直接调 `set_window_mode`）。
    //    两边都没有 → 退回球态：宁可"点击没反应"，也不要留一个坏掉的窗口。
    //    前端正常在场时这段永远不会触发。
    {
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2_500));
            let handled = PANEL_OPEN_HANDLED.load(std::sync::atomic::Ordering::Relaxed);
            if handled {
                return;
            }
            // 闭包要 'static，`handle` 已被 run_on_main_thread 借走 ——
            // 再 clone 一份专供闭包内使用（也要拿 data_dir 写 diag）
            let h2 = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                let Some(w) = h2.get_webview_window("orb") else {
                    return;
                };
                #[cfg(windows)]
                {
                    if let Err(e) = apply_mode(&w, "orb") {
                        eprintln!("[orbcat] ⚠️ 前端未就绪，退回球态失败: {e}");
                    }
                }
                #[cfg(not(windows))]
                let _ = &w;
                diag_write(
                    &resolve_data_dir(&h2),
                    "[wake] 前端 2.5s 未接住开面板信号 → 已退回球态（自愈）",
                );
            });
        });
    }

    diag_write(&resolve_data_dir(app), "[wake] 第二实例：已展开面板并广播 orb-open-panel");
}

// ---------------------------------------------------------------------------
// 系统托盘
// ---------------------------------------------------------------------------

/// 注册托盘图标 + 右键菜单（唤醒 / 睡眠 / 退出）。
///
/// 不改 skipTaskbar / TOOLWINDOW：悬浮球仍不该占任务栏或 Alt+Tab。
/// 托盘是用户**唯一**能稳定找到它的地方（skipTaskbar 让它在任务栏隐身），
/// 所以这里必须是一个完整的控制入口，而不只是"退出"。
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let wake = MenuItem::with_id(app, "wake", "唤醒", true, None::<&str>)?;
    let sleep = MenuItem::with_id(app, "sleep", "睡眠", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&wake, &sleep, &quit])?;

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
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => {
                eprintln!("[orbcat] 用户从托盘退出");
                app.exit(0);
            }
            // 球状态动作与球的右键菜单走**同一个函数**，
            // 保证"托盘唤醒"和"球菜单睡眠"语义完全一致
            act @ ("wake" | "sleep") => {
                if let Err(e) = orb_ui_apply(app, act) {
                    eprintln!("[orbcat] ⚠️ 托盘「{act}」失败: {e}");
                }
            }
            _ => {}
        })
        // 左键单击托盘图标 = 唤醒。球睡着时这是最顺手的恢复方式。
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Err(e) = orb_ui_apply(tray.app_handle(), "wake") {
                    eprintln!("[orbcat] ⚠️ 托盘左键唤醒失败: {e}");
                }
            }
        })
        .build(app)?;

    eprintln!("[orbcat] 托盘图标已注册（唤醒 / 睡眠 / 退出）");
    Ok(())
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 平台专属命令占位（非 Windows）
// ---------------------------------------------------------------------------

/// 隐形桌面相关的三个命令在非 Windows 上的**占位实现**。
///
/// ## 为什么需要它们（而不是从注册表里剔除）
///
/// `tauri::generate_handler![]` 的参数列表是**字面标识符**，既不允许写
/// `#[cfg]`，也不会展开嵌套宏（试过 `platform_commands!()`，报
/// `error: expected ','`）。所以只要注册表里写了 `bg_desk_status`，
/// 它就必须在这两个平台上有一个可解析的符号。
///
/// 而 Windows 版那三个函数挂了 `#[cfg(windows)]` —— 因为它们直接调
/// `win32desk`，而 `win32desk` 整体只存在于 Windows（`CreateDesktopW` 是
/// Win32 独有）。于是在 macOS 上就炸了：
///
/// ```text
/// error: cannot find macro `__cmd__bg_desk_status` in this scope
/// error: cannot find macro `__tauri_command_name_bg_desk_status` in this scope
/// ```
///
/// 这是 2026-10-10 由 GitHub Actions 的 `macos-14` runner 抓出来的 ——
/// 本机 Windows 上 `cargo check` 永远看不到它。
///
/// ## 语义选择：返回「不可用」而不是静默成功
///
/// 前端调用这三个命令时拿到明确的错误信息，比拿到一个假的 `false` / `null`
/// 要好得多 —— 用户打开设置页会看到「隐形桌面仅支持 Windows」，
/// 而不是困惑于「为什么开关打不开也不报错」。
#[cfg(not(windows))]
mod platform_command_stubs {
    use serde_json::Value;

    const MSG: &str = "隐形桌面仅支持 Windows（macOS 无此机制）";

    #[tauri::command]
    pub fn bg_desk_status() -> Value {
        serde_json::json!({
            "available": false,
            "enabled": false,
            "reason": MSG,
        })
    }

    #[tauri::command]
    pub fn bg_desk_set(_enabled: bool) -> Result<bool, String> {
        Err(MSG.to_string())
    }

    #[tauri::command]
    pub fn bg_desk_shot(_title: Option<String>, _out_dir: String) -> Result<String, String> {
        Err(MSG.to_string())
    }
}

#[cfg(not(windows))]
use platform_command_stubs::{bg_desk_set, bg_desk_shot, bg_desk_status};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // --- 单实例锁（必须最先做，早于任何窗口创建）---
    // 悬浮球是 always-on-top + skipTaskbar：跑起两个实例 = 两个球叠着、
    // 两个托盘图标、两份 settings.json 互相覆盖，而用户从任务栏找不到它们。
    // 第二个实例在这里直接退出，并顺手把第一个实例唤醒。
    match single_instance::acquire() {
        single_instance::Acquired::AlreadyRunning => {
            // 走 eprintln 而不是 diag.log：这个进程**没有 data_dir 上下文**，
            // 也算不出与主实例一致的路径（env 可能只对主实例有效）。
            // 但至少要让"点了图标没反应"在控制台/调试器里有据可查。
            eprintln!(
                "[orbcat] 已有实例在运行 → 已发唤醒信号（主实例会展开面板），本次启动退出"
            );
            return;
        }
        single_instance::Acquired::Primary => {}
    }

    // --- 前端资源外置（见 disk_assets.rs）---
    // exe 同目录若有 dist/，优先从磁盘读 → 改前端只需 `npm run build`，不必重编 Rust。
    // 用官方 `Context::set_assets`，窗口 URL 与 IPC 机制完全不变。
    let mut context = tauri::generate_context!();
    match disk_assets::resolve_dist_dir() {
        Some(dist) => {
            eprintln!("[orbcat] 前端资源目录: {}", dist.display());
            // 先用 Noop 换出嵌入资源，再把它作为 fallback 装进 DiskAssets
            let embedded = context.set_assets(Box::new(disk_assets::NoopAssets));
            context.set_assets(Box::new(disk_assets::DiskAssets::new(dist, embedded)));
        }
        None => {
            eprintln!("[orbcat] 未找到外部 dist/，使用内置占位页（stub-dist）");
        }
    }

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
            // 把前端资源来源写进诊断日志：出「占位页」时一眼看出是 dist 没放对
            diag_write(&data_dir, &disk_assets::dist_dir_status());

            // 开机自启校准：若之前的自启项指向 debug exe，release 已存在则改指 release
            #[cfg(windows)]
            autostart::sync_to_release();

            // 前台应用轮询线程（记录「用户上一个在看的应用」，对话时注入上下文）
            context::spawn_watcher();

            let mut gate = permission::load_gate(&data_dir, &app_dir);
            // 拼装项目的文件授权**增量**（最长前缀加法，canonicalize 后加）。
            //
            // ⚠️ 三条纪律，与 spec S2.6 一致：
            //   1. **只做加法** —— 用户在设置里的规则一条都不删、不改；
            //   2. 判定前 canonicalize（决策 7 原样适用）；
            //   3. 只影响**新轮次**的判定，`permissions.json` **不写回** ——
            //      切走项目即恢复（这也是"切项目不改历史会话"的一部分）。
            for (path, access, label) in project::extra_file_rules(&data_dir) {
                eprintln!(
                    "[orbcat] 项目授权路径: {} => {:?} ({label})",
                    path.display(),
                    access
                );
                gate.add_rule(&path, access, label);
            }
            // 激活项目绑定目录的动态授权（用户 2026-10-07 定案：项目激活期间
            // 默认读写其绑定目录；没绑目录就没有）。只活内存，切项目时重建。
            if let Some((path, name)) = active_project_dir(&data_dir) {
                gate.add_rule(
                    &path,
                    permission::Access::ReadWrite,
                    format!("{}{}」", permission::PROJECT_DIR_LABEL_PREFIX, name),
                );
                eprintln!(
                    "[orbcat] 项目目录授权: {} => readwrite（项目「{name}」）",
                    path.display()
                );
            }
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
            let mut registry = if servers.is_empty() {
                eprintln!("[orbcat] MCP 未配置（设置页可添加多个 server）");
                mcp::ToolRegistry::new("")
            } else {
                eprintln!(
                    "[orbcat] MCP servers: {}（后台拉取工具清单中…）",
                    servers.len()
                );
                mcp::ToolRegistry::with_servers(&servers)
            };
            // 能力索引（工具名 + 一句话用途）落盘到 data_dir：`agent.rs` 组装
            // system prompt 时只有 data_dir，读不到注册表 —— 这份缓存就是两者之间
            // 唯一的桥。必须在**后台拉取之前**注入，否则第一次拉取白写（含此后
            // 每次 apply_snapshot 都写不出去，索引永远为空）。
            registry.set_index_cache_dir(&data_dir);

            let shared = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
            let data_dir_for_mcp = data_dir.clone();
            if !servers.is_empty() {
                let bg = shared.clone();
                tauri::async_runtime::spawn(async move {
                    // 锁外做网络 IO，锁内只落内存账。
                    let clients = bg.lock().await.clients_clone();
                    let snap = mcp::ToolRegistry::fetch_snapshot(&clients).await;
                    // 启动即按设置 + 当前模式恢复组开关（决策：**默认全给**，
                    // 模式再收窄一层 —— 见 `modes.rs` 顶部）
                    let st = config::load_settings(&data_dir_for_mcp);
                    let allowed = allowed_for_dir_of(&data_dir_for_mcp, &bg);
                    let n = bg
                        .lock()
                        .await
                        .apply_snapshot_with_settings(snap, &st, Some(allowed));
                    eprintln!("[orbcat] MCP 活跃工具组：{n} 个（按设置 + 模式恢复）");
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

            // --- 唤醒监听（单实例锁的另一半）---
            // 用户再启动一次 exe → 第二实例 SetEvent → 这里收到 → **展开面板**。
            // 双击桌面/开始菜单图标的意图是"我要用它"，不是"把球从睡眠里叫醒"；
            // 只唤醒的话，球本来醒着时就是纯 no-op —— 用户看到的就是"点击无效"。
            {
                let handle = app.handle().clone();
                single_instance::spawn_wake_listener(move || {
                    eprintln!("[orbcat] 收到第二实例的唤醒信号 → 展开面板");
                    // 先置兜底标记：前端可能还没就绪，那时 emit 会丢（见标记的文档）
                    OPEN_PANEL_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
                    let h = handle.clone();
                    // 监听线程不是主线程 —— 窗口操作丢回主线程执行
                    let _ = handle.run_on_main_thread(move || {
                        wake_and_open_panel(&h);
                    });
                });
            }

            // --- 模型管理窗口：启动后预热（治首次点开 8.4s 冷启）---
            //
            // 放在这里（setup 尾部）而不是更早：必须等 orb 的 webview 起稳，
            // 否则两个 webview 抢 WebView2 环境（2026-09-29 实测的坑）。
            // 具体延迟与"为什么不能立刻建"见 `spawn_models_preheat` 的文档。
            {
                let handle = app.handle().clone();
                let dir = app.state::<AppState>().data_dir.clone();
                spawn_models_preheat(handle, dir);
            }

            // --- 诊断开关：启动后自动弹一次「模型管理」窗口 ---
            //
            // 设了 `ORBCAT_AUTO_OPEN_MODELS` 就在 2 秒后自动跑一遍打开流程，
            // 结果写进 `agent-data/diag.log`。用途：**无头验证** ——
            // 不用点屏幕、也不用看窗口，读日志就能判断窗口/页面是否正常。
            if std::env::var("ORBCAT_AUTO_OPEN_MODELS").is_ok() {
                let handle = app.handle().clone();
                let dir = app.state::<AppState>().data_dir.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(2000));
                    diag_write(&dir, "[auto] 自动触发 models_window_open");
                    let d2 = dir.clone();
                    let r = tauri::async_runtime::block_on(models_window_open_inner(handle, d2));
                    diag_write(&dir, &format!("[auto] 结果 = {r:?}"));
                });
            }

            // --- 诊断开关：自动跑一遍「睡眠 → 唤醒」 ---
            //
            // 与 `ORBCAT_AUTO_OPEN_MODELS` 同一套路：**无头验证**。
            // 睡眠/唤醒平时只能点托盘或球菜单触发，自动化点不到系统托盘，
            // 所以留这个开关把链路跑一遍，结果全落 `diag.log`。
            if std::env::var("ORBCAT_AUTO_SLEEP").is_ok() {
                let handle = app.handle().clone();
                let dir = app.state::<AppState>().data_dir.clone();
                std::thread::spawn(move || {
                    for (delay_ms, act) in [(2500u64, "sleep"), (3000u64, "wake")] {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        diag_write(&dir, &format!("[auto] 自动触发球状态: {act}"));
                        let h = handle.clone();
                        let d = dir.clone();
                        let act_owned = act.to_string();
                        let _ = handle.run_on_main_thread(move || {
                            let r = orb_ui_apply(&h, &act_owned);
                            diag_write(&d, &format!("[auto] {act_owned} 结果 = {r:?}"));
                        });
                    }
                });
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
            distill_session_open,
            mem_project_set_source,
            mem_project_archive,
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
            life_status,
            life_set_enabled,
            life_refresh,
            life_config_path,
            set_blur_collapse,
            set_exec_trust,
            modes_get,
            bg_desk_status,
            bg_desk_set,
            bg_desk_shot,
            foreground_context,
            foreground_history,
            context_capability,
            request_context_permission,
            open_accessibility_settings,
            image_thumb,
            skills_list,
            project_list,
            project_activate,
            project_save,
            project_delete,
            project_read,
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
            recovery_state,
            recovery_restore_backup,
            recovery_restore_trash,
            recovery_prune,
            recovery_diagnostic,
            mcp_status,
            mcp_refresh,
            mcp_set_url,
            mcp_servers_list,
            mcp_servers_save,
            mcp_load_group,
            mcp_unload_group,
            mcp_set_group_enabled,
            mcp_group_settings,
            mcp_set_all_groups,
            model_group_names,
            model_group_rename,
            models_set_group_key,
            models_remove_group,
            app_quit,
            autostart_status,
            autostart_set,
            exe_path,
            set_window_mode,
            raise_orb,
            orb_ui_action,
            orb_ui_state,
            take_open_panel_request,
            confirm_panel_opened,
            models_window_open,
            models_window_close,
            diag_log,
        ])
        .on_window_event(|window, event| {
            // 「模型管理」窗口点 ✕ / 系统关闭 = **隐藏不销毁**（再开秒出），
            // 顺手把用户拖出来的尺寸记回 settings —— 下次打开还是这个大小。
            if window.label() == "models" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let app = window.app_handle();
                    let dir = app.state::<AppState>().data_dir.clone();
                    if let Some(win) = app.get_webview_window("models") {
                        persist_models_window_size(&dir, &win);
                        let _ = win.hide();
                    }
                }
            }
        })
        .run(context)
        .expect("error while running tauri application");
}
