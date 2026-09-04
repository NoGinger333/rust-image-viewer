use crate::config::{load_config, save_config, AppConfig};
use crate::font::{icons, setup_custom_fonts};
use crate::image_loader::{format_bytes, scan_directory_for_images, transform_image, LoadedImage};
use eframe::egui;
use egui::{Color32, ColorImage, Context, Sense, TextureHandle, Vec2};
use poll_promise::Promise;
use rfd::FileDialog;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// 最大キャッシュ画像数（メモリ節約とレスポンス向上）
const MAX_CACHE_SIZE: usize = 20;
/// キャッシュ合計バイト数の上限（デコード後の原画像データ量合計。超過時は LRU で削除）
const MAX_CACHE_BYTES: u64 = 768 * 1024 * 1024;
/// リサイズ確定までのデバウンス時間
const RESIZE_DEBOUNCE_MS: u128 = 180;

/// 表示モード
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ViewMode {
    FitWindow,
    FreeZoom,
    OriginalSize,
}

/// テクスチャの再生成が必要かを判定するキー (画像世代 + 変形 + リサイズ後サイズ)
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct TextureKey {
    image_generation: u64,
    rotation_deg: i32,
    flip_h: bool,
    flip_v: bool,
    target: Option<(u32, u32)>,
}

pub struct ImageViewerApp {
    /// 現在読み込んでいる画像データ
    current_loaded_image: Option<LoadedImage>,
    /// 表示用eguiテクスチャ
    texture_handle: Option<TextureHandle>,
    /// 現在テクスチャへ反映済みの状態 (画像世代 + 変形 + リサイズ後サイズ)
    rendered_texture_key: Option<TextureKey>,
    /// 現在 texture_handle に入っているピクセルの寸法 (画像切替中の旧フレーム描画用)
    texture_pixel_size: Option<(u32, u32)>,
    /// バックグラウンドで生成中のテクスチャ (完了次第差し替え)
    pending_texture: Option<(TextureKey, Promise<ColorImage>)>,
    /// 画像が切り替わった回数 (テクスチャキーの画像識別子)
    image_generation: u64,
    /// リサイズデバウンス判定用 (ターゲットサイズの変化のみ対象)
    last_target_key: Option<(u32, u32)>,
    last_target_change_time: std::time::Instant,

    /// デコード済み画像のキャッシュ (パス -> (画像, 最終アクセスtick)) ※LRU+容量制限付き
    image_cache: HashMap<PathBuf, (LoadedImage, u64)>,
    /// LRU 用アクセスカウンタ
    cache_tick: u64,
    /// バックグラウンドで先読み中のPromise一覧
    preload_promises: Vec<Promise<(PathBuf, Result<LoadedImage, String>)>>,
    /// 先読み実行中のパス (同一画像の二重デコード防止)
    in_flight_preloads: HashSet<PathBuf>,
    /// キャッシュミス時に非同期ロード中の画像 (UIスレッドをブロックしない)
    pending_loads: Vec<(PathBuf, Promise<Result<LoadedImage, String>>)>,

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

    /// UI状態・設定
    config: AppConfig,
    sidebar_search_query: String,
    error_message: Option<String>,
    /// 前回ウィンドウタイトルへ送信した文字列 (変化時のみ送信)
    last_window_title: String,
    /// サイドバー検索フィルタのキャッシュ (小文字クエリ, 元リストのインデックス一覧)
    sidebar_filter: (String, Vec<usize>),
    /// サイドバーで最後に自動スクロールした選択インデックス
    last_sidebar_selected: Option<usize>,

    /// ホイールによるページ移動用ロック＆タイマー
    scroll_locked: bool,
    last_scroll_navigate_time: std::time::Instant,
}

impl Default for ImageViewerApp {
    fn default() -> Self {
        let config = load_config();
        Self {
            current_loaded_image: None,
            texture_handle: None,
            rendered_texture_key: None,
            texture_pixel_size: None,
            pending_texture: None,
            image_generation: 0,
            last_target_key: None,
            last_target_change_time: std::time::Instant::now(),
            image_cache: HashMap::new(),
            cache_tick: 0,
            preload_promises: Vec::new(),
            in_flight_preloads: HashSet::new(),
            pending_loads: Vec::new(),
            image_list: Vec::new(),
            current_index: 0,
            zoom_factor: 1.0,
            pan_offset: Vec2::ZERO,
            rotation_deg: 0,
            flip_h: false,
            flip_v: false,
            view_mode: ViewMode::FitWindow,
            config,
            sidebar_search_query: String::new(),
            error_message: None,
            last_window_title: String::new(),
            sidebar_filter: (String::new(), Vec::new()),
            last_sidebar_selected: None,
            scroll_locked: false,
            last_scroll_navigate_time: std::time::Instant::now(),
        }
    }
}

