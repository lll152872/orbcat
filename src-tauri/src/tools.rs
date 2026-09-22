//! 工具层 —— agent 能调用的动作
//!
//! ## 🔴 铁律
//! **所有**文件操作必须走 [`authorize`]。
//! 因此每个工具函数签名都强制带 [`ToolCtx`] —— 从类型层面让"绕过权限"写不出来。
//!
//! 为什么不是直接调 `gate.check`：用户当场批的**临时授权**不在规则里，
//! 而且 `Once` 档要在放行那一刻就扣次数 —— 判定与扣减必须一起做完，
//! 拆成两步迟早有人忘掉后一步，那"一次"就悄悄变成了"永久"。见 [`authorize`]。
//!
//! ## 设计约定
//! - 工具名、参数 schema、描述文案参考 Pi / 主流 agent 的成熟做法
//! - 一切读取都有**上限**（条数/字节），防止把上下文撑爆
//! - 写入前**自动备份**（可逆优先）

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::command_policy::{self, CmdScope, CmdVerdict};
use crate::perm_request::PermHub;
use crate::permission::{Access, DenyReason, GrantTier, PermissionGate};

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

/// 内置工具（常驻，每轮都在 tools 里）。
/// **不含** `web_search` —— 它只在配置了搜索后端时由 [`tool_specs`] 注入。
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
            "edit_file",
            "局部编辑一个文本文件：把 `old_string` **精确**替换成 `new_string`。\
             改代码/文档优先用这个，不要整文件 write_file 覆盖。\
             `old_string` 必须与文件原文**完全一致**（含缩进与空白）；\
             多次命中且未设 replace_all 时会报错，此时加长片段带上下文。\
             **改前自动备份**。受文件权限网关管辖（需 readwrite）。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件的绝对路径" },
                    "old_string": { "type": "string", "description": "文件中要替换的原文（须唯一，或配合 replace_all）" },
                    "new_string": { "type": "string", "description": "替换成的新文本" },
                    "replace_all": { "type": "boolean", "description": "是否替换全部命中（默认 false，只替第一处且要求唯一）" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        ToolSpec::new(
            "delete_file",
            "删除文件或目录 —— **永远归档，不硬删**：目标会被 move 到 \
             `agent-data/.trash/`，并记录原路径与时间，可手动还原。\
             即使用户说「硬删 / 永久删」也只归档；需要物理删除时告诉用户自行执行。\
             目录会先列出受影响文件再整目录归档。\
             受文件权限网关管辖（需 **full**）。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "要删除（归档）的文件或目录绝对路径" }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::new(
            "run_command",
            "在用户机器上执行一条 **PowerShell** 命令并返回输出（stdout/stderr/退出码）。\
             **受命令权限策略管辖**：只读命令（git status / ls / rg 等）直接放行；\
             危险命令（递归删除 / 格式化 / 关机 / 改注册表等）被**硬性阻断**（不可申请）；\
             其余命令会让你弹出确认卡片让用户当场拍板 —— 被拒就照用户给的理由调整，\
             不要原样重试。\
             默认工作目录是 agent 数据目录；用 `cwd` 指定别的目录时，该目录必须先有文件权限。\
             命令默认 60 秒超时，超时会被强制终止（连同子进程）。\
             用 PowerShell 语法（不是 cmd，也不是 bash）。",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的 PowerShell 命令（单条或复合，可用 ; | && 串联）"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "工作目录绝对路径（可选；默认 agent 数据目录。受文件权限网关管辖）"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "超时秒数（可选，默认 60，范围 1-600）"
                    }
                },
                "required": ["command"]
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
             适用：用户偏好/雷区/长期约定；**做完一轮后**可复用的「做过什么」；失败时的「错因与规避」。\
             不要用：临时状态、一次路径、闲聊、未验证猜测。任务完成或失败收尾时优先考虑调用本工具。",
            json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "要记住的信息，一句话，自包含（不要写\"他\"\"上面说的\"这类指代）" }
                },
                "required": ["content"]
            }),
        ),
        ToolSpec::new(
            "dispatch_task",
            "把**本助手解决不了**的任务分发给重 agent（首期 target=workbuddy）。\
             会从当前会话总结有效消息并附上图片，写交接文件后由**用户第一轮手动**\
             把内容交给 WorkBuddy；不会自动替 WB 开会话（自动化属第二轮，本期不做）。\
             适用：长会话/大工程/明显超出悬浮小窗能力的活。不要用来逃避简单任务。",
            json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "目标，目前仅 workbuddy" },
                    "title": { "type": "string", "description": "交接标题（短）；空则用会话标题" },
                    "summary": { "type": "string", "description": "任务摘要；空则由系统按有效消息压缩生成" },
                    "reason": { "type": "string", "description": "为何分发：本助手缺什么能力/上下文" },
                    "context": { "type": "string", "description": "补充上下文（路径、已尝试、约束），可空" }
                },
                "required": ["target", "reason"]
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
        ToolSpec::new(
            "request_access",
            "当文件工具因权限被拒时，用它向用户申请临时权限。**会阻塞等待用户当场拍板**\
             （最长 3 分钟，超时视为拒绝），返回后如果批准了就直接重试原来的工具。\
             只有在真的被拒之后才用；path 尽量填**最窄的够用范围**，填越宽用户越可能拒绝或收窄范围。",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "要申请的路径。必须是刚才被拒路径本身、或它的某个上层目录"
                    },
                    "access": {
                        "type": "string",
                        "enum": ["read", "readwrite", "full"],
                        "description": "需要的权限：read 只读 / readwrite 读写 / full 含删除"
                    },
                    "tier": {
                        "type": "string",
                        "enum": ["once", "turn", "task"],
                        "description": "有效期：once 只够这一次工具调用 / turn 本轮对话内有效 / task 整个任务期间有效。拿不准就选 once"
                    },
                    "reason": {
                        "type": "string",
                        "description": "一句话说明你要做什么、为什么需要这个权限。用户是靠它判断批不批的"
                    }
                },
                "required": ["path", "access", "tier", "reason"]
            }),
        ),
    ]
}

