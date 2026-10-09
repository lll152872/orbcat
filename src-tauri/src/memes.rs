//! 表情包库 —— `agent-data/memes/` 的**标签制**（2026-10-08 重写）。
//!
//! ## 一张图 = 一组标签；一个标签 = 多张图
//!
//! ```text
//! agent-data/memes/
//!   tags.json            ← 可选，补别名（只增不减）
//!   开心/                ← 目录名 = 这里每张图的标签之一
//!     猫_大笑.png        ← 文件名主干按 `_ - + # 空格` 分词 → 猫 / 大笑
//!     无语.gif
//!   无语/沉默.jpg
//!   无语/职场/扶额.png   ← 嵌套：路径上**每一级目录名**都是标签（无语 / 职场 / 扶额）
//!   摸鱼躺平.webp        ← 直接扔在根下的散图也能用（主干整串当一个标签）
//! ```
//!
//! 每张图的标签 = **路径上每一级目录名** ∪ **文件名分词** ∪ **`tags.json` 里的别名**。
//!
//! 于是：`开心/猫_大笑.png` 同时挂在 `开心` / `猫` / `大笑` 三个标签下；
//! 而 `开心` 这个标签下面有多少张图，就随机发哪张 —— **一个标签对应多张图**。
//!
//! ## 为什么从"目录分类"改成"标签制"（2026-10-08 用户指出）
//!
//! 初版只有"目录名 = 语义 + 文件名 = 精确到那一张"，用户一句话点到要害：
//! **「那按你的逻辑每个下面都只有一张图了」** —— 因为他按语义给文件命名
//! （`无语.png`），那个名字就只指向那**一张**。真要用起来必须能表达
//! "这个意思我有十来张，随便发一张"，也就是业界表情包插件的做法：
//! **多对多**（一张图多标签、一个标签多图）+ **命中后随机挑一张**。
//!
//! ## 为什么保留"文件即数据"、不引 sqlite
//!
//! 与 `skills.rs` 同一取向：**用户能用文件管理器管的东西，就别塞进数据库**。
//! 往 `开心/` 里丢一张 png 它就进库了 —— 没有导入流程、没有索引重建，
//! 也就没有"库说有一张、磁盘上却没有"这种两端不一致的状态。
//! 区别只在于"标签从哪来"：初版是目录名，现在是目录名 ∪ 文件名 ∪ 别名表。
//!
//! `tags.json` 是**纯增益**的：它只往图上**加**标签，不删、不改。
//! 所以它坏了/写错了，最坏结果就是"少几个别名"（退回按文件名用）——
//! 不会出现"某张图因为配置没写对而消失"。这是刻意选的容错方向。
//!
//! ## 模型侧只有一条路：`send_meme`
//!
//! 提示词**不教**模型 `list_dir` 再手写路径 —— 手写路径写错只会渲染成
//! 「图（读不出来）」，比不发更难看。模型只说"要什么标签"，
//! 挑图（含随机与存在性保证）全在这里做完，工具直接把可粘贴的 markdown 交回去。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// 补别名用的可选文件（放 `memes/` 下）。
pub const TAGS_FILE: &str = "tags.json";

/// 注入提示词时最多列几个标签（库大了不能把 prompt 塞爆）。
const MAX_TAGS_IN_PROMPT: usize = 24;
/// 出错时最多回几个可选标签（够模型换一个就行）。
const MAX_TAGS_IN_ERROR: usize = 16;

/// 表情包库根目录。**目录不存在是常态**（首次使用），不当错误 ——
/// 与 `skills::discover` 对不存在目录的处理一致。
pub fn memes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("memes")
}

/// 挑出来的一张图。
#[derive(Debug, Clone)]
pub struct Picked {
    /// 图片绝对路径
    pub path: PathBuf,
    /// 用作 markdown `alt` 的说明。**命中标签时用命中的那个标签**
    /// （`![无语](…)` 比 `![IMG_2034](…)` 可读得多），否则退回文件名主干。
    pub label: String,
}

