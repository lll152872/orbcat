#!/usr/bin/env node
/**
 * 给集成测试准备一份**假**的 models.json —— 但只在「本来就没有」的时候。
 *
 * 为什么需要（第二个「静默跳过」入口）：
 * `src-tauri/tests/pi_*.rs` 的 `orbcat_data_dir()` 向上找一个含 `models.json`
 * 的 agent-data/ 目录；找不到就打印「跳过：找不到 orbcat 的 agent-data/」然后
 * return —— 于是 `cargo test --tests` 全绿，而一条 e2e 都没跑。
 *
 * CI 是全新 checkout：`agent-data/` 被 .gitignore 整个忽略，**不可能存在**。
 * ⇒ 这里补一份假的：无真实 key、url 指向不可达端口。
 *    真模型测试要显式设 ORBCAT_REAL_MODEL=1 才跑（默认不跑），所以这份假配置
 *    不会被真的发出去。
 *
 * ⚠️ **只在文件不存在时写** —— 本地跑这个 action 时，仓库里的
 *    agent-data/models.json 是用户真配置（带 API key），绝不能被覆盖。
 *
 * 退出码：0 = 已就绪（现成的或新建的）；1 = 写失败。
 */

import fs from "node:fs";
import path from "node:path";

const ROOT = process.cwd();
const DATA_DIR = path.join(ROOT, "agent-data");
const MODELS = path.join(DATA_DIR, "models.json");

if (fs.existsSync(MODELS)) {
  console.log(`✓ agent-data/models.json 已存在（原样保留，${fs.statSync(MODELS).size} 字节）`);
  process.exit(0);
}

const fixture = [
  {
    id: "ci-fixture-model",
    name: "CI fixture model",
    // 不可达端口：这份配置只用来让测试找到数据目录，真模型测试默认不跑
    url: "http://127.0.0.1:9/v1",
    apiKey: "ci-fixture-not-a-real-key",
    maxInputTokens: 128000,
    maxOutputTokens: 16384,
    supportsToolCall: true,
    supportsImages: false,
  },
];

try {
  fs.mkdirSync(DATA_DIR, { recursive: true });
  fs.writeFileSync(MODELS, JSON.stringify(fixture, null, 2) + "\n", "utf8");
  console.log(`✓ 已创建夹具 agent-data/models.json（假模型，无真实 key）`);
} catch (e) {
  console.error(`✗ 写 agent-data/models.json 失败：${e.message}`);
  process.exit(1);
}
