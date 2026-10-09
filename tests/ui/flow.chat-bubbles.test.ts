import { describe, expect, it } from "vitest";
import { assistantBubbles, bootApp, openPanel, sendText, state, waitFor } from "./helpers";

/**
 * 闲聊模式 = 聊天式呈现（2026-10-08）。
 *
 * 用户原话：「像 QQ 聊天那样，可以多次发消息」。做法**抄业界**：模型照常输出一条
 * 正文，前端按**分段**切成气泡逐条显示（切分规则在 `src/bubble.ts`，纯函数测试
 * 在 `bubble.test.ts`）。
 *
 * 2026-10-08 追加：模式**跟会话走**了，所以这里判定的不再是"全局切到了哪个模式"，
 * 而是**当前会话**是什么模式 —— 用 `state.sessionMode`（= mock 里那条会话的模式）。
 *
 * 这里锁的是**渲染契约**：
 * - 闲聊会话 → 一条 assistant 渲染成**多个气泡**，且**不出现「过程」**
 * - 工作会话 → 仍是**一整块**（结构化长答案切成几个泡会非常难读）
 */
describe("闲聊模式的多气泡渲染", () => {
  it("闲聊模式：按分段拆成多个气泡，且不显示「过程」", async () => {
    state.sessionMode = "chat";
    state.chatReply =
      "刚看到你消息，我这边正好也在弄这个，等下细说。\n\n" +
      "不过先说结论：那个方案能走，不用改架构。\n\n" +
      "细节我等会儿整理一下发你。";

    await bootApp();
    await openPanel();
    await sendText("在吗");
    await waitFor(() => assistantBubbles().length > 0, 3000, "assistant 气泡");

    const bs = assistantBubbles();
    expect(bs.length, `应拆成多个气泡，实际=${JSON.stringify(bs)}`).toBeGreaterThan(1);
    expect(bs[0]).toContain("刚看到你消息");
    expect(bs[bs.length - 1]).toContain("细节我等会儿");

    // 容器上要有 chatty 标记（样式与动画都挂在它上面）
    expect(document.querySelector(".msg.assistant.chatty"), "应有 .msg.chatty").toBeTruthy();

    // **不带过程**：思考块与工具时间线在闲聊模式下都不该出现
    expect(document.querySelector(".msg.assistant details.run-process")).toBeNull();
    expect(document.querySelector(".msg.assistant .run-timeline")).toBeNull();
    expect(document.querySelector(".msg.assistant details.run-reason")).toBeNull();
  });

  it("工作模式：仍是一整块，不切气泡", async () => {
    state.sessionMode = "standard";
    state.chatReply =
      "刚看到你消息，我这边正好也在弄这个，等下细说。\n\n" +
      "不过先说结论：那个方案能走，不用改架构。";

    await bootApp();
    await openPanel();
    await sendText("在吗");
    await waitFor(() => assistantBubbles().length > 0, 3000, "assistant 气泡");

    expect(assistantBubbles().length, "工作模式不该切气泡").toBe(1);
    expect(document.querySelector(".msg.assistant.chatty")).toBeNull();
  });
});
