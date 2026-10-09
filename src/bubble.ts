/**
 * 把一条 assistant 正文切成「聊天气泡」（2026-10-08）。
 *
 * ## 为什么要切
 *
 * 闲聊模式下一条长回复读起来像**文档**，不像聊天 —— 人在 QQ 里是**连着发几条短消息**的。
 * 用户要求"像 QQ 那样可以多次发消息"，做法抄业界（Telegram/Discord 的 LLM bot、
 * ChatLab 的 Human-Like Messaging、bubble-splitter、openhuman 的 messageSegmentation）：
 * **模型照常输出一条正文，客户端切成气泡逐条显示**。
 *
 * 为什么不用"发消息工具"：那样每发一条要多一次模型往返（3 条 = 3 次 API 调用），
 * 闲聊最怕这个延迟；而且工具调用是整块产出，出不来打字机的感觉。
 * 客户端切分 = 一次往返 + 本地切，还能保住流式。
 *
 * ## 切分规则（按优先级）
 *
 * 1. **段落边界**（空行）—— 这是模型天然的呼吸点，"模型自己决定发几条"就落在这一层
 * 2. 段落仍太长 → **句子边界**（。！？!?… 与单换行）
 * 3. 兜底：整段一条
 *
 * ## 什么不许拆
 *
 * 代码块（```）、列表（- / 1.）、表格（|）**整块保留** —— 拆开就没法读了。
 * （bubble-splitter 与 SleekFlow 都专门强调过这条，是踩过坑的经验。）
 *
 * ## 两个保底
 *
 * - **太短的并进相邻气泡**：一连串「嗯」「好」会刷屏
 * - **硬上限 5 条**：超了**合并最短的相邻气泡**（不丢字，只压条数）
 *
 * 5 这个数字与 openhuman 的 `MAX_SEGMENTS` 一致，用户也点名要 5。
 *
 * ⚠️ 只给**闲聊模式**用（见 `main.ts` 里按 `chatty` 分流）。工作模式的结构化长答案
 *    切成 5 个泡会非常难读。
 */

/**
 * 短于这个长度的气泡算"太短"，会被并进相邻条。
 *
 * 20 约等于"一句完整的话"：「好的」「在的」「嗯嗯」这类会被并掉，而正常的一句
 * 陈述能独立成泡。英文场景通常取 40+，中文短句多，取小一点。
 */
const MIN_BUBBLE_CHARS = 20;

/** 硬上限：一条回复最多几个气泡（用户 2026-10-08 定） */
export const MAX_BUBBLES = 5;

/** 一个块。`atomic` = 代码块/列表/表格，整块当一个气泡，不再往下切。 */
interface Block {
  text: string;
  atomic: boolean;
}

/**
 * 按块切：段 / 代码块 / 列表 / 表格。
 *
 * ⚠️ 代码块内部**可能有空行**，只按空行切会把半个围栏当独立块 ——
 *    所以围栏要单独跟踪状态（`markdown.ts` 的 `splitStable` 踩过同一个坑）。
 */
