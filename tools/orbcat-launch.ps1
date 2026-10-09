<#
  orbcat-launch.ps1 —— 桌面快捷方式的**稳定入口**（"常驻"启动器）

  ## 为什么要有这一层
  桌面 .lnk 的 LinkTargetIDList 会把**目标文件名**写死。历史上构建脚本把旧 exe
  改名留在 release 根，快捷方式就被甩到那个旧文件名上 —— 用户每次双击都启动旧
  构建，新功能全部"消失"，被误判成 UI bug 排查了半天（见 allmemory/MEMORY.md）。

  根治办法不是"每次构建后重新同步那个文件"，而是**让快捷方式不指向任何 exe**：
  快捷方式 → orbcat-launch.vbs（路径永不改变）→ 本脚本 → 运行时挑出当前
  "能用的那个" exe。这样无论构建产物怎么改名、换档位（release / release-fast）、
  失败后残留旧构建，桌面图标都始终落在最新可用的那份上。

  对照：WorkBuddy 之类的商业软件不踩这个坑，是因为它**装到固定路径**
  （`...\WorkBuddyAI\WorkBuddyAI.exe`），升级时**原地替换同名文件**，
  快捷方式的目标路径自始至终没变。orbcat 没有安装器，所以用这一层把
  「目标永不改变」这个性质补出来。

  ## "能用"的判定（三条缺一不可）
  1. 文件存在、是有效 PE（MZ 头）、体积 > 1 MB（挡掉 0 字节 / 半截文件）
  2. **同目录有 dist\index.html** —— exe 已不自包含（frontendDist 指向 stub-dist，
     真实前端由 disk_assets.rs 从 exe 同目录的 dist\ 读），所以没有 dist 的 exe
     跑起来只会显示占位页，不算"能用"
  3. 因此 target\release\_old_builds\ 里的归档**天然不合格**（那里没有 dist/），
     不需要额外排除

  ## 用法
    pwsh -File tools\orbcat-launch.ps1              # 正常启动（被快捷方式调用）
    pwsh -File tools\orbcat-launch.ps1 -List        # 只打印解析结果，不启动（排查用）
    pwsh -File tools\orbcat-launch.ps1 -List -OutFile <路径>
                                                    # 结果写 UTF-8 文件而不是 stdout
                                                    # （orbcat-launch.vbs 用它取输出）
#>
[CmdletBinding()]
param(
    # 只列出候选与选中项，不启动（排查"为什么又跑了旧的"时用）
    [switch]$List,

    # 把结果写成 UTF-8 文件，而不是打到 stdout。
    # 为什么需要：调用方是 wscript.exe（GUI 子系统，没有控制台），拿不到子进程的
    # stdout；而让 VBS 经 cmd.exe 重定向又会撞上嵌套引号 + 中文编码两重坑。
    # 让 PowerShell 自己写文件，VBS 只管按 UTF-8 读回来，两边都干净。
    [string]$OutFile
)

$ErrorActionPreference = 'Stop'

# 仓库根：本脚本在 <repo>\tools\ 下，上一级即 <repo>
$scriptPath = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$repoRoot = Split-Path -Parent $scriptPath

function Test-OrbcatBuild {
    param([Parameter(Mandatory)][string]$ExePath)

    if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) { return $false }

    $fi = Get-Item -LiteralPath $ExePath
    if ($fi.Length -lt 1MB) { return $false }

    # PE 头 MZ —— 挡掉 0 字节、被截断、被换成文本的假 exe
    try {
        $fs = [System.IO.File]::OpenRead($ExePath)
        try {
            $head = New-Object byte[] 2
            if ($fs.Read($head, 0, 2) -ne 2) { return $false }
            if ($head[0] -ne 0x4D -or $head[1] -ne 0x5A) { return $false }
        } finally { $fs.Dispose() }
    } catch { return $false }

    # exe 不自包含：必须与 dist\index.html 同目录，否则起来只有占位页
    if (-not (Test-Path -LiteralPath (Join-Path $fi.DirectoryName 'dist\index.html') -PathType Leaf)) {
        return $false
    }

    return $true
}

# 输出汇聚：-OutFile 走文件（UTF-8 无 BOM），否则走 stdout
$script:Out = New-Object System.Collections.Generic.List[string]

function Add-Line {
    param([string]$Text = '')
    $script:Out.Add($Text)
}

