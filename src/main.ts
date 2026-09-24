/**
 * float-agent — 悬浮球前端
 *
 * 双态：orb（56x56 悬浮球）↔ panel（对话面板）
 *
 * 支持输入：
 *   - 文字
 *   - **图片**：Ctrl+V 粘贴 / 拖拽文件 / 点「截」按钮抓当前屏幕
 *
 * ⚠️ 窗口尺寸/位置**不在这里算**，一律交给 Rust 侧。
 *    原因：Windows 会把窗口最小尺寸钳制到 SM_CXMINTRACK（本机 202px），
 *    Tauri 的 setSize 走的是受约束的路径。Rust 侧用 Win32 SetWindowPos 绕开。
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { mdToHtml } from "./markdown";

/** 判定"拖动"的像素阈值 */
const DRAG_THRESHOLD = 6;

/** 单张图片大小上限（base64 后），超过就丢弃并提示 */
const MAX_IMAGE_CHARS = 8 * 1024 * 1024;

type Mode = "orb" | "panel" | "menu";

interface SearchStatus {
  provider: string;
  providerLabel: string;
  hasKey: boolean;
  keyPreview: string;
  ready: boolean;
  path: string;
}

interface ModelView {
  id: string;
  name: string;
  vendor: string;
  url: string;
  supportsToolCall: boolean;
  supportsImages: boolean;
  maxInputTokens?: number | null;
  maxOutputTokens?: number | null;
  hasKey?: boolean;
}

/** pmem.projects 一行（SQLite 索引；记忆正文在 projects/<名>/MEMORY.md） */
interface ProjectRow {
  id: number;
  name: string;
  rootPath: string;
  createdAt: string;
}

/** 远端 GET /models 返回的一项 */
interface RemoteModelInfo {
  id: string;
  ownedBy?: string;
  /** 上下文窗口（token）。部分网关提供，导入时自动填 maxInputTokens */
  contextLength?: number;
  /** 最大输出（token） */
  maxOutputLength?: number;
}

/** 编辑表单回填（不含完整 Key） */
interface ModelEditView {
  id: string;
  name: string;
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

/** 添加/编辑表单的会话内状态（re-render 时保住输入，错误也能回填） */
interface ModelFormState {
  /** null = 新建；否则为正在编辑的旧 id */
  editId: string | null;
  name: string;
  id: string;
  url: string;
  /** 明文 Key（仅内存；不落日志） */
  apiKey: string;
  clearKey: boolean;
  tool: boolean;
  img: boolean;
  headersText: string;
  maxIn: string;
  maxOut: string;
  /** 内联错误（不进聊天、不丢表单） */
  error?: string;
  /** 预设 key，用于填 URL */
  preset: string;
}

/** 设置页「拉取模型列表」的会话内状态（re-render 时要保住） */
interface RemoteFetchState {
  items: RemoteModelInfo[];
  /**
   * 来源：
   *   `custom`              手动填 URL + Key
   *   `preset:<key>`        常见提供商预设（自动填 Base URL）
   *   `model:<id>`          用已配置模型的提供商凭据
   */
  source: string;
  sourceUrl: string;
  apiKey: string;
  headersText: string;
  tool: boolean;
  img: boolean;
  error?: string;
  /** 本地已配置的 id 集合，渲染时用来标「已配置」 */
  localIds: Set<string>;
  checked: Set<string>;
  /** 最近一次拉取实际请求的提供商端点，展示用 */
  lastEndpoint?: string;
}

/** 常见提供商 → OpenAI 兼容 Base URL（拉列表 = GET {url}/models） */
const PROVIDER_PRESETS: { key: string; name: string; url: string }[] = [
  { key: "openrouter", name: "OpenRouter", url: "https://openrouter.ai/api/v1" },
  { key: "deepseek", name: "DeepSeek", url: "https://api.deepseek.com/v1" },
  { key: "zhipu", name: "智谱 GLM", url: "https://open.bigmodel.cn/api/paas/v4" },
  { key: "volces", name: "火山方舟", url: "https://ark.cn-beijing.volces.com/api/v3" },
  { key: "sensenova", name: "商汤 SenseNova", url: "https://token.sensenova.cn/v1" },
  { key: "xfyun", name: "讯飞 MaaS", url: "https://maas-api.cn-huabei-1.xf-yun.com/v2" },
  { key: "moonshot", name: "Moonshot", url: "https://api.moonshot.cn/v1" },
  {
    key: "dashscope",
    name: "阿里百炼",
    url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
  },
  { key: "openai", name: "OpenAI", url: "https://api.openai.com/v1" },
  { key: "ollama", name: "Ollama（本地）", url: "http://127.0.0.1:11434/v1" },
];

function providerPreset(key: string): { key: string; name: string; url: string } | undefined {
  return PROVIDER_PRESETS.find((p) => p.key === key);
}

function shortHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url.replace(/^https?:\/\//, "").split("/")[0] || url;
  }
}

function modelsEndpointPreview(baseUrl: string): string {
  return `${baseUrl.replace(/\/+$/, "")}/models`;
}

interface AgentStep {
  kind: string;
  name: string | null;
  detail: string;
}

interface AgentRun {
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

/** 一条待用户拍板的权限申请（与后端 `perm_request::PendingView` 对应） */
interface PendingPerm {
  id: string;
  /** "file"（文件级，路径维度） | "command"（命令级，指纹维度） */
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
}

interface ChatEntry {
  role: "user" | "assistant" | "error" | "system" | "running";
  text: string;
  /** 插话：插到 running 气泡之前，不要永远垫底 */
  insertBeforeRunning?: boolean;
  /** 轮数/上下文降级后可点「继续」 */
  continueRun?: boolean;
  /**
   * 时间线：这一步"想了什么 / 做了什么"，**按发生顺序**排列。
   *
   * - 运行中的气泡：由进度事件边收边建（`thinking` 事件给下一段思考开门）
   * - 历史消息：直接来自磁盘（后端 `cap_steps` 保序落盘）
   *
   * 以前这里是两个聚合字段（`progress` 行数组 + `streamReasoning` 单串），
   * 思考与工具天然被分到两处、不可能交错 —— 用户 2026-09-22 报的
   * 「思考都堆在一块」就是这个结构造成的。现在条目自带顺序。
   */
  items?: AgentStep[];
  images?: string[];
  /** 流式正文（边收边显示） */
  streamText?: string;
  /** 该轮流出的正文作废（模型中途又去调工具了） */
  streamDiscarded?: boolean;
  /**
   * 本轮（一次模型调用）的思考条目是否已经开过。
   *
   * 一条 `thinking` 事件（新一轮开始）把它置回 false，下一个思考增量就会
   * **新开一个条目** —— 空轮不留空标题，多轮则各成一段。
   */
  reasonOpen?: boolean;
  /**
   * **旧字段**：单串思考过程（老会话数据 / 老后端）。
   *
   * 新数据以 `items` 里的 `reasoning` 条目为准；只有拿不到条目时才回退到它，
   * 这样读到旧会话不会丢内容、也不会重复显示。
   */
  reasoning?: string;
  /** 本轮被「停止」/ 出错打断（回答是半截） */
  interrupted?: boolean;
  /** 生成本条回答的模型 id（用量角标 / 台账用） */
  model?: string;
  /** 本轮问答的 token 用量合计（无则不计） */
  usage?: TokenUsage;
  /**
   * 该条在**磁盘会话里的下标**。
   *
   * ⚠️ 不能用 `entries` 的下标代替：`entries` 里有 running/system/error 这些
   * 不落盘的条目，`sessionToEntries` 还会做一次过滤，落盘又是全量重写。
   * 分叉点必须按磁盘下标传，否则会稳定地错一条。
   */
  storedIdx?: number;
  /** 执行中「插话」的这条还在排队（等当前轮结束才送达） */
  queued?: boolean;
  /** 插话 id（收到 steer 事件后据此把「排队中」改成「已送达」） */
  steerId?: string;
  /** 入队时排在队列第几位 */
  steerQueuePos?: number;
}

// ---------------- 会话持久化（类型与后端 sessions.rs 对应） ----------------

/** 一次模型调用的 token 用量（后端 `llm::TokenUsage` 的归一化结果） */
interface TokenUsage {
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

interface StoredMsg {
  role: "user" | "assistant";
  text: string;
  images?: string[];
  steps?: AgentStep[];
  /** 本轮的思考过程。老数据没有这个字段 */
  reasoning?: string;
  /** 本轮被中断（text 是半截） */
  interrupted?: boolean;
  /** 插话 id —— UI 不铺气泡，画在 steps 时间线（真实交错） */
  steerId?: string;
  /** 生成本条回答的模型 id（用量统计用）。老数据没有 */
  model?: string;
  /** 本轮问答的 token 用量合计。老数据 / 服务端没给 usage → 无此字段 */
  usage?: TokenUsage;
  at: number;
}

interface Session {
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
}

interface SessionMeta {
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
}

interface SessionState {
  session: Session;
  list: SessionMeta[];
}

let currentSessionId = "";
let sessionList: SessionMeta[] = [];
/** 会话是否已从磁盘加载过（避免每次展开面板都重灌、丢掉进行中的对话） */
let sessionLoaded: Promise<void> | null = null;

/**
 * 是否需要初始化（Rust `boot_state` 告知）。
 *
 * 为 true 表示 `agent-data/` 目录不存在 = 全新克隆的仓库。
 * 这时**人格文件、记忆、技能全都是空的**，必须提示用户初始化，
 * 否则它会一脸茫然地开始瞎聊。
 */
let needsInit = false;

/**
 * 是否有可用模型（Rust `boot_state` 告知）。
 *
 * 目录建好了但一个模型都没配 → 聊天发不出去（Rust 侧会报
 * "尚未选择模型"）。所以提示要分两级：先「点初始化建目录」，
 * 再「去设置里加模型」。
 */
let hasModels = true;

/**
 * 流式**显示**的字符上限。
 *
 * 只截显示，不影响落盘与最终答案：回答落地时是用后端返回的完整 `run.answer`
 * 重建的。设它是为了让"模型疯狂重复输出"的时候界面还能动 —— 有个模型会把
 * 整篇正文重复吐进 reasoning 里。
 */
const MAX_STREAM_TEXT_CHARS = 60_000;
const MAX_STREAM_REASON_CHARS = 20_000;

/** 追加流式增量，超上限就停住（不再增长，避免每一帧都在拼超长字符串） */
function capAppend(cur: string | undefined, chunk: string, limit: number): string {
  const base = cur ?? "";
  if (base.length >= limit) return base;
  const s = base + chunk;
  return s.length <= limit ? s : `${s.slice(0, limit)}\n…（显示已截断）`;
}

/** 剥掉正文里混入的工具轨迹/思考尾巴（老数据 + 模型回显兜底，与后端 strip_step_folds 对齐） */
function stripStepFolds(text: string): string {
  if (!text) return text;
  const markers = [
    "\n«steps\n",
    "\n«steps",
    "\n[思考] ",
    "\n[状态] ",
    "\n[中间正文] ",
    "\n[已执行",
    "\n·think ",
    "\n·status ",
    "\n·text ",
    "\n调用 ",
    "\n  参数: ",
  ];
  let cut = text.length;
  for (const m of markers) {
    const i = text.indexOf(m);
    if (i > 0 && i < cut) cut = i;
  }
  let out = text.slice(0, cut).trimEnd();
  // 整块删掉嵌在中间的 «steps … »
  out = out.replace(/«steps\n[\s\S]*?\n»/g, "").trimEnd();
  return out;
}

/** 把磁盘上的会话消息转成面板的 entries */
function sessionToEntries(s: Session): ChatEntry[] {
  const out: ChatEntry[] = [];
  s.messages.forEach((m, idx) => {
    // 插话：历史回灌仍按 user 进模型，但 UI **不铺气泡** ——
    // 它已作为 `kind=steer` 画在后续 assistant 的 steps 时间线里（真实交错）。
    // 老数据没有 steerId → 仍当普通用户消息显示。
    if (m.steerId) {
      return;
    }
    // ⚠️ 有 reasoning / items / interrupted 也要留 —— 否则「被中断 / 出错，
    //    只有思考或只有一条错误」的那轮会整条消失，用户看不到"停在这儿了"
    if (
      !m.text &&
      !(m.images && m.images.length) &&
      !m.reasoning &&
      !(m.steps && m.steps.length) &&
      !m.interrupted
    ) {
      return;
    }
    out.push({
      role: m.role,
      text: stripStepFolds(m.text),
      // 磁盘上的 steps 就是时间线（后端保序落盘），直接当条目用
      items: m.steps,
      images: m.images,
      reasoning: m.reasoning,
      interrupted: m.interrupted,
      model: m.model,
      usage: m.usage,
      // 磁盘下标：分叉点要用它，不能拿 entries 下标顶替
      storedIdx: idx,
    });
  });
  return out;
}

/** 启动自检：agent-data 不存在 / 没有模型 → 标记需要引导 */
async function checkBoot(): Promise<void> {
  try {
    const st = await invoke<{
      needsInit: boolean;
      hasModels: boolean;
      dataDir: string;
    }>("boot_state");
    needsInit = st.needsInit;
    hasModels = st.hasModels;
    if (needsInit) {
      console.warn("[float-agent] 数据目录不存在，需要初始化:", st.dataDir);
    } else if (!hasModels) {
      console.warn("[float-agent] 还没有配置任何模型");
    }
  } catch (e) {
    // 自检失败不该挡住正常使用 —— 保守起见当作"不需要初始化"
    console.error("[float-agent] 启动自检失败:", e);
  }
}

async function loadSession(): Promise<void> {
  try {
    const st = await invoke<SessionState>("session_state");
    currentSessionId = st.session.id;
    sessionList = st.list;
    entries = sessionToEntries(st.session);
  } catch (e) {
    console.error("[float-agent] 读取会话失败:", e);
  }
}

function ensureSessionLoaded(): Promise<void> {
  sessionLoaded ??= loadSession();
  return sessionLoaded;
}

/** 开新会话 → 挂到**当前激活项目**（主对话则不绑） */
async function newSession(): Promise<void> {
  if (busy) return;
  try {
    const s = await invoke<Session>("session_new");
    currentSessionId = s.id;
    entries = [];
    await refreshSessionList();
    // 只绑**这一条新会话**，不动其它
    if (activeProjectId != null) {
      await invoke("pmem_bind_session", {
        sessionId: s.id,
        projectId: activeProjectId,
      }).catch(() => {});
      await refreshSessionProjectTags();
    }
    view = "chat";
    renderBody();
  } catch (e) {
    pushEntry("error", `新建会话失败：${e}`);
  }
}

/** 进入会话：顶栏项目跟着会话走（主聊天 → 主对话） */
async function switchSession(id: string): Promise<void> {
  if (busy) return;
  try {
    const s = await invoke<Session>("session_switch", { id });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    await refreshSessionList();
    await loadProjects();
    await refreshSessionProjectTags();
    const meta = sessionList.find((x) => x.id === id);
    // 主聊天永不挂项目 → 记忆回主对话
    const wantPid =
      meta?.kind === "main"
        ? null
        : (projects.find((p) => p.name === meta?.projectName)?.id ?? null);
    activeProjectId = wantPid;
    await invoke("pmem_set_active", { id: wantPid }).catch(() => {});
    view = "chat";
    renderBody();
  } catch (e) {
    pushEntry("error", `切换会话失败：${e}`);
  }
}

/**
 * 从主聊天的第 `upto` 条消息处分叉出一个新会话。
 *
 * 新会话拿到的是**这一段上下文窗口**（最近 6 小时 ∪ 最近 10 条，和下一轮回灌
 * 给模型的完全一致），不是到那条为止的全部历史 —— 之后两边各走各的。
 */
async function forkSession(upto: number): Promise<void> {
  if (busy) return;
  try {
    const s = await invoke<Session>("session_fork", { upto });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    await refreshSessionList();
    view = "chat";
    renderBody();
    pushEntry(
      "system",
      `⑂ 已分叉出新会话「${s.title}」：带走了下一轮会喂给模型的 ${s.messages.length} 条上下文（窗口 = 最近 6 小时 ∪ 最近 10 条）`,
    );
  } catch (e) {
    pushEntry("error", `分叉失败：${e}`);
  }
}

/**
 * 分叉按钮的事件委托。
 *
 * 与 `installCopyHandlers` 分开装：复制分支已经在用，别去动它（风险最小的做法）。
 * 委托挂在 document 上，重绘不用重新绑定。
 */
function installForkHandlers(): void {
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const btn = t.closest<HTMLElement>(".msg-fork");
    if (!btn) return;
    e.stopPropagation();
    const upto = Number(btn.dataset.upto);
    if (!Number.isFinite(upto) || upto < 0) return;
    void forkSession(upto);
  });
}

/**
 * 面板内多字段输入（替代 window.prompt 的系统白框）。
 * 返回 `null` = 取消；否则为各字段值。
 */
function askFields(
  title: string,
  fields: { key: string; label: string; placeholder?: string; value?: string }[],
  okLabel = "确定",
): Promise<Record<string, string> | null> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "confirm-mask";
    const inputs = fields
      .map(
        (f, i) =>
          `<label class="ask-field"><span>${esc(f.label)}</span>
           <input data-k="${esc(f.key)}"${i === 0 ? " data-autofocus='1'" : ""}
             placeholder="${esc(f.placeholder ?? "")}" value="${esc(f.value ?? "")}" /></label>`,
      )
      .join("");
    mask.innerHTML = `
      <div class="confirm-card">
        <div class="confirm-title">${esc(title)}</div>
        <div class="ask-fields">${inputs}</div>
        <div class="confirm-actions">
          <button type="button" class="set-btn" data-k="no">取消</button>
          <button type="button" class="set-btn add" data-k="yes">${esc(okLabel)}</button>
        </div>
      </div>`;
    (document.querySelector(".panel") ?? app).appendChild(mask);
    mask.querySelector<HTMLInputElement>("input[data-autofocus='1']")?.focus();

    let done = false;
    const collect = (): Record<string, string> => {
      const out: Record<string, string> = {};
      mask.querySelectorAll<HTMLInputElement>("input[data-k]").forEach((el) => {
        out[el.dataset.k!] = el.value.trim();
      });
      return out;
    };
    const close = (v: Record<string, string> | null): void => {
      if (done) return;
      done = true;
      mask.remove();
      resolve(v);
    };
    mask.addEventListener("click", (e) => {
      const t = e.target as HTMLElement;
      if (t === mask) {
        close(null);
        return;
      }
      const b = t.closest<HTMLElement>("[data-k]");
      if (!b) return;
      if (b.dataset.k === "no") close(null);
      if (b.dataset.k === "yes") close(collect());
    });
    mask.addEventListener("keydown", (e) => {
      const ke = e as KeyboardEvent;
      if (ke.key === "Escape") close(null);
      if (ke.key === "Enter") close(collect());
    });
  });
}

/**
 * 面板内的确认框（Promise<boolean>）。
 *
 * 为什么不用 `window.confirm`：面板只有 420x600、纯深色、常驻置顶，
 * WebView2 弹的是系统灰底对话框 —— 尺寸按桌面比例走，盖住整块面板还带
 * 系统字体，跟周围完全不是一回事；而且它是否被宿主拦掉取决于环境，
 * 一旦静默不弹，用户点了「删除」什么都不会发生（看起来像功能坏了）。
 * 自己画一个，尺寸可控、必弹、Esc/点遮罩也能取消。
 */
function askConfirm(
  title: string,
  body: string,
  danger = "删除",
  dangerStyle = true,
): Promise<boolean> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "confirm-mask";
    mask.innerHTML = `
      <div class="confirm-card">
        <div class="confirm-title">${esc(title)}</div>
        <div class="confirm-body">${esc(body)}</div>
        <div class="confirm-actions">
          <button type="button" class="set-btn" data-k="no">取消</button>
          <button type="button" class="set-btn${dangerStyle ? " danger" : ""}" data-k="yes">${esc(danger)}</button>
        </div>
      </div>`;
    (document.querySelector(".panel") ?? app).appendChild(mask);

    let done = false;
    const close = (v: boolean): void => {
      if (done) return;
      done = true;
      document.removeEventListener("keydown", onKey, true);
      mask.remove();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key !== "Escape") return; // Enter 交给浏览器：它点的是**当前有焦点**那颗（默认「取消」）
      e.preventDefault();
      e.stopPropagation();
      close(false);
    };

    mask.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
      b.addEventListener("click", () => close(b.dataset.k === "yes")),
    );
    // 点遮罩（卡片外）＝取消
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) close(false);
    });
    document.addEventListener("keydown", onKey, true);

    // 焦点给「取消」而不是「删除」：回车不该是破坏性操作
    mask.querySelector<HTMLButtonElement>('[data-k="no"]')?.focus();
  });
}

/**
 * 删除消息：从第 `upto` 条起（含）到末尾，全部丢掉。
 *
 * 用户在对话流里点某条消息的「删除」就到这里 —— 语义是"这条以及它后面引出的
 * 全部内容都不要了"。**不可恢复**，所以先弹确认框，并把要删的条数和内容摘要
 * 摆出来（只问"确定吗"不够，用户不知道自己删的是哪几条）。
 *
 * 三个细节：
 * - **忙碌时直接拒绝**：后端也有硬保护（`run.is_active()`），这里先拦一道免得白跑一趟。
 * - `upto` 用**磁盘下标**（storedIdx），不是 entries 下标 —— 后者被 running/system
 *   这些不落盘的条目顶偏过，会稳定地删错位置。
 * - 删完**重新对齐 entries 和会话列表**：后端返回截断后的会话，直接照着重建，
 *   不要自己在内存里 splice（还得分清哪些条目不落盘，容易错）。
 */
async function deleteMessagesFrom(upto: number): Promise<void> {
  if (busy) {
    showToast("对话进行中，先停止或等它结束再删除消息", "error");
    return;
  }

  // 要删掉的那几条在磁盘上是 [upto..]；面板上对应的 entries 用 storedIdx 反查
  const victims = entries.filter((e) => e.storedIdx !== undefined && e.storedIdx >= upto);
  const who = entries.find((e) => e.storedIdx === upto);
  const head = who ? who.text.replace(/\s+/g, " ").slice(0, 40) : "（空消息）";

  let total = victims.length;
  try {
    const st = await invoke<SessionState>("session_state");
    total = Math.max(0, st.session.messages.length - upto);
  } catch {
    // 读不到就用面板上的估算，不影响确认框继续
  }

  const ok = await askConfirm(
    "删除这条消息以及它后面的全部内容？",
    `从「${head}${who && who.text.length > 40 ? "…" : ""}」开始，共 ${total} 条。\n此操作不可恢复。`,
  );
  if (!ok) return;

  try {
    const s = await invoke<Session>("session_truncate", { upto });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    await refreshSessionList();
    view = "chat";
    renderBody();
    showToast(`已删除 ${total} 条消息，剩 ${s.messages.length} 条`, "ok");
  } catch (e) {
    showToast(`删除失败：${e}`, "error");
  }
}

/**
 * 「删除」按钮的事件委托。
 *
 * 与 copy / fork 分开装：它们都在用，别去动（风险最小的做法）。
 * 委托挂在 document 上，重绘不用重新绑定。
 */
function installDeleteHandlers(): void {
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const btn = t.closest<HTMLElement>(".msg-del");
    if (!btn) return;
    e.stopPropagation();
    const upto = Number(btn.dataset.upto);
    if (!Number.isFinite(upto) || upto < 0) return;
    void deleteMessagesFrom(upto);
  });
}

/**
 * 重新生成：丢掉这条 AI 回复（及其后全部），把上一句用户提问重发一遍。
 *
 * 为什么不新写后端命令：截断用现成的 `session_truncate`（语义一致 + 自带
 * 忙碌保护），重发用现成的 `chat`。前端串起来即可。
 *
 * 边界处理：
 * - 找不到上一条 user 消息（比如这条回答前面被删空了）→ 提示后中止，不硬发。
 * - 忙碌中 → 直接拒绝（后端也会拦，这里先给个明确提示）。
 * - 重发前先截断**并同步 entries**，否则 chat 完成后 `reloadSessionKeepScroll`
 *   会按旧条数对齐，容易把刚生成的回答冲掉。
 */
async function regenerateFrom(upto: number): Promise<void> {
  if (busy) {
    showToast("对话进行中，先停止或等它结束再重新生成", "error");
    return;
  }

  // 这条 assistant 在磁盘上的下标是 upto，它的上一条（user）应是 upto-1
  const userIdx = upto - 1;
  const userEntry = entries.find((e) => e.storedIdx === userIdx);
  if (!userEntry || userEntry.role !== "user") {
    showToast("找不到这条回答对应的提问，无法重新生成", "error");
    return;
  }

  const ok = await askConfirm(
    "重新生成这条回答？",
    "将丢弃这条 AI 回复，并基于同一条提问重新生成。\n会重新消耗 token。",
    "重新生成",
    false,
  );
  if (!ok) return;

  // 先把提问文本和图片存下来 —— 截断后 entries 会重建，拿不到原对象了
  const promptText = userEntry.text;
  const promptImgs = userEntry.images ? userEntry.images.slice() : [];

  try {
    // 1) 截断到那条提问之前（丢掉 assistant 及其后），保持"刚问完"的状态
    const s = await invoke<Session>("session_truncate", { upto });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    await refreshSessionList();
    renderBody();
  } catch (e) {
    showToast(`重新生成失败：${e}`, "error");
    return;
  }

  // 2) 把那条提问重新发一遍（走和 send 一样的流程，但输入来自历史而非输入框）
  await resendPrompt(promptText, promptImgs);
}

/**
 * 用一段**已存在的文本/图片**重跑一轮对话（重新生成的执行体）。
 *
 * 与 `send` 的差别：不读输入框、不清草稿、不 push 用户气泡（那条提问还在
 * entries 里，截断时被保留了）。其余流程（running 气泡、进度、重载）完全一致。
 */
async function resendPrompt(text: string, imgs: string[]): Promise<void> {
  if (!selectedModel) {
    pushEntry("error", "请先选择一个模型");
    return;
  }

  // 先放「进行中」气泡，进度事件会往里建时间线条目
  entries.push({ role: "running", text: "正在思考…", items: [] });
  const b0 = document.getElementById("panel-body");
  if (b0) {
    paintMessages(b0);
    b0.scrollTop = b0.scrollHeight;
  }

  busy = true;
  permMuteThisTurn = false;
  setThinking(true);
  setBusyUi(true);

  try {
    const run = await invoke<AgentRun>("chat", { input: text, images: imgs });
    const idx = entries.findIndex((x) => x.role === "running");
    if (idx >= 0) entries.splice(idx, 1);
    await reloadSessionKeepScroll(
      entries.filter((x) => (x.role === "user" && !x.queued) || x.role === "assistant").length,
    );
    if (run.interrupted) {
      const reason = run.stopReason && run.stopReason !== "已停止" ? run.stopReason : "";
      const canContinue = reason.includes("轮数上限") || reason.includes("上下文");
      pushEntry(
        "system",
        reason ? `⚠️ ${reason}` : "⏹ 已停止，本轮已产出的内容保留在上面",
        undefined,
        undefined,
        canContinue ? { continueRun: true } : undefined,
      );
    }
  } catch (e) {
    const idx = entries.findIndex((x) => x.role === "running");
    if (idx >= 0) entries.splice(idx, 1);
    pushEntry("error", String(e));
  } finally {
    busy = false;
    setThinking(false);
    setBusyUi(false);
  }
}

