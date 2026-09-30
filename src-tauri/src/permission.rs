//! 文件权限网关
//!
//! 设计目标（来自 docs/00-CONCEPT.md 决策 7）：
//!   - 三级权限：只读 / 读写 / 完全控制
//!   - **最长前缀匹配**（specificity rule）
//!   - **判断前必须 canonicalize**，防 Windows 路径别名绕过
//!
//! 必须防住的 5 类绕过：
//!   1. 大小写：`D:\MYWORD` vs `D:\myword`
//!   2. 路径穿越：`D:\myword\..\mycode`
//!   3. 符号链接 / junction：`D:\myword\link` → `D:\vsssms`
//!   4. 8.3 短名：`D:\PROGRA~1\...`
//!   5. UNC / 网络路径
//!
//! 核心思路：
//!   - `canonicalize` 解析掉 2/3/4/5（它会展开链接、解析短名、规整前缀）
//!   - **按路径组件比较**（不是字符串前缀）天然解决 `D:\myword` vs `D:\mywordx`
//!   - 组件比较时大小写不敏感，解决 1

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// 权限级别。
///
/// `Deny` 是**显式拒绝**，用来在放行范围内挖洞（例：`D:\` 只读，但 `D:\secret` 拒绝）。
/// 它在枚举里排最前，但 `check` 里**必须特判** —— 否则会走进 `access >= need`
/// 的分支被当成"权限不足"而不是"明确禁止"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// 显式拒绝（最长前缀命中即拒）
    Deny,
    /// 只能读、列目录
    Read,
    /// 读 + 写 + 改（但不能删除）
    ReadWrite,
    /// 完全控制（含删除、移到回收站）
    Full,
}

impl Access {
    pub fn as_str(&self) -> &'static str {
        match self {
            Access::Deny => "deny",
            Access::Read => "read",
            Access::ReadWrite => "readwrite",
            Access::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "deny" | "none" | "拒绝" => Some(Access::Deny),
            "read" | "只读" => Some(Access::Read),
            "readwrite" | "write" | "读写" => Some(Access::ReadWrite),
            "full" | "完全" | "完全控制" => Some(Access::Full),
            _ => None,
        }
    }
}

/// 一条路径规则（可序列化，用于持久化到 permissions.json）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// 规则作用的路径前缀（**必须已规范化**）
    pub prefix: PathBuf,
    /// 该前缀下允许的最大权限
    pub access: Access,
    /// 人类可读的来源说明（用于拒绝时解释原因）
    #[serde(default)]
    pub label: String,
}

/// 拒绝原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// 路径不在任何规则内
    NoMatchingRule { path: PathBuf },
    /// 命中了 `Deny` 规则（用户显式禁止）
    ExplicitlyDenied {
        path: PathBuf,
        rule_prefix: PathBuf,
        rule_label: String,
    },
    /// 匹配到了规则，但权限不足
    InsufficientAccess {
        path: PathBuf,
        rule_prefix: PathBuf,
        rule_label: String,
        required: Access,
        allowed: Access,
    },
    /// 路径无法规范化
    BadPath { path: PathBuf, detail: String },
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenyReason::NoMatchingRule { path } => write!(
                f,
                "拒绝访问：{} 不在任何已授权范围内。请在设置 → 文件权限中添加该目录。",
                path.display()
            ),
            DenyReason::ExplicitlyDenied {
                path,
                rule_prefix,
                rule_label,
            } => write!(
                f,
                "拒绝访问：{} 被规则「{}」（{}）显式禁止",
                path.display(),
                rule_label,
                rule_prefix.display()
            ),
            DenyReason::InsufficientAccess {
                path,
                rule_prefix,
                rule_label,
                required,
                allowed,
            } => write!(
                f,
                "权限不足：{} 需要 {}，但规则「{}」（{}）只授予 {}",
                path.display(),
                required.as_str(),
                rule_label,
                rule_prefix.display(),
                allowed.as_str()
            ),
            DenyReason::BadPath { path, detail } => {
                write!(f, "路径无法解析：{} —— {}", path.display(), detail)
            }
        }
    }
}

/// 权限网关。
///
/// 规则**按前缀长度降序**保存在内部，`check` 时取第一个命中的即为最长前缀匹配。
#[derive(Debug, Clone, Default)]
pub struct PermissionGate {
    rules: Vec<Rule>,
}

impl PermissionGate {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// 添加规则。prefix 会被尽力规范化；若规范化后为空则忽略。
    pub fn add_rule(&mut self, prefix: impl AsRef<Path>, access: Access, label: impl Into<String>) {
        let normalized = best_effort_canonicalize(prefix.as_ref());
        if normalized.as_os_str().is_empty() {
            return;
        }
        self.rules.push(Rule {
            prefix: normalized,
            access,
            label: label.into(),
        });
        self.resort();
    }

    /// 删除所有 label 匹配的规则（用于用户撤销授权）
    pub fn remove_rules_by_label(&mut self, label: &str) -> usize {
        let before = self.rules.len();
        self.rules.retain(|r| r.label != label);
        before - self.rules.len()
    }

