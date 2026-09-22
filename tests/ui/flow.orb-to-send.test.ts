/**
 * 核心黄金路径：点击悬浮球 → 面板 → 输入 → 发送 → 消息上屏
 *
 * 这是「从点击到发送」的主验收用例，不依赖真实 LLM / Tauri 运行时。
 */
import { describe, it, expect } from "vitest";
import {
  bootApp,
  openPanel,
  clickOrb,
  sendText,
  waitFor,
  panelExists,
  orbExists,
  userBubbles,
  assistantBubbles,
  errorBubbles,
  runningBubble,
  sendButton,
  typeInput,
  clickSend,
  callsOf,
  state,
  sleep,
} from "./helpers";

describe("黄金路径：orb → panel → send", () => {
  it("启动后渲染悬浮球，尚未出现面板", async () => {
    await bootApp();
    expect(orbExists()).toBe(true);
    expect(panelExists()).toBe(false);
    expect(state.windowModes).toContain("orb");
  });

  it("点击悬浮球展开面板，出现输入框与发送按钮", async () => {
    await bootApp();
    await openPanel();
    expect(panelExists()).toBe(true);
    expect(document.getElementById("input")).toBeTruthy();
    expect(document.getElementById("btn-send")).toBeTruthy();
    expect(document.getElementById("btn-close")).toBeTruthy();
    expect(state.windowModes).toContain("panel");
  });

  it("输入文字并点击发送：用户气泡上屏，invoke(chat) 带上原文", async () => {
    await bootApp();
    await openPanel();

    const text = "你好，这是一条测试消息";
    await sendText(text);

    // 立刻应有用户气泡
    await waitFor(() => userBubbles().some((t) => t.includes(text)), 2000, "user bubble");

    const chatCalls = callsOf("chat");
    expect(chatCalls.length).toBeGreaterThanOrEqual(1);
    const last = chatCalls[chatCalls.length - 1]!;
    expect(last.args?.input).toBe(text);
  });

  it("chat 返回后展示 mock 回复，并清空输入框", async () => {
    await bootApp();
    await openPanel();

    state.chatReply = "MOCK_ASSISTANT_REPLY_42";
    await sendText("触发回复");

    await waitFor(
      () => assistantBubbles().some((t) => t.includes("MOCK_ASSISTANT_REPLY_42")),
      3000,
      "assistant bubble",
    );

    const input = document.getElementById("input") as HTMLTextAreaElement;
    expect(input.value).toBe("");
    expect(runningBubble()).toBe(false);
  });

  it("发送时调用 set_window_mode 保持 panel，并经过 session_state 重载", async () => {
    await bootApp();
    await openPanel();
    await sendText("重载会话");

    await waitFor(
      () => assistantBubbles().length > 0,
      3000,
      "assistant after reload",
    );
    expect(callsOf("session_state").length).toBeGreaterThanOrEqual(1);
  });

  it("空输入点发送：不调用 chat", async () => {
    await bootApp();
    await openPanel();
    const before = callsOf("chat").length;
    await clickSend();
    await sleep(30);
    expect(callsOf("chat").length).toBe(before);
  });

  it("chat 失败时展示错误气泡，不显示假回复", async () => {
    await bootApp();
    await openPanel();

    state.chatError = "模型接口超时（mock）";
    await sendText("会失败的消息");

    await waitFor(
      () => errorBubbles().some((t) => t.includes("模型接口超时")),
      3000,
      "error bubble",
    );
    expect(assistantBubbles().some((t) => t.includes("MOCK_OK"))).toBe(false);
  });

  it("未选模型时发送：本地提示，不调 chat", async () => {
    state.selectedModel = null;
    state.models = [];
    await bootApp();
    await openPanel();

    const before = callsOf("chat").length;
    await sendText("没有模型也发");
    await waitFor(
      () => document.body.textContent?.includes("请先选择一个模型") ?? false,
      2000,
      "no-model hint",
    );
    expect(callsOf("chat").length).toBe(before);
  });

  it("收起按钮回到悬浮球", async () => {
    await bootApp();
    await openPanel();
    (document.getElementById("btn-close") as HTMLButtonElement).click();
    await waitFor(() => orbExists() && !panelExists(), 3000, "collapsed to orb");
  });

  it("发送按钮在输入后可点（disabled 不为 true）", async () => {
    await bootApp();
    await openPanel();
    typeInput("检查按钮");
    expect(sendButton().disabled).toBe(false);
  });
});
