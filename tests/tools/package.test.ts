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
  existsSync,
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
  stageSkeleton,
  verifyReleaseLayout,
  verifyNoUserData,
  DIST_ENTRY,
  DIST_SUBDIR,
  DATA_SUBDIR,
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

describe("「手动跑只出产物、不发布」这个约定不能被悄悄拆掉", () => {
  const ci = readFileSync(
    path.join(REPO_ROOT, ".github", "workflows", "ci.yml"),
    "utf8"
  );
  const TAG_GATE = /startsWith\(github\.ref, 'refs\/tags\/v'\)/g;

  it("workflow_dispatch 能拉起 release job（否则没法不发版就拿到 release 产物）", () => {
    expect(ci).toMatch(/workflow_dispatch:/);
    expect(ci).toMatch(
      /if: startsWith\(github\.ref, 'refs\/tags\/v'\) \|\| github\.event_name == 'workflow_dispatch'/
    );
  });

  // 一旦只剩一处 tag 闸门，就说明 Upload 那步的 if: 被删了 ——
  // softprops 会在没有 tag 的情况下**创建 Release**，
  // 「先实测再上线」会悄悄变成「直接上线」。
  it("tag 闸门至少两处：job 一处 + Upload 步骤一处", () => {
    const n = (ci.match(TAG_GATE) ?? []).length;
    expect(n).toBeGreaterThanOrEqual(2);
  });

// ---------------------------------------------------------------------------
// 三合一分发：zip = exe + dist/ + agent-data/（2026-10-12 用户需求）
// ---------------------------------------------------------------------------

describe("三合一：agent-data 空骨架进 zip", () => {
  it("--skeleton 铺出 <out>/agent-data/，且含 memory/（resolve_data_dir 的判据）", () => {
    const out = path.join(tmp, "out");
    const skel = path.join(tmp, "skel");
    // 造一个最小骨架：与 src-tauri/skeleton 同形
    mkdirSync(path.join(skel, "memory"), { recursive: true });
    mkdirSync(path.join(skel, "skills"), { recursive: true });
    writeFileSync(path.join(skel, "manifest.json"), '{"kind":"orbcat-skeleton"}');
    writeFileSync(path.join(skel, "memory", "MEMORY.md"), "# mem");

    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
      skeletonDir: skel,
    });

    expect(() => readFileSync(path.join(out, DATA_SUBDIR, "memory", "MEMORY.md"))).not.toThrow();
    expect(() => readFileSync(path.join(out, DATA_SUBDIR, "manifest.json"))).not.toThrow();
    // 布局校验不该再提示"zip 里没有 agent-data"
    const { notes } = verifyReleaseLayout(out, {
      exeName: "orbcat.exe",
      wantSkeleton: true,
    });
    expect(notes.join(" ")).not.toContain("没有 agent-data");
  });

  // ★ 反向用例：zip 里带 models.json = 把用户的 API key 公开发布，
  //   且更新时覆盖解压会把它冲掉。这是安全事故，必须硬拦。
  it("agent-data 里出现 models.json 必须抛错（不能把 key 打进发布包）", () => {
    const out = path.join(tmp, "out");
    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
      skeletonDir: (() => {
        const skel = path.join(tmp, "skel-with-key");
        mkdirSync(skel, { recursive: true });
        writeFileSync(path.join(skel, "models.json"), '[{"id":"x","url":"y","apiKey":"sk-secret"}]');
        return skel;
      })(),
    });

    expect(() => verifyNoUserData(out)).toThrow(/models.json/);
    expect(() => verifyNoUserData(out)).toThrow(/API key/);
  });

  it("sessions/ 与 diag.log 同样算用户数据，一并拦下", () => {
    const out = path.join(tmp, "out");
    const skel = path.join(tmp, "skel-2");
    mkdirSync(path.join(skel, "sessions"), { recursive: true });
    writeFileSync(path.join(skel, "sessions", "s1.json"), "{}");
    writeFileSync(path.join(skel, "diag.log"), "log");
    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
      skeletonDir: skel,
    });
    expect(() => verifyNoUserData(out)).toThrow(/sessions|diag\.log/);
  });

  // 没传 skeleton 时给提示（告诉装配方怎么让解压即用），
  // 且**不算失败** —— release 下 resolve_data_dir 会就地创建目录。
  it("不铺骨架时：给提示但不抛错", () => {
    const out = path.join(tmp, "out");
    stageWindowsRelease({ exePath: makeExe(), distDir: makeDist(), outDir: out });
    const { notes } = verifyReleaseLayout(out, { exeName: "orbcat.exe" });
    expect(notes.join(" ")).toContain("没有 agent-data");
    expect(() => verifyNoUserData(out)).not.toThrow();
  });
});
});


