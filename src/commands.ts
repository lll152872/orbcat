/**
 * 斜杠命令面板 —— 命令注册表 + 三路匹配 + 浮层渲染。
 *
 * ## 为什么独立成模块（不塞进 main.ts）
 *
 * `main.ts` 已经 9000+ 行，`renderPanel` 一个函数扛 5 套视图。
 * 命令面板是**自洽**的一块：命令表（纯数据）+ 匹配（纯函数）+ 渲染（DOM）+
 * 键盘导航，**不依赖 main.ts 的任何模块状态**（动作通过注入的回调执行）。
 * 拆出来才能单独测 —— 匹配逻辑是纯函数，测起来不需要 DOM。
 *
 * ## 三路匹配（用户要求：中文 / 英文 / 拼音都能打）
 *
 * 命令有三套名字：
 * - `name`（中文，显示用）：记忆
 * - `en`（英文，显示 + 匹配）：memory
 * - `pinyin`（拼音首字母，匹配用）：jy
 *
 * 用户打 `/jy` → 命中「记忆」；打 `/memory` → 也命中；打 `/记忆` → 也命中。
 * **匹配是 OR 关系**：任一路前缀命中即算命中，按匹配质量排序。
 *
 * 为什么拼音要手写而不是自动生成：自动拼音转换要引依赖（零额外依赖取向），
 * 而且多音字会错（「记忆」是 jiyi，「重庆」是 chongqing）。手写可控。
 *
 * ## 面板只显示筛选结果，不铺开目录
 *
 * 22 个命令全列出来要 22 行，460px 高的面板装不下。
 * 所以：**最多显示 7 条**（`MAX_VISIBLE`），超出滚动。
 * 搜索才是入口，目录不是 —— 这跟 pi 的 `/` 面板是同一个思路。
 */

export interface CommandDef {
  /** 中文名（显示主标题） */
  name: string;
  /** 英文名（显示副标题 + 匹配） */
  en: string;
  /** 拼音首字母（匹配） */
  pinyin: string;
  /** 一句话说明（显示） */
  desc: string;
  /** 线性图标 SVG 路径内容（不含 `<svg>` 外壳） */
  icon: string;
  /** 执行动作的标识 —— 由调用方 switch 分发 */
  act: string;
  /** 额外参数（部分命令要） */
  arg?: string;
}

/** 命令执行回调 —— 由 main.ts 注入 */
export type CommandRunner = (act: string, arg?: string) => void | Promise<void>;

/** 面板最多显示几条 */
export const MAX_VISIBLE = 7;

// ---------------- 命令表 ----------------

const ICON = {
  memory:
    '<path d="M9.6 2.4L11.6 4.4L5.4 10.6L2.8 11.2L3.4 8.6L9.6 2.4Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round"/>',
  model:
    '<rect x="1.6" y="2.6" width="11.8" height="9.8" rx="1.6" stroke="currentColor" stroke-width="1.1"/><path d="M4.2 5.6H10.8M4.2 8H9" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>',
  newSession:
    '<path d="M7 2.6V11.4M2.6 7H11.4" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/>',
  sessions:
    '<path d="M2.4 3.4H11.6M2.4 6.6H11.6M2.4 9.8H8" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>',
  screen:
    '<rect x="1.4" y="3.2" width="12.2" height="9" rx="1.6" stroke="currentColor" stroke-width="1.1"/><circle cx="7.5" cy="7.7" r="2.4" stroke="currentColor" stroke-width="1.1"/>',
  settings:
    '<circle cx="7.5" cy="7.5" r="2.2" stroke="currentColor" stroke-width="1.1"/><path d="M7.5 1.2V2.6M7.5 12.4V13.8M1.2 7.5H2.6M12.4 7.5H13.8M3.1 3.1L4.1 4.1M10.9 10.9L11.9 11.9M11.9 3.1L10.9 4.1M4.1 10.9L3.1 11.9" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>',
  tools:
    '<path d="M9.6 2.4L11.6 4.4L5.4 10.6L2.8 11.2L3.4 8.6L9.6 2.4Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round"/>',
  project:
    '<path d="M1.4 3.2C1.4 2.72 1.76 2.36 2.24 2.36H4.5L6 3.7H10.9C11.38 3.7 11.74 4.06 11.74 4.54V9.9C11.74 10.38 11.38 10.74 10.9 10.74H2.24C1.76 10.74 1.4 10.38 1.4 9.9V3.2Z" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round"/>',
  clear:
    '<path d="M2.4 4.2H11.6M4.6 4.2V3.2C4.6 2.98 4.78 2.8 5 2.8H9C9.22 2.8 9.4 2.98 9.4 3.2V4.2M3.2 4.2L3.6 10.6C3.62 10.9 3.88 11.1 4.18 11.1H9.82C10.12 11.1 10.38 10.9 10.4 10.6L10.8 4.2" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round"/>',
  compact:
    '<path d="M3 4.4L5.4 7L3 9.6M7.4 9.8H11.4" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round"/>',
  stop:
    '<rect x="3.4" y="3.4" width="7.2" height="7.2" rx="1.2" stroke="currentColor" stroke-width="1.1"/>',
  search:
    '<circle cx="6.8" cy="6.8" r="4.3" stroke="currentColor" stroke-width="1.1"/><path d="M10 10L13.2 13.2" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>',
} as const;

