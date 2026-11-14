//! A small ring of D3D11 textures shared through NT handles. Each captured frame is copied GPU to GPU
//! into the next free one; the renderer opens the same texture on its own D3D12 device and draws from
//! it, so the picture never visits the CPU.

use std::sync::Arc;

use ferrous_core::{SharedFrame, VideoFrame};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIResource1};

/// Textures in the ring: one being drawn, one being written, a spare, and one for a consumer that is
/// a frame late.
const SLOTS: usize = 4;

/// What a consumer's `SharedFrame` keeps alive: the texture and its handle.
struct SlotGuard {
    _texture: ID3D11Texture2D,
    handle: HANDLE,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        // SAFETY: the handle was made by `CreateSharedHandle` and is closed only here.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// SAFETY: D3D11 textures and NT handles may be used from any thread; the guard is only held, never used.
unsafe impl Send for SlotGuard {}
unsafe impl Sync for SlotGuard {}

struct Slot {
    id: u64,
    guard: Arc<SlotGuard>,
    seq: u64,
}

pub struct Ring {
    slots: Vec<Slot>,
    size: (u32, u32),
    luid: i64,
    query: ID3D11Query,
    next: usize,
}

/// The adapter a D3D11 device runs on, as a single number.
fn adapter_luid(device: &ID3D11Device) -> windows::core::Result<i64> {
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: plain COM calls on live objects.
    let desc = unsafe { dxgi.GetAdapter()?.GetDesc()? };
    Ok((i64::from(desc.AdapterLuid.HighPart) << 32) | i64::from(desc.AdapterLuid.LowPart))
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Ring {
    pub fn new(device: &ID3D11Device, w: u32, h: u32) -> windows::core::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut slots = Vec::with_capacity(SLOTS);
        for _ in 0..SLOTS {
            let mut tex: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is valid for the call and `tex` receives the new texture.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            let tex = tex.ok_or_else(windows::core::Error::empty)?;
            let resource: IDXGIResource1 = tex.cast()?;
            // Read and write access for the consumer's device (0x8000_0000 | 1).
            // SAFETY: no name and no security attributes: an anonymous handle, valid in this process.
            let handle = unsafe { resource.CreateSharedHandle(None, 0x8000_0001, windows::core::PCWSTR::null())? };
            slots.push(Slot { id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed), guard: Arc::new(SlotGuard { _texture: tex, handle }), seq: 0 });
        }
        let mut query = None;
        // SAFETY: valid description, receives the query.
        unsafe { device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query))? };
        Ok(Self { slots, size: (w, h), luid: adapter_luid(device)?, query: query.ok_or_else(windows::core::Error::empty)?, next: 0 })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Copies `src` into the next slot nobody is still using and returns it as a frame. `None` when
    /// every slot is busy (the consumer is far behind): the frame is simply dropped.
    pub fn publish(&mut self, context: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> windows::core::Result<Option<VideoFrame>> {
        let n = self.slots.len();
        let Some(i) = (0..n).map(|k| (self.next + k) % n).find(|&i| Arc::strong_count(&self.slots[i].guard) == 1) else {
            return Ok(None);
        };
        self.next = (i + 1) % n;
        let slot = &mut self.slots[i];
        // SAFETY: both are textures of the same size and format on the same device and context.
        unsafe {
            let dst: ID3D11Texture2D = slot.guard._texture.clone();
            context.CopyResource(&dst, src);
            // Other devices only see the copy once the GPU has finished it: flush, then wait for an
            // event query (a fraction of a millisecond for a 1080p copy).
            context.End(&self.query);
            context.Flush();
            let mut done = BOOL(0);
            let started = std::time::Instant::now();
            let mut spins = 0u32;
            loop {
                let _ = context.GetData(&self.query, Some(&mut done as *mut BOOL as *mut _), std::mem::size_of::<BOOL>() as u32, 0);
                if done.as_bool() {
                    break;
                }
                if started.elapsed() > std::time::Duration::from_millis(100) {
                    return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32)));
                }
                // A short copy finishes within a few polls; after that sleep instead of burning a core.
                spins += 1;
                if spins < 50 {
                    std::hint::spin_loop();
                } else {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            }
        }
        slot.seq += 1;
        let shared = SharedFrame { id: slot.id, seq: slot.seq, handle: slot.guard.handle.0 as usize, luid: self.luid, guard: slot.guard.clone() };
        Ok(Some(VideoFrame::from_gpu(self.size.0, self.size.1, shared)))
    }
}
