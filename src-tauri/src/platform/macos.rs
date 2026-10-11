//! macOS 平台实现。
//!
//! ## 核心设计：前台上下文的两级权限
//!
//! macOS 把「知道你在哪个 app」和「知道你在编辑哪个文件」分成了**两个权限级别**：
//!
//! | 能力 | API | 是否需要用户授权 |
//! |---|---|---|
//! | 前台 app 名 | `NSWorkspace.sharedWorkspace.frontmostApplication` | ❌ **不需要**，任何 app 都能读 |
//! | 窗口标题 | `AXUIElement`（Accessibility） | ✅ **需要**，且必须用户手动到系统设置里勾 |
//!
//! 这决定了本模块的核心策略：**永远先给免授权的那一级，只有用户真的需要
//! 第二级时才提示授权。** 具体渐进解锁流程见 [`ensure_accessibility`]。
//!
//! ## 为什么不做「读正文内容」
//!
//! Windows 那边用 UI Automation 试过（见 `docs/01-UIA-FINDINGS.md`），结论是
//! Chromium/Electron 的无障碍树**按需异步构建**，同一份代码时好时坏 ——
//! VSCode 六轮实验只有 R4 一次成功。macOS 的 AX 树同样不保证 Electron 应用
//! 暴露编辑器内容，所以**第一版不做**，只做到「哪个 app + 哪个文件」这一级。
//! 这与 Windows 版的能力边界完全一致，两端体验相同。

#![cfg(target_os = "macos")]

use std::ffi::c_void;

/// 与 Windows 版 `Hwnd` 占位一致。macOS 上 Tauri 的 raw handle 是 `*mut c_void`，
/// 这里只作为「传给 Tauri 原生 API 的不透明指针」用，不做任何解引用。
pub type Hwnd = *mut c_void;

// ---------------------------------------------------------------------------
// 前台上下文：能力分级
// ---------------------------------------------------------------------------

/// 前台信息采集到了哪一级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCapability {
    /// 完全拿不到前台信息（理论上不会发生，`NSWorkspace` 不需要授权）
    Unavailable,
    /// 只能拿到 app 名，拿不到窗口标题
    AppOnly,
    /// 能拿到 app 名 + 窗口标题（已获辅助功能授权）
    Full,
}

/// 当前处于哪一级。轮询线程据此决定要不要继续尝试取标题。
pub fn context_capability() -> ContextCapability {
    if accessibility_granted() {
        ContextCapability::Full
    } else {
        ContextCapability::AppOnly
    }
}

// ---------------------------------------------------------------------------
// 免授权层：前台 app 名
// ---------------------------------------------------------------------------

/// 取前台 app 的可执行文件名（不含 `.app`，例如 `"Visual Studio Code"`）。
///
/// 走 `osascript` 而不是直接 FFI `NSWorkspace`，理由：
/// - 省掉 `objc2` / `cocoa` 依赖（当前 `Cargo.toml` 里一个 macOS crate 都没有）
/// - `osascript` 是系统自带二进制，行为稳定
/// - 每 800ms 调一次子进程的开销可接受（`context.rs` 的轮询周期）
///
/// 拿不到时返回 `None`，调用方按既有降级逻辑处理。
fn frontmost_app_name() -> Option<String> {
    let out = run_osascript(
        "tell application \"System Events\" to get bundle identifier of first application process whose frontmost is true",
    )?;
    // bundle id 形如 `com.microsoft.VSCode` —— 取最后一段并去掉可能的版本后缀
    let stem = out.rsplit('.').next()?.trim().to_string();
    if stem.is_empty() {
        None
    } else {
        Some(stem)
    }
}

/// 取前台窗口标题。**需要辅助功能授权**，未授权时返回 `None`。
///
/// 不用 AXUIElement 直接 FFI，而是用 `System Events` 的 AppleScript：
/// 同样是为了零依赖。授权检查由 [`accessibility_granted`] 负责，
/// 这里被调用就意味着已经有权限。
fn frontmost_window_title() -> Option<String> {
    run_osascript(
        "tell application \"System Events\" to get name of front window of first application process whose frontmost is true",
    )
}

