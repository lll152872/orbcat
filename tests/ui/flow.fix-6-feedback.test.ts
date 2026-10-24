import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  bootApp,
  openPanel,
  waitFor,
  sleep,
  callsOf,
  state,
  runCommandByName,
  sendText,
} from "./helpers";
import { setMock } from "./mocks/tauri";

/**
 * **用户 2026-10-24 真机反馈的 6 项前端问题**（一次做完）。
 *
 * | # | 用户原话 | 根因 | 断言落在哪一层 |
 * |---|---|---|---|
 * | ① | 「模型下拉列表打不开」（顶栏 + 设置页两处） | 顶栏：菜单挂在 chip **里面**，chip 自己 `overflow:hidden` ⇒ 被自己裁掉；设置页：`#set-model-chip` **压根不存在** ⇒ 直接 early-return | **结构层**（菜单必须在 body 上、不在锚点里） |
 * | ② | 「工作的时候球几乎看不见」 | `@media (prefers-reduced-motion: reduce)` 把动画压成 0.001ms ⇒ 动画立刻结束 ⇒ 醒猫两帧回到基础 `opacity:0`（**只有「工作中」把可见性押在动画上**） | **样式契约层**（awake 帧必须写死 `opacity:1`） |
 * | ③ | 删掉「■ 已停止，本轮已产出的内容保留在上面」 | 用户自己按的停止 ⇒ 那行是纯噪声 | **行为层**（无 reason ⇒ 不插那行；有 reason ⇒ 仍要插） |
 * | ④ | 删掉「有 N 条记忆候选待审批」这类提示 | 用户明确不要这一类 | **行为层**（跑完不出现） |
 * | ⑤ | 「无任务空间」这一组用不了（点不进去 / 不能 new） | 前端 bug：这一格 `latestSessionIn` 恒 undefined（不跳会话）+ 默认折叠 + 徽标误标 + 右键弹浏览器原生菜单 + 建会话只跟「当前激活项目」 | **行为层 + 结构层** |
 * | ⑥ | 会话页要能 new 会话 | 标题条那颗 ＋ 有，但只建**默认模式**的会话、且落别的组；内容区那颗 `hidden` ⇒ 用户找不到 | **行为层 + 结构层** |
 *
 * ⚠️ happy-dom **没有排版引擎** ⇒ 「量不量得出裁切/透明度」这种像素断言注定失败 ⇒
 *    ① ② 落**结构/样式契约层**（断言导致 bug 的那几条 CSS/结构性质）；
 *    ③④⑤⑥ 是行为层，直接可测。
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const SRC = path.resolve(here, "../../src");
const css = readFileSync(path.join(SRC, "styles.css"), "utf8");

/** 取某条 CSS 规则的声明体（`{}` 里那段）。 */
function cssRule(sel: string): string {
  const re = new RegExp(sel.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "\\s*\\{([^}]*)\\}");
  return css.match(re)?.[1] ?? "";
}

/** 按 `data-fold` 找组标题（「无任务空间」那组的 key 是空串）。 */
function sepOf(key: string): HTMLElement {
  const sep = Array.from(document.querySelectorAll<HTMLElement>(".sess-sep")).find(
    (b) => (b.dataset.fold ?? "") === key,
  );
  if (!sep) throw new Error(`找不到组「${key}」的标题`);
  return sep;
}

/** 进会话页。 */
async function openSessionsView(): Promise<void> {
  await runCommandByName("sessions");
  await waitFor(() => !!document.getElementById("sess-new"), 3000, "会话页");
}

// ============================================================ ① 模型下拉

