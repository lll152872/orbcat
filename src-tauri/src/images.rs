//! 图片资产 —— 统一落盘 + 按模型能力递送
//!
//! ## 为什么图片必须外置
//! 早先图片以 `data:image/png;base64,...` 直接进内存和会话 JSON：
//! - 一张 200KB 截图 base64 后 ≈ 270KB 文本，几十张就让会话文件爆到几十 MB
//! - 每次落盘都要把这坨 base64 重新序列化写一遍
//! - 非多模态模型收到 base64 要么报 400、要么当文本 token 吃进去乱答
//!
//! 所以统一成：**一律落盘 `agent-data/images/<hash>.<ext>`（内容哈希去重），
//! 会话里只存路径**；发给模型时再按能力转换：
//! - 支持图片 → 读文件转 base64 data URL（多部分 content）
//! - 不支持   → 只发路径占位文本（模型知道"有图但看不到"，不会瞎编）
//!
//! ## 兼容
//! 旧会话里遗留的 data URL 图片由 `sessions.rs` 在读取时清掉（用户确认过没用）。

use std::path::{Path, PathBuf};

use base64::Engine;

/// 单张图转 base64 发给模型的大小上限（超了就只发占位，防止把上下文撑爆）
const MAX_INLINE_BYTES: usize = 6 * 1024 * 1024;

/// 图片资产目录
pub fn images_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("images")
}

/// 是不是 data URL（`data:image/png;base64,...`）
pub fn is_data_url(s: &str) -> bool {
    s.starts_with("data:")
}

/// 路径看起来是图片文件吗（按扩展名；不读内容）。
/// `view_image` / `read_file` 分流用。
pub fn is_image_path(path: &Path) -> bool {
    matches!(
        path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default()
            .as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
    )
}

/// 解析 data URL → (mime, 原始字节)
pub fn parse_data_url(u: &str) -> Option<(String, Vec<u8>)> {
    let rest = u.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(',')?;
    if !meta.contains("base64") {
        return None;
    }
    let mime = meta.split(';').next().unwrap_or("image/png").to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    Some((mime, bytes))
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

fn mime_for_ext(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// 内容哈希（非加密用途，只用于去重；不引 sha2 保持零依赖风格）
fn content_hash(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    // 再掺个长度，降低哈希碰撞的概率
    bytes.len().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 存字节到 `images/<hash>.<ext>`，返回**绝对路径**。同内容直接复用已有文件。
pub fn save_bytes(data_dir: &Path, bytes: &[u8], ext: &str) -> Result<String, String> {
    let dir = images_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建图片目录失败: {e}"))?;
    let path = dir.join(format!("{}.{}", content_hash(bytes), ext));
    if !path.exists() {
        std::fs::write(&path, bytes).map_err(|e| format!("写图片失败: {e}"))?;
    }
    Ok(path.to_string_lossy().into_owned())
}

/// 存 data URL 到磁盘，返回绝对路径。非 data URL（已经是路径）原样返回。
pub fn save_data_url(data_dir: &Path, maybe_url: &str) -> Result<String, String> {
    if !is_data_url(maybe_url) {
        return Ok(maybe_url.to_string());
    }
    let (mime, bytes) = parse_data_url(maybe_url).ok_or("无法解析 data URL")?;
    save_bytes(data_dir, &bytes, ext_for_mime(&mime))
}

/// 批量落盘：data URL 转路径，路径原样保留（去重由哈希保证）
pub fn save_all(data_dir: &Path, items: &[String]) -> Vec<String> {
    items
        .iter()
        .filter_map(|u| match save_data_url(data_dir, u) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("[float-agent] 图片落盘失败（已跳过）: {e}");
                None
            }
        })
        .collect()
}

/// 路径 → data URL（发给支持视觉的模型）
pub fn to_data_url(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let bytes = std::fs::read(p).map_err(|e| format!("读图失败（{path}）: {e}"))?;
    if bytes.len() > MAX_INLINE_BYTES {
        return Err(format!(
            "图片过大（{:.1} MB），超过 {} MB 内联上限",
            bytes.len() as f64 / 1024.0 / 1024.0,
            MAX_INLINE_BYTES / 1024 / 1024
        ));
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!(
        "data:{};base64,{b64}",
        mime_for_ext(p)
    ))
}

/// 非多模态模型的占位说明：告诉模型「有图、在哪儿、你看不到」
pub fn placeholder_note(paths: &[String], data_dir: &Path) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let list = paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            // 相对 agent-data 的短路径更好读，绝对路径信息也不丢
            let rel = Path::new(p)
                .strip_prefix(data_dir)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| p.clone());
            format!("  [图片{}] {}", i + 1, rel)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\n\n（用户附了 {} 张图片，但**当前模型不支持图像输入**，只能给到文件路径：\n{}\n\
         需要看图内容时，明确告诉用户「当前模型不支持图片，请到设置里换一个支持图片的模型」，\
         不要凭空猜测图里的内容。）",
        paths.len(),
        list
    )
}

