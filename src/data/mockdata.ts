/**
 * Mock 数据本体 —— **看起来像真的**。
 *
 * ## 为什么要单独一个文件
 *
 * 界面出视频、给人看设计稿效果时，lorem ipsum 和 `xxx` 会让人没法判断
 * 「排版到底成不成立」—— 中文和英文的字宽差 2 倍，长度也差很远。
 * 所以这里全部用**真实语料**：真实长度、真实语气、真实的中英混排
 * （路径、命令、模型名、代码标识符都是原样出现的）。
 *
 * ## 数据时间基准
 *
 * 固定在 **2026-10-23 16:20**（不用 `Date.now()`）——
 * 截图和视频才可复现，两次打开看到的是同一批数据。
 */
import type {
  Candidate,
  CmdAuditRecord,
  CmdGrantsView,
  CmdPolicy,
  ContextCapability,
  FgCtx,
  LifeSourceView,
  LifeStatus,
  McpServerCfg,
  McpStatus,
  MemStats,
  MemoryFiles,
  ModelView,
  PendingPerm,
  PermRule,
  ProjectListView,
  ProjectMemoryView,
  ProjectRow,
  RecoveryState,
  SearchStatus,
  Session,
  SessionMeta,
  SkillInfo,
  StoredMsg,
  UsageRecord,
} from "./types";
import type { BgDeskStatus, BootState, ModesView, PiStatus, SettingsView } from "./source";

/**
 * 固定时间基准：**本地时间** 2026-10-23 16:20。
 *
 * ⚠️ 必须用 `new Date(y, m, d, h, mi, s)`（本地时区），
 * 不能用 `Date.UTC` —— 后者会被当成 UTC，本机 UTC+8 下所有时刻都往后偏 8 小时，
 * 于是「下午 3 点的对话」显示成「次日凌晨 1 点」。
 *
 * 固定基准（而不是 `Date.now()`）：截图和视频要可复现 —— 两次打开看到同一批数据。
 */
const BASE = new Date(2026, 9, 23, 16, 20, 0).getTime();
/** 当天 h:m(:s) 的毫秒数（相对基准偏移） */
const at = (h: number, m: number, s = 0): number =>
  BASE + ((h - 16) * 60 + (m - 20) + s / 60) * 60_000;

// ---------------- 会话消息语料 ----------------

const msgs: StoredMsg[] = [
  {
    role: "user",
    text: "orbcat 的悬浮球拖动判定是不是太灵敏了？轻轻点一下经常被当成拖动，面板弹不出来",
    at: at(15, 2),
  },
  {
    role: "assistant",
    text: `拖动阈值现在是 6px，但要区分「点击」和「拖动」光看位移不够 —— 快速点一下也会产生 1~2px 的抖动。

我建议改成**看位移方向和时长**：位移超过阈值、或者按住超过 120ms 才算拖动，否则一律当点击。`,
    reasoning:
      "用户报的是「点击被误判成拖动」。先确认当前判定逻辑：attachDrag 里用 DRAG_THRESHOLD=6 比较 mousedown/mouseup 的位移，\n" +
      "超过阈值就取消 click。快速点击时浏览器仍可能产生微小位移，所以在 6px 附近会误判。\n" +
      "先读一下 attachDrag 的实现再决定改法。",
    steps: [
      {
        kind: "tool_call",
        name: "read_file",
        detail: '{"path":"src/main.ts","range":"1310-1360"}',
        durationMs: 82,
        result: "function attachDrag(elm: HTMLElement) {\n  let sx = 0, sy = 0, dragging = false;\n  // …\n}",
      },
      {
        kind: "tool_call",
        name: "read_file",
        detail: '{"path":"src/main.ts","range":"60-66"}',
        durationMs: 41,
        result: "/** 判定\"拖动\"的像素阈值 */\nconst DRAG_THRESHOLD = 6;",
      },
    ],
    usage: { prompt: 2841, completion: 396, total: 3237, cacheHit: 1536 },
    model: "MiniMax-M2.1",
    at: at(15, 2, 40),
  },
  {
    role: "user",
    text: "方向和时长一起判吧。另外顺手看一下会话页那个「当前行」的薄荷描边，是不是太亮了",
    at: at(15, 6),
  },
  {
    role: "assistant",
    text: `两处都改好了。

**拖动判定**：现在同时看位移与时长 —— 位移 < 6px 且按住 < 120ms 一律算点击；位移 > 6px 或按住超过 120ms 才开始跟手。

**会话页当前行**：描边从猫白 \`.50\` 降到 \`.40\`，底色保持 \`#1A2120\`。设计稿 05 节点 \`2:943\` 标的就是 \`.40\`，之前是按感觉加重的。`,
    steps: [
      {
        kind: "tool_call",
        name: "edit_file",
        detail: '{"path":"src/main.ts","op":"replace","find":"dragging = Math.abs(dx) > DRAG_THRESHOLD"}',
        durationMs: 118,
        result: "已替换 1 处",
      },
      {
        kind: "tool_call",
        name: "edit_file",
        detail: '{"path":"src/styles.css","op":"replace","find":"border: 1px solid rgba(228,235,231,.50)"}',
        durationMs: 96,
        result: "已替换 1 处",
      },
    ],
    usage: { prompt: 3917, completion: 512, total: 4429, cacheHit: 2048 },
    model: "MiniMax-M2.1",
    at: at(15, 6, 50),
  },
  {
    role: "assistant",
    text: "改完跑了一遍 UI 用例：27 个文件 225 条全过。要我顺手把发布脚本也一起更新吗？",
    steps: [],
    usage: { prompt: 2104, completion: 88, total: 2192 },
    model: "MiniMax-M2.1",
    at: at(15, 7),
  },
];

