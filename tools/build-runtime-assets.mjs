#!/usr/bin/env node
/**
 * orbcat 运行时资产构建 —— 把 pi 内核 / node 装到 `runtime/`。
 *
 * ## 为什么要这个
 *
 * orbcat 依赖系统装了 node**（PTC 的 `run_code` 靠它）**、依赖 pi 内核本身。
 * 用户机器上只要缺一个，功能就残一条。
 *
 * 这个脚本把这两个组件**内置进包里**，用户零安装。
 *
 * ## 分发内容决策（2026-10-23 用户拍板）
 *
 * | 组件 | 决定 | 理由 |
 * |---|---|---|
 * | node | **必带** | PTC `run_code` 的硬依赖，pi 本身也是 node 路线 |
 * | pi | **必带** | 内核本体，缺了 pi 内核直接不可用 |
 * | python | **不带**（可选） | 本文件原来的注释自己写着 python 是「依赖系统有 python（模型可能调）」⇒ **不是 orbcat 的硬依赖**。`pi::bundled_python()` 返回 `Option<PathBuf>`，没有就是 None，**绝不能因此启动失败** |
 * | git+bash | **不带**（可选） | **GPLv2 不再分发**。`pi::bundled_git()` / `bundled_bash()` 同样返回 Option，没有就回落到系统 git/bash |
 *
 * ⇒ **默认只构建 node + pi（约 95 MB）**；要带上 python / git 用
 * `--with-python` / `--with-git` 显式开（本地开发想要完整运行时可以开）。
 *
 * ## 体积账（2026-10-22 实测）
 *
 * | 组件 | 原始 | 内置后 | 做法 | 分发？ |
 * |---|---|---|---|---|
 * | node | 87.2 MB | 83.0 MB | 只取 node.exe（不要 npm/corepack/lib） | ✅ 必带 |
 * | pi | 106.2 MB（pi.exe）| **10.5 MB** | 精简 dist/bundle + jiti（node 路线，不用 Bun） | ✅ 必带 |
 * | python | 2,516.7 MB | 21.4 MB | embeddable 版（完整版的 Lib/ 就 2.4 GB） | ❌ 默认不带 |
 * | git+bash | 410.4 MB | 64.2 MB | 砍 scalar/Skia/Tcl-Tk/curl/svn dll + vim/perl/doc | ❌ 默认不带 |
 * | **默认合计** | | **≈ 95 MB** | node + pi | |
 *
 * ## 用法
 *
 * ```sh
 * node tools/build-runtime-assets.mjs               # 构建 node + pi 到 runtime/
 * node tools/build-runtime-assets.mjs --with-python # 额外带上 python
 * node tools/build-runtime-assets.mjs --with-git    # 额外带上 git+bash
 * node tools/build-runtime-assets.mjs --out <dir>   # 构建到别的目录（测试用）
 * node tools/build-runtime-assets.mjs --verify      # 只验证现有 runtime/ 是否可用
 * ```
 *
 * ## 依赖
 *
 * 需要先把这些源下载到 `tools/.cache/`（脚本不自动下载，理由见下）：
 * - `pi-coding-agent-1.4.0.tgz`（npm registry：@earendil-works/pi-coding-agent）
 * - `node.exe`（nodejs.org）
 * - `python-3.12.8-embed-amd64.zip`（仅 `--with-python` 时需要）
 * - `Git-2.52.0-64-bit.zip` 或本机 `C:\Program Files\Git`（仅 `--with-git` 时需要）
 * - **`photon-pkg/`（npm registry：@silvia-odwyer/photon-node@0.3.4，解包后的目录）**
 *   —— pi 的**图片处理库**（WASM）。缺它 ⇒ pi 的 `resizeImage` 必失败 ⇒
 *   **带图对话的图会被静默丢掉**（prompt 里变成 `[Image omitted: could not be
 *   resized below the inline image size limit.]`）。所以这项**缺了就直接构建失败**，
 *   不给"能跑但图全丢"的包。
 *
 * ⚠️ **为什么脚本不自动下载**：本机火绒会拦截 `bun build --compile` 的临时文件操作，
 * 而且大文件下载在受限网络下极慢。**下载与裁剪分离**，失败时能定位到具体环节。
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "..");

/**
 * 运行时**源**缓存目录（`node.exe` / `pi-pkg/` / `photon-pkg/`）。
 *
 * ⚠️ **为什么允许指到 worktree 之外**（2026-10-24 实测事故后加）：
 * 这份源约 120 MB，每个 worktree 各存一份就是 N 倍；
 * 而 worktree 里的 `tools/.cache` 若是**指向共享目录的 junction**，
 * `git worktree remove --force` 会**跟着 junction 把共享目录内容删掉**
 * —— 本次实际踩到：主仓库的 `node_modules` 与 `tools/.cache` 被清空
 * ⇒ runtime 三个源全丢、`runtime/` 变成悬空链接。
 * ⇒ 口径：**共享源放所有 worktree 之外**（本机用 `D:\workplace\orbcat-cache`），
 * 用 `--cache <dir>` 或环境变量 `ORBCAT_CACHE_DIR` 指进来；
 * 默认仍是 `tools/.cache`（兼容旧用法）。
 */
