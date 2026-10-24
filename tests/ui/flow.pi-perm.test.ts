import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor, callsOf, state } from "./helpers";
import type { PermRequestFixture } from "./mocks/tauri";

/**
 * **pi 确认卡**（`kind === "pi"`）—— pi 内核的 `ctx.ui.confirm(title, message)`。
 *
 * ## 背景
 *
 * 后端 kind 由 **id 前缀**推导（`src-tauri/src/perm_request.rs:314-320`）：
 * `pm*`→mcp、`pa*`→ask、**`pp*`→pi**、其余→command（文件卡是**另一张单独的表**）。
 *
 * 而前端 `renderPermCards` 的分派是 ask → command → mcp → **file（兜底）**
 * ⇒ `pp*` 的 pi 确认框掉进兜底分支，被**当成文件卡**渲染
 * （`path`/`access` 全是空串）⇒ 用户看到一张**什么都没有的空卡**。
 *
 * ## 契约（`docs/10-PI-CONFIRM-CONTRACT.md` §6 + `docs/11-PI-CONFIRM-UI-CONTRACT.md`）
 *
 * - 只有**允许 / 拒绝**两个动作（**没有**「记住这类/这条」「总是允许」）；
 * - **不给拒绝理由输入框**（pi 的确认通道只回一个布尔，reason 到不了模型）；
 * - **超时 3 分钟**（与 pm* 一致，超时由后端判成拒绝）；
 * - 失败必须**看得见**（用户明确要求「不能静默」）。
 */

/** 一张与后端 `PendingView` 同形的 pi 确认申请。 */
function piPerm(over: Partial<PermRequestFixture> = {}): PermRequestFixture {
  return {
    id: "pp1",
    kind: "pi",
    path: "",
    access: "",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "",
    normalized: "",
    risk: "medium",
    headFp: "",
    fullFp: "",
    tier: "once",
    reason: "pi 内核请求确认",
    sessionId: "s-main",
    createdAt: Date.now() - 5_000,
    timeoutMs: 180_000,
    question: "pi 要做一件事",
    message: "允许执行 rm -rf ~/project 吗？",
    options: ["允许", "拒绝"],
    ...over,
  };
}

/** 进面板并让 pi 卡渲染出来 */
async function openWithPiCard(fixture: PermRequestFixture = piPerm()): Promise<HTMLElement> {
  state.permPending = [fixture];
  await bootApp();
  await openPanel();
  await waitFor(() => !!document.querySelector(".perm-card-pi"), 3000, "pi 确认卡渲染");
  return document.querySelector<HTMLElement>(".perm-card-pi")!;
}

