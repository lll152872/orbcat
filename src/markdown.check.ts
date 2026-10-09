// markdown.ts 的快速验证脚本（node --experimental-strip-types 直接跑）
// 用法：node --experimental-strip-types src/markdown.check.ts

import { mdToHtml, splitStable } from "./markdown.ts";

const cases = [
  {
    name: "标题 + 粗体",
    md: "## 结论\n\n这是 **重点**，那是 *强调*。",
  },
  {
    name: "列表",
    md: "- 第一项\n- 第二项\n  - 嵌套？(不支持，会平铺)\n\n1. 有序一\n2. 有序二",
  },
  {
    name: "表格",
    md: "| 维度 | ArrayList | LinkedList |\n|---|---|---|\n| 随机访问 | O(1) | O(n) |\n| 插入删除 | O(n) | O(1) |",
  },
  {
    name: "代码块",
    md: "```java\nList<String> list = new ArrayList<>();\nlist.add(\"a\");\n```",
  },
  {
    name: "行内代码 + 链接",
    md: "用 `mvn clean install` 构建，详见 [官方文档](https://maven.apache.org)。",
  },
  {
    name: "引用 + 分隔线",
    md: "> 这是引用块\n> 第二行\n\n---\n\n正文",
  },
  {
    name: "XSS 防护（关键）",
    md: "正常文字 <script>alert('xss')</script>\n\n<img src=x onerror=alert(1)>\n\n[点我](javascript:alert(1))",
  },
  {
    // 图片是 2026-10-08 新开的注入面：模型可能试图用 `data:` 或 `javascript:`
    // 把东西塞进 src。判定失败必须**原样保留文本**（不生成标签），
    // 所以下面这些 payload 一条都不能变成真实 <img>。
    name: "XSS 防护·图片（关键）",
    md: "![a](javascript:alert)\n\n![b](data:text/html;base64,PHNjcmlwdD4=)\n\n<img src=\"x\" onerror=\"alert(1)\">",
  },
];

let fail = 0;
for (const c of cases) {
  const html = mdToHtml(c.md);
  console.log(`\n=== ${c.name} ===`);
  console.log(html);

  // 安全检查：**只看真实标签内部**有没有危险内容。
  // （不能直接 grep 原串 —— 转义后的 `&lt;img onerror=...&gt;` 是纯文本，不含标签，
  //   直接匹配 onerror= 会误报）
  const tagRe = /<[^>]*>/g;
  let m: RegExpExecArray | null;
  while ((m = tagRe.exec(html)) !== null) {
    const tag = m[0].toLowerCase();
    if (
      tag.startsWith("<script") ||
      tag.includes("onerror") ||
      tag.includes("onload") ||
      tag.includes("javascript:") ||
      tag.includes("<iframe")
    ) {
      console.log(`❌ XSS：真实标签里出现危险内容 → ${m[0]}`);
      fail++;
    }
  }
}

// 断言几个转换是否真的发生了
const checks: [string, string, string][] = [
  ["标题", mdToHtml("## 标题"), "<h2>标题</h2>"],
  ["粗体", mdToHtml("**b**"), "<strong>b</strong>"],
  ["表格", mdToHtml("|a|b|\n|---|---|\n|1|2|"), "<table"],
  ["代码块", mdToHtml("```\nx\n```"), "<pre class=\"md-code\">"],
  ["列表", mdToHtml("- a\n- b"), "<ul>"],
  ["引用", mdToHtml("> q"), "<blockquote>"],
  // ---- 图片（2026-10-08）----
  // 关键契约：本地路径**不能**出现在 src 里（webview 读不了，会留破图标），
  // 必须走 data-img 让 main.ts 异步换；data: 必须被原样保留（不生成 img）。
  ["图片·外链直接给 src", mdToHtml("![猫](https://x.com/a.png)"), 'src="https://x.com/a.png"'],
  ["图片·本地走 data-img", mdToHtml("![图](D:\\pics\\a.png)"), 'data-img="D:\\pics\\a.png"'],
  ["图片·尖括号裹带空格路径", mdToHtml("![图](<C:\\My Pics\\a.png>)"), "data-img=\"C:\\My Pics\\a.png\""],
  ["图片·data: 被挡（原样保留）", mdToHtml("![x](data:image/png;base64,AAA)"), "![x](data:image/png;base64,AAA)"],
];

console.log("\n=== 断言 ===");
for (const [name, got, want] of checks) {
  const ok = got.includes(want);
  console.log(`${ok ? "✅" : "❌"} ${name}: ${got.replace(/\n/g, "\\n").slice(0, 70)}`);
  if (!ok) fail++;
}

// ---- splitStable：流式「稳定前缀 / 活动尾巴」切分（2026-10-03）----
// 地基逻辑：切错了表格会在流式里反复突变，所以这组必须钉死。
// 断言形式统一为 [名称, 输入, stable 片段(空串=不应出现), tail 片段]
const splitCases: [string, string, string, string][] = [
  ["表格已收尾 → 进 stable", "| a | b |\n|---|---|\n| 1 | 2 |\n\n尾巴", "| a | b |", ""],
  ["表格分隔行未到 → 留 tail", "| a | b |\n| 1", "", "| a | b |"],
  ["无空行裸文本 → 整体留 tail", "还在写第一段", "", "还在写第一段"],
  ["未闭合代码块 → 不进 stable", "```js\nconst a = 1;\n\n第二段", "", "const a = 1;"],
  ["已闭合代码块 → 进 stable", "```js\nx\n```\n\n后面", "x", ""],
  ["空串", "", "", ""],
];

console.log("\n=== splitStable ===");
for (const [name, input, wantIn, wantTail] of splitCases) {
  const { stable, tail } = splitStable(input);
  const ok = stable.includes(wantIn) && tail.includes(wantTail);
  console.log(
    `${ok ? "✅" : "❌"} ${name}` +
      `  stable=${JSON.stringify(stable.slice(0, 36))} tail=${JSON.stringify(tail.slice(0, 28))}`,
  );
  if (!ok) fail++;
}

// 关键不变量：stable + tail 必须严丝合缝等于原文（渲染不能吞字/改字）
console.log("\n=== splitStable 不变量（stable+tail === 原文）===");
const invariants = [
  "| a | b |\n|---|---|\n| 1 | 2 |\n\n尾巴",
  "```js\nconst a = 1;\n\n未闭合",
  "- 一\n- 二\n\n\n- 三",
  "结尾没有空行的一段话",
  "多个空行\n\n\n\n之后",
];
for (const src of invariants) {
  const { stable, tail } = splitStable(src);
  const ok = stable + tail === src;
  console.log(`${ok ? "✅" : "❌"} ${JSON.stringify(src.slice(0, 30))}`);
  if (!ok) fail++;
}

console.log(fail === 0 ? "\n全部通过" : `\n失败 ${fail} 项`);
process.exit(fail === 0 ? 0 : 1);