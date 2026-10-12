// @vitest-environment node
// ============================================================================
// 发布产物布局 —— 回归测试
// ============================================================================
//
// ## 这组测试为什么存在
//
// 2026-10-12：v1.0.0 的 Windows zip 发出去，里面没有前端。用户解压后看到
// 「前端资源未就绪」。CI 全绿，Release 正常创建，只有真跑 exe 才会暴露。
//
// 成因：打包时把 dist 内容**平铺**到产物根，而不是拷进 dist/ 子目录。
// `compute_dist_dir()`（src-tauri/src/disk_assets.rs）只认
// `<exe 同目录>/dist/index.html`，找不到就静默回退 stub 占位页。
//
// ⚠️ 下面 case 2 是这组测试的核心：它拿**事故那版的真实布局**去喂校验器，
//    断言校验器必须报错。只测「正确布局能通过」的测试是自说自话——
//    事故当天 CI 里那句 Expand-Archive + Get-Childitem 也照样绿。
// ============================================================================

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  stageWindowsRelease,
  verifyReleaseLayout,
  DIST_ENTRY,
  DIST_SUBDIR,
} from "../../tools/package.mjs";

const REPO_ROOT = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  ".."
);

let tmp: string;

beforeEach(() => {
  tmp = mkdtempSync(path.join(tmpdir(), "orbcat-pkg-"));
});

afterEach(() => {
  rmSync(tmp, { recursive: true, force: true });
});

/**
 * 手写递归拷贝，不用 `fs.cpSync(..., { recursive: true })`。
 *
 * 本机（Windows 11 + 火绒）上 cpSync 会把 node 进程打死（0xC0000409），
 * 且是拷到一半才死。测试和被测代码不能共享同一个崩溃点，否则变成
 * 「工具坏了所以测试也测不出来」。
 */
function copyTree(src: string, dest: string) {
  mkdirSync(dest, { recursive: true });
  for (const e of readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, e.name);
    const d = path.join(dest, e.name);
    if (e.isDirectory()) copyTree(s, d);
    else copyFileSync(s, d);
  }
}

/** 造一个假的、但结构与真实 dist/ 一致的输入 */
function makeDist(base = tmp, name = "dist"): string {
  const dir = path.join(base, name);
  mkdirSync(path.join(dir, "assets"), { recursive: true });
  writeFileSync(path.join(dir, DIST_ENTRY), "<!doctype html><title>orbcat</title>");
  writeFileSync(path.join(dir, "assets", "index-abc123.js"), "console.log(1)");
  return dir;
}

function makeExe(base = tmp, name = "orbcat.exe"): string {
  const p = path.join(base, name);
  mkdirSync(path.dirname(p), { recursive: true });
  writeFileSync(p, "MZ fake executable");
  return p;
}

// ---------------------------------------------------------------------------

describe("stageWindowsRelease", () => {
  it("产出 <out>/<exe> + <out>/dist/index.html —— compute_dist_dir() 要的正是这两个", () => {
    const exe = makeExe();
    const dist = makeDist();

    const { outDir, exeName } = stageWindowsRelease({ exePath: exe, distDir: dist, outDir: path.join(tmp, "out") });

    expect(exeName).toBe("orbcat.exe");
    expect(verifyReleaseLayout(outDir, { exeName })).toMatchObject({ ok: true });
    expect(path.join(outDir, DIST_SUBDIR, DIST_ENTRY)).toBeTruthy();
  });

  // ⚠️ 防的就是 2026-10-12 那个事故：产物根不能出现 index.html
  it("绝不把 dist 内容平铺到产物根（out/index.html 不应存在）", () => {
    const out = path.join(tmp, "out");
    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
    });

    expect(() => readFileSync(path.join(out, DIST_ENTRY))).toThrow();
    expect(() => readFileSync(path.join(out, "assets", "index-abc123.js"))).toThrow();
  });

  it("附带文件（如 README）放到产物根", () => {
    const readme = path.join(tmp, "README.md");
    writeFileSync(readme, "# orbcat");
    const out = path.join(tmp, "out");

    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
      files: [readme],
    });

    expect(readFileSync(path.join(out, "README.md"), "utf8")).toBe("# orbcat");
    verifyReleaseLayout(out, { exeName: "orbcat.exe" });
  });

  it("输入 dist 缺 index.html 时**当场拒绝**，不产出半成品目录", () => {
    const emptyDist = path.join(tmp, "empty-dist");
    mkdirSync(emptyDist, { recursive: true });
    const out = path.join(tmp, "out");

    expect(() =>
      stageWindowsRelease({
        exePath: makeExe(),
        distDir: emptyDist,
        outDir: out,
      })
    ).toThrow(/dist 入口不存在/);
    // 拒绝发生在 mkdir 之前，不留下会被误当成有效产物的目录
    expect(() => readFileSync(path.join(out, "orbcat.exe"))).toThrow();
  });

  it("重跑会先清空旧目录，不会像 cp -rf 那样把历次 hash 产物攒起来", () => {
    const out = path.join(tmp, "out");
    const dist = makeDist();
    stageWindowsRelease({ exePath: makeExe(), distDir: dist, outDir: out });

    const stale = path.join(out, DIST_SUBDIR, "assets", "stale-old.js");
    writeFileSync(stale, "// 上一次构建的残留");

    stageWindowsRelease({ exePath: makeExe(), distDir: dist, outDir: out });

    expect(() => readFileSync(stale)).toThrow();
  });
});

