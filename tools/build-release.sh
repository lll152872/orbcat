#!/usr/bin/env bash
# orbcat 发布构建。
#
# ## 构建与桌面快捷方式的关系（2026-10-01 起）
#
#   桌面快捷方式**不指向任何 exe**。调用链是：
#     桌面 orbcat.lnk → wscript.exe → tools\orbcat-launch.vbs
#       → tools\orbcat-launch.ps1 → 运行时挑出"当前能用的那份" exe
#
#   所以本脚本**不再需要同步任何副本**。历史上那句
#   `cp orbcat.exe _old-pretrust-orbcat.exe` 是因为桌面 .lnk 的
#   LinkTargetIDList 写死了文件名（Windows 生成的 .lnk 改不动），
#   只能靠"保证那个名字的文件始终是最新构建"来兜。现在启动器在运行时挑，
#   改名 / 换档位 / 构建失败留残骸都不会把图标带偏，补丁随之删除。
#
#   仓库挪位置后需要重跑一次：pwsh -File tools\install-shortcut.ps1
#
# ## 用法
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

# 旧 exe 归档进 _old_builds/（那里没有 dist/，启动器天然不会选它）。
# 归档而不是就地改名，是为了让 release 根始终只有一个 orbcat.exe，
# 不给"哪份才是新的"留下歧义。
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

# ---- 内置运行时（node / pi 内核）----
#
# 位置契约：`pi::runtime_dir(exe)` 找 **`<exe 同目录>/runtime/`**。
# ⚠️ 目录名写错 = pi 内核静默不可用。
#
# 分发内容（2026-10-23 用户拍板）：**只带 node + pi**（≈95MB）。
#   · node —— PTC run_code 的硬依赖，pi 本身也是 node 路线
#   · pi   —— 内核本体
#   · **不带 git**（GPLv2 不再分发）、**不带 python**（不是硬依赖，
#     模型可能调而已 —— `pi::bundled_python()` 返回 Option，缺席即回落系统）
#
# ⚠️ **缺 runtime 就当场失败，不许「warn 一下继续」**（审计 F1）：
# 旧版是「没有也不阻断构建」，于是产出的是**没有内核的包** ——
# pi 内核在用户机器上静默失效，而构建日志一切正常。
#
# ⚠️ 每次都重建 $REL/runtime（不靠「不存在才拷」）：
# 旧逻辑会把上一轮的旧 runtime 留在那儿 —— 组件清单变了，包没跟着变。
RUNTIME_SRC=runtime
RUNTIME_COMPONENTS="node pi"

missing=""
for c in $RUNTIME_COMPONENTS; do
  [ -d "$RUNTIME_SRC/$c" ] || missing="$missing $c/"
done
if [ -n "$missing" ]; then
  echo "============================================================"
  echo " ✗ 缺少内置运行时组件：$missing"
  echo ""
  echo "   发布包必须带 runtime/{node,pi} —— 缺了 pi 内核在用户机器上静默失效。"
  echo "   修法（Git Bash）："
  echo "     node tools/build-runtime-assets.mjs"
  echo "   （它需要 tools/.cache/ 里有 node.exe 与 pi 包："
  echo "     npm pack @earendil-works/pi-coding-agent@1.4.0）"
  echo "============================================================"
  exit 1
fi

echo "==> 拷贝内置运行时到 $REL/runtime（node / pi）"
# 先清掉上一轮 —— runtime/ 有 700+ 文件，cp -rf 只增不减会攒垃圾
rm -rf "$REL/runtime" 2>/dev/null || true
mkdir -p "$REL/runtime"
for c in $RUNTIME_COMPONENTS; do
  cp -rf "$RUNTIME_SRC/$c" "$REL/runtime/$c"
done
# runtime/ 里残留的 git/、python/ **不进包**（白名单拷贝）
for c in git python; do
  [ -d "$RUNTIME_SRC/$c" ] && echo "  （$c/ 本轮不分发，未进包）"
done
echo "  $(du -sh "$REL/runtime" 2>/dev/null | cut -f1)"

