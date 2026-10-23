/**
 * 前端的数据接口（`DataSource`）—— **前端不直接摸 Tauri**。
 *
 * ## 为什么
 *
 * 业务逻辑里散着 `invoke("session_state")` 这类调用时：
 * - 换个后端 / 换个传输（HTTP、Worker、纯内存）要改一屏屏的调用点；
 * - 想在浏览器里渲染一屏给人看（出视频、做设计稿核对）**根本不可能** ——
 *   没有 Tauri runtime 就没有 `invoke`；
 * - 想测「空态 / 错误态 / 加载中」只能靠 mock 整个 `@tauri-apps/api`，
 *   而那个 mock 跟业务逻辑是**两套东西**，改一个字段要改两处。
 *
 * 所以把「取数据」这件事收进一个接口，两套实现：
 * - `TauriSource`（`./tauri.ts`）—— 真后端，走 Tauri `invoke`；
 * - `MockSource`（`./mock.ts`）—— 内存假数据，**零 Tauri 依赖**。
 *
 * 切换在 `src/data/index.ts` 里一行完成。
 *
 * ## 契约怎么读
 *
 * 每个方法名 = 一个后端命令；参数对象 = 该命令的入参（**键名与后端 Rust 同义**，
 * Tauri 自动做 snake_case ⇄ camelCase 转换）。
 * 前端要提供什么、后端要返回什么，就是这一份文件 + `types.ts`。
 *
 * ⚠️ 新增后端命令时：**先在这里加方法**（两个实现都要实现），
 * 再在业务里用 —— 这样「有没有 mock」永远是编译期就能问出来的事。
 */
import type {
  AgentRun,
  BundleInfo,
  BuiltinToolInfo,
  Candidate,
  CmdAuditRecord,
  CmdGrantsView,
  CmdPolicy,
  CmdVerdictView,
  ContextCapability,
  FgCtx,
  LifeStatus,
  McpServerCfg,
  McpStatus,
  MemStats,
  MemoryFiles,
  ModelEditView,
  ModelView,
  ModeView,
  PendingPerm,
  PermRule,
  ProjectListView,
  ProjectMemoryView,
  ProjectRow,
  RecoveryState,
  RemoteModelInfo,
  SearchStatus,
  Session,
  SessionState,
  SkillInfo,
  SteerAck,
  UsageRecord,
} from "./types";

/** 启动自检（`boot_state`） */
export interface BootState {
  needsInit: boolean;
  hasModels: boolean;
  dataDir: string;
  dataDirExists?: boolean;
}

/** 模式表（`modes_get`） */
export interface ModesView {
  activeMode: string;
  defaultMode?: string;
  modes: ModeView[];
}

/** 设置项（`get_settings`）—— 后端给的是「部分字段」型 */
export interface SettingsView {
  blurCollapse?: boolean;
  projectDirAutoread?: boolean;
  builtinToolsEnabled?: boolean;
  /**
   * 命令在**哪个 shell** 里跑（`pwsh` / `gitbash`）。
   *
   * ⚠️ 这是**新字段**（2026-10-23 加）—— 老 settings.json 里**没有**它。
   * ⇒ 后端读不到时必须回落 `"pwsh"`（老用户升级后行为**不变**）。
   * ⇒ 前端也按同一口径兜底，别让"字段缺失"变成"行为变了"。
   */
  execShell?: ExecShell;
  [k: string]: unknown;
}

/**
 * 执行用的 shell（`pwsh` = PowerShell，`gitbash` = Git Bash）。
 *
 * ⚠️ **不要跟 `execTrust` 混**：那个管"要不要问"（ask/smart/full），
 * 这个管"用哪个 shell 跑"（pwsh/gitbash）。两个维度互不干涉。
 */
export type ExecShell = "pwsh" | "gitbash";

/** 执行位置的三个选项（`bgDeskEnabled` 那一维由前端单独管，不进这里）。 */
export interface ExecShellOption {
  id: ExecShell;
  label: string;
  desc: string;
}

/**
 * 后端查回来的「当前 shell + Git Bash 是否可用」。
 *
 * ⚠️ `gitBashAvailable: false` = 这台机器**没装 Git for Windows**
 * （发布包不带 git —— runtime 只有 node+pi）⇒ 选了「本机 Git Bash」会直接失败。
 * ⇒ 设置页要**提前**提示，不能等用户选完才报错。
 */
