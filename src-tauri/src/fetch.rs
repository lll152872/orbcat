//! fetch_url —— 拉网页并抽出可读正文（Markdown 子集）
//!
//! ## 为什么不是「原样把 HTML 丢给模型」
//! 真实页面动辄几十 KB，其中 `<style>` / `<script>` / 属性噪声占大头。
//! 按 token 计费时，原样 HTML 的信噪比极差，还会挤爆工具结果截断。
//!
//! ## 做法（零额外依赖，手写子集转换）
//! 1. 整页剥掉 `script/style/svg/noscript/template` 与注释
//! 2. 标签映射到 Markdown 子集：标题 / 段 / 链接 / 列表 / 代码 / 引用 / 粗斜
//! 3. 其余标签当块边界或丢弃；实体只解常用的几个
//! 4. 按字数截断（默认 10k 字）
//!
//! **不做**：JS 渲染、readability 启发式、PDF、多页爬取。
//! SPA 正文拿不到时请走 playwright MCP 组。

use std::cell::RefCell;
use std::time::Duration;

/// 拉取超时
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// 默认正文截断（字符）
pub const DEFAULT_MAX_CHARS: usize = 10_000;
/// 响应体硬顶（字节）—— 防止把超大 HTML 读进内存
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

thread_local! {
    static HREFS: RefCell<Vec<String>> = RefCell::new(Vec::new());
}

/// 抓 URL 并返回「标题 + Markdown 子集正文」。
pub async fn fetch_and_extract(url: &str, max_chars: usize) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url 不能为空".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("只支持 http/https URL".into());
    }

    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent("orbcat/0.1 (+local desktop assistant)")
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }

    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ok_ctype = ctype.is_empty()
        || ctype.contains("text/html")
        || ctype.contains("application/xhtml")
        || ctype.contains("text/plain")
        || ctype.contains("text/xml")
        || ctype.contains("application/xml");
    if !ok_ctype {
        return Err(format!(
            "暂不支持的内容类型（{ctype}）。PDF/二进制请改用其他工具或手动下载。"
        ));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取响应体失败: {e}"))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "页面过大（{} 字节 > 上限 {}），请改用搜索摘要或指定更小的页面",
            bytes.len(),
            MAX_BODY_BYTES
        ));
    }

    let html = String::from_utf8_lossy(&bytes).to_string();
    let (title, md) = html_to_md(&html);

    let max = max_chars.clamp(200, 50_000);
    let full_chars = md.chars().count();
    let body: String = md.chars().take(max).collect();
    let truncated = full_chars > max;

    let mut out = String::new();
    out.push_str("【fetch_url】");
    out.push_str(url);
    out.push('\n');
    if !title.is_empty() {
        out.push_str(&format!("标题：{title}\n"));
    }
    out.push_str(&format!(
        "（已抽取正文：原页 {} 字节 → markdown {} 字{}）\n\n",
        bytes.len(),
        full_chars,
        if truncated {
            format!("，已截断至 {max} 字")
        } else {
            String::new()
        }
    ));
    out.push_str(&body);
    if truncated {
        out.push_str("\n\n…[正文已截断，如需后半段请指出章节/关键词]");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// HTML → Markdown 子集
// ---------------------------------------------------------------------------

/// 返回 `(title, markdown)`。
pub fn html_to_md(html: &str) -> (String, String) {
    // 清掉线程本地栈，防上次残缺 HTML 污染
    HREFS.with(|h| h.borrow_mut().clear());
    let cleaned = strip_noise(html);
    let title = extract_title(&cleaned);
    let md = convert_subset(&cleaned);
    (title, normalize_ws(&md))
}

fn strip_noise(html: &str) -> String {
    let mut s = html.to_string();
    while let Some(a) = s.find("<!--") {
        let Some(b) = s[a..].find("-->") else {
            s.truncate(a);
            break;
        };
        s.replace_range(a..a + b + 3, "");
    }
    for tag in [
        "script",
        "style",
        "svg",
        "noscript",
        "template",
        "iframe",
        "object",
    ] {
        s = remove_tag_blocks(&s, tag);
    }
    s
}

fn remove_tag_blocks(s: &str, tag: &str) -> String {
    let lower_open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(a) = find_tag_ci(rest, &lower_open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..a]);
        let after = &rest[a + lower_open.len()..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let body = &after[gt + 1..];
        match find_tag_ci(body, &close) {
            Some(b) => {
                rest = &body[b + close.len()..];
            }
            None => break,
        }
    }
    out
}

