# float-agent 测试框架

目标：**从点击悬浮球到发送消息**都能被自动化验证。

## 分层

| 层 | 目录/命令 | 测什么 | 依赖 |
|---|---|---|---|
| L0 Rust 单测 | `npm run test:rust` | 记忆闭环、权限、工具、prompt 注入 | cargo |
| L1 UI 流程 | `npm run test:ui` | **orb 点击 → 面板 → 输入 → 发送 → 气泡** | vitest + happy-dom + mock invoke |
| L2 协议 mock | `python tests/e2e/mock_openai.py` | OpenAI 兼容接口（离线假模型） | Python 3 |
| L3 真机验收 | `npm run test:e2e:live` | WebView2 + 真 IPC + 真 HTTP | release exe 或 dev |

**日常回归以 L0+L1 为准**（秒级、无网、无 GUI）。L2/L3 用于发布前真机验收。

## 快速开始

```powershell
cd D:\workplace\悬浮小agent\float-agent
npm install
npm run test:ui          # 点击→发送黄金路径
npm run test:rust        # 后端
npm test                 # ui + rust
```

## L1：UI 测试怎么工作

```
tests/ui/mocks/tauri.ts   ← 有状态假后端（chat 写入会话，session_state 读回）
tests/ui/setup.ts         ← vi.mock @tauri-apps/api/*
tests/ui/flow.orb-to-send.test.ts
```

主用例 `flow.orb-to-send.test.ts`：

1. `bootApp()` 动态 `import src/main.ts`（模块顶层会 `boot()`）
2. 断言渲染出 `#orb`
3. `mousedown+mouseup` 点球 → 等 `#input` / `#btn-send`
4. 填入文字，点 `#btn-send`
5. 断言 `invoke("chat")` 参数、`.msg.user` / `.msg.assistant` 气泡
6. chat 失败 / 空输入 / 无模型 / 插话 `chat_steer` / 记忆审批 等边界

新增用例：复制 `tests/ui/flow.*.test.ts`，用 `helpers.ts` 的 `bootApp/openPanel/sendText/callsOf`。

## L2/L3：假模型真机链路

```powershell
# 终端 A
python tests/e2e/mock_openai.py 8787

# 终端 B
python tests/e2e/run_live_flow.py --probe-http
python tests/e2e/run_live_flow.py          # 打印手测清单
```

在 agent 设置里添加模型 `mock-local` → `http://127.0.0.1:8787/v1`，key 任意。
界面里发 `e2e ping`，应看到 `MOCK_OPENAI_OK`。

## 约定

- 临时文件/日志 → 仓库根 `../tmp/`
- UI 测试**不**读真实 `agent-data/`（mock 全内存）
- 真机验收使用 `float-agent/agent-data/`，注意 API key 不进仓库
- 改前端后先 `npm run test:ui` 再 `npm run build`
