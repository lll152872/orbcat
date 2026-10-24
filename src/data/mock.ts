/**
 * Mock 数据源 —— **零 Tauri 依赖**。
 *
 * ## 用途
 *
 * 1. 纯 HTML 渲染件（`preview/`）：浏览器里双击就能看到每一屏，不装 Tauri。
 * 2. UI 测试：跑真实界面 + 假后端，不需要 Tauri runtime。
 * 3. 出视频 / 走查设计稿：状态随便挑，不用去后端造数据。
 *
 * ## 它是**有状态**的
 *
 * 不是死数据：批准一条记忆候选，它就从待批列表里消失、统计跟着加一；
 * 发一轮对话，会话里真的多出两条消息；插话真的排进队列。
 * 这样「交互链」在 mock 下也是真的能走通，不是只能看不能点。
 *
 * ## 状态注入
 *
 * - `delayMs` —— 每个命令都慢 N 毫秒 ⇒ 渲染「加载中」；
 * - `fail` —— 指定命令抛错 ⇒ 渲染「错误态」；
 * - 场景（`mockdata.ts`）—— 换一组数据 ⇒ 渲染「空态 / 多会话 / 权限卡」…
 */
import type {
  AgentRun,
  BundleInfo,
  BuiltinToolInfo,
  Candidate,
  CmdAuditRecord,
  CmdGrantsView,
  CmdPolicy,
  CmdVerdictView,
  ContextCapability,
  FgCtx,
  FileGrant,
  LifeStatus,
  McpServerCfg,
  McpStatus,
  MemStats,
  MemoryFiles,
  ModelEditView,
  ModelView,
  PendingPerm,
  PermRule,
  ProjectListView,
  ProjectMemoryView,
  ProjectRow,
  RecoveryState,
  RemoteModelInfo,
  SearchStatus,
  Session,
  SessionState,
  SkillInfo,
  SteerAck,
  UsageRecord,
} from "./types";
import type {
  BgDeskStatus,
  BootState,
  BootstrapReport,
  ChatApi,
  ImportModelsResult,
  CompactOutcome,
  DataSource,
  LifeApi,
  McpApi,
  MemoryApi,
  ModesView,
  ModelsApi,
  PermissionsApi,
  PiStatus,
  PruneOutcome,
  ProjectsApi,
  RecoveryApi,
  SearchApi,
  SessionsApi,
  SettingsApi,
  SettingsView,
  ExecShellStatus,
  SystemApi,
} from "./source";
import { defaultMockData, scenarioById, type MockData, type MockScenario } from "./mockdata";

/** 一次聊天（mock）：用户说 + 助手答 + 可选工具步骤 */
export interface MockChatTurn {
  answer: string;
  reasoning?: string;
  steps?: AgentRun["steps"];
}

/** mock 侧的「这一轮模型会说什么」脚本（按输入里的关键词命中） */
const CHAT_SCRIPT: { match: RegExp; turn: MockChatTurn }[] = [
  {
    match: /拖|点击|灵敏|阈值/,
    turn: {
      answer:
        "现在判定同时看**位移**和**时长**：位移 < 6px 且按住 < 120ms 一律算点击；位移 > 6px 或按住超过 120ms 才开始跟手。\n\n" +
        "这样快速点一下不会再被吃掉，而真要拖动时也不会粘滞。",
      reasoning: "用户报的是「点击被误判成拖动」，先看判定逻辑再改。",
      steps: [
        {
          kind: "tool_call",
          name: "read_file",
          detail: '{"path":"src/main.ts","range":"1310-1360"}',
          durationMs: 78,
          result: "function attachDrag(elm: HTMLElement) { /* … */ }",
        },
      ],
    },
  },
  {
    match: /权限|授权|审批/,
    turn: {
      answer:
        "这条申请是**命令级**的，判定结果是 `ask`（要你拍板），不是直接放行。\n\n" +
        "「记住这类命令」记的是 `rm::--recursive` 这个前缀指纹 —— 以后同类命令还会再问你一次。",
      reasoning: "先看命令归一化后的指纹，再决定该给哪一档建议。",
      steps: [
        {
          kind: "tool_call",
          name: "perm_check",
          detail: '{"command":"rm -rf ./target/release"}',
          durationMs: 31,
          result: "verdict=ask risk=high normalized=rm --recursive --force ./target/release",
        },
      ],
    },
  },
  {
    match: /记忆|审批/,
    turn: {
      answer: "现在有 **4 条**待批。批准后它们会进 `MEMORY.md`，驳回的话我会记住这次别再提。",
    },
  },
  {
    match: /发布|打包|出包/,
    turn: {
      answer:
        "出包走 `bash tools/build-release.sh --fast`（**必须在 Git Bash 里跑**，PATH 上的 bash 是 WSL 会炸）。\n\n" +
        "它做四件事：build → cargo release-fast → 拷 dist 到 exe 同目录 → 拷 runtime + 建快捷方式。",
    },
  },
  {
    match: /压缩|上下文/,
    turn: {
      answer: "上下文占用到 87% 了，压缩之后会省掉大概 6 万 token。压不压？",
    },
  },
];

/** 兜底回答（没命中任何脚本） */
const DEFAULT_TURN: MockChatTurn = {
  answer:
    "收到。我先看一眼相关的代码，确认一下现状再动手。\n\n" +
    "**当前判断**：问题出在拖动判定只看位移、没看按住时长 —— 快速点击时浏览器也会产生 1~2px 的抖动，" +
    "在 6px 阈值附近就会被误判成拖动，点按的 click 事件随之被取消，面板自然弹不出来。\n\n" +
    "改法是同时看位移与时长，两者都不满足才当点击。改完我跑一遍 UI 用例确认。",
  reasoning: "先定位 attachDrag 的判定逻辑，再决定改法。",
  steps: [
    {
      kind: "tool_call",
      name: "read_file",
      detail: '{"path":"src/main.ts","range":"1310-1360"}',
      durationMs: 64,
      result: "const DRAG_THRESHOLD = 6;",
    },
    {
      kind: "tool_call",
      name: "orb_run_command",
      detail: '{"cmd":"npx vitest run --project ui"}',
      durationMs: 8420,
      result: "Test Files 27 passed (27)\nTests 225 passed (225)",
    },
  ],
};