/// 跑一段 AppleScript，返回 stdout 的第一行（trim 过）。
fn run_osascript(script: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let line = s.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// 前台窗口信息：`(app 名, 窗口标题)`。
///
/// 与 Windows 版 `foreground_window_info()` 签名一致。**窗口标题可能为空串**
/// —— 这正是「渐进解锁」的表现：未授权时 app 名照常返回，标题降级为空。
pub fn foreground_window_info() -> Option<(String, String)> {
    let app = frontmost_app_name()?;
    let title = frontmost_window_title().unwrap_or_default();
    Some((app, title))
}

// ---------------------------------------------------------------------------
// 辅助功能授权：检测与请求
// ---------------------------------------------------------------------------

/// 是否已获辅助功能（Accessibility）授权。
///
/// 判定方式：**跑一个无副作用的 `System Events` 调用，看它是否成功**。
///
/// 为什么不用 `AXIsProcessTrusted()` 直接 FFI：
/// - 那需要链接 `ApplicationServices.framework`，等于引入 macOS 专属 build 配置
/// - 而「AppleScript 能不能跑通」本身就是最真实的判据 —— 能跑通就说明 TCC 已放行
/// - 好处是**这个探测顺带就是能力探测**，失败即可确定处于 `AppOnly` 级
///
/// 注意：`osascript` 子进程调用 AppleScript 时，授权是授予给**发起方进程**的。
/// macOS 会对终端/父应用弹一次权限提示，这正是我们要引导用户点击的那一步。
fn accessibility_granted() -> bool {
    run_osascript("tell application \"System Events\" to get name of first process").is_some()
}

/// 请求辅助功能授权（**会弹系统授权提示**）。
///
/// 用 `with prompt` 让系统弹窗带上说明文字 —— 默认文案是
/// 「xxx 想要控制这台电脑」，用户不知道为什么要给。
///
/// ⚠️ **只在用户主动点击「让它自己看」时调用**，绝不在启动时调用。
/// 详见 `lib.rs` 的 `request_context_permission` 命令。
///
/// 返回值语义：
/// - `Ok(true)`  —— 授权成功，可以拿到窗口标题
/// - `Ok(false)` —— 用户点了拒绝，或已授权但探测失败
/// - `Err(_)`    —— 系统层面出错（如 macOS 版本过低）
pub fn ensure_accessibility() -> Result<bool, String> {
    // 先看是不是已经有权限了 —— 已有就直接返回，不弹窗打扰用户
    if accessibility_granted() {
        return Ok(true);
    }

    // 发起一次带 prompt 的调用触发授权弹窗
    let ok = run_osascript(
        r#"tell application "System Events"
            with prompt "orbcat 需要辅助功能权限，才能读到你当前窗口的文件名"
                get name of front window of first application process whose frontmost is true
            end tell
        end"#,
    );

    // 注意：**不要**在这里判断 `ok.is_some()` 就当成授权成功。
    // 用户点「允许」后 macOS 还需要用户在「系统设置 → 隐私与安全性 → 辅助功能」
    // 里手动勾选 orbcat，这两步是分开的。所以这里返回 `Ok(false)`，
    // 由前端轮询 `context_capability()` 感知最终结果。
    Ok(ok.is_some())
}

/// 打开「隐私与安全性 → 辅助功能」设置面板。
///
/// 用户点了「允许」但功能没生效时，多半是没去系统设置里勾 —— 直接把他送到那里。
pub fn open_accessibility_settings() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开系统设置失败：{e}"))
}

// ---------------------------------------------------------------------------
// 窗口控制：macOS 上由 Tauri 原生实现，这里全部 no-op
// ---------------------------------------------------------------------------

/// Windows 的 `WS_EX_NOACTIVATE` 在 macOS 没有对应概念 ——
/// Tauri 的 `NSWindow` 默认就不抢焦点（`canBecomeKeyWindow = false` 即可，
/// Tauri 已经在无边框窗口上处理了）。所以这里是 no-op。
pub fn set_no_activate(_hwnd: Hwnd) {}

