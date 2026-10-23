/**
 * 「模型管理」独立窗口（models.html 入口）。
 *
 * 为什么拆成独立窗口：420px 面板塞不下「三列表格 + 表单 + 组菜单」，
 * 塞进去就是把选择和管理搅在一起 —— 定稿结论：**编辑弹到面板外面**。
 *
 * 分工：
 * - 面板（main.ts）     ：模型卡片展示 + ▾ 纯名称下拉切换，**不含任何管理功能**
 * - 本窗口（models.ts） ：表格 / 分组 / 添加 / 编辑 / 删除 / 测试连通，全部管理动作
 *
 * 窗口行为（后端 lib.rs）：
 * - ✕ / Esc = 隐藏不销毁（再开秒出），尺寸自动记回 settings.json —— 拖多大下次就多大
 * - 无边框 + 顶部自绘标题栏拖动 + always-on-top，与悬浮球一致
 * - 增删改命令会广播 `models-changed` 事件，面板收到后刷新模型卡片
 */
// 数据接口层：业务代码只认 `data.<域>.<方法>()`，不再直接调 Tauri invoke。
import { data } from "./data";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { MODELS_INTENT_KEY } from "./format";
import type {
  ModelView,
  RemoteModelInfo,
} from "./data/types";





/** 添加/编辑表单的会话内状态（re-render 时保住输入，错误也能回填） */
interface ModelFormState {
  /** null = 新建；否则为正在编辑的旧接口 id */
  editId: string | null;
  /** 正在编辑的条目所属组（Base URL）；与 editId 配对精确定位 */
  editUrl: string | null;
  id: string;
  url: string;
  apiKey: string;
  clearKey: boolean;
  tool: boolean;
  img: boolean;
  headersText: string;
  maxIn: string;
  maxOut: string;
  /** 「粘贴模型 JSON」框里的原文（重绘时保住） */
  jsonText: string;
  error?: string;
  preset: string;
}

/** 「拉取模型列表」弹层的会话内状态 */
interface RemoteFetchState {
  items: RemoteModelInfo[];
  source: string;
  sourceUrl: string;
  apiKey: string;
  headersText: string;
  tool: boolean;
  img: boolean;
  error?: string;
  /** 本地已配置的身份集合（`url::id`，渲染时用来标「已配置」） */
  localIds: Set<string>;
  checked: Set<string>;
  lastEndpoint?: string;
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

let app: HTMLElement;

let models: ModelView[] = [];
/** 当前选中的模型**接口 id**（身份 = (Base URL, id)，配 selectedModelUrl 消歧） */
let selectedModel: string | null = null;
let selectedModelUrl: string | null = null;
/** 分组显示名（键 = Base URL），settings.modelGroupNames */
let modelGroupNames: Record<string, string> = {};

let modelSearch = "";
/** 已折叠的分组（键 = Base URL）；默认全展开 */
const modelGroupClosed = new Set<string>();
/** 组菜单当前打开的目标（Base URL）；null = 没开 */
let modelGroupMenu: string | null = null;

let modelModalOpen = false;
let modelTypePickerOpen = false;
let modelForm: ModelFormState = emptyModelForm();
let remoteFetch: RemoteFetchState | null = null;

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function emptyModelForm(): ModelFormState {
  return {
    editId: null,
    editUrl: null,
    id: "",
    url: "",
    apiKey: "",
    clearKey: false,
    tool: true,
    img: true,
    headersText: "",
    maxIn: "",
    maxOut: "",
    jsonText: "",
    preset: "custom",
  };
}

/**
 * 当前选中判定：**只认唯一那一条**。
 *
 * 原来是 `id 相同 &&（有 url 才比 url）` —— 设置里只有 id 没记 url 时，
 * 所有同 id 的行都会命中（典型症状：「两个 glm-5.3-flash 都画线」——
 * `models.json` 里确实有两条 id 相同、Base URL 不同的 glm-5.3-flash）。
 * 现在先解析出当前模型，再按 (id, url) 全等比对，最多标一个。
 */
function isCurrent(m: ModelView): boolean {
  const cur = models.find((x) => x.id === selectedModel && (!selectedModelUrl || sameUrl(x.url, selectedModelUrl)));
  return cur !== undefined && m.id === cur.id && sameUrl(m.url, cur.url);
}

function shortHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url.replace(/^https?:\/\//, "").split("/")[0] || url;
  }
}

/**
 * 是不是**本机**跑的模型（设计稿 07:2:1081「本地 · 无工具」）。
 *
 * 本机模型的能力与云端不同（常常没有工具调用），标出来用户才知道
 * 为什么这个模型在工具行里没有「工具」。
 */
function isLocalUrl(url: string): boolean {
  const u = (url ?? "").toLowerCase();
  return (
    u.includes("localhost") ||
    u.includes("127.0.0.1") ||
    u.includes("0.0.0.0") ||
    u.includes("::1") ||
    u.includes("192.168.") ||
    u.includes("10.") ||
    u.startsWith("http://[")
  );
}

/** 模型身份键 = `url::id`（同 model 不同 Base URL 是两条独立配置） */
function identityUrl(id: string, url: string): string {
  return `${url.trim().replace(/\/+$/, "").toLowerCase()}::${id}`;
}

function configuredKeys(): Set<string> {
  return new Set(models.map((m) => identityUrl(m.id, m.url)));
}

/** URL 比较（忽略大小写与尾斜杠） */
function sameUrl(a: string, b: string): boolean {
  return a.trim().replace(/\/+$/, "").toLowerCase() === b.trim().replace(/\/+$/, "").toLowerCase();
}

function modelsEndpointPreview(baseUrl: string): string {
  return `${baseUrl.replace(/\/+$/, "")}/models`;
}

/**
 * API Key 输入框下方那行提示。
 *
 * 用户心智是「一个提供商一个 Key」：往已有组里加模型时，Key 留空就会
 * **自动继承**同组已配置的那把（后端 `models_add` / `models_fetch_remote`
 * 都是这条口径）。但表单上光写一句"云端留空自动继承同组 Key"，用户照样会
 * 再去粘一遍 —— 这里明确告诉他"这个 Base URL 下已经有 Key 了，留空即可"。
 */
function keyHintText(url: string, editing: boolean): string {
  if (editing) return "留空 = 保持原来的 Key 不变。";
  const u = url.trim();
  const fallback = "本地 / Ollama 可留空；云端留空自动继承同组 Key。";
  if (!u) return fallback;
  const same = models.filter((m) => sameUrl(m.url, u));
  if (same.length === 0) return fallback;
  const withKey = same.filter((m) => m.hasKey).length;
  if (withKey > 0) {
    return `这个 Base URL 下已有 ${withKey} 个模型配了 Key —— 留空即自动继承，不用再粘一遍。`;
  }
  return `这个 Base URL 下已有 ${same.length} 个模型（都没配 Key）—— 留空即不鉴权。`;
}

/** token 数：支持 `128000` / `128k` / `1m` */
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

