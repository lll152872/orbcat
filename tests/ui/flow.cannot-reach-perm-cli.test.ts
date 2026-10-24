import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { bootApp, openPanel, waitFor, callsOf, sleep } from "./helpers";
import { emitDragDrop } from "./mocks/tauri";

/**
 * **守卫用例：前端碰不到那个内置 CLI**（`bin\orbcat-perm.exe`）。
 *
 * ## 背景（用户拍板的新需求）
 *
 * orbcat 要加一个**内置 CLI**，让 **orbcat 进程之外的外部 AI**（用户手上跑的
 * Codex / Claude Code 之类）能**代用户**处理权限申请。
 *
 * **硬线：orbcat 内部的 AI 不许碰这个 CLI** ⇒ 隔离手段 = **路径阻断**
 * （碰到那个路径直接停）。主体实现归**后端**（CLI + 阻断规则）；
 * **前端这块是守卫用例** —— 用测试把「前端碰不到」这条性质**锁死**，
 * 免得以后有人给前端加一条能执行本地进程的通路。
 *
 * ## 这三条测的是「能力边界」，不是「功能」
 *
 * ① 前端**不存在**能执行本地命令/进程的调用（对着数据层的命令表断言）；
 * ② 拖放进来的路径**只填输入框**，不会被当成命令执行；
 * ③ 「用户没同意的命令」不会被当成可执行（自动放行的开关默认最严 + 决定只能由用户点出来）。
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const SRC = path.resolve(here, "../../src");

/** 读源码（静态断言用）。 */
function src(file: string): string {
  return readFileSync(path.join(SRC, file), "utf8");
}

/**
 * 前端**能触达的全部后端命令** —— 从数据层的真后端实现里扫出来。
 *
 * ⚠️ 为什么扫 `src/data/tauri.ts` 而不是别的：
 * 这是**唯一**一处把 TS 调用翻译成 Tauri `invoke` 的地方
 * ⇒ 它列出的命令集 = 前端能触达的后端能力的**全集**。
 * 前端业务代码一律走 `src/data/` 的接口层（业务不直接调 invoke），
 * 所以这个集合就是前端的全部能力面。
 */
