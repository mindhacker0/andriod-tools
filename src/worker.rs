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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mtp_rs::mtp::{MtpDevice, ObjectHandle, Storage};
use mtp_rs::UtcOffset;

use crate::cache::PhotoCache;
use crate::exif::{self, ExifData};
use crate::model::{
    DeviceSummary, ExifState, PhotoInfo, ScanScope, ScanStats, StorageSummary, ThumbState,
    UploadConfig,
};
use crate::prepare;
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
    /// 取一张照片的 EXIF：先把照片下载到本地缓存，再解析**缓存文件**的 EXIF。
    /// `force = true` 时忽略已有缓存，重新从手机下载。
    Exif { key: u64, force: bool },
    /// 上传指定照片（元数据必传，文件是否上传由 `cfg.upload_file` 决定）。
    /// 文件会先剥离额外信息（EXIF/GPS/注释…）并做无损压缩，再上传。
    Upload { keys: Vec<u64>, cfg: UploadConfig },
    /// 清空本地照片缓存。
    ClearCache,
    /// 查一下缓存占用并回报给界面。
    RefreshCacheStats,
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
    /// 本地缓存占用情况。
    CacheStats {
        bytes: u64,
        files: usize,
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
    /// 最近一次扫描用的范围；[`recover_session`] 重连后照原范围重扫。
    last_scope: Option<ScanScope>,
    /// 界面重绘回调：自动重连里的“安静等待”用它保持界面不假死。
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 最近一次自动重连的时间；用来给自动重连加冷却，避免排队中的多个读取各自重连一遍。
    last_recovery_attempt: Option<Instant>,
    /// 设备自带缩略图资源（`WPD_RESOURCE_THUMBNAIL`）是否已被证明不可用。
    ///
    /// 有些机型/驱动会对它报设备级错误（如 0x80070141）；既然一次不行，就没必要每张照片都试一遍。
    thumbnail_resource_bad: bool,
    /// 最近一次向用户提示“设备够不着”的时间；同一类错误只提醒一次，别刷屏。
    unreachable_last: Option<Instant>,
    /// 本地照片缓存：原图只从手机下载一次，EXIF 解析与上传都读它。
    cache: PhotoCache,
    /// 已经解析好的 EXIF（key → 结果），上传时直接复用，避免重复解析。
    exif: HashMap<u64, Result<ExifData, String>>,
}

impl State {
    /// 是否允许再自动重连一次。
    ///
    /// 会话失效时往往有多个读取（最多 8 个缩略图 + EXIF）在排队，如果每个失败都各自
    /// 重连一轮，会变成“重连风暴”。冷却期内只允许第一个失败触发重连，其余等它好。
    fn recovery_allowed(&mut self) -> bool {
        match self.last_recovery_attempt {
            Some(t) if t.elapsed() < Duration::from_secs(60) => false,
            _ => {
                self.last_recovery_attempt = Some(Instant::now());
                true
            }
        }
    }

    /// 是否该就 `ERROR_DEVICE_UNREACHABLE` 提醒用户一次（两分钟内只提醒一次）。
    fn should_report_unreachable(&mut self) -> bool {
        match self.unreachable_last {
            Some(t) if t.elapsed() < Duration::from_secs(120) => false,
            _ => {
                self.unreachable_last = Some(Instant::now());
                true
            }
        }
    }
}

/// 启动工作线程。
pub fn spawn(repaint: impl Fn() + Send + Sync + 'static) -> WorkerHandle {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (evt_tx, evt_rx) = channel::<Evt>();
    let cancel = Arc::new(AtomicBool::new(false));

    let cancel_for_thread = cancel.clone();
    let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);

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
            state.repaint = Some(Arc::clone(&repaint));
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
        Cmd::Scan { scope } => {
            state.last_scope = Some(scope);
            scan(state, scope, tx, cancel).await
        }
        Cmd::Thumbnail { key } => thumbnail(state, key, tx, cancel).await,
        Cmd::Exif { key, force } => load_exif(state, key, force, tx, cancel).await,
        Cmd::Upload { keys, cfg } => upload(state, keys, cfg, tx, cancel).await,
        Cmd::ClearCache => clear_cache(state, tx),
        Cmd::RefreshCacheStats => {
            send_cache_stats(state, tx);
            Ok(())
        }
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