function Emit-Output {
    $text = ($script:Out -join "`r`n")
    if ($OutFile) {
        $dir = Split-Path -Parent $OutFile
        if ($dir -and -not (Test-Path -LiteralPath $dir)) {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
        }
        [System.IO.File]::WriteAllText($OutFile, $text, (New-Object System.Text.UTF8Encoding($false)))
    } else {
        foreach ($l in $script:Out) { Write-Host $l }
    }
}

# 候选位置。只有旁边带 dist/ 的目录才可能合格；_old_builds/ 因此自动出局。
$patterns = @(
    'src-tauri\target\release\orbcat.exe',       # 正式产物（build-release.sh 的落点）
    'src-tauri\target\release-fast\orbcat.exe',  # 快速档产物
    'tmp\pkg\*\orbcat.exe'                       # 打包目录（兜底）
)

$candidates = @(
    foreach ($p in $patterns) {
        Get-Item -Path (Join-Path $repoRoot $p) -ErrorAction SilentlyContinue
    }
)

$valid = @($candidates | Where-Object { Test-OrbcatBuild -ExePath $_.FullName })

if ($valid.Count -eq 0) {
    $checked = ($patterns | ForEach-Object { '  ' + (Join-Path $repoRoot $_) }) -join "`n"
    Add-Line '没有找到可用的 orbcat 构建。'
    Add-Line ''
    Add-Line '判定"能用"的三条（缺一不可）：'
    Add-Line '  1. orbcat.exe 存在且是有效 PE，体积 > 1 MB'
    Add-Line '  2. 与它**同目录**有 dist\index.html'
    Add-Line '  3. （归档目录 _old_builds\ 里没有 dist\，所以天然不算）'
    Add-Line ''
    Add-Line '已检查：'
    Add-Line $checked
    Add-Line ''
    Add-Line '请先构建一次（在 Git Bash 里跑，不要在 WSL 里跑）：'
    Add-Line '  bash tools/build-release.sh --fast'
    Emit-Output

    if (-not $List -and -not $OutFile) {
        $popupMsg = ($script:Out -join "`r`n")
        (New-Object -ComObject WScript.Shell).Popup($popupMsg, 0, 'orbcat 启动失败', 16) | Out-Null
    }
    exit 1
}

# 一份构建的「新鲜度」= min(exe 时间, dist\index.html 时间)。
#
# 为什么要取 min 而不是只看 exe：exe 和 dist 是**两半**，只有一半是新的就等于旧的。
# 实测就撞上过：release-fast 的 exe 是 10-01，但它的 dist 还停在 09-29；
# 而 release 的 exe 是 10-01、dist 是 09-30 —— 按 exe 排会选中 release-fast，
# 起来就是「新内核 + 旧界面」。取 min 之后 release 正确胜出。
$ranked = $valid | ForEach-Object {
    $distTime = (Get-Item -LiteralPath (Join-Path $_.DirectoryName 'dist\index.html')).LastWriteTime
    [pscustomobject]@{
        Exe       = $_
        DistTime  = $distTime
        Freshness = if ($_.LastWriteTime -lt $distTime) { $_.LastWriteTime } else { $distTime }
    }
} | Sort-Object -Property @{ Expression = { $_.Freshness }; Descending = $true },
                          @{ Expression = { if ($_.Exe.DirectoryName -like '*\release') { 0 } else { 1 } } }

$best = ($ranked | Select-Object -First 1).Exe

if ($List) {
    Add-Line "仓库根  : $repoRoot"
    Add-Line '全部合格候选（按新鲜度 min(exe, dist) 新→旧）:'
    $ranked | ForEach-Object {
        Add-Line ("  新鲜度 {0}  exe {1}  dist {2}  {3}" -f `
            $_.Freshness.ToString('yyyy-MM-dd HH:mm:ss'),
            $_.Exe.LastWriteTime.ToString('MM-dd HH:mm:ss'),
            $_.DistTime.ToString('MM-dd HH:mm:ss'),
            $_.Exe.FullName)
    }
    Add-Line "选中    : $($best.FullName)"
    Emit-Output
    exit 0
}

# 分离启动：启动器退出后 orbcat 继续活着（单实例锁会处理"重复双击"→ 把球叫出来）
Start-Process -FilePath $best.FullName -WorkingDirectory $best.DirectoryName
