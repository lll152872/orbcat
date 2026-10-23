/**
 * 前端 ↔ 后端的**数据契约**（唯一来源）。
 *
 * ## 为什么要有这一层
 *
 * 以前这些类型散在 `main.ts` / `models.ts` / `views/recovery.ts` 各自的局部声明里，
 * 谁能改就谁改 —— 加一个字段要翻三个文件，接口对不对只能靠跑起来看。
 *
 * 现在统一放这里：**后端返回什么形状，前端就只认这里声明的形状**。
 * `src/data/source.ts` 的 `DataSource` 接口按这些类型声明方法，
 * 于是「前端要什么数据」变成一份可对照的契约，而不是散落的 invoke 调用。
 *
 * ## 范围
 *
 * 只放**跨边界的数据形状**（后端返回 / 前端发给后端的载荷）。
 * 纯前端的视图模型（`ChatEntry` 气泡条目、`View` 视图枚举、`ModeFormState` 表单态）
 * 留在各自文件里 —— 它们不是契约，搬进来只会把 UI 层和接口层搅在一起。
 *
 * ⚠️ 字段名与后端 Rust 结构**同名同义**；Tauri 会在 snake_case ↔ camelCase 之间
 * 自动转换（Rust `session_id` ⇔ 前端 `sessionId`），所以这里一律用 camelCase。
 */

// ---------------- 会话与消息 ----------------

/** 一次模型调用的 token 用量（后端 `llm::TokenUsage` 的归一化结果） */
export interface TokenUsage {
  prompt: number;
  completion: number;
  /** 思维链 token（部分模型单独给） */
  reasoning?: number;
  /** 缓存命中（读）token */
  cacheHit?: number;
  /** 缓存写入 token */
  cacheWrite?: number;
  total: number;
}

/**
 * 工具 / 思考的一个步骤（后端 `agent.rs` 的 `StoredStep` + 前端合并后的形态）。
 *
 * ⚠️ `result` 只在**前端**合并（`pairToolSteps()`），不落盘 ——
 * 后端落盘仍是 `tool_call` + `tool_result` 两条，时间线上渲染成一行。
 */
export interface AgentStep {
  kind: string;
  name: string | null;
  detail: string;
  /** 工具执行心跳（"⏳ 已运行 12s｜<输出尾巴>"）—— 只在 live 进行中气泡里用。 */
  tick?: string;
  /** 工具耗时（毫秒）—— 只对 tool_call / tool_result / tool_error 有意义。 */
  durationMs?: number;
  /** 插话（kind === "steer"）附带的图片**文件路径**。 */
  images?: string[];
  /** 工具返回预览 —— 只有已配对的 `tool_call` 条目才有。 */
  result?: string;
}

/** 一轮跑完的结果（后端 `agent::AgentRun` / `pi_runtime::PiRun` 的同构体） */
export interface AgentRun {
  answer: string;
  steps: AgentStep[];
  iterations: number;
  /** 本轮的思考过程（多轮时按「【第 N 轮】」分段） */
  reasoning?: string;
  /** 被「停止」打断 —— answer 是半截正文 */
  interrupted?: boolean;
  /** 执行中被消费掉的插话 */
  steers?: { id: string; text: string; images: string[] }[];
  /** 中断时没来得及送达的插话（还原到输入框，别让用户白打字） */
  pendingSteers?: string[];
  stopReason?: string;
}

/** 落盘的一条消息（后端 `sessions.rs` 的 `StoredMsg`） */
export interface StoredMsg {
  role: "user" | "assistant";
  text: string;
  images?: string[];
  steps?: AgentStep[];
  /** 本轮的思考过程。老数据没有这个字段 */
  reasoning?: string;
  /** 本轮被中断（text 是半截） */
  interrupted?: boolean;
  /** **未定稿**的助手消息（"退出不丢轮"机制）—— 进程在轮内被关掉时留下的半截轮。 */
  partial?: boolean;
  /** 插话 id —— UI 不铺气泡，画在 steps 时间线（真实交错） */
  steerId?: string;
  /** 生成本条回答的模型 id（用量统计用）。老数据没有 */
  model?: string;
  /** 本轮问答的 token 用量合计。老数据 / 服务端没给 usage → 无此字段 */
  usage?: TokenUsage;
  at: number;
}

