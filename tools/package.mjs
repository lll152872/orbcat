#!/usr/bin/env node
// ============================================================================
// 发布产物装配（Windows 形态） + 布局自检
// ============================================================================
//
// ## 这段代码存在的唯一原因
//
// 2026-10-12 真实事故：v1.0.0 的两个 Windows zip 发出去，**里面没有前端**。
// 用户解压看到的是「前端资源未就绪 / 请把 dist/ 目录放到 exe 同目录」。
//
// 机制不是「没打进去」，而是**打进去了但摊平了**。当时的 CI 写的是：
//
//     New-Item -ItemType Directory -Force -Path pkg | Out-Null
//     Copy-Item -Recurse -Force dist/* pkg/        # ← 内容平铺到 pkg/ 根
//
// 得到的是 pkg/index.html + pkg/assets/，而不是 pkg/dist/index.html。
// 运行时 `compute_dist_dir()`（src-tauri/src/disk_assets.rs）只认
// `<exe 同目录>/dist/index.html`，找不到就静默回退到编译期嵌的 stub-dist。
//
// ## 为什么不能只是「写对这一行」
//
// 同一个仓库里同时存在三份「怎么摆 dist」的约定：
//   · tools/build-release.sh      —— 正确的（mkdir -p "$REL/dist" 再拷）
//   · ci.yml 的 Windows 调试 job  —— 正确的（New-Item out/dist）
//   · ci.yml 的 Windows release job —— 错的（平铺）    ← 发给用户的就是这个
//
// 三份实现、两套答案、零交叉校验。所以真正的修法不是把第三份改对，而是
// **让 CI 走这一份**，再让测试锁住这一份的输出布局。
//
// ## 为什么是 Node 不是 PowerShell
//
// 同 tools/check-no-user-data.mjs：CI 的三个 runner 都装了 Node 22，
// 而 Windows runner 的 shell 语义（通配符展开、错误偏好、GHA 注入的
// `$ErrorActionPreference='stop'`）和 macOS runner 完全不一致，
// 同一个打包步骤写两遍必然漂移。
//
// ## 用法
//
//   node tools/package.mjs --exe <exe> --dist <dist> --out <dir> [--file <f> ...]
//   node tools/package.mjs --verify <dir> [--exe-name <name>]
// ============================================================================

import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  rmSync,
  statSync,
} from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

/**
 * 运行时硬契约 —— 与 `compute_dist_dir()` 的第 1 条规则逐字对应。
 *
 *   // src-tauri/src/disk_assets.rs
 *   let d = dir.join("dist");
 *   if d.join("index.html").is_file() { return Some(d); }
 *
 * 改这里必须同步改 Rust 那边，反之亦然。两侧各有一对测试
 * （tests/tools/package.test.ts 与 src-tauri 的 disk_assets 测试）。
 */
export const DIST_SUBDIR = "dist";
export const DIST_ENTRY = "index.html";

function isFile(p) {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
}

/**
 * 递归拷贝目录。
 *
 * ⚠️ 刻意**不用** `fs.cpSync(..., { recursive: true })`：在本机（Windows 11 +
 *    火绒）它会直接把 node 进程打死，退出码 0xC0000409 / -1073740791，
 *    而且是拷到一半才死（exe 已落盘、dist 没落），症状极像"偶尔失败"。
 *    单测因为只拷两个假文件而侥幸通过，是真实的端到端跑才暴露的。
 *    手写版本只用 readdirSync + copyFileSync，本机与 CI 行为一致。
 */
function copyTree(src, dest) {
  mkdirSync(dest, { recursive: true });
  for (const entry of readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, entry.name);
    const d = path.join(dest, entry.name);
    if (entry.isDirectory()) copyTree(s, d);
    else copyFileSync(s, d);
  }
}

/**
 * 装配 Windows 发布形态：<out>/<exe> + <out>/dist/**。
 *
 * 刻意**不**提供「把 dist 内容平铺进 out/」的选项 —— 那正是事故的成因，
 * 留着开关等于留着同一个坑。
 */
export function stageWindowsRelease({ exePath, distDir, outDir, files = [] }) {
  if (!isFile(exePath)) {
    throw new Error(`exe 不存在：${exePath}`);
  }
  if (!isFile(path.join(distDir, DIST_ENTRY))) {
    throw new Error(
      `dist 入口不存在：${path.join(distDir, DIST_ENTRY)}\n` +
        `（dist 目录本身在 .gitignore 里，CI 全新 checkout 必须先 npm run build）`
    );
  }

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  const exeName = path.basename(exePath);
  copyFileSync(exePath, path.join(outDir, exeName));

  // ⚠️ 关键一行：dist 是 **子目录**。这里写错就是 2026-10-12 那个事故。
  copyTree(distDir, path.join(outDir, DIST_SUBDIR));

  for (const f of files) {
    if (!isFile(f)) throw new Error(`附加文件不存在：${f}`);
    copyFileSync(f, path.join(outDir, path.basename(f)));
  }

  return { outDir, exeName };
}

