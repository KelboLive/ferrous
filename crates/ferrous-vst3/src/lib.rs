//! VST3 plugin hosting for audio filters.
//!
//! Loads a `.vst3` bundle, creates an effect's component, processor and edit
//! controller, wires them together, and runs the processor on Ferrous's 48 kHz
//! stereo mixer blocks. Parameters can be read and changed from Ferrous's UI,
//! the whole plugin state round-trips through a string saved with the filter,
//! and on Windows the plugin's own editor opens in a window of its own.
//!
//! Hosting follows the VST 3 SDK's documented sequence (initialise, connect
//! component and controller, transfer state, set up processing, activate
//! buses, set active, set processing). The VST 3 SDK and the `vst3` binding
//! crate are MIT-licensed.

// Casts that look redundant here aren't on Windows (the SDK's integer
// types differ by platform).
#![allow(non_upper_case_globals, non_snake_case, clippy::missing_safety_doc, clippy::unnecessary_cast)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use parking_lot::Mutex;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};

#[cfg(windows)]
mod editor;

// ------------------------------------------------------------- discovery

/// Where VST3 plugins are installed (the standard locations).
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("CommonProgramFiles") {
            v.push(PathBuf::from(p).join("VST3"));
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(p).join("Programs").join("Common").join("VST3"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join(".vst3"));
        }
        v.push("/usr/lib/vst3".into());
        v.push("/usr/local/lib/vst3".into());
    }
    v
}

/// `.vst3` bundles (or single-file plugins) under `dirs`, two levels deep
/// (vendors often add a folder of their own).
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3")) {
                out.push(p);
            } else if p.is_dir() && depth > 0 {
                walk(&p, depth - 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(d, 2, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// The loadable binary inside a bundle (or the path itself for the legacy
/// single-file layout).
pub fn module_binary(bundle: &Path) -> Option<PathBuf> {
    if bundle.is_file() {
        return Some(bundle.to_path_buf());
    }
    let stem = bundle.file_stem()?.to_string_lossy().into_owned();
    let (arch, ext) = if cfg!(windows) {
        (if cfg!(target_arch = "aarch64") { "arm64-win" } else { "x86_64-win" }, "vst3")
    } else {
        (if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "so")
    };
    let p = bundle.join("Contents").join(arch).join(format!("{stem}.{ext}"));
    p.is_file().then_some(p)
}

/// An audio effect a bundle provides.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassInfo {
    pub path: PathBuf,
    /// Class id, 32 hex digits.
    pub cid: String,
    pub name: String,
    pub vendor: String,
    /// e.g. "Fx|Dynamics" (empty if the plugin doesn't say).
    pub category: String,
}

// ---------------------------------------------------------------- module

/// A loaded plugin binary. Kept alive (via `Arc`) by every instance made from it.
pub struct Module {
    factory_ptr: Option<ComPtr<IPluginFactory>>,
    // Dropped after the factory: the library must outlive every COM pointer.
    lib: Option<libloading::Library>,
    path: PathBuf,
}

// The factory is only used under the host's own locking.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    pub fn load(bundle: &Path) -> anyhow::Result<Arc<Module>> {
        let bin = module_binary(bundle).ok_or_else(|| anyhow!("{} has no binary for this platform", bundle.display()))?;
        unsafe {
            let lib = libloading::Library::new(&bin).with_context(|| format!("loading {}", bin.display()))?;
            #[cfg(windows)]
            if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0") {
                if !init() {
                    bail!("InitDll failed");
                }
            }
            #[cfg(unix)]
            let lib = {
                let raw = libloading::os::unix::Library::from(lib).into_raw();
                let lib: libloading::Library = libloading::os::unix::Library::from_raw(raw).into();
                if let Ok(entry) = lib.get::<unsafe extern "system" fn(*mut c_void) -> bool>(b"ModuleEntry\0") {
                    if !entry(raw) {
                        bail!("ModuleEntry failed");
                    }
                }
                lib
            };
            let get = lib
                .get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0")
                .context("not a VST3 plugin (no GetPluginFactory)")?;
            let factory = ComPtr::from_raw(get()).ok_or_else(|| anyhow!("GetPluginFactory returned null"))?;
            Ok(Arc::new(Module { factory_ptr: Some(factory), lib: Some(lib), path: bundle.to_path_buf() }))
        }
    }

    fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory_ptr.as_ref().expect("factory lives until drop")
    }

    /// Audio effects in the module.
    pub fn classes(&self) -> Vec<ClassInfo> {
        let mut out = Vec::new();
        unsafe {
            let mut finfo: PFactoryInfo = std::mem::zeroed();
            let vendor = if self.factory().getFactoryInfo(&mut finfo) == kResultOk { cstr(&finfo.vendor) } else { String::new() };
            let f2 = self.factory().cast::<IPluginFactory2>();
            for i in 0..self.factory().countClasses() {
                let mut info: PClassInfo = std::mem::zeroed();
                if self.factory().getClassInfo(i, &mut info) != kResultOk {
                    continue;
                }
                if cstr(&info.category) != "Audio Module Class" {
                    continue;
                }
                let (mut category, mut v) = (String::new(), vendor.clone());
                if let Some(f2) = &f2 {
                    let mut i2: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut i2) == kResultOk {
                        category = cstr(&i2.subCategories);
                        let cv = cstr(&i2.vendor);
                        if !cv.is_empty() {
                            v = cv;
                        }
                    }
                }
                // Instruments need MIDI and have no audio input: not filters.
                if category.split('|').any(|c| c == "Instrument") && !category.split('|').any(|c| c == "Fx") {
                    continue;
                }
                out.push(ClassInfo { path: self.path.clone(), cid: tuid_hex(&info.cid), name: cstr(&info.name), vendor: v, category });
            }
        }
        out
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        let Some(lib) = self.lib.take() else { return };
        // Release the factory before the module's exit hook runs.
        self.factory_ptr.take();
        unsafe {
            #[cfg(windows)]
            if let Ok(exit) = lib.get::<unsafe extern "system" fn() -> bool>(b"ExitDll\0") {
                exit();
            }
            #[cfg(unix)]
            if let Ok(exit) = lib.get::<unsafe extern "system" fn() -> bool>(b"ModuleExit\0") {
                exit();
            }
        }
        drop(lib);
    }
}

