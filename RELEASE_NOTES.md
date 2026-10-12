# orbcat v1.0.0

悬浮球 agent —— Windows / macOS，单文件 agent。

## 下载

| 文件 | 平台 |
|---|---|
| `orbcat-windows-x64.zip` | Windows 10/11 x64 |
| `orbcat-windows-arm64.zip` | Windows 11 on ARM |
| `orbcat-macos-arm64.dmg` | macOS 11+ Apple Silicon |

> **未签名未公证**：Windows 会弹 SmartScreen（「更多信息」→「仍要运行」），
> macOS 会被 Gatekeeper 拦（右键 → 打开）。这是预期的，不是安全问题。

## 解压即用

orbcat 的 exe **不自包含前端** —— `orbcat.exe` 必须与同级的 `dist/` 目录
一起用。zip 里已经配好了，**整个文件夹一起拷走**，不要只把 exe 拖出来。

首次启动会读当前工作目录下的 `agent-data/`（自动创建）。
模型 API key 在设置里配，存在本机 `agent-data/models.json`。

---

## 本次发布新增（相对首个 1.0.0 tag，28 个提交）

### 用户可感知的修复

**MCP 工具组不再受 agent 模式限制（用户拍板：统统自己管理）**

原先「模式层 ∩ 用户设置」两道闸门：在 `mcpGroupsPreload: []` 的模式下
（内置「闲聊」就是），去设置 › MCP 勾某组的「给模型」会被后端直接拒掉 ——
「当前 agent 模式不允许使用组」，而且**开关连 `settings.json` 都写不进去**
（错误发生在保存之前）。想用一个工具组，得先去翻 JSON 改白名单。

现在能不能用某个组**只看用户在设置页的组开关**：想用就勾，不想用就别勾。
`mcpGroupsPreload` 仍会被解析（老配置文件照常能打开），但不再影响任何行为。

**点「设置」直接跳到会话**

「备份与回收站」页点顶部的「‹ 设置」，直接跳到了对话页。根因是**同一个
按钮上挂了两个 click handler**：`renderSubBar()` 已绑一个
`switchView("settings")`，`bindRecoveryView()` 又叠了一个。一次点击跑两遍，
第二遍撞上 `switchView` 的 toggle 语义，返回键就变成了「回对话」。

代码库里其余 8 个子页的 bind 函数都写着同一句「这里不能再绑一次」，
这一处是唯一的例外。已删掉冗余绑定，并补上回归测试（含反向验证：
把双绑放回去，测试确实红）。

**安全闸门页的「去命令策略 ›」吃浏览器默认蓝**

`.goto-cmdpolicy` 在 `styles.css` 里**一条规则都没有**，`<a>` 于是吃了
UA 默认值 —— 纯蓝 + 下划线，实测对比度 **1.99:1**。在暖黑 + 薄荷绿的深色
主题里像贴进来的一块异物色。而它是安全闸门页**唯一**通往命令策略的路径，
样式缺失等于入口隐形。现按主题口径补成薄荷绿，13.25:1。

### 对比度（用户报「什么都看不清」）

**去掉面板正文的 `opacity: 0.75`** —— 这是单一最大来源。它来自初始提交，
从没注释过原因、也从没被改过（连改版重挑周围每个文字色都漏了它）。
面板正文里**所有**内容都被按 0.75 与底色合成，于是所有文字的实测对比度
被系统性压低一档：`#9C9CA2` 6.85→4.37、`#8E8E94` 5.74→3.75、
`#7C8479` 4.84→3.26。**两套检测方法都测不到它** —— 静态 grep 只读
`color` 声明；`getComputedStyle().color` 返回的是指定色而非合成后的像素。

**43 处过暗文字色抬到 WCAG AA 以上**（7 个色值）。它们原本挤在 2.61~4.26
之间、彼此只差 0.2~0.5，逐个「刚好抬到 4.5」会全压进同一条窄带、彻底失去
层次，所以改为均匀铺开在现有档位里，色相与先后顺序都保住。

**消息底部五个按钮 1.92:1 → 5.25:1**。`opacity: 0.45` 叠加
`var(--cream-3)` 后只剩 1.92:1 —— 这就是「消息底部那排按钮几乎不存在」的
原因。注意**单调 opacity 不可能达标**（只会让暗底上的文字更暗），
必须同时把 color 抬上去，两个都改了。常显的设计意图保留（不靠 hover
决定可见性），只调了「低对比」的下限。