// ---------------------------------------------------------------------------
// 内置运行时契约（2026-10-23）
// ---------------------------------------------------------------------------
//
// ## 这组测试为什么存在
//
// 审计 F1：`package.mjs` 找不到 runtime/ 时**只 warn 就放过** ⇒ 装配出一个
// 没有内核的包，CI job 照样是绿的，坏包就这么发出去了。
// 用户那边只看到「未找到内置 runtime/ —— pi 内核不可用，继续用旧内核」，
// 根本不知道发生了什么。
//
// 审计 F2 + 2026-10-23 用户拍板：分发包 runtime = **node + pi**，
// **不含 git**（GPLv2）、**不含 python**（不是硬依赖）。
// 契约跟着决策走，所以这几条必须被测试锁住 ——
// 否则下次谁改回「整个 runtime/ 一起拷」，包会悄悄胖回去。
// ---------------------------------------------------------------------------

/** 造一个假的 runtime/（只放要分发的那两个组件） */
function makeRuntime(base = tmp, comps = ["node", "pi"], name = "runtime"): string {
  const dir = path.join(base, name);
  mkdirSync(dir, { recursive: true });
  for (const c of comps) {
    mkdirSync(path.join(dir, c), { recursive: true });
    if (c === "node") writeFileSync(path.join(dir, "node", "node.exe"), "MZ fake node");
    else writeFileSync(path.join(dir, "pi", "index.js"), "// pi");
  }
  return dir;
}

describe("内置运行时契约 —— 坏包不许放行（F1/F2）", () => {
  it("runtime 目录不存在时**当场报错**（旧版只 warn 就放过 ⇒ 坏包放行）", () => {
    expect(() =>
      stageWindowsRelease({
        exePath: makeExe(),
        distDir: makeDist(),
        outDir: path.join(tmp, "out"),
        runtimeDir: path.join(tmp, "no-such-runtime"),
      })
    ).toThrow(/runtime 目录不存在/);
  });

  it("runtime 缺 node/ 或 pi/ 时报错 —— 这两个是必查组件", () => {
    expect(() =>
      stageWindowsRelease({
        exePath: makeExe(),
        distDir: makeDist(),
        outDir: path.join(tmp, "out-node-missing"),
        runtimeDir: makeRuntime(tmp, ["pi"], "rt-no-node"),
      })
    ).toThrow(/缺 node\//);
    expect(() =>
      stageWindowsRelease({
        exePath: makeExe(),
        distDir: makeDist(),
        outDir: path.join(tmp, "out-pi-missing"),
        runtimeDir: makeRuntime(tmp, ["node"], "rt-no-pi"),
      })
    ).toThrow(/缺 pi\//);
  });

  it("只把 node/ 与 pi/ 拷进包：runtime 里残留的 git/、python/ 不进包", () => {
    const rt = makeRuntime(tmp, ["node", "pi"]);
    // 模拟「上一轮构建过 git/python，本轮决定不分发」
    mkdirSync(path.join(rt, "git"), { recursive: true });
    writeFileSync(path.join(rt, "git", "git.exe"), "MZ fake git");
    mkdirSync(path.join(rt, "python"), { recursive: true });
    writeFileSync(path.join(rt, "python", "python.exe"), "MZ fake python");

    const out = path.join(tmp, "out");
    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
      runtimeDir: rt,
    });
    expect(existsSync(path.join(out, "runtime", "node", "node.exe"))).toBe(true);
    expect(existsSync(path.join(out, "runtime", "pi", "index.js"))).toBe(true);
    expect(existsSync(path.join(out, "runtime", "git"))).toBe(false);
    expect(existsSync(path.join(out, "runtime", "python"))).toBe(false);
  });

  it("verifyReleaseLayout({wantRuntime:true})：缺 runtime 报错，缺 python/git 只出提示", () => {
    // 没有 runtime 的产物 —— 验发布包时必须报错
    const bare = path.join(tmp, "bare");
    stageWindowsRelease({ exePath: makeExe(), distDir: makeDist(), outDir: bare });
    expect(() =>
      verifyReleaseLayout(bare, { exeName: "orbcat.exe", wantRuntime: true })
    ).toThrow(/runtime/);

    // 有 node+pi ⇒ 通过；python/git 缺席只进 notes，**不算缺件**
    const rt = makeRuntime(tmp, ["node", "pi"]);
    const out = path.join(tmp, "out");
    stageWindowsRelease({
      exePath: makeExe(),
      distDir: makeDist(),
      outDir: out,
      runtimeDir: rt,
    });
    const r = verifyReleaseLayout(out, { exeName: "orbcat.exe", wantRuntime: true });
    expect(r.ok).toBe(true);
    expect(r.notes.join(" ")).toMatch(/python/);
    expect(r.notes.join(" ")).toMatch(/git/);
  });
});