/// 缩略图 data URL —— 前端渲染历史消息里的图片用。
///
/// 为什么走命令而不是 asset protocol：省掉 tauri.conf 的 scope 配置与权限坑，
/// 且缩到 320px 后单张只有几十 KB，IPC 传输无压力。
pub fn thumb_data_url(path: &str, max_edge: u32) -> Result<String, String> {
    let img = image::open(path).map_err(|e| format!("读图失败: {e}"))?;
    let thumb = img.thumbnail(max_edge, max_edge);
    let mut buf = std::io::Cursor::new(Vec::new());
    // JPEG 不支持 alpha：先转 RGB 再编码，否则 RGBA 图会编码失败
    image::DynamicImage::ImageRgb8(thumb.to_rgb8())
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| format!("编码缩略图失败: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("float_agent_img_{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 1x1 透明 PNG 的 data URL
    fn tiny_png_data_url() -> String {
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=".to_string()
    }

    #[test]
    fn data_url_roundtrip_and_dedup() {
        let d = tmp("round");
        let url = tiny_png_data_url();
        let p1 = save_data_url(&d, &url).unwrap();
        assert!(p1.ends_with(".png"), "应按 mime 定扩展名: {p1}");
        assert!(Path::new(&p1).exists());

        // 同样内容再存 → 复用同一个文件（去重）
        let p2 = save_data_url(&d, &url).unwrap();
        assert_eq!(p1, p2, "同内容应复用同一文件");
        assert_eq!(std::fs::read_dir(images_dir(&d)).unwrap().count(), 1);

        // 读回转 data URL
        let back = to_data_url(&p1).unwrap();
        assert!(back.starts_with("data:image/png;base64,"));

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn already_a_path_passes_through() {
        let d = tmp("path");
        let p = "D:\\some\\where\\a.png".to_string();
        assert_eq!(save_data_url(&d, &p).unwrap(), p);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn placeholder_lists_relative_paths() {
        let d = tmp("ph");
        let sub = d.join("images");
        std::fs::create_dir_all(&sub).unwrap();
        let p = sub.join("abc.png").to_string_lossy().into_owned();
        let note = placeholder_note(&[p], &d);
        assert!(note.contains("不支持图像输入"));
        assert!(note.contains("images/abc.png"), "应给相对路径: {note}");
        assert!(placeholder_note(&[], &d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_data_url_is_rejected() {
        assert!(parse_data_url("data:image/png;base64").is_none());
        assert!(parse_data_url("not-a-url").is_none());
        // 非 base64 编码的 data URL 也不收
        assert!(parse_data_url("data:text/plain,hello").is_none());
    }

    #[test]
    fn thumbnail_shrinks_large_image() {
        let d = tmp("thumb");
        // 造一张 800x600 的图（用平滑渐变，避免噪声让编码体积对比失真）
        let img = image::RgbImage::from_fn(800, 600, |x, y| {
            image::Rgb([(x / 4) as u8, (y / 4) as u8, 128])
        });
        let p = d.join("big.png");
        img.save(&p).unwrap();

        let thumb = thumb_data_url(&p.to_string_lossy(), 320).unwrap();
        assert!(thumb.starts_with("data:image/jpeg;base64,"));

        // 关键校验：解码回来确认真的被缩到了 max_edge 以内（比比字节数可靠）
        let (_, b64) = thumb.split_once(',').unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(
            decoded.width() <= 320 && decoded.height() <= 320,
            "缩略图尺寸应 <= 320，实际 {}x{}",
            decoded.width(),
            decoded.height()
        );
        assert_eq!(
            (decoded.width(), decoded.height()),
            (320, 240),
            "应按比例缩放到 320 宽"
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}
