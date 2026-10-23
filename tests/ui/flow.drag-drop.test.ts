/**
 * 拖放：把文件/文件夹拖到球上 = **在输入区填路径**（不自动发送）。
 *
 * ## 用户原话
 *
 * 「拖拽，能直接把文件拖拽到球上面，相当于输入里面输入了一个路径」
 *
 * ## 十条验收（逐条对应）
 *
 * ① 拖到球上 → 面板自动展开
 * ② 路径进输入区（路径条），条数与拖入的一致
 * ③ **不自动发送**（`chat` 没被调用）
 * ④ 输入框获得焦点（拖完就能接着打字）
 * ⑤ 路径单行不换行 + **中间**省略
 * ⑥ 提交用**完整值**（显示省略、提交不省）
 * ⑦ 多选 → 逐个引号包裹
 * ⑧ 旧的 HTML5 贴图拖放**已删**（原生通道接管后它收不到事件）
 * ⑨ 手动贴图仍可用（Ctrl+V 粘贴）
 * ⑩ 悬停有反馈（放大/光晕）、离开与放下都收回
 */
import { describe, it, expect } from "vitest";
import { bootApp, waitFor, callsOf, sleep } from "./helpers";
import { emitDragDrop } from "./mocks/tauri";

const P1 = "D:\\workplace\\悬浮小agent\\orbcat\\src\\views\\recovery.ts";
const P2 = "D:\\workplace\\悬浮小agent\\orbcat\\docs\\09-FRONTEND-HANDOVER.md";
/** 一条超长路径（用来验「中间省略」） */
const LONG =
  "D:\\workplace\\悬浮小agent\\orbcat\\src\\views\\recovery\\components\\backup-card\\index.tsx";

/** 往输入框打字（走真实事件，前端才认） */
function type(text: string): void {
  const input = document.getElementById("input") as HTMLTextAreaElement;
  input.value = text;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

/** 按回车发送 */
function pressEnter(): void {
  const input = document.getElementById("input") as HTMLTextAreaElement;
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
}

describe("拖放 · 文件拖到球上 = 填路径", () => {
  it("① 拖到球上 → 面板自动展开（球态下直接变面板）", async () => {
    await bootApp();
    expect(document.getElementById("input"), "一开始是球态，没有面板").toBeNull();

    emitDragDrop({ type: "drop", paths: [P1] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");
  });

  it("② 路径进输入区（路径条），条数与拖入的一致", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [P1, P2] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");

    const chips = document.querySelectorAll("#paths .path-chip");
    expect(chips.length, "两条路径 → 两个 chip").toBe(2);
  });

  it("③ 不自动发送（拖进来只是填，不是跑）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [P1] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");
    await sleep(120);
    expect(callsOf("chat").length, "拖放不该触发发送").toBe(0);
  });

  it("④ 输入框获得焦点（拖完就能接着打字）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [P1] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");
    await waitFor(
      () => document.activeElement?.id === "input",
      2000,
      "输入框已聚焦",
    );
  });

  it("⑤ 路径单行不换行 + 中间省略（尾部是文件名，不能省）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [LONG] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");
    await waitFor(() => !!document.querySelector("#paths .path-chip code"), 2000, "路径条");

    const code = document.querySelector("#paths .path-chip code")!;
    const shown = code.textContent ?? "";
    expect(shown.length, "显示的是省略版").toBeLessThan(LONG.length);
    expect(shown.includes("…"), "省略号在中间").toBe(true);
    expect(shown.includes("\n"), "单行不换行").toBe(false);
    // 尾部（文件名）要保住 —— 认得出是哪个文件
    expect(shown.endsWith("index.tsx"), "尾部文件名保留").toBe(true);
    // 完整值在 title 里
    expect(code.getAttribute("title"), "完整路径在 title").toBe(LONG);
  });

  it("⑥ 提交用完整值（显示省略、提交不省）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [LONG] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");
    await waitFor(() => !!document.querySelector("#paths .path-chip"), 2000, "路径条");

    type("看下这个文件");
    pressEnter();
    await waitFor(() => callsOf("chat").length >= 1, 3000, "已发送");

    const sent = String(callsOf("chat")[0]!.args?.input ?? "");
    expect(sent, "提交的是完整路径").toContain(LONG);
    expect(sent, "不是省略版").not.toContain("…");
    expect(sent.includes("看下这个文件"), "用户的话也在").toBe(true);
  });

  it("⑦ 多选 → 逐个引号包裹（路径里有空格时模型才不会拆词）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [P1, P2] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");
    await waitFor(() => document.querySelectorAll("#paths .path-chip").length === 2, 2000, "两条路径");

    pressEnter();
    await waitFor(() => callsOf("chat").length >= 1, 3000, "已发送");

    const sent = String(callsOf("chat")[0]!.args?.input ?? "");
    expect(sent, "两个路径都被引号包住").toContain(`"${P1}"`);
    expect(sent, "两个路径都被引号包住").toContain(`"${P2}"`);
  });

  it("⑧ 旧的 HTML5 贴图拖放已删（原生通道接管后它收不到事件）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [P1] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");

    // 模拟旧通道：往面板里 drop 一个图片文件
    const file = new File([new Uint8Array([137, 80, 78, 71])], "a.png", { type: "image/png" });
    const ev = new Event("drop", { bubbles: true, cancelable: true }) as Event & {
      dataTransfer?: unknown;
    };
    ev.dataTransfer = { files: [file] };
    document.dispatchEvent(ev);
    await sleep(200);

    expect(document.querySelectorAll("#thumbs .thumb").length, "HTML5 拖放不再贴图").toBe(0);
  });

  it("⑨ 手动贴图仍可用（Ctrl+V 粘贴这条通道没动）", async () => {
    await bootApp();
    emitDragDrop({ type: "drop", paths: [P1] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板自动展开");

    const file = new File([new Uint8Array([137, 80, 78, 71])], "a.png", { type: "image/png" });
    const ev = new Event("paste", { bubbles: true, cancelable: true }) as Event & {
      clipboardData?: unknown;
    };
    ev.clipboardData = { items: [{ type: "image/png", getAsFile: () => file }] };
    document.dispatchEvent(ev);

    await waitFor(() => document.querySelectorAll("#thumbs .thumb").length === 1, 3000, "粘贴贴上了图");
  });

  it("⑩ 悬停有反馈（放大 + 光晕），离开与放下都收回", async () => {
    await bootApp();
    const orb = document.getElementById("orb")!;

    emitDragDrop({ type: "enter", paths: [P1] });
    await waitFor(() => orb.classList.contains("orb--dragover"), 2000, "悬停状态已加");

    emitDragDrop({ type: "leave" });
    await waitFor(() => !orb.classList.contains("orb--dragover"), 2000, "离开后收回");

    // 放下也要收回（drop 之后球不再高亮）
    emitDragDrop({ type: "enter", paths: [P1] });
    await waitFor(() => orb.classList.contains("orb--dragover"), 2000, "再次悬停");
    emitDragDrop({ type: "drop", paths: [P1] });
    await waitFor(() => !orb.classList.contains("orb--dragover"), 2000, "放下后收回");
  });
});
