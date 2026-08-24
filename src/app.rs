use crate::image_loader::{format_bytes, scan_directory_for_images, LoadedImage};
use eframe::egui;
use egui::{Color32, Context, Sense, TextureHandle, Vec2};
use poll_promise::Promise;
use rfd::FileDialog;
use std::collections::HashMap;
use std::path::PathBuf;

/// 最大キャッシュ画像数（メモリ節約とレスポンス向上）
const MAX_CACHE_SIZE: usize = 20;

/// 表示モード
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ViewMode {
    FitWindow,
    FreeZoom,
    OriginalSize,
}

pub struct ImageViewerApp {
    /// 現在読み込んでいる画像データ
    current_loaded_image: Option<LoadedImage>,
    /// 表示用eguiテクスチャ
    texture_handle: Option<TextureHandle>,
    /// 描画済みのターゲットサイズ
    rendered_target_size: Option<(u32, u32)>,
    /// リサイズデバウンス判定用
    last_target_size: Option<(u32, u32)>,
    last_target_size_change_time: std::time::Instant,

    /// デコード済み画像のキャッシュ (パス -> LoadedImage)
    image_cache: HashMap<PathBuf, LoadedImage>,
    /// バックグラウンドで先読み中のPromise一覧
    preload_promises: Vec<Promise<(PathBuf, Result<LoadedImage, String>)>>,

    /// ディレクトリ内の画像一覧
    image_list: Vec<PathBuf>,
    current_index: usize,

    /// 変形パラメータ
    zoom_factor: f32,
    pan_offset: Vec2,
    rotation_deg: i32,
    flip_h: bool,
    flip_v: bool,
    view_mode: ViewMode,

    /// UI状態
    show_sidebar: bool,
    error_message: Option<String>,

    /// ホイールによるページ移動用ロック＆タイマー
    scroll_locked: bool,
    last_scroll_navigate_time: std::time::Instant,
}

/// アプリ設定の読み書き用
const CONFIG_FILE_NAME: &str = "rust_image_viewer_config.txt";

fn load_sidebar_config() -> bool {
    if let Ok(content) = std::fs::read_to_string(CONFIG_FILE_NAME) {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("show_sidebar=") {
                return val.trim().parse().unwrap_or(true);
            }
        }
    }
    true
}

fn save_sidebar_config(show_sidebar: bool) {
    let content = format!("show_sidebar={}\n", show_sidebar);
    let _ = std::fs::write(CONFIG_FILE_NAME, content);
}

impl Default for ImageViewerApp {
    fn default() -> Self {
        Self {
            current_loaded_image: None,
            texture_handle: None,
            rendered_target_size: None,
            last_target_size: None,
            last_target_size_change_time: std::time::Instant::now(),
            image_cache: HashMap::new(),
            preload_promises: Vec::new(),
            image_list: Vec::new(),
            current_index: 0,
            zoom_factor: 1.0,
            pan_offset: Vec2::ZERO,
            rotation_deg: 0,
            flip_h: false,
            flip_v: false,
            view_mode: ViewMode::FitWindow,
            show_sidebar: load_sidebar_config(),
            error_message: None,
            scroll_locked: false,
            last_scroll_navigate_time: std::time::Instant::now(),
        }
    }
}



