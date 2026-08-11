use anyhow::{Context, Result};
use egui::ColorImage;
use image::{DynamicImage, GenericImageView};
use std::path::{Path, PathBuf};

/// サポート対象の画像拡張子
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif", "ico",
];

/// 画像データの保持構造体
#[derive(Clone)]
pub struct LoadedImage {
    pub path: PathBuf,
    pub original_image: DynamicImage,
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
            original_image: dynamic_img,
            width,
            height,
            file_size_bytes,
        })
    }


    /// 回転・反転・シャープネス処理を行った新しい `ColorImage` を生成
    pub fn transform_color_image(&self, rotation_deg: i32, flip_h: bool, flip_v: bool, sharpen: bool) -> (ColorImage, u32, u32) {
        let mut img = self.original_image.clone();

        match (rotation_deg % 360 + 360) % 360 {
            90 => img = img.rotate90(),
            180 => img = img.rotate180(),
            270 => img = img.rotate270(),
            _ => {}
        }

        if flip_h {
            img = img.fliph();
        }
        if flip_v {
            img = img.flipv();
        }

        if sharpen {
            let sharpened_buf = image::imageops::unsharpen(&img, 1.2, 1);
            let sharpened = DynamicImage::ImageRgba8(sharpened_buf);
            let (w, h) = sharpened.dimensions();
            let color_img = dynamic_image_to_color_image(&sharpened);
            return (color_img, w, h);
        }

        let (w, h) = img.dimensions();
        let color_img = dynamic_image_to_color_image(&img);
        (color_img, w, h)
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