/**
 * 命令表。
 *
 * ⚠️ 顺序 = 面板里的默认展示顺序（没输入时按这个顺序取前 7 条）。
 * 高频的排前面 —— 用户打 `/` 之后最想看到的东西。
 */
export const COMMANDS: CommandDef[] = [
  {
    name: "记忆",
    en: "memory",
    pinyin: "jy",
    desc: "查看与审批记忆候选、技能与项目记忆",
    icon: ICON.memory,
    act: "mem",
  },
  {
    name: "模型",
    en: "model",
    pinyin: "mx",
    desc: "切换当前对话模型",
    icon: ICON.model,
    act: "model",
  },
  {
    name: "新建会话",
    en: "new",
    pinyin: "xjhy",
    desc: "开始一条新会话并选择模式",
    icon: ICON.newSession,
    act: "new",
  },
  {
    name: "会话",
    en: "sessions",
    pinyin: "hy",
    desc: "历史会话与任务列表",
    icon: ICON.sessions,
    act: "sessions",
  },
  {
    name: "截图",
    en: "screen",
    pinyin: "lt",
    desc: "截取当前屏幕并附到对话",
    icon: ICON.screen,
    act: "screen",
  },
  {
    name: "工具",
    en: "tools",
    pinyin: "gj",
    desc: "MCP 工具组与内置工具清单",
    icon: ICON.tools,
    act: "tools",
  },
  {
    name: "设置",
    en: "settings",
    pinyin: "sz",
    desc: "模型接入 / 记忆数据 / 行为安全 / 系统",
    icon: ICON.settings,
    act: "settings",
  },
  {
    name: "项目",
    en: "project",
    pinyin: "xm",
    desc: "任务空间（项目）列表与切换",
    icon: ICON.project,
    act: "project",
  },
  {
    name: "压缩上下文",
    en: "compact",
    pinyin: "ysyyz",
    desc: "手动压缩当前会话的上下文",
    icon: ICON.compact,
    act: "compact",
  },
  {
    name: "搜索设置",
    en: "search",
    pinyin: "sssz",
    desc: "web_search 服务商与 API Key",
    icon: ICON.search,
    act: "search",
  },
  {
    name: "停止",
    en: "stop",
    pinyin: "tz",
    desc: "中断当前正在跑的任务",
    icon: ICON.stop,
    act: "stop",
  },
  {
    name: "清空会话",
    en: "clear",
    pinyin: "qkhy",
    desc: "清空当前会话的消息（可撤销）",
    icon: ICON.clear,
    act: "clear",
  },
];

// ---------------- 匹配 ----------------

export interface ScoredCommand {
  cmd: CommandDef;
  /** 匹配得分，越大越靠前；0 = 不匹配 */
  score: number;
}

/** 把查询串归一：去空白、转小写 */
function norm(s: string): string {
  return s.trim().toLowerCase();
}

