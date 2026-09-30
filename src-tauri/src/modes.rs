//! Agent 模式（标准 / PTC / 极简 / 创造）
//!
//! ## 模式管什么、不管什么
//!
//! 一个模式**只**管一件事：**哪些 MCP 外部工具组对模型可见**。
//!
//! ```text
//!   生效的组 = 用户设置（mcp_all_groups / mcp_group_enabled）
//!              ∩ 模式允许的组（mcpGroupsPreload）
//! ```
//!
//! 也就是说模式是**约束层**，不是开关层：
//!   - 用户显式关掉的组，模式开不回来（模式只能收窄，不能放宽）；
//!   - `mcpGroupsPreload = []` 的取反语义 ——「什么都不预载」= 组全部收起来，
//!     模型只能靠 `list_tool_groups` / `search_tools` / `load_tool_group`
//!     按需加载（PTC / 创造模式用 `"all"` 一次全给）。
//!
//! **刻意不管**：内置工具集（`read_file` / `run_command` / `capture_screen` …）、
//! 执行权限档位（`ExecTrust`）、迭代轮数上限、system prompt 人格段。
//! 那些各有自己的闸门（`permissions.json` 三层 + `settings.execTrust`），
//! 模式插进去只会让"到底谁在拦我"变得说不清。一个模式 = 一组可见的 MCP 组。
//!
//! ## 为什么是 JSON 文件而不是 Rust 枚举
//!
//! 与「记忆必须是能用记事本改的 Markdown」同一条理由：用户要能**不重新编译**
//! 就加一个自己的模式（比如「只给数据库组」），改一行 `mcpGroupsPreload` 就够了。
//! 文件写坏 / 不存在 → 退回 [`builtin`] 的四个内置模式，界面永远有东西可选。
//!
//! ## 未知组名（UI 标灰）
//!
//! 文件里写了当前 MCP **不存在**的组名时：
//!   - 后端不报错（server 没起来 / 名字打错都是常态），只是这组**什么也不影响**；
//!   - [`allowed_groups_for_with`] 把它归到 `unknown`，前端把这一条**标灰**，
//!     用户一眼能看出"我写了个不存在的组"，而不是对着一个永远关不掉的开关猜。

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 模式定义文件名（放 `agent-data/`，人类可读可改）
pub const MODES_FILE: &str = "modes.json";

/// 「全给」的写法。选它而不是把 56 个组名列全 —— 组是动态的
/// （换 server / 换网关就会变），写死名字的模式过两天就自己变成"未知组"。
pub const ALL_GROUPS: &str = "all";

/// 兜底 / 无效时的模式 id
pub const DEFAULT_MODE_ID: &str = "standard";

/// 内置「极简」模式的 id —— 用户从设置里加自己的模式时，`id` 不要撞它
const MINIMAL_MODE_ID: &str = "minimal";

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 模式定义里"预载哪些 MCP 组"的写法。
///
/// 用 `#[serde(untagged)]` 是为了让 JSON 能写成最舒服的形式：
/// `"all"` 或 `[]` / `["github", "mysql"]` —— 而不是 `{"mode":"all"}` 这种要查文档的壳。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupSel {
    /// `"all"` = 全部组都可见
    All(String),
    /// 组名白名单（空数组 = 一个都不给）
    Some(Vec<String>),
}

/// 一个模式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    /// 稳定 id（落 `settings.json` 存的是它，不是显示名）
    pub id: String,
    /// 显示名
    pub name: String,
    /// 一句话说明（面板里显示在名字下面）
    pub description: String,
    /// 预载哪些 MCP 组（见 [`GroupSel`]）
    #[serde(rename = "mcpGroupsPreload")]
    pub mcp_groups_preload: GroupSel,
}

impl Mode {
    /// 是否「全给」
    pub fn allows_all(&self) -> bool {
        matches!(&self.mcp_groups_preload, GroupSel::All(s) if s.eq_ignore_ascii_case(ALL_GROUPS))
    }

    /// 白名单（`"all"` 时返回空 —— 调用方要先看 [`Mode::allows_all`]）
    pub fn listed_groups(&self) -> &[String] {
        match &self.mcp_groups_preload {
            GroupSel::Some(v) => v,
            GroupSel::All(_) => &[],
        }
    }
}

