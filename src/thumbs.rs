//! 缩略图解码：把 MTP 返回的图片字节解码、缩小成 RGBA，供 egui 上传成纹理。

use anyhow::{Context, Result};

/// 解码并缩放后的 RGBA 图像数据（未预乘 alpha）。
pub struct ThumbData {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` 字节，RGBA8。
    pub rgba: Vec<u8>,
}

/// 把任意受支持的图片字节解码成不超过 `max_edge` 像素的缩略图。
///
/// `max_edge` 是长边上限，等比缩放。
pub fn decode_thumbnail(bytes: &[u8], max_edge: u32) -> Result<ThumbData> {
    let img = image::load_from_memory(bytes).context("图片解码失败（可能是 MTP 缩略图不支持该格式）")?;
    let thumb = if img.width().max(img.height()) > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
    let rgba = thumb.to_rgba8();
    Ok(ThumbData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// 根据文件名扩展名推断 MIME 类型。
pub fn mime_from_filename(name: &str) -> String {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "dng" => "image/x-adobe-dng",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// 是否是我们认为的“照片”扩展名。
pub fn is_photo_extension(name: &str) -> bool {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg"
            | "jpe"
            | "jfif"
            | "png"
            | "gif"
            | "webp"
            | "bmp"
            | "tif"
            | "tiff"
            | "heic"
            | "heif"
            | "avif"
            | "dng"
            | "raw"
            | "cr2"
            | "nef"
            | "arw"
            | "orf"
            | "rw2"
    )
}

/// 人类可读的字节数。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}