    fn resort(&mut self) {
        // 长的在前 —— 保证最长前缀优先
        self.rules.sort_by(|a, b| {
            let la = a.prefix.components().count();
            let lb = b.prefix.components().count();
            lb.cmp(&la)
                .then_with(|| b.prefix.as_os_str().len().cmp(&a.prefix.as_os_str().len()))
        });
    }

    /// 返回当前所有规则（供设置界面展示）
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// 判定某路径是否满足所需权限（**不带临时授权**）。
    ///
    /// 内部就是 [`PermissionGate::authorize`] 传空授权表的特例 ——
    /// 这样"最长前缀 + Deny 特判"只有一份实现，不会两套逻辑各自演化。
    ///
    /// ⚠️ 新代码请优先用 `authorize`：它还会告诉你是「规则长期放行」还是
    /// 「临时授权放行」，后者用完要 `GrantStore::consume` 扣次数。
    /// 本签名保留是为了不惊动既有调用方。
    pub fn check(&self, path: impl AsRef<Path>, need: Access) -> Result<PathBuf, DenyReason> {
        self.authorize(path, need, &[]).map(|a| a.path)
    }

    // -- 规则的整体替换（持久化用） ------------------------------------------

    /// 用一批规则整体替换（会重新规范化并排序）
    pub fn replace_rules(&mut self, rules: &[Rule]) {
        self.rules.clear();
        for r in rules {
            self.add_rule(&r.prefix, r.access, r.label.clone());
        }
    }

    /// 导出规则（用于写入 permissions.json）
    pub fn export_rules(&self) -> Vec<Rule> {
        self.rules.clone()
    }

    /// 删除某一条规则（按前缀精确匹配）
    pub fn remove_rule(&mut self, prefix: impl AsRef<Path>) -> bool {
        let target = best_effort_canonicalize(prefix.as_ref());
        let before = self.rules.len();
        self.rules.retain(|r| r.prefix != target);
        before != self.rules.len()
    }

    /// 便捷：只读检查
    #[allow(dead_code)]
    pub fn check_read(&self, path: impl AsRef<Path>) -> Result<PathBuf, DenyReason> {
        self.check(path, Access::Read)
    }

    /// 便捷：写检查
    #[allow(dead_code)]
    pub fn check_write(&self, path: impl AsRef<Path>) -> Result<PathBuf, DenyReason> {
        self.check(path, Access::ReadWrite)
    }

    /// 便捷：删除检查
    #[allow(dead_code)]
    pub fn check_delete(&self, path: impl AsRef<Path>) -> Result<PathBuf, DenyReason> {
        self.check(path, Access::Full)
    }
}

// ---------------------------------------------------------------------------
// 路径工具
// ---------------------------------------------------------------------------

/// 尽力规范化：
///   1. 路径存在 → 直接用 `canonicalize`（会展开 junction / 解析 8.3 短名 / 去掉 `..`）
///   2. 路径不存在（如待创建的文件）→ 找到最近的**存在**祖先做 canonicalize，
///      再把剩余部分拼回去
///
/// 并剥掉 Windows canonicalize 产生的 `\\?\` 前缀。
pub fn best_effort_canonicalize(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return strip_verbatim(c);
    }

    // 逐级向上找存在的祖先
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = path.to_path_buf();

    loop {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        } else {
            // 到达根（如 `D:\`）或无法继续
            break;
        }

        let Some(parent) = cur.parent() else { break };

        if let Ok(c) = std::fs::canonicalize(parent) {
            let mut result = strip_verbatim(c);
            for seg in tail.iter().rev() {
                result.push(seg);
            }
            return result;
        }

        cur = parent.to_path_buf();
        if cur.as_os_str().is_empty() {
            break;
        }
    }

    // 完全无法规范化：至少做一次词法清理
    lexical_normalize(path)
}

/// 剥掉 `\\?\` / `\\?\UNC\` 前缀（Windows `canonicalize` 的产物）
pub(crate) fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

/// 纯词法规整：去掉 `.`、消解 `..`（不碰文件系统）
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 按**组件**判断 `path` 是否位于 `prefix` 之下（含相等）。
///
/// 关键：不是字符串前缀比较 —— 这样 `D:\myword` **不会**匹配 `D:\mywordx`。
/// 组件比较在 Windows 上大小写不敏感。
pub fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    let p: Vec<Component> = path.components().collect();
    let q: Vec<Component> = prefix.components().collect();
    if q.len() > p.len() {
        return false;
    }
    p.iter().zip(q.iter()).all(|(a, b)| component_eq(a, b))
}