let toastTimer: ReturnType<typeof setTimeout> | undefined;
function showToast(text: string, kind: "ok" | "error" | "info" = "ok", ms = 2600): void {
  document.querySelectorAll(".mw-toast").forEach((t) => t.remove());
  const t = document.createElement("div");
  t.className = `mw-toast ${kind}`;
  t.textContent = text;
  document.body.appendChild(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.remove(), ms);
}

/**
 * 确认框（Promise<boolean>）。不用 window.confirm：Tauri WebView 的系统对话框
 * 尺寸/样式不可控，还可能被宿主静默吞掉 —— 自己画，必弹、Esc/点遮罩可取消。
 */
function askConfirm(title: string, body: string, danger = "删除"): Promise<boolean> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "confirm-mask";
    mask.innerHTML = `
      <div class="confirm-card">
        <div class="confirm-title">${esc(title)}</div>
        <div class="confirm-body">${esc(body)}</div>
        <div class="confirm-actions">
          <button type="button" class="set-btn" data-k="no">取消</button>
          <button type="button" class="set-btn danger" data-k="yes">${esc(danger)}</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    let done = false;
    const close = (v: boolean): void => {
      if (done) return;
      done = true;
      document.removeEventListener("keydown", onKey, true);
      mask.remove();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      close(false);
    };

    mask.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
      b.addEventListener("click", () => close(b.dataset.k === "yes")),
    );
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) close(false);
    });
    document.addEventListener("keydown", onKey, true);

    // 焦点给「取消」：回车不该是破坏性操作
    mask.querySelector<HTMLButtonElement>('[data-k="no"]')?.focus();
  });
}

/** 单行文本输入弹窗（`prompt()` 在 Tauri WebView 里被禁用）。null = 取消。 */
function askInput(title: string, body: string, initial = ""): Promise<string | null> {
  return new Promise((resolve) => {
    const mask = document.createElement("div");
    mask.className = "confirm-mask";
    mask.innerHTML = `
      <div class="confirm-card">
        <div class="confirm-title">${esc(title)}</div>
        <div class="confirm-body">${esc(body)}</div>
        <input class="confirm-input" type="text" value="${esc(initial)}" spellcheck="false" />
        <div class="confirm-actions">
          <button type="button" class="set-btn" data-k="no">取消</button>
          <button type="button" class="set-btn ok" data-k="yes">确定</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    let done = false;
    const close = (v: string | null): void => {
      if (done) return;
      done = true;
      document.removeEventListener("keydown", onKey, true);
      mask.remove();
      resolve(v);
    };
    const input = mask.querySelector<HTMLInputElement>(".confirm-input")!;
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close(null);
        return;
      }
      if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        close(input.value);
      }
    };

    mask.querySelectorAll<HTMLButtonElement>("button").forEach((b) =>
      b.addEventListener("click", () => close(b.dataset.k === "yes" ? input.value : null)),
    );
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) close(null);
    });
    document.addEventListener("keydown", onKey, true);

    setTimeout(() => {
      input.focus();
      input.setSelectionRange(input.value.length, input.value.length);
    }, 0);
  });
}

// ---------------------------------------------------------------------------
// 数据
// ---------------------------------------------------------------------------

async function reloadModels(): Promise<void> {
  try {
    models = await data.models.list();
    const sel = await data.models.selected();
    selectedModel = sel?.id ?? null;
    selectedModelUrl = sel?.url ?? null;
    // 同 main.ts：旧设置只有 id 没 url 时按 (id, url) 全等判定会命中**所有**同 id 的行
    // （两个 glm-5.3-flash 都带当前标记）。这里也定死成唯一一条。
    if (selectedModel && !selectedModelUrl) {
      const resolved = models.find((x) => x.id === selectedModel);
      if (resolved) selectedModelUrl = resolved.url;
    }
    modelGroupNames = await data.models.groupNames();
  } catch (e) {
    showToast(`加载模型列表失败：${e}`, "error");
  }
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

/**
 * 八个缩放手柄。
 *
 * ⚠️ 为什么必须自己画（症状：可以拖拽的范围奇小）：
 * 后端这个窗口是 `resizable(true)` + `min_inner_size` + 尺寸写进 settings，
 * 缩放显然是设计内功能。但 Windows 的边缘 resize 命中测试要求 mousedown
 * 能落到窗口**边框那一圈**；而 `bindWindowDrag()` 几乎对整个客户区都做了
 * `preventDefault()` + `startDragging()` —— 客户区里根本没剩下能触发 resize
 * 的地方，只剩最外侧一条几乎抓不住的细缝。
 *
 * 所以显式画 8 条手柄，mousedown 直接调 `startResizeDragging(dir)`。
 * 它们是 `.mw-window` 的**兄弟**而不是子元素 —— `.mw-window` 有
 * `overflow:hidden` + 圆角，放里面会被裁掉。
 *
 * `dir` 用小写是为了 CSS 类名好写；传给 Tauri 前要换成首字母大写的枚举值
 * （`ResizeDirection` 是 `@tauri-apps/api/window` 的**模块内私有类型**，
 *  外部 import 不到，所以这里自己维护这张映射表）。
 */
const RESIZE_HANDLES = [
  { dir: "n", api: "North" },
  { dir: "s", api: "South" },
  { dir: "w", api: "West" },
  { dir: "e", api: "East" },
  { dir: "nw", api: "NorthWest" },
  { dir: "ne", api: "NorthEast" },
  { dir: "sw", api: "SouthWest" },
  { dir: "se", api: "SouthEast" },
] as const;

function bindResize(): void {
  if (!app) return;
  const apiByDir = new Map<string, string>(RESIZE_HANDLES.map((h) => [h.dir, h.api]));
  app.querySelectorAll<HTMLElement>(".mw-rz").forEach((el) => {
    el.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      // 必须 stopPropagation：否则冒泡到 .mw-window 的整窗拖动，
      // 一次操作会同时"移动窗口"和"缩放窗口"。
      e.preventDefault();
      e.stopPropagation();
      const api = apiByDir.get(el.dataset.dir ?? "");
      if (!api) return;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      void (getCurrentWindow() as any).startResizeDragging(api).catch(() => {});
    });
  });
}

/** 当前搜索命中的模型数（底部状态栏「匹配 / 总数」用） */
let modelFilteredCount = 0;

/** 有几个分组（= 几个不同的 Base URL）—— 标题条「N 个模型 · M 组」 */
function modelGroupCount(): number {
  const urls = new Set(models.map((m) => m.url.trim().replace(/\/+$/, "").toLowerCase()));
  return urls.size;
}

