//! 前台应用上下文采集 —— 「用户现在正在看什么」
//!
//! ## 为什么不硬啃 UIA 树
//! 验证报告（docs/01-UIA-FINDINGS.md）结论：Electron 应用的 UIA 树
//! 激活不稳定（VSCode 六轮实验只有 R4 成功）。但退一步，我们要的其实只是
//! **「哪个应用在前台 + 它在打开哪个目录/文件」**，这两样都有廉价可靠的来源：
//!
//! | 信息 | 来源 | 可靠性 |
//! |---|---|---|
//! | 前台进程 + 窗口标题 | Win32 `GetForegroundWindow` 等（见 win32.rs） | ✅ 内核级属性 |
//! | VSCode 打开的目录 | `%APPDATA%\Code\User\globalStorage\storage.json` 的 `windowsState.lastActiveWindow.folder` | ✅ VSCode 自己维护 |
//! | Typora 打开的文件 | 窗口标题的文件名 + `%APPDATA%\abnerworks\Typora\workspace-info.json` 反查全路径 | ✅ |
//!
//! 采集失败一律降级（只报应用名），绝不阻塞对话主流程。
//!
//! ## 隐私边界
//! 只读「前台窗口」这一条信息，不扫屏、不读内容、不进权限网关管辖范围
//! （它是程序自身的感知能力，不是模型的文件操作 —— 模型主动查屏幕仍走工具+审批）。

use serde::Serialize;
use std::sync::{Mutex, OnceLock};

/// 后台轮询缓存的「最近几个非自身的前台应用」（新的在前，去重）。
///
/// 为什么需要缓存：面板展开时**自己就是前台窗口**，chat 时现采只会采到
/// float-agent 自己。所以起一个轮询线程，持续记录「不是自己的前台」，
/// 对话时取这份缓存 —— 那才是用户真正在用的应用。
///
/// 为什么是列表而不是单值：用户往往在多个应用间来回切（VSCode 写码 →
/// Typora 查笔记 → 浏览器查文档），只留最后一个信息量太少。保留一小段
/// 历史，面板上能一眼看到「刚才都在看什么」。
static LAST_OTHER: OnceLock<Mutex<Vec<ForegroundContext>>> = OnceLock::new();

/// 历史保留条数
const HISTORY_MAX: usize = 6;

fn last_other() -> &'static Mutex<Vec<ForegroundContext>> {
    LAST_OTHER.get_or_init(|| Mutex::new(Vec::new()))
}

/// 启动轮询线程（setup 里调一次）。每 800ms 采一次前台，非自身则记入历史。
///
/// ⚠️ 每一轮都套 catch_unwind：单个采样轮 panic 不能让整个线程静默死掉
/// （线程死掉的表现就是「指示条突然永远空白」，且没有任何报错可查）。
pub fn spawn_watcher() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(800));
        let round = std::panic::catch_unwind(current_excluding_self);
        let Ok(Some(ctx)) = round else {
            if round.is_err() {
                eprintln!("[float-agent] 前台采样单轮 panic，已忽略（线程继续）");
            }
            continue;
        };
        if let Ok(mut g) = last_other().lock() {
            // 去重：同应用 + 同目录/文件算同一条（标题会因编辑状态抖动，
            // 比如 Typora 未保存时带 *），命中就删旧的、新的插到最前
            g.retain(|x| !same_place(x, &ctx));
            g.insert(0, ctx);
            g.truncate(HISTORY_MAX);
        }
    });
}

/// 是否同一个「位置」：应用相同，且目录/文件相同；都没解析出路径时退化为比标题。
fn same_place(a: &ForegroundContext, b: &ForegroundContext) -> bool {
    if a.app != b.app {
        return false;
    }
    if a.dir.is_none() && a.file.is_none() && b.dir.is_none() && b.file.is_none() {
        return a.title == b.title;
    }
    a.dir == b.dir && a.file == b.file
}

/// 取缓存的「用户最近在看的应用」（面板在前台时用这个）
pub fn cached() -> Option<ForegroundContext> {
    last_other().lock().ok().and_then(|g| g.first().cloned())
}

/// 最近的非自身前台应用（新的在前，最多 6 条）——面板展示用
pub fn recent_foregrounds() -> Vec<ForegroundContext> {
    last_other().lock().map(|g| g.clone()).unwrap_or_default()
}

