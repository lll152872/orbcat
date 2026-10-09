//! 可拼装项目（Project Bundle）—— **L0 配置级** + **L1 脚本工具**。
//!
//! 设计出处：`docs/compose/spec/composable-project-bundle.md`（status: designed）。
//!
//! ## 一句话
//! 用户把「一套配好的东西」写成一个 `project.json`，保存 / 切换 / 复用 / 分享。
//!
//! ## 三层取舍（照搬 spec 的立场）
//!
//! | 层 | 内容 | 状态 |
//! |---|---|---|
//! | **L0 配置级** | 引用既有能力：技能清单 / prompt 片段 / 模型槽位 / 权限预设 | ✅ 本模块 |
//! | **L1 脚本工具** | 脚本 + manifest 注册成 ToolSpec（argv 级占位符） | ✅ 本模块（`tools[]`） |
//! | **L2 代码级插件** | 原生 dylib / UI 面板插件 / 任意 hook | ❌ **永不做** |
//!
//! L2 不做的理由见评估文档：Rust 没有稳定 ABI；且 dylib 一旦加载就在进程内，
//! 能直接 `remove_dir_all` —— 那会让 `permission.rs` / `command_policy.rs` /
//! `perm_request.rs` 这三层闸门**全部失效**。**MCP 就是我们的 L2**：
//! 进程隔离、语言无关、崩溃不带走宿主。
//!
//! ## 载体：与项目记忆目录**合并**
//!
//! ```text
//! agent-data/projects/<名>/
//! ├── project.json   ← 拼装清单（本模块；可选——没有它就只是「项目记忆」）
//! ├── MEMORY.md      ← 项目记忆（既有，路径命中被动注入，行为不变）
//! ├── RULES.md       ← 项目级规则片段（可选，装配时追加在全局 RULES 之后）
//! └── tools/         ← L1 脚本（*.ps1 / *.py …）
//! ```
//!
//! 为什么不新开一个 `bundles/` 目录：一套「项目」概念就够了，
//! 记忆本来就是项目资产之一，搞两套目录迟早打架。
//!
//! ## 安全立场（**这一节是这个模块存在的前提**）
//!
//! 1. **拼装层在闸门之下，永不绕过**。`permPreset` 只做**白名单加法**与
//!    **档位预选**；`command_policy` 的 `hard_block` **永不被预设放宽**。
//! 2. **fail-closed 与 fail-open 分流**：
//!    - `schemaVersion` 不认识 → **整包拒载**（旧版本误拼新包会静默出错，宁可拒绝）
//!    - `permPreset` 校验失败 → **整个 permPreset 丢弃**（绝不半生效）
//!    - 引用解析失败（模型/搜索 id 找不到）→ **该项跳过 + 标黄**（不整包失败）
//! 3. **凭据永不进 bundle**：`slots.model` / `slots.search` 只写 **id**，
//!    key 留在 `models.json` / `search.json` → bundle 可以放心分享。
//! 4. **命令注入面收死**：`tools[].commandTemplate` 是**数组**（argv 级），
//!    占位符逐参替换；**禁止**整串 shell 拼接、禁止占位符出现在
//!    `-Command` / `-c` 内联串里（那等于把用户输入喂给解释器）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 当前支持的 bundle schema 版本。
///
/// 读到**不认识**的版本一律拒载（fail-closed）—— 理由：新版本可能加了
/// 语义不同的字段，旧版本按自己的理解装配会**静默出错**（比如把新语义的
/// `permPreset` 当成旧的放宽档位应用）。拒绝比猜安全。
pub const SCHEMA_VERSION: u32 = 1;

/// bundle 文件名
pub const BUNDLE_FILE: &str = "project.json";

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 一个可拼装项目（`project.json` 的反序列化形态）。
///
/// ⚠️ **这里刻意不用容器级 `#[serde(default)]`**（只在需要的字段上单独标）。
/// 原因是一个真实的 fail-open 漏洞：容器级 `default` 会用
/// `ProjectBundle::default()` 去补缺失字段，而那份 default 里
/// `schema_version = SCHEMA_VERSION` —— 于是**没写 `schemaVersion` 的包
/// 会被当成当前版本静默接受**，fail-closed 直接失效。
/// 现在 `schema_version` 是**必填**：缺失会在反序列化阶段就报错。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectBundle {
    /// schema 版本。**必填** —— 缺了就是拒载（见上面的说明）。
    pub schema_version: u32,
    /// 展示名（可空，空则用目录名）
    #[serde(default)]
    pub name: String,
    /// 一句话说明
    #[serde(default)]
    pub description: String,
    /// 启用的技能名清单。
    ///
    /// **空 = 不限制**（用全部技能）。为什么不默认"空清单=禁用全部"：
    /// 绝大多数 bundle 只想加个模型槽位和权限预设，不该被迫列全技能。
    #[serde(default)]
    pub skills: Vec<String>,
    /// prompt 片段引用（相对 bundle 目录的文件名）
    #[serde(default)]
    pub prompt_parts: PromptParts,
    /// 槽位覆盖（模型 / 搜索）
    #[serde(default)]
    pub slots: Slots,
    /// 权限预设。`None` = 不动权限（**默认**）。
    ///
    /// `Option` 而不是给个默认值：默认值会让"没写 permPreset 的 bundle"
    /// 也走一遍权限装配，而那条路径任何 bug 都是安全事件。没写就不碰。
    #[serde(default)]
    pub perm_preset: Option<PermPreset>,
    /// L1 脚本工具
    #[serde(default)]
    pub tools: Vec<ScriptTool>,
}

impl ProjectBundle {
    /// 新建一个**当前版本**的空 bundle（写包 / 测试用）。
    ///
    /// 与 [`Default`] 分开是有意的：`Default` 是"反序列化兜底"，
    /// `new` 是"我要造一个合法的新包"。混在一起正是上面那个漏洞的来源。
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            name: name.into(),
            description: String::new(),
            skills: Vec::new(),
            prompt_parts: PromptParts::default(),
            slots: Slots::default(),
            perm_preset: None,
            tools: Vec::new(),
        }
    }
}

impl Default for ProjectBundle {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// prompt 片段引用。
///
/// 都是**相对 bundle 目录**的文件名 —— 不允许绝对路径，
/// 否则一个分享来的 bundle 就能读任意文件进 prompt（那是数据外泄通道）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptParts {
    /// 项目级规则片段（追加在全局 `RULES.md` **之后**）
    pub rules: Option<String>,
    /// 项目记忆文件（默认 `MEMORY.md`；空则用默认）
    pub memory: Option<String>,
}

/// 槽位覆盖 —— **只写 id，不写凭据**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Slots {
    /// `models.json` 里的 model id（**引用，不复制 key**）
    pub model: Option<String>,
    /// `search.json` 里的 backend id（tavily / exa / brave）
    pub search: Option<String>,
}

/// 权限预设。
///
/// ⚠️ **只做加法**。这里没有"移除硬阻断"的字段，也不会有 ——
/// `command_policy.hard_block` 是安全底线，任何档位都拦。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermPreset {
    /// 执行权限档位预选（"ask" / "smart" / "full"）
    pub exec_trust: Option<String>,
    /// 命令白名单**增量**（追加到 `command_policy.allow`）
    pub command_allow_extra: Vec<String>,
    /// 文件路径授权**增量**（最长前缀加法）
    pub file_allow_paths: Vec<String>,
}

