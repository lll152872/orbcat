<#
  install-shortcut.ps1 —— 装一个「常驻」桌面快捷方式：**永不指向具体 exe**

  ## 它解决什么问题
  桌面 `.lnk` 里写死的是**一个具体文件**。orbcat 是源码构建产物，文件名/目录会随
  构建档位与归档策略变化（`orbcat.exe` / `release-fast` / `_old_builds\...`），
  于是「快捷方式指向的那份」很容易变成旧的 —— 用户双击后看到的是老界面，
  新功能全部"消失"，还会被误判成 UI bug（2026-09-26 真实事故）。

  WorkBuddy 之类的商业软件不会踩这个坑：它是**安装到固定路径**的
  （`...\WorkBuddyAI\WorkBuddyAI.exe`），升级时**原地替换同名文件**，
  快捷方式的目标路径自始至终没变。orbcat 没有安装器，所以要在启动链上
  补出同样的性质 —— 让快捷方式指向**永不改变的那一环**。

  ## 装出来的调用链（每一环路径都固定）
      桌面 orbcat.lnk
        → wscript.exe                    （系统目录，恒定；GUI 子系统，无控制台）
        → tools\orbcat-launch.vbs        （只在仓库挪位置时才需要重装）
        → pwsh 隐藏窗口
        → tools\orbcat-launch.ps1        ← **每次双击时才决定"当前能用的那个" exe**
        → 选中的 orbcat.exe

  「能用」的判定在 `orbcat-launch.ps1` 里（PE 有效 + >1MB + 同目录有 dist\index.html），
  候选按 `min(exe, dist)` 新鲜度排序。所以：构建改名、换档位、构建失败留残骸，
  桌面图标都不会被带偏。

  ## 为什么中间要夹一个 .vbs
  1. `.lnk` **不能直接指向 .ps1** —— Windows 没有 .ps1 的 ShellExecute 关联，
     双击会弹「你要如何打开这个文件？」。
  2. `.lnk` 指向 `pwsh.exe` 会**闪一下黑框** —— 即使加了 `-WindowStyle Hidden`，
     控制台窗口也是先创建、后隐藏。
     `wscript.exe` 本身没有控制台，它用 `Run(cmd, 0, False)` 传 `SW_HIDE`，
     控制台在创建那一刻就是隐藏的，屏幕上从头到尾不会出现。

  ## 用法
    pwsh -File tools\install-shortcut.ps1              # 装/修桌面快捷方式
    pwsh -File tools\install-shortcut.ps1 -StartMenu   # 顺带装开始菜单
    pwsh -File tools\install-shortcut.ps1 -Verify      # 只体检，不改动
    pwsh -File tools\install-shortcut.ps1 -Uninstall   # 卸载快捷方式

  ⚠️ 需要**非受限**的 shell（要写桌面）。沙箱里的 workspace-write 档写不了桌面。
#>
[CmdletBinding()]
param(
    # 只体检：把整条链走一遍并打印结论，不做任何改动
    [switch]$Verify,
    # 卸载：删掉桌面 / 开始菜单的快捷方式
    [switch]$Uninstall,
    # 顺带在开始菜单也放一份
    [switch]$StartMenu
)

$ErrorActionPreference = 'Stop'

$scriptPath = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$repoRoot   = Split-Path -Parent $scriptPath

$vbsPath      = Join-Path $repoRoot 'tools\orbcat-launch.vbs'
$launcherPath = Join-Path $repoRoot 'tools\orbcat-launch.ps1'
$iconPath     = Join-Path $repoRoot 'src-tauri\icons\icon.ico'
$wscriptPath  = Join-Path $env:SystemRoot 'System32\wscript.exe'
$cscriptPath  = Join-Path $env:SystemRoot 'System32\cscript.exe'

$desktopDir   = [Environment]::GetFolderPath('Desktop')
$startMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$lnkName      = 'orbcat.lnk'
$desktopLnk   = Join-Path $desktopDir $lnkName
$startLnk     = Join-Path $startMenuDir $lnkName

