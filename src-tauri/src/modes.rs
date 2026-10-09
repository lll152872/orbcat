//! Agent 模式（标准 / PTC / 闲聊 / 创造）
//!
//! ## 模式管什么、不管什么
//!
//! 一个模式管**两件正交的事**：
//!
//! 1. **哪些 MCP 外部工具组对模型可见**（`mcpGroupsPreload`）
//!
//! ```text
//!   生效的组 = 用户设置（mcp_all_groups / mcp_group_enabled）
//!              ∩ 模式允许的组（mcpGroupsPreload）
//! ```
//!
//! 也就是说这一维是**约束层**，不是开关层：
//!   - 用户显式关掉的组，模式开不回来（模式只能收窄，不能放宽）；
//!   - `mcpGroupsPreload = []` 的取反语义 ——「什么都不预载」= 组全部收起来，
//!     模型只能靠 `list_tool_groups` / `search_tools` / `load_tool_group`
//!     按需加载。
//!
//! 2. **工具以什么形态出现**（`toolPresentation`，见 [`ToolPresentation`]）
//!
//! ```text
//!   native = 每个工具的 schema 直接进工具列表（默认）
//!   ptc    = 坍缩成 run_code 一个传输工具；其余工具作为**程序内 SDK 绑定**
//! ```
//!
//! ⚠️ 为什么第 2 维是必要的（2026-10 修正）：第一维只能决定"有哪些工具"。
//! 早期把 PTC 实现成「给全部组 + 一句『请一次发多个 tool_calls』」，实测
//! 那等于**标准模式 + 更多工具** —— 模型仍要自己枚举每个调用，中间结果仍
//! 全部进上下文，没有循环/条件/错误处理。三个模式于是看不出区别。
//! 真正的 PTC 要把控制流搬进程序（见 `ptc.rs`），那必须改**呈现形态**，
//! 而不仅是"给多少工具"。
//!
//! **刻意不管**：执行权限档位（`ExecTrust`）、迭代轮数上限。
//! 那些各有自己的闸门（`permissions.json` 三层 + `settings.execTrust`），
//! 模式插进去只会让"到底谁在拦我"变得说不清。
//!
//! ## 2026-10-08 收窄的边界：可以管**上下文注入**
//!
//! 上面那条红线原本还写着"system prompt 人格段"，现在把边界划清了：
//!
//! ```text
//!   可以管：注入什么**内容**（`promptExtra`）—— "现在是闲聊场景" 这类
//!   不可以管：**拦不拦**（执行权限 / 迭代上限）—— 一律留在各自闸门里
//! ```
//!
//! 为什么前者不算破例：红线防的是**闸门分散**（出事不知道谁拦的）；而上下文
//! 是内容，注错了读一遍 prompt 就看得见，定位难度不在一个量级。
//! ⚠️ 边界要守住：模式**只**能决定"注入什么"，不能决定"拦不拦"。
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

/// 兜底 / 无效时的模式 id。
///
/// ⚠️ 用户自建模式时 `id` 别撞它，也别撞内置的 `ptc` / `chat` / `creator`。
/// （内置「极简」已于 2026-10-08 由「闲聊」取代 —— 两者机制相同
///  `mcpGroupsPreload: []`，闲聊多了发图能力与场景上下文。）
pub const DEFAULT_MODE_ID: &str = "standard";

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

/// 模型**看到工具的方式**（见 `ptc.rs`）。
///
/// 为什么它是一个独立于 `mcpGroupsPreload` 的维度：
/// 「给不给某个组」回答的是"有哪些工具"，而这里回答的是"工具以什么形态出现"。
/// PTC 的价值在后者 —— 把工具调用从「模型一轮轮喊」变成「模型写一个程序自己跑」，
/// 只给全部组 + 一句"请批量调用"prompt 是做不到这件事的（实测那样只是多给工具）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPresentation {
    /// 每个工具的 schema 直接进工具列表（默认）
    ///
    /// `alias` 让手写 JSON 的大小写不决定成败 —— 这份文件是给人用记事本改的。
    #[serde(alias = "Native", alias = "NATIVE")]
    Native,
    /// 只给 `run_code` 一个传输工具；其余工具作为**程序内的 SDK 绑定**存在
    #[serde(alias = "Ptc", alias = "PTC")]
    Ptc,
}

