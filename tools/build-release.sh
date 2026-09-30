#!/usr/bin/env bash
# orbcat 发布构建 + 同步桌面快捷方式的目标文件。
#
# 为什么要这个脚本（2026-09-26 踩坑）：
#   桌面 `orbcat.exe - 快捷方式.lnk` 是 **Windows 生成的**，它的 LinkTargetIDList 里
#   写死了 `_old-pretrust-orbcat.exe` 这个文件名。手写的 .lnk 会被 Explorer 判成
#   "没有与之关联的应用"（`SE_ERR_NOASSOC`）—— 试了三轮都不行，IDList 也没救回来。
#   所以只能保留那份原生 .lnk，并**保证它指向的文件始终是最新构建**。
#
#   只跑 `cargo build --release` 的话，桌面图标会启动旧构建 —— 就是当天那个
#   "权限 chip / 心跳消失"的根因。这个脚本把复制焊进流程，杜绝再犯。
#
# 用法：
#   bash tools/build-release.sh          # 正式版：fat LTO（lto=true, cgu=1）—— 最慢最小，发版用
#   bash tools/build-release.sh --fast   # 快速版：release-fast profile（无 LTO + cgu=256）—— 实测稳态 55s，给人真机测试用
#
# ⚠️⚠️ **必须在 Git Bash 里跑，不能在 WSL 里跑**（2026-09-30 踩坑）
#
#   本机 PATH 上的 `bash` 是 **WSL 的** `C:\Windows\system32\bash.exe`，
#   所以直接敲 `bash tools/build-release.sh` 会进 Ubuntu。而 `node_modules`
#   是 Windows 装的，里面只有 `rolldown-binding.win32-x64-msvc.node`
#   → 在 Linux 里 `npm run build` 直接炸：
#     `Cannot find module '../rolldown-binding.linux-x64-gnu.node'`
#
#   正确跑法（三选一）：
#     1. Git Bash 窗口里：      bash tools/build-release.sh --fast
#     2. PowerShell / cmd：    "C:\Program Files\Git\bin\bash.exe" -lc 'cd "/d/<你的仓库路径>/orbcat" && bash tools/build-release.sh --fast'
#     3. 双击/命令行：          tools\build-release.cmd --fast   （同目录的包装脚本）
#
#   一个线索：脚本里 `pwd` 若是 `/mnt/d/...` 就是 WSL（错）；
#   是 `/d/...` 才是 Git Bash（对）。
#
#   WSL 里也不是完全没救：可以用 Windows 的 node（`/mnt/d/NODE.JS/node.exe`），
#   但路径/换行/权限坑会一个接一个，不值当 —— 直接换 Git Bash。
#
# ⚠️ 两档用**独立的 profile + 独立的 target 子目录**，切换档位不重编依赖。
#    （2026-09-29 实测：用 CARGO_PROFILE_* env 覆盖会污染 fingerprint，切参数触发 294 crate 全量重编 5m15s。）
# ⚠️ --fast 首次使用会全量编一次依赖（约 5min，一次性），之后固定 55s。
set -euo pipefail

# ---- 环境自检：挡住"跑进 WSL"这个坑（2026-09-30 加）----
#
# 为什么要在这里挡：本机 PATH 上的 `bash` 是 WSL 的，直接敲
# `bash tools/build-release.sh` 会进 Ubuntu，然后在 `npm run build` 阶段
# 报一句 **误导性极强** 的错：
#   Cannot find module '../rolldown-binding.linux-x64-gnu.node'
# 看起来像"依赖装坏了"，实际是"你在错的操作系统里跑 Windows 的 node_modules"。
# 与其让人去查 rolldown，不如在这里就说清楚。
case "$(uname -s)" in
  Linux*)
    if [ -d /mnt/c ] || [ -d /mnt/d ]; then
      echo "============================================================"
      echo " ✗ 检测到你在 WSL (Linux) 里跑这个脚本，会失败。"
      echo ""
      echo "   原因：node_modules 是 Windows 装的，里面只有"
      echo "         rolldown-binding.win32-x64-msvc.node —— Linux 加载不了，"
      echo "         npm run build 会报 'Cannot find module ...linux-x64-gnu.node'，"
      echo "         那句错看起来像依赖坏了，实际是操作系统不对。"
      echo ""
      echo "   正确跑法（三选一）："
      echo "    1) Git Bash 窗口：   bash tools/build-release.sh --fast"
      echo "    2) PowerShell：      \"C:\\Program Files\\Git\\bin\\bash.exe\" -lc \\"
      echo "                          'cd \"/d/<你的仓库路径>/orbcat\" && bash tools/build-release.sh --fast'"
      echo "    3) 只跑 Rust 那半：  cargo build --profile release-fast  （前端单独 npm run build）"
      echo "============================================================"
      exit 1
    fi
    ;;
esac

cd "$(dirname "$0")/.."           # orbcat/
echo "==> 前端构建（npm run build）"
npm run build

MODE="${1:-}"

# 旧 exe 归档进 _old_builds/，别在 release 根里改名成别的 .exe（会甩掉桌面快捷方式）
REL=src-tauri/target/release
mkdir -p "$REL/_old_builds"
if [ -f "$REL/orbcat.exe" ]; then
  mv -f "$REL/orbcat.exe" "$REL/_old_builds/orbcat-$(date +%Y%m%d-%H%M%S).exe"
fi

if [ "$MODE" = "--fast" ]; then
  echo "==> Rust 构建（release-fast profile：无 LTO + cgu=256，快速模式）"
  ( cd src-tauri && cargo build --profile release-fast )
  SRC_EXE=src-tauri/target/release-fast/orbcat.exe
else
  echo "==> Rust 构建（release + fat LTO，正式模式）"
  ( cd src-tauri && cargo build --release )
  SRC_EXE=src-tauri/target/release/orbcat.exe
fi

# --fast 的产物在 target/release-fast/，要落到 release/ 供桌面快捷方式使用
if [ "$SRC_EXE" != "$REL/orbcat.exe" ]; then
  cp -f "$SRC_EXE" "$REL/orbcat.exe"
fi

echo "==> 拷贝前端资源到 $REL/dist（外置：exe 必须与 dist/ 同目录分发）"
mkdir -p "$REL/dist"
# 先清掉上一轮的 hash 产物：`cp -rf` 只增不减，dist/assets 会攒下十几次
# 构建的旧 bundle（实测同一个 exe 目录里躺着 5 份 main-*.js / 10 份
# window-*.css）。index.html 只引用新的那一个，但整个目录是要分发的。
# 运行中的 exe/WebView 可能占着文件，所以失败不致命（|| true）。
rm -rf "$REL/dist/assets" 2>/dev/null || true
cp -rf dist/. "$REL/dist/"

echo "==> 同步桌面快捷方式的目标文件"
cp -f "$REL/orbcat.exe" "$REL/_old-pretrust-orbcat.exe"

echo
echo "完成："
ls -la "$REL/orbcat.exe" "$REL/_old-pretrust-orbcat.exe"
echo
echo "桌面图标启动的是 _old-pretrust-orbcat.exe（已同步为本次构建）。"
