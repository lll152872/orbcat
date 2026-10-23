import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { callsOf, state } from "./mocks/tauri";

/**
 * 内核状态可见性（pi 内核 / 自研内核 + 运行时组件状态）
 *
 * ## 背景（用户原话）
 *
 * 用户实机问「这个对得上吗」⇒ 界面上能直接看到**现在跑的是 pi 内核还是自研内核**
 * 是可观测性；更重要的是**能立刻发现「静默回落自研内核」**——
 * pi 没起来时前端会静默退回自研，用户却以为在用 pi。
 * `diag.log` 里有这条，但普通用户不会去翻日志。
 *
 * ## 契约
 *
 * ① 走 `src/data/` 接口层（`data.system.piStatus()`），**不直接调 invoke**；
 * ② 位置：设置**入口页**（那行「内核」旁显示实际状态）+ 设置›**行为**页
 *    （「内核与运行时」明细：组件状态 + 回落警告）——
 *    ⚠️ **不放聊天区**（对话流是用户自己在改的地方）；
 * ③ mock 场景清单有「内核状态」场景。
 */

/** 进设置入口页 */
async function openSettings(): Promise<HTMLElement> {
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
  return document.getElementById("panel-body")!;
}

/** 进设置 › 行为页 */
async function openBehavior(): Promise<HTMLElement> {
  await openSettings();
  const entry = document.querySelector<HTMLElement>('.set-row[data-act="behavior"]');
  if (!entry) throw new Error("设置里找不到「行为」入口");
  entry.click();
  await waitFor(() => !!document.querySelector('input[name="bh-pos"]'), 3000, "行为页渲染");
  return document.getElementById("panel-body")!;
}

describe("内核状态 · 可见性", () => {
  it("① 走接口层取状态（data.system.piStatus），不是硬编码 invoke", async () => {
    await bootApp();
    await openPanel();
    // 前端必须真的调了 pi_status
    await waitFor(() => callsOf("pi_status").length > 0, 3000, "pi_status 被调用");
  });

  it("② 设置入口页「内核」那行显示实际在跑的是什么", async () => {
    await bootApp();
    await openPanel();
    const body = await openSettings();
    const val = body.querySelector<HTMLElement>("#kernel-state-val");
    expect(val, "内核行要有状态值").toBeTruthy();
    const txt = val?.textContent ?? "";
    // ⚠️ 不能是空、也不能是「未知」—— 得真显示
    expect(txt.trim(), "内核状态不能是空").not.toBe("");
    expect(txt).toMatch(/pi 内核|自研内核|已回落/);
  });

  it("③ 行为页有「内核与运行时」明细块（组件状态）", async () => {
    // ⚠️ UI 测试的 mock **故意不给 pi_status 默认值**（给了所有用例都会切到 pi 内核，
    //    存量断言会集体失真 —— 见 tests/ui/mocks/tauri.ts 的注释）⇒ 这里自己设。
    const { setMock } = await import("./mocks/tauri");
    setMock("pi_status", {
      available: true,
      runtime: {
        runtimeDir: "D:\\orbcat\\runtime",
        ready: true,
        node: "D:\\orbcat\\runtime\\node\\node.exe",
        piBundle: "D:\\orbcat\\runtime\\pi",
        python: null,
        git: "C:\\Program Files\\Git\\cmd\\git.exe",
        gitSource: "system",
        bash: "C:\\Program Files\\Git\\bin\\bash.exe",
        bashSource: "system",
        notes: [],
      },
    });
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    // ⚠️ pi_status 是**异步**回来的 ⇒ 要等它落到界面上
    await waitFor(
      () => (body.textContent ?? "").includes("node"),
      3000,
      "组件清单渲染",
    );
    expect(body.querySelector("#kernel-runtime-box"), "缺内核运行时明细块").toBeTruthy();
    const txt = body.textContent ?? "";
    // 组件逐项列出（缺什么一眼看到）
    for (const comp of ["node", "pi", "python", "git"]) {
      expect(txt, `组件清单缺 ${comp}`).toContain(comp);
    }
    // ⚠️ python 缺 ⇒ 要显示「缺失」而不是留空
    expect(txt, "缺组件要写明缺失").toContain("缺失");
  });

  it("④ 明细里能看到「现在跑的是 pi 内核」", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const txt = body.textContent ?? "";
    expect(txt, "要说清现在跑的是哪个内核").toMatch(/现在跑的是\s*pi 内核|现在跑的是\s*自研内核/);
  });

  it("⑤ pi 不可用 ⇒ 静默回落必须**显眼**（不是悄悄换成自研）", async () => {
    // ⚠️ 模拟「pi 没起来」：后端回 available=false
    const { setMock } = await import("./mocks/tauri");
    setMock("pi_status", { available: false, runtime: { ready: false, gitSource: "missing", bashSource: "missing" } });
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    await waitFor(
      () => (body.textContent ?? "").includes("回落"),
      3000,
      "回落警告",
    );
    const txt = body.textContent ?? "";
    // 警告要说清「实际跑的是自研」
    expect(txt, "要说清实际跑的是自研内核").toContain("自研内核");
    expect(txt, "要提示这是回落").toContain("回落");
  });

  it("⑥ mock 有「内核状态」场景（场景清单里能点到）", async () => {
    const { SCENARIOS } = await import("../../src/data/mockdata");
    const s = SCENARIOS.find((x) => x.id === "kernel");
    expect(s, "场景清单缺「内核状态」场景").toBeTruthy();
    expect(s!.label).toContain("内核");
  });
});
