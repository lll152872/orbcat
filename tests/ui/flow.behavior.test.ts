import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor, sleep } from "./helpers";
import { callsOf, resetMock } from "./mocks/tauri";

/**
 * 「行为」设置页（执行位置 / 执行权限）
 *
 * 背景（定稿）：这些是**设置**，不该占输入行。
 * 它们曾以 chip 形式挤在输入行，占掉约 110~165px，把输入框压到
 * 连一行 placeholder 都放不下（定稿口径："对话页不该有设置项"）。
 *
 * 二次搬迁：**Agent 模式从这里搬进了「＋」菜单**（定论：
 * "b 从设置页挪到一个'新建会话时选模式'的入口 默认是标准"）。理由：模式跟会话走、
 * 建完不能改 —— 它是"建会话的参数"而不是"设置"。模式选择的契约测试见
 * `flow.new-session-menu.test.ts`。
 *
 * 这里锁的是搬迁后的契约：
 *   ① 输入行**只剩输入相关的东西**（一个 chip 都不许有）；
 *   ② 设置页里有「行为」入口，进去能看到**两**组控制（位置 / 权限）；
 *   ③ 两组各自都能改，且真的调对应后端命令；
 *   ④ 模式**不在这里**（也不许回来），页面上要指路到「＋」菜单；
 *   ⑤ 「隐形桌面没有合成键鼠」这条限制仍在页面上（不许在搬迁中丢掉）。
 */

async function openBehavior(): Promise<HTMLElement> {
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
  const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-row")).find(
    (b) => b.dataset.act === "behavior",
  );
  if (!entry) throw new Error("设置里找不到「行为」入口");
  entry.click();
  // ⚠️ 「执行位置」现在是**三选一 radio**（2026-10-23 改成后台桌面 / 本机 pwsh /
  //    本机 Git Bash），不再是那个单独的 `#bh-bgdesk` checkbox。
  await waitFor(() => !!document.querySelector('input[name="bh-pos"]'), 3000, "行为页渲染");
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

  it("输入行左侧那颗按钮现在是「命令面板」入口（2026-10-23 改）", async () => {
    await bootApp();
    await openPanel();
    const btn = document.getElementById("btn-commands");
    expect(btn, "输入行缺「命令面板」按钮").toBeTruthy();
    expect(document.querySelector(".panel-input")!.contains(btn), "命令面板按钮必须在输入行里").toBe(true);
    btn!.click();
    // 点开就是命令清单 —— **不用记任何快捷键**（用户要求）
    await waitFor(() => !!document.getElementById("cmd-panel"), 3000, "命令面板浮层");
    const items = document.querySelectorAll(".cmd-item");
    expect(items.length, "命令面板应该有命令").toBeGreaterThan(0);
  });

  it("行为页仍能从设置进（按钮换了，能力没丢）", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
    // ⚠️ 定位用 `data-act="behavior"`，不要用文案匹配。
    // 设置入口页里「行为」是**分组标题**（`.mem-head`），那一项自己的标题
    // 叫「执行位置 / 权限」—— 按文案找「行为」会找不到（踩过）。
    const beh = document.querySelector<HTMLElement>('.set-row[data-act="behavior"]');
    expect(beh, "设置入口页缺「执行位置 / 权限」项").toBeTruthy();
    // 分组标题「行为与安全」也要在 —— 否则用户看到的是一堆没归类的行
    // （2026-10-23：设置页按设计稿 04 改成 4 组手风琴，组头是 .set-sec-name）
    const heads = Array.from(document.querySelectorAll<HTMLElement>(".set-sec-name")).map(
      (h) => h.textContent?.trim() ?? "",
    );
    expect(heads, "设置入口页缺「行为与安全」分组标题").toContain("行为与安全");
    beh!.click();
    await waitFor(() => !!document.querySelector('input[name="bh-pos"]'), 3000, "行为页渲染");
  });
});

