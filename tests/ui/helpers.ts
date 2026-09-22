import { vi } from "vitest";
// 从同一个 globalThis 单例取，避免 resetModules 双实例
import { callsOf, state } from "./mocks/tauri";

/** 等断言条件成立；超时则抛错（带最后一次观测描述） */
export async function waitFor(
  fn: () => boolean | string,
  timeoutMs = 3000,
  label = "condition",
): Promise<void> {
  const start = Date.now();
  let last = "";
  while (Date.now() - start < timeoutMs) {
    const r = fn();
    if (r === true) return;
    last = typeof r === "string" ? r : "still false";
    await sleep(16);
  }
  throw new Error(`waitFor(${label}) 超时 ${timeoutMs}ms：${last}`);
}

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/**
 * 加载前端入口。main.ts 在模块顶层 boot()，
 * 因此每个测试文件/用例必须 resetModules 后再动态 import。
 */
export async function bootApp(): Promise<void> {
  vi.resetModules();
  // resetMock 已在 setup 的 beforeEach 做过；这里再保证 DOM 容器在
  if (!document.getElementById("app")) {
    document.body.innerHTML = `<div id="app"></div>`;
  }
  await import("../../src/main");
  // boot 会 renderOrb + 一堆异步 invoke
  await waitFor(() => !!document.getElementById("orb"), 3000, "orb rendered");
}

/** 模拟「零位移点击」：mousedown + mouseup（attachDrag 的 click 判定） */
export function clickOrb(): void {
  const orb = document.getElementById("orb");
  if (!orb) throw new Error("orb 不存在");
  orb.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
  orb.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
}

export async function openPanel(): Promise<void> {
  clickOrb();
  await waitFor(() => !!document.getElementById("input"), 3000, "panel#input");
  await waitFor(() => !!document.getElementById("btn-send"), 3000, "panel#btn-send");
}

export function panelExists(): boolean {
  return !!document.querySelector(".panel");
}

export function orbExists(): boolean {
  return !!document.getElementById("orb");
}

export function typeInput(text: string): HTMLTextAreaElement {
  const input = document.getElementById("input") as HTMLTextAreaElement | null;
  if (!input) throw new Error("#input 不存在");
  input.value = text;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  return input;
}

export async function clickSend(): Promise<void> {
  const btn = document.getElementById("btn-send") as HTMLButtonElement | null;
  if (!btn) throw new Error("#btn-send 不存在");
  btn.click();
}

export function userBubbles(): string[] {
  return Array.from(document.querySelectorAll(".msg.user .msg-body")).map((n) =>
    (n.textContent ?? "").trim(),
  );
}

export function assistantBubbles(): string[] {
  return Array.from(document.querySelectorAll(".msg.assistant .msg-body")).map((n) =>
    (n.textContent ?? "").trim(),
  );
}

export function errorBubbles(): string[] {
  return Array.from(document.querySelectorAll(".msg.error .msg-body")).map((n) =>
    (n.textContent ?? "").trim(),
  );
}

export function systemBubbles(): string[] {
  return Array.from(document.querySelectorAll(".msg.system .msg-body, .msg.system")).map((n) =>
    (n.textContent ?? "").trim(),
  );
}

export function runningBubble(): boolean {
  return !!document.querySelector(".msg.running");
}

export function sendButton(): HTMLButtonElement {
  const b = document.getElementById("btn-send") as HTMLButtonElement | null;
  if (!b) throw new Error("#btn-send 不存在");
  return b;
}

/** 便捷：完整「输入 + 发送」 */
export async function sendText(text: string): Promise<void> {
  typeInput(text);
  await clickSend();
}

export { callsOf, state };
