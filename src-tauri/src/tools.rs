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

/// 内置工具的**全部**名字（含 `web_search` / `fetch_url`）。
///
/// 为什么要有这份清单（而不是每次去 `builtin_specs()` 里翻）：
/// `project.rs` 校验 L1 脚本工具名时**必须**能问出"这个名字是不是内置的" ——
/// 撞名的后果很具体：`execute` 的 `match` 先命中内置分支，
/// 用户的脚本工具**永远不会被执行**，而模型看到的是"我调了但没反应"。
///
/// ⚠️ 新增内置工具时**必须**同步这里。有一条测试守着
/// （`builtin_name_list_covers_every_spec`）会在漏掉时报错。
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_dir",
    "glob_files",
    "grep_files",
    "write_file",
    "edit_file",
    "delete_file",
    "move_file",
    "copy_file",
    "mkdir",
    "file_info",
    "append_file",
    "git",
    "run_command",
    "web_search",
    "fetch_url",
    "request_access",
    "capture_screen",
    "view_image",
    "send_image",
    "send_meme",
    "foreground_context",
    "current_time",
    "remember",
    "distill_move",
    "life_items",
    "ask_user",
    "recall_turns",
    "load_skill",
    "save_skill",
    "search_tools",
    "list_tool_groups",
    "load_tool_group",
    "unload_tool_group",
];

/// 这个名字是不是内置工具（L1 脚本工具不得占用）。
pub fn is_builtin_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name)
}

/// 内置工具（常驻，每轮都在 tools 里）。
/// **不含** `web_search` —— 它只在配置了搜索后端时由 [`tool_specs`] 注入。
pub fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "read_file",
            "读取一个文本文件的内容。返回带行号的文本。仅在确定路径存在时使用；\
             若不确定路径，先用 glob_files 或 list_dir 查找。受文件权限网关管辖。\
             **不要**用来读图片 —— 图片请用 view_image。",
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
            "列出目录下的条目（文件与子目录），含大小与是否为目录。\
             **优先于** run_command 跑 Get-ChildItem。可选 recursive 递归。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目录的绝对路径" },
                    "recursive": { "type": "boolean", "description": "是否递归列出（默认 false）" },
                    "depth": { "type": "integer", "description": "递归最大深度（默认 2，最大 4）" }
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
            "在指定路径下递归搜索子串文本，返回 文件:行号:内容。\
             `path` **可以是目录，也可以直接是一个文件**（搜单个文件时直接传它，别传父目录）。\
             **优先于** run_command 跑 Select-String/findstr。受文件权限网关管辖。\
             ⚠️ 只做**字面量子串**匹配：不支持 `|`、`.*`、`\\d` 这类正则语法（传了会直接报错提示）。\
             多个词请分开搜几次。默认**大小写不敏感**。\
             若有文件因体积/编码/权限被跳过，返回里会**明确列出**——看到「无匹配」时先看有没有跳过项。",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "要搜索的文本（**字面量子串**，不是正则；默认大小写不敏感）" },
                    "path": { "type": "string", "description": "搜索根目录（绝对路径），或要搜索的单个文件" },
                    "ext": { "type": "string", "description": "只搜这些扩展名，如 'rs,md'（可选）" },
                    "regex": { "type": "boolean", "description": "未启用；传 true 会报错。只用子串 pattern" },
                    "caseSensitive": { "type": "boolean", "description": "是否区分大小写（默认 false，不区分）" },
                    "context": { "type": "integer", "description": "命中行前后各显示几行（默认 0，最大 3）。注意「命中 N 行」只数真正的命中行" }
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
            "move_file",
            "移动或重命名文件/目录（相当于 Rename/Move）。**优先于** `run_command` 跑 Move-Item/Rename-Item。\
             需对**源路径与目标父目录**均有 readwrite。目标已存在时默认拒绝，除非 overwrite=true（会先备份目标文件）。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "src": { "type": "string", "description": "源路径（绝对）" },
                    "dst": { "type": "string", "description": "目标路径（绝对）" },
                    "overwrite": { "type": "boolean", "description": "目标已存在时是否覆盖（默认 false）" }
                },
                "required": ["src", "dst"]
            }),
        ),
        ToolSpec::new(
            "copy_file",
            "复制文件（目录请自行 glob/list 后多次 copy）。**优先于** `run_command` 跑 Copy-Item。\
             源需 read、目标需 readwrite。目标已存在时默认拒绝，除非 overwrite=true。受文件权限网关管辖。",
            json!({
                "type": "object",
                "properties": {
                    "src": { "type": "string", "description": "源文件路径（绝对）" },
                    "dst": { "type": "string", "description": "目标路径（绝对）" },
                    "overwrite": { "type": "boolean", "description": "目标已存在时是否覆盖（默认 false）" }
                },
                "required": ["src", "dst"]
            }),
        ),
        ToolSpec::new(
            "mkdir",
            "创建目录（含缺失的父目录）。**优先于** `run_command` 跑 New-Item。受文件权限网关管辖（需 readwrite）。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "要创建的目录绝对路径" }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::new(
            "file_info",
            "查看路径元信息：是否存在、是否目录、大小、修改时间。**优先于** PowerShell Get-Item。受文件权限网关管辖（需 read）。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "绝对路径" }
                },
                "required": ["path"]
            }),
        ),
        ToolSpec::new(
            "append_file",
            "向文本文件末尾追加内容（文件不存在则创建）。**优先于** Add-Content / `>>`。\
             写入前若文件已存在会自动备份。受文件权限网关管辖（需 readwrite）。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件绝对路径" },
                    "content": { "type": "string", "description": "要追加的文本（不会自动加换行，请自带）" }
                },
                "required": ["path", "content"]
            }),
        ),
        ToolSpec::new(
            "git",
            "Git **只读**查询：status / diff / log / show / branch（-v 含 sha）。\
             **优先于** `run_command` 跑 git —— 只读子命令不进命令确认卡。\
             写操作（add/commit/push/checkout/reset 等）**不可用**，请用 `run_command` 并接受命令策略。\
             cwd 默认为 path 所在仓库根，或显式传 repo。",
            json!({
                "type": "object",
                "properties": {
                    "subcommand": {
                        "type": "string",
                        "enum": ["status", "diff", "log", "show", "branch"],
                        "description": "只读子命令"
                    },
                    "repo": { "type": "string", "description": "仓库工作目录绝对路径（默认 agent 数据目录）" },
                    "args": { "type": "string", "description": "附加参数串，如 '-n 20' 或 'HEAD~3..HEAD'（可选）" }
                },
                "required": ["subcommand"]
            }),
        ),
        ToolSpec::new(
            "run_command",
            "在用户机器上执行一条 **PowerShell** 命令并返回输出（stdout/stderr/退出码）。\
             **硬规则：读/搜/列文件禁止用本工具**（禁止 Get-Content / Select-String / findstr / Get-ChildItem 查文件）—— \
             必须用 read_file / grep_files / glob_files / list_dir。\
             复制/移动/建目录用 copy_file / move_file / mkdir；git 只读用 `git` 工具。\
             仅当需要跑编译器、包管理器、自定义程序或无对应内置工具时才用本工具。\
             **受命令权限策略管辖**：只读命令（ls / rg 等）直接放行；\
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
                        "description": "超时秒数（可选，默认 600，范围 1-3600）。编译 / 构建 / 测试 / 装包这类长任务**直接跑即可**，不要为了确认而空转；只有预计超过 10 分钟（例如整库 release 构建、大型依赖安装）才需要显式调大。到点会连同子进程一起强杀；用户随时可点「停止」提前掐断。"
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
            "view_image",
            "查看一张**图片文件**的内容（png/jpg/webp/gif/bmp）。\
             当用户给你图片路径、你用 glob/list_dir 找到截图或图片、\
             或需要「看看这张图里是什么」时调用。\
             **不要**用 read_file 读图片（那是文本工具，只会得到乱码）。\
             画面会在下一条消息里附上，多模态模型下一轮就能看到；\
             纯文本模型看不到画面，会明确告诉你。PDF 等文档请用 skills 处理。",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "图片文件的绝对路径" }
                },
                "required": ["path"]
            }),
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
            "current_time",
            "获取**精确的当前时间**（到秒，含星期与 UTC 偏移）。\
             适用：要给文件/日记/文档写日期、要算「几小时前 / 上周 / 还有几天」、\
             要判断是否跨零点。system prompt 里已有一行「当前时间」（到分钟），\
             需要秒级精度或想确认它没过期时用本工具。\
             **不要**为了看时间跑 `run_command` 执行 Get-Date —— 那是绕路。",
            json!({ "type": "object", "properties": {}, "required": [] }),
        ),
        ToolSpec::new(
            "ask_user",
            "向用户**提一个问题并等待回答**（弹出问题卡，用户打字/点选项后你才能继续）。\
             适用：需要用户做选择、提供你无法从环境推断的信息（偏好、口令、确认方向）时。\
             不适用：能用工具查到的信息、是非确认（那是权限卡的事）—— 先查再问。\
             question 写清问题本身；options 给 2~5 个快捷选项（可空 = 自由输入）；\
             hint 写「为什么要问」（用户看到的背景）。用户跳过/不答时按返回内容自行决定下一步。",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "要问的问题，一句话，具体" },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "快捷选项（可选）：用户点一下即回答；不给则自由输入"
                    },
                    "hint": { "type": "string", "description": "为什么要问这个（给用户看的背景说明，可选）" }
                },
                "required": ["question"]
            }),
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
            "life_items",
            "读取本机**数据源**的明细（生活/学习信号：学习通作业、教务、邮件、\
             或用户自己接的任何东西）。system prompt 里每个源只有一行摘要\
             （条数 + 新鲜度），**细节要调这个工具才拿得到**。\
             适用：用户问「我有什么作业」「还有什么没交」「最近的安排」\
             「帮我规划一下」时，先看 prompt 里的数据源摘要，再来这里取明细。\
             不传 source = 列出有哪些源；传 source = 取那个源的明细。\
             只读：不登录平台、不修改任何数据。",
            json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "数据源 id（如 xuexitong）。留空则列出所有已启用的源"
                    }
                },
                "required": []
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
            "search_tools",
            "按**意图**检索外部工具（MCP）：给一个关键词，返回命中的工具名 + 一句话用途，\
             **不含参数 schema**，所以很便宜，可以放心多调几次。\
             当你要做一件事、但不确定有没有现成的外部工具时，**先搜这里**\
             （例：query=\"提交 issue\" 会命中 github 组的 create_issue）。\
             搜到后对**未加载**的组调 `load_tool_group` 拿参数细节。\
             留空 query = 返回全部外部工具索引（按组列出名字与用途）。",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "意图关键词，如「提交 issue」「截图」「数据库查询」「上传文件」。留空则列出完整索引"
                    }
                },
                "required": []
            }),
        ),
        ToolSpec::new(
            "list_tool_groups",
            "列出可用的 MCP 外部工具组（浏览器自动化、远程 SSH、数据库、桌面操作等）。\
             MCP 工具**不会默认加载**（为了省上下文），你需要先看这里有哪些组，\
             再用 load_tool_group 把需要的组装载进来。\
             当任务涉及网页、远程服务器、数据库、桌面应用操作时，先调用这个。\
             ⚠️ 只知道「要做什么」、不知道工具叫什么时，用 `search_tools` 按意图搜更准。",
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
            "查找**更早**的对话原文（不在你当前上下文里的部分）。\
             当你看到用户说「上次 / 之前 / 刚才那个 / 我们之前定的」\
             而你找不到依据时，**先调这个再回答**，不要猜也不要反问用户。\
             留空 query 可用来「翻一下最近聊过什么」。\n\
             默认**只查当前会话**（scope=session）。当用户说的是「以前/上次我们搞过\
             某个同类问题」而当前会话里查不到时，用 scope=all 跨会话再查一次\
             —— 它的结果会带「标题 · 时间」的来源标识，那些内容**不属于当前会话**，\
             引用时要说明是从哪个会话找到的，不要当成当前上下文。",
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
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["session", "all"],
                        "description": "检索范围：session=只查当前会话（默认）；\
                                        all=连同其他历史会话一起查（结果带来源标识）。\
                                        只在当前会话里查不到、且用户指的是别处聊过的事时才用 all"
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
                        "description": "有效期：once 只够这一次工具调用 / turn 本轮对话内有效 / task 整个任务期间有效（推荐 task，减少连环弹卡）"
                    },
                    "reason": {
                        "type": "string",
                        "description": "一句话说明你要做什么、为什么需要这个权限。用户是靠它判断批不批的"
                    }
                },
                "required": ["path", "access", "tier", "reason"]
            }),
        ),
        // 发图：**常驻**内置工具（所有模式都有）—— 工作模式也要能贴截图 / 图表。
        // 只有"发表情包"那一半（`send_meme`）按模式闸住，见 `all_tool_specs`。
        send_image_spec(),
    ]
}

/// `fetch_url` —— 拉网页正文（Markdown 子集）。始终可用（不依赖搜索 key）。
pub fn fetch_url_spec() -> ToolSpec {
    ToolSpec::new(
        "fetch_url",
        "抓取一个 http/https 网页并抽取可读正文（Markdown 子集：标题/段落/链接/列表/代码）。\
         用于读文档、博客、公告等**静态**页面。\
         **不**用于：需要点击/登录/JS 渲染的页面（改用 playwright MCP 组）；\
         也不是搜索 —— 不知道 URL 时先 `web_search`。\
         URL 会直接请求目标站点。结果会截断到约 1 万字。",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要抓取的 http/https 地址" },
                "max_chars": { "type": "integer", "description": "正文最多几字（200–50000，默认 10000）" }
            },
            "required": ["url"]
        }),
    )
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
/// + **当前项目的 L1 脚本工具**
///
/// `web_search_ready` = 设置里已配置搜索后端且 key 非空。
/// **未配置时不要暴露该工具**，避免模型徒劳调用。
///
/// ⚠️ 这个函数每轮都会被调用（因为模型可能中途 load 了新组），
/// 所以它必须只做拼接、不产生副作用（早先用 Box::leak 会反复泄漏）。
/// 项目工具也是每轮现算 —— 用户改完 `project.json` 下一轮就生效，不用重启
/// （与 skills 的热加载同一口径）。
///
/// ## PTC 模式的「坍缩」
///
/// 当前模式是 PTC 时，这里**只返回 `run_code`**：其余工具不作为独立 schema
/// 给模型，而是以**程序内 SDK 绑定**的形式出现在 system prompt 里
/// （见 [`ptc_sdk_tools`]）。这是"通告面与可调用面保持一致" ——
/// 公告了什么就能调什么，不会出现"prompt 里列了却调不动"。
pub fn tool_specs(
    mcp: Option<&crate::mcp::ToolRegistry>,
    web_search_ready: bool,
    data_dir: &Path,
    session_id: Option<&str>,
) -> Vec<ToolSpec> {
    if crate::ptc::mode_active(data_dir, session_id) {
        return vec![run_code_spec()];
    }
    all_tool_specs(mcp, web_search_ready, data_dir, session_id)
}

/// 「记忆蒸馏」会话专用工具集：标准工具 + `distill_move`。
///
/// 为什么要单独一份（2026-10-03）：`distill_move` 只在蒸馏会话里有意义
/// （把条目从中转站迁进 USER/SOUL/IDENTITY）。普通聊天里摆着它只会让模型
/// 误用 —— 所以按会话注入，而不是全局注册。
pub fn tool_specs_for_distill(
    mcp: Option<&crate::mcp::ToolRegistry>,
    web_search_ready: bool,
    data_dir: &Path,
    session_id: Option<&str>,
) -> Vec<ToolSpec> {
    let mut specs = tool_specs(mcp, web_search_ready, data_dir, session_id);
    specs.push(distill_move_spec());
    specs
}

/// **未坍缩**的完整工具列表（PTC 的 SDK 声明就是从这份生成的）。
///
/// 为什么单独留一个函数：PTC 下"模型能调什么"与"程序里能调什么"是**同一份**
/// 工具集，只是呈现形态不同。若 SDK 用另一份来源生成，两边迟早对不上
/// （典型症状：SDK 里声明了某工具，实际调用却报"没有这个工具"）。
pub fn all_tool_specs(
    mcp: Option<&crate::mcp::ToolRegistry>,
    web_search_ready: bool,
    data_dir: &Path,
    session_id: Option<&str>,
) -> Vec<ToolSpec> {
    let mut specs = builtin_specs();

    // ── 发图：只有"发表情包"那一半按模式闸住（2026-10-08）──────────────
    //
    // `send_image` 是常驻内置工具，已经在 `builtin_specs()` 里了（工作模式
    // 也要贴截图 / 图表）。这里单独注入 `send_meme` —— **只在能随便发图的
    // 模式**下出现，这就是"工作时不发表情包"的**硬闸门**：不是提示词劝它
    // 别发，而是工具表里根本没有这个东西，模型想调也调不到。
    //
    // 为什么模式判断放在函数内部、不改签名：与 `ptc::mode_active(data_dir)`
    // 同一个手法 —— `data_dir` 已经在参数里了，为传一个 mode 把 6 个调用点
    // 全改一遍不划算。
    if memes_allowed(data_dir, session_id) {
        specs.push(send_meme_spec());
    }

    specs.push(fetch_url_spec());
    if web_search_ready {
        specs.push(web_search_spec());
    }

    // ── L1：当前项目的脚本工具 ──────────────────────────────────────────
    //
    // 位置在 MCP 之前、内置之后：内置工具名是保留的（`is_builtin_tool_name`
    // 在装配阶段就拒绝了撞名的项目工具），所以这里不会有名字冲突。
    specs.extend(project_tool_specs(data_dir));

    let Some(reg) = mcp else { return specs };

    for g in reg.groups().iter().filter(|g| reg.is_active(&g.name)) {
        for t in &g.tools {
            // 组名 = server id，工具名原样透传（网关内部怎么命名是网关自己的事）
            let full = crate::mcp::mcp_tool_full_name(&g.server_id, &t.name);

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
    }

    specs
}

/// PTC 的 SDK 素材：(名字, 描述, 参数 schema)。
///
/// 直接复用 [`all_tool_specs`] —— 与 native 模式下的工具列表**逐项同源**，
/// 这正是"公告面 = 可调用面"的实现方式。
pub fn ptc_sdk_tools(
    mcp: Option<&crate::mcp::ToolRegistry>,
    web_search_ready: bool,
    data_dir: &Path,
    session_id: Option<&str>,
) -> Vec<(String, String, Value)> {
    all_tool_specs(mcp, web_search_ready, data_dir, session_id)
        .into_iter()
        .map(|s| (s.name, s.description, s.parameters))
        .collect()
}

/// `run_code` 的工具规格（PTC 模式下模型唯一能直接调的工具）。
fn run_code_spec() -> ToolSpec {
    ToolSpec::new(
        crate::ptc::RUN_CODE,
        "执行一段 TypeScript 程序，在程序里调用工具。**这是本模式下唯一能直接调用的工具** —— \
         其他工具（文件、命令、MCP 等）都以 SDK 声明的形式提供，在程序里用 \
         `await tools.<名字>(args)` 调用。\n\n\
         `code` 是一个 **async 函数的函数体**：可以直接 `await`、直接 `return`。\n\n\
         为什么用它：批量任务（读几十个文件、多源汇总、逐条筛选）在程序里用 \
         for / if / Promise.all / try-catch 组织，**中间结果不进入对话**，\
         只有你 `return` 或 `console.log` 的内容会回到上下文。所以先在程序里\
         筛选/汇总，只把结论交回来。\n\n\
         互不依赖的**只读**调用可以 `await Promise.all([...])` 真正并发；\
         有依赖的必须 `await` 串行。个别调用失败会 reject（`ToolCallError`，\
         带 `toolName` / `message`），需要「个别失败不影响整体」就 `try/catch` 它。\n\n\
         具体的工具声明见 system prompt 的「PTC 程序内可用的工具」段。",
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "TypeScript 程序的**函数体**（不是完整文件）。可以顶层 await / return。"
                },
                "description": {
                    "type": "string",
                    "description": "一句话说明这个程序做什么（给用户看的进度标题）"
                }
            },
            "required": ["code", "description"]
        }),
    )
}