/** 富内容会话：带插话、图片、工具失败、压缩提示 */
const richMsgs: StoredMsg[] = [
  {
    role: "user",
    text: "帮我看下这张报错截图",
    images: ["C:\\Users\\DELL\\AppData\\Local\\Temp\\orbcat-shot-01.png"],
    at: at(14, 12),
  },
  {
    role: "assistant",
    text: `看到了，是 \`cargo test --lib\` 在 \`recovery::tests\` 那个模块里卡住 —— 不是失败，是**死等**。

栈顶停在 \`Command::new("git").arg("--version")\`，这台机器上 git 不在 PATH 里，进程创建本身阻塞了。

建议给所有外部命令调用加超时：\`Command::spawn\` 换成带 \`wait_timeout\` 的封装，超时按「不可用」处理而不是无限等。`,
    reasoning: "先看报错发生的位置，确认是测试挂住还是真的失败。",
    steps: [
      {
        kind: "tool_call",
        name: "read_file",
        detail: '{"path":"src-tauri/src/recovery.rs","range":"220-268"}',
        durationMs: 64,
        result: "pub fn git_version() -> String {\n    let out = Command::new(\"git\").arg(\"--version\").output();\n    // …\n}",
      },
      {
        kind: "tool_error",
        name: "run_command",
        detail: '{"cmd":"cargo test --lib recovery::tests -- --nocapture"}',
        durationMs: 30007,
        result: "命令超时（30s）：git --version 无响应",
      },
    ],
    usage: { prompt: 5210, completion: 733, total: 5943, cacheHit: 3072 },
    model: "MiniMax-M2.1",
    at: at(14, 12, 35),
  },
  {
    role: "user",
    text: "等一下，先别改代码",
    steerId: "st1781012345",
    at: at(14, 13, 2),
  },
  {
    role: "assistant",
    text: "好，先不动代码。",
    steps: [],
    usage: { prompt: 880, completion: 12, total: 892 },
    model: "MiniMax-M2.1",
    at: at(14, 13, 20),
  },
];

/** 空会话：一条消息都没有 */
const emptyMsgs: StoredMsg[] = [];

// ---------------- 会话列表 ----------------

const sessionList: SessionMeta[] = [
  {
    id: "s-main",
    title: "主对话",
    createdAt: at(9, 12),
    updatedAt: at(15, 7),
    count: msgs.length,
    current: true,
    kind: "main",
    projectName: "悬浮小agent",
    mode: "standard",
  },
  {
    id: "s-task-1",
    title: "排查记忆审批不生效",
    createdAt: at(10, 4),
    updatedAt: at(11, 22),
    count: 18,
    current: false,
    kind: "task",
    projectName: "悬浮小agent",
    mode: "standard",
  },
  {
    id: "s-task-2",
    title: "把发布流程写成脚本",
    createdAt: at(13, 30),
    updatedAt: at(13, 51),
    count: 9,
    current: false,
    kind: "task",
    projectName: null,
    mode: "creator",
  },
  {
    id: "s-task-3",
    title: "调研 pi 内核的上下文压缩成本",
    createdAt: at(9, 40),
    updatedAt: at(9, 58),
    count: 24,
    current: false,
    kind: "task",
    projectName: "悬浮小agent",
    mode: "chat",
  },
  {
    id: "s-fork-1",
    title: "主对话（从第 4 条分叉）",
    createdAt: at(15, 1),
    updatedAt: at(15, 1),
    count: 4,
    current: false,
    kind: "fork",
    mode: "standard",
  },
];

const richList: SessionMeta[] = [
  {
    id: "s-main",
    title: "主对话",
    createdAt: at(9, 12),
    updatedAt: at(14, 13),
    count: richMsgs.length,
    current: true,
    kind: "main",
    projectName: "悬浮小agent",
    mode: "standard",
  },
  ...sessionList.slice(1),
];

// ---------------- 模型 ----------------

const models: ModelView[] = [
  {
    id: "MiniMax-M2.1",
    vendor: "MiniMax",
    url: "https://api.minimaxi.com/v1",
    supportsToolCall: true,
    supportsImages: true,
    maxInputTokens: 196608,
    maxOutputTokens: 32768,
    hasKey: true,
  },
  {
    id: "MiniMax-M2.1-highspeed",
    vendor: "MiniMax",
    url: "https://api.minimaxi.com/v1",
    supportsToolCall: true,
    supportsImages: false,
    maxInputTokens: 196608,
    maxOutputTokens: 16384,
    hasKey: true,
  },
  {
    id: "claude-sonnet-4-5",
    vendor: "Anthropic",
    url: "https://api.anthropic.com/v1",
    supportsToolCall: true,
    supportsImages: true,
    maxInputTokens: 200000,
    maxOutputTokens: 64000,
    hasKey: false,
  },
  {
    id: "gpt-4.1",
    vendor: "OpenAI",
    url: "https://api.openai.com/v1",
    supportsToolCall: true,
    supportsImages: true,
    maxInputTokens: 1048576,
    maxOutputTokens: 32768,
    hasKey: true,
  },
  {
    id: "qwen3-max",
    vendor: "阿里云",
    url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
    supportsToolCall: true,
    supportsImages: false,
    maxInputTokens: 262144,
    maxOutputTokens: 32768,
    hasKey: false,
  },
  {
    id: "deepseek-chat",
    vendor: "DeepSeek",
    url: "https://api.deepseek.com/v1",
    supportsToolCall: true,
    supportsImages: false,
    maxInputTokens: 65536,
    maxOutputTokens: 8192,
    hasKey: true,
  },
];

// ---------------- 记忆候选 ----------------

const pendingCandidates: Candidate[] = [
  {
    id: "cand-001",
    content:
      "用户要求：orbcat 面板里不要出现任何快捷键提示（用户原话「用你个软件还要背这么多快捷方式」）。",
    source: "conversation:2026-10-23T14:02:11Z",
    created_at: "2026-10-23T14:02:11Z",
    turn_ref: "以后别加快捷键了",
  },
  {
    id: "cand-002",
    content: "项目 orbcat 的 agent-data 目录在 D:\\workplace\\悬浮小agent\\agent-data（Windows 路径，勿用 /）。",
    source: "conversation:2026-10-23T15:06:44Z",
    created_at: "2026-10-23T15:06:44Z",
    turn_ref: "数据目录搬哪了",
  },
  {
    id: "cand-003",
    content: "用户偏好：中文回复，术语保留英文原词（如 hook、race condition）。",
    source: "project:悬浮小agent",
    created_at: "2026-10-22T09:31:02Z",
    turn_ref: null,
  },
  {
    id: "cand-004",
    content: "发布版不含 Git（GPLv2 不可再分发），只提示用户自装。",
    source: "conversation:2026-10-23T11:20:15Z",
    created_at: "2026-10-23T11:20:15Z",
    turn_ref: "git 能不能一起打包",
  },
];

