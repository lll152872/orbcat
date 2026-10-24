//! pi 运行时 —— 把 pi 变成 orbcat 的 agent 内核。
//!
//! ## 这一层负责什么
//!
//! 1. **常驻一个 pi 子进程**（不是每轮起一个 —— 冷启动 0.4s，每轮起太浪费）
//! 2. **起工具桥**（`pi_bridge.rs`），把 orbcat 的 34 个工具接给 pi
//! 3. **起权限闸门 extension**（`tool_call` → `ctx.ui.confirm()` → 回到 Rust 裁决）
//! 4. **接管 system prompt**（读 orbcat 的四文件记忆 + 项目记忆 + 技能命中）
//! 5. **把 pi 事件翻译成 orbcat 的 `Progress`**（`pi_run.rs`）
//!
//! ## 为什么 pi 进程要常驻
//!
//! - 冷启动 0.4 s（node 路线）—— 每轮起一个的话，每轮都付这个钱
//! - pi 的 `models.json` / `auth.json` 读一次就够
//! - 崩溃后要能自动重启（`ensure_running`）
//!
//! ## 崩溃恢复策略
//!
//! pi 进程死了 → 下次 `ensure_running()` 时重启 → **会话上下文靠 pi 的 session 文件恢复**
//! （`--session-dir` 指向 orbcat 的目录）。如果用的是 `--no-session`，
//! 那就只剩最后一轮 —— 所以**这里要用 `--session-dir` 而不是 `--no-session`**。
//!
//! ⚠️ 但 orbcat 自己的会话存储（`sessions.rs`，平铺 JSON）才是**前端显示用的真相**。
//! pi 的 session 文件是**给 pi 自己续上下文用的**，两者并行存在、互不干扰。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::agent::{Progress, ProgressFn};
use crate::pi::{self, PiClient, PiHostReply, PiHostRequest};
use crate::pi_bridge::{specs_to_json, ToolBridge, ToolHandler};

/// 权限裁决回调（宿主提供）。
///
/// 签名：`(工具名, 参数, 会话 id)` → `(允许?, 理由)`。
///
/// ## 为什么多一个「会话 id」
///
/// 多会话并行时，**每个会话一个 pi 进程**，每个进程的权限裁决必须用
/// **它自己那个会话**的上下文（`mcp_grants` 是按项目+会话算的、
/// 审计链要写对会话）。
///
/// 以前这个 id 是在回调里现取 `sessions::current_id()` —— 那拿到的是
/// **用户当前正在看的那个会话**，不是这个 pi 进程所属的会话 ⇒ 并行时串味。
///
/// `None` = 拿不到会话（测试路径）⇒ 宿主自己兜底。
///
/// ⚠️ 第三个参数是**拥有所有权**的 `Option<String>`，不是 `Option<&str>` ——
/// 返回的 boxed future 是 `'static` 的，借来的 `&str` 传不进去（编译期就报
/// "lifetime may not live long enough"）。
pub type GateFn = Arc<
    dyn Fn(&str, &Value, Option<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = (bool, String)> + Send>>
        + Send
        + Sync,
>;

/// 宿主确认钩子：pi 问「能不能做 X」（`ctx.ui.confirm()`）时，
/// 宿主弹一张 orbcat 确认卡等用户。
///
/// - 参数：`(标题, 正文, 会话 id)`
/// - 返回：`true` = 用户允许
///
/// ⚠️ **必须是 async** —— 里面要等用户在卡上点（最多 3 分钟）。
/// 所以宿主回调**不能在 pi 的 stdout 读循环里等**它 ⇒ 必须走
/// [`crate::pi::PiHostReply::Deferred`]（延迟应答）。
pub type ConfirmFn = Arc<
    dyn Fn(String, String, Option<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// 一轮对话的结果（与 `agent::AgentRun` 同构，便于前端复用现有渲染）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiRun {
    /// 最终答复正文
    pub answer: String,
    /// 时间线步骤
    pub steps: Vec<crate::agent::AgentStep>,
    /// 轮数
    pub iterations: usize,
    /// 思考过程
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 是否被打断
    #[serde(default)]
    pub interrupted: bool,
    /// 插话
    #[serde(default)]
    pub steers: Vec<crate::agent::SteeredMsg>,
    /// 中断时没送出去的插话
    #[serde(default)]
    pub pending_steers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// token 用量
    #[serde(default)]
    pub usage: crate::llm::TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_scale: Option<f64>,
}

/// 本轮的 prompt 上下文（**宿主在开跑前算好**再塞进来）。
///
/// ## 为什么宿主算好再传
///
/// `agent::build_system_prompt_full` 需要两样东西：
///
/// 1. **`session_id`** —— 决定 **per-session mode**
///    （`agent.rs` 里 `effective_mode(data_dir, session_id)`，
///    不能读全局 `settings.activeMode`）；
/// 2. **`ptc_sdk`** —— PTC 模式下的「程序内可用工具声明」。
///
/// 而第 2 样要读 MCP 注册表（`tools::ptc_sdk_tools`）—— 那是 **async** 的，
/// 桥的 prompt 回调却是**同步**闭包（`Fn(&str) -> String`），拿不到 await。
///
/// 所以：**宿主（`lib.rs::chat_pi`）在开跑前算好 → 存进这里 → 同步闭包直接读**。
///
/// ⚠️ 2026-10-23 修：pi 内核以前传的是 `(None, None)` ⇒ **丢了 per-session mode
/// 与 PTC SDK 声明**；自研内核（`agent.rs`）传的是 `(Some(session_id), ptc_sdk)`。
/// ⇒ 同一个会话在两个内核下看到的 prompt **不一样**。
#[derive(Debug, Clone, Default)]
pub struct PromptCtx {
    /// 当前会话 id（决定 per-session mode）
    pub session_id: Option<String>,
    /// PTC 模式下的工具声明（`None` = 非 PTC 模式）
    pub ptc_sdk: Option<String>,
}

/// pi 运行时（全局单例）。
pub struct PiRuntime {
    /// 数据目录（`agent-data/`）
    data_dir: PathBuf,
    /// 内置 runtime 目录
    runtime: PathBuf,
    /// pi 的 agent 目录（存 models.json / auth.json）
    agent_dir: PathBuf,
    /// 当前 pi 客户端
    client: tokio::sync::Mutex<Option<Arc<PiClient>>>,
    /// 工具桥
    bridge: tokio::sync::Mutex<Option<Arc<ToolBridge>>>,
    /// 权限裁决回调（宿主提供）
    gate: std::sync::Arc<std::sync::Mutex<Option<GateFn>>>,
    /// 宿主确认钩子（pi 的 `ctx.ui.confirm()` → 我们的确认卡）
    confirm_hook: std::sync::Arc<std::sync::Mutex<Option<ConfirmFn>>>,
    /// 强制指定模型（`(provider, model)`）—— 覆盖从 settings 反查的结果。
    ///
    /// ## 什么时候用
    ///
    /// - **测试**：用户设置里那个模型可能额度用完了（429），
    ///   要能显式换一个来验代码，而不是验"额度用完"。
    /// - 前端**临时切模型**而不改 `settings.json`。
    ///
    /// 生产路径不设它 —— 走 `settings.selected_model` 那条反查。
    /// 覆盖模型：`(模型 id, base URL, 是否声明支持图片)`。
    ///
    /// 第三个字段是**图片能力声明** —— pi 会按 `model.input` 决定
    /// **要不要把图发给模型**（上游 `openai-completions` chunk：
    /// `hasImages && model.input.includes("image")`）
    /// ⇒ 声明只有 `["text"]` 的话，**图会被静默丢掉**（不报错）。
    model_override: tokio::sync::Mutex<Option<(String, String, bool)>>,
    /// 额外注入的 MCP server（**测试专用**）。
    /// 为什么要这个：测「pi 真的能连 MCP、真的能调到 MCP 工具」需要一个
    /// **确定**的 server —— 不能依赖用户本机配的 `1mcp` 网关（它可能没起、
    /// 工具名可能变）。测试通过 `add_mcp_server_for_test()` 注入一个本地的
    /// 假 server，**不写用户的 `agent-data/mcp.json`**。
    extra_mcp: tokio::sync::Mutex<Vec<TestMcpServer>>,
    /// 本轮的 prompt 上下文（宿主开跑前塞进来，见 [`PromptCtx`]）。
    prompt_ctx: Arc<std::sync::Mutex<PromptCtx>>,
    /// 这个 pi 进程**属于哪个会话**（多会话并行时用于路由权限裁决）。
    session_id: Option<String>,
    /// pi 的会话文件目录（`--session-dir`）。`None` = 用 `agent_dir/sessions`。
    ///
    /// 多会话并行时每个会话一个 pi 进程 ⇒ 各自的 session 文件必须**分目录**，
    /// 否则两个进程抢同一个 session 文件。
    session_dir: Option<PathBuf>,
    /// 正在跑的标记（防重入）
    busy: AtomicBool,
}

/// 测试用的 MCP server 描述（直接对应 pi `mcp.json` 的一条 stdio entry）。
#[derive(Debug, Clone)]
pub struct TestMcpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
}

impl PiRuntime {
    /// 新建运行时。
    ///
    /// ## 两个目录别搞混
    ///
    /// - `data_dir` —— **orbcat 的数据目录**（`agent-data/`）。
    ///   模型的来源（`models.json`）、记忆、设置都在这儿。
    /// - `agent_dir` —— **pi 的 agent 目录**（`PI_CODING_AGENT_DIR`）。
    ///   pi 的 `models.json` / `auth.json` / `sessions/` 落这儿。
    ///
    /// 默认 `agent_dir = data_dir/pi`（两者同源，省事）。
    /// 但**测试**要把 agent_dir 指到临时目录（别污染真实数据），
    /// 而 data_dir 必须指向真实数据（模型从那儿来）。
    pub fn new(data_dir: PathBuf, runtime: PathBuf) -> Arc<Self> {
        let agent_dir = data_dir.join("pi");
        Self::with_agent_dir(data_dir, runtime, agent_dir)
    }

