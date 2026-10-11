//! 屏幕截图
//!
//! 用 Win32 GDI（`GetDC(NULL)` + `BitBlt`）抓**主屏幕**。
//!
//! ## ⚠️ 关键区分（2026-09-17 踩过坑）
//!
//! | 抓取目标 | GDI BitBlt 能否抓到 |
//! |---|---|
//! | **整个屏幕 / 桌面** | ✅ 能（桌面本身就由 DWM 合成到 GDI 可读的表面） |
//! | **WebView2 / Electron 窗口** | ❌ **抓不到**，会得到全黑 |
//!
//! 后者因为用 **DirectComposition 独立合成层**，必须换
//! Windows.Graphics.Capture 或直接抓全屏再裁剪。
//!
//! 我们这个功能要的正是「看看屏幕上是什么」→ 全屏截图正好合适。

use std::ffi::c_void;

use base64::Engine;

// ---------------------------------------------------------------------------
// Win32 FFI —— 仅 Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
type Hdc = *mut c_void;
#[cfg(windows)]
type Hbitmap = *mut c_void;
#[cfg(windows)]
type Hgdiobj = *mut c_void;

#[cfg(windows)]
const HORZRES: i32 = 8;
#[cfg(windows)]
const VERTRES: i32 = 10;
#[cfg(windows)]
const SRCCOPY: u32 = 0x00CC_0020;
#[cfg(windows)]
const BI_RGB: u32 = 0;
#[cfg(windows)]
const DIB_RGB_COLORS: u32 = 0;

#[cfg(windows)]
#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[cfg(windows)]
#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 3],
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: *mut c_void) -> Hdc;
    fn ReleaseDC(hwnd: *mut c_void, hdc: Hdc) -> i32;
}

#[cfg(windows)]
#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
    fn CreateCompatibleBitmap(hdc: Hdc, w: i32, h: i32) -> Hbitmap;
    fn SelectObject(hdc: Hdc, obj: Hgdiobj) -> Hgdiobj;
    fn BitBlt(dst: Hdc, x: i32, y: i32, w: i32, h: i32, src: Hdc, sx: i32, sy: i32, rop: u32) -> i32;
    fn GetDIBits(
        hdc: Hdc,
        hbm: Hbitmap,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        bmi: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(obj: Hgdiobj) -> i32;
    fn DeleteDC(hdc: Hdc) -> i32;
    fn GetDeviceCaps(hdc: Hdc, index: i32) -> i32;
}

// ---------------------------------------------------------------------------
// 缩放上限
// ---------------------------------------------------------------------------

/// 单边最大像素。超过就等比缩小。
///
/// 理由：2560x1440 的原始截图体积过大（PNG 数 MB，base64 再涨 33%），
/// 请求体会过大且明显变慢；主流视觉模型的有效输入分辨率也在 1500 左右。
const MAX_DIM: u32 = 1568;

/// JPEG 质量。85 在「截图 + 代码文字」场景下肉眼无损，体积约为 PNG 的 1/6。
const JPEG_QUALITY: u8 = 85;

/// 截图编码格式。
///
/// ⚠️ 目前**只有 JPEG 一种**（2026-09-30 清 dead-code 时删掉了 `Png` 变体）。
/// 理由：整个项目没有任何地方构造过 Png —— 它只在 match 臂里出现，
/// 属于"看着像可选项、实际走不到"的死分支。截图一律走 JPEG，
/// 因为 2560x1440 的原始 PNG 是数 MB 级，base64 再涨 33%，而 JPEG(q85)
/// 在「截图 + 代码文字」场景下肉眼无损、体积约为 PNG 的 1/6（见 JPEG_QUALITY）。
///
/// 保留 enum 而不是直接换成常量，是为了让"将来要加 WebP/PNG"时改一处就够。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotFormat {
    Jpeg,
}

impl ShotFormat {
    pub fn mime(&self) -> &'static str {
        match self {
            ShotFormat::Jpeg => "image/jpeg",
        }
    }
    pub fn ext(&self) -> &'static str {
        match self {
            ShotFormat::Jpeg => "jpg",
        }
    }
}

// ---------------------------------------------------------------------------
// 截图
// ---------------------------------------------------------------------------

/// 抓主屏幕，返回 RGBA 图像
///
/// ## 平台支持
///
/// 只有 Windows 走 GDI `BitBlt`。macOS 需要 `ScreenCaptureKit` 且必须用户授予
/// **屏幕录制**权限（比辅助功能权限更敏感，弹窗文案更重），第一版不接 ——
/// 见 `platform/macos.rs` 的说明。其余平台同理。
#[cfg(not(windows))]
pub fn capture_primary_screen() -> Result<image::RgbaImage, String> {
    Err("屏幕截图暂不支持当前平台（Windows 走 GDI；macOS 需 ScreenCaptureKit + 录屏授权，尚未接入）".to_string())
}

