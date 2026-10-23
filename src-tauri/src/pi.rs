//! pi 内核客户端 —— 用 `pi --mode rpc` 子进程驱动 pi，做 orbcat 的 agent 循环。
//!
//! ## 为什么是子进程而不是 SDK
//!
//! pi 的 SDK 必须在 Node/Bun 进程内跑（`docs/sdk.md:3`），Rust 侧用 SDK 只会多一跳 IPC。
//! RPC 是官方给**语言无关宿主**的正式接口（`docs/rpc.md:3-9`），所以走子进程。
//!
//! ## 协议要点（全部实测过，见 docs/06-RUNTIME-ASSETS.md）
//!
//! 1. **严格 LF 分隔 JSONL**。不能用 Node 的 `readline`（它会在 `U+2028`/`U+2029` 上切分，
//!    而这两个字符在 JSON 字符串里合法）。这里用 `tokio::io::BufReader::read_until(b'\n')`，
//!    它只认 LF，天然安全。
//! 2. **行尾的 `\r` 要剥掉**（pi 接受 CRLF 输入）。
//! 3. **stdout 专供协议**，诊断走 stderr —— 不要把 stderr 当协议解析。
//! 4. **必须持续读 stdout**，否则管道缓冲满会把 pi 卡死。
//! 5. **命令用 `id` 关联**，不要按响应顺序配对（处理是异步的）。
//! 6. **`agent_settled` 才是"自动工作结束"信号**，`agent_end` 不是（后面可能还有
//!    重试 / 溢出恢复 / 压缩 / 插话）。
//! 7. **发 prompt 之前就要装好事件监听**，否则会漏掉快速完成的事件。
//!
//! ## 权限决策怎么留在 Rust
//!
//! pi **没有任何内置权限机制**（`README.md:90`）。唯一的拦截点是 extension 的
//! `tool_call` 事件 → `ctx.ui.confirm()` → RPC 抛 `extension_ui_request{method:"confirm"}`
//! 给宿主 → 宿主回 `extension_ui_response{confirmed:bool}`。
//!
//! 本模块把这个往返做成 [`PiHost::confirm`] 回调，Rust 侧照常弹自己的权限卡。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// pi 子进程的一个事件（已从 JSONL 解析出的原始记录）。
pub type PiEvent = Value;

/// pi 的 **8 个内置工具**（上游 `createAllToolDefinitions`）。
///
/// ## 为什么必须全部排除
///
/// 这 8 个工具**原生执行**，**不经过** orbcat 的 `permission.rs` 文件网关与
/// `command_policy.rs` 命令策略 —— 留着它们 = orbcat 的权限体系出现旁路，
/// 直接违反 `docs/00-CONCEPT.md` 的决策 7（三层闸门）与决策 9（命令三路）。
///
/// 实测（2026-10-23，见 `docs/explore/pi-upstream-context-interaction.md` §二 2.6）：
/// pi 的 `bash` RPC **真的执行了命令**并返回结果（78ms）。
///
/// ## 上游机制（读 bundle 源码确认）
///
/// - `_isAllowedTool(name)`：`excludedTools` 命中 → **直接 false**（优先级最高）；
///   否则看 `allowedTools`（`--tools` 白名单）；再否则看是不是 MCP 且白名单不过滤 MCP。
/// - `_refreshToolRegistry`：`definitionRegistry` 与 `wrappedBuiltInTools` **都按
///   `_isAllowedTool` 过滤** ⇒ 被排除的工具**根本不在 registry 里**，
///   模型既不能直接调，`tool_search` / `codemode` 也**找不到**它。
/// - `_applyToolLoadout`：只从 registry 里取工具 ⇒ 排除 = 彻底消失。
///
/// ## orbcat 有全套等价物（都走网关）
///
/// `orb_read_file` / `orb_write_file` / `orb_edit_file` / `orb_grep_files` /
/// `orb_list_dir` / `orb_run_command` —— 全部经 `tools::execute` →
/// `authorize()` → 文件网关 + 命令策略 + 审计链。
///
/// 所以策略是：**8 个全排除**，用 orbcat 自己那套替代。
pub const PI_BUILTIN_TOOLS: &[&str] = &[
    "read", "bash", "powershell", "edit", "write", "grep", "find", "ls",
];

// ---------------------------------------------------------------------------
// 运行期资产定位
// ---------------------------------------------------------------------------

