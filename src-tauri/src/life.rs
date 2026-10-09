//! 数据源 —— 给 agent 的**认知面**注入（与 MCP 的能力面正交）。
//!
//! ## 为什么要单独一层（而不是全塞进 MCP）
//!
//! 实测过：把「学习通快照」包成 MCP server 完全可行（orbcat 的 stdio 客户端
//! 连一个 200 行 Python server 就能跑通）。但那 200 行里绝大部分是**协议样板**，
//! 而且每加一个源就要再写一个 server。更关键的是**触发权归属不同**：
//!
//! | | MCP 工具 | 数据源（本模块） |
//! |---|---|---|
//! | 是什么 | 能力面：agent 能**做**什么 | 认知面：agent **知道**什么 |
//! | 谁触发 | **模型**决定去调 | **系统**决定要不要呈现 |
//! | 失败语义 | 调用失败，换法子 | 快照过期 → **静默降级 + 摘要里说明** |
//!
//! 最后一条是要害：MCP 靠模型自觉去查，而用户问「帮我安排下今天」时模型
//! **没有理由**先调一个 `life_get_items`。而「更懂你」的前提恰恰是不用说就知道。
//! 这不是把工具做多能解决的 —— 是触发权不同。
//!
//! ## 一个源 = 一段声明，不是一段代码
//!
//! ```json
//! {
//!   "id": "xuexitong", "label": "学习通", "enabled": true,
//!   "kind": "file",
//!   "path": "D:/workplace/myssh/campus-monitor/state/xuexitong.json",
//!   "items": "$.works[*]",
//!   "map": { "title": "$.work", "group": "$.course", "dueRaw": "$.left" },
//!   "staleHours": 48
//! }
//! ```
//!
//! 加一个源 = 写这几行。`kind` 只有两种，覆盖"爬虫"与"函数"：
//!
//! - `file` —— 读一个已经存在的 JSON（**爬虫/别的程序产出的快照**）。
//!   你现在的 campus-monitor 就是这种，**零代码接入**。
//! - `command` —— 跑一条命令、收 stdout 当 JSON（**邮件、任意脚本**）。
//!
//! ⚠️ **刻意不做 `kind: mcp`**：MCP 工具模型本来就能直接调，再包一层
//! 数据源只会多一条会过期的副本。两个面各自解决自己的问题。
//!
//! ## 注入策略（用户 2026-10 拍板）
//!
//! **一行摘要常驻 + 详情按需**：
//!   - system prompt 动态尾部只放每个源**一行**（条数 + 新鲜度），几十 token；
//!   - 细节（标题/截止/备注）要模型调 `life_items` 才拿。
//!
//! 为什么不全文常驻：每轮都在烧 token，且快照过期时会把旧数据当事实说。
//! 为什么不完全按需：那就退化成 MCP 了 —— 模型不会主动想起去查。
//!
//! ## 安全边界（`command` 是这里唯一的新信任面）
//!
//! `kind: command` 意味着**一条命令会在没人要求时自动执行**（构建 prompt 时）。
//! 这比 MCP 的信任面大（MCP 是模型调才跑）。所以：
//!   - 默认 `enabled: false`，要用户显式打开；
//!   - 走 `shell::run_powershell`（与 `run_command` 同一条解释器/UTF-8 路径）；
//!   - 有超时，失败**静默降级**成一行"读取失败"，绝不打断对话主流程；
//!   - 结果**只**进这一行摘要与 `life_items`，不写进记忆、不进审计正文。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 数据源定义文件名（放 `agent-data/`，人类可读可改）
pub const LIFE_FILE: &str = "life.json";

/// `command` 类源的执行超时（秒）。给得比 `run_command` 短：
/// 它是在**构建 prompt 时**同步等待的，卡住会直接卡住用户发消息。
pub const COMMAND_TIMEOUT_SECS: u64 = 15;

/// 摘要缓存 TTL（秒）。
///
/// 为什么需要：构建 prompt 每轮都会问一次摘要，而 `command` 类源要起进程。
/// 不缓存的话每轮都付一次进程启动 + 脚本执行的钱。
///
/// 为什么是 60s 而不是更长：快照可能刚被爬虫更新，用户希望"刷新一下就看到"。
/// 一分钟是"感觉即时"与"别每轮都跑"之间的折中。
const CACHE_TTL_SECS: u64 = 60;

/// 单个源最多取多少条（防一个坏路径把整个快照灌进上下文）。
const MAX_ITEMS: usize = 200;

/// 摘要里单个源最多显示多少个字（防一条超长标题把 prompt 撑坏）
const MAX_LABEL_CHARS: usize = 40;

// ---------------------------------------------------------------------------
// 配置结构
// ---------------------------------------------------------------------------

/// 源的取数方式。
///
/// `#[serde(rename_all = "lowercase")]` + `alias`：这份文件是给人用记事本改的，
/// 大小写不该决定成败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// 读一个 JSON 文件（爬虫/别的程序的产出）
    #[serde(alias = "File", alias = "FILE")]
    File,
    /// 跑一条命令，stdout 当 JSON 收
    #[serde(alias = "Command", alias = "COMMAND")]
    Command,
}

