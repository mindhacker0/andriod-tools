//! 后台 MTP 工作线程。
//!
//! MTP 设备一次只允许一个会话/一条操作，而且所有操作都是 I/O 密集的，
//! 所以这里用一个**专用线程 + 单任务串行循环**来访问设备：
//!
//! ```text
//! 界面线程  --Cmd-->  工作线程（tokio runtime，串行执行）
//!    ^                     |
//!    +------ Evt <---------+
//! ```
//!
//! 界面线程只做渲染，所有阻塞/异步的 USB、网络操作都在工作线程里完成。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use mtp_rs::mtp::{MtpDevice, ObjectHandle, Storage};
use mtp_rs::UtcOffset;

use crate::exif::{self, ExifData};
use crate::model::{
    DeviceSummary, ExifState, PhotoInfo, ScanScope, ScanStats, StorageSummary, ThumbState,
    UploadConfig,
};
use crate::thumbs::{decode_thumbnail, is_photo_extension, mime_from_filename, ThumbData};
use crate::uploader::{build_photo_metadata, PhotoFile, Uploader};

/// 递归扫描的最大目录深度，防止异常目录结构导致无限递归。
const MAX_DEPTH: usize = 14;
/// 单次扫描最多遍历的目录数，防止病态存储分区。
const MAX_FOLDERS: usize = 40_000;
/// 每找到多少张照片就向界面推一次快照。
const SNAPSHOT_EVERY: usize = 150;
/// 界面缩略图的长边像素。
const THUMB_EDGE: u32 = 320;
/// 缩略图失败后回退到整张图片解码时允许的最大字节数。
const THUMB_FALLBACK_MAX_BYTES: u64 = 24 * 1024 * 1024;
/// 读 EXIF 时先只看文件头的字节数（JPEG 的 EXIF 在 APP1，位于文件最前面）。
const EXIF_HEAD_BYTES: u64 = 128 * 1024;
/// 文件头里没读到 EXIF 时，允许整张下载再试的上限（HEIC 等 EXIF 藏在后面）。
const EXIF_FULL_MAX_BYTES: u64 = 24 * 1024 * 1024;

/// 界面发给工作线程的命令。
pub enum Cmd {
    /// 列出当前插入的 MTP 设备。
    ListDevices,
    /// 打开设备。`None` 表示直接打开第一台可用设备。
    Connect { device: Option<DeviceSummary> },
    /// 关闭设备会话。
    Disconnect,
    /// 扫描照片。
    Scan { scope: ScanScope },
    /// 拉取一张照片的缩略图。
    Thumbnail { key: u64 },
    /// 读取一张照片的 EXIF。
    Exif { key: u64 },
    /// 上传指定照片（元数据必传，文件是否上传由 `cfg.upload_file` 决定）。
    Upload { keys: Vec<u64>, cfg: UploadConfig },
}

/// 工作线程发给界面的事件。
pub enum Evt {
    Log(String),
    /// 需要用户看到并处理错误（界面会弹红色横幅）。
    Error(String),
    /// 当前操作因错误结束，界面应复位所有“忙”状态。
    OperationFailed,
    Devices(Vec<DeviceSummary>),
    Connected {
        device: DeviceSummary,
        storages: Vec<StorageSummary>,
    },
    Disconnected,
    ScanStarted {
        storages: usize,
        scope: ScanScope,
    },
    ScanProgress {
        folders: usize,
        photos: usize,
        current: String,
    },
    ScanFinished(ScanStats),
    /// 照片列表快照（全量替换）。
    Photos(Vec<PhotoInfo>),
    Thumbnail {
        key: u64,
        result: Result<ThumbData, String>,
    },
    /// 一张照片的 EXIF 读取结果。
    Exif {
        key: u64,
        result: Result<ExifData, String>,
    },
    UploadStarted {
        total: usize,
    },
    UploadProgress {
        done: usize,
        total: usize,
        filename: String,
        ok: bool,
        message: String,
    },
    UploadFinished {
        ok: usize,
        failed: usize,
    },
}

/// 工作线程句柄：命令发送端 + 事件接收端。
pub struct WorkerHandle {
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Evt>,
    /// 界面直接置位即可取消正在进行的扫描。
    pub cancel: Arc<AtomicBool>,
}