/// 内置运行时资产目录（`runtime/`）。
///
/// 位置规则：**exe 同级**的 `runtime/`（与 `agent-data/` 同一套规则）。
/// 开发态是 `target/debug/runtime/`，发布态是 exe 旁 `runtime/`。
pub fn runtime_dir(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?.join("runtime");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// 内置的 node.exe。
pub fn bundled_node(exe: &Path) -> Option<PathBuf> {
    let p = runtime_dir(exe)?.join("node").join("node.exe");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// 内置的 git.exe。
pub fn bundled_git(exe: &Path) -> Option<PathBuf> {
    let p = runtime_dir(exe)?.join("git").join("cmd").join("git.exe");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// 内置的 python.exe。
pub fn bundled_python(exe: &Path) -> Option<PathBuf> {
    let p = runtime_dir(exe)?.join("python").join("python.exe");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// 内置的 bash（**必须走 wrapper**，见 docs/06 §坑 1）。
///
/// 直接跑 `usr/bin/bash.exe` 时 `ls`/`head`/`cat` 会 `command not found`
/// —— bash 继承 Windows PATH，`/usr/bin` 的映射不保证排在前面。
/// `bash.cmd` 显式设了 PATH。
pub fn bundled_bash(exe: &Path) -> Option<PathBuf> {
    let p = runtime_dir(exe)?.join("git").join("bash.cmd");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// pi 启动配置。
#[derive(Debug, Clone)]
pub struct PiConfig {
    /// 内置 runtime 目录（`runtime/`）。
    pub runtime: PathBuf,
    /// pi 的 agent 目录（`PI_CODING_AGENT_DIR`）—— 存 auth.json / sessions / models.json。
    pub agent_dir: PathBuf,
    /// 会话目录（`--session-dir`）。
    pub session_dir: Option<PathBuf>,
    /// 工作目录（cwd）。
    pub cwd: PathBuf,
    /// `--provider`
    pub provider: Option<String>,
    /// `--model`
    pub model: Option<String>,
    /// `--tools` 白名单。**注意是替换语义** —— 不列全会把内置工具一起干掉。
    pub tools: Vec<String>,
    /// `--exclude-tools`：**要排除**的工具名（pi 1.4.0 支持，`-xt`）。
    ///
    /// ## 与 `tools`（`--tools` 白名单）是**两道独立的闸**
    ///
    /// | 参数 | 作用 | 优先级 |
    /// |---|---|---|
    /// | `--exclude-tools` | 命中即**从 registry 剔除** | **最高**（上游 `_isAllowedTool` 先判排除）|
    /// | `--tools` | 白名单外的工具**不存在** | 次之 |
    ///
    /// 为什么要两道：`--tools` 的语义若因 pi 升级而变（它已经有 `--no-tools` /
    /// `--no-builtin-tools` / `+name` / `-name` 好几种形态），`--exclude-tools`
    /// 这一道仍然拦得住。**安全闸要独立于会被改动的那个**。
    ///
    /// 默认值 = [`PI_BUILTIN_TOOLS`] 全量。
    pub exclude_tools: Vec<String>,
    /// `--system-prompt` / `--append-system-prompt`
    pub append_system_prompt: Vec<String>,
    /// 额外参数（逃生口）
    pub extra_args: Vec<String>,
    /// 离线模式（`PI_OFFLINE=1`）—— 禁掉模型目录自动刷新。
    pub offline: bool,
    /// 工具桥端口（`ORB_BRIDGE_PORT`）—— extension 用它回调 Rust。
    pub bridge_port: u16,
    /// 工具清单（`ORB_TOOL_SPECS`）—— extension 用它注册工具。
    pub tool_specs: Value,
}

impl PiConfig {
    /// 从 orbcat 的数据目录 + 运行时目录拼出默认配置。
    pub fn new(runtime: PathBuf, agent_dir: PathBuf, cwd: PathBuf) -> Self {
        Self {
            runtime,
            agent_dir,
            session_dir: None,
            cwd,
            provider: None,
            model: None,
            tools: Vec::new(),
            // ⚠️ 默认就把 pi 的全部内置工具排掉 —— 这是**安全默认值**，
            // 不依赖调用方记得配。要放开得显式 `cfg.exclude_tools.clear()`。
            exclude_tools: PI_BUILTIN_TOOLS.iter().map(|s| s.to_string()).collect(),
            append_system_prompt: Vec::new(),
            extra_args: Vec::new(),
            offline: true,
            bridge_port: 0,
            tool_specs: Value::Object(Default::default()),
        }
    }

    /// pi 的入口脚本（`runtime/pi/dist/bundle/cli.js`）。
    fn entry(&self) -> PathBuf {
        self.runtime.join("pi").join("dist").join("bundle").join("cli.js")
    }

    /// 拼出完整命令行。
    fn argv(&self) -> Vec<String> {
        let mut v = vec![
            "--mode".into(),
            "rpc".into(),
            // ── 强制信任项目目录 ──
            //
            // pi 的「项目信任」机制：项目目录下有 `.pi/settings.json` /
            // `.pi/mcp.json` / `.pi/extensions` 等配置时，pi 会问用户
            // 「信任这个项目吗」（`core/project-trust.ts`）。
            //
            // ⚠️ 在 RPC 模式下**没有 UI** → 走 `ctx.ui.select()` 那条路
            // 会**永久阻塞**（没人能回答那个选择）。
            //
            // orbcat 已经有自己的一整套信任/权限模型（`permission.rs`
            // + `mcp_grants.json` + 确认卡），不需要 pi 再问一遍 ——
            // 所以直接用 `--approve` 强制信任，把这层交互掐掉。
            "--approve".into(),
        ];
        // ⚠️ **不要**加 `--no-session` ——
        // pi 的 `main.ts:364`：`noSession` 会走 `SessionManager.inMemory()`，
        // **`--session-dir` 被完全忽略** → pi 崩了上下文就没了。
        //
        // 之前的注释说「用 --session-dir 而不是 --no-session」，
        // 但 argv 里两个**同时**给了 —— 结果是 --no-session 赢，
        // 落盘根本没生效（2026-10-22 实测发现）。
        if let Some(d) = &self.session_dir {
            v.push("--session-dir".into());
            v.push(d.to_string_lossy().to_string());
        } else {
            // 没有 session 目录时才退回内存会话
            v.push("--no-session".into());
        }
        if let Some(p) = &self.provider {
            v.push("--provider".into());
            v.push(p.clone());
        }
        if let Some(m) = &self.model {
            v.push("--model".into());
            v.push(m.clone());
        }
        if !self.tools.is_empty() {
            v.push("--tools".into());
            v.push(self.tools.join(","));
        }
        // ⚠️ 排除闸：把 pi 的内置工具从 registry 里**整个剔除**。
        //
        // 上游：`--exclude-tools a,b` → `excludedToolNames` → `_excludedTools`
        // → `_isAllowedTool` 里**第一个**判 ⇒ 命中即 false ⇒ 不进 registry。
        //
        // ⚠️ **不能和 `--tools` 的 `+name` / `-name` 修饰符混用** ——
        // 上游 `getToolListError` 明确报 "tool names cannot be mixed with
        // +name or -name entries"。我们两个都不用（白名单是纯名字 + `mcp__*` 通配）。
        if !self.exclude_tools.is_empty() {
            v.push("--exclude-tools".into());
            v.push(self.exclude_tools.join(","));
        }
        for s in &self.append_system_prompt {
            v.push("--append-system-prompt".into());
            v.push(s.clone());
        }
        v.extend(self.extra_args.iter().cloned());
        v
    }
}

// ---------------------------------------------------------------------------
// 宿主回调
// ---------------------------------------------------------------------------

/// pi 需要宿主回答的事。
pub enum PiHostRequest {
    /// extension 请求弹一个确认框（`ctx.ui.confirm`）。
    ///
    /// `title` / `message` 是 pi 给的原文；`id` 是回执时要用的。
    Confirm { id: String, title: String, message: String },
    /// extension 请求选一个（`ctx.ui.select`）。
    Select {
        id: String,
        title: String,
        message: String,
        options: Vec<String>,
    },
    /// extension 要一段输入（`ctx.ui.input` / `editor`）。
    Input { id: String, title: String, message: String },
    /// 其它方法（`notify` / `setStatus` / `setWidget` / `setTitle`）：**不需要回答**。
    Ignored { id: String },
}

/// 宿主对 [`PiHostRequest`] 的回答。
pub enum PiHostReply {
    Confirm(bool),
    Select(Option<String>),
    Input(String),
    /// **稍后答复** —— 宿主正在弹确认卡（要等用户点，最多 3 分钟）。
    ///
    /// ## 为什么必须有这个变体
    ///
    /// [`PiHostFn`] 是**同步**签名，而 dispatch 跑在 pi 的 **stdout 读循环**里
    /// ⇒ 宿主要是就地等用户（几秒到 3 分钟），**读循环就被卡住** ⇒
    /// pi 写 stdout 会被背压卡住 ⇒ **死锁**（pi.rs 记过同族坑：
    /// 「等用户绝不能阻塞读循环」）。
    ///
    /// ⇒ 宿主拿到这个变体后**立即返回**（读循环继续跑），
    /// 真正的答复由宿主另起一个任务异步送来（`tokio::spawn` + oneshot）。
    Deferred(Box<DeferredReply>),
}

/// 延迟应答的接收端（pi.rs 侧 await 它）。
///
/// 宿主侧拿到的是**发送端**（[`DeferredReply::pair`] 的第二个返回值）。
pub struct DeferredReply {
    rx: tokio::sync::oneshot::Receiver<PiHostReply>,
}

impl DeferredReply {
    /// 造一对「延迟应答」：
    /// 第一个返回值给 pi.rs（它 await 这个），
    /// 第二个给宿主（将来把答复 `send` 进去）。
    pub fn pair() -> (Box<DeferredReply>, tokio::sync::oneshot::Sender<PiHostReply>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Box::new(DeferredReply { rx }), tx)
    }
}

/// 宿主答复 → pi 的应答 JSON。
///
/// 抽成独立函数：**同步答**与**延迟答**两条路共用同一份格式，
/// 免得以后改一边忘了另一边。
pub fn ui_reply_json(id: &str, reply: PiHostReply) -> Value {
    match reply {
        PiHostReply::Confirm(v) => {
            json!({"type": "extension_ui_response", "id": id, "confirmed": v})
        }
        PiHostReply::Select(v) => match v {
            Some(val) => json!({"type": "extension_ui_response", "id": id, "value": val}),
            None => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        },
        PiHostReply::Input(v) => json!({"type": "extension_ui_response", "id": id, "value": v}),
        // ⚠️ 延迟答**不该**走到这里 —— dispatch 会把它交给 spawn 出去的任务。
        // 兜底按「取消」回，绝不能让 pi 干等。
        PiHostReply::Deferred(_) => {
            json!({"type": "extension_ui_response", "id": id, "cancelled": true})
        }
    }
}

/// 延迟应答的后台任务 —— **「读循环零阻塞」这条硬指标的落点**。
pub fn spawn_deferred_reply<W, F>(id: String, d: Box<DeferredReply>, write: W)
where
    W: Fn(Value) -> F + Send + 'static,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let resp = match d.rx.await {
            Ok(r) => ui_reply_json(&id, r),
            // 宿主那边掉了（超时 / 取消 / 任务被中止）⇒ 回「取消」
            Err(_) => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        };
        write(resp).await;
    });
}