/// `modes.json` 的落盘形态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModesFile {
    /// 当前选中的模式 id。
    ///
    /// ⚠️ 真正生效的那份在 `settings.json` 的 `activeMode` —— 这里这份只是
    /// 为了让 `modes.json` 单文件自解释（"我现在用的是哪个"）。两处不一致时
    /// 以 `settings.json` 为准（它才是命令写的那份）。
    #[serde(default, rename = "activeMode")]
    pub active_mode: Option<String>,
    /// `_comment` 之类的说明字段：serde 默认忽略未知字段，所以这份文件
    /// 可以任意写注释键，不会让解析失败。
    pub modes: Vec<Mode>,
}

/// 「模式对组的影响」的计算结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedGroups {
    /// 允许全部（`mcpGroupsPreload = "all"`）—— 与"白名单恰好列全"是两回事，
    /// 后者会在新增组时把新组挡在外面，而"全给"应当自动包含未来新增的组。
    pub all: bool,
    /// 允许的组名（`all = true` 时为空）
    pub allowed: BTreeSet<String>,
    /// 模式点名了、但当前 MCP 里不存在的组（UI 标灰）
    pub unknown: BTreeSet<String>,
}

impl AllowedGroups {
    /// 这个组名允不允许（`all` 时一律允许）
    pub fn allows(&self, group: &str) -> bool {
        self.all || self.allowed.contains(group)
    }

    /// 转成注册表能用的集合：`all` → `*` 哨兵。
    ///
    /// 为什么用哨兵而不是 `Option<HashSet>`：活跃组 / 组过滤要在
    /// `ToolRegistry`、`tools::tool_specs`、`lib.rs` 三处传来传去，
    /// 加一层 `Option` 只会让每个调用点都多一个 `match`。
    pub fn as_registry_set(&self) -> HashSet<String> {
        if self.all {
            std::iter::once(ALL.to_string()).collect()
        } else {
            self.allowed.iter().cloned().collect()
        }
    }
}

/// 注册表里「全给」的哨兵值（不是合法组名，不会与真实组撞）
pub const ALL: &str = "*";

/// 把 [`AllowedGroups::as_registry_set`] 的哨兵集合展开成"允许哪些真实组"。
///
/// `available` = 当前注册表里的全部组名。哨兵 → 全部可用组。
pub fn expand_allowed(set: &HashSet<String>, available: &[String]) -> HashSet<String> {
    if set.contains(ALL) {
        available.iter().cloned().collect()
    } else {
        set.iter().cloned().collect()
    }
}

/// 哨兵集合是否表示「全给」
pub fn set_is_all(set: &HashSet<String>) -> bool {
    set.contains(ALL)
}

/// 全部内置模式（JSON 文件不存在 / 写坏时的兜底，也是初始化时落盘的模板）
pub fn builtin() -> Vec<Mode> {
    vec![
        Mode {
            id: DEFAULT_MODE_ID.into(),
            name: "标准模式".into(),
            description: "处理代码、文件和资料，适合大多数任务。不预载外部工具组，需要时按需加载。"
                .into(),
            mcp_groups_preload: GroupSel::Some(Vec::new()),
        },
        Mode {
            id: "ptc".into(),
            name: "PTC 模式".into(),
            description: "在标准模式之上把**全部 MCP 外部工具组**直接给模型，适合批量调用工具、\
                          对结果做筛选/整理/去重/统计/汇总的任务。"
                .into(),
            mcp_groups_preload: GroupSel::All(ALL_GROUPS.into()),
        },
        Mode {
            id: MINIMAL_MODE_ID.into(),
            name: "极简模式".into(),
            description: "外部工具组一个都不给，只用内置终端/文件工具完成任务。\
                          适合测试和对比基础表现（最省上下文）。"
                .into(),
            mcp_groups_preload: GroupSel::Some(Vec::new()),
        },
        Mode {
            id: "creator".into(),
            name: "创造模式".into(),
            description: "用来改自己 / 造新东西：不预载外部工具组，先把本地仓库和文件摸清楚，\
                          再动手加功能、写界面、组合出新的工作方式。"
                .into(),
            mcp_groups_preload: GroupSel::Some(Vec::new()),
        },
    ]
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

/// `modes.json` 的路径
pub fn modes_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MODES_FILE)
}