export class MockSource implements DataSource {
  readonly kind = "mock" as const;

  /** 当前世界（可被场景覆盖） */
  data: MockData;
  /** 当前场景 */
  scenario: MockScenario;
  /** 每个命令的额外延迟 */
  delayMs: number;
  /** 要失败的命令 */
  fail: string[];

  private steerSeq = 0;
  private sessionSeq = 0;
  private unknownWarned = new Set<string>();

  constructor(scenarioId = "default", opts?: { delayMs?: number; fail?: string[] }) {
    this.scenario = scenarioById(scenarioId);
    this.data = defaultMockData();
    this.scenario.cover(this.data);
    this.delayMs = opts?.delayMs ?? this.scenario.delayMs ?? 0;
    this.fail = opts?.fail ?? this.scenario.fail ?? [];
  }

  /** 换场景（预览页切下拉时用） */
  setScenario(id: string): void {
    this.scenario = scenarioById(id);
    this.data = defaultMockData();
    this.scenario.cover(this.data);
    this.delayMs = this.scenario.delayMs ?? 0;
    this.fail = this.scenario.fail ?? [];
  }

  // ---------------- 内部工具 ----------------

  private async settle<T>(cmd: string, value: T): Promise<T> {
    if (this.fail.includes(cmd)) {
      throw new Error(`[mock] 命令「${cmd}」按场景设置失败了（注入错误）`);
    }
    if (this.delayMs > 0) {
      await new Promise((r) => setTimeout(r, this.delayMs));
    }
    return value;
  }

  private pickTurn(input: string): MockChatTurn {
    for (const s of CHAT_SCRIPT) {
      if (s.match.test(input)) return s.turn;
    }
    return DEFAULT_TURN;
  }

  // ---------------- 通用调用 ----------------

