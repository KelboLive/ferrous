//! GPU compositor (wgpu: Direct3D 12 / Vulkan on Windows).
//!
//! Produces the same image as the CPU [`crate::Compositor`] (bilinear
//! sampling, with area averaging when a layer is shrunk, straight-alpha "over" blending in gamma space) so either can be
//! used interchangeably. Source textures are cached by frame identity, so
//! static sources (images, colors) are uploaded once, not every frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ferrous_core::VideoFrame;

use crate::gpufx::Fx;
use crate::Layer;

const SHADER: &str = r#"
struct Params {
    center: vec4<f32>,   // centre x, y (canvas px), half width, half height
    uv: vec4<f32>,       // u0, v0, u1, v1
    opacity: vec4<f32>,  // x = opacity, y = cos, z = sin, w = 1 if rotated (soft edges)
    canvas: vec4<f32>,   // canvas width, height
};
@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) local: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let fx = f32(i & 1u);
    let fy = f32((i >> 1u) & 1u);
    // Rotated quads grow by a pixel so their edges can be antialiased.
    let pad = p.opacity.w;
    let half = p.center.zw + vec2<f32>(pad, pad);
    let local = vec2<f32>(mix(-half.x, half.x, fx), mix(-half.y, half.y, fy));
    let c = p.opacity.y;
    let s = p.opacity.z;
    let px = p.center.xy + vec2<f32>(local.x * c - local.y * s, local.x * s + local.y * c);
    var o: VsOut;
    o.pos = vec4<f32>(px.x / p.canvas.x * 2.0 - 1.0, 1.0 - px.y / p.canvas.y * 2.0, 0.0, 1.0);
    // UV extrapolated over the padding (the sampler clamps).
    let t = (local + p.center.zw) / (2.0 * p.center.zw);
    o.uv = vec2<f32>(mix(p.uv.x, p.uv.z, t.x), mix(p.uv.y, p.uv.w, t.y));
    o.local = local;
    return o;
}