const memFiles: MemoryFiles = {
  rules: `# orbcat 行为准则

1. 不在界面里出现快捷键提示。
2. 聊天区由用户自己维护，AI 不主动改它的结构与配色。
3. 色值只认 \`styles.css\` 顶部 \`:root\`，不写裸色值。`,
  soul: `你是悬浮在桌面上的一点光。
你替人跑那些"记得住但懒得做"的事：查资料、盯进程、改配置、跑脚本。
你话不多，但说的每句都要能直接照着做。`,
  identity: `名字：orbcat
形态：56×56 的悬浮球，两态（球 / 面板）
常驻：屏幕右上角，置顶`,
  user: `名字：lll152
机器：Windows 11 26400（24H2）
常用目录：D:\\workplace
语言：中文`,
  memory: `- 2026-10-23 用户把发布流程改成 build-release.sh --fast
- 2026-10-23 决定前端不再往 main.ts 堆逻辑，新逻辑另开模块`,
};

// ---------------- 权限 ----------------

const permRequests: PendingPerm[] = [
  {
    id: "perm-file-01",
    kind: "file",
    path: "D:\\workplace\\悬浮小agent\\orbcat\\src-tauri\\src\\lib.rs",
    access: "readwrite",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "",
    normalized: "",
    risk: "medium",
    headFp: "",
    fullFp: "",
    tier: "session",
    reason: "修改权限申请时需要读写后端入口文件",
    sessionId: "s-main",
    createdAt: at(15, 3),
    timeoutMs: 120000,
    question: "",
    options: [],
  },
  {
    id: "perm-cmd-01",
    kind: "command",
    path: "",
    access: "",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "git status --short",
    normalized: "git status --short",
    risk: "low",
    headFp: "git::status",
    fullFp: "git::status::--short",
    tier: "session",
    reason: "查看工作区改动（只读命令）",
    sessionId: "s-main",
    createdAt: at(15, 4),
    timeoutMs: 120000,
    question: "",
    options: [],
  },
  {
    id: "perm-cmd-02",
    kind: "command",
    path: "",
    access: "",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "rm -rf ./target/release",
    normalized: "rm --recursive --force ./target/release",
    risk: "high",
    headFp: "rm::--recursive",
    fullFp: "rm::--recursive::--force::./target/release",
    tier: "session",
    reason: "清理 Rust 编译产物（约 6.9 GB）",
    sessionId: "s-task-2",
    createdAt: at(15, 9),
    timeoutMs: 120000,
    question: "",
    options: [],
  },
  {
    id: "perm-ask-01",
    kind: "ask",
    path: "",
    access: "",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "",
    normalized: "",
    risk: "low",
    headFp: "",
    fullFp: "",
    tier: "once",
    reason: "需要用户拍板",
    sessionId: "s-main",
    createdAt: at(15, 10),
    timeoutMs: 300000,
    question: "这次改动要落到 feat/frontend/ 下的新分支，还是直接并进当前的 pi 分支？",
    options: ["开新分支", "直接并进当前分支", "先只留在工作区"],
  },
  /**
   * **pi 确认卡**（`kind = "pi"`）—— pi 内核的 `ctx.ui.confirm(title, message)`。
   *
   * ⚠️ **字段严格照后端 `PendingView`**（不是照前端想象）：
   *  - `id` 用 **`pp` 前缀** —— 后端靠 id 前缀推 kind（`pm*`→mcp、`pa*`→ask、
   *    **`pp*`→pi**、其余→command）⇒ mock 的 id 也得守这个约定，
   *    否则 mock 场景渲染出来的卡 ≠ 真机那张（判据⑧要的就是这个）；
   *  - `question` = pi 的 **title**（后端把 title 存进 question 字段）；
   *  - `message` = pi 的 **message**（**后端这轮才补的字段，还没进基线**
   *    ⇒ 前端必须容忍它缺失，卡上不渲染那一行）；
   *  - `command`/`normalized`/`path`/`access`/`headFp`/`fullFp` **全空**
   *    （pi 侧没有这些概念）⇒ 渲染时**不许**显示这些行；
   *  - `risk` 恒 `medium`、`reason` 恒「pi 内核请求确认」、`tier` 恒 `once`；
   *  - `timeoutMs` = **180000**（3 分钟，与 pm* 一致）。
   */
  {
    id: "pp1",
    kind: "pi",
    path: "",
    access: "",
    deniedPath: null,
    widenLevels: null,
    widened: false,
    command: "",
    normalized: "",
    risk: "medium",
    headFp: "",
    fullFp: "",
    tier: "once",
    reason: "pi 内核请求确认",
    sessionId: "s-main",
    // ⚠️ **这里用相对当前时刻，不用 `at()`** ——
    //    倒计时 = `createdAt + timeoutMs - now`，
    //    固定基准（BASE 是 2026-10-23 16:20）⇒ 倒计时恒为 0:00，
    //    预览里看到的就永远是「已超时」的样子，看不出 3 分钟倒计时长什么样。
    //    ⇒ 只这一张卡用「8 秒前到达」，其余 fixture 仍用固定基准（可复现）。
    createdAt: Date.now() - 8_000,
    timeoutMs: 180000,
    question: "pi 要做一件事",
    message: "允许执行 rm -rf ~/project 吗？",
    options: ["允许", "拒绝"],
  },
];

const permRules: PermRule[] = [
  { prefix: "D:\\workplace\\悬浮小agent\\orbcat\\src", access: "readwrite", label: "orbcat 源码" },
  { prefix: "D:\\workplace\\悬浮小agent\\docs", access: "readwrite", label: "项目文档" },
  { prefix: "C:\\Users\\DELL\\.minimax", access: "read", label: "Mavis 配置" },
  { prefix: "D:\\wechat", access: "deny", label: "微信聊天存档（不删）" },
];

const cmdPolicy: CmdPolicy = {
  allow: ["git status", "git diff", "cargo test", "npm run build", "npx vitest run"],
  hard_block: ["rm -rf /", "rm -rf /*", "mkfs.*", "dd of=/dev/sd*", "curl ... | sh", "wget ... | bash"],
};