/**
 * 「重新生成」按钮的事件委托（与删除分开装，互不影响）。
 */
function installRegenHandlers(): void {
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const btn = t.closest<HTMLElement>(".msg-regen");
    if (!btn) return;
    e.stopPropagation();
    const upto = Number(btn.dataset.upto);
    if (!Number.isFinite(upto) || upto < 0) return;
    void regenerateFrom(upto);
  });
}

async function deleteSession(id: string): Promise<void> {
  if (busy) return;
  try {
    const meta = sessionList.find((s) => s.id === id);
    const proj = meta?.projectName ?? null;
    const sibs = sessionList.filter((s) => s.projectName === proj && s.id !== id);
    await invoke("session_delete", { id });
    // 若删的是当前会话，后端会切到别的 → 重新对齐
    const st = await invoke<SessionState>("session_state");
    currentSessionId = st.session.id;
    sessionList = st.list;
    entries = sessionToEntries(st.session);
    if (proj && sibs.length === 0) {
      const clear = await askConfirm(
        "清空项目记忆",
        `「${proj}」下已无其它会话。\n是否清空项目记忆 MEMORY.md？（项目保留）`,
        "清空记忆",
        true,
      );
      if (clear) {
        await invoke("pmem_clear_memory", { name: proj }).catch(() => {});
        showToast(`已清空「${proj}」项目记忆`, "ok");
      }
    }
    await refreshSessionList();
    await refreshSessionProjectTags();
    renderBody();
    showToast("已删除会话", "ok", 2000);
  } catch (e) {
    showToast(`删除失败：${e}`, "error");
  }
}

async function refreshSessionList(): Promise<void> {
  try {
    const st = await invoke<SessionState>("session_state");
    sessionList = st.list;
    currentSessionId = st.session.id;
  } catch {
    /* 列表刷新失败不影响主流程 */
  }
}

function fmtTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}/${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 会话页：**上方主聊天（唯一、不可删）**，下方任务会话列表 */
/** 会话页：按**项目**分组；📁 标签独立、不被标题省略吃掉 */
function renderSessionsView(): string {
  const projTag = (name?: string | null): string =>
    name ? `<span class="sess-proj" title="项目：${esc(name)}">📁 ${esc(name)}</span>` : "";

  const itemRow = (s: SessionMeta): string => `
      <div class="sess-item${s.current ? " cur" : ""}" data-id="${esc(s.id)}" title="右键挂靠到项目">
        <button class="sess-open" data-id="${esc(s.id)}" title="切换到该会话">
          <span class="sess-title-row">
            <span class="sess-title">${esc(s.title || "（无标题）")}</span>
            ${s.kind === "fork" ? '<span class="sess-fork-tag">分支</span>' : ""}
            ${projTag(s.projectName)}
            ${s.current ? '<span class="sess-cur-tag">当前</span>' : ""}
          </span>
          <span class="sess-meta">${fmtTime(s.updatedAt)} · ${s.count} 条</span>
        </button>
        ${s.kind === "main" ? "" : `<button class="sess-del" data-id="${esc(s.id)}" title="删除该会话">✕</button>`}
      </div>`;

  const groups = new Map<string | null, SessionMeta[]>();
  for (const s of sessionList) {
    // 主聊天永远进「主对话」组
    const k = s.kind === "main" ? null : (s.projectName ?? null);
    if (!groups.has(k)) groups.set(k, []);
    groups.get(k)!.push(s);
  }
  const activeName =
    activeProjectId == null
      ? null
      : (projects.find((p) => p.id === activeProjectId)?.name ?? null);
  const keys = [...groups.keys()].sort((a, b) => {
    if (a === activeName) return -1;
    if (b === activeName) return 1;
    if (a === null) return -1;
    if (b === null) return 1;
    return (a ?? "").localeCompare(b ?? "");
  });

  const sections = keys
    .map((k) => {
      const list = groups.get(k)!;
      const head =
        k === null
          ? `主对话 / 未挂靠 · ${list.length}`
          : `📁 ${esc(k)} · ${list.length}${k === activeName ? " · 当前项目" : ""}`;
      const rows = list.map(itemRow).join("");
      const sepAttrs =
        k === null
          ? ""
          : ` data-pname="${esc(k)}" data-pid="${projects.find((p) => p.name === k)?.id ?? ""}"`;
      return `
    <div class="sess-sep"${sepAttrs}>${head}</div>
    <div class="sess-group${k === activeName ? " cur-proj" : ""}">${rows}</div>`;
    })
    .join("");

  return `
    <div class="mem-head">会话<button class="mem-back" id="sess-new">＋ 新建</button></div>
    <div class="set-hint" style="margin-bottom:8px">
      <b>按项目分组</b> · 右键会话挂靠/改挂靠 · 点一条进入对话
    </div>
    ${sections || '<div class="mem-empty">还没有会话。</div>'}
    <div style="margin:8px 0">
      <button class="set-btn" id="sess-compact">压缩上下文（compact）</button>
    </div>`;
}

function bindSessionsView(root: HTMLElement): void {
  root.querySelectorAll<HTMLButtonElement>(".sess-main").forEach((btn) =>
    btn.addEventListener("click", () => void switchSession(btn.dataset.id ?? "")),
  );
  root.querySelectorAll<HTMLButtonElement>(".sess-open").forEach((btn) =>
    btn.addEventListener("click", () => void switchSession(btn.dataset.id ?? "")),
  );
  root.querySelectorAll<HTMLButtonElement>(".sess-del").forEach((btn) =>
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      void deleteSession(btn.dataset.id ?? "");
    }),
  );
  document.getElementById("sess-new")?.addEventListener("click", () => void newSession());
  document.getElementById("sess-compact")?.addEventListener("click", () => void compactCurrent());

  // 右键会话 → 挂靠到项目（session 之上是项目记忆）
  installSessionBindHandler(root);
}

/** 「＋」菜单：新建会话 / 新建项目（原「项+」并入） */
function showNewMenu(x: number, y: number): void {
  document.getElementById("new-ctx")?.remove();
  const menu = document.createElement("div");
  menu.id = "new-ctx";
  menu.className = "ctx-menu";
  menu.innerHTML = `
    <div class="ctx-item" data-act="sess">新建会话</div>
    <div class="ctx-item" data-act="proj">新建项目…</div>`;
  menu.style.left = `${Math.min(x, window.innerWidth - 160)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - 100)}px`;
  document.body.appendChild(menu);
  const close = (): void => menu.remove();
  menu.addEventListener("click", async (e) => {
    const t = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item");
    if (!t) return;
    close();
    if (t.dataset.act === "sess") {
      void newSession();
      return;
    }
    const got = await askFields(
      "新建项目",
      [
        { key: "name", label: "项目名", placeholder: "M3FM / 论文精读" },
        { key: "root", label: "绑定工作目录（可空）", placeholder: "D:\\myword\\文章\\论文" },
      ],
      "创建并切换",
    );
    if (!got?.name) return;
    try {
      const p = await invoke<ProjectRow>("pmem_create", {
        name: got.name,
        rootPath: got.root || null,
      });
      await loadProjects();
      activeProjectId = p.id;
      await invoke("pmem_set_active", { id: p.id }).catch(() => {});
      await newSession(); // 新会话会绑到 p.id
      await refreshSessionProjectTags();
      showToast(`已创建「${p.name}」并打开新会话`, "ok");
    } catch (err) {
      showToast(`创建项目失败：${err}`, "error");
    }
    renderBody();
  });
  window.addEventListener("click", close, { once: true });
}

/** 会话右键：挂靠 / 重命名会话 / 删除（打开用左键） */
function showSessionBindMenu(x: number, y: number, sid: string, kind: string): void {
  document.getElementById("sess-ctx")?.remove();
  const menu = document.createElement("div");
  menu.id = "sess-ctx";
  menu.className = "ctx-menu";
  const binds = [
    `<div class="ctx-item" data-act="bind" data-pid="">主对话（不挂项目）</div>`,
    ...projects.map(
      (p) => `<div class="ctx-item" data-act="bind" data-pid="${p.id}">📁 ${esc(p.name)}</div>`,
    ),
  ].join("");
  menu.innerHTML = `
    <div class="ctx-title">挂靠到项目</div>
    ${binds}
    <div class="ctx-sep"></div>
    <div class="ctx-item" data-act="rename">重命名会话</div>
    ${kind === "main" ? "" : '<div class="ctx-item danger" data-act="del">删除会话</div>'}
  `;
  menu.style.left = `${Math.min(x, window.innerWidth - 190)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - 280)}px`;
  document.body.appendChild(menu);
  const close = (): void => menu.remove();
  menu.addEventListener("click", async (e) => {
    const t = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item");
    if (!t) return;
    const act = t.dataset.act;

    if (act === "rename") {
      close();
      const meta = sessionList.find((s) => s.id === sid);
      const got = await askFields(
        "重命名会话",
        [{ key: "title", label: "标题", value: meta?.title ?? "" }],
        "保存",
      );
      if (!got?.title) return;
      // 用 session_set_title；后端若无则回退 truncate 不可 —— 需已有命令
      try {
        await invoke("session_set_title", { id: sid, title: got.title });
        await refreshSessionList();
        renderBody();
        showToast("已重命名", "ok", 2000);
      } catch (err) {
        showToast(`重命名失败：${err}`, "error");
      }
      return;
    }

    if (act === "del") {
      close();
      const meta = sessionList.find((s) => s.id === sid);
      const proj = meta?.projectName ?? null;
      const sibs = sessionList.filter((s) => s.projectName === proj && s.id !== sid);
      const ok = await askConfirm("删除会话", `确定删除「${meta?.title ?? sid}」？`, "删除");
      if (!ok) return;
      try {
        await invoke("session_delete", { id: sid });
        // 最后一条才问清不清项目记忆
        if (proj && sibs.length === 0) {
          const clear = await askConfirm(
            "清空项目记忆",
            `「${proj}」下已无其它会话。\n是否清空项目记忆 MEMORY.md？（项目本身保留）`,
            "清空记忆",
            true,
          );
          if (clear) {
            await invoke("pmem_clear_memory", { name: proj }).catch(() => {});
            showToast(`已清空「${proj}」项目记忆`, "ok");
          }
        }
        await refreshSessionList();
        await refreshSessionProjectTags();
        renderBody();
        showToast("已删除会话", "ok", 2000);
      } catch (err) {
        showToast(`删除失败：${err}`, "error");
      }
      return;
    }

    if (act === "bind") {
      // 主聊天不允许挂靠
      if (kind === "main") {
        close();
        showToast("主聊天不能挂靠项目", "info");
        return;
      }
      const pid = t.dataset.pid === "" ? null : Number(t.dataset.pid);
      try {
        await invoke("pmem_bind_session", { sessionId: sid, projectId: pid });
        await refreshSessionProjectTags();
        renderBody();
        showToast(
          pid == null
            ? "已解除挂靠"
            : `已挂靠到「${projects.find((p) => p.id === pid)?.name ?? pid}」`,
          "ok",
          2500,
        );
      } catch (err) {
        showToast(`挂靠失败：${err}`, "error");
      }
      close();
    }
  });
  window.addEventListener("click", close, { once: true });
}

/** 拉 session→项目名 映射；顺带把误绑到项目上的主聊天解绑 */
async function refreshSessionProjectTags(): Promise<void> {
  try {
    const map = await invoke<Record<string, string | null>>("pmem_session_map");
    for (const s of sessionList) {
      s.projectName = map[s.id] ?? null;
    }
    // 主聊天永不挂项目 —— 清历史脏数据
    for (const s of sessionList) {
      if (s.kind === "main" && s.projectName) {
        await invoke("pmem_bind_session", { sessionId: s.id, projectId: null }).catch(() => {});
        s.projectName = null;
      }
    }
  } catch {
    /* 库未建时静默 */
  }
}

function installSessionBindHandler(root: HTMLElement): void {
  // 会话行右键
  root.addEventListener("contextmenu", (e) => {
    const item = (e.target as HTMLElement).closest<HTMLElement>(".sess-item");
    if (!item) return;
    e.preventDefault();
    const sid = item.dataset.id ?? "";
    if (!sid) return;
    const meta = sessionList.find((s) => s.id === sid);
    const kind = meta?.kind ?? "task";
    void loadProjects()
      .then(() => refreshSessionProjectTags())
      .then(() => {
        showSessionBindMenu(e.clientX, e.clientY, sid, kind);
      });
    return;
  });
  // 项目组标题右键 → 重命名项目
  root.addEventListener("contextmenu", (e) => {
    const sep = (e.target as HTMLElement).closest<HTMLElement>(".sess-sep[data-pname]");
    if (!sep) return;
    e.preventDefault();
    const pname = sep.dataset.pname ?? "";
    const pid = Number(sep.dataset.pid ?? 0);
    if (!pname || !pid) return;
    document.getElementById("sess-ctx")?.remove();
    const menu = document.createElement("div");
    menu.id = "sess-ctx";
    menu.className = "ctx-menu";
    menu.innerHTML = `
      <div class="ctx-title">📁 ${esc(pname)}</div>
      <div class="ctx-item" data-act="rename-proj">重命名项目</div>`;
    menu.style.left = `${Math.min(e.clientX, window.innerWidth - 180)}px`;
    menu.style.top = `${Math.min(e.clientY, window.innerHeight - 80)}px`;
    document.body.appendChild(menu);
    const close = (): void => menu.remove();
    menu.addEventListener("click", async (ev) => {
      const t = (ev.target as HTMLElement).closest<HTMLElement>(".ctx-item");
      if (!t) return;
      close();
      const got = await askFields(
        "重命名项目",
        [{ key: "name", label: "项目名", value: pname }],
        "保存",
      );
      if (!got?.name || got.name === pname) return;
      try {
        await invoke("pmem_rename", { id: pid, name: got.name });
        await loadProjects();
        if (activeProjectId === pid) {
          // 名字变了，active 不变
        }
        await refreshSessionProjectTags();
        renderBody();
        showToast("项目已重命名", "ok", 2000);
      } catch (err) {
        showToast(`重命名失败：${err}`, "error");
      }
    });
    window.addEventListener("click", close, { once: true });
  });
}

/**
 * 手动压缩当前会话上下文（compact）。
 *
 * 摘要在**后端落盘**（`summary` / `summary_upto`），之后每轮回灌直接读它 ——
 * 保证 prompt cache 前缀稳定。这里只负责触发 + 反馈。
 */
async function compactCurrent(): Promise<void> {
  if (busy) {
    showToast("对话进行中，先停止或等它结束再压缩", "error");
    return;
  }
  const btn = document.getElementById("sess-compact") as HTMLButtonElement | null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "压缩中…（要发一次模型请求）";
  }
  try {
    const r = await invoke<{ summarized: number; kept: number; summaryChars: number }>(
      "session_compact",
      { force: true },
    );
    showToast(
      `已压缩 ${r.summarized} 条消息 → ${r.summaryChars} 字摘要（保留最近 ${r.kept} 条原文）`,
      "ok",
    );
    // 会话变了（摘要写回），刷新列表元信息
    await refreshSessionList();
  } catch (e) {
    showToast(`压缩失败：${e}`, "error");
  } finally {
    renderBody();
  }
}

/** Rust 侧推上来的进度事件 */
interface ProgressEvent {
  kind:
    | "thinking"
    | "status"
    | "delta"
    | "toolCall"
    | "toolResult"
    | "toolError"
    | "answering"
    | "discardStream"
    | "steer";
  iteration?: number;
  /** delta / status */
  text?: string;
  /**
   * status：这是否是「值得留在时间线」的状态（限流退避重试）。
   *
   * 「正在请求 / 等首包」这类过场状态只刷标题，不进时间线 —— 每轮都有，
   * 记下来纯属刷屏。后端按同一条件决定要不要落盘。
   */
  retry?: boolean;
  reasoning?: string;
  /** tool* */
  name?: string;
  args?: string;
  preview?: string;
  message?: string;
  /** discardStream */
  reason?: string;
  /** steer：被消费掉的那条插话的 id */
  id?: string;
}

interface Candidate {
  id: string;
  content: string;
  source: string;
  created_at: string;
}

interface McpGroupView {
  name: string;
  summary: string;
  toolCount: number;
  tokens: number;
  active: boolean;
  hasDestructive: boolean;
}

interface McpStatus {
  connected: boolean;
  error: string | null;
  /** 当前网关地址；空串 = 未配置（没有默认值，不配就不启用） */
  url: string;
  /** 活跃工具 schema 的粗略 token 估算（仅展示，无加载上限） */
  activeTokens: number;
  groups: McpGroupView[];
}

/** 一条 MCP server 配置（对应 Rust `config::McpServerCfg`，camelCase 序列化） */
interface McpServerCfg {
  id: string;
  url: string;
  enabled: boolean;
  label: string;
}

// ---------------- 状态 ----------------

let mode: Mode = "orb";
let busy = false;
let thinking = false;

/**
 * 输入框草稿。
 *
 * 收起面板会把整个 DOM 换成悬浮球，输入框连同内容一起没了 ——
 * 「打了半句话、出去看一眼、回来」是日常动作，所以草稿要单独存一份。
 * 发送成功时才清空。
 */
let draftInput = "";

/**
 * 胶囊态五态机。优先级 **error > pending > mem > thinking > idle** ——
 * 出事了最要紧，其次是"被挡住等你批"的权限申请，再次是"可选、不催"的记忆候选，
 * 再其次正在干活。
 *
 * ⚠️ pending（权限）与 mem（记忆）**必须分开**：两者语义不同。
 *    权限申请 = 被挡住了、动不了，急；记忆候选 = 有东西待你顺手审，不急。
 *    早先两者合用一个金球，会让"待审记忆"也亮一把锁，误导用户以为助手被拦了。
 *
 * 多通道编码（色盲友好）：色相 + 猫姿态 + 角标三层，不裸靠颜色。
 *    绿=健康(睡猫)、靛蓝=最活跃(醒猫摇尾)、绿+抬头=记忆候选(peek)、
 *    金=等待(睡猫)、红=异常(睡猫)。
 */
type OrbState = "idle" | "thinking" | "pending" | "mem" | "error";

/** 有错误但用户还没看到。**只在胶囊态记** —— 面板态用户正看着对话流，不需要再提醒一遍 */
let unseenError = false;

/** 待审批的**记忆候选**数（> 0 → 绿球 + peek + 紫脑标）。来源：`mem_pending`，5s 轮询 */
let pendingMem = 0;

/** 待拍板的**权限申请**数（> 0 → 金球 + 琥珀锁）。来源：`pendingPerms`，事件驱动 */
let pendingPermCount = 0;

const ORB_TITLE: Record<OrbState, string> = {
  idle: "float-agent — 点击展开",
  thinking: "float-agent — 正在思考…",
  pending: "float-agent — 有权限申请等你批",
  mem: "float-agent — 有记忆等你审",
  error: "float-agent — 出错了，点开看看",
};

function currentOrbState(): OrbState {
  if (unseenError) return "error";
  if (pendingPermCount > 0) return "pending"; // 权限=被挡住，最急
  if (pendingMem > 0) return "mem"; // 记忆=可选，不催
  if (thinking) return "thinking";
  return "idle";
}

const app = document.getElementById("app")!;

let models: ModelView[] = [];
let selectedModel: string | null = null;
/** 项目索引（pmem.sqlite）；activeProjectId=null = 主对话 */
let projects: ProjectRow[] = [];
let activeProjectId: number | null = null;
let entries: ChatEntry[] = [];

/** 远端模型列表拉取结果（设置 › 模型） */
let remoteFetch: RemoteFetchState | null = null;
/** 添加/编辑表单状态（统一一张表，不再拆「拉取 / 手动」） */
let modelForm: ModelFormState = emptyModelForm();
let modelSearch = "";

function emptyModelForm(): ModelFormState {
  return {
    editId: null,
    name: "",
    id: "",
    url: "",
    apiKey: "",
    clearKey: false,
    tool: true,
    img: true,
    headersText: "",
    maxIn: "",
    maxOut: "",
    preset: "custom",
  };
}

/** 待发送的图片（data URL） */
let pendingImages: string[] = [];

/**
 * 内容区视图（两级导航）：
 *   settings ─┬─ models    模型管理
 *             ├─ mcp       MCP 工具组
 *             ├─ perm      文件权限
 *             ├─ mem       记忆
 *             └─ usage     Token 用量（按天 × 模型）
 */
type View =
  | "chat"
  | "settings"
  | "models"
  | "mcp"
  | "perm"
  | "cmdpolicy"
  | "mem"
  | "sessions"
  | "usage"
  | "search";

let view: View = "chat";

/** 权限规则（前端镜像，用于渲染） */
interface PermRule {
  prefix: string;
  access: "deny" | "read" | "readwrite" | "full";
  label: string;
}

/** 开机自启是否已开启（设置页显示用） */
let autostartOn = false;

/** 失焦自动收起（防挡屏幕）。启动时从 Rust 读，设置页可改。 */
let blurCollapse = true;

/** 面板刚展开的时间戳 —— 展开瞬间可能收到 blur，忽略掉防抖 */
let panelOpenedAt = 0;

/** 权限规则条数（设置入口页摘要用） */
let permCount = 0;
let searchInfo: SearchStatus | null = null;

/** 用于在重绘时解绑上一轮的 window 监听器 */
let detachDrag: (() => void) | null = null;

// ---------------- 工具 ----------------

function fileToDataUrl(file: File | Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result));
    r.onerror = () => reject(new Error("读取图片失败"));
    r.readAsDataURL(file);
  });
}

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/** token 数紧凑显示：1234 → 1.2k，12345 → 12k，1234567 → 1.2M */
function fmtTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) {
    const k = n / 1000;
    return (k >= 10 ? k.toFixed(0) : k.toFixed(1)) + "k";
  }
  return (n / 1_000_000).toFixed(1) + "M";
}

// ---------------- 复制到剪贴板 ----------------

/**
 * 复制文本。
 *
 * 为什么不用 `@tauri-apps/plugin-clipboard-manager`：没必要为一次复制加
 * 一个 Rust 插件依赖。WebView2 里 `navigator.clipboard` 可用（tauri.localhost
 * 算安全上下文）；万一被拒，退回 textarea + execCommand 的老办法。
 */
async function copyText(text: string): Promise<boolean> {
  if (!text) return false;
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.top = "-1000px";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      ta.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

/** 按钮短暂反馈「已复制」 */
function flashCopied(btn: HTMLElement, ok: boolean): void {
  const old = btn.textContent ?? "复制";
  btn.textContent = ok ? "✓ 已复制" : "复制失败";
  btn.classList.toggle("copied", ok);
  setTimeout(() => {
    btn.textContent = old.startsWith("✓") || old.includes("失败") ? "复制" : old;
    btn.classList.remove("copied");
  }, 1200);
}

/**
 * 复制按钮的事件委托。
 *
 * 为什么用委托而不是逐个绑定：消息区每次进度事件都会重绘（renderMessages
 * 重建 innerHTML），逐元素绑定要跟着重绑，容易漏。委托挂在 document 上，
 * 一次装好永久有效。
 */
function installCopyHandlers(): void {
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;

    // 代码块复制
    const codeBtn = t.closest<HTMLElement>(".code-copy");
    if (codeBtn) {
      e.stopPropagation();
      const pre = codeBtn.closest(".md-code-wrap")?.querySelector("pre");
      void copyText(pre?.textContent ?? "").then((ok) => flashCopied(codeBtn, ok));
      return;
    }

    // 整条消息复制
    const msgBtn = t.closest<HTMLElement>(".msg-copy");
    if (msgBtn) {
      e.stopPropagation();
      const idx = Number(msgBtn.dataset.idx);
      const ent = entries[idx];
      void copyText(ent?.text ?? "").then((ok) => flashCopied(msgBtn, ok));
    }
  });
}

/** 消息区右键 → 复制菜单（元素与绑定都在这里管） */
let copyMenuEl: HTMLElement | null = null;

function closeCopyMenu(): void {
  copyMenuEl?.remove();
  copyMenuEl = null;
}

function showCopyMenu(x: number, y: number, bodyEl: HTMLElement): void {
  closeCopyMenu();
  const sel = (window.getSelection()?.toString() ?? "").trim();
  const whole = bodyEl.textContent ?? "";

  const el = document.createElement("div");
  el.className = "copy-menu";
  el.innerHTML =
    `<button type="button" data-k="sel"${sel ? "" : " disabled"}>复制选中${
      sel ? `（${sel.length} 字）` : "（未选中）"
    }</button>` + `<button type="button" data-k="all">复制整条消息</button>`;
  document.body.appendChild(el);

  // 贴边修正：面板只有 420x600，菜单不能跑出可视区
  const w = el.offsetWidth;
  const h = el.offsetHeight;
  el.style.left = `${Math.max(4, Math.min(x, window.innerWidth - w - 4))}px`;
  el.style.top = `${Math.max(4, Math.min(y, window.innerHeight - h - 4))}px`;
  copyMenuEl = el;

  el.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
    b.addEventListener("click", () => {
      const txt = b.dataset.k === "sel" ? sel : whole;
      void copyText(txt);
      closeCopyMenu();
    }),
  );
}

// ---------------- 拖动 / 点击二合一 ----------------

/**
 * 为什么不用 `data-tauri-drag-region`：
 *   那个属性会让 mousedown 立即进入拖动，导致 click 被吞掉，
 *   无法同时支持"点击展开"和"拖动移动"。
 */
function attachDrag(el: HTMLElement, onClick: () => void): void {
  detachDrag?.();

  const ac = new AbortController();
  detachDrag = () => ac.abort();

  let downX = 0;
  let downY = 0;
  let isDown = false;

  el.addEventListener(
    "mousedown",
    (e) => {
      if (e.button !== 0) return;
      // 表单元素 / 消息区上不触发拖动
      const t = e.target as HTMLElement;
      if (t.closest("input, textarea, select, button, .msg-body, .thumbs")) return;
      isDown = true;
      downX = e.screenX;
      downY = e.screenY;
    },
    { signal: ac.signal },
  );

  window.addEventListener(
    "mousemove",
    (e) => {
      if (!isDown) return;
      const moved =
        Math.abs(e.screenX - downX) + Math.abs(e.screenY - downY) > DRAG_THRESHOLD;
      if (moved) {
        isDown = false;
        void getCurrentWindow().startDragging();
      }
    },
    { signal: ac.signal },
  );

  window.addEventListener(
    "mouseup",
    () => {
      if (isDown) onClick();
      isDown = false;
    },
    { signal: ac.signal },
  );
}

// ---------------- 渲染：胶囊态 ----------------