/** 一条会话（后端 `sessions.rs` 的 `Session`） */
export interface Session {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  messages: StoredMsg[];
  /** main = 主聊天（唯一、不可删、可被分叉）；task = 任务会话；fork = 分叉出来的会话 */
  kind: "main" | "task" | "fork";
  /** 分叉来源（只有 kind === "fork" 时有） */
  forkedFrom?: string;
  /** 分叉点：源会话里的第几条消息 */
  forkAt?: number;
  /** 这条会话**自己的** Agent 模式 id（建会话时定死，之后只读）。 */
  mode?: string;
}

/** 会话列表里的一行（后端 `sessions.rs` 的 `SessionMeta`） */
export interface SessionMeta {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  count: number;
  current: boolean;
  /** 主会话固定在列表置顶且不可删；fork 是分叉出来的会话，其余都是任务会话 */
  kind: "main" | "task" | "fork";
  /** 已挂靠的项目名（右键挂靠后显示） */
  projectName?: string | null;
  /** 这条会话的模式 id（老会话回退成默认模式） */
  mode: string;
}

/** `session_state` 的返回：当前会话 + 列表 */
export interface SessionState {
  session: Session;
  list: SessionMeta[];
}

/** 插话确认（后端 `agent::SteerAck`） */
export interface SteerAck {
  id: string;
  queued: number;
}

/** 一条待用户拍板的权限申请（与后端 `perm_request::PendingView` 对应） */
export interface PendingPerm {
  id: string;
  /** "file"（文件级，路径维度） | "command"（命令级，指纹维度） | "ask"（问用户） */
  kind: string;
  // ---- 文件级 ----
  path: string;
  access: string;
  deniedPath: string | null;
  /** 申请范围比被拒路径宽了几层；null = 不是被拒路径的祖先 */
  widenLevels: number | null;
  widened: boolean;
  // ---- 命令级 ----
  /** 命令原文 */
  command: string;
  /** 归一化后的命令（别名/缩写已展开 —— 用户靠它看"实际会执行什么"） */
  normalized: string;
  /** 风险等级 low | medium | high */
  risk: string;
  /** 「可执行名 + 首个子命令」指纹（"记住这类命令"用） */
  headFp: string;
  /** 完整命令指纹（"记住这条命令"用） */
  fullFp: string;
  // ---- 公共 ----
  tier: string;
  reason: string;
  sessionId: string;
  createdAt: number;
  timeoutMs: number;
  // ---- ask_user（kind = "ask"）----
  /** 问题正文 */
  question: string;
  /** 快捷选项（点一下即回答；空 = 自由输入） */
  options: string[];
  /**
   * pi 确认框的**正文**（`kind = "pi"`）。
   *
   * ⚠️ **可选字段**（后端这轮才补上，还没进 release 基线）：
   *   pi 的 `ctx.ui.confirm(title, message)` 只有 title + message 两个字符串，
   *   其中**实质内容在 message**（「允许执行 rm -rf ~/project 吗？」在 message 里，
   *   不在 title 里）⇒ 后端把 message 落进卡片结构（`PendingCmd.message`）。
   *   在后端 cherry-pick 进来之前，这个字段恒为 `undefined`
   *   ⇒ **卡要能容忍它缺失**（空则不渲染那一行）。
   *
   * ⚠️ 语义上**不属于** `question`（那是 ask_user 的问题正文）、也不属于
   *   `normalized`（那是「归一化后的命令」）—— 复用会让前端在三种语义间猜字段。
   */
  message?: string;
}

/** 流式过程事件（后端 emit 的 progress 事件载荷） */
export interface ProgressEvent {
  kind:
    | "thinking"
    | "status"
    | "delta"
    | "toolCall"
    | "toolResult"
    | "toolError"
    | "toolTick"
    | "answering"
    | "discardStream"
    | "steer";
  iteration?: number;
  /** delta / status */
  text?: string;
  /** status：这是否是「值得留在时间线」的状态（限流退避重试）。 */
  retry?: boolean;
  reasoning?: string;
  /** tool* */
  name?: string;
  args?: string;
  preview?: string;
  message?: string;
  /** toolTick：长工具执行心跳（已运行秒数 + 输出尾巴） */
  elapsedSecs?: number;
  tail?: string;
  /** 工具耗时（毫秒） */
  durationMs?: number;
  /** discardStream */
  reason?: string;
  /** steer：被消费掉的那条插话的 id */
  id?: string;
  /** steer：插话附带的图片（文件路径） */
  images?: string[];
}

