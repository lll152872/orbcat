/**
 * 工具页（设计稿 09）与设置页搜索（设计稿 04）的 UI 行为。
 *
 * 这两页是 11 页对账的第二轮**结构重做**（组块 / 折叠 / 行内开关 / 搜索），
 * 断言要钉住的是**结构与行为**，颜色另有对比度体检脚本管。
 */
import { describe, it, expect, beforeEach } from "vitest";
import { bootApp, openPanel, waitFor, callsOf, state } from "./helpers";
import { setMock } from "./mocks/tauri";

/** 设置 › 工具（MCP 外部工具）子页 */
async function openToolsView(): Promise<void> {
  await bootApp();
  await openPanel();
  (document.getElementById("btn-gear") as HTMLButtonElement).click();
  await waitFor(
    () => document.body.textContent?.includes("记忆") ?? false,
    3000,
    "settings menu",
  );
  const row = document.querySelector<HTMLElement>('.set-row[data-act="mcp"]');
  if (!row) throw new Error("设置菜单里找不到「MCP 工具组」入口");
  row.click();
  await waitFor(() => !!document.querySelector(".mm-group"), 3000, "tools groups");
}

/** 设置入口页 */
async function openSettingsView(): Promise<void> {
  await bootApp();
  await openPanel();
  (document.getElementById("btn-gear") as HTMLButtonElement).click();
  await waitFor(() => !!document.querySelector(".set-acc"), 3000, "settings accordion");
}

describe("工具页（设计稿 09）", () => {
  beforeEach(() => {
    state.memPending = [];
    state.invokeOverrides = {};
    setMock("builtin_tools", [
      { name: "read_file", desc: "读取一个文本文件的内容" },
      { name: "write_file", desc: "写入文件" },
    ]);
    setMock("skills_list", [{ name: "my-skill", description: "一个技能", path: "x" }]);
  });

  it("三个组块都在：内置工具 / MCP 外部工具 / 技能加载的工具", async () => {
    await openToolsView();
    const groups = Array.from(document.querySelectorAll(".mm-group")).map(
      (g) => g.getAttribute("data-group") ?? "",
    );
    expect(groups).toContain("builtin");
    expect(groups).toContain("mcp");
    expect(groups).toContain("skills");
  });

  it("内置工具组默认展开，列出工具行；另两组默认折叠", async () => {
    await openToolsView();
    const builtin = document.querySelector<HTMLElement>('.mm-group[data-group="builtin"]')!;
    expect(builtin.querySelector(".mm-group-body")?.hasAttribute("hidden")).toBe(false);
    expect(builtin.querySelector(".mm-tool-row")).toBeTruthy();
    expect(document.body.textContent).toContain("read_file");

    const mcp = document.querySelector<HTMLElement>('.mm-group[data-group="mcp"]')!;
    expect(mcp.querySelector(".mm-group-body")?.hasAttribute("hidden")).toBe(true);
  });

  it("点组头的 ▸ 能展开/收起", async () => {
    await openToolsView();
    const mcp = document.querySelector<HTMLElement>('.mm-group[data-group="mcp"]')!;
    const fold = mcp.querySelector<HTMLButtonElement>(".mm-fold")!;
    fold.click();
    expect(mcp.querySelector(".mm-group-body")?.hasAttribute("hidden")).toBe(false);
    fold.click();
    expect(mcp.querySelector(".mm-group-body")?.hasAttribute("hidden")).toBe(true);
  });

  it("内置工具开关 → 真的调后端 set_builtin_tools", async () => {
    await openToolsView();
    const sw = document.querySelector<HTMLButtonElement>("#mm-sw-builtin")!;
    expect(sw).toBeTruthy();
    sw.click();
    await waitFor(() => callsOf("set_builtin_tools").length >= 1, 2000, "set_builtin_tools");
    expect(callsOf("set_builtin_tools")[0]?.args?.enable).toBe(false);
  });

  it("底部状态栏显示「工具 · N 组 · M 个工具」", async () => {
    await openToolsView();
    await waitFor(
      () => (document.getElementById("panel-status")?.textContent ?? "").includes("工具"),
      2000,
      "status bar",
    );
    const txt = document.getElementById("panel-status")?.textContent ?? "";
    expect(txt).toMatch(/\d+ 组 · \d+ 个工具/);
  });

  it("说明块与权限闸门提示都在（设计稿 09:2:1548 / 2:1551）", async () => {
    await openToolsView();
    expect(document.querySelector(".mm-note")).toBeTruthy();
    expect(document.querySelector(".mm-warn")).toBeTruthy();
    expect(document.body.textContent).toContain("工具可见 ≠ 有权限");
  });
});

describe("设置页搜索（设计稿 04）", () => {
  beforeEach(() => {
    state.memPending = [];
    state.invokeOverrides = {};
  });

  it("标题条右组有搜索按钮，点它展开输入框", async () => {
    await openSettingsView();
    const btn = document.querySelector<HTMLButtonElement>("#sb-set-search")!;
    expect(btn).toBeTruthy();
    expect(document.querySelector("#set-search-input")).toBeFalsy();
    btn.click();
    await waitFor(() => !!document.querySelector("#set-search-input"), 2000, "search input");
  });

  it("输入关键词 → 只留命中的行，别的组整组藏起来", async () => {
    await openSettingsView();
    document.querySelector<HTMLButtonElement>("#sb-set-search")!.click();
    await waitFor(() => !!document.querySelector("#set-search-input"), 2000, "search input");

    const input = document.querySelector<HTMLInputElement>("#set-search-input")!;
    input.value = "命令策略";
    input.dispatchEvent(new Event("input"));

    await waitFor(
      () => {
        const rows = Array.from(document.querySelectorAll<HTMLElement>(".set-sec-body .set-row"));
        const visible = rows.filter((r) => r.style.display !== "none");
        return visible.length === 1 && (visible[0].textContent ?? "").includes("命令策略");
      },
      2000,
      "filtered rows",
    );
  });
});