describe("① 模型下拉打不开", () => {
  it("①a 顶栏 chip 点开 ⇒ 下拉挂在 body 上（不在锚点里面 ⇒ 不会被锚点的 overflow:hidden 裁掉）", async () => {
    await bootApp();
    await openPanel();
    const chip = document.getElementById("model-chip")!;
    chip.click();
    await waitFor(() => !!document.getElementById("models-dropdown"), 2000, "下拉出现");
    const dd = document.getElementById("models-dropdown")!;
    expect(document.body.contains(dd), "菜单必须挂在 body 上").toBe(true);
    expect(
      chip.contains(dd),
      "菜单不能挂在锚点里面（锚点自己 overflow:hidden ⇒ 整块被裁 ⇒ 点了像没反应）",
    ).toBe(false);
  });

  it("①b 设置页「当前模型」那行要有下拉锚点 ⇒ 点它也能出下拉", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置页");
    const row = document.querySelector<HTMLElement>('.set-row[data-act="set-model"]')!;
    expect(
      row.querySelector("#set-model-chip"),
      "这行必须有 #set-model-chip 锚点（没有它 paintModelsDropdown 直接 return ⇒ 点开没反应）",
    ).toBeTruthy();
    row.click();
    await waitFor(() => !!document.getElementById("models-dropdown"), 2000, "设置页下拉");
    expect(
      document.body.contains(document.getElementById("models-dropdown")!),
      "设置页的下拉同样要挂在 body 上（.set-sec 是 overflow:hidden ⇒ 挂里面会被裁）",
    ).toBe(true);
  });

  it("①c 菜单挂到 body 之后 ⇒ 点菜单内部不会先被关掉，点条目能选中", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("model-chip")!.click();
    await waitFor(() => !!document.getElementById("models-dropdown"), 2000, "下拉出现");
    const item = document.querySelector<HTMLButtonElement>("#models-dropdown .mc-mi[data-id]")!;
    expect(item, "下拉里要有模型条目").toBeTruthy();
    // 真实点击的前半段：mousedown 会冒泡到 document 的「点外面就关」闸
    item.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await sleep(30);
    expect(document.getElementById("models-dropdown"), "点菜单内部不该把菜单关掉").toBeTruthy();
    item.click();
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "选完关闭");
    expect(callsOf("set_selected_model").length, "选中要真的落库").toBeGreaterThan(0);
  });
});

// ============================================================ ② 工作中球几乎看不见

describe("② 工作中悬浮球几乎看不见", () => {
  it("②a 「工作中」的醒猫帧必须写死 opacity:1（可见性不能只押在动画上）", () => {
    // ⚠️ 为什么这条能拦住 bug：`prefers-reduced-motion: reduce` 会把
    //    `animation-duration` 压到 0.001ms、`animation-iteration-count` 压到 1
    //    ⇒ 动画**立刻结束** ⇒ 帧回到基础样式 `opacity: 0`
    //    ⇒ 醒猫两帧全隐形 ⇒ 球只剩那圈淡环（用户原话「球消失了」）。
    //    实测（CDP 模拟 reduce=on）：awake-a / awake-b 的 computed opacity 都是 0。
    const a = cssRule(".orb--thinking .orb-frame-awake-a");
    const b = cssRule(".orb--thinking .orb-frame-awake-b");
    expect(a, "awake-a 规则要存在").not.toBe("");
    expect(/opacity:\s*1\s*;/.test(a), "awake-a 必须有静态 opacity:1").toBe(true);
    expect(/opacity:\s*1\s*;/.test(b), "awake-b 必须有静态 opacity:1").toBe(true);
  });

  it("②b 其它状态的帧也都在规则里写 opacity:1（同一口径：只有工作中靠过动画）", () => {
    for (const sel of [
      ".orb--idle .orb-frame-idle",
      ".orb--pending .orb-frame-pending",
      ".orb--mem .orb-frame-mem",
      ".orb--error .orb-frame-error",
    ]) {
      expect(/opacity:\s*1\s*;/.test(cssRule(sel)), `${sel} 必须写死 opacity:1`).toBe(true);
    }
  });
});

// ============================================================ ③ 删掉「已停止」那行

