/**
 * orbcat 权限闸门 extension —— pi 层面唯一的拦截点。
 *
 * ## 为什么需要
 *
 * pi **没有任何内置权限机制**（`README.md:90`："Pi does not include a built-in
 * permission system for restricting filesystem, process, network, or credential access"）。
 * 官方唯一的说法是"自己写扩展"（`README.md:92-96` 给了 Gondolin / Docker / OpenShell 三条路）。
 *
 * orbcat 的权限模型是**一套**（`permission.rs` 的文件网关 + `command_policy.rs` 的
 * 命令策略 + 确认卡 + 审计链），实现在 Rust 侧。
 *
 * ## 这一层做什么、不做什么
 *
 * **做**：把 pi 的 `tool_call` 事件**转成宿主请求**，让 Rust 侧用现成的权限逻辑裁决。
 * **不做**：不自己判"这个路径能不能写"——那会造出两套口径，必然漂移。
 *
 * 裁决发生在 **Rust 工具层**（`tools::execute` → `authorize()`），也就是
 * orbcat 那 34 个工具真正执行之前。pi 内置的 `bash`/`write` 不走 orbcat 的网关，
 * 所以这里额外兜一层：内置工具的写操作也过一遍宿主裁决。
 *
 * ## 协议
 *
 * - `ORB_BRIDGE_PORT` —— 同一个桥端口
 * - `POST /gate` body = `{"tool": name, "args": {...}}` → `{"ok":true,"allowed":bool,"reason":"..."}`
 */

const BRIDGE_PORT = process.env.ORB_BRIDGE_PORT || "";

/** 问宿主：这个工具调用允许吗。 */
async function askHost(tool: string, args: unknown): Promise<{ allowed: boolean; reason: string }> {
	if (!BRIDGE_PORT) {
		// 桥不可用 → **保守放行内置只读工具，阻断其它**
		// 为什么不全阻断：桥不可用是宿主问题，用户不该被自己的工具全锁死。
		// 为什么不全放行：写操作不能冒这个险。
		return { allowed: !isWriteish(tool), reason: "orbcat 权限桥不可用" };
	}
	try {
		const res = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/gate`, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ tool, args }),
		});
		const j: any = await res.json();
		if (!j.ok) return { allowed: true, reason: String(j.error ?? "") };
		return { allowed: !!j.allowed, reason: String(j.reason ?? "") };
	} catch {
		return { allowed: !isWriteish(tool), reason: "orbcat 权限桥不可达" };
	}
}

/** 这个工具会不会写东西（桥不可用时的兜底判据）。 */
function isWriteish(tool: string): boolean {
	return ["write", "edit", "bash", "powershell"].includes(tool);
}

export default function (pi: any) {
	pi.on("tool_call", async (event: any) => {
		const tool = String(event?.toolName ?? "");
		const input = event?.input ?? {};

		// orbcat 私有工具（orb_ 前缀）走 Rust 的网关裁决，**这里不重复判** ——
		// 判了会跟 Rust 那套打架（两套口径必然漂移）。
		if (tool.startsWith("orb_")) {
			return undefined;
		}

		// pi 内置工具：过宿主裁决（用户能对"删文件""跑命令"点确认）。
		const verdict = await askHost(tool, input);
		if (!verdict.allowed) {
			return {
				block: true,
				reason: verdict.reason || `用户拒绝了 ${tool}`,
			};
		}
		return undefined;
	});
}
