/**
 * orbcat — 悬浮球前端
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
import { mdToHtml, splitStable } from "./markdown";
import { MAX_BUBBLES, splitBubbles } from "./bubble";
import { clipLines } from "./truncate";
// 定时器回调的环境守卫（见 dom-alive.ts 头部说明）。
import { domAlive } from "./dom-alive";
// 纯格式化/工具函数已抽到 format.ts（拆模块，见该文件头部说明）。
// 这里集中 re-import，避免散落各处 import 语句。
import {
  aggregateUsage,
  cacheHitRate,
  cacheVerdict,
  todayTokenTotal,
  type UsageRecord,
} from "./usage";
import {
  bindRecoveryView,
  recoverySummaryText,
  renderRecoveryView,
  type RecoveryState,
} from "./views/recovery";
import {
  closeCommandPanel,
  currentCommand,
  moveActive,
  newPanelState,
  openCommandPanel,
  type CommandPanelState,
} from "./commands";
import {
  capAppend,
  esc,
  fmtAuditTime,
  fmtTime,
  fmtTokens,
  isTypingInInput,
  leafName,
  localDateKey,
  MODELS_INTENT_KEY,
  shortHost,
  stripStepFolds,
  summarizeArgs,
} from "./format";

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

/** 模型身份 = (Base URL, 接口 id)；同 id 可跨组共存，显示名已废弃 */
interface ModelView {
  id: string;
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


interface AgentStep {
  kind: string;
  name: string | null;
  detail: string;
  /**
   * 工具执行心跳（"⏳ 已运行 12s｜<输出尾巴>"）—— 只在 live 进行中气泡里用。
   * 独立字段而不是塞 detail：tool_call 的 detail 是参数 JSON（要过 summarizeArgs），
   * 混进心跳文本会把摘要搅掉。
   */
  tick?: string;
  /**
   * **工具耗时（毫秒）** —— 只对 tool_call / tool_result / tool_error 有意义。
   *
   * 为什么单开字段：耗时是**元信息**，不是结果内容。混进 `detail` 会污染
   * 结果预览（detail 要原样展示给用户，还要过 summarizeArgs）。
   * 后端 pi 路径取 `tool_execution_end.durationMs`，自研内核自己掐表。
   */
  durationMs?: number;
  /**
   * 插话（kind === "steer"）附带的图片**文件路径**。
   *
   * 为什么单独一个字段：steer 的 detail 是插话文字，图不能混进去（会污染文本）。
   * 落盘时存在 user 消息的 `images` 里，读回来时由后端补到 steer 条目上
   * （见 `sessions.rs` 的 `attach_steer_images`）。
   */
  images?: string[];
  /**
   * 工具返回预览 —— **只有已配对的 `tool_call` 条目**才有这个字段。
   *
   * 为什么存在：后端落盘是 `tool_call` + `tool_result` 两条（`agent.rs` 工具循环），
   * 时间线上就是两行（`🔧 列目录` / `✅ 列目录`）。设计要求是
   * **一条工具只占一行**，所以前端读盘 / 收流时用 `pairToolSteps()` 把返回并进
   * 调用条目里（`detail` 仍是参数），渲染成一行、点开才看参数与返回。
   *
   * ⚠️ 只在**前端**合并，不落盘 —— `AgentStep` 的磁盘形态（Rust `StoredStep`）
   *    保持 kind/name/detail 三条；合并发生在前端的内存条目上。
   */
  result?: string;
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
  // ---- ask_user（kind = "ask"）----
  /** 问题正文 */
  question: string;
  /** 快捷选项（点一下即回答；空 = 自由输入） */
  options: string[];
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
   * 思考与工具天然被分到两处、不可能交错 —— 「思考都堆在一块」
   * 这个症状就是这个结构造成的。现在条目自带顺序。
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
  /** **未跑完**：进程在轮内被关掉，磁盘上留下的是半截轮（见 StoredMsg.partial） */
  partial?: boolean;
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
  /**
   * **未定稿**的助手消息（"退出不丢轮"机制）。
   *
   * 进程在轮内被关掉时，磁盘上留下的是"跑到一半"的占位消息：它带着已产出的
   * 思考与工具步骤，但没有最终正文。UI 要明确告诉用户"这轮没跑完"，
   * 否则会被当成正常回答。
   */
  partial?: boolean;
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
  /**
   * 这条会话**自己的** Agent 模式 id。
   *
   * 约定："模式跟会话走，一个会话模式不能变"。建会话时定死，之后只读。
   * 老会话（升级前建的）磁盘上没这个字段 → Rust 侧兜到内置默认
   * （`modes::DEFAULT_MODE_ID` = standard），所以这里在切到老会话后可能短暂是
   * undefined —— 真正判定模式一律走 `currentModeId()`（它自己带兜底）。
   */
  mode?: string;
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
  /** 这条会话的模式 id（Rust `SessionMeta.mode` 必给：老会话回退成默认模式） */
  mode: string;
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
 * 记忆页的**待批条数**（标题条徽标 + 底部状态栏用）。
 *
 * 为什么缓存而不是每次现拉：标题条在 `#panel-body` 之外，
 * 渲染顺序是「先 body 后 sub-bar」，body 拿到数据时标题条已经画完了 ——
 * 缓存一份给标题条用，同一份数据不用拉第二次。
 */
let memPendingCount = 0;
/** 记忆页统计（底部状态栏「N / M」） */
let memStatsCache: MemStats | null = null;
/** 设置页「搜索设置」是否展开（设计稿 04 右组 🔍 + 底栏「/ 搜索设置」） */
let settingsSearchOpen = false;
/** 设置页搜索关键词 */
let settingsSearchQ = "";
/** 设置页统计（底部状态栏「N 组 · M 项」）—— 设置页渲染完从 DOM 数出来 */
let settingsStatCache = { groups: 0, items: 0 };
/** 工具页统计（底部状态栏「N 组 · M 个工具」） */
let toolsStatCache = { groups: 0, tools: 0 };
/** MCP 组的工具数（底部状态栏要用 —— DOM 里数不到，那是折叠组） */
let mcpToolsCount = 0;
/**
 * **内置工具总闸**（settings.builtinToolsEnabled）—— 设计稿 09「内置工具」组的开关。
 *
 * 关掉 = 模型工具表里一个内置工具都没有（只剩 MCP 外部工具与项目脚本工具）。
 */
let builtinToolsEnabled = true;

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
 *
 * ⚠️ 思考过程**不设上限**：
 * 以前这里是 20_000 字，配合落盘的 8KB 预算，导致思考显示得
 * 比模型真实产出的少一大截（症状："思考过程明显不止这么少"）。
 * 现在思考一行不截。正文仍保留上限 —— 它的完整版会由后端 `run.answer` 重建，
 * 显示层截断不影响最终结果。
 */
const MAX_STREAM_TEXT_CHARS = 60_000;



/** 把磁盘上的会话消息转成面板的 entries */
function sessionToEntries(s: Session): ChatEntry[] {
  const out: ChatEntry[] = [];

  // 插话附带的图：插话消息本身**不铺独立气泡**（见下），但它的图必须画在
  // 时间线的 `kind=steer` 条目上 —— 否则插话里塞的图永远看不到
  // 。这里先按 steerId 收集，落到后面回填。
  const steerImages = new Map<string, string[]>();
  for (const m of s.messages) {
    if (m.steerId && m.images && m.images.length) {
      steerImages.set(m.steerId, m.images);
    }
  }

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
    // 时间线条目：给 steer 条目补上插话的图（按 steerId = 条目 name 对齐）；
    // 再把 `tool_call` + 同名 `tool_result` 并成一条（一个工具只占一行）。
    const items = pairToolSteps(
      m.steps?.map((st) =>
        st.kind === "steer" && st.name && !(st.images && st.images.length)
          ? { ...st, images: steerImages.get(st.name) }
          : st,
      ),
    );
    out.push({
      role: m.role,
      text: stripStepFolds(m.text),
      // 磁盘上的 steps 就是时间线（后端保序落盘），直接当条目用
      items,
      images: m.images,
      reasoning: m.reasoning,
      interrupted: m.interrupted,
      partial: m.partial,
      model: m.model,
      usage: m.usage,
      // 磁盘下标：分叉点要用它，不能拿 entries 下标顶替
      storedIdx: idx,
    });
  });
  return out;
}

/**
 * `agent-data/` 到底存不存在（Rust `boot_state` 告知）。
 *
 * ⚠️ 别再拿它反推别的结论。这屏的判据是**骨架标记**（`.skeleton-state.json`），
 * 而目录本身在启动时就被 `resolve_data_dir` 建出来了 —— 所以
 * 「目录不存在」这句话在这屏里**曾经是假的**（2026-10-14 用户实测）。
 * 这个字段只用来让文案说实话用。
 */
let dataDirExists = true;

/** 启动自检：agent-data 不存在 / 没有模型 → 标记需要引导 */
async function checkBoot(): Promise<void> {
  try {
    const st = await invoke<{
      needsInit: boolean;
      hasModels: boolean;
      dataDir: string;
      dataDirExists?: boolean;
    }>("boot_state");
    needsInit = st.needsInit;
    hasModels = st.hasModels;
    // 老版本后端没这个字段 → 保守当成"存在"，宁可不说"不存在"也不说假话
    dataDirExists = st.dataDirExists ?? true;
    if (needsInit) {
      console.warn("[orbcat] 数据目录还没有骨架，需要初始化:", st.dataDir);
    } else if (!hasModels) {
      console.warn("[orbcat] 还没有配置任何模型");
    }
  } catch (e) {
    // 自检失败不该挡住正常使用 —— 保守起见当作"不需要初始化"
    console.error("[orbcat] 启动自检失败:", e);
  }
}

async function loadSession(): Promise<void> {
  try {
    const st = await invoke<SessionState>("session_state");
    currentSessionId = st.session.id;
    sessionList = st.list;
    entries = sessionToEntries(st.session);
    // 模式跟会话走 → 启动时按**当前这条会话**的模式拉一次（而不是全局默认）。
    // 必须 await：`renderMessagesInner` 里的 `isChattyMode()` 要它 ——
    // 拿默认模式顶替的话，闲聊会话的历史消息会画成一整块而不是泡泡。
    await loadModes(currentSessionId);
  } catch (e) {
    console.error("[orbcat] 读取会话失败:", e);
  }
}

function ensureSessionLoaded(): Promise<void> {
  sessionLoaded ??= loadSession();
  return sessionLoaded;
}

/**
 * 开新会话 → 挂到**当前激活项目**（「无任务空间」则不绑）。
 *
 * `mode` = 这条会话的模式 id（模式跟会话走）。不传就用
 * `defaultModeId` —— 它是**编译期常量级别的"标准模式"**，不是用户可配项
 * （`settings.activeMode` 已删：界面上没有改它的入口，留着就是个会撒谎的死字段）。
 * 「新建项目…」这类非显式选模式的路径自然也落在标准模式上。
 *
 * ⚠️ 模式**只在建的时候**给，建完就钉死（"一个会话模式不能变"）——
 * 所以这个参数**没有**对应的"改"接口，别在这里加"切模式"的分支。
 */
async function newSession(mode?: string): Promise<void> {
  // 多会话并行 run：新建会话**放行**（后端也不拦）—— 新会话不碰别的会话，
  // 而且你很可能正是因为"这边跑着、我还要开个新任务"才点它。
  try {
    const want = mode ?? defaultModeId ?? "standard";
    const s = await invoke<Session>("session_new", { mode: want });
    currentSessionId = s.id;
    entries = [];
    await refreshSessionList();
    // 新会话的模式可能跟上一个不同 → 模式缓存（含 MCP 组视图）要跟着这条会话重拉
    await loadModes(s.id);
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
    syncBusyUi();
    showToast(`已新建「${currentModeName()}」会话`, "ok", 2200);
  } catch (e) {
    pushEntry("error", `新建会话失败：${e}`);
  }
}

/** 进入会话：顶栏项目跟着会话走（主聊天 → 无任务空间） */
async function switchSession(id: string): Promise<void> {
  // ⚠️ 不再用 `busy` 拦住切换（切走、后台继续跑）。
  //    落盘路径自带开跑时的 session_id，回答不会写错会话；前端进度监听也按
  //    `runningSessionId !== currentSessionId` 丢掉旧会话的事件（不会污染当前视图）。
  if (id === currentSessionId) {
    // 点自己：**不重载消息**（会冲掉正在流式的气泡），
    // 但**必须把视图切回对话** ——
    // 2026-10-14 用户报：会话历史页里点当前那条会话**完全没反应**，
    // 像是进不去。早退原本只该拦"重载"，却把 `view = "chat"` 一起拦掉了，
    // 于是停在会话历史页出不来。会话管理入口加进设置后这条路径更容易撞上。
    if (view !== "chat") {
      view = "chat";
      renderBody();
      syncBusyUi();
      syncJumpButton();
    }
    return;
  }
  try {
    const s = await invoke<Session>("session_switch", { id });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    // 切回**正在跑的**会话时：磁盘上有一条 `partial` 占位（begin_turn 落的），
    // 它会和随后由进度事件新建的"进行中"气泡把同一条进度画两遍 → 丢掉这条占位，
    // 交给实时气泡接手（跑完后那次 reload 会用定稿版本重建）。
    if (runningSessions.has(id) && entries.length && entries[entries.length - 1].partial) {
      entries.pop();
    }
    await refreshSessionList();
    // 模式跟会话走 → 切会话可能换了模式，模式缓存（含 MCP 组视图）要重拉。
    // ⚠️ 必须 `await`：`isChattyMode()` 依赖它，慢了会把上一条会话的
    //    「闲聊/非闲聊」判定带进这一条（泡泡会画错）。
    await loadModes(id);
    await loadProjects();
    await refreshSessionProjectTags();
    // 顶栏跟着**会话**走：主聊天永不挂项目 → 记忆回「无任务空间」
    activeProjectId = sessionProjectId(sessionList.find((x) => x.id === id));
    await invoke("pmem_set_active", { id: activeProjectId }).catch(() => {});
    paintProjectSelect();
    // 计数是**按当前视图**的 —— 换会话就清零，别把上一条的"漏看 N 条"
    // 挂到这一条上（用户看到按钮上的 3 会以为这条有 3 条没看）
    chatMissed = 0;
    view = "chat";
    renderBody();
    syncBusyUi(); // 切换了会话 → busy / 停止按钮 / 提示条都要跟着这个会话走
    syncJumpButton();
  } catch (e) {
    pushEntry("error", `切换会话失败：${e}`);
  }
}

/**
 * 「别的会话正在后台跑」提示条。
 *
 * 多会话并行 run：切走后**别的**会话还在跑，它们的结果落各自的会话。
 * 不给提示的话，用户在 B 里看不到 A 的动静会以为任务停了。点这条切过去看。
 */
function updateBgRunBanner(): void {
  const el = document.getElementById("bg-run");
  if (!el) return;
  const others = [...runningSessions].filter((id) => id !== currentSessionId);
  if (others.length === 0) {
    el.style.display = "none";
    el.textContent = "";
    el.onclick = null;
    return;
  }
  const names = others
    .map((id) => `「${sessionList.find((s) => s.id === id)?.title || "未命名"}」`)
    .join("、");
  el.style.display = "block";
  el.textContent =
    others.length === 1
      ? `⏳ ${names} 正在后台运行 · 点此切回`
      : `⏳ ${others.length} 个会话在后台运行：${names} · 点此切回`;
  const next = others[0];
  el.onclick = () => void switchSession(next);
}

/**
 * 从主聊天的第 `upto` 条消息处分叉出一个新会话。
 *
 * 新会话拿到的是**这一段上下文窗口**（最近 6 小时 ∪ 最近 10 条，和下一轮回灌
 * 给模型的完全一致），不是到那条为止的全部历史 —— 之后两边各走各的。
 */