/// 照片在设备上的位置。
struct Stored {
    storage_index: usize,
    handle: ObjectHandle,
    info: PhotoInfo,
}

/// 扫描结果的累积器。
///
/// `keys` 把「存储序号 + MTP 句柄」映射到一个稳定的自增 ID，
/// 这样重新扫描时界面上的勾选状态和缩略图都能保留。
#[derive(Default)]
struct ScanAccum {
    stored: BTreeMap<u64, Stored>,
    keys: HashMap<(usize, u64), u64>,
    next_key: u64,
}

impl ScanAccum {
    fn key_for(&mut self, storage_index: usize, handle: u64) -> u64 {
        if let Some(existing) = self.keys.get(&(storage_index, handle)) {
            return *existing;
        }
        self.next_key += 1;
        let key = self.next_key;
        self.keys.insert((storage_index, handle), key);
        key
    }
}

/// 工作线程持有的设备状态。
#[derive(Default)]
struct State {
    device: Option<MtpDevice>,
    device_summary: DeviceSummary,
    storages: Vec<Storage>,
    storage_meta: Vec<StorageSummary>,
    scan: ScanAccum,
}

/// 启动工作线程。
pub fn spawn(repaint: impl Fn() + Send + Sync + 'static) -> WorkerHandle {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (evt_tx, evt_rx) = channel::<Evt>();
    let cancel = Arc::new(AtomicBool::new(false));

    let cancel_for_thread = cancel.clone();
    let repaint = Arc::new(repaint);

    std::thread::Builder::new()
        .name("mtp-worker".to_string())
        .spawn(move || {
            // 用单线程 runtime：Windows 上 mtp-rs 走 WPD COM，所有 COM 调用都固定在
            // 这一个线程上最稳妥（COM 单元线程模型），而 MTP 本身就是串行的，
            // 多线程没有收益。
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = evt_tx.send(Evt::Log(format!("创建异步运行时失败: {e}")));
                    repaint();
                    return;
                }
            };

            let mut state = State::default();
            while let Ok(cmd) = cmd_rx.recv() {
                let result = rt.block_on(handle_cmd(cmd, &mut state, &evt_tx, &cancel_for_thread));
                if let Err(e) = result {
                    let _ = evt_tx.send(Evt::Log(format!("操作失败: {e:#}")));
                    // 不管失败在哪一步，都要让界面把“连接中/扫描中/上传中”复位，“
                    // 否则按钮会永远禁用、转圈不会停。
                    let _ = evt_tx.send(Evt::OperationFailed);
                }
                repaint();
            }
        })
        .expect("无法启动 MTP 工作线程");

    WorkerHandle {
        tx: cmd_tx,
        rx: evt_rx,
        cancel,
    }
}

// ---------------------------------------------------------------------------
// 命令分发
// ---------------------------------------------------------------------------

async fn handle_cmd(
    cmd: Cmd,
    state: &mut State,
    tx: &Sender<Evt>,
    cancel: &AtomicBool,
) -> Result<()> {
    match cmd {
        Cmd::ListDevices => match list_devices(tx) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = tx.send(Evt::Error(format!("枚举 MTP 设备失败：{e:#}")));
                Err(e)
            }
        },
        Cmd::Connect { device } => connect(state, device, tx).await,
        Cmd::Disconnect => disconnect(state, tx).await,
        Cmd::Scan { scope } => scan(state, scope, tx, cancel).await,
        Cmd::Thumbnail { key } => thumbnail(state, key, tx).await,
        Cmd::Exif { key } => load_exif(state, key, tx).await,
        Cmd::Upload { keys, cfg } => upload(state, keys, cfg, tx).await,
    }
}

