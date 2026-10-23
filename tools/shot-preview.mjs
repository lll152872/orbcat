/**
 * 预览件截图 —— 把「纯 HTML 渲染件」里的某一屏抓成 PNG。
 *
 * 用法：node tools/shot-preview.mjs <scenario> <out.png> [宽] [高]
 * 例：  node tools/shot-preview.mjs behavior D:/out/behavior.png 690 900
 *
 * ## 为什么走 Playwright 而不是让人自己看
 *
 * 用户要**目视核对**「三选一的选择器和 690px 下的设置面板」。
 * 截图能一眼看出：① 三个选项都在；② 选中项对不对；③ 690px 下有没有挤/换行/溢出。
 *
 * ## 为什么用本地静态服务器而不是 file://
 *
 * 预览件已经是自包含单文件（build-preview.mjs 把 JS/CSS 都内联了），
 * 但 file:// 下某些资源的加载受 CORS 限制，用 http 更稳。
 */
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const previewDir = path.join(root, "preview");

/**
 * playwright 解析 —— 它不在本仓库的 node_modules 里（是 npx 缓存里的全局副本）。
 *
 * 做法：先按正常方式 import；失败就去 npx 缓存里找版本最高的那个。
 * ⚠️ 不写死路径 —— npx 缓存目录名会变（带 hash）。
 * ⚠️ 必须用**同步**读版本号（fs/promises 的 readFile 返回 Promise，
 *    JSON.parse(Promise) 会抛 ⇒ 候选全被跳过 ⇒ 永远「找不到 playwright」）。
 */
function loadPlaywright() {
  try {
    return require("playwright");
  } catch {
    const req = createRequire(import.meta.url);
    const cacheRoot = path.join(process.env.LOCALAPPDATA ?? "", "npm-cache", "_npx");
    if (!existsSync(cacheRoot)) throw new Error("找不到 playwright（npx 缓存里也没有）");
    const candidates = [];
    for (const dir of readdirSync(cacheRoot)) {
      const p = path.join(cacheRoot, dir, "node_modules", "playwright");
      if (!existsSync(p)) continue;
      try {
        const v = JSON.parse(readFileSync(path.join(p, "package.json"), "utf8")).version;
        candidates.push({ p, v });
      } catch {
        /* 跳过坏的 */
      }
    }
    if (!candidates.length) throw new Error("npx 缓存里没有 playwright");
    candidates.sort((a, b) => (a.v < b.v ? 1 : -1));
    return req(candidates[0].p);
  }
}

const require = createRequire(import.meta.url);
const { chromium } = loadPlaywright();

const scenario = process.argv[2] ?? "behavior";
const outPath = process.argv[3] ?? path.join(root, "tmp", `preview-${scenario}.png`);
const width = Number(process.argv[4] ?? 690);
const height = Number(process.argv[5] ?? 900);

const htmlPath = path.join(previewDir, "preview.html");
if (!existsSync(htmlPath)) {
  console.error(`[shot] 没有预览件：${htmlPath}（先跑 npm run build:preview）`);
  process.exit(1);
}

/** 极简静态服务器（只服务 preview/ 目录） */
const server = createServer(async (req, res) => {
  const url = (req.url ?? "/").split("?")[0];
  const rel = url === "/" ? "/preview.html" : url;
  const file = path.join(previewDir, rel);
  if (!existsSync(file)) {
    res.writeHead(404).end("not found");
    return;
  }
  const ext = path.extname(file);
  const type =
    ext === ".html"
      ? "text/html; charset=utf-8"
      : ext === ".png"
        ? "image/png"
        : ext === ".js"
          ? "text/javascript"
          : "application/octet-stream";
  res.writeHead(200, { "content-type": type });
  res.end(await readFile(file));
});

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const url = `http://127.0.0.1:${port}/preview.html?scenario=${encodeURIComponent(scenario)}`;

const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width, height },
  deviceScaleFactor: 1,
});

const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => {
  if (m.type() === "error") errors.push(`console: ${m.text()}`);
});

await page.goto(url, { waitUntil: "load" });
// 等预览件自己标记就绪
await page.waitForFunction(() => document.body.dataset.previewReady === "1", null, {
  timeout: 15000,
});
// 再等一拍，让最后一次重画落地
await page.waitForTimeout(400);

await page.screenshot({ path: outPath, fullPage: false });

console.log(`[shot] ${scenario} @${width}x${height} → ${outPath}`);
if (errors.length) {
  console.log(`[shot] ⚠️ 页面报错 ${errors.length} 条：`);
  for (const e of errors.slice(0, 8)) console.log(`  ${e}`);
}

await browser.close();
server.close();
