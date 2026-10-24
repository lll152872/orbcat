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
//                            [--cli-exe <orbcat-perm.exe>]
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

/**
 * 内置运行时里**要进包**的组件（2026-10-23 用户拍板）。
 *
 * 分发包 = `runtime/{node,pi}`：node 是 PTC run_code + pi 的硬依赖，
 * pi 是内核本体。**不含 git**（GPLv2 不再分发）、**不含 python**
 * （不是硬依赖，模型可能调而已，可让用户在设置里指定路径）。
 *
 * ⚠️ 这里是**白名单**，不是黑名单：runtime/ 里就算还留着上一轮的
 * `git/`、`python/`，也不会被带进发布包 —— 决策改了，包必须跟着变。
 *
 * ⚠️ 改这里要同步看 `src-tauri/src/pi.rs::bundled_*`：
 * 那些函数全部返回 `Option<PathBuf>`，组件缺席就是 None ⇒ 回落系统那份。
 */
export const RUNTIME_COMPONENTS = ["node", "pi"];

/**
 * pi 的图片处理库 **photon** 在 runtime/pi/ 里的**确切位置**（相对 runtime/）。
 *
 * ## 为什么它是「必查件」
 *
 * pi 打包后的 bundle 里，`resizeImage` 走 `loadPhoton()`：
 * `await import("@silvia-odwyer/photon-node")`（见
 * `runtime/pi/dist/bundle/chunks/chunk-*.js`），而这个包里的 wasm
 * （`photon_rs_bg.wasm`）才是真正干活的图像编解码器。
 *
 * **缺了它**：pi 加载图片失败 ⇒ **用户带图发消息，模型只收到文字，图直接不见**，
 * 而启动日志与构建日志**一切正常** —— 典型的「构建绿、线上坏」。
 * 这就是 2026-10-23 Chief 复查时点名的硬伤。
 *
 * ## ⚠️ 路径是 **scoped 包名**，最容易看错
 *
 * 它在 `node_modules/@silvia-odwyer/photon-node/`（**带 scope 目录**），
 * **不是** `node_modules/photon-node/`。本轮就有人按后者去查，
 * 得出「包里没有 photon」的**误判**（实际 7 个条目都在包里）。
 * ⇒ 下面的报错信息把**完整相对路径**写出来，就是为了让下一次不用再猜。
 */
export const PHOTON_REL = [
  "pi",
  "node_modules",
  "@silvia-odwyer",
  "photon-node",
  "photon_rs_bg.wasm",
];

/** photon wasm 的绝对路径（相对 runtime/ 根） */
export function photonWasmPath(runtimeRoot) {
  return path.join(runtimeRoot, ...PHOTON_REL);
}

/**
 * 外部代批 CLI 的**部署目录**（相对包根）。
 *
 * ⚠️ 位置契约：`perm_cli::is_under_bin_dir()` 认的正是 `<包根>/bin/`，
 *    而 CLI 自己被内置在**包根的 `bin/` 下面**（与 `dist/`、`runtime/` 同级）。
 *
 * ⚠️ **为什么不放进 `runtime/`**：`runtime/` 是**白名单分发**目录
 *    （`RUNTIME_COMPONENTS = ["node", "pi"]`）⇒ 放进去就得改白名单，
 *    而 runtime/ 的语义是「内置运行时（node / pi 内核）」，
 *    塞一个权限 CLI 进去会污染那个契约。
 */
export const CLI_SUBDIR = "bin";

/**
 * 外部代批 CLI 的**文件名**（含扩展名）。
 *
 * ⚠️ 必须精确是 `orbcat-perm.exe`（**带连字符**）：
 *    层 1 阻断按文件名匹配（`perm_cli::CLI_STEM = "orbcat-perm"`，
 *    忽略 .exe/.cmd/.bat/.ps1/.com 后缀）⇒ 叫 `orbcat_perm.exe` 就挡不住。
 *    Cargo 那边靠 `src-tauri/Cargo.toml` 的显式 `[[bin]] name = "orbcat-perm"`
 *    保证这个名字（自动发现会叫成 `orbcat_perm.exe`）。
 */