/// 列出设备。
///
/// 注意：`MtpDevice::list_devices()` 走的是 **nusb 原始 USB 枚举**，
/// 在 Windows 上并不能直接打开手机（手机绑定在 WPD 驱动下），
/// 真正的打开在 [`connect`] 里用位置/VID-PID 关联到 WPD 完成。
fn list_devices(tx: &Sender<Evt>) -> Result<()> {
    let devices = MtpDevice::list_devices().context("枚举 MTP 设备失败")?;
    let list: Vec<DeviceSummary> = devices
        .iter()
        .map(|d| DeviceSummary {
            serial: d.serial_number.clone().unwrap_or_default(),
            location_id: d.location_id,
            vendor_id: d.vendor_id,
            product_id: d.product_id,
            product: d.product.clone().unwrap_or_default(),
            manufacturer: d.manufacturer.clone().unwrap_or_default(),
            ..Default::default()
        })
        .collect();

    let _ = tx.send(Evt::Log(format!("发现 {} 台 MTP 设备", list.len())));
    for d in &devices {
        let _ = tx.send(Evt::Log(format!(
            "· {} （VID:PID={:04x}:{:04x}，位置 0x{:08x}，速率 {:?}，匹配原因 {:?}）",
            d.display(),
            d.vendor_id,
            d.product_id,
            d.location_id,
            d.speed,
            d.match_reason
        )));
    }
    if list.is_empty() {
        let _ = tx.send(Evt::Error(
            "没有发现任何 MTP 设备。请检查：\n\
             ① 手机 USB 用途选“文件传输 / MTP”（不要选“仅充电”）；\n\
             ② 手机保持解锁，弹窗“允许访问设备数据”要点允许；\n\
             ③ 换一根能传数据的 USB 线 / 换个 USB 口（优先主板直插，别用 Hub）；\n\
             ④ 关闭其它占用手机的软件（手机助手、iTunes 类、Android File Transfer 等）；\n\
             ⑤ 实在认不到，可以点“直接连接”让程序直接枚举 Windows 的 WPD 设备。"
                .to_string(),
        ));
    }
    let _ = tx.send(Evt::Devices(list));
    Ok(())
}

/// 打开设备并读取存储分区。
///
/// Windows 上手机枚举为 **WPD** 设备，而 USB 描述符里的序列号往往与 WPD 序列号不同，
/// 所以打开顺序按“可靠性从高到低”排：
///
/// 1. **USB 位置** —— `open_by_location` 会用 VID/PID 关联到 WPD 设备，这是 Windows 上最稳的路径；
/// 2. **序列号** —— 当 WPD 序列号与 USB 序列号一致时才有效；
/// 3. **第一台设备** —— 直接用 WPD 枚举，适合设备列表为空（nusb 认不到）的情况。
async fn connect(
    state: &mut State,
    requested: Option<DeviceSummary>,
    tx: &Sender<Evt>,
) -> Result<()> {
    if state.device.is_some() {
        disconnect(state, tx).await?;
    }

    let mut attempts: Vec<String> = Vec::new();
    let mut opened: Option<MtpDevice> = None;

    if let Some(d) = &requested {
        match MtpDevice::open_by_location(d.location_id).await {
            Ok(dev) => {
                let _ = tx.send(Evt::Log(format!("已按 USB 位置 0x{:08x} 打开设备", d.location_id)));
                opened = Some(dev);
            }
            Err(e) => attempts.push(format!("按 USB 位置 0x{:08x} 打开失败：{e}", d.location_id)),
        }

        if opened.is_none() && !d.serial.is_empty() {
            match MtpDevice::open_by_serial(&d.serial).await {
                Ok(dev) => {
                    let _ = tx.send(Evt::Log(format!("已按序列号 {} 打开设备", d.serial)));
                    opened = Some(dev);
                }
                Err(e) => attempts.push(format!("按序列号 {} 打开失败：{e}", d.serial)),
            }
        }
    }

    if opened.is_none() {
        match MtpDevice::open_first().await {
            Ok(dev) => {
                let _ = tx.send(Evt::Log("已打开第一台 MTP 设备".to_string()));
                opened = Some(dev);
            }
            Err(e) => attempts.push(format!("打开第一台设备失败：{e}")),
        }
    }

    let Some(device) = opened else {
        let msg = format!(
            "连接手机失败：{}\n\
             排查建议：\n\
             ① 手机上把 USB 用途切到“文件传输 / MTP”并保持解锁；\n\
             ② 手机弹窗“允许访问设备数据”要点允许（没弹就拔插一次数据线）；\n\
             ③ 换一根能传数据的 USB 线 / 换个 USB 口（优先主板直插）；\n\
             ④ 关闭其它占用手机的软件（手机助手、iTunes 类、Android File Transfer 等）；\n\
             ⑤ 若手机屏幕弹出“是否信任此电脑”，选信任。",
            attempts.join("；")
        );
        let _ = tx.send(Evt::Error(msg.clone()));
        anyhow::bail!("{msg}");
    };

    // 合并 USB 侧信息（下拉框里选中的那份）与设备自报的信息。
    let mut summary = requested.unwrap_or_default();
    let info = device.device_info();
    summary.manufacturer = info.manufacturer.clone();
    summary.model = info.model.clone();
    if !info.serial_number.is_empty() {
        summary.serial = info.serial_number.clone();
    }
    summary.device_version = info.device_version.clone();
    state.device_summary = summary;

    let storages = device.storages().await.context("读取存储分区失败")?;
    let mut metas = Vec::with_capacity(storages.len());
    for (i, s) in storages.iter().enumerate() {
        let si = s.info();
        metas.push(StorageSummary {
            id: s.id().0,
            description: si.description.clone(),
            free_space_bytes: si.free_space,
            max_capacity_bytes: si.total_capacity,
            storage_type: format!("{:?}", si.storage_type),
            writable: si.is_writable,
        });
        let _ = tx.send(Evt::Log(format!(
            "存储分区 #{i}: {} （可用 {:.2} GB / 共 {:.2} GB）",
            si.description,
            si.free_space as f64 / 1e9,
            si.total_capacity as f64 / 1e9
        )));
    }

    state.device = Some(device);
    state.storages = storages;
    state.storage_meta = metas.clone();
    state.scan.stored.clear();

    let _ = tx.send(Evt::Log(format!(
        "已连接: {} {} （序列号 {}）",
        state.device_summary.manufacturer, state.device_summary.model, state.device_summary.serial
    )));

    // 护栅：接到一台没有任何存储分区的设备，几乎一定是接错了。
    // Windows 上“打开第一台”可能选中同机插着的 U 盘/读卡器（它们也能讲 MTP），
    // 表现就是“连接成功但一张照片都没有”。
    if metas.is_empty() {
        let msg = format!(
            "已连接的设备「{} {}」没有任何存储分区，很可能不是你的手机（例如 U 盘 / 读卡器）。\n\
             请在顶部的设备下拉框里选中手机型号，再点「🔌 连接」。",
            state.device_summary.manufacturer, state.device_summary.model
        );
        let _ = tx.send(Evt::Error(msg));
        return Ok(());
    }
    let _ = tx.send(Evt::Connected {
        device: state.device_summary.clone(),
        storages: metas,
    });
    Ok(())
}

