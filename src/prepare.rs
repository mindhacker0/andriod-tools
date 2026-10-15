//! 上传前处理：**剥离额外信息**（EXIF / GPS / XMP / 注释 / 缩略图…）+ **无损压缩**。
//!
//! 手机拍出来的照片里塞了大量和"像素"无关的东西：EXIF（含拍摄时间、GPS、设备型号）、
//! XMP、缩略图、注释……它们既占体积又泄露隐私。上传前一律去掉，并且**不重编码图像数据**，
//! 所以画质 100% 不变（真·无损）。
//!
//! 各格式的处理方式：
//!
//! | 格式      | 剥离 | 无损压缩 | 说明                                                         |
//! |-----------|------|----------|--------------------------------------------------------------|
//! | JPEG      | ✅   | ✅       | 整段丢弃 APP1/APP3–APP13/APP15 与 COM；**不**动熵编码数据      |
//! | PNG       | ✅   | ✅       | 丢弃 tEXt/zTXt/iTXt/eXIf/tIME，并用最高级别重新 deflate IDAT   |
//! | WebP      | ✅   | ✅       | 从 RIFF 里删掉 `EXIF` / `XMP ` 块，其余数据原样保留             |
//! | 其它      | ❌   | ❌       | HEIC/AVIF/TIFF/RAW 等结构复杂，原样上传（会在日志里说明）      |
//!
//! JPEG 保留 APP0（JFIF 头）、APP2（ICC 色彩配置）和 APP14（Adobe 色彩变换），
//! 因为后两者影响**颜色**的解读；其余全部剥掉。熵编码数据一个字节都不改，
//! 所以这是无损的——只是把"额外信息"扔掉。

use std::io::{Read, Write};

use crate::thumbs::human_bytes;

/// 处理结果。
pub struct Prepared {
    /// 处理后（要上传）的字节。
    pub bytes: Vec<u8>,
    /// 文件名（格式没变，所以一般和原来一样）。
    pub filename: String,
    /// MIME 类型。
    pub mime: String,
    /// 处理前的字节数。
    pub original_len: usize,
    /// 是否真的剥离了额外信息。
    pub stripped: bool,
    /// 人类可读的处理说明，写日志 / 随上传元数据一起发出。
    pub note: String,
}

/// 简单格式判别（只看文件头，够用）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Jpeg,
    Png,
    WebP,
    Other,
}

impl Format {
    fn label(self) -> &'static str {
        match self {
            Format::Jpeg => "JPEG",
            Format::Png => "PNG",
            Format::WebP => "WebP",
            Format::Other => "其它格式",
        }
    }
}

fn sniff(bytes: &[u8]) -> Format {
    match bytes {
        [0xFF, 0xD8, ..] => Format::Jpeg,
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Format::Png,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Format::WebP,
        _ => Format::Other,
    }
}

/// 剥离额外信息 + 无损压缩。**一定不会让画质变差**：任何一步失败都退回原字节。
pub fn prepare(filename: &str, mime: &str, bytes: Vec<u8>) -> Prepared {
    let original_len = bytes.len();
    let format = sniff(&bytes);

    let cleaned = match format {
        Format::Jpeg => strip_jpeg(&bytes),
        Format::Png => strip_png(&bytes),
        Format::WebP => strip_webp(&bytes),
        Format::Other => None,
    };

    match cleaned {
        // 真正变小了才采用；没变小（甚至变大）就保持原样。
        Some(c) if c.bytes.len() < original_len => {
            let new_len = c.bytes.len();
            let saved_pct = if original_len == 0 {
                0.0
            } else {
                (original_len - new_len) as f64 / original_len as f64 * 100.0
            };
            let note = format!(
                "{} 已剥离额外信息{}，{} → {}（省 {:.1}%）",
                format.label(),
                if c.recompressed { "并无损重压缩" } else { "" },
                human_bytes(original_len as u64),
                human_bytes(new_len as u64),
                saved_pct
            );
            Prepared {
                bytes: c.bytes,
                filename: filename.to_string(),
                mime: mime.to_string(),
                original_len,
                stripped: true,
                note,
            }
        }
        Some(_) => Prepared {
            bytes,
            filename: filename.to_string(),
            mime: mime.to_string(),
            original_len,
            stripped: false,
            note: format!("{} 剥离后没有变小，保持原样上传", format.label()),
        },
        None => Prepared {
            bytes,
            filename: filename.to_string(),
            mime: mime.to_string(),
            original_len,
            stripped: false,
            note: match format {
                Format::Other => "该格式未做剥离（HEIC/AVIF/TIFF/RAW 等），原样上传".to_string(),
                other => format!("{} 结构无法安全剥离，原样上传", other.label()),
            },
        },
    }
}

