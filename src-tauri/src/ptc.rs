//! PTC（Programmatic Tool Calling）—— 让模型把工具调用**写成程序**。
//!
//! ## 这个模块解决什么
//!
//! 普通（native）模式下，模型每轮只能"喊"出若干个 tool_calls，然后等结果回灌。
//! 于是批量任务（读 50 个文件、查 5 个表、逐条筛选）会退化成：
//!
//! ```text
//!   喊 N 个调用 → N 份结果**全部进对话历史** → 下一轮再喊 → …
//! ```
//!
//! 两个代价都是硬的：**轮数**受限于模型每轮愿意输出多少调用，
//! **上下文**被每个中间结果永久占据（直到压缩触发）。
//!
//! PTC 把控制流搬进程序：
//!
//! ```text
//!   模型写一段 TypeScript（一次 run_code 调用）
//!     → 程序里 for / if / Promise.all / try-catch 自己组织调用
//!     → 中间结果只活在程序局部变量里
//!     → 只有 return / console.log 的东西回到对话
//! ```
//!
//! ## 为什么不是"多给工具 + 一句 prompt"
//!
//! 曾经的做法是 `mcpGroupsPreload: "all"` 加一段"请一轮发多个 tool_calls"的
//! 提示词。那**只做到了并发的一半**：模型仍要自己枚举每个调用，
//! 中间结果仍全部进历史，更没有循环与条件。实测那等于"标准模式 + 更多工具"，
//! 所以模式之间看不出区别。
//!
//! ## 工具怎么变成程序里的 SDK
//!
//! 工具**不再逐个进模型工具列表**（那是 native 的做法）。PTC 下模型只看到
//! 一个 `run_code`，其余工具以 TypeScript 声明（[`sdk_declarations`]）的形式
//! 出现在 system prompt 里，程序内通过 `tools.<名字>(args)` 调用。
//!
//! ## 一次执行的完整链路
//!
//! ```text
//!   run_code({code, description})
//!     ├─ 1. 把 code 包成一个 .ts 文件（导出 default async 函数）
//!     ├─ 2. 起 node bootstrap.mjs <user.ts>（stdin/stdout 走 JSON 行协议）
//!     ├─ 3. 程序里每次 tools.x() → 一行 {type:"call"} 到父进程
//!     ├─ 4. 父进程**复用原生工具管线**执行（权限/审计/截断全部照旧）
//!     ├─ 5. 结果回一行 {type:"result"} → 程序里的 Promise 兑现
//!     └─ 6. 程序返回 → 一行 {type:"done", value, logs} → 成为 run_code 的结果
//! ```
//!
//! ⚠️ **权限没有旁路**：子调用走的是同一个 [`crate::tools::execute`]，
//! 因此文件网关、命令策略、权限卡、审计链全部照旧生效。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 模型唯一能直接调用的工具名。
pub const RUN_CODE: &str = "run_code";

/// 当前生效的模式是不是 PTC。
///
/// 为什么收成一个函数：`tools.rs`（工具坍缩）、`agent.rs`（提示词段）
/// 两处都要问同一个问题，各写一遍"读 settings + resolve modes"迟早写岔 ——
/// 而写岔的后果是**工具坍缩了但提示词没换**（模型看到 run_code 却不知道怎么用），
/// 或反过来（提示词讲了 SDK 但工具还是 native 列表）。那是很难查的半坏状态。
pub fn mode_active(data_dir: &Path, session_id: Option<&str>) -> bool {
    // 2026-10-08：模式**跟着会话走**（用户定案），所以一律经
    // `sessions::effective_mode` 读 —— 它自己处理"老会话没 mode 字段 → 回退全局"。
    crate::sessions::effective_mode(data_dir, session_id).is_ptc()
}

/// 一次 `run_code` 最多允许多少次子调用。
///
/// 为什么要有上限：程序里的循环是模型写的，写错一个条件就是无限调用。
/// 这不是安全边界（真正的闸门是权限系统），而是**防呆**：
/// 让"忘记 break"变成一条清晰的错误，而不是跑到天荒地老。
pub const MAX_SUB_CALLS: usize = 500;

/// 同时在飞的子调用上限。
///
/// 与 native 侧 `agent.rs` 的并行池同一量级（10）：PTC 不该悄悄提高并发度，
/// 它改变的是"谁来组织调用"，不是"能同时跑多少"。
pub const MAX_PARALLEL: usize = 10;

/// 程序输出的字节上限（logs + 返回值合计）。
///
/// 为什么需要：程序可以 `console.log` 十万行。没有这个上限，
/// "中间结果不进上下文"这个卖点会被一行 `console.log(所有内容)` 当场毁掉。
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// SDK 声明生成
// ---------------------------------------------------------------------------

/// TypeScript 类型 → 声明里的类型文本。
///
/// 只做**够用**的映射，认不出来的一律 `unknown`：声明的作用是让模型知道
/// "有这个名字、大致收什么参数"，精确校验仍在工具层做（`defineTool` 那套）。
/// 在这里追求完备的 JSON Schema → TS 转换是过度工程。
fn ts_type(schema: &Value) -> String {
    let ty = schema.get("type").and_then(Value::as_str);
    match ty {
        Some("string") => "string".into(),
        Some("number") | Some("integer") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => {
            let item = schema
                .get("items")
                .map(ts_type)
                .unwrap_or_else(|| "unknown".into());
            format!("{item}[]")
        }
        Some("object") => {
            let Some(props) = schema.get("properties").and_then(Value::as_object) else {
                return "Record<string, unknown>".into();
            };
            if props.is_empty() {
                return "Record<string, unknown>".into();
            }
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let fields: Vec<String> = props
                .iter()
                .map(|(k, v)| {
                    // 键名不是合法标识符时加引号（`tools["my-tool"]` 同理）
                    let key = if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && !k.chars().next().is_some_and(|c| c.is_ascii_digit())
                    {
                        k.clone()
                    } else {
                        format!("\"{k}\"")
                    };
                    let opt = if required.contains(&k.as_str()) { "" } else { "?" };
                    format!("{key}{opt}: {}", ts_type(v))
                })
                .collect();
            format!("{{ {} }}", fields.join("; "))
        }
        _ => "unknown".into(),
    }
}

/// 工具名 → 合法的 TS 属性访问写法。
///
/// `mcp__github__create_issue` 这类名字里有 `__` 是合法的，但
/// 万一出现 `-` / `.` 就必须写成 `tools["my-tool"]` —— 否则生成的是**语法错误**，
/// 模型照着写必然解析失败。
fn ts_member(name: &str) -> String {
    let ok = !name.is_empty()
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ok {
        name.to_string()
    } else {
        format!("[{}]", serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into()))
    }
}

