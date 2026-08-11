use anyhow::{Context, Result};
use egui::ColorImage;
use image::{DynamicImage, GenericImageView};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// サポート対象の画像拡張子
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "ico",
];

/// 画像データの保持構造体
#[derive(Clone)]
pub struct LoadedImage {
    pub path: PathBuf,
    pub original_image: Arc<DynamicImage>,
    pub width: u32,
    pub height: u32,
    pub file_size_bytes: u64,
}

impl LoadedImage {
    /// ファイルパスから画像をロードする
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("Failed to read metadata for {:?}", path))?;
        let file_size_bytes = metadata.len();

        let reader = image::ImageReader::open(&path)
            .with_context(|| format!("Failed to open image file {:?}", path))?
            .with_guessed_format()
            .with_context(|| format!("Failed to guess format for {:?}", path))?;

        let dynamic_img = reader
            .decode()
            .with_context(|| format!("Failed to decode image {:?}", path))?;

        let (width, height) = dynamic_img.dimensions();

        Ok(Self {
            path,
            original_image: Arc::new(dynamic_img),
            width,
            height,
            file_size_bytes,
        })
    }


    /// 回転・反転・高品質ダウンサンプリング処理を行った新しい `ColorImage` を生成
    pub fn transform_color_image(
        &self,
        rotation_deg: i32,
        flip_h: bool,
        flip_v: bool,
        target_size: Option<(u32, u32)>,
    ) -> (ColorImage, u32, u32) {
        let rot = (rotation_deg % 360 + 360) % 360;
        let is_swapped = rot == 90 || rot == 270;

        let (orig_w, orig_h) = if is_swapped {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };

        let mut img: Cow<DynamicImage> = Cow::Borrowed(&self.original_image);

        match rot {
            90 => img = Cow::Owned(img.rotate90()),
            180 => img = Cow::Owned(img.rotate180()),
            270 => img = Cow::Owned(img.rotate270()),
            _ => {}
        }

        if flip_h {
            img = Cow::Owned(img.fliph());
        }
        if flip_v {
            img = Cow::Owned(img.flipv());
        }

        // 高精度リサンプリング (単一 Lanczos3 フィルター)
        if let Some((max_w, max_h)) = target_size {
            if max_w > 0 && max_h > 0 {
                // アスペクト比を維持したリサイズ後の寸法計算
                let ratio_x = max_w as f64 / orig_w as f64;
                let ratio_y = max_h as f64 / orig_h as f64;
                let ratio = ratio_x.min(ratio_y).min(1.0);

                let fit_w = ((orig_w as f64 * ratio).round() as u32).max(1);
                let fit_h = ((orig_h as f64 * ratio).round() as u32).max(1);

                if fit_w < orig_w || fit_h < orig_h {
                    let resized_buf = image::imageops::resize(
                        img.as_ref(),
                        fit_w,
                        fit_h,
                        image::imageops::FilterType::Lanczos3,
                    );
                    let resized_img = DynamicImage::ImageRgba8(resized_buf);
                    let color_img = dynamic_image_to_color_image(&resized_img);
                    return (color_img, fit_w, fit_h);
                }
            }
        }

        let (w, h) = img.dimensions();
        let color_img = dynamic_image_to_color_image(img.as_ref());
        (color_img, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_manga_downscale() {
        let test_img_path = PathBuf::from(r"C:\Users\kippe\.gemini\antigravity\brain\fe3af6f7-7851-4565-9e7e-aacd62dd1313\.user_uploaded\media_1786450134597.jpg");
        if test_img_path.exists() {
            let loaded = LoadedImage::load_from_path(&test_img_path).unwrap();
            
            // ガウシアンブラー (適応的ローパスフィルタ σ=0.45) を前処理してモアレのみを完璧にシャットアウト
            let blurred = image::imageops::blur(loaded.original_image.as_ref(), 0.45);
            let resized = image::imageops::resize(&blurred, 750, 1050, image::imageops::FilterType::CatmullRom);

            let color_img = dynamic_image_to_color_image(&DynamicImage::ImageRgba8(resized));
            
            let w = 750;
            let h = 1050;
            let mut img_buf = image::RgbaImage::new(w, h);
            for y in 0..h {
                for x in 0..w {
                    let pixel = color_img.pixels[(y * w + x) as usize];
                    img_buf.put_pixel(x, y, image::Rgba([pixel.r(), pixel.g(), pixel.b(), pixel.a()]));
                }
            }
            let out_path = r"C:\Users\kippe\.gemini\antigravity\brain\fe3af6f7-7851-4565-9e7e-aacd62dd1313\scratch\actual_rendered_output.png";
            img_buf.save(out_path).unwrap();
            println!("Rendered output saved to {}", out_path);
        }
    }

}







/// DynamicImage から egui::ColorImage への高精度変換
fn dynamic_image_to_color_image(img: &DynamicImage) -> ColorImage {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.as_flat_samples();
    ColorImage::from_rgba_unmultiplied(size, pixels.as_slice())
}


/// 指定したファイルと同じディレクトリにある対応画像一覧を取得する (高速化版)
pub fn scan_directory_for_images(current_path: &Path) -> (Vec<PathBuf>, usize) {
    let mut images = Vec::new();
    let mut current_idx = 0;

    let parent_dir = match current_path.parent() {
        Some(dir) => dir,
        None => return (images, 0),
    };

    if let Ok(entries) = std::fs::read_dir(parent_dir) {
        let mut entry_paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .filter(|path| {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str())
                } else {
                    false
                }
            })
            .collect();

        // 自然順（アルファベット順）にソート
        entry_paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for (idx, path) in entry_paths.into_iter().enumerate() {
            if path == current_path || path.file_name() == current_path.file_name() {
                current_idx = idx;
            }
            images.push(path);
        }
    }

    (images, current_idx)
}


/// ファイルサイズを読みやすい文字列にフォーマット
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
