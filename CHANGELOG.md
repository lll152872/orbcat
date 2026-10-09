# Changelog

本文件记录 orbcat 的版本变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

---

## [未发布] — 桌面快捷方式改为「常驻」入口

起因：用户问「能不能给这个 agent 加一个常驻快捷方式，以后不管怎么更新，
桌面上的快捷方式始终指向能用的那个」。同时问「为啥 WB 能更新完还能打开原来的」。

**WB 为什么不会踩这个坑**：WorkBuddy 是**安装到固定路径**的商业软件
（`D:\新建文件夹 (3)\WorkBuddyAI\WorkBuddyAI.exe`），升级时**原地替换同名文件**，
快捷方式里那条目标路径自始至终没变过。orbcat 是源码构建产物，没有安装器，
文件名/目录会随构建档位与归档策略变化，所以「目标路径不变」这个性质
得自己补出来。

### 修复

**桌面快捷方式不再指向任何 exe**（根治 2026-09-26 那次「新功能全部消失」）

- 原方案是「让快捷方式指向 `_old-pretrust-orbcat.exe`，并保证那个文件始终是最新
  构建」—— 一个补丁。只要构建脚本忘了同步，或有人直接 `cargo build`，
  用户双击启动的就是旧构建。**补丁已删除。**
- 新调用链，每一环路径都固定：
  `桌面 orbcat.lnk → wscript.exe → tools\orbcat-launch.vbs →
   tools\orbcat-launch.ps1 → 运行时挑出"能用的那份" exe`
- 「能用」判定三条缺一不可：有效 PE 且 > 1 MB；**同目录有 `dist\index.html`**；
  候选按 **`min(exe 时间, dist 时间)`** 取最新。
  最后一条是实测逼出来的：`release-fast` 的 exe 是 10-01 但 dist 停在 09-29，
  只看 exe 时间会选中「新内核 + 旧界面」。
- `_old_builds/` 里的归档没有 `dist/`，天然出局，不需要额外排除规则。

**中间夹 `.vbs` 是为了不闪控制台**

- `.lnk` 不能直接指向 `.ps1`（无 ShellExecute 关联，双击弹「你要如何打开这个文件？」）。
- 指向 `pwsh.exe` 会闪黑框 —— `-WindowStyle Hidden` 是**先创建、后隐藏**。
- `wscript.exe` 是 GUI 子系统、自身无控制台，用 `SW_HIDE` 启动 pwsh，
  控制台创建那一刻就是隐藏的。

### 新增

- `tools/orbcat-launch.vbs` —— 无控制台宿主；带参数运行（`cscript //nologo
  tools\orbcat-launch.vbs -List`）走排查模式，把结果按 UTF-8 打出来。
- `tools/install-shortcut.ps1` —— 装 / 修 / 体检 / 卸载桌面（与开始菜单）快捷方式。
  `-Verify` 会把整条链走一遍并打印「现在会启动哪一份构建」。
- `tools/orbcat-launch.ps1` 新增 `-OutFile`：结果写 UTF-8 文件而非 stdout，
  供 VBS 取回（避开控制台代码页把中文变乱码）。

### 变更

- `tools/build-release.sh`：删掉 `cp orbcat.exe _old-pretrust-orbcat.exe` 同步步骤，
  改为构建后跑一次 `install-shortcut.ps1 -Verify` 体检。

---

## [未发布] — 数据源（认知面注入）

起因：用户问「多数据源能不能设计为一个 MCP 工具」。**实测可行** ——
我写了个 ~200 行的 Python MCP server，用 orbcat 真实的 `McpClient`
（stdio）连它，读 campus-monitor 的学习通快照，全链路跑通。

但那 200 行里绝大部分是**协议样板**，而且每加一个源就要再写一个 server。
更关键的是**触发权归属不同**：

| | MCP 工具 | 数据源 |
|---|---|---|
| 是什么 | 能力面：agent 能**做**什么 | 认知面：agent **知道**什么 |
| 谁触发 | **模型**决定去调 | **系统**决定要不要呈现 |
| 失败语义 | 调用失败，换法子 | 快照过期 → 静默降级 + 摘要里说明 |