function renderModelsBody(): string {
  const q = (modelSearch || "").trim().toLowerCase();
  const filtered = q
    ? models.filter(
        (m) =>
          m.id.toLowerCase().includes(q) ||
          m.url.toLowerCase().includes(q),
      )
    : models;
  modelFilteredCount = filtered.length;

  // 按 Base URL 分组：模型身份 = (Base URL, 接口 id)，分组键就是 URL 本身。
  const groups = new Map<string, ModelView[]>();
  for (const m of filtered) {
    const list = groups.get(m.url);
    if (list) list.push(m);
    else groups.set(m.url, [m]);
  }

  const groupTitle = (url: string): string =>
    modelGroupNames[url]?.trim() || shortHost(url);

  const rows = [...groups.entries()]
    .map(([url, list]) => {
      const closed = modelGroupClosed.has(url);
      const curInside = list.some(isCurrent);
      const menuOpen = modelGroupMenu === url;
      const items = list
        .map((m) => {
          const cur = isCurrent(m);
          const ctx = m.maxInputTokens ? `${Math.round(m.maxInputTokens / 1000)}K` : "—";
          // 能力列（设计稿 07:2:1068「工具 · 视觉」/ 2:1073「工具」/ 2:1081「本地 · 无工具」）
          // —— 拼成**一段文字**，不是两颗 chip。
          const caps: string[] = [];
          if (isLocalUrl(m.url)) caps.push("本地");
          if (m.supportsToolCall) caps.push("工具");
          if (m.supportsImages) caps.push("视觉");
          const cap = caps.length ? caps.join(" · ") : "无工具";
          return `
      <div class="lm-row${cur ? " cur" : ""}" data-id="${esc(m.id)}" data-url="${esc(m.url)}">
        <div class="lm-name">
          <button class="lm-eye${cur ? " on" : ""}" data-id="${esc(m.id)}" data-url="${esc(m.url)}" title="${
            cur ? "当前使用中" : "设为当前模型"
          }">◎</button>
          <span class="lm-label" title="${esc(m.id)}">${esc(m.id)}</span>
          ${cur ? '<span class="lm-cur">当前</span>' : ""}
          ${m.hasKey === false ? '<span class="lm-nokey">无Key</span>' : ""}
        </div>
        <div class="lm-ctx">${esc(ctx)}</div>
        <div class="lm-tags">${esc(cap)}</div>
        <div class="lm-ops">
          <button class="set-btn edit" data-id="${esc(m.id)}" data-url="${esc(m.url)}">编辑</button>
          <button class="set-btn test" data-id="${esc(m.id)}" data-url="${esc(m.url)}">测</button>
          <button class="set-btn del" data-id="${esc(m.id)}" data-url="${esc(m.url)}">删</button>
        </div>
        <div class="set-test-msg" hidden></div>
      </div>`;
        })
        .join("");

      // 组菜单（VS Code 那个齿轮下拉）。不做「打开 JSON」：配置一律走表单。
      const menu = !menuOpen
        ? ""
        : `
      <div class="lm-menu" data-url="${esc(url)}">
        <button class="lm-mi" data-act="rename-group" data-url="${esc(url)}">重命名组</button>
        <button class="lm-mi" data-act="group-key" data-url="${esc(url)}">更新 API 密钥</button>
        <button class="lm-mi" data-act="add-into" data-url="${esc(url)}">在此添加模型</button>
        <button class="lm-mi" data-act="change-url" data-url="${esc(url)}">修改 Base URL</button>
        <div class="lm-sep"></div>
        <button class="lm-mi danger" data-act="del-group" data-url="${esc(url)}">删除组</button>
      </div>`;

      // 组块（设计稿 07:2:1034）：组头 = 折叠指示 + 组名 + 弹性空隙 + 组齿轮
      return `
      <div class="lm-group${curInside ? " cur" : ""}">
        <div class="lm-head-wrap">
          <button class="lm-head" data-url="${esc(url)}" title="${esc(url)}">
            <span class="lm-chev">${closed ? "▸" : "▾"}</span>
            <span class="lm-title">${esc(groupTitle(url))}</span>
          </button>
          <button class="lm-gear${menuOpen ? " on" : ""}" data-url="${esc(url)}" title="组操作">⚙</button>
          ${menu}
        </div>
        ${closed ? "" : `<div class="lm-body">${items}</div>`}
      </div>`;
    })
    .join("");

  // 表头（设计稿 07:2:1033）：**整表一行**，不是每组一行 ——
  // 列的含义跟组无关，重复画在每组头上只会让人以为列会变。
  const tableHead = `
    <div class="lm-th">
      <span>模型</span>
      <span>上下文</span>
      <span>能力</span>
      <span></span>
    </div>`;

  const f = modelForm;
  const editing = f.editId !== null;

  // 点「添加模型」先选类型。orbcat 只有 OpenAI 兼容一种协议，
  // 所以只留 Custom Endpoint（不摆 Anthropic/Azure/Google 那些点不通的选项）。
  const typePicker = !modelTypePickerOpen
    ? ""
    : `
    <div class="mm-mask" id="mm-mask">
      <div class="mm-dialog mm-narrow" role="dialog" aria-modal="true">
        <div class="mm-head">
          <span class="mm-title">选择提供程序类型</span>
          <button type="button" class="mm-x" id="mm-close" title="关闭（Esc）">✕</button>
        </div>
        <div class="mm-body mm-type-list">
          <button type="button" class="mm-type" id="mm-type-custom">
            <b>Custom Endpoint</b>
            <i>任意 OpenAI 兼容接口（含本地 Ollama / 自建网关）</i>
          </button>
        </div>
      </div>
    </div>`;

  // 「从提供商获取模型 ID」的弹出列表
  const picker = remoteFetch
    ? `
    <div class="id-picker">
      <div class="id-picker-head">
        <span>${
          remoteFetch.source === "json"
            ? "从粘贴的 JSON 里选择模型 ID"
            : `从 <code>${esc(remoteFetch.lastEndpoint ?? remoteFetch.sourceUrl)}</code> 选择模型 ID`
        }</span>
        <button type="button" class="set-btn" id="picker-close">关闭</button>
      </div>
      ${remoteFetch.error ? `<div class="form-err">${esc(remoteFetch.error)}</div>` : ""}
      <div class="id-picker-list">
        ${
          remoteFetch.items.length === 0
            ? '<div class="mem-empty">没有返回模型。</div>'
            : remoteFetch.items
                .map((it) => {
                  const already = remoteFetch!.localIds.has(
                    identityUrl(it.id, remoteFetch!.sourceUrl),
                  );
                  // 能力标签：只显示**对方明确声明**的（没声明就别显示，
                  // 否则会让人以为"不支持"）。ctx 同理。
                  const tags = [
                    it.supportsToolCall === true ? "工具" : "",
                    it.supportsImages === true ? "视觉" : "",
                  ]
                    .filter(Boolean)
                    .map((t) => `<span class="lm-tag">${t}</span>`)
                    .join("");
                  const ctx =
                    typeof it.contextLength === "number" && it.contextLength > 0
                      ? `${Math.round(it.contextLength / 1000)}K`
                      : "";
                  return `<button type="button" class="id-pick${already ? " already" : ""}" data-id="${esc(
                    it.id,
                  )}" data-ctx="${it.contextLength ?? ""}" data-out="${it.maxOutputLength ?? ""}" data-tool="${
                    it.supportsToolCall === true ? "1" : it.supportsToolCall === false ? "0" : ""
                  }" data-img="${
                    it.supportsImages === true ? "1" : it.supportsImages === false ? "0" : ""
                  }">
                    <span class="remote-id">${esc(it.id)}</span>
                    <span class="lm-tags">${tags}</span>
                    ${ctx ? `<span class="remote-meta">${ctx}</span>` : ""}
                    <span class="remote-meta">${already ? "已配置" : it.ownedBy ? esc(it.ownedBy) : ""}</span>
                  </button>`;
                })
                .join("")
        }
      </div>
      <div class="remote-actions">
        <button type="button" class="set-btn add" id="picker-batch">批量导入未配置的全部</button>
        <div class="set-hint">点一行 = 填入「接口模型 ID」（窗口/能力也会一并带入）；批量导入会共用当前 URL / Key。</div>
      </div>
    </div>`
    : "";

  // 添加 / 编辑模型 = 表单弹窗（永不打开 JSON 让用户手写）
  const dialog = !modelModalOpen
    ? ""
    : `
    <div class="mm-mask" id="mm-mask">
      <div class="mm-dialog" role="dialog" aria-modal="true">
        <div class="mm-head">
          <span class="mm-title">${editing ? "✏️ 编辑模型" : "➕ 添加模型"}</span>
          <button type="button" class="mm-x" id="mm-close" title="关闭（Esc）">✕</button>
        </div>
        <div class="set-form mm-body" id="model-form">
      ${f.error ? `<div class="form-err">${esc(f.error)}</div>` : ""}
      ${
        editing
          ? `<div class="set-hint">正在编辑 <code>${esc(f.editId!)}</code>${f.editUrl ? ` @ <code>${esc(shortHost(f.editUrl))}</code>` : ""} · <b>接口模型 ID</b> 才会发给提供商（request 的 <code>model</code>）</div>`
          : ""
      }
      <label class="fld"><span>Base URL</span>
        <!-- autocomplete="off"：关掉 WebView2 的自动填充下拉。
             那个浮层**不在我们的代码里**（"保存的信息"全仓库搜不到），
             是浏览器自己记着用户以前填过的 URL 弹出来的。
             底色由浏览器按系统浅色主题画，CSS 管不了，
             在深色窗口里就是一块刺眼的白板 —— 只能不让它弹。 -->
        <input id="f-url" placeholder="提供商 Base URL（含 /v1 等）" value="${esc(f.url)}"
          autocomplete="off" autocorrect="off" spellcheck="false" />
      </label>
      <label class="fld"><span>API Key</span>
        <input id="f-key" type="password" autocomplete="off" autocorrect="off" spellcheck="false" placeholder="${
          editing
            ? f.clearKey
              ? "保存后将清空 Key"
              : "留空保持原 Key"
            : "本地 / Ollama 可留空；云端留空自动继承同组 Key"
        }" value="${esc(f.clearKey ? "" : f.apiKey)}" />
      </label>
      <!-- 同组 Key 继承提示：Base URL 命中已有组时提示"留空即可用同组 Key"，
           否则用户会以为必须再粘一遍（其实后端已经会继承，但用户不知道）。
           实时更新走 JS（#f-key-hint），不整块重绘 —— 重绘会打断输入焦点。 -->
      <div class="set-hint" id="f-key-hint">${esc(keyHintText(f.url, editing))}</div>
      ${
        editing
          ? `<label class="set-check"><input type="checkbox" id="f-clear-key"${
              f.clearKey ? " checked" : ""
            } /> 清空 Key（不需要鉴权的本地模型）</label>`
          : ""
      }
      <div class="form-row-2">
        <label class="fld grow"><span>接口模型 ID（发给提供商的 model）</span>
          <input id="f-id" placeholder="deepseek-v4.1-flash" value="${esc(f.id)}"
          autocomplete="off" autocorrect="off" spellcheck="false" />
        </label>
        <button type="button" class="set-btn fetch-id" id="f-fetch-ids" title="GET ${esc(
          f.url.trim() ? modelsEndpointPreview(f.url.trim()) : "{Base URL}/models",
        )}">获取 ID</button>
      </div>
      <div class="form-row-2 even">
        <label class="fld"><span>上下文窗口（可填 128000 / 128k / 1m）</span>
          <input id="f-max-in" placeholder="128000" value="${esc(f.maxIn)}" />
        </label>
        <label class="fld"><span>最大输出</span>
          <input id="f-max-out" placeholder="8192" value="${esc(f.maxOut)}" />
        </label>
      </div>
      <div class="form-row-2 even">
        <label class="set-check"><input type="checkbox" id="f-tool"${f.tool ? " checked" : ""} /> 工具调用</label>
        <label class="set-check"><input type="checkbox" id="f-img"${f.img ? " checked" : ""} /> 图片（视觉）</label>
      </div>
      <details class="adv-box"${f.headersText ? " open" : ""}>
        <summary>额外请求头（可选）</summary>
        <textarea id="f-headers" rows="3" placeholder="每行一个 Key: Value">${esc(f.headersText)}</textarea>
        <div class="set-hint">OpenCode Zen 的 <code>x-opencode-session</code> 会自动补，不用手填。</div>
      </details>
      <details class="adv-box"${f.jsonText ? " open" : ""}>
        <summary>粘贴模型 JSON 导入（可选）</summary>
        <textarea id="f-json" rows="4" spellcheck="false" placeholder='{"id":"nemotron-3.5-lightning-free","context_length":262144,"max_completion_tokens":262144,"capabilities":{"tools":true,"vision":false}}'>${esc(f.jsonText)}</textarea>
        <div class="form-row-2" style="margin-top:6px">
          <button type="button" class="set-btn" id="f-parse-json">解析并填入</button>
        </div>
        <div class="set-hint">支持 OpenRouter / OpenAI 兼容的模型描述：<code>id</code>、<code>context_length</code>、<code>max_completion_tokens</code>、<code>capabilities.tools/vision</code> 都会被认出来。<b>Base URL 仍以上面那一栏为准</b>（可以改）。</div>
      </details>
      <div class="set-hint">Key 只存本地 <code>agent-data/models.json</code>（已 gitignore）。本地模型可不填 Key。</div>
      ${picker}
        </div>
        <div class="mm-foot">
          <button type="button" class="set-btn" id="mm-cancel">取消</button>
          <button type="button" class="set-btn add" id="f-add">${editing ? "保存修改" : "添加"}</button>
        </div>
      </div>
    </div>`;

  return `
    <div class="mw-body">
      <div class="set-form lm-tools">
        <input id="model-search" placeholder="键入以搜索…（名称 / 模型 ID / Base URL）" value="${esc(modelSearch)}"
          autocomplete="off" autocorrect="off" spellcheck="false" />
      </div>
      ${tableHead}
      <div class="lm-list">${
        rows || '<div class="mem-empty">还没有模型，点右上「添加自定义模型」。</div>'
      }</div>
    </div>
    ${typePicker}
    ${dialog}`;
}

