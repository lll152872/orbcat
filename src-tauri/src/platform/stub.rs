//! 非 Windows / 非 macOS 平台的占位实现（Linux、BSD 等）。
//!
//! ## 为什么需要它
//!
//! `platform/mod.rs` 会对每个平台做一次 `pub use`，如果某个 target 没有对应实现，
//! 编译期就会报「找不到模块」。有了这个 stub，任何 target 都能 `cargo check` 通过，
//! 交叉编译验证（`cargo check --target x86_64-unknown-linux-gnu`）不会因为
//! 缺模块而卡住。
//!
//! ## 行为
//!
//! **全部 no-op / 返回空。** 这与 `context.rs` 既有的降级原则一致：
//! 「采集失败一律降级，绝不阻塞对话主流程」。orbcat 在这些平台上能跑，
//! 只是没有「知道你在用哪个 app」的能力。
//!
//! ⚠️ 注意：Linux 上真正的无障碍方案是 AT-SPI2（DOGSCOOP），
//! 若将来要支持 Linux 桌面，应该在 `platform/linux.rs` 里接 AT-SPI，
//! 而不是复用这个 stub。

#![cfg(not(any(windows, target_os = "macos")))]

use std::ffi::c_void;

pub type Hwnd = *mut c_void;

/// 拿不到前台信息。
pub fn foreground_window_info() -> Option<(String, String)> {
    None
}

/// 见 [`crate::platform::ContextCapability`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCapability {
    Unavailable,
    Full,
}

pub fn context_capability() -> ContextCapability {
    ContextCapability::Unavailable
}

pub fn set_no_activate(_hwnd: Hwnd) {}
pub fn clear_no_activate(_hwnd: Hwnd) {}
pub fn keep_topmost_without_activating(_hwnd: Hwnd) {}
pub fn force_foreground(_hwnd: Hwnd) {}
pub fn set_bounds(_hwnd: Hwnd, _x: i32, _y: i32, _w: i32, _h: i32) {}

/// 无自启配置可读。
pub fn read_hkcu_sz(_subkey: &str, _value: &str) -> Option<String> {
    None
}

/// Linux 上应改用 XDG autostart（`~/.config/autostart/*.desktop`），第一版不支持。
pub fn write_hkcu_sz(_subkey: &str, _value: &str, _data: &str) -> Result<(), String> {
    Err("当前平台暂不支持开机自启（Linux 版可改用 XDG autostart）".to_string())
}

/// 与 [`write_hkcu_sz`] 配套。
pub fn delete_hkcu_value(_subkey: &str, _value: &str) -> Result<(), String> {
    Ok(())
}

/// 非 Windows 平台文本本就是 UTF-8。
pub fn decode_acp(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `.lnk` 是 Windows 专属格式。
pub fn lnk_target(_bytes: &[u8]) -> Option<String> {
    None
}