async function forkSession(upto: number): Promise<void> {
  if (busy) {
    showToast("该会话正在运行，先停止或等它结束再分叉", "error");
    return;
  }
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
  /**
   * 第三级提示行（设计稿 11:2:1594）。
   * 「如果只是想让它别再占地方，用「保留策略」批量…」——
   * 给用户一条**更安全的替代路径**，别只说「不能撤」。
   */
  hint = "",
): Promise<boolean> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "confirm-mask";
    mask.innerHTML = `
      <div class="confirm-card">
        <div class="confirm-title">${esc(title)}</div>
        <div class="confirm-body">${esc(body)}</div>
        ${hint ? `<div class="confirm-hint">${esc(hint)}</div>` : ""}
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
 * （askInput 已随「模型管理」一起搬进独立窗口 models.ts —— 面板里没有
 * 单行输入弹窗的需求了，组重命名/改Key 都发生在那边。）
 */

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
 * 「编辑」按钮的事件委托 —— 与删除同款挂法（document 委托，重绘不用重绑）。
 */
function installEditHandlers(): void {
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const btn = t.closest<HTMLElement>(".msg-edit");
    if (!btn) return;
    e.stopPropagation();
    const upto = Number(btn.dataset.upto);
    if (!Number.isFinite(upto) || upto < 0) return;
    void editUserMessage(upto);
  });
}

/**
 * 编辑并重发一条用户消息：确认 → 截断（含其后全部回复）→ 原文放回输入框。
 *
 * 为什么要确认框：它会连坐删掉后面的 AI 回复且不可恢复 —— 与「删除」同级
 * 危险，只是多还你一步「原文放回输入框」。`refillInput` 在输入框已有草稿时
 * 是**追加**不是覆盖，不会吃掉用户正在打的字。
 */
async function editUserMessage(upto: number): Promise<void> {
  if (busy) {
    showToast("对话进行中，先停止或等它结束再编辑消息", "error");
    return;
  }
  // 要丢弃的条数（磁盘上 [upto..]）；读不到就按"含本条"最少 1 条提示
  let total = 1;
  try {
    const st = await invoke<SessionState>("session_state");
    total = Math.max(1, st.session.messages.length - upto);
  } catch {
    // 读不到会话状态就按最少 1 条提示，不拦操作
  }
  const who = entries.find((e) => e.storedIdx === upto);
  const head = (who?.text ?? "").replace(/\s+/g, " ").slice(0, 40);
  const ok = await askConfirm(
    "编辑并重发这条消息？",
    `「${head}${who && who.text.length > 40 ? "…" : ""}」\n它后面的 ${
      total - 1
    } 条回复会被一并丢弃，原文放回输入框。此操作不可恢复。`,
    "编辑并重发",
  );
  if (!ok) return;
  try {
    const s = await invoke<Session>("session_truncate", { upto });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    await refreshSessionList();
    view = "chat";
    renderBody();
    if (who?.text) refillInput(who.text);
    showToast("已截断，原文已放回输入框（改完回车重发）", "ok");
  } catch (e) {
    showToast(`编辑失败：${e}`, "error");
  }
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
  runningSessionId = currentSessionId;
  const runSid = runningSessionId;
  const b0 = document.getElementById("panel-body");
  if (b0) {
    repaintChatToBottom();
  }

  markRunStart(runSid);
  permMuteThisTurn = false;

  try {
    // 选内核：pi 可用就走 pi，否则走自研的。
    // ⚠️ 两个命令的**参数名与返回结构完全一致**（PiRun 是 AgentRun 的同构体），
    //    所以这里只有一行分流，渲染逻辑一行都不用改。
    const cmd = usePiKernel && piKernelAvailable ? "chat_pi" : "chat";
    const run = await invoke<AgentRun>(cmd, { input: text, images: imgs });
    // 用户可能在等的时候切走了 → 别动当前视图（结果后端已落回它自己的会话）
    if (runSid !== currentSessionId) return;
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
    if (runSid !== currentSessionId) return; // 切走了：别往别的会话塞红字
    const idx = entries.findIndex((x) => x.role === "running");
    if (idx >= 0) entries.splice(idx, 1);
    pushEntry("error", String(e));
  } finally {
    markRunEnd(runSid);
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
  // 只拦"**这条**会话正在跑" —— 其它会话在跑不影响删除（后端同判）
  if (runningSessions.has(id)) {
    showToast("该会话正在运行，先停止或等它结束再删除", "error");
    return;
  }
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


/**
 * 会话页：按**任务空间**分组（`📁 项目名`），没挂靠任何项目的那一组叫「无任务空间」。
 * 📁 标签独立、不被标题省略吃掉。
 *
 * ## 组标题可折叠 + 默认收起（2026-10-17 / 18 用户两次提）
 *
 * `.sess-sep` 从纯 div 改成 `<button>`：点一下收/放本组会话。
 *
 * **默认只展开「当前会话所在的那一组」，其余全收起。**
 * 第一版做成全展开，等于折叠功能存在但默认没人用 —— 用户原话
 * 「默认应该是折叠的吧，为啥全是展开的」。
 * 为什么不全收：全收的话打开会话页一屏只剩标题，**连自己现在在哪条会话里都看不见**，
 * 得先展开一次才找得到（而「找一条旧会话」正是要做这个功能的原因）。
 *
 * ## 状态：内存 Map，且**默认是算出来的**不是存出来的
 *
 * `sessFoldOverride` 只记**用户点过**的组；没点过的走上面那条默认规则。
 * 于是「切到别的会话 → 刚才那组自动收起、新那组自动展开」是白送的，
 * 不必在 `switchSession` 里手动搬状态（那种写法漏一处就错）。
 *
 * 不落盘：这是「看一眼就想改」的临时视图偏好，跟 `blur_collapse` 那种
 * 「设了就想一直生效」的开关不同级。要落盘得给 `AgentSettings` 加字段并
 * **重编 Rust**，为一个临时偏好付那个代价不划算。
 *
 * 键用项目名本身（`null` 组用 `""`）—— 项目改名后旧键自动失效，
 * 比留一个指向旧名的孤儿键诚实。
 */
const sessFoldOverride = new Map<string, boolean>();

/**
 * 这一组收不收。
 *
 * @param hasCurrent 这一组里有没有**当前会话**（只用来算默认值）
 *
 * 显式点过的不吃默认 —— 用户说收就收，哪怕那正是他正在聊的那一组。
 */
function isSessGroupFolded(key: string, hasCurrent: boolean): boolean {
  const explicit = sessFoldOverride.get(key);
  return explicit ?? !hasCurrent;
}

function renderSessionsView(): string {
  const projTag = (name?: string | null): string =>
    name ? `<span class="sess-proj" title="项目：${esc(name)}">📁 ${esc(name)}</span>` : "";

  /**
   * 这条会话的 Agent 模式标记（行尾 chip）。
   *
   * 设计稿 05（2:943 模式标记）：底 #26262B + 字 #71717A 的小 chip。
   * 默认模式**不标** —— 它是绝大多数会话的状态，每条都标等于纯噪声；
   * 只有「跟默认不一样」的会话才值得一眼看出来。
   */
  const modeTag = (id: string): string => {
    if (!id || id === defaultModeId) return "";
    const nm = modesCache.find((x) => x.id === id)?.name ?? id;
    return `<span class="sess-mode-tag" title="Agent 模式：${esc(nm)}（建会话时定，不能改）">${esc(
      nm,
    )}</span>`;
  };

  /**
   * 行首的**模式点**（设计稿 05：2:941/2:944/2:947/2:953）。
   *
   * 每条会话都有 —— 默认模式也画（它就是「标准=薄荷」那一个）。
   * 远看只凭色点就能分辨这条是哪一档，不用读文字。
   */
  const modeDot = (id: string): string => {
    const m = id || defaultModeId;
    return `<span class="sess-mode-dot" data-mode="${esc(m)}" title="模式：${esc(
      modesCache.find((x) => x.id === m)?.name ?? m,
    )}"></span>`;
  };

  const itemRow = (s: SessionMeta): string => `
      <div class="sess-item${s.current ? " cur" : ""}" data-id="${esc(s.id)}" title="右键挂靠到项目">
        ${modeDot(s.mode)}
        <button class="sess-open" data-id="${esc(s.id)}" title="切换到该会话">
          <span class="sess-title-row">
            <span class="sess-title">${esc(s.title || "（无标题）")}</span>
            ${s.kind === "fork" ? '<span class="sess-fork-tag">分支</span>' : ""}
            ${projTag(s.projectName)}
            ${s.current ? '<span class="sess-cur-tag">当前</span>' : ""}
          </span>
          <span class="sess-meta">${fmtTime(s.updatedAt)} · ${s.count} 条</span>
        </button>
        ${modeTag(s.mode)}
        ${s.kind === "main" ? "" : `<button class="sess-del" data-id="${esc(s.id)}" title="删除该会话">✕</button>`}
      </div>`;

  const groups = new Map<string | null, SessionMeta[]>();
  for (const s of sessionList) {
    // 主聊天永远进「无任务空间」组（它不挂任何项目）
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
      // 「当前会话」按 **id** 判，不靠后端那个 `current` 字段 ——
      // 那是另一条线（`session_switch` 前后端都靠 current.txt），
      // 而这里要的只是"这一组要不要默认展开"，用前端自己的 `currentSessionId` 更直接。
      const hasCurrent = list.some((s) => s.id === currentSessionId);
      const folded = isSessGroupFolded(k ?? "", hasCurrent);
      // 标题：项目名太长就靠 CSS 省略（`.sess-sep-name`），计数与「当前项目」另起一颗 span
      const name = k === null ? "无任务空间" : `📁 ${esc(k)}`;
      const badge = `${list.length}${k === activeName ? " · 当前项目" : ""}`;
      const rows = list.map(itemRow).join("");
      const sepAttrs =
        k === null
          ? ""
          : ` data-pname="${esc(k)}" data-pid="${projects.find((p) => p.name === k)?.id ?? ""}"`;
      return `
    <button type="button" class="sess-sep${folded ? " folded" : ""}" aria-expanded="${!folded}"
            data-fold="${esc(k ?? "")}" title="点一下收起/展开这组会话"${sepAttrs}>
      <span class="sess-caret">${folded ? "▸" : "▾"}</span>
      <span class="sess-sep-name">${name}</span>
      <span class="sess-sep-n">${badge}</span>
    </button>
    <div class="sess-group${k === activeName ? " cur-proj" : ""}"${folded ? " hidden" : ""}>${rows}</div>`;
    })
    .join("");

  return `
    <div class="mem-head">会话
      <!-- 「＋ 新建」挪到**标题条右组**了（设计稿 05:2:922）——
           这里留一个 hidden 的同 id 元素：现有 UI 测试拿它当「会话页已打开」的锚点
           （tests/ui/flow.sessions-entry.test.ts），删了会连累那几条用例。
           界面上不显示，功能入口在标题条那颗 ＋ 上。 -->
      <button class="mem-back" id="sess-new" hidden>＋ 新建</button>
    </div>
    <div class="set-hint" style="margin-bottom:8px">
      <b>按项目分组</b> · 点组标题收起/展开 · 右键会话挂靠/改挂靠 · 点一条进入对话
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
  // 组标题 → 收/放本组
  //
  // ⚠️ **直接改 DOM，不走 renderBody()**：重渲染要重新拉 `session_state`
  // + `pmem_session_map`（会话页每次进来都拉两轮），点一下折叠等 200ms
  // 还带一次闪烁；而这里要做的只是「藏一个 div + 翻一个箭头」。
  // 真要重画也没错，只是这里的收益全是负的。
  root.querySelectorAll<HTMLButtonElement>(".sess-sep").forEach((btn) => {
    btn.addEventListener("click", () => {
      const key = btn.dataset.fold ?? "";
      // 当前收/放状态从 DOM 读（`aria-expanded` 就是刚渲染出来的那份），
      // 不重算 `isSessGroupFolded` —— 那里要 `hasCurrent`，这里没必要再找一遍。
      const folded = btn.getAttribute("aria-expanded") === "true";
      sessFoldOverride.set(key, folded);
      btn.classList.toggle("folded", folded);
      btn.setAttribute("aria-expanded", String(!folded));
      const caret = btn.querySelector(".sess-caret");
      if (caret) caret.textContent = folded ? "▸" : "▾";
      // 标题的下一个兄弟就是本组容器（`.sess-sep` 与 `.sess-group` 成对出现）
      const body = btn.nextElementSibling as HTMLElement | null;
      if (body?.classList.contains("sess-group")) body.hidden = folded;
    });
  });
  document.getElementById("sess-new")?.addEventListener("click", () => void newSession());
  document.getElementById("sess-compact")?.addEventListener("click", () => void compactCurrent());

  // 右键会话 → 挂靠到项目（session 之上是项目记忆）
  installSessionBindHandler(root);
}

/** 「＋」菜单：新建会话 / 新建项目（原「项+」并入） */
/**
 * 挂「点外面关掉」。
 *
 * ⚠️ 必须延后一拍（setTimeout 0）再挂：立刻挂会被**打开菜单的那次 click** 自己触发
 *（事件从按钮冒泡到 window），菜单在渲染同帧就被移除 = "点了没用"。
 * Playwright 实证：同帧日志 `menu opened → once-close FIRED`。
 */
function closeOnOutsideClick(close: () => void): void {
  // ⚠️ `domAlive` 守卫：定时器可能活过环境销毁（teardown 后 window 不存在），
  //   回调里摸 window 会抛 ReferenceError → vitest unhandled error → 退出码 1。
  setTimeout(() => {
    if (!domAlive()) return;
    window.addEventListener("click", close, { once: true });
  }, 0);
}

/**
 * 「＋」菜单 —— **新建会话时选模式**（从「设置 › 行为」挪过来）。
 *
 * 为什么模式住在这里而不是设置页：
 * - 模式**跟会话走**、建完就钉死（“一个会话模式不能变”）—— 它不是一个能随时
 *   回来改的“设置”，而是**建会话时的一个参数**。摆在设置页里既放错位置，
 *   又暗示“随时能改”，改完发现当前会话没变才是真疑惑。
 * - 菜单里显式列出每个模式（而不是“新建会话 + 事后切”），因为**没有事后切**。
 *
 * 「新建项目…」建完项目后**原地再弹一次这个菜单**：项目建好了、会话还没建，
 * 而模式必须在建会话那一刻定 —— 不能替用户默认掉，否则他想开个闲聊项目会话
 * 就得把刚建的会话删掉重来。
 *
 * ⚠️ 菜单里**不能**放“修改默认模式”。默认值 = **标准模式**（`modes::DEFAULT_MODE_ID`，
 * 编译期常量）：`settings.activeMode` 已删 —— 「闲聊」被标成
 * 「默认」就是因为那个字段（它可能残留着早先在设置页切过闲聊的值），
 * 而默认就该是标准模式。
 * 界面上没有了改默认值的入口，就别再把默认值做成可配项。
 */
async function showNewMenu(x: number, y: number): Promise<void> {
  // 打开前重拉一次：`modes.json` 用户是可以手编的（热重载），菜单得跟上
  await loadModes(currentSessionId);
  document.getElementById("new-ctx")?.remove();
  const menu = document.createElement("div");
  menu.id = "new-ctx";
  menu.className = "ctx-menu";

  // 默认模式排最前并打「默认」标 —— 用户要在列表里一眼看到不选会落到哪
  const def = defaultModeId;
  const ordered = [...modesCache].sort((a, b) =>
    a.id === def ? -1 : b.id === def ? 1 : 0,
  );
  const modeItems = ordered.length
    ? ordered
        .map((m) => {
        return `
      <div class="ctx-item" data-act="sess" data-mode="${esc(m.id)}" title="${esc(
        m.description,
      )}">
        <div class="ctx-row"><span class="ctx-item-ico">${
          MODE_ICON[m.id] ?? "🧩"
        }</span><span class="ctx-item-name">${esc(m.name)}</span>${
          m.id === def ? `<span class="ctx-item-note">默认</span>` : ""
        }</div>
      </div>`;
        })
        .join("")
    : // 读不到 modes.json 也得能开新会话 —— mode 传空让后端退回默认模式
      `<div class="ctx-item" data-act="sess" data-mode="">
        <div class="ctx-row"><span class="ctx-item-ico">＋</span><span class="ctx-item-name">新建会话</span></div>
      </div>`;

  menu.innerHTML = `
    <div class="ctx-title">新建会话 · 选个模式（建完不能改）</div>
    ${modeItems}
    <div class="ctx-sep"></div>
    <div class="ctx-item" data-act="proj">
      <div class="ctx-row"><span class="ctx-item-ico">📁</span><span class="ctx-item-name">新建项目…</span></div>
    </div>`;
  document.body.appendChild(menu);
  // 宽高要**先入 DOM 再量**：模式数是 JSON 里定的，提前算高度只会在加模式时错位
  menu.style.left = `${Math.max(4, Math.min(x, window.innerWidth - menu.offsetWidth - 4))}px`;
  menu.style.top = `${Math.max(4, Math.min(y, window.innerHeight - menu.offsetHeight - 4))}px`;

  const close = (): void => menu.remove();
  menu.addEventListener("click", async (e) => {
    const t = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item");
    if (!t) return;
    const act = t.dataset.act;
    const mode = t.dataset.mode;
    close();
    if (act === "sess") {
      // 空串 → undefined → `newSession` 用 `defaultModeId`
      void newSession(mode || undefined);
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
      await refreshSessionProjectTags();
      showToast(`已创建「${p.name}」，再选个模式开新会话`, "ok", 2600);
      // 项目就位、会话还没建 → 原地重开菜单让用户定模式（新会话会绑到 p.id）
      void showNewMenu(x, y);
    } catch (err) {
      showToast(`创建项目失败：${err}`, "error");
    }
    renderBody();
  });
  closeOnOutsideClick(close);
}

// 历史：执行位置 / Agent 模式 / 执行权限三组控制曾以 chip 形式住在输入行
// （先三颗、后合并成一颗）。曾定论「对话页不该有设置项」，
// 于是**整个搬进「设置 › 行为」页**，输入行只留一颗行为 logo（🎛）。
// 那三颗 chip 的刷新函数已随之删除 —— 状态由 `renderBehaviorView` 现算，
// 不再需要"状态变了刷新 chip"的钩子。
// ⚠️ **不要**重新往输入行塞设置类控件 —— 那正是被否掉的做法。
//
// 二次搬迁：**Agent 模式离开设置页**，挪进「＋」菜单的「新建会话」里
// （定论："b 从设置页挪到一个'新建会话时选模式'的入口 默认是标准"）。
// 理由：模式跟会话走、建完就钉死 —— 它根本不是一个"设置"，而是**建会话时的一个参数**，
// 摆在设置页里既放错了地方，又暗示"随时能改"。

/** 切换「后台执行」到指定状态（设置页的复选框用） */
async function setBgDesk(on: boolean): Promise<void> {
  try {
    await invoke<boolean>("bg_desk_set", { enabled: on });
    bgDeskEnabled = on;
    await loadBgDesk();
    showToast(on ? "已切到后台执行（隐形桌面）" : "已切回前台执行", "ok", 2600);
  } catch (err) {
    showToast(`切换失败：${err}`, "error");
    await loadBgDesk();
  }
}

/**
 * 拉一次模式表（启动、切会话后、打开「＋」菜单前都调）。
 *
 * `sessionId` 决定 `activeModeId` 是**哪条会话**的模式：
 * 传了就按它算，没传就用全局默认。模式跟会话走之后，
 * 所有"当前是什么模式"的判断都得带上会话，否则会把默认模式当成当前模式。
 *
 * ⚠️ `modesStamp` 是**乱序保护**：启动时先发了一条不带会话的（拿默认模式），
 * 紧接着 `loadSession()` 又发了一条带会话的。两条都是网络往返，回来顺序不保证 ——
 * 晚到的那条会把 `activeModeId` 覆盖成错的值，然后 `isChattyMode()` 就判错，
 * 闲聊会话的历史消息会画成一整块。只认最后一次发出的请求。
 */
let modesStamp = 0;
async function loadModes(sessionId?: string): Promise<void> {
  const my = ++modesStamp;
  try {
    const r = await invoke<{ activeMode: string; defaultMode?: string; modes: ModeView[] }>(
      "modes_get",
      // `|| null` 而不是 `?? null`：`currentSessionId` 在会话加载完成前是**空串**，
      // 空串会被 Rust 当成 `Some("")` 去查一条不存在的会话。统一成 null 走默认模式。
      { sessionId: sessionId || null },
    );
    if (my !== modesStamp) return; // 有更新的请求在飞 → 这条过期，丢掉
    activeModeId = r?.activeMode ?? activeModeId;
    defaultModeId = r?.defaultMode ?? defaultModeId ?? activeModeId;
    modesCache = Array.isArray(r?.modes) ? r.modes : [];
  } catch (err) {
    // 读不到不是致命错误：chip 退回显示 id，模式本身仍在后端生效
    console.warn("[orbcat] 读模式失败:", err);
  }
}

/** 拉一次「后台执行」状态（启动、开关后、面板展开时调） */
async function loadBgDesk(): Promise<void> {
  try {
    const r = await invoke<{
      enabled: boolean;
      available: boolean;
      error: string | null;
      windows: { title: string; width: number; height: number; pid: number }[];
    }>("bg_desk_status");
    bgDeskEnabled = !!r?.enabled;
    bgDeskError = r?.error ?? null;
    bgDeskWindows = Array.isArray(r?.windows) ? r.windows : [];
  } catch (err) {
    console.warn("[orbcat] 读后台执行状态失败:", err);
  }
}

/** 会话右键：挂靠 / 重命名会话 / 删除（打开用左键） */
function showSessionBindMenu(x: number, y: number, sid: string, kind: string): void {
  document.getElementById("sess-ctx")?.remove();
  const menu = document.createElement("div");
  menu.id = "sess-ctx";
  menu.className = "ctx-menu";
  const binds = [
    `<div class="ctx-item" data-act="bind" data-pid="">无任务空间（不挂项目）</div>`,
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
  closeOnOutsideClick(close);
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

/**
 * 每个空间里有多少条会话（键 = 项目名；无任务空间那组用 `null`）。
 *
 * 与会话页分组**同一套口径**：`kind === "main"` 的主聊天归「无任务空间」，
 * 其余按 `projectName` 归。顶栏下拉显示的条数因此与会话页看到的一致 ——
 * 两处用不同口径的话，顶栏说「论文 · 3」而会话页里只有 2 条，
 * 用户会以为是数据错了。
 */
function sessionCountBySpace(): Map<string | null, number> {
  const m = new Map<string | null, number>();
  for (const s of sessionList) {
    const k = s.kind === "main" ? null : (s.projectName ?? null);
    m.set(k, (m.get(k) ?? 0) + 1);
  }
  return m;
}

/**
 * 这条会话挂着的项目 id。
 *
 * `SessionMeta.projectName`（项目名）是前端从 `pmem_session_map` 补的，
 * 要 id 得回 `projects` 里按名反查 —— 反查不到 = 没挂靠（项目被删/改名留了残名）。
 * 主聊天永不挂项目（`pmem_bind_session` 那边也拒），恒 null。
 */
function sessionProjectId(meta?: SessionMeta): number | null {
  if (!meta || meta.kind === "main") return null;
  return projects.find((p) => p.name === meta.projectName)?.id ?? null;
}

/**
 * 这个项目里**最近动过**的那条会话（没有则 undefined）。
 *
 * 选项目后要落到的那一条：按 `updatedAt` 倒序取首个。
 * 主聊天恒不挂项目，所以天然不在任何项目里（`projectName === null` 过滤掉了）。
 */
function latestSessionIn(projectId: number | null): SessionMeta | undefined {
  if (projectId == null) return undefined;
  const name = projects.find((p) => p.id === projectId)?.name;
  if (!name) return undefined;
  return sessionList
    .filter((s) => s.projectName === name)
    .sort((a, b) => b.updatedAt - a.updatedAt)[0];
}

/** 把顶栏那颗 `<select>` 的值/提示刷成当前激活项目（`renderBody` 不重画标题条） */
function paintProjectSelect(): void {
  const psel = document.getElementById("project-select") as HTMLSelectElement | null;
  if (!psel) return;
  // ⚠️ 选项文案**也要重写**：条数会变（新建/删会话、切换挂靠），
  // 只改 `value` 的话收起状态下那颗 chip 上还挂着旧数字
  // （「论文 · 12」实际已经 13 条了）。
  const counts = sessionCountBySpace();
  const want = [
    { id: "", label: `无任务空间 · ${counts.get(null) ?? 0}` },
    ...projects.map((p) => ({
      id: String(p.id),
      label: `📁 ${p.name} · ${counts.get(p.name) ?? 0}`,
    })),
  ];
  // 只在**真的不一样**时才重写 —— `renderBody` 每条消息都会走到这里，
  // 每次都重建 <option> 是白做的功，还可能把用户正展开着的原生下拉弹回去。
  const same =
    psel.options.length === want.length &&
    want.every(
      (w, i) => psel.options[i].value === w.id && psel.options[i].textContent === w.label,
    );
  const wantVal = activeProjectId == null ? "" : String(activeProjectId);
  if (!same) {
    psel.innerHTML = want.map((w) => `<option value="${w.id}">${esc(w.label)}</option>`).join("");
  }
  // ⚠️ 选中状态**必须用 DOM 属性 `option.selected`**，不能只靠 HTML 的 `selected` 属性。
  //
  // 实测（jsdom 最小复现，四种写法都验过）：
  //   A 直接 innerHTML 带 selected 属性  → selectedIndex = **-1**（属性没转成 selectedness）
  //   B 空 select 再 innerHTML 带 selected → selectedIndex = **-1**
  //   D select 嵌在 div.innerHTML 里       → selectedIndex = **-1**
  //   C innerHTML 不带 selected + 赋值 value → selectedIndex = 0  ✅
  //
  // 根因：HTML 属性 `selected` 只是**初始选中标记**，解析完就不是 selectedness 了；
  // 而 `psel.value = wantVal` 在「目标值恰好等于当前 value」时会被
  // `if (psel.value !== wantVal)` 守卫跳过 —— 首项的 value 就是 ""，
  // 于是「无任务空间」这条永远选不上，`selectedIndex` 停在 -1。
  //
  // 浏览器里靠「默认选中第一项」的隐式兜底侥幸看不出问题，但那是不可靠行为：
  // 同一份代码换个 DOM 实现（jsdom / 测试环境）就崩。
  //
  // 正解：**无条件**把选中的那一项设成 `option.selected = true`。
  // 这是 DOM 属性（IDL attribute），直接改 selectedness，与解析时机无关。
  const idx = want.findIndex((w) => w.id === wantVal);
  for (let i = 0; i < psel.options.length; i++) {
    psel.options[i].selected = i === idx;
  }
  // value 兜底：有些环境对 selected 赋值不敏感，两手都做（无害）
  if (psel.value !== wantVal) psel.value = wantVal;
  const name = projects.find((p) => p.id === activeProjectId)?.name;
  psel.title = `当前：${name ?? "无任务空间"} —— 跟当前会话走（选它会跳到那个空间的会话）`;
}

/**
 * 进对话时把顶栏项目**拉回当前会话所在的项目**。
 *
 * ## 这不是洁癖，是修串味
 *
 * 顶栏项目（后端 `active_project_id`）不是装饰，它决定三件事：
 * ① 这一轮注入哪份项目记忆（`agent.rs` 的「当前项目」索引）；
 * ② 蒸馏出来的条目往哪个项目落（`memory.rs::auto_distill`）；
 * ③ 执行期给**哪个目录**开读写授权（`lib.rs::active_project_dir`）。
 * 所以它必须与「你正在聊的那条会话」一致 —— `config.rs` 里那句话就是这个意思：
 * 「决定注入哪份 MEMORY.md，**随会话走**」。
 *
 * ## 2026-10-17 用户报的现场
 *
 * 顶栏选「写作业」→ 那一步只改了激活项目（并跳去会话页）→ 用户**没点任何会话**，
 * 直接「返回对话」→ 回到的还是那条 test 会话，顶栏却还挂着「写作业」。
 * 于是这条 test 会话下一轮就被告知「顶栏切到了项目『写作业』」，
 * 还顺手拿到了那个目录的读写授权 —— 看着只是标题不对，**上下文和权限都串了**。
 *
 * ## 为什么挂在 `renderBody` 的对话分支上
 *
 * 这是「进入对话」的唯一必经之处：点球展开、`‹ 返回对话`、`switchSession`、发完消息
 * 重画……全走它。挂在别处（点返回时才补）会漏掉「启动时 settings.json 里就存着
 * 上次那个项目」这条冷启动路径。
 *
 * ## 拿不到就不动
 *
 * `renderBody()` 每条消息都会跑一遍，这里必须**零副作用**：
 * - `sessionList` 里找不到当前会话（还没拉）→ 不动；
 * - `projectName === undefined`（`refreshSessionProjectTags` 还没跑，挂靠未知）→ 不动。
 * 否则每次开面板都会把上次记住的项目误清成「无任务空间」。
 */
function alignProjectToSession(): void {
  const meta = sessionList.find((s) => s.id === currentSessionId);
  if (!meta || meta.projectName === undefined) return;
  const want = sessionProjectId(meta);
  if (want !== activeProjectId) {
    activeProjectId = want;
    void invoke("pmem_set_active", { id: want }).catch(() => {});
  }
  paintProjectSelect();
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
    closeOnOutsideClick(close);
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
    | "toolTick"
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
  /** toolTick：长工具执行心跳（已运行秒数 + 输出尾巴） */
  elapsedSecs?: number;
  tail?: string;
  /**
   * 工具耗时（毫秒）—— pi 的 `tool_execution_end.durationMs`，
   * 自研内核是后端掐的表。工具行后面显示「· 1.2s」。
   */
  durationMs?: number;
  /** discardStream */
  reason?: string;
  /** steer：被消费掉的那条插话的 id */
  id?: string;
  /** steer：插话附带的图片（文件路径）；插话不铺气泡，图要画在时间线 steer 条目上 */
  images?: string[];
}

interface Candidate {
  id: string;
  content: string;
  source: string;
  created_at: string;
  /** 触发轮次（用户原话，截断过）—— 后端可选，老数据没有 */
  turn_ref?: string | null;
}

/** 审批页底部状态栏的统计（后端 `mem_stats`） */
interface MemStats {
  /** 当前待批数 */
  pending: number;
  /** 已处理数（已批准 + 已驳回） */
  handled: number;
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

/**
 * MCP server 列表的**前端权威副本**。
 *
 * ⚠️ 为什么不再像以前那样"从 DOM 文本里 scrap"：
 * stdio 形态有 `command` / `args[]` / `env{}` 三类字段，它们**不是**
 * 一段可读回来的文本 —— 从 `<code>` 的 textContent 取，`args` 的
 * 参数边界和 `env` 的键值对都会丢。DOM 只负责**画**，数据以这里为准。
 */
let mcpServers: McpServerCfg[] = [];

// ---------------- 项目（拼装包，对应 Rust `project.rs`） ----------------

/** 一个项目包（`projects/<id>/project.json`）的清单摘要 */
interface BundleInfo {
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

/** `project_list` 的返回 */
interface ProjectListView {
  bundles: BundleInfo[];
  active: string | null;
  /** 引用解析失败项：`[类别, id, 原因]` */
  unresolved: [string, string, string][];
  /** permPreset 被整份丢弃的原因（fail-closed） */
  permError: string | null;
  tools: { name: string; description: string; template: string[]; perm: string }[];
}

/**
 * 设置入口页要的项目摘要（懒加载，进页面时才有）。
 *
 * ⚠️ 拼装包功能暂缓后，`projectSummaryText` 成了幽灵函数（返回 null），
 * 于是这里暂时没人读了 → TS6133。**故意不删**：`renderProjectView` / `project_list`
 * 命令 / 那一整套 vitest 都还在，恢复功能时立刻要用。下面的 `void` 引用
 * 把它钉在编译期可见，免得被当成死代码清掉。
 */
let projectInfo: ProjectListView | null = null;
void projectInfo;

/**
 * 技能清单缓存 —— 设置入口页的「记忆」摘要要显示技能数。
 *
 * 为什么缓存而不是每次现扫：`skills_list` 是热扫描目录（Rust 侧每次现读
 * `agent-data/skills` 下每个技能的 SKILL.md），设置入口页每次打开都拉一次没问题，
 * 但**摘要和记忆页会各拉一次** —— 同一个数据同一屏要两遍。
 * 记忆页渲染完顺手回填这里，两处共用一份。
 */
let skillsCache: SkillInfo[] = [];

/** 这条配置实际用哪种传输（与 Rust `resolved_transport` 同规则） */
function mcpTransportOf(s: McpServerCfg): "http" | "stdio" {
  if (s.transport === "stdio") return "stdio";
  if (s.transport === "http") return "http";
  return (s.command ?? "").trim() ? "stdio" : "http";
}

// ---------------- 状态 ----------------

let mode: Mode = "orb";

/**
 * `applyMode` 正在切换窗口形态。
 *
 * 存在的理由：`set_window_mode` 改 bounds 时 Windows 会触发一次激活状态变化，
 * WebView 随之抛 `blur`。若那一下被当成"用户点到别处去了"来处理，就会在
 * 切换半路把窗口又收回球大小（见 `applyMode` 开头那段注释的完整事故）。
 * 这个标志让切换期间的 blur 一律放行。
 */
let modeSwitching = false;
/** 当前会话是否在跑 —— 决定「发送」还是「插话」、显不显示 ■。**按会话**算（并行 run） */
let busy = false;
let thinking = false;
/**
 * 进行中气泡属于哪个会话 —— 进度事件到达时用来判断"是不是已经切走了会话"。
 * 切走后到达的事件直接丢（那是旧会话的收尾），没切走则重建气泡接着画。
 */
let runningSessionId = "";

/**
 * **正在跑的会话集合**（多会话并行 run）。
 *
 * 每个会话各自跑各自的 agent loop（后端 `RunRegistry` 按 session_id 分槽），
 * 互不干扰 —— 切到没在跑的会话可以立刻开新任务，切到在跑的会话可以插话。
 */
const runningSessions = new Set<string>();

/** 当前会话在不在跑 */
function isCurBusy(): boolean {
  return runningSessions.has(currentSessionId);
}

/** run 开始/结束、切会话之后：把 `busy` 与按钮布局对齐**当前会话**的状态 */
function syncBusyUi(): void {
  busy = isCurBusy();
  setBusyUi(busy);
  updateBgRunBanner();
}

/** 登记一轮开始（`runSid` = 这一轮归属的会话，开跑时就定死） */
function markRunStart(runSid: string): void {
  runningSessions.add(runSid);
  setThinking(true); // 任一 run 在跑，球就该是"干活中"
  syncBusyUi();
}

/** 一轮结束：注销并按当前会话重算 busy */
function markRunEnd(runSid: string): void {
  runningSessions.delete(runSid);
  if (runningSessionId === runSid) runningSessionId = "";
  // 下一轮重新计一次「↓」（一轮只加一次，见 paintRunningNow）
  chatMissedCountedForRun = false;
  setThinking(runningSessions.size > 0); // 还有别的会话在跑 → 球继续转
  syncBusyUi();
}

/**
 * 输入框草稿。
 *
 * 收起面板会把整个 DOM 换成悬浮球，输入框连同内容一起没了 ——
 * 「打了半句话、出去看一眼、回来」是日常动作，所以草稿要单独存一份。
 * 发送成功时才清空。
 */
let draftInput = "";

/**
 * 胶囊态五态机。优先级 **error > pending > thinking > mem > idle** ——
 * 出事了最要紧，其次是"被挡住等你批"的权限申请，再次是"正在干活"，
 * 然后才是"可选、不催"的记忆候选。
 *
 * ⚠️ 2026-10-14 用户拍板调整过一次：`thinking` 从 `mem` **下面**提到了上面。
 *    原顺序下，只要待审记忆 ≥ 1 条，球就永远是绿壳 peek，agent 干活时的
 *    蓝猫摇尾巴一次都露不出来 —— 用户看着像卡住了。运行时是当下最该被看见的事。
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
  idle: "orbcat — 点击展开",
  thinking: "orbcat — 正在思考…",
  pending: "orbcat — 有权限申请等你批",
  mem: "orbcat — 有记忆等你审",
  error: "orbcat — 出错了，点开看看",
};

/**
 * 睡眠中的球标题。
 *
 * ⚠️ 语义变更后这句**基本看不到**了 —— 睡眠时窗口被隐藏，
 * 没有可悬停的元素。保留它是因为 `orbSleeping` 仍会同步过来（例如
 * 后端在另一条路径上改了状态、而窗口还没来得及隐藏的那一瞬），
 * 措辞也改成描述**事实**（睡着的唤醒入口在托盘），不再说"点一下唤醒"
 * —— 隐藏的球点不到，那样写是在骗用户。
 */
const ORB_SLEEP_TITLE = "orbcat — 睡眠中（球已隐藏；唤醒：托盘图标右键 → 唤醒，或再启动一次）";

/**
 * 球是否处于睡眠。
 *
 * **后端才是权威**：托盘菜单、球的右键菜单、再启动一次 exe 三条路都能改它，
 * 前端自己记必然不同步，所以由 `orb-ui` 事件回灌。这里同时做一次乐观更新，
 * 让点了菜单立刻看到变化（不等 IPC 往返）。
 */
let orbSleeping = false;

function currentOrbState(): OrbState {
  if (unseenError) return "error";
  if (pendingPermCount > 0) return "pending"; // 权限=被挡住，最急
  // ⚠️ `thinking` 必须排在 `mem` **前面**（2026-10-14 用户拍板：运行时 > 记忆 > 常态）。
  //
  //    原先 `mem` 在前，于是只要待审记忆 ≥ 1 条，球就**永远**是绿壳 peek，
  //    agent 干活时的蓝猫摇尾巴一次都露不出来 —— 从用户角度看就是「怎么一直
  //    不动」。记忆是可选的（不催），运行时才是当下最该被看见的那件事。
  if (thinking) return "thinking";
  if (pendingMem > 0) return "mem";
  return "idle";
}

const app = document.getElementById("app")!;

let models: ModelView[] = [];
/** 当前选中模型的**接口 id**（身份 = (Base URL, id)，配 selectedModelUrl 消歧） */
let selectedModel: string | null = null;
let selectedModelUrl: string | null = null;

/** 当前选中的模型条目：接口 id 相同 +（有 url 记录时）同组才算命中 */
function curModel(): ModelView | undefined {
  return models.find(
    (m) =>
      m.id === selectedModel &&
      (!selectedModelUrl || sameUrl(m.url, selectedModelUrl)),
  );
}

/** Base URL 归一化比较（尾斜杠/大小写不算差异）—— 身份键的一半，必须用同一个口径 */
function sameUrl(a: string | null | undefined, b: string | null | undefined): boolean {
  const norm = (s: string | null | undefined) => (s ?? "").trim().replace(/\/+$/, "").toLowerCase();
  return norm(a) === norm(b);
}

/**
 * 「哪一条才是当前」——**唯一**一条。
 *
 * 为什么要它：`isCurrent` 那种 `id 相同 && (url 为空 || url 相同)`
 * 的写法，在"设置里只有 id、没记 url"时会命中**所有**同 id 的行 ——
 * 症状就是「两个 glm-5.3-flash 都画线（都带当前标记）」。
 * 统一改成"等于 `curModel()` 解析出来的那一条"，最多只标一个。
 */
function currentModelRef(): { id: string; url: string } | null {
  const c = curModel();
  return c ? { id: c.id, url: c.url } : null;
}

/**
 * 模型**分组显示名**（键 = Base URL）—— `settings.modelGroupNames`。
 *
 * 面板的模型卡片要显示「组别 + 模型名」（定稿：这一块就是放模型的，
 * 组别 + 模型名）。取不到时回退到 host。
 */
let modelGroupNames: Record<string, string> = {};

/** 当前模型所属分组的显示名（配过用配过的，否则用 host） */
function modelGroupLabel(url: string | undefined): string {
  if (!url) return "";
  return modelGroupNames[url]?.trim() || shortHost(url);
}
/** 项目索引（pmem.sqlite）；activeProjectId=null = 无任务空间（没有激活项目） */
let projects: ProjectRow[] = [];
let activeProjectId: number | null = null;
let entries: ChatEntry[] = [];

/**
 * MCP「默认全给」开关（settings.mcpAllGroups）。
 *
 * 决策背景：以前外部工具要模型自己 `list_tool_groups` → `load_tool_group`
 * 两步才用得上，多一道仪式还常忘。现在默认**直接全给**，这个开关只是保底
 * （工具集特别大、小窗口模型塞不下时才关掉）。
 */
let mcpAllGroups = true;

/**
 * 顶栏模型下拉（▾）是否展开 —— **纯模型名列表，只选不管**。
 * 开关在整颗 chip（点一下出模型列表）。管理入口已挪到 设置 › 🧩 模型。
 */
let modelsDropdownOpen = false;

/** 待发送的图片（data URL） */
let pendingImages: string[] = [];

/**
 * 内容区视图（两级导航）。
 *
 * 「模型管理」不再是面板内子页 —— 它是独立窗口（models.html，见 models.ts），
 * 入口：设置菜单「🧩 模型」。面板上的 chip 只负责**选**模型（出列表）。
 *
 *   settings ─┬─ mcp       MCP 工具组
 *             ├─ perm      文件权限
 *             ├─ mem       记忆
 *             └─ usage     Token 用量（按天 × 模型）
 */
type View =
  | "chat"
  | "settings"
  | "mcp"
  | "perm"
  | "cmdpolicy"
  | "mem"
  | "sessions"
  | "usage"
  | "recovery"

  | "behavior"
  | "search"
  | "life"
  | "project";

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
/**
 * **项目目录自动读写**（设计稿 04「行为与安全」组的行内开关）。
 * 关掉 = 激活项目绑的目录不再自动给 ReadWrite，工具碰它一律走弹卡审批。
 */
let projectDirAutoread = true;
/** 命令策略条数（白名单 / 硬阻断）—— 设置页行内摘要用 */
let cmdPolicyCounts = { allow: 0, hard: 0 };

/**
 * 执行权限档位（settings.execTrust）—— 管 run_command / MCP 弹卡频率。
 * 三档：ask 每次询问 / smart 低中风险免问 / full 全部免问（硬阻断永远拦）。
 * 对应 Rust 侧 `config::ExecTrust`；文件权限申请不受它影响。
 */
let execTrust: "ask" | "smart" | "full" = "ask";

const EXEC_TRUST_LABEL: Record<string, { icon: string; short: string; full: string }> = {
  ask: { icon: "🛡", short: "询问", full: "每次询问" },
  smart: { icon: "⚡", short: "智能", full: "智能放行（高风险仍问）" },
  full: { icon: "🔓", short: "全放", full: "允许完全访问" },
};

/**
 * Agent 模式（`agent-data/modes.json`）—— 管**两件正交的事**：
 *   1. 工具以什么形态出现（`toolPresentation`：native 逐个给 / ptc 坍缩成 run_code）
 *   2. 正文能不能发图（`imageReplies`）、场景上下文（`promptExtra`）、聊天式呈现（`chatty`）
 *
 * ⚠️ 模式**不再管 MCP 外部工具组**。`mcpGroupsPreload` 仍会从用户已有的
 * `modes.json` 读出来（不能因为改了语义就让人打不开自己的配置），但它
 * 不影响任何行为 —— 能不能用某个组只看用户在设置页的组开关。
 * 曾经的闸门在后端 `mcp::ToolRegistry::mode_allows`（现在恒为 true）。
 *
 * 与执行权限（`execTrust`）是两层互不干涉的闸门：
 *   模式 = 工具以什么形态出现   /   权限 = 用它们时问不问
 * 所以模式**不碰** `run_command` 的弹卡，`execTrust` 也**不碰**工具可见性。
 */
let activeModeId = "standard";
/**
 * **新建会话的默认模式** —— 就是内置的「标准模式」（`modes_get.defaultMode`）。
 *
 * 它是**常量**，不是可配项：`settings.activeMode` 已删除
 * （「闲聊」被标「默认」就是它干的，而默认就该是标准模式）。
 * 界面上早已没有改默认值的入口 —— 一个改不了的字段还留着，只会变成
 * "手编也不生效"的困惑源。
 */
let defaultModeId = "standard";
let modesCache: ModeView[] = [];

interface ModeView {
  id: string;
  name: string;
  description: string;
  /** 工具呈现方式：`native` / `ptc`（老后端不返回时按 native 处理） */
  toolPresentation?: "native" | "ptc";
  /**
   * 正文能不能发图。老后端不返回时按 `work` 处理。
   *
   * ⚠️ 只用于**显示**，前端不做拦截：工作模式也要渲染产物图（截图/图表），
   * 按这个值降级会把它一起挡掉。真正的约束在系统提示的措辞里。
   */
  imageReplies?: "work" | "free";
  /** 模式注入的场景上下文（可能为空） */
  promptExtra?: string;
  /**
   * 聊天式呈现：一条回复拆成多个气泡、且**不显示过程**。
   * 只有闲聊模式开 —— 工作模式的结构化长答案切成 5 个泡会非常难读。
   */
  chatty?: boolean;
}

/** 每个内置模式的图标。模式是 JSON 自定义的，认不出来就用通用图标。 */
const MODE_ICON: Record<string, string> = {
  standard: "🎯",
  ptc: "🚀",
  // 「极简」被「闲聊」取代（机制相同 = 不给外部工具组，
  // 闲聊多了发图能力与场景上下文）。旧 id 的图标留着无害，但新 id 要在这儿。
  chat: "💬",
  minimal: "🪶",
  creator: "🛠",
};

/**
 * 当前会话的模式 id（模式跟会话走）。
 *
 * ⚠️ 前端**唯一**该用来判断"现在是什么模式"的函数。直接读 `activeModeId`
 * 只在"它就是当前会话的模式"时才对 —— 而 `activeModeId` 是 `modes_get` 的
 * 返回值，`loadModes()` 没带会话 id 时它其实是**默认模式**。
 *
 * 三层兜底（从可信到保守）：
 * 1. `sessionList` 里当前那条的 `mode`（Rust 必给，老会话回退成默认模式）
 * 2. `activeModeId`（最后一次 `loadModes` 的结果）
 * 3. `"standard"`
 */
function currentModeId(): string {
  const m = sessionList.find((x) => x.id === currentSessionId);
  return m?.mode || activeModeId || "standard";
}

/** 当前会话的模式名（给人看的），认不出来就退回 id。 */
function currentModeName(): string {
  const id = currentModeId();
  return modesCache.find((x) => x.id === id)?.name ?? id;
}

/**
 * 「后台执行」状态（进程级，不落盘）。
 *
 * 开 = 命令跑在**隐形桌面**上：应用自己弹的窗口不会闪到你屏幕上、不抢焦点。
 *
 * ⚠️ 它**不是**一个 agent 模式：模式管"哪些 MCP 工具组可见"，
 * 这个管"在哪儿执行" —— 两个正交维度（定稿结论）。
 *
 * 历史：紧挨着这里原来还有一个 `PRESENTATION_LABEL`（native / ptc 的一句话说明）。
 * 模式选择器搬进「＋」菜单时删掉了 —— 那句说明只服务"设置页的模式列表"，
 * 现在那份说明在模式菜单项的 `title`（取模式自己的 `description`）与
 * 「设置 › 行为」的提示文字里，留着它就是一段没人读的死代码。
 */
let bgDeskEnabled = false;
let bgDeskError: string | null = null;
let bgDeskWindows: { title: string; width: number; height: number; pid: number }[] = [];

/** 面板刚展开的时间戳 —— 展开瞬间可能收到 blur，忽略掉防抖 */
let panelOpenedAt = 0;

/** 权限规则条数（设置入口页摘要用） */
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
    // ⚠️ 守卫：1.2s 后才回头改按钮，可能已经越过环境销毁/teardown。
    if (!domAlive()) return;
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
  // 环境已经拆掉了就直接放弃重绘。
  // 单元测试 teardown 之后，仍可能有 late 的事件/定时回调打进来
  // （例如 window resize、setTimeout）：那时 document 已经不在了，
  // 硬画下去会抛 ReferenceError，被 vitest 记成 unhandled error 并让
  // 整个 run 退出码为 1 —— 明明 50 个测试全绿却判失败。
  if (typeof document === "undefined") return;
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
  //
  // ⚠️ `orb--has-mem` 不是状态，是**独立于帧态**的提醒标记：
  //    思考时帧是蓝猫（thinking 压过 mem），但紫脑标照亮 —— 见 `applyOrbState`。
  const s = currentOrbState();
  app.innerHTML = `
    <div class="orb orb--${s}${orbSleeping ? " orb--sleeping" : ""}${
      pendingMem > 0 ? " orb--has-mem" : ""
    }" id="orb" title="${orbSleeping ? ORB_SLEEP_TITLE : ORB_TITLE[s]}">
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
  const orb = document.getElementById("orb");
  if (!orb) return; // 同上：DOM 已经不在了（被清空/换页），别再往上挂监听
  attachDrag(orb, () => {
    // 左键点球 → 展开面板。
    // ⚠️ 这里**不再有"睡着的球点一下唤醒"的分支**（语义变更后）：
    // 睡眠时窗口是隐藏的，隐藏的窗口收不到点击 —— 那段代码永远走不到，
    // 留着只会让人以为睡眠仍是"留在屏幕上"。唤醒入口改到托盘菜单与
    // 「再启动一次 exe」（后者会直接展开面板）。
    void expand();
  });
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
  // 紫脑标跟**帧态解耦**：thinking 压过 mem 之后，若角标还绑在 `.orb--mem` 上，
  // agent 一忙起来提醒就跟着消失 —— 正好在最该提醒"有记忆等你审"的时候没了。
  // 这里直接由 pendingMem 驱动，与球在演哪一帧无关。
  el.classList.toggle("orb--has-mem", pendingMem > 0);
  el.classList.toggle("orb--sleeping", orbSleeping);
  el.title = orbSleeping ? ORB_SLEEP_TITLE : ORB_TITLE[s];
}

// ---------------- 球的睡眠 / 唤醒 ----------------

/**
 * 睡眠：**把球从屏幕上藏起来**（后端 `win.hide()`），任务照跑。
 *
* ## 语义
 *
* *"睡眠是球从页面上消失但是任务不停止"* —— 所以这里：
 * 1. 先切回球态（窗口还是"球"这个形状，只是马上要被隐藏）；
 * 2. 调后端 `sleep` → 窗口隐藏；
 * 3. **完全不碰任务**：不取消、不清空、不重置任何在跑的一轮。
 *
 * `orbSleeping` 只是本地状态标记（用于 title 与菜单措辞）——球已经不可见了，
 * 所以**没有"睡眠视觉"这回事**，见 `styles.css` 里那条空声明。
 */
async function sleepOrb(): Promise<void> {
  orbSleeping = true;
  if (mode !== "orb") {
    await applyMode("orb");
  } else {
    applyOrbState();
  }
  await invoke("orb_ui_action", { action: "sleep" }).catch((e) =>
    console.error("[orbcat] 睡眠失败:", e),
  );
}

/**
 * 唤醒：**让球重新出现**（后端 `show()`）。
 *
 * 三条入口共用：托盘菜单「唤醒」、「再启动一次 exe」、以及任何显式调用。
 * ⚠️ 球睡着时窗口是隐藏的，所以**点不到球** —— 早期"点睡着的球唤醒"那条
 * 交互随语义变更失效（不可见的元素收不到点击），注释保留在此以免被误当回归。
 *
 * 同样**不碰任务**：只是把承载它的窗口显示回来。
 */
async function wakeOrb(): Promise<void> {
  orbSleeping = false;
  applyOrbState();
  await invoke("orb_ui_action", { action: "wake" }).catch((e) =>
    console.error("[orbcat] 唤醒失败:", e),
  );
}

/**
 * 订阅后端的球状态广播。
 *
 * ⚠️ 必须订阅而不是只信本地变量：托盘菜单（Rust 侧）和「再启动一次 exe」
 * 都能改这个状态，前端不知道就会画出错的球（睡着却显示醒着）。
 */
function installOrbUiListener(): void {
  void listen<{ sleeping: boolean }>("orb-ui", (e) => {
    orbSleeping = !!e.payload.sleeping;
    applyOrbState();
  });
  // 启动补拉：前端重载后同步一次（后端状态是内存态，重启后恒为清醒）
  void invoke<{ sleeping: boolean }>("orb_ui_state")
    .then((s) => {
      orbSleeping = !!s?.sleeping;
      applyOrbState();
    })
    .catch(() => {});
}

/**
 * 「又启动了一次 exe」→ 展开面板。
 *
 * ## 为什么需要两条通道（修「睡眠态点击应用图标无效」）
 *
 * 后端收到第二实例信号时会 `emit("orb-open-panel")`，但**事件可能赶在前端
 * 就绪之前**（WebView 还在加载），那一次 emit 就丢了 —— 而第二实例已经退出，
 * 表现就是完全没反应。所以后端同时留了一个 `take_open_panel_request`
 * 标记：前端 `boot` 时拉一次，迟到的信号照样生效。
 *
 * ⚠️ `take` 语义（后端 `swap(false)`）：标记只生效一次，不会每次刷新都弹面板。
 */
function installOpenPanelListener(): void {
  void listen("orb-open-panel", () => {
    void openPanelFromOutside();
  });
  // 兜底：启动时问一次"刚才有没有人让我打开面板"
  void invoke<boolean>("take_open_panel_request")
    .then((pending) => {
      if (pending) void openPanelFromOutside();
    })
    .catch(() => {});
}

/**
 * 把面板打开到可用状态（供「又启动了一次」与「程序已睡着」使用）。
 *
 * 与球上点击展开的区别：
 * 1. **先退出睡眠** —— 睡着时窗口是**隐藏**的（语义变更后），
 *    后端 `wake` 会把它重新显示出来；前端这份 `orbSleeping` 也要跟上，
 *    否则球回来了、状态还写着"睡着"。
 * 2. **要等一轮事件循环** —— 后端刚 `apply_mode("panel")` 改完窗口 bounds，
 *    立刻切 DOM 会闪一帧错位（见第四十四阶段 bug 3 的同源教训：
 *    `set_window_mode` 之后到 DOM 切换之间不能有 await，但这里反过来，
 *    是后端先动窗口、前端后动 DOM，所以要给它落地的时间）。
 *
 * ⚠️ 全程**不碰任务**：不取消、不重启任何正在跑的一轮，只是把界面弄出来。
 */