/** 重绘时必须保住滚动位置的容器（`innerHTML` 整块替换会把 scrollTop 归零） */
const SCROLL_KEEP = [".lm-list", ".mm-body", ".id-picker-list"];

function renderApp(): void {
  if (!app) return;

  // 记下滚动位置。整棵 innerHTML 换掉后滚动容器是新元素，scrollTop=0 ——
  // 这就是「点任何按钮页面都自动上翻」的根因。
  const kept = new Map<string, number>();
  for (const sel of SCROLL_KEEP) {
    const el = app.querySelector<HTMLElement>(sel);
    if (el && el.scrollTop > 0) kept.set(sel, el.scrollTop);
  }

  app.innerHTML = `
    <div class="mw-window">
      <!-- 标题条（设计稿 07:2:1024）：左组 = 应用图标 + 标题 + 统计，
           右组 = 「添加自定义模型」（猫白实心）+ 关闭。 -->
      <div class="mw-titlebar">
        <div class="mw-tb-left">
          <span class="mw-app-ico" title="orbcat"></span>
          <span class="mw-title">模型管理</span>
          <span class="mw-count">${models.length} 个模型 · ${modelGroupCount()} 组</span>
        </div>
        <div class="mw-tb-right">
          <button type="button" class="mm-add-btn" id="model-add-open" title="添加一个 OpenAI 兼容接口">＋ 添加自定义模型</button>
          <button class="mw-close" id="mw-close" title="关闭（Esc）">✕</button>
        </div>
      </div>
      ${renderModelsBody()}
      <!-- 底部状态栏（设计稿 07:2:1026） -->
      <div class="mw-status">
        <span>身份键 = Base URL + 接口 id</span>
        <span class="mono">${
          modelFilteredCount === models.length
            ? `${models.length} 个模型`
            : `${modelFilteredCount} / ${models.length} 个模型`
        }</span>
      </div>
    </div>
    ${RESIZE_HANDLES.map((h) => `<div class="mw-rz mw-rz-${h.dir}" data-dir="${h.dir}"></div>`).join("")}`;

  // 还原（写 scrollTop 会强制 layout，不用等下一帧）
  for (const [sel, top] of kept) {
    const el = app.querySelector<HTMLElement>(sel);
    if (el) el.scrollTop = top;
  }

  bindApp();
  bindWindowDrag();
  bindResize();
}

