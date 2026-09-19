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

    /// 判定某路径是否满足所需权限。
    ///
    /// 这是**唯一**的授权入口 —— 所有文件操作（读/写/删）都必须先过这里。
    pub fn check(&self, path: impl AsRef<Path>, need: Access) -> Result<PathBuf, DenyReason> {
        let raw = path.as_ref();

        let canon = if raw.is_absolute() {
            best_effort_canonicalize(raw)
        } else {
            // 相对路径：拼上当前工作目录再规范化
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

        // 规则已按前缀长度降序 —— 第一个命中的就是最长前缀
        for rule in &self.rules {
            if !path_has_prefix(&canon, &rule.prefix) {
                continue;
            }

            // Deny 必须特判：它在枚举里排最前，若走 `>=` 比较会被误判成"权限不足"
            if rule.access == Access::Deny {
                return Err(DenyReason::ExplicitlyDenied {
                    path: canon,
                    rule_prefix: rule.prefix.clone(),
                    rule_label: rule.label.clone(),
                });
            }

            if rule.access >= need {
                return Ok(canon);
            }
            return Err(DenyReason::InsufficientAccess {
                path: canon,
                rule_prefix: rule.prefix.clone(),
                rule_label: rule.label.clone(),
                required: need,
                allowed: rule.access,
            });
        }

        Err(DenyReason::NoMatchingRule { path: canon })
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
fn strip_verbatim(p: PathBuf) -> PathBuf {
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
        tmp.join("float-agent"),
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
                        "[float-agent] ⚠️ {} 解析失败（{e}），本次改用自举规则（文件未改动）",
                        path.display()
                    );
                }
            },
            Err(e) => {
                eprintln!("[float-agent] ⚠️ 读取 {} 失败（{e}），改用自举规则", path.display());
            }
        }
    }

    let gate = bootstrap_gate(data_dir, app_dir);
    match save_gate(data_dir, &gate) {
        Ok(p) => eprintln!("[float-agent] 已生成默认权限文件: {}", p.display()),
        Err(e) => eprintln!("[float-agent] ⚠️ 写权限文件失败: {e}"),
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
        let dir = std::env::temp_dir().join("float_agent_perm_persist");
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
        let dir = std::env::temp_dir().join("float_agent_perm_firstrun");
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
}
