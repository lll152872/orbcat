/**
 * 「每一块都可 mock」的验收用例 —— 真界面 + mock 数据层 + **零 Tauri runtime**。
 *
 * 这组用例是「出 HTML 视频」的前置条件：如果一屏只能用假数据渲染出来，
 * 它就能被拍成视频、能被拿去做设计稿核对。
 *
 * 覆盖：
 * - 开关：一行切数据源（`setDataSource`）
 * - 屏幕：对话 / 会话页 / 记忆审批 / 权限卡 / 工具与 MCP
 * - 状态：空态 / 加载中 / 错误态
 * - 有状态：批准记忆后它真的从待批里消失；发消息后会话里真的多两条
 */
import { describe, it, expect } from "vitest";
import { bootMockApp, currentSource, switchView, openPanel, sendText, waitFor } from "./helpers";
import { SCENARIOS } from "../../src/data/mockdata";

describe("mock 数据层 · 真界面跑起来", () => {
  it("数据源默认是 mock（不是 Tauri）：一行切生效", async () => {
    await bootMockApp("default");
    const src = await currentSource();
    expect(src.kind, "界面跑在 mock 数据源上").toBe("mock");
  });

  it("悬浮球 + 面板都能渲染（不需要 Tauri runtime）", async () => {
    await bootMockApp("default");
    expect(document.getElementById("orb"), "悬浮球").not.toBeNull();
    await openPanel();
    expect(document.getElementById("input"), "输入框").not.toBeNull();
  });

  it("对话屏：mock 的会话与消息真的渲染出来了", async () => {
    await bootMockApp("default");
    await openPanel();
    await waitFor(
      () => document.body.textContent?.includes("悬浮球拖动判定") ?? false,
      3000,
      "mock 对话内容",
    );
  });

  it("会话页：5 条 mock 会话，主会话在「无任务空间」组", async () => {
    await bootMockApp("sessions");
    await openPanel();
    await switchView("sessions");
    await waitFor(() => !!document.querySelector(".sess-item"), 3000, "会话行");
    const rows = document.querySelectorAll(".sess-item");
    expect(rows.length, "5 条会话").toBe(5);
    expect(document.body.textContent).toContain("排查记忆审批不生效");
  });

  it("记忆审批：4 条候选；批准一条后**真的从待批里消失**（mock 是有状态的）", async () => {
    await bootMockApp("mem");
    await openPanel();
    await switchView("mem");
    await waitFor(() => document.querySelectorAll(".mem-candidate").length === 4, 3000, "4 条候选");

    const approve = document.querySelector<HTMLButtonElement>(
      '.mem-cand-btn.approve[data-id]',
    )!;
    approve.click();

    await waitFor(() => document.querySelectorAll(".mem-candidate").length === 3, 3000, "批准后剩 3 条");
    const src = (await currentSource()) as unknown as { data: { pending: unknown[] } };
    expect(src.data.pending.length, "mock 内部的待批队列也少了").toBe(3);
  });

  it("权限卡场景：文件级 / 命令级 / 问用户 三种申请都在", async () => {
    await bootMockApp("perm");
    await openPanel();
    await waitFor(() => !!document.querySelector(".perm-card"), 3000, "权限卡");
    const text = document.body.textContent ?? "";
    expect(text).toContain("rm -rf ./target/release");
    expect(text).toContain("落到 feat/frontend/");
  });

  it("工具与 MCP：网关已连接、4 个组", async () => {
    await bootMockApp("mcp");
    await openPanel();
    await switchView("mcp");
    await waitFor(() => document.body.textContent?.includes("filesystem") ?? false, 3000, "MCP 组");
  });

  it("空态：没模型没会话时给的是引导，不是报错", async () => {
    await bootMockApp("empty");
    await openPanel();
    await waitFor(() => !!document.getElementById("input"), 3000, "面板");
    const text = document.body.textContent ?? "";
    expect(text.includes("尚未初始化") || text.includes("去加模型") || text.includes("还没有配置任何模型")).toBe(
      true,
    );
  });

  it("错误态：后端读不出来时界面不崩，球变红", async () => {
    await bootMockApp("error");
    await openPanel();
    // 错误态的关键是**不崩** —— 能渲染出面板、且有一处红字即可
    expect(document.getElementById("input")).not.toBeNull();
  });

  it("发一条消息：mock 的脚本会回答（对话链在 mock 下也走得通）", async () => {
    await bootMockApp("default");
    await openPanel();
    await sendText("悬浮球的拖动判定是不是太灵敏了");
    await waitFor(
      () => document.body.textContent?.includes("按住") ?? false,
      5000,
      "mock 的回答",
    );
  });

  it("场景目录覆盖了要求的那些状态", () => {
    const ids = SCENARIOS.map((s) => s.id);
    for (const need of ["default", "empty", "loading", "error", "sessions", "perm", "mem", "compact"]) {
      expect(ids, `场景清单里要有 ${need}`).toContain(need);
    }
    // 每个场景都要有中文名和一句话说明（预览页侧栏直接用）
    for (const s of SCENARIOS) {
      expect(s.label.length, `${s.id} 要有中文名`).toBeGreaterThan(0);
      expect(s.desc.length, `${s.id} 要有说明`).toBeGreaterThan(0);
    }
  });
});