/// 关闭设备会话。
async fn disconnect(state: &mut State, tx: &Sender<Evt>) -> Result<()> {
    if let Some(device) = state.device.take() {
        if let Err(e) = device.close().await {
            let _ = tx.send(Evt::Log(format!("关闭会话时出错（可忽略）: {e}")));
        }
    }
    state.storages.clear();
    state.storage_meta.clear();
    state.scan.stored.clear();
    state.device_summary = DeviceSummary::default();
    let _ = tx.send(Evt::Log("已断开设备".to_string()));
    let _ = tx.send(Evt::Disconnected);
    Ok(())
}

// ---------------------------------------------------------------------------
// 扫描照片
// ---------------------------------------------------------------------------

/// 递归扫描所有存储分区，找出照片，并把进度推给界面。
async fn scan(
    state: &mut State,
    scope: ScanScope,
    tx: &Sender<Evt>,
    cancel: &AtomicBool,
) -> Result<()> {
    if state.device.is_none() {
        let _ = tx.send(Evt::Log("请先连接手机再扫描".to_string()));
        return Ok(());
    }
    cancel.store(false, Ordering::Relaxed);
    state.scan.stored.clear();

    let started = Instant::now();
    let mut stats = ScanStats::default();
    let _ = tx.send(Evt::ScanStarted {
        storages: state.storages.len(),
        scope,
    });

    for index in 0..state.storages.len() {
        if cancel.load(Ordering::Relaxed) {
            stats.cancelled = true;
            break;
        }
        let storage = &state.storages[index];
        let summary = state.storage_meta[index].clone();
        let _ = tx.send(Evt::Log(format!(
            "开始扫描存储分区 [{}]（{}）",
            summary.description,
            scope.label()
        )));

        walk_storage(
            storage,
            &summary,
            index,
            scope,
            tx,
            cancel,
            &mut state.scan,
            &mut stats,
        )
        .await;
    }

    stats.photos_found = state.scan.stored.len();
    stats.elapsed_ms = started.elapsed().as_millis();
    let _ = tx.send(Evt::Log(format!(
        "扫描结束: 目录 {}，看到对象 {}，照片 {}，失败目录 {}{}，耗时 {:.1}s",
        stats.folders_scanned,
        stats.objects_seen,
        stats.photos_found,
        stats.folders_failed,
        if stats.cancelled { "（已取消）" } else { "" },
        stats.elapsed_ms as f64 / 1000.0
    )));
    if stats.photos_found == 0 && state.scan.stored.is_empty() {
        let _ = tx.send(Evt::Log(
            "没有找到照片。对照上面的“根目录可见目录”看看照片在哪个目录；\
             若是“常见照片目录（快速扫描）”模式，请把范围改成“全部目录（完整扫描）”再扫一次。"
                .to_string(),
        ));
    }
    let _ = tx.send(Evt::Photos(snapshot(&state.scan.stored)));
    let _ = tx.send(Evt::ScanFinished(stats));
    Ok(())
}

