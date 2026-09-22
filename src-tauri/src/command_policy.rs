//! 命令策略 —— **动作级**权限判定（与文件级 [`crate::permission::PermissionGate`] 并列）
//!
//! ## 为什么不能塞进文件网关
//!
//! 文件网关的 key 是**路径**（"这个目录能不能读"）；命令的 key 是**命令指纹**
//! （"这条命令要不要人工确认"）。两者维度不同，硬塞进同一张表只会得到假安全感。
//!
//! ## 三路判定（决策 9）
//!
//! ```text
//!                 ┌─ 硬阻断（黑名单/结构化危险）→ 永远拒绝，连问都不问
//!   一条命令 ──────┼─ 白名单免问（读类命令）      → 直接放行
//!                 └─ 其余                        → 走确认卡（用户当场拍板）
//! ```
//!
//! 取最严：任一段命中硬阻断 → 整条命令硬阻断（most-restrictive-wins）。
//!
//! ## shell 壳内联命令递归（2026-09-22 调整）
//!
//! agent 习惯把命令包一层 shell（`pwsh -Command "rg foo"`）。旧逻辑对 shell spawner
//! **一律拒绝**，导致"只读命令套个壳也弹卡"。现在改为：能抽出内联命令就**递归评估**，
//! 结论取内层（`pwsh -c "Get-ChildItem"` → 放行；`pwsh -c "shutdown"` → 硬阻断）；
//! 抽不出内联命令（`-File x.ps1`）才降级为确认。深度上限 [`MAX_SHELL_DEPTH`]。
//!
//! ## PowerShell 的绕过面（为什么必须归一化）
//!
//! 纯字符串匹配形同虚设，因为同一条删除命令有无穷多种写法：
//!
//! | 写法 | 绕过面 |
//! |---|---|
//! | `rm -r -fo D:\x` | 别名（`rm`→`Remove-Item`）+ 参数缩写（`-fo`→`-Force`） |
//! | `& (gcm R*-It*) ...` | 动态解析（`gcm`/`gal` 取命令对象再调用） |
//! | `Remove-Item 'D:\'+'x' -R -F` | 字符串拼接构造参数 |
//! | `powershell -EncodedCommand <base64>` | **base64 UTF-16LE，文本匹配完全失效** |
//!
//! 本模块的归一化是**静态**的（不起 PowerShell 子进程，逐条命令起进程太慢）：
//! 拆复合命令 → 逐个片段 tokenize → 展开别名/缩写 → 检测动态解析与混淆。
//! **归一化不可信（检测到混淆）→ 一律降级为确认（fail closed）**；
//! `-EncodedCommand` 直接硬阻断（设计决策，不作商量）。
//!
//! ## 授权（命令指纹，非路径）
//!
//! 用户批准时给两种粒度（对齐 Pi 的 Allow Always / cc-safety-net 的语义拦截）：
//! - [`CmdScope::Prefix`]：记住「可执行名 + 首个子命令」，如 `git status`（覆盖 `git status -s`）
//! - [`CmdScope::Full`]：记住**完整命令**（最窄）
//!
//! **永久授权落盘**（2026-09-22 用户拍板；这是与文件权限刻意拉开的一处差异）：
//!
//! - 「仅这一次」（`Once`）**不落盘** —— 落盘就跨重启存活，"一次"语义当场破功
//!   （同 `permission.rs` 里 Once/Turn 不落盘的硬约束）；
//! - 「记住这类」/「记住这条」（`Prefix` / `Full`）写入 `<data_dir>/command_grants.json`，
//!   **跨会话、跨重启存活**，只有用户主动撤销（或删文件）才失效。
//!
//! 与 Pi 的 Allow Always 的差别正在这里：Pi 的「记住」是会话级，本项目是**永久**的。
//! ⚠️ 文件权限（[`crate::permission`]）**不受本变更影响** —— 仍是
//! 「Task 档跟随会话落盘 + 主聊天降级为内存 + Once/Turn 不落盘」。

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::permission::{now_ms, GrantTier};

// ---------------------------------------------------------------------------
// 风险等级
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(&self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }

    pub fn from_str(s: &str) -> Risk {
        match s.to_ascii_lowercase().as_str() {
            "high" | "高" => Risk::High,
            "medium" | "中" => Risk::Medium,
            _ => Risk::Low,
        }
    }
}

// ---------------------------------------------------------------------------
// 判定结果
// ---------------------------------------------------------------------------

/// 一条命令的策略判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdVerdict {
    /// 硬阻断：命中危险模式，**永远拒绝**（用户也批不了，只能手工执行）
    HardBlock { reason: String },
    /// 白名单免问：直接放行
    Allow { reason: String },
    /// 需要用户确认（含风险等级，卡片据此标色）
    Ask { reason: String, risk: Risk },
}

// ---------------------------------------------------------------------------
// 归一化
// ---------------------------------------------------------------------------

/// 默认别名表（PowerShell 默认别名，子集）。
///
/// 为什么不查运行时别名表：那要起子进程（`gal`），逐条命令起进程太慢。
/// 内置一张常见表覆盖 99% 的场景；表外的别名无法展开 → 会落到"需确认"（fail closed），
/// 不会误放行。
const ALIASES: &[(&str, &str)] = &[
    // 删除家族 —— 最要紧的一族，全归并到 remove-item
    ("rm", "remove-item"),
    ("ri", "remove-item"),
    ("rd", "remove-item"),
    ("rmdir", "remove-item"),
    ("del", "remove-item"),
    ("erase", "remove-item"),
    // 读取
    ("ls", "get-childitem"),
    ("dir", "get-childitem"),
    ("gci", "get-childitem"),
    ("cat", "get-content"),
    ("gc", "get-content"),
    ("type", "get-content"),
    ("pwd", "get-location"),
    ("gl", "get-location"),
    ("cd", "set-location"),
    ("chdir", "set-location"),
    ("sl", "set-location"),
    // 复制 / 移动 / 新建
    ("cp", "copy-item"),
    ("cpi", "copy-item"),
    ("copy", "copy-item"),
    ("mv", "move-item"),
    ("mi", "move-item"),
    ("move", "move-item"),
    ("ni", "new-item"),
    // 进程
    ("ps", "get-process"),
    ("gps", "get-process"),
    ("kill", "stop-process"),
    ("spps", "stop-process"),
    // 写出
    ("echo", "write-output"),
    ("write", "write-output"),
    ("sc", "set-content"),
    ("ac", "add-content"),
    // 动态调用 / 反射（危险）
    ("iex", "invoke-expression"),
    ("icm", "invoke-command"),
    ("gcm", "get-command"),
    ("gal", "get-alias"),
    ("where", "where-object"),
    ("%", "foreach-object"),
    ("foreach", "foreach-object"),
    // 网络
    ("curl", "invoke-webrequest"),
    ("iwr", "invoke-webrequest"),
    ("wget", "invoke-webrequest"),
    // 选择 / 排序
    ("select", "select-object"),
    ("sort", "sort-object"),
    ("measure", "measure-object"),
    ("gm", "get-member"),
];