**补 5 处清单外的低对比度**。关键是要按**各自真实底色**算，不能一律套
面板底 —— `.thumb-bad` / `.md-img-bad` 的真实底是半透明粉罩在面板上
（合成 `#26211D`），实际只有 3.53:1；`.mcp-url-input::placeholder`
更是只有 1.75:1。

### 平台与构建

- 抽出 `platform/` 抽象层，**Mac 版可编译**（此前裸 Win32 FFI 编不过）
- macOS 出 `.app` + `.dmg`；修 `.app` 里 dist 无处安放导致的启动白窗
- 新增 Windows ARM64 CI job，并读 PE 头 machine 字段校验产物真的是 0xAA64
- 修两个只在 CI 上暴露的环境依赖测试，给 23 个平台相关测试加 `#[cfg(windows)]` 门

### 安全

- 拼装项目层 fail-closed / fail-open 分流：schema 不认识 → 整包拒载；
  权限预设不合法 → 整份丢弃；引用失效 → 该项跳过 + 标黄
- 拼装层**永不绕过**既有闸门：预设不得把硬阻断命令加进白名单、
  不得授权盘根 / 主目录级别的路径
- L1 脚本工具走 argv 不经 shell 解释器，加载期即拒绝占位符出现在
  解释器内联串里（`-Command` / `-c` / `/c` / `-e` 之后）

### 已知限制

- **未签名未公证**。Windows 会弹 SmartScreen，macOS 会被 Gatekeeper 拦
  （右键 → 打开）。这是预期的。
- **exe 不自包含前端**，必须与同级的 `dist/` 一起分发 —— zip 里已配好。
- 3 个测试在本机被火绒拦 `pwsh` spawn，已标 `#[ignore]`，CI 上正常。

---

## [1.0.0] — 2026-10-09 · 前端改版 + MCP 命名简化 + 两个实机 bug

面板从 v0.2.0 的样子整体重做了一遍，并把 MCP 的分组/命名层简化成通用协议。

### 变更

**MCP 命名层简化：一 server 一组，不再解析网关前缀**

原先 orbcat 会解析 1MCP 网关的工具名格式 `<子服务>_1mcp_<工具>`，按前缀把一个网关
拆成多个组（`github` / `ssh` / `mysql`…）。这是把**网关内部的命名约定**当成了协议：
换一个网关、或网关自己改了命名规则，客户端就得跟着改。

现在只维护通用 MCP 协议：

- **一个 MCP server = 一个组，组名 = server id**
- **工具名原样透传**，`mcp__<server_id>__<raw_tool_name>`，不改写
- 想给某个子服务单独一个组？在 `mcp.json` 里把它配成独立 server

配套改动：

- 删 `short_tool_name`（原先会剥 `_1mcp_` 前缀展示短名，导致索引显示的名字与可调用的
  全名对不上）、删 `tools.rs::tool_with_server`（反向把可调名还原成 `<server>_1mcp_<tool>`）
- `tool_specs` 改用 `reg.groups()` 遍历取 `server_id`，不再从工具名反解组名
- `execute` 的"是否已加载"判定从 `t.name.ends_with(&tool)` 改为精确相等 `t.name == tool`
  —— `ends_with` 会把 `x_run-command` 和 `run-command` 误判成同一个

**接入 dsh/cordis MCP 桥**

`agent-data/mcp.json` 新增 `cordis-bridge`（stdio），把整个 cordis/dsh 插件生态的
20 个工具（`lsp_*` / `pwc_*` / `bridge_*` / `learn_meme` / `send_meme`）接进 orbcat。
orbcat 侧零依赖、零体积增长 —— 桥跑在外部 Node 进程里。

**MCP 添加表单的输入框被撑成巨型空盒子**

`.mcp-add-form` 想把「添加 HTTP 网关」「添加 stdio 脚本」的输入框横排，但它与
`.set-form` 同为 (0,1,0) 特异性且排在后面，只覆盖了 `display/flex-wrap/gap/align-items`，
**漏了 `flex-direction`** → 沿用 `.set-form` 的 `column`。于是 input 的
`flex: 1 1 120px` 把 **basis 120px 落到了高度上**，再被父容器 `.mm-body`（`flex: 1`）
给的确定高度按 `flex-grow` 平分，每个输入框被撑成一两百像素高。补上
`flex-direction: row` 并给 input 锁死 `height: 30px`。

**设置页减法：一个拼装包都没有时，「项目」入口整行隐藏**