fn find_tag_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

fn extract_title(html: &str) -> String {
    if let Some(a) = find_tag_ci(html, "<title") {
        let after = &html[a..];
        if let Some(gt) = after.find('>') {
            let body = &after[gt + 1..];
            if let Some(b) = find_tag_ci(body, "</title") {
                let t = decode_entities(&body[..b]).trim().to_string();
                if !t.is_empty() {
                    return t.chars().take(200).collect();
                }
            }
        }
    }
    if let Some(a) = find_tag_ci(html, "<h1") {
        let after = &html[a..];
        if let Some(gt) = after.find('>') {
            let body = &after[gt + 1..];
            if let Some(b) = find_tag_ci(body, "</h1") {
                let t = decode_entities(&strip_tags(&body[..b]))
                    .trim()
                    .to_string();
                if !t.is_empty() {
                    return t.chars().take(200).collect();
                }
            }
        }
    }
    String::new()
}

fn convert_subset(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    let mut list_stack: Vec<bool> = Vec::new();
    let mut ol_counter: Vec<u32> = Vec::new();

    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push_str(&decode_entities(rest));
            break;
        };
        out.push_str(&decode_entities(&rest[..lt]));
        let Some(gt) = rest[lt..].find('>') else {
            break;
        };
        let tag_raw = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];
        let tag = tag_raw.trim();
        if tag.is_empty() {
            continue;
        }
        let closing = tag.starts_with('/');
        let name_part = tag.trim_start_matches('/');
        let name_end = name_part
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(name_part.len());
        let name = name_part[..name_end].to_ascii_lowercase();
        let attrs = if closing { "" } else { name_part };

        match name.as_str() {
            "br" => out.push('\n'),
            "hr" => out.push_str("\n\n---\n\n"),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<usize>().unwrap_or(3).clamp(1, 6);
                if closing {
                    out.push_str("\n\n");
                } else {
                    out.push_str("\n\n");
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
            }
            "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "aside" => {
                out.push_str("\n\n");
            }
            "li" => {
                if closing {
                    out.push('\n');
                } else {
                    let depth = list_stack.len().saturating_sub(1);
                    out.push('\n');
                    out.push_str(&"  ".repeat(depth));
                    let is_ul = list_stack.last().copied().unwrap_or(true);
                    if is_ul {
                        out.push_str("- ");
                    } else {
                        let i = match ol_counter.last_mut() {
                            Some(c) => {
                                *c += 1;
                                *c
                            }
                            None => 1,
                        };
                        out.push_str(&format!("{i}. "));
                    }
                }
            }
            "ul" => {
                if closing {
                    list_stack.pop();
                    ol_counter.pop();
                    out.push('\n');
                } else {
                    list_stack.push(true);
                    ol_counter.push(0);
                }
            }
            "ol" => {
                if closing {
                    list_stack.pop();
                    ol_counter.pop();
                    out.push('\n');
                } else {
                    list_stack.push(false);
                    ol_counter.push(0);
                }
            }
            "pre" => out.push_str("\n```\n"),
            "code" | "tt" => out.push('`'),
            "strong" | "b" => out.push_str("**"),
            "em" | "i" => out.push('*'),
            "blockquote" => {
                if closing {
                    out.push('\n');
                } else {
                    out.push_str("\n\n> ");
                }
            }
            "a" => {
                if closing {
                    let href = HREFS.with(|h| h.borrow_mut().pop()).unwrap_or_default();
                    close_anchor(&mut out, &href);
                } else {
                    let href = extract_attr(attrs, "href").unwrap_or_default();
                    HREFS.with(|h| h.borrow_mut().push(href));
                    out.push('[');
                }
            }
            "title" | "head" => {}
            "html" | "body" | "table" | "tbody" | "thead" | "tr" | "td" | "th" | "span"
            | "nav" | "form" | "input" | "button" | "img" | "picture" | "source" | "meta"
            | "link" => {
                if matches!(name.as_str(), "td" | "th") && !closing {
                    out.push_str(" | ");
                }
                if name == "tr" && closing {
                    out.push('\n');
                }
                if matches!(name.as_str(), "table" | "nav" | "form" | "body" | "html") {
                    out.push_str("\n\n");
                }
            }
            _ => {}
        }
    }

    // 残缺 HTML：未闭合的 <a>
    while HREFS.with(|h| h.borrow_mut().pop()).is_some() {
        if out.ends_with('[') {
            out.pop();
        }
    }
    out
}

