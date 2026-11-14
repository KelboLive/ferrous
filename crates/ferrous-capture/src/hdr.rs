//! HDR capture: when a display runs in HDR, Windows' 8-bit capture clips everything brighter than SDR
//! white, so bright skies, lights and white walls in an HDR game come out as flat white. Instead we
//! capture 16-bit floating point (scRGB) and tone-map it on the GPU to an ordinary 8-bit picture:
//! up to Windows' "SDR content brightness" everything is unchanged, and brighter detail rolls off
//! smoothly instead of clipping. This is what OBS does when it records SDR from an HDR display.

use std::ffi::c_void;

use windows::core::{s, PCSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Whether any active display is in HDR, and its SDR white level in nits (Windows' "SDR content
/// brightness"). `None` when no display is in HDR (capture stays 8-bit).
pub fn hdr_display() -> Option<f32> {
    let r = query_hdr();
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| match r {
        Some(n) => log::info!("display is in HDR (SDR content brightness {n:.0} nits): captures will be tone-mapped"),
        None => log::info!("no display is in HDR: captures are standard range"),
    });
    r
}

fn query_hdr() -> Option<f32> {
    // SAFETY: plain Win32 queries into buffers sized by the API itself.
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).ok().ok()?;
        paths.truncate(np as usize);
        for p in &paths {
            let header = |kind, size: usize| DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: kind, size: size as u32, adapterId: p.targetInfo.adapterId, id: p.targetInfo.id };
            let mut color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO { header: header(DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO, std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>()), ..Default::default() };
            if DisplayConfigGetDeviceInfo(&mut color.header) != 0 {
                continue;
            }
            // Bit 1: advanced colour (HDR) enabled.
            if color.Anonymous.value & 0b10 == 0 {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL { header: header(DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>()), SDRWhiteLevel: 0 };
            let nits = if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                white.SDRWhiteLevel as f32 / 1000.0 * 80.0
            } else {
                203.0
            };
            return Some(nits.clamp(80.0, 1000.0));
        }
        None
    }
}

const SHADER: &str = r#"
Texture2D<float4> src : register(t0);
cbuffer Params : register(b0) { float scale; float knee; float2 pad; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    // scRGB: linear, 1.0 = 80 nits. Scale so SDR white is 1.0; out-of-gamut (negative) parts clip.
    float3 c = max(src.Load(int3(pos.xy, 0)).rgb * scale, 0);
    // Roll off highlights on the brightest channel (keeps hue): identity below the knee, and
    // [knee, infinity) squeezed into [knee, 1).
    float m = max(c.r, max(c.g, c.b));
    if (m > knee) {
        float t = (m - knee) / (1 - knee);
        float nm = knee + (1 - knee) * (1 - exp(-t));
        c *= nm / m;
    }
    c = saturate(c);
    float3 lo = c * 12.92;
    float3 hi = 1.055 * pow(c, 1.0 / 2.4) - 0.055;
    return float4(lerp(hi, lo, step(c, 0.0031308)), 1);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    scale: f32,
    knee: f32,
    pad: [f32; 2],
}

/// Tone-maps captured scRGB frames into an 8-bit BGRA texture on the capture's own device.
pub struct ToneMapper {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cbuf: ID3D11Buffer,
    out: Option<(u32, u32, ID3D11Texture2D, ID3D11RenderTargetView)>,
    staging: Option<(u32, u32, ID3D11Texture2D)>,
    white_nits: f32,
}

// SAFETY: D3D11 objects are free-threaded; the mapper is only used on the capture thread.
unsafe impl Send for ToneMapper {}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, BoxError> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source buffer outlives the call; the blobs are returned owned.
    let r = unsafe { D3DCompile(SHADER.as_ptr() as *const c_void, SHADER.len(), PCSTR::null(), None, None, entry, target, 0, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = r {
        let msg = errors
            .map(|b: ID3DBlob| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() })
            .unwrap_or_default();
        return Err(format!("HDR shader didn't compile: {e} {msg}").into());
    }
    code.ok_or_else(|| "HDR shader: no code".into())
}

fn bytes(b: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns this memory for its lifetime.
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

impl ToneMapper {
    pub fn new(device: &ID3D11Device, white_nits: f32) -> Result<Self, BoxError> {
        let vsb = compile(s!("vs"), s!("vs_5_0"))?;
        let psb = compile(s!("ps"), s!("ps_5_0"))?;
        let (mut vs, mut ps, mut cbuf) = (None, None, None);
        // SAFETY: creating objects from compiled bytecode and a constant-buffer description.
        unsafe {
            device.CreateVertexShader(bytes(&vsb), None, Some(&mut vs))?;
            device.CreatePixelShader(bytes(&psb), None, Some(&mut ps))?;
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Params>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            device.CreateBuffer(&desc, None, Some(&mut cbuf))?;
        }
        log::info!("HDR display: tone-mapping captures to SDR (SDR white {white_nits:.0} nits)");
        Ok(Self { vs: vs.ok_or("no vertex shader")?, ps: ps.ok_or("no pixel shader")?, cbuf: cbuf.ok_or("no constant buffer")?, out: None, staging: None, white_nits })
    }

    /// Tone-maps `src` (scRGB, R16G16B16A16_FLOAT) into the mapper's BGRA texture and returns it.
    pub fn run(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<ID3D11Texture2D, BoxError> {
        if self.out.as_ref().is_none_or(|(ow, oh, ..)| (*ow, *oh) != (w, h)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let (mut tex, mut rtv) = (None, None);
            // SAFETY: plain resource creation on the capture device.
            unsafe {
                device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex = tex.as_ref().ok_or("no HDR target")?;
                device.CreateRenderTargetView(tex, None, Some(&mut rtv))?;
            }
            self.out = Some((w, h, tex.ok_or("no HDR target")?, rtv.ok_or("no HDR view")?));
        }
        let (_, _, out, rtv) = self.out.as_ref().expect("made above");
        let params = Params { scale: 80.0 / self.white_nits, knee: 0.75, pad: [0.0; 2] };
        // SAFETY: binds this mapper's own objects and a view of the captured texture for one draw,
        // then unbinds them so the capture's texture isn't held.
        unsafe {
            let mut srv = None;
            device.CreateShaderResourceView(src, None, Some(&mut srv))?;
            ctx.UpdateSubresource(&self.cbuf, 0, None, &params as *const Params as *const c_void, 0, 0);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(0, Some(&[srv.clone()]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
        }
        Ok(out.clone())
    }

    /// Copies the last tone-mapped picture to the CPU: calls `row(y, bytes)` for every row (BGRA).
    pub fn read(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, mut row: impl FnMut(usize, &[u8])) -> Result<(), BoxError> {
        let Some((w, h, out, _)) = self.out.as_ref() else { return Ok(()) };
        let (w, h) = (*w, *h);
        if self.staging.as_ref().is_none_or(|(sw, sh, _)| (*sw, *sh) != (w, h)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            // SAFETY: plain resource creation.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            self.staging = Some((w, h, tex.ok_or("no staging texture")?));
        }
        let (_, _, staging) = self.staging.as_ref().expect("made above");
        // SAFETY: the staging texture is mapped for reading and unmapped before returning; rows are
        // read within the mapped pitch.
        unsafe {
            ctx.CopyResource(staging, out);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let base = m.pData as *const u8;
            let pitch = m.RowPitch as usize;
            for y in 0..h as usize {
                row(y, std::slice::from_raw_parts(base.add(y * pitch), w as usize * 4));
            }
            ctx.Unmap(staging, 0);
        }
        Ok(())
    }
}

