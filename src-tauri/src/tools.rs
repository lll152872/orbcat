//! 工具层 —— agent 能调用的动作
//!
//! ## 🔴 铁律
//! **所有**文件操作必须先过 [`PermissionGate`]。
//! 因此每个工具函数签名都强制带 `&PermissionGate` —— 从类型层面让"绕过权限"写不出来。
//!
//! ## 设计约定
//! - 工具名、参数 schema、描述文案参考 Pi / 主流 agent 的成熟做法
//! - 一切读取都有**上限**（条数/字节），防止把上下文撑爆
//! - 写入前**自动备份**（可逆优先）

use std::path::Path;

use serde_json::{json, Value};

use crate::permission::{Access, PermissionGate};

// ---------------------------------------------------------------------------
// 上限
// ---------------------------------------------------------------------------

/// 单次 read_file 最多返回的字节数
const MAX_READ_BYTES: usize = 256 * 1024;
/// 单次 list_dir 最多返回的条目数
const MAX_LIST_ENTRIES: usize = 500;
/// 单次 glob 最多返回的匹配数
const MAX_GLOB_MATCHES: usize = 300;
/// grep 最多返回的匹配行数
const MAX_GREP_HITS: usize = 200;

// ---------------------------------------------------------------------------
// 工具规格
// ---------------------------------------------------------------------------

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    fn new(name: &str, description: &str, parameters: Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
        }
    }
}

