import { describe, it, expect, vi } from "vitest";
import { waitFor } from "./helpers";
import { callsOf } from "./mocks/tauri";

/**
 * models 独立窗口内部（src/models.ts）—— 2026-09-29 两个实机 bug 的回归锁。
 *
 *   Bug A：只有标题栏能拖，标题栏以下整片拖不动
 *   Bug B：点任何按钮 / 输入都会整块重绘 → 滚动位置归零（页面「自动上翻」）
 *
 * 这两个是连续交互（拖拽 / 滚动），截图验不了，所以在 happy-dom 里做确定性断言：
 *   A → 派发 mousedown，断言「空白处触发 startDragging / 按钮与滚动条不触发」
 *   B → 先给 .lm-list 写 scrollTop，再点会重绘的按钮，断言重绘后 scrollTop 被还原
 *
 * 注意：models.ts 在模块顶层 `boot()`，所以每个用例必须先 `vi.resetModules()`
 * 再动态 import，否则只有第一个用例能拿到渲染结果。
 */

/** 加载 models 窗口入口并等它渲染完 */
async function bootModels(): Promise<void> {
  vi.resetModules();
  document.body.innerHTML = `<div id="models-app"></div>`;
  await import("../../src/models");
  await waitFor(() => !!document.querySelector(".mw-window"), 3000, "models 窗口渲染");
}

function mousedown(el: Element, clientX = 10): void {
  el.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, clientX }));
}

function dragCalls(): number {
  return callsOf("window:start_dragging").length;
}

describe("models 窗口 · 整窗拖动（Bug A 回归）", () => {
  it("标题栏以下的 body 空白处按下 → startDragging", async () => {
    await bootModels();
    expect(dragCalls(), "初始不该有拖动").toBe(0);

    mousedown(document.querySelector(".mw-body")!);
    await waitFor(() => dragCalls() === 1, 2000, "body 空白处触发拖动");
  });

  it("标题栏本身按下 → startDragging（原有路径不能坏）", async () => {
    await bootModels();
    mousedown(document.querySelector(".mw-titlebar")!);
    await waitFor(() => dragCalls() === 1, 2000, "标题栏触发拖动");
  });

  it("按在按钮上 → 不拖（否则按钮点不动）", async () => {
    await bootModels();
    mousedown(document.getElementById("model-add-open")!);
    expect(dragCalls(), "按钮上不该触发拖动").toBe(0);
  });

  it("按在输入框上 → 不拖（否则没法聚焦输入）", async () => {
    await bootModels();
    mousedown(document.getElementById("model-search")!);
    expect(dragCalls(), "输入框上不该触发拖动").toBe(0);
  });

  it("按在滚动容器的滚动条上 → 不拖；按在内容区 → 拖", async () => {
    await bootModels();
    const list = document.querySelector<HTMLElement>(".lm-list")!;
    // happy-dom 不做布局：手工造一个「宽 320、右侧 20px 为滚动条槽」的盒子
    Object.defineProperty(list, "clientWidth", { value: 300, configurable: true });
    Object.defineProperty(list, "getBoundingClientRect", {
      value: () => ({
        left: 0,
        top: 0,
        width: 320,
        height: 200,
        right: 320,
        bottom: 200,
        x: 0,
        y: 0,
        toJSON: () => ({}),
      }),
      configurable: true,
    });

    mousedown(list, 310); // 滚动条槽内 → 放行，交给原生滚动
    expect(dragCalls(), "滚动条上不该触发拖动").toBe(0);

    mousedown(list, 100); // 内容区 → 拖
    expect(dragCalls(), "内容区应可拖").toBe(1);
  });
});

describe("models 窗口 · 重绘保滚动（Bug B 回归）", () => {
  it("点「添加模型」重绘后，列表 scrollTop 保持不回顶", async () => {
    await bootModels();
    const before = document.querySelector<HTMLElement>(".lm-list")!;
    before.scrollTop = 320;

    document.getElementById("model-add-open")!.click();
    await waitFor(() => !!document.getElementById("mm-type-custom"), 2000, "类型选择弹出");

    const after = document.querySelector<HTMLElement>(".lm-list")!;
    expect(after, "列表应是重绘后的新节点").not.toBe(before);
    expect(after.scrollTop, "重绘不该把滚动位置归零").toBe(320);
  });

  it("搜索框输入重绘后，scrollTop 保持且焦点仍在搜索框", async () => {
    await bootModels();
    document.querySelector<HTMLElement>(".lm-list")!.scrollTop = 180;

    const input = document.getElementById("model-search") as HTMLInputElement;
    input.value = "mock";
    input.dispatchEvent(new Event("input", { bubbles: true }));

    await waitFor(
      () => document.querySelector<HTMLElement>(".lm-list")!.scrollTop === 180,
      2000,
      "重绘后 scrollTop 还原",
    );
    expect(document.activeElement?.id, "输入后焦点应留在搜索框").toBe("model-search");
  });
});