/// 参数缩写归一化 —— PowerShell 的**唯一前缀匹配**允许 `-r` / `-rec` / `-recur` 等
/// 都指 `-Recurse`，`-f` / `-fo` / `-force` 都指 `-Force`。这里把它们统一展开成全称，
/// 好让"递归强制删除"能被结构化识别出来。
///
/// 为什么不担心 `-f`/`-r` 的歧义（如 `-Filter`）：
/// 展开只会把参数**往"更强"的方向**归一，永不把"需确认"变成"放行"；
/// 最坏结果是少数本该确认的命令多带了个 `-force`/`-recurse` 字样 —— fail closed，可接受。
/// 另外 PowerShell **不支持** `-rf` 这种合并短开关（会报参数找不到），
/// 所以真实危险形态是 `-r -f` / `-Recurse -Force` 这类**两 token** 写法。
const PARAM_ALIASES: &[(&str, &str)] = &[
    ("-f", "-force"),
    ("-fo", "-force"),
    ("-for", "-force"),
    ("-forc", "-force"),
    ("-r", "-recurse"),
    ("-re", "-recurse"),
    ("-rec", "-recurse"),
    ("-recu", "-recurse"),
    ("-recur", "-recurse"),
];

/// 会派生子进程/重新解析命令串的可执行名。
///
/// **不再一律拒绝**（2026-09-22 调整）：若内联命令（`pwsh -Command "..."` / `cmd /c ...`）
/// 能被抽出，就**递归评估内层命令**，按内层结论放行 / 确认 / 阻断。这样 agent 习惯性把
/// 只读命令包一层 shell 时不再必弹卡，而 `pwsh -c "shutdown"` 依旧被拦（内层命中硬阻断）。
/// 只有抽不出内联命令（`-File script.ps1`、纯交互、无参数）时才降级为确认。
const SHELL_SPAWNERS: &[&str] = &[
    "cmd", "cmd.exe", "powershell", "powershell.exe", "pwsh", "pwsh.exe",
    "bash", "bash.exe", "wsl", "wsl.exe", "sh", "sh.exe",
];

/// shell 内联命令递归展开的最大深度（防 `pwsh -c "pwsh -c ..."` 无限递归）。
const MAX_SHELL_DEPTH: usize = 4;

/// 一段（复合命令拆出来的一个子命令）。
#[derive(Debug, Clone)]
pub struct Segment {
    /// 归一化后的命令名（别名已展开、小写）
    pub exe: String,
    /// 归一化后的参数（小写、缩写已展开）
    pub args: Vec<String>,
    /// 原始片段（展示用）
    pub raw: String,
}

impl Segment {
    /// 完整归一化形式：`exe arg arg`
    pub fn normalized(&self) -> String {
        if self.args.is_empty() {
            self.exe.clone()
        } else {
            format!("{} {}", self.exe, self.args.join(" "))
        }
    }

    /// 「可执行名 + 首个子命令」—— 授权前缀粒度用。
    ///
    /// 首个子命令 = 第一个**不像路径/开关**的参数。取不到就退化成裸命令名。
    /// 例：`git status -s` → `git status`；`npm install lodash` → `npm install`；
    /// `rm -rf D:\x` → `rm`（参数是路径/开关，不算子命令）。
    pub fn head(&self) -> String {
        for a in &self.args {
            if looks_like_flag(a) || looks_like_path(a) {
                continue;
            }
            return format!("{} {}", self.exe, a);
        }
        self.exe.clone()
    }

    /// 是否含某开关（归一化后）
    fn has_flag(&self, f: &str) -> bool {
        self.args.iter().any(|a| a == f)
    }
}

fn looks_like_flag(s: &str) -> bool {
    s.starts_with('-') || s.starts_with('/')
}

fn looks_like_path(s: &str) -> bool {
    s.contains('\\') || s.contains('/') || (s.len() == 2 && s.ends_with(':'))
}

/// 归一化结果。
#[derive(Debug, Clone)]
pub struct Normalized {
    pub raw: String,
    pub segments: Vec<Segment>,
    /// 归一化是否**不可信**（检测到混淆/动态解析）→ 必须降级为确认
    pub obfuscated: bool,
    pub obfuscation: Option<String>,
    /// 是否命中 `-EncodedCommand`（直接硬阻断）
    pub encoded: bool,
}

impl Normalized {
    /// 所有片段的完整归一化串（指纹用）
    pub fn full(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.normalized())
            .collect::<Vec<_>>()
            .join(" ; ")
    }

    /// 首片段的 head（"记住名+首子命令"用）
    pub fn head(&self) -> String {
        self.segments
            .first()
            .map(|s| s.head())
            .unwrap_or_default()
    }
}

/// 归一化一条命令（不做判定，纯解析）。
pub fn normalize(raw: &str) -> Normalized {
    let raw_trim = raw.trim();

    // ① `-EncodedCommand` —— base64 UTF-16LE，文本匹配完全失效 → 直接标记硬阻断
    if contains_encoded_command(raw_trim) {
        return Normalized {
            raw: raw.to_string(),
            segments: Vec::new(),
            obfuscated: true,
            obfuscation: Some("EncodedCommand（base64 编码命令，无法审查）".into()),
            encoded: true,
        };
    }

    // ② 动态解析 / 字符串拼接 / 调用运算符 —— 归一化不可信
    let obfuscation = detect_obfuscation(raw_trim);

    // ③ 拆复合命令（`;` `&&` `||` `|` 换行），逐段解析
    let parts = split_segments(raw_trim);
    let mut segments = Vec::new();
    for p in parts {
        if let Some(s) = parse_segment(&p) {
            segments.push(s);
        }
    }

    Normalized {
        raw: raw.to_string(),
        segments,
        obfuscated: obfuscation.is_some(),
        obfuscation,
        encoded: false,
    }
}

/// 检出 `-EncodedCommand` 及其别名（`-e` / `-ec` / `-enc` / `-encoded`）。
///
/// ⚠️ 故意把 `-e` 也纳入 —— 它是 `-EncodedCommand` 的官方别名。
/// 代价是某些工具恰好用 `-e` 当自己的开关会被误硬阻断；安全性优先，可接受。
fn contains_encoded_command(s: &str) -> bool {
    const NAMES: &[&str] = &[
        "encodedcommand", "encoded", "enc", "ec", "e",
    ];
    for tok in s.split_whitespace() {
        let t = tok.trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase();
        // 形如 -EncodedCommand / -EncodedCommand:<b64> / -EncodedCommand=<b64>
        let rest = t.strip_prefix('-').unwrap_or(&t);
        let name = rest.split([':', '=']).next().unwrap_or("");
        if !name.is_empty() && NAMES.contains(&name) {
            return true;
        }
    }
    false
}