/// 当前项目的 L1 脚本工具 → `ToolSpec` 列表。
///
/// 无项目 / 项目无 `tools` → 空表。
///
/// 描述里会**点明这是本机脚本**并附上脚本相对路径 —— 模型据此知道
/// 它跑在用户机器上、且能看到参数模板，减少"瞎猜参数"的调用。
fn project_tool_specs(data_dir: &Path) -> Vec<ToolSpec> {
    let Some(a) = crate::project::active(data_dir) else {
        return Vec::new();
    };
    a.bundle
        .tools
        .iter()
        .map(|t| {
            let mut desc = t.description.trim().to_string();
            if !t.perm.trim().is_empty() {
                desc.push_str(&format!("（权限声明：{}）", t.perm.trim()));
            }
            // 模板对模型是**有用信息**（知道参数怎么传），且不含密钥
            desc.push_str(&format!(
                " [项目脚本，argv 模板：{}]",
                t.command_template.join(" ")
            ));
            ToolSpec {
                name: t.name.clone(),
                description: desc,
                parameters: crate::project::tool_params(t),
            }
        })
        .collect()
}

/// 执行一个**项目 L1 脚本工具**。
///
/// ## 安全路径（与 `run_command` **同一套闸门**，不另开通道）
/// 1. `commandTemplate` 经 [`crate::project::expand_template`] 展开成 argv ——
///    **逐参替换，绝不再拆分**（见该函数的契约）。
/// 2. `cwd` 过**文件权限网关**（Gate 1，与 `run_command` 同）。
/// 3. 命令文本过**命令策略**（Gate 2）—— 拼装项目的 `commandAllowExtra`
///    只做白名单加法，硬阻断永不放宽。
/// 4. 走 `shell::run_powershell_tick` 起进程。
///
/// ⚠️ 为什么把 argv 拼成一条命令串再过策略：`command_policy` 的判定对象是
/// **命令行文本**（它做归一化、拆 `;|&&`、识别 `iex` 等）。argv 直接执行会
/// **绕过这套静态分析**。所以这里把 argv 拼成字符串送进策略判定，
/// 判定通过后再**用原始 argv 执行**（不经 shell 解析）——
/// 判定看到的就是要跑的东西，执行时又不会引入 shell 语义。
async fn run_project_tool(
    t: &crate::project::ScriptTool,
    args: &Value,
    ctx: &ToolCtx<'_>,
) -> Result<ToolOutput, String> {
    let a = crate::project::active(ctx.data_dir)
        .ok_or_else(|| "当前没有激活的项目".to_string())?;
    let dir = a.dir(ctx.data_dir);

    let argv = crate::project::expand_template(&t.command_template, &dir, args);
    if argv.is_empty() {
        return Err(format!("项目工具「{}」的 argv 模板展开为空", t.name));
    }

    // cwd = 项目目录（脚本的相对路径基准）
    let cwd = dir.clone();

    // ---- Gate 1：cwd 过文件权限网关 ----
    authorize(ctx, &cwd, Access::Read)?;

    // ---- Gate 2：拼成命令行文本送策略判定 ----
    //
    // ⚠️ 这一步是**为了过策略**，不是为了执行。执行走 argv（见下）。
    //    拼串时对含空白/引号的参数加引号，让归一化后的文本能反映真实参数边界。
    let cmd_text = argv
        .iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '\t', '"', '\'']) {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    // ---- Gate 2：命令策略（与 run_command **同一套判定**）----
    //
    // ⚠️ 这里传的是**拼出来的命令行文本**，不是 argv —— 因为
    //    `command_policy` 的判定对象就是命令行文本（它要做归一化、拆 `;|&&`、
    //    识别 `iex` / `$()` 等动态解析）。argv 直接执行会**绕过这套静态分析**。
    //    判定通过后**执行仍走原始 argv**（见下），所以判定看到的东西
    //    与实际跑的东西一致，而执行时又不引入任何 shell 语义。
    match gate_command(&cmd_text, &cwd, ctx).await? {
        CmdGate::Allowed { .. } => {}
        CmdGate::Refused(out) => return Ok(out),
    }

    // ---- 执行：走 argv（不经 shell）----
    //
    // ⚠️ 这里**不**把 argv 拼成字符串交给 PowerShell —— 那会把上面精心
    //    保持的"参数边界"重新交还给 shell 解析，等于前功尽弃。
    //    而是让 shell.rs 直接以 argv 起进程。
    let out = crate::shell::run_argv(
        &argv,
        &cwd,
        600,
        ctx.tick.clone(),
        ctx.cancel.clone(),
    )
    .await?;

    let mut text = String::new();
    if !out.stdout.trim().is_empty() {
        text.push_str(out.stdout.trim_end());
    }
    if !out.stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("[stderr]\n");
        text.push_str(out.stderr.trim_end());
    }
    if out.timed_out {
        text.push_str("\n\n⏱ 执行超时（600s）已终止。");
    }
    if out.cancelled {
        text.push_str("\n\n⏹ 被用户停止。");
    }
    if text.trim().is_empty() {
        text = format!("（无输出，退出码 {:?}）", out.exit_code);
    } else if out.exit_code != Some(0) {
        text.push_str(&format!("\n\n退出码 {:?}", out.exit_code));
    }
    Ok(ToolOutput::text(text))
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
    /// 当前会话 id。`recall_turns` **默认**只在本会话范围内检索；
    /// `scope=all` 时才跨会话扫 `sessions/*.json`（见 [`recall_turns`] 的注释：
    /// 默认保守是因为跨会话要遍历整个会话目录、且会引入别的会话的上下文）。
    pub session_id: &'a str,
    /// 「申请权限」的授权表 + 待办通道。
    /// 打包成**一个引用**，避免以后每加一项能力就改一次 `agent::run` 的签名。
    pub perm: &'a PermHub,
    /// 长工具执行期的心跳回调（`run_command` 用它每 2 秒推"已运行 N 秒 + 输出尾巴"）。
    /// `None` = 不推心跳（测试与短工具）。见 `shell::TickFn`。
    pub tick: Option<crate::shell::TickFn>,
    /// 长工具执行期的**取消探针**（用户点「停止」→ `run_command` 立刻杀进程树）。
    /// `None` = 不响应取消（测试与短工具）。见 `shell::CancelFn`。
    pub cancel: Option<crate::shell::CancelFn>,
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
         - tier：**推荐 task**（整个任务期间有效）；turn=本轮；once=仅这一次。\
             **不要对同一父目录下多个文件用 once 连环申请** —— 请申请公共父目录 + task。\n\
         - reason：一句话说明你要做什么、为什么需要，用户是靠它判断批不批的。\n\
         用户拒绝或超时后**不要原样重复申请同一个路径** —— 那只会打扰用户。\n\
         如果用户的拒绝带了理由，请**按理由调整**（换更窄的路径 / 换做法）后再考虑申请。",
        need = need.as_str()
    )
}

/// PTC 子调用的桥：把程序里的一次 `tools.x(args)` 接到**同一个** [`execute`]。
///
/// ## 为什么必须复用 `execute` 而不是另开一条快路
///
/// 这是整个 PTC 安全模型的关键：程序里的调用与模型直接发的调用**走同一条管线**，
/// 于是文件权限网关、命令策略、权限卡、审计链、输出截断全部照旧生效。
/// 若为了"快"另写一条只做分发的路径，就等于给模型开了一个绕过权限的后门 ——
/// 而它看起来只是一个"性能优化"。
///
/// ## 递归
///
/// 程序里调用 `run_code` 本身是**允许**的（会被 [`execute`] 正常分派），
/// 但那是嵌套的第二次 Node 进程 —— 代价高且几乎不是模型的本意。
/// 这里显式拒绝，并给出清晰的理由，而不是让它悄悄起一堆 Node。
struct ToolCtxSub<'a, 'b> {
    ctx: &'a ToolCtx<'b>,
}

