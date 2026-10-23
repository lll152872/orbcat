/**
 * 数据源开关 —— **一行切**。
 *
 * ## 怎么切
 *
 * | 方式 | 用法 | 场景 |
 * |---|---|---|
 * | URL 参数 | `?mock=1` | 真应用里临时看假数据（调试用） |
 * | 构建期 env | `VITE_MOCK=1 npm run build` | 出一版纯 mock 的包（出视频用） |
 * | 代码一行 | `setDataSource(new MockSource("perm"))` | 预览页 / 测试里精确指定 |
 *
 * 优先级：代码里显式设置 > URL 参数 > 构建期 env > 默认（真后端）。
 *
 * ## 为什么门面叫 `invoke`
 *
 * 业务代码里写 `invoke("session_state")` 读着最顺，但**调用点**换成
 * `data.sessions.state()` 才能真正做到「不摸 Tauri 细节」。
 * 所以 `invoke` 这个门面**保留但标记为迁移中间态**：
 * 它现在指向数据源（mock 模式下真的走 mock），等所有调用点迁完就删掉。
 */
import type { DataSource } from "./source";
import { TauriSource } from "./tauri";
import { MockSource } from "./mock";

let current: DataSource = pickInitial();

/** 显式设置过的数据源（优先于 URL / env） */
let pinned: DataSource | null = null;

/**
 * 选初始数据源。
 *
 * - `?mock=1` → mock
 * - `import.meta.env.VITE_MOCK === "1"` → mock
 * - 否则 → 真后端
 */
function pickInitial(): DataSource {
  const urlMock = safeQuery("mock");
  const envMock = safeEnvMock();
  if (urlMock === "1" || urlMock === "true" || envMock) {
    const scenario = safeQuery("scenario") ?? "default";
    return new MockSource(scenario);
  }
  return new TauriSource();
}

/** 读 URL 参数（没有 location 也不炸 —— 单元测试里跑） */
function safeQuery(key: string): string | null {
  try {
    if (typeof location === "undefined") return null;
    return new URLSearchParams(location.search).get(key);
  } catch {
    return null;
  }
}

/** 读构建期 env（Vite 的 import.meta.env 在测试里也可能没有） */
function safeEnvMock(): boolean {
  try {
    const env = (import.meta as unknown as { env?: Record<string, string | undefined> }).env;
    return env?.VITE_MOCK === "1";
  } catch {
    return false;
  }
}

/** 当前数据源 */
export function dataSource(): DataSource {
  return current;
}

/** 换数据源（一行切） */
export function setDataSource(next: DataSource): void {
  current = next;
  pinned = next;
}

/** 解除显式设置，回到 URL/env 决定的默认 */
export function resetDataSource(): void {
  pinned = null;
  current = pickInitial();
}

/** 是否处于 mock 模式（UI 上可以标一个角标） */
export function isMock(): boolean {
  return current.kind === "mock";
}

/**
 * 迁移中间态的门面。
 *
 * ⚠️ 业务代码里的 `invoke(...)` 应该逐步换成 `data.<域>.<方法>(...)`；
 * 这个门面只保证「现在就能 mock」，不保证「以后不用改」。
 */
export async function invoke<T = unknown>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return current.call<T>(cmd, args);
}

/** 数据源单例（业务代码用这个） */
export const data: DataSource = {
  get kind() {
    return current.kind;
  },
  get sessions() {
    return current.sessions;
  },
  get chat() {
    return current.chat;
  },
  get models() {
    return current.models;
  },
  get memory() {
    return current.memory;
  },
  get permissions() {
    return current.permissions;
  },
  get projects() {
    return current.projects;
  },
  get recovery() {
    return current.recovery;
  },
  get settings() {
    return current.settings;
  },
  get life() {
    return current.life;
  },
  get mcp() {
    return current.mcp;
  },
  get search() {
    return current.search;
  },
  get system() {
    return current.system;
  },
  call<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    return current.call<T>(cmd, args);
  },
};

// pinned 变了要同步 current（setDataSource 已经做了，这里只防手滑）
void pinned;

export type { DataSource } from "./source";
export { MockSource } from "./mock";
export { TauriSource } from "./tauri";
export { SCENARIOS, scenarioById } from "./mockdata";
export type { MockScenario } from "./mockdata";
