//! 阿里万相 3.0（wan3.0）视频生成工具。
//!
//! 全能参考模式：上传参考图 + 提示词，可设置清晰度 / 比例 / 时长 / 是否启用声音，
//! 生成完成后自动把视频下载到本地输出目录。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod app;
mod config;
mod media;

use gpui_kit::prelude::*;
use gpui_kit::{SharedString, TitlebarOptions, WindowBounds, WindowOptions, px, size};

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1180.), px(880.)), cx)),
                window_min_size: Some(size(px(960.), px(640.))),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("万相 3.0 视频生成工具")),
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::WanApp::new(window, cx))
            })
            .expect("无法创建主窗口");
        });
}
