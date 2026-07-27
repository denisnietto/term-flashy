use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub font_family: Option<String>,
    pub font_size: f64,
    pub trigger_bell: bool,
    pub trigger_exit_code: bool,
    pub trigger_long_command: bool,
    pub long_command_threshold_secs: u64,
    /// Path to an image shown behind the terminal text, like a desktop
    /// wallpaper. `None` = no image, terminal background is solid.
    pub background_image: Option<String>,
    /// How much the terminal's own background color covers the image,
    /// from 0.0 (image fully visible, may hurt text readability) to
    /// 1.0 (image fully hidden). Ignored when there's no background image
    /// (neither `background_image` nor `background_folder` set).
    pub background_dim: f64,
    /// Folder of images to rotate through as the background, picked in
    /// random order without repeats until every image has shown once.
    /// Takes priority over `background_image` when set.
    pub background_folder: Option<String>,
    /// How often (in seconds) to switch to the next image when
    /// `background_folder` is set.
    pub background_rotate_interval_secs: u64,
    /// Restore open tabs and each tab's working directory on startup,
    /// continuously saved as they change (not just on clean quit).
    pub restore_session: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: 11.0,
            trigger_bell: true,
            trigger_exit_code: true,
            trigger_long_command: true,
            long_command_threshold_secs: 10,
            background_image: None,
            background_dim: 0.55,
            background_folder: None,
            background_rotate_interval_secs: 300,
            restore_session: true,
        }
    }
}

impl Config {
    pub fn font_description(&self) -> String {
        let family = self.font_family.as_deref().unwrap_or("Monospace");
        format!("{family} {}", self.font_size)
    }

    pub fn long_command_threshold(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.long_command_threshold_secs)
    }

    /// Alpha to use for the terminal's own background color: opaque
    /// (1.0) when there's no background image, otherwise `background_dim`.
    pub fn terminal_background_alpha(&self) -> f32 {
        if self.background_folder.is_some() || self.background_image.is_some() {
            self.background_dim as f32
        } else {
            1.0
        }
    }

    pub fn background_rotate_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.background_rotate_interval_secs)
    }
}

fn config_file_path() -> std::path::PathBuf {
    glib::user_config_dir().join("term-flashy").join("config.toml")
}

pub fn load() -> Config {
    let path = config_file_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
            eprintln!("term-flashy: failed to parse {}: {err}", path.display());
            Config::default()
        }),
        Err(_) => {
            let config = Config::default();
            save(&config);
            config
        }
    }
}

pub fn save(config: &Config) {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = toml::to_string_pretty(config) {
        let _ = std::fs::write(&path, text);
    }
}
