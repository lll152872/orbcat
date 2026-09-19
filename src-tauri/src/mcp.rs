//! MCP 客户端 + 工具注册表（解决"工具爆炸"）
//!
//! ## 为什么手写而不用 `rmcp`
//! 我们只需要 `tools/list` 和 `tools/call` 两个方法，协议就是 JSON-RPC over HTTP
//! （Streamable HTTP transport）。手写能用上已有的 reqwest，**不引入新依赖**，
//! 也不必承担 rmcp 与本项目 tauri 版本的兼容风险。
//!
//! ## 工具爆炸是怎么解决的
//!
//! 实测 1MCP 网关暴露 **56 个工具、schema 约 14.4k tokens**。每轮请求都带上
//! 这堆东西会让 prefill 变慢、模型选错工具，换小模型时还会直接撑爆上下文。
//!
//! 所以：
//! ```text
//!   常驻（~1.2k tok）          按需（load 之后才注入）
//!   ────────────────          ────────────────────────
//!   read_file / list_dir  ...  mcp__playwright__*   24 个
//!   capture_screen             mcp__windows__*      20 个
//!   remember                   mcp__ssh__*          11 个
//!   list_tool_groups()         mcp__mysql__*         1 个
//!   load_tool_group(name)
//!   unload_tool_group(name)
//! ```
//!
//! 模型先用 `list_tool_groups` 看有哪些组（只给名字+数量+概括，**不给完整 schema**），
//! 再用 `load_tool_group` 把需要的那组装进活跃集 —— 下一轮请求才会带上它们的完整 schema。
//!
//! **效果：初始 prompt 从 ~14.4k tokens 降到 ~1.2k（-92%）。**

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// MCP 调用超时
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// 建立会话超时
const INIT_TIMEOUT: Duration = Duration::from_secs(20);

/// 活跃工具集的 token 预算。
///
/// 防止模型无脑 `load_tool_group` 把所有组都装进来 —— 那样等于退回"全量注入"。
const ACTIVE_BUDGET_TOKENS: usize = 10_000;

/// 粗略 token 估算：JSON 字符数 / 3.5
fn est_tokens(s: &str) -> usize {
    s.len() / 3 + s.len() / 10 // ≈ /3.33，比除法简单
}

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 从 MCP server 拿到的工具定义
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: Option<Annotations>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Annotations {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "readOnlyHint", default)]
    pub read_only: Option<bool>,
    #[serde(rename = "destructiveHint", default)]
    pub destructive: Option<bool>,
}

impl McpTool {
    /// 展示用标题
    pub fn title(&self) -> String {
        self.annotations
            .as_ref()
            .and_then(|a| a.title.clone())
            .unwrap_or_else(|| self.name.clone())
    }

    /// 是否危险操作（会改数据/发命令）
    pub fn is_destructive(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|a| a.destructive)
            .unwrap_or(false)
    }

    /// 估算这个工具的 schema 占多少 token
    pub fn est_schema_tokens(&self) -> usize {
        let v = json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.input_schema,
        });
        est_tokens(&v.to_string())
    }
}

/// 一个工具组（= 一个 MCP server）
#[derive(Debug, Clone)]
pub struct McpGroup {
    /// 组名，如 `ssh`
    pub name: String,
    /// 一句话概括这个组是干嘛的（供模型决策）
    pub summary: String,
    /// 组内工具
    pub tools: Vec<McpTool>,
    /// 该 server 是否可用（连不上时为 false）
    pub available: bool,
}

impl McpGroup {
    pub fn total_tokens(&self) -> usize {
        self.tools.iter().map(McpTool::est_schema_tokens).sum()
    }
}

// ---------------------------------------------------------------------------
// 组名 → 概括
// ---------------------------------------------------------------------------

/// 已知 server 的中文概括（模型靠这个判断该不该加载）。
///
/// 没命中就用兜底文案 + 工具名前几个当提示。
fn summarize(server: &str, tools: &[McpTool]) -> String {
    let known = match server {
        "playwright-mcp" => Some("浏览器自动化：打开网页、点击输入、抓取 DOM、截图、执行 JS。用于任何需要操作网页的任务。"),
        "windows-mcp" => Some("本机桌面自动化：枚举/操作窗口、模拟键鼠、截屏、读写剪贴板、操作注册表与进程。用于操作桌面应用。"),
        "ssh" => Some("远程服务器：列出连接、开/关会话、执行命令（含只读命令与 sudo 提权）、上下传文件。用于操作远程主机。"),
        "mcp-server-mysql" => Some("MySQL 数据库：执行只读 SQL 查询。"),
        _ => None,
    };
    if let Some(s) = known {
        return s.to_string();
    }
    // 兜底：用前几个工具名当线索
    let sample: Vec<&str> = tools.iter().take(4).map(|t| t.name.as_str()).collect();
    format!("包含 {} 个工具，例如：{}", tools.len(), sample.join(", "))
}

