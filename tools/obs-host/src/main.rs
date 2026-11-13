//! `ferrous-obs-host`: runs one OBS plugin source on the real libobs.
//!
//! Usage: `ferrous-obs-host --obs-dir <OBS install> --media-port <port>`.
//! Commands arrive as JSON lines on stdin, replies go to stdout (see
//! `ferrous_obs::protocol`). Video (BGRA, the source's size) and audio (48 kHz
//! stereo float, interleaved) stream to Ferrous over a loopback socket.
//!
//! libobs is loaded at run time and its structs are declared here to match
//! libobs 30-32 (checked against the libobs 30 headers). Other major
//! versions are refused.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use ferrous_obs::protocol::*;
use libloading::Library;

const MAX_AV_PLANES: usize = 8;
const VIDEO_FORMAT_NV12: u32 = 2;
const VIDEO_CS_709: u32 = 2;
const VIDEO_RANGE_PARTIAL: u32 = 1;
const OBS_SCALE_BICUBIC: u32 = 2;
const SPEAKERS_STEREO: u32 = 2;
const AUDIO_FORMAT_FLOAT_PLANAR: u32 = 8;
const OBS_SOURCE_VIDEO: u32 = 1;
const OBS_SOURCE_AUDIO: u32 = 2;
const OBS_SOURCE_DEPRECATED: u32 = 1 << 8;
const OBS_SOURCE_CAP_OBSOLETE: u32 = 1 << 10;

#[repr(C)]
struct obs_video_info {
    graphics_module: *const c_char,
    fps_num: u32,
    fps_den: u32,
    base_width: u32,
    base_height: u32,
    output_width: u32,
    output_height: u32,
    output_format: u32,
    adapter: u32,
    gpu_conversion: bool,
    colorspace: u32,
    range: u32,
    scale_type: u32,
}

#[repr(C)]
struct obs_audio_info {
    samples_per_sec: u32,
    speakers: u32,
}



#[repr(C)]
struct audio_data {
    data: [*mut u8; MAX_AV_PLANES],
    frames: u32,
    timestamp: u64,
}

#[repr(C)]
struct audio_convert_info {
    samples_per_sec: u32,
    format: u32,
    speakers: u32,
    allow_clipping: bool,
}

type P = *mut c_void;
type DrawCb = unsafe extern "C" fn(P, u32, u32);

#[repr(C, align(16))]
struct Vec4([f32; 4]);

const GS_BGRA: u32 = 5;
const GS_ZS_NONE: u32 = 0;
const GS_CLEAR_COLOR: u32 = 1;
const GS_BLEND_ONE: u32 = 1;
const GS_BLEND_SRCALPHA: u32 = 4;
const GS_BLEND_INVSRCALPHA: u32 = 5;
type AudioCb = unsafe extern "C" fn(P, usize, *mut audio_data);

