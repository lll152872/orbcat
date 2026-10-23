/**
 * 工具结果预览的**截断口径**。
 *
 * ## 为什么单独成文件
 *
 * 1. **可单测**：这些函数在 `main.ts` 里的话，一 import 就会触发 `boot()`
 *    （main.ts 在模块顶层就 boot 了），没法直接测。
 * 2. **口径只一份**：以前是「后端按 1200 **字符**截 + 前端按 8K **字符**截」——
 *    两套口径不一致是明确的坏味道（前端那个阈值几乎永远不触发，
 *    实际生效的只有后端那个）。现在前后端都按**行 + 显示宽度**，
 *    规则也一致（Rust 侧对应 `pi_run.rs` 的 `preview_shape` / `truncate_lines`）。
 *
 * ## 为什么按行不按字符
 *
 * 按字符一刀切会把一条压缩 JSON **从中间斩断**，头尾都不完整。
 * pi 的理由原话（`visual-truncate.ts` 注释）：按逻辑行截断会让
 * **单条长行（如压缩 JSON）横向铺满全屏** —— 所以它截完行数还要再截一次宽度。
 * 460px 的面板同理。
 */

/** 一行的**显示宽度**上限 —— 与 Rust 侧 `pi_run::LINE_WIDTH` 同值。 */
export const LINE_WIDTH = 60;

/** 一个字符占几个显示宽度（中文/日文/韩文等算 2）。 */
export function charWidth(c: string): number {
  const u = c.codePointAt(0) ?? 0;
  const wide =
    (u >= 0x1100 && u <= 0x115f) ||
    (u >= 0x2e80 && u <= 0x303e) ||
    (u >= 0x3041 && u <= 0x33ff) ||
    (u >= 0x3400 && u <= 0x4dbf) ||
    (u >= 0x4e00 && u <= 0x9fff) ||
    (u >= 0xa000 && u <= 0xa4cf) ||
    (u >= 0xac00 && u <= 0xd7a3) ||
    (u >= 0xf900 && u <= 0xfaff) ||
    (u >= 0xfe30 && u <= 0xfe6f) ||
    (u >= 0xff00 && u <= 0xff60) ||
    (u >= 0xffe0 && u <= 0xffe6);
  return wide ? 2 : 1;
}

/** 单行按**显示宽度**截断。 */
export function truncateLineWidth(line: string): { text: string; cut: boolean } {
  let w = 0;
  let out = "";
  for (const ch of line) {
    const cw = charWidth(ch);
    if (w + cw > LINE_WIDTH) return { text: `${out}…`, cut: true };
    w += cw;
    out += ch;
  }
  return { text: out, cut: false };
}

/**
 * 这个工具的结果该看多少行、留头还是留尾。
 *
 * 抄 pi 的 `renderers/*` 里的预览行数（`bash` 5 行留尾、`read` 10 行、
 * `grep` 15 行、`ls` 20 行、通用 10 行）。留尾是因为**日志和错误栈的尾巴才是重点**。
 */
export function previewShape(tool: string): { max: number; keep: "head" | "end" } {
  switch (tool) {
    case "run_command":
    case "bash":
    case "shell":
      return { max: 5, keep: "end" };
    case "grep":
    case "search":
      return { max: 15, keep: "head" };
    case "list_dir":
    case "ls":
      return { max: 20, keep: "head" };
    default:
      return { max: 10, keep: "head" };
  }
}

/**
 * 按**行**截断（两级：行数 + 显示宽度）。
 *
 * 提示说的是**行**不是字符 —— 用户关心的是「还有多少行没看」，
 * 而且**不静默丢弃**：静默丢行用户会以为那就是全部。
 */
export function clipLines(text: string, tool: string): string {
  const { max, keep } = previewShape(tool);
  const lines = text.split("\n");

  let wideCut = 0;
  const trimmed = lines.map((l) => {
    const r = truncateLineWidth(l);
    if (r.cut) wideCut += 1;
    return r.text;
  });

  if (lines.length > max) {
    const hidden = lines.length - max;
    const kept = keep === "head" ? trimmed.slice(0, max) : trimmed.slice(trimmed.length - max);
    return `${kept.join("\n")}\n…（已隐藏 ${hidden} 行，已截断，点开看全部）`;
  }
  let out = trimmed.join("\n");
  if (wideCut > 0) out += `\n…（${wideCut} 行过长，已截断）`;
  return out;
}
