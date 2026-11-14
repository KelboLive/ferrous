//! The graphics card's own utilisation from NVIDIA's driver library (`nvml.dll`), which is what
//! RTSS, MSI Afterburner and `nvidia-smi` report. Windows' per-engine counters can read far higher
//! for the same card, because they count time a queue holds work, not time the card spends busy.

use std::ffi::c_void;

type Handle = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

pub struct Nvml {
    // Keeps the library loaded while the function pointers are in use.
    _lib: libloading::Library,
    device: Handle,
    utilization: unsafe extern "C" fn(Handle, *mut Utilization) -> i32,
}

// SAFETY: NVML's query functions may be called from any thread; the handle is only read.
unsafe impl Send for Nvml {}

impl Nvml {
    pub fn new() -> Option<Self> {
        // SAFETY: loading NVIDIA's driver library and looking up its documented C entry points.
        unsafe {
            let lib = libloading::Library::new("nvml.dll")
                .or_else(|_| libloading::Library::new(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll"))
                .ok()?;
            let init: libloading::Symbol<unsafe extern "C" fn() -> i32> = lib.get(b"nvmlInit_v2\0").ok()?;
            let by_index: libloading::Symbol<unsafe extern "C" fn(u32, *mut Handle) -> i32> = lib.get(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let utilization: libloading::Symbol<unsafe extern "C" fn(Handle, *mut Utilization) -> i32> = lib.get(b"nvmlDeviceGetUtilizationRates\0").ok()?;
            if init() != 0 {
                return None;
            }
            let mut device: Handle = std::ptr::null_mut();
            if by_index(0, &mut device) != 0 || device.is_null() {
                return None;
            }
            let utilization = *utilization;
            Some(Self { _lib: lib, device, utilization })
        }
    }

    /// Percent of the last sample period the card was running anything.
    pub fn percent(&self) -> Option<f32> {
        let mut u = Utilization::default();
        // SAFETY: `device` is a valid handle from NVML and `u` is a matching out-struct.
        (unsafe { (self.utilization)(self.device, &mut u) } == 0).then_some(u.gpu as f32)
    }
}
