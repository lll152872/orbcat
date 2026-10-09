/**
 * Tauri API mock —— UI 测试的「假后端」
 *
 * ⚠️ 必须挂在 globalThis 上：`vi.resetModules()` 会让 main.ts 与测试文件
 * 各自拿到一份新的模块实例；若 state/calls 是模块级变量，会出现
 * 「面板能点开（main 用了 B 实例 invoke），但 callsOf/state 是空（测试读 A）」。
 */

export interface MockStoredMsg {
  role: "user" | "assistant";
  text: string;
  images?: string[];
  steps?: { kind: string; name: string | null; detail: string }[];
  at: number;
  model?: string;
}

export interface MockInvokeCall {
  cmd: string;
  args: Record<string, unknown> | undefined;
}

type MockState = {
  chatReply: string;
  chatError: string | null;
  chatDelayMs: number;
  selectedModel: string | null;
  /**
   * 设置里记的 Base URL（身份键 = (Base URL, 接口 id) 的另一半）。
   *
   * `undefined` = 没这个字段（默认，按 `models[0].url` 返回，模拟"选过但没记 url"以外的情况）；
   * **`null` = 显式"设置里只有 id、没有 url"** —— 这正是 2026-10-02 用户报「两个 glm-5.3-flash
   * 都画线」的现场：旧版只存 id，身份键重构后没回填，前端判定式就把同 id 的所有行都标成当前。
   */
  selectedModelUrl?: string | null;
  models: {
    id: string;
    name: string;
    vendor: string;
    url: string;
    supportsToolCall: boolean;
    supportsImages: boolean;
  }[];
  messages: MockStoredMsg[];
  sessionList: {
    id: string;
    title: string;
    kind: string;
    updatedAt: number;
    /** 这条会话的模式 id（2026-10-08 起模式跟会话走，列表每行都要带） */
    mode?: string;
  }[];
  currentSessionId: string;
  memPending: { id: string; content: string; source: string; created_at: string }[];
  permPending: unknown[];
  /**
   * **当前会话**的 agent 模式（`modes_get({sessionId})` 读它）。
   *
   * 2026-10-08 前叫 `activeMode`（那时模式是全局设置）。改名的原因就是这次重构：
   * 模式跟会话走了，`modes_get` 的 `activeMode` 现在必须按**会话**算 ——
   * 测试里要"把当前会话切成闲聊"就改这个字段。
   */
  sessionMode: string;
  /**
   * **新建会话的默认模式**（`settings.activeMode`，`modes_get.defaultMode`）。
   *
   * 与 `sessionMode` 分开是有意的：它俩是这次重构最容易混的一对 ——
   * 「默认」只影响新建，改它**不该**动当前会话。
   */
  defaultMode: string;
  /** 切换模式时后端**拒绝**的 id（测"失败要报出来，不能静默留在旧模式"） */
  modeSetError: string | null;
  /** 后台执行（隐形桌面）是否开着 */
  bgDeskEnabled: boolean;
  /** 开后台失败时的报错（测"建不出桌面要立刻报错"） */
  bgDeskSetError: string | null;
  bootNeedsInit: boolean;
  bootHasModels: boolean;
  windowModes: string[];
  /**
   * 逐命令覆盖返回值（2026-09-30 加）。
   *
   * ## 为什么必须走 `MockState` 而不是直接改 `emptyOk`
   *
   * `helpers.bootApp()` 会 `vi.resetModules()` 再动态 `import("../../src/main")`
   * —— 前端拿到的是一份**新的** mock 模块实例。测试文件若是**静态** import
   * `setMock`，改的是**另一个实例**的表，前端根本看不到（这个坑在写
   * `flow.recovery.test.ts` 时真踩到了：改了数据、视图还是空态）。
   *
   * 放进 `MockState` 就没这个问题：state 存在 `globalThis` 单例上
   * （见下面的 `g.__orbcatMock`），两边共享同一份；而且 `resetMock()`
   * 每个用例都会重置它，**不会**把上一个用例的覆盖带进下一个。
   */
  invokeOverrides: Record<string, unknown>;
  /**
   * 数据源页的返回（`life_status`）。
   *
   * 默认空表（没配源是正常状态）；测试塞一个"已接学习通"的形态来验
   * 条数 / 过期标黄 / 读取失败三种状态 —— 它们只在**有源**时才出现。
   */
  lifeStatus: {
    path: string;
    sources: {
      id: string;
      label: string;
      enabled: boolean;
      kind: "file" | "command";
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
    }[];
  };
};

