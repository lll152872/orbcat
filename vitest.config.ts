import { defineConfig } from "vitest/config";
import path from "node:path";

/**
 * orbcat 测试配置
 *
 * 两组测试环境要求不同，用 projects 分开，**不要合并**：
 *
 * · ui —— happy-dom 里跑「点击悬浮球 → 面板 → 输入 → 发送」全流程
 *   （invoke/listen 全部 mock，不依赖 Tauri 运行时与真实 API）。
 *   需要 tests/ui/setup.ts：它重置 DOM 并重置 Tauri mock。
 *
 * · tools —— 纯 Node，测发布工具链（产物布局等），不碰 DOM 也不需要
 *   Tauri mock。**不能吃 ui 的 setupFiles**：setup.ts 里的
 *   `document.body.innerHTML` 在 node 环境直接 ReferenceError，
 *   会让 tools 整组收集失败（2026-10-12 加 tests/tools 时踩过）。
 */
export default defineConfig({
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: "ui",
          environment: "happy-dom",
          include: ["tests/ui/**/*.test.ts"],
          setupFiles: ["tests/ui/setup.ts"],
        },
      },
      {
        extends: true,
        test: {
          name: "tools",
          environment: "node",
          include: ["tests/tools/**/*.test.ts"],
        },
      },
    ],
    globals: false,
    testTimeout: 15_000,
    hookTimeout: 15_000,
    // main.ts 在 import 时会 boot()，文件之间必须隔离模块注册表
    isolate: true,
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src"),
    },
  },
});