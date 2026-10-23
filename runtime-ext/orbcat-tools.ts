/**
 * orbcat 工具桥 extension —— 把 orbcat 的 34 个私有工具暴露给 pi。
 *
 * ## 为什么需要这个 extension
 *
 * pi 只有 8 个内置工具（read/bash/edit/write/grep/find/ls/powershell），
 * 而 orbcat 有 34 个 —— 其中一半是 Windows 深度集成（截屏、前台窗口、
 * 图像发送、表情包、life/pmem、记忆蒸馏、跨会话检索…），**在 TS 里根本拿不到**。
 *
 * 所以工具**留在 Rust**（一行代码不重写），pi 通过本地 HTTP 回调来调。
 *
 * ## 协议
 *
 * - `ORB_BRIDGE_PORT` 环境变量给出桥服务端口
 * - `POST http://127.0.0.1:<port>/tool`  body = `{"tool": name, "args": {...}}`
 * - 响应 = `{"ok": true, "result": {content:[...], details:{...}}}` 或 `{"ok": false, "error": "..."}`
 * - `ORB_TOOL_SPECS` 环境变量给出工具清单（JSON：name → {description, parameters}）
 *
 * ## 为什么走 HTTP 而不是 stdin
 *
 * pi 的 stdin/stdout **专供 RPC 协议**（`docs/rpc.md:62`）。往里塞别的协议会破坏
 * 帧边界 —— pi 的帧是严格 LF JSONL，不能塞多路复用逻辑。另开临时端口更干净。
 *
 * ## 权限闸门
 *
 * 真正的权限决策**在 Rust 侧**（`command_policy.rs` / `permission.rs`）。
 * 这里不重复实现 —— 工具调到 Rust，Rust 那边照常走网关/策略/审计链。
 *
 * 但 pi 层面还有一个闸门：extension 的 `tool_call` 事件可以阻断。
 * 我们**不在这里阻断** —— 因为 orbcat 的权限模型是「文件网关 + 命令策略 + 确认卡」，
 * 由 Rust 的 `authorize()` 统一裁决。在 TS 层再判一次会造出两套口径。
 */

interface ToolSpec {
	description: string;
	parameters: unknown;
}

const BRIDGE_PORT = process.env.ORB_BRIDGE_PORT || "";
const TOOL_SPECS: Record<string, ToolSpec> = (() => {
	try {
		return JSON.parse(process.env.ORB_TOOL_SPECS || "{}");
	} catch {
		return {};
	}
})();

/** 桥服务不可用时的错误文案（要说清是配置问题，不是模型问题）。 */
function bridgeUnavailable(): string {
	return (
		`orbcat 工具桥不可用（ORB_BRIDGE_PORT=${BRIDGE_PORT || "未设置"}）。` +
		`这是宿主配置问题，不是你的操作有误 —— 请重启 orbcat 后重试。`
	);
}

/** 调 Rust 侧的工具执行器。 */
async function callRust(tool: string, args: unknown): Promise<unknown> {
	if (!BRIDGE_PORT) throw new Error(bridgeUnavailable());

	const ctrl = new AbortController();
	// 工具可能很慢（截屏、跑长命令）—— 给个上限，别把 pi 挂死。
	const timer = setTimeout(() => ctrl.abort(), 300_000);

	try {
		const res = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/tool`, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ tool, args }),
			signal: ctrl.signal,
		});
		const text = await res.text();
		let j: any;
		try {
			j = JSON.parse(text);
		} catch {
			throw new Error(`桥返回的不是 JSON（HTTP ${res.status}）：${text.slice(0, 300)}`);
		}
		if (!j.ok) throw new Error(String(j.error ?? "未知错误"));
		return j.result;
	} catch (e: any) {
		if (e?.name === "AbortError") {
			throw new Error(`工具 ${tool} 超时（300s）`);
		}
		throw e;
	} finally {
		clearTimeout(timer);
	}
}

/**
 * pi 的工具结果形状：`{content: [{type:"text", text}], details: {}}`。
 * Rust 那边已经是这个形状（`ToolOutput`），直接透传。
 */
function toPiResult(result: any) {
	if (result && typeof result === "object" && Array.isArray(result.content)) {
		return {
			content: result.content,
			details: result.details ?? {},
		};
	}
	return {
		content: [{ type: "text", text: typeof result === "string" ? result : JSON.stringify(result) }],
		details: {},
	};
}

export default function (pi: any) {
	for (const [rawName, spec] of Object.entries(TOOL_SPECS)) {
		// ⚠️ **必须加 `orb_` 前缀** —— pi 内置工具里有 `read`/`write`/`edit`/`grep`，
		// 而 orbcat 的叫 `read_file`/`write_file`/`edit_file`/`grep_files`。
		// 名字不完全一样但语义重叠，不加前缀会让模型面对两套几乎一样的工具。
		// 前缀同时也是 Rust 侧识别「这是 orbcat 的工具」的标记（见 pi_runtime.rs）。
		const name = `orb_${rawName}`;
		pi.registerTool({
			name,
			label: name,
			description: spec.description,
			// pi 的参数 schema 走 TypeBox；但 Rust 那边给的是 JSON Schema。
			// 两者形状兼容（JSON Schema 的 object/properties/required 都是 TypeBox 的子集），
			// 所以直接透传 —— 这也是 pi 官方 example 里 `parameters` 的最低要求。
			parameters: spec.parameters as any,
			async execute(_toolCallId: string, params: any) {
				const result = await callRust(name, params);
				return toPiResult(result);
			},
		});
	}
}