/// 把工具清单渲染成一段 TypeScript 声明。
///
/// 形态（模型看到的就是这个）：
///
/// ```ts
/// declare const tools: {
///   read_file(args: { path: string; offset?: number }): Promise<unknown>;
///   ...
/// };
/// ```
///
/// 为什么用 `declare`：这些名字**只在程序内**存在，不是可以直接调用的工具。
/// 生成一段真实可编译的声明能让模型少写错参数名，同时不给它"我可以直接调"的错觉。
pub fn sdk_declarations(tools: &[(String, String, Value)]) -> String {
    if tools.is_empty() {
        return "declare const tools: Record<string, (args: unknown) => Promise<unknown>>;".into();
    }
    let mut out = String::from("declare const tools: {\n");
    for (name, desc, schema) in tools {
        // 一行用途注释：让模型知道该用哪个（参数细节看类型）
        let one_line: String = desc
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(160)
            .collect();
        if !one_line.is_empty() {
            out.push_str(&format!("  /** {} */\n", one_line.replace("*/", "*\\/")));
        }
        out.push_str(&format!(
            "  {}: (args: {}) => Promise<unknown>;\n",
            ts_member(name),
            ts_type(schema)
        ));
    }
    out.push_str("};");
    out
}

/// PTC 模式下给模型的**行为契约**（进 system prompt 的动态尾部）。
///
/// 这一段是 PTC 的一半价值所在：光把工具换成 SDK 而不说清"怎么写、什么会回到上下文"，
/// 模型会当成普通的单次调用用（那就退化回 native 了）。
pub fn behavior_section(decls: &str) -> String {
    format!(
        "# 当前模式：PTC（把工具调用写成程序）\n\n\
         这个模式下你**只有 `run_code` 一个工具可以直接调用**。其余工具都以 \
         TypeScript 声明的形式提供给你，在程序里通过 `tools.<名字>(args)` 调用。\n\n\
         ## 怎么写\n\n\
         - `code` 是**一个 async 函数的函数体**（不是完整文件）：可以直接 `await`、\
         直接 `return`。\n\
         - 调用工具：`await tools.read_file({{ path: \"...\" }})`。\n\
         - 名字里有特殊字符的用引号访问：`tools[\"my-tool\"]({{...}})`。\n\
         - **并发只读**：互不依赖的只读调用可以 `await Promise.all([...])` —— \
         它们会真正并发跑。有依赖的（后一个要用前一个的结果）必须 `await` 串行。\n\
         - **失败要自己接**：某次调用失败会让那个 Promise reject（`ToolCallError`，\
         带 `toolName` 和 `message`）。想让「个别失败不影响整体」就 `try/catch` 它，\
         记进结果继续跑。\n\n\
         ## 什么会回到我这里（这条最关键）\n\n\
         **只有 `return` 的返回值和你 `console.log` 的内容会回到上下文。**\n\
         所有中间结果都只活在程序里 —— 所以**不要在循环里把每条原始数据都 log 出来**，\
         那等于把 PTC 最大的好处扔掉。先在程序里筛选/汇总/统计，只把结论交回来。\n\n\
         ## 参数声明\n\n\
         ```ts\n{decls}\n```\n\n\
         ⚠️ 声明只是给你看参数形状的，**它们不是可以直接调用的工具**；\
         唯一能直接调用的是 `run_code`。"
    )
}

// ---------------------------------------------------------------------------
// Node 引导脚本
// ---------------------------------------------------------------------------

/// Node 侧引导脚本（随 `run_code` 写到临时目录，用完即删）。
///
/// 为什么把 JS 嵌在 Rust 里而不是放一个 `assets/` 文件：它是一个**协议实现**，
/// 与下面 `drive` 里的读写逻辑必须逐字对齐（改一边忘一边 = 静默死锁）。
/// 放在同一个文件里，改协议时两半就在眼前。
///
/// 为什么用 `.mjs` + 动态 `import()` 而不是 `--eval`：
/// Node 的类型擦除（type stripping）只在**文件**上生效，`--eval` 的字符串
/// 不经过它，模型写的 `: number` 会直接语法报错。
const BOOTSTRAP: &str = r#"import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";

const userFile = process.argv[2];
const pending = new Map();
let seq = 0;
const logs = [];
let loggedBytes = 0;

const send = (msg) => { process.stdout.write(JSON.stringify(msg) + "\n"); };

const fmt = (v) => {
  if (typeof v === "string") return v;
  try { return JSON.stringify(v); } catch { return String(v); }
};

// console.* 全部收进 logs —— 它们随 done 一起回父进程，由父进程决定怎么呈现。
// 直接写 stdout 会污染协议通道（父进程按行 JSON 解析），所以必须截获。
const cap = (...a) => {
  const line = a.map(fmt).join(" ");
  loggedBytes += line.length;
  if (loggedBytes <= 262144) logs.push(line);
};
console.log = cap; console.info = cap; console.warn = cap;
console.debug = cap; console.error = cap;

// 工具命名空间用 Proxy：任意工具名都能取到，包括带 `-` 的名字（tools["my-tool"]）。
globalThis.tools = new Proxy({}, {
  get(_t, name) {
    if (typeof name !== "string") return undefined;
    return (args) => {
      const id = ++seq;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        send({ type: "call", id, name, args: args === undefined ? {} : args });
      });
    };
  },
});

const rl = createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.type !== "result") return;
  const p = pending.get(msg.id);
  if (!p) return;
  pending.delete(msg.id);
  if (msg.ok) { p.resolve(msg.value); }
  else {
    const e = new Error(msg.error);
    e.name = "ToolCallError";
    e.toolName = msg.name;
    p.reject(e);
  }
});

const finish = (msg) => {
  rl.close();
  process.stdout.write(JSON.stringify(msg) + "\n", () => process.exit(0));
};

process.on("unhandledRejection", (e) => {
  finish({ type: "error", message: "unhandledRejection: " + ((e && e.stack) || String(e)), logs });
});

try {
  const mod = await import(pathToFileURL(userFile).href);
  const value = await mod.default();
  finish({ type: "done", value: value === undefined ? null : value, logs });
} catch (e) {
  finish({ type: "error", message: (e && e.stack) || String(e), logs });
}
"#;

/// 把模型写的 `code`（函数体）包成一个可被 `import` 的 `.ts` 模块。
///
/// 为什么用"导出 default 函数"而不是把 body 直接塞进顶层：
/// 模型写的 body 里会有 `return`（顶层 return 在 ESM 里是语法错误），
/// 而且用户可能写 `await` 之外的顶层语句。包进 async 函数两者都合法。
pub fn wrap_program(code: &str) -> String {
    // 行号对齐：模型报错时会看行号，包一层会让行号偏移。
    // 前缀恰好占一行，所以 body 的物理行号 = 模型看到的行号 + 1。
    format!("export default async function () {{\n{code}\n}}\n")
}