/// 把 `out` 里最近的未闭合 `[text` 收成 `[text](href)`。
fn close_anchor(out: &mut String, href: &str) {
    let bytes = out.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b']' => break,
            b'[' => {
                if href.is_empty() {
                    out.replace_range(i..i + 1, "");
                } else {
                    out.push_str(&format!("]({href})"));
                }
                return;
            }
            _ => {}
        }
    }
    if !href.is_empty() {
        out.push_str(&format!("]({href})"));
    }
}

fn extract_attr(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let key = format!("{name}=");
    let i = lower.find(&key)?;
    let rest = attrs[i + key.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)? + 1;
        Some(decode_entities(&rest[1..end]))
    } else {
        let end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(decode_entities(&rest[..end]))
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('<') {
        out.push_str(&rest[..a]);
        match rest[a..].find('>') {
            Some(b) => rest = &rest[a + b + 1..],
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            blank += 1;
            if blank <= 2 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(t);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_and_style() {
        let html = r#"<html><head><style>.a{color:red}</style><script>alert(1)</script>
            <title>页面标题</title></head><body><p>正文</p></body></html>"#;
        let (title, md) = html_to_md(html);
        assert_eq!(title, "页面标题");
        assert!(md.contains("正文"));
        assert!(!md.contains("color:red"));
        assert!(!md.contains("alert"));
    }

    #[test]
    fn converts_headings_links_lists() {
        let html = r#"<h1>标题一</h1><p>看<a href="https://x.dev/doc">文档</a>。</p>
            <ul><li>甲</li><li>乙</li></ul>"#;
        let (_, md) = html_to_md(html);
        assert!(md.contains("# 标题一"), "md={md}");
        assert!(md.contains("[文档](https://x.dev/doc)"), "md={md}");
        assert!(md.contains("- 甲"), "md={md}");
        assert!(md.contains("- 乙"), "md={md}");
    }

    #[test]
    fn code_and_emphasis() {
        let html =
            r#"<p>用<code>fetch_url</code>，这是<strong>重点</strong>。</p><pre>let x=1;</pre>"#;
        let (_, md) = html_to_md(html);
        assert!(md.contains("`fetch_url`"), "md={md}");
        assert!(md.contains("**重点**"), "md={md}");
        assert!(md.contains("```\nlet x=1;"), "md={md}");
    }

    #[test]
    fn entities_are_decoded() {
        let (_, md) = html_to_md("<p>A&amp;B&nbsp;C&lt;D&gt;</p>");
        assert!(md.contains("A&B C<D>"), "md={md}");
    }

    #[test]
    fn nested_list_and_ol() {
        let html =
            r#"<ol><li>一</li><li>二</li></ol><ul><li>外<ul><li>内</li></ul></li></ul>"#;
        let (_, md) = html_to_md(html);
        assert!(md.contains("1. 一"), "md={md}");
        assert!(md.contains("2. 二"), "md={md}");
        assert!(md.contains("- 外"), "md={md}");
        assert!(md.contains("- 内"), "md={md}");
    }

    #[test]
    fn title_falls_back_to_h1() {
        let (t, _) = html_to_md("<body><h1>回退标题</h1></body>");
        assert_eq!(t, "回退标题");
    }

    #[test]
    fn empty_href_drops_brackets() {
        let (_, md) = html_to_md(r#"<p><a>纯文字</a></p>"#);
        assert!(md.contains("纯文字"), "md={md}");
        assert!(!md.contains('['), "md={md}");
    }
}
