//! 初始化 —— 纯 Rust 建出 `agent-data/` 骨架
//!
//! ## 为什么必须是纯代码
//!
//! 「让 agent 自己 mkdir」这条路是死的，有个闭环：
//!
//! ```text
//!   要跑 agent   →  得有模型配置（models.json）
//!   要有模型配置 →  得先有 agent-data/
//!   要有 agent-data/  →  得跑 agent 去建
//!                            ↑ 回到第一步
//! ```
//!
//! 所以初始化**第一段**必须由不依赖模型的代码完成：建目录、写空的
//! `models.json`。用户在设置面板填完一个模型，闭环才被打开。
//!
//! 剩下的「你是谁」——`RULES.md` / `SOUL.md` / `IDENTITY.md` / `USER.md`
//! ——必须**问用户**才能写，那是模型就绪之后的事，属于第二段，
//! 由 agent 读项目根的 `初始化.md` 自行推进。
//!
//! ## 幂等
//!
//! 已存在的文件**一律不覆盖**。用户可能已经改过 `settings.json` 或
//! `MEMORY.md`，重跑初始化不能毁掉它们。

use std::path::Path;

/// 初始化结果，回给前端用于提示
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapReport {
    /// 数据目录绝对路径
    pub data_dir: String,
    /// 本次新建的目录（相对 data_dir）
    pub created_dirs: Vec<String>,
    /// 本次新建的文件（相对 data_dir）
    pub created_files: Vec<String>,
    /// 本来就有、跳过的路径（相对 data_dir）
    pub skipped: Vec<String>,
    /// 是否已经存在过 models.json 且有内容 —— 前端据此决定要不要提示"去加模型"
    pub has_models: bool,
}

/// 需要建的 8 个目录。顺序 = 提示里的展示顺序。
const DIRS: [&str; 8] = [
    "daily",
    "images",
    "memory",
    "pending",
    "projects",
    "screenshots",
    "sessions",
    "skills",
];

/// 内置技能 —— 随程序分发，初始化时释放到 `agent-data/skills/`。
///
/// 为什么用 `include_str!` 而不是运行时读盘：发布版没有源码目录，
/// 技能得**编进二进制**才能跟着软件走。
///
/// ⚠️ 正文里的 `{{DATA_DIR}}` / `{{SKILL_DIR}}` 是占位符，写入时替换成本机绝对路径 ——
/// 模板里写死路径，换台机器就废了。
const BUNDLED_SKILLS: &[(&str, &str)] = &[
    (
        "dual-token-dashboard/SKILL.md",
        include_str!("../../tools/dual-token-dashboard/SKILL.md"),
    ),
    (
        "dual-token-dashboard/scripts/gen_dual_dashboard.py",
        include_str!("../../tools/dual-token-dashboard/scripts/gen_dual_dashboard.py"),
    ),
    (
        "dual-token-dashboard/panel/index.html",
        include_str!("../../tools/dual-token-dashboard/panel/index.html"),
    ),
    (
        "dual-token-dashboard/panel/panel.css",
        include_str!("../../tools/dual-token-dashboard/panel/panel.css"),
    ),
    (
        "dual-token-dashboard/panel/panel.js",
        include_str!("../../tools/dual-token-dashboard/panel/panel.js"),
    ),
    (
        "dual-token-dashboard/references/adapter-schema.md",
        include_str!("../../tools/dual-token-dashboard/references/adapter-schema.md"),
    ),
];

