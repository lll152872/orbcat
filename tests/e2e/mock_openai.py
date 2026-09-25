#!/usr/bin/env python3
"""本地 OpenAI 兼容 mock —— 真机 E2E / 手工验收时当「假模型」。

用法：
  python tests/e2e/mock_openai.py          # 默认 127.0.0.1:8787
  python tests/e2e/mock_openai.py 8787

在 orbcat 设置里加一个模型：
  id:   mock-local
  url:  http://127.0.0.1:8787/v1
  key:  mock-key
  支持 tool call / 图片：按需勾选

行为：
  - POST /v1/chat/completions（流式 SSE + 非流式）
  - 会话里尚无 tool 结果、且用户消息命中关键字 → 回 tool_call
  - 已有 tool 结果 → 回「最终正文」（便于 agent loop 第二轮收束）
  - GET /v1/models → 列表
"""

from __future__ import annotations

import json
import re
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def extract_last_user(messages: list) -> str:
    for m in reversed(messages or []):
        if m.get("role") == "user":
            c = m.get("content")
            if isinstance(c, str):
                return c
            if isinstance(c, list):
                return " ".join(
                    p.get("text", "") for p in c if isinstance(p, dict) and p.get("type") == "text"
                )
    return ""


def extract_tool_blobs(messages: list) -> list[str]:
    out = []
    for m in messages or []:
        if m.get("role") != "tool":
            continue
        c = m.get("content")
        if isinstance(c, str):
            out.append(c)
        elif isinstance(c, list):
            out.append(
                " ".join(
                    p.get("text", "") for p in c if isinstance(p, dict) and p.get("type") == "text"
                )
            )
        else:
            out.append(str(c) if c is not None else "")
    return out


def pick_tool_call(tools: list, last_user: str) -> tuple[str | None, dict]:
    """返回 (tool_name, args)；不需要工具时 (None, {})。"""
    if not tools:
        return None, {}
    if re.search(r"SHELL|run_command|命令策略|硬阻断|白名单|确认卡", last_user, re.I):
        low = last_user.lower()
        if re.search(r"硬阻断|hard.?block|shutdown|format", low):
            cmd = "shutdown /s /t 0"
        elif re.search(r"确认|confirm|卡", low):
            # Write-Host 不在 command_policy allow → 应弹确认卡
            cmd = "Write-Host 'SHELL_CONFIRM_CARD_OK'"
        else:
            # 白名单：git --version 在 allow 里
            cmd = "git --version"
        return "run_command", {"command": cmd}
    if re.search(r"USE_TOOL|调用工具|read_file", last_user, re.I):
        return "read_file", {
            "path": r"D:\workplace\悬浮小agent\orbcat\README.md"
        }
    return None, {}


def final_reply_text(last_user: str, tool_blobs: list[str]) -> str:
    if tool_blobs:
        blob = "\n".join(tool_blobs)[-800:]
        return (
            "MOCK_TOOL_DONE\n"
            f"你刚才说：{last_user[:120] or '（空）'}\n"
            f"工具结果摘录：\n{blob}\n"
            "（mock 第二轮正文，来自 tests/e2e/mock_openai.py）"
        )
    return (
        f"MOCK_OPENAI_OK\n你刚才说：{last_user[:200] or '（空）'}\n"
        "（此回复来自 tests/e2e/mock_openai.py，不访问外网）"
    )


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args) -> None:  # 安静一点
        sys.stderr.write("[mock-openai] " + (fmt % args) + "\n")

    def _json(self, code: int, obj: dict) -> None:
        raw = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:
        if self.path.rstrip("/").endswith("/models") or self.path.endswith("/v1/models"):
            self._json(
                200,
                {
                    "object": "list",
                    "data": [
                        {
                            "id": "mock-local",
                            "object": "model",
                            "owned_by": "orbcat-test",
                        }
                    ],
                },
            )
            return
        self._json(200, {"ok": True, "service": "orbcat-mock-openai"})

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0") or 0)
        body = self.rfile.read(length) if length else b"{}"
        try:
            req = json.loads(body.decode("utf-8") or "{}")
        except json.JSONDecodeError:
            self._json(400, {"error": {"message": "invalid json"}})
            return

        if "chat/completions" not in self.path:
            self._json(404, {"error": {"message": f"unknown path {self.path}"}})
            return

        messages = req.get("messages") or []
        tools = req.get("tools") or []
        last_user = extract_last_user(messages)
        stream = bool(req.get("stream"))
        tool_blobs = extract_tool_blobs(messages)

        # 第二轮（已有 tool 结果）→ 只出正文，避免无限 tool_call
        if tool_blobs:
            tool_name, tool_args = None, {}
        else:
            tool_name, tool_args = pick_tool_call(tools, last_user)

        if stream:
            self._stream_reply(last_user, tool_name, tool_args, tool_blobs)
            return

        if tool_name:
            payload = {
                "id": "chatcmpl-mock-tool",
                "object": "chat.completion",
                "model": req.get("model", "mock-local"),
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": None,
                            "tool_calls": [
                                {
                                    "id": "call_mock_1",
                                    "type": "function",
                                    "function": {
                                        "name": tool_name,
                                        "arguments": json.dumps(
                                            tool_args, ensure_ascii=False
                                        ),
                                    },
                                }
                            ],
                        },
                        "finish_reason": "tool_calls",
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18},
            }
        else:
            payload = {
                "id": "chatcmpl-mock-1",
                "object": "chat.completion",
                "model": req.get("model", "mock-local"),
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": final_reply_text(last_user, tool_blobs),
                        },
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 12, "completion_tokens": 24, "total_tokens": 36},
            }
        self._json(200, payload)

    def _stream_reply(
        self,
        last_user: str,
        tool_name: str | None,
        tool_args: dict,
        tool_blobs: list[str],
    ) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def sse(obj: dict) -> None:
            self.wfile.write(f"data: {json.dumps(obj, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush()

        model = "mock-local"
        base = {
            "id": "chatcmpl-mock-s",
            "object": "chat.completion.chunk",
            "model": model,
        }

        if tool_name:
            args_json = json.dumps(tool_args, ensure_ascii=False)
            # OpenAI 流式 tool_calls：先 id+name，再 arguments 分片
            sse(
                {
                    **base,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_mock_1",
                                        "type": "function",
                                        "function": {
                                            "name": tool_name,
                                            "arguments": "",
                                        },
                                    }
                                ],
                            },
                            "finish_reason": None,
                        }
                    ],
                }
            )
            sse(
                {
                    **base,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "function": {"arguments": args_json},
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ],
                }
            )
            sse(
                {
                    **base,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18},
                }
            )
        else:
            text = final_reply_text(last_user, tool_blobs)
            # 按较短切片模拟流式正文
            step = max(12, len(text) // 4)
            pieces = [text[i : i + step] for i in range(0, len(text), step)] or [""]
            for i, piece in enumerate(pieces):
                sse(
                    {
                        **base,
                        "choices": [
                            {
                                "index": 0,
                                "delta": (
                                    {"role": "assistant", "content": piece}
                                    if i == 0
                                    else {"content": piece}
                                ),
                                "finish_reason": None,
                            }
                        ],
                    }
                )
            sse(
                {
                    **base,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 24, "total_tokens": 36},
                }
            )
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8787
    host = "127.0.0.1"
    server = HTTPServer((host, port), Handler)
    print(f"[mock-openai] listening http://{host}:{port}/v1")
    print(f"[mock-openai] 设置模型 url = http://{host}:{port}/v1  id=mock-local  key=mock-key")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[mock-openai] bye")


if __name__ == "__main__":
    main()