export interface ExecShellStatus {
  execShell: ExecShell;
  label: string;
  gitBashAvailable: boolean;
  gitBashPath: string;
}

/** 后台执行状态（`bg_desk_status`） */
export interface BgDeskStatus {
  enabled: boolean;
  available: boolean;
  error: string | null;
  windows: { title: string; width: number; height: number; pid: number }[];
}

/** pi 内核状态（`pi_status`） */
export interface PiStatus {
  available: boolean;
  runtimeDir?: string;
  agentDir?: string;
  bridgePort?: number;
}

/** 压缩结果（`session_compact`） */
export interface CompactOutcome {
  [k: string]: unknown;
}

/** 清理结果（后端 `recovery::PruneOutcome`，有 rename_all ⇒ camelCase） */
export interface PruneOutcome {
  backupsRemoved: number;
  trashRemoved: number;
  bytesFreed: number;
}

/** 批量导入远端模型的结果（后端 `ImportModelsResult`） */
export interface ImportModelsResult {
  /** 成功导入的模型 id */
  added: string[];
  /** 跳过的模型 id（已存在等） */
  skipped: string[];
}

/** 首次初始化的引导报告（`bootstrap_data`） */
export interface BootstrapReport {
  /** 数据目录绝对路径 */
  dataDir: string;
  /** 本次新建的目录（相对 dataDir） */
  createdDirs: string[];
  /** 本次新建的文件（相对 dataDir） */
  createdFiles: string[];
  /** 本来就有、跳过的路径（相对 dataDir） */
  skipped: string[];
  /** 是否已经存在过 models.json 且有内容 —— 前端据此决定要不要提示"去加模型" */
  hasModels: boolean;
  /** 本次二进制里编进来的骨架指纹 */
  seed: string;
}

// ---------------- 领域接口 ----------------

/** 会话：列、建、切、分叉、截断、删、改名、压缩 */
export interface SessionsApi {
  /** 当前会话 + 会话列表 */
  state(): Promise<SessionState>;
  /**
   * 新建会话（`mode` 为空 = 用默认模式）。
   *
   * ⚠️ 叫 `create` 不叫 `new` —— 类型字面量里的 `new(...)` 会被 TypeScript
   * 当成**构造签名**，不是方法名。
   */
  create(args: { mode?: string | null }): Promise<Session>;
  /** 切到某条会话 */
  switch(args: { id: string }): Promise<Session>;
  /** 从第 upto 条处分叉一条新会话 */
  fork(args: { upto: number }): Promise<Session>;
  /** 截断到第 upto 条 */
  truncate(args: { upto: number }): Promise<Session>;
  /** 删一条会话 */
  delete(args: { id: string }): Promise<void>;
  /** 改会话名 */
  setTitle(args: { id: string; title: string }): Promise<void>;
  /** 手动压缩上下文 */
  compact(args?: { force?: boolean | null }): Promise<CompactOutcome>;
}

/** 对话：发一轮、插话、停止 */
export interface ChatApi {
  /** 自研内核发一轮 */
  send(args: { input: string; images?: string[] | null }): Promise<AgentRun>;
  /** pi 内核发一轮 */
  sendPi(args: { input: string; images?: string[] | null }): Promise<AgentRun>;
  /** 停止当前轮 */
  cancel(args?: { sessionId?: string | null }): Promise<void>;
  /** 自研内核插话 */
  steer(args: { input: string; images?: string[] | null; sessionId?: string | null }): Promise<SteerAck>;
  /**
   * pi 内核插话（参数名是 `message`，不是 `input` —— 两边刻意对齐过）。
   *
   * ⚠️ `images` 是 orbcat 的 **data URL 字符串**（前端 `pendingImages`），
   * 转成 pi 形状的活儿在 Rust 侧 `pi::to_pi_images` 里做（2026-10-23 fix002 起）。
   */
  steerPi(args: {
    message: string;
    images?: string[] | null;
    sessionId?: string | null;
  }): Promise<SteerAck>;
}