用户问「帮我安排下今天」时，模型**没有理由**先去调一个 `life_get_items`
—— 而「更懂你」的前提恰恰是不用说就知道。这不是把工具做多能解决的。

### 新增

**`life.rs`：一个源 = 一段声明，不是一段代码**

- `agent-data/life.json`：加一个源就是加一段 JSON（6 行），不用写代码。
  - `kind: file` —— 读现成的 JSON 快照（爬虫产出，**零代码接入**）
  - `kind: command` —— 跑一条命令、stdout 当 JSON 收（邮件、任意脚本）
- 极小的 JSON 路径实现（`$.works[*]` / `$.a.b` / `$.a[0]`），不引新依赖。
- **刻意不做 `kind: mcp`**：MCP 工具模型本来就能直接调，再包一层只会
  多一条会过期的副本。两个面各自解决自己的问题。

**注入策略（用户 2026-10 拍板：一行摘要常驻 + 详情按需）**

- system prompt 里每个源只占**一行**（条数 + 新鲜度 + 过期标记）；
- 细节要模型调新工具 `life_items` 才拿；
- 60 秒摘要缓存（否则每轮都付一次进程启动的钱）；
- 没有任何源时整段不出现（不浪费 token）。

**设置 › 数据源 一页**

- 每个源的声明 + 实时读取结果（条数 / 多久没更新 / 读没读到 / 明细预览）；
- 开关落盘；配置不完整的源不让打开（并说明缺哪个字段）；
- 页面上写明 `command` 类会**后台自动执行**（这条风险不能藏）。

### 安全边界

- `enabled` **缺省 false**，必须显式打开 —— `command` 类是在构建 prompt 时
  自动跑的，信任面比 MCP 大（MCP 是模型调才跑）；
- 有 15 秒超时，失败**静默降级**成一行"读取失败"，绝不打断对话主流程；
- 采集结果只进那一行摘要与 `life_items`，不写记忆、不进审计正文。

### 修复

- `bootstrap` 的幂等测试原本断言 `skipped.len() == 21` 这个**写死的总数** ——
  每加一个骨架文件都要来改这一行，而改的人很容易把数字改对却没意识到
  自己可能漏了真正的断言。改成逐个断言目录名与文件名。

### 测试

- `life.rs` 33 条：JSON 路径（含 `[*]`、越界、非数组）、字段映射与兜底、
  真实快照形状解析、文件源读/缺失/坏 JSON、过期阈值、摘要（条数/过期/失败/
  截短/长度上限）、配置读写（缺省关闭、kind 大小写、向后兼容）、缓存 TTL。
- `flow.life.test.ts` 11 条：入口摘要、空态、条数/过期/失败三态、开关调后端、
  刷新、以及**页面上必须写明 command 会自动跑**与认知面/能力面的分工。

### 踩坑

- Windows 上 Python 的 `sys.stdout.encoding` 默认是 **GBK**，
  `json.dumps(ensure_ascii=False)` 吐出的中文是 GBK 字节，而 Rust 侧
  `BufReader::lines()` 按 UTF-8 校验 → 乱码或报错。必须
  `sys.stdout.reconfigure(encoding="utf-8")`。与 `shell.rs::wrap_utf8` 同源。

---

## [未发布] — PTC 模式重做（真正的程序化工具调用）

起因：用户提出「感觉三个模式都没啥区别啊」，并指出「标准下不能用 mcp」。
对照 DSH 的 PTC 实现（`dsh-agent-tool-presentation` + `dsh-tools` 的 ptc mode）
核查后确认：**模式之间确实没有实质区别** —— 标准 / 极简 / 创造三者的
`mcpGroupsPreload` 都是 `[]`，逐字节相同，行为指令也走同一分支。

### 新增

**`ptc.rs`：把工具调用写成程序**

