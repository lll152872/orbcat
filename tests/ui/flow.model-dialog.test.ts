import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { callsOf, state } from "./mocks/tauri";

/**
 * 模型交互（2026-10-02 改定）：
 *   选择模型  → 留在面板：点**整颗 chip**（本体或 ▾）出「模型列表」，
 *               条目 = 组别 + 模型名两行，底部单独一颗「＋ 添加自定义模型」
 *   编辑/管理 → 弹出独立第二窗口，入口 = 设置菜单「🧩 模型」
 *
 * 旧定稿「点卡片本体 = 管理入口」已被用户推翻（他说点开该是列表不是管理窗口）。
 * jsdom 里无法渲染第二个 Tauri 窗口 —— 所以这里锁的是**入口与选择**，
 * 独立窗口内部（表格/表单/分组）靠 MCP 截图验收。
 */

/** 点 ▾ 打开模型列表，等它出现 */
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

  it("点 chip 本体 → 出模型列表，**不**调 models_window_open、面板不收", async () => {
    await bootApp();
    await openPanel();
    const before = callsOf("models_window_open").length;

    document.getElementById("model-chip")!.click();
    await waitFor(() => !!document.getElementById("models-dropdown"), 2000, "list opened");
    expect(callsOf("models_window_open").length, "点 chip 不该弹管理窗口").toBe(before);
    expect(document.querySelector(".panel"), "面板不该被收起").toBeTruthy();

    // 再点一下收起（chip 自己是开关）
    document.getElementById("model-chip")!.click();
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "list closed");
  });
});

describe("模型列表（只选不管 + 底部添加）", () => {
  it("条目是「组别 + 模型名」两行，当前模型带 ✓", async () => {
    await bootApp();
    await openPanel();
    const dd = await openDropdown();

    // 身份 = (Base URL, 接口 id)（2026-10-02）：第二行显示接口 id
    expect(dd.querySelector(".mc-mi-name")!.textContent).toContain("mock-1");
    // 第一行是组别（chip 上那行小字，同一个概念）
    const group = dd.querySelector(".mc-mi-group");
    expect(group, "条目要有组别行").toBeTruthy();
    expect(group!.textContent!.trim().length).toBeGreaterThan(0);
    expect(dd.querySelector(".mc-mi-check"), "当前模型应有 ✓").toBeTruthy();
    // 「只选不管」：列表里不允许出现添加表单/编辑删除类控件
    expect(dd.querySelector("#model-add-open")).toBeNull();
    expect(dd.querySelector(".set-btn")).toBeNull();
  });

  it("「＋ 添加自定义模型」排在最后、用分隔线隔开，点了开窗口且不乱切模型", async () => {
    await bootApp();
    await openPanel();
    localStorage.removeItem("orbcat.models.intent"); // 上个用例的残留清掉
    const dd = await openDropdown();
    const before = callsOf("models_window_open").length;
    const selBefore = callsOf("set_selected_model").length;

    const add = dd.querySelector<HTMLButtonElement>(".mc-mi-add");
    expect(add, "底部应有「添加自定义模型」").toBeTruthy();
    expect(add!.textContent).toContain("添加自定义模型");
    // 位置上必须在所有模型条目之后，且前面有分隔线
    const kids = Array.from(dd.children);
    expect(kids.indexOf(add!)).toBe(kids.length - 1);
    expect(dd.querySelector(".mc-menu-sep"), "添加条目要与其他条目分隔").toBeTruthy();
    expect(add!.previousElementSibling?.classList.contains("mc-menu-sep")).toBe(true);

    add!.click();
    await waitFor(
      () => callsOf("models_window_open").length === before + 1,
      2000,
      "add opens models window",
    );
    // 添加是"动作"不是"选项"：不该顺手改当前模型
    expect(callsOf("set_selected_model").length).toBe(selBefore);
    // 一次性意图位留给模型窗口消费（窗口那边据此直接落到添加表单）
    expect(localStorage.getItem("orbcat.models.intent")).toBe("add");
  });

  it("同 id 两条 + 设置里没记 url → 只标一条当前，并把 url 回填进设置", async () => {
    // 复刻 2026-10-02 用户报的现场：models.json 里两条 id 同为 glm-5.3-flash（Base URL 不同），
    // 而 settings 里只有 id、没有 url → 旧判定式 `id 相同 && (url 为空 || …)` 会把
    // **所有**同 id 的行都标成当前（更要命的是把 ✓ 画到了每一行上）。
    state.models = [
      {
        id: "dup-model",
        name: "dup-model",
        vendor: "Test",
        url: "https://a.example.com/v1",
        supportsToolCall: true,
        supportsImages: false,
      },
      {
        id: "dup-model",
        name: "dup-model",
        vendor: "Test",
        url: "https://b.example.com/v1",
        supportsToolCall: true,
        supportsImages: false,
      },
    ];
    state.selectedModel = "dup-model";
    state.selectedModelUrl = null; // 旧设置：只有 id
    await bootApp();
    await openPanel();

    const dd = await openDropdown();
    const items = Array.from(dd.querySelectorAll<HTMLButtonElement>(".mc-mi[data-id]"));
    expect(items.length).toBe(2);
    // 关键断言：✓ 只能有一个（回归：曾因把对象当布尔用，给每一行都画了 ✓）
    expect(dd.querySelectorAll(".mc-mi-check").length, "同 id 两条只能标一条当前").toBe(1);
    expect(dd.querySelectorAll(".mc-mi.cur").length, "高亮也只能有一条").toBe(1);
    // 被标中的那条 = 解析出来的那条（第一个），并且设置已回填
    const marked = items.find((b) => b.querySelector(".mc-mi-check"))!;
    expect(marked.dataset.url).toBe("https://a.example.com/v1");
    const sel = callsOf("set_selected_model");
    expect(sel.length, "启动时应把解析出的 url 回写设置").toBeGreaterThan(0);
    expect(sel[sel.length - 1].args).toEqual({
      id: "dup-model",
      url: "https://a.example.com/v1",
    });
  });

  it("点模型条目 → set_selected_model 被调、列表收起", async () => {
    await bootApp();
    await openPanel();
    const dd = await openDropdown();

    dd.querySelector<HTMLButtonElement>(".mc-mi[data-id]")!.click();
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "dropdown closed");
    const calls = callsOf("set_selected_model");
    expect(calls.length, "应保存选择").toBeGreaterThan(0);
    expect(calls[calls.length - 1].args).toEqual({
      id: "mock-1",
      url: "http://127.0.0.1:9/v1",
    });
  });

  it("点空白处收起列表，不触发 models_window_open", async () => {
    await bootApp();
    await openPanel();
    await openDropdown();
    const before = callsOf("models_window_open").length;

    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "dropdown closed");
    expect(callsOf("models_window_open").length).toBe(before);
  });

  it("Esc 收起列表，面板不收成球", async () => {
    await bootApp();
    await openPanel();
    await openDropdown();

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await waitFor(() => !document.getElementById("models-dropdown"), 2000, "dropdown closed by Esc");
    expect(document.querySelector(".panel"), "面板不该被收起").toBeTruthy();
  });
});
