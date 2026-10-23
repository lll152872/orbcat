/**
 * 预览用的 **Tauri 桩** —— 让真界面在纯浏览器里跑起来。
 *
 * ## 为什么需要
 *
 * 界面里有三处直接依赖 Tauri：
 * - `invoke`（后端命令）
 * - `listen`（后端事件）
 * - `getCurrentWindow`（窗口拖动等）
 *
 * 在真 Tauri 环境里它们来自 `@tauri-apps/api`；**预览构建**用 vite 的
 * `resolve.alias` 把这三个模块指向本文件，于是：
 * - `invoke` → mock 数据源（真实现走 `MockSource`）
 * - `listen` → 空订阅（返回取消函数）
 * - `getCurrentWindow` → 什么都不做的窗口对象
 *
 * 结果：**不需要 Tauri runtime、不连后端、不读用户数据**，
 * 双击一个 HTML 就能看到每一屏。
 *
 * ⚠️ 这个桩**只在预览构建里生效**（`vite.preview.config.ts` 的 alias），
 * 正式构建的 alias 表里没有它 —— 应用照常走真 Tauri。
 */

/** 真正的实现（由预览入口注入） */
type Handler = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

let handler: Handler = async () => undefined;

/** 注入真正的实现 */
export function setInvokeHandler(fn: Handler): void {
  handler = fn;
}

/** 命令调用 → 交给注入的实现（mock 数据源） */
export async function invoke<T = unknown>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return (await handler(cmd, args)) as T;
}

/**
 * 事件订阅 —— 预览里**不发事件**。
 *
 * 返回一个取消函数，签名与 Tauri 一致（`listen` 返 `Promise<UnlistenFn>`）。
 * 真机上由后端 emit 驱动（`orb-open-panel` / `models-changed`）；
 * 预览里改用「点球 / 命令面板」驱动。
 */
export async function listen(
  _event: string,
  _handler: (payload: unknown) => void,
): Promise<() => void> {
  return () => {};
}

/**
 * 窗口对象 —— 什么方法都是**空操作**，但**窗口拖放事件是真能触发的**。
 *
 * 用 Proxy 而不是逐个写方法：以后界面里新增 `getCurrentWindow().xxx()`
 * 也不会在预览里炸（顶多什么都不发生）。
 *
 * 唯一做成「真的」的是 `onDragDropEvent`：预览页与 UI 测试都要
 * **模拟一次拖放**（把文件拖到球上）—— 没有它，这条路径在预览里根本走不到。
 */
const dragDropHandlers: ((e: { payload: unknown }) => void)[] = [];

/** 模拟一次窗口拖放事件（预览页 / 测试用） */
export function emitDragDrop(payload: {
  type: "enter" | "over" | "drop" | "leave";
  paths?: string[];
  position?: { x: number; y: number };
}): void {
  for (const h of [...dragDropHandlers]) {
    h({ payload: { position: { x: 0, y: 0 }, ...payload } });
  }
}

export function getCurrentWindow(): unknown {
  const win = {
    label: "preview",
    async startDragging(): Promise<void> {},
    async setFocus(): Promise<void> {},
    async show(): Promise<void> {},
    async hide(): Promise<void> {},
    async center(): Promise<void> {},
    async setAlwaysOnTop(): Promise<void> {},
    /** 原生拖放开关（Tauri v2 里只有创建期选项，没有运行时方法，这里是空操作） */
    async dragDropEnabled(_enabled: boolean): Promise<void> {},
    /** 注册拖放监听 —— **真的**注册，预览页可以模拟拖放 */
    async onDragDropEvent(handler: (e: { payload: unknown }) => void): Promise<() => void> {
      dragDropHandlers.push(handler);
      return () => {
        const i = dragDropHandlers.indexOf(handler);
        if (i >= 0) dragDropHandlers.splice(i, 1);
      };
    },
  };
  return new Proxy(win, {
    get(target, prop) {
      if (prop in target) return (target as Record<string | symbol, unknown>)[prop as string];
      if (prop === "then") return undefined;
      return async () => {};
    },
  });
}

/** 打开外部链接（预览里只记录，不真的跳） */
export async function openUrl(url: string | URL): Promise<void> {
  console.info("[preview] openUrl:", String(url));
}

/** 打开外部文件（同上） */
export async function openPath(path: string): Promise<void> {
  console.info("[preview] openPath:", path);
}