describe("设置 › 行为页", () => {
  it("设置入口页有「行为」项，摘要只剩执行位置与权限", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
    const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-row")).find(
      (b) => b.dataset.act === "behavior",
    )!;
    // 2026-10-23：这一行按设计稿 04 只显示「执行位置」的值
    // （本机 pwsh / 本机 Git Bash / 后台桌面）。执行**权限档位**在行为子页里。
    const txt = entry.textContent ?? "";
    expect(txt).toContain("执行位置");
    expect(txt).toMatch(/本机 pwsh|本机 Git Bash|后台桌面/);
    // ⚠️ 模式**必须**从这一行消失 —— 它归「新建会话」菜单管。
    // 留着的话用户会以为"设置里能改模式"，而改了对当前会话根本无效。
    expect(txt, "模式不该再出现在行为摘要里").not.toContain("标准模式");
  });

  it("两组控制都在页面上，且模式选择器**不在**（已搬走）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    // ⚠️ 执行位置现在是**三选一**（后台桌面 / 本机 pwsh / 本机 Git Bash）
    expect(body.querySelectorAll('input[name="bh-pos"]').length, "执行位置应是三选一").toBe(3);
    expect(body.querySelectorAll('input[name="bh-trust"]').length, "缺权限选项").toBe(3);
    // 回归闸门：模式 radio 一旦回来，说明有人把设置页的旧代码恢复了
    expect(
      body.querySelectorAll('input[name="bh-mode"]').length,
      "模式选择器必须已从设置页搬走",
    ).toBe(0);
    expect(body.querySelector("#bh-mode"), "不该再有模式控件").toBeNull();
  });

  it("页面上要指路到「＋」菜单（搬走了不能不告诉用户去哪找）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const txt = body.textContent ?? "";
    expect(txt, "要说清模式入口在哪").toContain("新建会话");
    expect(txt, "要点名工具栏的 ＋").toContain("＋");
    // 这句是关键语义：建完不能改 —— 不说的话用户会去别处找"切模式"
    expect(txt, "要说明模式建完不能改").toContain("不能变");
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
    // ⚠️ 「后台桌面」现在是三选一里的**一支**（不是单独的 checkbox）
    const radio = body.querySelector<HTMLInputElement>(
      'input[name="bh-pos"][value="bgdesk"]',
    )!;
    radio.checked = true;
    radio.dispatchEvent(new Event("change", { bubbles: true }));
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

  /**
   * 模式能力的说明**不能随选择器一起消失**。
   *
   * 背景（2026-10）：PTC 曾经只是"多给几个工具组"，界面上与标准模式
   * 完全看不出区别（因为它们确实没区别）。改成真正的程序化调用后，
   * 工具列表会整个变掉（只剩 run_code），界面必须提前说清楚，
   * 否则用户会以为"我的工具全没了"。选择器搬去「＋」菜单后，
   * 这层说明留在行为页（并跟着模式菜单项的 `title`）。
   */
  it("模式能力的说明仍在页面上（工具组 / 调用方式 / 正文发图）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const txt = body.textContent ?? "";
    expect(txt, "缺工具组的说明").toContain("工具组");
    expect(txt, "缺 PTC 的调用方式说明").toContain("run_code");
    expect(txt, "缺正文发图的说明").toContain("表情包");
    // 手编 modes.json 这条口子也要留着说
    expect(txt, "要说清怎么加自定义模式").toContain("modes.json");
    // 模式只能收窄 —— 这是最容易误解的一条
    expect(txt, "要说清模式只能收窄").toContain("收窄");
  });

  it("返回按钮回设置入口页", async () => {
    await bootApp();
    await openPanel();
    await openBehavior();
    // 返回按钮在**滚动区之外**的 `#sub-bar` 里（不随内容滚），
    // 所以要从 document 查、不能在 panel-body 里找 —— 这正是那次修复的目的：
    // 滚到页面底部也点得到返回。
    const back = document.querySelector<HTMLElement>("#sub-bar #sub-back");
    expect(back, "返回按钮应在固定的 #sub-bar 内").toBeTruthy();
    back!.click();
    await waitFor(() => !!document.querySelector(".set-row"), 3000, "回到设置入口");
  });
});