impl Default for Kind {
    fn default() -> Self {
        Self::File
    }
}

/// 一个数据源。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// 稳定 id（模型调 `life_items` 时传的就是它）
    pub id: String,
    /// 显示名（摘要里用）
    #[serde(default)]
    pub label: String,
    /// 是否启用。**缺省 false** —— `command` 会自动跑，不能默认开。
    #[serde(default)]
    pub enabled: bool,
    /// 取数方式
    #[serde(default)]
    pub kind: Kind,
    /// `kind: file` 的文件路径（支持 `~` 与正斜杠）
    #[serde(default)]
    pub path: String,
    /// `kind: command` 的 PowerShell 命令（stdout 必须是 JSON）
    #[serde(default)]
    pub command: String,
    /// 条目数组的 JSON 路径，如 `$.works[*]`。缺省 = 顶层就是数组。
    #[serde(default)]
    pub items: String,
    /// 字段映射：我们的字段名 → 源里的 JSON 路径
    #[serde(default)]
    pub map: BTreeMap<String, String>,
    /// 超过这么多小时就算"快照过期"（摘要里会标出来）
    #[serde(default)]
    pub stale_hours: Option<f64>,
}

impl Source {
    /// 显示名（没写就退回 id）
    pub fn name(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.id
        } else {
            &self.label
        }
    }

    /// 这个源的配置是否够用（缺关键字段的源不该进"可读"列表）
    pub fn is_complete(&self) -> bool {
        match self.kind {
            Kind::File => !self.path.trim().is_empty(),
            Kind::Command => !self.command.trim().is_empty(),
        }
    }
}

/// `life.json` 的落盘形态
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifeFile {
    /// `_comment` 之类的说明字段：serde 默认忽略未知字段
    #[serde(default)]
    pub sources: Vec<Source>,
}

pub fn life_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LIFE_FILE)
}

/// 读数据源定义。**不存在 / 写坏 → 空表**（不是报错）：
/// 没配数据源是完全正常的状态，不该让设置页或 prompt 组装失败。
pub fn load(data_dir: &Path) -> LifeFile {
    let p = life_path(data_dir);
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return LifeFile::default();
    };
    match serde_json::from_str::<LifeFile>(&txt) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[orbcat] ⚠️ {} 解析失败（{e}），当作没有数据源", p.display());
            LifeFile::default()
        }
    }
}

/// 写回数据源定义（设置页改开关时用）。
pub fn save(data_dir: &Path, f: &LifeFile) -> Result<(), String> {
    let p = life_path(data_dir);
    let txt = serde_json::to_string_pretty(f).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&p, format!("{txt}\n")).map_err(|e| format!("写入 {} 失败: {e}", p.display()))
}

/// 生成 `life.json` 的初始文本（带说明注释块）。
///
/// 为什么不写死本机的 campus-monitor 路径：模板是给**所有**用户落的，
/// 塞一个 `D:\workplace\myssh\...` 进去等于把别人的机器路径写进产品。
/// 说明块里给出完整 schema 与那个例子，用户照抄改一行即可。
pub fn default_file_text() -> String {
    let doc = r#"{
  "_comment": [
    "数据源：给 agent 的「认知面」注入 —— 让它知道你的处境，而不只是会调工具。",
    "与 MCP 的分工：MCP 是能力面（模型决定去调），这里是认知面（系统决定呈现）。",
    "",
    "每个源一段声明，加一个源 = 加一段 JSON，不用写代码。",
    "  kind = \"file\"    → 读一个 JSON 快照（爬虫/别的程序的产出）",
    "  kind = \"command\" → 跑一条命令，stdout 当 JSON 收（邮件、任意脚本）",
    "  items            → 条目数组的路径，如 \"$.works[*]\"；缺省=顶层就是数组",
    "  map              → 字段映射，值是该字段在源里的路径",
    "  staleHours       → 超过这么久没更新就在摘要里标「可能过期」",
    "  enabled          → 缺省 false。command 类会自动跑，必须显式打开。",
    "",
    "例（本机已有的学习通爬虫快照）：",
    "  {",
    "    \"id\": \"xuexitong\", \"label\": \"学习通\", \"enabled\": true,",
    "    \"kind\": \"file\",",
    "    \"path\": \"D:/workplace/myssh/campus-monitor/state/xuexitong.json\",",
    "    \"items\": \"$.works[*]\",",
    "    \"map\": { \"title\": \"$.work\", \"group\": \"$.course\", \"dueRaw\": \"$.left\", \"note\": \"$.summary\" },",
    "    \"staleHours\": 48",
    "  }",
    "",
    "注入策略：system prompt 里只常驻每个源**一行**（条数+新鲜度）；",
    "细节要模型调 life_items 才拿 —— 省 token，且快照过期不会被当成事实。"
  ],
  "sources": []
}
"#;
    doc.to_string()
}

// ---------------------------------------------------------------------------
// JSON 路径（极小实现）
// ---------------------------------------------------------------------------