    /// 同 [`PiRuntime::new`]，但 agent 目录单独指定。
    pub fn with_agent_dir(
        data_dir: PathBuf,
        runtime: PathBuf,
        agent_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            runtime,
            agent_dir,
            client: tokio::sync::Mutex::new(None),
            bridge: tokio::sync::Mutex::new(None),
            gate: std::sync::Arc::new(std::sync::Mutex::new(None)),
            confirm_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: tokio::sync::Mutex::new(None),
            extra_mcp: tokio::sync::Mutex::new(Vec::new()),
            prompt_ctx: Arc::new(std::sync::Mutex::new(PromptCtx::default())),
            session_id: None,
            session_dir: None,
            busy: AtomicBool::new(false),
        })
    }

    /// 新建一个**属于某个会话**的运行时（多会话并行用）。
    ///
    /// - `agent_dir` —— 共享的（`models.json` / `mcp.json` 只有一份）
    /// - `session_dir` —— **每会话独立**（`agent_dir/sessions/<session_id>/`）
    ///   ⇒ 两个 pi 进程不会抢同一个 session 文件
    pub fn with_session(
        data_dir: PathBuf,
        runtime: PathBuf,
        agent_dir: PathBuf,
        session_id: &str,
    ) -> Arc<Self> {
        let session_dir = agent_dir.join("sessions").join(session_id);
        let _ = std::fs::create_dir_all(&session_dir);
        Arc::new(Self {
            data_dir,
            runtime,
            agent_dir,
            client: tokio::sync::Mutex::new(None),
            bridge: tokio::sync::Mutex::new(None),
            gate: std::sync::Arc::new(std::sync::Mutex::new(None)),
            confirm_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: tokio::sync::Mutex::new(None),
            extra_mcp: tokio::sync::Mutex::new(Vec::new()),
            prompt_ctx: Arc::new(std::sync::Mutex::new(PromptCtx::default())),
            session_id: Some(session_id.to_string()),
            session_dir: Some(session_dir),
            busy: AtomicBool::new(false),
        })
    }

    /// 这个 pi 进程属于哪个会话。
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 当前有没有在跑（跑着的不该被淘汰）。
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// 关掉这个 pi 进程（释放它占的内存）。
    pub async fn shutdown(&self) {
        if let Some(c) = self.client.lock().await.take() {
            c.shutdown().await;
        }
        if let Some(b) = self.bridge.lock().await.take() {
            b.stop();
        }
    }

    /// 注入一个 MCP server（**测试专用**，不写用户的 `mcp.json`）。
    ///
    /// 与真实配置的区别：这里注入的 server **只在本进程内生效** ——
    /// `sync_mcp()` 会把它一起写进 pi 的 `<agentDir>/mcp.json`，
    /// 但用户自己的 `agent-data/mcp.json` 一个字节都不动。
    pub async fn add_mcp_server_for_test(
        &self,
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: std::collections::BTreeMap<String, String>,
    ) {
        self.extra_mcp.lock().await.push(TestMcpServer {
            name: name.into(),
            command: command.into(),
            args,
            env,
        });
    }

    /// 强制指定模型（覆盖从 `settings.selected_model` 反查的结果）。
    ///
    /// 参数是**模型的 `(id, base URL)`** —— 不是 provider 名。
    /// 因为 provider 名（`orb1`/`orb6`…）是按 URL 去重后的**插入顺序**
    /// 生成的，外部没法预知（2026-10-22 踩到：以为能按名指定，结果错位）。
    ///
    /// 只在**测试**和「前端临时切模型」时用。生产路径不设它。
    pub async fn set_model_override(&self, model_id: impl Into<String>, base_url: impl Into<String>) {
        *self.model_override.lock().await = Some((model_id.into(), base_url.into(), false));
    }

    /// 同 [`PiRuntime::set_model_override`]，但**声明这个模型支持图片**。
    ///
    /// ## 为什么需要这一档
    ///
    /// pi 按模型的 `input` 能力声明决定**要不要把图发给模型**
    /// （上游 `openai-completions` chunk：`hasImages && model.input.includes("image")`）
    /// ⇒ 声明只有 `["text"]` 的话，**图会被静默丢掉** —— 不报错、模型收不到、
    /// 前端还以为图送出去了。
    ///
    /// 注入的临时 provider 默认按 orbcat `models.json` 里**同名模型**的
    /// `supportsImages` 声明；模型不在清单里（测试用的假模型就是）时，
    /// 用这个方法显式声明。
    pub async fn set_model_override_with_images(
        &self,
        model_id: impl Into<String>,
        base_url: impl Into<String>,
    ) {
        *self.model_override.lock().await = Some((model_id.into(), base_url.into(), true));
    }

    /// 设置本轮的 prompt 上下文（**宿主在开跑前调**）。
    ///
    /// 见 [`PromptCtx`]：pi 内核以前传的是 `(None, None)`，
    /// 丢了 per-session mode 与 PTC SDK 声明；这里补回来。
    pub fn set_prompt_ctx(&self, session_id: Option<String>, ptc_sdk: Option<String>) {
        if let Ok(mut g) = self.prompt_ctx.lock() {
            *g = PromptCtx { session_id, ptc_sdk };
        }
    }

    /// 设置权限裁决回调。
    ///
    /// 回调签名：`(工具名, 参数)` → `(允许?, 理由)`。
    /// 宿主（`lib.rs`）用 `permission.rs` 的现成逻辑来判；
    /// MCP 工具还要走 `PermHub::ask_mcp_tool` 弹确认卡。
    ///
    /// ## 为什么是**同步**的
    ///
    /// Tauri 的 `setup` 闭包是同步的 —— 里面没法 await。
    ///
    /// ⚠️ **不能抢不到就静默丢**（那就等于没设 gate ⇒ P2 复发：
    /// 桥会退回静态裁决，MCP 授权静默降级）。
    /// 现在用 `std::sync::Mutex` + `lock().unwrap_or_else(|e| e.into_inner())`
    /// —— 任何线程都安全（**不会在 async 上下文里 panic**），poison 也能继续用。
    /// 本函数只在**同步**上下文里被调（Tauri setup 闭包 / 池的 set_gate / 测试），
    /// 不会在 async 上下文里触发 tokio 的 panic。
    ///
    /// ⚠️ 签名第三个参数是**这个 pi 进程自己的会话 id**（`Option<&str>`）——
    /// 多会话并行时不能拿「用户当前正在看的会话」，否则权限裁决会串味。
    pub fn set_gate(&self, f: GateFn) {
        // std Mutex + into_inner()：两个坑一起堵 ——
        //  1) 原来用 tokio::sync::Mutex::blocking_lock() 兜底：它在 **async 上下文里会 panic**
        //     （tokio: cannot block the current thread from within a runtime）。
        //  2) poison 也要继续用（into_inner）—— 静默跳过 = 等于没设 gate ⇒ P2 静默降级复发。
        *self.gate.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// 设置宿主确认钩子（pi 的 `ctx.ui.confirm()` → 我们的确认卡）。
    ///
    /// 与 [`PiRuntime::set_gate`] 同口径：**已有**的运行时也一起套上。
    pub fn set_confirm_hook(&self, f: ConfirmFn) {
        // 同 set_gate：不走 blocking_lock（async 里会 panic），poison 也继续用。
        *self.confirm_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// 内置 runtime 目录。
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime
    }

    /// pi 的 agent 目录。
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// 工具桥端口（0 = 没起）。
    pub fn bridge_port(&self) -> u16 {
        // 同步方法不能用 await → try_lock（拿不到就报 0，调用方会再 ensure_bridge）
        self.bridge
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|b| b.port()))
            .unwrap_or(0)
    }

    /// 幂等地保证 pi 进程在跑。
    ///
    /// 崩溃了就在这里重启 —— 下一轮对话自动恢复，用户无感。
    pub async fn ensure_running(&self) -> Result<Arc<PiClient>, String> {
        // 已有活的且没被标记中止 → 直接用
        {
            let g = self.client.lock().await;
            if let Some(c) = g.as_ref() {
                if !c.is_aborted() {
                    return Ok(c.clone());
                }
            }
        }
        self.restart().await
    }

    /// 强制重启 pi 进程。
    pub async fn restart(&self) -> Result<Arc<PiClient>, String> {
        // 先关旧的
        if let Some(old) = self.client.lock().await.take() {
            old.shutdown().await;
        }

        let cfg = self.build_config().await?;
        // 权限闸（gate）**不在这里传** —— 它由 `build_gate` 做成「每次调用现取」的
        // 闭包（P2 的核心：不能在建桥那刻抄成快照）。
        // 旧代码这里有一行 `let gate = self.gate.lock().await.clone();` ——
        // 抄了快照却**根本没被用**（死变量），只会误导下一个人以为闸门是快照的。
        // 宿主确认通道（pi 的 `ctx.ui.confirm()` → 我们的确认卡）
        let confirm_hook = self
            .confirm_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let client = PiClient::spawn(
            cfg,
            Self::make_host(confirm_hook, self.session_id.clone()),
        )
        .await?;
        *self.client.lock().await = Some(client.clone());
        Ok(client)
    }

    /// 拼 pi 启动配置。
    async fn build_config(&self) -> Result<crate::pi::PiConfig, String> {
        // 桥要先起 —— extension 要拿端口
        let bridge = self.ensure_bridge().await?;

        let mut cfg = crate::pi::PiConfig::new(
            self.runtime.clone(),
            self.agent_dir.clone(),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        );
        // ⚠️ **不能用 `--no-session`** —— 那样 pi 崩了就丢上下文。
        // 用 orbcat 数据目录下的 pi/sessions，pi 自己能续。
        //
        // 多会话并行时：每会话一个 pi 进程 ⇒ session 目录**必须分开**，
        // 否则两个进程抢同一个 session 文件。
        let sessions = self
            .session_dir
            .clone()
            .unwrap_or_else(|| self.agent_dir.join("sessions"));
        let _ = std::fs::create_dir_all(&sessions);
        cfg.session_dir = Some(sessions);

        // 模型：从 orbcat 的 models.json 同步一份到 pi 的 models.json，
        // 并**显式指定**用哪个（不指定 → pi 用内置模型 → 没 key → 一句都跑不起来）
        //
        // ⚠️ `sync_models()` **必须照常调** —— 它负责把 models.json 写到
        // pi 的 agent 目录（否则 pi 找不到 provider）。
        // 它内部已经优先按 `model_override` 选模型，返回的就是最终选中的那个。
        if let Some((pid, mid)) = self.sync_models().await? {
            cfg.provider = Some(pid);
            cfg.model = Some(mid);
        }

        // 工具：orbcat 的 34 个（通过 bridge extension 注册）+ MCP 工具（pi 原生连）
        //
        // ⚠️ **绝不列 pi 的内置工具**（read/bash/edit/write/grep/find/ls/powershell）——
        //
        // `--tools` 是**替换语义**：列进去它就进 pi 的 registry，模型能直接调，
        // 而它们**原生执行**，**不经过** orbcat 的 `permission.rs` 文件网关与
        // `command_policy.rs` 命令策略 ⇒ 权限体系出现旁路。
        //
        // 实测（2026-10-23）：pi 的 `bash` RPC 真的执行了命令并返回结果。
        //
        // orbcat 有全套等价物（`orb_read_file` / `orb_write_file` / `orb_edit_file` /
        // `orb_grep_files` / `orb_list_dir` / `orb_run_command`），**都走网关**。
        //
        // ⚠️ 另有一道独立的闸：`--exclude-tools`（见 `pi::PiConfig::exclude_tools`），
        //    优先级最高，把这 8 个从 registry 里**整个剔除** —— 双保险，
        //    防止 `--tools` 语义将来被 pi 改动后漏回去。
        // ── 工具清单：orbcat 私有工具 + 项目 L1 工具 + MCP 工具 ──
        //
        // ⚠️ **绝不列 pi 的内置工具**（read/bash/edit/write/grep/find/ls/powershell）——
        //
        // `--tools` 是**替换语义**：列进去它就进 pi 的 registry，模型能直接调，
        // 而它们**原生执行**，**不经过** orbcat 的 `permission.rs` 文件网关与
        // `command_policy.rs` 命令策略 ⇒ 权限体系出现旁路。
        //
        // 实测（2026-10-23）：pi 的 `bash` RPC 真的执行了命令并返回结果。
        //
        // orbcat 有全套等价物（`orb_read_file` / `orb_write_file` / `orb_edit_file` /
        // `orb_grep_files` / `orb_list_dir` / `orb_run_command`），**都走网关**。
        //
        // ⚠️ 另有一道独立的闸：`--exclude-tools`（见 `pi::PiConfig::exclude_tools`），
        //    优先级最高，把这 8 个从 registry 里**整个剔除** —— 双保险，
        //    防止 `--tools` 语义将来被 pi 改动后漏回去。
        let bridge_specs = crate::tools::bridge_specs_for_dir(&self.data_dir);
        // MCP 工具清单由**宿主**取（进程内直连），pi 自己不连 MCP server。
        let mcp_specs = fetch_mcp_tool_specs(&self.data_dir).await;
        let tools = build_tools_whitelist(&bridge_specs, &mcp_specs);
        // 🔴 **关掉 pi 的原生 MCP（单通道，硬要求）**
        //
        // 用户 2026-10-24 拍板「走宿主桥，orbcat 本身作为 mcp 网管给 pi 提供 mcp」：
        // 原生那条路**绕过宿主权限闸**（工具调用走 pi 的 MCP client，
        // 不进 Rust 的 execute）⇒ 授权卡出不来、审计没行、事后禁用对
        // 在跑的 pi 无效 —— 宿主对 MCP **完全失明**。
        //
        // 现在：① 清单由宿主取（上面 `fetch_mcp_tool_specs`），
        //      ② 执行回宿主 bridge ⇒ 每次调用都过权限闸。
        //
        // ⚠️ 光「不调 sync_mcp」**不够** —— agent 目录里可能还留着上一轮写的
        // `mcp.json` ⇒ pi 照样会连 ⇒ 必须显式删掉。
        if disable_native_mcp(&self.agent_dir) {
            eprintln!("[orbcat][pi] 已移除 pi 的原生 MCP 配置（单通道：MCP 走宿主桥）");
        }
        cfg.tools = tools;

        // ── 安全自检：白名单里绝不能出现 pi 内置工具 ──
        //
        // 防止以后有人「顺手把 read/bash 加回去」—— 那等于把权限网关的旁路
        // 又打开一次。这道检查是**编译期之外**的最后一道：写错了直接起不来，
        // 而不是等真机被绕过一次才发现。
        for b in crate::pi::PI_BUILTIN_TOOLS {
            if cfg.tools.iter().any(|t| t == b) {
                return Err(format!(
                    "--tools 白名单里出现了 pi 内置工具 `{b}` —— 它原生执行、绕过文件网关与命令策略"
                ));
            }
        }
        // 🔴 单通道自检：白名单里**不许**有 `mcp__*` 裸名。
        // 有 = pi 原生 MCP 还在放行 = 模型有第二条路能调 MCP = 闸门形同虚设。
        if let Some(bad) = cfg.tools.iter().find(|t| t.starts_with("mcp__")) {
            return Err(format!(
                "--tools 白名单里出现了 MCP 裸名 `{bad}` —— pi 原生 MCP 必须关掉（MCP 只能走宿主桥）"
            ));
        }
        // 排除闸也必须真的带上（`PiConfig::new` 默认就有，但别被谁清掉了）
        // 排除闸也必须真的带上（`PiConfig::new` 默认就有，但别被谁清掉了）
        for b in crate::pi::PI_BUILTIN_TOOLS {
            if !cfg.exclude_tools.iter().any(|t| t == b) {
                return Err(format!(
                    "--exclude-tools 里缺少 pi 内置工具 `{b}` —— 排除闸失效，内置工具会回到 registry"
                ));
            }
        }

        // 桥端口 + 工具清单 → 通过环境变量传给 extension
        cfg.bridge_port = bridge.port();
        // ⚠️ MCP 规格也要进来 —— 桥侧（`orbcat-tools.ts`）给所有工具加 `orb_` 前缀
        // ⇒ 模型看到的是 `orb_mcp__<server>__<tool>`。
        let mut all_specs = bridge_specs;
        all_specs.extend(mcp_specs);
        cfg.tool_specs = specs_to_json(&all_specs);

        // 权限闸门 extension（走 --extension 一次性加载）
        let ext = self.runtime.join("pi").join("extensions").join("orbcat-gate.ts");
        if ext.is_file() {
            cfg.extra_args.push("--extension".into());
            cfg.extra_args.push(ext.to_string_lossy().to_string());
        }
        // 工具桥 extension
        let ext2 = self.runtime.join("pi").join("extensions").join("orbcat-tools.ts");
        if ext2.is_file() {
            cfg.extra_args.push("--extension".into());
            cfg.extra_args.push(ext2.to_string_lossy().to_string());
        }
        // system prompt 接管 extension
        //
        // ⚠️ **顺序很重要**：prompt 那个必须**排在最后** ——
        // `before_agent_start` 的 handler 按注册顺序跑，后面的能看到前面的改动。
        // 放在最后 = 我们的替换是最终结果。
        let ext3 = self.runtime.join("pi").join("extensions").join("orbcat-prompt.ts");
        if ext3.is_file() {
            cfg.extra_args.push("--extension".into());
            cfg.extra_args.push(ext3.to_string_lossy().to_string());
        } else {
            eprintln!("[orbcat] ⚠️ 找不到 orbcat-prompt.ts —— pi 内核会用它自己的默认 prompt（人格与记忆不生效）");
        }

        Ok(cfg)
    }

    /// 把 orbcat 的 `models.json` 同步成 pi 的 `models.json`。
    ///
    /// 两边格式不同：
    /// - orbcat：`[{id, vendor, url, apiKey, supportsToolCall, supportsImages, maxInputTokens, ...}]`
    /// - pi：`{providers: {pid: {baseUrl, api, apiKey, models: [{id, contextWindow, ...}]}}}`
    async fn sync_models(&self) -> Result<Option<(String, String)>, String> {
        let _ = std::fs::create_dir_all(&self.agent_dir);
        let dst = self.agent_dir.join("models.json");

        // 读 orbcat 的模型清单
        let src = self.data_dir.join("models.json");
        let Ok(raw) = std::fs::read_to_string(&src) else {
            return Ok(None); // 没有就跳过（pi 用自己的 catalog）
        };
        let Ok(list) = serde_json::from_str::<Vec<Value>>(&raw) else {
            return Ok(None);
        };

        // 按 (url, apiKey) 分组成 provider
        let mut providers: std::collections::BTreeMap<String, Value> = Default::default();
        let mut idx: std::collections::BTreeMap<(String, String), String> = Default::default();

        // ⚠️ 用**借用**遍历（不是 `for m in list`）——
        // 下面注入临时 provider 时还要查 `list` 里同名模型的 `supportsImages`。
        for m in &list {
            let Some(url) = m.get("url").and_then(|x| x.as_str()) else {
                continue;
            };
            if !url.starts_with("http") {
                continue;
            }
            let key = m.get("apiKey").and_then(|x| x.as_str()).unwrap_or("");
            let pk = (url.to_string(), key.to_string());
            let pid = match idx.get(&pk) {
                Some(p) => p.clone(),
                None => {
                    let p = format!("orb{}", providers.len() + 1);
                    idx.insert(pk, p.clone());
                    providers.insert(
                        p.clone(),
                        json_obj(&[
                            ("name", Value::String(format!("orbcat {}", providers.len() + 1))),
                            ("baseUrl", Value::String(url.to_string())),
                            ("api", Value::String("openai-completions".into())),
                            (
                                "apiKey",
                                // ⚠️ **不能是空串** —— pi 的 schema 要求 `minLength: 1`，
                                // 一旦有一个 provider 的 apiKey 是空的，**整个 models.json
                                // 校验失败**，所有 provider 都不加载（表现为
                                // "Unknown provider orbN"）。2026-10-22 实测踩到。
                                // 用 "dummy" 占位（Ollama 那种忽略 key 的端点也这么写）。
                                Value::String(if key.is_empty() {
                                    "dummy".into()
                                } else {
                                    key.to_string()
                                }),
                            ),
                            ("models", Value::Array(vec![])),
                        ]),
                    );
                    p
                }
            };
            // 模型条目
            let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            let mut entry = json_obj(&[
                ("id", Value::String(id.clone())),
                ("name", Value::String(id)),
            ]);
            if let Some(v) = m.get("maxInputTokens").and_then(|x| x.as_u64()) {
                entry["contextWindow"] = Value::from(v);
            }
            // ⚠️ `maxTokens` 必须是 **number**（schema: exclusiveMinimum 0）——
            // orbcat 的 models.json 里它是 `null`（"没上限"），
            // 直接透传会让整个 models.json 校验失败（2026-10-22 实测踩到）。
            if let Some(v) = m.get("maxOutputTokens").and_then(|x| x.as_u64()) {
                entry["maxTokens"] = Value::from(v);
            } else {
                entry["maxTokens"] = Value::from(16384);
            }
            let input = if m.get("supportsImages").and_then(|x| x.as_bool()) == Some(true) {
                json_arr(&["text", "image"])
            } else {
                json_arr(&["text"])
            };
            entry["input"] = input;
            entry["cost"] = json_obj(&[
                ("input", Value::from(0)),
                ("output", Value::from(0)),
                ("cacheRead", Value::from(0)),
                ("cacheWrite", Value::from(0)),
            ]);
            if let Some(p) = providers.get_mut(&pid) {
                if let Some(arr) = p["models"].as_array_mut() {
                    arr.push(entry);
                }
            }
        }

        // ── 先决定"想用哪个模型" ──
        //
        // `model_override` 存的是 **(模型 id, base URL)**，不是 provider 名 ——
        // provider 名（`orb1`/`orb6`…）是按 URL 去重后的**插入顺序**生成的，
        // 外部没法预知（2026-10-22 踩到：以为能按名指定，结果错位）。
        let settings = crate::config::load_settings(&self.data_dir);
        let (want_id, want_url, want_images) = match self.model_override.lock().await.clone() {
            Some((id, url, images)) => (Some(id), Some(url), images),
            None => (
                settings.selected_model.clone(),
                settings.selected_model_url.clone(),
                false,
            ),
        };

        // ── 覆盖的模型在 models.json 里**不存在** → 注入一个临时 provider ──
        //
        // 主要给**测试**用：假模型（`tests/fake_model.rs`）的 URL 不在用户的
        // models.json 里，但 pi 要真的去连 `http://127.0.0.1:<port>/v1`。
        //
        // ⚠️ 必须在**写文件之前**注入 —— 否则写出去的 models.json 里没有它，
        // pi 启动直接报 `Unknown provider "orbx7"`（2026-10-23 实测踩到）。
        if let (Some(wid), Some(wurl)) = (&want_id, &want_url) {
            let exists = providers.values().any(|p| {
                p.get("baseUrl").and_then(|x| x.as_str()) == Some(wurl.as_str())
                    && p.get("models")
                        .and_then(|x| x.as_array())
                        .map(|a| {
                            a.iter()
                                .any(|m| m.get("id").and_then(|x| x.as_str()) == Some(wid.as_str()))
                        })
                        .unwrap_or(false)
            });
            if !exists {
                // ── 这个覆盖模型**支不支持图片**？──
                //
                // pi 按 `model.input` 能力声明决定**要不要把图发给模型**
                // （上游 `openai-completions` chunk：`hasImages && model.input.includes("image")`）
                // ⇒ 声明只有 `["text"]` ⇒ **图被静默丢掉**（不报错）。
                //
                // 判定顺序：调用方显式声明（测试/临时切模型）→ orbcat 清单里
                // 同名模型的 `supportsImages` → 都没有就当不支持（保守）。
                let supports_images = want_images
                    || list.iter().any(|m| {
                        m.get("id").and_then(|x| x.as_str()) == Some(wid.as_str())
                            && m.get("supportsImages").and_then(|x| x.as_bool()) == Some(true)
                    });
                let input = if supports_images {
                    json_arr(&["text", "image"])
                } else {
                    json_arr(&["text"])
                };
                let pid = format!("orbx{}", providers.len() + 1);
                providers.insert(
                    pid,
                    json_obj(&[
                        ("name", Value::String("orbcat override".into())),
                        ("baseUrl", Value::String(wurl.clone())),
                        ("api", Value::String("openai-completions".into())),
                        ("apiKey", Value::String("dummy".into())),
                        (
                            "models",
                            Value::Array(vec![json_obj(&[
                                ("id", Value::String(wid.clone())),
                                ("name", Value::String(wid.clone())),
                                ("contextWindow", Value::from(128000)),
                                ("maxTokens", Value::from(16384)),
                                ("input", input),
                                (
                                    "cost",
                                    json_obj(&[
                                        ("input", Value::from(0)),
                                        ("output", Value::from(0)),
                                        ("cacheRead", Value::from(0)),
                                        ("cacheWrite", Value::from(0)),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                );
            }
        }

        if providers.is_empty() {
            return Ok(None);
        }

        // ── 写 pi 的 models.json ──
        let mut pmap = serde_json::Map::new();
        for (k, v) in &providers {
            pmap.insert(k.clone(), v.clone());
        }
        let doc = json_obj(&[
            (
                "$schema",
                Value::String("https://pi.dev/schemas/models.schema.json".into()),
            ),
            ("providers", Value::Object(pmap)),
        ]);
        let txt = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        write_atomic(&dst, &txt).map_err(|e| format!("写 pi models.json 失败: {e}"))?;

        // ── 选最终用的模型：优先用户在 orbcat 里选的那个（或 override）──
        //
        // 为什么必须显式告诉 pi 用哪个：pi 启动时如果没指定 provider/model，
        // 它的默认模型是 catalog 里的某个内置模型 —— 而那个模型**没有 API key**
        // → `prompt` 直接返回 "No API key found for the selected model"，
        // 一次都跑不起来（2026-10-22 实测踩到）。
        for (pid, p) in providers.iter() {
            let p_url = p.get("baseUrl").and_then(|x| x.as_str()).unwrap_or("");
            let models = p
                .get("models")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            for m in models {
                let mid = m.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let hit = match (&want_id, &want_url) {
                    (Some(w), Some(u)) => mid == w && p_url == u,
                    (Some(w), None) => mid == w,
                    _ => false,
                };
                if hit {
                    return Ok(Some((pid.clone(), mid.to_string())));
                }
            }
        }
        // 没匹配上 → 用第一个 provider 的第一个模型
        for (pid, p) in providers.iter() {
            if let Some(m) = p
                .get("models")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.get("id"))
                .and_then(|x| x.as_str())
            {
                return Ok(Some((pid.clone(), m.to_string())));
            }
        }
        Ok(None)
    }

/// 把 orbcat 的 MCP 服务器配置同步成 pi 的 `mcp.json`。
    ///
    /// ## 为什么**不用桥**
    ///
    /// pi **原生支持 MCP**（`packages/coding-agent/src/extensions/mcp/config.ts`），
    /// 而且工具命名口径与 orbcat **一模一样**：
    /// - orbcat：`mcp__<server>__<tool>`（`mcp.rs:1767`）
    /// - pi：`mcp__<server>__<tool>`（`mcp-servers.ts:157` `mcpNamespace`）
    ///
    /// 所以**不需要把 MCP 工具桥回 Rust** —— 让 pi 自己连、自己调，
    /// 省掉 127 个工具的序列化往返，也省掉一份 schema 转换。
    ///
    /// ## 两边的配置格式
    ///
    /// - orbcat：`agent-data/mcp.json` = `{"servers":[{id,url,command,args,env,enabled,...}]}`
    /// - pi：`<agentDir>/mcp.json` = `{"mcpServers":{"<name>":{url|command,args,env,enabled}}}`
    ///
    /// pi 的 server 名规则：`/^[A-Za-z0-9_-]+$/` —— orbcat 的 id 若含别的字符
    /// 要替换掉（否则**整条 server 被静默丢弃**，只有 `errors` 里能看到）。
    ///
    /// ## exposure 的坑
    ///
    /// pi 默认 `exposure: "codemode"` —— 工具**不直接声明给模型**，
    /// 只能通过 codemode / tool_search 调。orbcat 的习惯是**直接调**，
    /// 所以这里显式写 `"exposure": "direct"`。


    async fn sync_mcp(&self) -> Result<usize, String> {
        let servers = crate::config::resolve_mcp_servers(&self.data_dir);
        let extra = self.extra_mcp.lock().await.clone();

        if servers.is_empty() && extra.is_empty() {
            // 没有 server → 删掉旧文件（否则 pi 会拿上一轮的残留配置去连）
            let dst = self.agent_dir.join("mcp.json");
            let _ = std::fs::remove_file(dst);
            return Ok(0);
        }

        let mut map = serde_json::Map::new();
        let mut n = 0usize;

        // ── 测试注入的 server（先放，真实配置可以覆盖同名项）──
        for t in &extra {
            let name = sanitize_server_name(&t.name);
            if name.is_empty() {
                continue;
            }
            let mut entry = serde_json::Map::new();
            entry.insert("command".into(), Value::String(t.command.clone()));
            entry.insert(
                "args".into(),
                Value::Array(t.args.iter().map(|a| Value::String(a.clone())).collect()),
            );
            if !t.env.is_empty() {
                let mut e = serde_json::Map::new();
                for (k, v) in &t.env {
                    e.insert(k.clone(), Value::String(v.clone()));
                }
                entry.insert("env".into(), Value::Object(e));
            }
            // 测试 server 也一样要 direct —— 否则工具不直接给模型
            entry.insert("exposure".into(), Value::String("direct".into()));
            map.insert(name, Value::Object(entry));
            n += 1;
        }

        for s in &servers {
            // pi 的 server 名只允许字母/数字/`_`/`-`
            let name = sanitize_server_name(&s.id);
            if name.is_empty() {
                continue;
            }
            let mut entry = serde_json::Map::new();
            if s.is_stdio() {
                entry.insert("command".into(), Value::String(s.command.clone()));
                entry.insert(
                    "args".into(),
                    Value::Array(s.args.iter().map(|a| Value::String(a.clone())).collect()),
                );
                if !s.env.is_empty() {
                    let mut e = serde_json::Map::new();
                    for (k, v) in &s.env {
                        e.insert(k.clone(), Value::String(v.clone()));
                    }
                    entry.insert("env".into(), Value::Object(e));
                }
            } else {
                entry.insert("url".into(), Value::String(s.url.clone()));
            }
            if !s.enabled {
                entry.insert("enabled".into(), Value::Bool(false));
            }
            // ⚠️ pi 默认是 "codemode" —— 工具不直接给模型，只能间接调。
            // orbcat 的用户预期是**直接调**，所以显式写 "direct"。
            entry.insert("exposure".into(), Value::String("direct".into()));
            if !s.label.trim().is_empty() {
                entry.insert("description".into(), Value::String(s.label.clone()));
            }
            map.insert(name, Value::Object(entry));
            n += 1;
        }

        let _ = std::fs::create_dir_all(&self.agent_dir);
        let doc = json_obj(&[("mcpServers", Value::Object(map))]);
        let txt = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        write_atomic(&self.agent_dir.join("mcp.json"), &txt)
            .map_err(|e| format!("写 pi mcp.json 失败: {e}"))?;
        Ok(n)
    }

    /// 造宿主回调（pi 向我们提问时调）。
    ///
    /// `confirm_hook` 是**宿主确认通道**：pi 问「能不能做 X」时，
    /// 我们弹一张 orbcat 确认卡等用户（最多 3 分钟）。
    ///
    /// ⚠️ **绝不在这里等用户** —— 这个回调跑在 pi 的 **stdout 读循环**里
    /// （`dispatch` → `self.host(req)`），等下去会卡住 pi 写 stdout ⇒ **死锁**。
    /// ⇒ 立刻返回 [`crate::pi::PiHostReply::Deferred`]，
    ///    真正的答复由另起的任务异步送来（`tokio::spawn` + oneshot）。
    fn make_host(
        confirm_hook: Option<ConfirmFn>,
        session_id: Option<String>,
    ) -> crate::pi::PiHostFn {
        let handle = tokio::runtime::Handle::try_current().ok();
        Arc::new(move |req: PiHostRequest| -> Option<PiHostReply> {
            match req {
                PiHostRequest::Confirm { id: _, title, message } => {
                    // 没有宿主确认通道 ⇒ fail-closed（拒绝）
                    let Some(hook) = confirm_hook.clone() else {
                        eprintln!(
                            "[orbcat] ⚠️ pi 请求确认（宿主没有确认通道，已按**拒绝**处理）：{title} / {message}"
                        );
                        return Some(PiHostReply::Confirm(false));
                    };
                    let Some(handle) = handle.clone() else {
                        eprintln!("[orbcat] ⚠️ pi 请求确认（拿不到 tokio handle，已按**拒绝**处理）：{title}");
                        return Some(PiHostReply::Confirm(false));
                    };
                    // ⚠️ **立即返回** —— 读循环继续跑，答案晚点另路写回。
                    let (deferred, tx) = crate::pi::DeferredReply::pair();
                    let sid = session_id.clone();
                    handle.spawn(async move {
                        // 3 分钟兜底：钩子自己也有超时，这里再加一层，
                        // 绝不让 pi 干等（超时就当拒绝 = fail-closed）。
                        let approved = tokio::time::timeout(
                            std::time::Duration::from_secs(180),
                            hook(title, message, sid),
                        )
                        .await
                        .unwrap_or(false);
                        let _ = tx.send(crate::pi::PiHostReply::Confirm(approved));
                    });
                    Some(PiHostReply::Deferred(deferred))
                }
                PiHostRequest::Select { id: _, .. } => Some(PiHostReply::Select(Some(String::new()))),
                PiHostRequest::Input { .. } => Some(PiHostReply::Input(String::new())),
                PiHostRequest::Ignored { .. } => None,
            }
        })
    }

    /// 跑一轮对话。
    ///
    /// `progress` 回调把 pi 的事件翻译成 orbcat 的 `Progress` 推给前端。
    ///
    /// ## `checkpoint`
    ///
    /// 轮内增量落盘（与自研内核同一条路）。
    ///
    /// ⚠️ **这个回调不是可选的装饰** —— 前端跑完后会**按磁盘重建消息**。
    /// 不落盘的话，模型「写了半截正文 → 又去调工具」那半截正文
    /// 只活在运行中的内存气泡里，一轮跑完就**凭空消失**
    /// （用户原话：「我的中途正文呢」）。
    ///
    /// 落盘形态：`kind: "text"` 的 step（正文）+ `kind: "reasoning"`（思考）。
    pub async fn run(
        &self,
        input: &str,
        images: Vec<String>,
        progress: ProgressFn,
    ) -> Result<PiRun, String> {
        self.run_with_checkpoint(input, images, progress, None).await
    }

    /// 同 [`PiRuntime::run`]，但带轮内增量落盘回调。
    pub async fn run_with_checkpoint(
        &self,
        input: &str,
        images: Vec<String>,
        progress: ProgressFn,
        checkpoint: Option<crate::agent::CheckpointFn>,
    ) -> Result<PiRun, String> {
        // 防重入：同一时刻只跑一轮
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err("上一轮还没跑完".into());
        }
        let result = self
            .run_inner(input, images, progress, checkpoint)
            .await;
        self.busy.store(false, Ordering::SeqCst);
        result
    }

    async fn run_inner(
        &self,
        input: &str,
        images: Vec<String>,
        progress: ProgressFn,
        checkpoint: Option<crate::agent::CheckpointFn>,
    ) -> Result<PiRun, String> {
        let client = self.ensure_running().await?;

        // ── 订阅事件（**必须在发 prompt 之前**，否则会漏掉快速完成的事件）──
        //
        // ⚠️ **每轮一个独立通道**（不是从池子里「取走」一个共享的）。
        //
        // 旧实现是 `client.take_events()`（**take 移走语义**）⇒ 唯一调用点取走后
        // **再没有任何地方归还** ⇒ 同一个 pi 进程（= 同一个会话）的
        // **第二轮必然取不到通道** ⇒ 硬错「事件通道已被占用」。
        // ⇒ **用户第一条有回复、发第二条就报错**（2026-10-23 22:16 守门 CDP 实测，
        //    部署态真 app；pi 是默认路径 ⇒ 必现）。
        //
        // 现在每轮新建一个通道 ⇒ ① 第二轮一定能拿到；② 新通道是空的
        // ⇒ **不吃上一轮残留事件**（比 broadcast 方案更稳：broadcast 的 buffer
        // 会把上一轮的事件留给下一轮订阅者）。
        let (sub_id, mut rx) = client.subscribe_events();

        let result = self
            .run_events(client.clone(), rx, input, images, progress, checkpoint)
            .await;

        // ⚠️ **无论成功失败都要退订** —— 否则订阅者列表会一直涨。
        client.unsubscribe_events(sub_id);
        result
    }

    /// 真正跑一轮（**已订阅好事件通道**）。
    async fn run_events(
        &self,
        client: std::sync::Arc<crate::pi::PiClient>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::pi::PiEvent>,
        input: &str,
        images: Vec<String>,
        progress: ProgressFn,
        checkpoint: Option<crate::agent::CheckpointFn>,
    ) -> Result<PiRun, String> {

        // 把 system prompt 交给 pi：靠 `orbcat-prompt.ts` 的 `before_agent_start`
        // 回调宿主拿（`POST /prompt` → `agent::build_system_prompt_full`）。
        //
        // ⚠️ **不能**用 `--append-system-prompt` —— 那是**启动参数**，运行期改不了。
        //
        // （2026-10-23 更正：这里原来留着一句「先不接管 —— 见下方 TODO」，
        //   那是早期版本的状态。prompt **早已**通过 extension 接管，
        //   见 `ensure_bridge()` 里的 `prompt_handler`。）
        client.prompt(input, images).await?;

        // 收事件直到 settled
        let mut answer = String::new();
        let mut reasoning = String::new();
        let mut steps: Vec<crate::agent::AgentStep> = Vec::new();
        let mut iteration = 0usize;
        let interrupted = false;

        // ── 轮内落盘用的两个累加器 ──
        //
        // 「中途正文」= 模型在一次 turn 里写了字、然后又去调工具。
        // 它必须**单独**落成 `kind:"text"` 的 step ——
        // 否则前端按磁盘重建消息时这段就消失了（用户：「我的中途正文呢」）。
        //
        // 「中途思考」同理，落成 `kind:"reasoning"`。
        let mut mid_text = String::new();
        let mut mid_reasoning = String::new();

        while let Some(ev) = rx.recv().await {
            if pi::is_settled(&ev) {
                break;
            }
            // ── 致命错误：立刻失败，别让它转圈 ──
            //
            // pi 自己的重试判定只认英文文案（`packages/ai/src/utils/retry.ts`），
            // 国内厂商的中文「额度用完」它认不出来 → 当成瞬时限流 →
            // 无限重试 → 整轮卡死（实测 15 分钟）。
            //
            // 这里在 Rust 侧兜住，把错误原样抛给用户。
            //
            // 两条路都要看：
            // 1. `message_end{stopReason:"error"}` —— 单次调用就失败了
            // 2. `auto_retry_start` —— pi 决定重试（错误文案在 `errorMessage`）
            if let Some(msg) = pi::is_fatal_error(&ev) {
                self.busy.store(false, Ordering::SeqCst);
                return Err(format!("模型调用失败：{msg}"));
            }
            if ev.get("type").and_then(|x| x.as_str()) == Some("auto_retry_start") {
                if let Some(err) = ev.get("errorMessage").and_then(|x| x.as_str()) {
                    if pi::is_fatal_error_text(err) {
                        self.busy.store(false, Ordering::SeqCst);
                        return Err(format!("模型调用失败（不可重试）：{err}"));
                    }
                }
            }
            if pi::is_agent_end(&ev) {
                // 最后一轮的正文
                if let Some(t) = pi::assistant_text(&ev) {
                    answer = t;
                }
                continue;
            }
            if let Some(t) = pi::assistant_text(&ev) {
                if !t.is_empty() {
                    answer = t;
                }
            }
            // 翻译成 orbcat 的 Progress
            if let Some(p) = crate::pi_run::translate(&ev, &mut iteration) {
                match &p {
                    Progress::Delta {
                        text: Some(t), ..
                    } => {
                        answer.push_str(t);
                        mid_text.push_str(t);
                    }
                    Progress::Delta {
                        reasoning: Some(r), ..
                    } => {
                        reasoning.push_str(r);
                        mid_reasoning.push_str(r);
                    }
                    Progress::ToolCall { name, args } => {
                        // ── 工具调用前：把之前那段中途正文/思考**落成 step** ──
                        //
                        // 为什么在这落、而不是流式持续落：pi 的 delta 是
                        // **逐 token** 的，每次都落盘 = 每 token 写一次文件。
                        // 在**轮边界**（工具调用点 / turn 结束）落一次，
                        // 既不丢内容，也不把磁盘写爆。
                        //
                        // ── 同时发 `DiscardStream`（与自研内核**同口径**）──
                        //
                        // 模型这一轮写了正文又去调工具 → 前端把那段正文
                        // **淡化归档**（标注"不是最终答复"），而不是让它
                        // 跟最终答复混在一起。
                        //
                        // ⚠️ 顺序很重要：**先判有没有正文、再 flush**。
                        //    flush 会清空累加器，判晚了就永远发不出去了。
                        let had_mid_text = !mid_text.trim().is_empty();
                        let had_mid_reasoning = !mid_reasoning.trim().is_empty();
                        flush_mid(&mut steps, &mut mid_text, &mut mid_reasoning);
                        if had_mid_text || had_mid_reasoning {
                            progress(Progress::DiscardStream {
                                reason: "模型中途决定调用工具，以下内容不是最终答复".into(),
                            });
                        }
                        steps.push(crate::agent::AgentStep {
                            kind: "tool_call".into(),
                            name: Some(name.clone()),
                            detail: args.clone(),
                            duration_ms: None,
                            images: Vec::new(),
                        });
                        checkpoint_now(&checkpoint, &steps, &answer, &reasoning);
                    }
                    Progress::ToolResult { name, preview, duration_ms } => {
                        steps.push(crate::agent::AgentStep {
                            kind: "tool_result".into(),
                            name: Some(name.clone()),
                            detail: preview.clone(),
                            duration_ms: *duration_ms,
                            images: Vec::new(),
                        });
                        checkpoint_now(&checkpoint, &steps, &answer, &reasoning);
                    }
                    Progress::ToolError { name, message, duration_ms } => {
                        steps.push(crate::agent::AgentStep {
                            kind: "tool_error".into(),
                            name: Some(name.clone()),
                            detail: message.clone(),
                            duration_ms: *duration_ms,
                            images: Vec::new(),
                        });
                        checkpoint_now(&checkpoint, &steps, &answer, &reasoning);
                    }
                    _ => {}
                }
                progress(p);
            }
        }

        // ── 收尾：把最后一段中途正文/思考也落成 step ──
        flush_mid(&mut steps, &mut mid_text, &mut mid_reasoning);

        // 取用量
        let usage = match client.stats().await {
            Ok(v) => parse_usage(&v),
            Err(_) => Default::default(),
        };

        Ok(PiRun {
            answer,
            steps,
            iterations: iteration,
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            interrupted,
            steers: Vec::new(),
            pending_steers: Vec::new(),
            stop_reason: None,
            usage,
            last_prompt_tokens: None,
            token_scale: None,
        })
    }

    /// 插话。
    ///
    /// `images` 是 orbcat 的 data URL 字符串（前端 `pendingImages`），
    /// 转 pi 形状的活儿在 `pi::to_pi_images` 里做。
    pub async fn steer(&self, message: &str, images: Vec<String>) -> Result<(), String> {
        let client = self.ensure_running().await?;
        client.steer(message, images).await
    }

    /// 中断当前运行。
    pub async fn abort(&self) -> Result<(), String> {
        if let Some(c) = self.client.lock().await.clone() {
            c.set_aborted(true);
            c.abort().await?;
        }
        Ok(())
    }

    /// 手动触发压缩。
    pub async fn compact(&self) -> Result<Value, String> {
        let client = self.ensure_running().await?;
        client.compact(None).await
    }

    /// 切模型。
    pub async fn set_model(&self, provider: &str, model: &str) -> Result<Value, String> {
        let client = self.ensure_running().await?;
        client.set_model(provider, model).await
    }

    /// 工具桥（供 extension 连）。
    pub fn bridge(&self) -> Option<Arc<ToolBridge>> {
        self.bridge.try_lock().ok().and_then(|g| g.clone())
    }

    /// 起（或复用）工具桥。
    ///
    /// 桥把 orbcat 的 34 个工具接给 pi 的 extension（`orbcat-tools.ts`）。
    /// 端口通过环境变量 `ORB_BRIDGE_PORT` 传给 pi 子进程。
    ///
    /// 幂等：已经在跑就直接返回。
    pub async fn ensure_bridge(&self) -> Result<Arc<ToolBridge>, String> {
        if let Some(b) = self.bridge.lock().await.clone() {
            return Ok(b);
        }
        let data_dir = self.data_dir.clone();
        let handler: ToolHandler = Arc::new(move |tool, args| {
            // 工具名去掉 `orb_` 前缀 —— 那是给 pi 侧避免撞名用的
            let real = tool.strip_prefix("orb_").unwrap_or(tool);
            // 桥是**同步**回调（HTTP 线程），但 `execute` 是 async。
            // 用一个独立 runtime 把 future 跑完 —— 桥的线程本来就是
            // 「一次一个请求」的一次性线程，等它跑完是安全的。
            let args = args.clone();
            let name = real.to_string();
            let dd = data_dir.clone();
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(async move {
                    crate::tools::execute_bridge(&name, &args, &dd).await
                }),
                Err(e) => Err(format!("起工具执行 runtime 失败: {e}")),
            }
        });
        let gate_dir = self.data_dir.clone();
        // ⚠️ **会话 id 要用「这个 pi 进程自己的」**，不是用户当前正在看的那个。
        // 多会话并行时两者会不一样（用户可能在 B 会话看 A 的 run）。
        let gate_sid = self.session_id.clone();
        // ⚠️ **P2 根因：不要在建桥那刻把宿主 gate 抄成快照**。
        //
        // 旧写法：`let host_gate = self.gate.lock().await.clone();`（建桥时抄走）
        // ⇒ `set_gate` 若在**建桥之后**才调用（PiPool 里很常见：先 `acquire`
        //    建出运行时、之后 `pool.set_gate` 才设），那些运行时的桥**永远拿不到
        //    宿主 gate** ⇒ 落到静态裁决分支 ⇒ **MCP 授权静默降级**成
        //    静态拒绝：**用户永远等不到确认卡**，模型只看到「需要授权」，
        //    而且**没有任何日志**说明降级发生过。
        //
        // ⇒ 改成**每次调用现取**（见 `build_gate`）。
        let gate = build_gate(&self.gate, gate_sid, gate_dir);
        // system prompt 由**宿主**提供 —— pi 自己的默认 prompt 是「coding agent」
        // 那一套，与 orbcat 的「私人助手」定位、人格、记忆体系完全不同。
        //
        // 复用旧内核那条拼装路（`agent::build_system_prompt_full`）：
        // 四文件记忆 + 项目记忆 + 记忆使用规则 + 技能命中，全是同一份逻辑 ——
        // **两个内核看到的 prompt 逐字一致**，不会出现「切了内核人格就变了」。
        //
        // ⚠️ `session_id` 与 `ptc_sdk` 由宿主在开跑前算好放进 `prompt_ctx`
        //    （见 [`PromptCtx`]）—— 这两样决定 per-session mode 与 PTC 工具声明，
        //    漏了就是「pi 内核下的 prompt 比自研内核少一截」。
        let prompt_dir = self.data_dir.clone();
        let prompt_ctx = self.prompt_ctx.clone();
        let prompt_handler: crate::pi_bridge::PromptHandler = Arc::new(move |user_text: &str| {
            let ctx = prompt_ctx
                .lock()
                .ok()
                .map(|g| (*g).clone())
                .unwrap_or_default();
            crate::agent::build_system_prompt_parts(
                &prompt_dir,
                user_text,
                ctx.ptc_sdk.as_deref(),
                ctx.session_id.as_deref(),
            )
        });

        let bridge = ToolBridge::start(handler, gate, prompt_handler)
            .map_err(|e| format!("起工具桥失败: {e}"))?;
        *self.bridge.lock().await = Some(bridge.clone());
        Ok(bridge)
    }
}

/// 造桥用的**权限裁决器**。
///
/// ⚠️ **P2 核心：宿主 gate 必须「每次调用现取」，不能在建桥那刻抄成快照。**
///
/// 旧实现（建桥时 `self.gate.lock().await.clone()`）的问题：
/// `set_gate` 若在**建桥之后**才调用（PiPool 里很常见：先 `acquire` 建出运行时、
/// 之后 `pool.set_gate` 才设），那些运行时的桥**永远拿不到宿主 gate**
/// ⇒ 落到静态裁决分支 ⇒ **MCP 授权静默降级**成静态拒绝：
/// **用户永远等不到确认卡**，模型只看到「需要授权」，而且**没有任何日志**
/// 说明降级发生过。
///
/// 现在：闭包持有 `Arc<Mutex<Option<GateFn>>>`，每次调用现取。
/// 桥的 handler 跑在**同步线程**里 ⇒ `std::sync::Mutex::lock()` 在任何线程都安全
/// （不会在 async 上下文里 panic，也不会阻塞读循环）。
pub(crate) fn build_gate(
    gate_cell: &std::sync::Arc<std::sync::Mutex<Option<GateFn>>>,
    gate_sid: Option<String>,
    gate_dir: std::path::PathBuf,
) -> crate::pi_bridge::GateHandler {
    let gate_cell = gate_cell.clone();
    Arc::new(move |tool, args| {
        let real = tool.strip_prefix("orb_").unwrap_or(tool);
        // 每次现取 ⇒ 后设的 gate 也能生效
        let host_gate = gate_cell.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // 有宿主 gate → 用它（能弹卡、能走真实的 mcp_grants / 确认卡流程）
        if let Some(g) = host_gate {
            let real = real.to_string();
            let args = args.clone();
            let sid = gate_sid.clone();
            // gate 是 async（可能阻塞在确认卡上）→ 桥是同步线程，
            // 用独立 runtime 把 future 跑完（与工具执行那条路同口径）。
            return match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => Ok(rt.block_on(async move { g(&real, &args, sid).await })),
                Err(e) => Err(format!("起权限裁决 runtime 失败: {e}")),
            };
        }
        // 没有宿主 gate → 退回本地静态裁决（文件网关 + MCP grants）
        crate::tools::gate_bridge(real, args, &gate_dir)
    })
}

// ---------------------------------------------------------------------------
// 会话池（多会话并行）
// ---------------------------------------------------------------------------

/// 宿主共享的 MCP 上下文（**进程级**）。
///
/// ## 为什么要有这个
///
/// (c) 形态：PiRuntime 自己连一次 MCP server 取清单 ⇒ 宿主出现**两个** MCP 客户端
/// （server 侧两个 session）⇒ 执行路径唯一（不漏权限闸），但**不干净**。
/// (a) 形态：宿主把**自己的** registry 与 perm hub 共享出来 ⇒ **单一来源**。
///
/// ## 共享安全吗
///
/// - `ToolRegistry` 内部可变状态自带锁（`Arc<Mutex<_>>`）⇒ 共享没问题。
/// - `PermHub` 内部所有可变状态也自带 Mutex，`emitter` 是 `OnceLock`（setup 注入）
///   ⇒ **它本来就是设计成进程级共享的**。
/// - 权限请求靠 **`session_id` 区分**（`ask_mcp_tool(name, args, session_id)` 的第三个参数）
///   ⇒ 不需要「按会话找 hub」那套东西：一个共享 hub + 真实 session_id 就够。
pub struct HostMcp {
    /// 宿主的 MCP 注册表（`Arc<Mutex<ToolRegistry>>`）
    pub mcp: Arc<tokio::sync::Mutex<crate::mcp::ToolRegistry>>,
    /// 宿主的权限中枢（进程级单例）
    pub perm: Arc<crate::perm_request::PermHub>,
}

static HOST_MCP: std::sync::OnceLock<HostMcp> = std::sync::OnceLock::new();

/// 注册宿主共享的 MCP 上下文（**宿主在 `setup` 里调一次**）。
///
/// ```ignore
/// // lib.rs 的 setup 里：
/// pi_runtime::register_host_mcp(state.mcp.clone(), state.perm.clone());
/// ```
///
/// ## 现在是什么状态
///
/// ⚠️ **宿主还没接**（后端在做 `execute_bridge_full`）⇒ 静态槽是空的
/// ⇒ 清单侧自动回落 (c)（自建 registry + 5s 超时 + 降级）⇒ 行为不变、零风险。
/// 宿主一接上，这里**自动**切到 (a) —— **不需要再改任何代码**。
pub fn register_host_mcp(
    mcp: Arc<tokio::sync::Mutex<crate::mcp::ToolRegistry>>,
    perm: Arc<crate::perm_request::PermHub>,
) {
    let _ = HOST_MCP.set(HostMcp { mcp, perm });
    eprintln!("[orbcat][pi] 已注册宿主共享 MCP 上下文（MCP 清单走单一来源）");
}

/// 取宿主共享的 MCP 上下文（**未注册 ⇒ None** ⇒ 调用方走 (c) 兜底）。
pub fn host_mcp() -> Option<&'static HostMcp> {
    HOST_MCP.get()
}

/// MCP 工具规格（**只给 pi 一份声明，不给它一条执行路**）。
///
/// ## 为什么要有这个函数
///
/// MCP 走**宿主桥**（单通道）⇒ pi 侧不再自己连 MCP server ⇒
/// pi 必须**从宿主拿到工具声明**才能把 MCP 工具声明给模型
/// （否则模型只看到 orbcat 私有工具，完全不知道有 MCP 能力）。
///
/// ## 口径必须与 `tools::all_tool_specs` 的 MCP 段**逐项同源**
///
/// 同名、同描述、同 schema —— 否则模型在 pi 侧按 A 的参数调，
/// 宿主网关按 B 解析 ⇒ 表现为「参数莫名其妙」。有测试守着
/// （`mcp_tool_specs_与tools_all_tool_specs同源`）。
///
/// ## 为什么不能直接调 `tools::all_tool_specs`
///
/// 那个函数返回**全部**工具（内置 + 项目 + MCP），
/// 而 `cfg.tool_specs` 这条通道**已经**由 `bridge_specs_for_dir` 提供了前两批
/// ⇒ 再塞一遍会重复注册（同名后者覆盖前者）。
pub fn mcp_tool_specs(reg: &crate::mcp::ToolRegistry) -> Vec<crate::tools::ToolSpec> {
    let mut specs = Vec::new();
    for g in reg.groups().iter().filter(|g| reg.is_active(&g.name)) {
        for t in &g.tools {
            // 组名 = server id，工具名原样透传（与 tools.rs 的 MCP 段同一口径）
            let full = crate::mcp::mcp_tool_full_name(&g.server_id, &t.name);
            let mut desc = t.description.trim().to_string();
            if t.is_destructive() {
                desc.push_str(" ⚠️ 此操作会修改数据或执行命令，调用前请确认用户意图。");
            }
            specs.push(crate::tools::ToolSpec {
                name: full,
                description: desc,
                parameters: t.input_schema.clone(),
            });
        }
    }
    specs
}

/// 拉一份 MCP 工具清单（**进程内直连**，不走 HTTP 端口）。
///
/// ## 为什么在 Rust 侧拉，而不是让 pi 自己连
///
/// 单通道：pi **不再自己连 MCP server**（执行一律回宿主桥）。
/// 但 pi 仍需要工具**声明**才能把 MCP 工具告诉模型 ⇒ 清单由宿主取、宿主给。
///
/// ## ⚠️ 这是「第二份」MCP 连接
///
/// 宿主自己的 registry（`AppState.mcp`）在 `lib.rs` 里，而 `PiRuntime`
/// 只有 `data_dir` ⇒ 这里只能**自己连一次**取清单。
/// **执行仍然只有一条路**（宿主 bridge ⇒ 宿主 registry）⇒ 权限闸不失明。
///
/// 若后端后续把宿主 registry 共享出来（`PiPool` 注入），
/// 这里应改成直接读那份 —— 单一来源，也省掉一次连接。
///
/// ## 失败必须降级，不能连累 pi
///
/// MCP server 挂了 / 超时 ⇒ 清单为空 ⇒ pi 照常起（只是这轮没有 MCP 工具）。
/// 反过来做就会变成「MCP 不可用 ⇒ 整个 pi 起不来」。
pub async fn fetch_mcp_tool_specs(data_dir: &Path) -> Vec<crate::tools::ToolSpec> {
    // ── ① 宿主共享的 registry（(a) 形态）优先 ──
    //
    // 单一来源、实时（宿主改了 MCP 配置下一轮就生效）、**零额外连接**。
    // 宿主还没注册 ⇒ 落到下面的 (c) 兜底。
    if let Some(h) = host_mcp() {
        let reg = h.mcp.lock().await;
        return mcp_tool_specs(&reg);
    }

    // ── 缓存命中就直接用（别每个会话都连一遍 MCP server）──
    //
    // ⚠️ (c) 的代价：这里是**第二份** MCP 连接（宿主自己的 registry 在
    // `lib.rs` 里，`PiRuntime` 拿不到）⇒ 每个 pi 进程都去连一次的话，
    // 开 N 个会话就是 N 次连接 + N 组 stdio 子进程。
    // ⇒ 进程级缓存 + TTL 把它压成「一个宿主进程最多一次」。
    //
    // ⚠️ TTL 到期或 MCP 配置变更后清单会过期 ⇒ (a) 落地后（宿主共享 registry）
    // 这里应改成直接读那份 ⇒ 既是单一来源，也永远不过期。
    {
        let g = MCP_SPEC_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((at, raw)) = g.as_ref() {
            if at.elapsed() < MCP_SPEC_TTL {
                return raw
                    .iter()
                    .map(|(n, d, p)| crate::tools::ToolSpec {
                        name: n.clone(),
                        description: d.clone(),
                        parameters: p.clone(),
                    })
                    .collect();
            }
        }
    }
    let raw = fetch_mcp_tool_specs_raw(data_dir).await;
    *MCP_SPEC_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), raw.clone()));
    raw.into_iter()
        .map(|(n, d, p)| crate::tools::ToolSpec {
            name: n,
            description: d,
            parameters: p,
        })
        .collect()
}

/// 清单缓存（进程级）。`(时刻, 工具规格三元组)`。
///
/// ⚠️ 存的是**三元组**而不是 `ToolSpec` —— `ToolSpec` 没有 `Clone`
/// （它在 `tools.rs`，后端域），缓存一份就克隆不出来。
static MCP_SPEC_CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<(String, String, Value)>)>> =
    std::sync::Mutex::new(None);

