//! Skills 体系 —— 按需加载的操作手册（渐进式披露）
//!
//! ## 为什么是这个结构
//! 照搬 WorkBuddy / Claude 系验证过的模式：
//! - 每个技能 = `agent-data/skills/<名字>/SKILL.md`，frontmatter 带
//!   `name` + `description`。
//! - system prompt 里**只放清单**（名字 + 一行描述）—— 正文不进 prompt，
//!   不然十个技能就能吃掉几千 token。
//! - 模型判断任务匹配某技能时，调 `load_skill` 拿全文，照着做。
//! - 用户教了一套可复用流程 → `save_skill` 写回目录，下次（包括重启后）还在。
//!
//! ## 热加载
//! 每次组装 prompt / 调用工具时现扫目录。技能数量是「几个到几十个」量级，
//! 扫描成本微秒级，不需要缓存，也就不存在缓存失效问题。

use std::path::{Path, PathBuf};

/// 一个已发现的技能
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// SKILL.md 的绝对路径
    pub path: PathBuf,
}

/// 技能根目录
pub fn skills_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("skills")
}

/// 扫描 skills 目录，解析每个子目录里的 SKILL.md frontmatter。
/// 目录不存在返回空表（首次启动时正常现象）。按名字排序保证清单稳定。
pub fn discover(data_dir: &Path) -> Vec<Skill> {
    let root = skills_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<Skill> = Vec::new();
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let md = dir.join("SKILL.md");
        let Ok(txt) = std::fs::read_to_string(&md) else {
            continue;
        };
        let (name, desc) = parse_frontmatter(&txt);
        // frontmatter 没写 name 时用目录名兜底 —— 降低手写门槛
        let fallback = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = name.unwrap_or(fallback);
        if name.is_empty() {
            continue;
        }
        out.push(Skill {
            name,
            description: desc.unwrap_or_else(|| "（无描述）".into()),
            path: md,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 解析 `---\nkey: value\n---` frontmatter。
/// 支持缩进的续行（折成一行）。只认 name / description 两个键，其余忽略。
/// 返回 (name, description)，缺哪个哪个是 None。
pub fn parse_frontmatter(txt: &str) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut desc = None;

    let mut lines = txt.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None); // 没有 frontmatter
    }
    let mut cur_key = String::new();
    let mut cur_val = String::new();
    let commit = |key: &str, val: &str, name: &mut Option<String>, desc: &mut Option<String>| {
        let v = val.trim();
        if v.is_empty() {
            return;
        }
        match key {
            "name" => *name = Some(v.to_string()),
            "description" => *desc = Some(v.to_string()),
            _ => {}
        }
    };

    for line in lines {
        if line.trim() == "---" {
            commit(&cur_key, &cur_val, &mut name, &mut desc);
            return (name, desc);
        }
        // 续行：以空白开头且已有当前键 → 拼接
        if (line.starts_with(' ') || line.starts_with('\t')) && !cur_key.is_empty() {
            cur_val.push(' ');
            cur_val.push_str(line.trim());
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            commit(&cur_key, &cur_val, &mut name, &mut desc);
            cur_key = k.trim().to_string();
            cur_val = v.trim().to_string();
        }
    }
    // 没找到闭合的 --- ：frontmatter 不完整，按已收集的处理
    commit(&cur_key, &cur_val, &mut name, &mut desc);
    (name, desc)
}

/// 给 system prompt 用的技能清单。
/// 只有一行/技能 —— 正文靠 load_skill。
pub fn catalog(data_dir: &Path) -> String {
    let list = discover(data_dir);
    if list.is_empty() {
        return "（当前没有技能。当用户教会你一套今后会反复用到的操作流程时，\
                用 save_skill 保存下来。）"
            .into();
    }
    list.iter()
        .map(|s| format!("- **{}**：{}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// load_skill：返回 SKILL.md 全文 + 附属文件清单。
pub fn load(data_dir: &Path, name: &str) -> Result<String, String> {
    let sk = discover(data_dir)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let have: Vec<String> =
                discover(data_dir).into_iter().map(|s| s.name).collect();
            format!("没有名为「{name}」的技能。现有技能：{have:?}")
        })?;
    let txt = std::fs::read_to_string(&sk.path).map_err(|e| format!("读取失败: {e}"))?;

    let mut out = txt.trim_end().to_string();
    if let Some(dir) = sk.path.parent() {
        let mut extras: Vec<String> = Vec::new();
        list_rel(dir, dir, &mut extras, 50);
        if !extras.is_empty() {
            out.push_str("\n\n---\n附属文件（相对技能目录，需要时用 read_file 读绝对路径）：\n");
            for f in extras {
                out.push_str(&format!("  {f}\n"));
            }
        }
    }
    Ok(out)
}

/// 递归列技能目录里的附属文件（跳过 SKILL.md 自身）
fn list_rel(root: &Path, dir: &Path, out: &mut Vec<String>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            list_rel(root, &p, out, cap);
        } else {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if rel != "SKILL.md" {
                out.push(rel);
                if out.len() >= cap {
                    return;
                }
            }
        }
    }
}