/**
 * 整窗拖动。
 *
 * 为什么不用 Tauri 原生 `data-tauri-drag-region`：
 * - 裸属性只对「直接点在带属性的那个元素上」生效，子元素不继承 ——
 *   要整窗可拖就得给每个后代都贴一遍，漏一个就是一块拖不动的死区
 *   （这就是之前「只有标题栏能拖、下面全拖不动」的原因）；
 * - 新版支持 `="deep"`（整片子树可拖，可点击元素自动让路），但它会把
 *   **滚动条上的 mousedown** 也判成拖动区 —— 拖滚动条会变成拖窗口。
 *
 * 所以自己来：落在可点击元素 / 弹窗遮罩 / 滚动条上的 mousedown 一律放行，
 * 其余位置直接 `startDragging()`。与面板窗口（main.ts）同一套做法。
 */
function bindWindowDrag(): void {
  const win = app?.querySelector<HTMLElement>(".mw-window");
  if (!win) return;

  win.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;

    const t = e.target as HTMLElement;
    // 可点击元素 / 弹窗遮罩 / 下拉菜单：不拖，交给它们自己的 click
    if (
      t.closest(
        "button, input, select, textarea, label, a, summary, [tabindex], [contenteditable], .mm-mask, .lm-menu",
      )
    ) {
      return;
    }

    // 滚动容器右侧的滚动条上不拖，否则没法滚动
    const scroller = t.closest<HTMLElement>(".lm-list, .mm-body, .id-picker-list");
    if (scroller) {
      const r = scroller.getBoundingClientRect();
      if (e.clientX - r.left >= scroller.clientWidth) return;
    }

    e.preventDefault(); // 防止拖出文字选择光标
    void getCurrentWindow().startDragging();
  });
}

// ---------------------------------------------------------------------------
// 事件绑定
// ---------------------------------------------------------------------------

/** 把当前 DOM 表单读进 modelForm（保存 / 重绘前都要调，避免丢输入） */
function readModelForm(): void {
  if (!document.getElementById("model-form")) return;
  const f = modelForm;
  const val = (sel: string): string => {
    const el = document.querySelector(sel) as HTMLInputElement | HTMLTextAreaElement | null;
    return el?.value ?? "";
  };
  const chk = (sel: string, dflt: boolean): boolean => {
    const el = document.querySelector(sel) as HTMLInputElement | null;
    return el ? el.checked : dflt;
  };
  f.id = val("#f-id");
  f.url = val("#f-url");
  f.apiKey = val("#f-key");
  f.clearKey = chk("#f-clear-key", f.clearKey);
  f.tool = chk("#f-tool", f.tool);
  f.img = chk("#f-img", f.img);
  f.headersText = val("#f-headers");
  f.maxIn = val("#f-max-in");
  f.maxOut = val("#f-max-out");
  f.jsonText = val("#f-json");
}

function setModelFormError(msg: string | undefined): void {
  modelForm.error = msg;
  renderApp();
}

function closeDialog(): void {
  modelModalOpen = false;
  modelTypePickerOpen = false;
  modelForm = emptyModelForm();
  remoteFetch = null;
  renderApp();
}

