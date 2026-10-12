#!/usr/bin/env node
// ===========================================================================
// 防夹带闸门 · 仓库级
// ===========================================================================
//
// ## 它查什么
//
// build.rs 里那道闸门管的是「骨架区里有没有混进用户数据」—— 那是**编进二进制**
// 之前最后一道关。这道脚本查的是 build.rs **看不到**的东西：
//
//   a) git 有没有跟踪 agent-data/、密钥、调试符号
//   b) 全仓 include_str! / include_bytes! 的实参是不是都落在骨架区内
//   c) tauri.conf.json 的 bundle.resources 有没有把数据目录写进去
//   d) tools/build-release.sh 的拷贝目标里有没有数据目录
//
// ## 为什么用 Node 而不是 bash
//
// CI 有 windows-latest / macos-14 / macos-13 三个 runner（.github/workflows/ci.yml），
// bash 在 Windows runner 上不可用；写 bash + PowerShell 双份必然漂移。
// Node 22 在三个 runner 上都现成，且不新增任何依赖。
//
// ## 为什么还要这道 —— build.rs 不是已经拦了吗
//
// 因为 build.rs 只管「进二进制的那一份」。这个仓库里 `agent-data/` **物理躺在
// 仓库里**（开发期数据目录 = `env!("CARGO_MANIFEST_DIR")/../agent-data`），
// 它进包有三条路：git、打包配置、构建脚本里的 include_*。后两条 build.rs 能管，
// 第一条不能。
//
// 用法：
//   node tools/check-no-user-data.mjs
// 退出码非 0 = 有问题。

import { execFileSync } from 'node:child_process';
import { readFileSync, existsSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SKELETON = 'src-tauri' + sep + 'skeleton';
const DATA = 'agent-data';

const problems = [];
const notes = [];
const fail = (where, what, why) => problems.push({ where, what, why });

const ok = (s) => `\x1b[32m${s}\x1b[0m`;
const bad = (s) => `\x1b[31m${s}\x1b[0m`;
const dim = (s) => `\x1b[2m${s}\x1b[0m`;

function git(...args) {
  try {
    return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  } catch {
    return ''; // 不是 git 仓库 / 没装 git —— 跳过 git 类检查，不假装通过
  }
}

function walk(dir, out = []) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of entries) {
    if (e.name === 'node_modules' || e.name === 'target' || e.name === '.git' || e.name === 'dist') continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else out.push(p);
  }
  return out;
}

// ---------------------------------------------------------------------------
// a) git 有没有跟踪不该跟踪的东西
// ---------------------------------------------------------------------------
function checkGitTracking() {
  const raw = git('ls-files', '-z');
  if (!raw) {
    notes.push('跳过 a)：拿不到 git ls-files（不在 git 仓库里，或没装 git）');
    return;
  }
  const tracked = raw.split('\0').filter(Boolean).map((p) => p.split('/').join(sep));

  const DATA_RULES = [
    { re: /(^|\\)agent-data(\\|$)/, why: '运行时用户数据：记忆、画像、会话、API key、日记、截图' },
    { re: /\.(bak|old)(\.|$)/i, why: '备份残留 —— 里面往往是改了半截的配置或旧记忆' },
    { re: /\.pdb$/i, why: '调试符号，曾误入过工作树' },
    { re: /\.key$/i, why: '密钥' },
    { re: /(^|\\)\.trash(\\|$)/, why: '回收站残留' },
  ];

  for (const p of tracked) {
    for (const rule of DATA_RULES) {
      if (rule.re.test(p)) fail('a) git 跟踪', p, rule.why);
    }
  }
  console.log(`${ok('a) git 跟踪')}  ${dim(`${tracked.length} 个文件，${problems.length} 项可疑`)}`);
}

// ---------------------------------------------------------------------------
// b) include_str! / include_bytes! 的实参是否都落在骨架区内
// ---------------------------------------------------------------------------
const INCLUDE_RE = /include_(?:str|bytes)!\s*\(\s*"([^"]+)"\s*\)/g;