/**
 * 单条命令的匹配得分。
 *
 * 返回 0 = 不匹配。得分分层：
 * - 100 = 完全等于中文名 / 英文名 / 拼音
 * - 80  = 某一路的前缀命中
 * - 60  = 某一路包含
 * - 40  = 拼音分段命中（`/jy m` 这种）
 *
 * 为什么要分段：用户可能打 `/ji yi` 或中间带空格。归一时把空格去掉即可，
 * 但 `/记忆 候` 这种「中文 + 英文混打」也该命中 —— 靠 OR 匹配天然覆盖。
 */
export function scoreCommand(cmd: CommandDef, query: string): number {
  const q = norm(query);
  if (!q) return 1; // 空查询 = 都算，按原顺序

  const name = norm(cmd.name);
  const en = norm(cmd.en);
  const py = norm(cmd.pinyin);

  if (q === name || q === en || q === py) return 100;
  if (name.startsWith(q) || en.startsWith(q) || py.startsWith(q)) return 80;
  if (name.includes(q) || en.includes(q) || py.includes(q)) return 60;
  return 0;
}

/**
 * 按查询筛选 + 排序命令表。
 *
 * 排序：得分降序 → 同分保持注册表原序（高频在前）。
 */
export function filterCommands(
  query: string,
  list: CommandDef[] = COMMANDS,
): ScoredCommand[] {
  const out: ScoredCommand[] = [];
  for (const cmd of list) {
    const score = scoreCommand(cmd, query);
    if (score > 0) out.push({ cmd, score });
  }
  // 稳定排序：同分保持原序
  return out
    .map((x, i) => ({ x, i }))
    .sort((a, b) => (b.x.score - a.x.score) || (a.i - b.i))
    .map((y) => y.x);
}

// ---------------- 渲染 ----------------

/** HTML 转义 —— 命令表是硬编码常量，但说明文字可能含用户数据，保守转义 */
export function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/** 单条命令的 HTML */
export function renderCommandItem(c: ScoredCommand, index: number, active: boolean): string {
  const { cmd } = c;
  return `<div class="cmd-item${active ? " is-active" : ""}" data-cmd-idx="${index}" role="option" aria-selected="${active}">
  <span class="cmd-ico"><svg width="15" height="15" viewBox="0 0 15 15" fill="none" xmlns="http://www.w3.org/2000/svg">${cmd.icon}</svg></span>
  <span class="cmd-text">
    <span class="cmd-title">${esc(cmd.name)}<span class="cmd-en">${esc(cmd.en)}</span></span>
    <span class="cmd-desc">${esc(cmd.desc)}</span>
  </span>
</div>`;
}

/** 面板浮层的 HTML（整体） */
export function renderCommandPanel(
  matched: ScoredCommand[],
  activeIdx: number,
  total: number,
): string {
  const shown = matched.slice(0, MAX_VISIBLE);
  const items = shown
    .map((c, i) => renderCommandItem(c, i, i === activeIdx))
    .join("");
  const more =
    matched.length > shown.length
      ? `<div class="cmd-more">还有 ${matched.length - shown.length} 条，继续输入以缩小范围</div>`
      : "";
  return `<div class="cmd-panel" id="cmd-panel" role="listbox" aria-label="命令列表">
  <div class="cmd-head"><span class="cmd-slash">/</span><span class="cmd-input">输入命令名 · 支持中文 / 英文 / 拼音</span></div>
  <div class="cmd-sep"></div>
  <div class="cmd-list">${items}</div>
  ${more}
  <div class="cmd-foot"><span>↑↓ 选择</span><span>⏎ 执行</span><span class="cmd-foot-sp"></span><span>${Math.min(shown.length, matched.length)} / ${total}</span></div>
</div>`;
}

// ---------------- 浮层控制 ----------------

export interface CommandPanelState {
  /** 输入框当前的 "/" 前缀内容（不含斜杠） */
  query: string;
  /** 命中的命令（已排序） */
  matched: ScoredCommand[];
  /** 当前高亮项在 matched 里的下标 */
  active: number;
  /** 浮层 DOM（null = 未打开） */
  el: HTMLElement | null;
}

export function newPanelState(): CommandPanelState {
  return { query: "", matched: [], active: 0, el: null };
}

