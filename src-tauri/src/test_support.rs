//! **测试支持入口** —— 刻意暴露给**集成测试**（`src-tauri/tests/`）用。
//!
//! ## 为什么需要这个模块
//!
//! 集成测试是**独立 crate**，编译时：
//!
//! ① **不带 `cfg(test)`** ⇒ 凡标了 `#[cfg(test)]` 的东西在 `tests/` 里**根本不存在**
//!    （典型例子：`ToolRegistry::from_tools_for_test`）；
//! ② `mcp` / `config` / `perm_request` 模块原本是**私有** ⇒ 这些类型在 `tests/` 里**不可达**。
//!
//! ⇒ 结果：集成测试**造不出**宿主的 MCP 注册表 ⇒ 「MCP 工具经宿主桥进 pi 工具表」
//! 这条**安全红线**只能靠 crate 内单测 + 真机证据，**没有端到端守卫用例**。
//!
//! ## 这里提供什么
//!
//! **不依赖 `cfg(test)`** 的构造入口：真连一个 stdio MCP server，走**生产用的那条路**
//! （`with_servers` → `clients_clone` → `fetch_snapshot` → `apply_snapshot_with_settings`）
//! ⇒ 造出「已连接、有工具」的注册表。
//!
//! ## ⚠️ 它会进正式二进制
//!
//! 本模块**没有** `#[cfg(test)]`（否则集成测试又看不见）⇒ 只放**纯构造**能力：
//! **不含任何测试逻辑、不含任何断言、不含任何测试专用状态**。

use std::collections::BTreeMap;

use crate::config::{AgentSettings, McpServerCfg, McpTransport};
use crate::mcp::ToolRegistry;

/// 造一个「已连上、带 MCP 工具」的注册表（**真连** 一个 stdio MCP server）。
///
/// 走的是**生产用的那条路**（与 `lib.rs` 里宿主自己那份 registry 同款）：
/// `with_servers` → `clients_clone` → `fetch_snapshot` → `apply_snapshot_with_settings`。
///
/// ## 为什么不用 `ToolRegistry::from_tools_for_test`
///
/// 它带 `#[cfg(test)]` ⇒ 集成测试（独立 crate、编译不带 `cfg(test)`）里**根本不存在**。
///
/// ## 参数
///
/// - `server_id` —— server id（= 组名；工具全名会是 `mcp__<server_id>__<tool>`）
/// - `command` / `args` —— stdio server 的可执行文件与 argv（**逐参传递，不经 shell**）
/// - `env` —— 附加环境变量（如 `MCP_TRACE_FILE`）
///
/// ## 失败怎么表现
///
/// server 连不上时 `fetch_snapshot` 返回 `Err` ⇒ `apply_snapshot_with_settings`
/// 内部会**清空组**（降级成「这个 server 没有工具」）⇒ 返回的注册表是**空的**、
/// 但**不 panic**。⇒ 调用方要判断「有没有工具」而不是只看 `Ok`。
pub async fn mcp_registry_with_stdio_server(
    server_id: &str,
    command: &str,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) -> Result<ToolRegistry, String> {
    let cfg = McpServerCfg {
        id: server_id.to_string(),
        url: String::new(),
        enabled: true,
        label: server_id.to_string(),
        transport: McpTransport::Stdio,
        command: command.to_string(),
        args,
        env,
    };
    let mut reg = ToolRegistry::with_servers(&[cfg]);
    let clients = reg.clients_clone();
    let snap = ToolRegistry::fetch_snapshot(&clients).await;
    let settings = AgentSettings::default();
    // ⚠️ `apply_snapshot_with_settings` 吃的是 `Result` —— 拉取失败它内部清空组
    //    （降级成「没有工具」），这里如实传下去，让调用方自己判断。
    reg.apply_snapshot_with_settings(snap, &settings);
    Ok(reg)
}