function renderOrb(): void {
  // 猫球六帧，同一时刻只露一帧，由 .orb--<state> 决定：
  //   idle    绿壳睡猫  待命（静态）              ← 常驻态，99% 的时间是这个
  //   awake   靛蓝壳醒猫 工作中（A/B 交替=摇尾）
  //   pending 金壳睡猫  等待权限审批（呼吸放慢到 5s）
  //   mem     绿壳醒猫  记忆候选（periodic peek 抬头）
  //   error   红壳睡猫  错误（1.8s 缓慢明暗）
  // 素材是圆形透明 PNG（public/orb/），窗口 transparent 圆外自然露出桌面。
  //
  // 帧序 = 层级：idle/pending/error 三张睡猫在最底，awake 在中间（待命→工作是
  // "醒猫淡入盖上来"），mem(peek) 放最上（绿球盖住底下的睡猫/醒猫）。
  // 角标放最后，保证盖在球上面。
  const s = currentOrbState();
  app.innerHTML = `
    <div class="orb orb--${s}" id="orb" title="${ORB_TITLE[s]}">
      <img class="orb-frame orb-frame-idle"    src="orb/orb-state-green.png"  alt="" draggable="false" />
      <img class="orb-frame orb-frame-pending" src="orb/orb-state-yellow.png" alt="" draggable="false" />
      <img class="orb-frame orb-frame-error"   src="orb/orb-state-red.png"    alt="" draggable="false" />
      <img class="orb-frame orb-frame-awake-a" src="orb/orb-awake-a.png" alt="" draggable="false" />
      <img class="orb-frame orb-frame-awake-b" src="orb/orb-awake-b.png" alt="" draggable="false" />
      <img class="orb-frame orb-frame-mem"     src="orb/orb-peek.png"    alt="" draggable="false" />
      <img class="orb-badge orb-badge-perm"  src="orb/badge-perm.png"  alt="" draggable="false" />
      <img class="orb-badge orb-badge-error" src="orb/badge-error.png" alt="" draggable="false" />
      <img class="orb-badge orb-badge-mem"   src="orb/badge-mem.png"   alt="" draggable="false" />
    </div>
  `;
  const orb = document.getElementById("orb")!;
  attachDrag(orb, () => void expand());
}

/**
 * 把当前状态同步到**已存在**的球元素上（只换 class，不重建 DOM）——
 * 重建 DOM 会让五张图重新加载，肉眼能看到一次闪。
 *
 * 球不在场（面板态 / 菜单态）时静默跳过；下次 `renderOrb()` 会带上正确状态。
 */
function applyOrbState(): void {
  const el = document.getElementById("orb");
  if (!el) return;
  const s = currentOrbState();
  el.classList.remove("orb--idle", "orb--thinking", "orb--pending", "orb--mem", "orb--error");
  el.classList.add(`orb--${s}`);
  el.title = ORB_TITLE[s];
}

// ---------------- 图片缩略图缓存 ----------------

/**
 * 缩略图缓存：`图片路径 → data URL`。
 *
 * 历史消息里的图片存的是**文件路径**（图片本体在 agent-data/images/），
 * 前端渲染时通过 `image_thumb` 命令拿 320px 缩略图。缓存避免每次重绘都走一次 IPC。
 */
const thumbCache = new Map<string, string>();
const thumbInflight = new Set<string>();

/**
 * 同步取缓存；没有则异步加载，加载完**只替换那一个缩略图节点**。
 *
 * ⚠️ 两个坑，都踩过：
 * 1. 命中判定必须用 `has()` 而不是 `if (hit)` —— 加载失败时缓存里存的是**空串**，
 *    空串是 falsy，用值判等会让"读不出来"的图每次都重新请求，而请求失败又会
 *    触发重绘 → **无限重绘死循环**（表现就是悬停按钮疯狂闪烁、CPU 起飞）。
 * 2. 加载完不能整块 `paintMessages()`：那会重建全部消息节点，把悬停中的
 *    `.msg-copy` / `.msg-fork` 的 opacity 过渡反复重放（也是闪烁）。
 *    只换这一个 `.thumb` 就够了。
 */
function thumbOf(path: string): string | null {
  if (thumbCache.has(path)) return thumbCache.get(path)!; // "" = 已判定读不出来
  if (!thumbInflight.has(path)) {
    thumbInflight.add(path);
    void invoke<string>("image_thumb", { path, maxEdge: 320 })
      .then((url) => {
        thumbCache.set(path, url);
      })
      .catch(() => {
        // 读不了（文件被删/格式不支持）→ 记空串，渲染成占位，别反复重试
        thumbCache.set(path, "");
      })
      .finally(() => {
        thumbInflight.delete(path);
        patchThumb(path);
      });
  }
  return null;
}

/** 缩略图就绪/失败后，就地替换掉对应的占位节点（不重绘消息区） */
function patchThumb(path: string): void {
  const cached = thumbCache.get(path);
  if (cached === undefined) return;
  // 用 title 比对而不是属性选择器：路径里有反斜杠/引号，选择器转义太容易出错
  document.querySelectorAll<HTMLElement>(".thumb.thumb-loading").forEach((el) => {
    if (el.getAttribute("title") !== path) return;
    el.classList.remove("thumb-loading");
    if (cached) {
      el.innerHTML = `<img src="${cached}" alt="附图" />`;
    } else {
      el.classList.add("thumb-bad");
      el.textContent = "图";
    }
  });
}


// ---------------- 渲染：面板态 ----------------

/**
 * 时间线：把一轮里「想了什么 / 做了什么」**按发生顺序**渲染出来。
 *
 * ## 为什么要按顺序（2026-09-22 用户拍板）
 *
 * 以前是「一个思考折叠块 + 一个工具折叠行」，思考必然挤成一坨、工具另起一处，
 * 看不出"这一步的思考是为了调哪个工具"。现在条目自带顺序，渲染出来就是
 * `💭 思考 → 🔧 调工具 → ✅ 返回 → 💭 思考 → …` 的交错节奏。
 *
 * ## 条目种类
 *
 * | kind | 来源 | 显示 |
 * |---|---|---|
 * | `reasoning` | agent.rs 每轮思维链 | 可折叠的 `💭 思考过程（N 字）` |
 * | `tool_call` | 工具调用 | `🔧 调用 <code>名字</code> <i>参数摘要</i>` |
 * | `tool_result` | 工具返回 | `✅ <code>名字</code> 返回 <i>首行</i>` |
 * | `status` | 限流退避等链路状态 | `⏳ …`（灰色，只作说明） |
 * | `tool_error` / `error` | 工具失败 / 本轮失败 | `⚠️ / ✕ …`（红黄） |
 * | `omitted` | 后端省略了太早的步骤 | 灰色说明行 |
 * | `assistant` | 最终答案（正文另有渲染） | 跳过 |
 *
 * ## 兼容老数据
 *
 * - 折叠记录（落盘折叠机制时代留下的单条 `tool_call`，`detail` 以 `[已执行` 开头）
 *   → 整行显示，**不能**当 JSON 解析（会显示成乱码）
 * - 只有 `tool_call`、没有时间线条目的老消息 → 回退成原来的「调用了 N 个工具」
 *
 * @param live 运行中的气泡：思考块**默认展开**（用户就想看它现在在想什么）
 */
function renderTimeline(items: AgentStep[] | undefined, live: boolean): string {
  if (!items || items.length === 0) return "";

  // 折叠记录：整轮已被压成一行摘要（老会话数据）
  const calls = items.filter((s) => s.kind === "tool_call");
  const folded =
    items.length === 1 &&
    calls.length === 1 &&
    calls[0].detail.trimStart().startsWith("[已执行");
  if (folded) {
    return `<div class="steps-folded">${esc(calls[0].detail)}</div>`;
  }

  const body = items.map((s, i) => renderItem(s, live, i)).join("");
  if (!body) return "";
  // 跑完（历史）：包进默认折叠的「过程」，中间细节不再抢最终答案；
  // 中途（live）保持摊开 —— 用户要盯着进度。
  if (!live) {
    return `<details class="run-process" data-open-key="proc-${esc((items[0]?.detail ?? "").slice(0, 12))}-${items.length}">
      <summary>过程 · ${items.length} 步</summary>
      <div class="run-timeline tl-hist">${body}</div>
    </details>`;
  }
  return `<div class="run-timeline${live ? "" : " tl-hist"}">${body}</div>`;
}

/** 单条时间线条目 → HTML（`renderTimeline` 与 `patchTimeline` 的共同契约） */
function renderItem(s: AgentStep, live: boolean, idx = -1): string {
  // `data-i` 是增量 patch 的锚点（知道"第几条已画"），全量渲染时也要带上，
  // 否则一次全量重绘之后的流式更新会找不到节点、思考块就不再增长
  const di = idx >= 0 ? ` data-i="${idx}"` : "";
  switch (s.kind) {
    case "reasoning":
      if (!s.detail.trim()) return "";
      return `<details class="run-reason tl-reason"${di}${live ? " open" : ""} data-open-key="r-${esc(s.name || "")}-${idx}">
        <summary>💭 思考过程<span class="run-reason-len">（${s.detail.length} 字）</span></summary>
        <pre class="run-reason-pre">${esc(s.detail)}</pre>
      </details>`;

    case "tool_call":
      return `<div class="tl-line"${di}>🔧 调用 <code>${esc(s.name || "?")}</code>${
        s.detail ? ` <i>${esc(summarizeArgs(s.detail))}</i>` : ""
      }</div>`;

    case "tool_result": {
      const full = s.detail || "";
      const preview = full.split("\n")[0].slice(0, 70);
      const truncated = full.length > 200;
      const bodyFull = full.length > 8 * 1024 ? full.slice(0, 8 * 1024) + "\n…[截断]" : full;
      if (!truncated) {
        return `<div class="tl-line"${di}>✅ <code>${esc(s.name || "?")}</code> 返回 <i>${esc(
          preview,
        )}</i></div>`;
      }
      return `<details class="tl-tool-res"${di}${live ? "" : ""} data-open-key="tr-${esc(s.name || "")}-${idx}">
        <summary>✅ <code>${esc(s.name || "?")}</code> 返回 <i>${esc(preview)}</i></summary>
        <pre class="tl-tool-pre">${esc(bodyFull)}</pre>
      </details>`;
    }

    case "tool_error":
    case "error":
      return `<div class="tl-line ${s.kind === "error" ? "tl-error" : "tl-warn"}"${di}>${
        s.kind === "error" ? "✕" : "⚠️"
      } ${s.name ? `<code>${esc(s.name)}</code> ` : ""}${esc(s.detail)}</div>`;

    case "status":
    case "omitted":
      return `<div class="tl-line tl-status"${di}>${s.kind === "omitted" ? "" : "⏳ "}${esc(
        s.detail,
      )}</div>`;

    case "steer":
      return `<div class="tl-steer"${di}>💬 插话${s.name ? ` <i>${esc(s.name)}</i>` : ""}：${esc(
        s.detail,
      )}</div>`;

    case "text":
    case "stream":
      return `<div class="tl-text"${di}>${
        s.name ? `<span class="tl-text-tag">${esc(s.name)}</span>` : ""
      }${esc(s.detail).replace(/\n/g, "<br>")}</div>`;

    // 最终答案的正文另有渲染（`e.text` → markdown），这里跳过免得重复
    case "assistant":
    default:
      return "";
  }
}

/**
 * 工具参数摘要：优先取最有信息量的字段，取不到就原样截断。
 *
 * ⚠️ 落盘时 detail 可能被后端截断（不再保证是合法 JSON），所以解析失败要
 * 安静地退回原串 —— 中文路径 + 截断很容易把 JSON 切断。
 */
function summarizeArgs(detail: string): string {
  try {
    const o = JSON.parse(detail);
    const picked = o.path || o.file_path || o.pattern || o.query || o.command || o.url;
    if (typeof picked === "string" && picked) return picked.slice(0, 90);
    if (o && typeof o === "object" && Object.keys(o).length) return detail.slice(0, 90);
    return "—";
  } catch {
    return detail.slice(0, 90);
  }
}

/**
 * 图片缩略图。`src` 有两种形态：
 * - `data:...` —— 输入框里刚贴/刚截的图，直接内联
 * - 文件路径 —— 历史消息里的图（存盘后的路径），走缩略图缓存异步加载
 */
function renderThumb(src: string, idx: number, removable: boolean): string {
  if (src.startsWith("data:")) {
    return `
    <div class="thumb">
      <img src="${src}" alt="附图" />
      ${removable ? `<button class="thumb-x" data-idx="${idx}" title="移除">×</button>` : ""}
    </div>`;
  }

  const cached = thumbOf(src);
  if (cached) {
    return `
    <div class="thumb" title="${esc(src)}">
      <img src="${cached}" alt="附图" />
      ${removable ? `<button class="thumb-x" data-idx="${idx}" title="移除">×</button>` : ""}
    </div>`;
  }
  if (cached === "") {
    // 读不出来（文件没了 / 格式不支持）
    return `<div class="thumb thumb-bad" title="${esc(src)}">图</div>`;
  }
  // 首次渲染：先占位，缩略图加载完会自动重绘
  return `<div class="thumb thumb-loading" title="${esc(src)}">…</div>`;
}

/** 权限申请卡永远挂在消息流**最后** —— 它是"此刻要你拍板"的东西 */
function renderMessages(): string {
  return renderMessagesInner() + `<div class="perm-cards">${renderPermCards()}</div>`;
}

/**
 * 重绘消息区并**重新绑定**卡片按钮。
 *
 * ⚠️ 这里**不用** document 级事件委托：实测委托对卡片按钮不生效
 *    （点了完全没反应，只有键盘 Enter/Esc 有效）。项目里其余按钮
 *    也全是"渲染后直接绑"的写法，保持一致。
 *
 * 所有 `innerHTML = renderMessages()` 的位置都必须改走本函数 ——
 * 直接把字符串塞进 innerHTML 的话，卡片渲染出来了但按钮是死的。
 */
function paintMessages(el: HTMLElement): void {
  // 重绘前收集用户/系统已展开的 details，重建后写回 —— 否则权限卡刷新、
  // 记忆提示等整块 innerHTML 会把展开状态冲掉（「展开一下马上没了」）。
  const openKeys = new Set<string>();
  el.querySelectorAll<HTMLDetailsElement>("details[open]").forEach((d) => {
    const key = d.dataset.openKey ?? d.dataset.i ?? d.querySelector("summary")?.textContent?.slice(0, 24) ?? "";
    if (key) openKeys.add(key);
  });
  el.innerHTML = renderMessages();
  el.querySelectorAll<HTMLDetailsElement>("details").forEach((d) => {
    const key = d.dataset.openKey ?? d.dataset.i ?? d.querySelector("summary")?.textContent?.slice(0, 24) ?? "";
    if (key && openKeys.has(key)) {
      d.open = true;
      d.dataset.userOpen = "1";
    }
    // 记录用户手动展开（E5/E7）
    d.addEventListener("toggle", () => {
      if (d.open) d.dataset.userOpen = "1";
      else delete d.dataset.userOpen;
    });
  });
  el.querySelectorAll<HTMLButtonElement>("button[data-continue]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const ta = document.querySelector<HTMLTextAreaElement>("#prompt-input");
      if (ta) {
        ta.value = "继续";
        draftInput = "继续";
      }
      void send();
    });
  });
  bindPermButtons(el);
}

/**
 * 流式期间**只就地更新「进行中」那一个气泡**，不重建整块消息区。
 *
 * 为什么必须这么做：
 * 1. **复制按钮闪烁** —— `.msg-copy` 默认 `opacity:0` + `transition:0.12s`，靠
 *    `.msg:hover` 显现。整块 innerHTML 重建会把悬停中的按钮节点销毁重建，
 *    过渡被反复重放，视觉上就是闪。只改 running 盒子后，其它消息的节点
 *    一次都不会被替换，闪烁消失。
 * 2. **O(n²)** —— `renderMessagesInner` 会对每条历史 assistant 消息重跑
 *    `mdToHtml`。一条 60 条消息的会话、每个 chunk 就是 60 次 markdown 解析。
 *    这里只做一次转义，代价与消息条数无关。
 *
 * ⚠️ 输出必须与 `renderMessagesInner` 的 running 分支**逐字段一致**，
 *    否则「全量重绘」和「增量 patch」两条路径会画出不同的样子。
 */
function patchRunning(el: HTMLElement): void {
  const run = entries.find((e) => e.role === "running");
  const box = el.querySelector<HTMLElement>(".msg.running");
  // 气泡不在（还没画 / 刚被移除）→ 退回全量重绘，保证最终一致
  if (!run || !box) {
    paintMessages(el);
    return;
  }

  const head = box.querySelector<HTMLElement>(".run-head-text");
  if (head && head.textContent !== run.text) head.textContent = run.text;

  // 时间线：只 append 新增条目，最后一个思考条目原地更新（它正在长）
  const tl = box.querySelector<HTMLElement>(".run-timeline");
  if (tl) patchTimeline(tl, run.items ?? []);

  const stream = box.querySelector<HTMLElement>(".run-stream");
  if (stream) {
    const t = run.streamText ?? "";
    const html = t ? esc(t).replace(/\n/g, "<br>") : "";
    if (stream.innerHTML !== html) stream.innerHTML = html;
    stream.hidden = !t;
    stream.classList.toggle("discarded", !!run.streamDiscarded);
  }

  const note = box.querySelector<HTMLElement>(".run-note");
  if (note) note.hidden = !run.streamDiscarded;
}

/**
 * 时间线的**增量** patch：只 append 新条目，正在增长的那条思考原地更新。
 *
 * 为什么要增量而不是每次重建：思考块是 `<details>`，用户可能正展开着、正往回
 * 滚着看；每帧 innerHTML 重建会把展开状态和滚动位置一并重置。所以：
 * - `data-n` 记"已画几条"，只 append 差集
 * - `data-i` 锚定条目，最后一条若是思考条目 → 只换 `<pre>` 文本与字数
 *
 * ⚠️ 契约：产出必须与 `renderTimeline(items, true)` 完全一致（共用 `renderItem`）。
 *    唯一例外是思考块的 `open`：全量渲染时默认展开，而 patch **不碰 `open`**
 *    —— 用户手动收起之后不该被下一帧顶开。
 */
function patchTimeline(tl: HTMLElement, items: AgentStep[]): void {
  const n = Number(tl.dataset.n ?? "0");

  // 状态回退（条数变少 / 被清空 / 出错）→ 老老实实全量重画这一段
  if (!Number.isFinite(n) || n < 0 || n > items.length) {
    tl.innerHTML = items.map((s, i) => renderItem(s, true, i)).join("");
    tl.dataset.n = String(items.length);
    return;
  }

  // ① 最后一条已画条目：思考条目正在流式增长 → 原地更新文本
  if (n > 0) {
    const last = items[n - 1];
    const node = tl.querySelector<HTMLElement>(`[data-i="${n - 1}"]`);
    if (last.kind === "reasoning" && node) {
      const pre = node.querySelector<HTMLPreElement>(".run-reason-pre");
      const len = node.querySelector<HTMLElement>(".run-reason-len");
      if (pre && pre.textContent !== last.detail) {
        // 用户停在思考块底部时才自动跟随，往回翻就不打扰
        const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24;
        pre.textContent = last.detail;
        if (len) len.textContent = `（${last.detail.length} 字）`;
        if (atBottom) pre.scrollTop = pre.scrollHeight;
      }
    } else if (!node || node.outerHTML !== renderItem(last, true, n - 1)) {
      // 非思考条目照理不会再变；真变了就整条换掉（含首次由空串变成有内容的情况）
      const fresh = nodeFromHtml(renderItem(last, true, n - 1));
      if (fresh) {
        if (node) node.replaceWith(fresh);
        else tl.appendChild(fresh);
      }
    }
  }

  // ② 新增条目：纯 append
  for (let i = n; i < items.length; i++) {
    const node = nodeFromHtml(renderItem(items[i], true, i));
    if (node) tl.appendChild(node);
  }
  tl.dataset.n = String(items.length);
}

/** HTML 片段 → 第一个元素节点（取不到就返回 null，调用方跳过即可） */
function nodeFromHtml(html: string): HTMLElement | null {
  if (!html) return null;
  const tmp = document.createElement("div");
  tmp.innerHTML = html;
  return tmp.firstElementChild as HTMLElement | null;
}

/**
 * 一帧最多画一次。
 *
 * 流式 chunk 来得比帧还快（尤其 fast 模型），每个 chunk 都 patch 一次纯属浪费；
 * 用 rAF 合并后，视觉上仍是「逐字增长」，GPU 那边少干很多活。
 */
let runPaintScheduled = false;

function scheduleRunPaint(): void {
  if (runPaintScheduled) return;
  runPaintScheduled = true;
  requestAnimationFrame(() => {
    runPaintScheduled = false;
    if (view !== "chat") return;
    const b = document.getElementById("panel-body");
    if (!b) return;
    const atBottom = b.scrollHeight - b.scrollTop - b.clientHeight < 60;
    // 用户主动展开过过程块 → 禁止误滚走（E7）
    const userExpanded = b.querySelector("details[data-user-open='1']");
    patchRunning(b);
    if (userExpanded) {
      // 重建后按 data-open-key 恢复；若仍展开，别滚
      return;
    }
    // 用户没往上翻时才自动跟到底部
    if (atBottom) b.scrollTop = b.scrollHeight;
  });
}

/** 绑定申请卡上的按钮（渲染后调用；每次重绘都会重新绑一批新的） */
function bindPermButtons(root: ParentNode): void {
  root.querySelectorAll<HTMLButtonElement>(".perm-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const card = btn.closest<HTMLElement>(".perm-card");
      const id = card?.dataset.perm;
      if (!id) return;
      const act = btn.dataset.act ?? "deny";

      // ✎ → 才切到"写理由"形态。理由是可选的，所以默认那颗「拒绝」不拦人。
      if (act === "deny-why") {
        denyReasonFor = id;
        refreshPermCards();
        // ⚠️ refreshPermCards 重建了 DOM，上一步的 card 已经不在文档里 —— 必须重新取
        document
          .querySelector<HTMLInputElement>(`.perm-card[data-perm="${id}"] .perm-reason`)
          ?.focus();
        return;
      }
      if (act === "deny-cancel") {
        denyReasonFor = null;
        refreshPermCards();
        return;
      }
      if (act === "deny-confirm") {
        const why = card.querySelector<HTMLInputElement>(".perm-reason")?.value.trim() ?? "";
        denyReasonFor = null;
        void decidePerm(id, "deny", why);
        return;
      }

      // 批准类：立刻禁用本卡按钮防连点（后端也会拒重复决定，但别让用户看出来）
      card
        .querySelectorAll<HTMLButtonElement>(".perm-btn")
        .forEach((b) => (b.disabled = true));
      void decidePerm(id, act);
    });
  });

  // 理由输入框：Enter 确认 / Esc 取消。
  // 全局键盘在有输入焦点时**故意不接管**，所以这两个键必须在这里接。
  root.querySelectorAll<HTMLInputElement>(".perm-reason").forEach((inp) => {
    inp.addEventListener("keydown", (ev) => {
      const id = inp.closest<HTMLElement>(".perm-card")?.dataset.perm;
      if (!id) return;
      if (ev.key === "Enter") {
        ev.preventDefault();
        denyReasonFor = null;
        void decidePerm(id, "deny", inp.value.trim());
      } else if (ev.key === "Escape") {
        ev.preventDefault();
        denyReasonFor = null;
        refreshPermCards();
      }
    });
  });
}

function renderMessagesInner(): string {
  if (entries.length === 0) {
    // 未初始化时，空态要给出**可操作的下一步**，而不是通用引导语。
    // 用户不知道"agent-data 不存在"是什么意思，得直接把话说明白。
    if (needsInit) {
      return `<div class="empty init-prompt">
        <div class="init-title">尚未初始化</div>
        <div class="init-body">
          数据目录 <code>agent-data/</code> 还不存在 ——<br>
          没有人格文件、行为红线、用户画像、长期记忆。
        </div>
        <button class="init-go" id="init-go">建出目录结构</button>
        <div class="init-hint">
          点一下就行，这一步**不需要模型**。<br>
          建完再去 <b>设置</b> 里添加一个模型，就能开始对话了。
        </div>
      </div>`;
    }
    // 目录有了但没模型 —— 聊天发不出去，这时唯一该做的是去加模型
    if (!hasModels) {
      return `<div class="empty init-prompt">
        <div class="init-title">还没有模型</div>
        <div class="init-body">
          <code>agent-data/</code> 已经建好了，但里面没有任何模型配置 ——<br>
          没有模型，对话发不出去。
        </div>
        <button class="init-go" id="init-go">去设置里添加模型</button>
        <div class="init-hint">
          需要三样东西：<b>id</b>（自己起名）、<b>url</b>（OpenAI 兼容端点）、<b>API key</b>。
        </div>
      </div>`;
    }
    return `<div class="empty">
      问点什么，或者：<br>
      · <b>Ctrl+V</b> 粘贴截图<br>
      · 点 <b>截</b> 抓当前屏幕<br>
      · 直接把图片拖进来
    </div>`;
  }
  return entries
    .map((e, idx) => {
      // 进行中的进度气泡
      //
      // ⚠️ 结构与 class 是 `patchRunning()` 的契约，两边必须逐字段一致：
      //    流式期间只 patch 这一个盒子，不再整块重建消息区（否则复制按钮会闪）。
      if (e.role === "running") {
        const streamText = e.streamText ?? "";

        return `
        <div class="msg running">
          <div class="run-head"><span class="spinner"></span><span class="run-head-text">${esc(
            e.text,
          )}</span></div>
          <div class="run-timeline" data-n="${(e.items ?? []).length}">${(e.items ?? [])
            .map((s, i) => renderItem(s, true, i))
            .join("")}</div>
          <div class="run-stream${e.streamDiscarded ? " discarded" : ""}"${
            streamText ? "" : " hidden"
          }>${esc(streamText).replace(/\n/g, "<br>")}</div>
          <div class="run-note"${e.streamDiscarded ? "" : " hidden"}>已改调工具 · 仍可看</div>
        </div>`;
      }

      const cls = `${e.role}${e.queued ? " queued" : ""}`;
      // assistant 的回复按 markdown 渲染（mdToHtml 内部已做 HTML 转义，可安全直插）
      const isMd = e.role === "assistant";
      const body = isMd
        ? mdToHtml(e.text)
        : e.text
          ? esc(e.text).replace(/\n/g, "<br>")
          : "";
      const imgs =
        e.images && e.images.length
          ? `<div class="thumbs">${e.images.map((s, i) => renderThumb(s, i, false)).join("")}</div>`
          : "";
      // 历史里的思考过程与步骤：**默认收起**（回看时才展开，别让思维链淹没正文）。
      // 流式进行中的那个反而默认展开 —— 那时用户就想看它在想什么。
      //
      // 新数据：`items`（磁盘上的 steps）就是完整时间线，思考与工具按序交错。
      // 老数据：只有单串 `reasoning` + 折叠行 → 回退成"一个思考块 + 一行摘要"。
      const hasReasonItem = (e.items ?? []).some((s) => s.kind === "reasoning");
      const tl = renderTimeline(e.items, false);
      const legacyReason =
        !hasReasonItem && e.reasoning
          ? `<details class="run-reason hist-reason"><summary>💭 思考过程<span class="run-reason-len">（${
              e.reasoning.length
            } 字）</span></summary><pre class="run-reason-pre">${esc(e.reasoning)}</pre></details>`
          : "";
      const reason = `${legacyReason}${tl}`;
      /** 本轮是否以「失败」收尾（决定要不要显示"已中断"角标） */
      const failed = (e.items ?? []).some((s) => s.kind === "error");
      // 本轮 token 用量角标（只有拿到 usage 的 assistant 消息才有）
      const tokBadge =
        e.role === "assistant" && e.usage
          ? `<span class="tok-badge" title="输入 ${e.usage.prompt} · 输出 ${
              e.usage.completion
            }${e.usage.reasoning ? ` · 思考 ${e.usage.reasoning}` : ""}${
              e.usage.cacheHit ? ` · 缓存命中 ${e.usage.cacheHit}` : ""
            }${e.usage.cacheWrite ? ` · 缓存写入 ${e.usage.cacheWrite}` : ""} tokens${
              e.model ? ` · ${esc(e.model)}` : ""
            }">${fmtTokens(e.usage.total)} tok</span>`
          : "";
      return `
      <div class="msg ${cls}"${e.steerId ? ` data-steer="${e.steerId}"` : ""}>
        ${imgs}
        ${reason}
        ${body ? `<div class="msg-body${isMd ? " md" : ""}">${body}</div>` : ""}
        ${
          e.continueRun
            ? `<div class="perm-actions" style="margin-top:6px"><button type="button" class="perm-btn ok" data-continue="1">继续</button></div>`
            : ""
        }
        ${
          e.text || e.storedIdx !== undefined
            ? `<div class="msg-foot">${tokBadge}${renderForkBtn(e)}${renderRegenBtn(e)}${renderDelBtn(e)}${
                e.text
                  ? `<button type="button" class="msg-copy" data-idx="${idx}" title="复制这条消息">复制</button>`
                  : ""
              }</div>`
            : ""
        }
        ${
          // 「已中断」角标：出错的那轮时间线尾部已经有红色错误条目，
          // 再挂一个"（已中断）"是噪音，所以只在非失败轮显示
          e.interrupted && !failed
            ? `<div class="interrupted-tag" title="本轮被「停止」打断，以上是已产出的部分内容">（已中断）</div>`
            : ""
        }
        ${
          e.queued
            ? `<div class="queued-tag">${
                e.steerQueuePos ? `已排队（第 ${e.steerQueuePos} 位）` : "已排队"
              }，当前轮结束后送达</div>`
            : ""
        }
      </div>`;
    })
    .join("");
}