function bindApp(): void {
  // —— 顶栏 ——
  document.getElementById("mw-close")?.addEventListener("click", () => {
    void data.models.windowClose().catch(() => {});
  });

  // 「添加模型」→ 先选类型（只留 Custom Endpoint）
  document.getElementById("model-add-open")?.addEventListener("click", () => {
    modelForm = emptyModelForm();
    remoteFetch = null;
    modelModalOpen = false;
    modelTypePickerOpen = true;
    renderApp();
  });

  // 选中 Custom Endpoint → 打开表单弹窗
  document.getElementById("mm-type-custom")?.addEventListener("click", () => {
    modelTypePickerOpen = false;
    modelModalOpen = true;
    renderApp();
    setTimeout(() => {
      const el = document.getElementById("f-url") as HTMLInputElement | null;
      el?.focus();
    }, 0);
  });

  document.getElementById("mm-close")?.addEventListener("click", closeDialog);
  document.getElementById("mm-cancel")?.addEventListener("click", closeDialog);
  document.getElementById("mm-mask")?.addEventListener("mousedown", (e) => {
    if (e.target === e.currentTarget) closeDialog();
  });

  // —— 组菜单：齿轮开合 ——
  app.querySelectorAll<HTMLButtonElement>(".lm-gear").forEach((b) =>
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      const url = b.dataset.url!;
      modelGroupMenu = modelGroupMenu === url ? null : url;
      readModelForm();
      renderApp();
    }),
  );

  // —— 组菜单项 ——
  app.querySelectorAll<HTMLButtonElement>(".lm-mi").forEach((b) =>
    b.addEventListener("click", async (e) => {
      e.stopPropagation();
      const act = b.dataset.act!;
      const url = b.dataset.url!;
      modelGroupMenu = null;

      if (act === "rename-group") {
        const cur = modelGroupNames[url]?.trim() || shortHost(url);
        const name = await askInput("重命名组", "给这个 Base URL 起个短名字", cur);
        if (name === null) {
          renderApp();
          return;
        }
        try {
          await data.models.groupRename( { url, name });
          if (name.trim()) modelGroupNames[url] = name.trim();
          else delete modelGroupNames[url];
          showToast(`已重命名为「${name.trim() || shortHost(url)}」`);
        } catch (err) {
          showToast(`重命名失败：${err}`, "error");
        }
        renderApp();
        return;
      }

      if (act === "group-key") {
        const key = await askInput("更新 API 密钥", `将替换该组全部模型的 Key（${shortHost(url)}）`, "");
        if (key === null) {
          renderApp();
          return;
        }
        try {
          const n = await data.models.setGroupKey( { url, apiKey: key });
          await reloadModels();
          showToast(`已更新 ${n} 个模型的 Key`);
        } catch (err) {
          showToast(`更新失败：${err}`, "error");
        }
        renderApp();
        return;
      }

      if (act === "add-into") {
        // 带出该组 URL，直接进表单
        modelForm = emptyModelForm();
        modelForm.url = url;
        remoteFetch = null;
        modelModalOpen = true;
        renderApp();
        setTimeout(() => {
          (document.getElementById("f-id") as HTMLInputElement | null)?.focus();
        }, 0);
        return;
      }

      if (act === "change-url") {
        // 整组换 Base URL：以前只能逐个模型点编辑改 URL
        const n = models.filter((m) => sameUrl(m.url, url)).length;
        const next = await askInput(
          "修改 Base URL",
          `把「${shortHost(url)}」下的 ${n} 个模型挪到新的 Base URL（Key、窗口、能力都跟着走）`,
          url,
        );
        if (next === null) {
          renderApp();
          return;
        }
        if (!next.trim()) {
          showToast("Base URL 不能为空", "error");
          renderApp();
          return;
        }
        try {
          const res = await data.models.changeGroupUrl(
            { url, newUrl: next.trim() },
          );
          await reloadModels();
          showToast(
            `已改到 ${res.newUrl}（${res.moved} 个模型）` +
              (res.skipped.length ? `，跳过 ${res.skipped.length} 个重名` : ""),
          );
        } catch (err) {
          showToast(`修改失败：${err}`, "error");
        }
        renderApp();
        return;
      }

      if (act === "del-group") {
        const n = models.filter((m) => m.url === url).length;
        const ok = await askConfirm("删除组", `确定删除「${shortHost(url)}」下的全部 ${n} 个模型？`);
        if (!ok) {
          renderApp();
          return;
        }
        try {
          await data.models.removeGroup( { url });
          await reloadModels();
          showToast(`已删除该组 ${n} 个模型`);
        } catch (err) {
          showToast(`删除失败：${err}`, "error");
        }
        renderApp();
        return;
      }
    }),
  );

  // 点空白关组菜单
  app.addEventListener("mousedown", (e) => {
    if (!modelGroupMenu) return;
    const t = e.target as HTMLElement;
    if (t.closest(".lm-head-wrap")) return;
    modelGroupMenu = null;
    readModelForm();
    renderApp();
  });

  // —— 搜索 ——
  document.getElementById("model-search")?.addEventListener("input", (e) => {
    modelSearch = (e.target as HTMLInputElement).value;
    readModelForm();
    renderApp();
    const s = document.getElementById("model-search") as HTMLInputElement | null;
    if (s) {
      s.focus();
      s.setSelectionRange(s.value.length, s.value.length);
    }
  });

  // —— 从提供商获取模型 ID ——
  document.getElementById("f-fetch-ids")?.addEventListener("click", async () => {
    readModelForm();
    const f = modelForm;
    const url = f.url.trim();
    const key = f.apiKey.trim();
    if (!url) {
      setModelFormError("先填 Base URL，再获取模型 ID");
      return;
    }
    // Key 留空时后端会**继承同组 Key**（用户心智：一个提供商一个 Key）。
    // 这里先记下"这次是不是走的继承"，拉完告诉用户一声 —— 否则他以为是自己填的。
    const inherited = !key && models.some((m) => sameUrl(m.url, url) && m.hasKey);
    try {
      const list = await data.models.fetchRemote( {
        url,
        apiKey: key,
        headers: f.headersText.trim() ? f.headersText : null,
      });
      remoteFetch = {
        items: list,
        source: "custom",
        sourceUrl: url,
        apiKey: key,
        headersText: f.headersText,
        tool: f.tool,
        img: f.img,
        localIds: configuredKeys(),
        checked: new Set(),
        lastEndpoint: modelsEndpointPreview(url),
        error: undefined,
      };
      f.error = undefined;
      if (list.length === 0) {
        f.error = "提供商返回 0 个模型（接口可能不支持 /models）";
      } else if (inherited) {
        showToast(`已用同组 Key 拉到 ${list.length} 个模型`, "info");
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
        localIds: configuredKeys(),
        checked: new Set(),
        error: String(e),
      };
      f.error = undefined;
    }
    renderApp();
  });

  // —— Base URL 改一改 → 实时刷新「同组 Key 继承」提示 ——
  // 只改那行文字，**不整块重绘**（重绘会把输入焦点打断，用户正在打字就废了）。
  document.getElementById("f-url")?.addEventListener("input", (e) => {
    const val = (e.target as HTMLInputElement).value;
    modelForm.url = val;
    const hint = document.getElementById("f-key-hint");
    if (hint) hint.textContent = keyHintText(val, modelForm.editId !== null);
  });

  // —— 粘贴模型 JSON → 解析并填入 ——
  // 解析在后端做（`models_parse_spec` → 跟「从提供商拉列表」**同一个**解析器），
  // 所以"拉得到、粘进来就丢字段"这种事不会发生。
  document.getElementById("f-parse-json")?.addEventListener("click", async () => {
    readModelForm();
    const txt = modelForm.jsonText.trim();
    if (!txt) {
      setModelFormError("先粘贴一段模型 JSON");
      return;
    }
    try {
      const list = await data.models.parseSpec( { payload: txt });
      if (!list.length) {
        setModelFormError("这段 JSON 里没解析出模型");
        return;
      }
      remoteFetch = {
        items: list,
        source: "json",
        sourceUrl: modelForm.url.trim(),
        apiKey: modelForm.apiKey,
        headersText: modelForm.headersText,
        tool: modelForm.tool,
        img: modelForm.img,
        localIds: configuredKeys(),
        checked: new Set(),
        lastEndpoint: "粘贴的 JSON",
        error: undefined,
      };
      modelForm.error = undefined;
      renderApp();
      showToast(`解析出 ${list.length} 个模型，点一行填入`, "ok");
    } catch (e) {
      setModelFormError(`解析失败：${e}`);
    }
  });

  document.getElementById("picker-close")?.addEventListener("click", () => {
    readModelForm();
    remoteFetch = null;
    renderApp();
  });

  app.querySelectorAll<HTMLButtonElement>(".id-pick").forEach((b) =>
    b.addEventListener("click", async () => {
      readModelForm();
      const id = b.dataset.id ?? "";
      const ctx = Number(b.dataset.ctx || 0);
      const out = Number(b.dataset.out || 0);
      // 能力："" = 对方没声明（沿用表单勾选）；"1"/"0" = 明确支持/不支持
      const tool = b.dataset.tool ?? "";
      const img = b.dataset.img ?? "";
      const url0 = remoteFetch?.sourceUrl ?? modelForm.url;
      remoteFetch = null;
      // 同源同 id 已配置 → 直接进编辑（身份 = URL + id）
      const hit = models.find((m) => m.id === id && sameUrl(m.url, url0));
      if (hit) {
        try {
          const ev = await data.models.getEdit( { id: hit.id, url: hit.url });
          modelForm = {
            editId: ev.id,
            editUrl: ev.url,
            id: ev.id,
            url: ev.url,
            apiKey: "",
            clearKey: false,
            tool: ev.supportsToolCall,
            img: ev.supportsImages,
            headersText: ev.headersText,
            maxIn: ev.maxInputTokens != null ? String(ev.maxInputTokens) : "",
            maxOut: ev.maxOutputTokens != null ? String(ev.maxOutputTokens) : "",
            jsonText: modelForm.jsonText,
            preset: "custom",
            error: undefined,
          };
          renderApp();
          showToast(`「${hit.id}」已在列表中，已进入编辑`, "info");
        } catch (e) {
          showToast(`读取失败：${e}`, "error");
        }
        return;
      }
      modelForm.id = id;
      if (ctx > 0) modelForm.maxIn = String(ctx);
      if (out > 0) modelForm.maxOut = String(out);
      if (tool === "1") modelForm.tool = true;
      else if (tool === "0") modelForm.tool = false;
      if (img === "1") modelForm.img = true;
      else if (img === "0") modelForm.img = false;
      modelForm.error = undefined;
      renderApp();
      showToast(`已填入模型 ID：${id}`, "ok");
    }),
  );

  document.getElementById("picker-batch")?.addEventListener("click", async () => {
    readModelForm();
    const rf = remoteFetch;
    const f = modelForm;
    if (!rf) return;
    const ids = rf.items
      .map((x) => x.id)
      .filter((id) => !rf.localIds.has(identityUrl(id, f.url.trim() || rf.sourceUrl)));
    if (ids.length === 0) {
      showToast("没有可导入的模型（都已配置）", "info");
      return;
    }
    // 逐模型的元信息：OpenRouter 风格的返回里，窗口大小和能力都是**每个模型**
    // 各自一份的（`context_length` / `max_completion_tokens` / `capabilities`），
    // 一刀切按表单勾选会把能力标错。
    const contextLengths: Record<string, number> = {};
    const maxOutputTokens: Record<string, number> = {};
    const toolFlags: Record<string, boolean> = {};
    const imageFlags: Record<string, boolean> = {};
    for (const it of rf.items) {
      if (typeof it.contextLength === "number" && it.contextLength > 0) {
        contextLengths[it.id] = it.contextLength;
      }
      if (typeof it.maxOutputLength === "number" && it.maxOutputLength > 0) {
        maxOutputTokens[it.id] = it.maxOutputLength;
      }
      if (typeof it.supportsToolCall === "boolean") toolFlags[it.id] = it.supportsToolCall;
      if (typeof it.supportsImages === "boolean") imageFlags[it.id] = it.supportsImages;
    }
    try {
      const result = await data.models.importRemote( {
        ids,
        sourceId: null,
        url: f.url,
        apiKey: f.apiKey || null,
        headers: f.headersText.trim() ? f.headersText : null,
        supportsToolCall: f.tool,
        supportsImages: f.img,
        contextLengths,
        maxOutputTokens,
        toolFlags,
        imageFlags,
      });
      await reloadModels();
      remoteFetch = null;
      showToast(
        `已导入 ${result.added.length} 个` +
          (result.skipped.length ? `（跳过 ${result.skipped.length}）` : ""),
      );
    } catch (e) {
      showToast(`批量导入失败：${e}`, "error");
    }
    renderApp();
  });

  // —— 分组折叠 ——
  app.querySelectorAll<HTMLButtonElement>(".lm-head").forEach((b) =>
    b.addEventListener("click", () => {
      const url = b.dataset.url!;
      if (modelGroupClosed.has(url)) modelGroupClosed.delete(url);
      else modelGroupClosed.add(url);
      readModelForm();
      renderApp();
    }),
  );

  // —— 列表操作：◎ 切当前 ——
  app.querySelectorAll<HTMLButtonElement>(".lm-eye").forEach((b) =>
    b.addEventListener("click", async () => {
      selectedModel = b.dataset.id!;
      selectedModelUrl = b.dataset.url ?? null;
      await data.models.select( { id: selectedModel, url: selectedModelUrl }).catch((e) =>
        showToast(`保存失败：${e}`, "error"),
      );
      renderApp();
      showToast(`已切换到 ${selectedModel}`, "ok", 2000);
    }),
  );

  // —— 删除 ——
  app.querySelectorAll<HTMLButtonElement>(".set-btn.del").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      const url = b.dataset.url!;
      const ok = await askConfirm("删除模型", `确定删除「${id}」？`);
      if (!ok) return;
      await data.models.remove( { id, url }).catch((e) =>
        showToast(`删除失败：${e}`, "error"),
      );
      if (modelForm.editId === id && modelForm.editUrl && sameUrl(modelForm.editUrl, url)) {
        modelForm = emptyModelForm();
        modelModalOpen = false;
      }
      await reloadModels();
      renderApp();
      showToast(`已删除 ${id}`, "ok");
    }),
  );

  // —— 编辑 ——
  app.querySelectorAll<HTMLButtonElement>(".set-btn.edit").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      const url = b.dataset.url!;
      try {
        const ev = await data.models.getEdit( { id, url });
        modelForm = {
          editId: ev.id,
          editUrl: ev.url,
          id: ev.id,
          url: ev.url,
          apiKey: "",
          clearKey: false,
          tool: ev.supportsToolCall,
          img: ev.supportsImages,
          headersText: ev.headersText,
          maxIn: ev.maxInputTokens != null ? String(ev.maxInputTokens) : "",
          maxOut: ev.maxOutputTokens != null ? String(ev.maxOutputTokens) : "",
          jsonText: "",
          preset: "custom",
          error: undefined,
        };
        remoteFetch = null;
        modelModalOpen = true;
        renderApp();
        const hint = document.getElementById("f-key") as HTMLInputElement | null;
        if (hint && ev.hasKey) {
          hint.placeholder = `Key ${ev.keyPreview}（留空保持不变）`;
        }
      } catch (e) {
        showToast(`读取模型配置失败：${e}`, "error");
      }
    }),
  );

  // —— 测连通 ——
  app.querySelectorAll<HTMLButtonElement>(".set-btn.test").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      const url = b.dataset.url!;
      const row = [...app.querySelectorAll<HTMLElement>(".lm-row")].find(
        (r) => r.dataset.id === id && r.dataset.url === url,
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
        const reply = await data.models.test( { id, url });
        say(`✅ 连通正常：${reply.slice(0, 150)}`, "ok");
        showToast(`✅ ${id} 连通正常`);
      } catch (e) {
        say(`❌ 测试失败：${e}`, "bad");
        showToast(`❌ ${id} 测试失败：${e}`, "error");
      } finally {
        b.disabled = false;
        b.textContent = oldLabel;
      }
    }),
  );

  // —— 保存（添加 / 编辑同一入口）；唯一键 = 显示名，接口 model 允许重复 ——
  document.getElementById("f-add")?.addEventListener("click", async () => {
    readModelForm();
    const f = modelForm;
    const editing = f.editId !== null;
    const newId = f.id.trim();
    const url = f.url.trim();
    const key = f.apiKey.trim();

    const fail = (m: string): void => {
      f.error = m;
      renderApp();
    };

    if (!newId) return fail("接口模型 ID 不能为空（点「获取 ID」可从提供商选）");
    if (!url) return fail("Base URL 不能为空");
    if (/\s/.test(newId)) return fail("接口模型 ID 不能含空格");

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
        // 身份 = (Base URL, 接口 id)：编辑前条目按 editId + editUrl 精确定位
        //
        // ⚠️ `url` 与 `newUrl` 是**两个不同的东西**，别合并：
        //   url    = 定位用的**旧** Base URL（哪一条要改）
        //   newUrl = 表单里填的**新** Base URL（= 换组）
        // 以前只发 url=f.editUrl（新值根本没上线），于是输入框里能改 Base URL、
        // 保存后静默丢弃 —— 用户看不到任何报错，以为改了其实没改。
        await data.models.edit( {
          id: f.editId,
          url: f.editUrl,
          newUrl: url,
          newId,
          apiKey: key || null,
          clearKey: f.clearKey,
          supportsToolCall: f.tool,
          supportsImages: f.img,
          headers,
          maxInputTokens: maxIn,
          maxOutputTokens: maxOut,
        });
        showToast(`已保存 ${newId}`);
        // 选中项就是这条（id+url 匹配）→ 改了 id 跟随新 id，改了 Base URL 跟随新 url
        if (
          selectedModel === f.editId &&
          (!selectedModelUrl || sameUrl(f.editUrl ?? "", selectedModelUrl))
        ) {
          selectedModel = newId;
          selectedModelUrl = url;
        } else if (selectedModelUrl && sameUrl(f.editUrl ?? "", selectedModelUrl)) {
          // id 没改但 url 改了：当前选中项的分组也要跟着走
          selectedModelUrl = url;
        }
        modelForm = emptyModelForm();
      } else {
        await data.models.add( {
          id: newId,
          url,
          apiKey: key,
          supportsToolCall: f.tool,
          supportsImages: f.img,
          headers,
          maxInputTokens: maxIn,
          maxOutputTokens: maxOut,
          overwrite: true,
        });
        // 新加的模型设为当前（与旧版行为一致）；后端会广播事件刷新面板
        selectedModel = newId;
        selectedModelUrl = url;
        await data.models.select( { id: newId, url }).catch(() => {});
        modelForm = emptyModelForm();
        showToast(`已保存 ${newId}`);
      }
      await reloadModels();
      remoteFetch = null;
      modelModalOpen = false; // 保存成功 → 关弹窗回列表
      renderApp();
    } catch (e) {
      fail(`保存失败：${e}`);
    }
  });
}

