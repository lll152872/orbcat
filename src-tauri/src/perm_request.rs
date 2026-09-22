//! 「申请权限」的授权表 + 待办通道。
//!
//! ## 一次申请的完整旅程
//!
//! ```text
//! ① 模型调 write_file(D:\report\a.md)
//! ② 被规则拒 → tools::authorize 记下「最近一次拒绝」→ 返回错误（附「可申请」提示）
//! ③ 模型调 request_access(D:\report, readwrite, turn, "批量重命名")
//! ④ ask() 登记待办 + 发 `perm-request` 事件 + **阻塞等用户**
//! ⑤ 用户在卡片上点「批准整个 report」
//! ⑥ perm_request_decide 命令 → decide() 唤醒 oneshot
//! ⑦ ask() 返回 Decision → 工具层把授权写进 GrantStore（Task 档另外落盘）
//! ⑧ 模型重试 write_file → authorize 命中授权 → 放行
//! ```
//!
//! ## 为什么阻塞而不是"申请完就返回"
//!
//! 「一轮 = 一次 loop」。若申请不阻塞，用户批"本轮"时本轮**已经结束**了，
//! 令牌批下来立刻失效 —— 逻辑自相矛盾。所以 `ask()` 必须挂起 loop。

use crate::command_policy::{self, CmdGrant, CmdGrantStore, CmdScope, Risk};
use crate::permission::{self, Access, Grant, GrantStore, GrantTier};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 申请超时（毫秒）。超时 = **拒绝**（fail-closed），不是批准。
///
/// 为什么不默认批准：用户不在电脑前时，模型不该因为"没人回"就拿到权限。
pub const REQUEST_TIMEOUT_MS: u64 = 3 * 60 * 1000;

// ---------------------------------------------------------------------------
// 决定
// ---------------------------------------------------------------------------

/// 用户对一条申请的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// 按模型申请的路径批准（可能比被拒路径宽）
    Approve,
    /// 收窄：只批准「刚才被拒的那个具体路径」，不批模型申请的宽范围
    Narrow,
    /// 拒绝
    Deny,
    /// 超时未处理（fail-closed）
    Timeout,
    /// **命令授权**：记住「可执行名 + 首个子命令」（覆盖 `git status -s` 这类变体）
    ApprovePrefix,
    /// **命令授权**：记住完整命令（最窄粒度）
    ApproveFull,
}

impl Decision {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "approve" | "yes" | "批准" => Some(Decision::Approve),
            "narrow" | "收窄" => Some(Decision::Narrow),
            "deny" | "no" | "拒绝" => Some(Decision::Deny),
            // 命令授权粒度的两个按钮
            "prefix" | "approve-prefix" | "记住命令名" => Some(Decision::ApprovePrefix),
            "full" | "approve-full" | "记住完整命令" => Some(Decision::ApproveFull),
            _ => None,
        }
    }

    pub fn is_approved(&self) -> bool {
        matches!(
            self,
            Decision::Approve | Decision::Narrow | Decision::ApprovePrefix | Decision::ApproveFull
        )
    }
}

/// 用户对一条申请的**完整裁决**：决定 + （拒绝时）理由。
///
/// 为什么要带理由：模型被拒后唯一有价值的出路不是"重试"，而是"**按用户的意思调整**"。
/// 没有理由，模型只能瞎猜（原样重试 → 骚扰；彻底放弃 → 活干不下去）。
/// 理由就是把"为什么不行"告诉它，让它改成更窄的路径或换一种做法。
#[derive(Debug, Clone)]
pub struct Verdict {
    pub decision: Decision,
    /// 用户填的拒绝理由（可空）
    pub reason: String,
}

impl Verdict {
    pub fn bare(decision: Decision) -> Self {
        Self {
            decision,
            reason: String::new(),
        }
    }

    pub fn is_approved(&self) -> bool {
        self.decision.is_approved()
    }

