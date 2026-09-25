#!/usr/bin/env python3
"""真机 E2E：启动 mock 模型 + 可选启动 orbcat，指导/校验「点击到发送」。

模式：
  1) prepare-only（默认，无需 GUI 自动化权限）
     - 检查 mock 端口
     - 打印在 orbcat 里应如何配置模型与手测步骤
     - 校验 dist / release exe 是否就绪

  2) --probe-http
     - 向 mock 发一次 chat/completions，断言返回 MOCK_OPENAI_OK

依赖：先另开终端跑  python tests/e2e/mock_openai.py 8787
"""

from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PORT = 8787
URL = f"http://127.0.0.1:{PORT}/v1/chat/completions"
EXE = ROOT / "src-tauri" / "target" / "release" / "orbcat.exe"
DIST = ROOT / "dist" / "index.html"


def probe_http() -> int:
    body = {
        "model": "mock-local",
        "messages": [{"role": "user", "content": "ping from run_live_flow"}],
        "stream": False,
    }
    req = urllib.request.Request(
        URL,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer mock-key"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            data = json.loads(resp.read().decode())
    except urllib.error.URLError as e:
        print(f"[FAIL] mock 不可达：{e}\n请先运行：python tests/e2e/mock_openai.py {PORT}")
        return 1
    content = (data.get("choices") or [{}])[0].get("message", {}).get("content", "")
    print("[mock response]", content[:200])
    ok = "MOCK_OPENAI_OK" in (content or "")
    print("[result]", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def prepare_only() -> int:
    print("=== orbcat 真机 E2E 准备清单 ===\n")
    print("1) 启动假模型：")
    print(f"     python tests/e2e/mock_openai.py {PORT}\n")
    print("2) 启动 agent（任选其一）：")
    if EXE.exists():
        print(f"     {EXE}   (release 已存在, {EXE.stat().st_size} bytes)")
    else:
        print(f"     （尚无 release exe）npm run tauri build -- --no-bundle")
        print(f"     预期路径：{EXE}")
    if DIST.exists():
        print(f"     前端 dist 已就绪：{DIST}")
    else:
        print("     ⚠️ dist 不存在 —— 先 npm run build（Tauri 读 ../dist）\n")

    print("3) 在 orbcat 设置 › 模型 中添加：")
    print("     id:  mock-local")
    print(f"     url: http://127.0.0.1:{PORT}/v1")
    print("     key: mock-key\n")
    print("4) 手测点击链路（UI 测试无法覆盖真窗口焦点时走这里）：")
    print("     a. 点悬浮球 → 面板展开（约 420×600）")
    print("     b. 确认模型下拉里选中 mock-local（工具调用勾上）")
    print("     c. 输入：e2e ping")
    print("     d. 点 ↵ 或按 Enter")
    print("     e. 期望：出现用户气泡 + 含 MOCK_OPENAI_OK 的回复\n")
    print("5) shell / 命令策略 真机三路（mock 已支持 tool_call）：")
    print("     设置 › 命令策略：先确认三路判定页面能打开、能保存、能「试一下」")
    print("     对话（模型需勾选「支持工具调用」）：")
    print("       · SHELL 白名单     → 期望直接跑通，见 SHELL_ALLOW_OK")
    print("       · SHELL 确认卡     → 期望弹确认卡；批后见 SHELL_CONFIRM_CARD_OK")
    print("       · SHELL 硬阻断     → 期望拒绝且不执行 shutdown")
    print("     设置 › 命令策略 底部审计：三路应各有记录\n")
    print("6) 自动化断言：")
    print(f"     python tests/e2e/run_live_flow.py --probe-http   # 先测 mock 协议")
    print("     npm run test:ui                                 # DOM 级点击→发送")
    print("     npm run test:rust                               # 后端单测\n")
    print("UI 层「点击到发送」已由 vitest 覆盖（tests/ui/flow.orb-to-send.test.ts）。")
    print("真机部分负责验证：WebView2 渲染、真实 IPC、真实网络栈、run_command 双闸门。")
    return 0


def main() -> int:
    if "--probe-http" in sys.argv:
        return probe_http()
    return prepare_only()


if __name__ == "__main__":
    raise SystemExit(main())