fn component_eq(a: &Component, b: &Component) -> bool {
    use Component::*;
    match (a, b) {
        (Prefix(x), Prefix(y)) => x.as_os_str().to_string_lossy().eq_ignore_ascii_case(&y.as_os_str().to_string_lossy()),
        (RootDir, RootDir) => true,
        (CurDir, CurDir) => true,
        (ParentDir, ParentDir) => true,
        (Normal(x), Normal(y)) => x.to_string_lossy().eq_ignore_ascii_case(&y.to_string_lossy()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 默认规则集
// ---------------------------------------------------------------------------

/// 构造用户确认过的默认规则集（来自 docs/00-CONCEPT.md 决策 7）：
///
/// | 路径 | 权限 |
/// |---|---|
/// | `D:\` | 只读 |
/// | `D:\myword` | 读 + 写 |
/// | agent 自有数据目录 | 完全控制 |
/// | agent 程序目录 | 完全控制（开发需要） |
/// | 系统临时目录下的 agent 专属子目录 | 完全控制 |
/// 权限规则持久化文件
pub fn rules_path(data_dir: &Path) -> PathBuf {
    data_dir.join("permissions.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<Rule>,
}

/// 首次运行时生成的**初始规则**。
///
/// ⚠️ 与早先的硬编码版本的关键区别：**这些只是写进文件里的初始值**，
/// 用户在「设置 → 文件权限」里可以随意增删改，文件说了算。
///
/// 初始值的设计取向 —— **能读，但默认不给写**：
///   - 各盘根目录：只读（让"帮我看看某个文件"开箱即用）
///   - 程序自身目录：agent-data 完全控制 / 程序目录只读 / 临时区读写
///   - **不预置任何业务目录的写权限** —— 要写某个目录，由用户显式授权。
///     没授权时会明确拒绝并提示去哪加规则，而不是悄悄放行。
pub fn bootstrap_gate(agent_data_dir: impl AsRef<Path>, app_dir: impl AsRef<Path>) -> PermissionGate {
    let mut gate = PermissionGate::new();

    // ---- 程序自身（跑起来必须的）----
    gate.add_rule(
        agent_data_dir.as_ref(),
        Access::Full,
        "agent 数据目录（程序自身）",
    );
    gate.add_rule(app_dir.as_ref(), Access::Read, "agent 程序目录（程序自身）");

    let tmp = std::env::temp_dir();
    gate.add_rule(
        tmp.join("orbcat"),
        Access::ReadWrite,
        "agent 临时工作区（程序自身）",
    );

    // ---- 各盘根目录：只读（存在才加）----
    for letter in ["C", "D", "E", "F"] {
        let root = PathBuf::from(format!("{letter}:\\"));
        if root.exists() {
            gate.add_rule(
                &root,
                Access::Read,
                &format!("{letter} 盘根目录（只读，可改）"),
            );
        }
    }

    // ---- 用户主目录：只读 ----
    if let Ok(home) = std::env::var("USERPROFILE") {
        let h = PathBuf::from(&home);
        if h.exists() {
            gate.add_rule(&h, Access::Read, "用户主目录（只读，可改）");
        }
    }

    gate
}

/// 加载权限规则。
///
/// - 文件存在 → **完全以文件为准**（用户的配置说了算）
/// - 不存在 → 用自举规则，并**立刻写入文件**（让用户看得见、改得动）
pub fn load_gate(agent_data_dir: impl AsRef<Path>, app_dir: impl AsRef<Path>) -> PermissionGate {
    let data_dir = agent_data_dir.as_ref();
    let path = rules_path(data_dir);

    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(txt) => match serde_json::from_str::<RulesFile>(&txt) {
                Ok(f) => {
                    let mut gate = PermissionGate::new();
                    gate.replace_rules(&f.rules);
                    return gate;
                }
                Err(e) => {
                    eprintln!(
                        "[orbcat] ⚠️ {} 解析失败（{e}），本次改用自举规则（文件未改动）",
                        path.display()
                    );
                }
            },
            Err(e) => {
                eprintln!("[orbcat] ⚠️ 读取 {} 失败（{e}），改用自举规则", path.display());
            }
        }
    }

    let gate = bootstrap_gate(data_dir, app_dir);
    match save_gate(data_dir, &gate) {
        Ok(p) => eprintln!("[orbcat] 已生成默认权限文件: {}", p.display()),
        Err(e) => eprintln!("[orbcat] ⚠️ 写权限文件失败: {e}"),
    }
    gate
}

/// 保存规则到 `permissions.json`
pub fn save_gate(data_dir: &Path, gate: &PermissionGate) -> Result<PathBuf, String> {
    let path = rules_path(data_dir);
    let file = RulesFile {
        rules: gate.export_rules(),
    };
    let txt = serde_json::to_string_pretty(&file).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&path, txt).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// 临时授权层（「申请权限」功能的产物）
// ---------------------------------------------------------------------------
//
// 三个概念要分清：
//   - **规则**（`Rule`）     ：用户设的长期策略，落 `permissions.json`
//   - **授权**（`Grant`）    ：用户当场批的临时放行，**不落 permissions.json**
//   - **档位**（`GrantTier`）：授权能活多久
//
// 为什么要独立一层而不是"批准后往规则里加一条"：
//   「一次 / 一轮」如果写进规则，就会跨重启存活 —— 那这两个档位的语义当场破裂。
//   只有「整个任务」落盘，且落在会话目录旁边，随会话一起消失（无孤儿授权）。

/// 授权档位 —— **时间维度**。空间维度是 [`Grant::prefix`]，两者缺一不可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantTier {
    /// 一次：只够 **1 次**工具调用，用掉即销毁
    Once,
    /// 一轮：1 次 agent loop 内有效，loop 结束整批销毁
    Turn,
    /// 整个任务：任务会话生命周期。
    /// 主聊天里降级为「本次 app 运行期」→ 不落盘（见 [`GrantTier::is_persistent`] 的调用方）
    Task,
}

// ⚠️ 本段（含 `Grant` / `GrantStore` / `Allowed.grant_idx`）是「申请权限」的地基，
//    已实现且有单测覆盖，但**工具层还没接线**（见 docs/02-PERMISSION-REQUEST.md 第 5 项）。
//    接线后这些 allow 应当全部删掉。
#[allow(dead_code)]
impl GrantTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            GrantTier::Once => "once",
            GrantTier::Turn => "turn",
            GrantTier::Task => "task",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "once" | "一次" => Some(GrantTier::Once),
            "turn" | "一轮" => Some(GrantTier::Turn),
            "task" | "整个任务" => Some(GrantTier::Task),
            _ => None,
        }
    }

    /// 这一档是否落盘。只有 `Task` 落盘，且**落在会话旁边**（`sessions/<id>.grants.json`）。
    ///
    /// ⚠️ 主聊天是永久会话，在它里面批 `Task` 等于变相无限期授权 ——
    /// 所以调用方要判断会话 `kind`，主聊天时**不要**把 `Task` 写盘。
    pub fn is_persistent(&self) -> bool {
        matches!(self, GrantTier::Task)
    }
}

