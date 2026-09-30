/**
 * 设置 › 备份与回收站 —— 视图渲染与交互。
 *
 * ## 为什么单独成模块（2026-09-30 拆 main.ts）
 *
 * 这一整块（类型 + 渲染 + 事件绑定，约 240 行）是**自洽的**：只依赖
 * `invoke` 与三个格式化函数，不碰 main.ts 的任何模块状态。而它偏偏有
 * 一份独立的测试文件（`tests/ui/flow.recovery.test.ts`）——
 * 也就是说它的契约已经被测住，搬走的风险最低、收益又明确。
 *
 * ## 两处依赖改成注入（而不是 import main.ts）
 *
 * 反向 import（`views/recovery.ts` 去 import `main.ts`）会立刻造成循环依赖，
 * 而且会让这个模块重新耦合回 6000 行的主文件。所以：
 * - `header`：原本调 main 的 `subHeader("备份与回收站")`，现在由调用方传进来；
 * - `onReload`：原本调 main 的 `renderBody()` 重画整页，现在由调用方传进来。
 *
 * 两者都是"主文件才知道怎么做"的事（返回条要复用它的样式、重画要保住它的视图状态），
 * 让主文件提供是最自然的边界。
 */

import { invoke } from "@tauri-apps/api/core";
import { esc, fmtBytes, fmtWhen, tailPath } from "../format";

// ---------------- 设置 › 备份与回收站 ----------------

/**
 * 后端 `recovery::RecoveryState` 的镜像。
 *
 * 为什么要有这个视图（2026-09-30）：`backups/` 与 `.trash/` 在此之前**只进不出**
 * —— 实测 17.4 MB 备份 + 85.3 MB 归档，而还原只能靠用户手工搬文件。
 * 「可逆」是项目的底线承诺（决策 8 写的"30 天内可一键还原"），
 * 没有还原入口的归档等于半个硬删除。
 */
export interface BackupRec {
  id: string;
  at: number;
  /** 原路径。老备份（加清单之前）为空 → 前端显示"原路径未知"并要用户另选目标 */
  original: string;
  backup: string;
  size: number;
  legacy: boolean;
}

export interface TrashRec {
  at: number;
  original: string;
  trashed: string;
  kind: string;
  count: number;
  present: boolean;
  size: number;
}

export interface RecoveryStats {
  backupsCount: number;
  backupsBytes: number;
  trashCount: number;
  trashBytes: number;
  staleRecords: number;
}

export interface RecoveryState {
  stats: RecoveryStats;
  backups: BackupRec[];
  trash: TrashRec[];
  defaultKeepDays: number;
  defaultKeepItems: number;
}



/** 设置入口页那一行摘要 */
export function recoverySummaryText(st: RecoveryState | null): string {
  if (!st) return "读取中…";
  const b = st.stats.backupsCount;
  const t = st.stats.trashCount;
  if (b === 0 && t === 0) return "暂无备份与归档";
  const bytes = st.stats.backupsBytes + st.stats.trashBytes;
  return `${b} 份备份 · ${t} 项归档 · 共 ${fmtBytes(bytes)}`;
}


export function renderRecoveryView(st: RecoveryState, headerHtml: string): string {
  const s = st.stats;
  const total = s.backupsBytes + s.trashBytes;

  const cards = `
    <div class="usage-cards">
      <div class="usage-card"><b>${s.backupsCount}</b><i>备份 ${fmtBytes(s.backupsBytes)}</i></div>
      <div class="usage-card"><b>${s.trashCount}</b><i>归档 ${fmtBytes(s.trashBytes)}</i></div>
      <div class="usage-card"><b>${fmtBytes(total)}</b><i>合计占用</i></div>
    </div>`;

  const backupRows = st.backups.length
    ? st.backups
        .map((b) => {
          const where = b.legacy || !b.original
            ? `<i class="rec-unknown">原路径未知</i>`
            : `<i title="${esc(b.original)}">${esc(tailPath(b.original, 3))}</i>`;
          return `
          <div class="rec-row">
            <div class="rec-main">
              <span class="rec-name" title="${esc(b.backup)}">${esc(b.id)}</span>
              ${where}
            </div>
            <span class="rec-meta">${fmtWhen(b.at)} · ${fmtBytes(b.size)}</span>
            <button class="set-btn rec-restore" data-id="${esc(b.id)}">还原</button>
          </div>`;
        })
        .join("")
    : `<div class="mem-empty">还没有备份。<br>
         agent 每次写/改文件前都会备份原文件，这里就能一键还原。</div>`;

  const trashRows = st.trash.length
    ? st.trash
        .map((t) => {
          const label = `${tailPath(t.original, 3)}`;
          const dis = t.present ? "" : " disabled";
          return `
          <div class="rec-row${t.present ? "" : " rec-row--gone"}">
            <div class="rec-main">
              <span class="rec-name" title="${esc(t.original)}">${esc(label)}</span>
              <i>${t.kind === "dir" ? `目录（${t.count} 个文件）` : "文件"}${
                t.present ? "" : " · 已不在原处"
              }</i>
            </div>
            <span class="rec-meta">${fmtWhen(t.at)} · ${fmtBytes(t.size)}</span>
            <button class="set-btn rec-untrash" data-trashed="${esc(t.trashed)}"${dis}>还原</button>
          </div>`;
        })
        .join("")
    : `<div class="mem-empty">回收站是空的。<br>
         agent 删除文件时**永远只归档**到这里，不会硬删除。</div>`;

  return `
    ${headerHtml}
    ${cards}
    <div class="set-hint">
      <strong>还原是安全的</strong>：还原备份前会把目标文件的<strong>现状</strong>再备份一份，
      所以还原本身也能撤销。还原归档则<strong>拒绝覆盖</strong>已存在的目标 ——
      不会盖掉更新的内容。
    </div>

    <div class="mem-head">备份（写/改文件前自动产生）</div>
    <div class="rec-list">${backupRows}</div>

    <div class="mem-head">回收站（删除的文件都在这）</div>
    <div class="rec-list">${trashRows}</div>

    <div class="mem-head">保留策略</div>
    <div class="set-hint">
      清理**只在点按钮时执行**（不做后台自动删除 —— 后悔药不该被程序偷偷清掉）。
      两个条件<strong>同时满足</strong>才删：超过保留天数 <em>且</em> 超出条目上限。
    </div>
    <div class="rec-prune">
      <label>保留天数
        <input id="rec-days" type="number" min="1" max="3650" value="${st.defaultKeepDays}" />
      </label>
      <label>每类最多
        <input id="rec-items" type="number" min="1" max="100000" value="${st.defaultKeepItems}" />
        条
      </label>
      <button class="set-btn" id="rec-prune">立即清理</button>
    </div>
    <div id="rec-msg" class="set-hint"></div>

    <div class="mem-head">排查</div>
    <div class="set-hint">
      出问题时生成一份**诊断包**：日志尾部 + 审计链 + 会话里的错误条目 + 目录体积。
      模型 key 一律脱敏，可以直接转发。
    </div>
    <div class="rec-prune">
      <button class="set-btn" id="rec-diag">导出诊断包</button>
    </div>`;
}

