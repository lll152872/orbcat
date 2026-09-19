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
 *       引用 / 表格 / 分隔线 / 链接
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

/** 行内语法：代码 → 链接 → 粗体 → 斜体 → 删除线 */
function inline(src: string): string {
  let s = esc(src);

  // 行内代码先处理，并把内容占位保护起来（避免里面的 * 被当粗体）
  const codes: string[] = [];
  s = s.replace(/`([^`\n]+)`/g, (_m, code: string) => {
    codes.push(code);
    return `\u0000CODE${codes.length - 1}\u0000`;
  });

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

/** 判断一行是不是表格分隔行（|---|---|） */
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
