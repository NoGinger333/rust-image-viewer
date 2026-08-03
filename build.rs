fn main() {
    // Windows ターゲット時の .exe ファイルアイコン埋め込み設定
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        // assets/icon.jpg から assets/icon.ico を自動生成・変換
        if let Ok(img) = image::open("assets/icon.jpg") {
            let mut rgba_img = img.to_rgba8();
            let (width, height) = rgba_img.dimensions();

            let center_x = width as f32 / 2.0;
            let center_y = height as f32 / 2.0;
            let radius = (width.min(height) as f32 / 2.0) * 0.96;

            for (x, y, pixel) in rgba_img.enumerate_pixels_mut() {
                let dx = x as f32 - center_x;
                let dy = y as f32 - center_y;
                let dist = (dx * dx + dy * dy).sqrt();

                let is_bg_white = pixel[0] > 235 && pixel[1] > 235 && pixel[2] > 235;
                if dist > radius || is_bg_white {
                    pixel[3] = 0;
                } else if dist > radius - 2.0 {
                    let alpha_ratio = (radius - dist) / 2.0;
                    pixel[3] = (pixel[3] as f32 * alpha_ratio.clamp(0.0, 1.0)) as u8;
                }
            }

            // 256x256 の ICO アイコンとして保存
            let resized = image::imageops::resize(&rgba_img, 256, 256, image::imageops::FilterType::Lanczos3);
            let _ = resized.save("assets/icon.ico");
        }

        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("Failed to compile winres icon: {}", e);
        }
    }
}
