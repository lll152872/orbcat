//! 模型配置加载
//!
//! ## 数据源优先级
//!   1. 环境变量 `ORBCAT_MODELS_JSON` 指向的文件
//!   2. `<data_dir>/models.json` —— 用户自己的模型列表（**含 API key，已 gitignore**）
//!      这是**主数据源**：只要存在就用它，不再往后找。
//!   3. `%USERPROFILE%\.workbuddy\models.json`（WorkBuddy 现有的模型列表，只读导入）
//!
//! ## ⚠️ 与"初始化"的关系（重要）
//! `models.json` **必须由用户先配好，agent 才能跑**。所以它不能是
//! "agent 初始化时才生成"的产物 —— 那样就死锁了（没模型 → 初始化跑不了）。
//!
//! 流程：`初始化` 命令建一个**空数组** `[]` 的 `models.json`，
//! 用户在设置页添加模型 → 有了模型 → 之后 agent 才能读 `初始化.md` 补人格文件。
//!
//! ## ⚠️ 安全
//! 这些文件含 **API key**，属于敏感数据：
//!   - 只在**运行时**读取，绝不写入本仓库
//!   - 日志里一律用 `redacted_key()`，不打印明文
//!   - 前端拿到的模型列表**不含 apiKey 字段**

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// key 只留头尾（日志 / 前端安全展示共用）
pub fn redact_key_str(k: &str) -> String {
    if k.len() <= 10 {
        return "***".into();
    }
    format!("{}...{}", &k[..6], &k[k.len() - 4..])
}

/// 一个模型配置（对应 models.json 里的一项）
///
/// **身份 = (Base URL, 接口 id)**（2026-10-02 定稿）：同一组（同 Base URL）下
/// 接口 id 唯一，**跨组允许同名**——glm-5.3-flash 可以同时存在于火山两个套餐组。
/// 旧设计里的「显示名 name」已废弃：models.json 里残留的 `"name"` 键会被
/// serde 静默忽略（无 `deny_unknown_fields`），不需要迁移脚本。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// 接口模型 ID（发给提供商的 `model` 字段）；**组内唯一、跨组可重复**
    pub id: String,
    #[serde(default)]
    pub vendor: String,
    /// OpenAI 兼容的 base url，例如 `https://token.sensenova.cn/v1`
    pub url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub supports_tool_call: bool,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// **额外的请求头**。
    ///
    /// 为什么需要：有些 OpenAI 兼容网关会在标准协议之外要求自定义头。
    /// 典型例子是 OpenCode Zen 的 Go 网关 —— 它强制要求
    /// `x-opencode-session`（用于把同一会话的请求路由到同一上游、提高 prompt 缓存命中）。
    /// 这类需求不该写死进代码，所以开放成配置项。
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl ModelConfig {
    /// 用于日志/前端的安全展示：key 只留头尾
    pub fn redacted_key(&self) -> String {
        redact_key_str(&self.api_key)
    }

    /// **实际发出的请求头** = 用户配置的头 + 按需自动补的头。
    ///
    /// 自动补的是 OpenCode Zen（`opencode.ai/zen`）要求的 `x-opencode-session`：
    /// 这个头用于把同一会话的请求钉到同一个上游 provider，从而让 prompt 缓存命中
    /// （官方称热门模型能到 90%+）。**缺了它请求直接 400**。
    ///
    /// 值取进程级稳定 ID —— 对悬浮球来说「一次运行」就是一个会话，
    /// 这样整个进程内所有请求共享同一个 session，缓存亲和最好。
    /// 用户若在配置里自己写了这个头，以用户的为准。
    pub fn effective_headers(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let is_opencode = self.url.contains("opencode.ai");
        let has_session = out
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-session"));

        if is_opencode && !has_session {
            out.push(("x-opencode-session".into(), session_id().to_string()));
            // 上游用量规范也建议标识客户端
            if !out.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-client")) {
                out.push(("x-opencode-client".into(), "orbcat".into()));
            }
        }

        out
    }

    /// 拼出 chat completions 端点
    pub fn chat_endpoint(&self) -> String {
        let base = self.url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }
}

/// Base URL → `GET /models` 端点。用户常填 `https://host/v1`，少数只填 host。
pub fn models_endpoint_from_base(url: &str) -> String {
    let base = url.trim().trim_end_matches('/');
    format!("{base}/models")
}

/// 进程级稳定的 session id。
///
/// 不引 uuid 依赖 —— 时间戳 + 进程号足够唯一，且天然「每次启动不同、同进程内稳定」。
/// 格式取 32 位十六进制，与 OpenCode 文档里 `sha256(...).slice(0, 32)` 的示例形状一致。
pub fn session_id() -> &'static str {
    use std::sync::OnceLock;
    static SID: OnceLock<String> = OnceLock::new();
    SID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        // 简单混合：时间戳与进程号错开，取 32 位 hex
        let mixed = nanos
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(pid.wrapping_mul(0xBF58_476D_1CE4_E5B9));
        format!("{mixed:032x}")
    })
}

/// 给前端的模型视图（**不含 apiKey**）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelView {
    pub id: String,
    pub vendor: String,
    pub url: String,
    pub supports_tool_call: bool,
    pub supports_images: bool,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// 是否已配置 Key（本地/Ollama 可无 Key）
    pub has_key: bool,
}

impl From<&ModelConfig> for ModelView {
    fn from(m: &ModelConfig) -> Self {
        Self {
            id: m.id.clone(),
            vendor: m.vendor.clone(),
            url: m.url.clone(),
            supports_tool_call: m.supports_tool_call,
            supports_images: m.supports_images,
            max_input_tokens: m.max_input_tokens,
            max_output_tokens: m.max_output_tokens,
            has_key: !m.api_key.trim().is_empty(),
        }
    }
}

/// 解析 **读取** models.json 的路径。
///
/// 为什么收 `data_dir` 参数（而不是像以前那样写死 `../../agent-data`）：
/// 写死的路径有两个问题 ——
///   ① `env!("CARGO_MANIFEST_DIR")` 是**编译期常量**，源码一挪位置就失效；
///   ② 发布版（exe）跑起来时根本没有源码树，那条路径永远不存在，
///      于是悄悄退到 `~/.workbuddy/models.json`，用户会以为"我的配置丢了"。
/// 改成用运行时解析出来的 `data_dir`，两种形态（开发 / 发布）行为一致。
pub fn models_json_path(data_dir: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("ORBCAT_MODELS_JSON") {
        let p = PathBuf::from(p);
        if p.exists() {
            return p;
        }
    }

    // 主数据源：用户自己的模型列表
    let local = local_models_path(data_dir);
    if local.exists() {
        return local;
    }

    // 只读导入：WorkBuddy 的模型列表（首次使用时帮用户省去手抄）
    if let Ok(home) = std::env::var("USERPROFILE") {
        let wb = PathBuf::from(home).join(".workbuddy").join("models.json");
        if wb.exists() {
            return wb;
        }
    }

    // 都没有 → 返回本地路径（不存在），让 `load_models` 给出准确报错
    local
}

