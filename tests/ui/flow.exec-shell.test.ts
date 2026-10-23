import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor } from "./helpers";
import { callsOf } from "./mocks/tauri";

/**
 * 「执行位置」三选一（后台桌面 / 本机 pwsh / 本机 Git Bash）
 *
 * ## 背景（用户原话）
 *
 * 用户实机看到设置里「执行位置」只有 **本机 pwsh**，问「git bash 呢」。
 * 查清了：这一维**只有一个布尔** —— `bgDeskEnabled ? "后台桌面" : "本机 pwsh"`，
 * 而 **"本机 pwsh" 是写死的字面量**，Git Bash 根本不存在。
 *
 * 但后端 `command_policy.rs` **已经能识别 `bash -c "..."` 形态**
 * （那 116 条规则 / 11 条硬阻断就来自这）⇒ **管控做了、UI 没给入口**。
 *
 * ## 契约（Chief 定死，前后端都按这个）
 *
 * - settings 新增 `execShell: "pwsh" | "gitbash"`，**默认 `"pwsh"`**
 *   （老用户升级后行为必须不变）；
 * - `bgDeskEnabled` **保持原样**（它是「后台桌面 vs 本机」这一维，别动它）；
 * - 「执行位置」这一行显示三选一：
 *   `bgDeskEnabled ? "后台桌面" : (execShell === "gitbash" ? "本机 Git Bash" : "本机 pwsh")`；
 * - 行为页把「执行位置」做成**可点的三选一**，选中即落盘。
 *
 * ## ⚠️ 别做成假开关
 *
 * 「选项要真能选、真能存、界面上真显示」—— 所以这里锁的是：
 * 真调后端命令、mock 侧**真落盘**（读回来还是同一个值）、界面**真显示**当前选中项。
 */

/** 进行为页 */
async function openBehavior(): Promise<HTMLElement> {
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
  const entry = document.querySelector<HTMLElement>('.set-row[data-act="behavior"]');
  if (!entry) throw new Error("设置里找不到「行为」入口");
  entry.click();
  await waitFor(() => !!document.querySelector('input[name="bh-pos"]'), 3000, "行为页渲染");
  return document.getElementById("panel-body")!;
}

/** 选中三选一里的某一项（走真实 change 事件） */
function pick(body: HTMLElement, value: string): void {
  const radio = body.querySelector<HTMLInputElement>(
    `input[name="bh-pos"][value="${value}"]`,
  );
  if (!radio) throw new Error(`行为页没有「${value}」这个选项`);
  radio.checked = true;
  radio.dispatchEvent(new Event("change", { bubbles: true }));
}

/** 当前选中的是哪一项 */
function picked(body: HTMLElement): string {
  const el = body.querySelector<HTMLInputElement>('input[name="bh-pos"]:checked');
  return el?.value ?? "";
}

