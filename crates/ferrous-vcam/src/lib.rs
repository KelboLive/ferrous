//! Virtual camera transport.
//!
//! Ferrous writes NV12 frames into a named shared-memory region; the camera
//! source (`ferrous-vcam-source`, a Media Foundation media source that Windows
//! 11's Frame Server loads when an app opens "Ferrous Virtual Camera")
//! reads the newest one for each sample it delivers.
//!
//! The region is created by the camera source, not by Ferrous: the Frame
//! Server runs as a service, and only services may create objects in the
//! `Global\` namespace that both sides can see. Ferrous opens it once a
//! camera app has started the camera, and keeps retrying until then.
//!
//! Layout: a header, then [`SLOTS`] frame slots. Each slot has a sequence
//! number that is odd while being written (a seqlock), so the reader never
//! returns a torn frame.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAGIC: u32 = 0x4D43_5645; // "EVCM"
pub const VERSION: u32 = 1;
/// Fixed camera format (what the camera advertises to apps).
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const FPS: u32 = 30;
pub const FRAME_BYTES: usize = (WIDTH * HEIGHT * 3 / 2) as usize;
pub const SLOTS: usize = 3;
pub const NAME_GLOBAL: &str = "Global\\FerrousVirtualCamera";
pub const NAME_LOCAL: &str = "Local\\FerrousVirtualCamera";
/// The camera source's COM class id.
pub const CLSID: &str = "{93AC3189-7309-4939-A983-6883CEE48652}";
pub const FRIENDLY_NAME: &str = "Ferrous Virtual Camera";

#[repr(C)]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Index of the newest complete slot, plus one (0 = none yet).
    pub latest: AtomicU32,
    _pad: u32,
    /// Milliseconds (producer clock) of the last write, to detect a stopped producer.
    pub heartbeat_ms: AtomicU64,
    pub seq: [AtomicU64; SLOTS],
}

pub const HEADER_BYTES: usize = 128;
pub const REGION_BYTES: usize = HEADER_BYTES + SLOTS * FRAME_BYTES;

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

/// A view of the shared region (or any buffer with the same layout).
pub struct Region {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// # Safety
    /// `ptr` must point to `len >= REGION_BYTES` writable bytes, 8-aligned,
    /// valid for the life of the `Region`.
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> Self {
        assert!(len >= REGION_BYTES && (ptr as usize).is_multiple_of(8));
        Self { ptr, len }
    }

    pub fn header(&self) -> &Header {
        unsafe { &*(self.ptr as *const Header) }
    }

    fn slot(&self, i: usize) -> *mut u8 {
        debug_assert!(HEADER_BYTES + (i + 1) * FRAME_BYTES <= self.len);
        unsafe { self.ptr.add(HEADER_BYTES + i * FRAME_BYTES) }
    }

