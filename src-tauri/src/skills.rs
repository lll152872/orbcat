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
///
/// ⚠️ 过项目过滤：项目 `skills` 清单非空时只列其中启用的（见 `project.rs`）。
/// 被排除的技能**不出现在 prompt 里**，模型就不会去调它。
pub fn catalog(data_dir: &Path) -> String {
    let list: Vec<Skill> = discover(data_dir)
        .into_iter()
        .filter(|s| crate::project::allows_skill(data_dir, &s.name))
        .collect();
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

/// 一次最多自动加载几个技能 —— 防止用户一句话命中一堆、prompt 膨胀。
const MAX_AUTOLOAD: usize = 3;

/// 方案 B：按用户本轮消息的关键词**自动加载**命中的技能正文。
///
/// 与 `catalog()`（只给一行描述、靠模型自己调 `load_skill`）互补 ——
/// 这里在组装 prompt 时就把命中技能的**全文**塞进去，模型不必再调工具，
/// 从根上避免「模型觉得自己懂了、跳过 load_skill」导致的技能空转。
///
/// 触发词来源（按信号强度）：
/// - **技能名**（`self-rebuild` / `dual-token-dashboard`；匹配时忽略
///   连字符、下划线、大小写）
/// - **描述里成对引号内的短语**：「…」『…』“…”"…"‘…’
/// - 描述里**没有引号**时，退化用标点/空白切出的短语（长度 ≥ 2、
///   剔除虚词），且要**至少命中两个**才算数 —— 压制误触发
///
/// 无命中返回空串（调用方据此跳过，不往 prompt 里塞空段）。
pub fn autoload(data_dir: &Path, message: &str) -> String {
    let msg = message.trim();
    if msg.is_empty() {
        return String::new();
    }
    let msg_norm = msg.to_lowercase();
    let msg_stripped = strip_seps(msg);

    let mut hits: Vec<(usize, Skill, String)> = Vec::new();
    for sk in discover(data_dir) {
        // 项目未启用的技能不该被自动注入 —— 那比"出现在清单里"更糟：
        // 正文会直接进 prompt，用户根本不知道它被加载了。
        if !crate::project::allows_skill(data_dir, &sk.name) {
            continue;
        }
        let score = autoload_score(&sk, &msg_norm, &msg_stripped);
        if score == 0 {
            continue;
        }
        // 复用 load()：正文 + 附属文件清单，口径与模型手动 load 时一致
        if let Ok(body) = load(data_dir, &sk.name) {
            hits.push((score, sk, body));
        }
    }
    if hits.is_empty() {
        return String::new();
    }
    // 得分高者优先；同分按名字排序保证稳定
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    hits.truncate(MAX_AUTOLOAD);

    let mut out = String::from(
        "# 本轮自动加载的技能（已按用户这句话命中的关键词附上全文）\n\n\
         ⚠️ 下列技能的正文**已经在你面前**，直接照其步骤执行，\
         **不要再调 `load_skill`**（重复读纯属浪费）。\
         若某个与本轮任务明显无关，忽略它即可。\n",
    );
    for (_, sk, body) in &hits {
        out.push_str(&format!(
            "\n---\n\n## 已加载技能：{}\n\n{}\n",
            sk.name, body
        ));
    }
    out
}

/// 给一个技能打分：0 = 不命中。
fn autoload_score(sk: &Skill, msg_norm: &str, msg_stripped: &str) -> usize {
    let mut score = 0usize;

    // ① 技能名（忽略连字符/下划线/大小写）
    let name_key = strip_seps(&sk.name);
    if name_key.chars().count() >= 2 && msg_stripped.contains(&name_key) {
        score += 3;
    }

    // ② 触发词
    let quotes = quoted_phrases(&sk.description);
    if !quotes.is_empty() {
        for q in quotes {
            // 一个引号里常用「 / 」并列举法（如「重新构建/更新一下」）——
            // 整句 + 按强分隔符切出的子句都算强触发词，否则用户只说其中
            // 一个例子时永远撞不上整句。
            let mut cands = vec![q.clone()];
            cands.extend(split_strong(&q));
            for c in cands {
                let key = c.trim().to_lowercase();
                if key.chars().count() >= 2 && msg_norm.contains(&key) {
                    score += 3;
                    break; // 同一引号短语只记一次，避免刷分
                }
            }
        }
    } else {
        // 描述没写引号样例 → 切词兜底，但要至少两个词命中才认
        let mut weak = 0usize;
        for seg in split_segments(&sk.description) {
            let key = seg.to_lowercase();
            if key.chars().count() >= 2 && !is_noise(&key) && msg_norm.contains(&key) {
                weak += 1;
            }
        }
        if weak >= 2 {
            score += weak;
        }
    }
    score
}

/// 去掉连字符/下划线/空白并转小写 —— 让 `self-rebuild`、`self_rebuild`、
/// `self rebuild` 归一成同一个 key。
fn strip_seps(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| *c != '-' && *c != '_' && !c.is_whitespace())
        .collect()
}

