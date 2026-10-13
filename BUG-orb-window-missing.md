# BUG：orbcat 启动后没有任何界面（球和面板都不出现）

状态：**已修复**（运行态挪到 `D:\orbcat\`，实测球与面板都正常）。
**根因已定位到具体错误码**，只剩最后一步「哪个组件在拦」需要管理员权限的工具。
记录时间：2026-10-12 22:50
分支：`feat/platform-layer-macos`，HEAD `05ac346` + 本次未提交改动

> ⚠️ **21:50 那版文档的大部分结论已被证伪**，别再照着旧 §3 的排除表往下查。

---

## 0. 结论

失败发生在**创建 orb 窗口时**，不在窗口形态计算里：

```
tao 的 Win32 窗口建出来（HWND 存在，约 4ms）
  → WebView2 controller 创建失败
      HRESULT 0x800700AA = ERROR_BUSY「指定的资源正在使用中」
  → wry 销毁窗口
  → 事件循环里已经没有这个窗口
  → apply_mode 的任何 getter 都收不到回复 → failed to receive message from webview
  → 屏幕上什么都没有
```

**触发条件是「路径 × 权限」的组合，不是单一变量：**

| | **非提权**（桌面快捷方式 / 日常双击） | **提权** |
|---|---|---|
| `D:\workplace\悬浮小agent\` 下（`orbcat\` 之外） | **必挂** | 正常 |
| `orbcat\` 内 / 其他任何位置 | 正常 | 正常 |

⚠️ 「提权就能绕过」这一条**推翻了本文早期版本「唯一变量是 exe 所在目录」的表述**，
也让 Process Monitor 这条排查路线作废（它必须提权 → 提权就看不到 bug）。

**这是本机环境问题，不是 orbcat 的代码缺陷** —— 换个目录（`D:\orbcat\`）即正常，
发布 zip 给别人解压到任意目录都正常。

---

## 1. 失败时的完整时序（高频轮询实测）

以 25ms 间隔从进程外 `EnumWindows` 采样 class=`Tauri Window`：

```
采样 418 次 / 14 秒
曾出现 Tauri 窗口: True
首次出现: 54ms
最后出现: 58ms
出现过的不同形态: 1
   * 6688056 vis=True 135x76 at(815,418) title='orbcat'
```

**窗口被创建了，只活了约 4 毫秒就被销毁。** 这一条把"WebView2 从来没起来"
的旧判断纠正过来了：WebView2 是**被调用了但失败了**，不是压根没调用。

失败时 WebView2 的其它佐证：
- `msedgewebview2` 子进程数 = **0**
- WebView2 自己的日志（`--enable-logging` → `chrome_debug.log`）**一个字节都没有**
  （正常那次有 954 字节）
- `Env:WEBVIEW2_USER_DATA_FOLDER` / `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`
  覆盖后**仍然失败** —— wry 显式指定了 UDF，环境变量被忽略

---

## 2. 错误码（加诊断后拿到）

在 `run()` 开头装一个把 `log` crate 接到 stderr 的最小 logger
（只认 `ORBCAT_DIAG_WINDOW` 环境变量，正常启动零开销），
tauri/wry 那条一直被丢弃的内部日志就出来了：

```
[log:ERROR] failed to create webview: WebView2 error: WindowsError(Error {
    code: HRESULT(0x800700AA), message: "指定的资源正在使用中" })
