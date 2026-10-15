//! egui 图形界面。
//!
//! 界面线程只负责渲染与交互；所有设备/网络操作都通过 [`crate::worker`] 的
//! 命令/事件通道交给后台线程执行。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use egui::{
    Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Rect, RichText, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2,
};

use crate::model::{
    ExifState, PhotoInfo, ScanScope, ScanStats, StorageSummary, ThumbState, UploadConfig,
};
use crate::thumbs::{human_bytes, ThumbData};
use crate::worker::{Cmd, Evt, WorkerHandle};

/// 一次最多保留多少张缩略图纹理（超出后按最近最少使用淘汰）。
const MAX_TEXTURES: usize = 500;
/// 每帧最多请求多少张缩略图，避免把设备队列塞满。
const THUMB_REQUESTS_PER_FRAME: usize = 8;
/// 网格右侧额外留出的安全余量（像素），防止浮动滚动条压住最后一列。
const GRID_RIGHT_MARGIN: f32 = 10.0;
/// 缩略图高度占格子宽度的比例。
const CELL_IMAGE_RATIO: f32 = 0.72;
/// 缩略图下方文件名/大小文字区的高度。
const CELL_LABEL_HEIGHT: f32 = 30.0;

/// 一个格子的精确高度（图片 + 文字）。
///
/// **必须**和 [`PhoneToolApp::draw_cell`] 里真正 allocate 的尺寸一致，
/// 否则虚拟滚动的行位置会和实际内容对不上，底部就会出现大片空白。
fn cell_height(cell: f32) -> f32 {
    cell * CELL_IMAGE_RATIO + CELL_LABEL_HEIGHT
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortMode {
    NameAsc,
    NameDesc,
    SizeDesc,
    SizeAsc,
    DateDesc,
    DateAsc,
}

impl SortMode {
    const ALL: [SortMode; 6] = [
        SortMode::DateDesc,
        SortMode::DateAsc,
        SortMode::NameAsc,
        SortMode::NameDesc,
        SortMode::SizeDesc,
        SortMode::SizeAsc,
    ];

    fn label(self) -> &'static str {
        match self {
            SortMode::NameAsc => "文件名 ↑",
            SortMode::NameDesc => "文件名 ↓",
            SortMode::SizeDesc => "大小 ↓",
            SortMode::SizeAsc => "大小 ↑",
            SortMode::DateDesc => "时间 ↓",
            SortMode::DateAsc => "时间 ↑",
        }
    }
}

pub struct PhoneToolApp {
    cmd_tx: Sender<Cmd>,
    evt_rx: std::sync::mpsc::Receiver<Evt>,
    cancel: Arc<std::sync::atomic::AtomicBool>,

    // ── 设备 ──
    devices: Vec<crate::model::DeviceSummary>,
    device_sel: usize,
    connected: Option<crate::model::DeviceSummary>,
    storages: Vec<StorageSummary>,
    connecting: bool,

    // ── 照片 ──
    photos: Vec<PhotoInfo>,
    textures: HashMap<u64, TextureHandle>,
    texture_order: VecDeque<u64>,
    inflight: HashSet<u64>,
    scanning: bool,
    scan_scope: ScanScope,
    scan_stats: Option<ScanStats>,
    scan_progress: (usize, usize, String),

    // ── 过滤 / 排序 ──
    view: Vec<usize>,
    view_dirty: bool,
    filter_text: String,
    filter_min_kb: u64,
    sort_mode: SortMode,

    // ── 上传 ──
    cfg: UploadConfig,
    uploading: bool,
    upload_progress: Option<(usize, usize)>,
    upload_last: Option<String>,
    upload_summary: Option<(usize, usize)>,

    // ── 本地缓存 ──
    cache_bytes: u64,
    cache_files: usize,
    cache_root: String,

    // ── 其它 ──
    detail_key: Option<u64>,
    show_detail: bool,
    log: VecDeque<String>,
    cell_size: f32,
    /// 需要醒目展示的错误（连接失败等）。
    last_error: Option<String>,
    /// 连接开始时间，用于超时兜底。
    connect_started: Option<std::time::Instant>,
    /// 启动后是否自动连接第一台设备。
    auto_connect: bool,
}

impl PhoneToolApp {
    pub fn new(cc: &eframe::CreationContext<'_>, auto_connect: bool) -> Self {
        install_cjk_font(&cc.egui_ctx);

        let ctx = cc.egui_ctx.clone();
        let handle: WorkerHandle = crate::worker::spawn(move || ctx.request_repaint());

        let mut app = Self {
            cmd_tx: handle.tx,
            evt_rx: handle.rx,
            cancel: handle.cancel,

            devices: Vec::new(),
            device_sel: 0,
            connected: None,
            storages: Vec::new(),
            connecting: false,

            photos: Vec::new(),
            textures: HashMap::new(),
            texture_order: VecDeque::new(),
            inflight: HashSet::new(),
            scanning: false,
            scan_scope: ScanScope::Full,
            scan_stats: None,
            scan_progress: (0, 0, String::new()),

            view: Vec::new(),
            view_dirty: true,
            filter_text: String::new(),
            filter_min_kb: 0,
            sort_mode: SortMode::DateDesc,

            cfg: UploadConfig::default(),
            uploading: false,
            upload_progress: None,
            upload_last: None,
            upload_summary: None,

            cache_bytes: 0,
            cache_files: 0,
            cache_root: crate::cache::default_root().display().to_string(),

            detail_key: None,
            show_detail: false,
            log: VecDeque::new(),
            cell_size: 148.0,
            last_error: None,
            connect_started: None,
            auto_connect,
        };
        app.push_log("就绪。请把手机用 USB 连上电脑，USB 用途选择“文件传输 / MTP”，然后点“刷新设备”。");
        println!("phone-tool 已启动，正在枚举 MTP 设备…（Windows 上走系统 WPD，无需驱动）");
        app.send(Cmd::ListDevices);
        app.send(Cmd::RefreshCacheStats);
        app
    }