const cmdGrants: CmdGrantsView[] = [
  {
    fingerprint: "git::status::--short",
    display: "git status --short",
    scope: "session",
    tier: "session",
    remaining: null,
    reason: "查看工作区改动",
  },
  {
    fingerprint: "cargo::test::--lib",
    display: "cargo test --lib",
    scope: "project",
    tier: "project",
    remaining: null,
  },
  {
    fingerprint: "npm::run::build",
    display: "npm run build",
    scope: "project",
    tier: "project",
    remaining: 3,
    reason: "本周还剩 3 次",
  },
];

const cmdAudit: CmdAuditRecord[] = [
  {
    ts: at(15, 4, 12),
    sequence: 1081,
    event: "ask",
    source: "agent",
    command: "git status --short",
    normalized: "git status --short",
    cwd: "D:\\workplace\\悬浮小agent\\orbcat",
    risk: "low",
  },
  {
    ts: at(15, 4, 12),
    sequence: 1082,
    event: "allow",
    source: "agent",
    command: "git status --short",
    normalized: "git status --short",
    cwd: "D:\\workplace\\悬浮小agent\\orbcat",
    risk: "low",
  },
  {
    ts: at(15, 9, 3),
    sequence: 1083,
    event: "hard-block",
    source: "agent",
    command: "rm -rf /",
    normalized: "rm --recursive --force /",
    cwd: "D:\\workplace\\悬浮小agent\\orbcat",
    risk: "high",
  },
  {
    ts: at(15, 9, 40),
    sequence: 1084,
    event: "ask",
    source: "agent",
    command: "rm -rf ./target/release",
    normalized: "rm --recursive --force ./target/release",
    cwd: "D:\\workplace\\悬浮小agent\\orbcat",
    risk: "high",
  },
];

// ---------------- MCP ----------------

const mcpStatus: McpStatus = {
  connected: true,
  error: null,
  url: "http://127.0.0.1:3050/mcp",
  activeTokens: 18420,
  groups: [
    {
      name: "filesystem",
      summary: "本地文件读写与目录遍历",
      toolCount: 8,
      tokens: 5240,
      active: true,
      hasDestructive: true,
    },
    {
      name: "github",
      summary: "仓库、Issue、PR、代码搜索",
      toolCount: 12,
      tokens: 7310,
      active: true,
      hasDestructive: false,
    },
    {
      name: "playwright",
      summary: "无头浏览器操作与截图",
      toolCount: 6,
      tokens: 4120,
      active: false,
      hasDestructive: false,
    },
    {
      name: "ssh",
      summary: "远程主机命令执行",
      toolCount: 5,
      tokens: 1750,
      active: false,
      hasDestructive: true,
    },
  ],
};

const mcpServers: McpServerCfg[] = [
  {
    id: "1mcp-gateway",
    url: "http://127.0.0.1:3050/mcp",
    enabled: true,
    label: "本地 1MCP 网关",
    transport: "http",
  },
  {
    id: "playwright-headless",
    url: "",
    enabled: false,
    label: "Playwright（stdio）",
    transport: "stdio",
    command: "npx",
    args: ["-y", "@playwright/mcp@latest", "--headless"],
    env: { PLAYWRIGHT_BROWSERS_PATH: "C:\\Users\\DELL\\AppData\\Local\\ms-playwright" },
  },
];

// ---------------- 恢复区 ----------------

const recovery: RecoveryState = {
  stats: { backupsCount: 12, backupsBytes: 4_826_214, trashCount: 31, trashBytes: 1_204_880, staleRecords: 3 },
  backups: [
    {
      id: "bk-20261023-1412",
      at: at(14, 12),
      original: "D:\\workplace\\悬浮小agent\\orbcat\\src\\styles.css",
      backup: "D:\\workplace\\悬浮小agent\\orbcat\\.trash\\backups\\styles.css.20261023-1412",
      size: 288_412,
      legacy: false,
    },
    {
      id: "bk-20261022-0905",
      at: at(9, 5),
      original: "D:\\workplace\\悬浮小agent\\orbcat\\src\\main.ts",
      backup: "D:\\workplace\\悬浮小agent\\orbcat\\.trash\\backups\\main.ts.20261022-0905",
      size: 402_177,
      legacy: false,
    },
    {
      id: "bk-20261021-1733",
      at: at(17, 33),
      original: "D:\\workplace\\悬浮小agent\\orbcat\\docs\\09-FRONTEND-HANDOVER.md",
      backup: "D:\\workplace\\悬浮小agent\\orbcat\\.trash\\backups\\09-FRONTEND-HANDOVER.md.20261021-1733",
      size: 61_920,
      legacy: false,
    },
  ],
  trash: [
    {
      at: at(13, 40),
      original: "D:\\workplace\\悬浮小agent\\orbcat\\src\\views\\old-dashboard.ts",
      trashed: "D:\\workplace\\悬浮小agent\\orbcat\\.trash\\2026-10-23\\old-dashboard.ts",
      kind: "file",
      count: 1,
      present: true,
      size: 18_442,
    },
    {
      at: at(12, 18),
      original: "D:\\workplace\\悬浮小agent\\orbcat\\src\\views\\legacy",
      trashed: "D:\\workplace\\悬浮小agent\\orbcat\\.trash\\2026-10-23\\legacy",
      kind: "dir",
      count: 14,
      present: true,
      size: 96_730,
    },
    {
      at: at(9, 55),
      original: "D:\\workplace\\悬浮小agent\\orbcat\\dist\\assets\\main-CNKtJRFC.js",
      trashed: "D:\\workplace\\悬浮小agent\\orbcat\\.trash\\2026-10-23\\main-CNKtJRFC.js",
      kind: "file",
      count: 1,
      present: false,
      size: 421_005,
    },
  ],
  defaultKeepDays: 30,
  defaultKeepItems: 200,
};

// ---------------- 其它域 ----------------

const modes: ModesView = {
  activeMode: "standard",
  defaultMode: "standard",
  modes: [
    {
      id: "standard",
      name: "标准模式",
      description: "默认模式：直接干活，过程可见",
      toolPresentation: "native",
      imageReplies: "work",
      promptExtra: "你在用户的桌面上帮忙做事。",
    },
    {
      id: "chat",
      name: "闲聊模式",
      description: "只聊天，不调工具",
      toolPresentation: "native",
      imageReplies: "free",
      promptExtra: "只做聊天，不要调用任何工具。",
      chatty: true,
    },
    {
      id: "creator",
      name: "创造模式",
      description: "做方案、出设计稿、整文档",
      toolPresentation: "native",
      imageReplies: "free",
      promptExtra: "你负责把想法变成可交付的产物。",
    },
    {
      id: "ptc",
      name: "PTC 模式",
      description: "工具结果直接进上下文，省 token",
      toolPresentation: "ptc",
      imageReplies: "work",
      promptExtra: "工具结果会被折叠进上下文，注意不要重复罗列。",
    },
  ],
};