/// 一条临时授权。
///
/// **禁止写进 `permissions.json`** —— 那是长期策略的地盘，
/// 把临时授权混进去会让"一次 / 一轮"跨重启存活。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    /// 授权作用的路径前缀（**必须已规范化**，同 [`Rule::prefix`]）
    pub prefix: PathBuf,
    /// 授予的最大权限
    pub access: Access,
    /// 档位（时间维度）
    pub tier: GrantTier,
    /// 剩余可用次数。
    /// `None` = 不限次（`Turn` / `Task` 档）；`Some(n)` = 还能用 n 次（`Once` 档，初值 1）
    #[serde(default)]
    pub remaining: Option<u32>,
    /// 模型填的申请原因（展示用）
    #[serde(default)]
    pub reason: String,
    /// 批准时间（epoch 毫秒，展示用）
    #[serde(default)]
    pub granted_at: u64,
}

#[allow(dead_code)]
impl Grant {
    /// 造一条「一次」授权（剩余 1 次）
    pub fn once(prefix: impl AsRef<Path>, access: Access, reason: impl Into<String>) -> Self {
        Self {
            prefix: best_effort_canonicalize(prefix.as_ref()),
            access,
            tier: GrantTier::Once,
            remaining: Some(1),
            reason: reason.into(),
            granted_at: now_ms(),
        }
    }

    /// 造一条「一轮」/「整个任务」授权（不限次）
    pub fn lasting(
        prefix: impl AsRef<Path>,
        access: Access,
        tier: GrantTier,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            prefix: best_effort_canonicalize(prefix.as_ref()),
            access,
            tier,
            remaining: None,
            reason: reason.into(),
            granted_at: now_ms(),
        }
    }

    /// 是否还有剩余次数（`None` 视为无限）
    pub fn has_left(&self) -> bool {
        self.remaining.map(|n| n > 0).unwrap_or(true)
    }
}

/// 当前时间（epoch 毫秒）。`perm_request` 也用它给授权/待办打时间戳。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 临时授权表。
///
/// **生命周期由调用方驱动**，本结构不自带定时器：
///   - 一次 `loop` 结束 → `clear_tier(GrantTier::Turn)`
///   - 切换 / 删除会话 → `clear_tier(GrantTier::Task)`
///   - 进程退出 → `Once` / `Turn` 本就在内存里，自然消失
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct GrantStore {
    grants: Vec<Grant>,
}

#[allow(dead_code)]
impl GrantStore {
    pub fn new() -> Self {
        Self { grants: Vec::new() }
    }

    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// 是否没有任何授权。**仅供测试** —— 生产侧读 [`grants`](Self::grants)
    /// 自己判空即可，不必多一个包装。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// 加一条授权。
    ///
    /// 合并规则：**同前缀 + 同档位**（且都不是 `Once`）合并成一条，权限取高的。
    /// `Once` 不合并 —— 每条 `Once` 就是一次配额，叠两条就是两次。
    pub fn add(&mut self, grant: Grant) {
        if grant.tier != GrantTier::Once {
            if let Some(existing) = self
                .grants
                .iter_mut()
                .find(|g| g.tier == grant.tier && g.prefix == grant.prefix)
            {
                if grant.access > existing.access {
                    existing.access = grant.access;
                }
                if !grant.reason.is_empty() {
                    existing.reason = grant.reason;
                }
                return;
            }
        }
        self.grants.push(grant);
    }

    /// 整体替换（从 `*.grants.json` 加载时用）
    pub fn replace(&mut self, grants: Vec<Grant>) {
        self.grants = grants;
    }

