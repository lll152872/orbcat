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

interface ModelView {
  id: string;
  name: string;
  vendor: string;
  url: string;
  supportsToolCall: boolean;
  supportsImages: boolean;
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
}

interface ChatEntry {
  role: "user" | "assistant" | "error" | "system" | "running";
  text: string;
  steps?: AgentStep[];
  images?: string[];
  /** role === "running" 时的实时进度行（工具调用等） */
  progress?: string[];
  /** 流式正文（边收边显示） */
  streamText?: string;
  /** 流式思维链 */
  streamReasoning?: string;
  /** 该轮流出的正文作废（模型中途又去调工具了） */
  streamDiscarded?: boolean;
}

// ---------------- 会话持久化（类型与后端 sessions.rs 对应） ----------------

interface StoredMsg {
  role: "user" | "assistant";
  text: string;
  images?: string[];
  steps?: AgentStep[];
  at: number;
}

interface Session {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  messages: StoredMsg[];
  kind: "main" | "task";
}

interface SessionMeta {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  count: number;
  current: boolean;
  /** 主会话固定在列表置顶且不可删；其余都是任务会话 */
  kind: "main" | "task";
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

/** 把磁盘上的会话消息转成面板的 entries */
function sessionToEntries(s: Session): ChatEntry[] {
  return s.messages
    .filter((m) => m.text || (m.images && m.images.length))
    .map((m) => ({
      role: m.role,
      text: m.text,
      steps: m.steps,
      images: m.images,
    }));
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

/** 开新会话（旧会话留在「历史」里） */
async function newSession(): Promise<void> {
  if (busy) return;
  try {
    const s = await invoke<Session>("session_new");
    currentSessionId = s.id;
    entries = [];
    await refreshSessionList();
    view = "chat";
    renderBody();
  } catch (e) {
    pushEntry("error", `新建会话失败：${e}`);
  }
}

async function switchSession(id: string): Promise<void> {
  if (busy) return;
  try {
    const s = await invoke<Session>("session_switch", { id });
    currentSessionId = s.id;
    entries = sessionToEntries(s);
    await refreshSessionList();
    view = "chat";
    renderBody();
  } catch (e) {
    pushEntry("error", `切换会话失败：${e}`);
  }
}

async function deleteSession(id: string): Promise<void> {
  if (busy) return;
  try {
    await invoke("session_delete", { id });
    await refreshSessionList();
    // 删的可能是当前会话 → 后端已自动切到别的，这里重新对齐
    const st = await invoke<SessionState>("session_state");
    currentSessionId = st.session.id;
    sessionList = st.list;
    entries = sessionToEntries(st.session);
    renderBody();
  } catch (e) {
    pushEntry("error", `删除会话失败：${e}`);
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
function renderSessionsView(): string {
  const main = sessionList.find((s) => s.kind === "main");
  const tasks = sessionList.filter((s) => s.kind !== "main");

  // 主聊天卡片：无删除按钮，标题固定
  const mainHtml = main
    ? `
      <button class="sess-main${main.current ? " cur" : ""}" data-id="${esc(main.id)}" title="切回主聊天">
        <span class="sess-main-icon">👤</span>
        <span class="sess-main-text">
          <b>主聊天</b>
          <i>${fmtTime(main.updatedAt)} · ${main.count} 条 · 永久保留</i>
        </span>
        ${main.current ? '<span class="sess-main-cur">当前</span>' : '<span class="set-entry-arrow">›</span>'}
      </button>`
    : "";

  const taskRows =
    tasks.length === 0
      ? `<div class="mem-empty">还没有任务会话。点右上「＋」开一个，或直接把活丢给它。</div>`
      : tasks
          .map(
            (s) => `
      <div class="sess-item${s.current ? " cur" : ""}">
        <button class="sess-open" data-id="${esc(s.id)}" title="切换到该会话">
          <span class="sess-title">${esc(s.title)}${s.current ? " · 当前" : ""}</span>
          <span class="sess-meta">${fmtTime(s.updatedAt)} · ${s.count} 条</span>
        </button>
        <button class="sess-del" data-id="${esc(s.id)}" title="删除该会话">✕</button>
      </div>`,
          )
          .join("");

  return `
    <div class="mem-head">会话<button class="mem-back" id="sess-new">＋ 新建</button></div>
    <div class="set-hint" style="margin-bottom:8px">
      对话自动存到 <code>agent-data/sessions/</code>。主聊天永久保留，任务会话可删。
    </div>
    ${mainHtml}
    <div class="sess-sep">任务会话 ${tasks.length} 个</div>
    ${taskRows}`;
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
}

/** Rust 侧推上来的进度事件 */
interface ProgressEvent {
  kind: "thinking" | "delta" | "toolCall" | "toolResult" | "toolError" | "answering" | "discardStream";
  iteration?: number;
  /** delta */
  text?: string;
  reasoning?: string;
  /** tool* */
  name?: string;
  args?: string;
  preview?: string;
  message?: string;
  /** discardStream */
  reason?: string;
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
  activeTokens: number;
  budgetTokens: number;
  groups: McpGroupView[];
}

// ---------------- 状态 ----------------

let mode: Mode = "orb";
let busy = false;
let thinking = false;

const app = document.getElementById("app")!;

let models: ModelView[] = [];
let selectedModel: string | null = null;
let entries: ChatEntry[] = [];

/** 待发送的图片（data URL） */
let pendingImages: string[] = [];

/**
 * 内容区视图（两级导航）：
 *   settings ─┬─ models    模型管理
 *             ├─ mcp       MCP 工具组
 *             ├─ perm      文件权限
 *             └─ mem       记忆
 */
type View = "chat" | "settings" | "models" | "mcp" | "perm" | "mem" | "sessions";

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
  // 猫球三帧：sleep=待命（慢呼吸），awake-a/b=工作中（交替=摇尾巴）
  // 素材是圆形透明 PNG（public/orb/），窗口 transparent 圆外自然露出桌面
  app.innerHTML = `
    <div class="orb${thinking ? " thinking" : ""}" id="orb" title="float-agent — 点击展开">
      <img class="orb-frame orb-frame-sleep"   src="orb/orb-sleep.png"   alt="" draggable="false" />
      <img class="orb-frame orb-frame-awake-a" src="orb/orb-awake-a.png" alt="" draggable="false" />
      <img class="orb-frame orb-frame-awake-b" src="orb/orb-awake-b.png" alt="" draggable="false" />
    </div>
  `;
  const orb = document.getElementById("orb")!;
  attachDrag(orb, () => void expand());
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

/** 同步取缓存；没有则返回 null 并异步加载（加载完重绘消息区） */
function thumbOf(path: string): string | null {
  const hit = thumbCache.get(path);
  if (hit) return hit;
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
        const b = document.getElementById("panel-body");
        if (b && view === "chat") b.innerHTML = renderMessages();
      });
  }
  return null;
}


// ---------------- 渲染：面板态 ----------------

/**
 * 工具调用步骤。
 *
 * 两种形态要区分开（后端 `sessions::fold_steps` 落盘时会把 steps 压成一行）：
 * - **当轮的实时步骤** —— `detail` 是参数 JSON，逐个列出名字 + 参数摘要
 * - **历史会话里的步骤** —— 已折叠，`detail` 就是 `[已执行：...]` 摘要行，
 *   直接整行显示，**不能**再当 JSON 解析（会显示成乱码）
 */
function renderSteps(steps: AgentStep[]): string {
  if (!steps || steps.length === 0) return "";
  const calls = steps.filter((s) => s.kind === "tool_call");
  if (calls.length === 0) return "";

  // 折叠记录：单条 tool_call，detail 以 [已执行 开头
  const folded =
    calls.length === 1 && calls[0].detail.trimStart().startsWith("[已执行");
  if (folded) {
    return `<div class="steps-folded">${esc(calls[0].detail)}</div>`;
  }

  const items = calls
    .map((s) => {
      let arg = s.detail;
      try {
        const o = JSON.parse(s.detail);
        arg = o.path || o.pattern || (Object.keys(o).length ? s.detail : "—");
      } catch {
        /* 保持原样 */
      }
      return `<li><code>${esc(s.name || "?")}</code> ${esc(String(arg).slice(0, 90))}</li>`;
    })
    .join("");
  return `<details class="steps"><summary>调用了 ${calls.length} 个工具</summary><ul>${items}</ul></details>`;
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

function renderMessages(): string {
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
      if (e.role === "running") {
        const lines = (e.progress ?? []).map((l) => `<li>${l}</li>`).join("");

        const reasoning = e.streamReasoning
          ? `<details class="run-reason"><summary>💭 思考过程</summary><pre>${esc(
              e.streamReasoning,
            )}</pre></details>`
          : "";

        const body = e.streamText
          ? `<div class="run-stream${e.streamDiscarded ? " discarded" : ""}">${esc(
              e.streamText,
            ).replace(/\n/g, "<br>")}</div>`
          : "";

        const note = e.streamDiscarded
          ? `<div class="run-note">（模型中途决定调用工具，上面这段不是最终答复）</div>`
          : "";

        return `
        <div class="msg running">
          <div class="run-head"><span class="spinner"></span>${esc(e.text)}</div>
          ${reasoning}
          ${lines ? `<ul class="run-list">${lines}</ul>` : ""}
          ${body}
          ${note}
        </div>`;
      }

      const cls = e.role;
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
      return `
      <div class="msg ${cls}">
        ${imgs}
        ${body ? `<div class="msg-body${isMd ? " md" : ""}">${body}</div>` : ""}
        ${
          e.text
            ? `<div class="msg-foot"><button type="button" class="msg-copy" data-idx="${idx}" title="复制这条消息">复制</button></div>`
            : ""
        }
        ${e.steps ? renderSteps(e.steps) : ""}
      </div>`;
    })
    .join("");
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
        `<option value="${esc(m.id)}"${m.id === selectedModel ? " selected" : ""}>${esc(
          m.name,
        )}${m.vendor && m.vendor !== "Custom" ? ` · ${esc(m.vendor)}` : ""}${
          m.supportsImages ? " 🖼" : ""
        }${m.supportsToolCall ? " 🔧" : ""}</option>`,
    )
    .join("");

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
        <button class="mini-btn" id="btn-grab" title="截取当前屏幕">截</button>
        <button class="mini-btn" id="btn-new" title="新建任务会话">＋</button>
        <button class="mini-btn" id="btn-sess" title="会话（主聊天 / 任务）">史</button>
      </div>

      <div class="fg-ctx" id="fg-ctx" title="用户当前前台应用（点击复制路径）" style="display:none"></div>

      <div class="panel-body" id="panel-body"></div>

      <div class="thumbs" id="thumbs"></div>
      <div class="img-hint" id="img-hint" style="display:none"></div>

      <div class="panel-input">
        <textarea id="input" rows="1" placeholder="问点什么…（Enter 发送 / Shift+Enter 换行 / Ctrl+V 贴图）"></textarea>
        <button id="btn-send" title="发送">↵</button>
      </div>
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
  document.getElementById("btn-new")!.addEventListener("click", () => void newSession());
  document.getElementById("btn-sess")!.addEventListener("click", () => void switchView("sessions"));

  // 当前会话标题挂到「史」按钮的 tooltip 上，一眼知道在哪个会话里
  const curSess = sessionList.find((s) => s.id === currentSessionId);
  const sessBtn = document.getElementById("btn-sess");
  if (sessBtn && curSess) {
    const tag = curSess.kind === "main" ? "主聊天" : "任务";
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

  const input = document.getElementById("input") as HTMLTextAreaElement;
  input?.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  });
  input?.addEventListener("input", () => {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 120) + "px";
  });

  document.getElementById("btn-send")!.addEventListener("click", () => {
    // 忙碌中 → 停止；空闲 → 发送。同一颗按钮，两个职责。
    if (busy) {
      void invoke("chat_cancel").catch(() => {});
      // 立即给视觉反馈：否则用户以为没点到，会反复点
      const b = document.getElementById("btn-send") as HTMLButtonElement | null;
      if (b) {
        b.disabled = true;
        b.textContent = "…";
        b.title = "正在停止…";
      }
      const last = entries[entries.length - 1];
      if (last && last.role === "running") {
        last.text = "正在停止…";
        const bd = document.getElementById("panel-body");
        if (bd) {
          bd.innerHTML = renderMessages();
          bd.scrollTop = bd.scrollHeight;
        }
      }
      return;
    }
    void send();
  });

  renderPreview();
  renderBody();
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
const FG_SHOW_MAX = 4;
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
      el.innerHTML = items
        .map((c, i) => {
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
            <span class="fg-eye">${dot}</span>${esc(label)}</div>`;
        })
        .join("");
      el.style.display = "block";
      el.querySelectorAll<HTMLElement>(".fg-row").forEach((r) =>
        r.addEventListener("click", () => {
          const p = r.dataset.path || "";
          if (p) void navigator.clipboard.writeText(p).catch(() => {});
        }),
      );
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
    ]).then(([list, files, sk]) => {
      if (view !== "mem") return;
      b.innerHTML = renderMemView(list, files, sk);
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
    ])
      .then(([mcp, pending, autoOn, rules, settings]) => {
        if (view !== "settings") return;
        autostartOn = autoOn;
        blurCollapse = settings?.blurCollapse ?? true;
        permCount = rules.length;
        b.innerHTML = renderSettingsMenu(mcp, pending.length);
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

  if (view === "mcp") {
    const keep = b.scrollTop;
    b.innerHTML = `<div class="mem-loading">加载中…</div>`;
    void invoke<McpStatus>("mcp_status")
      .then((st) => {
        if (view !== "mcp") return;
        b.innerHTML = renderMcpView(st);
        b.scrollTop = keep;
        bindMcpView(b);
      })
      .catch(() => {
        if (view !== "mcp") return;
        b.innerHTML = renderMcpView(null);
        b.scrollTop = keep;
        bindMcpView(b);
      });
    return;
  }

  b.innerHTML = renderMessages();
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

function renderMemView(list: Candidate[], files: MemoryFiles | null, skills: SkillInfo[]): string {
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
    <div class="mem-head">技能 ${skills.length} 个</div>
    <div class="set-hint" style="margin-bottom:8px">
      模型按清单匹配任务，用 <code>load_skill</code> 读全文照做；教会它新流程可用 <code>save_skill</code> 保存。
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
  view = v === view && v !== "chat" ? "chat" : v;
  renderBody();
}

// ---------------- 设置：入口页 ----------------

/**
 * 设置入口页 —— **只列入口和状态摘要**，内容点进去再看。
 * （早先把模型/MCP/记忆全平铺在一页，滚半天找不到东西）
 */
function renderSettingsMenu(mcp: McpStatus | null, pendingCount: number): string {
  const curModel =
    models.find((m) => m.id === selectedModel)?.name ?? "（未选）";

  const mcpSummary = mcp
    ? mcp.connected
      ? `${mcp.groups.length} 组 / ${mcp.groups.reduce((a, g) => a + g.toolCount, 0)} 个工具` +
        ` · 已加载 ${mcp.groups.filter((g) => g.active).length}`
      : "未连接"
    : "读取中…";

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
    ${item("mcp", "🔌", "MCP 外部工具", mcpSummary)}
    ${item("perm", "📁", "文件权限", `已配置 ${permCount} 条规则`)}
    ${item("mem", "🧠", "记忆", "待审批与长期记忆", pendingCount > 0 ? String(pendingCount) : undefined)}

    <div class="mem-head">启动与退出</div>
    <label class="set-check set-check-row">
      <input type="checkbox" id="auto-start" ${autostartOn ? "checked" : ""} />
      开机自动启动（写 HKCU\\...\\Run 注册表项，release 版无黑框）
    </label>
    <label class="set-check set-check-row">
      <input type="checkbox" id="blur-collapse" ${blurCollapse ? "checked" : ""} />
      面板失焦自动收起（点别处就缩回球，不挡屏幕）
    </label>
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

// ---------------- 设置 › 模型 ----------------

function renderModelsView(): string {
  const rows = models
    .map((m) => {
      const cur = m.id === selectedModel;
      return `
      <div class="set-row${cur ? " cur" : ""}">
        <div class="set-name">${esc(m.name)}${cur ? ' <span class="set-cur">当前</span>' : ""}</div>
        <div class="set-url">${esc(m.url)} · ${m.supportsImages ? "🖼" : ""}${
        m.supportsToolCall ? "🔧" : ""
      }</div>
        <div class="set-actions">
          ${cur ? "" : `<button class="set-btn use" data-id="${esc(m.id)}">使用</button>`}
          <button class="set-btn test" data-id="${esc(m.id)}">测</button>
          <button class="set-btn del" data-id="${esc(m.id)}">删</button>
        </div>
      </div>`;
    })
    .join("");

  return `
    ${subHeader("模型")}
    <div class="set-list">${rows || '<div class="mem-empty">还没有模型。</div>'}</div>

    <div class="mem-head">➕ 添加模型</div>
    <div class="set-form">
      <input id="f-name" placeholder="显示名（可留空，默认同 id）" />
      <input id="f-url" placeholder="Base URL，如 https://token.sensenova.cn/v1" />
      <input id="f-key" type="password" placeholder="API Key（只存本地 agent-data/models.json）" />
      <label class="set-check"><input type="checkbox" id="f-tool" checked /> 支持工具调用</label>
      <label class="set-check"><input type="checkbox" id="f-img" checked /> 支持图片</label>
      <textarea id="f-headers" rows="3"
        placeholder="额外请求头（可选，每行一个 Key: Value）&#10;例：x-opencode-session: 你的会话id"></textarea>
      <div class="set-hint">
        有些网关在标准协议外要求自定义头。<br>
        <code>opencode.ai/zen</code> 会自动补 <code>x-opencode-session</code>，不用手填。
      </div>
      <button class="set-btn add" id="f-add">添加</button>
      <div class="set-hint">⚠️ Key 只保存在本地 agent-data/models.json（已 gitignore），不会进仓库。</div>
    </div>`;
}

function bindModelsView(root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));

  root.querySelectorAll<HTMLButtonElement>(".set-btn.use").forEach((b) =>
    b.addEventListener("click", async () => {
      selectedModel = b.dataset.id!;
      await invoke("set_selected_model", { id: selectedModel }).catch((e) =>
        pushEntry("error", `保存失败：${e}`),
      );
      renderBody();
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".set-btn.del").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      await invoke("models_remove", { id }).catch((e) => pushEntry("error", `删除失败：${e}`));
      await loadModels();
      renderBody();
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".set-btn.test").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      pushEntry("system", `正在测试 ${id} …`);
      try {
        const reply = await invoke<string>("test_model", { id });
        pushEntry("system", `✅ ${id} 连通正常：${reply.slice(0, 150)}`);
      } catch (e) {
        pushEntry("error", `❌ ${id} 测试失败：${e}`);
      }
    }),
  );

  document.getElementById("f-add")?.addEventListener("click", async () => {
    const id = (document.getElementById("f-name") as HTMLInputElement).value.trim();
    const url = (document.getElementById("f-url") as HTMLInputElement).value.trim();
    const key = (document.getElementById("f-key") as HTMLInputElement).value.trim();
    const tool = (document.getElementById("f-tool") as HTMLInputElement).checked;
    const img = (document.getElementById("f-img") as HTMLInputElement).checked;
    const headers = (document.getElementById("f-headers") as HTMLTextAreaElement).value;

    if (!id || !url || !key) {
      pushEntry("error", "名称 / URL / Key 都不能为空");
      return;
    }

    try {
      await invoke("models_add", {
        id,
        name: id,
        url,
        apiKey: key,
        supportsToolCall: tool,
        supportsImages: img,
        headers: headers.trim() ? headers : null,
      });
      await loadModels();
      selectedModel = id;
      await invoke("set_selected_model", { id }).catch(() => {});
      renderBody();
    } catch (e) {
      pushEntry("error", `添加失败：${e}`);
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

// ---------------- 设置 › MCP ----------------

function renderMcpView(mcp: McpStatus | null): string {
  const head = subHeader("MCP 外部工具");

  if (!mcp) {
    return head + `<div class="mem-empty">读取 MCP 状态失败。</div>`;
  }

  // 网关地址编辑器 —— MCP 网关是用户自己起的进程，没有内置默认值，
  // 不配就不启用。这里给一个输入框：填了保存即生效，清空即禁用。
  const urlRow = `
    <div class="mcp-url-row">
      <label class="mcp-url-label" for="mcp-url">网关地址</label>
      <input class="mcp-url-input" id="mcp-url" type="text"
        placeholder="http://127.0.0.1:3050/mcp" value="${esc(mcp.url)}" spellcheck="false">
      <button class="set-btn" id="mcp-url-save">保存</button>
    </div>
    <div class="set-hint" id="mcp-url-msg" style="margin-bottom:8px">
      ${mcp.url ? "改动保存后立即重连。清空并保存 = 禁用 MCP。" : "未配置 —— 填一个 MCP 网关地址（如 1MCP）并保存才会启用。"}
    </div>`;

  if (!mcp.connected) {
    return (
      head +
      urlRow +
      `<div class="mem-empty">${mcp.url ? `未连接到 ${esc(mcp.url)}${mcp.error ? `：${esc(mcp.error)}` : ""}` : "还没配网关地址。"}${mcp.url ? "<br>检查网关进程是否在运行。" : ""}</div>
       <div class="set-actions"><button class="set-btn" id="mcp-refresh">重新连接</button></div>`
    );
  }

  const pct = Math.min(100, Math.round((mcp.activeTokens / mcp.budgetTokens) * 100));
  const bar = `
    <div class="mcp-budget">
      <div class="mcp-budget-bar"><i style="width:${pct}%"></i></div>
      <span>已加载工具占用 ~${mcp.activeTokens} / ${mcp.budgetTokens} tokens</span>
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

  return (
    head +
    urlRow +
    `<div class="set-url" style="margin-bottom:8px">已连接 ${esc(mcp.url)}</div>` +
    bar +
    `<div class="set-hint" style="margin-bottom:8px">
      这些工具**不会默认塞进模型上下文**（全量约 14k tokens）。
      模型会按需自己加载；你也可以在这里手动加载/卸载。
     </div>
     <div class="set-actions" style="margin-bottom:10px">
       <button class="set-btn" id="mcp-refresh">重新拉取工具清单</button>
     </div>` +
    `<div class="set-list">${rows}</div>`
  );
}

function bindMcpView(root: HTMLElement): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void switchView("settings"));

  document.getElementById("mcp-refresh")?.addEventListener("click", async () => {
    await invoke("mcp_refresh").catch((e) => pushEntry("error", `刷新失败：${e}`));
    renderBody();
  });

  // 网关地址：保存（含清空=禁用）→ Rust 写 mcp.json + 换 client + 立即试连
  const saveUrl = async () => {
    const input = document.getElementById("mcp-url") as HTMLInputElement | null;
    const msg = document.getElementById("mcp-url-msg");
    if (!input) return;
    try {
      if (msg) msg.textContent = "保存并连接中…";
      await invoke("mcp_set_url", { url: input.value });
      renderBody(); // 整页重绘 → 连接状态/组列表跟着变
    } catch (e) {
      if (msg) msg.textContent = `保存失败：${e}`;
    }
  };
  document.getElementById("mcp-url-save")?.addEventListener("click", () => void saveUrl());
  document.getElementById("mcp-url")?.addEventListener("keydown", (ev) => {
    if ((ev as KeyboardEvent).key === "Enter") {
      ev.preventDefault();
      void saveUrl();
    }
  });

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
}

