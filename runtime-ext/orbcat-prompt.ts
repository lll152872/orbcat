/**
 * orbcat system prompt 接管 extension —— **合成模式**（pi 原生 + 我们的记忆）。
 *
 * ## 为什么改成「合成」而不是「整段替换」
 *
 * pi 的 system prompt 是**分段构造**的（上游 `buildSystemPromptSections`）：
 * `preamble`（默认是 coding agent 人格）+ `tools` + `rules` + `docs` +
 * `project_context` + `skills` + 各 extension 写进 `sections` 的段。
 *
 * 我们以前返回 `{ systemPrompt: p }` ⇒ 上游设 `forceSystemPrompt`
 * ⇒ **整段替换** ⇒ pi 原生那些段（工具说明 / 规则 / cwd / 项目上下文 / MCP 清单）**全丢**。
 *
 * 现在改成（与 pi 自己的 mcp extension 同法 —— 直接写同一个对象）：
 *
 * - `event.systemPromptOptions.customPrompt` = 我们的人格 / 红线 / 身份 / 用户
 *   ⇒ **替换**掉 pi 的 coding agent 人格 ⇒ **我们的身份占主导**，pi 那份退位；
 * - `event.systemPromptOptions.sections["orbcat_context"]` = 长期记忆 / 项目 /
 *   模式 / 技能 / **当前执行位置** ⇒ **追加**一段；
 * - pi 原生的 tools / rules / docs / project_context / MCP 清单 **全部保留**
 *   ⇒ 它们自然变成「工具用法 / 环境信息」的补充资料。
 *
 * ⚠️ **关键：不能返回 `{ systemPrompt }`** —— 返回了就会设 `forceSystemPrompt`
 * ⇒ 又变回整段替换 ⇒ 上面这一切白做。
 *
 * ⚠️ 段名必须匹配上游 `/^[a-z][a-z0-9_-]*$/`，否则上游直接 throw
 * （`Invalid system prompt section name`）⇒ 整轮 prompt 构建失败。
 */

const BRIDGE_PORT = process.env.ORB_BRIDGE_PORT || "";

/** 宿主给的两段。 */
interface PromptParts {
	/** 人格 / 红线 / 身份 / 用户 —— 进 `customPrompt` */
	persona: string;
	/** 长期记忆 / 项目 / 模式 / 技能 / 执行位置 —— 进 `sections` */
	context: string;
}

/** 向宿主要 prompt 的两份。 */
async function fetchParts(userText: string): Promise<PromptParts | null> {
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

		const persona = typeof j.persona === "string" ? j.persona : "";
		const context = typeof j.context === "string" ? j.context : "";
		if (!persona && !context) {
			// 旧宿主（只回 `prompt` 字段）⇒ 退回整段替换（老行为，不至于没 prompt 用）
			const p = typeof j.prompt === "string" ? j.prompt : "";
			return p.trim().length > 0 ? { persona: p, context: "" } : null;
		}
		return { persona, context };
	} catch {
		return null;
	}
}

export default function (pi: any) {
	pi.on("before_agent_start", async (event: any) => {
		const userText = String(event?.prompt ?? "");
		const parts = await fetchParts(userText);
		if (!parts) {
			console.error("[orbcat-prompt] 拿不到宿主 prompt，沿用 pi 默认");
			return undefined;
		}

		const opts = event?.systemPromptOptions;
		const canSection =
			!!opts && typeof opts === "object" && !!opts.sections && typeof opts.sections === "object";

		if (!canSection) {
			// 兜底：拿不到 `sections` ⇒ 退回整段替换（老行为）
			const full = parts.context ? `${parts.persona}\n\n---\n\n${parts.context}` : parts.persona;
			console.error(`[orbcat-prompt] 整段替换 system prompt (${full.length} 字)`);
			return { systemPrompt: full };
		}

		// ── 合成：人格进 customPrompt，其余进 sections ──
		if (parts.persona) opts.customPrompt = parts.persona;
		if (parts.context) opts.sections["orbcat_context"] = parts.context;

		// 读一次合成后的**完整** prompt（getter 会用当前 options 重建）⇒ 诊断用
		let full = "";
		try {
			full = String(event?.systemPrompt ?? "");
		} catch {
			full = "";
		}
		console.error(
			`[orbcat-prompt] 已合成 system prompt (${full.length} 字)：` +
				`人格 ${parts.persona.length}（customPrompt，替换掉 pi 的 coding agent 人格）+ ` +
				`上下文 ${parts.context.length}（sections）+ pi 其它段保留（extension 段/project_context/skills）`,
		);

		// ⚠️ **不要返回 `{ systemPrompt }`** —— 那会让上游设 forceSystemPrompt ⇒ 整段替换。
		return undefined;
	});
}