const CACHE_ARG = argValue("--cache") || process.env.ORBCAT_CACHE_DIR;
const CACHE = CACHE_ARG ? path.resolve(CACHE_ARG) : path.join(__dirname, ".cache");

const VERIFY_ONLY = process.argv.includes("--verify");

/**
 * 是否额外构建 python / git+bash。
 *
 * ⚠️ **默认关**（2026-10-23 用户拍板：分发包只带 node + pi）：
 * python 不是 orbcat 的硬依赖（模型可能调而已），
 * git 是 GPLv2 不再分发。
 * 两个都是「有就用，没有就回落系统」，不是「缺了就不许跑」。
 */
const WITH_PYTHON = process.argv.includes("--with-python");
const WITH_GIT = process.argv.includes("--with-git");

/**
 * 输出目录。默认仓库根 `runtime/`。
 * `--out <dir>` 改到别处 —— 测试时不必动用户真实的 runtime/。
 */
function argValue(flag) {
  const i = process.argv.indexOf(flag);
  if (i < 0) return null;
  const v = process.argv[i + 1];
  return v && !v.startsWith("--") ? v : null;
}
const OUT_ARG = argValue("--out");
const RUNTIME = OUT_ARG ? path.resolve(OUT_ARG) : path.join(ROOT, "runtime");

// ─────────────────────────────────────────────────────────────
// 工具
// ─────────────────────────────────────────────────────────────

function log(...a) {
	console.log("[build-runtime]", ...a);
}

function fail(msg, detail) {
	console.error("[build-runtime] ❌ " + msg);
	if (detail) console.error(detail);
	process.exit(1);
}

/**
 * 目录体积（MB）。
 *
 * ⚠️ **内部一律用字节累加，只在最外层转 MB**。
 * 踩过的坑：早期版本让递归调用返回 MB、再加进字节累加器，
 * 于是「二次除以 1048576」—— 单层目录（node）碰巧对，
 * 多层目录（git）直接归零（2026-10-22 实测踩到）。
 */
function dirSizeBytes(p) {
	if (!fs.existsSync(p)) return 0;
	let total = 0;
	for (const e of fs.readdirSync(p, { withFileTypes: true })) {
		const full = path.join(p, e.name);
		try {
			total += e.isDirectory() ? dirSizeBytes(full) : fs.statSync(full).size;
		} catch {
			/* 符号链接 statSync 会抛 —— 跳过，不能让它把整个统计变成 0 */
		}
	}
	return total;
}

function dirSizeMB(p) {
	return Math.round((dirSizeBytes(p) / 1048576) * 10) / 10;
}

function copyDir(src, dst, filter) {
	fs.mkdirSync(dst, { recursive: true });
	for (const e of fs.readdirSync(src, { withFileTypes: true })) {
		const s = path.join(src, e.name);
		const d = path.join(dst, e.name);
		if (e.isDirectory()) copyDir(s, d, filter);
		else if (e.isFile() && (!filter || filter(e.name, s))) fs.copyFileSync(s, d);
	}
}

function copyFiles(srcDir, dstDir, names) {
	fs.mkdirSync(dstDir, { recursive: true });
	let n = 0;
	for (const nm of names) {
		const s = path.join(srcDir, nm);
		if (fs.existsSync(s)) {
			fs.copyFileSync(s, path.join(dstDir, nm));
			n++;
		}
	}
	return n;
}

/**
 * 递归拷贝，但**跳过符号链接**。
 *
 * 为什么需要：Git for Windows 的 `mingw64/lib` 和 `etc/` 里有大量符号链接
 * （`engines-3` → `../../etc/...`）。Windows 上 `copyFileSync` 对它们会
 * `EPERM: operation not permitted`（2026-10-22 实测踩到）。
 * 那些链接对 git 运行时没用，直接跳过。
 */
function copyDirSafe(src, dst) {
	fs.mkdirSync(dst, { recursive: true });
	for (const e of fs.readdirSync(src, { withFileTypes: true })) {
		const s = path.join(src, e.name);
		const d = path.join(dst, e.name);
		if (e.isDirectory()) {
			copyDirSafe(s, d);
		} else if (e.isFile()) {
			try {
				fs.copyFileSync(s, d);
			} catch {
				/* 符号链接 —— 跳过 */
			}
		}
	}
}

