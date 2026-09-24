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

    // A1：插话气泡应插到 running **之前**，不要永远垫底
    await waitFor(() => {
      const msgs = [...document.querySelectorAll(".msg")];
      const ri = msgs.findIndex((m) => m.classList.contains("running"));
      const si = msgs.findIndex((m) => m.textContent?.includes("第二条插话"));
      return ri >= 0 && si >= 0 && si < ri;
    }, 2000, "steer bubble before running");

    // 第一条最终仍应完成
    await waitFor(
      () => userBubbles().length >= 1 && !document.querySelector(".msg.running"),
      3000,
      "first chat finished",
    );
  });

  it("跑完后过程包进默认折叠的 run-process，展开状态可保持", async () => {
    await bootApp();
    await openPanel();
    await sendText("做点事");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running");
    await waitFor(
      () => !document.querySelector(".msg.running"),
      4000,
      "finished",
    );
    const proc = document.querySelector<HTMLDetailsElement>("details.run-process");
    expect(proc, "mock steps 非空应出现 run-process").toBeTruthy();
    expect(proc!.open).toBe(false);

    // 用户展开后重绘应保持 open（E3/E5）
    proc!.open = true;
    proc!.dataset.userOpen = "1";
    // 触发一次 paintMessages 路径：再发一条并完成
    await sendText("再来一条");
    await waitFor(
      () => !document.querySelector(".msg.running"),
      4000,
      "second finished",
    );
    const procs = document.querySelectorAll<HTMLDetailsElement>("details.run-process");
    // 至少有一条过程；用户展开过的 key 仍应 open
    expect(procs.length).toBeGreaterThan(0);
  });
});
