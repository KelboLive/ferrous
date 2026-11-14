//! In-process NVENC (NVIDIA's hardware encoder), driven straight from the compositor's Direct3D 12
//! device, the way OBS does it: the finished picture is encoded where it already is, on the GPU,
//! instead of being copied to system memory and piped to an FFmpeg process. FFmpeg then only gets
//! the small compressed stream to put in a file or send to a server.
//!
//! `nvEncodeAPI64.dll` comes with the NVIDIA driver and is loaded at run time, so Ferrous starts
//! normally on PCs without it and keeps using the FFmpeg path there.

pub mod sys;
#[cfg(windows)]
pub mod encoder;

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

use sys::*;

/// `NVENCAPI_STRUCT_VERSION(v)` from the header (a macro, so bindgen can't emit it).
pub const fn struct_ver(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1 as _, Data2: d2, Data3: d3, Data4: d4 }
}

// GUIDs are `static const` in the header (not exported by the DLL), so they are spelled out here.
pub const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
pub const CODEC_HEVC: GUID = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
pub const CODEC_AV1: GUID = guid(0x0a352289, 0x0aa7, 0x4759, [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54]);

fn same(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn guid(self) -> GUID {
        match self {
            Codec::H264 => CODEC_H264,
            Codec::Hevc => CODEC_HEVC,
            Codec::Av1 => CODEC_AV1,
        }
    }
}

/// How the encoder should spend its bits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rate {
    /// Constant bitrate (streaming), kbps.
    Cbr(u32),
    /// Variable bitrate around a target, kbps, with headroom for complex scenes.
    Vbr(u32),
    /// Constant quality (NVENC's CQ, 1–51; lower is better), for recordings.
    Quality(u8),
    /// Fixed quantiser.
    Cqp(u8),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub codec: Codec,
    /// Canvas size (what the compositor draws).
    pub input: (u32, u32),
    /// Encoded size (even); the video processor scales when it differs.
    pub output: (u32, u32),
    pub fps: u32,
    pub rate: Rate,
    /// NVENC preset 1–7 (p1 fastest, p7 best).
    pub preset: u8,
    pub keyframe_interval_frames: u32,
    /// Look ahead (better bit distribution, more latency): recordings yes, live links no.
    pub lookahead: bool,
    /// Quarter-resolution first pass.
    pub multipass: bool,
    /// B-frames between reference frames (H.264/HEVC; AV1 ignores it). More efficient, one frame
    /// more latency each.
    pub bframes: u32,
}

/// Counters the engine shows and logs.
#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub repeats: AtomicU64,
    /// Frames the render thread had to leave out because the encoder was behind.
    pub skipped: AtomicU64,
    pub bytes: AtomicU64,
    pub failed: AtomicBool,
    pub error: Mutex<String>,
}

impl Stats {
    pub fn fail(&self, e: impl std::fmt::Display) {
        log::error!("GPU encoder: {e}");
        *self.error.lock() = e.to_string();
        self.failed.store(true, Ordering::Relaxed);
    }
}

/// Where compressed frames go. Returning false stops the encoder (the output has ended).
pub type Sink = Box<dyn FnMut(Packet<'_>) -> bool + Send>;

/// One compressed frame, in decode order.
#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
    pub data: &'a [u8],
    /// Display position, in frames from the first.
    pub pts: u64,
    /// Decode position, in frames (may start negative with B-frames); never after `pts`.
    pub dts: i64,
    pub keyframe: bool,
}

/// The driver's NVENC entry points.
pub struct Api {
    _lib: libloading::Library,
    pub f: NV_ENCODE_API_FUNCTION_LIST,
    /// Highest API version the driver supports, as `major.minor`.
    pub driver_version: (u32, u32),
}

// The function list is a table of plain C function pointers into the driver DLL.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

pub(crate) fn check(what: &str, s: NVENCSTATUS) -> anyhow::Result<()> {
    if s == _NVENCSTATUS_NV_ENC_SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("NVENC {what} failed (status {s})")
    }
}