function frontendBackendCommands(): string[] {
  const text = src("data/tauri.ts");
  const out = new Set<string>();
  // 形如：this.call<SomeType>("command_name", ...)
  const re = /this\.call<[^>]*>\(\s*"([a-z0-9_]+)"/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) out.add(m[1]!);
  // 还有不带泛型的：this.call("cmd")
  const re2 = /this\.call\(\s*"([a-z0-9_]+)"/g;
  while ((m = re2.exec(text))) out.add(m[1]!);
  return [...out].sort();
}

/**
 * 「能执行本地命令/进程」的命令名特征。
 *
 * ⚠️ **不能用宽泛的 /exec/ 匹配** —— `set_exec_shell`（设置里选 pwsh/gitbash）
 * 与 `exec_shell_status`（读 Git Bash 是否可用）名字里都有 exec，
 * 但它们**一个是写设置、一个是读状态**，都不执行任何东西。
 * ⇒ 这里只列**语义上就是执行**的命令名（denylist），宁可漏一个也不误伤。
 */
const EXEC_CAPABLE_COMMANDS = [
  "run_command",
  "exec_command",
  "execute_command",
  "cmd_run",
  "run_shell",
  "shell_exec",
  "spawn",
  "spawn_process",
  "start_process",
  "process_start",
  "run_process",
  "run_binary",
  "run_exe",
];

describe("守卫 · 前端碰不到内置 CLI（bin\\orbcat-perm.exe）", () => {
  it("① 前端不存在能执行本地命令/进程的调用（对着数据层命令表断言）", () => {
    const cmds = frontendBackendCommands();
    expect(cmds.length, "命令表不该是空的（扫描方式坏了）").toBeGreaterThan(40);

    // 断言 1：没有任何「执行类」命令在前端的可达集合里
    for (const bad of EXEC_CAPABLE_COMMANDS) {
      expect(
        cmds,
        `前端能触达「${bad}」⇒ 它就能执行本地命令 ⇒ 碰得到那个 CLI`,
      ).not.toContain(bad);
    }

    // 断言 2：前端**唯一**碰到 Tauri 的地方是数据层 ⇒ 业务代码无法绕过它自己 invoke
    const files = [
      "main.ts",
      "models.ts",
      "format.ts",
      "bubble.ts",
      "truncate.ts",
      "usage.ts",
      "commands.ts",
      "dom-alive.ts",
      "markdown.ts",
    ];
    for (const f of files) {
      expect(
        src(f).includes("@tauri-apps/api/core"),
        `${f} 直接 import 了 Tauri core ⇒ 业务逻辑绕过了接口层`,
      ).toBe(false);
    }
    // 数据层那一份是允许的（它就是那层接口）
    expect(src("data/tauri.ts").includes("@tauri-apps/api/core")).toBe(true);
  });

  it("①b 前端源码里没有那个 CLI 的路径（连引用都没有）", () => {
    const files = ["main.ts", "models.ts", "data/tauri.ts", "data/mock.ts", "data/mockdata.ts"];
    for (const f of files) {
      const text = src(f);
      expect(text.includes("orbcat-perm"), `${f} 引用了那个 CLI`).toBe(false);
    }
  });

  it("② 拖放进来的路径只填输入框，不会被当成命令执行", async () => {
    const P = "D:\\workplace\\悬浮小agent\\orbcat\\bin\\orbcat-perm.exe";
    await bootApp();
    await openPanel();

    emitDragDrop({ type: "drop", paths: [P] });
    await waitFor(() => !!document.getElementById("input"), 3000, "面板展开");
    await sleep(150);

    // ① 绝不能触发发送（那会把路径当成用户输入交给模型）
    expect(callsOf("chat").length, "拖放不该触发发送").toBe(0);
    expect(callsOf("chat_pi").length, "拖放不该触发发送").toBe(0);

    // ② 绝不能触发任何执行类命令
    for (const bad of EXEC_CAPABLE_COMMANDS) {
      expect(callsOf(bad).length, `拖放触发了执行命令 ${bad}`).toBe(0);
    }

    // ③ 路径只出现在**路径条**里（不是被塞进输入框当文本、也不是被执行）
    const chip = document.querySelector("#paths .path-chip code");
    expect(chip, "路径应进路径条").toBeTruthy();
    expect(chip!.textContent ?? "", "路径条里应有那个路径").toContain("orbcat-perm.exe");
  });

  it("③ 「用户没同意的命令」不会被当成可执行：自动放行默认最严 + 决定只能由用户点出来", async () => {
    // ① 执行权限档位默认 =「每次询问」（最严）—— 源码常量 + 界面显示
    expect(
      src("main.ts").includes('let execTrust: "ask" | "smart" | "full" = "ask";'),
      "执行权限默认必须是 ask（最严）",
    ).toBe(true);

    // ⚠️ pending 要在 **boot 之前**塞：前端启动时就会拉一次 `perm_requests_list`
    //    （main.ts 的 installPermHandlers 末尾那个「启动补拉」），
    //    boot 之后再塞就晚了 ⇒ 卡不会出现（这个坑我第一版就踩了）。
    const { state } = await import("./mocks/tauri");
    state.permPending = [
      {
        id: "pc1",
        kind: "command",
        path: "",
        access: "",
        deniedPath: null,
        widenLevels: null,
        widened: false,
        command: "rm -rf ./target/release",
        normalized: "rm --recursive --force ./target/release",
        risk: "high",
        headFp: "rm::--recursive",
        fullFp: "rm::--recursive::--force::./target/release",
        tier: "session",
        reason: "清理 Rust 编译产物",
        sessionId: "s-main",
        createdAt: Date.now() - 1000,
        timeoutMs: 120000,
        question: "",
        options: [],
      },
    ];

    await bootApp();
    await openPanel();
    await waitFor(() => !!document.querySelector(".perm-card"), 3000, "权限卡渲染");
    await sleep(150);

    // ② 启动时**不许**调 set_exec_trust（那会把用户没同意的档位写进去）
    expect(
      callsOf("set_exec_trust").length,
      "启动时不该自动改执行权限档位",
    ).toBe(0);

    // ③ 权限卡在时，**不点就不决定** —— 一个 decide 都不该发
    expect(
      callsOf("perm_request_decide").length,
      "没人点 ⇒ 不该有任何决定发出去",
    ).toBe(0);
  });
});