impl ImageViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        setup_japanese_font(&cc.egui_ctx);
        setup_custom_theme(&cc.egui_ctx);
        let mut app = Self::default();
        if let Some(path) = initial_path {
            app.open_image_file(path);
        }
        app
    }

    /// 画像の読み込み処理 (フォルダ再走査のキャッシュ & バックグラウンド先読みの最適化)
    fn open_image_file(&mut self, path: PathBuf) {
        self.error_message = None;
        self.texture_handle = None; // テクスチャ更新フラグのリセット
        self.rendered_target_size = None;

        let parent_dir = path.parent().map(|p| p.to_path_buf());

        // フォルダが変わっていない場合は再走査（重いディスクアクセス）をスキップ！
        let is_same_dir = match (&self.image_list.first(), &parent_dir) {
            (Some(first_path), Some(curr_parent)) => first_path.parent() == Some(curr_parent.as_path()),
            _ => false,
        };

        if is_same_dir && !self.image_list.is_empty() {
            if let Some(idx) = self.image_list.iter().position(|p| p == &path) {
                self.current_index = idx;
            }
        } else {
            let (list, idx) = scan_directory_for_images(&path);
            self.image_list = list;
            self.current_index = idx;
        }

        self.texture_handle = None;
        self.rendered_target_size = None;

        // キャッシュに既に存在する場合は即座に表示（Arc参照クローンによりO(1)爆速取得）
        if let Some(cached_image) = self.image_cache.get(&path).cloned() {
            self.current_loaded_image = Some(cached_image);
            self.reset_view();
            self.trigger_preloading();
            return;
        }

        // キャッシュにない場合は即時ロード
        match LoadedImage::load_from_path(&path) {
            Ok(loaded_img) => {
                self.image_cache.insert(path.clone(), loaded_img.clone());
                self.current_loaded_image = Some(loaded_img);
                self.reset_view();
                self.trigger_preloading();
            }

            Err(err) => {
                self.error_message = Some(format!("画像の読み込みに失敗しました: {}", err));
            }
        }
    }

    /// 現在位置の周辺（前後の画像）を非同期バックグラウンドで先読み（負荷を抑えて最適化）
    fn trigger_preloading(&mut self) {
        if self.image_list.is_empty() {
            return;
        }

        let len = self.image_list.len();
        // 前後 2 枚ずつ（軽量化）
        let mut paths_to_preload = Vec::new();
        for offset in 1..=2 {
            let next_idx = (self.current_index + offset) % len;
            paths_to_preload.push(self.image_list[next_idx].clone());

            let prev_idx = (self.current_index + len - offset) % len;
            paths_to_preload.push(self.image_list[prev_idx].clone());
        }

        // 古い遠いキャッシュの削除（メモリ節約）
        if self.image_cache.len() > MAX_CACHE_SIZE {
            let keep_set: std::collections::HashSet<&PathBuf> =
                paths_to_preload.iter().chain(std::iter::once(&self.image_list[self.current_index])).collect();

            self.image_cache.retain(|path, _| keep_set.contains(path));
        }

        // まだ先読み中・キャッシュにない画像のみ非同期ロード
        for path in paths_to_preload {
            if !self.image_cache.contains_key(&path) {
                let path_clone = path.clone();
                let promise = Promise::spawn_thread("preload_image", move || {
                    let res = LoadedImage::load_from_path(&path_clone).map_err(|e| e.to_string());
                    (path_clone, res)
                });
                self.preload_promises.push(promise);
            }
        }
    }

    /// バックグラウンド先読みの結果を取り込む
    fn poll_preload_promises(&mut self) {
        self.preload_promises.retain_mut(|promise| {
            if let Some((path, result)) = promise.ready() {
                if let Ok(loaded_img) = result {
                    self.image_cache.insert(path.clone(), loaded_img.clone());
                }
                false
            } else {
                true
            }
        });
    }

    /// 前/次の画像へ切り替え
    fn navigate_image(&mut self, delta: isize) {
        if self.image_list.is_empty() {
            return;
        }
        let len = self.image_list.len() as isize;
        let new_idx = (self.current_index as isize + delta).rem_euclid(len) as usize;
        self.current_index = new_idx;
        let target_path = self.image_list[new_idx].clone();
        self.open_image_file(target_path);
    }

    /// 視点のリセット
    fn reset_view(&mut self) {
        self.zoom_factor = 1.0;
        self.pan_offset = Vec2::ZERO;
        self.rotation_deg = 0;
        self.flip_h = false;
        self.flip_v = false;
        self.view_mode = ViewMode::FitWindow;
        self.rendered_target_size = None;
    }

    /// テクスチャの再生成 (モアレ防止の高品質アンチエイリアシング縮小適用)
    fn update_texture_with_target_size(&mut self, ctx: &Context, target_size: Option<(u32, u32)>) {
        if let Some(ref loaded) = self.current_loaded_image {
            let (color_img, _w, _h) =
                loaded.transform_color_image(
                    self.rotation_deg,
                    self.flip_h,
                    self.flip_v,
                    target_size,
                );

            let handle = ctx.load_texture(
                "current_image",
                color_img,
                egui::TextureOptions::LINEAR,
            );
            self.texture_handle = Some(handle);
            self.rendered_target_size = target_size;
        }
    }

    fn update_texture(&mut self, ctx: &Context) {
        self.rendered_target_size = None;
        self.update_texture_with_target_size(ctx, None);
    }







}

