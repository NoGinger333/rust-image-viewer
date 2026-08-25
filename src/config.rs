use std::path::PathBuf;

/// アプリケーションの設定保持構造体
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub show_sidebar: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { show_sidebar: true }
    }
}

/// 設定ファイルの保存先パスを取得する
pub fn get_config_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("rust-image-viewer");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("config.txt")
    } else if let Ok(userprofile) = std::env::var("USERPROFILE") {
        let dir = PathBuf::from(userprofile).join(".rust-image-viewer");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("config.txt")
    } else {
        PathBuf::from("rust_image_viewer_config.txt")
    }
}

/// 設定ファイルを読み込む（旧ファイルからの自動移行処理付き）
pub fn load_config() -> AppConfig {
    let mut config = AppConfig::default();
    let config_path = get_config_path();
    let legacy_config = std::path::Path::new("rust_image_viewer_config.txt");

    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path).ok()
    } else if legacy_config.exists() {
        let legacy_content = std::fs::read_to_string(legacy_config).ok();
        let _ = std::fs::remove_file(legacy_config);
        legacy_content
    } else {
        None
    };

    if legacy_config.exists() {
        let _ = std::fs::remove_file(legacy_config);
    }

    if let Some(content) = content {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("show_sidebar=") {
                config.show_sidebar = val.trim().parse().unwrap_or(true);
            }
        }
    }

    config
}

/// 設定ファイルを保存する
pub fn save_config(config: &AppConfig) {
    let config_path = get_config_path();
    let content = format!("show_sidebar={}\n", config.show_sidebar);
    let _ = std::fs::write(config_path, content);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert!(config.show_sidebar);
    }

    #[test]
    fn test_config_path() {
        let path = get_config_path();
        assert!(path.to_string_lossy().contains("config.txt"));
    }
}