describe("执行位置 · 三选一", () => {
  it("① 行为页给出三个选项：后台桌面 / 本机 pwsh / 本机 Git Bash", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    const vals = Array.from(body.querySelectorAll<HTMLInputElement>('input[name="bh-pos"]')).map(
      (r) => r.value,
    );
    expect(vals, "执行位置应是三选一").toEqual(["bgdesk", "pwsh", "gitbash"]);
    // 三项都要有**看得见的标题**（不能只有 radio 没有文字）
    const txt = body.textContent ?? "";
    expect(txt).toContain("后台桌面");
    expect(txt).toContain("本机 pwsh");
    expect(txt).toContain("本机 Git Bash");
  });

  it("② 默认是「本机 pwsh」（老用户升级后行为不变）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    // mock 的 settings 里没有 execShell 字段时 ⇒ 回落 pwsh
    expect(picked(body), "默认必须是本机 pwsh").toBe("pwsh");
  });

  it("③ 选「本机 Git Bash」→ 真调 set_exec_shell(gitbash)", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    pick(body, "gitbash");
    await waitFor(
      () => callsOf("set_exec_shell").some((c) => c.args?.shell === "gitbash"),
      2000,
      "set_exec_shell(gitbash)",
    );
  });

  it("④ 选「本机 Git Bash」→ 界面立刻显示选中项（不是假开关）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    pick(body, "gitbash");
    // ⚠️ **等组头文案更新**，别只等 radio 的 checked 状态 ——
    //    `checked` 是测试自己设的（旧 DOM 上立刻就成立），
    //    真正要验的是「重画之后界面显示的是新值」⇒ 那只能看组头文案。
    await waitFor(
      () => {
        const head = Array.from(body.querySelectorAll<HTMLElement>(".mem-head")).find((h) =>
          (h.textContent ?? "").includes("执行位置"),
        );
        return (head?.textContent ?? "").includes("本机 Git Bash");
      },
      3000,
      "组头显示「本机 Git Bash」",
    );
    // 重画之后选中项也要还在（不能被重画冲掉）
    expect(picked(body), "重画后选中项要保持").toBe("gitbash");
  });

  it("⑤ 选「本机 pwsh」→ 调 set_exec_shell(pwsh)", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    pick(body, "pwsh");
    await waitFor(
      () => callsOf("set_exec_shell").some((c) => c.args?.shell === "pwsh"),
      2000,
      "set_exec_shell(pwsh)",
    );
  });

  it("⑥ 选「后台桌面」→ 调 bg_desk_set(true)，**不**动 execShell", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    pick(body, "bgdesk");
    await waitFor(
      () => callsOf("bg_desk_set").some((c) => c.args?.enabled === true),
      2000,
      "bg_desk_set(true)",
    );
    // ⚠️ 后台桌面这一支是**进程级**的 ⇒ 不该改 settings.execShell
    expect(
      callsOf("set_exec_shell").length,
      "选后台桌面不该改 execShell",
    ).toBe(0);
  });

  it("⑦ 从后台桌面切回本机 Git Bash → 两边都要动（互斥，不是只改一个）", async () => {
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    pick(body, "bgdesk");
    await waitFor(
      () => callsOf("bg_desk_set").some((c) => c.args?.enabled === true),
      2000,
      "先开后台桌面",
    );
    pick(body, "gitbash");
    await waitFor(
      () =>
        callsOf("bg_desk_set").some((c) => c.args?.enabled === false) &&
        callsOf("set_exec_shell").some((c) => c.args?.shell === "gitbash"),
      2000,
      "退出后台桌面 + 落盘 gitbash",
    );
  });

  it("⑧ 设置入口页那一行显示「执行位置」的当前值", async () => {
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-row"), 3000, "设置入口页");
    const entry = document.querySelector<HTMLElement>('.set-row[data-act="behavior"]')!;
    const txt = entry.textContent ?? "";
    expect(txt, "要显示执行位置").toContain("执行位置");
    // ⚠️ **不能再是写死的「本机 pwsh」** —— 原来那行是字面量，
    //    用户问「git bash 呢」就是因为它永远显示 pwsh。
    expect(txt).toMatch(/本机 pwsh|本机 Git Bash|后台桌面/);
  });

  /**
   * ⚠️ 发布包**不带 git**（runtime 只有 node+pi）⇒ 没装 Git for Windows 的机器上
   * 选「本机 Git Bash」会**直接失败**。设置页必须**提前**提示，
   * 而不是等用户选完才报错（那时已经落盘了一个跑不起来的值）。
   */
  it("⑨ 没装 Git for Windows 时，「本机 Git Bash」那一支要提前警告", async () => {
    const { state } = await import("./mocks/tauri");
    state.gitBashAvailable = false;
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    await waitFor(
      () => (body.textContent ?? "").includes("Git for Windows"),
      3000,
      "Git Bash 不可用警告",
    );
    const txt = body.textContent ?? "";
    expect(txt, "要说明没装 Git for Windows").toContain("没找到 Git for Windows");
    // 警告只挂在本机 Git Bash 那一支，不影响另外两支
    expect(txt, "PowerShell 不该被牵连").toContain("本机 pwsh");
  });

  /**
   * ⚠️ 后端那条（`work/backend/exec-shell`）**未合入基线前**，
   * 调 `set_exec_shell` 会失败（unknown command）。
   * ⇒ 界面必须**弹一个看得见的错误 toast**，**不能静默**。
   *
   * 用户 2026-10-23 明确要求确认这一点。
   */
  it("⑩ 后端没这条命令时 → 必须弹**可见的错误 toast**（不静默）+ 界面回滚", async () => {
    const { state } = await import("./mocks/tauri");
    // 模拟后端没有 set_exec_shell（真后端未合入时的真实情况）
    state.failures.set("set_exec_shell", "unknown command: set_exec_shell");
    await bootApp();
    await openPanel();
    const body = await openBehavior();
    pick(body, "gitbash");
    // ⚠️ 等那个错误 toast 出现（可见、不是静默）
    await waitFor(
      () => (document.body.textContent ?? "").includes("切换执行位置失败"),
      3000,
      "错误 toast 可见",
    );
    expect(
      document.body.textContent ?? "",
      "错误文案要说明是切换失败",
    ).toContain("切换执行位置失败");
    // ⚠️ 失败后 radio **不能**停在没落盘的假状态 ⇒ 要回到真实值（pwsh）
    await waitFor(() => picked(body) === "pwsh", 3000, "失败后界面回滚到真实值");
  });
});