/// 内置工具（常驻，每轮都在 tools 里）
pub fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "read_file",
            "读取一个文本文件的内容。返回带行号的文本。仅在确定路径存在时使用；\
             若不确定路径，先用 glob_files 或 list_dir 查找。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件的绝对路径" },
                    "offset": { "type": "integer", "description": "从第几行开始读（1 起，可选）" },
                    "limit": { "type": "integer", "description": "最多读多少行（可选）" }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::new(
            "list_dir",
            "列出目录下的条目（文件与子目录），含大小与是否为目录。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目录的绝对路径" }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::new(
            "glob_files",
            "按 glob 模式查找文件，例如 `D:/myword/**/*.md`。支持 `*` `**` `?`。\
             用于在不清楚具体路径时定位文件。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "glob 模式（用正斜杠）" }
                },
                "required": ["pattern"]
            }),
        ),
        ToolSpec::new(
            "grep_files",
            "在指定目录下递归搜索包含某段文本的文件，返回 文件:行号:内容。\
             用于定位某个符号或字符串在哪里定义/使用。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "要搜索的文本（子串匹配）" },
                    "path": { "type": "string", "description": "搜索根目录（绝对路径）" },
                    "ext": { "type": "string", "description": "只搜这些扩展名，如 'rs,md'（可选）" }
                },
                "required": ["pattern", "path"]
            }),
        ),
        ToolSpec::new(
            "write_file",
            "写入（覆盖）一个文本文件。**写入前会自动备份原文件**。\
             只能写入权限网关允许的目录。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件的绝对路径" },
                    "content": { "type": "string", "description": "要写入的完整内容" }
                },
                "required": ["path", "content"]
            }),
        ),
        ToolSpec::new(
            "capture_screen",
            "截取用户当前的整个屏幕，用于「看看我屏幕上是什么」。\
             当用户提到「这个报错」「我屏幕上」「这张图」「当前画面」，\
             或者你需要了解用户正在看什么时调用。\
             图片会在下一条消息里附上，你届时就能看到。",
            json!({ "type": "object", "properties": {}, "required": [] }),
        ),
        ToolSpec::new(
            "foreground_context",
            "获取用户**此刻**正在使用的前台应用及其正在浏览的目录/文件\
             （VSCode、Typora 等能解析到具体路径）。\
             当 system prompt 里的「用户当前正在看什么」可能过期，\
             或用户刚切换了窗口时调用。",
            json!({ "type": "object", "properties": {}, "required": [] }),
        ),
        ToolSpec::new(
            "remember",
            "把一条值得**长期记住**的信息提交为记忆候选（用户确认后才会真正进入长期记忆）。\
             适用：用户的偏好、常用路径、项目背景、明确的长期约定。\
             不要用：临时性信息、一次性任务、闲聊内容。",
            json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "要记住的信息，一句话，自包含（不要写\"他\"\"上面说的\"这类指代）" }
                },
                "required": ["content"]
            }),
        ),
        ToolSpec::new(
            "load_skill",
            "加载一个技能的完整操作手册（SKILL.md 全文 + 附属文件清单）。\
             当任务与 system prompt 技能清单里的某条匹配时，先加载再照做 —— \
             清单里只有一行描述，细节全在正文里。",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名（清单里的加粗名字，原样传）" }
                },
                "required": ["name"]
            }),
        ),
        ToolSpec::new(
            "save_skill",
            "把一套可复用的操作流程保存为技能（写入 agent-data/skills/<name>/SKILL.md）。\
             时机：用户明确要求记住某流程；或你刚摸索出一套多步骤、验证过的做法\
             （含踩坑修复），判断今后会再用。description 必须写清触发场景\
             （「当用户…时使用」），正文写具体步骤与命令。重名默认拒绝，\
             修正既有技能才带 overwrite=true。",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名：小写字母/数字/连字符，如 pdf-report-fill" },
                    "description": { "type": "string", "description": "一行描述：做什么 + 什么时候用（进清单给模型看）" },
                    "content": { "type": "string", "description": "技能正文（markdown）：步骤、命令、注意事项" },
                    "overwrite": { "type": "boolean", "description": "覆盖同名技能（默认 false；仅用于修正既有技能）" }
                },
                "required": ["name", "description", "content"]
            }),
        ),
        ToolSpec::new(
            "list_tool_groups",
            "列出可用的 MCP 外部工具组（浏览器自动化、远程 SSH、数据库、桌面操作等）。\
             MCP 工具**不会默认加载**（为了省上下文），你需要先看这里有哪些组，\
             再用 load_tool_group 把需要的组装载进来。\
             当任务涉及网页、远程服务器、数据库、桌面应用操作时，先调用这个。",
            json!({ "type": "object", "properties": {}, "required": [] }),
        ),
        ToolSpec::new(
            "load_tool_group",
            "加载一个 MCP 工具组，使其工具在后续轮次可用。\
             加载后你会收到该组全部工具的**名字清单**。\
             遵守 token 预算，一次只加载当前任务真正需要的那一组。",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "组名，如 ssh / playwright-mcp / windows-mcp / mcp-server-mysql" }
                },
                "required": ["name"]
            }),
        ),
        ToolSpec::new(
            "unload_tool_group",
            "卸载一个已加载的 MCP 工具组，释放上下文预算。\
             当某个组的任务已完成、或需要给别的组腾地方时调用。",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "组名" }
                },
                "required": ["name"]
            }),
        ),
        ToolSpec::new(
            "recall_turns",
            "查找本会话**更早**的对话原文（不在你当前上下文里的部分）。\
             当你看到用户说「上次 / 之前 / 刚才那个 / 我们之前定的」\
             而你找不到依据时，**先调这个再回答**，不要猜也不要反问用户。\
             留空 query 可用来「翻一下最近聊过什么」。",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "关键词，如「跳页」「构建报错」。留空则按时间返回最近的记录"
                    },
                    "hours": {
                        "type": "number",
                        "description": "只看最近 N 小时内的对话（可选，不传则全时段）"
                    },
                    "limit": {
                        "type": "number",
                        "description": "最多返回几条，默认 10"
                    }
                },
                "required": []
            }),
        ),
    ]
}