- 新增 `toolPresentation` 模式维度（`native` / `ptc`，缺省 native，
  老 `modes.json` 不用改）。PTC 下 `tool_specs` **坍缩成只返回 `run_code`**，
  其余工具以**程序内 TypeScript SDK 声明**的形式进 system prompt。
  两者同源于 `all_tool_specs`，保证"公告的 = 能调的"。
- **坍缩同时作用在"可调用面"上**：PTC 下模型直接发别的工具名会被拒，
  并得到"请写程序、用 `tools.x(...)` 调"的指引。只藏列表不拦调用，
  等于公告一套、执行另一套 —— 模型会学到"公告不可信"然后乱试工具名。
  （DSH 的 `dsh-tools` 把这条做成 `UNKNOWN_TOOL`。）
- `run_code` 把模型写的 `code` 包成 `.ts`，起 Node 子进程，用**行式 JSON 协议**
  双向通信。Node 用原生**类型擦除**执行 TypeScript（不需要 tsc / esbuild）。
- 程序里的 `tools.x(args)` **复用同一个 `tools::execute`** ——
  文件权限网关、命令策略、权限卡、审计链、输出截断全部照旧。
  PTC 不新增信任面。
- 三条防呆闸门：子调用总数上限 500（防模型写错循环条件）、
  并发上限 10（与 native 侧并行池同量级）、程序输出上限 64 KB
  （否则一行 `console.log(所有内容)` 就把"中间结果不进上下文"的好处抵消掉）。
- 子调用失败 → 程序里 reject（`ToolCallError` 带 `toolName` / `message`），
  `try/catch` 后可继续跑 —— 对应"个别文件读失败也要把报告写完"。

### 修复

- **标准模式原本是 `mcpGroupsPreload: []`** → 默认配置下 MCP 工具一个都用不了：
  去设置 › MCP 点某组的「给模型」，后端 `load()` 被模式闸门拦下报
  「当前 agent 模式不允许使用组」，而那个开关**连 `settings.json` 都没写进去**
  （错误发生在保存之前）。现在标准模式 = `"all"`。
- 前端「设置 › 行为」页现在显示每个模式的**调用方式**
  （native 逐个直接调用 / PTC 写成程序）—— 这是模式之间真正的差别，
  不显示出来用户会以为"切了模式但什么都没变"。

### 测试

- `ptc.rs` 36 条：SDK 声明生成、协议解析、UTF-8 安全截断，
  以及 **9 条真起 Node 的端到端**（类型擦除 / 工具派发 / `Promise.all` 并发 /
  失败可 catch / 语法错误 / `console.log` 捕获 / 死循环被掐断 / 临时目录清理）。
- `tools.rs` 6 条：走**真实 `execute`**（含权限网关）读文件、
  子调用失败可捕获、拒绝嵌套 `run_code`、空 code 报错、
  **坍缩拦住模型直接调别的工具**、**但程序内的调用不被拦**（这一对互为对照）。
- `modes.rs` 新增 6 条：只有 PTC 是 ptc 呈现、标准必须给全部组、
  老 JSON 向后兼容、用户可自定义、大小写不敏感。

### 踩坑（都有测试钉住）

- `.ts` 会被当 CJS 解析：Node 按**沿目录树往上找的第一个 `package.json`
  的 `type` 字段**决定 ESM/CJS。用户家目录一个 `{"type":"commonjs"}`
  就让 `export default` 报语法错，且报错行指向我们生成的包装代码。
  → 工作目录里必须写 `{"type":"module"}`。
- stdin 回写必须串行，否则多子调用交错写 → Node 读到半截 JSON → 永久挂起。
- 主循环必须同时推进"在飞的子调用"，只读 stdout 会死锁。
- 不能用 `tokio::spawn` 驱动子调用（要求 `'static`，而 `ToolCtx` 是借用的），
  改用 `FuturesUnordered` 在作用域内并发。

---

## [未发布] — 插件化 P0 / P1 / P2

