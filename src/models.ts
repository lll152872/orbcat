/**
 * 「模型管理」独立窗口（models.html 入口）。
 *
 * 为什么拆成独立窗口：420px 面板塞不下「三列表格 + 表单 + 组菜单」，
 * 塞进去就是把选择和管理搅在一起 —— 用户明确拍板：**编辑弹到面板外面**。
 *
 * 分工（2026-09-29 定稿）：
 * - 面板（main.ts）     ：模型卡片展示 + ▾ 纯名称下拉切换，**不含任何管理功能**
 * - 本窗口（models.ts） ：表格 / 分组 / 添加 / 编辑 / 删除 / 测试连通，全部管理动作
 *
 * 窗口行为（后端 lib.rs）：
 * - ✕ / Esc = 隐藏不销毁（再开秒出），尺寸自动记回 settings.json —— 拖多大下次就多大
 * - 无边框 + 顶部自绘标题栏拖动 + always-on-top，与悬浮球一致
 * - 增删改命令会广播 `models-changed` 事件，面板收到后刷新模型卡片
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { MODELS_INTENT_KEY } from "./format";

// ---------------------------------------------------------------------------
// 类型（与 Rust 端 camelCase 序列化一一对应）
// ---------------------------------------------------------------------------

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

/** 远端 GET /models 返回的一项 */
interface RemoteModelInfo {
  id: string;
  ownedBy?: string;
  contextLength?: number;
  maxOutputLength?: number;
}

/** 编辑表单回填（不含完整 Key） */
interface ModelEditView {
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
    preset: "custom",
  };
}