/// **本地写入路径**：`<data_dir>/models.json`。
///
/// 一旦用户在设置里增删过模型，这里就成为主数据源（首次保存会把
/// 当时读到的完整列表拷贝过来，之后 WorkBuddy 的文件不再参与）。
fn local_models_path(data_dir: &Path) -> PathBuf {
    data_dir.join("models.json")
}

/// 保存完整模型列表到本地（含 apiKey，⚠️ 此文件绝不能进仓库）
pub fn save_models(data_dir: &Path, models: &[ModelConfig]) -> Result<PathBuf, String> {
    let path = local_models_path(data_dir);
    let txt = serde_json::to_string_pretty(models).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&path, txt).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    Ok(path)
}

/// 规范化 Base URL：去空白、去尾斜杠；缺 scheme 时补 `https://`（`localhost`/`127.0.0.1` 补 `http://`）。
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let u = raw.trim().trim_end_matches('/').to_string();
    if u.is_empty() {
        return Err("URL 不能为空".into());
    }
    let with_scheme = if u.starts_with("http://") || u.starts_with("https://") {
        u
    } else if u.starts_with("localhost")
        || u.starts_with("127.0.0.1")
        || u.starts_with("0.0.0.0")
        || u.starts_with("[::1]")
    {
        format!("http://{u}")
    } else {
        format!("https://{u}")
    };
    if !with_scheme.starts_with("http://") && !with_scheme.starts_with("https://") {
        return Err("URL 必须以 http:// 或 https:// 开头".into());
    }
    Ok(with_scheme)
}

/// 新增（或覆盖）一个模型。**身份 = (Base URL, 接口 id)**。
///
/// - 同组（同 Base URL）同接口 id = 更新这一条；否则新增
/// - 跨组同名完全合法（glm-5.3-flash 可同时存在于火山两个套餐组）
/// - API Key 允许为空（Ollama / 本地网关）
pub fn add_model(data_dir: &Path, m: ModelConfig) -> Result<(), String> {
    let mut m = m;
    if m.id.trim().is_empty() {
        return Err("接口模型 ID 不能为空".into());
    }
    m.id = m.id.trim().to_string();
    if m.url.trim().is_empty() {
        return Err("URL 不能为空".into());
    }
    let want_url = url_key(&m.url);
    let mut all = load_models(data_dir)?;
    match all
        .iter_mut()
        .find(|x| x.id == m.id && url_key(&x.url) == want_url)
    {
        Some(existing) => *existing = m,
        None => all.push(m),
    }
    save_models(data_dir, &all)?;
    Ok(())
}

/// 删除一个模型（按 **(Base URL, 接口 id)** 定位）。
pub fn remove_model(data_dir: &Path, url: &str, id: &str) -> Result<(), String> {
    let mut all = load_models(data_dir)?;
    let want_url = url_key(url);
    let before = all.len();
    all.retain(|x| !(x.id == id && url_key(&x.url) == want_url));
    if all.len() == before {
        return Err(format!("找不到模型「{id}」@ {url}"));
    }
    save_models(data_dir, &all)?;
    Ok(())
}

/// 读取全部模型配置。
///
/// `data_dir` 是解析路径的基准 —— 见 [`models_json_path`]。
/// 文件不存在时返回**空列表**而不是报错：全新克隆的仓库里
/// `models.json` 要么不存在，要么是初始化时写的 `[]`，
/// 两种情况都等价于「一个模型都没配」，UI 该引导用户去添加，
/// 而不是弹一条红字错误。
pub fn load_models(data_dir: &Path) -> Result<Vec<ModelConfig>, String> {
    let path = models_json_path(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let txt = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;

    let models: Vec<ModelConfig> =
        serde_json::from_str(&txt).map_err(|e| format!("解析 {} 失败: {e}", path.display()))?;

    Ok(models)
}

/// 批量导入远端模型列表里的若干 id。
///
/// 共用 `source` 的 url / apiKey / headers。
///
/// **身份 = (Base URL, 接口 id)**：同源同 id 才算「已配置」→ 跳过；
/// 不同源的同一个 `model`（官方 vs 火山 vs 本地网关）**各自独立入库**。
/// 返回 `(added, skipped)` —— 均为**接口 id**。
pub fn import_models_from_source(
    data_dir: &Path,
    source: &ModelConfig,
    ids: &[String],
    supports_tool_call: bool,
    supports_images: bool,
    context_lengths: &std::collections::HashMap<String, u64>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut all = load_models(data_dir)?;
    let src_url = url_key(&source.url);
    let mut added = Vec::new();
    let mut skipped = Vec::new();

    for raw in ids {
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        // 身份 = (Base URL, 接口 id)：同源同 id 才跳过
        if all
            .iter()
            .any(|m| m.id == id && url_key(&m.url) == src_url)
        {
            skipped.push(id.to_string());
            continue;
        }
        let vendor = if source.vendor.trim().is_empty() {
            "Custom".to_string()
        } else {
            source.vendor.trim().to_string()
        };
        let max_input = context_lengths.get(id).copied().or(source.max_input_tokens);
        all.push(ModelConfig {
            id: id.to_string(),
            vendor,
            url: source.url.clone(),
            api_key: source.api_key.clone(),
            supports_tool_call,
            supports_images,
            max_input_tokens: max_input,
            max_output_tokens: source.max_output_tokens,
            headers: source.headers.clone(),
        });
        added.push(id.to_string());
    }

    if !added.is_empty() {
        save_models(data_dir, &all)?;
    }
    Ok((added, skipped))
}

/// 编辑已有模型（按 **(Base URL, 接口 id)** 定位）。
/// - `api_key` 为 None/空串时**保持原 Key**；`clear_key: true` 清空
/// - `new_id`：改发给提供商的 `model` 字段；同组内若已有同 id 条目则报错
///   （跨组同名不受限）
/// - `max_*` 为 `Some(None)` 清空；`None` 不动
pub fn update_model(
    data_dir: &Path,
    url_key_str: &str,
    id_key: &str,
    url: Option<String>,
    api_key: Option<String>,
    clear_key: bool,
    supports_tool_call: Option<bool>,
    supports_images: Option<bool>,
    headers: Option<std::collections::BTreeMap<String, String>>,
    max_input_tokens: Option<Option<u64>>,
    max_output_tokens: Option<Option<u64>>,
    api_model_id: Option<String>,
) -> Result<(), String> {
    let mut all = load_models(data_dir)?;
    let want_url = url_key(url_key_str);
    let Some(idx) = all
        .iter()
        .position(|x| x.id == id_key && url_key(&x.url) == want_url)
    else {
        return Err(format!("找不到模型「{id_key}」@ {url_key_str}"));
    };
    // 先处理「改接口 id」（要全表查重，不能在 &mut all[idx] 借用期间做）：
    // 组内查重 —— 同 Base URL 下该接口 id 已被另一条占用 → 拒绝；跨组同名不受限。
    if let Some(mid) = api_model_id {
        let mid = mid.trim();
        if !mid.is_empty() && mid != all[idx].id {
            let target_url = url_key(&all[idx].url);
            if all
                .iter()
                .enumerate()
                .any(|(i, x)| i != idx && x.id == mid && url_key(&x.url) == target_url)
            {
                return Err(format!("该组下已存在接口模型 ID「{mid}」"));
            }
            all[idx].id = mid.to_string();
        }
    }
    let m = &mut all[idx];
    if let Some(u) = url {
        m.url = normalize_base_url(&u)?;
    }
    if clear_key {
        m.api_key = String::new();
    } else if let Some(k) = api_key {
        let k = k.trim().to_string();
        if !k.is_empty() {
            m.api_key = k;
        }
    }
    if let Some(t) = supports_tool_call {
        m.supports_tool_call = t;
    }
    if let Some(i) = supports_images {
        m.supports_images = i;
    }
    if let Some(h) = headers {
        m.headers = h;
    }
    if let Some(v) = max_input_tokens {
        m.max_input_tokens = v;
    }
    if let Some(v) = max_output_tokens {
        m.max_output_tokens = v;
    }
    save_models(data_dir, &all)?;
    Ok(())
}

/// 给编辑表单用的视图：**不回传完整 apiKey**，只给「有没有 Key + 掩码」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEditView {
    pub id: String,
    pub vendor: String,
    pub url: String,
    pub supports_tool_call: bool,
    pub supports_images: bool,
    /// 额外请求头，已格式化成 `Key: Value` 逐行文本
    pub headers_text: String,
    pub has_key: bool,
    pub key_preview: String,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

impl From<&ModelConfig> for ModelEditView {
    fn from(m: &ModelConfig) -> Self {
        let headers_text = m
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            id: m.id.clone(),
            vendor: m.vendor.clone(),
            url: m.url.clone(),
            supports_tool_call: m.supports_tool_call,
            supports_images: m.supports_images,
            headers_text,
            has_key: !m.api_key.trim().is_empty(),
            key_preview: redact_key_str(&m.api_key),
            max_input_tokens: m.max_input_tokens,
            max_output_tokens: m.max_output_tokens,
        }
    }
}