impl crate::ptc::SubCall for ToolCtxSub<'_, '_> {
    async fn call(&self, name: &str, args: &Value) -> Result<Value, String> {
        if name == crate::ptc::RUN_CODE {
            return Err(
                "程序里不能再调用 run_code（那是嵌套的第二层程序）。\
                 请把逻辑写在同一段程序里。"
                    .into(),
            );
        }
        match execute_inner(name, args, self.ctx, true).await {
            Ok(out) => {
                // 子调用只把**文本**交给程序。图片（截图/view_image）不能进
                // JSON 协议，也不该悄悄丢掉 —— 明确告诉程序"这里有张图"。
                if out.image.is_some() {
                    Ok(json!({
                        "text": out.text,
                        "image_path": out.image,
                        "note": "该调用产生了一张图片，已单独附给你（不在程序里）。"
                    }))
                } else {
                    Ok(json!({ "text": out.text }))
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// `run_code` —— PTC 模式下模型唯一能直接调用的工具。
///
/// 流程见 `ptc.rs` 顶部文档。这里只负责：取参数 → 建工作目录 → 驱动 Node →
/// 把结果转成 [`ToolOutput`]。
async fn run_code(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let code = get_str(args, "code")?;
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    if code.trim().is_empty() {
        return Err("code 不能为空".into());
    }

    let Some(node) = crate::ptc::resolve_node() else {
        return Err(
            "找不到 Node（PTC 需要它来执行程序）。请安装 Node.js，\
             或把 node.exe 的路径写进环境变量 ORBCAT_NODE 后重启。"
                .into(),
        );
    };

    if !description.is_empty() {
        // 进度心跳复用 tick 通道：用户能在卡片上看到"在跑什么程序"
        if let Some(t) = ctx.tick.as_ref() {
            t(0, format!("run_code: {description}"));
        }
    }

    let work = crate::ptc::WorkDir::create(ctx.data_dir, &code)?;
    let sub = ToolCtxSub { ctx };

    // 超时给得比 run_command 宽：一个程序可能要跑几十次工具调用。
    // 真正的防死循环闸门是 MAX_SUB_CALLS，不是这个时间。
    let outcome = crate::ptc::run_program(&node, &work, &sub, ctx.cancel.clone(), 600).await?;

    eprintln!(
        "[orbcat] run_code: {}（{} 次子调用，{}）",
        description,
        outcome.calls,
        if outcome.ok { "成功" } else { "失败" }
    );

    Ok(ToolOutput::text(outcome.text))
}

/// 执行一个工具调用。
///
/// ## PTC 的「坍缩」在这里也要拦一道
///
/// PTC 下工具列表只剩 `run_code`（见 [`tool_specs`]），但模型**可能凭印象
/// 直接发一个别的工具名**（它上一轮还在标准模式、或从 SDK 声明里抄了个名字）。
/// 那时若放行，就出现"公告面与可调用面不一致"：公告说只有 run_code 能用，
/// 实际却执行了别的工具。
///
/// DSH 的 `dsh-tools` 把这条写成 `UNKNOWN_TOOL` 并强调
/// "通告面与可调用面保持一致" —— 同一个道理：**说了不让调，就真的不能调**，
/// 否则模型学到的规律是"公告不可信"。
///
/// 子调用（程序内 `tools.x`）走的是同一个 `execute`，所以这里必须放行它们 ——
/// 用 `in_ptc_sub` 标记区分"模型直接发的"与"程序里发的"。
pub async fn execute(
    name: &str,
    args: &Value,
    ctx: &ToolCtx<'_>,
) -> Result<ToolOutput, String> {
    execute_inner(name, args, ctx, false).await
}

/// 同 [`execute`]，但标记这次调用来自 PTC 程序内部（跳过坍缩检查）。
async fn execute_inner(
    name: &str,
    args: &Value,
    ctx: &ToolCtx<'_>,
    in_ptc_sub: bool,
) -> Result<ToolOutput, String> {
    // PTC 坍缩：模型直接发的调用只允许 run_code
    if !in_ptc_sub
        && name != crate::ptc::RUN_CODE
        && crate::ptc::mode_active(ctx.data_dir, Some(ctx.session_id))
    {
        return Err(format!(
            "当前是 PTC 模式，只能直接调用 `{}`。\
             工具 `{name}` 在 PTC 下不是直接调用的 —— \
             请写一段 TypeScript，在程序里用 `await tools.{name}(...)` 调它。",
            crate::ptc::RUN_CODE
        ));
    }

    match name {
        crate::ptc::RUN_CODE => run_code(args, ctx).await,
        "read_file" => read_file(args, ctx),
        "list_dir" => list_dir(args, ctx),
        "glob_files" => glob_files(args, ctx),
        "grep_files" => grep_files(args, ctx),
        "write_file" => write_file(args, ctx),
        "edit_file" => edit_file(args, ctx),
        "delete_file" => delete_file(args, ctx),
        "move_file" => move_file(args, ctx),
        "copy_file" => copy_file(args, ctx),
        "mkdir" => mkdir(args, ctx),
        "file_info" => file_info(args, ctx),
        "append_file" => append_file(args, ctx),
        "git" => git_tool(args, ctx),
        "run_command" => run_command(args, ctx).await,
        "web_search" => web_search(args, ctx).await,
        "fetch_url" => fetch_url(args, ctx).await,
        "request_access" => request_access(args, ctx).await,
        "capture_screen" => capture_screen(ctx.data_dir),
        "view_image" => view_image(args, ctx),
        "send_image" => send_image(args, ctx),
        "send_meme" => send_meme(args, ctx),
        "foreground_context" => {
            let out = match crate::context::cached().or_else(crate::context::current) {
                Some(c) => c.summary(),
                None => "无法获取前台窗口信息".into(),
            };
            Ok(ToolOutput::text(out))
        }
        "current_time" => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let secs = (ms / 1000) % 60;
            Ok(ToolOutput::text(format!(
                "{}:{:02}（{}），UTC+8，epoch 毫秒 {}",
                crate::history::fmt_local(ms),
                secs,
                crate::history::weekday_cn(ms),
                ms
            )))
        }
        "remember" => remember(args, ctx.data_dir),
        "distill_move" => distill_move(args, ctx.data_dir),
        "life_items" => life_items(args, ctx).await,
        "ask_user" => ask_user(args, ctx).await,
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
        "search_tools" => {
            // query 是可选参数：缺失 = 列全部（"我到底有什么工具"）。
            // 这是**能力发现**入口，故意不产生任何副作用：不改活跃集、不弹权限卡，
            // 模型可以放心多调几次 —— 一次 search 只回几十行文本，比
            // load_tool_group 试探便宜两个数量级。
            let q = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let reg = ctx.mcp.lock().await;
            eprintln!("[orbcat] search_tools: {q:?}");
            Ok(ToolOutput::text(reg.search_tools(&q)))
        }
        "list_tool_groups" => {
            let reg = ctx.mcp.lock().await;
            Ok(ToolOutput::text(reg.describe_groups()))
        }
        "load_tool_group" => {
            let group = get_str(args, "name")?;
            let mut reg = ctx.mcp.lock().await;
            let msg = reg.load(&group)?;
            eprintln!("[orbcat] MCP 组已加载: {group}");
            Ok(ToolOutput::text(msg))
        }
        "unload_tool_group" => {
            let group = get_str(args, "name")?;
            let mut reg = ctx.mcp.lock().await;
            Ok(ToolOutput::text(reg.unload(&group)?))
        }
        other => {
            // ── L1：当前项目的脚本工具 ──────────────────────────────────
            //
            // 放在 MCP 分支**之前**：项目工具名是普通标识符（`check_homework`），
            // 与 `mcp__*` 命名不冲突；但先查项目能让"项目工具"这条路径
            // 不受 MCP 未连接状态影响。
            if let Some(a) = crate::project::active(ctx.data_dir) {
                if let Some(t) = a.bundle.tools.iter().find(|t| t.name == other) {
                    return run_project_tool(t, args, ctx).await;
                }
            }

            // MCP 工具：mcp__<server>__<tool>
            if let Some((_server, tool)) = crate::mcp::parse_mcp_tool_name(other) {
                // 先查是否已加载 —— 未加载的组不该弹权限卡（用户批了也执行不了）
                let reg = ctx.mcp.lock().await;
                let loaded = reg
                    .active_tools()
                    .iter()
                    .any(|t| t.name == tool);
                if !loaded {
                    // 默认配置下所有组都直接给模型，走到这里通常意味着：
                    // 用户把这组关掉了（MCP 设置页的「给模型」开关），或组名对不上。
                    return Err(format!(
                        "工具「{other}」所属的组当前**未启用**。\
                         去设置 › MCP 外部工具 把该组的「给模型」打开（或用 load_tool_group 加载），\
                         再重试；也可以用 list_tool_groups 看当前有哪些组可用。"
                    ));
                }
                drop(reg);

                // Cline 式：默认问；「总是允许」写 mcp_grants.json 后免问。
                // 执行权限档位 `full` 也免问（与命令侧同一开关，见 config::ExecTrust）。
                //
                // ⚠️ 档位取 `project::effective_exec_trust`：拼装项目可以**预选**
                // 档位。这是"预选"而非"绕过"—— `full` 本来就能由用户自己在设置里
                // 打开，项目只是替用户预先选了；硬阻断与防骚扰逻辑完全不受影响。
                let allowed = crate::mcp::is_mcp_tool_allowed(ctx.data_dir, other)
                    || crate::project::effective_exec_trust(ctx.data_dir).auto_allows_mcp();
                if !allowed {
                    let verdict = ctx
                        .perm
                        .ask_mcp_tool(other.to_string(), crate::agent::summarize_args_pub(&args.to_string()), ctx.session_id)
                        .await?;
                    if !verdict.is_approved() {
                        return Ok(ToolOutput::text(verdict.to_model_message()));
                    }
                    // ApproveFull = 总是允许 → 落盘
                    if matches!(verdict.decision, crate::perm_request::Decision::ApproveFull) {
                        crate::mcp::allow_mcp_tool(ctx.data_dir, other)?;
                    }
                }

                let reg = ctx.mcp.lock().await;
                let msg = reg.call_tool(&tool, args).await?;
                return Ok(ToolOutput::text(msg));
            }
            Err(format!("未知工具：{other}"))
        }
    }
}

/// `life_items` —— 取某个数据源的**明细**。
///
/// ## 为什么需要它（摘要已经常驻了）
///
/// system prompt 里只常驻每个源**一行**（条数 + 新鲜度，见 `life.rs`）——
/// 那是为了让模型"知道你的处境"，而不是把全文塞进每轮请求。
/// 细节（作业标题、截止、备注）要模型主动来拿。
///
/// ## 为什么不把明细也常驻
///
/// 用户 2026-10 拍板「一行摘要常驻 + 详情按需」。全文常驻的代价是
/// **每轮都在烧 token**，而且快照过期时模型会把旧数据当最新事实说 ——
/// 那比"不知道"更糟。
///
/// ## 不带 `source` 参数 = 列出所有源
///
/// 与 `search_tools` 留空列出全部索引同一口径：模型可以先看一眼有哪些源。
async fn life_items(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let want = args
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let f = crate::life::load(ctx.data_dir);
    if f.sources.is_empty() {
        return Ok(ToolOutput::text(
            "本机还没有配置任何数据源。\
             在 设置 › 数据源 里加一个（读一个 JSON 快照，或跑一条命令）。",
        ));
    }

    let enabled: Vec<crate::life::Source> =
        f.sources.iter().filter(|s| s.enabled).cloned().collect();
    if enabled.is_empty() {
        return Ok(ToolOutput::text(
            "配置里所有数据源都是关闭状态。要读取请在 设置 › 数据源 里打开。",
        ));
    }

    // 不带 source → 列出有哪些源（不读数据，便宜）
    if want.is_empty() {
        let mut out = String::from("已启用的数据源：\n");
        for s in &enabled {
            out.push_str(&format!("- {}（id: `{}`）\n", s.name(), s.id));
        }
        out.push_str("\n要看某个源的明细，再调一次并传 `source`。");
        return Ok(ToolOutput::text(out));
    }

    let Some(src) = enabled.iter().find(|s| s.id == want) else {
        let ids: Vec<String> = enabled.iter().map(|s| s.id.clone()).collect();
        return Err(format!(
            "没有 id 为「{want}」的已启用数据源。可用的：{}",
            ids.join(", ")
        ));
    };

    // 用当前工作目录跑 command 类源（与设置页/摘要同一条路径）
    let cwd = std::env::current_dir().unwrap_or_else(|_| ctx.data_dir.to_path_buf());
    let snap = crate::life::read_source(src, &cwd).await;

    if !snap.ok {
        return Ok(ToolOutput::text(format!(
            "读取「{}」失败：{}\n\n\
             （这是数据源侧的问题，不是工具坏了。可以告诉用户去 设置 › 数据源 看看，\
             或检查产出快照的那个程序有没有在跑。）",
            snap.label, snap.error
        )));
    }

    if snap.items.is_empty() {
        return Ok(ToolOutput::text(format!(
            "{}：当前没有条目{}。",
            snap.label,
            if snap.updated_at.is_empty() {
                String::new()
            } else {
                format!("（快照 {}）", snap.updated_at)
            }
        )));
    }

    let mut out = format!("{}：{} 条", snap.label, snap.items.len());
    if !snap.updated_at.is_empty() {
        out.push_str(&format!("（快照 {}）", snap.updated_at));
    }
    out.push('\n');
    if snap.stale {
        out.push_str(&format!(
            "⚠️ 该快照已 {} 小时未更新，可能不是最新 —— 说明情况时要点出来。\n",
            snap.age_hours.unwrap_or(0.0)
        ));
    }
    for (i, it) in snap.items.iter().enumerate() {
        out.push_str(&format!("\n{}. {}", i + 1, it.title));
        if !it.group.is_empty() {
            out.push_str(&format!("　[{}]", it.group));
        }
        if !it.due_raw.is_empty() {
            out.push_str(&format!("\n   截止：{}", it.due_raw));
        }
        if !it.note.is_empty() && it.note != it.title {
            out.push_str(&format!("\n   {}", it.note));
        }
    }
    Ok(ToolOutput::text(out))
}

// ---------------------------------------------------------------------------
// recall_turns
// ---------------------------------------------------------------------------

/// 查更早的对话原文。
///
/// 为什么需要它：`history.rs` 只回灌「最近 6h ∪ 最近 10 条」，
/// 更早的内容不在上下文里。没有这个工具，模型遇到"上次那个方案"
/// 只能瞎编或反问用户。原文**永久**留在 `sessions/<id>.json`，这里负责捞回来。
///
/// ## 范围：默认只查当前会话（`scope=session`）
///
/// 跨会话检索（`scope=all`）**必须由模型显式要求**，理由有两条：
///   1. **成本**：单会话只读一个文件；跨会话要遍历 `sessions/*.json`
///      （实测 31 个 / 10.2 MB），而本函数是同步执行的，会占住 tokio 工作线程；
///   2. **上下文污染**：别的会话聊的是别的事，把它们灌进当前上下文会让模型
///      把两个话题搅在一起 —— 那是"答错"而不是"答慢"，代价更高。
/// 所以默认值是保守的一侧，激进的一侧要模型自己开口（见 system prompt 的
/// 「# 对话历史」段：加了能力不回写提示词，工具就等于摆设）。
///
/// ## 为什么要走权限网关
///
/// `sessions/` 在 `agent-data/` 内，读它跟读技能目录、读普通文件是同一种行为，
/// 所以走同一个出口（同 `load_skill` 的纪律）。理由不只是"守规矩"：
/// 用户可能在设置里把 `agent-data` 从权限规则里删掉（比如把数据目录挪到别处），
/// 那时**所有**文件工具都该一致地拒绝，而不是这里留一个不检查的裸读后门。
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
    // 只认字面量 "all"：写错、写成大写、留空都落回默认的当前会话。
    // 保守方向是对的 —— 认错了最多是"没查到，再调一次"，认反了就是污染上下文。
    let all = matches!(
        args.get("scope").and_then(Value::as_str).map(str::trim),
        Some("all")
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    if !all {
        return recall_in_session(ctx, &query, limit, hours, now);
    }

    // 跨会话：先过权限网关再读目录，与 `load_skill` 同一写法。
    if let Err(e) = authorize(ctx, crate::sessions::sessions_dir(ctx.data_dir), Access::Read) {
        return Err(format!("会话目录无读取权限，无法跨会话检索：{e}"));
    }

    let scan = crate::history::search_all_sessions(
        ctx.data_dir,
        &query,
        limit,
        hours,
        now,
        crate::history::CROSS_SESSION_MAX_FILES,
        crate::history::CROSS_SESSION_MAX_BYTES,
    );
    let hits = &scan.hits;

    let scope_desc = if query.is_empty() {
        "最近聊过的".to_string()
    } else {
        format!("含「{query}」的")
    };

    if hits.is_empty() {
        // 「没找到」和「没查完」是两回事：前者可以让模型换个关键词，
        // 后者只说明预算用完了 —— 不说清楚，模型会把"扫描被截断"
        // 当成"库里确实没有"，然后对着用户打包票说"没这回事"。
        return Ok(ToolOutput::text(format!(
            "在所有会话里都没找到{what}记录（本次扫了 {n} 个会话{extra}）。\n{hint}",
            what = if query.is_empty() {
                String::new()
            } else {
                format!("含「{query}」的")
            },
            n = scan.scanned,
            extra = if scan.truncated { "，已达扫描上限" } else { "" },
            hint = if scan.truncated {
                "注意：**还有更早的会话没查**（本次扫描到达上限）—— 这不等于「没有」。\
                 请换更具体的关键词（人名 / 项目名 / 报错原文）再试，\
                 或让用户说清大概是哪一次。"
            } else {
                "可能原因：关键词不对，或确实没聊过这个话题。\
                 可以试着换个关键词，或不传 query 翻一下最近的记录。"
            }
        )));
    }

    let mut out = format!(
        "在**所有会话**（含当前会话）里找到 {} 条{}记录，按时间倒序：\n",
        hits.len(),
        scope_desc
    );
    for (i, sh) in hits.iter().enumerate() {
        let who = if sh.hit.role == "user" { "用户" } else { "你" };
        // 来源标识挨着说话人放；时间已经含在来源里，不再重复第二遍时间。
        out.push_str(&format!(
            // 序号用命名参数 `{n}`：写成 `{}.` + `i + 1` 时，若同时把别的参数
            // 写成 `idx = ...` 形式，rustc 会警告 `named argument idx is not
            // used by name`（名字没在格式串里用到）—— 命名与位置参数不要混用。
            "\n{n}. [{who} · {src}] {text}\n",
            n = i + 1,
            src = sh.source,
            text = sh.hit.text
        ));
    }
    out.push_str(&format!(
        "\n（以上是历史记录的节选，工具输出已在落盘时折叠成一行。\
         本次扫了 {scanned} 个会话{extra}。\
         **注意：这些内容来自其他会话，不是你当前上下文的一部分** —— \
         引用时要说明「这是在会话《…》里做的」，不要把它当成当前会话里说过的话。）",
        scanned = scan.scanned,
        extra = if scan.truncated { "（已达上限，更早的没查）" } else { "" },
    ));

    Ok(ToolOutput::text(out))
}

/// `scope=session`（默认）：只查当前会话。行为与加 `scope` 参数之前**完全一致**。
fn recall_in_session(
    ctx: &ToolCtx<'_>,
    query: &str,
    limit: usize,
    hours: Option<u64>,
    now: u64,
) -> Result<ToolOutput, String> {
    let hits = crate::history::search(ctx.data_dir, ctx.session_id, query, limit, hours, now);

    if hits.is_empty() {
        return Ok(ToolOutput::text(format!(
            "没找到匹配的对话记录{}。\n可能原因：关键词不对，或本会话确实没聊过这个话题。\n\
             可以试着换个关键词，或不传 query 翻一下最近的记录。\n\
             （如果这是**别处**（其他会话）聊过的事，用 scope=all 跨会话再查一次。）",
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
/// 提交长期记忆。
/// - **主对话**（无激活项目）→ 全局 pending 审批
/// - **已切到项目** → 直接追加 `projects/<名>/MEMORY.md`（跳脱主对话）
fn remember(args: &Value, data_dir: &Path) -> Result<ToolOutput, String> {
    let content = get_str(args, "content")?;
    let settings = crate::config::load_settings(data_dir);
    if let Some(pid) = settings.active_project_id {
        let projects = crate::pmem::list_projects(data_dir)?;
        if let Some(p) = projects.into_iter().find(|x| x.id == pid) {
            crate::pmem::append_project_memory(data_dir, &p.name, &content, "model")?;
            return Ok(ToolOutput::text(format!(
                "已写入项目「{}」的记忆（projects/{}/MEMORY.md），跨会话有效。",
                p.name, p.name
            )));
        }
    }
    let store = crate::memory::MemoryStore::new(data_dir);
    let c = store.propose(&content, "model")?;
    Ok(ToolOutput::text(format!(
        "已提交全局记忆候选（id: {}），等用户在面板里确认后才会进入长期记忆。",
        c.id
    )))
}

/// `distill_move` 的 schema —— **只在蒸馏会话注入**（见 [`tool_specs_for_distill`]）。
fn distill_move_spec() -> ToolSpec {
    ToolSpec::new(
        "distill_move",
        "把长期记忆中转站（MEMORY.md）里的**一条**迁移到目标文件，并在成功后从中转站移除。\
         仅在「记忆蒸馏」会话里可用。执行前会自动备份 MEMORY.md（按天）。\
         `target`：user（用户画像）/ soul（人格语气）/ identity（称呼）/ duplicate（目标已有同文，只从中转站删）/ stay（保持不动）。\
         `text` 必须与 MEMORY.md 里的条目正文一致（不含行首 \"- \"），否则报错。\
         一次只迁一条；多条请多次调用（每条都会单独返回结果）。",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "条目正文（与 MEMORY.md 逐字一致，不含行首 \"- \"）" },
                "target": { "type": "string", "enum": ["user", "soul", "identity", "duplicate", "stay"], "description": "迁往哪：user 用户画像 / soul 人格语气 / identity 称呼 / duplicate 已有同文只删中转 / stay 不动" },
                "section": { "type": "string", "description": "目标文件里的小节标题（如 \"## 偏好\"）；留空则追加到文件末尾" },
                "reason": { "type": "string", "description": "迁移理由（会在结果里回显，便于你向用户交代）" }
            },
            "required": ["text", "target"]
        }),
    )
}

/// `distill_move` —— 蒸馏会话专用：把中转站里的一条迁到目标文件并移除之。
///
/// 返回文本要**说清结果**（写进哪个文件/小节、是否跳过、中转站已移除、备份在哪），
/// 因为模型会把它原样转述给用户 —— 含糊的返回会让用户不知道到底改没改。
fn distill_move(args: &Value, data_dir: &Path) -> Result<ToolOutput, String> {
    let p = crate::distill::DistillProposal {
        text: get_str(args, "text")?,
        target: get_str(args, "target")?,
        section: args
            .get("section")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        reason: args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    };
    let r = crate::distill::apply_entry(data_dir, &p)?;
    let where_ = if p.section.trim().is_empty() {
        format!("{}.md 末尾", p.target.to_uppercase())
    } else {
        format!("{}.md {}", p.target.to_uppercase(), p.section.trim())
    };
    let head = if !p.reason.trim().is_empty() {
        format!("{}（理由：{}）", clip(&p.text, 60), p.reason.trim())
    } else {
        clip(&p.text, 60)
    };
    let mut msg = match p.target.as_str() {
        "stay" => format!("「{head}」按你的判断留在中转站，未改动任何文件。"),
        "duplicate" => format!(
            "「{head}」目标文件已有同文，未重复写入；已从中转站移除。备份：memory/{}",
            r.backup
        ),
        _ => {
            if r.moved == 1 {
                format!("「{head}」已写入 {where_}，并从中转站移除。备份：memory/{}", r.backup)
            } else {
                format!(
                    "「{head}」**未写入**（{}），但已从中转站移除。备份：memory/{}",
                    r.skipped.first().cloned().unwrap_or_else(|| "目标已有同文".into()),
                    r.backup
                )
            }
        }
    };
    if r.removed == 0 {
        msg.push_str("（未从中转站移除）");
    }
    Ok(ToolOutput::text(msg))
}

/// 短文本裁剪（工具结果里回显用）
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------------------
// capture_screen / view_image
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

/// 查看一张图片文件。路径经权限网关后，把**文件路径**随 `ToolOutput.image` 带出，
/// 由 agent loop 按模型能力转 base64 / 占位 —— 与 `capture_screen` 同一条递送链路。
/// 当前模式允不允许发表情包。
///
/// `send_meme` 的**可见性**（`all_tool_specs` 里决定要不要给这个 schema）
/// 与**可调用性**（`send_meme` 里的二次检查）共用这一处判断 —— 两条路径
/// 分头判断的话，迟早出现"列表里没有但调了居然能过"这种裂缝。
fn memes_allowed(data_dir: &Path, session_id: Option<&str>) -> bool {
    crate::sessions::effective_mode(data_dir, session_id).freestyle_images()
}

/// `send_image` 的工具声明 —— **常驻**（所有模式都有）。
fn send_image_spec() -> ToolSpec {
    ToolSpec::new(
        "send_image",
        "把一张**图片**贴进你的回复，让用户直接看到（截图、图表、示意图、网图…）。\n\
         \n\
         调用后你会拿到一段 markdown。⚠️ **必须把它原样复制到你的回复正文里** —— \
         工具返回值本身折叠在「过程」里，用户看不见；只有贴进正文才会显示成图。\n\
         \n\
         什么时候用：刚 `capture_screen` 截了屏、或你知道某个图片文件的路径，\
         想让用户看到它。**纯文本能说清的就别发图**。\n\
         \n\
         跟 `view_image` 的分工：那个是**你看**（画面进下一轮，你来描述）；\
         这个是**给用户看**（他亲眼看到原图）。",
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "图片绝对路径（png/jpg/jpeg/webp/gif/bmp），或 http(s) 网址"
                },
                "caption": {
                    "type": "string",
                    "description": "图的说明（当 alt 用），可省"
                }
            },
            "required": ["path"]
        }),
    )
}

/// `send_meme` 的工具声明 —— **只在能随便发图的模式里出现**。
fn send_meme_spec() -> ToolSpec {
    ToolSpec::new(
        "send_meme",
        "从你的**表情包库**里挑一张发出去（闲聊用）。\n\
         \n\
         库的位置和**可用标签**见系统提示的「表情包库」段。`query` 填一个标签\
         （如「无语」「摸鱼」「大笑」）；不传就随机挑一张。\n\
         一个标签下面通常有**好几张**，工具会随机挑其中一张 —— 你不会每次拿到同一张，\
         所以**同一轮里没必要为了\"换一张\"再调一次**。\n\
         \n\
         调用后你会拿到一段 markdown。⚠️ **必须把它原样复制到你的回复正文里**，\
         否则用户什么都看不到。\n\
         \n\
         找不到匹配的标签时工具会报错并列出可用的 —— 那时**换个标签重试，或者干脆不发**，\
         绝对不要自己编一个路径（那只会渲染成「图（读不出来）」）。\n\
         \n\
         分寸：情绪到位才发，一条回复最多一张；用户明显在正经办事时不要发。",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "标签词，如「无语」「摸鱼」。省略则全库随机"
                }
            },
            "required": []
        }),
    )
}