type MockBag = {
  state: MockState;
  calls: MockInvokeCall[];
  /** `listen` 注册的处理器 —— 测试用 `emitEvent` 模拟后端推事件 */
  listeners: { event: string; handler: (e: { payload: unknown }) => void }[];
};

function defaultState(): MockState {
  return {
    chatReply: "这是 mock 回复 MOCK_OK。",
    chatError: null,
    chatDelayMs: 0,
    // 身份 = (Base URL, 接口 id)（2026-10-02）：selectedModel 存**接口 id**，
    // get_selected_model 返回 {id, url} 双字段；显示名已废弃
    selectedModel: "mock-1",
    models: [
      {
        id: "mock-1",
        vendor: "Test",
        url: "http://127.0.0.1:9/v1",
        supportsToolCall: true,
        supportsImages: true,
      },
    ],
    messages: [],
    sessionList: [],
    currentSessionId: "sess-main",
    memPending: [],
    permPending: [],
    sessionMode: "standard",
    defaultMode: "standard",
    modeSetError: null,
    bgDeskEnabled: false,
    bgDeskSetError: null,
    bootNeedsInit: false,
    bootHasModels: true,
    windowModes: [],
    invokeOverrides: {},
    lifeStatus: {
      path: "C:\\mock\\agent-data\\life.json",
      sources: [],
    },
  };
}

const g = globalThis as typeof globalThis & { __orbcatMock?: MockBag };
if (!g.__orbcatMock) {
  g.__orbcatMock = { state: defaultState(), calls: [], listeners: [] };
}

export const bag = g.__orbcatMock;
export const state = bag.state;
export const calls = bag.calls;

export function resetMock(overrides: Partial<MockState> = {}): void {
  bag.calls.length = 0;
  bag.listeners.length = 0;
  Object.assign(bag.state, defaultState(), overrides);
}

/** 一条会话的返回体。`mode` 缺省 = 当前会话的模式（真实后端会话文件里必有）。 */
function sessionPayload(mode?: string) {
  return {
    id: state.currentSessionId,
    title: "主聊天",
    kind: "main",
    createdAt: Date.now() - 1000,
    updatedAt: Date.now(),
    messages: state.messages.map((m) => ({ ...m })),
    mode: mode ?? state.sessionMode,
  };
}

function sessionStatePayload() {
  return {
    session: sessionPayload(),
    list:
      state.sessionList.length > 0
        ? state.sessionList.map((s) => ({ mode: state.sessionMode, ...s }))
        : [
            {
              id: state.currentSessionId,
              title: "主聊天",
              kind: "main",
              updatedAt: Date.now(),
              mode: state.sessionMode,
            },
          ],
  };
}