impl Default for ToolPresentation {
    fn default() -> Self {
        Self::Native
    }
}

/// 正文里能不能出现图片（2026-10-08）。
///
/// 这是模式管的**能力位**，与 `mcpGroupsPreload` 同族 —— 都是"这个模式下
/// 模型**能做什么**"，而不是"用它时问不问"（那是权限的事）。
///
/// 为什么按**模式**而不是按项目：工作场景别乱发表情包、闲聊场景该能发，
/// 这跟你在做哪个项目无关（职责划分见 `docs` 里的 mode vs bundle）。
///
/// ⚠️ 它**不**是闸门：模型仍可能不听话地写出图片语法。真正的约束是
///    `agent.rs` 里按这个值注入的那段说明（措辞不同）。前端**不做**降级拦
///    —— 因为"工作模式"也要渲染产物图（截图、图表），拦了会连它一起挡掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageReplies {
    /// 只贴**工作产物**（截图、生成的图表），不主动发表情包 —— 默认
    #[serde(alias = "Work", alias = "WORK")]
    Work,
    /// 什么都行，包括表情包（闲聊场景）
    #[serde(alias = "Free", alias = "FREE")]
    Free,
}

impl Default for ImageReplies {
    fn default() -> Self {
        Self::Work
    }
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
    /// 工具呈现方式（见 [`ToolPresentation`]）。缺省 = `native`，
    /// 所以老 `modes.json` 不用改也能读。
    #[serde(default, rename = "toolPresentation")]
    pub tool_presentation: ToolPresentation,
    /// 正文里能不能发图（见 [`ImageReplies`]）。缺省 = `work`：
    /// **老 `modes.json` 不写这一项时不会突然变成"什么图都能发"**。
    #[serde(default, rename = "imageReplies")]
    pub image_replies: ImageReplies,
    /// 这个模式注入的**场景上下文**（一两句话，见文件顶部「2026-10-08 收窄的边界」）。
    ///
    /// ⚠️ **只允许内联，刻意不支持引用文件**：`modes.json` 和项目 bundle 一样
    /// 会被复制分享。`project::PromptParts` 那边为了挡住"分享来的包读任意文件
    /// 进 prompt"这条数据外泄通道，强制只收相对文件名 —— 这里干脆连文件名都不收，
    /// 直接堵死。而且场景约定**本该短**（"现在是闲聊场景"），长规则属于项目包。
    #[serde(
        default,
        rename = "promptExtra",
        skip_serializing_if = "String::is_empty"
    )]
    pub prompt_extra: String,
    /// 聊天式呈现（2026-10-08）：一条回复在界面上拆成**多个气泡**，且**不显示「过程」**。
    ///
    /// 与 `imageReplies` **刻意分开**：那个管"能不能发表情包"（能力），
    /// 这个管"输出长什么样"（形态）。两者经常同开（闲聊模式），但不是一回事 ——
    /// 将来完全可能有"聊天式但不许发图"或"工作式但能发表情包"的配置。
    ///
    /// 前端消费（`main.ts` 的 `isChattyMode`）：多气泡切分见 `bubble.ts`。
    #[serde(default, rename = "chatty")]
    pub chatty: bool,
}

impl Mode {
    /// 是否「全给」
    pub fn allows_all(&self) -> bool {
        matches!(&self.mcp_groups_preload, GroupSel::All(s) if s.eq_ignore_ascii_case(ALL_GROUPS))
    }

    /// 是否 PTC（工具坍缩成 `run_code` + 程序内 SDK）
    pub fn is_ptc(&self) -> bool {
        self.tool_presentation == ToolPresentation::Ptc
    }

    /// 白名单（`"all"` 时返回空 —— 调用方要先看 [`Mode::allows_all`]）
    pub fn listed_groups(&self) -> &[String] {
        match &self.mcp_groups_preload {
            GroupSel::Some(v) => v,
            GroupSel::All(_) => &[],
        }
    }