    fn send(&self, cmd: Cmd) {
        if self.cmd_tx.send(cmd).is_err() {
            // 接收端已退出，界面下次重建即可。
        }
    }

    fn push_log(&mut self, msg: impl Into<String>) {
        let line = format!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg.into());
        self.log.push_back(line);
        while self.log.len() > 500 {
            self.log.pop_front();
        }
    }

    // ------------------------------------------------------------------
    // 事件处理
    // ------------------------------------------------------------------
    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(evt) = self.evt_rx.try_recv() {
            match evt {
                Evt::Log(msg) => self.push_log(msg),
                Evt::Error(msg) => {
                    self.push_log(format!("❗ {msg}"));
                    self.last_error = Some(msg);
                    self.connecting = false;
                    self.connect_started = None;
                }
                Evt::OperationFailed => {
                    self.connecting = false;
                    self.connect_started = None;
                    self.scanning = false;
                    self.uploading = false;
                }
                Evt::Devices(list) => {
                    self.devices = list;
                    self.device_sel = self.device_sel.min(self.devices.len().saturating_sub(1));

                    // --auto-connect：列表一回来就自动连第一台并扫描
                    if self.auto_connect && self.connected.is_none() && !self.connecting {
                        if let Some(device) = self.devices.first().cloned() {
                            self.push_log(format!("自动连接：{}", device.short_label()));
                            self.connecting = true;
                            self.connect_started = Some(std::time::Instant::now());
                            self.send(Cmd::Connect {
                                device: Some(device),
                            });
                        }
                    }
                }
                Evt::Connected { device, storages } => {
                    self.connecting = false;
                    self.connect_started = None;
                    self.last_error = None;
                    self.connected = Some(device);
                    self.storages = storages;
                    // 连上以后立即自动扫一次，不用用户再去点一次按钮。
                    self.push_log(format!(
                        "连接成功，自动开始扫描（{}）。切换范围可以重新扫。",
                        self.scan_scope.label()
                    ));
                    self.send(Cmd::Scan {
                        scope: self.scan_scope,
                    });
                }
                Evt::Disconnected => {
                    self.connected = None;
                    self.storages.clear();
                    self.photos.clear();
                    self.view.clear();
                    self.view_dirty = true;
                    self.textures.clear();
                    self.texture_order.clear();
                    self.inflight.clear();
                }
                Evt::ScanStarted { storages, scope } => {
                    self.scanning = true;
                    self.scan_stats = None;
                    self.scan_progress = (0, 0, String::new());
                    self.photos.clear();
                    self.view.clear();
                    self.view_dirty = true;
                    self.inflight.clear();
                    self.push_log(format!("开始扫描 {storages} 个存储分区（{}）", scope.label()));
                }
                Evt::ScanProgress {
                    folders,
                    photos,
                    current,
                } => {
                    self.scan_progress = (folders, photos, current);
                }
                Evt::ScanFinished(stats) => {
                    self.scanning = false;
                    self.scan_stats = Some(stats);
                }
                Evt::Photos(list) => self.replace_photos(list),
                Evt::Thumbnail { key, result } => {
                    self.inflight.remove(&key);
                    match result {
                        Ok(data) => self.install_texture(ctx, key, data),
                        Err(msg) => {
                            if let Some(p) = self.photos.iter_mut().find(|p| p.key == key) {
                                p.thumb = ThumbState::Failed(msg);
                            }
                        }
                    }
                }
                Evt::Exif { key, result } => {
                    if let Some(p) = self.photos.iter_mut().find(|p| p.key == key) {
                        p.exif = match result {
                            Ok(data) => ExifState::Ready(data),
                            Err(msg) => ExifState::Failed(msg),
                        };
                    }
                }
                Evt::CacheStats { bytes, files } => {
                    self.cache_bytes = bytes;
                    self.cache_files = files;
                }
                Evt::UploadStarted { total } => {
                    self.uploading = true;
                    self.upload_progress = Some((0, total));
                    self.upload_summary = None;
                }
                Evt::UploadProgress {
                    done,
                    total,
                    filename,
                    ok,
                    message,
                } => {
                    self.upload_progress = Some((done, total));
                    self.upload_last = Some(format!("{filename} → {message}"));
                    self.push_log(format!(
                        "[{done}/{total}] {} {filename} — {message}",
                        if ok { "✅" } else { "❌" }
                    ));
                }
                Evt::UploadFinished { ok, failed } => {
                    self.uploading = false;
                    self.upload_summary = Some((ok, failed));
                    self.push_log(format!("上传结束：成功 {ok}，失败 {failed}"));
                }
            }
        }
    }

    /// 全量替换照片列表，同时尽量保留上次的勾选状态与缩略图。
    fn replace_photos(&mut self, mut incoming: Vec<PhotoInfo>) {
        let old: HashMap<u64, (bool, ThumbState, ExifState)> = self
            .photos
            .iter()
            .map(|p| (p.key, (p.picked, p.thumb.clone(), p.exif.clone())))
            .collect();
        for p in &mut incoming {
            if let Some((picked, thumb, exif)) = old.get(&p.key) {
                p.picked = *picked;
                p.thumb = thumb.clone();
                p.exif = exif.clone();
            }
        }
        if incoming.len() != self.photos.len() {
            self.view_dirty = true;
        }
        self.photos = incoming;
        self.view_dirty = true;
    }

    fn install_texture(&mut self, ctx: &egui::Context, key: u64, data: ThumbData) {
        let image =
            egui::ColorImage::from_rgba_unmultiplied([data.width as usize, data.height as usize], &data.rgba);
        let tex = ctx.load_texture(format!("thumb-{key}"), image, TextureOptions::LINEAR);
        self.textures.insert(key, tex);
        self.texture_order.push_back(key);

        // 超量后按插入顺序淘汰最早的纹理，控制显存占用。
        while self.texture_order.len() > MAX_TEXTURES {
            if let Some(old) = self.texture_order.pop_front() {
                self.textures.remove(&old);
                if let Some(p) = self.photos.iter_mut().find(|p| p.key == old) {
                    if p.thumb == ThumbState::Ready {
                        p.thumb = ThumbState::Idle;
                    }
                }
            }
        }
        if let Some(p) = self.photos.iter_mut().find(|p| p.key == key) {
            p.thumb = ThumbState::Ready;
        }
    }

    /// 重新计算“筛选 + 排序”后的下标列表。
    fn rebuild_view(&mut self) {
        let needle = self.filter_text.trim().to_lowercase();
        let min_bytes = self.filter_min_kb.saturating_mul(1024);
        let mut idx: Vec<usize> = self
            .photos
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                let name_ok = needle.is_empty()
                    || p.filename.to_lowercase().contains(&needle)
                    || p.rel_path.to_lowercase().contains(&needle);
                name_ok && p.size >= min_bytes
            })
            .map(|(i, _)| i)
            .collect();

        let photos = &self.photos;
        match self.sort_mode {
            SortMode::NameAsc => idx.sort_by(|a, b| photos[*a].filename.cmp(&photos[*b].filename)),
            SortMode::NameDesc => idx.sort_by(|a, b| photos[*b].filename.cmp(&photos[*a].filename)),
            SortMode::SizeDesc => idx.sort_by(|a, b| photos[*b].size.cmp(&photos[*a].size)),
            SortMode::SizeAsc => idx.sort_by(|a, b| photos[*a].size.cmp(&photos[*b].size)),
            SortMode::DateDesc => idx.sort_by(|a, b| photos[*b].modified_unix.cmp(&photos[*a].modified_unix)),
            SortMode::DateAsc => idx.sort_by(|a, b| photos[*a].modified_unix.cmp(&photos[*b].modified_unix)),
        }
        self.view = idx;
        self.view_dirty = false;
    }

    fn picked_keys(&self) -> Vec<u64> {
        self.photos
            .iter()
            .filter(|p| p.picked)
            .map(|p| p.key)
            .collect()
    }

    fn set_all_in_view(&mut self, picked: bool) {
        let keys: Vec<u64> = self.view.iter().map(|i| self.photos[*i].key).collect();
        for p in self.photos.iter_mut() {
            if keys.contains(&p.key) {
                p.picked = picked;
            }
        }
    }

    fn invert_in_view(&mut self) {
        let keys: Vec<u64> = self.view.iter().map(|i| self.photos[*i].key).collect();
        for p in self.photos.iter_mut() {
            if keys.contains(&p.key) {
                p.picked = !p.picked;
            }
        }
    }

    // ------------------------------------------------------------------
    // 各个面板
    // ------------------------------------------------------------------
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading("📱 手机照片上传工具");
                ui.separator();

                if ui.button("🔄 刷新设备").clicked() {
                    self.send(Cmd::ListDevices);
                }

                ui.label("设备:");
                let selected_text = self
                    .devices
                    .get(self.device_sel)
                    .map(|d| d.short_label())
                    .unwrap_or_else(|| "（未发现设备）".to_string());
                egui::ComboBox::from_id_salt("device-combo")
                    .selected_text(selected_text)
                    .width(340.0)
                    .show_ui(ui, |ui| {
                        for (i, d) in self.devices.iter().enumerate() {
                            ui.selectable_value(&mut self.device_sel, i, d.short_label());
                        }
                    });

                let busy = self.connecting || self.scanning || self.uploading;
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button("🔌 连接").clicked() {
                        match self.devices.get(self.device_sel).cloned() {
                            Some(device) => {
                                self.connecting = true;
                                self.connect_started = Some(std::time::Instant::now());
                                self.last_error = None;
                                self.send(Cmd::Connect {
                                    device: Some(device),
                                });
                            }
                            None => {
                                self.push_log("设备列表为空，可点“⚡ 打开第一台”试试。");
                            }
                        }
                    }
                    if ui
                        .button("⚡ 打开第一台")
                        .on_hover_text(
                            "不看设备列表，直接枚举 Windows 的 WPD 设备并打开第一台。\n\
                             注意：如果电脑上还插着 U 盘 / 读卡器，可能打开的是它们而不是手机，\
                             所以优先用左边的「🔌 连接」。",
                        )
                        .clicked()
                    {
                        self.connecting = true;
                        self.connect_started = Some(std::time::Instant::now());
                        self.last_error = None;
                        self.send(Cmd::Connect { device: None });
                    }
                    if ui
                        .add_enabled(self.connected.is_some(), egui::Button::new("⏏ 断开"))
                        .clicked()
                    {
                        self.send(Cmd::Disconnect);
                    }
                });

                if self.connecting {
                    ui.spinner();
                    ui.label("正在打开设备…（若手机弹出授权提示，请在手机上允许）");
                }

                ui.separator();
                match &self.connected {
                    Some(d) => {
                        ui.colored_label(
                            Color32::from_rgb(80, 180, 120),
                            format!("已连接：{} {}", d.manufacturer, d.model),
                        );
                    }
                    None => {
                        ui.colored_label(Color32::GRAY, "未连接");
                    }
                }
            });

            // 错误横幅：连接失败等信息直接紫在最上面，不让用户去翻日志
            if self.last_error.is_some() {
                let err = self.last_error.clone().unwrap_or_default();
                let mut dismiss = false;
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(230, 96, 96), "⚠ 出错了");
                    if ui.small_button("关闭").clicked() {
                        dismiss = true;
                    }
                });
                ui.colored_label(Color32::from_rgb(230, 96, 96), err);
                if dismiss {
                    self.last_error = None;
                }
            }
            ui.add_space(4.0);
        });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("left")
            .resizable(true)
            .default_size(330.0)
            .min_size(240.0)
            .max_size(560.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                    ui.heading("① 扫描照片");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("scope")
                            .selected_text(self.scan_scope.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.scan_scope, ScanScope::Full, ScanScope::Full.label());
                                ui.selectable_value(
                                    &mut self.scan_scope,
                                    ScanScope::CommonFolders,
                                    ScanScope::CommonFolders.label(),
                                );
                            });
                    });
                    ui.horizontal(|ui| {
                        let can_scan = self.connected.is_some() && !self.scanning && !self.uploading;
                        if ui.add_enabled(can_scan, egui::Button::new("🔍 扫描照片")).clicked() {
                            self.send(Cmd::Scan { scope: self.scan_scope });
                        }
                        if ui.add_enabled(self.scanning, egui::Button::new("⏹ 取消扫描")).clicked() {
                            self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    });
                    if self.scanning {
                        let (folders, photos, current) = self.scan_progress.clone();
                        ui.label(format!("已遍历目录 {folders}，找到照片 {photos}"));
                        ui.label(RichText::new(current).small().weak());
                        ui.spinner();
                    }
                    if let Some(s) = &self.scan_stats {
                        ui.label(format!(
                            "完成：{} 张照片 / {} 个目录，看到 {} 个对象，失败 {}，耗时 {:.1}s{}",
                            s.photos_found,
                            s.folders_scanned,
                            s.objects_seen,
                            s.folders_failed,
                            s.elapsed_ms as f64 / 1000.0,
                            if s.cancelled { "（已取消）" } else { "" }
                        ));
                    }
                    if !self.storages.is_empty() {
                        ui.collapsing("存储分区", |ui| {
                            for s in &self.storages {
                                ui.label(format!(
                                    "{} · 可用 {}/{}",
                                    s.description,
                                    human_bytes(s.free_space_bytes),
                                    human_bytes(s.max_capacity_bytes)
                                ));
                            }
                        });
                    }

                    ui.separator();
                    ui.heading("② 筛选与选择");
                    ui.horizontal(|ui| {
                        ui.label("文件名包含:");
                        if ui.text_edit_singleline(&mut self.filter_text).changed() {
                            self.view_dirty = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("最小大小:");
                        let mut mb = (self.filter_min_kb / 1024) as u32;
                        if ui
                            .add(egui::DragValue::new(&mut mb).range(0..=200).suffix(" MB"))
                            .changed()
                        {
                            self.filter_min_kb = u64::from(mb) * 1024;
                            self.view_dirty = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("排序:");
                        egui::ComboBox::from_id_salt("sort")
                            .selected_text(self.sort_mode.label())
                            .show_ui(ui, |ui| {
                                for m in SortMode::ALL {
                                    if ui
                                        .selectable_value(&mut self.sort_mode, m, m.label())
                                        .clicked()
                                    {
                                        self.view_dirty = true;
                                    }
                                }
                            });
                    });

                    let picked = self.photos.iter().filter(|p| p.picked).count();
                    ui.label(format!(
                        "共 {} 张照片，筛选出 {} 张，已选 {} 张",
                        self.photos.len(),
                        self.view.len(),
                        picked
                    ));
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("全选").clicked() {
                            self.set_all_in_view(true);
                        }
                        if ui.button("取消全选").clicked() {
                            self.set_all_in_view(false);
                        }
                        if ui.button("反选").clicked() {
                            self.invert_in_view();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("缩略图大小:");
                        ui.add(egui::Slider::new(&mut self.cell_size, 96.0..=260.0).show_value(false));
                    });

                    ui.separator();
                    ui.heading("③ 服务器接口");
                    ui.label("所有照片都通过**同一个接口**上传。");
                    egui::Grid::new("upload-cfg")
                        .num_columns(2)
                        .spacing([6.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("服务器地址");
                            ui.text_edit_singleline(&mut self.cfg.base_url);
                            ui.end_row();

                            ui.label("接口路径");
                            ui.text_edit_singleline(&mut self.cfg.api_path);
                            ui.end_row();

                            ui.label("Token");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.cfg.token)
                                    .password(true)
                                    .hint_text("可选，Bearer"),
                            );
                            ui.end_row();

                            ui.label("元数据字段");
                            ui.text_edit_singleline(&mut self.cfg.field_metadata);
                            ui.end_row();

                            ui.label("文件字段");
                            ui.text_edit_singleline(&mut self.cfg.field_file);
                            ui.end_row();

                            ui.label("超时(秒)");
                            let mut timeout = self.cfg.timeout_secs as u32;
                            if ui
                                .add(egui::DragValue::new(&mut timeout).range(5..=600))
                                .changed()
                            {
                                self.cfg.timeout_secs = u64::from(timeout);
                            }
                            ui.end_row();

                            ui.label("失败重试");
                            ui.add(egui::DragValue::new(&mut self.cfg.retries).range(0..=5));
                            ui.end_row();

                            ui.label("单文件上限");
                            let mut max_mb = (self.cfg.max_file_bytes / 1024 / 1024) as u32;
                            if ui
                                .add(egui::DragValue::new(&mut max_mb).range(0..=4096).suffix(" MB"))
                                .changed()
                            {
                                self.cfg.max_file_bytes = u64::from(max_mb) * 1024 * 1024;
                            }
                            ui.end_row();
                        });
                    ui.checkbox(&mut self.cfg.upload_file, "上传照片文件本身（关闭则只上传信息）");
                    ui.checkbox(&mut self.cfg.include_exif, "读取并上传 EXIF 元数据（相机/时间/曝光/GPS）");
                    ui.label(
                        RichText::new(
                            "上传前会自动剥离 EXIF/GPS/注释等额外信息并对文件做无损压缩（画质不变）。",
                        )
                        .small()
                        .weak(),
                    );
                    ui.label(RichText::new(format!("POST {}", self.cfg.endpoint())).small().weak());

                    ui.add_space(6.0);
                    let ready = self.connected.is_some() && !self.uploading;
                    let picked = self.photos.iter().filter(|p| p.picked).count();
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                ready && picked > 0,
                                egui::Button::new("☁ 上传选中照片（信息+文件）"),
                            )
                            .clicked()
                        {
                            let mut cfg = self.cfg.clone();
                            cfg.upload_file = true;
                            self.start_upload(cfg);
                        }
                        if ui
                            .add_enabled(ready && picked > 0, egui::Button::new("📄 仅上传选中信息"))
                            .clicked()
                        {
                            let mut cfg = self.cfg.clone();
                            cfg.upload_file = false;
                            self.start_upload(cfg);
                        }
                    });
                    if self.uploading {
                        if let Some((done, total)) = self.upload_progress {
                            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
                            ui.add(egui::ProgressBar::new(frac).show_percentage());
                            ui.label(format!("上传中 {done}/{total}"));
                        }
                        if let Some(last) = &self.upload_last {
                            ui.label(RichText::new(last).small().weak());
                        }
                    }
                    if let Some((ok, failed)) = self.upload_summary {
                        ui.colored_label(
                            if failed == 0 { Color32::from_rgb(80, 180, 120) } else { Color32::from_rgb(220, 140, 60) },
                            format!("上次上传：成功 {ok}，失败 {failed}"),
                        );
                    }

                    ui.separator();
                    ui.heading("④ 本地缓存");
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "已缓存 {} 个文件，占用 {}",
                            self.cache_files,
                            human_bytes(self.cache_bytes)
                        ));
                        if ui.small_button("刷新").clicked() {
                            self.send(Cmd::RefreshCacheStats);
                        }
                    });
                    ui.label(
                        RichText::new(
                            "选中照片时会把原图下载到这里并解析 EXIF；上传时复用同一份，避免重复从手机读取。",
                        )
                        .small()
                        .weak(),
                    );
                    ui.label(RichText::new(self.cache_root.as_str()).small().weak());
                    if ui
                        .add_enabled(self.cache_files > 0, egui::Button::new("🗑 清空缓存"))
                        .clicked()
                    {
                        self.send(Cmd::ClearCache);
                    }
                });
            });
    }

    fn start_upload(&mut self, cfg: UploadConfig) {
        let keys = self.picked_keys();
        if keys.is_empty() {
            return;
        }
        self.uploading = true;
        self.upload_progress = Some((0, keys.len()));
        self.upload_summary = None;
        self.send(Cmd::Upload { keys, cfg });
    }

    fn right_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::right("right")
            .resizable(true)
            .default_size(320.0)
            .min_size(220.0)
            .max_size(560.0)
            .show(ui, |ui| {
                ui.heading("照片详情");
                let Some(key) = self.detail_key else {
                    ui.label(RichText::new("点选缩略图查看该照片的全部信息；右键查看详情。").weak());
                    return;
                };
                if !self.photos.iter().any(|p| p.key == key) {
                    ui.label("照片已不在列表中（可能重新扫描过）。");
                    return;
                }
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.draw_detail(ui, key);
                    });
            });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("bottom")
            .resizable(true)
            .default_size(180.0)
            .min_size(80.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("日志");
                    if ui.button("清空").clicked() {
                        self.log.clear();
                    }
                    ui.label(RichText::new(format!("{} 行", self.log.len())).weak());
                });
                ui.separator();
                // 日志行的信息很长（设备信息、目录清单…），必须允许横向滚动，
                // 否则超出面板宽度的部分会被裁掉且拉不出来。
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(RichText::new(line).monospace().size(12.0));
                        }
                    });
            });
    }

    /// 照片网格（虚拟滚动，只渲染可见行）。
    fn grid(&mut self, ui: &mut egui::Ui) {
        if self.photos.is_empty() {
            self.empty_state(ui);
            return;
        }
        if self.view_dirty {
            self.rebuild_view();
        }

        let cell = self.cell_size;
        let spacing = ui.spacing().item_spacing;
        let row_h = cell_height(cell);
        let pitch = row_h + spacing.y;

        // 自己算虚拟滚动，而不是用 `show_rows`，原因：
        // 1) `show_rows` 要求传入行高与实际行高**完全一致**，且列数只能在滚动区外算；
        // 2) 列数必须按滚动区**内部**的可用宽度算，才能随面板拖动自适应、不被右侧裁掉。
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                let area = ui.max_rect();
                let avail = (area.width() - GRID_RIGHT_MARGIN).max(cell);
                let per_row = (((avail + spacing.x) / (cell + spacing.x)).floor() as usize).max(1);
                let total_rows = self.view.len().div_ceil(per_row);

                // 内容高度取精确值，底部不会多出空白。
                ui.set_height((pitch * total_rows as f32 - spacing.y).max(0.0));

                let first = (viewport.min.y / pitch).floor().max(0.0) as usize;
                let last = ((viewport.max.y / pitch).ceil().max(0.0) as usize + 1).min(total_rows);

                for row in first..last {
                    let row_rect = Rect::from_min_size(
                        egui::pos2(area.left(), area.top() + row as f32 * pitch),
                        Vec2::new(area.width(), row_h),
                    );
                    // id_salt 用行号，保证滚动时同一行的控件 id 稳定（悬停状态不会闪）。
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(row_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Min))
                            .id_salt(("photo-grid-row", row)),
                        |ui| {
                            for col in 0..per_row {
                                let view_pos = row * per_row + col;
                                if view_pos >= self.view.len() {
                                    break;
                                }
                                let idx = self.view[view_pos];
                                self.draw_cell(ui, idx, cell);
                            }
                        },
                    );
                }
            });
    }

    /// 没有任何照片可显示时，明确告诉用户“卡在哪一步”。
    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(70.0);
            if self.connected.is_none() {
                ui.heading("① 还没连接手机");
                ui.label("1) 手机用 USB 连接电脑，USB 用途选“文件传输 / MTP”");
                ui.label("2) 点上方“刷新设备”，选中手机后点“🔌 连接”；列表为空就点“⚡ 打开第一台”");
            } else if self.scanning {
                let (folders, photos, current) = self.scan_progress.clone();
                ui.heading("② 正在扫描照片…");
                ui.spinner();
                ui.label(format!("已遍历目录 {folders} 个，已找到照片 {photos} 张"));
                ui.label(RichText::new(current).small().weak());
                ui.label("扫描是逐个目录向手机要数据的，第一次会比较慢；可点左侧“⏹ 取消扫描”。");
            } else if let Some(s) = &self.scan_stats {
                ui.heading("③ 扫描完成，但没有找到照片");
                ui.label(format!(
                    "遍历目录 {} 个（失败 {} 个），共看到 {} 个对象",
                    s.folders_scanned, s.folders_failed, s.objects_seen
                ));
                if s.folders_failed > 0 {
                    ui.colored_label(
                        Color32::from_rgb(220, 150, 60),
                        "有目录读取失败，具体原因在左下角日志里",
                    );
                }
                if self.scan_scope == ScanScope::CommonFolders {
                    ui.colored_label(
                        Color32::from_rgb(220, 150, 60),
                        "当前是“常见照片目录”快速扫描，请改成“全部目录（完整扫描）”再扫一次",
                    );
                }
                ui.label("左下角日志里有“根目录可见目录”，对照它就能看出照片在哪个目录。");
                ui.label("如果根目录也是空的，说明手机没有向电脑开放文件，重新拔插数据线并选“文件传输 / MTP”。");
            } else {
                ui.heading("② 已连接，但还没扫描");
                ui.label("点左侧的“🔍 扫描照片”");
            }
        });
    }

    /// 画一个缩略图方格。点选切换勾选状态，右键打开详情。
    fn draw_cell(&mut self, ui: &mut egui::Ui, idx: usize, cell: f32) {
        let key = self.photos[idx].key;
        let picked = self.photos[idx].picked;
        let filename = self.photos[idx].filename.clone();
        let size = self.photos[idx].size;
        let thumb = self.photos[idx].thumb.clone();
        let (exif_ready, exif_has_gps) = match &self.photos[idx].exif {
            ExifState::Ready(data) => (true, data.gps.is_some()),
            _ => (false, false),
        };

        // 整格只 allocate 一整个矩形，图与文字都用 painter 画。
        // 之前是图、文字各 allocate 一次，在横向布局里会**并排**占位，
        // 导致单个格子真实宽度是 `cell + spacing + cell`，列数算出来必然偏大、右侧被裁。
        let img_h = cell * CELL_IMAGE_RATIO;
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(cell, cell_height(cell)), Sense::click());
        let img_rect = Rect::from_min_size(rect.min, Vec2::new(cell, img_h));
        let label_rect = Rect::from_min_size(
            egui::pos2(rect.min.x, rect.min.y + img_h),
            Vec2::new(cell, CELL_LABEL_HEIGHT),
        );
        let painter = ui.painter_at(rect);

        let bg = if picked {
            Color32::from_rgb(38, 70, 56)
        } else {
            ui.visuals().extreme_bg_color
        };
        painter.rect_filled(img_rect, 4.0, bg);

        match &thumb {
            ThumbState::Ready => {
                if let Some(tex) = self.textures.get(&key) {
                    let ts = tex.size_vec2();
                    let scale = (img_rect.width() / ts.x).min(img_rect.height() / ts.y);
                    let drawn = Rect::from_center_size(img_rect.center(), ts * scale);
                    painter.image(
                        tex.id(),
                        drawn,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
            }
            ThumbState::Loading => {
                painter.text(
                    img_rect.center(),
                    Align2::CENTER_CENTER,
                    "加载中…",
                    FontId::proportional(12.0),
                    Color32::GRAY,
                );
            }
            ThumbState::Failed(err) => {
                painter.text(
                    img_rect.center(),
                    Align2::CENTER_CENTER,
                    "无法预览",
                    FontId::proportional(12.0),
                    Color32::GRAY,
                );
                painter.text(
                    img_rect.center_bottom() - Vec2::new(0.0, 4.0),
                    Align2::CENTER_BOTTOM,
                    err,
                    FontId::proportional(9.0),
                    Color32::DARK_GRAY,
                );
            }
            ThumbState::Idle => {
                painter.text(
                    img_rect.center(),
                    Align2::CENTER_CENTER,
                    "🖼",
                    FontId::proportional(cell * 0.28),
                    Color32::GRAY,
                );
            }
        }

        let stroke_color = if picked {
            Color32::from_rgb(90, 210, 150)
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        painter.rect_stroke(
            img_rect,
            4.0,
            Stroke::new(if picked { 2.0 } else { 1.0 }, stroke_color),
            StrokeKind::Inside,
        );

        if picked {
            // 左上角打勾
            let c = img_rect.left_top() + Vec2::new(12.0, 12.0);
            painter.circle_filled(c, 9.0, Color32::from_rgb(90, 210, 150));
            painter.text(
                c,
                Align2::CENTER_CENTER,
                "✓",
                FontId::proportional(12.0),
                Color32::BLACK,
            );
        }

        // 已读到 EXIF 时在右上角标一下（带 GPS 时额外标个图钉）
        if exif_ready {
            painter.text(
                img_rect.right_top() + Vec2::new(-5.0, 4.0),
                Align2::RIGHT_TOP,
                if exif_has_gps { "EXIF 📍" } else { "EXIF" },
                FontId::proportional(10.0),
                Color32::from_rgb(120, 200, 255),
            );
        }

        // 文件名 + 大小（画在图片下方的文字区里，不再是并排的第二个控件）
        let short: String = if filename.chars().count() > 22 {
            filename.chars().take(21).collect::<String>() + "…"
        } else {
            filename.clone()
        };
        painter.text(
            label_rect.left_top() + Vec2::new(2.0, 1.0),
            Align2::LEFT_TOP,
            short,
            FontId::proportional(11.0),
            ui.visuals().text_color(),
        );
        painter.text(
            label_rect.left_top() + Vec2::new(2.0, 14.0),
            Align2::LEFT_TOP,
            human_bytes(size),
            FontId::proportional(10.0),
            Color32::GRAY,
        );

        // 交互
        let response = response.on_hover_text(format!("{}\n{}", filename, human_bytes(size)));
        if response.clicked() {
            self.photos[idx].picked = !picked;
            self.detail_key = Some(key);
        }
        if response.secondary_clicked() {
            self.detail_key = Some(key);
            self.show_detail = true;
        }

        // 按需请求缩略图
        if thumb == ThumbState::Idle
            && !self.inflight.contains(&key)
            && self.inflight.len() < THUMB_REQUESTS_PER_FRAME
        {
            self.inflight.insert(key);
            self.photos[idx].thumb = ThumbState::Loading;
            self.send(Cmd::Thumbnail { key });
        }
    }

    fn detail_window(&mut self, ctx: &egui::Context) {
        if !self.show_detail {
            return;
        }
        let mut open = self.show_detail;
        egui::Window::new("照片全部信息")
            .open(&mut open)
            .default_size([460.0, 520.0])
            .vscroll(true)
            .show(ctx, |ui| {
                match self.detail_key {
                    Some(key) => self.draw_detail(ui, key),
                    None => {
                        ui.label("请先在网格里点选一张照片。");
                    }
                }
            });
        self.show_detail = open;
    }
}

impl eframe::App for PhoneToolApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump_events(&ctx);

        // 连接超时兜底：不能因为一次打开卡死就永远转圈。
        if let Some(started) = self.connect_started {
            if started.elapsed() > Duration::from_secs(30) {
                self.connect_started = None;
                self.connecting = false;
                self.last_error = Some(
                    "连接超时（30 秒）。请确认手机已解锁并在弹窗里点了“允许”，然后拔插一次数据线重试。"
                        .to_string(),
                );
            }
        }

        // 面板只在各自的方法里创建一次；这里直接调用即可，
        // 曾经这里又包了一层同 id 的 Panel，导致 egui 报 “Second use of widget ID”
        // 并且设备栏高度、左右栏拖拽都出错。顺序不能变：上 → 下 → 左 → 右 → 中央。
        self.top_bar(ui);
        self.bottom_bar(ui);
        self.left_panel(ui);
        self.right_panel(ui);
        egui::CentralPanel::default().show(ui, |ui| self.grid(ui));
        self.detail_window(&ctx);

        if self.scanning || self.uploading || self.connecting {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// 退出时把 MTP 会话关干净。
    ///
    /// 不关就直接杀进程，手机上那条 MTP 连接会卡住，之后所有数据读取都会报
    /// `0x80070141 无法连接至设备`（这种失效工作线程里会自动重连兜底，但不保证一定成功）。
    fn on_exit(&mut self) {
        if self.connected.is_some() {
            self.send(Cmd::Disconnect);
            // 给工作线程一点时间把 CloseSession 发出去
            std::thread::sleep(Duration::from_millis(800));
        }
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

impl PhoneToolApp {
    /// 在详情面板里画一张照片的全部信息：先 MTP 元数据，再 EXIF。
    fn draw_detail(&mut self, ui: &mut egui::Ui, key: u64) {
        let Some(photo) = self.photos.iter().find(|p| p.key == key).cloned() else {
            ui.label("照片已不在列表中（可能重新扫描过）。");
            return;
        };

        // 只有被选中的照片才会触发解析：第一次看到它时把原图下载到本地缓存，
        // 再解析缓存文件的 EXIF（不会为整个列表预先解析）。
        if matches!(photo.exif, ExifState::Idle) {
            self.request_exif(key, false);
        }

        draw_photo_detail(ui, &photo);

        ui.separator();
        ui.heading("EXIF");

        let mut retry = false;
        match &photo.exif {
            ExifState::Idle | ExifState::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在下载照片到本地缓存并解析 EXIF…");
                });
            }
            ExifState::Failed(msg) => {
                ui.colored_label(
                    Color32::from_rgb(220, 150, 60),
                    format!("没有读到 EXIF：{msg}"),
                );
                ui.label(
                    RichText::new("（截图、微信保存的图、部分编辑过的图片本身就没有 EXIF）")
                        .small()
                        .weak(),
                );
                if ui.button("重新读取 EXIF").clicked() {
                    retry = true;
                }
            }
            ExifState::Ready(data) => {
                draw_exif(ui, data);
                if ui.small_button("重新读取").clicked() {
                    retry = true;
                }
            }
        }
        if retry {
            // 用户手动点“重新读取”→ 忽略缓存，重新从手机下载。
            self.request_exif(key, true);
        }
    }

    /// 把某张照片的 EXIF 状态置为“读取中”并请求工作线程去读。
    ///
    /// `force = false` 时优先用本地缓存（已下载过的照片不再打扰手机）；
    /// `force = true` 时忽略缓存重新下载。
    fn request_exif(&mut self, key: u64, force: bool) {
        if let Some(p) = self.photos.iter_mut().find(|p| p.key == key) {
            p.exif = ExifState::Loading;
        }
        self.send(Cmd::Exif { key, force });
    }
}

