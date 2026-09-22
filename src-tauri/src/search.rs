//! 内置网页搜索 —— 默认 Tavily，可选 Exa / Brave
//!
//! ## 设计
//! - 配置落在 `agent-data/search.json`（**含 key，勿进仓库**；agent-data 已 gitignore）
//! - **未配置 key → `is_ready()==false`**，`tool_specs` 不暴露 `web_search`
//! - 结果统一压成短列表再喂给模型，避免灌爆上下文
//! - 查询词会发送到所选服务商（隐私：设置页与 RULES 各有一句）
//!
//! ## Provider
//! | id | 说明 |
//! |----|------|
//! | `tavily` | **默认**，agent 友好；有免费额度（以控制台为准） |
//! | `exa` | 语义搜索（用户说的 "seaxy"） |
//! | `brave` | Brave Search API |
//! | `""` / 其它 | 视为未配置 |

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::redact_key_str;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchConfig {
    /// `""` = 关闭；`tavily` | `exa` | `brave`
    pub provider: String,
    pub api_key: String,
}

impl SearchConfig {
    pub fn provider_norm(&self) -> String {
        self.provider.trim().to_ascii_lowercase()
    }

    pub fn is_ready(&self) -> bool {
        let p = self.provider_norm();
        !p.is_empty() && p != "off" && !self.api_key.trim().is_empty()
    }

    pub fn provider_label(&self) -> &'static str {
        match self.provider_norm().as_str() {
            "tavily" => "Tavily",
            "exa" => "Exa",
            "brave" => "Brave",
            _ => "未配置",
        }
    }
}

pub fn search_json_path(data_dir: &Path) -> PathBuf {
    data_dir.join("search.json")
}

pub fn load_config(data_dir: &Path) -> Result<SearchConfig, String> {
    let p = search_json_path(data_dir);
    if !p.exists() {
        return Ok(SearchConfig::default());
    }
    let txt = std::fs::read_to_string(&p)
        .map_err(|e| format!("读取 {} 失败: {e}", p.display()))?;
    serde_json::from_str(&txt).map_err(|e| format!("解析 {} 失败: {e}", p.display()))
}

pub fn save_config(data_dir: &Path, cfg: &SearchConfig) -> Result<(), String> {
    let p = search_json_path(data_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let txt = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&p, txt).map_err(|e| format!("写入 {} 失败: {e}", p.display()))
}

pub fn is_ready(data_dir: &Path) -> bool {
    load_config(data_dir).map(|c| c.is_ready()).unwrap_or(false)
}

/// 给前端的状态（**不含明文 key**）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchStatus {
    pub provider: String,
    pub provider_label: String,
    pub has_key: bool,
    pub key_preview: String,
    pub ready: bool,
    pub path: String,
}

pub fn status(data_dir: &Path) -> SearchStatus {
    let cfg = load_config(data_dir).unwrap_or_default();
    let has_key = !cfg.api_key.trim().is_empty();
    SearchStatus {
        provider: cfg.provider.trim().to_ascii_lowercase(),
        provider_label: cfg.provider_label().to_string(),
        has_key,
        key_preview: if has_key {
            redact_key_str(cfg.api_key.trim())
        } else {
            String::new()
        },
        ready: cfg.is_ready(),
        path: search_json_path(data_dir).display().to_string(),
    }
}

fn normalize_provider(raw: &str) -> Result<String, String> {
    let p = raw.trim().to_ascii_lowercase();
    match p.as_str() {
        "" | "off" => Ok(String::new()),
        "tavily" | "exa" | "brave" => Ok(p),
        other => Err(format!(
            "不支持的搜索后端「{other}」，可选：tavily（默认）/ exa / brave / 关闭"
        )),
    }
}

pub fn set_config(data_dir: &Path, provider: &str, api_key: Option<&str>) -> Result<SearchStatus, String> {
    let provider = normalize_provider(provider)?;
    let mut cfg = load_config(data_dir)?;
    cfg.provider = provider;
    // key：Some 才覆盖（空 Some = 清空 key）；None = 保留原 key
    if let Some(k) = api_key {
        cfg.api_key = k.trim().to_string();
    }
    if !cfg.is_ready() && !cfg.provider.is_empty() && cfg.api_key.trim().is_empty() {
        // 允许只切 provider、key 稍后填 —— ready=false，工具仍隐藏
    }
    save_config(data_dir, &cfg)?;
    Ok(status(data_dir))
}

