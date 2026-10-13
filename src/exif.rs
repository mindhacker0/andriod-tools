//! EXIF 元数据：从照片字节里解析出来，用于界面展示和随上传一起发给服务器。
//!
//! 解析用 [`kamadak_exif`]，它支持 JPEG / TIFF / PNG(eXIf) / WebP / HEIC 容器。
//! 注意：EXIF 一般在文件最前面（JPEG 的 APP1），所以优先只读文件头；
//! HEIC 的 `meta` box 可能在后面，读头失败时会退化成整张下载。

use std::collections::BTreeMap;
use std::io::Cursor;

use anyhow::{Context, Result};
use serde::Serialize;

/// 界面上重点展示的字段（键是 EXIF 标签名）。
pub const HIGHLIGHT_TAGS: &[&str] = &[
    "Make",
    "Model",
    "LensModel",
    "Software",
    "DateTimeOriginal",
    "DateTimeDigitized",
    "DateTime",
    "OffsetTimeOriginal",
    "SubSecTimeOriginal",
    "ExposureTime",
    "ShutterSpeedValue",
    "FNumber",
    "ApertureValue",
    "ISOSpeedRatings",
    "PhotographicSensitivity",
    "RecommendedExposureIndex",
    "FocalLength",
    "FocalLengthIn35mmFilm",
    "ExposureProgram",
    "ExposureBiasValue",
    "MeteringMode",
    "Flash",
    "WhiteBalance",
    "LightSource",
    "SceneCaptureType",
    "ExposureMode",
    "ColorSpace",
    "Orientation",
    "PixelXDimension",
    "PixelYDimension",
    "Artist",
    "Copyright",
    "ImageDescription",
];

/// 一条 EXIF 字段。
#[derive(Clone, Debug, Serialize)]
pub struct ExifField {
    /// 所在 IFD：`Primary` / `Thumbnail` / `Other(n)`。
    pub ifd: String,
    /// 标签名，例如 `ExposureTime`。
    pub tag: String,
    /// 标签编号（MTP/EXIF 规范里的 tag id）。
    pub id: u16,
    /// 便于阅读的显示值（带单位）。
    pub value: String,
}

/// GPS 坐标。
#[derive(Clone, Debug, Serialize)]
pub struct GpsPosition {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: Option<f64>,
}

/// 解析出的全部 EXIF 信息。
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExifData {
    /// 字段总条数。
    pub field_count: usize,
    /// 关键字段摘要（相机、时间、曝光参数…），键是标签名。
    pub highlights: BTreeMap<String, String>,
    /// 全部字段，原样保留，便于服务端做自己的解析。
    pub fields: Vec<ExifField>,
    /// 拍摄地坐标（有就带）。
    pub gps: Option<GpsPosition>,
}

impl ExifData {
    /// 取一个关键字段的显示值。
    pub fn get(&self, tag: &str) -> Option<&str> {
        self.highlights.get(tag).map(String::as_str)
    }

    /// 相机标识，例如 `Xiaomi 2201122C`。
    pub fn camera(&self) -> Option<String> {
        match (self.get("Make"), self.get("Model")) {
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (None, Some(model)) => Some(model.to_string()),
            (Some(make), None) => Some(make.to_string()),
            (None, None) => None,
        }
    }
}

/// 从文件头字节猜一下容器格式，用于诊断“为什么没有 EXIF”。
pub fn sniff_format(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xFF, 0xD8, ..] => "JPEG",
        [0x89, b'P', b'N', b'G', ..] => "PNG",
        [b'G', b'I', b'F', ..] => "GIF",
        [b'B', b'M', ..] => "BMP",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "WebP",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'A', b'V', b'I', b'F', ..] => "AVIF",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "HEIF/MP4 系",
        [] => "空文件",
        _ => "未知格式",
    }
}