// ---------------- 模型 ----------------

/** 模型身份 = (Base URL, 接口 id)；同 id 可跨组共存，显示名已废弃 */
export interface ModelView {
  id: string;
  vendor: string;
  url: string;
  supportsToolCall: boolean;
  supportsImages: boolean;
  maxInputTokens?: number | null;
  maxOutputTokens?: number | null;
  hasKey?: boolean;
}

/** 远端拉到的模型信息（`models_fetch_remote` / `models_parse_spec`） */
export interface RemoteModelInfo {
  id: string;
  ownedBy?: string;
  contextLength?: number | null;
  maxOutputLength?: number | null;
  /** 提供商自报的工具调用能力。null/undefined = 对方没声明。 */
  supportsToolCall?: boolean | null;
  /** 提供商自报的看图能力（同上：null = 没声明） */
  supportsImages?: boolean | null;
}

/** 编辑模型时的完整视图（`models_get_edit`） */
export interface ModelEditView {
  id: string;
  vendor: string;
  url: string;
  supportsToolCall: boolean;
  supportsImages: boolean;
  headersText: string;
  hasKey: boolean;
  keyPreview: string;
  maxInputTokens?: number | null;
  maxOutputTokens?: number | null;
}

// ---------------- 记忆 ----------------

/** 一条待批的记忆候选（后端 `memory::Candidate`） */
export interface Candidate {
  id: string;
  content: string;
  source: string;
  created_at: string;
  /** 触发轮次（用户原话，截断过）—— 后端可选，老数据没有 */
  turn_ref?: string | null;
}

/** 记忆页统计（后端 `mem_stats`） */
export interface MemStats {
  /** 当前待批数 */
  pending: number;
  /** 已处理数（已批准 + 已驳回） */
  handled: number;
}

/** 记忆文件（`mem_files`：规则 / 灵魂 / 身份 / 用户 / 记忆） */
export interface MemoryFiles {
  rules: string;
  soul: string;
  identity: string;
  user: string;
  memory: string;
}

/** 项目记忆视图（`mem_projects` 的一行） */
export interface ProjectMemoryView {
  name: string;
  source: string;
  sourceAlive: boolean;
  hasMemory: boolean;
}

// ---------------- 权限 ----------------

/** 一条文件权限规则（后端 `permission::Rule`） */
export interface PermRule {
  prefix: string;
  access: "deny" | "read" | "readwrite" | "full";
  label: string;
}

/**
 * 命令策略（`perm_cmd_policy`）
 *
 * ⚠️ 字段是 **snake_case**（`hard_block`），不是 camelCase ——
 * 后端 `command_policy.rs` 的 `CommandPolicy` **没有** `#[serde(rename_all = "camelCase")]`
 * （对比 `SessionMeta` / `BootstrapReport` 都有）。Tauri 只对**入参**做
 * camelCase ⇄ snake_case 转换，**返回值原样序列化** ⇒ 读 `hardBlock` 永远是 undefined。
 */
export interface CmdPolicy {
  allow: string[];
  hard_block: string[];
}

/** 一条命令授权（`perm_cmd_grants_list`） */
export interface CmdGrantsView {
  fingerprint: string;
  display: string;
  scope: string;
  tier: string;
  remaining?: number | null;
  reason?: string;
}

/** 命令策略判定结果（`perm_cmd_test`） */
export interface CmdVerdictView {
  verdict: "allow" | "ask" | "hard-block" | string;
  reason: string;
  risk: string;
  normalized: string;
}

/** 一条命令审计记录（`perm_cmd_audit_tail`） */
export interface CmdAuditRecord {
  ts: number;
  sequence: number;
  event: string;
  source: string;
  command: string;
  normalized: string;
  cwd: string;
  risk: string;
  exit_code?: number | null;
}

// ---------------- 项目 ----------------

