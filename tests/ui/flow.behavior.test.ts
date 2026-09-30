import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor, sleep } from "./helpers";
import { callsOf, resetMock } from "./mocks/tauri";

/**
 * 「行为」设置页（执行位置 / Agent 模式 / 执行权限）
 *
 * 背景（2026-10 用户拍板）：这三样是**设置**，不该占输入行。
 * 它们曾以 chip 形式挤在输入行，占掉约 110~165px，把输入框压到
 * 连一行 placeholder 都放不下。用户原话："不要设置我这个页面有设置的啊"。
 *
 * 这里锁的是搬迁后的契约：
 *   ① 输入行**只剩输入相关的东西**（一个 chip 都不许有）；
 *   ② 设置页里有「行为」入口，进去能看到三组控制；
 *   ③ 三组各自都能改，且真的调对应后端命令；
 *   ④ 「隐形桌面没有合成键鼠」这条限制仍在页面上（不许在搬迁中丢掉）。
 */

async function openBehavior(): Promise<HTMLElement> {
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-entry"), 3000, "设置入口页");
  const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-entry")).find(
    (b) => b.dataset.act === "behavior",
  );
  if (!entry) throw new Error("设置里找不到「行为」入口");
  entry.click();
  await waitFor(() => !!document.getElementById("bh-bgdesk"), 3000, "行为页渲染");
  return document.getElementById("panel-body")!;
}

describe("输入行只剩输入相关的东西", () => {
  it("输入行里没有任何 chip（三组控制已搬走）", async () => {
    await bootApp();
    await openPanel();
    const inputRow = document.querySelector(".panel-input")!;
    expect(
      inputRow.querySelectorAll(".trust-chip").length,
      "输入行不该再有设置类 chip",
    ).toBe(0);
    expect(document.getElementById("btn-status")).toBeNull();
    expect(document.getElementById("btn-mode")).toBeNull();
    expect(document.getElementById("btn-trust")).toBeNull();
    expect(document.getElementById("btn-bgdesk")).toBeNull();
  });

  it("placeholder 短到能放一行（这是「太挤」的直接症状）", async () => {
    await bootApp();
    await openPanel();
    const ta = document.getElementById("input") as HTMLTextAreaElement;
    // 老文案 38 字（约 470px @13px）在输入框里必然折行
    expect(ta.placeholder.length, `placeholder 仍偏长：${ta.placeholder}`).toBeLessThan(12);
    // 快捷键说明不能丢，挪到 title
    expect(ta.title).toContain("Shift+Enter");
    expect(ta.title).toContain("Ctrl+V");
  });

  it("输入行左侧那颗 🎛 真的能打开行为页（不能只摆样子）", async () => {
    await bootApp();
    await openPanel();
    const btn = document.getElementById("btn-behavior");
    expect(btn, "输入行缺「行为」logo").toBeTruthy();
    expect(document.querySelector(".panel-input")!.contains(btn), "🎛 必须在输入行里").toBe(true);
    btn!.click();
    // 曾经只有 markup、没有 listener —— 点了毫无反应，所以这条必须点到底
    await waitFor(() => !!document.getElementById("bh-bgdesk"), 3000, "行为页渲染");
  });
});

describe("设置 › 行为页", () => {
  it("设置入口页有「行为」项，摘要显示当前三态", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "设置入口页");
    const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-entry")).find(
      (b) => b.dataset.act === "behavior",
    )!;
    const txt = entry.textContent ?? "";
    expect(txt).toContain("前台");
    expect(txt).toContain("标准");
    expect(txt).toContain("询问");
  });

  it("三组控制都在页面上", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    expect(body.querySelector("#bh-bgdesk"), "缺执行位置").toBeTruthy();
    expect(body.querySelectorAll('input[name="bh-mode"]').length, "缺模式选项").toBe(4);
    expect(body.querySelectorAll('input[name="bh-trust"]').length, "缺权限选项").toBe(3);
  });

  it("切 Agent 模式 → 调 modes_set", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const ptc = body.querySelector<HTMLInputElement>('input[name="bh-mode"][value="ptc"]')!;
    ptc.checked = true;
    ptc.dispatchEvent(new Event("change", { bubbles: true }));
    await waitFor(
      () => callsOf("modes_set").some((c) => c.args?.id === "ptc"),
      2000,
      "modes_set(ptc)",
    );
  });

  it("切执行权限 → 调 set_exec_trust", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const full = body.querySelector<HTMLInputElement>('input[name="bh-trust"][value="full"]')!;
    full.checked = true;
    full.dispatchEvent(new Event("change", { bubbles: true }));
    await waitFor(
      () => callsOf("set_exec_trust").some((c) => c.args?.mode === "full"),
      2000,
      "set_exec_trust(full)",
    );
  });

  it("开后台执行 → 调 bg_desk_set(true)", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const cb = body.querySelector<HTMLInputElement>("#bh-bgdesk")!;
    cb.checked = true;
    cb.dispatchEvent(new Event("change", { bubbles: true }));
    await waitFor(
      () => callsOf("bg_desk_set").some((c) => c.args?.enabled === true),
      2000,
      "bg_desk_set(true)",
    );
  });

  it("页面上必须写明「隐形桌面没有合成键鼠」这条限制", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    expect(body.textContent, "搬迁过程中不能把这条限制丢掉").toContain("合成键鼠");
  });

  it("模式写错的组名仍要标灰（搬迁不丢提示）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const unknown = body.querySelector(".mg-unknown");
    expect(unknown, "modes.json 里写错的组名必须能看出来").toBeTruthy();
    expect(unknown!.textContent).toContain("打错的组名");
  });

  it("返回按钮回设置入口页", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    body.querySelector<HTMLElement>("#sub-back")!.click();
    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "回到设置入口");
  });
});