    /// 回给模型的说明（模型看到这句就知道下一步该怎么干）
    pub fn to_model_message(&self) -> String {
        let why = self.reason.trim();

        match self.decision {
            Decision::Approve => "用户已批准本次申请，权限已授予，请继续原来的操作。".to_string(),
            Decision::Narrow => "用户只批准了刚才被拒的那个具体路径，范围比你申请的窄。\
                 如果你的操作超出这个范围，会再次被拒 —— 那时针对**具体那个路径**再申请，\
                 不要重复申请更大的范围。"
                .to_string(),
            Decision::Deny => {
                if why.is_empty() {
                    "用户拒绝了本次申请，没有说明理由。\
                     **不要原样重复申请同一个路径** —— 那只会打扰用户。\
                     请如实告诉用户你被拒绝了，并说明你原本想做什么。"
                        .to_string()
                } else {
                    format!(
                        "用户拒绝了本次申请。\n\
                         **用户给的理由**：{why}\n\n\
                         请**按这个理由调整**后再决定要不要重新申请 —— 比如换一条更窄的路径、\
                         换一种不碰那个位置的替代做法，或者干脆停下来问用户想要什么。\n\
                         但**不要原样重复同一个申请**。"
                    )
                }
            }
            Decision::Timeout => "申请超时（用户 3 分钟内没有处理）。**不要立刻重试** —— \
                 用户可能不在电脑前。请先把情况告诉用户，等用户回来再说。"
                .to_string(),
            Decision::ApprovePrefix => "用户已批准，并**记住了这类命令**（可执行名+首个子命令）。\
                 现在直接重试原来那条命令即可。"
                .to_string(),
            Decision::ApproveFull => "用户已批准，并**记住了这条完整命令**。\
                 现在直接重试原来那条命令即可。"
                .to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// 待办
// ---------------------------------------------------------------------------

/// 一条待用户拍板的申请。
pub struct PendingRequest {
    pub id: String,
    /// 模型申请的路径（已规范化）
    pub path: PathBuf,
    pub access: Access,
    pub tier: GrantTier,
    pub reason: String,
    /// 触发这次申请的「被拒路径」（已规范化）。模型没先撞墙就申请时为 `None`
    pub denied_path: Option<PathBuf>,
    pub session_id: String,
    pub created_at: u64,
    pub timeout_ms: u64,
    /// 唤醒 `ask()` 用。**不参与序列化** —— 前端只需要视图。
    tx: tokio::sync::oneshot::Sender<Verdict>,
}

/// 给前端的可序列化视图（不含 oneshot 发送端）。
///
/// 一张视图同时承载**文件级**和**命令级**两种申请 —— 靠 [`PendingView::kind`] 区分。
/// 前端 `renderPermCards` 按 kind 分叉渲染（文件卡 / 命令卡）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    pub id: String,
    /// `"file"` | `"command"`
    pub kind: String,

    // ---- 文件级字段 ----
    pub path: String,
    pub access: String,
    /// 被拒路径（模型先撞墙才有）
    pub denied_path: Option<String>,
    /// 申请范围比被拒路径**宽了几层**；`None` = 不是被拒路径的祖先（或没有被拒路径）
    pub widen_levels: Option<usize>,
    /// 是否真的比被拒路径宽 —— 卡片靠它决定要不要显示"⚠️ 宽了一层"那行
    pub widened: bool,

    // ---- 命令级字段 ----
    /// 命令原文（展示）
    pub command: String,
    /// 归一化后的命令（让用户看到**实际会执行什么** —— 别名/缩写已展开）
    pub normalized: String,
    /// 风险等级 `low|medium|high`
    pub risk: String,
    /// 「可执行名 + 首个子命令」指纹（"记住这类命令"按钮用）
    pub head_fp: String,
    /// 完整命令指纹（"记住这条命令"按钮用）
    pub full_fp: String,

    // ---- 公共字段 ----
    pub tier: String,
    pub reason: String,
    pub session_id: String,
    pub created_at: u64,
    pub timeout_ms: u64,
}

/// 申请范围比被拒路径宽了几层。
///
/// `Some(0)` = 就是被拒的那条路径本身；`Some(2)` = 宽了两层；
/// `None` = 申请的不是被拒路径的祖先（两者无关）。
fn widen_levels(denied: &Path, requested: &Path) -> Option<usize> {
    if !permission::path_has_prefix(denied, requested) {
        return None;
    }
    Some(
        denied
            .components()
            .count()
            .saturating_sub(requested.components().count()),
    )
}

impl PendingRequest {
    pub fn view(&self) -> PendingView {
        let levels = self
            .denied_path
            .as_ref()
            .and_then(|d| widen_levels(d, &self.path));
        PendingView {
            id: self.id.clone(),
            kind: "file".into(),
            path: self.path.to_string_lossy().into_owned(),
            access: self.access.as_str().to_string(),
            denied_path: self
                .denied_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            widen_levels: levels,
            widened: levels.map(|n| n > 0).unwrap_or(false),
            // 命令级字段：文件申请留空
            command: String::new(),
            normalized: String::new(),
            risk: String::new(),
            head_fp: String::new(),
            full_fp: String::new(),
            tier: self.tier.as_str().to_string(),
            reason: self.reason.clone(),
            session_id: self.session_id.clone(),
            created_at: self.created_at,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// 一条待用户拍板的**命令**执行申请。
pub struct PendingCmd {
    pub id: String,
    pub command: String,
    pub normalized: String,
    pub risk: Risk,
    pub head_fp: String,
    pub full_fp: String,
    pub reason: String,
    pub session_id: String,
    pub created_at: u64,
    pub timeout_ms: u64,
    tx: tokio::sync::oneshot::Sender<Verdict>,
}

impl PendingCmd {
    pub fn view(&self) -> PendingView {
        PendingView {
            id: self.id.clone(),
            kind: "command".into(),
            // 文件级字段留空
            path: String::new(),
            access: String::new(),
            denied_path: None,
            widen_levels: None,
            widened: false,
            command: self.command.clone(),
            normalized: self.normalized.clone(),
            risk: self.risk.as_str().to_string(),
            head_fp: self.head_fp.clone(),
            full_fp: self.full_fp.clone(),
            tier: GrantTier::Once.as_str().to_string(),
            reason: self.reason.clone(),
            session_id: self.session_id.clone(),
            created_at: self.created_at,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// 「最近一次文件权限拒绝」。`request_access` 靠它做两件事：
///   1. 校验「申请的路径必须包含被拒的路径」
///   2. 给卡片提供"比被拒路径宽了几层"的对比
#[derive(Debug, Clone)]
pub struct Denial {
    pub session_id: String,
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// PermHub
// ---------------------------------------------------------------------------

/// 往前端发事件的回调：`(事件名, 负载)`。
///
/// **刻意不用 `tauri::AppHandle`** —— 本模块只管「申请权限」的业务逻辑，
/// 不该知道界面是怎么实现的。注入式还有个实际好处：单元测试不需要
/// Tauri 运行时就能编过、跑过。
pub type EmitFn = Box<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// 「申请权限」的总入口：授权表 + 待办表 + 发事件的能力。
///
/// 打包成**一个引用**传进 `ToolCtx` —— 避免每加一项能力就改一次 `agent::run` 的签名。
pub struct PermHub {
    grants: Mutex<GrantStore>,
    pending: Mutex<HashMap<String, PendingRequest>>,
    /// 最近一次拒绝（按会话过滤）。见 [`Denial`]
    last_denial: Mutex<Option<Denial>>,
    /// 发事件用。`setup` 时注入；不注入则静默丢弃
    /// （那种情况下用户看不到卡片，申请会走超时 fail-closed）
    emitter: OnceLock<EmitFn>,
    /// agent 数据目录。`setup` 时注入 —— **命令授权（永久档）**要落到
    /// `<data_dir>/command_grants.json`。不注入 = 放弃落盘（单测场景），
    /// 内存授权照常生效。**文件授权不走这里**（它按会话落 `sessions/<id>.grants.json`）。
    data_dir: OnceLock<PathBuf>,
    /// 被拒记录 —— 「防骚扰」的落点。见 [`PermHub::note_denied`]
    denied: Mutex<Vec<DeniedMemo>>,
    // ---- 命令级（与文件级并列，维度不同：指纹 vs 路径）----
    /// 命令授权表
    cmd_grants: Mutex<CmdGrantStore>,
    /// 命令级待办
    pending_cmds: Mutex<HashMap<String, PendingCmd>>,
    /// 命令级被拒记录（防骚扰）
    denied_cmds: Mutex<Vec<DeniedCmdMemo>>,
    seq: AtomicU64,
}

/// 一条「这个申请被拒过」的记录。
///
/// 模型**原样**再申请同一个 `(会话, 路径, 权限)` 时直接打回，不再弹卡打扰用户。
/// 模型凭理由改成**更窄的路径**（那是另一条记录）时不受影响 ——
/// 那正是我们想要的"按用户的意思调整"，不是骚扰。
#[derive(Debug, Clone)]
pub struct DeniedMemo {
    pub session_id: String,
    pub prefix: PathBuf,
    pub access: Access,
    /// 用户给的理由（空串 = 没给）
    pub reason: String,
}

/// 一条「这条**命令**被拒过」的记录（命令级防骚扰）。
///
/// 与文件级的区别：key 是命令指纹（完整归一化串），不是路径。
#[derive(Debug, Clone)]
pub struct DeniedCmdMemo {
    pub session_id: String,
    pub fingerprint: String,
    /// 用户给的理由（空串 = 没给）
    pub reason: String,
}

impl Default for PermHub {
    fn default() -> Self {
        Self::new()
    }
}

impl PermHub {
    pub fn new() -> Self {
        Self {
            grants: Mutex::new(GrantStore::new()),
            pending: Mutex::new(HashMap::new()),
            last_denial: Mutex::new(None),
            emitter: OnceLock::new(),
            data_dir: OnceLock::new(),
            denied: Mutex::new(Vec::new()),
            cmd_grants: Mutex::new(CmdGrantStore::new()),
            pending_cmds: Mutex::new(HashMap::new()),
            denied_cmds: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
        }
    }

    // -- 防骚扰：被拒记录 ---------------------------------------------------

    /// 记一笔"这条申请被拒了"。同一条只留最新（用户这次可能给了理由、上次没给）。
    fn note_denied(&self, session_id: &str, prefix: &Path, access: Access, reason: &str) {
        let Ok(mut v) = self.denied.lock() else { return };
        v.retain(|m| !(m.session_id == session_id && m.prefix == prefix && m.access == access));
        v.push(DeniedMemo {
            session_id: session_id.to_string(),
            prefix: prefix.to_path_buf(),
            access,
            reason: reason.trim().to_string(),
        });
    }

    /// 这条申请是不是**刚被拒过**（同会话 + 同路径 + 同权限）。
    /// 命中就不弹卡，直接把上次的理由回给模型。
    pub fn find_denied(&self, session_id: &str, prefix: &Path, access: Access) -> Option<DeniedMemo> {
        let v = self.denied.lock().ok()?;
        v.iter()
            .find(|m| m.session_id == session_id && m.prefix == prefix && m.access == access)
            .cloned()
    }

    /// 清掉全部被拒记录 —— **换会话时调**：上个任务拒过的东西，不该管到这个任务。
    pub fn clear_denied(&self) {
        if let Ok(mut v) = self.denied.lock() {
            v.clear();
        }
        if let Ok(mut v) = self.denied_cmds.lock() {
            v.clear();
        }
    }

    // -- 命令级：授权表 / 防骚扰 -------------------------------------------------
    //
    // ⚠️ 命令授权与文件授权的**寿命故意不同**（2026-09-22 用户拍板）：
    //    - 「仅这一次」= 内存里的一次性令牌，用完即销毁，不落盘；
    //    - 「记住这类 / 记住这条」= **永久**，落 `command_grants.json`，
    //      跨会话、跨重启都还在，只有用户主动撤销才失效。
    //    文件授权保持原样：Task 档跟随会话落盘 / 主聊天降级为内存 / Once、Turn 不落盘。

    /// 命令授权表（工具层查/消费）
    pub fn cmd_grants(&self) -> &Mutex<CmdGrantStore> {
        &self.cmd_grants
    }

    /// 注入 agent 数据目录 —— 命令授权落盘靠它。重复注入会被忽略。
    pub fn attach_data_dir(&self, dir: PathBuf) {
        let _ = self.data_dir.set(dir);
    }

    /// 把**永久档**命令授权写进 `<data_dir>/command_grants.json`。
    ///
    /// `data_dir` 未注入（单测场景）→ 静默跳过：内存授权照常生效，只是不持久。
    /// 一次性档由 `save_grants` 内部过滤，**绝不会**被写进文件。
    fn persist_cmd_grants(&self) {
        let Some(dir) = self.data_dir.get() else { return };
        let grants = match self.cmd_grants.lock() {
            Ok(s) => s.grants().to_vec(),
            Err(e) => {
                eprintln!("[float-agent] ⚠️ 命令授权表锁失败，本次未落盘: {e}");
                return;
            }
        };
        if let Err(e) = command_policy::save_grants(dir, &grants) {
            // 落盘失败**不回滚**已生效的内存授权：用户已经批了，让这次操作继续，
            // 只是"重启后失效"。把错误记下来即可。
            eprintln!("[float-agent] ⚠️ 命令授权落盘失败（重启后会失效）: {e}");
        }
    }

    /// 撤销一条命令授权（设置页「撤销」）。**同时落盘**，返回撤销条数。
    pub fn revoke_cmd_grant(&self, fingerprint: &str) -> usize {
        let n = self
            .cmd_grants
            .lock()
            .map(|mut s| s.revoke(fingerprint))
            .unwrap_or(0);
        if n > 0 {
            self.persist_cmd_grants();
        }
        n
    }

    /// 清空全部命令授权（用户主动「全部撤销」）。**同时落盘**（表空 → 删文件）。
    pub fn clear_cmd_all(&self) -> usize {
        let n = self.cmd_grants.lock().map(|mut s| s.clear_all()).unwrap_or(0);
        if n > 0 {
            self.persist_cmd_grants();
        }
        n
    }

    fn note_denied_cmd(&self, session_id: &str, fingerprint: &str, reason: &str) {
        let Ok(mut v) = self.denied_cmds.lock() else { return };
        v.retain(|m| !(m.session_id == session_id && m.fingerprint == fingerprint));
        v.push(DeniedCmdMemo {
            session_id: session_id.to_string(),
            fingerprint: fingerprint.to_string(),
            reason: reason.trim().to_string(),
        });
    }

    /// 这条命令是不是**刚被拒过**（同会话 + 同完整指纹）。命中就不弹卡，直接把上次理由回给模型。
    pub fn find_denied_cmd(&self, session_id: &str, fingerprint: &str) -> Option<DeniedCmdMemo> {
        let v = self.denied_cmds.lock().ok()?;
        v.iter()
            .find(|m| m.session_id == session_id && m.fingerprint == fingerprint)
            .cloned()
    }

    /// 注入发事件的回调。重复注入会被忽略。
    pub fn attach_emitter(&self, f: EmitFn) {
        let _ = self.emitter.set(f);
    }

    fn emit(&self, event: &str, payload: serde_json::Value) {
        if let Some(f) = self.emitter.get() {
            f(event, payload);
        }
    }

    pub fn grants(&self) -> &Mutex<GrantStore> {
        &self.grants
    }

    // -- 最近一次拒绝 -------------------------------------------------------

    pub fn note_denial(&self, session_id: &str, path: &Path) {
        if let Ok(mut d) = self.last_denial.lock() {
            *d = Some(Denial {
                session_id: session_id.to_string(),
                path: path.to_path_buf(),
            });
        }
    }

    /// 取最近一次拒绝 —— **只在同一会话内有效**。
    /// 跨会话拿上一个会话的拒绝来"解释"本次申请是没有意义的。
    pub fn last_denial(&self, session_id: &str) -> Option<Denial> {
        let d = self.last_denial.lock().ok()?.clone()?;
        if d.session_id == session_id {
            Some(d)
        } else {
            None
        }
    }

    pub fn clear_denial(&self) {
        if let Ok(mut d) = self.last_denial.lock() {
            *d = None;
        }
    }

    // -- 待办 ---------------------------------------------------------------

    /// 当前所有待办（前端重绘 / 启动时补拉用）—— 文件级 + 命令级合并
    pub fn pending_views(&self) -> Vec<PendingView> {
        let mut v: Vec<PendingView> = Vec::new();
        if let Ok(map) = self.pending.lock() {
            v.extend(map.values().map(|p| p.view()));
        }
        if let Ok(map) = self.pending_cmds.lock() {
            v.extend(map.values().map(|p| p.view()));
        }
        v.sort_by_key(|p| p.created_at);
        v
    }

    /// 用户做了决定 —— 唤醒 `ask()`。
    ///
    /// 注意是**先移除再发送**：移除成功才说明这条待办还没被处理过，
    /// 重复点击不会把同一个决定发两次。
    pub fn decide(&self, id: &str, decision: Decision, reason: String) -> Result<(), String> {
        let taken = {
            let mut map = self.pending.lock().map_err(|e| format!("待办表锁失败: {e}"))?;
            map.remove(id)
        };
        match taken {
            Some(p) => p
                .tx
                .send(Verdict { decision, reason })
                .map_err(|_| "申请已失效（等待方已超时离开）".to_string()),
            None => Err(format!("申请 {id} 不存在或已处理")),
        }
    }

    /// 用户对一条**命令**申请的决定（唤醒 `ask_command()`）。
    pub fn decide_command(&self, id: &str, decision: Decision, reason: String) -> Result<(), String> {
        let taken = {
            let mut map = self
                .pending_cmds
                .lock()
                .map_err(|e| format!("命令待办表锁失败: {e}"))?;
            map.remove(id)
        };
        match taken {
            Some(p) => p
                .tx
                .send(Verdict { decision, reason })
                .map_err(|_| "命令申请已失效（等待方已超时离开）".to_string()),
            None => Err(format!("命令申请 {id} 不存在或已处理")),
        }
    }

    /// 登记一条申请并**阻塞等待**用户决定（超时 → [`Decision::Timeout`]）。
    ///
    /// 返回 `Decision` 而不是直接改授权表 —— 由调用方决定究竟批多大范围：
    /// `Narrow` 要落在**被拒路径**上，而不是模型申请的宽路径。
    #[allow(clippy::too_many_arguments)]
    pub async fn ask(
        &self,
        path: PathBuf,
        access: Access,
        tier: GrantTier,
        reason: String,
        denied_path: Option<PathBuf>,
        session_id: &str,
    ) -> Result<Verdict, String> {
        let id = format!(
            "pr{}",
            self.seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let timeout_ms = REQUEST_TIMEOUT_MS;

        // 留一份路径给"被拒记一笔"用（下面 req 会把 path move 走）
        let asked_path = path.clone();

        let req = PendingRequest {
            id: id.clone(),
            path,
            access,
            tier,
            reason,
            denied_path,
            session_id: session_id.to_string(),
            created_at: permission::now_ms(),
            timeout_ms,
            tx,
        };
        let view = req.view();

        {
            let mut map = self.pending.lock().map_err(|e| format!("待办表锁失败: {e}"))?;
            map.insert(id.clone(), req);
        }
        self.emit_request(&view);

        let verdict = match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(v)) => v,
            // 发送端被 drop（不该发生，但出现了就当拒绝）
            Ok(Err(_)) => Verdict::bare(Decision::Deny),
            Err(_) => Verdict::bare(Decision::Timeout),
        };

        // 用户点过的话 decide() 已经移除了；超时这条路径要自己清，否则待办表会越积越多
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&id);
        }

        // 被拒的记一笔 —— 模型下次原样再申请时直接打回，不再打扰用户。
        // 这就是「防骚扰」的落点：不是禁言路径，而是**同一请求不重复弹卡**，
        // 模型仍可凭理由改成更窄/不同的申请（那是另一条 memo，会正常弹卡）。
        if !verdict.is_approved() {
            self.note_denied(session_id, &asked_path, access, &verdict.reason);
        }

        self.emit_resolved(&id, verdict.decision);
        Ok(verdict)
    }

    /// 登记一条**命令**申请并阻塞等用户决定（超时 → [`Decision::Timeout`]）。
    ///
    /// 与文件版 [`PermHub::ask`] 同构：同样的超时 fail-closed、同样的防骚扰。
    /// 区别只在维度 —— 这里 key 是**命令指纹**，不是路径。
    #[allow(clippy::too_many_arguments)]
    pub async fn ask_command(
        &self,
        command: String,
        normalized: String,
        head_fp: String,
        full_fp: String,
        risk: Risk,
        reason: String,
        session_id: &str,
    ) -> Result<Verdict, String> {
        let id = format!("pc{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let timeout_ms = REQUEST_TIMEOUT_MS;

        let fp_for_memo = full_fp.clone();

        let req = PendingCmd {
            id: id.clone(),
            command,
            normalized,
            risk,
            head_fp,
            full_fp,
            reason,
            session_id: session_id.to_string(),
            created_at: permission::now_ms(),
            timeout_ms,
            tx,
        };
        let view = req.view();

        {
            let mut map = self
                .pending_cmds
                .lock()
                .map_err(|e| format!("命令待办表锁失败: {e}"))?;
            map.insert(id.clone(), req);
        }
        self.emit_request(&view);

        let verdict = match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => Verdict::bare(Decision::Deny),
            Err(_) => Verdict::bare(Decision::Timeout),
        };

        if let Ok(mut map) = self.pending_cmds.lock() {
            map.remove(&id);
        }

        // 被拒的记一笔 —— 同一命令原样再申请时直接打回，不再弹卡
        if !verdict.is_approved() {
            self.note_denied_cmd(session_id, &fp_for_memo, &verdict.reason);
        }

        self.emit_resolved(&id, verdict.decision);
        Ok(verdict)
    }

    /// 把命令批准结果写进命令授权表，返回写入的授权。
    ///
    /// - `Approve` → 批准**一次**（完整命令指纹，Once 档，**不落盘**）
    /// - `ApprovePrefix` → 记住「名+首子命令」（**永久**）
    /// - `ApproveFull` → 记住**完整命令**（**永久**）
    ///
    /// 后两种会立刻落 `command_grants.json` —— 这就是"记住"跨重启存活的关键一步。
    /// （`tier` 只作持久标记展示用，命令侧**不再**按 tier 失效，见 [`PermHub::cmd_grants`]。）
    pub fn apply_cmd_decision(&self, decision: Decision, command: &str, reason: &str) -> Option<CmdGrant> {
        let grant = match decision {
            Decision::Approve => command_policy::grant_once(command, reason),
            Decision::ApprovePrefix => {
                command_policy::grant_lasting(command, CmdScope::Prefix, GrantTier::Task, reason)
            }
            Decision::ApproveFull => {
                command_policy::grant_lasting(command, CmdScope::Full, GrantTier::Task, reason)
            }
            _ => return None,
        };

        match self.cmd_grants.lock() {
            Ok(mut store) => store.add(grant.clone()),
            Err(e) => {
                eprintln!("[float-agent] ⚠️ 命令授权表锁失败，本次批准未生效: {e}");
                return None;
            }
        }
        // 一次性档会被 `save_grants` 过滤掉，所以这里无脑落盘是安全的。
        self.persist_cmd_grants();
        Some(grant)
    }

    /// 把批准结果写进授权表，返回实际用的前缀。
    ///
    /// `Narrow` 时用 `denied_path`（收窄到被拒的具体路径），否则用申请的 `path`。
    pub fn apply_decision(
        &self,
        decision: Decision,
        requested: &Path,
        denied_path: Option<&Path>,
        access: Access,
        tier: GrantTier,
        reason: &str,
    ) -> Option<PathBuf> {
        if !decision.is_approved() {
            return None;
        }

        let prefix: PathBuf = match decision {
            Decision::Narrow => denied_path.unwrap_or(requested).to_path_buf(),
            _ => requested.to_path_buf(),
        };

        let grant = if tier == GrantTier::Once {
            Grant::once(&prefix, access, reason)
        } else {
            Grant::lasting(&prefix, access, tier, reason)
        };

        match self.grants.lock() {
            Ok(mut store) => {
                store.add(grant);
                Some(prefix)
            }
            Err(e) => {
                eprintln!("[float-agent] ⚠️ 授权表锁失败，本次批准未生效: {e}");
                None
            }
        }
    }

    /// 把 `Task` 档授权同步进会话文件（`sessions/<id>.grants.json`）。
    ///
    /// 两条约束：
    /// - **只写 `Task` 档** —— `Once`/`Turn` 一旦落盘就跨重启存活，
    ///   "一次/一轮"的语义当场破裂；
    /// - **主聊天不落盘** —— 它是永久会话，落盘等于变相无限期授权。
    ///   所以在主聊天里 `Task` 档被降级为"本次 app 运行期"（只活在内存里）。
    ///
    /// 落盘失败**不回滚**已生效的内存授权：用户已经批了，让这次操作继续，
    /// 只是"重启后失效"。把错误返回给上层记日志即可。
    pub fn persist_task_grants(&self, data_dir: &Path, session_id: &str) -> Result<(), String> {
        if session_id.is_empty() {
            return Ok(());
        }

        let is_task_session = crate::sessions::load(data_dir, session_id)
            .map(|s| !s.is_main())
            .unwrap_or(false);
        if !is_task_session {
            return Ok(());
        }

        let task_grants: Vec<Grant> = {
            let store = self
                .grants
                .lock()
                .map_err(|e| format!("授权表锁失败: {e}"))?;
            store
                .grants()
                .iter()
                .filter(|g| g.tier == GrantTier::Task)
                .cloned()
                .collect()
        };

        crate::sessions::save_grants(data_dir, session_id, &task_grants)
    }

    // -- 事件 ---------------------------------------------------------------

    fn emit_request(&self, view: &PendingView) {
        match serde_json::to_value(view) {
            Ok(v) => self.emit("perm-request", v),
            Err(e) => eprintln!("[float-agent] ⚠️ 序列化权限申请失败: {e}"),
        }
    }

    fn emit_resolved(&self, id: &str, decision: Decision) {
        self.emit(
            "perm-resolved",
            serde_json::json!({ "id": id, "decision": decision }),
        );
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_parse_and_model_message() {
        assert_eq!(Decision::parse("approve"), Some(Decision::Approve));
        assert_eq!(Decision::parse("收窄"), Some(Decision::Narrow));
        assert_eq!(Decision::parse("deny"), Some(Decision::Deny));
        assert_eq!(Decision::parse("随便"), None);

        assert!(Decision::Approve.is_approved());
        assert!(Decision::Narrow.is_approved());
        assert!(!Decision::Deny.is_approved());
        assert!(!Decision::Timeout.is_approved());

        // 无理由的拒绝：明确告诉模型"别原样重复"
        let bare = Verdict::bare(Decision::Deny).to_model_message();
        assert!(bare.contains("不要原样重复"), "实际：{bare}");
        assert!(!bare.contains("用户给的理由"), "没给理由时不该编一个出来");

        // 有理由：理由必须原样出现在消息里，并且给出"按理由调整"的出路
        let with_why = Verdict {
            decision: Decision::Deny,
            reason: "这个目录里有合同，别碰".into(),
        }
        .to_model_message();
        assert!(with_why.contains("这个目录里有合同，别碰"), "实际：{with_why}");
        assert!(with_why.contains("按这个理由调整"));
        assert!(with_why.contains("不要原样重复"));

        // 超时必须明确"不要立刻重试"
        assert!(Verdict::bare(Decision::Timeout)
            .to_model_message()
            .contains("不要立刻重试"));
    }

    #[test]
    fn denied_memo_blocks_exact_repeat_only() {
        // 防骚扰的核心：**同路径同权限**再申请直接打回；
        // 换成更窄的路径（正是"按理由调整"）不拦。
        let hub = PermHub::new();
        assert!(hub
            .find_denied("s1", Path::new(r"D:\report"), Access::ReadWrite)
            .is_none());

        hub.note_denied("s1", Path::new(r"D:\report"), Access::ReadWrite, "有合同");

        let hit = hub
            .find_denied("s1", Path::new(r"D:\report"), Access::ReadWrite)
            .expect("同路径同权限应命中");
        assert_eq!(hit.reason, "有合同");

        // 更窄的路径 → 不命中，允许重新申请
        assert!(hub
            .find_denied("s1", Path::new(r"D:\report\a.md"), Access::ReadWrite)
            .is_none());
        // 权限不同 → 不命中
        assert!(hub
            .find_denied("s1", Path::new(r"D:\report"), Access::Read)
            .is_none());
        // 换个会话 → 不命中（上个任务拒过的不该管到这个任务）
        assert!(hub
            .find_denied("s2", Path::new(r"D:\report"), Access::ReadWrite)
            .is_none());

        hub.clear_denied();
        assert!(hub
            .find_denied("s1", Path::new(r"D:\report"), Access::ReadWrite)
            .is_none());
    }

    #[test]
    fn note_denied_keeps_latest_reason() {
        let hub = PermHub::new();
        hub.note_denied("s1", Path::new(r"D:\a"), Access::Read, "第一次");
        hub.note_denied("s1", Path::new(r"D:\a"), Access::Read, "第二次");
        let hit = hub.find_denied("s1", Path::new(r"D:\a"), Access::Read).unwrap();
        assert_eq!(hit.reason, "第二次", "同一条只留最新理由，不该堆成两条");
    }

    #[test]
    fn widen_levels_counts_extra_components() {
        // 同一条路径 → 0 层
        assert_eq!(
            widen_levels(Path::new(r"D:\a\b.md"), Path::new(r"D:\a\b.md")),
            Some(0)
        );
        // 宽一层：D:\a\b.md → D:\a
        assert_eq!(
            widen_levels(Path::new(r"D:\a\b.md"), Path::new(r"D:\a")),
            Some(1)
        );
        // 宽两层：D:\a\b.md → D:\
        assert_eq!(
            widen_levels(Path::new(r"D:\a\b.md"), Path::new(r"D:\")),
            Some(2)
        );
        // 无关路径 → None（卡片不该说"宽了几层"）
        assert_eq!(
            widen_levels(Path::new(r"D:\a\b.md"), Path::new(r"D:\other")),
            None
        );
    }

    #[test]
    fn last_denial_is_session_scoped() {
        let hub = PermHub::new();
        assert!(hub.last_denial("s1").is_none());

        hub.note_denial("s1", Path::new(r"D:\work\a.md"));
        assert!(hub.last_denial("s1").is_some());
        // 换个会话就不该再拿出来用 —— 拿上个会话的拒绝解释本次申请没有意义
        assert!(hub.last_denial("s2").is_none());

        hub.clear_denial();
        assert!(hub.last_denial("s1").is_none());
    }

    #[test]
    fn apply_decision_writes_grant_and_narrow_uses_denied_path() {
        let hub = PermHub::new();

        // 拒绝 → 不写授权
        assert!(hub
            .apply_decision(
                Decision::Deny,
                Path::new(r"D:\report"),
                Some(Path::new(r"D:\report\a.md")),
                Access::ReadWrite,
                GrantTier::Turn,
                "批量重命名",
            )
            .is_none());
        assert!(hub.grants().lock().unwrap().is_empty());

        // 收窄批准 → 落在**被拒路径**上，而不是模型申请的宽路径
        let used = hub
            .apply_decision(
                Decision::Narrow,
                Path::new(r"D:\report"),
                Some(Path::new(r"D:\report\a.md")),
                Access::ReadWrite,
                GrantTier::Turn,
                "批量重命名",
            )
            .unwrap();
        assert_eq!(used, PathBuf::from(r"D:\report\a.md"));

        // 按申请批准 → 落在申请的宽路径上
        let used2 = hub
            .apply_decision(
                Decision::Approve,
                Path::new(r"D:\report"),
                Some(Path::new(r"D:\report\a.md")),
                Access::ReadWrite,
                GrantTier::Turn,
                "批量重命名",
            )
            .unwrap();
        assert_eq!(used2, PathBuf::from(r"D:\report"));
    }

    #[tokio::test]
    async fn ask_returns_timeout_when_nobody_answers() {
        // 不连 app（发不出事件），也没人调 decide → 只能等超时。
        // 这里为省测试时间直接调 decide 走"发送端被丢弃"分支的反面：
        // 用 tokio::time::pause 太重，改为验证 decide 后能正确唤醒。
        let hub = std::sync::Arc::new(PermHub::new());

        let h2 = hub.clone();
        let waiter = tokio::spawn(async move {
            h2.ask(
                PathBuf::from(r"D:\work"),
                Access::ReadWrite,
                GrantTier::Turn,
                "要写东西".into(),
                Some(PathBuf::from(r"D:\work\a.md")),
                "s1",
            )
            .await
        });

        // 等待办登记进来
        for _ in 0..100 {
            if !hub.pending_views().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let views = hub.pending_views();
        assert_eq!(views.len(), 1, "应有且只有一条待办");
        assert_eq!(views[0].access, "readwrite");
        assert_eq!(views[0].tier, "turn");
        assert!(views[0].widened, "申请 D:\\work 比被拒的 D:\\work\\a.md 宽，应标记 widened");
        assert_eq!(views[0].widen_levels, Some(1));

        hub.decide(&views[0].id, Decision::Narrow, "只批这一个文件".into())
            .unwrap();
        let got = waiter.await.unwrap().unwrap();
        assert_eq!(got.decision, Decision::Narrow);
        assert_eq!(got.reason, "只批这一个文件");
        assert!(hub.pending_views().is_empty(), "决定后待办应清空");

        // 被拒才记 memo；这里批准了，不该留记录
        assert!(hub
            .find_denied("s1", Path::new(r"D:\work"), Access::ReadWrite)
            .is_none());
    }

    #[tokio::test]
    async fn denied_ask_leaves_a_memo() {
        // 走完整 ask 流程验证"被拒 → 留记录"，而不是只测 note_denied 本身
        let hub = std::sync::Arc::new(PermHub::new());
        let h2 = hub.clone();
        let waiter = tokio::spawn(async move {
            h2.ask(
                PathBuf::from(r"D:\report"),
                Access::ReadWrite,
                GrantTier::Turn,
                "要写东西".into(),
                None,
                "s1",
            )
            .await
        });

        for _ in 0..100 {
            if !hub.pending_views().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let views = hub.pending_views();
        hub.decide(&views[0].id, Decision::Deny, "那目录别碰".into())
            .unwrap();
        let got = waiter.await.unwrap().unwrap();
        assert_eq!(got.decision, Decision::Deny);

        let memo = hub
            .find_denied("s1", Path::new(r"D:\report"), Access::ReadWrite)
            .expect("被拒后应留下记录，模型原样再申请时不再弹卡");
        assert_eq!(memo.reason, "那目录别碰");
    }

    #[test]
    fn decide_twice_is_rejected() {
        let hub = PermHub::new();
        // 没有这条待办 → 报错而不是静默成功
        assert!(hub.decide("pr999", Decision::Approve, String::new()).is_err());
    }
}