const settings: SettingsView = {
  blurCollapse: true,
  projectDirAutoread: false,
  builtinToolsEnabled: true,
  /**
   * 演示用「非默认」值 —— 让预览件里三选一默认选中「本机 Git Bash」，
   * 这样一眼能看出"选中项"长什么样（而不是永远停在 pwsh 那条）。
   * ⚠️ 真机老用户升级后没有这个字段 ⇒ 回落 pwsh（行为不变）。
   */
  execShell: "gitbash",
};

const lifeSources: LifeSourceView[] = [
  {
    id: "wechat",
    label: "微信聊天存档",
    enabled: true,
    kind: "file",
    target: "D:\\wechat\\dec",
    itemsPath: "D:\\wechat\\dec\\msg\\file",
    staleHours: null,
    complete: true,
    ok: true,
    error: "",
    updatedAt: "2026-10-22T02:14:00Z",
    ageHours: 38.1,
    stale: false,
    count: 1842,
    preview: ["2026-10-21 微信支付账单.xlsx", "2026-10-22 群聊记录.txt", "设计方案 v3.png"],
  },
  {
    id: "todo",
    label: "待办清单",
    enabled: true,
    kind: "command",
    target: "orbcat todo list --json",
    itemsPath: "",
    staleHours: 24,
    complete: false,
    ok: true,
    error: "",
    updatedAt: "2026-10-23T08:02:00Z",
    ageHours: 0.3,
    stale: false,
    count: 6,
    preview: ["[ ] 补 settings 页的压缩水位三档", "[x] 前端接口化", "[ ] 写预览脚本"],
  },
  {
    id: "claude",
    label: "Claude Code 会话",
    enabled: false,
    kind: "file",
    target: "C:\\Users\\DELL\\.claude\\projects",
    itemsPath: "",
    staleHours: 72,
    complete: true,
    ok: false,
    error: "目录不存在：.claude\\projects",
    updatedAt: "2026-10-19T11:40:00Z",
    ageHours: 101.2,
    stale: true,
    count: 0,
    preview: [],
  },
];

const life: LifeStatus = {
  path: "D:\\workplace\\悬浮小agent\\agent-data\\life.json",
  sources: lifeSources,
};

const skills: SkillInfo[] = [
  {
    name: "disk-usage-audit",
    description: "只读扫描分区占用排行，枚举回收站内容，给出可回收清单",
    path: "C:\\Users\\DELL\\.minimax\\skills\\disk-usage-audit",
  },
  {
    name: "docx-report-fill",
    description: "在既有的实验报告模板上就地填结果，并插入自己生成的 UML 图",
    path: "C:\\Users\\DELL\\.minimax\\skills\\docx-report-fill",
  },
  {
    name: "ardot-design",
    description: "读 Ardot 设计稿节点树，把 fills/strokes 落到代码里",
    path: "C:\\Users\\DELL\\.minimax\\skills\\ardot-design",
  },
  {
    name: "windows-rust-build-lock",
    description: "排查 cargo build 报 LNK1104 无法打开 deps\\*.exe",
    path: "C:\\Users\\DELL\\.minimax\\skills\\windows-rust-build-lock",
  },
];

const usage: UsageRecord[] = [
  { session: "s-main", model: "MiniMax-M2.1", at: at(15, 2, 40), usage: { prompt: 2841, completion: 396, total: 3237, cacheHit: 1536 } },
  { session: "s-main", model: "MiniMax-M2.1", at: at(15, 6, 50), usage: { prompt: 3917, completion: 512, total: 4429, cacheHit: 2048 } },
  { session: "s-task-1", model: "claude-sonnet-4-5", at: at(11, 22), usage: { prompt: 12480, completion: 2140, total: 14620, cacheHit: 8192 } },
  { session: "s-task-2", model: "gpt-4.1", at: at(13, 51), usage: { prompt: 8302, completion: 1877, total: 10179, cacheHit: 4096 } },
  { session: "s-task-3", model: "deepseek-chat", at: at(9, 58), usage: { prompt: 22140, completion: 4021, total: 26161, cacheHit: 12288 } },
];

const searchStatus: SearchStatus = {
  provider: "tavily",
  providerLabel: "Tavily",
  hasKey: true,
  keyPreview: "tvly-1...abcd",
  ready: true,
  path: "C:\\Users\\DELL\\.minimax\\config\\search.json",
};

const pmemProjects: ProjectRow[] = [
  { id: 1, name: "悬浮小agent", rootPath: "D:\\workplace\\悬浮小agent", createdAt: "2026-10-20T08:12:00Z" },
  { id: 2, name: "Mavis", rootPath: "D:\\workplace\\mavis", createdAt: "2026-10-22T13:40:00Z" },
];

const projectMemoryViews: ProjectMemoryView[] = [
  { name: "悬浮小agent", source: "D:\\workplace\\悬浮小agent\\MEMORY.md", sourceAlive: true, hasMemory: true },
  { name: "Mavis", source: "D:\\workplace\\mavis\\MEMORY.md", sourceAlive: false, hasMemory: false },
];

const projectList: ProjectListView = {
  bundles: [
    {
      id: "bundle-frontend",
      name: "前端工具箱",
      description: "读设计稿、跑 UI 用例、截图对比",
      active: true,
      valid: true,
      error: null,
      skillCount: 3,
      toolCount: 7,
      hasPermPreset: true,
    },
    {
      id: "bundle-ops",
      name: "运维脚本",
      description: "构建、发布、磁盘巡检",
      active: false,
      valid: true,
      error: null,
      skillCount: 2,
      toolCount: 4,
      hasPermPreset: false,
    },
    {
      id: "bundle-broken",
      name: "实验性工具",
      description: "还在写的半成品",
      active: false,
      valid: false,
      error: "缺少必填字段 tools[].name",
      skillCount: 0,
      toolCount: 2,
      hasPermPreset: true,
    },
  ],
  active: "bundle-frontend",
  unresolved: [["tool", "legacy-screenshot", "找不到定义：screenshot.compare"]],
  permError: null,
  tools: [
    { name: "read_design", description: "读 Ardot 设计稿节点", template: ["fileId", "nodeId"], perm: "read" },
    { name: "run_ui_tests", description: "跑 UI 用例", template: ["project"], perm: "exec" },
    { name: "capture_screenshot", description: "截当前窗口", template: ["out"], perm: "exec" },
  ],
};