/// 按 **显示名优先、接口 id 兜底** 找模型。
///
/// 为什么要分两轮：接口 `id` 允许多条重复（同一个 `model` 经不同 Base URL
/// 是两条独立配置）。若用 `name == key || id == key` 一次遍历，命中的是
/// **第一条**，于是「点选手列表里的 `glm-5.3-flash`」会误开另一个同 id
/// 条目的编辑表单。显示名是唯一键，必须先精确匹配显示名。
/// 同组（同 Base URL，按 `url_key` 比较）里第一个**非空** Key。
///
/// 用户心智："一个提供商一个 Key"——组内 Key 必然一致。新模型 Key 留空时
/// 直接继承组里已有的 Key，而不是拿空 Key 去请求然后吃 401。
pub fn group_key_for_url(all: &[ModelConfig], url: &str) -> String {
    let want = url_key(url);
    all.iter()
        .find(|m| url_key(&m.url) == want && !m.api_key.trim().is_empty())
        .map(|m| m.api_key.clone())
        .unwrap_or_default()
}

/// 模型实际应使用的 API Key：自身非空用自身；否则回落**同组** Key。
///
/// 只在内存里解析，**不写回配置**——用户显式存下的空 Key（本地模型）不该被悄悄改写。
pub fn resolve_api_key(all: &[ModelConfig], target: &ModelConfig) -> String {
    if !target.api_key.trim().is_empty() {
        return target.api_key.clone();
    }
    group_key_for_url(all, &target.url)
}

/// 按 **接口 id** 找模型（裸 id 兜底查找）。
///
/// 身份已改为 (Base URL, 接口 id)，同 id 可跨组共存；本函数只在
/// **拿不到组上下文**时用（旧 project 槽位、agent 测试、settings 迁移兜底）：
/// - 恰好一条 → 命中
/// - 多条同名（同 id 跨组）→ 取第一条并打警告日志（歧义场景请走 `find_model_by_url_id`）
/// - 0 条 → 报错
///
/// 命中后同样应用组 Key 回落（见 [`resolve_api_key`]）。
pub fn find_model(data_dir: &Path, key: &str) -> Result<ModelConfig, String> {
    let models = load_models(data_dir)?;
    let hits: Vec<&ModelConfig> = models.iter().filter(|m| m.id == key).collect();
    let hit = match hits.len() {
        0 => return Err(format!("找不到模型「{key}」")),
        1 => hits[0],
        _ => {
            eprintln!(
                "[orbcat] ⚠️ 接口 id「{key}」在 {} 个组中存在，已取第一条（需要精确请按 组+id 定位）",
                hits.len()
            );
            hits[0]
        }
    };
    let mut m = hit.clone();
    m.api_key = resolve_api_key(&models, hit);
    Ok(m)
}

/// URL 归一化比较键：去空白、去尾斜杠、转小写。
/// **只用于比较**，不写回配置（原值保留用户的写法）。
pub fn url_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

/// 只取主机名（分组展示用）。
pub fn short_host(url: &str) -> String {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    let host = rest.split('/').next().unwrap_or(rest);
    host.rsplit('@').next().unwrap_or(host).to_string()
}

/// 按 **Base URL + 接口 id** 找模型 —— 这才是模型的真正身份。
///
/// 同一个 `model` 从不同 Base URL 拉进来是两条独立配置，UI 的选中 /
/// 编辑 / 删除 / 测试都按这个二元组精确定位。命中后应用组 Key 回落。
pub fn find_model_by_url_id(data_dir: &Path, url: &str, id: &str) -> Result<ModelConfig, String> {
    let models = load_models(data_dir)?;
    let want_url = url_key(url);
    let want_id = id.trim();
    let hit = models
        .iter()
        .find(|m| m.id == want_id && url_key(&m.url) == want_url)
        .ok_or_else(|| format!("找不到模型「{want_id}」@ {url}"))?;
    let mut m = hit.clone();
    m.api_key = resolve_api_key(&models, hit);
    Ok(m)
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            vendor: String::new(),
            url: String::new(),
            api_key: String::new(),
            supports_tool_call: false,
            supports_images: false,
            max_input_tokens: None,
            max_output_tokens: None,
            headers: Default::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// 运行时设置（选中的模型等）
// ---------------------------------------------------------------------------

/// 执行权限档位 —— 控制 `run_command` / MCP 工具**弹卡问用户的频率**。
///
/// 灵感来自 WorkBuddy 的 permission modes（default / acceptEdits / dontAsk /
/// bypassPermissions），但只收窄到「命令执行」一层：文件权限仍走
/// `permissions.json` 三层闸门 + `request_access` 申请卡，两者互不影响。
///
/// 三档语义（`command_policy::evaluate` 的三路判定之上再叠一层"要不要问"）：
///
/// | 档位 | 白名单 | 硬阻断 | 其余命令 | MCP 工具 |
/// |---|---|---|---|---|
/// | `ask`（默认） | 免问 | 拦 | **弹卡** | **弹卡**（未授权时） |
/// | `smart` | 免问 | 拦 | 低/中风险**免问**，高风险弹卡 | 弹卡 |
/// | `full` | 免问 | **仍拦** | 全部免问 | 全部免问 |
///
/// ⚠️ **硬阻断在任何档位下都拦** —— 对齐 WorkBuddy「`-y` 也不是真全通，
/// HIGH/CRITICAL 仍会确认」的设计；我们比它更严：直接不执行。
/// `full` 免掉的是"问用户"，不是安全底线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ExecTrust {
    /// 每次询问（默认）：非白名单命令全部弹确认卡
    #[default]
    Ask,
    /// 智能放行：低/中风险自动执行不弹卡，高风险仍弹卡
    Smart,
    /// 允许完全访问：命令与 MCP 全部免问（硬阻断仍拦）
    Full,
}

