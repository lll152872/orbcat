# dual-token-dashboard（随程序分发的内置技能）

这个目录**不是**给 agent 直接加载的技能，而是**内置区的模板**。

## 机制

```
src-tauri/skeleton/                    ← 内置区（干净模板，唯一允许进包的地方）
│   └── manifest.json                  ← 声明：哪些文件、落到哪、要不要渲染占位符
        │  build.rs 遍历 + 闸门 + 算 seed
        ▼
orbcat.exe
        │  初始化时物化（bootstrap.rs::bootstrap_agent_data）
        ▼
agent-data/skills/dual-token-dashboard/   ← 真实副本（用户区，被 gitignore）
```

- 声明在 `src-tauri/skeleton/manifest.json`，**不在代码里**。加文件 = 往 manifest 加一条，
  不改 Rust。
- `build.rs` 在**编译期**遍历骨架、跑闸门、把内容烧进二进制。发布版没有源码目录，
  不编进去就跟不走。
- 闸门在这里、也在 `npm run check:no-user-data`。未声明的文件、本机绝对路径、
  编译缓存、凭证 —— 任一命中**直接构建失败**。
- 释放时把 `{{DATA_DIR}}` / `{{SKILL_DIR}}` 两个占位符**替换成本机真实路径**，
  所以换台机器 / 换个数据目录都能用，不会指向某个烧死的旧路径。
  （代价：`build.rs` 会因为**把构建机的绝对路径写进产物**而失败 —— 那是特性不是 bug。）
- 幂等：目标文件已存在就跳过。`skills/` 同时是用户和 agent 的地盘
  （`save_skill` 往里写），无条件覆盖会毁掉他们的改动。

## 文件

| 文件 | 说明 |
|---|---|
| `SKILL.md` | 技能正文，**含占位符**，释放时渲染 |
| `scripts/gen_dual_dashboard.py` | 看板生成脚本（原样释放，无占位符） |
| `panel/index.html` `panel.css` `panel.js` | 看板渲染层 |
| `references/adapter-schema.md` | 数据源适配器契约（原样释放） |
| `README.md` | **本文件**。机制说明 + 来源声明，`distribute: false` —— 只在源码树，不进二进制也不释放 |

## ⚠️ 改这个目录要注意

- **改完必须重新编译 exe**，否则跑的还是旧内容（编译期就嵌进去了）。
- 改了 `SKILL.md` 的正文，**已经释放过的副本不会自动更新**（幂等跳过）。
  要刷新：删掉 `agent-data/skills/dual-token-dashboard/` 再跑一次初始化。
- **不要在这里写死绝对路径** —— 用 `{{DATA_DIR}}` / `{{SKILL_DIR}}` 占位符。
  写盘符路径闸门会让构建失败：它会把构建者的用户名和磁盘布局分发给所有用户。
- 改完还要在 `src-tauri/skeleton/manifest.json` 里同步 `src` 字段，
  否则 build.rs 的白名单会因为「多出来一个没声明的文件」而失败。
- 测试展开逻辑请用**临时目录**（见 `bootstrap.rs` 里
  `builtin_skills_expand_into_arbitrary_dir`），**不要拿真实 `agent-data` 试**。

## 来源

本目录的渲染层与数据源适配均为本项目自研实现，无第三方派生代码。

> 2026-09-20 曾记录过「派生自第三方、与上游重合 95.9%」的结论。那是重写**之前**的状态。
> 2026-09-30 按上游原件实测比对：渲染层重合 **2.3%**，Python 脚本 **27.1%**，重合项是
> `</div>` / `return;` 这类无独创性的通用语法。当时据此删除了那段版权存疑说明。
> 依据见 commit `f7471c9`。