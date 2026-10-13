//! 上传客户端：把一张照片的元数据（可选带照片文件本身）POST 到服务器的**同一个接口**。
//!
//! 请求格式为 `multipart/form-data`：
//!
//! | 字段                     | 类型        | 说明                                    |
//! |--------------------------|-------------|-----------------------------------------|
//! | `metadata`（可配置）     | text/json   | 照片全部信息的 JSON（见 `build_photo_metadata`） |
//! | `file`（可配置）         | binary      | 照片文件本身；关闭“上传文件”时不发送     |
//!
//! 服务端只要返回任意 2xx 即视为成功。

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};

use crate::model::{UploadConfig, UploadOutcome};

/// 要上传的文件内容。
pub struct PhotoFile {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// 上传客户端（内部复用连接池）。
pub struct Uploader {
    client: reqwest::Client,
}

impl Uploader {
    pub fn new(cfg: &UploadConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5)))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("phone-tool/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Self { client })
    }

    /// 向同一个接口 POST 一条照片记录。
    ///
    /// `file` 为 `None` 时只上传元数据（请求里不含文件部分）。
    pub async fn post_photo(
        &self,
        cfg: &UploadConfig,
        metadata: &serde_json::Value,
        file: Option<PhotoFile>,
    ) -> Result<UploadOutcome> {
        let url = cfg.endpoint();
        let metadata_json = serde_json::to_string(metadata).context("序列化元数据失败")?;
        let attempts = cfg.retries.saturating_add(1);

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=attempts {
            let mut form = Form::new().text(cfg.field_metadata.clone(), metadata_json.clone());

            if let Some(f) = &file {
                let part = Part::bytes(f.bytes.clone())
                    .file_name(f.filename.clone())
                    .mime_str(&f.mime)
                    .with_context(|| format!("非法的 MIME 类型: {}", f.mime))?;
                form = form.part(cfg.field_file.clone(), part);
            }

            let mut req = self.client.post(&url).multipart(form);
            if !cfg.token.trim().is_empty() {
                req = req.bearer_auth(cfg.token.trim());
            }

            let started = Instant::now();
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let outcome = UploadOutcome {
                        status: status.as_u16(),
                        body,
                        elapsed_ms: started.elapsed().as_millis(),
                    };
                    if outcome.is_success() {
                        return Ok(outcome);
                    }
                    // 4xx（除 408 / 429）重试没有意义，直接返回让上层记录。
                    let retryable = status.is_server_error()
                        || status.as_u16() == 408
                        || status.as_u16() == 429;
                    if !retryable || attempt == attempts {
                        return Ok(outcome);
                    }
                    last_err = Some(anyhow::anyhow!("HTTP {} {}", status.as_u16(), outcome.brief_body()));
                }
                Err(err) => {
                    last_err = Some(anyhow::Error::new(err).context(format!("请求 {url} 失败")));
                    if attempt == attempts {
                        break;
                    }
                }
            }

            // 指数退避：400ms, 800ms, 1600ms …
            let backoff = Duration::from_millis(400u64 * (1 << (attempt - 1).min(4)));
            tokio::time::sleep(backoff).await;
        }

        match last_err {
            Some(err) => Err(err),
            None => bail!("上传失败但未捕获到错误信息"),
        }
    }
}

/// 组装一张照片的上传 JSON。
///
/// 结构：
/// ```json
/// {
///   "request_id": "…uuid…",
///   "uploaded_at": "2026-10-12T09:00:00+08:00",
///   "client": { "name": "phone-tool", "version": "0.1.0" },
///   "device": { …DeviceSummary… },
///   "storage": { …StorageSummary… },
///   "photo": { …照片全部字段… },
///   "file_included": true
/// }
/// ```
pub fn build_photo_metadata(
    device: &serde_json::Value,
    storage: &serde_json::Value,
    photo: &serde_json::Value,
    file_included: bool,
) -> serde_json::Value {
    serde_json::json!({
        "request_id": uuid::Uuid::new_v4().to_string(),
        "uploaded_at": chrono::Local::now().to_rfc3339(),
        "client": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "device": device,
        "storage": storage,
        "photo": photo,
        "file_included": file_included,
    })
}