describe("③ 「已停止，本轮已产出的内容保留在上面」", () => {
  it("③a 用户自己按的停止 ⇒ 不再插那一行（用户点名要删）", async () => {
    await bootApp();
    await openPanel();
    // ⚠️ 要在 bootApp 之后设：bootApp 会把 mock 状态重置回去
    state.chatInterrupted = true;
    state.chatStopReason = ""; // 空 = 用户自己按的停止
    await sendText("跑一下");
    await waitFor(() => (document.body.textContent ?? "").includes("MOCK_OK"), 4000, "跑完");
    expect(
      document.body.textContent ?? "",
      "这行用户明确要求删掉（自己按的停止 ⇒ 纯噪声）",
    ).not.toContain("已停止，本轮已产出的内容保留在上面");
  });

  it("③b 有原因（轮数上限）⇒ 那行仍然要显示（那是真信息，不许一起删掉）", async () => {
    await bootApp();
    await openPanel();
    state.chatInterrupted = true;
    state.chatStopReason = "轮数上限";
    await sendText("跑一下");
    await waitFor(() => (document.body.textContent ?? "").includes("MOCK_OK"), 4000, "跑完");
    expect(document.body.textContent ?? "", "有原因时必须告诉用户为什么停").toContain("轮数上限");
  });
});

// ============================================================ ④ 删掉「记忆候选待审批」

describe("④ 「有 N 条记忆候选待审批」这类提示", () => {
  it("④ 一轮跑完 ⇒ 不再出现这类提示（其它提示不受影响）", async () => {
    await bootApp();
    await openPanel();
    await sendText("做点事");
    await waitFor(() => (document.body.textContent ?? "").includes("MOCK_OK"), 4000, "跑完");
    expect(
      document.body.textContent ?? "",
      "这类「有 N 条候选待审批」的提示用户不要了（2026-10-24）",
    ).not.toContain("记忆候选待审批");
  });
});

// ============================================================ ⑤ 「无任务空间」这一组

describe("⑤ 「无任务空间」这一组用不了", () => {
  it("⑤a 这一组默认展开（**哪怕当前会话不在这一组**）", async () => {
    // ⚠️ 场景：当前会话在**项目组**里 ⇒ 「无任务空间」组里没有当前会话
    //    ⇒ 旧口径（没有当前会话就折起来）会把这一组折掉 ⇒ 用户进会话页
    //    只看到一行标题 ⇒ 以为「整组用不了」（真机反馈 2026-10-24）。
    //    ⇒ 列表要摆好再 boot（应用启动时拉一次 session_state，之后不自己再拉）。
    state.sessionList = [
      { id: "sess-main", title: "主聊天", kind: "main", updatedAt: Date.now() - 9_000 },
      { id: "sess-xw", title: "作文那条", kind: "task", updatedAt: Date.now() },
    ];
    state.currentSessionId = "sess-xw";
    setMock("pmem_session_map", { "sess-xw": "写作作业" });
    await bootApp();
    await openPanel();
    await openSessionsView();
    const sep = sepOf("");
    expect(sep, "要有「无任务空间」组").toBeTruthy();
    expect(sep.getAttribute("aria-expanded"), "「无任务空间」组默认要展开").toBe("true");
  });

  it("⑤b 组标题上有「＋」⇒ 点它开「＋ 菜单」，选的模式就是新会话的模式，且落在这一组（不挂项目）", async () => {
    await bootApp();
    await openPanel();
    await openSessionsView();
    const plus = sepOf("").querySelector<HTMLElement>(".sess-sep-new");
    expect(plus, "组标题要有「＋」入口（用户说这一组没法 new）").toBeTruthy();
    plus!.click();
    await waitFor(() => !!document.getElementById("new-ctx"), 2000, "＋ 菜单");
    const ptc = document.querySelector<HTMLElement>('#new-ctx .ctx-item[data-mode="ptc"]');
    expect(ptc, "菜单里要有可选的模式（模式建完不能改 ⇒ 必须给选择机会）").toBeTruthy();
    ptc!.click();
    await waitFor(() => callsOf("session_new").length >= 1, 2000, "新建会话");
    expect(callsOf("session_new")[0]!.args?.mode, "新会话的模式 = 用户选的那个").toBe("ptc");
    expect(
      callsOf("pmem_bind_session").length,
      "「无任务空间」组里新建 ⇒ 不挂任何项目",
    ).toBe(0);
  });

  it("⑤c 没有激活项目时，「无任务空间」组**不带**「· 当前项目」", async () => {
    await bootApp();
    await openPanel();
    await openSessionsView();
    const txt = sepOf("").textContent ?? "";
    expect(txt, "没激活项目时不能误标成「· 当前项目」").not.toContain("· 当前项目");
    expect(txt, "激活的是无任务空间 ⇒ 标「当前空间」").toContain("当前空间");
  });

  it("⑤d 组标题右键 ⇒ 出**应用菜单**（以前这一组右键什么都没有）", async () => {
    await bootApp();
    await openPanel();
    await openSessionsView();
    const ev = new MouseEvent("contextmenu", {
      bubbles: true,
      cancelable: true,
      clientX: 120,
      clientY: 220,
    });
    sepOf("").dispatchEvent(ev);
    expect(ev.defaultPrevented, "右键必须被应用接管").toBe(true);
    await waitFor(() => !!document.getElementById("sess-ctx"), 2000, "应用菜单");
    expect(
      document.getElementById("sess-ctx")!.textContent ?? "",
      "这一组右键要有能用的东西（以前什么都没有 ⇒ 看着像坏了）",
    ).toContain("在这组新建会话");
  });

  it("⑤e 顶栏选「无任务空间」⇒ session_switch 到那个空间最近的一条（不是唯独这一格不跳）", async () => {
    // ⚠️ 列表要在 bootApp **之前**摆好：应用启动时拉一次 session_state，
    //    之后不会自己再拉 ⇒ 摆晚了应用看不到（resetMock 在 setup 的 beforeEach 里做，
    //    bootApp 不重置 ⇒ 摆在前面对）。
    state.sessionList = [
      { id: state.currentSessionId, title: "主聊天", kind: "main", updatedAt: Date.now() - 5_000 },
      { id: "sess-free", title: "没挂项目那条", kind: "task", updatedAt: Date.now() - 1_000 },
    ];
    await bootApp();
    await openPanel();
    const psel = document.getElementById("project-select") as HTMLSelectElement;
    psel.value = "";
    psel.dispatchEvent(new Event("change", { bubbles: true }));
    await waitFor(() => callsOf("session_switch").length >= 1, 2000, "跳到那个空间的会话");
    expect(
      callsOf("session_switch")[0]!.args?.id,
      "要跳到「无任务空间」里最近的那条",
    ).toBe("sess-free");
  });
});