/// 建目录骨架 + 最小配置文件。**不碰 `models.json` 之外的任何密钥**。
///
/// 返回 [`BootstrapReport`]；任何一步失败都返回 `Err`（带上具体路径），
/// 但不回滚已建的部分 —— 目录都是幂等的，重试即可。
pub fn bootstrap_agent_data(data_dir: &Path) -> Result<BootstrapReport, String> {
    let mut created_dirs = Vec::new();
    let mut created_files = Vec::new();
    let mut skipped = Vec::new();

    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("创建数据目录 {} 失败: {e}", data_dir.display()))?;

    // ── 一、目录 ────────────────────────────────────────────────
    for d in DIRS {
        let p = data_dir.join(d);
        if p.is_dir() {
            skipped.push(d.to_string());
        } else {
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("创建目录 {} 失败: {e}", p.display()))?;
            created_dirs.push(d.to_string());
        }
    }

    // ── 二、models.json（空数组）────────────────────────────────
    // ⚠️ 写空数组而不是模板：模板里的占位 url/apiKey 会让
    //    「模型列表」多出一条点不动的废条目。空数组干净，
    //    UI 直接显示"还没有模型，点这里添加"。
    let models = data_dir.join("models.json");
    let mut has_models = false;
    if models.exists() {
        skipped.push("models.json".into());
        has_models = crate::config::load_models(data_dir)
            .map(|m| !m.is_empty())
            .unwrap_or(false);
    } else {
        std::fs::write(&models, "[]\n")
            .map_err(|e| format!("写入 {} 失败: {e}", models.display()))?;
        created_files.push("models.json".into());
    }

    // ── 三、settings.json（默认值）──────────────────────────────
    let settings = data_dir.join("settings.json");
    if settings.exists() {
        skipped.push("settings.json".into());
    } else {
        let d = crate::config::AgentSettings::default();
        let txt = serde_json::to_string_pretty(&d)
            .map_err(|e| format!("序列化 settings 失败: {e}"))?;
        std::fs::write(&settings, format!("{txt}\n"))
            .map_err(|e| format!("写入 {} 失败: {e}", settings.display()))?;
        created_files.push("settings.json".into());
    }

    // ── 三之二、modes.json（agent 模式定义）──────────────────────
    // 为什么要落盘而不是"不存在就用内置"：文件不存在时用户**看不见**有这么个东西，
    // 也就不会想到可以改它。落一份带说明的模板，才是"可发现、可改"。
    // 与其它文件同一条铁律：**已存在一律不覆盖**（用户改过的东西不能被升级冲掉）。
    let modes_file = data_dir.join(crate::modes::MODES_FILE);
    if modes_file.exists() {
        skipped.push(crate::modes::MODES_FILE.into());
    } else {
        std::fs::write(&modes_file, crate::modes::default_file_text())
            .map_err(|e| format!("写入 {} 失败: {e}", modes_file.display()))?;
        created_files.push(crate::modes::MODES_FILE.into());
    }

    // ── 四、说明性文件（骨架，不覆盖）────────────────────────────
    // 这几个是「给未来的 agent 和用户看的格式说明」，不是人格文件。
    // 人格文件（RULES/SOUL/IDENTITY/USER）故意**不在这里建**：
    // 那是第二段、要问过用户才能写的东西。
    let skeletons: [(&str, &str); 4] = [
        ("memory/MEMORY.md", MEMORY_SKELETON),
        ("skills/README.md", SKILLS_README),
        ("pending/README.md", PENDING_README),
        ("projects/README.md", PROJECTS_README),
    ];
    for (rel, body) in skeletons {
        let p = data_dir.join(rel);
        if p.exists() {
            skipped.push(rel.to_string());
            continue;
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
        }
        std::fs::write(&p, body)
            .map_err(|e| format!("写入 {} 失败: {e}", p.display()))?;
        created_files.push(rel.to_string());
    }

    // ── 五、内置技能（随程序分发，释放到 skills/）────────────────
    // 这些技能**跟着程序走**：编进二进制（见 [`BUNDLED_SKILLS`]），
    // 初始化时落地到 `agent-data/skills/`。agent-data 整体被 gitignore，
    // 所以新克隆的人跑一次初始化就能拿到，不用手动 clone 面板。
    //
    // 策略与骨架文件一致：**已存在就不动**。skills/ 同时是用户和 agent
    // 的地盘（`save_skill` 往里写），无条件覆盖会毁掉他们的改动。
    // 代价：程序升级不会自动刷新已装好的技能 —— 要更新就删掉旧目录再初始化。
    for (rel, tpl) in BUNDLED_SKILLS {
        let full_rel = format!("skills/{rel}");
        let p = data_dir.join(&full_rel);
        if p.exists() {
            skipped.push(full_rel);
            continue;
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
        }
        // SKILL_DIR = 该技能自己的目录（rel 的第一段），不能写死 dual-token-dashboard
        let skill_root = data_dir
            .join("skills")
            .join(rel.split('/').next().unwrap_or_default());
        let body = render_bundled(tpl, data_dir, &skill_root);
        std::fs::write(&p, body)
            .map_err(|e| format!("写入 {} 失败: {e}", p.display()))?;
        created_files.push(full_rel);
    }

    // ── 六、permissions.json 由权限模块自举，这里不碰 ────────────
    if data_dir.join("permissions.json").exists() {
        skipped.push("permissions.json".into());
    }

    Ok(BootstrapReport {
        data_dir: data_dir.display().to_string(),
        created_dirs,
        created_files,
        skipped,
        has_models,
    })
}