/// EXIF 标签名的中文说明。
fn exif_label(tag: &str) -> &str {
    match tag {
        "Make" => "厂商",
        "Model" => "机型",
        "LensModel" => "镜头",
        "Software" => "软件",
        "DateTimeOriginal" => "拍摄时间",
        "DateTimeDigitized" => "数字化时间",
        "DateTime" => "修改时间",
        "OffsetTimeOriginal" => "拍摄时区",
        "SubSecTimeOriginal" => "亚秒",
        "ExposureTime" => "曝光时间",
        "ShutterSpeedValue" => "快门速度",
        "FNumber" => "光圈",
        "ApertureValue" => "光圈值",
        "ISOSpeedRatings" | "PhotographicSensitivity" => "ISO",
        "RecommendedExposureIndex" => "推荐曝光指数",
        "FocalLength" => "焦距",
        "FocalLengthIn35mmFilm" => "等效焦距",
        "ExposureProgram" => "曝光程序",
        "ExposureBiasValue" => "曝光补偿",
        "MeteringMode" => "测光模式",
        "Flash" => "闪光灯",
        "WhiteBalance" => "白平衡",
        "LightSource" => "光源",
        "SceneCaptureType" => "场景类型",
        "ExposureMode" => "曝光模式",
        "ColorSpace" => "色彩空间",
        "Orientation" => "方向",
        "PixelXDimension" => "像素宽",
        "PixelYDimension" => "像素高",
        "Artist" => "作者",
        "Copyright" => "版权",
        "ImageDescription" => "图像描述",
        other => other,
    }
}