  /** 通用调用（对外）：把 dispatch 的结果收窄成调用方要的形状 */
  async call<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    return (await this.dispatch(cmd, args)) as T;
  }

  /** 真正的分发（内部）：返回 unknown，由 call 收窄 */
  private async dispatch(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
    const a = (args ?? {}) as Record<string, never>;
    const d = this.data;

    switch (cmd) {
      // ---- 会话 ----
      case "session_state": {
        const list = d.list.map((s) => ({ ...s, current: s.id === d.session.id }));
        return this.settle<SessionState>(cmd, { session: d.session, list });
      }
      case "session_new": {
        const id = `s-new-${++this.sessionSeq}`;
        const mode = (a.mode as string | undefined) || d.modes.defaultMode || "standard";
        const s: Session = {
          id,
          title: "新对话",
          createdAt: Date.now(),
          updatedAt: Date.now(),
          messages: [],
          kind: "task",
          mode,
        };
        d.list = [d.list[0]!, ...d.list.slice(1), { ...toMeta(s), current: false }];
        d.session = s;
        return this.settle<Session>(cmd, s);
      }
      case "session_switch": {
        const id = a.id as string;
        const hit = d.list.find((s) => s.id === id);
        if (hit) {
          d.list = d.list.map((s) => ({ ...s, current: s.id === id }));
          d.session = {
            ...d.session,
            id: hit.id,
            title: hit.title,
            kind: hit.kind,
            mode: hit.mode,
            messages: hit.id === "s-main" ? d.session.messages : [],
          };
        }
        return this.settle<Session>(cmd, d.session);
      }
      case "session_fork": {
        const upto = a.upto as number;
        const s: Session = {
          ...d.session,
          id: `s-fork-${++this.sessionSeq}`,
          kind: "fork",
          forkedFrom: d.session.id,
          forkAt: upto,
          title: `${d.session.title}（从第 ${upto + 1} 条分叉）`,
          messages: d.session.messages.slice(0, upto),
          createdAt: Date.now(),
          updatedAt: Date.now(),
        };
        d.list = [...d.list, { ...toMeta(s), current: false }];
        return this.settle<Session>(cmd, s);
      }
      case "session_truncate": {
        const upto = a.upto as number;
        d.session = { ...d.session, messages: d.session.messages.slice(0, upto), updatedAt: Date.now() };
        d.list = d.list.map((s) => (s.id === d.session.id ? { ...s, count: upto } : s));
        return this.settle<Session>(cmd, d.session);
      }
      case "session_delete": {
        const id = a.id as string;
        d.list = d.list.filter((s) => s.id !== id);
        return this.settle<void>(cmd, undefined as never);
      }
      case "session_set_title": {
        const id = a.id as string;
        const title = a.title as string;
        d.list = d.list.map((s) => (s.id === id ? { ...s, title } : s));
        if (id === d.session.id) d.session = { ...d.session, title };
        return this.settle<void>(cmd, undefined as never);
      }
      case "session_compact":
        return this.settle<CompactOutcome>(cmd, { ok: true, freedTokens: 61_240 });

      // ---- 对话 ----
      case "chat":
      case "chat_pi": {
        const input = a.input as string;
        const turn = this.pickTurn(input);
        const now = Date.now();
        const usage = { prompt: 2_840 + input.length, completion: 260 + turn.answer.length, total: 0 };
        usage.total = usage.prompt + usage.completion;
        const userMsg = { role: "user" as const, text: input, at: now };
        const asstMsg = {
          role: "assistant" as const,
          text: turn.answer,
          steps: turn.steps ?? [],
          reasoning: turn.reasoning,
          usage,
          model: "MiniMax-M2.1",
          at: now + 1,
        };
        d.session = {
          ...d.session,
          messages: [...d.session.messages, userMsg, asstMsg],
          updatedAt: now,
        };
        d.list = d.list.map((s) => (s.id === d.session.id ? { ...s, count: d.session.messages.length } : s));
        return this.settle<AgentRun>(cmd, {
          answer: turn.answer,
          steps: turn.steps ?? [],
          iterations: 1,
          reasoning: turn.reasoning,
        });
      }
      case "chat_cancel":
        return this.settle<void>(cmd, undefined as never);
      case "chat_steer":
      case "chat_pi_steer": {
        const text = (cmd === "chat_steer" ? (a.input as string) : (a.message as string)) ?? "";
        if (!text.trim()) throw new Error("插话内容为空");
        // 图：自研内核与 pi 内核都收（data URL 字符串），mock 原样记下
        const imgs = (a.images as string[] | undefined) ?? [];
        const id = `st${++this.steerSeq}`;
        const now = Date.now();
        d.session = {
          ...d.session,
          messages: [
            ...d.session.messages,
            {
              role: "user" as const,
              text,
              images: imgs.length ? imgs : undefined,
              steerId: id,
              at: now,
            },
          ],
        };
        return this.settle<SteerAck>(cmd, { id, queued: 1 });
      }

      // ---- 模型 ----
      case "list_models":
        return this.settle<ModelView[]>(cmd, d.models);
      case "get_selected_model":
        return this.settle<{ id: string; url: string } | null>(cmd, d.selectedModel);
      case "set_selected_model": {
        const id = a.id as string;
        const url = (a.url as string | undefined) ?? d.models.find((m) => m.id === id)?.url ?? "";
        d.selectedModel = { id, url };
        return this.settle<void>(cmd, undefined as never);
      }
      case "model_group_rename":
        return this.settle<void>(cmd, undefined as never);
      case "model_group_names": {
        const names: Record<string, string> = {};
        for (const m of d.models) {
          if (!names[m.url]) names[m.url] = hostOf(m.url);
        }
        return this.settle<Record<string, string>>(cmd, names);
      }
      case "models_change_group_url": {
        const url = a.url as string;
        const newUrl = a.newUrl as string;
        const moved = d.models.filter((m) => m.url === url);
        const skipped: string[] = [];
        const seen = new Set(d.models.filter((m) => m.url !== url).map((m) => m.id));
        for (const m of moved) {
          if (seen.has(m.id)) {
            skipped.push(m.id);
            continue;
          }
          m.url = newUrl;
          seen.add(m.id);
        }
        return this.settle<{ moved: number; skipped: string[]; newUrl: string }>(cmd, {
          moved: moved.length - skipped.length,
          skipped,
          newUrl,
        });
      }
      case "models_add": {
        const m: ModelView = {
          id: a.id as string,
          vendor: "自定义",
          url: a.url as string,
          supportsToolCall: !!a.supports_tool_call,
          supportsImages: !!a.supports_images,
          maxInputTokens: (a.max_input_tokens as number | undefined) ?? null,
          maxOutputTokens: (a.max_output_tokens as number | undefined) ?? null,
          hasKey: !!a.api_key,
        };
        d.models = [...d.models, m];
        return this.settle<string>(cmd, "已添加");
      }
      case "models_edit":
        return this.settle<void>(cmd, undefined as never);
      case "models_remove": {
        const id = a.id as string;
        d.models = d.models.filter((m) => m.id !== id);
        return this.settle<void>(cmd, undefined as never);
      }
      case "models_remove_group":
        return this.settle<number>(cmd, 0);
      case "models_set_group_key":
        return this.settle<number>(cmd, 1);
      case "models_get_edit": {
        const id = a.id as string;
        const m = d.models.find((x) => x.id === id) ?? d.models[0]!;
        const view: ModelEditView = {
          id: m.id,
          vendor: m.vendor,
          url: m.url,
          supportsToolCall: m.supportsToolCall,
          supportsImages: m.supportsImages,
          headersText: "",
          hasKey: !!m.hasKey,
          keyPreview: m.hasKey ? "sk-****...9f2a" : "",
          maxInputTokens: m.maxInputTokens,
          maxOutputTokens: m.maxOutputTokens,
        };
        return this.settle<ModelEditView>(cmd, view);
      }
      case "models_fetch_remote":
        return this.settle<RemoteModelInfo[]>(cmd, [
          { id: "gpt-4.1", ownedBy: "openai", contextLength: 1048576, maxOutputLength: 32768, supportsToolCall: true, supportsImages: true },
          { id: "gpt-4.1-mini", ownedBy: "openai", contextLength: 1048576, maxOutputLength: 32768, supportsToolCall: true, supportsImages: true },
          { id: "o4-mini", ownedBy: "openai", contextLength: 200000, maxOutputLength: 100000, supportsToolCall: true, supportsImages: false },
        ]);
      case "models_parse_spec":
        return this.settle<RemoteModelInfo[]>(cmd, []);
      case "models_import_remote": {
        const ids = (a.ids as string[] | undefined) ?? [];
        const added: string[] = [];
        const skipped: string[] = [];
        for (const id of ids) {
          if (d.models.some((m) => m.id === id)) {
            skipped.push(id);
            continue;
          }
          d.models = [
            ...d.models,
            {
              id,
              vendor: "远端导入",
              url: (a.url as string | undefined) ?? "https://api.openai.com/v1",
              supportsToolCall: !!a.supports_tool_call,
              supportsImages: !!a.supports_images,
              hasKey: !!a.api_key,
            },
          ];
          added.push(id);
        }
        return this.settle<ImportModelsResult>(cmd, { added, skipped });
      }
      case "test_model":
        return this.settle<string>(cmd, "连通性 OK（mock）：200 OK，延迟 412ms");
      case "models_window_open":
      case "models_window_close":
        return this.settle<void>(cmd, undefined as never);

      // ---- 记忆 ----
      case "mem_pending":
        return this.settle<Candidate[]>(cmd, d.pending);
      case "mem_files":
        return this.settle<MemoryFiles>(cmd, d.memFiles);
      case "mem_approve": {
        const id = a.id as string;
        d.pending = d.pending.filter((c) => c.id !== id);
        d.memStats = { ...d.memStats, pending: d.pending.length, handled: d.memStats.handled + 1 };
        return this.settle<void>(cmd, undefined as never);
      }
      case "mem_approve_all": {
        const n = d.pending.length;
        d.pending = [];
        d.memStats = { ...d.memStats, pending: 0, handled: d.memStats.handled + n };
        return this.settle<number>(cmd, n);
      }
      case "mem_reject": {
        const id = a.id as string;
        d.pending = d.pending.filter((c) => c.id !== id);
        d.memStats = { ...d.memStats, pending: d.pending.length, handled: d.memStats.handled + 1 };
        return this.settle<void>(cmd, undefined as never);
      }
      case "mem_rewrite": {
        const id = a.id as string;
        const content = a.content as string;
        d.pending = d.pending.map((c) => (c.id === id ? { ...c, content } : c));
        return this.settle<void>(cmd, undefined as never);
      }
      case "mem_stats":
        return this.settle<MemStats>(cmd, d.memStats);
      case "mem_projects":
        return this.settle<ProjectMemoryView[]>(cmd, d.projectMemory);
      case "mem_project_set_source":
        return this.settle<void>(cmd, undefined as never);
      case "mem_project_archive":
        return this.settle<string>(cmd, "已归档");
      case "mem_propose": {
        const c: Candidate = {
          id: `cand-${900 + d.pending.length}`,
          content: a.content as string,
          source: "conversation:mock",
          created_at: new Date().toISOString(),
          turn_ref: null,
        };
        d.pending = [...d.pending, c];
        d.memStats = { ...d.memStats, pending: d.pending.length };
        return this.settle<Candidate>(cmd, c);
      }
      case "mem_read":
        return this.settle<string>(cmd, d.memFiles.memory);

      // ---- 权限 ----
      case "perm_requests_list":
        return this.settle<PendingPerm[]>(cmd, d.permRequests);
      case "perm_request_decide": {
        const id = String(a.id ?? "");
        const decision = String(a.decision ?? "").trim().toLowerCase();
        // ⚠️ **与后端同形**（`perm_request.rs` 的 `Decision::parse`）：
        //   未知决定要**报错**，不能默默接受 ⇒ 前端的错误路径能被测到。
        const ok = ["approve", "yes", "批准", "narrow", "收窄", "deny", "no", "拒绝",
                    "prefix", "approve-prefix", "记住命令名", "full", "approve-full",
                    "记住完整命令", "always", "总是允许"];
        if (!ok.includes(decision)) {
          throw new Error(`未知的决定：${decision}（只能是 approve / narrow / prefix / full / always / deny）`);
        }
        // ⚠️ **pi 卡（`pp*`）只接受 approve / deny** ——
        //   pi 的确认通道只回一个布尔（`confirmed`），
        //   其余决定在 pi 侧一律按**拒绝**处理（后端 `pi_runtime.rs` 的钩子）。
        //   ⇒ mock 这里也拒，让「误发 always」的路径在前端就能测出来。
        const target = d.permRequests.find((p) => p.id === id);
        if (target?.kind === "pi" && decision !== "approve" && decision !== "deny" &&
            decision !== "yes" && decision !== "no" && decision !== "批准" && decision !== "拒绝") {
          throw new Error("pi 确认只支持允许 / 拒绝");
        }
        d.permRequests = d.permRequests.filter((p) => p.id !== id);
        return this.settle<void>(cmd, undefined as never);
      }
      case "perm_rules":
        return this.settle<PermRule[]>(cmd, d.permRules);
      case "perm_add_rule": {
        d.permRules = [
          ...d.permRules,
          { prefix: a.prefix as string, access: (a.access as PermRule["access"]) ?? "read", label: (a.label as string) ?? "" },
        ];
        return this.settle<void>(cmd, undefined as never);
      }
      case "perm_remove_rule": {
        const prefix = a.prefix as string;
        d.permRules = d.permRules.filter((r) => r.prefix !== prefix);
        return this.settle<void>(cmd, undefined as never);
      }
      case "perm_check":
        return this.settle<string>(cmd, "read");
      case "perm_cmd_policy":
        return this.settle<CmdPolicy>(cmd, d.cmdPolicy);
      case "perm_cmd_policy_set": {
        d.cmdPolicy = { allow: [...(a.allow as string[])], hard_block: [...(a.hard_block as string[])] };
        return this.settle<CmdPolicy>(cmd, d.cmdPolicy);
      }
      case "perm_cmd_policy_path":
        return this.settle<string>(cmd, "D:\\workplace\\悬浮小agent\\agent-data\\command-policy.json");
      case "perm_cmd_grants_list":
        return this.settle<CmdGrantsView[]>(cmd, d.cmdGrants);
      case "perm_cmd_grants_clear": {
        const n = d.cmdGrants.length;
        d.cmdGrants = [];
        return this.settle<number>(cmd, n);
      }
      case "perm_cmd_grant_revoke": {
        const fp = a.fingerprint as string;
        const n = d.cmdGrants.length;
        d.cmdGrants = d.cmdGrants.filter((g) => g.fingerprint !== fp);
        return this.settle<number>(cmd, n);
      }
      case "perm_cmd_audit_tail":
        return this.settle<CmdAuditRecord[]>(cmd, d.cmdAudit);
      case "perm_cmd_test": {
        const v: CmdVerdictView = {
          verdict: "ask",
          reason: "删除类命令需要你拍板",
          risk: "high",
          normalized: "rm --recursive --force ./target/release",
        };
        return this.settle<CmdVerdictView>(cmd, v);
      }
      case "perm_grants_list":
        return this.settle<unknown[]>(cmd, []);
      // 永久文件授权（「永远允许这个路径」）—— 落 file_grants.json
      case "file_grants_list":
        return this.settle<FileGrant[]>(cmd, [...d.fileGrants]);
      case "file_grants_revoke": {
        const prefix = String(a.prefix ?? "");
        const before = d.fileGrants.length;
        d.fileGrants = d.fileGrants.filter((g) => g.prefix !== prefix);
        return this.settle<number>(cmd, before - d.fileGrants.length);
      }
      case "perm_grant_revoke":
      case "perm_revoke_label":
        return this.settle<number>(cmd, 0);
      case "perm_set_rules": {
        d.permRules = [...(a.rules as PermRule[])];
        return this.settle<void>(cmd, undefined as never);
      }
      case "perm_rules_path":
        return this.settle<string>(cmd, "D:\\workplace\\悬浮小agent\\agent-data\\permissions.json");

      // ---- 项目 ----
      case "pmem_list":
        return this.settle<ProjectRow[]>(cmd, d.pmem);
      case "pmem_create": {
        const p: ProjectRow = {
          id: d.pmem.length + 1,
          name: a.name as string,
          rootPath: (a.rootPath as string | undefined) ?? `D:\\workplace\\${a.name as string}`,
          createdAt: new Date().toISOString(),
        };
        d.pmem = [...d.pmem, p];
        return this.settle<ProjectRow>(cmd, p);
      }
      case "pmem_rename": {
        const id = a.id as number;
        d.pmem = d.pmem.map((p) => (p.id === id ? { ...p, name: a.name as string } : p));
        return this.settle<void>(cmd, undefined as never);
      }
      case "pmem_delete": {
        const id = a.id as number;
        d.pmem = d.pmem.filter((p) => p.id !== id);
        return this.settle<void>(cmd, undefined as never);
      }
      case "pmem_memory_read":
        return this.settle<string>(
          cmd,
          `# ${a.name as string}\n\n- 项目根：${d.pmem.find((p) => p.name === a.name)?.rootPath ?? ""}\n- 约定：改前端前先读设计稿节点色值\n`,
        );
      case "pmem_memory_write":
        return this.settle<string>(cmd, "已保存");
      case "pmem_clear_memory":
        return this.settle<void>(cmd, undefined as never);
      case "pmem_get_active":
        return this.settle<ProjectRow | null>(cmd, d.pmem[0] ?? null);
      case "pmem_set_active":
        return this.settle<void>(cmd, undefined as never);
      case "pmem_bind_session":
        return this.settle<void>(cmd, undefined as never);
      case "pmem_remember":
        return this.settle<string>(cmd, "已记下");
      case "pmem_session_map":
        return this.settle<Record<string, string | null>>(cmd, { "s-main": "悬浮小agent" });
      case "project_list":
        return this.settle<ProjectListView>(cmd, d.projects);
      case "project_activate":
        return this.settle<void>(cmd, undefined as never);
      case "project_save":
        return this.settle<void>(cmd, undefined as never);
      case "project_read":
        return this.settle<string>(cmd, "# 项目说明\n\n这是 mock 数据。\n");
      case "project_delete":
        return this.settle<void>(cmd, undefined as never);

      // ---- 恢复区 ----
      case "recovery_state":
        return this.settle<RecoveryState>(cmd, d.recovery);
      case "recovery_restore_backup":
        return this.settle<string>(cmd, "已还原");
      case "recovery_delete_backup":
        return this.settle<string>(cmd, "已永久删除");
      case "recovery_restore_trash":
        return this.settle<string>(cmd, "已还原");
      case "recovery_delete_trash":
        return this.settle<string>(cmd, "已永久删除");
      case "recovery_clear_trash":
        return this.settle<string>(cmd, `已清空 ${d.recovery.trash.length} 项`);
      case "recovery_diagnostic":
        return this.settle<string>(cmd, "恢复区：3 份备份 / 3 条归档 / 1 条孤儿索引（.trash 里有记录但文件已不在）");
      case "recovery_prune":
        return this.settle<PruneOutcome>(cmd, { backupsRemoved: 2, trashRemoved: 5, bytesFreed: 1_204_880 });

      // ---- 设置 ----
      case "get_settings":
        return this.settle<SettingsView>(cmd, d.settings);
      case "set_exec_trust":
        return this.settle<string>(cmd, "已设为「始终信任」");
      /**
       * 切 shell（`pwsh` / `gitbash`）—— **真落盘**（写进 d.settings）。
       *
       * ⚠️ 为什么要「真落盘」而不是像 set_exec_trust 那样返回一句文案：
       *    用户要求「选项要真能选、真能存、界面上真显示」。
       *    存了之后 `get_settings` 必须能读回来 ⇒ 刷新页面/重开设置仍是同一个值。
       */
      case "set_exec_shell": {
        const shell = String(a.shell ?? "pwsh").trim().toLowerCase();
        if (shell !== "pwsh" && shell !== "gitbash") {
          throw new Error(`未知执行位置「${shell}」（可用：pwsh / gitbash）`);
        }
        d.settings = { ...d.settings, execShell: shell };
        return this.settle<string>(cmd, shell);
      }
      /**
       * 查当前 shell + Git Bash 可用性。
       *
       * ⚠️ 发布包**不带 git** ⇒ 真实场景里没装 Git for Windows 时是 false。
       *    mock 默认给 true（演示正常态）；要演示「没装」的失败态可覆盖。
       */
      case "exec_shell_status":
        return this.settle<ExecShellStatus>(cmd, {
          execShell: d.settings.execShell ?? "pwsh",
          label: d.settings.execShell === "gitbash" ? "本机 Git Bash" : "本机 PowerShell",
          gitBashAvailable: true,
          gitBashPath: "C:\\Program Files\\Git\\bin\\bash.exe",
        });
      case "set_builtin_tools":
        d.settings = { ...d.settings, builtinToolsEnabled: !!a.enable };
        return this.settle<void>(cmd, undefined as never);
      case "set_blur_collapse":
        d.settings = { ...d.settings, blurCollapse: !!a.enable };
        return this.settle<void>(cmd, undefined as never);
      case "set_project_dir_autoread":
        d.settings = { ...d.settings, projectDirAutoread: !!a.enable };
        return this.settle<void>(cmd, undefined as never);
      case "set_window_mode":
        return this.settle<void>(cmd, undefined as never);
      case "autostart_status":
        return this.settle<boolean>(cmd, false);
      case "autostart_set":
        return this.settle<void>(cmd, undefined as never);
      case "bg_desk_status":
        return this.settle<BgDeskStatus>(cmd, d.bgDesk);
      case "bg_desk_set":
        d.bgDesk = { ...d.bgDesk, enabled: !!a.enabled };
        return this.settle<boolean>(cmd, !!a.enabled);
      case "modes_get":
        return this.settle<ModesView>(cmd, d.modes);

      // ---- life ----
      case "life_status":
        return this.settle<LifeStatus>(cmd, d.life);
      case "life_set_enabled": {
        const id = a.id as string;
        d.life = {
          ...d.life,
          sources: d.life.sources.map((s) => (s.id === id ? { ...s, enabled: !!a.enabled } : s)),
        };
        return this.settle<void>(cmd, undefined as never);
      }
      case "life_refresh":
      case "life_config_path":
        return this.settle<void>(cmd, undefined as never);

      // ---- MCP ----
      case "mcp_status":
        return this.settle<McpStatus>(cmd, d.mcp);
      case "mcp_servers_list":
        return this.settle<McpServerCfg[]>(cmd, d.mcpServers);
      case "mcp_servers_save": {
        d.mcpServers = [...(a.servers as McpServerCfg[])];
        return this.settle<void>(cmd, undefined as never);
      }
      case "mcp_set_all_groups": {
        const all = !!a.all;
        d.mcp = { ...d.mcp, groups: d.mcp.groups.map((g) => ({ ...g, active: all })) };
        return this.settle<void>(cmd, undefined as never);
      }
      case "mcp_set_group_enabled": {
        const name = a.name as string;
        const enabled = !!a.enabled;
        d.mcp = {
          ...d.mcp,
          groups: d.mcp.groups.map((g) => (g.name === name ? { ...g, active: enabled } : g)),
        };
        return this.settle<void>(cmd, undefined as never);
      }
      case "mcp_group_settings":
        return this.settle<Record<string, boolean>>(cmd, { __all__: true });
      case "mcp_grants_list":
        return this.settle<unknown[]>(cmd, []);
      case "mcp_grants_clear":
        return this.settle<number>(cmd, 0);
      case "mcp_grant_revoke":
        return this.settle<number>(cmd, 0);
      case "mcp_refresh":
        return this.settle<void>(cmd, undefined as never);

      // ---- 搜索 ----
      case "search_status":
        return this.settle<SearchStatus | null>(cmd, d.search.provider ? d.search : null);
      case "search_test":
        return this.settle<string>(cmd, "搜索连通正常 · Tavily · 约 0 条结果行");
      case "search_set": {
        d.search = { ...d.search, provider: a.provider as string, hasKey: !!a.api_key, ready: !!a.api_key };
        return this.settle<SearchStatus>(cmd, d.search);
      }

      // ---- 系统 ----
      case "boot_state":
        return this.settle<BootState>(cmd, d.boot);
      case "bootstrap_data":
        return this.settle<BootstrapReport>(cmd, {
          dataDir: d.boot.dataDir,
          createdDirs: d.boot.dataDirExists ? [] : ["sessions", "memory", "projects"],
          createdFiles: d.boot.dataDirExists ? [] : ["settings.json", "models.json"],
          skipped: d.boot.dataDirExists ? ["settings.json", "sessions", "memory"] : [],
          hasModels: d.models.length > 0,
          seed: "orbcat-skeleton-20261023",
        });
      case "diag_log":
        return this.settle<void>(cmd, undefined as never);
      case "pi_status":
        return this.settle<PiStatus>(cmd, d.pi);
      case "context_capability":
        return this.settle<ContextCapability>(cmd, d.contextCapability);
      case "request_context_permission":
        return this.settle<ContextCapability>(cmd, d.contextCapability);
      case "foreground_history":
        return this.settle<FgCtx[]>(cmd, d.fg);
      case "foreground_is_capture_tool":
        return this.settle<boolean>(cmd, true);
      case "orb_ui_state":
        return this.settle<{ sleeping: boolean }>(cmd, { sleeping: false });
      case "orb_ui_action":
        return this.settle<void>(cmd, undefined as never);
      case "take_open_panel_request":
        return this.settle<boolean>(cmd, false);
      case "confirm_panel_opened":
        return this.settle<void>(cmd, undefined as never);
      case "image_thumb":
        return this.settle<string>(cmd, "");
      case "capture_screen":
        return this.settle<string>(cmd, "");
      case "exe_path":
        return this.settle<string>(cmd, "D:\\orbcat\\orbcat.exe");
      case "app_quit":
        return this.settle<void>(cmd, undefined as never);
      case "usage_report":
        return this.settle<UsageRecord[]>(cmd, d.usage);
      case "skills_list":
        return this.settle<SkillInfo[]>(cmd, d.skills);
      case "builtin_tools":
        return this.settle<BuiltinToolInfo[]>(cmd, d.builtinTools);
      case "open_accessibility_settings":
        return this.settle<void>(cmd, undefined as never);
      case "distill_session_open":
        return this.settle<Session>(cmd, d.session);
      case "bundles":
        return this.settle<BundleInfo[]>(cmd, d.projects.bundles);

      default:
        if (!this.unknownWarned.has(cmd)) {
          this.unknownWarned.add(cmd);
          console.warn(`[mock] 未实现的命令：${cmd}`);
        }
        return this.settle(cmd, undefined);
    }
  }

  // ---------------- 领域方法 ----------------

  readonly sessions: SessionsApi = {
    state: () => this.call<SessionState>("session_state"),
    create: (args) => this.call<Session>("session_new", args),
    switch: (args) => this.call<Session>("session_switch", args),
    fork: (args) => this.call<Session>("session_fork", args),
    truncate: (args) => this.call<Session>("session_truncate", args),
    delete: (args) => this.call<void>("session_delete", args),
    setTitle: (args) => this.call<void>("session_set_title", args),
    compact: (args) => this.call<CompactOutcome>("session_compact", args),
  };

  readonly chat: ChatApi = {
    send: (args) => this.call<AgentRun>("chat", args),
    sendPi: (args) => this.call<AgentRun>("chat_pi", args),
    cancel: (args) => this.call<void>("chat_cancel", args),
    steer: (args) => this.call<SteerAck>("chat_steer", args),
    steerPi: (args) => this.call<SteerAck>("chat_pi_steer", args),
  };

  readonly models: ModelsApi = {
    list: () => this.call<ModelView[]>("list_models"),
    selected: () => this.call<{ id: string; url?: string } | null>("get_selected_model"),
    select: (args) => this.call<void>("set_selected_model", args),
    groupRename: (args) => this.call<void>("model_group_rename", args),
    groupNames: () => this.call<Record<string, string>>("model_group_names"),
    changeGroupUrl: (args) =>
      this.call<{ moved: number; skipped: string[]; newUrl: string }>("models_change_group_url", args),
    add: (args) => this.call<string>("models_add", args),
    edit: (args) => this.call<void>("models_edit", args),
    remove: (args) => this.call<void>("models_remove", args),
    removeGroup: (args) => this.call<number>("models_remove_group", args),
    setGroupKey: (args) => this.call<number>("models_set_group_key", args),
    getEdit: (args) => this.call<ModelEditView>("models_get_edit", args),
    fetchRemote: (args) => this.call<RemoteModelInfo[]>("models_fetch_remote", args),
    parseSpec: (args) => this.call<RemoteModelInfo[]>("models_parse_spec", args),
    importRemote: (args) => this.call<ImportModelsResult>("models_import_remote", args),
    test: (args) => this.call<string>("test_model", args),
    windowOpen: () => this.call<void>("models_window_open"),
    windowClose: () => this.call<void>("models_window_close"),
  };

  readonly memory: MemoryApi = {
    pending: () => this.call<Candidate[]>("mem_pending"),
    files: () => this.call<MemoryFiles>("mem_files"),
    approve: (args) => this.call<void>("mem_approve", args),
    approveAll: () => this.call<number>("mem_approve_all"),
    reject: (args) => this.call<void>("mem_reject", args),
    rewrite: (args) => this.call<void>("mem_rewrite", args),
    stats: () => this.call<MemStats>("mem_stats"),
    projects: () => this.call<ProjectMemoryView[]>("mem_projects"),
    projectSetSource: (args) => this.call<void>("mem_project_set_source", args),
    projectArchive: (args) => this.call<string>("mem_project_archive", args),
    propose: (args) => this.call<Candidate>("mem_propose", args),
    read: () => this.call<string>("mem_read"),
  };

  readonly permissions: PermissionsApi = {
    requests: () => this.call<PendingPerm[]>("perm_requests_list"),
    decide: (args) => this.call<void>("perm_request_decide", args),
    rules: () => this.call<PermRule[]>("perm_rules"),
    addRule: (args) => this.call<void>("perm_add_rule", args),
    removeRule: (args) => this.call<void>("perm_remove_rule", args),
    check: (args) => this.call<string>("perm_check", args),
    cmdPolicy: () => this.call<CmdPolicy>("perm_cmd_policy"),
    cmdPolicySet: (args) => this.call<CmdPolicy>("perm_cmd_policy_set", args),
    cmdPolicyPath: () => this.call<string>("perm_cmd_policy_path"),
    cmdGrants: () => this.call<CmdGrantsView[]>("perm_cmd_grants_list"),
    cmdGrantsClear: () => this.call<number>("perm_cmd_grants_clear"),
    cmdGrantRevoke: (args) => this.call<number>("perm_cmd_grant_revoke", args),
    cmdAuditTail: (args) => this.call<CmdAuditRecord[]>("perm_cmd_audit_tail", args),
    cmdTest: (args) => this.call<CmdVerdictView>("perm_cmd_test", args),
    grantsList: () => this.call<unknown[]>("perm_grants_list"),
    grantRevoke: (args) => this.call<number>("perm_grant_revoke", args),
    // 永久文件授权（「永远允许这个路径」）—— 落 file_grants.json
    fileGrantsList: () => this.call<FileGrant[]>("file_grants_list"),
    fileGrantsRevoke: (args) => this.call<number>("file_grants_revoke", args),
    revokeLabel: (args) => this.call<number>("perm_revoke_label", args),
    setRules: (args) => this.call<void>("perm_set_rules", args),
    rulesPath: () => this.call<string>("perm_rules_path"),
  };

  readonly projects: ProjectsApi = {
    pmemList: () => this.call<ProjectRow[]>("pmem_list"),
    pmemCreate: (args) => this.call<ProjectRow>("pmem_create", args),
    pmemRename: (args) => this.call<void>("pmem_rename", args),
    pmemDelete: (args) => this.call<void>("pmem_delete", args),
    pmemRead: (args) => this.call<string>("pmem_memory_read", args),
    pmemWrite: (args) => this.call<string>("pmem_memory_write", args),
    pmemClear: (args) => this.call<void>("pmem_clear_memory", args),
    pmemGetActive: () => this.call<ProjectRow | null>("pmem_get_active"),
    pmemSetActive: (args) => this.call<void>("pmem_set_active", args),
    pmemBindSession: (args) => this.call<void>("pmem_bind_session", args),
    pmemRemember: (args) => this.call<string>("pmem_remember", args),
    pmemSessionMap: () => this.call<Record<string, string | null>>("pmem_session_map"),
    list: () => this.call<ProjectListView>("project_list"),
    activate: (args) => this.call<void>("project_activate", args),
    save: (args) => this.call<void>("project_save", args),
    read: (args) => this.call<string>("project_read", args),
    delete: (args) => this.call<void>("project_delete", args),
  };

  readonly recovery: RecoveryApi = {
    state: () => this.call<RecoveryState>("recovery_state"),
    restoreBackup: (args) => this.call<string>("recovery_restore_backup", args),
    deleteBackup: (args) => this.call<string>("recovery_delete_backup", args),
    restoreTrash: (args) => this.call<string>("recovery_restore_trash", args),
    deleteTrash: (args) => this.call<string>("recovery_delete_trash", args),
    clearTrash: () => this.call<string>("recovery_clear_trash"),
    diagnostic: () => this.call<string>("recovery_diagnostic"),
    prune: (args) => this.call<PruneOutcome>("recovery_prune", args),
  };

  readonly settings: SettingsApi = {
    get: () => this.call<SettingsView>("get_settings"),
    setExecTrust: (args) => this.call<string>("set_exec_trust", args),
    setExecShell: (args) => this.call<string>("set_exec_shell", args),
    execShellStatus: () => this.call<ExecShellStatus>("exec_shell_status"),
    setBuiltinTools: (args) => this.call<void>("set_builtin_tools", args),
    setBlurCollapse: (args) => this.call<void>("set_blur_collapse", args),
    setProjectDirAutoread: (args) => this.call<void>("set_project_dir_autoread", args),
    setWindowMode: (args) => this.call<void>("set_window_mode", args),
    autostartStatus: () => this.call<boolean>("autostart_status"),
    autostartSet: (args) => this.call<void>("autostart_set", args),
    bgDeskStatus: () => this.call<BgDeskStatus>("bg_desk_status"),
    bgDeskSet: (args) => this.call<boolean>("bg_desk_set", args),
    modesGet: (args) => this.call<ModesView>("modes_get", args),
  };

  readonly life: LifeApi = {
    status: () => this.call<LifeStatus>("life_status"),
    setEnabled: (args) => this.call<void>("life_set_enabled", args),
    refresh: () => this.call<void>("life_refresh"),
    configPath: () => this.call<void>("life_config_path"),
  };

  readonly mcp: McpApi = {
    status: () => this.call<McpStatus>("mcp_status"),
    serversList: () => this.call<McpServerCfg[]>("mcp_servers_list"),
    serversSave: (args) => this.call<void>("mcp_servers_save", args),
    setAllGroups: (args) => this.call<void>("mcp_set_all_groups", args),
    setGroupEnabled: (args) => this.call<void>("mcp_set_group_enabled", args),
    groupSettings: () => this.call<Record<string, boolean>>("mcp_group_settings"),
    grantsList: () => this.call<string[]>("mcp_grants_list"),
    grantsClear: () => this.call<number>("mcp_grants_clear"),
    grantRevoke: (args) => this.call<number>("mcp_grant_revoke", args),
    refresh: () => this.call<void>("mcp_refresh"),
  };

  readonly search: SearchApi = {
    status: () => this.call<SearchStatus | null>("search_status"),
    test: () => this.call<string>("search_test"),
    set: (args) => this.call<SearchStatus>("search_set", args),
  };

  readonly system: SystemApi = {
    bootState: () => this.call<BootState>("boot_state"),
    bootstrapData: () => this.call<BootstrapReport>("bootstrap_data"),
    diagLog: (args) => this.call<void>("diag_log", args),
    piStatus: () => this.call<PiStatus>("pi_status"),
    contextCapability: () => this.call<ContextCapability>("context_capability"),
    requestContextPermission: () => this.call<ContextCapability>("request_context_permission"),
    foregroundHistory: () => this.call<FgCtx[]>("foreground_history"),
    foregroundIsCaptureTool: () => this.call<boolean>("foreground_is_capture_tool"),
    orbUiState: () => this.call<{ sleeping: boolean }>("orb_ui_state"),
    orbUiAction: (args) => this.call<void>("orb_ui_action", args),
    takeOpenPanelRequest: () => this.call<boolean>("take_open_panel_request"),
    confirmPanelOpened: () => this.call<void>("confirm_panel_opened"),
    imageThumb: (args) => this.call<string>("image_thumb", args),
    captureScreen: () => this.call<string>("capture_screen"),
    exePath: () => this.call<string>("exe_path"),
    appQuit: () => this.call<void>("app_quit"),
    usageReport: () => this.call<UsageRecord[]>("usage_report"),
    skillsList: () => this.call<SkillInfo[]>("skills_list"),
    builtinTools: () => this.call<BuiltinToolInfo[]>("builtin_tools"),
    openAccessibilitySettings: () => this.call<void>("open_accessibility_settings"),
    distillSessionOpen: () => this.call<Session>("distill_session_open"),
    bundles: () => this.call<BundleInfo[]>("bundles"),
  };
}

/** 从 Base URL 里取出 host 当默认显示名 */
function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/** Session → SessionMeta（会话列表一行） */
function toMeta(s: Session): {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  count: number;
  current: boolean;
  kind: "main" | "task" | "fork";
  projectName?: string | null;
  mode: string;
} {
  return {
    id: s.id,
    title: s.title,
    createdAt: s.createdAt,
    updatedAt: s.updatedAt,
    count: s.messages.length,
    current: false,
    kind: s.kind,
    projectName: null,
    mode: s.mode ?? "standard",
  };
}