/// 宿主回调：pi 向宿主提问。
///
/// 返回 `None` 表示**不回答** —— pi 会一直等着（实测：整个 run 挂死）。
/// 所以**必须**有超时兜底，见 [`PiClient::confirm`]。
pub type PiHostFn = Arc<dyn Fn(PiHostRequest) -> Option<PiHostReply> + Send + Sync>;

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// pi 子进程客户端。
pub struct PiClient {
    child: tokio::sync::Mutex<Option<Child>>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    seq: AtomicU64,
    /// 事件广播
    events_tx: mpsc::UnboundedSender<PiEvent>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<PiEvent>>>,
    /// 命令 id → 一次性响应通道
    pending: Arc<tokio::sync::Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>>,
    /// 已收到的响应（按 id）
    responses: Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    aborted: Arc<AtomicBool>,
    /// 宿主回调
    host: PiHostFn,
    /// 运行时目录
    runtime: PathBuf,
}

impl PiClient {
    /// 启动 pi 子进程。
    pub async fn spawn(cfg: PiConfig, host: PiHostFn) -> Result<Arc<Self>, String> {
        let node = cfg
            .runtime
            .join("node")
            .join("node.exe")
            .is_file()
            .then(|| cfg.runtime.join("node").join("node.exe"))
            .ok_or_else(|| "内置 node 不存在：runtime/node/node.exe".to_string())?;
        let entry = cfg.entry();
        if !entry.is_file() {
            return Err(format!("pi 入口不存在：{}", entry.display()));
        }

        let mut cmd = Command::new(&node);
        cmd.arg(&entry);
        for a in cfg.argv() {
            cmd.arg(a);
        }
        cmd.current_dir(&cfg.cwd);
        cmd.env("PI_CODING_AGENT_DIR", &cfg.agent_dir);
        cmd.env("PI_OFFLINE", if cfg.offline { "1" } else { "0" });
        // 工具桥：extension 靠这两个环境变量找到 Rust
        if cfg.bridge_port > 0 {
            cmd.env("ORB_BRIDGE_PORT", cfg.bridge_port.to_string());
            cmd.env("ORB_TOOL_SPECS", cfg.tool_specs.to_string());
        }
        // ⚠️ 把内置 git/bash 挂到 PATH —— pi 的 bash 工具要用
        let git_bin = cfg.runtime.join("git");
        if git_bin.is_dir() {
            let usr = git_bin.join("usr").join("bin");
            let cmd_dir = git_bin.join("cmd");
            let mingw = git_bin.join("mingw64").join("bin");
            let new_path = format!("{};{};{};{}", usr.display(), cmd_dir.display(), mingw.display(), std::env::var("PATH").unwrap_or_default());
            cmd.env("PATH", new_path);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Windows 下隐藏控制台窗口
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW = 0x0800_0000 —— 避免弹控制台窗口
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }

        let mut child = cmd.spawn().map_err(|e| format!("启动 pi 失败: {e}"))?;
        let stdin = child.stdin.take().ok_or("拿不到 pi 的 stdin")?;
        let stdout = child.stdout.take().ok_or("拿不到 pi 的 stdout")?;
        let stderr = child.stderr.take().ok_or("拿不到 pi 的 stderr")?;

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let pending: Arc<tokio::sync::Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let responses: Arc<tokio::sync::Mutex<HashMap<String, Value>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let client = Arc::new(Self {
            child: tokio::sync::Mutex::new(Some(child)),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            seq: AtomicU64::new(0),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
            pending: pending.clone(),
            responses: responses.clone(),
            aborted: Arc::new(AtomicBool::new(false)),
            host,
            runtime: cfg.runtime.clone(),
        });

        // 读 stdout —— **必须持续读**，否则 pi 被背压卡死
        let c = client.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => break, // EOF：pi 退出了
                    Ok(_) => {}
                    Err(_) => break,
                }
                // 剥掉行尾可选的 \r（pi 接受 CRLF 输入）
                let line = {
                    let mut s = &buf[..];
                    if s.last() == Some(&b'\r') {
                        s = &s[..s.len() - 1];
                    }
                    s.to_vec()
                };
                if line.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                let v: Value = match serde_json::from_slice(&line) {
                    Ok(v) => v,
                    Err(_) => continue, // 不是 JSON —— 忽略（不该发生，但别让一条坏行毁掉整个流）
                };
                c.dispatch(v).await;
            }
        });

        // 读 stderr —— 只记日志，不当协议解析
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                let s = String::from_utf8_lossy(&buf);
                let s = s.trim();
                if !s.is_empty() {
                    eprintln!("[pi] {}", s);
                }
            }
        });

        Ok(client)
    }

    /// 处理一条记录：要么是命令响应，要么是事件，要么是 extension UI 请求。
    async fn dispatch(self: &Arc<Self>, v: Value) {
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        // ── 命令响应 ──
        if t == "response" {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                let id = id.to_string();
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let _ = tx.send(v.clone());
                } else {
                    self.responses.lock().await.insert(id, v.clone());
                }
            }
            return;
        }

        // ── extension UI 请求（权限往返的关键）──
        if t == "extension_ui_request" {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let message = v.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string();

            let req = match method {
                "confirm" => PiHostRequest::Confirm {
                    id: id.clone(),
                    title,
                    message,
                },
                "select" => {
                    let options = v
                        .get("options")
                        .and_then(|x| x.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|o| {
                                    o.get("label")
                                        .and_then(|l| l.as_str())
                                        .map(|s| s.to_string())
                                        .or_else(|| o.as_str().map(|s| s.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    PiHostRequest::Select {
                        id: id.clone(),
                        title,
                        message,
                        options,
                    }
                }
                "input" | "editor" => PiHostRequest::Input {
                    id: id.clone(),
                    title,
                    message,
                },
                _ => PiHostRequest::Ignored { id: id.clone() },
            };

            // 宿主不回答 → pi 会一直等（实测整个 run 挂死）→ **必须回一个取消**
            let reply = (self.host)(req);
            match reply {
                // ⚠️ **延迟答**：宿主正在弹确认卡（要等用户）。
                // ⇒ **绝不在读循环里等** —— 等下去会卡住 pi 写 stdout ⇒ 死锁。
                //    另起一个任务等答案，拿到后**另路**写回 pi。
                Some(PiHostReply::Deferred(d)) => {
                    let client = self.clone();
                    spawn_deferred_reply(id.clone(), d, move |resp| {
                        let c = client.clone();
                        async move {
                            let _ = c.write_line(&resp).await;
                        }
                    });
                    return;
                }
                Some(r) => {
                    let resp = ui_reply_json(&id, r);
                    let _ = self.write_line(&resp).await;
                }
                None => {
                    let resp =
                        json!({"type": "extension_ui_response", "id": id, "cancelled": true});
                    let _ = self.write_line(&resp).await;
                }
            }
            return;
        }

        // ── 其它都是事件 ──
        let _ = self.events_tx.send(v);
    }

    /// 写一行 JSONL 到 pi 的 stdin。
    async fn write_line(&self, v: &Value) -> Result<(), String> {
        let mut guard = self.stdin.lock().await;
        let mut s = match serde_json::to_string(v) {
            Ok(s) => s,
            Err(e) => return Err(format!("序列化命令失败: {e}")),
        };
        s.push('\n');
        let mut g = guard
            .as_mut()
            .ok_or_else(|| "pi 的 stdin 已关闭".to_string())?;
        g.write_all(s.as_bytes())
            .await
            .map_err(|e| format!("写 pi stdin 失败: {e}"))?;
        g.flush()
            .await
            .map_err(|e| format!("flush pi stdin 失败: {e}"))?;
        Ok(())
    }

    /// 发一条命令并等它的响应。
    pub async fn command(&self, mut cmd: Value, timeout_ms: u64) -> Result<Value, String> {
        let id = format!("c{}", self.seq.fetch_add(1, Ordering::Relaxed));
        cmd["id"] = json!(id);
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.pending.lock().await.insert(id.clone(), tx);

        if let Err(e) = self.write_line(&cmd).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), rx.recv()).await {
            Ok(Some(v)) => {
                if v.get("success").and_then(|x| x.as_bool()) == Some(false) {
                    let msg = v
                        .get("error")
                        .and_then(|x| x.as_str())
                        .unwrap_or("命令失败")
                        .to_string();
                    return Err(msg);
                }
                Ok(v.get("data").cloned().unwrap_or(Value::Null))
            }
            Ok(None) => Err("pi 的响应通道已关闭".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("命令超时（{}ms）", timeout_ms))
            }
        }
    }

    /// 订阅事件流。
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<PiEvent>> {
        self.events_rx.lock().unwrap().take()
    }

    /// 发一条 prompt（不等它跑完 —— 跑完靠 `agent_settled` 事件）。
    pub async fn prompt(&self, message: &str, images: Vec<String>) -> Result<(), String> {
        let mut cmd = json!({"type": "prompt", "message": message});
        if !images.is_empty() {
            // ⚠️ **必须转成 pi 的 ImageContent** —— 直接送 data URL 字符串会让
            // 整条 prompt 被 pi 拒（实测：success:false，模型收到 0 次请求）。
            // 见 [`to_pi_images`]。
            cmd["images"] = json!(to_pi_images(&images)?);
        }
        self.command(cmd, 30_000).await.map(|_| ())
    }

    /// 插入一条插话（排队，不打断当前轮）。
    ///
    /// `images` 传 orbcat 的 data URL 字符串，这里统一转成 pi 的形状。
    /// ⚠️ 上游 steer 路径**不归一化**（`_queueSteer` 直接 `content.push`），
    /// 形状错了**不报错**，只会让模型收到 `data:undefined;base64,undefined`
    /// —— 静默坏图（实测）。见 [`to_pi_images`]。
    pub async fn steer(&self, message: &str, images: Vec<String>) -> Result<(), String> {
        let mut cmd = json!({"type": "steer", "message": message});
        if !images.is_empty() {
            cmd["images"] = json!(to_pi_images(&images)?);
        }
        self.command(cmd, 10_000).await.map(|_| ())
    }

    /// 追问（排队，下一轮之后执行）。
    pub async fn follow_up(&self, message: &str) -> Result<(), String> {
        self.command(json!({"type": "follow_up", "message": message}), 10_000)
            .await
            .map(|_| ())
    }

    /// 中断当前运行。
    pub async fn abort(&self) -> Result<(), String> {
        self.command(json!({"type": "abort"}), 5_000).await.map(|_| ())
    }

    /// 取会话统计（tokens / cost / context 占用）。
    pub async fn stats(&self) -> Result<Value, String> {
        self.command(json!({"type": "get_session_stats"}), 10_000).await
    }

    /// 手动触发压缩。
    pub async fn compact(&self, custom_instructions: Option<String>) -> Result<Value, String> {
        let mut cmd = json!({"type": "compact"});
        if let Some(c) = custom_instructions {
            cmd["customInstructions"] = json!(c);
        }
        self.command(cmd, 120_000).await
    }

    /// 切模型。
    pub async fn set_model(&self, provider: &str, model_id: &str) -> Result<Value, String> {
        self.command(
            json!({"type": "set_model", "provider": provider, "modelId": model_id}),
            10_000,
        )
        .await
    }

    /// 取可用模型列表。
    pub async fn available_models(&self) -> Result<Value, String> {
        self.command(json!({"type": "get_available_models"}), 10_000).await
    }

    /// 切 thinking level。
    pub async fn set_thinking_level(&self, level: &str) -> Result<(), String> {
        self.command(json!({"type": "set_thinking_level", "level": level}), 10_000)
            .await
            .map(|_| ())
    }

    /// 请求中止标记（供外部取消）。
    pub fn set_aborted(&self, v: bool) {
        self.aborted.store(v, Ordering::Relaxed);
    }

    /// 是否已被请求中止。
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Relaxed)
    }

    /// 关掉 pi（关 stdin = 请求有序退出）。
    pub async fn shutdown(&self) {
        {
            let mut g = self.stdin.lock().await;
            *g = None;
        }
        let mut g = self.child.lock().await;
        if let Some(mut c) = g.take() {
            // 给它一点时间自己退
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), c.wait()).await;
            if c.try_wait().ok().flatten().is_none() {
                let _ = c.kill().await;
            }
        }
    }

    /// 内置 runtime 目录。
    pub fn runtime(&self) -> &Path {
        &self.runtime
    }
}