/// 广度优先遍历一个存储分区。
///
/// 注意：Android 的 `GetObjectHandles` 递归是坏的，所以这里手动逐目录遍历，
/// 每进一个目录发一次进度，让界面可以显示进度并响应取消。
#[allow(clippy::too_many_arguments)]
async fn walk_storage(
    storage: &Storage,
    summary: &StorageSummary,
    storage_index: usize,
    scope: ScanScope,
    tx: &Sender<Evt>,
    cancel: &AtomicBool,
    acc: &mut ScanAccum,
    stats: &mut ScanStats,
) {
    let mut queue: VecDeque<(Option<ObjectHandle>, String, usize)> = VecDeque::new();
    queue.push_back((None, String::new(), 0));
    let mut last_snapshot = 0usize;

    while let Some((parent, dir_path, depth)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) || stats.folders_scanned >= MAX_FOLDERS {
            stats.cancelled = cancel.load(Ordering::Relaxed);
            return;
        }
        if depth > MAX_DEPTH {
            continue;
        }

        let objects = match storage.list_objects(parent).await {
            Ok(objects) => objects,
            Err(e) => {
                stats.folders_failed += 1;
                let _ = tx.send(Evt::Log(format!("读取目录 /{dir_path} 失败: {e}")));
                continue;
            }
        };
        stats.folders_scanned += 1;
        stats.objects_seen += objects.len();

        // 根目录的第一层名单对排查“照片到底在哪”非常关键，直接打到日志里。
        if dir_path.is_empty() {
            let mut folders: Vec<String> = objects
                .iter()
                .filter(|o| o.is_folder())
                .map(|o| o.filename.clone())
                .collect();
            folders.sort();
            let shown = folders.len().min(15);
            let mut text = folders[..shown].join(", ");
            if folders.len() > shown {
                text.push_str(&format!(" …（共 {} 个目录）", folders.len()));
            }
            let _ = tx.send(Evt::Log(format!(
                "[{}] 根目录返回 {} 个对象，可见目录：{}",
                summary.description,
                objects.len(),
                if text.is_empty() { "（空）".to_string() } else { text }
            )));
        }

        for obj in objects {
            if obj.is_folder() {
                let child_path = join_path(&dir_path, &obj.filename);
                if should_skip_dir(&child_path, depth, scope) {
                    continue;
                }
                queue.push_back((Some(obj.handle), child_path, depth + 1));
            } else if is_photo_object(&obj.filename, obj.image_width, obj.image_height) {
                let key = acc.key_for(storage_index, obj.handle.0);
                let mime = mime_from_filename(&obj.filename);
                let (created, created_unix) = format_time(obj.created.as_ref());
                let (modified, modified_unix) = format_time(obj.modified.as_ref());
                let info = PhotoInfo {
                    key,
                    storage_id: summary.id,
                    storage_desc: summary.description.clone(),
                    handle: obj.handle.0,
                    parent_handle: obj.parent.0,
                    filename: obj.filename.clone(),
                    rel_path: join_path(&dir_path, &obj.filename),
                    size: obj.size,
                    mime_type: mime,
                    format_code: obj.format.code(),
                    width: obj.image_width,
                    height: obj.image_height,
                    created,
                    modified,
                    created_unix,
                    modified_unix,
                    picked: false,
                    thumb: ThumbState::Idle,
                    exif: ExifState::Idle,
                };
                acc.stored.insert(
                    key,
                    Stored {
                        storage_index,
                        handle: obj.handle,
                        info,
                    },
                );
            }
        }

        let found = acc.stored.len();
        if found >= last_snapshot + SNAPSHOT_EVERY {
            last_snapshot = found;
            let _ = tx.send(Evt::Photos(snapshot(&acc.stored)));
        }
        if stats.folders_scanned % 5 == 0 {
            let _ = tx.send(Evt::ScanProgress {
                folders: stats.folders_scanned,
                photos: found,
                current: format!("/{dir_path}"),
            });
        }
    }
}

