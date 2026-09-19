// markdown.ts 的快速验证脚本（node --experimental-strip-types 直接跑）
// 用法：node --experimental-strip-types src/markdown.check.ts

import { mdToHtml } from "./markdown.ts";

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
];

console.log("\n=== 断言 ===");
for (const [name, got, want] of checks) {
  const ok = got.includes(want);
  console.log(`${ok ? "✅" : "❌"} ${name}: ${got.replace(/\n/g, "\\n").slice(0, 70)}`);
  if (!ok) fail++;
}

console.log(fail === 0 ? "\n全部通过" : `\n失败 ${fail} 项`);
process.exit(fail === 0 ? 0 : 1);