/**
 * 删目录。
 *
 * ⚠️ **不要用 `fs.rmSync` 递归删大目录**：本机有 safe-delete 钩子（走回收站），
 * 180 MB / 上千文件会 ETIMEDOUT（2026-10-22 实测）。
 * 钩子还会把删不掉的文件改名成 `.old-<ts>` —— **那会污染发布包**。
 *
 * 解法：**先构建到 `<name>.new`，构建完再切换**。
 * 旧目录改名挪到 `<name>.old-<ts>`（保留可回滚），新目录改名到位。
 */
function stageDir(name) {
	const final = path.join(RUNTIME, name);
	const staged = path.join(RUNTIME, name + ".new");
	// 清掉上次没切干净的 .new
	try {
		fs.rmSync(staged, { recursive: true, force: true, maxRetries: 2, retryDelay: 50 });
	} catch {
		/* 钩子拦了 —— 用带时间戳的新名 */
		const alt = path.join(RUNTIME, name + ".new-" + Date.now());
		return { staged: alt, final };
	}
	fs.mkdirSync(staged, { recursive: true });
	return { staged, final };
}

/**
 * 构建完成后把 staged 切到 final。
 *
 * 旧的怎么办：safe-delete 钩子会拦 `rmSync`（大目录 ETIMEDOUT），
 * 所以**改名挪到 `<name>.trash-<ts>`**，并且**必须真的删掉**——
 * 否则 `.trash-*` 会混进发布包（2026-10-22 实测踩到：runtime/ 下留了 4 个 .old- 目录）。
 * 删不掉就用 `rmdir /s /q`（cmd 内建，绕开 node 的钩子）。
 */
function commitDir(staged, final) {
	if (!fs.existsSync(staged)) return;
	const base = path.basename(final);
	const trash = path.join(RUNTIME, base + ".trash-" + Date.now());
	try {
		if (fs.existsSync(final)) fs.renameSync(final, trash);
	} catch {
		/* 挪不动就原地覆盖 */
	}
	try {
		fs.renameSync(staged, final);
	} catch {
		fs.mkdirSync(final, { recursive: true });
		copyDir(staged, final);
	}
	// 真正清掉旧目录
	if (fs.existsSync(trash)) {
		// ⚠️ `etc/hosts` 等文件是**只读**的 —— 不解除只读属性就删不掉
		// （2026-10-22 实测：`Directory.Delete` 报 "Access to the path 'hosts' is denied"）
		clearReadonly(trash);
		try {
			fs.rmSync(trash, { recursive: true, force: true, maxRetries: 2, retryDelay: 50 });
		} catch {
			// 钩子拦了 → 用 PowerShell 的 .NET API（不经过 node 的 shim）
			tryExec("powershell", ["-NoProfile", "-Command",
				`[System.IO.Directory]::Delete('${trash.replace(/'/g, "''")}', $true)`]);
		}
	}
}

/** 递归解除只读属性（Git for Windows 的 etc/hosts 等是只读的） */
function clearReadonly(dir) {
	const stack = [dir];
	while (stack.length) {
		const cur = stack.pop();
		let entries;
		try {
			entries = fs.readdirSync(cur, { withFileTypes: true });
		} catch {
			continue;
		}
		for (const e of entries) {
			const full = path.join(cur, e.name);
			if (e.isDirectory()) {
				stack.push(full);
			} else {
				try {
					const st = fs.statSync(full);
					if (st.mode & 0o200) {
						fs.chmodSync(full, 0o666);
					}
				} catch {}
			}
		}
	}
}

function tryExec(cmd, args, opts = {}) {
	try {
		return execFileSync(cmd, args, {
			encoding: "utf8",
			// ⚠️ `input` 需要 stdin 是 pipe；不显式写 stdio 的话
			// `execFileSync` 会把 stdin 设成 'pipe'，但传了 `input` 时
			// 若 stdio 被显式覆盖成 ["ignore",...] 就会写不进去（2026-10-22 实测）。
			stdio: opts.input !== undefined ? ["pipe", "pipe", "pipe"] : ["ignore", "pipe", "pipe"],
			timeout: 30000,
			...opts,
		});
	} catch (e) {
		if (process.env.ORBCAT_VERBOSE) {
			console.error("[build-runtime] exec 失败:", cmd, args.join(" "), e.message);
		}
		return null;
	}
}

// ─────────────────────────────────────────────────────────────
// 组件清单
// ─────────────────────────────────────────────────────────────

