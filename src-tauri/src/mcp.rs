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
//!
//! ## 能力发现索引（2026-10 补）
//!
//! 光"懒加载"还不够：`list_tool_groups` 只给**组名 + 工具数 + token 成本**，
//! 56 个工具**叫什么、能干什么**在模型上下文里完全缺席。于是用户说
//! "帮我给那个仓库提个 issue"时，模型只能猜 `github` 组存不存在，再
//! `load_tool_group` 试探 —— 试探失败就白烧一轮 prefill。
//!
//! 解法是第三条路（既不猜、也不全量注入）：把"名字 + 一句话用途"压成一份
//! **紧凑索引**，常驻在 system prompt 的**动态区**，并配一个
//! `search_tools(query)` 让模型按意图检索。逐字实测（56 工具 / 4 组）：
//!
//! ```text
//!   全量 schema           ~14.4k tok   不可接受（本文件开头的全部理由）
//!   索引本体（本方案）       ~826 tok   可发现：知道有什么、该加载哪组
//!   ＋ prompt 里的规则文字   ~230 tok   合计 ~1.06k = 全量 schema 的 7.3%
//!   只有组名（旧行为）          0 tok   不可发现：只能猜和试探
//! ```
//!
//! 数字不是估的：`mcp::tests::index_stays_compact` 量索引本体，
//! `agent::tests::tool_index_constant_cost_stays_small` 量"有/无索引两种
//! system prompt 的 token 差"。两条都设了上限，谁把索引写肥谁先炸。
//!
//! 索引**必须短**，所以：每个工具一行、描述截断到 [`INDEX_DESC_CHARS`] 字、
//! 只取描述的第一句、过滤空描述与过渡句（否则这份"摘要"会慢慢长成第二份 schema，
//! 那就等于把懒加载又拆掉了）。

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

/// 能力索引里**每个工具描述最多留多少字**（超出截断加 `…`）。
///
/// 为什么是 60：一行「名字 — 用途」在中文下约 12~16 tokens，60 字的中文描述
/// 已经能说清"这个工具干什么"，再长就是参数细节 —— 那属于 schema，
/// 等 `load_tool_group` 之后再付钱。调大这个数等于悄悄退回全量注入。
const INDEX_DESC_CHARS: usize = 60;

/// 一句话摘要短于这个长度就认为"没信息量"，索引用工具名兜底。
///
/// 现实里 MCP 工具的描述首行常是 `Run a command` / `Get the current state` 这类
/// 复述工具名的过渡句，留着只是浪费 token。
const INDEX_MIN_DESC_CHARS: usize = 14;

/// 能力索引落盘文件名（`<data_dir>/mcp_tool_index.txt`）。
///
/// 为什么要落盘：`build_system_prompt_for` 只拿得到 `data_dir`，拿不到 MCP
/// 注册表（它每轮现组 prompt，签名里塞不进 `&Mutex<Registry>`）。而索引内容
/// 只在**拉取快照**时变 —— 落一份缓存，组装 prompt 时按文件读即是，
/// 与 `skills::catalog` / `memory/MEMORY.md` 的做法一致（同一条纪律：
/// 组装 prompt 不做网络 IO、不碰注册表锁）。
const INDEX_CACHE_FILE: &str = "mcp_tool_index.txt";

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
    /// 展示用标题（MCP 的 `annotations.title`，缺省回落工具名）。
    ///
    /// ⚠️ **仅供测试**（2026-09-30 标注）：生产 UI 直接用 `name`，
    /// 没走 `annotations.title`。留着是为了让"缺省回落工具名"这条规则有断言。
    #[cfg(test)]
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
    // ⚠️ 2026-09-30 删掉了 `available: bool`（"该 server 是否可用"）：
    //    它被写了 4 处、**从没被读过**，而且值恒为 true ——
    //    "连不上"这件事在别处表达（组根本不会进 `groups`，或整体状态不是 connected）。
    //    恒真字段最坏的效果是让人以为某处会把它置 false，从而去找一个不存在的分支。
}

impl McpGroup {
    pub fn total_tokens(&self) -> usize {
        self.tools.iter().map(McpTool::est_schema_tokens).sum()
    }
}

// ---------------------------------------------------------------------------
// 能力索引：工具名 + 一句话用途（**不含 schema**）
// ---------------------------------------------------------------------------

/// 把一个工具的描述压成"一句话用途"。
///
/// 只取**第一段有信息量的行**，理由：MCP 工具的描述普遍是
/// 「一句话概述\n\nUsage: ...\n参数说明...」这种结构，一句话概述就在第一行。
/// 多行拼接会让 56 个工具的行数翻倍，索引就没意义了。
fn one_line_usage(t: &McpTool) -> String {
    let mut best: Option<String> = None;
    for raw in t.description.lines() {
        // 去掉 markdown 标题符与列表符号：`## Run a command` → `Run a command`
        let line = raw.trim_start_matches(['#', '-', '*', ' ']).trim();
        if line.is_empty() {
            continue;
        }
        let short = line.chars().count() < INDEX_MIN_DESC_CHARS;
        // 短行先留着（可能就是全部内容），够长的直接采用
        if !short {
            best = Some(line.to_string());
            break;
        }
        if best.is_none() {
            best = Some(line.to_string());
        }
    }
    let Some(s) = best else {
        // 没有描述：用工具名兜底，至少让模型知道有这么个工具
        return format!("（{name}，无描述）", name = t.name);
    };
    truncate_chars(&s, INDEX_DESC_CHARS)
}