function pushEntry(
  role: ChatEntry["role"],
  text: string,
  steps?: AgentStep[],
  images?: string[],
): void {
  entries.push({ role, text, steps, images });
  const b = document.getElementById("panel-body");
  if (b) {
    b.innerHTML = renderMessages();
    scrollToBottom();
  }
}

function setThinking(on: boolean): void {
  thinking = on;
  document.getElementById("orb")?.classList.toggle("thinking", on);
}

// ---------------- 实时进度 ----------------

/**
 * 接收 Rust 侧推上来的 agent 进度事件。
 *
 * 为什么需要：agent loop 一轮可能跑 30-60 秒（多次模型调用 + 工具执行），
 * 没有这个的话用户面对的是**纯黑盒空白**，然后答案突然砸出来。
 */
function installProgressListener(): void {
  void listen<ProgressEvent>("agent-progress", (e) => {
    const p = e.payload;
    const last = entries[entries.length - 1];
    if (!last || last.role !== "running") return;
    last.progress = last.progress ?? [];

    switch (p.kind) {
      case "thinking":
        if (p.iteration) last.text = `正在思考…（第 ${p.iteration} 轮）`;
        // 新一轮开始 → 清掉上一轮的流式残留
        last.streamText = undefined;
        last.streamReasoning = undefined;
        last.streamDiscarded = false;
        break;

      case "delta":
        if (p.reasoning) {
          last.streamReasoning = (last.streamReasoning ?? "") + p.reasoning;
        }
        if (p.text) {
          last.streamText = (last.streamText ?? "") + p.text;
        }
        break;

      case "discardStream":
        last.streamDiscarded = true;
        break;

      default: {
        const line = formatProgress(p);
        if (line) last.progress.push(line);
        break;
      }
    }

    // 只重绘消息区，不动输入框（避免打断用户输入）
    const b = document.getElementById("panel-body");
    if (b) {
      const atBottom = b.scrollHeight - b.scrollTop - b.clientHeight < 60;
      b.innerHTML = renderMessages();
      // 用户没往上翻时才自动跟到底部
      if (atBottom) b.scrollTop = b.scrollHeight;
    }
  });
}