/// 库里的一张图（含它的标签集）。
#[derive(Debug, Clone)]
struct Item {
    path: PathBuf,
    /// 文件名主干（不带扩展名）
    stem: String,
    /// 去重后的标签（保留首次出现的写法）
    tags: Vec<String>,
    /// 图库包给的**官方描述**（`index.db` 的 caption，如「得意闭眼拳头，好耶」）。
    /// 有它就用它当 markdown 的 `alt` —— 比文件名/标签可读得多。
    caption: Option<String>,
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

/// 标签 → 张数，按张数降序（同数目按名字升序）。
///
/// 排序固定是为了让同一份磁盘内容产出同一个顺序 —— 这段清单会进系统提示，
/// 顺序抖动会让 prompt 前缀不稳定（见 `agent.rs` 的缓存说明）。
pub fn tag_index(data_dir: &Path) -> Vec<(String, usize)> {
    let mut m: BTreeMap<String, usize> = BTreeMap::new();
    for it in collect(data_dir) {
        for t in it.tags {
            *m.entry(t).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// 生成注入系统提示的那段摘要。**库为空时返回空串**（调用方据此跳过注入，
/// 不往 prompt 里塞一段"你有一个空库"的废话）。
pub fn prompt_section(data_dir: &Path) -> String {
    let items = collect(data_dir);
    if items.is_empty() {
        return String::new();
    }
    let tags = tag_index(data_dir);
    let list = tags
        .iter()
        .take(MAX_TAGS_IN_PROMPT)
        .map(|(t, n)| format!("{t}（{n}）"))
        .collect::<Vec<_>>()
        .join(" · ");
    let more = if tags.len() > MAX_TAGS_IN_PROMPT {
        format!(" …（共 {} 个标签）", tags.len())
    } else {
        String::new()
    };

    format!(
        "# 表情包库\n\n\
         你有自己的表情包库：`{dir}`（共 {total} 张）。可用标签（括号里是张数）：{list}{more}\n\n\
         用法：直接调 `send_meme`，`query` 填一个标签，例如 `send_meme(query=\"无语\")`。\
         系统会在该标签下**随机挑一张**发给你（同一个标签通常有好几张，不会老是同一张）。\n\
         ⚠️ **不要**自己 `list_dir` 再手写图片路径：路径写错只会渲染成「图（读不出来）」，\
         比干脆不发更难看。标签对不上时 `send_meme` 会把可用标签列回来，照着换一个就行。\n\
         ⚠️ 表情包是调味品不是主菜：一次一张，整轮最多一两张，别刷屏。",
        dir = memes_dir(data_dir).display(),
        total = items.len(),
    )
}

/// 从库里挑一张图 —— `send_meme` 工具的实现。
///
/// `query` 的分词与打分：
/// 1. 把 query 按 `_ - + # 空格` 切开（整串也当一个候选词，见下）
/// 2. 每张图算**命中词数**：某个标签与某个词"双向包含"就算命中
///    （中文没有分词，用包含关系当兜底：标签`大笑`能被词`大笑`或`笑`命中）
/// 3. 取命中词数**最高**的那一档，在其中**随机**挑一张
/// 4. `query` 为空 → 全库随机
///
/// ⚠️ `query` 给了却**没命中** → 返回 `Err`（附可用标签清单），
///     **不**退回"随便发一张"。这是刻意的：表情包发错比不发更尴尬，
///     让模型看着清单重新挑一次。
pub fn pick(data_dir: &Path, query: Option<&str>) -> Result<Picked, String> {
    let items = collect(data_dir);
    if items.is_empty() {
        return Err(format!(
            "表情包库是空的。把图片放进 `{}`（子目录名当标签，如 开心/ 无语/；\
             文件名里也能带标签，如 `猫_大笑.png`；还可用 `{}` 补别名），重启后即可使用。",
            memes_dir(data_dir).display(),
            TAGS_FILE
        ));
    }

    let q = query.map(str::trim).filter(|s| !s.is_empty());
    let Some(q) = q else {
        let it = &items[pseudo_rand(items.len())];
        return Ok(Picked {
            path: it.path.clone(),
            label: display_label(it, ""),
        });
    };

    // 词表：分词结果 + **整串本身**。
    // 为什么整串也当一个词：`摸鱼躺平` 这种连写的标签不该被切成两个词后
    // 各自去配一半（那会把"摸鱼"和"躺平"两个不同标签的图都拉进来）。
    // 带上整串后，"完整命中"天然多得一分，排在前面的就是最贴的那个。
    let mut words: Vec<String> = Vec::new();
    for w in tokenize(q) {
        if !words.iter().any(|x| x.eq_ignore_ascii_case(&w)) {
            words.push(w);
        }
    }
    if !words.iter().any(|x| x.eq_ignore_ascii_case(q)) {
        words.push(q.to_string());
    }

    let mut scored: Vec<(usize, String, &Item)> = Vec::new();
    for it in &items {
        let mut score = 0usize;
        let mut hit_tag = String::new();
        for w in &words {
            if let Some(t) = it.tags.iter().find(|t| tag_hits(t, w)) {
                score += 1;
                if hit_tag.is_empty() {
                    hit_tag = t.clone();
                }
            }
        }
        if score > 0 {
            scored.push((score, hit_tag, it));
        }
    }

    if scored.is_empty() {
        let tags = tag_index(data_dir);
        let list = tags
            .iter()
            .take(MAX_TAGS_IN_ERROR)
            .map(|(t, n)| format!("{t}（{n}）"))
            .collect::<Vec<_>>()
            .join(" · ");
        let more = if tags.len() > MAX_TAGS_IN_ERROR {
            " …"
        } else {
            ""
        };
        return Err(format!(
            "库里没有匹配「{q}」的表情包。可用标签：{list}{more}。\
             请换一个标签重试；确实没有合适的就别发。"
        ));
    }

    // 只留命中最多的那一档 → 在其中随机（"一个标签多张图、随机发一张"就落在这里）
    let max = scored.iter().map(|(s, _, _)| *s).max().unwrap_or(0);
    let pool: Vec<&(usize, String, &Item)> = scored.iter().filter(|(s, _, _)| *s == max).collect();
    let (_, tag, it) = pool[pseudo_rand(pool.len())];
    Ok(Picked {
        path: it.path.clone(),
        label: display_label(it, &tag.clone()),
    })
}

/// 这张图的 `alt` 用什么：**包给的官方描述** > 命中的标签 > 文件名主干。
///
/// 顺序是有意的：图库包（dsh-meme）的 caption 是人写的、比自动标签可读
/// （「得意闭眼拳头，好耶」 > 「得意」 > `deyi.jpg`）。散图没有 caption，
/// 才退回标签 / 文件名。
fn display_label(it: &Item, hit_tag: &str) -> String {
    if let Some(c) = &it.caption {
        if !c.is_empty() {
            return c.clone();
        }
    }
    if !hit_tag.is_empty() {
        return hit_tag.to_string();
    }
    it.stem.clone()
}

/// 标签与查询词是否算命中：**双向包含**（大小写不敏感）。
///
/// 为什么不要求全等：中文没有分词器，用户给文件起名 `猫猫头大笑.png`、
/// 查询写 `大笑`，全等就永远配不上。包含关系是这里最实用的兜底。
fn tag_hits(tag: &str, word: &str) -> bool {
    let t = tag.to_lowercase();
    let w = word.to_lowercase();
    !t.is_empty() && !w.is_empty() && (t == w || t.contains(&w) || w.contains(&t))
}

// ---------------------------------------------------------------------------
// 收库
// ---------------------------------------------------------------------------

/// 收全库。顺序固定（按绝对路径升序）——同一份磁盘内容必须产出同一个顺序。
///
/// 两种形态并存（2026-10-08 接入 dsh-meme 图库包后）：
/// - **图库包**：子目录里有 `index.db`（SQLite，带每张图的中文描述/关键词）→
///   直接读库，**包的目录布局原样保留**（`<包>/memes/<tag>/<图>`）；
/// - **散图/自建分类**：普通目录 → 走 [`walk`] 递归。
fn collect(data_dir: &Path) -> Vec<Item> {
    let root = memes_dir(data_dir);
    let extra = extra_tags(&root);
    let mut out: Vec<Item> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !is_hidden(&p) {
                subdirs.push(p);
            }
        } else if crate::images::is_image_path(&p) {
            out.push(make_item(&p, &[], &extra));
        }
    }
    subdirs.sort();

    for d in subdirs {
        if is_pack(&d) {
            collect_pack(&d, &mut out);
        } else {
            // 顶层目录的名字**是**标签（`开心/`、`无语/`）—— 与递归进去的
            // 更深层目录一致。图库包除外（包根是 id 不是语义，见 collect_pack）。
            walk(&d, &[name_of(&d)], &extra, &mut out);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// 这是不是一个**图库包**（dsh-meme 的图库：`index.db` + `manifest.json` + `memes/<tag>/`）。
fn is_pack(dir: &Path) -> bool {
    dir.join(PACK_INDEX).is_file()
}

/// 图库包的索引文件名。
pub const PACK_INDEX: &str = "index.db";

/// 读一个图库包：**`index.db` 就是素材的权威清单**（含中文描述与关键词），
/// 目录布局原样保留、不做任何转换 —— 这样以后接别的包（同一格式）零成本。
///
/// ⚠️ **只读打开**：包是别人的数据，我们绝不写它（写坏一个 20KB 的索引
/// 就废了一整套图）。打开/解析失败 → 跳过整包 + 打一行日志，
/// 绝不能让一个坏包把整个表情包库搞挂。
fn collect_pack(pack_dir: &Path, out: &mut Vec<Item>) {
    use rusqlite::OpenFlags;

    let db = pack_dir.join(PACK_INDEX);
    let conn = match rusqlite::Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[orbcat] ⚠️ 图库包 {} 的 {} 打不开（{e}），整包跳过", pack_dir.display(), PACK_INDEX);
            return;
        }
    };

    let mut stmt = match conn.prepare("SELECT path, tag, keywords, caption FROM memes") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[orbcat] ⚠️ 图库包 {} 的表结构认不出来（{e}），整包跳过", pack_dir.display());
            return;
        }
    };

    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    });

    let rows = match rows {
        Ok(it) => it,
        Err(e) => {
            eprintln!("[orbcat] ⚠️ 图库包 {} 读取失败（{e}），整包跳过", pack_dir.display());
            return;
        }
    };

    for row in rows {
        let (rel, tag, keywords, caption) = match row {
            Ok(v) => v,
            Err(_) => continue,
        };
        // rel 是**相对包根**的路径（`memes/<tag>/<图>`）—— 拼上包目录才是真实位置
        let p = pack_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !p.is_file() || !crate::images::is_image_path(&p) {
            continue; // 索引里有、磁盘上没有 → 跳过（部分下载是常态）
        }
        let stem = stem_of(&p);
        let mut tags: Vec<String> = Vec::new();
        push_tag(&mut tags, &tag);
        for k in keywords.split_whitespace() {
            push_tag(&mut tags, k);
        }
        let caption = if caption.trim().is_empty() {
            None
        } else {
            Some(caption.trim().to_string())
        };
        out.push(Item {
            path: p,
            stem,
            tags,
            caption,
        });
    }
}