/// 把内部存储状态拍一份快照发给界面。
fn snapshot(stored: &BTreeMap<u64, Stored>) -> Vec<PhotoInfo> {
    stored.values().map(|s| s.info.clone()).collect()
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// 是否算“照片”：靠扩展名，另外把设备明确标注了宽高的对象也算进去。
fn is_photo_object(filename: &str, width: u32, height: u32) -> bool {
    is_photo_extension(filename) || (width > 0 && height > 0)
}

/// 常见照片目录（快速扫描模式只看这些一级目录）。
const COMMON_PHOTO_FOLDERS: &[&str] = &[
    "dcim",
    "pictures",
    "download",
    "downloads",
    "camera",
    "photos",
    "images",
    "screenshots",
    "screenshot",
    "movies",
    "video",
    "videos",
    "media",
    "pictures/screenshots",
    "weixin",
    "tencent",
    "micromsg",
    "bluetooth",
    "pictures/screenshots",
    "qq",
    "dingtalk",
];

/// 无价值的目录（缩略图缓存、系统目录……），直接跳过。
const SKIP_PATH_PARTS: &[&str] = &[
    "android/data",
    "android/obb",
    "android/media/",
    ".thumbnails",
    "lost.dir",
    "$recycle.bin",
    "system volume information",
    ".trash",
    ".nomedia",
];

fn should_skip_dir(child_path: &str, depth: usize, scope: ScanScope) -> bool {
    let lower = child_path.to_lowercase();
    if SKIP_PATH_PARTS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    if lower.starts_with("android/") && lower.split('/').count() > 1 && !lower.starts_with("android/media")
    {
        return true;
    }
    if depth == 0 && scope == ScanScope::CommonFolders {
        let name = lower.split('/').next().unwrap_or(&lower).to_string();
        return !COMMON_PHOTO_FOLDERS.iter().any(|f| *f == name);
    }
    false
}

/// MTP 时间 → (本地时间字符串, Unix 秒)。
fn format_time(dt: Option<&mtp_rs::mtp::DateTime>) -> (Option<String>, Option<i64>) {
    let Some(dt) = dt else {
        return (None, None);
    };
    // 绝大多数手机写入的时间不带时区，此时用本机时区兜底。
    let local_minutes = (chrono::Local::now().offset().local_minus_utc() / 60) as i16;
    let unix = match UtcOffset::from_minutes(local_minutes) {
        Some(offset) => dt.to_unix_seconds_with_fallback(offset),
        None => dt.to_unix_seconds(),
    };
    let text = unix
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        });
    (text, unix)
}

// ---------------------------------------------------------------------------
// 缩略图
// ---------------------------------------------------------------------------

async fn thumbnail(state: &State, key: u64, tx: &Sender<Evt>) -> Result<()> {
    let Some(stored) = state.scan.stored.get(&key) else {
        return Ok(());
    };
    let storage = &state.storages[stored.storage_index];
    let handle = stored.handle;

    // 优先用设备自带的缩略图（快、流量小）。
    match storage.thumbnail(handle).await {
        Ok(bytes) => match decode_thumbnail(&bytes, THUMB_EDGE) {
            Ok(data) => {
                let _ = tx.send(Evt::Thumbnail {
                    key,
                    result: Ok(data),
                });
                return Ok(());
            }
            Err(e) => {
                let _ = tx.send(Evt::Log(format!(
                    "{} 的 MTP 缩略图无法解码（{e}），改为读取原图",
                    stored.info.filename
                )));
            }
        },
        Err(e) => {
            // 有些设备/格式没有缩略图，回退到读原图再缩小。
            let _ = tx.send(Evt::Log(format!(
                "{} 没有可用缩略图（{e}），改为读取原图",
                stored.info.filename
            )));
        }
    }

    if stored.info.size > THUMB_FALLBACK_MAX_BYTES {
        let _ = tx.send(Evt::Thumbnail {
            key,
            result: Err("文件过大，已跳过缩略图".to_string()),
        });
        return Ok(());
    }

    let result = match storage.download_to_vec(handle).await {
        Ok(bytes) => decode_thumbnail(&bytes, THUMB_EDGE).map_err(|e| e.to_string()),
        Err(e) => Err(format!("读取失败: {e}")),
    };
    let _ = tx.send(Evt::Thumbnail { key, result });
    Ok(())
}