/// 清单缓存有效期。
const MCP_SPEC_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// 真去拉一次清单（**不做缓存**）。
async fn fetch_mcp_tool_specs_raw(data_dir: &Path) -> Vec<(String, String, Value)> {
    let servers = crate::config::resolve_mcp_servers(data_dir);
    if servers.is_empty() {
        return Vec::new();
    }
    let mut reg = crate::mcp::ToolRegistry::with_servers(&servers);
    let clients = reg.clients_clone();
    // ⚠️ **不要提前解包 Result** —— `apply_snapshot_with_settings` 吃的就是
    // `Result<(..)>`，它内部负责「拉取失败 ⇒ 清空组」这条降级路径。
    let snap = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::mcp::ToolRegistry::fetch_snapshot(&clients),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[orbcat][pi] MCP 工具清单拉取超时 5s（本轮没有 MCP 工具）");
            return Vec::new();
        }
    };
    if let Err(e) = &snap {
        eprintln!("[orbcat][pi] MCP 工具清单拉取失败（本轮没有 MCP 工具）：{e}");
    }
    let settings = crate::config::load_settings(data_dir);
    reg.apply_snapshot_with_settings(snap, &settings);
    mcp_tool_specs(&reg)
        .into_iter()
        .map(|s| (s.name, s.description, s.parameters))
        .collect()
}