/// `send_image` —— 把一张现成的图贴进回复。
///
/// 它**不**自己做渲染：只返回一段 markdown，由模型贴进正文，前端 `mdToHtml`
/// 的图片分支负责显示（见 `markdown.ts` 的 `inline()`）。这样图出现在正文里，
/// 而不是折叠的「过程」里 —— 后者用户根本看不到。
fn send_image(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let caption = args
        .get("caption")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    // 外链直接放行（前端当 `<img src>` 加载，没读本地文件，不走权限网关）；
    // 本地路径必须过网关 + 真存在 —— 同 `view_image` 的口径，不新开绕过面。
    let is_url = raw.starts_with("http://") || raw.starts_with("https://");
    let shown = if is_url {
        raw.clone()
    } else {
        let safe = authorize(ctx, &raw, Access::Read)?;
        if safe.is_dir() {
            return Err(format!("{} 是目录，不是图片。", safe.display()));
        }
        if !crate::images::is_image_path(&safe) {
            return Err(format!(
                "{} 不是图片文件（支持 png/jpg/jpeg/webp/gif/bmp）。",
                safe.display()
            ));
        }
        safe.display().to_string()
    };

    let alt = if caption.is_empty() { "图" } else { caption.as_str() };
    Ok(ToolOutput::text(format!(
        "把下面这一行**原样复制**到你的回复里（不复制的话用户看不到图）：\n\n\
         ![{}]({})",
        alt, shown
    )))
}

/// `send_meme` —— 从表情包库挑一张。
fn send_meme(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    // 双保险：非 free 模式下这个工具本来就**不在工具表里**（见 `all_tool_specs`），
    // 但模型可能凭上一轮的记忆硬调。那时给它一句能听懂的话，比含糊的
    // "没有这个工具"有用 —— 后者会让它反复重试。
    if !memes_allowed(ctx.data_dir, Some(ctx.session_id)) {
        let mode = crate::sessions::effective_mode(ctx.data_dir, Some(ctx.session_id));
        return Err(format!(
            "当前是「{}」，这个模式不发表情包（要贴工作产物图请用 `send_image`）。\
             想发表情包得让用户切到闲聊模式。",
            mode.name
        ));
    }

    let q = args.get("query").and_then(|v| v.as_str());
    let picked = crate::memes::pick(ctx.data_dir, q)?;
    Ok(ToolOutput::text(format!(
        "把下面这一行**原样复制**到你的回复里（不复制的话用户看不到图）：\n\n\
         ![{}]({})",
        picked.label,
        picked.path.display()
    )))
}

