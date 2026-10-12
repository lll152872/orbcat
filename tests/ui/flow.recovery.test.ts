import { describe, expect, it, beforeEach } from "vitest";
import { bootApp, openPanel, panelExists, waitFor, callsOf, state } from "./helpers";
import { setMock, resetMock } from "./mocks/tauri";

/**
 * 新增的两块前端行为：
 *  1. 设置 › 备份与回收站：有还原入口、能清理、能导诊断包
 *  2. 「又启动了一次 exe」应**打开面板**（曾报的 bug：点了图标没反应）
 *
 * 为什么这两块值得有测试：它们各自对应一个**用户可感知的失败**
 *  —— 归档还原不回去（只进不出）、点击图标无反应。
 * 这类 bug 单测后端函数是抓不到的，必须在真实 DOM 上走一遍。
 */

/** 打开设置 › 备份与回收站 */
async function openRecoveryView(): Promise<void> {
  await openPanel();
  document.getElementById("btn-gear")!.click();
  await waitFor(() => !!document.querySelector(".set-entry"), 3000, "settings entries");
  const entry = Array.from(document.querySelectorAll<HTMLButtonElement>(".set-entry")).find(
    (b) => b.dataset.act === "recovery",
  );
  if (!entry) throw new Error("设置里找不到「备份与回收站」入口");
  entry.click();
  await waitFor(
    () => !!document.querySelector(".rec-prune input"),
    3000,
    "recovery view inputs",
  );
}

beforeEach(() => {
  resetMock();
});