/// 完整工具列表 = 内置 + **已加载的** MCP 工具
///
/// ⚠️ 这个函数每轮都会被调用（因为模型可能中途 load 了新组），
/// 所以它必须只做拼接、不产生副作用（早先用 Box::leak 会反复泄漏）。
pub fn tool_specs(mcp: Option<&crate::mcp::ToolRegistry>) -> Vec<ToolSpec> {
    let mut specs = builtin_specs();

    let Some(reg) = mcp else { return specs };

    for t in reg.active_tools() {
        let full = match t.name.split_once("_1mcp_") {
            Some((server, tool)) => crate::mcp::mcp_tool_full_name(server, tool),
            None => format!("mcp__standalone__{}", t.name),
        };

        let mut desc = t.description.trim().to_string();
        if t.is_destructive() {
            desc.push_str(" ⚠️ 此操作会修改数据或执行命令，调用前请确认用户意图。");
        }

        specs.push(ToolSpec {
            name: full,
            description: desc,
            parameters: t.input_schema.clone(),
        });
    }

    specs
}

// ---------------------------------------------------------------------------
// 执行入口
// ---------------------------------------------------------------------------

/// 工具的执行结果。
///
/// 为什么不是简单返回 `String`：
///   OpenAI 的 `tool` 角色消息**只能承载文本**，图片塞不进去。
///   所以截图这类工具需要把图片**单独带出来**，由 agent loop 作为一条
///   `user` 消息（多模态）插入对话 —— 模型下一轮才看得到。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 给模型看的文本结果
    pub text: String,
    /// 可选图片**文件路径**（由 agent loop 按模型能力转 base64 / 占位，作为
    /// 一条 user 消息插入对话）
    pub image: Option<String>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self { text: s.into(), image: None }
    }
}

/// 工具的调用上下文。
///
/// MCP 注册表用 `tokio::sync::Mutex` 是因为工具执行可能跨 await，
/// 而 std 的 MutexGuard 不能跨 await 点。
pub struct ToolCtx<'a> {
    pub gate: &'a PermissionGate,
    pub data_dir: &'a Path,
    pub mcp: &'a tokio::sync::Mutex<crate::mcp::ToolRegistry>,
    /// 当前会话 id。`recall_turns` 只在本会话范围内检索 ——
    /// 跨会话检索是独立功能（"从主聊天起任务"才需要），暂不做。
    pub session_id: &'a str,
}

/// 执行一个工具调用。
pub async fn execute(
    name: &str,
    args: &Value,
    ctx: &ToolCtx<'_>,
) -> Result<ToolOutput, String> {
    match name {
        "read_file" => read_file(args, ctx.gate),
        "list_dir" => list_dir(args, ctx.gate),
        "glob_files" => glob_files(args, ctx.gate),
        "grep_files" => grep_files(args, ctx.gate),
        "write_file" => write_file(args, ctx.gate, ctx.data_dir),
        "capture_screen" => capture_screen(ctx.data_dir),
        "foreground_context" => {
            let out = match crate::context::cached().or_else(crate::context::current) {
                Some(c) => c.summary(),
                None => "无法获取前台窗口信息".into(),
            };
            Ok(ToolOutput::text(out))
        }
        "remember" => remember(args, ctx.data_dir),
        "recall_turns" => recall_turns(args, ctx),
        "load_skill" => {
            let name = get_str(args, "name")?;
            // 技能正文可能带命令示例，但那是「模型照着敲」的，不经过这里；
            // 加载本身只是读文件，且目录在 data_dir 内 —— 不过仍显式走网关，
            // 与其他文件工具保持同一纪律。
            if let Err(e) = ctx.gate.check(&crate::skills::skills_dir(ctx.data_dir), Access::Read) {
                return Err(format!("技能目录无读取权限：{e}"));
            }
            let text = crate::skills::load(ctx.data_dir, &name)?;
            Ok(ToolOutput::text(text))
        }
        "save_skill" => {
            let name = get_str(args, "name")?;
            let desc = get_str(args, "description")?;
            let content = get_str(args, "content")?;
            let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
            let msg = crate::skills::save(ctx.data_dir, &name, &desc, &content, overwrite)?;
            Ok(ToolOutput::text(msg))
        }
        "list_tool_groups" => {
            let reg = ctx.mcp.lock().await;
            Ok(ToolOutput::text(reg.describe_groups()))
        }
        "load_tool_group" => {
            let group = get_str(args, "name")?;
            let mut reg = ctx.mcp.lock().await;
            let msg = reg.load(&group)?;
            eprintln!("[float-agent] MCP 组已加载: {group}");
            Ok(ToolOutput::text(msg))
        }
        "unload_tool_group" => {
            let group = get_str(args, "name")?;
            let mut reg = ctx.mcp.lock().await;
            Ok(ToolOutput::text(reg.unload(&group)?))
        }
        other => {
            // MCP 工具：mcp__<server>__<tool>
            if let Some((_server, tool)) = crate::mcp::parse_mcp_tool_name(other) {
                let reg = ctx.mcp.lock().await;
                // 确认这个工具确实在已加载的组里（防止模型调用未加载的工具）
                let loaded = reg
                    .active_tools()
                    .iter()
                    .any(|t| t.name.ends_with(&tool));
                if !loaded {
                    return Err(format!(
                        "工具「{other}」所属的组尚未加载，请先用 list_tool_groups 查看、\
                         再用 load_tool_group 加载对应组。"
                    ));
                }
                let msg = reg.call_tool(&tool_with_server(other), args).await?;
                return Ok(ToolOutput::text(msg));
            }
            Err(format!("未知工具：{other}"))
        }
    }
}