/// `web_search` —— 仅在搜索已配置时进入 tool_specs
pub fn web_search_spec() -> ToolSpec {
    ToolSpec::new(
        "web_search",
        "搜索公开网页（默认 Tavily，也可 Exa/Brave，取决于设置）。\
         用于模型知识截止之后的新闻/文档/版本/公开事实。\
         **不**用它查本地记忆、文件或用户约定（先用记忆与 recall_turns）。\
         返回标题 + URL + 短摘要；查询词会发送到搜索服务商。",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索关键词（建议具体、简短）" },
                "max_results": { "type": "integer", "description": "最多几条（1–8，默认 5）" }
            },
            "required": ["query"]
        }),
    )
}

/// 完整工具列表 = 内置 + （可选）web_search + **已加载的** MCP 工具
///
/// `web_search_ready` = 设置里已配置搜索后端且 key 非空。
/// **未配置时不要暴露该工具**，避免模型徒劳调用。
///
/// ⚠️ 这个函数每轮都会被调用（因为模型可能中途 load 了新组），
/// 所以它必须只做拼接、不产生副作用（早先用 Box::leak 会反复泄漏）。
pub fn tool_specs(mcp: Option<&crate::mcp::ToolRegistry>, web_search_ready: bool) -> Vec<ToolSpec> {
    let mut specs = builtin_specs();
    if web_search_ready {
        specs.push(web_search_spec());
    }

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
    /// 「申请权限」的授权表 + 待办通道。
    /// 打包成**一个引用**，避免以后每加一项能力就改一次 `agent::run` 的签名。
    pub perm: &'a PermHub,
}

// ---------------------------------------------------------------------------
// 授权入口
// ---------------------------------------------------------------------------

/// 文件操作的**唯一**授权入口：判定 + 就地消费 `Once` 档配额。
///
/// 为什么不让各工具直接调 `gate.check`：
///   1. 用户当场批的**临时授权**不在规则里，只有 `PermissionGate::authorize` 看得见；
///   2. `Once` 档要在放行那一刻就扣次数 —— 判定与扣减必须一起做完，
///      拆开迟早有人忘掉后一步，那"一次"就悄悄变成了"永久"。
///
/// 失败时还会**记下这次拒绝**（`PermHub::note_denial`），`request_access`
/// 靠它做"申请范围必须包含被拒路径"的校验，并给卡片提供"宽了几层"的对比。
fn authorize(ctx: &ToolCtx<'_>, path: impl AsRef<Path>, need: Access) -> Result<PathBuf, String> {
    let mut store = ctx
        .perm
        .grants()
        .lock()
        .map_err(|e| format!("授权表锁失败: {e}"))?;

    match ctx.gate.authorize(path, need, store.grants()) {
        Ok(allowed) => {
            // 命中临时授权 → 立刻扣次数（`Once` 用尽即从表里移除）
            if let Some(idx) = allowed.grant_idx {
                store.consume(idx);
            }
            Ok(allowed.path)
        }
        Err(e) => {
            let canon = match &e {
                DenyReason::NoMatchingRule { path }
                | DenyReason::ExplicitlyDenied { path, .. }
                | DenyReason::InsufficientAccess { path, .. } => Some(path.clone()),
                DenyReason::BadPath { .. } => None,
            };
            if let Some(p) = canon {
                ctx.perm.note_denial(ctx.session_id, &p);
            }
            Err(deny_message(&e, need))
        }
    }
}

/// **不消费**授权的判定 —— 只给遍历过程中的**逐文件过滤**用。
///
/// 为什么不能用 [`authorize`]：`glob`/`grep` 一次调用会检查几百个文件，
/// 若每个都扣次数，一条「一次」授权会在**第一个文件**上就被吃光，后面的全被拒。
/// 消费只发生在**工具入口**那一次判定上 —— 那才是"一次工具调用"的粒度。
fn check_passive(ctx: &ToolCtx<'_>, path: &Path, need: Access) -> Option<PathBuf> {
    let store = ctx.perm.grants().lock().ok()?;
    ctx.gate
        .authorize(path, need, store.grants())
        .ok()
        .map(|a| a.path)
}

/// 把拒绝原因翻成**给模型看**的文本：说明情况，并明确告诉它"这条路能申请"。
///
/// 这句提示必须真的出现在工具的返回里 —— 只在系统提示词里写"可以申请"是不够的：
/// 模型撞墙时看到的是这条错误，那里不提它就不会想到（同 `recall_turns` 的教训，
/// 工具加了但没在提示里说 = 摆设）。
fn deny_message(e: &DenyReason, need: Access) -> String {
    let base = e.to_string();

    // 路径本身解析不了 —— 这不是权限问题，申请也救不了，别误导模型去申请
    if matches!(e, DenyReason::BadPath { .. }) {
        return base;
    }

    format!(
        "{base}\n\n\
         → 如果你确实需要访问它，可以调用 `request_access` 向用户申请权限（需用户当场批准）：\n\
         - path：要访问的路径。**尽量填最窄的够用范围** —— 填得越宽用户越可能拒绝或收窄；\n\
         - access：\"{need}\"（就是你这次需要的权限）；\n\
         - tier：once（只够这一次工具调用）/ turn（本轮对话内有效）/ task（整个任务期间有效）；\n\
         - reason：一句话说明你要做什么、为什么需要，用户是靠它判断批不批的。\n\
         用户拒绝或超时后**不要原样重复申请同一个路径** —— 那只会打扰用户。\n\
         如果用户的拒绝带了理由，请**按理由调整**（换更窄的路径 / 换做法）后再考虑申请。",
        need = need.as_str()
    )
}