    /// 能不能**随便**发图（含表情包）。`false` = 只贴工作产物。
    ///
    /// `agent.rs` 按这个值选一段不同的措辞注入系统提示，见 [`ImageReplies`]。
    pub fn freestyle_images(&self) -> bool {
        self.image_replies == ImageReplies::Free
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
            description: "处理代码、文件和资料，适合大多数任务。工具以原生 schema 逐个给模型，需要时直接调用。"
                .into(),
            mcp_groups_preload: GroupSel::All(ALL_GROUPS.into()),
            tool_presentation: ToolPresentation::Native,
            image_replies: ImageReplies::Work,
            prompt_extra: String::new(),
            chatty: false,
        },
        Mode {
            id: "ptc".into(),
            name: "PTC 模式".into(),
            description: "在标准模式之上把工具调用**写成程序**：模型写一段 TypeScript，用生成的 SDK \
                          在程序里循环/条件/并发地调工具，中间结果不进上下文，只把精选结论交回。\
                          适合批量处理文件、多源汇总、逐条筛选这类任务。"
                .into(),
            mcp_groups_preload: GroupSel::All(ALL_GROUPS.into()),
            tool_presentation: ToolPresentation::Ptc,
            image_replies: ImageReplies::Work,
            prompt_extra: String::new(),
            chatty: false,
        },
        Mode {
            id: "chat".into(),
            name: "闲聊模式".into(),
            description: "轻松聊天，不端工作架子：不预载外部工具组，但可以主动发\
                          表情包 / 图片；记忆与看屏幕仍走内置工具。"
                .into(),
            mcp_groups_preload: GroupSel::Some(Vec::new()),
            tool_presentation: ToolPresentation::Native,
            image_replies: ImageReplies::Free,
            prompt_extra: "现在是**闲聊场景**，不是工作现场。放松一点，像熟人发消息那样说话：\
                           短句、口语、一次说一个念头，**别写小标题、别分点罗列**。想说几句就\
                           分成几段发出去 —— 界面会按你的分段显示成一条条消息，那才是聊天的样子。\
                           情绪到了可以发一张表情包（用 `send_meme`）。仍然遵守红线。"
                .into(),
            chatty: true,
        },
        Mode {
            id: "creator".into(),
            name: "创造模式".into(),
            description: "用来改自己 / 造新东西：不预载外部工具组，先把本地仓库和文件摸清楚，\
                          再动手加功能、写界面、组合出新的工作方式。"
                .into(),
            mcp_groups_preload: GroupSel::Some(Vec::new()),
            tool_presentation: ToolPresentation::Native,
            image_replies: ImageReplies::Work,
            prompt_extra: String::new(),
            chatty: false,
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

// 历史（2026-10-08 删）：这里原来有 `exists(data_dir, id)` —— 给 `modes_set` 做
// "不接受不存在的 id"的校验（免得用户点了个模式、界面显示成功、实际却静默退回标准）。
// `modes_set` 随 `settings.activeMode` 一起删了，它就没有调用者了。
// 要校验"某 id 存不存在"的话：`resolve(id).id == id` 即可（不存在会兜到标准模式）。

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
    let doc = "{\n  \"_comment\": [\n    \"Agent 模式定义。改这个文件不用重新编译。\",\n    \"一个模式管两件正交的事：\",\n    \"  1) mcpGroupsPreload —— 哪些 MCP 外部工具组对模型可见；\",\n    \"       \\\"all\\\" = 全部组直接给模型；[] = 一个都不预载（按需 load_tool_group）；\",\n    \"       [\\\"github\\\", \\\"mysql\\\"] = 只给这几组。\",\n    \"  2) toolPresentation —— 工具以什么形态出现（可选，缺省 native）：\",\n    \"       \\\"native\\\" = 每个工具的 schema 直接进工具列表；\",\n    \"       \\\"ptc\\\"    = 坍缩成 run_code 一个工具，其余作为程序内 SDK 绑定\",\n    \"                  （模型写 TypeScript 调工具，中间结果不进上下文）。\",\n    \"生效组 = 用户设置（设置›MCP 的组开关）∩ 模式允许的组 —— 模式只能收窄，不能放宽。\",\n    \"写了当前 MCP 里不存在的组名不会报错，面板里会把那一条标灰。\",\n    \"  3) imageReplies —— 正文里能不能发图（可选，缺省 work）：\",\n    \"       \\\"work\\\" = 只贴工作产物（截图 / 图表）；\\\"free\\\" = 含表情包，闲聊用。\",\n    \"  4) promptExtra —— 这个模式的场景上下文（可选，内联短文本）。\",\n    \"       ⚠️ 只支持内联，刻意不支持引用文件：这份文件会被复制分享，\",\n    \"          开\\\"读任意文件进 prompt\\\"的口子等于给别人一条外泄通道。\",\n    \"          长规则请写进项目包的 promptParts。\",\n    \"  5) chatty —— 聊天式呈现（可选，缺省 false）：\",\n    \"       true = 一条回复在界面上拆成多个气泡，且**不显示「过程」**。\",\n    \"       只有闲聊模式该开 —— 工作模式的结构化长答案切成几个气泡会很难读。\"\n  ],\n";
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
        assert_eq!(ids, vec!["standard", "ptc", "chat", "creator"]);
    }

