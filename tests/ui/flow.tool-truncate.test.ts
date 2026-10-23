/**
 * 工具结果预览的**截断口径**。
 *
 * ## 为什么单独测这个模块
 *
 * 截断逻辑以前散在两处且**口径不一致**：
 * - 后端 `pi_run.rs` 按 1200 **字符**切
 * - 前端 `main.ts` 按 8K **字符**切（这个阈值几乎永远不触发 → 实际生效的只有后端）
 *
 * 现在两边都按**行 + 显示宽度**，规则一致（学 pi 的 `visual-truncate.ts`）。
 * 这个模块是前端口径的**唯一实现**（Rust 侧 `pi_run.rs` 有对应的一份），
 * 所以可以直接 import 测 —— 不用 import `main.ts`（那会触发 `boot()`）。
 */

import { describe, expect, it } from "vitest";
import {
  charWidth,
  clipLines,
  LINE_WIDTH,
  previewShape,
  truncateLineWidth,
} from "@/truncate";

describe("工具结果预览 · 截断口径", () => {
  it("行数不超就原样返回", () => {
    const s = "1\n2\n3";
    expect(clipLines(s, "read_file")).toBe(s);
  });

  it("行数超了按行截断，并提示隐藏了多少行", () => {
    const s = Array.from({ length: 30 }, (_, i) => `第${i + 1}行`).join("\n");
    const out = clipLines(s, "read_file");
    expect(out).toContain("已隐藏 20 行");
    expect(out).toContain("已截断");
    // 保留前 10 行（通用档）
    expect(out.startsWith("第1行\n第2行")).toBe(true);
    expect(out).not.toContain("第20行");
  });

  it("留尾时保留最后几行（日志和错误栈的尾巴才是重点）", () => {
    const s = Array.from({ length: 30 }, (_, i) => `第${i + 1}行`).join("\n");
    const out = clipLines(s, "run_command");
    // run_command = 5 行留尾
    expect(out).toContain("已隐藏 25 行");
    expect(out).toContain("第30行");
    expect(out).not.toContain("第1行\n第2行");
  });

  it("单行超长按**显示宽度**截断（不能横向铺满 460px 面板）", () => {
    const long = "x".repeat(5000);
    const out = clipLines(long, "read_file");
    expect(out.length).toBeLessThan(200);
    expect(out).toContain("行过长");
  });

  it("中文按 2 个宽度算", () => {
    expect(charWidth("中")).toBe(2);
    expect(charWidth("a")).toBe(1);
    // 30 个汉字 = 60 宽，正好一行
    const cn = "中".repeat(30);
    expect(truncateLineWidth(cn).cut).toBe(false);
    expect(truncateLineWidth(cn + "中").cut).toBe(true);
  });

  it("各工具的预览档位抄 pi", () => {
    expect(previewShape("run_command")).toEqual({ max: 5, keep: "end" });
    expect(previewShape("read_file")).toEqual({ max: 10, keep: "head" });
    expect(previewShape("grep")).toEqual({ max: 15, keep: "head" });
    expect(previewShape("list_dir")).toEqual({ max: 20, keep: "head" });
    expect(previewShape("whatever")).toEqual({ max: 10, keep: "head" });
  });

  it("宽度上限与 Rust 侧一致（60）", () => {
    expect(LINE_WIDTH).toBe(60);
  });
});