export const CLI_BIN_NAME = "orbcat-perm.exe";

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
 * 装配 Windows 发布形态：<out>/<exe> + <out>/dist/** + <out>/agent-data/**
 *                                  + <out>/bin/orbcat-perm.exe（外部代批 CLI）
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
  initDoc = null,
  cliExe = null,
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

  // ── 外部代批 CLI：**先验**（存在 + 文件名精确）──
  //
  // ⚠️ 校验必须在 `rmSync(outDir)` **之前** ——
  //    放在后面的话，一次失败的装配会留下一个「看着挺像样」的半成品目录
  //    （exe 已经拷进去了）⇒ 被人当成有效产物就麻烦了。
  //    这跟上面 dist 入口那条是同一口径。
  //
  // ⚠️ **文件名必须精确是 `orbcat-perm.exe`（带连字符）**：
  //    层 1 阻断按**文件名**匹配（`perm_cli::CLI_STEM = "orbcat-perm"`，
  //    忽略 .exe/.cmd/.bat/.ps1/.com 后缀）⇒ 名字对不上 = 阻断规则在真机上失效
  //    （内部 AI 引用 `orbcat-perm.exe` 扫不到，因为它在找 `orbcat_perm.exe`）。
  if (cliExe) {
    if (!isFile(cliExe)) {
      throw new Error(`外部代批 CLI 不存在：${cliExe}`);
    }
    const base = path.basename(cliExe);
    if (base !== CLI_BIN_NAME) {
      throw new Error(
        `外部代批 CLI 的文件名不对：${base}\n` +
          `  必须精确是 ${CLI_BIN_NAME}（**带连字符**）—— 层 1 阻断按文件名匹配\n` +
          `  （perm_cli::CLI_STEM = "orbcat-perm"）⇒ 名字对不上 = 阻断规则形同虚设。\n` +
          `  修法：src-tauri/Cargo.toml 里显式写 [[bin]] name = "orbcat-perm"。`
      );
    }
  }

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  const exeName = path.basename(exePath);
  copyFileSync(exePath, path.join(outDir, exeName));

  // ⚠️ 关键一行：dist 是 **子目录**。这里写错就是 2026-10-12 那个事故。
  copyTree(distDir, path.join(outDir, DIST_SUBDIR));

  // ── 内置运行时（node / pi 内核）──
  //
  // 位置契约：`pi::runtime_dir(exe)` 找的是 **`<exe 同目录>/runtime/`**。
  // ⚠️ 目录名写错 = pi 内核静默不可用（启动日志会打「未找到内置 runtime/」）。
  //
  // ⚠️ **runtime 目录不存在 = 当场抛错，不许只 warn 就放过**（审计 F1）。
  // 旧版只 `console.warn` 然后继续 ⇒ 装配出一个**没有内核**的包，
  // CI job 还是绿的，坏包就这么发出去了（pi 内核静默失效）。
  // 「打错了但没人报错」正是 2026-10-12 那次事故的直接成因。
  //
  // 例外：`runtimeDir` 传 null = 调用方**明确不要**内置运行时
  // （开发态 cargo build、单测）。那种情况跳过本段。
  if (runtimeDir) {
    if (!isDir(runtimeDir)) {
      throw new Error(
        `runtime 目录不存在：${runtimeDir}\n` +
          `  发布包必须带内置运行时（${RUNTIME_COMPONENTS.join(" + ")}）——` +
          `缺了 pi 内核在用户机器上静默失效。\n` +
          `  修法：node tools/build-runtime-assets.mjs（先把源放进 tools/.cache/）`
      );
    }
    // 白名单拷贝：runtime/ 里残留的 git/、python/ 不会被带进包里
    for (const comp of RUNTIME_COMPONENTS) {
      const from = path.join(runtimeDir, comp);
      if (!isDir(from)) {
        throw new Error(
          `runtime 里缺 ${comp}/：${from}\n` +
            `  必查组件是 ${RUNTIME_COMPONENTS.join(" + ")} —— 缺一个这个包就不该发出去。\n` +
            `  修法：node tools/build-runtime-assets.mjs`
        );
      }
      copyTree(from, path.join(outDir, RUNTIME_SUBDIR, comp));
    }
    const skipped = readdirSync(runtimeDir).filter(
      (f) => isDir(path.join(runtimeDir, f)) && !RUNTIME_COMPONENTS.includes(f)
    );
    if (skipped.length > 0) {
      console.log(
        `[package] runtime 里还有 ${skipped.join(", ")} —— 本轮不分发，未进包`
      );
    }

    // ── photon（pi 的图片处理库）必查 ──
    //
    // ⚠️ 这条是 2026-10-23 Chief 点名的硬伤：**缺 photon ⇒ 用户带图发消息，
    //    模型只收到文字，图直接不见**，而构建日志一切正常。
    // ⇒ 缺了当场抛错，绝不只 warn（warn = 又一次「构建绿、线上坏」）。
    const photon = photonWasmPath(path.join(outDir, RUNTIME_SUBDIR));
    if (!isFile(photon)) {
      throw new Error(
        `runtime 里缺 photon（pi 的图片处理库）：${photon}\n` +
          `  期望的**完整相对路径**是 runtime/${PHOTON_REL.join("/")}\n` +
          `  ⚠️ 它是 **scoped 包名**（@silvia-odwyer/photon-node），\n` +
          `     不是 node_modules/photon-node/ —— 别查错路径下结论。\n` +
          `  缺它的后果：用户带图发消息，模型只收到文字，图直接不见（构建日志仍全绿）。\n` +
          `  修法：node tools/build-runtime-assets.mjs（源放在 tools/.cache/photon-pkg/）`
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

  // ── 外部代批 CLI（orbcat-perm）──
  //
  // ⚠️ 名字/存在性在函数开头已验过（见上），这里只管拷。
  // ⚠️ 位置是**包根的 bin/**（与 dist/、runtime/ 同级），**不进 runtime/**：
  //    runtime/ 是白名单分发目录（RUNTIME_COMPONENTS = node + pi）。
  //
  // `cliExe` 传 null = 调用方**明确不要**这个组件（开发态 / 单测）；
  // 发布包走 main() 的路径 ⇒ 那条路**强制要求**它存在。
  if (cliExe) {
    mkdirSync(path.join(outDir, CLI_SUBDIR), { recursive: true });
    copyFileSync(cliExe, path.join(outDir, CLI_SUBDIR, CLI_BIN_NAME));
  }

  // ── 初始化指引（**包根**，与 agent-data/ 同级）──
  //
  // ⚠️ 位置不是随便定的：
  //   · prompt 的原话是「读**项目根**的 `初始化.md`（和 `agent-data/` 同级）」；
  //   · 启动器 `tools/orbcat-launch.ps1` 用 `-WorkingDirectory <exe 目录>` 起进程
  //     ⇒ 运行时 cwd = 包根 ⇒ agent 按相对路径就找得到。
  //
  // 放进 `agent-data/` 里 agent 是**找不到**的（那里不是 cwd）⇒ 初始化链路断链。
  if (initDoc) {
    if (!isFile(initDoc)) {
      throw new Error(`初始化指引不存在：${initDoc}`);
    }
    copyFileSync(initDoc, path.join(outDir, path.basename(initDoc)));
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
export function verifyReleaseLayout(
  dir,
  { exeName = null, wantSkeleton = false, wantRuntime = false, wantCli = false } = {}
) {
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

  // ---- 内置运行时：node + pi 必查，python / git 不查 --------------------
  //
  // `wantRuntime` 由调用方传入：装配/验发布包时传 true，
  // 单测与开发态校验（不想带 runtime）不传 ⇒ 不查。
  //
  // ⚠️ 契约跟着 2026-10-23 的分发决策走：
  //   必查 = `runtime/node/node.exe` + `runtime/pi/`
  //   不查 = `runtime/python/`、`runtime/git/`（不是硬依赖，缺席即回落系统）
  if (wantRuntime) {
    const rt = path.join(dir, RUNTIME_SUBDIR);
    if (!isDir(rt)) {
      errors.push(
        `产物里没有 ${RUNTIME_SUBDIR}/：${rt} 不存在。\n` +
          `        发布包必须内置运行时（${RUNTIME_COMPONENTS.join(" + ")}），` +
          `否则用户机器上 pi 内核静默失效。\n` +
          `        修法：node tools/build-runtime-assets.mjs，再重新装配。`
      );
    } else {
      if (!isFile(path.join(rt, "node", "node.exe"))) {
        errors.push(
          `${RUNTIME_SUBDIR}/node/node.exe 不存在：node 是硬依赖（PTC run_code + pi 都是 node 路线）。`
        );
      }
      if (!isDir(path.join(rt, "pi"))) {
        errors.push(
          `${RUNTIME_SUBDIR}/pi/ 不存在：pi 是内核本体，缺了 pi 内核直接不可用。`
        );
      }
      // photon（pi 的图片处理库）—— 必查
      //
      // ⚠️ 缺它的后果不是「功能差一点」，而是**用户带图发消息，模型只收到文字、
      //    图直接不见**，而构建日志与启动日志一切正常（2026-10-23 Chief 点名的硬伤）。
      // ⇒ 验包阶段就必须红，不许放过。
      const photon = photonWasmPath(rt);
      if (!isFile(photon)) {
        errors.push(
          `${RUNTIME_SUBDIR}/ 里缺 photon（pi 的图片处理库）：${photon}\n` +
            `        期望的**完整相对路径**是 ${RUNTIME_SUBDIR}/${PHOTON_REL.join("/")}\n` +
            `        ⚠️ 它是 **scoped 包名**（@silvia-odwyer/photon-node），\n` +
            `           不是 node_modules/photon-node/ —— 别查错路径下结论。\n` +
            `        后果：用户带图发消息，模型只收到文字，图直接不见。\n` +
            `        修法：node tools/build-runtime-assets.mjs，再重新装配。`
        );
      }
      // 不查的两个：缺席只提示，不算缺件
      for (const opt of ["python", "git"]) {
        if (!isDir(path.join(rt, opt))) {
          notes.push(
            `${RUNTIME_SUBDIR}/${opt}/ 不在包里 —— 这是**预期**（2026-10-23 决定不分发）：` +
              (opt === "python"
                ? `未检测到内置 Python，可在设置里指定路径或自行安装。`
                : `未分发内置 Git（GPLv2），将使用系统 Git。`)
          );
        }
      }
    }
  }

  // ---- 外部代批 CLI：<包根>/bin/orbcat-perm.exe 必查 -------------------
  //
  // `wantCli` 由调用方传入：验发布包时传 true，单测 / 开发态不传。
  //
  // ⚠️ 缺它的后果不是「功能差一点」，而是**外部 AI 代批这条链路整条断掉**
  //    （用户侧 Codex / Claude Code 拿不到 CLI）。
  //    且**当场红**，不许放过 —— 跟 runtime / photon 同一口径。
  if (wantCli) {
    const cli = path.join(dir, CLI_SUBDIR, CLI_BIN_NAME);
    if (!isFile(cli)) {
      errors.push(
        `产物里没有 ${CLI_SUBDIR}/${CLI_BIN_NAME}：${cli} 不存在。\n` +
          `        发布包必须带外部代批 CLI（用户侧外部 AI 靠它代批权限申请）。\n` +
          `        修法：cargo build --release --bin orbcat-perm（产物名必须带连字符），\n` +
          `              再用 --cli-exe <路径> 传给装配脚本。`
      );
    }
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
 * 初始化指引（`初始化.md`）的默认位置 —— **仓库根**。
 *
 * ## 为什么它必须进「包根」，不是 `agent-data/` 下面
 *
 * 1. prompt 的原话（`src-tauri/src/agent.rs` 的「初始化未完成」段）：
 *    「读**项目根**的 `初始化.md`（和 `agent-data/` 同级）」——
 *    「同级」= 包根，与 `agent-data/` 并列。
 * 2. 启动器 `tools/orbcat-launch.ps1` 用 `-WorkingDirectory <exe 目录>` 起进程
 *    ⇒ 运行时 cwd = 包根 ⇒ agent 按相对路径就能找到它。
 *
 * 找不到返回 null —— **调用方必须自己决定要不要报错**：
 * 缺它 ⇒ 新用户点完「建出目录结构」后，agent 被引导去读一个不存在的文件
 * ⇒ 初始化链路直接断链 ⇒ 发布包不能这么发出去。
 */
