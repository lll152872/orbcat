//! 任务分发交接文件（首期：WorkBuddy，**第一轮人工投递**）。
//!
//! 分发语义（2026-09-21 用户拍板）：
//! - **不是**只抄一句用户输入，而是从当前会话**总结有效消息**（用户/助手正文，
//!   跳过空行与工具噪声），并**把会话里的图片复制**进交接目录。
//! - 交接材料写好后**打开该文件**，由用户第一轮手动交给 WorkBuddy。
//! - 第二轮自动化投递给 WB —— **明确不在本期**（无可靠 CLI/API，先人工）。
//!
//! 可选 settings `dispatch_wb_drop_dir`：权限网关放行后额外复制一份 md；
//! 失败不阻断主交接文件。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 首期唯一目标。后续加 CC/CX 时往这里加常量即可。
pub const SUPPORTED_TARGETS: &[&str] = &["workbuddy"];

/// 单条消息进入交接正文的最大字符数（防整页刷屏）
const MAX_MSG_CHARS: usize = 2000;
/// 有效消息最多收录条数（最近优先，向前截取）
const MAX_MSGS: usize = 40;
/// 单次交接最多复制的图片张数
const MAX_IMAGES: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffMeta {
    pub path: String,
    pub target: String,
    pub title: String,
    pub created_at: String,
    /// 已复制进交接目录的图片相对/绝对路径列表
    #[serde(default)]
    pub images: Vec<String>,
    /// 有效消息条数（用户+助手正文）
    pub message_count: usize,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_display() -> String {
    let secs = now_secs();
    let (h, mi, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("ts{secs}T{h:02}:{mi:02}:{s:02}Z")
}

fn slugify(title: &str) -> String {
    let mut out = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | ' ') {
            out.push('-');
        } else if (ch as u32) < 128 {
            // drop other ascii punct
        } else {
            out.push(ch);
        }
        if out.chars().count() >= 40 {
            break;
        }
    }
    let t = out.trim_matches('-').to_string();
    if t.is_empty() {
        "untitled".into()
    } else {
        t
    }
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…（已截断）")
    }
}

pub fn normalize_target(raw: &str) -> Result<String, String> {
    let t = raw.trim().to_ascii_lowercase();
    if SUPPORTED_TARGETS.contains(&t.as_str()) {
        Ok(t)
    } else {
        Err(format!(
            "不支持的分发目标「{raw}」，当前仅支持：{}",
            SUPPORTED_TARGETS.join(", ")
        ))
    }
}

pub fn dispatch_root(data_dir: &Path) -> PathBuf {
    data_dir.join("dispatch")
}

pub fn target_dir(data_dir: &Path, target: &str) -> PathBuf {
    dispatch_root(data_dir).join(target)
}