// ---------------------------------------------------------------------------
// 审计 F3：CI 必须真的执行 src-tauri/tests/ 下的集成测试 ——
// 而且必须**防住「静默跳过」**，否则加上去的那一步是假绿。
//
// 背景：`cargo test --lib` **根本不编译** `src-tauri/tests/`，所以 1811 行
// pi e2e 测试在 CI 里一条都没跑过（2026-10-23 守门实测 + 审计复核）。
// 而 `src-tauri/tests/pi_*.rs` 有**两个静默跳过**入口：
//   ① `runtime_dir()` 找不到 runtime/        → 「跳过：runtime/ 未构建」
//   ② `orbcat_data_dir()` 找不到 models.json → 「跳过：找不到 orbcat 的 agent-data/」
// 两者都是 `return` 掉整个用例，测试照样算通过 ⇒ CI 全绿而一条 e2e 都没跑。
// 这比不跑更坏：它让人以为集成层已经有守门人了。
//
// 所以这里锁四条：**跑 --tests** + **装 runtime** + **补夹具** + **扫「跳过」字样**。
// ---------------------------------------------------------------------------

describe("集成测试闸门（审计 F3）—— 跑 + 不许静默跳过", () => {
  const ci = readFileSync(
    path.join(REPO_ROOT, ".github", "workflows", "ci.yml"),
    "utf8"
  );
  const actionDir = path.join(REPO_ROOT, ".github", "actions", "setup-runtime");

  it("CI 真的跑 cargo test --tests（--lib 不编译 src-tauri/tests/，那些是死代码）", () => {
    expect(ci).toMatch(/cargo test --tests --manifest-path src-tauri\/Cargo\.toml/);
  });

  it("runtime 装配走复合 action，两处共用同一份实现（不各抄一遍）", () => {
    // build-windows（跑集成测试）+ release job（装发布包）都要 runtime
    const uses = ci.match(/uses: \.\/\.github\/actions\/setup-runtime/g) ?? [];
    expect(uses.length).toBeGreaterThanOrEqual(2);
    // 复合 action 本体必须存在
    expect(existsSync(path.join(actionDir, "action.yml"))).toBe(true);
    expect(existsSync(path.join(actionDir, "assert-runtime.mjs"))).toBe(true);
    expect(existsSync(path.join(actionDir, "ensure-fixture-models.mjs"))).toBe(true);
    // 旧的内联下载步骤不能再留一份（各抄一份 = 改版本号只改一处，另一处照旧）
    expect(ci).not.toMatch(/Invoke-WebRequest -Uri "https:\/\/nodejs\.org/);
  });

  it("「静默跳过」要变成红：CI 扫输出里的跳过字样", () => {
    expect(ci).toMatch(/跳过：runtime\/ 未构建/);
    expect(ci).toMatch(/跳过：找不到 orbcat 的 agent-data\//);
    // ⚠️ `-- --nocapture` 不能省（2026-10-23 守门实测踩到）：
    //   cargo 默认**不打印通过用例的 stderr**，而那两句「跳过：…」正是 eprintln
    //   打出来的 ⇒ 没有 --nocapture，grep 永远匹配不到，断言形同虚设 = 又一次假绿。
    expect(ci).toMatch(/cargo test --tests[^\n]*-- --nocapture/);
  });

  it("本地 npm test 也要跑集成测试（package.json 的 test 链不能只有 --lib）", () => {
    const pkg = JSON.parse(
      readFileSync(path.join(REPO_ROOT, "package.json"), "utf8")
    );
    expect(pkg.scripts["test:rust:integration"]).toMatch(/cargo test --tests/);
    expect(pkg.scripts.test).toContain("test:rust:integration");
  });
});