impl eframe::App for ImageViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // バックグラウンドで進行中の画像先読みプロミスを更新
        self.poll_preload_promises();

        // ウィンドウタイトルを開いているファイル名に合わせて動的更新
        let window_title = if let Some(ref img) = self.current_loaded_image {
            let filename = img
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown");
            format!("{} - Rust 画像ビューア", filename)
        } else {
            "Rust 画像ビューア (Image Viewer)".to_string()
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title));

        // ドラッグ＆ドロップ受け取り
        if !ctx.input(|i| i.raw.dropped_files.is_empty()) {
            let dropped = ctx.input(|i| i.raw.dropped_files.clone());
            if let Some(file) = dropped.first() {
                if let Some(path) = &file.path {
                    self.open_image_file(path.clone());
                    self.update_texture(ctx);
                }
            }
        }

        // キーボード操作
        ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.navigate_image(-1);
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                self.navigate_image(1);
            }
            if i.key_pressed(egui::Key::R) {
                self.reset_view();
            }
            if i.key_pressed(egui::Key::F) {
                self.view_mode = ViewMode::FitWindow;
                self.pan_offset = Vec2::ZERO;
            }
            if i.modifiers.command && i.key_pressed(egui::Key::O) {
                if let Some(path) = FileDialog::new()
                    .add_filter("Image Files", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                    .pick_file()
                {
                    self.open_image_file(path);
                }
            }
        });

        // ツールバー (TopPanel) - プロフェッショナルで洗練されたグループ化ツールバー
        egui::TopBottomPanel::top("top_toolbar").show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = Vec2::new(8.0, 0.0);
            ui.style_mut().spacing.button_padding = Vec2::new(8.0, 5.0);

            ui.horizontal_centered(|ui| {
                let text_color = ui.visuals().text_color();
                let icon_style = move |text: &str| {
                    egui::RichText::new(text)
                        .size(14.0)
                        .color(text_color)
                };

                // グループ1: ファイル / サイドバー
                if ui
                    .button(icon_style("🗁 開く"))
                    .on_hover_text("ファイルを開く (Ctrl+O)")
                    .clicked()
                {
                    if let Some(path) = FileDialog::new()
                        .add_filter("Image Files", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                        .pick_file()
                    {
                        self.open_image_file(path);
                    }
                }

                if ui
                    .selectable_label(self.show_sidebar, icon_style("☰ 一覧"))
                    .on_hover_text("画像一覧サイドバーの表示切替")
                    .clicked()
                {
                    self.show_sidebar = !self.show_sidebar;
                    save_sidebar_config(self.show_sidebar);
                }

                ui.separator();

                // グループ2: ズームコントロール
                if ui
                    .button(icon_style("🔍−"))
                    .on_hover_text("縮小")
                    .clicked()
                {
                    self.zoom_factor = (self.zoom_factor / 1.25).max(0.05);
                    self.view_mode = ViewMode::FreeZoom;
                }

                let mut zoom_percent = (self.zoom_factor * 100.0) as i32;
                if ui
                    .add(egui::DragValue::new(&mut zoom_percent).speed(1.0).suffix("%").range(5..=5000))
                    .on_hover_text("拡大率の直接指定 (ドラッグまたはクリック入力)")
                    .changed()
                {
                    self.zoom_factor = (zoom_percent as f32 / 100.0).max(0.05);
                    self.view_mode = ViewMode::FreeZoom;
                }

                if ui
                    .button(icon_style("🔍＋"))
                    .on_hover_text("拡大")
                    .clicked()
                {
                    self.zoom_factor = (self.zoom_factor * 1.25).min(50.0);
                    self.view_mode = ViewMode::FreeZoom;
                }

                if ui
                    .selectable_label(self.view_mode == ViewMode::FitWindow, icon_style("⛶ フィット"))
                    .on_hover_text("ウィンドウにフィット")
                    .clicked()
                {
                    self.view_mode = ViewMode::FitWindow;
                    self.pan_offset = Vec2::ZERO;
                }

                if ui
                    .selectable_label(
                        self.view_mode == ViewMode::OriginalSize,
                        icon_style("1:1 原寸"),
                    )
                    .on_hover_text("原寸大 (100%) 表示")
                    .clicked()
                {
                    self.view_mode = ViewMode::OriginalSize;
                    self.zoom_factor = 1.0;
                    self.pan_offset = Vec2::ZERO;
                }

                ui.separator();

                // グループ3: 変形 (回転・反転)
                if ui
                    .button(icon_style("⟳ 90°回転"))
                    .on_hover_text("90°時計回りに回転")
                    .clicked()
                {
                    self.rotation_deg = (self.rotation_deg + 90) % 360;
                    self.update_texture(ctx);
                }

                if ui
                    .selectable_label(self.flip_h, icon_style("⇄ 左右反転"))
                    .on_hover_text("左右反転")
                    .clicked()
                {
                    self.flip_h = !self.flip_h;
                    self.update_texture(ctx);
                }

                if ui
                    .selectable_label(self.flip_v, icon_style("⇅ 上下反転"))
                    .on_hover_text("上下反転")
                    .clicked()
                {
                    self.flip_v = !self.flip_v;
                    self.update_texture(ctx);
                }

                ui.separator();

                // グループ4: リセット
                if ui
                    .button(icon_style("↺ リセット"))
                    .on_hover_text("表示位置・ズーム・回転をリセット (R)")
                    .clicked()
                {
                    self.reset_view();
                    self.update_texture(ctx);
                }

                // テーマ切替ボタン (右寄せ)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(4.0);

                    let is_dark = ctx.style().visuals.dark_mode;
                    let theme_label = if is_dark { "☀ ライト" } else { "🌙 ダーク" };
                    let theme_tooltip = if is_dark {
                        "ライトモードに切り替え"
                    } else {
                        "ダークモードに切り替え"
                    };

                    if ui
                        .button(icon_style(theme_label))
                        .on_hover_text(theme_tooltip)
                        .clicked()
                    {
                        if is_dark {
                            ctx.set_visuals(egui::Visuals::light());
                        } else {
                            setup_custom_theme(ctx);
                        }
                    }
                });
            });
        });

        // ボトムステータスバー (BottomPanel) - ピルバッジスタイル
        egui::TopBottomPanel::bottom("bottom_status").show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = Vec2::new(8.0, 0.0);
            ui.horizontal(|ui| {
                if let Some(ref img) = self.current_loaded_image {
                    let filename = img
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Unknown");
                    pill_badge(ui, &format!("📄 {}", filename));
                    pill_badge(ui, &format!("📐 {} × {} px", img.width, img.height));
                    pill_badge(ui, &format!("💾 {}", format_bytes(img.file_size_bytes)));
                    pill_badge(ui, &format!("🔍 {:.0}%", self.zoom_factor * 100.0));
                    pill_badge(
                        ui,
                        &format!("📂 {} / {}", self.current_index + 1, self.image_list.len()),
                    );
                } else {
                    pill_badge(ui, "画像を読み込んでください（ドラッグ＆ドロップ可能）");
                }
            });
        });

        // サイドバー (SidePanel) - ピル型アイテム & 選択追従スクロール
        if self.show_sidebar {
            let mut selected_to_open = None;

            egui::SidePanel::left("image_sidebar")
                .default_width(240.0)
                .show(ctx, |ui| {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("📁 画像一覧 ({} 件)", self.image_list.len()))
                            .strong()
                            .size(13.0),
                    );
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    if self.image_list.is_empty() {
                        ui.add_space(10.0);
                        ui.label("（画像が選択されていません）");
                    } else {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for (idx, path) in self.image_list.iter().enumerate() {
                                let name = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("File");
                                let selected = idx == self.current_index;

                                let text = egui::RichText::new(format!("📄 {}", name)).size(12.5);
                                let response = ui.selectable_label(selected, text);

                                if selected {
                                    ui.scroll_to_rect(response.rect, Some(egui::Align::Center));
                                }

                                if response.clicked() {
                                    if idx != self.current_index {
                                        selected_to_open = Some((idx, path.clone()));
                                    }
                                }
                            }
                        });
                    }
                });

            if let Some((idx, path)) = selected_to_open {
                self.current_index = idx;
                self.open_image_file(path);
            }
        }

        // メイン領域 (CentralPanel)
        egui::CentralPanel::default().show(ctx, |ui| {
            // エラー表示
            if let Some(ref err) = self.error_message {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(Color32::RED, err);
                });
                return;
            }

            // 画像なし状態のウェルカム画面（モダンなドロップゾーン）
            if self.current_loaded_image.is_none() {
                ui.centered_and_justified(|ui| {
                    let is_file_hovered = ctx.input(|i| !i.raw.hovered_files.is_empty());
                    let drop_zone_size = Vec2::new(380.0, 220.0);
                    let (rect, _response) = ui.allocate_exact_size(drop_zone_size, Sense::hover());

                    let stroke_color = if is_file_hovered {
                        Color32::from_rgb(99, 102, 241) // Indigo-500
                    } else {
                        Color32::from_rgba_unmultiplied(255, 255, 255, 30)
                    };
                    let bg_color = if is_file_hovered {
                        Color32::from_rgba_unmultiplied(99, 102, 241, 24)
                    } else {
                        Color32::from_rgba_unmultiplied(255, 255, 255, 4)
                    };

                    ui.painter().rect(
                        rect,
                        egui::Rounding::same(12.0),
                        bg_color,
                        egui::Stroke::new(if is_file_hovered { 2.0_f32 } else { 1.0_f32 }, stroke_color),
                    );

                    ui.allocate_ui_at_rect(rect, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(24.0);
                            ui.label(egui::RichText::new("🖼️").size(42.0));
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("画像ファイルをここにドラッグ＆ドロップ")
                                    .size(15.0)
                                    .strong()
                                    .color(if is_file_hovered {
                                        Color32::from_rgb(165, 180, 252)
                                    } else {
                                        ui.visuals().text_color()
                                    }),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("または")
                                    .size(12.0)
                                    .color(Color32::from_rgba_unmultiplied(160, 160, 170, 180)),
                            );
                            ui.add_space(8.0);
                            if ui
                                .button(egui::RichText::new("📂 画像ファイルを選択").size(13.0))
                                .clicked()
                            {
                                if let Some(path) = FileDialog::new()
                                    .add_filter(
                                        "Image Files",
                                        &["png", "jpg", "jpeg", "webp", "gif", "bmp"],
                                    )
                                    .pick_file()
                                {
                                    self.open_image_file(path);
                                }
                            }
                        });
                    });
                });
                return;
            }

            let loaded = self.current_loaded_image.as_ref().unwrap();

            // 描画可能領域のサイズ
            let available_size = ui.available_size();

            let rot = (self.rotation_deg % 360 + 360) % 360;
            let (disp_w, disp_h) = if rot == 90 || rot == 270 {
                (loaded.height, loaded.width)
            } else {
                (loaded.width, loaded.height)
            };

            // ズーム倍率の計算
            let actual_zoom = match self.view_mode {
                ViewMode::FitWindow => {
                    let scale_x = available_size.x / disp_w as f32;
                    let scale_y = available_size.y / disp_h as f32;
                    scale_x.min(scale_y).min(10.0)
                }
                ViewMode::OriginalSize => 1.0,
                ViewMode::FreeZoom => self.zoom_factor,
            };

            if self.view_mode == ViewMode::FitWindow {
                self.zoom_factor = actual_zoom;
            }

            // 現在の解像度 target_w, target_h を算出
            let ppp = ctx.pixels_per_point();
            let target_w = ((disp_w as f32 * actual_zoom * ppp).round() as u32).max(1);
            let target_h = ((disp_h as f32 * actual_zoom * ppp).round() as u32).max(1);

            let target_size = if disp_w > target_w || disp_h > target_h {
                Some((target_w, target_h))
            } else {
                None
            };

            let mut should_update = self.texture_handle.is_none();

            if !should_update && self.rendered_target_size != target_size {
                if self.last_target_size != target_size {
                    self.last_target_size = target_size;
                    self.last_target_size_change_time = std::time::Instant::now();
                }

                if self.last_target_size_change_time.elapsed().as_millis() >= 180 {
                    should_update = true;
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(20));
                }
            }

            if should_update {
                self.update_texture_with_target_size(ctx, target_size);
                self.rendered_target_size = target_size;
                self.last_target_size = target_size;
            }

            if self.texture_handle.is_none() {
                return;
            }

            let texture = self.texture_handle.as_ref().unwrap();

            // 表示する最終描画サイズ
            let display_size = Vec2::new(disp_w as f32 * actual_zoom, disp_h as f32 * actual_zoom);

            let mut pending_navigation: Option<isize> = None;

            // マウス操作用のレスポンス
            let response = ui.allocate_response(available_size, Sense::click_and_drag());
            let is_hovered = ui.rect_contains_pointer(ui.max_rect());

            // ダブルクリックで FitWindow ⇄ OriginalSize 切替
            if response.double_clicked() {
                if self.view_mode == ViewMode::FitWindow {
                    self.view_mode = ViewMode::OriginalSize;
                    self.zoom_factor = 1.0;
                    self.pan_offset = Vec2::ZERO;
                } else {
                    self.view_mode = ViewMode::FitWindow;
                    self.pan_offset = Vec2::ZERO;
                }
            }

            // 右クリックコンテキストメニュー
            response.context_menu(|ui| {
                if let Some(ref img) = self.current_loaded_image {
                    let path_buf = img.path.clone();
                    let path_str = path_buf.to_string_lossy().to_string();

                    if ui.button("📂 エクスプローラーで表示").clicked() {
                        #[cfg(target_os = "windows")]
                        {
                            let _ = std::process::Command::new("explorer")
                                .args(["/select,", &path_str])
                                .spawn();
                        }
                        ui.close_menu();
                    }

                    if ui.button("📋 ファイルパスをコピー").clicked() {
                        ui.ctx().output_mut(|o| o.copied_text = path_str);
                        ui.close_menu();
                    }
                }
            });

            // スクロール入力の取得：MouseWheel イベント優先で画面間の描画ラグ・高速デコードによる連続暴発を完全防止
            let mut wheel_delta_y = 0.0f32;
            ctx.input(|i| {
                for event in &i.events {
                    if let egui::Event::MouseWheel { delta, .. } = event {
                        wheel_delta_y += delta.y;
                    }
                }
            });

            if wheel_delta_y == 0.0 {
                wheel_delta_y = ui.input(|i| i.raw_scroll_delta.y);
            }

            let ctrl_pressed = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
            let elapsed_ms = self.last_scroll_navigate_time.elapsed().as_millis();

            if wheel_delta_y == 0.0 {
                if elapsed_ms > 150 {
                    self.scroll_locked = false;
                }
            } else if is_hovered {
                if ctrl_pressed {
                    // Ctrl + ホイールスクロール = 拡大・縮小 (ズーム)
                    let zoom_multiplier = (wheel_delta_y * 0.035).exp();
                    self.zoom_factor = (self.zoom_factor * zoom_multiplier).clamp(0.05, 50.0);
                    self.view_mode = ViewMode::FreeZoom;
                } else {
                    // 通常のマウスホイール = ページ送り (爆速デコード画像群でも絶対に1ノッチ1枚固定)
                    if !self.scroll_locked && elapsed_ms > 180 {
                        if wheel_delta_y < -0.05 {
                            pending_navigation = Some(1); // 下スクロール -> 次の画像へ
                            self.scroll_locked = true;
                            self.last_scroll_navigate_time = std::time::Instant::now();
                        } else if wheel_delta_y > 0.05 {
                            pending_navigation = Some(-1); // 上スクロール -> 前の画像へ
                            self.scroll_locked = true;
                            self.last_scroll_navigate_time = std::time::Instant::now();
                        }
                    }
                }
            }

            // マウスドラッグによる移動（パン）
            if response.dragged() {
                self.pan_offset += response.drag_delta();
            }

            // 画像の中央配置位置計算
            let rect_center = ui.min_rect().center() + self.pan_offset;
            let image_rect = egui::Rect::from_center_size(rect_center, display_size);

            // 画像の描画
            let painter = ui.painter();
            painter.image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );

            // ナビゲーションボタン（左右のオーバーレイ：カーソル近接時に「ふわぁ」とスムーズにフェードイン/アウト）
            if !self.image_list.is_empty() {
                let left_btn_rect = egui::Rect::from_min_size(
                    ui.min_rect().left_center() - Vec2::new(0.0, 25.0),
                    Vec2::new(34.0, 50.0),
                );
                let right_btn_rect = egui::Rect::from_min_size(
                    ui.min_rect().right_center() - Vec2::new(34.0, 25.0),
                    Vec2::new(34.0, 50.0),
                );

                let pointer_pos = ctx.pointer_latest_pos();

                // ボタン周辺にカーソルが近づいたか判定 (マージン過敏防止のため 6px の適正拡張に調整)
                let left_hovered =
                    pointer_pos.map_or(false, |pos| left_btn_rect.expand(6.0).contains(pos));
                let right_hovered =
                    pointer_pos.map_or(false, |pos| right_btn_rect.expand(6.0).contains(pos));

                // 0.0 〜 1.0 へ「ふわぁ」と滑らかに遷移するアニメーション値を取得
                let left_alpha = ctx.animate_bool_responsive(
                    ui.make_persistent_id("anim_nav_left"),
                    left_hovered,
                );
                let right_alpha = ctx.animate_bool_responsive(
                    ui.make_persistent_id("anim_nav_right"),
                    right_hovered,
                );

                if left_alpha > 0.001 {
                    ui.scope(|ui| {
                        let text_color = ui.visuals().text_color();
                        let alpha_u8 = (left_alpha * 220.0) as u8;
                        ui.visuals_mut().widgets.inactive.bg_fill =
                            Color32::from_black_alpha((left_alpha * 120.0) as u8);
                        ui.visuals_mut().widgets.hovered.bg_fill =
                            Color32::from_black_alpha((left_alpha * 180.0) as u8);
                        ui.visuals_mut().widgets.inactive.fg_stroke.color =
                            Color32::from_rgba_unmultiplied(
                                text_color.r(),
                                text_color.g(),
                                text_color.b(),
                                alpha_u8,
                            );
                        ui.visuals_mut().widgets.hovered.fg_stroke.color = Color32::WHITE;

                        if ui.put(left_btn_rect, egui::Button::new("◀")).clicked() {
                            pending_navigation = Some(-1);
                        }
                    });
                }

                if right_alpha > 0.001 {
                    ui.scope(|ui| {
                        let text_color = ui.visuals().text_color();
                        let alpha_u8 = (right_alpha * 220.0) as u8;
                        ui.visuals_mut().widgets.inactive.bg_fill =
                            Color32::from_black_alpha((right_alpha * 120.0) as u8);
                        ui.visuals_mut().widgets.hovered.bg_fill =
                            Color32::from_black_alpha((right_alpha * 180.0) as u8);
                        ui.visuals_mut().widgets.inactive.fg_stroke.color =
                            Color32::from_rgba_unmultiplied(
                                text_color.r(),
                                text_color.g(),
                                text_color.b(),
                                alpha_u8,
                            );
                        ui.visuals_mut().widgets.hovered.fg_stroke.color = Color32::WHITE;

                        if ui.put(right_btn_rect, egui::Button::new("▶")).clicked() {
                            pending_navigation = Some(1);
                        }
                    });
                }
            }

            // 画面描画後に安全にページ遷移を実行（借用競合の完全防止）
            if let Some(delta) = pending_navigation {
                self.navigate_image(delta);
            }
        });
    }
}

