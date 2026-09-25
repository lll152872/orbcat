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

/// 一个工具组（= 一个 MCP server 上的一簇工具）
#[derive(Debug, Clone)]
pub struct McpGroup {
    /// 组名，如 `ssh` / `github`
    pub name: String,
    /// 来自哪条 server 配置（call 时按它路由）
    pub server_id: String,
    /// 一句话概括这个组是干嘛的（供模型决策）
    pub summary: String,
    /// 组内工具（`name` = 该 server 上的原始工具名）
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
                        "clientInfo": {"name": "orbcat", "version": env!("CARGO_PKG_VERSION")}
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
        eprintln!("[orbcat] MCP 已连接: {server}（session={sid:?}）");

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
    /// 每条启用的 server 一个 client；key = server id
    clients: std::collections::HashMap<String, McpClient>,
    /// 全部分组（含未加载的）
    groups: Vec<McpGroup>,
    /// 已加载进活跃集的组名
    active: HashSet<String>,
    /// 至少一条 server 拉取成功
    pub connected: bool,
    pub error: Option<String>,
}

impl ToolRegistry {
    /// 单 URL（兼容旧调用 / 测试）
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        let mut clients = std::collections::HashMap::new();
        if !url.trim().is_empty() {
            clients.insert("default".into(), McpClient::new(url));
        }
        Self {
            clients,
            groups: Vec::new(),
            active: HashSet::new(),
            connected: false,
            error: None,
        }
    }

    /// 按 server 列表建注册表。空列表 = MCP 不启用。
    pub fn with_servers(servers: &[crate::config::McpServerCfg]) -> Self {
        let mut clients = std::collections::HashMap::new();
        for s in servers {
            if s.enabled && !s.url.trim().is_empty() {
                clients.insert(s.id.clone(), McpClient::new(s.url.clone()));
            }
        }
        Self {
            clients,
            groups: Vec::new(),
            active: HashSet::new(),
            connected: false,
            error: None,
        }
    }

    /// 克隆全部 client（锁外网络 IO 用）。
    pub fn clients_clone(&self) -> Vec<(String, McpClient)> {
        self.clients
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// 兼容旧接口：第一个 client
    pub fn client_clone(&self) -> McpClient {
        self.clients
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| McpClient::new(""))
    }

    /// 概况：`id=url` 列表（设置页展示用）
    pub fn server_urls(&self) -> Vec<(String, String)> {
        self.clients
            .iter()
            .map(|(k, v)| (k.clone(), v.url().to_string()))
            .collect()
    }

    /// 兼容旧接口：第一个 URL
    pub fn url(&self) -> String {
        self.clients
            .values()
            .next()
            .map(|c| c.url().to_string())
            .unwrap_or_default()
    }

    /// 整表替换 servers（设置页改完配置后调用）。
    pub fn set_servers(&mut self, servers: &[crate::config::McpServerCfg]) {
        self.clients.clear();
        for s in servers {
            if s.enabled && !s.url.trim().is_empty() {
                self.clients.insert(s.id.clone(), McpClient::new(s.url.clone()));
            }
        }
        self.connected = false;
        self.error = None;
        self.groups.clear();
        self.active.clear();
    }

    /// 兼容旧接口：换单 URL
    pub fn set_url(&mut self, url: &str) {
        self.set_servers(&[crate::config::McpServerCfg {
            id: "default".into(),
            url: url.to_string(),
            enabled: !url.trim().is_empty(),
            label: String::new(),
        }]);
    }

    /// 锁外做网络 IO：拉取**全部** server 的工具清单。**不碰 self 状态**。
    pub async fn fetch_snapshot(
        clients: &[(String, McpClient)],
    ) -> Result<(Option<String>, Vec<(String, Vec<McpTool>)>), String> {
        if clients.is_empty() {
            return Err("没有已启用的 MCP server".into());
        }
        let mut all = Vec::new();
        let mut errs: Vec<String> = Vec::new();
        let mut any_ok = false;
        for (sid, client) in clients {
            match client.fetch_tools().await {
                Ok((_, tools)) => {
                    any_ok = true;
                    all.push((sid.clone(), tools));
                }
                Err(e) => errs.push(format!("{sid}: {e}")),
            }
        }
        if !any_ok {
            return Err(if errs.is_empty() {
                "全部 MCP server 拉取失败".into()
            } else {
                errs.join("; ")
            });
        }
        Ok((None, all))
    }

    /// 兼容旧接口：单 client snapshot
    pub async fn fetch_snapshot_one(
        client: &McpClient,
    ) -> Result<(Option<String>, Vec<McpTool>), String> {
        client.fetch_tools().await
    }

    /// 锁内落账。
    pub fn apply_snapshot(
        &mut self,
        res: Result<(Option<String>, Vec<(String, Vec<McpTool>)>), String>,
    ) {
        match res {
            Ok((_, per_server)) => {
                let mut groups = Vec::new();
                for (sid, tools) in per_server {
                    groups.extend(Self::group_tools_for(&sid, tools));
                }
                groups.sort_by(|a, b| b.tools.len().cmp(&a.tools.len()).then(a.name.cmp(&b.name)));
                eprintln!(
                    "[orbcat] MCP 工具已加载：{} 个组 / {} 个工具",
                    groups.len(),
                    groups.iter().map(|g| g.tools.len()).sum::<usize>()
                );
                self.groups = groups;
                self.connected = true;
                self.error = None;
            }
            Err(e) => {
                eprintln!("[orbcat] MCP 拉取失败（不影响其他功能）: {e}");
                self.groups.clear();
                self.active.clear();
                self.connected = false;
                self.error = Some(e);
            }
        }
    }

    /// 兼容旧接口：单 client 落账
    pub fn apply_snapshot_one(&mut self, res: Result<(Option<String>, Vec<McpTool>), String>) {
        match res {
            Ok((_, tools)) => {
                let sid = self
                    .clients
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "default".into());
                self.apply_snapshot(Ok((None, vec![(sid, tools)])));
            }
            Err(e) => self.apply_snapshot(Err(e)),
        }
    }

    /// 从全部 server 拉取。失败不致命。
    pub async fn refresh(&mut self) {
        let clients = self.clients_clone();
        let res = Self::fetch_snapshot(&clients).await;
        self.apply_snapshot(res);
    }

    /// 把一个 server 上的工具切成组。
    ///
    /// - 工具名带 `_1mcp_`（1MCP 网关）→ 按前缀拆成多组
    /// - 否则整包一组，组名 = server id
    fn group_tools_for(server_id: &str, tools: Vec<McpTool>) -> Vec<McpGroup> {
        let gateway_style = tools.iter().any(|t| t.name.contains("_1mcp_"));
        if !gateway_style {
            let summary = summarize(server_id, &tools);
            return vec![McpGroup {
                name: server_id.to_string(),
                server_id: server_id.to_string(),
                summary,
                tools,
                available: true,
            }];
        }

        let mut map: Vec<(String, Vec<McpTool>)> = Vec::new();
        for t in tools {
            let gname = match t.name.split_once("_1mcp_") {
                Some((s, _)) => s.to_string(),
                None => "(standalone)".to_string(),
            };
            match map.iter_mut().find(|(k, _)| *k == gname) {
                Some((_, v)) => v.push(t),
                None => map.push((gname, vec![t])),
            }
        }

        map.into_iter()
            .map(|(name, tools)| {
                let summary = summarize(&name, &tools);
                McpGroup {
                    name,
                    server_id: server_id.to_string(),
                    summary,
                    tools,
                    available: true,
                }
            })
            .collect()
    }

    /// 兼容旧测试/调用
    fn group_tools(tools: Vec<McpTool>) -> Vec<McpGroup> {
        let groups = Self::group_tools_for("default", tools);
        // 工具多的排前面；同名按组名稳定序（大小相同保持确定性）
        let mut groups = groups;
        groups.sort_by(|a, b| b.tools.len().cmp(&a.tools.len()).then(a.name.cmp(&b.name)));
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

    /// 活跃集的 token 估算（仅展示用，不再做加载拦截）
    pub fn active_tokens(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| self.active.contains(&g.name))
            .map(McpGroup::total_tokens)
            .sum()
    }

    /// 调用一个 MCP 工具（原始工具名，如 `ssh_1mcp_run-command`）。
    /// 按组上的 `server_id` 路由到对应 client。
    pub async fn call_tool(&self, raw_name: &str, args: &Value) -> Result<String, String> {
        let sid = self
            .groups
            .iter()
            .find(|g| g.tools.iter().any(|t| t.name == raw_name))
            .map(|g| g.server_id.clone());
        let client = match sid {
            Some(id) => self.clients.get(&id),
            None => self.clients.values().next(),
        };
        let Some(client) = client else {
            return Err("没有可用的 MCP server".into());
        };
        client.call_tool(raw_name, args).await
    }

    /// 按完整名 `mcp__<group>__<tool>` 调用。
    pub async fn call_tool_full(&self, full: &str, args: &Value) -> Result<String, String> {
        let raw = self.raw_name_of(full)?;
        self.call_tool(&raw, args).await
    }

    /// `mcp__ssh__run-command` → 该 server 上的原始名（可能是 `ssh_1mcp_run-command` 或 `run-command`）
    pub fn raw_name_of(&self, full: &str) -> Result<String, String> {
        let (group, tool) = crate::mcp::parse_mcp_tool_name(full)
            .ok_or_else(|| format!("非法 MCP 工具名：{full}"))?;
        let g = self
            .groups
            .iter()
            .find(|g| g.name == group)
            .ok_or_else(|| format!("未知工具组：{group}"))?;
        // 1mcp 网关：raw = <group>_1mcp_<tool>；独立 server：raw = <tool> 或组内唯一后缀匹配
        if let Some(t) = g.tools.iter().find(|t| t.name == format!("{group}_1mcp_{tool}")) {
            return Ok(t.name.clone());
        }
        if let Some(t) = g.tools.iter().find(|t| t.name == tool) {
            return Ok(t.name.clone());
        }
        if let Some(t) = g
            .tools
            .iter()
            .find(|t| t.name.ends_with(&format!("_{tool}")) || t.name.ends_with(tool.as_str()))
        {
            return Ok(t.name.clone());
        }
        Err(format!("组「{group}」里没有工具「{tool}」"))
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
        out.push_str(
            "\n用 load_tool_group 加载需要的组，用 unload_tool_group 卸载暂时不用的组。\n\
             只加载当前任务真正需要的组，避免上下文被工具 schema 撑爆。",
        );
        out
    }
}

