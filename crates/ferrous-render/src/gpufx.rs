//! GPU video filters: each [`GpuFilter`] is one full-screen pass from a layer's texture into the next,
//! so a filtered source never leaves the GPU. The maths mirrors the CPU filters in `filters.rs`
//! (the CPU versions are the reference the tests compare against).

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::Zeroable;
use wgpu::util::DeviceExt;

use crate::filters::GpuFilter;
use crate::matte::Mask;

const SHADER: &str = r#"
struct P {
    ints: vec4<i32>,   // x = kind, y/z = scroll offset, w = LUT size
    f0: vec4<f32>,
    f1: vec4<f32>,
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
};
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var lut: texture_3d<f32>;
@group(0) @binding(3) var mask: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn luma(c: vec3<f32>) -> f32 { return 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b; }

fn lut_at(r: i32, g: i32, b: i32) -> vec3<f32> { return textureLoad(lut, vec3<i32>(r, g, b), 0).rgb; }

fn lut_sample(c: vec3<f32>) -> vec3<f32> {
    let n = p.ints.w - 1;
    let x = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)) * f32(n);
    let i = min(vec3<i32>(floor(x)), vec3<i32>(n - 1));
    let f = x - vec3<f32>(i);
    let c00 = mix(lut_at(i.x, i.y, i.z), lut_at(i.x + 1, i.y, i.z), f.x);
    let c10 = mix(lut_at(i.x, i.y + 1, i.z), lut_at(i.x + 1, i.y + 1, i.z), f.x);
    let c01 = mix(lut_at(i.x, i.y, i.z + 1), lut_at(i.x + 1, i.y, i.z + 1), f.x);
    let c11 = mix(lut_at(i.x, i.y + 1, i.z + 1), lut_at(i.x + 1, i.y + 1, i.z + 1), f.x);
    return mix(mix(c00, c10, f.y), mix(c01, c11, f.y), f.z);
}

fn mask_at(x: i32, y: i32) -> f32 { return textureLoad(mask, vec2<i32>(x, y), 0).r; }

@fragment
fn fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let ip = vec2<i32>(frag.xy);
    let dims = vec2<i32>(textureDimensions(src));
    let kind = p.ints.x;
    var px = textureLoad(src, ip, 0);
    if (kind == 0) {
        // Chroma key (OBS's maths): distance in the Cb/Cr plane from the key colour.
        let y = luma(px.rgb);
        let cb = (px.b - y) / 1.8556;
        let cr = (px.r - y) / 1.5748;
        let base = length(vec2<f32>(cb - p.f0.x, cr - p.f0.y)) - p.f0.z;
        let alpha = pow(clamp(base / p.f0.w, 0.0, 1.0), 1.5);
        let spill = pow(clamp(base / p.f1.x, 0.0, 1.0), 1.5);
        let grey = clamp(y, 0.0, 1.0);
        return vec4<f32>(grey + (px.rgb - vec3<f32>(grey)) * spill, px.a * alpha);
    } else if (kind == 1) {
        // Colour correction: tone curve, hue rotation, saturation, opacity.
        var v = pow(px.rgb, vec3<f32>(p.f0.x));
        v = clamp((v - vec3<f32>(0.5)) * p.f0.y + vec3<f32>(0.5 + p.f0.z), vec3<f32>(0.0), vec3<f32>(1.0));
        if (p.f1.y < 0.5) {
            v = vec3<f32>(dot(p.m0.xyz, v), dot(p.m1.xyz, v), dot(p.m2.xyz, v));
        }
        let y = luma(v);
        v = clamp(vec3<f32>(y) + (v - vec3<f32>(y)) * p.f0.w, vec3<f32>(0.0), vec3<f32>(1.0));
        return vec4<f32>(v, px.a * clamp(p.f1.x, 0.0, 1.0));
    } else if (kind == 2) {
        // Scroll with wrap-around.
        let s = vec2<i32>((ip.x - p.ints.y) % dims.x + dims.x, (ip.y - p.ints.z) % dims.y + dims.y) % dims;
        return textureLoad(src, s, 0);
    } else if (kind == 3) {
        let o = lut_sample(px.rgb);
        let v = clamp(px.rgb + (o - px.rgb) * p.f0.x, vec3<f32>(0.0), vec3<f32>(1.0));
        return vec4<f32>(v, px.a);
    } else {
        // Person mask, upsampled bilinearly, sharpened around a threshold.
        let size = f32(textureDimensions(mask).x);
        let hi_i = i32(size) - 1;
        let fx = clamp((f32(ip.x) + 0.5) / f32(dims.x) * size - 0.5, 0.0, size - 1.0);
        let fy = clamp((f32(ip.y) + 0.5) / f32(dims.y) * size - 0.5, 0.0, size - 1.0);
        let x0 = i32(floor(fx));
        let y0 = i32(floor(fy));
        let x1 = min(x0 + 1, hi_i);
        let y1 = min(y0 + 1, hi_i);
        let tx = fx - f32(x0);
        let ty = fy - f32(y0);
        let top = mix(mask_at(x0, y0), mask_at(x1, y0), tx);
        let bot = mix(mask_at(x0, y1), mask_at(x1, y1), tx);
        let t = clamp((mix(top, bot, ty) - p.f0.x) / (p.f0.y - p.f0.x), 0.0, 1.0);
        let m = t * t * (3.0 - 2.0 * t);
        return vec4<f32>(px.rgb, px.a * m);
    }
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    ints: [i32; 4],
    f0: [f32; 4],
    f1: [f32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
}