/// 从照片字节里解析 EXIF。没有 EXIF 时返回错误（错误信息可直接展示给用户）。
pub fn parse(bytes: &[u8]) -> Result<ExifData> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .with_context(|| {
            format!(
                "这个 {} 文件里没有 EXIF 数据",
                sniff_format(bytes)
            )
        })?;

    let mut data = ExifData::default();
    let mut raw: BTreeMap<String, exif::Value> = BTreeMap::new();

    for field in exif.fields() {
        let tag = field.tag.to_string();
        // kamadak-exif 给 ASCII 值会包上一层引号，展示时去掉更干净。
        let value = clean_value(field.display_value().with_unit(&exif).to_string());
        if HIGHLIGHT_TAGS.contains(&tag.as_str()) && !is_noise(&value) {
            data.highlights.entry(tag.clone()).or_insert(value.clone());
        }
        data.fields.push(ExifField {
            ifd: ifd_name(&field.ifd_num),
            tag: tag.clone(),
            id: field.tag.number(),
            value,
        });
        raw.insert(tag, field.value.clone());
    }

    if data.fields.is_empty() {
        anyhow::bail!("这个文件里没有 EXIF 数据");
    }
    data.field_count = data.fields.len();
    data.gps = read_gps(&raw);
    Ok(data)
}

/// 去掉 kamadak-exif 给 ASCII 值加的引号。
fn clean_value(value: String) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        value
    }
}

/// 这种值等于没写，不往“关键参数”里放。
fn is_noise(value: &str) -> bool {
    let v = value.trim();
    v.is_empty()
        || v.eq_ignore_ascii_case("unknown")
        || v.eq_ignore_ascii_case("undefined")
        || v.eq_ignore_ascii_case("0 pixels")
        || v.starts_with('[')
}

/// IFD 名称可读化：`In(0)` → `Primary`。
fn ifd_name(ifd: &exif::In) -> String {
    if *ifd == exif::In::PRIMARY {
        "Primary".to_string()
    } else if *ifd == exif::In::THUMBNAIL {
        "Thumbnail".to_string()
    } else {
        format!("{ifd:?}")
    }
}

/// 从原始字段里拼出 GPS 坐标。
fn read_gps(raw: &BTreeMap<String, exif::Value>) -> Option<GpsPosition> {
    let latitude = dms(raw.get("GPSLatitude")?)?;
    let longitude = dms(raw.get("GPSLongitude")?)?;

    let lat_ref = raw.get("GPSLatitudeRef").and_then(|v| ascii_at(v, 0)).unwrap_or("N");
    let lon_ref = raw
        .get("GPSLongitudeRef")
        .and_then(|v| ascii_at(v, 0))
        .unwrap_or("E");

    let lat = if lat_ref.to_ascii_uppercase().starts_with('S') {
        -latitude
    } else {
        latitude
    };
    let lon = if lon_ref.to_ascii_uppercase().starts_with('W') {
        -longitude
    } else {
        longitude
    };

    let altitude = raw.get("GPSAltitude").and_then(rational_to_f64).and_then(|v| {
        // 0 表示设备没测到海拔，当成没有
        if v == 0.0 {
            return None;
        }
        // GPSAltitudeRef: 0 = 海平面以上，1 = 海平面以下
        let below = raw
            .get("GPSAltitudeRef")
            .and_then(|r| uint_at(r, 0))
            .map(|code| code == 1)
            .unwrap_or(false);
        Some(if below { -v } else { v })
    });

    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some(GpsPosition {
        latitude: lat,
        longitude: lon,
        altitude,
    })
}

/// 取 ASCII 字段的第 `index` 个字符串（kamadak-exif 把它存成原始字节）。
fn ascii_at(value: &exif::Value, index: usize) -> Option<&str> {
    match value {
        exif::Value::Ascii(parts) => parts
            .get(index)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(|s| s.trim_end_matches('\0')),
        _ => None,
    }
}

/// 取整数型字段的第 `index` 个值。
fn uint_at(value: &exif::Value, index: usize) -> Option<u32> {
    match value {
        exif::Value::Byte(v) => v.get(index).map(|x| u32::from(*x)),
        exif::Value::Short(v) => v.get(index).map(|x| u32::from(*x)),
        exif::Value::Long(v) => v.get(index).copied(),
        exif::Value::SShort(v) => v.get(index).map(|x| u32::try_from(*x).ok()).flatten(),
        exif::Value::SLong(v) => v.get(index).copied().and_then(|x| u32::try_from(x).ok()),
        _ => None,
    }
}

