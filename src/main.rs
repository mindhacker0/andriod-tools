//! 手机照片工具：通过 USB (MTP) 读取手机照片，并把照片信息 / 照片文件上传到服务器。
//!
//! Windows 上 `mtp-rs` 使用系统自带的 WPD (Windows Portable Devices) COM 栈访问手机，
//! **不需要额外安装驱动**；手机侧只需把 USB 用途切到“文件传输 / MTP”。

// 发布版不弹控制台窗口（调试版保留，方便看日志）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod exif;
mod model;
mod thumbs;
mod uploader;
mod worker;

fn main() -> eframe::Result<()> {
    // 命令行模式（诊断等）不启动界面。
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::run(&args) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            eprintln!("错误: {e:#}");
            std::process::exit(1);
        }
    }

    // --auto-connect：启动后自动连上第一台设备并开始扫描
    let auto_connect = args.iter().any(|a| a == "--auto-connect" || a == "-a");

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 900.0])
            .with_min_inner_size([960.0, 620.0])
            .with_title("手机照片上传工具"),
        ..Default::default()
    };

    eframe::run_native(
        "手机照片上传工具",
        options,
        Box::new(move |cc| Ok(Box::new(app::PhoneToolApp::new(cc, auto_connect)))),
    )
}