/// Lists a bundle's effects (loads it in this process; the app does this in
/// a helper process so a crashing plugin can't take Ferrous down).
pub fn scan(bundle: &Path) -> anyhow::Result<Vec<ClassInfo>> {
    let m = Module::load(bundle)?;
    let classes = m.classes();
    // Leak-free unload, but some plugins misbehave on unload; scanning is short-lived anyway.
    drop(m);
    Ok(classes)
}

fn cstr(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn wstr(b: &[TChar]) -> String {
    let n = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n].iter().map(|c| *c as u16).collect::<Vec<_>>())
}

pub fn tuid_hex(t: &TUID) -> String {
    t.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

pub fn hex_tuid(s: &str) -> Option<TUID> {
    if s.len() != 32 {
        return None;
    }
    let mut t: TUID = [0; 16];
    for i in 0..16 {
        t[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()? as c_char;
    }
    Some(t)
}

fn guid_eq(t: *const TUID, g: &vst3::com_scrape_types::Guid) -> bool {
    !t.is_null() && unsafe { (*t).iter().zip(g.iter()).all(|(a, b)| *a as u8 == *b) }
}

// ---------------------------------------------------- host-side COM objects

/// The host context passed to `initialize`: names the host and creates
/// messages/attribute lists for plugins whose halves talk to each other.
struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let s: Vec<u16> = "Ferrous".encode_utf16().collect();
        let n = &mut *name;
        n.iter_mut().for_each(|c| *c = 0);
        for (d, c) in n.iter_mut().zip(s) {
            *d = c as TChar;
        }
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        let unk: Option<ComPtr<FUnknown>> = if guid_eq(cid, &IMessage::IID) {
            ComWrapper::new(Message::default()).to_com_ptr::<FUnknown>()
        } else if guid_eq(cid, &IAttributeList::IID) {
            ComWrapper::new(AttrList::default()).to_com_ptr::<FUnknown>()
        } else {
            None
        };
        match unk {
            Some(u) => {
                let p = u.as_ptr();
                ((*(*p).vtbl).queryInterface)(p, iid as *const TUID, obj)
            }
            None => {
                *obj = std::ptr::null_mut();
                kNoInterface
            }
        }
    }
}

#[derive(Default)]
struct Message {
    id: Mutex<Option<CString>>,
    attrs: Mutex<Option<ComPtr<IAttributeList>>>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.lock().as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null())
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        *self.id.lock() = (!id.is_null()).then(|| CStr::from_ptr(id).to_owned());
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        let mut a = self.attrs.lock();
        if a.is_none() {
            *a = ComWrapper::new(AttrList::default()).to_com_ptr::<IAttributeList>();
        }
        a.as_ref().map(|p| p.as_ptr()).unwrap_or(std::ptr::null_mut())
    }
}

enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<TChar>),
    Bin(Vec<u8>),
}

