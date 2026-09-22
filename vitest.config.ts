import { defineConfig } from "vitest/config";
import path from "node:path";

/**
 * float-agent 测试配置
 *
 * ui 层：happy-dom 里跑「点击悬浮球 → 面板 → 输入 → 发送」全流程
 * （invoke/listen 全部 mock，不依赖 Tauri 运行时与真实 API）
 */
export default defineConfig({
  test: {
    environment: "happy-dom",
    include: ["tests/ui/**/*.test.ts"],
    setupFiles: ["tests/ui/setup.ts"],
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