// ---------------- 申请权限：卡片 ----------------

/** 待用户拍板的申请。**事件驱动**，不轮询 —— 后端有申请就推过来 */
let pendingPerms: PendingPerm[] = [];

/**
 * 正在填写拒绝理由的那张卡（id）。
 *
 * 点「拒绝」不立刻提交，而是切到"写理由"形态 —— 理由会原样回给模型，
 * 它才知道该怎么调整（换更窄的路径 / 换做法），而不是盲目重试或干脆放弃。
 * 不想写就直接确认，留空也行。
 */
let denyReasonFor: string | null = null;

/**
 * Shift+Enter「本轮不再问」：置位后本轮后续申请**自动拒绝**。
 *
 * ⚠️ 这里是 **fail-closed** 解读：免问 = 不再弹卡打扰，不等于免检。
 *    若你要的是"本轮后续全部自动批准"，改这一个 `decidePerm(p.id, "deny")` 即可。
 */
let permMuteThisTurn = false;

const PERM_ACCESS_LABEL: Record<string, string> = {
  read: "只读",
  readwrite: "改（读写）",
  full: "完全控制（含删除）",
};

const PERM_TIER_LABEL: Record<string, string> = {
  once: "仅这一次",
  turn: "本轮对话",
  task: "整个任务",
};

/** 命令授权的档位展示 —— 命令侧只有两种寿命：「仅这一次」与「永久」 */
const CMD_TIER_LABEL: Record<string, string> = {
  once: "仅这一次",
  turn: "本轮",
  task: "永久",
};

/** 取路径最后一段（卡片按钮上要短，不能糊一长串路径） */
function leafName(p: string): string {
  const s = p.replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("\\"), s.lastIndexOf("/"));
  return i >= 0 ? s.slice(i + 1) : s;
}

const PERM_RISK_LABEL: Record<string, string> = {
  low: "低风险",
  medium: "中风险",
  high: "高风险",
};

function renderPermCards(): string {
  return pendingPerms
    .map((p) =>
      p.kind === "command"
        ? renderCommandPermCard(p)
        : p.kind === "mcp"
          ? renderMcpPermCard(p)
          : renderFilePermCard(p),
    )
    .join("");
}

/** 文件级申请卡（路径维度） */
function renderFilePermCard(p: PendingPerm): string {
  const deadline = p.createdAt + p.timeoutMs;
  const rest = Math.max(0, deadline - Date.now());
  const mm = Math.floor(rest / 60000);
  const ss = String(Math.floor((rest % 60000) / 1000)).padStart(2, "0");

  const leaf = leafName(p.deniedPath ?? p.path);
  const target = leafName(p.path) || p.path;

  // 范围对比：比被拒路径宽了就**明说** ——
  // 用户点「批准」的那一刻必须知道批的是多大一片，否则等于把升权决策悄悄交给模型。
  const widen =
    p.widened && p.widenLevels
      ? `<div class="perm-widen">↑ 比你被拒的 <code>${esc(leaf)}</code> 宽了 ${p.widenLevels} 层</div>`
      : "";

  // 把模型的宽范围**收窄**回实际被拒的那条路径
  const narrowBtn =
    p.widened && p.deniedPath
      ? `<button type="button" class="perm-btn narrow" data-act="narrow">仅批准 ${esc(leaf)}</button>`
      : "";

  // 「拒绝」是**一步到位**的（大多数情况不想写字）。
  // 想给理由才点旁边那颗 ✎ —— 理由是可选的，不能逼着每次拒绝都多走一步。
  const actions =
    denyReasonFor === p.id
      ? `
    <div class="perm-actions">
      <input class="perm-reason" type="text"
             placeholder="拒绝理由（可留空）—— 写了就告诉 AI 它该怎么改" />
      <button type="button" class="perm-btn no" data-act="deny-confirm">确认拒绝</button>
      <button type="button" class="perm-btn" data-act="deny-cancel">取消</button>
    </div>
    <div class="perm-hint">Enter 确认 · Esc 取消 · 留空也能确认</div>`
      : `
    <div class="perm-actions">
      ${narrowBtn}
      <button type="button" class="perm-btn ok" data-act="approve">批准 ${esc(target)}</button>
      <button type="button" class="perm-btn no" data-act="deny">拒绝</button>
      <button type="button" class="perm-btn why" data-act="deny-why"
              title="拒绝并说明理由（会告诉 AI 它下次该怎么改）">✎</button>
    </div>
    <div class="perm-hint">Enter 批准 · Esc 拒绝 · Shift+Enter 本轮不再问 · ✎ 拒绝并说明</div>`;

  return `
  <div class="perm-card" data-perm="${esc(p.id)}">
    <div class="perm-head">
      <span class="perm-title">🔐 申请权限</span>
      <span class="perm-timer" data-deadline="${deadline}">⏱ ${mm}:${ss}</span>
    </div>
    <div class="perm-row"><span class="perm-k">操作</span><span class="perm-v">${esc(
      PERM_ACCESS_LABEL[p.access] ?? p.access,
    )} · ${esc(PERM_TIER_LABEL[p.tier] ?? p.tier)}</span></div>
    <div class="perm-row"><span class="perm-k">路径</span><code class="perm-path">${esc(p.path)}</code></div>
    ${widen}
    <div class="perm-row"><span class="perm-k">原因</span><span class="perm-v">${esc(
      p.reason || "（模型没说明）",
    )}</span></div>
    ${actions}
  </div>`;
}

/** 命令级申请卡（指纹维度）：显示原文 + 归一化（实际执行什么）+ 三种批准粒度 */
function renderMcpPermCard(p: PendingPerm): string {
  return `
  <div class="perm-card perm-card-cmd perm-risk-${esc(p.risk || "medium")}" data-perm="${esc(p.id)}">
    <div class="perm-title">🔌 调用 MCP 工具</div>
    <div class="perm-cmd">${esc(p.command)}</div>
    ${p.normalized ? `<div class="perm-hint">参数：${esc(p.normalized)}</div>` : ""}
    <div class="perm-hint">默认每次询问。选「总是允许」后同工具不再弹卡（可在设置 › MCP 撤销）。</div>
    <div class="perm-actions">
      <button type="button" class="perm-btn ok" data-act="approve">允许一次</button>
      <button type="button" class="perm-btn ok" data-act="always" title="永久允许该工具，跨会话、重启后仍生效">总是允许</button>
      <button type="button" class="perm-btn no" data-act="deny">拒绝</button>
      <button type="button" class="perm-btn why" data-act="deny-why" title="拒绝并说明理由">拒绝并说明</button>
    </div>
    <div class="perm-hint">Enter 允许一次 · Esc 拒绝 · Shift+Enter 总是允许</div>
  </div>`;
}

function renderCommandPermCard(p: PendingPerm): string {
  const deadline = p.createdAt + p.timeoutMs;
  const rest = Math.max(0, deadline - Date.now());
  const mm = Math.floor(rest / 60000);
  const ss = String(Math.floor((rest % 60000) / 1000)).padStart(2, "0");
  const riskLabel = PERM_RISK_LABEL[p.risk] ?? p.risk;

  const actions =
    denyReasonFor === p.id
      ? `
    <div class="perm-actions">
      <input class="perm-reason" type="text"
             placeholder="拒绝理由（可留空）—— 写了就告诉 AI 它该怎么改" />
      <button type="button" class="perm-btn no" data-act="deny-confirm">确认拒绝</button>
      <button type="button" class="perm-btn" data-act="deny-cancel">取消</button>
    </div>
    <div class="perm-hint">Enter 确认 · Esc 取消 · 留空也能确认</div>`
      : `
    <div class="perm-actions">
      <button type="button" class="perm-btn ok" data-act="approve">仅这一次</button>
      <button type="button" class="perm-btn ok" data-act="prefix"
              title="永久记住这类命令：${esc(p.headFp || "（命令名）")} —— 跨会话、重启后仍生效，可在设置里撤销">记住这类</button>
      <button type="button" class="perm-btn ok" data-act="full"
              title="永久记住这条完整命令 —— 跨会话、重启后仍生效，可在设置里撤销">记住这条</button>
      <button type="button" class="perm-btn no" data-act="deny">拒绝</button>
      <button type="button" class="perm-btn why" data-act="deny-why"
              title="拒绝并说明理由（会告诉 AI 它下次该怎么改）">✎</button>
    </div>
    <div class="perm-hint">Enter 仅这一次 · Esc 拒绝 · Shift+Enter 本轮不再问 · 「记住」=永久，可在设置撤销</div>`;

  // 归一化与原文一致时不重复展示（省一行空间）
  const normRow =
    p.normalized && p.normalized !== p.command
      ? `<div class="perm-row"><span class="perm-k">实际执行</span><code class="perm-cmd perm-cmd-norm">${esc(
          p.normalized,
        )}</code></div>`
      : "";

  return `
  <div class="perm-card perm-card-cmd perm-risk-${esc(p.risk)}" data-perm="${esc(p.id)}">
    <div class="perm-head">
      <span class="perm-title">🖥️ 执行命令 · ${esc(riskLabel)}</span>
      <span class="perm-timer" data-deadline="${deadline}">⏱ ${mm}:${ss}</span>
    </div>
    <div class="perm-row"><span class="perm-k">命令</span><code class="perm-cmd">${esc(p.command)}</code></div>
    ${normRow}
    <div class="perm-row"><span class="perm-k">原因</span><span class="perm-v">${esc(
      p.reason || "（模型没说明）",
    )}</span></div>
    ${actions}
  </div>`;
}

/** 权限卡局部刷新：只动卡片区，不整页 paintMessages（保 details.open，降重绘） */
function refreshPermCards(): void {
  if (view !== "chat") return;
  const b = document.getElementById("panel-body");
  if (!b) return;
  const host = b.querySelector<HTMLElement>(".perm-cards");
  if (host) {
    host.innerHTML = renderPermCards();
    bindPermButtons(host);
  } else {
    // 老布局：卡片区可能嵌在消息流里 —— 兜底整绘
    paintMessages(b);
  }
  // 权限卡出现/消失时若用户在底部，跟一下；否则别抢滚动
  const atBottom = b.scrollHeight - b.scrollTop - b.clientHeight < 60;
  if (atBottom) b.scrollTop = b.scrollHeight;
}

/** 提交用户的决定。失败不吞 —— 明确报出来，否则用户以为点了没反应 */
async function decidePerm(id: string, act: string, reason = ""): Promise<void> {
  try {
    // reason 只在拒绝时有意义（批准时后端会忽略），原样回给模型当"为什么不行"
    await invoke("perm_request_decide", { id, decision: act, reason });
  } catch (e) {
    const msg = String(e);
    // 已被处理 / 超时作废：卡片留着也没意义，安静移除即可
    if (msg.includes("不存在或已处理") || msg.includes("已失效")) {
      pendingPerms = pendingPerms.filter((x) => x.id !== id);
      void refreshPendingApprovals();
      refreshPermCards();
      return;
    }
    // 其它失败：**保留卡片**并恢复按钮，否则用户点失败后没法重试，
    // 后端还在阻塞等决定 —— 表现就是「卡死」。
    pushEntry("error", `权限申请处理失败：${e}`);
    const card = document.querySelector<HTMLElement>(`.perm-card[data-perm="${id}"]`);
    card?.querySelectorAll<HTMLButtonElement>(".perm-btn").forEach((b) => (b.disabled = false));
    return;
  }
  denyReasonFor = null;
  // 乐观移除：不等 perm-resolved 事件回来 —— 它会因为重绘时序晚到，
  // 那时再移除会让卡片闪一下又出现。
  pendingPerms = pendingPerms.filter((x) => x.id !== id);
  void refreshPendingApprovals();
  refreshPermCards();
}

/** 当前焦点在输入框里吗？在的话键盘不接管卡片（防"打字回车"误批权限） */
function isTypingInInput(): boolean {
  const a = document.activeElement;
  return a instanceof HTMLTextAreaElement || a instanceof HTMLInputElement;
}

function installPermHandlers(): void {
  // 按钮的绑定在 `bindPermButtons()`（渲染后直接绑），这里只装事件通道。

  // 新申请 → 推进来
  void listen<PendingPerm>("perm-request", (e) => {
    const p = e.payload;
    if (pendingPerms.some((x) => x.id === p.id)) return;

    if (permMuteThisTurn) {
      void decidePerm(p.id, "deny"); // 见 permMuteThisTurn 的说明（fail-closed）
      return;
    }

    pendingPerms.push(p);
    void refreshPendingApprovals(); // 点亮金球

    // 胶囊态**不自动展开** —— 项目定位是 always-on-top 不打扰，
    // 由角标提示，用户点球才展开。面板态就直接把卡插进对话流。
    if (mode === "panel") refreshPermCards();
  });

  // 申请被处理（多半是自己点的，但超时那条路径只有事件知道）
  void listen<{ id: string }>("perm-resolved", (e) => {
    pendingPerms = pendingPerms.filter((x) => x.id !== e.payload.id);
    void refreshPendingApprovals();
    refreshPermCards();
  });

  // 启动补拉：前端重载后别把已经挂起的申请弄丢（后端还在阻塞等它）
  void invoke<PendingPerm[]>("perm_requests_list")
    .then((list) => {
      if (list.length === 0) return;
      pendingPerms = list;
      void refreshPendingApprovals();
      refreshPermCards();
    })
    .catch(() => {});
}

/** 倒计时：只改文本，不重绘整个消息区 */
function startPermTimer(): void {
  window.setInterval(() => {
    document.querySelectorAll<HTMLElement>(".perm-timer").forEach((el) => {
      const deadline = Number(el.dataset.deadline ?? 0);
      if (!deadline) return;
      const rest = Math.max(0, deadline - Date.now());
      const mm = Math.floor(rest / 60000);
      const ss = String(Math.floor((rest % 60000) / 1000)).padStart(2, "0");
      el.textContent = `⏱ ${mm}:${ss}`;
    });
  }, 1000);
}

function renderPreview(): void {
  const box = document.getElementById("thumbs");
  if (!box) return;
  if (pendingImages.length === 0) {
    box.innerHTML = "";
    box.classList.remove("has");
    renderImageHint();
    return;
  }
  box.classList.add("has");
  renderImageHint();
  box.innerHTML = pendingImages
    .map((s, i) => renderThumb(s, i, true))
    .join("");
  box.querySelectorAll<HTMLButtonElement>(".thumb-x").forEach((b) => {
    b.addEventListener("click", () => {
      const i = Number(b.dataset.idx);
      pendingImages.splice(i, 1);
      renderPreview();
    });
  });
}

/**
 * 模型能力提示条：当前模型不支持图片、但用户贴了图时，明确告知会发生什么。
 *
 * 为什么要显式提示：非多模态模型下图片只会以**文件路径**发给模型，
 * 用户不知道的话会以为模型"能看图"，得到答非所问的结果还找不到原因。
 */
function renderImageHint(): void {
  const el = document.getElementById("img-hint");
  if (!el) return;
  const m = models.find((x) => x.id === selectedModel);
  const needHint = pendingImages.length > 0 && m && !m.supportsImages;
  el.style.display = needHint ? "block" : "none";
  if (needHint) {
    el.textContent = `当前模型 ${m.name || m.id} 不支持图片，将只发送图片路径（不会被识别内容）`;
  }
}

/** 给 promise 套超时：超时返回 fallback —— UI 绝不因某个命令挂起而永久"加载中" */
async function withTimeout<T>(p: Promise<T>, ms: number, fallback: T): Promise<T> {
  p.catch(() => {}); // 超时后原 promise 的失败不再冒泡（防 unhandled rejection）
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<T>((res) => {
    timer = setTimeout(() => res(fallback), ms);
  });
  try {
    return await Promise.race([p, timeout]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

function renderPanel(): void {
  const opts = models
    .map(
      (m) =>
        `<option value="${esc(m.name)}"${m.name === selectedModel ? " selected" : ""}>${esc(
          m.name,
        )}${m.vendor && m.vendor !== "Custom" ? ` · ${esc(m.vendor)}` : ""}${
          m.supportsImages ? " 🖼" : ""
        }${m.supportsToolCall ? " 🔧" : ""}</option>`,
    )
    .join("");

  // 主对话 / 项目（新建走独立「项+」，select 只显示真实项目，永远选中当前）
  const projOpts = [
    `<option value=""${activeProjectId == null ? " selected" : ""}>主对话</option>`,
    ...projects.map(
      (p) =>
        `<option value="${p.id}"${p.id === activeProjectId ? " selected" : ""}>📁 ${esc(
          p.name,
        )}</option>`,
    ),
  ].join("");
  const projTitle =
    activeProjectId == null
      ? "主对话"
      : (projects.find((p) => p.id === activeProjectId)?.name ?? "主对话");

  app.innerHTML = `
    <div class="panel">
      <div class="panel-header" id="panel-header">
        <span class="panel-title">float-agent</span>
        <button class="panel-btn" id="btn-gear" title="设置 / 模型">⚙</button>
        <button class="panel-btn" id="btn-close" title="收起">—</button>
      </div>

      <div class="panel-toolbar">
        <select id="model-select" ${models.length ? "" : "disabled"}>
          ${opts || '<option>（没有可用模型）</option>'}
        </select>
        <select id="project-select" title="当前：${esc(projTitle)} · 切项目看该项目会话">
          ${projOpts}
        </select>
        <button class="mini-btn" id="btn-grab" title="截取当前屏幕">截</button>
        <button class="mini-btn" id="btn-new" title="新建会话 / 项目">＋</button>
        <button class="mini-btn" id="btn-sess" title="会话（主聊天 / 任务）">史</button>
      </div>

      <div class="fg-ctx" id="fg-ctx" title="用户当前前台应用（点击复制路径）" style="display:none"></div>

      <div class="panel-body" id="panel-body"></div>

      <div class="thumbs" id="thumbs"></div>
      <div class="img-hint" id="img-hint" style="display:none"></div>

      <div class="panel-input">
        <textarea id="input" rows="1" placeholder="问点什么…（Enter 发送 / Shift+Enter 换行 / Ctrl+V 贴图）"></textarea>
        <button id="btn-stop" title="停止生成" hidden>■</button>
        <button id="btn-send" title="发送">↵</button>
      </div>

      <!-- 轻提示层：非对话页（设置/记忆/会话）里的临时信息走这里，
           绝不塞进对话流。见 pushEntry 里的说明。 -->
      <div class="toast-wrap" id="toast-wrap"></div>
    </div>
  `;

  attachDrag(document.getElementById("panel-header")!, () => {
    /* 标题栏只拖动 */
  });

  document.getElementById("btn-close")!.addEventListener("click", () => void collapse());
  document.getElementById("btn-grab")!.addEventListener("click", () => void grabScreen());
  // 「测」「忆」的工具栏入口已删（与设置页重复）：
  //   测试连通性 → 设置 › 模型（每个模型一行一颗）
  //   记忆审批   → 设置入口页「🧠 记忆」
  document.getElementById("btn-new")!.addEventListener("click", (e) => {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    showNewMenu(r.left, r.bottom + 4);
  });
  document.getElementById("btn-sess")!.addEventListener("click", () => void switchView("sessions"));

  // 当前会话标题挂到「史」按钮的 tooltip 上，一眼知道在哪个会话里
  const curSess = sessionList.find((s) => s.id === currentSessionId);
  const sessBtn = document.getElementById("btn-sess");
  if (sessBtn && curSess) {
    const tag = curSess.kind === "main" ? "主聊天" : curSess.kind === "fork" ? "分支" : "任务";
    sessBtn.title = `会话（${tag}：${curSess.title}）`;
  }
  document.getElementById("btn-gear")!.addEventListener("click", () => void switchView("settings"));

  const sel = document.getElementById("model-select") as HTMLSelectElement;
  sel?.addEventListener("change", async () => {
    selectedModel = sel.value;
    renderImageHint(); // 换模型可能改变"支不支持图片"，提示条要跟着变
    await invoke("set_selected_model", { id: selectedModel }).catch((e) =>
      pushEntry("error", `保存模型选择失败：${e}`),
    );
  });

  // 顶栏项目：只切换；新建入口在「＋」菜单里（与新建会话合并）
  const psel = document.getElementById("project-select") as HTMLSelectElement | null;
  psel?.addEventListener("change", async () => {
    const id = psel.value === "" ? null : Number(psel.value);
    activeProjectId = id;
    try {
      await invoke("pmem_set_active", { id });
      // 只切记忆上下文，不改任何会话的挂靠
      await refreshSessionProjectTags();
    } catch (e) {
      showToast(`切换项目失败：${e}`, "error");
    }
    view = "sessions";
    renderBody();
  });

  const input = document.getElementById("input") as HTMLTextAreaElement;

  // 恢复上次收起时留下的草稿（含高度自适应，否则多行草稿会被压成一行）
  if (input && draftInput) {
    input.value = draftInput;
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 120) + "px";
  }

  input?.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  });
  input?.addEventListener("input", () => {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 120) + "px";
    // 边打边同步草稿：这样无论面板因什么原因被重建，内容都不会丢
    draftInput = input.value;
  });

  // 发送按钮**只负责发送**。忙碌时它变成「插话」——ENTER 永远把用户打的字送出去，
  // 语义唯一，不用猜。停止是旁边那颗独立的 ■。
  document.getElementById("btn-send")!.addEventListener("click", () => void send());
  document.getElementById("btn-stop")!.addEventListener("click", () => void stopRun());

  renderPreview();
  renderBody();
  // 面板可能是"跑着任务时收起 → 又展开"的，按钮布局要跟着当前状态走
  setBusyUi(busy);
  input?.focus();
  void refreshFgCtx();
}

/** 前台应用指示条 —— 展示「用户最近在看的几个应用/目录」 */
interface FgCtx {
  app: string;
  title: string;
  dir: string | null;
  file: string | null;
}

/** 指示条最多显示几条（Rust 侧保留 6 条） */
const FG_SHOW_MAX = 6;
/** 列表是否展开：折叠时只显示第一条 + 「+N」角标，点角标翻开/收起 */
let fgExpanded = false;
let fgTimer: ReturnType<typeof setTimeout> | undefined;

async function refreshFgCtx(loop = true): Promise<void> {
  const el = document.getElementById("fg-ctx");
  if (!el) return;
  try {
    const list = await withTimeout(
      invoke<FgCtx[]>("foreground_history"),
      2000,
      [] as FgCtx[],
    );
    const items = (list || [])
      .filter((c) => c && c.app && !c.app.toLowerCase().includes("float-agent"))
      .slice(0, FG_SHOW_MAX);

    if (items.length === 0) {
      // 空态不隐藏整体 —— 显示提示，避免用户以为功能没了
      el.innerHTML = `<div class="fg-row fg-empty">👁 还没记录到其他应用（切到 VSCode/Typora 再回来看）</div>`;
      el.style.display = "block";
    } else {
      const rowHtml = (c: FgCtx, i: number) => {
        const where = c.file || c.dir || "";
        const base = where
          ? (where.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? where)
          : "";
        const label = base ? `${c.app} · ${base}` : c.app;
        const tip = [c.app, c.dir && `目录 ${c.dir}`, c.file && `文件 ${c.file}`]
          .filter(Boolean)
          .join("\n");
        const dot = i === 0 ? "👁" : "·";
        return `<div class="fg-row" data-path="${esc(where)}" title="${esc(tip)}">
          <span class="fg-eye">${dot}</span><span class="fg-label">${esc(label)}</span></div>`;
      };
      // 可翻列表：折叠态只露最近 1 条 + 「▾ N」角标；点开看全部，再点收起。
      // 好处：默认不占面板高度（原来 4 条能把聊天区挤掉一屏），
      //       需要回看历史时一键翻开。
      const shown = fgExpanded ? items : items.slice(0, 1);
      const more = items.length - shown.length;
      el.innerHTML =
        shown.map((c, i) => rowHtml(c, i)).join("") +
        (items.length > 1
          ? `<button class="fg-toggle" id="fg-toggle" title="${fgExpanded ? "收起" : "展开全部 " + items.length + " 条"}">${fgExpanded ? "▴ 收起" : `▾ 更多 ${more}`}</button>`
          : "");
      el.classList.toggle("fg-expanded", fgExpanded);
      el.style.display = "block";
      el.querySelectorAll<HTMLElement>(".fg-row").forEach((r) =>
        r.addEventListener("click", () => {
          const p = r.dataset.path || "";
          if (p) void navigator.clipboard.writeText(p).catch(() => {});
        }),
      );
      const tg = document.getElementById("fg-toggle");
      tg?.addEventListener("click", (e) => {
        e.stopPropagation();
        fgExpanded = !fgExpanded;
        void refreshFgCtx(false);
      });
    }
  } catch {
    el.innerHTML = `<div class="fg-row fg-empty">👁 上下文读取失败</div>`;
    el.style.display = "block";
  }
  if (loop) scheduleFgCtx();
}