const fgHistory: FgCtx[] = [
  { app: "Code.exe", title: "orbcat · src/data/source.ts", dir: "D:\\workplace\\悬浮小agent\\orbcat", file: "D:\\workplace\\悬浮小agent\\orbcat\\src\\data\\source.ts" },
  { app: "WindowsTerminal.exe", title: "Git Bash", dir: "D:\\workplace\\悬浮小agent", file: null },
  { app: "explorer.exe", title: "悬浮小agent", dir: "D:\\workplace", file: null },
];

const contextCapability: ContextCapability = {
  level: "full",
  detail: "已授权读取前台窗口标题与路径",
  canRequest: true,
  needsSystemSettings: false,
};

const piStatus: PiStatus = {
  available: true,
  runtimeDir: "D:\\orbcat\\runtime\\pi",
  agentDir: "C:\\Users\\DELL\\.minimax\\agents\\mavis",
  bridgePort: 41_517,
  liveSessions: ["sess-main"],
  maxLive: 4,
  currentSession: "sess-main",
  /**
   * 运行时组件状态 —— **演示 pi 真的在用**的那份数据。
   *
   * node ✓ / pi ✓ / python ✗（默认包不含）/ git ✓（用系统的）——
   * 这正是用户能一眼看出「缺什么」的样子。
   */
  runtime: {
    runtimeDir: "D:\\orbcat\\runtime",
    ready: true,
    node: "D:\\orbcat\\runtime\\node\\node.exe",
    piBundle: "D:\\orbcat\\runtime\\pi",
    python: null,
    git: "C:\\Program Files\\Git\\cmd\\git.exe",
    gitSource: "system",
    bash: "C:\\Program Files\\Git\\bin\\bash.exe",
    bashSource: "system",
    notes: ["python 不在发布包里（需要时用系统装的或自行指定路径）"],
  },
};

const bootState: BootState = {
  needsInit: false,
  hasModels: true,
  dataDir: "D:\\workplace\\悬浮小agent\\agent-data",
  dataDirExists: true,
};

const bgDesk: BgDeskStatus = { enabled: false, available: true, error: null, windows: [] };

// ---------------- 组装 ----------------

/** 一个完整的 mock 世界（可被场景覆盖） */
export interface MockData {
  session: Session;
  list: SessionMeta[];
  models: ModelView[];
  selectedModel: { id: string; url: string } | null;
  pending: Candidate[];
  memFiles: MemoryFiles;
  memStats: MemStats;
  permRequests: PendingPerm[];
  permRules: PermRule[];
  cmdPolicy: CmdPolicy;
  cmdGrants: CmdGrantsView[];
  cmdAudit: CmdAuditRecord[];
  mcp: McpStatus;
  mcpServers: McpServerCfg[];
  recovery: RecoveryState;
  settings: SettingsView;
  modes: ModesView;
  life: LifeStatus;
  skills: SkillInfo[];
  usage: UsageRecord[];
  search: SearchStatus;
  projects: ProjectListView;
  pmem: ProjectRow[];
  projectMemory: ProjectMemoryView[];
  fg: FgCtx[];
  contextCapability: ContextCapability;
  pi: PiStatus;
  boot: BootState;
  bgDesk: BgDeskStatus;
  builtinTools: { name: string; desc: string }[];
  bundles: { id: string; name: string; description: string }[];
}

const mainSession: Session = {
  id: "s-main",
  title: "主对话",
  createdAt: at(9, 12),
  updatedAt: at(15, 7),
  messages: msgs,
  kind: "main",
  mode: "standard",
};

/** 默认世界：一条有内容的会话 + 全部辅助域都有数据 */
export function defaultMockData(): MockData {
  return {
    session: { ...mainSession, messages: [...msgs] },
    list: sessionList.map((s) => ({ ...s })),
    models: models.map((m) => ({ ...m })),
    selectedModel: { id: "MiniMax-M2.1", url: "https://api.minimaxi.com/v1" },
    pending: pendingCandidates.map((c) => ({ ...c })),
    memFiles: { ...memFiles },
    memStats: { pending: pendingCandidates.length, handled: 37 },
    // ⚠️ 默认场景**不放**待批权限申请 —— 权限卡由「权限卡」那个场景专门承担，
    // 混在对话里会把聊天流顶掉（用户打开预览第一眼看到的是权限卡，不是对话）。
    permRequests: [],
    permRules: permRules.map((r) => ({ ...r })),
    cmdPolicy: { allow: [...cmdPolicy.allow], hard_block: [...cmdPolicy.hard_block] },
    cmdGrants: cmdGrants.map((g) => ({ ...g })),
    cmdAudit: cmdAudit.map((a) => ({ ...a })),
    mcp: { ...mcpStatus, groups: mcpStatus.groups.map((g) => ({ ...g })) },
    mcpServers: mcpServers.map((s) => ({ ...s })),
    recovery: {
      ...recovery,
      stats: { ...recovery.stats },
      backups: recovery.backups.map((b) => ({ ...b })),
      trash: recovery.trash.map((t) => ({ ...t })),
    },
    settings: { ...settings },
    modes: { ...modes, modes: modes.modes.map((m) => ({ ...m })) },
    life: { ...life, sources: life.sources.map((s) => ({ ...s })) },
    skills: skills.map((s) => ({ ...s })),
    usage: usage.map((u) => ({ ...u })),
    search: { ...searchStatus },
    projects: {
      ...projectList,
      bundles: projectList.bundles.map((b) => ({ ...b })),
      unresolved: projectList.unresolved.map((u) => [...u] as [string, string, string]),
      tools: projectList.tools.map((t) => ({ ...t, template: [...t.template] })),
    },
    pmem: pmemProjects.map((p) => ({ ...p })),
    projectMemory: projectMemoryViews.map((p) => ({ ...p })),
    fg: fgHistory.map((f) => ({ ...f })),
    contextCapability: { ...contextCapability },
    pi: { ...piStatus },
    boot: { ...bootState },
    bgDesk: { ...bgDesk, windows: [] },
    builtinTools: [
      { name: "orb_read_file", desc: "读文件（受权限网关约束）" },
      { name: "orb_write_file", desc: "写文件（受权限网关约束）" },
      { name: "orb_edit_file", desc: "改文件（受权限网关约束）" },
      { name: "orb_run_command", desc: "执行命令（受命令策略约束）" },
      { name: "orb_search", desc: "联网搜索" },
    ],
    bundles: [
      { id: "bundle-frontend", name: "前端工具箱", description: "读设计稿、跑 UI 用例、截图对比" },
      { id: "bundle-ops", name: "运维脚本", description: "构建、发布、磁盘巡检" },
    ],
  };
}