// Source texels covered by one output pixel. Above ~1.25 a single bilinear tap skips texels (thin
// lines vanish or shimmer), so shrunk layers average an n x n grid of taps over the pixel's footprint
// (premultiplied, so transparent edges don't bleed). 1:1 and upscaled layers keep the single tap.
fn shrink_sample(uv: vec2<f32>, dx: vec2<f32>, dy: vec2<f32>) -> vec4<f32> {
    let dims = vec2<f32>(textureDimensions(tex));
    let r = max(length(dx * dims), length(dy * dims));
    if (r <= 1.25) {
        return textureSampleLevel(tex, samp, uv, 0.0);
    }
    let n = i32(min(ceil(r), 8.0));
    let inv = 1.0 / f32(n);
    var acc = vec4<f32>(0.0);
    for (var j = 0; j < n; j = j + 1) {
        for (var k = 0; k < n; k = k + 1) {
            let o = (vec2<f32>(f32(k), f32(j)) + 0.5) * inv - 0.5;
            let s = textureSampleLevel(tex, samp, uv + dx * o.x + dy * o.y, 0.0);
            acc = acc + vec4<f32>(s.rgb * s.a, s.a);
        }
    }
    acc = acc * (inv * inv);
    if (acc.a <= 0.0) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(acc.rgb / acc.a, acc.a);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4<f32> {
    // Derivatives must be taken in uniform control flow, before any branch.
    let dx = dpdx(i.uv);
    let dy = dpdy(i.uv);
    let c = shrink_sample(i.uv, dx, dy);
    var cover = 1.0;
    if (p.opacity.w > 0.0) {
        let d = p.center.zw - abs(i.local);
        cover = clamp(min(d.x, d.y) + 0.5, 0.0, 1.0);
    }
    return vec4<f32>(c.rgb, c.a * p.opacity.x * cover);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    center: [f32; 4],
    uv: [f32; 4],
    opacity: [f32; 4],
    canvas: [f32; 4],
}

fn params_buffer(device: &wgpu::Device, stride: u64, slots: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("params"),
        size: stride * slots as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

struct CachedTexture {
    /// Keeps the frame alive so its pointer can't be reused by a new frame.
    _frame: Arc<Vec<u8>>,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
}

/// Source textures no longer on screen, kept for reuse: a live source
/// brings a new frame every tick, and uploading into an existing texture
/// is much cheaper than creating (and destroying) one per frame.
const SPARE_TEXTURES: usize = 8;

static PREFER_DX12: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks for the DX12 backend when a GPU compositor is created next (Windows; GPU-resident capture
/// needs it).
pub fn prefer_dx12(on: bool) {
    PREFER_DX12.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Cache keys at or above this are shared GPU textures (the rest are frame buffer addresses).
const SHARED_KEY: usize = 1 << (usize::BITS - 1);

/// A texture captured on the GPU by another device, opened here.
struct SharedTex {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    last_used: u64,
}

pub struct GpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Per-layer parameters, one aligned slot each, rewritten every frame (grows on demand).
    ubo: wgpu::Buffer,
    ubo_slots: usize,
    ubo_stride: u64,
    target: wgpu::Texture,
    /// Two readback slots: while the CPU reads one, the GPU fills the other.
    slots: Vec<Slot>,
    /// Slot holding a submitted frame that hasn't been read yet.
    pending: Option<usize>,
    padded_row: u32,
    nv12: Option<Nv12Pass>,
    width: u32,
    height: u32,
    cache: HashMap<usize, CachedTexture>,
    /// Shared textures opened so far, by ring slot id; dropped when unused for a while.
    shared: HashMap<u64, SharedTex>,
    /// Shared frames drawn by the last two compositions: their ring slots stay off limits to the
    /// capture until the GPU is done reading them.
    held: [Vec<Arc<ferrous_core::SharedFrame>>; 2],
    frames: u64,
    /// False once a shared texture could not be opened (wrong adapter, not DX12): the engine then
    /// uses CPU frames for good.
    shared_ok: bool,
    spare: Vec<((u32, u32), wgpu::Texture, wgpu::TextureView)>,
    /// Video filter passes (chroma key, colour, scroll, LUT, background mask).
    /// Created the first time a layer has filters, so a driver problem with these shaders can't stop startup.
    fx: Option<Fx>,
    adapter_name: String,
    software: bool,
    /// Timestamp queries around the compositing pass, when supported.
    timing: Option<(wgpu::QuerySet, wgpu::Buffer, f32)>,
    last_gpu_ms: Option<f32>,
}

impl GpuCompositor {
    /// True for CPU rasterisers (llvmpipe, WARP), which are slower than our
    /// own CPU compositor.
    pub fn is_software(&self) -> bool {
        self.software
    }

    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        // GPU-resident capture shares textures through DX12, so ask for that first and take whatever
        // else is there if it isn't (the engine then uses CPU frames).
        if cfg!(windows) && PREFER_DX12.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(g) = Self::new_on(width, height, Some(wgpu::Backends::DX12)) {
                return Ok(g);
            }
        }
        Self::new_on(width, height, None)
    }

    fn new_on(width: u32, height: u32, backends: Option<wgpu::Backends>) -> anyhow::Result<Self> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        if let Some(b) = backends {
            desc.backends = b;
        } else if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
            // Never OpenGL: finding its adapter makes the driver open a hidden window and GL context,
            // which on some machines crashed the process while the main window was starting.
            desc.backends = wgpu::Backends::VULKAN | wgpu::Backends::DX12;
        }
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("no GPU adapter: {e}"))?;
        let software = adapter.get_info().device_type == wgpu::DeviceType::Cpu;
        let adapter_is_dx12 = adapter.get_info().backend == wgpu::Backend::Dx12;
        let adapter_name = format!("{} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ferrous-compositor"),
            required_features: if timestamps { wgpu::Features::TIMESTAMP_QUERY } else { wgpu::Features::empty() },
            ..Default::default()
        }))?;
        let timing = timestamps.then(|| {
            let qs = device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("pass time"), ty: wgpu::QueryType::Timestamp, count: 2 });
            let resolve = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ts resolve"),
                size: 16,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            (qs, resolve, queue.get_timestamp_period())
        });

        log::info!("GPU compositor: device ready, building pipeline");
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("layer"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("composite"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        // The canvas stays opaque, like the CPU path.
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        log::info!("GPU compositor: pipeline built");
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("canvas"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let slots = (0..2)
            .map(|_| Slot {
                bgra: map_buffer(&device, "readback", (padded_row * height) as u64),
                ts: timing.is_some().then(|| map_buffer(&device, "ts read", 16)),
                planes: None,
                done: Arc::new(AtomicUsize::new(0)),
                expect: 0,
                has_nv12: false,
                sub: None,
            })
            .collect();
        log::info!("GPU compositor on {adapter_name}");
        let align = u64::from(device.limits().min_uniform_buffer_offset_alignment).max(1);
        let ubo_stride = (std::mem::size_of::<Params>() as u64).div_ceil(align) * align;
        let ubo = params_buffer(&device, ubo_stride, 8);
        Ok(Self {
            device,
            queue,
            pipeline,
            layout,
            sampler,
            ubo,
            ubo_slots: 8,
            ubo_stride,
            target,
            slots,
            pending: None,
            padded_row,
            nv12: None,
            width,
            height,
            cache: HashMap::new(),
            shared: HashMap::new(),
            held: [Vec::new(), Vec::new()],
            frames: 0,
            shared_ok: cfg!(windows) && adapter_is_dx12,
            spare: Vec::new(),
            fx: None,
            adapter_name,
            software,
            timing,
            last_gpu_ms: None,
        })
    }

    /// GPU time of the last compositing pass, if the adapter supports timestamps.
    pub fn last_gpu_ms(&self) -> Option<f32> {
        self.last_gpu_ms
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn cached_textures(&self) -> usize {
        self.cache.len()
    }

    /// Whether shared GPU frames may still be offered to this compositor.
    pub fn shared_ok(&self) -> bool {
        self.shared_ok
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Whether `frame`, a shared GPU texture, can be drawn here. Opens it on first sight; a failure
    /// (a different GPU, a backend other than DX12) turns shared frames off for good.
    pub fn accepts_shared(&mut self, frame: &VideoFrame) -> bool {
        let Some(shared) = frame.gpu.as_ref() else { return true };
        if !self.shared_ok {
            return false;
        }
        if self.shared.contains_key(&shared.id) {
            return true;
        }
        match self.open_shared(shared, (frame.width, frame.height)) {
            Some(t) => {
                self.shared.insert(shared.id, t);
                true
            }
            None => {
                log::warn!("GPU-resident capture unavailable on this adapter; using CPU frames");
                self.shared_ok = false;
                false
            }
        }
    }

    #[cfg(windows)]
    fn open_shared(&self, shared: &ferrous_core::SharedFrame, dims: (u32, u32)) -> Option<SharedTex> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
        // SAFETY: the handle came from `CreateSharedHandle` on a texture of this size and format, and
        // stays valid while the frame's guard lives; the imported resource is a new reference to it.
        unsafe {
            let hal = self.device.as_hal::<wgpu::hal::api::Dx12>()?;
            let raw = hal.raw_device();
            let luid = raw.GetAdapterLuid();
            if (i64::from(luid.HighPart) << 32 | i64::from(luid.LowPart)) != shared.luid {
                return None;
            }
            let mut resource: Option<ID3D12Resource> = None;
            raw.OpenSharedHandle(HANDLE(shared.handle as *mut _), &mut resource).ok()?;
            let resource = resource?;
            let size = wgpu::Extent3d { width: dims.0, height: dims.1, depth_or_array_layers: 1 };
            let hal_tex = wgpu::hal::dx12::Device::texture_from_raw(resource, wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureDimension::D2, size, 1, 1);
            drop(hal);
            let desc = wgpu::TextureDescriptor {
                label: Some("shared source"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            };
            let texture = self.device.create_texture_from_hal::<wgpu::hal::api::Dx12>(hal_tex, &desc, wgpu::TextureUses::RESOURCE);
            let view = texture.create_view(&Default::default());
            Some(SharedTex { _texture: texture, view, last_used: self.frames })
        }
    }

    #[cfg(not(windows))]
    fn open_shared(&self, _shared: &ferrous_core::SharedFrame, _dims: (u32, u32)) -> Option<SharedTex> {
        None
    }

    fn view_for(&self, key: usize) -> wgpu::TextureView {
        if key >= SHARED_KEY {
            self.shared[&((key - SHARED_KEY) as u64)].view.clone()
        } else {
            self.cache[&key].view.clone()
        }
    }

    fn texture_for(&mut self, frame: &VideoFrame) -> usize {
        if let Some(shared) = frame.gpu.as_ref() {
            if !self.accepts_shared(frame) && !self.shared.contains_key(&shared.id) {
                // Can't happen when the engine asked first; draw nothing rather than fail.
                let size = wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 };
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("missing shared source"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                if self.shared.is_empty() {
                    log::info!("GPU capture active: drawing captured frames straight from the GPU ({}x{})", frame.width, frame.height);
                }
                self.shared.insert(shared.id, SharedTex { _texture: texture, view, last_used: self.frames });
            }
            if let Some(t) = self.shared.get_mut(&shared.id) {
                t.last_used = self.frames;
            }
            self.held[0].push(shared.clone());
            return SHARED_KEY + shared.id as usize;
        }
        let key = Arc::as_ptr(&frame.data) as usize;
        if self.cache.get(&key).is_some_and(|c| c.size == (frame.width, frame.height)) {
            return key;
        }
        let size = wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 };
        let dims = (frame.width, frame.height);
        let (tex, view) = match self.spare.iter().position(|(s, ..)| *s == dims) {
            Some(i) => {
                let (_, t, v) = self.spare.swap_remove(i);
                (t, v)
            }
            None => {
                let t = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("source"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let v = t.create_view(&Default::default());
                (t, v)
            }
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &frame.data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frame.width * 4), rows_per_image: Some(frame.height) },
            size,
        );
        self.cache.insert(key, CachedTexture { _frame: frame.data.clone(), texture: tex, view, size: dims });
        key
    }

    pub fn compose(&mut self, layers: &[Layer<'_>]) -> VideoFrame {
        self.compose_into(layers, Vec::new(), false)
    }

    /// Composes `layers` and waits for the result (one GPU round trip).
    /// Reads back into `buf`'s allocation (its contents are replaced); with
    /// `nv12`, the frame also carries the GPU-converted NV12 picture.
    pub fn compose_into(&mut self, layers: &[Layer<'_>], buf: Vec<u8>, nv12: bool) -> VideoFrame {
        self.discard_pending();
        let groups = self.prepare(layers);
        self.record(&groups, 0, nv12);
        self.collect(0, buf)
    }

    /// Pipelined: submits this frame and returns the one submitted last
    /// time, which the GPU has had a whole frame to finish, so the CPU
    /// doesn't wait on it (OBS does the same with its staging surfaces).
    /// The price is one frame of latency. The flag says the returned frame
    /// is of `layers` itself, which is the case only on the first call.
    pub fn compose_pipelined(&mut self, layers: &[Layer<'_>], buf: Vec<u8>, nv12: bool) -> (VideoFrame, bool) {
        let groups = self.prepare(layers);
        match self.pending.take() {
            Some(prev) => {
                let cur = 1 - prev;
                self.record(&groups, cur, nv12);
                let frame = self.collect(prev, buf);
                self.pending = Some(cur);
                (frame, false)
            }
            None => {
                // Prime the pipeline: this frame now, plus one in flight.
                self.record(&groups, 0, nv12);
                self.record(&groups, 1, nv12);
                let frame = self.collect(0, buf);
                self.pending = Some(1);
                (frame, true)
            }
        }
    }

    /// Drops a frame still in flight (the caller stopped pipelining).
    pub fn discard_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            self.drain(p);
        }
    }

    /// Uploads sources and builds one bind group per drawn layer.
    fn prepare(&mut self, layers: &[Layer<'_>]) -> Vec<wgpu::BindGroup> {
        self.frames += 1;
        self.held.swap(0, 1);
        self.held[0].clear();
        let frames = self.frames;
        self.shared.retain(|_, t| frames - t.last_used < 240);
        let keys: Vec<usize> = layers.iter().map(|l| self.texture_for(l.frame)).collect();
        // Frames no longer on screen: their textures go to the spare list.
        let gone: Vec<usize> = self.cache.keys().filter(|k| !keys.contains(k)).copied().collect();
        for k in gone {
            if let Some(c) = self.cache.remove(&k) {
                if self.spare.len() < SPARE_TEXTURES {
                    self.spare.push((c.size, c.texture, c.view));
                }
            }
        }

        // Video filters run as passes over each filtered layer's texture, before the composite pass
        // (submitted first, so the queue keeps them in order).
        let live_luts: Vec<usize> = layers
            .iter()
            .flat_map(|l| l.filters.iter())
            .filter_map(|f| match f {
                crate::filters::GpuFilter::Lut { lut, .. } => Some(Arc::as_ptr(lut) as usize),
                _ => None,
            })
            .collect();
        if self.fx.is_none() && layers.iter().any(|l| !l.filters.is_empty()) {
            log::info!("GPU filters: compiling shaders");
            self.fx = Some(Fx::new(&self.device, &self.queue));
            log::info!("GPU filters: ready");
        }
        if let Some(fx) = self.fx.as_mut() {
            fx.begin_frame(&live_luts);
        }
        let mut fx_encoder = None;
        let views: Vec<wgpu::TextureView> = layers
            .iter()
            .zip(&keys)
            .map(|(l, key)| {
                let src = self.view_for(*key);
                if l.filters.is_empty() {
                    return src;
                }
                let enc = fx_encoder.get_or_insert_with(|| self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fx") }));
                match self.fx.as_mut() {
                    Some(fx) => fx.run(&self.device, &self.queue, enc, &src, (l.frame.width, l.frame.height), l.filters),
                    None => src,
                }
            })
            .collect();
        if let Some(enc) = fx_encoder {
            self.queue.submit([enc.finish()]);
        }

        let (cw, ch) = (self.width as f32, self.height as f32);
        if layers.len() > self.ubo_slots {
            self.ubo_slots = layers.len().next_power_of_two();
            self.ubo = params_buffer(&self.device, self.ubo_stride, self.ubo_slots);
        }
        let stride = self.ubo_stride as usize;
        let mut staged: Vec<u8> = Vec::new();
        let mut bind_groups = Vec::new();
        for (layer, view) in layers.iter().zip(&views) {
            let t = &layer.transform;
            let f = layer.frame;
            let [cl, ct, cr, cb] = t.crop;
            let src_w = f.width.saturating_sub(cl + cr);
            let src_h = f.height.saturating_sub(ct + cb);
            if src_w == 0 || src_h == 0 || t.scale_x <= 0.0 || t.scale_y <= 0.0 || t.opacity <= 0.0 {
                continue;
            }
            let (hw, hh) = (src_w as f32 * t.scale_x / 2.0, src_h as f32 * t.scale_y / 2.0);
            let rotated = t.is_rotated();
            let (c, s) = if rotated { t.rotation_cs() } else { (1.0, 0.0) };
            let params = Params {
                center: [t.x + hw, t.y + hh, hw, hh],
                uv: [
                    cl as f32 / f.width as f32,
                    ct as f32 / f.height as f32,
                    (cl + src_w) as f32 / f.width as f32,
                    (ct + src_h) as f32 / f.height as f32,
                ],
                opacity: [t.opacity.clamp(0.0, 1.0), c, s, if rotated { 1.0 } else { 0.0 }],
                canvas: [cw, ch, 0.0, 0.0],
            };
            staged.resize((bind_groups.len() + 1) * stride, 0);
            let at = bind_groups.len() * stride;
            staged[at..at + std::mem::size_of::<Params>()].copy_from_slice(bytemuck::bytes_of(&params));
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("layer"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &self.ubo,
                            offset: 0,
                            size: std::num::NonZeroU64::new(std::mem::size_of::<Params>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            bind_groups.push(bg);
        }
        if !staged.is_empty() {
            self.queue.write_buffer(&self.ubo, 0, &staged);
        }

        bind_groups
    }

    /// Whether the NV12 passes are ready (the canvas needs even sides).
    fn ensure_nv12(&mut self) -> bool {
        if self.nv12.is_some() {
            return true;
        }
        let (w, h) = (self.width, self.height);
        if !w.is_multiple_of(2) || !h.is_multiple_of(2) || w < 2 || h < 2 {
            return false;
        }
        let pass = Nv12Pass::new(&self.device, &self.target, w, h);
        for s in &mut self.slots {
            s.planes = Some((
                map_buffer(&self.device, "nv12 y", (pass.y_row * h) as u64),
                map_buffer(&self.device, "nv12 uv", (pass.uv_row * (h / 2)) as u64),
            ));
        }
        self.nv12 = Some(pass);
        true
    }

    /// Draws the layers into the canvas and queues the copies into `slot`.
    fn record(&mut self, groups: &[wgpu::BindGroup], slot: usize, want_nv12: bool) {
        let nv12 = want_nv12 && self.ensure_nv12();
        let view = self.target.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                timestamp_writes: self.timing.as_ref().map(|(qs, ..)| wgpu::RenderPassTimestampWrites {
                    query_set: qs,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            for (i, bg) in groups.iter().enumerate() {
                pass.set_bind_group(0, bg, &[(i as u64 * self.ubo_stride) as u32]);
                pass.draw(0..4, 0..1);
            }
        }
        let (w, h) = (self.width, self.height);
        let s = &self.slots[slot];
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &s.bgra,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.padded_row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        if nv12 {
            if let (Some(n), Some((yb, uvb))) = (&self.nv12, &s.planes) {
                n.encode(&mut enc, yb, uvb, w, h);
            }
        }
        if let (Some((qs, resolve, _)), Some(ts)) = (&self.timing, &s.ts) {
            enc.resolve_query_set(qs, 0..2, resolve, 0);
            enc.copy_buffer_to_buffer(resolve, 0, ts, 0, 16);
        }
        let sub = self.queue.submit([enc.finish()]);

        let s = &mut self.slots[slot];
        s.done.store(0, Ordering::SeqCst);
        let mut expect = 0;
        let mut map = |b: &wgpu::Buffer| {
            let done = s.done.clone();
            b.slice(..).map_async(wgpu::MapMode::Read, move |_| {
                done.fetch_add(1, Ordering::SeqCst);
            });
            expect += 1;
        };
        map(&s.bgra);
        if let Some(ts) = &s.ts {
            map(ts);
        }
        if nv12 {
            if let Some((yb, uvb)) = &s.planes {
                map(yb);
                map(uvb);
            }
        }
        s.expect = expect;
        s.has_nv12 = nv12;
        s.sub = Some(sub);
    }

    /// Waits until `slot`'s submission and its mappings have finished.
    fn drain(&mut self, slot: usize) {
        let s = &mut self.slots[slot];
        if let Some(sub) = s.sub.take() {
            let _ = self.device.poll(wgpu::PollType::Wait { submission_index: Some(sub), timeout: None });
            while s.done.load(Ordering::SeqCst) < s.expect {
                let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
            }
            self.unmap_slot(slot);
        }
    }

    fn unmap_slot(&self, slot: usize) {
        let s = &self.slots[slot];
        s.bgra.unmap();
        if let Some(t) = &s.ts {
            t.unmap();
        }
        if let Some((y, uv)) = &s.planes {
            if s.has_nv12 {
                y.unmap();
                uv.unmap();
            }
        }
    }

    /// Reads a finished slot back into frames.
    fn collect(&mut self, slot: usize, buf: Vec<u8>) -> VideoFrame {
        // Wait (a no-op when the GPU is already done) but keep the mappings.
        {
            let s = &mut self.slots[slot];
            if let Some(sub) = s.sub.take() {
                let _ = self.device.poll(wgpu::PollType::Wait { submission_index: Some(sub), timeout: None });
                while s.done.load(Ordering::SeqCst) < s.expect {
                    let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
                }
            }
        }
        let s = &self.slots[slot];
        let row = (self.width * 4) as usize;
        let mut out = buf;
        out.clear();
        out.reserve(row * self.height as usize);
        {
            let Ok(data) = s.bgra.slice(..).get_mapped_range() else {
                self.unmap_slot(slot);
                return VideoFrame::new(self.width, self.height, vec![0u8; row * self.height as usize]);
            };
            // Appending rows avoids zero-filling the whole frame first.
            for y in 0..self.height as usize {
                out.extend_from_slice(&data[y * self.padded_row as usize..][..row]);
            }
        }
        let mut frame = VideoFrame::new(self.width, self.height, out);
        if let (true, Some(n), Some((yb, uvb))) = (s.has_nv12, &self.nv12, &s.planes) {
            let (w, h) = (self.width as usize, self.height as usize);
            let mut v = Vec::with_capacity(w * h * 3 / 2);
            if let (Ok(y), Ok(uv)) = (yb.slice(..).get_mapped_range(), uvb.slice(..).get_mapped_range()) {
                for r in 0..h {
                    v.extend_from_slice(&y[r * n.y_row as usize..][..w]);
                }
                for r in 0..h / 2 {
                    v.extend_from_slice(&uv[r * n.uv_row as usize..][..w]);
                }
                frame.nv12 = Some(Arc::new(v));
            }
        }
        if let Some(ts) = &s.ts {
            if let (Some((_, _, period)), Ok(data)) = (&self.timing, ts.slice(..).get_mapped_range()) {
                let t0 = u64::from_le_bytes(data[0..8].try_into().unwrap_or_default());
                let t1 = u64::from_le_bytes(data[8..16].try_into().unwrap_or_default());
                drop(data);
                self.last_gpu_ms = (t1 > t0).then(|| (t1 - t0) as f32 * period / 1_000_000.0);
            }
        }
        self.unmap_slot(slot);
        frame
    }
}

/// A readback target: the canvas as BGRA, optionally as NV12 planes, and
/// the pass's timestamps.
struct Slot {
    bgra: wgpu::Buffer,
    ts: Option<wgpu::Buffer>,
    planes: Option<(wgpu::Buffer, wgpu::Buffer)>,
    /// Mappings finished since the last submit, against how many were asked.
    done: Arc<AtomicUsize>,
    expect: usize,
    has_nv12: bool,
    sub: Option<wgpu::SubmissionIndex>,
}

fn map_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    })
}

/// RGB -> NV12 (BT.709, limited range) on the GPU: a full-size luma pass
/// and a half-size chroma pass, whose bilinear sample at each 2x2 block's
/// centre averages the block. Reads back 1.5 bytes per pixel and spares the
/// CPU the conversion.
struct Nv12Pass {
    y_tex: wgpu::Texture,
    uv_tex: wgpu::Texture,
    y_view: wgpu::TextureView,
    uv_view: wgpu::TextureView,
    y_pipe: wgpu::RenderPipeline,
    uv_pipe: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    y_row: u32,
    uv_row: u32,
}

const NV12_SHADER: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct V { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    let x = f32(i & 1u);
    let y = f32((i >> 1u) & 1u);
    var o: V;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}
fn luma(c: vec3<f32>) -> f32 { return 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b; }
@fragment
fn fs_y(v: V) -> @location(0) vec4<f32> {
    let c = textureSample(src, samp, v.uv).rgb;
    return vec4<f32>((16.0 + 219.0 * luma(c)) / 255.0, 0.0, 0.0, 1.0);
}
@fragment
fn fs_uv(v: V) -> @location(0) vec4<f32> {
    let c = textureSample(src, samp, v.uv).rgb;
    let y = luma(c);
    let cb = (c.b - y) / 1.8556;
    let cr = (c.r - y) / 1.5748;
    return vec4<f32>((128.0 + 224.0 * cb) / 255.0, (128.0 + 224.0 * cr) / 255.0, 0.0, 1.0);
}
"#;

impl Nv12Pass {
    fn new(device: &wgpu::Device, target: &wgpu::Texture, w: u32, h: u32) -> Self {
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let tex = |label: &str, w: u32, h: u32, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let y_tex = tex("nv12 y", w, h, wgpu::TextureFormat::R8Unorm);
        let uv_tex = tex("nv12 uv", w / 2, h / 2, wgpu::TextureFormat::Rg8Unorm);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("nv12"), source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()) });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("nv12"), bind_group_layouts: &[Some(&layout)], ..Default::default() });
        let pipe = |entry: &str, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("nv12"),
                layout: Some(&pl),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&target.create_view(&Default::default())) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        Self {
            y_view: y_tex.create_view(&Default::default()),
            uv_view: uv_tex.create_view(&Default::default()),
            y_pipe: pipe("fs_y", wgpu::TextureFormat::R8Unorm),
            uv_pipe: pipe("fs_uv", wgpu::TextureFormat::Rg8Unorm),
            y_tex,
            uv_tex,
            bind,
            y_row: w.div_ceil(align) * align,
            uv_row: w.div_ceil(align) * align,
        }
    }

    fn encode(&self, enc: &mut wgpu::CommandEncoder, y_buf: &wgpu::Buffer, uv_buf: &wgpu::Buffer, w: u32, h: u32) {
        for (pipe, view) in [(&self.y_pipe, &self.y_view), (&self.uv_pipe, &self.uv_view)] {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipe);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.draw(0..4, 0..1);
        }
        let copy = |enc: &mut wgpu::CommandEncoder, tex: &wgpu::Texture, buf: &wgpu::Buffer, row: u32, w: u32, h: u32| {
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        copy(enc, &self.y_tex, y_buf, self.y_row, w, h);
        copy(enc, &self.uv_tex, uv_buf, self.uv_row, w / 2, h / 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrous_core::Transform;

    fn gpu(w: u32, h: u32) -> Option<GpuCompositor> {
        match GpuCompositor::new(w, h) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("no GPU adapter ({e}); skipping");
                None
            }
        }
    }

    fn scene() -> Vec<VideoFrame> {
        let mut a = vec![0u8; 64 * 48 * 4];
        for (i, px) in a.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % 64, i / 64);
            px.copy_from_slice(&[(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8, 255]);
        }
        vec![VideoFrame::new(64, 48, a), VideoFrame::solid(64, 48, [10, 200, 90, 255])]
    }

    fn layer(f: &VideoFrame) -> Layer<'_> {
        Layer { frame: f, transform: Transform::default(), filters: &[] }
    }

    #[test]
    fn nv12_matches_a_reference_conversion() {
        let Some(mut g) = gpu(64, 48) else { return };
        let frames = scene();
        let out = g.compose_into(&[layer(&frames[0])], Vec::new(), true);
        let nv12 = out.nv12.expect("nv12 requested");
        assert_eq!(nv12.len(), 64 * 48 * 3 / 2);
        let bgra = &out.data;
        let mut worst = 0i32;
        for y in 0..48usize {
            for x in 0..64usize {
                let i = (y * 64 + x) * 4;
                let (b, gg, r) = (bgra[i] as f32 / 255.0, bgra[i + 1] as f32 / 255.0, bgra[i + 2] as f32 / 255.0);
                let want = 16.0 + 219.0 * (0.2126 * r + 0.7152 * gg + 0.0722 * b);
                worst = worst.max((nv12[y * 64 + x] as f32 - want).abs().round() as i32);
            }
        }
        assert!(worst <= 2, "luma off by {worst}");
        // Chroma of a 2x2 block: the average colour, converted.
        let (mut cb_worst, mut cr_worst) = (0i32, 0i32);
        for by in 0..24usize {
            for bx in 0..32usize {
                let (mut r, mut gg, mut b) = (0.0, 0.0, 0.0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let i = ((by * 2 + dy) * 64 + bx * 2 + dx) * 4;
                    b += bgra[i] as f32 / 255.0 / 4.0;
                    gg += bgra[i + 1] as f32 / 255.0 / 4.0;
                    r += bgra[i + 2] as f32 / 255.0 / 4.0;
                }
                let y = 0.2126 * r + 0.7152 * gg + 0.0722 * b;
                let (cb, cr) = (128.0 + 224.0 * (b - y) / 1.8556, 128.0 + 224.0 * (r - y) / 1.5748);
                let o = 64 * 48 + (by * 32 + bx) * 2;
                cb_worst = cb_worst.max((nv12[o] as f32 - cb).abs().round() as i32);
                cr_worst = cr_worst.max((nv12[o + 1] as f32 - cr).abs().round() as i32);
            }
        }
        assert!(cb_worst <= 3 && cr_worst <= 3, "chroma off by {cb_worst}/{cr_worst}");
    }

    #[test]
    fn pipelined_frames_arrive_one_call_late_and_in_order() {
        let Some(mut g) = gpu(64, 48) else { return };
        let frames = scene();
        // First call primes the pipeline and returns its own picture.
        let (f0, current) = g.compose_pipelined(&[layer(&frames[0])], Vec::new(), false);
        assert!(current);
        assert_eq!(&f0.data[..4], &frames[0].data[..4]);
        // Then each call returns the previous call's picture.
        let (f1, current) = g.compose_pipelined(&[layer(&frames[1])], Vec::new(), false);
        assert!(!current);
        assert_eq!(&f1.data[..4], &frames[0].data[..4], "still the first scene");
        let (f2, _) = g.compose_pipelined(&[layer(&frames[1])], Vec::new(), false);
        assert_eq!(&f2.data[..4], &[10, 200, 90, 255], "the second scene arrives a call later");
        // Leaving the pipeline gives a synchronous, current result again.
        let f3 = g.compose_into(&[layer(&frames[0])], Vec::new(), false);
        assert_eq!(&f3.data[..4], &frames[0].data[..4]);
    }

    #[test]
    fn pipelined_and_synchronous_output_agree() {
        let Some(mut g) = gpu(64, 48) else { return };
        let frames = scene();
        let sync = g.compose_into(&[layer(&frames[0])], Vec::new(), false);
        g.discard_pending();
        let (piped, _) = g.compose_pipelined(&[layer(&frames[0])], Vec::new(), false);
        assert_eq!(sync.data, piped.data);
    }

    /// Shrinking must average the source, not skip over it: 1-pixel white lines every 4th pixel,
    /// drawn at a quarter size, should come out as about 25% grey (bilinear alone lands between the
    /// lines and gives black).
    #[test]
    fn downscaling_averages_instead_of_skipping_pixels() {
        let Some(mut g) = gpu(64, 64) else { return };
        let mut src = vec![0u8; 256 * 256 * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            let v = if (i % 256) % 4 == 0 { 255 } else { 0 };
            px.copy_from_slice(&[v, v, v, 255]);
        }
        let f = VideoFrame::new(256, 256, src);
        let tf = Transform { scale_x: 0.25, scale_y: 0.25, ..Transform::default() };
        let out = g.compose_into(&[Layer { frame: &f, transform: tf, filters: &[] }], Vec::new(), false);
        let inner: Vec<u8> = out.data.chunks_exact(4).enumerate().filter(|(i, _)| (4..60).contains(&(i % 64)) && (4..60).contains(&(i / 64))).map(|(_, p)| p[0]).collect();
        let mean = inner.iter().map(|&v| v as f32).sum::<f32>() / inner.len() as f32;
        let worst = inner.iter().map(|&v| (v as f32 - 64.0).abs()).fold(0.0, f32::max);
        assert!((mean - 64.0).abs() < 8.0, "mean {mean}, expected ~64");
        assert!(worst < 24.0, "pixels vary by {worst} from the average: the lines were skipped, not averaged");
    }

    /// 1:1 and upscaled layers must keep the plain single-sample path (and so the CPU compositor's result).
    #[test]
    fn unscaled_layers_are_not_blurred() {
        let Some(mut g) = gpu(64, 48) else { return };
        let frames = scene();
        let a = g.compose_into(&[layer(&frames[0])], Vec::new(), false);
        assert_eq!(&a.data[..4], &frames[0].data[..4]);
        assert_eq!(a.data, frames[0].data, "a 1:1 layer changed");
    }

    // ---- GPU video filters against their CPU reference ----

    fn varied(w: u32, h: u32) -> VideoFrame {
        let mut d = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                // A green-screen block, skin-ish tones, gradients, and a range of alpha.
                let green = x < w / 3 && y < h / 2;
                let (b, g, r) = if green { (30, 200, 40) } else { ((x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y) * 3 % 256) as u8) };
                d[i..i + 4].copy_from_slice(&[b, g, r, 255 - (x * 2 % 90) as u8]);
            }
        }
        VideoFrame::new(w, h, d)
    }

    /// GPU-filtered composite vs the CPU filters composited on the CPU. Differences should be rounding only.
    fn check_filters(plan: Vec<crate::filters::GpuFilter>, tolerance: i32) {
        let Some(mut g) = gpu(96, 64) else { return };
        let f = varied(96, 64);
        let on_gpu = g.compose_into(&[Layer { frame: &f, transform: Transform::default(), filters: &plan }], Vec::new(), false);
        let baked = crate::filters::apply_plan_cpu(&f, &plan);
        let on_cpu = crate::Compositor::new(96, 64).compose(&[Layer { frame: &baked, transform: Transform::default(), filters: &[] }]);
        let worst = on_gpu.data.iter().zip(on_cpu.data.iter()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= tolerance, "GPU and CPU filters differ by {worst} (allowed {tolerance})");
        assert_ne!(on_gpu.data, g.compose_into(&[layer(&f)], Vec::new(), false).data, "the filter changed nothing");
    }

    use crate::filters::GpuFilter;

    #[test]
    fn gpu_chroma_key_matches_cpu() {
        check_filters(vec![GpuFilter::ChromaKey { key_rgb: [0, 255, 0], similarity: 400, smoothness: 80, spill: 100 }], 4);
    }

    #[test]
    fn gpu_color_correction_matches_cpu() {
        check_filters(vec![GpuFilter::ColorCorrection { gamma: 0.3, contrast: 0.2, brightness: 0.1, saturation: 0.4, hue_shift: 40.0, opacity: 0.8 }], 4);
        check_filters(vec![GpuFilter::ColorCorrection { gamma: -0.5, contrast: -0.3, brightness: -0.1, saturation: -0.6, hue_shift: 0.0, opacity: 1.0 }], 4);
    }

    #[test]
    fn gpu_scroll_matches_cpu() {
        // A pure pixel shuffle: the only difference is the compositors' own alpha-blend rounding (1).
        check_filters(vec![GpuFilter::Scroll { dx: 7.0, dy: -5.0 }], 1);
        check_filters(vec![GpuFilter::Scroll { dx: 200.0, dy: 130.4 }], 1); // more than a frame: wraps
    }

    #[test]
    fn gpu_lut_matches_cpu() {
        // A 3x3x3 cube that tints and swaps channels, so any axis mix-up shows.
        let mut text = String::from("LUT_3D_SIZE 3\n");
        for b in 0..3 {
            for g in 0..3 {
                for r in 0..3 {
                    let (r, g, b) = (r as f32 / 2.0, g as f32 / 2.0, b as f32 / 2.0);
                    text.push_str(&format!("{} {} {}\n", (g * 0.8 + 0.1).min(1.0), (b * 0.9).min(1.0), (r * 0.5 + 0.3).min(1.0)));
                }
            }
        }
        let lut = Arc::new(crate::filters::Lut3d::parse(&text).unwrap());
        check_filters(vec![GpuFilter::Lut { lut, amount: 0.7 }], 4);
    }

    #[test]
    fn gpu_background_mask_matches_cpu() {
        let n = ferrous_matte::SIZE;
        let cells: Vec<f32> = (0..n * n).map(|i| ((i % n) as f32 / n as f32 * 0.7 + (i / n) as f32 / n as f32 * 0.3).min(1.0)).collect();
        let mask = Arc::new(crate::matte::Mask { cells, version: 3 });
        check_filters(vec![GpuFilter::Matte { mask, threshold: 0.5, softness: 0.2 }], 4);
    }

    #[test]
    fn gpu_filter_chains_run_in_order() {
        let chain = vec![
            GpuFilter::ChromaKey { key_rgb: [0, 255, 0], similarity: 400, smoothness: 80, spill: 100 },
            GpuFilter::ColorCorrection { gamma: 0.2, contrast: 0.1, brightness: 0.05, saturation: 0.3, hue_shift: 25.0, opacity: 0.9 },
            GpuFilter::Scroll { dx: 11.0, dy: 3.0 },
        ];
        check_filters(chain.clone(), 5);
        let mut reversed = chain;
        reversed.reverse();
        check_filters(reversed, 5);
    }

    #[test]
    fn filtered_layers_do_not_disturb_unfiltered_ones() {
        let Some(mut g) = gpu(96, 64) else { return };
        let f = varied(96, 64);
        let plain = g.compose_into(&[layer(&f)], Vec::new(), false);
        let plan = [GpuFilter::Scroll { dx: 5.0, dy: 0.0 }];
        // Same frame drawn twice, once filtered: the unfiltered copy must be untouched, and repeated
        // composition (texture reuse) must give the same result each time.
        for _ in 0..3 {
            let mixed = g.compose_into(&[layer(&f), Layer { frame: &f, transform: Transform { x: 0.0, y: 0.0, scale_x: 0.5, scale_y: 0.5, ..Transform::default() }, filters: &plan }], Vec::new(), false);
            assert_eq!(&mixed.data[(60 * 96 * 4)..(60 * 96 * 4 + 40)], &plain.data[(60 * 96 * 4)..(60 * 96 * 4 + 40)], "bottom rows come only from the unfiltered layer");
        }
    }
}