/// 采集，但如果前台就是 float-agent 自己则返回 None
pub fn current_excluding_self() -> Option<ForegroundContext> {
    let ctx = current()?;
    let self_stem = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if ctx.app.to_lowercase().contains("float-agent")
        || ctx.app.to_lowercase() == self_stem
    {
        return None;
    }
    Some(ctx)
}

/// 一次采集的结果
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForegroundContext {
    /// 应用名（`VSCode` / `Typora` / `explorer` / …）
    pub app: String,
    /// 窗口标题原文
    pub title: String,
    /// 解析出的「正在看的目录」（能拿到才有）
    pub dir: Option<String>,
    /// 解析出的「正在看的文件」（能拿到才有）
    pub file: Option<String>,
}

impl ForegroundContext {
    /// 给 system prompt / 工具返回用的一行摘要
    pub fn summary(&self) -> String {
        let mut s = format!("前台应用：{}（标题「{}」）", self.app, self.title);
        if let Some(d) = &self.dir {
            s.push_str(&format!("，正在浏览目录 {d}"));
        }
        if let Some(f) = &self.file {
            s.push_str(&format!("，正在打开文件 {f}"));
        }
        s
    }
}

/// 目录/文件反查的结果缓存：`key = "{app}|{title}"`，5 秒 TTL。
///
/// 为什么需要：VSCode 的 workspaceStorage 反查、Typora 的 Recent/*.lnk
/// 全量扫描都是重 IO（几百个文件），而 watcher 每 800ms 轮询一次——
/// 不缓存就是每秒 1+ 次全量磁盘扫，纯烧 CPU。同一标题下结果几乎不变，
/// 5 秒足够新。
static RESOLVE_CACHE: OnceLock<Mutex<Option<(String, std::time::Instant, (Option<String>, Option<String>))>>> =
    OnceLock::new();

fn resolve_cache() -> &'static Mutex<Option<(String, std::time::Instant, (Option<String>, Option<String>))>> {
    RESOLVE_CACHE.get_or_init(|| Mutex::new(None))
}

const RESOLVE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

fn resolve_dir_file(app: &str, title: &str) -> (Option<String>, Option<String>) {
    let key = format!("{app}|{title}");
    if let Ok(g) = resolve_cache().lock() {
        if let Some((k, at, hit)) = g.as_ref() {
            if *k == key && at.elapsed() < RESOLVE_TTL {
                return hit.clone();
            }
        }
    }
    let result = match app {
        "VSCode" => vscode_workspace(title),
        "Typora" => typora_file(title),
        _ => (None, None),
    };
    if let Ok(mut g) = resolve_cache().lock() {
        *g = Some((key, std::time::Instant::now(), result.clone()));
    }
    result
}