/// pi 的 `--tools` 白名单。
///
/// ## 口径
///
/// - **orbcat 私有工具**：`orb_<name>`（前缀避免撞 pi 内置名）
/// - **项目 L1 工具**：`orb_<name>`（名字随 bundle 变 ⇒ 现取）
/// - **MCP 工具**：`orb_mcp__<server>__<tool>`（**只有 `orb_` 前缀版**）
///
/// ## 🔴 为什么 MCP 必须是 `orb_mcp__*` 而不是 `mcp__*`
///
/// `mcp__*` 是 **pi 原生 MCP** 的命名空间 ⇒ 白名单里放它 = 放行原生那条路
/// ⇒ 模型有两条路能调 MCP，宿主权限闸形同虚设。
/// 桥侧（`orbcat-tools.ts`）给所有工具加 `orb_` 前缀，
/// Rust 侧收到后用 `strip_prefix("orb_")` 还原 ⇒ 宿主 execute 看到的仍是
/// `mcp__<server>__<tool>` ⇒ **正好进 `mcp::parse_mcp_tool_name` 的网关分支**。
fn build_tools_whitelist(
    bridge_specs: &[crate::tools::ToolSpec],
    mcp_specs: &[crate::tools::ToolSpec],
) -> Vec<String> {
    let mut tools: Vec<String> = Vec::new();
    for name in crate::tools::BUILTIN_TOOL_NAMES {
        tools.push(format!("orb_{name}"));
    }
    for spec in bridge_specs {
        // `fetch_url` / `web_search` 已在 BUILTIN_TOOL_NAMES 里（上面加过了）
        if crate::tools::BUILTIN_TOOL_NAMES.contains(&spec.name.as_str()) {
            continue;
        }
        tools.push(format!("orb_{}", spec.name));
    }
    for spec in mcp_specs {
        tools.push(format!("orb_{}", spec.name));
    }
    tools
}

