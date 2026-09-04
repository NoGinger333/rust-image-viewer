use egui::Context;

pub mod icons {
    pub const FOLDER_OPEN: &str = "\u{e2c8}";
    pub const FOLDER: &str = "\u{e2c7}";
    pub const MENU: &str = "\u{e5d2}";
    pub const ZOOM_IN: &str = "\u{e8ff}";
    pub const ZOOM_OUT: &str = "\u{e900}";
    pub const FIT_SCREEN: &str = "\u{ea10}";
    pub const ROTATE_RIGHT: &str = "\u{e41a}";
    pub const FLIP: &str = "\u{e3e8}";
    pub const SWAP_VERT: &str = "\u{e8d5}";
    pub const REFRESH: &str = "\u{e5d5}";
    pub const LIGHT_MODE: &str = "\u{e518}";
    pub const DARK_MODE: &str = "\u{e51c}";
    pub const SEARCH: &str = "\u{e8b6}";
    pub const CLOSE: &str = "\u{e5cd}";
    pub const IMAGE: &str = "\u{e3f4}";
    pub const CHEVRON_LEFT: &str = "\u{e5cb}";
    pub const CHEVRON_RIGHT: &str = "\u{e5cc}";
    pub const ASPECT_RATIO: &str = "\u{e85b}";
    pub const SD_STORAGE: &str = "\u{e1c2}";
    pub const COLLECTIONS: &str = "\u{e3b6}";
}

/// Windowsシステムフォント（メイリオ / 游ゴシック等）および記号・絵文字フォントをロードして文字化け（豆腐）を防止
pub fn setup_custom_fonts(ctx: &Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "material_icons".to_owned(),
        egui::FontData {
            font: std::borrow::Cow::Borrowed(include_bytes!("../assets/MaterialIcons-Regular.ttf")),
            index: 0,
            tweak: egui::FontTweak {
                // アイコングリフが行ボックス内で上に寄って描画されるため、
                // 上下の余白を等間隔にする下方オフセット (フォントサイズに比例)
                y_offset_factor: 0.28,
                ..Default::default()
            },
        },
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "material_icons".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "material_icons".to_owned());

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

            break;
        }
    }

    // Windows標準のシンボル＆絵文字フォントをフォールバック登録（⛶, ⟳, ⇄, ⇅, ↺, ☰, 🗁, 📁 等の描画安定化）
    if let Ok(symbol_data) = std::fs::read("C:\\Windows\\Fonts\\seguisym.ttf") {
        fonts.font_data.insert(
            "segoe_ui_symbol".to_owned(),
            egui::FontData::from_owned(symbol_data),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("segoe_ui_symbol".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("segoe_ui_symbol".to_owned());
    }

    if let Ok(emoji_data) = std::fs::read("C:\\Windows\\Fonts\\seguiemj.ttf") {
        fonts.font_data.insert(
            "segoe_ui_emoji".to_owned(),
            egui::FontData::from_owned(emoji_data),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("segoe_ui_emoji".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("segoe_ui_emoji".to_owned());
    }

    ctx.set_fonts(fonts);
}
