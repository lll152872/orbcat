/**
 * 真数据源 —— 走 Tauri `invoke`。
 *
 * 每个方法 = 一个后端命令，**参数原样透传**（不做任何重塑）：
 * 这样「接口层」只是加了一层壳，后端契约一个字都不用改，
 * 也不用担心两边的形状对不上。
 *
 * ⚠️ 本文件是**唯一**允许出现 Tauri `invoke` 的地方（`src/data/index.ts` 的
 * 切换逻辑除外）—— 业务代码里再出现 `invoke` 就是漏改了。
 */
import { invoke } from "@tauri-apps/api/core";
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

export class TauriSource implements DataSource {
  readonly kind = "tauri" as const;

  call<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    return invoke<T>(cmd, args);
  }

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
    /**
     * 切 shell（`pwsh` / `gitbash`）。
     *
     * ⚠️ 后端那条在 `work/backend/exec-shell` 分支实现中 —— 本分支先落前端契约。
     *    后端未上线时这个调用会失败（unknown command）⇒ 前端要兜住并提示，
     *    **不能静默失败**（用户点了没反应 = 假开关）。
     */
    setExecShell: (args) => this.call<string>("set_exec_shell", args),
    /**
     * 查当前 shell + Git Bash 可用性。
     *
     * ⚠️ 发布包不带 git ⇒ 这台机器没装 Git for Windows 时 `gitBashAvailable` 为 false
     *    ⇒ 设置页要提前提示「选了会失败」，不能等用户选完才报错。
     */
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