    /// 命中判定（**不消费**）：返回「最长前缀且权限足够」的授权下标。
    ///
    /// 返回下标而不是引用 —— 调用方拿到它之后还要 `consume`，借用会打架。
    pub fn find(&self, canon: &Path, need: Access) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None; // (下标, 前缀组件数)

        for (i, g) in self.grants.iter().enumerate() {
            if !g.has_left() || g.access < need {
                continue;
            }
            if !path_has_prefix(canon, &g.prefix) {
                continue;
            }
            let n = g.prefix.components().count();
            match best {
                Some((_, bn)) if bn >= n => {}
                _ => best = Some((i, n)),
            }
        }

        best.map(|(i, _)| i)
    }

    /// 消费一次。`Once` 档扣 1，扣到 0 就移出表；其余档位不动。
    ///
    /// 返回被消费掉的授权副本（`Once` 用尽时返回 `Some`，用于日志/审计）。
    pub fn consume(&mut self, idx: usize) -> Option<Grant> {
        let g = self.grants.get_mut(idx)?;
        match g.remaining {
            None => None,
            Some(n) if n <= 1 => {
                let used = g.clone();
                self.grants.remove(idx);
                Some(used)
            }
            Some(n) => {
                g.remaining = Some(n - 1);
                None
            }
        }
    }

    /// 清掉某一档的全部授权，返回清掉的条数。
    pub fn clear_tier(&mut self, tier: GrantTier) -> usize {
        let before = self.grants.len();
        self.grants.retain(|g| g.tier != tier);
        before - self.grants.len()
    }

    /// 按前缀撤销（设置界面用），可限定档位
    pub fn revoke(&mut self, prefix: impl AsRef<Path>, tier: Option<GrantTier>) -> usize {
        let target = best_effort_canonicalize(prefix.as_ref());
        let before = self.grants.len();
        self.grants
            .retain(|g| !(g.prefix == target && tier.map(|t| t == g.tier).unwrap_or(true)));
        before - self.grants.len()
    }
}

// ---------------------------------------------------------------------------
// 授权判定的完整结果
// ---------------------------------------------------------------------------

/// 放行结果。
///
/// 比裸 `PathBuf` 多一个 [`Allowed::grant_idx`]：调用方靠它区分
/// 「规则长期放行」和「临时授权放行」，后者用完要 `GrantStore::consume` 扣次数。
#[derive(Debug, Clone)]
pub struct Allowed {
    /// 规范化后的路径（**后续所有文件操作都该用这个**）
    pub path: PathBuf,
    /// 命中的临时授权下标；`None` = 由规则放行。
    /// 调用方靠它决定「要不要 `GrantStore::consume` 扣次数」—— 现在还没人读，
    /// 工具层接线后立刻会成为必读字段。
    #[allow(dead_code)]
    pub grant_idx: Option<usize>,
}

impl PermissionGate {
    /// 带临时授权的判定 —— **唯一**的文件操作授权入口。
    ///
    /// 与 [`PermissionGate::check`] 的唯一区别是多看一层 `grants`。优先级：
    ///
    /// | 情况 | 结果 | 为什么 |
    /// |---|---|---|
    /// | 授权前缀**更长** | 授权放行 | 长的优先，与规则同一套逻辑 |
    /// | 前缀**长度相同** | **授权 > 规则** | 否则"Deny 也能申请"这条决策形同虚设 |
    /// | 授权前缀更短 | 规则说了算 | 所以 `D:\` 的授权**压不住** `D:\secret` 的 Deny 规则 —— 不能因为批了个上层目录就顺手解开红线 |
    ///
    /// 注意 `grants` 里只有**权限足够**的才参与竞争；不够的会让位给规则报错，
    /// 这样错误信息仍能准确指向"是哪条规则不够权限"。
    pub fn authorize(
        &self,
        path: impl AsRef<Path>,
        need: Access,
        grants: &[Grant],
    ) -> Result<Allowed, DenyReason> {
        let raw = path.as_ref();
        let canon = self.canonicalize(raw)?;

        // --- 候选 1：临时授权（只收下权限足够的）---
        let mut best_grant: Option<(usize, usize)> = None; // (下标, 组件数)
        for (i, g) in grants.iter().enumerate() {
            if g.access < need || !path_has_prefix(&canon, &g.prefix) {
                continue;
            }
            let n = g.prefix.components().count();
            match best_grant {
                Some((_, bn)) if bn >= n => {}
                _ => best_grant = Some((i, n)),
            }
        }

        // --- 候选 2：规则（取最长前缀，**不看权限够不够** —— Deny 必须能被看见）---
        let mut best_rule: Option<(usize, usize)> = None;
        for (i, r) in self.rules.iter().enumerate() {
            if !path_has_prefix(&canon, &r.prefix) {
                continue;
            }
            let n = r.prefix.components().count();
            match best_rule {
                Some((_, bn)) if bn >= n => {}
                _ => best_rule = Some((i, n)),
            }
        }

        // --- 比长短 ---
        match (best_grant, best_rule) {
            // 授权命中且不短于规则 → 授权放行（等长时"授权 > 规则"）
            (Some((gi, gn)), Some((_, rn))) if gn >= rn => {
                return Ok(Allowed {
                    path: canon,
                    grant_idx: Some(gi),
                })
            }
            (Some((gi, _)), None) => {
                return Ok(Allowed {
                    path: canon,
                    grant_idx: Some(gi),
                })
            }
            _ => {}
        }

        // --- 规则说了算 ---
        match best_rule {
            Some((ri, _)) => {
                let rule = &self.rules[ri];
                if rule.access == Access::Deny {
                    return Err(DenyReason::ExplicitlyDenied {
                        path: canon,
                        rule_prefix: rule.prefix.clone(),
                        rule_label: rule.label.clone(),
                    });
                }
                if rule.access >= need {
                    return Ok(Allowed {
                        path: canon,
                        grant_idx: None,
                    });
                }
                Err(DenyReason::InsufficientAccess {
                    path: canon,
                    rule_prefix: rule.prefix.clone(),
                    rule_label: rule.label.clone(),
                    required: need,
                    allowed: rule.access,
                })
            }
            None => Err(DenyReason::NoMatchingRule { path: canon }),
        }
    }

