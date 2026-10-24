#!/usr/bin/env node
/**
 * 预览件截图 —— 驱动**本机的 Chromium**（headless + CDP）把某一屏抓成 PNG。
 *
 * 用法：node tools/shot-preview.mjs <scenario> <out.png> [宽] [高]
 * 例：  node tools/shot-preview.mjs pi-perm D:/out/pi-perm.png 520 700
 *
 * ## 为什么不用 Playwright
 *
 * 本机 `ms-playwright/` 里装的 chromium 是 1181/1223/1243，
 * 而 npx 缓存里的 playwright 要的是 1200/1208/1247/1248 ⇒ **版本对不上**，
 * `chromium.launch()` 直接报「Please run npx playwright install」。
 * ⇒ 这里直接用 **Chromium 自带的 headless + CDP**（DevTools 协议），
 * 零依赖、零安装、版本永远匹配。
 *
 * ## 流程
 * 1) 起一个本地静态服务器（服务 preview/ 目录）—— file:// 下 ES module 会被 CORS 挡；
 * 2) 起 chromium --headless --remote-debugging-port=0；
 * 3) 用 CDP（WebSocket）开页、等 `document.body.dataset.previewReady`、截图。
 */
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { existsSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const previewDir = path.join(root, "preview");

const scenario = process.argv[2] ?? "default";
const outPath = process.argv[3] ?? path.join(root, "tmp", `preview-${scenario}.png`);
const width = Number(process.argv[4] ?? 520);
const height = Number(process.argv[5] ?? 700);

/** 找本机 chromium（ms-playwright 下的任一版本）。 */
function findChromium() {
  const base = path.join(process.env.LOCALAPPDATA ?? "", "ms-playwright");
  if (!existsSync(base)) return null;
  const dirs = readdirSync(base)
    .filter((d) => d.startsWith("chromium-"))
    .sort()
    .reverse();
  for (const d of dirs) {
    for (const sub of ["chrome-win64", "chrome-win"]) {
      const exe = path.join(base, d, sub, "chrome.exe");
      if (existsSync(exe)) return exe;
    }
  }
  return null;
}

const htmlPath = path.join(previewDir, "preview.html");
if (!existsSync(htmlPath)) {
  console.error(`[shot] 没有预览件：${htmlPath}（先跑 npm run build:preview）`);
  process.exit(1);
}
const chrome = findChromium();
if (!chrome) {
  console.error("[shot] 找不到本机 chromium（ms-playwright 下）");
  process.exit(1);
}

/** 极简静态服务器（只服务 preview/ 目录）。 */
const server = createServer(async (req, res) => {
  const url = (req.url ?? "/").split("?")[0];
  const file = path.join(previewDir, url === "/" ? "/preview.html" : url);
  if (!existsSync(file)) {
    res.writeHead(404).end("not found");
    return;
  }
  const ext = path.extname(file);
  const type =
    ext === ".html" ? "text/html; charset=utf-8"
    : ext === ".png" ? "image/png"
    : ext === ".js" ? "text/javascript"
    : "application/octet-stream";
  res.writeHead(200, { "content-type": type });
  res.end(await readFile(file));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const url = `http://127.0.0.1:${port}/preview.html?scenario=${encodeURIComponent(scenario)}&harness=0`;

/** 起 chromium（headless + CDP）。 */
const chromeProc = spawn(
  chrome,
  [
    "--headless=new",
    "--disable-gpu",
    "--hide-scrollbars",
    "--remote-debugging-port=0",
    `--window-size=${width},${height}`,
    "--user-data-dir=" + path.join(root, "tmp", "chrome-shot-profile"),
    "about:blank",
  ],
  { stdio: ["ignore", "pipe", "pipe"] },
);

/** 从 stderr 里抠出 CDP 端口（`DevTools listening on ws://...`）。 */
const wsUrl = await new Promise((resolve, reject) => {
  let buf = "";
  const t = setTimeout(() => reject(new Error("等 chromium 起来超时")), 20000);
  chromeProc.stderr.on("data", (d) => {
    buf += d.toString();
    const m = buf.match(/DevTools listening on (ws:\/\/\S+)/);
    if (m) {
      clearTimeout(t);
      resolve(m[1]);
    }
  });
  chromeProc.on("exit", (c) => {
    clearTimeout(t);
    reject(new Error(`chromium 退出（code ${c}）：${buf.slice(-400)}`));
  });
});

/** 连浏览器级 WS，建一个 target 并拿它的 page WS。 */
const browser = {
  onMessage: null,
  onEvent: null,
  connect() {
    this.ws = new WebSocket(wsUrl);
    this.opened = new Promise((resolve, reject) => {
      this.ws.addEventListener("open", resolve, { once: true });
      this.ws.addEventListener("error", reject, { once: true });
    });
    this.ws.addEventListener("message", (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id && pending.has(m.id)) {
        const { resolve, reject } = pending.get(m.id);
        pending.delete(m.id);
        m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
        return;
      }
      this.onEvent?.(ev.data);
    });
  },
  send(obj) {
    // ⚠️ 必须等 socket **真的 open** —— Node 的全局 WebSocket 在 open 前 send()
    //    直接抛 InvalidStateError("Sent before connected")。
    return this.opened.then(
      () =>
        new Promise((resolve, reject) => {
          pending.set(obj.id, { resolve, reject });
          this.ws.send(JSON.stringify(obj));
        }),
    );
  },
  close() {
    try {
      this.ws.close();
    } catch {
      /* 已关 */
    }
  },
};
browser.connect();

let msgId = 0;
const pending = new Map();
const send = (method, params = {}, sessionId) =>
  browser.send({ id: ++msgId, method, params, sessionId });

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });

const pageSend = (method, params = {}) => send(method, params, sessionId);
await pageSend("Page.enable");
await pageSend("Runtime.enable");
await pageSend("Emulation.setDeviceMetricsOverride", {
  width,
  height,
  deviceScaleFactor: 1,
  mobile: false,
});

const errors = [];
await pageSend("Log.enable").catch(() => {});
browser.onEvent = (raw) => {
  const m = JSON.parse(raw);
  if (m.method === "Log.entryAdded" && m.params?.entry?.level === "error") {
    errors.push(m.params.entry.text);
  }
  if (m.method === "Runtime.exceptionThrown") {
    errors.push(m.params?.exceptionDetails?.text ?? "exception");
  }
};

await pageSend("Page.navigate", { url });

/** 等预览件自己标记就绪（最多 20s）。 */
const deadline = Date.now() + 20000;
let ready = false;
while (Date.now() < deadline) {
  const r = await pageSend("Runtime.evaluate", {
    expression: "document.body && document.body.dataset.previewReady === '1'",
    returnByValue: true,
  });
  if (r?.result?.value) {
    ready = true;
    break;
  }
  await new Promise((r2) => setTimeout(r2, 120));
}
if (!ready) console.warn("[shot] ⚠️ 预览件没在 20s 内标记就绪（可能场景名不存在）");

// 再等一拍，让最后一次重画落地
await new Promise((r) => setTimeout(r, 500));

const shot = await pageSend("Page.captureScreenshot", { format: "png" });
await mkdir(path.dirname(outPath), { recursive: true });
await writeFile(outPath, Buffer.from(shot.data, "base64"));

console.log(`[shot] ${scenario} @${width}x${height} → ${outPath}`);
if (errors.length) {
  console.log(`[shot] ⚠️ 页面报错 ${errors.length} 条：`);
  for (const e of errors.slice(0, 6)) console.log(`  ${e}`);
}

browser.close();
chromeProc.kill();
server.close();
process.exit(0);