impl Drop for PiClient {
    fn drop(&mut self) {
        // 进程被丢弃时确保子进程不留孤儿。
        // ⚠️ Drop 里**不能 await** → 用 `try_lock`（拿不到锁就算了，
        //    那种情况下进程本来就还在被别人用，不该杀）。
        if let Ok(mut g) = self.child.try_lock() {
            if let Some(mut c) = g.take() {
                let _ = c.start_kill();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 事件辅助
// ---------------------------------------------------------------------------

/// 把 orbcat 的图片格式（**data URL 字符串**）转成 pi 要的形状
/// （`{type:"image", data:<纯 base64>, mimeType:"image/png"}`）。
///
/// ## 为什么必须转（2026-10-23 活体实测，不是猜的）
///
/// pi 侧**不认** data URL 字符串，两条通道的表现还不一样：
///
/// | 通道 | 上游代码 | 传字符串的后果（实测） |
/// |---|---|---|
/// | `prompt` | `core/agent-session.js:1604-1617` `_normalizePromptImages` 取 `image.data` / `image.mimeType` | **整条 prompt 被拒**：`success:false`，错误 `The first argument must be of type string or an instance of Buffer... Received undefined`，模型收到 **0 次**请求 |
/// | `steer` | `core/agent-session.js:1795-1800` `_queueSteer` 直接 `content.push(...images)`，**不归一化** | **不报错**，但模型收到 `{"image_url":{"url":"data:undefined;base64,undefined"}}` —— 静默坏图，最难查的那种 |
///
/// 而 orbcat 这边图片一直是 data URL 字符串（前端 `pendingImages.push(dataUrl)`，
/// Rust 侧 `images: Option<Vec<String>>`）⇒ **pi 内核下带图对话原本是坏的**。
///
/// ## 形状（上游 `dist/modes/rpc/rpc-types.d.ts:20-25`）
///
/// ```text
/// type:"prompt" | type:"steer" | message:string | images?:ImageContent[]
/// ImageContent = { type:"image", data:<纯 base64，不含 data: 前缀>, mimeType:string }
/// ```
///
/// 自研内核不受影响 —— 那边走 OpenAI 兼容的 `image_url`，data URL 是对的（`llm.rs:114`）。
pub fn to_pi_images(images: &[String]) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(images.len());
    for raw in images {
        out.push(to_pi_image(raw)?);
    }
    Ok(out)
}

/// 单张图的 data URL → `ImageContent`。
///
/// 支持两种输入：
/// - `data:image/png;base64,AAA`（orbcat 前端给的就是这个）
/// - 裸 base64（没有 data: 前缀）→ mimeType 缺省 `image/png`
fn to_pi_image(raw: &str) -> Result<Value, String> {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("data:") {
        // `data:<mime>;base64,<payload>` —— 也容忍 `data:<mime>,<payload>`（非 base64）
        let (mime, payload) = match rest.split_once(";base64,") {
            Some((m, p)) => (m, p),
            None => match rest.split_once(',') {
                Some((m, p)) => (m, p),
                None => return Err(format!("图片 data URL 解析失败: {}", s)),
            },
        };
        let mime = if mime.is_empty() { "image/png" } else { mime };
        Ok(json!({
            "type": "image",
            "data": payload,
            "mimeType": mime,
        }))
    } else if s.is_empty() {
        Err("图片内容为空".into())
    } else {
        // 裸 base64 —— mimeType 缺省
        Ok(json!({
            "type": "image",
            "data": s,
            "mimeType": "image/png",
        }))
    }
}

/// 从事件里取出「正文增量」。
pub fn text_delta(ev: &PiEvent) -> Option<String> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("message_update") {
        return None;
    }
    let a = ev.get("assistantMessageEvent")?;
    if a.get("type").and_then(|x| x.as_str()) != Some("text_delta") {
        return None;
    }
    a.get("delta").and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 从事件里取出「思考增量」。
pub fn thinking_delta(ev: &PiEvent) -> Option<String> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("message_update") {
        return None;
    }
    let a = ev.get("assistantMessageEvent")?;
    if a.get("type").and_then(|x| x.as_str()) != Some("thinking_delta") {
        return None;
    }
    a.get("delta").and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 从事件里取出「工具调用」。
pub fn tool_call(ev: &PiEvent) -> Option<(String, String, Value)> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("message_update") {
        return None;
    }
    let a = ev.get("assistantMessageEvent")?;
    if a.get("type").and_then(|x| x.as_str()) != Some("toolcall_end") {
        return None;
    }
    let tc = a.get("toolCall")?;
    let name = tc.get("name").and_then(|x| x.as_str())?.to_string();
    let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let args = tc.get("arguments").cloned().unwrap_or(Value::Null);
    Some((id, name, args))
}

/// 工具执行结果。
pub fn tool_result(ev: &PiEvent) -> Option<(String, bool, Value)> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("tool_execution_end") {
        return None;
    }
    let name = ev.get("toolName").and_then(|x| x.as_str())?.to_string();
    let is_err = ev.get("isError").and_then(|x| x.as_bool()).unwrap_or(false);
    let result = ev.get("result").cloned().unwrap_or(Value::Null);
    Some((name, is_err, result))
}

/// 工具执行开始。
pub fn tool_start(ev: &PiEvent) -> Option<(String, Value)> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("tool_execution_start") {
        return None;
    }
    let name = ev.get("toolName").and_then(|x| x.as_str())?.to_string();
    let args = ev.get("args").cloned().unwrap_or(Value::Null);
    Some((name, args))
}