function checkIncludes() {
  const files = walk(ROOT).filter((f) => f.endsWith('.rs'));
  let found = 0;

  for (const f of files) {
    // build.rs 本身用 include! 拼 OUT_DIR 生成的清单，那是结构性的，跳过
    if (f.endsWith(join('src-tauri', 'build.rs'))) continue;
    const text = readFileSync(f, 'utf8');
    for (const m of text.matchAll(INCLUDE_RE)) {
      found++;
      const target = resolve(dirname(f), m[1].split('/').join(sep));
      const rel = relative(ROOT, target).split(sep).join('/');
      if (!rel.startsWith(SKELETON.split(sep).join('/') + '/')) {
        fail(
          'b) include_*',
          `${relative(ROOT, f).split(sep).join('/')} → ${m[1]}`,
          `编译期嵌入的文件不在骨架区 ${SKELETON}/ 内（实际解析到 ${rel}）。\n` +
            `        include_* 会把文件内容焊进二进制 —— 只有骨架区里的干净模板可以这么做。\n` +
            `        活数据要从磁盘读（用 data_dir 拼路径），绝不能编译进去。`
        );
      }
    }
  }
  console.log(`${ok('b) include_*')}  ${dim(`${files.length} 个 .rs，${found} 处嵌入，全部在骨架区内`)}`);
}

// ---------------------------------------------------------------------------
// c) tauri.conf.json 的 bundle 有没有把数据目录写进去
// ---------------------------------------------------------------------------
function checkTauriBundle() {
  const p = join(ROOT, 'src-tauri', 'tauri.conf.json');
  if (!existsSync(p)) {
    notes.push('跳过 c)：找不到 src-tauri/tauri.conf.json');
    return;
  }
  const conf = JSON.parse(readFileSync(p, 'utf8'));
  const bundle = conf.bundle ?? {};
  const blob = JSON.stringify(bundle.resources ?? bundle);
  if (blob.includes(DATA)) {
    fail('c) tauri.conf.json', 'bundle.resources / bundle', `提到了 ${DATA} —— 打包配置会把用户数据卷进安装包`);
  }
  if (conf.build?.frontendDist?.includes(DATA)) {
    fail('c) tauri.conf.json', 'build.frontendDist', `提到了 ${DATA} —— 前端资源目录指到了用户数据区`);
  }
  console.log(`${ok('c) tauri 打包')}  ${dim(`bundle.targets=${bundle.targets ?? '(无)'}，未提及 ${DATA}`)}`);
}

// ---------------------------------------------------------------------------
// d) 发布脚本 / 打包脚本的拷贝目标
// ---------------------------------------------------------------------------
//
// ## 2026-10-12 起放开「从 skeleton/ 铺空骨架」
//
// zip 改成三合一形态（exe + dist/ + agent-data/）之后，打包需要把
// `src-tauri/skeleton/` 铺进产物里的 agent-data/ —— 这是**编译期固定内容**
// （memory/pending/projects/skills 的 README 与内置技能），build.rs 的
// 白名单闸门（下面 e 项）本来就在管它，不含任何用户数据。
//
// 而 `agent-data/` 本身仍是**禁区**：那里面有 API key、会话、截图、日记。
//
// 所以这里从「提到 agent-data 就fail」改成**看拷贝的源路径**：
//   · 源是 `src-tauri/skeleton/`  → 允许（空骨架，可公开分发）
//   · 源是仓库根的 `agent-data/` → 仍然 fail（用户数据）
//   · 目标是数据目录但源不明    → fail（保守，宁枉勿纵）
//
// ## ⚠️ 为什么不能只匹配字面量 "agent-data"（2026-10-12 实测漏洞）
//
// 第一版写的是 `line.includes(DATA)`，变异验证时被绕过：
// 把拷贝目标写成常量名 `DATA_SUBDIR`（而非字面 "agent-data"），
// 闸门就完全不看这一行 —— 一次变异注入、零告警。
// **凡是"禁止出现某物"的闸门，都必须假设对手会改名。**
// 所以改成同时认字面量与常量名，且**默认拒绝**：
// 认不出源是 skeleton 的行一律 fail，而不是放过。
const DATA_LITERAL = 'agent-data';
const DATA_CONST = /\bDATA_SUBDIR\b/;

