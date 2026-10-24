import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor, callsOf, state } from "./helpers";
import type { PermRequestFixture } from "./mocks/tauri";

/**
 * **文件权限的「永远允许」档**（用户 2026-10-24 拍板加的）。
 *
 * ## 背景
 *
 * 三张权限卡里，**只有文件卡没有能一直绑定的选项**：
 *
 * | 卡 | 按钮 | 永久？ |
 * |---|---|---|
 * | 命令 | 一次 / 记住这类 / 记住这条 / 拒绝 | ✅（`command_grants.json`） |
 * | MCP | 允许一次 / 总是允许 / 拒绝 | ✅（`mcp_grants.json`） |
 * | **文件** | 收窄 / 批准 / 拒绝 | ❌ **没有** |
 *
 * ⇒ 用户原话「**权限不会一直绑定**」⇒ 文件卡加一颗「永远允许」。
 *
 * ## 口径（用户拍板的三条）
 *
 * ① 永久档**只批「这个路径」**（最窄）—— 不做「记住这类」；
 * ② 现有「一次 / 一轮 / 整个任务」三档**全部保留**，加一档永久；
 * ③ 永久授权**能在设置里撤销**。
 *
 * ## 后端对应
 *
 * `GrantTier::Forever` → 落 `<data_dir>/file_grants.json`（**不进**
 * `sessions/<id>.grants.json` —— 那个随会话消失，语义会破）。
 */

/** 一张与后端 `PendingView` 同形的**文件**权限申请（文件卡的 kind 恒为 "file"）。 */
function filePerm(over: Partial<PermRequestFixture> = {}): PermRequestFixture {
  return {
    id: "pr1",
    kind: "file",
    path: "D:\\docs",
    access: "readwrite",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "",
    normalized: "",
    risk: "medium",
    headFp: "",
    fullFp: "",
    tier: "once",
    reason: "整理文档",
    sessionId: "s-main",
    createdAt: Date.now() - 1000,
    timeoutMs: 180_000,
    question: "",
    options: [],
    ...over,
  };
}

/** 进面板并让文件权限卡渲染出来 */
async function openWithFileCard(fixture: PermRequestFixture = filePerm()): Promise<HTMLElement> {
  state.permPending = [fixture];
  await bootApp();
  await openPanel();
  await waitFor(() => !!document.querySelector(".perm-card"), 3000, "文件权限卡渲染");
  return document.querySelector<HTMLElement>(".perm-card")!;
}

describe("文件权限卡 · 「永远允许」", () => {
  it("① 卡上要有「永远允许」按钮（原来只有 收窄/批准/拒绝/✎ ⇒ 没法永久绑定）", async () => {
    await openWithFileCard();
    const btn = document.querySelector<HTMLButtonElement>('.perm-btn[data-act="forever"]');
    expect(btn, "文件卡要有「永远允许」按钮").toBeTruthy();
    expect(btn!.textContent ?? "", "按钮上要写清是「永远允许」").toContain("永远允许");
    expect(
      btn!.getAttribute("title") ?? "",
      "要说明它跨会话/重启都生效、且能在设置里撤销",
    ).toContain("撤销");
  });

  it("② 原有三档按钮**一个都不许少**（用户拍板：只加一档，不重新设计）", async () => {
    await openWithFileCard();
    for (const act of ["approve", "deny"]) {
      expect(
        document.querySelector(`.perm-btn[data-act="${act}"]`),
        `「${act}」按钮必须还在`,
      ).toBeTruthy();
    }
    expect(
      document.querySelector('.perm-btn[data-act="deny-why"]'),
      "拒绝并说明（✎）必须还在",
    ).toBeTruthy();
  });

  it("③ 点「永远允许」⇒ 发 decide(forever)（后端 parse 成永久档）", async () => {
    await openWithFileCard();
    document.querySelector<HTMLButtonElement>('.perm-btn[data-act="forever"]')!.click();
    await waitFor(() => callsOf("perm_request_decide").length >= 1, 2000, "decide");
    const call = callsOf("perm_request_decide")[0]!;
    expect(call.args?.decision, "决定必须是 forever（= 永久档）").toBe("forever");
    expect(call.args?.id, "要带上申请的 id").toBe("pr1");
  });

  it("④ 「永远允许」不许顶掉「批准」—— 两颗都在（两档语义不同）", async () => {
    await openWithFileCard();
    const acts = Array.from(document.querySelectorAll<HTMLElement>(".perm-btn[data-act]")).map(
      (b) => b.dataset.act,
    );
    expect(acts, "批准与永远允许必须同时存在").toContain("approve");
    expect(acts, "批准与永远允许必须同时存在").toContain("forever");
  });
});

describe("设置 › 文件权限 · 永久授权清单", () => {
  /** 进「安全闸门 / 文件权限」页 */
  async function openPermView(): Promise<HTMLElement> {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
    const entry = Array.from(document.querySelectorAll<HTMLButtonElement>(".set-row")).find(
      (b) => b.dataset.act === "perm",
    );
    if (!entry) throw new Error("设置里找不到「安全闸门」入口");
    entry.click();
    await waitFor(
      () => !!document.querySelector(".mem-head"),
      3000,
      "文件权限页",
    );
    return document.getElementById("panel-body")!;
  }

  it("⑤ 列表里列出永久授权（路径 + 权限）并给一颗「撤销」", async () => {
    state.file_grants_list = [
      {
        prefix: "D:\\docs",
        access: "readwrite",
        tier: "forever",
        reason: "整理文档",
        granted_at: Date.now() - 1000,
      },
    ];
    const body = await openPermView();
    const txt = body.textContent ?? "";
    expect(txt, "要列出那条路径").toContain("D:\\docs");
    expect(txt, "要说明落盘在哪儿").toContain("file_grants.json");
    const revoke = body.querySelector<HTMLButtonElement>(".file-grant-revoke");
    expect(revoke, "每条要有「撤销」按钮").toBeTruthy();
  });

  it("⑥ 点「撤销」⇒ 调 file_grants_revoke（带路径前缀）", async () => {
    state.file_grants_list = [
      {
        prefix: "D:\\docs",
        access: "readwrite",
        tier: "forever",
        reason: "整理文档",
        granted_at: Date.now() - 1000,
      },
    ];
    const body = await openPermView();
    const revoke = body.querySelector<HTMLButtonElement>(".file-grant-revoke")!;
    revoke.click();
    await waitFor(() => callsOf("file_grants_revoke").length >= 1, 2000, "撤销");
    expect(
      callsOf("file_grants_revoke")[0]!.args?.prefix,
      "撤销要带路径前缀",
    ).toBe("D:\\docs");
  });

  it("⑦ 没有永久授权时给一句空态说明（别让人以为功能没了）", async () => {
    state.file_grants_list = [];
    const body = await openPermView();
    const txt = body.textContent ?? "";
    expect(txt, "空态要说清怎么才有").toContain("永远允许");
    expect(txt, "空态要点出落盘文件").toContain("file_grants.json");
  });
});
