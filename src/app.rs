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

    /// 表示補正設定
    sharpen_enabled: bool,
    use_nearest_filter: bool,

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
            sharpen_enabled: false,
            use_nearest_filter: true,
            scroll_locked: false,
            last_scroll_navigate_time: std::time::Instant::now(),
        }
    }
}



impl ImageViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        setup_japanese_font(&cc.egui_ctx);
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

        // キャッシュに既に存在する場合は即座に表示（0ミリ秒ラグ）
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
    }

    /// テクスチャの再生成
    fn update_texture(&mut self, ctx: &Context) {
        if let Some(ref loaded) = self.current_loaded_image {
            let (color_img, _w, _h) =
                loaded.transform_color_image(self.rotation_deg, self.flip_h, self.flip_v, self.sharpen_enabled);

            let texture_options = if self.use_nearest_filter {
                let mut opts = egui::TextureOptions::NEAREST;
                opts.magnification = egui::TextureFilter::Linear;
                opts.minification = egui::TextureFilter::Nearest;
                opts
            } else {
                egui::TextureOptions::LINEAR
            };

            let handle = ctx.load_texture(
                "current_image",
                color_img,
                texture_options,
            );
            self.texture_handle = Some(handle);
        }
    }



}

impl eframe::App for ImageViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // バックグラウンドで進行中の画像先読みプロミスを更新
        self.poll_preload_promises();

        // 現在表示すべきテクスチャが更新されていない場合は再生成
        if self.texture_handle.is_none() && self.current_loaded_image.is_some() {
            self.update_texture(ctx);
        }

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

        // ツールバー (TopPanel) - 超モダン＆洗練されたベクトルUIアイコン
        egui::TopBottomPanel::top("top_toolbar").show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = Vec2::new(14.0, 0.0);
            ui.style_mut().spacing.button_padding = Vec2::new(8.0, 5.0);

            ui.horizontal_centered(|ui| {
                // アイコンスタイルの共通定義（スタイリッシュな単色ミニマルアイコン）
                let text_color = ui.visuals().text_color();
                let icon_style = move |text: &str| {
                    egui::RichText::new(text)
                        .size(17.0)
                        .color(text_color)
                };


                // ファイルを開く
                if ui
                    .add(egui::Button::new(icon_style("🗁")).frame(false))
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

                // サイドバー切替
                if ui
                    .add(egui::Button::new(icon_style("☰")).frame(false))
                    .on_hover_text("サイドバー (画像一覧) の表示切替")
                    .clicked()
                {
                    self.show_sidebar = !self.show_sidebar;
                    save_sidebar_config(self.show_sidebar);
                }

                ui.separator();

                // ズームコントロール (拡大・縮小)
                if ui
                    .add(egui::Button::new(icon_style("🔍+")).frame(false))
                    .on_hover_text("拡大")
                    .clicked()
                {
                    self.zoom_factor *= 1.6;
                    self.view_mode = ViewMode::FreeZoom;
                }

                if ui
                    .add(egui::Button::new(icon_style("🔍-")).frame(false))
                    .on_hover_text("縮小")
                    .clicked()
                {
                    self.zoom_factor /= 1.6;
                    self.view_mode = ViewMode::FreeZoom;
                }

                let mut zoom_percent = (self.zoom_factor * 100.0) as i32;
                if ui
                    .add(egui::DragValue::new(&mut zoom_percent).speed(1.0).suffix("%").range(10..=3000))
                    .on_hover_text("拡大率の直接指定")
                    .changed()

                {
                    self.zoom_factor = (zoom_percent as f32 / 100.0).max(0.1);
                    self.view_mode = ViewMode::FreeZoom;
                }

                if ui
                    .selectable_label(self.view_mode == ViewMode::FitWindow, icon_style("⛶"))
                    .on_hover_text("ウィンドウにフィット")
                    .clicked()
                {
                    self.view_mode = ViewMode::FitWindow;
                    self.pan_offset = Vec2::ZERO;
                }
                if ui
                    .selectable_label(
                        self.view_mode == ViewMode::OriginalSize,
                        egui::RichText::new("1:1").size(14.0).strong(),
                    )
                    .on_hover_text("原寸大 (100%) 表示")
                    .clicked()
                {
                    self.view_mode = ViewMode::OriginalSize;
                    self.zoom_factor = 1.0;
                    self.pan_offset = Vec2::ZERO;
                }

                ui.separator();

                // 表示補正トグルボタン
                if ui
                    .selectable_label(self.sharpen_enabled, icon_style("✒"))
                    .on_hover_text("文字くっきり (シャープネス補正) ON/OFF")
                    .clicked()
                {
                    self.sharpen_enabled = !self.sharpen_enabled;
                    self.update_texture(ctx);
                }

                if ui
                    .selectable_label(self.use_nearest_filter, icon_style("🎯"))
                    .on_hover_text("縮小文字のぼやけ防止 (シャープ補間 / スムース補間) 切替")
                    .clicked()
                {
                    self.use_nearest_filter = !self.use_nearest_filter;
                    self.update_texture(ctx);
                }

                ui.separator();


                // 回転・反転

                if ui
                    .add(egui::Button::new(icon_style("⟳")).frame(false))
                    .on_hover_text("90°時計回りに回転")
                    .clicked()
                {
                    self.rotation_deg = (self.rotation_deg + 90) % 360;
                    self.update_texture(ctx);
                }
                if ui
                    .add(egui::Button::new(icon_style("⇄")).frame(false))
                    .on_hover_text("左右反転")
                    .clicked()
                {
                    self.flip_h = !self.flip_h;
                    self.update_texture(ctx);
                }
                if ui
                    .add(egui::Button::new(icon_style("⇅")).frame(false))
                    .on_hover_text("上下反転")
                    .clicked()
                {
                    self.flip_v = !self.flip_v;
                    self.update_texture(ctx);
                }

                ui.separator();

                // リセット
                if ui
                    .add(egui::Button::new(icon_style("↺")).frame(false))
                    .on_hover_text("表示位置・ズームをリセット")
                    .clicked()
                {
                    self.reset_view();
                    self.update_texture(ctx);
                }

                // テーマ切替ボタン (右寄せ)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);

                    let is_dark = ctx.style().visuals.dark_mode;
                    let theme_icon = if is_dark { "☀" } else { "🌙" };
                    let theme_tooltip = if is_dark {
                        "ライトモードに切り替え"
                    } else {
                        "ダークモードに切り替え"
                    };

                    if ui
                        .add(egui::Button::new(icon_style(theme_icon)).frame(false))
                        .on_hover_text(theme_tooltip)
                        .clicked()
                    {
                        let new_visuals = if is_dark {
                            egui::Visuals::light()
                        } else {
                            egui::Visuals::dark()
                        };
                        ctx.set_visuals(new_visuals);
                    }
                });
            });
        });

        // ボトムステータスバー (BottomPanel)
        egui::TopBottomPanel::bottom("bottom_status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(ref img) = self.current_loaded_image {
                    let filename = img
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Unknown");
                    ui.label(format!("📄 {}", filename));
                    ui.separator();
                    ui.label(format!("📐 {} x {} px", img.width, img.height));
                    ui.separator();
                    ui.label(format!("💾 {}", format_bytes(img.file_size_bytes)));
                    ui.separator();
                    ui.label(format!("🔍 {:.0}%", self.zoom_factor * 100.0));
                    ui.separator();
                    ui.label(format!("📂 {} / {}", self.current_index + 1, self.image_list.len()));
                } else {
                    ui.label("画像を読み込んでください（ドラッグ＆ドロップ可能）");
                }
            });
        });

        // サイドバー (SidePanel) - フォルダに関係なくユーザーのON/OFF設定を常に固定で維持
        if self.show_sidebar {
            let mut selected_to_open = None;

            egui::SidePanel::left("image_sidebar")
                .default_width(220.0)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("📁 フォルダ内一覧").strong());
                    ui.separator();

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

                                if ui.selectable_label(selected, format!("📄 {}", name)).clicked() {
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

            // 画像なし状態のウェルカム画面
            if self.texture_handle.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("📷 Rust 画像ビューア");
                        ui.add_space(10.0);
                        ui.label("画像ファイルをここにドラッグ＆ドロップするか");
                        ui.add_space(5.0);
                        if ui.button("📂 画像ファイルを選択").clicked() {
                            if let Some(path) = FileDialog::new()
                                .add_filter("Image Files", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                                .pick_file()
                            {
                                self.open_image_file(path);
                            }
                        }
                    });
                });
                return;
            }

            let texture = self.texture_handle.as_ref().unwrap();
            let tex_size = texture.size_vec2();

            // 描画可能領域のサイズ
            let available_size = ui.available_size();

            // ズーム倍率の計算
            let actual_zoom = match self.view_mode {
                ViewMode::FitWindow => {
                    let scale_x = available_size.x / tex_size.x;
                    let scale_y = available_size.y / tex_size.y;
                    scale_x.min(scale_y).min(10.0)
                }
                ViewMode::OriginalSize => 1.0,
                ViewMode::FreeZoom => self.zoom_factor,
            };

            if self.view_mode == ViewMode::FitWindow {
                self.zoom_factor = actual_zoom;
            }

            // 表示する最終描画サイズ
            let display_size = tex_size * actual_zoom;

            let mut pending_navigation: Option<isize> = None;

            // マウス操作用のレスポンス
            let response = ui.allocate_response(available_size, Sense::click_and_drag());
            let is_hovered = ui.rect_contains_pointer(ui.max_rect());

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
                let left_hovered = pointer_pos.map_or(false, |pos| left_btn_rect.expand(6.0).contains(pos));
                let right_hovered = pointer_pos.map_or(false, |pos| right_btn_rect.expand(6.0).contains(pos));



                // 0.0 〜 1.0 へ「ふわぁ」と滑らかに遷移するアニメーション値を取得
                let left_alpha = ctx.animate_bool_responsive(ui.make_persistent_id("anim_nav_left"), left_hovered);
                let right_alpha = ctx.animate_bool_responsive(ui.make_persistent_id("anim_nav_right"), right_hovered);

                if left_alpha > 0.001 {
                    ui.scope(|ui| {
                        let text_color = ui.visuals().text_color();
                        let alpha_u8 = (left_alpha * 220.0) as u8;
                        ui.visuals_mut().widgets.inactive.bg_fill = Color32::from_black_alpha((left_alpha * 120.0) as u8);
                        ui.visuals_mut().widgets.hovered.bg_fill = Color32::from_black_alpha((left_alpha * 180.0) as u8);
                        ui.visuals_mut().widgets.inactive.fg_stroke.color = Color32::from_rgba_unmultiplied(text_color.r(), text_color.g(), text_color.b(), alpha_u8);
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
                        ui.visuals_mut().widgets.inactive.bg_fill = Color32::from_black_alpha((right_alpha * 120.0) as u8);
                        ui.visuals_mut().widgets.hovered.bg_fill = Color32::from_black_alpha((right_alpha * 180.0) as u8);
                        ui.visuals_mut().widgets.inactive.fg_stroke.color = Color32::from_rgba_unmultiplied(text_color.r(), text_color.g(), text_color.b(), alpha_u8);
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