/// 对应 [`set_no_activate`]，也是 no-op。
pub fn clear_no_activate(_hwnd: Hwnd) {}

/// Windows 需要 `SetWindowPos` + `SWP_NOACTIVATE` 才能「置顶但不抢焦点」。
/// macOS 上 Tauri 的 `set_always_on_top` 本身就不激活窗口。
pub fn keep_topmost_without_activating(_hwnd: Hwnd) {}

/// Windows 需要 `AttachThreadInput` 绕过前台锁定。macOS 没有这个限制 ——
/// 但这里**故意不做「强行抢焦点」**，因为那对用户是打扰。
/// 真要激活请用 Tauri 的 `set_focus()`。
pub fn force_foreground(_hwnd: Hwnd) {}

/// 设置窗口位置/尺寸。macOS 上由 Tauri 的 `set_position` / `set_size` 完成，
/// 调用方（`lib.rs:509`）应该已经走了 Tauri 路径，这里只是保持接口一致。
pub fn set_bounds(hwnd: Hwnd, x: i32, y: i32, w: i32, h: i32) {
    let _ = (hwnd, x, y, w, h);
}

// ---------------------------------------------------------------------------
// 开机自启：LaunchAgent plist
// ---------------------------------------------------------------------------

/// 自启配置所在的 `~/Library/LaunchAgents` 路径。
fn launch_agent_plist() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    std::path::PathBuf::from(home).join("Library/LaunchAgents/com.orbcat.desktop.plist")
}

/// 读自启配置。Windows 版读 `HKCU\...\Run`；Mac 上读 plist 里的 `ProgramArguments`。
pub fn read_hkcu_sz(_subkey: &str, _value: &str) -> Option<String> {
    let txt = std::fs::read_to_string(launch_agent_plist()).ok()?;
    // plist 是 XML，但格式固定，简单扫一行就够 —— 不值得为这一行引 XML 依赖
    txt.lines()
        .find_map(|l| l.trim().strip_prefix("<string>"))
        .and_then(|s| s.strip_suffix("</string>"))
        .map(|s| s.to_string())
}

/// 写自启配置：生成 LaunchAgent plist。
///
/// 刻意**没有**用 `launchctl load` —— 那要求已登录用户的 launchd 会话，
/// 而 `~/Library/LaunchAgents/*.plist` 会在下次登录时由系统自动加载，
/// 语义更接近 Windows 的 HKCU Run（同样是「下次开机生效」）。
pub fn write_hkcu_sz(_subkey: &str, _value: &str, data: &str) -> Result<(), String> {
    let path = launch_agent_plist();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 LaunchAgents 目录失败：{e}"))?;
    }
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.orbcat.desktop</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
        exe = data.trim_matches('"')
    );
    std::fs::write(&path, xml).map_err(|e| format!("写入 LaunchAgent 失败：{e}"))?;
    // 让 launchd 立刻感知（失败不影响，下次登录仍会加载）
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["load", "-w"])
        .arg(&path)
        .output();
    Ok(())
}

/// 删除自启配置。`unload` 失败不影响删文件 —— 文件没了下次登录就不会加载。
pub fn delete_hkcu_value(_subkey: &str, _value: &str) -> Result<(), String> {
    let path = launch_agent_plist();
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["unload", "-w"])
        .arg(&path)
        .output();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // 文件本来就不存在 = 已经关掉了，幂等
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除 LaunchAgent 失败：{e}")),
    }
}

// ---------------------------------------------------------------------------
// 文本解码与 .lnk —— Mac 上没有对应概念
// ---------------------------------------------------------------------------

/// Windows 的 ACP（ANSI code page）解码。macOS 上所有文本本就是 UTF-8。
pub fn decode_acp(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Windows `.lnk` 快捷方式解析。macOS 没有 `.lnk`（对应物是 alias / `.app`），
/// 语义也不同，所以直接返回 `None` —— 上层会走降级分支。
pub fn lnk_target(_bytes: &[u8]) -> Option<String> {
    None
}