# 旧命名（Windows 生成的「xxx - 快捷方式.lnk」），装上新的之后要提示清掉，
# 否则桌面上会有两个图标，其中一个还指向废弃的 _old-pretrust-orbcat.exe。
$legacyNames = @('orbcat.exe - 快捷方式.lnk', 'orbcat - 快捷方式.lnk')

function Write-Head([string]$Text) {
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

# 体检结果放脚本作用域，**不用 return 传出**。
#
# ⚠️ 踩过的坑：原先写 `return $ok`，而函数里 `& $cscriptPath ...` 的输出也会进
# 管道 —— 于是 `$ok = Test-Chain` 拿到的是「启动器输出 + 布尔值」拼成的数组，
# 体检结果一行都没显示，`exit $(if ($ok) ...)` 也判错。
# 规矩：这个函数只往屏幕写，结论走 $script:ChainOk。
$script:ChainOk = $true

function Test-Chain {
    Write-Head '启动链体检'
    foreach ($item in @(
            @{ Label = 'wscript.exe（无控制台宿主）'; Path = $wscriptPath },
            @{ Label = 'orbcat-launch.vbs（固定入口）'; Path = $vbsPath },
            @{ Label = 'orbcat-launch.ps1（挑"能用的那个"）'; Path = $launcherPath },
            @{ Label = '图标 icon.ico'; Path = $iconPath })) {
        if (Test-Path -LiteralPath $item.Path -PathType Leaf) {
            Write-Host ("  [OK]   {0}" -f $item.Label)
        } else {
            Write-Host ("  [缺失] {0}`n         {1}" -f $item.Label, $item.Path) -ForegroundColor Red
            $script:ChainOk = $false
        }
    }

    Write-Head '快捷方式解析结果'
    foreach ($lnk in @($desktopLnk, $startLnk)) {
        if (-not (Test-Path -LiteralPath $lnk)) {
            Write-Host ("  [未安装] {0}" -f $lnk) -ForegroundColor DarkGray
            continue
        }
        $sh = New-Object -ComObject WScript.Shell
        $l = $sh.CreateShortcut($lnk)
        Write-Host ("  [已安装] {0}" -f $lnk)
        Write-Host ("           目标   : {0}" -f $l.TargetPath)
        Write-Host ("           参数   : {0}" -f $l.Arguments)
        Write-Host ("           工作目录: {0}" -f $l.WorkingDirectory)
        Write-Host ("           图标   : {0}" -f $l.IconLocation)
        if ($l.TargetPath -notlike '*wscript.exe') {
            Write-Host '           ⚠ 目标不是 wscript.exe —— 这份快捷方式可能还是"直指 exe"的旧式，建议重装。' -ForegroundColor Yellow
        }
    }

    Write-Head '现在会启动哪一份构建'
    # 经 -OutFile 拿 UTF-8 文本再读回来，而不是直接吃子进程 stdout：
    # wscript/cscript 的 stdout 走控制台代码页，中文在 pwsh 管道里会变成乱码。
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("orbcat-verify-{0}.txt" -f ([guid]::NewGuid().ToString('N')))
    try {
        & $cscriptPath //nologo $vbsPath -List -OutFile $tmp | Out-Null
        $rc = $LASTEXITCODE
        if (Test-Path -LiteralPath $tmp) {
            [System.IO.File]::ReadAllText($tmp, [System.Text.Encoding]::UTF8) -split "`r?`n" |
                Where-Object { $_ -ne '' } | ForEach-Object { Write-Host "  $_" }
        }
        if ($rc -ne 0) {
            Write-Host '  ⚠ 启动器没找到可用的构建（上面的提示里有构建命令）。' -ForegroundColor Yellow
            $script:ChainOk = $false
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------------
# -Verify：只体检
# ---------------------------------------------------------------------------
if ($Verify) {
    Write-Host "仓库根: $repoRoot"
    Test-Chain
    Write-Host ''
    if ($script:ChainOk) { Write-Host '结论：链路完整。' -ForegroundColor Green }
    else { Write-Host '结论：有问题，见上面标红/标黄的行。' -ForegroundColor Yellow }
    exit $(if ($script:ChainOk) { 0 } else { 1 })
}

# ---------------------------------------------------------------------------
# -Uninstall：卸载
# ---------------------------------------------------------------------------
if ($Uninstall) {
    Write-Head '卸载快捷方式'
    foreach ($lnk in @($desktopLnk, $startLnk)) {
        if (Test-Path -LiteralPath $lnk) {
            Remove-Item -LiteralPath $lnk -Force
            Write-Host ("  已删除 {0}" -f $lnk)
        } else {
            Write-Host ("  跳过（不存在）{0}" -f $lnk) -ForegroundColor DarkGray
        }
    }
    Write-Host ''
    Write-Host '启动器脚本本身保留在仓库里，随时可以再装回来。' -ForegroundColor DarkGray
    exit 0
}

# ---------------------------------------------------------------------------
# 默认：装 / 修
# ---------------------------------------------------------------------------
foreach ($p in @($vbsPath, $launcherPath, $wscriptPath)) {
    if (-not (Test-Path -LiteralPath $p -PathType Leaf)) {
        throw "缺少必需文件：$p"
    }
}

function Install-Lnk {
    param([Parameter(Mandatory)][string]$LnkPath, [Parameter(Mandatory)][string]$Label)

    $sh = New-Object -ComObject WScript.Shell
    $l = $sh.CreateShortcut($LnkPath)
    $l.TargetPath       = $wscriptPath
    # 只传脚本路径，不传参数 → vbs 走"隐藏 + 不等待"的正常模式
    $l.Arguments        = '"{0}"' -f $vbsPath
    $l.WorkingDirectory = $scriptPath
    $l.Description      = 'orbcat 悬浮球助手（常驻入口：始终启动最新可用的构建）'
    if (Test-Path -LiteralPath $iconPath) {
        $l.IconLocation = '{0},0' -f $iconPath
    }
    $l.Save()

    # 立刻回读校验 —— 只信"再读出来的值"
    $back = $sh.CreateShortcut($LnkPath)
    if ($back.TargetPath -ne $wscriptPath -or $back.Arguments -notlike "*$vbsPath*") {
        throw "写入后回读不一致：$LnkPath"
    }
    Write-Host ("  [OK] {0} → {1}" -f $Label, $LnkPath) -ForegroundColor Green
    Write-Host ("       目标 {0} {1}" -f $back.TargetPath, $back.Arguments) -ForegroundColor DarkGray
}

Write-Host "仓库根: $repoRoot"
Write-Head '安装快捷方式'
Install-Lnk -LnkPath $desktopLnk -Label '桌面'
if ($StartMenu) {
    Install-Lnk -LnkPath $startLnk -Label '开始菜单'
}

# 提示旧式快捷方式，避免桌面上两个图标互相打架
$legacyFound = @()
foreach ($n in $legacyNames) {
    $p = Join-Path $desktopDir $n
    if (Test-Path -LiteralPath $p) { $legacyFound += $p }
}
if ($legacyFound.Count -gt 0) {
    Write-Head '发现旧式快捷方式（建议删掉，否则桌面上会有两个 orbcat 图标）'
    foreach ($p in $legacyFound) {
        $sh = New-Object -ComObject WScript.Shell
        $l = $sh.CreateShortcut($p)
        Write-Host ("  {0}`n      → {1}" -f $p, $l.TargetPath) -ForegroundColor Yellow
    }
    Write-Host '  删除命令（想删再跑）：' -ForegroundColor DarkGray
    foreach ($p in $legacyFound) {
        Write-Host ("    Remove-Item -LiteralPath '{0}'" -f $p) -ForegroundColor DarkGray
    }
}

Write-Head '完成'
Write-Host '以后重新构建（tools\build-release.sh）不用再管桌面图标 ——'
Write-Host '每次双击都会重新挑一次"当前能用的那份"构建。'
Write-Host ''
Write-Host '体检：pwsh -File tools\install-shortcut.ps1 -Verify' -ForegroundColor DarkGray
exit 0