// ---------------- 场景 ----------------

/**
 * 场景 = 一组「要展示哪一屏、什么状态」的覆盖。
 *
 * 覆盖字段用**工厂函数**，每次取新对象 —— 场景之间不会互相污染。
 */
export interface MockScenario {
  id: string;
  /** 中文名（预览页下拉里显示） */
  label: string;
  /** 一句话说清这屏要展示什么 */
  desc: string;
  /** 覆盖哪些域 */
  cover: (d: MockData) => void;
  /** 每个命令的额外延迟（毫秒）—— 用来渲染「加载中」 */
  delayMs?: number;
  /** 让这些命令失败（渲染错误态） */
  fail?: string[];
  /** 预览页自动导航到哪一屏（命令面板的命令名） */
  goto?: string;
  /**
   * 预览页在 boot 之后**模拟一次窗口拖放**（把文件拖到球上）。
   *
   * 为什么用模拟而不是造数据：拖放走的是 Tauri 的**窗口事件**（不是 invoke），
   * 只有真的发一次事件，前端那条链路才会被走一遍 —— 这才是「可 mock」的正解
   * （mock 的是**事件源**，不是把结果硬塞进状态）。
   */
  simulateDrop?: string[];
}

/** 空态：什么都没配过 */
const emptyCover = (d: MockData): void => {
  d.session = { ...mainSession, title: "新对话", messages: [...emptyMsgs] };
  d.list = [
    {
      id: "s-main",
      title: "新对话",
      createdAt: at(16, 20),
      updatedAt: at(16, 20),
      count: 0,
      current: true,
      kind: "main",
      projectName: null,
      mode: "standard",
    },
  ];
  d.models = [];
  d.selectedModel = null;
  d.pending = [];
  d.memStats = { pending: 0, handled: 0 };
  d.permRequests = [];
  d.permRules = [];
  d.cmdGrants = [];
  d.cmdAudit = [];
  d.mcp = { connected: false, error: null, url: "", activeTokens: 0, groups: [] };
  d.mcpServers = [];
  d.recovery = {
    stats: { backupsCount: 0, backupsBytes: 0, trashCount: 0, trashBytes: 0, staleRecords: 0 },
    backups: [],
    trash: [],
    defaultKeepDays: 30,
    defaultKeepItems: 200,
  };
  d.usage = [];
  d.skills = [];
  d.life = { path: "D:\\workplace\\悬浮小agent\\agent-data\\life.json", sources: [] };
  d.pmem = [];
  d.projectMemory = [];
  d.projects = { bundles: [], active: null, unresolved: [], permError: null, tools: [] };
  d.fg = [];
  d.search = { provider: "", providerLabel: "", hasKey: false, keyPreview: "", ready: false, path: "" };
  d.boot = { needsInit: true, hasModels: false, dataDir: "D:\\workplace\\悬浮小agent\\agent-data", dataDirExists: false };
  d.pi = { available: false };
  d.bundles = [];
};

/** 错误态：后端读不出来 */
const errorCover = (d: MockData): void => {
  d.session = { ...mainSession, messages: [] };
  d.list = [];
  d.memStats = { pending: 0, handled: 0 };
  d.pending = [];
  d.permRequests = [];
  d.mcp = { connected: false, error: "网关连不上：127.0.0.1:3050 拒绝连接", url: "", activeTokens: 0, groups: [] };
};

/** 压缩提示：上下文快满了 */
const compactCover = (d: MockData): void => {
  d.session = {
    ...mainSession,
    messages: [
      ...msgs,
      {
        role: "assistant",
        text: "这一轮上下文已经到 87%，再聊下去要开始丢早期内容了。我先把前面的结论压成摘要？",
        steps: [],
        usage: { prompt: 171_000, completion: 210, total: 171_210, cacheHit: 0 },
        model: "MiniMax-M2.1",
        at: at(15, 58),
      },
    ],
  };
};

/** 多会话：五条会话、主会话在跑 */
const multiSessionCover = (d: MockData): void => {
  d.list = sessionList.map((s) => ({ ...s, current: s.id === "s-task-2" }));
  d.session = { ...mainSession, id: "s-task-2", title: "把发布流程写成脚本", kind: "task", mode: "creator" };
};

/** 富内容聊天：工具调用 + 插话 + 错误 + 图片 */
const richChatCover = (d: MockData): void => {
  d.session = { ...mainSession, messages: [...richMsgs] };
  d.list = richList.map((s) => ({ ...s }));
};

/** 权限卡：三种申请都在（文件 / 命令 / 问用户） */
const permCover = (d: MockData): void => {
  d.permRequests = permRequests.map((p) => ({ ...p }));
  d.memStats = { pending: 0, handled: 37 };
};

/**
 * **pi 确认卡场景** —— 只挂一张 `kind="pi"` 的卡。
 *
 * ## 为什么单独一个场景
 *
 * 权限卡渲染在**对话流**里（`renderMessages()` 的 `.perm-cards` 容器），
 * 而现有的 `perm` 场景跳到的是设置›文件权限页（规则列表）⇒ **看不到卡**。
 * ⇒ 这条场景专门用来在纯 HTML 预览里**目视核对 pi 卡**
 * （判据⑧：mock 场景能渲染出同一张卡 ⇒ 证明渲染逻辑不依赖真 invoke）。
 *
 * 只放 pi 一张（不混别的卡）—— 混了截图里就看不清这张卡对不对。
 */