/** 面板停留期间每 3 秒刷新一次（用户切走又回来也能看到最新历史） */
function scheduleFgCtx(): void {
  if (fgTimer) clearTimeout(fgTimer);
  fgTimer = setTimeout(() => {
    if (mode !== "panel" || !document.getElementById("fg-ctx")) return;
    void refreshFgCtx(true);
  }, 3000);
}

function scrollToBottom(): void {
  const b = document.getElementById("panel-body");
  if (b) b.scrollTop = b.scrollHeight;
}

// ---------------- 内容区：对话 / 记忆 / 设置 ----------------

/** 渲染内容区 —— 由 view 决定 */
function renderBody(): void {
  const b = document.getElementById("panel-body");
  if (!b) return;

  // 非对话视图下不需要输入区
  const inputArea = document.querySelector(".panel-input") as HTMLElement | null;
  if (inputArea) inputArea.style.display = view === "chat" ? "flex" : "none";
  const thumbs = document.getElementById("thumbs");
  if (thumbs && view !== "chat") {
    thumbs.style.display = "none";
    thumbs.classList.remove("has");
  }

  if (view === "mem") {
    const keepMem = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void Promise.all([
      invoke<Candidate[]>("mem_pending").catch(() => [] as Candidate[]),
      invoke<MemoryFiles>("mem_files").catch(() => null),
      invoke<SkillInfo[]>("skills_list").catch(() => [] as SkillInfo[]),
      invoke<ProjectMemoryView[]>("mem_projects").catch(() => [] as ProjectMemoryView[]),
    ]).then(([list, files, sk, projects]) => {
      if (view !== "mem") return;
      b.innerHTML = renderMemView(list, files, sk, projects ?? []);
      b.scrollTop = keepMem;
      bindMemButtons(b);
    })
    .catch((e) => {
      if (view !== "mem") return;
      b.innerHTML = `<div class="mem-head">记忆</div>
        <div class="mem-empty">记忆页渲染失败：${esc(String(e))}</div>`;
    });
    return;
  }

  if (view === "sessions") {
    b.innerHTML = `<div class="mem-head">会话历史<button class="mem-back" id="sub-back">‹ 对话</button></div>`;
    void refreshSessionList().then(() => {
      if (view !== "sessions") return;
      b.innerHTML = renderSessionsView();
      bindSessionsView(b);
      void refreshSessionProjectTags().then(() => {
        if (view === "sessions") {
          b.innerHTML = renderSessionsView();
          bindSessionsView(b);
        }
      });
    });
    return;
  }

  if (view === "settings") {
    // 设置入口页：只列三个入口 + 关键状态摘要，不铺开内容
    // ⚠️ 每个 invoke 都套 3s 超时 —— 曾因 reg.exe 子进程挂死把本页卡成永久"加载中"
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    const bd = <T,>(pr: Promise<T>, fb: T) => withTimeout(pr, 3000, fb);
    void Promise.all([
      bd(invoke<McpStatus>("mcp_status").catch(() => null), null),
      bd(invoke<Candidate[]>("mem_pending").catch(() => [] as Candidate[]), [] as Candidate[]),
      bd(invoke<boolean>("autostart_status").catch(() => false), false),
      bd(invoke<PermRule[]>("perm_rules").catch(() => [] as PermRule[]), [] as PermRule[]),
      bd(invoke<{ blurCollapse?: boolean }>("get_settings").catch(() => null), null),
      bd(invoke<UsageRecord[]>("usage_report").catch(() => [] as UsageRecord[]), [] as UsageRecord[]),
      bd(invoke<SearchStatus | null>("search_status").catch(() => null), null),
    ])
      .then(([mcp, pending, autoOn, rules, settings, usage, search]) => {
        if (view !== "settings") return;
        autostartOn = autoOn;
        blurCollapse = settings?.blurCollapse ?? true;
        permCount = rules.length;
        searchInfo = search;
        const tSum = todayTokenTotal(usage);
        const summary = usage.length ? `今日 ${fmtTokens(tSum)} · 共 ${usage.length} 次问答` : "暂无记录";
        b.innerHTML = renderSettingsMenu(mcp, pending.length, summary);
        b.scrollTop = keep;
        bindSettingsMenu(b);
      })
      .catch((e) => {
        // 渲染链抛异常也必须显示出来 —— 之前没 catch，异常会让页面永远停在"加载中"
        if (view !== "settings") return;
        b.innerHTML = `<div class="mem-head">设置</div>
          <div class="mem-empty">设置页渲染失败：${esc(String(e))}<br>请把这句话反馈给开发者</div>`;
      });
    return;
  }

  if (view === "models") {
    b.innerHTML = renderModelsView();
    bindModelsView(b);
    return;
  }

  if (view === "search") {
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<SearchStatus | null>("search_status")
      .catch(() => null)
      .then((st) => {
        if (view !== "search") return;
        searchInfo = st;
        b.innerHTML = renderSearchView(st);
        bindSearchView(b);
      });
    return;
  }

  if (view === "perm") {
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<PermRule[]>("perm_rules")
      .then((rules) => {
        if (view !== "perm") return;
        b.innerHTML = renderPermView(rules);
        bindPermView(b);
      })
      .catch((e) => {
        if (view !== "perm") return;
        b.innerHTML = `<div class="mem-head">文件权限</div>
          <div class="mem-empty">读取权限规则失败：${esc(String(e))}</div>`;
      });
    return;
  }

  if (view === "cmdpolicy") {
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void Promise.all([
      invoke<CmdPolicy>("perm_cmd_policy").catch(() => ({ allow: [], hard_block: [] })),
      invoke<string>("perm_cmd_policy_path").catch(() => ""),
      invoke<CmdGrantsView[]>("perm_cmd_grants_list").catch(() => [] as CmdGrantsView[]),
      invoke<CmdAuditRecord[]>("perm_cmd_audit_tail", { limit: 80 }).catch(() => [] as CmdAuditRecord[]),
    ])
      .then(([policy, path, grants, audit]) => {
        if (view !== "cmdpolicy") return;
        b.innerHTML = renderCmdPolicyView(policy, path, grants, audit);
        b.scrollTop = keep;
        bindCmdPolicyView(b);
      })
      .catch((e) => {
        if (view !== "cmdpolicy") return;
        b.innerHTML = `${subHeader("命令策略")}
          <div class="mem-empty">读取命令策略失败：${esc(String(e))}</div>`;
      });
    return;
  }

  if (view === "mcp") {
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void Promise.all([
      invoke<McpStatus>("mcp_status").catch(() => null),
      invoke<McpServerCfg[]>("mcp_servers_list").catch(() => [] as McpServerCfg[]),
      invoke<string[]>("mcp_grants_list").catch(() => [] as string[]),
    ])
      .then(([st, servers, grants]) => {
        if (view !== "mcp") return;
        b.innerHTML = renderMcpView(st, servers ?? [], grants ?? []);
        b.scrollTop = keep;
        bindMcpView(b);
      })
      .catch(() => {
        if (view !== "mcp") return;
        b.innerHTML = renderMcpView(null, [], []);
        b.scrollTop = keep;
        bindMcpView(b);
      });
    return;
  }

  if (view === "usage") {
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<UsageRecord[]>("usage_report")
      .then((recs) => {
        if (view !== "usage") return;
        b.innerHTML = renderUsageView(recs ?? []);
        b.scrollTop = keep;
        bindUsageView(b);
      })
      .catch((e) => {
        if (view !== "usage") return;
        b.innerHTML = `<div class="mem-head">Token 用量</div>
          <div class="mem-empty">读取用量失败：${esc(String(e))}</div>`;
      });
    return;
  }

  paintMessages(b);
  renderPreview(); // 回到对话时恢复缩略图区
  scrollToBottom();
  bindInitPrompt(b);
}

/**
 * 空态里的按钮 —— 两种形态，按当前缺什么决定。
 *
 * 形态一（`needsInit`）：调 Rust 的 `bootstrap_data` **建目录**。
 *   这里**必须**是代码干活，不能把「初始化」这句话发给模型 ——
 *   那是个死锁：要跑模型得有 `models.json`，而 `models.json`
 *   就在还没建出来的 `agent-data/` 里。
 *
 * 形态二（目录已建、没模型）：直接打开设置面板让用户加模型。
 *   加模型只有这一条路（id / url / apiKey 三样，得用户自己填）。
 */
function bindInitPrompt(root: HTMLElement): void {
  const btn = root.querySelector<HTMLButtonElement>("#init-go");
  if (!btn) return;

  if (!needsInit) {
    // 形态二：没模型 → 直接跳"模型"设置页
    btn.addEventListener("click", () => {
      view = "chat"; // 保证 switchView 的 toggle 不把 models 又切回 chat
      void switchView("models");
    });
    return;
  }

  // 形态一：建目录（纯 Rust，不碰模型）
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    btn.textContent = "创建中…";
    try {
      const r = await invoke<{
        dataDir: string;
        createdDirs: string[];
        createdFiles: string[];
        skipped: string[];
        hasModels: boolean;
      }>("bootstrap_data");

      console.log(
        `[float-agent] 初始化完成：建目录 ${r.createdDirs.length} 个 / 建文件 ${r.createdFiles.length} 个 / 跳过 ${r.skipped.length} 个 → ${r.dataDir}`,
      );

      // 重新自检 —— needsInit 变 false 后空态自动切到「去加模型」那一屏，
      // 这就是给用户的最直接反馈
      await checkBoot();
      void loadModels();
      view = "chat";
      renderBody();

      // 顺手把人带到模型页，省得他去找设置入口
      void switchView("models");
    } catch (e) {
      console.error("[float-agent] 初始化失败:", e);
      btn.disabled = false;
      btn.textContent = "重试";
      const hint = root.querySelector<HTMLElement>(".init-hint");
      if (hint) hint.innerHTML = `初始化失败：${esc(String(e))}`;
    }
  });
}

/** 记忆文件族（Rust mem_files 返回） */
interface MemoryFiles {
  rules: string;
  soul: string;
  identity: string;
  user: string;
  memory: string;
}

interface ProjectMemoryView {
  name: string;
  source: string;
  sourceAlive: boolean;
  hasMemory: boolean;
}

interface CmdPolicy {
  allow: string[];
  hard_block: string[];
}

interface CmdGrantsView {
  fingerprint: string;
  display: string;
  scope: string;
  tier: string;
  remaining?: number | null;
  reason?: string;
}

interface CmdVerdictView {
  verdict: "allow" | "ask" | "hard-block" | string;
  reason: string;
  risk: string;
  normalized: string;
}

interface CmdAuditRecord {
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

const MEM_FILE_DEFS: Array<{ key: keyof MemoryFiles; icon: string; name: string; desc: string }> = [
  { key: "rules", icon: "⚠️", name: "RULES.md", desc: "行为红线（最高优先级）" },
  { key: "soul", icon: "🎭", name: "SOUL.md", desc: "人格 / 语气" },
  { key: "identity", icon: "🪪", name: "IDENTITY.md", desc: "称呼约定" },
  { key: "user", icon: "👤", name: "USER.md", desc: "用户画像" },
  { key: "memory", icon: "📜", name: "memory/MEMORY.md", desc: "长期记忆（审批后写入）" },
];

/// 技能清单项（Rust `skills_list` 返回）
interface SkillInfo {
  name: string;
  description: string;
  path: string;
}

function renderMemView(
  list: Candidate[],
  files: MemoryFiles | null,
  skills: SkillInfo[],
  projects: ProjectMemoryView[],
): string {
  const pendingHtml =
    list.length === 0
      ? `<div class="mem-empty">没有待审批的记忆。模型认为值得长期记住的信息会通过 <code>remember</code> 提交到这里。</div>`
      : list
          .map(
            (c) => `
      <div class="mem-item">
        <div class="mem-text">${esc(c.content)}</div>
        <div class="mem-meta">${esc(c.source)} · ${esc(c.created_at)}</div>
        <div class="mem-actions">
          <button class="mem-btn ok" data-id="${esc(c.id)}" title="写入长期记忆">✓ 记住</button>
          <button class="mem-btn no" data-id="${esc(c.id)}" title="驳回（留档）">✗ 丢弃</button>
        </div>
      </div>`,
          )
          .join("");

  // 四大记忆文件 + 长期记忆 —— 每个一份可折叠全文，空文件明确标出
  const filesHtml = MEM_FILE_DEFS.map((d) => {
    const content = (files?.[d.key] ?? "").trim();
    const body = content
      ? `<details class="mem-file"><summary>${d.icon} ${d.name}<i>${d.desc} · ${content.length} 字</i></summary><pre>${esc(content)}</pre></details>`
      : `<div class="mem-file-empty">${d.icon} ${d.name} <i>${d.desc}</i> —— （空）</div>`;
    return body;
  }).join("");

  const projHtml =
    projects.length === 0
      ? `<div class="mem-empty">还没有项目记忆。在 <code>agent-data/projects/&lt;名&gt;/</code> 放 <code>source.ref</code>（项目绝对路径）和 <code>MEMORY.md</code> 即可；对话时前台路径命中会自动注入。</div>`
      : projects
          .map((p) => {
            const badge = !p.sourceAlive
              ? `<span style="color:#f0a8a8">源路径不存在</span>`
              : p.hasMemory
                ? `<span style="color:#8fd49a">已绑定</span>`
                : `<span style="color:#f0c674">MEMORY 为空</span>`;
            return `
      <div class="skill-item">
        <div class="skill-name">📁 ${esc(p.name)} ${badge}</div>
        <div class="skill-desc"><code>${esc(p.source || "（无 source.ref）")}</code></div>
      </div>`;
          })
          .join("");

  // 技能：清单来自 agent-data/skills/*/SKILL.md 的 frontmatter，热扫描
  const skillsHtml =
    skills.length === 0
      ? `<div class="mem-empty">还没有技能。对话中教它一套流程，让它用 <code>save_skill</code> 沉淀，或直接在 <code>agent-data/skills/名字/SKILL.md</code> 手写（frontmatter 带 name + description）。</div>`
      : skills
          .map(
            (s) => `
      <div class="skill-item">
        <div class="skill-name">🧩 ${esc(s.name)}</div>
        <div class="skill-desc">${esc(s.description)}</div>
      </div>`,
          )
          .join("");

  return `
    <div class="mem-head">待审批 ${list.length} 条</div>
    ${pendingHtml}
    <div class="mem-head"> 记忆文件族</div>
    <div class="set-hint" style="margin-bottom:8px">
      这些文件都在 <code>agent-data/</code> 下，直接编辑即生效（下次对话），也可用 <code>write_file</code> 改。
    </div>
    ${filesHtml}
    <div class="mem-head">📁 项目记忆 ${projects.length} 个</div>
    ${projHtml}
    <div class="mem-head">技能 ${skills.length} 个</div>
    <div class="set-hint" style="margin-bottom:8px">
      用户消息命中技能触发词（描述里的引号短语 / 技能名）时正文会<strong>自动加载</strong>；其余靠模型调 <code>load_skill</code> 读全文。教会它新流程可用 <code>save_skill</code> 保存。
    </div>
    ${skillsHtml}`;
}

function bindMemButtons(root: HTMLElement): void {
  root.querySelectorAll<HTMLButtonElement>(".mem-btn.ok").forEach((btn) =>
    btn.addEventListener("click", async () => {
      await invoke("mem_approve", { id: btn.dataset.id }).catch((e) =>
        pushEntry("error", `批准失败：${e}`),
      );
      renderBody();
    }),
  );
  root.querySelectorAll<HTMLButtonElement>(".mem-btn.no").forEach((btn) =>
    btn.addEventListener("click", async () => {
      await invoke("mem_reject", { id: btn.dataset.id }).catch((e) =>
        pushEntry("error", `驳回失败：${e}`),
      );
      renderBody();
    }),
  );
}

async function switchView(v: View): Promise<void> {
  // 再点同一个非 chat 视图 → 退回对话（toggle 手感）
  const next = v === view && v !== "chat" ? "chat" : v;
  // 离开模型页时丢掉表单草稿，避免下次进来卡在「编辑模式 + 空字段」
  if (view === "models" && next !== "models") {
    modelForm = emptyModelForm();
    remoteFetch = null;
  }
  view = next;
  renderBody();
}

// ---------------- 设置：入口页 ----------------

/**
 * 设置入口页 —— **只列入口和状态摘要**，内容点进去再看。
 * （早先把模型/MCP/记忆全平铺在一页，滚半天找不到东西）
 */
function renderSettingsMenu(
  mcp: McpStatus | null,
  pendingCount: number,
  usageSummary: string,
): string {
  const curModel =
    models.find((m) => m.id === selectedModel)?.name ?? "（未选）";

  const mcpSummary = mcp
    ? mcp.connected
      ? `${mcp.groups.length} 组 / ${mcp.groups.reduce((a, g) => a + g.toolCount, 0)} 个工具` +
        ` · 已加载 ${mcp.groups.filter((g) => g.active).length}`
      : "未连接"
    : "读取中…";

  const searchSummary = searchInfo?.ready
    ? `${searchInfo.providerLabel}${searchInfo.hasKey ? ` · ${searchInfo.keyPreview}` : ""}`
    : "未配置（模型无 web_search）";

  const item = (
    act: View,
    icon: string,
    title: string,
    summary: string,
    badge?: string,
  ) => `
    <button class="set-entry" data-act="${act}">
      <span class="set-entry-icon">${icon}</span>
      <span class="set-entry-text">
        <b>${title}</b>
        <i>${esc(summary)}</i>
      </span>
      ${badge ? `<span class="set-entry-badge">${badge}</span>` : ""}
      <span class="set-entry-arrow">›</span>
    </button>`;

  return `
    <div class="mem-head">设置<button class="mem-back" id="set-back">返回对话</button></div>
    ${item("models", "🧩", "模型", `当前 ${curModel} · 共 ${models.length} 个`)}
    ${item("search", "🌐", "搜索 web_search", searchSummary)}
    ${item("usage", "📊", "Token 用量", usageSummary)}
    ${item("mcp", "🔌", "MCP 外部工具", mcpSummary)}
    ${item("perm", "📁", "文件权限", `已配置 ${permCount} 条规则`)}
    ${item("cmdpolicy", "⌨️", "命令策略", "白名单 / 硬阻断 / 审计（run_command）")}
    ${item("mem", "🧠", "记忆", "待审批、记忆文件族与项目绑定", pendingCount > 0 ? String(pendingCount) : undefined)}

    <div class="mem-head">启动与退出</div>
    <label class="set-check set-check-row">
      <input type="checkbox" id="auto-start" ${autostartOn ? "checked" : ""} />
      开机自动启动（写 HKCU\\...\\Run 注册表项，release 版无黑框）
    </label>
    <label class="set-check set-check-row">
      <input type="checkbox" id="blur-collapse" ${blurCollapse ? "checked" : ""} />
      面板失焦自动收起（点别处就缩回球，不挡屏幕）
    </label>
    <div class="set-hint">分发 WB：对 agent 说「交给 WB / 分发给 WorkBuddy」（技能 dispatch-workbuddy），或收起后右键悬浮球。交接文件在 agent-data/dispatch/workbuddy/。</div>
    <div class="set-hint" id="exe-path">程序路径读取中…</div>
    <div class="set-actions" style="margin-top:10px">
      <button class="set-btn del" id="btn-quit">⏻ 退出 float-agent</button>
    </div>
  `;
}

function bindSettingsMenu(root: HTMLElement): void {
  document.getElementById("set-back")?.addEventListener("click", () => void switchView("chat"));
  root.querySelectorAll<HTMLButtonElement>(".set-entry").forEach((b) =>
    b.addEventListener("click", () => void switchView(b.dataset.act as View)),
  );

  // --- 开机自启 ---
  const chk = document.getElementById("auto-start") as HTMLInputElement | null;
  chk?.addEventListener("change", async () => {
    try {
      await invoke("autostart_set", { enable: chk.checked });
    } catch (e) {
      chk.checked = !chk.checked; // 回滚
      pushEntry("error", `设置开机自启失败：${e}`);
    }
  });

  // --- 失焦收起 ---
  const bc = document.getElementById("blur-collapse") as HTMLInputElement | null;
  bc?.addEventListener("change", async () => {
    try {
      await invoke("set_blur_collapse", { enable: bc.checked });
      blurCollapse = bc.checked;
    } catch (e) {
      bc.checked = !bc.checked;
      pushEntry("error", `设置失焦收起失败：${e}`);
    }
  });

  // --- 分发走 skill：设置页不再配置路径；球菜单/agent 调 dispatch_task ---
  // drop 目录仅 settings.json 高级字段，主 UI 不暴露。

  void invoke<string>("exe_path")
    .then((p) => {
      const el = document.getElementById("exe-path");
      if (el) el.textContent = `程序路径：${p}`;
    })
    .catch(() => {});

  // --- 退出 ---
  document.getElementById("btn-quit")?.addEventListener("click", () => {
    void invoke("app_quit").catch(() => {});
  });
}

/** 子页顶部的返回条（统一回设置入口页） */
function subHeader(title: string): string {
  return `<div class="mem-head">${title}<button class="mem-back" id="sub-back">‹ 设置</button></div>`;
}

// ---------------- 设置 › 搜索（web_search） ----------------

const SEARCH_PROVIDERS: { id: string; label: string }[] = [
  { id: "", label: "关闭（不暴露 web_search）" },
  { id: "tavily", label: "Tavily（默认 · 常有免费额度）" },
  { id: "exa", label: "Exa（语义搜索）" },
  { id: "brave", label: "Brave Search" },
];

function renderSearchView(st: SearchStatus | null): string {
  const opts = SEARCH_PROVIDERS.map(
    (p) =>
      `<option value="${p.id}"${(st?.provider ?? "") === p.id ? " selected" : ""}>${p.label}</option>`,
  ).join("");
  const readyMsg = st?.ready
    ? `<div class="set-hint">✅ 已启用：${esc(st.providerLabel)} · key ${esc(st.keyPreview || "（已保存）")}</div>`
    : `<div class="set-hint">未启用 —— 模型工具列表里<strong>不会</strong>出现 <code>web_search</code>。</div>`;
  return `
    ${subHeader("搜索 web_search")}
    <div class="set-hint" style="margin-bottom:8px">
      内置网页搜索：<b>默认 Tavily</b>，也可换成 Exa / Brave。<br>
      配置后模型才会多出 <code>web_search</code> 工具。<br>
      ⚠️ <b>查询词会发送到所选服务商</b>；key 只存本地
      <code>agent-data/search.json</code>（已 gitignore）。
    </div>
    ${readyMsg}
    <div class="mem-head">配置</div>
    <div class="set-form">
      <select id="search-provider">${opts}</select>
      <input id="search-key" type="password"
        placeholder="${st?.hasKey ? "已保存 key（留空则不修改；填新值则覆盖）" : "API Key"}" />
      <button class="set-btn add" id="search-save">保存</button>
      <button class="set-btn test" id="search-test" ${st?.ready ? "" : "disabled"}>测一下</button>
      <div class="set-hint" id="search-msg"></div>
      <div class="set-hint">
        Tavily：tavily.com 控制台创建 key（额度以控制台为准）。<br>
        Exa：exa.ai · Brave：brave.com/search/api。<br>
        拿不准先用 Tavily；换后端只改下拉，key 各用各的。
      </div>
    </div>`;
}

function bindSearchView(_root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));
  const msg = document.getElementById("search-msg");
  const say = (t: string, cls: "run" | "ok" | "bad"): void => {
    if (!msg) return;
    msg.className = `set-hint ${cls}`;
    msg.textContent = t;
  };

  document.getElementById("search-save")?.addEventListener("click", async () => {
    const provider = (document.getElementById("search-provider") as HTMLSelectElement).value;
    const keyInput = (document.getElementById("search-key") as HTMLInputElement).value;
    const args: Record<string, unknown> = { provider };
    // 空输入 = 不改 key；非空 = 覆盖
    if (keyInput.trim()) args.apiKey = keyInput.trim();
    try {
      const st = await invoke<SearchStatus>("search_set", args);
      searchInfo = st;
      say(
        st.ready
          ? `✅ 已保存：${st.providerLabel} · ${st.keyPreview}`
          : `已保存（未就绪：provider=${st.provider || "关闭"}${st.hasKey ? "" : "，缺 key"}）`,
        st.ready ? "ok" : "run",
      );
      showToast(st.ready ? `✅ 搜索：${st.providerLabel}` : "搜索配置已保存", st.ready ? "ok" : "info");
      b_refresh_search_test();
    } catch (e) {
      say(`保存失败：${e}`, "bad");
    }
  });

  const b_refresh_search_test = (): void => {
    const btn = document.getElementById("search-test") as HTMLButtonElement | null;
    if (btn) btn.disabled = !searchInfo?.ready;
  };

  document.getElementById("search-test")?.addEventListener("click", async () => {
    const btn = document.getElementById("search-test") as HTMLButtonElement;
    btn.disabled = true;
    const old = btn.textContent;
    btn.textContent = "测…";
    say("正在搜索 ping…", "run");
    try {
      const r = await invoke<string>("search_test");
      say(`✅ ${r}`, "ok");
      showToast("✅ 搜索连通正常", "ok");
    } catch (e) {
      say(`❌ ${e}`, "bad");
      showToast(`❌ 搜索测试失败：${e}`, "error");
    } finally {
      btn.disabled = !searchInfo?.ready;
      btn.textContent = old;
    }
  });
}