/// 剥离后的结果。
struct Cleaned {
    bytes: Vec<u8>,
    /// 是否额外做了无损重压缩（只有 PNG 会）。
    recompressed: bool,
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

/// 保留哪些 APPn 段：APP0(JFIF 头)、APP2(ICC 色彩配置)、APP14(Adobe 色彩变换)。
/// 其余 APPn 与 COM 全部丢弃。
fn keep_app(marker: u8) -> bool {
    matches!(marker, 0xE0 | 0xE2 | 0xEE)
}

/// 逐段重写 JPEG：丢掉元数据段，熵编码数据原样搬运。
fn strip_jpeg(src: &[u8]) -> Option<Cleaned> {
    if src.len() < 4 || src[0] != 0xFF || src[1] != 0xD8 {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    out.extend_from_slice(&src[0..2]); // SOI

    let mut i = 2usize;
    let mut stripped = false;

    while i < src.len() {
        if src[i] != 0xFF {
            // 段边界对不上，说明结构比较特殊；放弃剥离，保持原样。
            return None;
        }
        // 允许 0xFF 填充
        let mut j = i;
        while j < src.len() && src[j] == 0xFF {
            j += 1;
        }
        if j >= src.len() {
            return None;
        }
        let marker = src[j];
        if marker == 0x00 {
            // FF00 只会出现在熵编码数据里，不该在段头位置出现。
            return None;
        }

        // 无长度字段的标记：SOI / EOI / RSTn / TEM
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&src[i..=j]);
            i = j + 1;
            if marker == 0xD9 {
                break; // EOI 之后的字节（一般没有）不再搬运
            }
            continue;
        }

        // 带长度字段的标记
        if j + 2 >= src.len() {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([src[j + 1], src[j + 2]]));
        if len < 2 || j + 1 + len > src.len() {
            return None;
        }
        let seg_end = j + 1 + len; // 排他

        let is_app = (0xE0..=0xEF).contains(&marker);
        let is_comment = marker == 0xFE;
        let drop = (is_app && !keep_app(marker)) || is_comment;
        if drop {
            stripped = true;
        } else {
            out.extend_from_slice(&src[i..seg_end]);
        }
        i = seg_end;

        // SOS：后面是熵编码数据，整段搬到 EOI / 下一个真标记为止。
        if marker == 0xDA {
            let data_start = i;
            let mut k = i;
            let mut end = src.len();
            while k + 1 < src.len() {
                if src[k] == 0xFF {
                    let m = src[k + 1];
                    // FF00 是数据里的转义，FFxx(填充) 也一样，RSTn 是重启标记
                    if m == 0x00 || m == 0xFF || (0xD0..=0xD7).contains(&m) {
                        k += 2;
                        continue;
                    }
                    end = k;
                    break;
                }
                k += 1;
            }
            out.extend_from_slice(&src[data_start..end]);
            i = end;
        }
    }

    if !stripped {
        return None;
    }
    // 保证以 EOI 收尾
    if out.len() < 2 || out[out.len() - 2] != 0xFF || out[out.len() - 1] != 0xD9 {
        out.extend_from_slice(&[0xFF, 0xD9]);
    }
    Some(Cleaned {
        bytes: out,
        recompressed: false,
    })
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

/// 丢掉的元数据块。
fn drop_png_chunk(name: &str) -> bool {
    matches!(name, "tEXt" | "zTXt" | "iTXt" | "eXIf" | "tIME")
}

/// 重写 PNG：丢掉元数据块，并把 IDAT 用最高压缩级别重新 deflate（无损）。
fn strip_png(src: &[u8]) -> Option<Cleaned> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if src.len() < 8 || src[..8] != SIG {
        return None;
    }