    /// Writes the header (the creator of the region does this once).
    pub fn init(&self) {
        let h = self.header();
        h.width.store(WIDTH, Ordering::Relaxed);
        h.height.store(HEIGHT, Ordering::Relaxed);
        h.version.store(VERSION, Ordering::Relaxed);
        h.latest.store(0, Ordering::Relaxed);
        h.magic.store(MAGIC, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        let h = self.header();
        h.magic.load(Ordering::Acquire) == MAGIC && h.version.load(Ordering::Relaxed) == VERSION
    }

    /// Producer: publishes one NV12 frame of [`FRAME_BYTES`].
    pub fn write(&self, nv12: &[u8], now_ms: u64) {
        assert_eq!(nv12.len(), FRAME_BYTES);
        let h = self.header();
        let cur = h.latest.load(Ordering::Acquire);
        let slot = if cur == 0 { 0 } else { cur as usize % SLOTS }; // next after (cur-1)
        let s = &h.seq[slot];
        let v = s.load(Ordering::Relaxed);
        s.store(v | 1, Ordering::Relaxed); // odd: writing
        std::sync::atomic::fence(Ordering::Release);
        unsafe { std::ptr::copy_nonoverlapping(nv12.as_ptr(), self.slot(slot), FRAME_BYTES) };
        s.store((v | 1) + 1, Ordering::Release); // even: done
        h.latest.store(slot as u32 + 1, Ordering::Release);
        h.heartbeat_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Consumer: copies the newest complete frame into `out`. Returns false
    /// if there's none (or it kept changing while being read).
    pub fn read(&self, out: &mut [u8]) -> bool {
        assert_eq!(out.len(), FRAME_BYTES);
        let h = self.header();
        for _ in 0..4 {
            let latest = h.latest.load(Ordering::Acquire);
            if latest == 0 {
                return false;
            }
            let slot = (latest - 1) as usize % SLOTS;
            let s = &h.seq[slot];
            let before = s.load(Ordering::Acquire);
            if before & 1 == 1 {
                continue;
            }
            unsafe { std::ptr::copy_nonoverlapping(self.slot(slot), out.as_mut_ptr(), FRAME_BYTES) };
            std::sync::atomic::fence(Ordering::Acquire);
            if s.load(Ordering::Relaxed) == before {
                return true;
            }
        }
        false
    }
}

/// A "no signal" frame (dark grey with the Ferrous orange bar), shown by the
/// camera while Ferrous isn't sending.
pub fn idle_frame() -> Vec<u8> {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut f = vec![0u8; FRAME_BYTES];
    let (y, uv) = f.split_at_mut(w * h);
    y.fill(40);
    uv.chunks_exact_mut(2).for_each(|c| {
        c[0] = 128;
        c[1] = 128;
    });
    // Orange bar across the middle (BT.709 limited: Y 136, U 70, V 196).
    for row in h * 45 / 100..h * 55 / 100 {
        y[row * w + w / 4..row * w + w * 3 / 4].fill(136);
        if row % 2 == 0 {
            let r = row / 2;
            for x in (w / 4..w * 3 / 4).step_by(2) {
                uv[r * w + x] = 70;
                uv[r * w + x + 1] = 196;
            }
        }
    }
    f
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ------------------------------------------------------------- producer

/// Ferrous's side: opens the region the camera source created.
pub struct Producer {
    region: Region,
    #[cfg(windows)]
    _map: win::Mapping,
    #[cfg(unix)]
    _map: memmap2::MmapMut,
}

impl Producer {
    /// Opens the shared region; fails until a camera app has started the camera.
    pub fn open() -> anyhow::Result<Producer> {
        #[cfg(windows)]
        {
            let map = win::Mapping::open(NAME_GLOBAL).or_else(|_| win::Mapping::open(NAME_LOCAL))?;
            let region = unsafe { Region::from_raw(map.ptr, REGION_BYTES) };
            if !region.is_valid() {
                anyhow::bail!("the camera's shared memory isn't ready");
            }
            Ok(Producer { region, _map: map })
        }
        #[cfg(unix)]
        {
            Self::open_file(&unix_path())
        }
    }

    /// (Unix, for tests and tools) opens a file-backed region.
    #[cfg(unix)]
    pub fn open_file(path: &std::path::Path) -> anyhow::Result<Producer> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let mut map = unsafe { memmap2::MmapMut::map_mut(&f)? };
        let region = unsafe { Region::from_raw(map.as_mut_ptr(), map.len()) };
        if !region.is_valid() {
            anyhow::bail!("the camera's shared memory isn't ready");
        }
        Ok(Producer { region, _map: map })
    }

    pub fn write(&self, nv12: &[u8]) {
        self.region.write(nv12, now_ms());
    }
}

/// Where a Unix "camera" would put its region (used by tests).
#[cfg(unix)]
pub fn unix_path() -> std::path::PathBuf {
    std::env::temp_dir().join("ferrous-vcam.shm")
}

/// (Unix, for tests) the consumer side: creates a file-backed region.
#[cfg(unix)]
pub struct FileConsumer {
    pub region: Region,
    _map: memmap2::MmapMut,
}

#[cfg(unix)]
impl FileConsumer {
    pub fn create(path: &std::path::Path) -> anyhow::Result<Self> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        f.set_len(REGION_BYTES as u64)?;
        let mut map = unsafe { memmap2::MmapMut::map_mut(&f)? };
        let region = unsafe { Region::from_raw(map.as_mut_ptr(), map.len()) };
        region.init();
        Ok(Self { region, _map: map })
    }
}

#[cfg(windows)]
pub mod win;

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> (Vec<u64>, Region) {
        let mut buf = vec![0u64; REGION_BYTES / 8 + 1];
        let r = unsafe { Region::from_raw(buf.as_mut_ptr() as *mut u8, REGION_BYTES) };
        r.init();
        (buf, r)
    }

    #[test]
    fn newest_frame_wins_and_reads_are_whole() {
        let (_b, r) = region();
        let mut out = vec![0u8; FRAME_BYTES];
        assert!(!r.read(&mut out), "nothing written yet");
        for v in 1..=5u8 {
            r.write(&vec![v; FRAME_BYTES], v as u64);
        }
        assert!(r.read(&mut out));
        assert!(out.iter().all(|b| *b == 5));
        assert_eq!(r.header().heartbeat_ms.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn concurrent_reader_never_sees_a_torn_frame() {
        let (_b, r) = region();
        let r = std::sync::Arc::new(r);
        let w = r.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = stop.clone();
        let writer = std::thread::spawn(move || {
            let mut v = 0u8;
            while !s2.load(Ordering::Relaxed) {
                v = v.wrapping_add(1);
                w.write(&vec![v; FRAME_BYTES], 0);
            }
        });
        let mut out = vec![0u8; FRAME_BYTES];
        while r.header().latest.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        let mut good = 0;
        for _ in 0..200 {
            if r.read(&mut out) {
                let first = out[0];
                assert!(out.iter().all(|b| *b == first), "torn frame");
                good += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(good > 50, "{good}");
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_producer_and_consumer() {
        let path = std::env::temp_dir().join(format!("ferrous-vcam-test-{}.shm", std::process::id()));
        assert!(Producer::open_file(&path).is_err(), "no camera yet");
        let c = FileConsumer::create(&path).unwrap();
        let p = Producer::open_file(&path).unwrap();
        let f = idle_frame();
        p.write(&f);
        let mut out = vec![0u8; FRAME_BYTES];
        assert!(c.region.read(&mut out));
        assert_eq!(out, f);
        let _ = std::fs::remove_file(path);
    }
}