const emptyOk: Record<string, unknown> = {  set_window_mode: null,
  set_selected_model: null,
  set_blur_collapse: null,
  autostart_set: null,
  models_add: null,
  models_edit: null,
  models_remove: null,
  models_get_edit: {
    id: "mock-1",
    name: "Mock One",
    vendor: "Custom",
    url: "http://127.0.0.1:8787/v1",
    supportsToolCall: true,
    supportsImages: true,
    headersText: "",
    hasKey: true,
    keyPreview: "mock-...key",
  },
  models_fetch_remote: [
    { id: "remote-a", ownedBy: "mock" },
    { id: "remote-b", ownedBy: "mock" },
  ],
  models_fetch_remote_using: [
    { id: "remote-a", ownedBy: "mock" },
    { id: "remote-b", ownedBy: "mock" },
  ],
  models_import_remote: { added: ["remote-a"], skipped: [] },
  mem_approve: null,
  mem_reject: null,
  mem_propose: null,
  // 记忆蒸馏：现在就是一条普通会话（后端按 sessions/<id>.distill 标记换角色 + 注入 distill_move）
  distill_session_open: null,
  mem_project_set_source: null,
  mem_project_archive: "C:\\mock\\agent-data\\.trash\\projects-x-00000000",
  chat_cancel: null,
  app_quit: null,
  mcp_refresh: null,
  mcp_set_url: null,
  mcp_load_group: "ok",
  mcp_unload_group: "ok",
  mcp_set_group_enabled: null,
  mcp_set_all_groups: null,
  mcp_group_settings: { __all__: true },
  // 组名映射：默认空 = 组头回落到 host（与真实后端一致）
  model_group_names: {},
  model_group_rename: null,
  models_set_group_key: 1,
  models_remove_group: 1,
  // 独立「模型管理」窗口（2026-09-29 定稿）：编辑弹窗外、选择留面板
  models_window_open: null,
  models_window_close: null,
  perm_add_rule: null,
  perm_remove_rule: null,
  perm_request_decide: null,
  perm_cmd_policy_set: null,
  image_thumb: "",
  capture_screen: "data:image/jpeg;base64,xxxx",
  exe_path: "C:\\mock\\orbcat.exe",
  exe_cmd: "",
  autostart_status: false,
  foreground_context: null,
  foreground_history: [],
  skills_list: [],
  // 拼装项目（project.rs）：默认给"没有项目"的空态，
  // 专测项目页的用例自己覆盖这个返回值。
  project_list: {
    bundles: [],
    active: null,
    unresolved: [],
    permError: null,
    tools: [],
  },
  project_activate: null,
  project_save: null,
  project_delete: null,
  project_read: "",
  mem_files: {
    rules: "# RULES",
    soul: "# SOUL",
    identity: "# IDENTITY",
    user: "# USER",
    memory: "# MEMORY",
  },
  mem_projects: [],
  mem_read: "# MEMORY",
  perm_rules: [],
  perm_rules_path: "C:\\mock\\permissions.json",
  perm_requests_list: [],
  perm_cmd_policy: { allow: [], hard_block: [] },
  perm_cmd_policy_path: "C:\\mock\\command_policy.json",
  perm_cmd_grants_list: [],
  perm_cmd_audit_tail: [],
  usage_report: [],
  // 备份与回收站（2026-09-30）：默认给空态，专门测这个视图的用例自己覆盖
  recovery_state: {
    stats: { backupsCount: 0, backupsBytes: 0, trashCount: 0, trashBytes: 0, staleRecords: 0 },
    backups: [],
    trash: [],
    defaultKeepDays: 90,
    defaultKeepItems: 500,
  },
  recovery_restore_backup: "已还原（mock）",
  recovery_restore_trash: "已还原（mock）",
  recovery_prune: { backupsRemoved: 0, trashRemoved: 0, bytesFreed: 0 },
  // 「又启动了一次 exe」的兜底标记：mock 下默认没有待办
  take_open_panel_request: false,
  confirm_panel_opened: null,
  recovery_diagnostic: "C:\\\\mock\\\\agent-data\\\\diagnostics\\\\diag-mock.txt",
  mcp_status: null,
  get_settings: { selectedModel: "mock-1", lastSession: null, blurCollapse: false },
  search_status: {
    provider: "",
    providerLabel: "未配置",
    hasKey: false,
    keyPreview: "",
    ready: false,
    path: "C:\\mock\\search.json",
  },
  search_set: {
    provider: "tavily",
    providerLabel: "Tavily",
    hasKey: true,
    keyPreview: "tvly-1...abcd",
    ready: true,
    path: "C:\\mock\\search.json",
  },
  search_test: "搜索连通正常 · Tavily · 约 0 条结果行",
  // 数据源：默认**空表**（没配任何源是完全正常的状态，UI 要能画"还没有"）
  life_status: {
    path: "C:\\mock\\agent-data\\life.json",
    sources: [],
  },
  life_set_enabled: null,
  life_refresh: null,
  life_config_path: "C:\\mock\\agent-data\\life.json",
  test_model: "连通性 OK（mock）",
  perm_check: "read",
  perm_cmd_test: { verdict: "allow", risk: "low", normalized: "", reason: "mock" },
  session_new: null,
  session_switch: null,
  session_fork: null,
  session_delete: null,
  session_truncate: null,
  chat_steer: { id: "steer-1", queued: 1 },
  bootstrap_data: { ok: true },
  pmem_list: [],
  pmem_get_active: null,
  pmem_session_map: {},
};

/**
 * 测试用：改一条命令的返回值。等价于 `resetMock({ invokeOverrides: { [cmd]: v } })`，
 * 但可以连着调多次。
 *
 * ⚠️ 必须在 `resetMock()` **之后**调用（`beforeEach` 里的 `resetMock` 会清掉覆盖）。
 * 别在测试文件顶层静态 import 后再指望它影响前端 —— 原因见
 * `MockState.invokeOverrides` 的注释（模块双实例坑）。
 */
export function setMock(cmd: string, value: unknown): void {
  state.invokeOverrides[cmd] = value;
}

