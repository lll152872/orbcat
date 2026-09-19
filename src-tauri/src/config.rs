//! 模型配置加载
//!
//! ## 数据源优先级
//!   1. 环境变量 `FLOAT_AGENT_MODELS_JSON` 指向的文件
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
        let k = &self.api_key;
        if k.len() <= 10 {
            return "***".into();
        }
        format!("{}...{}", &k[..6], &k[k.len() - 4..])
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
                out.push(("x-opencode-client".into(), "float-agent".into()));
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
    if let Ok(p) = std::env::var("FLOAT_AGENT_MODELS_JSON") {
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

/// 新增（或按 id 覆盖）一个模型
pub fn add_model(data_dir: &Path, m: ModelConfig) -> Result<(), String> {
    if m.id.trim().is_empty() || m.url.trim().is_empty() {
        return Err("模型 id 和 url 不能为空".into());
    }
    let mut all = load_models(data_dir)?;
    match all.iter_mut().find(|x| x.id == m.id) {
        Some(existing) => *existing = m, // 同 id 覆盖
        None => all.push(m),
    }
    save_models(data_dir, &all)?;
    Ok(())
}

/// 删除一个模型；返回被删的 id（供调用方检查是否是当前选中项）
pub fn remove_model(data_dir: &Path, id: &str) -> Result<(), String> {
    let mut all = load_models(data_dir)?;
    let before = all.len();
    all.retain(|x| x.id != id);
    if all.len() == before {
        return Err(format!("找不到模型「{id}」"));
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

/// 按 id 找一个模型
pub fn find_model(data_dir: &Path, id: &str) -> Result<ModelConfig, String> {
    let models = load_models(data_dir)?;
    models
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("找不到模型「{id}」"))
}

// ---------------------------------------------------------------------------
// 运行时设置（选中的模型等）
// ---------------------------------------------------------------------------

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
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            selected_model: None,
            last_session: None,
            blur_collapse: true,
        }
    }
}

fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

// ---------------------------------------------------------------------------
// MCP 网关地址
// ---------------------------------------------------------------------------

/// MCP 网关的默认地址（用户的 1MCP 网关）
pub const DEFAULT_MCP_URL: &str = "http://127.0.0.1:3050/mcp";

/// 解析 MCP 网关地址。
///
/// 优先级：
///   1. 环境变量 `FLOAT_AGENT_MCP_URL`
///   2. `<data_dir>/mcp.json` 的 `{"url": "...", "enabled": true}`
///   3. [`DEFAULT_MCP_URL`]
///
/// 返回 `None` 表示显式禁用了 MCP（`enabled: false`）。
pub fn resolve_mcp_url(data_dir: &Path) -> Option<String> {
    if let Ok(u) = std::env::var("FLOAT_AGENT_MCP_URL") {
        if !u.trim().is_empty() {
            return Some(u);
        }
    }

    let p = data_dir.join("mcp.json");
    if p.exists() {
        if let Ok(txt) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                if v.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
                    return None;
                }
                if let Some(u) = v.get("url").and_then(serde_json::Value::as_str) {
                    if !u.trim().is_empty() {
                        return Some(u.to_string());
                    }
                }
            }
        }
    }

    Some(DEFAULT_MCP_URL.to_string())
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

    #[test]
    fn redact_hides_middle() {
        let m = ModelConfig {
            id: "x".into(),
            name: "x".into(),
            vendor: "".into(),
            url: "https://example.com/v1".into(),
            api_key: "sk-1234567890abcdefghij".into(),
            supports_tool_call: true,
            supports_images: false,
            max_input_tokens: None,
            max_output_tokens: None,
            headers: Default::default(),
        };
        let r = m.redacted_key();
        assert!(r.starts_with("sk-123"));
        assert!(r.ends_with("ghij"));
        assert!(!r.contains("4567890abcdef"));
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
            max_input_tokens: None,
            max_output_tokens: None,
            headers: Default::default(),
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
            max_input_tokens: None,
            max_output_tokens: None,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
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
        assert_eq!(get(&hs, "x-opencode-client"), Some("float-agent"));
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
            max_input_tokens: None,
            max_output_tokens: None,
            headers: Default::default(),
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
            headers: Default::default(),
        };
        let v = ModelView::from(&m);
        let j = serde_json::to_string(&v).unwrap();
        assert!(!j.contains("SECRET"), "ModelView 绝不能带 apiKey: {j}");
    }
}
