---
name: dual-token-dashboard
description: 生成并打开「双数据源 Token 消耗看板」（单文件离线 HTML，顶部 tab 切换 WorkBuddy 与本机小 agent 的 token 统计，含 KPI / 日历热力图 / 模型分布）。触发词（用户说）：「看看 token 用量」「看 token 用量」「token 用量」「我的 token」「看看 token」「token 消耗」「token 统计」「算一下花了多少 token」「看看花了多少 token」「用了多少 token」「token 花了多少」「token 看板」「token 面板」「打开 token 面板」「打开 token 看板」「生成本地看板」「生成看板」「打开看板」「打开面板看 token」「消耗看板」「双数据源看板」「两个来源的 token」「看两个来源的消耗对比」「两个来源的消耗对比」「WorkBuddy 和小 agent 的 token」「成本审计」「token 审计」「用量统计」「费用统计」「看看消耗」「生成 token 看板」。
---

# 双数据源 Token 消耗看板（本机自启动）

用户说"看看 token 用量"时，**你直接把整套跑起来：生成 HTML → 用默认浏览器打开 → 口头汇报数字**。
不要只把命令贴给用户让他自己敲。

渲染层只有一份，数据源可插拔（WorkBuddy / 小 agent）。

## 执行方式

**用 PowerShell 工具执行命令。** 这是唯一的硬前提。
把下面的脚本整段作为命令内容传进去（参数名按该工具的实际 schema，通常是 `command`；
若支持超时参数，给 150 秒左右 —— 脚本要扫上万个日志文件）。

## 步骤

### 1. 生成看板

```powershell
$py = (Get-Command python.exe -ErrorAction SilentlyContinue).Source
if (-not $py) { $py = (Get-Command python -ErrorAction SilentlyContinue).Source }
if (-not $py) { $py = (Get-Command py -ErrorAction SilentlyContinue).Source }
if (-not $py) {
  foreach ($c in @(
      "$env:LOCALAPPDATA\Programs\Python\Python313\python.exe",
      "$env:LOCALAPPDATA\Programs\Python\Python312\python.exe")) {
    if (Test-Path $c) { $py = $c; break }
  }
}

if (-not $py) {
  "ERR=找不到 Python，请先装 Python 3.8+ 或把它加进 PATH"
} else {
  $sk  = "{{SKILL_DIR}}\scripts\gen_dual_dashboard.py"
  $ad  = "{{DATA_DIR}}"
  $out = "{{DATA_DIR}}\dual-token-dashboard.html"
  & $py $sk --agent-data $ad --out $out 2>&1 | Out-String
  "EXIT=$LASTEXITCODE"
  if (Test-Path $out) { "SIZE=" + (Get-Item $out).Length }
}
```

正常输出：

```
written: ...\dual-token-dashboard.html (0.73 MB)
  [WorkBuddy] 会话 201 | 请求 16951 | 模型 16 | 分组 120
  [小 agent] 会话 0 | 请求 0 | 模型 0 | 分组 0
EXIT=0
SIZE=725132
```

**判据：`EXIT=0` 且 `SIZE` 大于 60 万。** 不满足就把输出读一遍排查，别猜、别假装成功。

### 2. 打开给用户看

```powershell
Start-Process "{{DATA_DIR}}\dual-token-dashboard.html"
```

走系统默认浏览器。纯静态单文件，**不需要起服务器**。

### 3. 读 AI 可读摘要（必做）

生成脚本会同时写出：

```
{{DATA_DIR}}\dual-token-dashboard.summary.json
```

**回答用户 token 用量问题时，直接读这份 JSON**，不要去解析 HTML。
关键字段：`sources[]` 里每个源的 `total_tokens / sessions / requests / top_models / daily / est_cost_cny`，以及顶层 `totals`。成本汇报必须带上「牌价粗估」说明。

### 4. 汇报（必做）

打开后直接讲数字，别让用户自己数：

- 两个源各自的 **合计 token / 会话数 / 请求数**
- 各源 **TOP 3 模型**
- 当前时间窗口（默认 7 天）
- 数据入口：HTML 给人看，`dual-token-dashboard.summary.json` 给 AI 读

## 两个数据源

| tab | 数据来自 | 扫描字段 |
|---|---|---|
| WorkBuddy | `~/.workbuddy/projects/**/*.jsonl` | `providerData.rawUsage / usage` |
| 小 agent | `{{DATA_DIR}}\sessions\*.json` | 消息里的 `usage`（本程序自己写的） |

### 小 agent tab 为空是正常的

`usage` 字段是面板改造**之后**才开始写入的，更早的历史会话没有这个字段。
从下一次真实对话起才会有数据。**别把空数据当成脚本坏了**，先确认改造后是否聊过新对话。

## 参数

```powershell
--projects  <目录>   # 覆盖 WorkBuddy 日志目录（默认 ~/.workbuddy/projects）
--agent-data <目录>  # 覆盖小 agent 数据目录
--out       <文件>   # 输出 HTML 路径
--summary-out <文件> # AI 可读摘要 JSON（默认与 --out 同目录 dual-token-dashboard.summary.json）
```

## 看板有什么

- KPI：合计 token / 日均 / 会话 / 轮次 / 输入输出比 / 缓存命中率 / 预估金额（元）
- 日活热力图（20 分钟粒度）、0–24 时模型山脊图、每日模型占比堆叠柱、单次请求分位
- 三级下钻：分组 → 会话 → 单次请求（散点 + 明细表）
- 1/3/7/30 天窗口 + 自定义日期区间；深浅色主题；tab 选择记在 localStorage

## 坑

- **命令输出可能不回显**：这台机器上执行完有时拿不到输出。
  解法 = 结尾 `| Out-String`，或 `| Set-Content <文件>` 落地后用 read_file 读。
- **成本估算只是牌价**：脚本内 `PRICE` 表按公开牌价算，用户套餐不同就不准。
  汇报时说明"**按牌价粗略预估**"。
- **没有任何执行手段时**：如实说"我目前没法执行命令"，**绝对不要假装生成了文件**。

## 接入新数据源

渲染层与数据源层解耦。加第三个源（Cursor / 自研 agent 等）只需在脚本里加一个 `scan_xxx()`
并注册进 `SOURCES`，面板层一行都不用改。契约见 `references/adapter-schema.md`。