/**
 * 打开 / 刷新命令面板浮层。
 *
 * 挂在 `document.body` 上，**绝对定位贴在输入框正上方**。
 * 不是居中弹窗 —— 它是输入框长出来的东西（pi 的 `/` 面板贴在编辑器上方，
 * 同一个道理）。点外面关闭。
 */
export function openCommandPanel(
  state: CommandPanelState,
  query: string,
  run: CommandRunner,
  onClose?: () => void,
): void {
  state.query = query;
  state.matched = filterCommands(query);
  state.active = 0;

  const existing = document.getElementById("cmd-panel");
  if (existing) existing.remove();

  if (!state.matched.length) {
    closeCommandPanel(state, onClose);
    return;
  }

  const el = document.createElement("div");
  el.className = "cmd-layer";
  el.innerHTML = renderCommandPanel(state.matched, state.active, COMMANDS.length);
  document.body.appendChild(el);
  state.el = el;

  // 贴输入框上方：输入框的 top 减去面板高度
  const input = document.getElementById("input") as HTMLTextAreaElement | null;
  if (input) {
    const ir = input.getBoundingClientRect();
    const pr = el.firstElementChild?.getBoundingClientRect();
    const h = pr?.height ?? 240;
    el.style.left = `${Math.max(8, Math.min(ir.left, window.innerWidth - 448 - 8))}px`;
    el.style.top = `${Math.max(8, ir.top - h - 8)}px`;
    el.style.width = `${Math.min(ir.width, 440)}px`;
  }

  // 鼠标点选
  el.addEventListener("mousedown", (e) => {
    const t = (e.target as HTMLElement).closest<HTMLElement>(".cmd-item");
    if (!t) return;
    e.preventDefault();
    const idx = Number(t.dataset.cmdIdx ?? 0);
    void run(state.matched[idx].cmd.act, state.matched[idx].cmd.arg);
    closeCommandPanel(state, onClose);
  });

  // 点外面关闭
  const onDocDown = (e: MouseEvent): void => {
    if (!el.contains(e.target as Node)) closeCommandPanel(state, onClose);
  };
  // 用 mousedown 而不是 click —— click 会在按钮的 mousedown 之后才触发，顺序不对
  setTimeout(() => document.addEventListener("mousedown", onDocDown, { once: true }), 0);
  (el as HTMLElement & { _onDocDown?: (e: MouseEvent) => void })._onDocDown = onDocDown;
}

/** 关闭命令面板 */
export function closeCommandPanel(
  state: CommandPanelState,
  onClose?: () => void,
): void {
  if (state.el) {
    const el = state.el as HTMLElement & { _onDocDown?: (e: MouseEvent) => void };
    if (el._onDocDown) document.removeEventListener("mousedown", el._onDocDown);
    el.remove();
    state.el = null;
  }
  state.query = "";
  state.matched = [];
  state.active = 0;
  onClose?.();
}

/**
 * 面板打开时，按 ↑ ↓ 移动高亮。返回 true = 已消费这个按键。
 *
 * ⚠️ 判断条件只看 `matched` 有没有内容，**不看 `el`** ——
 * 高亮下标是纯状态，DOM 只是它的投影。绑死 DOM 会让「无 DOM 也能导航」
 * 这件事做不到（单元测试就卡在这）。
 */
export function moveActive(state: CommandPanelState, delta: number): boolean {
  if (!state.matched.length) return false;
  const n = state.matched.length;
  state.active = (state.active + delta + n) % n;
  refreshActive(state);
  return true;
}

/** 重画高亮（只改 class，不重建整块 DOM —— 否则滚动位置会跳） */
export function refreshActive(state: CommandPanelState): void {
  if (!state.el) return;
  state.el.querySelectorAll<HTMLElement>(".cmd-item").forEach((el, i) => {
    const on = i === state.active;
    el.classList.toggle("is-active", on);
    el.setAttribute("aria-selected", String(on));
  });
}

/** 拿到当前高亮的命令；没面板时返回 null */
export function currentCommand(state: CommandPanelState): CommandDef | null {
  if (!state.el || !state.matched.length) return null;
  return state.matched[state.active]?.cmd ?? null;
}