/** pmem.projects 一行（SQLite 索引；记忆正文在 projects/<名>/MEMORY.md） */
export interface ProjectRow {
  id: number;
  name: string;
  rootPath: string;
  createdAt: string;
}

/** 一个技能包（`skills_list` 的一行） */
export interface BundleInfo {
  id: string;
  name: string;
  description: string;
  active: boolean;
  /** false = schema 不认识 / 结构不合法（设置页标黄，可看 error） */
  valid: boolean;
  error?: string | null;
  skillCount: number;
  toolCount: number;
  /** 带权限预设（导入时要提示用户确认） */
  hasPermPreset: boolean;
}

/** 项目列表页整体视图（`project_list`） */
export interface ProjectListView {
  bundles: BundleInfo[];
  active: string | null;
  /** 引用解析失败项：`[类别, id, 原因]` */
  unresolved: [string, string, string][];
  /** permPreset 被整份丢弃的原因（fail-closed） */
  permError: string | null;
  tools: { name: string; description: string; template: string[]; perm: string }[];
}

// ---------------- MCP ----------------

/** 一个 MCP 组的概览 */
export interface McpGroupView {
  name: string;
  summary: string;
  toolCount: number;
  tokens: number;
  active: boolean;
  hasDestructive: boolean;
}

/** MCP 总状态（`mcp_status`） */
export interface McpStatus {
  connected: boolean;
  error: string | null;
  /** 当前网关地址；空串 = 未配置（没有默认值，不配就不启用） */
  url: string;
  /** 活跃工具 schema 的粗略 token 估算（仅展示，无加载上限） */
  activeTokens: number;
  groups: McpGroupView[];
}

/** 一个 MCP server 的配置（`mcp_servers_list`） */
export interface McpServerCfg {
  id: string;
  /** HTTP 形态的网关地址；stdio 形态留空 */
  url: string;
  enabled: boolean;
  label: string;
  /** "auto" | "http" | "stdio"；缺省按 url / command 推断 */
  transport?: "auto" | "http" | "stdio";
  /** stdio 形态：可执行文件 / 脚本路径 */
  command?: string;
  /** stdio 形态：argv（逐参传递，不经 shell） */
  args?: string[];
  /** stdio 形态：附加环境变量 */
  env?: Record<string, string>;
}

// ---------------- 恢复 / 回收站 ----------------

/** 一份备份（`recovery_state`） */
export interface BackupRec {
  id: string;
  at: number;
  /** 原路径。老备份（加清单之前）为空 → 前端显示"原路径未知"并要用户另选目标 */
  original: string;
  backup: string;
  size: number;
  legacy: boolean;
}

/** 一条归档（回收站，`recovery_state`） */
export interface TrashRec {
  at: number;
  original: string;
  trashed: string;
  kind: string;
  count: number;
  present: boolean;
  size: number;
}

/** 恢复区统计（`recovery_state`） */
export interface RecoveryStats {
  backupsCount: number;
  backupsBytes: number;
  trashCount: number;
  trashBytes: number;
  staleRecords: number;
}

/** 恢复区整体状态（`recovery_state`） */
export interface RecoveryState {
  stats: RecoveryStats;
  backups: BackupRec[];
  trash: TrashRec[];
  defaultKeepDays: number;
  defaultKeepItems: number;
}

// ---------------- 其它 ----------------

/** 搜索配置状态（`search_status`） */
export interface SearchStatus {
  provider: string;
  providerLabel: string;
  hasKey: boolean;
  keyPreview: string;
  ready: boolean;
  path: string;
}

/** Agent 模式（`modes_get` 的一行） */
export interface ModeView {
  id: string;
  name: string;
  description: string;
  /** 工具呈现方式：`native` / `ptc`（老后端不返回时按 native 处理） */
  toolPresentation?: "native" | "ptc";
  /** 正文能不能发图。老后端不返回时按 `work` 处理。 */
  imageReplies?: "work" | "free";
  /** 模式注入的场景上下文（可能为空） */
  promptExtra?: string;
  /** 聊天式呈现：一条回复拆成多个气泡、且**不显示过程**。 */
  chatty?: boolean;
}

