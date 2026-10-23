/**
 * 最小可用的 **MCP stdio server** —— 专门用来测 pi 的 MCP 集成。
 *
 * ## 为什么自己写一个
 *
 * 测「pi 真的能连 MCP、真的能调到 MCP 工具」需要**确定**的 server：
 * - 不能依赖 npm 包（离线环境 / 版本漂移）
 * - 不能依赖用户本机配的 `1mcp` 网关（它可能没起、可能换了工具名）
 * - 必须能**断言**工具真的被调了（所以调用要留痕）
 *
 * 所以手写一个最小的：MCP 的 stdio 传输就是 **JSON-RPC 2.0 over stdin/stdout，
 * 一行一条**，实现 `initialize` / `tools/list` / `tools/call` 三个方法就够。
 *
 * ## 它提供的工具
 *
 * | 工具 | 作用 |
 * |---|---|
 * | `echo` | 原样返回 `text` 参数 —— 用来验参数传递 |
 * | `get_marker` | 返回固定串 `MCP-MARKER-42` —— 用来验"真的是这个 server 回的" |
 *
 * ## 留痕
 *
 * 每次 `tools/call` 都往 `MCP_TRACE_FILE`（环境变量）追加一行 JSON。
 * 测试据此断言"工具真被调了、参数真的是那个" —— 不看模型输出，看副作用。
 *
 * ## 协议要点（pi 的 client 说的）
 *
 * - `protocolVersion` 必须是 pi 支持的：`2025-11-25` / `2025-06-18` / `2025-03-26` / `2024-11-05`
 * - 握手：`initialize` → 回 capabilities → client 再发 `notifications/initialized`
 * - `tools/list` 回 `{tools: [{name, description, inputSchema}]}`
 * - `tools/call` 回 `{content: [{type:"text", text}], isError?}`
 */

const readline = require("node:readline");
const fs = require("node:fs");

const TRACE = process.env.MCP_TRACE_FILE || "";

/** 留痕：把一次调用写进 trace 文件（测试据此断言）。 */
function trace(kind, payload) {
  if (!TRACE) return;
  try {
    fs.appendFileSync(TRACE, JSON.stringify({ kind, payload, ts: Date.now() }) + "\n");
  } catch {
    /* 留痕失败不影响协议 */
  }
}

/** 回一条响应。 */
function reply(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n");
}

/** 回一条错误。 */
function replyErr(id, code, message) {
  process.stdout.write(
    JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } }) + "\n",
  );
}

const TOOLS = [
  {
    name: "echo",
    description: "原样返回传入的 text。用于验证 MCP 参数传递。",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string", description: "要回显的文本" } },
      required: ["text"],
    },
  },
  {
    name: "get_marker",
    description: "返回固定标记串 MCP-MARKER-42。用于验证调用真的落到了本 server。",
    inputSchema: { type: "object", properties: {} },
  },
];

const rl = readline.createInterface({ input: process.stdin, terminal: false });

rl.on("line", (line) => {
  const raw = line.trim();
  if (!raw) return;
  let msg;
  try {
    msg = JSON.parse(raw);
  } catch {
    return; // 不是 JSON 就忽略（stdout 不能有噪音，所以不打印）
  }

  const { id, method, params } = msg;

  // 通知（没有 id）—— 不用回
  if (method === "notifications/initialized") {
    trace("initialized", null);
    return;
  }

  switch (method) {
    case "initialize": {
      trace("initialize", params);
      reply(id, {
        // 用 pi 支持的最新版本回
        protocolVersion: params?.protocolVersion || "2025-11-25",
        capabilities: { tools: {} },
        serverInfo: { name: "orbcat-test-mcp", version: "1.0.0" },
      });
      return;
    }

    case "tools/list": {
      trace("tools/list", null);
      reply(id, { tools: TOOLS });
      return;
    }

    case "tools/call": {
      const name = params?.name;
      const args = params?.arguments || {};
      trace("tools/call", { name, args });

      if (name === "echo") {
        reply(id, {
          content: [{ type: "text", text: `ECHO:${args.text ?? ""}` }],
        });
        return;
      }
      if (name === "get_marker") {
        reply(id, {
          content: [{ type: "text", text: "MCP-MARKER-42" }],
        });
        return;
      }
      replyErr(id, -32602, `未知工具: ${name}`);
      return;
    }

    // resources / prompts 一律回空 —— pi 会问，但不要报错
    case "resources/list":
      reply(id, { resources: [] });
      return;
    case "resources/templates/list":
      reply(id, { resourceTemplates: [] });
      return;
    case "prompts/list":
      reply(id, { prompts: [] });
      return;

    case "ping":
      reply(id, {});
      return;

    default:
      // 未知方法：只在有 id（是请求）时回错误；通知不回
      if (id !== undefined && id !== null) {
        replyErr(id, -32601, `Method not found: ${method}`);
      }
  }
});

// 父进程关了 stdin 就退出（pi 停掉连接时）
rl.on("close", () => process.exit(0));
