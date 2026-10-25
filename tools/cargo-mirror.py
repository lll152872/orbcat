#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""cargo 本机配置：镜像 source replacement + 只预取当前平台的依赖

## 为什么需要这个（2026-10-25 实测）
`cargo test --lib` 在**解析/校验 Cargo.lock 完整性**时，会去下载 lock 里
**当前平台用不到**的 crate（例：`objc2-app-kit` —— macOS only，tauri 2.11.5 的
macOS target 依赖）。本机直连 crates.io 太慢 ⇒
```
unable to update registry `crates-io`
Caused by: download of ob/jc/objc2-app-kit failed
Caused by: [28] Timeout was reached (Operation too slow. Less than 10 bytes/sec ...)
```
⇒ 退出码 101，**没进到编译阶段**。

## 两个动作
① `apply` ＝ 给本机 cargo 配置**追加**镜像 source replacement
   （只动 `[source.*]` 段，**绝不覆盖** `[build] target-dir`）
② `fetch` ＝ 只预取**当前平台**要用的依赖（`--target x86_64-pc-windows-msvc`），
   避免再被全平台依赖拖下水

⚠️ **换源的代价**：source id 变了 ⇒ 本地 `~/.cargo/registry` 里**已下载的 crate
全部失效** ⇒ 首次会**全量重下**（本项目 300+ crate）⇒ 所以
**① 要在没人正在跑 cargo 的时候做**（构建槽空闲时）。

## 用法
```bash
python tools/cargo-mirror.py dry-run            # 只看会写什么，不落盘
python tools/cargo-mirror.py apply              # 幂等：追加/更新镜像段
python tools/cargo-mirror.py apply --index tuna # 换清华源
python tools/cargo-mirror.py fetch              # 只预取当前平台依赖
python tools/cargo-mirror.py status             # 打印当前 source 段
```

## 注意
· 本机 cargo 配置默认在 `D:/workplace/.cargo/config.toml`（**在 git 仓库之外**
  ⇒ 不会提交、不会污染 CI、不会进发布包）；CI 用 GitHub runner 自己的配置。
· 本脚本**只**改 `D:/workplace/.cargo/config.toml` 这一个文件（可用 `--config` 覆盖），
  **不动** `Cargo.toml` / `Cargo.lock` / 任何仓库文件。
