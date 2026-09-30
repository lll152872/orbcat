/**
 * 格式化与通用小工具 —— **纯函数，零模块状态、零 Tauri 依赖**。
 *
 * ## 为什么单独成模块（2026-09-30）
 *
 * `main.ts` 单体已经涨到 6300+ 行，而它内部结构其实比看上去好：
 * 154 个顶层具名函数、只有 61 个模块级绑定。也就是说**大部分代码是函数**，
 * 问题是全挤在一个文件里，找一个 `fmtWhen` 要翻六千行。
 *
 * 拆分策略是**只搬零状态依赖的**（判定方式：函数体不引用任何模块级绑定、
 * 不碰 `invoke`/`listen`）。这样搬出去是纯平移，行为不可能变 ——
 * 对 UI 主文件来说这条纪律很重要：行为一变，回归很难查。
 *
 * ⚠️ 这里的函数**不许**引用 `document` / `window`（`isTypingInInput` 是唯一
 *    例外，它读 `document.activeElement`，但因为它同样零模块状态、且
 *    属于"通用环境判断"而不是某个视图的逻辑，放在这里比留在 6000 行里好找）。
 */

/** HTML 转义（所有拼接 innerHTML 的地方都必须过它） */
export function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/**
 * 转义 + 允许 `**粗体**`。
 *
 * 为什么需要：`agent-data/modes.json` 是**用户手写**的，里面的模式说明天然
 * 会写 markdown。直接 `esc()` 上屏的话，那些星号会被原样画出来
 * （截图里 PTC 模式那句就是「把**全部 MCP 外部工具组**直接给模型」）。
 *
 * ⚠️ 必须在 `esc()` **之后**再换 `<b>`：否则用户写的 `<script>` 会先被
 * 当成粗体标记处理，反而绕过了转义。这里的替换只认 `**…**`，不认任何 HTML。
 */
export function escMd(s: string): string {
  return esc(s).replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>");
}

/** token 数量的可读形式（1234 → 1.2k，12345 → 12k，1234567 → 1.2M） */
export function fmtTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) {
    const k = n / 1000;
    return (k >= 10 ? k.toFixed(0) : k.toFixed(1)) + "k";
  }
  return (n / 1_000_000).toFixed(1) + "M";
}

/** 文件大小的人类可读形式 */
export function fmtBytes(n: number): string {
  if (!n) return "0 B";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/**
 * epoch 毫秒 → `YYYY-MM-DD HH:MM`（**完整日期**）。
 *
 * 为什么手写而不是 `toLocaleString`：后端所有时间戳都是 epoch 毫秒，
 * 前端自己格式化才能保证跨机器一致（`toLocaleString` 受系统区域设置影响，
 * 同一个时间在不同机器上长得不一样，截图对不上）。
 */
export function fmtWhen(ms: number): string {
  if (!ms) return "（时间未知）";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(
    d.getMinutes(),
  )}`;
}

/** epoch 毫秒 → `M/D HH:MM`（**短格式**，列表里省地方） */
export function fmtTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}/${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/**
 * 审计时间戳 → 本地时间串。
 *
 * ⚠️ 审计链存的是**秒**（`command_policy` 侧口径），而项目其余时间戳是**毫秒**。
 * 这里按数量级判别（> 1e12 视为毫秒），所以两种都能吃 —— 别改成只认一种，
 * 老审计记录会整体显示成 1970 年。
 */
export function fmtAuditTime(ts: number): string {
  const ms = ts > 1e12 ? ts : ts * 1000;
  try {
    return new Date(ms).toLocaleString();
  } catch {
    return String(ts);
  }
}

/** 路径尾部（列表里省地方，完整路径放 title） */
export function tailPath(p: string, keep = 2): string {
  if (!p) return "";
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.slice(-keep).join("\\");
}

/** 取路径最后一段（切片按钮要短，不能和上方路径重复） */
export function leafName(p: string): string {
  const s = p.replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("\\"), s.lastIndexOf("/"));
  return i >= 0 ? s.slice(i + 1) : s;
}

/** URL → host（拿不到就用正则剥协议；模型分组默认名用它） */
export function shortHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url.replace(/^https?:\/\//, "").split("/")[0] || url;
  }
}

/** epoch 毫秒 → 本地日期 `YYYY-MM-DD`（时区只有前端知道，所以后端不干这事） */
export function localDateKey(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** 追加流式增量，超上限就停住（不再增长，避免每一帧都在拼超长字符串） */
export function capAppend(cur: string | undefined, chunk: string, limit: number): string {
  const base = cur ?? "";
  if (base.length >= limit) return base;
  const s = base + chunk;
  return s.length <= limit ? s : `${s.slice(0, limit)}\n…（显示已截断）`;
}

/**
 * 剥掉正文里混入的工具轨迹/思考尾巴。
 *
 * ⚠️ 必须与后端 `sessions::strip_step_folds` 的标记表**保持对齐**：
 * 前端剥一遍只是老数据 + 模型回显的兜底，后端落盘时已经剥过一次。
 * 两边不一致的表现是"同一段回答，刷新前后长得不一样"。
 */
export function stripStepFolds(text: string): string {
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

/**
 * 工具参数摘要：优先取最有信息量的字段，取不到就原样截断。
 *
 * ⚠️ 落盘时 detail 可能被后端截断（不再保证是合法 JSON），所以解析失败要
 * 安静地退回原串 —— 中文路径 + 截断很容易把 JSON 切断。
 */
export function summarizeArgs(detail: string): string {
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

/** 当前焦点在输入框里吗？在的话键盘不接管卡片（防"打字回车"误批权限） */
export function isTypingInInput(): boolean {
  const a = document.activeElement;
  return a instanceof HTMLTextAreaElement || a instanceof HTMLInputElement;
}