/// `mcp__ssh__run-command` → 原始 MCP 工具名 `ssh_1mcp_run-command`
fn tool_with_server(full: &str) -> String {
    match crate::mcp::parse_mcp_tool_name(full) {
        Some((server, tool)) => format!("{server}_1mcp_{tool}"),
        None => full.to_string(),
    }
}

// ---------------------------------------------------------------------------
// recall_turns
// ---------------------------------------------------------------------------

/// 查本会话更早的对话原文。
///
/// 为什么需要它：`history.rs` 只回灌「最近 6h ∪ 最近 10 条」，
/// 更早的内容不在上下文里。没有这个工具，模型遇到"上次那个方案"
/// 只能瞎编或反问用户。原文**永久**留在 `sessions/<id>.json`，这里负责捞回来。
///
/// 范围**只限当前会话**：跨会话检索是独立功能（"从主聊天起任务"才需要）。
fn recall_turns(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    if ctx.session_id.is_empty() {
        return Ok(ToolOutput::text("当前没有会话上下文，无法查找历史。"));
    }

    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(10);
    let hours = args.get("hours").and_then(Value::as_u64);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let hits = crate::history::search(ctx.data_dir, ctx.session_id, &query, limit, hours, now);

    if hits.is_empty() {
        return Ok(ToolOutput::text(format!(
            "没找到匹配的对话记录{}。\n可能原因：关键词不对，或本会话确实没聊过这个话题。\n\
             可以试着换个关键词，或不传 query 翻一下最近的记录。",
            if query.is_empty() {
                String::new()
            } else {
                format!("（关键词：{query}）")
            }
        )));
    }

    let scope = if query.is_empty() {
        "最近的".to_string()
    } else {
        format!("含「{query}」的")
    };
    let mut out = format!("在本会话历史中找到 {} 条{}记录：\n", hits.len(), scope);
    for (i, h) in hits.iter().enumerate() {
        let who = if h.role == "user" { "用户" } else { "你" };
        out.push_str(&format!("\n{}. [{}] {}\n   {}\n", i + 1, h.when, who, h.text));
    }
    out.push_str(
        "\n（以上是历史记录的节选，工具输出已在落盘时折叠成一行。\
         基于这些回答，并说明是「从更早的对话里找到的」。）",
    );

    Ok(ToolOutput::text(out))
}