/** 模型：列表、选择、增删改、远端导入、连通测试 */
export interface ModelsApi {
  list(): Promise<ModelView[]>;
  selected(): Promise<{ id: string; url?: string } | null>;
  select(args: { id: string; url?: string | null }): Promise<void>;
  groupRename(args: { url: string; name: string }): Promise<void>;
  /** 组显示名表（键 = Base URL，值 = 显示名） */
  groupNames(): Promise<Record<string, string>>;
  /** 整组换 Base URL（会把该组所有模型搬过去） */
  changeGroupUrl(args: { url: string; newUrl: string }): Promise<{ moved: number; skipped: string[]; newUrl: string }>;
  add(args: {
    id: string;
    url: string;
    apiKey?: string | null;
    supportsToolCall: boolean;
    supportsImages: boolean;
    headers?: string | null;
    maxInputTokens?: number | null;
    maxOutputTokens?: number | null;
    overwrite?: boolean | null;
  }): Promise<string>;
  edit(args: {
    id: string | null;
    newId?: string | null;
    url?: string | null;
    newUrl?: string | null;
    apiKey?: string | null;
    clearKey?: boolean | null;
    supportsToolCall?: boolean | null;
    supportsImages?: boolean | null;
    headers?: string | null;
    maxInputTokens?: number | null;
    maxOutputTokens?: number | null;
  }): Promise<void>;
  remove(args: { id: string; url?: string | null }): Promise<void>;
  removeGroup(args: { url: string }): Promise<number>;
  setGroupKey(args: { url: string; apiKey: string }): Promise<number>;
  getEdit(args: { id: string; url?: string | null }): Promise<ModelEditView>;
  fetchRemote(args: { url: string; apiKey: string; headers?: string | null }): Promise<RemoteModelInfo[]>;
  parseSpec(args: { payload: string }): Promise<RemoteModelInfo[]>;
  importRemote(args: {
    ids: string[];
    sourceId?: string | null;
    sourceUrl?: string | null;
    url?: string | null;
    apiKey?: string | null;
    headers?: string | null;
    supportsToolCall: boolean;
    supportsImages: boolean;
    contextLengths?: Record<string, number> | null;
    maxOutputTokens?: Record<string, number> | null;
    toolFlags?: Record<string, boolean> | null;
    imageFlags?: Record<string, boolean> | null;
  }): Promise<ImportModelsResult>;
  test(args: { id: string; url?: string | null }): Promise<string>;
  windowOpen(): Promise<void>;
  windowClose(): Promise<void>;
}

/** 记忆：待批候选、批准/驳回/改写、文件、项目记忆 */
export interface MemoryApi {
  pending(): Promise<Candidate[]>;
  files(): Promise<MemoryFiles>;
  approve(args: { id: string }): Promise<void>;
  approveAll(): Promise<number>;
  reject(args: { id: string; reason?: string | null }): Promise<void>;
  rewrite(args: { id: string; content: string }): Promise<void>;
  stats(): Promise<MemStats>;
  projects(): Promise<ProjectMemoryView[]>;
  projectSetSource(args: { name: string; path: string }): Promise<void>;
  projectArchive(args: { name: string }): Promise<string>;
  propose(args: { content: string }): Promise<Candidate>;
  read(): Promise<string>;
}

/** 权限：待批申请、文件规则、命令策略与授权、审计 */
export interface PermissionsApi {
  requests(): Promise<PendingPerm[]>;
  decide(args: { id: string; decision: string; reason?: string | null }): Promise<void>;
  rules(): Promise<PermRule[]>;
  addRule(args: { prefix: string; access: string; label: string }): Promise<void>;
  removeRule(args: { prefix: string }): Promise<void>;
  check(args: { path: string; need: string }): Promise<string>;
  cmdPolicy(): Promise<CmdPolicy>;
  /**
   * 保存命令策略。
   *
   * ⚠️ 入参用 **camelCase**（`hardBlock`）：Tauri 会把命令入参的 camelCase
   * 自动转成后端的 snake_case（`hard_block`）。返回值的字段名则**不做转换**
   * —— 见 `CmdPolicy` 的注释。
   */
  cmdPolicySet(args: { allow: string[]; hardBlock: string[] }): Promise<CmdPolicy>;
  cmdPolicyPath(): Promise<string>;
  cmdGrants(): Promise<CmdGrantsView[]>;
  cmdGrantsClear(): Promise<number>;
  cmdGrantRevoke(args: { fingerprint: string }): Promise<number>;
  cmdAuditTail(args?: { limit?: number | null }): Promise<CmdAuditRecord[]>;
  cmdTest(args: { command: string }): Promise<CmdVerdictView>;
  grantsList(): Promise<unknown[]>;
  grantRevoke(args: { prefix: string; tier?: string | null }): Promise<number>;
  revokeLabel(args: { label: string }): Promise<number>;
  setRules(args: { rules: PermRule[] }): Promise<void>;
  rulesPath(): Promise<string>;
}