/**
 * 校验产物布局满足 `compute_dist_dir()` 的契约。
 *
 * 失败时**必须抛异常**，不能只返回 false —— 上游是 CI 步骤和本地发布脚本，
 * 返回 false 只会变成又一个被忽略的返回值。事故的直接原因就是
 * 「打错了但没人报错」：CI 在 job 绿着的情况下把坏包发了出去。
 *
 * 错误信息区分三种成因，因为它们要修的地方完全不同：
 * 平铺（改打包脚本）、缺失（查 dist 是否构建）、放错层（查 .app 结构）。
 */
export function verifyReleaseLayout(dir, { exeName = null } = {}) {
  const errors = [];

  // exe 名不确定时：只要目录里有 *.exe / *.dmg 就算有 exe。
  const hasExe =
    exeName != null
      ? isFile(path.join(dir, exeName))
      : existsSync(dir) &&
        readdirSync(dir).some((f) => /\.(exe|dmg|app)$/i.test(f));
  if (!hasExe) {
    errors.push(`找不到可执行文件：${path.join(dir, exeName ?? "*.exe")}`);
  }

  const wantIndex = path.join(dir, DIST_SUBDIR, DIST_ENTRY);
  if (!isFile(wantIndex)) {
    const flattened = isFile(path.join(dir, DIST_ENTRY));
    const anyDist = existsSync(path.join(dir, DIST_SUBDIR));
    if (flattened) {
      errors.push(
        `dist 被**平铺**进了产物根目录：找到 ${path.join(dir, DIST_ENTRY)}，` +
          `但 compute_dist_dir() 要的是 ${wantIndex}。\n` +
          `        修法：dist 内容必须拷进 <out>/dist/ 子目录，不能拷进 <out>/ 根。`
      );
    } else if (anyDist) {
      errors.push(
        `${path.join(dir, DIST_SUBDIR)} 存在但里面没有 ${DIST_ENTRY}：` +
          `dist 目录是空的或结构不对。`
      );
    } else {
      errors.push(
        `产物里完全没有 dist：${wantIndex} 不存在。\n` +
          `        修法：先 npm run build 生成 dist/，再装配。`
      );
    }
  }

  if (errors.length > 0) {
    throw new Error(`发布产物布局校验失败：\n  - ${errors.join("\n  - ")}`);
  }

  return { distDir: path.join(dir, DIST_SUBDIR), ok: true };
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) {
      out._.push(a);
      continue;
    }
    const key = a.slice(2);
    if (key === "file") {
      (out.file ??= []).push(argv[++i]);
    } else {
      out[key] = argv[++i];
    }
  }
  return out;
}

function main() {
  const args = parseArgs(process.argv.slice(2));

  if (args.verify) {
    verifyReleaseLayout(args.verify, { exeName: args["exe-name"] ?? null });
    console.log(`OK: 产物布局合法 ${path.join(args.verify, DIST_SUBDIR, DIST_ENTRY)}`);
    return;
  }

  if (!args.exe || !args.dist || !args.out) {
    console.error(
      "用法：\n" +
        "  node tools/package.mjs --exe <exe> --dist <dist> --out <dir> [--file <f> ...]\n" +
        "  node tools/package.mjs --verify <dir> [--exe-name <name>]"
    );
    process.exit(2);
  }

  const { exeName } = stageWindowsRelease({
    exePath: args.exe,
    distDir: args.dist,
    outDir: args.out,
    files: args.file ?? [],
  });
  verifyReleaseLayout(args.out, { exeName });
  console.log(`OK: 已装配并自检通过 ${args.out}（${exeName} + ${DIST_SUBDIR}/）`);
}

// 直接运行本文件时才进 main()。
//
// ⚠️ 不要手拼 `file://${process.argv[1]}` —— Windows 上 import.meta.url 是
//    `file:///D:/...`（**三个**斜杠），手拼出来只有两个，永远不相等，
//    结果就是 CLI 静默什么都不干（2026-10-12 踩过：单测全绿，
//    因为它只当库 import，根本没走 main）。
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}