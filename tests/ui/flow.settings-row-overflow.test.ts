import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * 守卫用例：设置页「行为与安全」组的**右列不能被裁掉**。
 *
 * ## 背景（2026-10-24 真机 bug）
 *
 * 用户真机截图：设置页 →「行为与安全」组，**右列整列越出面板右边缘被切掉** ——
 * 「内核」「项目目录自动读写」「失焦自动收起」的开关**只剩左半截**。
 *
 * ## 根因（实测，不是猜）
 *
 * 项目**没有**全局 `* { box-sizing: border-box }`（默认 content-box）。
 * `.set-row` 是 `width: 100%` + `padding-left: 17px` ⇒ content-box 下**总宽多 17px**。
 * 而 `row()` helper 是 `const tag = act ? "button" : "div"`：
 * - **有 act 的行是 `<button>`** ⇒ 浏览器 UA 样式表给 button **默认 border-box** ⇒ 恰好正常；
 * - **没有 act 的行是 `<div>`** ⇒ 落回 content-box ⇒ 超出 17px
 *   ⇒ 被祖先 `.set-sec { overflow: hidden }` 切掉 ⇒ 34px 开关正好被切一半。
 *
 * ## 为什么断言在「样式契约层」而不是量像素
 *
 * ⚠️ **happy-dom（vitest 的 DOM 环境）没有排版引擎** —— `clientWidth` / `getBoundingClientRect()`
 * 全是 0，量不出真实裁切。
 * ⇒ 这里断言**导致这个 bug 的那几条 CSS 契约**（谁设 box-sizing、值列能不能收缩、
 * 开关会不会被挤、祖先有没有裁切），把「修好」这件事锁在**可判定的属性**上。
 * 真实像素验证走 `tools/measure-settings.mjs`（本机 Chromium + CDP，取真盒模型）。
 */

/**
 * 读 src/styles.css（**先剥掉注释**）。
 *
 * ⚠️ 必须剥：本文件的注释里夹着大量 `/* … *\/`，而且**注释常卡在
 *    「上一条规则的 `}`」与「下一条选择器」之间** ⇒ 不剥的话正则根本匹配不到
 *    下一条规则（实测 `.set-row-right` / `.sw` 全部找不到）。
 *    注释里还夹着 `;` 与 `:` ⇒ 不断掉会把声明切碎。
 */