// ---------------------------------------------------------------------------
// remember
// ---------------------------------------------------------------------------

/// 提交长期记忆候选。不直接写 MEMORY.md —— 要等用户审批。
fn remember(args: &Value, data_dir: &Path) -> Result<ToolOutput, String> {
    let content = get_str(args, "content")?;
    let store = crate::memory::MemoryStore::new(data_dir);
    let c = store.propose(&content, "model")?;
    Ok(ToolOutput::text(format!(
        "已提交记忆候选（id: {}），等用户在面板里确认后才会进入长期记忆。",
        c.id
    )))
}

// ---------------------------------------------------------------------------
// capture_screen
// ---------------------------------------------------------------------------

/// 截取当前主屏幕。图片**存盘后把路径**随 `ToolOutput.image` 返回，
/// 由 agent loop 插成 user 消息（按模型能力转 base64 或占位）。
///
/// ⚠️ 只截一次屏：早先这里同时调了 `capture_screen_to_file` 和
/// `capture_screen_data_url`，等于**一次工具调用截两遍屏**，纯浪费。
fn capture_screen(data_dir: &Path) -> Result<ToolOutput, String> {
    use crate::capture::ShotFormat;

    let shot_dir = data_dir.join("screenshots");
    let (path, w, h) = crate::capture::capture_screen_to_file(&shot_dir, ShotFormat::Jpeg)?;

    Ok(ToolOutput {
        text: format!(
            "已截取主屏幕（{w}x{h}），图片已附加在下一条消息里。\n保存位置：{}",
            path.display()
        ),
        image: Some(path.to_string_lossy().into_owned()),
    })
}

fn get_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("缺少参数 `{key}`（应为字符串）"))
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

fn read_file(args: &Value, gate: &PermissionGate) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe_path = gate.check(&raw, Access::Read).map_err(|e| e.to_string())?;

    if safe_path.is_dir() {
        return Err(format!(
            "{} 是一个目录，请用 list_dir",
            safe_path.display()
        ));
    }

    let bytes = std::fs::read(&safe_path).map_err(|e| format!("读取失败: {e}"))?;
    let truncated = bytes.len() > MAX_READ_BYTES;
    let slice = &bytes[..bytes.len().min(MAX_READ_BYTES)];
    let text = String::from_utf8_lossy(slice).to_string();

    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = args.get("limit").and_then(Value::as_u64).map(|v| v as usize);

    let mut out = String::new();
    let mut shown = 0usize;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if n < offset {
            continue;
        }
        if let Some(l) = limit {
            if shown >= l {
                break;
            }
        }
        out.push_str(&format!("{n}\t{line}\n"));
        shown += 1;
    }

    if truncated {
        out.push_str(&format!(
            "\n… [文件超过 {} KB，已截断]\n",
            MAX_READ_BYTES / 1024
        ));
    }
    if out.is_empty() {
        out.push_str("(文件为空，或指定的行范围没有内容)");
    }

    Ok(ToolOutput::text(out))
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

fn list_dir(args: &Value, gate: &PermissionGate) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe_path = gate.check(&raw, Access::Read).map_err(|e| e.to_string())?;

    if !safe_path.is_dir() {
        return Err(format!("{} 不是目录", safe_path.display()));
    }

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();

    let entries = std::fs::read_dir(&safe_path).map_err(|e| format!("读目录失败: {e}"))?;
    let mut count = 0usize;
    for e in entries.flatten() {
        if count >= MAX_LIST_ENTRIES {
            break;
        }
        count += 1;
        let name = e.file_name().to_string_lossy().to_string();
        match e.metadata() {
            Ok(md) if md.is_dir() => dirs.push(name),
            Ok(md) => files.push((name, md.len())),
            Err(_) => files.push((name, 0)),
        }
    }

    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = format!("{}\n", safe_path.display());
    out.push_str(&format!("目录 {} 个：\n", dirs.len()));
    for d in &dirs {
        out.push_str(&format!("  [DIR]  {d}\n"));
    }
    out.push_str(&format!("文件 {} 个：\n", files.len()));
    for (f, sz) in &files {
        out.push_str(&format!("  {:>9}B  {f}\n", sz));
    }
    if count >= MAX_LIST_ENTRIES {
        out.push_str(&format!("\n… [已截断，最多列 {MAX_LIST_ENTRIES} 项]\n"));
    }

    Ok(ToolOutput::text(out))
}