/// 递归收一个目录：**路径上每一级目录名都是标签**。
///
/// 为什么要递归（而不是"只认一层"）：不递归的话，用户顺手建的
/// `无语/职场/xxx.png` 会**凭空消失**（既不在库里、也没有任何提示）——
/// 这种静默不可达是这个项目最忌讳的一类 bug。递归之后嵌套反而有了意义：
/// 越深越具体，`无语/职场/扶额.png` 同时挂在 `无语` / `职场` / `扶额` 下。
///
/// ⚠️ 深度有闸（[`MAX_DEPTH`]）：目录软链/junction 能成环，无限递归会把
///    启动卡死。表情包分类不存在超过 4 层的真实需求。
fn walk(
    dir: &Path,
    groups: &[String],
    extra: &HashMap<String, Vec<String>>,
    out: &mut Vec<Item>,
) {
    let depth = groups.len();
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !is_hidden(&p) {
                subdirs.push(p);
            }
        } else if crate::images::is_image_path(&p) {
            out.push(make_item(&p, groups, extra));
        }
    }
    subdirs.sort();

    for d in subdirs {
        let mut g: Vec<String> = groups.to_vec();
        g.push(name_of(&d));
        walk(&d, &g, extra, out);
    }
}

/// 组装一张图：标签 = **路径上每一级目录名** ∪ **文件名分词** ∪ **别名表**。
///
/// ⚠️ 不做 canonicalize：Windows 上会给路径加 `\\?\` 前缀，
/// 写进 markdown 后既难看、又可能让 `image::open` 认不出来。
fn make_item(p: &Path, groups: &[String], extra: &HashMap<String, Vec<String>>) -> Item {
    let stem = stem_of(p);
    let mut tags: Vec<String> = Vec::new();
    for g in groups {
        push_tag(&mut tags, g);
    }
    for t in tokenize(&stem) {
        push_tag(&mut tags, &t);
    }
    if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
        if let Some(list) = extra.get(name) {
            for t in list {
                push_tag(&mut tags, t);
            }
        }
    }
    Item {
        path: p.to_path_buf(),
        stem,
        tags,
        caption: None,
    }
}

