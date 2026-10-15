/**
 * 胶囊态球的帧态优先级 —— 2026-10-14 用户实测带出来的回归
 *
 * ## 现象
 *
 * 用户问：「思考不是蓝猫摇尾巴吗，为啥变成这样了」—— 球一直显示绿壳 peek
 * （`orb-peek.png`）+ 紫脑标，而 agent 明明在干活。
 *
 * ## 根因
 *
 * `currentOrbState()` 里 `pendingMem > 0` 排在 `thinking` **前面**。
 * 只要待审记忆 ≥ 1 条（用户那条是「用户有 Typora/Obsidian 初始化习惯…」），
 * 球就永远被钉在 `mem` 帧，蓝猫摇尾巴一次都轮不上。
 *
 * ## 这次锁住两件事
 *
 * 1. **优先级**：运行时 > 记忆 > 常态（用户拍板）。
 * 2. **角标与帧态解耦**：紫脑标改绑 `.orb--has-mem`，不再绑 `.orb--mem`。
 *    否则 thinking 一盖上来，「有记忆等你审」的提醒正好在最该出现的时刻消失。
 *
 * 第 2 条 happy-dom 算不出 opacity，所以最后一条用例直接读 `styles.css`
 * 断言选择器还在 —— 改回去的话，只有这里会红。
 */
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { bootApp, openPanel, sendText, waitFor, orbExists, clickOrb, sleep } from "./helpers";
import { state } from "./mocks/tauri";

function orb(): HTMLElement {
  const el = document.getElementById("orb");
  if (!el) throw new Error("#orb 不存在");
  return el;
}

function has(cls: string): boolean {
  return orb().classList.contains(cls);
}

/** 待审记忆 ≥ 1 条 —— 用户的真实状态 */
function onePendingMemory(): void {
  state.memPending = [
    {
      id: "cand_1",
      content: "用户有 Typora/Obsidian 初始化习惯",
      source: "model",
      created_at: "ts0000000001T00:00:00Z",
    },
  ];
}

/** 收起面板回到胶囊态 */
async function collapseToOrb(): Promise<void> {
  (document.getElementById("btn-close") as HTMLButtonElement).click();
  await waitFor(() => orbExists(), 3000, "collapsed to orb");
}

describe("胶囊态：帧态优先级 error > pending > thinking > mem > idle", () => {
  it("没有待审记忆时：空转是 idle，干活是 thinking（蓝猫摇尾巴）", async () => {
    await bootApp();
    await waitFor(() => has("orb--idle"), 3000, "idle");

    state.chatDelayMs = 600; // 让这一轮还挂在"进行中"
    await openPanel();
    await sendText("跑一会儿");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running");

    await collapseToOrb();
    expect(has("orb--thinking"), "干活中必须是 thinking 帧（蓝猫）").toBe(true);
    expect(has("orb--idle")).toBe(false);
  });

  // ⚠️ 这条就是用户报的 bug：待审记忆把蓝猫盖住了
  it("有待审记忆 + 正在干活 → 仍然是 thinking 帧（记忆让位给运行时）", async () => {
    onePendingMemory();
    await bootApp();
    await waitFor(() => has("orb--mem"), 3000, "mem frame");

    state.chatDelayMs = 600;
    await openPanel();
    await sendText("一边有记忆待审，一边让它干活");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running");

    await collapseToOrb();
    expect(
      has("orb--thinking"),
      "有待审记忆时干活中仍要露蓝猫摇尾巴 —— 这正是 2026-10-14 用户报的回归",
    ).toBe(true);
    expect(has("orb--mem"), "记忆帧必须让位，否则用户以为卡住了").toBe(false);
  });

  it("有待审记忆但没在干活 → mem 帧（常态优先级不变）", async () => {
    onePendingMemory();
    await bootApp();
    await waitFor(() => has("orb--mem"), 3000, "mem frame");
    expect(has("orb--thinking")).toBe(false);
    expect(has("orb--idle")).toBe(false);
  });
});

describe("紫脑标与帧态解耦：提醒不能随帧消失", () => {
  it("有待审记忆 → 球上带 orb--has-mem 标记", async () => {
    onePendingMemory();
    await bootApp();
    await waitFor(() => has("orb--has-mem"), 3000, "has-mem marker");
  });

  it("干活中（有 orb--thinking）紫脑标标记仍在 —— 别被盖掉", async () => {
    onePendingMemory();
    await bootApp();
    await waitFor(() => has("orb--mem"), 3000, "mem frame");

    state.chatDelayMs = 600;
    await openPanel();
    await sendText("忙起来也别忘了提醒我审记忆");
    await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running");

    await collapseToOrb();
    await sleep(30); // 让 renderOrb / applyOrbState 都落定
    expect(has("orb--thinking")).toBe(true);
    expect(
      has("orb--has-mem"),
      "「有记忆等你审」必须在 agent 忙着的时候仍然看得见",
    ).toBe(true);
  });

  it("没有待审记忆 → 不带 orb--has-mem", async () => {
    await bootApp();
    await waitFor(() => has("orb--idle"), 3000, "idle");
    expect(has("orb--has-mem")).toBe(false);
  });

  // happy-dom 不做 CSS 级联，算不出 opacity —— 直接锁选择器
  it("styles.css 里紫脑标绑的是 .orb--has-mem，不是 .orb--mem", () => {
    // ⚠️ 别用 `new URL(..., import.meta.url)` —— happy-dom 下 `import.meta.url`
    //    不是 file: 协议，`fileURLToPath` 会直接抛「The URL must be of scheme file」。
    //    vitest 的 cwd 就是仓库根，相对路径最省事也最稳。
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    expect(css, "角标必须由独立标记驱动，否则改回旧绑法只有这里会发现").toContain(
      ".orb--has-mem .orb-badge-mem",
    );
    expect(css, "不该再绑在帧态 .orb--mem 上").not.toContain(".orb--mem .orb-badge-mem");
  });
});

describe("收起面板后仍能看到球的状态（顺带防 collapse 丢状态）", () => {
  it("展开再收起，帧态与角标标记都还在", async () => {
    onePendingMemory();
    await bootApp();
    await waitFor(() => has("orb--mem"), 3000, "mem frame");
    clickOrb();
    await waitFor(() => !!document.getElementById("input"), 3000, "panel");
    await collapseToOrb();
    expect(has("orb--mem")).toBe(true);
    expect(has("orb--has-mem")).toBe(true);
  });
});