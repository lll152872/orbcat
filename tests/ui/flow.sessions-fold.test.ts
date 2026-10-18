/**
 * 会话页：组标题折叠 + 「无任务空间」组名
 *
 * ## 用户提的（2026-10-17）
 *
 * 「这个先把项目的折叠起来，然后主对话改为无任务空间的一个集合」
 * —— 配图上 3 组、几十条会话，找一条旧的要滚半天。
 *
 * ## 锁的五条
 *
 * 1. 组标题是**真 button**（能 Tab 能回车），点一下收/放本组；
 * 2. **默认只展开当前会话所在的那组**，其余全收起
 *    （第一版全展开 = 功能存在但默认没人用；用户原话
 *    「默认应该是折叠的吧，为啥全是展开的」）；
 * 3. 默认是**算出来的**不是存出来的 —— 切会话后自动跟着换；
 * 4. 显式点过的**不吃默认**：用户收掉当前会话那组，就让它收着；
 * 5. 折叠态**跨重渲染还在** —— 会话页每次进来都重画两轮
 *    （`session_state` 一轮、`pmem_session_map` 一轮），状态只在 DOM 上
 *    的话第二次重画就白折了。
 *
 * 另锁「无任务空间」组名（旧名「主对话 / 未挂靠」把两件事混成一句）。
 */
import { describe, it, expect } from "vitest";
import { bootApp, openPanel, waitFor, state, sleep } from "./helpers";
import { setMock } from "./mocks/tauri";

const PROJECTS = [
  { id: 1, name: "写作��业", rootPath: null, entryCount: 2, memoryChars: 80 },
  { id: 2, name: "论文", rootPath: null, entryCount: 1, memoryChars: 40 },
];

/** 顶栏「史」→ 会话页 */
async function openSessions(): Promise<void> {
  (document.getElementById("btn-sess") as HTMLButtonElement).click();
  await waitFor(() => !!document.getElementById("sess-new"), 3000, "sessions view");
}

/** 按 `data-fold` 找组标题 */
function sepOf(key: string): HTMLButtonElement {
  const sep = Array.from(document.querySelectorAll<HTMLButtonElement>(".sess-sep")).find(
    (b) => (b.dataset.fold ?? "") === key,
  );
  if (!sep) throw new Error(`找不到组「${key}」的标题`);
  return sep;
}

/**
 * 组容器 —— 折叠用 `hidden` 属性，**不删节点**。
 *
 * ⚠️ 顺带锁住「折叠必须真的藏起来」：happy-dom 不做样式计算，
 * `.sess-group[hidden]{display:none}` 那条 CSS 在这里测不出来（见 styles.css 的注释）。
 * 能测的契约是 **`hidden` 属性被设上** —— 真浏览器里那条 CSS 靠的就是它。
 */
function groupOf(key: string): HTMLElement {
  const next = sepOf(key).nextElementSibling as HTMLElement | null;
  if (!next?.classList.contains("sess-group")) throw new Error(`组「${key}」的容器丢了`);
  return next;
}

function stageThreeGroups(): void {
  state.sessionList = [
    { id: state.currentSessionId, title: "test", kind: "main", updatedAt: Date.now() },
    { id: "sess-a", title: "随手聊的", kind: "task", updatedAt: Date.now() - 1_000 },
    { id: "sess-xw", title: "作文那条", kind: "task", updatedAt: Date.now() - 2_000 },
    { id: "sess-ly", title: "论文那条", kind: "task", updatedAt: Date.now() - 3_000 },
  ];
  setMock("pmem_list", PROJECTS);
  setMock("pmem_get_active", null);
  setMock("pmem_session_map", { "sess-xw": "写作作业", "sess-ly": "论文" });
}

/** 回到对话页 —— 判据是**会话页没了**，不能拿 `#input` 判。
 *
 *  ⚠️ `#input` 在**每一页都存在**（非对话页只是 `.panel-input` 被 `display:none`），
 *     拿它当"回到对话了"的判据会**立刻通过**，于是跟上一步的
 *     `await switchSession()` 赛跑 —— `switchSession` 还没跑到 `view = "chat"`
 *     就被点了「史」，`switchView` 的 toggle 把它弹回对话，表现成
 *     「会话页永远进不去」。踩过一次，记在这儿。 */
async function backToChat(): Promise<void> {
  await waitFor(() => !document.getElementById("sess-new"), 3000, "left sessions view");
}

