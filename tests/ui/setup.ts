import { vi, beforeEach, afterEach } from "vitest";
import { resetMock } from "./mocks/tauri";

// 在任何测试加载 main.ts 之前，把 Tauri 运行时整包换成 mock
vi.mock("@tauri-apps/api/core", async () => {
  const mod = await import("./mocks/tauri");
  return { invoke: mod.invoke };
});

vi.mock("@tauri-apps/api/event", async () => {
  const mod = await import("./mocks/tauri");
  return { listen: mod.listen };
});

vi.mock("@tauri-apps/api/window", async () => {
  const mod = await import("./mocks/tauri");
  return { getCurrentWindow: mod.getCurrentWindow };
});

// markdown 模块依赖 DOM 即可；若以后引入重依赖再 mock

beforeEach(() => {
  document.body.innerHTML = `<div id="app"></div>`;
  resetMock();
});

afterEach(() => {
  document.body.innerHTML = "";
  vi.clearAllTimers();
});
