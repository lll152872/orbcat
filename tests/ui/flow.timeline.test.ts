import { describe, it, expect } from "vitest";
import { bootApp, openPanel, sendText, waitFor, sleep } from "./helpers";
import { state, emitEvent, callsOf } from "./mocks/tauri";

/**
 * 流式时间线：思考与工具必须**交错**，出错**不丢**本轮流。
 *
 * 这两条对应 2026-09-22 用户报的两个问题：
 * ① 「他做的都消失了」—— 撞 429 后屏幕上只剩一条红字
 * ② 「思考都堆在一块」—— 所有思考挤成一个块、工具另起一处
 *
 * 断言直接打在 DOM 顺序上（`.run-timeline > *`），因为问题本身就是顺序问题。
 */

/** 时间线里每个子节点的「种类」（供顺序断言） */
function timelineKinds(scope = document): string[] {
  return Array.from(scope.querySelectorAll(".run-timeline > *")).map((n) => {
    if (n.classList.contains("tl-reason")) return "reasoning";
    if (n.classList.contains("tl-error")) return "error";
    if (n.classList.contains("tl-status")) return "status";
    if (n.classList.contains("tl-warn")) return "tool_error";
    if (n.classList.contains("tl-text")) return "text";
    return "tool";
  });
}

function timelineText(scope = document): string {
  return Array.from(scope.querySelectorAll(".run-timeline > *"))
    .map((n) => (n.textContent ?? "").trim())
    .join(" | ");
}