/// 取一个路径指向的值。支持 `$.a.b` / `$.a[0]` / `$.a[*]`（后者取数组本身）。
///
/// 为什么自己写而不是引 crate：只需要一个子集（点 + 下标 + 通配），
/// 而多一个依赖要跟着编译、升级、审。这里的语义完全够映射快照字段用。
pub fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let p = path.trim();
    if p.is_empty() || p == "$" {
        return Some(root);
    }
    let mut cur = root;
    // 去掉前导 `$.` 或 `$`
    let rest = p.strip_prefix("$.").or_else(|| p.strip_prefix('$')).unwrap_or(p);
    for seg in rest.split('.').filter(|s| !s.is_empty()) {
        // 段内可能有 `[n]` 或 `[*]`
        let (name, idx) = match seg.find('[') {
            Some(i) => (&seg[..i], Some(&seg[i..])),
            None => (seg, None),
        };
        if !name.is_empty() {
            cur = cur.get(name)?;
        }
        if let Some(idx) = idx {
            let inner = idx.trim_start_matches('[').trim_end_matches(']');
            if inner == "*" {
                // 通配：返回数组本身（调用方负责遍历）
                if !cur.is_array() {
                    return None;
                }
            } else {
                let n: usize = inner.parse().ok()?;
                cur = cur.get(n)?;
            }
        }
    }
    Some(cur)
}

/// 把路径指向的值当成**条目数组**取出来。
///
/// 三种都能吃：顶层数组、`$.a[*]`（数组）、`$.a`（数组）。
/// 非数组 → 当成单条目数组（"一个 JSON 就是一个条目"也是合理用法）。
pub fn get_items<'a>(root: &'a Value, path: &str) -> Vec<&'a Value> {
    match get_path(root, path) {
        Some(Value::Array(a)) => a.iter().take(MAX_ITEMS).collect(),
        Some(v) => vec![v],
        None => Vec::new(),
    }
}

/// 值 → 显示文本。字符串原样，数字/布尔转字符串，其余压缩成 JSON。
///
/// 为什么不 `unwrap_or_default`：源里某个字段是数字（`"left": 3`）时
/// 直接丢弃会让摘要少一块，而用户看不出为什么。
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// 解析结果
// ---------------------------------------------------------------------------

/// 一条归一化后的条目。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Item {
    pub title: String,
    /// 分组/课程（可选，摘要与详情里显示）
    pub group: String,
    /// 截止时间的**原文**（如"剩余1453小时59分钟"）
    ///
    /// ⚠️ 刻意**不**解析成绝对时间：那种相对时长串解析失败会得到错的截止，
    /// 而错的截止比"没有截止"更糟（会让模型算错紧急度）。原样带出去，
    /// 让模型自己看着办，或让用户自己判断。
    pub due_raw: String,
    /// 备注/摘要
    pub note: String,
}

/// 一个源在某次读取后的状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: String,
    pub label: String,
    /// 读成功了吗
    pub ok: bool,
    /// 失败原因（ok=false 时给用户看）
    pub error: String,
    /// 快照自报的更新时间（如果源里有）
    pub updated_at: String,
    /// 快照文件/命令结果的年龄（小时）。`None` = 不知道
    pub age_hours: Option<f64>,
    /// 是否超过 `staleHours`
    pub stale: bool,
    pub items: Vec<Item>,
}

impl Snapshot {
    fn fail(src: &Source, error: impl Into<String>) -> Self {
        Self {
            id: src.id.clone(),
            label: src.name().to_string(),
            ok: false,
            error: error.into(),
            updated_at: String::new(),
            age_hours: None,
            stale: false,
            items: Vec::new(),
        }
    }
}

/// 把一条原始 JSON 按 `map` 映射成 [`Item`]。
///
/// 缺 `title` 时用 `group` 或整条 JSON 兜底 —— 宁可显示得难看，
/// 也不要出现一个空标题的条目（用户会以为数据坏了）。
pub fn map_item(raw: &Value, map: &BTreeMap<String, String>) -> Item {
    let pick = |key: &str| -> String {
        map.get(key)
            .and_then(|p| get_path(raw, p))
            .map(value_text)
            .unwrap_or_default()
    };
    let mut it = Item {
        title: pick("title"),
        group: pick("group"),
        due_raw: pick("dueRaw"),
        note: pick("note"),
    };
    if it.title.is_empty() {
        it.title = if !it.group.is_empty() {
            it.group.clone()
        } else {
            value_text(raw)
        };
    }
    it
}

/// 从原始 JSON 文档解析出一个源的全部条目。
pub fn parse_document(doc: &Value, src: &Source) -> Vec<Item> {
    get_items(doc, &src.items)
        .into_iter()
        .map(|raw| map_item(raw, &src.map))
        .filter(|it| !it.title.trim().is_empty())
        .collect()
}