起因：用户提问「我这个 agent 能不能插件化，还是说都是 rust 没办法」。
评估结论见 [`docs/compose/spec/plugin-feasibility.md`](../docs/compose/spec/plugin-feasibility.md)：
**能，而且大部分早就插件化了** —— 扩展点是 JSON 与 Markdown，不是 Rust 代码。
本轮把评估里排出的 P0/P1/P2 三层全部落地。

### 新增

**P0：MCP stdio 传输（「一个脚本 + 几行 JSON 就是工具」）**

- `McpServerCfg` 新增 `transport` / `command` / `args` / `env` 四个字段。
  缺省 `transport: "auto"` 按字段推断（有 `command` → stdio，否则 http），
  所以**存量 `mcp.json` 一行都不用改**。
- `McpClient` 拆成 `HttpTransport` / `StdioSession` 两条通道，执行侧**完全同权**：
  都走 `mcp__<server>__<tool>` 命名、都经 `tools.rs` 同一段 MCP 分支
  （权限卡 + 执行档位判定）。**stdio 不新增信任面**，它只是换了条
  "怎么把 JSON-RPC 送过去"的通道。
- stdio 会话**懒启动 + 跨调用复用 + 死了自动重起**；换行分隔 JSON-RPC，
  严格按 `id` 匹配响应（跳过 notification 与并发响应 —— 不跳会随机串包）。
- 设置 › MCP 页支持 stdio server 的增删改（前端改为**数据驱动**：
  `args[]` / `env{}` 不是可读回来的文本，从 DOM scrap 会丢参数边界）。
- 修一个会导致 stdio **静默全失效**的 bug：`resolve_mcp_servers` 早先按
  `url` 非空过滤，而 stdio server 本来就没有 url → 重启后配好的 stdio
  server 全部消失，日志里只写"MCP 未配置"。

**P1：拼装项目 L0（`project.rs`）**

- `agent-data/projects/<名>/project.json`：一套配好的东西打包成可保存 /
  切换 / 分享的项目。与既有项目记忆目录**合并**（没有 `project.json`
  的目录仍然只是项目记忆，行为不变）。
- 装配内容：技能范围 / prompt 片段（`RULES.md` 追加在全局红线之后）/
  模型槽位 / 搜索槽位 / 权限预设 / L1 脚本工具。
- **凭据永不进 bundle**：`slots.model` / `slots.search` 只写 id。
- fail-closed 与 fail-open **分流**（这是本轮最重要的设计）：
  - `schemaVersion` 不认识 / **缺失** → **整包拒载**
  - `permPreset` 任一处不合法 → **整份丢弃**（绝不半生效），设置页显眼告警
  - 模型 / 搜索 / prompt 片段引用失效 → **该项跳过 + 标黄**，不整包失败
- 设置 › 项目 页：激活 / 退出 / 编辑（JSON 文本框）/ 删除 / 模板，
  失效引用与丢弃原因逐条列出。

**P2：L1 脚本工具 + 注入穿透验证**

- `tools[].commandTemplate` 是 **argv 数组**（不是整串命令），
  `{project}` / `{param.*}` **逐参替换、绝不再拆分**。
- 新增 `shell::run_argv`：`Command::new(argv[0]).args(argv[1..])` ——
  **不经任何 shell 解释器**，`;` `|` `$()` 全是普通字符。
- 加载期即拒绝**占位符出现在解释器内联串**（`-Command` / `-c` / `/c` /
  `-e` 之后），那是把用户输入喂给解释器。
- 新增 `gate_command`：把 `run_command` 的整条 Gate 2 抽成**唯一**实现，
  项目工具与 `run_command` **共用同一套判定**（硬阻断 → 白名单 → 临时授权
  → 防骚扰 → 档位 → 弹卡）。各写一份的结果是两边语义慢慢分叉，
  而分叉的那一侧就是漏洞。

### 变更

- `tools::tool_specs` 新增 `data_dir` 参数（要读当前项目的脚本工具）。
- `AgentSettings` 新增 `active_bundle`。⚠️ 与既有的 `active_project_id`
  **不是一回事**：前者是拼装包（全局生效），后者是项目记忆绑定（随会话）。
