/**
 * 顶栏项目 ↔ 当前会话的一致性 —— 2026-10-17 用户报
 *
 * ## 用户原话
 *
 * 「这个不匹配，这个是 test 的但是上面显示的是写作业，问题是我先选项目之后
 *   直接返回对话，就会出现不匹配现在」
 *
 * 现场：当前会话是 `test`（无项目），顶栏选「写作业」→ 跳去会话页 →
 * **没点任何会话**就点「‹ 返回对话」→ 回到的还是 test 会话，顶栏却挂着「写作业」。
 *
 * ## 为什么这不是标题小事
 *
 * 后端 `active_project_id` 决定三件事（见 `alignProjectToSession` 的注释）：
 * 注入哪份项目记忆 / 蒸馏往哪落 / 执行期给哪个目录开读写授权。
 * 所以「顶栏写 A、聊着 B」= 记忆与权限一起串到 B 之外的地方去。
 *
 * 三条锁：
 * 1. **选项目直接落到该项目最新那条会话**（不再停在会话页等人点）；
 * 2. **任何一次进对话都把顶栏拉回当前会话的项目**（兜底，含冷启动那条路径）；
 * 3. 一致时**不**白写 `pmem_set_active`（`renderBody` 每条消息都跑，不能有副作用）。
 */
import { describe, it, expect, beforeEach } from "vitest";
import { bootApp, openPanel, waitFor, callsOf, state, sleep } from "./helpers";
import { setMock } from "./mocks/tauri";

/** 两个项目：写作业 / 论文 */
const PROJECTS = [
  { id: 1, name: "写作业", rootPath: null, entryCount: 3, memoryChars: 100 },
  { id: 2, name: "论文", rootPath: null, entryCount: 1, memoryChars: 50 },
];

function select(): HTMLSelectElement {
  const el = document.getElementById("project-select") as HTMLSelectElement | null;
  if (!el) throw new Error("#project-select 不存在");
  return el;
}

/** 顶栏那颗 select 当前显示的项目名（`""` = 主对话） */
function shownProject(): string {
  const s = select();
  const opt = s.options[s.selectedIndex];
  return (opt?.textContent ?? "").replace(/^📁\s*/, "");
}

/** 当前对话页（不是会话/设置页） */
function inChat(): boolean {
  return !!document.getElementById("input") && !document.getElementById("sess-new");
}

/**
 * 摆出现场：`test` 是当前会话且**无项目**；「写作业」下另有一条会话。
 * `sess-xw` 挂在项目 1（写作业）上。
 */
function stageUnmatchedScene(): void {
  state.sessionList = [
    { id: state.currentSessionId, title: "test", kind: "main", updatedAt: Date.now() },
    { id: "sess-xw", title: "写作业那条", kind: "task", updatedAt: Date.now() - 60_000 },
    { id: "sess-ly", title: "论文那条", kind: "task", updatedAt: Date.now() - 120_000 },
  ];
  setMock("pmem_list", PROJECTS);
  setMock("pmem_get_active", null);
  setMock("pmem_session_map", { "sess-xw": "写作业", "sess-ly": "论文" });
}

/** 顶栏选中某个项目并触发 change */
async function pickProject(name: string): Promise<void> {
  const s = select();
  const opt = Array.from(s.options).find((o) => (o.textContent ?? "").includes(name));
  if (!opt) throw new Error(`顶栏没有「${name}」这一项`);
  s.value = String((opt as HTMLOptionElement).value);
  s.dispatchEvent(new Event("change"));
  await sleep(30);
}