function formatProgress(p: ProgressEvent): string | null {
  switch (p.kind) {
    case "toolCall":
      return `🔧 调用 <code>${esc(p.name ?? "?")}</code>${
        p.args ? ` <i>${esc(p.args)}</i>` : ""
      }`;
    case "toolResult": {
      const preview = (p.preview ?? "").split("\n")[0].slice(0, 70);
      return `✅ <code>${esc(p.name ?? "?")}</code> 返回 <i>${esc(preview)}</i>`;
    }
    case "toolError":
      return `⚠️ <code>${esc(p.name ?? "?")}</code> 失败：<i>${esc(p.message ?? "")}</i>`;
    case "answering":
      return `✍️ 已生成回答`;
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

async function send(): Promise<void> {
  if (busy) return;
  const input = document.getElementById("input") as HTMLTextAreaElement;
  const text = (input?.value ?? "").trim();
  const imgs = pendingImages.slice();

  // 允许"只发图不写字"
  if (!text && imgs.length === 0) return;

  if (!selectedModel) {
    pushEntry("error", "请先选择一个模型");
    return;
  }

  input.value = "";
  input.style.height = "auto";
  pendingImages = [];
  renderPreview();

  pushEntry("user", text || "（仅图片）", undefined, imgs);

  // 先放一个「进行中」气泡，进度事件会往里追加行；结束后替换成最终结果
  entries.push({ role: "running", text: "正在思考…", progress: [] });
  const b0 = document.getElementById("panel-body");
  if (b0) {
    b0.innerHTML = renderMessages();
    b0.scrollTop = b0.scrollHeight;
  }

  busy = true;
  setThinking(true);
  const btn = document.getElementById("btn-send") as HTMLButtonElement;
  if (btn) {
    // 忙碌时按钮变成「停止」：点它走取消通道，而不是禁用干等
    btn.disabled = false;
    btn.textContent = "■";
    btn.title = "停止生成";
  }

  try {
    const run = await invoke<AgentRun>("chat", { input: text, images: imgs });
    // 去掉 running 气泡，换成正式回答
    const idx = entries.findIndex((x) => x.role === "running");
    if (idx >= 0) entries.splice(idx, 1);
    pushEntry("assistant", run.answer, run.steps);
  } catch (e) {
    const idx = entries.findIndex((x) => x.role === "running");
    if (idx >= 0) entries.splice(idx, 1);
    const msg = String(e);
    if (msg.includes("已停止")) {
      pushEntry("system", "⏹ 已停止");
    } else {
      pushEntry("error", msg);
    }
  } finally {
    busy = false;
    setThinking(false);
    if (btn) {
      btn.disabled = false;
      btn.textContent = "↵";
      btn.title = "发送";
    }
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
    panelOpenedAt = Date.now();
    await ensureSessionLoaded(); // 会话从磁盘恢复（只做一次）
    await loadModels();
    renderPanel();
  } else if (next === "menu") {
    renderMenu();
  } else {
    // 收起时把视图重置回对话：
    // 否则下次左键展开会停在上次的「设置」或「记忆」页 ——
    // 左键的语义就是「进来聊天」，要视图选择请走右键菜单。
    view = "chat";
    renderOrb();
  }
}

/** 左键点球：进对话（不管上次停在哪一页） */
async function expand(): Promise<void> {
  view = "chat";
  await applyMode("panel");
}

async function collapse(): Promise<void> {
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
  installProgressListener();
  // 启动自检要先于会话预热：needsInit 决定了对话空态渲染成"引导语"还是"初始化提示"
  void checkBoot().then(() => ensureSessionLoaded());

  window.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    if (copyMenuEl) {
      closeCopyMenu();
      return;
    }
    if (mode === "menu") {
      void applyMode("orb");
    } else if (mode === "panel" && !busy) {
      // 逐级返回：子页 → 设置入口 → 对话 → 收起面板
      if (view === "models" || view === "mcp" || view === "perm") {
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