- 命令执行与 MCP 免问判定改读 `project::effective_exec_trust` /
  `project::effective_policy`（项目预设**只做加法**，`hard_block` 永不放宽）。
- `skills::{catalog, autoload, load}` 三处**一致地**过项目技能过滤 ——
  少一处就是漏洞（模型能凭记忆直接点名 `load_skill` 读回被排除的技能）。

### 安全

- **拼装层在闸门之下，永不绕过**：`permPreset` 只做白名单加法与档位预选。
- 预设**不得**把硬阻断命令加进白名单（装配期显式拒绝，把隐式保证变成契约）。
- 预设**不得**授权盘根 / 主目录级别的路径（分享来的包不该一次拿到一整片盘）。
- 项目工具名不得与内置工具撞名、不得占用 `mcp__*` 命名空间
  （项目分支排在 MCP 分支之前，否则能劫持真 MCP 工具）。
- prompt 片段必须是 bundle 目录内的**相对文件名**（绝对路径 = 数据外泄通道）。

### 测试

- Rust `cargo test --lib`：**356 passed**（本轮新增 46 条）。
- 前端 `npm run test:ui`：**61 passed**（新增 `flow.project.test.ts` 10 条）。
- **注入穿透测试是真起子进程的端到端验证**：脚本回显收到的参数，
  断言 `; New-Item ...` 这类载荷**原样作为普通字符到达**且**没有副作用**。
- 另有 1 条 `#[ignore]` 的真实第三方 server 验证
  （`mcp::tests::real_github_stdio_roundtrip`，用本机 github-mcp-server.exe
  跑完整 stdio 往返：45 个工具 + 真实 `get_me` 调用 + 会话复用）。

---

## [0.2.0] — 2026-09-30

首个"功能完整、可日常使用"的版本。相对 v0.1.0（初始提交：Tauri 2 悬浮球骨架），
这一版把悬浮球做成了一个真正能干活、且**权限可控**的私人 agent。

### 新增

**会话与并发**

- 多会话运行注册表：多个会话可同时跑，互不干扰
- 对话中**插话**（`chat_steer`）：一轮进行中再次发送，不会丢，改为插话
- 取消正在跑的一轮
- `ask_user`：模型可在对话中主动向你提问并等待回答

**权限与安全**

- 执行权限档位（`ask` / `smart` / `full`）：输入框左侧 chip 切换，
  硬阻断在任何档位下都生效
- 权限申请通道（`request_access`）：被拒不再是死路，模型可在对话内发起申请，
  你在对话内批准（阻塞式 + 3min 超时 fail-closed + 防骚扰）
- 命令授权持久化（`command_grants.json`）：跨会话、跨重启存活
- 命令策略静态归一化：拆复合命令、别名展开、参数缩写展开、动态解析检测
- 审计哈希链：`agent-data/audit-commands.jsonl`

**记忆与检索**

- 跨会话检索：`recall_turns` 新增 `scope`（默认 `session` 保持旧行为，
  `all` 跨会话并按时间倒序、带来源标识；两道扫描闸 40 文件 / 60 MB）
- 能力发现索引：`search_tools` 按意图搜 + `capability_index` 常驻摘要 +
  `mcp_tool_index.txt` 落盘缓存。**成本实测 ≈ 2375 token / 127 个工具**，
  是完整 schema（≈14k token）的 **17%**
- 项目记忆索引（`pmem`）

**备份与恢复**

- 备份 / 回收站**还原入口**（新增 `recovery.rs`）：备份补记**原路径**、
  一键还原、拒绝覆盖、保留策略
- 崩溃恢复：流式正文按 3 秒节流增量落盘（治"退出后整轮蒸发"）
- 一键诊断包：约 40 KB、key 全脱敏，含目录体积 / 模型 / 日志尾部 / 会话错误

**上下文与成本**