// ---------------------------------------------------------------------------
// 落盘 + 清理
// ---------------------------------------------------------------------------

/// 一次 PTC 运行的工作目录（放 `data_dir/tmp/ptc-<stamp>/`）。
///
/// 为什么不用系统 temp：这个目录在 `data_dir` 下，用户能自己去看
/// （程序写坏了、想复现模型到底跑了什么时，源码还躺在那里）。
pub struct WorkDir {
    pub dir: PathBuf,
}

impl WorkDir {
    /// 建目录 + 写引导脚本与用户程序，返回可执行的 argv 素材。
    pub fn create(data_dir: &Path, code: &str) -> Result<Self, String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = data_dir.join("tmp").join(format!("ptc-{stamp}"));
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("建 PTC 临时目录 {} 失败: {e}", dir.display()))?;

        // ⚠️ **必须有这个文件**（2026-10 实测踩到）：
        // Node 决定 `.ts` 用 ESM 还是 CJS 解析，靠的是**最近的 package.json 的
        // `type` 字段**。而"最近"是沿目录树往上找的 —— 用户家目录里只要有一个
        // `{"type":"commonjs"}`（实测 `C:\Users\DELL\package.json` 就是，
        // 大概来自某次 `npm init`），我们写出的 `program.ts` 就会被当 CJS 加载，
        // 于是 `export default` 直接 `SyntaxError: Unexpected token 'export'`。
        //
        // 现象极具误导性：程序看起来"写得完全正确"却报语法错，且报错行指向
        // 我们**自己生成**的那行包装代码（用户会以为是我们坏了）。
        // 所以不能依赖上层环境 —— 在自己目录里放一份，就近生效。
        std::fs::write(dir.join("package.json"), "{\"type\":\"module\"}\n")
            .map_err(|e| format!("写 PTC package.json 失败: {e}"))?;

        std::fs::write(dir.join("bootstrap.mjs"), BOOTSTRAP)
            .map_err(|e| format!("写 PTC 引导脚本失败: {e}"))?;
        std::fs::write(dir.join("program.ts"), wrap_program(code))
            .map_err(|e| format!("写 PTC 程序失败: {e}"))?;

        Ok(Self { dir })
    }

    /// 引导脚本路径
    pub fn bootstrap(&self) -> PathBuf {
        self.dir.join("bootstrap.mjs")
    }

    /// 用户程序路径
    pub fn program(&self) -> PathBuf {
        self.dir.join("program.ts")
    }
}

impl Drop for WorkDir {
    /// 清理。失败**不报错**：临时目录留一点垃圾，远好过因为删不掉而让
    /// 整个 `run_code` 失败（Windows 上文件被占是常态）。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 找 Node 可执行文件。
///
/// 为什么不用 `Command::new("node")` 让系统搜 PATH：与 `shell.rs` 的
/// `resolve_interpreter` 同一个坑 —— 隐形桌面下 `CreateProcessW` **不做 PATH 搜索**，
/// 传 "node" 直接 `GetLastError=2`。所以这里显式探测几个常见位置。
pub fn resolve_node() -> Option<PathBuf> {
    // 1) 环境变量优先（用户想指定自己那份 Node）
    if let Ok(p) = std::env::var("ORBCAT_NODE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    // 2) PATH（std::process::Command 会搜，用于探测；真正起进程时传绝对路径）
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in ["node.exe", "node"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }

    // 3) 常见安装位置（PATH 没配全时的兜底）
    let mut cands: Vec<PathBuf> = vec![
        PathBuf::from(r"D:\NODE.JS\node.exe"),
        PathBuf::from(r"C:\Program Files\nodejs\node.exe"),
        PathBuf::from(r"C:\Program Files (x86)\nodejs\node.exe"),
    ];
    if let Ok(pf) = std::env::var("ProgramFiles") {
        cands.push(PathBuf::from(pf).join("nodejs").join("node.exe"));
    }
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        cands.push(PathBuf::from(la).join("Programs").join("nodejs").join("node.exe"));
    }
    cands.into_iter().find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// 协议消息
// ---------------------------------------------------------------------------

/// Node → 父进程的一条消息。
#[derive(Debug, Clone)]
pub enum NodeMsg {
    /// 程序要调一个工具
    Call {
        id: u64,
        name: String,
        args: Value,
    },
    /// 程序跑完了
    Done { value: Value, logs: Vec<String> },
    /// 程序抛了（含语法错误）
    Error { message: String, logs: Vec<String> },
}

/// 解析 Node 的一行输出。
///
/// 认不出来的行返回 `None` 而不是报错：Node 自己会往 stdout 打警告
/// （`ExperimentalWarning` 之类），那些不是协议消息，不该让整个执行失败。
pub fn parse_line(line: &str) -> Option<NodeMsg> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let ty = v.get("type")?.as_str()?;
    let logs = || {
        v.get("logs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    match ty {
        "call" => Some(NodeMsg::Call {
            id: v.get("id")?.as_u64()?,
            name: v.get("name")?.as_str()?.to_string(),
            args: v.get("args").cloned().unwrap_or(Value::Null),
        }),
        "done" => Some(NodeMsg::Done {
            value: v.get("value").cloned().unwrap_or(Value::Null),
            logs: logs(),
        }),
        "error" => Some(NodeMsg::Error {
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("（程序失败，无错误信息）")
                .to_string(),
            logs: logs(),
        }),
        _ => None,
    }
}

/// 父进程 → Node 的一条结果。
pub fn result_line(id: u64, name: &str, outcome: Result<Value, String>) -> String {
    let msg = match outcome {
        Ok(value) => json!({ "type": "result", "id": id, "name": name, "ok": true, "value": value }),
        Err(error) => json!({ "type": "result", "id": id, "name": name, "ok": false, "error": error }),
    };
    msg.to_string()
}

// ---------------------------------------------------------------------------
// 结果呈现
// ---------------------------------------------------------------------------

/// 把一次 PTC 执行的结果渲染成给模型看的文本。
///
/// 排版刻意贴近模型自己写的程序：先 logs（程序主动打印的），后 return 值。
/// 两者都空时给一句明确的话 —— 空结果最容易让模型以为"工具坏了"。
pub fn render_result(logs: &[String], value: &Value, truncated: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !logs.is_empty() {
        parts.push(logs.join("\n"));
    }
    if !value.is_null() {
        let rendered = match value {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        };
        parts.push(rendered);
    }
    if parts.is_empty() {
        parts.push("(run_code 执行完成，程序没有输出)".into());
    }
    let mut out = parts.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n\n⚠️ 程序输出超过 {} KB，已截断。下次请在程序里先汇总，只 return 结论。",
            MAX_OUTPUT_BYTES / 1024
        ));
    }
    out
}

