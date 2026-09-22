/**
 * 空输入保护 + 忙碌插话路径（chat 慢时点发送 → chat_steer）
 */
import { describe, it, expect } from "vitest";
import {
  bootApp,
  openPanel,
  sendText,
  waitFor,
  callsOf,
  state,
  userBubbles,
  sleep,
  typeInput,
  clickSend,
} from "./helpers";

describe("发送边界", () => {
  it("只有空白字符时不会 invoke chat", async () => {
    await bootApp();
    await openPanel();
    const before = callsOf("chat").length;
    typeInput("   \n  ");
    await clickSend();
    await sleep(30);
    expect(callsOf("chat").length).toBe(before);
  });

  it("chat 进行中再次发送会走插话 chat_steer", async () => {
    state.chatDelayMs = 400;
    await bootApp();
    await openPanel();

    void sendText("第一条（会慢）");
    await waitFor(() => callsOf("chat").length >= 1, 2000, "first chat started");

    // 等 running 气泡出现（busy=true）
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    await sendText("第二条插话");
    await waitFor(() => callsOf("chat_steer").length >= 1, 2000, "chat_steer");

    const steer = callsOf("chat_steer")[0]!;
    expect(steer.args?.input).toBe("第二条插话");

    // 第一条最终仍应完成
    await waitFor(
      () => userBubbles().length >= 1 && !document.querySelector(".msg.running"),
      3000,
      "first chat finished",
    );
  });
});