export function bindRecoveryView(
  root: HTMLElement,
  onReload: () => void,
  onBack: () => void,
): void {
  document.getElementById("sub-back")?.addEventListener("click", () => void onBack());

  const setMsg = (msg: string, ok: boolean) => {
    const el = document.getElementById("rec-msg");
    if (el) {
      el.innerHTML = `<span class="${ok ? "rec-ok" : "rec-err"}">${esc(msg)}</span>`;
    }
  };

  /**
   * 重画本视图。
   *
   * 为什么不直接 `switchView("recovery")`：`switchView` 对**同一视图**有 toggle
   * 语义（再点一次退回对话），所以连点两次会跳到对话页 —— 这里必须绕开它，
   * 直接重渲染 body（`view` 不变）。
   */
  const reload = () => onReload();

  // 还原：成功失败都刷新（列表状态会变）
  root.querySelectorAll<HTMLButtonElement>(".rec-restore").forEach((b) =>
    b.addEventListener("click", async () => {
      const id = b.dataset.id!;
      b.disabled = true;
      try {
        const msg = await invoke<string>("recovery_restore_backup", { id, target: null });
        setMsg(msg.split("\n")[0], true);
      } catch (e) {
        setMsg(`还原失败：${e}`, false);
      }
      reload();
    }),
  );

  root.querySelectorAll<HTMLButtonElement>(".rec-untrash").forEach((b) =>
    b.addEventListener("click", async () => {
      const trashed = b.dataset.trashed!;
      b.disabled = true;
      try {
        const msg = await invoke<string>("recovery_restore_trash", { trashed, target: null });
        setMsg(msg, true);
      } catch (e) {
        // 典型失败：目标已存在（拒绝覆盖）—— 把原因原样告诉用户，别吞掉
        setMsg(`还原失败：${e}`, false);
      }
      reload();
    }),
  );

  document.getElementById("rec-prune")?.addEventListener("click", async () => {
    const days = Number((document.getElementById("rec-days") as HTMLInputElement | null)?.value);
    const items = Number((document.getElementById("rec-items") as HTMLInputElement | null)?.value);
    if (!Number.isFinite(days) || days < 1 || !Number.isFinite(items) || items < 1) {
      setMsg("保留天数与条数都必须是 ≥ 1 的整数", false);
      return;
    }
    try {
      const out = await invoke<{
        backupsRemoved: number;
        trashRemoved: number;
        bytesFreed: number;
      }>("recovery_prune", { keepDays: days, keepItems: items });
      const n = out.backupsRemoved + out.trashRemoved;
      if (n === 0) {
        setMsg("没有符合清理条件的项（要**超期且超量**才删）", true);
        return;
      }
      setMsg(
        `已清理 ${out.backupsRemoved} 份备份 + ${out.trashRemoved} 项归档，释放 ${fmtBytes(
          out.bytesFreed,
        )}`,
        true,
      );
      reload();
    } catch (e) {
      setMsg(`清理失败：${e}`, false);
    }
  });

  // 导出诊断包（2026-09-30）：出问题时用户此前只能截图红条，
  // 而现场散在 diag.log / 审计链 / 会话 error 条目 / 目录体积四处。
  document.getElementById("rec-diag")?.addEventListener("click", async () => {
    const btn = document.getElementById("rec-diag") as HTMLButtonElement | null;
    if (btn) btn.disabled = true;
    try {
      const path = await invoke<string>("recovery_diagnostic");
      setMsg(`诊断包已生成：${path}（key 已脱敏，可直接转发）`, true);
    } catch (e) {
      setMsg(`生成诊断包失败：${e}`, false);
    }
    if (btn) btn.disabled = false;
  });
}