/// 把内置模板里的占位符换成本机真实路径。
///
/// 为什么不在模板里写死路径：开发机上的绝对路径换台机器（或换个数据目录）
/// 就全错。释放时按实际 `data_dir` 渲染才通用。
fn render_bundled(tpl: &str, data_dir: &Path, skill_dir: &Path) -> String {
    tpl.replace("{{DATA_DIR}}", &data_dir.display().to_string())
        .replace("{{SKILL_DIR}}", &skill_dir.display().to_string())
}

const MEMORY_SKELETON: &str = "\
# MEMORY.md — 长期记忆

> 本文件是**跨会话的长期提炼**，按主题组织。
> 日记（`daily/YYYY-MM-DD.md`）是流水账；这里是蒸馏后的结论。
>
> **维护原则**：
> - 条目必须有**来源**（哪天的日记 / 哪个文件）
> - 失效条目**直接删除**，不要留着「以防万一」—— 那正是记忆污染的根源
> - 只写**有长期价值**的东西：结论、决策、教训。不写临时状态

---

## 项目

_（待积累）_

## 教训

_（待积累）_

## 用户确认过的决策

_（待积累）_
";

const SKILLS_README: &str = "\
# skills/ — 按需加载的操作手册

每个技能一个子目录，内含 `SKILL.md`：

```
skills/
└── <技能名>/
    └── SKILL.md
```

## SKILL.md 格式

```markdown
---
name: 技能名
description: 一句话说清**什么场景下该用**这个技能
---

正文：具体步骤、命令、注意事项。
```

`description` 是**唯一**决定模型会不会加载这个技能的依据，
所以写「触发场景」而不是「功能介绍」：

- 好：`当用户要求把 Markdown 发布到 CSDN 时使用，含图片上传流程`
- 差：`一个关于 CSDN 的技能`

## 渐进式披露 + 关键词自动加载

system prompt 里**只放清单**（名字 + description），正文默认不进 prompt。
但当用户**本轮消息命中**某个技能的触发词时，系统会把它**自动加载**进来
（拿 description 里的引号短语 / 技能名去匹配），不用模型主动调工具。

所以 description 要写清触发场景，并**把用户可能说的原话用引号列出来**：

- 好：`当用户说「发给 WB」「交给 WorkBuddy」时使用`
- 差：`一个关于分发的技能`

没被自动命中的技能，模型也能自己调 `load_skill` 拿全文。
所以技能写多长都不心疼，写清就行。

## 热加载

每次组装 prompt 时现扫目录，没有缓存 —— 写完立即生效，不用重启。
";

const PENDING_README: &str = "\
# pending/ — 待审批记忆

候选记忆先落这里，**用户批准后**才写进 `memory/MEMORY.md`。

| 文件 | 内容 |
|---|---|
| `candidates.jsonl` | 待审批候选，每行一个 JSON |
| `rejected.jsonl` | 被拒记录（用于避免重复提同一个候选） |

## 候选字段

```json
{
  \"id\": \"唯一 id\",
  \"at\": \"ISO8601 时间\",
  \"scope\": \"user | project\",
  \"kind\": \"fact | preference | decision | lesson\",
  \"content\": \"记忆正文\",
  \"source\": \"来源（哪天/哪个文件）\",
  \"fingerprint\": \"内容指纹，用于去重\"
}
```