const CSS_RAW = readFileSync(
  path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../src/styles.css"),
  "utf8",
);
const CSS = CSS_RAW.replace(/\/\*[\s\S]*?\*\//g, " ");

/**
 * 取某个选择器的 CSS 块（`selector { … }`）。
 *
 * ⚠️ 同一个选择器可能出现多次（后写的覆盖先写的）⇒ 返回**所有**匹配块。
 */
function cssBlocks(selector: string): string[] {
  const out: string[] = [];
  const esc = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp(`(?:^|[};])\\s*${esc}\\s*\\{([^}]*)\\}`, "g");
  let m: RegExpExecArray | null;
  while ((m = re.exec(CSS))) out.push(m[1]!);
  return out;
}

/** 这个块里有没有声明某属性（值可按正则限定）。 */
function declares(blocks: string[], prop: string, value?: RegExp): boolean {
  for (const b of blocks) {
    for (const decl of b.split(";")) {
      const m = decl.match(new RegExp(`^\\s*${prop}\\s*:\\s*(.+)$`));
      if (m && (!value || value.test(m[1]!.trim()))) return true;
    }
  }
  return false;
}

describe("守卫 · 设置页右列不被裁", () => {
  it("① `.set-row` 必须显式 border-box（否则 div 行会超出 17px 被裁）", () => {
    const blocks = cssBlocks(".set-row");
    expect(blocks.length, "没找到 .set-row 规则（样式文件结构变了？）").toBeGreaterThan(0);
    expect(
      declares(blocks, "box-sizing", /border-box/),
      ".set-row 没有 border-box ⇒ <div> 行（无 act 的那些）会按 content-box 算，\n" +
        "  总宽 = 100% + padding-left 17px ⇒ 超出面板右缘 ⇒ 被 .set-sec{overflow:hidden} 切掉",
    ).toBe(true);
  });

  it("② 值列要能收缩 + 省略号收尾（长文字不许把开关挤出去）", () => {
    const val = cssBlocks(".set-row-val");
    expect(val.length, "没找到 .set-row-val 规则").toBeGreaterThan(0);
    expect(declares(val, "overflow", /hidden/), "值列要 overflow:hidden（才能省略号）").toBe(true);
    expect(
      declares(val, "text-overflow", /ellipsis/),
      "值列要 text-overflow:ellipsis（长文字省略号收尾，不许溢出）",
    ).toBe(true);
  });

  it("③ 值+开关那一组（.set-row-right）必须可收缩，不能 flex:none", () => {
    const right = cssBlocks(".set-row-right");
    expect(right.length, "没找到 .set-row-right 规则").toBeGreaterThan(0);
    const flexNone = right.some((b) => /(?:^|;)\s*flex\s*:\s*none/.test(b));
    expect(
      flexNone,
      ".set-row-right 是 flex:none ⇒ 它不收缩 ⇒ 值一长就把右边的开关挤出面板",
    ).toBe(false);
    expect(
      declares(right, "min-width", /0/),
      ".set-row-right 要 min-width:0（flex 子项能收缩的前提）",
    ).toBe(true);
  });

  it("④ 组里的值要能收缩（否则组收缩了、值还是把开关顶出去）", () => {
    const inner = cssBlocks(".set-row-right .set-row-val");
    expect(inner.length, "没找到 .set-row-right .set-row-val 规则").toBeGreaterThan(0);
    expect(
      declares(inner, "min-width", /0/),
      "组里的值要 min-width:0（否则它按 min-content 撑开，把开关挤出去）",
    ).toBe(true);
    expect(
      inner.some((b) => /(?:^|;)\s*flex\s*:\s*(0 1 auto|1 1 auto|0 1 0|1 0 auto)/.test(b)),
      "组里的值要可收缩（flex: 0 1 auto 之类），不许 flex:none",
    ).toBe(true);
  });

  it("⑤ 开关本身不能被压缩（`.sw` 要 flex:none）", () => {
    const sw = cssBlocks(".sw");
    expect(sw.length, "没找到 .sw 规则").toBeGreaterThan(0);
    expect(
      declares(sw, "flex", /none/),
      ".sw 必须 flex:none（开关是固定 34px 的控件，被压扁就成「只剩半截」）",
    ).toBe(true);
  });

  it("⑥ 行容器本身不裁切（裁切交给 .set-sec，但行不该自己 overflow:hidden 藏内容）", () => {
    const row = cssBlocks(".set-row");
    const clipping = row.filter((b) => /(?:^|;)\s*overflow(-x)?\s*:\s*hidden/.test(b));
    expect(
      clipping.length,
      ".set-row 不该自己 overflow:hidden（值应该用 ellipsis 收尾，不是被裁掉）",
    ).toBe(0);
  });

  it("⑦ 结构层：带开关的行，其右列容器必须可收缩（值与开关同组）", () => {
    // 源码层：row() helper 产出的结构 —— 有开关且有值时包进 .set-row-right
    const main = readFileSync(
      path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../src/main.ts"),
      "utf8",
    );
    expect(
      main.includes('class="set-row-right"'),
      "main.ts 的 row() helper 里没有 .set-row-right ⇒ 值与开关没分组，长值会顶掉开关",
    ).toBe(true);
    // 开关是 .sw（不是裸 button）⇒ 才有 flex:none 可依
    expect(main.includes('class="sw'), "row() helper 里没有 .sw 开关").toBe(true);
  });
});