/** 项目：pmem 项目索引 + 项目文件 */
export interface ProjectsApi {
  pmemList(): Promise<ProjectRow[]>;
  pmemCreate(args: { name: string; rootPath?: string | null }): Promise<ProjectRow>;
  pmemRename(args: { id: number; name: string }): Promise<void>;
  pmemDelete(args: { id: number }): Promise<void>;
  pmemRead(args: { name: string }): Promise<string>;
  pmemWrite(args: { name: string; text: string }): Promise<string>;
  pmemClear(args: { name: string }): Promise<void>;
  pmemGetActive(): Promise<ProjectRow | null>;
  pmemSetActive(args: { id: number | null }): Promise<void>;
  pmemBindSession(args: { sessionId: string; projectId: number | null }): Promise<void>;
  pmemRemember(args: { content: string }): Promise<string>;
  /** 会话 id → 项目名（没有项目是 null）—— 后端是 `HashMap<String, Option<String>>` */
  pmemSessionMap(): Promise<Record<string, string | null>>;
  list(): Promise<ProjectListView>;
  activate(args: { id: string | null }): Promise<void>;
  save(args: { id: string; body: string }): Promise<void>;
  read(args: { id: string }): Promise<string>;
  delete(args: { id: string }): Promise<void>;
}

/** 恢复区：备份、回收站、清理 */
export interface RecoveryApi {
  state(): Promise<RecoveryState>;
  restoreBackup(args: { id: string; target?: string | null }): Promise<string>;
  deleteBackup(args: { id: string }): Promise<string>;
  restoreTrash(args: { trashed: string; target?: string | null }): Promise<string>;
  deleteTrash(args: { trashed: string }): Promise<string>;
  clearTrash(): Promise<string>;
  diagnostic(): Promise<string>;
  prune(args?: { keepDays?: number | null; keepItems?: number | null }): Promise<PruneOutcome>;
}

/** 设置：开关、信任、窗口、后台执行、模式表 */
export interface SettingsApi {
  get(): Promise<SettingsView>;
  setExecTrust(args: { mode: string }): Promise<string>;
  /**
   * 切换命令用的 shell（`pwsh` / `gitbash`）。
   *
   * ⚠️ **后端那条正在另开分支实现**（`work/backend/exec-shell`）。
   *    本轮前端先把契约定死：入参 `{ shell: string }`、返回**落盘后的那个值**。
   *    后端还没上之前，真后端调用会失败 ⇒ 前端必须有兜底（见 main.ts 的落盘逻辑）。
   */
  setExecShell(args: { shell: string }): Promise<string>;
  /**
   * 查当前 shell + **Git Bash 是否可用**。
   *
   * ⚠️ 发布包**不带 git**（runtime 只有 node+pi）⇒ 没装 Git for Windows 的机器上
   *    选「本机 Git Bash」会直接失败 ⇒ 设置页要提前提示。
   */
  execShellStatus(): Promise<ExecShellStatus>;
  setBuiltinTools(args: { enable: boolean }): Promise<void>;
  setBlurCollapse(args: { enable: boolean }): Promise<void>;
  setProjectDirAutoread(args: { enable: boolean }): Promise<void>;
  setWindowMode(args: { mode: string }): Promise<void>;
  autostartStatus(): Promise<boolean>;
  autostartSet(args: { enable: boolean }): Promise<void>;
  bgDeskStatus(): Promise<BgDeskStatus>;
  bgDeskSet(args: { enabled: boolean }): Promise<boolean>;
  modesGet(args?: { sessionId?: string | null }): Promise<ModesView>;
}

