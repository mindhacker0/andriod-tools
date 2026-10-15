//! 本地照片缓存。
//!
//! 手机上读一个文件要一次（甚至多次）USB 往返，很慢。而同一个文件既要
//! **解析 EXIF**（点开详情时）又要**上传**（剥离元数据 + 无损压缩），
//! 所以先把原图下载到本地缓存，之后所有处理都读缓存：
//!
//! ```text
//! 手机 --(MTP 下载一次)--> 缓存文件 --+--> EXIF 解析
//!                                   +--> 剥离元数据 + 无损压缩 --> 上传
//! ```
//!
//! 缓存放在系统临时目录下的 `phone-tool-cache/`。文件名由
//! 「存储 id + MTP 句柄 + 文件大小 + 扩展名」决定：`mtp-rs` 的句柄是
//! WPD 对象 id 的确定性哈希，重新扫描后同一张照片的句柄不变，
//! 所以缓存可以跨扫描、跨多次查看复用。

use std::fs;
use std::path::{Path, PathBuf};

/// 缓存子目录名（位于系统临时目录下）。
const CACHE_DIR: &str = "phone-tool-cache";

/// 默认缓存根目录（系统临时目录下的 `phone-tool-cache/`）。
pub fn default_root() -> PathBuf {
    std::env::temp_dir().join(CACHE_DIR)
}

/// 照片本地缓存。
pub struct PhotoCache {
    root: PathBuf,
}

impl PhotoCache {
    /// 打开（必要时创建）缓存目录。
    pub fn new() -> Self {
        let root = default_root();
        let _ = fs::create_dir_all(&root);
        Self { root }
    }

    /// 某张照片在缓存里的路径。
    ///
    /// `storage_id + handle + size` 足以唯一定位一个 MTP 对象；扩展名保留下来，
    /// 让它一眼能看出是什么格式（也方便外部工具直接打开）。
    pub fn path_for(&self, storage_id: u64, handle: u64, size: u64, filename: &str) -> PathBuf {
        // 扩展名只留字母数字，防止非法字符 / 路径穿越。
        let ext: String = Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();
        let ext = if ext.is_empty() { "bin".to_string() } else { ext };
        self.root
            .join(format!("{storage_id:x}-{handle:x}-{size}.{ext}"))
    }

    /// 读缓存；不存在或读失败返回 `None`。
    pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    /// 写缓存。
    ///
    /// 先写 `.part` 临时文件再改名，避免读到「写了一半」的文件被当成有效缓存。
    pub fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("part");
        fs::write(&tmp, bytes)?;
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// 缓存占用：(字节数, 文件数)。
    pub fn stats(&self) -> (u64, usize) {
        let mut bytes = 0u64;
        let mut files = 0usize;
        let Ok(entries) = fs::read_dir(&self.root) else {
            return (0, 0);
        };
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    bytes += meta.len();
                    files += 1;
                }
            }
        }
        (bytes, files)
    }

    /// 清空缓存，返回释放的字节数。
    pub fn clear(&self) -> u64 {
        let (bytes, _) = self.stats();
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        bytes
    }
}

impl Default for PhotoCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_stable_and_extension_is_sanitised() {
        let cache = PhotoCache {
            root: PathBuf::from("X:/tmp"),
        };
        let a = cache.path_for(1, 0x2a, 1024, "IMG_0001.JPG");
        let b = cache.path_for(1, 0x2a, 1024, "IMG_0001.JPG");
        assert_eq!(a, b, "同一张照片必须映射到同一个缓存文件");
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("jpg"));

        // 大小变了（手机上被改过）→ 不能复用旧缓存
        assert_ne!(a, cache.path_for(1, 0x2a, 2048, "IMG_0001.JPG"));

        // 非法扩展名被清掉，退回 bin
        let weird = cache.path_for(1, 7, 5, "no-extension");
        assert_eq!(weird.extension().and_then(|e| e.to_str()), Some("bin"));
    }
}