// ---------------------------------------------------------------------------
// 工具名映射
// ---------------------------------------------------------------------------

/// 暴露给模型的完整工具名：`mcp__<server>__<tool>`
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
// MCP 工具「总是允许」（Cline 式，独立于 command_grants）
// ---------------------------------------------------------------------------

/// `<data_dir>/mcp_grants.json` 的形状。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpGrantsFile {
    /// 已永久允许的**完整工具名**（`mcp__ssh__run-command` 这种）
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

fn mcp_grants_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("mcp_grants.json")
}

/// 读「总是允许」名单。坏文件退空表。
pub fn load_mcp_grants(data_dir: &std::path::Path) -> Vec<String> {
    let p = mcp_grants_path(data_dir);
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    match serde_json::from_str::<McpGrantsFile>(&txt) {
        Ok(f) => f.allowed_tools,
        Err(_) => Vec::new(),
    }
}

/// 写「总是允许」名单。空表 = 删文件。已去重、排序稳定。
pub fn save_mcp_grants(data_dir: &std::path::Path, tools: &[String]) -> Result<(), String> {
    let p = mcp_grants_path(data_dir);
    let mut v: Vec<String> = tools.to_vec();
    v.sort();
    v.dedup();
    if v.is_empty() {
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| format!("删除 {} 失败: {e}", p.display()))?;
        }
        return Ok(());
    }
    let file = McpGrantsFile {
        allowed_tools: v,
    };
    let txt = serde_json::to_string_pretty(&file).map_err(|e| format!("序列化失败: {e}"))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, txt).map_err(|e| format!("写入 {} 失败: {e}", p.display()))
}