/** life：外部数据源摘要 */
export interface LifeApi {
  status(): Promise<LifeStatus>;
  setEnabled(args: { id: string; enabled: boolean }): Promise<void>;
  refresh(): Promise<void>;
  configPath(): Promise<void>;
}

/** MCP：网关、server 列表、组开关、授权 */
export interface McpApi {
  status(): Promise<McpStatus>;
  serversList(): Promise<McpServerCfg[]>;
  serversSave(args: { servers: McpServerCfg[] }): Promise<void>;
  setAllGroups(args: { all: boolean }): Promise<void>;
  setGroupEnabled(args: { name: string; enabled: boolean }): Promise<void>;
  groupSettings(): Promise<Record<string, boolean>>;
  grantsList(): Promise<string[]>;
  grantsClear(): Promise<number>;
  grantRevoke(args: { tool: string }): Promise<number>;
  refresh(): Promise<void>;
}

/** 搜索：配置与连通测试 */
export interface SearchApi {
  status(): Promise<SearchStatus | null>;
  test(): Promise<string>;
  set(args: { provider: string; apiKey?: string | null }): Promise<SearchStatus>;
}

/** 清理结果（后端 `recovery::PruneOutcome`，有 rename_all ⇒ camelCase） */
export interface PruneOutcome {
  backupsRemoved: number;
  trashRemoved: number;
  bytesFreed: number;
}

/** 批量导入远端模型的结果（后端 `ImportModelsResult`） */
export interface ImportModelsResult {
  /** 成功导入的模型 id */
  added: string[];
  /** 跳过的模型 id（已存在等） */
  skipped: string[];
}

/** 系统杂项：自检、上下文、窗口、截图、技能、用量 */
export interface SystemApi {
  bootState(): Promise<BootState>;
  /** 首次初始化的引导报告（`bootstrap_data`） */
  bootstrapData(): Promise<BootstrapReport>;
  diagLog(args: { msg: string }): Promise<void>;
  piStatus(): Promise<PiStatus>;
  contextCapability(): Promise<ContextCapability>;
  requestContextPermission(): Promise<ContextCapability>;
  foregroundHistory(): Promise<FgCtx[]>;
  foregroundIsCaptureTool(): Promise<boolean>;
  orbUiState(): Promise<{ sleeping: boolean }>;
  orbUiAction(args: { action: string }): Promise<void>;
  takeOpenPanelRequest(): Promise<boolean>;
  confirmPanelOpened(): Promise<void>;
  imageThumb(args: { path: string; maxEdge?: number | null }): Promise<string>;
  captureScreen(): Promise<string>;
  exePath(): Promise<string>;
  appQuit(): Promise<void>;
  usageReport(): Promise<UsageRecord[]>;
  skillsList(): Promise<SkillInfo[]>;
  builtinTools(): Promise<BuiltinToolInfo[]>;
  openAccessibilitySettings(): Promise<void>;
  distillSessionOpen(): Promise<Session>;
  bundles(): Promise<BundleInfo[]>;
}

// ---------------- 数据源本体 ----------------

/**
 * 数据源。
 *
 * 两个实现必须**行为等价**：真后端能给的，mock 也要能给出对应形状；
 * mock 多出来的（延迟、错误注入）是为了渲染状态用，不改变契约。
 */
export interface DataSource {
  /** 是哪种实现（UI 上标「mock 模式」用） */
  readonly kind: "tauri" | "mock";
  /**
   * 通用命令调用 —— **逃生舱**。
   *
   * 正规做法是走上面的领域方法；这里只给「还没归类的命令」和
   * 「调试时临时探一下」用。新代码别用。
   */
  call<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T>;

  readonly sessions: SessionsApi;
  readonly chat: ChatApi;
  readonly models: ModelsApi;
  readonly memory: MemoryApi;
  readonly permissions: PermissionsApi;
  readonly projects: ProjectsApi;
  readonly recovery: RecoveryApi;
  readonly settings: SettingsApi;
  readonly life: LifeApi;
  readonly mcp: McpApi;
  readonly search: SearchApi;
  readonly system: SystemApi;
}