/// 这条事件是不是「自动工作彻底结束」。
///
/// ⚠️ **不能用 `agent_end`** —— 它只表示「一轮低层 run 结束」，
/// 后面可能还有重试 / 溢出恢复 / 压缩 / 插话。要等 `agent_settled`。
pub fn is_settled(ev: &PiEvent) -> bool {
    ev.get("type").and_then(|x| x.as_str()) == Some("agent_settled")
}

/// 这条事件是不是「一轮结束」。
pub fn is_agent_end(ev: &PiEvent) -> bool {
    ev.get("type").and_then(|x| x.as_str()) == Some("agent_end")
}

/// 这条事件是不是**致命错误**（重试也没用的那种）。
///
/// ## 为什么需要自己判
///
/// pi 有一套自己的重试判定（`packages/ai/src/utils/retry.ts`），
/// 但它的「不可重试」清单**只认英文文案**：
/// `insufficient_quota` / `quota exceeded` / `billing` / `available balance`…
///
/// 而国内厂商的错误是**中文**。实测（2026-10-22）：
/// MiniMax 额度用完返回
/// `429: "已达到 Token Plan 用量上限：请升级 Token Plan 套餐或购买积分补充用量。 (2056)"`
///
/// → 一个都匹配不上 → 落到 `RETRYABLE_PROVIDER_ERROR_PATTERN` 的 `429`
/// → 被当成**瞬时限流** → pi 无限重试（每轮 3 次 × 无限轮）→
/// **整轮对话卡死**（实测卡了 15 分钟，session 里全是空 assistant 消息）。
///
/// 所以这里在 Rust 侧兜一层：识别到「额度/账单/密钥」类错误就**立即失败**，
/// 把错误原样抛给用户，而不是让它转圈。
///
/// 判定依据（任一命中即致命）：
/// - HTTP 4xx（除 408/429 之外的 4xx = 请求本身有问题，重试没用）
/// - 中文额度/账单/密钥/欠费关键词
/// - 英文额度/账单/鉴权关键词（与 pi 的清单对齐，但中文补齐）
pub fn is_fatal_error(ev: &PiEvent) -> Option<String> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("message_end") {
        return None;
    }
    let m = ev.get("message")?;
    if m.get("role").and_then(|x| x.as_str()) != Some("assistant") {
        return None;
    }
    if m.get("stopReason").and_then(|x| x.as_str()) != Some("error") {
        return None;
    }
    let msg = m
        .get("errorMessage")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    if msg.is_empty() {
        return None;
    }
    if is_fatal_error_text(msg) {
        Some(msg.to_string())
    } else {
        None
    }
}

