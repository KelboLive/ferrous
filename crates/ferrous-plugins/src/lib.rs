//! Plugin host: discovers plugin DLLs, keeps them loaded, and hands out
//! instances that clean up after themselves.

use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use ferrous_core::VideoFrame;
use ferrous_plugin_api::*;
use libloading::Library;
use parking_lot::RwLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    VideoSource,
    VideoFilter,
    AudioFilter,
}

#[derive(Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub file: PathBuf,
    create: CreateFn,
    destroy: DestroyFn,
    video_size: Option<VideoSizeFn>,
    video_render: Option<VideoRenderFn>,
    audio_process: Option<AudioProcessFn>,
    lib: Arc<Library>,
}

fn registry() -> &'static RwLock<Vec<PluginInfo>> {
    static R: OnceLock<RwLock<Vec<PluginInfo>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Standard plugin folders: `plugins\` next to the exe and `%APPDATA%\Ferrous\plugins`.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut v = vec![ferrous_core::paths::config_dir().join("plugins")];
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("plugins"))) {
        v.push(dir);
    }
    v
}

fn is_library(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()), Some("dll" | "so" | "dylib"))
}

/// Loads one plugin library. Returns the plugin ids it registered.
pub fn load_file(path: &Path) -> anyhow::Result<Vec<String>> {
    // SAFETY: loading a library runs its initialisers; plugins are trusted
    // code the user chose to install, exactly like OBS plugins.
    let lib = Arc::new(unsafe { Library::new(path) }?);
    let module = unsafe {
        let entry: libloading::Symbol<ModuleFn> = lib.get(FERROUS_PLUGIN_ENTRY)?;
        &*entry()
    };
    if module.abi_version != FERROUS_PLUGIN_ABI_VERSION {
        anyhow::bail!("{} uses plugin ABI {}, this build supports {}", path.display(), module.abi_version, FERROUS_PLUGIN_ABI_VERSION);
    }
    let descs = unsafe { std::slice::from_raw_parts(module.plugins, module.count as usize) };
    let mut ids = Vec::new();
    let mut reg = registry().write();
    for d in descs {
        let id = unsafe { CStr::from_ptr(d.id) }.to_string_lossy().into_owned();
        let name = unsafe { CStr::from_ptr(d.name) }.to_string_lossy().into_owned();
        let kind = match d.kind {
            FERROUS_KIND_VIDEO_SOURCE if d.video_size.is_some() && d.video_render.is_some() => Kind::VideoSource,
            FERROUS_KIND_VIDEO_FILTER if d.video_render.is_some() => Kind::VideoFilter,
            FERROUS_KIND_AUDIO_FILTER if d.audio_process.is_some() => Kind::AudioFilter,
            k => {
                log::warn!("plugin {id}: invalid kind {k} or missing callbacks; skipped");
                continue;
            }
        };
        reg.retain(|p| p.id != id);
        reg.push(PluginInfo {
            id: id.clone(),
            name,
            kind,
            file: path.to_path_buf(),
            create: d.create,
            destroy: d.destroy,
            video_size: d.video_size,
            video_render: d.video_render,
            audio_process: d.audio_process,
            lib: lib.clone(),
        });
        ids.push(id);
    }
    Ok(ids)
}

/// Loads every library in `dirs`, logging (not failing on) bad ones.
pub fn load_dirs(dirs: &[PathBuf]) -> Vec<String> {
    let mut problems = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if is_library(&p) {
                match load_file(&p) {
                    Ok(ids) => log::info!("loaded plugins {ids:?} from {}", p.display()),
                    Err(e) => problems.push(format!("{}: {e}", p.display())),
                }
            }
        }
    }
    problems
}

pub fn list() -> Vec<(String, String, Kind)> {
    registry().read().iter().map(|p| (p.id.clone(), p.name.clone(), p.kind)).collect()
}

pub struct Instance {
    ptr: *mut c_void,
    info: PluginInfo,
    start: Instant,
}

// The host never uses an instance from two threads at once.
unsafe impl Send for Instance {}

impl Instance {
    pub fn new(id: &str, settings_json: &str, kind: Kind) -> anyhow::Result<Self> {
        let info = registry().read().iter().find(|p| p.id == id).cloned().ok_or_else(|| anyhow::anyhow!("plugin \"{id}\" isn't installed"))?;
        if info.kind != kind {
            anyhow::bail!("plugin \"{id}\" is a {:?}, not a {kind:?}", info.kind);
        }
        let c = CString::new(if settings_json.trim().is_empty() { "{}" } else { settings_json })?;
        let ptr = unsafe { (info.create)(c.as_ptr()) };
        if ptr.is_null() {
            anyhow::bail!("plugin \"{id}\" failed to start");
        }
        Ok(Self { ptr, info, start: Instant::now() })
    }

    fn time_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Video sources: renders a new frame.
    pub fn render_source(&mut self) -> Option<VideoFrame> {
        let (size_fn, render) = (self.info.video_size?, self.info.video_render?);
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { size_fn(self.ptr, &mut w, &mut h) };
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            return None;
        }
        let mut data = vec![0u8; (w * h * 4) as usize];
        let mut f = FerrousFrame { width: w, height: h, stride: w * 4, data: data.as_mut_ptr() };
        unsafe { render(self.ptr, &mut f, self.time_ns()) };
        Some(VideoFrame::new(w, h, data))
    }

    /// Video filters: processes a copy of `frame`.
    pub fn filter_video(&mut self, frame: &VideoFrame) -> VideoFrame {
        let Some(render) = self.info.video_render else { return frame.clone() };
        let mut data = frame.data.as_ref().clone();
        let mut f = FerrousFrame { width: frame.width, height: frame.height, stride: frame.width * 4, data: data.as_mut_ptr() };
        unsafe { render(self.ptr, &mut f, self.time_ns()) };
        VideoFrame::new(frame.width, frame.height, data)
    }

    /// Audio filters: processes interleaved stereo in place.
    pub fn filter_audio(&mut self, samples: &mut [f32]) {
        if let Some(f) = self.info.audio_process {
            unsafe { f(self.ptr, samples.as_mut_ptr(), (samples.len() / 2) as u32) };
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { (self.info.destroy)(self.ptr) };
        // `info.lib` keeps the library loaded until every instance is gone.
        let _ = &self.info.lib;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the example plugin and drives all three of its plugins through the host.
    #[test]
    fn loads_and_runs_example_plugin() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "ferrous-example-plugin"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let target = std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or(root.join("target"));
        let name = if cfg!(windows) { "ferrous_example_plugin.dll" } else if cfg!(target_os = "macos") { "libferrous_example_plugin.dylib" } else { "libferrous_example_plugin.so" };
        let ids = load_file(&target.join("debug").join(name)).unwrap();
        assert_eq!(ids, vec!["example.gradient", "example.invert", "example.gain"]);

        let mut src = Instance::new("example.gradient", r#"{"width": 64, "height": 32}"#, Kind::VideoSource).unwrap();
        let f = src.render_source().unwrap();
        assert_eq!((f.width, f.height), (64, 32));
        assert_eq!(f.data[1], 64); // green channel constant

        let mut inv = Instance::new("example.invert", "", Kind::VideoFilter).unwrap();
        let g = inv.filter_video(&f);
        assert_eq!(g.data[1], 191);
        assert_eq!(g.data[3], 255);

        let mut gain = Instance::new("example.gain", r#"{"db": -6}"#, Kind::AudioFilter).unwrap();
        let mut a = vec![1.0f32; 960];
        gain.filter_audio(&mut a);
        assert!((a[0] - 0.501).abs() < 0.01);

        assert!(Instance::new("example.gain", "", Kind::VideoSource).is_err(), "kind mismatch is rejected");
        assert!(Instance::new("nope", "", Kind::VideoSource).is_err());
    }
}