describe("默认只展开当前会话所在的那一组", () => {
  it("进来时：当前会话那组展开，其余全收起", async () => {
    stageThreeGroups(); // 当前会话 = state.currentSessionId = 主聊天「test」→ 落在「无任务空间」
    await bootApp();
    await openPanel();
    await openSessions();

    expect(groupOf("").hidden, "当前会话所在那组默认展开").toBe(false);
    expect(sepOf("").getAttribute("aria-expanded")).toBe("true");
    expect(groupOf("论文").hidden, "别的组默认收起（第一版全展开 = 功能白做）").toBe(true);
    expect(groupOf("写作作业").hidden).toBe(true);
  });

  it("切到别的会话 → 展开跟着走（旧组自动收、新组自动开）", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();
    expect(groupOf("论文").hidden).toBe(true);

    // 手动展开「论文」并切进去 —— 「写作作业」没点过，该收仍收
    sepOf("论文").click();
    expect(groupOf("论文").hidden).toBe(false);
    document.querySelector<HTMLButtonElement>('.sess-open[data-id="sess-ly"]')!.click();
    await backToChat();

    await openSessions();
    await sleep(50);
    expect(groupOf("论文").hidden, "现在轮到它是当前会话所在组 → 展开").toBe(false);
    expect(groupOf("写作作业").hidden, "没点过的组仍按默认收起（不必手动搬状态）").toBe(true);
  });

  it("显式收掉当前会话那组 → 保持收着（不吃默认）", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    sepOf("").click(); // 收掉当前会话所在的那一组
    expect(groupOf("").hidden).toBe(true);

    // 离开再回来 —— 默认规则会想把它展开，但显式设置优先
    (document.getElementById("sub-back") as HTMLButtonElement).click();
    await backToChat();
    await openSessions();
    await sleep(50);
    expect(groupOf("").hidden, "用户说收就收，哪怕那是当前会话所在组").toBe(true);
  });
});

describe("组标题折叠", () => {
  it("组标题是真 button，点一下收起本组、再点展开", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    const sep = sepOf("论文");
    expect(sep.tagName, "必须是 button（Tab/回车可达，不能是纯 div）").toBe("BUTTON");
    expect(groupOf("论文").hidden, "默认收起").toBe(true);

    sep.click();
    expect(sep.getAttribute("aria-expanded")).toBe("true");
    expect(sep.classList.contains("folded")).toBe(false);
    expect(groupOf("论文").hidden, "点一下该组会话要展开").toBe(false);
    // ⚠️ **只藏不删**：藏的时候节点仍在，展开不用重拉数据
    expect(document.querySelector('.sess-open[data-id="sess-ly"]')).toBeTruthy();
    expect(sep.querySelector(".sess-caret")?.textContent).toBe("▾");

    sep.click();
    expect(sep.getAttribute("aria-expanded")).toBe("false");
    expect(sep.classList.contains("folded")).toBe(true);
    expect(groupOf("论文").hidden).toBe(true);
    expect(sep.querySelector(".sess-caret")?.textContent).toBe("▸");
  });

  it("折叠互不影响：收「论文」不碰「写作作业」", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    sepOf("论文").click(); // 展开
    sepOf("论文").click(); // 再收回去
    expect(groupOf("论文").hidden).toBe(true);
    expect(groupOf("写作作业").hidden).toBe(true);
    // 无任务空间那组（当前会话所在）从没被点过，仍展开
    expect(groupOf("").hidden, "无任务空间那组不该被动").toBe(false);
  });

  it("折叠态跨重渲染还在（会话页每次进来都重画两轮）", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    sepOf("").click(); // 显式收掉当前会话那组（默认规则不会这么干，只能靠显式设置）
    expect(groupOf("").hidden).toBe(true);

    // 离开再回来 —— 会话页会重新拉 session_state + pmem_session_map 重画两轮
    (document.getElementById("sub-back") as HTMLButtonElement).click();
    await backToChat();
    await openSessions();
    await sleep(50);

    expect(groupOf("").hidden, "显式状态存在模块里（不是 DOM 上），重画后仍然折叠").toBe(true);
  });

  it("收起状态下仍能进对话（折叠≠禁用）", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    // 「论文」默认就是收的 —— 组藏了但节点还在
    expect(groupOf("论文").hidden).toBe(true);
    const row = document.querySelector<HTMLButtonElement>('.sess-open[data-id="sess-ly"]');
    expect(row, "折叠不能把会话行摘掉").toBeTruthy();
  });
});

describe("「无任务空间」组", () => {
  it("没挂项目的会话归到「无任务空间」，主聊天也在里面", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    const sep = sepOf("");
    expect(sep.textContent ?? "").toContain("无任务空间");
    expect(sep.textContent ?? "").not.toContain("未挂靠", "旧名把两件事混成一句");
    // 这一组装的是「主聊天 + 没挂项目的任务会话」
    expect(document.querySelector('.sess-open[data-id="sess-a"]')).toBeTruthy();
    expect(document.querySelector('.sess-del[data-id="sess-a"]')).toBeTruthy();
  });

  it("空项目会话的那一组标题带 📁 与项目名", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();
    expect(sepOf("论文").textContent ?? "").toContain("论文");
    expect(sepOf("写作作业").textContent ?? "").toContain("写作作业");
  });
});

describe("折叠不能破坏既有的组标题右键重命名", () => {
  it("组标题仍是 .sess-sep[data-pname]（右键菜单靠它认人）", async () => {
    stageThreeGroups();
    await bootApp();
    await openPanel();
    await openSessions();

    const sep = sepOf("论文");
    expect(sep.dataset.pname, "改 <div>→<button> 时别把 data-pname 弄丢").toBe("论文");
    expect(sep.dataset.pid).toBe("2");
    // 无任务空间那组没有 data-pname（它不是项目，右键无从重命名）
    expect(sepOf("").dataset.pname).toBeUndefined();
  });
});