// ---------------------------------------------------------------------------
// EXIF
// ---------------------------------------------------------------------------

/// 读取并解析一张照片的 EXIF，结果通过 [`Evt::Exif`] 回给界面。
async fn load_exif(state: &State, key: u64, tx: &Sender<Evt>) -> Result<()> {
    let Some(stored) = state.scan.stored.get(&key) else {
        return Ok(());
    };
    let storage = &state.storages[stored.storage_index];
    let result = fetch_exif(storage, stored.handle, stored.info.size).await;
    let _ = tx.send(Evt::Exif { key, result });
    Ok(())
}

/// 真正去设备上取 EXIF：先只读文件头（快），不行再退化成整张下载。
///
/// 上传流程和 CLI 诊断都用这一个函数，保证现场跑的代码路径一致。
pub(crate) async fn fetch_exif(
    storage: &Storage,
    handle: ObjectHandle,
    size: u64,
) -> Result<ExifData, String> {
    let mut last_err: String;

    // 1) 只读文件头。EXIF 的 APP1 段最大 64KB，所以 128KB 的文件头对 JPEG 一定是够的。
    let head_len = if size == 0 {
        EXIF_HEAD_BYTES
    } else {
        size.min(EXIF_HEAD_BYTES)
    } as u32;
    let mut maybe_deeper = true;
    match storage.read_range(handle, 0, head_len).await {
        Ok(bytes) => {
            let format = exif::sniff_format(&bytes);
            match exif::parse(&bytes) {
                Ok(data) => return Ok(data),
                Err(_) => last_err = format!("{format} 文件里没有 EXIF"),
            }
            // JPEG/GIF/BMP 的 EXIF 只可能在文件头里；既然没有就别白下载整张了。
            // HEIC/AVIF/WebP/PNG 的元数据可能排在后面，才值得再整张读一次。
            maybe_deeper = matches!(
                format,
                "HEIC/HEIF/MP4 系" | "AVIF" | "WebP" | "PNG" | "未知格式"
            );
        }
        Err(e) => last_err = format!("读取文件头失败: {e}"),
    }

    if !maybe_deeper {
        return Err(last_err);
    }

    // 2) 退化成整张下载再试（HEIC 等的 EXIF 不在文件头）。
    if size == 0 || size <= EXIF_FULL_MAX_BYTES {
        match storage.download_to_vec(handle).await {
            Ok(bytes) => match exif::parse(&bytes) {
                Ok(data) => return Ok(data),
                Err(e) => last_err = format!("{e}"),
            },
            Err(e) => last_err = format!("读取整张照片失败: {e}"),
        }
    } else {
        last_err.push_str(&format!(
            "（文件 {} 超过整张读取上限，未读到 EXIF）",
            crate::thumbs::human_bytes(size)
        ));
    }

    Err(last_err)
}

// ---------------------------------------------------------------------------
// 上传
// ---------------------------------------------------------------------------

