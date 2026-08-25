#![windows_subsystem = "windows"]

mod app;
mod config;
mod font;
mod image_loader;

use anyhow::Result;
use app::ImageViewerApp;
use mimalloc::MiMalloc;
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// アプリアイコンを読み込み、外枠の背景を綺麗に自動透過（アルファチャンネル処理）して IconData を作成
fn load_app_icon() -> Option<egui::IconData> {
    let icon_bytes = include_bytes!("../assets/icon.jpg");
    if let Ok(img) = image::load_from_memory(icon_bytes) {
        let mut rgba_img = img.to_rgba8();
        let (width, height) = rgba_img.dimensions();

        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0;
        let radius = (width.min(height) as f32 / 2.0) * 0.96;

        for (x, y, pixel) in rgba_img.enumerate_pixels_mut() {
            let dx = x as f32 - center_x;
            let dy = y as f32 - center_y;
            let dist = (dx * dx + dy * dy).sqrt();

            // 外枠背景（白色領域および円形外）を完全透明化(Alpha=0)
            let is_bg_white = pixel[0] > 235 && pixel[1] > 235 && pixel[2] > 235;
            if dist > radius || is_bg_white {
                pixel[3] = 0;
            } else if dist > radius - 2.0 {
                // アンテナアンチエイリアス処理（滑らかな透明境界）
                let alpha_ratio = (radius - dist) / 2.0;
                pixel[3] = (pixel[3] as f32 * alpha_ratio.clamp(0.0, 1.0)) as u8;
            }
        }

        return Some(egui::IconData {
            rgba: rgba_img.into_raw(),
            width,
            height,
        });
    }
    None
}

fn main() -> Result<()> {
    env_logger::init();

    // 起動時のコマンドライン引数（画像パス）を取得
    let initial_path = std::env::args().nth(1).map(PathBuf::from);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Rust 画像ビューア (Image Viewer)")
        .with_inner_size([1100.0, 750.0])
        .with_min_inner_size([600.0, 400.0])
        .with_active(true);

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Rust Image Viewer",
        native_options,
        Box::new(move |cc| Ok(Box::new(ImageViewerApp::new(cc, initial_path)))),
    )
    .map_err(|e| anyhow::anyhow!("Application error: {}", e))
}