// ============================================================ ⑥ 会话页能 new 会话

describe("⑥ 会话页要能 new 会话", () => {
  it("⑥a 会话页有一个**看得见**的新建入口（原来 hidden ⇒ 用户根本找不到）", async () => {
    await bootApp();
    await openPanel();
    await openSessionsView();
    const btn = document.getElementById("sess-new")!;
    expect(btn, "会话页要有新建按钮").toBeTruthy();
    expect(btn.hasAttribute("hidden"), "不能再 hidden（用户：会话页要能 new 会话）").toBe(false);
  });

  it("⑥b 会话页那颗 ＋ 走「＋ 菜单」⇒ 建出的会话模式 = 用户选的那个", async () => {
    await bootApp();
    await openPanel();
    await openSessionsView();
    document.getElementById("sess-new")!.click();
    await waitFor(() => !!document.getElementById("new-ctx"), 2000, "＋ 菜单");
    const ptc = document.querySelector<HTMLElement>('#new-ctx .ctx-item[data-mode="ptc"]');
    expect(ptc, "菜单里要有可选的模式").toBeTruthy();
    ptc!.click();
    await waitFor(() => callsOf("session_new").length >= 1, 2000, "新建会话");
    expect(callsOf("session_new")[0]!.args?.mode, "模式 = 用户选的那个").toBe("ptc");
  });

  it("⑥c 标题条那颗 ＋ 也走「＋ 菜单」（模式建完不能改 ⇒ 不许默默用默认模式）", async () => {
    await bootApp();
    await openPanel();
    await openSessionsView();
    const sb = document.getElementById("sb-new-session");
    expect(sb, "标题条右组要有 ＋").toBeTruthy();
    sb!.click();
    await waitFor(() => !!document.getElementById("new-ctx"), 2000, "＋ 菜单");
    const ptc = document.querySelector<HTMLElement>('#new-ctx .ctx-item[data-mode="ptc"]');
    ptc!.click();
    await waitFor(() => callsOf("session_new").length >= 1, 2000, "新建会话");
    expect(callsOf("session_new")[0]!.args?.mode, "模式 = 用户选的那个").toBe("ptc");
  });
});