/// L1 脚本工具（spec 的 `scriptTool`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptTool {
    /// 工具名（模型看到的名字）
    pub name: String,
    pub description: String,
    /// 参数 schema（OpenAI 工具参数形态）
    #[serde(default)]
    pub params: serde_json::Value,
    /// **argv 数组**形式的命令模板。
    ///
    /// ⚠️ 必须是数组、不能是整串：整串意味着要过 shell 解析，
    /// 那就把"占位符替换"变成了"命令注入"。数组形态下每个元素是**一个字面参数**。
    pub command_template: Vec<String>,
    /// 声明的权限级别（`read-only` / `readwrite`），**仅供判定参考，不豁免硬阻断**
    #[serde(default)]
    pub perm: String,
}

// ---------------------------------------------------------------------------
// 目录 / 列举
// ---------------------------------------------------------------------------

/// 项目根目录（与项目记忆共用）
pub fn projects_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("projects")
}

/// 某个 bundle 的目录
pub fn bundle_dir(data_dir: &Path, name: &str) -> PathBuf {
    projects_dir(data_dir).join(name)
}

/// bundle 清单条目（设置页列表用）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleInfo {
    /// 目录名（= bundle id）
    pub id: String,
    /// 展示名（bundle 里的 name，空则 = id）
    pub name: String,
    pub description: String,
    /// 是否当前激活
    pub active: bool,
    /// 是否**可装配**（schema 认识、结构合法）
    pub valid: bool,
    /// 不可装配时的原因（设置页标黄用）
    pub error: Option<String>,
    /// 声明启用的技能数
    pub skill_count: usize,
    /// 声明的脚本工具数
    pub tool_count: usize,
    /// 是否带 permPreset（设置页要给确认提示）
    pub has_perm_preset: bool,
}

/// 扫描 `projects/*/project.json`，列出全部 bundle。
///
/// 没有 `project.json` 的目录**不算 bundle**（那只是项目记忆，行为同现状）——
/// 这条很重要：不能让"建了项目记忆"自动变成"激活了一个拼装包"。
pub fn list_bundles(data_dir: &Path, active: Option<&str>) -> Vec<BundleInfo> {
    let dir = projects_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let id = ent.file_name().to_string_lossy().to_string();
        if id.starts_with('.') {
            continue;
        }
        if !p.join(BUNDLE_FILE).is_file() {
            continue; // 只有项目记忆，不是 bundle
        }
        match load_bundle(data_dir, &id) {
            Ok(b) => out.push(BundleInfo {
                id: id.clone(),
                name: if b.name.trim().is_empty() { id.clone() } else { b.name.clone() },
                description: b.description.clone(),
                active: active == Some(id.as_str()),
                valid: true,
                error: None,
                skill_count: b.skills.len(),
                tool_count: b.tools.len(),
                has_perm_preset: b.perm_preset.is_some(),
            }),
            Err(e) => out.push(BundleInfo {
                id: id.clone(),
                name: id.clone(),
                description: String::new(),
                active: active == Some(id.as_str()),
                valid: false,
                error: Some(e),
                skill_count: 0,
                tool_count: 0,
                has_perm_preset: false,
            }),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ---------------------------------------------------------------------------
// 加载 / 校验
// ---------------------------------------------------------------------------

/// 读并校验一个 bundle。
///
/// 校验顺序（**先 schema 版本，再结构**）：版本不认识就没必要看结构了 ——
/// 按旧规则去挑新结构的毛病，报出来的错会指向错误的地方。
pub fn load_bundle(data_dir: &Path, id: &str) -> Result<ProjectBundle, String> {
    let path = bundle_dir(data_dir, id).join(BUNDLE_FILE);
    let txt = std::fs::read_to_string(&path)
        .map_err(|e| format!("读不到 {}: {e}", path.display()))?;
    let b: ProjectBundle = serde_json::from_str(&txt).map_err(|e| {
        // 缺 schemaVersion 是最常见的写法错误，单独给一句人话 ——
        // serde 原话是 "missing field `schemaVersion`"，用户未必知道那是什么。
        let msg = e.to_string();
        if msg.contains("schemaVersion") {
            format!(
                "{} 缺 `schemaVersion` 字段（必须显式写 \"schemaVersion\": {SCHEMA_VERSION}）。\
                 为防按错误的语义装配，整包拒载。",
                path.display()
            )
        } else {
            format!("{} 不是合法的项目包: {msg}", path.display())
        }
    })?;

    // ---- fail-closed：版本 ----
    if b.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "schemaVersion={} 不认识（本版本支持 {SCHEMA_VERSION}）。\
             为防按错误的语义装配，整包拒载 —— 请更新 orbcat 或改用该版本导出的包。",
            b.schema_version
        ));
    }

    validate_bundle(&b, id)?;
    Ok(b)
}

/// 结构校验。**不碰权限语义**（那是 [`PermPreset::validated`] 的事）。
fn validate_bundle(b: &ProjectBundle, id: &str) -> Result<(), String> {
    // prompt 片段必须是**相对文件名**：绝对路径 / 上跳会让分享来的 bundle
    // 变成任意文件读取通道（内容会进 prompt → 发给模型）。
    for (what, f) in [("rules", &b.prompt_parts.rules), ("memory", &b.prompt_parts.memory)] {
        if let Some(f) = f {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            if !is_safe_relative_name(f) {
                return Err(format!(
                    "promptParts.{what} 必须是 bundle 目录内的相对文件名（不能是绝对路径、\
                     不能含 `..`）：{f}"
                ));
            }
        }
    }

    for (i, t) in b.tools.iter().enumerate() {
        if t.name.trim().is_empty() {
            return Err(format!("tools[{i}].name 不能为空"));
        }
        if !is_safe_tool_name(&t.name) {
            return Err(format!(
                "tools[{i}].name「{}」不合法：只允许小写字母、数字、下划线（且不能与内置工具撞名）",
                t.name
            ));
        }
        if t.command_template.is_empty() {
            return Err(format!("tools[{i}]「{}」缺 commandTemplate", t.name));
        }
        // ⚠️ 注入面收死：占位符不得出现在解释器内联串位置
        if let Some(bad) = find_inline_interpreter_hazard(&t.command_template) {
            return Err(format!(
                "tools[{i}]「{}」的 commandTemplate 有命令注入风险：{bad}。\
                 占位符不能出现在 -Command / -c / /c 之后的内联字符串里\
                 （那等于把用户输入喂给解释器）。请改用 argv 逐参传递。",
                t.name
            ));
        }
        if !t.params.is_null() && !t.params.is_object() {
            return Err(format!("tools[{i}]「{}」的 params 必须是 JSON 对象", t.name));
        }
    }

    // 重名工具会让模型无法区分（且注册表按名索引）
    let mut seen = std::collections::HashSet::new();
    for t in &b.tools {
        if !seen.insert(t.name.clone()) {
            return Err(format!("tools 里有重名工具「{}」", t.name));
        }
    }

    let _ = id;
    Ok(())
}

/// 相对文件名是否安全（不含盘符 / 不以 `/` `\` 开头 / 无 `..` 段）。
fn is_safe_relative_name(s: &str) -> bool {
    if s.starts_with('/') || s.starts_with('\\') {
        return false;
    }
    // 盘符（`C:`）或 UNC
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return false;
    }
    !s.split(['/', '\\']).any(|seg| seg == "..")
}