    /// 把路径规范化成判定用的绝对形式（`authorize` 内部用，单独暴露给上层复用）
    fn canonicalize(&self, raw: &Path) -> Result<PathBuf, DenyReason> {
        let canon = if raw.is_absolute() {
            best_effort_canonicalize(raw)
        } else {
            match std::env::current_dir() {
                Ok(cwd) => best_effort_canonicalize(&cwd.join(raw)),
                Err(e) => {
                    return Err(DenyReason::BadPath {
                        path: raw.to_path_buf(),
                        detail: format!("无法获取工作目录: {e}"),
                    })
                }
            }
        };

        if canon.as_os_str().is_empty() {
            return Err(DenyReason::BadPath {
                path: raw.to_path_buf(),
                detail: "规范化后为空".into(),
            });
        }
        Ok(canon)
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> PermissionGate {
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Read, "D盘根");
        g.add_rule(r"D:\myword", Access::ReadWrite, "D:\\myword");
        g.add_rule(r"D:\myword\agent-data", Access::Full, "agent 数据");
        g
    }

    #[test]
    fn d_drive_is_read_only() {
        let g = gate();
        assert!(g.check(r"D:\mycode\x.txt", Access::Read).is_ok());
        assert!(matches!(
            g.check(r"D:\mycode\x.txt", Access::ReadWrite),
            Err(DenyReason::InsufficientAccess { .. })
        ));
    }

    #[test]
    fn longest_prefix_wins() {
        let g = gate();
        // D:\myword 是读写
        assert!(g.check(r"D:\myword\a.md", Access::ReadWrite).is_ok());
        // D:\myword\agent-data 是完全控制，应该覆盖上面的规则
        assert!(g.check(r"D:\myword\agent-data\m.json", Access::Full).is_ok());
    }

    #[test]
    fn sibling_prefix_must_not_match() {
        // 关键：D:\myword 不能匹配 D:\mywordx
        let g = gate();
        let r = g.check(r"D:\mywordx\secret.txt", Access::ReadWrite);
        assert!(matches!(r, Err(DenyReason::InsufficientAccess { .. })),
                "D:\\mywordx 应落到 D:\\ 的只读规则，而不是 D:\\myword 的读写规则；实际: {r:?}");
    }

    #[test]
    fn path_traversal_is_resolved() {
        let g = gate();
        // D:\myword\..\mycode 应被解析为 D:\mycode → 只读
        let r = g.check(r"D:\myword\..\mycode\x.txt", Access::ReadWrite);
        assert!(matches!(r, Err(DenyReason::InsufficientAccess { .. })),
                "路径穿越应被规范化后拒绝；实际: {r:?}");
    }

    #[test]
    fn case_insensitive_on_windows() {
        let g = gate();
        assert!(g.check(r"D:\MYWORD\a.md", Access::ReadWrite).is_ok());
        assert!(g.check(r"d:\MyWord\a.md", Access::ReadWrite).is_ok());
    }

    #[test]
    fn outside_rules_is_denied() {
        let g = gate();
        assert!(matches!(
            g.check(r"C:\Windows\system32\drivers\etc\hosts", Access::Read),
            Err(DenyReason::NoMatchingRule { .. })
        ));
    }

    #[test]
    fn nonexistent_file_under_allowed_dir() {
        // 待创建的文件：路径不存在，但应能规范化并授权
        let g = gate();
        let r = g.check(r"D:\myword\brand-new-file-xyz.md", Access::ReadWrite);
        assert!(r.is_ok(), "应允许在 D:\\myword 下创建新文件；实际: {r:?}");
    }

    #[test]
    fn component_prefix_equality() {
        assert!(path_has_prefix(Path::new(r"D:\a\b"), Path::new(r"D:\a")));
        assert!(path_has_prefix(Path::new(r"D:\a"), Path::new(r"D:\a")));
        assert!(!path_has_prefix(Path::new(r"D:\ab"), Path::new(r"D:\a")));
        assert!(!path_has_prefix(Path::new(r"D:\a"), Path::new(r"D:\a\b")));
    }