impl ExecTrust {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "smart" => Self::Smart,
            "full" => Self::Full,
            _ => Self::Ask,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Smart => "smart",
            Self::Full => "full",
        }
    }

    /// 这条被判 `Ask` 的命令（`risk` 已给出）要不要**免问直接跑**。
    pub fn auto_allows_command(&self, risk_high: bool) -> bool {
        match self {
            Self::Ask => false,
            Self::Smart => !risk_high,
            Self::Full => true,
        }
    }

    /// 未授权的 MCP 工具要不要免问。
    pub fn auto_allows_mcp(&self) -> bool {
        matches!(self, Self::Full)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentSettings {
    /// 当前选中的模型**接口 id**（身份 = (Base URL, id)，见 [`AgentSettings::selected_model_url`]）
    pub selected_model: Option<String>,
    /// 最近一次使用的会话标题（预留）
    pub last_session: Option<String>,
    /// 面板失去焦点时自动收起为悬浮球（防止一直挡屏幕）。
    /// 默认开 —— 悬浮球的本分是「用时展开、完事让路」。
    pub blur_collapse: bool,
    /// 快答模式：不发工具列表，system prompt 只带人格段（见
    /// `agent::build_quick_system_prompt`）。托盘菜单手动切换 —— 不做自动
    /// 启发式，误判（该带工具没带）比省那点 token 严重得多。
    #[serde(default)]
    pub quick_mode: bool,
    /// 当前激活的项目 id（pmem.projects）。`None` = 主对话（无项目记忆）。
    #[serde(default)]
    pub active_project_id: Option<i64>,
    /// 执行权限档位（管 `run_command` / MCP 工具的弹卡频率，见 [`ExecTrust`]）。
    /// 参考 WorkBuddy 的 permission modes，但收窄到「命令执行」这一层。
    #[serde(default)]
    pub exec_trust: ExecTrust,
    /// **MCP 工具组默认全体加载**（决策：不再按需注入）。
    ///
    /// 为什么改成默认全给：懒加载要求模型自己先 `list_tool_groups` 再
    /// `load_tool_group`，多两步、还经常忘了——用户观感就是"工具明明配了却用不上"。
    /// 现在改为直接全量塞进上下文，省掉这套仪式。
    /// 设 false 才回到按需加载（保底开关，避免超大工具集撑爆小窗口模型）。
    #[serde(default = "default_true")]
    pub mcp_all_groups: bool,
    /// MCP 组开关的**持久化记忆**（组名 → 是否启用）。
    ///
    /// 只有显式关过的组才会出现在这里（键存在且 false）；没记录的组按
    /// [`mcp_all_groups`] 决定 —— 默认全开。
    #[serde(default)]
    pub mcp_group_enabled: std::collections::BTreeMap<String, bool>,
    /// 模型分组的**显示名**（键 = Base URL）。默认取 host，用户可改。
    #[serde(default)]
    pub model_group_names: std::collections::BTreeMap<String, String>,
    /// 「模型管理」独立窗口的尺寸（逻辑像素）。用户拖拽缩放后记回来，
    /// 下次打开按这个尺寸弹 —— 改尺寸不需要重新编译。
    #[serde(default)]
    pub models_window: ModelsWindowCfg,
    /// 当前激活的**拼装项目**（`project.rs`，= `projects/<名>/project.json` 的目录名）。
    ///
    /// `None` = 无项目（默认）。
    ///
    /// ⚠️ 与 [`active_project_id`](Self::active_project_id) **不是一回事**，别看错：
    /// - `active_project_id`（i64）：**项目记忆**的绑定（`pmem.sqlite`），
    ///   决定注入哪份 `MEMORY.md`，随会话走。
    /// - `active_bundle`（String）：**拼装包**（能力清单 / 权限预设 / 脚本工具），
    ///   决定这一轮的工具面与闸门参数，**全局**生效。
    ///
    /// 两者可独立存在：一个项目可以只有记忆、没有拼装清单（那就只是记忆，
    /// 行为与从前完全一致）。这也是 spec 里"绑定语义不混淆"那条。
    #[serde(default)]
    pub active_bundle: Option<String>,
    /// 当前选中模型的 **Base URL**（与 `selected_model` 配对消歧）。
    ///
    /// 模型身份 = (Base URL, 接口 id)，同 id 可跨组共存；只有 id 时裸定位
    /// 取第一条。旧 settings.json 没有这个字段 → None（回落裸 id 查找）。
    #[serde(default)]
    pub selected_model_url: Option<String>,
}

/// ⚠️ 「当前 agent 模式」**不在 settings.json 里**（2026-10-08 起）。
///
/// 模式跟**会话**走：`Session.mode`（建会话时钉死），读法唯一入口 =
/// `sessions::effective_mode(data_dir, session_id)`，兜底是
/// `modes::DEFAULT_MODE_ID`（= `standard`）。
///
/// 这里曾经有个 `active_mode` 字段（三级演变：全局当前模式 → 新建会话的默认模式
/// → **彻底删掉**）。删它的理由：界面上已无任何入口写它，留着就是
/// **手编 settings.json 也不生效**那种"设置不生效"的经典困惑
/// —— 这个项目对那类 bug 是零容忍的（见 `modes_set` 当年的注释）。
/// 老 settings.json 里残留的 `"activeMode": "chat"` 会被 serde 直接忽略，
/// 不报错、也没有任何行为（`legacy_settings_active_mode_is_ignored` 锁这条）。

/// 「模型管理」独立窗口尺寸。默认 720×640；拖边缩放后写回 settings.json。
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelsWindowCfg {
    pub w: f64,
    pub h: f64,
}

impl Default for ModelsWindowCfg {
    fn default() -> Self {
        Self { w: 720.0, h: 640.0 }
    }
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            selected_model: None,
            last_session: None,
            blur_collapse: true,
            quick_mode: false,
            active_project_id: None,
            exec_trust: ExecTrust::Ask,
            mcp_all_groups: true,
            mcp_group_enabled: Default::default(),
            model_group_names: Default::default(),
            models_window: Default::default(),
            active_bundle: None,
            selected_model_url: None,
        }
    }
}

fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

// ---------------------------------------------------------------------------
// MCP 服务器列表（可单独添加多个）
// ---------------------------------------------------------------------------

/// MCP 传输方式。
///
/// `Auto`（缺省）按字段推断：填了 `command` → stdio，否则 → http。
/// 为什么留一个"自动"档而不是强制二选一：**存量 `mcp.json` 里只有 `url`**，
/// 强制用户补一个 `transport` 字段才能升级是没必要的破坏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// 按 url / command 哪个非空推断
    #[default]
    Auto,
    /// Streamable HTTP（连一个已经在跑的网关，如 1MCP）
    Http,
    /// 起子进程走 stdin/stdout 的 JSON-RPC（本机脚本 / exe 直接当工具）
    Stdio,
}