/// 采集当前前台上下文（非 Windows / 拿不到时 None）
pub fn current() -> Option<ForegroundContext> {
    #[cfg(windows)]
    {
        let (exe, title) = crate::win32::foreground_window_info()?;
        let stem = std::path::Path::new(&exe)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        let app = match stem.as_str() {
            "code" | "code-insiders" | "codium" => "VSCode",
            "typora" => "Typora",
            "idea64" | "idea" => "IntelliJ IDEA",
            "explorer" => "文件资源管理器",
            other => other,
        };

        let (dir, file) = resolve_dir_file(&app, &title);

        Some(ForegroundContext {
            app: app.to_string(),
            title,
            dir,
            file,
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// VSCode
// ---------------------------------------------------------------------------

/// VSCode 的 workspace / 文件定位。
///
/// 顺序：
///   1. storage.json 的 `windowsState.lastActiveWindow.folder`（VSCode 自己记的，最准）
///   2. 标题解析：`文件 - 目录 - Visual Studio Code`；目录段若是裸名字，
///      再去 workspaceStorage/*/workspace.json 反查全路径
fn vscode_workspace(title: &str) -> (Option<String>, Option<String>) {
    let mut dir = None;
    let mut file = None;

    // 1) storage.json —— lastActiveWindow.folder
    if let Some(root) = appdata_dir() {
        let p = root
            .join("Code")
            .join("User")
            .join("globalStorage")
            .join("storage.json");
        if let Ok(txt) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let law = v.get("windowsState").and_then(|x| x.get("lastActiveWindow"));
                if let Some(folder_uri) = law.and_then(|x| x.get("folder")).and_then(|x| x.as_str()) {
                    dir = uri_to_path(folder_uri);
                }
                if let Some(f) = law
                    .and_then(|x| x.get("lastFileUri"))
                    .and_then(|x| x.as_str())
                {
                    file = uri_to_path(f);
                }
            }
        }
    }

    // 2) 标题兜底：`a.rs — proj — Visual Studio Code`（分隔符 em dash 或 -）
    if dir.is_none() {
        let split_once = |t: &str, sep: &str| -> Vec<String> {
            t.split(sep).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        };
        let mut parts = split_once(title, " — ");
        if parts.len() < 2 {
            parts = split_once(title, " - ");
        }
        parts.retain(|p| !p.contains("Visual Studio Code"));

        if let Some(l) = parts.last().cloned() {
            if parts.len() >= 2 {
                let f = &parts[parts.len() - 2];
                if !looks_like_abs_path(f) {
                    file = Some(f.clone());
                }
            }
            // 形如 `D:\workplace\xxx` 的段直接当全路径；裸名字去 workspaceStorage 反查
            dir = if looks_like_abs_path(&l) {
                Some(l)
            } else {
                resolve_vscode_folder(&l).or(Some(l))
            };
        }
    }

    (dir, file)
}

fn looks_like_abs_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// 用 workspaceStorage 反查「裸目录名 → 全路径」。
/// 命中多个时取 mtime 最新的（就是当前在用那个）。
fn resolve_vscode_folder(name: &str) -> Option<String> {
    let root = appdata_dir()?;
    let ws_dir = root.join("Code").join("User").join("workspaceStorage");
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let entries = std::fs::read_dir(&ws_dir).ok()?;
    for e in entries.flatten() {
        let wp = e.path().join("workspace.json");
        let Ok(txt) = std::fs::read_to_string(&wp) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
        let Some(uri) = v.get("folder").and_then(|x| x.as_str()) else { continue };
        let Some(full) = uri_to_path(uri) else { continue };
        if !full.ends_with(name) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let Ok(mtime) = md.modified() else { continue };
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, full));
        }
    }
    best.map(|(_, p)| p)
}

/// `file:///d%3A/workplace/x` → `d:\workplace\x`
fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file:///")?;
    let decoded = percent_decode(rest);
    // Windows: d:/path → d:\path；去掉开头的斜杠
    let s = decoded.replace('/', "\\");
    let s = s.trim_start_matches('\\').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Typora
// ---------------------------------------------------------------------------

/// Typora 标题是 `文件名.md - Typora`（无全路径）。
/// 反查来源：`%APPDATA%\Microsoft\Windows\Recent\*.lnk` —— 资源管理器为每个
/// 打开过的文件写的快捷方式，文件名就是 `xxx.md`，目标路径手解 MS-SHLLINK
/// （win32::lnk_target）拿全路径。注册表 RecentDocs 路子不通（值名是 MRU
/// 数字 id、数据是相对 PIDL，SHGetPathFromIDListW 解不出来），已弃用。
fn typora_file(title: &str) -> (Option<String>, Option<String>) {
    let name = title
        .trim_end_matches(" - Typora")
        .trim_end_matches(" — Typora")
        .trim()
        .to_string();
    if name.is_empty() || name == "Typora" {
        return (None, None);
    }

    #[cfg(windows)]
    {
        if let Some(full) = recent_lnk_target(&name) {
            let parent = std::path::Path::new(&full)
                .parent()
                .map(|p| p.to_string_lossy().into_owned());
            return (parent, Some(full));
        }
    }

    // 反查不到全路径：至少把标题里的文件名给出去
    (None, Some(name))
}

