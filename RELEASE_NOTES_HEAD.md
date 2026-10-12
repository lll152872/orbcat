# orbcat v1.0.0

悬浮球 agent —— Windows / macOS，单文件 agent。

## 下载

| 文件 | 平台 |
|---|---|
| `orbcat-windows-x64.zip` | Windows 10/11 x64 |
| `orbcat-windows-arm64.zip` | Windows 11 on ARM |
| `orbcat-macos-arm64.dmg` | macOS 11+ Apple Silicon |

> **未签名未公证**：Windows 会弹 SmartScreen（「更多信息」→「仍要运行」），
> macOS 会被 Gatekeeper 拦（右键 → 打开）。这是预期的，不是安全问题。

## 解压即用

orbcat 的 exe **不自包含前端** —— `orbcat.exe` 必须与同级的 `dist/` 目录
一起用。zip 里已经配好了，**整个文件夹一起拷走**，不要只把 exe 拖出来。

首次启动会读当前工作目录下的 `agent-data/`（自动创建）。
模型 API key 在设置里配，存在本机 `agent-data/models.json`。