// ---------------- 设置 › 模型 ----------------
function renderModelsView(): string {
  const q = (modelSearch || "").trim().toLowerCase();
  const filtered = q
    ? models.filter(
        (m) =>
          m.id.toLowerCase().includes(q) ||
          m.name.toLowerCase().includes(q) ||
          m.url.toLowerCase().includes(q),
      )
    : models;

  const rows = filtered
    .map((m) => {
      const cur = m.name === selectedModel;
      const ctx = m.maxInputTokens ? ` · ${Math.round(m.maxInputTokens / 1000)}k` : "";
      return `
      <div class="set-row${cur ? " cur" : ""}" data-id="${esc(m.name)}">
        <div class="set-name">${esc(m.name)}${cur ? ' <span class="set-cur">当前</span>' : ""}</div>
        <div class="set-url">${
          m.name !== m.id ? `<span class="set-id">model=${esc(m.id)}</span> · ` : ""
        }${esc(m.url)}${ctx} · ${m.hasKey === false ? "🔓无Key " : ""}${m.supportsImages ? "🖼" : ""}${
        m.supportsToolCall ? "🔧" : ""
      }</div>
        <div class="set-actions">
          ${cur ? "" : `<button class="set-btn use" data-id="${esc(m.name)}">使用</button>`}
          <button class="set-btn edit" data-id="${esc(m.name)}">改</button>
          <button class="set-btn test" data-id="${esc(m.name)}">测</button>
          <button class="set-btn del" data-id="${esc(m.name)}">删</button>
        </div>
        <div class="set-test-msg" hidden></div>
      </div>`;
    })
    .join("");

  const f = modelForm;
  const editing = f.editId !== null;
  const presetOpts = [
    `<option value="custom"${f.preset === "custom" ? " selected" : ""}>自定义</option>`,
    ...PROVIDER_PRESETS.map(
      (p) =>
        `<option value="${esc(p.key)}"${f.preset === p.key ? " selected" : ""}>${esc(p.name)} · ${esc(
          shortHost(p.url),
        )}</option>`,
    ),
  ].join("");

  // 「从提供商获取模型 ID」的弹出列表
  const picker = remoteFetch
    ? `
    <div class="id-picker">
      <div class="id-picker-head">
        <span>从 <code>${esc(remoteFetch.lastEndpoint ?? remoteFetch.sourceUrl)}</code> 选择模型 ID</span>
        <button type="button" class="set-btn" id="picker-close">关闭</button>
      </div>
      ${remoteFetch.error ? `<div class="form-err">${esc(remoteFetch.error)}</div>` : ""}
      <div class="id-picker-list">
        ${
          remoteFetch.items.length === 0
            ? '<div class="mem-empty">没有返回模型。</div>'
            : remoteFetch.items
                .map((it) => {
                  const already = remoteFetch!.localIds.has(it.id);
                  return `<button type="button" class="id-pick${already ? " already" : ""}" data-id="${esc(
                    it.id,
                  )}" data-ctx="${it.contextLength ?? ""}">
                    <span class="remote-id">${esc(it.id)}</span>
                    <span class="remote-meta">${already ? "已配置" : it.ownedBy ? esc(it.ownedBy) : ""}</span>
                  </button>`;
                })
                .join("")
        }
      </div>
      <div class="remote-actions">
        <button type="button" class="set-btn add" id="picker-batch">批量导入未配置的全部</button>
        <div class="set-hint">点一行 = 填入上方「接口模型 ID」；批量导入会共用当前 URL / Key。</div>
      </div>
    </div>`
    : "";

  return `
    ${subHeader("模型")}
    <div class="set-form" style="margin-bottom:8px">
      <input id="model-search" placeholder="筛选名称 / ID / URL" value="${esc(modelSearch)}" />
    </div>
    <div class="set-list">${rows || '<div class="mem-empty">还没有模型。点下方表单添加。</div>'}</div>

    <div class="mem-head">${editing ? "✏️ 编辑模型" : "➕ 添加模型"}</div>
    <div class="set-form" id="model-form">
      ${f.error ? `<div class="form-err">${esc(f.error)}</div>` : ""}
      ${
        editing
          ? `<div class="set-hint">正在编辑 <code>${esc(f.editId!)}</code> · <b>接口模型 ID</b> 才会发给提供商（request 的 <code>model</code>）</div>`
          : ""
      }
      <select id="f-preset">
        ${presetOpts}
      </select>
      <label class="fld"><span>Base URL</span>
        <input id="f-url" placeholder="提供商 Base URL（含 /v1 等）" value="${esc(f.url)}" />
      </label>
      <label class="fld"><span>API Key</span>
        <input id="f-key" type="password" placeholder="${
          editing
            ? f.clearKey
              ? "保存后将清空 Key"
              : "留空保持原 Key"
            : "本地 / Ollama 可留空"
        }" value="${esc(f.clearKey ? "" : f.apiKey)}" />
      </label>
      ${
        editing
          ? `<label class="set-check"><input type="checkbox" id="f-clear-key"${
              f.clearKey ? " checked" : ""
            } /> 清空 Key（不需要鉴权的本地模型）</label>`
          : ""
      }
      <div class="form-row-2">
        <label class="fld grow"><span>接口模型 ID（发给提供商的 model）</span>
          <input id="f-id" placeholder="deepseek-v4.1-flash" value="${esc(f.id)}" />
        </label>
        <button type="button" class="set-btn fetch-id" id="f-fetch-ids" title="GET ${esc(
          f.url.trim() ? modelsEndpointPreview(f.url.trim()) : "{Base URL}/models",
        )}">获取 ID</button>
      </div>
      <label class="fld"><span>显示名（可空，默认同 ID）</span>
        <input id="f-name" placeholder="火山agent" value="${esc(f.name)}" />
      </label>
      <div class="form-row-2 even">
        <label class="fld"><span>上下文窗口</span>
          <input id="f-max-in" placeholder="128000" value="${esc(f.maxIn)}" inputmode="numeric" />
        </label>
        <label class="fld"><span>最大输出</span>
          <input id="f-max-out" placeholder="8192" value="${esc(f.maxOut)}" inputmode="numeric" />
        </label>
      </div>
      <div class="form-row-2 even">
        <label class="set-check"><input type="checkbox" id="f-tool"${f.tool ? " checked" : ""} /> 工具调用</label>
        <label class="set-check"><input type="checkbox" id="f-img"${f.img ? " checked" : ""} /> 图片</label>
      </div>
      <details class="adv-box"${f.headersText ? " open" : ""}>
        <summary>额外请求头（可选）</summary>
        <textarea id="f-headers" rows="3" placeholder="每行一个 Key: Value">${esc(f.headersText)}</textarea>
        <div class="set-hint">OpenCode Zen 的 <code>x-opencode-session</code> 会自动补，不用手填。</div>
      </details>
      <div class="set-form-row">
        <button class="set-btn add" id="f-add">${editing ? "保存修改" : "添加"}</button>
        ${editing ? `<button class="set-btn" id="f-cancel-edit">取消</button>` : ""}
      </div>
      <div class="set-hint">Key 只存本地 <code>agent-data/models.json</code>（已 gitignore）。本地模型可不填 Key。</div>
      ${picker}
    </div>`;
}

/** 把当前 DOM 表单读进 modelForm（保存 / 重绘前都要调，避免丢输入） */
function readModelForm(): void {
  const f = modelForm;
  const val = (sel: string): string => {
    const el = document.querySelector(sel) as HTMLInputElement | HTMLTextAreaElement | null;
    return el?.value ?? "";
  };
  const chk = (sel: string, dflt: boolean): boolean => {
    const el = document.querySelector(sel) as HTMLInputElement | null;
    return el ? el.checked : dflt;
  };
  f.name = val("#f-name");
  f.id = val("#f-id");
  f.url = val("#f-url");
  f.apiKey = val("#f-key");
  f.clearKey = chk("#f-clear-key", f.clearKey);
  f.tool = chk("#f-tool", f.tool);
  f.img = chk("#f-img", f.img);
  f.headersText = val("#f-headers");
  f.maxIn = val("#f-max-in");
  f.maxOut = val("#f-max-out");
  const ps = document.getElementById("f-preset") as HTMLSelectElement | null;
  if (ps) f.preset = ps.value || "custom";
}

function setModelFormError(msg: string | undefined): void {
  modelForm.error = msg;
  renderBody();
}

/** token 数：支持 `128000` / `128k` / `1m`（1m=1000k=1000000） */
function parseTok(v: string): number | null {
  const t = v.trim().toLowerCase();
  if (!t) return null;
  const m = /^(\d+(?:\.\d+)?)([km]?)$/.exec(t);
  if (!m) {
    throw new Error(`「${v}」不是合法 token 数（可用 128000 / 128k / 1m）`);
  }
  const n = Number(m[1]);
  const unit = m[2];
  const mul = unit === "m" ? 1000_000 : unit === "k" ? 1000 : 1;
  const out = Math.round(n * mul);
  if (!Number.isFinite(out) || out < 0) {
    throw new Error(`「${v}」不是合法 token 数`);
  }
  return out;
}

function bindModelsView(root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => {
    readModelForm();
    void switchView("settings");
  });

  document.getElementById("model-search")?.addEventListener("input", (e) => {
    modelSearch = (e.target as HTMLInputElement).value;
    readModelForm();
    renderBody();
    const s = document.getElementById("model-search") as HTMLInputElement | null;
    if (s) {
      s.focus();
      s.setSelectionRange(s.value.length, s.value.length);
    }
  });

  // —— 预设切换：只填 URL ——
  document.getElementById("f-preset")?.addEventListener("change", (e) => {
    readModelForm();
    const key = (e.target as HTMLSelectElement).value;
    modelForm.preset = key;
    const p = providerPreset(key);
    if (p) modelForm.url = p.url;
    modelForm.error = undefined;
    renderBody();
  });

  // —— 从提供商获取模型 ID（填表辅助，不是另一套流程） ——
  document.getElementById("f-fetch-ids")?.addEventListener("click", async () => {
    readModelForm();
    const f = modelForm;
    const url = f.url.trim();
    const key = f.apiKey.trim();
    if (!url) {
      setModelFormError("先填 Base URL，再获取模型 ID");
      return;
    }
    // 本地网关允许无 Key；远程建议有 Key（没有也先试一次）
    try {
      const list = await invoke<RemoteModelInfo[]>("models_fetch_remote", {
        url,
        apiKey: key,
        headers: f.headersText.trim() ? f.headersText : null,
      });
      const localIds = new Set(models.map((m) => m.id));
      remoteFetch = {
        items: list,
        source: "custom",
        sourceUrl: url,
        apiKey: key,
        headersText: f.headersText,
        tool: f.tool,
        img: f.img,
        localIds,
        checked: new Set(),
        lastEndpoint: modelsEndpointPreview(url),
        error: undefined,
      };
      f.error = undefined;
      if (list.length === 0) {
        f.error = "提供商返回 0 个模型（接口可能不支持 /models）";
      }
    } catch (e) {
      remoteFetch = {
        items: [],
        source: "custom",
        sourceUrl: url,
        apiKey: key,
        headersText: f.headersText,
        tool: f.tool,
        img: f.img,
        localIds: new Set(models.map((m) => m.id)),
        checked: new Set(),
        error: String(e),
      };
      f.error = undefined;
    }
    renderBody();
  });

  document.getElementById("picker-close")?.addEventListener("click", () => {
    readModelForm();
    remoteFetch = null;
    renderBody();
  });

  root.querySelectorAll<HTMLButtonElement>(".id-pick").forEach((b) =>
    b.addEventListener("click", async () => {
      readModelForm();
      const id = b.dataset.id ?? "";
      const ctx = Number(b.dataset.ctx || 0);
      remoteFetch = null;
      // 已配置 → 直接进编辑（选中的就是这个 ID）
      if (models.some((m) => m.id === id)) {
        try {
          const ev = await invoke<ModelEditView>("models_get_edit", { id });
          modelForm = {
            editId: id,
            name: ev.name && ev.name !== id ? ev.name : "",
            id: ev.id,
            url: ev.url,
            apiKey: "",
            clearKey: false,
            tool: ev.supportsToolCall,
            img: ev.supportsImages,
            headersText: ev.headersText,
            maxIn: ev.maxInputTokens != null ? String(ev.maxInputTokens) : "",
            maxOut: ev.maxOutputTokens != null ? String(ev.maxOutputTokens) : "",
            preset: "custom",
            error: undefined,
          };
          renderBody();
          showToast(`「${id}」已在列表中，已进入编辑`, "info", 2500);
        } catch (e) {
          showToast(`读取失败：${e}`, "error");
        }
        return;
      }
      modelForm.id = id;
      if (ctx > 0) modelForm.maxIn = String(ctx);
      modelForm.error = undefined;
      renderBody();
      showToast(`已填入模型 ID：${id}`, "ok", 2500);
    }),
  );

  document.getElementById("picker-batch")?.addEventListener("click", async () => {
    readModelForm();
    const rf = remoteFetch;
    const f = modelForm;
    if (!rf) return;
    const ids = rf.items.map((x) => x.id).filter((id) => !rf.localIds.has(id));
    if (ids.length === 0) {
      showToast("没有可导入的模型（都已配置）", "info");
      return;
    }
    const contextLengths: Record<string, number> = {};
    for (const it of rf.items) {
      if (typeof it.contextLength === "number" && it.contextLength > 0) {
        contextLengths[it.id] = it.contextLength;
      }
    }
    try {
      const result = await invoke<{ added: string[]; skipped: string[] }>("models_import_remote", {
        ids,
        sourceId: null,
        url: f.url,
        apiKey: f.apiKey || null,
        headers: f.headersText.trim() ? f.headersText : null,
        supportsToolCall: f.tool,
        supportsImages: f.img,
        contextLengths,
      });
      await loadModels();
      remoteFetch = null;
      showToast(
        `已导入 ${result.added.length} 个` +
          (result.skipped.length ? `（跳过 ${result.skipped.length}）` : ""),
        "ok",
      );
    } catch (e) {
      showToast(`批量导入失败：${e}`, "error");
    }
    renderBody();
  });

  // —— 列表操作 ——
  root.querySelectorAll<HTMLButtonElement>(".set-btn.use").forEach((b) =>
    b.addEventListener("click", async () => {
      selectedModel = b.dataset.id!;
      await invoke("set_selected_model", { id: selectedModel }).catch((e) =>
        showToast(`保存失败：${e}`, "error"),
      );
      renderBody();
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".set-btn.del").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      const ok = await askConfirm("删除模型", `确定删除「${id}」？`, "删除");
      if (!ok) return;
      await invoke("models_remove", { id }).catch((e) => showToast(`删除失败：${e}`, "error"));
      if (modelForm.editId === id) modelForm = emptyModelForm();
      await loadModels();
      renderBody();
      showToast(`已删除 ${id}`, "ok", 2500);
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".set-btn.edit").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      try {
        const ev = await invoke<ModelEditView>("models_get_edit", { id });
        modelForm = {
          editId: id,
          name: ev.name && ev.name !== id ? ev.name : "",
          id: ev.id,
          url: ev.url,
          apiKey: "",
          clearKey: false,
          tool: ev.supportsToolCall,
          img: ev.supportsImages,
          headersText: ev.headersText,
          maxIn: ev.maxInputTokens != null ? String(ev.maxInputTokens) : "",
          maxOut: ev.maxOutputTokens != null ? String(ev.maxOutputTokens) : "",
          preset: "custom",
          error: undefined,
        };
        remoteFetch = null;
        renderBody();
        const hint = document.getElementById("f-key") as HTMLInputElement | null;
        if (hint && ev.hasKey) {
          hint.placeholder = `Key ${ev.keyPreview}（留空保持不变）`;
        }
      } catch (e) {
        showToast(`读取模型配置失败：${e}`, "error");
      }
    }),
  );

  document.getElementById("f-cancel-edit")?.addEventListener("click", () => {
    modelForm = emptyModelForm();
    remoteFetch = null;
    renderBody();
  });

  root.querySelectorAll<HTMLButtonElement>(".set-btn.test").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      const row = [...root.querySelectorAll<HTMLElement>(".set-row")].find(
        (r) => r.dataset.id === id,
      );
      const msg = row?.querySelector<HTMLElement>(".set-test-msg");

      const say = (text: string, cls: "run" | "ok" | "bad"): void => {
        if (!msg) return;
        msg.hidden = false;
        msg.className = `set-test-msg ${cls}`;
        msg.textContent = text;
      };

      b.disabled = true;
      const oldLabel = b.textContent;
      b.textContent = "测…";
      say(`正在测试 ${id} …`, "run");

      try {
        const reply = await invoke<string>("test_model", { id });
        say(`✅ 连通正常：${reply.slice(0, 150)}`, "ok");
        showToast(`✅ ${id} 连通正常`, "ok");
      } catch (e) {
        say(`❌ 测试失败：${e}`, "bad");
        showToast(`❌ ${id} 测试失败：${e}`, "error");
      } finally {
        b.disabled = false;
        b.textContent = oldLabel;
      }
    }),
  );

  // —— 保存（添加 / 编辑同一入口） ——
  // 唯一键 = 显示名；接口 model（f.id）允许重复（官方/火山可同 model）
  document.getElementById("f-add")?.addEventListener("click", async () => {
    readModelForm();
    const f = modelForm;
    const editing = f.editId !== null;
    const newId = f.id.trim();
    const url = f.url.trim();
    const name = f.name.trim();
    const key = f.apiKey.trim();
    const display = name || newId;

    const fail = (m: string): void => {
      f.error = m;
      renderBody();
    };

    if (!newId) return fail("接口模型 ID 不能为空（点「获取 ID」可从提供商选）");
    if (!url) return fail("Base URL 不能为空");
    if (/\s/.test(newId)) return fail("接口模型 ID 不能含空格");
    if (!display) return fail("显示名不能为空");

    let maxIn: number | null;
    let maxOut: number | null;
    try {
      maxIn = parseTok(f.maxIn);
      maxOut = parseTok(f.maxOut);
    } catch (e) {
      return fail(String(e instanceof Error ? e.message : e));
    }

    const headers = f.headersText.trim() ? f.headersText : null;

    try {
      if (editing) {
        await invoke("models_edit", {
          id: f.editId,
          newId, // 接口 model 可与其它条目相同
          name: display,
          url,
          apiKey: key || null,
          clearKey: f.clearKey,
          supportsToolCall: f.tool,
          supportsImages: f.img,
          headers,
          maxInputTokens: maxIn,
          maxOutputTokens: maxOut,
        });
        showToast(`已保存 ${display}`, "ok");
        if (selectedModel === f.editId) selectedModel = display;
        modelForm = emptyModelForm();
      } else {
        await invokeModelsAdd({
          id: newId,
          name: display,
          url,
          apiKey: key,
          supportsToolCall: f.tool,
          supportsImages: f.img,
          headers,
          maxInputTokens: maxIn,
          maxOutputTokens: maxOut,
          overwrite: true,
        });
        selectedModel = display;
        await invoke("set_selected_model", { id: display }).catch(() => {});
        modelForm = emptyModelForm();
        showToast(`已保存 ${display}`, "ok");
      }
      await loadModels();
      remoteFetch = null;
      renderBody();
    } catch (e) {
      fail(`保存失败：${e}`);
    }
  });
}

/** models_add 的参数打包（含 overwrite） */
async function invokeModelsAdd(args: {
  id: string;
  name: string;
  url: string;
  apiKey: string;
  supportsToolCall: boolean;
  supportsImages: boolean;
  headers: string | null;
  maxInputTokens: number | null;
  maxOutputTokens: number | null;
  overwrite: boolean;
}): Promise<string> {
  return await invoke<string>("models_add", args);
}

// ---------------- 设置 › 文件权限 ----------------

const ACCESS_LABEL: Record<string, string> = {
  deny: "🚫 拒绝",
  read: "👁 只读",
  readwrite: "✏️ 读写",
  full: "🔓 完全控制",
};

const ACCESS_DESC: Record<string, string> = {
  deny: "命中即拒绝（用于在放行范围内挖洞）",
  read: "读文件、列目录",
  readwrite: "读 + 写 + 改（不能删）",
  full: "含删除在内的完全控制",
};

function renderPermView(rules: PermRule[]): string {
  const rows = rules
    .map(
      (r, i) => `
      <div class="set-row${r.access === "deny" ? " deny" : ""}">
        <div class="set-name">${esc(r.prefix)}</div>
        <div class="set-url">${ACCESS_LABEL[r.access] ?? r.access}${
        r.label ? ` · ${esc(r.label)}` : ""
      }</div>
        <div class="set-actions">
          <button class="set-btn del" data-idx="${i}">删除</button>
        </div>
      </div>`,
    )
    .join("");

  const opts = ["read", "readwrite", "full", "deny"]
    .map((a) => `<option value="${a}">${ACCESS_LABEL[a]}</option>`)
    .join("");

  return `
    ${subHeader("文件权限")}
    <div class="set-hint" style="margin-bottom:8px">
      <b>最长前缀匹配</b>：越具体的路径优先。没命中任何规则 = <b>拒绝</b>。<br>
      规则保存在本地 <code>agent-data/permissions.json</code>，可手工编辑。
    </div>

    <div class="set-list">${rows || '<div class="mem-empty">还没有规则（所有文件操作都会被拒绝）。</div>'}</div>

    <div class="mem-head">➕ 添加规则</div>
    <div class="set-form">
      <input id="p-path" placeholder="目录绝对路径，如 D:\\myword" />
      <select id="p-access">${opts}</select>
      <input id="p-label" placeholder="备注（可留空）" />
      <button class="set-btn add" id="p-add">添加</button>
      <div class="set-hint" id="p-legend">${Object.entries(ACCESS_DESC)
        .map(([k, v]) => `${ACCESS_LABEL[k]} = ${esc(v)}`)
        .join("<br>")}</div>
    </div>

    <div class="mem-head">🔍 试一下</div>
    <div class="set-form">
      <input id="p-test-path" placeholder="输入路径，看当前规则下能不能访问" />
      <select id="p-test-need">
        <option value="read">需要：只读</option>
        <option value="readwrite">需要：读写</option>
        <option value="full">需要：完全控制</option>
      </select>
      <button class="set-btn" id="p-test-btn">检查</button>
      <div class="set-hint" id="p-test-result"></div>
    </div>`;
}

function bindPermView(root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));

  root.querySelectorAll<HTMLButtonElement>(".set-btn.del").forEach((b) =>
    b.addEventListener("click", async () => {
      const idx = Number(b.dataset.idx);
      const rules = await invoke<PermRule[]>("perm_rules").catch(() => [] as PermRule[]);
      const target = rules[idx];
      if (!target) return;
      await invoke("perm_remove_rule", { prefix: target.prefix }).catch((e) =>
        pushEntry("error", `删除失败：${e}`),
      );
      renderBody();
    }),
  );

  document.getElementById("p-add")?.addEventListener("click", async () => {
    const path = (document.getElementById("p-path") as HTMLInputElement).value.trim();
    const access = (document.getElementById("p-access") as HTMLSelectElement).value;
    const label = (document.getElementById("p-label") as HTMLInputElement).value.trim();

    if (!path) {
      pushEntry("error", "目录路径不能为空");
      return;
    }

    try {
      await invoke("perm_add_rule", { prefix: path, access, label });
      renderBody();
    } catch (e) {
      pushEntry("error", `添加规则失败：${e}`);
    }
  });

  document.getElementById("p-test-btn")?.addEventListener("click", async () => {
    const path = (document.getElementById("p-test-path") as HTMLInputElement).value.trim();
    const need = (document.getElementById("p-test-need") as HTMLSelectElement).value;
    const out = document.getElementById("p-test-result");
    if (!path || !out) return;
    try {
      const ok = await invoke<string>("perm_check", { path, need });
      out.innerHTML = `✅ 允许访问：<code>${esc(ok)}</code>`;
    } catch (e) {
      out.innerHTML = `❌ ${esc(String(e))}`;
    }
  });
}

// ---------------- 设置 › 命令策略 ----------------

function fmtAuditTime(ts: number): string {
  const ms = ts > 1e12 ? ts : ts * 1000;
  try {
    return new Date(ms).toLocaleString();
  } catch {
    return String(ts);
  }
}

function renderCmdPolicyView(
  policy: CmdPolicy,
  path: string,
  grants: CmdGrantsView[],
  audit: CmdAuditRecord[],
): string {
  const listArea = (id: string, items: string[], placeholder: string) => `
    <textarea id="${id}" rows="8" spellcheck="false"
      placeholder="${placeholder}"
      style="width:100%;font-family:ui-monospace,Consolas,monospace;font-size:11.5px"
    >${esc(items.join("\n"))}</textarea>`;

  const grantRows =
    grants.length === 0
      ? `<div class="mem-empty">当前没有持久命令授权。点命令卡的「记住这类 / 记住这条」即写入
         <code>command_grants.json</code>，跨会话、重启后仍生效。</div>`
      : grants
          .map(
            (g) => `
      <div class="set-row">
        <div class="set-name">${esc(g.display || g.fingerprint)}</div>
        <div class="set-url">${esc(g.scope === "prefix" ? "这类命令" : "完整命令")} · ${
          CMD_TIER_LABEL[g.tier] ?? esc(g.tier)
        }${g.reason ? ` · ${esc(g.reason)}` : ""}</div>
        <button class="set-btn cp-revoke" data-fp="${esc(g.fingerprint)}">撤销</button>
      </div>`,
          )
          .join("");

  const auditRows =
    audit.length === 0
      ? `<div class="mem-empty">还没有命令审计记录。agent 跑过 <code>run_command</code> 后会写入审计链。</div>`
      : audit
          .slice()
          .reverse()
          .map((r) => {
            const cls = r.event === "blocked" || r.risk === "high" ? " deny" : "";
            return `
      <div class="set-row${cls}">
        <div class="set-name">${esc(r.command)}</div>
        <div class="set-url">#${r.sequence} · ${esc(r.event)}/${esc(r.source)} · ${esc(r.risk)}${
          r.exit_code != null ? ` · exit ${r.exit_code}` : ""
        } · ${esc(fmtAuditTime(r.ts))}</div>
      </div>`;
          })
          .join("");

  return `
    ${subHeader("命令策略")}
    <div class="set-hint" style="margin-bottom:8px">
      管的是 <code>run_command</code>（PowerShell）。三路判定：
      <b>白名单免问</b> / <b>硬阻断</b>（危险命令连问都不问）/ <b>其余弹确认卡</b>。<br>
      规则文件：<code>${esc(path || "agent-data/command_policy.json")}</code>（也可手工改）。<br>
      命令授权是<strong>永久的</strong>：点卡片「记住这类 / 记住这条」会写进
      <code>command_grants.json</code>，跨会话、重启都还在 —— 要收回请用下面列表里的
      <b>撤销</b>。文件权限不受此影响（仍是按会话的临时授权）。
    </div>

    <div class="mem-head">✅ 白名单（免问，每行一条通配，如 <code>git status *</code>）</div>
    ${listArea("cp-allow", policy.allow ?? [], "git status\ngit status *")}

    <div class="mem-head">⛔ 硬阻断（永远拒绝，每行一条通配）</div>
    ${listArea("cp-block", policy.hard_block ?? [], "shutdown*\n*remove-partition*")}

    <div class="set-actions" style="margin-top:8px">
      <button class="set-btn add" id="cp-save">保存策略</button>
      <button class="set-btn" id="cp-reload">重新加载</button>
    </div>
    <div class="set-hint" id="cp-save-msg"></div>

    <div class="mem-head">🔍 试一下（只判定，不执行）</div>
    <div class="set-form">
      <input id="cp-test-cmd" placeholder="如 git status -s" />
      <button class="set-btn" id="cp-test-btn">判定</button>
      <div class="set-hint" id="cp-test-result"></div>
    </div>

    <div class="mem-head">🎫 永久命令授权 ${grants.length}</div>
    ${
      grants.length
        ? `<div class="set-actions" style="margin-bottom:6px">
             <button class="set-btn" id="cp-revoke-all">全部撤销</button>
           </div>`
        : ""
    }
    ${grantRows}

    <div class="mem-head">📜 命令审计（最近 ${audit.length} 条）</div>
    <div class="set-list">${auditRows}</div>`;
}

