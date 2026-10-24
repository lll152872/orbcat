import { describe, it, expect } from "vitest";
import { mdToHtml } from "../../src/markdown";
import { splitBubbles } from "../../src/bubble";

/**
 * 表情包 markdown **必须活着穿过切气泡**（真机回归）。
 *
 * 真机现场：闲聊模式下模型发了
 *   `![双手比心卖萌，星星眼超开心](D:\…bixin.jpg)`
 * 界面却显示原文。根因：`splitSentences` 的句末标点 lookbehind 把
 * `![` 里的 **`!` 当成句末感叹号**，一刀切成 `!` + `[描述](路径)` 两半
 * —— 图片语法报废，剩下的 `[…](本地路径)` 又被链接渲染判为危险协议
 * 原样显示。修复 = 句子切分前先把图片/链接语法摘出来，切完放回去。
 *
 * 断言口径：**图片语法必须完整地待在某一个气泡里**（不能是 `!` + `[…]` 两半）。
 * 短句被 `mergeShort` 用 `\n\n` 并进相邻气泡是既有设计，不算破坏。
 */

const PATH =
  "D:\\workplace\\悬浮小agent\\orbcat\\src-tauri\\../agent-data\\memes\\dafeiyu-001\\memes\\like\\bixin.jpg";
const IMG_MD = `![双手比心卖萌，星星眼超开心](${PATH})`;
/** 真机那条回复的原样（`!` 单独一行是模型自己的语气词，不是我们的产物） */
const ACTUAL = `!\n${IMG_MD}\n来了，一张比心~`;

describe("表情包 markdown 穿过切气泡", () => {
  it("图片语法必须完整地待在某一个气泡里（真机回归：不许切成 `!` + `[…]`）", () => {
    const parts = splitBubbles(ACTUAL);
    const intact = parts.filter((p) => p.includes(IMG_MD));
    expect(
      intact.length,
      `图片语法被切坏了：${JSON.stringify(parts)}`,
    ).toBe(1);
    // 被切坏的特征是出现「孤立的 `[…]`」—— 这里必须没有
    expect(
      parts.some((p) => /^\[双手比心卖萌/.test(p.trim())),
      "不允许出现失去 `!` 的裸链接形式",
    ).toBe(false);
  });

  it("带图片的那个气泡经 mdToHtml 仍是 <img>，而不是原文", () => {
    const parts = splitBubbles(ACTUAL);
    const carrier = parts.find((p) => p.includes(IMG_MD))!;
    const html = mdToHtml(carrier);
    expect(html).toContain(`<img class="md-img`);
    expect(html).toContain(`data-img="${PATH}"`);
    expect(html).toContain(`alt="双手比心卖萌，星星眼超开心"`);
  });

  // ── 2026-10-24 真机回归：表情包变「破图图标 + alt 文字」──
  //
  // 本地路径的 <img> 必须是 **无 src 的占位**（路径挂 data-img）。
  // ⚠️ 写 `src="D:\…"` 在 webview 里**必定加载失败** ⇒ 只留一个破图标。
  it("本地路径必须渲染成无 src 的 data-img 占位（不许直接写 src）", () => {
    const html = mdToHtml(IMG_MD);
    expect(html).toContain(`class="md-img md-img-local"`);
    expect(html).toContain(`data-img="${PATH}"`);
    // ⚠️ 关键：本地路径**不许**出现 src=（那在 webview 里必定加载失败）
    expect(
      /<img[^>]*\ssrc=/.test(html),
      `本地路径的 img 不许带 src：${html}`,
    ).toBe(false);
  });

  it("路径里的反斜杠必须原样保留（不许被改写成无分隔符的畸形名）", () => {
    const html = mdToHtml(IMG_MD);
    expect(html).toContain(PATH);
    // ⚠️ 不许出现「反斜杠被删掉」的畸形名
    //    （真机现场：agent-data 根目录躺过 `D:orbcatagentscreenshotshw7_filled.png`
    //      —— 那是 `D:\orbcat\agent-data\screenshots\hw7_filled.png` 去掉所有分隔符的结果）
    const squashed = PATH.replace(/\\/g, "");
    expect(
      html.includes(squashed),
      `路径被改写成了无分隔符的畸形名：${squashed}`,
    ).toBe(false);
    expect(
      (html.match(/\\/g) || []).length >= PATH.split("\\").length - 1,
      `路径里的反斜杠数量不对：${html}`,
    ).toBe(true);
  });

  it("alt 里带 `！` 也不能把图片切坏", () => {
    const src = `![哇！好耶](D:\\a\\b.jpg)`;
    const parts = splitBubbles(src);
    expect(parts.join("\n")).toBe(src, "单图一条必须原样保留");
    expect(mdToHtml(parts[0])).toContain("<img");
  });

  it("链接语法同理（文本里的感叹号不许切开链接）", () => {
    const src = `看这个！[点这看图](http://x/a.png)`;
    const parts = splitBubbles(src);
    expect(parts.some((p) => p.includes("[点这看图](http://x/a.png)"))).toBe(
      true,
      `链接被切坏了：${JSON.stringify(parts)}`,
    );
  });

  it("正常句末标点的切分不受影响（回归既有行为，用足够长的句子）", () => {
    const src =
      "刚看到你消息，我这边正好也在弄这个，等下细说哈。\n\n不过先说结论：那个方案能走，不用改架构，你放心。";
    const parts = splitBubbles(src);
    expect(parts.length).toBe(2);
    expect(parts[0]).toContain("刚看到你消息");
    expect(parts[1]).toContain("先说结论");
  });
});
