/**
 * 失败轮也要能重跑（R-033 / 审计 F7）。
 *
 * ## 要锁住什么
 *
 * `renderRegenBtn` 的老守卫是 `e.role !== "assistant"` ⇒ **失败轮拿不到重跑入口**。
 *
 * 失败轮长什么样：
 * - **回答落盘了**（后端 `persist_run` 把错误写成 assistant 消息里的一条
 *   `kind = "error"` 时间线条目）⇒ 那条气泡 role 仍是 "assistant"，
 *   老守卫就放行了，**按钮本来就有**；
 * - **回答没落上盘**（写盘失败等）⇒ 后端那条时间线也没有，
 *   前端在重载后补一条**内存里的 `role: "error"` 红字**（`main.ts` 的
 *   `pushEntry("error", msg)`）—— 这才是拿不到按钮的那一种。
 *
 * 所以这里专门造第二种：磁盘上**只有那条提问**，没有回答。
 *
 * ## 断言
 *
 * ① 红字气泡上有 `.msg-regen`（看得见入口）；
 * ② 按钮带 `data-rr`（= 提问的磁盘下标 + 1）而不是 `data-upto`；
 * ③ 点了 → 确认 → **重发同一轮输入**（`chat` 被再次调用，input 一字不差）。
 */
import { describe, it, expect } from "vitest";
import { bootApp, openPanel, sendText, waitFor, callsOf, state } from "./helpers";

/** 造一个「回答没落盘」的失败轮：磁盘上只有提问 */
function seedFailedTurn(text: string): void {
  state.messages.push({ role: "user", text, images: [], at: Date.now() });
  state.chatError = "模型接口超时（mock）";
}

describe("失败轮的重跑入口（R-033）", () => {
  it("回答没落盘时，红字气泡上有重跑入口，且指向那条提问", async () => {
    await bootApp();
    await openPanel();

    seedFailedTurn("把摘要落盘改一下");
    await sendText("把摘要落盘改一下");

    await waitFor(() => !!document.querySelector(".msg.error"), 3000, "error 气泡");

    const regen = document.querySelector<HTMLButtonElement>(".msg.error .msg-regen");
    expect(regen, "失败轮要看得见重跑入口").not.toBeNull();
    // 提问在磁盘下标 0 ⇒ data-rr = 0 + 1 = 1
    expect(regen!.dataset.rr, "失败轮用 data-rr（提问下标 + 1）").toBe("1");
    expect(regen!.dataset.upto, "失败轮不用 data-upto（那是回答轮的下标）").toBeUndefined();
  });

  it("点了重跑 → 确认 → 重发同一轮输入", async () => {
    await bootApp();
    await openPanel();

    seedFailedTurn("把摘要落盘改一下");
    await sendText("把摘要落盘改一下");
    await waitFor(() => !!document.querySelector(".msg.error .msg-regen"), 3000, "重跑按钮");

    const before = callsOf("chat").length;
    document.querySelector<HTMLButtonElement>(".msg.error .msg-regen")!.click();

    // 二次确认（重跑要花 token ⇒ 弹确认）
    await waitFor(() => !!document.querySelector(".confirm-card"), 2000, "确认框");
    document.querySelector<HTMLButtonElement>('.confirm-actions [data-k="yes"]')!.click();

    await waitFor(() => callsOf("chat").length > before, 3000, "重发");
    const last = callsOf("chat").at(-1)!;
    expect(last.args?.input, "重发的是同一轮输入").toBe("把摘要落盘改一下");
  });

  it("回答轮不受影响：仍走 data-upto（重生成 = 丢掉这条回答）", async () => {
    await bootApp();
    await openPanel();

    await sendText("正常的一轮");
    await waitFor(() => !!document.querySelector(".msg.assistant .msg-regen"), 3000, "回答轮重生成按钮");

    const btn = document.querySelector<HTMLButtonElement>(".msg.assistant .msg-regen")!;
    expect(btn.dataset.upto, "回答轮用 data-upto").toBeDefined();
    expect(btn.dataset.rr, "回答轮不用 data-rr").toBeUndefined();
  });
});