/// 把一个工具名加进「总是允许」并落盘。
pub fn allow_mcp_tool(data_dir: &std::path::Path, full_name: &str) -> Result<(), String> {
    let mut list = load_mcp_grants(data_dir);
    if !list.iter().any(|t| t == full_name) {
        list.push(full_name.to_string());
    }
    save_mcp_grants(data_dir, &list)
}

/// 从「总是允许」移除一个工具。
pub fn revoke_mcp_tool(data_dir: &std::path::Path, full_name: &str) -> Result<usize, String> {
    let mut list = load_mcp_grants(data_dir);
    let before = list.len();
    list.retain(|t| t != full_name);
    save_mcp_grants(data_dir, &list)?;
    Ok(before - list.len())
}

/// 清空全部 MCP 永久授权。
pub fn clear_mcp_grants(data_dir: &std::path::Path) -> Result<usize, String> {
    let n = load_mcp_grants(data_dir).len();
    save_mcp_grants(data_dir, &[])?;
    Ok(n)
}

/// 该工具是否已「总是允许」
pub fn is_mcp_tool_allowed(data_dir: &std::path::Path, full_name: &str) -> bool {
    load_mcp_grants(data_dir).iter().any(|t| t == full_name)
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
    fn load_has_no_token_budget() {
        let mut reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        // 两个大组：预算时代第二组会被拦；现在应都能装下
        let big = |n: &str, cnt: usize| McpGroup {
            name: n.into(),
            server_id: "default".into(),
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

        reg.load("g1").expect("第一组应该能加载");
        reg.load("g2").expect("第二组也应能加载（预算已删除）");
        assert!(reg.is_active("g1") && reg.is_active("g2"));

        reg.unload("g1").unwrap();
        assert!(!reg.is_active("g1"));
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