"""
import argparse
import os
import re
import shutil
import subprocess
import sys

DEFAULT_CONFIG = r"D:\workplace\.cargo\config.toml"
HOST_TARGET = "x86_64-pc-windows-msvc"

# 镜像注释块的标记串 —— **写入与剥离共用同一个**（单一事实源）
# ⚠️ 换标记串 = 老配置里的旧注释块再也剥不掉 ⇒ 又会开始堆叠 ⇒ 别随便改。
MIRROR_MARKER = "【crates.io 镜像】"

# 稀疏索引镜像（checksum 与 crates.io 一致：原样镜像）
INDEXES = {
    "rsproxy": ("rsproxy-sparse", "sparse+https://rsproxy.cn/index/"),
    "tuna": ("tuna-sparse", "sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/"),
}


def cargo_exe():
    """真 cargo 的绝对路径（本机 PATH 里有一条失效残留 /d/cache/cargo/bin ⇒ 非交互 shell 里 cargo 会 127）。"""
    for p in (r"C:\Users\DELL\.cargo\bin\cargo.exe",):
        if os.path.isfile(p):
            return p
    found = shutil.which("cargo")
    if found:
        return found
    raise SystemExit("找不到 cargo：既没有 C:\\Users\\DELL\\.cargo\\bin\\cargo.exe，PATH 里也没有")


def read_config(path):
    if not os.path.isfile(path):
        return ""
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def strip_source_sections(text):
    """去掉所有 [source.*] 段（连同其下的键），其余原样保留。"""
    out, skip = [], False
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("[") and s.endswith("]"):
            skip = s.startswith("[source.")
        if not skip:
            out.append(line)
    return out


def strip_mirror_comment_blocks(lines):
    """去掉所有**镜像注释块**（以 `# 【crates.io 镜像】` 开头的连续注释行）。

    ⚠️ **为什么必须单独剥**（2026-10-25 实测踩到）：
    注释块**不在 `[source.*]` 段里** ⇒ `strip_source_sections` 剥不掉它
    ⇒ 每次 apply 都「旧的留下 + 新的又追加」⇒ 同一段注释越堆越多
    （实测 apply 三次 ⇒ 注释出现三遍；TOML 本身仍有效，所以不报错、只是脏）。

    ⇒ 标记串是**单一事实源**：写入用它（`build_mirror_block`）、剥离也用它。
    """
    out, skipping = [], False
    for line in lines:
        s = line.strip()
        if skipping:
            if s.startswith("#"):
                continue          # 注释块的续行
            skipping = False     # 遇到非注释行 ⇒ 块结束
        if s.startswith("#") and MIRROR_MARKER in s:
            skipping = True
            continue
        out.append(line)
    return out


def tidy_blank_lines(lines):
    """去掉由此产生的连续空行（最多留一个）。"""
    cleaned = []
    for ln in lines:
        if not ln.strip() and cleaned and not cleaned[-1].strip():
            continue
        cleaned.append(ln)
    return cleaned


def build_mirror_block(index_name):
    alias, url = INDEXES[index_name]
    return (
        f"\n# {MIRROR_MARKER}2026-10-25 守门加：直连 crates.io 太慢，cargo 校验 Cargo.lock\n"
        "# 完整性时会去下载**当前平台用不到**的 crate（objc2-app-kit 等 macOS-only）⇒ 超时 ⇒ 退出码 101。\n"
        "# 镜像与 crates.io **checksum 完全一致**（原样镜像）⇒ Cargo.lock 不用改。\n"
        "# ⚠️ 换 source id ⇒ 本地 registry 缓存里已下载的 crate 全部失效 ⇒ 首次会全量重下。\n"
        f"[source.crates-io]\n"
        f'replace-with = "{alias}"\n'
        f"\n[source.{alias}]\n"
        f'registry = "{url}"\n'
    )


def count_mirror_blocks(text):
    """配置里镜像注释块有几份（>1 就是重复堆叠了）。"""
    return sum(1 for l in text.splitlines()
               if l.strip().startswith("#") and MIRROR_MARKER in l)


def apply_mirror(config_path, index_name, dry_run=False):
    alias, url = INDEXES[index_name]
    text = read_config(config_path)
    has = re.search(r"^\s*\[source\.crates-io\]", text, re.M) is not None
    old_blocks = count_mirror_blocks(text)
    # ⚠️ 顺序很重要：先剥 [source.*] 段，再剥**镜像注释块**（它不在段里）
    kept_lines = strip_mirror_comment_blocks(strip_source_sections(text))
    kept = "\n".join(tidy_blank_lines(kept_lines)).rstrip() + "\n"
    new_text = kept + build_mirror_block(index_name)
    print("=== 目标配置：%s ===" % config_path)
    print("已存在 [source.crates-io]：%s" % ("是（将被替换为 %s）" % alias if has else "否（将新增）"))
    print("原有镜像注释块：%d 份%s" % (
        old_blocks, "（将被归并成 1 份）" if old_blocks > 1 else ""))
    print("--- 将写入的尾部内容 ---")
    print(new_text[len(kept):].rstrip())
    if dry_run:
        print("（dry-run：未落盘）")
        return 0
    os.makedirs(os.path.dirname(config_path), exist_ok=True)
    with open(config_path, "w", encoding="utf-8", newline="\n") as f:
        f.write(new_text)
    print("✓ 已写入：%s" % config_path)
    # ⚠️ **幂等自检**：写完再读，注释块与 [source.*] 段都必须各只有一份 ——
    #   否则「apply 两次 = 文件变两次」这种脏问题又会悄悄回来。
    written = read_config(config_path)
    n_comment = count_mirror_blocks(written)
    n_src = len(re.findall(r"^\s*\[source\.", written, re.M))
    ok = (n_comment == 1 and n_src == 2)
    print("   幂等自检：镜像注释块 %d 份（应 1）· [source.*] 段 %d 个（应 2：crates-io + 镜像别名）%s"
          % (n_comment, n_src, " ✓" if ok else " ❌"))
    if not ok:
        return 1
    print("⚠️ 首次 cargo 会**全量重下**依赖（source id 变了 ⇒ 旧缓存失效）")
    return 0


def show_status(config_path):
    text = read_config(config_path)
    if not text:
        print("配置不存在：%s" % config_path)
        return 0
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("[") or s.startswith("replace-with") or s.startswith("registry") or s.startswith("target-dir"):
            print("  " + s)
    # ⚠️ 顺手报一下**漂移**：镜像注释块 > 1 份 = 老版本 apply 留下的重复堆叠
    n = count_mirror_blocks(text)
    print("  镜像注释块：%d 份%s" % (n, " ✓" if n <= 1 else " ⚠️ 重复堆叠（跑一次 apply 即可归并）"))
    return 0


def fetch_current_platform(config_path, target=HOST_TARGET):
    exe = cargo_exe()
    print("cargo = %s" % exe)
    print("预取目标平台依赖：--target %s（跳过 macOS-only 依赖）" % target)
    env = dict(os.environ)
    env["PATH"] = os.path.dirname(exe) + os.pathsep + env.get("PATH", "")
    cmd = [exe, "fetch", "--target", target]
    print("$ " + " ".join(cmd))
    return subprocess.call(cmd, env=env)


def bootstrap_worktree(wt, main_wt, target=HOST_TARGET, do_mirror=True, index_name="rsproxy"):
    r"""新 worktree 引导：**这才是「cargo 跑不完」的真正根因所在**

    ⚠️ 症状（伪装成 cargo 网络问题）：
    ```
    error: failed to run custom build command for `orbcat v2.0.0`
      resource path `..\dist` doesn't exist
    ```
    根因：`git worktree add` 出来的目录**没有 node_modules** ⇒ `npm run build` 跑不了
    ⇒ **`dist/` 生成不出来** ⇒ tauri 的 build.rs 读 `../dist` 失败 ⇒ **编译期就 exit**。
    ⇒ 于是「cargo 跑不完」看起来像下载超时（其实中途还可能撞上 crates.io 超时）。

    步骤：① 拷 node_modules → ② npm run build（出 dist/）→ ③ 只预取当前平台依赖 → ④ 可选换镜像源
    """
    exe = cargo_exe()
    env = dict(os.environ)
    env["PATH"] = os.path.dirname(exe) + os.pathsep + env.get("PATH", "")
    nm = os.path.join(wt, "node_modules")
    if not os.path.isdir(nm):
        src = os.path.join(main_wt, "node_modules")
        if not os.path.isdir(src):
            print("✗ 找不到源 node_modules：%s" % src)
            return 2
        print("① 拷 node_modules：%s -> %s" % (src, nm))
        shutil.copytree(src, nm, symlinks=True)
        print("   ✓ %d 个文件" % sum(len(f) for _, _, f in os.walk(nm)))
    else:
        print("① node_modules 已存在，跳过")

    print("② npm run build（生成 dist/）")
    npm = shutil.which("npm") or shutil.which("npm.cmd")
    for cand in (r"D:\NODE.JS\npm.cmd", r"C:\Program Files\nodejs\npm.cmd"):
        if os.path.isfile(cand):
            npm = cand
            break
    if not npm:
        print("   ✗ 找不到 npm（试过 PATH 与常见安装位置）")
        return 2
    print("   npm = %s" % npm)
    r = subprocess.call([npm, "run", "build"], cwd=wt, env=env)
    if r != 0:
        print("   ✗ npm run build 失败（%d）" % r)
        return r
    print("   ✓ dist/ 已生成：%s" % os.path.isfile(os.path.join(wt, "dist", "index.html")))

    print("③ 只预取当前平台依赖（--target %s，避开 macOS-only 的 objc2 等）" % target)
    r = subprocess.call([exe, "fetch", "--target", target], cwd=os.path.join(wt, "src-tauri"), env=env)
    print("   fetch RC=%d" % r)
    if do_mirror:
        print("④ 换 crates.io 镜像源（抗直连超时）")
        apply_mirror(DEFAULT_CONFIG, index_name, dry_run=False)
    return 0


def main():
    # Windows 控制台默认 GBK ⇒ 打印 ⇒ / ✔ 这类符号会 UnicodeEncodeError ⇒ 兜底成 UTF-8
    for _s in (sys.stdout, sys.stderr):
        try:
            _s.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass
    ap = argparse.ArgumentParser(description="cargo 镜像配置 + 只预取当前平台依赖")
    ap.add_argument("action", choices=["apply", "dry-run", "status", "fetch", "bootstrap"])
    ap.add_argument("--worktree", default=r"D:\workplace\wt-cargo")
    ap.add_argument("--main-wt", default=r"D:\workplace\wt-release")
    ap.add_argument("--no-mirror", action="store_true")
    ap.add_argument("--config", default=DEFAULT_CONFIG)
    ap.add_argument("--index", default="rsproxy", choices=sorted(INDEXES))
    ap.add_argument("--target", default=HOST_TARGET)
    a = ap.parse_args()
    if a.action == "apply":
        return apply_mirror(a.config, a.index, dry_run=False)
    if a.action == "dry-run":
        return apply_mirror(a.config, a.index, dry_run=True)
    if a.action == "status":
        return show_status(a.config)
    if a.action == "fetch":
        return fetch_current_platform(a.config, a.target)
    if a.action == "bootstrap":
        return bootstrap_worktree(a.worktree, a.main_wt, a.target, not a.no_mirror, a.index)
    return 1


if __name__ == "__main__":
    sys.exit(main())