/// 加标签并去重（大小写不敏感，保留首次出现的写法）。
fn push_tag(tags: &mut Vec<String>, raw: &str) {
    let t = raw.trim();
    if t.is_empty() {
        return;
    }
    if !tags.iter().any(|x| x.eq_ignore_ascii_case(t)) {
        tags.push(t.to_string());
    }
}

/// 文件名 → 标签词。分隔符：`_ - + # 空格 全角空格 逗号`。
///
/// 不用 `.` 当分隔符：主干里出现点（`v1.2`）切碎反而更难认；
/// 而真正的扩展名在 `stem_of` 里已经掉了。
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| {
        c == '_' || c == '-' || c == '+' || c == '#' || c == ' ' || c == '　' || c == ','
    })
    .map(str::trim)
    .filter(|t| !t.is_empty())
    .map(|t| t.to_string())
    .collect()
}

/// 读可选的别名表 `memes/tags.json`。
///
/// 支持两种写法（键可以是文件名，也可以是 `memes/` 下的相对路径；
/// 值可以是字符串，也可以是字符串数组）：
/// ```json
/// { "IMG_2034.png": ["无语", "扶额"], "开心/猫.png": "猫猫" }
/// ```
///
/// **容错方向必须偏"少"**：文件不存在 / 解析失败 / 键值类型不对 → 忽略，
/// 表情包库照旧按目录名与文件名工作。绝不能让一个写坏的别名表把整库搞空。
fn extra_tags(root: &Path) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(txt) = std::fs::read_to_string(root.join(TAGS_FILE)) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        eprintln!("[orbcat] ⚠️ memes/{TAGS_FILE} 解析失败，已忽略（仍按目录名/文件名取标签）");
        return out;
    };
    // 允许外面再包一层 {"tags": {...}}，纯粹为了让人手写时能加注释之外的东西
    let map = v.get("tags").unwrap_or(&v);
    let Some(obj) = map.as_object() else {
        return out;
    };
    for (key, val) in obj {
        let mut list: Vec<String> = Vec::new();
        match val {
            serde_json::Value::String(s) => list.extend(tokenize(s)),
            serde_json::Value::Array(a) => {
                for it in a {
                    if let Some(s) = it.as_str() {
                        list.extend(tokenize(s));
                    }
                }
            }
            _ => continue,
        }
        if list.is_empty() {
            continue;
        }
        // 键是路径就取文件名 —— 别名挂到**文件名**上（同名文件在不同标签目录下会一起生效，
        // 这是刻意的取舍：比"必须写全路径、写错就静默失效"友好得多）
        let key = key.replace('\\', "/");
        let Some(name) = key.rsplit('/').next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let slot = out.entry(name.to_string()).or_default();
        for t in list {
            push_tag(slot, &t);
        }
    }
    out
}