describe("pi 确认卡 · kind === 'pi'", () => {
  it("① pi 卡有**独立渲染**（不落进文件卡兜底分支）", async () => {
    await openWithPiCard();
    // 文件卡的类名是 .perm-card-file（或落在 .perm-card 兜底里）
    expect(
      document.querySelectorAll(".perm-card-pi").length,
      "pi 卡要用自己的类名渲染",
    ).toBe(1);
    // ⚠️ 兜底的文件卡类名不能出现在这张卡上
    const card = document.querySelector<HTMLElement>(".perm-card-pi")!;
    expect(card.className, "pi 卡不该带文件卡的类").not.toContain("perm-card-file");
    // 且**不能**出现文件卡才有的「路径 / 访问方式」行
    expect(card.textContent ?? "", "pi 卡不该有路径行").not.toContain("访问");
  });

  it("② 卡上能看到 title + message（pi 确认框的全部信息）", async () => {
    const card = await openWithPiCard();
    const txt = card.textContent ?? "";
    expect(txt, "要有 title").toContain("pi 要做一件事");
    expect(txt, "要有 message 正文").toContain("允许执行 rm -rf ~/project 吗？");
  });

  it("②b message 缺失时不留空行（后端还没把 message 补进基线时的常态）", async () => {
    const card = await openWithPiCard(piPerm({ message: undefined }));
    const txt = card.textContent ?? "";
    expect(txt, "title 还在").toContain("pi 要做一件事");
    // 正文那行不渲染 ⇒ 也没有空的内容容器
    expect(card.querySelector(".perm-pi-msg"), "没有正文就不该有那个行").toBeNull();
  });

  it("③ 只有「允许 / 拒绝」两个动作（没有记住这类/这条/总是允许）", async () => {
    const card = await openWithPiCard();
    const acts = Array.from(card.querySelectorAll<HTMLButtonElement>(".perm-btn")).map(
      (b) => b.dataset.act ?? "",
    );
    expect(acts.sort(), "只有两个动作").toEqual(["approve", "deny"]);
    const txt = card.textContent ?? "";
    for (const forbidden of ["记住这类", "记住这条", "总是允许", "拒绝并说明"]) {
      expect(txt, `不该出现「${forbidden}」`).not.toContain(forbidden);
    }
  });

  it("④ 点「允许」→ 调 decide(approve) + 卡消失", async () => {
    const card = await openWithPiCard();
    const ok = card.querySelector<HTMLButtonElement>('.perm-btn[data-act="approve"]')!;
    ok.click();
    await waitFor(
      () => callsOf("perm_request_decide").some((c) => c.args?.decision === "approve"),
      3000,
      "decide(approve)",
    );
    // ⚠️ 带 reason 也不行（pi 侧只回一个布尔，reason 到不了模型）
    const call = callsOf("perm_request_decide").find((c) => c.args?.decision === "approve")!;
    expect(call.args?.reason ?? "", "pi 卡不该带 reason").toBe("");
    await waitFor(
      () => !document.querySelector(".perm-card-pi"),
      3000,
      "卡消失",
    );
  });

  it("⑤ 点「拒绝」→ 调 decide(deny) + 卡消失（助手那轮收到拒绝，不是卡死）", async () => {
    const card = await openWithPiCard();
    const no = card.querySelector<HTMLButtonElement>('.perm-btn[data-act="deny"]')!;
    no.click();
    await waitFor(
      () => callsOf("perm_request_decide").some((c) => c.args?.decision === "deny"),
      3000,
      "decide(deny)",
    );
    await waitFor(() => !document.querySelector(".perm-card-pi"), 3000, "卡消失");
  });

  it("⑥ 失败**可见**：decide 报错 → 有错误提示 + 卡片保留（用户能重试）", async () => {
    // 模拟后端报错（真后端那条还没合进基线时就是这个效果）
    state.failures.set("perm_request_decide", "unknown command: perm_request_decide");
    const card = await openWithPiCard();
    const ok = card.querySelector<HTMLButtonElement>('.perm-btn[data-act="approve"]')!;
    ok.click();
    // ⚠️ 必须出现**看得见**的错误提示（不许静默吞掉）
    await waitFor(
      () => (document.body.textContent ?? "").includes("权限申请处理失败"),
      3000,
      "错误提示可见",
    );
    // 卡片要**留着**（后端还在等决定），按钮要能再点
    await waitFor(() => !!document.querySelector(".perm-card-pi"), 3000, "卡片保留");
    const btn = document.querySelector<HTMLButtonElement>(
      '.perm-card-pi .perm-btn[data-act="approve"]',
    )!;
    expect(btn.disabled, "失败后按钮要恢复可点（用户能重试）").toBe(false);
  });

  it("⑦ 倒计时 3 分钟（与 pm* 同一套机制：.perm-timer + data-deadline）", async () => {
    const card = await openWithPiCard();
    const timer = card.querySelector<HTMLElement>(".perm-timer");
    expect(timer, "要有倒计时").toBeTruthy();
    // data-deadline = createdAt + timeoutMs
    const deadline = Number(timer!.dataset.deadline ?? 0);
    expect(deadline, "倒计时锚点要对").toBeGreaterThan(0);
    const rest = deadline - Date.now();
    expect(rest, "应该是 3 分钟内的剩余时间").toBeLessThanOrEqual(180_000);
    expect(rest, "不该已经是过期的").toBeGreaterThan(170_000);
  });

  it("⑧ mock 与真同形：mock 场景能渲染出同一张 pi 卡（不依赖真 invoke）", async () => {
    // 用的是 **src/data/ 的 mock 层**（预览件用的那套），
    // 不是 tests/ui 的旧 mock ⇒ 证明渲染逻辑与真 invoke 无关。
    const { SCENARIOS, scenarioById } = await import("../../src/data/mockdata");
    const sc = SCENARIOS.find((s) => s.id === "pi-perm");
    expect(sc, "场景清单缺「pi 确认卡」场景").toBeTruthy();

    // 该场景的 fixture 里有一张 kind="pi" 的卡，且 id 用 pp 前缀（后端约定）
    const d = scenarioById("pi-perm");
    void d;
    const { defaultMockData } = await import("../../src/data/mockdata");
    const data = defaultMockData();
    // 手动套一遍 cover，确认场景确实挂上了 pi 卡
    const sc2 = scenarioById("pi-perm");
    expect(sc2.cover, "场景要有 cover").toBeTruthy();
    sc2.cover!(data);
    const piCard = data.permRequests.find((p) => p.kind === "pi");
    expect(piCard, "场景应挂上一张 pi 卡").toBeTruthy();
    expect(piCard!.id.startsWith("pp"), "id 要用 pp 前缀（后端靠前缀推 kind）").toBe(true);
    expect(piCard!.timeoutMs, "超时 3 分钟").toBe(180_000);
    expect(piCard!.message, "要有 message 正文").toBeTruthy();
  });
});