# ── 🔴 extension 副本防漂移闸门（2026-10-25 守门加）────────────────────
#
# 背景：`runtime/pi/extensions/*.ts` 是 `runtime-ext/*.ts` 的**拷贝**
# （runtime/ 整个是构建产物，被 .gitignore 忽略）。
# ⇒ **改了 runtime-ext/ 的源码但没重建 runtime/** ⇒ 包里带的是**旧 extension**。
#
# 实测踩到：prompt 合成那笔（runtime-ext/orbcat-prompt.ts 从「整段替换」改成
# 「合成模式」）合进基线后，`runtime/pi/extensions/orbcat-prompt.ts` 还是旧的
# （72 行 vs 源码 108 行）⇒ 宿主已按「拆分」返回 prompt、扩展还按「整串」处理
# ⇒ **契约不配 ⇒ 真机上的 prompt 合成是坏的**，而构建日志一片绿。
#
# ⇒ 这里**当场断言副本 == 源码**，不一致就**当场失败**，不许带着旧 extension 出包。
#   修法：node tools/build-runtime-assets.mjs（会重新把 runtime-ext/ 拷进 runtime/）
if [ -d runtime-ext ] && [ -d "$RUNTIME_SRC/pi/extensions" ]; then
  stale=""
  for f in runtime-ext/*.ts; do
    [ -f "$f" ] || continue
    base=$(basename "$f")
    if ! cmp -s "$f" "$RUNTIME_SRC/pi/extensions/$base"; then
      stale="$stale $base"
    fi
  done
  if [ -n "$stale" ]; then
    echo "============================================================"
    echo " ✗ runtime/pi/extensions 与 runtime-ext/ 源码不一致：$stale"
    echo ""
    echo "   runtime/ 是构建产物，extensions/ 是 runtime-ext/ 的拷贝 ——"
    echo "   改了源码没重建 runtime ⇒ **包里带的是旧 extension**（真机上功能是坏的，"
    echo "   而构建日志全绿）。2026-10-25 实测：prompt 合成就是这样漏出去的。"
    echo ""
    echo "   修法（Git Bash）：node tools/build-runtime-assets.mjs"
    echo "============================================================"
    exit 1
  fi
  echo "  ✓ extension 副本与 runtime-ext/ 源码一致（防漂移闸门通过）"
fi

echo "==> 拷贝前端资源到 $REL/dist（外置：exe 必须与 dist/ 同目录分发）"
mkdir -p "$REL/dist"
# 先清掉上一轮的 hash 产物：`cp -rf` 只增不减，dist/assets 会攒下十几次
# 构建的旧 bundle（实测同一个 exe 目录里躺着 5 份 main-*.js / 10 份
# window-*.css）。index.html 只引用新的那一个，但整个目录是要分发的。
# 运行中的 exe/WebView 可能占着文件，所以失败不致命（|| true）。
rm -rf "$REL/dist/assets" 2>/dev/null || true
cp -rf dist/. "$REL/dist/"

# ---- 部署到运行态目录（2026-10-12 加）----
#
# 为什么不直接让桌面图标用 $REL（target/release/）：
#   orbcat 的 exe 会在**同级**找 agent-data\。用 target/release/ 当运行态，
#   就等于把用户的真实数据（API key / 会话 / 记忆 / 截图）放进 git 仓库目录里。
#   即使 .gitignore 挡住了也不放心 —— 那是"靠配置保证不泄漏"，不是结构上不可能。
#
# 为什么不能省掉这一步、让运行态保持手动拷贝：
#   启动器按「新鲜度 min(exe 时间, dist\index.html 时间)」排序。
#   不部署的话，构建完 $REL 是新的、$RUNTIME 是旧的，
#   双击桌面图标启动的一直是旧的 —— 就是上面注释里写的"新内核 + 旧界面"。
#
# RUNTIME 必须同时满足三个条件（实测，详见 BUG-orb-window-missing.md）：
#   1. 在 git 仓库外面；2. 在 D:\ 根部；3. 不在 D:\workplace\悬浮小agent\ 底下
#      —— 那条路径上的 exe **100% 起不来**（WebView2 子进程数为 0，orb 窗口不出现）。
RUNTIME=${ORBCAT_RUNTIME_DIR:-D:/orbcat}

echo "==> 部署到运行态 $RUNTIME"
mkdir -p "$RUNTIME"
# 运行中的 orbcat 可能占着 exe —— 部署失败不致命，下面的体检会提示用户先退出。
cp -f "$REL/orbcat.exe" "$RUNTIME/orbcat.exe"
# dist 同样先清 assets 再整份覆盖，保证运行态里不会攒下旧 bundle。
rm -rf "$RUNTIME/dist/assets" 2>/dev/null || true
mkdir -p "$RUNTIME/dist"
cp -rf "$REL/dist/." "$RUNTIME/dist/"

# 内置运行时同样部署到运行态 —— 位置契约同上（<exe 同目录>/runtime/）
#
# ⚠️ 这里也只部署 node + pi（$REL/runtime 里就这两个）——
# 运行态的 runtime/ 是**启动器判据**的一部分（审计 F2）：
# 缺 runtime\node\node.exe 或 runtime\pi\ 的运行态会被启动器判为不合格，
# 双击桌面图标就会去启动别的副本。
if [ -d "$REL/runtime" ]; then
  echo "==> 部署内置运行时到 $RUNTIME/runtime"
  rm -rf "$RUNTIME/runtime" 2>/dev/null || true
  mkdir -p "$RUNTIME/runtime"
  cp -rf "$REL/runtime/." "$RUNTIME/runtime/"
  echo "  $(du -sh "$RUNTIME/runtime" 2>/dev/null | cut -f1)"
  for c in node pi; do
    if [ ! -d "$RUNTIME/runtime/$c" ]; then
      echo "  ✗ 部署后缺 runtime/$c —— 启动器会判这份运行态不合格！"
      exit 1
    fi
  done
fi

echo "==> 部署初始化指引（包根，与 agent-data 同级）"
# ⚠️ **位置必须是包根**，不是 agent-data/ 下面 ——
#    prompt 的原话是「读**项目根**的 `初始化.md`（和 `agent-data/` 同级）」，
#    启动器又用 `-WorkingDirectory <exe 目录>` 起进程
#    （tools/orbcat-launch.ps1）⇒ 只有包根那份找得到。
#
# ⚠️ 旧实现找的是 `src-tauri/skeleton/agent-data/初始化.md` —— 那个路径
#    **从来不存在** ⇒ 这段一直是死分支 ⇒ 部署态从来没有过初始化.md
#    ⇒ 新用户点完「建出目录结构」，agent 被引导去读一个不存在的文件
#    ⇒ 初始化链路断链。
#
# 源：仓库根的 `初始化.md`（入库文件，不是生成物）。
# 它是**说明书**，不是用户数据 ⇒ 覆盖部署无风险。
mkdir -p "$RUNTIME/agent-data"
if [ -f "初始化.md" ]; then
  cp -f "初始化.md" "$RUNTIME/初始化.md"
  echo "  已部署 初始化.md（包根，与 agent-data 同级）"
else
  echo "  ✗ 仓库根没有 初始化.md —— 新用户初始化链路会断链，拒绝部署"
  exit 1
fi

echo "==> 检查桌面快捷方式"
# 2026-10-01 起，桌面快捷方式不再指向任何 exe：它指向 tools\orbcat-launch.vbs
# → orbcat-launch.ps1，由启动器在每次双击时挑出"当前能用的那份"构建。
# 所以**构建不再需要同步任何副本**（旧的 `cp orbcat.exe _old-pretrust-orbcat.exe`
# 已删除 —— 那是快捷方式被迫写死文件名时的补丁，现在没有存在意义了）。
#
# 这里只做一次体检，确认链路还在。找不到启动器就提示装一次。
if [ -f "tools/install-shortcut.ps1" ]; then
  if command -v pwsh >/dev/null 2>&1; then
    pwsh -NoProfile -File tools/install-shortcut.ps1 -Verify || true
  fi
else
  echo "  ⚠ 没找到 tools/install-shortcut.ps1，桌面快捷方式无法体检。"
fi

echo
echo "完成："
ls -la "$REL/orbcat.exe"
echo
echo "已部署到运行态（桌面图标下次双击就是这份）："
ls -la "$RUNTIME/orbcat.exe"
echo
echo "运行态 agent-data 未被改动。"
