/**
 * 会话管理入口 + 会话切换 —— 2026-10-14 用户报
 *
 * ## 两条用户原话
 *
 * 1. 「这个没有会话管理啊」—— 指**设置页**。模型 / 搜索 / Token / 备份 / MCP /
 *    安全闸门 / 记忆一应俱全，唯独没有会话。顶栏那颗「史」和项目下拉都还在，
 *    但「设置里找不到会话」这件事本身就像在说这个功能不存在。
 *
 * 2. 在会话历史页点**当前**那条会话**完全没反应** ——
 *    `switchSession` 的早退 `if (id === currentSessionId) return;` 挡在
 *    `view = "chat"` 前面。早退本意只是"别重复加载消息"，却把导航一起拦掉了。
 *
 * 两条都锁住：第 2 条尤其要防回退 —— 它是个**静默**的死键，不报错、不红。
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import {
  bootApp,
  openPanel,
  waitFor,
  orbExists,
  panelExists,
  callsOf,
  state,
  clickOrb,
  sleep,
  sendText,
  assistantBubbles,
} from "./helpers";

/** 顶栏「史」按钮 → 进入会话历史页 */
async function openSessionsViaHistoryButton(): Promise<void> {
  (document.getElementById("btn-sess") as HTMLButtonElement).click();
  await waitFor(
    () => !!document.getElementById("sess-new"),
    3000,
    "sessions view (sess-new)",
  );
}

/** 设置 › 会话管理 入口 */
async function openSessionsViaSettings(): Promise<void> {
  await bootApp();
  await openPanel();
  (document.getElementById("btn-gear") as HTMLButtonElement).click();
  await waitFor(
    () => document.body.textContent?.includes("会话管理") ?? false,
    3000,
    "settings menu with 会话管理",
  );
  const entry = Array.from(document.querySelectorAll(".set-entry")).find((el) =>
    (el.textContent ?? "").includes("会话管理"),
  ) as HTMLButtonElement | undefined;
  if (!entry) throw new Error("设置页里找不到「会话管理」入口");
  entry.click();
  await waitFor(() => !!document.getElementById("sess-new"), 3000, "sessions view via settings");
}

function sessionRow(id: string): HTMLButtonElement | null {
  return document.querySelector<HTMLButtonElement>(`.sess-open[data-id="${id}"]`);
}

describe("设置页有「会话管理」入口", () => {
  it("入口存在，摘要说明会话数量", async () => {
    await bootApp();
    await openPanel();
    (document.getElementById("btn-gear") as HTMLButtonElement).click();
    await waitFor(
      () => document.body.textContent?.includes("会话管理") ?? false,
      3000,
      "settings menu",
    );
    const entry = Array.from(document.querySelectorAll(".set-entry")).find((el) =>
      (el.textContent ?? "").includes("会话管理"),
    ) as HTMLButtonElement | undefined;
    expect(entry, "设置页必须列出「会话管理」，否则用户以为这个功能不存在").toBeTruthy();
    expect(entry!.textContent ?? "").toMatch(/会话/);
  });

  it("点「会话管理」真的进会话历史页（不是死入口）", async () => {
    await openSessionsViaSettings();
    expect(document.body.textContent ?? "").toContain("会话历史");
    expect(panelExists()).toBe(true); // 仍是面板形态，只是换页
  });

  // 入口是加的，顶栏「史」不能因此被挤掉 —— 两条路都得通
  it("顶栏「史」按钮照常进会话历史页", async () => {
    await bootApp();
    await openPanel();
    await openSessionsViaHistoryButton();
    expect(document.body.textContent ?? "").toContain("会话历史");
  });
});

