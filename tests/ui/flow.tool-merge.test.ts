import { describe, it, expect } from "vitest";
import { bootApp, openPanel, sendText, waitFor } from "./helpers";
import { state, emitEvent } from "./mocks/tauri";

/**
 * 工具行合并（2026-10-02 用户："直接变成执行命令一条就行了"）。
 *
 * 以前每次工具调用占**两行** —— `🔧 列目录`（tool_call）+ `✅ 列目录`（tool_result）。
 * 现在前端在**数据层**把同名相邻的两条并成一条（`pairToolSteps`，见 main.ts），
 * 渲染成一行、点开才看参数与返回。
 *
 * ⚠️ 合并必须在数据层做：时间线是「1 条目 = 1 个顶层 DOM 节点」的契约
 * （`patchTimeline` 靠 `data-n` / `data-i` 定位），渲染层合并会让流式增量错位。
 *
 * 失败**不合并**：后端把工具错误伪装成 `tool_result`（detail 以「工具执行失败」开头），
 * 这里归一成 `tool_error`，画成常显的红黄行 —— 折进一行就等于没人看得见。
 */

/** 直接喂一条磁盘历史消息（比走一遍流式再重载更稳） */
async function renderHistory(steps: { kind: string; name: string | null; detail: string }[]) {
  state.messages = [
    { role: "user", text: "看看", at: Date.now() },
    {
      role: "assistant",
      text: "好了。",
      at: Date.now() + 1,
      steps,
    },
  ];
  await bootApp();
  await openPanel();
  await waitFor(() => !!document.querySelector(".msg.assistant"), 3000, "history rendered");
}

describe("工具调用 + 返回合并成一行", () => {
  it("一次工具调用只占一行，点开参数与返回都能看到", async () => {
    await renderHistory([
      { kind: "tool_call", name: "read_file", detail: '{"path":"a.md"}' },
      { kind: "tool_result", name: "read_file", detail: "FILE_BODY_MARKER\n" + "x".repeat(300) },
    ]);

    const tools = Array.from(document.querySelectorAll<HTMLDetailsElement>("details.tl-tool"));
    expect(tools.length, "调用与返回应合成一行").toBe(1);
    // 前缀按"有没有返回"在 🔧 / ✅ 之间切，文案仍是中文动作名
    expect(tools[0]!.querySelector("summary")!.textContent).toContain("读取文件");
    expect(tools[0]!.querySelector("summary")!.textContent).toContain("✅");
    // 详情块里：参数 + 返回，两段都在
    const pres = Array.from(tools[0]!.querySelectorAll(".tl-tool-pre")).map((p) => p.textContent ?? "");
    expect(pres.some((t) => t.includes("a.md")), "参数要在详情里").toBe(true);
    expect(pres.some((t) => t.includes("FILE_BODY_MARKER")), "返回要在详情里").toBe(true);
    // 返回条目不再单独占一行
    expect(document.querySelector("details.tl-res")).toBeNull();
    // 默认折叠
    expect(tools[0]!.open).toBe(false);
  });

  it("工具失败：不合并，单独一条常显的红黄行", async () => {
    await renderHistory([
      { kind: "tool_call", name: "run_command", detail: '{"command":"boom"}' },
      { kind: "tool_result", name: "run_command", detail: "工具执行失败：boom 不是内部命令" },
    ]);

    // 调用行还在（没被合并成"成功"的样子）
    const tools = Array.from(document.querySelectorAll<HTMLDetailsElement>("details.tl-tool"));
    expect(tools.length).toBe(1);
    // 失败单独一条，且**在折叠行之外**（直接挂在时间线上，常显）
    const warn = document.querySelector<HTMLElement>(".run-timeline > .tl-line.tl-warn");
    expect(warn, "失败要有常显的红黄行").toBeTruthy();
    expect(warn!.textContent).toContain("boom");
    // 它只被「过程」外壳罩着（跑完的整体收起），**不能**被折进那条工具折叠块
    expect(warn!.closest("details.tl-tool"), "失败说明不能被折进工具折叠块").toBeNull();
  });

  it("流式：toolCall 之后再来的 toolResult 是**原地合并**，不多出一个节点", async () => {
    state.chatDelayMs = 2000;
    await bootApp();
    await openPanel();
    await sendText("跑个工具");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    const emit = (p: Record<string, unknown>) =>
      emitEvent("agent-progress", { sessionId: state.currentSessionId, p });

    emit({ kind: "toolCall", name: "read_file", args: '{"path":"a.md"}' });
    await waitFor(
      () => document.querySelectorAll(".run-timeline > details.tl-tool").length === 1,
      2000,
      `tool row: ${document.querySelectorAll(".run-timeline > *").length} nodes`,
    );
    expect(
      document.querySelector(".run-timeline details.tl-tool summary")!.textContent,
      "还没返回时是 🔧",
    ).toContain("🔧");

    emit({ kind: "toolResult", name: "read_file", preview: "FILE_BODY_MARKER" });

    await waitFor(
      () => !!document.querySelector(".run-timeline details.tl-tool summary")?.textContent?.includes("✅"),
      2000,
      "merged row shows ✅",
    );
    // 关键：节点数没变（原地合并），且返回内容已进同一个折叠块
    const nodes = document.querySelectorAll(".run-timeline > *").length;
    expect(nodes, "合并后不该多出一个节点").toBe(1);
    expect(document.querySelector(".run-timeline details.tl-tool")!.textContent).toContain(
      "FILE_BODY_MARKER",
    );

    // 收尾：别把挂起的 chat 请求带进下一个用例
    await waitFor(() => !document.querySelector(".msg.running"), 4000, "run finished");
  });
});