/// 错误文案是不是「重试也没用」的那一类。
///
/// 单列出来是为了**能直接测**（不用起 pi 子进程）。
pub fn is_fatal_error_text(msg: &str) -> bool {
    let m = msg.to_lowercase();

    // ── 中文：额度 / 账单 / 鉴权 ──
    // 这些是国内厂商的常见措辞，pi 的英文清单一个都覆盖不到。
    // 顺带把几个英文鉴权/额度词也放这儿（中文文案里常夹英文）。
    const CN_FATAL: &[&str] = &[
        "用量上限",
        "额度",
        "余额不足",
        "欠费",
        "余额已用尽",
        "积分不足",
        "套餐",
        "token plan",
        "api key",
        "apikey",
        "密钥",
        "鉴权失败",
        "认证失败",
        "未授权",
        "invalid api key",
        "unauthorized",
    ];
    for k in CN_FATAL {
        if m.contains(&k.to_lowercase()) {
            return true;
        }
    }

    // ── 英文：与 pi 的清单对齐 ──
    //
    // ⚠️ 同一句话有**多种词序**：`quota exceeded` / `exceeded your quota` /
    // `exceed your quota`。只写一种会漏（2026-10-22 单测踩到）。
    const EN_FATAL: &[&str] = &[
        "insufficient_quota",
        "quota exceeded",
        "exceeded your quota",
        "exceed your quota",
        "out of budget",
        "billing",
        "available balance",
        "usage limit",
    ];
    for k in EN_FATAL {
        if m.contains(k) {
            return true;
        }
    }

    // ── 4xx（408/429 除外 —— 那两个可能是瞬时的）──
    // 403/401 = 鉴权问题，重试没用。
    for code in ["401", "403", "402"] {
        if m.contains(&format!("http_code\":\"{code}")) || m.contains(&format!(" {code} ")) {
            return true;
        }
    }

    false
}