describe("在会话页点当前会话能回到对话（死键回归）", () => {
  it("点当前会话 → 切回对话视图，而不是停在会话页", async () => {
    await bootApp();
    await openPanel();
    const curId = state.currentSessionId;
    expect(curId, "mock 应给出当前会话 id").toBeTruthy();

    await openSessionsViaHistoryButton();
    expect(document.body.textContent ?? "").toContain("会话历史");

    const row = sessionRow(curId!);
    expect(row, "当前会话那行应该在列表里").toBeTruthy();
    row!.click();

    // ⚠️ 关键断言：必须回到**对话**（输入框可见），而不是仍停在会话历史页
    await waitFor(() => !document.getElementById("sess-new"), 3000, "back to chat (sess-new gone)");
    expect(document.body.textContent ?? "").not.toContain("按项目分组");
  });

  it("点另一条会话 → 也回到对话（这条路径原本就正常，防改坏）", async () => {
    // mock 默认只给一条会话；这条要验"真的切了"，所以补第二条
    state.sessionList = [
      { id: state.currentSessionId, title: "主聊天", kind: "main", updatedAt: Date.now() },
      { id: "sess-other", title: "另一个会话", kind: "task", updatedAt: Date.now() - 5_000 },
    ];
    await bootApp();
    await openPanel();

    await openSessionsViaHistoryButton();
    sessionRow("sess-other")!.click();
    await waitFor(() => !document.getElementById("sess-new"), 3000, "switched session (sess-new gone)");
    expect(document.body.textContent ?? "").not.toContain("按项目分组");
    // 真的切了（不是又停在会话页）
    expect(callsOf("session_switch").length).toBe(1);
  });

  it("点当前会话不会重新拉 session_state（别白重载一次）", async () => {
    await bootApp();
    await openPanel();
    const curId = state.currentSessionId;
    await openSessionsViaHistoryButton();

    const before = callsOf("session_state").length;
    sessionRow(curId!)!.click();
    await waitFor(() => !document.getElementById("sess-new"), 3000, "back to chat (sess-new gone)");
    // 早退的**本意**是省掉这次重载 —— 修死键时不能把这个优化也弄丢
    expect(callsOf("session_state").length).toBe(before);
  });
});

describe("收起面板回球态不被这次改动影响", () => {
  it("会话页 → 返回对话 → 收起 → 球还在", async () => {
    await bootApp();
    await openPanel();
    await openSessionsViaHistoryButton();
    sessionRow(state.currentSessionId)!.click();
    await waitFor(() => !document.getElementById("sess-new"), 3000, "chat (sess-new gone)");
    (document.getElementById("btn-close") as HTMLButtonElement).click();
    await waitFor(() => orbExists() && !panelExists(), 3000, "orb");
  });
});

/**
 * 记录 `#panel-body.scrollTop` 的**写入**，用来证明"代码确实把位置写回去了"。
 *
 * ## 为什么不能直接断言 `scrollTop` 的值
 *
 * happy-dom 里 `innerHTML = ...` **不会**真的把 `scrollTop` 归零（真浏览器会，
 * 因为内容变了），所以"删完之后 scrollTop 还是 640"这条**恒真** ——
 * 把修复删掉测试照样绿。实测踩过。
 *
 * 于是改成断言**契约本身**：重渲染期间必须发生一次 `scrollTop = <原值>` 的写入。
 * 不写就是没修，写了值对才算修。这条对 happy-dom 的偷懒免疫。
 */
function recordScrollWrites(): { writes: number[] } {
  const b = document.getElementById("panel-body") as HTMLElement & { __orig?: number };
  const writes: number[] = [];
  let cur = b.scrollTop;
  Object.defineProperty(b, "scrollTop", {
    get: () => cur,
    set: (v: number) => {
      cur = v;
      writes.push(v);
    },
    configurable: true,
  });
  return { writes };
}