/// 读模式表。**不存在 / 解析失败 → 内置兜底**（不报错：模型都没配好时
/// 也不能让设置页空着）。
///
/// ⚠️ 不做缓存：文件是用户随时会用记事本改的，读一次解析一次（几十字节，
/// 相对一次 LLM 请求可以忽略）。
pub fn load(data_dir: &Path) -> Vec<Mode> {
    let p = modes_path(data_dir);
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return builtin();
    };
    match serde_json::from_str::<ModesFile>(&txt) {
        Ok(f) if !f.modes.is_empty() => f.modes,
        Ok(_) => {
            eprintln!("[orbcat] {} 里 modes 为空，改用内置模式", p.display());
            builtin()
        }
        Err(e) => {
            eprintln!("[orbcat] ⚠️ {} 解析失败（{e}），改用内置模式", p.display());
            builtin()
        }
    }
}

/// 按 id 找一个模式；找不到 / id 为空 → 标准模式（永不返回 `None`）
pub fn resolve(data_dir: &Path, id: Option<&str>) -> Mode {
    let modes = load(data_dir);
    let want = id.unwrap_or(DEFAULT_MODE_ID).trim();
    modes
        .iter()
        .find(|m| m.id == want)
        .or_else(|| modes.iter().find(|m| m.id == DEFAULT_MODE_ID))
        .cloned()
        .unwrap_or_else(|| builtin().remove(0))
}

/// 模式存不存在（`modes_set` 校验用：不接受一个不存在的 id，
/// 免得用户点了个模式、界面显示成功、实际却静默退回标准）
pub fn exists(data_dir: &Path, id: &str) -> bool {
    load(data_dir).iter().any(|m| m.id == id.trim())
}

/// 算「该模式允许哪些组」，并把点名的未知组挑出来。
///
/// `available` = 注册表里当前真实存在的组名。
pub fn allowed_groups_for_with(mode: &Mode, available: &[String]) -> AllowedGroups {
    if mode.allows_all() {
        return AllowedGroups {
            all: true,
            allowed: BTreeSet::new(),
            unknown: BTreeSet::new(),
        };
    }
    let avail: HashSet<&str> = available.iter().map(String::as_str).collect();
    let mut allowed = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for g in mode.listed_groups() {
        let name = g.trim();
        if name.is_empty() {
            continue;
        }
        if avail.contains(name) {
            allowed.insert(name.to_string());
        } else {
            unknown.insert(name.to_string());
        }
    }
    AllowedGroups {
        all: false,
        allowed,
        unknown,
    }
}

/// 从磁盘读模式 + 算允许组（生产路径：给个 `data_dir` 就够了）
pub fn allowed_groups_for(data_dir: &Path, mode_id: Option<&str>, available: &[String]) -> AllowedGroups {
    allowed_groups_for_with(&resolve(data_dir, mode_id), available)
}