export function defaultInitDoc() {
  const p = path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    "..",
    "初始化.md"
  );
  return isFile(p) ? p : null;
}

/**
 * 内置运行时的默认位置：仓库根的 `runtime/`。
 *
 * 找不到就返回 null —— **调用方必须自己决定要不要报错**：
 * 开发态（cargo build、单测）可以没有 runtime，
 * 但**发布包必须有**（审计 F1：缺 runtime 的包 = pi 内核静默失效）。
 */
export function defaultRuntimeDir() {
  const p = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "runtime");
  return isDir(p) ? p : null;
}

/**
 * 外部代批 CLI 的默认位置：cargo 的产物目录。
 *
 * 找不到就返回 null —— **调用方必须自己决定要不要报错**：
 * 开发态 / 单测可以不带它，但**发布包必须有**
 * （缺它 = 用户侧外部 AI 拿不到「代批权限申请」这个能力，链路整条断掉）。
 */
export function defaultCliExe() {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const candidates = [
    path.join(root, "src-tauri", "target", "release", CLI_BIN_NAME),
    path.join(root, "src-tauri", "target", "release-fast", CLI_BIN_NAME),
  ];
  for (const c of candidates) if (isFile(c)) return c;
  return null;
}

function main() {
  const args = parseArgs(process.argv.slice(2));

  if (args.verify) {
    // 验发布包 = 必须有内置运行时（node + pi）+ 外部代批 CLI —— 与装配时同一套契约
    const r = verifyReleaseLayout(args.verify, {
      exeName: args["exe-name"] ?? null,
      wantRuntime: true,
      wantCli: true,
    });
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
        "                         [--runtime <dir>] [--cli-exe <orbcat-perm.exe>]\n" +
        "  node tools/package.mjs --verify <dir> [--exe-name <name>]\n" +
        "\n" +
        "  --cli-exe   外部代批 CLI 的路径（产物名必须精确是\n" +
        "              orbcat-perm.exe，带连字符）。拷成 <out>/bin/orbcat-perm.exe。\n" +
        "              缺省找 src-tauri/target/{release,release-fast}/。\n" +
        "              **发布包必须有**：缺了它，外部 AI 代批链路整条断掉。\n" +
        "  --runtime   内置运行时目录（node / pi 内核），\n" +
        "              拷成 <out>/runtime/。缺省找仓库根的 runtime/。\n" +
        "              **发布包必须有**：找不到会当场报错，不许发出坏包\n" +
        "              （审计 F1 —— 缺 runtime 的包 = pi 内核静默失效）。\n" +
        "  --skeleton  把空骨架铺进 <out>/agent-data/（三合一分发）。\n" +
        "              只能指向 src-tauri/skeleton —— 铺真实 agent-data/ 会被\n" +
        "              verifyNoUserData 拦下，那等于把 API key 打进发布包。"
    );
    process.exit(2);
  }

  // 内置运行时（node / pi 内核）——
  // 缺省找仓库根的 runtime/（`node tools/build-runtime-assets.mjs` 的产物）。
  //
  // ⚠️ **找不到 = 当场报错**（审计 F1）。旧版是「warn 一下跳过」，
  // 于是 CI 装配出一个没有内核的包，job 还是绿的，坏包就发出去了。
  const runtimeDir = args.runtime ?? defaultRuntimeDir();
  if (!runtimeDir || !isDir(runtimeDir)) {
    console.error(
      `找不到内置运行时：${args.runtime ?? "仓库根 runtime/"}\n` +
        `  发布包必须带 runtime/{${RUNTIME_COMPONENTS.join(",")}} —— 缺了 pi 内核在用户机器上静默失效。\n` +
        `  修法：node tools/build-runtime-assets.mjs（先把 node.exe 与 pi 包放进 tools/.cache/）`
    );
    process.exit(2);
  }

  // ── 初始化指引（包根，与 agent-data/ 同级）──
  //
  // ⚠️ **必须进包根**，不能塞进 `agent-data/`：prompt 让 agent 读的是
  // 「项目根的 `初始化.md`（和 `agent-data/` 同级）」，而启动器把 cwd 设成
  // exe 目录（`tools/orbcat-launch.ps1` 的 `-WorkingDirectory`）
  // ⇒ 只有包根那份找得到。
  //
  // 缺它 ⇒ 新用户初始化链路断链（agent 去读一个不存在的文件）
  // ⇒ **当场报错**，不许 warn 就放过（跟 runtime / photon 同一口径）。
  const initDoc = defaultInitDoc();
  if (!initDoc) {
    console.error(
      "找不到初始化指引（仓库根的 初始化.md）\n" +
        "  发布包必须带它（**包根**，与 agent-data/ 同级）—— 缺了它，新用户\n" +
        "  点完「建出目录结构」后，agent 被 prompt 引导去读一个不存在的文件\n" +
        "  ⇒ 初始化链路直接断链。\n" +
        "  修法：确认仓库根有 初始化.md（它是入库文件，不是生成物）。"
    );
    process.exit(2);
  }

  // ── 外部代批 CLI（orbcat-perm）——
  //
  // ⚠️ **找不到 = 当场报错**，跟 runtime 同一口径：
  //    缺它 ⇒ 发布包里没有 bin/orbcat-perm.exe ⇒ 用户侧外部 AI（Codex / Claude Code）
  //    根本拿不到「代批权限申请」这个能力 ⇒ 链路整条断掉，而构建日志一切正常。
  const cliExe = args["cli-exe"] ?? defaultCliExe();
  if (!cliExe || !isFile(cliExe)) {
    console.error(
      `找不到外部代批 CLI：${args["cli-exe"] ?? `src-tauri/target/{release,release-fast}/${CLI_BIN_NAME}`}\n` +
        `  发布包必须带 ${CLI_SUBDIR}/${CLI_BIN_NAME} —— 缺了它，用户侧外部 AI 拿不到代批能力。\n` +
        `  修法：cargo build --release --bin orbcat-perm（产物名必须带连字符）`
    );
    process.exit(2);
  }

  const { exeName } = stageWindowsRelease({
    exePath: args.exe,
    distDir: args.dist,
    outDir: args.out,
    skeletonDir: args.skeleton ?? null,
    files: args.file ?? [],
    runtimeDir,
    initDoc,
    cliExe,
  });
  const { notes } = verifyReleaseLayout(args.out, {
    exeName,
    wantSkeleton: !!args.skeleton,
    wantRuntime: true,
    wantCli: true,
  });
  verifyNoUserData(args.out);
  console.log(
    `OK: 已装配并自检通过 ${args.out}（${exeName} + ${DIST_SUBDIR}/` +
      `${args.skeleton ? ` + ${DATA_SUBDIR}/` : ""} + ${CLI_SUBDIR}/${CLI_BIN_NAME}）`
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