/// 打开设备并读取存储分区（**不**向界面发 `Evt::Connected`）。
///
/// Windows 上手机枚举为 **WPD** 设备，而 USB 描述符里的序列号往往与 WPD 序列号不同，
/// 所以打开顺序按“可靠性从高到低”排：
///
/// 1. **USB 位置** —— `open_by_location` 会用 VID/PID 关联到 WPD 设备，这是 Windows 上最稳的路径；
/// 2. **序列号** —— 当 WPD 序列号与 USB 序列号一致时才有效；
/// 3. **第一台设备** —— 直接用 WPD 枚举，适合设备列表为空（nusb 认不到）的情况。
///
/// 单独拆出这一层是为了让 [`recover_session`] 复用：自动重连时不能再发
/// `Evt::Connected`，否则界面会把它当作一次新的手动连接而再触发一遍扫描。
async fn open_session(
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
    // 新会话重新评估“设备自带缩略图资源”是否可用。
    state.thumbnail_resource_bad = false;

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
    Ok(())
}

/// 连接设备：打开会话并通知界面（界面收到 `Evt::Connected` 后会自动开始扫描）。
async fn connect(
    state: &mut State,
    requested: Option<DeviceSummary>,
    tx: &Sender<Evt>,
) -> Result<()> {
    open_session(state, requested, tx).await?;
    // 手动连接成功即视为“用户重新来过了”，清掉自动重连的冷却。
    state.last_recovery_attempt = None;
    // 没有任何存储分区时 `open_session` 已经发过错误提示，别再当成连接成功。
    if state.storages.is_empty() {
        return Ok(());
    }
    let _ = tx.send(Evt::Connected {
        device: state.device_summary.clone(),
        storages: state.storage_meta.clone(),
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

/// Windows `ERROR_DEVICE_UNREACHABLE`（`0x80070141`）：设备“够不着”，传输根本建不起来。
///
/// 这是**设备 / USB 通道**层面的错误，不是程序里的会话过期：在资源管理器里从手机复制一张
/// 照片同样会失败。重开会话救不回它，只能先把设备 / 线 / 口弄好，所以这里不做自动重连，
/// 只给用户一次清晰的提示（`mtp-rs` 把它归为未映射的 HRESULT，原样透出中文系统消息）。
fn is_device_unreachable(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("0x80070141") || m.contains("无法连接至设备") || m.contains("device is unreachable")
}

/// `ERROR_DEVICE_UNREACHABLE` 的处理建议（只在第一次遇到时弹一次红色横幅）。
const DEVICE_UNREACHABLE_HINT: &str = "\
读取手机数据失败：Windows 报 ERROR_DEVICE_UNREACHABLE（0x80070141）——“设备够不着”。\n\
这是设备 / USB 通道层面的问题，程序修不了：在资源管理器里从手机复制一张照片到电脑同样会失败。\n\
请按顺序排查：① 拔线重插并重选“文件传输 / MTP”；② 换 USB 口（优先主板后置 / USB 2.0，别用 Hub）；\n\
③ 换一根能传数据的线；④ 关掉其它占用手机的软件；⑤ 重启手机。\n\
先在资源管理器里能成功复制一张照片，再回到本程序点「🔌 连接」。";

/// 判断一条设备错误是否意味着“会话已失效、需要重新打开设备”。
///
/// 这些是 mtp-rs 明说“reopen to continue”的情形：设备被重置（Android 取消读的楔子）、
/// 设备断开、或对象句柄被设备重新编号。重开 + 重新枚举即可继续。
fn is_session_lost(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("device disconnected")
        || m.contains("device was reset")
        || m.contains("stale object handle")
}

/// 会话失效后的自愈：丢弃旧会话 → 静默几秒 → 空闲间隔重开 → 重新扫描重建句柄表。
///
/// 之所以要重扫：mtp-rs 的 handle 是 WPD 对象 id 的**确定性哈希**，重扫后同一张照片的
/// handle/key 与原来完全一致，所以界面上的勾选、缩略图、详情都能原样保留。
///
/// 重开前的“安静期”和“间隔重试”是 mtp-rs 明确要求的：设备刚被重置时立刻连打
/// close/open 反而会把它再顶死，所以前几次失败属于正常现象。
async fn recover_session(state: &mut State, tx: &Sender<Evt>, cancel: &AtomicBool) -> Result<()> {
    let summary = state.device_summary.clone();
    let scope = state.last_scope;
    let repaint = state.repaint.clone();

    // 1) 静默丢弃旧会话：不发 Evt::Disconnected，界面订阅的连接状态保持不变。
    if let Some(device) = state.device.take() {
        let _ = tokio::time::timeout(Duration::from_secs(5), device.close()).await;
    }
    state.storages.clear();
    state.storage_meta.clear();
    state.scan.stored.clear();

    // 2) 空闲间隔地重开，前几次失败是正常的。
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..4u64 {
        quiet_pause(2 + attempt, repaint.as_ref()).await;
        match open_session(state, Some(summary.clone()), tx).await {
            Ok(()) if !state.storages.is_empty() => {
                last_err = None;
                break;
            }
            Ok(()) => last_err = Some(anyhow::anyhow!("重连后设备没有可用的存储分区")),
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(e) = last_err {
        return Err(e.context("设备会话失效后自动重连失败，请拔插一次数据线后手动重连"));
    }

    // 3) 重新枚举，重建丢掉的句柄映射（key/handle 不变）。
    if let Some(scope) = scope {
        let _ = tx.send(Evt::Log(
            "已重新连接设备，正在重新扫描以恢复照片列表…".to_string(),
        ));
        scan(state, scope, tx, cancel).await?;
    }
    Ok(())
}

/// 安静地等 `secs` 秒，期间定期重绘界面（重连时窗口不至于看起来像卡死）。
async fn quiet_pause(secs: u64, repaint: Option<&Arc<dyn Fn() + Send + Sync>>) {
    for _ in 0..secs.saturating_mul(5) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(repaint) = repaint {
            repaint();
        }
    }
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

async fn thumbnail(state: &mut State, key: u64, tx: &Sender<Evt>, cancel: &AtomicBool) -> Result<()> {
    let mut result = read_thumbnail(state, key, tx).await;
    if let Err(msg) = &result {
        if is_device_unreachable(msg) {
            // 设备 / 线 / 口层面的问题，重连也救不回来，提醒用户一次就好。
            if state.should_report_unreachable() {
                let _ = tx.send(Evt::Error(DEVICE_UNREACHABLE_HINT.to_string()));
            }
        } else if is_session_lost(msg) && state.recovery_allowed() {
            // 会话失效时重连一次再读：读的是设备同一张照片，句柄在重扫后不变。
            let _ = tx.send(Evt::Log(format!(
                "读取缩略图失败（{msg}），疑似设备会话已失效，正在自动重连…"
            )));
            match recover_session(state, tx, cancel).await {
                Ok(()) => result = read_thumbnail(state, key, tx).await,
                Err(e) => result = Err(format!("{e:#}")),
            }
        }
    }
    let _ = tx.send(Evt::Thumbnail { key, result });
    Ok(())
}

/// 取一张照片的缩略图字节并解码。
///
/// 只负责“一次读取”：优先用设备自带缩略图，失败就退化成读原图再缩小。
/// 会话级别的失败由 [`thumbnail`] 负责重连后重试。
async fn read_thumbnail(state: &mut State, key: u64, tx: &Sender<Evt>) -> Result<ThumbData, String> {
    let Some(stored) = state.scan.stored.get(&key) else {
        return Err("照片已不在列表中".to_string());
    };
    let storage_index = stored.storage_index;
    let handle = stored.handle;
    let name = stored.info.filename.clone();
    let size = stored.info.size;

    // 优先用设备自带的缩略图（快、流量小）。
    // 一旦发现它对这台设备不可用（例如报 0x80070141），就不再每张都试一遍：既省下一次设备
    // 往返，也不会把日志刷爆。
    if !state.thumbnail_resource_bad {
        let mut mark_bad = false;
        {
            let storage = &state.storages[storage_index];
            match storage.thumbnail(handle).await {
                Ok(bytes) => match decode_thumbnail(&bytes, THUMB_EDGE) {
                    Ok(data) => return Ok(data),
                    Err(e) => {
                        let _ = tx.send(Evt::Log(format!(
                            "{name} 的 MTP 缩略图无法解码（{e}），改为读取原图"
                        )));
                    }
                },
                Err(e) => {
                    let msg = e.to_string();
                    if thumbnail_missing(&msg) {
                        // 只是这种文件没有缩略图，回退到读原图再缩小。
                        let _ = tx.send(Evt::Log(format!(
                            "{name} 没有可用缩略图（{msg}），改为读取原图"
                        )));
                    } else {
                        mark_bad = true;
                        let _ = tx.send(Evt::Log(format!(
                            "设备缩略图资源不可用（{msg}），后续一律改为读取原图再缩放"
                        )));
                    }
                }
            }
        }
        if mark_bad {
            state.thumbnail_resource_bad = true;
        }
    }

    if size > THUMB_FALLBACK_MAX_BYTES {
        return Err("文件过大，已跳过缩略图".to_string());
    }

    let storage = &state.storages[storage_index];
    match storage.download_to_vec(handle).await {
        Ok(bytes) => decode_thumbnail(&bytes, THUMB_EDGE).map_err(|e| e.to_string()),
        Err(e) => Err(format!("读取失败: {e}")),
    }
}

/// 设备只是“这张没有缩略图”（`mtp-rs` 把它们映射成 Unsupported / NotFound），不是设备级故障。
fn thumbnail_missing(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("not supported") || m.contains("unsupported") || m.contains("not found")
}

// ---------------------------------------------------------------------------
// 本地缓存 + EXIF
// ---------------------------------------------------------------------------
//
// 设计要点：**原图只从手机下载一次**。
//
// * 用户在网格里选中一张照片时（详情面板）才触发下载 + 解析 EXIF；
// * 下载下来的原图落在本地缓存目录里；
// * 上传时复用同一份缓存，再从缓存字节里剥元数据 / 无损压缩，最后才 POST。
//
// 这样既满足“只有选中照片才解析 EXIF”，又保证上传前一定做了剥离 + 无损压缩。

/// 把缓存占用回报给界面。
fn send_cache_stats(state: &State, tx: &Sender<Evt>) {
    let (bytes, files) = state.cache.stats();
    let _ = tx.send(Evt::CacheStats { bytes, files });
}

/// 清空缓存（也顺手清掉已解析的 EXIF 记录）。
fn clear_cache(state: &mut State, tx: &Sender<Evt>) -> Result<()> {
    let freed = state.cache.clear();
    state.exif.clear();
    let _ = tx.send(Evt::Log(format!(
        "已清空本地照片缓存，释放 {:.2} MB",
        freed as f64 / 1024.0 / 1024.0
    )));
    send_cache_stats(state, tx);
    Ok(())
}

/// 取一张照片的**原始字节**（未经任何处理），优先用本地缓存。
///
/// * 缓存命中且 `force == false` → 直接读文件，完全不碰设备；
/// * 否则整张从手机下载，写进缓存后返回。
///
/// 会话失效（`stale object handle` 之类）会自动重连并重试一次 —— 重扫后
/// 同一张照片的句柄不变，所以重试用的还是同一个 handle。
async fn ensure_cached(
    state: &mut State,
    photo: &PhotoInfo,
    storage_index: usize,
    handle: ObjectHandle,
    force: bool,
    tx: &Sender<Evt>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let path = state
        .cache
        .path_for(photo.storage_id, photo.handle, photo.size, &photo.filename);

    if !force {
        if let Some(bytes) = state.cache.read(&path) {
            return Ok(bytes);
        }
    }

    let mut result = download_bytes(state, storage_index, handle).await;
    if let Err(msg) = &result {
        if is_device_unreachable(msg) {
            // 设备 / 线 / 口层面的问题，重连也救不回来，提醒用户一次就好。
            if state.should_report_unreachable() {
                let _ = tx.send(Evt::Error(DEVICE_UNREACHABLE_HINT.to_string()));
            }
        } else if is_session_lost(msg) && state.recovery_allowed() {
            let _ = tx.send(Evt::Log(format!(
                "读取 {} 失败（{msg}），疑似设备会话已失效，正在自动重连…",
                photo.filename
            )));
            match recover_session(state, tx, cancel).await {
                Ok(()) => result = download_bytes(state, storage_index, handle).await,
                Err(e) => return Err(format!("{e:#}")),
            }
        }
    }
    let bytes = result?;

    match state.cache.write(&path, &bytes) {
        Ok(()) => {
            let _ = tx.send(Evt::Log(format!(
                "已缓存 {}（{}）",
                photo.filename,
                crate::thumbs::human_bytes(bytes.len() as u64)
            )));
        }
        Err(e) => {
            // 缓存失败不影响这次处理，只是下次还得重新下载。
            let _ = tx.send(Evt::Log(format!("写入本地缓存失败（不影响本次处理）: {e}")));
        }
    }
    send_cache_stats(state, tx);
    Ok(bytes)
}

/// 从设备上整张读一个对象。
async fn download_bytes(
    state: &State,
    storage_index: usize,
    handle: ObjectHandle,
) -> Result<Vec<u8>, String> {
    let Some(storage) = state.storages.get(storage_index) else {
        return Err("存储分区已失效，请重新连接设备".to_string());
    };
    storage
        .download_to_vec(handle)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// 选中某张照片时：把照片下载到本地缓存，再解析**缓存文件**里的 EXIF。
///
/// 结果记在 `state.exif` 里，上传时优先复用，避免重复解析。
async fn load_exif(
    state: &mut State,
    key: u64,
    force: bool,
    tx: &Sender<Evt>,
    cancel: &AtomicBool,
) -> Result<()> {
    let Some(stored) = state.scan.stored.get(&key) else {
        return Ok(());
    };
    let photo = stored.info.clone();
    let storage_index = stored.storage_index;
    let handle = stored.handle;

    let result = match ensure_cached(state, &photo, storage_index, handle, force, tx, cancel).await {
        Ok(bytes) => exif::parse(&bytes).map_err(|e| format!("{e:#}")),
        Err(msg) => Err(msg),
    };
    state.exif.insert(key, result.clone());
    let _ = tx.send(Evt::Exif { key, result });
    Ok(())
}

/// 整张下载一张照片并解析 EXIF（命令行诊断用，保证和界面跑的是同一套读取逻辑）。
pub(crate) async fn fetch_exif(storage: &Storage, handle: ObjectHandle) -> Result<ExifData, String> {
    let bytes = storage
        .download_to_vec(handle)
        .await
        .map_err(|e| format!("读取照片失败: {e:#}"))?;
    exif::parse(&bytes).map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// 上传
// ---------------------------------------------------------------------------

/// 逐个上传选中的照片：**同一个接口**，元数据必发，文件可选。
///
/// 文件走这样一条流水线（缓存 → 剥离 → 无损压缩 → 上传）：
///
/// ```text
/// 本地缓存（没有就从手机下载一次）
///    └─ 原始字节 ──┬─→ 解析 EXIF（放进元数据，解析的是**原始**文件）
///                  └─→ 剥掉 EXIF/XMP/注释… + 无损压缩 ──→ multipart 的 file 字段
/// ```
async fn upload(
    state: &mut State,
    keys: Vec<u64>,
    cfg: UploadConfig,
    tx: &Sender<Evt>,
    cancel: &AtomicBool,
) -> Result<()> {
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
            "元数据 + 照片文件（已剥离额外信息并无损压缩）"
        } else {
            "仅元数据"
        }
    )));
    let _ = tx.send(Evt::UploadStarted { total });

    let device_json = serde_json::to_value(&state.device_summary).unwrap_or_default();
    let mut ok = 0usize;
    let mut failed = 0usize;

    for (i, key) in keys.iter().enumerate() {
        let key = *key;
        // 先把要用的字段拷出来，之后才能可变借用 state（读缓存 / 重连）。
        let (photo, storage_index, handle) = match state.scan.stored.get(&key) {
            Some(s) => (s.info.clone(), s.storage_index, s.handle),
            None => continue,
        };
        let storage_json = state
            .storage_meta
            .get(storage_index)
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or_default();

        let mut note = String::new();

        // 1) 拿原图字节（缓存优先，只从手机下载一次）。上传文件要用，读 EXIF 也要用。
        let too_big = cfg.upload_file && cfg.max_file_bytes > 0 && photo.size > cfg.max_file_bytes;
        if too_big {
            note.push_str(&format!(
                "（超过单文件上限 {}，本次只上传信息）",
                crate::thumbs::human_bytes(cfg.max_file_bytes)
            ));
        }
        let want_file = cfg.upload_file && !too_big;
        let mut raw: Option<Vec<u8>> = None;
        if want_file || cfg.include_exif {
            match ensure_cached(state, &photo, storage_index, handle, false, tx, cancel).await {
                Ok(bytes) => raw = Some(bytes),
                Err(e) => note.push_str(&format!("（读取原图失败：{e}）")),
            }
        }

        // 2) EXIF：优先用「选中照片看详情」时已经解析好的结果；没有就从原始字节解析。
        let exif = if cfg.include_exif {
            if let Some(Ok(data)) = state.exif.get(&key) {
                Some(data.clone())
            } else {
                match raw.as_deref().map(exif::parse) {
                    Some(Ok(data)) => {
                        state.exif.insert(key, Ok(data.clone()));
                        Some(data)
                    }
                    Some(Err(e)) => {
                        note.push_str(&format!("（无 EXIF：{e:#}）"));
                        None
                    }
                    None => {
                        note.push_str("（无 EXIF：未取到原图）");
                        None
                    }
                }
            }
        } else {
            None
        };
        let exif_note = match &exif {
            Some(data) => format!("EXIF {} 项", data.field_count),
            None if cfg.include_exif => "无 EXIF".to_string(),
            None => "未读 EXIF".to_string(),
        };

        // 3) 文件：剥离额外信息 + 无损压缩后才上传。
        let mut file: Option<PhotoFile> = None;
        let mut file_json: Option<serde_json::Value> = None;
        if want_file {
            if let Some(bytes) = raw.take() {
                let prepared = prepare::prepare(&photo.filename, &photo.mime_type, bytes);
                if !prepared.note.is_empty() {
                    if !note.is_empty() {
                        note.push(' ');
                    }
                    note.push_str(&format!("（{}）", prepared.note));
                }
                file_json = Some(serde_json::json!({
                    "filename": prepared.filename,
                    "mime_type": prepared.mime,
                    "size_bytes": prepared.bytes.len(),
                    "original_size_bytes": prepared.original_len,
                    "metadata_stripped": prepared.stripped,
                }));
                file = Some(PhotoFile {
                    filename: prepared.filename,
                    mime: prepared.mime,
                    bytes: prepared.bytes,
                });
            }
        }

        // 4) 同一接口 POST。
        let photo_json = photo_metadata_json(&photo, exif.as_ref());
        let metadata = build_photo_metadata(&device_json, &storage_json, &photo_json, file_json);
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
