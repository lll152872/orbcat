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
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
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
    pub name: String,
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
            name: if m.name.is_empty() {
                m.id.clone()
            } else {
                m.name.clone()
            },
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

/// 新增或按 **显示名** 覆盖一个模型。
///
/// - **显示名 `name` 是唯一键**（官方 DeepSeek 与火山可以同时存在，都调 `deepseek-v4.1-flash`）
/// - **接口 `id` 允许重复**，只是 request 的 `model` 字段
/// - 同名 = 更新这一条；异名 = 新增一条（哪怕接口 id 相同）
/// - API Key 允许为空（Ollama / 本地网关）
pub fn add_model(data_dir: &Path, m: ModelConfig) -> Result<(), String> {
    let mut m = m;
    let key = if m.name.trim().is_empty() {
        m.id.trim().to_string()
    } else {
        m.name.trim().to_string()
    };
    if key.is_empty() {
        return Err("显示名不能为空".into());
    }
    if m.id.trim().is_empty() {
        m.id = key.clone();
    }
    if m.url.trim().is_empty() {
        return Err("URL 不能为空".into());
    }
    m.name = key;
    let mut all = load_models(data_dir)?;
    match all.iter_mut().find(|x| x.name == m.name) {
        Some(existing) => *existing = m,
        None => all.push(m),
    }
    save_models(data_dir, &all)?;
    Ok(())
}

/// 删除一个模型（按显示名）
pub fn remove_model(data_dir: &Path, name: &str) -> Result<(), String> {
    let mut all = load_models(data_dir)?;
    let before = all.len();
    all.retain(|x| x.name != name);
    if all.len() == before {
        return Err(format!("找不到模型「{name}」"));
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
/// 显示名（唯一键）被别的来源占用时自动加主机后缀，避免覆盖。
/// 返回 `(added, skipped)` —— 均为最终**显示名**。
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
    let src_host = short_host(&source.url);
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
        // 显示名默认 = 接口 id；被**其它来源**占用则加主机后缀（同名不同源 = 两条）
        let mut display = id.to_string();
        if all.iter().any(|m| m.name == display) {
            display = format!("{id} · {src_host}");
            let mut n = 2;
            while all.iter().any(|m| m.name == display) {
                display = format!("{id} · {src_host} #{n}");
                n += 1;
            }
        }
        let vendor = if source.vendor.trim().is_empty() {
            "Custom".to_string()
        } else {
            source.vendor.trim().to_string()
        };
        let max_input = context_lengths.get(id).copied().or(source.max_input_tokens);
        all.push(ModelConfig {
            id: id.to_string(),
            name: display.clone(),
            vendor,
            url: source.url.clone(),
            api_key: source.api_key.clone(),
            supports_tool_call,
            supports_images,
            max_input_tokens: max_input,
            max_output_tokens: source.max_output_tokens,
            headers: source.headers.clone(),
        });
        added.push(display);
    }

    if !added.is_empty() {
        save_models(data_dir, &all)?;
    }
    Ok((added, skipped))
}

