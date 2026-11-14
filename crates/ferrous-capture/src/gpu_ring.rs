//! A small ring of D3D11 textures shared through NT handles. Each captured frame is copied GPU to GPU
//! into the next free one; the renderer opens the same texture on its own D3D12 device and draws from
//! it, so the picture never visits the CPU.

use std::sync::Arc;

use ferrous_core::{SharedFence, SharedFrame, VideoFrame};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Query, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_FENCE_FLAG_SHARED, D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
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
    /// The ring's fence stays open while any of its frames is held.
    _fence: Option<Arc<FenceGuard>>,
}

/// The ring's shared fence and its NT handle.
struct FenceGuard {
    fence: ID3D11Fence,
    handle: HANDLE,
    id: u64,
}

impl Drop for FenceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle was made by `CreateSharedHandle` and is closed only here.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// SAFETY: as for `SlotGuard`: D3D11 fences may be used from any thread.
unsafe impl Send for FenceGuard {}
unsafe impl Sync for FenceGuard {}

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
    /// Signalled after each copy (the consumer's GPU waits for it); `None` where shared fences are
    /// unavailable, and the copy is then waited for here.
    fence: Option<(Arc<FenceGuard>, ID3D11DeviceContext4)>,
    fence_value: u64,
    next: usize,
}

/// A fence other devices can open and wait on, if this device can make one.
fn shared_fence(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Option<(Arc<FenceGuard>, ID3D11DeviceContext4)> {
    let d5: ID3D11Device5 = device.cast().ok()?;
    let c4: ID3D11DeviceContext4 = context.cast().ok()?;
    // SAFETY: plain COM calls; the fence starts at 0 and the handle is anonymous (this process only).
    unsafe {
        let mut fence: Option<ID3D11Fence> = None;
        d5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence).ok()?;
        let fence = fence?;
        let handle = fence.CreateSharedHandle(None, 0x1000_0000, windows::core::PCWSTR::null()).ok()?;
        Some((Arc::new(FenceGuard { fence, handle, id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }), c4))
    }
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
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, w: u32, h: u32) -> windows::core::Result<Self> {
        let fence = shared_fence(device, context);
        if fence.is_none() {
            log::info!("GPU capture: no shared fences on this device; waiting for each copy instead");
        }
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
            slots.push(Slot { id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed), guard: Arc::new(SlotGuard { _texture: tex, handle, _fence: fence.as_ref().map(|(f, _)| f.clone()) }), seq: 0 });
        }
        let mut query = None;
        // SAFETY: valid description, receives the query.
        unsafe { device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query))? };
        Ok(Self { slots, size: (w, h), luid: adapter_luid(device)?, query: query.ok_or_else(windows::core::Error::empty)?, fence, fence_value: 0, next: 0 })
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
        }
        let mut fence = None;
        if let Some((f, c4)) = &self.fence {
            self.fence_value += 1;
            // SAFETY: the fence belongs to this device; the flush sends the copy and the signal on
            // their way, and the consumer's GPU waits for the value before reading the texture.
            unsafe {
                c4.Signal(&f.fence, self.fence_value)?;
                context.Flush();
            }
            fence = Some(SharedFence { id: f.id, handle: f.handle.0 as usize, value: self.fence_value });
        } else {
            self.wait_for_copy(context)?;
        }
        let slot = &mut self.slots[i];
        slot.seq += 1;
        let shared = SharedFrame { id: slot.id, seq: slot.seq, handle: slot.guard.handle.0 as usize, luid: self.luid, guard: slot.guard.clone(), fence };
        Ok(Some(VideoFrame::from_gpu(self.size.0, self.size.1, shared)))
    }

    /// Without a shared fence: other devices only see the copy once the GPU has finished it.
    fn wait_for_copy(&self, context: &ID3D11DeviceContext) -> windows::core::Result<()> {
        // SAFETY: the query belongs to this device and context.
        unsafe {
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
        Ok(())
    }
}