    let mut ihdr: Option<Vec<u8>> = None; // IHDR 原始块
    let mut head: Vec<Vec<u8>> = Vec::new(); // IHDR 与 IDAT 之间要保留的块
    let mut idat_chunks: Vec<Vec<u8>> = Vec::new(); // 原始 IDAT 块
    let mut idat_data: Vec<u8> = Vec::new(); // 拼接后的 IDAT 数据
    let mut dropped = 0usize;
    let mut seen_iend = false;

    let mut i = 8usize;
    while i + 8 <= src.len() {
        let len = u32::from_be_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]) as usize;
        let total = 12 + len; // length(4) + type(4) + data + crc(4)
        if i + total > src.len() {
            return None;
        }
        let chunk = &src[i..i + total];
        let name = std::str::from_utf8(&src[i + 4..i + 8]).ok()?;

        match name {
            "IHDR" => ihdr = Some(chunk.to_vec()),
            "IDAT" => {
                idat_data.extend_from_slice(&src[i + 8..i + 8 + len]);
                idat_chunks.push(chunk.to_vec());
            }
            "IEND" => {
                seen_iend = true;
                break;
            }
            n if drop_png_chunk(n) => dropped += 1,
            _ => head.push(chunk.to_vec()),
        }
        i += total;
    }

    let ihdr = ihdr?;
    if !seen_iend || idat_data.is_empty() {
        return None;
    }

    // 无损重压缩：zlib 流解开再用最高级别压缩回去，像素数据完全等价。
    let recompressed = recompress_zlib(&idat_data);
    let use_new = recompressed
        .as_ref()
        .map(|v| v.len() < idat_chunks.iter().map(|c| c.len()).sum::<usize>())
        .unwrap_or(false);
    if !use_new && dropped == 0 {
        return None;
    }

    let mut out = Vec::with_capacity(src.len());
    out.extend_from_slice(&SIG);
    out.extend_from_slice(&ihdr);
    for c in &head {
        out.extend_from_slice(c);
    }
    if use_new {
        write_png_chunk(&mut out, b"IDAT", recompressed.as_deref().unwrap_or_default());
    } else {
        for c in &idat_chunks {
            out.extend_from_slice(c);
        }
    }
    write_png_chunk(&mut out, b"IEND", &[]);

    Some(Cleaned {
        bytes: out,
        recompressed: use_new,
    })
}

/// 写一个 PNG 块（自动算 CRC32）。
fn write_png_chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    let mut crc = 0xFFFF_FFFFu32;
    for &b in name.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}

/// 解开 zlib 流再用最高级别压回去（无损）。失败返回 `None`。
fn recompress_zlib(data: &[u8]) -> Option<Vec<u8>> {
    let mut plain = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut plain)
        .ok()?;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&plain).ok()?;
    let out = enc.finish().ok()?;
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// WebP
// ---------------------------------------------------------------------------