/// 编辑已有模型（按 **显示名** 定位）。
/// - `api_key` 为 None/空串时**保持原 Key**；`clear_key: true` 清空
/// - `new_id`：只改发给提供商的 `model` 字段（**允许与别的条目相同**）
/// - `name`：改显示名（唯一）
/// - `max_*` 为 `Some(None)` 清空；`None` 不动
pub fn update_model(
    data_dir: &Path,
    name_key: &str,
    name: Option<String>,
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
    let Some(idx) = all.iter().position(|x| x.name == name_key) else {
        return Err(format!("找不到模型「{name_key}」"));
    };
    if let Some(n) = name {
        let n = n.trim();
        let n = if n.is_empty() {
            name_key.to_string()
        } else {
            n.to_string()
        };
        if n != all[idx].name && all.iter().any(|x| x.name == n) {
            return Err(format!("显示名「{n}」已存在，请换一个"));
        }
        all[idx].name = n;
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
    if let Some(mid) = api_model_id {
        let mid = mid.trim();
        if !mid.is_empty() {
            m.id = mid.to_string();
        }
    }
    save_models(data_dir, &all)?;
    Ok(())
}

/// 重命名**显示名**（唯一键）。接口 `id` 不动。
pub fn rename_model(data_dir: &Path, old_name: &str, new_name: &str) -> Result<(), String> {
    let old_name = old_name.trim();
    let new_name = new_name.trim();
    if old_name.is_empty() || new_name.is_empty() {
        return Err("显示名不能为空".into());
    }
    if old_name == new_name {
        return Ok(());
    }
    let mut all = load_models(data_dir)?;
    if all.iter().any(|m| m.name == new_name) {
        return Err(format!("显示名「{new_name}」已存在，请换一个"));
    }
    let Some(m) = all.iter_mut().find(|x| x.name == old_name) else {
        return Err(format!("找不到模型「{old_name}」"));
    };
    m.name = new_name.to_string();
    save_models(data_dir, &all)?;
    Ok(())
}

/// 给编辑表单用的视图：**不回传完整 apiKey**，只给「有没有 Key + 掩码」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEditView {
    pub id: String,
    pub name: String,
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
            name: m.name.clone(),
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
pub fn find_model(data_dir: &Path, key: &str) -> Result<ModelConfig, String> {
    let models = load_models(data_dir)?;
    if let Some(m) = models.iter().find(|m| m.name == key) {
        return Ok(m.clone());
    }
    models
        .into_iter()
        .find(|m| m.id == key)
        .ok_or_else(|| format!("找不到模型「{key}」"))
}

/// URL 归一化比较键：去空白、去尾斜杠、转小写。
/// **只用于比较**，不写回配置（原值保留用户的写法）。
pub fn url_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

/// 只取主机名（分组展示 / 显示名去重用）。
pub fn short_host(url: &str) -> String {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    let host = rest.split('/').next().unwrap_or(rest);
    host.rsplit('@').next().unwrap_or(host).to_string()
}

/// 按 **Base URL + 接口 id** 找模型 —— 这才是「已配置」的真正身份。
///
/// 显示名只是展示用；同一个 `model` 从不同 Base URL 拉进来应当**共存**。
///
/// ⚠️ **仅供测试**（2026-09-30 标注）：目前 UI 的"当前模型"身份键是
/// **显示名**（`settings.selectedModel` 存的是 name，见 `find_model`），
/// 所以生产路径不按 url+id 找。这个函数留在这是为了给"同 id 不同 url 应当共存"
/// 这条不变量留一个可断言的入口 —— 它是设计意图的记录，不是遗漏的接线。
#[cfg(test)]
pub fn find_model_by_url_id(data_dir: &Path, url: &str, id: &str) -> Option<ModelConfig> {
    let want_url = url_key(url);
    let want_id = id.trim();
    load_models(data_dir)
        .ok()?
        .into_iter()
        .find(|m| m.id == want_id && url_key(&m.url) == want_url)
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
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
    /// 当前选中的模型 id
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
    /// 当前 agent 模式 id（见 `modes.rs`）。**只决定哪些 MCP 组对模型可见**，
    /// 不管内置工具集、也不管执行权限档位。
    ///
    /// 存 id 而不是显示名：`modes.json` 的名字是给用户看的、随时能改，
    /// 存名字的话用户改个名就把当前模式改没了（`find_model` 存显示名那套
    /// 是历史包袱，别照抄到这里）。
    #[serde(default = "default_mode_id")]
    pub active_mode: String,
}

/// 旧 `settings.json` 里没有 `activeMode` 时的默认值。
/// 用函数而不是 `String::new()`：空串要靠 `modes::resolve` 兜底，
/// 而"存的就是空"在 UI 上会显示成"没选模式"，不如直接写标准。
fn default_mode_id() -> String {
    crate::modes::DEFAULT_MODE_ID.to_string()
}

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
            active_mode: default_mode_id(),
        }
    }
}

fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

// ---------------------------------------------------------------------------
// MCP 服务器列表（可单独添加多个）
// ---------------------------------------------------------------------------

/// 一条 MCP 服务器配置（Streamable HTTP）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerCfg {
    /// 稳定 id（展示/路由用），用户可改
    pub id: String,
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 展示名（可空，空则用 id）
    #[serde(default)]
    pub label: String,
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

