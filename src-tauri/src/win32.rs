//! Windows 原生 API 门面 —— **转发层，真实实现在 [`platform`] 下**。
//!
//! ## 为什么保留这个文件
//!
//! 2026-10-10 之前，本文件就是完整的 Win32 FFI 实现。抽出平台层后它退化成
//! 一层转发，好处是 **37 处 `crate::win32::xxx` 调用点一行都不用改**
//! （分布在 `lib.rs` / `tools.rs` / `context.rs`）。
//!
//! ## 新增代码应该写在哪
//!
//! - Windows 专属实现 → `platform/windows.rs`
//! - macOS 实现 → `platform/macos.rs`
//! - 其他平台 → `platform/stub.rs`
//!
//! 新增函数时**三个文件都要有同名同签名的实现**，否则跨平台编译会挂。
//! 函数清单以 `platform/mod.rs` 顶部的对照表为准。

// 只有 Windows 上才有真实的 Win32 实现；在 Mac/Linux 上整个模块不存在。
#[cfg(windows)]
#[allow(unused_imports)] // Hwnd 只被部分调用点用（如 `lib.rs` 的 set_bounds）
pub use crate::platform::{
    clear_no_activate, decode_acp, delete_hkcu_value, force_foreground, foreground_window_info,
    keep_topmost_without_activating, lnk_target, read_hkcu_sz, set_bounds, set_no_activate,
    write_hkcu_sz, Hwnd,
};

// 非 Windows 平台：提供一组 no-op 占位，保证 `crate::win32::xxx` 的调用点
// 依然能编译（语义是「这个能力在本平台不存在」，上层会走降级分支）。
#[cfg(not(windows))]
#[allow(unused_imports)]
pub use crate::platform::{
    clear_no_activate, decode_acp, delete_hkcu_value, force_foreground, foreground_window_info,
    keep_topmost_without_activating, lnk_target, read_hkcu_sz, set_bounds, set_no_activate,
    write_hkcu_sz, Hwnd,
};