    #[test]
    fn strip_verbatim_prefix() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\D:\myword")),
            PathBuf::from(r"D:\myword")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\server\share")
        );
    }

    // ---- Deny 级别 ----

    #[test]
    fn deny_beats_broader_allow() {
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Read, "D盘");
        g.add_rule(r"D:\secret", Access::Deny, "敏感目录");

        // 普通路径放行
        assert!(g.check(r"D:\myword\a.txt", Access::Read).is_ok());

        // 被 deny 的子树即使只读也应拒绝（最长前缀命中 Deny）
        let e = g.check(r"D:\secret\x.txt", Access::Read).unwrap_err();
        assert!(
            matches!(e, DenyReason::ExplicitlyDenied { .. }),
            "应是显式拒绝而非权限不足，实际 {e:?}"
        );
    }

    #[test]
    fn deny_is_not_reported_as_insufficient() {
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Full, "D盘");
        g.add_rule(r"D:\locked", Access::Deny, "锁住");

        let e = g.check(r"D:\locked\f", Access::Read).unwrap_err();
        // 关键：必须报 ExplicitlyDenied —— 若走 `>=` 比较会误报 InsufficientAccess
        assert!(matches!(e, DenyReason::ExplicitlyDenied { .. }));
    }

    // ---- 持久化 ----

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("orbcat_perm_persist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut g = PermissionGate::new();
        g.add_rule(&dir, Access::Full, "测试目录");
        g.add_rule(r"D:\", Access::Deny, "示例拒绝");

        let p = save_gate(&dir, &g).unwrap();
        assert!(p.exists(), "应写出 permissions.json");

        let loaded = load_gate(&dir, &dir);
        assert_eq!(loaded.rules().len(), 2);

        // 拒绝规则应被还原
        assert!(matches!(
            loaded.check(r"D:\a", Access::Read),
            Err(DenyReason::ExplicitlyDenied { .. })
        ));
        // 放行规则应被还原
        assert!(loaded.check(dir.join("f"), Access::Read).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_generates_file_on_first_run() {
        let dir = std::env::temp_dir().join("orbcat_perm_firstrun");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!rules_path(&dir).exists());
        let g = load_gate(&dir, &dir);
        // 首次运行应生成文件（让用户看得见、改得动）
        assert!(rules_path(&dir).exists(), "首次运行应生成 permissions.json");
        assert!(!g.rules().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_rules_respects_deny() {
        let mut g = PermissionGate::new();
        let rules = vec![
            Rule {
                prefix: PathBuf::from(r"D:\"),
                access: Access::ReadWrite,
                label: "用户改的".into(),
            },
            Rule {
                prefix: PathBuf::from(r"D:\no"),
                access: Access::Deny,
                label: "用户禁的".into(),
            },
        ];
        g.replace_rules(&rules);
        assert_eq!(g.rules().len(), 2);
        assert!(g.check(r"D:\x", Access::ReadWrite).is_ok());
        assert!(g.check(r"D:\no\y", Access::Read).is_err());
    }

    // ---- 临时授权层（「申请权限」）----

    #[test]
    fn grant_wins_over_deny_rule_at_same_prefix() {
        // 决策 6 的核心：Deny 也能申请 —— 批准后**必须**能压过 Deny 规则，
        // 否则「所有拒绝一视同仁都能申请」这条决策形同虚设。
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Read, "D盘");
        g.add_rule(r"D:\private", Access::Deny, "我设的红线");

        // 无授权时：显式拒绝
        assert!(matches!(
            g.check(r"D:\private\x.txt", Access::Read),
            Err(DenyReason::ExplicitlyDenied { .. })
        ));

        // 同前缀授权 → 授权赢
        let grants = vec![Grant::lasting(
            r"D:\private",
            Access::Read,
            GrantTier::Turn,
            "用户批了",
        )];
        let r = g.authorize(r"D:\private\x.txt", Access::Read, &grants);
        assert!(r.is_ok(), "等长前缀时授权必须压过 Deny 规则；实际 {r:?}");
        assert_eq!(r.unwrap().grant_idx, Some(0));
    }

    #[test]
    fn longer_deny_rule_beats_shorter_grant() {
        // 反向保护：批了上层目录**不能**顺手解开下层红线。
        // 这是"授权 > 规则"唯一不该越界的地方。
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Read, "D盘");
        g.add_rule(r"D:\a\secret", Access::Deny, "红线");

        let grants = vec![Grant::lasting(
            r"D:\a",
            Access::ReadWrite,
            GrantTier::Turn,
            "批了 a 目录",
        )];
        let r = g.authorize(r"D:\a\secret\x", Access::ReadWrite, &grants);
        assert!(
            matches!(r, Err(DenyReason::ExplicitlyDenied { .. })),
            "Deny 规则前缀更长时必须仍然拒绝；实际 {r:?}"
        );
    }

    #[test]
    fn longer_grant_beats_shorter_rule() {
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Read, "D盘只读");

        let grants = vec![Grant::lasting(
            r"D:\work",
            Access::ReadWrite,
            GrantTier::Turn,
            "要写东西",
        )];

        let ok = g.authorize(r"D:\work\a.md", Access::ReadWrite, &grants);
        assert!(ok.is_ok(), "授权前缀更长应放行；实际 {ok:?}");
        assert_eq!(ok.unwrap().grant_idx, Some(0));

        // 授权范围之外照旧被规则拒 —— 授权不是全局开关
        assert!(matches!(
            g.authorize(r"D:\other\a.md", Access::ReadWrite, &grants),
            Err(DenyReason::InsufficientAccess { .. })
        ));
    }

    #[test]
    fn insufficient_grant_falls_through_to_rule() {
        // 权限不够的授权**不参与竞争** —— 这样错误信息仍准确指向"是哪条规则不够"
        let mut g = PermissionGate::new();
        g.add_rule(r"D:\", Access::Read, "D盘只读");

        let grants = vec![Grant::lasting(
            r"D:\work",
            Access::Read,
            GrantTier::Turn,
            "只批了读",
        )];
        let r = g.authorize(r"D:\work\a.md", Access::ReadWrite, &grants);
        assert!(
            matches!(r, Err(DenyReason::InsufficientAccess { .. })),
            "实际 {r:?}"
        );
    }

    #[test]
    fn check_without_grants_is_unchanged() {
        // 收敛到 authorize() 之后，不带授权表的行为必须与旧实现完全一致
        let g = gate();
        assert!(g.check(r"D:\myword\a.md", Access::ReadWrite).is_ok());
        assert!(matches!(
            g.check(r"D:\mycode\x.txt", Access::ReadWrite),
            Err(DenyReason::InsufficientAccess { .. })
        ));
        assert!(matches!(
            g.check(r"D:\mywordx\s.txt", Access::ReadWrite),
            Err(DenyReason::InsufficientAccess { .. })
        ));
    }

    #[test]
    fn once_grant_is_consumed_and_removed() {
        let mut store = GrantStore::new();
        store.add(Grant::once(r"D:\work", Access::ReadWrite, "写一个文件"));
        assert_eq!(store.grants().len(), 1);
        assert_eq!(store.grants()[0].remaining, Some(1));

        assert_eq!(
            store.find(Path::new(r"D:\work\a.md"), Access::ReadWrite),
            Some(0)
        );
        // 范围外不该命中
        assert_eq!(store.find(Path::new(r"D:\other\a.md"), Access::ReadWrite), None);
        // 权限不够不该命中
        assert_eq!(store.find(Path::new(r"D:\work\a.md"), Access::Full), None);

        assert!(store.consume(0).is_some(), "用尽应返回被消费的授权");
        assert!(store.is_empty(), "用尽后应从表里移除 —— 这就是「一次」");
    }

    #[test]
    fn clear_tier_only_removes_that_tier() {
        let mut store = GrantStore::new();
        store.add(Grant::once(r"D:\a", Access::Read, ""));
        store.add(Grant::lasting(r"D:\b", Access::Read, GrantTier::Turn, ""));
        store.add(Grant::lasting(r"D:\c", Access::Read, GrantTier::Task, ""));

        assert_eq!(store.clear_tier(GrantTier::Turn), 1);
        let tiers: Vec<GrantTier> = store.grants().iter().map(|g| g.tier).collect();
        assert_eq!(
            tiers,
            vec![GrantTier::Once, GrantTier::Task],
            "只该清掉 Turn 档，Once / Task 不受影响"
        );
    }

    #[test]
    fn same_prefix_non_once_grants_coalesce() {
        let mut store = GrantStore::new();
        store.add(Grant::lasting(r"D:\a", Access::Read, GrantTier::Turn, "第一次"));
        store.add(Grant::lasting(r"D:\a", Access::ReadWrite, GrantTier::Turn, "第二次"));
        assert_eq!(store.grants().len(), 1, "同前缀同档位应合并");
        assert_eq!(store.grants()[0].access, Access::ReadWrite, "合并取权限高的");

        // Once **不合并** —— 每条 Once 就是一次配额，叠两条就是两次
        store.add(Grant::once(r"D:\b", Access::Read, ""));
        store.add(Grant::once(r"D:\b", Access::Read, ""));
        assert_eq!(store.grants().len(), 3);
    }

    #[test]
    fn tier_and_grant_json_roundtrip() {
        for t in [GrantTier::Once, GrantTier::Turn, GrantTier::Task] {
            assert_eq!(GrantTier::parse(t.as_str()), Some(t));
        }
        assert!(GrantTier::Task.is_persistent(), "只有「整个任务」落盘");
        assert!(!GrantTier::Once.is_persistent());
        assert!(!GrantTier::Turn.is_persistent());

        let g = Grant::lasting(r"D:\work", Access::ReadWrite, GrantTier::Task, "批量重命名");
        let txt = serde_json::to_string(&g).unwrap();
        let back: Grant = serde_json::from_str(&txt).unwrap();
        assert_eq!(back.prefix, g.prefix);
        assert_eq!(back.access, Access::ReadWrite);
        assert_eq!(back.tier, GrantTier::Task);
        assert_eq!(back.reason, "批量重命名");
        assert_eq!(back.remaining, None, "无限次档位 remaining 为 None");
    }
}