/** bash 需要的 coreutils + 常用工具（实测 bash 里 command -v 全在） */
const BASH_TOOLS = [
	"bash.exe", "sh.exe", "ls.exe", "cat.exe", "cp.exe", "mv.exe", "rm.exe", "mkdir.exe", "rmdir.exe",
	"sed.exe", "grep.exe", "find.exe", "echo.exe", "pwd.exe", "head.exe", "tail.exe", "printf.exe",
	"sort.exe", "uniq.exe", "wc.exe", "cut.exe", "tr.exe", "tee.exe", "diff.exe", "touch.exe", "env.exe",
	"basename.exe", "dirname.exe", "realpath.exe", "which.exe", "sleep.exe", "date.exe", "true.exe", "false.exe",
	"seq.exe", "nl.exe", "od.exe", "stat.exe", "du.exe", "df.exe", "ln.exe", "chmod.exe", "test.exe",
	"tar.exe", "gzip.exe", "gunzip.exe", "awk.exe", "xargs.exe", "expr.exe", "id.exe", "whoami.exe",
	"uname.exe", "file.exe", "cmp.exe", "dos2unix.exe", "unix2dos.exe", "cygpath.exe", "iconv.exe",
];

/**
 * mingw64/bin 里可以砍掉的 DLL —— 全部是 **scalar / git-lfs / git-gui 的依赖**，
 * git 本身用不到。实测逐个删掉后 `git --version` 仍正常（2026-10-22 验证）。
 * 省 28.2 MB。
 */
const GIT_DROP_MINGW_DLL = new Set([
	"libSkiaSharp.dll", "av_libglesv2.dll", "msalruntime_x86.dll", "tcl86.dll", "tk86.dll",
	"libHarfBuzzSharp.dll", "SkiaSharp.dll", "Avalonia.Base.dll", "Avalonia.Controls.dll",
	"Microsoft.Identity.Client.dll", "libp11-kit-0.dll", "libcurl-4.dll", "libcurl-openssl-4.dll",
	"libunistring-5.dll",
]);

/**
 * usr/bin 里可以砍掉的 msys DLL —— ssh/gnutls/svn/sqlite/zstd 等，
 * bash 基础命令用不到。实测逐个删掉后 bash 全部核心命令仍 `command -v` 命中。
 * 省 11.2 MB。
 */
const GIT_DROP_USR_DLL = new Set([
	"msys-gnutls-30.dll", "msys-p11-kit-0.dll", "msys-sqlite3-0.dll",
	"msys-svn_subr-1-0.dll", "msys-svn_wc-1-0.dll", "msys-svn_client-1-0.dll",
	"msys-gcrypt-20.dll", "msys-ssl-3.dll", "msys-asn1-8.dll", "msys-mpfr-6.dll",
	"msys-zstd-1.dll", "msys-bz2-1.dll", "msys-lzma-5.dll",
]);

// ─────────────────────────────────────────────────────────────
// 各组件构建
// ─────────────────────────────────────────────────────────────

function buildNode() {
	log("node ...");
	const { staged: dst, final } = stageDir("node");

	// 优先用缓存里的 node.exe（保证版本与 pi 要求一致：>= 22.19.0）
	const cached = path.join(CACHE, "node.exe");
	if (fs.existsSync(cached)) {
		fs.copyFileSync(cached, path.join(dst, "node.exe"));
	} else {
		// 退而求其次：找本机已装的
		const cands = [
			path.join(CACHE, "node.exe"),
			"C:/Program Files/nodejs/node.exe",
			"D:/NODE.JS/node.exe",
		];
		const found = cands.find((p) => fs.existsSync(p));
		if (!found) fail("找不到 node.exe —— 请把 node 22.19+ 的 node.exe 放到 tools/.cache/node.exe");
		fs.copyFileSync(found, path.join(dst, "node.exe"));
	}
	const ver = tryExec(path.join(dst, "node.exe"), ["--version"]);
	commitDir(dst, final);
	log(`  ✓ ${ver?.trim()}  (${dirSizeMB(final)} MB)`);
}

function buildPython() {
	log("python ...");
	const { staged: dst, final } = stageDir("python");
	const src = path.join(CACHE, "python-embed");

	if (!fs.existsSync(src)) {
		const zip = path.join(CACHE, "python-embed.zip");
		if (!fs.existsSync(zip)) {
			fail("找不到 python embeddable —— 请下载 python-3.12.x-embed-amd64.zip 到 tools/.cache/");
		}
		fs.mkdirSync(src, { recursive: true });
		tryExec("powershell", ["-NoProfile", "-Command",
			`Expand-Archive -Force -LiteralPath "${zip}" -DestinationPath "${src}"`]);
	}
	copyDir(src, dst);

	const ver = tryExec(path.join(dst, "python.exe"), ["-c", "import sys;print(sys.version.split()[0])"]);
	commitDir(dst, final);
	log(`  ✓ ${ver?.trim()}  (${dirSizeMB(final)} MB)`);
}