```

窗口探针对照（同一份二进制、同一份 dist、同一数据目录）：

| 探针 | 中毒目录 | 正常目录 |
|---|---|---|
| `get_webview_window("orb")` | Some("orb") | Some("orb") |
| **`hwnd()`** | **Err("the underlying handle is not available")** | **Ok(HWND(0x371008))** |
| `is_visible()` | Err | Ok(true) |
| `title()` | Err | Ok("orbcat") |
| `scale_factor()` | Err | Ok(1.5) |

即：管理器里**有**这个窗口，运行时里**没有**。

### 另一个错误：UDF 为空时直接 panic

把 `%LOCALAPPDATA%\com.orbcat.desktop` 挪走让它重建后，失败得更早：

```
thread 'main' panicked at tauri-2.11.5/src/app.rs:1425:
Failed to setup app: 拒绝访问。 (os error 5)
```

`app.rs:1425` = `panic!("Failed to setup app: {e}")`，来自 `app.rs:2525`
的 `WebviewWindowBuilder::from_config(app.handle(), window_config)?.build()?`
—— **建 orb 窗口本身就失败了**，还没进我们自己的 setup 钩子
（`[orbcat] agent-data 目录:` 那行没打印就是证据）。

所以 `os error 5`（ACCESS_DENIED）和 `0x800700AA`（ERROR_BUSY）是**同一个病的两副面孔**：
WebView2 的 profile 建立不起来。UDF 已存在时报"占用"，不存在时报"拒绝访问"。

---

## 3. 触发条件矩阵（实测）

同一份二进制（SHA `71CFCD9D9795`）+ 同一份 dist（SHA `12EBBA63A6B4`）+ 同一个空数据目录，
用同一套探针（`%TEMP%\opencode\orbprobe.ps1`）交替跑：

### 非提权（真实使用场景）

| 目录 | 结果 |
|---|---|
| `D:\workplace\悬浮小agent\diag-FAIL\` | **FAIL**（多轮稳定） |
| `D:\workplace\悬浮小agent\zzT3\sub1\sub2\` | FAIL（深度无关） |
| `D:\workplace\悬浮小agent\orbcat-portable\` | **FAIL** ← 用户双击的就是这个 |
| `D:\workplace\悬浮小agent\zzT11-fromPASS\`（PASS 内容搬出来） | **FAIL** |
| **`D:\workplace\悬浮小agent\orbcat\` 内部任意深度** | **PASS** |
| `D:\workplace\悬浮小agent\orbcat\zzT12-fromFAIL\`（FAIL 内容搬进去） | **PASS** |
| `D:\workplace\`（无中文） | PASS |
| `D:\workplace\中文测试\` / `D:\edge\中文测试\` | PASS（中文路径无关） |
| `D:\悬浮小agent\`（同名、不同父目录） | PASS（**目录名无关**） |
| `D:\orbcat\` | **PASS** ← 现在用它 |
| `D:\edge\orbcat-windows-x64-debug (1)\` | PASS |

**内容互换那两行是决定性的**：判定跟着位置走，内容无关。

### 提权（同一套探针，只换权限）

| 目录 | 结果 |
|---|---|
| `D:\workplace\悬浮小agent\diag-FAIL\` | **PASS**（2/2） |
| `D:\workplace\悬浮小agent\orbcat\diag-PASS\` | PASS（2/2） |

**提权之后两个目录都正常。** 这条同时说明：任何需要管理员权限的排查工具
（Process Monitor、`fltmc filters`）都**无法复现**这个 bug。

### 已逐条排除

| 假设 | 排除方式 | 结论 |
|---|---|---|
| agent-data 内容 | `ORBCAT_DATA_DIR` 指向空目录仍 FAIL | ❌ |
| 二进制 / dist 不同 | SHA 全部相同 | ❌ |
| release vs debug | 同一份 release exe 在 `D:\edge\` PASS、在 `悬浮小agent\` FAIL | ❌ |
| exe 文件名 / 目录名 | `orbcat2\`、`orbcat-x\`、`zzz-unique-99.exe` 全 FAIL | ❌ |
| 当前工作目录 | FAIL exe + PASS 目录做 CWD 仍 FAIL；反向亦然 | ❌ |
| 嵌套深度 | 第三层 `zzT3\sub1\sub2` 仍 FAIL | ❌ |
| git 仓库 | `git init` 后仍 FAIL | ❌ |
| **目录 ACL** | FAIL 的 `zzT3`/`zzT4` 与 PASS 的 `zzT5`/`diag-PASS` **10 条 ACE 逐条相同**，两边都带那条 `Deny Everyone DeleteSubdirectoriesAndFiles` | ❌ |
| 文件系统 | 单分区 GPT、无 reparse point、无 EFS/压缩 | ❌ |
| WebView2 UDF 位置 | PASS 时实测 `%LOCALAPPDATA%\com.orbcat.desktop\EBWebView`，全局、与 exe 路径无关；换 `WEBVIEW2_USER_DATA_FOLDER` 无效（wry 显式指定） | ❌ |
| 火绒拦截 | `applog.db` 的 `AppRunInfoList_60` 里有每次运行记录、**零拦截**；`FileInfo_60` 总行数=1 | ❌ |
| Defender | `AMRunningMode = Not running`（已被火绒接管） | ❌ |

---

## 4. ⚠️ 上一版文档被污染的原因

21:45:44 有个残留进程 `zzz-unique-99.exe`
（`D:\workplace\悬浮小agent\orbcat-probe-zh\zzz-unique-99.exe`，§3 改名测试留下的）
一直活着，占着命名互斥体 `Local\orbcat-single-instance`。

旧版 §5 的清场命令是 `Get-Process -Name orbcat | Stop-Process` ——
**按进程名杀，杀不到 `zzz-unique-99.exe`**。于是它占着锁不放，
之后每次启动都被单实例机制踢掉（stderr 只有 43 字节的
`已有实例在运行 → 已发唤醒信号`），**根本没真跑**。

所以 **21:45 之后的一切观测全部无效**，包括旧 §4 那张矩阵、
旧 §5 那轮 A/B，以及"挪进仓库内依然失败"这条结论。

**正确的前置清理**（按路径扫，不能只按进程名）：

```powershell
Get-CimInstance Win32_Process |
  Where-Object { $_.ExecutablePath -and $_.ExecutablePath -match 'orbcat|zzz-unique|orb-alt|probe' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
Start-Sleep -Seconds 2
```

**校验锁真的空了**（比杀进程可靠）：

```powershell
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class P {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  static extern IntPtr CreateMutex(IntPtr a, int o, string n);
  public static bool Busy(string name) {
    IntPtr h = CreateMutex(IntPtr.Zero, 0, name);
    return Marshal.GetLastWin32Error() == 183;   // True = 还有实例占着
  }
}
'@
[P]::Busy('Local\orbcat-single-instance')
```

**判定一次运行是否有效**，必须同时断言 stderr 里有 `托盘图标已注册`
（被单实例踢掉的进程没有这行，只有 43 字节，很容易被误判成 PASS）：

```powershell
if ($log -match '初始化窗口形态失败') { 'FAIL' }
elseif ($log -match '托盘图标已注册') { 'PASS' }
else { 'INVALID(未真正启动)' }
```

---

## 5. 已生效的修复

**运行态 = `D:\orbcat\{orbcat.exe, dist\, agent-data\}`**，三个约束同时满足：

1. **在 git 仓库外面** —— 用户的 agent-data（API key / 会话 / 记忆 / 截图）
   结构上不可能被提交，不依赖 `.gitignore` 兜底
2. **在 `D:\` 根部** —— 既在仓库外，又不在 `悬浮小agent\` 底下
3. **实测能起来**

改动：

1. **`tools/build-release.sh` 新增部署步骤**
   构建产物先落在 `src-tauri\target\release\`（原本就是这样），
   然后**拷贝 exe + dist 到 `$RUNTIME`（默认 `D:/orbcat`，可用 `ORBCAT_RUNTIME_DIR` 覆盖）**。

   ⚠️ 这一步不是可有可无的：启动器按「新鲜度 = min(exe 时间, dist\index.html 时间)」
   排序。不部署的话，构建完 `target\release\` 是新的、`D:\orbcat\` 是旧的，
   双击图标启动的一直是旧的 —— 正是脚本注释里警告的「新内核 + 旧界面」。

   部署**只碰 exe 与 dist**，`agent-data\` 仅在不存在时创建、**绝不覆盖用户数据**。

2. **`tools/orbcat-launch.ps1`**
   - 新增 `$RuntimeDir = 'D:\orbcat'` 作为第一候选；
     `$patterns` 里的每项现在支持绝对路径
   - 同新鲜度排序的判据从 `$d -like '*orbcat-portable'` 收紧成 `$d -eq $RuntimeDir`
   - 删掉旧的 `..\orbcat-portable\orbcat.exe`（那条路径已废弃且起不来）
   - 顶部注释写清了三个约束和为什么不能放回 `悬浮小agent\`

3. **`src-tauri/src/lib.rs` 新增诊断开关 `ORBCAT_DIAG_WINDOW`**
   - `install_diag_logger()`：把 `log` crate 接到 stderr（Debug 级）
     ——**这次能拿到 HRESULT 全靠它**，之前那条日志被默认丢弃了
   - `diag_window_state()`：打印 `hwnd()` / `is_visible()` / `title()` /
     `scale_factor()` / `is_maximized()`，并现造一个 webview 探针看能不能成
   - 只在设了该环境变量时启用，正常启动零开销、零行为变化
4. **`Cargo.toml`** 加 `log = "0.4"`（只服务于上面那个诊断开关）

**验证**：桌面快捷方式 → `wscript.exe` → `orbcat-launch.vbs` → `orbcat-launch.ps1`
→ 进程从 `D:\orbcat\orbcat.exe` 起来，EnumWindows 枚举到
`Tauri Window vis=True 76x76 at(1617,374) title='orbcat'`，
MCP 147 个工具正常加载，前端 JS 正常执行。

---

## 6. 还差最后一步：谁在拦

已知：WebView2 的 profile 建不起来（`ERROR_BUSY` / UDF 为空时 `ACCESS_DENIED`），
且只在「**非提权 + `D:\workplace\悬浮小agent\`（`orbcat\` 外）**」这个组合下发生。

### 已排除的「拦截者」

| 候选 | 检查方式 | 结果 |
|---|---|---|
| 软件限制策略 SRP | 读 `HKLM\...\Safer\CodeIdentifiers` | 只有 `authenticodeenabled=0`，**无任何路径/哈希规则** |
| AppLocker | `C:\Windows\*\appxpl\*.xml` | 无策略文件 |
| WDAC / Defender 策略 | `HKLM\SOFTWARE\Policies\Microsoft\Defender` | 无策略键；且 `AMRunningMode=Not running` |
| 火绒拦截 | 解 `applog.db`：`AppRunInfoList_60` 有每次运行记录、**零拦截**；`FileInfo_60` 总行数=1 | 无拦截记录 |
| 目录 ACL | FAIL 与 PASS 目录 **10 条 ACE 逐条相同**，两边都带 `Deny Everyone DeleteSubdirectoriesAndFiles` | ❌ |

### 已加载的文件系统过滤驱动（按 altitude）

```
 40500  FileInfo            ← 极低，所有 IO 都过它
 40700  Wof                 （Overlay FS）
 41007  RsFx0700            ← Intel RST/VMD 的 Rapid Storage Filter
130850  UnionFS
141100  FileCrypt
180451  CldFlt
189850  gameflt
244000  storqosflt
265000  applockerfltr
324600  sysdiag             ← 火绒内核
328010  WdFilter            ← Defender（AM 未运行）
385000  Filetrace
385600  MsSecFlt
407000  FsDepends
```

### 为什么查不下去了

剩下的唯一可能是某个**用户态安全产品的钩子**（火绒 HIPS 最可疑）对
「**中完整性级别的进程 + 从 `悬浮小agent\` 路径启动**」做了特殊处理，
而提权后该钩子的判定路径变了。

但要抓它需要**非提权**的 API 追踪。已试过并放弃的方案：

- **Process Monitor** —— 必须提权 → 提权就看不到 bug（本文件 §3）
- `fltmc filters` —— 同样要提权，且只能看到"有过滤器"，看不到"谁返回了错误"
- WebView2 `--enable-logging` —— 被 wry 的 `additional_browser_args` 盖掉
- `RUST_BACKTRACE` —— release profile 下符号被 strip，栈全是 `<unknown>`

可行的下一步（需要额外装工具）：
**API Monitor**（komarev/erik）——可以**不提权**追踪一个进程的全部 API 调用，
正好对上这个场景。或用 xperf/ETW 抓 `Microsoft-Windows-Kernel-File`，但那个也需要管理员。

> 排查用的 `diag-FAIL\` / `diag-PASS\` 目录在收尾时已删除；
> 要复现就按 §3 的表格重建一对，且**必须非提权**运行。

**目前的判断**：这条线索对本项目的价值有限 —— orbcat 的代码、构建、发布链路都是对的，
问题只在这台机器的某个目录上，而运行态已经挪到 `D:\orbcat\` 绕开了。
除非要在同一台机器上把 orbcat 放回 `悬浮小agent\` 目录，否则可以就此收手。

---

## 7. 开机自启指向了没有数据的那份（顺带发现并已修）

用户开机后看到的球是**「尚未初始化 / 数据目录 agent-data/ 还不存在 / 没有人格文件、
行为红线、用户画像、长期记忆」**的空白状态，但真实数据明明在 `D:\orbcat\agent-data\`。

### 根因

```
HKCU\Software\Microsoft\Windows\CurrentVersion\Run
    orbcat = "D:\workplace\悬浮小agent\orbcat\src-tauri\target\release\orbcat.exe"
```

自启指向的是 **构建目录里的 exe**，而 exe 在**同级**找 `agent-data\`
（见 `resolve_data_dir`）。`target\release\` 旁边没有 agent-data，
于是开机启动的是一份**数据目录为空的白板**，真实数据压根没被读到。

### 为什么老代码不会自己修回来

`autostart::sync_to_release()` 原来只处理一种情况：**注册表值里含 "debug" 才改写**。
`target\release\...` 里没有 "debug" 字样 → 直接 return → 永远不会被纠正。
而当年它确实被写成 release，是因为**在设置里点"开启自启"时人正跑着 debug 构建**。

### 修法

`sync_to_release()` 改成「**自启项必须指向真正有数据的那份 exe**」，两条都满足才改写：

1. 注册表里现有的 exe 同级**没有真实数据**（它一定是错的）；
2. 当前 exe 同级**确实有真实数据**（我们自己才是对的那个）。

「真实数据」的判据不能只判 `is_dir()` —— 坏自启已经造出一个 5 文件的空壳
`target\release\agent-data\`（`permissions.json` + 空 `pmem.sqlite` + `diag.log`
+ `sessions\current.txt` + 一个 **143 字节的空会话**），`is_dir()` 会把它误判成有数据。
所以改成：**除每次启动都会自动生成的那几个文件外，是否还有超过 1KB 的实质内容**。
真实会话是 21KB~775KB，空壳里那些是 0~24KB 的生成物，两条都判得开。

顺手删掉了那个空壳 `target\release\agent-data\`。

### 验证

```
启动前: "D:\workplace\悬浮小agent\orbcat\src-tauri\target\release\orbcat.exe"
启动后: "D:\orbcat\orbcat.exe"     ← 启动器跑一次即自动纠正
```

且**不误伤**：自启项已经指向对的那份时，日志里不会出现纠正行（实测 `(无纠正日志)`）。

---

## 8. 顺带发现（与本 bug 无关，另开）

启动日志里每次刷两条：

```
[sessions] ⚠️ ...\agent-data\sessions\s1791436272501-0.json 解析失败，本次跳过: expected value at line 1 column 1
```

某个会话文件不是合法 JSON。需单独修。

---

## 9. 用户数据安全

- 真实数据现在只在 `D:\orbcat\agent-data\`（215.5MB / 1464 文件），全程未动过。
- 排查期间产生的所有数据副本（`orbcat-portable - 副本`、`zz-t-data`、
  `zz-t-data2`、`orbcat\orbcat\`）**都先比对过 manifest 再删**：
  确认「只在副本里、真实数据没有的文件」为 **0** 才动手。
- 探测目录（`zzT*`、`diag-*`、`orbcat-probe-zh`、`orbcat-zh-exe-only`）已全部清理。

---

## 10. 当前 todo 状态

1-5. ✅ 已完成（上一版）
6. ✅ 定位根因 —— **WebView2 profile 建立失败**（`0x800700AA` / `os error 5`），
   触发条件是「非提权 + `悬浮小agent\` 路径（`orbcat\` 外）」
7. ✅ 修好「双击打不开」—— 运行态挪到 `D:\orbcat\`，构建脚本改为部署而非就地构建
8. ✅ 开机自启指向有数据的那份（§7）—— 原来指向 `target\release\orbcat.exe`，开机开出空白板
9. ⬜ 指名拦截者（需 API Monitor 这类**非提权**追踪工具；见 §6）
10. ⬜ 修 `sessions\s1791436272501-0.json` 损坏（§8）
11. ⬜ 全量测试 + 提交推送

**v1.0.0 可以发了** —— 代码、构建、部署链路都已验证正确，
问题只在本机某个目录下，而运行态已经挪走绕开了。
（§8 的机制没查清不影响发布，但建议在这台机器上不要把 orbcat 放回 `悬浮小agent\` 目录。）