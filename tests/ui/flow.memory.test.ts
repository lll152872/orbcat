/**
 * 记忆闭环 UI：设置 › 记忆 的展示与批准/驳回按钮
 */
import { describe, it, expect, beforeEach } from "vitest";
import {
  bootApp,
  openPanel,
  waitFor,
  callsOf,
  sleep,
  state,
} from "./helpers";
import { setMock } from "./mocks/tauri";

async function openSettingsThenMemory(): Promise<void> {
  await bootApp();
  await openPanel();
  const gear = document.getElementById("btn-gear") as HTMLButtonElement;
  gear.click();
  // 设置入口页异步渲染，等菜单出现
  await waitFor(() => document.body.textContent?.includes("记忆") ?? false, 3000, "settings menu");
  // 点「记忆」入口（设置菜单项）
  const memEntry = Array.from(document.querySelectorAll("button, .set-entry, [data-view]")).find(
    (el) => (el.textContent ?? "").includes("记忆"),
  ) as HTMLElement | undefined;
  if (!memEntry) throw new Error("设置菜单里找不到记忆入口");
  memEntry.click();
}

describe("记忆审批 UI", () => {
  it("无待审批时显示空态文案", async () => {
    state.memPending = [];
    await openSettingsThenMemory();
    await waitFor(
      () => document.body.textContent?.includes("没有待审批") ?? false,
      3000,
      "empty pending",
    );
  });

  it("有待审批候选时展示内容，并能点批准/驳回", async () => {
    state.memPending = [
      {
        id: "cand_1",
        content: "用户偏好先结论后细节",
        source: "model",
        created_at: "ts0000000001T00:00:00Z",
      },
      {
        id: "cand_2",
        content: "临时状态不该记",
        source: "user",
        created_at: "ts0000000002T00:00:00Z",
      },
    ];
    await openSettingsThenMemory();

    await waitFor(
      () => document.body.textContent?.includes("用户偏好先结论后细节") ?? false,
      3000,
      "pending content",
    );

    const okBtn = document.querySelector<HTMLButtonElement>('.mem-btn.ok[data-id="cand_1"]');
    const noBtn = document.querySelector<HTMLButtonElement>('.mem-btn.no[data-id="cand_2"]');
    expect(okBtn).toBeTruthy();
    expect(noBtn).toBeTruthy();

    okBtn!.click();
    noBtn!.click();

    await waitFor(() => callsOf("mem_approve").length >= 1, 2000, "mem_approve invoked");
    await waitFor(() => callsOf("mem_reject").length >= 1, 2000, "mem_reject invoked");

    expect(callsOf("mem_approve")[0]?.args?.id).toBe("cand_1");
    expect(callsOf("mem_reject")[0]?.args?.id).toBe("cand_2");
  });
});

/**
 * 项目 MEMORY.md 的**用户手改**通道（2026-10-15）
 *
 * 分工：agent 是维护者（追加走 `remember(scope:"project")`，改删走常规
 * `read_file`/`edit_file`），用户是特殊场合兜底 —— 想亲手改一条，
 * 或者 agent 把整份写乱了要收拾。
 */
