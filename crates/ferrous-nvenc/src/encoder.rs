//! Zero-copy NVENC encoding of the compositor's canvas (Windows).
//!
//! The canvas lives in a Direct3D 12 texture owned by wgpu. Each frame it is copied, GPU to GPU, into
//! one of a few textures shared with a Direct3D 11 device on the same graphics card. On that device
//! the GPU's video processor converts it to NV12 (BT.709, limited range, scaled if the output size
//! differs) and NVENC encodes it. Two shared fences keep the devices in step without the CPU waiting:
//! "ready" (D3D12 signals once a copy has landed, D3D11 waits for it) and "free" (D3D11 signals once
//! the video processor has read a slot; the render thread only reuses a slot that is free).
//!
//! The encoder runs on its own thread and hands each compressed frame to a sink, which in Ferrous is
//! an FFmpeg process that only muxes (`-c:v copy`). This is the path OBS takes: the picture never
//! visits system memory, and no raw frames go through a pipe.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Fence, ID3D12Resource, D3D12_FENCE_FLAG_SHARED};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4, IDXGIResource1};

use crate::sys::*;
use crate::{check, struct_ver, Api, Codec, Config, Rate, Sink, Stats};

/// Shared canvas slots: one being written, one being converted, one spare.
const SLOTS: usize = 3;
/// GENERIC_ALL, for shared handles.
const GENERIC_ALL: u32 = 0x1000_0000;

enum Job {
    /// The canvas is in `slot` once the ready fence reaches `ready`.
    Frame { slot: usize, ready: u64, pts: u64 },
    /// No new picture this tick (a static scene): encode the previous one again.
    Repeat { pts: u64 },
}

// SAFETY: D3D11/D3D12 devices, textures and fences are free-threaded COM objects; each is used by one
// thread at a time here (the immediate context only ever on the encoder thread).
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

/// The render thread's half: owns the shared slots and decides which one to fill each frame.
pub struct GpuEncoder {
    slots: Vec<ID3D12Resource>,
    last_use: [u64; SLOTS],
    ready: ID3D12Fence,
    free: ID3D12Fence,
    value: u64,
    next: usize,
    /// Highest pts sent to the encoder thread.
    sent_pts: Option<u64>,
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<Stats>,
    pub config: Config,
}

/// A frame the render thread is about to copy into a slot.
pub struct Pending {
    pub slot: usize,
    pub value: u64,
}