/// 快照里自报的更新时间（尽量从常见字段名里找）。
///
/// 为什么值得猜这几个名字：`updated_at` 是这类快照的事实标准
/// （campus-monitor 就是它）。找不到就退回文件 mtime，不编造。
pub fn self_reported_time(doc: &Value) -> String {
    for k in ["updated_at", "updatedAt", "update_time", "time", "date"] {
        if let Some(v) = doc.get(k) {
            let t = value_text(v);
            if !t.is_empty() {
                return t;
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// 读取
// ---------------------------------------------------------------------------

/// 把配置里的路径展开成本机路径（支持 `~` 与正斜杠）。
pub fn expand_path(p: &str) -> PathBuf {
    let t = p.trim();
    if let Some(rest) = t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            return PathBuf::from(home).join(rest.replace('\\', "/"));
        }
    }
    PathBuf::from(t.replace('\\', "/"))
}

/// 文件的年龄（小时）。读不到 mtime → `None`。
fn file_age_hours(p: &Path) -> Option<f64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(m).ok()?;
    Some(age.as_secs_f64() / 3600.0)
}

/// 读一个 `file` 类源。
pub fn read_file_source(src: &Source) -> Snapshot {
    let p = expand_path(&src.path);
    let txt = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) => return Snapshot::fail(src, format!("读不到快照 {}：{e}", p.display())),
    };
    let doc: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(e) => return Snapshot::fail(src, format!("快照不是合法 JSON：{e}")),
    };

    let age = file_age_hours(&p);
    let stale = matches!((age, src.stale_hours), (Some(a), Some(lim)) if a > lim);
    Snapshot {
        id: src.id.clone(),
        label: src.name().to_string(),
        ok: true,
        error: String::new(),
        updated_at: self_reported_time(&doc),
        age_hours: age.map(|a| (a * 10.0).round() / 10.0),
        stale,
        items: parse_document(&doc, src),
    }
}

/// 读一个 `command` 类源（跑命令，stdout 当 JSON）。
///
/// ⚠️ 失败**一律降级成 `ok:false`**，绝不返回 `Err`：
/// 这个函数在构建 prompt 的路径上，一个坏源不该让用户发不出消息。
pub async fn read_command_source(src: &Source, cwd: &Path) -> Snapshot {
    // 用 `run_powershell_tick`（生产路径）而不是 `run_powershell`（那个是测试专用）：
    // 它与 `run_command` 走同一条解释器解析 + UTF-8 包裹路径，行为一致。
    //
    // ⚠️ tick/cancel 都传 `None`：数据源采集是**后台自动**的，
    // 没有"用户点停止"这个交互，也不该往对话里推心跳。
    let out = match crate::shell::run_powershell_tick(
        &src.command,
        cwd,
        COMMAND_TIMEOUT_SECS,
        None,
        None,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return Snapshot::fail(src, format!("命令启动失败：{e}")),
    };

    if out.timed_out {
        return Snapshot::fail(src, format!("命令超时（{COMMAND_TIMEOUT_SECS}s）"));
    }
    if !out.success() {
        let tail: String = out.stderr.trim().chars().take(300).collect();
        return Snapshot::fail(
            src,
            format!("命令失败（退出码 {:?}）：{tail}", out.exit_code),
        );
    }

    let doc: Value = match serde_json::from_str(out.stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            let head: String = out.stdout.trim().chars().take(200).collect();
            return Snapshot::fail(
                src,
                format!("命令输出不是合法 JSON（{e}）。开头是：{head}"),
            );
        }
    };

    Snapshot {
        id: src.id.clone(),
        label: src.name().to_string(),
        ok: true,
        error: String::new(),
        updated_at: self_reported_time(&doc),
        age_hours: None, // 命令是现算的，没有"快照年龄"这回事
        stale: false,
        items: parse_document(&doc, src),
    }
}

/// 读任意一个源（按 `kind` 分派）。
pub async fn read_source(src: &Source, cwd: &Path) -> Snapshot {
    if !src.is_complete() {
        return Snapshot::fail(
            src,
            match src.kind {
                Kind::File => "配置不完整：缺 path".to_string(),
                Kind::Command => "配置不完整：缺 command".to_string(),
            },
        );
    }
    match src.kind {
        Kind::File => read_file_source(src),
        Kind::Command => read_command_source(src, cwd).await,
    }
}

/// 读全部**已启用**的源。
pub async fn read_all(data_dir: &Path, cwd: &Path) -> Vec<Snapshot> {
    let f = load(data_dir);
    let mut out = Vec::new();
    for src in f.sources.iter().filter(|s| s.enabled) {
        out.push(read_source(src, cwd).await);
    }
    out
}

// ---------------------------------------------------------------------------
// 摘要（进 system prompt 的那一行）
// ---------------------------------------------------------------------------

/// 把标签截短（防一个超长标题把 prompt 撑坏）。
fn clip(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let head: String = t.chars().take(max).collect();
    format!("{head}…")
}

/// 单个源的一行摘要。
///
/// 形态：`- 学习通：2 条未完成 · 快照 15.8 小时前`
/// 过期时补一句，失败时明说 —— **不静默**（模型以为有数据、其实没有，
/// 比"知道源挂了"糟得多）。
pub fn summary_line(snap: &Snapshot) -> String {
    if !snap.ok {
        return format!("- {}：⚠️ 读取失败（{}）", clip(&snap.label, MAX_LABEL_CHARS), clip(&snap.error, 80));
    }
    let n = snap.items.len();
    let mut line = format!("- {}：{n} 条", clip(&snap.label, MAX_LABEL_CHARS));
    if let Some(a) = snap.age_hours {
        line.push_str(&format!(" · 快照 {a} 小时前"));
    } else if !snap.updated_at.is_empty() {
        line.push_str(&format!(" · 快照 {}", clip(&snap.updated_at, 24)));
    }
    if snap.stale {
        line.push_str(" ⚠️ 可能已过期");
    }
    line
}