// ---------------------------------------------------------------------------
// MCP 客户端
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct McpClient {
    url: String,
    http: reqwest::Client,
}

impl McpClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .timeout(CALL_TIMEOUT)
                .build()
                .expect("构建 HTTP 客户端失败"),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// 走一遍 MCP 握手 + 取工具列表。
    ///
    /// 注意：1mcp 会返回 `Mcp-Session-Id`，后续请求要带上。
    /// 这个 session 用完即弃（每次调用工具时重新握手太浪费，所以缓存在 Registry 里）。
    async fn rpc(
        &self,
        payload: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<(Value, Option<String>), String> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .timeout(timeout)
            .json(&payload);

        if let Some(s) = session {
            req = req.header("Mcp-Session-Id", s);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("连接 MCP 失败（{}）：{e}", self.url))?;

        let status = resp.status();
        let sid = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            let brief: String = text.chars().take(400).collect();
            return Err(format!("MCP HTTP {status}: {brief}"));
        }

        // 响应可能是纯 JSON，也可能是 SSE（data: {...}）
        let body = text.trim();
        let parsed: Value = if body.starts_with('{') {
            serde_json::from_str(body).map_err(|e| format!("解析 MCP 响应失败: {e}"))?
        } else {
            let mut found = None;
            for line in body.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("data:") {
                    let rest = rest.trim();
                    if rest.starts_with('{') {
                        found = Some(rest.to_string());
                        break;
                    }
                }
            }
            let raw = found.ok_or_else(|| format!("MCP 响应无法解析: {}", &body[..body.len().min(200)]))?;
            serde_json::from_str(&raw).map_err(|e| format!("解析 MCP SSE 失败: {e}"))?
        };

        if let Some(err) = parsed.get("error") {
            return Err(format!("MCP 返回错误: {err}"));
        }

        Ok((parsed, sid))
    }

    /// 建立会话并返回 (session_id, tools)
    pub async fn fetch_tools(&self) -> Result<(Option<String>, Vec<McpTool>), String> {
        // 1) initialize
        let (init, sid) = self
            .rpc(
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "float-agent", "version": env!("CARGO_PKG_VERSION")}
                    }
                }),
                None,
                INIT_TIMEOUT,
            )
            .await?;

        let server = init
            .pointer("/result/serverInfo/name")
            .and_then(Value::as_str)
            .unwrap_or("mcp");
        eprintln!("[float-agent] MCP 已连接: {server}（session={sid:?}）");

        // 2) notifications/initialized（无 id，失败不致命）
        let _ = self
            .rpc(
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                sid.as_deref(),
                INIT_TIMEOUT,
            )
            .await;

        // 3) tools/list
        let (res, _) = self
            .rpc(
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
                sid.as_deref(),
                INIT_TIMEOUT,
            )
            .await?;

        let tools: Vec<McpTool> = serde_json::from_value(
            res.pointer("/result/tools").cloned().unwrap_or(json!([])),
        )
        .map_err(|e| format!("解析 tools/list 失败: {e}"))?;

        Ok((sid, tools))
    }

    /// 调用一个工具（会自动重新握手拿 session，因为 session 可能已过期）
    pub async fn call_tool(&self, tool: &str, args: &Value) -> Result<String, String> {
        let (sid, _) = self.fetch_tools().await?;

        let (res, _) = self
            .rpc(
                json!({
                    "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": tool, "arguments": args}
                }),
                sid.as_deref(),
                CALL_TIMEOUT,
            )
            .await?;

        // MCP 的 result.content 是 [{type:"text", text:"..."}] 形式
        let content = res.pointer("/result/content").cloned();
        if let Some(arr) = content.as_ref().and_then(Value::as_array) {
            let mut out = String::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                    out.push('\n');
                } else {
                    out.push_str(&item.to_string());
                    out.push('\n');
                }
            }
            if !out.trim().is_empty() {
                return Ok(out);
            }
        }

        if res.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("MCP 工具报错: {}", res["result"]));
        }

        Ok(res["result"].to_string())
    }
}

