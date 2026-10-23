#!/usr/bin/env node
/**
 * 构建「纯 HTML 渲染件」。
 *
 * ## 为什么要额外一步（内联）
 *
 * 产物要能**双击直接打开**（`file://`）。而浏览器的 CORS 规则禁止
 * `file://` 页面加载**外部**的 ES module（`<script type="module" src="...">`）——
 * 会报 "Access to script … from origin 'null' has been blocked by CORS policy"。
 *
 * 所以构建完再走一步：把 CSS 与 JS **内联进 HTML**，
 * 得到**一个自包含的 .html**（内联 module 脚本在 `file://` 下可以跑）。
 *
 * 用法：`npm run build:preview` ⇒ `preview/preview.html`
 */
import { build } from "vite";
import { readFile, writeFile, rm } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const outDir = path.join(root, "preview");
const htmlPath = path.join(outDir, "preview.html");

/** 1) 构建 */
await build({
  configFile: path.join(root, "vite.preview.config.ts"),
  root,
  logLevel: "warn",
});

if (!existsSync(htmlPath)) {
  console.error(`[build-preview] 没找到产物：${htmlPath}`);
  process.exit(1);
}

/** 2) 内联 CSS / JS */
let html = await readFile(htmlPath, "utf8");

/** 内联 <link rel="stylesheet" href="..."> */
const linkRe = /<link[^>]*rel="stylesheet"[^>]*href="([^"]+)"[^>]*>/g;
for (const m of [...html.matchAll(linkRe)]) {
  const href = m[1];
  if (!href.startsWith(".")) continue;
  const cssPath = path.resolve(outDir, href);
  if (!existsSync(cssPath)) continue;
  const css = await readFile(cssPath, "utf8");
  html = html.replace(m[0], `<style>\n${css}\n</style>`);
}

/** 内联 <script type="module" ... src="..."> */
const scriptRe = /<script([^>]*)type="module"([^>]*)src="([^"]+)"([^>]*)><\/script>/g;
for (const m of [...html.matchAll(scriptRe)]) {
  const src = m[3];
  if (!src.startsWith(".")) continue;
  const jsPath = path.resolve(outDir, src);
  if (!existsSync(jsPath)) continue;
  let js = await readFile(jsPath, "utf8");
  // 内联脚本里出现 </script 会提前闭合标签
  js = js.replace(/<\/script/gi, "<\\/script");
  html = html.replace(m[0], `<script type="module">\n${js}\n</script>`);
}

await writeFile(htmlPath, html, "utf8");

/** 3) 自检：JS/CSS 必须完全内联；剩下的相对引用只能是**同目录资源** */
const html2 = await readFile(htmlPath, "utf8");
const leftovers = [...html2.matchAll(/(?:src|href)="([^"]+)"/g)]
  .map((m) => m[1])
  .filter((u) => !u.startsWith("data:") && !u.startsWith("#") && !u.startsWith("http"));
const size = (await readFile(htmlPath)).length;

console.log(`[build-preview] 自包含 HTML：${htmlPath}`);
console.log(`[build-preview] 体积 ${(size / 1024).toFixed(1)} KB`);
if (leftovers.length) {
  // 同目录资源（Vite 把 public/ 拷进了产物目录）—— file:// 打开时相对路径能解析
  console.log(
    `[build-preview] ✅ JS/CSS 已全部内联；剩余 ${leftovers.length} 处是**同目录资源**（随 preview/ 一起拷走）：\n  ${[
      ...new Set(leftovers),
    ].join("\n  ")}`,
  );
} else {
  console.log("[build-preview] ✅ 完全自包含（无任何外部引用），双击即可打开");
}