/// 按字节上限截断（UTF-8 安全：不切断多字节字符）。
pub fn truncate_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

// ---------------------------------------------------------------------------
// 驱动一次执行
// ---------------------------------------------------------------------------

/// 把 Node 的原始失败信息翻译成**可行动**的提示。
///
/// 目前只翻译一种：旧版 Node 不支持原生类型擦除时报
/// `Unknown file extension ".ts"` —— 那句话完全看不出"是 Node 太老"，
/// 用户（和模型）会以为是自己代码写错了，然后反复改一段本来正确的程序。
///
/// 抽成独立函数是为了**可测**：这条提示的价值全在"能不能命中真实的错误串"，
/// 而真实的错误串只有真起一个老 Node 才拿得到。
pub fn explain_node_error(message: &str) -> String {
    if message.contains("Unknown file extension") && message.contains(".ts") {
        return format!(
            "{message}\n\n⚠️ 这通常说明本机 Node 版本过旧，不支持**原生 TypeScript \
             类型擦除**（PTC 靠它直接跑 .ts，不需要 tsc）。请升级 Node，\
             或用 ORBCAT_NODE 环境变量指向较新的 node.exe。"
        );
    }
    message.to_string()
}

/// 子调用执行器：把一个 (工具名, 参数) 变成结果。
///
/// 为什么是 trait 而不是直接调 `tools::execute`：`ptc.rs` 不该知道
/// `ToolCtx` / 权限网关 / MCP 注册表的细节 —— 那些是 `tools.rs` 的事。
/// 这里只要一个"能执行工具"的能力，测试里就能塞一个假的进去，
/// 不必为了测协议去搭一整套权限环境。
///
/// ⚠️ 用 `async fn` + **泛型**而不是 `dyn`：trait 里的 `async fn`
/// 不是 dyn-compatible（要 dyn 就得引入 `async-trait` 依赖，或手写
/// `Pin<Box<dyn Future>>`）。泛型化没有这个代价，而调用方本来就只有一个。
///
/// `+ Send` 是必须的：`run_program` 整体要能跨线程（它跑在 tauri 的
/// 多线程 runtime 上，`#[tauri::command]` 的 future 要求 `Send`）。
pub trait SubCall {
    /// 执行一次子调用。`Err` = 工具失败（会成为程序里的 reject）。
    fn call(
        &self,
        name: &str,
        args: &Value,
    ) -> impl std::future::Future<Output = Result<Value, String>> + Send;
}

/// 一次执行的产物。
pub struct RunOutcome {
    /// 给模型看的文本（已按上限截断）
    pub text: String,
    /// 子调用次数（用于进度展示与测试断言）
    pub calls: usize,
    /// 是否成功（程序抛异常 = false）
    pub ok: bool,
}

/// 主循环一次迭代"醒来"的原因。
///
/// 为什么需要它：`select!` 的几个分支返回类型不同（`io::Result<Option<String>>`
/// 和 `Option<()>`），直接 match 表达不了。收成一个枚举后，
/// "为什么醒来"与"醒来做什么"就分开了。
enum Event {
    /// Node 来了一行（或读到 EOF / 出错）
    Line(std::io::Result<Option<String>>),
    /// 一个子调用跑完并回写了结果
    SubDone,
}