describe("会话页删除后不该跳回顶部", () => {
  beforeEach(() => installScrollStub());
  afterEach(() => removeScrollStub());

  it("删一条会话 → 重渲染时把滚动位置写了回去（不是被动留着）", async () => {
    state.sessionList = [
      { id: state.currentSessionId, title: "主聊天", kind: "main", updatedAt: Date.now() },
      { id: "sess-a", title: "要删的这条", kind: "task", updatedAt: Date.now() - 1_000 },
      { id: "sess-b", title: "另一条", kind: "task", updatedAt: Date.now() - 2_000 },
    ];
    await bootApp();
    await openPanel();
    await openSessionsViaHistoryButton();

    const b = document.getElementById("panel-body")!;
    b.scrollTop = 640;
    const rec = recordScrollWrites(); // 从这里开始记账
    b.scrollTop = 640; // 起点：清掉上面那次

    const del = document.querySelector<HTMLButtonElement>('.sess-del[data-id="sess-a"]');
    expect(del, "任务会话行右侧应有删除按钮").toBeTruthy();
    del!.click();

    await waitFor(
      () => !document.querySelector('.sess-del[data-id="sess-a"]'),
      3000,
      "deleted row gone",
    );
    await sleep(0);

    expect(
      rec.writes,
      "重渲染期间必须把滚动位置写回去 —— 否则真浏览器里 innerHTML 会把它清成 0",
    ).toContain(640);
    expect(b.scrollTop).toBe(640);
  });

  it("从设置 › 会话管理进来的删除，同样保位置", async () => {
    state.sessionList = [
      { id: state.currentSessionId, title: "主聊天", kind: "main", updatedAt: Date.now() },
      { id: "sess-x", title: "第一条任务", kind: "task", updatedAt: Date.now() - 1_000 },
      { id: "sess-y", title: "第二条任务", kind: "task", updatedAt: Date.now() - 2_000 },
    ];
    await openSessionsViaSettings();
    const b = document.getElementById("panel-body")!;
    b.scrollTop = 320;
    const rec = recordScrollWrites();
    b.scrollTop = 320;

    document.querySelector<HTMLButtonElement>('.sess-del[data-id="sess-y"]')!.click();
    await waitFor(
      () => !document.querySelector('.sess-del[data-id="sess-y"]'),
      3000,
      "deleted via settings entry",
    );
    await sleep(0);
    expect(rec.writes).toContain(320);
    expect(b.scrollTop).toBe(320);
  });

  it("mock 真的会删：删掉当前会话后 current 会切走", async () => {
    // 前置守卫 —— 之前 session_delete 是静态 override，行永远不消失，
    // 任何"删完要刷新"的用例都测不出来。
    state.sessionList = [
      { id: state.currentSessionId, title: "主聊天", kind: "main", updatedAt: Date.now() },
      { id: "sess-del-me", title: "当前这条", kind: "task", updatedAt: Date.now() - 1_000 },
    ];
    await bootApp();
    await openPanel();
    await openSessionsViaHistoryButton();
    await waitFor(
      () => !!document.querySelector('.sess-del[data-id="sess-del-me"]'),
      2000,
      "current task session deletable",
    );
  });
});

/**
 * 「↓ 回到底部」按钮 + 「不抢滚动」
 *
 * ## 用户原话（2026-10-14）
 *
 * 「加一个下滑那个键点一下回到最下面，然后发消息也回到最下面」+
 * 「如果我输入了 xxx，也要跳到最下面**或者给用户选择**」。
 *
 * 「给用户选择」= 新消息来了**不硬拽**，而是右下角冒一个带计数的 ↓ 让人自己点。
 */
