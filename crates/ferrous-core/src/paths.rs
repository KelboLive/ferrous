use std::path::PathBuf;

/// `%APPDATA%\Ferrous` on Windows (falls back to the working directory).
pub fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Ferrous");
    // The app used to be called Ember Studio: carry its settings over once.
    let old = base.join("EmberStudio");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Default folder for recordings: the user's Videos folder.
pub fn default_recording_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}