macro_rules! api {
    ($($name:ident : fn($($a:ty),*) $(-> $r:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Api { $($name: unsafe extern "C" fn($($a),*) $(-> $r)?,)* _lib: Library }
        impl Api {
            unsafe fn load(lib: Library) -> anyhow::Result<Api> {
                Ok(Api {
                    $($name: *lib.get::<unsafe extern "C" fn($($a),*) $(-> $r)?>(concat!(stringify!($name), "\0").as_bytes()).with_context(|| format!("libobs has no {}", stringify!($name)))?,)*
                    _lib: lib,
                })
            }
        }
    };
}

api! {
    obs_startup: fn(*const c_char, *const c_char, P) -> bool;
    obs_shutdown: fn();
    obs_get_version_string: fn() -> *const c_char;
    obs_reset_video: fn(*mut obs_video_info) -> i32;
    obs_reset_audio: fn(*const obs_audio_info) -> bool;
    obs_add_data_path: fn(*const c_char);
    obs_open_module: fn(*mut P, *const c_char, *const c_char) -> i32;
    obs_init_module: fn(P) -> bool;
    obs_post_load_modules: fn();
    obs_enum_input_types: fn(usize, *mut *const c_char) -> bool;
    obs_enum_filter_types: fn(usize, *mut *const c_char) -> bool;
    obs_source_get_display_name: fn(*const c_char) -> *const c_char;
    obs_get_source_output_flags: fn(*const c_char) -> u32;
    obs_source_create: fn(*const c_char, *const c_char, P, P) -> P;
    obs_source_release: fn(P);
    obs_source_get_width: fn(P) -> u32;
    obs_source_get_height: fn(P) -> u32;
    obs_source_update: fn(P, P);
    obs_source_get_settings: fn(P) -> P;
    obs_source_properties: fn(P) -> P;
    obs_set_output_source: fn(u32, P);
    obs_add_main_render_callback: fn(DrawCb, P);
    obs_enter_graphics: fn();
    obs_leave_graphics: fn();
    obs_remove_main_render_callback: fn(DrawCb, P);
    obs_source_video_render: fn(P);
    gs_texrender_create: fn(u32, u32) -> P;
    gs_texrender_reset: fn(P);
    gs_texrender_begin: fn(P, u32, u32) -> bool;
    gs_texrender_end: fn(P);
    gs_texrender_get_texture: fn(P) -> P;
    gs_stagesurface_create: fn(u32, u32, u32) -> P;
    gs_stagesurface_destroy: fn(P);
    gs_stage_texture: fn(P, P);
    gs_stagesurface_map: fn(P, *mut *mut u8, *mut u32) -> bool;
    gs_stagesurface_unmap: fn(P);
    gs_clear: fn(u32, *const Vec4, f32, u8);
    gs_ortho: fn(f32, f32, f32, f32, f32, f32);
    gs_blend_state_push: fn();
    gs_blend_state_pop: fn();
    gs_blend_function_separate: fn(u32, u32, u32, u32);
    obs_add_raw_audio_callback: fn(usize, *const audio_convert_info, AudioCb, P);
    obs_data_create_from_json: fn(*const c_char) -> P;
    obs_data_get_json: fn(P) -> *const c_char;
    obs_data_release: fn(P);
    obs_properties_destroy: fn(P);
    obs_properties_first: fn(P) -> P;
    obs_property_next: fn(*mut P) -> bool;
    obs_property_name: fn(P) -> *const c_char;
    obs_property_description: fn(P) -> *const c_char;
    obs_property_get_type: fn(P) -> i32;
    obs_property_visible: fn(P) -> bool;
    obs_property_enabled: fn(P) -> bool;
    obs_property_int_min: fn(P) -> i32;
    obs_property_int_max: fn(P) -> i32;
    obs_property_int_step: fn(P) -> i32;
    obs_property_int_suffix: fn(P) -> *const c_char;
    obs_property_float_min: fn(P) -> f64;
    obs_property_float_max: fn(P) -> f64;
    obs_property_float_step: fn(P) -> f64;
    obs_property_float_suffix: fn(P) -> *const c_char;
    obs_property_text_type: fn(P) -> i32;
    obs_property_path_type: fn(P) -> i32;
    obs_property_path_filter: fn(P) -> *const c_char;
    obs_property_list_format: fn(P) -> i32;
    obs_property_list_item_count: fn(P) -> usize;
    obs_property_list_item_name: fn(P, usize) -> *const c_char;
    obs_property_list_item_string: fn(P, usize) -> *const c_char;
    obs_property_list_item_int: fn(P, usize) -> i64;
    obs_property_list_item_float: fn(P, usize) -> f64;
    obs_property_group_content: fn(P) -> P;
    obs_property_button_clicked: fn(P, P) -> bool;
}

fn s(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

fn c(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Video and audio go to the writer thread through bounded channels; a slow
/// reader drops frames rather than stalling libobs.
struct Media {
    tx: SyncSender<(MediaHeader, Vec<u8>)>,
    /// Only forward the mix when the source actually has audio.
    audio: std::sync::atomic::AtomicBool,
    api: std::sync::atomic::AtomicPtr<Api>,
    source: std::sync::atomic::AtomicPtr<c_void>,
    gfx: std::sync::Mutex<Gfx>,
}

/// Graphics-thread state for reading the source back with its alpha.
struct Gfx {
    texrender: P,
    stage: P,
    stage_size: (u32, u32),
    staged: bool,
}

unsafe impl Send for Gfx {}

/// Runs on libobs's graphics thread every frame: renders the source alone
/// onto a transparent texture (so its alpha survives, unlike the canvas
/// output), and reads back the previous frame's copy (one frame behind, so
/// the GPU never stalls).
unsafe extern "C" fn on_draw(param: P, _cx: u32, _cy: u32) {
    use std::sync::atomic::Ordering::Acquire;
    let m = &*(param as *const Media);
    let (api, src) = (m.api.load(Acquire), m.source.load(Acquire));
    if api.is_null() || src.is_null() {
        return;
    }
    let a = &*api;
    let (w, h) = ((a.obs_source_get_width)(src), (a.obs_source_get_height)(src));
    if w == 0 || h == 0 {
        return;
    }
    let mut g = m.gfx.lock().unwrap();
    if g.texrender.is_null() {
        g.texrender = (a.gs_texrender_create)(GS_BGRA, GS_ZS_NONE);
    }
    (a.gs_texrender_reset)(g.texrender);
    if !(a.gs_texrender_begin)(g.texrender, w, h) {
        return;
    }
    let zero = Vec4([0.0; 4]);
    (a.gs_clear)(GS_CLEAR_COLOR, &zero, 0.0, 0);
    (a.gs_ortho)(0.0, w as f32, 0.0, h as f32, -100.0, 100.0);
    (a.gs_blend_state_push)();
    (a.gs_blend_function_separate)(GS_BLEND_SRCALPHA, GS_BLEND_INVSRCALPHA, GS_BLEND_ONE, GS_BLEND_INVSRCALPHA);
    (a.obs_source_video_render)(src);
    (a.gs_blend_state_pop)();
    (a.gs_texrender_end)(g.texrender);

    if g.stage_size != (w, h) {
        if !g.stage.is_null() {
            (a.gs_stagesurface_destroy)(g.stage);
        }
        g.stage = (a.gs_stagesurface_create)(w, h, GS_BGRA);
        g.stage_size = (w, h);
        g.staged = false;
    }
    if g.stage.is_null() {
        return;
    }
    if g.staged {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut linesize = 0u32;
        if (a.gs_stagesurface_map)(g.stage, &mut data, &mut linesize) && !data.is_null() {
            let row = (w * 4) as usize;
            let mut buf = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                buf.extend_from_slice(std::slice::from_raw_parts(data.add(y * linesize as usize), row));
            }
            (a.gs_stagesurface_unmap)(g.stage);
            // Blending left the colour premultiplied; Ferrous composites straight alpha.
            for px in buf.chunks_exact_mut(4) {
                let al = px[3] as u32;
                if al > 0 && al < 255 {
                    for c in &mut px[..3] {
                        *c = ((*c as u32 * 255 + al / 2) / al).min(255) as u8;
                    }
                }
            }
            let _ = m.tx.try_send((MediaHeader { kind: KIND_VIDEO, width: w, height: h, timestamp: 0, len: buf.len() as u32 }, buf));
        }
    }
    (a.gs_stage_texture)(g.stage, (a.gs_texrender_get_texture)(g.texrender));
    g.staged = true;
}

unsafe extern "C" fn on_audio(param: P, _mix: usize, data: *mut audio_data) {
    let m = &*(param as *const Media);
    if data.is_null() || !m.audio.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let d = &*data;
    let n = d.frames as usize;
    if d.data[0].is_null() || d.data[1].is_null() {
        return;
    }
    let l = std::slice::from_raw_parts(d.data[0] as *const f32, n);
    let r = std::slice::from_raw_parts(d.data[1] as *const f32, n);
    let mut buf = Vec::with_capacity(n * 8);
    for i in 0..n {
        buf.extend_from_slice(&l[i].to_le_bytes());
        buf.extend_from_slice(&r[i].to_le_bytes());
    }
    let _ = m.tx.try_send((MediaHeader { kind: KIND_AUDIO, width: 0, height: 0, timestamp: d.timestamp, len: buf.len() as u32 }, buf));
}

fn reply(r: &Reply) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", serde_json::to_string(r).unwrap_or_default());
    let _ = out.flush();
}

struct Host {
    api: Box<Api>,
    source: P,
    media: Box<Media>,
    graphics: CString,
}

impl Host {
    /// `probe_only`: load just this module (to see if it survives loading
    /// without OBS's UI) and stop.
    unsafe fn start(obs_dir: &Path, media: Box<Media>, probe_only: Option<&Path>) -> anyhow::Result<Host> {
        #[cfg(windows)]
        let (lib_path, graphics, modules, data, libobs_data) = {
            let bin = obs_dir.join("bin").join("64bit");
            // obs.dll's dependencies live next to it.
            std::env::set_current_dir(&bin).ok();
            (bin.join("obs.dll"), "libobs-d3d11", obs_dir.join("obs-plugins").join("64bit"), obs_dir.join("data").join("obs-plugins").join("%module%"), obs_dir.join("data").join("libobs"))
        };
        #[cfg(not(windows))]
        let (lib_path, graphics, modules, data, libobs_data) = {
            let lib = ["lib/x86_64-linux-gnu/libobs.so.0", "lib/libobs.so.0", "lib64/libobs.so.0"].iter().map(|p| obs_dir.join(p)).find(|p| p.exists()).unwrap_or_else(|| "libobs.so.0".into());
            let libdir = lib.parent().map(Path::to_path_buf).unwrap_or_default();
            (lib, "libobs-opengl", libdir.join("obs-plugins"), obs_dir.join("share").join("obs").join("obs-plugins").join("%module%"), obs_dir.join("share").join("obs").join("libobs"))
        };
        let lib = Library::new(&lib_path).with_context(|| format!("loading {}", lib_path.display()))?;
        #[cfg(not(windows))]
        nix_platform(&lib)?;
        let api = Api::load(lib)?;
        let version = s((api.obs_get_version_string)());
        let major: u32 = version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0);
        if !(30..=32).contains(&major) {
            bail!("libobs {version} isn't supported (OBS 30 to 32 are)");
        }
        let cfg = std::env::temp_dir().join("ferrous-obs-host-config");
        let _ = std::fs::create_dir_all(&cfg);
        if !(api.obs_startup)(c("en-US").as_ptr(), c(&cfg.to_string_lossy()).as_ptr(), std::ptr::null_mut()) {
            bail!("obs_startup failed");
        }
        (api.obs_add_data_path)(c(&format!("{}/", libobs_data.display())).as_ptr());
        let ai = obs_audio_info { samples_per_sec: 48_000, speakers: SPEAKERS_STEREO };
        if !(api.obs_reset_audio)(&ai) {
            bail!("obs_reset_audio failed");
        }
        let mut host = Host { api: Box::new(api), source: std::ptr::null_mut(), media, graphics: c(graphics) };
        // The canvas only drives libobs's render loop; sources are read back on their own.
        host.reset_video(64, 64)?;
        host.media.api.store(&*host.api as *const Api as *mut Api, std::sync::atomic::Ordering::Release);
        (host.api.obs_add_main_render_callback)(on_draw, host.media_ptr());
        let _ = &data;
        host.load_modules(&modules, &data, probe_only)?;
        // Post-load is where UI plugins build their docks and menus, so it's part of the probe.
        (host.api.obs_post_load_modules)();
        if probe_only.is_some() {
            return Ok(host);
        }
        let conv = audio_convert_info { samples_per_sec: 48_000, format: AUDIO_FORMAT_FLOAT_PLANAR, speakers: SPEAKERS_STEREO, allow_clipping: false };
        (host.api.obs_add_raw_audio_callback)(0, &conv, on_audio, host.media_ptr());
        reply(&Reply::Ready { version });
        Ok(host)
    }

    /// Loads plugin modules one by one. Plugins built around OBS's Qt UI
    /// (docks, menus) abort when there's no main window, so each module is
    /// first tried in a throwaway child process; ones that crash are
    /// remembered and skipped.
    unsafe fn load_modules(&mut self, dir: &Path, data_tmpl: &Path, probe_only: Option<&Path>) -> anyhow::Result<()> {
        let ext = if cfg!(windows) { "dll" } else { "so" };
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == ext)).collect())
            .unwrap_or_default();
        files.sort();
        if let Some(only) = probe_only {
            files = vec![only.to_path_buf()];
        }
        let cache_path = std::env::temp_dir().join("ferrous-obs-host-config").join("modules.json");
        let mut cache: std::collections::BTreeMap<String, (u64, bool)> =
            std::fs::read(&cache_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let exe = std::env::current_exe().ok();
        let mut changed = false;
        for f in files {
            let key = f.to_string_lossy().into_owned();
            let mtime = std::fs::metadata(&f).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            if probe_only.is_none() {
                let ok = match cache.get(&key) {
                    Some((t, ok)) if *t == mtime => *ok,
                    _ => {
                        // Plugins that attach to OBS's main window are UI plugins
                        // (docks, menus); without OBS's window they abort.
                        let ui_plugin = std::fs::read(&f).map(|b| contains(&b, b"obs_frontend_get_main_window")).unwrap_or(false);
                        let ok = !ui_plugin && exe.as_ref().is_some_and(|exe| probe(exe, &f));
                        cache.insert(key.clone(), (mtime, ok));
                        changed = true;
                        ok
                    }
                };
                if !ok {
                    eprintln!("ferrous-obs-host: skipping {} (needs OBS's UI)", f.display());
                    continue;
                }
            }
            let name = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let data = data_tmpl.to_string_lossy().replace("%module%", &name);
            let mut module: P = std::ptr::null_mut();
            if (self.api.obs_open_module)(&mut module, c(&f.to_string_lossy()).as_ptr(), c(&data).as_ptr()) == 0 && !module.is_null() {
                (self.api.obs_init_module)(module);
            }
        }
        if changed {
            let _ = std::fs::write(&cache_path, serde_json::to_vec(&cache).unwrap_or_default());
        }
        Ok(())
    }

    fn media_ptr(&self) -> P {
        &*self.media as *const Media as P
    }

    unsafe fn reset_video(&mut self, w: u32, h: u32) -> anyhow::Result<()> {
        let mut ovi = obs_video_info {
            graphics_module: self.graphics.as_ptr(),
            fps_num: 30,
            fps_den: 1,
            base_width: w,
            base_height: h,
            output_width: w,
            output_height: h,
            output_format: VIDEO_FORMAT_NV12,
            adapter: 0,
            gpu_conversion: true,
            colorspace: VIDEO_CS_709,
            range: VIDEO_RANGE_PARTIAL,
            scale_type: OBS_SCALE_BICUBIC,
        };
        let r = (self.api.obs_reset_video)(&mut ovi);
        if r != 0 {
            bail!("obs_reset_video failed ({r})");
        }
        Ok(())
    }

    unsafe fn types(&self, filters: bool) -> Vec<TypeInfo> {
        let mut v = Vec::new();
        let mut i = 0;
        let mut id: *const c_char = std::ptr::null();
        loop {
            let more = if filters { (self.api.obs_enum_filter_types)(i, &mut id) } else { (self.api.obs_enum_input_types)(i, &mut id) };
            if !more {
                break;
            }
            let flags = (self.api.obs_get_source_output_flags)(id);
            i += 1;
            // Old versions of a type that OBS hides from its own menus.
            if flags & (OBS_SOURCE_DEPRECATED | OBS_SOURCE_CAP_OBSOLETE) != 0 {
                continue;
            }
            let name = s((self.api.obs_source_get_display_name)(id));
            v.push(TypeInfo { id: s(id), name: if name.is_empty() { s(id) } else { name }, video: flags & OBS_SOURCE_VIDEO != 0, audio: flags & OBS_SOURCE_AUDIO != 0 });
        }
        v
    }

    unsafe fn create(&mut self, id: &str, settings: &serde_json::Value) -> anyhow::Result<(u32, u32)> {
        if !self.source.is_null() {
            bail!("this host already runs a source");
        }
        let data = (self.api.obs_data_create_from_json)(c(&settings.to_string()).as_ptr());
        let src = (self.api.obs_source_create)(c(id).as_ptr(), c("Ferrous").as_ptr(), data, std::ptr::null_mut());
        (self.api.obs_data_release)(data);
        if src.is_null() {
            bail!("OBS couldn't create a \"{id}\" source (is its plugin installed?)");
        }
        self.source = src;
        self.media.source.store(src, std::sync::atomic::Ordering::Release);
        let flags = (self.api.obs_get_source_output_flags)(c(id).as_ptr());
        self.media.audio.store(flags & OBS_SOURCE_AUDIO != 0, std::sync::atomic::Ordering::Relaxed);
        (self.api.obs_set_output_source)(0, src);
        // Async sources (media, capture) only know their size after the first frame.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (w, h) = self.size();
            if w > 0 && h > 0 {
                return Ok((w, h));
            }
            if Instant::now() > deadline {
                return Ok((0, 0));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    unsafe fn size(&self) -> (u32, u32) {
        if self.source.is_null() {
            return (0, 0);
        }
        ((self.api.obs_source_get_width)(self.source) & !1, (self.api.obs_source_get_height)(self.source) & !1)
    }

    unsafe fn properties(&self) -> anyhow::Result<(Vec<Property>, serde_json::Value)> {
        if self.source.is_null() {
            bail!("no source yet");
        }
        let props = (self.api.obs_source_properties)(self.source);
        let list = if props.is_null() { Vec::new() } else { self.walk(props) };
        if !props.is_null() {
            (self.api.obs_properties_destroy)(props);
        }
        Ok((list, self.settings()))
    }

    /// Current settings with the type's defaults filled in.
    unsafe fn settings(&self) -> serde_json::Value {
        let json = |d: P| -> serde_json::Value {
            if d.is_null() {
                return serde_json::json!({});
            }
            let v = serde_json::from_str(&s((self.api.obs_data_get_json)(d))).unwrap_or(serde_json::json!({}));
            (self.api.obs_data_release)(d);
            v
        };
        let mut out = serde_json::Map::new();
        // The type id isn't kept; defaults come from the source's own settings object.
        let cur = json((self.api.obs_source_get_settings)(self.source));
        if let serde_json::Value::Object(m) = cur {
            out.extend(m);
        }
        serde_json::Value::Object(out)
    }

    unsafe fn walk(&self, props: P) -> Vec<Property> {
        let a = &self.api;
        let mut out = Vec::new();
        let mut p = (a.obs_properties_first)(props);
        while !p.is_null() {
            let kind = match (a.obs_property_get_type)(p) {
                1 => PropertyKind::Bool,
                2 => PropertyKind::Int { min: (a.obs_property_int_min)(p) as i64, max: (a.obs_property_int_max)(p) as i64, step: (a.obs_property_int_step)(p).max(1) as i64, suffix: s((a.obs_property_int_suffix)(p)) },
                3 => PropertyKind::Float { min: (a.obs_property_float_min)(p), max: (a.obs_property_float_max)(p), step: (a.obs_property_float_step)(p), suffix: s((a.obs_property_float_suffix)(p)) },
                4 => {
                    let t = (a.obs_property_text_type)(p);
                    PropertyKind::Text { password: t == 1, multiline: t == 2, info: t == 3 }
                }
                5 => {
                    let t = (a.obs_property_path_type)(p);
                    PropertyKind::Path { filter: s((a.obs_property_path_filter)(p)), save: t == 1, directory: t == 2 }
                }
                6 => {
                    let fmt = (a.obs_property_list_format)(p);
                    let items = (0..(a.obs_property_list_item_count)(p))
                        .map(|i| {
                            let v = match fmt {
                                1 => serde_json::json!((a.obs_property_list_item_int)(p, i)),
                                2 => serde_json::json!((a.obs_property_list_item_float)(p, i)),
                                3 => serde_json::json!(s((a.obs_property_list_item_string)(p, i))),
                                _ => serde_json::Value::Null,
                            };
                            (s((a.obs_property_list_item_name)(p, i)), v)
                        })
                        .collect();
                    PropertyKind::List { items }
                }
                7 => PropertyKind::Color { alpha: false },
                13 => PropertyKind::Color { alpha: true },
                8 => PropertyKind::Button,
                9 => PropertyKind::Font,
                12 => {
                    let content = (a.obs_property_group_content)(p);
                    PropertyKind::Group { properties: if content.is_null() { Vec::new() } else { self.walk(content) } }
                }
                t => PropertyKind::Other { obs_type: t },
            };
            out.push(Property { name: s((a.obs_property_name)(p)), description: s((a.obs_property_description)(p)), visible: (a.obs_property_visible)(p), enabled: (a.obs_property_enabled)(p), kind });
            if !(a.obs_property_next)(&mut p) {
                break;
            }
        }
        out
    }

    unsafe fn update(&mut self, settings: &serde_json::Value) {
        let data = (self.api.obs_data_create_from_json)(c(&settings.to_string()).as_ptr());
        (self.api.obs_source_update)(self.source, data);
        (self.api.obs_data_release)(data);
    }

    unsafe fn click(&mut self, name: &str) -> anyhow::Result<()> {
        let props = (self.api.obs_source_properties)(self.source);
        if props.is_null() {
            bail!("no properties");
        }
        let mut p = (self.api.obs_properties_first)(props);
        let mut found = false;
        while !p.is_null() {
            if s((self.api.obs_property_name)(p)) == name {
                (self.api.obs_property_button_clicked)(p, self.source);
                found = true;
                break;
            }
            if !(self.api.obs_property_next)(&mut p) {
                break;
            }
        }
        (self.api.obs_properties_destroy)(props);
        if found { Ok(()) } else { Err(anyhow!("no button {name}")) }
    }

    unsafe fn shutdown(self) {
        (self.api.obs_remove_main_render_callback)(on_draw, self.media_ptr());
        self.media.source.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
        {
            (self.api.obs_enter_graphics)();
            let mut g = self.media.gfx.lock().unwrap();
            if !g.stage.is_null() {
                (self.api.gs_stagesurface_destroy)(g.stage);
                g.stage = std::ptr::null_mut();
            }
            g.texrender = std::ptr::null_mut(); // freed with the graphics context
            (self.api.obs_leave_graphics)();
        }
        (self.api.obs_set_output_source)(0, std::ptr::null_mut());
        if !self.source.is_null() {
            (self.api.obs_source_release)(self.source);
        }
        (self.api.obs_shutdown)();
    }
}

/// Linux: libobs's OpenGL renderer needs an X11 EGL display.
#[cfg(not(windows))]
unsafe fn nix_platform(lib: &Library) -> anyhow::Result<()> {
    let set_platform = lib.get::<unsafe extern "C" fn(i32)>(b"obs_set_nix_platform\0")?;
    let set_display = lib.get::<unsafe extern "C" fn(P)>(b"obs_set_nix_platform_display\0")?;
    let x11 = Library::new("libX11.so.6").context("libX11")?;
    let open = x11.get::<unsafe extern "C" fn(*const c_char) -> P>(b"XOpenDisplay\0")?;
    let display = open(std::ptr::null());
    if display.is_null() {
        bail!("no X display (set DISPLAY)");
    }
    set_platform(1); // OBS_NIX_PLATFORM_X11_EGL
    set_display(display);
    std::mem::forget(x11);
    Ok(())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Runs `ferrous-obs-host --probe-module <file>`: true if loading it doesn't crash.
fn probe(exe: &Path, module: &Path) -> bool {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--probe-module").arg(module);
    if let Some(d) = std::env::args().skip_while(|a| a != "--obs-dir").nth(1) {
        cmd.arg("--obs-dir").arg(d);
    }
    let Ok(mut child) = cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn() else { return false };
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = out.as_mut() {
            let _ = std::io::Read::read_to_string(o, &mut s);
        }
        s.contains("probe-ok")
    });
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return reader.join().unwrap_or(false),
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
}