「项目」这一页讲的从头到尾都是**拼装包**（`projects/<名>/project.json`），一行项目
记忆都没有 —— 项目记忆实际在顶栏 📁 下拉和「记忆」页里管。所以 0 个包时它对用户
是纯空壳，只在设置入口页占一行、还让人误以为「项目」= 那 8 个记忆目录。

改为：`projectInfo.bundles.length > 0` 才渲染这一行。副作用（有意接受）：造第一个包
没法从 UI 点，靠手写 `projects/<名>/project.json` —— 这本来就是模块的设计立场
（「那份 JSON 本来就该能用记事本改」），文件一出现这一行自动回来。

**归档一批无代码引用的残留**

`agent-data/` 下有三笔东西功能已删 / 代码零引用，白占磁盘：`dispatch/workbuddy/`
（WorkBuddy 分发功能早随 `dispatch-workbuddy` 技能一起删了，3 文件 300KB）、
`trae-proxy/trae_proxy.py`（代码零引用，13KB）、`tmp/wb-app/`（WorkBuddy 解包的
115 个 JS chunk，28MB）+ `tmp/trae_cfg/` + `tmp/trae_dump/`（trae 逆向分析遗留，342KB）。

全部按 RULES.md 第二节**归档**到 `.trash/`（rename + 补 `trash.jsonl` 来路），
可在「设置 › 备份与回收站」一键还原 —— 不是硬删。

**修 `trash.jsonl` 里 19 条记录全部指向已不存在的路径**

项目从 `float-agent` 改名成 `orbcat` 之后，`trash.jsonl` 里的路径没跟着改：
19 条里 18 条的 `trashed` 路径指着已经不存在的 `float-agent\...`。`list_trash`
按这个路径探测，于是 85MB 归档物在回收站 UI 上**全部显示「已不在原处」**，
一个也还原不了 —— 文件明明好好躺在 `.trash/` 里。修正 36 处路径前缀后 19/19 可定位。

**设置入口页补一句「技能 N 个」—— 之前根本找不到 skills 在哪**

技能清单一直在「记忆」页最底下，但设置入口那行摘要写的是「待审批、记忆文件族与
项目绑定」，**压根没提技能** —— 从入口页完全看不出这页里有技能，等于找不到。
摘要改成显式列出技能数（`技能 6 个 · 记忆文件 · 项目绑定`，有待审批时前置）。

**设置页减法：两个功能整体暂缓，入口标记 `hidden`**

「项目」（拼装包）和「数据源」（`life.json` 生活清单）这两个功能用户决定**暂缓，
不随本次发布**。实现一行没删，只是：

- 设置入口页那两行加了 `hidden` 属性（配 `.set-entry[hidden]{display:none!important}`
  ——`.set-entry` 自己就是 `display:flex`，会盖掉 HTML `hidden` 的 UA 默认值，不加
  这条等于没隐藏）
- 两个摘要函数 `projectSummaryText` / `lifeSummaryText` 改成**幽灵函数**：
  签名保留、调用点保留、返回 `null`（不渲染那行摘要），原实现以注释形式整段
  留在函数体内
- `projectInfo` 加一行 `void` 引用钉住 —— 幽灵函数导致它 TS6133，但
  `renderProjectView` / `project_list` / 整套 vitest 都还活着，不能被当成死代码清掉

恢复功能只需两步：把函数体里的注释实现放回去 + 去掉 `item()` 末尾那个 `true`。
页面、后端命令、11 个数据源测试和 10 个项目测试全程原地保留，不用重建。

**「文件权限」+「命令策略」合并成一行「安全闸门」**

两个页面都还在、互相可达（文件权限页顶部有「去命令策略 ›」），只是设置入口页不再
并排摆两行 —— 它们本来就是同一件事的两个面：agent 能碰什么。

### 修复

**右键球 → 点「记忆」→ 面板缩成一小块，而且回不去**

窗口有胶囊态 / 面板态 / 右键菜单态三套尺寸。点菜单项时 `applyMode("panel")`
要把窗口从 196×214 拉到 460×600，但 `applyMode` 两个分支给 `mode` 赋值的时间点
不一致：

```ts
// orb/menu 分支 —— 先赋值
mode = next;
await invoke("set_window_mode", { mode: next });

// panel 分支 —— 先 await（错）
await invoke("set_window_mode", { mode: next });
mode = next;
```