describe("流式时间线", () => {
  /** 思考块数量（失败时把现场打出来，顺序类问题最难从"still false"里看出来） */
  function reasonBlocks(): number {
    return document.querySelectorAll(".run-timeline > .tl-reason").length;
  }

  function timelineState(): string {
    const run = document.querySelector(".msg.running");
    return `running=${run ? "yes" : "no"} blocks=${reasonBlocks()} kinds=[${timelineKinds().join(
      ",",
    )}]`;
  }

  /**
   * 推一条 agent 进度事件。
   *
   * ⚠️ **必须走 `{sessionId, p}` 信封**（2026-09-30 修）：
   * 后端早已把进度事件改成带会话 id 的信封（`lib.rs::chat` 的
   * `json!({ "sessionId": sid, "p": p })`），目的是多会话并行 run 时前端能分清
   * 这条进度属于谁。而本文件原先直接发 `{kind: ...}` —— 前端里 `p` 就是
   * `undefined`，于是**三个用例长期红着**（报 `Cannot read properties of
   * undefined (reading 'kind')`）。
   *
   * 红测试比没有测试更糟：它掩盖了真实回归，也让人习惯性忽略 `vitest run` 的失败。
   * 修法不是放宽断言，而是让测试按**真实线上形态**发事件。
   */
  function emitProgress(p: Record<string, unknown>): void {
    emitEvent("agent-progress", { sessionId: state.currentSessionId, p });
  }

  it("思考与工具按发生顺序交错，不再堆成一块", async () => {
    // chat 不立刻返回，让「进行中气泡」留有机会接收进度事件
    state.chatDelayMs = 400;
    await bootApp();
    await openPanel();

    await sendText("把 sessions 的摘要落盘改一下");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    // 第 1 轮：思考 → 调工具
    emitProgress({ kind: "thinking", iteration: 1 });
    emitProgress({ kind: "delta", reasoning: "先看 sessions 结构" });
    emitProgress({
      kind: "toolCall",
      name: "read_file",
      args: '{"path":"sessions.rs"}',
    });
    emitProgress({
      kind: "toolResult",
      name: "read_file",
      preview: "pub struct Session {",
    });

    // 第 2 轮：再思考 → 再调工具
    emitProgress({ kind: "thinking", iteration: 2 });
    emitProgress({ kind: "delta", reasoning: "摘要必须持久化" });
    emitProgress({
      kind: "toolCall",
      name: "edit_file",
      args: '{"path":"sessions.rs"}',
    });

    await waitFor(
      () => timelineKinds().length === 4,
      2000,
      `timeline items: ${timelineKinds().join(",")}`,
    );

    // 2026-10-02：`read_file` 的调用与返回并成**一行**（原来是 "tool","tool" 两行）
    expect(timelineKinds()).toEqual(["reasoning", "tool", "reasoning", "tool"]);
    // 两段思考是**两个**折叠块（这就是"不堆在一块"）
    expect(document.querySelectorAll(".run-timeline > .tl-reason").length).toBe(2);
    const text = timelineText();
    expect(text).toContain("先看 sessions 结构");
    // 折叠行上给的是中文标签，不是原始工具名（2026-10-02）
    expect(text).toContain("读取文件");
    expect(text).toContain("摘要必须持久化");
    expect(text.indexOf("先看 sessions 结构")).toBeLessThan(text.indexOf("读取文件"));

    // 折叠策略：思考 / 工具调用 / 工具返回**全部默认折叠**（用户点开才看），
    // 中途正文（.tl-text）才是常显的
    const folded = Array.from(document.querySelectorAll<HTMLDetailsElement>(".run-timeline > details"));
    expect(folded.length).toBeGreaterThan(0);
    folded.forEach((d) => {
      expect(d.open, `${d.className} 应默认折叠`).toBe(false);
    });

    // 等这一轮收尾：别把挂起的 chat 请求带进下一个用例
    await waitFor(() => !document.querySelector(".msg.running"), 3000, "run finished");
  });

  it("限流退避状态进时间线，过场状态只刷标题", async () => {
    state.chatDelayMs = 400;
    await bootApp();
    await openPanel();
    await sendText("会限流的请求");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    emitProgress({ kind: "thinking", iteration: 1 });

    // 过场（retry 未置位）→ 只刷标题，不入时间线
    emitProgress({ kind: "status", text: "正在请求 mock 模型…" });
    await waitFor(
      () => document.querySelector(".run-head-text")?.textContent?.includes("正在请求") ?? false,
      2000,
      "head shows transient status",
    );
    expect(timelineKinds()).toEqual([]);

    // 限流退避 → 留在时间线里；标题跟着换成最新状态
    emitProgress({
      kind: "status",
      text: "接口限流/临时故障（HTTP 429），4s 后自动重试（第 1/4 次）…",
      retry: true,
    });

    await waitFor(() => timelineKinds().length === 1, 2000, "one timeline item");
    expect(timelineKinds()).toEqual(["status"]);
    expect(timelineText()).toContain("429");
    // 时间线里只有退避重试这条，过场状态没被记进来
    expect(timelineText()).not.toContain("正在请求 mock");
    expect(document.querySelector(".run-head-text")?.textContent).toContain("429");

    // ⚠️ 必须等这一轮收尾。以前这个用例不等 chat 就结束，那个挂起的 promise
    //    会在**下一个用例**里 resolve：往 mock 的 messages 里塞记录、并让**旧模块**
    //    的 then 回调去动**当前** DOM，把正在测的 running 气泡冲掉。
    //    症状是「增量 patch」那条随机红（running=no blocks=0 kinds=[tool,tool]），
    //    加一条用例就可能复现 —— 见同文件第一条用例的同一句注释。
    await waitFor(() => !document.querySelector(".msg.running"), 3000, "run finished");
  });

  it("chat 报错后保住本轮流：重载磁盘内容并显示时间线里的错误条目", async () => {
    await bootApp();
    await openPanel();

    // 模拟后端在错误路径已落盘的那份：提问 + 半截产出（含尾部 error 条目）
    const errText = "模型接口持续限流或无响应，已退避重试 4 次仍未成功。";
    state.messages = [
      { role: "user", text: "把 sessions 的摘要落盘改一下", at: Date.now() },
      {
        role: "assistant",
        text: "",
        at: Date.now() + 1,
        steps: [
          { kind: "status", name: null, detail: "HTTP 429，4s 后自动重试（第 1/4 次）…" },
          { kind: "reasoning", name: null, detail: "先看 sessions 结构" },
          { kind: "tool_call", name: "read_file", detail: '{"path":"sessions.rs"}' },
          { kind: "error", name: null, detail: errText },
        ],
      },
    ];
    state.chatError = errText;

    await sendText("把 sessions 的摘要落盘改一下");

    // ⚠️ 关键：不能只剩一条红字 —— 提问、思考、工具、错误都要在
    await waitFor(() => timelineKinds().length === 4, 3000, "reloaded timeline");
    expect(timelineKinds()).toEqual(["status", "reasoning", "tool", "error"]);
    expect(document.querySelectorAll(".msg.user").length).toBeGreaterThan(0);
    expect(timelineText()).toContain("先看 sessions 结构");

    // 错误文案只出现一次（磁盘那条就是它，不该再叠一条内存错误气泡）
    const occurrences = (document.body.textContent ?? "").split(errText).length - 1;
    expect(occurrences).toBe(1);

    // 磁盘那份带 storedIdx → 「重新生成」按钮可用（修好之前提问没落盘，点了会报找不到）
    expect(document.querySelector(".msg.assistant .msg-regen")).not.toBeNull();
  });

  it("中途正文：运行中摊开（不折叠成块）", async () => {
    state.chatDelayMs = 400;
    await bootApp();
    await openPanel();
    await sendText("边走边说");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    emitProgress({ kind: "thinking", iteration: 1 });
    emitProgress({ kind: "delta", text: "先看一眼结构" });
    emitProgress({ kind: "toolCall", name: "read_file", args: '{"path":"a.md"}' });
    // 下一轮 thinking 会把上一轮的流式正文收进时间线（kind: text）
    emitProgress({ kind: "thinking", iteration: 2 });

    await waitFor(
      () => !!document.querySelector(".run-timeline > .tl-text"),
      2000,
      `live mid text: ${timelineState()}`,
    );
    // 运行中：不套「过程」外壳、正文摊开 —— 用户要盯着进度
    expect(document.querySelector("details.run-process"), "运行中不该有「过程」外壳").toBeNull();
    expect(
      document.querySelector(".run-timeline > .tl-text"),
      "运行中中途正文要摊开",
    ).toBeTruthy();

    // 收尾：等 chat 返回，避免用例结束时还挂着未完成请求
    await waitFor(() => !document.querySelector(".msg.running"), 3000, "run finished");
  });

  it("中途正文按 markdown 渲染：标题/表格出标签，XSS 仍被挡", async () => {
    state.chatDelayMs = 600;
    await bootApp();
    await openPanel();
    await sendText("给我一份分流方案");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    // 表格要**连分隔行一起**到（`splitStable` 靠空行判块收尾）
    emitProgress({ kind: "thinking", iteration: 1 });
    emitProgress({ kind: "delta", text: "## 分流总览\n\n先看结论。\n\n" });
    emitProgress({
      kind: "delta",
      text: "| 建议 | 条数 |\n|---|---|\n| 保留 | 19 |\n\n收尾一段。",
    });
    // 下一轮 thinking 把本轮流式正文收进时间线（kind: text）
    emitProgress({ kind: "thinking", iteration: 2 });

    await waitFor(
      () => !!document.querySelector(".run-timeline > .tl-text table.md-table"),
      2000,
      `中途正文表格未渲染: ${timelineState()}`,
    );
    const box = document.querySelector(".run-timeline > .tl-text") as HTMLElement;
    // 标题成标签、表格成 table —— 这是"渲染了"的直接证据
    expect(box.querySelector("h2"), "标题应渲染成 h2").toBeTruthy();
    expect(box.querySelector("h2")?.textContent).toContain("分流总览");
    expect(box.querySelectorAll("table.md-table th").length, "表头 2 列").toBe(2);
    // 收尾的段落也在
    expect(box.textContent ?? "").toContain("收尾一段");

    // ⚠️ XSS：模型正文里的标签必须仍被转义（渲染不等于放行原始 HTML）
    emitProgress({ kind: "delta", text: "<img src=x onerror=alert(1)>\n\n" });
    await waitFor(
      () => (document.querySelector(".run-stream")?.textContent ?? "").includes("onerror"),
      2000,
      "XSS 样例未到达流式区",
    );
    expect(
      document.querySelector(".run-stream img"),
      "模型给的裸 <img> 不能变成真元素",
    ).toBeNull();

    await waitFor(() => !document.querySelector(".msg.running"), 3000, "run finished");
  });

  it("思考块 markdown 懒渲染：折叠时纯文本，点开才转 HTML", async () => {
    state.chatDelayMs = 800;
    await bootApp();
    await openPanel();
    await sendText("想一下");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    emitProgress({ kind: "thinking", iteration: 1 });
    emitProgress({ kind: "delta", reasoning: "## 拆解\n\n先看结构。\n\n" });
    emitProgress({ kind: "delta", reasoning: "- 第一步\n- 第二步\n\n" });

    await waitFor(
      () => !!document.querySelector(".run-timeline > .tl-reason"),
      2000,
      `思考块未出现: ${timelineState()}`,
    );
    const d = document.querySelector(".run-timeline > .tl-reason") as HTMLDetailsElement;
    const pre = d.querySelector(".run-reason-pre") as HTMLElement;

    // 折叠态：不该已经渲染出标签（省流式开销）
    expect(d.open, "思考块默认折叠").toBe(false);
    expect(pre.querySelector("h2"), "折叠时不该已渲染 h2").toBeNull();
    expect(pre.textContent ?? "").toContain("## 拆解");

    // 点开 → 懒渲染
    d.open = true;
    d.dispatchEvent(new Event("toggle"));
    await waitFor(() => !!pre.querySelector("h2"), 1000, "展开后未转 markdown");
    expect(pre.querySelector("h2")?.textContent).toContain("拆解");
    expect(pre.querySelector("ul li")?.textContent).toContain("第一步");
    expect(pre.dataset.md, "转换后应打标记，幂等").toBe("1");

    // 再 toggle 一次不应重复转换 / 报错
    d.open = false;
    d.dispatchEvent(new Event("toggle"));
    expect(pre.querySelector("h2"), "收起后内容仍在（不丢）").toBeTruthy();

    await waitFor(() => !document.querySelector(".msg.running"), 3000, "run finished");
  });

  it("跑完：整轮压成一行「过程 · N 步」，点开才看得到中途正文与工具行", async () => {
    // 直接喂磁盘历史（跑完的形态），比走一遍流式再重载更稳
    state.messages = [
      { role: "user", text: "看看结构", at: Date.now() },
      {
        role: "assistant",
        text: "结构没问题。",
        at: Date.now() + 1,
        steps: [
          { kind: "reasoning", name: null, detail: "先列目录" },
          { kind: "text", name: "第 1 轮", detail: "先看一眼目录再说，别急着改代码。" },
          { kind: "tool_call", name: "list_dir", detail: '{"path":"."}' },
          { kind: "tool_result", name: "list_dir", detail: "src\nREADME.md" },
        ],
      },
    ];
    await bootApp();
    await openPanel();

    // 用户 2026-10-02："这个应该全部压缩掉啊" —— 回看时整轮只占一行
    const proc = document.querySelector<HTMLDetailsElement>("details.run-process");
    expect(proc, "跑完应出现「过程」外壳").toBeTruthy();
    expect(proc!.open, "「过程」默认闭合").toBe(false);
    expect(proc!.querySelector("summary")?.textContent).toContain("3 步");

    // 中途正文在外壳里（不额外折叠 —— 展开外壳就该读到它）
    const mid = proc!.querySelector<HTMLElement>(".tl-text");
    expect(mid?.textContent).toContain("别急着改代码");
    // 工具行各自折叠，且调用+返回已并成**一行**
    const tools = Array.from(proc!.querySelectorAll<HTMLDetailsElement>("details.tl-tool"));
    expect(tools.length, "list_dir 的调用与返回应合成一行").toBe(1);
    tools.forEach((d) => expect(d.open).toBe(false));
    // 那一行里参数与返回都能看到
    expect(tools[0]!.querySelector("summary")?.textContent).toContain("列目录");
    expect(tools[0]!.textContent).toContain("README.md");
    // 最终答案在壳外、始终摊开
    expect(document.querySelector(".msg.assistant .msg-body")?.textContent).toContain("结构没问题");
  });

  it("进时间线增量 patch：流式增长的思考块只有两个节点，不重建整块", async () => {
    state.chatDelayMs = 2000;
    await bootApp();
    await openPanel();
    await sendText("慢慢想");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");

    emitProgress({ kind: "thinking", iteration: 1 });
    for (const chunk of ["第一段", "，继续说", "，还在说"]) {
      emitProgress({ kind: "delta", reasoning: chunk });
      await sleep(30);
    }

    await waitFor(() => reasonBlocks() === 1, 3000, `one reasoning block: ${timelineState()}`);
    const pre = document.querySelector(".run-timeline .run-reason-pre");
    expect(pre?.textContent).toBe("第一段，继续说，还在说");

    // 第 2 轮思考必须**另起一块**，而不是并进上一块
    emitProgress({ kind: "thinking", iteration: 2 });
    emitProgress({ kind: "delta", reasoning: "换个思路" });
    await waitFor(
      () => reasonBlocks() === 2,
      3000,
      `second reasoning block: ${timelineState()}`,
    );
    const pres = Array.from(document.querySelectorAll(".run-timeline .run-reason-pre")).map(
      (n) => (n.textContent ?? "").trim(),
    );
    expect(pres).toEqual(["第一段，继续说，还在说", "换个思路"]);

    // 收尾：等 chat 返回，避免用例结束时还挂着未完成请求
    await waitFor(() => !document.querySelector(".msg.running"), 4000, "run finished");
    expect(callsOf("chat").length).toBeGreaterThan(0);
  });
});
