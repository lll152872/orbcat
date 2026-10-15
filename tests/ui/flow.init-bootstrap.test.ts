import { describe, expect, it, beforeEach } from "vitest";
import { bootApp, openPanel, waitFor, callsOf } from "./helpers";
import { setMock, resetMock } from "./mocks/tauri";

/**
 * 未初始化引导 —— 用户可见面回归
 *
 * ## 为什么单开一个文件
 *
 * 2026-10-12 用户反馈：「为什么我没有初始化按键，这个文件夹为啥建立不出来」。
 *
 * 查下来是 `boot_state` 的判据写错了：
 *
 *     let needs_init = !state.data_dir.exists();   // ← 错
 *
 * `data_dir` 在发布版是 `%APPDATA%\com.orbcat.desktop`，而程序**第一次启动**
 * 就会往里面写 `diag.log` / `permissions.json` / `settings.json` / `pmem.sqlite`
 * —— 目录被这些文件创建了，骨架（memory / pending / projects / skills）却一个
 * 都没铺。于是 `!exists()` 恒为 false，「建出目录结构」按钮**永远不出现**。
 *
 * 判据已改成查 `.skeleton-state.json`（bootstrap 铺完才写的状态文件）。
 *
 * ## 为什么这个 bug 只能靠 UI 测试守
 *
 * Rust 侧的 `lib.rs` 没有 `#[cfg(test)] mod tests`（它是 tauri 的
 * setup/builder 装配层，跑测试要起整个 app），而 `boot_state` 正是一个
 * `#[tauri::command]` 定义在那 —— 也就是说**改回旧判据的话，Rust 测试全绿，
 * 只有这里会红**。没有下面这组用例，这个回归只能靠人工发现。
 */

/** 让 boot_state 返回「未初始化」：dataDir 下没有 .skeleton-state.json */
function mockNotInitialized(): void {
  setMock("boot_state", {
    needsInit: true,
    hasModels: false,
    dataDir: "C:\\Users\\someone\\AppData\\Roaming\\com.orbcat.desktop",
  });
}

/** 已初始化且有模型：正常态，不该看到任何引导 */
function mockReady(): void {
  setMock("boot_state", {
    needsInit: false,
    hasModels: true,
    dataDir: "C:\\Users\\someone\\AppData\\Roaming\\com.orbcat.desktop",
  });
}

beforeEach(() => {
  resetMock();
});

describe("未初始化时给出「建出目录结构」按钮", () => {
  it("needsInit=true → 空态显示初始化按钮，文案说清楚不需要模型", async () => {
    mockNotInitialized();
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.querySelector(".empty"), 3000, "empty state");

    const btn = document.getElementById("init-go") as HTMLButtonElement | null;
    expect(btn, "未初始化时必须给出 init-go 按钮").toBeTruthy();
    expect(btn!.textContent).toContain("建出目录结构");
    // 文案必须说明这一步不需要模型 —— 否则用户会以为必须先配模型
    const hint = document.querySelector(".init-hint");
    expect(hint?.textContent).toContain("不需要模型");
  });

  it("点「建出目录结构」真的调 bootstrap_data（不是死按钮）", async () => {
    mockNotInitialized();
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.getElementById("init-go"), 3000, "init button");

    document.getElementById("init-go")!.click();
    await waitFor(
      () => callsOf("bootstrap_data").length > 0,
      3000,
      "bootstrap_data invoked",
    );
    expect(callsOf("bootstrap_data").length).toBe(1);
  });

  // 反向对照：判据太松（比如只看目录在不在）会让这个按钮**消失**，
  // 而 Rust 侧全绿。这条就是唯一能拦住"按钮不见了"的用例。
  it("已初始化 + 有模型 → 不显示初始化按钮", async () => {
    mockReady();
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.querySelector(".empty"), 3000, "empty state");

    expect(document.getElementById("init-go")).toBeNull();
    expect(document.querySelector(".init-title")).toBeNull();
  });
});

describe("有目录但没模型时引导去加模型（不是再去初始化）", () => {
  it("needsInit=false + hasModels=false → 按钮指向设置，不指向建目录", async () => {
    setMock("boot_state", {
      needsInit: false,
      hasModels: false,
      dataDir: "C:\\Users\\someone\\AppData\\Roaming\\com.orbcat.desktop",
    });
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.getElementById("init-go"), 3000, "init button");

    const btn = document.getElementById("init-go") as HTMLButtonElement;
    expect(btn.textContent).toContain("去设置里添加模型");
    expect(btn.textContent).not.toContain("建出目录结构");
  });
});

/**
 * 文案不许说假话（2026-10-14 用户实测）
 *
 * `agent-data/` 由 `resolve_data_dir` 在**启动时就已经建出来了**，
 * 而判据是骨架标记 `.skeleton-state.json` 在不在。两者不是一回事，
 * 于是这屏长期喊着一句假话「数据目录 agent-data/ 还不存在」——
 * 而用户那份数据（60 个会话、2.5MB 审计日志）明明就在那儿躺着。
 *
 * 这里锁住「文案跟着判据走」：目录存在就该说"是空的"，不是"不存在"。
 */
describe("初始化文案跟着判据走，不喊「agent-data 不存在」", () => {
  it("dataDirExists=true → 说「里还是空的」", async () => {
    setMock("boot_state", {
      needsInit: true,
      hasModels: false,
      dataDir: "C:\\Users\\someone\\AppData\\Roaming\\com.orbcat.desktop",
      dataDirExists: true,
    });
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.getElementById("init-go"), 3000, "init button");

    const body = document.querySelector(".init-body")?.textContent ?? "";
    expect(body).toContain("里还是空的");
    expect(body).not.toContain("还不存在");
  });

  it("dataDirExists=false → 才允许说「还不存在」", async () => {
    setMock("boot_state", {
      needsInit: true,
      hasModels: false,
      dataDir: "D:\\nowhere\\agent-data",
      dataDirExists: false,
    });
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.getElementById("init-go"), 3000, "init button");

    expect(document.querySelector(".init-body")?.textContent ?? "").toContain("还不存在");
  });

  it("老后端没有 dataDirExists 字段时也别崩、也不说「不存在」", async () => {
    mockNotInitialized();
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.getElementById("init-go"), 3000, "init button");

    const body = document.querySelector(".init-body")?.textContent ?? "";
    expect(body).not.toContain("还不存在");
  });

  it("按钮旁必须保证「不覆盖已有文件」", async () => {
    mockNotInitialized();
    await bootApp();
    await openPanel();
    await waitFor(() => !!document.getElementById("init-go"), 3000, "init button");

    // 用户被吓到不敢点，多半是因为不知道会不会毁掉自己的数据
    expect(document.querySelector(".init-hint")?.textContent ?? "").toContain(
      "不会被覆盖",
    );
  });
});