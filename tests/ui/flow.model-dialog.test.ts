import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { callsOf } from "./mocks/tauri";

/**
 * 模型管理交互（2026-09-29 定稿）：
 *   编辑/管理 → 弹出独立第二窗口（点卡片本体 / 设置入口 → invoke models_window_open）
 *   选择模型  → 留在面板：卡片 ▾ 弹「纯模型名下拉」，只选不管
 *
 * 旧版「面板内 overlay 大面板 + 设置页模型页」已随回档删除，
 * jsdom 里也无法渲染第二个 Tauri 窗口 —— 所以这里锁的是**入口与选择**，
 * 独立窗口内部（表格/表单/分组）由 models.ts 自测覆盖范围之外，靠 MCP 截图验收。
 */

/** 点 ▾ 打开纯名称下拉，等它出现 */
async function openDropdown(): Promise<HTMLElement> {
  document.querySelector<HTMLElement>("#model-chip .mc-caret")!.click();
  await waitFor(() => !!document.getElementById("models-dropdown"), 2000, "dropdown opened");
  return document.getElementById("models-dropdown")!;
}

describe("模型管理独立窗口入口（编辑弹窗外）", () => {
  it("设置菜单「模型」→ 调 models_window_open，不进面板内模型页", async () => {
    await bootApp();
    await openPanel();
    const before = callsOf("models_window_open").length;

    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "settings menu");
    const entry = Array.from(document.querySelectorAll(".set-entry")).find(
      (b) => (b as HTMLElement).dataset.act === "models",
    );
    expect(entry, "设置里找不到「模型」入口").toBeTruthy();
    (entry as HTMLElement).click();

    await waitFor(
      () => callsOf("models_window_open").length === before + 1,
      2000,
      "models_window_open invoked",
    );
    // 面板内不再渲染模型管理页（那是独立窗口的 DOM）
    expect(document.getElementById("model-add-open")).toBeNull();
  });

  it("点卡片本体 → 调 models_window_open；面板仍在前台", async () => {
    await bootApp();
    await openPanel();
    const before = callsOf("models_window_open").length;

    document.getElementById("model-chip")!.click();
    await waitFor(
      () => callsOf("models_window_open").length === before + 1,
      2000,
      "models_window_open invoked by chip",
    );
    expect(document.querySelector(".panel"), "面板不该被收起").toBeTruthy();
  });
});

describe("▾ 纯名称下拉（只选不管）", () => {
  it("点 ▾ 出下拉：只有模型名与勾选，没有任何管理按钮", async () => {
    await bootApp();
    await openPanel();
    const dd = await openDropdown();

    expect(dd.querySelector(".mc-mi-name")!.textContent).toContain("Mock 模型");
    expect(dd.querySelector(".mc-mi-check"), "当前模型应有 ✓").toBeTruthy();
    // 「只选不管」：下拉里不允许出现添加/编辑/删除类入口
    expect(dd.querySelector("#model-add-open")).toBeNull();
    expect(dd.querySelector(".set-btn")).toBeNull();
  });

  it("点下拉项 → set_selected_model 被调、✓ 移位、下拉收起", async () => {
    await bootApp();
    await openPanel();
    const dd = await openDropdown();

    dd.querySelector<HTMLButtonElement>(".mc-mi")!.click();
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "dropdown closed");
    const calls = callsOf("set_selected_model");
    expect(calls.length, "应保存选择").toBeGreaterThan(0);
    expect(calls[calls.length - 1].args).toEqual({ id: "Mock 模型" });
  });

  it("点空白处收起下拉，不触发 models_window_open", async () => {
    await bootApp();
    await openPanel();
    await openDropdown();
    const before = callsOf("models_window_open").length;

    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "dropdown closed");
    expect(callsOf("models_window_open").length).toBe(before);
  });

  it("Esc 收起下拉，面板不收成球", async () => {
    await bootApp();
    await openPanel();
    await openDropdown();

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "dropdown closed by Esc");
    expect(document.querySelector(".panel"), "面板不该被收起").toBeTruthy();
  });
});
