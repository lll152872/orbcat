# 数据源适配器契约（开放 schema）

渲染层唯一，数据源可插拔。接入新数据源**只需写一个 `scan_xxx()` 并注册进 `SOURCES`**，面板层（HTML/JS）一行都不用改。

## 适配器返回结构

```python
def scan_mytool():
    # ... 扫描你自己的日志 ...
    return {
        'key':    'mytool',       # 唯一标识（前端 tab 的 data-src）
        'name':   'MyTool',       # tab 显示名
        'ws':     [...],          # 分组名列表（没有分组就填一个固定名）
        'models': [...],          # 模型 id 表
        'sess':   [...],          # 会话列表，见下
    }

SOURCES = [scan_workbuddy, scan_agent, scan_mytool]   # 注册即可
```

`genMs` / `gen`（生成时刻）由主流程统一注入，**适配器不用管**。

## 会话与记录格式

```python
sess = {
    'w':  0,                  # 分组下标（对应 ws 数组）
    'id': 'abcd1234',         # 短 id（展示用）
    't':  '会话标题',
    'st': 29745542,           # 起始分钟戳（用于排序）
    'r':  [                   # 记录元组列表，按时间升序
        (分钟戳, 模型下标, total, input, output, 缓存命中, 缓存写入),
        #   int     int      int    int    int     int       int
    ],
}
```

**记录用定长元组而非对象**，是控制单文件体积的关键（WorkBuddy 16000+ 请求压进 0.7MB）。
单位：token 数；时间戳是 **epoch 分钟**（`ms // 60000`）。

## 字段口径对照

| 内部字段 | WorkBuddy 来源 | 小 agent 来源 | 缺失时 |
|---|---|---|---|
| total | `rawUsage.total_tokens` | `usage.total` | **必须有**，无则跳过该条 |
| input | `rawUsage.prompt_tokens` | `usage.prompt` | 0 |
| output | `rawUsage.completion_tokens` | `usage.completion` | 0 |
| 缓存命中 | `prompt_cache_hit_tokens` 或 `prompt_tokens_details.cached_tokens` | 无 | 0 |
| 缓存写入 | `cached_creation_tokens` / `cache_write_tokens` | 无 | 0 |

**没有 usage 的记录必须跳过**（不是记 0）——否则会把"没拿到"算成"没消耗"。

## 现有实现位置

脚本：`scripts/gen_dual_dashboard.py`
- `scan_workbuddy(projects)` —— 扫 `~/.workbuddy/projects/**/*.jsonl`
- `scan_agent(agent_data)` —— 扫 `<agent-data>/sessions/*.json`
- `SOURCES = [scan_workbuddy, scan_agent]` —— 注册表
- 注意：主流程另有一份 `src_order` 列表维护 tab 顺序，**加新源时两处都要加**。