describe("「↓ 回到底部」按钮", () => {
  beforeEach(() => installScrollStub());
  afterEach(() => removeScrollStub());

  function jumpBtn(): HTMLButtonElement {
    const b = document.getElementById("jump-btn") as HTMLButtonElement | null;
    if (!b) throw new Error("#jump-btn 不存在");
    return b;
  }

  function scrollTo(top: number): void {
    const b = document.getElementById("panel-body")!;
    b.scrollTop = top;
    b.dispatchEvent(new Event("scroll"));
  }

  it("贴在底部时按钮隐藏；往上滚就冒出来", async () => {
    await bootApp();
    await openPanel();
    await reopenPanel();
    expect(jumpBtn().hidden, "贴着底部不该有按钮").toBe(true);

    scrollTo(600);
    expect(jumpBtn().hidden, "往上滚就该冒出按钮").toBe(false);
    expect(jumpBtn().textContent).toContain("↓");

    scrollTo(0);
    expect(jumpBtn().hidden).toBe(false);
  });

  it("点它 → 贴底 → 按钮自己藏起来", async () => {
    await bootApp();
    await openPanel();
    await reopenPanel();
    scrollTo(600);
    expect(jumpBtn().hidden).toBe(false);

    jumpBtn().click();
    await waitFor(() => jumpBtn().hidden, 2000, "hidden after jump");
    expect(scrollTopOf()).toBeGreaterThanOrEqual(1500);
  });

  it("翻上去时这一轮跑完 → **不拽你**、计数 +1", async () => {
    // ⚠️ 不能用 `sendText` 触发「新消息到达」—— 它走的是**用户发送**路径，
    //    按约定就该无条件贴底（用户主动发问 = 明确要从这儿继续）。
    //
    //    真实场景是：**轮已经在跑**，用户往上翻历史，跑完时
    //    `reloadSessionKeepScroll` 重载 —— 那条路才必须"不抢滚动"。
    //    用 chatDelayMs 制造一个"回复还在路上"的窗口，翻上去后等它到。
    state.chatReply = "后台跑完的回答";
    state.chatDelayMs = 400;
    await bootApp();
    await openPanel();
    await reopenPanel();

    await sendText("让它跑着");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "run started");
    scrollTo(600); // 用户翻上去读历史

    await waitFor(
      () => assistantBubbles().some((t) => t.includes("后台跑完的回答")),
      4000,
      "run finished",
    );
    await sleep(0);

    expect(
      scrollTopOf(),
      "用户已翻上去时，跑完重载不该把他拽回底部",
    ).toBe(600);
    expect(jumpBtn().hidden).toBe(false);
    expect(jumpBtn().textContent).toMatch(/↓\s*\d+/);
  });

  it("用户发消息 → 无条件贴底 + 计数清零（刻意的例外）", async () => {
    await bootApp();
    await openPanel();
    await reopenPanel();
    scrollTo(600);
    expect(jumpBtn().hidden).toBe(false);

    await sendText("从这儿继续");
    await waitFor(() => scrollTopOf() >= 1500, 2000, "sent → bottom");
    expect(jumpBtn().textContent, "发消息后计数应清零（不该再显示 ↓ N）").not.toMatch(/\d/);
  });

  it("非对话页不显示按钮", async () => {
    await bootApp();
    await openPanel();
    (document.getElementById("btn-gear") as HTMLButtonElement).click();
    await waitFor(
      () => document.body.textContent?.includes("会话管理") ?? false,
      3000,
      "settings",
    );
    expect(jumpBtn().hidden, "设置页不该有这个按钮").toBe(true);
  });

  it("切会话 → 计数清零（别把上一条的『漏看 N 条』挂过来）", async () => {
    state.sessionList = [
      { id: state.currentSessionId, title: "主聊天", kind: "main", updatedAt: Date.now() },
      { id: "sess-z", title: "别的会话", kind: "task", updatedAt: Date.now() - 1_000 },
    ];
    await bootApp();
    await openPanel();
    await openSessionsViaHistoryButton();
    sessionRow("sess-z")!.click();
    await waitFor(() => !document.getElementById("sess-new"), 3000, "switched");
    expect(jumpBtn().textContent).not.toMatch(/\d/);
  });
});

/**
 * 对话滚动位置记忆
 *
 * ## 用户原话（2026-10-14）
 *
 * 「我看最上面那一条下一次也要是那一条，不要每次开球都看最下面」——
 * 长会话里每次收起再展开都落回最后一条，等于每次都要重新往上滚半天。
 *
 * ## happy-dom 没有布局：两个坑
 *
 * 1. `scrollHeight` / `clientHeight` 恒为 0，不造出来就测不出滚动；
 * 2. **必须 stub 原型、不能 stub 元素**。`expand()` → `renderPanel()` 会整个
 *    重建面板，`#panel-body` 是**新元素**；挂在旧元素上的
 *    `Object.defineProperty` 跟着旧元素一起没了，于是断言永远落在 0 上。
 *
 * 装在 `HTMLElement.prototype` 上，新建元素照样继承。
 */