describe("项目记忆的手改编辑器", () => {
  async function openMemoryView(): Promise<void> {
    await bootApp();
    await openPanel();
    (document.getElementById("btn-gear") as HTMLButtonElement).click();
    // ⚠️ 判据是"记忆"而不是"项目记忆"：设置**入口页**上只有摘要
    //    （"技能 N 个 · 记忆文件 · 项目绑定"），"项目记忆"要进到记忆页才有。
    await waitFor(
      () => document.body.textContent?.includes("记忆") ?? false,
      3000,
      "settings menu",
    );
    const entry = Array.from(document.querySelectorAll(".set-entry")).find((el) =>
      (el.textContent ?? "").includes("记忆"),
    ) as HTMLButtonElement | undefined;
    if (!entry) throw new Error("设置菜单里找不到记忆入口");
    entry.click();
    await waitFor(
      () => !!document.querySelector(".skill-item"),
      3000,
      "project list",
    );
  }

  beforeEach(() => {
    state.memPending = [];
    state.projectMemory = {};
    state.invokeOverrides = {};
  });

  it("每个项目都有「编辑记忆」按钮（不论有没有绑定路径）", async () => {
    // ⚠️ 「改路径/归档」只在未绑定或路径失效时才出现；「编辑记忆」必须**人人都有**
    //    —— 未绑定路径的项目照样会往里写（agent 是按项目名读它的）。
    setMock("mem_projects", [
      { name: "论文", source: "", sourceAlive: false, hasMemory: false },
      { name: "悬浮小agent", source: "D:\\a\\orbcat", sourceAlive: true, hasMemory: true },
    ]);
    await openMemoryView();
    const btns = document.querySelectorAll<HTMLButtonElement>('.proj-edit-mem[data-proj]');
    expect(btns.length, "两个项目都该有编辑入口").toBe(2);
  });

  it("点开 → 读到现有正文并显示在编辑器里", async () => {
    setMock("mem_projects", [
      { name: "论文", source: "", sourceAlive: false, hasMemory: true },
    ]);
    state.projectMemory = { 论文: "# 项目记忆 — 论文\n\n- 图 3 证明模型在读病历\n" };
    await openMemoryView();

    document.querySelector<HTMLButtonElement>('.proj-edit-mem[data-proj="论文"]')!.click();
    await waitFor(() => !!document.querySelector(".proj-mem-editor"), 3000, "editor opened");

    const ta = document.querySelector<HTMLTextAreaElement>(".proj-mem-editor")!;
    expect(ta.value).toContain("图 3 证明模型在读病历");
    expect(callsOf("pmem_memory_read")[0]?.args?.name).toBe("论文");
  });

  it("保存 → 真的写回去（不是只弹个提示）", async () => {
    setMock("mem_projects", [
      { name: "论文", source: "", sourceAlive: false, hasMemory: true },
    ]);
    state.projectMemory = { 论文: "- 旧的一条\n" };
    await openMemoryView();
    document.querySelector<HTMLButtonElement>('.proj-edit-mem[data-proj="论文"]')!.click();
    await waitFor(() => !!document.querySelector(".proj-mem-editor"), 3000, "editor");

    const ta = document.querySelector<HTMLTextAreaElement>(".proj-mem-editor")!;
    ta.value = "- 新的一条\n";
    document.querySelector<HTMLButtonElement>(".proj-mem-save")!.click();

    await waitFor(() => callsOf("pmem_memory_write").length > 0, 2000, "saved");
    expect(state.projectMemory["论文"]).toBe("- 新的一条\n");
  });

  it("提示里写清「备份按天只留一份」，别让人误以为有历史版本", async () => {
    setMock("mem_projects", [
      { name: "论文", source: "", sourceAlive: false, hasMemory: true },
    ]);
    state.projectMemory = { 论文: "- x\n" };
    await openMemoryView();
    document.querySelector<HTMLButtonElement>('.proj-edit-mem[data-proj="论文"]')!.click();
    await waitFor(() => !!document.querySelector(".proj-mem-hint"), 3000, "hint");

    const hint = document.querySelector(".proj-mem-hint")?.textContent ?? "";
    expect(hint).toContain("备份");
    expect(hint).toMatch(/同一天/);
  });

  it("取消 → 不写盘", async () => {
    setMock("mem_projects", [
      { name: "论文", source: "", sourceAlive: false, hasMemory: true },
    ]);
    state.projectMemory = { 论文: "- 原来的\n" };
    await openMemoryView();
    document.querySelector<HTMLButtonElement>('.proj-edit-mem[data-proj="论文"]')!.click();
    await waitFor(() => !!document.querySelector(".proj-mem-editor"), 3000, "editor");
    document.querySelector<HTMLTextAreaElement>(".proj-mem-editor")!.value = "- 改坏了\n";
    document.querySelector<HTMLButtonElement>(".proj-mem-cancel")!.click();

    await sleep(0);
    expect(callsOf("pmem_memory_write").length, "取消不该写盘").toBe(0);
    expect(state.projectMemory["论文"]).toBe("- 原来的\n");
  });

  it("多行正文里 Enter 是换行，不是保存", async () => {
    setMock("mem_projects", [
      { name: "论文", source: "", sourceAlive: false, hasMemory: true },
    ]);
    state.projectMemory = { 论文: "- x\n" };
    await openMemoryView();
    document.querySelector<HTMLButtonElement>('.proj-edit-mem[data-proj="论文"]')!.click();
    await waitFor(() => !!document.querySelector(".proj-mem-editor"), 3000, "editor");

    const ta = document.querySelector<HTMLTextAreaElement>(".proj-mem-editor")!;
    ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await sleep(0);
    expect(
      callsOf("pmem_memory_write").length,
      "Enter 必须照常换行（Ctrl+S 才保存）",
    ).toBe(0);

    ta.dispatchEvent(
      new KeyboardEvent("keydown", { key: "s", ctrlKey: true, bubbles: true }),
    );
    await waitFor(() => callsOf("pmem_memory_write").length > 0, 2000, "ctrl+s saved");
  });
});
