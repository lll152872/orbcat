# orbcat

> 悬浮球形态的私人 agent —— 一个记得住你的 Windows 桌面助手。
>
> **v0.2.0**（2026-09-30）

orbcat 平时是屏幕上的一颗小球，点开是一个能读文件、跑命令、查网页、写日记的
agent。它和通用 AI 助手的区别在于两件事：**它记得你是谁**，以及**它把你的偏好
变成硬约束**，而不是只存在一份没人看的记忆文件里。

---

## 一、它是什么

| 要素 | 说明 |
|---|---|
| **悬浮球** | 无边框、置顶、不占任务栏。可拖动；点一下展开面板，再点收起 |
| **睡眠** | 球**真的从屏幕上消失**（窗口 `hide()`），但**在跑的任务不受影响** |
| **唤醒** | 托盘左键 / 托盘右键「唤醒」/ **再启动一次 exe** |
| **面板** | 聊天、流式时间线、模型切换、Token 用量、记忆与技能、设置 |
| **前台感知** | 记录你当前在看什么窗口，作为上下文喂给模型（睡眠时也不停） |

**核心目标**（源自项目立项时锁定的设计决策）：

- **懂「人」** —— 记得你的说话方式、偏好、正在忙什么
- **懂「路子」** —— 偏好要**驱动行为**，编译进系统提示词，不是摆设
- **懂「活」** —— 知道具体项目的进展与上下文

---

## 二、快速开始

### 首次运行（两段式初始化）

新克隆的仓库里**没有** `agent-data/`，而模型配置又存在 `agent-data/` 里 ——
要 agent 自己建目录会撞上死锁。所以拆成两段：

**第一段（纯 Rust，不需要模型）**

启动 orbcat，点界面上的**「建出目录结构」**按钮。它会建好 8 个目录 +
空的 `models.json` + 默认 `settings.json`。

**第二段（需要模型，由 agent 完成）**

1. 在**设置面板**里加上你的模型（API 地址 / key / 模型名）
2. 开始对话 —— agent 会引导你补齐四个人格文件：
   `RULES.md`（行为红线）、`SOUL.md`（人格语气）、
   `IDENTITY.md`（身份称呼）、`USER.md`（用户画像）

> ⚠️ **添模型只有设置面板一个入口。** 别直接编辑 `agent-data/models.json` ——
> 里面有真实密钥，手改容易写坏。
>
> ⚠️ 这四个人格文件必须**先问后写**，细节见 [`初始化.md`](初始化.md)。

### 从源码构建

```bash
# 前端（10~20s，不重编 Rust）
npm install
npm run build

# 开发运行
npm run dev          # 仅前端
npm run tauri dev    # 完整应用

# 正式发布构建
bash tools/build-release.sh          # fat LTO，最慢最小（约 4~5 分钟）
bash tools/build-release.sh --fast   # 快速档（约 55s），给人真机测试用
```

> ⚠️⚠️ **必须在 Git Bash 里跑，不能在 WSL 里跑。**
> 本机 PATH 上的 `bash` 是 WSL 的 `C:\Windows\system32\bash.exe`，而
> `node_modules` 是 Windows 装的 —— 进 Ubuntu 后 `npm run build` 会报
> `Cannot find module '../rolldown-binding.linux-x64-gnu.node'`，
> 这句错看起来像依赖坏了，**实际是操作系统不对**。
>
> 判断方法：脚本里 `pwd` 输出 `/mnt/d/...` 就是 WSL（错），`/d/...` 才是 Git Bash（对）。
>
> 或者直接用 PowerShell：
> ```
> "C:\Program Files\Git\bin\bash.exe" -lc 'cd "/d/workplace/悬浮小agent/orbcat" && bash tools/build-release.sh'
> ```

---

## 三、分发形态（重要）

**orbcat 的 exe 不自包含前端。** `tauri.conf.json` 的 `frontendDist` 指向
`stub-dist/`（一个内容恒定的占位目录），真实前端在运行时从
**exe 同目录的 `dist/`** 读取（见 `src-tauri/src/disk_assets.rs`）。

这样做的收益：改前端只跑 `npm run build`，**Rust 完全不用重编**
（否则每次改一行 CSS 都要重链一次，实测约 57s）。

代价：**`orbcat.exe` 必须和 `dist/` 同目录分发**，不能只拷一个 exe 走。
`tools/build-release.sh` 已经把拷贝 `dist/` 这一步焊进流程。

```
发布目录/
├── orbcat.exe
└── dist/
    ├── index.html
    ├── models.html
    └── assets/...
```

> 找不到 `dist/` 时会回退到编译期嵌入的 `stub-dist` 占位页，
> 启动日志里会写 `前端资源: 未找到外部 dist/，回退内置占位页`。

---

## 四、安全模型

orbcat 能读写你的文件、跑命令，所以权限是分层的，**默认全部要问你**。

### 4.1 文件权限

| 档位 | 含义 |
|---|---|
| **Once** | 只这一次 |
| **Turn** | 本轮有效 |
| **Task** | 跟随会话，落盘 `sessions/<id>.grants.json` |
| **Always** | 永久 |

