/**
 * orbcat system prompt 接管 extension。
 *
 * ## 为什么需要
 *
 * pi 自己的默认 system prompt 是「coding agent」那一套：
 * "You are an expert coding assistant operating inside pi, a coding agent harness."
 *
 * 而 orbcat 的定位是**用户的私人助手** —— 有自己的：
 * - 人格与语气（`SOUL.md`）
 * - 身份（`IDENTITY.md`）
 * - 对用户的了解（`USER.md`）
 * - 行为红线（`RULES.md`）
 * - 长期记忆（`memory/MEMORY.md`）
 * - 项目记忆 + 记忆使用规则
 *
 * **不接这条 → pi 内核下的助手不知道你是谁、也不记得任何事**，
 * 而且用户会以为「切内核把我的记忆弄丢了」。
 *
 * ## 怎么接
 *
 * `before_agent_start` 事件的 `systemPrompt` 是**当前**的 prompt，
 * 返回 `{ systemPrompt }` 就**完全替换**它（实测有效，见 tmp/PI-PROBE-RESULTS.md）。
 *
 * 我们向宿主要一份 —— 宿主的 `/prompt` 端点内部调的是
 * `agent::build_system_prompt_full()`，**与自研内核那条拼装路逐字同源**，
 * 所以两个内核看到的 prompt 完全一致。
 *
 * ## 为什么不是 `SYSTEM.md`
 *
 * pi 支持 `<agent-dir>/SYSTEM.md` 直接替换 prompt。但那是**静态文件** ——
 * 装在 `agent-data/pi/` 下，而 orbcat 的 prompt 是**动态**的
 * （技能命中、项目上下文、当前消息都会影响它）。走 HTTP 才能拿到实时的。
 */

const BRIDGE_PORT = process.env.ORB_BRIDGE_PORT || "";

/** 拿宿主的 system prompt。失败时返回 null（不替换，让 pi 用自己的默认）。 */
async function fetchPrompt(userText: string): Promise<string | null> {
	if (!BRIDGE_PORT) return null;
	try {
		const ctrl = new AbortController();
		const timer = setTimeout(() => ctrl.abort(), 20_000);
		const res = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/prompt`, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ message: userText }),
			signal: ctrl.signal,
		});
		clearTimeout(timer);
		const j: any = await res.json();
		if (!j.ok) return null;
		const p = typeof j.prompt === "string" ? j.prompt : null;
		// 空 prompt 不替换 —— pi 的默认 prompt 至少还有基本工具说明
		return p && p.trim().length > 0 ? p : null;
	} catch {
		return null;
	}
}

export default function (pi: any) {
	pi.on("before_agent_start", async (event: any) => {
		const userText = String(event?.prompt ?? "");
		const p = await fetchPrompt(userText);
		if (!p) {
			console.error("[orbcat-prompt] 拿不到宿主 prompt，沿用 pi 默认");
			return undefined;
		}
		console.error(`[orbcat-prompt] 已接管 system prompt (${p.length} 字)`);
		return { systemPrompt: p };
	});
}