function splitBlocks(src: string): Block[] {
  const lines = src.split("\n");
  const out: Block[] = [];
  let buf: string[] = [];
  let kind: "para" | "fence" | "list" | "table" | null = null;

  const flush = (): void => {
    if (buf.length) {
      const text = buf.join("\n").trim();
      if (text) out.push({ text, atomic: kind !== "para" });
    }
    buf = [];
    kind = null;
  };

  for (const line of lines) {
    const t = line.trim();

    if (!t) {
      flush();
      continue;
    }

    // ``` 围栏：进入 / 离开
    if (/^```/.test(t)) {
      if (kind === "fence") {
        buf.push(line);
        flush(); // 收尾围栏也在 buf 里，跟着一起走
        continue;
      }
      flush();
      kind = "fence";
      buf.push(line);
      continue;
    }
    if (kind === "fence") {
      buf.push(line);
      continue;
    }

    if (/^([-*+]|\d+[.)])\s+/.test(t)) {
      if (kind !== "list") {
        flush();
        kind = "list";
      }
      buf.push(line);
      continue;
    }

    if (/^\s*\|/.test(line)) {
      if (kind !== "table") {
        flush();
        kind = "table";
      }
      buf.push(line);
      continue;
    }

    if (kind !== "para") {
      flush();
      kind = "para";
    }
    buf.push(line);
  }
  flush();
  return out;
}

/**
 * 按句子边界切。
 *
 * 只认**中文句末标点与换行**，不切英文句号 —— 那会把 `3.14`、`Mr. Smith` 切坏，
 * 而 orbcat 的闲聊场景以中文为主。
 *
 * ⚠️ **行内语法（图片/链接）必须先摘出来再切**（2026-10-08 真机踩坑）：
 *   `![双手比心卖萌](D:\…bixin.jpg)` 里的 `!` 会被下面的 lookbehind 当成句末
 *   感叹号，一刀切成 `!` + `[描述](路径)` 两半 —— 图片语法当场报废，
 *   剩下的 `[…](本地路径)` 又被链接渲染判为危险协议原样显示，
 *   用户看到的就是一坨 markdown 原文。所以：先摘、切、再放回去。
 */
const INLINE_SYNTAX_RE = /!\[[^\]\n]*\]\([^)\n]*\)|\[[^\]\n]*\]\([^)\n]*\)/g;

function splitSentences(text: string): string[] {
  const stash: string[] = [];
  const masked = text.replace(INLINE_SYNTAX_RE, (m) => `\u0000${stash.push(m) - 1}\u0000`);
  return masked
    .split(/(?<=[。！？!?…])\s*|\n+/)
    .map((s) =>
      s.trim().replace(/\u0000(\d+)\u0000/g, (_m, i) => stash[Number(i)] ?? ""),
    )
    .filter(Boolean);
}

/** 太短的并进相邻气泡；首条太短则**向后**并（只做"向前并"会让首泡过短） */
function mergeShort(parts: string[]): string[] {
  const out: string[] = [];
  for (const p of parts) {
    if (out.length && p.length < MIN_BUBBLE_CHARS) {
      out[out.length - 1] = `${out[out.length - 1]}\n\n${p}`;
    } else {
      out.push(p);
    }
  }
  if (out.length >= 2 && out[0].length < MIN_BUBBLE_CHARS) {
    out[1] = `${out[0]}\n\n${out[1]}`;
    out.shift();
  }
  return out;
}

/** 超上限 → 反复合并**最短的相邻对**，直到条数达标（一个字都不丢，只压条数） */
function capCount(parts: string[], max: number): string[] {
  const out = [...parts];
  while (out.length > max) {
    let best = 0;
    let bestLen = Infinity;
    for (let i = 0; i < out.length - 1; i++) {
      const len = out[i].length + out[i + 1].length;
      if (len < bestLen) {
        bestLen = len;
        best = i;
      }
    }
    out.splice(best, 2, `${out[best]}\n\n${out[best + 1]}`);
  }
  return out;
}

/**
 * 主入口：正文 → 气泡数组。
 *
 * 返回的元素**已经是各自独立的完整文本**（不含分隔标记）—— 直接一个元素一个气泡，
 * 不需要调用方再剥什么。
 */
export function splitBubbles(src: string, final = true): string[] {
  const text = (src ?? "").trim();
  if (!text) return [];
  // 短消息不切：一句话还要拆两条反而别扭（SleekFlow 的做法是 <150 字单泡）
  if (text.length <= MIN_BUBBLE_CHARS) return [text];

  const parts: string[] = [];
  for (const b of splitBlocks(text)) {
    if (b.atomic) {
      parts.push(b.text); // 代码块 / 列表 / 表格：整块一个气泡
    } else {
      parts.push(...splitSentences(b.text));
    }
  }

  if (parts.length <= 1) return parts.length ? parts : [text];

  // 流式（`final = false`）：**只切，不合并**。
  // 合并会让已经显示出来的气泡突然消失（第 3 泡太短 → 被吸进第 2 泡），
  // 正盯着屏幕看的人只会觉得"咦，怎么没了"。合并留到跑完再来。
  if (!final) return parts.filter((p) => p.trim());

  return capCount(mergeShort(parts), MAX_BUBBLES).filter((p) => p.trim());
}
