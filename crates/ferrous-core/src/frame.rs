use std::sync::Arc;

/// A tightly packed BGRA8 frame (4 bytes per pixel, no row padding).
///
/// Frames are cheap to clone: pixel data is reference counted so a single
/// captured frame can be shared by the compositor, preview and outputs.
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Arc<Vec<u8>>,
    /// The same picture as NV12 (BT.709 limited range), when the GPU
    /// compositor already converted it. Encoders use it instead of
    /// converting on the CPU; anything that changes `data` builds a new
    /// frame, which leaves this `None`.
    pub nv12: Option<Arc<Vec<u8>>>,
    /// `nv12` holds JPEG's YCbCr instead (BT.601, full range: what JFIF decoders expect), made for
    /// the JPEG Link, which then compresses it without touching BGRA. Video encoders must not use it.
    pub nv12_jfif: bool,
    /// Set for a frame that lives only on the GPU (Windows capture handing over a shared texture):
    /// `data` is then empty, and only the GPU compositor can draw it. The engine asks for such
    /// frames only when it can, and otherwise reads the source's ordinary CPU frame.
    pub gpu: Option<Arc<SharedFrame>>,
}

/// A frame in a shared GPU texture, as Windows capture produces it: a D3D11 texture shared through an
/// NT handle that the renderer opens on its own D3D12 device, so the picture never visits the CPU.
pub struct SharedFrame {
    /// Identifies the texture (one of a small ring); `seq` counts the frames written into it.
    pub id: u64,
    pub seq: u64,
    /// The NT handle (as an integer, so this type is the same on every platform).
    pub handle: usize,
    /// The adapter the texture lives on; the renderer must be on the same one.
    pub luid: i64,
    /// Keeps the ring slot from being overwritten while a consumer still holds this frame.
    pub guard: Arc<dyn std::any::Any + Send + Sync>,
    /// A shared fence the producer signals once its copy into the texture is done: the consumer's
    /// GPU waits for it, so nobody waits on the CPU. `None`: the copy was finished before handing over.
    pub fence: Option<SharedFence>,
}

/// See [`SharedFrame::fence`].
#[derive(Clone, Copy, Debug)]
pub struct SharedFence {
    /// Unique for the fence's lifetime (handles can be reused once closed; this can't).
    pub id: u64,
    /// The fence's NT handle, kept open by the frame's guard.
    pub handle: usize,
    /// Wait until the fence reaches this.
    pub value: u64,
}

impl VideoFrame {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        debug_assert_eq!(data.len(), (width * height * 4) as usize);
        Self { width, height, data: Arc::new(data), nv12: None, nv12_jfif: false, gpu: None }
    }

    pub fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Self {
        let mut data = vec![0u8; (width * height * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&bgra);
        }
        Self::new(width, height, data)
    }

    /// A frame that exists only as a shared GPU texture.
    pub fn from_gpu(width: u32, height: u32, shared: SharedFrame) -> Self {
        Self { width, height, data: Arc::new(Vec::new()), nv12: None, nv12_jfif: false, gpu: Some(Arc::new(shared)) }
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VideoFrame({}x{})", self.width, self.height)
    }
}

/// Recycles frame buffers (compositor output, captured frames). A frame's
/// buffer comes back once every consumer (encoders, previews, links) has
/// dropped it, instead of a fresh 8 MB allocation per 1080p frame. On Windows each fresh one is new pages
/// from the OS, faulted in and zeroed on first touch.
#[derive(Default)]
pub struct FramePool {
    handed_out: Vec<Arc<Vec<u8>>>,
}

/// Recent frames tracked for reuse; more in flight just means a fresh buffer.
const POOL_FRAMES: usize = 6;

impl FramePool {
    /// A buffer of `len` bytes. Returns it and whether it is fresh (zeroed);
    /// a recycled one holds an old frame and must be fully overwritten.
    pub fn take(&mut self, len: usize) -> (Vec<u8>, bool) {
        if let Some(i) = self.handed_out.iter().position(|a| Arc::strong_count(a) == 1 && a.len() == len) {
            if let Ok(v) = Arc::try_unwrap(self.handed_out.swap_remove(i)) {
                return (v, false);
            }
        }
        (vec![0u8; len], true)
    }

    /// Remembers a frame this pool's buffer went into.
    pub fn track(&mut self, frame: &VideoFrame) {
        if self.handed_out.iter().any(|a| Arc::ptr_eq(a, &frame.data)) {
            return;
        }
        if self.handed_out.len() >= POOL_FRAMES {
            self.handed_out.remove(0);
        }
        self.handed_out.push(frame.data.clone());
    }
}
