//! 无界面诊断模式。
//!
//! 界面出问题时，控制台里跑一条命令就能拿到设备的真实情况：
//!
//! ```text
//! cargo run -- probe
//! ```
//!
//! 因为发布版是 `windows_subsystem = "windows"`（没有控制台），需要重定向输出到文件：
//!
//! ```powershell
//! cargo run -- probe *> probe.txt
//! ```
//!
//! `probe` 会依次打印：设备枚举结果 → 打开第一台设备 → 设备信息 → 存储分区 →
//! 每个存储的根目录条目 → 一级目录里直接包含的照片数。
//! 「照片在哪个目录」和「手机有没有向电脑开放文件」看这份输出就够了。

use std::collections::VecDeque;

use anyhow::Result;
use mtp_rs::mtp::{MtpDevice, Storage};

use crate::thumbs::is_photo_extension;

/// 命令行入口。返回 `Ok(true)` 表示已经处理完，不需要再启动界面。
pub fn run(args: &[String]) -> Result<bool> {
    match args.first().map(String::as_str) {
        None | Some("--gui") | Some("--auto-connect") | Some("-a") => Ok(false),
        Some("probe") => {
            probe()?;
            Ok(true)
        }
        Some("exif") => {
            let limit = args
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(3);
            exif_dump(limit)?;
            Ok(true)
        }
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            Ok(true)
        }
        Some(other) => {
            print_help();
            anyhow::bail!("未知命令: {other}");
        }
    }
}

fn print_help() {
    println!("phone-tool — 手机照片 USB 读取 & 上传工具");
    println!();
    println!("用法:");
    println!("  phone-tool                    启动图形界面（默认）");
    println!("  phone-tool --auto-connect     启动界面，并自动连接第一台设备后开始扫描");
    println!("  phone-tool probe              无界面诊断：打印设备、存储、根目录、各目录照片数");
    println!("  phone-tool exif [数量]        读取前 N 张照片的 EXIF（默认 3），并打印上传用的 JSON");
    println!("  phone-tool help               显示本帮助");
    println!();
    println!("提示: 发布版没有控制台窗口，请用重定向查看输出：");
    println!("  cargo run -- probe *> probe.txt");
}