/// 🔴 关掉 pi 的原生 MCP（**单通道**，硬要求）。
///
/// ## 为什么不能只是「不调 `sync_mcp`」
///
/// pi 的 agent 目录里**可能还留着**上一轮写的 `mcp.json`
/// ⇒ pi 照样会连、原生通道照样开着 ⇒ 模型有两条路能调 MCP。
/// ⇒ 必须**显式删掉**那个文件。
///
/// ⚠️ `sync_mcp()` **函数本身保留**（别处可能用），只是不再用它给 pi 配 MCP。
fn disable_native_mcp(agent_dir: &Path) -> bool {
    let cfg = agent_dir.join("mcp.json");
    if !cfg.is_file() {
        return false;
    }
    match std::fs::remove_file(&cfg) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[orbcat][pi] ⚠️ 删不掉 pi 的原生 MCP 配置（原生通道可能还开着）：{e}");
            false
        }
    }
}

/// pi 内核的**会话池** —— 每个会话一个 pi 进程。
///
/// ## 为什么必须是「一进程一会话」
///
/// pi 的 RPC 模式**一个进程只有一个会话**：`new_session` 是**换绑**当前会话，
/// 不是开并存会话（实测，见 `docs/explore/pi-upstream-context-interaction.md`）。
///
/// 而 orbcat 有一条**已拍板**的产品能力：多会话并行 run ——「切到别的会话能干活」
/// （`allmemory/MEMORY.md`，2026-09-26 用户拍板）。自研内核靠
/// `agent::RunRegistry`（`session_id → RunSlot`）实现。
///
/// ⇒ pi 侧要做到同一件事，只能**开多个 pi 进程**。
///
/// ## 上限为什么是 2
///
/// 本机只剩 **2.8 GB 空闲内存**（实测 2026-10-23），而 pi 是 Node 进程，
/// 基线常驻约 **40~80 MB**。开太多会把机器拖垮 —— 所以**默认只允许 2 个**，
/// 超了直接报错让用户关掉一个，而不是悄悄把机器榨干。
///
/// ⚠️ 不用 `#[derive(Clone)]` —— 里面有 `AtomicUsize`（不是 `Clone`）。
/// 需要共享就包 `Arc`（调用方本来就这么用）。
pub struct PiPool {
    data_dir: PathBuf,
    runtime: PathBuf,
    agent_dir: PathBuf,
    /// `session_id` → 该会话的 pi 运行时
    sessions: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<PiRuntime>>>>,
    /// 权限裁决回调（创建新运行时时套上）
    ///
    /// ⚠️ 用 `std::sync::Mutex` 而不是 `tokio::sync::Mutex`：
    /// Tauri 的 `setup` 闭包是**同步**的，没法 await。
    /// 这里只在启动时写一次、创建运行时时读一次，临界区里没有 await ⇒ 安全。
    gate: std::sync::Arc<std::sync::Mutex<Option<GateFn>>>,
    /// 宿主确认钩子（pi 的确认框 → 我们的确认卡）
    confirm_hook: std::sync::Arc<std::sync::Mutex<Option<ConfirmFn>>>,
    /// 最多同时存在几个 pi 进程
    max_live: std::sync::atomic::AtomicUsize,
}