impl GpuEncoder {
    /// Sets up the shared slots, the D3D11 device on the same GPU and the NVENC session.
    ///
    /// # Safety
    /// `d3d12` must be the compositor's live device.
    pub unsafe fn new(d3d12: &ID3D12Device, config: Config, sink: Sink) -> anyhow::Result<Self> {
        let api = Arc::new(Api::load()?);
        let luid: LUID = d3d12.GetAdapterLuid();
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter = factory.EnumAdapterByLuid(luid)?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device: ID3D11Device = device.ok_or_else(|| anyhow::anyhow!("no D3D11 device"))?;
        let context: ID3D11DeviceContext = context.ok_or_else(|| anyhow::anyhow!("no D3D11 context"))?;
        let device5: ID3D11Device5 = device.cast()?;
        let context4: ID3D11DeviceContext4 = context.cast()?;

        // Fences, created on D3D12 (shared) and opened on D3D11.
        let ready: ID3D12Fence = d3d12.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
        let free: ID3D12Fence = d3d12.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
        let open_fence = |f: &ID3D12Fence| -> anyhow::Result<ID3D11Fence> {
            let h = d3d12.CreateSharedHandle(f, None, GENERIC_ALL, PCWSTR::null())?;
            let mut out: Option<ID3D11Fence> = None;
            let r = device5.OpenSharedFence(h, &mut out);
            let _ = CloseHandle(h);
            r?;
            out.ok_or_else(|| anyhow::anyhow!("couldn't open a shared fence"))
        };
        let ready11 = open_fence(&ready)?;
        let free11 = open_fence(&free)?;

        // Shared BGRA canvas slots, created on D3D11 and opened on D3D12.
        let (iw, ih) = config.input;
        let bgra_desc = D3D11_TEXTURE2D_DESC {
            Width: iw,
            Height: ih,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut slots11 = Vec::new();
        let mut slots12 = Vec::new();
        for _ in 0..SLOTS {
            let mut tex: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&bgra_desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(|| anyhow::anyhow!("no slot texture"))?;
            let h = tex.cast::<IDXGIResource1>()?.CreateSharedHandle(None, 0x8000_0001, PCWSTR::null())?;
            let mut res: Option<ID3D12Resource> = None;
            let r = d3d12.OpenSharedHandle(h, &mut res);
            let _ = CloseHandle(h);
            r?;
            slots12.push(res.ok_or_else(|| anyhow::anyhow!("couldn't open a slot on D3D12"))?);
            slots11.push(tex);
        }

        // Video processor: BGRA (sRGB-encoded, full range) -> NV12 (BT.709, limited range), scaling.
        let (ow, oh) = config.output;
        let vdev: ID3D11VideoDevice = device.cast()?;
        let vctx: ID3D11VideoContext1 = context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: config.fps.max(1), Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: iw,
            InputHeight: ih,
            OutputFrameRate: rate,
            OutputWidth: ow,
            OutputHeight: oh,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        let vp_enum = vdev.CreateVideoProcessorEnumerator(&content)?;
        let vp = vdev.CreateVideoProcessor(&vp_enum, 0)?;
        vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
        vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        vctx.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
        let mut in_views = Vec::new();
        for t in &slots11 {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut v = None;
            vdev.CreateVideoProcessorInputView(t, &vp_enum, &desc, Some(&mut v))?;
            in_views.push(v.ok_or_else(|| anyhow::anyhow!("no input view"))?);
        }

        // NVENC session on the D3D11 device, and its pool of NV12 inputs and bitstream buffers.
        let session = Session::open(api.clone(), device.as_raw())?;
        let lookahead = if config.lookahead { 20 } else { 0 };
        let bframes = if config.codec == Codec::Av1 { 0 } else { config.bframes.min(4) };
        let pool = 4 + lookahead as usize + bframes as usize;
        let enc = session.initialize(&config, lookahead, bframes)?;
        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: ow,
            Height: oh,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut frames = Vec::new();
        for _ in 0..pool {
            let mut tex: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&nv12_desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(|| anyhow::anyhow!("no NV12 texture"))?;
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut view = None;
            vdev.CreateVideoProcessorOutputView(&tex, &vp_enum, &desc, Some(&mut view))?;
            let registered = enc.register(tex.as_raw(), ow, oh)?;
            let bitstream = enc.bitstream_buffer()?;
            frames.push(NvFrame { tex, view: view.ok_or_else(|| anyhow::anyhow!("no output view"))?, registered, mapped: std::ptr::null_mut(), bitstream });
        }

        let stats = Arc::new(Stats::default());
        let (tx, rx) = crossbeam_channel::bounded::<Job>(SLOTS + 2);
        let worker = Worker {
            enc,
            context: context.clone(),
            context4,
            vctx,
            vp,
            in_views,
            ready11,
            free11,
            frames,
            submitted: 0,
            collected: 0,
            last_frame: None,
            first: true,
            first_pts: None,
            bframes,
            ended: false,
            stats: stats.clone(),
            sink,
            output: (ow, oh),
            _slots11: slots11,
            _device: device,
            _api: api,
        };
        let job_rx = SendBox((worker, rx));
        let thread = std::thread::Builder::new().name("gpu-encoder".into()).spawn(move || {
            // (Bind the box whole, so the closure captures the Send wrapper and not its fields.)
            let boxed = job_rx;
            let SendBox((mut w, rx)) = boxed;
            w.run(rx);
        })?;
        log::info!("GPU encoder: {:?} {}x{} -> {}x{} at {} fps, {:?}, p{}", config.codec, iw, ih, ow, oh, config.fps, config.rate, config.preset);
        Ok(Self { slots: slots12, last_use: [0; SLOTS], ready, free, value: 0, next: 0, sent_pts: None, tx: Some(tx), thread: Some(thread), stats, config })
    }

    /// The shared slot textures, for the renderer to wrap and copy into.
    pub fn slot_resources(&self) -> &[ID3D12Resource] {
        &self.slots
    }

    /// The fence the renderer's queue must signal (with [`Pending::value`]) after its copy.
    pub fn ready_fence(&self) -> &ID3D12Fence {
        &self.ready
    }

    /// False once the encoder thread has stopped (the output ended or encoding failed).
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// The slot to copy this frame into, or `None` when the encoder hasn't finished with any of
    /// them yet (it is behind; the frame is left out and repeated instead).
    pub fn begin(&mut self) -> Option<Pending> {
        if !self.is_alive() {
            return None;
        }
        // SAFETY: reading a fence's completed value is always allowed.
        let done = unsafe { self.free.GetCompletedValue() };
        let slot = self.next;
        if self.last_use[slot] > done {
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.value += 1;
        Some(Pending { slot, value: self.value })
    }

    /// The copy for `p` has been submitted (with the ready fence signalled after it).
    pub fn submitted(&mut self, p: Pending, pts: u64) {
        self.last_use[p.slot] = p.value;
        self.next = (p.slot + 1) % SLOTS;
        self.send(Job::Frame { slot: p.slot, ready: p.value, pts });
    }

    /// Called once per output frame after the program picture was produced: if no new frame was
    /// sent for `pts` (a static scene, or the encoder was behind), the previous one is encoded again,
    /// so the video stays exactly one frame per tick and in step with the audio.
    pub fn tick(&mut self, pts: u64) {
        if self.sent_pts.is_some_and(|s| s < pts) {
            self.send(Job::Repeat { pts });
        }
    }

    fn send(&mut self, job: Job) {
        let pts = match &job {
            Job::Frame { pts, .. } | Job::Repeat { pts } => *pts,
        };
        let Some(tx) = &self.tx else { return };
        match tx.try_send(job) {
            Ok(()) => self.sent_pts = Some(pts),
            // Shouldn't happen (slots gate the frames), but a full queue must not block rendering.
            Err(TrySendError::Full(_)) => {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for GpuEncoder {
    fn drop(&mut self) {
        // Closing the channel lets the thread flush the encoder and end.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// An NVENC session that owns its API handle (unlike [`crate::Session`], which borrows it).
struct Session {
    api: Arc<Api>,
    enc: *mut c_void,
}

impl Session {
    unsafe fn open(api: Arc<Api>, device: *mut c_void) -> anyhow::Result<Self> {
        let mut p: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
        p.version = struct_ver(1);
        p.deviceType = _NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_DIRECTX;
        p.device = device;
        p.apiVersion = NVENCAPI_VERSION;
        let mut enc = std::ptr::null_mut();
        let open = api.f.nvEncOpenEncodeSessionEx.ok_or_else(|| anyhow::anyhow!("NVENC: no open-session entry"))?;
        check("open session", open(&mut p, &mut enc))?;
        Ok(Self { api, enc })
    }

    unsafe fn initialize(self, cfg: &Config, lookahead: u16, bframes: u32) -> anyhow::Result<Self> {
        let f = &self.api.f;
        let codec = cfg.codec.guid();
        let preset = preset_guid(cfg.preset);
        let tuning = NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_HIGH_QUALITY;
        let mut pc: NV_ENC_PRESET_CONFIG = std::mem::zeroed();
        pc.version = struct_ver(5) | (1 << 31);
        pc.presetCfg.version = struct_ver(9) | (1 << 31);
        let get = f.nvEncGetEncodePresetConfigEx.ok_or_else(|| anyhow::anyhow!("no preset query"))?;
        check("preset config", get(self.enc, codec, preset, tuning, &mut pc))?;
        let mut c = pc.presetCfg;
        let gop = cfg.keyframe_interval_frames.max(1);
        c.gopLength = gop;
        // B-frames: frames come out in decode order, so each packet carries its own pts/dts.
        c.frameIntervalP = bframes as i32 + 1;
        let rc = &mut c.rcParams;
        match cfg.rate {
            Rate::Cbr(k) => {
                rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_CBR;
                rc.averageBitRate = k * 1000;
                rc.maxBitRate = k * 1000;
                rc.vbvBufferSize = k * 1000;
                rc.vbvInitialDelay = k * 1000;
            }
            Rate::Vbr(k) => {
                rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_VBR;
                rc.averageBitRate = k * 1000;
                rc.maxBitRate = k * 1500;
                rc.vbvBufferSize = k * 2000;
            }
            Rate::Quality(q) => {
                rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_VBR;
                rc.averageBitRate = 0;
                rc.maxBitRate = 0;
                rc.targetQuality = q.clamp(1, 51);
            }
            Rate::Cqp(q) => {
                rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_CONSTQP;
                let q = u32::from(q.min(51));
                rc.constQP = NV_ENC_QP { qpInterP: q, qpInterB: q, qpIntra: q };
            }
        }
        rc.set_enableAQ(1);
        if lookahead > 0 {
            rc.set_enableLookahead(1);
            rc.lookaheadDepth = lookahead;
            rc.set_enableTemporalAQ(1);
        }
        rc.multiPass = if cfg.multipass { _NV_ENC_MULTI_PASS_NV_ENC_TWO_PASS_QUARTER_RESOLUTION } else { _NV_ENC_MULTI_PASS_NV_ENC_MULTI_PASS_DISABLED };
        // BT.709, limited range, signalled in the stream so players show the right colours.
        match cfg.codec {
            Codec::H264 => {
                let h = &mut c.encodeCodecConfig.h264Config;
                h.idrPeriod = gop;
                h.set_repeatSPSPPS(1);
                vui709(&mut h.h264VUIParameters);
            }
            Codec::Hevc => {
                let h = &mut c.encodeCodecConfig.hevcConfig;
                h.idrPeriod = gop;
                h.set_repeatSPSPPS(1);
                vui709(&mut h.hevcVUIParameters);
            }
            Codec::Av1 => {
                let a = &mut c.encodeCodecConfig.av1Config;
                a.idrPeriod = gop;
                a.set_repeatSeqHdr(1);
                a.colorPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                a.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                a.matrixCoefficients = _NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;
                a.colorRange = 0;
            }
        }
        let (w, h) = cfg.output;
        let mut init: NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
        init.version = struct_ver(7) | (1 << 31);
        init.encodeGUID = codec;
        init.presetGUID = preset;
        init.encodeWidth = w;
        init.encodeHeight = h;
        init.darWidth = w;
        init.darHeight = h;
        init.maxEncodeWidth = w;
        init.maxEncodeHeight = h;
        init.frameRateNum = cfg.fps.max(1);
        init.frameRateDen = 1;
        init.enableEncodeAsync = 0;
        init.enablePTD = 1;
        init.encodeConfig = &mut c;
        init.tuningInfo = tuning;
        let initialize = f.nvEncInitializeEncoder.ok_or_else(|| anyhow::anyhow!("no initialise entry"))?;
        check("initialise", initialize(self.enc, &mut init)).map_err(|e| anyhow::anyhow!("{e}: {}", self.last_error()))?;
        Ok(self)
    }

    fn last_error(&self) -> String {
        let Some(f) = self.api.f.nvEncGetLastErrorString else { return String::new() };
        // SAFETY: returns a NUL-terminated string owned by the session (or null).
        unsafe {
            let p = f(self.enc);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    unsafe fn register(&self, texture: *mut c_void, w: u32, h: u32) -> anyhow::Result<*mut c_void> {
        let mut r: NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
        r.version = struct_ver(5);
        r.resourceType = _NV_ENC_INPUT_RESOURCE_TYPE_NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        r.width = w;
        r.height = h;
        r.resourceToRegister = texture;
        r.bufferFormat = _NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_NV12;
        r.bufferUsage = _NV_ENC_BUFFER_USAGE_NV_ENC_INPUT_IMAGE;
        let f = self.api.f.nvEncRegisterResource.ok_or_else(|| anyhow::anyhow!("no register entry"))?;
        check("register texture", f(self.enc, &mut r))?;
        Ok(r.registeredResource)
    }

    unsafe fn bitstream_buffer(&self) -> anyhow::Result<*mut c_void> {
        let mut b: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
        b.version = struct_ver(1);
        let f = self.api.f.nvEncCreateBitstreamBuffer.ok_or_else(|| anyhow::anyhow!("no bitstream entry"))?;
        check("bitstream buffer", f(self.enc, &mut b))?;
        Ok(b.bitstreamBuffer)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(destroy) = self.api.f.nvEncDestroyEncoder {
            // SAFETY: destroyed once, after every resource of the session was released.
            unsafe { destroy(self.enc) };
        }
    }
}

fn vui709(v: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS) {
    v.videoSignalTypePresentFlag = 1;
    v.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT_NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    v.videoFullRangeFlag = 0;
    v.colourDescriptionPresentFlag = 1;
    v.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    v.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    v.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;
}

fn preset_guid(p: u8) -> GUID {
    let g = |d1: u32, d2: u16, d3: u16, d4: [u8; 8]| GUID { Data1: d1 as _, Data2: d2, Data3: d3, Data4: d4 };
    match p {
        1 => g(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]),
        2 => g(0xf581cfb8, 0x88d6, 0x4381, [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab]),
        3 => g(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]),
        4 => g(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]),
        6 => g(0x8e75c279, 0x6299, 0x4ab6, [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5]),
        7 => g(0x84848c12, 0x6f71, 0x4c13, [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74]),
        _ => g(0x21c6e6b4, 0x297a, 0x4cba, [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3]), // p5
    }
}

/// One NVENC input (NV12 texture) with the bitstream buffer it encodes into.
struct NvFrame {
    tex: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
    registered: *mut c_void,
    mapped: *mut c_void,
    bitstream: *mut c_void,
}

/// The encoder thread's half.
struct Worker {
    enc: Session,
    context: ID3D11DeviceContext,
    context4: ID3D11DeviceContext4,
    vctx: ID3D11VideoContext1,
    vp: ID3D11VideoProcessor,
    in_views: Vec<ID3D11VideoProcessorInputView>,
    ready11: ID3D11Fence,
    free11: ID3D11Fence,
    frames: Vec<NvFrame>,
    /// Pictures handed to NVENC, and pictures whose output has been collected (pool indices are
    /// these counts modulo the pool size).
    submitted: u64,
    collected: u64,
    /// Pool index holding the latest picture, for repeats.
    last_frame: Option<usize>,
    first: bool,
    /// Input timestamp of the first picture: packet timestamps count from it.
    first_pts: Option<u64>,
    bframes: u32,
    /// The sink refused a packet: the output has stopped (a normal end, not an error).
    ended: bool,
    stats: Arc<Stats>,
    sink: Sink,
    output: (u32, u32),
    _slots11: Vec<ID3D11Texture2D>,
    _device: ID3D11Device,
    _api: Arc<Api>,
}

impl Worker {
    fn run(&mut self, rx: Receiver<Job>) {
        let mut next_pts: Option<u64> = None;
        for job in rx {
            let pts = match job {
                Job::Frame { pts, .. } | Job::Repeat { pts } => pts,
            };
            // Ticks the render thread couldn't send (it was behind): repeat, so timing stays exact.
            if let Some(n) = next_pts {
                for missing in n..pts {
                    if let Err(e) = self.repeat(missing) {
                        return self.stats.fail(e);
                    }
                }
            }
            let r = match job {
                Job::Frame { slot, ready, pts } => self.frame(slot, ready, pts),
                Job::Repeat { pts } => self.repeat(pts),
            };
            if let Err(e) = r {
                return self.stats.fail(e);
            }
            next_pts = Some(pts + 1);
            if self.ended {
                log::info!("GPU encoder: output finished");
                return;
            }
            if self.stats.failed.load(Ordering::Relaxed) {
                return;
            }
        }
        // Channel closed: the output is stopping. Flush what NVENC still holds.
        if let Err(e) = self.flush() {
            log::warn!("GPU encoder flush: {e}");
        }
    }

    fn frame(&mut self, slot: usize, ready: u64, pts: u64) -> anyhow::Result<()> {
        let i = (self.submitted % self.frames.len() as u64) as usize;
        self.make_room()?;
        // SAFETY: all objects belong to this device; the stream struct's views are released below.
        unsafe {
            self.context4.Wait(&self.ready11, ready)?;
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.in_views[slot].clone())),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.vp, &self.frames[i].view, 0, &streams);
            let [mut stream] = streams;
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r?;
            // The slot may be overwritten once the conversion above has run.
            self.context4.Signal(&self.free11, ready)?;
            self.context.Flush();
        }
        self.encode(i, pts)?;
        self.last_frame = Some(i);
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn repeat(&mut self, pts: u64) -> anyhow::Result<()> {
        let Some(prev) = self.last_frame else { return Ok(()) };
        let i = (self.submitted % self.frames.len() as u64) as usize;
        self.make_room()?;
        if i != prev {
            // SAFETY: two NV12 textures of the same size on this device.
            unsafe { self.context.CopyResource(&self.frames[i].tex, &self.frames[prev].tex) };
        }
        self.encode(i, pts)?;
        self.last_frame = Some(i);
        self.stats.repeats.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Every pool entry may be waiting for output; collect the oldest before reusing it.
    fn make_room(&mut self) -> anyhow::Result<()> {
        while self.submitted - self.collected >= self.frames.len() as u64 {
            self.collect_one()?;
        }
        Ok(())
    }

    fn encode(&mut self, i: usize, pts: u64) -> anyhow::Result<()> {
        let f = &self.enc.api.f;
        let (map, encode) = (f.nvEncMapInputResource.ok_or_else(|| anyhow::anyhow!("no map entry"))?, f.nvEncEncodePicture.ok_or_else(|| anyhow::anyhow!("no encode entry"))?);
        // SAFETY: the texture was registered with this session; the pool entry is free (collected).
        unsafe {
            let mut m: NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            m.version = struct_ver(4);
            m.registeredResource = self.frames[i].registered;
            check("map input", map(self.enc.enc, &mut m))?;
            self.frames[i].mapped = m.mappedResource;
            let mut p: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            p.version = struct_ver(7) | (1 << 31);
            p.inputWidth = self.output.0;
            p.inputHeight = self.output.1;
            p.inputPitch = self.output.0;
            p.inputBuffer = m.mappedResource;
            p.outputBitstream = self.frames[i].bitstream;
            p.bufferFmt = m.mappedBufferFmt;
            p.pictureStruct = _NV_ENC_PIC_STRUCT_NV_ENC_PIC_STRUCT_FRAME;
            p.inputTimeStamp = pts;
            p.frameIdx = pts as u32;
            if self.first {
                p.encodePicFlags = _NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_FORCEIDR;
                self.first = false;
            }
            let s = encode(self.enc.enc, &mut p);
            if s != _NVENCSTATUS_NV_ENC_SUCCESS && s != _NVENCSTATUS_NV_ENC_ERR_NEED_MORE_INPUT {
                check("encode", s).map_err(|e| anyhow::anyhow!("{e}: {}", self.enc.last_error()))?;
            }
        }
        self.submitted += 1;
        // With look-ahead NVENC holds pictures back; collect only what can no longer be pending
        // (the pool keeps one entry free for the next picture).
        while self.submitted - self.collected > (self.frames.len() as u64).saturating_sub(1) {
            self.collect_one()?;
        }
        Ok(())
    }

    /// Waits for the oldest submitted picture's output, hands it to the sink and frees its entry.
    fn collect_one(&mut self) -> anyhow::Result<()> {
        if self.collected >= self.submitted {
            return Ok(());
        }
        let i = (self.collected % self.frames.len() as u64) as usize;
        let f = &self.enc.api.f;
        let (lock, unlock, unmap) = (
            f.nvEncLockBitstream.ok_or_else(|| anyhow::anyhow!("no lock entry"))?,
            f.nvEncUnlockBitstream.ok_or_else(|| anyhow::anyhow!("no unlock entry"))?,
            f.nvEncUnmapInputResource.ok_or_else(|| anyhow::anyhow!("no unmap entry"))?,
        );
        // SAFETY: the buffer belongs to a picture submitted to this session; locking waits for it.
        let keep_going = unsafe {
            let mut l: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            l.version = struct_ver(2) | (1 << 31);
            l.outputBitstream = self.frames[i].bitstream;
            check("lock bitstream", lock(self.enc.enc, &mut l))?;
            let data = std::slice::from_raw_parts(l.bitstreamBufferPtr as *const u8, l.bitstreamSizeInBytes as usize);
            self.stats.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            let base = *self.first_pts.get_or_insert(l.outputTimeStamp);
            let pts = l.outputTimeStamp.saturating_sub(base);
            // Decode order count, held back by the reordering delay so dts never passes pts.
            let dts = self.collected as i64 - i64::from(self.bframes);
            let keyframe = l.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_IDR || l.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_I;
            let ok = data.is_empty() || (self.sink)(crate::Packet { data, pts, dts, keyframe });
            check("unlock bitstream", unlock(self.enc.enc, self.frames[i].bitstream))?;
            if !self.frames[i].mapped.is_null() {
                check("unmap input", unmap(self.enc.enc, self.frames[i].mapped))?;
                self.frames[i].mapped = std::ptr::null_mut();
            }
            ok
        };
        self.collected += 1;
        if !keep_going {
            self.ended = true;
        }
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        let f = &self.enc.api.f;
        if let Some(encode) = f.nvEncEncodePicture {
            // SAFETY: an end-of-stream picture with no input tells NVENC to finish everything pending.
            unsafe {
                let mut p: NV_ENC_PIC_PARAMS = std::mem::zeroed();
                p.version = struct_ver(7) | (1 << 31);
                p.encodePicFlags = _NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_EOS;
                let _ = encode(self.enc.enc, &mut p);
            }
        }
        while self.collected < self.submitted && !self.ended {
            self.collect_one()?;
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let f = &self.enc.api.f;
        // SAFETY: releases this session's resources before the session itself (dropped after).
        unsafe {
            for fr in &self.frames {
                if !fr.mapped.is_null() {
                    if let Some(u) = f.nvEncUnmapInputResource {
                        u(self.enc.enc, fr.mapped);
                    }
                }
                if let Some(u) = f.nvEncUnregisterResource {
                    u(self.enc.enc, fr.registered);
                }
                if let Some(d) = f.nvEncDestroyBitstreamBuffer {
                    d(self.enc.enc, fr.bitstream);
                }
            }
        }
    }
}