/// 按**字符**截断（不是字节 —— 中文按字节切会切碎 UTF-8，直接 panic）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 索引里展示的短工具名：`ssh_1mcp_run-command` → `run-command`。
///
/// 为什么剥掉 `<组>_1mcp_` 前缀：索引按组分行，组名已经写过一次了，
/// 56 行里每行再重复一遍前缀纯属浪费 —— 而且模型要调的是
/// `mcp__<组>__<工具>`，短的反而更好对上。
fn short_tool_name(name: &str) -> &str {
    match name.split_once("_1mcp_") {
        Some((_, tool)) if !tool.is_empty() => tool,
        _ => name,
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

    /// 发一次 POST，带**末尾斜杠兜底**。
    ///
    /// ## 为什么需要它
    /// 实测本机 1MCP 网关：`http://127.0.0.1:3050/mcp` → **404**，
    /// `http://127.0.0.1:3050/mcp/` → 200。
    /// 用户在设置页填地址时，末尾带不带 `/` 完全看手感 —— 而这个差别会让
    /// 整个 MCP 静默不可用（索引缓存里只留一句"连接 MCP 失败"，看不出是斜杠问题）。
    ///
    /// ## 为什么是"兜底"而不是"一律规范化"
    /// 不能无脑补 `/`：也有网关只在**不带**斜杠时能通。
    /// 所以顺序是「先用用户写的地址；**只有 404** 才换另一种写法再试一次」，
    /// 两边都不猜、两边都试，错误信息里带上**实际用的 URL**（`used_url`）
    /// 方便对上日志。
    async fn post_with_slash_fallback(
        &self,
        payload: &Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<(reqwest::Response, String), String> {
        let primary = self.url.clone();
        let alt = slash_variant(&primary);

        let resp = self.post_once(&primary, payload, session, timeout).await?;
        // 只有 404 才值得换写法：403/500 换 URL 也是白搭，反而把真错误掩盖掉
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            if let Some(alt) = alt {
                eprintln!("[orbcat] MCP {primary} 返回 404，改用 {alt} 重试一次");
                let resp2 = self.post_once(&alt, payload, session, timeout).await?;
                return Ok((resp2, alt));
            }
        }
        Ok((resp, primary))
    }

    async fn post_once(
        &self,
        url: &str,
        payload: &Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<reqwest::Response, String> {
        let mut req = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .timeout(timeout)
            .json(payload);

        if let Some(s) = session {
            req = req.header("Mcp-Session-Id", s);
        }

        req.send()
            .await
            .map_err(|e| format!("连接 MCP 失败（{url}）：{e}"))
    }

    /// 走一遍 MCP 握手 + 取工具列表。
    async fn rpc(
        &self,
        payload: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<(Value, Option<String>), String> {
        let (resp, used_url) = self.post_with_slash_fallback(&payload, session, timeout).await?;

        let status = resp.status();
        let sid = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            let brief: String = text.chars().take(400).collect();
            return Err(format!("MCP HTTP {status}（{used_url}）: {brief}"));
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
    /// **模式允许的组名**（见 `crate::modes`）：`None` = 不设限（单测与
    /// "模式功能上线前的行为"）。`Some` 里含 `modes::ALL` 哨兵 = 全给。
    ///
    /// 为什么存在注册表里而不是每次现算：`load_tool_group` 是**模型主动调**的
    /// （`tools.rs` 里只拿得到注册表，拿不到 `data_dir`），没有一个落在这里的
    /// 允许集，模型就能绕开模式约束把任意组装进上下文。
    allowed: Option<HashSet<String>>,
    /// 索引缓存写到哪（`<data_dir>/mcp_tool_index.txt`）。
    ///
    /// `None` = 不落盘（单测默认如此：测试不该往磁盘上留状态）。
    /// 由 `lib.rs` 启动时注入，见 `set_index_cache_dir`。
    index_dir: Option<std::path::PathBuf>,
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
            allowed: None,
            index_dir: None,
            connected: false,
            error: None,
        }
    }

    /// 按 server 列表建注册表。空列表 = MCP 不启用。
    ///
    /// 注意：这里**不设置任何组为活跃** —— 活跃集由 `apply_snapshot` 依据
    /// `config::AgentSettings::mcp_all_groups` / `mcp_group_enabled` 决定。
    /// （历史原因：早先是"一个都不加载、全靠模型按需 load"，现在是默认全给。）
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
            allowed: None,
            index_dir: None,
            connected: false,
            error: None,
        }
    }

    /// 指定能力索引的落盘目录（`<data_dir>`）。
    ///
    /// 为什么不在 `new` / `with_servers` 里要这个参数：那两个函数被**大量单测**
    /// 直接调用（20+ 处），加参数等于让每个测试都得编一个临时目录。改成
    /// 一个可选的注入点，生产路径在 `lib.rs` 启动时调一次即可。
    pub fn set_index_cache_dir(&mut self, dir: &std::path::Path) {
        self.index_dir = Some(dir.to_path_buf());
    }

    /// 能力索引缓存文件路径（未注入目录时为 `None`）。
    pub fn index_cache_path(&self) -> Option<std::path::PathBuf> {
        self.index_dir.as_ref().map(|d| d.join(INDEX_CACHE_FILE))
    }

    /// 能力索引缓存文件路径 —— **不依赖注册表实例**。
    ///
    /// `agent::build_system_prompt_for` 只有 `data_dir`，拿不到注册表，
    /// 所以这条"路径怎么算"的规则必须能单独问出来（两处各写一份迟早写岔，
    /// 写岔的后果是索引永远读不到 → 静默失效，最难查的那种 bug）。
    pub fn index_cache_path_in(data_dir: &std::path::Path) -> std::path::PathBuf {
        data_dir.join(INDEX_CACHE_FILE)
    }

    /// 读回能力索引缓存（组装 system prompt 用）。
    ///
    /// 坏文件 / 不存在都退空串 —— 索引是**可选优化**，读不到就退回
    /// "模型自己 list_tool_groups"，绝不能让 prompt 组装失败。
    pub fn read_cached_index(data_dir: &std::path::Path) -> String {
        std::fs::read_to_string(Self::index_cache_path_in(data_dir))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    /// 把当前分组写成索引缓存。没注入目录 = 空操作（单测路径）。
    ///
    /// 只在内容真的变了才写：这个函数在每次拉快照后都会跑，而快照拉取
    /// 可能被设置页反复触发；无脑覆写会让磁盘上文件的 mtime 一直跳。
    fn persist_index(&self) {
        let Some(path) = self.index_cache_path() else {
            return;
        };
        let text = self.capability_index();
        if std::fs::read_to_string(&path).map(|s| s == text).unwrap_or(false) {
            return;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Err(e) = std::fs::write(&path, &text) {
            // 索引写不进去不影响任何工具可用性 —— 只记一行日志，不打断拉取流程
            eprintln!("[orbcat] ⚠️ 写 MCP 能力索引失败（不影响工具使用）: {e}");
        }
    }

    /// 克隆全部 client（锁外网络 IO 用）。
    pub fn clients_clone(&self) -> Vec<(String, McpClient)> {
        self.clients
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    // ⚠️ 2026-09-30 清 dead-code：这个 impl 块里原本有五个"兼容旧接口"的方法
    //    （`client_clone` / `server_urls` / `fetch_snapshot_one` /
    //     `apply_snapshot_one` / `refresh`），**全部零调用者**，已删。
    //
    // 为什么值得删而不是留着：它们是**多 server 改造之前**的单 client 路径。
    // 尤其 `refresh()` —— 它看着像"刷新 MCP"的正规入口，而实际生产路径在
    // `lib.rs` 里自己 `fetch_snapshot` + `apply_snapshot_with_settings`
    // （因为要顺带按 settings 重算活跃集与索引缓存）。留着 `refresh()` 会让人
    // 以为改它能改刷新行为，改完发现没生效。
    //
    // `client_clone()` 的语义更危险：MCP 会话是有状态的（握手拿 session id、
    // 工具列表跟着会话走），clone 出来的 client 会另开一套状态。
    //
    // `server_urls()` 则连需求都不成立：设置页读的是 `mcp.json`
    // （`mcp_servers_list` 命令），不经过注册表。

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
        // 索引缓存与分组同步刷新 —— 缓存**晚于**内存状态更新，顺序不能反：
        // 反过来的话中间那一瞬间写出的索引和实际分组对不上。
        // 失败路径也会走到这里（写出"未连接"的短提示），这样旧索引不会
        // 在 MCP 掉线后继续留在 prompt 里骗模型。
        self.persist_index();
    }

    /// 锁内落账。
    ///
    /// 落账后按 `settings` 决定活跃集（决策：**默认全给**，不再按需注入）：
    /// - `mcp_all_groups = true`（默认）：所有组都设为活跃，仅减去用户显式关掉的
    /// - `mcp_all_groups = false`：回到旧行为，只有 `mcp_group_enabled[name]=true` 的才活跃
    ///
    /// ⚠️ 必须在 groups 重建**之后**调用，否则组名对不上。
    /// 返回恢复后的活跃组数（打印日志 / 测试断言用）。
    pub fn apply_snapshot_with_settings(
        &mut self,
        res: Result<(Option<String>, Vec<(String, Vec<McpTool>)>), String>,
        settings: &crate::config::AgentSettings,
        allowed: Option<HashSet<String>>,
    ) -> usize {
        self.apply_snapshot(res);
        self.restore_active_from_settings(settings, allowed)
    }

    /// 按设置重建活跃集。组名不存在的条目会被忽略（server 下线 / 改名）。
    ///
    /// `allowed` = **模式允许的组**（`crate::modes::AllowedGroups::as_registry_set`）。
    /// `None` = 不设限。两层的关系是**交集**：
    ///
    /// ```text
    ///   活跃 = 用户设置（mcp_all_groups / mcp_group_enabled）∩ 模式允许
    /// ```
    ///
    /// 为什么是交集而不是"模式优先"：用户显式关掉的组，模式**开不回来**
    /// （模式只能收窄）—— 否则用户在设置页关了一个组、切一次模式它又回来了，
    /// 那是"设置不生效"的经典投诉。
    ///
    /// 返回活跃组数，便于日志与测试断言。
    pub fn restore_active_from_settings(
        &mut self,
        settings: &crate::config::AgentSettings,
        allowed: Option<HashSet<String>>,
    ) -> usize {
        let n = self.recompute_active(settings, allowed.as_ref());
        // 活跃集刚被重算 → 索引里的「已加载 / 未加载」标记要跟着刷新。
        // 调用链是 apply_snapshot → persist_index（那时 active 还是空的）→ 这里，
        // 所以必须在本函数收尾再刷一次，否则缓存里会写着"全部未加载"。
        self.persist_index();
        n
    }

    fn recompute_active(
        &mut self,
        settings: &crate::config::AgentSettings,
        allowed: Option<&HashSet<String>>,
    ) -> usize {
        self.active.clear();
        self.allowed = allowed.cloned();
        if !self.connected {
            return 0;
        }
        let names: Vec<String> = self.groups.iter().map(|g| g.name.clone()).collect();
        // 哨兵 → 展开成"当前真实存在的全部组"：`"all"` 必须自动包含**未来新增**的组，
        // 所以不能把当时的组名快照下来。
        let allowed_names = match allowed {
            None => None,
            Some(s) => Some(crate::modes::expand_allowed(s, &names)),
        };
        for g in &self.groups {
            // 未记录过的组：按全局开关决定（默认全开）
            let enabled = *settings
                .mcp_group_enabled
                .get(&g.name)
                .unwrap_or(&settings.mcp_all_groups);
            if !enabled {
                continue;
            }
            // 模式不许可 → 不活跃（但**不进** mcp_group_enabled，用户的设置不动）
            if let Some(a) = &allowed_names {
                if !a.contains(&g.name) {
                    continue;
                }
            }
            self.active.insert(g.name.clone());
        }
        self.active.len()
    }

    /// 这个组**当前模式**允不允许。
    ///
    /// `true` 的两种情况：没设限（`None`）、组在白名单里 / 哨兵全给。
    /// 组名不存在时：全给 → 允许（新增组自动放行）；白名单 → 由
    /// [`crate::modes::AllowedGroups`] 侧归入 `unknown`，这里只认名字。
    pub fn mode_allows(&self, group: &str) -> bool {
        match &self.allowed {
            None => true,
            Some(s) => {
                crate::modes::set_is_all(s) || s.contains(group)
            }
        }
    }

    /// 当前模式不允许、但 MCP 里真实存在的组（面板里标"本模式不给"）。
    pub fn mode_denied(&self) -> Vec<String> {
        if self.allowed.is_none() {
            return Vec::new();
        }
        self.groups
            .iter()
            .filter(|g| !self.mode_allows(&g.name))
            .map(|g| g.name.clone())
            .collect()
    }

    /// **仅测试用**：按设置恢复活跃集、不设模式限制（等价于上线前的行为）。
    #[cfg(test)]
    pub(crate) fn restore_active_from_settings_unrestricted(
        &mut self,
        settings: &crate::config::AgentSettings,
    ) -> usize {
        let n = self.recompute_active(settings, None);
        self.persist_index();
        n
    }

    /// 把一个 server 上的工具切成组。**仅供测试**：与生产 `fetch_snapshot`
    /// 内部那段完全等价（`group_tools_for` + 按工具数降序、同数按组名），
    /// 供跨模块测试造"已连接"的注册表。生产不调用它 —— 生产走 `apply_snapshot`。
    #[cfg(test)]
    fn group_tools(tools: Vec<McpTool>) -> Vec<McpGroup> {
        let mut groups = Self::group_tools_for("default", tools);
        // 工具多的排前面；同名按组名稳定序（大小相同保持确定性）
        groups.sort_by(|a, b| b.tools.len().cmp(&a.tools.len()).then(a.name.cmp(&b.name)));
        groups
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
                }
            })
            .collect()
    }

    /// 测试用建注册表的统一入口。
    ///
    /// ⚠️ 2026-09-30 清 dead-code 时删掉了 `group_tools()`（"兼容旧测试/调用"）：
    /// 它只是 `group_tools_for("default", …)` + 一段排序，而生产侧
    /// `fetch_snapshot` 内部已经带同样的排序 —— 两处各写一份，迟早不一致。
    /// 需要造注册表请走下面这个 `from_tools_for_test`。
    ///
    /// **仅测试用**：直接用一组工具造一个"已连接"的注册表。
    ///
    /// 为什么需要它：`tools.rs` 的测试要验证 `search_tools` 的端到端行为，
    /// 而得跨模块直接摸 `groups` 私有字段 —— 那等于为了测试把内部状态
    /// 开放给全 crate。给一个有明确名字的测试构造器，比放宽字段可见性干净。
    /// （不给 `pub`：生产代码没有"手工造分组"的需求，快照才是唯一入口。）
    #[cfg(test)]
    pub(crate) fn from_tools_for_test(tools: Vec<McpTool>) -> Self {
        let mut reg = Self::new("");
        reg.groups = Self::group_tools(tools);
        reg.connected = true;
        reg
    }

    /// **仅测试用**：与真实 1MCP 网关同形的夹具（56 个工具 / 4 组 / 多行描述）。
    ///
    /// 为什么夹具要建在 `impl` 里而不是某个 `mod tests` 内：`agent.rs` 也用它
    /// 量"索引让 system prompt 贵了多少"，而 `agent.rs` 读不到 `mcp::tests`
    /// 里的私有夹具。同一份夹具服务三处（mcp 的成本断言、tools 的检索断言、
    /// agent 的常驻成本断言），才是"同一形状"的唯一保证。
    #[cfg(test)]
    pub(crate) fn gateway_like_for_test() -> Self {
        let mk = |name: &str, head: &str| McpTool {
            name: name.to_string(),
            description: format!(
                "{head}\n\nUsage: call this tool with the documented arguments.\n\
                 Args:\n  - foo: the foo argument\n  - bar: the bar argument\n\
                 Returns a structured result. Errors are reported as text."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "foo": {"type": "string"},
                    "bar": {"type": "string"},
                    "baz": {"type": "string"},
                }
            }),
            annotations: None,
        };

        let playwright = [
            ("browser_navigate", "Navigate to a URL in the browser"),
            ("browser_click", "Click an element on the page"),
            ("browser_type", "Type text into an input field"),
            ("browser_snapshot", "Capture the accessibility snapshot of the page"),
            ("browser_take_screenshot", "Take a screenshot of the current page"),
            ("browser_evaluate", "Evaluate JavaScript in the page context"),
            ("browser_press_key", "Press a keyboard key"),
            ("browser_hover", "Hover over an element"),
            ("browser_select_option", "Select an option in a dropdown"),
            ("browser_wait_for", "Wait for text to appear or disappear"),
            ("browser_file_upload", "Upload files through a file chooser"),
            ("browser_handle_dialog", "Accept or dismiss a browser dialog"),
            ("browser_navigate_back", "Go back to the previous page"),
            ("browser_tabs", "List, create, close or select browser tabs"),
            ("browser_console_messages", "Return console messages from the page"),
            ("browser_network_requests", "Return network requests made by the page"),
            ("browser_resize", "Resize the browser window"),
            ("browser_close", "Close the page"),
            ("browser_install", "Install the browser for the current channel"),
            ("browser_drag", "Drag and drop between two elements"),
            ("browser_drop", "Drop a file onto an element"),
            ("browser_pdf_save", "Save the page as a PDF file"),
            ("browser_generate_playwright_test", "Generate a Playwright test for the last actions"),
            ("browser_run_code", "Run arbitrary Playwright code"),
        ];
        let windows = [
            ("State-Tool", "Get the current desktop and window state"),
            ("Click-Tool", "Click at the given screen coordinates"),
            ("Type-Tool", "Type text with the keyboard"),
            ("Scroll-Tool", "Scroll the active window"),
            ("Move-Tool", "Move the mouse cursor"),
            ("Shortcut-Tool", "Press a keyboard shortcut"),
            ("Clipboard-Tool", "Read or write the system clipboard"),
            ("Screenshot-Tool", "Capture a screenshot of the desktop"),
            ("Wait-Tool", "Wait for a number of seconds"),
            ("Powershell-Tool", "Run a PowerShell command on the host"),
            ("Registry-Tool", "Read or write Windows registry values"),
            ("Process-Tool", "List or manage running processes"),
            ("App-Tool", "Launch or focus an application"),
            ("Shell-Tool", "Open a shell in a window"),
            ("Scrape-Tool", "Scrape the content of a window"),
            ("Launch-Tool", "Launch an application by name"),
            ("Resize-Tool", "Resize a window"),
            ("Desktop-Tool", "Switch between virtual desktops"),
            ("Notification-Tool", "Show a desktop notification"),
            ("Multi-Select-Tool", "Select multiple items with the mouse"),
        ];
        let ssh = [
            ("list-connections", "List all configured SSH connections"),
            ("open-connection", "Open an SSH connection by name"),
            ("close-connection", "Close an open SSH connection"),
            ("run-command", "Run a command on a remote host"),
            ("sudo-run-command", "Run a command with sudo on a remote host"),
            ("read-only-command", "Run a whitelisted read-only command"),
            ("upload-file", "Upload a local file to the remote host"),
            ("download-file", "Download a file from the remote host"),
            ("list-directory", "List a directory on the remote host"),
            ("get-system-info", "Get system information from the remote host"),
            ("check-status", "Check the status of an SSH connection"),
        ];
        let mysql = [("execute_query", "Execute a read-only SQL query against MySQL")];

        let mut tools = Vec::new();
        for (g, list) in [
            ("playwright-mcp", &playwright[..]),
            ("windows-mcp", &windows[..]),
            ("ssh", &ssh[..]),
            ("mcp-server-mysql", &mysql[..]),
        ] {
            for (n, head) in list {
                tools.push(mk(&format!("{g}_1mcp_{n}"), head));
            }
        }
        assert_eq!(tools.len(), 56, "夹具要与真实网关同形：56 个工具");

        let mut reg = Self::new("http://127.0.0.1:1/mcp");
        reg.groups = Self::group_tools(tools);
        reg.connected = true;
        reg
    }

    /// **仅测试用**：上面那份夹具的索引文本（给 `agent.rs` 量常驻成本用）。
    #[cfg(test)]
    pub(crate) fn test_fixture_index() -> String {
        Self::gateway_like_for_test().capability_index()
    }

    // -- 查询 ---------------------------------------------------------------

    pub fn groups(&self) -> &[McpGroup] {
        &self.groups
    }

    // ⚠️ 2026-09-30 清 dead-code 删了三个查询方法（都零调用者）：
    //    - `server_urls()` —— 设置页的 server 列表读的是 `mcp.json`（`mcp_servers_list`
    //      命令），不走注册表
    //    - `active_groups()` —— 活跃组在别处是从 `groups().filter(is_active)` 现算的；
    //      这个"排好序的 Vec<String>"没人用
    //    - `group_of()` —— 它按原始工具名反查组，而 `call_tool` **内联了同一段查找**
    //      来路由 server。同一条规则两份实现迟早不一致，留 `call_tool` 那份。
    //      真要按工具名取组，从 `groups()` 现查一次即可。

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

    /// ⚠️ 2026-09-30 清 dead-code：这个 impl 块里原本还有五个"兼容旧接口"的
    /// 方法（`client_clone` / `server_urls` / `call_tool_full` / `raw_name_of` /
    /// `group_of`），**全部零调用者**，已删。
    ///
    /// 关键理由（不只是"没用"）：
    /// - `raw_name_of()` 是"完整名 → 原始名"的**反向映射**（含三种后缀兜底匹配），
    ///   而工具层用的是正向映射 `mcp_tool_callable_name()`（原始名 → 可调名）。
    ///   **两个方向各有一套猜测规则**，迟早对不上；而实际调用链只需要正向那一个。
    /// - `call_tool_full()` 只是 `raw_name_of` + `call_tool` 的组合壳，跟着一起走。
    /// - `group_of()` 按原始工具名反查组，而 `call_tool` 已经**内联了同一段查找**
    ///   来路由 server —— 同一规则两份实现，留 `call_tool` 那份。
    /// - `server_urls()` 想给设置页用，但设置页读的是 `mcp.json`
    ///   （`mcp_servers_list` 命令），根本不经过注册表。

    // -- 变更 ---------------------------------------------------------------

    /// 加载一个组。返回给模型看的说明文本。
    pub fn load(&mut self, name: &str) -> Result<String, String> {
        // 模式闸门**先于**"有没有这个组"：模式不让给的组，连"存在"都不该被确认。
        // 不拦的话模型可以用 load_tool_group 把模式收起来的组重新装回活跃集，
        // 那一层约束就等于不存在。
        if !self.mode_allows(name) {
            return Err(format!(
                "当前 agent 模式不允许使用组「{name}」。\
                 需要它时请让用户切到 PTC 模式（或改 agent-data/modes.json 里当前模式的\
                 mcpGroupsPreload）。"
            ));
        }
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
        // 工具名清单（让模型知道现在能调什么，但不重复 schema）。
        //
        // ⚠️ 必须用 `mcp_tool_callable_name`（剥掉 `_1mcp_` 网关前缀），
        // 不能直接拼 raw name：那样给出的是 `mcp__ssh__ssh_1mcp_run-command`
        // 这种**不存在的工具名**，模型照着调只会失败。
        // 这与 `tools.rs::tool_specs` 构建的可调列表是同一口径。
        let names: Vec<String> = g
            .tools
            .iter()
            .map(|t| mcp_tool_callable_name(&g.name, &t.name))
            .collect();

        self.active.insert(name.to_string());
        // 索引里带着每组的「已加载 / 未加载」标记 → 活跃集变了要刷缓存。
        // （只影响那几个字的准确性，但刷新本身是"内容没变就不写盘"，很便宜。）
        self.persist_index();

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
        self.persist_index();
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
        for g in self.groups.iter().filter(|g| self.mode_allows(&g.name)) {
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
        // 模式收起来的组，列出来（否则模型会以为工具不存在）
        out.push_str(&self.mode_denied_hint());
        out
    }

    /// 「本模式还收着哪些组」那段文字。没有收着的组时返回空串。
    ///
    /// 为什么要说：模式把组藏起来后，模型最容易的反应是"我没有这个能力"，
    /// 然后放弃或改用终端乱试。告诉它"这些组存在、只是当前模式不给，
    /// 需要就请用户切模式"，比让它自己猜要省一整轮。
    fn mode_denied_hint(&self) -> String {
        let denied = self.mode_denied();
        if denied.is_empty() {
            return String::new();
        }
        format!(
            "\n⚠️ 另有一组外部工具被**当前 agent 模式**收起来了（本次不可用）：{}。\
             需要它们时请用户切到 PTC 模式。\n",
            denied.join(", ")
        )
    }

    // -- 能力发现（名字 + 一句话用途，**永不含 schema**）────────────────────

    /// 把全部组压成一份**紧凑能力索引**：组 → 工具名 + 一句话用途。
    ///
    /// ## 为什么要有它（而不是继续只给组名）
    /// 只给组名的问题是"不可发现"：模型知道有个 `github` 组，但不知道里面有
    /// `create_issue`。于是它要么放弃（回"我没有这个能力"），要么 `load_tool_group`
    /// 试探 —— 试探是**有代价**的（那一整组 schema 当轮就进上下文，几百到几千 token），
    /// 而且失败一次就白烧一轮 prefill。
    ///
    /// ## 为什么不是全量注入
    /// 56 个工具的 schema ≈ 14.4k tokens，每轮都带 → prefill 变慢、模型选错工具、
    /// 小模型直接撑爆上下文。索引只留"名字 + 一句话"（约 0.7k tokens，5%），
    /// 足够回答"有没有这个能力、该加载哪一组"，参数细节仍等 `load_tool_group`。
    ///
    /// ## 注意
    /// 这里**不提** token 成本 —— 成本信息 `describe_groups` 已经给了，
    /// 常驻内容里每多一列就是 56 次重复。也不列 `mcp__组__工具` 全名，
    /// 前缀按组写一次就够（省下的正是"全量注入"要避免的那部分）。
    pub fn capability_index(&self) -> String {
        if !self.connected {
            return format!(
                "MCP 未连接（{}），暂无外部工具。",
                self.error.as_deref().unwrap_or("原因未知")
            );
        }
        if self.groups.is_empty() {
            return "MCP 已连接，但没有可用工具。".to_string();
        }

        // ⚠️ 模式收起来的组**不进索引**：这份索引会落盘成 `mcp_tool_index.txt`、
        // 进而被 `agent::build_system_prompt_for` 注进 system prompt ——
        // 列在那里等于告诉模型"你有这些工具"，然后它调了才发现被拦
        // （白烧一轮，且"用户看不到为什么失败"）。
        let mut total: usize = 0;
        for g in self.groups.iter().filter(|g| self.mode_allows(&g.name)) {
            total += g.tools.len();
        }
        let mut out = format!(
            "外部工具索引（{total} 个，按组）：\
             名字形如 `mcp__<组名>__<工具名>`；已加载的组可直接调用，\
             未加载的先 `load_tool_group`。\n"
        );
        for g in self.groups.iter().filter(|g| self.mode_allows(&g.name)) {
            let mark = if self.active.contains(&g.name) { "已加载" } else { "未加载" };
            out.push_str(&format!(
                "\n**{}**（{} 个，{mark}）：\n",
                g.name,
                g.tools.len()
            ));
            for t in g.tools.iter() {
                out.push_str(&format!(
                    "- {} — {}\n",
                    short_tool_name(&t.name),
                    one_line_usage(t)
                ));
            }
        }
        out.push_str(&self.mode_denied_hint());
        out
    }

    /// 按意图检索工具（`search_tools` 的实现）。
    ///
    /// 匹配范围刻意做得宽：组名、工具名、一句话用途、**工具原始描述全文**
    /// 都参与匹配（描述全文只在检索时读，不进常驻内容 —— 这正是索引与 schema
    /// 之间的分工）。命中按组聚合返回。
    ///
    /// `query` 为空 → 返回完整索引（等价于"我有什么工具"）。
    ///
    /// 注：system prompt 里那一段由 `agent::build_system_prompt_for` 直接拼
    /// （它要在"动态区"的确切位置插入，且不该为此去抢注册表的异步锁），
    /// 所以这里只负责**内容**，不负责 prompt 措辞 —— 措辞写两处迟早分叉。
    pub fn search_tools(&self, query: &str) -> String {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.capability_index();
        }
        if !self.connected {
            return format!(
                "MCP 未连接（{}），搜不到外部工具。",
                self.error.as_deref().unwrap_or("原因未知")
            );
        }

        let mut hits: Vec<(&McpGroup, Vec<&McpTool>)> = Vec::new();
        for g in self.groups.iter().filter(|g| self.mode_allows(&g.name)) {
            // 组名命中 = 这一组整个算命中（用户说的可能就是"用那个 mysql 组"）
            let group_hit = g.name.to_lowercase().contains(&q) || g.summary.to_lowercase().contains(&q);
            let matched: Vec<&McpTool> = g
                .tools
                .iter()
                .filter(|t| {
                    group_hit
                        || t.name.to_lowercase().contains(&q)
                        || t.description.to_lowercase().contains(&q)
                })
                .collect();
            if !matched.is_empty() {
                hits.push((g, matched));
            }
        }

        if hits.is_empty() {
            let groups: Vec<&str> = self
                .groups
                .iter()
                .filter(|g| self.mode_allows(&g.name))
                .map(|g| g.name.as_str())
                .collect();
            return format!(
                "没有匹配「{}」的外部工具。可用组：{}。\
                 也可以 `list_tool_groups` 看各组概括，或直接 `load_tool_group` 试一组。{}",
                query.trim(),
                groups.join(", "),
                self.mode_denied_hint()
            );
        }

        let mut out = format!("匹配「{}」的外部工具：\n", query.trim());
        for (g, tools) in hits {
            let mark = if self.active.contains(&g.name) { "已加载，可直接调用" } else { "未加载，需先 load_tool_group" };
            out.push_str(&format!("\n**{}**（{mark}）：\n", g.name));
            for t in tools {
                out.push_str(&format!(
                    "- `{}` — {}\n",
                    mcp_tool_callable_name(&g.name, &t.name),
                    one_line_usage(t)
                ));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 工具名映射
// ---------------------------------------------------------------------------

/// 给出"另一种末尾斜杠写法"：`…/mcp` ↔ `…/mcp/`。
///
/// 只对**路径部分**动手（用 `://` 之后的第一个 `/` 定位），避免把
/// `http://host` 这种"没有路径"的地址变成 `http://host/` —— 那属于另一个 URL 了。
/// 带查询串/片段的地址不去猜（`?x=1` 后面补斜杠没有意义）。
fn slash_variant(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() || u.contains('?') || u.contains('#') {
        return None;
    }
    let scheme_end = u.find("://")? + 3;
    let path_start = u[scheme_end..].find('/').map(|i| i + scheme_end)?;
    let path = &u[path_start..];
    if path == "/" {
        return None; // 只剩根路径，两种写法等价
    }
    match path.strip_suffix('/') {
        Some(stripped) => Some(format!("{}{}", &u[..path_start], stripped)),
        None => Some(format!("{u}/")),
    }
}

/// 暴露给模型的完整工具名：`mcp__<server>__<tool>`
pub fn mcp_tool_full_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// 模型**实际看到并调用**的那个工具名（`tool_specs()` 用的就是这个口径）。
///
/// 为什么要单独一个函数、而不是直接 `mcp_tool_full_name(group, raw_name)`：
/// 1MCP 网关上的原始名是 `<组>_1mcp_<工具>`，直接拼会得到
/// `mcp__github__github_1mcp_create_issue` —— 这是个**不存在的工具名**，
/// 模型照着调只会拿到"未启用"的报错。必须先剥掉 `_1mcp_` 前缀，
/// 与 `tools.rs::tool_specs`（构建可调工具列表的地方）保持**同一口径**：
/// 两处口径一旦分叉，`search_tools` 给出的名字就是调不通的。
///
/// 非网关（独立 server）的工具没有前缀可剥，按 `standalone` 命名 ——
/// 与 `tool_specs` 的兜底分支一致。
pub fn mcp_tool_callable_name(group: &str, raw_name: &str) -> String {
    match raw_name.split_once("_1mcp_") {
        Some((_, tool)) if !tool.is_empty() => mcp_tool_full_name(group, tool),
        _ => mcp_tool_full_name("standalone", raw_name),
    }
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

    /// 末尾斜杠兜底：本机实测 `http://127.0.0.1:3050/mcp` = 404、`…/mcp/` = 200，
    /// 所以这一层必须有；同时**不能**乱补斜杠（会毁掉本来就正确的地址）。
    #[test]
    fn slash_variant_only_touches_path() {
        // 缺斜杠 → 补上（本机 1MCP 的实际情况）
        assert_eq!(
            slash_variant("http://127.0.0.1:3050/mcp").as_deref(),
            Some("http://127.0.0.1:3050/mcp/")
        );
        // 多个斜杠 → 去掉最后一个
        assert_eq!(
            slash_variant("http://127.0.0.1:3050/mcp/").as_deref(),
            Some("http://127.0.0.1:3050/mcp")
        );
        // 深路径同样只动末尾
        assert_eq!(
            slash_variant("https://host/api/v1/mcp").as_deref(),
            Some("https://host/api/v1/mcp/")
        );
        // ⚠️ 根路径不该变：`http://host` → `http://host/` 是另一个 URL，不能猜
        assert_eq!(slash_variant("http://host"), None);
        assert_eq!(slash_variant("http://host/"), None);
        // 带查询串/片段的不猜
        assert_eq!(slash_variant("http://host/mcp?x=1"), None);
        assert_eq!(slash_variant("http://host/mcp#frag"), None);
        assert_eq!(slash_variant(""), None);
        // 没有 scheme 的裸字符串也不猜（宁可原样失败，也不要拼出个怪 URL）
        assert_eq!(slash_variant("127.0.0.1:3050/mcp"), None);
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
        };
        reg.groups = vec![big("g1", 10), big("g2", 10)];
        reg.connected = true;

        reg.load("g1").expect("第一组应该能加载");
        reg.load("g2").expect("第二组也应能加载（预算已删除）");
        assert!(reg.is_active("g1") && reg.is_active("g2"));

        reg.unload("g1").unwrap();
        assert!(!reg.is_active("g1"));
    }

    /// 决策回归：**默认全给**。
    /// 拉完快照后，没被显式关过的组应当全部活跃；显式关掉的保持关闭。
    #[test]
    fn default_activates_all_groups_with_memory() {
        let mut reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        let g = |n: &str| McpGroup {
            name: n.into(),
            server_id: "default".into(),
            summary: "x".into(),
            tools: vec![tool(&format!("{n}_1mcp_t"))],
        };
        reg.groups = vec![g("a"), g("b"), g("c")];
        reg.connected = true;

        // 默认：一个都没记录 → 全给
        let mut st = crate::config::AgentSettings::default();
        assert!(st.mcp_all_groups, "默认应为「直接全给」");
        let n = reg.restore_active_from_settings_unrestricted(&st);
        assert_eq!(n, 3, "默认应激活全部 3 组");
        assert!(reg.is_active("a") && reg.is_active("b") && reg.is_active("c"));

        // 用户显式关掉 b → 只有 b 不活跃，重启后仍记得
        st.mcp_group_enabled.insert("b".into(), false);
        let n = reg.restore_active_from_settings_unrestricted(&st);
        assert_eq!(n, 2);
        assert!(reg.is_active("a") && !reg.is_active("b") && reg.is_active("c"));

        // 全局开关关掉 + 只显式开 c → 回到按需加载
        st.mcp_all_groups = false;
        st.mcp_group_enabled.insert("c".into(), true);
        let n = reg.restore_active_from_settings_unrestricted(&st);
        assert_eq!(n, 1, "按需模式下只激活显式打开的组");
        assert!(!reg.is_active("a") && !reg.is_active("b") && reg.is_active("c"));
    }

    /// 模式是**约束层**：与用户设置取交集，且只能收窄不能放宽。
    ///
    /// 这是整个「模式」功能的语义底线，写死在这里：
    ///   1. 用户关掉的组，模式给不回来（否则设置页的开关形同虚设）；
    ///   2. 模式没点名的组，用户开着也不给（否则模式什么也管不住）；
    ///   3. `"all"` 哨兵 = 当前全部组（含未来新增的）。
    #[test]
    fn mode_layer_intersects_user_settings_and_only_narrows() {
        let mut reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        let g = |n: &str| McpGroup {
            name: n.into(),
            server_id: "default".into(),
            summary: "x".into(),
            tools: vec![tool(&format!("{n}_1mcp_t"))],
        };
        reg.groups = vec![g("github"), g("mysql"), g("ssh")];
        reg.connected = true;

        let mut st = crate::config::AgentSettings::default();
        st.mcp_all_groups = true;

        // 白名单只给 github → 只有它活跃，用户那边什么设置都没改
        let only_github: HashSet<String> = ["github".to_string()].into_iter().collect();
        let n = reg.restore_active_from_settings(&st, Some(only_github.clone()));
        assert_eq!(n, 1);
        assert!(reg.is_active("github"));
        assert!(!reg.is_active("mysql") && !reg.is_active("ssh"));
        assert!(!st.mcp_group_enabled.contains_key("mysql"), "模式不得改写用户设置");
        assert_eq!(reg.mode_denied(), vec!["mysql".to_string(), "ssh".to_string()]);

        // 用户**显式开**了 mysql，但模式仍不给 → 依然不活跃（模式只能收窄）
        st.mcp_group_enabled.insert("mysql".into(), true);
        let n = reg.restore_active_from_settings(&st, Some(only_github));
        assert_eq!(n, 1);
        assert!(!reg.is_active("mysql"), "模式收窄优先于用户的「开」");

        // 模式给了 github + mysql，但用户显式关了 github → 交集只剩 mysql
        let pair: HashSet<String> = ["github".to_string(), "mysql".to_string()].into_iter().collect();
        st.mcp_group_enabled.insert("github".into(), false);
        let n = reg.restore_active_from_settings(&st, Some(pair));
        assert_eq!(n, 1, "用户关掉的不该被模式开回来");
        assert!(!reg.is_active("github") && reg.is_active("mysql"));

        // 哨兵 "all" → 全部组（含任何后来新增的）
        let all: HashSet<String> = [crate::modes::ALL.to_string()].into_iter().collect();
        st.mcp_group_enabled.clear();
        let n = reg.restore_active_from_settings(&st, Some(all));
        assert_eq!(n, 3);
        assert!(reg.mode_denied().is_empty());
    }

    /// 模式收起来的组，模型不能靠 `load_tool_group` 自己装回来
    /// —— 否则"模式"只是个建议。
    #[test]
    fn mode_denied_group_cannot_be_loaded_by_model() {
        let mut reg = ToolRegistry::new("http://127.0.0.1:1/mcp");
        reg.groups = vec![
            McpGroup {
                name: "github".into(),
                server_id: "default".into(),
                summary: "x".into(),
                tools: vec![tool("github_1mcp_create_issue")],
            },
            McpGroup {
                name: "ssh".into(),
                server_id: "default".into(),
                summary: "x".into(),
                tools: vec![tool("ssh_1mcp_run-command")],
            },
        ];
        reg.connected = true;
        let st = crate::config::AgentSettings::default();
        let only_github: HashSet<String> = ["github".to_string()].into_iter().collect();
        reg.restore_active_from_settings(&st, Some(only_github));

        let err = reg.load("ssh").unwrap_err();
        assert!(err.contains("不允许"), "{err}");
        assert!(!reg.is_active("ssh"));
        // 被收起来的组不该出现在给模型看的两份清单里
        assert!(!reg.describe_groups().contains("**ssh**"), "{}", reg.describe_groups());
        assert!(!reg.capability_index().contains("run-command"));
        assert!(!reg.search_tools("ssh").contains("run-command"));

        // 允许的组照常能加载
        assert!(reg.load("github").is_ok());
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

    // -- 能力发现索引 -------------------------------------------------------

    /// 复刻真实的 1MCP 网关形状：56 个工具 / 4 组 / 描述是「一句话概述 + 空行 +
    /// 用法细节 + 参数说明」。索引的成本断言必须建在这个形状上 ——
    /// 用 `desc of t0` 这种玩具描述测出来的 token 数没有任何说服力。
    ///
    /// 夹具本体在 `ToolRegistry::gateway_like_for_test`（`agent.rs` 的常驻成本
    /// 断言也要用同一份，两处各建一份迟早长成不同形状）。
    fn gateway_like_registry() -> ToolRegistry {
        ToolRegistry::gateway_like_for_test()
    }

    /// 🔴 本任务的核心不变量：索引必须**短**。
    ///
    /// 如果哪天有人往索引里加 schema、加参数列表、加例子，这条会先炸 ——
    /// 它就是"懒加载没被悄悄拆掉"的守门人。上限取 1.2k tokens：
    /// 实测 826（56 工具 / 4 组），留约 45% 余量给工具数从 56 长到 80，
    /// 但不允许它长成第二份 schema。
    #[test]
    fn index_stays_compact() {
        let reg = gateway_like_registry();
        let idx = reg.capability_index();
        let tokens = crate::llm::estimate_tokens(&idx);
        eprintln!("[orbcat][test] 能力索引：{} 字符 / 约 {tokens} tokens", idx.chars().count());
        assert!(
            tokens < 1200,
            "能力索引必须保持紧凑（实测 ~700 tokens），现在 {tokens} —— \
             是不是把 schema / 参数细节写进去了？那等于取消懒加载"
        );

        // 全部 56 个工具都要在，且一个不漏（漏了模型就又会去猜）
        for g in reg.groups() {
            assert!(idx.contains(&format!("**{}**", g.name)), "缺组：{}", g.name);
            for t in &g.tools {
                let short = short_tool_name(&t.name);
                assert!(idx.contains(short), "索引缺工具 {short}");
            }
        }

        // 索引必须是**名字 + 用途**：用途那半句要在（否则退回"只有组名"的旧缺陷）
        assert!(idx.contains("Create a new issue") || idx.contains("Execute a read-only SQL query"));
        // 而 schema 绝不能在
        assert!(!idx.contains("properties"), "索引不得携带 schema");
        assert!(!idx.contains("Usage:"), "索引不得携带描述正文（只取一句话）");
    }

    /// 描述压缩：多行描述只取**第一句有信息量的行**，超长按字符截断（不能切碎 UTF-8）。
    #[test]
    fn one_line_usage_takes_first_line_and_truncates() {
        let long = McpTool {
            name: "t".into(),
            description: format!("## {}\n\nUsage: whatever", "很".repeat(200)),
            input_schema: json!({}),
            annotations: None,
        };
        let u = one_line_usage(&long);
        assert!(u.starts_with('很'));
        assert!(u.ends_with('…'), "超长应截断：{u}");
        assert_eq!(u.chars().count(), INDEX_DESC_CHARS, "截断后应正好是上限长度");
        assert!(!u.contains("Usage:"), "第二行不该进来");

        // 首行是过渡句（太短没信息量）→ 退到下一行
        let fallback = McpTool {
            name: "t".into(),
            description: "Run\n从远程主机读取一个文件的内容".into(),
            input_schema: json!({}),
            annotations: None,
        };
        assert_eq!(one_line_usage(&fallback), "从远程主机读取一个文件的内容");

        // 完全没有描述 → 用工具名兜底，不能panic也不能给空串
        let none = McpTool {
            name: "mystery_tool".into(),
            description: String::new(),
            input_schema: json!({}),
            annotations: None,
        };
        assert!(one_line_usage(&none).contains("mystery_tool"));
    }

    /// `search_tools` 必须**按意图**命中：中文意图词、英文工具名、组名三条路都要通。
    #[test]
    fn search_matches_by_intent_name_and_group() {
        let reg = gateway_like_registry();

        // 英文词命中工具名/描述
        let r = reg.search_tools("screenshot");
        assert!(r.contains("browser_take_screenshot"), "{r}");
        assert!(r.contains("Screenshot-Tool"), "windows 组的截图也该命中：{r}");

        // 意图词不一定要和工具名同字：靠**描述**也要能命中
        // （真实场景："给那个仓库提个 issue" → 工具名 create_issue 命中）
        let r = reg.search_tools("read");
        assert!(r.contains("read-only-command") || r.contains("list-directory"), "{r}");

        // 组名命中 = 整组给出（用户说"用那个 ssh 组"）
        let r = reg.search_tools("ssh");
        assert!(r.contains("list-connections") && r.contains("run-command"), "{r}");

        // 大小写不敏感
        assert!(reg.search_tools("SCREENSHOT").contains("browser_take_screenshot"));

        // 命中结果里给出**可直接调用的全名**，而不是短名（省掉模型自己拼的一步）
        // 搜索命中结果必须给出**模型真正能调用的**名字：1MCP 网关的原始名是
        // `<组>_1mcp_<工具>`，直接拼会得到 `mcp__x__x_1mcp_y` 这种不存在的工具名。
        // （这条口径必须与 `tools.rs::tool_specs` 一致，否则模型照着搜到的名字调必失败。）
        assert!(
            reg.search_tools("execute_query").contains("mcp__mcp-server-mysql__execute_query"),
            "{}",
            reg.search_tools("execute_query")
        );
        assert!(
            !reg.search_tools("execute_query").contains("mcp-server-mysql_1mcp_"),
            "不得给出带网关前缀的工具名"
        );

        // 搜不到时要给可行动的出口（列出可用组），不能只说"没有"
        let miss = reg.search_tools("zzz-nothing-matches");
        assert!(miss.contains("没有匹配"), "{miss}");
        assert!(miss.contains("playwright-mcp") && miss.contains("ssh"), "{miss}");

        // 空 query = 完整索引（模型在问"我有什么工具"）
        assert_eq!(reg.search_tools("  "), reg.capability_index());
    }

    /// 索引缓存：注册表写的、`agent.rs` 读的必须是**同一份内容**（同一路径口径）。
    ///
    /// 这条守的是最容易静默失效的一环：路径算法两处各写一份、写岔了的话
    /// 索引永远读不出来，而 prompt 里只是"少一段"，没人会注意到。
    #[test]
    fn index_cache_roundtrip() {
        // 用带时间戳的目录：两个测试进程/多次运行共用一个固定名会互相污染
        // （上一次运行留下的索引会让"没有目录 = 不落盘"这条断言假失败）。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!("orbcat_mcp_index_cache_{stamp}"));
        std::fs::create_dir_all(&tmp).unwrap();
        let cache = ToolRegistry::index_cache_path_in(&tmp);

        let mut reg = gateway_like_registry();
        // 注入前：没目录 → 不落盘（单测不该有副作用）
        assert!(reg.index_cache_path().is_none());
        reg.persist_index();
        assert!(!cache.exists(), "没注入目录时不该写盘");

        // 注入后：拉快照（这里是手工造组 + persist）→ 落盘
        reg.set_index_cache_dir(&tmp);
        reg.persist_index();
        assert!(cache.exists(), "注入目录后应落盘");
        let cached = ToolRegistry::read_cached_index(&tmp);
        assert!(cached.contains("browser_click"), "{cached}");
        // 逐字节一致（读回时会 trim，所以两边都 trim 再比 ——
        // 这里守的是"写进去的和读出来的必须是同一份索引"，不是行尾空白）
        assert_eq!(
            cached,
            reg.capability_index().trim(),
            "读回的必须与当前索引一致"
        );
        assert_eq!(cached, std::fs::read_to_string(&cache).unwrap().trim());

        // 掉线：缓存要被覆盖成"未连接"，不能让旧索引继续在 prompt 里骗模型
        reg.apply_snapshot(Err("boom".into()));
        let cached = ToolRegistry::read_cached_index(&tmp);
        assert!(cached.contains("未连接"), "{cached}");
        assert!(!cached.contains("browser_click"), "掉线后不该还列出工具");

        // 读不到文件（例如从没拉过）→ 空串，不 panic
        // （要删**文件**：删目录后 `read_to_string` 仍能读到文件，等于没测到）
        std::fs::remove_file(&cache).unwrap();
        assert_eq!(ToolRegistry::read_cached_index(&tmp), "");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 索引里每组的「已加载 / 未加载」标记要跟着活跃集走 ——
    /// 标错会让模型以为工具已经能用，直接调用后拿到"未启用"错误。
    #[test]
    fn index_marks_active_state() {
        let mut reg = gateway_like_registry();
        reg.active.clear();
        let idx = reg.capability_index();
        assert!(idx.contains("（24 个，未加载）"), "{idx}");

        reg.active.insert("playwright-mcp".into());
        let idx = reg.capability_index();
        assert!(idx.contains("（24 个，已加载）"), "{idx}");
        assert!(idx.contains("（11 个，未加载）"), "{idx}");
    }
}