于是整个 IPC 往返里 `mode` 一直是旧的 `"menu"`。`set_bounds` 改窗口大小会触发
一次激活状态变化 → WebView 抛 `blur` → blur 处理器看到 `mode === "menu"` 就
`applyMode("orb")` 把窗口收回球大小。外层 await 回来后才写 `mode = "panel"` ——
**`mode` 说面板、窗口实际是球**。再点球时 `if (next === mode) return` 直接返回，
从此再也回不去。

两处都修了：

1. panel 分支改成 `mode = next` 在 await **之前**（与另一分支一致），并给
   `invoke` 加 try/catch —— 万一窗口没改成，`mode` 立刻写回旧值，
   不让它停在「和实际尺寸不一致」的状态
2. 新增 `modeSwitching` 标志：切换期间 blur 与 mousedown 一律放行，
   自己 resize 引来的事件不再被当成「用户点到别处去了」

**MCP「添加」按钮整个看不见（低对比度）**

`.set-btn.add` 的 `background` 与 `color` **是同一个色值 `#a8e6cf`** —— 文字直接
糊在底色上。改版时底色从深色调成薄荷绿，字色忘了跟着换。改成深字 `#10261E`。
`.init-go`（初始化引导按钮）同款（近白字压薄荷绿底）一并修。

`#btn-send` 与 `.msg.user .msg-body` 也是同款写法，只是恰好被文件更靠后的规则
改回深色才没出事 —— 已加注释标注这两处必须成对改。

**模型窗口里改不了 Base URL（保存后静默丢弃）**

模型编辑表单有 Base URL 输入框，但改完保存**什么都不会发生、也不报错**。

根因是接口设计缺陷而非漏传参数：`config::update_model` 的第 3 个参数是「**新**
Base URL」，本来支持换组；但 `models_edit` 把**同一个** `url` 参数同时当作
「定位用的旧 URL」和「新 URL」传下去，而前端只发了旧值（`url: f.editUrl`），
表单里真正填的新值压根没上线。

拆成两个参数：`url`（定位）+ `new_url`（新值）。前端改发 `newUrl`，后端用
`url_key` 归一后判断是否真的变了，变了才落盘；同时当前选中项的
`selectedModelUrl` 跟着更新，否则「当前模型」标记会掉没。

**模型窗口几乎没法缩放（只剩一条抓不住的细缝）**

后端建这个窗口时是 `resizable(true)` + `min_inner_size(480,360)`，尺寸还写进
`settings.modelsWindow` —— 缩放是设计内功能。但代码里**从来没调用过
`startResizeDragging`**：Windows 的边缘 resize 依赖 mousedown 落到窗口边框，
而整窗拖动对几乎整个客户区都做了 `preventDefault()` + `startDragging()`，
客户区里一点 resize 的余地都不剩。

显式画 8 条缩放手柄（四边 + 四角，命中区 7px / 角 12px），mousedown 调
`startResizeDragging`，并 `stopPropagation` 防止一次操作同时移动+缩放。
手柄是 `.mw-window` 的**兄弟**节点 —— 它有 `overflow:hidden` + 圆角，
放里面会被裁掉。

**卡死：思考块越长越卡（最严重）**

`patchTimeline` 每一帧都读 `pre.scrollHeight` 判断"是否贴底"，而**读布局属性会强制排版**——
哪怕那个思考块正折叠着、内容根本不在视口里。同时 `.run-reason pre` 的限高又在 2026-09-26
被去掉过（因为"展开内容比标注字数少"），于是内容越长、每帧越贵 → 平方级增长。
复现方式：让模型在思考里输出 100 个 100。

- 折叠时改用 `details.open`（纯属性查询，不触发布局）短路掉这两次读取
- `.msg.running .run-reason pre { max-height: 360px }` —— **只在流式期间限高**，
  跑完 `.msg.running` 消失，限高自动解除，正文完整展开。
  两个对立需求（要看到全部 / 别卡死）用同一个属性同时满足
- 新增回归闸门：spy `Element.prototype.scrollHeight`，断言折叠时读取次数为 0

**MCP 添加表单的输入框被撑成巨型空盒子**

`.mcp-add-form` 想把「添加 HTTP 网关」「添加 stdio 脚本」的输入框横排，但它与
`.set-form` 同为 (0,1,0) 特异性且排在后面，只覆盖了 `display/flex-wrap/gap/align-items`，
**漏了 `flex-direction`** → 沿用 `.set-form` 的 `column`。于是 input 的
`flex: 1 1 120px` 把 **basis 120px 落到了高度上**，再被父容器 `.mm-body`（`flex: 1`）
给的确定高度按 `flex-grow` 平分，每个输入框被撑成一两百像素高。补上
`flex-direction: row` 并给 input 锁死 `height: 30px`。