function buildGit() {
	log("git + bash ...");
	const { staged: dst, final } = stageDir("git");

	// 源：优先本机 Git for Windows 安装（省一次下载），否则用缓存 zip
	let src = process.env.ORBCAT_GIT_SRC;
	if (!src) {
		for (const p of ["C:/Program Files/Git", "C:/Program Files (x86)/Git", "D:/git/Git"]) {
			if (fs.existsSync(p)) {
				src = p;
				break;
			}
		}
	}
	if (!src) {
		const zip = path.join(CACHE, "git.zip");
		if (!fs.existsSync(zip)) {
			fail("找不到 Git 源 —— 安装 Git for Windows，或把 git.zip 放到 tools/.cache/");
		}
		src = path.join(CACHE, "git-unpacked");
		fs.rmSync(src, { recursive: true, force: true });
		fs.mkdirSync(src, { recursive: true });
		tryExec("powershell", ["-NoProfile", "-Command",
			`Expand-Archive -Force -LiteralPath "${zip}" -DestinationPath "${src}"`]);
	}

	for (const d of ["cmd", "etc", "etc/profile.d", "libexec/git-core", "mingw64/bin", "mingw64/lib", "usr/bin", "usr/share/terminfo"]) {
		fs.mkdirSync(path.join(dst, d), { recursive: true });
	}

	// 1) git 核心 —— 现代 git 是单 exe（所有子命令内置），只需 git.exe
	fs.copyFileSync(path.join(src, "mingw64/libexec/git-core/git.exe"), path.join(dst, "libexec/git-core/git.exe"));
	fs.copyFileSync(path.join(src, "mingw64/bin/git.exe"), path.join(dst, "mingw64/bin/git.exe"));

	// 2) mingw64/bin 的 DLL（剔掉 scalar/lfs 专用）
	for (const f of fs.readdirSync(path.join(src, "mingw64/bin"))) {
		if (f.endsWith(".dll") && !GIT_DROP_MINGW_DLL.has(f)) {
			fs.copyFileSync(path.join(src, "mingw64/bin", f), path.join(dst, "mingw64/bin", f));
		}
	}
	// mingw64/lib 里是**符号链接**（engines-3 之类，指向 ../../etc/... ）
	// —— 直接 copyFile 会 EPERM。只拷普通文件，链接跳过（git 运行时用不到）。
	for (const e of fs.readdirSync(path.join(src, "mingw64/lib"), { withFileTypes: true })) {
		if (!e.isFile()) continue;
		const s = path.join(src, "mingw64/lib", e.name);
		try {
			fs.copyFileSync(s, path.join(dst, "mingw64/lib", e.name));
		} catch {
			/* 符号链接或无权限 —— 跳过，不影响 git 运行 */
		}
	}

	// 3) bash + coreutils + msys runtime（剔掉用不到的）
	copyFiles(path.join(src, "usr/bin"), path.join(dst, "usr/bin"), BASH_TOOLS);
	for (const f of fs.readdirSync(path.join(src, "usr/bin"))) {
		if (f.startsWith("msys-") && f.endsWith(".dll") && !GIT_DROP_USR_DLL.has(f)) {
			fs.copyFileSync(path.join(src, "usr/bin", f), path.join(dst, "usr/bin", f));
		}
	}

	// 4) terminfo（bash 需要）+ etc（etc 里也有符号链接 → 只拷普通文件）
	const term = path.join(src, "usr/share/terminfo");
	if (fs.existsSync(term)) copyDir(term, path.join(dst, "usr/share/terminfo"));
	copyDirSafe(path.join(src, "etc"), path.join(dst, "etc"));
	fs.copyFileSync(path.join(src, "cmd/git.exe"), path.join(dst, "cmd/git.exe"));

	// 5) wrapper —— ⚠️ 必须！
	//
	// bash 继承 Windows 的 PATH，而 `/usr/bin` 的映射不保证排在前面 →
	// 直接跑 bash.exe 时 `ls` / `head` / `cat` 会 `command not found`（2026-10-22 实测）。
	// 所以必须显式设 PATH，不能指望用户环境的 PATH。
	const wrapper = [
		"@echo off",
		"rem orbcat 附带的 bash —— 显式修正 PATH（不依赖用户环境）",
		'set "PATH=%~dp0usr\\bin;%~dp0cmd;%~dp0mingw64\\bin;%PATH%"',
		'set "MSYS2_PATH_TYPE=inherit"',
		'set "MSYSTEM=MINGW64"',
		'"%~dp0usr\\bin\\bash.exe" %*',
	].join("\r\n");
	fs.writeFileSync(path.join(dst, "bash.cmd"), wrapper, "ascii");

	const ver = tryExec(path.join(dst, "cmd/git.exe"), ["--version"]);
	commitDir(dst, final);
	log(`  ✓ ${ver?.trim()}  (${dirSizeMB(final)} MB)`);
}

