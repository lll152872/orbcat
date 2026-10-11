---
name: token-usage-report
description: 回答「某天/昨天用了多少 token、WB 和小 agent 各多少」的问题：直接读 dual-token-dashboard 生成的 summary.json（或先跑一次生成脚本），从中拿每日/每模型的请求数、总 token、输入/输出、粗估成本。用户不需要 HTML 看板时用。
---

# Token 用量统计（读 summary.json，别自己扫日志）

## 核心原则

**不要自己写脚本扫 `~/.workbuddy/` 或数据目录的 `sessions/`。** `dual-token-dashboard` 技能的 `gen_dual_dashboard.py` 已经把所有聚合做好了，写出一份 `dual-token-dashboard.summary.json`。**读那份 JSON 就行。**

历史上踩过坑：直接翻 `~/.workbuddy/traces/`、`session_usage`、数据库文件都拿不到 per-request token（那些地方根本没有 inputTokens 字段）。唯一可信的数据源是：

- **WorkBuddy**（**没装 WorkBuddy 的机器上这个源不存在，跳过即可，不要当成"数据丢了"**）：`~/.workbuddy/projects/**/*.jsonl`（`providerData.rawUsage`）
- **小 agent**：`<数据目录>/sessions/*.json`（消息里的 `usage` 字段）

两份都由 `gen_dual_dashboard.py` 读取。

## 步骤

### 1. 生成/更新摘要（如 summary.json 不存在或时间过旧）

先定位 Python —— **不要写死解释器路径**，换台机器就没有了：

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
if (-not $py) { "ERR=找不到 Python，请先装 Python 3.8+ 或把它加进 PATH"; exit 1 }

& $py "{{DATA_DIR}}\skills\dual-token-dashboard\scripts\gen_dual_dashboard.py" `
       --agent-data "{{DATA_DIR}}" `
       --out        "{{DATA_DIR}}\dual-token-dashboard.html"
```

- 超时给 150 秒（要扫上万个 jsonl）
- 输出格式：`written: ... (X MB)` + `[WorkBuddy] 会话 N | 请求 N | ... | token N | 粗估 ¥N` + `[小 agent] ...`
- 判据：退出码 0 且 HTML 大于 60 万字节
- 同时写 `dual-token-dashboard.summary.json`

> 若提示 `dual-token-dashboard` 技能没装（找不到上面那个脚本路径），先让用户装上该技能，
> 或者改用「打开看板」的说法转到 `dual-token-dashboard` 技能去生成。

### 2. 读摘要

```
read_file {{DATA_DIR}}\dual-token-dashboard.summary.json
```

关键字段：
- 顶层 `totals`：全窗口合计
- `sources[]`：两个数据源（`workbuddy` / `agent`）
  - `total_tokens / prompt_tokens / completion_tokens / cache_hit_tokens / cache_write_tokens`
  - `sessions / requests / models / groups`
  - `est_cost_cny / cost_note`
  - `last_request_local`
  - `top_models[]`：模型细分（`model / requests / total / prompt / completion / cost_cny`）
  - `daily[]`：**按天**的 `requests / total / prompt / completion`

### 3. 汇报

用户问「昨天/某天」时，从对应源的 `daily[]` 里筛那天，给一张对比表：

| 来源 | 请求 | 总 token | 输入 | 输出 |
|---|---:|---:|---:|---:|
| WorkBuddy | ... | ... | ... | ... |
| 小 agent | ... | ... | ... | ... |
| 合计 | ... | ... | ... | ... |

**必须带上**：
- 「按公开牌价粗估」标注，不给具体金额（除非用户问）
- 提示 cache 命中率（`cache_hit_tokens / prompt_tokens`），WB 侧通常在 90%+，实际账单会明显低于粗估
- 提示 `dual-token-dashboard.html` 已生成，可直接 `Start-Process` 打开
- **某一源没数据时明确说"没装"或"没数据"**，不要含糊过去

## 什么时候用哪个技能

| 用户说 | 用什么 |
|---|---|
| 「昨天/今天/某天花了多少 token」 | **本技能**（只读 summary.json） |
| 「打开看板」「生成 token 看板」「看两个来源对比」 | `dual-token-dashboard` |
| 「某个模型用了多少」 | 本技能（读 `top_models[]`） |

如果 summary.json 里没用户问的日期（比如用户问「下周几」），直接说「没数据」，别瞎猜。

## 成本估算口径

摘要里的 `est_cost_cny` 是 `gen_dual_dashboard.py` 内部 `PRICE` 表按公开牌价算的，**没扣缓存折扣**。

- WB 侧 `cache_hit_tokens` 通常占输入 90%+，实际账单通常只有粗估的 1/5 ~ 1/3
- 汇报时说「粗估 ¥X（含未折扣的缓存命中 token，实际账单会明显更低）」，**不要报一个精确金额当账单**
- 具体套餐价随用户订阅变动，无法准确换算

## 不要做的事

- ❌ 不要自己写 PowerShell / Python 去扫 `~/.workbuddy/traces/`、`session_usage`、数据库 —— 拿不到 per-request token，浪费时间
- ❌ 不要把 `session_usage.used`（上下文字节数）当 token 数
- ❌ 不要写死 Python 解释器的绝对路径 —— 换台机器就废了，用上面的探测写法
- ❌ 不要假装生成了 HTML —— 没跑出输出就说没跑
- ❌ 不要按输入/输出单独报金额（cache 命中率不公开时算不准），除非用户明确要

## 相关

- HTML 看板：`dual-token-dashboard` 技能
- 生成脚本：`dual-token-dashboard` 技能的 `scripts/gen_dual_dashboard.py`