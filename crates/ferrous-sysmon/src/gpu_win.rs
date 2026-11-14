//! GPU usage from Windows performance counters (PDH): the "GPU Engine"
//! utilisation and "GPU Process Memory" counters behind Task Manager.

use std::collections::HashMap;

use windows::core::{w, PCWSTR};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};

pub struct GpuSample {
    pub total: f32,
    pub per_pid: HashMap<u32, f32>,
    pub vram_total: u64,
    pub vram_per_pid: HashMap<u32, u64>,
}

pub struct GpuCounters {
    query: PDH_HQUERY,
    engine: PDH_HCOUNTER,
    memory: PDH_HCOUNTER,
    adapters: Option<PDH_HCOUNTER>,
    last_report: std::time::Instant,
}

// The PDH handles are only used from the sysmon thread.
unsafe impl Send for GpuCounters {}

fn check(status: u32, what: &str) -> Result<(), String> {
    if status == 0 { Ok(()) } else { Err(format!("{what} failed (0x{status:08X})")) }
}

/// Reads every instance of a wildcard counter as (instance name, value).
unsafe fn read_array(counter: PDH_HCOUNTER, large: bool) -> Vec<(String, f64)> {
    let fmt = if large { PDH_FMT_LARGE } else { PDH_FMT_DOUBLE };
    let mut size = 0u32;
    let mut count = 0u32;
    let status = unsafe { PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, None) };
    if status != PDH_MORE_DATA || size == 0 {
        return Vec::new();
    }
    // The API wants a byte buffer; items are followed by their name strings.
    let items = (size as usize).div_ceil(std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());
    let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = vec![Default::default(); items];
    let status = unsafe { PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, Some(buf.as_mut_ptr())) };
    if status != 0 {
        return Vec::new();
    }
    buf.iter()
        .take(count as usize)
        .filter(|it| it.FmtValue.CStatus == 0)
        .map(|it| {
            let name = unsafe { it.szName.to_string() }.unwrap_or_default();
            let v = unsafe { if large { it.FmtValue.Anonymous.largeValue as f64 } else { it.FmtValue.Anonymous.doubleValue } };
            (name, v)
        })
        .collect()
}

impl GpuCounters {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            check(PdhOpenQueryW(PCWSTR::null(), 0, &mut query), "PdhOpenQuery")?;
            let mut engine = PDH_HCOUNTER::default();
            check(PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut engine), "GPU Engine counter")?;
            let mut memory = PDH_HCOUNTER::default();
            check(PdhAddEnglishCounterW(query, w!("\\GPU Process Memory(*)\\Dedicated Usage"), 0, &mut memory), "GPU memory counter")?;
            // Rate counters need two collections before the first value.
            let mut adapters = PDH_HCOUNTER::default();
            let adapters = (PdhAddEnglishCounterW(query, w!("\\GPU Adapter Memory(*)\\Dedicated Usage"), 0, &mut adapters) == 0).then_some(adapters);
            let _ = PdhCollectQueryData(query);
            Ok(Self { query, engine, memory, adapters, last_report: std::time::Instant::now() - std::time::Duration::from_secs(60) })
        }
    }

    pub fn sample(&mut self) -> Option<GpuSample> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let engines = read_array(self.engine, false);
            // The graphics card is the adapter with the most video memory in use.
            let main = self.adapters.and_then(|c| {
                read_array(c, true).into_iter().filter_map(|(n, v)| crate::luid_of(&n).map(|l| (l, v))).max_by(|a, b| a.1.total_cmp(&b.1)).map(|(l, _)| l)
            });
            let (per_pid, total) = crate::aggregate_gpu_on(&engines, main.as_deref());
            if total >= 30.0 && self.last_report.elapsed() >= std::time::Duration::from_secs(30) {
                self.last_report = std::time::Instant::now();
                let mut top: Vec<_> = engines.iter().filter(|(_, v)| *v > 1.0).collect();
                top.sort_by(|a, b| b.1.total_cmp(&a.1));
                log::debug!("GPU reads {total:.0}% (main adapter {main:?}); busiest engine instances: {:?}", top.into_iter().take(6).collect::<Vec<_>>());
            }
            let mut vram_per_pid: HashMap<u32, u64> = HashMap::new();
            for (name, v) in read_array(self.memory, true) {
                if let Some((pid, _)) = crate::parse_gpu_instance(&name) {
                    *vram_per_pid.entry(pid).or_default() += v as u64;
                }
            }
            let vram_total = vram_per_pid.values().sum();
            Some(GpuSample { total, per_pid, vram_total, vram_per_pid })
        }
    }
}

impl Drop for GpuCounters {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