function bindCmdPolicyView(_root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));

  document.getElementById("cp-reload")?.addEventListener("click", () => renderBody());

  // 永久授权必须有「撤销」入口 —— 否则用户只能手改 command_grants.json。
  // 按项目约定：渲染后直接绑（不用 document 级委托）。
  document.querySelectorAll<HTMLButtonElement>(".cp-revoke").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const fp = btn.dataset.fp ?? "";
      if (!fp) return;
      try {
        const n = await invoke<number>("perm_cmd_grant_revoke", { fingerprint: fp });
        showToast(n > 0 ? "已撤销这条命令授权" : "这条授权已经不存在了", "ok");
        renderBody();
      } catch (e) {
        showToast(`撤销失败：${e}`, "error");
      }
    });
  });

  document.getElementById("cp-revoke-all")?.addEventListener("click", async () => {
    try {
      const n = await invoke<number>("perm_cmd_grants_clear");
      showToast(n > 0 ? `已撤销全部 ${n} 条命令授权` : "本来就没有命令授权", "ok");
      renderBody();
    } catch (e) {
      showToast(`撤销失败：${e}`, "error");
    }
  });

  document.getElementById("cp-save")?.addEventListener("click", async () => {
    const allow = ((document.getElementById("cp-allow") as HTMLTextAreaElement | null)?.value ?? "")
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    const hardBlock = ((document.getElementById("cp-block") as HTMLTextAreaElement | null)?.value ?? "")
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    const msg = document.getElementById("cp-save-msg");
    try {
      await invoke("perm_cmd_policy_set", { allow, hardBlock });
      if (msg) msg.innerHTML = "✅ 已保存到 command_policy.json（立即生效）";
      showToast("命令策略已保存", "ok");
    } catch (e) {
      if (msg) msg.innerHTML = `❌ 保存失败：${esc(String(e))}`;
      showToast(`保存命令策略失败：${e}`, "error");
    }
  });

  document.getElementById("cp-test-btn")?.addEventListener("click", async () => {    const cmd = (document.getElementById("cp-test-cmd") as HTMLInputElement | null)?.value.trim() ?? "";
    const out = document.getElementById("cp-test-result");
    if (!cmd || !out) {
      if (out) out.textContent = "请输入命令";
      return;
    }
    out.textContent = "判定中…";
    try {
      const r = await invoke<CmdVerdictView>("perm_cmd_test", { command: cmd });
      const label =
        r.verdict === "allow" ? "✅ 白名单放行" : r.verdict === "hard-block" ? "⛔ 硬阻断" : "🟨 需确认卡";
      out.innerHTML = `${label} · 风险 ${esc(r.risk)}<br>${esc(r.reason)}${
        r.normalized ? `<br>归一化：<code>${esc(r.normalized)}</code>` : ""
      }`;
    } catch (e) {
      out.innerHTML = `❌ ${esc(String(e))}`;
    }
  });
}

// ---------------- 设置 › MCP ----------------

function renderMcpView(
  mcp: McpStatus | null,
  servers: McpServerCfg[],
  grants: string[],
): string {
  const head = subHeader("MCP 外部工具");

  // ---- 多 server 列表（增删启用） ----
  const serverRows =
    servers.length === 0
      ? `<div class="mem-empty">还没有配置任何 MCP server。下面填一行再点「添加」。</div>`
      : servers
          .map(
            (s, i) => `
      <div class="set-row mcp-srv-row" data-idx="${i}">
        <div class="set-name">
          <label class="mcp-en"><input type="checkbox" class="mcp-en-cb" data-idx="${i}" ${
            s.enabled ? "checked" : ""
          }> 启用</label>
          <code class="mcp-srv-id">${esc(s.id)}</code>
          <span class="set-cur">${esc(s.label || s.id)}</span>
        </div>
        <div class="set-url"><code>${esc(s.url)}</code></div>
        <div class="set-actions">
          <button class="set-btn mcp-srv-del" data-idx="${i}">删除</button>
        </div>
      </div>`,
          )
          .join("");

  const serverBlock = `
    <div class="mem-head">🔗 MCP Server 列表</div>
    <div class="set-hint" style="margin-bottom:8px">
      可单独添加多个 server（Streamable HTTP）。未启用的保存后不会连接。
    </div>
    <div class="set-list" id="mcp-srv-list">${serverRows}</div>
    <div class="mem-head">➕ 添加一行</div>
    <div class="set-form mcp-add-form">
      <input id="mcp-new-id" placeholder="id（如 github）" spellcheck="false">
      <input id="mcp-new-label" placeholder="显示名（可空）" spellcheck="false">
      <input id="mcp-new-url" placeholder="http://127.0.0.1:3050/mcp" spellcheck="false">
      <button class="set-btn add" id="mcp-srv-add">添加</button>
    </div>
    <div class="set-actions" style="margin-top:8px">
      <button class="set-btn ok" id="mcp-srv-save">保存并重连</button>
      <button class="set-btn" id="mcp-refresh">重新拉取工具清单</button>
    </div>
    <div class="set-hint" id="mcp-srv-msg"></div>`;

  // ---- 连接状态 + 工具组 ----
  let statusBlock = "";
  if (!mcp) {
    statusBlock = `<div class="mem-empty">读取 MCP 状态失败。</div>`;
  } else if (!mcp.connected) {
    statusBlock = `<div class="mem-empty">${
      mcp.url
        ? `未连接到 ${esc(mcp.url)}${mcp.error ? `：${esc(mcp.error)}` : ""}`
        : "还没配可用的 server。"
    }${mcp.url ? "<br>检查 MCP 进程是否在运行。" : ""}</div>`;
  } else {
    const bar = `
      <div class="mcp-budget">
        <span>已加载工具占用 ~${mcp.activeTokens} tokens</span>
      </div>`;
    const rows = mcp.groups
      .map(
        (g) => `
        <div class="set-row${g.active ? " cur" : ""}">
          <div class="set-name">${esc(g.name)}${
          g.active ? ' <span class="set-cur">已加载</span>' : ""
        }${g.hasDestructive ? ' <span class="set-warn">含危险操作</span>' : ""}</div>
          <div class="set-url">${esc(g.summary)}</div>
          <div class="set-url">${g.toolCount} 个工具 · 约 ${g.tokens} tokens</div>
          <div class="set-actions">
            ${
              g.active
                ? `<button class="set-btn unload" data-group="${esc(g.name)}">卸载</button>`
                : `<button class="set-btn use" data-group="${esc(g.name)}">加载</button>`
            }
          </div>
        </div>`,
      )
      .join("");
    statusBlock =
      `<div class="set-url" style="margin-bottom:8px">已连接 ${esc(mcp.url)}</div>` +
      bar +
      `<div class="set-hint" style="margin-bottom:8px">
        这些工具**不会默认塞进模型上下文**（全量约 14k tokens）。
        模型会按需自己加载；你也可以在这里手动加载/卸载。
       </div>` +
      `<div class="set-list">${rows}</div>`;
  }

  // ---- T3：永久授权（总是允许） ----
  const grantRows =
    grants.length === 0
      ? `<div class="mem-empty">当前没有「总是允许」的 MCP 工具。点 MCP 权限卡的「总是允许」（或 Shift+Enter）即写入 <code>mcp_grants.json</code>。</div>`
      : grants
          .map(
            (t) => `
      <div class="set-row">
        <div class="set-name"><code>${esc(t)}</code></div>
        <button class="set-btn mcp-grant-revoke" data-tool="${esc(t)}">撤销</button>
      </div>`,
          )
          .join("");

  const grantsBlock = `
    <div class="mem-head">🔓 总是允许 ${grants.length}</div>
    ${
      grants.length
        ? `<div class="set-actions" style="margin-bottom:6px">
             <button class="set-btn" id="mcp-grant-clear">全部撤销</button>
           </div>`
        : ""
    }
    ${grantRows}
    <div class="set-hint" style="margin-top:6px">
      落盘 <code>mcp_grants.json</code>，跨会话、重启后仍生效。要收回就在这里撤销。
    </div>`;

  return head + serverBlock + statusBlock + grantsBlock;
}

function bindMcpView(root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));

  document.getElementById("mcp-refresh")?.addEventListener("click", async () => {
    await invoke("mcp_refresh").catch((e) => pushEntry("error", `刷新失败：${e}`));
    renderBody();
  });

  // ---- server 列表：从 DOM 收集当前编辑态 ----
  const collectServers = (): McpServerCfg[] => {
    const rows = root.querySelectorAll<HTMLElement>(".mcp-srv-row");
    return Array.from(rows).map((row) => {
      const i = Number(row.dataset.idx ?? "0");
      const cb = row.querySelector<HTMLInputElement>(".mcp-en-cb");
      const idEl = row.querySelector<HTMLElement>(".mcp-srv-id");
      const urlEl = row.querySelector<HTMLElement>(".set-url code");
      // label 从展示名取（set-cur），删掉可能的空白
      const labelEl = row.querySelector<HTMLElement>(".set-cur");
      return {
        id: idEl?.textContent?.trim() || `s${i + 1}`,
        url: urlEl?.textContent?.trim() || "",
        enabled: !!cb?.checked,
        label: labelEl?.textContent?.trim() || "",
      };
    });
  };

  // 删除一行 → 直接从 DOM 移除并重绘（未保存前只动 UI）
  root.querySelectorAll<HTMLButtonElement>(".mcp-srv-del").forEach((btn) => {
    btn.addEventListener("click", () => {
      const row = btn.closest(".mcp-srv-row");
      row?.remove();
    });
  });

  // 添加一行：先插进列表（内存/DOM），点「保存并重连」才落盘
  document.getElementById("mcp-srv-add")?.addEventListener("click", () => {
    const id = (document.getElementById("mcp-new-id") as HTMLInputElement | null)?.value.trim() ?? "";
    const label =
      (document.getElementById("mcp-new-label") as HTMLInputElement | null)?.value.trim() ?? "";
    const url = (document.getElementById("mcp-new-url") as HTMLInputElement | null)?.value.trim() ?? "";
    const msg = document.getElementById("mcp-srv-msg");
    if (!url) {
      if (msg) msg.textContent = "URL 不能为空";
      return;
    }
    if (!(url.startsWith("http://") || url.startsWith("https://"))) {
      if (msg) msg.textContent = "MCP 地址必须是 http/https URL";
      return;
    }
    const list = document.getElementById("mcp-srv-list");
    if (!list) return;
    // 空列表提示先清掉
    if (list.querySelector(".mem-empty")) list.innerHTML = "";
    const idx = list.querySelectorAll(".mcp-srv-row").length;
    const row = document.createElement("div");
    row.className = "set-row mcp-srv-row";
    row.dataset.idx = String(idx);
    row.innerHTML = `
      <div class="set-name">
        <label class="mcp-en"><input type="checkbox" class="mcp-en-cb" data-idx="${idx}" checked> 启用</label>
        <code class="mcp-srv-id">${esc(id || `s${idx + 1}`)}</code>
        <span class="set-cur">${esc(label || id || `s${idx + 1}`)}</span>
      </div>
      <div class="set-url"><code>${esc(url)}</code></div>
      <div class="set-actions">
        <button class="set-btn mcp-srv-del" data-idx="${idx}">删除</button>
      </div>`;
    row.querySelector(".mcp-srv-del")?.addEventListener("click", () => row.remove());
    list.appendChild(row);
    // 清空添加表单
    for (const fid of ["mcp-new-id", "mcp-new-label", "mcp-new-url"]) {
      const el = document.getElementById(fid) as HTMLInputElement | null;
      if (el) el.value = "";
    }
    if (msg) msg.textContent = "已加入列表，点「保存并重连」生效";
  });

  // 保存并重连
  document.getElementById("mcp-srv-save")?.addEventListener("click", async () => {
    const msg = document.getElementById("mcp-srv-msg");
    try {
      if (msg) msg.textContent = "保存并连接中…";
      await invoke("mcp_servers_save", { servers: collectServers() });
      renderBody();
    } catch (e) {
      if (msg) msg.textContent = `保存失败：${e}`;
    }
  });

  // 工具组加载/卸载（沿用）
  root.querySelectorAll<HTMLButtonElement>(".set-btn[data-group]").forEach((b) =>
    b.addEventListener("click", async () => {
      const name = b.dataset.group!;
      const isActive = b.classList.contains("unload");
      try {
        await invoke(isActive ? "mcp_unload_group" : "mcp_load_group", { name });
      } catch (e) {
        pushEntry("error", `${isActive ? "卸载" : "加载"}「${name}」失败：${e}`);
      }
      renderBody();
    }),
  );

  // ---- 永久授权撤销 ----
  root.querySelectorAll<HTMLButtonElement>(".mcp-grant-revoke").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const tool = btn.dataset.tool ?? "";
      if (!tool) return;
      try {
        const n = await invoke<number>("mcp_grant_revoke", { tool });
        showToast(n > 0 ? "已撤销这条总是允许" : "这条授权已经不存在了", "ok");
        renderBody();
      } catch (e) {
        showToast(`撤销失败：${e}`, "error");
      }
    });
  });

  document.getElementById("mcp-grant-clear")?.addEventListener("click", async () => {
    try {
      const n = await invoke<number>("mcp_grants_clear");
      showToast(n > 0 ? `已撤销全部 ${n} 条总是允许` : "本来就没有总是允许", "ok");
      renderBody();
    } catch (e) {
      showToast(`撤销失败：${e}`, "error");
    }
  });
}

// ---------------- 设置 › Token 用量 ----------------

/** 用量记录（后端 `sessions::UsageRecord`） */
interface UsageRecord {
  at: number;
  model: string;
  session: string;
  usage: TokenUsage;
}

/** epoch ms → 本地日期 `YYYY-MM-DD`。时区只有前端知道，所以分组在前端做 */
function localDateKey(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

interface DayAgg {
  date: string;
  total: number;
  prompt: number;
  completion: number;
  /** 模型 id → 当天小计 */
  models: Map<string, { total: number; prompt: number; completion: number }>;
}

/** 扁平记录 → 「本地日期 → 模型」聚合；日期倒序（最近在最上面） */
function aggregateUsage(records: UsageRecord[]): DayAgg[] {
  const byDay = new Map<string, DayAgg>();
  for (const r of records) {
    const date = localDateKey(r.at);
    let day = byDay.get(date);
    if (!day) {
      day = { date, total: 0, prompt: 0, completion: 0, models: new Map() };
      byDay.set(date, day);
    }
    day.total += r.usage.total;
    day.prompt += r.usage.prompt;
    day.completion += r.usage.completion;
    const mid = r.model || "（未知模型）";
    const m = day.models.get(mid) ?? { total: 0, prompt: 0, completion: 0 };
    m.total += r.usage.total;
    m.prompt += r.usage.prompt;
    m.completion += r.usage.completion;
    day.models.set(mid, m);
  }
  return [...byDay.values()].sort((a, b) => (a.date < b.date ? 1 : -1));
}

/** 今日消耗合计（设置入口页摘要用） */
function todayTokenTotal(records: UsageRecord[]): number {
  const key = localDateKey(Date.now());
  let n = 0;
  for (const r of records) if (localDateKey(r.at) === key) n += r.usage.total;
  return n;
}

/** 模型 id → 展示名（模型可能已删，那就退回 id） */
function modelName(id: string): string {
  return models.find((m) => m.id === id)?.name ?? id;
}

function renderUsageView(records: UsageRecord[]): string {
  const head = subHeader("Token 用量");

  if (records.length === 0) {
    return (
      head +
      `<div class="mem-empty">还没有用量记录。<br><br>
        对话时只要服务端返回 <code>usage</code>，这里就会按「每天 × 每个模型」自动记账。<br>
        <span class="set-hint">（少数网关不透传 usage，那种情况记不到 —— 不是这里坏了）</span>
      </div>`
    );
  }

  const days = aggregateUsage(records);
  const allTotal = records.reduce((a, r) => a + r.usage.total, 0);
  const weekKey = localDateKey(Date.now() - 6 * 86400_000);
  const weekTotal = records
    .filter((r) => localDateKey(r.at) >= weekKey)
    .reduce((a, r) => a + r.usage.total, 0);
  const tTotal = todayTokenTotal(records);
  const maxDay = Math.max(...days.map((d) => d.total), 1);

  const cards = `
    <div class="usage-cards">
      <div class="usage-card"><b>${fmtTokens(tTotal)}</b><i>今日</i></div>
      <div class="usage-card"><b>${fmtTokens(weekTotal)}</b><i>近 7 天</i></div>
      <div class="usage-card"><b>${fmtTokens(allTotal)}</b><i>全部</i></div>
    </div>`;

  const rows = days
    .map((d) => {
      const pct = Math.max(2, Math.round((d.total / maxDay) * 100));
      const modelRows = [...d.models.entries()]
        .sort((a, b) => b[1].total - a[1].total)
        .map(
          ([id, m]) =>
            `<div class="usage-model"><span>${esc(modelName(id))}</span><b>${fmtTokens(
              m.total,
            )}</b></div>`,
        )
        .join("");
      return `
        <div class="usage-day">
          <div class="usage-day-head">
            <span class="usage-date">${d.date}</span>
            <span class="usage-total" title="输入 ${d.prompt} · 输出 ${d.completion}">${fmtTokens(
              d.total,
            )} tok</span>
          </div>
          <div class="usage-bar"><i style="width:${pct}%"></i></div>
          ${modelRows}
        </div>`;
    })
    .join("");

  return head + cards + `<div class="usage-list">${rows}</div>`;
}

function bindUsageView(_root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));
}

// ---------------- 轻提示（toast） ----------------

/**
 * 非对话页（设置 / 记忆 / 会话）里的临时信息 —— **浮动提示，不碰消息区**。
 *
 * 为什么要有这个东西：这些页面的报错（保存失败、测试结果…）以前一律走
 * `pushEntry`，而 `pushEntry` 会 `paintMessages(panel-body)` 整块重绘。
 * 结果是：在设置页点「测」→ 模型列表被顶掉、聊天记录凭空出现在设置页里，
 * 而输入框因为 `renderBody()` 早在切到设置页时就 `display:none` 了 ——
 * 用户看到的就是"测试结果跑进聊天界面 + 输入框消失"（2026-09-20 用户反馈）。
 *
 * toast 只往里塞内容、自己超时消失，`pointer-events:none` 保证不挡点击。
 */
const TOAST_MAX = 3;

function ensureToastHost(): HTMLElement {
  let host = document.getElementById("toast-wrap");
  if (!host) {
    // renderPanel 重建 DOM 后旧的 host 会连同被删掉，这里补一个
    host = document.createElement("div");
    host.className = "toast-wrap";
    host.id = "toast-wrap";
    (document.querySelector(".panel") ?? app).appendChild(host);
  }
  return host;
}

function showToast(text: string, kind: "info" | "error" | "ok" = "info", ms?: number): void {
  const host = ensureToastHost();
  const el = document.createElement("div");
  el.className = `toast toast-${kind}`;
  el.textContent = text;
  host.appendChild(el);

  // 超出上限就丢最老的，避免连点"测"堆满屏幕
  while (host.children.length > TOAST_MAX) host.removeChild(host.firstElementChild!);

  // 强制一帧后再加 .in，否则 transition 不会触发（刚插入的元素没有起始状态）
  requestAnimationFrame(() => el.classList.add("in"));

  const life = ms ?? (kind === "error" ? 7000 : 4500);
  window.setTimeout(() => {
    el.classList.remove("in");
    window.setTimeout(() => el.remove(), 220);
  }, life);
}

function pushEntry(
  role: ChatEntry["role"],
  text: string,
  items?: AgentStep[],
  images?: string[],
  extra?: Partial<ChatEntry>,
): void {
  // ⚠️ system / error 是「临时提示」，只属于对话页。
  //    在设置/记忆/会话页里必须走 toast —— 否则会把那个页面整块重绘掉
  //    （设置页被聊天记录顶掉、输入框还是隐藏状态 = 页面看起来坏掉了）。
  if ((role === "system" || role === "error") && view !== "chat") {
    showToast(text, role === "error" ? "error" : "info");
    // 胶囊态仍要记错误 → 球变红（这条逻辑原本就有，别丢）
    if (role === "error" && mode === "orb") {
      unseenError = true;
      applyOrbState();
    }
    return;
  }

  const entry: ChatEntry = { role, text, items, images, ...extra };
  // 插话插到 running 之前（用户说话时的进度位置），不要永远垫底
  if (extra?.insertBeforeRunning) {
    const ri = entries.findIndex((x) => x.role === "running");
    if (ri >= 0) {
      entries.splice(ri, 0, entry);
    } else {
      entries.push(entry);
    }
  } else {
    entries.push(entry);
  }
  const b = document.getElementById("panel-body");
  if (b) {
    paintMessages(b);
    scrollToBottom();
  }

  // 胶囊态收到错误 → 球变红。
  // **只在胶囊态记**：面板态用户正看着对话流，红球纯属噪音；
  // 而且这个状态专门为"收起了面板、后台还在跑"这种看不见的场景准备。
  if (role === "error" && mode === "orb") {
    unseenError = true;
    applyOrbState();
  }
}

function setThinking(on: boolean): void {
  thinking = on;
  applyOrbState();
}

/**
 * 刷新「待拍板」条数 → 驱动胶囊态金色。
 *
 * 两个来源：**待处理权限申请**（事件驱动，已经在内存里）+ **待审批记忆候选**（要问后端）。
 * 两者同属"有事情等你拍板"的语义。
 */
async function refreshPendingApprovals(): Promise<void> {
  // 权限申请：事件驱动，直接读当前数组
  const permN = pendingPerms.length;
  // 记忆候选：轮询 mem_pending
  let memN = 0;
  try {
    const cands = await invoke<Candidate[]>("mem_pending");
    memN = cands.length;
  } catch {
    // 读不到就当 0：一个统计查询失败不该把球弄成有事的颜色骗人
  }

  if (permN !== pendingPermCount || memN !== pendingMem) {
    pendingPermCount = permN;
    pendingMem = memN;
    applyOrbState();
  }
}

// ---------------- 实时进度 ----------------

/** 把当前 streamText 收进 items 时间线（按轮分段），再清空 stream 区 */
function archiveStreamToItems(last: ChatEntry, tag?: string): void {
  const t = (last.streamText ?? "").trim();
  if (!t) return;
  last.items = last.items ?? [];
  last.items.push({ kind: "text", name: tag ?? null, detail: t });
  last.streamText = undefined;
  last.streamDiscarded = false;
}

/**
 * 接收 Rust 侧推上来的 agent 进度事件。
 *
 * 为什么需要：agent loop 一轮可能跑 30-60 秒（多次模型调用 + 工具执行），
 * 没有这个的话用户面对的是**纯黑盒空白**，然后答案突然砸出来。
 */
function installProgressListener(): void {
  void listen<ProgressEvent>("agent-progress", (e) => {
    const p = e.payload;

    // 「插话被送达」跟进行中气泡无关，先就地处理（改的是那条排队消息的角标）
    if (p.kind === "steer") {
      const q = entries.find((x) => x.steerId === p.id);
      if (q) q.queued = false;
      if (p.id) markSteerDelivered(p.id);
      return;
    }

    // ⚠️ 不能用 entries[len-1]：允许执行中「插话」后，队尾可能是刚排队的用户消息，
    //    那样所有进度事件都会被整批丢弃。进行中的气泡按 role 找。
    const last = entries.find((x) => x.role === "running");
    if (!last) return;
    last.items = last.items ?? [];

    switch (p.kind) {
      case "thinking":
        if (p.iteration) last.text = `正在思考…（第 ${p.iteration} 轮）`;
        // 上一轮流式正文收进时间线（保留可看），再开新轮。
        // ⚠️ 不能只「不清空」——那会把多轮正文 capAppend 成一坨（用户 2026-09-23 截图）。
        archiveStreamToItems(last);
        last.streamDiscarded = false;
        last.reasonOpen = false;
        break;

      case "status":
        // 请求链路状态（连接中/等首包/限流重试）。覆盖在标题上，
        // 否则用户会以为「没发出去」——其实 HTTP 还在飞或在退避。
        if (p.text) last.text = p.text;
        // 只有限流退避这类状态才同时落进时间线（后端也按同一条件落盘，
        // 这样刷新前后看到的是同一批条目）
        if (p.retry && p.text) {
          last.items.push({ kind: "status", name: null, detail: p.text });
        }
        break;

      case "delta":
        if (p.reasoning) {
          const tail = last.items[last.items.length - 1];
          let cur =
            last.reasonOpen === true && tail && tail.kind === "reasoning" ? tail : undefined;
          if (!cur) {
            cur = { kind: "reasoning", name: null, detail: "" };
            last.items.push(cur);
            last.reasonOpen = true;
          }
          cur.detail = capAppend(cur.detail, p.reasoning, MAX_STREAM_REASON_CHARS);
        }
        if (p.text) {
          last.streamText = capAppend(last.streamText, p.text, MAX_STREAM_TEXT_CHARS);
        }
        break;

      case "discardStream":
        last.streamDiscarded = true;
        // 这段不是最终答复，但用户要能看 → 收进时间线，stream 区留给下一轮
        // （工具改调 / 断流重试共用）。reasonOpen 收掉，避免新 reasoning 续进半截
        archiveStreamToItems(last, p.reason ? p.reason : "已改调工具");
        last.streamDiscarded = false;
        last.reasonOpen = false;
        break;

      default: {
        const item = progressItem(p);
        if (item) last.items.push(item);
        break;
      }
    }

    // 只更新「进行中」那一个气泡，不动输入框、也不重建其它消息
    // （重建会让悬停中的复制按钮反复淡入淡出 = 闪烁）
    scheduleRunPaint();
  });
}

/**
 * 「分叉」入口 —— 只画在**主聊天里、AI 完整回复**上。
 *
 * 语义（用户 2026-09-20 → 后续修正）：以这条**完整的一问一答**为界，把下一轮
 * 会喂给模型的上下文窗口（最近 6 小时 ∪ 最近 10 条）截出来开一条新线。
 * 挂在 assistant 上而不是 user 上：挂在 user 上时新会话末条是未回答的提问，
 * 接着再发一句就会拼成 user→user，模型侧轮次结构是坏的。完整轮次边界在
 * assistant 之后，从这里分叉才能干净续写。
 *
 * 下标用 `storedIdx`（磁盘下标），不能用 entries 下标 —— 后者会因为
 * running/system 条目和过滤而偏移，分叉点会稳定地错一条。
 */
function renderForkBtn(e: ChatEntry): string {
  if (e.role !== "assistant" || e.storedIdx === undefined) return "";
  if (currentSessionKind() !== "main") return "";
  return `<button type="button" class="msg-fork" data-upto="${e.storedIdx}" title="从这条 AI 回复分叉：截出「下一轮会带上的上下文窗口」（最近 6 小时 ∪ 最近 10 条）开一条新线。只有主聊天能分叉">⑂</button>`;
}

/** 当前会话类型（会话列表还没加载时按 task 处理 —— 最保守，不给出分叉入口） */
function currentSessionKind(): "main" | "task" | "fork" {
  return sessionList.find((s) => s.id === currentSessionId)?.kind ?? "task";
}