/// 在「最近使用」里找文件名 == `name` 的 .lnk，解出目标完整路径。
/// 同名多条时取修改时间最新的（最近打开的那个）。
#[cfg(windows)]
fn recent_lnk_target(name: &str) -> Option<String> {
    let root = std::path::PathBuf::from(std::env::var("APPDATA").ok()?)
        .join("Microsoft")
        .join("Windows")
        .join("Recent");
    let entries = std::fs::read_dir(&root).ok()?;
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("lnk") {
            continue;
        }
        // .lnk 的文件名（去扩展名）应等于目标文件名
        if p.file_stem().and_then(|x| x.to_str()) != Some(name) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let Some(target) = crate::win32::lnk_target(&bytes) else { continue };
        let Ok(md) = std::fs::metadata(&p) else { continue };
        let Ok(mtime) = md.modified() else { continue };
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, target));
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn appdata_dir() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_decodes_windows_path() {
        assert_eq!(
            uri_to_path("file:///d%3A/workplace/%E6%82%AC%E6%B5%AEx").as_deref(),
            Some("d:\\workplace\\悬浮x")
        );
        assert_eq!(uri_to_path("https://x"), None);
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
    }

    #[test]
    fn current_does_not_panic() {
        // 无前台窗口也不应 panic（CI/服务环境）
        let _ = current();
    }

    /// 本机诊断：打印当前前台与缓存。手动跑：
    /// `cargo test --lib -- --ignored --nocapture dump_foreground`
    #[test]
    #[ignore = "本机诊断用"]
    fn dump_foreground() {
        println!("current = {:#?}", current());
        spawn_watcher();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        println!("cached  = {:#?}", cached());
        println!("recent  = {:#?}", recent_foregrounds());
    }

    #[test]
    fn same_place_dedup_rules() {
        let mk = |app: &str, title: &str, dir: Option<&str>, file: Option<&str>| ForegroundContext {
            app: app.into(),
            title: title.into(),
            dir: dir.map(str::to_string),
            file: file.map(str::to_string),
        };
        // 同应用同文件、标题抖动（Typora 未保存带 *）→ 视作同一处
        assert!(same_place(
            &mk("Typora", "a.md - Typora", None, Some("D:\\x\\a.md")),
            &mk("Typora", "* a.md - Typora", None, Some("D:\\x\\a.md")),
        ));
        // 不同文件 → 不是同一处
        assert!(!same_place(
            &mk("Typora", "a.md", None, Some("D:\\x\\a.md")),
            &mk("Typora", "b.md", None, Some("D:\\x\\b.md")),
        ));
        // 路径都拿不到时退化为比标题
        assert!(same_place(
            &mk("Chrome", "百度一下", None, None),
            &mk("Chrome", "百度一下", None, None),
        ));
        assert!(!same_place(
            &mk("Chrome", "百度一下", None, None),
            &mk("Chrome", "知乎", None, None),
        ));
        // 不同应用 → 不是同一处
        assert!(!same_place(
            &mk("VSCode", "t", Some("D:\\p"), None),
            &mk("Typora", "t", Some("D:\\p"), None),
        ));
    }

    /// 本机实测：Recent 里必须能反解出至少一条 .lnk 的完整目标路径。
    /// 只在 Windows 桌面会话下有意义的冒烟（CI 上 Recent 可能为空则跳过）。
    #[test]
    #[cfg(windows)]
    fn recent_lnk_smoke() {
        let root = match std::env::var("APPDATA") {
            Ok(a) => std::path::PathBuf::from(a)
                .join("Microsoft")
                .join("Windows")
                .join("Recent"),
            Err(_) => return,
        };
        let Ok(entries) = std::fs::read_dir(&root) else { return };
        let mut parsed = 0;
        for e in entries.flatten().take(60) {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("lnk") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            if let Some(t) = crate::win32::lnk_target(&bytes) {
                assert!(looks_like_abs_path(&t), "lnk 解出的不是绝对路径: {t}");
                parsed += 1;
            }
        }
        // 本机 Recent 有内容时至少要解出一条；全空环境不强制
        if std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0) > 10 {
            assert!(parsed > 0, "Recent 有文件但一条 .lnk 都没解析出来");
        }
    }

    /// 本机实测：typora_file 对真实打开过的文件应能拿到全路径。
    /// 用 check_lnk.py 验证过的样本 `redis使用.md`（若本机 Recent 无此条则跳过）。
    #[test]
    #[cfg(windows)]
    fn typora_file_resolves_via_recent() {
        let probe = std::path::PathBuf::from(std::env::var("APPDATA").unwrap())
            .join("Microsoft")
            .join("Windows")
            .join("Recent")
            .join("redis使用.md.lnk");
        if !probe.exists() {
            return;
        }
        let (dir, file) = typora_file("redis使用.md - Typora");
        let f = file.expect("应反查到全路径");
        assert!(f.ends_with("redis使用.md"), "路径异常: {f}");
        assert!(dir.is_some(), "应有父目录");
    }
}