/// 展开 Windows `%VAR%` 环境变量（设置页占位符可能写 `%USERPROFILE%\\...`）。
/// 未知变量保持原样；非 Windows 直接返回。
pub fn expand_windows_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '%' {
            if let Some(end) = bytes[i + 1..].iter().position(|&c| c == '%') {
                let name: String = bytes[i + 1..i + 1 + end].iter().collect();
                if name.is_empty() {
                    out.push('%');
                    i += 1;
                    continue;
                }
                match std::env::var(&name) {
                    Ok(v) => {
                        out.push_str(&v);
                        i += end + 2;
                    }
                    Err(_) => {
                        out.push('%');
                        out.push_str(&name);
                        out.push('%');
                        i += end + 2;
                    }
                }
            } else {
                out.push('%');
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// 有效消息：仅 user/assistant 的非空正文（工具 steps / reasoning 不进交接）。
#[derive(Debug, Clone)]
pub struct EffectiveMsg {
    pub role: String,
    pub text: String,
    pub images: Vec<String>,
}

pub fn effective_messages(messages: &[crate::sessions::StoredMessage]) -> Vec<EffectiveMsg> {
    let mut all: Vec<EffectiveMsg> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| EffectiveMsg {
            role: m.role.clone(),
            text: m.text.trim().to_string(),
            images: m
                .images
                .iter()
                .filter(|p| !p.trim().is_empty() && !p.starts_with("data:"))
                .cloned()
                .collect(),
        })
        .filter(|m| !m.text.is_empty() || !m.images.is_empty())
        .collect();
    if all.len() > MAX_MSGS {
        let skip = all.len() - MAX_MSGS;
        all = all.split_off(skip);
    }
    for m in &mut all {
        m.text = cap_chars(&m.text, MAX_MSG_CHARS);
    }
    all
}

fn render_markdown(
    target: &str,
    title: &str,
    summary: &str,
    reason: &str,
    context: &str,
    source: &str,
    body_msgs: &str,
    image_refs: &[String],
) -> String {
    let display = match target {
        "workbuddy" => "WorkBuddy",
        other => other,
    };
    let images_block = if image_refs.is_empty() {
        "（本轮有效消息中没有可复制的图片）".to_string()
    } else {
        image_refs
            .iter()
            .map(|p| format!("- `{p}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "# 交接：{title}\n\n\
         - 目标：{display}\n\
         - 来源：float-agent（悬浮小agent）\n\
         - 来源标记：{source}\n\
         - 时间：{}\n\
         - 原因：{reason}\n\
         - 投递方式：**第一轮请用户手动**把本文件内容/图片交给 {display}；\
         自动化投递属第二轮，本期不做。\n\n\
         ## 任务摘要\n\n{summary}\n\n\
         ## 有效消息（已总结收录）\n\n{body_msgs}\n\n\
         ## 图片\n\n{images_block}\n\n\
         ## 补充上下文\n\n{context}\n\n\
         ## 怎么用\n\n\
         1. 打开本文件，复制「任务摘要 + 有效消息 + 图片」到 {display} 新会话。\n\
         2. 或在 {display} 中直接引用本路径/附件目录。\n\
         float-agent **不会**自动替 {display} 开会话。\n",
        now_display()
    )
}

fn format_effective_body(msgs: &[EffectiveMsg]) -> String {
    if msgs.is_empty() {
        return "（会话里暂无有效正文消息）".to_string();
    }
    msgs.iter()
        .enumerate()
        .map(|(i, m)| {
            let role = if m.role == "user" { "用户" } else { "助手" };
            let mut block = format!("### {}. {}\n\n{}\n", i + 1, role, m.text);
            if !m.images.is_empty() {
                block.push_str("\n图片：\n");
                for img in &m.images {
                    block.push_str(&format!("- `{img}`\n"));
                }
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把会话中的图片复制到 `dispatch/<target>/assets/`，返回复制后的路径。
fn copy_images(
    data_dir: &Path,
    target: &str,
    handoff_stem: &str,
    images: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    if images.is_empty() {
        return out;
    }
    let dest_dir = target_dir(data_dir, target).join(format!("{handoff_stem}__assets"));
    if std::fs::create_dir_all(&dest_dir).is_err() {
        return out;
    }
    for (i, src) in images.iter().take(MAX_IMAGES).enumerate() {
        let src_path = Path::new(src);
        if !src_path.is_file() {
            continue;
        }
        let ext = src_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("png");
        let dest = dest_dir.join(format!("{i:02}.{ext}"));
        if std::fs::copy(src_path, &dest).is_ok() {
            out.push(dest.to_string_lossy().to_string());
        }
    }
    out
}

/// 通用交接（摘要由调用方给出；图片路径可选，会复制）。
pub fn write_handoff(
    data_dir: &Path,
    target: &str,
    title: &str,
    summary: &str,
    reason: &str,
    context: &str,
    source: &str,
    extra_images: &[String],
    extra_msgs: &[EffectiveMsg],
) -> Result<HandoffMeta, String> {
    let target = normalize_target(target)?;
    let title = title.trim();
    let summary = summary.trim();
    let reason = reason.trim();
    if title.is_empty() {
        return Err("交接标题不能为空".into());
    }
    if summary.is_empty() {
        return Err("任务摘要不能为空".into());
    }
    if reason.is_empty() {
        return Err("请说明为何要分发（原因不能为空）".into());
    }

    // 先占位 stem：复制图片需要目录名，与 md 同步生成
    let dir = target_dir(data_dir, &target);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 dispatch 目录失败: {e}"))?;
    let stem = format!("{}__{}", now_secs(), slugify(title));

    let mut all_imgs: Vec<String> = extra_msgs.iter().flat_map(|m| m.images.clone()).collect();
    all_imgs.extend(extra_images.iter().cloned());
    all_imgs.dedup();
    let copied = copy_images(data_dir, &target, &stem, &all_imgs);

    let body_msgs = format_effective_body(extra_msgs);
    let body = render_markdown(
        &target,
        title,
        summary,
        reason,
        context.trim(),
        source.trim(),
        &body_msgs,
        &copied,
    );
    let path = dir.join(format!("{stem}.md"));
    std::fs::write(&path, body).map_err(|e| format!("写入交接文件失败: {e}"))?;

    Ok(HandoffMeta {
        path: path.to_string_lossy().to_string(),
        target,
        title: title.to_string(),
        created_at: now_display(),
        images: copied,
        message_count: extra_msgs.len(),
    })
}

/// 从当前会话总结有效消息 + 附图，写交接（分发主路径）。
pub fn write_handoff_from_session(
    data_dir: &Path,
    session_id: &str,
    target: &str,
    title: Option<&str>,
    summary: Option<&str>,
    reason: &str,
    context: &str,
    source: &str,
) -> Result<HandoffMeta, String> {
    let target = normalize_target(target)?;
    let sid = if session_id.trim().is_empty() {
        crate::sessions::current_id(data_dir)
            .ok_or_else(|| "没有当前会话可总结".to_string())?
    } else {
        session_id.trim().to_string()
    };
    let session = crate::sessions::load(data_dir, &sid)
        .ok_or_else(|| format!("读不到会话 {sid}"))?;
    let msgs = effective_messages(&session.messages);
    if msgs.is_empty() {
        return Err("当前会话没有有效消息，无法生成交接".into());
    }

    let title = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            session
                .title
                .chars()
                .take(40)
                .collect::<String>()
                .trim()
                .to_string()
        });
    let title = if title.is_empty() {
        "来自悬浮窗会话的交接".to_string()
    } else {
        title
    };

    // 摘要：模型未给时，用「有效消息」机械压缩（最后几条 + 条数/图数）
    let summary = summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let user_n = msgs.iter().filter(|m| m.role == "user").count();
            let asst_n = msgs.iter().filter(|m| m.role == "assistant").count();
            let img_n: usize = msgs.iter().map(|m| m.images.len()).sum();
            let last_user = msgs
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| cap_chars(&m.text, 200))
                .unwrap_or_else(|| "（无）".into());
            let last_asst = msgs
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .map(|m| cap_chars(&m.text, 300))
                .unwrap_or_else(|| "（无）".into());
            format!(
                "会话 `{sid}` 共 {user_n} 条用户 / {asst_n} 条助手有效消息，图片 {img_n} 张。\n\n\
                 **最近用户**：{last_user}\n\n\
                 **最近助手**：{last_asst}\n\n\
                 （完整有效消息见下文；请在 WorkBuddy 里继续处理该任务。）"
            )
        });

    write_handoff(
        data_dir,
        &target,
        &title,
        &summary,
        reason,
        context,
        source,
        &[],
        &msgs,
    )
}

/// 用系统默认程序打开交接文件（第一轮人工投递的入口）。
pub fn open_handoff_file(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(format!("交接文件不存在：{path}"));
    }
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(p)
            .spawn()
            .map_err(|e| format!("打开交接文件失败: {e}"))?;
    }
    #[cfg(not(windows))]
    {
        let _ = p;
    }
    Ok(())
}

