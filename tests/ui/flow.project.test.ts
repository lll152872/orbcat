import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { setMock, callsOf } from "./mocks/tauri";

/**
 * 「项目」（拼装包）设置页
 *
 * 锁的契约：
 *   ① 设置入口页有「项目」入口，能进；
 *   ② 没有项目时是**空态**（不该假装有什么）；
 *   ③ 激活一个项目真的调 `project_activate`；
 *   ④ **权限预设被丢弃时必须显眼告警** —— 这是 fail-closed 的用户可见面，
 *      悄悄不生效比报错更糟（用户以为预设生效了）；
 *   ⑤ 引用失效要逐条列出来（哪个类别、哪个 id、为什么）。
 */

async function openProject(): Promise<HTMLElement> {
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-entry"), 3000, "设置入口页");
  const entry = Array.from(document.querySelectorAll<HTMLElement>(".set-entry")).find(
    (b) => b.dataset.act === "project",
  );
  if (!entry) throw new Error("设置里找不到「项目」入口");
  entry.click();
  await waitFor(() => !!document.getElementById("proj-edit-body"), 3000, "项目页渲染");
  return document.getElementById("panel-body")!;
}

describe("设置 › 项目", () => {
  it("入口存在，且空态说清「没有项目」而不是假装有", async () => {
    await bootApp();
    await openPanel();
    await openProject();
    const body = document.getElementById("panel-body")!;
    expect(body.textContent).toContain("当前没有激活任何项目");
    // 空列表要给出解释（项目是什么），不能只留一片空白
    expect(body.textContent).toContain("还没有项目包");
    // 编辑器必须在（新建入口）
    expect(document.getElementById("proj-edit-id")).toBeTruthy();
    expect(document.getElementById("proj-save")).toBeTruthy();
  });

  it("有项目时列出，点「激活」真的调 project_activate", async () => {
    setMock("project_list", {
      bundles: [
        {
          id: "学习助手",
          name: "学习助手",
          description: "学习通提醒 + token 看板",
          active: false,
          valid: true,
          error: null,
          skillCount: 2,
          toolCount: 1,
          hasPermPreset: true,
        },
      ],
      active: null,
      unresolved: [],
      permError: null,
      tools: [],
    });

    await bootApp();
    await openPanel();
    await openProject();

    const body = document.getElementById("panel-body")!;
    expect(body.textContent).toContain("学习助手");
    expect(body.textContent).toContain("学习通提醒");

    const btn = body.querySelector<HTMLButtonElement>(".proj-activate")!;
    expect(btn, "应有激活按钮").toBeTruthy();
    btn.click();
    await waitFor(() => callsOf("project_activate").length > 0, 3000, "调用 project_activate");

    const call = callsOf("project_activate").at(-1)!;
    expect(call.args?.id).toBe("学习助手");
  });

  it("激活中的项目显示「当前」而不是再给一个激活按钮", async () => {
    setMock("project_list", {
      bundles: [
        {
          id: "p1",
          name: "我的项目",
          description: "",
          active: true,
          valid: true,
          error: null,
          skillCount: 0,
          toolCount: 0,
          hasPermPreset: false,
        },
      ],
      active: "p1",
      unresolved: [],
      permError: null,
      tools: [],
    });

    await bootApp();
    await openPanel();
    await openProject();

    const body = document.getElementById("panel-body")!;
    expect(body.textContent).toContain("当前");
    expect(body.querySelector(".proj-activate"), "激活中的不该再有激活按钮").toBeNull();
    // 但必须给退出口
    expect(document.getElementById("proj-deactivate")).toBeTruthy();
  });

  it("权限预设被丢弃 → 页面上必须显眼告警（fail-closed 的用户可见面）", async () => {
    setMock("project_list", {
      bundles: [
        {
          id: "evil",
          name: "可疑的包",
          description: "",
          active: true,
          valid: true,
          error: null,
          skillCount: 0,
          toolCount: 0,
          hasPermPreset: true,
        },
      ],
      active: "evil",
      unresolved: [],
      permError: "commandAllowExtra 里的「shutdown*」会匹配硬阻断模式「shutdown*」",
      tools: [],
    });

    await bootApp();
    await openPanel();
    await openProject();

    const body = document.getElementById("panel-body")!;
    const warn = body.querySelector(".proj-warn");
    expect(warn, "必须有告警块（悄悄不生效比报错更糟）").toBeTruthy();
    expect(warn!.textContent).toContain("权限预设已整份丢弃");
    // 要说清"当前用的是你自己的设置"，否则用户不知道现在到底是什么状态
    expect(warn!.textContent).toContain("你自己的权限设置");
  });

  it("引用失效逐条列出（类别 / id / 原因）", async () => {
    setMock("project_list", {
      bundles: [
        {
          id: "p",
          name: "p",
          description: "",
          active: true,
          valid: true,
          error: null,
          skillCount: 0,
          toolCount: 0,
          hasPermPreset: false,
        },
      ],
      active: "p",
      unresolved: [
        ["model", "别人的模型", "models.json 里没有这个 id"],
        ["tool", "check_homework", "脚本不存在：D:\\x\\check.ps1"],
      ],
      permError: null,
      tools: [],
    });

    await bootApp();
    await openPanel();
    await openProject();

    const body = document.getElementById("panel-body")!;
    expect(body.textContent).toContain("2 项引用没解析上");
    expect(body.textContent).toContain("别人的模型");
    expect(body.textContent).toContain("models.json 里没有这个 id");
    expect(body.textContent).toContain("check_homework");
  });

  it("装配失败的项目仍然列出来并标出原因（不能隐藏，否则用户修不了）", async () => {
    setMock("project_list", {
      bundles: [
        {
          id: "broken",
          name: "broken",
          description: "",
          active: false,
          valid: false,
          error: "schemaVersion=99 不认识（本版本支持 1）",
          skillCount: 0,
          toolCount: 0,
          hasPermPreset: false,
        },
      ],
      active: null,
      unresolved: [],
      permError: null,
      tools: [],
    });

    await bootApp();
    await openPanel();
    await openProject();

    const body = document.getElementById("panel-body")!;
    expect(body.textContent).toContain("broken");
    expect(body.textContent).toContain("schemaVersion=99");
    // 无效包不该给「激活」按钮（点了也只会报错）
    expect(body.querySelector(".proj-activate")).toBeNull();
  });

  it("当前包的脚本工具会列出来，并说明与 run_command 同闸门", async () => {
    setMock("project_list", {
      bundles: [
        {
          id: "p",
          name: "p",
          description: "",
          active: true,
          valid: true,
          error: null,
          skillCount: 0,
          toolCount: 1,
          hasPermPreset: false,
        },
      ],
      active: "p",
      unresolved: [],
      permError: null,
      tools: [
        {
          name: "check_homework",
          description: "查询学习通作业快照",
          template: ["pwsh", "-NoProfile", "-File", "{project}/tools/check.ps1"],
          perm: "read-only",
        },
      ],
    });

    await bootApp();
    await openPanel();
    await openProject();

    const body = document.getElementById("panel-body")!;
    expect(body.textContent).toContain("check_homework");
    expect(body.textContent).toContain("查询学习通作业快照");
    // 用户要知道它跑起来受什么约束
    expect(body.textContent).toContain("同一套闸门");
  });

  it("保存会把编辑器内容原样交给后端（校验在后端，前端不重复实现）", async () => {
    await bootApp();
    await openPanel();
    await openProject();

    const idEl = document.getElementById("proj-edit-id") as HTMLInputElement;
    const bodyEl = document.getElementById("proj-edit-body") as HTMLTextAreaElement;
    idEl.value = "新项目";
    const json = '{"schemaVersion":1,"name":"新项目"}';
    bodyEl.value = json;

    document.getElementById("proj-save")!.click();
    await waitFor(() => callsOf("project_save").length > 0, 3000, "调用 project_save");

    const call = callsOf("project_save").at(-1)!;
    expect(call.args?.id).toBe("新项目");
    expect(call.args?.body).toBe(json);
  });

  it("空 id 或空内容不发请求（前端挡掉明显无效的提交）", async () => {
    await bootApp();
    await openPanel();
    await openProject();

    // 空 id
    (document.getElementById("proj-edit-body") as HTMLTextAreaElement).value = "{}";
    document.getElementById("proj-save")!.click();
    await new Promise((r) => setTimeout(r, 50));
    expect(callsOf("project_save").length, "空 id 不该提交").toBe(0);
    expect(document.getElementById("proj-msg")!.textContent).toContain("id");

    // 有 id 但内容空
    (document.getElementById("proj-edit-id") as HTMLInputElement).value = "p";
    (document.getElementById("proj-edit-body") as HTMLTextAreaElement).value = "   ";
    document.getElementById("proj-save")!.click();
    await new Promise((r) => setTimeout(r, 50));
    expect(callsOf("project_save").length, "空内容不该提交").toBe(0);
  });

  it("「填入模板」给出一份能直接用的骨架，且含 schemaVersion", async () => {
    await bootApp();
    await openPanel();
    await openProject();

    document.getElementById("proj-new-template")!.click();
    const body = (document.getElementById("proj-edit-body") as HTMLTextAreaElement).value;
    const parsed = JSON.parse(body);
    expect(parsed.schemaVersion, "模板必须带 schemaVersion（缺了会被拒载）").toBe(1);
    expect(Array.isArray(parsed.tools)).toBe(true);
    // 模板里的脚本工具必须是 argv 数组（不是整串命令）
    expect(Array.isArray(parsed.tools[0].commandTemplate)).toBe(true);
    // 模板不该自带任何密钥字段
    expect(body).not.toMatch(/apiKey|api_key|token/i);
  });
});
