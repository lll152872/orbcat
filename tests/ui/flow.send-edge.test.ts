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
  it("按下发送 = **立刻**调 chat（回归：不许再有等待窗口把消息吞掉）", async () => {
    // 2026-10-09 用户报的 bug：消息进了"待发批次"、还没真发出去时把面板收起
    // （切悬浮球态、不退出）→ 批次被当成"没地方去"直接丢掉，消息凭空消失。
    // 修法是**删掉合并窗口**，回到"按下即发出"。这条锁住它别再回来。
    await bootApp();
    await openPanel();
    typeInput("立刻发");
    await clickSend();
    // 只给一个宏任务（send() 内部那个 `await mem_pending` 的微任务要落地）
    await sleep(0);
    expect(callsOf("chat").length, "发送必须立刻落到 chat，不许攒着等窗口").toBe(1);
    expect(String(callsOf("chat")[0]?.args?.input)).toBe("立刻发");
  });

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

  it("跑完整轮收进默认折叠的「过程」，里面工具条目也各自折叠且展开可保持", async () => {
    await bootApp();
    await openPanel();
    await sendText("做点事");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running");
    await waitFor(
      () => !document.querySelector(".msg.running"),
      4000,
      "finished",
    );

    // 2026-10-02（用户："这个应该全部压缩掉啊"）：整轮收成一行「过程 · N 步」，
    // **默认闭合** —— 回看时不该占十几行。
    const proc = document.querySelector<HTMLDetailsElement>("details.run-process");
    expect(proc, "跑完应出现「过程」外壳").toBeTruthy();
    expect(proc!.open, "「过程」必须默认闭合").toBe(false);
    expect(proc!.querySelector("summary")?.textContent).toContain("步");

    // 外壳里面：工具调用/返回各折成一行，同样默认闭合
    const tools = Array.from(proc!.querySelectorAll<HTMLDetailsElement>("details.tl-tool"));
    expect(tools.length, "mock 有 tool_call/tool_result，应渲染成折叠行").toBeGreaterThan(0);
    tools.forEach((d) => expect(d.open, `${d.className} 应默认折叠`).toBe(false));

    // 用户展开后重绘应保持 open（E3/E5）
    proc!.open = true;
    proc!.dataset.userOpen = "1";
    tools[0]!.open = true;
    tools[0]!.dataset.userOpen = "1";
    // 触发一次 paintMessages 路径：再发一条并完成
    await sendText("再来一条");
    await waitFor(
      () => !document.querySelector(".msg.running"),
      4000,
      "second finished",
    );
    const procs = Array.from(document.querySelectorAll<HTMLDetailsElement>("details.run-process"));
    expect(procs.length).toBeGreaterThan(0);
    // 展开过的那条（按 data-open-key 复原）在重绘后仍是 open
    expect(procs.some((d) => d.open)).toBe(true);
  });
});