// ---------------------------------------------------------------------------
// 工具注册表
// ---------------------------------------------------------------------------

pub struct ToolRegistry {
    client: McpClient,
    /// 全部分组（含未加载的）
    groups: Vec<McpGroup>,
    /// 已加载进活跃集的组名
    active: HashSet<String>,
    /// 上次拉取是否成功
    pub connected: bool,
    pub error: Option<String>,
}

impl ToolRegistry {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            client: McpClient::new(url),
            groups: Vec::new(),
            active: HashSet::new(),
            connected: false,
            error: None,
        }
    }

    /// 克隆一份 client（reqwest::Client 内部是 Arc，克隆廉价）。
    /// 用途：**锁外**做网络 IO —— 见 fetch_snapshot 的说明。
    pub fn client_clone(&self) -> McpClient {
        self.client.clone()
    }

    /// 当前网关地址（空串 = 未配置）
    pub fn url(&self) -> String {
        self.client.url().to_string()
    }

    /// 换网关地址（设置页改配置后调用）。
    /// 换地址 = 旧 session 作废，所以顺手清掉连接态，等下次 refresh。
    pub fn set_url(&mut self, url: &str) {
        self.client = McpClient::new(url);
        self.connected = false;
        self.error = None;
        self.groups.clear();
        self.active.clear();
    }

    /// 锁外做网络 IO：拉取工具清单。**不碰 self 状态**。
    ///
    /// 为什么拆出来：refresh 持锁做网络 IO 时（网关僵持最长 ~40s），
    /// mcp_status / 工具列表 / load_tool_group 全部在锁上排队，
    /// 表现就是「点开设置卡半天」。正确姿势：锁外 fetch，锁内只做内存落账。
    pub async fn fetch_snapshot(
        client: &McpClient,
    ) -> Result<(Option<String>, Vec<McpTool>), String> {
        client.fetch_tools().await
    }

    /// 锁内落账：把 fetch_snapshot 的结果写进注册表（纯内存，微秒级）。
    pub fn apply_snapshot(
        &mut self,
        res: Result<(Option<String>, Vec<McpTool>), String>,
    ) {
        match res {
            Ok((_, tools)) => {
                let groups = Self::group_tools(tools);
                eprintln!(
                    "[float-agent] MCP 工具已加载：{} 个组 / {} 个工具",
                    groups.len(),
                    groups.iter().map(|g| g.tools.len()).sum::<usize>()
                );
                self.groups = groups;
                self.connected = true;
                self.error = None;
            }
            Err(e) => {
                eprintln!("[float-agent] MCP 拉取失败（不影响其他功能）: {e}");
                self.groups.clear();
                self.active.clear();
                self.connected = false;
                self.error = Some(e);
            }
        }
    }

    /// 从网关拉取全部工具并按 server 分组。
    /// 失败不致命 —— 应用照常运行，只是没有 MCP 工具。
    ///
    /// ⚠️ 持锁调用方注意：本方法内部做网络 IO。后台刷新请改用
    /// `fetch_snapshot`（锁外）+ `apply_snapshot`（锁内）的组合。
    pub async fn refresh(&mut self) {
        let res = Self::fetch_snapshot(&self.client).await;
        self.apply_snapshot(res);
    }

    /// 按 `_1mcp_` 切分组名
    fn group_tools(tools: Vec<McpTool>) -> Vec<McpGroup> {
        let mut map: Vec<(String, Vec<McpTool>)> = Vec::new();
        for t in tools {
            let server = match t.name.split_once("_1mcp_") {
                Some((s, _)) => s.to_string(),
                None => "(standalone)".to_string(),
            };
            match map.iter_mut().find(|(k, _)| *k == server) {
                Some((_, v)) => v.push(t),
                None => map.push((server, vec![t])),
            }
        }

        let mut groups: Vec<McpGroup> = map
            .into_iter()
            .map(|(name, tools)| {
                let summary = summarize(&name, &tools);
                McpGroup {
                    name,
                    summary,
                    tools,
                    available: true,
                }
            })
            .collect();

        // 工具多的排前面（通常也更常用）
        groups.sort_by(|a, b| b.tools.len().cmp(&a.tools.len()));
        groups
    }

    // -- 查询 ---------------------------------------------------------------

    pub fn groups(&self) -> &[McpGroup] {
        &self.groups
    }

    pub fn active_groups(&self) -> Vec<String> {
        let mut v: Vec<String> = self.active.iter().cloned().collect();
        v.sort();
        v
    }

    pub fn is_active(&self, group: &str) -> bool {
        self.active.contains(group)
    }

    /// 当前活跃集里所有 MCP 工具（供 tools 数组使用）
    pub fn active_tools(&self) -> Vec<&McpTool> {
        self.groups
            .iter()
            .filter(|g| self.active.contains(&g.name))
            .flat_map(|g| g.tools.iter())
            .collect()
    }

    /// 活跃集的 token 估算
    pub fn active_tokens(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| self.active.contains(&g.name))
            .map(McpGroup::total_tokens)
            .sum()
    }

    pub fn budget_tokens(&self) -> usize {
        ACTIVE_BUDGET_TOKENS
    }

    /// 调用一个 MCP 工具（原始工具名，如 `ssh_1mcp_run-command`）
    pub async fn call_tool(&self, raw_name: &str, args: &Value) -> Result<String, String> {
        self.client.call_tool(raw_name, args).await
    }

    /// 按组内工具的原始名反查它属于哪个组（用于校验是否已加载）
    pub fn group_of(&self, raw_tool_name: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|g| g.tools.iter().any(|t| t.name == raw_tool_name))
            .map(|g| g.name.as_str())
    }

    // -- 变更 ---------------------------------------------------------------

    /// 加载一个组。返回给模型看的说明文本。
    pub fn load(&mut self, name: &str) -> Result<String, String> {
        let g = self
            .groups
            .iter()
            .find(|g| g.name == name)
            .ok_or_else(|| {
                let all: Vec<&str> = self.groups.iter().map(|g| g.name.as_str()).collect();
                format!("没有名为「{name}」的组。可用组：{}", all.join(", "))
            })?;

        if self.active.contains(name) {
            return Ok(format!("组「{name}」已经加载过了，直接用即可。"));
        }

        let cost = g.total_tokens();
        let now = self.active_tokens();
        if now + cost > ACTIVE_BUDGET_TOKENS {
            return Err(format!(
                "加载「{name}」需要约 {cost} tokens，但当前活跃工具已占 {now}，\
                 超出预算 {ACTIVE_BUDGET_TOKENS}。请先用 unload_tool_group 卸载暂时不用的组。\
                 当前已加载：{}",
                if self.active.is_empty() {
                    "（无）".to_string()
                } else {
                    self.active_groups().join(", ")
                }
            ));
        }

        // 工具名清单（让模型知道现在能调什么，但不重复 schema）
        let names: Vec<String> = g.tools.iter().map(|t| mcp_tool_full_name(&g.name, &t.name)).collect();

        self.active.insert(name.to_string());

        Ok(format!(
            "已加载组「{name}」（{} 个工具，约 {cost} tokens）。\
             现在可以调用它们了：\n{}",
            g.tools.len(),
            names.join("\n")
        ))
    }

    pub fn unload(&mut self, name: &str) -> Result<String, String> {
        if !self.active.remove(name) {
            return Ok(format!("组「{name}」本来就没加载。"));
        }
        Ok(format!("已卸载组「{name}」。"))
    }

    /// 列出所有组（给模型看，**不含 schema**）
    pub fn describe_groups(&self) -> String {
        if !self.connected {
            return format!(
                "MCP 未连接（{}）。无法使用外部工具，请只用内置工具。",
                self.error.as_deref().unwrap_or("原因未知")
            );
        }
        if self.groups.is_empty() {
            return "MCP 已连接，但没有可用工具组。".to_string();
        }

        let mut out = String::from("可用的 MCP 工具组：\n\n");
        for g in &self.groups {
            let flag = if self.active.contains(&g.name) { "✅已加载" } else { "未加载" };
            let danger = if g.tools.iter().any(McpTool::is_destructive) { "，含危险操作" } else { "" };
            out.push_str(&format!(
                "- **{}**（{} 个工具，约 {} tokens{}{}）\n  {}\n",
                g.name,
                g.tools.len(),
                g.total_tokens(),
                danger,
                format!("，{flag}"),
                g.summary
            ));
        }
        out.push_str(&format!(
            "\n当前活跃工具占用约 {} / {} tokens。\n\
             用 load_tool_group 加载需要的组，用 unload_tool_group 卸载。\
             一次只加载当前任务真正需要的组。",
            self.active_tokens(),
            ACTIVE_BUDGET_TOKENS
        ));
        out
    }
}