type Target = ((u32, u32), wgpu::Texture, wgpu::TextureView);

/// Filter pipeline plus the textures it needs.
pub(crate) struct Fx {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    dummy_lut: wgpu::TextureView,
    dummy_mask: wgpu::TextureView,
    /// Uploaded LUTs by identity; kept while some layer still uses them.
    luts: HashMap<usize, (Arc<crate::filters::Lut3d>, wgpu::TextureView)>,
    mask: Option<(u64, wgpu::TextureView)>,
    /// Intermediate targets, free for reuse.
    free: Vec<Target>,
    /// Targets handed out for the frame being prepared; they go back to `free` on the next frame.
    used: Vec<Target>,
}

fn target_usage() -> wgpu::TextureUsages {
    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
}

impl Fx {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let entry = |binding: u32, ty: wgpu::BindingType| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty, count: None };
        let tex = |dim: wgpu::TextureViewDimension| wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: false }, view_dimension: dim, multisampled: false };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fx"),
            entries: &[
                entry(0, wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }),
                entry(1, tex(wgpu::TextureViewDimension::D2)),
                entry(2, tex(wgpu::TextureViewDimension::D3)),
                entry(3, tex(wgpu::TextureViewDimension::D2)),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("fx"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("fx"), bind_group_layouts: &[Some(&layout)], ..Default::default() });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fx"),
            layout: Some(&pl),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Bgra8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let make = |dim: wgpu::TextureDimension, format: wgpu::TextureFormat, w: u32, h: u32, d: u32, data: &[u8]| {
            let t = device.create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some("fx dummy"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: dim,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                data,
            );
            t.create_view(&Default::default())
        };
        Self {
            pipeline,
            layout,
            dummy_lut: make(wgpu::TextureDimension::D3, wgpu::TextureFormat::Rgba32Float, 1, 1, 1, &[0u8; 16]),
            dummy_mask: make(wgpu::TextureDimension::D2, wgpu::TextureFormat::R32Float, 1, 1, 1, &[0u8; 4]),
            luts: HashMap::new(),
            mask: None,
            free: Vec::new(),
            used: Vec::new(),
        }
    }

    /// Call once per frame before any [`Fx::run`]: last frame's targets are free again, and LUTs
    /// no longer in use are dropped.
    pub(crate) fn begin_frame(&mut self, live_luts: &[usize]) {
        self.free.append(&mut self.used);
        self.free.truncate(12);
        self.luts.retain(|k, _| live_luts.contains(k));
    }

    fn target(&mut self, device: &wgpu::Device, size: (u32, u32)) -> (wgpu::Texture, wgpu::TextureView) {
        if let Some(i) = self.free.iter().position(|t| t.0 == size) {
            let (s, t, v) = self.free.swap_remove(i);
            self.used.push((s, t.clone(), v.clone()));
            return (t, v);
        }
        let t = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fx target"),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: target_usage(),
            view_formats: &[],
        });
        let v = t.create_view(&Default::default());
        self.used.push((size, t.clone(), v.clone()));
        (t, v)
    }

    fn lut_view(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, lut: &Arc<crate::filters::Lut3d>) -> wgpu::TextureView {
        let key = Arc::as_ptr(lut) as usize;
        if let Some((_, v)) = self.luts.get(&key) {
            return v.clone();
        }
        let n = lut.size() as u32;
        let mut texels = Vec::with_capacity(lut.entries().len() * 4);
        for e in lut.entries() {
            texels.extend_from_slice(&[e[0], e[1], e[2], 1.0]);
        }
        let t = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("fx lut"),
                size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(&texels),
        );
        let v = t.create_view(&Default::default());
        self.luts.insert(key, (lut.clone(), v.clone()));
        v
    }

    fn mask_view(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, mask: &Mask) -> wgpu::TextureView {
        if let Some((v, view)) = &self.mask {
            if *v == mask.version {
                return view.clone();
            }
        }
        let n = ferrous_matte::SIZE as u32;
        let t = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("fx mask"),
                size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(&mask.cells),
        );
        let view = t.create_view(&Default::default());
        self.mask = Some((mask.version, view.clone()));
        view
    }

    fn params(op: &GpuFilter, size: (u32, u32)) -> Params {
        let mut p = Params::zeroed();
        match op {
            GpuFilter::ChromaKey { key_rgb, similarity, smoothness, spill } => {
                let k = key_rgb.map(|v| v as f32 / 255.0);
                let y = 0.2126 * k[0] + 0.7152 * k[1] + 0.0722 * k[2];
                p.ints[0] = 0;
                p.f0 = [(k[2] - y) / 1.8556, (k[0] - y) / 1.5748, *similarity as f32 / 1000.0, (*smoothness as f32 / 1000.0).max(0.001)];
                p.f1[0] = (*spill as f32 / 1000.0).max(0.001);
            }
            GpuFilter::ColorCorrection { gamma, contrast, brightness, saturation, hue_shift, opacity } => {
                p.ints[0] = 1;
                let g = if *gamma < 0.0 { 1.0 - gamma } else { 1.0 / (1.0 + gamma) };
                p.f0 = [g, contrast + 1.0, *brightness, saturation + 1.0];
                p.f1 = [*opacity, if hue_shift.abs() < 0.01 { 1.0 } else { 0.0 }, 0.0, 0.0];
                let (sin, cos) = hue_shift.to_radians().sin_cos();
                let k = 1.0f32 / 3.0;
                let sq = k.sqrt();
                let (a, b, c) = (cos + (1.0 - cos) * k, k * (1.0 - cos) - sq * sin, k * (1.0 - cos) + sq * sin);
                p.m0 = [a, b, c, 0.0];
                p.m1 = [c, a, b, 0.0];
                p.m2 = [b, c, a, 0.0];
            }
            GpuFilter::Scroll { dx, dy } => {
                p.ints[0] = 2;
                p.ints[1] = (dx.round() as i64).rem_euclid(size.0 as i64) as i32;
                p.ints[2] = (dy.round() as i64).rem_euclid(size.1 as i64) as i32;
            }
            GpuFilter::Lut { lut, amount } => {
                p.ints[0] = 3;
                p.ints[3] = lut.size() as i32;
                p.f0[0] = amount.clamp(0.0, 1.0);
            }
            GpuFilter::Matte { threshold, softness, .. } => {
                p.ints[0] = 4;
                let soft = softness.max(0.01);
                p.f0 = [threshold - soft, threshold + soft, 0.0, 0.0];
            }
        }
        p
    }

    /// Runs `ops` over `src` (size `size`), returning the final texture view. Each step draws into a
    /// fresh or recycled target; the encoder is the caller's.
    pub(crate) fn run(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        size: (u32, u32),
        ops: &[GpuFilter],
    ) -> wgpu::TextureView {
        let mut cur = src.clone();
        for op in ops {
            let params = Self::params(op, size);
            let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("fx params"), contents: bytemuck::bytes_of(&params), usage: wgpu::BufferUsages::UNIFORM });
            let lut = match op {
                GpuFilter::Lut { lut, .. } => self.lut_view(device, queue, lut),
                _ => self.dummy_lut.clone(),
            };
            let mask = match op {
                GpuFilter::Matte { mask, .. } => self.mask_view(device, queue, mask),
                _ => self.dummy_mask.clone(),
            };
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fx"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&cur) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&lut) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&mask) },
                ],
            });
            let (_tex, out) = self.target(device, size);
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("fx"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &out,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            }
            cur = out;
        }
        cur
    }
}