/// 单条搜索命中（解析层）
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn clip(s: &str, n: usize) -> String {
    let t = s.trim().replace('\n', " ");
    if t.chars().count() <= n {
        t
    } else {
        let cut: String = t.chars().take(n).collect();
        format!("{cut}…")
    }
}

fn format_hits(query: &str, provider: &str, answer: Option<&str>, hits: &[SearchHit]) -> String {
    let mut out = format!("【web_search · {provider}】{query}\n");
    if let Some(a) = answer.filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("摘要：{}\n", clip(a, 240)));
    }
    if hits.is_empty() {
        out.push_str("（无结果）\n");
        return out;
    }
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}\n",
            i + 1,
            clip(&h.title, 80),
            h.url,
            clip(&h.snippet, 160)
        ));
    }
    out
}

async fn http_json(
    method: reqwest::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Result<(u16, Value), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
    let mut req = client.request(method, url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|e| format!("请求 {url} 失败: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let brief: String = text.chars().take(300).collect();
        return Err(format!("HTTP {status}（{url}）：{brief}"));
    }
    Ok((status, v))
}

fn hits_from_tavily(v: &Value) -> (Option<String>, Vec<SearchHit>) {
    let answer = v.get("answer").and_then(|x| x.as_str()).map(str::to_string);
    let mut hits = Vec::new();
    if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
        for r in arr {
            hits.push(SearchHit {
                title: r.get("title").and_then(|x| x.as_str()).unwrap_or("").into(),
                url: r.get("url").and_then(|x| x.as_str()).unwrap_or("").into(),
                snippet: r
                    .get("content")
                    .or_else(|| r.get("snippet"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
            });
        }
    }
    (answer, hits)
}

fn hits_from_exa(v: &Value) -> (Option<String>, Vec<SearchHit>) {
    let mut hits = Vec::new();
    if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
        for r in arr {
            hits.push(SearchHit {
                title: r.get("title").and_then(|x| x.as_str()).unwrap_or("").into(),
                url: r.get("url").and_then(|x| x.as_str()).unwrap_or("").into(),
                snippet: r
                    .get("text")
                    .or_else(|| r.get("snippet"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
            });
        }
    }
    (None, hits)
}

fn hits_from_brave(v: &Value) -> (Option<String>, Vec<SearchHit>) {
    let mut hits = Vec::new();
    if let Some(arr) = v
        .pointer("/web/results")
        .and_then(|x| x.as_array())
    {
        for r in arr {
            hits.push(SearchHit {
                title: r.get("title").and_then(|x| x.as_str()).unwrap_or("").into(),
                url: r.get("url").and_then(|x| x.as_str()).unwrap_or("").into(),
                snippet: r
                    .get("description")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .into(),
            });
        }
    }
    (None, hits)
}

/// 执行一次搜索，返回给模型的短文本
pub async fn run(cfg: &SearchConfig, query: &str, max_results: usize) -> Result<String, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("搜索关键词不能为空".into());
    }
    if !cfg.is_ready() {
        return Err("尚未配置网页搜索（设置 › 搜索：选 Tavily/Exa/Brave 并填入 API key）".into());
    }
    let max = max_results.clamp(1, 8);
    let provider = cfg.provider.trim().to_ascii_lowercase();
    let key = cfg.api_key.trim();

    let (answer, hits) = match provider.as_str() {
        "tavily" => {
            let body = json!({
                "api_key": key,
                "query": q,
                "max_results": max,
                "include_answer": true,
                "search_depth": "basic",
            });
            let (_s, v) = http_json(
                reqwest::Method::POST,
                "https://api.tavily.com/search",
                &[("Content-Type", "application/json")],
                Some(body),
            )
            .await?;
            hits_from_tavily(&v)
        }
        "exa" => {
            let body = json!({
                "query": q,
                "numResults": max,
                "contents": { "text": { "maxCharacters": 400 } },
            });
            let (_s, v) = http_json(
                reqwest::Method::POST,
                "https://api.exa.ai/search",
                &[("x-api-key", key), ("Content-Type", "application/json")],
                Some(body),
            )
            .await?;
            hits_from_exa(&v)
        }
        "brave" => {
            let url = format!(
                "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
                urlencoding_min(q),
                max
            );
            let (_s, v) = http_json(
                reqwest::Method::GET,
                &url,
                &[
                    ("Accept", "application/json"),
                    ("X-Subscription-Token", key),
                ],
                None,
            )
            .await?;
            hits_from_brave(&v)
        }
        other => return Err(format!("未知搜索后端「{other}」")),
    };

    Ok(format_hits(q, cfg.provider_label(), answer.as_deref(), &hits))
}

/// 极简 query 转义（Brave GET 用）
fn urlencoding_min(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 连通性自检（发一个极短 query）
pub async fn ping(cfg: &SearchConfig) -> Result<String, String> {
    if !cfg.is_ready() {
        return Err("尚未配置搜索后端与 key".into());
    }
    let text = run(cfg, "float-agent ping", 2).await?;
    let n = text.lines().filter(|l| l.starts_with(char::is_numeric)).count();
    Ok(format!(
        "搜索连通正常 · {} · 约 {n} 条结果行",
        cfg.provider_label()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits() -> Vec<SearchHit> {
        vec![
            SearchHit {
                title: "Tavily".into(),
                url: "https://tavily.com".into(),
                snippet: "agent search".into(),
            },
            SearchHit {
                title: "Exa".into(),
                url: "https://exa.ai".into(),
                snippet: "semantic".into(),
            },
        ]
    }

    #[test]
    fn provider_normalize_and_ready() {
        assert!(normalize_provider("Tavily").unwrap() == "tavily");
        assert!(normalize_provider("exa").unwrap() == "exa");
        assert!(normalize_provider("").unwrap().is_empty());
        assert!(normalize_provider("seaxy").is_err());

        let mut c = SearchConfig {
            provider: "tavily".into(),
            api_key: "tvly-abc".into(),
        };
        assert!(c.is_ready() && c.provider_label() == "Tavily");
        c.api_key.clear();
        assert!(!c.is_ready());
        c.provider = "exa".into();
        c.api_key = "x".into();
        assert_eq!(c.provider_label(), "Exa");
    }

    #[test]
    fn format_hits_shape() {
        let s = format_hits("hello", "Tavily", Some("ans"), &hits());
        assert!(s.contains("web_search · Tavily"));
        assert!(s.contains("hello"));
        assert!(s.contains("https://tavily.com"));
        assert!(s.contains("1. Tavily"));
    }

    #[test]
    fn parse_tavily_and_brave() {
        let tv = serde_json::json!({
            "answer": "A",
            "results": [{"title":"T","url":"U","content":"C"}]
        });
        let (a, h) = hits_from_tavily(&tv);
        assert_eq!(a.as_deref(), Some("A"));
        assert_eq!(h.len(), 1);

        let br = serde_json::json!({
            "web": {"results": [{"title":"B","url":"https://b","description":"D"}]}
        });
        let (_, h) = hits_from_brave(&br);
        assert_eq!(h[0].title, "B");
    }

    #[test]
    fn status_never_leaks_full_key() {
        let dir = std::env::temp_dir().join("fa_search_status_test");
        let _ = std::fs::create_dir_all(&dir);
        let cfg = SearchConfig {
            provider: "tavily".into(),
            api_key: "tvly-1234567890abcdefgh".into(),
        };
        save_config(&dir, &cfg).unwrap();
        let st = status(&dir);
        assert!(st.ready && st.has_key);
        assert!(!st.key_preview.contains("1234567890abcdefgh"));
        assert!(st.key_preview.starts_with("tvly-1") || st.key_preview.contains("…") || st.key_preview.contains("..."));
    }

    #[tokio::test]
    async fn run_rejects_when_not_ready() {
        let c = SearchConfig::default();
        let e = run(&c, "q", 3).await.unwrap_err();
        assert!(e.contains("尚未配置") || e.contains("搜索"));
    }
}
