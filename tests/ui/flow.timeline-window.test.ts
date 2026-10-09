import { describe, it, expect } from "vitest";
import { bootApp, openPanel, sendText, waitFor, sleep } from "./helpers";
import { emitEvent, state } from "./mocks/tauri";

/**
 * 超长时间线只渲染尾部（2026-10-09）。
 *
 * 背景：renderTimeline 会把每一轮的全部 step 拼成 HTML 字符串，而
 * paintMessages 在流式过程中会反复重绘 —— 221 步的长轮次等于每帧把
 * 整轮（含最长 38,689 字符那一步）重拼一遍。折叠的 <details> 已经让
 * 浏览器跳过布局和绘制，所以真正要省的是**拼字符串和建节点**。
 *
 * 做法是阈值而非虚拟滚动：≤40 步走原路径一行不变，超过只出尾部，
 * 前面的点「展开更早的 N 步」现取（prepend，不整段重画）。
 */
type Step = { kind: string; name: string | null; detail: string };

/** n 组「调用 + 返回」= 时间线上 n 个可数的节点 */
function nToolSteps(n: number): Step[] {
  const out: Step[] = [];
  for (let i = 0; i < n; i++) {
    out.push({ kind: "tool_call", name: "read_file", detail: `{"path":"f${i}.txt"}` });
    out.push({ kind: "tool_result", name: "read_file", detail: `内容 ${i}` });
  }
  return out;
}

async function renderHistory(steps: Step[]): Promise<void> {
  state.messages = [
    { role: "user", text: "看看", at: Date.now() },
    { role: "assistant", text: "好了。", at: Date.now() + 1, steps },
  ];
  await bootApp();
  await openPanel();
  await waitFor(() => !!document.querySelector(".msg.assistant"), 3000, "history rendered");
}

describe("超长时间线只渲染尾部", () => {
  it(">40 步只出尾部；点开一次取到头，一步不少、顺序不乱", async () => {
    const TOTAL = 60;
    await renderHistory(nToolSteps(TOTAL));

    const proc = document.querySelector<HTMLDetailsElement>("details.run-process");
    expect(proc, "应该有「过程」外壳").toBeTruthy();
    expect(proc!.querySelector("summary")?.textContent).toContain(`${TOTAL} 步`);

    const tl = proc!.querySelector<HTMLElement>(".tl-hist")!;
    expect(tl.dataset.head, "data-head = 还藏着的步数").toBe(String(TOTAL - 40));
    expect(tl.querySelectorAll("details.tl-tool").length, "首屏只渲染尾部 40 步").toBe(40);
    expect(tl.querySelectorAll(":scope > *").length, "40 步 + 1 颗按钮").toBe(41);

    const more = tl.querySelector<HTMLElement>(".tl-more");
    expect(more?.textContent).toContain("20 步");
    more!.click();
    await waitFor(() => tl.dataset.head === "0", 1000, "head 归零");

    expect(tl.querySelector(".tl-more"), "取到头后按钮应消失").toBeNull();
    const tools = Array.from(tl.querySelectorAll<HTMLElement>("details.tl-tool"));
    expect(tools.length, "60 步一个不少").toBe(TOTAL);
    for (let i = 0; i < TOTAL; i++) {
      expect(tools[i]!.textContent ?? "", `第 ${i} 步顺序不对`).toContain(`f${i}.txt`);
    }
  });

  it("≤40 步走原路径：不设 head、不出现按钮", async () => {
    await renderHistory(nToolSteps(3));

    const tl = document.querySelector<HTMLElement>(".tl-hist")!;
    expect(tl.dataset.head, "短轮次不设 head").toBeUndefined();
    expect(tl.querySelector(".tl-more"), "短轮次不该有按钮").toBeNull();
    expect(tl.querySelectorAll("details.tl-tool").length).toBe(3);
  });

  it("正好 40 步是边界：仍走原路径", async () => {
    await renderHistory(nToolSteps(40));
    const tl = document.querySelector<HTMLElement>(".tl-hist")!;
    expect(tl.querySelector(".tl-more"), "边界值不该触发窗口化").toBeNull();
    expect(tl.querySelectorAll("details.tl-tool").length).toBe(40);
  });
});

/**
 * 卡死回归闸门（2026-10-09）。
 *
 * 用户实机报：让模型在**思考**里输出 100 个 "100"，面板直接卡死。
 *
 * 根因：`patchTimeline` 为了判断"思考块是否贴底"，**每帧**读一次
 * `pre.scrollHeight`。读 scrollHeight / clientHeight 会**强制浏览器排版**
 * 这个 <pre> —— 哪怕它折叠着、内容看不见。内容越长，每帧代价越大，
 * 平方级增长。
 *
 * 修法：折叠时用 `details.open`（纯属性查询，不触发布局）短路掉那次读取。
 *
 * ⚠️ happy-dom **没有排版引擎**，scrollHeight 是桩，所以量不出真实卡顿；
 * 这里退一步断言"折叠时一次都不读 scrollHeight" ——
 * 它锁的正是那条会引发强制排版的代码路径。
 */
describe("思考块强制排版闸门", () => {
  it("思考块折叠时，patch 不读 scrollHeight", async () => {
    state.chatDelayMs = 50;
    await bootApp();
    await openPanel();
    await sendText("让它想");

    const sid = state.currentSessionId;
    const p = (o: Record<string, unknown>) => emitEvent("agent-progress", { sessionId: sid, p: o });
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running");
    p({ kind: "delta", reasoning: "先想想" });
    await sleep(60);

    // 计数：给 Element.prototype.scrollHeight 套一层
    const proto = Object.getOwnPropertyDescriptor(Element.prototype, "scrollHeight");
    const proto2 = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight");
    const real = proto?.get ?? proto2?.get;
    let reads = 0;
    const spy = function (this: Element) {
      // 只数思考块那个 <pre> 上的读取
      if (this.classList?.contains("run-reason-pre")) reads++;
      return real?.call(this) ?? 0;
    };
    if (proto?.get) Object.defineProperty(Element.prototype, "scrollHeight", { ...proto, get: spy });
    else if (proto2?.get)
      Object.defineProperty(HTMLElement.prototype, "scrollHeight", { ...proto2, get: spy });

    try {
      for (let i = 0; i < 20; i++) {
        p({ kind: "delta", reasoning: "100".repeat(i + 1) });
        await sleep(20);
      }
    } finally {
      if (proto?.get) Object.defineProperty(Element.prototype, "scrollHeight", proto);
      else if (proto2?.get)
        Object.defineProperty(HTMLElement.prototype, "scrollHeight", proto2);
    }

    const pre = document.querySelector(".run-reason-pre");
    expect(pre, "应该有思考块的 pre").toBeTruthy();
    expect(
      (pre!.closest("details") as HTMLDetailsElement | null)?.open,
      "思考块默认是折叠的（思考过程要「想看才点开」）",
    ).toBeFalsy();
    expect(reads, "折叠状态下不该读 scrollHeight —— 那会强制排版并卡死界面").toBe(0);
  });
});