fn writer(port: u16, rx: Receiver<(MediaHeader, Vec<u8>)>) {
    let Ok(mut sock) = TcpStream::connect(("127.0.0.1", port)) else { return };
    let _ = sock.set_nodelay(true);
    while let Ok((h, buf)) = rx.recv() {
        if sock.write_all(&h.to_bytes()).is_err() || sock.write_all(&buf).is_err() {
            return;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let obs_dir = PathBuf::from(arg("--obs-dir").unwrap_or_else(|| if cfg!(windows) { "C:\\Program Files\\obs-studio".into() } else { "/usr".into() }));
    let port: Option<u16> = arg("--media-port").and_then(|p| p.parse().ok());

    let probe_module = arg("--probe-module").map(PathBuf::from);
    let (tx, rx) = sync_channel(4);
    if let Some(port) = port {
        std::thread::spawn(move || writer(port, rx));
    }
    let media = Box::new(Media {
        tx,
        audio: Default::default(),
        api: Default::default(),
        source: Default::default(),
        gfx: std::sync::Mutex::new(Gfx { texrender: std::ptr::null_mut(), stage: std::ptr::null_mut(), stage_size: (0, 0), staged: false }),
    });
    if let Some(m) = &probe_module {
        // Loading succeeded if we get here; skip libobs teardown (some UI
        // plugins crash on unload) and leave quickly.
        match unsafe { Host::start(&obs_dir, media, Some(m)) } {
            Ok(_) => {
                // Teardown isn't part of the test (some plugins crash on exit).
                println!("probe-ok");
                let _ = std::io::stdout().flush();
                std::process::exit(0)
            }
            Err(_) => std::process::exit(1),
        }
    }
    let mut host = match unsafe { Host::start(&obs_dir, media, None) } {
        Ok(h) => h,
        Err(e) => {
            reply(&Reply::Error { message: format!("{e:#}") });
            std::process::exit(1);
        }
    };

    // Commands on their own thread, so the main loop can also follow size changes.
    let (ctx, crx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(l) = line else { break };
            if ctx.send(l).is_err() {
                break;
            }
        }
    });
    loop {
        let line = match crx.recv_timeout(Duration::from_millis(500)) {
            Ok(l) => l,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break, // Ferrous went away
        };
        let cmd: Command_ = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                reply(&Reply::Error { message: format!("bad command: {e}") });
                continue;
            }
        };
        let r = unsafe {
            match cmd {
                Command_::List => Reply::Types { inputs: host.types(false), filters: host.types(true) },
                Command_::Create { id, settings } => match host.create(&id, &settings) {
                    Ok((width, height)) => Reply::Created { width, height },
                    Err(e) => Reply::Error { message: format!("{e:#}") },
                },
                Command_::Properties => match host.properties() {
                    Ok((properties, settings)) => Reply::Properties { properties, settings },
                    Err(e) => Reply::Error { message: format!("{e:#}") },
                },
                Command_::Update { settings } => {
                    host.update(&settings);
                    Reply::Ok
                }
                Command_::Click { property } => match host.click(&property) {
                    Ok(()) => Reply::Ok,
                    Err(e) => Reply::Error { message: format!("{e:#}") },
                },
                Command_::Quit => break,
            }
        };
        reply(&r);
    }
    unsafe { host.shutdown() };
}
