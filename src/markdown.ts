/**
 * 极简 Markdown → HTML（零依赖）
 *
 * 为什么自己写而不引 `marked` / `markdown-it`：
 *   - 只需要模型回复里实际会用到的语法子集
 *   - 悬浮球是个小工具，不想为渲染塞 50KB 依赖
 *   - 自己写能把 XSS 防护做成**结构上的默认行为**（见下）
 *
 * ⚠️ 安全前提：**先 escape，再套标签**。
 *   所有来自模型的文本都先经过 `esc()`，之后插入的标签都是我们自己生成的，
 *   模型没法通过 `<script>` 或 `onerror=` 注入。
 *   链接也强制白名单协议（http/https/mailto），挡掉 `javascript:`。
 *
 * 支持：标题 / 粗体 / 斜体 / 行内代码 / 代码块 / 有序无序列表 /
 *       引用 / 表格 / 分隔线 / 链接 / **图片**
 *
 * ## 图片（2026-10-08 加）
 *
 * 模型在正文里写 `![说明](来源)` 就能把图显示在回复里。来源两种：
 * `http(s)` 外链、本地绝对路径。**本地路径产的是占位 `<img data-img>`**，
 * 真正的 data URL 由 `main.ts` 走 `image_thumb` 异步换上去 ——
 * 本模块是纯同步函数，碰不了 IPC。
 */

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** 只允许安全协议，挡掉 javascript: / data: 之类 */
function safeUrl(raw: string): string | null {
  const u = raw.trim();
  if (/^https?:\/\//i.test(u) || /^mailto:/i.test(u)) return u;
  if (u.startsWith("/") || u.startsWith("#") || u.startsWith("./")) return u;
  return null;
}

/**
 * 判定 markdown 图片的**来源**是否可渲染（2026-10-08 加）。
 *
 * 只放行两类：
 *   - `http(s)://` 外链 —— webview 直接加载（本项目 `tauri.conf.json` 的
 *     `csp` 为 `null`，没有外链拦截）
 *   - **本地绝对路径** —— 走 `image_thumb` 命令转 data URL。webview 读不了
 *     `file://`，这正是本地图必须先产占位 img、再由 `main.ts` 异步换 `src`
 *     的原因（见 `inline()` 里的图片分支）。
 *
 * **刻意挡掉 `data:`**：它是任意 base64 的载体，混进 `src` 等于开了一个
 * 绕过"先 escape 再套标签"这条结构防线的口子。`javascript:` 在 `img src`
 * 里本来就不会执行，但一并挡掉，免得日后有代码把这里的结果挪去 `href`。
 *
 * 判定失败返回 `null` → 调用方**原样保留文本**。宁可让用户看到一串路径，
 * 也不要图凭空消失（那会让人以为是渲染 bug）。
 */
function imgSrc(raw: string): { kind: "url" | "path"; value: string } | null {
  const u = raw.trim();
  if (!u) return null;
  if (/^https?:\/\//i.test(u)) return { kind: "url", value: u };
  // Windows 盘符绝对路径（C:\pics\a.png 或 C:/pics/a.png）
  if (/^[a-zA-Z]:[\\/]/.test(u)) return { kind: "path", value: u };
  // POSIX 绝对路径 —— 排除协议相对的 `//host/…`（那个在本地文件语境下无意义）
  if (u.startsWith("/") && !u.startsWith("//")) return { kind: "path", value: u };
  return null;
}

/** 行内语法：代码 → 链接 → 粗体 → 斜体 → 删除线 */
function inline(src: string): string {
  let s = esc(src);

  // 行内代码先处理，并把内容占位保护起来（避免里面的 * 被当粗体）
  const codes: string[] = [];
  s = s.replace(/`([^`\n]+)`/g, (_m, code: string) => {
    codes.push(code);
    return `\u0000CODE${codes.length - 1}\u0000`;
  });

  // 图片 ![说明](来源) —— **必须排在链接之前**（2026-10-08 加）。
  // 否则 `[说明](来源)` 会被下面的链接规则先吃掉，留下一个孤零零的 `!`，
  // 渲染出来就是「!<a href=…>说明</a>`」这种残废输出（加图之前就是这个表现）。
  //
  // 来源写法支持两种：`(路径)` 与 `(<路径>)`。后者专治**带空格的路径**
  // ——Windows 下 `C:\My Pics\a.png` 用裸括号会被 `[^)\s]+` 在空格处截断，
  // 所以系统提示里教模型：路径有空格就用尖括号裹起来。
  //
  // ⚠️ 正则里写的是 `&lt;` / `&gt;` 而**不是** `<` / `>`：`inline()` 开头先跑了
  //    `esc()`，此刻原文里的尖括号早已变成实体。**这里极容易改错** ——
  //    想把正则改回字面尖括号等于同时打开注入面（esc 在前的顺序是安全前提，
  //    不能为了让正则好写而调序）。
  s = s.replace(
    /!\[([^\]\n]*)\]\((?:&lt;([^\n]*?)&gt;|([^)\s]+))\)/g,
    (m, alt: string, angled: string | undefined, plain: string | undefined) => {
      const hit = imgSrc((angled ?? plain ?? "").replace(/&amp;/g, "&"));
      if (!hit) return m; // 不支持的来源 → 原样保留文本（用户能看到路径）
      if (hit.kind === "url") {
        // 外链：直接给 src，webview 自己加载
        return `<img class="md-img" src="${esc(hit.value)}" alt="${alt}" loading="lazy">`;
      }
      // 本地路径：**不写 src**，只把路径挂到 data-img，
      // 由 main.ts 的 hydrateBodyImages() 走 image_thumb 异步换成 data URL。
      // 写 src="C:\…" 在 webview 里必定加载失败，只会留一个破图标。
      return `<img class="md-img md-img-local" data-img="${esc(hit.value)}" alt="${alt}" title="${esc(
        hit.value,
      )}">`;
    },
  );

  // 链接 [文本](url)
  s = s.replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, (m, txt: string, url: string) => {
    const safe = safeUrl(url.replace(/&amp;/g, "&"));
    if (!safe) return m; // 危险协议 → 原样保留，不生成 <a>
    return `<a href="${esc(safe)}" target="_blank" rel="noreferrer noopener">${txt}</a>`;
  });

  // 粗体 / 斜体 / 删除线
  s = s.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*\w])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  s = s.replace(/~~([^~\n]+)~~/g, "<del>$1</del>");

  // 还原行内代码
  s = s.replace(/\u0000CODE(\d+)\u0000/g, (_m, i: string) => `<code>${codes[Number(i)]}</code>`);

  return s;
}