export async function invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  bag.calls.push({ cmd, args });

  // 逐命令覆盖优先（见 `MockState.invokeOverrides` 的说明）
  if (cmd in state.invokeOverrides) {
    return state.invokeOverrides[cmd] as T;
  }

  switch (cmd) {
    case "set_window_mode": {
      state.windowModes.push(String(args?.mode ?? ""));
      return null as T;
    }
    case "boot_state":
      return {
        needsInit: state.bootNeedsInit,
        hasModels: state.bootHasModels,
        dataDir: "C:\\mock\\agent-data",
      } as T;
    case "list_models":
      return state.models as T;
    case "get_selected_model":
      return {
        id: state.selectedModel,
        // undefined → 走老默认；null → 「设置里只有 id」（复现同 id 双标现场）
        url:
          state.selectedModelUrl !== undefined
            ? state.selectedModelUrl
            : (state.models[0]?.url ?? null),
      } as T;
    case "session_state":
      return sessionStatePayload() as T;
    case "session_new": {
      // 模式**建的时候**定（2026-10-08）：传了就用传的，没传落默认模式。
      // 与真实后端同一套兜底（`session_new` → `settings.activeMode`）。
      const m = typeof args?.mode === "string" && args.mode ? args.mode : state.defaultMode;
      state.sessionMode = m;
      return sessionPayload(m) as T;
    }
    case "session_switch":
    case "session_fork":
    case "session_truncate":
      return sessionPayload() as T;
    case "mem_pending":
      return state.memPending as T;
    case "perm_requests_list":
      return state.permPending as T;
    // 数据源：默认返回 state.lifeStatus（测试可以塞一个"已接学习通"的形态）。
    // 为什么不留成静态空表：这张页要测的三件事（条数、过期标黄、读取失败）
    // 全都只在**有源**时才出现，静态空表只能测出"还没有数据源"。
    case "life_status":
      return state.lifeStatus as T;
    // agent 模式：返回**与真实后端同形**的四模式结构。
    // 为什么不塞进 emptyOk：模式菜单要按 id 找当前项、要标灰 unknownGroups，
    // 一份静态假数据测不出"切换后 chip 变了 / 未知组标灰"这两件事。
    case "modes_get": {
      // `activeMode` 按**会话**算（2026-10-08）：传了 sessionId 就查那条会话的模式，
      // 没传（启动早期）退回 `defaultMode`。真实后端走 `sessions::effective_mode`，
      // 同一套优先级 —— mock 跟它不同步的话，"切会话后 UI 还显示旧模式"这种
      // 真 bug 会被 mock 悄悄抹平。
      const sidRaw = args?.sessionId;
      const sid = typeof sidRaw === "string" && sidRaw ? sidRaw : null;
      const fromList = sid ? state.sessionList.find((s) => s.id === sid)?.mode : undefined;
      const activeMode =
        sid === null
          ? state.defaultMode
          : sid === state.currentSessionId
            ? state.sessionMode
            : (fromList ?? state.defaultMode);
      return {
        activeMode,
        defaultMode: state.defaultMode,
        modes: [
          {
            id: "standard",
            name: "标准模式",
            description: "处理代码、文件和资料，适合大多数任务。",
            mcpGroupsPreload: "all",
            allowedCount: null,
            unknownGroups: [],
            toolPresentation: "native",
          },
          {
            id: "ptc",
            name: "PTC 模式",
            description: "把工具调用写成程序，批量跑并只把结论交回来。",
            mcpGroupsPreload: "all",
            allowedCount: null,
            unknownGroups: [],
            // 与标准模式的**真正差别**：工具坍缩成 run_code + 程序内 SDK
            toolPresentation: "ptc",
          },
          {
            // 2026-10-08：「极简」被「闲聊」取代（真实后端同步改名）。
            // `chatty` 是闲聊的核心 —— 前端据此走 `bubble.ts` 的多气泡切分、
            // 并且**不显示「过程」**。
            id: "chat",
            name: "闲聊模式",
            description: "轻松聊天，可以发表情包。",
            mcpGroupsPreload: [],
            allowedCount: 0,
            unknownGroups: [],
            toolPresentation: "native",
            imageReplies: "free",
            chatty: true,
          },
          {
            id: "creator",
            name: "创造模式",
            description: "改自己 / 造新东西。",
            // 故意放一个不存在的组：UI 必须把它标灰（用户写错组名的唯一提示）
            mcpGroupsPreload: ["github", "打错的组名"],
            allowedCount: 1,
            unknownGroups: ["打错的组名"],
            toolPresentation: "native",
          },
        ],
        groups: [
          { name: "github", state: "allowed" },
          { name: "ssh", state: "denied" },
        ],
        unknownGroups: [],
        mcpConnected: true,
      } as T;
    }
    case "modes_set": {
      if (state.modeSetError) {
        throw state.modeSetError;
      }
      // 2026-10-08：这条命令现在只改**新建会话的默认模式**，不动当前会话
      // （用户定案"一个会话模式不能变"）。界面上已经没有入口调它了。
      state.defaultMode = String(args?.id ?? "standard");
      return state.defaultMode as T;
    }
    // 后台执行（隐形桌面）：开着时返回一个"跑在隐形桌面上的窗口"，
    // 让 UI 能测出"chip 变紫 + 提示里列出正在跑什么"
    case "bg_desk_status":
      return {
        enabled: state.bgDeskEnabled,
        available: true,
        error: null,
        deskName: state.bgDeskEnabled ? "orbcat_bg" : "",
        windows: state.bgDeskEnabled
          ? [{ title: "报告.docx - Word", width: 1200, height: 800, pid: 4242 }]
          : [],
      } as T;
    case "bg_desk_set": {
      if (state.bgDeskSetError) {
        throw state.bgDeskSetError;
      }
      state.bgDeskEnabled = !!args?.enabled;
      return state.bgDeskEnabled as T;
    }
    case "chat": {
      if (state.chatDelayMs > 0) {
        await new Promise((r) => setTimeout(r, state.chatDelayMs));
      }
      if (state.chatError) {
        throw state.chatError;
      }
      const input = String(args?.input ?? "");
      const images = (args?.images as string[] | undefined) ?? [];
      const now = Date.now();
      state.messages.push({ role: "user", text: input, images, at: now });
      state.messages.push({
        role: "assistant",
        text: state.chatReply,
        steps: [
          { kind: "tool_call", name: "read_file", detail: '{"path":"a.md"}' },
          { kind: "tool_result", name: "read_file", detail: "FILE_BODY_MARKER\n" + "x".repeat(300) },
        ],
        at: now + 1,
        model: state.selectedModel ?? "mock-1",
      });
      return {
        answer: state.chatReply,
        steps: [
          { kind: "tool_call", name: "read_file", detail: '{"path":"a.md"}' },
          { kind: "tool_result", name: "read_file", detail: "FILE_BODY_MARKER\n" + "x".repeat(300) },
        ],
        iterations: 1,
        interrupted: false,
        usage: { prompt: 1, completion: 2, total: 3 },
      } as T;
    }
    default: {
      if (cmd in emptyOk) {
        const base = emptyOk[cmd];
        if (
          cmd === "session_new" ||
          cmd === "session_switch" ||
          cmd === "session_fork" ||
          cmd === "session_truncate" ||
          cmd === "distill_session_open"
        ) {
          return sessionPayload() as T;
        }
        return base as T;
      }
      console.warn(`[mock-invoke] 未映射命令: ${cmd}`, args);
      return null as T;
    }
  }
}