// ⚠️ 两套命令形态，**必须分开判**：
//   · shell（build-release.sh）：`cp -rf x y` / `rsync x y` —— 命令后跟参数，无括号
//   · JS（package.mjs）：`copyTree(x, y)` —— 函数调用，必须带 `(`
//
// 2026-10-12 两次踩坑都是想用一套正则通吃：
//   第一次 `\b(cp|zip|install|...)\b` 匹配整行 → 模板字符串里的
//     "zip 里没有 ${DATA_SUBDIR}/" 被当成违规命令，误报；
//   第二次改成"必须带 (" → bash 的 `cp -rf agent-data "$REL/"` 全放过了，
//     本地发布脚本反而成了盲区（变异验证抓出来的，正常代码全绿）。
// 教训：给"禁止出现某物"的闸门做变异验证时，**每一种被测文件的语法
// 都要各试一次**，否则你测的只是你脑子里那种写法。
const COPY_CMD_SHELL = /^\s*(?:cp|mv|rsync|tar|7z|install)\s+/;
const COPY_CMD_JS = /(?:\b(?:copyFileSync|cpSync|copyTree)\s*\()|(?:\binstall\s*\()/;

function copyCmdFor(rel) {
  return rel.endsWith('.sh') ? COPY_CMD_SHELL : COPY_CMD_JS;
}

function checkPackagingScripts() {
  const scripts = ['tools/build-release.sh', 'tools/package.mjs'];
  let anyFound = false;

  for (const rel of scripts) {
    const p = join(ROOT, rel);
    if (!existsSync(p)) {
      notes.push(`跳过 d)：找不到 ${rel}`);
      continue;
    }
    anyFound = true;
    const lines = readFileSync(p, 'utf8').split('\n');

    lines.forEach((line, i) => {
      // 只看拷贝/移动类命令，注释里的提及不算。
      // ⚠️ `*` 与 `/**` 也要跳过 —— package.mjs 的块注释续行是 ` * xxx`，
      //    doc 注释首行是 `/** ... */`。2026-10-12 实测漏了这两种，
      //    结果"讲清楚为什么不许铺 agent-data"的注释本身被当成违规命令报出来。
      const t = line.trimStart();
      if (t.startsWith('#') || t.startsWith('//')) return;
      if (t.startsWith('*') || t.startsWith('/**')) return;

      if (!copyCmdFor(rel).test(line)) return;

      // 这一行到底有没有"往数据目录里写"的意图？
      // 字面量或常量名都算 —— 只认字面量会被 `DATA_SUBDIR` 绕过去。
      const mentionsData =
        line.includes(DATA_LITERAL) || DATA_CONST.test(line);
      if (!mentionsData) return;

      // 允许：源路径指向骨架目录（编译期固定内容，e) 项另有白名单闸门）
      // 例：copyTree(SKELETON, path.join(outDir, DATA))
      // 判据看**整行**里有没有 skeleton —— 只要出现就算把来源讲清楚了，
      // 不要求它精确出现在 DATA 之前（那太容易因参数顺序不同而漏判）。
      if (/skeleton/i.test(line)) return;

      fail(
        `d) ${rel}`,
        `第 ${i + 1} 行`,
        `${line.trim()}\n        这条命令涉及 ${DATA} 且源不是 src-tauri/skeleton/ ——\n` +
          `        要么你铺的是真实用户数据（绝不允许），要么请把源改成 skeleton/。`,
      );
    });
  }

  if (anyFound) {
    console.log(`${ok('d) 发布脚本')}  ${dim(`拷贝目标未夹带用户 ${DATA}`)}`);
  }
}

// ---------------------------------------------------------------------------
// e) 骨架区白名单（与 build.rs 的闸门①互为镜像，抓「绕开 cargo 构建」的情况）
// ---------------------------------------------------------------------------
function checkSkeletonWhitelist() {
  const skel = join(ROOT, SKELETON);
  if (!existsSync(skel)) {
    fail('e) 骨架区', SKELETON, '目录不存在 —— 声明式骨架区是内置区的唯一来源');
    return;
  }
  const manifestPath = join(skel, 'manifest.json');
  if (!existsSync(manifestPath)) {
    fail('e) 骨架区', 'skeleton/manifest.json', '缺声明文件 —— manifest 是内置区唯一真相');
    return;
  }
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const declared = new Set(manifest.assets.map((a) => a.src.split('/').join(sep)));

  const onDisk = [];
  (function rec(d) {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const p = join(d, e.name);
      if (e.isDirectory()) rec(p);
      else onDisk.push(relative(skel, p));
    }
  })(skel);

  let undeclared = 0;
  for (const rel of onDisk) {
    if (rel === 'manifest.json') continue;
    if (!declared.has(rel)) {
      undeclared++;
      fail('e) 骨架区', `skeleton/${rel.split(sep).join('/')}`, '没在 manifest.json 里声明 —— 白名单制，多出来的文件一律视为「忘了审」');
    }
  }
  // 反向也要查：声明了但文件不在。否则「删了文件忘了改 manifest」会一路绿灯放过去
  // —— 构建时才会炸，而 build.rs 可能压根没跑（只跑了 npm run test）。
  let missing = 0;
  for (const rel of declared) {
    if (!onDisk.includes(rel)) {
      missing++;
      fail('e) 骨架区', `manifest 声明了 skeleton/${rel.split(sep).join('/')}`, '但文件不存在 —— 声明与实际不一致，产物会和源码对不上');
    }
  }
  console.log(`${ok('e) 骨架白名单')}  ${dim(`${onDisk.length} 个文件，${undeclared} 个未声明，${missing} 个声明了但缺失`)}`);
}

// ---------------------------------------------------------------------------
// f) 骨架区文件不得含本机绝对路径（与 build.rs 闸门⑤⑥⑦ 同规则，跨进程复验）
// ---------------------------------------------------------------------------
function checkSkeletonClean() {
  const skel = join(ROOT, SKELETON);
  if (!existsSync(skel)) return;
  const BAD = [
    { re: /[A-Za-z]:\\/g, why: '本机绝对路径（盘符）—— 会把构建者的用户名与磁盘布局分发给所有用户' },
    { re: /__pycache__|\.pyc$|node_modules|\.venv/gi, why: '编译缓存 / 依赖目录 —— 本机运行状态的残留' },
    { re: /Bearer /g, why: 'Bearer 令牌' },
    { re: /api_key=|apiKey:/g, why: '凭证赋值' },
  ];
  for (const f of walk(skel)) {
    if (f.endsWith('manifest.json')) continue;
    const rel = relative(skel, f).split(sep).join('/');
    const text = readFileSync(f, 'utf8');
    for (const rule of BAD) {
      const m = text.match(rule.re);
      if (m) {
        fail('f) 骨架正文', `skeleton/${rel}`, `${rule.why}（命中 ${JSON.stringify(m[0])}）`);
      }
    }
  }
  console.log(`${ok('f) 骨架正文')}  ${dim('无本机路径 / 无缓存 / 无凭证')}`);
}

// ---------------------------------------------------------------------------
console.log('防夹带闸门 · 仓库级\n');
checkGitTracking();
checkIncludes();
checkTauriBundle();
checkPackagingScripts();
checkSkeletonWhitelist();
checkSkeletonClean();
console.log('');

for (const n of notes) console.log(dim(`  note: ${n}`));
if (notes.length) console.log('');

if (problems.length) {
  console.log(bad(`✗ ${problems.length} 项不合规\n`));
  for (const p of problems) {
    console.log(bad(`  [${p.where}] ${p.what}`));
    console.log(dim(`      ${p.why}\n`));
  }
  process.exit(1);
}
console.log(ok('✓ 全部通过 —— 仓库里没有任何用户数据会被带进产物'));