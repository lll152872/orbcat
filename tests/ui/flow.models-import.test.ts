import { describe, it, expect } from "vitest";
import { waitFor } from "./helpers";
import { callsOf, state } from "./mocks/tauri";

/**
 * 「模型管理」窗口的三处新能力（回归锁）：
 *
 *   1. 优化1 · 模型导入能解析 OpenRouter 风格的描述
 *      （`context_length` / `max_completion_tokens` / `capabilities.tools|vision`）
 *      —— 粘 JSON 导入走的是**后端同一个解析器**（`models_parse_spec`）。
 *   2. bug1 · API Key 留空要能**继承同组 Key**，
 *      否则「获取 ID」永远拉不到列表（表单里明明写着"留空自动继承"）。
 *   3. bug2 · 组菜单里要能**改 Base URL**（以前只能逐个模型点编辑）。
 *
 * models.ts 在模块顶层 boot()，所以每个用例必须先 `vi.resetModules()`
 * 再动态 import，否则只有第一个用例能拿到渲染结果。
 */

/** 加载 models 窗口入口并等它渲染完 */
async function bootModels(): Promise<void> {
  const { vi } = await import("vitest");
  vi.resetModules();
  document.body.innerHTML = `<div id="models-app"></div>`;
  await import("../../src/models");
  await waitFor(() => !!document.querySelector(".mw-window"), 3000, "models 窗口渲染");
}

/** 打开「添加模型」表单（跳过类型选择那一步） */
async function openAddForm(): Promise<void> {
  document.getElementById("model-add-open")!.click();
  await waitFor(() => !!document.getElementById("mm-type-custom"), 2000, "类型选择弹出");
  document.getElementById("mm-type-custom")!.click();
  await waitFor(() => !!document.getElementById("f-url"), 2000, "添加表单弹出");
}