/// 逐个上传选中的照片：**同一个接口**，元数据必发，文件可选。
async fn upload(state: &State, keys: Vec<u64>, cfg: UploadConfig, tx: &Sender<Evt>) -> Result<()> {
    if keys.is_empty() {
        let _ = tx.send(Evt::Log("没有选中任何照片".to_string()));
        return Ok(());
    }
    if cfg.base_url.trim().is_empty() {
        let _ = tx.send(Evt::Log("服务器地址为空".to_string()));
        return Ok(());
    }

    let endpoint = cfg.endpoint();
    let uploader = Uploader::new(&cfg)?;
    let total = keys.len();
    let _ = tx.send(Evt::Log(format!(
        "开始上传 {total} 张照片 → {endpoint}（{}）",
        if cfg.upload_file {
            "元数据 + 照片文件"
        } else {
            "仅元数据"
        }
    )));
    let _ = tx.send(Evt::UploadStarted { total });

    let device_json = serde_json::to_value(&state.device_summary).unwrap_or_default();
    let mut ok = 0usize;
    let mut failed = 0usize;

    for (i, key) in keys.iter().enumerate() {
        let Some(stored) = state.scan.stored.get(key) else {
            continue;
        };
        let photo = &stored.info;
        let storage_json = state
            .storage_meta
            .get(stored.storage_index)
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or_default();

        // 1) 需要的话先把照片从手机读出来。
        let mut file: Option<PhotoFile> = None;
        let mut note = String::new();
        if cfg.upload_file {
            let too_big = cfg.max_file_bytes > 0 && photo.size > cfg.max_file_bytes;
            if too_big {
                note = format!(
                    "（超过 {}，本次只上传信息）",
                    crate::thumbs::human_bytes(cfg.max_file_bytes)
                );
            } else {
                let storage = &state.storages[stored.storage_index];
                match storage.download_to_vec(stored.handle).await {
                    Ok(bytes) => {
                        file = Some(PhotoFile {
                            filename: photo.filename.clone(),
                            mime: photo.mime_type.clone(),
                            bytes,
                        });
                    }
                    Err(e) => {
                        note = format!("（读取照片失败，只上传信息: {e}）");
                    }
                }
            }
        }

        // 2) 需要的话把 EXIF 也读出来（会一起发给服务器）。
        let exif = if cfg.include_exif {
            let storage = &state.storages[stored.storage_index];
            match fetch_exif(storage, stored.handle, photo.size).await {
                Ok(data) => Some(data),
                Err(e) => {
                    if !note.is_empty() {
                        note.push(' ');
                    }
                    note.push_str(&format!("（无 EXIF: {e}）"));
                    None
                }
            }
        } else {
            None
        };
        let exif_note = match &exif {
            Some(data) => format!("EXIF {} 项", data.field_count),
            None => "无 EXIF".to_string(),
        };

        // 3) 同一接口 POST。
        let file_included = file.is_some();
        let photo_json = photo_metadata_json(photo, exif.as_ref());
        let metadata = build_photo_metadata(&device_json, &storage_json, &photo_json, file_included);
        let (success, message) = match uploader.post_photo(&cfg, &metadata, file).await {
            Ok(outcome) if outcome.is_success() => {
                ok += 1;
                (
                    true,
                    format!(
                        "HTTP {} · {}ms · {} {} {}",
                        outcome.status,
                        outcome.elapsed_ms,
                        exif_note,
                        note,
                        outcome.brief_body()
                    ),
                )
            }
            Ok(outcome) => {
                failed += 1;
                (
                    false,
                    format!("HTTP {} {} {}", outcome.status, note, outcome.brief_body()),
                )
            }
            Err(e) => {
                failed += 1;
                (false, format!("发送失败: {e:#} {note}"))
            }
        };

        let _ = tx.send(Evt::UploadProgress {
            done: i + 1,
            total,
            filename: photo.filename.clone(),
            ok: success,
            message,
        });
    }

    let _ = tx.send(Evt::Log(format!("上传完成: 成功 {ok}，失败 {failed}")));
    let _ = tx.send(Evt::UploadFinished { ok, failed });
    Ok(())
}

/// 把照片的全部可读信息（含 EXIF）整理成上传用的 JSON。
fn photo_metadata_json(p: &PhotoInfo, exif: Option<&ExifData>) -> serde_json::Value {
    let mut value = serde_json::json!({
        "object_handle": p.handle,
        "parent_handle": p.parent_handle,
        "storage_id": p.storage_id,
        "storage_description": p.storage_desc,
        "filename": p.filename,
        "relative_path": p.normalized_path(),
        "size_bytes": p.size,
        "mime_type": p.mime_type,
        "format_code": p.format_code,
        "width": p.width,
        "height": p.height,
        "created_at": p.created,
        "modified_at": p.modified,
        "created_unix": p.created_unix,
        "modified_unix": p.modified_unix,
    });

    // EXIF：读到了就带上全部字段 + GPS；没读到就显式给 null，服务端好判断。
    let exif_value = match exif {
        Some(data) => serde_json::to_value(data).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("exif".to_string(), exif_value);
    }
    value
}
