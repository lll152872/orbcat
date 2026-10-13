// @vitest-environment node
// ============================================================================
// 行尾一致性 —— 回归测试
// ============================================================================
//
// ## 为什么需要这组测试
//
// 2026-10-12：v1.0.0 那轮 CI，`Windows (regression guard)` 红在
//
//     SyntaxError: Invalid or unexpected token
//      ❯ tests/tools/package.test.ts:34:1
//
// 但同一个 commit 在本机 14/14 全绿。差异只有一个 ——
//
//   本机：工作区 LF
//   CI  ：windows-latest 默认 core.autocrlf=true → 全部转成 CRLF
//
// 已二分定位到具体构造：
//
//   无 shebang + CRLF .................... PASS
//   `#!/usr/bin/env node` + CRLF ....... FAIL  ← 就是它
//   `#!/usr/bin/env node` + LF ......... PASS
//   真实 package.mjs 去 shebang + CRLF . PASS
//
// shebang 行尾的 `\r` 被 vite 的剥离逻辑留下，模块最前面多出一个游离回车。
// `tools/check-no-user-data.mjs` 同样带 shebang 却从没炸过，因为它只被当
// CLI 直接跑、从不 import；`tools/package.mjs` 是第一个会被测试 import 的
// .mjs，所以只有它中招。
//
// ## 这组测试断言的是「策略」，不是「工作区当前状态」
//
// 刻意**不**扫描工作区里有没有 CRLF：那取决于每个贡献者各自的
// `core.autocrlf`，别人机器上必然红，而且红得毫无行动价值
//（`.gitattributes` 已经接管了 checkout，之后重新拉取即可）。
//
// 这里断言的是**策略本身是否生效** —— 用 `git check-attr` 问 git：
//「你checkout 这些文件时打算用什么行尾？」。这个答案只取决于仓库里的
// .gitattributes，与谁在什么机器上、用什么 git 配置无关。
// ============================================================================

import { describe, expect, it } from "vitest";
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  ".."
);

const CRLF_EXCEPTIONS = [".bat", ".cmd", ".ps1", ".vbs"];

/** 问 git：checkout 这些文件时用什么行尾 */
function checkAttrEol(files: string[]): Map<string, string> {
  const out = execFileSync(
    "git",
    ["check-attr", "eol", "--", ...files],
    { cwd: REPO_ROOT, encoding: "utf8" }
  );
  const m = new Map<string, string>();
  for (const line of out.trim().split("\n")) {
    const at = line.lastIndexOf(": eol: ");
    if (at > 0) m.set(line.slice(0, at), line.slice(at + 7).trim());
  }
  return m;
}

function collectFiles(dir: string, exts: Set<string>, acc: string[] = []): string[] {
  if (!existsSync(dir)) return acc;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) {
      if (["node_modules", "target", "dist"].includes(e.name)) continue;
      collectFiles(p, exts, acc);
    } else if (exts.has(path.extname(e.name))) {
      acc.push(p);
    }
  }
  return acc;
}

describe("行尾：checkout 策略必须与本机一致", () => {
  it(".gitattributes 存在，且把默认行尾钉成 LF", () => {
    const p = path.join(REPO_ROOT, ".gitattributes");
    expect(
      existsSync(p),
      "缺少 .gitattributes —— CI 的 windows runner 默认 core.autocrlf=true，" +
        "会把源码全转成 CRLF，而 vite 加载 CRLF+shebang 的 .mjs 会抛 SyntaxError"
    ).toBe(true);
    expect(readFileSync(p, "utf8")).toMatch(/^\*\s+text=auto\s+eol=lf\s*$/m);
  });

  it(".gitattributes 给 Windows 脚本单列 CRLF（.bat/.cmd 对 LF 敏感）", () => {
    const ga = readFileSync(path.join(REPO_ROOT, ".gitattributes"), "utf8");
    for (const ext of CRLF_EXCEPTIONS) {
      expect(ga, `${ext} 没在 .gitattributes 里单列 CRLF`).toMatch(
        new RegExp(`\\*${ext.replace(".", "\\.")}\\s+text\\s+eol=crlf`)
      );
    }
  });

  // 核心：不是断言文件里写了什么，而是断言 git 真的会照做
  it("git check-attr 确认源码一律 lf（不是只看配置文件文本）", () => {
    const samples = [
      "src/main.ts",
      "src/styles.css",
      "tools/package.mjs",
      "tools/check-no-user-data.mjs",
      "tests/tools/package.test.ts",
      "tests/ui/setup.ts",
      "vitest.config.ts",
      "package.json",
    ].filter((f) => existsSync(path.join(REPO_ROOT, f)));

    expect(samples.length).toBeGreaterThan(5);
    const got = checkAttrEol(samples);
    for (const f of samples) {
      expect(got.get(f), `${f} 的 checkout 行尾`).toBe("lf");
    }
  });

  it("git check-attr 确认 Windows 脚本仍是 crlf", () => {
    const scripts = [
      "tools/orbcat-launch.vbs",
      "tools/install-shortcut.ps1",
      "tools/orbcat-launch.ps1",
    ].filter((f) => existsSync(path.join(REPO_ROOT, f)));

    if (scripts.length === 0) return; // 脚本挪走了就不强制这条
    const got = checkAttrEol(scripts);
    for (const f of scripts) {
      expect(got.get(f), `${f} 的 checkout 行尾`).toBe("crlf");
    }
  });

  it("带 shebang 的脚本文件没有 BOM", () => {
    // BOM 与行尾无关，但同样是 vite/Node 会炸的隐形字符
    const shebangFiles = ["tools", "src", "tests"]
      .flatMap((d) => collectFiles(path.join(REPO_ROOT, d), new Set([".mjs", ".ts", ".js"])))
      .filter((f) => {
        const b = readFileSync(f);
        return b[0] === 0x23 && b[1] === 0x21;
      });

    for (const f of shebangFiles) {
      const buf = readFileSync(f);
      expect(
        buf[0] === 0xef && buf[1] === 0xbb && buf[2] === 0xbf,
        `${path.relative(REPO_ROOT, f)} 带 UTF-8 BOM`
      ).toBe(false);
    }
  });
});