/**
 * DOM 还活着吗 —— 给**定时器回调**用的守卫。
 *
 * ## 为什么需要
 *
 * 定时器活过它所属的环境是常态：面板关掉了、页面卸载了，
 * 排队的回调照样会在队列里等着触发。产品里无所谓（`window`/`document` 一直在），
 * 但**测试环境会在文件/用例结束时销毁** —— 回调晚于销毁才触发，
 * `window` / `document` 已经不存在 ⇒ `ReferenceError: window is not defined`
 * ⇒ vitest 记成 **unhandled error** ⇒ 用例明明全过，**退出码却是 1**。
 *
 * 2026-10-23 实测：`showToast` 的自删链（淡出 → 移除，4.5s / 7s）拖过了
 * 测试环境销毁，直接把整套 UI 用例的退出码打成 1（CI 必红）。
 * 同一个坑 `models.ts` 的 `fgTimer` 早就踩过并就地加了守卫，
 * 这里把「还活着吗」抽成一个共用判定，5 处定时器统一用它。
 *
 * ## 用法
 *
 * ```ts
 * setTimeout(() => {
 *   if (!domAlive()) return;   // 环境没了就安静退出
 *   el.classList.remove("in");
 * }, 4500);
 * ```
 *
 * ⚠️ 守卫只能挡住「回调里摸全局」这一类；
 * 回调里如果还持有 DOM 元素引用（`el`），那些元素的方法在环境销毁后
 * 同样可能出问题 —— 所以**回调只操作自己创建、且已挂载的元素**。
 */

/**
 * `window` 与 `document` 是否都还在。
 *
 * 用 `typeof` 而不是 `try/catch`：销毁后的全局是 `undefined`（不是抛错），
 * `typeof` 判定不会把真实的运行时异常一起吞掉。
 */
export function domAlive(): boolean {
  return typeof window !== "undefined" && typeof document !== "undefined";
}