function setValue(sel: string, v: string): void {
  const el = document.querySelector(sel) as HTMLInputElement | HTMLTextAreaElement;
  el.value = v;
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("模型窗口 · OpenRouter 风格描述（优化1）", () => {
  it("粘 JSON → 解析 → 点一行把窗口/能力都填进表单", async () => {
    await bootModels();
    await openAddForm();

    // 用户从文档里粘过来的就是这种形态（单个对象，不是数组）
    setValue(
      "#f-json",
      JSON.stringify({
        id: "nemotron-3.5-lightning-free",
        context_length: 262144,
        max_completion_tokens: 262144,
        capabilities: { vision: false, tools: true, reasoning: true },
      }),
    );
    document.getElementById("f-parse-json")!.click();

    await waitFor(() => !!document.querySelector(".id-pick"), 3000, "解析结果列表");
    const parseCall = callsOf("models_parse_spec")[0];
    expect(parseCall, "解析要走后端（跟拉列表同一个解析器）").toBeTruthy();
    expect(String(parseCall!.args?.payload)).toContain("nemotron-3.5-lightning-free");

    // 点那一行 → 模型 ID / 窗口 / 最大输出 / 工具能力都进表单
    (document.querySelector(".id-pick") as HTMLButtonElement).click();
    await waitFor(() => (document.getElementById("f-id") as HTMLInputElement).value !== "", 2000, "填入模型 ID");

    expect((document.getElementById("f-id") as HTMLInputElement).value).toBe(
      "nemotron-3.5-lightning-free",
    );
    expect((document.getElementById("f-max-in") as HTMLInputElement).value).toBe("262144");
    expect((document.getElementById("f-max-out") as HTMLInputElement).value).toBe("262144");
    expect((document.getElementById("f-tool") as HTMLInputElement).checked).toBe(true);
    expect((document.getElementById("f-img") as HTMLInputElement).checked).toBe(false);
  });

  it("批量导入把逐模型的窗口/能力一起传给后端", async () => {
    await bootModels();
    await openAddForm();
    setValue("#f-url", "http://127.0.0.1:9/v1");

    // 「获取 ID」→ 列表（mock 返回带能力的两项）
    document.getElementById("f-fetch-ids")!.click();
    await waitFor(() => !!document.querySelector(".id-pick"), 3000, "拉取模型列表");
    document.getElementById("picker-batch")!.click();
    await waitFor(() => callsOf("models_import_remote").length > 0, 3000, "批量导入");

    const args = callsOf("models_import_remote")[0].args as Record<string, unknown>;
    expect(args.maxOutputTokens, "最大输出要逐模型带过去").toBeTruthy();
    expect(args.toolFlags, "工具能力要逐模型带过去").toBeTruthy();
    expect(args.imageFlags, "视觉能力要逐模型带过去").toBeTruthy();
  });
});

describe("模型窗口 · API Key 继承（bug1）", () => {
  it("Base URL 命中已有组 → 提示「留空即自动继承」", async () => {
    state.models = [
      {
        id: "mock-1",
        name: "Mock One",
        vendor: "Test",
        url: "http://39.105.206.31:8792/v1",
        supportsToolCall: true,
        supportsImages: true,
        hasKey: true,
      },
    ];
    await bootModels();
    await openAddForm();

    setValue("#f-url", "http://39.105.206.31:8792/v1");
    await waitFor(
      () => (document.getElementById("f-key-hint")?.textContent ?? "").includes("自动继承"),
      2000,
      "同组 Key 继承提示",
    );
    expect(document.getElementById("f-key-hint")!.textContent).toContain("不用再粘一遍");
  });

  it("Key 留空也照样能点「获取 ID」（后端负责继承）", async () => {
    state.models = [
      {
        id: "mock-1",
        name: "Mock One",
        vendor: "Test",
        url: "http://39.105.206.31:8792/v1",
        supportsToolCall: true,
        supportsImages: true,
        hasKey: true,
      },
    ];
    await bootModels();
    await openAddForm();
    setValue("#f-url", "http://39.105.206.31:8792/v1");
    // Key 故意留空
    setValue("#f-key", "");

    document.getElementById("f-fetch-ids")!.click();
    await waitFor(() => callsOf("models_fetch_remote").length > 0, 3000, "发起拉取");

    const args = callsOf("models_fetch_remote")[0].args as Record<string, unknown>;
    expect(args.apiKey, "留空即可，继承在后端做").toBe("");
    expect(args.url).toBe("http://39.105.206.31:8792/v1");
  });
});

describe("模型窗口 · 整组改 Base URL（bug2）", () => {
  it("组菜单里有「修改 Base URL」，并把新 URL 发给后端", async () => {
    state.models = [
      {
        id: "mock-1",
        name: "Mock One",
        vendor: "Test",
        url: "http://39.105.206.31:8792/v1",
        supportsToolCall: true,
        supportsImages: true,
        hasKey: true,
      },
    ];
    await bootModels();

    // 打开组菜单
    document.querySelector<HTMLButtonElement>(".lm-gear")!.click();
    await waitFor(() => !!document.querySelector(".lm-menu"), 2000, "组菜单打开");

    const item = [...document.querySelectorAll<HTMLButtonElement>(".lm-mi")].find(
      (b) => b.dataset.act === "change-url",
    );
    expect(item, "组菜单里必须有「修改 Base URL」这一格").toBeTruthy();

    item!.click();
    // 输入框（askInput）→ 填新 URL → 确定
    await waitFor(() => !!document.querySelector(".confirm-input"), 2000, "输入框弹出");
    const input = document.querySelector<HTMLInputElement>(".confirm-input")!;
    input.value = "http://39.105.206.31:8792/v2";
    document.querySelector<HTMLButtonElement>('.confirm-actions [data-k="yes"]')!.click();

    await waitFor(() => callsOf("models_change_group_url").length > 0, 3000, "改组 URL");
    const args = callsOf("models_change_group_url")[0].args as Record<string, unknown>;
    expect(args.url).toBe("http://39.105.206.31:8792/v1");
    expect(args.newUrl).toBe("http://39.105.206.31:8792/v2");
  });
});
