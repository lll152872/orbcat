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
import { fileURLToPath, pathToFileURL } from "node:url";

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

/**
 * 内置运行时的目录名 —— 与 `pi::runtime_dir()` 逐字对应。
 *
 * ⚠️ 改这里必须同步改 `src-tauri/src/pi.rs::runtime_dir()`。
 * 目录名写错 = pi 内核静默不可用（启动日志会打「未找到内置 runtime/」）。
 */
export const RUNTIME_SUBDIR = "runtime";

/** 目录存在？ */
function isDir(p) {
  try {
    return statSync(p).isDirectory();
  } catch {
    return false;
  }
}

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
 * 数据目录名 —— 与 `resolve_data_dir()` 的发布版分支逐字对应。
 *
 *   // src-tauri/src/lib.rs
 *   let p = dir.join("agent-data");
 *
 * 运行时 exe 旁边找不到这个目录就**就地创建**（release），
 * 所以 zip 里带不带都能跑；带是为了解压即有完整骨架、不用再点初始化。
 */
export const DATA_SUBDIR = "agent-data";

/**
 * 把**空骨架**铺进 `<out>/agent-data/`。
 *
 * ## 为什么只铺 skeleton/，绝不铺仓库根的 agent-data/
 *
 * zip 是三合一分发（exe + dist/ + agent-data/）。空骨架 = 编译期固定内容
 * （memory/pending/projects/skills 的 README、内置技能），
 * `src-tauri/skeleton/` 是它的唯一来源，build.rs 的白名单闸门管着。
 *
 * 而仓库根的 `agent-data/` 是**运行时数据**：API key、会话、截图、日记。
 * 把它打进去等于把作者的凭据公开发布。
 * `tools/check-no-user-data.mjs` 的 d) 项就是拦这件事的 ——
 * 它认「源路径含 skeleton」，所以这里刻意用含 skeleton 的形参名。
 *
 * ## 刻意不放 models.json
 *
 * 用户填了 key 的 models.json 在 `<data_dir>/models.json`。
 * zip 里放一个同名文件的话，**更新时覆盖解压会把用户的 key 冲掉**。
 * 不放 → 首次启动由 `bootstrap_data` 建空数组，用户自己加模型。
 */
export function stageSkeleton(skeletonDir, outDir) {
  if (!existsSync(skeletonDir)) {
    throw new Error(`骨架目录不存在：${skeletonDir}`);
  }
  copyTree(skeletonDir, path.join(outDir, DATA_SUBDIR));
}

/**
 * 装配 Windows 发布形态：<out>/<exe> + <out>/dist/** + <out>/agent-data/**。
 *
 * 刻意**不**提供「把 dist 内容平铺进 out/」的选项 —— 那正是事故的成因，
 * 留着开关等于留着同一个坑。
 */
export function stageWindowsRelease({
  exePath,
  distDir,
  outDir,
  skeletonDir = null,
  files = [],
  runtimeDir = null,
}) {
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

  // ── 内置运行时（node / python / git+bash / pi 内核）──
  //
  // 位置契约：`pi::runtime_dir(exe)` 找的是 **`<exe 同目录>/runtime/`**。
  // ⚠️ 目录名写错 = pi 内核静默不可用（启动日志会打「未找到内置 runtime/」）。
  //
  // **可选**：runtime/ 不存在时不报错 —— 开发态（cargo build）本来就没有，
  // 那时 pi 内核不可用，orbcat 继续用自研内核（AppState 里 pi 字段是 None）。
  if (runtimeDir) {
    if (isDir(runtimeDir)) {
      copyTree(runtimeDir, path.join(outDir, RUNTIME_SUBDIR));
    } else {
      console.warn(
        `[package] ⚠️ runtime 目录不存在，跳过内置运行时：${runtimeDir}\n` +
          `          （pi 内核将不可用 —— 构建它：node tools/build-runtime-assets.mjs）`
      );
    }
  }

  // 三合一：空骨架也进 zip（不含 models.json，见 stageSkeleton 的说明）
  if (skeletonDir) {
    stageSkeleton(skeletonDir, outDir);
  }

  for (const f of files) {
    if (!isFile(f)) throw new Error(`附加文件不存在：${f}`);
    copyFileSync(f, path.join(outDir, path.basename(f)));
  }

  return { outDir, exeName };
}

/**
 * 校验产物布局满足运行时的两条契约：
 *   · `compute_dist_dir()`  —— `<exe 旁>/dist/index.html`
 *   · `resolve_data_dir()`  —— `<exe 旁>/agent-data/`（三合一分发）
 *
 * 失败时**必须抛异常**，不能只返回 false —— 上游是 CI 步骤和本地发布脚本，
 * 返回 false 只会变成又一个被忽略的返回值。事故的直接原因就是
 * 「打错了但没人报错」：CI 在 job 绿着的情况下把坏包发了出去。
 *
 * 错误信息区分三种成因，因为它们要修的地方完全不同：
 * 平铺（改打包脚本）、缺失（查 dist 是否构建）、放错层（查 .app 结构）。
 *
 * ⚠️ `agent-data` 缺失**不**算失败：release 下 `resolve_data_dir()` 会就地
 *    创建，界面也有「建出目录结构」按钮。它只是提示解压体验差一点
 *    （首次要自己点一下），而不是发布阻断项。
 *    但带了对的内容也不校验 —— 见 `verifyNoUserData`，那个才是硬闸。
 */