impl PiPool {
    /// 新建会话池。
    pub fn new(data_dir: PathBuf, runtime: PathBuf) -> Arc<Self> {
        let agent_dir = data_dir.join("pi");
        Arc::new(Self {
            data_dir,
            runtime,
            agent_dir,
            sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            gate: std::sync::Arc::new(std::sync::Mutex::new(None)),
            confirm_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
            max_live: std::sync::atomic::AtomicUsize::new(2),
        })
    }

    /// 设置权限裁决回调。
    ///
    /// **同步** —— Tauri 的 `setup` 闭包是同步的，没法 await。
    ///
    /// ⚠️ **P2：必须同时套到「已经存在」的运行时上** ——
    /// 旧实现只写池子自己那份（`pool.gate`），而**已经建出来的**运行时
    /// 各自的 gate 仍是 `None` ⇒ 它们的桥会落到静态裁决分支
    /// ⇒ **MCP 授权静默降级**（用户永远等不到确认卡，且没有任何日志）。
    /// 池里的运行时是**懒创建**的（首次对话才建），所以
    /// 「先 acquire、后 set_gate」是**常见顺序**，不是边角情况。
    pub fn set_gate(&self, f: GateFn) {
        if let Ok(mut g) = self.gate.lock() {
            *g = Some(f.clone());
        }
        // 同步套到已有运行时上（它们的 gate 字段是 `try_lock` 写的）
        let sessions = self.sessions.clone();
        // ⚠️ 必须把 match 结果**绑到变量** —— 直接写成函数尾表达式的话，
        //    guard 的临时量活到函数结束，而 `sessions` 先被 drop（E0597）。
        let applied: usize = match sessions.try_lock() {
            Ok(map) => {
                let n = map.len();
                for rt in map.values() {
                    rt.set_gate(f.clone());
                }
                n
            }
            Err(_) => {
                // ⚠️ **不能静默跳过**（契约 C5：宿主 gate 未接线 ⇒ 阻断 + **显式报错**，
                //    不许当成「允许」）。抢不到锁 ⇒ 至少要留下一行日志。
                eprintln!(
                    "[orbcat] ⚠️ pi 内核：宿主权限闸**没套到**已有运行时上（会话表锁被占）—— \
                     那些会话的 MCP 授权会退化成静态拒绝。"
                );
                0
            }
        };
        if applied > 0 {
            eprintln!("[orbcat] pi 内核：已把宿主权限闸套到 {applied} 个已存在的运行时");
        }
    }

