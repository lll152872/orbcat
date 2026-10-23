#!/usr/bin/env node
/**
 * 断言 runtime/ 真的在，而且结构对。
 *
 * 为什么单独一个脚本（而不是 action.yml 里写两行 if）：
 * ① **同一个判据要在多处用** —— CI 装配前、CI 跑集成测试前、本地出包前，
 *    判据必须逐字一致，否则三处就会各判各的；
 * ② **这是「防假绿」的闸**：pi 集成测试找不到 runtime/ 时是**静默跳过**
 *    （`src-tauri/tests/pi_*.rs` 的 `runtime_dir()` 返回 None 就 return），
 *    没有这道断言，CI 会「全部通过」而一条 e2e 都没跑；
 * ③ **判据要跟实际结构对齐**，不能凭想象：
 *    runtime/node/node.exe —— PTC run_code 与 pi 都是 node 路线，硬依赖
 *    runtime/pi/dist/bundle/cli.js —— pi 内核本体（自包含 bundle）
 *    runtime/pi/node_modules/jiti —— 加载 runtime-ext/*.ts 扩展必需
 *    runtime/pi/extensions/*.ts —— orbcat 的工具桥 + 权限闸门，
 *      缺了 pi 就看不到 orbcat 的 34 个工具（build-runtime-assets.mjs 会拷）
 *
 * 退出码：0 = 齐；1 = 缺（CI 必须红）。
 */

import fs from "node:fs";
import path from "node:path";

const ROOT = process.cwd();
const RT = path.join(ROOT, "runtime");

/** 必查：文件 */
const NEED_FILES = [
  ["node/node.exe", "node —— PTC run_code 与 pi 的硬依赖"],
  ["pi/dist/bundle/cli.js", "pi 内核本体（自包含 bundle）"],
];

/**
 * 必查：目录（至少一个文件在里面）。
 *
 * ⚠️ `pi/node_modules/jiti` 原来被放在 NEED_FILES 里用 `isFile()` 判 ——
 * **但 jiti 是目录**（`runtime/pi/node_modules/jiti/`），`isFile()` 恒 false
 * ⇒ 断言恒判「缺 jiti」⇒ **CI 必红，哪怕 runtime 完全是好的**
 * （2026-10-23 守门实测：仓库 runtime/、D:\orbcat\runtime、wt-pi\runtime
 *   三份都有 jiti，断言却报缺 ⇒ 假阴性）。
 */
const NEED_DIRS = [
  ["pi/extensions", "orbcat 的工具桥 + 权限闸门（缺了 pi 看不到 orbcat 的工具）"],
  ["pi/node_modules/jiti", "加载 runtime-ext/*.ts 扩展必需（.ts 扩展靠它）"],
];

const problems = [];

for (const [rel, why] of NEED_FILES) {
  const p = path.join(RT, rel);
  if (!fs.existsSync(p) || !fs.statSync(p).isFile()) {
    problems.push(`缺文件 runtime/${rel} —— ${why}`);
  } else {
    const mb = (fs.statSync(p).size / 1024 / 1024).toFixed(1);
    console.log(`  ✓ runtime/${rel}  (${mb} MB)  ${why}`);
  }
}

for (const [rel, why] of NEED_DIRS) {
  const p = path.join(RT, rel);
  const entries = fs.existsSync(p) && fs.statSync(p).isDirectory() ? fs.readdirSync(p) : [];
  if (entries.length === 0) {
    problems.push(`缺目录 runtime/${rel}/（或里面是空的）—— ${why}`);
  } else {
    console.log(`  ✓ runtime/${rel}/  (${entries.length} 个文件：${entries.join(", ")})`);
  }
}

if (problems.length > 0) {
  console.error("");
  console.error("✗ runtime/ 不完整 —— 这会让 pi 集成测试**静默跳过**（= 假绿）：");
  for (const p of problems) console.error(`   · ${p}`);
  console.error("");
  console.error("   修法：node tools/build-runtime-assets.mjs（先把源放进 tools/.cache/）");
  console.error("         CI 上：.github/actions/setup-runtime 这个复合 action 会做这件事。");
  process.exit(1);
}

console.log("✓ runtime/ 完整（node + pi + jiti + extensions）");