/** 前台窗口上下文（`foreground_history` 的一行） */
export interface FgCtx {
  app: string;
  title: string;
  dir: string | null;
  file: string | null;
}

/** 上下文获取能力（`context_capability`） */
export interface ContextCapability {
  level: "unavailable" | "appOnly" | "full";
  detail: string;
  canRequest: boolean;
  needsSystemSettings: boolean;
}

/** 一个技能（`skills_list` 的一行） */
export interface SkillInfo {
  name: string;
  description: string;
  path: string;
}

/** 内置工具（`builtin_tools` 的一行） */
export interface BuiltinToolInfo {
  name: string;
  desc: string;
}

/** life 数据源（`life_status` 的一行） */
export interface LifeSourceView {
  id: string;
  label: string;
  enabled: boolean;
  kind: "file" | "command";
  /** 文件路径或命令 */
  target: string;
  itemsPath: string;
  staleHours: number | null;
  complete: boolean;
  ok: boolean;
  error: string;
  updatedAt: string;
  ageHours: number | null;
  stale: boolean;
  count: number;
  preview: string[];
}

/** life 状态（`life_status`） */
export interface LifeStatus {
  path: string;
  sources: LifeSourceView[];
}

/** 用量记录（`usage_report` 的一行）—— 形状与后端 `sessions::UsageRecord` 一致 */
export interface UsageRecord {
  /** 消息时间戳（epoch ms） */
  at: number;
  /** 模型 id（老数据可能为空串） */
  model: string;
  /** 所属会话 id */
  session: string;
  /** 该轮的 token 用量 */
  usage: TokenUsage;
}

// ---------------- pi 内核 ----------------

/**
 * pi 运行时**组件状态**（`pi_status.runtime`，Rust `pi::RuntimeStatus`）。
 *
 * ## 为什么前端要它
 *
 * ① **可观测性**：用户实机问过「这个对得上吗」—— 界面上能直接看到
 *    「现在跑的是 pi 内核还是自研内核」，比让用户翻 `diag.log` 强得多
 *    （普通用户不会去翻日志）。
 * ② **发现静默回落**：pi 没起来时前端会**静默退回自研内核**
 *    （`usePiKernel` 被改成 false），用户却以为在用 pi ⇒ 这是**有问题的情况**，
 *    界面上必须显眼。
 * ③ **缺什么一眼看到**：node ✓ / pi ✓ / python ✗ / git ✓（用系统的）——
 *    不用猜「为什么 pi 起不来」。
 *
 * ⚠️ 字段名是 Rust 端 serde 的 camelCase（`runtime_dir` → `runtimeDir`）。
 */
export interface PiRuntimeStatus {
  /** runtime 目录（存在就填）。 */
  runtimeDir?: string | null;
  /** pi 内核是否**可用**（node + pi bundle 都在）。 */
  ready: boolean;
  /** 内置 node 路径 */
  node?: string | null;
  /** 内置 pi bundle 路径 */
  piBundle?: string | null;
  /** 内置 python 路径（默认包不含 ⇒ 通常是 null） */
  python?: string | null;
  /** git 路径 */
  git?: string | null;
  /** git 来源：`builtin` / `system` / `missing` */
  gitSource: string;
  /** bash 路径 */
  bash?: string | null;
  /** bash 来源：`builtin` / `system` / `missing` */
  bashSource: string;
  /** 人话提示（后端写好的，直接可展示给用户）。 */
  notes?: string[];
}

/** pi 内核状态（`pi_status`） */
export interface PiStatus {
  /** pi 内核是否可用（能起 runtime）。 */
  available: boolean;
  runtimeDir?: string;
  agentDir?: string;
  bridgePort?: number;
  /** 正在跑的会话（多会话并行时可能有多个）。 */
  liveSessions?: string[];
  /** 同时最多能跑几个会话。 */
  maxLive?: number;
  /** 当前有 pi 运行时的那条会话。 */
  currentSession?: string | null;
  /**
   * 运行时组件状态（node / pi / python / git / bash）。
   *
   * ⚠️ **可能缺**：老后端没有这个字段 ⇒ 读不到就是 `undefined`
   *    ⇒ 前端按「未知」处理，**不误报**。
   */
  runtime?: PiRuntimeStatus;
}