#[cfg(windows)]
pub fn capture_primary_screen() -> Result<image::RgbaImage, String> {
    unsafe {
        let hdc_screen = GetDC(std::ptr::null_mut());
        if hdc_screen.is_null() {
            return Err("GetDC(NULL) 失败".into());
        }

        // 用 GetDeviceCaps 而非 GetSystemMetrics —— 后者受 DPI 虚拟化影响会偏小
        let width = GetDeviceCaps(hdc_screen, HORZRES);
        let height = GetDeviceCaps(hdc_screen, VERTRES);

        if width <= 0 || height <= 0 {
            ReleaseDC(std::ptr::null_mut(), hdc_screen);
            return Err(format!("屏幕尺寸异常：{width}x{height}"));
        }

        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let hbmp = CreateCompatibleBitmap(hdc_screen, width, height);
        if hdc_mem.is_null() || hbmp.is_null() {
            ReleaseDC(std::ptr::null_mut(), hdc_screen);
            return Err("创建兼容 DC/Bitmap 失败".into());
        }

        let old = SelectObject(hdc_mem, hbmp);

        let ok = BitBlt(
            hdc_mem, 0, 0, width, height,
            hdc_screen, 0, 0, SRCCOPY,
        );

        let mut bmi = BitmapInfo {
            bmi_header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: width,
                bi_height: -height, // 负数 = top-down，省一次翻转
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: BI_RGB,
                bi_size_image: 0,
                bi_x_pels_per_meter: 0,
                bi_y_pels_per_meter: 0,
                bi_clr_used: 0,
                bi_clr_important: 0,
            },
            bmi_colors: [0; 3],
        };

        let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
        let got = GetDIBits(
            hdc_mem,
            hbmp,
            0,
            height as u32,
            buf.as_mut_ptr() as *mut c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );

        // --- 清理 GDI 资源（顺序：先还原，再删）---
        SelectObject(hdc_mem, old);
        DeleteObject(hbmp);
        DeleteDC(hdc_mem);
        ReleaseDC(std::ptr::null_mut(), hdc_screen);

        if ok == 0 {
            return Err("BitBlt 失败".into());
        }
        if got == 0 {
            return Err("GetDIBits 失败".into());
        }

        // BGRA → RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255; // 有些驱动给的 alpha 是 0，强制不透明
        }

        let mut img = image::RgbaImage::from_raw(width as u32, height as u32, buf)
            .ok_or_else(|| "构造图像失败".to_string())?;

        // --- 超过上限就等比缩小 ---
        if img.width() > MAX_DIM || img.height() > MAX_DIM {
            let (nw, nh) = fit_within(img.width(), img.height(), MAX_DIM);
            img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3);
        }

        Ok(img)
    }
}

/// 等比缩放到不超过 `max` 的尺寸
fn fit_within(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w <= max && h <= max {
        return (w, h);
    }
    let ratio = (max as f64) / (w.max(h) as f64);
    (
        ((w as f64 * ratio).round() as u32).max(1),
        ((h as f64 * ratio).round() as u32).max(1),
    )
}

/// 把图像编码成字节
///
/// ⚠️ 匹配臂只剩 `Jpeg`（`Png` 分支已删，理由见 [`ShotFormat`]）。
/// `match` 保留单臂写法，是为了将来加格式时不必改调用方。
pub fn encode(img: &image::RgbaImage, fmt: ShotFormat) -> Result<Vec<u8>, String> {
    match fmt {
        ShotFormat::Jpeg => {
            // JPEG 不支持 alpha，先转 RGB
            let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
            use image::codecs::jpeg::JpegEncoder;
            let mut buf = Vec::with_capacity(256 * 1024);
            let mut enc = JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
            enc.encode_image(&rgb)
                .map_err(|e| format!("JPEG 编码失败：{e}"))?;
            Ok(buf)
        }
    }
}

/// 抓主屏幕并编码为 `data:<mime>;base64,...`（可直接塞进 OpenAI 的 image_url）
/// 截图 → data URL（**目前无调用方**：图片已统一走「落盘 + 按模型能力递送」，
/// 留在这里只是备用；真要发 base64 也从文件转，不必再截一次屏）
#[allow(dead_code)]
pub fn capture_screen_data_url(fmt: ShotFormat) -> Result<String, String> {
    let img = capture_primary_screen()?;
    let bytes = encode(&img, fmt)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{};base64,{b64}", fmt.mime()))
}

/// 供工具层用：截图存到目录并返回路径与尺寸
pub fn capture_screen_to_file(
    dir: &std::path::Path,
    fmt: ShotFormat,
) -> Result<(std::path::PathBuf, u32, u32), String> {
    let img = capture_primary_screen()?;
    let (w, h) = (img.width(), img.height());
    let bytes = encode(&img, fmt)?;

    std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("screen_{stamp}.{}", fmt.ext()));

    std::fs::write(&path, bytes).map_err(|e| format!("保存截图失败：{e}"))?;
    Ok((path, w, h))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_keeps_small_unchanged() {
        assert_eq!(fit_within(800, 600, 1568), (800, 600));
    }

    #[test]
    fn fit_within_scales_large() {
        let (w, h) = fit_within(2560, 1440, 1568);
        assert_eq!(w, 1568);
        assert!(h < 1440 && h > 800, "高度应等比缩小，实际 {h}");
    }

    #[test]
    fn fit_within_handles_portrait() {
        let (w, h) = fit_within(1080, 1920, 1000);
        assert_eq!(h, 1000);
        assert!(w < 1080);
    }
}