export function callsOf(cmd: string): MockInvokeCall[] {
  return bag.calls.filter((c) => c.cmd === cmd);
}

export async function listen<T>(
  event: string,
  handler: (e: { payload: T }) => void,
): Promise<() => void> {
  const l = { event, handler: handler as (e: { payload: unknown }) => void };
  bag.listeners.push(l);
  return () => {
    const i = bag.listeners.indexOf(l);
    if (i >= 0) bag.listeners.splice(i, 1);
  };
}

/**
 * 测试用：模拟后端推一条事件（`agent-progress` 等）。
 *
 * 为什么要这个：进度事件是「流式时间线」的唯一输入源 —— 没有它就只能测
 * 最终结果，测不到"思考与工具是否交错"这类顺序问题。
 */
export function emitEvent(event: string, payload: unknown): void {
  for (const l of [...bag.listeners]) {
    if (l.event === event) l.handler({ payload });
  }
}

export function getCurrentWindow() {
  return {
    setFocus: async () => {},
    show: async () => {},
    hide: async () => {},
    setAlwaysOnTop: async () => {},
    close: async () => {},
    /**
     * 手动拖动窗口（models 窗口整窗拖动用，见 src/models.ts bindWindowDrag）。
     * 记进 calls 里，供测试断言「哪些区域该拖 / 不该拖」。
     */
    startDragging: async () => {
      bag.calls.push({ cmd: "window:start_dragging", args: undefined });
    },
  };
}