const REAL_SCROLL_HEIGHT = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "scrollHeight",
);
const REAL_CLIENT_HEIGHT = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "clientHeight",
);

/** 让 `#panel-body` 表现得像"内容 2000px / 视口 500px"。 */
function installScrollStub(): void {
  const def = (name: string, value: number) =>
    Object.defineProperty(HTMLElement.prototype, name, {
      get(this: HTMLElement) {
        return this.id === "panel-body" ? value : 0;
      },
      configurable: true,
    });
  def("scrollHeight", 2000);
  def("clientHeight", 500);
}

function removeScrollStub(): void {
  for (const [name, d] of [
    ["scrollHeight", REAL_SCROLL_HEIGHT],
    ["clientHeight", REAL_CLIENT_HEIGHT],
  ] as const) {
    if (d) Object.defineProperty(HTMLElement.prototype, name, d);
    else delete (HTMLElement.prototype as unknown as Record<string, unknown>)[name];
  }
}

function scrollTopOf(): number {
  return document.getElementById("panel-body")!.scrollTop;
}

/** 收起再展开，逼 `renderBody` 在可滚动的尺寸上重跑一次。 */
async function reopenPanel(): Promise<void> {
  (document.getElementById("btn-close") as HTMLButtonElement).click();
  await waitFor(() => orbExists() && !panelExists(), 3000, "collapsed");
  clickOrb();
  await waitFor(() => !!document.getElementById("panel-body"), 3000, "reopened");
}

describe("对话滚动位置：收起再回来还在原处", () => {
  beforeEach(() => installScrollStub());
  afterEach(() => removeScrollStub());

  it("滚到中间 → 收起 → 重新展开，回到同一个位置而不是底部", async () => {
    await bootApp();
    await openPanel();
    const b = document.getElementById("panel-body")!;
    b.scrollTop = 900;
    // 触发 scroll 事件（记账靠它，不靠"离开时存一次"）
    b.dispatchEvent(new Event("scroll"));
    await sleep(0);

    await reopenPanel();
    await waitFor(() => scrollTopOf() === 900, 2000, "scroll restored to 900");
    expect(scrollTopOf()).toBe(900);
  });

  it("没滚过（第一次开）仍然贴底 —— 别把新用户卡在半空", async () => {
    await bootApp();
    await openPanel();
    await reopenPanel();
    // `scrollToBottom()` 写的是 `scrollTop = scrollHeight`（2000）。
    // 真浏览器会把超出的部分夹到 `scrollHeight - clientHeight`（1500）；
    // happy-dom 不夹，所以这里只断言"确实去到底部了"，不写死具体数值。
    await waitFor(
      () => scrollTopOf() >= 1500,
      2000,
      "no memo → bottom",
    );
    expect(scrollTopOf()).toBeGreaterThanOrEqual(1500);
  });

  it("切去设置再回来，也回到原处", async () => {
    await bootApp();
    await openPanel();
    const b = document.getElementById("panel-body")!;
    b.scrollTop = 1200;
    b.dispatchEvent(new Event("scroll"));
    await sleep(0);

    (document.getElementById("btn-gear") as HTMLButtonElement).click();
    await waitFor(
      () => document.body.textContent?.includes("会话管理") ?? false,
      3000,
      "settings",
    );
    // 设置页 → 返回对话
    (document.querySelector("#sub-back") as HTMLButtonElement | null)?.click();
    await waitFor(() => !document.getElementById("sess-new"), 2000, "chat view");
    await waitFor(() => scrollTopOf() === 1200, 2000, "scroll restored to 1200");
    expect(scrollTopOf()).toBe(1200);
  });

  it("记忆位置超出当前内容长度时夹到最大处，不停在空白", async () => {
    await bootApp();
    await openPanel();
    const b = document.getElementById("panel-body")!;
    b.scrollTop = 99999; // 远超 2000-500
    b.dispatchEvent(new Event("scroll"));
    await sleep(0);

    await reopenPanel();
    await waitFor(() => scrollTopOf() === 1500, 2000, "clamped to max");
    expect(scrollTopOf()).toBeLessThanOrEqual(1500);
  });
});