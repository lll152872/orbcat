# Changelog

本文件记录 orbcat 的版本变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

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