/// 读 MCP 服务器列表（只返回 enabled 的，供连接用；管理界面用 [`load_mcp_servers`]）。
pub fn resolve_mcp_servers(data_dir: &Path) -> Vec<McpServerCfg> {
    load_mcp_servers(data_dir)
        .into_iter()
        .filter(|s| s.enabled && !s.url.trim().is_empty())
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
            list.push(McpServerCfg {
                id: "env".into(),
                url: u,
                enabled: true,
                label: "环境变量 ORBCAT_MCP_URL".into(),
            });
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
            list.push(McpServerCfg {
                id: "default".into(),
                url: u.trim().to_string(),
                enabled,
                label: "默认网关".into(),
            });
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
        Some(u) if !u.trim().is_empty() => vec![McpServerCfg {
            id: "default".into(),
            url: u.trim().to_string(),
            enabled: true,
            label: "默认网关".into(),
        }],
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

    /// 老的 `settings.json`（没有 `activeMode`）必须能读，且默认落到标准模式。
    ///
    /// 为什么单独立一条：`agent-data/settings.json` 是用户手改过的文件，
    /// 加了新字段后**绝不能**因为反序列化失败而整体退回默认值 ——
    /// 那会把用户配好的模型/项目/权限全丢掉（`load_settings` 用的是
    /// `.unwrap_or_default()`，解析失败是**静默**的，最容易出事的写法）。
    #[test]
    fn legacy_settings_without_active_mode_defaults_to_standard() {
        let d = std::env::temp_dir().join(format!(
            "orbcat_cfg_mode_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        // 模拟"上一个版本写出来的 settings.json"：有 selectedModel，没有 activeMode
        std::fs::write(
            settings_path(&d),
            r#"{"selectedModel":"火山agent","execTrust":"full","mcpAllGroups":true}"#,
        )
        .unwrap();

        let s = load_settings(&d);
        assert_eq!(s.selected_model.as_deref(), Some("火山agent"), "老字段不能被丢掉");
        assert_eq!(s.exec_trust, ExecTrust::Full);
        assert_eq!(s.active_mode, "standard", "缺 activeMode 应默认标准模式");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn redact_hides_middle() {
        let m = ModelConfig {
            id: "x".into(),
            name: "x".into(),
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
            name: "x".into(),
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
            name: "t".into(),
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
            name: "x".into(),
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
            name: "sensenova-6.8-flash-lite".into(),
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
            name: "M1".into(),
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
            name: "Keep".into(),
            url: "https://a/v1".into(),
            api_key: "sk-ORIGINALKEY12345".into(),
            supports_tool_call: true,
            supports_images: false,
            ..Default::default()
        };
        add_model(&dir, m).unwrap();
        update_model(
            &dir,
            "Keep",
            Some("新名字".into()),
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
        let got = find_model(&dir, "新名字").unwrap();
        assert_eq!(got.name, "新名字");
        assert_eq!(got.id, "glm-5.3-flash");
        assert_eq!(got.url, "https://b/v1");
        assert_eq!(got.api_key, "sk-ORIGINALKEY12345", "空 Key 必须保留原值");
        assert!(!got.supports_tool_call);
        assert!(got.supports_images);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_api_id_different_display_name_both_allowed() {
        let dir = std::env::temp_dir().join(format!("fa-dupid-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        let mk = |name: &str, url: &str| ModelConfig {
            id: "deepseek-v4.1-flash".into(),
            name: name.into(),
            url: url.into(),
            api_key: "sk-x1234567890".into(),
            ..Default::default()
        };
        add_model(&dir, mk("dpsk官方", "https://api.deepseek.com/v1")).unwrap();
        add_model(&dir, mk("火山", "https://ark.cn-beijing.volces.com/api/v3")).unwrap();
        assert_eq!(load_models(&dir).unwrap().len(), 2, "同接口 id 不同显示名应共存");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_model_changes_display_name_only() {
        let dir = std::env::temp_dir().join(format!("fa-rename-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "deepseek-v4.1-flash".into(),
                name: "glm5.3-flash".into(),
                url: "https://ark.example.com/v3".into(),
                api_key: "k".into(),
                supports_tool_call: true,
                supports_images: true,
                ..Default::default()
            },
        )
        .unwrap();
        rename_model(&dir, "glm5.3-flash", "glm-5.3-flash").unwrap();
        let got = find_model(&dir, "glm-5.3-flash").unwrap();
        assert_eq!(got.name, "glm-5.3-flash");
        assert_eq!(
            got.id, "deepseek-v4.1-flash",
            "接口 model 不应被改显示名冲掉"
        );
        assert!(find_model(&dir, "glm5.3-flash").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_skips_existing_and_reuses_source_creds() {
        let dir = std::env::temp_dir().join(format!("fa-import-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        let src = ModelConfig {
            id: "src-model".into(),
            name: "Source".into(),
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
            name: "火山coding".into(),
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
            name: String::new(),
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

        // 第三个来源同 id 且显示名已被占用 → 自动加主机后缀，不覆盖
        let cline = ModelConfig {
            id: "__probe__".into(),
            name: String::new(),
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
        assert_eq!(added, vec!["glm-5.3-flash · 127.0.0.1:8789"]);
        assert_eq!(load_models(&dir).unwrap().len(), 3);
        // 旧条目显示名未被冲掉
        assert!(find_model(&dir, "火山coding").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_model_prefers_display_name_over_api_id() {
        // 显示名唯一 → 必须先精确匹配显示名，否则点列表会误开同 id 的另一条
        let dir = std::env::temp_dir().join(format!("fa-findname-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("models.json"), "[]").unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "glm-5.3-flash".into(),
                name: "火山coding".into(),
                url: "https://ark.cn-beijing.volces.com/api/coding/v3".into(),
                ..Default::default()
            },
        )
        .unwrap();
        add_model(
            &dir,
            ModelConfig {
                id: "glm-5.3-flash".into(),
                name: "glm-5.3-flash".into(),
                url: "http://127.0.0.1:8788/v1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let got = find_model(&dir, "glm-5.3-flash").unwrap();
        assert_eq!(
            got.url, "http://127.0.0.1:8788/v1",
            "按显示名查找不得落到同 id 的第一条"
        );
        let by_name = find_model(&dir, "火山coding").unwrap();
        assert_eq!(by_name.url, "https://ark.cn-beijing.volces.com/api/coding/v3");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
