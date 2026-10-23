/**
 * 定时器 × 环境销毁 —— 回归用例。
 *
 * ## 要锁住什么
 *
 * 2026-10-23：UI 用例「全过但退出码 1」的根因是 toast 的自删定时器
 * （淡出 → 移除，life 4.5s / 7s / 2.2s）在**测试环境销毁之后**才触发，
 * 回调里摸 `window` ⇒ `ReferenceError: window is not defined`
 * ⇒ vitest 记成 unhandled error ⇒ **整套退出码 1**（CI 必红）。
 *
 * 守门的 F5 结论与本用例是同一件事：`src/main.ts` 的 `showToast` 定时器泄漏。
 *
 * ## 怎么复现（不靠运气）
 *
 * 真实触发要看「哪个测试文件最后收尾」，撞上才炸，CI 上时有时无。
 * 这里改成**直接制造环境销毁**：把全局 `window` / `document` 摘掉
 * （等价于 happy-dom teardown 后的状态），再让 toast 的定时器到期 ——
 * 缺守卫就**一定**抛 ReferenceError，稳定复现。
 *
 * ## 两条用例的分工
 *
 * 1. 守卫判定本身（`domAlive`）—— 锁住它不是「永远 true」。
 * 2. 环境销毁后定时器到期**不抛错** + 环境正常时 toast **仍会自己消失**
 *    （防「为了让 CI 绿，干脆把自删逻辑删了」这种假修）。
 */
import { describe, it, expect } from "vitest";
import { bootApp, openPanel, runCommandByName, sleep, waitFor } from "./helpers";
import { domAlive } from "../../src/dom-alive";

/**
 * 把 `window` / `document` 从全局摘掉 —— 模拟「环境已销毁」。
 *
 * @returns 恢复函数（测试收尾必须调，否则后续用例全废）
 */
function killDom(): () => void {
  const g = globalThis as unknown as Record<string, unknown>;
  const w = g.window;
  const d = g.document;
  delete g.window;
  delete g.document;
  return () => {
    g.window = w;
    g.document = d;
  };
}

/** 造一个 toast：`/new` → 选模式 → 新建会话（toast life = 2200ms） */
async function makeToast(): Promise<void> {
  await runCommandByName("new");
  await waitFor(() => !!document.getElementById("new-ctx"), 3000, "新建会话菜单");
  document.querySelector<HTMLElement>('.ctx-item[data-mode="chat"]')!.click();
  await waitFor(() => !!document.querySelector(".toast"), 3000, "toast 已出现");
}

describe("定时器 × 环境销毁", () => {
  it("domAlive：环境在时 true，被摘掉后 false", () => {
    expect(domAlive()).toBe(true);
    const restore = killDom();
    try {
      expect(domAlive()).toBe(false);
    } finally {
      restore();
    }
    expect(domAlive()).toBe(true);
  });

  it("toast 的自删定时器在 DOM 消失后不抛错（缺守卫必炸 ReferenceError）", async () => {
    await bootApp();
    await openPanel();
    await makeToast();

    // 环境销毁：全局 window / document 都没了
    const restore = killDom();
    try {
      // 等过 toast 的淡出定时器（2200ms）+ 自删链（+220ms）
      await sleep(3200);
    } finally {
      restore();
    }

    // 走到这里没抛错 = 守卫生效。
    // 缺守卫时错误发生在定时器回调里 ⇒ vitest 记 unhandled error ⇒ 本文件退出码 1。
    // （DOM 被摘掉时元素本来就移除不掉，这里不断言元素状态。）
    expect(domAlive()).toBe(true);
  });

  it("环境正常时 toast 仍会自己淡出消失（守卫不能把自删逻辑关掉）", async () => {
    await bootApp();
    await openPanel();
    await makeToast();
    expect(document.querySelector(".toast"), "toast 先在").toBeTruthy();

    // 2200ms（淡出）+ 220ms（移除）+ 余量
    await sleep(3000);
    expect(document.querySelector(".toast"), "toast 该自己消失").toBeNull();
  });
});