- 给模型注入**当前时间**（放在动态区，不击穿 prompt cache）
- 修复 auto-compact 触发量：改用**上一轮服务端返回的真实 prompt token**
  作锚点 + 倍率自校准 + 循环内复查（原先估算的是 `history`，低估 4.9×~144×，
  导致自动 compact **一次都没触发过**）
- 每轮 cache 命中率度量：后端逐轮记录 + 设置页命中率卡 / 每天 / 每模型徽标

**桌面能力**

- 后台模式（隐形桌面）：`CreateDesktop` + 把 GUI 程序启动到该桌面 +
  用 `PrintWindow(PW_RENDERFULLCONTENT)` 抓真实画面。
  **限制：该桌面上无法合成键鼠输入**（内核拒绝），输入通道须走应用自身自动化接口
- 前台应用指示条：可翻列表，折叠态只显 1 条 + 「▾更多 N」角标
- 单实例锁 + 第二实例唤醒主实例
- 睡眠语义修正：球**真的从屏幕消失**（`hide()`），任务不受影响；
  唤醒走托盘 / 再启动一次 exe

**其他**

- MCP 网关地址去硬编码：删除 `DEFAULT_MCP_URL`，未配置即不启用
- 配置脱敏
- `fetch` 正文抽取
- Token 用量面板（含每日 / 每模型维度）
- 前端拆分：`main.ts` 拆出 `format.ts` / `models.ts` / `usage.ts` / `views/`

### 修复

- `load_tool_group` 与 `search_tools` 拼出**不存在的 MCP 工具名**
  （`mcp__github__github_1mcp_create_issue`），与 `tool_specs` 口径分叉 ——
  已统一到 `mcp_tool_callable_name()` 并加防回归断言
- `fmt_local` 跨日不进位
- `weekday_cn` 用 UTC 天算星期
- 启动时 6 行假的「grants.json 解析失败」警告（`read_session` 现在显式跳过
  `<id>.grants.json`）
- 「睡眠态点击应用图标无效」
- `flow.timeline.test.ts` 3 个红测试（测试用旧事件信封 `{kind}`，
  代码早已改成 `{sessionId,p}`）
- 悬浮球外黑框
- `indexmap` 补 `std` feature，修复 release 构建失败（E0107）
- 清理 6 条 dead-code 编译警告（`win32desk.rs` 中尚未接线的 API，
  改为说明性 `#[allow(dead_code)]` 而非删除）

### 变更

- 项目更名 `float-agent` → `orbcat`
- 移除分发 WorkBuddy 功能与 `dispatch-workbuddy` 技能（退回设计阶段）
- 删除 `context::PAUSED` 开关（已无调用者；留着容易把"界面隐身"错接成"停采样"）
- 前台应用指示条不再常驻挤占聊天区

### 已知限制

- **隐形桌面无法合成键鼠输入**（见上）
- **UIA 选区对 Electron 应用不可靠**（VSCode 需 `--force-renderer-accessibility`；
  Typora 本机为改造版）；Win32 / WinUI 完全可用
- 命令策略编辑器**后端就绪、UI 未接**
- 策略为 `deny` 时**不隐藏工具**（尚无可见性过滤）
- 仅支持 Windows

### 验证基线

| 检查 | 结果 |
|---|---|
| `cargo test --lib` | **310 passed / 0 failed / 4 ignored** |
| `npm run test:ui` | **8 文件 / 51 用例全绿** |
| `npx tsc --noEmit` | 0 错误 |
| `npx vite build` | ✓ 通过 |
| `cargo build --release`（fat LTO） | ✓ 通过，**0 警告** |

### 未包含（明确不在本版范围）

- **可组合项目包**（T1–T7 未开工）
- **生活信号与提醒**（未实现）

### 分发说明

exe **不自包含前端**，必须与 `dist/` 同目录分发。详见 README「分发形态」。

---

## [0.1.0] — 初始提交

Tauri 2 悬浮球骨架。工具集 / 权限 / 会话的早期 WIP。
