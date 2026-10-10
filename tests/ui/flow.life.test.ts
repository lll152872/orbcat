import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { callsOf, resetMock, state } from "./mocks/tauri";

/**
 * 设置 › 数据源（认知面注入）
 *
 * 背景（2026-10 用户拍板）：数据源和 MCP 是两件事 ——
 * MCP 是能力面（模型决定去调），数据源是认知面（系统决定呈现）。
 * 用户问「帮我安排下今天」时模型**没有理由**先去调一个工具，
 * 所以数据源要主动出现在 system prompt 里（一行摘要）。
 *
 * 这一页锁的是：
 *   ① 设置入口有「数据源」且摘要反映真实状态；
 *   ② 有源时画得出条数 / 过期 / 读取失败三种状态；
 *   ③ 开关真的调后端命令；
 *   ④ 页面上写明「command 类会自动执行」这条风险（不能藏）。
 */

/** 一个"接了学习通"的形态 */
function withXuexitong(over: Partial<(typeof state)["lifeStatus"]["sources"][number]> = {}) {
  resetMock({
    lifeStatus: {
      path: "C:\\mock\\agent-data\\life.json",
      sources: [
        {
          id: "xuexitong",
          label: "学习通",
          enabled: true,
          kind: "file",
          target: "D:/workplace/myssh/campus-monitor/state/xuexitong.json",
          itemsPath: "$.works[*]",
          staleHours: 48,
          complete: true,
          ok: true,
          error: "",
          updatedAt: "2026-09-30 22:00:12",
          ageHours: 15.8,
          stale: false,
          count: 2,
          preview: [
            "[软件文档写作] 实验四：项目验收总结报告的撰写",
            "[软件文档写作] 实验三：详细设计说明书的撰写",
          ],
          ...over,
        },
      ],
    },
  });
}

async function openLife(): Promise<HTMLElement> {
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-entry"), 3000, "设置入口页");
  const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-entry")).find(
    (b) => b.dataset.act === "life",
  );
  if (!entry) throw new Error("设置里找不到「数据源」入口");
  entry.click();
  await waitFor(() => !!document.getElementById("life-refresh"), 3000, "数据源页渲染");
  return document.getElementById("panel-body")!;
}