/// 画 EXIF 摘要 + 全部字段 + GPS。
fn draw_exif(ui: &mut egui::Ui, data: &crate::exif::ExifData) {
    let summary = crate::exif::exposure_summary(data);
    if !summary.is_empty() {
        ui.label(RichText::new(summary).strong());
    }
    if let Some(camera) = data.camera() {
        ui.label(RichText::new(camera).monospace());
    }

    egui::Grid::new("exif-highlights")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for tag in crate::exif::HIGHLIGHT_TAGS {
                let Some(value) = data.get(tag) else {
                    continue;
                };
                // 相机/曝光参数已经在上面单独展示过了
                if matches!(*tag, "Make" | "Model" | "ExposureTime" | "FNumber") {
                    continue;
                }
                ui.label(RichText::new(exif_label(tag)).strong());
                ui.label(RichText::new(value).monospace());
                ui.end_row();
            }
        });

    if let Some(gps) = &data.gps {
        ui.add_space(4.0);
        ui.label(RichText::new("拍摄位置").strong());
        let altitude = gps
            .altitude
            .map(|a| format!(" · 海拔 {a:.1} m"))
            .unwrap_or_default();
        ui.label(
            RichText::new(format!(
                "{:.6}, {:.6}{altitude}",
                gps.latitude, gps.longitude
            ))
            .monospace(),
        );
        let url = format!(
            "https://www.openstreetmap.org/?mlat={}&mlon={}#map=16/{}/{}\
",
            gps.latitude, gps.longitude, gps.latitude, gps.longitude
        );
        ui.hyperlink_to("在地图中打开", url.trim_end());
    }

    ui.collapsing(format!("全部 EXIF 字段（{} 项）", data.field_count), |ui| {
        egui::Grid::new("exif-all")
            .num_columns(4)
            .spacing([8.0, 3.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("IFD").strong());
                ui.label(RichText::new("标签").strong());
                ui.label(RichText::new("ID").strong());
                ui.label(RichText::new("值").strong());
                ui.end_row();
                for field in &data.fields {
                    ui.label(RichText::new(&field.ifd).small().weak());
                    ui.label(RichText::new(exif_label(&field.tag)).small());
                    ui.label(RichText::new(field.id.to_string()).small().weak());
                    ui.label(RichText::new(&field.value).monospace().small());
                    ui.end_row();
                }
            });
    });
}