/// 给 system prompt 的整段摘要。没有任何已启用源 → 空串（整段不出现）。
pub fn summary_section(snaps: &[Snapshot]) -> String {
    if snaps.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = snaps.iter().map(summary_line).collect();
    format!(
        "# 数据源（用户的生活/学习信号）\n\n\
         下面是本机接入的数据源摘要。用户问「我有什么作业」「最近有什么要交的」\
         「帮我安排一下」这类问题时，先看这里 —— 需要**细节**（标题、截止、备注）\
         就调 `life_items`（传源的 id），不要凭摘要里的条数猜内容。\n\
         快照可能过期：标了「可能已过期」的，说明情况时要点出来，别当成最新事实。\n\n\
         {}",
        lines.join("\n")
    )
}

// ---------------------------------------------------------------------------
// 摘要缓存
// ---------------------------------------------------------------------------
//
// 为什么要有：构建 prompt 每轮都问一次摘要，而 `command` 类源要起进程。
// 不缓存等于每轮付一次进程启动 + 脚本执行的钱。
//
// 用全局而不是塞进 AppState：`build_system_prompt_full` 的调用方
// （`run()` / 测试 / test_hooks）都能拿到 `data_dir` 却未必拿得到 State，
// 而缓存键就是 data_dir。这是**纯性能优化**，丢了最多慢一点，不影响正确性。

fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 拿摘要段（带缓存）。这是 `run()` 调的那个入口。
pub async fn cached_summary(data_dir: &Path, cwd: &Path) -> String {
    let key = data_dir.display().to_string();
    {
        let g = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, text)) = g.get(&key) {
            if at.elapsed().as_secs() < CACHE_TTL_SECS {
                return text.clone();
            }
        }
    }
    let snaps = read_all(data_dir, cwd).await;
    let text = summary_section(&snaps);
    {
        let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
        g.insert(key, (std::time::Instant::now(), text.clone()));
    }
    text
}

