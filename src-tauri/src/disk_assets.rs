//! 前端资源外置：优先从磁盘 `dist/` 读，读不到回退编译期嵌入的那份。
//!
//! ## 为什么要有这个
//!
//! Tauri 默认把 `build.frontendDist` 指向的目录**嵌进二进制**，而且
//! `tauri-build` 的 build.rs 会监听该目录 —— 所以**改一行前端也要重编
//! `orbcat` crate + 重链**（实测约 57s）。
//!
//! 把 `frontendDist` 改指向 `stub-dist/`（内容恒定的占位目录）后，真实的
//! `dist/` 不再被 build.rs 监听 → **改前端只跑 `npm run build`（10~20s），
//! Rust 完全不动**。
//!
//! ## 用的是官方 API
//!
//! `Context::set_assets(&mut self, Box<dyn Assets>) -> Box<dyn Assets>`
//! （tauri 2.11 起；`Context.assets` 字段本身也是 `pub`）。
//! 页面仍从 `tauri://localhost` 加载，**窗口 URL 与 IPC 机制完全不变** ——
//! 这也是不用自定义 URI scheme 的原因（那样在 Windows 上 origin 会变成
//! `http://<scheme>.localhost`，有被 capability 拦掉的风险）。
//!
//! ## 交付形态变化
//!
//! exe 不再自包含：必须与 `dist/` 目录同目录分发。`tools/build-release.sh`
//! 已把 `cp -r dist` 焊进流程。

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use tauri::utils::assets::{AssetKey, AssetsIter, CspHash};
use tauri::{Assets, Runtime};

/// 空资源集，用于「换出」嵌入资源时的临时占位。
pub struct NoopAssets;

impl<R: Runtime> Assets<R> for NoopAssets {
    fn get(&self, _key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        None
    }
    fn iter(&self) -> Box<AssetsIter<'_>> {
        Box::new(std::iter::empty())
    }
    fn csp_hashes(&self, _html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

/// 先查磁盘 `root`，miss 时回退 `fallback`（编译期嵌入资源）。
pub struct DiskAssets<R: Runtime> {
    root: PathBuf,
    fallback: Box<dyn Assets<R>>,
}

impl<R: Runtime> DiskAssets<R> {
    pub fn new(root: PathBuf, fallback: Box<dyn Assets<R>>) -> Self {
        Self { root, fallback }
    }

    /// 把资源 key 映射到 `root` 下的路径。
    ///
    /// 逐段校验，拒绝空段 / `.` / `..` / 绝对路径 —— 防目录穿越
    /// （虽然 dist 是我们自己的产物，但防御性检查成本为零）。
    fn resolve(&self, key: &AssetKey) -> Option<PathBuf> {
        let rel = key.as_ref().trim_start_matches('/');
        if rel.is_empty() {
            return None;
        }
        let mut out = self.root.clone();
        for seg in rel.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return None;
            }
            out.push(seg);
        }
        Some(out)
    }
}

impl<R: Runtime> Assets<R> for DiskAssets<R> {
    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        if let Some(path) = self.resolve(key) {
            if let Ok(data) = std::fs::read(&path) {
                return Some(Cow::Owned(data));
            }
        }
        self.fallback.get(key)
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        collect(&self.root, &self.root, &mut files);
        if files.is_empty() {
            return self.fallback.iter();
        }
        Box::new(
            files
                .into_iter()
                .map(|(k, v)| -> (Cow<'_, str>, Cow<'_, [u8]>) { (Cow::Owned(k), Cow::Owned(v)) }),
        )
    }

    fn csp_hashes(&self, html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        // 本项目 `tauri.conf.json` 的 `csp` 为 null，无需 nonce 哈希 ——
        // 沿用 fallback 行为即可。
        self.fallback.csp_hashes(html_path)
    }
}

/// 递归收集目录下所有文件，key 用 `/` 分隔（与 `AssetKey` 的归一化一致）。
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let key = rel.to_string_lossy().replace('\\', "/");
            if let Ok(data) = std::fs::read(&path) {
                out.push((key, data));
            }
        }
    }
}

/// 解析前端资源目录。
///
/// 1. 发布形态：exe 同目录的 `dist/`
/// 2. dev 构建：仓库根的 `dist/`（`cargo run` 时 exe 在 target/ 里，旁边没有 dist）
///
/// 都找不到就返回 `None` → 走编译期嵌入资源（即 `stub-dist` 占位页）。
///
/// 结果缓存进 `DIST_DIR`，供 `dist_dir_status()` 写诊断日志用。
pub fn resolve_dist_dir() -> Option<PathBuf> {
    DIST_DIR.get_or_init(compute_dist_dir).clone()
}

static DIST_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// 供启动时写 `diag.log` 的一句话状态，便于生产环境排查「为什么显示占位页」。
pub fn dist_dir_status() -> String {
    match resolve_dist_dir() {
        Some(p) => format!("前端资源: 外置磁盘 {}", p.display()),
        None => "前端资源: 未找到外部 dist/，回退内置占位页".to_string(),
    }
}

fn compute_dist_dir() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let d = dir.join("dist");
            if d.join("index.html").is_file() {
                return Some(d);
            }
        }
    }
    #[cfg(debug_assertions)]
    if let Some(repo) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() {
        let d = repo.join("dist");
        if d.join("index.html").is_file() {
            return Some(d);
        }
    }
    None
}