#[derive(Default)]
struct AttrList(Mutex<HashMap<String, Attr>>);

impl Class for AttrList {
    type Interfaces = (IAttributeList,);
}

unsafe fn key(id: IAttributeList_::AttrID) -> Option<String> {
    (!id.is_null()).then(|| CStr::from_ptr(id).to_string_lossy().into_owned())
}

impl IAttributeListTrait for AttrList {
    unsafe fn setInt(&self, id: IAttributeList_::AttrID, value: int64) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        self.0.lock().insert(k, Attr::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: IAttributeList_::AttrID, value: *mut int64) -> tresult {
        match key(id).and_then(|k| match self.0.lock().get(&k) { Some(Attr::Int(v)) => Some(*v), _ => None }) {
            Some(v) => {
                *value = v;
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: IAttributeList_::AttrID, value: f64) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        self.0.lock().insert(k, Attr::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: IAttributeList_::AttrID, value: *mut f64) -> tresult {
        match key(id).and_then(|k| match self.0.lock().get(&k) { Some(Attr::Float(v)) => Some(*v), _ => None }) {
            Some(v) => {
                *value = v;
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: IAttributeList_::AttrID, string: *const TChar) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let mut v = Vec::new();
        if !string.is_null() {
            let mut i = 0;
            while *string.add(i) != 0 {
                v.push(*string.add(i));
                i += 1;
            }
        }
        self.0.lock().insert(k, Attr::Str(v));
        kResultOk
    }
    unsafe fn getString(&self, id: IAttributeList_::AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let g = self.0.lock();
        let Some(Attr::Str(v)) = key(id).and_then(|k| g.get(&k)) else { return kResultFalse };
        let cap = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if cap == 0 {
            return kResultFalse;
        }
        let n = v.len().min(cap - 1);
        std::ptr::copy_nonoverlapping(v.as_ptr(), string, n);
        *string.add(n) = 0;
        kResultOk
    }
    unsafe fn setBinary(&self, id: IAttributeList_::AttrID, data: *const c_void, size: uint32) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let v = if data.is_null() { Vec::new() } else { std::slice::from_raw_parts(data as *const u8, size as usize).to_vec() };
        self.0.lock().insert(k, Attr::Bin(v));
        kResultOk
    }
    unsafe fn getBinary(&self, id: IAttributeList_::AttrID, data: *mut *const c_void, size: *mut uint32) -> tresult {
        let g = self.0.lock();
        match key(id).and_then(|k| g.get(&k)) {
            Some(Attr::Bin(v)) => {
                // The map entry outlives this call (until it's overwritten), as the API expects.
                *data = v.as_ptr() as *const c_void;
                *size = v.len() as u32;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

/// In-memory IBStream for state transfer.
#[derive(Default)]
pub(crate) struct MemStream(Mutex<(Vec<u8>, usize)>);

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl MemStream {
    fn with(data: Vec<u8>) -> ComWrapper<MemStream> {
        ComWrapper::new(MemStream(Mutex::new((data, 0))))
    }
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let mut g = self.0.lock();
        let (data, pos) = &mut *g;
        let k = (n.max(0) as usize).min(data.len().saturating_sub(*pos));
        if k > 0 {
            std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer as *mut u8, k);
        }
        *pos += k;
        if !read.is_null() {
            *read = k as int32;
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        let mut g = self.0.lock();
        let (data, pos) = &mut *g;
        let n = n.max(0) as usize;
        let src = std::slice::from_raw_parts(buffer as *const u8, n);
        if *pos + n > data.len() {
            data.resize(*pos + n, 0);
        }
        data[*pos..*pos + n].copy_from_slice(src);
        *pos += n;
        if !written.is_null() {
            *written = n as int32;
        }
        kResultOk
    }
    unsafe fn seek(&self, p: int64, mode: int32, result: *mut int64) -> tresult {
        let mut g = self.0.lock();
        let (data, pos) = &mut *g;
        use IBStream_::IStreamSeekMode_::*;
        let base = match mode as u32 {
            m if m == kIBSeekSet as u32 => 0i64,
            m if m == kIBSeekCur as u32 => *pos as i64,
            m if m == kIBSeekEnd as u32 => data.len() as i64,
            _ => return kInvalidArgument,
        };
        let np = (base + p).max(0) as usize;
        *pos = np;
        if !result.is_null() {
            *result = np as int64;
        }
        kResultOk
    }
    unsafe fn tell(&self, p: *mut int64) -> tresult {
        if !p.is_null() {
            *p = self.0.lock().1 as int64;
        }
        kResultOk
    }
}

/// One parameter's changes for one block.
struct ParamQueue {
    id: ParamID,
    points: Mutex<Vec<(i32, f64)>>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        self.points.lock().len() as int32
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        match self.points.lock().get(index as usize) {
            Some((o, v)) => {
                *offset = *o;
                *value = *v;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let mut p = self.points.lock();
        p.push((offset, value));
        if !index.is_null() {
            *index = p.len() as int32 - 1;
        }
        kResultOk
    }
}

#[derive(Default)]
struct ParamChanges(Mutex<Vec<ComPtr<IParamValueQueue>>>, Mutex<Vec<ParamID>>);

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    fn add(&self, id: ParamID, value: f64) {
        let mut ids = self.1.lock();
        let mut qs = self.0.lock();
        match ids.iter().position(|i| *i == id) {
            Some(i) => unsafe {
                if let Some(q) = ComRef::from_raw(qs[i].as_ptr()) {
                    let mut idx = 0;
                    q.addPoint(0, value, &mut idx);
                }
            },
            None => {
                if let Some(q) = ComWrapper::new(ParamQueue { id, points: Mutex::new(vec![(0, value)]) }).to_com_ptr::<IParamValueQueue>() {
                    ids.push(id);
                    qs.push(q);
                }
            }
        }
    }
    fn clear(&self) {
        self.0.lock().clear();
        self.1.lock().clear();
    }

    /// Changes the plugin wrote (output parameter changes): last value per id.
    fn take_last(&self) -> Vec<(ParamID, f64)> {
        let qs = std::mem::take(&mut *self.0.lock());
        self.1.lock().clear();
        qs.iter()
            .filter_map(|q| unsafe {
                let q = ComRef::from_raw(q.as_ptr())?;
                let n = q.getPointCount();
                let (mut o, mut v) = (0, 0.0);
                (n > 0 && q.getPoint(n - 1, &mut o, &mut v) == kResultOk).then(|| (q.getParameterId(), v))
            })
            .collect()
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.0.lock().len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        self.0.lock().get(index as usize).map(|q| q.as_ptr()).unwrap_or(std::ptr::null_mut())
    }
    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        let id = *id;
        let mut ids = self.1.lock();
        let mut qs = self.0.lock();
        let i = match ids.iter().position(|x| *x == id) {
            Some(i) => i,
            None => {
                let Some(q) = ComWrapper::new(ParamQueue { id, points: Mutex::new(Vec::new()) }).to_com_ptr::<IParamValueQueue>() else {
                    return std::ptr::null_mut();
                };
                ids.push(id);
                qs.push(q);
                qs.len() - 1
            }
        };
        if !index.is_null() {
            *index = i as int32;
        }
        qs[i].as_ptr()
    }
}

/// State shared between the audio thread, the UI and the editor window.
#[derive(Default)]
pub(crate) struct Shared {
    /// Parameter changes waiting for the next audio block.
    pub(crate) pending: Mutex<Vec<(ParamID, f64)>>,
    /// Bumped whenever a parameter changes (so the UI can resave state).
    pub(crate) edits: std::sync::atomic::AtomicU64,
    pub(crate) latency_changed: std::sync::atomic::AtomicBool,
}

/// Receives edits made in the plugin's own editor.
struct Handler(Arc<Shared>);

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.0.pending.lock().push((id, value));
        self.0.edits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        if flags & RestartFlags_::kLatencyChanged as int32 != 0 {
            self.0.latency_changed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.0.edits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        kResultOk
    }
}

// ---------------------------------------------------------------- plugin

/// One parameter as shown in Ferrous's generic editor.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub id: u32,
    pub title: String,
    pub units: String,
    /// 0 = continuous, 1 = toggle, n = n+1 discrete steps.
    pub steps: i32,
    pub default: f64,
    pub value: f64,
    pub display: String,
    pub read_only: bool,
    pub is_bypass: bool,
}

/// A running VST3 effect.
pub struct Plugin {
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    controller: Option<ComPtr<IEditController>>,
    connections: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    host: ComPtr<FUnknown>,
    shared: Arc<Shared>,
    in_channels: i32,
    out_channels: i32,
    n_in_buses: i32,
    n_out_buses: i32,
    planar: Vec<Vec<f32>>,
    scratch: Vec<Vec<f32>>,
    block: usize,
    out_changes: ComWrapper<ParamChanges>,
    /// Reused every block (cleared, then filled with pending edits) rather
    /// than allocating a COM object per block on the audio thread.
    in_changes: ComWrapper<ParamChanges>,
    /// Channels per (input, output) bus; arrangements are fixed once
    /// processing starts, so they're queried on the first block only.
    bus_channels: Option<(Vec<i32>, Vec<i32>)>,
    pub name: String,
    #[cfg(windows)]
    editor: Mutex<Option<editor::EditorHandle>>,
    // Last: unload the library only after everything above is released.
    _module: Arc<Module>,
}

unsafe impl Send for Plugin {}

impl Plugin {
    /// Creates and starts the effect `cid` (hex) from `bundle` for 48 kHz
    /// stereo blocks of up to `max_block` frames, restoring `state` (from
    /// [`Plugin::save_state`]) if given.
    pub fn load(bundle: &Path, cid: &str, sample_rate: f64, max_block: usize, state: Option<&str>) -> anyhow::Result<Plugin> {
        let module = Module::load(bundle)?;
        let tuid = hex_tuid(cid).ok_or_else(|| anyhow!("bad class id {cid}"))?;
        let name = module.classes().into_iter().find(|c| c.cid.eq_ignore_ascii_case(cid)).map(|c| c.name).unwrap_or_else(|| "VST3".into());
        unsafe {
            let host = ComWrapper::new(HostApp).to_com_ptr::<FUnknown>().ok_or_else(|| anyhow!("host context"))?;
            let mut obj = std::ptr::null_mut();
            if module.factory().createInstance(tuid.as_ptr(), IComponent::IID.as_ptr() as FIDString, &mut obj) != kResultOk || obj.is_null() {
                bail!("the plugin wouldn't create {name}");
            }
            let component = ComPtr::<IComponent>::from_raw(obj as *mut IComponent).unwrap();
            if component.initialize(host.as_ptr()) != kResultOk {
                bail!("{name} failed to initialise");
            }
            let processor = component.cast::<IAudioProcessor>().ok_or_else(|| anyhow!("{name} has no audio processor"))?;

            // Edit controller: a separate class, or the component itself.
            let mut controller: Option<ComPtr<IEditController>> = component.cast::<IEditController>();
            let mut separate = false;
            if controller.is_none() {
                let mut ccid: TUID = [0; 16];
                if component.getControllerClassId(&mut ccid) == kResultOk && ccid.iter().any(|b| *b != 0) {
                    let mut obj = std::ptr::null_mut();
                    if module.factory().createInstance(ccid.as_ptr(), IEditController::IID.as_ptr() as FIDString, &mut obj) == kResultOk && !obj.is_null() {
                        let c = ComPtr::<IEditController>::from_raw(obj as *mut IEditController).unwrap();
                        if c.initialize(host.as_ptr()) == kResultOk {
                            controller = Some(c);
                            separate = true;
                        }
                    }
                }
            }
            let shared = Arc::new(Shared::default());
            if let Some(c) = &controller {
                if let Some(h) = ComWrapper::new(Handler(shared.clone())).to_com_ptr::<IComponentHandler>() {
                    c.setComponentHandler(h.as_ptr());
                }
            }
            let connections = match (&controller, separate) {
                (Some(c), true) => match (component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>()) {
                    (Some(a), Some(b)) => {
                        a.connect(b.as_ptr());
                        b.connect(a.as_ptr());
                        Some((a, b))
                    }
                    _ => None,
                },
                _ => None,
            };

            // State: restore saved state, else hand the component's defaults to the controller.
            let (cstate, estate) = state.map(decode_state).unwrap_or_default();
            if let Some(cs) = cstate {
                let s = MemStream::with(cs.clone());
                if let Some(p) = s.to_com_ptr::<IBStream>() {
                    component.setState(p.as_ptr());
                }
                if let (Some(c), true) = (&controller, separate) {
                    let s = MemStream::with(cs);
                    if let Some(p) = s.to_com_ptr::<IBStream>() {
                        c.setComponentState(p.as_ptr());
                    }
                }
                if let (Some(c), Some(es)) = (&controller, estate) {
                    let s = MemStream::with(es);
                    if let Some(p) = s.to_com_ptr::<IBStream>() {
                        c.setState(p.as_ptr());
                    }
                }
            } else if let (Some(c), true) = (&controller, separate) {
                let s = MemStream::with(Vec::new());
                if let Some(p) = s.to_com_ptr::<IBStream>() {
                    if component.getState(p.as_ptr()) == kResultOk {
                        p.seek(0, IBStream_::IStreamSeekMode_::kIBSeekSet as int32, std::ptr::null_mut());
                        c.setComponentState(p.as_ptr());
                    }
                }
            }

            // Buses: main audio in and out, stereo if the plugin agrees, else mono.
            let kaudio = MediaTypes_::kAudio as MediaType;
            let (kin, kout) = (BusDirections_::kInput as BusDirection, BusDirections_::kOutput as BusDirection);
            let n_in = component.getBusCount(kaudio, kin);
            let n_out = component.getBusCount(kaudio, kout);
            if n_in < 1 || n_out < 1 {
                bail!("{name} isn't an effect (it has no audio input)");
            }
            let arr_of = |dir, n: i32| -> Vec<SpeakerArrangement> {
                (0..n)
                    .map(|i| {
                        let mut a: SpeakerArrangement = 0;
                        processor.getBusArrangement(dir, i, &mut a);
                        a
                    })
                    .collect()
            };
            let mut ins = arr_of(kin, n_in);
            let mut outs = arr_of(kout, n_out);
            let mut chans = 2;
            for want in [SpeakerArr::kStereo, SpeakerArr::kMono] {
                ins[0] = want;
                outs[0] = want;
                if processor.setBusArrangements(ins.as_mut_ptr(), n_in, outs.as_mut_ptr(), n_out) == kResultTrue {
                    chans = if want == SpeakerArr::kStereo { 2 } else { 1 };
                    break;
                }
                // Some plugins return false yet keep working with what they had.
                let now = arr_of(kin, 1)[0];
                if now == want {
                    chans = if want == SpeakerArr::kStereo { 2 } else { 1 };
                    break;
                }
            }
            let ins_now = arr_of(kin, n_in);
            let outs_now = arr_of(kout, n_out);
            let in_channels = channel_count(ins_now[0]).max(1).min(chans.max(channel_count(ins_now[0])));
            let out_channels = channel_count(outs_now[0]).max(1);
            component.activateBus(kaudio, kin, 0, 1);
            component.activateBus(kaudio, kout, 0, 1);

            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as int32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                maxSamplesPerBlock: max_block as int32,
                sampleRate: sample_rate,
            };
            if processor.setupProcessing(&mut setup) != kResultOk {
                bail!("{name} refused 48 kHz / 32-bit processing");
            }
            component.setActive(1);
            processor.setProcessing(1);

            // Buffers for every bus's channels (extra buses get silence).
            let mut planar = Vec::new();
            for a in &ins_now {
                for _ in 0..channel_count(*a).max(if planar.is_empty() { in_channels } else { 0 }) {
                    planar.push(vec![0f32; max_block]);
                }
            }
            let mut scratch = Vec::new();
            for a in &outs_now {
                for _ in 0..channel_count(*a).max(if scratch.is_empty() { out_channels } else { 0 }) {
                    scratch.push(vec![0f32; max_block]);
                }
            }
            let _ = &arr_of;
            Ok(Plugin {
                processor,
                component,
                controller,
                connections,
                host,
                shared,
                in_channels,
                out_channels,
                n_in_buses: n_in,
                n_out_buses: n_out,
                planar,
                scratch,
                block: max_block,
                out_changes: ComWrapper::new(ParamChanges::default()),
                in_changes: ComWrapper::new(ParamChanges::default()),
                bus_channels: None,
                name,
                #[cfg(windows)]
                editor: Mutex::new(None),
                _module: module,
            })
        }
    }

    /// Processes interleaved stereo samples in place.
    pub fn process(&mut self, interleaved: &mut [f32]) {
        let frames = interleaved.len() / 2;
        for chunk_start in (0..frames).step_by(self.block.max(1)) {
            let n = (frames - chunk_start).min(self.block);
            let buf = &mut interleaved[chunk_start * 2..(chunk_start + n) * 2];
            self.process_block(buf, n);
        }
    }

    fn process_block(&mut self, buf: &mut [f32], n: usize) {
        // De-interleave into the main input bus.
        for (i, fr) in buf.chunks_exact(2).enumerate() {
            if self.in_channels >= 2 {
                self.planar[0][i] = fr[0];
                self.planar[1][i] = fr[1];
            } else {
                self.planar[0][i] = 0.5 * (fr[0] + fr[1]);
            }
        }
        let changes = &self.in_changes;
        changes.clear();
        {
            let mut pending = self.shared.pending.lock();
            for (id, v) in pending.iter() {
                changes.add(*id, *v);
            }
            pending.clear();
        }
        if self.bus_channels.is_none() {
            let counts = |dir: BusDirection, nbus: i32, first: i32| -> Vec<i32> {
                (0..nbus)
                    .map(|b| {
                        let mut a: SpeakerArrangement = 0;
                        unsafe { self.processor.getBusArrangement(dir, b, &mut a) };
                        if b == 0 { first } else { channel_count(a) }
                    })
                    .collect()
            };
            self.bus_channels = Some((
                counts(BusDirections_::kInput as BusDirection, self.n_in_buses, self.in_channels),
                counts(BusDirections_::kOutput as BusDirection, self.n_out_buses, self.out_channels),
            ));
        }
        let (in_ch, out_ch) = self.bus_channels.as_ref().unwrap();
        unsafe {
            let mut in_ptrs: Vec<*mut f32> = self.planar.iter_mut().map(|v| v.as_mut_ptr()).collect();
            let mut out_ptrs: Vec<*mut f32> = self.scratch.iter_mut().map(|v| v.as_mut_ptr()).collect();
            let bus_bufs = |counts: &[i32], ptrs: &mut Vec<*mut f32>| -> Vec<AudioBusBuffers> {
                let mut off = 0usize;
                counts
                    .iter()
                    .map(|&ch| {
                        let mut bb: AudioBusBuffers = std::mem::zeroed();
                        bb.numChannels = ch;
                        bb.__field0.channelBuffers32 = ptrs.as_mut_ptr().add(off.min(ptrs.len()));
                        off += ch as usize;
                        bb
                    })
                    .collect()
            };
            let mut ins = bus_bufs(in_ch, &mut in_ptrs);
            let mut outs = bus_bufs(out_ch, &mut out_ptrs);
            let in_changes = changes.to_com_ptr::<IParameterChanges>();
            let out_changes = self.out_changes.to_com_ptr::<IParameterChanges>();
            let mut data = ProcessData {
                processMode: ProcessModes_::kRealtime as int32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                numSamples: n as int32,
                numInputs: ins.len() as int32,
                numOutputs: outs.len() as int32,
                inputs: ins.as_mut_ptr(),
                outputs: outs.as_mut_ptr(),
                inputParameterChanges: in_changes.as_ref().map(|p| p.as_ptr()).unwrap_or(std::ptr::null_mut()),
                outputParameterChanges: out_changes.as_ref().map(|p| p.as_ptr()).unwrap_or(std::ptr::null_mut()),
                inputEvents: std::ptr::null_mut(),
                outputEvents: std::ptr::null_mut(),
                processContext: std::ptr::null_mut(),
            };
            if self.processor.process(&mut data) != kResultOk {
                return; // leave the audio untouched
            }
        }
        // Interleave the main output bus back.
        for (i, fr) in buf.chunks_exact_mut(2).enumerate() {
            if self.out_channels >= 2 {
                fr[0] = self.scratch[0][i];
                fr[1] = self.scratch[1][i];
            } else {
                fr[0] = self.scratch[0][i];
                fr[1] = self.scratch[0][i];
            }
        }
        // Values the plugin reported back go to its controller (meters, etc.).
        let back = self.out_changes.take_last();
        if let Some(c) = &self.controller {
            for (id, v) in back {
                unsafe {
                    c.setParamNormalized(id, v);
                }
            }
        }
    }

    /// The effect's parameters (hidden ones left out).
    pub fn params(&self) -> Vec<Param> {
        let Some(c) = &self.controller else { return Vec::new() };
        let mut out = Vec::new();
        unsafe {
            for i in 0..c.getParameterCount() {
                let mut info: ParameterInfo = std::mem::zeroed();
                if c.getParameterInfo(i, &mut info) != kResultOk {
                    continue;
                }
                let flags = info.flags;
                if flags & ParameterInfo_::ParameterFlags_::kIsHidden as i32 != 0 {
                    continue;
                }
                let value = c.getParamNormalized(info.id);
                let mut s: String128 = [0; 128];
                let display = if c.getParamStringByValue(info.id, value, &mut s) == kResultOk { wstr(&s) } else { format!("{value:.3}") };
                out.push(Param {
                    id: info.id,
                    title: wstr(&info.title),
                    units: wstr(&info.units),
                    steps: info.stepCount,
                    default: info.defaultNormalizedValue,
                    value,
                    display,
                    read_only: flags & ParameterInfo_::ParameterFlags_::kIsReadOnly as i32 != 0,
                    is_bypass: flags & ParameterInfo_::ParameterFlags_::kIsBypass as i32 != 0,
                });
            }
        }
        out
    }

    /// Sets a parameter (normalised 0..1): the controller now, the processor
    /// at the next block.
    pub fn set_param(&self, id: u32, value: f64) {
        let v = value.clamp(0.0, 1.0);
        if let Some(c) = &self.controller {
            unsafe {
                c.setParamNormalized(id, v);
            }
        }
        self.shared.pending.lock().push((id, v));
        self.shared.edits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counter that changes whenever a parameter is edited (here or in the
    /// plugin's own editor).
    pub fn edit_count(&self) -> u64 {
        self.shared.edits.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn latency_samples(&self) -> u32 {
        unsafe { self.processor.getLatencySamples() }
    }

    /// Hands queued parameter changes to the processor now (a zero-sample
    /// "parameter flush" block, which the VST3 API provides for this).
    pub fn flush_params(&mut self) {
        if self.shared.pending.lock().is_empty() {
            return;
        }
        let pending = std::mem::take(&mut *self.shared.pending.lock());
        let changes = ComWrapper::new(ParamChanges::default());
        for (id, v) in &pending {
            changes.add(*id, *v);
        }
        unsafe {
            let in_changes = changes.to_com_ptr::<IParameterChanges>();
            let mut data: ProcessData = std::mem::zeroed();
            data.processMode = ProcessModes_::kRealtime as int32;
            data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
            data.inputParameterChanges = in_changes.as_ref().map(|p| p.as_ptr()).unwrap_or(std::ptr::null_mut());
            self.processor.process(&mut data);
        }
    }

    /// Component and controller state as one string (base64, `c:...;e:...`).
    /// Takes `&mut` to flush pending parameter changes into the processor first.
    pub fn save_state(&mut self) -> String {
        self.flush_params();
        let grab = |f: &dyn Fn(*mut IBStream) -> tresult| -> Option<Vec<u8>> {
            let s = MemStream::with(Vec::new());
            let p = s.to_com_ptr::<IBStream>()?;
            (f(p.as_ptr()) == kResultOk).then(|| s.0.lock().0.clone())
        };
        let b64 = base64::engine::general_purpose::STANDARD;
        let c = grab(&|p| unsafe { self.component.getState(p) });
        let e = self.controller.as_ref().and_then(|ctl| grab(&|p| unsafe { ctl.getState(p) }));
        let mut out = String::new();
        if let Some(c) = c {
            out.push_str("c:");
            out.push_str(&b64.encode(c));
        }
        if let Some(e) = e.filter(|e| !e.is_empty()) {
            out.push_str(";e:");
            out.push_str(&b64.encode(e));
        }
        out
    }

    /// Whether the plugin has its own editor that can open here.
    pub fn has_editor(&self) -> bool {
        #[cfg(windows)]
        {
            self.controller.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Opens (or focuses) the plugin's own editor window. Windows only.
    pub fn open_editor(&self, title: &str) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            let Some(c) = &self.controller else { bail!("{} has no editor", self.name) };
            let mut ed = self.editor.lock();
            if let Some(h) = ed.as_ref() {
                if h.is_open() {
                    h.focus();
                    return Ok(());
                }
            }
            *ed = Some(editor::open(c.clone(), title)?);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = title;
            bail!("plugin editors open on Windows only")
        }
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(h) = self.editor.lock().take() {
            h.close();
        }
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = &self.connections {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if c.cast::<IComponent>().is_none() {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
        let _ = &self.host;
    }
}

fn channel_count(arr: SpeakerArrangement) -> i32 {
    arr.count_ones() as i32
}

fn decode_state(s: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut c = None;
    let mut e = None;
    for part in s.split(';') {
        if let Some(v) = part.strip_prefix("c:") {
            c = b64.decode(v).ok();
        } else if let Some(v) = part.strip_prefix("e:") {
            e = b64.decode(v).ok();
        }
    }
    (c, e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuid_hex_round_trip() {
        let t: TUID = [1, -2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 127];
        let h = tuid_hex(&t);
        assert_eq!(h.len(), 32);
        assert_eq!(hex_tuid(&h), Some(t));
        assert_eq!(hex_tuid("xyz"), None);
    }

    #[test]
    fn state_string_round_trip() {
        let (c, e) = decode_state("c:AAEC;e:AwQ=");
        assert_eq!(c, Some(vec![0, 1, 2]));
        assert_eq!(e, Some(vec![3, 4]));
        assert_eq!(decode_state(""), (None, None));
    }

    #[test]
    fn finds_bundles_two_levels_deep() {
        let d = std::env::temp_dir().join("ferrous-vst3-find");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Vendor").join("Comp.vst3")).unwrap();
        std::fs::create_dir_all(d.join("EQ.vst3")).unwrap();
        std::fs::write(d.join("readme.txt"), "x").unwrap();
        let found = find_bundles(std::slice::from_ref(&d));
        assert_eq!(found.len(), 2, "{found:?}");
    }
}