/**
 * 当前选中判定：**只认唯一那一条**。
 *
 * 原来是 `id 相同 &&（有 url 才比 url）` —— 设置里只有 id 没记 url 时，
 * 所有同 id 的行都会命中（用户 2026-10-02 报的「两个 glm-5.3-flash 都画线」：
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
    models = await invoke<ModelView[]>("list_models");
    const sel = await invoke<{ id: string; url: string | null } | null>("get_selected_model");
    selectedModel = sel?.id ?? null;
    selectedModelUrl = sel?.url ?? null;
    // 同 main.ts：旧设置只有 id 没 url 时按 (id, url) 全等判定会命中**所有**同 id 的行
    // （两个 glm-5.3-flash 都带当前标记）。这里也定死成唯一一条。
    if (selectedModel && !selectedModelUrl) {
      const resolved = models.find((x) => x.id === selectedModel);
      if (resolved) selectedModelUrl = resolved.url;
    }
    modelGroupNames = await invoke<Record<string, string>>("model_group_names");
  } catch (e) {
    showToast(`加载模型列表失败：${e}`, "error");
  }
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

function renderModelsBody(): string {
  const q = (modelSearch || "").trim().toLowerCase();
  const filtered = q
    ? models.filter(
        (m) =>
          m.id.toLowerCase().includes(q) ||
          m.url.toLowerCase().includes(q),
      )
    : models;

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
          const tags = `${m.supportsToolCall ? '<span class="lm-tag">工具</span>' : ""}${
            m.supportsImages ? '<span class="lm-tag">视觉</span>' : ""
          }`;
          return `
      <div class="lm-row${cur ? " cur" : ""}" data-id="${esc(m.id)}" data-url="${esc(m.url)}">
        <div class="lm-name">
          <button class="lm-eye${cur ? " on" : ""}" data-id="${esc(m.id)}" data-url="${esc(m.url)}" title="${
            cur ? "当前使用中" : "设为当前模型"
          }">◎</button>
          <span class="lm-label" title="${esc(m.id)}">${esc(m.id)}</span>
          ${m.hasKey === false ? '<span class="lm-nokey">无Key</span>' : ""}
        </div>
        <div class="lm-ctx">${esc(ctx)}</div>
        <div class="lm-tags">${tags}</div>
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
        <div class="lm-sep"></div>
        <button class="lm-mi danger" data-act="del-group" data-url="${esc(url)}">删除组</button>
      </div>`;

      return `
      <div class="lm-group${curInside ? " cur" : ""}">
        <div class="lm-head-wrap">
          <button class="lm-head" data-url="${esc(url)}" title="${esc(url)}">
            <span class="lm-chev">${closed ? "▸" : "▾"}</span>
            <span class="lm-title">${esc(groupTitle(url))}</span>
            <span class="lm-count">${list.length}</span>
          </button>
          <button class="lm-gear${menuOpen ? " on" : ""}" data-url="${esc(url)}" title="组操作">⚙</button>
          ${menu}
        </div>
        ${
          closed
            ? ""
            : `<div class="lm-body">
                <div class="lm-th">
                  <span>名称</span><span>上下文大小</span><span>功能</span><span>操作</span>
                </div>
                ${items}
              </div>`
        }
      </div>`;
    })
    .join("");

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
                  const already = remoteFetch!.localIds.has(
                    identityUrl(it.id, remoteFetch!.sourceUrl),
                  );
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
        <div class="set-hint">点一行 = 填入「接口模型 ID」；批量导入会共用当前 URL / Key。</div>
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
        <button type="button" class="set-btn add" id="model-add-open">添加模型</button>
      </div>
      <div class="lm-list">${
        rows || '<div class="mem-empty">还没有模型，点右上「添加模型」。</div>'
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
      <div class="mw-titlebar">
        <span class="mw-title">模型管理</span>
        <span class="mw-count">${models.length} 个模型 · 点卡片◎切当前 · Esc 关闭</span>
        <button class="mw-close" id="mw-close" title="关闭（Esc）">✕</button>
      </div>
      ${renderModelsBody()}
    </div>`;

  // 还原（写 scrollTop 会强制 layout，不用等下一帧）
  for (const [sel, top] of kept) {
    const el = app.querySelector<HTMLElement>(sel);
    if (el) el.scrollTop = top;
  }

  bindApp();
  bindWindowDrag();
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
    void invoke("models_window_close").catch(() => {});
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
          await invoke("model_group_rename", { url, name });
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
          const n = await invoke<number>("models_set_group_key", { url, apiKey: key });
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

      if (act === "del-group") {
        const n = models.filter((m) => m.url === url).length;
        const ok = await askConfirm("删除组", `确定删除「${shortHost(url)}」下的全部 ${n} 个模型？`);
        if (!ok) {
          renderApp();
          return;
        }
        try {
          await invoke<number>("models_remove_group", { url });
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
    try {
      const list = await invoke<RemoteModelInfo[]>("models_fetch_remote", {
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
      const url0 = remoteFetch?.sourceUrl ?? modelForm.url;
      remoteFetch = null;
      // 同源同 id 已配置 → 直接进编辑（身份 = URL + id）
      const hit = models.find((m) => m.id === id && sameUrl(m.url, url0));
      if (hit) {
        try {
          const ev = await invoke<ModelEditView>("models_get_edit", { id: hit.id, url: hit.url });
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
      await invoke("set_selected_model", { id: selectedModel, url: selectedModelUrl }).catch((e) =>
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
      await invoke("models_remove", { id, url }).catch((e) =>
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
        const ev = await invoke<ModelEditView>("models_get_edit", { id, url });
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
        const reply = await invoke<string>("test_model", { id, url });
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
        await invoke("models_edit", {
          id: f.editId,
          url: f.editUrl,
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
        // 选中项就是这条（id+url 匹配）且改了 id → selected 跟随新 id
        if (
          selectedModel === f.editId &&
          (!selectedModelUrl || sameUrl(f.editUrl ?? "", selectedModelUrl))
        ) {
          selectedModel = newId;
        }
        modelForm = emptyModelForm();
      } else {
        await invoke<string>("models_add", {
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
        await invoke("set_selected_model", { id: newId, url }).catch(() => {});
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
  void invoke("models_window_close").catch(() => {});
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
  void invoke("diag_log", { msg }).catch(() => {});
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
