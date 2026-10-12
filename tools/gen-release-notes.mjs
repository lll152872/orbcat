import { readFileSync, writeFileSync, existsSync } from "node:fs";

// 生成 GitHub Release 的发布说明（RELEASE_NOTES.md）。
//
// ## 为什么要这个脚本，而不是让 workflow 直接 `body_path: CHANGELOG.md`
//
// CHANGELOG.md 里有 **5 个都叫 [1.0.0] 的段**（历史累积，只有第一段有日期）。
// 整个贴上去会把所有旧版本的内容混进 Release 页面。
// 本脚本只取第一个 [1.0.0] 段，前后再拼上两段手工内容。
//
// ## 三段内容的分工
//
//   1. 发布头（RELEASE_NOTES_HEAD.md）—— 下载表、解压说明、未签名提醒
//   2. 手工段（RELEASE_NOTES_EXTRA.md）—— 本次相对上个 tag 的新增/已知限制
//   3. CHANGELOG 的第一个 [1.0.0] 段 —— 自动取，不抄
//
// 手工内容单独放文件而不是写在本脚本里：发新版时改 Markdown 就行，
// 不用动代码，也不必担心「改了脚本忘提交」。

const HEAD = "RELEASE_NOTES_HEAD.md";
const EXTRA = "RELEASE_NOTES_EXTRA.md";

const changelog = readFileSync("CHANGELOG.md", "utf8").split(/\r?\n/);

const startIdx = changelog.findIndex((l) => /^## \[1\.0\.0\]/.test(l));
if (startIdx < 0) {
  console.error("CHANGELOG.md 里找不到 [1.0.0] 段");
  process.exit(1);
}
let endIdx = changelog.length;
for (let i = startIdx + 1; i < changelog.length; i++) {
  if (/^## \[/.test(changelog[i])) { endIdx = i; break }
}
const body = changelog.slice(startIdx, endIdx).join("\n").trimEnd();

const head = existsSync(HEAD) ? readFileSync(HEAD, "utf8").trimEnd() : "";
const extra = existsSync(EXTRA) ? readFileSync(EXTRA, "utf8").trimEnd() : "";

const parts = [head, extra, body].filter((s) => s.length > 0);
writeFileSync("RELEASE_NOTES.md", parts.join("\n\n---\n\n") + "\n", "utf8");

console.log(
  `RELEASE_NOTES.md 已生成：` +
    `发布头 ${head ? "有" : "无"} · 手工段 ${extra ? `${extra.split("\n").length} 行` : "无"} · ` +
    `CHANGELOG 第 ${startIdx + 1}~${endIdx} 行（${endIdx - startIdx} 行）`,
);
