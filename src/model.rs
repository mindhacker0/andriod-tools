//! 数据模型：设备 / 存储 / 照片 / 上传配置 / 上传结果。
//!
//! 这里的类型既用于界面展示，也用于序列化成上传到服务器的 JSON。

use serde::{Deserialize, Serialize};

/// MTP 设备（手机）概要信息。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceSummary {
    /// 设备序列号，`MtpDevice::open_by_serial` 用它唯一定位设备。
    pub serial: String,
    /// USB 位置 ID，`MtpDevice::open_by_location` 用它定位设备。
    pub location_id: u64,
    pub vendor_id: u16,
    pub product_id: u16,
    /// USB 描述符里的产品名（可能是无意义的英文串）。
    pub product: String,
    /// 打开设备后由 `GetDeviceInfo` 得到的厂商。
    pub manufacturer: String,
    /// 打开设备后由 `GetDeviceInfo` 得到的型号。
    pub model: String,
    /// 设备固件/软件版本。
    pub device_version: String,
}

impl DeviceSummary {
    /// 用于下拉框显示的短标签。
    pub fn short_label(&self) -> String {
        let mut name = if !self.model.is_empty() {
            self.model.clone()
        } else if !self.product.is_empty() {
            self.product.clone()
        } else {
            format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
        };
        if !self.serial.is_empty() {
            name.push_str(&format!(" · {}", self.serial));
        }
        name
    }
}

/// 一个存储分区（内置存储 / SD 卡）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StorageSummary {
    pub id: u64,
    pub description: String,
    pub free_space_bytes: u64,
    pub max_capacity_bytes: u64,
    pub storage_type: String,
    /// 该存储是否可写（上传/删除/重命名）。
    pub writable: bool,
}

/// 缩略图的加载状态，用于界面显示占位符。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ThumbState {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// EXIF 的加载状态。
#[derive(Clone, Debug, Default)]
pub enum ExifState {
    /// 还没读取。
    #[default]
    Idle,
    /// 正在读。
    Loading,
    /// 已经解析出来。
    Ready(crate::exif::ExifData),
    /// 读取/解析失败（没有 EXIF 也会走到这里）。
    Failed(String),
}

/// 一张照片（MTP 对象）的全部元数据。
#[derive(Clone, Debug, Default)]
pub struct PhotoInfo {
    /// 界面内部使用的稳定 ID（= 数组下标 + 1 也可以，这里用自增计数）。
    pub key: u64,
    /// 文件所在存储分区。
    pub storage_id: u64,
    pub storage_desc: String,
    /// MTP 对象句柄。
    pub handle: u64,
    pub parent_handle: u64,
    pub filename: String,
    /// 相对存储根目录的路径，例如 `DCIM/Camera/IMG_0001.JPG`。
    pub rel_path: String,
    pub size: u64,
    /// 由扩展名推断的 MIME 类型。
    pub mime_type: String,
    /// MTP `ObjectFormat` 代码（例如 JPEG = 0x3801）。
    pub format_code: u16,
    /// 设备上报的宽度（可能为 0，表示未知）。
    pub width: u32,
    /// 设备上报的高度（可能为 0，表示未知）。
    pub height: u32,
    /// MTP 时间字符串，例如 `2026-10-01 12:30:45`。
    pub created: Option<String>,
    pub modified: Option<String>,
    /// 由 MTP 时间换算出的 Unix 秒（设备没给时区时用本机时区兜底）。
    pub created_unix: Option<i64>,
    pub modified_unix: Option<i64>,
    /// 是否被用户勾选（仅界面使用，不上传）。
    pub picked: bool,
    pub thumb: ThumbState,
    /// 照片的 EXIF（按需读取）。
    pub exif: ExifState,
}

impl PhotoInfo {
    /// 上传用的相对路径，保证以 `/` 分隔。
    pub fn normalized_path(&self) -> String {
        if self.rel_path.is_empty() {
            self.filename.clone()
        } else {
            self.rel_path.replace('\\', "/")
        }
    }
}

/// 一次扫描的统计结果。
#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    /// 已遍历的目录数量。
    pub folders_scanned: usize,
    /// 设备返回的对象总数（文件 + 目录）。
    pub objects_seen: usize,
    /// 找到的照片数量。
    pub photos_found: usize,
    /// 读取失败的目录数量。
    pub folders_failed: usize,
    /// 扫描是否被用户取消。
    pub cancelled: bool,
    /// 扫描耗时（毫秒）。
    pub elapsed_ms: u128,
}

/// 扫描范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanScope {
    /// 递归整个存储分区（保证“所有照片”，但慢）。
    Full,
    /// 只扫描常见的照片目录及其子目录（快）。
    CommonFolders,
}

impl ScanScope {
    pub fn label(self) -> &'static str {
        match self {
            ScanScope::Full => "全部目录（完整扫描）",
            ScanScope::CommonFolders => "常见照片目录（快速扫描）",
        }
    }
}

/// 上传配置。
#[derive(Clone, Debug)]
pub struct UploadConfig {
    /// 服务器根地址，例如 `http://192.168.1.10:8080`。
    pub base_url: String,
    /// 唯一接口路径，默认 `/api/photo/upload`（对齐 NestJS 的 `@Controller('photo')` + `@Post('/upload')`）。
    pub api_path: String,
    /// 可选 Bearer Token。
    pub token: String,
    /// 表单里元数据字段名。
    pub field_metadata: String,
    /// 表单里文件字段名。
    pub field_file: String,
    /// 是否把照片文件本身一起上传（关闭则只上传信息）。
    ///
    /// 勾选时也不是原样上传：文件会先经过 [`crate::prepare`] —— 剥离 EXIF/GPS/注释等
    /// 额外信息，并对支持的格式做无损压缩，画质不变。
    pub upload_file: bool,
    /// 是否解析照片 EXIF 并随信息一起上传。
    ///
    /// 解析的是**缓存里的原始文件**（剥离元数据之前）。界面上选中照片时也会解析一次，
    /// 结果会复用，不会重复解析。
    pub include_exif: bool,
    /// 单文件上限，超过则跳过文件只传信息（0 = 不限制）。
    pub max_file_bytes: u64,
    /// 请求超时（秒）。
    pub timeout_secs: u64,
    /// 失败重试次数。
    pub retries: u32,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".to_string(),
            api_path: "/api/photo/upload".to_string(),
            token: String::new(),
            field_metadata: "metadata".to_string(),
            field_file: "file".to_string(),
            upload_file: true,
            include_exif: true,
            max_file_bytes: 64 * 1024 * 1024,
            timeout_secs: 60,
            retries: 2,
        }
    }
}

impl UploadConfig {
    /// 拼出完整的接口 URL。
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        let path = self.api_path.trim();
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }
}

/// 一次上传请求的结果。
#[derive(Clone, Debug)]
pub struct UploadOutcome {
    pub status: u16,
    pub body: String,
    pub elapsed_ms: u128,
}

impl UploadOutcome {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 截断后的响应体，用于日志展示。
    pub fn brief_body(&self) -> String {
        const MAX: usize = 300;
        let trimmed = self.body.trim();
        if trimmed.chars().count() <= MAX {
            trimmed.to_string()
        } else {
            let s: String = trimmed.chars().take(MAX).collect();
            format!("{s}…")
        }
    }
}