/// 清缓存（设置页改完源之后调，让下次构建 prompt 立刻生效）。
pub fn invalidate_cache() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("orbcat_life_{tag}_{stamp}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn src_file(id: &str, path: &Path) -> Source {
        Source {
            id: id.into(),
            label: "测试源".into(),
            enabled: true,
            kind: Kind::File,
            path: path.to_string_lossy().to_string(),
            command: String::new(),
            items: "$.works[*]".into(),
            map: [
                ("title".to_string(), "$.work".to_string()),
                ("group".to_string(), "$.course".to_string()),
                ("dueRaw".to_string(), "$.left".to_string()),
                ("note".to_string(), "$.summary".to_string()),
            ]
            .into_iter()
            .collect(),
            stale_hours: Some(48.0),
        }
    }

    // ── JSON 路径 ──────────────────────────────────────────────────────

    #[test]
    fn get_path_walks_dots_and_indices() {
        let v = json!({"a": {"b": [10, 20, 30]}});
        assert_eq!(get_path(&v, "$.a.b[1]").unwrap(), &json!(20));
        assert_eq!(get_path(&v, "$.a.b").unwrap().as_array().unwrap().len(), 3);
        // `$` 与空串都是整份文档
        assert_eq!(get_path(&v, "$").unwrap(), &v);
        assert_eq!(get_path(&v, "").unwrap(), &v);
        // 走错路 → None（不 panic）
        assert!(get_path(&v, "$.nope").is_none());
        assert!(get_path(&v, "$.a.b[9]").is_none());
    }

    #[test]
    fn get_path_star_returns_the_array_itself() {
        let v = json!({"works": [{"x": 1}, {"x": 2}]});
        let got = get_path(&v, "$.works[*]").unwrap();
        assert!(got.is_array());
        assert_eq!(got.as_array().unwrap().len(), 2);
        // 对非数组用 `[*]` → None（而不是静默给个空）
        let s = json!({"works": "not-an-array"});
        assert!(get_path(&s, "$.works[*]").is_none());
    }

    #[test]
    fn get_items_accepts_three_shapes() {
        // 1) 顶层数组
        let top = json!([{"t": "a"}, {"t": "b"}]);
        assert_eq!(get_items(&top, "").len(), 2);
        // 2) `$.a[*]`
        let nested = json!({"works": [{"t": "a"}]});
        assert_eq!(get_items(&nested, "$.works[*]").len(), 1);
        // 3) `$.a`（没写通配也认）
        assert_eq!(get_items(&nested, "$.works").len(), 1);
        // 4) 非数组 → 当单条目（"一个 JSON 就是一个条目"是合理用法）
        let one = json!({"work": "只有一条"});
        assert_eq!(get_items(&one, "$.work").len(), 1);
    }

    /// 一个坏路径不该把整份文档灌进上下文。
    #[test]
    fn get_items_caps_at_max() {
        let many: Vec<Value> = (0..(MAX_ITEMS + 50)).map(|i| json!({"t": i})).collect();
        let v = json!({"works": many});
        assert_eq!(get_items(&v, "$.works[*]").len(), MAX_ITEMS);
    }

    #[test]
    fn value_text_covers_non_strings() {
        assert_eq!(value_text(&json!("  中文  ")), "中文");
        assert_eq!(value_text(&json!(42)), "42");
        assert_eq!(value_text(&json!(true)), "true");
        assert_eq!(value_text(&json!(null)), "");
        // 复杂值压缩成 JSON（不丢信息）
        assert!(value_text(&json!({"a": 1})).contains("\"a\""));
    }

    // ── 条目映射 ───────────────────────────────────────────────────────

    #[test]
    fn map_item_maps_all_four_fields() {
        let raw = json!({
            "work": "实验四", "course": "软件文档写作",
            "left": "剩余1453小时59分钟", "summary": "[软件文档写作] 作业"
        });
        let mut map = BTreeMap::new();
        map.insert("title".into(), "$.work".into());
        map.insert("group".into(), "$.course".into());
        map.insert("dueRaw".into(), "$.left".into());
        map.insert("note".into(), "$.summary".into());
        let it = map_item(&raw, &map);
        assert_eq!(it.title, "实验四");
        assert_eq!(it.group, "软件文档写作");
        assert_eq!(it.due_raw, "剩余1453小时59分钟");
        assert_eq!(it.note, "[软件文档写作] 作业");
    }

    /// 缺 title 时必须兜底 —— 空标题的条目会让用户以为数据坏了。
    #[test]
    fn map_item_falls_back_when_title_missing() {
        let mut map = BTreeMap::new();
        map.insert("title".into(), "$.nope".into());
        map.insert("group".into(), "$.course".into());
        let it = map_item(&json!({"course": "高数"}), &map);
        assert_eq!(it.title, "高数", "没标题就用分组");

        // 连分组也没有 → 整条 JSON 兜底（难看但不空）
        let it2 = map_item(&json!({"z": 1}), &map);
        assert!(!it2.title.is_empty(), "绝不该出现空标题");
    }

    /// **端到端：真实的 campus-monitor 快照形状**
    /// （`{works:[{work,left,summary,course}], updated_at}`）。
    #[test]
    fn parses_the_real_campus_monitor_shape() {
        let doc = json!({
            "works": [
                {"work": "实验四：项目验收总结报告的撰写",
                 "left": "剩余1453小时59分钟",
                 "summary": "[软件文档写作] 作业《实验四…》",
                 "course": "软件文档写作"},
                {"work": "实验三：详细设计说明书的撰写",
                 "left": "剩余1069小时59分钟",
                 "summary": "[软件文档写作] 作业《实验三…》",
                 "course": "软件文档写作"}
            ],
            "updated_at": "2026-09-30 22:00:12"
        });
        let s = src_file("xuexitong", Path::new("x.json"));
        let items = parse_document(&doc, &s);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "实验四：项目验收总结报告的撰写");
        assert_eq!(items[0].group, "软件文档写作");
        assert!(items[0].due_raw.starts_with("剩余"));
        assert_eq!(self_reported_time(&doc), "2026-09-30 22:00:12");
    }

    // ── 文件源 ─────────────────────────────────────────────────────────

    #[test]
    fn read_file_source_reads_and_maps() {
        let d = tmp("readfile");
        let snap = d.join("snap.json");
        std::fs::write(
            &snap,
            r#"{"works":[{"work":"作业A","course":"高数","left":"剩余3小时"}],"updated_at":"2026-10-01 10:00:00"}"#,
        )
        .unwrap();

        let s = src_file("x", &snap);
        let out = read_file_source(&s);
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].title, "作业A");
        assert_eq!(out.updated_at, "2026-10-01 10:00:00");
        // 刚写的文件 → 年龄接近 0，不该被判过期
        assert!(!out.stale, "刚写的快照不该算过期");
        assert!(out.age_hours.unwrap() < 1.0);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_file_source_reports_missing_file_without_panicking() {
        let s = src_file("x", Path::new("D:/definitely/not/here/xyz.json"));
        let out = read_file_source(&s);
        assert!(!out.ok);
        assert!(out.error.contains("读不到快照"), "{}", out.error);
        assert!(out.items.is_empty());
    }

    #[test]
    fn read_file_source_reports_bad_json() {
        let d = tmp("badjson");
        let snap = d.join("s.json");
        std::fs::write(&snap, "{ not json").unwrap();
        let out = read_file_source(&src_file("x", &snap));
        assert!(!out.ok);
        assert!(out.error.contains("不是合法 JSON"), "{}", out.error);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 过期判定：`staleHours` 设成 0 → 任何文件都算过期。
    #[test]
    fn stale_flag_respects_threshold() {
        let d = tmp("stale");
        let snap = d.join("s.json");
        std::fs::write(&snap, r#"{"works":[]}"#).unwrap();
        let mut s = src_file("x", &snap);
        s.stale_hours = Some(0.0);
        let out = read_file_source(&s);
        assert!(out.ok);
        assert!(out.stale, "阈值为 0 时任何快照都算过期");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 没配 `staleHours` → 永不判过期（用户没要求就别自作主张标黄）。
    #[test]
    fn no_stale_threshold_means_never_stale() {
        let d = tmp("nostale");
        let snap = d.join("s.json");
        std::fs::write(&snap, r#"{"works":[]}"#).unwrap();
        let mut s = src_file("x", &snap);
        s.stale_hours = None;
        let out = read_file_source(&s);
        assert!(!out.stale);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 摘要 ───────────────────────────────────────────────────────────

    #[test]
    fn summary_line_shows_count_and_freshness() {
        let snap = Snapshot {
            id: "x".into(),
            label: "学习通".into(),
            ok: true,
            error: String::new(),
            updated_at: "2026-09-30 22:00:12".into(),
            age_hours: Some(15.8),
            stale: false,
            items: vec![Item::default(), Item::default()],
        };
        let l = summary_line(&snap);
        assert!(l.contains("学习通"), "{l}");
        assert!(l.contains("2 条"), "{l}");
        assert!(l.contains("15.8 小时前"), "{l}");
        assert!(!l.contains("过期"), "没过期就不该标: {l}");
    }

    /// 过期必须在摘要里**明说** —— 模型把旧快照当最新事实是最糟的失败模式。
    #[test]
    fn summary_line_flags_stale() {
        let snap = Snapshot {
            id: "x".into(),
            label: "学习通".into(),
            ok: true,
            error: String::new(),
            updated_at: String::new(),
            age_hours: Some(100.0),
            stale: true,
            items: vec![],
        };
        assert!(summary_line(&snap).contains("可能已过期"));
    }

    /// 源挂了也必须**明说**，不能静默消失。
    #[test]
    fn summary_line_reports_failure_explicitly() {
        let snap = Snapshot::fail(&src_file("x", Path::new("z")), "读不到快照 D:/z");
        let l = summary_line(&snap);
        assert!(l.contains("读取失败"), "{l}");
        assert!(l.contains("读不到快照"), "{l}");
    }

    #[test]
    fn summary_section_is_empty_without_sources() {
        assert_eq!(summary_section(&[]), "");
    }

    #[test]
    fn summary_section_teaches_when_to_call_the_tool() {
        let snaps = vec![Snapshot {
            id: "x".into(),
            label: "学习通".into(),
            ok: true,
            error: String::new(),
            updated_at: String::new(),
            age_hours: Some(1.0),
            stale: false,
            items: vec![Item::default()],
        }];
        let s = summary_section(&snaps);
        assert!(s.contains("数据源"), "{s}");
        // 必须告诉模型"细节去调工具"，否则它会拿条数编内容
        assert!(s.contains("life_items"), "{s}");
        // 必须提醒"过期别当事实"
        assert!(s.contains("过期"), "{s}");
    }

    /// 摘要必须**短**（它是每轮常驻的）。这条防以后有人往里塞全文。
    #[test]
    fn summary_stays_compact() {
        let snaps: Vec<Snapshot> = (0..5)
            .map(|i| Snapshot {
                id: format!("s{i}"),
                label: format!("源{i}"),
                ok: true,
                error: String::new(),
                updated_at: String::new(),
                age_hours: Some(1.0),
                stale: false,
                items: vec![Item::default(); 3],
            })
            .collect();
        let s = summary_section(&snaps);
        // 5 个源 + 说明文字，控制在 ~600 字符内
        assert!(s.chars().count() < 600, "摘要太长了（{} 字符）:\n{s}", s.chars().count());
    }

    /// 超长标签要被截短（防一条脏数据把 prompt 撑坏）。
    #[test]
    fn summary_line_clips_absurd_labels() {
        let snap = Snapshot {
            id: "x".into(),
            label: "很".repeat(500),
            ok: true,
            error: String::new(),
            updated_at: String::new(),
            age_hours: None,
            stale: false,
            items: vec![],
        };
        let l = summary_line(&snap);
        assert!(l.chars().count() < 100, "标签没被截短: {} 字符", l.chars().count());
        assert!(l.contains('…'));
    }

    // ── 配置 ───────────────────────────────────────────────────────────

    #[test]
    fn missing_config_means_no_sources() {
        let d = tmp("noconf");
        assert!(load(&d).sources.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn broken_config_means_no_sources_not_a_panic() {
        let d = tmp("badconf");
        std::fs::write(life_path(&d), "{ not json").unwrap();
        assert!(load(&d).sources.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn default_file_text_round_trips() {
        let d = tmp("roundtrip");
        std::fs::write(life_path(&d), default_file_text()).unwrap();
        let f = load(&d);
        assert!(f.sources.is_empty(), "模板默认不带源（用户自己加）");
        // 说明块不能把 JSON 弄坏
        let txt = std::fs::read_to_string(life_path(&d)).unwrap();
        assert!(txt.contains("_comment"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **默认必须是不启用** —— `command` 类会自动跑，不能默认开。
    #[test]
    fn enabled_defaults_to_false() {
        let d = tmp("defoff");
        std::fs::write(
            life_path(&d),
            r#"{"sources":[{"id":"a","kind":"file","path":"x.json"}]}"#,
        )
        .unwrap();
        let f = load(&d);
        assert_eq!(f.sources.len(), 1);
        assert!(!f.sources[0].enabled, "缺 enabled 必须默认 false");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn kind_is_case_insensitive_and_defaults_to_file() {
        let d = tmp("kindcase");
        std::fs::write(
            life_path(&d),
            r#"{"sources":[
                 {"id":"a","kind":"COMMAND","command":"x"},
                 {"id":"b","kind":"File","path":"y"},
                 {"id":"c","path":"z"}
               ]}"#,
        )
        .unwrap();
        let f = load(&d);
        assert_eq!(f.sources[0].kind, Kind::Command);
        assert_eq!(f.sources[1].kind, Kind::File);
        assert_eq!(f.sources[2].kind, Kind::File, "缺 kind 默认 file（最安全）");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn is_complete_checks_the_right_field_per_kind() {
        let mut s = src_file("x", Path::new("p.json"));
        assert!(s.is_complete());
        s.path = "  ".into();
        assert!(!s.is_complete(), "file 类缺 path 算不完整");

        let mut c = Source {
            id: "c".into(),
            label: String::new(),
            enabled: true,
            kind: Kind::Command,
            path: String::new(),
            command: "echo 1".into(),
            items: String::new(),
            map: BTreeMap::new(),
            stale_hours: None,
        };
        assert!(c.is_complete());
        c.command = String::new();
        assert!(!c.is_complete(), "command 类缺 command 算不完整");
    }

    #[test]
    fn name_falls_back_to_id() {
        let mut s = src_file("xuexitong", Path::new("p"));
        s.label = "   ".into();
        assert_eq!(s.name(), "xuexitong");
        s.label = "学习通".into();
        assert_eq!(s.name(), "学习通");
    }

    #[test]
    fn save_then_load_round_trips() {
        let d = tmp("save");
        let f = LifeFile {
            sources: vec![src_file("x", Path::new("p.json"))],
        };
        save(&d, &f).unwrap();
        let back = load(&d);
        assert_eq!(back.sources.len(), 1);
        assert_eq!(back.sources[0].id, "x");
        assert!(back.sources[0].enabled);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 只读已启用的源 ─────────────────────────────────────────────────

    /// 未启用的源**不该被读取**（尤其 command 类：那等于偷偷跑命令）。
    #[tokio::test]
    async fn read_all_skips_disabled_sources() {
        let d = tmp("skipdis");
        let snap = d.join("s.json");
        std::fs::write(&snap, r#"{"works":[{"work":"A"}]}"#).unwrap();
        let mut on = src_file("on", &snap);
        on.enabled = true;
        let mut off = src_file("off", &snap);
        off.enabled = false;
        save(&d, &LifeFile { sources: vec![on, off] }).unwrap();

        let out = read_all(&d, &d).await;
        assert_eq!(out.len(), 1, "只该读启用的那个");
        assert_eq!(out[0].id, "on");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 配置不完整的源：报错但**不 panic**（用户存了半成品是常态）。
    #[tokio::test]
    async fn incomplete_source_reports_error() {
        let mut s = src_file("bad", Path::new(""));
        s.path = String::new();
        let out = read_source(&s, Path::new(".")).await;
        assert!(!out.ok);
        assert!(out.error.contains("缺 path"), "{}", out.error);
    }

    #[test]
    fn expand_path_handles_slashes_and_tilde() {
        let p = expand_path("D:\\a\\b.json");
        assert!(p.to_string_lossy().contains("a/b.json") || p.to_string_lossy().contains("a\\b.json"));
        // `~` 展开到用户目录（本机一定有）
        let t = expand_path("~/x.json");
        assert!(!t.to_string_lossy().starts_with('~'), "~ 没展开: {}", t.display());
    }

    /// 缓存：同一个 data_dir 连续问两次，第二次应命中缓存（不重读文件）。
    ///
    /// 验证方式：先读一次拿到内容，然后把**文件删掉**再读一次 ——
    /// 命中缓存的话仍然拿得到旧内容；没缓存就会变成"读取失败"。
    #[tokio::test]
    async fn cached_summary_reuses_result_within_ttl() {
        let d = tmp("cache");
        let snap = d.join("s.json");
        std::fs::write(&snap, r#"{"works":[{"work":"缓存测试"}]}"#).unwrap();
        save(
            &d,
            &LifeFile {
                sources: vec![src_file("c", &snap)],
            },
        )
        .unwrap();
        invalidate_cache();

        let first = cached_summary(&d, &d).await;
        assert!(first.contains("1 条"), "{first}");

        // 删掉快照 → 缓存内仍应拿到旧结果
        std::fs::remove_file(&snap).unwrap();
        let second = cached_summary(&d, &d).await;
        assert_eq!(first, second, "TTL 内应命中缓存（不重读文件）");

        // 清缓存后 → 变成读取失败
        invalidate_cache();
        let third = cached_summary(&d, &d).await;
        assert!(third.contains("读取失败"), "清缓存后该重新读: {third}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 没有任何源 → 摘要段是空串（整段不出现，不浪费 token）。
    #[tokio::test]
    async fn cached_summary_empty_without_sources() {
        let d = tmp("nosrc");
        invalidate_cache();
        assert_eq!(cached_summary(&d, &d).await, "");
        let _ = std::fs::remove_dir_all(&d);
    }
}