/**
 * 「重新生成」按钮 —— 挂在**已落盘的 AI 回复**上。
 *
 * 语义（用户 2026-09-22 要求）：对这条回答不满意，**丢掉它并重跑这一轮**。
 * 实现方式：把这条 assistant 及其后的所有消息截掉（回到"用户刚问完"的状态），
 * 然后把上一条用户消息重新发一遍。
 *
 * 为什么复用 `session_truncate` 而不是新写一个后端命令：
 * 截断语义完全一致（从第 upto 条起全丢），且它已带"对话进行中禁止"的硬保护。
 * 前端拿到截断后的会话，把最后一条 user 消息重新 `chat` 一遍即可 —— 不需要
 * 后端新增任何命令。
 *
 * 下标必须用 `storedIdx`（磁盘下标），理由同删除/分叉：entries 下标会被
 * running/system 这类不落盘的条目顶偏。
 */
function renderRegenBtn(e: ChatEntry): string {
  if (e.role !== "assistant" || e.storedIdx === undefined) return "";
  return `<button type="button" class="msg-regen" data-upto="${e.storedIdx}" title="重新生成这条回答：丢掉它并重跑这一轮（会重新消耗 token）">↻</button>`;
}

/**
 * 「删除」按钮 —— 挂在每条**已落盘**的消息上（用户消息和回答都给）。
 *
 * 语义（用户 2026-09-20 要求）：**删掉这条，以及它后面的全部消息**。
 * 点在用户自己的发言上，就是"把我这句话和它引出的回答一起丢掉"；
 * 点在一条回答上，则是"这次交互不要了"。
 *
 * 为什么必须用 `storedIdx`：`entries` 的下标会被 running/system/error 这些
 * **不落盘**的条目顶偏，拿 entries 下标去截盘上的 messages 会稳定地删错位置。
 * 没有 `storedIdx` 的条目（进行中的气泡、临时提示）不给按钮。
 *
 * 名字用 `msg-del` 而不是 `msg-del-msg` 之类，是为了和 `.msg-copy` / `.msg-fork`
 * 一起在 `.msg-foot` 里排布时样式规则好写。
 */
function renderDelBtn(e: ChatEntry): string {
  if (e.storedIdx === undefined) return "";
  return `<button type="button" class="msg-del" data-upto="${e.storedIdx}" title="删除这条消息以及它后面的所有消息（不可恢复）">删除</button>`;
}

/** 插话被送达 → 只改那一条气泡的角标（不整块重绘，免得又闪） */
function markSteerDelivered(id: string): void {
  const el = document.querySelector<HTMLElement>(`.msg.user[data-steer="${id}"]`);
  if (!el) return;
  el.classList.remove("queued");
  el.removeAttribute("data-steer");
  const tag = el.querySelector<HTMLElement>(".queued-tag");
  if (tag) {
    tag.textContent = "已送达";
    tag.classList.add("done");
  }
}

/**
 * 进度事件 → 时间线条目。
 *
 * 为什么要产出**结构化条目**而不是拼好的 HTML 行：时间线要保住"思考在哪一步
 * 之间"的顺序，条目得先成为数据（`AgentStep`）才能按序渲染、也才能和磁盘上
 * 那份（后端 `cap_steps` 落的）对上。HTML 由 `renderItem` 统一产出。
 *
 * `answering` 不产出条目：那是收尾信号，紧接着气泡就被最终回答替换掉了。
 */
function progressItem(p: ProgressEvent): AgentStep | null {
  switch (p.kind) {
    case "toolCall":
      return { kind: "tool_call", name: p.name ?? "?", detail: p.args ?? "" };
    case "toolResult":
      return { kind: "tool_result", name: p.name ?? "?", detail: p.preview ?? "" };
    case "toolError":
      return {
        kind: "tool_error",
        name: p.name ?? "?",
        detail: p.message ? `失败：${p.message}` : "执行失败",
      };
    default:
      return null;
  }
}

// ---------------- 图片输入 ----------------

async function grabScreen(): Promise<void> {
  try {
    const dataUrl = await invoke<string>("capture_screen");
    if (dataUrl.length > MAX_IMAGE_CHARS) {
      pushEntry("error", "截图太大，已丢弃");
      return;
    }
    pendingImages.push(dataUrl);
    renderPreview();
  } catch (e) {
    pushEntry("error", `截图失败：${e}`);
  }
}

/** 全局粘贴：面板态下 Ctrl+V 贴入剪贴板图片 */
function installPasteHandler(): void {
  document.addEventListener("paste", async (e) => {
    if (mode !== "panel") return;
    const items = e.clipboardData?.items;
    if (!items) return;

    let got = 0;
    for (const item of Array.from(items)) {
      if (!item.type.startsWith("image/")) continue;
      const file = item.getAsFile();
      if (!file) continue;
      try {
        const dataUrl = await fileToDataUrl(file);
        if (dataUrl.length > MAX_IMAGE_CHARS) {
          pushEntry("error", "剪贴板图片太大，已丢弃");
          continue;
        }
        pendingImages.push(dataUrl);
        got++;
      } catch (err) {
        pushEntry("error", String(err));
      }
    }
    if (got > 0) {
      e.preventDefault();
      renderPreview();
    }
  });
}

/** 全局拖拽：把图片文件拖进面板 */
function installDropHandler(): void {
  document.addEventListener("dragover", (e) => {
    if (mode !== "panel") return;
    e.preventDefault();
  });

  document.addEventListener("drop", async (e) => {
    if (mode !== "panel") return;
    e.preventDefault();
    const files = Array.from(e.dataTransfer?.files ?? []).filter((f) =>
      f.type.startsWith("image/"),
    );
    for (const f of files) {
      try {
        const dataUrl = await fileToDataUrl(f);
        if (dataUrl.length > MAX_IMAGE_CHARS) {
          pushEntry("error", `${f.name} 太大，已丢弃`);
          continue;
        }
        pendingImages.push(dataUrl);
      } catch (err) {
        pushEntry("error", String(err));
      }
    }
    if (files.length) renderPreview();
  });
}

// ---------------- 业务 ----------------

async function loadModels(): Promise<void> {
  try {
    models = await invoke<ModelView[]>("list_models");
    selectedModel = await invoke<string | null>("get_selected_model");
    if (!selectedModel && models.length > 0) {
      selectedModel = models[0].id;
      await invoke("set_selected_model", { id: selectedModel }).catch(() => {});
    }
  } catch (e) {
    models = [];
    entries.push({ role: "error", text: `加载模型列表失败：${e}` });
  }
}

async function loadProjects(): Promise<void> {
  try {
    projects = await invoke<ProjectRow[]>("pmem_list");
    const act = await invoke<ProjectRow | null>("pmem_get_active");
    activeProjectId = act?.id ?? null;
  } catch {
    projects = [];
    activeProjectId = null;
  }
}

async function send(): Promise<void> {
  // 忙碌中 → 走「插话」通道（排队），而不是静默吞掉用户打的字
  if (busy) {
    await sendSteer();
    return;
  }
  const input = document.getElementById("input") as HTMLTextAreaElement;
  const text = (input?.value ?? "").trim();
  const imgs = pendingImages.slice();

  // 允许"只发图不写字"
  if (!text && imgs.length === 0) return;

  if (!selectedModel) {
    pushEntry("error", "请先选择一个模型");
    return;
  }

  // 「做完后提议记忆」：发话前记一下候选数，答完对比，有新增就提示去审批
  const pendingBefore = await invoke<Candidate[]>("mem_pending").catch(
    () => [] as Candidate[],
  );

  input.value = "";
  input.style.height = "auto";
  draftInput = ""; // 发出去才算用掉，这时候草稿才能清
  pendingImages = [];
  renderPreview();

  pushEntry("user", text || "（仅图片）", undefined, imgs);

  // 先放一个「进行中」气泡，进度事件会往里建时间线条目；结束后替换成最终结果
  entries.push({ role: "running", text: "正在思考…", items: [] });
  const b0 = document.getElementById("panel-body");
  if (b0) {
    paintMessages(b0);
    b0.scrollTop = b0.scrollHeight;
  }

  busy = true;
  // 新的一轮 —— 上一轮的「本轮不再问」不该管到这一轮
  permMuteThisTurn = false;
  setThinking(true);
  setBusyUi(true);

  try {
    const run = await invoke<AgentRun>("chat", { input: text, images: imgs });
    // 去掉 running 气泡
    const idx = entries.findIndex((x) => x.role === "running");
    if (idx >= 0) entries.splice(idx, 1);
    // 以**磁盘**为准重建 entries：插话顺序、中断标记、思考过程、400 条截断
    // 全都在后端那份里，重载一次两边就对齐了（顺带解决 storedIdx 漂移）。
    // 传期望条数：写入失败时宁可保留内存内容，也不要抹掉用户刚看到的回答。
    // ⚠️ 排队的插话（还没送达的）落盘时不会写进去，不能算进期望值。
    await reloadSessionKeepScroll(
      entries.filter((x) => (x.role === "user" && !x.queued) || x.role === "assistant").length,
    );
    // 下面这两条是**临时提示**，只在内存里 —— 必须放在重载之后，否则会被冲掉
    if (run.interrupted) {
      // 区分「用户主动停止」与「轮数/上下文降级」：后者 stopReason 会说明原因
      const reason = run.stopReason && run.stopReason !== "已停止" ? run.stopReason : "";
      const canContinue = reason.includes("轮数上限") || reason.includes("上下文");
      pushEntry(
        "system",
        reason ? `⚠️ ${reason}` : "⏹ 已停止，本轮已产出的内容保留在上面",
        undefined,
        undefined,
        canContinue ? { continueRun: true } : undefined,
      );
    }
    // 没来得及送达的插话 → 塞回输入框。用户打的字不该因为"停得太快"白打。
    if (run.pendingSteers && run.pendingSteers.length) {
      refillInput(run.pendingSteers.join("\n"));
      pushEntry("system", `有 ${run.pendingSteers.length} 条插话没来得及送达，已放回输入框`);
    }
  } catch (e) {
    const msg = String(e);
    if (msg.includes("已停止")) {
      const idx = entries.findIndex((x) => x.role === "running");
      if (idx >= 0) entries.splice(idx, 1);
      pushEntry("system", "⏹ 已停止");
    } else {
      // ⚠️ 出错时**不能只删掉进行中气泡**（2026-09-22 用户报的 bug）。
      //
      // 那个气泡里装着这一轮全部的思考与工具步骤，删了之后屏幕上只剩一条红字
      // —— 用户看到的就是"他做的也都消失了"。后端现在在错误路径也会把部分
      // 结果落盘（见 `lib.rs::persist_run`），所以这里改成「以磁盘为准重载」：
      // 留下来的就是它真正做过的事，顺带拿到 `storedIdx`
      // （重新生成 / 删除 / 分叉 这些按钮要用它）。
      const idx = entries.findIndex((x) => x.role === "running");
      if (idx >= 0) entries.splice(idx, 1);
      await reloadSessionKeepScroll(
        entries.filter((x) => (x.role === "user" && !x.queued) || x.role === "assistant").length,
      );
      // 磁盘那份时间线尾部已经有后端落的红色 error 条目；
      // 只有它没落上（写盘失败等）才补一条内存错误提示，避免同一条错误显示两遍。
      const shown = entries.some((x) =>
        (x.items ?? []).some((s) => s.kind === "error" && s.detail === msg),
      );
      if (shown) {
        // 错误已经画在时间线里了，但胶囊态的"红球"信号不能丢
        if (mode === "orb") {
          unseenError = true;
          applyOrbState();
        }
      } else {
        pushEntry("error", msg);
      }
      pushEntry(
        "system",
        "若本轮超出能力，可对我说「交给 WB / 分发给 WorkBuddy」（技能 dispatch-workbuddy），会总结有效消息+附图写交接并打开；第一轮由你手动交给 WB。收起球后右键也有「分发 WB」。",
      );
    }
  } finally {
    busy = false;
    setThinking(false);
    setBusyUi(false);
  }

  // 记忆候选若在本轮增加 → 提醒去设置审批（做完后提议记忆的产品闭环）
  const pendingAfter = await invoke<Candidate[]>("mem_pending").catch(
    () => [] as Candidate[],
  );
  if (pendingAfter.length > pendingBefore.length) {
    const n = pendingAfter.length - pendingBefore.length;
    const bd = document.getElementById("panel-body");
    pushEntry("system", `有 ${n} 条记忆候选待审批（设置 › 记忆，批了才会进长期记忆）`);
    if (bd) {
      paintMessages(bd);
      bd.scrollTop = bd.scrollHeight;
    }
  }
}

/**
 * 执行中「插话」：入队，等当前轮（LLM 调用 + 工具）结束后作为追加的 user 消息送入。
 *
 * 为什么不打断：当前轮可能已经流出正文、跑了一半工具，用户只是补一句要求，
 * 不值得作废这些已经烧掉的 token。排队同样保住上下文延续 —— 它就是一条普通
 * 历史消息，模型下一轮自然看得到。
 */
async function sendSteer(): Promise<void> {
  const input = document.getElementById("input") as HTMLTextAreaElement;
  const text = (input?.value ?? "").trim();
  const imgs = pendingImages.slice();
  if (!text && imgs.length === 0) return;

  try {
    const ack = await invoke<{ id: string; queued: number }>("chat_steer", {
      input: text,
      images: imgs,
    });
    // 已经送出去了才清输入框
    input.value = "";
    input.style.height = "auto";
    draftInput = "";
    pendingImages = [];
    renderPreview();
    // 立刻上屏一条「排队中」的气泡：插到 running **之前**，不要永远垫底
    pushEntry("user", text || "（仅图片）", undefined, imgs, {
      queued: true,
      steerId: ack.id,
      steerQueuePos: ack.queued,
      // force insert-before-running handled in pushEntry via extra
      insertBeforeRunning: true,
    });
  } catch (e) {
    // 多半是当前轮刚好结束了（或队列满）→ 把文本塞回输入框，别弄丢
    const msg = String(e);
    input.value = text;
    draftInput = text;
    if (msg.includes("没有进行中的对话")) {
      // run 已经结束 → 现在它是"新的一轮"，直接按普通发送走
      void send();
      return;
    }
    pushEntry("system", `插话没送出去：${msg}`);
  }
}

/** 「停止」：请求中断当前轮 */
async function stopRun(): Promise<void> {
  const btn = document.getElementById("btn-stop") as HTMLButtonElement | null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "…";
    btn.title = "正在停止…";
  }
  await invoke("chat_cancel").catch(() => {});
  const run = entries.find((x) => x.role === "running");
  if (run) {
    run.text = "正在停止…";
    const bd = document.getElementById("panel-body");
    if (bd) {
      patchRunning(bd);
      bd.scrollTop = bd.scrollHeight;
    }
  }
}

/** 忙碌态下的按钮布局：显示 ■、输入框提示改成"插话" */
function setBusyUi(on: boolean): void {
  const stop = document.getElementById("btn-stop") as HTMLButtonElement | null;
  const sendBtn = document.getElementById("btn-send") as HTMLButtonElement | null;
  const input = document.getElementById("input") as HTMLTextAreaElement | null;
  if (stop) {
    stop.hidden = !on;
    if (on) {
      stop.disabled = false;
      stop.textContent = "■";
      stop.title = "停止生成";
    }
  }
  if (sendBtn) {
    sendBtn.disabled = false;
    sendBtn.title = on ? "插话（排队，当前轮结束后送达）" : "发送";
  }
  if (input) {
    input.placeholder = on
      ? "插话…（会排队，当前轮结束后送达；Enter 发送 / Shift+Enter 换行）"
      : "问点什么…（Enter 发送 / Shift+Enter 换行 / Ctrl+V 贴图）";
  }
}

/** 把文本放回输入框（中断后没送达的插话） */
function refillInput(text: string): void {
  const input = document.getElementById("input") as HTMLTextAreaElement | null;
  if (!input) return;
  const cur = input.value.trim();
  input.value = cur ? `${text}\n${cur}` : text;
  draftInput = input.value;
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 120)}px`;
}

/**
 * 回答落地后重载会话。
 *
 * 为什么需要：落盘的是**后端**那一份（含插话顺序、中断标记、400 条截断），
 * 前端的 `entries` 只是它的一份近似。重载一次，两边就对齐了。
 * 用户在往上翻历史时不要把他拽到底 —— 所以只有原本贴底才滚。
 *
 * `expect` 是"内存里已有多少条落盘消息"。若磁盘那份反而更少，说明这次写入
 * 失败了（`append_run` 出错只打日志），此时**保留内存里的**，别把用户刚看到的
 * 回答从界面上抹掉。
 */
async function reloadSessionKeepScroll(expect = 0): Promise<void> {
  const bd = document.getElementById("panel-body");
  const atBottom = bd ? bd.scrollHeight - bd.scrollTop - bd.clientHeight < 60 : true;
  try {
    const st = await invoke<SessionState>("session_state");
    const fresh = sessionToEntries(st.session);
    if (fresh.length < expect) {
      console.warn(
        `[float-agent] 会话重载后发现磁盘更旧（${fresh.length} < ${expect}），保留内存内容`,
      );
      return;
    }
    currentSessionId = st.session.id;
    sessionList = st.list;
    entries = fresh;
    // ⚠️ 只有停在对话页才重绘：一轮跑完时用户可能正开着设置页，
    //    这里不设防就会把设置页整块换成聊天记录（输入框还是隐藏的，页面像坏了）
    if (bd && view === "chat") {
      paintMessages(bd);
      if (atBottom) bd.scrollTop = bd.scrollHeight;
    }
  } catch (e) {
    console.warn(`[float-agent] 重载会话失败：${e}`);
  }
}

// ---------------- 形态切换 ----------------

async function applyMode(next: Mode): Promise<void> {
  // ⚠️ 不要用 busy 挡形态切换：收起面板是纯 UI 动作，
  //    请求会继续在后台跑，结果照常写进 entries，下次展开就能看到。
  //    （早先这里 `if (busy) return` 导致"请求中收不起面板"）
  if (next === mode) return;

  await invoke("set_window_mode", { mode: next });
  mode = next;

  if (next === "panel") {
    // 打开面板 = 必然看到对话流 → 红球使命结束
    unseenError = false;
    panelOpenedAt = Date.now();
    await ensureSessionLoaded(); // 会话从磁盘恢复（只做一次）
    await loadModels();
    await loadProjects();
    // ⚠️ 不要 bindCurrentSession —— 切/开面板改挂靠是主聊天被塞进 redis 的根因
    await refreshSessionProjectTags();
    renderPanel();
  } else if (next === "menu") {
    renderMenu();
  } else {
    // 收起时把视图重置回对话：
    // 否则下次左键展开会停在上次的「设置」或「记忆」页 ——
    // 左键的语义就是「进来聊天」，要视图选择请走右键菜单。
    view = "chat";
    // ⚠️ 顺序铁律：必须**先换 DOM，再拉数据**。
    //    `set_window_mode` 上一行已经把窗口缩到 56x56 了 —— 中间插任何 await，
    //    都会出现"小窗口里还残留着面板"的一帧：用户看到的就是一个深色圆角块，
    //    里面是被裁掉的「float-…」（面板标题栏）。这个残影曾被用户截图报过。
    renderOrb();
    void refreshPendingApprovals(); // 异步补正金色，不等它
  }
}

/** 左键点球：进对话（不管上次停在哪一页） */
async function expand(): Promise<void> {
  view = "chat";
  await applyMode("panel");
}

async function collapse(): Promise<void> {
  // 收起会把整个 DOM 换成球（`renderOrb` 覆盖 app.innerHTML），先把手打的字捞出来 ——
  // 否则"打了半句话、出去看一眼、回来"内容就没了。
  const input = document.getElementById("input") as HTMLTextAreaElement | null;
  if (input) draftInput = input.value;
  await applyMode("orb");
}

// ---------------- 渲染：菜单态 ----------------

/**
 * 右键菜单。
 *
 * ⚠️ 为什么不塞进球态窗口：球只有 84x84 物理像素，菜单会被裁成一角。
 *    所以菜单有**独立的窗口形态**（Rust 侧 `menu`：196x148 逻辑像素），
 *    位置右对齐胶囊、向左展开，点菜单项才切到面板。
 */
function renderMenu(): void {
  app.innerHTML = `
    <div class="ctx-card">
      <button data-act="settings"><span>⚙</span>设置 / 模型</button>
      <button data-act="mem"><span>🧠</span>记忆</button>
      <button data-act="chat"><span>💬</span>对话</button>
      <button data-act="dispatch"><span>↗</span>分发 WB</button>
      <button data-act="quit" class="danger"><span>⏻</span>退出</button>
    </div>
  `;

  app.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
    b.addEventListener("click", async (e) => {
      e.stopPropagation();
      const act = b.dataset.act!;

      // 悬浮球设了 skipTaskbar（不占任务栏），所以必须给一个正常退出入口，
      // 否则用户只能去任务管理器杀进程
      if (act === "quit") {
        await invoke("app_quit").catch(() => {});
        return;
      }

      if (act === "dispatch") {
        try {
          const meta = await invoke<{
            path: string;
            images?: string[];
            messageCount?: number;
          }>("dispatch_create", {
            title: null,
            summary: null,
            reason: "用户在悬浮窗菜单选择分发给 WorkBuddy（第一轮人工投递）",
            context: "",
            open: true,
          });
          view = "chat";
          await applyMode("panel");
          renderBody();
          const imgs = meta.images?.length ?? 0;
          pushEntry(
            "system",
            `已总结会话有效消息（${meta.messageCount ?? 0} 条，图片 ${imgs} 张）并写好 WB 交接，已尝试打开：\n${meta.path}\n第一轮请手动把文件内容/图片交给 WorkBuddy；自动化投递属第二轮。`,
          );
          const bd = document.getElementById("panel-body");
          if (bd) {
            paintMessages(bd);
            bd.scrollTop = bd.scrollHeight;
          }
        } catch (err) {
          view = "chat";
          await applyMode("panel");
          renderBody();
          pushEntry("error", `分发失败：${err}`);
        }
        return;
      }

      // 先定视图、再展开 —— 避免走 switchView 的 toggle 语义（同视图会翻回对话）
      view = act as View;
      await applyMode("panel");
      renderBody();
    }),
  );
}

// ---------------- 右键菜单触发 ----------------

function installContextMenu(): void {
  document.addEventListener("contextmenu", async (e) => {
    const t = e.target as HTMLElement;
    e.preventDefault();
    e.stopPropagation();

    // 消息正文上右键 → 弹复制菜单。
    // 为什么需要：Tauri 里 WebView 的默认右键菜单是关掉的（我们自己
    // preventDefault 了 document 的 contextmenu），不自己做菜单，
    // 用户选中文字后右键就是"没反应"——这正是「AI 消息没法复制」的观感来源。
    const bodyEl = t.closest<HTMLElement>(".msg-body, pre.md-code, code");
    if (bodyEl) {
      const target =
        bodyEl.closest<HTMLElement>(".msg-body") ??
        (bodyEl.closest(".md-code-wrap")?.querySelector<HTMLElement>("pre") ?? bodyEl);
      showCopyMenu(e.clientX, e.clientY, target);
      return;
    }

    closeCopyMenu();
    if (busy) return;

    // 只有胶囊态需要"弹菜单"；面板态内右键直接忽略（已经在面板里了）
    if (mode === "orb") {
      await applyMode("menu");
    }
  });

  // 点菜单外的任何地方 → 关掉（回胶囊 / 关复制菜单）
  document.addEventListener("mousedown", (e) => {
    const t = e.target as HTMLElement;
    if (copyMenuEl && !t.closest(".copy-menu")) closeCopyMenu();
    if (mode !== "menu") return;
    if (!t.closest(".ctx-card")) void applyMode("orb");
  });

  window.addEventListener("blur", () => {
    if (mode === "menu") void applyMode("orb");
    // 面板失焦自动收起 —— 悬浮球的本分是用完让路，不该一直挡着屏幕。
    // 展开后 500ms 内的 blur 忽略（展开动画/set_focus 时序抖动）。
    if (mode === "panel" && blurCollapse && Date.now() - panelOpenedAt > 500) {
      void collapse();
    }
  });
}

// ---------------- 启动 ----------------

function boot(): void {
  mode = "orb";
  renderOrb();

  // 双保险：Rust 侧 setup 已经应用过一次，这里再确认一次
  void invoke("set_window_mode", { mode: "orb" }).catch((e) =>
    console.error("[float-agent] 初始化形态失败:", e),
  );

  installPasteHandler();
  installDropHandler();
  installContextMenu();
  installCopyHandlers();
  installForkHandlers();
  installDeleteHandlers();
  installRegenHandlers();
  installProgressListener();
  installPermHandlers();
  startPermTimer();

  // 待拍板条数（记忆候选 / 权限申请）：启动拉一次 + 每 5s 轮询。
  // 读的是本地小 JSONL，成本可忽略；这是胶囊态"金色 = 有事情等你拍板"的唯一数据源。
  void refreshPendingApprovals();
  window.setInterval(() => void refreshPendingApprovals(), 5000);

  // 启动自检要先于会话预热：needsInit 决定了对话空态渲染成"引导语"还是"初始化提示"
  void checkBoot().then(() => ensureSessionLoaded());

  window.addEventListener("keydown", (e) => {
    // 有申请卡时键盘优先接管（决策 9）：Enter 批准 / Esc 拒绝 / Shift+Enter 本轮不再问。
    // ⚠️ 焦点在输入框里就**不接管** —— 否则你打字时敲个回车就把权限批了。
    // 正在写拒绝理由时不接管：那时 Enter/Esc 属于那个输入框（见 bindPermButtons），
    // 否则用户在"写理由"状态下点开别处再敲回车，会把申请**批准**掉。
    if (pendingPerms.length > 0 && !isTypingInInput() && !denyReasonFor) {
      const top = pendingPerms[0];
      if (e.key === "Enter") {
        e.preventDefault();
        // MCP 卡：Shift+Enter =「总是允许」（跨会话持久），不是「本轮不再问」
        if (top.kind === "mcp" && e.shiftKey) {
          void decidePerm(top.id, "always");
          return;
        }
        const mute = e.shiftKey;
        if (mute) permMuteThisTurn = true;
        void decidePerm(top.id, mute ? "deny" : "approve");
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        void decidePerm(top.id, "deny");
        return;
      }
    }

    if (e.key !== "Escape") return;
    if (copyMenuEl) {
      closeCopyMenu();
      return;
    }
    if (mode === "menu") {
      void applyMode("orb");
    } else if (mode === "panel" && !busy) {
      // 逐级返回：子页 → 设置入口 → 对话 → 收起面板
      if (view === "models" || view === "mcp" || view === "perm" || view === "cmdpolicy" || view === "search") {
        void switchView("settings");
      } else if (view === "sessions") {
        void switchView("chat");
      } else if (view === "settings") {
        void switchView("chat");
      } else {
        void collapse();
      }
    }
  });
}

boot();