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
    /// (テスト専用ラッパー。本番経路はバックグラウンドスレッドから `transform_image` を直接使用)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn transform_color_image(
        &self,
        rotation_deg: i32,
        flip_h: bool,
        flip_v: bool,
        target_size: Option<(u32, u32)>,
    ) -> (ColorImage, u32, u32) {
        transform_image(
            &self.original_image,
            rotation_deg,
            flip_h,
            flip_v,
            target_size,
        )
    }
}

/// 回転・反転・高品質ダウンサンプリングを行う (バックグラウンドスレッドからも呼び出せる純粋関数)
pub fn transform_image(
    original_image: &DynamicImage,
    rotation_deg: i32,
    flip_h: bool,
    flip_v: bool,
    target_size: Option<(u32, u32)>,
) -> (ColorImage, u32, u32) {
    let rot = rotation_deg.rem_euclid(360);
    let is_swapped = rot == 90 || rot == 270;

    let (orig_w, orig_h) = if is_swapped {
        (original_image.height(), original_image.width())
    } else {
        (original_image.width(), original_image.height())
    };

    let mut img: Cow<DynamicImage> = Cow::Borrowed(original_image);

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

    // 変形後の寸法 (以降のフォールバックでも使用)
    let (w, h) = img.dimensions();

    // 高速 SIMD Lanczos3 リサンプリング (fast_image_resize)
    if let Some((max_w, max_h)) = target_size {
        if max_w > 0 && max_h > 0 {
            // アスペクト比を維持したリサイズ後の寸法計算
            let ratio_x = max_w as f64 / orig_w as f64;
            let ratio_y = max_h as f64 / orig_h as f64;
            let ratio = ratio_x.min(ratio_y).min(1.0);

            let fit_w = ((orig_w as f64 * ratio).round() as u32).max(1);
            let fit_h = ((orig_h as f64 * ratio).round() as u32).max(1);

            if fit_w < orig_w || fit_h < orig_h {
                // RGBA8 変換は 1 回だけ (回転・反転済み Rgba8 バッファはコピーなしで所有権を取得)
                let rgba = match Cow::into_owned(img) {
                    DynamicImage::ImageRgba8(buf) => buf,
                    other => other.to_rgba8(),
                };
                let (src_w, src_h) = (rgba.width(), rgba.height());
                let src_image = fast_image_resize::images::Image::from_vec_u8(
                    src_w,
                    src_h,
                    rgba.into_raw(),
                    fast_image_resize::PixelType::U8x4,
                );

                if let Ok(src_img) = src_image {
                    let mut dst_image = fast_image_resize::images::Image::new(
                        fit_w,
                        fit_h,
                        fast_image_resize::PixelType::U8x4,
                    );

                    let mut resizer = fast_image_resize::Resizer::new();
                    let options = fast_image_resize::ResizeOptions::new().resize_alg(
                        fast_image_resize::ResizeAlg::Convolution(
                            fast_image_resize::FilterType::Lanczos3,
                        ),
                    );

                    if resizer.resize(&src_img, &mut dst_image, &options).is_ok() {
                        let dst_vec = dst_image.into_vec();
                        let color_img = ColorImage::from_rgba_unmultiplied(
                            [fit_w as usize, fit_h as usize],
                            &dst_vec,
                        );
                        return (color_img, fit_w, fit_h);
                    }

                    // リサイズ失敗時 (実質発生しない) は変形済みバッファをそのまま返す
                    let fallback_raw = src_img.into_vec();
                    let color_img = ColorImage::from_rgba_unmultiplied(
                        [src_w as usize, src_h as usize],
                        &fallback_raw,
                    );
                    return (color_img, w, h);
                }

                // from_vec_u8 失敗はバッファ長不整合のみで発生し得ないため、
                // その場合のみ安全側に倒して原画像をそのまま返す
                let (ow, oh) = original_image.dimensions();
                return (dynamic_image_to_color_image(original_image), ow, oh);
            }
        }
    }

    let color_img = dynamic_image_to_color_image(img.as_ref());
    (color_img, w, h)
}

/// DynamicImage から egui::ColorImage への高精度変換 (ゼロコピームーブ最適化)
fn dynamic_image_to_color_image(img: &DynamicImage) -> ColorImage {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, &rgba.into_raw())
}

/// 指定したファイルと同じディレクトリにある対応画像一覧を取得する (キャッシュ付きソート最適化版)
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

        // 自然順（アルファベット順）にキャッシュキーで高速ソート
        entry_paths.sort_by_cached_key(|p| p.file_name().map(|n| n.to_os_string()));

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

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 5), "5.00 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.00 GB");
    }

    #[test]
    fn test_dynamic_image_to_color_image() {
        let mut img_buf = RgbaImage::new(10, 10);
        img_buf.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let dyn_img = DynamicImage::ImageRgba8(img_buf);

        let color_img = dynamic_image_to_color_image(&dyn_img);
        assert_eq!(color_img.size, [10, 10]);
        assert_eq!(color_img.pixels[0].r(), 255);
        assert_eq!(color_img.pixels[0].g(), 0);
        assert_eq!(color_img.pixels[0].b(), 0);
    }

    #[test]
    fn test_transform_color_image_resize() {
        let mut img_buf = RgbaImage::new(100, 100);
        for pixel in img_buf.pixels_mut() {
            *pixel = Rgba([100, 150, 200, 255]);
        }
        let loaded = LoadedImage {
            path: PathBuf::from("test.png"),
            original_image: Arc::new(DynamicImage::ImageRgba8(img_buf)),
            width: 100,
            height: 100,
            file_size_bytes: 1024,
        };

        let (color_img, fit_w, fit_h) = loaded.transform_color_image(0, false, false, Some((50, 50)));
        assert_eq!(fit_w, 50);
        assert_eq!(fit_h, 50);
        assert_eq!(color_img.size, [50, 50]);
        assert_eq!(color_img.pixels.len(), 50 * 50);
    }
}