/// 度分秒 → 十进制度。
fn dms(value: &exif::Value) -> Option<f64> {
    let parts = match value {
        exif::Value::Rational(v) => v,
        exif::Value::SRational(_) => return None,
        _ => return None,
    };
    if parts.len() < 3 {
        return None;
    }
    let to_f64 = |r: &exif::Rational| {
        if r.denom == 0 {
            0.0
        } else {
            f64::from(r.num) / f64::from(r.denom)
        }
    };
    let (deg, min, sec) = (to_f64(&parts[0]), to_f64(&parts[1]), to_f64(&parts[2]));
    Some(deg + min / 60.0 + sec / 3600.0)
}

/// 单个有理数 → f64。
fn rational_to_f64(value: &exif::Value) -> Option<f64> {
    match value {
        exif::Value::Rational(v) => {
            let r = v.first()?;
            if r.denom == 0 {
                None
            } else {
                Some(f64::from(r.num) / f64::from(r.denom))
            }
        }
        exif::Value::SRational(v) => {
            let r = v.first()?;
            if r.denom == 0 {
                None
            } else {
                Some(f64::from(r.num) / f64::from(r.denom))
            }
        }
        _ => None,
    }
}

/// 人话版的曝光参数，例如 `1/100s · f/1.9 · ISO 100 · 6.9mm`。
pub fn exposure_summary(data: &ExifData) -> String {
    let mut parts = Vec::new();
    if let Some(v) = data.get("ExposureTime") {
        parts.push(v.to_string());
    }
    if let Some(v) = data.get("FNumber") {
        parts.push(v.to_string());
    }
    for key in ["ISOSpeedRatings", "PhotographicSensitivity"] {
        if let Some(v) = data.get(key) {
            parts.push(format!("ISO {v}"));
            break;
        }
    }
    if let Some(v) = data.get("FocalLength") {
        parts.push(v.to_string());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼一个最小 JPEG：只有 SOI + APP1(EXIF) + EOI。
    /// 用于离线验证解析与取值清洗，不依赖真机。
    fn minimal_jpeg_with_exif() -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II"); // 小端
        tiff.extend_from_slice(&[0x2A, 0x00]); // 42
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 偏移
        // IFD0：1 个条目
        tiff.extend_from_slice(&1u16.to_le_bytes());
        // Make = 0x010F，类型 2(ASCII)，7 字节，值放在 TIFF 偏移 26
        tiff.extend_from_slice(&0x010Fu16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&7u32.to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes());
        // 没有下一个 IFD
        tiff.extend_from_slice(&0u32.to_le_bytes());
        // 值区（TIFF 偏移 26）
        tiff.extend_from_slice(b"Xiaomi\0");

        let mut payload: Vec<u8> = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let seg_len = (payload.len() + 2) as u16;

        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(&payload);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn parses_make_and_cleans_quotes() {
        let bytes = minimal_jpeg_with_exif();
        let data = parse(&bytes).expect("应当能解析出 EXIF");
        // kamadak-exif 的显示值是带引号的，展示前必须去掉
        assert_eq!(data.get("Make"), Some("Xiaomi"), "引号应当被去掉");
        assert_eq!(data.field_count, 1);
        assert_eq!(data.fields[0].ifd, "Primary", "IFD 名称应当可读");
        assert_eq!(data.fields[0].id, 0x010F);
        assert!(data.gps.is_none());
    }

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff_format(&[0xFF, 0xD8, 0xFF, 0xE0]), "JPEG");
        assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\n"), "PNG");
        assert_eq!(sniff_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "WebP");
        assert_eq!(sniff_format(b""), "空文件");
    }

    #[test]
    fn missing_exif_error_names_the_format() {
        let err = parse(&[0xFF, 0xD8, 0xFF, 0xD9]).unwrap_err().to_string();
        assert!(err.contains("JPEG"), "错误信息里应说明容器格式: {err}");
    }

    #[test]
    fn exposure_summary_is_human_readable() {
        let mut data = ExifData::default();
        data.highlights
            .insert("ExposureTime".into(), "1/100 s".into());
        data.highlights.insert("FNumber".into(), "f/1.75".into());
        data.highlights
            .insert("PhotographicSensitivity".into(), "996".into());
        data.highlights.insert("FocalLength".into(), "4.75 mm".into());
        assert_eq!(exposure_summary(&data), "1/100 s · f/1.75 · ISO 996 · 4.75 mm");
    }
}