// ---------------------------------------------------------------------------
// 全局键盘：Esc 逐级关闭（表单弹窗 → 组菜单 → 隐藏窗口）
// ---------------------------------------------------------------------------

window.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (modelModalOpen || modelTypePickerOpen) {
    e.preventDefault();
    closeDialog();
    return;
  }
  if (modelGroupMenu) {
    e.preventDefault();
    modelGroupMenu = null;
    renderApp();
    return;
  }
  // confirm-mask 自己带 Esc（document 捕获阶段），能走到这里说明没有弹层
  if (document.querySelector(".confirm-mask")) return;
  e.preventDefault();
  void data.models.windowClose().catch(() => {});
});

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

/**
 * 诊断打点（排查用）：把前端启动/异常写进 `agent-data/diag.log`。
 *
 * 独立窗口没有控制台可看，出问题时只能靠后端日志文件 ——
 * 这条链路本身失败也不能再抛错（静默吞掉）。
 */
function diag(msg: string): void {
  void data.system.diagLog( { msg }).catch(() => {});
}

window.addEventListener("error", (e) => {
  diag(`models.ts window.onerror: ${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`);
});
window.addEventListener("unhandledrejection", (e) => {
  diag(`models.ts unhandledrejection: ${String(e.reason)}`);
});

/**
 * 消费面板留下的一次性意图位：`"add"` → **直接落到添加表单**上。
 *
 * 面板那颗「＋ 添加自定义模型」隔着一个 webview，手上没有"给对方发事件"的命令
 * （后端只有 open/close 两个），但两个窗口同源 —— `localStorage` 共享，
 * 拿它当一次性信使。键名与写入方共用 `format.ts::MODELS_INTENT_KEY`。
 *
 * 为什么除 boot 之外还要挂 `focus`：窗口是「✕ = 隐藏不销毁」，第二次打开
 * **不会再跑一遍模块初始化**；`show()` 会把焦点还给窗口，focus 是唯一稳定信号。
 * 另外「选类型」那一步只剩 Custom Endpoint 一种协议，这里直接跳过它。
 */