fn draw_photo_detail(ui: &mut egui::Ui, p: &PhotoInfo) {
    egui::Grid::new("photo-detail")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).strong());
                ui.label(RichText::new(v).monospace());
                ui.end_row();
            };
            row("文件名", p.filename.clone());
            row("路径", p.normalized_path());
            row("大小", format!("{} ({} 字节)", human_bytes(p.size), p.size));
            row("类型", p.mime_type.clone());
            row(
                "分辨率",
                if p.width > 0 && p.height > 0 {
                    format!("{} × {}", p.width, p.height)
                } else {
                    "设备未上报".to_string()
                },
            );
            row("创建时间", p.created.clone().unwrap_or_else(|| "未知".into()));
            row("修改时间", p.modified.clone().unwrap_or_else(|| "未知".into()));
            row(
                "创建(Unix)",
                p.created_unix.map(|v| v.to_string()).unwrap_or_else(|| "未知".into()),
            );
            row(
                "修改(Unix)",
                p.modified_unix.map(|v| v.to_string()).unwrap_or_else(|| "未知".into()),
            );
            row("存储分区", format!("{} (id={})", p.storage_desc, p.storage_id));
            row("MTP 句柄", p.handle.to_string());
            row("父目录句柄", p.parent_handle.to_string());
            row(
                "是否已选",
                if p.picked { "是".into() } else { "否".into() },
            );
        });
}

/// 安装中文字体：egui 默认字体不含 CJK 字形，不装会全是方框。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\simkai.ttf",
        r"C:\Windows\Fonts\simfang.ttf",
    ];

    let mut chosen: Option<Vec<u8>> = None;
    if let Ok(path) = std::env::var("PHONE_TOOL_CJK_FONT") {
        if let Ok(bytes) = std::fs::read(&path) {
            chosen = Some(bytes);
        }
    }
    if chosen.is_none() {
        for path in CANDIDATES {
            if let Ok(bytes) = std::fs::read(path) {
                chosen = Some(bytes);
                break;
            }
        }
    }

    let Some(bytes) = chosen else {
        eprintln!("警告：未找到中文字体，界面文字可能显示为方块。可设置环境变量 PHONE_TOOL_CJK_FONT 指定字体文件。");
        return;
    };

    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), Arc::new(FontData::from_owned(bytes)));
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "cjk".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("cjk".to_owned());
    ctx.set_fonts(fonts);
}
