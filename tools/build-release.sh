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
# 用法：bash tools/build-release.sh
set -euo pipefail

cd "$(dirname "$0")/.."           # orbcat/
echo "==> 前端构建（npm run build）"
npm run build

echo "==> Rust 构建（cargo build --release）"
# 旧 exe 归档进 _old_builds/，别在 release 根里改名成别的 .exe（会甩掉桌面快捷方式）
REL=src-tauri/target/release
mkdir -p "$REL/_old_builds"
if [ -f "$REL/orbcat.exe" ]; then
  mv -f "$REL/orbcat.exe" "$REL/_old_builds/orbcat-$(date +%Y%m%d-%H%M%S).exe"
fi
( cd src-tauri && cargo build --release )

echo "==> 同步桌面快捷方式的目标文件"
cp -f "$REL/orbcat.exe" "$REL/_old-pretrust-orbcat.exe"

echo
echo "完成："
ls -la "$REL/orbcat.exe" "$REL/_old-pretrust-orbcat.exe"
echo
echo "桌面图标启动的是 _old-pretrust-orbcat.exe（已同步为本次构建）。"