/// 抽出成对引号内的短语：「」『』“”‘’ 以及 ASCII 的 "" 和 ''。
/// 引号不成对时忽略其一。
fn quoted_phrases(s: &str) -> Vec<String> {
    const PAIRS: [(char, char); 6] = [
        ('「', '」'),
        ('『', '』'),
        ('“', '”'),
        ('‘', '’'),
        ('"', '"'),
        ('\'', '\''),
    ];
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some(&(_, close)) = PAIRS.iter().find(|p| p.0 == chars[i]) {
            if let Some(j) = (i + 1..chars.len()).find(|&j| chars[j] == close) {
                let inner: String = chars[i + 1..j].iter().collect();
                let inner = inner.trim().to_string();
                if !inner.is_empty() {
                    out.push(inner);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// 按标点/空白切短语（兜底路径用）。
fn split_segments(s: &str) -> Vec<String> {
    s.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '、' | '，'
                    | ','
                    | '；'
                    | ';'
                    | '：'
                    | ':'
                    | '／'
                    | '/'
                    | '|'
                    | '｜'
                    | '。'
                    | '！'
                    | '!'
                    | '？'
                    | '?'
                    | '…'
                    | '（'
                    | '）'
                    | '('
                    | ')'
                    | '【'
                    | '】'
                    | '['
                    | ']'
                    | '—'
                    | '·'
            )
    })
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
    .collect()
}

/// 按**强分隔符**切短语（并列举法用）：`/ ／ 、 ， ; ； | ｜ 或`。
/// 与 `split_segments` 的区别：**不按空白切** —— 保住「看看 token 用量」
/// 这类多词短语的完整性；只拆明确表示"并列多个例子"的分隔符。
fn split_strong(s: &str) -> Vec<String> {
    s.split(|c: char| {
        matches!(
            c,
            '/' | '／' | '、' | '，' | ',' | ';' | '；' | '|' | '｜' | '或'
        )
    })
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
    .collect()
}

/// 兜底切词里的高频虚词 —— 命中它们不算数（否则「用户」这种词到处撞）。
fn is_noise(p: &str) -> bool {
    const NOISE: [&str; 12] = [
        "用户", "使用", "触发", "当用户", "时使用", "以及", "的时候", "可以", "需要", "这个",
        "任务", "流程",
    ];
    NOISE.contains(&p)
}

/// load_skill：返回 SKILL.md 全文 + 附属文件清单。
///
/// ⚠️ **必须过项目过滤**（`project::allows_skill`）：`catalog` 里被排除的技能，
/// 模型仍可能凭记忆直接调 `load_skill` 点名要 —— 只过滤清单不过滤这里，
/// 等于过滤形同虚设。三处（catalog / autoload / load）口径必须一致。
pub fn load(data_dir: &Path, name: &str) -> Result<String, String> {
    let sk = discover(data_dir)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let have: Vec<String> =
                discover(data_dir).into_iter().map(|s| s.name).collect();
            format!("没有名为「{name}」的技能。现有技能：{have:?}")
        })?;
    if !crate::project::allows_skill(data_dir, &sk.name) {
        return Err(format!(
            "当前项目未启用技能「{}」。需要它请在项目设置里加入 skills 清单，\
             或切换/退出项目。",
            sk.name
        ));
    }
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
        let p = std::env::temp_dir().join(format!("orbcat_skills_{tag}"));
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

    /// 造一个带引号触发词的技能
    fn seed(d: &Path, name: &str, desc: &str) {
        let sdir = d.join("skills").join(name);
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(
            sdir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n\n# 步骤\n{name} 的正文\n"),
        )
        .unwrap();
    }

    #[test]
    fn autoload_hits_quoted_trigger() {
        let d = tmp("autoload_q");
        seed(
            &d,
            "self-rebuild",
            "重新编译时使用。触发：用户说「重新构建」「更新一下」。",
        );
        // 命中引号短语 → 正文进 prompt
        let a = autoload(&d, "帮我重新构建一下");
        assert!(a.contains("self-rebuild"), "应含技能名:\n{a}");
        assert!(a.contains("的正文"), "应含正文:\n{a}");
        assert!(a.contains("不要再调"), "应有『不必再 load』提示:\n{a}");
        // 无关消息 → 不命中
        assert!(autoload(&d, "今天天气不错").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn autoload_hits_skill_name() {
        let d = tmp("autoload_name");
        seed(&d, "dual-token-dashboard", "生成看板。当用户说\"看看用量\"时使用。");
        // 消息里直接出现技能名（含连字符）
        let a = autoload(&d, "跑一下 dual-token-dashboard");
        assert!(a.contains("dual-token-dashboard 的正文"), "{a}");
        // 归一化：下划线/空格写法也能命中
        let b = autoload(&d, "执行 dual token dashboard 谢谢");
        assert!(b.contains("的正文"), "{b}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn autoload_splits_bundled_quoted_examples() {
        let d = tmp("autoload_split");
        // 真实的写法：一个引号里用「 / 」并列多个例子
        seed(
            &d,
            "dispatch-demo",
            "分发时用。触发：用户说「发给 WB / 分发给 WorkBuddy / 交给 WB」「解决不了就给 WB」。",
        );
        // 只说其中一个例子 → 也要命中
        assert!(autoload(&d, "把这事交给 WB 吧").contains("的正文"));
        assert!(autoload(&d, "发给 WB").contains("的正文"));
        assert!(autoload(&d, "解决不了就给 WB").contains("的正文"));
        // 无关 → 不命中
        assert!(autoload(&d, "随便聊聊天气").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn autoload_no_quote_needs_two_weak_hits() {
        let d = tmp("autoload_weak");
        seed(&d, "video-analyzer", "视频分析、抽帧、转写 的时候用。");
        // 只命中一个弱词 → 不算
        assert!(autoload(&d, "帮我做个视频").is_empty());
        // 命中两个弱词 → 算
        let a = autoload(&d, "视频分析一下顺便转写");
        assert!(a.contains("video-analyzer 的正文"), "{a}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 🔴 真实数据冒烟 —— 用仓库真实的 `agent-data/skills`（含内置三技能）
    /// 验证自动加载命中。手动跑：
    /// ```text
    /// cargo test --lib real_autoload -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "依赖本机真实 agent-data/skills"]
    fn real_autoload_smoke() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-data");
        let cases = [
            ("帮我重新构建一下 orbcat", "self-rebuild"),
            // dual-token-dashboard：多种说法都要中
            ("看看 token 用量", "dual-token-dashboard"),
            ("算一下花了多少 token", "dual-token-dashboard"),
            ("打开 token 看板", "dual-token-dashboard"),
        ];
        for (msg, expect) in cases {
            let out = autoload(&dir, msg);
            let hit = out
                .lines()
                .find(|l| l.starts_with("## 已加载技能："))
                .unwrap_or("(无命中)");
            println!("「{msg}」 → {hit}");
            assert!(out.contains(expect), "「{msg}」应命中 {expect}:\n{out}");
        }
    }

    #[test]
    fn autoload_empty_and_caps_count() {
        let d = tmp("autoload_cap");
        assert!(autoload(&d, "").is_empty());
        assert!(autoload(&d, "   ").is_empty());
        // 造 4 个都会被同一条消息命中的技能 → 最多注入 MAX_AUTOLOAD 个
        for n in ["aaa-one", "bbb-two", "ccc-three", "ddd-four"] {
            seed(&d, n, "触发：用户说「万能咒语」。");
        }
        let a = autoload(&d, "来一段万能咒语");
        let count = a.matches("## 已加载技能：").count();
        assert_eq!(count, MAX_AUTOLOAD, "应封顶 {MAX_AUTOLOAD} 个:\n{a}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