**为什么要有审批环节**：记忆是**滚动累积**的，一条错的条目会一直影响
后续所有对话，而且不会自己过期。所以宁可多一道人工关卡。
";

const PROJECTS_README: &str = "\
# projects/ — 项目级记忆

每个子目录 = 一个项目：

```
projects/
└── <项目名>/
    ├── source.ref     源路径（指向真实项目目录）
    ├── MEMORY.md      项目长期记忆
    └── YYYY-MM-DD.md  项目日记
```

## source.ref

一行纯文本，写项目的真实绝对路径。

**懒检测**：启动或首次访问时检查 `source.ref` 指向的路径还在不在。
不在 → 标记 `orphaned`（孤儿），在面板提示用户「源目录没了」，
由用户决定删除还是改路径。**不要自动删** —— 记忆本身还有价值。

## 与 memory/ 的分工

- `memory/MEMORY.md`：**跨项目**的长期记忆（用户偏好、通用教训）
- `projects/<名>/MEMORY.md`：**只在这个项目里成立**的事实
  （构建命令、目录约定、踩过的坑）
";

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fa-bootstrap-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn creates_all_dirs_and_skeleton_files() {
        let d = tmp("fresh");
        let r = bootstrap_agent_data(&d).expect("初始化应成功");

        assert_eq!(r.created_dirs.len(), 8, "8 个目录全新建");
        assert!(r.skipped.is_empty(), "首次运行不该有跳过项: {:?}", r.skipped);
        assert!(!r.has_models, "空 models.json 不算有模型");

        for name in DIRS {
            assert!(d.join(name).is_dir(), "{name} 应存在");
        }
        for f in [
            "models.json",
            "settings.json",
            "memory/MEMORY.md",
            "skills/README.md",
            "pending/README.md",
            "projects/README.md",
        ] {
            assert!(d.join(f).is_file(), "{f} 应存在");
        }

        // 内置技能：随程序分发，初始化时必须落地
        for f in [
            "skills/dual-token-dashboard/SKILL.md",
            "skills/dual-token-dashboard/scripts/gen_dual_dashboard.py",
            "skills/dual-token-dashboard/panel/index.html",
            "skills/dual-token-dashboard/panel/panel.css",
            "skills/dual-token-dashboard/panel/panel.js",
            "skills/dual-token-dashboard/references/adapter-schema.md",
        ] {
            assert!(d.join(f).is_file(), "内置技能文件 {f} 应存在");
        }

        // 占位符必须被替换成本机真实路径，不能烧死在模板里
        let skill_md =
            std::fs::read_to_string(d.join("skills/dual-token-dashboard/SKILL.md")).unwrap();
        assert!(
            !skill_md.contains("{{DATA_DIR}}") && !skill_md.contains("{{SKILL_DIR}}"),
            "占位符应全部被替换:\n{skill_md}"
        );
        assert!(
            skill_md.contains(&d.display().to_string()),
            "应写入真实 data_dir 路径"
        );

        // models.json 必须是**合法空数组**，不能是模板占位
        let txt = std::fs::read_to_string(d.join("models.json")).unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_str(&txt).expect("应为合法 JSON");
        assert!(v.is_empty(), "应为空数组");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn does_not_create_personality_files() {
        // 人格文件必须由「问过用户」的 agent 写，不能由代码凭空造
        let d = tmp("nopersona");
        bootstrap_agent_data(&d).expect("初始化应成功");
        for f in ["RULES.md", "SOUL.md", "IDENTITY.md", "USER.md"] {
            assert!(!d.join(f).exists(), "{f} 不该由 bootstrap 创建");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn second_run_is_idempotent_and_preserves_user_data() {
        let d = tmp("idem");
        bootstrap_agent_data(&d).expect("首次初始化应成功");

        // 模拟用户动过手：写记忆、改设置
        std::fs::write(d.join("memory/MEMORY.md"), "用户改过的记忆").unwrap();
        std::fs::write(d.join("settings.json"), "{\"selectedModel\":\"x\"}").unwrap();

        let r = bootstrap_agent_data(&d).expect("二次初始化应成功");
        assert!(r.created_dirs.is_empty(), "不该重复建目录");
        assert!(r.created_files.is_empty(), "不该重复写文件");
        assert_eq!(
            r.skipped.len(),
            21,
            "8 目录 + 7 骨架/配置文件 + 6 内置技能文件 应全跳过: {:?}",
            r.skipped
        );
        // 用户改过的模式定义同样不能被覆盖（与 settings.json 同一条铁律）
        assert!(
            r.skipped.iter().any(|s| s == "modes.json"),
            "modes.json 应进跳过列表: {:?}",
            r.skipped
        );

        assert_eq!(
            std::fs::read_to_string(d.join("memory/MEMORY.md")).unwrap(),
            "用户改过的记忆",
            "已有记忆不能被覆盖"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("settings.json")).unwrap(),
            "{\"selectedModel\":\"x\"}",
            "已有设置不能被覆盖"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 初始化要落一份**可发现、可改**的 `modes.json`。
    ///
    /// 为什么这条必须有：模式定义如果只活在二进制里，用户根本不知道有这回事，
    /// 也就永远不会想到"我可以加一个自己的模式"。
    #[test]
    fn creates_modes_json_with_four_builtin_modes() {
        let d = tmp("modes");
        bootstrap_agent_data(&d).expect("初始化应成功");

        let p = d.join("modes.json");
        assert!(p.exists(), "初始化应写出 modes.json");
        let modes = crate::modes::load(&d);
        let ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["standard", "ptc", "minimal", "creator"]);
        // 写出来的文件必须是**能解析回来**的（注释块不能把它弄坏）
        assert!(modes.iter().any(|m| m.id == "ptc" && m.allows_all()));

        // 用户改过之后，二次初始化不许覆盖
        std::fs::write(&p, r#"{"modes":[{"id":"mine","name":"我的","description":"x","mcpGroupsPreload":[]}]}"#)
            .unwrap();
        let r = bootstrap_agent_data(&d).expect("二次初始化应成功");
        assert!(r.skipped.iter().any(|s| s == "modes.json"));
        assert_eq!(crate::modes::load(&d).len(), 1, "用户改过的模式定义不能被覆盖");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_existing_models() {
        let d = tmp("hasmodels");
        bootstrap_agent_data(&d).expect("首次初始化应成功");
        std::fs::write(d.join("models.json"), "[{\"id\":\"m\",\"url\":\"http://x\"}]").unwrap();

        let r = bootstrap_agent_data(&d).expect("二次初始化应成功");
        assert!(r.has_models, "已有模型应被识别");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 展开到**任意临时目录**也成立 —— 证明内置技能不是只会写死的那一个路径。
    #[test]
    fn bundled_skills_expand_into_arbitrary_dir() {
        let d = tmp("expand");
        bootstrap_agent_data(&d).expect("初始化应成功");

        let md = std::fs::read_to_string(d.join("skills/dual-token-dashboard/SKILL.md"))
            .expect("SKILL.md 应被释放");
        // 正文里的路径必须指向**这个**临时目录，而不是某个烧死的旧路径
        assert!(
            md.contains(&d.display().to_string()),
            "释放出的 SKILL.md 应引用本次的 data_dir: {}",
            d.display()
        );
        assert!(!md.contains("{{"), "不该残留占位符");

        // 附带的两个文件也要在
        assert!(d.join("skills/dual-token-dashboard/scripts/gen_dual_dashboard.py").is_file());
        assert!(d.join("skills/dual-token-dashboard/panel/index.html").is_file());
        assert!(d.join("skills/dual-token-dashboard/panel/panel.css").is_file());
        assert!(d.join("skills/dual-token-dashboard/panel/panel.js").is_file());
        assert!(d.join("skills/dual-token-dashboard/references/adapter-schema.md").is_file());

        let _ = std::fs::remove_dir_all(&d);
    }
}