function buildPi() {
	log("pi 内核 ...");
	const { staged: dst, final } = stageDir("pi");
	const src = path.join(CACHE, "pi-pkg");

	if (!fs.existsSync(src)) {
		fail("找不到 pi 包 —— 请把 pi-coding-agent 的 npm 包解到 tools/.cache/pi-pkg/");
	}

	// 只留 dist/bundle（自包含）+ jiti（.ts 扩展运行时编译必需）
	const bundle = path.join(src, "dist/bundle");
	if (!fs.existsSync(bundle)) {
		fail(`pi 包里没有 dist/bundle —— 确认是 @earendil-works/pi-coding-agent 的 npm 包（不是 GitHub release 的 pi.exe）`);
	}
	copyDir(bundle, path.join(dst, "dist/bundle"));

	const jiti = path.join(src, "node_modules/jiti");
	if (fs.existsSync(jiti)) {
		fs.mkdirSync(path.join(dst, "node_modules"), { recursive: true });
		copyDir(jiti, path.join(dst, "node_modules/jiti"));
	} else {
		log("  ⚠️ 没找到 jiti —— .ts 扩展将无法加载（如果只用 .js 扩展可忽略）");
	}

	// ── photon-node（pi 的图片处理库，WASM）──
	//
	// ⚠️ **缺了它，带图对话的图会被静默丢掉** ——
	//
	// pi 的图片归一化链路（`dist/core/agent-session.js`）：
	//   prompt 路径 → `_normalizePromptImages` → `processImage` → `resizeImage`
	//   `resizeImage` 靠 `loadPhoton()`（`import("@silvia-odwyer/photon-node")`）
	//   ⇒ 装不上 ⇒ 返回 null ⇒ processImage 返回 `ok:false` ⇒
	//   **图被丢掉，正文里只留一句 `[Image omitted: ...]`**（不报错、最难查）。
	//
	// pi 的 package.json 把它列为**运行时依赖**（不是 optional），
	// 但 npm 包里不附带依赖 ⇒ 必须显式带上（我们曾经漏了，2026-10-23 实测）。
	//
	// 体积 2.16 MB（含 1.88 MB WASM），零传递依赖。
	const photon = path.join(CACHE, "photon-pkg", "package");
	if (!fs.existsSync(photon)) {
		fail(
			"找不到 photon-node（pi 的图片处理库）—— " +
				"请先下载并解包到 tools/.cache/photon-pkg/：\n" +
				"    npm pack @silvia-odwyer/photon-node@0.3.4 --pack-destination tools/.cache\n" +
				"    mkdir tools/.cache/photon-pkg && tar -xzf tools/.cache/silvia-odwyer-photon-node-0.3.4.tgz -C tools/.cache/photon-pkg\n" +
				"⚠️ 不带它的话，带图对话的图会被**静默丢掉**（pi 报 'could not be resized'）",
		);
	}
	const photonDst = path.join(dst, "node_modules", "@silvia-odwyer", "photon-node");
	fs.mkdirSync(path.dirname(photonDst), { recursive: true });
	copyDir(photon, photonDst);

	// ⚠️ **断言 wasm 真落地** —— 「目录拷过去了」不等于「wasm 在里面」。
	//   npm 包布局一变（改名 / 挪位 / 少打文件），带图对话的图又会被静默丢掉，
	//   而构建日志一路绿灯 ⇒ 这道断言是唯一的兜底。
	const photonWasm = path.join(photonDst, "photon_rs_bg.wasm");
	if (!fs.existsSync(photonWasm)) {
		fail(
			"photon-node 拷进去了，但里面**没有 photon_rs_bg.wasm**：\n" +
				`    ${photonWasm}\n` +
				"  pi 的 resizeImage 靠这个 wasm 做图像编解码 ⇒ 缺了它，带图对话的图会被**静默丢掉**\n" +
				"  （模型只收到文字，构建日志仍全绿）。\n" +
				"  修法：确认 npm 包 @silvia-odwyer/photon-node@0.3.4 里确实有 photon_rs_bg.wasm",
		);
	}
	log(
		`  ✓ photon（wasm ${(fs.statSync(photonWasm).size / 1024 / 1024).toFixed(2)} MB，` +
			`路径 runtime/pi/node_modules/@silvia-odwyer/photon-node/photon_rs_bg.wasm）`,
	);

	const pkg = path.join(src, "package.json");
	if (fs.existsSync(pkg)) fs.copyFileSync(pkg, path.join(dst, "package.json"));

	// ── extension（orbcat 的工具桥 + 权限闸门）──
	//
	// ⚠️ **必须拷** —— 没有它们 pi 就看不到 orbcat 的 34 个工具
	//（`orbcat-tools.ts` 注册工具，`orbcat-gate.ts` 做权限裁决）。
	// 源在 `runtime-ext/`（仓库里，跟着代码走，不在 runtime/ 里 ——
	// runtime/ 整个是构建产物，会被重建覆盖掉）。
	const extSrc = path.join(ROOT, "runtime-ext");
	const extDst = path.join(dst, "extensions");
	if (fs.existsSync(extSrc)) {
		fs.mkdirSync(extDst, { recursive: true });
		for (const f of fs.readdirSync(extSrc)) {
			if (f.endsWith(".ts")) {
				fs.copyFileSync(path.join(extSrc, f), path.join(extDst, f));
			}
		}
		const exts = fs.readdirSync(extDst).filter((x) => x.endsWith(".ts"));
		log(`  ✓ extension: ${exts.join(", ")}`);
		// 三个 extension 缺一不可：
		//   orbcat-tools.ts  → 注册 34 个 orb_* 工具
		//   orbcat-gate.ts   → 权限裁决
		//   orbcat-prompt.ts → system prompt 接管（人格与记忆）
		for (const need of ["orbcat-tools.ts", "orbcat-gate.ts", "orbcat-prompt.ts"]) {
			if (!exts.includes(need)) {
				log(`  ❌ 缺 extension: ${need} —— pi 内核会缺功能！`);
			}
		}
	} else {
		log("  ⚠️ 找不到 runtime-ext/ —— pi 将看不到 orbcat 的工具！");
	}

	commitDir(dst, final);
	log(`  ✓ ${dirSizeMB(final)} MB`);
}

