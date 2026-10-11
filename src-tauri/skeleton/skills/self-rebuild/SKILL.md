---
name: self-rebuild
description: 重新编译/更新 orbcat 自己时使用（改代码后、用户说「重新构建」「更新一下」「自更新」）。**前提：持有 orbcat 源码**（有 `src-tauri/` 目录）；没有源码的用户不要用本技能。核心：不杀宿主进程，用「exe 改名腾锁 + detached 重启脚本」完成自举，避免自杀导致 build 中断。
---

# orbcat 自举重建（不自杀版）

项目根：**先探测，不要写死**。按顺序试，任一成功即可：

```powershell
# 1) 仓库根（最可靠 —— 项目是 git 仓库）
$root = git -C . rev-parse --show-toplevel
# 2) 退化：当前工作目录向上找含 src-tauri\ 的那一层
if (-not $root) { $root = (Get-Location).Path }
```

Rust 后端在 `$root\src-tauri\`，前端在 `$root\src\`。
探测不到就**问用户项目根在哪**，不要瞎猜一个路径硬编。

**核心原则：orbcat 是 agent 的宿主进程 —— 杀它 = 杀自己 = build 永远完不成。全程不杀进程。**

## 步骤

### 1. 腾锁（不杀进程！）
Windows 运行中的 exe **可以改名、不能覆盖**。如果运行实例就在 `orbcat.exe` 原路径上：
```powershell
# 查谁在跑（只读，直接放行）
$rel = "$root\src-tauri\target\release\*.exe"
Get-Process | Where-Object { $_.Path -like $rel } | Select-Object Id,ProcessName,Path
# 把运行中的 exe 改名腾出路径（Move-Item，等价 Rename）
```
用 `move_file` 把 `orbcat.exe` → `target\release\_old-orbcat.exe`（`.pdb` 同理）。进程照跑，锁腾掉。
如果跑的本来就是 `_old-*.exe`（上次改过名），`orbcat.exe` 路径已空闲，**这步跳过**。

### 2. 编译（宿主活着，结果才回得来）
```powershell
cd $root\src-tauri
cargo build --release 2>&1 | Select-Object -Last 15
```
验证 `orbcat.exe` 的 mtime 是刚刚（`file_info`）。
前端改过才需要先 `npm run build`；Rust 验证 `cargo test --lib`。

### 3. 换血重启脚本（写到 agent-data/tmp/）
⚠️ **若你的权限策略禁止写程序目录**（把「agent 程序目录」设成只读的权限规则很常见），
则脚本一律写数据目录，不要写 `target\release\` —— 写那里会被权限网关拒：
`{{DATA_DIR}}\tmp\restart.ps1`
脚本逻辑：sleep 30（给宿主说完最后一句话）→ 按路径杀 `target\release\*.exe` 里非 orbcat 的旧实例 → `Start-Process orbcat.exe`。日志落 `{{DATA_DIR}}\tmp\restart.log`。

### 4. 放手（本轮最后一步）
```powershell
Start-Process powershell -WindowStyle Hidden -File "{{DATA_DIR}}\tmp\restart.ps1"
```
`Start-Process` 的独立进程不随宿主死亡（已验证：同法跨轮存活）。**这条必须是最后一个工具调用**，之后立刻收尾说话。30 秒后旧实例被杀、新 exe 起来。

## 收尾验证（下轮）
- `{{DATA_DIR}}\tmp\restart.log` 有 "launched" 行
- 前台进程名变回 `orbcat`
- 旧 `_old-*.exe` 还在盘上 —— 删除走归档（`delete_file`），或留给用户自行删

## 踩坑记录
- **自杀式流程**（先杀实例再编译）：杀完 `run_command` 结果永远回不来，build 死在半路。已废弃。
- **腾锁要 exe 与 pdb 一起改**：只改 exe 时 pdb 仍可能被占用导致链接失败。
- **写程序目录可能被权限规则拒绝**：那就把脚本与日志改写到数据目录的 `tmp/` 下。
- 脚本路径里**不要写死绝对路径**：项目会挪位置，写死就废。