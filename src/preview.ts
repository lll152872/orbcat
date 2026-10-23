/**
 * 预览入口 —— **纯 HTML 渲染件**的启动器。
 *
 * ## 它做什么
 *
 * 1. 按 URL 上的 `?scenario=` 造一份 mock 数据；
 * 2. 把 Tauri 的 `invoke` 指到这份 mock（见 `preview/tauri-shim.ts`）；
 * 3. 把数据源切成 `MockSource`（**一行切**）；
 * 4. 导入真界面 `main.ts` —— 渲染的是**和线上一样的代码**，不是另画一遍；
 * 5. 把场景对应的屏幕打开（点球 → 展开面板 → 切到那一屏）。
 *
 * ## 不做什么
 *
 * - 不装 Tauri runtime、不连后端、不读用户数据；
 * - 不改界面的 DOM / 样式 / 逻辑 —— 聊天区那部分一概没碰。
 */
// 预览外壳样式由 preview.html 用 <link> 引（与正式应用一样走 HTML 引入，
// 免得给 tsconfig 加 CSS 模块声明）
import { setDataSource } from "./data";
import { MockSource } from "./data/mock";
import { SCENARIOS, scenarioById } from "./data/mockdata";
import { setInvokeHandler } from "./preview/tauri-shim";

/** 场景 id（URL 参数 `?scenario=`） */
function currentScenarioId(): string {
  try {
    return new URLSearchParams(location.search).get("scenario") ?? "default";
  } catch {
    return "default";
  }
}

const scenarioId = currentScenarioId();
const scenario = scenarioById(scenarioId);

/** 1) 造 mock 数据 */
const mock = new MockSource(scenarioId);

/** 2) Tauri 的 invoke → mock */
setInvokeHandler((cmd, args) => mock.call(cmd, args));

/** 3) 一行切：数据源换成 mock */
setDataSource(mock);

// ---------------------------------------------------------------- 预览外壳

/** 预览侧栏（场景清单）—— 纯预览用，不属于应用界面 */
function renderHarness(): void {
  const side = document.createElement("aside");
  side.className = "pv-harness";
  const rows = SCENARIOS.map((s) => {
    const active = s.id === scenarioId;
    return `<button class="pv-sc${active ? " on" : ""}" data-s="${s.id}" title="${escAttr(s.desc)}">
      <b>${esc(s.label)}</b><i>${esc(s.desc)}</i>
    </button>`;
  }).join("");
  side.innerHTML = `
    <header>
      <b>orbcat · 纯 HTML 预览</b>
      <i>mock 数据，无 Tauri runtime · 场景由 ?scenario= 决定</i>
    </header>
    <div class="pv-list">${rows}</div>
    <footer>
      <span>当前：<b>${esc(scenario.label)}</b></span>
      <button class="pv-reload" onclick="location.reload()">重载</button>
    </footer>`;
  document.body.appendChild(side);

  side.querySelectorAll<HTMLButtonElement>(".pv-sc").forEach((btn) => {
    btn.addEventListener("click", () => {
      const id = btn.dataset.s ?? "default";
      const u = new URL(location.href);
      u.searchParams.set("scenario", id);
      location.href = u.toString();
    });
  });
}

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
function escAttr(s: string): string {
  return esc(s).replace(/"/g, "&quot;");
}

// ---------------------------------------------------------------- 打开界面

/** 等条件成立（轮询） */
async function waitFor(fn: () => boolean, timeoutMs = 5000, label = "condition"): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (fn()) return;
    await new Promise((r) => setTimeout(r, 30));
  }
  console.warn(`[preview] 等待超时：${label}`);
}

/** 点球：mousedown + mouseup（attachDrag 靠位移判定点击） */
function clickOrb(): void {
  const orb = document.getElementById("orb");
  if (!orb) return;
  orb.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
  orb.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
}

/** 场景 → 视图名（`main.ts` 的 `View` 枚举值） */
const SCENARIO_VIEW: Record<string, string> = {
  chat: "chat",
  sessions: "sessions",
  perm: "perm",
  mem: "mem",
  mcp: "mcp",
  recovery: "recovery",
  project: "project",
  usage: "usage",
  life: "life",
  settings: "settings",
};

async function boot(): Promise<void> {
  renderHarness();

  // 4) 导入真界面（main.ts 在模块顶层 boot）
  const app = await import("./main");

  // 5) 展开面板 → 切到场景对应的那一屏
  await waitFor(() => !!document.getElementById("orb"), 5000, "悬浮球");
  clickOrb();
  await waitFor(() => !!document.getElementById("input"), 5000, "面板");

  const target = SCENARIO_VIEW[scenario.goto ?? "chat"] ?? "chat";
  if (target !== "chat") {
    const sw = (app as unknown as { __previewSwitchView?: (v: string) => Promise<void> })
      .__previewSwitchView;
    if (sw) await sw(target);
  }

  // 状态条/标题条有时是异步补的，等一拍再收尾
  await new Promise((r) => setTimeout(r, 250));
  document.body.dataset.previewReady = "1";
}

void boot();