默认工作目录是 agent 数据目录；越界访问会被拦下并回灌给模型。

### 4.2 命令执行

`run_command` 走**双闸门**：

- **Gate 1** —— `cwd` 过文件权限网关
- **Gate 2** —— 命令策略三路判定（白名单免问 / 硬阻断 / 弹卡）

命令策略会做静态归一化（拆 `; | && ||`、别名展开 `rm`→`Remove-Item`、
参数缩写 `-r`→`-Recurse`、检测 `iex`/`$()`/反引号等动态解析），
并结构化硬阻断 `Remove-Item -Recurse -Force`、`format`、`shutdown`、
`reg delete`、`diskpart`、`-EncodedCommand` 等。

### 4.3 执行权限档位

输入框左侧的 chip，三档：

| 档位 | 白名单 | 硬阻断 | 其余命令 | MCP 工具 |
|---|---|---|---|---|
| 🛡 `ask`（默认） | 免问 | 拦 | 弹卡 | 弹卡 |
| ⚡ `smart` | 免问 | 拦 | 低/中风险免问，高风险弹卡 | 弹卡 |
| 🔓 `full` | 免问 | **仍拦** | 全部免问 | 全部免问 |

> **硬阻断在任何档位下都拦。** 档位免掉的是"问你"，不是安全底线。
> 免问执行照写审计链（`agent-data/audit-commands.jsonl`，`source = "trust-auto"`）。

### 4.4 命令授权是永久的

命令卡上的「记住这类 / 记住这条」会写进 `agent-data/command_grants.json`，
**跨会话、跨重启存活**，只有设置页「撤销 / 全部撤销」才失效。
「仅这一次」是内存里的一次性令牌，**不落盘**。

---

## 五、已知限制

**这些是实测结论，不是待办事项。**

| 限制 | 说明 |
|---|---|
| **隐形桌面没有合成键鼠** | 「后台模式」能建一张你看不见的桌面并真跑 GUI 程序（实测 WPS 打开 docx 导出 PDF），但**跨桌面注入键鼠被内核拒绝**。因此后台模式的输入通道必须是应用自身的自动化接口（COM / CLI / UIA Pattern / HTTP） |
| **UIA 选区对 Electron 不可靠** | VSCode 需 `--force-renderer-accessibility` 启动且不稳定；Typora 本机为改造版。Win32/WinUI 完全可用 |
| **命令策略编辑器未接 UI** | 后端 `perm_cmd_policy` / `perm_path` / `perm_grants_list` 命令已就绪，设置页界面待接 |
| **deny 时不隐藏工具** | 策略为 deny 的工具仍会出现在工具列表里（尚未做可见性过滤） |
| **仅 Windows** | 大量 Win32 FFI（`capture.rs` / `win32.rs` / `win32desk.rs`），无跨平台计划 |

---

## 六、目录结构

```
orbcat/
├── src/                     前端（TypeScript + 原生 DOM）
│   ├── main.ts              主面板
│   ├── models.ts            模型窗口
│   ├── format.ts / usage.ts
│   └── views/               子视图（如 recovery.ts）
├── src-tauri/
│   ├── src/
│   │   ├── lib.rs           Tauri 命令注册 + 应用装配
│   │   ├── agent.rs         agent loop / 系统提示词
│   │   ├── tools.rs         内置工具
│   │   ├── llm.rs           LLM 客户端（SSE 流式）
│   │   ├── command_policy.rs 命令策略（归一化 / 硬阻断 / 授权表）
│   │   ├── permission.rs    文件权限
│   │   ├── perm_request.rs  权限申请卡
│   │   ├── sessions.rs      会话持久化
│   │   ├── mcp.rs           MCP 网关
│   │   ├── win32.rs / win32desk.rs / capture.rs   原生能力
│   │   ├── recovery.rs      备份 / 回收站还原
│   │   └── modes.rs         模式
│   └── Cargo.toml
├── tests/
│   ├── ui/                  vitest（happy-dom）
│   └── e2e/                 真实链路（mock / live）
├── tools/build-release.sh   发布构建
├── agent-data/              运行时数据（**不进版本库**）
├── 初始化.md                两段式初始化说明（给 agent 读）
└── CHANGELOG.md
```

---

## 七、测试

```bash
npm test              # 前端 + Rust 全跑
npm run test:ui       # vitest（8 文件 / 51 用例）
npm run test:rust     # cargo test --lib（310 用例）
npx tsc --noEmit      # 类型检查
```

E2E：

```bash
npm run test:e2e:mock   # 本地 mock OpenAI 服务
npm run test:e2e:live   # 真实链路
```

---

## 八、技术栈

- **前端**：TypeScript + 原生 DOM（无框架）、Vite、vitest
- **后端**：Rust、Tauri 2、tokio、reqwest（**native-tls / SChannel**）
- **存储**：SQLite（`rusqlite`，仅索引）+ Markdown（长期记忆）

> ⚠️ TLS 必须用 native-tls（Windows SChannel），**不能用 rustls**。
> 2026-09-21 实测：`ark.cn-beijing.volces.com` 在 rustls 下 body ≥16KB 会超时，
> 同请求在 SChannel 下正常。