/// 生成 `modes.json` 的初始文本（带说明注释块，初始化时落盘）。
///
/// 为什么写文件而不是"不存在就用内置"：用户要能看见这份文件、
/// 知道有这回事、并且直接改 —— 一个只活在二进制里的默认值等于不可发现。
pub fn default_file_text() -> String {
    let f = ModesFile {
        active_mode: Some(DEFAULT_MODE_ID.into()),
        modes: builtin(),
    };
    let body = serde_json::to_string_pretty(&f).unwrap_or_else(|_| "{}".into());
    // JSON 里插注释块不合法，所以说明放在**外层包一层对象**也不行 ——
    // 直接把说明写成额外键 `_comment`（serde 忽略未知键，解析不受影响）。
    let doc = "{\n  \"_comment\": [\n    \"Agent 模式定义。改这个文件不用重新编译。\",\n    \"每个模式只决定：哪些 MCP 外部工具组对模型可见。\",\n    \"  \\\"all\\\" = 全部组直接给模型；[] = 一个都不预载（按需 load_tool_group）；\",\n    \"  [\\\"github\\\", \\\"mysql\\\"] = 只给这几组。\",\n    \"生效组 = 用户设置（设置›MCP 的组开关）∩ 模式允许的组 —— 模式只能收窄，不能放宽。\",\n    \"写了当前 MCP 里不存在的组名不会报错，面板里会把那一条标灰。\"\n  ],\n";
    // 把 pretty 输出里紧跟在 `{` 后的换行替换成我们的注释块
    if let Some(rest) = body.strip_prefix("{\n") {
        format!("{doc}{rest}")
    } else {
        body
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("orbcat_modes_{tag}_{stamp}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn builtin_has_four_modes_with_stable_ids() {
        let ids: Vec<String> = builtin().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["standard", "ptc", "minimal", "creator"]);
    }

    /// PTC 的语义就是「全给」—— 这条钉死它，免得以后被改成白名单。
    #[test]
    fn ptc_allows_all_groups() {
        let m = builtin().into_iter().find(|m| m.id == "ptc").unwrap();
        assert!(m.allows_all());
        let a = allowed_groups_for_with(&m, &["github".into(), "ssh".into()]);
        assert!(a.all && a.allows("github") && a.allows("没有的组"));
    }

    /// 缺文件 → 内置兜底（界面永远有东西可选）
    #[test]
    fn missing_file_falls_back_to_builtin() {
        let d = tmp("missing");
        assert_eq!(load(&d).len(), 4);
        let m = resolve(&d, Some("ptc"));
        assert_eq!(m.id, "ptc");
    }

    /// 文件写坏 → 内置兜底，**不 panic**
    #[test]
    fn broken_file_falls_back_to_builtin() {
        let d = tmp("broken");
        std::fs::write(modes_path(&d), "{ this is not json").unwrap();
        assert_eq!(load(&d).len(), 4);
    }

    /// 空 modes 数组也算坏文件（否则界面会一个模式都没有）
    #[test]
    fn empty_modes_array_falls_back_to_builtin() {
        let d = tmp("empty");
        std::fs::write(modes_path(&d), "{\"modes\":[]}").unwrap();
        assert_eq!(load(&d).len(), 4);
    }

    /// 用户自己加的模式要被认（这是"JSON 驱动"的全部意义）
    #[test]
    fn user_defined_mode_is_loaded_and_usable() {
        let d = tmp("user");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[
                 {"id":"db","name":"只查库","description":"只给 mysql","mcpGroupsPreload":["mysql"]},
                 {"id":"standard","name":"标准模式","description":"x","mcpGroupsPreload":[]}
               ]}"#,
        )
        .unwrap();
        assert!(exists(&d, "db"));
        let a = allowed_groups_for(&d, Some("db"), &["mysql".into(), "github".into()]);
        assert!(!a.all);
        assert_eq!(a.allowed.iter().cloned().collect::<Vec<_>>(), vec!["mysql"]);
        assert!(a.unknown.is_empty());
        // 没点名的组不给
        assert!(!a.allows("github"));
    }

    /// 点名的组当前不存在 → 进 unknown（前端标灰），不影响其它组
    #[test]
    fn unknown_group_names_are_reported_not_fatal() {
        let d = tmp("unknown");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[{"id":"standard","name":"标准","description":"x",
                 "mcpGroupsPreload":["github","打错的组名"]}]}"#,
        )
        .unwrap();
        let a = allowed_groups_for(&d, Some("standard"), &["github".into()]);
        assert!(a.allows("github"));
        assert_eq!(a.unknown.iter().cloned().collect::<Vec<_>>(), vec!["打错的组名"]);
    }

    /// id 不存在 / 为空 → 退回标准（不是"什么都不给"，那样用户会以为工具全坏了）
    #[test]
    fn unknown_mode_id_falls_back_to_standard() {
        let d = tmp("fallback");
        assert_eq!(resolve(&d, Some("nope")).id, DEFAULT_MODE_ID);
        assert_eq!(resolve(&d, None).id, DEFAULT_MODE_ID);
        assert_eq!(resolve(&d, Some("  ")).id, DEFAULT_MODE_ID);
    }

    /// 哨兵集合的语义：`all` → `*`；展开时 `*` → 当前全部可用组
    #[test]
    fn registry_set_uses_star_sentinel_for_all() {
        let all = AllowedGroups { all: true, ..Default::default() };
        let s = all.as_registry_set();
        assert!(set_is_all(&s));
        let avail = vec!["a".to_string(), "b".to_string()];
        let e = expand_allowed(&s, &avail);
        assert_eq!(e.len(), 2, "全给要自动包含未来新增的组");

        let one = AllowedGroups {
            all: false,
            allowed: ["a".to_string()].into_iter().collect(),
            unknown: Default::default(),
        };
        let s2 = one.as_registry_set();
        assert!(!set_is_all(&s2));
        assert_eq!(expand_allowed(&s2, &avail).len(), 1);
    }

    /// 落盘的初始文件必须能被自己解析回来（带 `_comment` 键也不行）
    #[test]
    fn default_file_text_round_trips() {
        let d = tmp("roundtrip");
        std::fs::write(modes_path(&d), default_file_text()).unwrap();
        let modes = load(&d);
        assert_eq!(modes.len(), 4, "{}", default_file_text());
        assert!(modes.iter().any(|m| m.id == "ptc" && m.allows_all()));
    }
}