/// save_skill：把一套流程写成新技能。
/// - `overwrite=false` 且重名 → 报错（防止误覆盖既有技能）
/// - content 是正文（markdown），frontmatter 由这里生成
pub fn save(
    data_dir: &Path,
    name: &str,
    description: &str,
    content: &str,
    overwrite: bool,
) -> Result<String, String> {
    validate_name(name)?;
    if description.trim().is_empty() {
        return Err("description 不能为空 —— 它是清单里唯一的路标，模型靠它决定用不用这个技能".into());
    }
    let dir = skills_dir(data_dir).join(name);
    let path = dir.join("SKILL.md");
    if path.exists() && !overwrite {
        return Err(format!(
            "技能「{name}」已存在（{}）。确认要覆盖就带 overwrite=true，否则换个名字。",
            path.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;

    // 描述压成一行（frontmatter 单行语义）
    let desc_one_line: String = description
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let body = if content.trim().starts_with("---") {
        // 模型自己带了 frontmatter：剥掉，避免双 frontmatter
        match content[3..].find("\n---") {
            Some(i) => content[3 + i + 4..].trim_start().to_string(),
            None => content.to_string(),
        }
    } else {
        content.to_string()
    };

    let file = format!(
        "---\nname: {name}\ndescription: {desc_one_line}\n---\n\n{}\n",
        body.trim()
    );
    std::fs::write(&path, file).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!(
        "技能「{name}」已保存到 {}。下次组装 prompt 时自动出现在清单里。",
        path.display()
    ))
}

/// 技能名规则：小写字母/数字/连字符/下划线，2-64 字符，不做路径穿越。
fn validate_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.len() < 2 || n.len() > 64 {
        return Err("技能名需 2-64 字符".into());
    }
    if n.contains("..") || n.contains('/') || n.contains('\\') {
        return Err("技能名不能包含路径分隔符或 ..".into());
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!(
            "技能名只能用 ascii 小写字母/数字/连字符/下划线（当前「{n}」）"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("float_agent_skills_{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn frontmatter_basic() {
        let txt = "---\nname: foo\ndescription: 干这个事\n---\n\n正文\n";
        let (n, d) = parse_frontmatter(txt);
        assert_eq!(n.as_deref(), Some("foo"));
        assert_eq!(d.as_deref(), Some("干这个事"));
    }

    #[test]
    fn frontmatter_multiline_desc() {
        let txt = "---\nname: foo\ndescription: 第一行\n  第二行续上\n---\n正文\n";
        let (n, d) = parse_frontmatter(txt);
        assert_eq!(n.as_deref(), Some("foo"));
        assert_eq!(d.as_deref(), Some("第一行 第二行续上"));
    }

    #[test]
    fn frontmatter_absent_or_broken() {
        assert_eq!(parse_frontmatter("正文为主").0, None);
        assert_eq!(parse_frontmatter("---\nname: x\n").0.as_deref(), Some("x"));
    }

    #[test]
    fn discover_and_load_roundtrip() {
        let d = tmp("disc");
        let sdir = d.join("skills").join("demo-skill");
        std::fs::create_dir_all(sdir.join("scripts")).unwrap();
        std::fs::write(
            sdir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: 演示\n---\n\n# 步骤\n1. 做 A\n",
        )
        .unwrap();
        std::fs::write(sdir.join("scripts").join("run.ps1"), "echo hi").unwrap();

        let list = discover(&d);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "demo-skill");

        let cat = catalog(&d);
        assert!(cat.contains("demo-skill") && cat.contains("演示"));

        let full = load(&d, "demo-skill").unwrap();
        assert!(full.contains("做 A"));
        assert!(full.contains("scripts/run.ps1"), "应列出附属文件: {full}");

        // 不存在的技能：报错并带现有清单提示
        let miss = load(&d, "nope").unwrap_err();
        assert!(miss.contains("demo-skill"));

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_validates_and_writes() {
        let d = tmp("save");
        // 非法名
        assert!(save(&d, "Bad Name", "d", "c", false).is_err());
        assert!(save(&d, "a/../b", "d", "c", false).is_err());
        assert!(save(&d, "ok", "", "c", false).is_err(), "描述不能为空");

        let msg = save(&d, "ok", "能干 X", "# 步骤\n先 A 后 B", false).unwrap();
        assert!(msg.contains("已保存"));
        let txt = std::fs::read_to_string(d.join("skills").join("ok").join("SKILL.md")).unwrap();
        assert!(txt.starts_with("---\nname: ok"));
        assert!(txt.contains("先 A 后 B"));

        // 重名默认拒绝，overwrite 放行
        assert!(save(&d, "ok", "x", "y", false).is_err());
        assert!(save(&d, "ok", "x", "y", true).is_ok());

        // 模型自己塞了 frontmatter：剥掉，只剩一份
        save(&d, "twofm", "d", "---\nname: junk\n---\n真正文", true).unwrap();
        let txt = std::fs::read_to_string(d.join("skills").join("twofm").join("SKILL.md")).unwrap();
        assert!(!txt[3..].contains("junk"), "不应保留模型自带的 frontmatter:\n{txt}");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn catalog_empty_hint() {
        let d = tmp("empty");
        assert!(catalog(&d).contains("save_skill"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