function consumeModelsIntent(): void {
  let intent: string | null = null;
  try {
    intent = localStorage.getItem(MODELS_INTENT_KEY);
    if (intent) localStorage.removeItem(MODELS_INTENT_KEY);
  } catch {
    return; // 拿不到 localStorage 就什么都不做，别把窗口搞崩
  }
  if (intent !== "add") return;
  diag("models.ts: 收到「添加自定义模型」意图 → 直接落到添加表单");
  modelSearch = "";
  modelForm = emptyModelForm();
  remoteFetch = null;
  modelTypePickerOpen = false;
  modelModalOpen = true;
  if (app) renderApp();
  setTimeout(() => {
    const el = document.getElementById("f-url") as HTMLInputElement | null;
    el?.focus();
  }, 0);
}

async function boot(): Promise<void> {
  diag("models.ts boot: 开始");
  const el = document.getElementById("models-app");
  if (!el) {
    diag("models.ts boot: 找不到 #models-app，放弃");
    return;
  }
  app = el;
  try {
    await reloadModels();
    renderApp();
    diag(`models.ts boot: 渲染完成（${models.length} 个模型）`);
  } catch (err) {
    diag(`models.ts boot: 失败 ${String(err)}`);
  }

  // 面板点「＋ 添加自定义模型」留下的意图位 —— 首次加载时消费
  consumeModelsIntent();
  // 窗口隐藏不销毁：第二次打开走 focus，不再走 boot
  window.addEventListener("focus", consumeModelsIntent);

  // 后端在任何增删改/切换后会广播 —— 数据悄悄更一遍；表单开着就不重绘（防丢输入）
  void listen("models-changed", () => {
    void reloadModels().then(() => {
      if (!modelModalOpen && !modelTypePickerOpen) renderApp();
    });
  });
}

void boot();