impl ImageViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        setup_custom_fonts(&cc.egui_ctx);
        let mut app = Self::default();
        if let Some(path) = initial_path {
            app.open_image_file(path);
        }
        app
    }

    /// 画像の読み込み処理 (フォルダ再走査のキャッシュ & バックグラウンド先読みの最適化)
    fn open_image_file(&mut self, path: PathBuf) {
        self.error_message = None;

        let parent_dir = path.parent().map(|p| p.to_path_buf());

        // フォルダが変わっていない場合は再走査（重いディスクアクセス）をスキップ
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

        // キャッシュに既に存在する場合は即座に表示（Arc参照クローンによりO(1)取得）
        if let Some(cached_image) = self.cache_get(&path) {
            self.invalidate_texture();
            self.current_loaded_image = Some(cached_image);
            self.reset_view();
            self.trigger_preloading();
            return;
        }

        // すでに同じ画像を非同期ロード中なら何もしない
        if self.pending_loads.iter().any(|(p, _)| p == &path) {
            return;
        }

        // キャッシュにない場合はバックグラウンドで非同期ロード (UIスレッドをブロックしない)
        let path_clone = path.clone();
        let promise = Promise::spawn_thread("load_image", move || {
            LoadedImage::load_from_path(&path_clone).map_err(|e| e.to_string())
        });
        self.pending_loads.push((path, promise));
    }

    /// 表示画像が変わる際にテクスチャ関連の状態をリセット
    /// (旧テクスチャは新しい画像の変換が完成するまで表示を維持し、切替時のちらつきを防止。
    ///  完成次第 TextureHandle::set() で同一テクスチャIDのまま差し替えるためブランクは出ない)
    fn invalidate_texture(&mut self) {
        self.image_generation += 1;
        self.pending_texture = None;
        self.rendered_texture_key = None;
    }

    /// キャッシュ参照 (LRU のアクセス時刻を更新)
    fn cache_get(&mut self, path: &std::path::Path) -> Option<LoadedImage> {
        if let Some((img, tick)) = self.image_cache.get_mut(path) {
            self.cache_tick += 1;
            *tick = self.cache_tick;
            Some(img.clone())
        } else {
            None
        }
    }

    /// キャッシュミスで起動した非同期ロードの完了を取り込む (表示中の画像が完成次第即表示)
    fn poll_pending_loads(&mut self) {
        if self.pending_loads.is_empty() {
            return;
        }

        let current_path = self.image_list.get(self.current_index).cloned();
        let mut loaded_current: Option<LoadedImage> = None;
        let mut failed_current: Option<String> = None;
        self.cache_tick += 1;
        let tick = self.cache_tick;
        {
            let cache = &mut self.image_cache;
            self.pending_loads.retain_mut(|(path, promise)| {
                if let Some(result) = promise.ready() {
                    let result = result.clone();
                    match result {
                        Ok(loaded_img) => {
                            if current_path.as_ref() == Some(path) {
                                loaded_current = Some(loaded_img.clone());
                            }
                            cache.insert(path.clone(), (loaded_img, tick));
                        }
                        Err(e) => {
                            if current_path.as_ref() == Some(path) {
                                failed_current = Some(e);
                            }
                        }
                    }
                    false
                } else {
                    true
                }
            });
        }

        if let Some(loaded_img) = loaded_current {
            self.invalidate_texture();
            self.current_loaded_image = Some(loaded_img);
            self.reset_view();
            self.trigger_preloading();
        } else if let Some(err) = failed_current {
            self.error_message = Some(format!("画像の読み込みに失敗しました: {}", err));
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

        // LRU + 合計バイト数でキャッシュを削限 (表示中・先読み対象・ロード中は必ず保持)
        let keep_set: HashSet<PathBuf> = paths_to_preload
            .iter()
            .chain(std::iter::once(&self.image_list[self.current_index]))
            .chain(self.pending_loads.iter().map(|(p, _)| p))
            .cloned()
            .collect();
        loop {
            let total_bytes: u64 = self
                .image_cache
                .values()
                .map(|(img, _)| img.original_image.as_bytes().len() as u64)
                .sum();
            if self.image_cache.len() <= MAX_CACHE_SIZE && total_bytes <= MAX_CACHE_BYTES {
                break;
            }
            // 保護対象以外からもっとも古く使われた画像を 1 つ削除
            let victim = self
                .image_cache
                .iter()
                .filter(|(path, _)| !keep_set.contains(*path))
                .min_by_key(|(_, (_, tick))| *tick)
                .map(|(path, _)| path.clone());
            match victim {
                Some(path) => {
                    self.image_cache.remove(&path);
                }
                None => break, // 削除候補なし
            }
        }

        // まだ先読み中・キャッシュ済み・非同期ロード中でない画像のみ非同期ロード (二重起動防止)
        for path in paths_to_preload {
            if !self.image_cache.contains_key(&path)
                && !self.pending_loads.iter().any(|(p, _)| p == &path)
                && self.in_flight_preloads.insert(path.clone())
            {
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
                let path = path.clone();
                let result = result.clone();
                self.in_flight_preloads.remove(&path);
                if let Ok(loaded_img) = result {
                    self.cache_tick += 1;
                    self.image_cache.insert(path, (loaded_img, self.cache_tick));
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

    /// 視点のリセット (テクスチャは中央パネルのキー比較で必要時のみ自動再生成)
    fn reset_view(&mut self) {
        self.zoom_factor = 1.0;
        self.pan_offset = Vec2::ZERO;
        self.rotation_deg = 0;
        self.flip_h = false;
        self.flip_v = false;
        self.view_mode = ViewMode::FitWindow;
    }

    // テクスチャの再生成は update() の中央パネル処理で TextureKey 比較に基づき
    // バックグラウンドスレッド (transform_image) で行うため、同期版メソッドは廃止。
}

impl eframe::App for ImageViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // バックグラウンドで進行中の画像先読み・ロードのプロミスを更新
        self.poll_preload_promises();
        self.poll_pending_loads();

        // ウィンドウタイトルを開いているファイル名に合わせて動的更新 (変化時のみ送信)
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
        if self.last_window_title != window_title {
            self.last_window_title = window_title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title));
        }

        // ドラッグ＆ドロップ受け取り
        if !ctx.input(|i| i.raw.dropped_files.is_empty()) {
            let dropped = ctx.input(|i| i.raw.dropped_files.clone());
            if let Some(file) = dropped.first() {
                if let Some(path) = &file.path {
                    self.open_image_file(path.clone());
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
        // パネル高さを固定し、horizontal_centered による自動センタリングで
        // アイコンの上下の余白を常に等間隔に保つ (egui が DPI を自動スケール)
        const TOOLBAR_HEIGHT: f32 = 44.0; // ボタン28pt + 上下マージン8ptずつ
        egui::TopBottomPanel::top("top_toolbar")
            .exact_height(TOOLBAR_HEIGHT)
            .show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = Vec2::new(12.0, 0.0);

            // セパレーターの主張を抑制（半透明グレー）
            let sep_color = if ctx.style().visuals.dark_mode {
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30)
            } else {
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 30)
            };
            ui.visuals_mut().widgets.noninteractive.bg_stroke.color = sep_color;

            ui.horizontal_centered(|ui| {
                // アイコンスタイルの共通定義（スタイリッシュな単色ミニマルアイコン）
                let text_color = ui.visuals().text_color();
                let icon_text = move |text: &str| {
                    egui::RichText::new(text)
                        .size(16.0)
                        .color(text_color)
                };

                const BTN_SIZE: Vec2 = Vec2::new(28.0, 28.0);
                let icon_btn = |text: &str| {
                    egui::Button::new(icon_text(text))
                        .min_size(BTN_SIZE)
                        .frame(false)
                };
                let toggle_btn = |text: egui::RichText, is_selected: bool| {
                    egui::Button::new(text)
                        .min_size(BTN_SIZE)
                        .frame(is_selected)
                        .rounding(egui::Rounding::same(4.0))
                        .selected(is_selected)
                };

                // ファイルを開く
                if ui
                    .add(icon_btn(icons::FOLDER_OPEN))
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
                    .add(icon_btn(icons::MENU))
                    .on_hover_text("サイドバー (画像一覧) の表示切替")
                    .clicked()
                {
                    self.config.show_sidebar = !self.config.show_sidebar;
                    save_config(&self.config);
                }

                ui.separator();

                // ズームコントロール (拡大・縮小)
                if ui
                    .add(icon_btn(icons::ZOOM_IN))
                    .on_hover_text("拡大")
                    .clicked()
                {
                    self.zoom_factor *= 1.6;
                    self.view_mode = ViewMode::FreeZoom;
                }

                if ui
                    .add(icon_btn(icons::ZOOM_OUT))
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
                    .add(toggle_btn(icon_text(icons::FIT_SCREEN), self.view_mode == ViewMode::FitWindow))
                    .on_hover_text("ウィンドウにフィット")
                    .clicked()
                {
                    self.view_mode = ViewMode::FitWindow;
                    self.pan_offset = Vec2::ZERO;
                }
                if ui
                    .add(toggle_btn(
                        egui::RichText::new("1:1").size(13.0).strong().color(text_color),
                        self.view_mode == ViewMode::OriginalSize,
                    ))
                    .on_hover_text("原寸大 (100%) 表示")
                    .clicked()
                {
                    self.view_mode = ViewMode::OriginalSize;
                    self.zoom_factor = 1.0;
                    self.pan_offset = Vec2::ZERO;
                }

                ui.separator();

                // 回転・反転
                if ui
                    .add(icon_btn(icons::ROTATE_RIGHT))
                    .on_hover_text("90°時計回りに回転")
                    .clicked()
                {
                    self.rotation_deg = (self.rotation_deg + 90) % 360;
                }
                if ui
                    .add(icon_btn(icons::FLIP))
                    .on_hover_text("左右反転")
                    .clicked()
                {
                    self.flip_h = !self.flip_h;
                }
                if ui
                    .add(icon_btn(icons::SWAP_VERT))
                    .on_hover_text("上下反転")
                    .clicked()
                {
                    self.flip_v = !self.flip_v;
                }

                ui.separator();

                // リセット
                if ui
                    .add(icon_btn(icons::REFRESH))
                    .on_hover_text("表示位置・ズームをリセット")
                    .clicked()
                {
                    self.reset_view();
                }

                // テーマ切替ボタン (右寄せ)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);

                    let is_dark = ctx.style().visuals.dark_mode;
                    let theme_icon = if is_dark { icons::LIGHT_MODE } else { icons::DARK_MODE };
                    let theme_tooltip = if is_dark {
                        "ライトモードに切り替え"
                    } else {
                        "ダークモードに切り替え"
                    };

                    if ui
                        .add(icon_btn(theme_icon))
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
                    ui.label(format!("{} {}", icons::IMAGE, filename));
                    ui.separator();
                    ui.label(format!("{} {} x {} px", icons::ASPECT_RATIO, img.width, img.height));
                    ui.separator();
                    ui.label(format!("{} {}", icons::SD_STORAGE, format_bytes(img.file_size_bytes)));
                    ui.separator();
                    ui.label(format!("{} {:.0}%", icons::ZOOM_IN, self.zoom_factor * 100.0));
                    ui.separator();
                    ui.label(format!("{} {} / {}", icons::COLLECTIONS, self.current_index + 1, self.image_list.len()));
                } else {
                    ui.label("画像を読み込んでください（ドラッグ＆ドロップ可能）");
                }
            });
        });

        // サイドバー (SidePanel) - フォルダに関係なくユーザーのON/OFF設定を常に固定で維持
        if self.config.show_sidebar {
            let mut selected_to_open = None;

            // 検索クエリが変わったときだけフィルタ結果を再計算 (毎フレームの全件アロケート回避)
            let is_filtered = !self.sidebar_search_query.is_empty();
            let query = self.sidebar_search_query.to_lowercase();
            if self.sidebar_filter.0 != query {
                self.sidebar_filter.0 = query.clone();
                self.sidebar_filter.1 = if query.is_empty() {
                    (0..self.image_list.len()).collect()
                } else {
                    self.image_list
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| {
                            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            name.to_lowercase().contains(&query)
                        })
                        .map(|(i, _)| i)
                        .collect()
                };
                self.last_sidebar_selected = None; // フィルタ変更時は選択位置へ再センタリング
            }
            let filtered_indices = &self.sidebar_filter.1;
            let filtered_count = filtered_indices.len();

            egui::SidePanel::left("image_sidebar")
                .default_width(175.0)
                .width_range(120.0..=400.0)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    let header_text = if !is_filtered {
                        format!("{} フォルダ内一覧 ({} 件)", icons::FOLDER, self.image_list.len())
                    } else {
                        format!("{} フォルダ内一覧 ({} / {} 件)", icons::FOLDER, filtered_count, self.image_list.len())
                    };
                    ui.label(egui::RichText::new(header_text).strong());

                    if self.image_list.len() > 1 {
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            let width = if is_filtered {
                                ui.available_width() - 28.0
                            } else {
                                ui.available_width()
                            };
                            ui.add(
                                egui::TextEdit::singleline(&mut self.sidebar_search_query)
                                    .hint_text(format!("{} ファイル名で検索...", icons::SEARCH))
                                    .desired_width(width),
                            );
                            if is_filtered
                                && ui.button(icons::CLOSE).on_hover_text("検索をクリア").clicked()
                            {
                                self.sidebar_search_query.clear();
                            }
                        });
                    }

                    ui.separator();

                    if self.image_list.is_empty() {
                        ui.add_space(10.0);
                        ui.label("（画像が選択されていません）");
                    } else if is_filtered && filtered_count == 0 {
                        ui.add_space(10.0);
                        ui.label("一致する画像がありません");
                    } else {
                        const ROW_HEIGHT: f32 = 30.0; // 24pt の行 + 6pt の間隔

                        // 選択画像が変わったときだけ一覧の中央へ自動スクロール (仮想化のためオフセット指定)
                        let mut scroll_to_row = None;
                        if self.last_sidebar_selected != Some(self.current_index) {
                            scroll_to_row = filtered_indices
                                .iter()
                                .position(|&i| i == self.current_index);
                            self.last_sidebar_selected = Some(self.current_index);
                        }
                        let visible_rows = (ui.available_height() / ROW_HEIGHT).max(1.0);

                        let mut scroll_area =
                            egui::ScrollArea::vertical().auto_shrink([false, false]);
                        if let Some(row) = scroll_to_row {
                            let center_offset =
                                (row as f32 * ROW_HEIGHT) - (visible_rows * ROW_HEIGHT / 2.0);
                            scroll_area =
                                scroll_area.vertical_scroll_offset(center_offset.max(0.0));
                        }

                        // 表示中の行のウィジェットのみ生成 (大量画像フォルダでもフレーム時間が増えない)
                        scroll_area.show_rows(ui, ROW_HEIGHT, filtered_indices.len(), |ui, range| {
                            for row in range {
                                let idx = filtered_indices[row];
                                let path = &self.image_list[idx];
                                let name = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("File");

                                let selected = idx == self.current_index;
                                let response = ui.add_sized(
                                    [ui.available_width(), 24.0],
                                    egui::SelectableLabel::new(
                                        selected,
                                        format!("{} {}", icons::IMAGE, name),
                                    ),
                                );

                                if response.clicked() && idx != self.current_index {
                                    selected_to_open = Some((idx, path.clone()));
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
            let Some(ref loaded) = self.current_loaded_image else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(icons::IMAGE).size(48.0));
                        ui.heading("Rust 画像ビューア");
                        ui.add_space(10.0);
                        ui.label("画像ファイルをここにドラッグ＆ドロップするか");
                        ui.add_space(5.0);
                        if ui
                            .button(format!("{} 画像ファイルを選択", icons::FOLDER_OPEN))
                            .clicked()
                        {
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
            };

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

            // テクスチャキー (画像世代 + 変形 + リサイズ後サイズ)
            let texture_key = TextureKey {
                image_generation: self.image_generation,
                rotation_deg: rot,
                flip_h: self.flip_h,
                flip_v: self.flip_v,
                target: target_size,
            };

            // テクスチャの変形・リサイズ生成はバックグラウンドスレッドで実施 (UIの固まり防止)
            if self.rendered_texture_key != Some(texture_key) {
                let pending_matches =
                    matches!(&self.pending_texture, Some((k, _)) if *k == texture_key);
                let mut spawn_now = false;
                let mut waiting = false;

                if pending_matches {
                    // 生成済みテクスチャの取り込み (ハンドル再利用で GPU 再確保を削減)
                    let done = self
                        .pending_texture
                        .as_mut()
                        .expect("pending_texture exists")
                        .1
                        .ready()
                        .is_some();
                    if done {
                        let (_, promise) = self.pending_texture.take().unwrap();
                        let color_img = promise.block_until_ready().clone();
                        let [tw, th] = color_img.size;
                        if let Some(handle) = self.texture_handle.as_mut() {
                            handle.set(color_img, egui::TextureOptions::LINEAR);
                        } else {
                            self.texture_handle = Some(ctx.load_texture(
                                "current_image",
                                color_img,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        self.texture_pixel_size = Some((tw as u32, th as u32));
                        self.rendered_texture_key = Some(texture_key);
                    } else {
                        waiting = true;
                    }
                } else {
                    // ターゲットサイズのみの変化 (ウィンドウリサイズ/ズーム) はデバウンス、
                    // 回転・反転・画像切替は即座に再生成
                    let only_target_changed = match self.rendered_texture_key {
                        Some(k) => {
                            k.image_generation == texture_key.image_generation
                                && k.rotation_deg == texture_key.rotation_deg
                                && k.flip_h == texture_key.flip_h
                                && k.flip_v == texture_key.flip_v
                        }
                        None => false,
                    };

                    if only_target_changed && self.texture_handle.is_some() {
                        if self.last_target_key != target_size {
                            self.last_target_key = target_size;
                            self.last_target_change_time = std::time::Instant::now();
                        }
                        if self.last_target_change_time.elapsed().as_millis()
                            >= RESIZE_DEBOUNCE_MS
                        {
                            spawn_now = true;
                        } else {
                            waiting = true;
                        }
                    } else {
                        spawn_now = true;
                    }

                    if spawn_now {
                        let original_image = Arc::clone(&loaded.original_image);
                        self.pending_texture = Some((
                            texture_key,
                            Promise::spawn_thread("transform_image", move || {
                                transform_image(
                                    &original_image,
                                    texture_key.rotation_deg,
                                    texture_key.flip_h,
                                    texture_key.flip_v,
                                    texture_key.target,
                                )
                                .0
                            }),
                        ));
                    }
                }

                if waiting || spawn_now {
                    ctx.request_repaint_after(std::time::Duration::from_millis(15));
                }
            }

            let Some(ref texture) = self.texture_handle else {
                return;
            };

            // 表示する最終描画サイズ
            // 新テクスチャの生成完了前は旧テクスチャが表示されているため、
            // 旧ピクセルのアスペクト比を維持したまま描画する (切替時の歪み・ちらつき防止)
            let stale = self.rendered_texture_key != Some(texture_key);
            let (draw_w, draw_h, draw_zoom) = if stale {
                match self.texture_pixel_size {
                    Some((tw, th)) => {
                        let zoom = match self.view_mode {
                            ViewMode::FitWindow => {
                                let sx = available_size.x / tw as f32;
                                let sy = available_size.y / th as f32;
                                sx.min(sy).min(10.0)
                            }
                            ViewMode::OriginalSize => 1.0,
                            ViewMode::FreeZoom => self.zoom_factor,
                        };
                        (tw as f32, th as f32, zoom)
                    }
                    None => (disp_w as f32, disp_h as f32, actual_zoom),
                }
            } else {
                (disp_w as f32, disp_h as f32, actual_zoom)
            };

            let display_size = Vec2::new(draw_w * draw_zoom, draw_h * draw_zoom);

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

            // 非同期ロード中のインジケータ (現在の画像がバックグラウンドで読み込み中のとき表示)
            let loading_current = self.pending_loads.iter().any(|(p, _)| {
                self.image_list.get(self.current_index).map(|cp| cp == p) == Some(true)
            });
            if loading_current {
                let spinner_rect = egui::Rect::from_min_size(
                    ui.max_rect().right_top() + Vec2::new(-52.0, 12.0),
                    Vec2::new(36.0, 36.0),
                );
                ui.put(spinner_rect, egui::Spinner::new().size(32.0));
            }

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
                let left_hovered = pointer_pos.is_some_and(|pos| left_btn_rect.expand(6.0).contains(pos));
                let right_hovered = pointer_pos.is_some_and(|pos| right_btn_rect.expand(6.0).contains(pos));

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

                        if ui
                            .put(
                                left_btn_rect,
                                egui::Button::new(
                                    egui::RichText::new(icons::CHEVRON_LEFT).size(22.0),
                                ),
                            )
                            .clicked()
                        {
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

                        if ui
                            .put(
                                right_btn_rect,
                                egui::Button::new(
                                    egui::RichText::new(icons::CHEVRON_RIGHT).size(22.0),
                                ),
                            )
                            .clicked()
                        {
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