fn probe() -> Result<()> {
    println!("== 1) 枚举 MTP 设备（mtp-rs 的 USB 描述符枚举）==");
    let devices = match MtpDevice::list_devices() {
        Ok(devices) => {
            if devices.is_empty() {
                println!("  没有发现设备。");
            }
            for d in &devices {
                println!(
                    "  · {} （VID:PID={:04x}:{:04x}，位置 0x{:08x}，速率 {:?}，匹配原因 {:?}）",
                    d.display(),
                    d.vendor_id,
                    d.product_id,
                    d.location_id,
                    d.speed,
                    d.match_reason
                );
            }
            println!("  共 {} 台", devices.len());
            devices
        }
        Err(e) => {
            println!("  枚举失败: {e}");
            Vec::new()
        }
    };

    let first = devices.first().map(|d| {
        (
            d.location_id,
            d.serial_number.clone().unwrap_or_default(),
            d.display(),
        )
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        println!("\n== 2) 按图形界面的优先级依次尝试打开 ==");
        let mut picked: Option<(String, MtpDevice)> = None;
        let mut notes: Vec<String> = Vec::new();

        // 1/3 按 USB 位置（Windows 上用 VID/PID 关联到 WPD 设备）
        if let Some((location_id, _, label)) = &first {
            match MtpDevice::open_by_location(*location_id).await {
                Ok(dev) => {
                    println!(
                        "  [1/3] 按 USB 位置 0x{location_id:08x} → 成功（{label}）",
                    );
                    picked = Some(("按 USB 位置".to_string(), dev));
                }
                Err(e) => {
                    let note = format!("[1/3] 按 USB 位置 0x{location_id:08x} 失败: {e}");
                    println!("  {note}");
                    notes.push(note);
                }
            }
        } else {
            println!("  [1/3] 按 USB 位置 → 跳过（设备列表为空）");
        }

        // 2/3 按序列号
        match &picked {
            Some(_) => println!("  [2/3] 按序列号 → 跳过（上一步已成功）"),
            None => match &first {
                Some((_, serial, _)) if !serial.is_empty() => {
                    match MtpDevice::open_by_serial(serial).await {
                        Ok(dev) => {
                            println!("  [2/3] 按序列号 {serial} → 成功");
                            picked = Some(("按序列号".to_string(), dev));
                        }
                        Err(e) => {
                            let note = format!("[2/3] 按序列号 {serial} 失败: {e}");
                            println!("  {note}");
                            notes.push(note);
                        }
                    }
                }
                _ => println!("  [2/3] 按序列号 → 跳过（没有序列号）"),
            },
        }

        // 3/3 兜底：打开第一台（Windows 上直接枚举 WPD，可能选中非手机设备）
        match &picked {
            Some(_) => println!("  [3/3] 打开第一台 → 跳过（前面已成功）"),
            None => match MtpDevice::open_first().await {
                Ok(dev) => {
                    println!("  [3/3] 打开第一台 → 成功");
                    picked = Some(("打开第一台".to_string(), dev));
                }
                Err(e) => {
                    let note = format!("[3/3] 打开第一台失败: {e}");
                    println!("  {note}");
                    notes.push(note);
                }
            },
        }

        let Some((how, device)) = picked else {
            anyhow::bail!(
                "所有打开方式都失败了：{}\n\
                 手机请解锁、USB 用途选“文件传输 / MTP”，并允许电脑访问设备数据。",
                notes.join("；")
            );
        };

        println!("\n== 3) 设备信息（打开方式：{how}）==");
        let info = device.device_info();
        println!(
            "  厂商={} 型号={} 序列号={} 固件={}",
            info.manufacturer, info.model, info.serial_number, info.device_version
        );

        let storages = device.storages().await?;
        println!("\n== 4) 存储分区（{} 个）==", storages.len());
        if storages.is_empty() {
            println!("  ⚠ 这台设备没有任何存储分区 —— 它很可能不是你手机（例如 U 盘/读卡器）。");
        }
        for (i, s) in storages.iter().enumerate() {
            let si = s.info();
            println!(
                "  #{i} [{}] 可用 {:.2} GB / 共 {:.2} GB · 可写={} · 类型={:?} · 文件系统={:?}",
                si.description,
                si.free_space as f64 / 1e9,
                si.total_capacity as f64 / 1e9,
                si.is_writable,
                si.storage_type,
                si.filesystem_type
            );
        }

        for (i, storage) in storages.iter().enumerate() {
            let label = storage.info().description.clone();
            println!("\n== 5.{i}) 存储「{label}」==");
            if let Err(e) = dump_storage(storage, &label).await {
                println!("  读取失败: {e}");
            }
        }

        Ok::<(), anyhow::Error>(())
    })
}

/// 打印一个存储的根目录，以及一级目录里直接包含的照片数。
async fn dump_storage(storage: &Storage, label: &str) -> Result<()> {
    let root = storage.list_objects(None).await?;
    println!("  根目录返回 {} 个对象:", root.len());
    for o in &root {
        println!(
            "    {:<4} {:<30} {:>13} 字节  {:?}",
            if o.is_folder() { "DIR" } else { "FILE" },
            o.filename,
            o.size,
            o.format
        );
    }
    if root.is_empty() {
        println!("    ⚠ 根目录是空的：手机没有向电脑开放文件（检查 USB 用途是否为“文件传输 / MTP”）。");
        return Ok(());
    }

    println!("\n  一级目录内容统计:");
    let mut total = 0usize;
    for folder in root.iter().filter(|o| o.is_folder()) {
        match storage.list_objects(Some(folder.handle)).await {
            Ok(children) => {
                let dirs = children.iter().filter(|c| c.is_folder()).count();
                let files = children.len() - dirs;
                let photos = children
                    .iter()
                    .filter(|c| !c.is_folder() && is_photo_extension(&c.filename))
                    .count();
                total += photos;
                println!(
                    "    [{}] {} 项 → 子目录 {} / 文件 {}，照片 {}",
                    folder.filename,
                    children.len(),
                    dirs,
                    files,
                    photos
                );
            }
            Err(e) => println!("    [{}] 读取失败: {e}", folder.filename),
        }
    }
    println!("  [{label}] 一级目录里直接包含的照片合计: {total}");
    Ok(())
}

// ---------------------------------------------------------------------------
// exif：用真机照片验证 EXIF 解析与上传 JSON
// ---------------------------------------------------------------------------

