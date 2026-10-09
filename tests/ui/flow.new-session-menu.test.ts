import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { callsOf, state } from "./mocks/tauri";

/**
 * 「＋」菜单 = **新建会话时选模式**（2026-10-08）
 *
 * 用户定案原话："b 从设置页挪到一个'新建会话时选模式'的入口 默认是标准"。
 *
 * 为什么必须锁死这些契约：
 *  - 模式**跟会话走、建完不能改**（用户另一条定案："一个会话模式不能变"）。
 *    所以"选模式"只发生在这一个地方 —— 这个菜单一旦画错，用户就没有别的补救路径，
 *    只能删会话重来。
 *  - 模式**绝不能**再走 `modes_set`（那是改全局默认值）。前端若把它当"切模式"调，
 *    界面会提示成功而当前会话纹丝不动 —— 那种静默失效最难查。
 */

async function openNewMenu(): Promise<HTMLElement> {
  document.getElementById("btn-new")!.click();
  // 菜单要等一次 `loadModes` 往返（打开前会重拉模式表），所以必须 waitFor
  await waitFor(() => !!document.getElementById("new-ctx"), 3000, "「＋」菜单");
  return document.getElementById("new-ctx")!;
}

/** 菜单里的模式项（不含「新建项目…」） */
function modeItems(menu: HTMLElement): HTMLElement[] {
  return Array.from(menu.querySelectorAll<HTMLElement>('.ctx-item[data-act="sess"]'));
}

describe("「＋」菜单 · 新建会话时选模式", () => {
  it("工具栏「＋」能打开菜单，且列出全部模式 + 新建项目", async () => {
    await bootApp();
    await openPanel();
    const menu = await openNewMenu();
    expect(modeItems(menu).length, "四个内置模式都要列出来").toBe(4);
    expect(menu.textContent).toContain("标准模式");
    expect(menu.textContent).toContain("PTC 模式");
    expect(menu.textContent).toContain("闲聊模式");
    expect(menu.textContent).toContain("创造模式");
    // 原能力不能丢：新建项目仍在这个菜单里
    expect(menu.querySelector('.ctx-item[data-act="proj"]'), "缺「新建项目…」").toBeTruthy();
  });

  it("默认模式排在最前并打「默认」标（用户要一眼看到不选会落到哪）", async () => {
    await bootApp();
    await openPanel();
    const menu = await openNewMenu();
    const items = modeItems(menu);
    expect(items[0].dataset.mode, "默认模式要排第一").toBe(state.defaultMode);
    const note = items[0].querySelector(".ctx-item-note");
    expect(note?.textContent, "默认项要有「默认」标").toContain("默认");
    // 别的项不该有标 —— 四条都标等于没标
    expect(items[1].querySelector(".ctx-item-note"), "非默认项不该有标").toBeNull();
  });

  it("点「闲聊模式」→ 调 session_new({mode:'chat'})，且**不**调 modes_set", async () => {
    await bootApp();
    await openPanel();
    const menu = await openNewMenu();
    const chat = menu.querySelector<HTMLElement>('.ctx-item[data-mode="chat"]')!;
    expect(chat, "菜单里要有闲聊模式").toBeTruthy();
    chat.click();
    await waitFor(
      () => callsOf("session_new").some((c) => c.args?.mode === "chat"),
      2000,
      "session_new(mode=chat)",
    );
    // ⚠️ 关键回归闸门：模式**绝不能**走 modes_set。
    // 走了的话 = 把全局默认改了、当前会话没变，界面上却提示"已切到闲聊"。
    expect(callsOf("modes_set").length, "模式不该走 modes_set（那是改默认值）").toBe(0);
  });

  it("新建会话后要按**新会话**重拉一次模式表（否则 UI 还显示上一个模式）", async () => {
    await bootApp();
    await openPanel();
    const menu = await openNewMenu();
    const before = callsOf("modes_get").length;
    menu.querySelector<HTMLElement>('.ctx-item[data-mode="chat"]')!.click();
    await waitFor(
      () => callsOf("modes_get").length > before,
      2000,
      "新建后重拉 modes_get",
    );
    // 重拉时要带上会话 id：不带的话后端只能回"默认模式"，等于没拉
    const last = callsOf("modes_get").at(-1)!;
    expect(last.args?.sessionId, "重拉必须带会话 id").toBe(state.currentSessionId);
    expect(state.sessionMode, "新会话的模式要真的落到 chat").toBe("chat");
  });

  it("模式说明来自 modes.json（菜单项的 tooltip），不是前端写死的", async () => {
    await bootApp();
    await openPanel();
    const menu = await openNewMenu();
    const ptc = menu.querySelector<HTMLElement>('.ctx-item[data-mode="ptc"]')!;
    // mock 的 modes_get 里 ptc 的 description（与真实 modes.json 同源）
    expect(ptc.title, "说明要跟 modes.json 一致").toContain("写成程序");
  });

  it("模式里写了不存在的组名 → 菜单里要看得见（不许静默失效）", async () => {
    await bootApp();
    await openPanel();
    const menu = await openNewMenu();
    const creator = menu.querySelector<HTMLElement>('.ctx-item[data-mode="creator"]')!;
    const warn = creator.querySelector(".ctx-sub");
    expect(warn, "写错的组名必须提示").toBeTruthy();
    expect(warn!.textContent).toContain("打错的组名");
    expect(warn!.textContent).toContain("不存在");
  });

  it("读不到模式表也要能开新会话（退回默认模式）", async () => {
    await bootApp();
    await openPanel();
    // 模拟 modes.json 读不出来：modes_get 返回空表
    state.invokeOverrides["modes_get"] = {
      activeMode: "standard",
      defaultMode: "standard",
      modes: [],
    };
    const menu = await openNewMenu();
    const items = modeItems(menu);
    expect(items.length, "空表时也要有一颗可点的「新建会话」").toBe(1);
    expect(items[0].dataset.mode).toBe("");
    items[0].click();
    await waitFor(() => callsOf("session_new").length > 0, 2000, "session_new");
    // 空串 → undefined → 前端补上「默认模式」，所以后端收到的永远是具体 id
    // （不补的话后端自己也能兜底，但那样前端日志会出一条 mode=null 的噪声）
    expect(callsOf("session_new").at(-1)!.args?.mode).toBe("standard");
  });
});