/// ピルバッジ形式のメタデータタグを描画
fn pill_badge(ui: &mut egui::Ui, text: &str) {
    egui::Frame::none()
        .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 14))
        .rounding(egui::Rounding::same(4.0))
        .inner_margin(egui::Margin::symmetric(8.0, 4.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(12.0));
        });
}

/// Content-First / Dark Canvas なカスタムテーマを設定
fn setup_custom_theme(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(24, 24, 27); // #18181b (Slate 900)
    visuals.window_fill = Color32::from_rgb(18, 18, 20); // #121214 (Dark Canvas)
    visuals.extreme_bg_color = Color32::from_rgb(15, 15, 18);
    visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 20));
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(34, 34, 38);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 48, 56);
    visuals.widgets.active.bg_fill = Color32::from_rgb(60, 60, 72);
    visuals.selection.bg_fill = Color32::from_rgb(99, 102, 241); // #6366f1 (Indigo-500)
    visuals.widgets.noninteractive.rounding = egui::Rounding::same(5.0);
    visuals.widgets.inactive.rounding = egui::Rounding::same(5.0);
    visuals.widgets.hovered.rounding = egui::Rounding::same(5.0);
    visuals.widgets.active.rounding = egui::Rounding::same(5.0);
    visuals.widgets.open.rounding = egui::Rounding::same(5.0);
    visuals.window_rounding = egui::Rounding::same(6.0);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(6.0, 4.0);
    style.spacing.button_padding = Vec2::new(8.0, 5.0);
    style.visuals = visuals;
    ctx.set_style(style);
}

/// Windowsシステムフォント（メイリオ / 游ゴシック等）をロードして日本語文字化け（豆腐）を防止
fn setup_japanese_font(ctx: &Context) {
    let mut fonts = egui::FontDefinitions::default();

    let font_paths = [
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\meiryob.ttc",
        "C:\\Windows\\Fonts\\msgothic.ttc",
        "C:\\Windows\\Fonts\\yu Gothic.ttf",
        "C:\\Windows\\Fonts\\yugothm.ttc",
        "C:\\Windows\\Fonts\\yugothb.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msjh.ttc",
    ];

    for path in font_paths {
        if let Ok(font_data) = std::fs::read(path) {
            fonts.font_data.insert(
                "jp_system_font".to_owned(),
                egui::FontData::from_owned(font_data),
            );

            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "jp_system_font".to_owned());

            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "jp_system_font".to_owned());

            ctx.set_fonts(fonts);
            break;
        }
    }
}