/// 检测动态解析 / 字符串拼接 / 调用运算符等"归一化不可信"的写法。
fn detect_obfuscation(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();

    // 反引号转义（PS 里 ` 可拆散 token）
    if s.contains('`') {
        return Some("反引号转义（`）可拆散命令 token".into());
    }
    // 子表达式构造命令名
    if lower.contains("$(") {
        // 允许常见的 `$(...)` 作为参数值？保守起见仍视为不可信 —— fail closed
        return Some("子表达式 $(...) 动态构造命令".into());
    }
    // 调用运算符 & / . 后接表达式
    if lower.contains("& (") || lower.contains("&(") || lower.contains(". (") {
        return Some("调用运算符 & / . 动态执行".into());
    }
    // 动态取命令 / 别名
    for kw in ["get-command", "get-alias", "gcm", "gal"] {
        if contains_word(&lower, kw) {
            return Some(format!("动态解析命令名（{kw}）"));
        }
    }
    // 字符串拆解 / 拼接构造
    for kw in ["[char]", "-join", "-replace", "[convert]", "[system.text.encoding]"] {
        if lower.contains(kw) {
            return Some(format!("字符串拆解构造（{kw}）"));
        }
    }
    // Invoke-Expression / iex（把字符串当代码跑）
    for kw in ["invoke-expression", "iex "] {
        if lower.contains(kw) {
            return Some("Invoke-Expression 把字符串当代码执行".into());
        }
    }
    None
}

/// 单词级包含（避免 `gal` 命中 `legal` 之类）
fn contains_word(haystack: &str, needle: &str) -> bool {
    let n = needle.trim();
    if n.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(n) {
        let i = start + pos;
        let before_ok = i == 0 || !is_word_char(bytes[i - 1] as char);
        let end = i + n.len();
        let after_ok = end >= bytes.len() || !is_word_char(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        start = i + n.len();
        if start >= haystack.len() {
            break;
        }
    }
    false
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// 拆复合命令：`;` `&&` `||` `|` 换行 都算分隔符（引号内的不算）。
fn split_segments(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '\n' | '\r' => {
                push_part(&mut parts, &mut cur);
            }
            ';' => {
                push_part(&mut parts, &mut cur);
            }
            '|' => {
                // `||` 也走同一条路
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                push_part(&mut parts, &mut cur);
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    push_part(&mut parts, &mut cur);
                } else {
                    cur.push(c); // 单个 & 可能是调用运算符，交给 obfuscation 检测
                }
            }
            _ => cur.push(c),
        }
    }
    push_part(&mut parts, &mut cur);
    parts
}

fn push_part(parts: &mut Vec<String>, cur: &mut String) {
    let t = cur.trim().to_string();
    if !t.is_empty() {
        parts.push(t);
    }
    cur.clear();
}

/// 解析单个片段成 [`Segment`]；空片段返回 None。
fn parse_segment(part: &str) -> Option<Segment> {
    let tokens = tokenize(part);
    if tokens.is_empty() {
        return None;
    }

    let raw = part.to_string();
    let mut it = tokens.into_iter();

    let mut exe = it.next().unwrap_or_default().to_ascii_lowercase();
    // 调用运算符 `&` / `.` 在段首 → 动态调用，命令名不可信。保留原义供 obfuscation 判定。
    if exe == "&" || exe == "." {
        exe = it.next().unwrap_or_default().to_ascii_lowercase();
    }
    exe = expand_alias(&exe);

    let mut args: Vec<String> = Vec::new();
    for a in it {
        let lower = a.to_ascii_lowercase();
        args.push(expand_param(&lower));
    }

    Some(Segment { exe, args, raw })
}

fn expand_alias(name: &str) -> String {
    ALIASES
        .iter()
        .find(|(a, _)| *a == name)
        .map(|(_, full)| (*full).to_string())
        .unwrap_or_else(|| name.to_string())
}

fn expand_param(p: &str) -> String {
    // 形如 `-fo:` / `-fo=` 的值也一起归一化
    for &(abbr, full) in PARAM_ALIASES {
        if p == abbr {
            return full.to_string();
        }
        if let Some(rest) = p.strip_prefix(abbr) {
            if let Some(sep) = rest.chars().next() {
                if sep == ':' || sep == '=' {
                    return format!("{full}{rest}");
                }
            }
        }
    }
    p.to_string()
}

/// 极简 tokenizer：空白分隔，尊重单/双引号（引号本身剥掉）。
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;

    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// 策略
// ---------------------------------------------------------------------------

/// 命令策略（可持久化到 `command_policy.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPolicy {
    /// 白名单免问（对**归一化后的片段**做通配匹配）
    #[serde(default = "default_allow")]
    pub allow: Vec<String>,
    /// 硬阻断的通配模式（对**归一化后的片段**做通配匹配）
    #[serde(default = "default_hard_block")]
    pub hard_block: Vec<String>,
}

impl Default for CommandPolicy {
    fn default() -> Self {
        Self {
            allow: default_allow(),
            hard_block: default_hard_block(),
        }
    }
}