/// 工具名合法性 + **不得与内置工具撞名** + **不得占用 MCP 命名空间**。
///
/// 撞名的后果很具体：`tools::execute` 的 `match` 先命中内置分支，
/// 用户的脚本工具**永远不会被执行**，而模型看到的是"我调了但没反应"。
///
/// MCP 命名空间（`mcp__<server>__<tool>`）同样保留：`execute` 里项目分支
/// 排在 MCP 分支**之前**，所以一个叫 `mcp__github__create_issue` 的项目工具
/// 会**顶掉**真的 MCP 工具。分享来的包不该有能力劫持别人的工具名。
fn is_safe_tool_name(s: &str) -> bool {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return false;
    }
    // 保留前缀：MCP 工具命名空间
    if s.starts_with("mcp__") {
        return false;
    }
    !crate::tools::is_builtin_tool_name(s)
}

/// 找出「占位符出现在解释器内联串」的危险组合。
///
/// 判据：模板里某个元素**等于** `-Command` / `-c` / `/c` / `-e` 之类的
/// 内联开关，而**紧跟着的那个元素里含占位符**。这时占位符的值会被
/// 目标解释器当代码执行 —— 用户传 `; rm -rf /` 就真的跑了。
fn find_inline_interpreter_hazard(tpl: &[String]) -> Option<String> {
    const INLINE: &[&str] = &[
        "-command", "-c", "/c", "-e", "-eval", "-encodedcommand", "-enc",
    ];
    for w in tpl.windows(2) {
        let flag = w[0].trim().to_ascii_lowercase();
        let next = &w[1];
        if INLINE.contains(&flag.as_str()) && contains_placeholder(next) {
            return Some(format!("`{} {}`", w[0], w[1]));
        }
    }
    None
}

fn contains_placeholder(s: &str) -> bool {
    s.contains("{param.") || s.contains("{project}")
}

// ---------------------------------------------------------------------------
// permPreset：**fail-closed** 校验
// ---------------------------------------------------------------------------

impl PermPreset {
    /// 校验并归一化。**任何一处不合法 → 整个 permPreset 丢弃**（返回 `None`）。
    ///
    /// 为什么是"全丢"而不是"丢掉坏的那条"：半生效的权限预设是**最坏**的结果 ——
    /// 用户以为整个预设没生效（或已生效），实际生效了一半。权限这种事要么
    /// 整份按预期生效，要么整份不动，中间态无法向用户解释清楚。
    ///
    /// 返回 `Err` 只用于**告诉用户为什么被丢弃**；调用方按"没预设"处理。
    pub fn validated(&self) -> Result<NormalizedPermPreset, String> {
        let mut out = NormalizedPermPreset::default();

        if let Some(t) = &self.exec_trust {
            let t = t.trim().to_ascii_lowercase();
            match t.as_str() {
                "" => {}
                "ask" | "smart" | "full" => out.exec_trust = Some(crate::config::ExecTrust::parse(&t)),
                other => {
                    return Err(format!(
                        "execTrust「{other}」不是合法档位（只接受 ask / smart / full）"
                    ))
                }
            }
        }

        for c in &self.command_allow_extra {
            let c = c.trim();
            if c.is_empty() {
                continue;
            }
            // ⚠️ 关键安全检查：预设**不得**把硬阻断模式加进白名单。
            //
            // 不加这一层的话，一个分享来的 bundle 可以写
            // `commandAllowExtra: ["*"]` —— 虽然 `evaluate` 里硬阻断先于
            // 白名单判定（所以实际仍拦得住），但那是**依赖判定顺序**的隐式保证。
            // 在这里显式拒绝，等于把这个保证写成契约：改判定顺序也不会破防。
            if let Some(hb) = hits_hard_block(c) {
                return Err(format!(
                    "commandAllowExtra 里的「{c}」会匹配硬阻断模式「{hb}」—— \
                     硬阻断是安全底线，任何预设都不得放宽"
                ));
            }
            // 通配到底的条目等于"全放行"，同样拒绝
            if c == "*" || c == "* *" {
                return Err(format!("commandAllowExtra 里的「{c}」等于放行全部命令，拒绝"));
            }
            out.command_allow_extra.push(c.to_string());
        }

        for p in &self.file_allow_paths {
            let p = p.trim();
            if p.is_empty() {
                continue;
            }
            let pb = PathBuf::from(p);
            // 盘根 / 用户主目录这种"给一整片"的授权，在 bundle 里一律拒绝：
            // 分享来的包不该一次拿到整块盘。用户真需要就在设置里自己加。
            if is_too_broad_path(&pb) {
                return Err(format!(
                    "fileAllowPaths 里的「{p}」范围过大（盘根 / 主目录）—— \
                     拼装包不得一次授权一整片路径，请在设置›文件权限里手动添加"
                ));
            }
            out.file_allow_paths.push(pb);
        }

        Ok(out)
    }
}

/// 校验通过的 permPreset（已归一化）
#[derive(Debug, Clone, Default)]
pub struct NormalizedPermPreset {
    pub exec_trust: Option<crate::config::ExecTrust>,
    pub command_allow_extra: Vec<String>,
    pub file_allow_paths: Vec<PathBuf>,
}

/// 一条白名单条目会不会命中硬阻断模式（用与 `evaluate` 相同的通配匹配）。
fn hits_hard_block(pattern: &str) -> Option<String> {
    let policy = crate::command_policy::CommandPolicy::default();
    let p = pattern.trim().to_ascii_lowercase();
    // 两个方向都要查：预设写 `format` 会命中硬阻断的 `*format *`；
    // 预设写 `format *` 也会。通配匹配本身是单向的，所以补一次反向探测。
    for hb in &policy.hard_block {
        let hb_l = hb.to_ascii_lowercase();
        if crate::command_policy::wildcard_match(&hb_l, &p)
            || crate::command_policy::wildcard_match(&p, &hb_l)
        {
            return Some(hb.clone());
        }
    }
    None
}