/// 按「USB 位置 → 序列号 → 第一台」的顺序打开设备（和图形界面一致）。
async fn open_device() -> Result<MtpDevice> {
    let devices = MtpDevice::list_devices().unwrap_or_default();
    let mut errors: Vec<String> = Vec::new();

    if let Some(d) = devices.first() {
        match MtpDevice::open_by_location(d.location_id).await {
            Ok(dev) => return Ok(dev),
            Err(e) => errors.push(format!("按位置 0x{:08x} 打开失败: {e}", d.location_id)),
        }
        if let Some(serial) = d.serial_number.as_deref().filter(|s| !s.is_empty()) {
            match MtpDevice::open_by_serial(serial).await {
                Ok(dev) => return Ok(dev),
                Err(e) => errors.push(format!("按序列号 {serial} 打开失败: {e}")),
            }
        }
    }

    match MtpDevice::open_first().await {
        Ok(dev) => Ok(dev),
        Err(e) => {
            errors.push(format!("打开第一台失败: {e}"));
            anyhow::bail!("打不开设备：{}", errors.join("；"))
        }
    }
}

fn exif_dump(limit: usize) -> Result<()> {
    let limit = limit.clamp(1, 50);
    println!("== 读取手机上前 {limit} 张照片的 EXIF（以及上传时用的 JSON）==");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let device = open_device().await?;
        let info = device.device_info();
        println!("设备: {} {} ({})", info.manufacturer, info.model, info.serial_number);

        let storages = device.storages().await?;
        let storage = storages
            .first()
            .ok_or_else(|| anyhow::anyhow!("设备没有存储分区，可能不是手机"))?;
        println!("存储: {}", storage.info().description);

        // 广度优先找照片；优先扫相机目录（那里的照片才带 EXIF/GPS）
        let mut queue: VecDeque<Option<mtp_rs::mtp::ObjectHandle>> = VecDeque::new();
        queue.push_back(None);
        let mut visited = 0usize;
        let mut found: Vec<mtp_rs::mtp::ObjectInfo> = Vec::new();
        let mut camera_first: VecDeque<Option<mtp_rs::mtp::ObjectHandle>> = VecDeque::new();

        while let Some(parent) = queue.pop_front() {
            if found.len() >= limit || visited >= 120 {
                break;
            }
            let Ok(objects) = storage.list_objects(parent).await else {
                continue;
            };
            visited += 1;
            for obj in objects {
                if obj.is_folder() {
                    let name = obj.filename.to_lowercase();
                    if name.starts_with('.') || name == "android" {
                        continue;
                    }
                    // DCIM/Camera 先扫，其次是 DCIM 里的其它目录
                    if name == "camera" || name == "dcim" {
                        camera_first.push_back(Some(obj.handle));
                    } else {
                        queue.push_back(Some(obj.handle));
                    }
                } else if is_photo_extension(&obj.filename) {
                    found.push(obj);
                    if found.len() >= limit {
                        break;
                    }
                }
            }
        }

        // 还没凑够就先把相机目录扫了
        while found.len() < limit {
            let Some(next) = camera_first.pop_front() else {
                break;
            };
            let Ok(objects) = storage.list_objects(next).await else {
                continue;
            };
            for obj in objects {
                if obj.is_folder() {
                    let name = obj.filename.to_lowercase();
                    if !name.starts_with('.') && name != "android" {
                        camera_first.push_back(Some(obj.handle));
                    }
                } else if is_photo_extension(&obj.filename) {
                    found.push(obj);
                    if found.len() >= limit {
                        break;
                    }
                }
            }
        }

        if found.is_empty() {
            println!("没找到照片。");
            return Ok(());
        }

        for obj in &found {
            println!("\n--- {} （{} 字节）---", obj.filename, obj.size);
            // 复用上传时用的同一套读取逻辑：整张下载后解析
            match crate::worker::fetch_exif(storage, obj.handle).await {
                Ok(data) => {
                    println!(
                        "相机   : {}",
                        data.camera().unwrap_or_else(|| "未知".to_string())
                    );
                    println!("曝光   : {}", crate::exif::exposure_summary(&data));
                    println!("字段数 : {}", data.field_count);
                    if let Some(gps) = &data.gps {
                        println!(
                            "GPS    : {:.6}, {:.6}{}",
                            gps.latitude,
                            gps.longitude,
                            gps.altitude
                                .map(|a| format!(" （海拔 {a:.1} m）"))
                                .unwrap_or_default()
                        );
                    }
                    println!("上传时 photo.exif 的内容:");
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&data).unwrap_or_default()
                    );
                }
                Err(e) => println!("没有 EXIF：{e}"),
            }
        }

        Ok::<(), anyhow::Error>(())
    })
}