describe("选项目 → 落到那个项目的会话（2026-10-17 不匹配）", () => {
  beforeEach(() => stageUnmatchedScene());

  it("选「写作业」→ 直接切到它那条会话，顶栏与内容一致", async () => {
    await bootApp();
    await openPanel();
    expect(shownProject(), "起点应是主对话（当前会话没挂项目）").toBe("主对话");

    await pickProject("写作业");
    await waitFor(() => inChat(), 3000, "back in chat with 写作业's session");

    const sw = callsOf("session_switch");
    expect(sw.length, "应该真切了会话（不是停在会话页等人点）").toBe(1);
    expect(sw[0].args?.id).toBe("sess-xw");
    expect(shownProject()).toBe("写作业");
    // 落进去的确实是那条会话的标题（不是还停在 test）
    expect(document.body.textContent ?? "").not.toContain("主聊天");
  });

  it("选「论文」→ 落到论文那条（不是「第一个项目」）", async () => {
    await bootApp();
    await openPanel();
    await pickProject("论文");
    await waitFor(() => inChat(), 3000, "switched to 论文 session");
    expect(callsOf("session_switch")[0].args?.id).toBe("sess-ly");
    expect(shownProject()).toBe("论文");
  });

  it("项目里一条会话都没有 → 停在会话页，不硬造会话", async () => {
    setMock("pmem_list", [{ id: 9, name: "空项目", rootPath: null, entryCount: 0, memoryChars: 0 }]);
    setMock("pmem_session_map", {});
    await bootApp();
    await openPanel();
    await pickProject("空项目");
    await waitFor(() => !!document.getElementById("sess-new"), 3000, "sessions view");
    expect(callsOf("session_switch").length, "没有会话就不该凭空切一条").toBe(0);
    expect(shownProject()).toBe("空项目");
  });
});

describe("兜底：任何一次进对话，顶栏都回到当前会话的项目", () => {
  beforeEach(() => stageUnmatchedScene());

  it("停在会话页时点「返回对话」→ 顶栏回到当前会话的项目", async () => {
    // 兜底这条路径要一个「选完项目仍停在会话页」的场景 = 项目里一条会话都没有。
    // （项目里有会话时，选项目会直接落进那条会话 —— 那是上一组用例锁的。）
    setMock("pmem_list", [{ id: 7, name: "空项目", rootPath: null, entryCount: 0, memoryChars: 0 }]);
    setMock("pmem_session_map", {});
    await bootApp();
    await openPanel();
    await pickProject("空项目");
    await waitFor(() => !!document.getElementById("sess-new"), 3000, "sessions view");
    expect(shownProject(), "会话页上顶栏显示用户刚选的项目").toBe("空项目");

    // ⚠️ 用户现场：不点任何会话，直接返回对话
    (document.getElementById("sub-back") as HTMLButtonElement).click();
    await waitFor(() => inChat(), 3000, "back to chat");

    expect(shownProject(), "顶栏必须与当前会话一致（当前会话没挂项目）").toBe("主对话");
    // 后端也被拉回来了 —— 否则下一轮仍会注入那个项目的记忆与目录授权
    const sets = callsOf("pmem_set_active");
    expect(sets.length, "应补写一次激活项目").toBeGreaterThan(0);
    expect(sets[sets.length - 1].args?.id).toBeNull();
  });

  it("冷启动：settings 里存着别的项目、当前会话没挂 → 一样被拉回来", async () => {
    // 上次退出前选了「写作业」，这次启动当前会话却是无项目的 test
    setMock("pmem_get_active", { id: 1, name: "写作业" });
    await bootApp();
    await openPanel();
    await waitFor(() => shownProject() === "主对话", 3000, "aligned on open");
    const sets = callsOf("pmem_set_active");
    expect(sets[sets.length - 1].args?.id).toBeNull();
  });

  it("当前会话就属于该项目 → 顶栏显示它，且不多写 pmem_set_active", async () => {
    // 当前会话就是「论文」那条
    state.currentSessionId = "sess-ly";
    setMock("pmem_get_active", { id: 2, name: "论文" });
    await bootApp();
    await openPanel();
    expect(shownProject()).toBe("论文");
    expect(
      callsOf("pmem_set_active").length,
      "已经一致就别白写 settings.json（renderBody 每条消息都跑）",
    ).toBe(0);
  });

  it("当前会话的项目被删了（名反查不到）→ 当作无项目，不报错", async () => {
    setMock("pmem_session_map", { [state.currentSessionId]: "已删的项目" });
    setMock("pmem_list", []);
    await bootApp();
    await openPanel();
    await waitFor(() => inChat(), 2000, "chat still fine");
    expect(shownProject()).toBe("主对话");
  });
});