pub fn reveal_target_dir(data_dir: &Path, target: &str) -> Result<String, String> {
    let t = normalize_target(target)?;
    let dir = target_dir(data_dir, &t);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let path = dir.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("打开资源管理器失败: {e}"))?;
    }
    Ok(path)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffListItem {
    pub path: String,
    pub file_name: String,
    pub target: String,
}

pub fn list_handoffs(data_dir: &Path, target: Option<&str>) -> Vec<HandoffListItem> {
    let mut out = Vec::new();
    let roots: Vec<PathBuf> = match target {
        Some(t) => match normalize_target(t) {
            Ok(t) => vec![target_dir(data_dir, &t)],
            Err(_) => return out,
        },
        None => {
            let root = dispatch_root(data_dir);
            if !root.is_dir() {
                return out;
            }
            match std::fs::read_dir(&root) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect(),
                Err(_) => return out,
            }
        }
    };

    for dir in roots {
        let target = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.filter_map(|e| e.ok()) {
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            out.push(HandoffListItem {
                path: path.to_string_lossy().to_string(),
                file_name: path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                target: target.clone(),
            });
        }
    }
    out.sort_by(|a, b| b.file_name.cmp(&a.file_name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{Session, StoredMessage};

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "fa_dispatch_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn msg(role: &str, text: &str, images: Vec<String>) -> StoredMessage {
        StoredMessage {
            role: role.into(),
            text: text.into(),
            images,
            steps: vec![],
            reasoning: None,
            interrupted: false,
            steer_id: None,
            model: None,
            usage: None,
            at: 1,
        }
    }

    #[test]
    fn effective_messages_filters_empty_and_caps() {
        let msgs = vec![
            msg("user", "  ", vec![]),
            msg("assistant", "", vec![]),
            msg("user", "真正的问题", vec!["D:\\a.png".into(), "data:image/png;base64,xx".into()]),
            msg("assistant", "回答", vec![]),
        ];
        let eff = effective_messages(&msgs);
        assert_eq!(eff.len(), 2);
        assert_eq!(eff[0].text, "真正的问题");
        assert_eq!(eff[0].images, vec!["D:\\a.png".to_string()]);
    }

    #[test]
    fn write_handoff_from_session_summarizes_and_copies_images() {
        let d = tmp_dir("sess");
        let img_src = d.join("src.png");
        std::fs::write(&img_src, b"fake-png-bytes").unwrap();
        let s = Session {
            id: "s-test".into(),
            title: "测试会话".into(),
            created_at: 1,
            updated_at: 1,
            messages: vec![
                msg("user", "帮我改权限模块", vec![img_src.to_string_lossy().to_string()]),
                msg("assistant", "这里我卡住了，建议交给 WorkBuddy", vec![]),
            ],
            kind: "main".into(),
            forked_from: None,
            fork_at: None,
            summary: None,
            summary_upto: None,
        };
        let sdir = crate::sessions::sessions_dir(&d);
        std::fs::create_dir_all(&sdir).unwrap();
        let sp = sdir.join("s-test.json");
        std::fs::write(&sp, serde_json::to_string(&s).unwrap()).unwrap();
        let _ = std::fs::write(sdir.join("current.txt"), "s-test");

        let meta = write_handoff_from_session(
            &d,
            "s-test",
            "workbuddy",
            None,
            None,
            "小窗上下文不够",
            "",
            "model",
        )
        .unwrap();
        assert_eq!(meta.message_count, 2);
        assert!(!meta.images.is_empty(), "应复制会话图片");
        let body = std::fs::read_to_string(&meta.path).unwrap();
        assert!(body.contains("帮我改权限模块"));
        assert!(body.contains("第一轮"));
        assert!(body.contains("WorkBuddy"));
        assert!(Path::new(&meta.images[0]).is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reject_unknown_target_and_empty_fields() {
        let d = tmp_dir("bad");
        assert!(write_handoff(&d, "cursor", "t", "s", "r", "", "model", &[], &[]).is_err());
        assert!(write_handoff(&d, "workbuddy", " ", "s", "r", "", "model", &[], &[]).is_err());
        assert!(write_handoff(&d, "workbuddy", "t", " ", "r", "", "model", &[], &[]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn expand_windows_env_known_and_unknown() {
        std::env::set_var("FA_TEST_VAR", "hello");
        assert_eq!(expand_windows_env("%FA_TEST_VAR%\\x"), "hello\\x");
        assert_eq!(expand_windows_env("%NO_SUCH_VAR_XYZ%"), "%NO_SUCH_VAR_XYZ%");
        assert_eq!(expand_windows_env("plain"), "plain");
    }

    #[test]
    fn list_finds_created_handoff() {
        let d = tmp_dir("list");
        let msgs = effective_messages(&[msg("user", "任务A", vec![])]);
        write_handoff(
            &d,
            "workbuddy",
            "任务A",
            "摘要",
            "原因",
            "ctx",
            "user",
            &[],
            &msgs,
        )
        .unwrap();
        let items = list_handoffs(&d, Some("workbuddy"));
        assert_eq!(items.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