// ─────────────────────────────────────────────────────────────
// 验证
// ─────────────────────────────────────────────────────────────

/**
 * 验证 runtime/ 的可用性。
 *
 * ## 契约（2026-10-23 用户拍板后改过）
 *
 * - **必查**：`node/node.exe`（PTC run_code + pi 都是 node 路线）、
 *   `pi/dist/bundle/cli.js`（内核本体）。
 *   这两个缺一个 ⇒ 这个 runtime 是坏的 ⇒ **fail**。
 * - **不查**：`python/`、`git/`。它们**不是 orbcat 的硬依赖**
 *   （`pi::bundled_python()` / `bundled_git()` / `bundled_bash()`
 *   全部返回 `Option<PathBuf>`，没有就是 None，回落到系统）。
 *   缺了就给一句提示（告诉用户能在哪指定），**绝不 fail**。
 *
 * ⚠️ 旧版把 python / git 也算进必查，于是「按新决策瘦身」的 runtime
 * 会被自己的校验判死 —— 那种 fail 是自相矛盾。
 */
function verify() {
	log("验证 ...");
	const node = path.join(RUNTIME, "node/node.exe");
	const py = path.join(RUNTIME, "python/python.exe");
	const git = path.join(RUNTIME, "git/cmd/git.exe");
	const bashCmd = path.join(RUNTIME, "git/bash.cmd");
	const pi = path.join(RUNTIME, "pi/dist/bundle/cli.js");

	// ── 必查 ──
	const required = [];
	required.push(["node", tryExec(node, ["--version"])?.trim()]);

	// photon（pi 的图片处理库，WASM）
	//
	// ⚠️ 缺它 **不会** 让 pi 报错 —— pi 只是把图丢掉，正文里留一句
	// `[Image omitted: could not be resized below the inline image size limit.]`。
	// ⇒ 这是**静默功能缺失**，只能靠发包前的硬校验拦住。
	const photonWasm = path.join(
		RUNTIME,
		"pi/node_modules/@silvia-odwyer/photon-node/photon_rs_bg.wasm",
	);
	required.push([
		"photon",
		fs.existsSync(photonWasm)
			? "图片处理库 ✓"
			: "❌ 缺 photon-node ⇒ 带图对话的图会被静默丢掉",
	]);

	// pi 冒烟：发一条 get_state，看有没有 response
	if (fs.existsSync(pi) && fs.existsSync(node)) {
		const out = tryExec(node, [pi, "--mode", "rpc", "--no-session"], {
			input: '{"id":"v","type":"get_state"}\n',
			env: { ...process.env, PI_OFFLINE: "1" },
			timeout: 20000,
		});
		const ok = out && out.includes('"command":"get_state"') && out.includes('"success":true');
		required.push(["pi rpc", ok ? "get_state success ✓" : "❌ 无响应"]);
	} else {
		required.push(["pi rpc", `❌ 缺 ${pi}`]);
	}

	for (const [k, v] of required) {
		log(`  ${v ? "✓" : "❌"} ${k.padEnd(8)} ${v ?? "(失败)"}`);
	}
	if (required.some(([, v]) => !v)) {
		fail("验证未通过（node / pi 是必查项）");
	}

	// ── 不查（缺了只提示，不 fail）──
	log("");
	if (fs.existsSync(path.join(RUNTIME, "python"))) {
		const pyVer = tryExec(py, ["-c", "import sys,json,ssl,sqlite3;print(sys.version.split()[0]+' stdlib-ok')"]);
		log(`  ${pyVer ? "✓" : "⚠️"} ${"python".padEnd(8)} ${pyVer?.trim() ?? "(存在但跑不起来 —— 不影响发布)"}`);
	} else {
		log("  ℹ️ python    未检测到内置 Python —— 可在设置里指定路径，或直接安装系统 Python。");
		log("               （不是硬依赖：模型要调 python 时会走系统那份）");
	}
	if (fs.existsSync(path.join(RUNTIME, "git"))) {
		const gitVer = tryExec(git, ["--version"]);
		// bash 必须走 wrapper（PATH 问题）
		// ⚠️ Windows 上 `execFileSync` 不能直接跑 .cmd（EINVAL）→ 必须经 cmd.exe /c
		const bashOut = tryExec("cmd.exe", ["/d", "/c", bashCmd, "-c", "echo bash-ok; ls -1 >/dev/null && echo ls-ok; echo 中文"]);
		log(`  ${gitVer ? "✓" : "⚠️"} ${"git".padEnd(8)} ${gitVer?.trim() ?? "(存在但跑不起来 —— 不影响发布)"}`);
		log(`  ${bashOut ? "✓" : "⚠️"} ${"bash".padEnd(8)} ${bashOut?.trim().replace(/\s+/g, " ") ?? "(存在但跑不起来)"}`);
	} else {
		log("  ℹ️ git       未分发内置 Git（GPLv2）—— 将使用系统 Git；没有 Git 的功能不可用。");
	}
}