fn view_image(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe = authorize(ctx, &raw, Access::Read)?;

    if safe.is_dir() {
        return Err(format!("{} 是目录，不是图片。请给出具体图片文件路径。", safe.display()));
    }
    if !crate::images::is_image_path(&safe) {
        return Err(format!(
            "{} 不是图片文件（支持 png/jpg/jpeg/webp/gif/bmp）。\n\
             文本请用 read_file；PDF 等文档请用 skills 处理。",
            safe.display()
        ));
    }

    let meta = std::fs::metadata(&safe).map_err(|e| format!("读取失败: {e}"))?;
    if meta.len() == 0 {
        return Err(format!("{} 是空文件。", safe.display()));
    }

    // 尺寸尽力而为：image crate 未编译的格式（webp/gif…）拿不到就跳过，
    // 不影响递送 —— to_data_url 只读字节。
    let dim = image::image_dimensions(&safe)
        .ok()
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "未知尺寸".into());

    let path = safe.to_string_lossy().into_owned();
    Ok(ToolOutput {
        text: format!(
            "已读取图片（{dim}，{:.1} KB），画面内容附在下一条消息里。\n路径：{path}",
            meta.len() as f64 / 1024.0
        ),
        image: Some(path),
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
    // 图片走 view_image：read_file 是文本工具，对图片只会吐乱码
    if crate::images::is_image_path(&safe_path) {
        return Err(format!(
            "{} 是图片文件，请改用 view_image 查看画面内容（read_file 只能读文本）。",
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

    let recursive = args
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let depth = args
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 4) as usize;

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();

    fn walk_list(
        root: &Path,
        prefix: &str,
        depth_left: usize,
        recursive: bool,
        dirs: &mut Vec<String>,
        files: &mut Vec<(String, u64)>,
    ) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for e in entries.flatten() {
            if dirs.len() + files.len() >= MAX_LIST_ENTRIES {
                return;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let label = format!("{prefix}{name}");
            match e.metadata() {
                Ok(md) if md.is_dir() => {
                    dirs.push(label.clone());
                    if recursive && depth_left > 0 {
                        walk_list(
                            &e.path(),
                            &format!("{label}/"),
                            depth_left - 1,
                            recursive,
                            dirs,
                            files,
                        );
                    }
                }
                Ok(md) => files.push((label, md.len())),
                Err(_) => files.push((label, 0)),
            }
        }
    }

    walk_list(
        &safe_path,
        "",
        depth,
        recursive,
        &mut dirs,
        &mut files,
    );

    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = format!("{}\n", safe_path.display());
    if recursive {
        out.push_str(&format!("（递归 depth≤{depth}）\n"));
    }
    out.push_str(&format!("目录 {} 个：\n", dirs.len()));
    for d in &dirs {
        out.push_str(&format!("  [DIR]  {d}\n"));
    }
    out.push_str(&format!("文件 {} 个：\n", files.len()));
    for (f, sz) in &files {
        out.push_str(&format!("  {:>9}B  {f}\n", sz));
    }
    if dirs.len() + files.len() >= MAX_LIST_ENTRIES {
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

/// 命中行在正文里最多显示多少字符（围绕**命中位置**取窗口，不是从行首截）。
///
/// ⚠️ 为什么必须围绕命中位置：压缩后的单行文件动辄几十万字符
/// （实测 `trae_dump.cjs` 单行 116 万字符，词在第 50 万字符处）。
/// 从行首 `take(200)` 会让模型看到一行**不含所搜词**的内容 ——
/// 它据此判断"命中了但内容对不上"，然后放弃工具去写 Python 脚本。
const GREP_LINE_CHARS: usize = 200;

/// 文件大于这个体积就跳过（并在结果里**明确回报**跳过了几个）。
const GREP_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// grep 的跳过统计 —— 每一条都要出现在返回文本里。
///
/// ## 为什么这是本工具最重要的设计（2026-10-01 定论）
/// 旧版每一条跳过路径都是裸 `return`，返回的永远是 `(无匹配)`。
/// 模型**无法区分**「真的没有」和「工具压根没读」—— 实测 543 次调用里
/// 58% 报零命中，其中 145 次是 `path` 传了文件（`read_dir` 对文件必失败）、
/// 还有大批撞在 2MB 上限与非 UTF-8 上。模型的反应是放弃工具改跑 PowerShell，
/// 于是绕开权限网关、被 24KB 截断、甚至查错文件得出错误结论。
///
/// **跳过必须可见** —— 这是让模型能自我纠正的前提。
#[derive(Default)]
struct GrepSkips {
    too_big: usize,
    not_utf8: usize,
    denied: usize,
    ext_filtered: usize,
    visited_limit: bool,
}

impl GrepSkips {
    fn any(&self) -> bool {
        self.too_big + self.not_utf8 + self.denied + self.ext_filtered > 0 || self.visited_limit
    }

    /// 拼成给模型看的说明。**没有跳过就返回空串**，避免噪声。
    fn describe(&self) -> String {
        if !self.any() {
            return String::new();
        }
        let mut parts = Vec::new();
        if self.too_big > 0 {
            parts.push(format!(
                "{} 个文件超过 {} MB 未读",
                self.too_big,
                GREP_MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        if self.not_utf8 > 0 {
            parts.push(format!(
                "{} 个文件不是 UTF-8 文本（GBK/二进制）未读",
                self.not_utf8
            ));
        }
        if self.denied > 0 {
            parts.push(format!("{} 个文件因权限被过滤", self.denied));
        }
        if self.ext_filtered > 0 {
            parts.push(format!("{} 个文件因 ext 过滤未读", self.ext_filtered));
        }
        if self.visited_limit {
            parts.push("已达扫描文件数上限，后面的没扫".to_string());
        }
        format!("\n⚠️ 注意：{}\n", parts.join("；"))
    }
}

/// 读文本文件：严格 UTF-8 优先，失败回落 GBK（中文 Windows 的 .md/.txt 常见）。
///
/// 旧版用 `std::fs::read_to_string` —— 它只认严格 UTF-8，GBK 文件直接失败，
/// 然后被静默 `return` 掉。这台机器上 `D:\myword\文章\skills\nature-figure\SKILL.md`
/// 就是 GBK，grep 永远搜不到它。
fn read_text_lossy(path: &Path) -> Option<(String, bool)> {
    let bytes = std::fs::read(path).ok()?;
    match String::from_utf8(bytes) {
        Ok(s) => Some((s, true)),
        Err(e) => {
            let bytes = e.into_bytes();
            // 回落 CP_ACP（本机 = GBK）。GBK 解不出来就是二进制，跳过。
            let s = crate::win32::decode_acp(&bytes);
            if s.is_empty() {
                None
            } else {
                Some((s, false))
            }
        }
    }
}

fn grep_files(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let needle = get_str(args, "pattern")?;
    let raw = get_str(args, "path")?;
    let roots = authorize(ctx, &raw, Access::Read)?;

    // 不支持正则（本项目不引 regex crate）。schema/描述只承诺子串匹配。
    let use_regex = args
        .get("regex")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if use_regex {
        return Err(
            "grep_files 只支持子串匹配（regex=true 未启用）。请用普通 pattern，或 run_command + rg".into(),
        );
    }
    // 模型习惯性写正则（实测 32 次：`a|b`、`.*`、`\[x\]`）—— 子串匹配下必然 0 命中。
    // 与其让它拿到"无匹配"去猜，不如直接点破。
    if let Some(hint) = regex_looking_pattern(&needle) {
        return Err(format!(
            "grep_files 只做**字面量子串**匹配，不支持正则。你给的 pattern 含正则语法（{hint}），\
             照这样搜一定 0 命中。\n\
             → 请改成普通子串（例如搜 `foo` 而不是 `foo|bar`），\
             多个词就**分开搜几次**；确实需要正则时用 run_command 跑 rg。"
        ));
    }

    let context = args
        .get("context")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(3) as usize;

    // 大小写：默认不敏感（实测 319 次零命中里 206 次 pattern 纯小写，
    // 而目标是 `Custom Endpoint` 这类首字母大写 —— 大小写敏感是纯坑）。
    let case_sensitive = args
        .get("caseSensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let exts: Vec<String> = args
        .get("ext")
        .and_then(Value::as_str)
        .map(|s| s.split(',').map(|x| x.trim().to_lowercase()).collect())
        .unwrap_or_default();

    let mut hits: Vec<String> = Vec::new();
    let mut hit_count = 0usize;
    let mut visited = 0usize;
    let mut skips = GrepSkips::default();

    // 搜索单个文件的内容。抽成闭包是因为下面要处理两种入口：
    // ① `path` 是目录 → walk 出每个文件；② `path` 是文件 → 直接搜它。
    let needle_cmp = if case_sensitive {
        needle.clone()
    } else {
        needle.to_lowercase()
    };
    let scan_file = |p: &Path, hits: &mut Vec<String>, hit_count: &mut usize, skips: &mut GrepSkips| {
        if *hit_count >= MAX_GREP_HITS {
            return;
        }
        // 扩展名过滤
        if !exts.is_empty() {
            let ok = p
                .extension()
                .map(|e| exts.contains(&e.to_string_lossy().to_lowercase()))
                .unwrap_or(false);
            if !ok {
                skips.ext_filtered += 1;
                return;
            }
        }

        // 大文件跳过（**计数**，不再静默）
        let Ok(md) = p.metadata() else { return };
        if md.len() > GREP_MAX_FILE_BYTES {
            skips.too_big += 1;
            return;
        }

        // 权限：逐个文件再确认（防止 walk 过程中穿过 junction）。
        // ⚠️ 同 glob：用**不消费**的 check_passive，否则一次 grep 会把「一次」授权吃光。
        let Some(safe) = check_passive(ctx, p, Access::Read) else {
            skips.denied += 1;
            return;
        };
        // 非 UTF-8 回落 GBK；解不出来才算"不是文本"
        let Some((content, _is_utf8)) = read_text_lossy(&safe) else {
            skips.not_utf8 += 1;
            return;
        };

        let all: Vec<&str> = content.lines().collect();
        for (i, line) in all.iter().enumerate() {
            if *hit_count >= MAX_GREP_HITS {
                return;
            }
            let matched = if case_sensitive {
                line.contains(&needle_cmp)
            } else {
                line.to_lowercase().contains(&needle_cmp)
            };
            if !matched {
                continue;
            }
            *hit_count += 1;
            if context > 0 {
                let start = i.saturating_sub(context);
                let end = (i + context + 1).min(all.len());
                for j in start..end {
                    let mark = if j == i { ">" } else { " " };
                    // 上下文行也要围绕命中位置取窗口
                    let shown = window_around(all[j], &needle_cmp, case_sensitive, 120);
                    hits.push(format!("{}:{}: {mark} {}", safe.display(), j + 1, shown));
                }
            } else {
                let shown = window_around(line, &needle_cmp, case_sensitive, GREP_LINE_CHARS);
                hits.push(format!("{}:{}: {}", safe.display(), i + 1, shown));
            }
        }
    };

    if roots.is_file() {
        // ★ 2026-10-01 修：`path` 传文件时**直接搜这个文件**。
        //
        // 旧版把 roots 丢给 `walk()`，而 walk 第一件事是 `read_dir` ——
        // 对文件必然失败，然后裸 `return`，于是返回「命中 0 行」。
        // 实测这一条坑掉了 26% 的调用（145/543），模型完全无从察觉。
        scan_file(&roots, &mut hits, &mut hit_count, &mut skips);
    } else {
        walk(&roots, &mut |p: &Path| {
            if hit_count >= MAX_GREP_HITS {
                return;
            }
            visited += 1;
            if visited > 20000 {
                skips.visited_limit = true;
                return;
            }
            scan_file(p, &mut hits, &mut hit_count, &mut skips);
        });
    }

    // ⚠️ 标题里的"命中 N 行"必须是**真正的命中行数**（hit_count），
    //    不能是 `hits.len()` —— 带 context 时后者是"命中行 + 上下文行"，
    //    实测 context=2 报「命中 200 行」其实只有 40 处命中（虚高 160）。
    let scope = if roots.is_file() {
        format!("文件 {}", roots.display())
    } else {
        format!("{} 中", roots.display())
    };
    let mut out = format!("在 {}搜索「{}」，命中 {} 行\n", scope, needle, hit_count);
    for h in &hits {
        out.push_str(h);
        out.push('\n');
    }
    if hit_count >= MAX_GREP_HITS {
        out.push_str(&format!("\n… [已截断，最多 {MAX_GREP_HITS} 行命中]\n"));
    }
    out.push_str(&skips.describe());
    if hit_count == 0 {
        if skips.any() {
            out.push_str("(在这些被读取的文件里无匹配 —— 注意上面列出的跳过项，答案可能就在里面)\n");
        } else {
            out.push_str("(无匹配)\n");
        }
    }

    Ok(ToolOutput::text(out))
}

/// 在长行里**围绕命中位置**取一段窗口，而不是从行首截。
///
/// 压缩/单行文件里命中位置可能在几十万字符处，从行首截等于返回一段
/// 与所搜词无关的内容 —— 模型会认为"命中了但内容不对"，进而放弃工具。
fn window_around(line: &str, needle_lower: &str, case_sensitive: bool, max_chars: usize) -> String {
    let trimmed = line.trim();
    // 找到命中位置（字符下标，避免多字节切坏）
    let hay = if case_sensitive {
        trimmed.to_string()
    } else {
        trimmed.to_lowercase()
    };
    let byte_pos = hay.find(needle_lower).unwrap_or(0);
    // 字节位置 → 字符位置
    let char_pos = hay[..byte_pos].chars().count();

    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max_chars {
        return trimmed.to_string();
    }
    // 命中处前面留 1/4 窗口，保证词可见且有上下文
    let lead = max_chars / 4;
    let start = char_pos.saturating_sub(lead);
    let end = (start + max_chars).min(chars.len());
    let start = end.saturating_sub(max_chars);
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s.insert_str(0, "…");
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// pattern 看着像正则就返回命中的语法提示（用于给出**可操作**的报错）。
///
/// 只认明确的信号，避免把普通文本误判：`|` 交替、`.*`/`.+`、`\d`/`\w`/`\s`、
/// 字符类 `[...]`、锚点 `^`/`$`、转义括号 `\(`/`\[`。
fn regex_looking_pattern(pat: &str) -> Option<&'static str> {
    if pat.contains("|") {
        return Some("`|` 交替");
    }
    if pat.contains(".*") || pat.contains(".+") {
        return Some("`.*` / `.+` 通配");
    }
    if pat.contains("\\d") || pat.contains("\\w") || pat.contains("\\s") || pat.contains("\\b") {
        return Some("`\\d`/`\\w`/`\\s` 转义类");
    }
    if pat.contains("\\[") || pat.contains("\\(") || pat.contains("\\]") || pat.contains("\\)") {
        return Some("转义括号");
    }
    if pat.starts_with('^') || pat.ends_with('$') {
        return Some("`^`/`$` 锚点");
    }
    if pat.contains('[') && pat.contains(']') {
        return Some("`[...]` 字符类");
    }
    None
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
///
/// ⚠️ 2026-09-30 起实现搬到 [`crate::recovery`]：原来的版本只把副本命名为
/// `{时间戳}__{文件名}`，**不记原路径** —— 于是 210 个备份都还原不回去
/// （谁知道那个 `README.md` 原来在哪个目录）。现在每次都写
/// `backups.jsonl`（原路径 / 时间 / 大小），设置页也能列出并一键还原。
fn backup_file(data_dir: &Path, path: &Path) -> Result<String, String> {
    match crate::recovery::backup_file(data_dir, path)? {
        Some(rec) => Ok(format!(
            "\n（原文件已备份到 {}，可从设置 › 备份与回收站还原）",
            rec.backup
        )),
        None => Ok(String::new()),
    }
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

// ---------------------------------------------------------------------------
// move / copy / mkdir / file_info / append / git
// ---------------------------------------------------------------------------

fn move_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let src_raw = get_str(args, "src")?;
    let dst_raw = get_str(args, "dst")?;
    let overwrite = args
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let src = authorize(ctx, &src_raw, Access::ReadWrite)?;
    let dst = authorize(ctx, &dst_raw, Access::ReadWrite)?;

    if !src.exists() {
        return Err(format!("源不存在：{}", src.display()));
    }
    if dst.exists() {
        if !overwrite {
            return Err(format!(
                "目标已存在：{}。确认要覆盖请带 overwrite=true。",
                dst.display()
            ));
        }
        if dst.is_file() {
            let _ = backup_file(ctx.data_dir, &dst)?;
        }
        if dst.is_dir() && !dst.read_dir().map(|mut d| d.next().is_none()).unwrap_or(false) {
            return Err(format!(
                "目标是非空目录：{}。请换目标路径或自行清理。",
                dst.display()
            ));
        }
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目标父目录失败: {e}"))?;
    }

    let same = src == dst;
    if same {
        return Ok(ToolOutput::text("源与目标相同，未移动。"));
    }

    match std::fs::rename(&src, &dst) {
        Ok(()) => {}
        Err(e) => {
            // 跨盘 rename 可能失败 → 文件退回 copy+delete
            if src.is_file() {
                if std::fs::copy(&src, &dst).is_ok() {
                    let _ = std::fs::remove_file(&src);
                } else {
                    return Err(format!("移动失败: {e}"));
                }
            } else {
                return Err(format!("移动失败: {e}"));
            }
        }
    }

    Ok(ToolOutput::text(format!(
        "已移动\n  {} → {}",
        src.display(),
        dst.display()
    )))
}

fn copy_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let src_raw = get_str(args, "src")?;
    let dst_raw = get_str(args, "dst")?;
    let overwrite = args
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let src = authorize(ctx, &src_raw, Access::Read)?;
    let dst = authorize(ctx, &dst_raw, Access::ReadWrite)?;

    if !src.is_file() {
        return Err(format!(
            "源不是文件：{}。目录请 glob/list 后对每个文件调用 copy_file。",
            src.display()
        ));
    }
    if dst.exists() && !overwrite {
        return Err(format!(
            "目标已存在：{}。确认要覆盖请带 overwrite=true。",
            dst.display()
        ));
    }
    if dst.is_file() && overwrite {
        let _ = backup_file(ctx.data_dir, &dst)?;
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目标父目录失败: {e}"))?;
    }

    let n = std::fs::copy(&src, &dst).map_err(|e| format!("复制失败: {e}"))?;
    Ok(ToolOutput::text(format!(
        "已复制 {} → {}（{n} 字节）",
        src.display(),
        dst.display()
    )))
}

fn mkdir(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe = authorize(ctx, &raw, Access::ReadWrite)?;
    if safe.exists() {
        if safe.is_dir() {
            return Ok(ToolOutput::text(format!("目录已存在：{}", safe.display())));
        }
        return Err(format!("路径已存在且是文件：{}", safe.display()));
    }
    std::fs::create_dir_all(&safe).map_err(|e| format!("建目录失败: {e}"))?;
    Ok(ToolOutput::text(format!("已创建目录 {}", safe.display())))
}

fn file_info(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let safe = authorize(ctx, &raw, Access::Read)?;

    if !safe.exists() {
        return Ok(ToolOutput::text(format!("不存在：{}", safe.display())));
    }
    let md = safe.metadata().map_err(|e| format!("读元信息失败: {e}"))?;
    let is_dir = md.is_dir();
    let len = md.len();
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(ToolOutput::text(format!(
        "path: {}\nexists: true\nis_dir: {}\nsize: {} bytes\nmtime_unix: {}",
        safe.display(),
        is_dir,
        len,
        mtime
    )))
}

fn append_file(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let raw = get_str(args, "path")?;
    let content = get_str(args, "content")?;
    let safe = authorize(ctx, &raw, Access::ReadWrite)?;

    if safe.is_dir() {
        return Err(format!("{} 是目录，不能 append", safe.display()));
    }
    if safe.exists() {
        let _ = backup_file(ctx.data_dir, &safe)?;
    }
    if let Some(parent) = safe.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建父目录失败: {e}"))?;
    }

    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&safe)
        .map_err(|e| format!("打开文件失败: {e}"))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("追加失败: {e}"))?;

    Ok(ToolOutput::text(format!(
        "已追加 {} 字节到 {}",
        content.len(),
        safe.display()
    )))
}

/// Git 只读查询。写子命令明确拒绝，避免静默绕过 `command_policy`。
fn git_tool(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let sub = get_str(args, "subcommand")?;
    let allowed = ["status", "diff", "log", "show", "branch"];
    if !allowed.contains(&sub.as_str()) {
        return Err(format!(
            "git 只读子命令仅限 {allowed:?}。写操作（add/commit/push 等）请用 run_command 并接受命令策略。"
        ));
    }

    let repo_raw = args
        .get("repo")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.data_dir.to_string_lossy().into_owned());
    let repo = authorize(ctx, &repo_raw, Access::Read)?;

    let extra = args
        .get("args")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(|x| x.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();

    let mut cmd = std::process::Command::new("git");
    cmd.arg(&sub).current_dir(&repo);

    // 只读白名单：拒绝一切可能写工作区/输出到文件/调外部 diff 的 flag。
    // 自由 args 无法用「黑名单」保证只读，这里用**前缀/子串危险特征**收紧。
    for a in &extra {
        let lower = a.to_ascii_lowercase();
        let dangerous = matches!(
            lower.as_str(),
            "-m"
                | "--amend"
                | "--force"
                | "-f"
                | "--hard"
                | "--cached"
                | "-d"
                | "-D"
                | "--delete"
                | "--move"
                | "-M"
                | "--set-upstream-to"
                | "--edit-description"
                | "-e"
                | "--ext-diff"
                | "--no-index"
                | "--exit-code"
                | "--output"
        ) || lower.starts_with("--output=")
            || lower.starts_with("--ext-diff")
            || lower.starts_with("-o")
            || lower.starts_with("--exec=")
            // branch 的 -m/-d 等短选项夹在字母里也拒
            || (sub == "branch" && (lower == "-d" || lower == "-D" || lower == "-m" || lower == "-M"));
        if dangerous {
            return Err(format!(
                "git {sub} 的附加参数 `{a}` 可能产生写操作或外部副作用，已拒绝。只读查询请用更简单参数。"
            ));
        }
        cmd.arg(a);
    }
    // branch 只允许 -v/-vv 列表；其余 branch 形态不透传
    if sub == "branch" {
        if extra.iter().any(|x| {
            let l = x.to_ascii_lowercase();
            !(l == "-v" || l == "-vv" || l.starts_with("--list") || l == "--contains" || l.starts_with("--contains="))
        }) {
            return Err("git branch 仅支持只读列出（-v/--list）。改名/删除请用 run_command。".into());
        }
        if !extra.iter().any(|x| x == "-v" || x == "-vv") {
            cmd.arg("-v");
        }
    }

    let out = cmd
        .output()
        .map_err(|e| format!("启动 git 失败（是否安装了 git？）: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let mut body = if !stdout.is_empty() { stdout } else { stderr };
    const GIT_LIMIT: usize = 32 * 1024;
    if body.chars().count() > GIT_LIMIT {
        // 按字符截断，避免 UTF-8 边界 panic
        body = body.chars().take(GIT_LIMIT).collect();
        body.push_str("\n… [输出已截断]");
    }
    if !out.status.success() {
        return Err(format!("git {sub} 退出码 {:?}\n{body}", out.status.code()));
    }
    Ok(ToolOutput::text(format!("$ git {sub} {}\n{body}", extra.join(" "))))
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
         还原：设置 › 备份与回收站 → 回收站，点「还原」即可（记录见 {}）。\n\
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

    // ---- 校验 3：这条申请**刚被拒过**（含同 parent/更宽 access）→ 打回 ----
    //
    // 2026-09-23：同目录连续写曾连环弹卡。现在同 parent + 同/更宽 access 也拦，
    // 并提示模型**申请父目录 + task**（而不是单文件 once）。
    if let Some(m) = ctx.perm.find_denied(ctx.session_id, &canon, access) {
        let why = if m.reason.is_empty() {
            "（用户上次没有说明理由）".to_string()
        } else {
            format!("用户上次给的理由：{}", m.reason)
        };
        return Ok(ToolOutput::text(format!(
            "{} 的这条申请（{}）刚刚已经被拒绝过了（或落在已拒范围 {} 内），**不会再弹第二次卡**，\n\
             请不要原样重复申请。\n{}\n\n\
             若确需继续：请按理由换更窄路径、换做法；\
             或一次性申请**公共父目录 + tier=task**（不要单文件/once 连环申请）。",
            canon.display(),
            access.as_str(),
            m.prefix.display(),
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
            eprintln!("[orbcat] ⚠️ 授权落盘失败（本次仍有效，重启后失效）: {e}");
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

/// `fetch_url` 执行体：GET → 剥壳 → Markdown 子集 → 截断
async fn fetch_url(args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let url = get_str(args, "url")?;
    let max = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(crate::fetch::DEFAULT_MAX_CHARS);
    let text = crate::fetch::fetch_and_extract(&url, max).await?;
    Ok(ToolOutput::text(text))
}

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
/// `ask_user`：向用户提一个问题并阻塞等回答（2026-09-25 用户要求加的工具）。
///
/// 通道复用权限申请的「弹卡等回答」机制（[`PermHub::ask_user`]）：
/// 张问题卡 → agent loop 阻塞在这里 → 用户打字/点选项 → 唤醒 → 回答进 tool 结果。
///
/// 返回给模型的语义：
/// - 有回答 → `用户回答：<原文>`
/// - 用户跳过 → 明确说"用户选择跳过"，让模型自己决定兜底
/// - 超时（10 分钟）→ 明确说"用户没有回答"，模型**不要干等**，自行降级
async fn ask_user(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let question = get_str(args, "question")?;
    if question.trim().is_empty() {
        return Err("question 不能为空".into());
    }
    let options: Vec<String> = args
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let hint = args
        .get("hint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let verdict = ctx
        .perm
        .ask_user(question, options, hint, ctx.session_id)
        .await?;

    use crate::perm_request::Decision;
    let msg = match verdict.decision {
        // 回答文本走 reason 通道（perm_request_decide 把输入框内容放这里）
        d if d == Decision::Approve => {
            let a = verdict.reason.trim();
            if a.is_empty() {
                "用户提交了回答但内容为空。请按最保守的理解继续。".to_string()
            } else {
                format!("用户回答：{a}")
            }
        }
        Decision::Timeout => "用户没有在时限内回答这个问题。请不要等待，按最合理的默认继续。".to_string(),
        d if d == Decision::Deny && crate::perm_request::is_system_cancel(&verdict.reason) => {
            return Err("已停止".into());
        }
        _ => {
            let why = verdict.reason.trim();
            if why.is_empty() {
                "用户选择跳过这个问题。请按最合理的默认继续。".to_string()
            } else {
                format!("用户跳过了这个问题。用户留言：{why}。请据此调整。")
            }
        }
    };
    Ok(ToolOutput::text(msg))
}

/// 命令闸门的判定结果。
///
/// 抽出来是为了让 **`run_command` 与项目 L1 脚本工具走完全相同的一套判定** ——
/// 各写一份的结果是"两个入口的权限语义慢慢分叉"，而分叉的那一侧
/// 就是安全漏洞（通常是后来加的那个忘了补某个检查）。
enum CmdGate {
    /// 放行。`source` 用于审计（白名单 / 免问 / 已授权 / 用户批准）
    Allowed {
        source: String,
        risk: command_policy::Risk,
        norm: command_policy::Normalized,
    },
    /// 不执行 —— 这段文本直接回给模型（硬阻断 / 用户拒绝 / 防骚扰）
    Refused(ToolOutput),
}

/// **命令执行的唯一闸门**（Gate 2 全流程）。
///
/// 覆盖：硬阻断 → 白名单 → 临时授权 → 防骚扰 → 执行档位 → 弹卡。
/// 顺序是**有意的**，不要调整：
/// - 硬阻断最前：任何档位、任何项目预设都拦
/// - 防骚扰在档位之前：用户刚拒过的命令，切档位也不该偷偷放行
///
/// 调用方负责 Gate 1（`cwd` 过文件权限网关）与真正的进程启动。
async fn gate_command(
    cmd: &str,
    cwd: &Path,
    ctx: &ToolCtx<'_>,
) -> Result<CmdGate, String> {
    // ⚠️ 用 `project::effective_policy` 而**不是** `load_policy`：
    // 拼装项目可以在白名单上做**加法**（`permPreset.commandAllowExtra`）。
    // `hard_block` 由该函数原样保留，且 `PermPreset::validated` 在装配阶段
    // 已经拒绝过试图命中硬阻断的白名单条目 —— 两层保证，改任何一层都破不了防。
    let policy = crate::project::effective_policy(ctx.data_dir);
    let norm = command_policy::normalize(cmd);

    match command_policy::evaluate(&policy, cmd) {
        CmdVerdict::HardBlock { reason } => {
            audit_cmd(
                ctx, cmd, &norm, cwd, "blocked", "hard-block", command_policy::Risk::High, None,
            );
            Err(format!(
                "命令被**硬性阻断**（不可申请）：{reason}\n\
                 这条命令命中危险模式，永远不会执行 —— 不要重试，也不要换个写法绕过。\n\
                 如确有正当需要，请让用户**手工**执行。"
            ))
        }
        CmdVerdict::Allow { reason } => {
            let risk = command_policy::Risk::Low;
            audit_cmd(ctx, cmd, &norm, cwd, "allowed", "whitelist", risk, None);
            Ok(CmdGate::Allowed {
                source: format!("白名单（{reason}）"),
                risk,
                norm,
            })
        }
        CmdVerdict::Ask { reason, risk } => {
            // ① 已有临时授权（Once/Turn/Task 三档）→ 消费一次直接放行
            let hit = {
                let mut store = ctx
                    .perm
                    .cmd_grants()
                    .lock()
                    .map_err(|e| format!("命令授权表锁失败: {e}"))?;
                match store.find(cmd) {
                    Some(i) => {
                        store.consume(i);
                        true
                    }
                    None => false,
                }
            };

            if hit {
                audit_cmd(ctx, cmd, &norm, cwd, "allowed", "grant", risk, None);
                return Ok(CmdGate::Allowed {
                    source: "已授权（临时授权生效）".into(),
                    risk,
                    norm,
                });
            }

            // ② 防骚扰：这条命令刚被拒过 → 不再弹卡，直接把理由回给模型
            let full_fp = norm.full();
            if let Some(m) = ctx.perm.find_denied_cmd(ctx.session_id, &full_fp) {
                let why = if m.reason.is_empty() {
                    "（用户上次没有说明理由）".to_string()
                } else {
                    format!("用户上次给的理由：{}", m.reason)
                };
                return Ok(CmdGate::Refused(ToolOutput::text(format!(
                    "这条命令刚刚已经被拒绝过了，**不会重复弹卡**。\n{why}\n\n\
                     请按这个理由调整：换一种做法，或如实告诉用户你做不了。"
                ))));
            }

            // ③ 执行权限档位（smart/full 档免问直接跑）。
            //    ⚠️ 放在防骚扰**之后** —— 用户刚拒过的命令，切档位也不该偷偷放行。
            //    取 `project::effective_exec_trust`：拼装项目可**预选**档位
            //    （不是绕过 —— 用户自己本来就能开 full）。
            let trust = crate::project::effective_exec_trust(ctx.data_dir);
            if trust.auto_allows_command(risk == command_policy::Risk::High) {
                let source = format!(
                    "免问放行（{}档）",
                    match trust {
                        crate::config::ExecTrust::Smart => "智能",
                        _ => "完全访问",
                    }
                );
                audit_cmd(ctx, cmd, &norm, cwd, "allowed", "trust-auto", risk, None);
                return Ok(CmdGate::Allowed { source, risk, norm });
            }

            // ④ 弹卡等用户当场拍板
            let passed_head = norm.head();
            let passed_full = norm.full();
            let verdict = ctx
                .perm
                .ask_command(
                    cmd.to_string(),
                    passed_full.clone(),
                    passed_head,
                    passed_full,
                    risk,
                    reason.clone(),
                    ctx.session_id,
                )
                .await?;

            if !verdict.is_approved() {
                audit_cmd(ctx, cmd, &norm, cwd, "rejected", "user-denied", risk, None);
                return Ok(CmdGate::Refused(ToolOutput::text(verdict.to_model_message())));
            }

            let applied = ctx.perm.apply_cmd_decision(verdict.decision, cmd, &reason);
            let Some(g) = applied else {
                return Ok(CmdGate::Refused(ToolOutput::text(
                    "用户已批准，但命令授权写入失败，请重试一次。",
                )));
            };
            let scope_label = match g.scope {
                CmdScope::Prefix => "记住这类命令",
                CmdScope::Full => "记住完整命令",
            };
            let source = format!("用户已批准（{scope_label}）");
            audit_cmd(ctx, cmd, &norm, cwd, "approved", "user-approved", risk, None);
            Ok(CmdGate::Allowed { source, risk, norm })
        }
    }
}

async fn run_command(args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput, String> {
    let cmd = get_str(args, "command")?;
    if cmd.trim().is_empty() {
        return Err("command 不能为空".into());
    }

    // ---- Gate 1：cwd 过文件路径网关 ----
    let cwd = resolve_command_cwd(args, ctx)?;

    // 默认 600s（10 分钟）：**构建软件**这件事很容易超过 5 分钟 —— 本项目自己的
    // `cargo build --release` 就跑了 5m24s，用户实报「build 4-5 分钟不输出很正常」。
    // 以前默认 300s 会刚好卡在 5 分钟把 build 掐死，而模型又经常不显式传 timeout_secs。
    //
    // 硬上限提到 3600s（1 小时）：整库 release 构建、大型依赖安装都放得下。
    // 提高默认值的代价很小 —— 命令跑飞了有 3 条路兜住：
    //   ① 用户点「停止」立刻 kill_tree（见 shell::CancelFn，命令执行期间也响应）；
    //   ② 每 2s 一次心跳，"已运行 N 秒" 一直涨，看得出它在动还是死了；
    //   ③ 轮内已增量落盘，进程被杀也不丢这一轮。
    // ⚠️ 安全相关的另有硬闸：命令策略的危险命令硬阻断、`-EncodedCommand` 硬阻断
    //    都在**启动之前**判，跟超时长短无关。
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .clamp(1, 3600);

    // ---- Gate 2：命令策略（唯一闸门，见 `gate_command`）----
    //
    // 与项目 L1 脚本工具**共用同一套判定** —— 不各写一份。
    let (source, risk, norm) = match gate_command(&cmd, &cwd, ctx).await? {
        CmdGate::Allowed { source, risk, norm } => (source, risk, norm),
        CmdGate::Refused(out) => return Ok(out),
    };
    // ---- 执行 ----
    //
    // 两条路，取决于「后台桌面」开关（`win32desk::is_enabled`）：
    //   · 普通：用户桌面上跑，有心跳 + 取消（默认，行为与以前完全一致）
    //   · 后台：隐形桌面上跑 —— 应用自己弹的窗口不会闪到用户屏幕上、不抢焦点
    //
    // 为什么用进程级开关而不是 agent 模式：后台执行管的是"在哪儿跑"，
    // 与模式管的"哪些工具可见"是正交的两个维度（用户拍板）。
    #[cfg(windows)]
    let use_bg_desk = crate::win32desk::is_enabled();
    #[cfg(not(windows))]
    let use_bg_desk = false;

    let out = if use_bg_desk {
        eprintln!("[orbcat] run_command 走隐形桌面：{}", cmd.chars().take(80).collect::<String>());
        match crate::shell::run_on_desktop(&cmd, &cwd, timeout_secs).await {
            Ok(o) => o,
            Err(e) => {
                audit_cmd(ctx, &cmd, &norm, &cwd, "error", &source, risk, None);
                return Err(format!("后台桌面执行失败：{e}"));
            }
        }
    } else {
        let tick = ctx.tick.clone();
        let cancel = ctx.cancel.clone();
        match crate::shell::run_powershell_tick(&cmd, &cwd, timeout_secs, tick, cancel).await {
            Ok(o) => o,
            Err(e) => {
                audit_cmd(ctx, &cmd, &norm, &cwd, "error", &source, risk, None);
                return Err(format!("执行失败：{e}"));
            }
        }
    };

    let event = if out.cancelled {
        "cancelled"
    } else if out.timed_out {
        "timeout"
    } else {
        "executed"
    };
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
        eprintln!("[orbcat] ⚠️ 写命令审计失败: {e}");
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

    if out.cancelled {
        s.push_str(
            "\n⚠️ **命令被用户「停止」掐断（连同子进程）。**\n\
             以下输出是掐断前已收到的部分。若用户仍需要这个结果，请告知后重试。\n",
        );
        return s;
    }

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
    use crate::mcp::{McpTool, ToolRegistry};
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
                tick: None,
                cancel: None,
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
                tick: None,
                cancel: None,
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
        // 能力发现入口：必须在（否则模型只能靠猜组名做发现，见 mcp.rs 的索引注释）
        assert!(names.contains(&"search_tools"), "缺 search_tools：能力发现不能退化回猜测");
    }

    #[test]
    fn specs_include_loaded_mcp_tools_only() {
        let tmp = std::env::temp_dir().join(format!("orbcat_specs_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let before = tool_specs(Some(&mcp.try_lock().unwrap()), false, &tmp, None).len();
        // 初始：内置 + fetch_url（无 MCP、无项目）
        assert_eq!(before, builtin_specs().len() + 1, "初始 = 内置 + fetch_url");
        assert!(tool_specs(None, false, &tmp, None).iter().any(|s| s.name == "fetch_url"));
        let with = tool_specs(Some(&mcp.try_lock().unwrap()), true, &tmp, None).len();
        assert_eq!(with, builtin_specs().len() + 2, "应有 fetch_url + web_search");
        assert!(tool_specs(None, false, &tmp, None).iter().all(|s| s.name != "web_search"));
        assert!(tool_specs(None, true, &tmp, None).iter().any(|s| s.name == "web_search"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 内置工具名清单必须与**实际可调用的内置工具**完全一致。
    ///
    /// `BUILTIN_TOOL_NAMES` 是 `project.rs` 用来拒绝"项目脚本工具撞名"的依据。
    /// 漏一个的后果很具体：项目可以定义一个与内置同名的工具，
    /// 而 `execute` 的 `match` 会先命中内置分支 —— 用户的脚本**永远不执行**，
    /// 模型只看到"我调了但没反应"。
    ///
    /// ⚠️ 注意 `fetch_url` 与 `web_search` **不在** `builtin_specs()` 里：
    /// 前者由 `tool_specs` 无条件追加，后者按配置追加。但它们都是
    /// `execute` 里真实存在的分支，所以必须在这份清单里。
    #[test]
    fn builtin_name_list_covers_every_spec() {
        let from_specs: std::collections::HashSet<String> = builtin_specs()
            .into_iter()
            .map(|s| s.name)
            // ⚠️ 这三个不在 `builtin_specs()` 里，但**仍是保留的内置工具名**：
            //   web_search / fetch_url 按配置条件注入；distill_move 只在
            //   「记忆蒸馏」会话注入（见 tool_specs_for_distill）。它们照样要进
            //   保留清单，否则项目脚本工具能起同名工具把它顶掉。
            .chain([
                "web_search".to_string(),
                "fetch_url".to_string(),
                "distill_move".to_string(),
                // 2026-10-08：只在 `imageReplies = "free"` 的模式里注入
                // （见 `all_tool_specs`），但**名字必须保留** —— 否则项目
                // 脚本工具能起个同名的，把这道闸门顶掉。
                "send_meme".to_string(),
            ])
            .collect();
        let listed: std::collections::HashSet<String> =
            BUILTIN_TOOL_NAMES.iter().map(|s| s.to_string()).collect();

        let missing: Vec<_> = from_specs.difference(&listed).collect();
        assert!(
            missing.is_empty(),
            "BUILTIN_TOOL_NAMES 漏了这些内置工具（会导致项目脚本可与之撞名）：{missing:?}"
        );
        let extra: Vec<_> = listed.difference(&from_specs).collect();
        assert!(
            extra.is_empty(),
            "BUILTIN_TOOL_NAMES 里有已不存在的工具（清单过期）：{extra:?}"
        );
    }

    /// `send_meme` 必须被模式**硬**闸住（用户 2026-10-08 要求）。
    ///
    /// 这条别退化成提示词：闸门的意义就是"模型想调也调不到"。
    /// 同时钉住另一半 —— `send_image` 常驻（工作模式也要贴截图 / 图表）。
    #[test]
    fn send_meme_spec_is_gated_by_mode() {
        let d = std::env::temp_dir().join(format!(
            "orbcat_memegate_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        std::fs::write(
            d.join("modes.json"),
            r#"{"modes":[
                 {"id":"standard","name":"标准","description":"x","mcpGroupsPreload":"all"},
                 {"id":"chat","name":"闲聊","description":"x","mcpGroupsPreload":[],
                  "imageReplies":"free"}
               ]}"#,
        )
        .unwrap();

        let names = |dir: &Path, mode: &str| -> Vec<String> {
            // 模式跟会话走（2026-10-08）：给这个模式**建一条会话**，按它的 id 取工具表。
            // ⚠️ 不能再写 `settings.json` 的 `activeMode` —— 那个字段已删，
            //    而且就算留着也不影响任何一条会话的模式（曾经靠它切模式的写法已全废）。
            let sid = crate::sessions::new_session(dir, mode).id;
            tool_specs(None, false, dir, Some(&sid))
                .into_iter()
                .map(|s| s.name)
                .collect()
        };

        // work（standard）：有 send_image，**没有** send_meme
        let work = names(&d, "standard");
        assert!(
            work.contains(&"send_image".to_string()),
            "send_image 必须常驻: {work:?}"
        );
        assert!(
            !work.contains(&"send_meme".to_string()),
            "work 模式不该出现 send_meme（这是硬闸门，不是提示词）: {work:?}"
        );

        // free（chat）：两个都在
        let chat = names(&d, "chat");
        assert!(chat.contains(&"send_image".to_string()));
        assert!(
            chat.contains(&"send_meme".to_string()),
            "free 模式必须有 send_meme: {chat:?}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 清单里的**每一个**名字都必须在 `execute` 里真的有分支。
    ///
    /// 反向检查：防止清单里混进一个"已经删掉的内置工具名"，
    /// 那会让项目工具被无理由地拒绝（用户看到"名字不合法"却不知道为什么）。
    #[test]
    fn builtin_names_are_all_real_dispatch_targets() {
        for name in BUILTIN_TOOL_NAMES {
            // MCP 元工具与项目工具走 `other` 分支，所以这里只断言
            // "内置清单里的名字都被 is_builtin_tool_name 认可"这个自洽性，
            // 真正的分派覆盖由 `builtin_name_list_covers_every_spec` 保证。
            assert!(
                is_builtin_tool_name(name),
                "清单自洽性坏了：{name}"
            );
        }
        // 反向：普通名字不该被当成内置
        assert!(!is_builtin_tool_name("my_project_tool"));
        assert!(!is_builtin_tool_name("mcp__github__create_issue"));
        assert!(!is_builtin_tool_name(""));
    }

    // ---------------- 项目 L1 脚本工具（端到端 + 注入穿透）----------------

    /// 造一个带 L1 脚本工具的项目，返回 `(data_dir, 项目目录)`。
    fn setup_project_tool(
        tag: &str,
        tool_name: &str,
        template: &[&str],
        script: Option<(&str, &str)>, // (相对路径, 内容)
    ) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "orbcat_ptool_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let bundle = crate::project::bundle_dir(&d, "p");
        std::fs::create_dir_all(&bundle).unwrap();
        let json = json!({
            "schemaVersion": 1,
            "name": "测试项目",
            "tools": [{
                "name": tool_name,
                "description": "测试脚本工具",
                "params": {"type":"object","properties":{"msg":{"type":"string"}}},
                "commandTemplate": template,
            }]
        });
        std::fs::write(
            bundle.join(crate::project::BUNDLE_FILE),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();

        if let Some((rel, content)) = script {
            let p = bundle.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }

        // 激活
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        // 测试要免问：否则会弹卡等到超时。这里验证的是**注入面**，
        // 不是权限卡（权限卡有它自己的测试）。
        s.exec_trust = crate::config::ExecTrust::Full;
        crate::config::save_settings(&d, &s).unwrap();

        d
    }

    /// **注入穿透测试：用户输入里的 shell 元字符必须原样留在同一个参数里。**
    ///
    /// 这是 L1 安全性的核心断言，而且是**真起子进程**的端到端验证 ——
    /// 光测 `expand_template` 只证明"字符串没被拆"，证明不了
    /// "从工具调用到子进程这一整条路都没有把参数交给 shell"。
    ///
    /// 做法：脚本把收到的每个参数**逐行原样打印**，然后断言：
    ///   ① 危险字符出现在输出里（说明脚本真的收到了它）
    ///   ② 脚本**没有被注入执行**（副作用标记文件不存在）
    #[tokio::test]
    async fn project_tool_does_not_interpret_shell_metacharacters() {
        let marker = std::env::temp_dir().join(format!(
            "orbcat_pwned_{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&marker);

        // 假脚本：把 argv 逐行打印。若参数被 shell 解析，`;` 之后的
        // 命令就会真的执行（下面那个 `New-Item` 会创建 marker）。
        let script_body = r#"
param([string]$msg)
[Console]::OutputEncoding=[System.Text.Encoding]::UTF8
Write-Output "ARG=$msg"
"#;
        let payload = format!(
            "safe; New-Item -Path '{}' -ItemType File -Force; echo",
            marker.to_string_lossy()
        );

        let d = setup_project_tool(
            "inject",
            "echo_tool",
            &["{exe}", "-NoProfile", "-NonInteractive", "-File", "{project}/tools/echo.ps1", "-msg", "{param.msg}"],
            Some(("tools/echo.ps1", script_body)),
        );
        // 模板里第 0 项用真实解释器路径
        let (exe, _) = crate::shell::resolve_interpreter();
        let bundle = crate::project::bundle_dir(&d, "p");
        let json = json!({
            "schemaVersion": 1,
            "name": "测试项目",
            "tools": [{
                "name": "echo_tool",
                "description": "回显参数",
                "params": {"type":"object","properties":{"msg":{"type":"string"}}},
                "commandTemplate": [exe, "-NoProfile", "-NonInteractive", "-File",
                                    "{project}/tools/echo.ps1", "-msg", "{param.msg}"],
            }]
        });
        std::fs::write(
            bundle.join(crate::project::BUNDLE_FILE),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();

        let gate = gate_for(&d);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let out = match execute(
            "echo_tool",
            &json!({ "msg": payload }),
            &ctx_for!(gate, &d, mcp),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                // 起不了进程（受限环境）→ 跳过，别误红
                eprintln!("跳过：{e}");
                let _ = std::fs::remove_dir_all(&d);
                return;
            }
        };

        // ① 危险字符**确实**到达了脚本（说明没有被静默丢弃/转义掉语义）
        assert!(
            out.text.contains("ARG="),
            "脚本应打印参数，实际输出: {}",
            out.text
        );
        assert!(
            out.text.contains("safe;"),
            "参数里的 `;` 应原样传给脚本（作为普通字符）: {}",
            out.text
        );

        // ② **关键**：注入没有生效 —— marker 文件不该被创建
        assert!(
            !marker.exists(),
            "注入生效了！`;` 之后的命令被执行，marker={} 被创建。\
             这说明参数在某处被交给了 shell 解析。",
            marker.display()
        );

        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **`{param.*}` 不会被再拆分** —— 含空格的单个参数仍是一个参数。
    ///
    /// 若被拆分，`--msg "a b"` 会变成两个参数，
    /// 用户就能用一个值伪造出额外开关（如 `x --dangerous-flag`）。
    #[tokio::test]
    async fn project_tool_keeps_spaced_param_as_one_argument() {
        let script_body = r#"
param([string]$msg)
[Console]::OutputEncoding=[System.Text.Encoding]::UTF8
Write-Output "COUNT_MARK"
Write-Output "ARG=[$msg]"
"#;
        let d = setup_project_tool(
            "spaces",
            "echo_tool",
            &["{exe}", "-NoProfile", "-File", "{project}/tools/echo.ps1", "-msg", "{param.msg}"],
            Some(("tools/echo.ps1", script_body)),
        );
        let (exe, _) = crate::shell::resolve_interpreter();
        let bundle = crate::project::bundle_dir(&d, "p");
        let json = json!({
            "schemaVersion": 1,
            "name": "测试项目",
            "tools": [{
                "name": "echo_tool",
                "description": "回显",
                "params": {"type":"object","properties":{"msg":{"type":"string"}}},
                "commandTemplate": [exe, "-NoProfile", "-File",
                                    "{project}/tools/echo.ps1", "-msg", "{param.msg}"],
            }]
        });
        std::fs::write(
            bundle.join(crate::project::BUNDLE_FILE),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();

        let gate = gate_for(&d);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let out = match execute(
            "echo_tool",
            &json!({ "msg": "带 空格 的 值" }),
            &ctx_for!(gate, &d, mcp),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("跳过：{e}");
                let _ = std::fs::remove_dir_all(&d);
                return;
            }
        };

        // 参数应被**完整**收到（带空格也只有一个值）
        assert!(
            out.text.contains("带 空格 的 值"),
            "含空格的参数应完整传给脚本（说明没被拆成多个）: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **硬阻断在项目工具上照样拦**（即使项目把 exec_trust 设成 full）。
    ///
    /// 这条是"拼装层在闸门之下"的端到端证明：
    /// 一个项目可以把档位预选成 full（免问），但硬阻断的命令仍然**不执行**。
    #[tokio::test]
    async fn project_tool_hard_block_still_applies_under_full_trust() {
        // 模板直接调用一个会被硬阻断的命令（shutdown 在硬阻断清单里）
        let d = setup_project_tool(
            "hardblock",
            "danger_tool",
            &["shutdown", "/s"],
            None,
        );

        let gate = gate_for(&d);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let r = execute(
            "danger_tool",
            &json!({}),
            &ctx_for!(gate, &d, mcp),
        )
        .await;

        // 必须是**错误**（硬阻断）—— 不能返回成功
        match r {
            Err(e) => {
                assert!(
                    e.contains("硬性阻断") || e.contains("硬阻断"),
                    "应是硬阻断错误，实际: {e}"
                );
            }
            Ok(o) => panic!(
                "硬阻断被绕过了！full 档 + 项目工具不该放行 shutdown。输出: {}",
                o.text
            ),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 项目工具**未激活时不可调用**（调用会走到 MCP 分支然后报未知工具）。
    #[tokio::test]
    async fn project_tool_not_callable_when_project_inactive() {
        let d = std::env::temp_dir().join(format!(
            "orbcat_ptool_inactive_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // 写包但**不激活**
        let bundle = crate::project::bundle_dir(&d, "p");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join(crate::project::BUNDLE_FILE),
            r#"{"schemaVersion":1,"name":"p","tools":[{"name":"ghost_tool","description":"x","commandTemplate":["echo","hi"]}]}"#,
        )
        .unwrap();

        let gate = gate_for(&d);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let r = execute("ghost_tool", &json!({}), &ctx_for!(gate, &d, mcp)).await;
        assert!(r.is_err(), "未激活的项目工具不该能调用");
        let e = r.unwrap_err();
        assert!(e.contains("未知工具"), "应报未知工具，实际: {e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = std::env::temp_dir().join("orbcat_tool_test");
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
        let tmp = std::env::temp_dir().join("orbcat_perm_test");
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
        let tmp = std::env::temp_dir().join("orbcat_list_test");
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
        let tmp = std::env::temp_dir().join("orbcat_mcp_reject_test");
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
            r.unwrap_err().contains("未启用"),
            "应提示该组未启用（默认全给后，未启用=用户关掉了）"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---------------- recall_turns ----------------

    #[tokio::test]
    async fn recall_turns_finds_earlier_conversation() {
        let tmp = std::env::temp_dir().join("orbcat_recall_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        // 造一个有历史的会话
        let s = crate::sessions::new_session(&tmp, "standard");
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

    // ---------------- recall_turns：跨会话（scope=all） ----------------

    /// 造一条**可控时间戳**的会话并落盘。
    ///
    /// 为什么要手搓时间戳而不用 `append_turn`（它只用"现在"）：跨会话检索的
    /// 核心是**按时间合并多个会话**，没有可控的时间就没法断言"倒序"这件事。
    fn write_session_at(
        dir: &Path,
        id: &str,
        title: &str,
        updated_at: u64,
        msgs: &[(&str, &str, u64)],
    ) {
        let s = crate::sessions::Session {
            id: id.to_string(),
            title: title.to_string(),
            created_at: updated_at,
            updated_at,
            messages: msgs
                .iter()
                .map(|(role, text, at)| crate::sessions::StoredMessage {
                    role: role.to_string(),
                    text: text.to_string(),
                    images: Vec::new(),
                    steps: Vec::new(),
                    reasoning: None,
                    interrupted: false,
                    partial: false,
                    steer_id: None,
                    model: None,
                    usage: None,
                    at: *at,
                })
                .collect(),
            kind: crate::sessions::kind::TASK.to_string(),
            forked_from: None,
            fork_at: None,
            summary: None,
            summary_upto: None,
            last_prompt_tokens: None,
            token_scale: None,
            mode: Some("standard".to_string()),
        };
        crate::sessions::save(dir, &s).unwrap();
    }

    /// 跨会话检索的**主不变量**：默认不跨、显式才跨，跨了必须带来源。
    ///
    /// 守住三件事，每一件都是"默认只查当前会话"这条设计决策的一部分：
    ///   ① 不传 scope 时，别的会话聊过的东西**一个字都不能带出来**
    ///      （否则等于默认就把别的话题灌进上下文）；
    ///   ② `scope=all` 时要能查到，且必须标出**是哪个会话**说的
    ///      （不标来源，模型会把别的会话的内容当成当前上下文）；
    ///   ③ 判定用**精确匹配**：文件名里带有 `scope=all` 只是为了让"误把
    ///      grants 文件当会话读"这类错误一眼可见，不代表断言依赖文件名。
    #[tokio::test]
    async fn recall_turns_scope_all_searches_other_sessions() {
        let tmp = fresh_tmp("recall_scope_all");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let now = crate::sessions::now_ms();

        write_session_at(
            &tmp,
            "cur1",
            "当前会话",
            now,
            &[("user", "今天聊聊天气", now - 60_000)],
        );
        write_session_at(
            &tmp,
            "old2",
            "编译加速那次的会话",
            now - 3_600_000,
            &[
                ("user", "cargo 构建要 8 分钟太慢了", now - 3_600_000),
                (
                    "assistant",
                    "真相在这里：开了 sccache 之后构建降到 90 秒",
                    now - 3_590_000,
                ),
            ],
        );

        // ① 默认（scope=session）：别的会话的内容一个字都不该出现。
        //
        // ⚠️ 断言盯的是**别的会话正文里的那句话**，不是关键词本身：
        //    `recall_in_session` 的"没找到"提示会把关键词原样回显
        //    （`（关键词：sccache）`），拿关键词当"泄漏探测器"会误报。
        let out = execute(
            "recall_turns",
            &json!({ "query": "sccache" }),
            &ctx_with_session!(gate, &tmp, mcp, "cur1"),
        )
        .await
        .unwrap();
        assert!(
            !out.text.contains("真相在这里"),
            "默认不该跨会话（别的会话的正文泄漏了）: {}",
            out.text
        );
        assert!(
            out.text.contains("scope=all"),
            "当前会话没命中时要提示还能跨会话查（否则模型不知道有这个能力）: {}",
            out.text
        );

        // ② scope=all：能查到，且带来源标识。
        //    断言盯的是**别的会话正文里的那句话**（不是关键词 —— 关键词在查询里
        //    本来就出现过，拿它当命中证据等于什么都没测）。
        let out = execute(
            "recall_turns",
            &json!({ "query": "sccache", "scope": "all" }),
            &ctx_with_session!(gate, &tmp, mcp, "cur1"),
        )
        .await
        .unwrap();
        assert!(
            out.text.contains("真相在这里"),
            "scope=all 应跨会话命中别的会话的正文: {}",
            out.text
        );
        assert!(
            out.text.contains("编译加速那次的会话"),
            "必须标出来自哪个会话: {}",
            out.text
        );
        assert!(
            out.text.contains("来自其他会话"),
            "必须提醒模型那些内容不属于当前上下文: {}",
            out.text
        );
        // 命中只在别的会话里 —— 当前会话的"天气"不该被无关地捎带出来
        assert!(!out.text.contains("天气"), "不该带出无关内容: {}", out.text);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 跨会话结果必须按**时间倒序**（跨会话合并后最容易被写成"按文件顺序"）。
    #[tokio::test]
    async fn recall_turns_scope_all_orders_hits_by_time_desc() {
        let tmp = fresh_tmp("recall_scope_order");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let now = crate::sessions::now_ms();

        write_session_at(
            &tmp,
            "newA",
            "最近的会话",
            now - 60_000,
            &[("user", "锚点词 新", now - 60_000)],
        );
        write_session_at(
            &tmp,
            "oldB",
            "很久以前的会话",
            now - 86_400_000,
            &[("user", "锚点词 旧", now - 86_400_000)],
        );

        let out = execute(
            "recall_turns",
            &json!({ "query": "锚点词", "scope": "all" }),
            &ctx_with_session!(gate, &tmp, mcp, "newA"),
        )
        .await
        .unwrap();
        let new_at = out.text.find("锚点词 新").expect("应命中新会话");
        let old_at = out.text.find("锚点词 旧").expect("应命中旧会话");
        assert!(
            new_at < old_at,
            "跨会话结果必须按时间倒序（新的在前）: {}",
            out.text
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 扫描上限：字节预算耗尽时**如实说"没查完"**，不能报成"没这回事"。
    ///
    /// 这是上限存在的意义所在：宁可说"还有更早的会话没查"，
    /// 也不能让模型拿着半截扫描结果对用户打包票。
    #[test]
    fn cross_session_scan_reports_truncation_when_budget_exhausted() {
        let tmp = fresh_tmp("recall_scan_budget");
        let now = crate::sessions::now_ms();
        write_session_at(&tmp, "s1", "会话一", now, &[("user", "内容", now - 1000)]);

        // 预算给 0 字节 → 一个会话都读不了，且必须标成 truncated
        let scan = crate::history::search_all_sessions(&tmp, "内容", 10, None, now, 40, 0);
        assert!(scan.hits.is_empty(), "预算为 0 时不该有命中");
        assert_eq!(scan.scanned, 0, "预算为 0 时一个文件都不该读");
        assert!(scan.truncated, "预算耗尽必须标成 truncated（还有没查的）");

        // 文件数上限为 1、但有 2 个会话 → 同样算 truncated
        write_session_at(&tmp, "s2", "会话二", now, &[("user", "内容", now - 1000)]);
        let scan =
            crate::history::search_all_sessions(&tmp, "内容", 10, None, now, 1, u64::MAX);
        assert_eq!(scan.scanned, 1, "文件数上限应生效");
        assert!(scan.truncated, "目录里还有会话没扫，必须标成 truncated");

        // 上限足够时不该谎报 truncated
        let scan =
            crate::history::search_all_sessions(&tmp, "内容", 10, None, now, 40, u64::MAX);
        assert_eq!(scan.scanned, 2, "上限足够时应扫完全部会话");
        assert!(!scan.truncated, "没超限不该标 truncated");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 跨会话扫描**不能把 `*.grants.json` 当会话读**。
    ///
    /// `sessions/<id>.grants.json` 与会话平级存放，里面是临时授权。
    /// 复用 `sessions::list()` 就自动躲开了这个坑（见 `is_grants_file`）；
    /// 这个测试是钉住那份复用的 —— 以后谁改成自己 read_dir，
    /// grants 文件会混进来（最多是解析失败被跳过，于是错误悄无声息）。
    #[test]
    fn cross_session_scan_skips_grants_files() {
        let tmp = fresh_tmp("recall_scan_grants");
        let now = crate::sessions::now_ms();
        write_session_at(&tmp, "s1", "真会话", now, &[("user", "独特词zzz", now - 1000)]);

        // 平级放一个 grants 文件（名字与会话同构，只有后缀不同）
        let g = crate::sessions::sessions_dir(&tmp).join("s1.grants.json");
        std::fs::write(&g, "{\"grants\":[]}").unwrap();

        let scan = crate::history::search_all_sessions(&tmp, "", 10, None, now, 40, u64::MAX);
        assert_eq!(scan.scanned, 1, "grants 文件不该被当成会话扫描");
        assert_eq!(scan.hits.len(), 1, "只该有真会话里的那一条");
        assert!(scan.hits[0].hit.text.contains("独特词zzz"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 跨会话检索必须走**文件权限网关** —— 没权限时要拒绝，而不是裸读。
    ///
    /// 同 `load_skill` 的纪律：`sessions/` 在 `agent-data/` 内，读它和读别处
    /// 是同一种行为，不能因为"是程序自己的目录"就留一个不检查的后门
    /// （用户可能把数据目录挪走、或从权限规则里删掉它）。
    #[tokio::test]
    async fn recall_turns_scope_all_needs_read_permission() {
        // ⚠️ 这里**不用** `gate_for(&tmp)`：那条规则把 tmp 整个授了 Full，
        //    连它的父目录都够不着，测不出"没权限"的情形。
        let tmp = fresh_tmp("recall_scope_perm");
        let data = tmp.join("data");
        std::fs::create_dir_all(&data).unwrap();

        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        // ① data 目录**没有任何规则** → 必须被拒
        let gate = PermissionGate::new();
        let r = execute(
            "recall_turns",
            &json!({ "query": "x", "scope": "all" }),
            &ctx_with_session!(gate, &data, mcp, "sid"),
        )
        .await;
        let err = r.expect_err("无权限时必须拒绝跨会话检索");
        assert!(
            err.contains("无读取权限"),
            "拒绝理由要能读懂（还要能引导模型申请权限）: {err}"
        );

        // ② 只给 data 目录 Read → 放行（scope=all 只需要读）
        let mut gate2 = PermissionGate::new();
        gate2.add_rule(&data, Access::Read, "test");
        let out = execute(
            "recall_turns",
            &json!({ "query": "x", "scope": "all" }),
            &ctx_with_session!(gate2, &data, mcp, "sid"),
        )
        .await
        .expect("只读权限足够跨会话检索");
        assert!(out.text.contains("没找到"), "空库应友好提示: {}", out.text);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 工具 schema 里 `scope` 必须存在、默认值必须是 `session`、且向模型说清了要显式选 all。
    ///
    /// 为什么把"默认"当不变量测：`scope` 的默认值是**安全属性**而不是功能属性 ——
    /// 默认翻成 `all` 会让每次 recall_turns 都去遍历整个会话目录并把别的话题
    /// 灌进上下文。schema 是模型唯一的说明书，写错了就等于默认值错了。
    #[test]
    fn recall_turns_spec_exposes_scope_with_session_default() {
        let spec = builtin_specs()
            .into_iter()
            .find(|s| s.name == "recall_turns")
            .expect("缺 recall_turns");
        let props = &spec.parameters["properties"];
        let scope = &props["scope"];
        assert!(!scope.is_null(), "schema 必须暴露 scope: {}", spec.parameters);
        let enums: Vec<&str> = scope["enum"]
            .as_array()
            .expect("scope 应是枚举")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(enums, vec!["session", "all"], "scope 取值只能是这两个");
        assert!(
            scope["description"]
                .as_str()
                .unwrap_or("")
                .contains("默认"),
            "schema 必须说明默认值（否则模型不知道不传 scope 是什么行为）"
        );
        assert!(
            spec.description.contains("scope=all"),
            "工具描述里必须提到跨会话能力，否则工具等于摆设: {}",
            spec.description
        );
        // scope 不能是必填 —— 必填会破坏"旧调用照样能用"
        let required = spec.parameters["required"].as_array().cloned().unwrap_or_default();
        assert!(
            !required.iter().any(|v| v.as_str() == Some("scope")),
            "scope 必须可选（保持向后兼容）"
        );
    }

    #[test]
    fn edit_and_delete_are_in_builtin_specs() {
        let names: Vec<String> = builtin_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"edit_file".to_string()), "缺 edit_file");
        assert!(names.contains(&"delete_file".to_string()), "缺 delete_file");
    }

    #[test]
    fn view_image_is_in_builtin_specs_and_is_image_path() {
        let names: Vec<String> = builtin_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"view_image".to_string()), "缺 view_image");
        assert!(crate::images::is_image_path(std::path::Path::new("a/b.PNG")));
        assert!(crate::images::is_image_path(std::path::Path::new("x.jpeg")));
        assert!(!crate::images::is_image_path(std::path::Path::new("a.pdf")));
        assert!(!crate::images::is_image_path(std::path::Path::new("a.txt")));
    }

    #[test]
    fn builtin_specs_contains_core_tools() {
        let names: Vec<String> = builtin_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"load_skill".to_string()), "缺 load_skill");
        assert!(names.contains(&"search_tools".to_string()), "缺 search_tools");
    }

    // -----------------------------------------------------------------------
    // grep_files 的「静默失败」回归（2026-10-01）
    //
    // 背景：543 次真实调用里 58% 报零命中，其中 145 次是 `path` 传了文件 ——
    // walk() 对文件 read_dir 必失败后裸 return，工具报「命中 0 行」，
    // 模型无从区分「真的没有」和「压根没读」，于是放弃工具改跑 PowerShell。
    // -----------------------------------------------------------------------

    #[test]
    fn regex_looking_pattern_flags_real_regex_syntax() {
        // 实测模型真写过的那些（32 次零命中的形态）
        assert!(regex_looking_pattern("mimo-server-cn|/api/route/chat").is_some());
        assert!(regex_looking_pattern("current.*source").is_some());
        assert!(regex_looking_pattern("8883|10558|llm-server").is_some());
        assert!(regex_looking_pattern("\\[mimo\\]\\[api\\]").is_some());
        assert!(regex_looking_pattern("^=====").is_some());
        assert!(regex_looking_pattern("fn \\w+").is_some());
        // 普通子串不能误判 —— 这些必须放行
        assert!(regex_looking_pattern("fn find_model").is_none());
        assert!(regex_looking_pattern("Custom Endpoint").is_none());
        assert!(regex_looking_pattern("已配置").is_none());
        assert!(regex_looking_pattern("deepseek").is_none());
        assert!(regex_looking_pattern("api/ide/v1/chat").is_none());
    }

    #[test]
    fn window_around_keeps_needle_visible_in_giant_line() {
        // 复刻 trae_dump.cjs：单行 116 万字符，词在第 50 万字符处。
        // 旧版 `trim().chars().take(200)` 会返回一段**不含该词**的内容。
        let mut line = "x".repeat(500_000);
        line.push_str("NEEDLE_HERE");
        line.push_str(&"y".repeat(500_000));
        let shown = window_around(&line, "needle_here", false, 200);
        assert!(
            shown.to_lowercase().contains("needle_here"),
            "围绕命中位置取窗口后必须仍能看到所搜词，实际: {}",
            &shown[..shown.len().min(80)]
        );
        assert!(shown.chars().count() <= 202, "窗口不能超长（含省略号）");
        // 短行原样返回
        assert_eq!(window_around("hello world", "world", false, 200), "hello world");
    }

    #[test]
    fn window_around_is_multibyte_safe() {
        // 中文命中位置不能把字符切坏
        let line = format!("{}目标词{}", "汉".repeat(500), "字".repeat(500));
        let shown = window_around(&line, "目标词", false, 120);
        assert!(shown.contains("目标词"), "中文窗口必须保留命中词");
    }

    #[test]
    fn grep_skips_describe_reports_every_category() {
        // 「跳过必须可见」—— 这是本工具最重要的设计约束
        let mut s = GrepSkips::default();
        assert!(!s.any(), "没有跳过时 any() 必须为 false");
        assert!(s.describe().is_empty(), "没有跳过时不能产生噪声");

        s.too_big = 2;
        s.not_utf8 = 1;
        s.denied = 3;
        assert!(s.any());
        let d = s.describe();
        assert!(d.contains("2 个文件超过 2 MB"), "必须说明大文件跳过: {d}");
        assert!(d.contains("1 个文件不是 UTF-8"), "必须说明非文本跳过: {d}");
        assert!(d.contains("3 个文件因权限"), "必须说明权限跳过: {d}");

        let mut s2 = GrepSkips::default();
        s2.visited_limit = true;
        assert!(s2.describe().contains("扫描文件数上限"));
    }

    #[test]
    fn read_text_lossy_falls_back_to_gbk() {
        let dir = fresh_tmp("grep_gbk");
        // 中文机器上的 GBK 文本（严格 UTF-8 解不开）——旧版直接静默跳过
        let p = dir.join("gbk.md");
        let gbk: &[u8] = &[0xCA, 0xFD, 0xBE, 0xDD, 0x2E, 0x6D, 0x64]; // "数据.md" 的 GBK 字节
        std::fs::write(&p, gbk).unwrap();
        let got = read_text_lossy(&p);
        assert!(got.is_some(), "GBK 文件必须能读出来（回落 CP_ACP），不能静默跳过");
        let (text, is_utf8) = got.unwrap();
        assert!(!is_utf8, "应标记为非 UTF-8");
        assert!(!text.is_empty(), "解码结果不能为空");

        // 纯 UTF-8 文件走原路径
        let p2 = dir.join("utf8.md");
        std::fs::write(&p2, "正常 UTF-8 内容").unwrap();
        let (t2, u2) = read_text_lossy(&p2).unwrap();
        assert!(u2 && t2.contains("正常"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // grep_files 端到端：重放真实失败现场（2026-10-01）
    // -----------------------------------------------------------------------

    /// 复刻 `s1790833919027-2` 那次事故：模型对**单个文件**调 grep_files，
    /// 文件里明明有这个词，工具却报「命中 0 行」→ 模型放弃工具改跑 PowerShell。
    #[test]
    fn grep_files_accepts_a_file_as_path() {
        let dir = fresh_tmp("grep_filepath");
        let f = dir.join("wb_sysprompt_extract.txt");
        // 真实内容形态：system prompt 全文，第 78 行那句收尾模板
        let body = format!(
            "line1\n{}\n- End-of-turn summary: What changed and what's next. Nothing else.\n{}",
            "填充\n".repeat(70),
            "尾巴\n".repeat(30)
        );
        std::fs::write(&f, &body).unwrap();

        let gate = gate_for(&dir);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &dir, mcp);

        // ★ 旧版在这里返回「命中 0 行」——walk() 对文件 read_dir 必失败
        let out = grep_files(
            &json!({ "pattern": "End-of-turn summary", "path": f.to_string_lossy() }),
            &ctx,
        )
        .expect("grep 不应报错");
        assert!(
            out.text.contains("命中 1 行"),
            "path 传文件必须能搜到内容，实际返回:\n{}",
            out.text
        );
        assert!(out.text.contains("End-of-turn summary"), "应回显命中行");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 大小写不敏感：实测 319 次零命中里 206 次 pattern 纯小写，
    /// 而目标是 `Custom Endpoint` 这类首字母大写。
    #[test]
    fn grep_files_is_case_insensitive_by_default() {
        let dir = fresh_tmp("grep_case");
        std::fs::write(dir.join("a.md"), "配置里叫 Custom Endpoint，只支持 openai 兼容。").unwrap();

        let gate = gate_for(&dir);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &dir, mcp);

        let out = grep_files(
            &json!({ "pattern": "custom endpoint", "path": dir.to_string_lossy() }),
            &ctx,
        )
        .unwrap();
        assert!(out.text.contains("命中 1 行"), "小写 pattern 应能命中大写文本:\n{}", out.text);

        // 显式要求区分大小写时，就不该命中
        let out2 = grep_files(
            &json!({
                "pattern": "custom endpoint",
                "path": dir.to_string_lossy(),
                "caseSensitive": true
            }),
            &ctx,
        )
        .unwrap();
        assert!(out2.text.contains("命中 0 行"), "caseSensitive=true 时不应命中");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「命中 N 行」必须只数真正的命中行 —— 带 context 时不能把上下文行算进去。
    /// （实测 context=2 报「命中 200 行」其实只有 40 处命中，虚高 160。）
    #[test]
    fn grep_files_hit_count_excludes_context_lines() {
        let dir = fresh_tmp("grep_ctxcount");
        // 3 处命中，每处前后各 2 行 → 旧版会报 3×5 = 15 行
        let mut s = String::new();
        for i in 0..3 {
            s.push_str("ctx-a\nctx-b\n");
            s.push_str(&format!("NEEDLE at {i}\n"));
            s.push_str("ctx-c\nctx-d\n");
        }
        std::fs::write(dir.join("a.txt"), &s).unwrap();

        let gate = gate_for(&dir);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &dir, mcp);

        let out = grep_files(
            &json!({ "pattern": "NEEDLE", "path": dir.to_string_lossy(), "context": 2 }),
            &ctx,
        )
        .unwrap();
        assert!(
            out.text.contains("命中 3 行"),
            "命中数必须是 3（真正的命中行），不能把上下文行算进去:\n{}",
            out.text
        );
        // 上下文行仍要展示出来
        assert!(out.text.contains("ctx-a"), "上下文行仍应显示");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跳过的文件必须**明确回报** —— 这是让模型能自我纠正的前提。
    #[test]
    fn grep_files_reports_skipped_oversized_file() {
        let dir = fresh_tmp("grep_skipbig");
        // > 2MB 的文件，里面有目标词 —— 旧版静默跳过并报「无匹配」
        let big = dir.join("big.log");
        let mut s = String::from("PADDING\n");
        s.push_str(&"x".repeat(3 * 1024 * 1024));
        s.push_str("\nTARGET_TOKEN\n");
        std::fs::write(&big, &s).unwrap();

        let gate = gate_for(&dir);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &dir, mcp);

        let out = grep_files(
            &json!({ "pattern": "TARGET_TOKEN", "path": dir.to_string_lossy() }),
            &ctx,
        )
        .unwrap();
        assert!(out.text.contains("命中 0 行"), "大文件确实读不了");
        assert!(
            out.text.contains("超过 2 MB"),
            "必须明确告诉模型有文件因体积被跳过（否则它无法区分'没有'和'没读'）:\n{}",
            out.text
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正则语法要**当场点破**，而不是返回一个误导性的「无匹配」。
    #[test]
    fn grep_files_rejects_regex_syntax_with_actionable_error() {
        let dir = fresh_tmp("grep_regex");
        std::fs::write(dir.join("a.md"), "mimo-server-cn").unwrap();

        let gate = gate_for(&dir);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &dir, mcp);

        // 模型真实写过的形态：多词用 | 串联
        let err = grep_files(
            &json!({ "pattern": "mimo-server-cn|/api/route/chat", "path": dir.to_string_lossy() }),
            &ctx,
        )
        .expect_err("含正则语法必须报错，不能返回误导性的 0 命中");
        assert!(err.contains("子串"), "错误信息要说清是子串匹配: {err}");
        assert!(err.contains("|"), "应点出具体是哪个语法: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GBK 文件不再被静默跳过（中文 Windows 的 .md/.txt 常见）。
    #[cfg(windows)]
    // 这些用例硬编码了 Windows 路径（`D:\…`）与 Windows 的大小写不敏感语义，
    // 在 macOS/Linux 上 `D:\` 只是一个相对文件名，断言必然失败 —— 不是产品 bug。
    // 2026-10-10 由 macos-14 runner 抓出。
    #[test]
    fn grep_files_reads_gbk_files() {
        let dir = fresh_tmp("grep_gbk_e2e");
        // "数据.md" 的 GBK 字节（数=CAFD 据=BEDD）—— 严格 UTF-8 解不开
        let gbk: &[u8] = &[0xCA, 0xFD, 0xBE, 0xDD, 0x2E, 0x6D, 0x64];
        std::fs::write(dir.join("gbk.md"), gbk).unwrap();

        let gate = gate_for(&dir);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &dir, mcp);

        let out = grep_files(
            &json!({ "pattern": "数据", "path": dir.to_string_lossy() }),
            &ctx,
        )
        .unwrap();
        assert!(
            out.text.contains("命中 1 行"),
            "GBK 文件里的中文必须能搜到:\n{}",
            out.text
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fresh_tmp(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("orbcat_{tag}_{stamp}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // -----------------------------------------------------------------------
    // run_code（PTC）端到端
    // -----------------------------------------------------------------------
    //
    // ⚠️ 这些测试**真的会起 Node**，且走的是**真实的 `execute`**（含权限网关）。
    // 为什么值得：PTC 的全部价值在于"程序里的调用与模型直接发的调用走同一条
    // 管线"—— 若为子调用另开一条绕过权限的快路，那就是个后门。
    // 假 Node 或假 execute 都证明不了这件事。

    /// PTC 下 `run_code` 能真的读文件，且**权限网关照常生效**。
    ///
    /// 这条同时钉住两件事：
    ///   1. 子调用复用了 `execute`（所以文件真被读到）
    ///   2. 越权路径仍被拒（所以不是绕过网关的旁路）
    #[tokio::test]
    async fn run_code_reads_file_through_the_real_permission_gate() {
        let tmp = fresh_tmp("ptc_read");
        if crate::ptc::resolve_node().is_none() {
            eprintln!("跳过：本机没有 node");
            return;
        }
        // 建一个数据目录 + 一个目标文件（都在网关允许的范围内）
        let data = tmp.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let target = tmp.join("hello.txt");
        std::fs::write(&target, "PTC_MARKER_12345").unwrap();

        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &data, mcp);

        let code = format!(
            "const r = await tools.read_file({{ path: {} }});\nreturn r;",
            serde_json::to_string(&target.to_string_lossy()).unwrap()
        );
        let out = execute(
            crate::ptc::RUN_CODE,
            &json!({ "code": code, "description": "读一个文件" }),
            &ctx,
        )
        .await
        .expect("run_code 本身不该 Err");

        assert!(
            out.text.contains("PTC_MARKER_12345"),
            "程序里读到的文件内容要回到模型: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 程序里调用**不存在**的工具 → 程序收到 reject，`try/catch` 能接住。
    /// 证明子调用真的走的是工具管线（而不是"什么都成功"的假桥）。
    #[tokio::test]
    async fn run_code_sub_call_failure_is_catchable_in_program() {
        let tmp = fresh_tmp("ptc_fail");
        if crate::ptc::resolve_node().is_none() {
            return;
        }
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &tmp, mcp);

        let code = "let got = null;\n\
                    try { await tools.no_such_tool({}); } catch (e) { got = e.toolName; }\n\
                    return { got };";
        let out = execute(
            crate::ptc::RUN_CODE,
            &json!({ "code": code, "description": "试探不存在的工具" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(
            out.text.contains("no_such_tool"),
            "失败要能被程序捕获且带上工具名: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 程序里**不能**再调 `run_code`（嵌套第二层 Node 进程）。
    /// 必须给出清晰理由，而不是悄悄起一堆 Node。
    #[tokio::test]
    async fn run_code_refuses_nested_run_code() {
        let tmp = fresh_tmp("ptc_nested");
        if crate::ptc::resolve_node().is_none() {
            return;
        }
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &tmp, mcp);

        let code = "let msg = null;\n\
                    try { await tools.run_code({ code: 'return 1', description: 'x' }); } \
                    catch (e) { msg = e.message; }\n\
                    return { msg };";
        let out = execute(
            crate::ptc::RUN_CODE,
            &json!({ "code": code, "description": "试着嵌套" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(
            out.text.contains("嵌套"),
            "嵌套 run_code 要被拒并说明原因: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `code` 为空要当场报错（别白起一个 Node 进程）。
    #[tokio::test]
    async fn run_code_rejects_empty_code() {
        let tmp = fresh_tmp("ptc_empty");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &tmp, mcp);

        let err = execute(
            crate::ptc::RUN_CODE,
            &json!({ "code": "   ", "description": "x" }),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(err.contains("不能为空"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PTC 的 `run_code` 规格必须在**坍缩后**的工具列表里（且是唯一一个）。
    #[test]
    fn run_code_spec_is_present_and_well_formed() {
        let spec = run_code_spec();
        assert_eq!(spec.name, crate::ptc::RUN_CODE);
        let req = spec
            .parameters
            .get("required")
            .and_then(Value::as_array)
            .expect("run_code 要有 required");
        let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
        assert!(names.contains(&"code"), "code 必填");
        assert!(names.contains(&"description"), "description 必填");
        // 描述里必须点明"唯一能直接调用的工具"，否则模型会去猜别的工具名
        assert!(
            spec.description.contains("唯一能直接调用"),
            "run_code 描述要说明它是唯一入口"
        );
    }

    /// 🔴 **坍缩必须同时作用在"可调用面"上**，不只是工具列表。
    ///
    /// 只藏工具列表、却仍执行模型直接发的别的工具名，就是"公告面与可调用面
    /// 不一致"：模型会学到"公告不可信"，然后开始乱试工具名。
    /// DSH 的 `dsh-tools` 把这条做成 `UNKNOWN_TOOL`，同一个道理。
    #[tokio::test]
    async fn ptc_collapse_blocks_direct_calls_to_other_tools() {
        let tmp = fresh_tmp("ptc_collapse_block");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));

        // 先确认**非 PTC** 下 read_file 是能调的（否则下面证明不了什么）
        // 注：`ctx_for!` 的 `session_id` 是空串 → `effective_mode` 兜到内置默认
        // （standard），正是"非 PTC"那一侧。
        let target = tmp.join("x.txt");
        std::fs::write(&target, "OK_CONTENT").unwrap();
        {
            let ctx = ctx_for!(gate, &tmp, mcp);
            let ok = execute(
                "read_file",
                &json!({ "path": target.to_string_lossy() }),
                &ctx,
            )
            .await
            .expect("标准模式下 read_file 该能调");
            assert!(ok.text.contains("OK_CONTENT"));
        }

        // 换成一条 **PTC 会话**（模式跟会话走，2026-10-08）。
        // ⚠️ 不能再写 `settings.json` 的 `activeMode` —— 那字段已删。
        let sid = crate::sessions::new_session(&tmp, "ptc").id;

        let ctx = ctx_with_session!(gate, &tmp, mcp, &sid);
        let err = execute(
            "read_file",
            &json!({ "path": target.to_string_lossy() }),
            &ctx,
        )
        .await
        .expect_err("PTC 下模型不该能直接调 read_file");

        // 报错要**教它怎么写**，而不是只说"不行"
        assert!(err.contains(crate::ptc::RUN_CODE), "要指出唯一入口: {err}");
        assert!(err.contains("tools.read_file"), "要给出程序内的写法: {err}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 但**程序内部**的调用不能被上面那道闸拦住（否则 PTC 直接不可用）。
    /// 这条与上一条是一对：一个证明拦得住，一个证明没拦错。
    #[tokio::test]
    async fn ptc_sub_calls_are_not_blocked_by_the_collapse() {
        let tmp = fresh_tmp("ptc_sub_ok");
        if crate::ptc::resolve_node().is_none() {
            eprintln!("跳过：本机没有 node");
            return;
        }
        let target = tmp.join("y.txt");
        std::fs::write(&target, "SUB_CALL_OK").unwrap();

        // 整场都在 PTC 会话里跑（模式跟会话走，2026-10-08）
        let sid = crate::sessions::new_session(&tmp, "ptc").id;

        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_with_session!(gate, &tmp, mcp, &sid);

        let code = format!(
            "return await tools.read_file({{ path: {} }});",
            serde_json::to_string(&target.to_string_lossy()).unwrap()
        );
        let out = execute(
            crate::ptc::RUN_CODE,
            &json!({ "code": code, "description": "程序内读文件" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(
            out.text.contains("SUB_CALL_OK"),
            "程序内的 read_file 必须能正常执行: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `search_tools` 是**能力发现**入口，必须端到端可用：
    /// 模型给一个意图词 → 拿到工具全名 + 一句话用途，且**不该**顺带塞进 schema。
    ///
    /// 同时断言它是**只读**的：搜完活跃集不能变（否则模型会拿它当 load 用，
    /// 一次搜索就把整组 schema 拉进上下文，正好把懒加载的意义抵消掉）。
    #[tokio::test]
    async fn search_tools_finds_tool_by_intent_without_loading_group() {
        let tmp = fresh_tmp("search_tools");
        let gate = gate_for(&tmp);

        let reg = ToolRegistry::from_tools_for_test(vec![
            McpTool {
                name: "github_1mcp_create_issue".into(),
                description: "Create a new issue in a GitHub repository".into(),
                input_schema: json!({"type":"object","properties":{"title":{"type":"string"}}}),
                annotations: None,
            },
            McpTool {
                name: "ssh_1mcp_run-command".into(),
                description: "在远程主机上执行命令".into(),
                input_schema: json!({"type":"object","properties":{}}),
                annotations: None,
            },
        ]);
        let mcp = tokio::sync::Mutex::new(reg);

        // 命中：意图词 `issue` → 那条工具的全名与用途都在结果里
        let hit = execute("search_tools", &json!({ "query": "issue" }), &ctx_for!(gate, &tmp, mcp))
            .await
            .unwrap();
        // 新方案：组名 = server id，工具名原样透传（不剥网关前缀）
        assert!(hit.text.contains("mcp__default__github_1mcp_create_issue"), "{}", hit.text);
        assert!(hit.text.contains("Create a new issue"), "{}", hit.text);
        // 组摘要含 "issue"（来自工具名采样）→ 整组命中，另一条工具也会列出
        // 这是 by-design：组名/摘要命中 = 整组给出

        // 空 query = 完整索引（模型在问"我到底有什么工具"）
        let all = execute("search_tools", &json!({}), &ctx_for!(gate, &tmp, mcp))
            .await
            .unwrap();
        assert!(all.text.contains("create_issue"));
        assert!(all.text.contains("run-command"));
        // 索引绝不含 schema —— 那是 load_tool_group 之后才该付的钱
        assert!(!all.text.contains("input_schema"), "索引不得携带 schema");
        assert!(!all.text.contains("\"properties\""), "索引不得携带 schema");

        // 只读：搜完活跃集不变（未加载的组仍未加载）
        assert!(
            !mcp.lock().await.is_active("github"),
            "search_tools 不许有加载副作用"
        );

        // 搜不到时给出可行动信息（可用组名），而不是干巴巴一句"没有"
        let miss = execute(
            "search_tools",
            &json!({ "query": "zzzz-not-a-tool" }),
            &ctx_for!(gate, &tmp, mcp),
        )
        .await
        .unwrap();
        assert!(miss.text.contains("default"), "{}", miss.text);

        let _ = std::fs::remove_dir_all(&tmp);
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

    #[tokio::test]
    async fn move_copy_mkdir_file_info_append_work() {
        let tmp = fresh_tmp("fs_extra");
        let gate = gate_for(&tmp);
        let mcp = tokio::sync::Mutex::new(ToolRegistry::new(""));
        let ctx = ctx_for!(gate, &tmp, mcp);

        let sub = tmp.join("sub");
        let out = execute("mkdir", &json!({ "path": sub.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(out.text.contains("已创建") || out.text.contains("已存在"), "{}", out.text);
        assert!(sub.is_dir());

        let log = tmp.join("sub/log.txt");
        execute(
            "append_file",
            &json!({ "path": log.to_string_lossy(), "content": "hello\n" }),
            &ctx,
        )
        .await
        .unwrap();
        execute(
            "append_file",
            &json!({ "path": log.to_string_lossy(), "content": "world\n" }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "hello\nworld\n");

        let out = execute("file_info", &json!({ "path": log.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(out.text.contains("exists: true"), "{}", out.text);
        assert!(out.text.contains("is_dir: false"), "{}", out.text);

        let cp = tmp.join("sub/log-copy.txt");
        let out = execute(
            "copy_file",
            &json!({ "src": log.to_string_lossy(), "dst": cp.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.text.contains("已复制"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(&cp).unwrap(), "hello\nworld\n");

        let mv = tmp.join("sub/log-moved.txt");
        let out = execute(
            "move_file",
            &json!({ "src": cp.to_string_lossy(), "dst": mv.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.text.contains("已移动"), "{}", out.text);
        assert!(!cp.exists());
        assert!(mv.exists());

        let err = execute(
            "copy_file",
            &json!({ "src": mv.to_string_lossy(), "dst": log.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(err.contains("已存在"), "{}", err);

        let err = execute(
            "git",
            &json!({ "subcommand": "commit", "repo": tmp.to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(err.contains("只读") || err.contains("run_command"), "{}", err);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