**列得出、调不通**

非网关 server 的工具没有 `_1mcp_` 前缀可剥，`mcp_tool_callable_name` 走兜底给出
`mcp__standalone__lsp_diagnostics`，但 `tools.rs::tool_with_server` 又把它还原成
`standalone_1mcp_lsp_diagnostics` —— 与 server 上的真实名匹配不上，只能落到任意一个
client 上，必然报 unknown tool。这正是 `mcp.rs` 里那句「两个方向各有一套猜测规则，
迟早对不上」注释警告的分叉点，网关场景不触发、非网关场景必触发。

新方案下 `parse_mcp_tool_name` 取第一段为 server、其余即原名，
`tool_with_server` 整个函数不再需要，分叉点从根上消失。

**原生 `<select>` 下拉项看不见**

浮层底色走系统浅色，而文字色继承了近白色 → 浅底白字。给 `option` 显式指定深底浅字，
并强制覆盖 `:checked` / `:hover`。

**模型窗口那块白浮层**

「保存的信息」**全仓库搜不到** —— 是 WebView2 自己的浏览器自动填充下拉，属浏览器 UI，
CSS 管不了。只能给输入框加 `autocomplete="off" autocorrect="off" spellcheck="false"`。

### 变更

**设计令牌层**

`styles.css` 头部新增 `:root`，约 40 个变量。改之前 108 个 hex + 331 个 rgba 全是硬编码，
`var()` 出现 **0 次**。

**配色**

蓝紫 `#5b6ef5` → 琥珀 → 暖沙 → 中性 → **马卡龙薄荷绿 `#a8e6cf`**（接悬浮球的绿玻璃与奶油猫）。
语义色同步马卡龙：薄荷=成功 / 柠檬=警告 / 蜜桃=危险 / 薰衣草=信息。

**强调色只放小面积 —— 但小面积 ≠ 零面积**

这条是七轮试错里最贵的一条教训。收得太狠时，整块 460×600 面板是纯黑的，
唯一的薄荷绿是右下角 28×28 的发送键（约 0.3% 面积），肉眼看过去等于没上色。
最终形态是：色相不动，把薄荷铺到**四处低透明度染色**（≤0.17，不是色块 —— 被否掉两轮的
是实心琥珀块，染色底下文字对比度不变，读作"空气里有绿"）：用户气泡 / 标题条渐层 /
助手左细线 / 时间线外壳。实心色块（发送键、权限卡顶边、焦点环）保持不变 —— 到处都是绿的，
等于没有一处是绿的。

**chrome 五带合并**

header / toolbar / fg-ctx / sub-bar / bg-run → **单行标题条 + 可塌陷第二行**。
后三者本来就各自 `getElementById` 单独切 `display`，挪进容器**零 JS 改动**。

**消息操作键悬停显形**

按钮全部留在 DOM、class 与 `data-*` 未变（委托处理器零改动），纯 CSS 控制显隐；
`hover: none` 设备常驻。不加假「⋯」按钮 —— 这个项目有过"只有 markup 没有 listener"的前科。

**设置页 13 个 emoji → 手绘 24×24 线性 SVG**

**时间线阈值窗口化**

>40 步只渲染尾部 40 条 + 「展开更早的 N 步」。取回时用 **prepend** 而非整段重画 ——
保住 `<details>` 的开合状态与思考块的懒渲染状态。没做虚拟滚动：虚拟滚动要接管滚动容器高度，
会和 `patchTimeline` 的增量追加打架。

**补齐原来全为 0 的东西**

`:focus-visible`（焦点环以前全靠浏览器默认，深底上几乎看不见）/ `.panel-sub:empty` /
`prefers-reduced-motion` / 折叠块 chevron（删掉默认三角后必须补）。

### 未做

| 项 | 原因 |
|---|---|
| 面板加宽到 480×640 | 要改 `lib.rs:221` 的 `PANEL_W/H_LOGICAL` = 后端 |
| 权限卡吸底 | 要改对话流与输入区的 flex 归属，收益小于风险 |
| 混搭配色（薄荷底 + 重蓝强调） | 方案已存 `docs/preview/mix-mintbase-blueaccent.html`，待做 |

---