/**
 * 把流式文本切成「已稳定」与「还在吐」两段。
 *
 * 为什么需要（2026-10-03）：`mdToHtml` 判表格靠"当前行有 `|` 且下一行是 `|---|`"，
 * 而流式是逐 chunk 到的。同一个表格在到达分隔行之前会被当成普通段落，
 * 补上分隔行的瞬间整块突变成 `<table>` —— 列宽一变，用户正看着的位置就跳。
 * 逐 chunk 全量重渲的话，一段输出里这种突变能有几十次，看着像闪烁。
 *
 * 这里的规则：**只有确定吐完的块才交给 mdToHtml，尾巴保持纯文本**。
 * 判定"吐完"用空行 —— markdown 里空行是块边界，模型写完一段几乎总会空行。
 * 于是每个块只会**在它自己收尾的那一瞬间**从纯文本变 markdown（1 次），
 * 而不是每个 chunk 都抖。
 *
 * 未闭合的 ``` 代码块要额外回退：代码块内部常有空行，只按空行切会把
 * 半个代码块当"已稳定"，`mdToHtml` 会把剩下的全吞进代码块里。
 *
 * @returns `stable` 可安全渲染；`tail` 必须原样 escape + `<br>`
 */
export function splitStable(src: string): { stable: string; tail: string } {
  if (!src) return { stable: "", tail: "" };

  // ① 最后一个空行（允许行尾只有空白字符）之后的部分 = 活动尾巴
  let cut = -1;
  const blank = /\n[ \t]*\n/g;
  let m: RegExpExecArray | null;
  while ((m = blank.exec(src)) !== null) {
    cut = m.index + m[0].length;
  }
  let stable = cut >= 0 ? src.slice(0, cut) : "";
  let tail = cut >= 0 ? src.slice(cut) : src;

  // ② 未闭合的 ``` 围栏 → 整段退回围栏之前（`mdToHtml` 的围栏循环会吞到末尾）
  const fences = (stable.match(/^\s*```/gm) ?? []).length;
  if (fences % 2 === 1) {
    const lastFence = stable.lastIndexOf("```");
    // 往前找围栏所在行之前的最后一个空行；找不到就整段当尾巴（宁可不渲染）
    const before = stable.slice(0, lastFence);
    const blankBefore = [...before.matchAll(/\n[ \t]*\n/g)].pop();
    stable = blankBefore ? before.slice(0, blankBefore.index! + blankBefore[0].length) : "";
    tail = src.slice(stable.length);
  }

  return { stable, tail };
}

/**
 * 判断一行是不是表格分隔行（|---|---|） */
function isTableSep(line: string): boolean {
  return /^\s*\|?[\s:|-]+\|[\s:|-]*$/.test(line) && line.includes("-");
}

function splitRow(line: string): string[] {
  return line
    .replace(/^\s*\|/, "")
    .replace(/\|\s*$/, "")
    .split("|")
    .map((c) => c.trim());
}

/**
 * 主入口：把 markdown 文本转成安全的 HTML。
 */
export function mdToHtml(src: string): string {
  if (!src) return "";

  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  const out: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    // ---- 代码块 ----
    const fence = line.match(/^\s*```(\w*)\s*$/);
    if (fence) {
      const lang = fence[1];
      const body: string[] = [];
      i++;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) {
        body.push(lines[i]);
        i++;
      }
      i++; // 跳过结束围栏
      const cls = lang ? ` class="lang-${esc(lang)}"` : "";
      // 包一层容器：右上角语言标签 + 复制按钮（点击由 main.ts 的事件委托处理，
      // 因为消息区每次重绘都会重新生成这段 HTML）
      out.push(
        `<div class="md-code-wrap">` +
          `<div class="md-code-bar"><span class="md-code-lang">${esc(lang || "text")}</span>` +
          `<button type="button" class="code-copy" title="复制代码">复制</button></div>` +
          `<pre class="md-code"><code${cls}>${esc(body.join("\n"))}</code></pre>` +
          `</div>`,
      );
      continue;
    }

    // ---- 分隔线 ----
    if (/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
      out.push("<hr>");
      i++;
      continue;
    }

    // ---- 标题 ----
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      const lv = h[1].length;
      out.push(`<h${lv}>${inline(h[2])}</h${lv}>`);
      i++;
      continue;
    }

    // ---- 表格（当前行含 |，下一行是分隔行）----
    if (line.includes("|") && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      const head = splitRow(line);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && lines[i].includes("|") && lines[i].trim() !== "") {
        rows.push(splitRow(lines[i]));
        i++;
      }
      const th = head.map((c) => `<th>${inline(c)}</th>`).join("");
      const tb = rows
        .map(
          (r) =>
            `<tr>${r
              .map((c, idx) => `<td>${inline(c ?? "")}${idx >= head.length ? "" : ""}</td>`)
              .join("")}</tr>`,
        )
        .join("");
      out.push(`<table class="md-table"><thead><tr>${th}</tr></thead><tbody>${tb}</tbody></table>`);
      continue;
    }

    // ---- 引用 ----
    if (/^\s*>\s?/.test(line)) {
      const body: string[] = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        body.push(lines[i].replace(/^\s*>\s?/, ""));
        i++;
      }
      out.push(`<blockquote>${mdToHtml(body.join("\n"))}</blockquote>`);
      continue;
    }

    // ---- 列表（有序 / 无序，不处理嵌套）----
    const ul = /^\s*[-*+]\s+(.*)$/;
    const ol = /^\s*(\d+)[.)]\s+(.*)$/;
    if (ul.test(line) || ol.test(line)) {
      const ordered = ol.test(line);
      const items: string[] = [];
      while (i < lines.length) {
        const m = ordered ? lines[i].match(ol) : lines[i].match(ul);
        if (!m) break;
        items.push(ordered ? m[2] : m[1]);
        i++;
      }
      const tag = ordered ? "ol" : "ul";
      out.push(
        `<${tag}>${items.map((t) => `<li>${inline(t)}</li>`).join("")}</${tag}>`,
      );
      continue;
    }

    // ---- 空行 ----
    if (line.trim() === "") {
      i++;
      continue;
    }

    // ---- 段落（连续非空行合并）----
    const para: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !/^\s*```/.test(lines[i]) &&
      !/^(#{1,6})\s/.test(lines[i]) &&
      !/^\s*>\s?/.test(lines[i]) &&
      !ul.test(lines[i]) &&
      !ol.test(lines[i]) &&
      !/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(lines[i])
    ) {
      para.push(lines[i]);
      i++;
    }
    if (para.length) out.push(`<p>${inline(para.join("\n")).replace(/\n/g, "<br>")}</p>`);
  }

  return out.join("");
}