    /// PTC 的语义就是「全给」—— 这条钉死它，免得以后被改成白名单。
    #[test]
    fn ptc_allows_all_groups() {
        let m = builtin().into_iter().find(|m| m.id == "ptc").unwrap();
        assert!(m.allows_all());
        let a = allowed_groups_for_with(&m, &["github".into(), "ssh".into()]);
        assert!(a.all && a.allows("github") && a.allows("没有的组"));
    }

    /// **只有 PTC 是 ptc 呈现**，其余内置模式都是 native。
    ///
    /// 这条是 2026-10 那次修正的核心断言：以前 PTC 与标准模式的差别**只有**
    /// `mcpGroupsPreload`（一个是 `"all"` 一个是 `[]`），于是"标准模式下不能用
    /// MCP"成了默认行为。现在两者都给全部组，差别落在呈现形态上。
    #[test]
    fn only_ptc_uses_ptc_presentation() {
        let modes = builtin();
        for m in &modes {
            if m.id == "ptc" {
                assert!(m.is_ptc(), "ptc 模式必须是 PTC 呈现");
            } else {
                assert!(!m.is_ptc(), "{} 不该是 PTC 呈现", m.id);
            }
        }
    }

    /// **标准模式必须给全部组**（回归：曾经是 `[]`，导致"标准下不能用 MCP"）。
    ///
    /// 症状（用户实测）：默认模式下去 MCP 设置页点某组的「给模型」，
    /// 后端 `load()` 被模式闸门拦下报"当前 agent 模式不允许使用组"，
    /// 而那个开关**连 settings.json 都没写进去**（错误发生在保存之前）。
    #[test]
    fn standard_mode_allows_all_groups() {
        let m = builtin().into_iter().find(|m| m.id == DEFAULT_MODE_ID).unwrap();
        assert!(
            m.allows_all(),
            "标准模式必须允许全部组 —— 否则默认配置下 MCP 工具一个都用不了"
        );
        assert!(!m.is_ptc(), "标准模式不该坍缩工具列表");
    }

    /// 「什么都不给」的只有闲聊与创造（两者都不预载外部组）。
    ///
    /// 注：这条原本断言的是「极简」—— 2026-10-08 极简被「闲聊」取代，
    /// 机制没变（都是 `mcpGroupsPreload: []`），所以断言只是换了个 id。
    /// 创造模式那一条是**沿袭原设计**（它原本就是 `[]`）。
    /// 写进断言是为了让"谁不给组"有据可查，而不是靠读代码猜。
    #[test]
    fn chat_and_creator_withhold_all_groups() {
        let modes = builtin();
        let empty: Vec<&str> = modes
            .iter()
            .filter(|m| !m.allows_all() && m.listed_groups().is_empty())
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(empty, vec!["chat", "creator"], "实际 {empty:?}");
    }