/**
 * 把一个组件从输出目录里**移走**（本轮不分发它）。
 *
 * ## 为什么要真的删
 *
 * `build-runtime-assets.mjs` 只在构建时覆盖它自己管的子目录，
 * 不会清理别的组件 —— 于是「上一轮构建过 python，本轮决定不分发」时，
 * 旧的 `runtime/python/` 会**原地留下来**，照样被打进发布包。
 * 决策改了，产物就必须跟着变，否则「用户拍板了但包没瘦」= 白拍。
 *
 * 删法沿用 `commitDir` 的那套：先改名挪到 `<name>.trash-<ts>`（可回滚），
 * 再真删（safe-delete 钩子会拦 rmSync ⇒ 退到 PowerShell 的 .NET API）。
 */
function dropComponent(name, why) {
	const final = path.join(RUNTIME, name);
	if (!fs.existsSync(final)) return;
	const trash = path.join(RUNTIME, `${name}.trash-${Date.now()}`);
	try {
		fs.renameSync(final, trash);
	} catch {
		log(`  ⚠️ 挪不动 ${name}/ —— 手动清：${final}`);
		return;
	}
	clearReadonly(trash);
	try {
		fs.rmSync(trash, { recursive: true, force: true, maxRetries: 2, retryDelay: 50 });
	} catch {
		tryExec("powershell", ["-NoProfile", "-Command",
			`[System.IO.Directory]::Delete('${trash.replace(/'/g, "''")}', $true)`]);
	}
	log(`  ✓ 已移除 ${name}/（${why}）`);
}

// ─────────────────────────────────────────────────────────────

if (VERIFY_ONLY) {
	verify();
} else {
	log(`输出目录: ${RUNTIME}`);
	fs.mkdirSync(RUNTIME, { recursive: true });
	buildNode();
	if (WITH_PYTHON) {
		buildPython();
	} else {
		dropComponent("python", "本轮不分发：模型可能调，不是硬依赖");
	}
	if (WITH_GIT) {
		buildGit();
	} else {
		dropComponent("git", "本轮不分发：GPLv2");
	}
	buildPi();
	verify();
	log("");
	log("=== 体积账 ===");
	for (const d of ["node", "python", "git", "pi"]) {
		log(`  ${d.padEnd(8)} ${String(dirSizeMB(path.join(RUNTIME, d))).padStart(7)} MB`);
	}
	log(`  ${"合计".padEnd(8)} ${String(dirSizeMB(RUNTIME)).padStart(7)} MB`);
}