async function openPanelFromOutside(): Promise<void> {
  if (orbSleeping) {
    await wakeOrb();
  }
  await new Promise((r) => setTimeout(r, 30));
  await expand();
  // 告诉后端"这次开面板我接住了" —— 它在等这个确认来决定要不要自愈退回球态
  // （不确认 = 前端不在场/启动失败 → 后端把窗口退回球状，免得留一个
  //  面板大小、里面画着球的坏窗口）
  void invoke("confirm_panel_opened").catch(() => {});
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

// ---------------- 正文图（模型在回复里发的图）----------------
//
// 与上面的「用户附图缩略图」是**两套**，刻意不合并：
//   - 缩略图：62×62 的小方块，320px 足够，缓存复用率高（同一条消息反复重绘）；
//   - 正文图：是**内容本身**（截图 / 图表 / 表情包），要看清，取 960px。
// 共用一份缓存的话，谁先用就把尺寸钉死了 —— 要么图糊、要么小缩略图白烧带宽。

/** 正文图缓存：`图片路径 → data URL`（`""` = 已判定读不出来，别再重试） */
const bodyImgCache = new Map<string, string>();
const bodyImgInflight = new Set<string>();

/** 正文图长边上限（px）—— 960 是"看得清"和"base64 不撑爆 IPC"之间的折中 */
const BODY_IMG_MAX_EDGE = 960;

/**
 * 同步取正文图缓存；没有就异步加载。
 *
 * 两个坑与 `thumbOf` 完全相同（都是血泪）：
 * 1. 命中判定必须用 `has()` —— 失败时存的是空串，用值判等会让"读不出来"的图
 *    每次重绘都重新请求，请求失败又触发替换 → 死循环。
 * 2. 加载完**只换那一个 `<img>` 的 src**，不重绘消息区。
 */
function bodyImgOf(path: string): string | null {
  if (bodyImgCache.has(path)) return bodyImgCache.get(path)!;
  if (!bodyImgInflight.has(path)) {
    bodyImgInflight.add(path);
    void invoke<string>("image_thumb", { path, maxEdge: BODY_IMG_MAX_EDGE })
      .then((url) => {
        bodyImgCache.set(path, url);
      })
      .catch(() => {
        bodyImgCache.set(path, "");
      })
      .finally(() => {
        bodyImgInflight.delete(path);
        patchBodyImg(path);
      });
  }
  return null;
}

/** 正文图就绪/失败后，就地换上 src（或换成"读不出来"标记） */
function patchBodyImg(path: string): void {
  const cached = bodyImgCache.get(path);
  if (cached === undefined) return;
  // 用 dataset 比对而不是属性选择器：路径里有反斜杠/空格/引号，
  // 选择器转义太容易出错（`thumbOf` 那边用的是同一个理由）
  document.querySelectorAll<HTMLImageElement>("img.md-img-local").forEach((img) => {
    if (img.dataset.img !== path) return;
    if (cached) {
      img.src = cached;
      // 摘掉标记，等于"这个占位已经兑现"—— hydrate 不会再重复处理它
      img.classList.remove("md-img-local");
      img.removeAttribute("data-img");
    } else {
      const span = document.createElement("span");
      span.className = "md-img-bad";
      span.textContent = "图（读不出来）";
      span.title = path;
      img.replaceWith(span);
    }
  });
}

/**
 * 扫描刚上屏的 HTML，触发其中**本地图占位**的异步加载。
 *
 * 必须在每次 `mdToHtml` 输出落到 DOM 之后调用（历史消息重绘、流式正文 patch）。
 * 只认 `img.md-img-local[data-img]`：外链图（`src` 直接就是 http(s)）和
 * 已经换好的图都不在范围内，所以重复调用是安全的（幂等）。
 */
/**
 * 这张本地图是不是**表情包** —— 判据是路径里有 `memes` 这一级目录。
 *
 * 为什么不用 alt 文本判断：alt 是模型抄的，它可能改写；
 * 路径是 `send_meme` 直接给的字节，不会撒谎。
 * 表情包要 QQ 那种**贴纸尺寸**（~160px），和工作产物图（截图/图表，全宽）
 * 是两种东西，CSS 用 `.md-img-meme` 区分。
 */
function isMemePath(path: string): boolean {
  return /[\\/]memes[\\/]/.test(path);
}

function hydrateBodyImages(root: ParentNode): void {
  root.querySelectorAll<HTMLImageElement>("img.md-img-local[data-img]").forEach((img) => {
    const path = img.dataset.img;
    if (!path) return;
    // 表情包走贴纸尺寸 —— 必须在换 src **之前**加类，避免尺寸跳一下
    if (isMemePath(path)) img.classList.add("md-img-meme");
    const cached = bodyImgOf(path);
    // 缓存命中就同步换上，省掉"先占位再替换"的那一下闪
    if (cached) {
      img.src = cached;
      img.classList.remove("md-img-local");
      img.removeAttribute("data-img");
    }
    // 未命中：保持无 src 的占位，等 `patchBodyImg` 加载完来换
  });
}


// ---------------- 渲染：面板态 ----------------

/**
 * 时间线：把一轮里「想了什么 / 做了什么」**按发生顺序**渲染出来。
 *
 * ## 为什么要按顺序（定稿结论）
 *
 * 以前是「一个思考折叠块 + 一个工具折叠行」，思考必然挤成一坨、工具另起一处，
 * 看不出"这一步的思考是为了调哪个工具"。现在条目自带顺序，渲染出来就是
 * `💭 思考 → 🔧 调工具 → ✅ 返回 → 💭 思考 → …` 的交错节奏。
 *
 * ## 折叠策略（三轮定稿）
 *
 * | kind | 来源 | 运行中（live） | 跑完（历史） |
 * |---|---|---|---|
 * | `reasoning` | agent.rs 每轮思维链 | 折叠 | 折叠 |
 * | `tool_call` | 工具调用 | 折叠成一行 `🔧 运行命令` | 折叠成一行 |
 * | `tool_result` | 工具返回 | 折叠成一行 `✅ 运行命令` | 折叠成一行 |
 * | `text` / `stream` | **中途正文** | **展开**（要盯着进度） | 在「过程」里，随外壳一起收起 |
 * | `status` | 限流退避等链路状态 | 常显（灰色说明） | 常显 |
 * | `tool_error` / `error` | 工具失败 / 本轮失败 | 常显（红黄） | 常显 |
 * | `omitted` | 后端省略了太早的步骤 | 灰色说明行 | 灰色说明行 |
 * | `steer` | 用户插话 | 黄边块 | 黄边块（留在「过程」外壳之外） |
 * | `assistant` | 最终答案（正文另有渲染） | 跳过 | 跳过 |
 *
 * ## 两级结构：跑完**整轮收成一行**
 *
 * 跑完（历史）时整轮包进 `details.run-process`，摘要就是 `过程 · N 步` ——
 * 定稿结论是"这个应该全部压缩掉"（此前摊平后回看一条消息
 * 要占十几行折叠行，正文反被顶下去）。
 *
 * - 运行中**不包外壳**：平铺，中途正文摊开（要盯着进度）。
 * - 跑完包外壳，**默认闭合**：一行搞定；点开才看得到里面的思考 / 工具 / 正文。
 * - 里面每一条**再各自折叠**（思考、工具行）—— 展开外壳不会一下子哗啦出一屏原文。
 *
 * ❌ 别再走回头路：① 外壳**不许默认展开**（第一轮报的就是它摊开着一堆
 * 工具参数）；② `reasoning` **不许**在 live 时自动展开（"思考应该让人选择展开"）。
 *
 * ## 兼容老数据
 *
 * - 折叠记录（落盘折叠机制时代留下的单条 `tool_call`，`detail` 以 `[已执行` 开头）
 *   → 整行显示，**不能**当 JSON 解析（会显示成乱码）
 * - 只有 `tool_call`、没有时间线条目的老消息 → 回退成原来的「调用了 N 个工具」
 *
 * @param live 运行中的气泡（中途）→ 平铺；否则收进 `details.run-process`
 * @param key  本条消息的稳定标识，拼进 `data-open-key` —— 没有它，两条消息里
 *             同序号的折叠块会共用 key，展开一个就把另一条里的也顶开（旧 bug）
 */
/** 超过这个步数就只渲染尾部，前面的点「展开更早的 N 步」现取 */
const TIMELINE_TAIL = 40;

/** openKey → 那一轮完整的 AgentStep[]，供「展开更早的 N 步」回头取 */
const timelineStore = new Map<string, AgentStep[]>();

function renderTimeline(items: AgentStep[] | undefined, live: boolean, key = ""): string {
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

  // 超长轮次：只出尾部，前面的按需现取。
  //
  // 为什么值得做：`renderItem` 会把每一步的 detail 拼成 HTML 字符串，而
  // `paintMessages` 在流式过程中会**反复**重绘 —— 221 步的长轮次等于
  // 每帧把整轮（含最长的 38,689 字符那一步）重拼一遍。
  // 折叠的 <details> 已经让浏览器跳过了布局和绘制，所以要省的是
  // **拼字符串和建节点**，不是渲染。
  //
  // 为什么是阈值而不是虚拟滚动：阈值以下的轮次（绝大多数）走原路径，
  // 一行代码不变，patch 逻辑也不用动；虚拟滚动要接管滚动容器高度，
  // 会和 patchTimeline 的 data-n 增量追加打架，得不偿失。
  if (!live && items.length > TIMELINE_TAIL) {
    const head = items.length - TIMELINE_TAIL;
    timelineStore.set(key, items);
    // 超长轮次走「只渲染尾部 + 按需取更早」的省流路径。
    // ⚠️ 原先这里会把"必须常显的正文"提到壳外，那套 `tkn`
    //    启发式已随 `midturn` 改名 + 后端提升一起删除（见 renderHistTimeline）。
    return `<details class="run-process" data-open-key="proc-${esc(key)}">
      <summary>过程 · ${items.length} 步</summary>
      <div class="run-timeline tl-hist" data-open-key="${esc(key)}" data-head="${head}">
        <button type="button" class="tl-more" title="再往前取 ${TIMELINE_TAIL} 步">展开更早的 ${head} 步</button>
        ${items.slice(head).map((s, i) => renderItem(s, head + i, key)).join("")}
      </div>
    </details>`;
  }

  if (!live) return renderHistTimeline(items, key);

  const body = items.map((s, i) => renderItem(s, i, key)).join("");
  if (!body) return "";
  return `<div class="run-timeline">${body}</div>`;
}

/**
 * 「过程」条目 —— 与 WorkBuddy 的 `foldable` 对齐：折起来不损失信息的东西。
 *
 * `error` / `tool_error` / `steer` **刻意不算** —— 出错的行和用户插话
 * 折进去就等于没人看得见（既有约定）。
 */
function isFoldableStep(s: AgentStep): boolean {
  return (
    s.kind === "reasoning" ||
    s.kind === "tool_call" ||
    s.kind === "tool_result" ||
    s.kind === "status" ||
    s.kind === "omitted"
  );
}

/**
 * 过程中途正文（**永远进「过程」折叠，不常显**）的判定。
 *
 * `midturn` = 模型中途写、然后接着调工具的正文（后端 `agent.rs` 落盘）。
 * 它**不是**答案本体 —— 答案走 `e.text` 单独常显。
 *
 * ⚠️ `text` 只为**读旧会话**保留（改名之前 midturn 就叫 text）。
 *    新一轮的 run 不该再产生 `kind:"text"`。
 */
function isMidturn(s: AgentStep): boolean {
  return s.kind === "midturn" || s.kind === "text" || s.kind === "stream";
}

/**
 * 跑完的整轮时间线 → HTML：**保序段切分 + 正文筛选**，不再把整轮塞进一个外壳。
 *
* ## 定稿规则
 *
* | 条目 | 去向 |
 * |---|---|
 * | 正文（`text`/`stream` 非空）中**最长 ∪ 最后一条** | 平铺常显（对标 WorkBuddy `tkn`） |
 * | 其余一切：中间播报、`reasoning`、工具（**含失败行**）、`status` | **一个**「过程 · N 步」外壳 |
 * | 轮级 `error`、`steer`（插话） | 平铺常显 |
 *
 * 渲染顺序：**过程折叠在最前，壳外条目按原序跟在壳后** —— 不做穿插
 * （对标实现里没有这种渲染法）。上一版的"段切分 + 落单平铺"产生
 * 「壳 / 正文 / 壳 / 正文」交替的多段结构，中间播报还会漏出来，已废弃。
 *
 * ## ⭐ 为什么这里不再有任何"正文常显"启发式
 *
 * 上午为了抢救"被折叠吞掉的核心内容"，这里加过 `tkn`（最长 ∪ 最后）——
 * 一度反复（正文全平铺 → 步骤播报刷屏 → 步骤 3/4 漏出来 → 表格两套渲染）。
 *
 * 根因（逆向对标实现后确认）：**items 里
 * 本来就没有答案本体**。WorkBuddy 一条消息 = 一个 `output` 数组，
 * 「最后一条 text」天然就是答案，所以它那边不需要猜；orbcat 的中途正文被
 * 拆成了独立 step（`kind:"midturn"`），答案另走 `e.text`，两者无结构对应。
 *
 * 所以正解不在前端：让**后端**把单薄收尾时的实质内容提升进 `e.text`
 * （`promote_thin_answer`），前端就不必再猜。壳外只留 `e.text` 一个来源。
 */
function renderHistTimeline(items: AgentStep[], key: string): string {
  // 定稿（逆向对标实现后的结果）：
  // **items 里没有任何"答案本体"** —— `midturn` 全是过程，`assistant` 走 `e.text`
  // 单独常显。所以壳外只剩轮级 error / 插话行，其余**统统进过程折叠**。
  // （原先的 `tkn`「最长 ∪ 最后」已删：那份职责移交给了后端的
  //   `promote_thin_answer`，它把答案直接搬进 `e.text`。）
  const isProcessStep = (s: AgentStep): boolean =>
    isFoldableStep(s) || s.kind === "tool_error" || isMidturn(s);

  const fold: number[] = [];
  /** 壳外但仍属"时间线"的条目（轮级 error / 插话行）—— 需要 .run-timeline 父节点 */
  const timeline: string[] = [];
  items.forEach((s, i) => {
    if (isProcessStep(s)) {
      fold.push(i);
      return;
    }
    timeline.push(renderItem(s, i, key));
  });

  const tl = timeline.length ? `<div class="run-timeline tl-hist">${timeline.join("")}</div>` : "";

  const shell = fold.length
    ? `<details class="run-process" data-open-key="proc-${esc(key)}">
      <summary>过程 · ${fold.length} 步</summary>
      <div class="run-timeline tl-hist">${fold.map((i) => renderItem(items[i], i, key)).join("")}</div>
    </details>`
    : "";

  return shell + tl;
}

/**
 * 「展开更早的 N 步」—— **prepend**，不整段重画。
 *
 * 整段重画会把 <details> 的展开状态、里面每个 details.tl-tool 的开合、
 * 以及思考块的 markdown 懒渲染状态全部打回原形；
 * prepend 只往前面插新节点，已有的那 40 步原封不动。
 */
function installTimelineMore(root: ParentNode = document): void {
  root.addEventListener("click", (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLElement>(".tl-more");
    if (!btn) return;
    const tl = btn.closest<HTMLElement>(".tl-hist");
    const openKey = tl?.dataset.openKey ?? "";
    const items = timelineStore.get(openKey);
    const cur = Number(tl?.dataset.head ?? "0");
    if (!tl || !items || !Number.isFinite(cur) || cur <= 0) return;

    const take = Math.min(TIMELINE_TAIL, cur);
    const from = cur - take;
    const html = items.slice(from, cur).map((s, i) => renderItem(s, from + i, openKey)).join("");

    // ⚠️ 顺序要紧：必须**先插入再决定按钮去留**。
    //    反过来写（先 remove 再 insertAdjacentHTML）时，按钮已经脱离文档，
    //    "beforebegin" 在无父节点的情况下是空操作 —— 那 20 步会静默丢失。
    tl.dataset.head = String(from);
    btn.insertAdjacentHTML("beforebegin", html);
    if (from > 0) {
      btn.textContent = `展开更早的 ${from} 步`;
    } else {
      btn.remove();
    }

    // 新插入的思考块同样要登记原文并补绑 toggle，否则点开不渲染
    reindexReasonRaw(tl, items);
    tl.querySelectorAll<HTMLElement>("details.run-reason").forEach(bindReasonToggle);
  });
}

/**
 * 工具名 → 面向用户的短标签。
 *
 * 折叠行上**只给这个**（原始工具名进 `title`）—— 用户不需要看 `grep_files`，
 * 他需要看「搜内容」。没映射到的（MCP 工具、用户自定义脚本工具）原样显示，
 * 猜错名字比显示真名更糟。
 */
const TOOL_LABELS: Record<string, string> = {
  read_file: "读取文件",
  list_dir: "列目录",
  glob_files: "找文件",
  grep_files: "搜内容",
  write_file: "写入文件",
  edit_file: "改文件",
  append_file: "追加内容",
  delete_file: "删除文件",
  move_file: "移动文件",
  copy_file: "复制文件",
  mkdir: "新建目录",
  file_info: "查文件信息",
  git: "Git",
  run_command: "运行命令",
  run_code: "运行脚本",
  web_search: "联网搜索",
  fetch_url: "抓取网页",
  request_access: "申请权限",
  capture_screen: "截图",
  view_image: "看图片",
  foreground_context: "看前台窗口",
  current_time: "取当前时间",
  remember: "记一笔",
  distill_move: "迁移记忆",
  life_items: "生活清单",
  ask_user: "问你",
  recall_turns: "翻历史",
  load_skill: "载入技能",
  save_skill: "保存技能",
  search_tools: "找工具",
  list_tool_groups: "列工具组",
};

/** 工具名 → 折叠行上的短标签（未知工具原样返回，别猜） */
function toolLabel(name?: string | null): string {
  const n = (name ?? "").trim();
  if (!n) return "工具";
  return TOOL_LABELS[n] ?? n;
}

/**
 * 这条工具返回是失败吗？
 *
 * 判据是后端 `agent.rs` 工具循环里的约定：工具 `Err` 时**不新增 kind**，
 * 而是把错误伪装成正常输出文本 `工具执行失败：…`（`tools.rs` 的 `ToolOutput::text`），
 * 所以磁盘上失败也是 `kind === "tool_result"`。前端要按"失败"给它常显红黄行。
 */
function isToolFailure(detail?: string | null): boolean {
  return (detail ?? "").trimStart().startsWith("工具执行失败");
}

/**
 * 把 `tool_call` + 紧跟的同名 `tool_result` **并成一条**（定稿：直接变成执行命令
 * 一条就行了）—— 以前一个工具占两行（`🔧 列目录` / `✅ 列目录`）。
 *
 * ## 为什么必须在**数据层**合并，不能只改渲染
 *
 * 时间线是严格 `1 条目 = 1 个顶层 DOM 节点` 的契约：`patchTimeline` 用
 * `data-n`（已渲染条目数）和 `data-i`（条目下标）定位节点，`nodeFromHtml`
 * 又只取 `firstElementChild`。渲染层做「两条目 → 一节点」会让下标与节点错位，
 * 流式期间就会出现重复/丢失的折叠块。
 *
 * ## 配对规则
 *
 * - **相邻 + 同名 + 非失败** → 合并成一条（保留 `tool_call` 的 kind 与参数，
 *   返回塞进 `result`）。后端工具循环天然是"先 push call、执行完 push result"，
 *   所以同名必相邻。
 * - **失败** → 不合并，且把 kind 归一成 `tool_error`，让渲染层画出**常显**的红黄行
 *   （失败被折进一行等于没人看得见，违背既有约定）。live 路径本来就是 `tool_error`，
 *   这里归一后两条路径形态一致。
 * - **孤立**（`cap_steps` 截断拆对 / 工具被取消 / 老数据）→ 原样保留：
 *   `renderItem` 的 `tool_call` 与 `tool_result` 分支都还能画。
 */
function pairToolSteps(items: AgentStep[] | undefined): AgentStep[] | undefined {
  if (!items || items.length === 0) return items;
  const out: AgentStep[] = [];
  for (let i = 0; i < items.length; i++) {
    const s = items[i];
    const next = items[i + 1];
    if (
      s.kind === "tool_call" &&
      next &&
      next.kind === "tool_result" &&
      (next.name ?? "") === (s.name ?? "") &&
      !isToolFailure(next.detail)
    ) {
      out.push({ ...s, result: next.detail, durationMs: next.durationMs ?? s.durationMs });
      i++; // 返回条目被吃掉，不再单独占一行
      continue;
    }
    if (s.kind === "tool_result" && isToolFailure(s.detail)) {
      out.push({ ...s, kind: "tool_error" });
      continue;
    }
    out.push(s);
  }
  return out;
}

/**
 * 单条时间线条目 → HTML（`renderTimeline` 与 `patchTimeline` 的共同契约）。
 *
 * `kp`（key prefix）= 本条消息的稳定标识，拼进 `data-open-key`。**必须带**：
 * 不带的话两条消息里同序号的折叠块共用同一个 key，`paintMessages` 还原展开状态时
 * 会把另一条消息里的同序号块也一起顶开（"我展开这一个，别的也开了"）。
 * 运行中的增量 patch 路径（`patchTimeline`）不需要 key，传空串即可。
 */
/**
 * 把流式正文渲染成 HTML：已收尾的块走 markdown，活动尾巴保持纯文本。
 *
 * 为什么要切一刀而不是整篇 `mdToHtml`：表格判定依赖"下一行是 `|---|`"，
 * 流式下同一张表会先被当段落、补上分隔行再突变成 `<table>`，列宽一变
 * 视线就跳。切开后每个块只在自己收尾时变一次。详见 `markdown.ts` 的 `splitStable`。
 */
function renderStreamingMd(src: string): string {
  const { stable, tail } = splitStable(src);
  // 两段拼起来：stable 已是安全 HTML，tail 必须 escape
  return mdToHtml(stable) + esc(tail).replace(/\n/g, "<br>");
}

/**
 * 思考块的 markdown **懒渲染**：折叠时不动，展开时才转。
 *
 * 幂等 —— 转完打 `data-md="1"`，重复调用直接返回。
 * 流式期间若用户正展开着看，`patchTimeline` 会带着新内容再调一次。
 *
 * ⚠️ 原文存 WeakMap 而不是 `data-raw` 属性：思维链动辄几万字，
 * 塞进 HTML 属性等于同时持有"转义后的属性值 + 文本节点"两份，
 * 而且每次 patchTimeline 重建节点都要重新序列化一遍。WeakMap 不进 DOM，
 * 节点被 GC 就一起回收。
 */
const reasonRawText = new WeakMap<HTMLElement, string>();

/** 渲染出一个「待懒渲染」的思考块 */
function renderReasonBody(detail: string): string {
  return `<pre class="run-reason-pre">${esc(detail)}</pre>`;
}

function renderReasonMdInPlace(details: HTMLDetailsElement): void {
  const pre = details.querySelector<HTMLElement>(".run-reason-pre");
  if (!pre || pre.dataset.md === "1") return;
  const raw = reasonRawText.get(pre);
  if (raw === undefined) return; // 已经是转换后的（或没经过懒渲染路径），不动
  pre.innerHTML = renderStreamingMd(raw);
  reasonRawText.delete(pre);
  pre.dataset.md = "1";
}

/** 全量重绘后重建 WeakMap（原节点已随 innerHTML 一起没了） */
function reindexReasonRaw(root: ParentNode, items: AgentStep[]): void {
  root.querySelectorAll<HTMLElement>(".run-reason-pre").forEach((pre) => {
    if (pre.dataset.md === "1") return; // 已转换的不用再存
    const i = Number(pre.closest<HTMLElement>("[data-i]")?.dataset.i ?? "-1");
    const src = items[i];
    if (src && src.kind === "reasoning") reasonRawText.set(pre, src.detail);
  });
}

function renderItem(s: AgentStep, idx = -1, kp = ""): string {
  // `data-i` 是增量 patch 的锚点（知道"第几条已画"），全量渲染时也要带上，
  // 否则一次全量重绘之后的流式更新会找不到节点、思考块就不再增长
  const di = idx >= 0 ? ` data-i="${idx}"` : "";
  switch (s.kind) {
    case "reasoning":
      if (!s.detail.trim()) return "";
      // ⚠️ **永远不自动展开**：思维链是"想看才点开"的东西，
      //    不管 live 还是历史。字数进 summary，看长度决定要不要点。
      //    增量 patch 只换 <pre> 文本、**不碰 open**，所以展开着不会被顶回去。
      //
      // 正文改走 **markdown 懒渲染** —— 折叠时是纯 <pre>，
      // 点开（`toggle` → `data-md="1"`）才转成 HTML 并打标记。
      // 为什么不在这里直接 `mdToHtml`：
      //   ① 思考默认折叠，转不转都看不见，转换纯属白做；
      //   ② 流式期间每 chunk 都要重跑一次转换（正文可到几万字），
      //      而折叠状态下没人看 —— 白烧 CPU 还会让 patch 变慢。
      // 详见 `mdToHtml` 上方 `splitStable` 的注释。
      return `<details class="run-reason tl-reason"${di} data-open-key="r-${esc(kp)}-${esc(s.name || "")}-${idx}">
        <summary>💭 思考过程<span class="run-reason-len">（${s.detail.length} 字）</span></summary>
        ${renderReasonBody(s.detail)}
      </details>`;

    case "tool_call": {
      // 工具调用**默认折叠成一行**：
      // 以前把参数 JSON 直接铺在时间线上（`🔧 调用 write_file {"content":"# -*- coding…`），
      // 那是给机器看的东西，不是给人看的。现在只留「🔧 运行命令」，
      // 参数进 `title` 与详情块，想看再点。
      //
      // 第二轮：**调用与返回并成一行**（`pairToolSteps` 已把返回塞进 `s.result`），
      // 所以一个工具只占一行 —— 前缀按有没有返回在 🔧 / ✅ 之间切，
      // 详情块里先参数后返回，点开一次两样都能看到。
      //
      // 第三轮（学 pi）：**加上耗时**（`Took 1.2s`）—— 工具跑了多久是用户很想知道的一件事，
      // pi 用 monotonic clock 测，我们后端给 `durationMs`。
      const args = s.detail ? summarizeArgs(s.detail) : "";
      const body = [
        s.detail
          ? `<pre class="tl-tool-pre">${esc(clipLines(s.detail, s.name || ""))}</pre>`
          : "",
        s.result
          ? `<pre class="tl-tool-pre tl-res-pre">返回：${esc(
              clipLines(s.result, s.name || ""),
            )}</pre>`
          : "",
      ].join("");
      const dur =
        s.durationMs !== undefined
          ? `<span class="tl-dur">· ${(s.durationMs / 1000).toFixed(1)}s</span>`
          : "";
      return `<details class="tl-tool tl-call"${di} data-open-key="tc-${esc(kp)}-${esc(s.name || "")}-${idx}">
        <summary title="${esc(s.name || "?")}${args ? ` · ${esc(args)}` : ""}">${
          s.result ? "✅" : "🔧"
        } ${esc(toolLabel(s.name))}${dur}${
          s.tick ? `<span class="tl-tick">${esc(s.tick)}</span>` : ""
        }</summary>${body}</details>`;
    }

    case "tool_result": {
      // ⚠️ 正常路径下**到不了这里** —— 返回已被 `pairToolSteps()` 并进上面那条
      //    `tool_call`（合成一行）。这里只兜**孤立返回**：`cap_steps` 超限截断把一对拆开、
      //    或老会话数据里本来就是孤儿。照样折成一行，别让它消失。
      const full = s.detail || "";
      const bodyFull = clipLines(full, s.name || "");
      const tip = full.split("\n")[0].slice(0, 90);
      const dur =
        s.durationMs !== undefined
          ? `<span class="tl-dur">· ${(s.durationMs / 1000).toFixed(1)}s</span>`
          : "";
      return `<details class="tl-tool tl-res"${di} data-open-key="tr-${esc(kp)}-${esc(s.name || "")}-${idx}">
        <summary title="${esc(tip)}">✅ ${esc(toolLabel(s.name))}${dur}</summary>
        ${bodyFull ? `<pre class="tl-tool-pre">${esc(bodyFull)}</pre>` : ""}
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

    case "steer": {
      // 插话附带的图（可选）：插话不铺独立气泡，这里是它唯一的展示位置。
      // 复用历史消息那套缩略图渲染（文件路径 → image_thumb 异步加载 + 就地替换）。
      const thumbs =
        s.images && s.images.length
          ? `<div class="thumbs tl-thumbs">${s.images
              .map((src, i) => renderThumb(src, i, false))
              .join("")}</div>`
          : "";
      // ⚠️ 不显示 `s.name`：steer 条目的 name 是**内部插话 id**（`st1790…`），
      //    它的用途是与 user 消息的 `steerId` 对齐（回填图片 / 标记已送达），
      //    画出来只会是一串看不懂的编号。
      void s.name;
      return `<div class="tl-steer"${di}>💬 插话：${esc(s.detail)}${thumbs}</div>`;
    }

    case "midturn":
    case "text":
    case "stream":
      // **中途正文**：本身不折叠 —— 运行中它就该摊着（要盯进度），
      // 跑完之后整轮已经被「过程」外壳收掉了（见 `renderTimeline`），
      // 所以这里再套一层折叠只会变成"点两次才读到一句话"。
      //
      // 中途正文**按 markdown 渲染**（它跟最终答复同源，
      // 模型在里面写 `## 分流总览` / 表格 / 代码块很常见，纯文本没法看）。
      // 走 `splitStable` 切一刀：已收尾的块转 HTML，活动尾巴保持纯文本 ——
      // 否则表格会在分隔行到达的瞬间整块突变，逐 chunk 看就是闪。
      // `.tl-text` 带 `md` class 复用 `.msg-body.md` 那套排版（见 styles.css）。
      if (!s.detail.trim()) return "";
      return `<div class="tl-text md"${di}>${
        s.name ? `<span class="tl-text-tag">${esc(s.name)}</span>` : ""
      }${renderStreamingMd(s.detail)}</div>`;

    // 最终答案的正文另有渲染（`e.text` → markdown），这里跳过免得重复
    case "assistant":
    default:
      return "";
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
 * ⚠️ 这里**不用** document 级事件委托：委托对卡片按钮不生效
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
  // 重建思考块懒渲染的原文索引（WeakMap 不随 innerHTML 存活，新节点要重新登记）
  // `data-mid` = 该消息在 entries 里的下标，与 renderMessagesInner 的 idx 一致
  el.querySelectorAll<HTMLElement>(".msg[data-mid]").forEach((box) => {
    const e = entries[Number(box.dataset.mid)];
    const tl = box.querySelector<HTMLElement>(".run-timeline");
    if (e && tl) reindexReasonRaw(tl, e.items ?? []);
    // 老数据兜底：只有单串 `reasoning`、没有 items 的历史消息
    const legacy = box.querySelector<HTMLDetailsElement>("details.hist-reason .run-reason-pre");
    if (legacy && e?.reasoning && !legacy.dataset.md) reasonRawText.set(legacy, e.reasoning);
  });
  el.querySelectorAll<HTMLDetailsElement>("details").forEach((d) => {
    const key = d.dataset.openKey ?? d.dataset.i ?? d.querySelector("summary")?.textContent?.slice(0, 24) ?? "";
    if (key && openKeys.has(key)) {
      d.open = true;
      d.dataset.userOpen = "1";
    }
    // 记录用户手动展开（E5/E7）；思考块顺带在这里做 markdown 懒渲染
    // （与 patchTimeline 的增量路径共用 bindReasonToggle，两边行为必须一致）
    d.addEventListener("toggle", () => {
      if (d.open) {
        d.dataset.userOpen = "1";
        if (d.classList.contains("run-reason")) renderReasonMdInPlace(d);
      } else delete d.dataset.userOpen;
    });
    // 重绘后 open 是被程序写回的，**不触发 toggle** —— 展开中的思考块要主动补转换
    if (d.open && d.classList.contains("run-reason")) renderReasonMdInPlace(d);
  });
  el.querySelectorAll<HTMLButtonElement>("button[data-continue]").forEach((btn) => {
    btn.addEventListener("click", () => {
      // ⚠️ 输入框的真实 id 是 `input`（renderPanel 里写死的）。
      //    曾写成 `#prompt-input`（不存在）→ 填字落空 → send() 读到空值
      //    撞 `if (!text) return` 静默退出 → 按钮"点了没反应"（曾因此报过 bug）。
      const ta = document.getElementById("input") as HTMLTextAreaElement | null;
      if (ta) {
        ta.value = "继续";
        draftInput = "继续";
      }
      void send();
    });
  });
  bindPermButtons(el);
  // 历史消息里模型发的图（`![说明](本地路径)`）→ 触发异步取图。
  // 与 `patchRunning` 里那次同源：谁把 HTML 落到 DOM 上，谁就负责 hydrate。
  hydrateBodyImages(el);

  // 闲聊模式：给**首播**的气泡挂上「逐条出现」动画（样式见 styles.css 的
  // `.bubble.play` + `bubble-in`）。延迟按序号递增，模拟人一条条发消息的节奏 ——
  // 这才是"像 QQ"的关键：切成 5 泡但一瞬间全冒出来，看着还是一坨。
  el.querySelectorAll<HTMLElement>(".msg.chatty[data-mid]").forEach((box) => {
    const key = `${currentSessionId}:${box.dataset.mid}`;
    if (bubblePlayed.has(key)) return;
    bubblePlayed.add(key);
    box.querySelectorAll<HTMLElement>(".bubble").forEach((b, i) => {
      b.classList.add("play");
      b.style.animationDelay = `${Math.min(i, MAX_BUBBLES - 1) * 380}ms`;
    });
  });
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

  // 闲聊模式：整块气泡区重画。
  // 为什么这里不做增量：气泡**数量**会随文本增长而变（多切出一段就多一个），
  // `data-n` 那套"只 append 新增条目"的锚点对不上号。闲聊正文短，重画代价可忽略。
  // 仍加一道 `innerHTML` 比较 —— 内容没变就别碰 DOM（否则每帧都在重建节点）。
  if (box.classList.contains("chatty")) {
    const bs = splitBubbles(run.streamText ?? "", false);
    const inner = bs.length
      ? bs
          .map(
            (b, i) =>
              `<div class="msg-body md bubble${
                i === bs.length - 1 ? " typing" : ""
              }">${renderStreamingMd(b)}</div>`,
          )
          .join("")
      : `<div class="msg-body bubble typing">…</div>`;
    if (box.innerHTML !== inner) box.innerHTML = inner;
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
    // ⚠️ 必须与 renderMessagesInner 的 running 分支逐字段一致（同一条渲染契约）：
    //    这里渲染 markdown，中途正文才有标题 / 表格 / 代码块排版。
    //    `splitStable` 切过的结果与流式长度相关，但两条路径拿的是同一个
    //    `run.streamText`，切法一致 → 输出必然一致。
    const html = t ? renderStreamingMd(t) : "";
    if (stream.innerHTML !== html) stream.innerHTML = html;
    stream.hidden = !t;
    stream.classList.toggle("discarded", !!run.streamDiscarded);
  }

  const note = box.querySelector<HTMLElement>(".run-note");
  if (note) note.hidden = !run.streamDiscarded;

  // 本帧可能新冒出来正文图占位（中途正文里的 `![说明](路径)`）→ 触发加载。
  // 放在最后：前面刚换过 `run-stream` 的 innerHTML，新节点得走一遍才知道要取图。
  hydrateBodyImages(box);
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
 *    唯一例外是折叠块的 `open`：全量渲染一律闭合（思考/工具/返回都默认折叠），
 *    而 patch **不碰 `open`** —— 用户手动展开之后不该被下一帧顶回去。
 */
function patchTimeline(tl: HTMLElement, items: AgentStep[]): void {
  const n = Number(tl.dataset.n ?? "0");

  // 状态回退（条数变少 / 被清空 / 出错）→ 老老实实全量重画这一段
  if (!Number.isFinite(n) || n < 0 || n > items.length) {
    tl.innerHTML = items.map((s, i) => renderItem(s, i)).join("");
    tl.dataset.n = String(items.length);
    reindexReasonRaw(tl, items);
    // 同 ②：innerHTML 重建的节点也得补绑 toggle，否则点开不渲染
    tl.querySelectorAll<HTMLElement>("details.run-reason").forEach(bindReasonToggle);
    return;
  }

  // ① 最后一条已画条目：思考条目正在流式增长 → 原地更新文本
  if (n > 0) {
    const last = items[n - 1];
    const node = tl.querySelector<HTMLElement>(`[data-i="${n - 1}"]`);
    if (last.kind === "reasoning" && node) {
      // 思考块正文可能已转成 markdown（展开过）。
      // 这种情况下 `textContent` 存的是"渲染后的文字"，不再等于 `last.detail`，
      // 拿它比较会永远不等 → 每帧都重写一遍。这里按 `data-md` 分流：
      //   未转换 → 只换 <pre> 文本（保持懒渲染的省流式开销）
      //   已转换 → 重新走一遍 renderStreamingMd，保持 markdown 形态
      const pre = node.querySelector<HTMLElement>(".run-reason-pre");
      const len = node.querySelector<HTMLElement>(".run-reason-len");
      if (pre) {
        const rendered = pre.dataset.md === "1";
        // ⚠️⚠️ **卡死修复**。
        //
        // 原来这里无条件读 `pre.scrollHeight` 来判断"是否贴底"，
        // 而读 scrollHeight / clientHeight 会**强制浏览器排版这个 <pre>** ——
        // 哪怕它此刻折叠着、内容根本看不见。
        // 模型在思考里连续吐字时，这个 <pre> 越长，
        // 每帧被强制排版的代价就越大 → 平方级增长 → 整个面板卡死。
        // （复现方式：让模型在思考里输出 100 个 "100" 就会卡住。）
        //
        // 现在：折叠时**不读**任何布局属性（`details.open` 是纯属性查询，不触发布局），
        // 只在真看得见的时候才判断贴底。
        const det = pre.closest<HTMLDetailsElement>("details");
        const visible = !det || det.open;
        const atBottom = visible
          ? pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24
          : true;
        if (rendered) {
          const next = renderStreamingMd(last.detail);
          if (pre.innerHTML !== next) pre.innerHTML = next;
        } else if (pre.textContent !== last.detail) {
          pre.textContent = last.detail;
          // 未转换的块也要登记原文 —— 用户随后点开时才能补渲染
          reasonRawText.set(pre, last.detail);
        }
        if (len) len.textContent = `（${last.detail.length} 字）`;
        // ⚠️ 自动跟随滚动**只在看得见时做**。
        //    `pre.scrollTop = pre.scrollHeight` 右边那个读取同样会强制排版，
        //    折叠时做它等于每帧排版一次看不见的长文本 —— 卡死的另一半来源。
        if (visible && atBottom) pre.scrollTop = pre.scrollHeight;
      }
    } else if (!node || node.outerHTML !== renderItem(last, n - 1)) {
      // 非思考条目照理不会再变；真变了就整条换掉（含首次由空串变成有内容的情况）
      const fresh = nodeFromHtml(renderItem(last, n - 1));
      if (fresh) {
        if (node) node.replaceWith(fresh);
        else tl.appendChild(fresh);
      }
    }
  }

  // ② 新增条目：纯 append
  for (let i = n; i < items.length; i++) {
    const node = nodeFromHtml(renderItem(items[i], i));
    if (node) {
      // ⚠️ 增量路径 append 的节点**不经过 paintMessages**，
      // 那里统一绑的 details 回调这里一个都没有 —— 不单独绑的话，
      // 运行中点开思考块不会触发懒渲染（`toggle` 无人监听）。
      if (items[i].kind === "reasoning") {
        const pre = node.querySelector<HTMLElement>(".run-reason-pre");
        if (pre) reasonRawText.set(pre, items[i].detail);
        bindReasonToggle(node);
      }
      tl.appendChild(node);
    }
  }
  tl.dataset.n = String(items.length);
}

/**
 * 给思考块绑「展开时懒渲染 markdown」。
 *
 * 与 `paintMessages` 里的 details 回调同源，抽出来是为了让全量重绘和
 * 增量 append 两条路径行为一致 —— 少一处就少一类"点开还是纯文本"的 bug。
 */
function bindReasonToggle(node: HTMLElement): void {
  const d = node as HTMLDetailsElement;
  if (!(d instanceof HTMLDetailsElement)) return;
  d.addEventListener("toggle", () => {
    if (d.open) {
      d.dataset.userOpen = "1";
      renderReasonMdInPlace(d);
    } else delete d.dataset.userOpen;
  });
  if (d.open) renderReasonMdInPlace(d);
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
 *
 * ⚠️ rAF **不是可靠的心跳**（症状：界面卡住不动，其实在跑）：
 * 窗口被最小化/完全遮挡时 WebView2 会暂停 requestAnimationFrame —— 回调
 * 可能迟迟不来甚至被丢弃。一旦被丢弃，`runPaintScheduled` 就永远卡在 true，
 * 后续所有 scheduleRunPaint() 直接 return —— 界面**彻底冻结**在某一帧，
 * 只有刷新才能恢复。所以加 250ms 的 setTimeout 看门狗：rAF 没来就用定时器兜底，
 * 两条路都调用同一个 flush（幂等，谁先到谁画）。
 */
let runPaintScheduled = false;

function scheduleRunPaint(): void {
  if (runPaintScheduled) return;
  runPaintScheduled = true;
  const flush = () => {
    if (!runPaintScheduled) return;
    runPaintScheduled = false;
    paintRunningNow();
  };
  requestAnimationFrame(flush);
  // ⚠️ 守卫：250ms 看门狗可能落在环境销毁之后，flush 会去摸 DOM。
  window.setTimeout(() => {
    if (!domAlive()) return;
    flush();
  }, 250);
}

/** 实际重绘逻辑（rAF / 看门狗共用） */
function paintRunningNow(): void {
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
  if (atBottom) {
    b.scrollTop = b.scrollHeight;
  } else {
    // 用户翻上去之后这一轮还在跑 → 「↓」该冒出来了。
    //
    // ⚠️ **一轮只加一次**：流式每个 token 都会走到这里，逐帧加会让数字
    //    一路狂跳，看着像来了一堆新消息。`chatMissedCountedForRun` 认当前 run。
    if (!chatMissedCountedForRun) {
      chatMissedCountedForRun = true;
      chatMissed += 1;
    }
    syncJumpButton();
  }
}

/** 绑定申请卡上的按钮（渲染后调用；每次重绘都会重新绑一批新的） */
function bindPermButtons(root: ParentNode): void {
  // ask_user 问题卡：点选项/发送/跳过 → 回答经 reason 通道回给模型
  root.querySelectorAll<HTMLButtonElement>(".perm-btn[data-act^='ask-']").forEach((btn) => {
    btn.addEventListener("click", () => {
      const card = btn.closest<HTMLElement>(".perm-card");
      const id = card?.dataset.perm;
      if (!id) return;
      const act = btn.dataset.act ?? "";
      if (act === "ask-option") {
        // 快捷选项：点一下即回答
        void decidePerm(id, "approve", btn.dataset.val ?? "");
      } else if (act === "ask-send") {
        const val = card.querySelector<HTMLInputElement>(".ask-input")?.value.trim() ?? "";
        if (!val) {
          card.querySelector<HTMLInputElement>(".ask-input")?.focus();
          return;
        }
        void decidePerm(id, "approve", val);
      } else if (act === "ask-skip") {
        const why = card.querySelector<HTMLInputElement>(".ask-input")?.value.trim() ?? "";
        void decidePerm(id, "deny", why || "（用户选择跳过）");
      }
    });
  });
  // 提问输入框：Enter 发送
  root.querySelectorAll<HTMLInputElement>(".ask-input").forEach((inp) => {
    inp.addEventListener("keydown", (e) => {
      if (e.key !== "Enter") return;
      e.preventDefault();
      const card = inp.closest<HTMLElement>(".perm-card");
      const id = card?.dataset.perm;
      const val = inp.value.trim();
      if (!id || !val) return;
      void decidePerm(id, "approve", val);
    });
  });

  root.querySelectorAll<HTMLButtonElement>(".perm-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const card = btn.closest<HTMLElement>(".perm-card");
      const id = card?.dataset.perm;
      if (!id) return;
      const act = btn.dataset.act ?? "deny";
      // ask 卡的动作在上面已单独绑定（回答走 reason 通道），这里绝不能再当"决定"处理
      if (act.startsWith("ask-")) return;

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
  // ⚠️ ask 卡的输入框也用了 perm-reason 的样式类 —— 它的 Enter 是「回答」不是「拒绝」，
  //    必须跳过（否则用户打字回车会被当成拒绝发出去）。
  root.querySelectorAll<HTMLInputElement>(".perm-reason").forEach((inp) => {
    if (inp.classList.contains("ask-input")) return;
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

/**
 * 已经播过「气泡逐条出现」动画的消息（key = `会话id:渲染下标`）。
 *
 * 为什么要记：`paintMessages` 在发消息 / 切视图 / 权限卡刷新时都会整块重建 DOM。
 * 无脑给气泡加动画 class，会让整屏**历史**气泡重新蹦一遍 —— 比不动画更难受。
 */
const bubblePlayed = new Set<string>();

/**
 * 当前会话是不是"聊天式呈现"。
 *
 * ⚠️ 用的是**当前会话**的模式（`currentModeId()`，模式跟会话走），
 *    不是"生成那条消息时的模式"—— 所以换到另一个模式的会话后回看历史消息，
 *    观感会跟着变（闲聊的一串气泡变回一大段）。这是刻意的取舍：要做到"按生成时的
 *    模式渲染"，得在落盘的每条消息上记一个 `chatty` 标记（改 `StoredMsg` 格式 +
 *    前端解析），收益只有"换会话后回看不变"。换会话是低频操作，不值这个复杂度。
 */
function isChattyMode(): boolean {
  return !!modesCache.find((x) => x.id === currentModeId())?.chatty;
}

function renderMessagesInner(): string {
  if (entries.length === 0) {
    // 未初始化时，空态要给出**可操作的下一步**，而不是通用引导语。
    // 用户不知道"agent-data 不存在"是什么意思，得直接把话说明白。
    if (needsInit) {
      // ⚠️ 文案跟着判据走：判据是**骨架标记缺失**，不是"目录不存在"。
      //    目录早被 `resolve_data_dir` 建出来了，说它"还不存在"是假的 ——
      //    用户看到这句会以为数据没了（2026-10-14 实测）。
      return `<div class="empty init-prompt">
        <div class="init-title">尚未初始化</div>
        <div class="init-body">
          数据目录 <code>agent-data/</code> ${dataDirExists ? "里还是空的" : "还不存在"} ——<br>
          没有人格文件、行为红线、用户画像、长期记忆。
        </div>
        <button class="init-go" id="init-go">建出目录结构</button>
        <div class="init-hint">
          点一下就行，这一步<b>不需要模型</b>；<b>已有的文件一个都不会被覆盖</b>。<br>
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
  // 「编辑」只挂在**最后一条已落盘的用户消息**上。
  // 进行中的气泡 / 排队插话没有 storedIdx，自然落选；有比它更新的用户消息也不给。
  const lastUserStored = entries.reduce(
    (m, e) =>
      e.role === "user" && e.storedIdx !== undefined && e.storedIdx > m ? e.storedIdx : m,
    -1,
  );
  return entries
    .map((e, idx) => {
      // 进行中的进度气泡
      //
      // ⚠️ 结构与 class 是 `patchRunning()` 的契约，两边必须逐字段一致：
      //    流式期间只 patch 这一个盒子，不再整块重建消息区（否则复制按钮会闪）。
      if (e.role === "running") {
        const streamText = e.streamText ?? "";

        // 闲聊模式：流式期间**就**按气泡渲染，而且不显示状态行 / 时间线。
        // 为什么不在跑完后再切：那样会出现"先是一大段，跑完突然裂成几泡"的跳变。
        //
        // ⚠️ 与 `patchRunning` 的 chatty 分支必须**逐字段一致**（同一条渲染契约）。
        // 流式期间传 `final = false`：不做"太短合并 / 超限合并"——
        // 那会让已经显示出来的气泡突然消失，看着像 bug。
        if (isChattyMode()) {
          const bs = splitBubbles(streamText, false);
          const inner = bs.length
            ? bs
                .map(
                  (b, i) =>
                    `<div class="msg-body md bubble${
                      i === bs.length - 1 ? " typing" : ""
                    }">${renderStreamingMd(b)}</div>`,
                )
                .join("")
            : `<div class="msg-body bubble typing">…</div>`;
          return `<div class="msg running chatty">${inner}</div>`;
        }

        return `
        <div class="msg running">
          <div class="run-head"><span class="spinner"></span><span class="run-head-text">${esc(
            e.text,
          )}</span></div>
          <div class="run-timeline" data-n="${(e.items ?? []).length}">${(e.items ?? [])
            .map((s, i) => renderItem(s, i))
            .join("")}</div>
          <div class="run-stream md${
            e.streamDiscarded ? " discarded" : ""
          }"${streamText ? "" : " hidden"}>${streamText ? renderStreamingMd(streamText) : ""}</div>
          <div class="run-note"${e.streamDiscarded ? "" : " hidden"}>已改调工具 · 仍可看</div>
        </div>`;
      }

      const chatty = isChattyMode();
      const cls = `${e.role}${e.queued ? " queued" : ""}${chatty ? " chatty" : ""}`;
      // assistant 的回复按 markdown 渲染（mdToHtml 内部已做 HTML 转义，可安全直插）
      const isMd = e.role === "assistant";
      // 闲聊模式：一条 assistant 拆成**多个气泡**（规则见 `bubble.ts`）。
      // 工作模式照旧一整块 —— 结构化长答案切成 5 个泡会非常难读。
      const bubbles = chatty && isMd && e.text ? splitBubbles(e.text) : null;
      const body = bubbles
        ? bubbles.map((b) => `<div class="msg-body md bubble">${mdToHtml(b)}</div>`).join("")
        : isMd
          ? mdToHtml(e.text)
          : e.text
            ? esc(e.text).replace(/\n/g, "<br>")
            : "";
      const imgs =
        e.images && e.images.length
          ? `<div class="thumbs">${e.images.map((s, i) => renderThumb(s, i, false)).join("")}</div>`
          : "";
      // 历史里的思考过程与步骤：整轮收进 `details.run-process`「过程 · N 步」，
      // **默认闭合**（定稿："这个应该全部压缩掉" —— 不留外壳的话，
      // 一条消息回看时要占十几行折叠行，最终答案被顶到屏幕外）。
      // 点开外壳之后，里面每条仍各自折叠（思考 / 工具行），不会一次哗啦出一屏。
      //
      // 新数据：`items`（磁盘上的 steps）就是完整时间线，思考与工具按序交错。
      // 老数据：只有单串 `reasoning` + 折叠行 → 回退成"一个思考块 + 一行摘要"。
      const hasReasonItem = (e.items ?? []).some((s) => s.kind === "reasoning");
      // key 用磁盘序号优先（`storedIdx` 跨重绘稳定），退而用本次渲染的下标 ——
      // 没有它，两条消息里同序号的折叠块会共用 key，展开一个会连带顶开别的消息。
      const tlKey = e.storedIdx !== undefined ? `s${e.storedIdx}` : `i${idx}`;
      const tl = renderTimeline(e.items, false, tlKey);
      const legacyReason =
        !hasReasonItem && e.reasoning
          ? `<details class="run-reason hist-reason" data-open-key="lr-${esc(tlKey)}"><summary>💭 思考过程<span class="run-reason-len">（${
              e.reasoning.length
            } 字）</span></summary>${renderReasonBody(e.reasoning)}</details>`
          : "";
      // 闲聊模式**不带过程**：思考块与工具时间线一并收掉，
      // 只留气泡 —— 否则还是"文档感"，不是聊天。
      const reason = chatty ? "" : `${legacyReason}${tl}`;
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
      <div class="msg ${cls}" data-mid="${idx}"${e.steerId ? ` data-steer="${e.steerId}"` : ""}>
        ${imgs}
        ${reason}
        ${
          // 闲聊模式下 `body` 已经是**若干个气泡**的 HTML（各自带 .msg-body），
          // 不能再套一层 .msg-body —— 那会变成"气泡里套气泡"。
          bubbles
            ? body
            : body
              ? `<div class="msg-body${isMd ? " md" : ""}">${body}</div>`
              : ""
        }
        ${
          e.continueRun
            ? `<div class="perm-actions" style="margin-top:6px"><button type="button" class="perm-btn ok" data-continue="1">继续</button></div>`
            : ""
        }
        ${
          // 操作键整组包在 .msg-more 里，**按钮本身一个不少**（class / data-* 全不变，
          // 委托处理器因此零改动，flow.timeline.test.ts 对 .msg.assistant .msg-regen
          // 的断言也照过）。显隐纯由 CSS 控制：悬停/聚焦才现形。
          //
          // 为什么不给它加一颗「⋯」按钮：项目里有过"只有 markup 没有 listener、
          // 点了毫无反应"的前科（flow.behavior.test.ts:70 的注释），
          // 宁可不加，也不摆一个假的。
          e.text || e.storedIdx !== undefined
            ? `<div class="msg-foot">${tokBadge}<span class="msg-more">${renderEditBtn(e, lastUserStored)}${renderForkBtn(e)}${renderRegenBtn(e)}${renderDelBtn(e)}${
                e.text
                  ? `<button type="button" class="msg-copy" data-idx="${idx}" title="复制这条消息">复制</button>`
                  : ""
              }</span></div>`
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
          // 「未跑完」角标：进程在轮内被关掉（"退出不丢轮"保下来的半截轮）。
          // 必须与 interrupted 区分开 —— 那个是用户点了停止，这个是进程没了。
          e.partial
            ? `<div class="partial-tag" title="这轮在进程退出时被截断，以下是当时已产出的部分（提问与步骤都已保留）">（未跑完：进程退出时中断）</div>`
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


const PERM_RISK_LABEL: Record<string, string> = {
  low: "低风险",
  medium: "中风险",
  high: "高风险",
};

function renderPermCards(): string {
  return pendingPerms
    .map((p) =>
      p.kind === "ask"
        ? renderAskCard(p)
        : p.kind === "command"
          ? renderCommandPermCard(p)
          : p.kind === "mcp"
            ? renderMcpPermCard(p)
            : renderFilePermCard(p),
    )
    .join("");
}

/**
 * ask_user 问题卡：模型向用户提问并等回答。
 *
 * 与权限卡的区别：没有批/拒，只有**回答**。快捷选项点一下即回；
 * 也可以打字自由回答（Enter 发送）。跳过 = 明确告诉模型"不答"。
 */
function renderAskCard(p: PendingPerm): string {
  const deadline = p.createdAt + p.timeoutMs;
  const rest = Math.max(0, deadline - Date.now());
  const mm = Math.floor(rest / 60000);
  const ss = String(Math.floor((rest % 60000) / 1000)).padStart(2, "0");

  const opts = (p.options ?? [])
    .map(
      (o) =>
        `<button type="button" class="perm-btn ok" data-act="ask-option" data-val="${esc(o)}">${esc(o)}</button>`,
    )
    .join("");

  return `
  <div class="perm-card perm-card-ask" data-perm="${esc(p.id)}">
    <div class="perm-head">
      <span class="perm-title">💬 向你提问</span>
      <span class="perm-timer" data-deadline="${deadline}">⏱ ${mm}:${ss}</span>
    </div>
    <div class="perm-row"><span class="perm-k">问题</span><span class="perm-v">${esc(
      p.question || p.command,
    )}</span></div>
    ${
      p.reason
        ? `<div class="perm-row"><span class="perm-k">原因</span><span class="perm-v">${esc(p.reason)}</span></div>`
        : ""
    }
    <div class="perm-actions">
      ${opts}
    </div>
    <div class="perm-actions">
      <input class="perm-reason ask-input" type="text" placeholder="或在这里打字回答…" />
      <button type="button" class="perm-btn ok" data-act="ask-send">发送</button>
      <button type="button" class="perm-btn" data-act="ask-skip">跳过</button>
    </div>
    <div class="perm-hint">Enter 发送 · 「跳过」= 不回答（AI 会按默认继续）</div>
  </div>`;
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
  // ⚠️ `atBottom` 必须在**重绘之前**问：真浏览器里 `innerHTML=` 会把 scrollTop
  //    清 0，重绘之后再问恒为 false，「在底部就跟随」这个闸门就成了摆设
  //    （2026-10-14 复查时发现的）。
  const wasAtBottom = isChatAtBottom();
  if (host) {
    host.innerHTML = renderPermCards();
    bindPermButtons(host);
  } else {
    // 老布局：卡片区可能嵌在消息流里 —— 兜底整绘
    repaintChatKeepScroll();
  }
  // 权限卡出现/消失时若用户在底部，跟一下；否则别抢滚动
  if (wasAtBottom) b.scrollTop = b.scrollHeight;
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

/**
 * 权限倒计时用的定时器句柄。
 *
 * ⚠️ 为什么必须存下来（2026-10 修）：以前是裸 `window.setInterval(...)`，
 * 句柄直接丢掉 —— 于是**每次 boot() 都留下一个永不停止的 1s 定时器**。
 * 生产上它一直空转（`querySelectorAll` 找不到 `.perm-timer` 就什么也不做），
 * 测试里则更糟：多个用例各自 boot 一次 → 一堆定时器在**环境拆掉之后**
 * 才触发 → `document is not defined` 未捕获异常 → 整个 vitest 进程退出码 1。
 *
 * 存句柄 + 重入前先清，让 boot() 幂等。
 */
let permTimer: number | null = null;

/// 待拍板条数轮询的定时器句柄（同 `permTimer`：必须存下来，见 boot 里的说明）。
let pendingPollTimer: number | null = null;

/** 倒计时：只改文本，不重绘整个消息区 */
function startPermTimer(): void {
  if (permTimer !== null) window.clearInterval(permTimer);
  permTimer = window.setInterval(() => {
    // ⚠️ 环境已拆掉时**自杀**，不要继续跑（2026-10 修）。
    // 生产上对应"窗口正在卸载、定时器刚好触发"这一瞬；
    // 测试里则是 jsdom 环境拆除后定时器仍在队列里 —— 那时 `document`
    // 和 `window` **都没了**，碰任何一个都是未捕获异常，
    // 会把整个 vitest 进程的退出码变成 1。
    //
    // ⚠️ 连 `clearInterval` 都不能用 `window.` —— 那正是这里踩到的第二个坑
    //    （第一版写 `window.clearInterval`，于是"自杀"那句自己又炸了）。
    //    用裸 `clearInterval`：它是全局函数，Node 环境下仍然存在。
    if (typeof document === "undefined" || !document.body) {
      if (permTimer !== null) clearInterval(permTimer);
      permTimer = null;
      return;
    }
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
  const m = curModel();
  const needHint = pendingImages.length > 0 && m && !m.supportsImages;
  el.style.display = needHint ? "block" : "none";
  if (needHint) {
    el.textContent = `当前模型 ${m!.id} 不支持图片，将只发送图片路径（不会被识别内容）`;
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

/**
 * 只刷新顶栏的模型体现区（切模型后调）。
 *
 * 为什么不直接 renderPanel()：那会重建整块 DOM —— 对话流全部重绘、滚动位置丢失、
 * 输入框内容也没了。切个模型代价太大，所以只改 chip 那几个文本节点。
 */
function renderPanelRefreshChip(): void {
  const chip = document.getElementById("model-chip");
  if (!chip) return;
  const cur = curModel();
  const nameEl = chip.querySelector(".mc-name");
  if (nameEl) nameEl.textContent = cur ? cur.id : selectedModel || "（未选模型）";
  // 上下文 / host / 功能标签都不占版面，只更新 tooltip（chip 已是单行小控件）
  const ctx = cur?.maxInputTokens ? `${Math.round(cur.maxInputTokens / 1000)}K` : "";
  const host = cur?.url ? shortHost(cur.url) : "";
  const tags = `${cur?.supportsToolCall ? "工具" : ""}${
    cur?.supportsImages ? (cur?.supportsToolCall ? " / 视觉" : "视觉") : ""
  }`;
  chip.title = `模型：${cur ? cur.id : "（未选模型）"}${host ? ` · ${host}` : ""}${
    ctx ? ` · 上下文 ${ctx}` : ""
  }${tags ? ` · ${tags}` : ""} —— 点开选模型（共 ${models.length} 个）`;
  // 下拉开着的话，✓ 的位置可能变了
  paintModelsDropdown();
}

/**
 * 打开「模型管理」独立窗口。
 *
 * 入口（改定）：**设置菜单「🧩 模型」**、空态按钮、会话没模型时的兜底。
 * ❌ 不再是"点模型卡片本体" —— 那一下现在开的是下面这个**模型列表**。
 *
 * 失败时既弹 toast 给用户看，也落盘到 `agent-data/diag.log`：
 * 排查时截图可能拿不到，日志文件是可靠通道。
 */
function openModelsWindow(): void {
  void invoke("models_window_open").catch((err) => {
    const msg = `打开模型管理失败：${err}`;
    showToast(msg, "error");
    void invoke("diag_log", { msg: `main.ts ${msg}` }).catch(() => {});
  });
}

/** 「模型列表」底部那颗「添加自定义模型」：开窗口 + 让窗口**直接落在添加表单**上 */
function openModelsAdd(): void {
  try {
    localStorage.setItem(MODELS_INTENT_KEY, "add");
  } catch {
    // 拿不到 localStorage（配额/隐私模式）就当普通打开，别让添加流程整个走不通
  }
  openModelsWindow();
}

/**
 * 模型列表（点 chip 本体或 ▾ 都出这个）：**只选不管** —— 谁被点谁成为当前模型。
 *
 * ## 条目长相（定稿）
 *
 * 每条 = 「组别 + 模型名」两行，与工具栏那颗 chip **同构**（就是这个 chip 的
 * 罗列）。所以组别再按"撞名时才显示"处理，一律显示。
 * 纯模型名一行（旧形态）看起来像一堆裸 id，认不出是哪个通道的。
 *
 * ## 管理入口不在这儿
 *
 * 增删改仍然只在独立「模型管理」窗口（设置 › 🧩 模型）。这里唯一多出来的
 * 是底部那颗 **＋ 添加自定义模型** —— 它分隔线以下单独一行，不参与"选择"这件事。
 */
/**
 * 模型下拉的绘制。锚点默认是顶栏的模型 chip；设置页那行用 `set-model-chip`
 * （设计稿 04:2:806 的行内下拉）—— 同一套渲染，只是锚点不同。
 */
function paintModelsDropdown(anchorId = "model-chip"): void {
  document.getElementById("models-dropdown")?.remove();
  if (!modelsDropdownOpen) return;
  const chip = document.getElementById(anchorId);
  if (!chip) return;
  // 当前模型解析成**唯一一条**（提到 map 外面：它跟行无关，别每行算一次）
  const cur = currentModelRef();
  const rows = models.length
    ? models
        .map((m) => {
          const isCur = cur !== null && m.id === cur.id && sameUrl(m.url, cur.url);
          return `
        <button class="mc-mi${isCur ? " cur" : ""}" data-id="${esc(m.id)}" data-url="${esc(
          m.url,
        )}" title="${esc(m.id)} @ ${esc(m.url)}">
          <span class="mc-mi-text">
            <span class="mc-mi-group">${esc(modelGroupLabel(m.url))}</span>
            <span class="mc-mi-name">${esc(m.id)}</span>
          </span>
          ${isCur ? '<span class="mc-mi-check">✓</span>' : ""}
        </button>`;
        })
        .join("")
    : '<div class="mc-mi-empty">（还没有模型，点下面添加）</div>';
  const dd = document.createElement("div");
  dd.className = "mc-menu";
  dd.id = "models-dropdown";
  dd.innerHTML = `${rows}
    <div class="mc-menu-sep"></div>
    <button class="mc-mi mc-mi-add" data-add="1" title="添加一个 OpenAI 兼容的模型（Base URL / 模型 ID / Key）">
      <span class="mc-mi-text"><span class="mc-mi-name">＋ 添加自定义模型</span></span>
    </button>`;
  chip.appendChild(dd);
  // 只给「模型」条目绑选择：`[data-id]` 把底部那颗添加按钮排除在外
  dd.querySelectorAll<HTMLButtonElement>(".mc-mi[data-id]").forEach((b) =>
    b.addEventListener("click", async (e) => {
      e.stopPropagation();
      selectedModel = b.dataset.id!;
      selectedModelUrl = b.dataset.url ?? null;
      modelsDropdownOpen = false;
      paintModelsDropdown();
      renderPanelRefreshChip();
      renderImageHint(); // 换模型可能改变「支不支持图片」
      await invoke("set_selected_model", { id: selectedModel, url: selectedModelUrl }).catch(
        (err) => pushEntry("error", `保存模型选择失败：${err}`),
      );
    }),
  );
  dd.querySelector<HTMLButtonElement>(".mc-mi-add")?.addEventListener("click", (e) => {
    e.stopPropagation();
    modelsDropdownOpen = false;
    paintModelsDropdown();
    openModelsAdd();
  });
}

function renderPanel(): void {
  // 当前模型 —— 工具栏最右那颗 chip。
  // 整颗 chip（本体 + ▾）= **模型列表**的开关（组别+模型名的罗列 + 底部添加自定义模型）。
  // 管理（增删改/测试/分组）在独立窗口，入口是 设置 › 🧩 模型。
  const cur = curModel();
  const curName = cur ? cur.id : selectedModel || "（未选模型）";
  const curCtx = cur?.maxInputTokens ? `${Math.round(cur.maxInputTokens / 1000)}K` : "";
  // 分组显示名（可在模型管理里给每个 Base URL 起名；取不到回退 host）
  const curGroup = modelGroupLabel(cur?.url);
  const curTags = `${cur?.supportsToolCall ? "工具" : ""}${
    cur?.supportsImages ? (cur?.supportsToolCall ? " / 视觉" : "视觉") : ""
  }`;
  // chip 显示「组别 + 模型名」（定稿）：
  //   组别在上、模型名在下（两行紧凑排），或者空间不够时省略组别。
  //   上下文 / host / 功能标签仍全在 title tooltip 里。
  const modelTip = `模型：${curName}${curGroup ? ` · 组别 ${curGroup}` : ""}${
    curCtx ? ` · 上下文 ${curCtx}` : ""
  }${curTags ? ` · ${curTags}` : ""} —— 点开选模型（共 ${models.length} 个）`;
  const modelBtn = `
    <button class="model-chip" id="model-chip" title="${esc(modelTip)}">
      ${curGroup ? `<span class="mc-group">${esc(curGroup)}</span>` : ""}
      <span class="mc-name">${esc(curName)}</span>
      <span class="mc-caret">▾</span>
    </button>`;

  // ── 内核切换 chip（pi ↔ 自研）────────────────────────────────
  //
  // 为什么要给用户看这个：两个内核的**行为**确实不同
  // （pi 有自己的 compaction / codemode / 扩展生态；自研的那套是 orbcat 独有的
  //  记忆蒸馏 / PTC / 时间线折叠规则）。用户有权知道自己在用哪个。
  // 模式 chip（2026-10-23 新增）—— **常驻可见**。
  //
  // 为什么要它：模式是**会话级钉死不可改**的（项目铁律），但以前只在
  // 「＋新建会话」菜单里选，选完界面上**看不出当前是什么模式** ——
  // 不可见就等于不可控。模式 chip 补上这个缺口。
  //
  // 为什么**不是下拉**：模式不可改，给个下拉是骗人。点了只提示"建会话时定"。
  // 想换模式 → 走「＋」新建一条会话。
  //
  // 状态点用**模式色**（见 styles.css 的 --mode-standard 等）：
  // 薄荷=标准 / 沙金=闲聊 / 珊瑚=创作 / 靛蓝=PTC。远看只凭色点就能分辨。
  const curModeId = currentModeId();
  const curModeName = currentModeName();
  const modeChip = `
    <button class="mode-chip" id="mode-chip" data-mode="${esc(curModeId)}"
      title="当前模式：${esc(curModeName)}（建会话时定，不能改）"><span class="mc-dot"></span><span class="mc-mode-name">${esc(
      curModeName,
    )}</span></button>`;

  // 「无任务空间」/ 各项目（新建走独立「＋」菜单，select 只显示真实项目，永远选中当前）
  //
  // ⚠️ 第一项叫「**无任务空间**」而不是「主对话」（2026-10-21）：
  // 它指的是「**当前没有激活任何项目**」这个状态，不是「主聊天那条会话」。
  // 旧名把两件事混成一句 —— 而会话页那一组早就改叫「无任务空间」了，
  // 同一个概念在两个地方两个名字，用户看到的和它实际指的是两回事。
  //
  // 每项后面带**会话数**（学习别家的空间管理做法，2026-10-21 用户给的参考图）：
  // 那张参考图里下拉就是一列空间名，但**一个数都没有** —— 选之前你不知道
  // 哪个空间里有东西、哪个是空的。补上条数之后，这个下拉从「一个下拉框」变成
  // 「一张空间总览」：条数是 0 的空间一眼就知道是空的。
  // 顺带一个好处：收起状态下那颗 chip 也就直接写着「论文 · 12」，不用展开就知道。
  const spaceCounts = sessionCountBySpace();
  const projOpts = [
    `<option value=""${activeProjectId == null ? " selected" : ""}>无任务空间 · ${
      spaceCounts.get(null) ?? 0
    }</option>`,
    ...projects.map(
      (p) =>
        `<option value="${p.id}"${p.id === activeProjectId ? " selected" : ""}>📁 ${esc(
          p.name,
        )} · ${spaceCounts.get(p.name) ?? 0}</option>`,
    ),
  ].join("");
  const projTitle =
    activeProjectId == null
      ? "无任务空间"
      : (projects.find((p) => p.id === activeProjectId)?.name ?? "无任务空间");

  app.innerHTML = `
    <div class="panel">
      <!-- 标题条 = 面板里**唯一**一条 chrome（合并后）。
           原来 header / toolbar / fg-ctx / sub-bar / bg-run 是五条各自独立的带，
           最多同时出现四条 —— 在 600px 高的窗口里正文还没开始就被吃掉 ~130px。
           现在：项目 select + 三个入口 + 模型 chip 全收进这一行；
           fg-ctx / sub-bar / bg-run 移进第二行容器，三者各自独立显隐，
           **全隐藏时容器高度为 0**，不给正文留任何空高度。

           ⚠️ 高度是 flex 分配的（.panel-body flex:1），所以这三块从"带"变成
           "行内元素"不需要动任何 JS —— 它们本来就是各自 getElementById
           单独切 display 的。 -->
      <div class="panel-header" id="panel-header">
        <!-- 左组：空间 + 模式。**两件事都是"我在哪 / 我是谁"，必须挨着**。
             旧的写法是 select + 三个按钮 + 一个 ph-flex 弹性占位，
             结果空间和模式之间被撑出一个大洞 —— 视觉上像缺了什么。
             现在改成左右两组，组内紧凑，组间才拉开。 -->
        <div class="ph-left">
          <select id="project-select" title="当前：${esc(projTitle)} —— 跟当前会话走；选别的会跳到那个空间的会话">
            ${projOpts}
          </select>
          ${modeChip}
        </div>
        <!-- 右组：模型 + 设置 + 收起
             设计稿 02（2:1595 右侧组）：**就这三件**。
             内核 chip 不在设计稿里 → 挪进设置页「行为与安全」组，
             不占标题条（真机实报：它把空间 chip 的文字都挤没了）。 -->
        <div class="ph-right">
          ${modelBtn}
          <button class="panel-btn" id="btn-gear" title="设置 / 模型">⚙</button>
          <button class="panel-btn" id="btn-close" title="收起">—</button>
        </div>
      </div>

      <!-- 第二行：有内容才占高度。前台应用 / 子页标题 / 后台任务。 -->
      <div class="panel-sub">
        <div class="fg-ctx" id="fg-ctx" title="用户当前前台应用（点击复制路径）" style="display:none"></div>
        <!-- 非对话视图的**固定**标题条。
             与 #fg-ctx 同位置、互斥显示：对话视图显示前台应用，其余视图显示
             当前页标题 + 返回。关键差别是**它在 #panel-body 之外** —— 返回按钮
             不再跟着内容滚，滚到页面底部也够得着（曾因此报过 bug）。 -->
        <div class="sub-bar" id="sub-bar" style="display:none"></div>
        <!-- 「另一个会话在后台跑」提示条（切走后自动出现，点击切回） -->
        <div class="bg-run" id="bg-run" style="display:none"></div>
      </div>

      <div class="panel-body" id="panel-body"></div>

      <!-- 底部状态栏（设计稿 04/05/06/07/09）：内容区**之外**的一条固定窄带。
           为什么固定不跟着内容滚：它是「这一页的仪表盘」（多少条 / 几组 / 搜什么），
           滚走就看不见了 —— 与标题条同一条理由（返回按钮也是因此挪到 #sub-bar 的）。
           没有状态栏的页面（对话 / 备份回收站）整条隐藏，不占高度。 -->
      <div class="panel-status" id="panel-status" style="display:none"></div>

      <div class="thumbs" id="thumbs"></div>
      <div class="img-hint" id="img-hint" style="display:none"></div>

      <div class="panel-input">
        <!-- 「命令面板」入口：**一颗**小 logo。
             定稿（2026-10-23）：这颗按钮从「行为」改成「命令面板」——
             行为（执行位置/模式/权限）已经整块搬进设置 › 行为页，输入行不该占着它；
             而命令面板是**高频入口**，点开就是菜单，**不用记任何快捷键**
             （用户原话："用你个软件还要背这么多快捷方式"）。
             打斜杠也能开同一个面板 —— 按钮是给不想记斜杠的人的。 -->
        <button id="btn-commands" class="mini-btn commands-btn" title="命令面板（也可以在输入框打 /）">${setIcon("commands")}</button>
        <textarea id="input" rows="1" placeholder="问点什么…"
          title="Enter 发送 / Shift+Enter 换行 / Ctrl+V 贴图"></textarea>
        <button class="mini-btn" id="btn-grab" title="截取当前屏幕">截</button>
        <!-- 设计稿 03 的输入行就这 4 件：命令面板按钮 · 输入框 · 截图 · 发送。
             「＋新建」「史会话」**不在这里** —— 它们在命令面板里
             （/new 新建会话、/sessions 会话列表），点开即菜单，不用记快捷键。 -->
        <button id="btn-stop" title="停止生成" hidden>■</button>
        <button id="btn-send" title="发送">↵</button>
      </div>

      <!-- 轻提示层：非对话页（设置/记忆/会话）里的临时信息走这里，
           绝不塞进对话流。见 pushEntry 里的说明。 -->
      <div class="toast-wrap" id="toast-wrap"></div>

      <!-- 「↓ 回到底部」：只有翻上去时才出现（由 syncJumpButton 控制 hidden）。
           ⚠️ 放在输入区外面：panel-input 是 flex 容器，塞进去会被拉伸布局挤变形。
           绝对定位贴右下角，压在消息区底部一点 —— 半透明，不遮内容，hover 才实心。 -->
      <button class="jump-btn" id="jump-btn" hidden title="回到最新消息">↓</button>
    </div>
  `;

  attachDrag(document.getElementById("panel-header")!, () => {
    /* 标题栏只拖动 */
  });
  // 滚动记账要在 innerHTML 落地**之后**才挂得上（此时才拿得到 #panel-body）。
  // `renderBody` 会把它整个换掉，所以这里每次都调；函数内部有重复挂防护。
  installScrollMemo();

  // 「↓ 回到底部」：点了就贴底并清零计数
  document.getElementById("jump-btn")?.addEventListener("click", () => {
    scrollToBottom();
    chatMissed = 0;
    syncJumpButton();
  });

  document.getElementById("btn-close")!.addEventListener("click", () => void collapse());
  document.getElementById("kernel-chip")?.addEventListener("click", () => void toggleKernel());
  document.getElementById("btn-grab")!.addEventListener("click", () => void grabScreen());
  // 「测」「忆」的工具栏入口已删：
  //   测试连通性 → 模型管理窗口（设置 › 🧩 模型，每行一颗「测」）
  //   记忆审批   → 设置入口页「🧠 记忆」
  //
  // ⚠️ 「＋新建」「史会话」两颗也按设计稿 03 从输入行删了（输入行就 4 件）。
  //    它们的入口在**命令面板**里：`/new` 新建会话、`/sessions` 会话列表。
  //    所以这里没有按钮可绑 —— 下面 `runCommand("new")` 直接调 `showNewMenu`。

  // 模式 chip：点了**不换模式**（会话级钉死不可改），只说明怎么换。
  // 给个下拉是骗人 —— 用户点开发现改不了，比不显示更糟。
  document.getElementById("mode-chip")?.addEventListener("click", () => {
    showToast(
      `当前模式「${currentModeName()}」在建会话时就定了，不能换。想换 → 命令面板里「新建会话」。`,
      "info",
      3200,
    );
  });

  // 当前会话信息挂到**命令面板按钮**的 tooltip 上（输入行没有「史」了）。
  const curSess = sessionList.find((s) => s.id === currentSessionId);
  const cmdBtn = document.getElementById("btn-commands");
  if (cmdBtn && curSess) {
    const tag = curSess.kind === "main" ? "主聊天" : curSess.kind === "fork" ? "分支" : "任务";
    cmdBtn.title = `命令面板（新建会话 / 会话列表都在里面）· 当前：${tag}「${curSess.title}」· 模式：${currentModeName()}`;
  }
  document.getElementById("btn-gear")!.addEventListener("click", () => void switchView("settings"));

  // 模型 chip（改定）：
  //   整颗 chip（本体 + ▾）→ 出**模型列表**（组别+模型名的罗列，底部带「添加自定义模型」）
  //   管理窗口不在这儿了 —— 走 设置 › 🧩 模型
  const chip = document.getElementById("model-chip");
  chip?.addEventListener("click", (e) => {
    // 菜单就挂在 chip 里面，点菜单内部的按钮由它们各自的 handler 处理，
    // 这里必须放行，否则会把刚点开的菜单又切掉
    if ((e.target as HTMLElement).closest(".mc-menu")) return;
    modelsDropdownOpen = !modelsDropdownOpen;
    paintModelsDropdown();
  });
  if (modelsDropdownOpen) {
    paintModelsDropdown();
  }

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
    // 选项目 = "去那个项目干活"。项目里已有会话就直接落到**最新那条**。
    //
    // 为什么不能只停在会话页让用户自己点（2026-10-17 用户报）：
    // 他选了项目没点会话就「返回对话」→ 回到的还是**别的项目**的会话，
    // 顶栏却写着刚选的项目 —— 记忆与目录授权一起串到别的项目上。
    // `alignProjectToSession()` 只能在回对话时把顶栏拉回来（那是兜底），
    // 这里直接把他送到对的地方，才是"选项目"这个动作本来的意思。
    const target = latestSessionIn(id);
    if (target && target.id !== currentSessionId) {
      await switchSession(target.id);
      return;
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
    // 面板打开时，↑↓ / Enter / Esc 归面板管 —— 不能让 ↑ 去移动光标、
    // Enter 去把「/记」当消息发出去。
    if (cmdPanel.el) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        moveActive(cmdPanel, 1);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        moveActive(cmdPanel, -1);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        closeCommandPanel(cmdPanel);
        return;
      }
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        // 选中即**执行**（不是发消息）—— 命令是动作，runSelectedCommand 内部已 dispatch
        runSelectedCommand(input);
        return;
      }
    }
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
    // 同步命令面板：内容以 `/` 开头就开，改掉就关
    syncCommandPanel(input);
  });

  // 失焦就关面板 —— 否则输入框里还带着 `/记`，面板却浮在别的窗口上
  input?.addEventListener("blur", () => {
    // 延迟一帧：点面板里的某一项时，blur 会先于 mousedown 触发，
    // 直接关会把用户正在点的那次点击吃掉
    setTimeout(() => {
      // ⚠️ 守卫：120ms 后才判焦点，可能已经越过环境销毁（document 不存在）。
      if (!domAlive()) return;
      if (cmdPanel.el && document.activeElement?.id !== "cmd-panel") {
        closeCommandPanel(cmdPanel);
      }
    }, 120);
  });

  // 发送按钮**只负责发送**。忙碌时它变成「插话」——ENTER 永远把用户打的字送出去，
  // 语义唯一，不用猜。停止是旁边那颗独立的 ■。
  document.getElementById("btn-send")!.addEventListener("click", () => void send());
  document.getElementById("btn-stop")!.addEventListener("click", () => void stopRun());

  // 注：执行位置 / Agent 模式 / 执行权限三组控制**已移出输入行**（见 renderBehaviorView）。
  // 它们曾以 chip 形式占掉输入行 110~165px，把输入框压到放不下一行提示。
  //
  // 输入行左侧那颗按钮现在开的是**命令面板**（2026-10-23 改）：
  // 点开就是命令清单，**不需要记任何快捷键**。打 `/` 开的是同一个面板。
  document.getElementById("btn-commands")!.addEventListener("click", () => {
    const input = document.getElementById("input") as HTMLTextAreaElement;
    // 空查询 = 展示全部命令（按注册表顺序，高频在前）
    openCommandPanel(cmdPanel, "", (act) => {
      void runCommand(act);
    });
    input?.focus();
  });

  renderPreview();
  renderBody();
  // 面板可能是"跑着任务时收起 → 又展开"的，按钮布局要跟着当前状态走
  setBusyUi(busy);
  updateBgRunBanner();
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

/**
 * 前台上下文的能力级别（对应 Rust 侧 `platform::ContextCapability`）。
 *
 * macOS 上「知道你在哪个 app」和「知道你在编辑哪个文件」是两个权限级别，
 * 所以这里要区分 `appOnly` 与 `full`，据此决定要不要引导用户授权。
 * Windows 恒为 `full`（不需要任何系统权限），这段逻辑在 Windows 上不会触发。
 */
interface ContextCapability {
  level: "unavailable" | "appOnly" | "full";
  detail: string;
  canRequest: boolean;
  needsSystemSettings: boolean;
}

/** 能力级别缓存。Windows 上第一次问完就固定了；macOS 上会随用户授权而变。 */
let fgCapability: ContextCapability | null = null;
/** 用户已明确点过「不再提示」—— 不管是授权成功还是主动拒绝 */
let fgCapabilityDismissed = false;
/** 正在等用户在系统设置里勾选，轮询期间不重复弹引导 */
let fgAwaitingSystemSettings = false;

/**
 * 拉一次能力级别（带缓存）。
 *
 * 只有 `appOnly` 且用户还能授权时才会真的问 Rust；拿到 `full` 之后缓存起来，
 * 不再产生任何 IPC —— 这是这条路径在 Windows 上零开销的原因。
 */
async function fetchCapability(force = false): Promise<ContextCapability | null> {
  if (!force && fgCapability && fgCapability.level !== "appOnly") return fgCapability;
  try {
    const c = await withTimeout(
      invoke<ContextCapability>("context_capability"),
      1500,
      null as ContextCapability | null,
    );
    if (c) fgCapability = c;
    return c;
  } catch {
    return fgCapability;
  }
}

/**
 * 请求辅助功能授权。
 *
 * ⚠️ macOS 的辅助功能授权是**两步**：先弹框点「允许」，**再去
 * 「系统设置 → 隐私与安全性 → 辅助功能」手动勾选 orbcat**。
 * 所以点完「允许」只是开始 —— 这里转成「等待中」状态并开始轮询，
 * 用户勾完回来引导条会自动变成完整版。
 */
async function requestFgPermission(): Promise<void> {
  try {
    await invoke("request_context_permission");
  } catch {
    // 非 macOS 或用户拒绝：走降级，不打扰
    fgCapabilityDismissed = true;
    return;
  }
  fgAwaitingSystemSettings = true;
  // 轮询：用户去系统设置勾选可能要几十秒，10 秒一次足够，别太频繁
  const poll = setInterval(() => {
    void (async () => {
      const c = await fetchCapability(true);
      if (!c) return;
      if (c.level === "full") {
        clearInterval(poll);
        fgAwaitingSystemSettings = false;
        void refreshFgCtx(false);
      }
    })();
  }, 10_000);
  // 5 分钟后放弃等待，恢复普通展示
  setTimeout(() => clearInterval(poll), 5 * 60_000);
}

/**
 * macOS 渐进式授权的引导条。
 *
 * ## 为什么是「渐进」而不是启动就弹
 *
 * 启动就弹窗的话，用户第一次打开 orbcat 就看到一个
 * 「orbcat 想要控制这台电脑」—— 他不知道为什么要给，多半直接拒绝，
 * 然后这个功能就永远没了。
 *
 * 渐进的做法是：**只在用户真的需要它的那一刻**才提示 —— 也就是
 * 「他说『帮我改一下这个』，而我知道他在 VSCode 但不知道具体文件」的时候。
 * 那时他已经体验到 orbcat 的价值，自然愿意给。
 *
 * @returns 引导条的 HTML，null 表示不需要展示
 */
async function fgPermissionPromptHtml(): Promise<string | null> {
  if (fgCapabilityDismissed) return null;

  const cap = await fetchCapability();
  // Windows / 已授权 / 拿不到能力信息 —— 都不提示
  if (!cap || cap.level !== "appOnly" || !cap.canRequest) return null;

  // 已经点过授权、正等用户去系统设置勾选 —— 换成「等待中」文案
  if (fgAwaitingSystemSettings) {
    return `<div class="fg-row fg-perm fg-perm-wait">
      <span class="fg-eye">👁</span>
      <span class="fg-label">请到「系统设置 → 隐私与安全性 → 辅助功能」勾选 orbcat，勾完这里会自动更新</span>
      <button class="fg-perm-btn" id="fg-perm-open">打开设置</button>
    </div>`;
  }

  return `<div class="fg-row fg-perm">
    <span class="fg-eye">👁</span>
    <span class="fg-label">${esc(cap.detail)}</span>
    <button class="fg-perm-btn" id="fg-perm-ask">授权</button>
    <button class="fg-perm-x" id="fg-perm-no" title="不用了">✕</button>
  </div>`;
}

/** 指示条最多显示几条（Rust 侧保留 6 条） */
const FG_SHOW_MAX = 6;
/** 列表是否展开：折叠时只显示第一条 + 「+N」角标，点角标翻开/收起 */
let fgExpanded = false;
let fgTimer: ReturnType<typeof setTimeout> | undefined;

async function refreshFgCtx(loop = true): Promise<void> {
  const el = document.getElementById("fg-ctx");
  if (!el) return;
  // 非对话视图时这一行已经让位给 `#sub-bar`（见 `renderSubBar`）。
  // **必须在这里拦住**：下面每个分支都会 `el.style.display = "block"`，
  // 不拦的话它每 3 秒就自己冒一次头，把固定标题条顶下去。
  // 顺带省掉一次无用的 IPC（前台历史只在对话视图里被看见）。
  if (view !== "chat") {
    if (loop) scheduleFgCtx();
    return;
  }
  try {
    const list = await withTimeout(
      invoke<FgCtx[]>("foreground_history"),
      2000,
      [] as FgCtx[],
    );
    const items = (list || [])
      .filter((c) => c && c.app && !c.app.toLowerCase().includes("orbcat"))
      .slice(0, FG_SHOW_MAX);

    if (items.length === 0) {
      // 空态不隐藏整体 —— 显示提示，避免用户以为功能没了
      el.innerHTML = `<div class="fg-row fg-empty">👁 还没记录到其他应用（切到 VSCode/Typora 再回来看）</div>`;
      el.style.display = "block";
    } else {      const rowHtml = (c: FgCtx, i: number) => {
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

    // macOS 渐进式授权引导（Windows 上永远返回 null，是死代码）
    const permHtml = await fgPermissionPromptHtml();
    if (permHtml) {
      el.insertAdjacentHTML("beforeend", permHtml);
      document.getElementById("fg-perm-ask")?.addEventListener("click", (e) => {
        e.stopPropagation();
        void requestFgPermission().then(() => refreshFgCtx(false));
      });
      document.getElementById("fg-perm-open")?.addEventListener("click", (e) => {
        e.stopPropagation();
        void invoke("open_accessibility_settings").catch(() => {});
      });
      document.getElementById("fg-perm-no")?.addEventListener("click", (e) => {
        e.stopPropagation();
        fgCapabilityDismissed = true;
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
    // ⚠️ 定时器活过页面卸载是常态（面板一关它照样在队列里）。
    //   测试环境 teardown 之后 `document` 整个不存在，直接 `getElementById`
    //   会抛 ReferenceError，被 vitest 记成 unhandled error → **整个文件退出码 1**，
    //   哪怕断言全绿（曾撞过：新增两个用例多花 1.5s，
    //   就把这个 3s 定时器拖过了 teardown）。
    //   `typeof` 守卫：无 DOM 时安静退出，不留噪声。
    if (typeof document === "undefined") return;
    if (mode !== "panel" || !document.getElementById("fg-ctx")) return;
    void refreshFgCtx(true);
  }, 3000);
}

/**
 * 每个会话记住的对话滚动位置。
 *
 * ## 用户原话（2026-10-14）
 *
 * 「我看最上面那一条下一次也要是那一条，不要每次开球都看最下面」。
 *
 * 收起面板再展开、切去设置再回来、切会话再切回来 —— `renderBody` 的
 * chat 分支每次都无条件 `scrollToBottom()`，于是**每次开球都落在最后一条**。
 * 长会话里这意味着每次都要重新往上滚半天才能接上上次读的进度。
 *
 * ## 为什么用 scroll 事件而不是"离开时存一次"
 *
 * 离开面板的路径太多（收起 / 切 view / 切会话 / 失焦收起 / 托盘…），
 * 逐个挂钩迟早漏一条，漏了就是"这条路径不记忆"。
 * 监听滚动则**永远**是当前值，且顺带把会话切换前的位置也存对了。
 *
 * ## 键是会话 id
 *
 * 每条会话各自记。切会话时读的是**目标**会话记的位置，不是上一条的。
 *
 * 不落盘：只是"这次开着 app 期间回到原位"，跨重启无意义
 * （消息列表长度都可能变了）。
 */
const chatScrollMemo = new Map<string, number>();

/**
 * 用户翻上去之后「新到的消息」条数 —— 显示在「↓ 回到底部」按钮上。
 *
 * ## 计数口径
 *
 * 只有**用户已经在往上翻**时新到的非用户消息才计数。
 * 贴着底部时来的消息不计数（本来就看得见，数了是噪音）。
 * 一路滚到底 / 点 ↓ / 发出消息 → 清零。
 *
 * 不数流式过程中的每个 token：那是同一条消息在长高，
 * 一边长一边跳数字会让人以为来了一堆新消息。
 */
let chatMissed = 0;

/**
 * 「这一轮已经给「↓」记过数了没」—— 防止流式每个 token 都 +1。
 * 标记跟着**当前 run**，一轮结束（`markRunEnd`）或发新消息时清掉。
 */
let chatMissedCountedForRun = false;

/**
 * 「↓ 回到底部」按钮的显隐与计数。
 *
 * 贴在底部时**隐藏**（没有要回去的地方），否则常驻一个按钮
 * 只会让人以为是别的功能。滚动到底 / 点它 / 发消息时清零。
 */
function syncJumpButton(): void {
  const btn = document.getElementById("jump-btn");
  if (!btn) return;
  if (view !== "chat") {
    btn.hidden = true;
    return;
  }
  const atBottom = isChatAtBottom();
  btn.hidden = atBottom;
  if (atBottom) chatMissed = 0;
  btn.textContent = chatMissed > 0 ? `↓ ${chatMissed > 99 ? "99+" : chatMissed}` : "↓";
}

/** 恢复滚动位置：记过就回原处，没记过（或原处已越界）才贴底。 */
function restoreChatScroll(): void {
  const b = document.getElementById("panel-body");
  if (!b) return;
  const memo = chatScrollMemo.get(currentSessionId);
  if (memo === undefined) {
    scrollToBottom();
    return;
  }
  // ⚠️ 上限夹取：会话可能被截断/分叉变短，原来那个 scrollTop 会超出，
  //    直接赋值会停在空白处（看着像"消息丢了"）。
  const max = Math.max(0, b.scrollHeight - b.clientHeight);
  b.scrollTop = Math.min(memo, max);
}

/**
 * 重绘对话区并**保持**当前滚动位置（用户正在往上翻历史时用）。
 *
 * ## 为什么必须显式写回
 *
 * 真浏览器里 `innerHTML = ...` 会把 `scrollTop` 清成 0，并**随后**抛一次
 * `scroll` 事件。两个后果：
 * 1. 用户的视线被拽回顶部；
 * 2. `chatScrollMemo` 被这条事件写成 0 —— 下次开面板直接回到顶部，
 *    滚动记忆等于白做。
 *
 * 同步写回就够：scroll 事件是**异步**抛的，等它触发时读到的已经是写回后的值。
 */
function repaintChatKeepScroll(): void {
  const b = document.getElementById("panel-body");
  if (!b) return;
  const keep = b.scrollTop;
  paintMessages(b);
  b.scrollTop = keep;
}

/** 重绘对话区并**贴底**（发消息、跑完一轮、来了新候选）。 */
function repaintChatToBottom(): void {
  const b = document.getElementById("panel-body");
  if (!b) return;
  paintMessages(b);
  b.scrollTop = b.scrollHeight;
}

/**
 * 此刻是不是贴在底部。
 *
 * ⚠️ **必须在重绘之前问**：真浏览器里 `innerHTML=` 刚把 `scrollTop` 清 0，
 *    这时再问永远是 false（`main.ts` 权限卡那处就是这么错的 —— 闸门形同虚设）。
 */
function isChatAtBottom(): boolean {
  const b = document.getElementById("panel-body");
  if (!b) return true;
  return b.scrollHeight - b.scrollTop - b.clientHeight < 60;
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

  // 顶部标题条跟着 view 走（对话 → 前台应用条；其余 → 当前页标题 + 返回）
  renderSubBar();
  // 底部状态栏（设计稿 04/05/06/07/09）—— 没有状态栏的页面整条隐藏
  renderStatusBar();

  // ⚠️ **子页只有一行标题条**（设计稿 04/05/06/09/10 都是）：
  // 左组 = 返回 + 标题，右组 = 本页动作。原来的主面板标题条（空间/模式/模型/设置/收起）
  // 在子页里**收起来** —— 两行标题条把 600px 高的面板吃掉一大截，
  // 而且「设置」页顶上那行（空间·模式·模型）跟下面那行「‹ 设置」自相矛盾。
  const hdr = document.getElementById("panel-header");
  if (hdr) hdr.style.display = view === "chat" ? "flex" : "none";

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
      invoke<MemStats>("mem_stats").catch(() => null),
    ]).then(([list, files, sk, projects, st]) => {
      if (view !== "mem") return;
      b.innerHTML = renderMemView(list, files, sk, projects ?? [], st);
      b.scrollTop = keepMem;
      bindMemButtons(b);
      // 标题条徽标与底部状态栏都靠这份数（渲染顺序：body 先，标题条后）
      renderSubBar();
      renderStatusBar();
    })
    .catch((e) => {
      if (view !== "mem") return;
      b.innerHTML = `<div class="mem-head">记忆</div>
        <div class="mem-empty">记忆页渲染失败：${esc(String(e))}</div>`;
    });
    return;
  }

  if (view === "sessions") {
    // ⚠️ 必须保 scrollTop（2026-10-14 用户报）：删掉一条会话后列表要重渲染，
    //    `innerHTML = ...` 会把 scrollTop 归零 → 视线被拽回列表顶部。
    //    长列表里删一条中间的就更烦 —— 删完还得重新滚回原来的位置。
    //    （mem 页 / 设置页早就这么做了，只有会话页漏了。）
    const keep = b.scrollTop;
    const paint = () => {
      b.innerHTML = renderSessionsView();
      bindSessionsView(b);
      // ⚠️ 两轮重渲染都要还回去。删掉末条之后内容变矮，浏览器会自动夹到
      //    可滚动范围内，不需要（也不该）自己再夹一次。
      b.scrollTop = keep;
    };
    b.innerHTML = `<div class="mem-head">会话历史</div>`;
    void refreshSessionList().then(() => {
      if (view !== "sessions") return;
      paint();
      void refreshSessionProjectTags().then(() => {
        if (view === "sessions") paint();
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
      
      bd(invoke<{ blurCollapse?: boolean; projectDirAutoread?: boolean }>("get_settings").catch(() => null), null),
      bd(invoke<UsageRecord[]>("usage_report").catch(() => [] as UsageRecord[]), [] as UsageRecord[]),
      bd(invoke<SearchStatus | null>("search_status").catch(() => null), null),
      bd(invoke<RecoveryState>("recovery_state").catch(() => null), null),
      bd(invoke<ProjectListView>("project_list").catch(() => null), null),
      // 数据源：摘要行要显示条数，所以这里拉一次（内部对未启用的源不读取）
      bd(invoke<LifeStatus>("life_status").catch(() => null), null),
      // 技能：同样只为摘要里的个数（skills_list 是热扫描目录，失败退化成空）
      bd(invoke<SkillInfo[]>("skills_list").catch(() => [] as SkillInfo[]), [] as SkillInfo[]),
      // 命令策略：设置页「行为与安全」组要显示「N 条规则 · M 条硬阻断」
      bd(invoke<{ allow?: string[]; hardBlock?: string[] }>("perm_cmd_policy").catch(() => null), null),
    ])
      .then(([mcp, pending, autoOn, settings, usage, search, recovery, proj, life, skills, cmdPolicy]) => {
        if (view !== "settings") return;
        autostartOn = autoOn;
        blurCollapse = settings?.blurCollapse ?? true;
        projectDirAutoread = settings?.projectDirAutoread ?? true;
        searchInfo = search;
        projectInfo = proj;
        lifeInfo = life;
        skillsCache = skills;
        cmdPolicyCounts = {
          allow: cmdPolicy?.allow?.length ?? 0,
          hard: cmdPolicy?.hardBlock?.length ?? 0,
        };
        const tSum = todayTokenTotal(usage);
        const summary = usage.length ? `今日 ${fmtTokens(tSum)} · 共 ${usage.length} 次问答` : "暂无记录";
        b.innerHTML = renderSettingsMenu(mcp, pending.length, summary, recoverySummaryText(recovery));        b.scrollTop = keep;
        bindSettingsMenu(b);
        applySettingsSearch(b);
        refreshSettingsStat(b);
        renderStatusBar();
      })
      .catch((e) => {
        // 渲染链抛异常也必须显示出来 —— 之前没 catch，异常会让页面永远停在"加载中"
        if (view !== "settings") return;
        b.innerHTML = `<div class="mem-head">设置</div>
          <div class="mem-empty">设置页渲染失败：${esc(String(e))}<br>请把这句话反馈给开发者</div>`;
      });
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

  if (view === "life") {
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<LifeStatus>("life_status")
      .then((st) => {
        if (view !== "life") return;
        lifeInfo = st;
        b.innerHTML = renderLifeView();
        bindLifeView(b);
      })
      .catch((e) => {
        if (view !== "life") return;
        b.innerHTML = `${subHeader("数据源")}
          <div class="mem-empty">读取数据源失败：${esc(String(e))}</div>`;
        // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。
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

  if (view === "behavior") {
    const keep = b.scrollTop;
    void loadModes().then(() => loadBgDesk()).then(() => {
      if (view !== "behavior") return;
      b.innerHTML = renderBehaviorView();
      b.scrollTop = keep;
      bindBehaviorView(b);
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
      invoke<Record<string, boolean>>("mcp_group_settings").catch(() => ({})),
      invoke<BuiltinToolInfo[]>("builtin_tools").catch(() => [] as BuiltinToolInfo[]),
      invoke<SkillInfo[]>("skills_list").catch(() => [] as SkillInfo[]),
      invoke<{ builtinToolsEnabled?: boolean }>("get_settings").catch(() => null),
    ])
      .then(([st, servers, grants, grpSettings, builtins, skills, stg]) => {
        if (view !== "mcp") return;
        // 全局"直接全给"开关（保留键 __all__），组开关本身由后端决定 g.active
        const gs: Record<string, boolean> = grpSettings ?? {};
        mcpAllGroups = gs["__all__"] ?? true;
        builtinToolsEnabled = stg?.builtinToolsEnabled ?? true;
        // 数据源交给 `mcpServers`（DOM 只负责画）—— 见它的注释
        mcpServers = servers ?? [];
        mcpToolsCount = st?.groups.reduce((a, g) => a + g.toolCount, 0) ?? 0;
        b.innerHTML = renderMcpView(st, mcpServers, grants ?? [], builtins ?? [], skills ?? []);
        b.scrollTop = keep;
        bindMcpView(b);
        refreshToolsStat(b);
        renderSubBar();
        renderStatusBar();
      })
      .catch(() => {
        if (view !== "mcp") return;
        mcpServers = [];
        b.innerHTML = renderMcpView(null, [], [], [], []);
        b.scrollTop = keep;
        bindMcpView(b);
        refreshToolsStat(b);
        renderStatusBar();
      });
    return;
  }

  if (view === "project") {
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<ProjectListView>("project_list")
      .then((v) => {
        if (view !== "project") return;
        projectInfo = v;
        b.innerHTML = renderProjectView(v);
        b.scrollTop = keep;
        bindProjectView(b);
      })
      .catch((e) => {
        if (view !== "project") return;
        b.innerHTML = renderProjectView(null) + `<div class="mem-empty">${esc(String(e))}</div>`;
      });
    return;
  }

  if (view === "recovery") {
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<RecoveryState>("recovery_state")
      .then((st) => {
        if (view !== "recovery") return;
        b.innerHTML = renderRecoveryView(st, subHeader("备份与回收站"));
        b.scrollTop = keep;
        bindRecoveryView(
          b,
          () => renderBody(),
          // `hint` = 第三级提示行（设计稿 11:2:1594）
          (t, body, hint) => askConfirm(t, body, "永久删除", true, hint ?? ""),
        );
      })
      .catch((e) => {
        if (view !== "recovery") return;
        b.innerHTML = `<div class="mem-head">备份与回收站</div>
          <div class="mem-empty">读取失败：${esc(String(e))}</div>`;
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

  alignProjectToSession(); // 顶栏项目必须与当前会话一致（别让「标题写 A、聊着 B」）
  paintMessages(b);
  renderPreview(); // 回到对话时恢复缩略图区
  // ⚠️ 不再无条件贴底：记过位置就回原处（2026-10-14 用户报「每次开球都看最下面」）
  restoreChatScroll();
  syncJumpButton(); // 回到原处就意味着"不在底部" → ↓ 按钮要冒出来
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
/**
 * 把当前滚动位置记进 [`chatScrollMemo`]。**每次滚动都调**，见那里的说明。
 *
 * 只在 `view === "chat"` 时记 —— 设置/记忆页的滚动不该污染对话的位置。
 */
function memoizeChatScroll(): void {
  if (view !== "chat") return;
  const b = document.getElementById("panel-body");
  if (!b || !currentSessionId) return;
  chatScrollMemo.set(currentSessionId, b.scrollTop);
}

/** 在面板容器上挂滚动记账。`renderPanel` 每次重建 DOM，所以要防重复挂。 */
function installScrollMemo(): void {
  const b = document.getElementById("panel-body");
  if (!b || b.dataset.scrollMemo === "1") return;
  b.dataset.scrollMemo = "1";
  b.addEventListener("scroll", memoizeChatScroll, { passive: true });
  // 显隐也要跟着滚动走：滚下去藏起来、滚上来冒出来
  b.addEventListener("scroll", syncJumpButton, { passive: true });
}

function bindInitPrompt(root: HTMLElement): void {
  const btn = root.querySelector<HTMLButtonElement>("#init-go");
  if (!btn) return;

  if (!needsInit) {
    // 形态二：没模型 → 直接弹「模型管理」独立窗口（加模型就在那边）
    btn.addEventListener("click", () => {
      openModelsWindow();
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
        `[orbcat] 初始化完成：建目录 ${r.createdDirs.length} 个 / 建文件 ${r.createdFiles.length} 个 / 跳过 ${r.skipped.length} 个 → ${r.dataDir}`,
      );

      // 重新自检 —— needsInit 变 false 后空态自动切到「去加模型」那一屏，
      // 这就是给用户的最直接反馈
      await checkBoot();
      void loadModels();
      view = "chat";
      renderBody();

      // 顺手弹出模型管理窗口，省得他去找入口
      openModelsWindow();
    } catch (e) {
      console.error("[orbcat] 初始化失败:", e);
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


/**
 * 候选来源的**显示名**（设计稿 06:2:1013「全局 · 自动蒸馏」）。
 *
 * 后端 `source` 只有三个值：model（模型主动提交）/ auto（轮末自动蒸馏）/
 * user（用户手写）。这里翻成人话，并统一加「全局 ·」前缀 ——
 * 目前项目记忆是直写 `projects/<名>/MEMORY.md`、**不进**候选队列，
 * 所以队列里的候选一律是全局的。
 */
function memSourceLabel(source: string): string {
  const s = (source ?? "").trim().toLowerCase();
  const detail =
    s === "auto" ? "自动蒸馏" : s === "user" ? "手写" : s === "model" ? "模型提交" : s || "模型提交";
  return `全局 · ${detail}`;
}

/**
 * 候选时间的**相对显示**（设计稿 06:2:1001「2 小时前」/「昨天」）。
 *
 * 后端 `created_at` 形如 `ts1795326872T13:54:32Z` —— 前 10 位是 epoch 秒。
 * 这里只取那 10 位算差值，别去解析整串（那串里的时分秒是**本地时区**，
 * 与 epoch 秒混在一块，解析它只会引出时区 bug）。
 */
function memRelTime(createdAt: string): string {
  // ⚠️ epoch 现在是 **10 位**（`ts1792731001`）。后端格式是 `ts{secs:08}` ——
  //    `:08` 只是「至少 8 位」，位数不够会自然长到 10 位。所以这里必须
  //    匹配**全部**数字（`\d+`），写死 8 位会把 1792731001 截成 17927310，
  //    差值算出来是 50 多年前 → 满屏「更早」。
  const m = /^ts(\d+)/.exec((createdAt ?? "").trim());
  if (!m) return (createdAt ?? "").trim();
  const secs = Number(m[1]);
  if (!Number.isFinite(secs) || secs <= 0) return (createdAt ?? "").trim();
  const diff = Math.floor(Date.now() / 1000) - secs;
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  if (diff < 86400 * 2) return "昨天";
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)} 天前`;
  return "更早";
}

function renderMemView(
  list: Candidate[],
  files: MemoryFiles | null,
  skills: SkillInfo[],
  projects: ProjectMemoryView[],
  stats: MemStats | null,
): string {
  // 回填缓存：设置入口页的「记忆」摘要要显示技能数，别让它白拉一遍
  skillsCache = skills;
  // 底部状态栏与标题条徽标都要用这个数（渲染顺序：先 body 后 sub-bar）
  memPendingCount = list.length;
  memStatsCache = stats;

  // 候选卡（设计稿 06:2:981）
  // 结构照抄设计稿：候选头（来源标记 + 时间）· 正文 · 触发轮次 · 操作行（驳回/改写/批准）
  const pendingHtml =
    list.length === 0
      ? `<div class="mem-empty mem-empty-done">全部处理完了。模型认为值得长期记住的信息会通过 <code>remember</code> 提交到这里。</div>`
      : list
          .map(
            (c) => `
      <div class="mem-candidate" data-id="${esc(c.id)}">
        <div class="cand-head">
          <span class="cand-source">${esc(memSourceLabel(c.source))}</span>
          <span class="cand-time">${esc(memRelTime(c.created_at))}</span>
        </div>
        <div class="cand-body">${esc(c.content)}</div>
        ${
          c.turn_ref
            ? `<div class="cand-turn">出自「${esc(c.turn_ref)}」这一轮</div>`
            : ""
        }
        <div class="cand-actions">
          <button type="button" class="mem-cand-btn reject" data-id="${esc(c.id)}" title="驳回（留档到 rejected.jsonl）">驳回</button>
          <button type="button" class="mem-cand-btn edit" data-id="${esc(c.id)}" title="改写措辞，改完再点批准">改写</button>
          <button type="button" class="mem-cand-btn approve" data-id="${esc(c.id)}" title="写入长期记忆">批准</button>
        </div>
        <div class="cand-edit" hidden>
          <textarea class="cand-edit-box" spellcheck="false"></textarea>
          <div class="cand-edit-ops">
            <button type="button" class="mem-cand-btn cancel-edit">取消</button>
            <button type="button" class="mem-cand-btn approve save-edit">保存改写</button>
          </div>
        </div>
        <input class="mem-reason" data-for="${esc(c.id)}" placeholder="驳回理由（可留空，写进 rejected.jsonl）" spellcheck="false" hidden />
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
            // 三态徽标（修正）：空 source ≠ 路径失效 ——
            // 从没绑过路径的项目是「未绑定」，不该跟死路径一样标红吓人
            const badge = !p.source
              ? `<span style="color:var(--sem-warn)">未绑定路径</span>`
              : !p.sourceAlive
                ? `<span style="color:var(--sem-danger)">源路径不存在</span>`
                : p.hasMemory
                  ? `<span style="color:var(--sem-ok)">已绑定</span>`
                  : `<span style="color:var(--sem-warn)">MEMORY 为空</span>`;
            const canFix = !p.source || !p.sourceAlive;
            return `
      <div class="skill-item">
        <div class="skill-name">📁 ${esc(p.name)} ${badge}</div>
        <div class="skill-desc"><code>${esc(p.source || "（无 source.ref）")}</code></div>
        <div class="proj-fix">
          <button type="button" class="mem-btn proj-edit-mem" data-proj="${esc(p.name)}" title="直接改这个项目的 MEMORY.md（agent 平时自己维护，你偶尔手改）">编辑记忆</button>
          ${
            canFix
              ? `<button type="button" class="mem-btn proj-set-src" data-proj="${esc(p.name)}" title="把 source.ref 指到新路径（留空 = 解绑）">改路径</button>
                 <button type="button" class="mem-btn no proj-archive" data-proj="${esc(p.name)}" title="整个目录挪进 agent-data/.trash/（不硬删，可手工还原）">归档</button>`
              : ""
          }
        </div>
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
    ${pendingHtml}
    <div class="mem-head"> 记忆文件族<button type="button" class="mem-btn mem-distill" title="打开蒸馏页：把 MEMORY.md 中转站的条目分流到 USER / SOUL / IDENTITY，拿不准的 AI 会向你提问">🧹 蒸馏</button></div>
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
  // ---- 待审批候选：批准 / 驳回 / 改写（设计稿 06:2:1004~2:1006）----
  //
  // ⚠️ 选择器带 `.mem-cand-btn` + 动作类名：项目记忆行上的「改路径/归档」
  //    用的是 `.mem-btn` 配色，别把它们绑成批准/驳回。
  root.querySelectorAll<HTMLButtonElement>(".mem-cand-btn.approve[data-id]").forEach((btn) =>
    btn.addEventListener("click", async () => {
      await invoke("mem_approve", { id: btn.dataset.id }).catch((e) =>
        pushEntry("error", `批准失败：${e}`),
      );
      renderBody();
    }),
  );

  // 驳回：第一次点**只展开理由输入框**（设计稿的卡里没有这个框，
  // 但 rejected.jsonl 的「留理由」功能不能丢 —— 见 bindMemButtons 里
  // `mem_reject` 的注释）。第二次点才真的驳回。
  root.querySelectorAll<HTMLButtonElement>(".mem-cand-btn.reject[data-id]").forEach((btn) =>
    btn.addEventListener("click", async () => {
      const id = btn.dataset.id ?? "";
      const box = root.querySelector<HTMLInputElement>(`.mem-reason[data-for="${CSS.escape(id)}"]`);
      if (!box) return;
      if (box.hidden) {
        box.hidden = false;
        btn.textContent = "确认驳回";
        box.focus();
        return;
      }
      // 驳回理由跟着一起留档（2026-05-15）：rejected.jsonl 是留档，
      // 只留内容不留理由，过几天只能问人「这条当初为什么被打回」。
      const reason = box.value.trim() ?? "";
      await invoke("mem_reject", { id, reason }).catch((e) =>
        pushEntry("error", `驳回失败：${e}`),
      );
      renderBody();
    }),
  );

  // 改写：把候选正文换成可编辑的文本框；「保存改写」只改内容（**不批准**），
  // 改完再点「批准」才写进长期记忆 —— 改与批是两件事，不合并。
  root.querySelectorAll<HTMLButtonElement>(".mem-cand-btn.edit[data-id]").forEach((btn) =>
    btn.addEventListener("click", () => {
      const card = btn.closest(".mem-candidate");
      const editor = card?.querySelector<HTMLElement>(".cand-edit");
      const body = card?.querySelector<HTMLElement>(".cand-body");
      const ta = card?.querySelector<HTMLTextAreaElement>(".cand-edit-box");
      if (!editor || !body || !ta) return;
      ta.value = body.textContent ?? "";
      body.hidden = true;
      editor.hidden = false;
      ta.focus();
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".mem-cand-btn.cancel-edit").forEach((btn) =>
    btn.addEventListener("click", () => {
      const card = btn.closest(".mem-candidate");
      const editor = card?.querySelector<HTMLElement>(".cand-edit");
      const body = card?.querySelector<HTMLElement>(".cand-body");
      if (!editor || !body) return;
      editor.hidden = true;
      body.hidden = false;
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".mem-cand-btn.save-edit").forEach((btn) =>
    btn.addEventListener("click", async () => {
      const card = btn.closest(".mem-candidate");
      const ta = card?.querySelector<HTMLTextAreaElement>(".cand-edit-box");
      const id = card?.getAttribute("data-id") ?? "";
      const text = (ta?.value ?? "").trim();
      if (!id) return;
      if (!text) {
        pushEntry("error", "改写内容不能为空");
        return;
      }
      try {
        await invoke("mem_rewrite", { id, content: text });
        showToast("已改写，点「批准」写进长期记忆", "ok", 2400);
      } catch (e) {
        pushEntry("error", `改写失败：${e}`);
        return;
      }
      renderBody();
    }),
  );

  // ---- 记忆蒸馏：开一条专用会话，剩下的就是常规聊天 ----
  root.querySelector<HTMLButtonElement>(".mem-distill")?.addEventListener("click", () => {
    void distillStart();
  });

  // ---- 项目记忆维护：编辑正文 / 改路径 / 归档 ----
  root.querySelectorAll<HTMLButtonElement>(".proj-edit-mem").forEach((btn) =>
    btn.addEventListener("click", () => void startProjMemoryEdit(btn.dataset.proj ?? "")),
  );

  root.querySelectorAll<HTMLButtonElement>(".proj-set-src").forEach((btn) =>
    btn.addEventListener("click", () => startProjEdit(btn.dataset.proj ?? "")),
  );

  root.querySelectorAll<HTMLButtonElement>(".proj-archive").forEach((btn) =>
    btn.addEventListener("click", async () => {
      const name = btn.dataset.proj ?? "";
      const ok = await askConfirm(
        `归档项目记忆「${name}」？`,
        "整个目录会挪进 agent-data/.trash/（不硬删，可手工还原）。\n归档后该项目的记忆不再注入。",
        "归档",
      );
      if (!ok) return;
      try {
        const dest = await invoke<string>("mem_project_archive", { name });
        showToast(`已归档到 ${dest}`, "ok", 4500);
      } catch (e) {
        showToast(`归档失败：${e}`, "error");
      }
      renderBody();
    }),
  );
}

/**
 * 项目记忆「改路径」：把该条目的路径行就地换成输入框 + 保存/取消。
 *
 * 为什么不弹独立窗口：面板里没有单行输入弹窗组件（askInput 已随模型管理
 * 搬进 models 窗口），就地替换最省事，Enter 保存 / Esc 取消也好按。
 */
/**
 * 「蒸馏」入口（定稿）：开一条**普通聊天**会话，让 agent 自己蒸馏。
 *
 * 定稿口径是"思考就在常规聊天里发生" —— 所以这里**不做任何自定义页面**：
 * 新会话带 `distill` 标记（`sessions/<id>.distill`），后端 `agent::run` 见到它
 * 就换蒸馏角色说明 + 注入 `distill_move` 工具，模型在标准循环里读中转站、
 * 问你、搬条目 —— 思考过程与工具调用全走主对话那套时间线 UI。
 * 本函数只负责：开会话 → 切到对话 → 填一句开场把它跑起来。
 */
async function distillStart(): Promise<void> {
  try {
    const s = await invoke<Session>("distill_session_open");
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    view = "chat";
    renderBody();

    const kickoff =
      "开始蒸馏：先读 agent-data/memory/MEMORY.md（中转站），把每条分流到 " +
      "USER.md（只写「人」）/ SOUL.md（语气人格）/ IDENTITY.md（称呼）/ " +
      "projects/<名>/MEMORY.md（只在本项目内成立的事实，用 distill_move 的 project 参数指定项目名，留空 = 顶栏激活的项目）。" +
      "逐条引用原文把建议摆出来给我看；拿不准的条目直接问我，我答复后你再用 distill_move 落盘。";
    const input = document.getElementById("input") as HTMLTextAreaElement | null;
    if (input) {
      input.value = kickoff;
      draftInput = kickoff;
    }
    if (busy) {
      showToast("蒸馏会话已开好（当前有任务在跑）——等它结束后按回车发出开场", "ok", 4000);
      return;
    }
    void send();
  } catch (e) {
    showToast(`开蒸馏会话失败：${e}`, "error");
  }
}

/**
 * 就地编辑项目 MEMORY.md（用户手改通道）。
 *
 * ## 分工（2026-10-15 用户拍板）
 *
 * agent 是项目记忆的**维护者** —— 追加走 `remember(scope:"project")`，
 * 改删走常规 `read_file`/`edit_file`。这个编辑器是给用户的**特殊场合**：
 * 想亲手改一条、或者 agent 把整份写乱了要收拾。
 *
 * ## 为什么就地展开而不是弹窗
 *
 * 同「改路径」的交互（`startProjEdit`）：记忆页是长列表，弹窗会把上下文
 * 弹走，改完还要重新找回到哪一条。
 *
 * ## 保存语义
 *
 * 整份覆盖，后端先按天备份成 `MEMORY.md.bak-<日期>`。⚠️ 同一天改多次只回到
 * 当天第一次的状态 —— 提示里写明，别让用户以为有历史版本。
 */
async function startProjMemoryEdit(name: string): Promise<void> {
  const item = [...document.querySelectorAll(".skill-item")].find(
    (el) => el.querySelector(".proj-edit-mem")?.getAttribute("data-proj") === name,
  );
  const desc = item?.querySelector<HTMLElement>(".skill-desc");
  if (!desc) return;

  let text = "";
  try {
    text = await invoke<string>("pmem_memory_read", { name });
  } catch (e) {
    showToast(`读取项目记忆失败：${e}`, "error");
    return;
  }

  const stat = item?.querySelector<HTMLElement>(".skill-name")?.textContent ?? "";
  desc.innerHTML = `
    <textarea class="proj-mem-editor" rows="10" spellcheck="false"
      placeholder="项目的路径、术语、目录与命名约定、进展与未决项…"></textarea>
    <div class="proj-mem-actions">
      <button type="button" class="mem-btn ok proj-mem-save">保存</button>
      <button type="button" class="mem-btn no proj-mem-cancel">取消</button>
    </div>
    <div class="proj-mem-hint">${esc(
      text.trim() ? `${stat.trim()}` : "（这个项目还没有记忆）",
    )} · 整份覆盖，保存前自动备份到 <code>MEMORY.md.bak-&lt;日期&gt;</code>（同一天只留一份）</div>`;
  const ta = desc.querySelector<HTMLTextAreaElement>(".proj-mem-editor");
  if (!ta) return;
  ta.value = text;
  ta.focus();

  const save = async (): Promise<void> => {
    const btn = desc.querySelector<HTMLButtonElement>(".proj-mem-save");
    if (btn) btn.disabled = true;
    try {
      const bak = await invoke<string>("pmem_memory_write", { name, text: ta.value });
      showToast(bak ? `已保存「${name}」的项目记忆（备份 ${bak}）` : `已保存「${name}」的项目记忆`, "ok");
      renderBody();
    } catch (e) {
      if (btn) btn.disabled = false;
      showToast(`保存失败：${e}`, "error");
    }
  };
  desc.querySelector<HTMLButtonElement>(".proj-mem-save")?.addEventListener("click", () => {
    void save();
  });
  desc.querySelector<HTMLButtonElement>(".proj-mem-cancel")?.addEventListener("click", () => {
    renderBody();
  });
  ta.addEventListener("keydown", (ev) => {
    // ⚠️ Ctrl/Cmd+S = 保存（编辑器里的肌肉记忆）；**普通 Enter 必须照常换行**，
    //    不能像「改路径」那个单行输入那样 Enter 即存 —— 这里是多行正文。
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "s") {
      ev.preventDefault();
      void save();
    }
    if (ev.key === "Escape") renderBody();
  });
}

function startProjEdit(name: string): void {
  const item = [...document.querySelectorAll(".skill-item")].find(
    (el) => el.querySelector(".proj-set-src")?.getAttribute("data-proj") === name,
  );
  const desc = item?.querySelector<HTMLElement>(".skill-desc");
  if (!desc) return;
  const cur = desc.querySelector("code")?.textContent ?? "";
  desc.innerHTML = `
    <input class="proj-src-input" type="text" placeholder="D:\\path\\to\\project（留空 = 解绑）" spellcheck="false" />
    <button type="button" class="mem-btn ok proj-src-save">保存</button>
    <button type="button" class="mem-btn no proj-src-cancel">取消</button>`;
  const input = desc.querySelector<HTMLInputElement>(".proj-src-input");
  if (!input) return;
  input.value = cur === "（无 source.ref）" ? "" : cur;
  input.focus();
  const save = async (): Promise<void> => {
    try {
      await invoke("mem_project_set_source", { name, path: input.value });
      showToast(`已更新「${name}」的源路径`, "ok");
      renderBody();
    } catch (e) {
      showToast(`保存失败：${e}`, "error");
    }
  };
  desc.querySelector<HTMLButtonElement>(".proj-src-save")?.addEventListener("click", () => {
    void save();
  });
  desc.querySelector<HTMLButtonElement>(".proj-src-cancel")?.addEventListener("click", () => {
    renderBody();
  });
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      void save();
    }
    if (ev.key === "Escape") renderBody();
  });
}

async function switchView(v: View): Promise<void> {
  // 再点同一个非 chat 视图 → 退回对话（toggle 手感）
  const next = v === view && v !== "chat" ? "chat" : v;
  // 进子页从顶部看起：面板体是**同一个**滚动容器，不重置的话从聊天流
  // 中间点进「行为」会直接落在页面中段 —— 看到的是半截卡片，
  // 还以为页面坏了（曾截图复现）。回 chat 不动，那边有自己的保滚动。
  const b = document.getElementById("panel-body");
  if (next !== view && next !== "chat" && b) b.scrollTop = 0;
  view = next;
  renderBody();
}

// ---------------- 设置：入口页 ----------------

/**
 * 设置入口页 —— **只列入口和状态摘要**，内容点进去再看。
 * （早先把模型/MCP/记忆全平铺在一页，滚半天找不到东西）
 */
/**
 * 设置入口的线性图标（24×24 描边，跟随 currentColor）。
 *
 * 原来是 13 个 emoji（🧩🌐📊♻️🔌📡📦📁⌨️🧠🎛）。Windows 会把它们彩色渲染，
 * 一屏五颜六色，和面板冷暖统一的底色打架 —— 单色描边才能融进去。
 * 离线 + 不能加依赖，所以路径手写。
 */
const ICONS: Record<string, string> = {
  // 模型：四宫格 = 一张模型清单
  models:
    '<rect x="3" y="3" width="7.5" height="7.5" rx="1.6"/><rect x="13.5" y="3" width="7.5" height="7.5" rx="1.6"/><rect x="3" y="13.5" width="7.5" height="7.5" rx="1.6"/><rect x="13.5" y="13.5" width="7.5" height="7.5" rx="1.6"/>',
  // 搜索：地球
  search:
    '<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17"/><path d="M12 3.5c2.6 2.3 4 5.3 4 8.5s-1.4 6.2-4 8.5c-2.6-2.3-4-5.3-4-8.5s1.4-6.2 4-8.5z"/>',
  // Token 用量：柱状图
  usage: '<path d="M3 20.5h18"/><path d="M6.5 20.5v-5"/><path d="M12 20.5V5.5"/><path d="M17.5 20.5v-8"/>',
  // 备份与回收站：还原箭头
  recovery: '<path d="M3.5 12a8.5 8.5 0 1 0 2.9-6.4"/><path d="M3 3.5v5h5"/>',
  // MCP 外部工具：插头
  mcp: '<path d="M9 3v5.5"/><path d="M15 3v5.5"/><path d="M6 8.5h12v2.6a6 6 0 0 1-12 0z"/><path d="M12 17.1V21"/>',
  // 数据源：信号
  life: '<path d="M4.5 12a7.5 7.5 0 0 1 15 0"/><path d="M8 12a4 4 0 0 1 8 0"/><circle cx="12" cy="16" r="1.8"/>',
  // 项目：包裹
  project:
    '<path d="M3 7.5l9-4 9 4v9l-9 4-9-4z"/><path d="M3 7.5l9 4 9-4"/><path d="M12 11.5v9"/>',
  // 文件权限：文件夹
  perm: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2.2h8a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  // 命令策略：终端
  cmdpolicy: '<rect x="2.5" y="4.5" width="19" height="15" rx="2.2"/><path d="M6.5 9.5l2.4 2.5-2.4 2.5"/><path d="M12.5 14.5h5"/>',
  // 记忆：灯泡
  mem: '<path d="M12 3a6 6 0 0 0-3.4 10.9V17h6.8v-3.1A6 6 0 0 0 12 3z"/><path d="M10 20.2h4"/>',
  // 会话管理：带尾巴的气泡 + 两行横线（会话列表的通用隐喻）
  sessions:
    '<path d="M4 5h16v11H9.5L4 20.5z"/><path d="M8 9.5h8"/><path d="M8 13h5"/>',
  // 行为：滑杆
  behavior:
    '<path d="M3.5 8.5h9"/><path d="M17.5 8.5h3"/><circle cx="15" cy="8.5" r="2.2"/><path d="M3.5 15.5h3"/><path d="M11.5 15.5h9"/><circle cx="9" cy="15.5" r="2.2"/>',
  // 命令面板：四宫格 = 一堆可用命令（网格 = 功能一览，最贴切）
  commands:
    '<rect x="3.5" y="3.5" width="7" height="7" rx="1.6"/><rect x="13.5" y="3.5" width="7" height="7" rx="1.6"/><rect x="3.5" y="13.5" width="7" height="7" rx="1.6"/><rect x="13.5" y="13.5" width="7" height="7" rx="1.6"/>',
};

function setIcon(name: string): string {
  // ⚠️ 2026-10-23 用户实报：设置页组头的前置图标（四宫格等）**根本看不见**。
  // 根因：SVG 里的 `<path>`/`<rect>` 默认 `fill: black; stroke: none` ——
  // 黑色填充画在深色底上 = 什么都没有（截图里那个位置只剩一块空）。
  // 修法：图标统一用**描边**画（`fill:none; stroke:currentColor`），
  // 颜色跟着外层容器的 `color` 走（组头是 `--g2`）。
  return `<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">${
    ICONS[name] ?? ""
  }</svg>`;
}

/**
 * 设置页 **4 个组头**的图标（设计稿 04:2:829 / 2:846 / 2:863 / 2:879）。
 *
 * 全部 14×14、描边 #8A8A90（`.set-sec-ico` 上的 `color` 决定）。
 * 形状照设计稿节点的构成画：
 *
 * | 组 | 设计稿节点 | 构成 |
 * |---|---|---|
 * | 模型与接入 | 2:829 | 圆角矩形 11.2×9.2 (2:894) + 内部短横 6.4×2.4 (2:895) |
 * | 记忆与数据 | 2:846 | 对话气泡：上半 7.6×10.4 (2:898) + 下半带尾 10.4×5.4 (2:899) |
 * | 行为与安全 | 2:863 | 盾牌 10×10.8 (2:903) + 勾 4.4×2.8 (2:904) |
 * | 系统 | 2:879 | 圆 9.2×9.2 (2:908) + 一根齿 1.7×3.6 (2:909) |
 *
 * ⚠️ **别拿 `ICONS` 那套来顶**（用户实报「看他的配置，你照着抄也不明白吗」）：
 * `ICONS` 是设置**入口页**的图标（四宫格 / 灯泡 / 滑块 / 齿轮），
 * 跟设计稿组头画的**不是一回事** —— 入口页那些是「这一类设置」的隐喻，
 * 组头这些是设计稿给每一组专门画的。
 */
const SET_GROUP_ICONS: Record<string, string> = {
  // 模型与接入：圆角矩形 + 一条横线
  models:
    '<rect x="1.4" y="2.4" width="11.2" height="9.2" rx="1.5"/><path d="M3.8 6.6h6.4"/>',
  // 记忆与数据：对话气泡（圆角矩形 + 左下小尾巴）
  mem: '<path d="M10.8 9.4H8.6l-3.2 2.6V9.4H3.2A1.6 1.6 0 0 1 1.6 7.8V3.9a1.6 1.6 0 0 1 1.6-1.6h7.6a1.6 1.6 0 0 1 1.6 1.6v3.9a1.6 1.6 0 0 1-1.6 1.6z"/>',
  // 行为与安全：盾牌 + 勾
  behavior:
    '<path d="M7 1.7l4.9 2v4.1c0 2.9-2 5.3-4.9 6.1-2.9-.8-4.9-3.2-4.9-6.1V3.7z"/><path d="M4.9 6.7l1.6 1.6 3-2.5"/>',
  // 系统：齿轮（圆 + 一根齿）
  system: '<circle cx="7" cy="7" r="4.5"/><path d="M7 4.6v4.8"/>',
};

/** 设置页组头图标（描边，跟随 `.set-sec-ico` 的 color）。 */
function setGroupIcon(name: string): string {
  return `<svg viewBox="0 0 14 14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round">${
    SET_GROUP_ICONS[name] ?? ""
  }</svg>`;
}

/**
 * 设置页 = **4 组手风琴**（设计稿 04「04 · 设置页（分组手风琴）」）。
 *
 * ## 为什么不是「大卡片列表」
 *
 * 旧版是一屏 10 张大卡片，每张点进去才是子页 —— 想找「失焦自动收起」得点两层，
 * 而且看不到**当前值**。设计稿 04 改成 **4 组 + 行内值 + 行内开关**：
 * 一屏看全所有设置项和当前值，能改的直接在行里改（开关），不能改的显示摘要。
 *
 * ## 照抄设计稿的值（Ardot 节点 id 记在注释里）
 *   组块      #141418 + 描边 rgba(255,255,255,.07)   （04:2:795）
 *   计数徽标  #1F1F23 + 字 #71717A                  （04:2:831）
 *   文字      L1 #F2F2F4 / L2 #A8A8B4 / L3 #71717A   （04:2:830/2:834）
 *   开关·开   底 #E4EBE7 + 滑块 #101012              （04:2:872/2:906）
 *   开关·关   底 #26262B + 滑块 #6E6E74              （04:2:885/2:911）
 *   待批徽标  #F0C46B + 字 #101012                  （04:2:848/2:900）
 */
function renderSettingsMenu(
  mcp: McpStatus | null,
  pendingCount: number,
  usageSummary: string,
  recoverySummary: string,
): string {
  const curModelName = curModel()?.id ?? "（未选）";

  const mcpSummary = mcp
    ? mcp.connected
      ? `${mcp.groups.length} 组 / ${mcp.groups.reduce((a, g) => a + g.toolCount, 0)} 个工具`
      : "未连接"
    : "读取中…";

  const searchReady = !!searchInfo?.ready;
  const searchSummary = searchReady
    ? `${searchInfo!.providerLabel}${searchInfo!.hasKey ? ` · ${searchInfo!.keyPreview}` : ""}`
    : "未配置";

  /**
   * 一行：名称 …… 值。
   * `act` 有值 = 可点进子页；`swId` 有值 = 行内开关。
   */
  const row = (
    name: string,
    valHtml: string,
    act?: string,
    valClass = "",
    swId?: string,
    swOn?: boolean,
  ): string => {
    const tag = act ? "button" : "div";
    const right = swId
      ? `<button type="button" class="sw${swOn ? " on" : ""}" id="${swId}" role="switch"
                aria-checked="${swOn}" title="${esc(name)}"><i></i></button>`
      : `<span class="set-row-val ${valClass}">${valHtml}</span>`;
    return `
    <${tag} class="set-row${act ? "" : " set-row-static"}"${act ? ` data-act="${act}"` : ""}${
      swId ? ` data-sw="${swId}"` : ""
    }>
      <span class="set-row-name">${esc(name)}</span>
      ${right}
    </${tag}>`;
  };

  /** 一个组：组头（图标 + 标题 + 计数徽标 + 折叠箭头）+ 组体 */
  const sec = (
    key: string,
    icon: string,
    name: string,
    count: number | null,
    countCls: string,
    body: string,
  ): string => `
    <section class="set-sec" data-sec="${key}">
      <button type="button" class="set-sec-head" data-fold="${key}" aria-expanded="true">
        <span class="set-sec-ico">${setGroupIcon(icon)}</span>
        <span class="set-sec-name">${esc(name)}</span>
        ${count === null ? "" : `<span class="set-sec-count ${countCls}">${count}</span>`}
        <span class="set-sec-caret">▾</span>
      </button>
      <div class="set-sec-body">${body}</div>
    </section>`;

  return `
    ${
      settingsSearchOpen
        ? `<div class="set-search">
      <input id="set-search-input" placeholder="搜索设置项…" value="${esc(settingsSearchQ)}" spellcheck="false" autocomplete="off">
    </div>`
        : ""
    }
    <div class="set-acc">
      ${sec(
        "model",
        "models",
        "模型与接入",
        5,
        "",
        `
        ${row(
          "当前模型",
          `${esc(curModelName)} <span class="set-val-caret">▾</span>`,
          "set-model",
        )}
        ${row("模型管理", `共 ${models.length} 个（独立窗口）`, "models")}
        ${row("web_search", esc(searchSummary), "search", searchReady ? "ok" : "")}
        ${row("MCP 工具组", esc(mcpSummary), "mcp")}
        ${row("技能", `${skillsCache.length} 个已启用`, "mem")}
      `,
      )}
      ${sec(
        "data",
        "mem",
        "记忆与数据",
        pendingCount || null,
        "warn",
        `
        ${row(
          "待审批候选",
          pendingCount > 0
            ? `<span class="set-val-badge">${pendingCount} 待批</span>`
            : "无",
          "mem",
        )}
        ${row("记忆文件", `${projects.length} 个项目`, "mem")}
        ${row("备份与回收站", esc(recoverySummary), "recovery")}
        ${row("Token 用量", esc(usageSummary), "usage")}
        <!-- 「数据源」入口：功能暂缓（沿用旧版 off=true 的做法）——
             设计稿 04 里没有这一行 ⇒ 界面上不显示，但**入口留在 DOM 里**，
             功能恢复时把那层 hidden 去掉即可。 -->
        <button class="set-row" data-act="life" hidden>数据源</button>
        <!-- 「项目」入口：讲的是**拼装包**（projects/<名>/project.json），不是项目记忆。
             功能暂缓 ⇒ 沿用旧版做法：行存在但 hidden，界面上不显示。 -->
        <button class="set-row" data-act="project" hidden>项目</button>
      `,
      )}
      ${sec(
        "behavior",
        "behavior",
        "行为与安全",
        null,
        "",
        `
        ${row("执行位置", bgDeskEnabled ? "后台桌面" : "本机 pwsh", "behavior")}
        ${row(
          "内核",
          "",
          undefined,
          "",
          "sw-kernel",
          usePiKernel,
        )}
        ${row("项目目录自动读写", "", undefined, "", "sw-dir", projectDirAutoread)}
        ${row(
          "命令策略",
          `${cmdPolicyCounts.allow + cmdPolicyCounts.hard} 条规则 · ${cmdPolicyCounts.hard} 条硬阻断`,
          "perm",
        )}
        ${row("失焦自动收起", "", undefined, "", "sw-blur", blurCollapse)}
      `,
      )}
      ${sec(
        "system",
        "system",
        "系统",
        null,
        "",
        `
        ${row("开机自动启动", "", undefined, "", "sw-autostart", autostartOn)}
        ${row("会话管理", `${sessionList.length} 条`, "sessions")}
        ${row("程序路径", '<span id="exe-path">读取中…</span>')}
        <div class="set-actions set-actions-row">
          <button class="set-btn del" id="btn-quit">⏻ 退出 orbcat</button>
        </div>
      `,
      )}
    </div>`;
}


/**
 * 设置页（4 组手风琴）的交互绑定。
 *
 * 三件事：
 * ① 组头点击 = 折叠/展开该组（`data-fold`）
 * ② 行点击 = 进子页（`data-act`）；「当前模型」那行是**行内下拉**（设计稿 04:2:806）
 * ③ 行内开关 = 直接改设置（`data-sw`）
 */
function bindSettingsMenu(root: HTMLElement): void {  // ① 组头折叠
  root.querySelectorAll<HTMLButtonElement>(".set-sec-head").forEach((h) => {
    h.addEventListener("click", () => {
      const sec = h.closest(".set-sec");
      if (!sec) return;
      const folded = sec.classList.toggle("folded");
      h.setAttribute("aria-expanded", String(!folded));
      const caret = h.querySelector(".set-sec-caret");
      if (caret) caret.textContent = folded ? "▸" : "▾";
    });
  });

  // ② 行 → 子页 / 行内模型下拉
  root.querySelectorAll<HTMLElement>(".set-row[data-act]").forEach((b) => {
    const act = b.dataset.act ?? "";
    // 「模型管理」= 弹**独立窗口**（编辑住在外面，面板只管选）
    if (act === "models") {
      b.addEventListener("click", () => openModelsWindow());
      return;
    }
    // 「当前模型」= 行内下拉（设计稿 04:2:806 「glm-5.2 ▾」）
    if (act === "set-model") {
      b.addEventListener("click", (e) => {
        e.stopPropagation();
        modelsDropdownOpen = !modelsDropdownOpen;
        paintModelsDropdown("set-model-chip");
      });
      return;
    }
    b.addEventListener("click", () => void switchView(act as View));
  });

  // ③ 行内开关
  const sw = (id: string, apply: (v: boolean) => Promise<void>): void => {
    const btn = document.getElementById(id) as HTMLButtonElement | null;
    if (!btn) return;
    btn.addEventListener("click", async () => {
      const next = !btn.classList.contains("on");
      btn.classList.toggle("on", next);
      btn.setAttribute("aria-checked", String(next));
      try {
        await apply(next);
      } catch (e) {
        btn.classList.toggle("on", !next); // 回滚
        btn.setAttribute("aria-checked", String(!next));
        pushEntry("error", `设置失败：${e}`);
      }
    });
  };

  sw("sw-autostart", async (v) => {
    await invoke("autostart_set", { enable: v });
    autostartOn = v;
  });
  sw("sw-blur", async (v) => {
    await invoke("set_blur_collapse", { enable: v });
    blurCollapse = v;
  });
  sw("sw-dir", async (v) => {
    await invoke("set_project_dir_autoread", { enable: v });
    projectDirAutoread = v;
  });
  // 内核切换（pi ↔ 自研）—— 设计稿 02/04 都没有这颗 chip，
  // 但「用户有权知道自己在用哪个内核」是项目铁律 ⇒ 放设置页行内开关。
  const kb = document.getElementById("sw-kernel") as HTMLButtonElement | null;
  kb?.addEventListener("click", async () => {
    const next = !usePiKernel;
    kb.classList.toggle("on", next);
    kb.setAttribute("aria-checked", String(next));
    try {
      await toggleKernel();
    } catch (e) {
      kb.classList.toggle("on", !next);
      kb.setAttribute("aria-checked", String(!next));
      pushEntry("error", `切换内核失败：${e}`);
    }
  });

  void invoke<string>("exe_path")
    .then((p) => {
      const el = document.getElementById("exe-path");
      if (el) el.textContent = p;
    })
    .catch(() => {});

  // --- 退出 ---
  document.getElementById("btn-quit")?.addEventListener("click", () => {
    void invoke("app_quit").catch(() => {});
  });
}

/**
 * 设置页「搜索设置」（设计稿 04 右组 🔍 + 底栏「/ 搜索设置」）。
 *
 * 做什么：按关键词过滤 4 组手风琴里的**行** —— 命中的留下，其余隐藏；
 * 一个命中都没有的组整组隐藏。命中时**自动展开**那个组（不然搜到了却看不见）。
 *
 * 为什么不搜组名：组名只有 4 个（模型与接入 / 记忆与数据 / 行为与安全 / 系统），
 * 搜组名等于没用；用户记得的是「项目目录自动读写」这种**行名**。
 */
function applySettingsSearch(root: HTMLElement): void {
  const box = root.querySelector<HTMLInputElement>("#set-search-input");
  if (box) {
    box.addEventListener("input", () => {
      settingsSearchQ = box.value.trim();
      applySettingsSearch(root);
      refreshSettingsStat(root);
      renderStatusBar();
    });
    box.addEventListener("keydown", (e) => {
      if ((e as KeyboardEvent).key === "Escape") {
        settingsSearchQ = "";
        box.value = "";
        applySettingsSearch(root);
        refreshSettingsStat(root);
      }
    });
  }

  const q = settingsSearchQ.toLowerCase();
  root.querySelectorAll<HTMLElement>(".set-sec").forEach((sec) => {
    let hit = 0;
    sec.querySelectorAll<HTMLElement>(".set-sec-body .set-row").forEach((row) => {
      const name = (row.querySelector(".set-row-name")?.textContent ?? "").toLowerCase();
      const val = (row.querySelector(".set-row-val")?.textContent ?? "").toLowerCase();
      const match = !q || name.includes(q) || val.includes(q);
      // 命中的行永远显示；未命中的行在有搜索词时隐藏
      row.style.display = !q || match ? "" : "none";
      if (!q || match) hit++;
    });
    sec.style.display = hit > 0 ? "" : "none";
    if (q && hit > 0) {
      sec.classList.remove("folded");
      const head = sec.querySelector(".set-sec-head");
      head?.setAttribute("aria-expanded", "true");
      const caret = sec.querySelector(".set-sec-caret");
      if (caret) caret.textContent = "▾";
    }
  });
}

/** 设置页统计：数**可见**的组与行（搜索时统计跟着变）。 */
function refreshSettingsStat(root: HTMLElement): void {
  const secs = Array.from(root.querySelectorAll<HTMLElement>(".set-sec")).filter(
    (s) => s.style.display !== "none",
  );
  let items = 0;
  secs.forEach((s) => {
    items += Array.from(s.querySelectorAll<HTMLElement>(".set-sec-body .set-row")).filter(
      (r) => r.style.display !== "none" && !r.hasAttribute("hidden"),
    ).length;
  });
  settingsStatCache = { groups: secs.length, items };
}

/** 打开/关闭设置页搜索（标题条 🔍 按钮）。 */
function toggleSettingsSearch(): void {
  settingsSearchOpen = !settingsSearchOpen;
  if (!settingsSearchOpen) settingsSearchQ = "";
  renderBody();
}


/**
 * 子页内容流里的**节标题**（不含返回按钮）。
 *
 * ⚠️ 变更后：返回按钮**从这里拿掉了**，改由滚动区外的
 * `#sub-bar` 承担（见 `renderSubBar`）。原来按钮写在这儿，跟着
 * `#panel-body` 一起滚 —— 滚到页面下半部分就够不着，必须一路
 * 滚回顶部才能退出。现在这里只留标题，标题不动也无所谓（顶部条常驻）。
 */
function subHeader(title: string): string {
  return `<div class="mem-head">${title}</div>`;
}

/**
 * 非对话视图的固定顶部条配置。
 *
 * `back` = 点返回去哪；`backLabel` = 按钮文案。
 * 两档返回目标：子页回设置入口页、设置与会话历史回对话。
 */
const VIEW_BAR: Record<
  Exclude<View, "chat">,
  { title: string; icon: string; back: View; backLabel: string }
> = {
  settings: { title: "设置", icon: "⚙", back: "chat", backLabel: "返回对话" },
  sessions: { title: "会话", icon: "🕘", back: "chat", backLabel: "返回对话" },
  // 设计稿 09:2:1489 —— 标题就叫「工具」（这页管的是**所有**工具组，
  // 不只是 MCP；「MCP 外部工具」只是其中一个组）。
  mcp: { title: "工具", icon: "🔗", back: "settings", backLabel: "设置" },
  perm: { title: "文件权限", icon: "🔒", back: "settings", backLabel: "设置" },
  cmdpolicy: { title: "命令策略", icon: "🛡", back: "settings", backLabel: "设置" },
  behavior: { title: "行为", icon: "🎛", back: "settings", backLabel: "设置" },
  mem: { title: "记忆", icon: "🧠", back: "settings", backLabel: "设置" },
  usage: { title: "Token 用量", icon: "📊", back: "settings", backLabel: "设置" },
  recovery: { title: "备份与回收站", icon: "♻", back: "settings", backLabel: "设置" },
  search: { title: "搜索", icon: "🌐", back: "settings", backLabel: "设置" },
  life: { title: "数据源", icon: "📄", back: "settings", backLabel: "设置" },
  project: { title: "项目", icon: "📦", back: "settings", backLabel: "设置" },
};

/**
 * 画/收顶部标题条 —— 每次 `renderBody()` 都会调（view 一变它就得跟着变）。
 *
 * ## 结构照抄设计稿 04/05/06/09/10
 *
 * 所有子页的标题条都是同一个形状：
 * ```
 * 左组：[返回图标] 页面标题(13px SemiBold)   右组：[这一页自己的动作按钮]
 * ```
 * 原来这里是「标题在左、返回在右」，与设计稿相反 —— 已按 04/05/06/09/10
 * 的标题条改成左组返回 + 标题、右组放本页动作。
 *
 * 非对话视图：亮 `#sub-bar`，收 `#fg-ctx` ——
 * 后者是"用户当前在看什么"，在设置页里既没用又白占一行，正好把这一行让给返回。
 * 对话视图：反过来（前台条的显隐交给 `refreshFgCtx` 自己管，它有
 * "读取中/空态/失败"三种内容，这里不插手）。
 */
function renderSubBar(): void {
  const bar = document.getElementById("sub-bar");
  const fg = document.getElementById("fg-ctx");
  if (!bar || !fg) return;

  if (view === "chat") {
    bar.style.display = "none";
    bar.innerHTML = "";
    return;
  }

  fg.style.display = "none";
  const cfg = VIEW_BAR[view];

  // 右组：这一页自己的动作（设计稿 04 搜索设置 / 05 新建 / 06 全部批准 / 10 清空回收站）
  const extra = subBarActions(view);

  // 徽标：只有记忆页有（设计稿 06:2:978「3 待批」）
  const badge =
    view === "mem" && memPendingCount > 0
      ? `<span class="sb-badge">${memPendingCount} 待批</span>`
      : "";

  bar.innerHTML =
    `<div class="sb-left">` +
    // `id="sub-back"` 是**沿用旧锚点**：子页的返回按钮原来长在内容流里就带这个 id，
    // 现有 UI 测试直接查它来点击（见 tests/ui/flow.behavior.test.ts 等）。
    // 语义没变（还是"回上一层"），只是挪到了滚动区之外。
    `<button class="mem-back sub-bar-back" id="sub-back" type="button" title="${esc(
      cfg.backLabel,
    )}">‹</button>` +
    `<span class="sub-bar-title">${esc(cfg.title)}</span>` +
    badge +
    `</div>` +
    `<div class="sb-right">${extra}</div>`;

  bindSubBarActions(bar);
  bar.style.display = "flex";
  bar.querySelector<HTMLButtonElement>(".sub-bar-back")?.addEventListener("click", () => {
    void switchView(cfg.back);
  });
}

/**
 * 子页标题条**右组**的按钮（设计稿 04/05/06/10）。
 *
 * 「动作」都放右组，「我在哪」放左组 —— 这样标题条自己就是一张地图：
 * 左边告诉你站在哪儿，右边是这一页能做的事。
 */
function subBarActions(v: View): string {
  switch (v) {
    // 设计稿 04:2:794 —— 搜索设置（28×28 **线性**放大镜，灰白描边）
    // ⚠️ 原来这里用 emoji 🔍 —— Windows 把它渲染成**彩色渐变**小球，
    //    在冷灰面板上非常扎眼，而且跟设计稿的线性图标完全两回事。
    case "settings":
      return `<button class="sb-icon-btn" id="sb-set-search" type="button" title="搜索设置项">${setIcon(
        "search",
      )}</button>`;
    // 设计稿 05:2:922 —— 新建会话（28×28 实心图标按钮）
    case "sessions":
      return '<button class="sb-icon-btn" id="sb-new-session" type="button" title="新建会话">＋</button>';
    // 设计稿 06:2:980 —— 全部批准
    // ⚠️ **有候选才显示**：候选还没加载出来时（memPendingCount = 0），
    //    摆一颗点了没反应的按钮比不摆更糟 —— 用户会以为功能坏了。
    case "mem":
      return memPendingCount > 0
        ? '<button class="sb-btn" id="mem-approve-all" type="button" title="把当前所有候选一次性写进长期记忆">全部批准</button>'
        : "";
    // 设计稿 09:2:1491 —— 「48 个工具 · 6 个组」（右边是**状态**，不是动作）
    case "mcp":
      return `<span class="sb-stat">${esc(
        `${toolsStatCache.tools} 个工具 · ${toolsStatCache.groups} 个组`,
      )}</span>`;
    // 设计稿 10:2:1336 —— 清空回收站
    // 备份区**不给**批量清空 —— 备份是安全网，清空它风险不对称（设计稿 §7）。
    case "recovery":
      return '<button class="rec-clear-trash" id="rec-clear-trash" type="button">清空回收站</button>';
    default:
      return "";
  }
}

/** 给右组按钮绑事件（每次重画都要重绑）。 */
function bindSubBarActions(bar: HTMLElement): void {
  // ---- 10 回收站：清空回收站（不可还原，要二次确认）----
  const clearBtn = bar.querySelector<HTMLButtonElement>("#rec-clear-trash");
  clearBtn?.addEventListener("click", async () => {
    const ok = await askConfirm(
      "清空回收站？",
      "回收站里的归档会被永久删除，不可还原。备份区不受影响。",
      "永久清空",
    );
    if (!ok) return;
    try {
      const msg = await invoke<string>("recovery_clear_trash");
      showToast(msg, "ok", 2600);
      renderBody(); // 重画本页（switchView 对同一视图有 toggle 语义，会退回对话）
    } catch (e) {
      showToast(`清空回收站失败：${e}`, "error");
    }
  });

  // ---- 06 记忆：全部批准 ----
  const allBtn = bar.querySelector<HTMLButtonElement>("#mem-approve-all");
  allBtn?.addEventListener("click", async () => {
    if (memPendingCount <= 0) return;
    const ok = await askConfirm(
      `全部批准 ${memPendingCount} 条候选？`,
      "这些候选会一次性写进长期记忆，以后每轮对话都会带上它们。",
      "全部批准",
      false,
      "想一条条看，可以直接在下面逐条批准或驳回。",
    );
    if (!ok) return;
    try {
      const n = await invoke<number>("mem_approve_all");
      showToast(`已批准 ${n} 条，写进长期记忆`, "ok", 2600);
      renderBody();
    } catch (e) {
      showToast(`全部批准失败：${e}`, "error");
    }
  });

  // ---- 05 会话：新建会话 ----
  bar.querySelector<HTMLButtonElement>("#sb-new-session")?.addEventListener("click", () => {
    void newSession();
  });

  // ---- 04 设置：搜索设置项 ----
  bar.querySelector<HTMLButtonElement>("#sb-set-search")?.addEventListener("click", () => {
    toggleSettingsSearch();
  });
}

/**
 * 底部状态栏（设计稿 04/05/06/07/09 的「底部状态栏」）。
 *
 * 04 设置：「设置」+「4 组 · 13 项」+「/ 搜索设置」
 * 05 会话：「会话」+「17 条」
 * 06 记忆：「待审批候选」+「N / M」（N 待批，M 候选总数）
 * 09 工具：「工具」+「6 组 · 48 个工具」
 *
 * 固定在内容区**之外**：它是仪表盘，滚走就看不见了。
 */
function renderStatusBar(): void {
  const el = document.getElementById("panel-status");
  if (!el) return;
  if (view === "chat") {
    el.style.display = "none";
    el.innerHTML = "";
    return;
  }

  const left = "status-bar-left";
  const right = "status-bar-right";
  let html = "";
  switch (view) {
    case "settings":
      html = `<span class="${left}">设置</span><span class="${right}">${esc(
        `${settingsStatCache.groups} 组 · ${settingsStatCache.items} 项`,
      )}</span><span class="${right} mono">/ 搜索设置</span>`;
      break;
    case "sessions":
      html = `<span class="${left}">会话</span><span class="${right} mono">${esc(
        `${sessionList.length} 条`,
      )}</span>`;
      break;
    case "mem": {
      const pending = memStatsCache?.pending ?? memPendingCount;
      const total = pending + (memStatsCache?.handled ?? 0);
      html = `<span class="${left}">待审批候选</span><span class="${right} mono">${esc(
        `${pending} / ${total}`,
      )}</span>`;
      break;
    }
    case "mcp":
      html = `<span class="${left}">工具</span><span class="${right} mono">${esc(
        `${toolsStatCache.groups} 组 · ${toolsStatCache.tools} 个工具`,
      )}</span>`;
      break;
    default:
      html = "";
  }

  if (!html) {
    el.style.display = "none";
    el.innerHTML = "";
    return;
  }
  el.innerHTML = html;
  el.style.display = "flex";
}

// ---------------- 设置 › 行为（执行位置 / 执行权限） ----------------
//
// 为什么这些住在这里、而不是输入行：
//   它们是**设置**（改一次管很久），不是每次发消息都要碰的东西。
//   定稿口径："对话页不该有设置项" ——
//   它们曾以三颗 chip 的形式挤在输入行，占掉约 110~165px，
//   把输入框压到连一行 placeholder 都放不下。
//
// ⚠️ **Agent 模式已从这里搬走**（定论："b 从设置页挪到一个
//    '新建会话时选模式'的入口 默认是标准"）。理由见 `showNewMenu` 的注释 ——
//    一句话：模式跟会话走、**建完不能改**，所以它不是"设置"而是"建会话的参数"。
//    这里只剩执行位置与执行权限两维。别再往这儿加模式选择器。

function renderBehaviorView(): string {
  const trust = EXEC_TRUST_LABEL[execTrust] ?? EXEC_TRUST_LABEL.ask;

  // ── 执行位置 ──
  const bgRows = `
    <label class="set-check set-check-row">
      <input type="checkbox" id="bh-bgdesk" ${bgDeskEnabled ? "checked" : ""}>
      后台执行（命令跑在隐形桌面上）
    </label>
    <div class="set-hint">
      开着：命令与 GUI 程序跑在一张<b>用户看不见的桌面</b>上 —— 不闪窗口、不抢焦点。<br>
      ⚠️ 隐形桌面上<b>没有合成键鼠</b>，所以只能驱动有自动化接口（COM / 命令行）的程序；
      要「点界面」的老软件请关掉它。<br>
      这是<b>进程级</b>开关，不落盘，重启回到前台。
      ${bgDeskError ? `<br><span style="color:var(--sem-danger)">⚠️ ${esc(bgDeskError)}</span>` : ""}
    </div>
    ${
      bgDeskEnabled && bgDeskWindows.length
        ? `<div class="set-hint">隐形桌面上正在跑（${bgDeskWindows.length}）：<br>${bgDeskWindows
            .slice(0, 6)
            .map((w) => `· ${esc(w.title)}（${w.width}×${w.height}）`)
            .join("<br>")}</div>`
        : ""
    }`;

  // ── 执行权限 ──
  const trustRows = (
    [
      ["ask", "每条命令 / MCP 调用都弹卡让你拍板"],
      ["smart", "低/中风险自动执行，高风险才问"],
      ["full", "全部自动执行；危险命令仍被硬阻断"],
    ] as const
  )
    .map(
      ([id, desc]) => `
      <label class="set-check set-check-row bh-row">
        <input type="radio" name="bh-trust" value="${id}" ${id === execTrust ? "checked" : ""}>
        <b>${EXEC_TRUST_LABEL[id].icon} ${EXEC_TRUST_LABEL[id].full}</b>
        <div class="ctx-sub">${desc}</div>
      </label>`,
    )
    .join("");

  return (
    subHeader("行为") +
    `<div class="set-hint" style="margin-bottom:8px">
       两个维度互不干涉：<b>执行位置</b>管窗口开在哪张桌面、<b>执行权限</b>管调用工具时问不问。
     </div>` +
    `<div class="mem-head">🖥 执行位置</div>${bgRows}` +
    `<div class="mem-head">${trust.icon} 执行权限（当前：${esc(trust.full)}）</div>${trustRows}` +
    `<div class="set-hint">文件权限申请不受这一档影响（那是 permissions.json 三层闸门）。</div>` +
    `<div class="set-hint">
       <b>Agent 模式不在这里</b>：它跟着<b>会话</b>走、建会话时就定死（一个会话模式不能变），
       所以入口在工具栏的 <b>「＋」→ 新建会话</b> 里。想看当前会话是什么模式，
       打开工具栏<b>「史」</b>看列表里每条会话后面的标记。<br>
       模式管什么：<b>哪些 MCP 工具组对模型可见</b>（只能<b>收窄</b> —— 你在设置 › MCP
       里关掉的组，任何模式都开不回来）、<b>工具怎么调</b>（native 逐个直接调 /
       PTC 只给 <code>run_code</code>、其余写成 TypeScript 程序调，中间结果不进上下文）、
       以及<b>正文能不能发表情包</b>（工作模式只准贴产物图；这条靠提示词约束，
       不前端硬拦，拦了会连截图一起挡掉）。<br>
       要加自己的模式：改 <code>agent-data/modes.json</code>，不用重新编译。
     </div>`
  );
}

function bindBehaviorView(root: HTMLElement): void {
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。

  root.querySelector<HTMLInputElement>("#bh-bgdesk")?.addEventListener("change", async (e) => {
    const on = (e.currentTarget as HTMLInputElement).checked;
    await setBgDesk(on);
    renderBody(); // 重绘：要刷新摘要行与"正在跑什么"列表
  });

  // ⚠️ 原来这里还有一坨 'input[name="bh-mode"]' 的 change 监听
  //    （勾模式 → `applyModeChange`）。模式搬进「＋」菜单后**整块删掉**：
  //    留着它俩的下场是"改了个不存在的 radio、或者把当前会话的模式偷改了"——
  //    而定论是**一个会话模式不能变**。要改默认模式只能手编 settings.json。
  root.querySelectorAll<HTMLInputElement>('input[name="bh-trust"]').forEach((el) =>
    el.addEventListener("change", async () => {
      if (!el.checked) return;
      try {
        const saved = await invoke<string>("set_exec_trust", { mode: el.value });
        execTrust = (saved as typeof execTrust) || (el.value as typeof execTrust);
        showToast(`执行权限：${EXEC_TRUST_LABEL[execTrust].full}`, "ok");
        renderBody();
      } catch (err) {
        showToast(`切换执行权限失败：${err}`, "error");
        renderBody();
      }
    }),
  );
}

// ---------------- 设置 › 数据源 ----------------
//
// 数据源 = 给 agent 的**认知面**注入（与 MCP 的能力面正交）。
//
// 为什么单独一页而不是塞进 MCP：MCP 是"模型决定去调"，数据源是
// "系统决定呈现"。问「帮我安排下今天」时模型**没有理由**先去调一个工具，
// 而「更懂你」的前提恰恰是不用说就知道 —— 这是触发权不同，不是工具多少。
//
// 注入策略（定稿）：system prompt 里只常驻每个源**一行**
// （条数 + 新鲜度），细节要模型调 `life_items`。

interface LifeSourceView {
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

interface LifeStatus {
  path: string;
  sources: LifeSourceView[];
}

let lifeInfo: LifeStatus | null = null;

function renderLifeView(): string {
  const st = lifeInfo;
  if (!st) {
    return subHeader("数据源") + `<div class="mem-empty">读取中…</div>`;
  }

  const rows = st.sources.length
    ? st.sources
        .map((s) => {
          // 状态：关闭 / 配置不完整 / 读取失败 / 正常（含过期）
          let status: string;
          if (!s.enabled) {
            status = `<span class="mg-off">已关闭</span>`;
          } else if (!s.complete) {
            status = `<span class="mg-unknown">配置不完整</span>`;
          } else if (!s.ok) {
            status = `<span class="life-bad">⚠️ ${esc(s.error)}</span>`;
          } else {
            const age = s.ageHours === null ? "" : ` · ${s.ageHours} 小时前`;
            const stale = s.stale ? ` <span class="life-bad">可能已过期</span>` : "";
            status = `<span class="mg-ok">${s.count} 条${age}</span>${stale}`;
          }

          const preview = s.enabled && s.ok && s.preview.length
            ? `<div class="life-preview">${s.preview
                .map((p) => `· ${esc(p)}`)
                .join("<br>")}${s.count > s.preview.length ? `<br>…还有 ${s.count - s.preview.length} 条` : ""}</div>`
            : "";

          return `
        <div class="set-row life-row">
          <div class="set-name">
            <label class="set-check mcp-group-on">
              <input type="checkbox" class="life-cb" data-id="${esc(s.id)}" ${
                s.enabled ? "checked" : ""
              }> 启用
            </label>
            <b>${esc(s.label)}</b>
            <span class="life-kind">${s.kind === "file" ? "📄 文件快照" : "⚙️ 命令"}</span>
          </div>
          <div class="set-url"><code>${esc(s.target)}</code></div>
          <div class="set-url">${status}</div>
          ${preview}
        </div>`;
        })
        .join("")
    : `<div class="mem-empty">
         还没有数据源。下面「怎么加一个」照抄改一行就行 —— 不用写代码。
       </div>`;

  return (
    subHeader("数据源") +
    `<div class="set-hint" style="margin-bottom:8px">
      数据源让 agent <b>知道你的处境</b>（学习通作业、教务、邮件…），
      而不只是会调工具。它和 MCP 是两件事：<br>
      · <b>MCP</b> 是能力面 —— <b>模型</b>决定去调；<br>
      · <b>数据源</b> 是认知面 —— <b>系统</b>决定要不要呈现给你看。<br>
      注入方式：对话里每个源只占<b>一行摘要</b>（条数 + 新鲜度），
      细节要模型调 <code>life_items</code> 才拿 —— 省 token，且快照过期不会被当成事实。
     </div>` +
    `<div class="mem-head">已配置（${st.sources.length}）</div>` +
    `<div class="set-list">${rows}</div>` +
    `<div class="set-hint" style="margin-top:8px">
      配置文件：<code>${esc(st.path)}</code>（改完点下面「刷新」即可生效，不用重启）
     </div>` +
    `<div class="set-actions" style="margin-top:6px">
       <button class="set-btn test" id="life-refresh">刷新</button>
     </div>` +
    `<div class="mem-head">怎么加一个</div>` +
    `<div class="set-hint">
      加一个源 = 在 <code>life.json</code> 的 <code>sources</code> 里加一段 JSON：<br><br>
      <code>{ "id": "xuexitong", "label": "学习通", "enabled": true,</code><br>
      <code>&nbsp;&nbsp;"kind": "file",</code><br>
      <code>&nbsp;&nbsp;"path": "D:/…/campus-monitor/state/xuexitong.json",</code><br>
      <code>&nbsp;&nbsp;"items": "$.works[*]",</code><br>
      <code>&nbsp;&nbsp;"map": { "title": "$.work", "group": "$.course", "dueRaw": "$.left" },</code><br>
      <code>&nbsp;&nbsp;"staleHours": 48 }</code><br><br>
      <b>kind: file</b> —— 读一个现成的 JSON 快照（你现在的爬虫就属于这种，零代码接入）。<br>
      <b>kind: command</b> —— 跑一条命令、stdout 当 JSON 收（邮件、任意脚本）。<br>
      ⚠️ <code>command</code> 类会在<b>后台自动执行</b>（不是模型调才跑），
      所以默认关闭，要显式打开；有 15 秒超时，失败只会在上面显示一行错误，
      不会打断对话。
     </div>`
  );
}

function bindLifeView(root: HTMLElement): void {
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。

  document.getElementById("life-refresh")?.addEventListener("click", async () => {
    await invoke("life_refresh").catch(() => {});
    await loadLifeStatus();
    renderBody();
    showToast("已重新读取", "ok", 1500);
  });

  root.querySelectorAll<HTMLInputElement>(".life-cb").forEach((cb) =>
    cb.addEventListener("change", async () => {
      const id = cb.dataset.id!;
      const on = cb.checked;
      try {
        await invoke("life_set_enabled", { id, enabled: on });
        showToast(`${on ? "已启用" : "已关闭"}「${id}」`, "ok", 2000);
      } catch (e) {
        pushEntry("error", `${on ? "启用" : "关闭"}「${id}」失败：${e}`);
      }
      await loadLifeStatus();
      renderBody();
    }),
  );
}

/** 拉一次数据源状态（设置入口页与子页都要用） */
async function loadLifeStatus(): Promise<void> {
  try {
    lifeInfo = await invoke<LifeStatus>("life_status");
  } catch (e) {
    console.warn("[orbcat] 读数据源失败:", e);
    lifeInfo = null;
  }
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
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。
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
    ${subHeader("安全闸门")}
    <div class="set-hint" style="margin-bottom:8px">
      这是 agent 能碰什么的<b>全部</b>开关。另一面是<b>命令策略</b>（白名单 / 硬阻断 / 审计）——
      <a href="#" class="goto-cmdpolicy">去命令策略 ›</a>
    </div>

    <div class="mem-head">文件权限 ${rules.length} 条</div>
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
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。

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

  // 「安全闸门」是两个页面合并后的入口：文件权限页顶部要能跳到命令策略，
  // 否则设置入口页只留一行之后，命令策略就没有可达路径了。
  document
    .querySelector(".goto-cmdpolicy")
    ?.addEventListener("click", (e) => {
      e.preventDefault();
      void switchView("cmdpolicy");
    });

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
      <b>弹卡频率</b>由输入框左侧的<b>执行权限档位</b>控制（🛡 每次询问 / ⚡ 智能放行 / 🔓 允许完全访问）——
      硬阻断在任何档位下都拦，档位免掉的只是"问你"。<br>
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
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。

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

/**
 * 把一行参数文本切成 argv。
 *
 * 规则：按空白切分，但**引号内的空白不切**（`"C:\Program Files\x.exe" --flag`）。
 * 引号本身不进结果 —— 它只是分组手段，真正的参数边界由 argv 表达。
 *
 * ⚠️ 这里**只做切分，不做 shell 解析**：没有变量展开、没有通配符、
 * 没有管道。切出来的每一项会被当作**字面参数**原样传给子进程
 * （Rust 侧 `Command::args`，不经 shell）—— 所以用户输入永远进不了解释器。
 */
function splitArgs(raw: string): string[] {
  const out: string[] = [];
  let cur = "";
  let quote: '"' | "'" | null = null;
  let has = false;
  for (const ch of raw) {
    if (quote) {
      if (ch === quote) quote = null;
      else cur += ch;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      has = true; // 空引号 `""` 也要算一个参数
      continue;
    }
    if (/\s/.test(ch)) {
      if (has || cur) {
        out.push(cur);
        cur = "";
        has = false;
      }
      continue;
    }
    cur += ch;
  }
  if (has || cur) out.push(cur);
  return out;
}

/** 内置工具的一条摘要（后端 `builtin_tools`） */
interface BuiltinToolInfo {
  name: string;
  desc: string;
}

/**
 * 工具页（设计稿 09）的**三个工具组**的折叠状态。
 *
 * 设计稿里「内置工具」默认展开，另两组默认折叠 —— 内置工具是最常看的，
 * 另两组是「配了才看」的。
 */
const TOOLS_GROUP_OPEN: Record<string, boolean> = {
  builtin: true,
  mcp: false,
  skills: false,
};

function renderMcpView(
  mcp: McpStatus | null,
  servers: McpServerCfg[],
  grants: string[],
  builtins: BuiltinToolInfo[],
  skills: SkillInfo[],
): string {
  // ⚠️ 这里**不写** `subHeader("工具")` —— 设计稿 09 的内容区直接就是组块，
  //    页名已经在标题条上（sub-bar），再写一遍就是重复。
  const head = "";

  // ---- 多 server 列表（增删启用） ----
  const serverRows =
    servers.length === 0
      ? `<div class="mem-empty">还没有配置任何 MCP server。下面填一行再点「添加」。</div>`
      : servers
          .map((s, i) => {
            const isStdio = mcpTransportOf(s) === "stdio";
            const kindTag = isStdio
              ? `<span class="mcp-kind stdio">stdio</span>`
              : `<span class="mcp-kind http">http</span>`;
            // 两种形态显示不同的"地址行"
            const target = isStdio
              ? `<code>${esc(s.command ?? "")}</code>${
                  (s.args ?? []).length
                    ? ` <span class="mcp-args">${esc((s.args ?? []).join(" "))}</span>`
                    : ""
                }${
                  Object.keys(s.env ?? {}).length
                    ? ` <span class="mcp-env-n">+${Object.keys(s.env ?? {}).length} 环境变量</span>`
                    : ""
                }`
              : `<code>${esc(s.url)}</code>`;
            return `
      <div class="set-row mcp-srv-row" data-idx="${i}">
        <div class="set-name">
          <label class="mcp-en"><input type="checkbox" class="mcp-en-cb" data-idx="${i}" ${
            s.enabled ? "checked" : ""
          }> 启用</label>
          ${kindTag}
          <code class="mcp-srv-id">${esc(s.id)}</code>
          <span class="set-cur">${esc(s.label || s.id)}</span>
        </div>
        <div class="set-url">${target}</div>
        <div class="set-actions">
          <button class="set-btn mcp-srv-del" data-idx="${i}">删除</button>
        </div>
      </div>`;
          })
          .join("");

  const serverBlock = `
    <div class="mem-head">🔗 MCP Server 列表</div>
    <div class="set-hint" style="margin-bottom:8px">
      两种形态：<b>http</b> 连一个已在跑的网关（如 1MCP）；
      <b>stdio</b> 由 orbcat 自己拉起子进程 —— 一个脚本 + 几行配置就是工具，
      不必额外起网关。两者在权限卡与执行档位上<b>完全同权</b>。
    </div>
    <div class="set-list" id="mcp-srv-list">${serverRows}</div>

    <div class="mem-head">➕ 添加 HTTP 网关</div>
    <div class="set-form mcp-add-form">
      <input id="mcp-new-id" placeholder="id（如 github）" spellcheck="false">
      <input id="mcp-new-label" placeholder="显示名（可空）" spellcheck="false">
      <input id="mcp-new-url" placeholder="http://127.0.0.1:3050/mcp" spellcheck="false">
      <button class="set-btn add" id="mcp-srv-add">添加</button>
    </div>

    <div class="mem-head">➕ 添加 stdio 脚本</div>
    <div class="set-form mcp-add-form">
      <input id="mcp-new-sid" placeholder="id（如 my-script）" spellcheck="false">
      <input id="mcp-new-slabel" placeholder="显示名（可空）" spellcheck="false">
      <input id="mcp-new-cmd" placeholder="命令（如 node / pwsh / D:\\tools\\x.exe）" spellcheck="false">
      <input id="mcp-new-args" placeholder="参数，空格分隔（如 server.js --port 1）" spellcheck="false">
      <button class="set-btn add" id="mcp-stdio-add">添加</button>
    </div>
    <div class="set-hint" style="margin-top:-4px">
      参数按<b>空格切分</b>后逐参传递（不经 shell）。路径含空格时用引号：
      <code>"C:\\Program Files\\x.exe" --flag</code>
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
    const onCount = mcp.groups.filter((g) => g.active).length;
    const bar = `
      <div class="mcp-budget">
        <span>已启用 ${onCount}/${mcp.groups.length} 组 · 占用 ~${mcp.activeTokens} tokens</span>
      </div>`;
    const rows = mcp.groups
      .map(
        (g) => `
        <div class="set-row${g.active ? " cur" : ""}">
          <div class="set-name">${esc(g.name)}${
          g.hasDestructive ? ' <span class="set-warn">含危险操作</span>' : ""
        }</div>
          <div class="set-url">${esc(g.summary)}</div>
          <div class="set-url">${g.toolCount} 个工具 · 约 ${g.tokens} tokens</div>
          <div class="set-actions">
            <label class="set-check mcp-group-on">
              <input type="checkbox" class="mcp-grp-cb" data-group="${esc(g.name)}" ${
                g.active ? "checked" : ""
              }> 给模型
            </label>
          </div>
        </div>`,
      )
      .join("");
    statusBlock =
      `<div class="set-url" style="margin-bottom:8px">已连接 ${esc(mcp.url)}</div>` +
      `<label class="set-check set-check-row">
         <input type="checkbox" id="mcp-all-cb" ${mcpAllGroups ? "checked" : ""}>
         直接全给模型（默认开）—— 不再让模型自己"先查再加载"
       </label>` +
      bar +
      `<div class="set-hint" style="margin-bottom:8px">
        开着的时候所有组<b>默认全部进模型上下文</b>；下面每组的开关会<b>记住</b>
        （落 <code>settings.json</code>，重启后仍生效）。关掉全局开关才回到老式的按需加载。
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

  // ---- 组块（设计稿 09:2:1493 / 2:1524 / 2:1537）----
  //
  // 一个组 = 组头（图标 · 名字 · 计数徽标 · 开关 · 折叠指示）+ 组体。
  // 「内置工具」「MCP 外部工具」带开关（能不能给模型）；「技能加载的工具」没有
  // —— 技能随会话自动加载，用户不该在这里替它做决定。
  const group = (
    key: string,
    icon: string,
    name: string,
    count: string,
    body: string,
    withSwitch: boolean,
    switchId: string,
    switchOn: boolean,
  ): string => {
    const open = TOOLS_GROUP_OPEN[key] !== false;
    return `
    <section class="mm-group${open ? "" : " folded"}" data-group="${key}">
      <div class="mm-group-head">
        <span class="mm-group-ico">${icon}</span>
        <span class="mm-group-name">${esc(name)}</span>
        <span class="mm-count">${esc(count)}</span>
        <span class="mm-group-flex"></span>
        ${
          withSwitch
            ? `<button type="button" class="sw mm-sw${switchOn ? " on" : ""}" id="${switchId}"
                 role="switch" aria-checked="${switchOn}" title="是否给模型"><i></i></button>`
            : ""
        }
        <button type="button" class="mm-fold" data-fold="${key}" title="展开/收起">${open ? "▾" : "▸"}</button>
      </div>
      <div class="mm-group-body"${open ? "" : " hidden"}>${body}</div>
    </section>`;
  };

  // 内置工具行（设计稿 09:2:1503）—— 左工具名，右「中文名 · 说明」
  const builtinRows = builtins
    .map(
      (t) => `
      <div class="mm-tool-row">
        <span class="mm-tool-ico">◆</span>
        <span class="mm-tool-name">${esc(t.name)}</span>
        <span class="mm-group-flex"></span>
        <span class="mm-tool-desc">${esc(
          [TOOL_LABELS[t.name] ?? "", t.desc].filter(Boolean).join(" · "),
        )}</span>
      </div>`,
    )
    .join("");

  // 技能组体：技能清单（技能随会话加载，不单独开关）
  const skillRows =
    skills.length === 0
      ? `<div class="mem-empty">还没有技能。对话中教它一套流程，让它用 <code>save_skill</code> 沉淀。</div>`
      : skills
          .map(
            (s) => `
      <div class="mm-tool-row">
        <span class="mm-tool-ico">◇</span>
        <span class="mm-tool-name">${esc(s.name)}</span>
        <span class="mm-group-flex"></span>
        <span class="mm-tool-desc">${esc(s.description)}</span>
      </div>`,
          )
          .join("");

  const mcpToolCount = mcp?.groups.reduce((a, g) => a + g.toolCount, 0) ?? 0;
  const mcpGroupCount = mcp?.groups.length ?? 0;

  const builtinGroup = group(
    "builtin",
    "◆",
    "内置工具",
    `${builtins.length} 个`,
    builtinRows || `<div class="mem-empty">读不到内置工具清单。</div>`,
    true,
    "mm-sw-builtin",
    builtinToolsEnabled,
  );

  const mcpGroup = group(
    "mcp",
    "◇",
    "MCP 外部工具",
    mcp ? `${mcpGroupCount} 组 · ${mcpToolCount} 工具` : "未连接",
    serverBlock + statusBlock + grantsBlock,
    true,
    "mm-sw-mcp",
    mcpAllGroups,
  );

  const skillGroup = group(
    "skills",
    "◈",
    "技能加载的工具",
    "随会话加载",
    skillRows,
    false,
    "",
    false,
  );

  // 说明块 + 权限闸门提示（设计稿 09:2:1548 / 2:1551）
  const noteBlock = `
    <div class="mm-note">
      <div class="mm-note-title">工具组跟会话走</div>
      <div class="mm-note-body">
        会话创建时定死模式，模式决定哪些组可见（标准=全给 / 闲聊=无 MCP）。切会话会重算活跃组 —— 已有任务在跑也不跳过重算。
        MCP 组默认「直接全给」，可逐组关闭；关闭后该组工具不在提示词里出现。
      </div>
    </div>
    <div class="mm-warn">
      <span class="mm-warn-ico">⚠</span>
      <span class="mm-warn-text">工具可见 ≠ 有权限。文件与命令各有独立闸门，白名单外的动作会在对话流里弹出权限卡要你拍板。</span>
    </div>`;

  return (
    head +
    builtinGroup +
    mcpGroup +
    skillGroup +
    noteBlock
  );
}

/** 工具页统计（底部状态栏「N 组 · M 个工具」）。 */
function refreshToolsStat(root: HTMLElement): void {
  const groups = root.querySelectorAll<HTMLElement>(".mm-group").length;
  const builtin = root.querySelectorAll<HTMLElement>(
    '.mm-group[data-group="builtin"] .mm-tool-row',
  ).length;
  const skill = root.querySelectorAll<HTMLElement>(
    '.mm-group[data-group="skills"] .mm-tool-row',
  ).length;
  toolsStatCache = { groups, tools: builtin + skill + mcpToolsCount };
}

function bindMcpView(root: HTMLElement): void {
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。

  // ---- 组折叠（设计稿 09：组头右侧的 ▸/▾）----
  root.querySelectorAll<HTMLButtonElement>(".mm-fold").forEach((btn) => {
    btn.addEventListener("click", () => {
      const key = btn.dataset.fold ?? "";
      const sec = btn.closest<HTMLElement>(".mm-group");
      const body = sec?.querySelector<HTMLElement>(".mm-group-body");
      if (!body) return;
      const open = body.hasAttribute("hidden");
      body.toggleAttribute("hidden", !open);
      sec?.classList.toggle("folded", !open);
      btn.textContent = open ? "▾" : "▸";
      TOOLS_GROUP_OPEN[key] = open;
    });
  });

  // ---- 内置工具总闸（设计稿 09:2:1501）----
  const swBuiltin = root.querySelector<HTMLButtonElement>("#mm-sw-builtin");
  swBuiltin?.addEventListener("click", async () => {
    const next = !builtinToolsEnabled;
    swBuiltin.classList.toggle("on", next);
    swBuiltin.setAttribute("aria-checked", String(next));
    try {
      await invoke("set_builtin_tools", { enable: next });
      builtinToolsEnabled = next;
      showToast(
        next ? "内置工具已给模型" : "内置工具已从模型工具表里拿掉",
        "ok",
        2400,
      );
    } catch (e) {
      swBuiltin.classList.toggle("on", !next);
      swBuiltin.setAttribute("aria-checked", String(!next));
      pushEntry("error", `切换内置工具失败：${e}`);
    }
  });

  // ---- MCP 组总闸（设计稿 09:2:1533，与组内逐组开关同源）----
  const swMcp = root.querySelector<HTMLButtonElement>("#mm-sw-mcp");
  swMcp?.addEventListener("click", async () => {
    const next = !mcpAllGroups;
    swMcp.classList.toggle("on", next);
    swMcp.setAttribute("aria-checked", String(next));
    try {
      await invoke("mcp_set_all_groups", { all: next });
      mcpAllGroups = next;
    } catch (e) {
      swMcp.classList.toggle("on", !next);
      swMcp.setAttribute("aria-checked", String(!next));
      pushEntry("error", `切换 MCP 工具组失败：${e}`);
    }
  });

  document.getElementById("mcp-refresh")?.addEventListener("click", async () => {
    await invoke("mcp_refresh").catch((e) => pushEntry("error", `刷新失败：${e}`));
    renderBody();
  });

  // ---- server 列表：以 `mcpServers` 为准（DOM 只负责画） ----
  //
  // 为什么要改成数据驱动：stdio 的 `args[]` / `env{}` 不是一段可读回来的
  // 文本，从 DOM 里 scrap 会把参数边界和键值对丢掉（见 `mcpServers` 的注释）。
  const collectServers = (): McpServerCfg[] => {
    // 把 DOM 上的"启用"勾选状态同步回数据（这是唯一一个纯 UI 态）
    const rows = root.querySelectorAll<HTMLElement>(".mcp-srv-row");
    rows.forEach((row) => {
      const i = Number(row.dataset.idx ?? "0");
      const cb = row.querySelector<HTMLInputElement>(".mcp-en-cb");
      if (mcpServers[i] && cb) mcpServers[i].enabled = cb.checked;
    });
    return mcpServers;
  };

  // 删除一行 → 同时从数据与 DOM 移除（未保存前只动内存）
  const bindDel = (btn: HTMLButtonElement) => {
    btn.addEventListener("click", () => {
      const row = btn.closest<HTMLElement>(".mcp-srv-row");
      const i = Number(row?.dataset.idx ?? "-1");
      if (i >= 0) mcpServers.splice(i, 1);
      row?.remove();
      // 删完要重编 idx —— 否则下一次 collect 会把 enabled 写到错的条目上
      root.querySelectorAll<HTMLElement>(".mcp-srv-row").forEach((r, n) => {
        r.dataset.idx = String(n);
        const cb = r.querySelector<HTMLInputElement>(".mcp-en-cb");
        if (cb) cb.dataset.idx = String(n);
      });
    });
  };
  root.querySelectorAll<HTMLButtonElement>(".mcp-srv-del").forEach(bindDel);

  /** 往列表尾部插一行（只画 DOM，数据已 push） */
  const appendRow = (idx: number, s: McpServerCfg) => {
    const list = document.getElementById("mcp-srv-list");
    if (!list) return;
    if (list.querySelector(".mem-empty")) list.innerHTML = "";
    const isStdio = mcpTransportOf(s) === "stdio";
    const row = document.createElement("div");
    row.className = "set-row mcp-srv-row";
    row.dataset.idx = String(idx);
    row.innerHTML = `
      <div class="set-name">
        <label class="mcp-en"><input type="checkbox" class="mcp-en-cb" data-idx="${idx}" ${
          s.enabled ? "checked" : ""
        }> 启用</label>
        <span class="mcp-kind ${isStdio ? "stdio" : "http"}">${isStdio ? "stdio" : "http"}</span>
        <code class="mcp-srv-id">${esc(s.id)}</code>
        <span class="set-cur">${esc(s.label || s.id)}</span>
      </div>
      <div class="set-url">${
        isStdio
          ? `<code>${esc(s.command ?? "")}</code>${
              (s.args ?? []).length
                ? ` <span class="mcp-args">${esc((s.args ?? []).join(" "))}</span>`
                : ""
            }`
          : `<code>${esc(s.url)}</code>`
      }</div>
      <div class="set-actions">
        <button class="set-btn mcp-srv-del" data-idx="${idx}">删除</button>
      </div>`;
    const del = row.querySelector<HTMLButtonElement>(".mcp-srv-del");
    if (del) bindDel(del);
    list.appendChild(row);
  };

  // 添加 HTTP 一行
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
    const idx = mcpServers.length;
    const cfg: McpServerCfg = {
      id: id || `s${idx + 1}`,
      url,
      enabled: true,
      label: label || id || `s${idx + 1}`,
      transport: "http",
    };
    mcpServers.push(cfg);
    appendRow(idx, cfg);
    for (const fid of ["mcp-new-id", "mcp-new-label", "mcp-new-url"]) {
      const el = document.getElementById(fid) as HTMLInputElement | null;
      if (el) el.value = "";
    }
    if (msg) msg.textContent = "已加入列表，点「保存并重连」生效";
  });

  // 添加 stdio 一行
  document.getElementById("mcp-stdio-add")?.addEventListener("click", () => {
    const id = (document.getElementById("mcp-new-sid") as HTMLInputElement | null)?.value.trim() ?? "";
    const label =
      (document.getElementById("mcp-new-slabel") as HTMLInputElement | null)?.value.trim() ?? "";
    const cmd = (document.getElementById("mcp-new-cmd") as HTMLInputElement | null)?.value.trim() ?? "";
    const argsRaw =
      (document.getElementById("mcp-new-args") as HTMLInputElement | null)?.value.trim() ?? "";
    const msg = document.getElementById("mcp-srv-msg");
    if (!cmd) {
      if (msg) msg.textContent = "命令不能为空";
      return;
    }
    const idx = mcpServers.length;
    const cfg: McpServerCfg = {
      id: id || `s${idx + 1}`,
      url: "",
      enabled: true,
      label: label || id || `s${idx + 1}`,
      transport: "stdio",
      command: cmd,
      args: splitArgs(argsRaw),
      env: {},
    };
    mcpServers.push(cfg);
    appendRow(idx, cfg);
    for (const fid of ["mcp-new-sid", "mcp-new-slabel", "mcp-new-cmd", "mcp-new-args"]) {
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

  // 全局「直接全给」
  document.getElementById("mcp-all-cb")?.addEventListener("change", async (e) => {
    const on = (e.target as HTMLInputElement).checked;
    mcpAllGroups = on;
    try {
      await invoke("mcp_set_all_groups", { all: on });
      showToast(on ? "已设为「直接全给模型」" : "已切回按需加载", "ok");
    } catch (err) {
      showToast(`设置失败：${err}`, "error");
    }
    renderBody();
  });

  // 每组的「给模型」开关 —— **落盘记忆**，重启后仍生效
  root.querySelectorAll<HTMLInputElement>(".mcp-grp-cb").forEach((cb) =>
    cb.addEventListener("change", async () => {
      const name = cb.dataset.group!;
      const on = cb.checked;
      try {
        await invoke("mcp_set_group_enabled", { name, enabled: on });
        showToast(`${on ? "已启用" : "已停用"}「${name}」`, "ok", 2000);
      } catch (e) {
        pushEntry("error", `${on ? "启用" : "停用"}「${name}」失败：${e}`);
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

// ---------------- 设置 › 项目（拼装包） ----------------

/**
 * 项目页 —— 列出全部项目包，激活 / 退出 / 编辑 / 导入。
 *
 * 页面分四段（顺序即用户的理解顺序）：
 *   ① 当前状态（激活了什么、有什么警告）
 *   ② 包列表（可激活 / 删除）
 *   ③ 当前包声明的脚本工具（只读展示，让用户知道项目带来了什么能力）
 *   ④ 新建 / 编辑（JSON 文本框，因为 project.json 本来就该能用记事本改）
 */
function renderProjectView(v: ProjectListView | null): string {
  const head = subHeader("项目");

  if (!v) {
    return head + `<div class="mem-empty">读取项目列表失败。</div>`;
  }

  // ---- ① 当前状态 ----
  let statusBlock: string;
  if (!v.active) {
    statusBlock = `<div class="mem-empty">当前没有激活任何项目 —— 行为与普通模式完全一致。</div>`;
  } else {
    const b = v.bundles.find((x) => x.id === v.active);
    const lines: string[] = [];
    if (v.permError) {
      lines.push(
        `<div class="proj-warn">⚠️ <b>权限预设已整份丢弃</b>（fail-closed）：${esc(
          v.permError,
        )}<br><i>为避免半生效，整个 permPreset 都没应用 —— 当前用的是你自己的权限设置。</i></div>`,
      );
    }
    if (v.unresolved.length) {
      const items = v.unresolved
        .map(([kind, id, why]) => `<li><code>${esc(kind)}</code> · ${esc(id)} — ${esc(why)}</li>`)
        .join("");
      lines.push(
        `<div class="proj-warn">⚠️ 有 ${v.unresolved.length} 项引用没解析上（已跳过，不影响其他功能）：<ul>${items}</ul></div>`,
      );
    }
    statusBlock = `
      <div class="set-row cur">
        <div class="set-name">📦 ${esc(b?.name ?? v.active)}</div>
        <div class="set-url">${esc(b?.description ?? "")}</div>
        <div class="set-url">技能 ${b?.skillCount ?? 0} 项 · 脚本工具 ${v.tools.length} 个${
          b?.hasPermPreset ? " · 含权限预设" : ""
        }</div>
        <div class="set-actions">
          <button class="set-btn" id="proj-deactivate">退出项目</button>
        </div>
      </div>
      ${lines.join("")}`;
  }

  // ---- ② 包列表 ----
  const rows = v.bundles.length
    ? v.bundles
        .map(
          (b) => `
      <div class="set-row${b.active ? " cur" : ""}${b.valid ? "" : " deny"}">
        <div class="set-name">
          ${b.valid ? "" : "⚠️ "}${esc(b.name)}
          ${b.hasPermPreset ? '<span class="proj-tag perm">含权限预设</span>' : ""}
        </div>
        <div class="set-url">${esc(b.description || "（无说明）")}</div>
        <div class="set-url">${
          b.valid
            ? `技能 ${b.skillCount} · 工具 ${b.toolCount}`
            : `装配失败：${esc(b.error ?? "未知原因")}`
        }</div>
        <div class="set-actions">
          ${
            b.active
              ? `<span class="set-cur">当前</span>`
              : b.valid
                ? `<button class="set-btn ok proj-activate" data-id="${esc(b.id)}">激活</button>`
                : // 无效包不给激活按钮：点了必然报错，不如让用户先去修
                  `<span class="set-cur">先修好才能激活</span>`
          }
          <button class="set-btn proj-edit" data-id="${esc(b.id)}">编辑</button>
          <button class="set-btn del proj-del" data-id="${esc(b.id)}">删除</button>
        </div>
      </div>`,
        )
        .join("")
    : `<div class="mem-empty">还没有项目包。项目 = 一套配好的「技能范围 + 模型槽位 + 权限预设 + 脚本工具」，可保存、切换、分享。</div>`;

  // ---- ③ 当前包的脚本工具 ----
  const toolsBlock = v.tools.length
    ? `<div class="mem-head">🔧 本项目带来的脚本工具 ${v.tools.length}</div>` +
      v.tools
        .map(
          (t) => `
      <div class="set-row">
        <div class="set-name"><code>${esc(t.name)}</code>${
            t.perm ? `<span class="proj-tag perm">${esc(t.perm)}</span>` : ""
          }</div>
        <div class="set-url">${esc(t.description)}</div>
        <div class="set-url"><code>${esc(t.template.join(" "))}</code></div>
      </div>`,
        )
        .join("") +
      `<div class="set-hint">
        脚本工具与 <code>run_command</code> 走<b>同一套闸门</b>：命令策略判定 + 弹卡 + 审计。
        参数按 argv 逐参传递（不经 shell），所以参数里的 <code>;</code> <code>|</code> 只是普通字符。
       </div>`
    : "";

  // ---- ④ 新建 / 编辑 ----
  const editorBlock = `
    <div class="mem-head">✏️ 编辑 / 新建项目包</div>
    <div class="set-hint" style="margin-bottom:6px">
      <code>project.json</code> 就是这段 JSON。字段说明见
      <code>docs/compose/spec/composable-project-bundle.md</code>。
      <b>密钥永远不要写进来</b> —— <code>slots</code> 只写模型/搜索的 id。
    </div>
    <div class="set-form" style="margin-bottom:6px">
      <input id="proj-edit-id" placeholder="项目 id（= 目录名，如 学习助手）" spellcheck="false">
    </div>
    <textarea id="proj-edit-body" class="proj-editor" spellcheck="false"
      placeholder='{"schemaVersion":1,"name":"学习助手","description":"...","skills":[],"tools":[]}'></textarea>
    <div class="set-actions" style="margin-top:6px">
      <button class="set-btn ok" id="proj-save">保存</button>
      <button class="set-btn" id="proj-new-template">填入模板</button>
      <button class="set-btn" id="proj-validate">只校验不保存</button>
    </div>
    <div class="set-hint" id="proj-msg"></div>`;

  return head + statusBlock + `<div class="mem-head">📦 全部项目包</div>` + rows + toolsBlock + editorBlock;
}

function bindProjectView(root: HTMLElement): void {
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。

  const msg = () => document.getElementById("proj-msg");
  const idEl = () => document.getElementById("proj-edit-id") as HTMLInputElement | null;
  const bodyEl = () => document.getElementById("proj-edit-body") as HTMLTextAreaElement | null;

  // 激活
  root.querySelectorAll<HTMLButtonElement>(".proj-activate").forEach((btn) => {
    btn.addEventListener("click", async () => {
      try {
        await invoke("project_activate", { id: btn.dataset.id });
        showToast("已激活项目（下一轮对话生效）", "ok");
        renderBody();
      } catch (e) {
        showToast(`激活失败：${e}`, "error");
      }
    });
  });

  // 退出项目
  document.getElementById("proj-deactivate")?.addEventListener("click", async () => {
    try {
      await invoke("project_activate", { id: null });
      showToast("已退出项目", "ok");
      renderBody();
    } catch (e) {
      showToast(`退出失败：${e}`, "error");
    }
  });

  // 编辑：读原始文本填进编辑器
  root.querySelectorAll<HTMLButtonElement>(".proj-edit").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const id = btn.dataset.id ?? "";
      try {
        const body = await invoke<string>("project_read", { id });
        const i = idEl();
        const b = bodyEl();
        if (i) i.value = id;
        if (b) b.value = body;
        const m = msg();
        if (m) m.textContent = `已载入「${id}」，改完点保存`;
        b?.scrollIntoView({ block: "center" });
      } catch (e) {
        showToast(`读取失败：${e}`, "error");
      }
    });
  });

  // 删除（只删 project.json，保留 MEMORY.md 与脚本）
  root.querySelectorAll<HTMLButtonElement>(".proj-del").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const id = btn.dataset.id ?? "";
      if (!confirm(`删除项目包「${id}」的 project.json？\n\n项目记忆（MEMORY.md）与脚本文件都会保留。`)) {
        return;
      }
      try {
        await invoke("project_delete", { id });
        showToast("已删除项目包", "ok");
        renderBody();
      } catch (e) {
        showToast(`删除失败：${e}`, "error");
      }
    });
  });

  // 填入模板（降低手写门槛）
  document.getElementById("proj-new-template")?.addEventListener("click", () => {
    const b = bodyEl();
    if (!b) return;
    b.value = JSON.stringify(
      {
        schemaVersion: 1,
        name: "我的项目",
        description: "一句话说明这个项目是干什么的",
        skills: [],
        slots: { model: "", search: "" },
        permPreset: {
          execTrust: "ask",
          commandAllowExtra: [],
          fileAllowPaths: [],
        },
        tools: [
          {
            name: "my_tool",
            description: "这个脚本工具做什么",
            params: { type: "object", properties: {} },
            commandTemplate: ["pwsh", "-NoProfile", "-File", "{project}/tools/my_tool.ps1"],
            perm: "read-only",
          },
        ],
      },
      null,
      2,
    );
    const m = msg();
    if (m) m.textContent = "已填入模板 —— 把 skills 留空 = 不限制技能；不需要的段整段删掉即可";
  });

  // 只校验不保存：用后端同一套校验器（前端不重复实现，避免两套规则分叉）
  document.getElementById("proj-validate")?.addEventListener("click", async () => {
    const id = idEl()?.value.trim() ?? "";
    const body = bodyEl()?.value ?? "";
    const m = msg();
    if (!body.trim()) {
      if (m) m.textContent = "内容为空";
      return;
    }
    if (!id) {
      if (m) m.textContent = "先填项目 id（校验要用它当目录名）";
      return;
    }
    // 校验走 `project_save` 会真的写盘 —— 这里用一个临时 id 写进临时目录不可行，
    // 所以退一步：让用户点保存，后端校验失败时会原样报错且**不会留下坏文件**
    // （`project_save` 是先解析校验、后写盘）。
    if (m) m.textContent = "点「保存」即会先校验；校验不过不会写盘、也不会覆盖原文件。";
  });

  // 保存
  document.getElementById("proj-save")?.addEventListener("click", async () => {
    const id = idEl()?.value.trim() ?? "";
    const body = bodyEl()?.value ?? "";
    const m = msg();
    if (!id) {
      if (m) m.textContent = "项目 id 不能为空（它同时是目录名）";
      return;
    }
    if (!body.trim()) {
      if (m) m.textContent = "内容为空";
      return;
    }
    try {
      if (m) m.textContent = "校验并保存中…";
      await invoke("project_save", { id, body });
      showToast(`已保存项目包「${id}」`, "ok");
      renderBody();
    } catch (e) {
      // 校验失败的原话直接显示 —— 后端的信息比前端能编的准确
      if (m) m.textContent = `保存失败：${e}`;
    }
  });
}

// ---------------- 设置 › Token 用量 ----------------

/**
 * 模型展示名 = 接口 id 本身（显示名已废弃；函数留着兼容用量页调用点）。
 *
 * ⚠️ 留在 main.ts 而不是搬进 `usage.ts`：它要读 `models` 这个**模块级状态**
 * （当前模型列表）。纯聚合逻辑才搬得走 —— 判定标准就是"是否引用模块级绑定"。
 */
function modelName(id: string): string {
  return id;
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

  // 全局缓存命中率：prompt token 的大头是命中价，
  // 全价部分 = 输入 − 命中。命中率掉下来时成本会成倍上涨而总量看不出变化，
  // 所以这里必须**单独成一张卡**，而不是塞进「全部」的 tooltip。
  const allPrompt = records.reduce((a, r) => a + r.usage.prompt, 0);
  const allHit = records.reduce((a, r) => a + (r.usage.cacheHit ?? 0), 0);
  const allRate = cacheHitRate(allPrompt, allHit);
  const allMiss = Math.max(0, allPrompt - allHit);
  const verdict = allRate === null ? null : cacheVerdict(allRate);

  const cards = `
    <div class="usage-cards">
      <div class="usage-card"><b>${fmtTokens(tTotal)}</b><i>今日</i></div>
      <div class="usage-card"><b>${fmtTokens(weekTotal)}</b><i>近 7 天</i></div>
      <div class="usage-card"><b>${fmtTokens(allTotal)}</b><i>全部</i></div>
    </div>
    ${
      verdict === null
        ? ""
        : `<div class="usage-cache${verdict.warn ? " usage-cache--warn" : ""}">
             <div class="usage-cache-head">
               <span>缓存命中率 <b>${verdict.label}</b></span>
               <span class="usage-cache-miss" title="按全价计费的那部分输入">全价输入 ${fmtTokens(
                 allMiss,
               )}</span>
             </div>
             <div class="usage-cache-hint">${
               verdict.warn
                 ? "命中率偏低 —— 上下文前缀可能在变（换模型/改配置/技能或 MCP 组变化都会让缓存失效）。"
                 : "输入的大头走缓存命中价，全价只占一小截。"
             }</div>
           </div>`
    }`;

  const rows = days
    .map((d) => {
      const pct = Math.max(2, Math.round((d.total / maxDay) * 100));
      const dayRate = cacheHitRate(d.prompt, d.cacheHit);
      const dayVerdict = dayRate === null ? null : cacheVerdict(dayRate);
      const modelRows = [...d.models.entries()]
        .sort((a, b) => b[1].total - a[1].total)
        .map(([id, m]) => {
          const r = cacheHitRate(m.prompt, m.cacheHit);
          const v = r === null ? null : cacheVerdict(r);
          const badge =
            v === null
              ? ""
              : `<i class="usage-cache-badge${v.warn ? " is-warn" : ""}" title="命中 ${
                  m.cacheHit
                } / 输入 ${m.prompt}，全价部分 ${Math.max(
                  0,
                  m.prompt - m.cacheHit,
                )}">缓存 ${v.label}</i>`;
          return `<div class="usage-model"><span>${esc(modelName(id))}</span>${badge}<b>${fmtTokens(
            m.total,
          )}</b></div>`;
        })
        .join("");
      return `
        <div class="usage-day">
          <div class="usage-day-head">
            <span class="usage-date">${d.date}</span>
            ${
              dayVerdict === null
                ? ""
                : `<span class="usage-cache-day${
                    dayVerdict.warn ? " is-warn" : ""
                  }">缓存 ${dayVerdict.label}</span>`
            }
            <span class="usage-total" title="输入 ${d.prompt} · 输出 ${
              d.completion
            } · 缓存命中 ${d.cacheHit}">${fmtTokens(d.total)} tok</span>
          </div>
          <div class="usage-bar"><i style="width:${pct}%"></i></div>
          ${modelRows}
        </div>`;
    })
    .join("");

  return head + cards + `<div class="usage-list">${rows}</div>`;
}

function bindUsageView(_root: HTMLElement): void {
  // 返回按钮已挪到滚动区外的 `#sub-bar`，绑定统一在 `renderSubBar` 里做。
  // ⚠️ 这里**不能**再绑一次：同一个按钮挂两个 handler 会让 `switchView` 跑两遍，
  //    第二遍撞上它的 toggle 逻辑（再点同一个非 chat 视图 → 回对话），
  //    表现出来就是"点了返回却回到了对话页"。
}

// ---------------- 轻提示（toast） ----------------

/**
 * 非对话页（设置 / 记忆 / 会话）里的临时信息 —— **浮动提示，不碰消息区**。
 *
 * 为什么要有这个东西：这些页面的报错（保存失败、测试结果…）以前一律走
 * `pushEntry`，而 `pushEntry` 会 `paintMessages(panel-body)` 整块重绘。
 * 结果是：在设置页点「测」→ 模型列表被顶掉、聊天记录凭空出现在设置页里，
 * 而输入框因为 `renderBody()` 早在切到设置页时就 `display:none` 了 ——
 * 表现就是"测试结果跑进聊天界面 + 输入框消失"（曾因此报过 bug）。
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
    // ⚠️ 守卫：定时器活过环境销毁是常态（面板一关它照样在队列里）。
    //   测试环境 teardown 之后 `window` / `document` 整个不存在，
    //   下面再摸 window 就是 ReferenceError → vitest 记成 unhandled error →
    //   **整个文件退出码 1**，哪怕断言全绿（与 models.ts 的 fgTimer 同一个坑）。
    //   `domAlive` 守卫：无 DOM 时安静退出，不留噪声。
    if (!domAlive()) return;
    el.classList.remove("in");
    window.setTimeout(() => {
      if (!domAlive()) return;
      el.remove();
    }, 220);
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
  if (document.getElementById("panel-body")) {
    // ⚠️ **不抢滚动**（2026-10-14 用户要「给用户选择」）：
    //    用户正在往上翻历史时，新消息一律不把他拽回底部 ——
    //    改由右下角的「↓ 回到底部」按钮（带新消息计数）让他自己决定。
    //    在底部时照旧跟随。
    if (isChatAtBottom()) {
      repaintChatToBottom();
    } else {
      repaintChatKeepScroll();
      // 用户自己发的那条不算"漏看的" —— `send()` 随后会显式贴底
      if (role !== "user") chatMissed += 1;
    }
    syncJumpButton();
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
  void listen<{ sessionId: string; p: ProgressEvent }>("agent-progress", (e) => {
    const { sessionId, p } = e.payload;
    // 多会话并行 run：只画**当前会话**那条 run 的进度。
    // 别的会话在后台跑的事件直接丢 —— 它们的结果由后端落盘，切过去就能看到。
    if (sessionId && sessionId !== currentSessionId) return;

    // 「插话被送达」：改排队消息的角标 + 把它画进进行中气泡的时间线
    // （与重载后的形态一致 —— 重载走 sessionToEntries，插话同样画在时间线上）
    if (p.kind === "steer") {
      const q = entries.find((x) => x.steerId === p.id);
      if (q) q.queued = false;
      if (p.id) markSteerDelivered(p.id);
      // 送达的插话带图时，图只能画在时间线上（插话不铺独立气泡）
      const run = entries.find((x) => x.role === "running");
      if (run) {
        run.items = run.items ?? [];
        run.items.push({
          kind: "steer",
          name: null,
          detail: p.text ?? "",
          images: p.images && p.images.length ? p.images : undefined,
        });
        // 只调度重绘（内部会 rAF + 250ms 看门狗兜底），不需要传元素
        scheduleRunPaint();
      }
      return;
    }

    // ⚠️ 不能用 entries[len-1]：允许执行中「插话」后，队尾可能是刚排队的用户消息，
    //    那样所有进度事件都会被整批丢弃。进行中的气泡按 role 找。
    let last = entries.find((x) => x.role === "running");
    if (!last) {
      // running 气泡丢了（切会话/重载把它冲掉了）但后端还在跑 ——
      // 以前这里直接 `return`，事件全被吞掉：用户看到的是「明明在动、界面不更新」。
      // 现在补一个气泡把事件接住（会话归属已在上面按 sessionId 校验过）。
      last = { role: "running", text: "正在思考…", items: [] };
      entries.push(last);
    }
    last.items = last.items ?? [];

    switch (p.kind) {
      case "thinking":
        if (p.iteration) last.text = `正在思考…（第 ${p.iteration} 轮）`;
        // 上一轮流式正文收进时间线（保留可看），再开新轮。
        // ⚠️ 不能只「不清空」——那会把多轮正文 capAppend 成一坨（曾因此报过 bug）。
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
          // 思考过程**不截**（定稿："完全不截"）：直接拼接，不设上限。
          // 见 MAX_STREAM_TEXT_CHARS 的说明。
          cur.detail = (cur.detail ?? "") + p.reasoning;
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

      case "toolTick": {
        // 长工具心跳：只就地更新**最后一个** tool_call 条目 + 头部状态，
        // 不新增时间线条目（每 2s 一条会把时间线刷成流水账）。
        //
        // ⚠️ 秒数由**前端本地每秒推**，后端这个 2s 心跳只负责对齐起点 + 带输出尾巴。
        //    理由：后端心跳一旦中断（读任务被掐 / 事件被吞 / 单 run 被卡住），
        //    显示就冻在某个秒数上 —— 曾实报「一直 4s」「心跳也没有了」，
        //    看起来像死掉了。本地推的话，哪怕后端一个心跳都不来，秒数照样往前走。
        startToolTick(p.name ?? "工具", p.elapsedSecs ?? 0, p.tail);
        break;
      }

      default: {
        // 新一轮工具调用 / 工具结果落地 → 旧心跳收摊，秒数由新的接棒
        if (p.kind === "toolCall" || p.kind === "toolResult") stopToolTick();
        const item = progressItem(p);
        if (item) {
          // 工具返回**并进上一条调用**，不新占一行。
          // ⚠️ 必须是"原地合并"而不是 push 完再重排：条目数一旦先增后减，
          //    `patchTimeline` 的 `data-n` 就对不上实际节点数，会退回全量重画
          //    （表现是折叠块被重建、展开状态被冲掉）。
          const tail = last.items[last.items.length - 1];
          const pair =
            item.kind === "tool_result" &&
            tail &&
            tail.kind === "tool_call" &&
            (tail.name ?? "") === (item.name ?? "") &&
            !isToolFailure(item.detail);
          if (pair && tail) {
            tail.result = item.detail;
            // 耗时跟着返回走（合成成一行后，summary 上显示「· 1.2s」）
            if (item.durationMs !== undefined) tail.durationMs = item.durationMs;
          } else {
            last.items.push(item);
          }
        }
        break;
      }
    }

    // 只更新「进行中」那一个气泡，不动输入框、也不重建其它消息
    // （重建会让悬停中的复制按钮反复淡入淡出 = 闪烁）
    scheduleRunPaint();
  });
}

// ---------------- 长工具心跳：前端本地每秒推 ----------------
//
// 后端每 2s 推一次 `toolTick`（带输出尾巴），但**秒数显示由前端自己每秒 +1**。
// 理由见 `installProgressListener` 里 toolTick 分支的注释 —— 后端心跳一断，
// 显示就冻在某个秒数上，看起来像死掉了；本地推就不会。
let tickTimer: ReturnType<typeof setInterval> | undefined;
/** 秒数起点（毫秒时间戳）。后端心跳到达时按它的秒数对齐一次 */
let tickBaseAt = 0;
let tickName = "";
let tickTail = "";

/** 后端心跳到达：对齐起点 + 更新工具名/输出尾巴（后端的秒数优先，它更准） */
function startToolTick(name: string, elapsedSecs: number, tail?: string): void {
  tickName = name;
  if (tail) tickTail = tail;
  tickBaseAt = Date.now() - elapsedSecs * 1000;
  if (!tickTimer) {
    tickTimer = setInterval(() => paintToolTick(), 1000);
  }
  paintToolTick();
}

/** 工具跑完 / 一轮结束 / 气泡没了 → 收摊 */
function stopToolTick(): void {
  if (tickTimer) clearInterval(tickTimer);
  tickTimer = undefined;
  tickTail = "";
}

/** 就地刷「已运行 Ns」——不新增时间线条目（每秒一条会刷成流水账） */
function paintToolTick(): void {
  const run = entries.find((x) => x.role === "running");
  if (!run) {
    stopToolTick();
    return;
  }
  const secs = Math.max(0, Math.floor((Date.now() - tickBaseAt) / 1000));
  const items = run.items ?? [];
  const tailItem = items[items.length - 1];
  // 为什么是"最后一个"：工具执行是串行的 —— toolCall 事件先到、tool_result 执行完
  // 才补，所以执行期间 items 的尾条就是正在跑的那个调用。
  if (tailItem && tailItem.kind === "tool_call") {
    tailItem.tick = `⏳ 已运行 ${secs}s${tickTail ? `｜${tickTail}` : ""}`;
  }
  run.text = `正在执行 ${tickName || "工具"}…（${secs}s）`;
  scheduleRunPaint();
}

/**
 * 「分叉」入口 —— 只画在**主聊天里、AI 完整回复**上。
 *
 * 语义（后续修正）：以这条**完整的一问一答**为界，把下一轮
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
 * 语义：对这条回答不满意，**丢掉它并重跑这一轮**。
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
 * 语义：**删掉这条，以及它后面的全部消息**。
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

/**
 * 「编辑」按钮 —— 只挂在**最后一条已落盘的用户消息**上。
 *
 * 语义（ChatGPT 式「编辑并重发」）：把它和它后面的全部回复截掉，
 * 原文放回输入框，改完重新发送。实现完全复用删除链路
 * （`session_truncate` + `refillInput`），后端零新增命令。
 *
 * busy 时按钮照渲染（渲染态不重算），点击由 `editUserMessage` 的忙碌检查拦截。
 */
function renderEditBtn(e: ChatEntry, lastUserStored: number): string {
  if (e.role !== "user" || e.storedIdx === undefined || e.storedIdx !== lastUserStored) {
    return "";
  }
  return `<button type="button" class="msg-edit" data-upto="${e.storedIdx}" title="编辑并重发：这条消息和它后面的回复会被截掉，原文放回输入框">编辑</button>`;
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
      return {
        kind: "tool_result",
        name: p.name ?? "?",
        detail: p.preview ?? "",
        durationMs: p.durationMs,
      };
    case "toolError":
      return {
        kind: "tool_error",
        name: p.name ?? "?",
        detail: p.message ? `失败：${p.message}` : "执行失败",
        durationMs: p.durationMs,
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

/**
 * 窗口拿到 OS 焦点后，把 DOM 焦点补回输入框。
 *
 * 为什么需要：窗口在"非前台"状态下构建时，WebView 里的 `input.focus()` 不生效
 * （输入游标看不见、打字没反应）。`apply_mode(panel)` 现在会用
 * `win32::force_foreground` 把窗口真正提到前台，但前台切换与 DOM 焦点建立的
 * 先后顺序不保证 —— 这里在 `focus` 事件上补一次，对**自动化输入**尤其关键。
 *
 * 只补"面板内没有任何输入焦点"的情况：用户在消息区选字 / 点按钮 / 正在确认卡
 * 里打字时不抢，免得打断。
 */
function installFocusRefocus(): void {
  window.addEventListener("focus", () => {
    if (mode !== "panel") return;
    const input = document.getElementById("input") as HTMLTextAreaElement | null;
    if (!input) return;
    const ae = document.activeElement as HTMLElement | null;
    const busyElsewhere =
      ae != null && ae !== document.body && ae.closest("input, textarea, select, button, .msg-body, .confirm-mask") != null;
    if (!busyElsewhere) input.focus();
  });
}

// ---------------- 业务 ----------------

async function loadModels(): Promise<void> {
  try {
    models = await invoke<ModelView[]>("list_models");
    // 身份 = (Base URL, 接口 id)：get_selected_model 返回 {id, url} 双字段
    const sel = await invoke<{ id: string; url: string | null } | null>("get_selected_model");
    selectedModel = sel?.id ?? null;
    selectedModelUrl = sel?.url ?? null;
    // ⚠️ 消歧回填：旧设置里**只有 id、没有 url**（身份键重构之前写的），
    // 而同 id 可能有多条（不同 Base URL）。不补的话「当前项」判定会命中所有同 id 的行 ——
    // 现象是「两个 glm-5.3-flash 都带当前标记」。这里定死成解析出来的那条并回写。
    if (selectedModel && !selectedModelUrl) {
      const resolved = curModel();
      if (resolved) {
        selectedModelUrl = resolved.url;
        await invoke("set_selected_model", { id: selectedModel, url: resolved.url }).catch(
          () => {},
        );
      }
    }
    // 旧值失效（后端 list_models 已清过）或本来就没选 → 兜底选第一个
    if (!selectedModel && models.length > 0) {
      selectedModel = models[0].id;
      selectedModelUrl = models[0].url;
      await invoke("set_selected_model", { id: selectedModel, url: selectedModelUrl }).catch(
        () => {},
      );
    }
    // 分组显示名：面板的模型卡片要显示「组别 + 模型名」
    // （定稿：这一块就是放模型的，组别+模型名）。
    // 以前只在独立窗口（models.ts）读，所以面板上永远只看到裸模型名。
    modelGroupNames = await invoke<Record<string, string>>("model_group_names").catch(
      () => ({}),
    );
  } catch (e) {
    models = [];
    entries.push({ role: "error", text: `加载模型列表失败：${e}` });
  }
}

/** 模型管理窗口里增删改/切模型后，后端会广播 —— 面板把卡片和下拉刷一遍 */
function installModelsListener(): void {
  void listen("models-changed", () => {
    void loadModels().then(() => {
      if (mode === "panel") renderPanelRefreshChip();
    });
  });
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

/**
 * 发一条消息：**立刻开跑**（定稿删掉「合并窗口」）。
 *
 * 曾经有个 3s 的合并窗口（连发的几条攒起来一次发）。两个问题：
 *   ① 观感上"点了没反应"，还得靠提示条解释；
 *   ② **致命**：消息进了待发批次时若把面板收起（切悬浮球态、不退出），
 *      批次没有 DOM 可落，被当成"没地方去"直接丢掉 —— 消息**凭空消失**。
 * 定稿结论："直接删掉这个逻辑，就相当于只多一个发图算了"。
 * 于是这里回到最朴素的语义：**按下发送 = 立刻发出**，出去就是出去了。
 * （仍然保留"先同步清空输入框"那一步 —— 它修的是双击回车连发两次的老 bug。）
 *
 * 「连发多条」现在只有一条路：run 已经在跑 → `sendSteer()` 进插话队列（原有机制）。
 */
/**
 * 用哪个内核跑对话。
 *
 * - "pi"    → `chat_pi`（pi 子进程，2026-10-22 新增）
 * - "legacy"→ `chat`（自研 agent loop）
 *
 * 默认走 pi —— 但**只在 pi 内核可用时**。`piStatus` 拉到后才知道。
 * 不可用就静默退回 legacy（用户不该看到一个不能用的选项）。
 */
let usePiKernel = true;
let piKernelAvailable = false;
let piStatusChecked = false;

/**
 * 查 pi 内核是否可用（启动时调一次）。
 *
 * 可用 = exe 旁边有 `runtime/`（内置 node + pi bundle）。
 * 不可用就静默退回自研内核 —— 用户不该看到一个不能用的选项。
 */
async function refreshPiStatus(): Promise<void> {
  if (piStatusChecked) return;
  piStatusChecked = true;
  try {
    const st = await invoke<{ available: boolean }>("pi_status");
    piKernelAvailable = !!st.available;
    if (!piKernelAvailable) {
      usePiKernel = false;
    }
  } catch {
    piKernelAvailable = false;
    usePiKernel = false;
  }
  updateKernelChip();
}

/**
 * 刷新工具栏上的内核 chip。
 *
 * 只在 pi 可用时显示 chip；不可用就不显示（用户看不到一个不能用的选项）。
 */
function updateKernelChip(): void {
  const el = document.getElementById("kernel-chip");
  if (!el) return;
  el.textContent = usePiKernel ? "pi" : "自研";
  el.title = usePiKernel
    ? "内核：pi（agent harness）—— 点开切换到自研内核"
    : "内核：自研 —— 点开切换到 pi 内核";
}

/** 切换内核（pi ↔ 自研）。 */
async function toggleKernel(): Promise<void> {
  if (!piKernelAvailable) return;
  usePiKernel = !usePiKernel;
  updateKernelChip();
  pushEntry(
    "system",
    usePiKernel
      ? "已切换到 pi 内核（下一轮生效）"
      : "已切换到自研内核（下一轮生效）",
  );
}

async function send(): Promise<void> {
  // 当前会话在跑 → 走「插话」通道（排队），而不是静默吞掉用户打的字。
  // ⚠️ `busy` 是**按会话**算的（多会话并行 run）：别的会话在跑不影响这里开新任务。
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

  // ⚠️ **同步**清空输入框，必须放在第一个 await 之前 ——
  //    原来清空在 `mem_pending` 之后，机器卡时快速双击回车会把
  //    同一句话连发两次（Rust 侧 `chat` 没有"这条会话已在跑"的拦截）。
  input.value = "";
  input.style.height = "auto";
  draftInput = ""; // 发出去才算用掉，这时候草稿才能清
  pendingImages = [];
  renderPreview();

  const pendingBefore = await invoke<Candidate[]>("mem_pending").catch(
    () => [] as Candidate[],
  );
  pushEntry("user", text || "（仅图片）", undefined, imgs);

  // 先放一个「进行中」气泡，进度事件会往里建时间线条目；结束后替换成最终结果
  entries.push({ role: "running", text: "正在思考…", items: [] });
  runningSessionId = currentSessionId;
  // 这一轮归属哪个会话 —— 用户中途切走后，收尾逻辑要靠它判断"还该不该动当前视图"
  const runSid = runningSessionId;
  const b0 = document.getElementById("panel-body");
  if (b0) {
    // ⚠️ 发消息**无条件**贴底，不看当前滚到哪 —— 用户主动发了新问题，
    //    意图明确是「从这儿继续」。pushEntry 那边不抢滚动，这里是刻意的例外。
    repaintChatToBottom();
    chatMissed = 0;
    chatMissedCountedForRun = false; // 新的一轮，重新计
    syncJumpButton();
  }

  markRunStart(runSid);
  // 新的一轮 —— 上一轮的「本轮不再问」不该管到这一轮
  permMuteThisTurn = false;

  try {
    const run = await invoke<AgentRun>("chat", { input: text, images: imgs });
    // ⚠️ 用户可能在等待期间切走了。切走后 entries 已经是**别的会话**的内容，
    //    所以清气泡 / 重载 / 提示一律不动 —— 这一轮的结果后端已经落回它自己的
    //    会话（`append_run_in`），切回去就能看到。
    if (runSid !== currentSessionId) {
      // 出错信号仍然要给（否则"后台跑挂了"完全无声）
      if (run.interrupted) {
        unseenError = true;
        if (mode === "orb") applyOrbState();
      }
      return;
    }
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
    // 用户已经切走 → 当前视图是别的会话，别往里写红色错误；只把"出事了"的信号留下
    const stillHere = runSid === currentSessionId;
    if (msg.includes("已停止")) {
      if (stillHere) {
        const idx = entries.findIndex((x) => x.role === "running");
        if (idx >= 0) entries.splice(idx, 1);
        pushEntry("system", "⏹ 已停止");
      }
    } else {
      // ⚠️ 出错时**不能只删掉进行中气泡**（曾因此报过 bug）。
      //
      // 那个气泡里装着这一轮全部的思考与工具步骤，删了之后屏幕上只剩一条红字
      // —— 表现就是"他做的也都消失了"。后端现在在错误路径也会把部分
      // 结果落盘（见 `lib.rs::persist_run`），所以这里改成「以磁盘为准重载」：
      // 留下来的就是它真正做过的事，顺带拿到 `storedIdx`
      // （重新生成 / 删除 / 分叉 这些按钮要用它）。
      if (!stillHere) {
        // 后台那个会话挂了：给一条轻提示 + 亮红球，等用户切回去看细节
        showToast(`后台会话出错：${msg.slice(0, 90)}`, "error", 4000);
        unseenError = true;
        if (mode === "orb") applyOrbState();
        return;
      }
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
        "若本轮超出能力，建议把问题拆小或补充上下文后重试。",
      );
    }
  } finally {
    stopToolTick();
    // 注销本轮 + 按当前会话重算 busy / 后台运行提示条
    markRunEnd(runSid);
  }

  // 记忆候选若在本轮增加 → 提醒去设置审批（做完后提议记忆的产品闭环）。
  // 切走了就不往当前会话塞这条提示 —— 它属于刚才那轮。
  if (runSid !== currentSessionId) return;
  const pendingAfter = await invoke<Candidate[]>("mem_pending").catch(
    () => [] as Candidate[],
  );
  if (pendingAfter.length > pendingBefore.length) {
    const n = pendingAfter.length - pendingBefore.length;
    // ⚠️ 这里**不**再自己贴底：`pushEntry` 内部已经按"用户在底部才跟随"的规矩
    //    处理过了（会保住位置 + 给「↓」加计数）。在外面再无条件
    //    `repaintChatToBottom()` 就等于把「不抢滚动」这个约定又破了 ——
    //    用户正翻历史，看到的却是"被拽到底 + 数字在跳"。
    pushEntry("system", `有 ${n} 条记忆候选待审批（设置 › 记忆，批了才会进长期记忆）`);
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

  // 内核分流：pi 内核走 `chat_pi_steer`（参数名不同：`message` 而非 `input`），
  // 自研内核走 `chat_steer`。返回结构已对齐（`SteerAck`），渲染逻辑不变。
  const usePi = usePiKernel && piKernelAvailable;

  // ⚠️ pi 内核的 `chat_pi_steer` **不接收图片**（参数只有 message / session_id；
  //    对比 `chat_pi` 是带 images 的）。所以 pi 下「插话 + 图」送不过去。
  //    这里必须**照实说**：图不上气泡（不然用户以为图排上了，其实没送出去），
  //    图留在附件区等下一轮一起发，并提示一句。
  //    （pi 侧若后续补上 images 参数，这里改回直接透传即可。）
  const piDropsImages = usePi && imgs.length > 0;
  if (piDropsImages) {
    if (!text) {
      // 只有图没文字 ⇒ pi 侧必然回「插话内容为空」，别去撞这堵墙
      showToast("pi 内核的插话暂不支持带图，请先发一轮或切回自研内核", "info");
      return;
    }
    showToast("pi 内核的插话暂不支持带图，图留在附件里等下一轮", "info");
  }

  try {
    const ack = usePi
      ? await invoke<{ id: string; queued: number }>("chat_pi_steer", {
          message: text,
          sessionId: currentSessionId,
        })
      : await invoke<{ id: string; queued: number }>("chat_steer", {
          input: text,
          images: imgs,
          // 多会话并行 run：插话必须路由到**当前会话**那个 run 的收件箱
          sessionId: currentSessionId,
        });
    // 已经送出去了才清输入框
    input.value = "";
    input.style.height = "auto";
    draftInput = "";
    // pi 下图片没送出去 ⇒ 附件留着（下一轮随正文一起发）
    if (!piDropsImages) pendingImages = [];
    renderPreview();
    // 立刻上屏一条「排队中」的气泡：插到 running **之前**，不要永远垫底
    pushEntry("user", text || "（仅图片）", undefined, piDropsImages ? [] : imgs, {
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
// ---------------- 斜杠命令面板 ----------------

/**
 * 命令面板的浮层状态。
 *
 * 面板是**输入框的附属物** —— 打开/关闭由输入框内容决定：
 * - 内容以 `/` 开头 → 打开
 * - 内容里的 `/` 被删掉 → 关闭
 *
 * ⚠️ 只有**光标在开头**的 `/` 才触发。句中的 `/`（比如路径 `D:/workplace`）
 * 绝不能被当成命令 —— 那是这个项目最常见的误触场景。
 */
const cmdPanel: CommandPanelState = newPanelState();

/** 执行一条命令 —— 由面板回调 */
async function runCommand(act: string): Promise<void> {
  switch (act) {
    case "mem":
      await switchView("mem");
      return;
    case "model": {
      // 模型的「管理」在独立窗口，「选择」在面板 —— 这里只管选择。
      // 复用顶栏模型 chip 的逻辑：点开 `.mc-menu` 列表。
      const chip = document.getElementById("model-chip") as HTMLElement | null;
      chip?.click();
      return;
    }
    case "new":
      // 「＋」按钮按设计稿 03 从输入行删了 → 这里直接开菜单。
      // 菜单锚在命令面板按钮附近（原来是锚在「＋」按钮上）。
      {
        const anchor = document.getElementById("btn-commands") ?? document.getElementById("input");
        const r = anchor?.getBoundingClientRect();
        void showNewMenu(r?.left ?? 10, (r?.bottom ?? 40) + 4);
      }
      return;
    case "sessions":
      await switchView("sessions");
      return;
    case "screen":
      await grabScreen();
      return;
    case "tools":
      await switchView("mcp");
      return;
    case "settings":
      await switchView("settings");
      return;
    case "project":
      await switchView("project");
      return;
    case "search":
      await switchView("search");
      return;
    case "compact":
      try {
        const r = await invoke<{ before: number; after: number }>("session_compact", {
          force: true,
        });
        showToast(
          `上下文已压缩：${r.before} → ${r.after} 字`,
          "ok",
          2600,
        );
        renderBody();
      } catch (e) {
        showToast(`压缩失败：${e}`, "error");
      }
      return;
    case "stop":
      await stopRun();
      return;
    case "clear":
      try {
        await invoke("session_truncate", { upto: 0 });
        draftInput = "";
        showToast("已清空当前会话", "ok", 2200);
        renderBody();
      } catch (e) {
        showToast(`清空失败：${e}`, "error");
      }
      return;
    default:
      showToast(`未知命令：${act}`, "error");
  }
}

/**
 * 输入框内容变了 → 决定面板开不开。
 *
 * 判定：内容**以 `/` 开头**且长度 > 1（光标不在那一个斜杠上）。
 * 这时候面板里的命令**替换**输入框内容 —— 选中一条就把它当消息发出去。
 */
function syncCommandPanel(input: HTMLTextAreaElement): void {
  const v = input.value;
  const shouldOpen = v.startsWith("/") && v.length > 1;

  if (!shouldOpen) {
    if (cmdPanel.el) closeCommandPanel(cmdPanel);
    return;
  }
  openCommandPanel(cmdPanel, v.slice(1), (act) => {
    void runCommand(act);
  });
}

/**
 * 面板里的 Enter：选中当前高亮的命令，**直接执行**。
 *
 * ⚠️ 命令是**动作**，不是消息（2026-10-23 修的真 bug）。
 * 原写法是把 `/新建会话` 填回输入框再 `send()` —— 结果模型收到一句
 * 「新建会话」，而真正的新建动作**根本没发生**；而且原注释说
 * 「用户还能在发送前改」，可 Enter 紧接着就 send() 了，根本没有改的机会。
 * 现在：Enter = 执行该命令；用户打的那段 `/xxx` 一起吃掉（命令不是消息）。
 */
function runSelectedCommand(input: HTMLTextAreaElement): boolean {
  const cmd = currentCommand(cmdPanel);
  if (!cmd) return false;
  closeCommandPanel(cmdPanel);
  // 把 "/xxx" 清掉 —— 执行的是命令，不是这句话
  input.value = "";
  input.style.height = "auto";
  draftInput = "";
  void runCommand(cmd.act);
  return true;
}

async function stopRun(): Promise<void> {
  const btn = document.getElementById("btn-stop") as HTMLButtonElement | null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "…";
    btn.title = "正在停止…";
  }
  await invoke("chat_cancel", { sessionId: currentSessionId }).catch(() => {});
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
    // ⚠️ 这两句是 placeholder 的**真正来源**（运行时覆盖 HTML 里那份）。
    //    刻意保持极短：老文案 38~44 字（约 470px @13px）在输入框里会折成两行、
    //    把输入框撑高 —— 那正是曾截图报"太挤"的直接症状。
    //    快捷键说明在 `title` 里（悬停可看），不占输入框宽度。
    input.placeholder = on ? "插话…（会排队）" : "问点什么…";
    input.title = on
      ? "当前轮进行中：发出去会排队，等本轮结束后送达。Enter 发送 / Shift+Enter 换行"
      : "Enter 发送 / Shift+Enter 换行 / Ctrl+V 贴图";
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
  const atBottom = isChatAtBottom();
  try {
    const st = await invoke<SessionState>("session_state");
    const fresh = sessionToEntries(st.session);
    if (fresh.length < expect) {
      console.warn(
        `[orbcat] 会话重载后发现磁盘更旧（${fresh.length} < ${expect}），保留内存内容`,
      );
      return;
    }
    currentSessionId = st.session.id;
    sessionList = st.list;
    entries = fresh;
    // ⚠️ 只有停在对话页才重绘：一轮跑完时用户可能正开着设置页，
    //    这里不设防就会把设置页整块换成聊天记录（输入框还是隐藏的，页面像坏了）
    if (bd && view === "chat") {
      // ⚠️ 不在底部时**必须保位置**，不能只是"不滚"：
      //    `innerHTML=` 会把 scrollTop 清 0（还会把 `chatScrollMemo` 写成 0），
      //    于是"往上翻历史"在一轮跑完后会变成"被拽回顶部 + 下次也回顶部"。
      if (atBottom) {
        repaintChatToBottom();
      } else {
        // 这一轮刚跑完 = 用户翻着的这段历史**后面**多了一条回答。
        // 这是「新消息到达」最常见的形态，计数要在这儿加 —— pushEntry 那条
        // 路只覆盖系统提示类消息，覆盖不到助手回答本身。
        repaintChatKeepScroll();
        chatMissed += 1;
        syncJumpButton();
      }
    }
  } catch (e) {
    console.warn(`[orbcat] 重载会话失败：${e}`);
  }
}

// ---------------- 形态切换 ----------------

async function applyMode(next: Mode): Promise<void> {
  // ⚠️ 不要用 busy 挡形态切换：收起面板是纯 UI 动作，
  //    请求会继续在后台跑，结果照常写进 entries，下次展开就能看到。
  //    （早先这里 `if (busy) return` 导致"请求中收不起面板"）
  if (next === mode) return;

  // ⚠️🔴 切换期间必须**先写 `mode` 再 await**（下面两个分支都这么做）。
  //
  //    踩过的坑（症状：右键球 → 点记忆 → 面板缩成一小块且回不去）：
  //    panel 分支原先是 `await invoke(...)` 之后才 `mode = next`，
  //    而 orb/menu 分支是先赋值。于是点「记忆」时 `mode` 在整个 IPC 往返里
  //    **一直是旧的 "menu"**；`set_bounds` 把窗口从 196×214 拉到 460×600
  //    会触发一次激活状态变化 → WebView 抛 `blur` → blur 处理器看见
  //    `mode === "menu"` 就调 `applyMode("orb")` 把窗口收回球大小。
  //    外层 await 回来后又把 `mode` 写成 "panel" —— **`mode` 说面板、
  //    窗口实际是球**。再点球时 `if (next === mode) return` 直接返回，
  //    于是再也回不去。
  //
  //    下面 `modeSwitching` 是第二道保险：自己触发的 resize 引来的 blur
  //    一律不处理，避免再出现"半路被自己的 blur 打断"。
  modeSwitching = true;

  // ⚠️ 顺序铁律（收起/切菜单）：必须**先换 DOM，再缩窗口**。
  //    `set_window_mode` 是一次 IPC 往返 —— 窗口在 Rust 侧当场就缩好了，
  //    而 renderOrb 要等 promise 回来才执行。复杂任务刚跑完时主线程正忙
  //    （paintMessages 重绘大消息列表），这个空档能拉长到几十毫秒，
  //    合成器就会画出"小窗口里残留着面板"的帧：屏幕上是一个深色圆角块，
  //    里面是被裁掉的面板标题/图表（曾截图报过，且只在
  //    复杂任务后出现 —— 就是这个竞态被主线程繁忙放大了）。
  //    球/菜单的 DOM 都是同步就绪的：先换 DOM 再缩窗，窗口缩的时候
  //    球已经在了，没有空档。过渡期 visibility:hidden 兜底 ——
  //    orb→menu 是反向放大，先换 DOM 会闪一帧"被裁的菜单"，藏一下就没有。
  if (next === "orb" || next === "menu") {
    app.style.visibility = "hidden";
    try {
      if (next === "menu") {
        renderMenu();
      } else {
        // 收起时把视图重置回对话：
        // 否则下次左键展开会停在上次的「设置」或「记忆」页 ——
        // 左键的语义就是「进来聊天」，要视图选择请走右键菜单。
        view = "chat";
        renderOrb();
      }
      mode = next;
      await invoke("set_window_mode", { mode: next });
    } catch (e) {
      console.error("[orbcat] 切换窗口形态失败:", e);
    } finally {
      app.style.visibility = "";
      modeSwitching = false;
    }
    if (next === "orb") void refreshPendingApprovals(); // 异步补正金色，不等它
    return;
  }

  // 展开成面板：窗口先放大再拉数据 —— renderPanel 依赖会话/模型数据，
  // 换不成"DOM 先行"；旧 DOM 是球，窗口透明，放大空档无残影。
  // 🔴 `mode = next` 必须在 await **之前**（与上面的 orb/menu 分支一致）——
  //    理由见函数开头那段注释：await 期间 mode 若是旧值，自己 resize 引来的
  //    blur 会把它当成"还在菜单态"而把窗口收回球大小。
  mode = next;
  try {
    await invoke("set_window_mode", { mode: next });
  } catch (e) {
    // 窗口没改成不算失败内容，但 `mode` 已经写了 —— 立刻写回去，
    // 否则又落进"mode 与窗口实际尺寸不一致"那个不可恢复的状态。
    mode = next === "panel" ? "menu" : "orb";
    console.error("[orbcat] 展开面板失败:", e);
    modeSwitching = false;
    return;
  }
  modeSwitching = false;

  // 打开面板 = 必然看到对话流 → 红球使命结束
  unseenError = false;
  panelOpenedAt = Date.now();
  await ensureSessionLoaded(); // 会话从磁盘恢复（只做一次）
  await loadModels();
  await loadProjects();
  // ⚠️ 不要 bindCurrentSession —— 切/开面板改挂靠是主聊天被塞进 redis 的根因
  await refreshSessionProjectTags();
  renderPanel();
}

/** 左键点球：进对话（不管上次停在哪一页） */
async function expand(): Promise<void> {
  view = "chat";
  await applyMode("panel");
}

/**
 * 失焦收起 —— 但「正在截图」时**不许收**。
 *
 * ## 为什么
 *
 * 拿微信 `Alt+A`（或系统截图工具）截面板内容时，**blur 一定先于截图发生**：
 * 面板一失焦，原来的 `collapse()` 就把它缩成球 —— 截到的是个球，
 * 想截的长内容根本够不着。典型现象："微信长截图 orbcat，orbcat 会缩成球导致无法截屏"。
 *
 * ## 判定方式（硬判断，不靠提示词）
 *
 * 问后端「当前前台窗口的进程名是不是已知截图/录屏工具」——
 * 白名单在 `context.rs::CAPTURE_TOOL_STEMS`（微信 / QQ / snippingtool / Snipaste …）。
 * 命中 → 跳过这次收起。
 *
 * ## 为什么要判两次
 *
 * blur 触发的瞬间新前台窗口**可能还没切过去**（微信是先激活自己的 overlay
 * 再取画面），此刻采到的仍是切换前的窗口。所以 150ms 后再问一次，
 * 两次都不命中才收 —— 代价是收起晚 150ms，肉眼无感。
 */
async function collapseUnlessCapturing(): Promise<void> {
  if (await foregroundIsCaptureTool()) return;
  await new Promise((r) => setTimeout(r, 150));
  if (await foregroundIsCaptureTool()) return;
  // 等待期间可能已自己收起 / 切了形态 —— 别重复动手
  if (mode !== "panel" || !blurCollapse) return;
  void collapse();
}

/**
 * 问后端：当前前台是不是截图/录屏工具。
 *
 * 命令不可用（老构建 / 调用异常）时**退回旧行为**（返回 false → 照常收起），
 * 绝不让一个诊断性判定把"失焦让路"这个本分功能卡死。
 */
async function foregroundIsCaptureTool(): Promise<boolean> {
  try {
    return await invoke<boolean>("foreground_is_capture_tool");
  } catch {
    return false;
  }
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
      <button data-act="sleep"><span>💤</span>睡眠</button>
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

      // 睡眠：先置位再收起 —— `sleepOrb()` 里的 renderOrb() 就会
      // 直接画出睡眠态，不会闪一帧"醒着的球"再变暗
      if (act === "sleep") {
        await sleepOrb();
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
    // ⚠️ 不要用 `busy` 挡住球态菜单：它只是导航（设置/记忆/对话/退出），
    //    跑着任务时更要能进去看设置、或去别的会话开新任务（并行 run）。
    // 只有胶囊态需要"弹菜单"；面板态内右键直接忽略（已经在面板里了）
    if (mode === "orb") {
      await applyMode("menu");
    }
  });

  // 点菜单外的任何地方 → 关掉（回胶囊 / 关复制菜单）
  document.addEventListener("mousedown", (e) => {
    const t = e.target as HTMLElement;
    if (copyMenuEl && !t.closest(".copy-menu")) closeCopyMenu();
    if (modeSwitching) return; // 切换期间不判定"点外面了"（同 blur 那条理由）
    if (mode !== "menu") return;
    if (!t.closest(".ctx-card")) void applyMode("orb");
  });

  window.addEventListener("blur", () => {
    // ⚠️🔴 自己触发的形态切换会引来一次 blur（见 applyMode 的注释）。
    //    切换期间**必须放行**，否则面板在半路被收回球大小，且 mode 与窗口
    //    实际尺寸就此不一致 → 之后点球也回不去（曾因此报过 bug）。
    if (modeSwitching) return;
    if (mode === "menu") void applyMode("orb");
    // 面板失焦自动收起 —— 悬浮球的本分是用完让路，不该一直挡着屏幕。
    // 展开后 500ms 内的 blur 忽略（展开动画/set_focus 时序抖动）。
    //
    // 不再直接 `collapse()`，改成**先问一句新前台是不是截图工具**。
    // 拿微信/系统截图工具截面板时 blur 会先发生，原来那一下直接把面板
    // 缩成球 → 截图里只剩一个球（"微信长截图 orbcat，会缩成球导致无法截屏"）。
    if (mode === "panel" && blurCollapse && Date.now() - panelOpenedAt > 500) {
      void collapseUnlessCapturing();
    }
  });
}

// ---------------- 启动 ----------------

function boot(): void {
  mode = "orb";
  renderOrb();

  // 诊断打点：主窗口前端启动 + 未捕获异常都落盘（排查窗口问题时唯一的可靠通道）
  void invoke("diag_log", { msg: "main.ts boot: 开始" }).catch(() => {});
  window.addEventListener("error", (e) => {
    void invoke("diag_log", {
      msg: `main.ts window.onerror: ${e.message} @ ${e.filename}:${e.lineno}`,
    }).catch(() => {});
  });
  window.addEventListener("unhandledrejection", (e) => {
    void invoke("diag_log", { msg: `main.ts unhandledrejection: ${String(e.reason)}` }).catch(
      () => {},
    );
  });

  // 双保险：Rust 侧 setup 已经应用过一次，这里再确认一次
  void invoke("set_window_mode", { mode: "orb" }).catch((e) =>
    console.error("[orbcat] 初始化形态失败:", e),
  );

  installPasteHandler();
  installDropHandler();
  installFocusRefocus();
  installContextMenu();
  installCopyHandlers();
  installTimelineMore();
  installForkHandlers();
  installDeleteHandlers();
  installEditHandlers();
  installRegenHandlers();
  installProgressListener();
  installModelsListener();

  // 查 pi 内核是否可用（内置 runtime 在不在）。
  // 异步 —— 不阻塞首屏。查到后 chip 才会出现。
  void refreshPiStatus();

  // 点面板空白处收起模型列表（点 chip 本身或列表内部不收 —— chip 自己管开关）
  document.addEventListener("mousedown", (e) => {
    if (!modelsDropdownOpen) return;
    const t = e.target as HTMLElement;
    if (t.closest("#model-chip")) return;
    modelsDropdownOpen = false;
    paintModelsDropdown();
  });
  installPermHandlers();
  installOrbUiListener();
  installOpenPanelListener();
  startPermTimer();

  // 待拍板条数（记忆候选 / 权限申请）：启动拉一次 + 每 5s 轮询。
  // 读的是本地小 JSONL，成本可忽略；这是胶囊态"金色 = 有事情等你拍板"的唯一数据源。
  //
  // ⚠️ 与 `permTimer` 同一个理由：句柄必须存下来并重入前清掉。
  // 裸 `setInterval` 会在测试里留下环境拆掉后才触发的定时器
  // （→ `document is not defined` 未捕获异常）。
  void refreshPendingApprovals();
  if (pendingPollTimer !== null) window.clearInterval(pendingPollTimer);
  pendingPollTimer = window.setInterval(() => {
    // 同 `startPermTimer` 的自杀闸：环境拆掉后不再跑。
    // 注意用裸 `clearInterval`（`window` 那时也没了）。
    if (typeof document === "undefined" || !document.body) {
      if (pendingPollTimer !== null) clearInterval(pendingPollTimer);
      pendingPollTimer = null;
      return;
    }
    void refreshPendingApprovals();
  }, 5000);

  // 执行权限档位：启动拉一次（chip 默认 ask，拉到什么显什么）
  void invoke<{ execTrust?: string }>("get_settings")
    .then((s) => {
      if (s?.execTrust) {
        execTrust = s.execTrust as typeof execTrust;
      }
    })
    .catch(() => {});

  // Agent 模式：启动拉一次（chip 默认「标准」，拉到什么显什么）
  void loadModes();
  // 后台执行状态：启动拉一次
  void loadBgDesk();

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
    // ▾ 模型下拉优先吃掉 Esc（管理弹窗都搬去独立窗口了，这里只剩它一层）
    if (modelsDropdownOpen) {
      e.preventDefault();
      modelsDropdownOpen = false;
      paintModelsDropdown();
      return;
    }
    if (copyMenuEl) {
      closeCopyMenu();
      return;
    }
    if (mode === "menu") {
      void applyMode("orb");
    } else if (mode === "panel" && !busy) {
      // 逐级返回：子页 → 设置入口 → 对话 → 收起面板
      if (view === "mcp" || view === "perm" || view === "cmdpolicy" || view === "search") {
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