    /// 缺省 `imageReplies` 必须是 `work`（只贴产物图）。
    ///
    /// 这条是**安全侧的默认值**：老 `modes.json` 不写这个字段时，
    /// 不能一升级就变成"什么图都能发"。
    #[test]
    fn image_replies_defaults_to_work() {
        let d = tmp("imgdef");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[{"id":"m","name":"M","description":"x","mcpGroupsPreload":[]}]}"#,
        )
        .unwrap();
        let m = resolve(&d, Some("m"));
        assert_eq!(m.image_replies, ImageReplies::Work);
        assert!(!m.freestyle_images());
    }

    /// 内置闲聊模式必须是 `free` —— 否则"闲聊能发表情包"这个卖点根本不存在。
    #[test]
    fn chat_mode_allows_freestyle_images() {
        let m = builtin().into_iter().find(|m| m.id == "chat").unwrap();
        assert_eq!(m.image_replies, ImageReplies::Free);
        assert!(m.freestyle_images());
        assert!(
            !m.prompt_extra.is_empty(),
            "闲聊模式必须自带场景上下文，否则它跟普通模式没区别"
        );
    }

    /// 闲聊模式必须是 `chatty`（多气泡 + 隐藏「过程」）—— 用户 2026-10-08 的核心需求。
    ///
    /// 反向也要钉住：**只有它**该开。工作模式的结构化长答案切成 5 个气泡会非常难读。
    #[test]
    fn only_chat_mode_is_chatty() {
        let modes = builtin();
        let chat = modes.iter().find(|m| m.id == "chat").unwrap();
        assert!(chat.chatty, "闲聊模式必须开 chatty");

        let others: Vec<&str> = modes
            .iter()
            .filter(|m| m.chatty && m.id != "chat")
            .map(|m| m.id.as_str())
            .collect();
        assert!(others.is_empty(), "只有闲聊模式该开 chatty，实际还有 {others:?}");
    }

    /// 用户可以在自己的 `modes.json` 里写 `imageReplies` / `promptExtra`，
    /// 且大小写不该决定成败（这份文件是人手写的）。
    #[test]
    fn user_can_declare_image_replies_and_prompt_extra() {
        let d = tmp("imguser");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[{"id":"m","name":"M","description":"x","mcpGroupsPreload":"all",
                 "imageReplies":"FREE","promptExtra":"随便点，别端着"}]}"#,
        )
        .unwrap();
        let m = resolve(&d, Some("m"));
        assert!(m.freestyle_images(), "大写 FREE 也要认");
        assert_eq!(m.prompt_extra, "随便点，别端着");
    }

    /// 老 `modes.json`（没有 `toolPresentation` 字段）必须能读，且默认 native。
    /// 这是向后兼容的硬要求：用户手上的文件不会因为升级而解析失败。
    #[test]
    fn legacy_modes_json_without_presentation_defaults_to_native() {
        let d = tmp("legacy");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[{"id":"standard","name":"标准","description":"x","mcpGroupsPreload":[]}]}"#,
        )
        .unwrap();
        let m = resolve(&d, Some("standard"));
        assert_eq!(m.tool_presentation, ToolPresentation::Native);
        assert!(!m.is_ptc());
    }

    /// 用户可以在自己的 `modes.json` 里写 `toolPresentation: "ptc"`。
    #[test]
    fn user_can_declare_ptc_presentation() {
        let d = tmp("userptc");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[{"id":"mine","name":"我的","description":"x",
                 "mcpGroupsPreload":"all","toolPresentation":"ptc"}]}"#,
        )
        .unwrap();
        let m = resolve(&d, Some("mine"));
        assert!(m.is_ptc(), "用户自定义的 PTC 模式要被认");
        assert!(m.allows_all());
    }

    /// 大小写不该决定成败（JSON 是人手写的）。
    #[test]
    fn presentation_is_case_insensitive() {
        let d = tmp("case");
        std::fs::write(
            modes_path(&d),
            r#"{"modes":[{"id":"m","name":"M","description":"x",
                 "mcpGroupsPreload":[],"toolPresentation":"PTC"}]}"#,
        )
        .unwrap();
        assert!(resolve(&d, Some("m")).is_ptc(), "大写 PTC 也要认");
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
        // "这个 id 认不认"用 resolve 判：认得出就用它自己，认不出兜到标准模式
        assert_eq!(resolve(&d, Some("db")).id, "db");
        assert_eq!(
            resolve(&d, Some("没这个")).id,
            DEFAULT_MODE_ID,
            "不存在的 id 必须兜回标准"
        );
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
