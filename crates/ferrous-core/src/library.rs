//! Profiles (settings sets) and scene collections, OBS-style.
//!
//! Layout under the config folder:
//! - `profiles/<name>.json`: one `Settings` each
//! - `collections/<name>.json`: one `SceneCollection` each
//! - `active.json`: which of each is in use
//!
//! Pre-profile installs (`settings.json` / `scenes.json`) are migrated to "Default".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{SceneCollection, Settings};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Active {
    pub profile: String,
    pub collection: String,
}

impl Default for Active {
    fn default() -> Self {
        Self { profile: "Default".into(), collection: "Default".into() }
    }
}

pub struct Library {
    root: PathBuf,
}

/// Keeps names usable as file names on Windows.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() { "Untitled".into() } else { cleaned }
}

impl Library {
    pub fn open(root: &Path) -> Self {
        let lib = Self { root: root.to_path_buf() };
        let _ = std::fs::create_dir_all(lib.dir("profiles"));
        let _ = std::fs::create_dir_all(lib.dir("collections"));
        lib.migrate();
        lib
    }

    pub fn default_root() -> Self {
        Self::open(&crate::paths::config_dir())
    }

    fn dir(&self, kind: &str) -> PathBuf {
        self.root.join(kind)
    }

    fn migrate(&self) {
        let old_settings = self.root.join("settings.json");
        let p = self.profile_path("Default");
        if old_settings.is_file() && !p.exists() {
            let _ = std::fs::rename(&old_settings, &p);
        }
        let old_scenes = self.root.join("scenes.json");
        let c = self.collection_path("Default");
        if old_scenes.is_file() && !c.exists() {
            let _ = std::fs::rename(&old_scenes, &c);
        }
    }

    pub fn profile_path(&self, name: &str) -> PathBuf {
        self.dir("profiles").join(format!("{}.json", sanitize(name)))
    }

    pub fn collection_path(&self, name: &str) -> PathBuf {
        self.dir("collections").join(format!("{}.json", sanitize(name)))
    }

    fn list(&self, kind: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dir(kind))
            .map(|d| {
                d.flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension()? == "json" { Some(p.file_stem()?.to_string_lossy().into_owned()) } else { None }
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|s| s.to_lowercase());
        v
    }

    pub fn profiles(&self) -> Vec<String> {
        self.list("profiles")
    }

    pub fn collections(&self) -> Vec<String> {
        self.list("collections")
    }

    pub fn active(&self) -> Active {
        std::fs::read_to_string(self.root.join("active.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn set_active(&self, a: &Active) -> anyhow::Result<()> {
        std::fs::write(self.root.join("active.json"), serde_json::to_string_pretty(a)?)?;
        Ok(())
    }

    pub fn load_profile(&self, name: &str) -> Settings {
        Settings::load_or_default(&self.profile_path(name))
    }

    pub fn save_profile(&self, name: &str, s: &Settings) -> anyhow::Result<()> {
        s.save(&self.profile_path(name))
    }

    pub fn load_collection(&self, name: &str) -> SceneCollection {
        SceneCollection::load(&self.collection_path(name)).unwrap_or_default()
    }

    pub fn save_collection(&self, name: &str, c: &SceneCollection) -> anyhow::Result<()> {
        c.save(&self.collection_path(name))
    }

    /// Returns a name that doesn't collide with an existing one of this kind.
    pub fn unique_name(&self, kind: Kind, wanted: &str) -> String {
        let existing = match kind {
            Kind::Profile => self.profiles(),
            Kind::Collection => self.collections(),
        };
        let base = sanitize(wanted);
        if !existing.iter().any(|e| e.eq_ignore_ascii_case(&base)) {
            return base;
        }
        (2..).map(|n| format!("{base} {n}")).find(|n| !existing.iter().any(|e| e.eq_ignore_ascii_case(n))).unwrap()
    }

    pub fn rename(&self, kind: Kind, from: &str, to: &str) -> anyhow::Result<String> {
        let to = self.unique_name(kind, to);
        let (a, b) = match kind {
            Kind::Profile => (self.profile_path(from), self.profile_path(&to)),
            Kind::Collection => (self.collection_path(from), self.collection_path(&to)),
        };
        std::fs::rename(a, b)?;
        Ok(to)
    }

    /// Deletes one, refusing to delete the last remaining.
    pub fn remove(&self, kind: Kind, name: &str) -> anyhow::Result<()> {
        let (list, path) = match kind {
            Kind::Profile => (self.profiles(), self.profile_path(name)),
            Kind::Collection => (self.collections(), self.collection_path(name)),
        };
        if list.len() <= 1 {
            anyhow::bail!("can't remove the only one");
        }
        std::fs::remove_file(path)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Profile,
    Collection,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_list_rename_remove() {
        let root = std::env::temp_dir().join(format!("ferrous-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut s = Settings::default();
        s.video.fps = 30;
        s.save(&root.join("settings.json")).unwrap();

        let lib = Library::open(&root);
        assert_eq!(lib.profiles(), vec!["Default"]);
        assert_eq!(lib.load_profile("Default").video.fps, 30, "migrated");

        lib.save_profile("Streaming", &Settings::default()).unwrap();
        assert_eq!(lib.unique_name(Kind::Profile, "streaming"), "streaming 2");
        let renamed = lib.rename(Kind::Profile, "Streaming", "TikTok: Vertical").unwrap();
        assert_eq!(renamed, "TikTok_ Vertical");
        assert_eq!(lib.profiles(), vec!["Default", "TikTok_ Vertical"]);
        lib.remove(Kind::Profile, "Default").unwrap();
        assert!(lib.remove(Kind::Profile, "TikTok_ Vertical").is_err(), "keeps the last one");

        lib.set_active(&Active { profile: "A".into(), collection: "B".into() }).unwrap();
        assert_eq!(lib.active().collection, "B");
        let _ = std::fs::remove_dir_all(&root);
    }
}