/// 执行一个工具调用。
pub async fn execute(
    name: &str,
    args: &Value,
    ctx: &ToolCtx<'_>,
) -> Result<ToolOutput, String> {
    match name {
        "read_file" => read_file(args, ctx),
        "list_dir" => list_dir(args, ctx),
        "glob_files" => glob_files(args, ctx),
        "grep_files" => grep_files(args, ctx),
        "write_file" => write_file(args, ctx),
        "edit_file" => edit_file(args, ctx),
        "delete_file" => delete_file(args, ctx),
        "run_command" => run_command(args, ctx).await,
        "web_search" => web_search(args, ctx).await,
        "request_access" => request_access(args, ctx).await,
        "capture_screen" => capture_screen(ctx.data_dir),
        "foreground_context" => {
            let out = match crate::context::cached().or_else(crate::context::current) {
                Some(c) => c.summary(),
                None => "无法获取前台窗口信息".into(),
            };
            Ok(ToolOutput::text(out))
        }
        "remember" => remember(args, ctx.data_dir),
        "dispatch_task" => dispatch_task(args, ctx),
        "recall_turns" => recall_turns(args, ctx),
        "load_skill" => {
            let name = get_str(args, "name")?;
            // 技能正文可能带命令示例，但那是「模型照着敲」的，不经过这里；
            // 加载本身只是读文件，且目录在 data_dir 内 —— 不过仍显式走网关，
            // 与其他文件工具保持同一纪律。
            if let Err(e) = authorize(ctx, crate::skills::skills_dir(ctx.data_dir), Access::Read) {
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

/// 分发：总结会话有效消息 + 附图 → 写交接 → 打开文件（用户第一轮手动交给 WB）。
fn dispatch_task(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let target = get_str(args, "target")?;
    let reason = get_str(args, "reason")?;
    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    let summary = args.get("summary").and_then(Value::as_str).unwrap_or("");
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let meta = crate::dispatch::write_handoff_from_session(
        ctx.data_dir,
        ctx.session_id,
        &target,
        if title.trim().is_empty() {
            None
        } else {
            Some(title)
        },
        if summary.trim().is_empty() {
            None
        } else {
            Some(summary)
        },
        &reason,
        &context,
        "model",
    )?;

    let opened = crate::dispatch::open_handoff_file(&meta.path).is_ok();
    let mut msg = format!(
        "已写好交接文件（{}，有效消息 {} 条，图片 {} 张）：\n{}\n\
         第一轮请用户**手动**把该文件内容/图片交给 WorkBuddy；\
         自动化投递属第二轮，本期不做。{}",
        meta.target,
        meta.message_count,
        meta.images.len(),
        meta.path,
        if opened {
            "（已尝试用系统默认程序打开该文件）"
        } else {
            "（打开失败，请手动打开上述路径）"
        }
    );

    // 可选：复制到用户配置的 WB 目录（须 Full 授权；失败不回滚主文件）
    let drop = crate::config::load_settings(ctx.data_dir)
        .dispatch_wb_drop_dir
        .filter(|s| !s.trim().is_empty());
    if let Some(drop_dir) = drop {
        let expanded = crate::dispatch::expand_windows_env(&drop_dir);
        let drop_path = PathBuf::from(&expanded);
        match authorize(ctx, &drop_path, Access::Full) {
            Ok(canon) => {
                if let Err(e) = std::fs::create_dir_all(&canon) {
                    msg.push_str(&format!("\n（drop 目录创建失败，已忽略：{e}）"));
                } else {
                    let dest = canon.join(std::path::Path::new(&meta.path).file_name().unwrap());
                    match std::fs::copy(&meta.path, &dest) {
                        Ok(_) => msg.push_str(&format!("\n已复制副本：{}", dest.display())),
                        Err(e) => msg.push_str(&format!("\n（副本复制失败，已忽略：{e}）")),
                    }
                }
            }
            Err(e) => {
                msg.push_str(&format!(
                    "\n（配置的 drop 目录无权限，仅保留 agent-data 内主文件：{e}）"
                ));
            }
        }
    }

    Ok(ToolOutput::text(msg))
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

fn read_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe_path = authorize(ctx, &raw, Access::Read)?;

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

fn list_dir(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe_path = authorize(ctx, &raw, Access::Read)?;

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

fn glob_files(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
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
    let _ = authorize(ctx, &base_path, Access::Read)
        .map_err(|e| format!("搜索起点无权限：{e}"))?;

    let mut hits: Vec<String> = Vec::new();
    let mut denied = 0usize;

    let paths = glob::glob(&pattern).map_err(|e| format!("glob 模式无效: {e}"))?;

    for p in paths.flatten() {
        if hits.len() >= MAX_GLOB_MATCHES {
            break;
        }
        // 逐个结果再过一次权限网关（防止模式匹配到越界路径）。
        // ⚠️ 这里必须用**不消费**的 check_passive —— 一次 glob 会过几百个文件，
        //    若每个都扣次数，一条「一次」授权会在第一个文件上就被吃完。
        match check_passive(ctx, &p, Access::Read) {
            Some(safe) => hits.push(safe.to_string_lossy().into_owned()),
            None => denied += 1,
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

fn grep_files(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let needle = get_str(args, "pattern")?;
    let raw = get_str(args, "path")?;
    let roots = authorize(ctx, &raw, Access::Read)?;

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

        // 权限：逐个文件再确认（防止 walk 过程中穿过 junction）。
        // ⚠️ 同 glob：用**不消费**的 check_passive，否则一次 grep 会把「一次」授权吃光。
        let Some(safe) = check_passive(ctx, p, Access::Read) else {
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
// write_file / edit_file / delete_file
// ---------------------------------------------------------------------------

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 写/改前备份到 `agent-data/backups/`。原文件不存在则不备份，返回空说明。
fn backup_file(data_dir: &Path, path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let backup_dir = data_dir.join("backups");
    std::fs::create_dir_all(&backup_dir).map_err(|e| format!("建备份目录失败: {e}"))?;

    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let backup = backup_dir.join(format!("{}__{stem}", now_ms()));
    std::fs::copy(path, &backup).map_err(|e| format!("备份失败: {e}"))?;
    Ok(format!("\n（原文件已备份到 {}）", backup.display()))
}

fn write_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let content = get_str(args, "content")?;
    let safe_path = authorize(ctx, &raw, Access::ReadWrite)?;

    let note = backup_file(ctx.data_dir, &safe_path)?;

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

/// 局部编辑：精确字符串替换。优先于整文件覆盖（决策：工具层要有 edit）。
fn edit_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let old_s = get_str(args, "old_string")?;
    let new_s = get_str(args, "new_string")?;
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if old_s.is_empty() {
        return Err(
            "old_string 不能为空 —— 空串语义不清。要写整个文件请用 write_file。".into(),
        );
    }
    if old_s == new_s {
        return Ok(ToolOutput::text("old_string 与 new_string 相同，未修改。"));
    }

    let safe_path = authorize(ctx, &raw, Access::ReadWrite)?;
    if safe_path.is_dir() {
        return Err(format!("{} 是目录，请编辑具体文件", safe_path.display()));
    }
    if !safe_path.exists() {
        return Err(format!(
            "{} 不存在。新建文件请用 write_file；编辑前请先 read_file 核对路径。",
            safe_path.display()
        ));
    }

    let bytes = std::fs::read(&safe_path).map_err(|e| format!("读取失败: {e}"))?;
    let text = String::from_utf8_lossy(&bytes).to_string();

    let count = text.matches(&old_s).count();
    if count == 0 {
        return Err(format!(
            "在 {} 中找不到 old_string。\n请先 read_file，用**完全一致**的片段（含缩进/空白/换行）再试。",
            safe_path.display()
        ));
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string 在 {} 中出现 {count} 次，无法确定改哪一处。\n\
             请加长 old_string（带上唯一上下文）使其只命中一次，\
             或设 replace_all=true 替换全部。",
            safe_path.display()
        ));
    }

    let note = backup_file(ctx.data_dir, &safe_path)?;
    let replaced = if replace_all { count } else { 1 };
    let out_text = if replace_all {
        text.replace(&old_s, &new_s)
    } else {
        text.replacen(&old_s, &new_s, 1)
    };
    let delta = out_text.len() as i64 - text.len() as i64;

    std::fs::write(&safe_path, out_text.as_bytes()).map_err(|e| format!("写入失败: {e}"))?;

    Ok(ToolOutput::text(format!(
        "已编辑 {}（替换 {replaced} 处，体积变化 {delta:+} 字节）{note}",
        safe_path.display()
    )))
}

fn collect_files_limited(root: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        if out.len() >= limit {
            return;
        }
        let p = e.path();
        if p.is_dir() {
            collect_files_limited(&p, out, limit);
        } else {
            out.push(p);
        }
    }
}

/// 删除 = **永远归档**（决策 8 / RULES 第二节）。不硬删，即使用户点名硬删。
fn delete_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    // 删除需要 full（readwrite 明确「不能删」）
    let safe_path = authorize(ctx, &raw, Access::Full)?;

    if !safe_path.exists() {
        return Err(format!("{} 不存在，无事可删。", safe_path.display()));
    }

    // 不许把数据目录本身或其上层「删掉」——那会毁掉记忆/会话/权限表
    if safe_path == ctx.data_dir || ctx.data_dir.starts_with(&safe_path) {
        return Err(format!(
            "拒绝归档 {} —— 它是 agent 数据目录或其上层，删掉会毁掉记忆/会话/配置。",
            safe_path.display()
        ));
    }

    let mut affected: Vec<PathBuf> = Vec::new();
    if safe_path.is_file() {
        affected.push(safe_path.clone());
    } else {
        collect_files_limited(&safe_path, &mut affected, 500);
    }

    let mut listing = String::new();
    for p in affected.iter().take(50) {
        listing.push_str("  - ");
        listing.push_str(&p.display().to_string());
        listing.push('\n');
    }
    if affected.len() > 50 {
        listing.push_str(&format!("  … 另有 {} 个文件未逐一列出\n", affected.len() - 50));
    }

    let trash_root = ctx.data_dir.join(".trash");
    std::fs::create_dir_all(&trash_root).map_err(|e| format!("建 .trash 失败: {e}"))?;

    let stem = safe_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "item".into());
    let stamp = now_ms();
    let mut dest = trash_root.join(format!("{stamp}__{stem}"));
    let mut n = 1u32;
    while dest.exists() {
        dest = trash_root.join(format!("{stamp}_{n}__{stem}"));
        n += 1;
    }

    // rename = move；失败则原文件仍在，不算删除成功
    std::fs::rename(&safe_path, &dest).map_err(|e| {
        format!(
            "归档失败: {e}\n目标：{}\n**原文件未被删除**。",
            dest.display()
        )
    })?;

    let is_dir = dest.is_dir();
    let rec = json!({
        "at": stamp as u64,
        "original": safe_path.display().to_string(),
        "trashed": dest.display().to_string(),
        "kind": if is_dir { "dir" } else { "file" },
        "count": affected.len(),
    });
    let manifest = trash_root.join("trash.jsonl");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest)
    {
        let _ = writeln!(f, "{rec}");
    }

    Ok(ToolOutput::text(format!(
        "已归档（**非硬删除**）→ {}\n\
         原路径：{}\n\
         受影响文件约 {} 个：\n{listing}\
         还原：把归档路径 move 回原路径即可（记录见 {}）。\n\
         ⚠️ agent 永远只归档到 .trash/，不会硬删除。若需物理删除，请用户自行执行。",
        dest.display(),
        safe_path.display(),
        affected.len(),
        manifest.display()
    )))
}

// ---------------------------------------------------------------------------
// request_access
// ---------------------------------------------------------------------------

/// 向用户申请文件权限 —— 「申请权限」在模型侧的唯一入口。
///
/// **会阻塞**：登记待办 → 前端弹卡 → 用户拍板 → 这里才返回
/// （超时 3 分钟 → fail-closed，按拒绝处理）。完整时序见 `perm_request` 模块头。
async fn request_access(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    // ---- 参数 ----
    let raw = get_str(args, "path")?;

    let access_raw = get_str(args, "access")?;
    let Some(access) = Access::parse(&access_raw) else {
        return Ok(ToolOutput::text(format!(
            "access 填错了：\"{access_raw}\" 不是合法值，只能是 read / readwrite / full。"
        )));
    };
    if access == Access::Deny {
        return Ok(ToolOutput::text(
            "access 不能是 deny —— 申请只能申请「放行」，不能申请「禁止」。".to_string(),
        ));
    }

    let tier_raw = args
        .get("tier")
        .and_then(Value::as_str)
        .unwrap_or("once")
        .to_string();
    let Some(tier) = GrantTier::parse(&tier_raw) else {
        return Ok(ToolOutput::text(format!(
            "tier 填错了：\"{tier_raw}\" 不是合法值，只能是 once / turn / task。"
        )));
    };

    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let canon = crate::permission::best_effort_canonicalize(Path::new(&raw));
    if canon.as_os_str().is_empty() {
        return Ok(ToolOutput::text(format!("路径无法解析：{raw}")));
    }

    // ---- 校验 1：申请的路径必须**包含**刚才被拒的路径 ----
    //
    // 防的是"申请一个跟被拒路径无关的东西"。只在**确实有前一次拒绝**时生效 ——
    // 模型还没撞过墙就提前申请（比如预判要写某个目录）不拦。
    let denial = ctx.perm.last_denial(ctx.session_id);
    if let Some(d) = &denial {
        if !crate::permission::path_has_prefix(&d.path, &canon) {
            return Ok(ToolOutput::text(format!(
                "申请的路径不成立：{} 并不包含刚才被拒的 {}。\n\
                 请把 path 改成 {} 本身、或它的某个上层目录，再申请一次。",
                canon.display(),
                d.path.display(),
                d.path.display()
            )));
        }
    }

    // ---- 校验 2：其实已经能访问了，就别打扰用户 ----
    {
        let store = ctx
            .perm
            .grants()
            .lock()
            .map_err(|e| format!("授权表锁失败: {e}"))?;
        if ctx.gate.authorize(&canon, access, store.grants()).is_ok() {
            return Ok(ToolOutput::text(format!(
                "{} 现在已在可访问范围内，不用申请 —— 直接重试原来那个工具即可。",
                canon.display()
            )));
        }
    }

    // ---- 校验 3：这条申请**刚被拒过** → 直接打回，不再弹卡打扰用户 ----
    //
    // 这就是「防骚扰」的落点：不是把路径禁言，而是**同一个请求不重复放卡**。
    // 模型凭理由换成更窄的路径时那是一条新记录，会正常弹卡 —— 那正是"按用户的意思调整"。
    // 判定按 (会话, 规范路径, 权限) 精确匹配，所以"窄一点再试"不会被误拦。
    if let Some(m) = ctx.perm.find_denied(ctx.session_id, &canon, access) {
        let why = if m.reason.is_empty() {
            "（用户上次没有说明理由）".to_string()
        } else {
            format!("用户上次给的理由：{}", m.reason)
        };
        return Ok(ToolOutput::text(format!(
            "{} 的这条申请（{}）刚刚已经被拒绝过了，**不会再弹第二次卡**，\n\
             请不要原样重复申请。\n{}\n\n\
             请按这个理由换个做法：换一条更窄的路径、或者改用别的方案；\
             实在做不了就如实告诉用户。",
            canon.display(),
            access.as_str(),
            why
        )));
    }

    // ---- 阻塞等用户拍板 ----
    let denied_path = denial.as_ref().map(|d| d.path.clone());
    let verdict = ctx
        .perm
        .ask(
            canon.clone(),
            access,
            tier,
            reason.clone(),
            denied_path.clone(),
            ctx.session_id,
        )
        .await?;

    let applied = ctx.perm.apply_decision(
        verdict.decision,
        &canon,
        denied_path.as_deref(),
        access,
        tier,
        &reason,
    );

    let Some(prefix) = applied else {
        // 拒绝 / 超时 —— 消息里已经明确告诉模型下一步该怎么办
        // （拒绝时会带上用户填的理由，模型据此调整）
        return Ok(ToolOutput::text(verdict.to_model_message()));
    };

    // 「整个任务」档要落盘（跟随会话）。主聊天会被 `persist_task_grants` 内部跳过 ——
    // 它是永久会话，落盘等于变相无限期授权。
    if tier == GrantTier::Task {
        if let Err(e) = ctx.perm.persist_task_grants(ctx.data_dir, ctx.session_id) {
            // 不回滚已生效的内存授权：用户已经批了，让这次操作继续，
            // 代价只是"重启后失效"。记日志即可。
            eprintln!("[float-agent] ⚠️ 授权落盘失败（本次仍有效，重启后失效）: {e}");
        }
    }

    Ok(ToolOutput::text(format!(
        "{msg}\n\n已授权范围：{prefix}（{access} / {tier} 档）。现在直接重试原来那个工具即可。",
        msg = verdict.to_model_message(),
        prefix = prefix.display(),
        access = access.as_str(),
        tier = tier.as_str(),
    )))
}

// ---------------------------------------------------------------------------
// run_command

/// `web_search` 执行体：读 search.json → 调 provider API
async fn web_search(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let q = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if q.is_empty() {
        return Err("query 不能为空".into());
    }
    let max = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as usize;
    let cfg = crate::search::load_config(ctx.data_dir)?;
    if !cfg.is_ready() {
        return Err(
            "网页搜索未配置：请在 设置 › 搜索 选择 Tavily（默认）/ Exa / Brave 并填入 API key"
                .into(),
        );
    }
    let text = crate::search::run(&cfg, &q, max).await?;
    Ok(ToolOutput::text(text))
}

// web_search 执行体在本文件末尾附近（async fn web_search）

/// 文件操作的**唯一**授权入口
// ---------------------------------------------------------------------------

/// 执行一条 PowerShell 命令。**双闸门**：
///
/// - **Gate 1（资源级）**：`cwd` 必须过文件权限网关（至少只读）——
///   命令的副作用会落在那个目录，先把"目录这一关"卡死。
/// - **Gate 2（动作级）**：命令本身走 [`crate::command_policy`] 三路判定
///   （硬阻断 / 白名单 / 确认卡）。
///
/// ⚠️ 命令的副作用**无法静态穷举**（`python -c` / 嵌套 shell 都是逃生口），
///    所以本工具只承诺 **default-deny + 逐条用户批准**，不承诺"批了就绝对安全"。
///    这与各家 agent 的官方共识一致（静态规则不是安全边界）。
async fn run_command(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let cmd = get_str(args, "command")?;
    if cmd.trim().is_empty() {
        return Err("command 不能为空".into());
    }

    // ---- Gate 1：cwd 过文件路径网关 ----
    let cwd = resolve_command_cwd(args, ctx)?;

    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 600);

    // ---- Gate 2：命令策略 ----
    let policy = command_policy::load_policy(ctx.data_dir);
    let norm = command_policy::normalize(&cmd);

    let source: String;
    let risk: command_policy::Risk;

    match command_policy::evaluate(&policy, &cmd) {
        CmdVerdict::HardBlock { reason } => {
            audit_cmd(
                ctx, &cmd, &norm, &cwd, "blocked", "hard-block", command_policy::Risk::High, None,
            );
            return Err(format!(
                "命令被**硬性阻断**（不可申请）：{reason}\n\
                 这条命令命中危险模式，永远不会执行 —— 不要重试，也不要换个写法绕过。\n\
                 如确有正当需要，请让用户**手工**执行。"
            ));
        }
        CmdVerdict::Allow { reason } => {
            risk = command_policy::Risk::Low;
            source = format!("白名单（{reason}）");
            audit_cmd(ctx, &cmd, &norm, &cwd, "allowed", "whitelist", risk, None);
        }
        CmdVerdict::Ask { reason, risk: r } => {
            risk = r;

            // ① 已有临时授权（Once/Turn/Task 三档）→ 消费一次直接放行
            let hit = {
                let mut store = ctx
                    .perm
                    .cmd_grants()
                    .lock()
                    .map_err(|e| format!("命令授权表锁失败: {e}"))?;
                match store.find(&cmd) {
                    Some(i) => {
                        store.consume(i);
                        true
                    }
                    None => false,
                }
            };

            if hit {
                source = "已授权（临时授权生效）".into();
                audit_cmd(ctx, &cmd, &norm, &cwd, "allowed", "grant", risk, None);
            } else {
                // ② 防骚扰：这条命令刚被拒过 → 不再弹卡，直接把理由回给模型
                let full_fp = norm.full();
                if let Some(m) = ctx.perm.find_denied_cmd(ctx.session_id, &full_fp) {
                    let why = if m.reason.is_empty() {
                        "（用户上次没有说明理由）".to_string()
                    } else {
                        format!("用户上次给的理由：{}", m.reason)
                    };
                    return Ok(ToolOutput::text(format!(
                        "这条命令刚刚已经被拒绝过了，**不会重复弹卡**。\n{why}\n\n\
                         请按这个理由调整：换一种做法，或如实告诉用户你做不了。"
                    )));
                }

                // ③ 弹卡等用户当场拍板
                let passed_head = norm.head();
                let passed_full = norm.full();
                let verdict = ctx
                    .perm
                    .ask_command(
                        cmd.clone(),
                        passed_full.clone(),
                        passed_head,
                        passed_full,
                        risk,
                        reason.clone(),
                        ctx.session_id,
                    )
                    .await?;

                if !verdict.is_approved() {
                    audit_cmd(ctx, &cmd, &norm, &cwd, "rejected", "user-denied", risk, None);
                    return Ok(ToolOutput::text(verdict.to_model_message()));
                }

                let applied = ctx.perm.apply_cmd_decision(verdict.decision, &cmd, &reason);
                let Some(g) = applied else {
                    return Ok(ToolOutput::text(
                        "用户已批准，但命令授权写入失败，请重试一次。",
                    ));
                };
                let scope_label = match g.scope {
                    CmdScope::Prefix => "记住这类命令",
                    CmdScope::Full => "记住完整命令",
                };
                source = format!("用户已批准（{scope_label}）");
                audit_cmd(ctx, &cmd, &norm, &cwd, "approved", "user-approved", risk, None);
            }
        }
    }

    // ---- 执行 ----
    let out = match crate::shell::run_powershell(&cmd, &cwd, timeout_secs).await {
        Ok(o) => o,
        Err(e) => {
            audit_cmd(ctx, &cmd, &norm, &cwd, "error", &source, risk, None);
            return Err(format!("执行失败：{e}"));
        }
    };

    let event = if out.timed_out { "timeout" } else { "executed" };
    audit_cmd(ctx, &cmd, &norm, &cwd, event, &source, risk, out.exit_code);

    Ok(ToolOutput::text(format_command_output(
        &cmd,
        &out,
        &cwd,
        timeout_secs,
    )))
}

/// 解析命令的工作目录。默认 agent 数据目录；指定时**必须过文件权限网关**（至少只读）。
fn resolve_command_cwd(args: &Value, ctx: &ToolCtx<'_>) -> Result<PathBuf, String> {
    let raw = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if raw.is_empty() {
        // 默认：agent 数据目录（自有、Full 权限，必定允许）
        return Ok(ctx.data_dir.to_path_buf());
    }
    authorize(ctx, raw, Access::Read)
}

/// 写一条命令审计（自动补哈希链）。失败只记日志，不影响命令执行。
#[allow(clippy::too_many_arguments)]
fn audit_cmd(
    ctx: &ToolCtx<'_>,
    cmd: &str,
    norm: &command_policy::Normalized,
    cwd: &Path,
    event: &str,
    source: &str,
    risk: command_policy::Risk,
    exit_code: Option<i32>,
) {
    let rec = command_policy::AuditRecord {
        ts: crate::permission::now_ms(),
        sequence: 0,
        prev_hash: String::new(),
        hash: String::new(),
        event: event.to_string(),
        source: source.to_string(),
        command: cmd.to_string(),
        normalized: norm.full(),
        cwd: cwd.to_string_lossy().into_owned(),
        session_id: ctx.session_id.to_string(),
        risk: risk.as_str().to_string(),
        exit_code,
    };
    if let Err(e) = command_policy::audit_append(ctx.data_dir, rec) {
        eprintln!("[float-agent] ⚠️ 写命令审计失败: {e}");
    }
}

/// 把执行结果整理成给模型看的文本。
fn format_command_output(
    cmd: &str,
    out: &crate::shell::CmdOutput,
    cwd: &Path,
    timeout_secs: u64,
) -> String {
    let mut s = format!(
        "$ {cmd}\n（工作目录 {}；解释器 {}）\n",
        cwd.display(),
        out.interpreter
    );

    if out.timed_out {
        s.push_str(&format!(
            "\n⚠️ **执行超时（>{timeout_secs}s），已强制终止（连同子进程）。**\n\
             如果是长任务，请拆小或调大 timeout_secs 后重试。\n"
        ));
        return s;
    }

    let has_out = !out.stdout.trim().is_empty();
    let has_err = !out.stderr.trim().is_empty();
    if has_out {
        s.push_str("\n--- stdout ---\n");
        s.push_str(&out.stdout);
        if !out.stdout.ends_with('\n') {
            s.push('\n');
        }
    }
    if has_err {
        s.push_str("\n--- stderr ---\n");
        s.push_str(&out.stderr);
        if !out.stderr.ends_with('\n') {
            s.push('\n');
        }
    }
    if !has_out && !has_err {
        s.push_str("\n（无输出）\n");
    }

    s.push_str(&format!(
        "\n退出码：{}\n",
        out.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "（未知 / 被信号终止）".into())
    ));

    // 特殊退出码：不是错误，别让模型误判
    if let Some(c) = out.exit_code {
        if c == 1 && cmd.contains("findstr") {
            s.push_str("提示：findstr 退出码 1 = 没找到匹配，属正常，不是错误。\n");
        }
        if (0..=7).contains(&c) && cmd.contains("robocopy") {
            s.push_str("提示：robocopy 退出码 0-7 都表示成功，≥8 才是失败。\n");
        }
    }

    s
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

    /// 每个 ctx 配一个**独立的** PermHub。
    ///
    /// 用 `Box::leak` 拿 `'static` 是为了满足 `ToolCtx` 的借用 —— 测试进程本就短命，
    /// 不值得为这点内存引入生命周期参数。**每个宏调用各给一个**（而不是共享一个 static），
    /// 否则并行跑的测试会通过授权表互相污染。
    macro_rules! leak_hub {
        () => {
            Box::leak(Box::new(PermHub::new()))
        };
    }

    /// 测试用的 ctx（不连 MCP、无会话）
    macro_rules! ctx_for {
        ($gate:expr, $dir:expr, $mcp:expr) => {
            ToolCtx {
                gate: &$gate,
                data_dir: $dir,
                mcp: &$mcp,
                session_id: "",
                perm: leak_hub!(),
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
                perm: leak_hub!(),
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
        let before = tool_specs(Some(&mcp.try_lock().unwrap()), false).len();
        assert_eq!(before, builtin_specs().len(), "初始不应带 MCP 工具");
        let with = tool_specs(Some(&mcp.try_lock().unwrap()), true).len();
        assert_eq!(with, builtin_specs().len() + 1, "配置搜索后应多 web_search");
        assert!(tool_specs(None, false).iter().all(|s| s.name != "web_search"));
        assert!(tool_specs(None, true).iter().any(|s| s.name == "web_search"));
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

    #[test]
    fn edit_and_delete_are_in_builtin_specs() {
        let names: Vec<String> = builtin_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"edit_file".to_string()), "缺 edit_file");
        assert!(names.contains(&"delete_file".to_string()), "缺 delete_file");
    }

    #[test]
    fn dispatch_task_is_in_builtin_specs() {
        let names: Vec<String> = builtin_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"dispatch_task".to_string()), "缺 dispatch_task");
    }

    #[tokio::test]
    async fn dispatch_task_writes_handoff_from_session() {
        let tmp = fresh_tmp("dispatch_tool");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        // 造一个会话：dispatch_task 会从当前会话总结有效消息
        let _s = crate::sessions::ensure_current(&tmp);
        crate::sessions::append_turn(
            &tmp,
            "帮我搞定复杂重构",
            &[],
            "我这边上下文不够，建议分发",
            &[],
        )
        .unwrap();
        let out = execute(
            "dispatch_task",
            &json!({
                "target": "workbuddy",
                "reason": "需要长会话与多文件对照"
            }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(out.text.contains("dispatch") || out.text.contains("交接"), "{}", out.text);
        assert!(out.text.contains("第一轮"), "应标明第一轮人工投递：{}", out.text);
        let items = crate::dispatch::list_handoffs(&tmp, Some("workbuddy"));
        assert_eq!(items.len(), 1);
        let body = std::fs::read_to_string(&items[0].path).unwrap();
        assert!(body.contains("帮我搞定复杂重构") || body.contains("有效消息"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn fresh_tmp(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("float_agent_{tag}_{stamp}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn edit_file_replaces_unique_snippet() {
        let tmp = fresh_tmp("edit_ok");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let target = tmp.join("a.txt");
        std::fs::write(&target, "hello world\nfoo bar\n").unwrap();

        let out = execute(
            "edit_file",
            &json!({
                "path": target.to_string_lossy(),
                "old_string": "foo bar",
                "new_string": "baz qux"
            }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(out.text.contains("已编辑"), "{}", out.text);
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "hello world\nbaz qux\n");
        // 备份应存在
        assert!(tmp.join("backups").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn edit_file_requires_unique_match() {
        let tmp = fresh_tmp("edit_multi");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let target = tmp.join("b.txt");
        std::fs::write(&target, "xx aa xx aa xx\n").unwrap();

        let err = execute(
            "edit_file",
            &json!({
                "path": target.to_string_lossy(),
                "old_string": "aa",
                "new_string": "bb"
            }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap_err();
        assert!(err.contains("出现"), "{}", err);
        // 原文未被改动
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "xx aa xx aa xx\n");

        let ok = execute(
            "edit_file",
            &json!({
                "path": target.to_string_lossy(),
                "old_string": "aa",
                "new_string": "bb",
                "replace_all": true
            }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(ok.text.contains("替换 2 处"), "{}", ok.text);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "xx bb xx bb xx\n");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn delete_file_archives_instead_of_hard_delete() {
        let tmp = fresh_tmp("del_file");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let target = tmp.join("gone.txt");
        std::fs::write(&target, "secret").unwrap();

        let out = execute(
            "delete_file",
            &json!({ "path": target.to_string_lossy() }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(out.text.contains("已归档"), "{}", out.text);
        assert!(out.text.contains("非硬删除") || out.text.contains("不会硬删除"), "{}", out.text);
        assert!(!target.exists(), "原路径应已不在");
        let trash = tmp.join(".trash");
        assert!(trash.is_dir());
        let mut found = false;
        for e in std::fs::read_dir(&trash).unwrap().flatten() {
            let p = e.path();
            if p.is_file() && p.file_name().unwrap().to_string_lossy().contains("gone.txt") {
                assert_eq!(std::fs::read_to_string(&p).unwrap(), "secret");
                found = true;
            }
        }
        assert!(found, ".trash 下应能找到归档的 gone.txt");
        assert!(trash.join("trash.jsonl").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn delete_file_dir_lists_and_archives() {
        let tmp = fresh_tmp("del_dir");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let dir = tmp.join("pkg");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/one.txt"), "1").unwrap();
        std::fs::write(dir.join("two.txt"), "2").unwrap();

        let out = execute(
            "delete_file",
            &json!({ "path": dir.to_string_lossy() }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(out.text.contains("已归档"), "{}", out.text);
        assert!(out.text.contains("one.txt"), "{}", out.text);
        assert!(!dir.exists());
        let trash = tmp.join(".trash");
        let mut ok = false;
        for e in std::fs::read_dir(&trash).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() && p.join("sub/one.txt").is_file() {
                ok = true;
            }
        }
        assert!(ok, "目录应整体进入 .trash");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn delete_file_denied_without_full_access() {
        let tmp = fresh_tmp("del_perm");
        let mut gate = PermissionGate::new();
        gate.add_rule(&tmp, Access::ReadWrite, "rw-only");
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let target = tmp.join("keep.txt");
        std::fs::write(&target, "x").unwrap();

        let err = execute(
            "delete_file",
            &json!({ "path": target.to_string_lossy() }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap_err();
        assert!(err.contains("request_access") || err.contains("full") || err.contains("权限"), "{}", err);
        assert!(target.exists(), "被拒时不得动文件");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