    /// 设置宿主确认钩子（池里**新建**的运行时都会套上；已有的也一起套）。
    ///
    /// 与 [`PiPool::set_gate`] 同口径。
    pub fn set_confirm_hook(&self, f: ConfirmFn) {
        if let Ok(mut g) = self.confirm_hook.lock() {
            *g = Some(f.clone());
        }
        let sessions = self.sessions.clone();
        let applied: usize = match sessions.try_lock() {
            Ok(map) => {
                let n = map.len();
                for rt in map.values() {
                    rt.set_confirm_hook(f.clone());
                }
                n
            }
            Err(_) => {
                // ⚠️ **不能静默跳过**（契约 C5：宿主 gate 未接线 ⇒ 阻断 + **显式报错**，
                //    不许当成「允许」）。抢不到锁 ⇒ 至少要留下一行日志。
                eprintln!(
                    "[orbcat] ⚠️ pi 内核：宿主确认通道**没套到**已有运行时上（会话表锁被占）—— \
                     那些会话的 pi 确认框会恒拒绝。"
                );
                0
            }
        };
        if applied > 0 {
            eprintln!("[orbcat] pi 内核：已把宿主确认通道套到 {applied} 个已存在的运行时");
        }
    }

    /// 并发上限。
    pub fn max_live(&self) -> usize {
        self.max_live.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 调整并发上限（至少 1）。
    pub fn set_max_live(&self, n: usize) {
        self.max_live
            .store(n.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// 内置 runtime 目录。
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime
    }

    /// pi 的 agent 目录（`models.json` / `mcp.json` 落这儿，**共享**）。
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// 拿到（或为）某个会话的 pi 运行时。
    ///
    /// - 已有 → 直接复用（pi 进程常驻，避免每轮付 0.4s 冷启动）
    /// - 没有 → 新建一个**属于该会话**的运行时（session 目录独立）
    /// - 超过并发上限 → **报错**（不排队、不静默挤掉别人正在跑的）
    pub async fn acquire(&self, session_id: &str) -> Result<Arc<PiRuntime>, String> {
        // 快路径：已存在
        {
            let g = self.sessions.lock().await;
            if let Some(rt) = g.get(session_id).cloned() {
                return Ok(rt);
            }
        }
        // 慢路径：加锁再判一次（避免并发 acquire 建出两个）
        let mut g = self.sessions.lock().await;
        if let Some(rt) = g.get(session_id).cloned() {
            return Ok(rt);
        }
        let mut evicted: Option<Arc<PiRuntime>> = None;
        if g.len() >= self.max_live() {
            // 满了 → 先找**空闲的**挤掉（不打断正在跑的）。
            //
            // ⚠️ 不能只报错：用户依次开 3 个会话，第 3 个就永远起不来，
            // 而第 1 个可能早就跑完了 —— 那是「明明空着却说满了」。
            let idle: Option<String> = g
                .iter()
                .find(|(_, rt)| !rt.is_busy())
                .map(|(k, _)| k.clone());
            match idle {
                Some(k) => {
                    evicted = g.remove(&k);
                }
                None => {
                    let ids: Vec<String> = g.keys().cloned().collect();
                    return Err(format!(
                        "pi 内核同时最多跑 {} 个会话（当前 {} 个都在跑：{}）—— \
                         等一个跑完，或先停掉一个。",
                        self.max_live(),
                        g.len(),
                        ids.join("、")
                    ));
                }
            }
        }
        let rt = PiRuntime::with_session(
            self.data_dir.clone(),
            self.runtime.clone(),
            self.agent_dir.clone(),
            session_id,
        );
        {
            // std Mutex 的 guard 在这个语句结束就drop —— 临界区里没有 await。
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(gate) = gate {
                rt.set_gate(gate);
            }
        }
        {
            // 宿主确认通道同样要套到新运行时上（否则新会话的 pi 确认框没人接）
            let hook = self.confirm_hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(hook) = hook {
                rt.set_confirm_hook(hook);
            }
        }
        g.insert(session_id.to_string(), rt.clone());
        // ⚠️ **放锁之后**再关被挤掉的进程 —— 关进程最多要等 3s，
        // 占着锁会让别的会话的 acquire 一起卡住。
        drop(g);
        if let Some(old) = evicted {
            // 挤掉的进程要**关掉**，否则 node 进程白占 40~80MB
            old.shutdown().await;
        }
        Ok(rt)
    }

    /// 拿到某个会话的运行时（**不新建**）。
    ///
    /// 给 `steer` / `abort` / `compact` 用 —— 它们只该作用于**正在跑的那个会话**。
    pub async fn get(&self, session_id: &str) -> Option<Arc<PiRuntime>> {
        self.sessions.lock().await.get(session_id).cloned()
    }

    /// 当前活着的 pi 进程数。
    pub async fn live_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// 活着的会话 id 列表（诊断用）。
    pub async fn live_sessions(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sessions.lock().await.keys().cloned().collect();
        v.sort();
        v
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 把累积的「中途正文」/「中途思考」落成 step，然后清空累加器。
///
/// ## 为什么要单独落成 step
///
/// 模型一轮里可能「先写了半截话 → 又决定去调工具」。
/// 那半截话**不是**最终答复，但它是真实发生过的输出 ——
/// 前端按磁盘重建消息时，如果它没落盘就**凭空消失**
/// （用户原话：「我的中途正文呢」）。
///
/// 形态对齐自研内核：`kind: "text"` = 中途正文，
/// `kind: "reasoning"` = 中途思考（前端会折叠它）。
fn flush_mid(
steps: &mut Vec<crate::agent::AgentStep>,
mid_text: &mut String,
mid_reasoning: &mut String,
) {
if !mid_reasoning.trim().is_empty() {
    steps.push(crate::agent::AgentStep {
        kind: "reasoning".into(),
        name: None,
        detail: std::mem::take(mid_reasoning),
        duration_ms: None,
        images: Vec::new(),
    });
} else {
    mid_reasoning.clear();
}
if !mid_text.trim().is_empty() {
    steps.push(crate::agent::AgentStep {
        kind: "text".into(),
        name: None,
        detail: std::mem::take(mid_text),
        duration_ms: None,
        images: Vec::new(),
    });
} else {
    mid_text.clear();
}
}

/// 触发一次轮内落盘（没配回调就什么都不做）。
fn checkpoint_now(
cb: &Option<crate::agent::CheckpointFn>,
steps: &[crate::agent::AgentStep],
answer: &str,
reasoning: &str,
) {
if let Some(f) = cb.as_ref() {
    f(steps, answer, reasoning);
}
}

/// **原子**写文件（先写临时文件，再 rename）。
///
/// ## 为什么必须原子
///
/// 多会话并行时**每个会话一个 pi 进程**，但 `models.json` / `mcp.json` 是
/// **共享**的（都在 `agent_dir` 下）—— 两个进程可能同时写。
///
/// `std::fs::write` 是「截断 → 写」：写到一半被另一个进程读走，
/// pi 就会读到**半截 JSON** ⇒ 报 `Unknown provider` / schema 校验失败。
///
/// `rename` 在同一卷上是原子的 ⇒ 读到的永远是完整的旧版本或完整的新版本。
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!(
        "tmp{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 失败就清掉临时文件，别留垃圾
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn json_obj(pairs: &[(&str, Value)]) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

fn json_arr(items: &[&str]) -> Value {
    Value::Array(items.iter().map(|s| Value::String((*s).to_string())).collect())
}

/// 把 orbcat 的 MCP server id 洗成 pi 允许的名字。
///
/// pi 的规则是 `/^[A-Za-z0-9_-]+$/` —— 不合法的名字会让**整条 server
/// 被静默丢弃**（只在 `mcp.json` 的 `errors` 里能看到一句），
/// 表现为"配了 MCP 但一个工具都没有"。
fn sanitize_server_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 从 pi 的 `get_session_stats` 里抽 token 用量。
fn parse_usage(v: &Value) -> crate::llm::TokenUsage {
    let t = v.get("tokens");
    let g = |k: &str| t.and_then(|x| x.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
    crate::llm::TokenUsage {
        prompt: g("input") as u32,
        completion: g("output") as u32,
        total: g("total") as u32,
        reasoning: None,
        cache_hit: g("cacheRead") as u32,
        cache_write: g("cacheWrite") as u32,
    }
}

#[cfg(test)]
mod tests {

    // ---------------- MCP 清单通路（单通道）----------------

    /// 🔴 MCP 规格必须与 `tools::all_tool_specs` 的 MCP 段**逐项同源**。
    ///
    /// 为什么：模型在 pi 侧按 A 的参数调，宿主网关按 B 解析 ⇒ 表现为
    /// 「参数莫名其妙」。两份构造逻辑各写一遍必然漂移 ⇒ 用测试钉住。
    #[test]
    fn mcp_tool_specs_与tools_all_tool_specs同源() {
        let tmp = std::env::temp_dir().join("orbcat_mcp_specs_same");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut reg = crate::mcp::ToolRegistry::from_tools_for_test(vec![
            crate::mcp::McpTool {
                name: "github_1mcp_create_issue".into(),
                description: "Create a new issue in a GitHub repository".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "title": { "type": "string" } }
                }),
                annotations: None,
            },
            crate::mcp::McpTool {
                name: "ssh_1mcp_run-command".into(),
                description: "在远程主机上执行命令".into(),
                input_schema: serde_json::json!({ "type": "object", "properties": {} }),
                annotations: None,
            },
        ]);
        let settings = crate::config::AgentSettings::default();
        reg.restore_active_from_settings_unrestricted(&settings);

        let mine = super::mcp_tool_specs(&reg);
        let theirs = crate::tools::all_tool_specs(Some(&reg), false, &tmp, None)
            .into_iter()
            .filter(|s| s.name.starts_with("mcp__"))
            .collect::<Vec<_>>();

        assert_eq!(mine.len(), 2, "两条 MCP 工具都要在：{:?}", mine.iter().map(|s| &s.name).collect::<Vec<_>>());
        assert_eq!(mine.len(), theirs.len(), "条数必须与 tools.rs 的 MCP 段一致");
        for (a, b) in mine.iter().zip(theirs.iter()) {
            assert_eq!(a.name, b.name, "工具名必须一致");
            assert_eq!(a.description, b.description, "描述必须一致");
            assert_eq!(a.parameters, b.parameters, "参数 schema 必须一致");
        }
        // 名字口径：组名 = server id，工具名原样透传
        assert!(
            mine.iter()
                .any(|s| s.name == "mcp__default__github_1mcp_create_issue"),
            "名字口径必须是 mcp__<组名>__<工具名>：{:?}",
            mine.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 🔴 白名单里**不许**有 `mcp__*` 裸名（单通道的硬要求）。
    ///
    /// `mcp__*` 是 **pi 原生 MCP** 的命名空间 ⇒ 白名单里放它 = 放行原生那条路
    /// ⇒ 模型有两条路能调 MCP，宿主权限闸形同虚设。
    #[test]
    fn build_tools_whitelist_只列orb前缀_没有mcp裸名() {
        let bridge = vec![crate::tools::ToolSpec {
            name: "read_file".into(),
            description: "读文件".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }];
        let mcp = vec![crate::tools::ToolSpec {
            name: "mcp__1mcp__github_1mcp_create_issue".into(),
            description: "建 issue".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }];
        let wl = super::build_tools_whitelist(&bridge, &mcp);

        // MCP 工具带 orb_ 前缀（模型看到的是 orb_mcp__<组>__<工具>）
        assert!(
            wl.iter()
                .any(|t| t == "orb_mcp__1mcp__github_1mcp_create_issue"),
            "MCP 工具应以 orb_ 前缀进白名单：{wl:?}"
        );
        // 🔴 没有任何 mcp__ 裸名
        assert!(
            !wl.iter().any(|t| t.starts_with("mcp__")),
            "白名单里不许有 MCP 裸名：{wl:?}"
        );
        // 🔴 也不许有 mcp__* 通配（那是放行整条原生路）
        assert!(
            !wl.iter().any(|t| t == "mcp__*"),
            "白名单里不许有 mcp__* 通配：{wl:?}"
        );
        // orbcat 私有工具照旧
        assert!(wl.iter().any(|t| t == "orb_read_file"), "{wl:?}");
    }

    /// 🔴 桥侧加 `orb_` 前缀后，**必须还能被宿主的 `parse_mcp_tool_name` 解析**。
    ///
    /// 为什么这条是生死线：模型看到的是 `orb_mcp__<server>__<tool>`；
    /// 桥把它原样传给 Rust（`callRust(name, …)`）⇒ Rust 侧先
    /// `strip_prefix("orb_")` 还原成 `mcp__<server>__<tool>`
    /// ⇒ 才能进 `tools.rs` 的 MCP 网关分支。
    /// **对不上 ⇒ 宿主 execute 认不出、走不进网关分支 ⇒ MCP 工具调不动。**
    #[test]
    fn 白名单里的orb_mcp名_能被宿主parse_mcp_tool_name解析() {
        let full = "mcp__1mcp__github_1mcp_create_issue";
        let orb = format!("orb_{full}");

        // 桥侧：注册给 pi 的名字带 orb_ 前缀
        assert!(orb.starts_with("orb_"), "模型侧看到的名字必须带 orb_ 前缀");

        // Rust 侧：还原后正好是宿主的口径
        let stripped = orb.strip_prefix("orb_").unwrap();
        let (server, tool) =
            crate::mcp::parse_mcp_tool_name(stripped).expect("还原后必须能解析");
        assert_eq!(server, "1mcp", "组名 = server id");
        assert_eq!(tool, "github_1mcp_create_issue", "工具名原样透传");

        // ⚠️ 反面：`mcp__*` 裸名在 pi 侧是**原生 MCP 命名空间** ⇒ 绝不能进白名单
        assert!(
            !full.starts_with("orb_"),
            "裸名不是模型侧该看到的名字"
        );
    }

    /// 🔴 关原生 MCP 必须**删掉残留的 `mcp.json`**。
    ///
    /// 为什么：光「不调 `sync_mcp`」不够 —— agent 目录里可能还留着上一轮写的
    /// `mcp.json` ⇒ pi 照样读它、照样注册原生 MCP 工具 ⇒ 判据 4 直接失效。
    #[test]
    fn disable_native_mcp_删掉残留的mcp_json() {
        let d = std::env::temp_dir().join("orbcat_disable_native_mcp");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let cfg = d.join("mcp.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"x":{"url":"http://127.0.0.1:1/mcp"}}}"#,
        )
        .unwrap();

        assert!(super::disable_native_mcp(&d), "有残留文件时该删掉并返回 true");
        assert!(
            !cfg.exists(),
            "mcp.json 必须被删掉（否则 pi 照样连原生 MCP）"
        );
        // 幂等：再删一次不报错、返回 false
        assert!(
            !super::disable_native_mcp(&d),
            "没有文件时返回 false（幂等）"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    use super::*;

    #[test]
    fn parse_usage_抽token() {
        let v = serde_json::json!({
            "tokens": {"input": 100, "output": 50, "cacheRead": 10, "cacheWrite": 5, "total": 165},
            "cost": 0.25
        });
        let u = parse_usage(&v);
        assert_eq!(u.prompt, 100);
        assert_eq!(u.completion, 50);
        assert_eq!(u.total, 165);
    }

    #[test]
    fn parse_usage_缺字段不炸() {
        let u = parse_usage(&serde_json::json!({}));
        assert_eq!(u.prompt, 0);
        assert_eq!(u.total, 0);
    }

    #[test]
    fn json_obj_构造正确() {
        let o = json_obj(&[("a", Value::from(1)), ("b", Value::from("x"))]);
        assert_eq!(o["a"], 1);
        assert_eq!(o["b"], "x");
    }

    #[test]
    fn server名要洗成pi允许的字符() {
        // pi 的规则：`/^[A-Za-z0-9_-]+$/`
        assert_eq!(sanitize_server_name("1mcp"), "1mcp");
        assert_eq!(sanitize_server_name("ssh"), "ssh");
        assert_eq!(sanitize_server_name("playwright-mcp"), "playwright-mcp");
        // 含非法字符 → 替换成 `_`（不能原样丢，否则整条 server 被静默丢弃）
        assert_eq!(sanitize_server_name("my server"), "my_server");
        assert_eq!(sanitize_server_name("a.b"), "a_b");
        assert_eq!(sanitize_server_name("工具组"), "___");
    }

    /// 🔴 P2 回归测试：**建桥之后**才设的宿主 gate，桥的裁决**必须**用上。
    ///
    /// ## 旧实现为什么坏
    ///
    /// 旧代码在建桥那刻 `self.gate.lock().await.clone()` 把宿主 gate **抄成快照**。
    /// 于是「先建桥、后 `set_gate`」的运行时**永远拿不到**宿主 gate
    /// ⇒ 落到静态裁决分支 ⇒ **MCP 授权静默降级**成静态拒绝：
    /// **用户永远等不到确认卡**，模型只看到「需要授权」，且**没有任何日志**。
    ///
    /// PiPool 的运行时是**懒创建**的（首次对话才建），
    /// 所以「先 `acquire`、后 `pool.set_gate`」是**常见顺序**，不是边角情况。
    ///
    /// ## 判据（副作用式，不是看日志）
    ///
    /// 直接问「桥的裁决器」：同一个 MCP 工具名，
    /// 没设宿主 gate 时被静态拒绝、设了之后被宿主放行 ⇒ 说明它**每次现取**。
    #[test]
    fn P2_建桥之后设的宿主gate_桥的裁决也要用上() {
        let cell = std::sync::Arc::new(std::sync::Mutex::new(None));
        let dir = std::env::temp_dir().join("orbcat-pi-gate-test");
        let _ = std::fs::create_dir_all(&dir);

        // ① 先建桥（宿主 gate 还没设）
        let gate = build_gate(&cell, Some("sess-1".into()), dir.clone());

        // ② 没设宿主 gate ⇒ 静态裁决：MCP 工具没授权 ⇒ 拒绝
        let (ok, _why) = gate("mcp__probe__tool", &serde_json::json!({}))
            .expect("裁决不该报错");
        assert!(
            !ok,
            "没设宿主 gate 时，未授权的 MCP 工具应被静态拒绝（回落分支）"
        );

        // ③ ⚠️ **建桥之后**才设宿主 gate —— 旧实现在这里就露馅了
        let cell2 = cell.clone();
        std::thread::spawn(move || {
            // ⚠️ `std::sync::Mutex::lock()` 返回 `LockResult` ——
            //    用 `unwrap_or_else(|e| e.into_inner())` 而不是 `unwrap()`：
            //    poison 时仍能拿到 guard（静默丢掉等于没设 gate）。
            let mut g = cell2.lock().unwrap_or_else(|e| e.into_inner());
            // ⚠️ 闭包参数**不要自己标类型** —— 交给 `GateFn` 推断，
            //    否则 boxed future 的协变推导不上（E0277）。
            let host: GateFn = std::sync::Arc::new(move |_tool, _args, _sid| {
                Box::pin(async { (true, "宿主放行".to_string()) })
            });
            *g = Some(host);
        })
        .join()
        .expect("设置宿主 gate 的线程不该 panic");

        // ④ 现在必须走宿主裁决 ⇒ 放行
        let (ok, why) = gate("mcp__probe__tool", &serde_json::json!({}))
            .expect("裁决不该报错");
        assert!(
            ok,
            "设了宿主 gate 之后，桥的裁决必须走宿主（旧的快照实现这里会被静态拒绝）"
        );
        assert_eq!(why, "宿主放行");
    }
}