impl Api {
    /// Loads the driver's NVENC library, or explains why it can't be used.
    pub fn load() -> anyhow::Result<Self> {
        let name = if cfg!(windows) { "nvEncodeAPI64.dll" } else { "libnvidia-encode.so.1" };
        // SAFETY: loading the NVIDIA driver's own library; it has no initialisers we depend on.
        let lib = unsafe { libloading::Library::new(name) }.map_err(|e| anyhow::anyhow!("no NVIDIA encoder driver ({name}): {e}"))?;
        type GetMax = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;
        type Create = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
        // SAFETY: symbol types match the header's declarations.
        let (get_max, create) = unsafe {
            (*lib.get::<GetMax>(b"NvEncodeAPIGetMaxSupportedVersion\0")?, *lib.get::<Create>(b"NvEncodeAPICreateInstance\0")?)
        };
        let mut v = 0u32;
        // SAFETY: writes one u32.
        check("version query", unsafe { get_max(&mut v) })?;
        let driver_version = (v >> 4, v & 0xf);
        let ours = (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
        if driver_version < ours {
            anyhow::bail!("the NVIDIA driver supports encoder API {}.{}, Ferrous needs {}.{}: update the driver", driver_version.0, driver_version.1, ours.0, ours.1);
        }
        // SAFETY: the list is plain data (u32s, pointers); all-zero is its documented initial state.
        let mut f: NV_ENCODE_API_FUNCTION_LIST = unsafe { std::mem::zeroed() };
        f.version = struct_ver(2);
        // SAFETY: fills the function table.
        check("create instance", unsafe { create(&mut f) })?;
        Ok(Self { _lib: lib, f, driver_version })
    }

    /// Opens an encode session on a Direct3D 12 device (`ID3D12Device*`).
    ///
    /// # Safety
    /// `device` must be a live `ID3D12Device` (or `ID3D11Device`) pointer for the session's lifetime.
    pub unsafe fn open(&self, device: *mut c_void) -> anyhow::Result<Session<'_>> {
        let mut p: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
        p.version = struct_ver(1);
        p.deviceType = _NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_DIRECTX;
        p.device = device;
        p.apiVersion = NVENCAPI_VERSION;
        let mut enc = ptr::null_mut();
        let open = self.f.nvEncOpenEncodeSessionEx.ok_or_else(|| anyhow::anyhow!("NVENC: no open-session entry"))?;
        check("open session", open(&mut p, &mut enc))?;
        Ok(Session { api: self, enc })
    }
}

/// One encoder session; closed on drop.
pub struct Session<'a> {
    pub api: &'a Api,
    pub enc: *mut c_void,
}

impl Session<'_> {
    /// Codecs this GPU can encode.
    pub fn codecs(&self) -> anyhow::Result<Vec<Codec>> {
        let f = &self.api.f;
        let (count_fn, list_fn) = (f.nvEncGetEncodeGUIDCount.ok_or_else(|| anyhow::anyhow!("no GUID count"))?, f.nvEncGetEncodeGUIDs.ok_or_else(|| anyhow::anyhow!("no GUID list"))?);
        let mut n = 0u32;
        // SAFETY: valid session; writes one u32.
        check("codec count", unsafe { count_fn(self.enc, &mut n) })?;
        let mut guids = vec![guid(0, 0, 0, [0; 8]); n as usize];
        let mut got = 0u32;
        // SAFETY: the buffer holds `n` GUIDs.
        check("codec list", unsafe { list_fn(self.enc, guids.as_mut_ptr(), n, &mut got) })?;
        guids.truncate(got as usize);
        Ok([Codec::H264, Codec::Hevc, Codec::Av1].into_iter().filter(|c| guids.iter().any(|g| same(g, &c.guid()))).collect())
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(destroy) = self.api.f.nvEncDestroyEncoder {
            // SAFETY: the session was opened by this API and is destroyed once.
            unsafe { destroy(self.enc) };
        }
    }
}

/// Stage 1 check: can NVENC run in-process on this device? Logs the answer either way.
///
/// # Safety
/// `device` must be a live `ID3D12Device` pointer.
pub unsafe fn probe(device: *mut c_void) -> Option<Vec<Codec>> {
    let api = match Api::load() {
        Ok(a) => a,
        Err(e) => {
            log::info!("in-process NVENC unavailable: {e}");
            return None;
        }
    };
    let result = api.open(device).and_then(|s| s.codecs());
    match result {
        Ok(c) => {
            log::info!("in-process NVENC ready on the compositor's GPU (driver API {}.{}): {:?}", api.driver_version.0, api.driver_version.1, c);
            Some(c)
        }
        Err(e) => {
            log::warn!("in-process NVENC failed to open on the compositor's GPU: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_versions_match_the_header() {
        // NV_ENCODE_API_FUNCTION_LIST_VER for API 13.1.
        assert_eq!(struct_ver(2), NVENCAPI_VERSION | (2 << 16) | 0x7000_0000);
        assert_eq!(NVENCAPI_VERSION, 13 | (1 << 24));
    }

    #[test]
    fn missing_driver_is_an_error_not_a_crash() {
        if Api::load().is_err() {
            // Expected on machines without an NVIDIA driver.
        }
    }
}
