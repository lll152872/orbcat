/**
 * 「用 mock 数据层跑真界面」的测试工程 —— **不依赖 Tauri runtime**。
 *
 * ## 与 `tests/ui/` 的区别
 *
 * | | tests/ui | tests/ui-mock（本工程） |
 * |---|---|---|
 * | 后端 | `tests/ui/mocks/tauri.ts`（手搓的 Tauri 桩） | `src/data/mock.ts`（**产品自带的 mock 数据层**） |
 * | 数据 | 写死的 reply / 空表 | **有状态的假世界**（批准记忆会真消失、发消息会真多两条） |
 * | Tauri runtime | 桩掉了 `@tauri-apps/api`（桩是测试专用的） | **整个包 alias 掉**（连 import 都不走 Tauri 那条路） |
 *
 * 也就是说：这组用例证明的是「**产品代码本身**可以在没有 Tauri 的环境里跑起来」——
 * 跟「纯 HTML 渲染件」是同一套机制。
 */
import { beforeEach, afterEach } from "vitest";

beforeEach(() => {
  document.body.innerHTML = `<div id="app"></div>`;
});

afterEach(() => {
  document.body.innerHTML = "";
});