/// 从 RIFF 里删掉 `EXIF` / `XMP ` 块，其余块原样保留（含 VP8/VP8L/VP8X/ALPH/ICCP）。
fn strip_webp(src: &[u8]) -> Option<Cleaned> {
    if src.len() < 12 || &src[0..4] != b"RIFF" || &src[8..12] != b"WEBP" {
        return None;
    }
    let mut body: Vec<u8> = Vec::with_capacity(src.len());
    let mut dropped = 0usize;

    let mut i = 12usize;
    while i + 8 <= src.len() {
        let fourcc = &src[i..i + 4];
        let size = u32::from_le_bytes([src[i + 4], src[i + 5], src[i + 6], src[i + 7]]) as usize;
        let padded = size + (size & 1); // 块按偶数长度对齐
        if i + 8 + padded > src.len() {
            return None; // 结构不对，别乱动
        }
        if fourcc == b"EXIF" || fourcc == b"XMP " {
            dropped += 1;
        } else {
            body.extend_from_slice(&src[i..i + 8 + padded]);
        }
        i += 8 + padded;
    }

    if dropped == 0 {
        return None;
    }

    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(b"RIFF");
    // RIFF 的 size 字段 = 文件总长 - 8
    out.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Some(Cleaned {
        bytes: out,
        recompressed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个"带 EXIF 的最小 JPEG"：SOI + APP1(Exif) + SOS(空数据) + EOI。
    fn jpeg_with_exif() -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        // APP1
        let payload = b"Exif\0\0MM\0*\0\0\0\x08\0\x00";
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(payload);
        // SOS（长度 2，无实际分量数据）
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        jpeg.extend_from_slice(&[0x12, 0x34]); // 假熵数据
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn jpeg_metadata_is_stripped_and_still_valid() {
        let src = jpeg_with_exif();
        let out = strip_jpeg(&src).expect("应当能剥离");
        assert!(out.bytes.len() < src.len(), "剥离后应当更小");
        assert_eq!(&out.bytes[0..2], &[0xFF, 0xD8], "SOI 保留");
        assert_eq!(&out.bytes[out.bytes.len() - 2..], &[0xFF, 0xD9], "EOI 保留");
        // APP1 不该再出现
        assert!(
            !out.bytes.windows(2).any(|w| w == [0xFF, 0xE1]),
            "EXIF(APP1) 应当被删掉"
        );
        // 熵编码数据原样保留
        assert!(out.bytes.windows(2).any(|w| w == [0x12, 0x34]), "图像数据不能被改");
    }

    #[test]
    fn jpeg_without_metadata_is_left_alone() {
        let src = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0xD9];
        assert!(strip_jpeg(&src).is_none(), "没有额外信息时不做任何改动");
    }

    #[test]
    fn png_text_chunk_is_dropped() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        // IHDR
        write_png_chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        // tEXt（要丢掉）
        write_png_chunk(&mut png, b"tEXt", b"Comment\0secret");
        // IDAT：zlib("abc")
        let idat = {
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(b"abc").unwrap();
            enc.finish().unwrap()
        };
        write_png_chunk(&mut png, b"IDAT", &idat);
        write_png_chunk(&mut png, b"IEND", &[]);

        let out = strip_png(&png).expect("应当能剥离");
        assert!(
            !out.bytes.windows(4).any(|w| w == b"tEXt"),
            "tEXt 元数据块必须被删掉"
        );
        assert!(out.bytes.windows(4).any(|w| w == b"IHDR"));
        assert!(out.bytes.windows(4).any(|w| w == b"IDAT"));
        assert!(out.bytes.windows(4).any(|w| w == b"IEND"));
    }

    #[test]
    fn webp_exif_chunk_is_dropped() {
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&0u32.to_le_bytes()); // 稍后补
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(b"VP8 ");
        webp.extend_from_slice(&4u32.to_le_bytes());
        webp.extend_from_slice(&[1, 2, 3, 4]); // 4 字节数据
        webp.extend_from_slice(b"EXIF");
        webp.extend_from_slice(&4u32.to_le_bytes());
        webp.extend_from_slice(&[9, 9, 9, 9]);
        let size = (webp.len() - 8) as u32;
        webp[4..8].copy_from_slice(&size.to_le_bytes());

        let out = strip_webp(&webp).expect("应当能剥离");
        assert!(
            !out.bytes.windows(4).any(|w| w == b"EXIF"),
            "EXIF 块必须被删掉"
        );
        assert!(out.bytes.windows(4).any(|w| w == b"VP8 "), "图像数据保留");
        // RIFF 长度字段要跟着更新
        let declared = u32::from_le_bytes([out.bytes[4], out.bytes[5], out.bytes[6], out.bytes[7]]);
        assert_eq!(declared as usize, out.bytes.len() - 8);
    }

    #[test]
    fn prepare_falls_back_for_unknown_formats() {
        let raw = b"not an image at all".to_vec();
        let p = prepare("a.bin", "application/octet-stream", raw.clone());
        assert_eq!(p.bytes, raw, "未知格式必须原样返回");
        assert!(!p.stripped);
    }
}
