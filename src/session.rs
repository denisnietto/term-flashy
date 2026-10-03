use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Default)]
pub struct SessionState {
    pub active: usize,
    pub tabs: Vec<TabState>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct TabState {
    pub title: String,
    pub cwd: Option<String>,
    /// Whether the tab was showing the pending highlight.
    #[serde(default)]
    pub notify: bool,
    /// Font size set by this tab's own zoom; `None` = follows the default.
    #[serde(default)]
    pub font_size: Option<f64>,
}

fn session_file_path() -> std::path::PathBuf {
    glib::user_config_dir().join("term-flashy").join("session.json")
}

pub fn load() -> SessionState {
    std::fs::read_to_string(session_file_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(state: &SessionState) {
    let path = session_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&path, text);
    }
}
