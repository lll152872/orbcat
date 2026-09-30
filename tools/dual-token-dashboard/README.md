# dual-token-dashboard（随程序分发的内置技能）

这个目录**不是**给 agent 直接加载的技能，而是**模板**。

## 机制

```
tools/dual-token-dashboard/          ← 模板（在本仓库，进 git）
        │  include_str! 编译进二进制
        ▼
orbcat.exe
        │  首次初始化时释放（bootstrap.rs::bootstrap_agent_data）
        ▼
agent-data/skills/dual-token-dashboard/   ← 真实副本（被 gitignore）
```

- `BUNDLED_SKILLS` 常量在 `src-tauri/src/bootstrap.rs` 里，
  用 `include_str!` 把下面三个文件编进 exe —— 发布版没有源码目录，不编进去就跟不走。
- 释放时把 `{{DATA_DIR}}` / `{{SKILL_DIR}}` 两个占位符**替换成本机真实路径**，
  所以换台机器 / 换个数据目录都能用，不会指向某个烧死的旧路径。
- 幂等：目标文件已存在就跳过。`skills/` 同时是用户和 agent 的地盘
  （`save_skill` 往里写），无条件覆盖会毁掉他们的改动。

## 文件

| 文件 | 说明 |
|---|---|
| `SKILL.md` | 技能正文，**含占位符**，释放时渲染 |
| `scripts/gen_dual_dashboard.py` | 看板生成脚本（原样释放，无占位符） |
| `references/adapter-schema.md` | 数据源适配器契约（原样释放） |

## ⚠️ 改这个目录要注意

- **改完必须重新编译 exe**，否则跑的还是旧内容（编译期就嵌进去了）。
- 改了 `SKILL.md` 的正文，**已经释放过的副本不会自动更新**（幂等跳过）。
  要刷新：删掉 `agent-data/skills/dual-token-dashboard/` 再跑一次初始化。
- **不要在这里写死绝对路径** —— 用 `{{DATA_DIR}}` / `{{SKILL_DIR}}` 占位符。
- 测试展开逻辑请用**临时目录**（见 `bootstrap.rs` 里
  `bundled_skills_expand_into_arbitrary_dir`），**不要拿真实 `agent-data` 试**。

## 来源

本目录的渲染层与数据源适配均为本项目自研实现，无第三方派生代码。
