/**
 * 记忆闭环 UI：设置 › 记忆 的展示与批准/驳回按钮
 */
import { describe, it, expect } from "vitest";
import {
  bootApp,
  openPanel,
  waitFor,
  callsOf,
  state,
} from "./helpers";

async function openSettingsThenMemory(): Promise<void> {
  await bootApp();
  await openPanel();
  const gear = document.getElementById("btn-gear") as HTMLButtonElement;
  gear.click();
  // 设置入口页异步渲染，等菜单出现
  await waitFor(() => document.body.textContent?.includes("记忆") ?? false, 3000, "settings menu");
  // 点「记忆」入口（设置菜单项）
  const memEntry = Array.from(document.querySelectorAll("button, .set-entry, [data-view]")).find(
    (el) => (el.textContent ?? "").includes("记忆"),
  ) as HTMLElement | undefined;
  if (!memEntry) throw new Error("设置菜单里找不到记忆入口");
  memEntry.click();
}

describe("记忆审批 UI", () => {
  it("无待审批时显示空态文案", async () => {
    state.memPending = [];
    await openSettingsThenMemory();
    await waitFor(
      () => document.body.textContent?.includes("没有待审批") ?? false,
      3000,
      "empty pending",
    );
  });

  it("有待审批候选时展示内容，并能点批准/驳回", async () => {
    state.memPending = [
      {
        id: "cand_1",
        content: "用户偏好先结论后细节",
        source: "model",
        created_at: "ts0000000001T00:00:00Z",
      },
      {
        id: "cand_2",
        content: "临时状态不该记",
        source: "user",
        created_at: "ts0000000002T00:00:00Z",
      },
    ];
    await openSettingsThenMemory();

    await waitFor(
      () => document.body.textContent?.includes("用户偏好先结论后细节") ?? false,
      3000,
      "pending content",
    );

    const okBtn = document.querySelector<HTMLButtonElement>('.mem-btn.ok[data-id="cand_1"]');
    const noBtn = document.querySelector<HTMLButtonElement>('.mem-btn.no[data-id="cand_2"]');
    expect(okBtn).toBeTruthy();
    expect(noBtn).toBeTruthy();

    okBtn!.click();
    noBtn!.click();

    await waitFor(() => callsOf("mem_approve").length >= 1, 2000, "mem_approve invoked");
    await waitFor(() => callsOf("mem_reject").length >= 1, 2000, "mem_reject invoked");

    expect(callsOf("mem_approve")[0]?.args?.id).toBe("cand_1");
    expect(callsOf("mem_reject")[0]?.args?.id).toBe("cand_2");
  });
});