/// 一条 MCP 服务器配置。
///
/// ## 两种形态
/// - **HTTP**：填 `url`，连一个已经在跑的网关（如本机 1MCP `:3050`）。
/// - **Stdio**：填 `command` / `args` / `env`，由 orbcat **自己拉起子进程**，
///   走 stdin/stdout 的 JSON-RPC。这是「一个脚本 + 几行 JSON 就是工具」那条路 ——
///   不需要额外起 HTTP 网关。
///
/// ⚠️ 两种形态在**工具执行侧完全同权**：都经由 `mcp__<server>__<tool>` 命名、
/// 都走 `tools.rs` 里同一段 MCP 分支（权限卡 + 执行档位判定）。stdio 不新增信任面，
/// 它只是换了个"怎么把 JSON-RPC 送过去"的通道。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerCfg {
    /// 稳定 id（展示/路由用），用户可改
    pub id: String,
    /// HTTP 形态的网关地址。stdio 形态留空。
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 展示名（可空，空则用 id）
    #[serde(default)]
    pub label: String,
    /// 传输方式，缺省 `Auto`（按 url / command 推断）
    #[serde(default)]
    pub transport: McpTransport,
    /// stdio 形态：可执行文件 / 脚本路径
    #[serde(default)]
    pub command: String,
    /// stdio 形态：argv（**逐参传递，不经 shell**）
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio 形态：附加环境变量
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl McpServerCfg {
    /// 构造一条 HTTP server（兼容旧调用点，字段少写一半）。
    pub fn http(id: impl Into<String>, url: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            enabled: true,
            label: label.into(),
            transport: McpTransport::Http,
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
        }
    }

    /// 这条配置实际用哪种传输。
    ///
    /// `Auto` 的推断规则：`command` 非空 → Stdio；否则 Http。
    /// 显式写了 `transport` 就听显式的 —— 用户写死是有意的，
    /// 这时即使另一个字段也非空也按显式的走（校验器会另外警告）。
    pub fn resolved_transport(&self) -> McpTransport {
        match self.transport {
            McpTransport::Auto => {
                if !self.command.trim().is_empty() {
                    McpTransport::Stdio
                } else {
                    McpTransport::Http
                }
            }
            t => t,
        }
    }

    pub fn is_stdio(&self) -> bool {
        self.resolved_transport() == McpTransport::Stdio
    }

    /// 这条配置是否**可用**（有必要的字段）。
    ///
    /// 设置页允许存半成品（用户填一半去干别的），但**连接时**必须跳过它们，
    /// 否则会拿空 command 去 `Command::new("")` —— 那是个很难从报错里看懂的失败。
    pub fn is_connectable(&self) -> bool {
        if !self.enabled {
            return false;
        }
        if self.is_stdio() {
            !self.command.trim().is_empty()
        } else {
            !self.url.trim().is_empty()
        }
    }

    /// 给日志 / 错误信息用的一句话描述（**不含 env 值**，避免把密钥写进日志）。
    pub fn describe(&self) -> String {
        if self.is_stdio() {
            let args = if self.args.is_empty() {
                String::new()
            } else {
                format!(" {}", self.args.join(" "))
            };
            format!("stdio: {}{}", self.command, args)
        } else {
            format!("http: {}", self.url)
        }
    }
}

fn default_true() -> bool {
    true
}

/// `<data_dir>/mcp.json` 的多 server 格式。
///
/// 兼容旧的 `{"url","enabled"}` 单网关写法 —— 读入时自动升成单元素列表。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpFile {
    #[serde(default)]
    pub servers: Vec<McpServerCfg>,
}

fn mcp_path(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp.json")
}

/// 读 MCP 服务器列表（只返回**可连接的**，供连接用；管理界面用 [`load_mcp_servers`]）。
///
/// ⚠️ 这里必须用 [`McpServerCfg::is_connectable`] 而**不是**判 `url` 非空：
/// stdio 形态的 server 本来就**没有 url**（它填的是 `command`），
/// 按 url 过滤会把它们全部静默丢掉 —— 表现是"设置页里明明配了，
/// 重启后却一个 stdio 工具都没有"，且日志里看不出原因。
pub fn resolve_mcp_servers(data_dir: &Path) -> Vec<McpServerCfg> {
    load_mcp_servers(data_dir)
        .into_iter()
        .filter(|s| s.is_connectable())
        .collect()
}

/// 读全部 MCP 服务器（含禁用），设置页用。
///
/// 环境变量 `ORBCAT_MCP_URL` 若存在，会在列表最前插入一条临时启用项
/// （不落盘）——保持旧调试入口可用。
pub fn load_mcp_servers(data_dir: &Path) -> Vec<McpServerCfg> {
    let mut list = Vec::new();

    if let Ok(u) = std::env::var("ORBCAT_MCP_URL") {
        let u = u.trim().to_string();
        if !u.is_empty() {
            list.push(McpServerCfg::http("env", u, "环境变量 ORBCAT_MCP_URL"));
        }
    }

    let p = mcp_path(data_dir);
    if !p.exists() {
        return list;
    }
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return list;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return list;
    };

    // 新格式：{"servers":[...]}
    if let Ok(f) = serde_json::from_value::<McpFile>(v.clone()) {
        if !f.servers.is_empty() {
            list.extend(f.servers);
            return list;
        }
    }
    // 旧格式：{"url":"...","enabled":true}
    if let Some(u) = v.get("url").and_then(serde_json::Value::as_str) {
        let enabled = v.get("enabled").and_then(serde_json::Value::as_bool) != Some(false);
        if !u.trim().is_empty() {
            let mut cfg = McpServerCfg::http("default", u.trim(), "默认网关");
            cfg.enabled = enabled;
            list.push(cfg);
        }
    }
    list
}

/// 整表写回 `mcp.json`（多 server）。
pub fn save_mcp_servers(data_dir: &Path, servers: &[McpServerCfg]) -> Result<(), String> {
    let p = mcp_path(data_dir);
    let file = McpFile {
        servers: servers.to_vec(),
    };
    let txt = serde_json::to_string_pretty(&file).map_err(|e| format!("序列化失败: {e}"))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, txt).map_err(|e| format!("写入 {} 失败: {e}", p.display()))
}

/// 兼容旧接口：设置单条 URL（覆盖成仅此一条）。
pub fn save_mcp_url(data_dir: &Path, url: Option<&str>) -> Result<(), String> {
    let servers = match url {
        Some(u) if !u.trim().is_empty() => {
            vec![McpServerCfg::http("default", u.trim(), "默认网关")]
        }
        _ => Vec::new(),
    };
    save_mcp_servers(data_dir, &servers)
}

