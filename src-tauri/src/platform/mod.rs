//! 平台抽象层 —— 隔离「只有 Windows 才有」的系统调用。
//!
//! ## 为什么需要这一层
//!
//! 2026-10-10 之前，`win32.rs` / `capture.rs` / `context.rs` / `single_instance.rs`
//! 直接写着 `extern "system"` + `#[link(name = "user32")]`，**只有 `win32desk.rs`
//! 挂了 `#![cfg(windows)]`**。结果是整个 crate 在 macOS 上**根本编译不过**。
//!
//! ## 设计原则
//!
//! 1. **facade 不改名**：`crate::win32::xxx` 的 37 处调用点一行都不用改，
//!    `win32.rs` 退化成薄转发层。
//! 2. **不追求功能对等**：Mac 上做不到的（隐藏桌面、窗口截图、`SendInput` 注入）
//!    直接返回 `None` / 友好错误，而不是硬凑一个半残实现。
//! 3. **降级不阻塞**：与 `context.rs` 既有的「采集失败一律降级，绝不阻塞对话主流程」
//!    保持一致 —— 平台能力缺失是常态，不是异常。
//!
//! ## 各能力在 Mac 上的支持度
//!
//! | 能力 | Windows | macOS | 说明 |
//! |---|---|---|---|
//! | 前台 app 名 | `GetForegroundWindow` | `NSWorkspace` | **免授权** |
//! | 前台窗口标题 | `GetWindowTextW` | AXUIElement | 需辅助功能授权，渐进解锁 |
//! | 窗口置顶/不抢焦点 | `WS_EX_NOACTIVATE` | Tauri 原生支持 | `set_no_activate` 在 Mac 上 no-op |
//! | 开机自启 | HKCU Run | LaunchAgent plist | |
//! | 单实例 | `CreateMutexW` | `flock` | `single_instance.rs` 另有实现 |
//! | 隐藏桌面子进程 | `CreateDesktopW` | ❌ 无等价 | `win32desk.rs` 整体标 unsupported |
//! | 窗口截图 | `PrintWindow` | ❌ 未接入 | 需要 ScreenCaptureKit + 录屏授权 |
//! | 输入注入 | `SendInput` | ❌ 未接入 | 第一版不做 |
//! | `.lnk` 解析 | ✅ | ❌ | Mac 用 `.app` / alias，语义不同 |

// Windows 上是真实实现，其他平台转发到对应实现
#[cfg(windows)]
pub mod windows;

// macOS 实现
#[cfg(target_os = "macos")]
pub mod macos;

// 其他平台（Linux 等）的占位实现，保证任何 target 都能编译
#[cfg(not(any(windows, target_os = "macos")))]
pub mod stub;

// ---------------------------------------------------------------------------
// 统一对外接口 —— 签名必须与 `platform/windows.rs` 里的同名函数**逐字一致**
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub use windows::{
    clear_no_activate, decode_acp, delete_hkcu_value, force_foreground, foreground_window_info,
    keep_topmost_without_activating, lnk_target, read_hkcu_sz, set_bounds, set_no_activate,
    write_hkcu_sz, Hwnd,
};

#[cfg(target_os = "macos")]
pub use macos::{
    clear_no_activate, decode_acp, delete_hkcu_value, force_foreground, foreground_window_info,
    keep_topmost_without_activating, lnk_target, read_hkcu_sz, set_bounds, set_no_activate,
    write_hkcu_sz, Hwnd,
};

#[cfg(not(any(windows, target_os = "macos")))]
pub use stub::{
    clear_no_activate, decode_acp, delete_hkcu_value, force_foreground, foreground_window_info,
    keep_topmost_without_activating, lnk_target, read_hkcu_sz, set_bounds, set_no_activate,
    write_hkcu_sz, Hwnd,
};

// ---------------------------------------------------------------------------
// 前台上下文的两级能力探测（macOS 专用，见 `macos.rs` 详解）
// ---------------------------------------------------------------------------

/// 前台信息采集到了哪一级。
///
/// Mac 上「知道你在哪个 app」和「知道你在编辑哪个文件」是**两个不同权限级别**，
/// 所以必须显式区分，否则调用方没法决定要不要提示用户授权。
#[cfg(target_os = "macos")]
pub use macos::{ContextCapability, context_capability};

/// 非 macOS 平台的能力常量（永远是最强的那一级）。
///
/// 之所以定义在 facade 里而不是复用 `windows::ContextCapability`（不存在），
/// 是为了让 `lib.rs::context_capability()` 命令**不需要任何 `cfg` 分支**
/// 就能 import 这个类型。
#[cfg(not(target_os = "macos"))]
pub use shim::ContextCapability;

/// 非 macOS 平台的能力常量
///
/// 这些类型只在 macOS 的 `lib.rs::context_capability()` 命令里被真正 import，
/// 非 macOS 平台走的是「无需授权」的硬编码分支，所以这里会是 dead code。
/// 用 `allow` 注明原因，避免后来人误以为是遗漏的接线。
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
mod shim {
    /// Windows 上 `GetForegroundWindow` 是普通 API，不存在权限分级。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ContextCapability {
        /// 拿不到前台信息（例如 `GetForegroundWindow` 返回空）
        Unavailable,
        /// 能拿到 app 名与窗口标题
        Full,
    }

    pub fn context_capability() -> ContextCapability {
        ContextCapability::Full
    }
}

