#!/usr/bin/env node
/**
 * 量一量：设置页在 460 宽下到底谁在溢出、谁在裁。
 *
 * 用法：node tools/measure-settings.mjs [scenario]
 *
 * 走**真 CSS + 真渲染**（预览件），用 CDP 的 Runtime.evaluate 取真实盒模型。
 * ⚠️ 不靠算术猜 —— 宽度链有别的环节（flex 拉伸、祖先 padding…），
 *    只有实测的 bounding box 说了算。
 */
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { existsSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const previewDir = path.join(root, "preview");
const scenario = process.argv[2] ?? "settings";
/**
 * 可选：把面板宽度改成别的值（模拟「更窄」）。
 * ⚠️ 真机上面板宽是**固定常量** 460 逻辑像素（lib.rs:230 PANEL_W_LOGICAL，
 *    tauri.conf.json `resizable: false`）⇒ 没有「用户拖窄」这条路。
 *    这里改成更窄只是**验证布局不随宽度变差**（健壮性检查）。
 */
const narrow = Number(process.argv[3] ?? 0);

function findChromium() {
  const base = path.join(process.env.LOCALAPPDATA ?? "", "ms-playwright");
  if (!existsSync(base)) return null;
  for (const d of readdirSync(base).filter((x) => x.startsWith("chromium-")).sort().reverse()) {
    for (const sub of ["chrome-win64", "chrome-win"]) {
      const exe = path.join(base, d, sub, "chrome.exe");
      if (existsSync(exe)) return exe;
    }
  }
  return null;
}

const server = createServer(async (req, res) => {
  const url = (req.url ?? "/").split("?")[0];
  const file = path.join(previewDir, url === "/" ? "/preview.html" : url);
  if (!existsSync(file)) return res.writeHead(404).end("nf");
  const ext = path.extname(file);
  res.writeHead(200, {
    "content-type":
      ext === ".html" ? "text/html; charset=utf-8" : ext === ".png" ? "image/png" : "text/javascript",
  });
  res.end(await readFile(file));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const chrome = spawn(
  findChromium(),
  [
    "--headless=new",
    "--disable-gpu",
    "--hide-scrollbars",
    "--remote-debugging-port=0",
    "--window-size=520,900",
    "--user-data-dir=" + path.join(root, "tmp", "chrome-measure-profile"),
    "about:blank",
  ],
  { stdio: ["ignore", "pipe", "pipe"] },
);

const wsUrl = await new Promise((resolve, reject) => {
  let buf = "";
  const t = setTimeout(() => reject(new Error("chromium 起来超时")), 20000);
  chrome.stderr.on("data", (d) => {
    buf += d.toString();
    const m = buf.match(/DevTools listening on (ws:\/\/\S+)/);
    if (m) {
      clearTimeout(t);
      resolve(m[1]);
    }
  });
});

let msgId = 0;
const pending = new Map();
const ws = new WebSocket(wsUrl);
const opened = new Promise((res, rej) => {
  ws.addEventListener("open", res, { once: true });
  ws.addEventListener("error", rej, { once: true });
});
ws.addEventListener("message", (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) {
    const { resolve, reject } = pending.get(m.id);
    pending.delete(m.id);
    m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
  }
});
await opened;
const send = (method, params = {}, sessionId) =>
  new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
const p = (method, params = {}) => send(method, params, sessionId);
await p("Page.enable");
await p("Runtime.enable");
await p("Emulation.setDeviceMetricsOverride", {
  width: 520,
  height: 900,
  deviceScaleFactor: 1,
  mobile: false,
});
await p("Page.navigate", { url: `http://127.0.0.1:${port}/preview.html?scenario=${scenario}&harness=0` });

const deadline = Date.now() + 20000;
while (Date.now() < deadline) {
  const r = await p("Runtime.evaluate", {
    expression: "document.body && document.body.dataset.previewReady === '1'",
    returnByValue: true,
  });
  if (r?.result?.value) break;
  await new Promise((r2) => setTimeout(r2, 120));
}
await new Promise((r) => setTimeout(r, 600));

/**
 * 进设置页：点齿轮 → 展开「行为与安全」组 → 滚到该组。
 * ⚠️ 场景不一定跳到设置页（`?scenario=settings` 落在对话流），
 *    所以这里**按真实交互路径**走一遍。
 */
await p("Runtime.evaluate", {
  expression: `(() => {
    document.getElementById("btn-gear")?.click();
    return true;
  })()`,
  returnByValue: true,
});
await new Promise((r) => setTimeout(r, 900));

/** 展开**所有**组（折叠着的话）—— 每组都要量，不能只看一组 */
await p("Runtime.evaluate", {
  expression: `(() => {
    for (const sec of document.querySelectorAll(".set-sec.folded")) {
      sec.querySelector(".set-sec-head")?.click();
    }
    return true;
  })()`,
  returnByValue: true,
});
await new Promise((r) => setTimeout(r, 600));

/** 滚到设置页（面板正文区） */
await p("Runtime.evaluate", {
  expression: `(() => {
    const b = document.getElementById("panel-body");
    if (b) b.scrollTop = b.scrollHeight;
    return true;
  })()`,
  returnByValue: true,
});
await new Promise((r) => setTimeout(r, 400));

/** 可选：把面板改成更窄（健壮性检查） */
if (narrow > 0) {
  await p("Runtime.evaluate", {
    expression: `(() => {
      const w = document.querySelector(".pv-window");
      if (w) { w.style.width = ${narrow} + "px"; }
      return true;
    })()`,
    returnByValue: true,
  });
  await new Promise((r) => setTimeout(r, 400));
}

/** 量的脚本：谁溢出、谁裁、每行右列的右边界在哪。 */
const probe = `(() => {
  const out = { rows: [], clips: [], groups: [], viewport: innerWidth };
  const host = document.getElementById("panel-body");
  out.panelBody = host ? { w: host.clientWidth, sw: host.scrollWidth, rect: Math.round(host.getBoundingClientRect().width) } : null;
  // 面板的**内容右缘**（所有行都不该越过它）
  const win = document.querySelector(".pv-window");
  out.panelRight = win ? Math.round(win.getBoundingClientRect().right) : null;
  // 所有 overflow:hidden 的祖先（可能的裁切者）
  for (const el of document.querySelectorAll("#panel-body *")) {
    const cs = getComputedStyle(el);
    if (cs.overflowX === "hidden" || cs.overflow === "hidden") {
      const r = el.getBoundingClientRect();
      out.clips.push({
        sel: el.className || el.tagName,
        overflow: cs.overflowX,
        w: Math.round(r.width),
        sw: el.scrollWidth,
        overflowsBy: el.scrollWidth - el.clientWidth,
      });
    }
  }
  // 每个组（含是否折叠）
  for (const sec of document.querySelectorAll(".set-sec")) {
    const head = sec.querySelector(".set-sec-head");
    const r = sec.getBoundingClientRect();
    out.groups.push({
      name: (head?.textContent || "").trim().slice(0, 12),
      folded: sec.classList.contains("folded"),
      left: Math.round(r.left), right: Math.round(r.right), w: Math.round(r.width),
      sw: sec.scrollWidth, cw: sec.clientWidth,
    });
  }
  // 设置页每一行（带 computed 样式：定位 content-box / border-box）
  for (const row of document.querySelectorAll(".set-row")) {
    const cs = getComputedStyle(row);
    const r = row.getBoundingClientRect();
    const name = row.querySelector(".set-row-name");
    const val = row.querySelector(".set-row-val");
    const sw = row.querySelector(".sw");
    const right = row.querySelector(".set-row-right");
    const g = (e) => e ? (({left,right,width}) => ({left: Math.round(left), right: Math.round(right), w: Math.round(width)}))(e.getBoundingClientRect()) : null;
    // 找出**到底是哪条规则**给它设了 box-sizing（跨样式表扫）
    const rules = [];
    for (const sheet of document.styleSheets) {
      let list;
      try { list = sheet.cssRules; } catch { continue; }
      for (const rule of list) {
        if (!rule.selectorText || !rule.style) continue;
        if (!rule.style.getPropertyValue("box-sizing")) continue;
        try { if (row.matches(rule.selectorText)) rules.push(rule.selectorText); } catch {}
      }
    }
    out.rows.push({
      name: (name?.textContent || "").trim().slice(0, 20),
      box: cs.boxSizing,
      boxRules: rules,
      cssW: cs.width,
      padL: cs.paddingLeft,
      row: { left: Math.round(r.left), right: Math.round(r.right), w: Math.round(r.width), sw: row.scrollWidth },
      val: g(val),
      sw: g(sw),
      rightGrp: g(right),
      clipped: sw ? Math.round(sw.getBoundingClientRect().right - r.right) : null,
    });
  }
  return JSON.stringify(out);
})()`;

const res = await p("Runtime.evaluate", { expression: probe, returnByValue: true });
console.log(res?.result?.value ?? JSON.stringify(res));

ws.close();
chrome.kill();
server.close();
process.exit(0);