const piPermCover = (d: MockData): void => {
  d.permRequests = permRequests
    .filter((p) => p.kind === "pi")
    .map((p) => ({ ...p }));
  // 顺便给条上下文消息，让卡不是孤零零地贴在最上面
  d.session = {
    ...mainSession,
    messages: [
      ...msgs,
      {
        role: "assistant" as const,
        text: "这一步需要你确认一下。",
        at: at(15, 10),
      },
    ],
  };
};

/** 记忆审批：四条候选 + 统计 */
const memCover = (d: MockData): void => {
  d.pending = pendingCandidates.map((c) => ({ ...c }));
  d.memStats = { pending: pendingCandidates.length, handled: 37 };
};

/** 场景清单（预览页下拉 / 测试用） */
export const SCENARIOS: MockScenario[] = [
  {
    id: "default",
    label: "默认（主对话 + 多会话）",
    desc: "一条有工具调用、有思考的完整对话；会话列表 5 条",
    cover: () => {},
    goto: "chat",
  },
  {
    id: "empty",
    label: "空态（首次启动）",
    desc: "没模型、没会话、没记忆、没项目 —— 引导用户去配置",
    cover: emptyCover,
    goto: "chat",
  },
  {
    id: "loading",
    label: "加载中",
    desc: "所有命令延迟 3 秒 —— 渲染骨架/转圈态",
    cover: () => {},
    delayMs: 3000,
    goto: "chat",
  },
  {
    id: "error",
    label: "错误态",
    desc: "后端读不出来：会话空、MCP 断连、球变红",
    cover: errorCover,
    fail: ["session_state", "mcp_status", "mem_pending"],
    goto: "chat",
  },
  {
    id: "rich",
    label: "聊天富内容",
    desc: "工具调用 + 工具失败 + 插话 + 图片 + 思考",
    cover: richChatCover,
    goto: "chat",
  },
  {
    id: "sessions",
    label: "会话页（多会话）",
    desc: "主会话 + 3 任务会话 + 1 分叉，每行带模式点",
    cover: multiSessionCover,
    goto: "sessions",
  },
  {
    id: "perm",
    label: "权限卡（文件/命令/问用户）",
    desc: "三种待批申请同时在列",
    cover: permCover,
    goto: "perm",
  },
  {
    id: "pi-perm",
    label: "pi 确认卡（kind=pi）",
    desc: "pi 内核的确认框：标题 + 正文 + 只「允许 / 拒绝」",
    cover: piPermCover,
    goto: "chat",
  },
  {
    id: "mem",
    label: "记忆审批（4 条候选）",
    desc: "候选卡 + 来源标记 + 批准/驳回按钮",
    cover: memCover,
    goto: "mem",
  },
  {
    id: "compact",
    label: "压缩提示",
    desc: "上下文占用 87%，助手问要不要压缩",
    cover: compactCover,
    goto: "chat",
  },
  {
    id: "drop-path",
    label: "拖入路径（文件/文件夹）",
    desc: "把文件拖到球上：面板自动展开、路径进输入区、单行中间省略、不自动发送",
    cover: () => {},
    goto: "chat",
    simulateDrop: [
      "D:\\workplace\\悬浮小agent\\orbcat\\src\\views\\recovery.ts",
      "D:\\workplace\\悬浮小agent\\orbcat\\docs\\09-FRONTEND-HANDOVER.md",
    ],
  },
  {
    id: "mcp",
    label: "工具与 MCP",
    desc: "网关已连接，4 个组（2 个启用），stdio server 也在列",
    cover: () => {},
    goto: "mcp",
  },
  {
    id: "recovery",
    label: "备份与回收站",
    desc: "3 份备份 + 3 条归档 + 统计卡",
    cover: () => {},
    goto: "recovery",
  },
  {
    id: "project",
    label: "项目页",
    desc: "3 个技能包（1 个无效）+ 工具清单 + 解析失败项",
    cover: () => {},
    goto: "project",
  },
  {
    id: "usage",
    label: "用量台账",
    desc: "5 条用量记录 + 今日合计",
    cover: () => {},
    goto: "usage",
  },
  {
    id: "life",
    label: "life 数据源",
    desc: "3 个源：1 正常、1 陈旧、1 报错",
    cover: () => {},
    goto: "life",
  },
  {
    id: "behavior",
    label: "行为页（执行位置三选一）",
    desc: "执行位置 = 后台桌面 / 本机 pwsh / 本机 Git Bash 三选一",
    // ⚠️ 这个场景**要看到「本机 Git Bash」被选中** ⇒ shell 存 gitbash。
    //    （默认 fixture 里就是 gitbash；这里显式写一遍，防止以后改默认值时
    //     这个场景的演示效果被悄悄改掉。）
    cover: (d: MockData) => {
      d.settings = { ...d.settings, execShell: "gitbash" };
    },
    goto: "behavior",
  },
  {
    id: "kernel",
    label: "内核状态（pi 运行时）",
    desc: "pi 内核就绪 + node/pi/git/python 组件状态；掉一个组件看缺了谁",
    /**
     * 这个场景演示**组件缺失**的形态：python 与 git 都缺。
     *
     * ⚠️ 为什么专门造一个场景：用户实机问过「这个对得上吗」——
     *    「pi 没起来」最常见的原因就是**运行时缺组件**（node/pi bundle 没打进去）。
     *    只演示全绿的话，这个界面看起来永远是好的 ⇒ 看不出价值。
     *    ⇒ 这里让 git 与 python 缺失，能看到 ✕ 的样子。
     */
    cover: (d: MockData) => {
      d.pi = {
        ...d.pi,
        available: true,
        runtime: {
          runtimeDir: "D:\\orbcat\\runtime",
          ready: true,
          node: "D:\\orbcat\\runtime\\node\\node.exe",
          piBundle: "D:\\orbcat\\runtime\\pi",
          python: null,
          git: null,
          gitSource: "missing",
          bash: "C:\\Program Files\\Git\\bin\\bash.exe",
          bashSource: "system",
          notes: ["git 不在发布包里 —— Git Bash 与 git 命令会不可用"],
        },
      };
    },
    goto: "behavior",
  },
];

/** 按 id 取场景 */
export function scenarioById(id: string): MockScenario {
  return SCENARIOS.find((s) => s.id === id) ?? SCENARIOS[0]!;
}