describe("设置 › 数据源", () => {
  it("功能暂缓：入口行在 DOM 里但带 hidden，且摘要为空（幽灵函数返回 null）", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "设置入口页");
    const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-entry")).find(
      (b) => b.dataset.act === "life",
    )!;
    // 行保留在 DOM 里 —— 页面代码与后面 10 个用例都还依赖它，用户看不见而已
    expect(entry, "「数据源」行应当仍在 DOM 中").toBeTruthy();
    expect(entry.hasAttribute("hidden"), "但它必须带 hidden 属性").toBe(true);
    // 摘要函数是幽灵函数（返回 null）→ 不该渲染出那行 <i>
    expect(entry.querySelector("i"), "暂缓期间不该有摘要文本").toBeNull();
  });

  it("空表时画出「还没有数据源」+ 怎么加的说明", async () => {
    await bootApp();
    await openPanel();
    const body = await openLife();
    expect(body.textContent).toContain("还没有数据源");
    // 必须给出可照抄的 schema，否则用户不知道怎么写
    expect(body.textContent).toContain("sources");
    expect(body.textContent).toContain("$.works[*]");
  });

  it("有源时显示条数与新鲜度", async () => {
    await bootApp();
    withXuexitong();
    await openPanel();
    const body = await openLife();
    const txt = body.textContent ?? "";
    expect(txt).toContain("学习通");
    expect(txt).toContain("2 条");
    expect(txt).toContain("15.8 小时前");
    // 明细预览（让用户一眼确认接对了）
    expect(txt).toContain("实验四");
  });

  it("快照过期必须标出来（不能静默当最新）", async () => {
    await bootApp();
    withXuexitong({ ageHours: 100, stale: true });
    await openPanel();
    const body = await openLife();
    expect(body.textContent).toContain("可能已过期");
    expect(body.querySelector(".life-bad"), "过期要用暖色标出来").toBeTruthy();
  });

  it("读取失败要显示原因（不能只显示 0 条）", async () => {
    await bootApp();
    withXuexitong({
      ok: false,
      error: "读不到快照 D:/nope.json：系统找不到指定的文件",
      count: 0,
      preview: [],
    });
    await openPanel();
    const body = await openLife();
    const txt = body.textContent ?? "";
    // 失败必须**把原因带出来**（"0 条"和"读不到"是两回事，
    // 后者要用户去处理，前者是正常状态）
    expect(txt).toContain("读不到快照");
    expect(body.querySelector(".life-bad"), "失败要用暖色标出来").toBeTruthy();
  });

  it("关闭的源显示「已关闭」且不显示明细", async () => {
    await bootApp();
    withXuexitong({ enabled: false, ok: false, error: "", count: 0, preview: [] });
    await openPanel();
    const body = await openLife();
    expect(body.textContent).toContain("已关闭");
    expect(body.textContent).not.toContain("实验四");
  });

  it("切开关 → 调 life_set_enabled", async () => {
    await bootApp();
    withXuexitong({ enabled: false, ok: false, error: "", count: 0, preview: [] });
    await openPanel();
    const body = await openLife();
    const cb = body.querySelector<HTMLInputElement>(".life-cb")!;
    expect(cb).toBeTruthy();
    cb.checked = true;
    cb.dispatchEvent(new Event("change", { bubbles: true }));
    await waitFor(
      () => callsOf("life_set_enabled").some((c) => c.args?.id === "xuexitong" && c.args?.enabled === true),
      2000,
      "life_set_enabled(xuexitong, true)",
    );
  });

  it("刷新按钮 → 调 life_refresh 并重读", async () => {
    await bootApp();
    await openPanel();
    const body = await openLife();
    body.querySelector<HTMLElement>("#life-refresh")!.click();
    await waitFor(() => callsOf("life_refresh").length > 0, 2000, "life_refresh");
    // 刷新后要重新拉状态（否则界面还显示旧数据）
    await waitFor(() => callsOf("life_status").length >= 2, 2000, "重读 life_status");
  });

  it("页面上必须写明 command 类会自动执行（这条风险不能藏）", async () => {
    await bootApp();
    await openPanel();
    const body = await openLife();
    const txt = body.textContent ?? "";
    expect(txt, "必须说明 command 是后台自动跑的").toContain("后台自动执行");
    expect(txt, "要给出两种 kind 的说明").toContain("kind: file");
    expect(txt).toContain("kind: command");
  });

  it("页面上要说明数据源与 MCP 的分工", async () => {
    await bootApp();
    await openPanel();
    const body = await openLife();
    const txt = body.textContent ?? "";
    expect(txt, "要解释认知面 vs 能力面").toContain("认知面");
    expect(txt).toContain("能力面");
    // 注入策略要说清楚（一行摘要 + 细节按需）
    expect(txt).toContain("life_items");
  });

  it("返回按钮回设置入口页", async () => {
    await bootApp();
    await openPanel();
    await openLife();
    // 返回按钮 2026-10-08 起在**滚动区之外**的 `#sub-bar` 里（不随内容滚），
    // 所以要从 document 查、不能在 panel-body 里找 —— 这正是那次修复的目的：
    // 滚到页面底部也点得到返回。
    const bar = document.querySelector("#sub-bar");
    expect(bar, "#sub-bar 应存在（固定的子页标题条）").toBeTruthy();
    const back = bar!.querySelector<HTMLElement>("#sub-back");
    expect(back, "返回按钮应在 #sub-bar 内").toBeTruthy();
    back!.click();
    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "回到设置入口");
  });
});