export function verifyReleaseLayout(dir, { exeName = null, wantSkeleton = false } = {}) {
  const errors = [];
  const notes = [];

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

  // ---- 三合一的第二半：agent-data ----------------------------------------
  // `wantSkeleton` 由调用方传入：装配时传了 --skeleton 就没理由再提示
  // 「zip 里没有 agent-data」，否则会出现"明明带了却说没带"的自相矛盾输出。
  const dataDir = path.join(dir, DATA_SUBDIR);
  if (!existsSync(dataDir)) {
    if (!wantSkeleton) {
      notes.push(
        `zip 里没有 ${DATA_SUBDIR}/：解压后首次启动会在 exe 旁创建，` +
          `用户要自己点一次「建出目录结构」。` +
          `（想让解压即用就在装配时加 --skeleton src-tauri/skeleton）`
      );
    }
  } else if (!readdirSync(dataDir).some((f) => f === "memory")) {
    notes.push(
      `${DATA_SUBDIR}/ 存在但里面没有 memory/：骨架不完整，` +
        `启动后仍会走一遍 bootstrap 补缺项。`
    );
  }

  if (errors.length > 0) {
    throw new Error(`发布产物布局校验失败：\n  - ${errors.join("\n  - ")}`);
  }

  return { distDir: path.join(dir, DIST_SUBDIR), dataDir, ok: true, notes };
}

/**
 * **硬闸**：确认 zip 里的 agent-data 只是空骨架，没有夹带用户数据。
 *
 * 为什么单独一个函数而不并进 `verifyReleaseLayout`：那条的失败是「产物坏了」
 * （发布阻断），这条的失败是「泄露了」（安全事故）。两者的严重程度和
 * 处理方式不同，混在一起会让"缺个目录"和"密钥外泄"看起来一样重。
 *
 * 拦的东西：
 *   · `models.json` —— 里面是 API key。zip 里放它 = 覆盖解压时冲掉用户的 key，
 *     公开发布时更是直接把作者的凭据交出去。
 *   · `sessions/`、`pmem.sqlite` —— 会话历史与长期记忆，纯用户数据。
 *   · `screenshots/`、`images/`、`diag.log` —— 同理。
 */
export function verifyNoUserData(dir) {
  const dataDir = path.join(dir, DATA_SUBDIR);
  if (!existsSync(dataDir)) return { ok: true, checked: false };

  const forbidden = [
    "models.json",
    "sessions",
    "pmem.sqlite",
    "diag.log",
    "screenshots",
    "images",
    ".trash",
    ".skeleton-state.json",
  ];
  const found = forbidden.filter((f) => existsSync(path.join(dataDir, f)));
  if (found.length > 0) {
    throw new Error(
      `产物 ${DATA_SUBDIR}/ 里夹带了用户数据：${found.join(", ")}\n` +
        `        zip 只该带 src-tauri/skeleton 的空骨架。\n` +
        `        models.json 尤其不能进 —— 它含 API key，且覆盖解压会冲掉用户自己那份。`
    );
  }
  return { ok: true, checked: true };
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

/**
 * 内置运行时的默认位置：仓库根的 `runtime/`。
 *
 * 找不到就返回 null —— `stageWindowsRelease` 会打警告并跳过
 * （开发态本来就没有 runtime/，那不是错误）。
 */
export function defaultRuntimeDir() {
  const p = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "runtime");
  return isDir(p) ? p : null;
}

function main() {
  const args = parseArgs(process.argv.slice(2));

  if (args.verify) {
    const r = verifyReleaseLayout(args.verify, { exeName: args["exe-name"] ?? null });
    verifyNoUserData(args.verify);
    console.log(`OK: 产物布局合法 ${path.join(args.verify, DIST_SUBDIR, DIST_ENTRY)}`);
    for (const n of r.notes ?? []) console.log(`  提示: ${n}`);
    return;
  }

  if (!args.exe || !args.dist || !args.out) {
    console.error(
      "用法：\n" +
        "  node tools/package.mjs --exe <exe> --dist <dist> --out <dir>\n" +
        "                         [--skeleton src-tauri/skeleton] [--file <f> ...]\n" +
        "                         [--runtime <dir>]\n" +
        "  node tools/package.mjs --verify <dir> [--exe-name <name>]\n" +
        "\n" +
        "  --runtime   内置运行时目录（node / python / git+bash / pi 内核），\n" +
        "              拷成 <out>/runtime/。缺省找仓库根的 runtime/。\n" +
        "  --skeleton  把空骨架铺进 <out>/agent-data/（三合一分发）。\n" +
        "              只能指向 src-tauri/skeleton —— 铺真实 agent-data/ 会被\n" +
        "              verifyNoUserData 拦下，那等于把 API key 打进发布包。"
    );
    process.exit(2);
  }

  const { exeName } = stageWindowsRelease({
    exePath: args.exe,
    distDir: args.dist,
    outDir: args.out,
    skeletonDir: args.skeleton ?? null,
    files: args.file ?? [],
    // 内置运行时（node / python / git+bash / pi 内核）——
    // 缺省就找仓库根的 runtime/（`node tools/build-runtime-assets.mjs` 的产物）。
    runtimeDir: args.runtime ?? defaultRuntimeDir(),
  });
  const { notes } = verifyReleaseLayout(args.out, {
    exeName,
    wantSkeleton: !!args.skeleton,
  });
  verifyNoUserData(args.out);
  console.log(
    `OK: 已装配并自检通过 ${args.out}（${exeName} + ${DIST_SUBDIR}/` +
      `${args.skeleton ? ` + ${DATA_SUBDIR}/` : ""}）`
  );
  for (const n of notes ?? []) console.log(`  提示: ${n}`);
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