/// 最终 assistant 正文（从 `message_end` 里取）。
pub fn assistant_text(ev: &PiEvent) -> Option<String> {
    if ev.get("type").and_then(|x| x.as_str()) != Some("message_end") {
        return None;
    }
    let m = ev.get("message")?;
    if m.get("role").and_then(|x| x.as_str()) != Some("assistant") {
        return None;
    }
    let content = m.get("content")?;
    // content 可能是字符串，也可能是块数组
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for b in arr {
            if b.get("type").and_then(|x| x.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                    out.push_str(t);
                }
            }
        }
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_识别() {
        let ev = json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_delta", "delta": "你好"}
        });
        assert_eq!(text_delta(&ev).as_deref(), Some("你好"));
    }

    #[test]
    fn 非文本事件返回None() {
        let ev = json!({"type": "agent_start"});
        assert!(text_delta(&ev).is_none());
        assert!(!is_settled(&ev));
    }

    #[test]
    fn settled_只认agent_settled() {
        assert!(is_settled(&json!({"type": "agent_settled"})));
        assert!(!is_settled(&json!({"type": "agent_end"})));
        assert!(is_agent_end(&json!({"type": "agent_end"})));
    }

    /// 回归测试（2026-10-22 实测）：MiniMax 额度用完时 pi 会**无限重试**。
    ///
    /// pi 的不可重试清单只认英文（`insufficient_quota` / `quota exceeded`…），
    /// 国内厂商的中文文案一个都匹配不上 → 落到 `429` 的可重试正则 →
    /// 每轮重试 3 次 × 无限轮 → 整轮对话卡死（实测 15 分钟）。
    #[test]
    fn 致命错误_认得出中文额度文案() {
        let msg = "429: {\"type\":\"rate_limit_error\",\"message\":\"已达到 Token Plan 用量上限：请升级 Token Plan 套餐或购买积分补充用量。 (2056)\",\"http_code\":\"429\"}";
        assert!(
            is_fatal_error_text(msg),
            "中文额度文案必须判为致命错误（否则 pi 无限重试 → 卡死）"
        );
    }

    #[test]
    fn 致命错误_认得出英文额度文案() {
        assert!(is_fatal_error_text("429 insufficient_quota"));
        assert!(is_fatal_error_text("You have exceeded your quota"));
        assert!(is_fatal_error_text("billing not available"));
    }

    /// 瞬时限流**不能**判成致命 —— 否则正常的高峰限流会被误杀。
    #[test]
    fn 瞬时限流不判致命() {
        assert!(!is_fatal_error_text("429 too many requests"));
        assert!(!is_fatal_error_text("503 service temporarily unavailable"));
        assert!(!is_fatal_error_text("model is at capacity"));
    }

    #[test]
    fn 致命错误_从message_end里抽() {
        let ev = json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [],
                "stopReason": "error",
                "errorMessage": "已达到 Token Plan 用量上限"
            }
        });
        assert!(is_fatal_error(&ev).is_some());
    }

    /// 正常结束的消息**不能**被判成致命错误。
    #[test]
    fn 正常消息不是致命错误() {
        let ev = json!({
            "type": "message_end",
            "message": {"role": "assistant", "content": [{"type":"text","text":"你好"}], "stopReason": "stop"}
        });
        assert!(is_fatal_error(&ev).is_none());
    }

    #[test]
    fn assistant_text_从块数组里抽() {        let ev = json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "思考中"},
                    {"type": "text", "text": "答案A"},
                    {"type": "text", "text": "答案B"}
                ]
            }
        });
        assert_eq!(assistant_text(&ev).as_deref(), Some("答案A答案B"));
    }

    #[test]
    fn tool_call_解析() {
        let ev = json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "toolcall_end",
                "toolCall": {"id": "t1", "name": "bash", "arguments": {"command": "ls"}}
            }
        });
        let (id, name, args) = tool_call(&ev).expect("应解析出 tool call");
        assert_eq!(id, "t1");
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn tool_result_识别错误() {
        let ok = json!({"type": "tool_execution_end", "toolName": "bash", "isError": false, "result": {}});
        let err = json!({"type": "tool_execution_end", "toolName": "bash", "isError": true, "result": {}});
        assert_eq!(tool_result(&ok).unwrap().1, false);
        assert_eq!(tool_result(&err).unwrap().1, true);
    }

    #[test]
    fn argv_是替换语义的工具白名单() {
        let mut cfg = PiConfig::new(
            PathBuf::from("C:/rt"),
            PathBuf::from("C:/ad"),
            PathBuf::from("C:/cwd"),
        );
        cfg.provider = Some("ep1".into());
        cfg.model = Some("m1".into());
        cfg.tools = vec!["read".into(), "bash".into()];
        let a = cfg.argv();
        // --tools 后面必须跟完整白名单（不能只列要加的）
        let i = a.iter().position(|x| x == "--tools").expect("应有 --tools");
        assert_eq!(a[i + 1], "read,bash");
        // --mode 必须有
        assert!(a.contains(&"--mode".to_string()));
        assert_eq!(a[a.iter().position(|x| x == "--mode").unwrap() + 1], "rpc");
    }

    /// 回归测试：`--no-session` 会让 pi 走**内存会话**，
    /// `--session-dir` 被完全忽略（`main.ts:364`）。
    ///
    /// 之前 argv 里两个**同时**给 → `--no-session` 赢 →
    /// pi 崩了上下文就没了（注释说的"用 --session-dir"根本没生效）。
    #[test]
    fn argv_有session_dir时不能加no_session() {
        let mut cfg = PiConfig::new(
            PathBuf::from("C:/rt"),
            PathBuf::from("C:/ad"),
            PathBuf::from("C:/cwd"),
        );
        cfg.session_dir = Some(PathBuf::from("C:/ad/sessions"));
        let a = cfg.argv();
        assert!(
            !a.contains(&"--no-session".to_string()),
            "有 session_dir 时不能加 --no-session（那会让 --session-dir 失效）"
        );
        assert!(a.contains(&"--session-dir".to_string()));
    }

    /// 回归测试（2026-10-23 安全修复）：pi 的 8 个内置工具**必须**全部被排除。
    ///
    /// 它们原生执行，绕过 orbcat 的 `permission.rs` 文件网关与
    /// `command_policy.rs` 命令策略（实测 pi 的 `bash` RPC 真的执行了命令）。
    #[test]
    fn argv_排除全部pi内置工具() {
        let cfg = PiConfig::new(
            PathBuf::from("C:/rt"),
            PathBuf::from("C:/ad"),
            PathBuf::from("C:/cwd"),
        );
        let a = cfg.argv();
        let i = a
            .iter()
            .position(|x| x == "--exclude-tools")
            .expect("应有 --exclude-tools");
        let list: Vec<&str> = a[i + 1].split(',').collect();
        for b in PI_BUILTIN_TOOLS {
            assert!(
                list.contains(b),
                "pi 内置工具 `{b}` 必须被排除（它绕过权限网关），实际排除列表: {list:?}"
            );
        }
        // 8 个内置工具一个都不能漏
        assert_eq!(
            PI_BUILTIN_TOOLS.len(),
            8,
            "pi 的内置工具数变了？上游 createAllToolDefinitions 改了的话这里要跟着改"
        );
    }

    /// 回归测试：`--tools` 白名单里**不能**出现 pi 内置工具。
    ///
    /// `--tools` 是**替换语义** —— 白名单里列了它，它就进 registry，模型能直接调。
    #[test]
    fn argv_白名单不含任何pi内置工具() {
        let mut cfg = PiConfig::new(
            PathBuf::from("C:/rt"),
            PathBuf::from("C:/ad"),
            PathBuf::from("C:/cwd"),
        );
        cfg.tools = vec!["orb_read_file".into(), "orb_run_command".into(), "mcp__*".into()];
        let a = cfg.argv();
        let i = a.iter().position(|x| x == "--tools").expect("应有 --tools");
        let list: Vec<&str> = a[i + 1].split(',').collect();
        for b in PI_BUILTIN_TOOLS {
            assert!(
                !list.contains(b),
                "白名单里不该有 pi 内置工具 `{b}`（会绕过权限网关），实际: {list:?}"
            );
        }
    }

    /// 回归测试：白名单**不能**混用 `+name` / `-name` 修饰符。
    ///
    /// 上游 `getToolListError`：混用会直接报
    /// "tool names cannot be mixed with +name or -name entries" → pi 起不来。
    /// 白名单里的 `mcp__*` 是**通配**，不是修饰符 —— 两者别混。
    #[test]
    fn argv_白名单不用加减修饰符() {
        let mut cfg = PiConfig::new(
            PathBuf::from("C:/rt"),
            PathBuf::from("C:/ad"),
            PathBuf::from("C:/cwd"),
        );
        cfg.tools = vec!["orb_read_file".into(), "mcp__*".into()];
        let a = cfg.argv();
        let i = a.iter().position(|x| x == "--tools").expect("应有 --tools");
        let list: Vec<&str> = a[i + 1].split(',').collect();
        for entry in &list {
            assert!(
                !entry.starts_with('+') && !entry.starts_with('-'),
                "白名单里不该有 +/- 修饰符（上游会直接报错），实际: {list:?}"
            );
        }
    }

    /// 回归测试：必须带 `--approve`。
    ///
    /// 没有它，pi 在项目目录有 `.pi/` 配置时会问「信任这个项目吗」——
    /// RPC 模式下**没有 UI** → 那个选择**永久阻塞** → 整轮对话卡死。
    #[test]
    fn argv_必须强制信任项目() {
        let cfg = PiConfig::new(
            PathBuf::from("C:/rt"),
            PathBuf::from("C:/ad"),
            PathBuf::from("C:/cwd"),
        );
        assert!(
            cfg.argv().contains(&"--approve".to_string()),
            "必须带 --approve —— 否则 pi 的项目信任提问会永久阻塞 RPC"
        );
    }

    /// 回归测试：data URL 字符串必须转成 pi 的 `ImageContent`。
    ///
    /// 这条不是形式检查 —— 2026-10-23 **活体实测**（随包 node 直接驱动 pi 的 RPC）：
    /// - 送 data URL 字符串 → `success:false`，错误
    ///   `The first argument must be of type string or an instance of Buffer... Received undefined`，
    ///   模型收到 **0 次**请求（上游 `Buffer.from(image.data,...)` 里 `image.data` 是 undefined）
    /// - 送 `{type:"image",data,mimeType}` → `success:true`，模型真收到图
    #[test]
    fn 图片要转成pi的ImageContent形状() {
        let out = to_pi_images(&["data:image/png;base64,AAA".to_string()]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "image");
        assert_eq!(out[0]["data"], "AAA", "data 必须剥掉 data: 前缀，只留纯 base64");
        assert_eq!(out[0]["mimeType"], "image/png");
        // ⚠️ 绝不能带 data: 前缀 —— pi 会拿整串去 base64 解码
        assert!(!out[0]["data"].as_str().unwrap().contains("data:"));
    }

    /// 回归测试：裸 base64 也要能用（mimeType 缺省 png）。
    #[test]
    fn 裸base64图片缺省mimeType() {
        let out = to_pi_images(&["BBB".to_string()]).unwrap();
        assert_eq!(out[0]["data"], "BBB");
        assert_eq!(out[0]["mimeType"], "image/png");
    }

    /// 回归测试：空图不许悄悄变成 `data:undefined`（那是最难查的静默失败）。
    #[test]
    fn 空图片要报错而不是变成undefined() {
        assert!(to_pi_images(&["   ".to_string()]).is_err());
        assert!(to_pi_images(&[]).unwrap().is_empty());
    }

    /// 🔴 延迟应答：**调用立即返回**（读循环零阻塞），答复到了才写。
    ///
    /// ## 为什么这条是硬指标
    ///
    /// `dispatch` 跑在 pi 的 **stdout 读循环**里。等用户（最多 3 分钟）会卡住
    /// 读循环 ⇒ pi 写 stdout 被背压卡住 ⇒ **死锁**（pi.rs 记过同族坑）。
    ///
    /// ## 判据
    ///
    /// ① `spawn_deferred_reply` **立即返回**（不 await 任何东西）
    /// ② 答复送到后，writer 收到 `confirmed:true`
    /// ③ sender 被丢掉（超时 / 取消）时，writer 收到 `cancelled:true`（pi 不干等）
    #[tokio::test]
    async fn 延迟应答_立即返回_答复到了才写_掉了写cancelled() {
        // ① 立即返回
        let (d, tx) = DeferredReply::pair();
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let w2 = written.clone();
        let t0 = std::time::Instant::now();
        spawn_deferred_reply("ui-1".to_string(), d, move |resp| {
            let w = w2.clone();
            async move {
                w.lock().unwrap().push(resp);
            }
        });
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(50),
            "spawn_deferred_reply 必须立即返回（读循环零阻塞）"
        );
        // 还没答复 ⇒ 什么都没写
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(written.lock().unwrap().is_empty(), "没答复就不该写");

        // ② 答复送到 ⇒ 写出 confirmed
        let _ = tx.send(PiHostReply::Confirm(true));
        for _ in 0..100 {
            if !written.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let got = written.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["type"], "extension_ui_response");
        assert_eq!(got[0]["id"], "ui-1");
        assert_eq!(got[0]["confirmed"], true);

        // ③ sender 被丢掉 ⇒ 写 cancelled（pi 不干等）
        let (d2, tx2) = DeferredReply::pair();
        let written2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let w3 = written2.clone();
        spawn_deferred_reply("ui-2".to_string(), d2, move |resp| {
            let w = w3.clone();
            async move {
                w.lock().unwrap().push(resp);
            }
        });
        drop(tx2);
        for _ in 0..100 {
            if !written2.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let got2 = written2.lock().unwrap().clone();
        assert_eq!(got2.len(), 1, "sender 掉了也要写一条（cancelled）");
        assert_eq!(got2[0]["cancelled"], true);
    }

    /// 回归测试：应答 JSON 的三种形态（同步答 / 延迟答兜底）。
    #[test]
    fn 应答json三种形态() {
        let a = ui_reply_json("x", PiHostReply::Confirm(false));
        assert_eq!(a["confirmed"], false);
        let b = ui_reply_json("x", PiHostReply::Select(None));
        assert_eq!(b["cancelled"], true);
        let c = ui_reply_json("x", PiHostReply::Input("hi".into()));
        assert_eq!(c["value"], "hi");
    }
}
