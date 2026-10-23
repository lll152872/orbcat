/**
 * 预览构建配置 —— **只服务于「纯 HTML 渲染件」**。
 *
 * 与正式构建（`vite.config.ts`）的差别只有三处：
 * 1. `resolve.alias` 把 `@tauri-apps/api/*` 指向 `src/preview/tauri-shim.ts`
 *    ⇒ 预览不需要 Tauri runtime；
 * 2. 入口是 `preview.html`（不是 `index.html`）；
 * 3. 产物进 `preview/`，且 `base: "./"`（相对路径）。
 *
 * 正式构建的 alias 表里**没有**这些 ⇒ 应用照常走真 Tauri。
 */
import { defineConfig } from "vite";
import path from "node:path";

export default defineConfig({
  root: ".",
  base: "./",
  resolve: {
    alias: {
      "@tauri-apps/api/core": path.resolve(__dirname, "src/preview/tauri-shim.ts"),
      "@tauri-apps/api/event": path.resolve(__dirname, "src/preview/tauri-shim.ts"),
      "@tauri-apps/api/window": path.resolve(__dirname, "src/preview/tauri-shim.ts"),
      "@tauri-apps/plugin-opener": path.resolve(__dirname, "src/preview/tauri-shim.ts"),
    },
  },
  build: {
    outDir: "preview",
    emptyOutDir: true,
    target: "es2020",
    cssCodeSplit: false,
    // 单文件：没有动态 import 分包
    rollupOptions: {
      input: { preview: path.resolve(__dirname, "preview.html") },
      output: { inlineDynamicImports: true, entryFileNames: "assets/preview.js" },
    },
    assetsInlineLimit: 1024 * 1024 * 8,
    modulePreload: { polyfill: false },
  },
});
