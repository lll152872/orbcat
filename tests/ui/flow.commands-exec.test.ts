/**
 * 斜杠命令面板的**执行**链路（集成层）。
 *
 * ## 为什么单独测
 *
 * `flow.commands.test.ts` 测的是**匹配**（纯函数：命令表 + 查询串 → 排序结果）。
 * 这里测的是「键盘选中一条命令之后，到底发生了什么」——
 *
 * ## 锁住的是一个真 bug（2026-10-23 修）
 *
 * 原写法：Enter 把 `/新建会话` **填回输入框**再 `send()`。
 * 结果：模型收到一句「新建会话」，而真正的「新建会话」动作**压根没发生**。
 * 原注释说「用户还能在发送前改」，可 Enter 紧接着就 send() 了，根本没有改的机会。
 *
 * 现在的契约：**命令是动作，不是消息**。
 * - Enter 选中 = 立刻执行该命令
 * - 用户打的那段 `/xxx` 一起被吃掉（命令不是消息，不该留在输入框里）
 * - 一条用户消息都不该发出去
 */

import { describe, expect, it } from "vitest";
import { bootApp, openPanel, typeInput, userBubbles, waitFor } from "./helpers";

describe("斜杠命令面板 · 执行", () => {
  it("输入 /new 回车 = 打开新建会话菜单，不是发一条叫 /new 的消息", async () => {
    await bootApp();
    await openPanel();

    const input = typeInput("/new");
    await waitFor(() => !!document.querySelector(".cmd-panel"), 3000, "命令面板");

    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
    );

    // 新建会话（选模式）菜单被打开
    await waitFor(() => !!document.getElementById("new-ctx"), 3000, "新建会话菜单");

    // 一条消息都没发出去 —— 命令不是消息
    expect(userBubbles()).toEqual([]);

    // 用户打的那段 "/new" 被吃掉了
    const box = document.getElementById("input") as HTMLTextAreaElement;
    expect(box.value).toBe("");
  });

  it("命令面板已关（不该再浮在界面上）", async () => {
    await bootApp();
    await openPanel();

    const input = typeInput("/new");
    await waitFor(() => !!document.querySelector(".cmd-panel"), 3000, "命令面板");

    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
    );
    await waitFor(() => !!document.getElementById("new-ctx"), 3000, "新建会话菜单");

    expect(document.querySelector(".cmd-panel")).toBeNull();
    expect(document.getElementById("cmd-layer")).toBeNull();
  });
});