/// 收库的最大目录深度（防软链/junction 成环把启动卡死）。
///
/// 4 层对表情包分类远远够用（`情绪/职场/扶额.png` 已经 3 层了）。
const MAX_DEPTH: usize = 4;

fn is_hidden(p: &Path) -> bool {
    p.file_name()
        .map(|s| s.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 文件名主干（不带扩展名）
fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "表情".to_string())
}

/// 够用的伪随机：**时间 × 调用序号**，过一遍 splitmix64 收尾再取模。
///
/// ## ⚠️ 为什么不能只拿 `subsec_nanos() % n`（2026-10-08 实测踩到）
///
/// Windows 上 `SystemTime::now()` 的分辨率绑在**系统时钟 tick** 上（默认 15.6ms），
/// 一个 tick 内连调几十次拿到的纳秒值**完全一样**。更糟的是这个 tick 增量
/// `15_600_000` 是 5 的倍数 —— 于是 `subsec_nanos() % 5` **恒等于 0**：
/// 5 张图的标签下，60 次全挑同一张。测试
/// `one_tag_maps_to_many_images_and_picks_randomly` 就是抓这个的。
///
/// 所以必须**先搅散再取模**：时间戳异或一个自增序号，再过一遍 splitmix64 收尾
/// （雪崩好、零依赖）。带上序号之后，同一个 tick 内连续调用也会得到不同结果。
///
/// 为什么不用 `rand`：项目零额外依赖（见 `command_policy.rs` / `images.rs`
/// 用 `DefaultHasher` 的同一理由）。这里只要"每次挑得不一样"，不需要统计质量
/// —— 但**不能**用 `DefaultHasher` 那种纯确定性哈希，否则同一个标签
/// 每次都挑到同一张，聊几轮就复读了。
fn pseudo_rand(n: usize) -> usize {
    use std::sync::atomic::{AtomicU64, Ordering};
    /// 进程内调用序号：唯一目的是把"同一个时钟 tick 内的连续调用"区分开。
    static TICK: AtomicU64 = AtomicU64::new(0);

    if n <= 1 {
        return 0;
    }
    let seq = TICK.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let mut x = now ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x % n as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("orbcat_memes_{tag}_{stamp}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 目录不存在 → 空结果 + 空摘要（首次使用的常态，不是错误）
    #[test]
    fn missing_dir_is_empty_not_error() {
        let d = tmp("missing");
        assert!(collect(&d).is_empty());
        assert!(tag_index(&d).is_empty());
        assert!(prompt_section(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 标签有三个来源：**目录名、文件名分词、别名表** —— 而且能叠加。
    #[test]
    fn tags_come_from_folder_filename_and_sidecar() {
        let d = tmp("tags3src");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("开心")).unwrap();
        std::fs::write(root.join("开心/猫_大笑.png"), b"x").unwrap();
        std::fs::write(root.join("散图.webp"), b"x").unwrap();
        std::fs::write(root.join("说明.txt"), b"x").unwrap();
        std::fs::write(
            root.join(TAGS_FILE),
            r#"{"散图.webp":["摸鱼","老板来了"],"不存在的图.png":["无所谓"]}"#,
        )
        .unwrap();

        let items = collect(&d);
        assert_eq!(items.len(), 2, "非图片不算: {items:?}");
        let cat = items.iter().find(|i| i.stem == "猫_大笑").unwrap();
        for t in ["开心", "猫", "大笑"] {
            assert!(cat.tags.iter().any(|x| x == t), "缺标签 {t}: {:?}", cat.tags);
        }
        let loose = items.iter().find(|i| i.stem == "散图").unwrap();
        for t in ["散图", "摸鱼", "老板来了"] {
            assert!(
                loose.tags.iter().any(|x| x == t),
                "缺标签 {t}: {:?}",
                loose.tags
            );
        }
        // 别名指向不存在的文件 → 静默忽略，不产生幽灵条目
        assert_eq!(items.len(), 2);

        let idx = tag_index(&d);
        assert!(idx.iter().any(|(t, n)| t == "开心" && *n == 1));
        assert!(idx.iter().any(|(t, n)| t == "摸鱼" && *n == 1));
        // 张数降序：两个 1 张的标签按名字排，顺序必须稳定
        assert_eq!(idx, tag_index(&d));

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **一个标签对应多张图，且随机发一张** —— 这条就是 2026-10-08 改标签制的理由。
    ///
    /// 回归背景：初版"目录名 = 语义 / 文件名 = 精确到那一张"，
    /// 于是用户按语义命名文件时，一个意思只有**一张**图可选。
    #[test]
    fn one_tag_maps_to_many_images_and_picks_randomly() {
        let d = tmp("many");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("开心")).unwrap();
        for i in 0..5 {
            std::fs::write(root.join("开心").join(format!("{i}.png")), b"x").unwrap();
        }

        let idx = tag_index(&d);
        assert!(
            idx.iter().any(|(t, n)| t == "开心" && *n == 5),
            "开心 这个标签下应该有 5 张: {idx:?}"
        );

        let mut seen: Vec<String> = Vec::new();
        for _ in 0..60 {
            let p = pick(&d, Some("开心")).unwrap();
            let s = p.path.to_string_lossy().into_owned();
            assert!(s.contains("开心"), "只在命中标签内挑: {s}");
            assert_eq!(p.label, "开心", "命中的标签拿来做 alt");
            if !seen.contains(&s) {
                seen.push(s);
            }
        }
        assert!(
            seen.len() > 1,
            "60 次全挑到同一张说明没有随机（5 张里挑到 1 张的概率是 5^-59）"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 一张图挂**多个**标签：文件名分词让同一张图从多个入口都能被找到。
    #[test]
    fn one_image_is_reachable_by_several_tags() {
        let d = tmp("multi");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("开心")).unwrap();
        std::fs::write(root.join("开心/猫_大笑.png"), b"x").unwrap();

        for q in ["开心", "猫", "大笑"] {
            let p = pick(&d, Some(q)).unwrap_or_else(|e| panic!("{q} 该能命中: {e}"));
            assert!(p.path.to_string_lossy().ends_with("猫_大笑.png"));
        }

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 查询词多一个、命中更精确的那张应该赢（打分是"命中词数"）。
    ///
    /// ⚠️ 用**同一层**里的两张图来构造：`无语/沉默.png` 的标签是 {无语, 沉默}，
    ///    而 `无语/随便.png` 只有 {无语} —— 查询「无语 沉默」时后者只命中 1 个词。
    #[test]
    fn more_specific_match_wins() {
        let d = tmp("score");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("无语")).unwrap();
        std::fs::write(root.join("无语/随便.png"), b"x").unwrap();
        std::fs::write(root.join("无语/沉默.png"), b"x").unwrap();

        for _ in 0..12 {
            let p = pick(&d, Some("无语 沉默")).unwrap();
            assert!(
                p.path.to_string_lossy().ends_with("沉默.png"),
                "命中两个词的那张该赢: {:?}",
                p.path
            );
        }
        // 只查一个词时两张都有机会（都在 无语 这个标签下）
        let mut seen_same = 0;
        for _ in 0..40 {
            if pick(&d, Some("无语")).unwrap().path.to_string_lossy().ends_with("随便.png") {
                seen_same += 1;
            }
        }
        assert!(seen_same > 0, "只命中一个词时不该永远只有一张能出");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **嵌套目录不丢图，而且每一级目录名都变成标签**。
    ///
    /// 回归背景：初版收库**不递归** —— 用户顺手建一层 `无语/职场/xxx.png`，
    /// 那些图会凭空消失（不在库里、也没有任何提示）。
    #[test]
    fn nested_dirs_add_tags_and_never_lose_images() {
        let d = tmp("nested");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("无语/职场")).unwrap();
        std::fs::write(root.join("无语/职场/扶额.png"), b"x").unwrap();

        let items = collect(&d);
        assert_eq!(items.len(), 1, "嵌套里的图必须进库: {items:?}");
        let tags = &items[0].tags;
        for t in ["无语", "职场", "扶额"] {
            assert!(tags.iter().any(|x| x == t), "缺标签 {t}: {tags:?}");
        }

        // 三个标签都能查到它，且越具体越优先
        for q in ["无语", "职场", "扶额"] {
            assert!(pick(&d, Some(q)).is_ok(), "{q} 该能命中");
        }

        // 深度闸：够深的一层不再往下走（防软链成环），但不 panic
        let deep = root.join("a1/a2/a3/a4/a5/a6");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.png"), b"x").unwrap();
        let n_before = collect(&d).len();
        assert_eq!(n_before, 1, "超过深度闸的图不收，但也不许把前面的搞坏");
        assert_eq!(collect(&d).len(), 1, "重复收库结果一致");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 别名表坏掉 / 不存在都不影响按目录名与文件名用（容错方向偏"少"）。
    #[test]
    fn broken_sidecar_is_ignored_not_fatal() {
        let d = tmp("badsidecar");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("无语")).unwrap();
        std::fs::write(root.join("无语/沉默.png"), b"x").unwrap();
        std::fs::write(root.join(TAGS_FILE), "{ 这不是 JSON").unwrap();

        assert_eq!(pick(&d, Some("无语")).unwrap().label, "无语");
        assert_eq!(pick(&d, Some("沉默")).unwrap().label, "沉默");

        // 类型不对的条目也忽略，且不影响别的条目
        std::fs::write(
            root.join(TAGS_FILE),
            r#"{"无语/沉默.png":{"nested":"obj"},"沉默.png":123}"#,
        )
        .unwrap();
        assert!(tag_index(&d).iter().any(|(t, _)| t == "无语"));

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 空库 → `pick` 报错（带"怎么放图"的指引），既不 panic 也不硬发
    #[test]
    fn pick_on_empty_library_errors_with_hint() {
        let d = tmp("pickempty");
        let e = pick(&d, None).unwrap_err();
        assert!(e.contains("空的"), "{e}");
        assert!(e.contains("memes"), "{e}");
        // 提示要把三种放图方式都说清楚（目录名 / 文件名 / 别名表）
        assert!(e.contains(TAGS_FILE), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// **没命中 → Err 且带可用标签**（宁可让模型重挑，也不硬发一张不相关的）
    #[test]
    fn no_match_errors_with_available_tags() {
        let d = tmp("nomatch");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("无语")).unwrap();
        std::fs::create_dir_all(root.join("开心")).unwrap();
        std::fs::write(root.join("无语/沉默.png"), b"x").unwrap();
        std::fs::write(root.join("开心/大笑.png"), b"x").unwrap();

        let e = pick(&d, Some("愤怒")).unwrap_err();
        assert!(e.contains("无语") && e.contains("开心"), "报错要带可用标签: {e}");
        assert!(e.contains("愤怒"), "要说清是哪个词没配上: {e}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 不给 query → 全库随机，但**挑出来的必须真实存在于库里**
    #[test]
    fn pick_without_query_stays_inside_library() {
        let d = tmp("pickany");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("摸鱼")).unwrap();
        std::fs::write(root.join("摸鱼/躺平.png"), b"x").unwrap();
        for _ in 0..8 {
            let p = pick(&d, None).unwrap();
            assert!(p.path.exists(), "挑出来的路径必须真实存在: {:?}", p.path);
            assert_eq!(p.label, "躺平", "没命中标签时 alt 用文件名主干");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 标签清单顺序必须稳定 —— 它进系统提示，顺序抖动等于白烧一次 prompt 缓存。
    #[test]
    fn tag_index_order_is_stable_and_count_desc() {
        let d = tmp("stable");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join("摸鱼")).unwrap();
        std::fs::create_dir_all(root.join("开心")).unwrap();
        std::fs::write(root.join("摸鱼/a.png"), b"x").unwrap();
        std::fs::write(root.join("摸鱼/b.png"), b"x").unwrap();
        std::fs::write(root.join("开心/c.png"), b"x").unwrap();

        assert_eq!(tag_index(&d), tag_index(&d));
        let idx = tag_index(&d);
        assert_eq!(idx[0], ("摸鱼".to_string(), 2), "张数多的排前面: {idx:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 点开头的目录（`.trash` 等）不参与收库
    #[test]
    fn hidden_dirs_are_skipped() {
        let d = tmp("hidden");
        let root = memes_dir(&d);
        std::fs::create_dir_all(root.join(".trash")).unwrap();
        std::fs::write(root.join(".trash/old.png"), b"x").unwrap();
        assert!(tag_index(&d).is_empty(), "点开头目录里的图不该进库");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 建一个**与 dsh-meme 同构**的图库包（`index.db` + `manifest.json` + `memes/<tag>/`）
    fn make_pack(root: &Path, pack_id: &str, rows: &[(&str, &str, &str, &str)]) {
        use rusqlite::params;
        let dir = root.join(pack_id);
        let meme_dir = dir.join("memes");
        std::fs::create_dir_all(&meme_dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join(PACK_INDEX)).unwrap();
        conn.execute_batch(
            "CREATE TABLE memes (path TEXT PRIMARY KEY, tag TEXT NOT NULL, \
             file_name TEXT NOT NULL, file_hash TEXT, caption TEXT NOT NULL DEFAULT '', \
             keywords TEXT NOT NULL DEFAULT '', mtime REAL, captioned_at REAL);",
        )
        .unwrap();
        for (path, tag, caption, keywords) in rows {
            // path = `<tag>/<图>`，要落到 `memes/<tag>/<图>`（与真实包布局一致）
            let dst = meme_dir.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(&dst, b"x").unwrap();
            conn.execute(
                "INSERT INTO memes (path, tag, file_name, caption, keywords) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![format!("memes/{path}"), tag, path.split_once('/').unwrap().1, caption, keywords],
            )
            .unwrap();
        }
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"id":"{pack_id}","license":"personal"}}"#),
        )
        .unwrap();
    }

    /// **图库包原样接入**：`index.db` 里的中文描述/关键词要变成可搜的标签，
    /// `caption` 要当 `alt` 用 —— 这是接入 dsh-meme 图库的全部意义。
    #[test]
    fn pack_index_db_is_read_and_becomes_searchable() {
        let d = tmp("pack");
        make_pack(
            memes_dir(&d).as_path(),
            "dafeiyu-001",
            &[("happy/deyi.jpg", "happy", "得意闭眼拳头，好耶", "得意 好耶 满意 开心")],
        );

        let items = collect(&d);
        assert_eq!(items.len(), 1, "包里的图要进库: {items:?}");
        let it = &items[0];
        assert_eq!(
            it.caption.as_deref(),
            Some("得意闭眼拳头，好耶"),
            "官方描述要带出来"
        );
        for t in ["happy", "得意", "好耶"] {
            assert!(it.tags.iter().any(|x| x == t), "缺标签 {t}: {:?}", it.tags);
        }

        // 中文关键词能搜到（文件名是拼音 `deyi.jpg`，没有库里这份描述就永远配不上）
        let p = pick(&d, Some("得意")).unwrap();
        assert!(p.path.to_string_lossy().ends_with("deyi.jpg"));
        assert_eq!(p.label, "得意闭眼拳头，好耶", "alt 用包给的官方描述");
        assert!(p.path.exists());

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **包根自己的名字不当标签** —— `dafeiyu-001` 是包 id 不是语义，
    /// 混进标签清单就是纯噪声（还会让 prompt 里那段「可用标签」变长）。
    #[test]
    fn pack_root_name_is_not_a_tag() {
        let d = tmp("packroot");
        make_pack(
            memes_dir(&d).as_path(),
            "dafeiyu-001",
            &[("happy/deyi.jpg", "happy", "得意", "得意 开心")],
        );
        let idx = tag_index(&d);
        assert!(
            !idx.iter().any(|(t, _)| t.contains("dafeiyu")),
            "包 id 不该出现在标签里: {idx:?}"
        );
        assert!(idx.iter().any(|(t, _)| t == "得意"), "{idx:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// **坏包只跳过自己，不拖累整个库** —— 一个 20KB 的索引写坏了，
    /// 不该把用户自己攒的表情包一起搞没。
    #[test]
    fn broken_pack_is_skipped_not_fatal() {
        let d = tmp("badpack");
        let root = memes_dir(&d);
        // 坏包：index.db 不是 SQLite
        let bad = root.join("broken-pack");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(PACK_INDEX), "{ 这不是 SQLite").unwrap();
        // 好包：正常
        make_pack(root.as_path(), "dafeiyu-001", &[("happy/deyi.jpg", "happy", "得意", "得意")]);
        // 用户自己的散图也该照常进库
        std::fs::write(root.join("我的.png"), b"x").unwrap();

        let items = collect(&d);
        assert_eq!(items.len(), 2, "坏包跳过，好包与散图照常: {items:?}");
        assert!(items.iter().any(|i| i.caption.is_some()), "好包的数据还在");
        assert!(items.iter().any(|i| i.stem == "我的"), "用户散图还在");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 真机烟测：`ORBCAT_MEMES_SMOKE` 指向真实的**数据目录**（`agent-data`）时才跑
    /// （`cargo test --lib memes:: -- --ignored`）。验证接入的 dsh-meme 包
    /// 能被读出足量图片、且每张都带中文描述。
    #[test]
    #[ignore = "真机烟测：设 ORBCAT_MEMES_SMOKE 指向 agent-data/memes 才跑"]
    fn real_pack_smoke() {
        let Ok(dir) = std::env::var("ORBCAT_MEMES_SMOKE") else {
            eprintln!("跳过：未设 ORBCAT_MEMES_SMOKE");
            return;
        };
        // ⚠️ 这里传的是**数据目录**（agent-data），不是 memes/ ——
        //    `collect()` 内部会自己拼 `memes`。传错一层就永远是 0 张，
        //    而且没有任何报错（目录不存在 = 空库是合法状态）。第一次就栽在这。
        let items = collect(PathBuf::from(dir.clone()).as_path());
        assert!(items.len() >= 20, "应读到 20+ 张，实际 {}", items.len());
        let with_caption = items.iter().filter(|i| i.caption.is_some()).count();
        assert!(
            with_caption >= items.len() / 2,
            "大部分图该带官方描述：{with_caption}/{}",
            items.len()
        );
        eprintln!("图库共 {} 张，其中 {} 张带描述", items.len(), with_caption);
    }
}