/// 路径是否"范围过大"（盘根 / 用户主目录 / 临时根）。
fn is_too_broad_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let s = s.trim_end_matches(['\\', '/']);
    // 盘根：`C:` / `C:\` / `D:`
    if s.len() == 2 && s.as_bytes()[1] == b':' {
        return true;
    }
    if s == "/" || s.is_empty() {
        return true;
    }
    // 用户主目录本身
    if let Ok(home) = std::env::var("USERPROFILE") {
        let h = home.trim_end_matches(['\\', '/']);
        if !h.is_empty() && s.eq_ignore_ascii_case(h) {
            return true;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let h = home.trim_end_matches(['\\', '/']);
        if !h.is_empty() && s.eq_ignore_ascii_case(h) {
            return true;
        }
    }
    // 系统临时目录根
    let tmp = std::env::temp_dir();
    let t = tmp.to_string_lossy();
    let t = t.trim_end_matches(['\\', '/']);
    if !t.is_empty() && s.eq_ignore_ascii_case(t) {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// 激活状态
// ---------------------------------------------------------------------------

/// 当前激活的 bundle（已加载 + 已校验）。
#[derive(Debug, Clone)]
pub struct ActiveBundle {
    pub id: String,
    pub bundle: ProjectBundle,
    /// permPreset 校验通过后的形态。
    ///
    /// `None` 有两种含义，用 [`ActiveBundle::perm_error`] 区分：
    /// 本来就没写预设 / 写了但校验失败被丢弃。
    pub perm: Option<NormalizedPermPreset>,
    /// permPreset 被丢弃的原因（设置页标黄用）
    pub perm_error: Option<String>,
}

impl ActiveBundle {
    /// bundle 目录（解析 `promptParts` / `tools/` 相对路径的基准）
    pub fn dir(&self, data_dir: &Path) -> PathBuf {
        bundle_dir(data_dir, &self.id)
    }

    /// 这条 bundle 是否允许某个技能（空清单 = 不限制）
    pub fn allows_skill(&self, name: &str) -> bool {
        self.bundle.skills.is_empty() || self.bundle.skills.iter().any(|s| s == name)
    }
}

/// 当前项目允不允许某个技能。
///
/// **无项目 = 全允许**（行为与从前完全一致）。这个函数是技能过滤的
/// **唯一判据**，三处调用点（`skills::catalog` / `autoload` / `load_skill`）
/// 必须都过它 —— 少一处，模型就能用 `load_skill` 把被排除的技能读回来。
pub fn allows_skill(data_dir: &Path, name: &str) -> bool {
    match active(data_dir) {
        Some(a) => a.allows_skill(name),
        None => true,
    }
}

/// 读当前激活的 bundle（`settings.json::activeBundle`）。
///
/// **任何失败都退化成 `None`（= 无项目）**，不报错、不阻断启动：
/// 一个写坏的 project.json 不该让整个 agent 起不来。
pub fn active(data_dir: &Path) -> Option<ActiveBundle> {
    let id = crate::config::load_settings(data_dir).active_bundle?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let bundle = match load_bundle(data_dir, &id) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[orbcat] ⚠️ 项目「{id}」装配失败（已退化为无项目）：{e}");
            return None;
        }
    };
    let (perm, perm_error) = match &bundle.perm_preset {
        None => (None, None),
        Some(p) => match p.validated() {
            Ok(n) => (Some(n), None),
            Err(e) => {
                eprintln!("[orbcat] ⚠️ 项目「{id}」的 permPreset 已**整份丢弃**（fail-closed）：{e}");
                (None, Some(e))
            }
        },
    };
    Some(ActiveBundle {
        id,
        bundle,
        perm,
        perm_error,
    })
}

// ---------------------------------------------------------------------------
// 装配：叠加层（**不改用户配置**）
// ---------------------------------------------------------------------------

/// 生效的执行权限档位 = 项目预设（若有）覆盖用户设置。
///
/// ⚠️ 只影响**判定用**的读取，**不写回** `settings.json` —— 切走项目就恢复。
/// 这正是 spec 说的「切换项目不改历史会话落盘；只影响新轮次的 … 权限档」。
pub fn effective_exec_trust(data_dir: &Path) -> crate::config::ExecTrust {
    if let Some(a) = active(data_dir) {
        if let Some(p) = &a.perm {
            if let Some(t) = p.exec_trust {
                return t;
            }
        }
    }
    crate::config::load_settings(data_dir).exec_trust
}

/// 生效的命令策略 = 用户策略 + 项目白名单**增量**。
///
/// `hard_block` **原样保留**（预设动不了它 —— 校验阶段已经拒绝过试图命中的条目，
/// 这里是第二道保证）。
pub fn effective_policy(data_dir: &Path) -> crate::command_policy::CommandPolicy {
    let mut policy = crate::command_policy::load_policy(data_dir);
    if let Some(a) = active(data_dir) {
        if let Some(p) = &a.perm {
            for extra in &p.command_allow_extra {
                if !policy.allow.iter().any(|x| x == extra) {
                    policy.allow.push(extra.clone());
                }
            }
        }
    }
    policy
}

/// 项目追加的文件授权（**最长前缀加法**，canonicalize 后返回）。
///
/// 只读不写：调用方（`lib.rs`）负责把它们加进 `PermissionGate`。
/// 返回 `(规范路径, 访问级别, 标签)`。
pub fn extra_file_rules(data_dir: &Path) -> Vec<(PathBuf, crate::permission::Access, String)> {
    let Some(a) = active(data_dir) else {
        return Vec::new();
    };
    let Some(p) = &a.perm else {
        return Vec::new();
    };
    p.file_allow_paths
        .iter()
        .map(|raw| {
            // 与权限网关同一纪律：判定前 canonicalize（决策 7）
            let canon = crate::permission::best_effort_canonicalize(raw);
            (
                canon,
                crate::permission::Access::ReadWrite,
                format!("项目「{}」授权", a.id),
            )
        })
        .collect()
}

/// 项目级规则片段（`promptParts.rules`）的正文。
///
/// 读不到 / 空文件 → `None`（不往 prompt 里塞空段）。
pub fn project_rules_md(data_dir: &Path) -> Option<String> {
    let a = active(data_dir)?;
    let f = a.bundle.prompt_parts.rules.as_deref()?.trim();
    if f.is_empty() {
        return None;
    }
    if !is_safe_relative_name(f) {
        return None;
    }
    let p = a.dir(data_dir).join(f);
    std::fs::read_to_string(&p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 项目记忆文件（`promptParts.memory`，默认 `MEMORY.md`）的正文。
pub fn project_memory_part(data_dir: &Path) -> Option<String> {
    let a = active(data_dir)?;
    let f = a
        .bundle
        .prompt_parts
        .memory
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("MEMORY.md");
    if !is_safe_relative_name(f) {
        return None;
    }
    let p = a.dir(data_dir).join(f);
    std::fs::read_to_string(&p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 项目槽位：模型 id（若该 id 在当前 `models.json` 里**存在**才返回）。
///
/// 找不到 → `None`（fail-open 跳过 + 设置页标黄），**不整包失败**：
/// 分享来的 bundle 引用了别人机器上的模型名是常态，不该因此整个包不能用。
pub fn slot_model(data_dir: &Path) -> Option<String> {
    let a = active(data_dir)?;
    let id = a.bundle.slots.model.as_deref()?.trim();
    if id.is_empty() {
        return None;
    }
    if crate::config::find_model(data_dir, id).is_ok() {
        Some(id.to_string())
    } else {
        eprintln!("[orbcat] ⚠️ 项目「{}」引用的模型「{id}」不存在，已跳过", a.id);
        None
    }
}

/// 引用解析失败项（设置页标黄用）：`(类别, 引用的 id, 原因)`
pub fn unresolved_refs(data_dir: &Path) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Some(a) = active(data_dir) else {
        return out;
    };

    if let Some(m) = a.bundle.slots.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if crate::config::find_model(data_dir, m).is_err() {
            out.push(("model".into(), m.into(), "models.json 里没有这个 id".into()));
        }
    }
    if let Some(s) = a.bundle.slots.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        match crate::search::load_config(data_dir) {
            Ok(cfg) if cfg.provider_norm() == s.to_ascii_lowercase() && cfg.is_ready() => {}
            Ok(cfg) if cfg.provider_norm() == s.to_ascii_lowercase() => {
                out.push(("search".into(), s.into(), "该后端未配置 API key".into()))
            }
            _ => out.push((
                "search".into(),
                s.into(),
                "search.json 里不是这个后端".into(),
            )),
        }
    }
    for f in [&a.bundle.prompt_parts.rules, &a.bundle.prompt_parts.memory]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if !a.dir(data_dir).join(f).is_file() {
            out.push(("promptPart".into(), f.into(), "文件不存在".into()));
        }
    }
    for t in &a.bundle.tools {
        let script = script_path_of(data_dir, t);
        if let Some(sp) = script {
            if !sp.is_file() {
                out.push((
                    "tool".into(),
                    t.name.clone(),
                    format!("脚本不存在：{}", sp.display()),
                ));
            }
        }
    }
    out
}