fn default_allow() -> Vec<String> {
    [
        // git 只读
        "git status", "git status *", "git diff", "git diff *",
        "git log", "git log *", "git show", "git show *",
        "git branch", "git branch *", "git remote", "git remote *",
        "git rev-parse *", "git describe *", "git ls-files *", "git blame *",
        "git --version", "git config --get *", "git config --list *",
        "git grep *", "git tag", "git tag *", "git shortlog *",
        "git stash list *", "git worktree list *",
        // 只读文件/目录
        "get-childitem", "get-childitem *",
        "get-location", "get-item *", "test-path *", "resolve-path *",
        "get-content *",
        // 目录切换（只改 cwd，不执行任何东西）—— 补 `cd`（别名 → set-location）
        "set-location", "set-location *", "push-location *", "pop-location",
        // 只读搜索（补 Unix 只读工具：grep / head / tail 等）
        "select-string *", "findstr *", "rg *", "where.exe *", "where-object *",
        "grep *", "head *", "tail *", "wc *", "nl *", "uniq *", "cut *",
        "basename *", "dirname *", "realpath *", "which *", "type *",
        // 只读系统信息
        "get-process", "get-process *", "get-service", "get-service *",
        "get-command *", "get-help *", "get-member *", "get-date", "get-host",
        "get-computerinfo", "get-ciminstance *",
        "whoami", "hostname", "uname *", "du *", "df *", "stat *", "tree *", "file *",
        // 只读加工
        "measure-object *", "sort-object *", "select-object *",
        // 输出
        "write-output *", "echo *",
        // 包管理器只读查询（`npm config get registry` 这类不该弹卡）
        "npm config get *", "npm ls *", "npm list *", "npm view *", "npm info *",
        "pnpm list *", "pnpm config get *", "yarn list *", "yarn config get *",
        // 版本查询
        "node --version", "npm --version", "pnpm --version", "yarn --version",
        "cargo --version", "rustc --version", "python --version", "pip --version",
        "python3 --version", "py --version", "dotnet --version",
        "pwsh --version", "java -version", "go version",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_hard_block() -> Vec<String> {
    [
        "*format-volume*", "format-volume*",
        "shutdown*", "restart-computer*", "stop-computer*",
        "diskpart*", "*diskpart*",
        "*clear-disk*", "*initialize-disk*",
        "*remove-partition*", "*format *",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 策略文件路径
pub fn policy_path(data_dir: &Path) -> PathBuf {
    data_dir.join("command_policy.json")
}

/// 加载策略：文件存在以文件为准，不存在则用默认并**落盘**（让用户看得见、改得动）。
pub fn load_policy(data_dir: &Path) -> CommandPolicy {
    let path = policy_path(data_dir);
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(txt) => match serde_json::from_str::<CommandPolicy>(&txt) {
                Ok(p) => return p,
                Err(e) => eprintln!(
                    "[float-agent] ⚠️ {} 解析失败（{e}），改用默认命令策略（文件未改动）",
                    path.display()
                ),
            },
            Err(e) => eprintln!("[float-agent] ⚠️ 读 {} 失败（{e}），改用默认命令策略", path.display()),
        }
    }
    let p = CommandPolicy::default();
    if let Err(e) = save_policy(data_dir, &p) {
        eprintln!("[float-agent] ⚠️ 写命令策略文件失败: {e}");
    }
    p
}

/// 保存策略（整体覆盖）
pub fn save_policy(data_dir: &Path, policy: &CommandPolicy) -> Result<PathBuf, String> {
    let path = policy_path(data_dir);
    let txt = serde_json::to_string_pretty(policy).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&path, txt).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// 判定
// ---------------------------------------------------------------------------

/// 对一条命令做策略判定（不执行、不申请，只看策略）。
pub fn evaluate(policy: &CommandPolicy, raw: &str) -> CmdVerdict {
    evaluate_depth(policy, raw, 0)
}

/// [`evaluate`] 的带深度版本。`depth` 用于限制 shell 内联命令的递归层数。
fn evaluate_depth(policy: &CommandPolicy, raw: &str, depth: usize) -> CmdVerdict {
    let norm = normalize(raw);

    // 混淆 → 归一化不可信。EncodedCommand 硬阻断；其余降级为确认（高风险）。
    if norm.obfuscated {
        if norm.encoded {
            return CmdVerdict::HardBlock {
                reason: "-EncodedCommand 类编码命令（base64）无法审查，已硬阻断".into(),
            };
        }
        // 仍尝试对已解析片段做硬阻断（多一道保险）
        for seg in &norm.segments {
            if let CmdVerdict::HardBlock { reason } = classify_segment_depth(policy, seg, depth) {
                return CmdVerdict::HardBlock { reason };
            }
        }
        return CmdVerdict::Ask {
            reason: format!(
                "命令含动态构造，无法确认实际执行内容（{}）",
                norm.obfuscation.unwrap_or_else(|| "未知混淆".into())
            ),
            risk: Risk::High,
        };
    }

    if norm.segments.is_empty() {
        // 什么都没解析出来（纯空白 / 只有注释）→ 不执行也不放行
        return CmdVerdict::Ask {
            reason: "命令为空或无法解析".into(),
            risk: Risk::Low,
        };
    }

    // 硬阻断优先（most-restrictive-wins）
    for seg in &norm.segments {
        if let CmdVerdict::HardBlock { reason } = classify_segment_depth(policy, seg, depth) {
            return CmdVerdict::HardBlock { reason };
        }
    }

    // 全部段都在白名单里 → 免问
    let mut all_allow = true;
    let mut max_risk = Risk::Low;
    let mut ask_reason = String::new();
    for seg in &norm.segments {
        match classify_segment_depth(policy, seg, depth) {
            CmdVerdict::Allow { .. } => {}
            CmdVerdict::Ask { reason, risk } => {
                all_allow = false;
                if risk > max_risk {
                    max_risk = risk;
                }
                if ask_reason.is_empty() {
                    ask_reason = reason;
                }
            }
            CmdVerdict::HardBlock { .. } => unreachable!("上面已处理"),
        }
    }

    if all_allow {
        CmdVerdict::Allow {
            reason: "命中只读白名单".into(),
        }
    } else {
        CmdVerdict::Ask {
            reason: ask_reason,
            risk: max_risk,
        }
    }
}

/// 对**单个片段**分类。`depth` 用于限制 shell 内联命令的递归层数。
fn classify_segment_depth(policy: &CommandPolicy, seg: &Segment, depth: usize) -> CmdVerdict {
    // ① 结构化危险（比通配更可靠：管它参数什么顺序、什么写法）
    if let Some(reason) = structured_danger(seg) {
        return CmdVerdict::HardBlock { reason };
    }

    let n = seg.normalized();

    // ② 策略硬阻断通配
    for pat in &policy.hard_block {
        if wildcard_match(pat, &n) {
            return CmdVerdict::HardBlock {
                reason: format!("命中硬阻断规则「{pat}」"),
            };
        }
    }

    // ③ 变量 / 环境变量赋值段（`$p = ...` / `$env:X = ...`）—— 不执行任何东西，放行。
    //    动态构造（`$(...)` / `& expr`）已被 detect_obfuscation 拦在前面，到不了这里。
    if seg.exe.starts_with('$') {
        return CmdVerdict::Allow {
            reason: "变量赋值（无执行）".into(),
        };
    }

    // ④ shell spawner：能抽出内联命令 → 递归评估内层，结论以内层为准；
    //    抽不出（`-File x.ps1` / 纯交互 / 超深度）→ 降级为确认。
    if SHELL_SPAWNERS.contains(&seg.exe.as_str()) {
        if depth < MAX_SHELL_DEPTH {
            if let Some(payload) = extract_shell_payload(seg) {
                return evaluate_depth(policy, &payload, depth + 1);
            }
        }
        return CmdVerdict::Ask {
            reason: describe_risk(seg),
            risk: Risk::High,
        };
    }

    // ⑤ 白名单通配
    for pat in &policy.allow {
        if wildcard_match(pat, &n) {
            return CmdVerdict::Allow {
                reason: format!("命中白名单「{pat}」"),
            };
        }
    }

    // ⑥ 其余 → 需确认，按语义粗判风险
    CmdVerdict::Ask {
        reason: describe_risk(seg),
        risk: risk_of(seg),
    }
}

/// 从 shell spawner 片段里抽出内联命令串：
/// `pwsh -Command "..."` / `powershell -c ...` / `cmd /c ...` / `bash -c ...`。
///
/// 抽不出（`-File script.ps1`、无 `-Command` 类开关、开关后无内容）→ `None`。
/// 说明：tokenize 已剥引号，引号内的整串会作为**单个 token** 保留，正好是内联命令。
fn extract_shell_payload(seg: &Segment) -> Option<String> {
    if !SHELL_SPAWNERS.contains(&seg.exe.as_str()) {
        return None;
    }
    let args = &seg.args;
    for (i, a) in args.iter().enumerate() {
        let lower = a.as_str();
        // 形如 `-command:<payload>` / `-c:<payload>` / `/c:<payload>`
        for flag in ["-command:", "-c:", "/c:"] {
            if let Some(rest) = lower.strip_prefix(flag) {
                let payload = format!("{} {}", rest, args[i + 1..].join(" "));
                let t = payload.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        // 形如 `-command <payload>` / `-c <payload>` / `/c <payload>` / `/k <payload>`
        if matches!(lower, "-command" | "-c" | "/c" | "/k") {
            let t = args[i + 1..].join(" ");
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 结构化危险判定（不依赖通配的写法）。
fn structured_danger(seg: &Segment) -> Option<String> {
    let exe = seg.exe.as_str();

    // Remove-Item 家族带 -Recurse + -Force → 递归强删
    if exe == "remove-item" && seg.has_flag("-recurse") && seg.has_flag("-force") {
        return Some("Remove-Item -Recurse -Force（递归强制删除）".into());
    }
    // Remove-Item 带 -Recurse 删目录（无 -Force 也危险）
    if exe == "remove-item" && seg.has_flag("-recurse") {
        return Some("Remove-Item -Recurse（递归删除目录）".into());
    }
    if exe == "format" || exe == "format-volume" {
        return Some("格式化卷".into());
    }
    if exe == "shutdown" || exe == "restart-computer" || exe == "stop-computer" {
        return Some("关机/重启".into());
    }
    if exe == "reg" && seg.args.first().map(|a| a == "delete").unwrap_or(false) {
        return Some("reg delete（删除注册表项）".into());
    }
    if exe == "diskpart" {
        return Some("diskpart（磁盘分区操作）".into());
    }
    if exe == "vssadmin" && seg.args.iter().any(|a| a == "delete") {
        return Some("vssadmin delete（删除卷影副本）".into());
    }
    if (exe == "cipher") && seg.args.iter().any(|a| a == "/w") {
        return Some("cipher /w（擦除空闲空间）".into());
    }
    if exe == "sc.exe" && seg.args.first().map(|a| a == "delete").unwrap_or(false) {
        return Some("sc delete（删除系统服务）".into());
    }
    if exe == "bcdedit" && seg.args.iter().any(|a| a == "delete") {
        return Some("bcdedit delete（改启动配置）".into());
    }
    None
}

/// 语义粗判风险等级
fn risk_of(seg: &Segment) -> Risk {
    let exe = seg.exe.as_str();
    if SHELL_SPAWNERS.contains(&exe) {
        return Risk::High;
    }
    if matches!(
        exe,
        "remove-item"
            | "stop-process"
            | "set-content"
            | "add-content"
            | "out-file"
            | "move-item"
            | "copy-item"
            | "new-item"
            | "set-item"
            | "set-itemproperty"
            | "remove-itemproperty"
            | "start-process"
            | "invoke-command"
            | "invoke-expression"
            | "invoke-webrequest"
            | "schtasks"
            | "takeown"
            | "icacls"
            | "net"
            | "reg"
            | "set-executionpolicy"
    ) || seg.args.iter().any(|a| a == "-force" || a == "-recurse")
    {
        Risk::High
    } else if matches!(exe, "npm" | "pnpm" | "yarn" | "cargo" | "pip" | "docker" | "git" | "node")
        || seg.args.iter().any(|a| a.starts_with("--fix") || a == "-w")
    {
        Risk::Medium
    } else {
        Risk::Medium
    }
}

fn describe_risk(seg: &Segment) -> String {
    if SHELL_SPAWNERS.contains(&seg.exe.as_str()) {
        return format!("派生新 shell（{}）—— 可执行任意命令，无法预先审查", seg.exe);
    }
    format!("不在白名单内：{}", seg.normalized())
}

// ---------------------------------------------------------------------------
// 通配匹配（只支持 `*` 和 `?`，`*` 可跨任何字符，含路径分隔符）
// ---------------------------------------------------------------------------

/// 大小写不敏感的通配匹配。`*` 匹配任意长度（含 `\` `/`），`?` 匹配单字符。
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    wildcard_match_chars(&p, &t)
}

fn wildcard_match_chars(p: &[char], t: &[char]) -> bool {
    // 经典双指针 + 回溯（星号仅需回溯到最近一个）
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// 命令授权（指纹 = 归一化命令，非路径）
// ---------------------------------------------------------------------------

/// 授权粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CmdScope {
    /// 记住「可执行名 + 首个子命令」（覆盖 `git status` / `git status -s`）
    Prefix,
    /// 记住**完整命令**（最窄）
    Full,
}

/// 一条命令授权。
///
/// `remaining == None` 的**永久档**会落 `command_grants.json`（跨会话 / 跨重启存活）；
/// `remaining = Some(n)` 的**一次性档**只活在内存，用完即销毁。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdGrant {
    /// 归一化后的指纹（Prefix 存 head，Full 存 full）
    pub fingerprint: String,
    /// 人类可读展示（原始命令或 head）
    pub display: String,
    pub scope: CmdScope,
    pub tier: GrantTier,
    /// `None` = 不限次；`Some(1)` = 一次
    #[serde(default)]
    pub remaining: Option<u32>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub granted_at: u64,
}

impl CmdGrant {
    pub fn has_left(&self) -> bool {
        self.remaining.map(|n| n > 0).unwrap_or(true)
    }
}

/// 命令授权表。
#[derive(Debug, Clone, Default)]
pub struct CmdGrantStore {
    grants: Vec<CmdGrant>,
}

impl CmdGrantStore {
    pub fn new() -> Self {
        Self { grants: Vec::new() }
    }

    /// 整体替换（启动时从 `command_grants.json` 加载）。
    pub fn replace(&mut self, grants: Vec<CmdGrant>) {
        self.grants = grants;
    }

    pub fn grants(&self) -> &[CmdGrant] {
        &self.grants
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    pub fn add(&mut self, g: CmdGrant) {
        // 同指纹 + 同档位 + 同粒度 → 合并（权限不分高低，命令授权只有"有/无"）
        if g.tier != GrantTier::Once {
            if let Some(ex) = self
                .grants
                .iter_mut()
                .find(|x| x.tier == g.tier && x.scope == g.scope && x.fingerprint == g.fingerprint)
            {
                if !g.reason.is_empty() {
                    ex.reason = g.reason;
                }
                return;
            }
        }
        self.grants.push(g);
    }

    /// 命中判定（**不消费**）：返回命中的授权下标。
    pub fn find(&self, raw: &str) -> Option<usize> {
        let norm = normalize(raw);
        let full = norm.full();
        let head = norm.head();

        for (i, g) in self.grants.iter().enumerate() {
            if !g.has_left() {
                continue;
            }
            let hit = match g.scope {
                CmdScope::Full => g.fingerprint == full,
                CmdScope::Prefix => !head.is_empty() && g.fingerprint == head,
            };
            if hit {
                return Some(i);
            }
        }
        None
    }

    /// 消费一次。`Once` 扣到 0 即移除；其余档位不动。
    pub fn consume(&mut self, idx: usize) -> Option<CmdGrant> {
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

    // 注意：这里**没有** `clear_tier`。
    // 命令授权只有两种寿命：一次性（用完即销毁）与永久（落盘，靠撤销回收），
    // 不存在"某一档整体失效"的语义（2026-09-22 用户拍板）。

    pub fn clear_all(&mut self) -> usize {
        let n = self.grants.len();
        self.grants.clear();
        n
    }

    /// 按指纹撤销（设置页「撤销」按钮用）。返回撤销条数。
    pub fn revoke(&mut self, fingerprint: &str) -> usize {
        let before = self.grants.len();
        self.grants.retain(|g| g.fingerprint != fingerprint);
        before - self.grants.len()
    }
}

/// 造一条「批准一次」授权（完整命令指纹，一次性）。
pub fn grant_once(raw: &str, reason: impl Into<String>) -> CmdGrant {
    let norm = normalize(raw);
    CmdGrant {
        fingerprint: norm.full(),
        display: raw.trim().to_string(),
        scope: CmdScope::Full,
        tier: GrantTier::Once,
        remaining: Some(1),
        reason: reason.into(),
        granted_at: now_ms(),
    }
}

/// 造一条「记住」授权（Prefix / Full）。**永久档**（`remaining = None`）——
/// 会被写进 `command_grants.json`，跨会话 / 跨重启存活，只有撤销才失效。
pub fn grant_lasting(raw: &str, scope: CmdScope, tier: GrantTier, reason: impl Into<String>) -> CmdGrant {
    let norm = normalize(raw);
    let (fingerprint, display) = match scope {
        CmdScope::Prefix => (norm.head(), norm.head()),
        CmdScope::Full => (norm.full(), raw.trim().to_string()),
    };
    CmdGrant {
        fingerprint,
        display,
        scope,
        tier,
        remaining: None,
        reason: reason.into(),
        granted_at: now_ms(),
    }
}

// ---------------------------------------------------------------------------
// 持久化（`command_grants.json`）
// ---------------------------------------------------------------------------

/// 命令授权持久化文件：`<data_dir>/command_grants.json`。
pub fn grants_path(data_dir: &Path) -> PathBuf {
    data_dir.join("command_grants.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct CmdGrantsFile {
    #[serde(default)]
    grants: Vec<CmdGrant>,
}

/// 读永久命令授权。**文件不存在 = 空表，不是错误**。
///
/// 只收永久档：`Once` 档若出现在文件里（历史脏数据 / 手工编辑 / 误写）
/// 一律丢弃 —— 一次性授权跨重启存活，等于把"仅这一次"变成"永远"。
pub fn load_grants(data_dir: &Path) -> Vec<CmdGrant> {
    let path = grants_path(data_dir);
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<CmdGrantsFile>(&txt) {
        Ok(f) => f
            .grants
            .into_iter()
            .filter(|g| g.remaining.is_none() && g.has_left())
            .collect(),
        Err(e) => {
            // 坏文件按空表处理。方向是**保守的**：空表 = 回到"每条都问" = 更严，
            // 不会因为文件损坏而多放行一条命令。
            eprintln!(
                "[float-agent] ⚠️ {} 解析失败（{e}），本次按「无命令授权」处理",
                path.display()
            );
            Vec::new()
        }
    }
}

/// 写永久命令授权。**只写 `remaining == None` 的档**；空表 → 删文件，不留空壳。
pub fn save_grants(data_dir: &Path, grants: &[CmdGrant]) -> Result<(), String> {
    let path = grants_path(data_dir);
    let lasting: Vec<CmdGrant> = grants
        .iter()
        .filter(|g| g.remaining.is_none())
        .cloned()
        .collect();

    if lasting.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("清理 {} 失败: {e}", path.display())),
        };
    }

    std::fs::create_dir_all(data_dir).map_err(|e| format!("建数据目录失败: {e}"))?;
    let file = CmdGrantsFile { grants: lasting };
    let txt = serde_json::to_string_pretty(&file).map_err(|e| format!("序列化命令授权失败: {e}"))?;
    std::fs::write(&path, txt).map_err(|e| format!("写命令授权失败: {e}"))
}

// ---------------------------------------------------------------------------
// 审计（哈希链）
// ---------------------------------------------------------------------------

/// 一条命令审计记录（落 `audit-commands.jsonl`）。
///
/// 哈希链：`hash = H(prev_hash + 规范化字段)`，`sequence` 自增。
/// ⚠️ 用 [`DefaultHasher`]（SipHash）而非 sha2 —— 与本项目"零额外依赖"取向一致
///    （见 `images.rs`）。它**不是加密哈希**，只保证"改一行会让链对不上"的
///    篡改可见性，够用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts: u64,
    pub sequence: u64,
    pub prev_hash: String,
    pub hash: String,
    /// blocked | allowed | approved | executed | error
    pub event: String,
    /// hard-block | whitelist | grant | user-approved
    pub source: String,
    pub command: String,
    pub normalized: String,
    pub cwd: String,
    pub session_id: String,
    pub risk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

pub fn audit_path(data_dir: &Path) -> PathBuf {
    data_dir.join("audit-commands.jsonl")
}

/// 追加一条审计（自动补 sequence / prev_hash / hash）。
pub fn audit_append(data_dir: &Path, mut rec: AuditRecord) -> Result<(), String> {
    let path = audit_path(data_dir);
    let (prev_seq, prev_hash) = last_chain(&path);
    rec.sequence = prev_seq + 1;
    rec.prev_hash = prev_hash;
    rec.hash = chain_hash(&rec);

    let mut line = serde_json::to_string(&rec).map_err(|e| format!("审计序列化失败: {e}"))?;
    line.push('\n');

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开审计文件失败: {e}"))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("写审计失败: {e}"))?;
    Ok(())
}

/// 计算链哈希（把除 `hash` 外的字段按固定顺序拼起来再哈希）。
fn chain_hash(rec: &AuditRecord) -> String {
    let mut h = DefaultHasher::new();
    rec.ts.hash(&mut h);
    rec.sequence.hash(&mut h);
    rec.prev_hash.hash(&mut h);
    rec.event.hash(&mut h);
    rec.source.hash(&mut h);
    rec.command.hash(&mut h);
    rec.normalized.hash(&mut h);
    rec.cwd.hash(&mut h);
    rec.session_id.hash(&mut h);
    rec.risk.hash(&mut h);
    rec.exit_code.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 取文件里最后一条记录的 `(sequence, hash)`；没有则 `(0, "")`。
///
/// 只读**尾部 64KB** —— 审计文件会一直长，全量读会越来越慢。
fn last_chain(path: &Path) -> (u64, String) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (0, String::new());
    };
    use std::io::{Read, Seek, SeekFrom};
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return (0, String::new());
    }
    let window = 64 * 1024u64;
    let start = len.saturating_sub(window);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (0, String::new());
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (0, String::new());
    }
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<AuditRecord>(line) {
            return (rec.sequence, rec.hash);
        }
    }
    (0, String::new())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_policy() -> CommandPolicy {
        CommandPolicy::default()
    }

    #[test]
    fn alias_and_param_are_normalized() {
        let n = normalize("rm -r -fo D:\\tmp\\x");
        assert_eq!(n.segments.len(), 1);
        let s = &n.segments[0];
        assert_eq!(s.exe, "remove-item");
        assert!(s.has_flag("-recurse"), "缩写 -r 应展开为 -recurse");
        assert!(s.has_flag("-force"), "缩写 -fo 应展开为 -force");
    }

    #[test]
    fn alias_rm_maps_to_remove_item() {
        let n = normalize("rm foo.txt");
        assert_eq!(n.segments[0].exe, "remove-item");
    }

    #[test]
    fn recursive_force_delete_is_hard_blocked() {
        // 五种写法都应被硬阻断
        for cmd in [
            "rm -r -force D:\\x",
            "Remove-Item -Recurse -Force D:\\x",
            "del -Recurse -Force D:\\x",
            "rd -Rec -Fo D:\\x",
            "rm -rec -for D:\\x",
        ] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::HardBlock { .. }),
                "应硬阻断: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn encoded_command_is_hard_blocked() {
        for cmd in [
            "powershell -EncodedCommand SQBFAFgA",
            "pwsh -enc ZQBjAGgAbwA=",
            "pwsh -e ZQBjAGgAbwA=",
            "powershell.exe -ec ZQBjAGgAbwA=",
        ] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::HardBlock { .. }),
                "EncodedCommand 必须硬阻断: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn dynamic_obfuscation_downgrades_to_ask() {
        for cmd in [
            "& (gcm R*-It*) -Recurse -Force D:\\x",
            "iex \"Remove-Item D:\\x\"",
            "Remove-Item $('D:'+'\\x') -Force",
            "gal rm",
        ] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                !matches!(v, CmdVerdict::Allow { .. }),
                "混淆命令绝不能放行: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn whitelist_readonly_is_allowed() {
        for cmd in ["git status", "git status -s", "get-childitem D:\\a", "ls D:\\a"] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::Allow { .. }),
                "只读命令应免问: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn ordinary_command_needs_ask() {
        let v = evaluate(&allow_policy(), "npm install lodash");
        assert!(matches!(v, CmdVerdict::Ask { .. }), "实际 {v:?}");
    }

    #[test]
    fn compound_command_hard_block_wins() {
        // 前半只读、后半危险 → 整条硬阻断（most-restrictive-wins）
        let v = evaluate(&allow_policy(), "git status; rm -r -force D:\\x");
        assert!(matches!(v, CmdVerdict::HardBlock { .. }), "实际 {v:?}");
    }

    #[test]
    fn compound_command_all_allow() {
        let v = evaluate(&allow_policy(), "git status && git log -1");
        assert!(matches!(v, CmdVerdict::Allow { .. }), "实际 {v:?}");
    }

    // ---- shell 内联命令递归（2026-09-22）----

    #[test]
    fn shell_spawner_readonly_inner_is_allowed() {
        // agent 习惯把只读命令包一层 shell —— 内层只读 → 放行，不再必弹卡。
        for cmd in [
            "cmd /c dir",
            "pwsh -Command Get-ChildItem",
            "pwsh -c \"rg foo\"",
            "bash -c \"cat a.txt | grep foo\"",
            "powershell -Command \"git status\"",
        ] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::Allow { .. }),
                "壳内只读命令应放行: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn shell_spawner_dangerous_inner_still_blocked() {
        // 内层危险 → 整条硬阻断（内层结论向上传播）。
        for cmd in [
            "pwsh -c \"shutdown /s\"",
            "pwsh -Command \"Remove-Item -Recurse -Force D:\\\\x\"",
            "cmd /c \"diskpart\"",
        ] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::HardBlock { .. }),
                "壳内危险命令必须硬阻断: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn shell_spawner_opaque_inner_asks() {
        // 抽不出内联命令（脚本文件 / 无内容）→ 降级为确认，不误放行。
        for cmd in ["powershell -File x.ps1", "pwsh -NoProfile", "cmd"] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::Ask { .. }),
                "无法审查的 shell 应确认: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn shell_spawner_unknown_inner_asks() {
        // 内层不在白名单 → 仍要确认（递归不放宽，只做等价传递）。
        let v = evaluate(&allow_policy(), "pwsh -c \"npm install lodash\"");
        assert!(matches!(v, CmdVerdict::Ask { .. }), "实际 {v:?}");
    }

    // ---- 白名单扩容（2026-09-22）----

    #[test]
    fn expanded_readonly_allowlist() {
        for cmd in [
            "grep foo a.txt",
            "head -n 20 a.txt",
            "tail -f a.log",
            "cd D:\\workplace",
            "Set-Location ..",
            "npm config get registry",
            "npm ls -g",
            "git grep TODO",
            "which node",
            "tree src",
        ] {
            let v = evaluate(&allow_policy(), cmd);
            assert!(
                matches!(v, CmdVerdict::Allow { .. }),
                "扩容只读命令应免问: {cmd} → {v:?}"
            );
        }
    }

    #[test]
    fn variable_assignment_is_allowed() {
        // 纯赋值段不执行任何东西 → 放行；配合后面的只读命令整条免问。
        let v = evaluate(&allow_policy(), "$env:PYTHONUTF8='1'; git status");
        assert!(matches!(v, CmdVerdict::Allow { .. }), "实际 {v:?}");

        // 但赋值 + 危险命令 → 危险命令那段照样硬阻断。
        let v2 = evaluate(&allow_policy(), "$p = 'D:\\x'; Remove-Item $p -Recurse -Force");
        assert!(matches!(v2, CmdVerdict::HardBlock { .. }), "实际 {v2:?}");
    }

    #[test]
    fn wildcard_match_basics() {
        assert!(wildcard_match("git status *", "git status -s"));
        assert!(wildcard_match("git status", "git status"));
        assert!(wildcard_match("*format *", "c format D:"));
        assert!(wildcard_match("get-content *", "get-content D:\\a\\b.txt"));
        assert!(!wildcard_match("git status", "git statusx"));
        assert!(!wildcard_match("git log", "git status"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn segment_head_takes_subcommand() {
        let n = normalize("git status -s");
        assert_eq!(n.head(), "git status");
        let n2 = normalize("npm install lodash");
        assert_eq!(n2.head(), "npm install");
        // 参数是路径/开关 → 退化成裸命令名
        let n3 = normalize("rm -rf D:\\x");
        assert_eq!(n3.head(), "remove-item");
    }

    #[test]
    fn prefix_grant_covers_variants() {
        let mut store = CmdGrantStore::new();
        store.add(grant_lasting("git status", CmdScope::Prefix, GrantTier::Task, "看状态"));
        assert_eq!(store.find("git status"), Some(0));
        assert_eq!(store.find("git status -s"), Some(0), "前缀授权应覆盖变体");
        assert_eq!(store.find("git log"), None, "不同命令不该命中");
    }

    #[test]
    fn full_grant_is_exact() {
        let mut store = CmdGrantStore::new();
        store.add(grant_lasting("npm install lodash", CmdScope::Full, GrantTier::Task, "装依赖"));
        assert_eq!(store.find("npm install lodash"), Some(0));
        assert_eq!(store.find("npm install lodash --save"), None, "完整授权是精确匹配");
    }

    #[test]
    fn once_cmd_grant_is_consumed() {
        let mut store = CmdGrantStore::new();
        store.add(grant_once("npm install lodash", "一次"));
        assert_eq!(store.find("npm install lodash"), Some(0));
        assert!(store.consume(0).is_some());
        assert!(store.is_empty(), "一次性授权用掉即移除");
    }

    // ---- 永久授权落盘（2026-09-22 用户拍板）----

    fn grants_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cmd_grants_round_trip_keeps_only_lasting() {
        let dir = grants_dir("float_agent_cmd_grants_roundtrip");

        let mut store = CmdGrantStore::new();
        store.add(grant_lasting(
            "git status",
            CmdScope::Prefix,
            GrantTier::Task,
            "看状态",
        ));
        store.add(grant_lasting(
            "npm install lodash",
            CmdScope::Full,
            GrantTier::Task,
            "装依赖",
        ));
        store.add(grant_once("git add .", "仅这一次")); // ⚠️ 这条不许进文件

        save_grants(&dir, &store.grants()).unwrap();
        let back = load_grants(&dir);
        assert_eq!(back.len(), 2, "一次性档不该落盘");
        assert!(back.iter().all(|g| g.remaining.is_none()));

        // 重启：装进一个全新的 store，指纹必须仍然命中
        let mut fresh = CmdGrantStore::new();
        fresh.replace(back);
        assert!(fresh.find("git status -s").is_some(), "Prefix 应覆盖变体");
        assert!(fresh.find("npm install lodash").is_some());
        assert!(fresh.find("git add .").is_none(), "一次性档重启后必须失效");
    }

    #[test]
    fn cmd_grants_empty_table_removes_file() {
        let dir = grants_dir("float_agent_cmd_grants_empty");

        let one = [grant_lasting(
            "git status",
            CmdScope::Full,
            GrantTier::Task,
            "",
        )];
        save_grants(&dir, &one).unwrap();
        assert!(grants_path(&dir).exists());

        save_grants(&dir, &[]).unwrap();
        assert!(!grants_path(&dir).exists(), "空表应删文件，不留空壳");
        assert!(load_grants(&dir).is_empty());
    }

    #[test]
    fn cmd_grants_bad_file_falls_back_to_empty() {
        let dir = grants_dir("float_agent_cmd_grants_bad");
        std::fs::write(grants_path(&dir), "{ 这不是 json").unwrap();
        assert!(
            load_grants(&dir).is_empty(),
            "坏文件退化为空表（更严，不会多放行）"
        );
    }

    #[test]
    fn cmd_grants_file_with_once_entry_is_dropped() {
        // 手工塞一条 Once 进文件（脏数据 / 手工编辑）—— 必须被丢弃，
        // 否则「仅这一次」会跨重启变成「永远」。
        let dir = grants_dir("float_agent_cmd_grants_dirty");
        let dirty = serde_json::json!({
            "grants": [{
                "fingerprint": "git status",
                "display": "git status",
                "scope": "prefix",
                "tier": "task",
                "remaining": 1,
                "reason": "",
                "granted_at": 0
            }]
        })
        .to_string();
        std::fs::write(grants_path(&dir), dirty).unwrap();
        assert!(load_grants(&dir).is_empty(), "文件里的 Once 档必须丢弃");
    }

    #[test]
    fn audit_chain_links() {
        let dir = std::env::temp_dir().join("float_agent_audit_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mk = |ev: &str| AuditRecord {
            ts: 1,
            sequence: 0,
            prev_hash: String::new(),
            hash: String::new(),
            event: ev.into(),
            source: "whitelist".into(),
            command: "git status".into(),
            normalized: "git status".into(),
            cwd: "D:\\".into(),
            session_id: "s1".into(),
            risk: "low".into(),
            exit_code: None,
        };

        audit_append(&dir, mk("allowed")).unwrap();
        audit_append(&dir, mk("executed")).unwrap();

        let txt = std::fs::read_to_string(audit_path(&dir)).unwrap();
        let recs: Vec<AuditRecord> = txt
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].sequence, 1);
        assert_eq!(recs[1].sequence, 2);
        assert_eq!(recs[0].prev_hash, "");
        assert_eq!(recs[1].prev_hash, recs[0].hash, "第二条的 prev 应指向第一条");
        assert!(!recs[0].hash.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn policy_roundtrip() {
        let dir = std::env::temp_dir().join("float_agent_cmdpol_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let p = CommandPolicy {
            allow: vec!["git status".into()],
            hard_block: vec!["rm -r -force *".into()],
        };
        save_policy(&dir, &p).unwrap();
        let back = load_policy(&dir);
        assert_eq!(back.allow, p.allow);
        assert_eq!(back.hard_block, p.hard_block);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
