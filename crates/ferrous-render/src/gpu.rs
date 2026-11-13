//! GPU compositor (wgpu: Direct3D 12 / Vulkan on Windows).
//!
//! Produces the same image as the CPU [`crate::Compositor`] (bilinear
//! sampling, straight-alpha "over" blending in gamma space) so either can be
//! used interchangeably. Source textures are cached by frame identity, so
//! static sources (images, colors) are uploaded once, not every frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ferrous_core::VideoFrame;
use wgpu::util::DeviceExt;

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

@fragment
fn fs(i: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, i.uv);
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

pub struct GpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
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
    spare: Vec<((u32, u32), wgpu::Texture, wgpu::TextureView)>,
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
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("no GPU adapter: {e}"))?;
        let software = adapter.get_info().device_type == wgpu::DeviceType::Cpu;
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
                        has_dynamic_offset: false,
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
        Ok(Self {
            device,
            queue,
            pipeline,
            layout,
            sampler,
            target,
            slots,
            pending: None,
            padded_row,
            nv12: None,
            width,
            height,
            cache: HashMap::new(),
            spare: Vec::new(),
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

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    fn texture_for(&mut self, frame: &VideoFrame) -> usize {
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

        let (cw, ch) = (self.width as f32, self.height as f32);
        let mut bind_groups = Vec::new();
        for (layer, key) in layers.iter().zip(&keys) {
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
            let ubuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("layer"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.cache[key].view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            bind_groups.push(bg);
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
            for bg in groups {
                pass.set_bind_group(0, bg, &[]);
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
        Layer { frame: f, transform: Transform::default() }
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
}
