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
  models: {
    id: string;
    name: string;
    vendor: string;
    url: string;
    supportsToolCall: boolean;
    supportsImages: boolean;
  }[];
  messages: MockStoredMsg[];
  sessionList: { id: string; title: string; kind: string; updatedAt: number }[];
  currentSessionId: string;
  memPending: { id: string; content: string; source: string; created_at: string }[];
  permPending: unknown[];
  bootNeedsInit: boolean;
  bootHasModels: boolean;
  windowModes: string[];
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
    selectedModel: "mock-1",
    models: [
      {
        id: "mock-1",
        name: "Mock 模型",
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
    bootNeedsInit: false,
    bootHasModels: true,
    windowModes: [],
  };
}

const g = globalThis as typeof globalThis & { __floatAgentMock?: MockBag };
if (!g.__floatAgentMock) {
  g.__floatAgentMock = { state: defaultState(), calls: [], listeners: [] };
}

export const bag = g.__floatAgentMock;
export const state = bag.state;
export const calls = bag.calls;

export function resetMock(overrides: Partial<MockState> = {}): void {
  bag.calls.length = 0;
  bag.listeners.length = 0;
  Object.assign(bag.state, defaultState(), overrides);
}

function sessionPayload() {
  return {
    id: state.currentSessionId,
    title: "主聊天",
    kind: "main",
    createdAt: Date.now() - 1000,
    updatedAt: Date.now(),
    messages: state.messages.map((m) => ({ ...m })),
  };
}

function sessionStatePayload() {
  return {
    session: sessionPayload(),
    list:
      state.sessionList.length > 0
        ? state.sessionList
        : [
            {
              id: state.currentSessionId,
              title: "主聊天",
              kind: "main",
              updatedAt: Date.now(),
            },
          ],
  };
}

const emptyOk: Record<string, unknown> = {
  set_window_mode: null,
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
  chat_cancel: null,
  app_quit: null,
  mcp_refresh: null,
  mcp_set_url: null,
  mcp_load_group: "ok",
  mcp_unload_group: "ok",
  perm_add_rule: null,
  perm_remove_rule: null,
  perm_request_decide: null,
  perm_cmd_policy_set: null,
  image_thumb: "",
  capture_screen: "data:image/jpeg;base64,xxxx",
  exe_path: "C:\\mock\\float-agent.exe",
  exe_cmd: "",
  autostart_status: false,
  foreground_context: null,
  foreground_history: [],
  skills_list: [],
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
};

export async function invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  bag.calls.push({ cmd, args });

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
      return state.selectedModel as T;
    case "session_state":
      return sessionStatePayload() as T;
    case "session_new":
    case "session_switch":
    case "session_fork":
    case "session_truncate":
      return sessionPayload() as T;
    case "mem_pending":
      return state.memPending as T;
    case "perm_requests_list":
      return state.permPending as T;
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
          cmd === "session_truncate"
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
  };
}