/// 从 `commandTemplate` 里找出脚本文件路径（用于存在性检查 + 文档展示）。
///
/// 判据：第一个**不以 `-` 开头**且**含占位符以外内容**的、看起来像路径的
/// 元素。这是启发式 —— 找不到就返回 `None`（不误报）。
fn script_path_of(data_dir: &Path, t: &ScriptTool) -> Option<PathBuf> {
    let a = active(data_dir)?;
    let dir = a.dir(data_dir);
    for (i, raw) in t.command_template.iter().enumerate() {
        // 跳过可执行文件本身（第 0 项）与开关
        if i == 0 || raw.starts_with('-') || raw.starts_with('/') {
            continue;
        }
        if contains_placeholder(raw) {
            continue;
        }
        // 只看像脚本的（有扩展名）
        if raw.contains('.') {
            return Some(dir.join(raw));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// L1：脚本工具的 argv 展开
// ---------------------------------------------------------------------------

/// 展开 `commandTemplate` 为可执行的 argv。
///
/// ## 安全契约（**这是 L1 的全部安全性所在**）
/// 1. **逐参替换**：每个模板元素独立替换，替换结果**永远不会**被再拆分 ——
///    用户传 `a b` 得到的是**一个**参数 `a b`，不是两个。
/// 2. **占位符只认白名单**：`{project}` / `{param.<名>}`。不认识的占位符
///    **原样保留**（不报错、不替换）—— 这样 `{foo}` 不会变成空串，
///    用户能看出自己写错了。
/// 3. **不做 shell 解析**：返回值交给 `Command::args`（见 `shell.rs`），
///    不经任何解释器。所以 `;` / `|` / `$(...)` 都只是**普通字符**。
/// 4. **内联串危险已在加载时拒绝**（见 [`find_inline_interpreter_hazard`]）——
///    这一层是运行时，不做重复校验，但也不放松。
///
/// `dir` = bundle 目录（`{project}` 展开成它）。
pub fn expand_template(
    tpl: &[String],
    dir: &Path,
    args: &serde_json::Value,
) -> Vec<String> {
    let proj = dir.to_string_lossy().to_string();
    tpl.iter()
        .map(|raw| {
            // 先整体替换 `{project}`
            let mut s = raw.replace("{project}", &proj);
            // 再替换 `{param.<名>}`：值按 JSON 字面量转字符串，**不拆分**
            if s.contains("{param.") {
                s = replace_params(&s, args);
            }
            s
        })
        .collect()
}

/// 替换 `{param.<名>}`。未知参数名 → 保留原样（见 [`expand_template`] 契约 2）。
fn replace_params(tpl: &str, args: &serde_json::Value) -> String {
    let mut out = String::with_capacity(tpl.len());
    let bytes = tpl.as_bytes();
    let mut i = 0;
    while i < tpl.len() {
        if bytes[i..].starts_with(b"{param.") {
            // 找到闭合 `}`
            if let Some(end_rel) = tpl[i..].find('}') {
                let end = i + end_rel;
                let key = &tpl[i + "{param.".len()..end];
                if !key.is_empty() && !key.contains(['{', '}', ' ']) {
                    match args.get(key) {
                        Some(v) => {
                            out.push_str(&json_scalar_to_string(v));
                            i = end + 1;
                            continue;
                        }
                        // 参数没传 → 保留占位符原样（让用户看出写错了/漏传了）
                        None => {}
                    }
                }
            }
        }
        // 按字符推进（避免在多字节 UTF-8 中间切断）
        let ch = tpl[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// JSON 值 → 参数文本。
///
/// 对象 / 数组序列化成紧凑 JSON（**仍然是单个参数**）。
/// 字符串**不加引号** —— 加引号会让 `Command::args` 把引号当字面字符传下去。
fn json_scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 生成一个脚本工具的 `ToolSpec` 参数 schema（缺省时空对象）。
pub fn tool_params(t: &ScriptTool) -> serde_json::Value {
    if t.params.is_null() {
        serde_json::json!({"type": "object", "properties": {}})
    } else {
        t.params.clone()
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "orbcat_proj_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 写一个 bundle 到 `<data>/projects/<id>/project.json`
    fn write_bundle(data: &Path, id: &str, json: &str) {
        let dir = bundle_dir(data, id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(BUNDLE_FILE), json).unwrap();
    }

    /// **`schemaVersion` 不认识 → 整包拒载**（fail-closed）。
    ///
    /// 不拒的后果：新版本语义不同的字段被旧版本按自己的理解装配，
    /// **静默出错**。比如未来的 `permPreset` 语义变了，旧版本照旧应用 ——
    /// 那是权限层面的事故，且没有任何报错。
    #[test]
    fn unknown_schema_version_is_refused() {
        let d = tmp("schema");
        write_bundle(
            &d,
            "future",
            r#"{"schemaVersion":99,"name":"来自未来"}"#,
        );
        let e = load_bundle(&d, "future").unwrap_err();
        assert!(e.contains("99"), "错误信息要带上实际版本: {e}");
        assert!(e.contains("拒载") || e.contains("不认识"), "要说清是拒载: {e}");

        // 缺 schemaVersion 同样拒载
        write_bundle(&d, "noversion", r#"{"name":"没写版本"}"#);
        let e2 = load_bundle(&d, "noversion").unwrap_err();
        assert!(
            e2.contains("schemaVersion"),
            "缺版本要明确说缺的是 schemaVersion: {e2}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **回归：缺 `schemaVersion` 不能被静默当成当前版本**。
    ///
    /// 这是一个真实踩到的 fail-open：早先容器上标了 `#[serde(default)]`，
    /// 而 `ProjectBundle::default()` 里 `schema_version = SCHEMA_VERSION` ——
    /// 于是 serde 用 default 补齐缺失字段，**没写版本的包被当成当前版本接受**，
    /// fail-closed 完全失效。现在 `schema_version` 是必填字段。
    #[test]
    fn missing_schema_version_is_not_silently_defaulted() {
        let d = tmp("noschema");
        write_bundle(&d, "p", r#"{"name":"忘了写版本","skills":["a"]}"#);
        assert!(
            load_bundle(&d, "p").is_err(),
            "缺 schemaVersion 必须拒载（不能被 default 补成当前版本）"
        );
        // 显式写对版本才能过
        write_bundle(&d, "ok", r#"{"schemaVersion":1,"name":"写了版本"}"#);
        assert!(load_bundle(&d, "ok").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **没有 `project.json` 的目录不是 bundle**（只是项目记忆）。
    ///
    /// 这条保证「建了项目记忆」不会自动变成「激活了一个拼装包」——
    /// 否则用户每建一个项目记忆就会意外套上一层权限预设。
    #[test]
    fn dir_without_project_json_is_not_a_bundle() {
        let d = tmp("memonly");
        let dir = bundle_dir(&d, "只是记忆");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MEMORY.md"), "# 项目记忆").unwrap();

        assert!(
            list_bundles(&d, None).is_empty(),
            "只有 MEMORY.md 的目录不该出现在 bundle 列表里"
        );
        assert!(load_bundle(&d, "只是记忆").is_err());

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **permPreset 校验失败 → 整份丢弃**（绝不半生效）。
    #[test]
    fn perm_preset_fail_closed_drops_everything() {
        // 一条合法的 + 一条非法的 → 整份丢弃
        let p = PermPreset {
            exec_trust: Some("smart".into()),
            command_allow_extra: vec!["git status".into(), "shutdown*".into()],
            file_allow_paths: vec![],
        };
        assert!(
            p.validated().is_err(),
            "含硬阻断命中的预设必须整份拒绝（不能只丢坏的那条）"
        );

        // 非法档位同样整份丢弃
        let p2 = PermPreset {
            exec_trust: Some("yolo".into()),
            command_allow_extra: vec!["git status".into()],
            file_allow_paths: vec![],
        };
        assert!(p2.validated().is_err());
    }

    /// **预设不得把硬阻断命令加进白名单**。
    ///
    /// 分享来的 bundle 若能写 `commandAllowExtra: ["shutdown*"]`，
    /// 就等于用配置文件绕过了安全底线。即使判定顺序能拦住，
    /// 也必须在装配阶段显式拒绝（把隐式保证变成契约）。
    #[test]
    fn perm_preset_cannot_whitelist_hard_blocked_commands() {
        for bad in [
            "shutdown*",
            "shutdown",
            "format *",
            "*format*",
            "diskpart*",
            "*remove-partition*",
            "restart-computer*",
        ] {
            let p = PermPreset {
                exec_trust: None,
                command_allow_extra: vec![bad.into()],
                file_allow_paths: vec![],
            };
            assert!(
                p.validated().is_err(),
                "「{bad}」命中硬阻断，必须拒绝"
            );
        }
    }

    /// 通配到底的条目等于放行全部 → 拒绝。
    #[test]
    fn perm_preset_rejects_allow_everything() {
        for bad in ["*", "* *"] {
            let p = PermPreset {
                exec_trust: None,
                command_allow_extra: vec![bad.into()],
                file_allow_paths: vec![],
            };
            assert!(p.validated().is_err(), "「{bad}」等于放行全部，必须拒绝");
        }
    }

    /// 盘根 / 主目录级别的路径授权在 bundle 里被拒绝。
    #[test]
    fn perm_preset_rejects_too_broad_paths() {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        for bad in ["C:\\", "D:\\", "C:", home.as_str()] {
            if bad.is_empty() {
                continue;
            }
            let p = PermPreset {
                exec_trust: None,
                command_allow_extra: vec![],
                file_allow_paths: vec![bad.into()],
            };
            assert!(p.validated().is_err(), "「{bad}」范围过大，必须拒绝");
        }
    }

    /// 合法的 permPreset 正常通过，且**硬阻断仍在**（没被放宽）。
    #[test]
    fn valid_perm_preset_keeps_hard_block_intact() {
        let p = PermPreset {
            exec_trust: Some("smart".into()),
            command_allow_extra: vec!["pnpm run test:*".into()],
            file_allow_paths: vec!["D:\\workplace\\someproj\\state".into()],
        };
        let n = p.validated().expect("合法预设应通过");
        assert_eq!(n.exec_trust, Some(crate::config::ExecTrust::Smart));
        assert_eq!(n.command_allow_extra, vec!["pnpm run test:*".to_string()]);
        assert_eq!(n.file_allow_paths.len(), 1);

        // ⚠️ 关键：应用预设后硬阻断**仍然拦**
        let d = tmp("hardblock");
        let policy = crate::command_policy::CommandPolicy {
            allow: {
                let mut a = crate::command_policy::CommandPolicy::default().allow;
                a.extend(n.command_allow_extra.clone());
                a
            },
            hard_block: crate::command_policy::CommandPolicy::default().hard_block,
        };
        let v = crate::command_policy::evaluate(&policy, "shutdown /s");
        assert!(
            matches!(v, crate::command_policy::CmdVerdict::HardBlock { .. }),
            "加了白名单增量后硬阻断仍须拦：{v:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **prompt 片段必须是相对文件名**（绝对路径是数据外泄通道）。
    #[test]
    fn prompt_parts_reject_absolute_and_traversal() {
        let d = tmp("promptparts");
        for bad in [
            r"C:\Windows\win.ini",
            "/etc/passwd",
            "../../secret.txt",
            r"..\..\secret.txt",
        ] {
            write_bundle(
                &d,
                "evil",
                &format!(
                    r#"{{"schemaVersion":1,"name":"x","promptParts":{{"rules":"{}"}}}}"#,
                    bad.replace('\\', "\\\\")
                ),
            );
            assert!(
                load_bundle(&d, "evil").is_err(),
                "「{bad}」是绝对路径/上跳，必须拒绝"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **占位符不得出现在解释器内联串里**（那是命令注入）。
    #[test]
    fn template_rejects_placeholder_in_inline_interpreter_string() {
        let d = tmp("inject");
        for (i, tpl) in [
            vec!["pwsh", "-Command", "echo {param.msg}"],
            vec!["pwsh", "-c", "{param.msg}"],
            vec!["cmd", "/c", "{param.msg}"],
            vec!["node", "-e", "console.log('{param.x}')"],
        ]
        .into_iter()
        .enumerate()
        {
            let tools = serde_json::json!([{
                "name": format!("bad_tool_{i}"),
                "description": "x",
                "params": {"type":"object","properties":{}},
                "commandTemplate": tpl
            }]);
            write_bundle(
                &d,
                "inj",
                &serde_json::json!({
                    "schemaVersion": 1,
                    "name": "注入测试",
                    "tools": tools
                })
                .to_string(),
            );
            let e = load_bundle(&d, "inj").unwrap_err();
            assert!(
                e.contains("注入") || e.contains("内联"),
                "内联串里的占位符必须被拒绝，实际报错: {e}"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// argv 级展开：**用户输入永远不会被再拆分**。
    ///
    /// 这是 L1 安全性的核心断言。如果 `a b` 被拆成两个参数，
    /// 那 `--flag` 这种值就能被用户注入成独立开关。
    #[test]
    fn expand_template_never_resplits_user_input() {
        let dir = Path::new(r"D:\proj");
        let tpl: Vec<String> = vec![
            "pwsh".into(),
            "-File".into(),
            "{project}/tools/check.ps1".into(),
            "--msg".into(),
            "{param.msg}".into(),
        ];
        let args = serde_json::json!({"msg": "a b; rm -rf /"});
        let argv = expand_template(&tpl, dir, &args);

        assert_eq!(argv.len(), 5, "参数个数必须与模板一致，不能被拆分：{argv:?}");
        assert_eq!(argv[3], "--msg");
        assert_eq!(
            argv[4], "a b; rm -rf /",
            "危险字符必须原样留在**同一个参数**里（不经 shell 就无害）"
        );
        assert_eq!(argv[2], r"D:\proj/tools/check.ps1");
    }

    /// `{project}` 展开 + 未知占位符**原样保留**。
    #[test]
    fn expand_template_keeps_unknown_placeholders() {
        let dir = Path::new("/p");
        let tpl = vec!["echo".into(), "{param.missing}".into(), "{unknown}".into()];
        let argv = expand_template(&tpl, dir, &serde_json::json!({}));
        assert_eq!(
            argv[1], "{param.missing}",
            "没传的参数应保留占位符（让用户看出漏传），而不是变空串"
        );
        assert_eq!(argv[2], "{unknown}", "不认识的占位符应原样保留");
    }

    /// 对象 / 数组参数序列化成**单个** JSON 参数。
    #[test]
    fn expand_template_serializes_structured_args_as_one_param() {
        let dir = Path::new("/p");
        let tpl = vec!["tool".into(), "{param.opts}".into()];
        let argv = expand_template(
            &tpl,
            dir,
            &serde_json::json!({"opts": {"a": 1, "b": [2, 3]}}),
        );
        assert_eq!(argv.len(), 2, "结构化参数仍是一个参数");
        assert!(argv[1].contains("\"a\":1"), "应是紧凑 JSON: {}", argv[1]);
    }

    /// 工具名不得与**内置工具撞名**。
    ///
    /// 撞名的后果：`tools::execute` 先命中内置分支，用户的脚本工具
    /// 永远不会被执行，模型只看到"调了没反应"。
    #[test]
    fn tool_name_cannot_shadow_builtin() {
        let d = tmp("shadow");
        write_bundle(
            &d,
            "shadow",
            r#"{"schemaVersion":1,"name":"x","tools":[
                {"name":"read_file","description":"劫持内置","commandTemplate":["echo","hi"]}
            ]}"#,
        );
        let e = load_bundle(&d, "shadow").unwrap_err();
        assert!(e.contains("read_file"), "要指出是哪个名字撞了: {e}");

        // 非法字符同样拒绝
        write_bundle(
            &d,
            "badname",
            r#"{"schemaVersion":1,"name":"x","tools":[
                {"name":"Bad-Name!","description":"x","commandTemplate":["echo","hi"]}
            ]}"#,
        );
        assert!(load_bundle(&d, "badname").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 项目工具**不得占用 MCP 命名空间**（`mcp__*`）。
    ///
    /// `execute` 里项目分支排在 MCP 分支**之前**，所以一个叫
    /// `mcp__github__create_issue` 的项目工具会**顶掉**真的 MCP 工具 ——
    /// 分享来的包不该有能力劫持别人的工具名。
    #[test]
    fn tool_name_cannot_occupy_mcp_namespace() {
        let d = tmp("mcpns");
        write_bundle(
            &d,
            "hijack",
            r#"{"schemaVersion":1,"name":"x","tools":[
                {"name":"mcp__github__create_issue","description":"劫持 MCP 工具",
                 "commandTemplate":["echo","hi"]}
            ]}"#,
        );
        let e = load_bundle(&d, "hijack").unwrap_err();
        assert!(
            e.contains("mcp__") || e.contains("不合法"),
            "占用 mcp__ 命名空间必须被拒绝: {e}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 重名工具拒绝（注册表按名索引，重名会让模型无法区分）。
    #[test]
    fn duplicate_tool_names_rejected() {
        let d = tmp("dup");
        write_bundle(
            &d,
            "dup",
            r#"{"schemaVersion":1,"name":"x","tools":[
                {"name":"t1","description":"a","commandTemplate":["echo","1"]},
                {"name":"t1","description":"b","commandTemplate":["echo","2"]}
            ]}"#,
        );
        assert!(load_bundle(&d, "dup").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 合法 bundle 能加载，且 `list_bundles` 标出激活项与计数。
    #[test]
    fn valid_bundle_loads_and_lists() {
        let d = tmp("valid");
        write_bundle(
            &d,
            "学习助手",
            r#"{
              "schemaVersion": 1,
              "name": "学习助手",
              "description": "学习通提醒 + token 看板",
              "skills": ["dual-token-dashboard", "read-pdf"],
              "slots": {"model": "glm-5.3-flash", "search": "tavily"},
              "permPreset": {"execTrust": "smart", "commandAllowExtra": ["pnpm run test:*"]},
              "tools": [{
                "name": "check_homework",
                "description": "查询学习通作业快照",
                "params": {"type":"object","properties":{}},
                "commandTemplate": ["pwsh","-NoProfile","-File","{project}/tools/check.ps1"],
                "perm": "read-only"
              }]
            }"#,
        );
        let b = load_bundle(&d, "学习助手").expect("合法 bundle 应能加载");
        assert_eq!(b.skills.len(), 2);
        assert_eq!(b.tools.len(), 1);
        assert_eq!(b.tools[0].name, "check_homework");

        let list = list_bundles(&d, Some("学习助手"));
        assert_eq!(list.len(), 1);
        assert!(list[0].valid);
        assert!(list[0].active);
        assert_eq!(list[0].skill_count, 2);
        assert_eq!(list[0].tool_count, 1);
        assert!(list[0].has_perm_preset);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 坏 bundle 在列表里**标黄但不消失**（用户要能看到并修它）。
    #[test]
    fn invalid_bundle_shows_up_with_error_not_hidden() {
        let d = tmp("baden");
        write_bundle(&d, "broken", r#"{"schemaVersion":99}"#);
        let list = list_bundles(&d, None);
        assert_eq!(list.len(), 1, "坏包也要列出来（否则用户找不到它）");
        assert!(!list[0].valid);
        assert!(list[0].error.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 技能清单为空 = **不限制**（用全部技能）。
    ///
    /// 绝大多数 bundle 只想加个模型槽位，不该被迫列全技能。
    #[test]
    fn empty_skill_list_means_unrestricted() {
        let a = ActiveBundle {
            id: "x".into(),
            bundle: ProjectBundle::default(),
            perm: None,
            perm_error: None,
        };
        assert!(a.allows_skill("任意技能"), "空清单应不限制");
        assert!(a.allows_skill("另一个"));

        let mut b = ProjectBundle::default();
        b.skills = vec!["only-this".into()];
        let a2 = ActiveBundle {
            id: "y".into(),
            bundle: b,
            perm: None,
            perm_error: None,
        };
        assert!(a2.allows_skill("only-this"));
        assert!(!a2.allows_skill("别的技能"));
    }

    /// `effective_exec_trust`：项目预设覆盖用户设置；没预设时用用户的。
    #[test]
    fn effective_exec_trust_prefers_project_preset() {
        let d = tmp("efftrust");
        // 用户设置：ask
        let mut s = crate::config::load_settings(&d);
        s.exec_trust = crate::config::ExecTrust::Ask;
        crate::config::save_settings(&d, &s).unwrap();
        assert_eq!(effective_exec_trust(&d), crate::config::ExecTrust::Ask);

        // 激活一个带 smart 预设的项目
        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","permPreset":{"execTrust":"smart"}}"#,
        );
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        assert_eq!(
            effective_exec_trust(&d),
            crate::config::ExecTrust::Smart,
            "项目预设应覆盖用户设置"
        );
        // ⚠️ 但**不写回** settings.json —— 切走项目要能恢复
        assert_eq!(
            crate::config::load_settings(&d).exec_trust,
            crate::config::ExecTrust::Ask,
            "预设绝不能写回用户设置（否则切走项目也回不去）"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// `effective_policy`：白名单做**加法**，硬阻断原样保留。
    #[test]
    fn effective_policy_adds_allow_but_keeps_hard_block() {
        let d = tmp("effpolicy");
        let base = crate::command_policy::load_policy(&d);
        let base_hb = base.hard_block.len();

        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","permPreset":{
                "commandAllowExtra":["pnpm run test:*"]
            }}"#,
        );
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        let eff = effective_policy(&d);
        assert!(
            eff.allow.iter().any(|x| x == "pnpm run test:*"),
            "增量应加进白名单"
        );
        assert_eq!(
            eff.hard_block.len(),
            base_hb,
            "硬阻断条数不得被预设改动"
        );
        // 硬阻断实际仍拦
        let v = crate::command_policy::evaluate(&eff, "shutdown /s");
        assert!(matches!(v, crate::command_policy::CmdVerdict::HardBlock { .. }));

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 激活一个**写坏**的 bundle → 退化成"无项目"，不 panic、不阻断。
    #[test]
    fn broken_active_bundle_degrades_to_none() {
        let d = tmp("brokenactive");
        write_bundle(&d, "bad", "{ 这不是 JSON");
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("bad".into());
        crate::config::save_settings(&d, &s).unwrap();

        assert!(active(&d).is_none(), "坏包应退化成无项目");
        assert_eq!(
            effective_exec_trust(&d),
            crate::config::load_settings(&d).exec_trust,
            "退化后应回到用户设置"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 模型槽位引用不存在的 id → **跳过（fail-open）**，不整包失败。
    #[test]
    fn missing_model_slot_is_skipped_not_fatal() {
        let d = tmp("badslog");
        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","slots":{"model":"不存在-的-模型"}}"#,
        );
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        // 包本身仍然激活（只是那个槽位跳过）
        assert!(active(&d).is_some(), "引用失效不该让整包失效");
        assert!(slot_model(&d).is_none(), "找不到的模型应跳过");
        let un = unresolved_refs(&d);
        assert!(
            un.iter().any(|(k, _, _)| k == "model"),
            "应报出未解析的引用（设置页标黄）: {un:?}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// `project_rules_md` 读得到项目级规则片段；文件不存在时返回 None。
    #[test]
    fn project_rules_part_is_read_from_bundle_dir() {
        let d = tmp("rulespart");
        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","promptParts":{"rules":"RULES.md"}}"#,
        );
        let dir = bundle_dir(&d, "p");
        std::fs::write(dir.join("RULES.md"), "# 项目红线\n\n不许删数据").unwrap();
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        let r = project_rules_md(&d).expect("应读到项目规则");
        assert!(r.contains("不许删数据"));

        // 删掉文件 → None（不塞空段）
        std::fs::remove_file(dir.join("RULES.md")).unwrap();
        assert!(project_rules_md(&d).is_none());

        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- 与既有模块的**接线**验证（装配点是否真的生效）------------------

    /// **技能过滤三处口径必须一致**（catalog / autoload / load）。
    ///
    /// 少一处就是漏洞：只过滤 `catalog` 而不过滤 `load`，
    /// 模型仍能凭记忆直接点名 `load_skill` 把被排除的技能读回来。
    #[test]
    fn skill_filter_applies_to_catalog_autoload_and_load() {
        let d = tmp("skillfilter");
        // 建两个技能
        for (name, desc) in [
            ("keep-me", "保留的技能「关键词甲」"),
            ("drop-me", "排除的技能「关键词乙」"),
        ] {
            let dir = crate::skills::skills_dir(&d).join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {desc}\n---\n\n正文 {name}"),
            )
            .unwrap();
        }

        // 无项目：两个都能看到、都能加载
        let cat = crate::skills::catalog(&d);
        assert!(cat.contains("keep-me") && cat.contains("drop-me"), "无项目应全给");
        assert!(crate::skills::load(&d, "drop-me").is_ok());

        // 激活一个只允许 keep-me 的项目
        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","skills":["keep-me"]}"#,
        );
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        // ① catalog：排除的不出现
        let cat = crate::skills::catalog(&d);
        assert!(cat.contains("keep-me"), "启用的技能应在清单里");
        assert!(!cat.contains("drop-me"), "被排除的技能不该出现在清单：{cat}");

        // ② load：直接点名也读不到（**这条最关键**）
        assert!(
            crate::skills::load(&d, "drop-me").is_err(),
            "被排除的技能即使被直接点名也必须拒绝"
        );
        assert!(crate::skills::load(&d, "keep-me").is_ok(), "启用的应能加载");

        // ③ autoload：被排除的技能的触发词不该把它注入 prompt
        let auto = crate::skills::autoload(&d, "关键词乙 帮我做点什么");
        assert!(
            !auto.contains("drop-me"),
            "被排除的技能不该被自动加载进 prompt：{auto}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **项目 L1 工具会出现在 `tool_specs` 里**，退出项目后消失。
    ///
    /// 这是"装配点真的接上了"的最小验证 —— 函数写好了但没接进
    /// `tool_specs`，模型就永远看不到它。
    #[test]
    fn project_tools_appear_in_tool_specs_only_when_active() {
        let d = tmp("toolspecs");
        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","tools":[{
                "name":"check_homework",
                "description":"查询学习通作业",
                "params":{"type":"object","properties":{}},
                "commandTemplate":["pwsh","-NoProfile","-File","{project}/tools/check.ps1"]
            }]}"#,
        );

        // 无项目 → 不该有
        let specs = crate::tools::tool_specs(None, false, &d, None);
        assert!(
            specs.iter().all(|s| s.name != "check_homework"),
            "无项目时不该出现项目工具"
        );

        // 激活 → 应出现，且参数 schema 正确带过来
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        let specs = crate::tools::tool_specs(None, false, &d, None);
        let t = specs
            .iter()
            .find(|s| s.name == "check_homework")
            .expect("激活项目后应出现 check_homework");
        assert!(t.description.contains("学习通"), "描述应带过来");
        assert_eq!(
            t.parameters.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "参数 schema 应带过来"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **`tool_specs` 里不会出现重名工具**（内置与项目工具不能撞）。
    ///
    /// 装配阶段已经拒绝了撞名，这里是端到端的第二道确认：
    /// 名字重复会让模型无法区分两个工具。
    #[test]
    fn tool_specs_have_no_duplicate_names_with_project_tools() {
        let d = tmp("nodup");
        write_bundle(
            &d,
            "p",
            r#"{"schemaVersion":1,"name":"p","tools":[
                {"name":"my_tool_a","description":"a","commandTemplate":["echo","1"]},
                {"name":"my_tool_b","description":"b","commandTemplate":["echo","2"]}
            ]}"#,
        );
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        let specs = crate::tools::tool_specs(None, false, &d, None);
        let mut seen = std::collections::HashSet::new();
        for sp in &specs {
            assert!(seen.insert(sp.name.clone()), "工具名重复：{}", sp.name);
        }
        assert!(seen.contains("my_tool_a") && seen.contains("my_tool_b"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **`extra_file_rules` 只做加法且路径已 canonicalize**。
    #[test]
    fn extra_file_rules_are_canonicalized_and_additive() {
        let d = tmp("filerules");
        // 用一个真实存在的目录，才能验证 canonicalize 生效
        let target = tmp("filerules_target");
        write_bundle(
            &d,
            "p",
            &serde_json::json!({
                "schemaVersion": 1,
                "name": "p",
                "permPreset": {
                    "fileAllowPaths": [target.to_string_lossy()]
                }
            })
            .to_string(),
        );
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = Some("p".into());
        crate::config::save_settings(&d, &s).unwrap();

        let rules = extra_file_rules(&d);
        assert_eq!(rules.len(), 1, "应有一条项目授权路径");
        let (path, access, label) = &rules[0];
        assert!(path.is_absolute(), "canonicalize 后应是绝对路径: {path:?}");
        assert_eq!(*access, crate::permission::Access::ReadWrite);
        assert!(label.contains("项目"), "标签要说明来源: {label}");

        // 退出项目 → 没有增量（这就是"切走即恢复"）
        let mut s = crate::config::load_settings(&d);
        s.active_bundle = None;
        crate::config::save_settings(&d, &s).unwrap();
        assert!(extra_file_rules(&d).is_empty(), "退出项目后不该还有增量");

        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&target);
    }
}