/// 跑一次 PTC 程序，把子调用派回 `sub`。
///
/// ## 并发模型（为什么不用 `tokio::spawn`）
///
/// 子调用是**并发**跑的（程序里的 `Promise.all` 要真的同时飞），但这里用
/// `FuturesUnordered` 在**当前任务内**驱动，而不是 `spawn` 出去。
///
/// 理由：`spawn` 要求 `'static`，而生产环境的 `sub` 持有 `ToolCtx<'_>`
/// （借用权限网关、MCP 注册表、会话 id…）—— 那些全是借用，包不成 `'static`。
/// 为了 spawn 去把 `ToolCtx` 整个改成 `Arc` 会污染一大片无关代码；
/// 而在作用域内并发根本不需要 `'static`。
///
/// 副作用（好的那种）：子调用失败时不会变成"孤儿任务"，
/// 主循环一返回它们就被 drop，工具执行随之取消。
///
/// ## 并发上限
///
/// 用 `Semaphore`（[`MAX_PARALLEL`]）而不是"数一数在飞几个"：
/// 许可由每个子调用自己 await，于是"排队"这件事对主循环是透明的 ——
/// 主循环照常读下一行，不会因为池子满了就停止读 stdout（那会让 Node 阻塞在写）。
///
/// ## 为什么必须限制子调用总数
///
/// 程序里的循环是模型写的。写错一个 `while` 条件就是无限调用，
/// 而且**不会自己停**（每次调用都合法、都成功）。所以超限时直接
/// 终止子进程并把错误回给模型。
pub async fn run_program<S: SubCall + Sync>(
    node: &Path,
    work: &WorkDir,
    sub: &S,
    cancel: Option<crate::shell::CancelFn>,
    timeout_secs: u64,
) -> Result<RunOutcome, String> {
    use futures_util::stream::{FuturesUnordered, StreamExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut cmd = tokio::process::Command::new(node);
    cmd.arg(work.bootstrap())
        .arg(work.program())
        .current_dir(&work.dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 Node 失败（{}）: {e}", node.display()))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "拿不到 Node 的 stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "拿不到 Node 的 stdout".to_string())?;
    let stderr = child.stderr.take();

    // stderr 单独收着：Node 的语法错误报告全在这里，程序崩了要把它给模型看。
    let stderr_buf: std::sync::Arc<tokio::sync::Mutex<String>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    if let Some(mut e) = stderr {
        let buf = stderr_buf.clone();
        tokio::spawn(async move {
            let mut s = String::new();
            use tokio::io::AsyncReadExt;
            let _ = e.read_to_string(&mut s).await;
            *buf.lock().await = s;
        });
    }

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL));
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // 回写 stdin 要串行：多个子调用同时写会**交错**，Node 那边按行 JSON 解析
    // 就会读到半截（`{"type":"res` + `ult"...`）→ 解析失败 → 程序永久挂起。
    let write_lock = std::sync::Arc::new(tokio::sync::Mutex::new(stdin));

    let mut lines = BufReader::new(stdout).lines();
    let mut done: Option<NodeMsg> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.clamp(1, 3600));

    // 在飞的子调用。每个 future 自己拿许可、执行、回写结果。
    // `FuturesUnordered` 让它们真正并发，且**不需要 'static**（见函数文档）。
    //
    // ⚠️ 显式写 `+ Send`：不写的话这个 trait object 默认不 Send，
    //    整个 `run_program` 的 future 就跟着不 Send —— 而它跑在 tauri 的
    //    多线程 runtime 上（`#[tauri::command]` 要求 future 是 Send）。
    //    报错会出现在**离这里很远**的地方（lib.rs 的 generate_handler!），
    //    所以这里写明白。
    let mut inflight: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>,
    > = FuturesUnordered::new();

    // 取消探针：用户点「停止」时杀掉 Node（kill_on_drop 会在 drop 时收尾）
    let cancel_probe = {
        let c = cancel.clone();
        async move {
            match c {
                Some(f) => loop {
                    if f() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                },
                None => std::future::pending::<()>().await,
            }
        }
    };
    tokio::pin!(cancel_probe);

    loop {
        // ⚠️ 三个分支必须都参与 select：只读 stdout 会在
        //    "子调用结果还没回写 → Node 在等结果 → 不产生新行" 时死锁。
        //    `inflight` 非空时也要推进它们。
        let next = tokio::select! {
            r = lines.next_line() => Event::Line(r),
            Some(()) = inflight.next(), if !inflight.is_empty() => Event::SubDone,
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.start_kill();
                return Err(format!("run_code 超时（{timeout_secs}s）。程序可能陷入了死循环。"));
            }
            _ = &mut cancel_probe => {
                let _ = child.start_kill();
                return Err("run_code 已被用户停止。".into());
            }
        };

        let line = match next {
            Event::SubDone => continue, // 一个子调用跑完了，回去继续等
            Event::Line(Ok(Some(l))) => l,
            Event::Line(Ok(None)) => break, // EOF：程序退出了
            Event::Line(Err(e)) => return Err(format!("读 Node 输出失败: {e}")),
        };

        let Some(msg) = parse_line(&line) else {
            continue; // Node 自己的警告行，不是协议消息
        };

        match msg {
            NodeMsg::Done { .. } | NodeMsg::Error { .. } => {
                done = Some(msg);
                break;
            }
            NodeMsg::Call { id, name, args } => {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n > MAX_SUB_CALLS {
                    let _ = child.start_kill();
                    return Err(format!(
                        "run_code 的子调用超过上限（{MAX_SUB_CALLS} 次）—— 程序里的循环可能没有终止条件。\
                         请检查循环边界，或分批处理。"
                    ));
                }

                let permit = sem.clone().acquire_owned();
                let wl = write_lock.clone();
                // 把这个子调用推进在飞集合：它自己等许可、执行、回写。
                // 不在这里 await —— 那会把程序里的 Promise.all 退化成串行。
                inflight.push(Box::pin(async move {
                    // 许可在这里才真正获取：池子满时它排队，而不是阻塞主循环
                    let _permit = match permit.await {
                        Ok(p) => p,
                        Err(_) => return, // 信号量被关（不可能，除非主循环提前退出）
                    };
                    let outcome = sub.call(&name, &args).await;
                    let line = result_line(id, &name, outcome);
                    let mut guard = wl.lock().await;
                    let _ = guard.write_all(line.as_bytes()).await;
                    let _ = guard.write_all(b"\n").await;
                    let _ = guard.flush().await;
                }));
            }
        }
    }

    // 收到 done/error 后，仍要给在飞的子调用一个收尾机会：
    // 程序已经返回，但它的某次调用可能还在跑（比如 Promise.all 里被放弃的分支）。
    // 不等它们也行（结果没人要了），但**必须 drop 掉**才能释放许可与借用。
    drop(inflight);

    let _ = child.wait().await;

    let stderr_text = stderr_buf.lock().await.clone();

    match done {
        Some(NodeMsg::Done { value, logs }) => {
            let (text, truncated) = truncate_bytes(&render_result(&logs, &value, false), MAX_OUTPUT_BYTES);
            Ok(RunOutcome {
                text: if truncated {
                    render_result(&logs, &value, true)
                } else {
                    text
                },
                calls: calls.load(std::sync::atomic::Ordering::Relaxed),
                ok: true,
            })
        }
        Some(NodeMsg::Error { message, logs }) => {
            // 程序自己抛的异常：logs 仍要带上（崩之前打印的东西往往正是线索）
            let body = if logs.is_empty() {
                message
            } else {
                format!("{}\n\n（失败前程序打印的内容）\n{}", message, logs.join("\n"))
            };
            // 旧版 Node 不支持原生类型擦除时，报的是 "Unknown file extension \".ts\"" ——
            // 那句话完全看不出"是 Node 太老"，所以在这里翻译成人话。
            let body = explain_node_error(&body);
            let (text, _) = truncate_bytes(&body, MAX_OUTPUT_BYTES);
            Ok(RunOutcome {
                text: format!("run_code 失败：\n{text}"),
                calls: calls.load(std::sync::atomic::Ordering::Relaxed),
                ok: false,
            })
        }
        None => {
            // 没等到 done/error 就 EOF —— 通常是**语法错误**：
            // Node 解析 .ts 失败时会在 import 阶段抛，而那时我们的 try/catch
            // 已经装好，正常会走 error 分支。走到这里基本是进程被外部杀了。
            let detail = if stderr_text.trim().is_empty() {
                "（Node 没有输出任何错误信息）".to_string()
            } else {
                stderr_text.trim().chars().take(4000).collect()
            };
            Ok(RunOutcome {
                text: format!(
                    "run_code 失败：程序没有正常结束（可能被中断，或 Node 启动失败）。\n\n{detail}"
                ),
                calls: calls.load(std::sync::atomic::Ordering::Relaxed),
                ok: false,
            })
        }
        // 循环里只在 Done/Error 时 `break`，所以 done 不可能是 Call。
        // 用 unreachable 而不是 `_ =>`：万一将来有人加了新的 break 路径，
        // 这里会当场炸出来，而不是静默当成"失败"。
        Some(NodeMsg::Call { .. }) => unreachable!("done 只可能是 Done / Error"),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_type_maps_common_schemas() {
        assert_eq!(ts_type(&json!({"type": "string"})), "string");
        assert_eq!(ts_type(&json!({"type": "integer"})), "number");
        assert_eq!(ts_type(&json!({"type": "boolean"})), "boolean");
        assert_eq!(
            ts_type(&json!({"type": "array", "items": {"type": "string"}})),
            "string[]"
        );
        // 认不出来的一律 unknown，不编造
        assert_eq!(ts_type(&json!({})), "unknown");
        assert_eq!(ts_type(&json!({"type": "weird"})), "unknown");
    }

    #[test]
    fn ts_type_renders_object_with_optionality() {
        let s = json!({
            "type": "object",
            "properties": { "path": {"type": "string"}, "limit": {"type": "integer"} },
            "required": ["path"]
        });
        let t = ts_type(&s);
        assert!(t.contains("path: string"), "{t}");
        assert!(t.contains("limit?: number"), "可选参数必须带问号: {t}");
    }

    /// 键名不是合法标识符时必须加引号，否则生成的是**语法错误**的 TS。
    #[test]
    fn ts_type_quotes_exotic_property_names() {
        let s = json!({
            "type": "object",
            "properties": { "my-key": {"type": "string"} },
            "required": ["my-key"]
        });
        assert!(ts_type(&s).contains("\"my-key\""), "{}", ts_type(&s));
    }

    /// `mcp__组__工具` 是合法标识符（下划线），不该被加引号 —— 加了反而难看且非必要。
    #[test]
    fn ts_member_keeps_underscore_names_bare() {
        assert_eq!(ts_member("mcp__github__create_issue"), "mcp__github__create_issue");
        assert_eq!(ts_member("read_file"), "read_file");
    }

    /// 带 `-` 的名字必须走引号访问，否则模型照抄会得到语法错误。
    #[test]
    fn ts_member_quotes_dashed_names() {
        assert_eq!(ts_member("my-tool"), "[\"my-tool\"]");
        assert_eq!(ts_member("2fast"), "[\"2fast\"]");
    }

    #[test]
    fn sdk_declarations_render_tools() {
        let tools = vec![
            (
                "read_file".to_string(),
                "读取一个文本文件。\n第二行不该出现".to_string(),
                json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            ),
        ];
        let d = sdk_declarations(&tools);
        assert!(d.starts_with("declare const tools:"), "{d}");
        assert!(d.contains("read_file: (args: { path: string }) => Promise<unknown>;"), "{d}");
        // 只取描述第一行（多行描述会把声明撑得很难读）
        assert!(d.contains("读取一个文本文件。"), "{d}");
        assert!(!d.contains("第二行"), "多行描述只保留第一行: {d}");
    }

    /// 描述里出现 `*/` 会**提前结束注释**，让后面所有声明变成语法错误。
    #[test]
    fn sdk_declarations_escapes_comment_terminator() {
        let tools = vec![(
            "t".to_string(),
            "危险 */ 结束".to_string(),
            json!({"type":"object","properties":{}}),
        )];
        let d = sdk_declarations(&tools);
        // 注释块内不能出现未转义的 */
        let comment = d.lines().find(|l| l.trim_start().starts_with("/**")).unwrap();
        assert!(!comment.contains("*/ 结束"), "注释终结符必须转义: {comment}");
    }

    #[test]
    fn sdk_declarations_handles_empty_tool_list() {
        let d = sdk_declarations(&[]);
        assert!(d.contains("Record<string, (args: unknown) => Promise<unknown>>"), "{d}");
    }

    #[test]
    fn behavior_section_mentions_the_key_contracts() {
        let s = behavior_section("declare const tools: {};");
        // 三条必须出现：只有 run_code 可直呼、并发写法、什么回到上下文
        assert!(s.contains("run_code"), "必须点明唯一可直接调用的工具");
        assert!(s.contains("Promise.all"), "必须教并发写法");
        assert!(s.contains("只有 `return` 的返回值和你 `console.log` 的内容会回到上下文"));
        assert!(s.contains("declare const tools: {};"), "SDK 声明要嵌进去");
    }

    #[test]
    fn wrap_program_makes_an_importable_module() {
        let w = wrap_program("return await tools.read_file({ path: 'x' });");
        assert!(w.starts_with("export default async function () {"), "{w}");
        assert!(w.trim_end().ends_with('}'));
        // 模型写的 body 原样在内（含 return，顶层 return 在 ESM 里是非法的）
        assert!(w.contains("return await tools.read_file({ path: 'x' });"));
    }

    /// 前缀恰好一行 —— 报错行号才只偏 1，模型能自己对回去。
    #[test]
    fn wrap_program_prefix_is_exactly_one_line() {
        let w = wrap_program("const a = 1;");
        let body_line = w.lines().nth(1).unwrap();
        assert_eq!(body_line, "const a = 1;");
    }

    #[test]
    fn parse_line_reads_call() {
        let m = parse_line(r#"{"type":"call","id":7,"name":"read_file","args":{"path":"a"}}"#);
        match m {
            Some(NodeMsg::Call { id, name, args }) => {
                assert_eq!(id, 7);
                assert_eq!(name, "read_file");
                assert_eq!(args["path"], "a");
            }
            other => panic!("应解析出 Call，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_line_reads_done_and_error() {
        match parse_line(r#"{"type":"done","value":{"n":1},"logs":["hi"]}"#) {
            Some(NodeMsg::Done { value, logs }) => {
                assert_eq!(value["n"], 1);
                assert_eq!(logs, vec!["hi"]);
            }
            other => panic!("应解析出 Done，得到 {other:?}"),
        }
        match parse_line(r#"{"type":"error","message":"炸了","logs":[]}"#) {
            Some(NodeMsg::Error { message, .. }) => assert_eq!(message, "炸了"),
            other => panic!("应解析出 Error，得到 {other:?}"),
        }
    }

    /// Node 自己会往 stdout 打警告 —— 那不是协议消息，不能让执行失败。
    #[test]
    fn parse_line_ignores_foreign_lines() {
        assert!(parse_line("(node:123) ExperimentalWarning: Type Stripping is an experimental feature").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("{ not json").is_none());
        // 有 type 但不认识的，也忽略
        assert!(parse_line(r#"{"type":"whatever"}"#).is_none());
    }

    /// `id` 缺失的 call 是坏消息：回不了结果，程序会永久挂起。宁可丢弃。
    #[test]
    fn parse_line_rejects_call_without_id() {
        assert!(parse_line(r#"{"type":"call","name":"x","args":{}}"#).is_none());
    }

    #[test]
    fn result_line_encodes_both_outcomes() {
        let ok = result_line(1, "t", Ok(json!({"a": 1})));
        assert!(ok.contains(r#""ok":true"#), "{ok}");
        assert!(ok.contains(r#""value":{"a":1}"#), "{ok}");
        let err = result_line(2, "t", Err("炸了".into()));
        assert!(err.contains(r#""ok":false"#), "{err}");
        assert!(err.contains("炸了"), "{err}");
        // name 必须回传：Node 侧用它填 ToolCallError.toolName
        assert!(err.contains(r#""name":"t""#), "{err}");
    }

    #[test]
    fn render_result_shows_logs_then_value() {
        let out = render_result(&["第一行".into(), "第二行".into()], &json!({"n": 1}), false);
        assert!(out.starts_with("第一行\n第二行"), "{out}");
        assert!(out.contains("\"n\": 1"), "{out}");
    }

    #[test]
    fn render_result_says_so_when_empty() {
        let out = render_result(&[], &Value::Null, false);
        assert!(out.contains("没有输出"), "{out}");
    }

    /// 字符串返回值直接呈现，不套 JSON 引号 —— 模型 return 一段报告时更好读。
    #[test]
    fn render_result_renders_bare_string_without_quotes() {
        let out = render_result(&[], &json!("一份报告"), false);
        assert_eq!(out, "一份报告");
    }

    #[test]
    fn render_result_notes_truncation() {
        let out = render_result(&["x".into()], &Value::Null, true);
        assert!(out.contains("已截断"), "{out}");
    }

    /// 截断必须落在字符边界上，否则会产生**非法 UTF-8**（中文被劈成半个）。
    #[test]
    fn truncate_bytes_is_utf8_safe() {
        let s = "中文中文中文"; // 每字 3 字节
        let (out, cut) = truncate_bytes(s, 7);
        assert!(cut);
        assert_eq!(out, "中文"); // 7 落在第二个字中间 → 退到 6
        assert!(s.starts_with(&out));
    }

    #[test]
    fn truncate_bytes_passes_short_input_through() {
        let (out, cut) = truncate_bytes("abc", 10);
        assert_eq!(out, "abc");
        assert!(!cut);
    }

    #[test]
    fn wrap_program_handles_empty_body() {
        let w = wrap_program("");
        assert!(w.starts_with("export default async function () {"));
        assert!(w.trim_end().ends_with('}'));
    }

    // -----------------------------------------------------------------------
    // 端到端：真起 Node 子进程
    // -----------------------------------------------------------------------
    //
    // 这些测试**真的会 spawn node**。为什么值得：整套 PTC 的价值全在
    // "Rust ↔ Node 的行协议 + 类型擦除 + 并发派发"这三件事能不能真的对上，
    // 而它们**没有一个**能被纯函数单测覆盖 —— 假的 Node 只会证明假的东西。
    //
    // 没有 node 就跳过（CI / 别人机器上不该因此变红）。

    /// 假工具：名字 → 固定结果。`boom` 一定失败（测 try/catch）。
    struct FakeSub {
        /// 记录被调用的顺序（测并发）
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl SubCall for FakeSub {
        async fn call(&self, name: &str, args: &Value) -> Result<Value, String> {
            self.seen.lock().unwrap().push(name.to_string());
            if name == "boom" {
                return Err("工具执行失败：故意炸".into());
            }
            // 模拟"读"：返回参数里的 i，加个前缀便于断言
            Ok(json!({ "echo": args.get("i").cloned().unwrap_or(Value::Null) }))
        }
    }

    fn node_or_skip() -> Option<PathBuf> {
        resolve_node()
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("orbcat_ptc_{tag}_{stamp}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 最基本的：程序 return 一个值，父进程能拿到。
    #[test]
    fn end_to_end_returns_value() {
        let Some(node) = node_or_skip() else {
            eprintln!("跳过：本机没有 node");
            return;
        };
        let d = tmp_dir("basic");
        let work = WorkDir::create(&d, "return { hello: '世界' };").unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .expect("run_program 不该 Err");
        assert!(out.ok, "{}", out.text);
        assert!(out.text.contains("世界"), "{}", out.text);
        assert_eq!(out.calls, 0, "没调工具就不该有子调用");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **类型注解必须能用** —— 这是选 Node 类型擦除的全部理由。
    /// 若 Node 版本不支持，模型写的 `const n: number` 会当场语法错误。
    #[test]
    fn end_to_end_supports_typescript_annotations() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("ts");
        let code = "const n: number = 41;\nconst xs: string[] = ['a', 'b'];\nreturn { n: n + 1, xs };";
        let work = WorkDir::create(&d, code).unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(out.ok, "类型注解必须被擦除而不是报错: {}", out.text);
        assert!(out.text.contains("42"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 工具调用：程序里 `tools.read(...)` → 父进程执行 → 结果回程序。
    #[test]
    fn end_to_end_dispatches_tool_calls() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("call");
        let code = "const r = await tools.read({ i: 5 });\nreturn r;";
        let work = WorkDir::create(&d, code).unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(out.ok, "{}", out.text);
        assert_eq!(out.calls, 1);
        assert!(out.text.contains("\"echo\": 5"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 循环 + 并发：模型写 `Promise.all` 时，多个调用要真的同时飞。
    /// 这条是 PTC 相对 native 的核心卖点，必须有测试钉住。
    #[test]
    fn end_to_end_runs_concurrent_calls() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("par");
        let code = "const xs = await Promise.all([1,2,3].map((i) => tools.read({ i })));\nreturn xs;";
        let work = WorkDir::create(&d, code).unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(out.ok, "{}", out.text);
        assert_eq!(out.calls, 3, "三个调用都要派出去");
        // 结果按 Promise.all 的**入参顺序**回来（并发不等于乱序结果）
        let t = &out.text;
        let p1 = t.find("\"echo\": 1").expect("缺 1");
        let p2 = t.find("\"echo\": 2").expect("缺 2");
        let p3 = t.find("\"echo\": 3").expect("缺 3");
        assert!(p1 < p2 && p2 < p3, "Promise.all 的结果顺序必须是入参顺序: {t}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 失败的子调用让 Promise reject；程序 `try/catch` 后能继续跑。
    /// 这条对应"个别文件读失败也要把报告写完"。
    #[test]
    fn end_to_end_tool_failure_is_catchable() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("catch");
        let code = "let caught = null;\n\
                    try { await tools.boom({}); } catch (e) { caught = e.toolName + '|' + e.message; }\n\
                    const after = await tools.read({ i: 9 });\n\
                    return { caught, after };";
        let work = WorkDir::create(&d, code).unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(out.ok, "被 catch 住的失败不该让整个程序失败: {}", out.text);
        assert!(out.text.contains("boom|"), "ToolCallError 要带 toolName: {}", out.text);
        assert!(out.text.contains("故意炸"), "{}", out.text);
        assert!(out.text.contains("\"echo\": 9"), "失败后必须能继续调工具: {}", out.text);
        assert_eq!(out.calls, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 没被 catch 的失败 → 程序失败，错误信息要回到模型（含堆栈）。
    #[test]
    fn end_to_end_uncaught_failure_reports_error() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("uncaught");
        let work = WorkDir::create(&d, "await tools.boom({});\nreturn 1;").unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(!out.ok, "未捕获的失败必须标记为失败");
        assert!(out.text.contains("run_code 失败"), "{}", out.text);
        assert!(out.text.contains("故意炸"), "要带上原始错误: {}", out.text);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 语法错误：必须报错而不是静默挂住（这条最容易做成死锁）。
    #[test]
    fn end_to_end_syntax_error_is_reported() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("syntax");
        let work = WorkDir::create(&d, "const = ;;;").unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(!out.ok);
        assert!(out.text.contains("run_code 失败"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 旧版 Node 不支持类型擦除时，报的是 `Unknown file extension ".ts"` ——
    /// 那句话看不出"是 Node 太老"。必须翻译成人话并指出怎么办。
    ///
    /// 这里**真的**用 `--no-strip-types` 复现出那个错误串，再喂给
    /// [`explain_node_error`] —— 而不是手写一个假错误。
    /// 假串只能证明"我能写字符串"，证明不了"提示能命中真实错误"。
    #[test]
    fn stale_node_error_is_translated_into_actionable_hint() {
        let Some(node) = node_or_skip() else {
            return;
        };
        // 先确认这个 Node 认识 --no-strip-types（不认识就没法复现，跳过）
        let supports = std::process::Command::new(&node)
            .arg("--no-strip-types")
            .arg("-e")
            .arg("0")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !supports {
            eprintln!("跳过：这个 Node 不认识 --no-strip-types");
            return;
        }

        let d = tmp_dir("oldnode");
        let work = WorkDir::create(&d, "const x: number = 1;\nreturn x;").unwrap();

        // 用"类型擦除被关掉"的 Node 跑同一个程序 → 拿到真实的错误输出
        let out = std::process::Command::new(&node)
            .arg("--no-strip-types")
            .arg(work.bootstrap())
            .arg(work.program())
            .current_dir(&work.dir)
            .output()
            .expect("起 Node 失败");
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();

        // 错误可能出现在 stdout（我们的 bootstrap 把 import 包在 try/catch 里，
        // 失败会走 error 分支回一条 JSON）或 stderr（Node 自己的诊断）。
        let raw = format!("{stdout}{stderr}");
        assert!(
            raw.contains("Unknown file extension") || raw.contains(".ts"),
            "预期拿到 .ts 相关的错误，实际 stdout={stdout:?} stderr={stderr:?}"
        );

        // ★ 真正要验的：翻译函数能命中这个真实错误串
        let explained = explain_node_error(&raw);
        assert_ne!(explained, raw, "真实的旧版 Node 错误必须被翻译（提示没命中）");
        assert!(
            explained.contains("类型擦除"),
            "翻译后要说明是类型擦除的问题: {explained}"
        );
        assert!(
            explained.contains("ORBCAT_NODE"),
            "要给出可行动的出路（怎么指到新 Node）: {explained}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 翻译函数不该乱改无关的错误（否则真正的错误信息会被盖掉）。
    #[test]
    fn explain_node_error_leaves_unrelated_errors_alone() {
        let plain = "SyntaxError: Unexpected token ';'";
        assert_eq!(explain_node_error(plain), plain);

        // 提到 .ts 但**不是**扩展名错误的，也不该被翻译
        let other = "Error: cannot find module './foo.ts'";
        assert_eq!(explain_node_error(other), other);

        // 提到扩展名但不是 .ts 的（比如 .jsx），也不该翻译
        let jsx = "Unknown file extension \".jsx\" for x.jsx";
        assert_eq!(explain_node_error(jsx), jsx);
    }

    /// `console.log` 的内容要回到模型（它是程序主动要"说话"的唯一通道）。
    #[test]
    fn end_to_end_captures_console_log() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("log");
        let work = WorkDir::create(&d, "console.log('进度 50%');\nreturn 'done';").unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 60))
            .unwrap();
        assert!(out.ok, "{}", out.text);
        assert!(out.text.contains("进度 50%"), "{}", out.text);
        assert!(out.text.contains("done"), "{}", out.text);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 死循环必须被上限掐断，而不是把 orbcat 拖死。
    /// 用 while(true) 调工具 —— 每次调用都合法、都成功，只有计数能拦住它。
    #[test]
    fn end_to_end_infinite_loop_is_stopped() {
        let Some(node) = node_or_skip() else {
            return;
        };
        let d = tmp_dir("loop");
        let work = WorkDir::create(&d, "while (true) { await tools.read({ i: 1 }); }\nreturn 1;").unwrap();
        let sub = FakeSub {
            seen: Default::default(),
        };
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_program(&node, &work, &sub, None, 120));
        match out {
            Err(e) => assert!(e.contains("上限"), "应是子调用上限错误: {e}"),
            Ok(o) => panic!("死循环必须被掐断，却正常返回了: {}", o.text),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `WorkDir` 析构要真的删掉目录（否则每次 run_code 都留一堆垃圾）。
    #[test]
    fn workdir_is_cleaned_on_drop() {
        let d = tmp_dir("cleanup");
        let dir = {
            let work = WorkDir::create(&d, "return 1;").unwrap();
            assert!(work.program().exists());
            work.dir.clone()
        };
        assert!(!dir.exists(), "WorkDir drop 后目录应被删除: {}", dir.display());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 🔴 **回归测试**：工作目录里必须有 `package.json` 且 `type: module`。
    ///
    /// 为什么这条单独钉住：`.ts` 用 ESM 还是 CJS 解析取决于**沿目录树往上
    /// 找到的第一个 package.json**。用户家目录里一个 `{"type":"commonjs"}`
    /// （实测存在）就会让 `export default` 报语法错误，而报错行指向我们生成的
    /// 包装代码 —— 看起来像 orbcat 自己坏了。删掉这个文件测试不会立刻变红
    /// （取决于跑测试时的 cwd），所以显式断言它的存在与内容。
    #[test]
    fn workdir_declares_esm_so_ts_parses() {
        let d = tmp_dir("pkgjson");
        let work = WorkDir::create(&d, "return 1;").unwrap();
        let pkg = work.dir.join("package.json");
        assert!(pkg.exists(), "缺 package.json 时 .ts 可能被当 CJS 解析");
        let txt = std::fs::read_to_string(&pkg).unwrap();
        let v: Value = serde_json::from_str(&txt).expect("package.json 必须是合法 JSON");
        assert_eq!(
            v.get("type").and_then(Value::as_str),
            Some("module"),
            "必须是 type=module，否则 export default 会语法错误: {txt}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 程序写坏了也不该让 `WorkDir` 漏在磁盘上。
    #[test]
    fn workdir_drop_cleans_even_after_error() {
        let d = tmp_dir("cleanup2");
        let dir = {
            let work = WorkDir::create(&d, "const = ;").unwrap();
            work.dir.clone()
        };
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Node 探测：本机应有 node（否则 PTC 根本不可用，要能报出来）。
    /// 不断言一定有 —— 别人机器上可能真没装。
    #[test]
    fn resolve_node_finds_something_or_is_none() {
        match resolve_node() {
            Some(p) => assert!(p.is_file(), "返回的路径必须是文件: {}", p.display()),
            None => eprintln!("本机没有探测到 node"),
        }
    }
}