// ---------------------------------------------------------------------------

describe("verifyReleaseLayout —— 反向用例才是重点", () => {
  // ★★★ 事故回归：CI 当时就是这个布局，校验器必须拦下来 ★★★
  it("【事故回归】平铺布局必须报错，且信息指明是平铺", () => {
    const out = path.join(tmp, "flat");
    const dist = makeDist();
    mkdirSync(out, { recursive: true });
    copyFileSync(makeExe(), path.join(out, "orbcat.exe"));
    copyTree(dist, out); // ← 平铺：dist/* → out/

    expect(() => verifyReleaseLayout(out, { exeName: "orbcat.exe" })).toThrow(
      /平铺/
    );
  });

  it("完全没有 dist 时报错，且信息指明要先 npm run build", () => {
    const out = path.join(tmp, "bare");
    mkdirSync(out, { recursive: true });
    copyFileSync(makeExe(), path.join(out, "orbcat.exe"));

    expect(() => verifyReleaseLayout(out, { exeName: "orbcat.exe" })).toThrow(
      /完全没有 dist/
    );
  });

  it("dist 子目录在但缺 index.html 时报错，不与「平铺」混淆", () => {
    const out = path.join(tmp, "halfdist");
    mkdirSync(path.join(out, DIST_SUBDIR, "assets"), { recursive: true });
    copyFileSync(makeExe(), path.join(out, "orbcat.exe"));

    expect(() => verifyReleaseLayout(out, { exeName: "orbcat.exe" })).toThrow(
      /存在但里面没有/
    );
  });

  it("dist 齐全但 exe 缺失时报错", () => {
    const out = path.join(tmp, "noexe");
    mkdirSync(out, { recursive: true });
    copyTree(makeDist(), path.join(out, DIST_SUBDIR));

    expect(() => verifyReleaseLayout(out, { exeName: "orbcat.exe" })).toThrow(
      /找不到可执行文件/
    );
  });

  it("exe 名未知时按 *.exe 探测（macOS 分支用得上）", () => {
    const out = path.join(tmp, "guess");
    mkdirSync(out, { recursive: true });
    copyFileSync(makeExe(), path.join(out, "whatever.exe"));
    copyTree(makeDist(), path.join(out, DIST_SUBDIR));

    expect(verifyReleaseLayout(out)).toMatchObject({ ok: true });
  });
});

// ---------------------------------------------------------------------------

describe("JS 侧的布局契约与 Rust 侧不漂移", () => {
  // 这组测试的价值不在覆盖率，在于：万一有人改了 disk_assets.rs 的查找规则，
  // 这里立刻红，而不是等到某个用户下载到坏包。
  it("disk_assets.rs 仍然按 <exe 同目录>/dist/index.html 查找", () => {
    const src = readFileSync(
      path.join(REPO_ROOT, "src-tauri", "src", "disk_assets.rs"),
      "utf8"
    );
    expect(src).toMatch(/dir\.join\("dist"\)/);
    expect(src).toMatch(/d\.join\("index\.html"\)\.is_file\(\)/);
  });

  it("CI 的 Windows release job 走 tools/package.mjs，不再自带一份复制逻辑", () => {
    const ci = readFileSync(
      path.join(REPO_ROOT, ".github", "workflows", "ci.yml"),
      "utf8"
    );
    expect(ci).toContain("tools/package.mjs");
    // 平铺写法是事故本体，出现即红
    expect(ci).not.toMatch(/Copy-Item -Recurse -Force dist\/\* pkg\/?\s*$/m);
  });
});