// ---------------------------------------------------------------------------
// 工具名映射
// ---------------------------------------------------------------------------

/// 暴露给模型的完整工具名：`mcp__<server>__<tool>`
///
/// 加前缀是为了和内置工具（read_file 等）隔离，避免重名冲突。
pub fn mcp_tool_full_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// 从完整名反解出 (server, tool)
pub fn parse_mcp_tool_name(full: &str) -> Option<(String, String)> {
    let rest = full.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some((server.to_string(), tool.to_string()))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.into(),
            description: format!("desc of {name}"),
            input_schema: json!({"type":"object","properties":{}}),
            annotations: None,
        }
    }

    #[test]
    fn groups_by_1mcp_prefix() {
        let tools = vec![
            tool("ssh_1mcp_run-command"),
            tool("ssh_1mcp_list-connections"),
            tool("playwright-mcp_1mcp_browser_click"),
            tool("weird_no_prefix_tool"),
        ];
        let groups = ToolRegistry::group_tools(tools);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"ssh"));
        assert!(names.contains(&"playwright-mcp"));
        assert!(names.contains(&"(standalone)"));

        let ssh = groups.iter().find(|g| g.name == "ssh").unwrap();
        assert_eq!(ssh.tools.len(), 2);
        assert!(ssh.summary.contains("远程"));
    }

    #[test]
    fn groups_sorted_by_size_desc() {
        let tools = vec![
            tool("a_1mcp_1"),
            tool("b_1mcp_1"),
            tool("b_1mcp_2"),
            tool("b_1mcp_3"),
        ];
        let groups = ToolRegistry::group_tools(tools);
        assert_eq!(groups[0].name, "b");
        assert_eq!(groups[1].name, "a");
    }

    #[test]
    fn full_name_roundtrip() {
        let full = mcp_tool_full_name("ssh", "run-command");
        assert_eq!(full, "mcp__ssh__run-command");
        let (s, t) = parse_mcp_tool_name(&full).unwrap();
        assert_eq!(s, "ssh");
        assert_eq!(t, "run-command");
    }

    #[test]
    fn parse_rejects_builtin_names() {
        assert!(parse_mcp_tool_name("read_file").is_none());
        assert!(parse_mcp_tool_name("mcp__broken").is_none());
    }

    #[test]
    fn budget_guard_blocks_overload() {
        let mut reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        // 造两个组，各自都很大
        let big = |n: &str, cnt: usize| McpGroup {
            name: n.into(),
            summary: "x".into(),
            tools: (0..cnt)
                .map(|i| McpTool {
                    name: format!("t{i}"),
                    description: "d".repeat(1500),
                    input_schema: json!({"type":"object"}),
                    annotations: None,
                })
                .collect(),
            available: true,
        };
        reg.groups = vec![big("g1", 10), big("g2", 10)];
        reg.connected = true;

        // 第一个能装下
        reg.load("g1").expect("第一组应该能加载");
        // 第二个应该被预算挡住
        let err = reg.load("g2").unwrap_err();
        assert!(err.contains("超出预算"), "应被预算拦住: {err}");

        // 卸载后可加载
        reg.unload("g1").unwrap();
        reg.load("g2").expect("卸载后应能加载");
    }

    #[test]
    fn destructive_flag_detected() {
        let mut t = tool("ssh_1mcp_run-command");
        t.annotations = Some(Annotations {
            title: Some("Run".into()),
            read_only: Some(false),
            destructive: Some(true),
        });
        assert!(t.is_destructive());
        assert_eq!(t.title(), "Run");

        assert!(!tool("x").is_destructive());
    }

    #[test]
    fn describe_lists_all_groups() {
        let mut reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        reg.groups = ToolRegistry::group_tools(vec![
            tool("ssh_1mcp_a"),
            tool("mysql_1mcp_b"),
        ]);
        reg.connected = true;
        let d = reg.describe_groups();
        assert!(d.contains("ssh"));
        assert!(d.contains("mysql"));
        assert!(d.contains("未加载"));
        assert!(d.contains("load_tool_group"));
    }

    #[test]
    fn describe_when_disconnected() {
        let reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        let d = reg.describe_groups();
        assert!(d.contains("未连接"));
    }
}