pub fn load_settings(data_dir: &Path) -> AgentSettings {
    let p = settings_path(data_dir);
    if !p.exists() {
        return AgentSettings::default();
    }
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_settings(data_dir: &Path, s: &AgentSettings) -> Result<(), String> {
    let p = settings_path(data_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let txt = serde_json::to_string_pretty(s).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&p, txt).map_err(|e| format!("写入 {} 失败: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录（带纳秒后缀防并发撞名）
    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "orbcat_cfg_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// **stdio server 不能因为没有 url 就被丢掉**。
    ///
    /// 回归测试：`resolve_mcp_servers` 早先按 `!url.trim().is_empty()` 过滤，
    /// 而 stdio 形态填的是 `command`、**url 是空的** —— 于是设置页里配好的
    /// stdio server 重启后会全部消失，且日志里只写"MCP 未配置"，
    /// 完全看不出是过滤条件的问题。
    #[test]
    fn resolve_keeps_stdio_servers_that_have_no_url() {
        let d = tmp_dir("resolve_stdio");
        let cfg = McpFile {
            servers: vec![
                McpServerCfg {
                    id: "script".into(),
                    url: String::new(), // ← stdio 没有 url
                    enabled: true,
                    label: "本机脚本".into(),
                    transport: McpTransport::Stdio,
                    command: "node".into(),
                    args: vec!["s.js".into()],
                    env: Default::default(),
                },
                McpServerCfg::http("gw", "http://127.0.0.1:3050/mcp", "网关"),
            ],
        };
        std::fs::write(
            mcp_path(&d),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();

        let out = resolve_mcp_servers(&d);
        assert_eq!(out.len(), 2, "stdio 与 http 都应留下：{out:#?}");
        assert!(
            out.iter().any(|s| s.id == "script" && s.is_stdio()),
            "stdio server 不该因为 url 为空被过滤掉"
        );
        assert!(out.iter().any(|s| s.id == "gw" && !s.is_stdio()));

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 半成品配置（只填了 id）在**连接侧**被跳过，但**仍在管理列表里**。
    ///
    /// 两个列表的口径不同是有意的：设置页要能看到并继续编辑半成品，
    /// 连接侧不能拿空 command/url 去连。
    #[test]
    fn resolve_skips_unconnectable_but_list_keeps_them() {
        let d = tmp_dir("resolve_partial");
        let cfg = McpFile {
            servers: vec![McpServerCfg {
                id: "half".into(),
                url: String::new(),
                enabled: true,
                label: "半成品".into(),
                transport: McpTransport::Auto,
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
            }],
        };
        std::fs::write(mcp_path(&d), serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

        assert!(
            resolve_mcp_servers(&d).is_empty(),
            "半成品不该进连接列表"
        );
        assert_eq!(
            load_mcp_servers(&d).len(),
            1,
            "半成品必须留在管理列表里（否则用户没法继续编辑）"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **禁用**的 server 不出现在连接列表。
    #[test]
    fn resolve_respects_enabled_flag() {
        let d = tmp_dir("resolve_disabled");
        let mut off = McpServerCfg::http("off", "http://x/mcp", "");
        off.enabled = false;
        let cfg = McpFile {
            servers: vec![off, McpServerCfg::http("on", "http://y/mcp", "")],
        };
        std::fs::write(mcp_path(&d), serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

        let out = resolve_mcp_servers(&d);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "on");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 老的 `settings.json` 必须能读 —— **包括它里面已经废弃的 `activeMode`**。
    ///
    /// 为什么单独立一条：`agent-data/settings.json` 是用户手改过的文件，
    /// 字段增删后**绝不能**因为反序列化失败而整体退回默认值 ——
    /// 那会把用户配好的模型/项目/权限全丢掉（`load_settings` 用的是
    /// `.unwrap_or_default()`，解析失败是**静默**的，最容易出事的写法）。
    ///
    /// 2026-10-08：`activeMode` 字段已从 `AgentSettings` 删掉（模式跟会话走），
    /// 所以这条的断言从"默认落到标准"改成"**残留的键被忽略、别的字段照旧生效**"。
    /// serde 默认忽略未知键 —— 这正是我们要的：老文件不用迁移、不报错、
    /// 也不产生任何"看起来能配、其实不生效"的假象。
    #[test]
    fn legacy_settings_active_mode_is_ignored() {
        let d = std::env::temp_dir().join(format!(
            "orbcat_cfg_mode_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        // 模拟"上一个版本写出来的 settings.json"：有 selectedModel，还留着 activeMode
        std::fs::write(
            settings_path(&d),
            r#"{"selectedModel":"火山agent","execTrust":"full","mcpAllGroups":true,"activeMode":"chat"}"#,
        )
        .unwrap();

        let s = load_settings(&d);
        assert_eq!(s.selected_model.as_deref(), Some("火山agent"), "老字段不能被丢掉");
        assert_eq!(s.exec_trust, ExecTrust::Full);

        // 关键：`activeMode` 现在**只是被忽略的陌生键**，不进任何地方。
        // 想要"当前会话是什么模式"只能问 `sessions::effective_mode`。
        let back = std::fs::read_to_string(settings_path(&d)).unwrap();
        assert!(back.contains("activeMode"), "文件是用户手写的，我们不动它");
        let parsed: serde_json::Value = serde_json::from_str(&back).unwrap();
        assert_eq!(parsed["activeMode"], "chat", "残留值原样留着，但没有任何行为");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn redact_hides_middle() {
        let m = ModelConfig {
            id: "x".into(),
            vendor: "".into(),
            url: "https://example.com/v1".into(),
            api_key: "sk-TESTKEY0001abcdef".into(),
            supports_tool_call: true,
            supports_images: false,
            ..Default::default()
        };
        let r = m.redacted_key();
        assert!(r.starts_with("sk-TE"));
        assert!(r.ends_with("cdef"));
        assert!(!r.contains("sk-TESTKEY0001abcdef"));
    }

    #[test]
    fn redact_short_key() {
        let m = ModelConfig {
            id: "x".into(),
            vendor: "".into(),
            url: "u".into(),
            api_key: "short".into(),
            supports_tool_call: false,
            supports_images: false,
            ..Default::default()
        };
        assert_eq!(m.redacted_key(), "***");
    }

    // ---- 额外请求头 / OpenCode session ----

    fn mk(url: &str, headers: &[(&str, &str)]) -> ModelConfig {
        ModelConfig {
            id: "t".into(),
            vendor: "".into(),
            url: url.into(),
            api_key: "k".into(),
            supports_tool_call: true,
            supports_images: false,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn get<'a>(hs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        hs.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn opencode_url_gets_session_header_automatically() {
        let m = mk("https://opencode.ai/zen/go/v1", &[]);
        let hs = m.effective_headers();
        let sid = get(&hs, "x-opencode-session").expect("应自动补 x-opencode-session");
        assert!(!sid.is_empty());
        // 顺带标识客户端
        assert_eq!(get(&hs, "x-opencode-client"), Some("orbcat"));
    }

    #[test]
    fn non_opencode_url_gets_no_session_header() {
        let m = mk("https://token.sensenova.cn/v1", &[]);
        let hs = m.effective_headers();
        assert!(get(&hs, "x-opencode-session").is_none(), "普通端点不该被塞头");
    }

    #[test]
    fn user_supplied_session_header_wins() {
        let m = mk(
            "https://opencode.ai/zen/go/v1",
            &[("x-opencode-session", "my-own-id")],
        );
        let hs = m.effective_headers();
        assert_eq!(
            get(&hs, "x-opencode-session"),
            Some("my-own-id"),
            "用户配的值必须优先于自动生成"
        );
    }

    #[test]
    fn custom_headers_are_all_passed_through() {
        let m = mk(
            "https://example.com/v1",
            &[("X-Foo", "bar"), ("X-Baz", "qux")],
        );
        let hs = m.effective_headers();
        assert_eq!(get(&hs, "X-Foo"), Some("bar"));
        assert_eq!(get(&hs, "X-Baz"), Some("qux"));
    }

    #[test]
    fn session_id_is_stable_within_process() {
        // 同进程内多次取应完全一致（这是缓存亲和的前提）
        assert_eq!(session_id(), session_id());
        assert_eq!(session_id().len(), 32, "应为 32 位 hex");
        assert!(session_id().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn endpoint_joins_correctly() {
        let m = ModelConfig {
            id: "x".into(),
            vendor: "".into(),
            url: "https://token.sensenova.cn/v1/".into(),
            api_key: "k".into(),
            supports_tool_call: false,
            supports_images: false,
            ..Default::default()
        };
        assert_eq!(
            m.chat_endpoint(),
            "https://token.sensenova.cn/v1/chat/completions"
        );
    }

    #[test]
    fn model_view_has_no_key_field() {
        let m = ModelConfig {
            id: "sensenova-6.8-flash-lite".into(),
            vendor: "Custom".into(),
            url: "https://token.sensenova.cn/v1".into(),
            api_key: "SECRET".into(),
            supports_tool_call: true,
            supports_images: true,
            max_input_tokens: Some(262144),
            max_output_tokens: Some(65536),
            ..Default::default()
        };
        let v = ModelView::from(&m);
        let j = serde_json::to_string(&v).unwrap();
        assert!(!j.contains("SECRET"), "ModelView 绝不能带 apiKey: {j}");
    }

    #[test]
    fn edit_view_masks_key_and_formats_headers() {
        let m = ModelConfig {
            id: "m1".into(),
            vendor: "Custom".into(),
            url: "https://x/v1".into(),
            api_key: "sk-TESTKEY0001abcdef".into(),
            supports_tool_call: true,
            supports_images: false,
            headers: [("x-foo".into(), "bar".into())].into_iter().collect(),
            ..Default::default()
        };
        let v = ModelEditView::from(&m);
        let j = serde_json::to_string(&v).unwrap();
        assert!(!j.contains("sk-TESTKEY0001abcdef"), "编辑视图不能带完整 Key: {j}");
        assert!(v.has_key);
        assert!(v.key_preview.starts_with("sk-TE"));
        assert_eq!(v.headers_text, "x-foo: bar");
    }

    #[test]
    fn update_model_keeps_key_when_blank() {
        let dir = std::env::temp_dir().join(format!("fa-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let m = ModelConfig {
            id: "keep".into(),
            url: "https://a/v1".into(),
            api_key: "sk-ORIGINALKEY12345".into(),
            supports_tool_call: true,
            supports_images: false,
            ..Default::default()
        };
        add_model(&dir, m).unwrap();
        update_model(
            &dir,
            "https://a/v1",
            "keep",
            Some("https://b/v1/".into()),
            Some("   ".into()),
            false,
            Some(false),
            Some(true),
            None,
            None,
            None,
            Some("glm-5.3-flash".into()),
        )
        .unwrap();
        let got = find_model_by_url_id(&dir, "https://b/v1", "glm-5.3-flash").unwrap();
        assert_eq!(got.id, "glm-5.3-flash", "接口 id 应已更新");
        assert_eq!(got.url, "https://b/v1", "尾斜杠应被归一化");
        assert_eq!(got.api_key, "sk-ORIGINALKEY12345", "空 Key 必须保留原值");
        assert!(!got.supports_tool_call);
        assert!(got.supports_images);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_model_falls_back_to_group_api_key() {
        // 用户心智："一个提供商（同 Base URL 组）一个 Key"。
        // 组里新加的模型 Key 留空 → 请求时继承组 Key，而不是拿空 Key 吃 401。
        let dir = std::env::temp_dir().join(format!("fa-groupkey-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "deepseek-v4.1-flash".into(),
                url: "https://ark.cn-beijing.volces.com/api/plan/v3".into(),
                api_key: "sk-GROUPKEY".into(),
                ..Default::default()
            },
        )
        .unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "glm-5.3-flash".into(),
                url: "https://ark.cn-beijing.volces.com/api/plan/v3/".into(), // 尾斜杠差异也要算同组
                api_key: String::new(),
                ..Default::default()
            },
        )
        .unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "local".into(),
                url: "http://127.0.0.1:11434/v1".into(),
                api_key: String::new(),
                ..Default::default()
            },
        )
        .unwrap();

        let hit = find_model(&dir, "glm-5.3-flash").unwrap();
        assert_eq!(
            hit.api_key, "sk-GROUPKEY",
            "空 Key 模型应回落到同组 Key（URL 归一化匹配）"
        );
        // 自身有 Key 时不回落；组内全空时保持空（本地模型）
        let owner =
            find_model_by_url_id(&dir, "https://ark.cn-beijing.volces.com/api/plan/v3", "deepseek-v4.1-flash")
                .unwrap();
        assert_eq!(owner.api_key, "sk-GROUPKEY");
        let local = find_model_by_url_id(&dir, "http://127.0.0.1:11434/v1", "local").unwrap();
        assert_eq!(local.api_key, "", "组内全空必须保持空，不得跨组借 Key");
        // 回落只发生在内存：磁盘上的空 Key 不被改写
        let raw = load_models(&dir).unwrap();
        let stored = raw.iter().find(|m| m.id == "glm-5.3-flash").unwrap();
        assert_eq!(stored.api_key, "", "回落不得写回 models.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_api_id_from_different_groups_both_allowed() {
        let dir = std::env::temp_dir().join(format!("fa-dupid-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        let mk = |url: &str| ModelConfig {
            id: "deepseek-v4.1-flash".into(),
            url: url.into(),
            api_key: "sk-x1234567890".into(),
            ..Default::default()
        };
        add_model(&dir, mk("https://api.deepseek.com/v1")).unwrap();
        add_model(&dir, mk("https://ark.cn-beijing.volces.com/api/v3")).unwrap();
        assert_eq!(load_models(&dir).unwrap().len(), 2, "同接口 id 不同组应共存");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_model_renames_api_id_with_dup_check() {
        // 改接口 id：同组查重（被占用 → 拒绝）；跨组同名不受限
        let dir = std::env::temp_dir().join(format!("fa-rename-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        let mk = |id: &str, url: &str| ModelConfig {
            id: id.into(),
            url: url.into(),
            api_key: "k123456".into(),
            ..Default::default()
        };
        add_model(&dir, mk("deepseek-v4.1-flash", "https://ark.example.com/v3")).unwrap();
        add_model(&dir, mk("other", "https://ark.example.com/v3")).unwrap();
        add_model(&dir, mk("ccc", "http://127.0.0.1:8791/v1")).unwrap();

        // 同组内 id 唯一 → A 改成组里没有的 id：成功
        update_model(
            &dir,
            "https://ark.example.com/v3",
            "deepseek-v4.1-flash",
            None,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            Some("glm-5.3-flash".into()),
        )
        .unwrap();
        assert!(
            find_model_by_url_id(&dir, "https://ark.example.com/v3", "glm-5.3-flash").is_ok(),
            "接口 id 应已改"
        );

        // B 改成同组已被占用的 id → 拒绝
        assert!(
            update_model(
                &dir,
                "https://ark.example.com/v3",
                "other",
                None,
                None,
                false,
                None,
                None,
                None,
                None,
                None,
                Some("glm-5.3-flash".into()),
            )
            .is_err(),
            "同组 id 冲突必须报错"
        );

        // 跨组同名不受限：另一组的 ccc 改成同一个 id → 成功
        update_model(
            &dir,
            "http://127.0.0.1:8791/v1",
            "ccc",
            None,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            Some("glm-5.3-flash".into()),
        )
        .unwrap();
        assert!(find_model_by_url_id(&dir, "http://127.0.0.1:8791/v1", "glm-5.3-flash").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_skips_existing_and_reuses_source_creds() {
        let dir = std::env::temp_dir().join(format!("fa-import-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        let src = ModelConfig {
            id: "src-model".into(),
            vendor: "VendorA".into(),
            url: "https://api.vendor.com/v1".into(),
            api_key: "sk-SOURCEKEY".into(),
            supports_tool_call: false,
            supports_images: false,
            ..Default::default()
        };
        add_model(&dir, src.clone()).unwrap();
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("alpha".to_string(), 262_144u64);
        // `src-model` 与来源**同 URL 同 id** → 已配置，跳过
        let (added, skipped) = import_models_from_source(
            &dir,
            &src,
            &["alpha".into(), "src-model".into(), "beta".into()],
            true,
            true,
            &ctx,
        )
        .unwrap();
        assert_eq!(added, vec!["alpha", "beta"]);
        assert_eq!(skipped, vec!["src-model"], "同源同 id 不得重复入库");
        let a = find_model(&dir, "alpha").unwrap();
        assert_eq!(a.url, src.url);
        assert_eq!(a.api_key, src.api_key);
        assert_eq!(a.vendor, "VendorA");
        assert!(a.supports_tool_call);
        assert_eq!(
            a.max_input_tokens,
            Some(262_144),
            "拉取到的 context_length 应写入 maxInputTokens"
        );
        // 没上报窗口的模型 → 回退来源配置（此处为 None）
        let b = find_model(&dir, "beta").unwrap();
        assert_eq!(b.max_input_tokens, None, "无 context_length 时保持来源值");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_api_id_from_different_urls_coexist() {
        // 身份 = (Base URL, 接口 id)：官方 / 火山 / 本地反代可同时存在同一个 model
        let dir = std::env::temp_dir().join(format!("fa-crossurl-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        let ark = ModelConfig {
            id: "glm-5.3-flash".into(),
            url: "https://ark.cn-beijing.volces.com/api/coding/v3".into(),
            api_key: "ark-x".into(),
            ..Default::default()
        };
        add_model(&dir, ark.clone()).unwrap();

        // 同源同 id → 跳过
        let (added, skipped) = import_models_from_source(
            &dir,
            &ark,
            &["glm-5.3-flash".into()],
            true,
            true,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(added.is_empty());
        assert_eq!(skipped, vec!["glm-5.3-flash"]);

        // 换来源（本地 trae 反代）同 id → **允许入库**
        let trae = ModelConfig {
            id: "__probe__".into(),
            url: "http://127.0.0.1:8790/v1".into(),
            ..Default::default()
        };
        let (added, skipped) = import_models_from_source(
            &dir,
            &trae,
            &["glm-5.3-flash".into()],
            true,
            true,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(skipped.is_empty(), "不同来源不算已配置：{skipped:?}");
        assert_eq!(added, vec!["glm-5.3-flash"]);
        assert_eq!(load_models(&dir).unwrap().len(), 2, "同 model 不同 URL 应共存");
        let t = find_model_by_url_id(&dir, "http://127.0.0.1:8790/v1", "glm-5.3-flash").unwrap();
        assert_eq!(t.url, "http://127.0.0.1:8790/v1");

        // 第三个来源同 id → 照样独立入库（跨组同名合法，不再有显示名后缀那套）
        let cline = ModelConfig {
            id: "__probe__".into(),
            url: "http://127.0.0.1:8789/v1".into(),
            ..Default::default()
        };
        let (added, skipped) = import_models_from_source(
            &dir,
            &cline,
            &["glm-5.3-flash".into()],
            true,
            true,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert!(skipped.is_empty());
        assert_eq!(added, vec!["glm-5.3-flash"]);
        assert_eq!(load_models(&dir).unwrap().len(), 3);
        // 三个组各有一条同 id，互不覆盖
        for u in [
            "https://ark.cn-beijing.volces.com/api/coding/v3",
            "http://127.0.0.1:8790/v1",
            "http://127.0.0.1:8789/v1",
        ] {
            assert!(find_model_by_url_id(&dir, u, "glm-5.3-flash").is_ok(), "{u}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_model_bare_id_ambiguous_takes_first() {
        // 裸 id 兜底查找：同 id 跨组多条 → 取第一条不报错；精确请走 find_model_by_url_id
        let dir = std::env::temp_dir().join(format!("fa-findname-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "glm-5.3-flash".into(),
                url: "https://ark.cn-beijing.volces.com/api/coding/v3".into(),
                api_key: "ark-k".into(),
                ..Default::default()
            },
        )
        .unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "glm-5.3-flash".into(),
                url: "http://127.0.0.1:8788/v1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let got = find_model(&dir, "glm-5.3-flash").unwrap();
        assert_eq!(
            got.url, "https://ark.cn-beijing.volces.com/api/coding/v3",
            "多条同 id 取第一条（文件顺序）"
        );
        assert_eq!(got.api_key, "ark-k", "组 Key 回落仍生效");
        let exact = find_model_by_url_id(&dir, "http://127.0.0.1:8788/v1", "glm-5.3-flash").unwrap();
        assert_eq!(exact.url, "http://127.0.0.1:8788/v1");
        assert!(find_model(&dir, "不存在").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_json_with_name_key_still_parses() {
        // 2026-10-02 废弃显示名：旧 models.json 里的 "name" 键必须被静默忽略，
        // 旧 settings.json 没有 selectedModelUrl 也必须能读。
        let dir = std::env::temp_dir().join(format!("fa-oldjson-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("models.json"),
            r#"[
            {
                "id": "glm-5.3-flash",
                "name": "火山coding",
                "vendor": "Custom",
                "url": "https://ark.cn-beijing.volces.com/api/coding/v3",
                "apiKey": "ark-x",
                "supportsToolCall": true,
                "supportsImages": false,
                "maxInputTokens": 1000000,
                "maxOutputTokens": null,
                "headers": {}
            }
        ]"#,
        )
        .unwrap();
        let all = load_models(&dir).unwrap();
        assert_eq!(all.len(), 1, "含 name 键的旧 JSON 必须能读");
        assert_eq!(all[0].id, "glm-5.3-flash");
        assert_eq!(all[0].api_key, "ark-x");

        std::fs::write(
            dir.join("settings.json"),
            r#"{"selectedModel":"glm-5.3-flash","execTrust":"full"}"#,
        )
        .unwrap();
        let s = load_settings(&dir);
        assert_eq!(s.selected_model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(s.selected_model_url, None, "缺 selectedModelUrl 应为 None");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