// ---------------------------------------------------------------------------
// glob_files
// ---------------------------------------------------------------------------

fn glob_files(args: &Value, gate: &PermissionGate) -> Result<ToolOutput, String> {
    let pattern = get_str(args, "pattern")?;

    // 先对模式的基础目录做一次权限检查，避免扫到没权限的区域
    let base = pattern
        .split('*')
        .next()
        .unwrap_or(&pattern)
        .trim_end_matches('/');
    let base_path = if base.is_empty() {
        pattern.clone()
    } else {
        base.to_string()
    };
    let _ = gate
        .check(&base_path, Access::Read)
        .map_err(|e| format!("搜索起点无权限：{e}"))?;

    let mut hits: Vec<String> = Vec::new();
    let mut denied = 0usize;

    let paths = glob::glob(&pattern).map_err(|e| format!("glob 模式无效: {e}"))?;

    for p in paths.flatten() {
        if hits.len() >= MAX_GLOB_MATCHES {
            break;
        }
        // 逐个结果再过一次权限网关（防止模式匹配到越界路径）
        match gate.check(&p, Access::Read) {
            Ok(safe) => hits.push(safe.to_string_lossy().into_owned()),
            Err(_) => denied += 1,
        }
    }

    let mut out = format!("模式：{pattern}\n匹配 {} 个", hits.len());
    if denied > 0 {
        out.push_str(&format!("（另外 {denied} 个因权限被过滤）"));
    }
    out.push('\n');
    for h in &hits {
        out.push_str(&format!("  {h}\n"));
    }
    if hits.len() >= MAX_GLOB_MATCHES {
        out.push_str(&format!("\n… [已截断，最多 {MAX_GLOB_MATCHES} 条]\n"));
    }
    if hits.is_empty() {
        out.push_str("(无匹配)\n");
    }

    Ok(ToolOutput::text(out))
}

// ---------------------------------------------------------------------------
// grep_files
// ---------------------------------------------------------------------------

fn grep_files(args: &Value, gate: &PermissionGate) -> Result<ToolOutput, String> {
    let needle = get_str(args, "pattern")?;
    let raw = get_str(args, "path")?;
    let roots = gate.check(&raw, Access::Read).map_err(|e| e.to_string())?;

    let exts: Vec<String> = args
        .get("ext")
        .and_then(Value::as_str)
        .map(|s| s.split(',').map(|x| x.trim().to_lowercase()).collect())
        .unwrap_or_default();

    let mut hits: Vec<String> = Vec::new();
    let mut visited = 0usize;

    walk(&roots, &mut |p: &Path| {
        if hits.len() >= MAX_GREP_HITS {
            return;
        }
        visited += 1;
        if visited > 20000 {
            return;
        }

        // 扩展名过滤
        if !exts.is_empty() {
            let ok = p
                .extension()
                .map(|e| exts.contains(&e.to_string_lossy().to_lowercase()))
                .unwrap_or(false);
            if !ok {
                return;
            }
        }

        // 大文件跳过
        let Ok(md) = p.metadata() else { return };
        if md.len() > 2 * 1024 * 1024 {
            return;
        }

        // 权限：逐个文件再确认（防止 walk 过程中穿过 junction）
        let Ok(safe) = gate.check(p, Access::Read) else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(&safe) else {
            return;
        };

        for (i, line) in content.lines().enumerate() {
            if hits.len() >= MAX_GREP_HITS {
                return;
            }
            if line.contains(&needle) {
                let trimmed: String = line.trim().chars().take(200).collect();
                hits.push(format!("{}:{}: {}", safe.display(), i + 1, trimmed));
            }
        }
    });

    let mut out = format!("在 {} 中搜索「{}」，命中 {} 行\n", roots.display(), needle, hits.len());
    for h in &hits {
        out.push_str(h);
        out.push('\n');
    }
    if hits.len() >= MAX_GREP_HITS {
        out.push_str(&format!("\n… [已截断，最多 {MAX_GREP_HITS} 行]\n"));
    }
    if hits.is_empty() {
        out.push_str("(无匹配)\n");
    }

    Ok(ToolOutput::text(out))
}