describe("设置 › 备份与回收站", () => {
  it("入口存在，且列出备份与回收站（含还原按钮）", async () => {
    setMock("recovery_state", {
      stats: { backupsCount: 1, backupsBytes: 2048, trashCount: 1, trashBytes: 4096, staleRecords: 0 },
      backups: [
        {
          id: "1790000000000__README.md",
          at: 1_790_000_000_000,
          original: "D:\\workplace\\demo\\README.md",
          backup: "D:\\workplace\\demo\\agent-data\\backups\\1790000000000__README.md",
          size: 2048,
          legacy: false,
        },
      ],
      trash: [
        {
          at: 1_790_000_000_001,
          original: "D:\\workplace\\demo\\gone.txt",
          trashed: "D:\\workplace\\demo\\agent-data\\.trash\\1790000000001__gone.txt",
          kind: "file",
          count: 1,
          present: true,
          size: 4096,
        },
      ],
      defaultKeepDays: 90,
      defaultKeepItems: 500,
    });

    await bootApp();
    await openRecoveryView();

    // 备份行：显示原路径尾部（用户要能认出"这是哪个文件"）
    expect(document.body.textContent).toContain("README.md");
    expect(document.body.textContent).toContain("demo");
    // 归档行
    expect(document.body.textContent).toContain("gone.txt");
    // 两个还原按钮
    expect(document.querySelectorAll(".rec-restore").length).toBe(1);
    expect(document.querySelectorAll(".rec-untrash").length).toBe(1);
    // 诊断包入口
    expect(document.getElementById("rec-diag")).toBeTruthy();
  });

  it("老备份（原路径未知）也列出来，但不假装知道它原来在哪", async () => {
    setMock("recovery_state", {
      stats: { backupsCount: 1, backupsBytes: 10, trashCount: 0, trashBytes: 0, staleRecords: 0 },
      backups: [
        {
          id: "1700000000000__old.txt",
          at: 1_700_000_000_000,
          original: "",
          backup: "D:\\demo\\agent-data\\backups\\1700000000000__old.txt",
          size: 10,
          legacy: true,
        },
      ],
      trash: [],
      defaultKeepDays: 90,
      defaultKeepItems: 500,
    });

    await bootApp();
    await openRecoveryView();

    expect(document.body.textContent).toContain("原路径未知");
    // 仍然给还原按钮（后端支持带 target 还原），但不能显示成"知道原路径"
    expect(document.querySelectorAll(".rec-restore").length).toBe(1);
  });

  it("点还原会调用后端命令", async () => {
    setMock("recovery_state", {
      stats: { backupsCount: 1, backupsBytes: 10, trashCount: 0, trashBytes: 0, staleRecords: 0 },
      backups: [
        {
          id: "b1",
          at: 1_790_000_000_000,
          original: "D:\\demo\\a.txt",
          backup: "D:\\demo\\agent-data\\backups\\b1",
          size: 10,
          legacy: false,
        },
      ],
      trash: [],
      defaultKeepDays: 90,
      defaultKeepItems: 500,
    });

    await bootApp();
    await openRecoveryView();

    (document.querySelector(".rec-restore") as HTMLButtonElement).click();
    await waitFor(
      () => callsOf("recovery_restore_backup").length > 0,
      3000,
      "restore_backup invoked",
    );
    const call = callsOf("recovery_restore_backup")[0] as { args?: { id?: string } };
    expect(call.args?.id).toBe("b1");
  });

  it("清理按钮把保留策略传给后端", async () => {
    await bootApp();
    await openRecoveryView();

    (document.getElementById("rec-days") as HTMLInputElement).value = "30";
    (document.getElementById("rec-items") as HTMLInputElement).value = "100";
    (document.getElementById("rec-prune") as HTMLButtonElement).click();

    await waitFor(() => callsOf("recovery_prune").length > 0, 3000, "prune invoked");
    const call = callsOf("recovery_prune")[0] as {
      args?: { keepDays?: number; keepItems?: number };
    };
    expect(call.args?.keepDays).toBe(30);
    expect(call.args?.keepItems).toBe(100);
  });

  it("导出诊断包走 recovery_diagnostic 命令", async () => {
    await bootApp();
    await openRecoveryView();

    (document.getElementById("rec-diag") as HTMLButtonElement).click();
    await waitFor(() => callsOf("recovery_diagnostic").length > 0, 3000, "diag invoked");
  });

  // 回归：曾报「在备份与回收站点『设置』直接跳到会话」。
  // 根因是 `#sub-back` 被绑了**两个** click handler —— `renderSubBar` 绑了一个
  // `switchView("settings")`，`bindRecoveryView` 又叠了一个。一次点击跑两遍，
  // 第二遍撞上 `switchView` 的 toggle（同一非 chat 视图再点 → 退回对话），
  // 返回键就变成了「回对话」。其余 8 个子页都有防复绑的注释，这里曾经是例外，
  // 所以这个视图也缺一条「返回真的回设置」的测试 —— 现在补上。
  it("顶部「‹ 设置」返回到设置入口页，不是对话页", async () => {
    await bootApp();
    await openRecoveryView();

    const back = document.querySelector<HTMLElement>("#sub-bar #sub-back");
    expect(back, "返回按钮应在固定的 #sub-bar 内").toBeTruthy();
    expect(back!.textContent).toContain("设置");
    back!.click();

    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "回到设置入口");
    // 对话页的输入框是「在设置页」和「在对话页」最快的区分点：
    // renderBody 把它 display 切成 none/flex。双绑时这里会是 flex（= 跳去了对话）。
    const input = document.querySelector<HTMLElement>(".panel-input");
    expect(input?.style.display, "不该回到对话页").toBe("none");
  });

  it("点安全闸门里的「去命令策略 ›」跳到命令策略页", async () => {
    // BUG-001 的可达性回归：这个 <a> 是安全闸门页**唯一**通往命令策略的路径
    // （设置入口把「文件权限」和「命令策略」并成了一行之后）。
    await bootApp();
    await openPanel();
    document.getElementById("btn-gear")!.click();
    await waitFor(() => !!document.querySelector(".set-entry"), 3000, "settings entries");
    const entry = Array.from(document.querySelectorAll<HTMLButtonElement>(".set-entry")).find(
      (b) => b.dataset.act === "perm",
    );
    if (!entry) throw new Error("设置里找不到「安全闸门」入口");
    entry.click();

    await waitFor(() => !!document.querySelector(".goto-cmdpolicy"), 3000, "安全闸门页");
    const link = document.querySelector<HTMLAnchorElement>(".goto-cmdpolicy")!;
    link.click();

    await waitFor(
      () => !!document.querySelector("#cp-reload"),
      3000,
      "命令策略页（#cp-reload 是它的刷新按钮）",
    );
  });
});

describe("「又启动了一次 exe」→ 打开面板", () => {
  it("后端留了待办标记时，启动即展开面板", async () => {
    // 模拟：第二实例在前端就绪之前发了信号 —— 只能靠这个标记兜底
    setMock("take_open_panel_request", true);

    // 注意：bootApp 会等 orb 渲染完；标记应当在之后被消费并展开面板
    await bootApp();
    await waitFor(() => panelExists(), 3000, "panel opened from pending flag");
    expect(document.getElementById("input")).toBeTruthy();
  });

  it("没有待办标记时保持球态（别无缘无故弹面板）", async () => {
    setMock("take_open_panel_request", false);
    await bootApp();
    // 给异步链路一点时间，确认它**不会**自己展开
    await new Promise((r) => setTimeout(r, 120));
    expect(panelExists()).toBe(false);
    expect(document.getElementById("orb")).toBeTruthy();
  });

  it("标记是 take 语义：消费一次后不应反复弹（后端 swap 由后端保证，这里守住不重复读）", async () => {
    setMock("take_open_panel_request", true);
    await bootApp();
    await waitFor(() => panelExists(), 3000, "panel opened");
    // 启动只该问一次
    expect(callsOf("take_open_panel_request").length).toBe(1);
    void state; // 保持 helpers 的 state 导入被使用（与其他测试文件一致）
  });
});
