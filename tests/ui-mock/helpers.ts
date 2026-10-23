/**
 * mock 数据层测试的公共helper。
 *
 * 关键点：`vi.resetModules()` 之后 `src/data/index.ts` 是**新实例**，
 * 所以数据源必须在**重置之后**再设 —— 否则设的那个源被丢掉了
 * （这正是 `tests/ui/mocks/tauri.ts` 注释里说的「模块双实例坑」）。
 */
import { vi } from "vitest";

/** 等条件成立；超时抛错（带最后一次观测描述） */
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
 * 装载界面 —— **走 mock 数据层**。
 *
 * 顺序很重要：先 resetModules（拿到新的 data 模块实例），
 * 再 setDataSource（切到 mock），最后 import main（触发 boot）。
 */
export async function bootMockApp(scenario = "default"): Promise<void> {
  vi.resetModules();
  if (!document.getElementById("app")) {
    document.body.innerHTML = `<div id="app"></div>`;
  }
  const { setDataSource } = await import("../../src/data");
  const { MockSource } = await import("../../src/data/mock");
  setDataSource(new MockSource(scenario));
  await import("../../src/main");
  await waitFor(() => !!document.getElementById("orb"), 3000, "orb rendered");
}

/** 当前生效的数据源（断言用） */
export async function currentSource(): Promise<{ kind: string; data: unknown }> {
  const { dataSource } = await import("../../src/data");
  const src = dataSource() as unknown as { kind: string; data: unknown };
  return src;
}

/** 切屏（用 main.ts 导出的预览钩子） */
export async function switchView(v: string): Promise<void> {
  const app = (await import("../../src/main")) as unknown as {
    __previewSwitchView?: (v: string) => Promise<void>;
  };
  await app.__previewSwitchView?.(v);
}

/** 点球：mousedown + mouseup（attachDrag 靠位移判定点击） */
export function clickOrb(): void {
  const orb = document.getElementById("orb");
  if (!orb) throw new Error("orb 不存在");
  orb.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
  orb.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, button: 0 }));
}

/** 展开面板 */
export async function openPanel(): Promise<void> {
  clickOrb();
  await waitFor(() => !!document.getElementById("input"), 3000, "panel#input");
}

/** 在输入框里打字并回车 */
export async function sendText(text: string): Promise<void> {
  const input = document.getElementById("input") as HTMLTextAreaElement;
  input.value = text;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
}