/// 递归遍历（不跟随目录符号链接，避免出界）
fn walk<F: FnMut(&Path)>(root: &Path, f: &mut F) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            // 跳过常见的大目录
            let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if matches!(name.as_str(), "node_modules" | "target" | ".git" | "dist" | ".next") {
                continue;
            }
            walk(&p, f);
        } else if ft.is_file() {
            f(&p);
        }
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

fn write_file(
    args: &Value,
    gate: &PermissionGate,
    data_dir: &Path,
) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let content = get_str(args, "content")?;
    let safe_path = gate
        .check(&raw, Access::ReadWrite)
        .map_err(|e| e.to_string())?;

    let mut note = String::new();

    // --- 可逆优先：写前备份 ---
    if safe_path.exists() {
        let backup_dir = data_dir.join("backups");
        std::fs::create_dir_all(&backup_dir).map_err(|e| format!("建备份目录失败: {e}"))?;

        let stem = safe_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let backup = backup_dir.join(format!("{stamp}__{stem}"));

        std::fs::copy(&safe_path, &backup).map_err(|e| format!("备份失败: {e}"))?;
        note = format!("\n（原文件已备份到 {}）", backup.display());
    }

    if let Some(parent) = safe_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }

    let len = content.len();
    std::fs::write(&safe_path, content).map_err(|e| format!("写入失败: {e}"))?;

    Ok(ToolOutput::text(format!(
        "已写入 {}（{len} 字节）{note}",
        safe_path.display()
    )))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::ToolRegistry;
    use crate::permission::PermissionGate;

    fn gate_for(dir: &Path) -> PermissionGate {
        let mut g = PermissionGate::new();
        g.add_rule(dir, Access::Full, "test");
        g
    }

    /// 测试用的 ctx（不连 MCP、无会话）
    macro_rules! ctx_for {
        ($gate:expr, $dir:expr, $mcp:expr) => {
            ToolCtx {
                gate: &$gate,
                data_dir: $dir,
                mcp: &$mcp,
                session_id: "",
            }
        };
    }

    /// 带会话 id 的 ctx（测 recall_turns 用）
    macro_rules! ctx_with_session {
        ($gate:expr, $dir:expr, $mcp:expr, $sid:expr) => {
            ToolCtx {
                gate: &$gate,
                data_dir: $dir,
                mcp: &$mcp,
                session_id: $sid,
            }
        };
    }

    #[test]
    fn tool_specs_have_required_fields() {
        let specs = builtin_specs();
        assert!(specs.len() >= 8, "内置工具应有 8 个以上，实际 {}", specs.len());
        for s in &specs {
            assert!(!s.name.is_empty());
            assert!(!s.description.is_empty());
            assert!(s.parameters.get("type").is_some());
        }
        // 三个 MCP 元工具必须在
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"list_tool_groups"));
        assert!(names.contains(&"load_tool_group"));
        assert!(names.contains(&"unload_tool_group"));
    }

    #[test]
    fn specs_include_loaded_mcp_tools_only() {
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let before = tool_specs(Some(&mcp.try_lock().unwrap())).len();
        assert_eq!(before, builtin_specs().len(), "初始不应带 MCP 工具");
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = std::env::temp_dir().join("float_agent_tool_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let target = tmp.join("hello.txt");

        let w = execute(
            "write_file",
            &json!({ "path": target.to_string_lossy(), "content": "line1\nline2\n" }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(w.text.contains("已写入"));

        let r = execute(
            "read_file",
            &json!({ "path": target.to_string_lossy() }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(r.text.contains("line1"));
        assert!(r.text.contains("2\tline2"));

        // 备份目录应该被创建（第二次写入时）
        let w2 = execute(
            "write_file",
            &json!({ "path": target.to_string_lossy(), "content": "x" }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(w2.text.contains("已备份"), "第二次写应产生备份: {}", w2.text);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn read_outside_allowed_is_denied() {
        let tmp = std::env::temp_dir().join("float_agent_perm_test");
        let _ = std::fs::create_dir_all(&tmp);
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        let r = execute(
            "read_file",
            &json!({ "path": "C:\\Windows\\win.ini" }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await;
        assert!(r.is_err(), "越权读取必须失败");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn list_dir_marks_subdirs() {
        let tmp = std::env::temp_dir().join("float_agent_list_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("a.txt"), b"hi").unwrap();

        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        let out = execute(
            "list_dir",
            &json!({ "path": tmp.to_string_lossy() }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(out.text.contains("[DIR]  sub"));
        assert!(out.text.contains("a.txt"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn calling_unloaded_mcp_tool_is_rejected() {
        let tmp = std::env::temp_dir().join("float_agent_mcp_reject_test");
        let _ = std::fs::create_dir_all(&tmp);
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        let r = execute(
            "mcp__ssh__run-command",
            &json!({ "cmd": "ls" }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await;
        assert!(r.is_err());
        assert!(
            r.unwrap_err().contains("尚未加载"),
            "应提示先加载组"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---------------- recall_turns ----------------

    #[tokio::test]
    async fn recall_turns_finds_earlier_conversation() {
        let tmp = std::env::temp_dir().join("float_agent_recall_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        // 造一个有历史的会话
        let s = crate::sessions::new_session(&tmp);
        crate::sessions::append_turn(&tmp, "先把 venera 的跳页 bug 修了", &[], "好的", &[]).unwrap();
        crate::sessions::append_turn(&tmp, "再顺手看看 apidash", &[], "行", &[]).unwrap();
        let sid = s.id.as_str();

        // 关键词命中
        let out = execute(
            "recall_turns",
            &json!({ "query": "跳页" }),
            &ctx_with_session!(gate, &tmp, mcp, sid),
        )
        .await
        .unwrap();
        assert!(out.text.contains("跳页"), "应返回命中内容: {}", out.text);
        assert!(out.text.contains("venera"));
        assert!(!out.text.contains("apidash"), "不该带出无关记录");
        assert!(out.text.contains("用户"), "应标明说话人");

        // 空 query → 返回最近的
        let out = execute(
            "recall_turns",
            &json!({ "limit": 10 }),
            &ctx_with_session!(gate, &tmp, mcp, sid),
        )
        .await
        .unwrap();
        assert!(out.text.contains("apidash"), "空 query 应返回最近记录");

        // 没命中 → 明确说明，而不是报错
        let out = execute(
            "recall_turns",
            &json!({ "query": "完全不存在的词xyz" }),
            &ctx_with_session!(gate, &tmp, mcp, sid),
        )
        .await
        .unwrap();
        assert!(out.text.contains("没找到"), "无命中应友好提示: {}", out.text);

        // 无会话上下文 → 友好提示，不 panic
        let out = execute(
            "recall_turns",
            &json!({ "query": "x" }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(out.text.contains("没有会话上下文"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recall_turns_is_in_builtin_specs() {
        let names: Vec<String> = builtin_specs().into_iter().map(|s| s.name).collect();
        assert!(
            names.iter().any(|n| n == "recall_turns"),
            "recall_turns 必须在内置工具里: {names:?}"
        );
    }
}
