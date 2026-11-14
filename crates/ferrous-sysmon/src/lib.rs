//! System and per-process resource sampling (1 Hz, on its own thread).
//!
//! - CPU and memory for the whole system, for Ferrous itself, and for helper
//!   processes (FFmpeg encoders/decoders, the headless browser) via sysinfo.
//! - On Windows, GPU utilisation and dedicated VRAM, system-wide and per
//!   process, from the same "GPU Engine" performance counters Task Manager uses.

#[cfg(windows)]
mod gpu_win;
#[cfg(windows)]
mod nvml;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use sysinfo::{Disks, Pid, ProcessesToUpdate, System};

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcStats {
    /// Percent of one core (can exceed 100 on multi-threaded processes).
    pub cpu: f32,
    pub mem_bytes: u64,
    /// Percent of the busiest GPU engine, Windows only.
    pub gpu: Option<f32>,
    pub vram_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// 0–100, all cores.
    pub sys_cpu: f32,
    pub cores: usize,
    pub sys_mem_used: u64,
    pub sys_mem_total: u64,
    pub own: ProcStats,
    pub gpu: Option<f32>,
    pub vram_used: Option<u64>,
    pub per_pid: HashMap<u32, ProcStats>,
    /// Free bytes on the disk holding the watched path.
    pub disk_free: Option<u64>,
}

pub struct SystemMonitor {
    snapshot: Arc<Mutex<Snapshot>>,
    watched: Arc<Mutex<HashSet<u32>>>,
    disk_path: Arc<Mutex<Option<PathBuf>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn disk_free(disks: &Disks, path: &Path) -> Option<u64> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

impl SystemMonitor {
    pub fn start() -> Self {
        let snapshot: Arc<Mutex<Snapshot>> = Arc::default();
        let watched: Arc<Mutex<HashSet<u32>>> = Arc::default();
        let disk_path: Arc<Mutex<Option<PathBuf>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, w2, d2, st2) = (snapshot.clone(), watched.clone(), disk_path.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("sysmon".into())
            .spawn(move || {
                let mut sys = System::new();
                let mut disks = Disks::new_with_refreshed_list();
                let own_pid = sysinfo::get_current_pid().ok();
                // Opening NVIDIA's and Windows' GPU counters while the renderer is creating its own GPU
                // device has crashed the driver on some machines, so wait until start-up is over.
                #[cfg(windows)]
                std::thread::sleep(Duration::from_secs(5));
                #[cfg(windows)]
                let nvml = nvml::Nvml::new();
                #[cfg(windows)]
                let mut gpu = gpu_win::GpuCounters::new().map_err(|e| log::info!("GPU counters unavailable: {e}")).ok();
                let mut tick = 0u32;
                while !st2.load(Ordering::Relaxed) {
                    sys.refresh_cpu_usage();
                    sys.refresh_memory();
                    let mut pids: Vec<Pid> = w2.lock().iter().map(|p| Pid::from_u32(*p)).collect();
                    if let Some(p) = own_pid {
                        pids.push(p);
                    }
                    sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);
                    #[cfg(windows)]
                    let gpu_sample = gpu.as_mut().and_then(|g| g.sample());
                    let stat = |pid: Pid| -> ProcStats {
                        #[allow(unused_mut)]
                        let mut s = sys.process(pid).map(|p| ProcStats { cpu: p.cpu_usage(), mem_bytes: p.memory(), ..Default::default() }).unwrap_or_default();
                        #[cfg(windows)]
                        if let Some(g) = &gpu_sample {
                            s.gpu = Some(g.per_pid.get(&pid.as_u32()).copied().unwrap_or(0.0));
                            s.vram_bytes = g.vram_per_pid.get(&pid.as_u32()).copied();
                        }
                        s
                    };
                    let mut snap = Snapshot {
                        sys_cpu: sys.global_cpu_usage(),
                        cores: sys.cpus().len().max(1),
                        sys_mem_used: sys.used_memory(),
                        sys_mem_total: sys.total_memory(),
                        own: own_pid.map(stat).unwrap_or_default(),
                        per_pid: w2.lock().iter().map(|p| (*p, stat(Pid::from_u32(*p)))).collect(),
                        ..Default::default()
                    };
                    #[cfg(windows)]
                    if let Some(g) = &gpu_sample {
                        // The card's own figure when the driver gives one; Windows' per-engine counters
                        // (g.total) read far higher than the card is really busy.
                        snap.gpu = Some(nvml.as_ref().and_then(|n| n.percent()).unwrap_or(g.total));
                        snap.vram_used = Some(g.vram_total);
                    }
                    if tick.is_multiple_of(5) {
                        disks.refresh(true);
                    }
                    if let Some(p) = d2.lock().as_ref() {
                        snap.disk_free = disk_free(&disks, p);
                    }
                    *s2.lock() = snap;
                    tick += 1;
                    std::thread::sleep(Duration::from_millis(1000));
                }
            })
            .expect("spawn sysmon");
        Self { snapshot, watched, disk_path, stop, thread: Some(thread) }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().clone()
    }

    /// Processes to sample (helper processes of sources and outputs).
    pub fn set_watched(&self, pids: impl IntoIterator<Item = u32>) {
        *self.watched.lock() = pids.into_iter().collect();
    }

    /// Folder whose disk's free space is reported (the recordings folder).
    pub fn set_disk_path(&self, p: &Path) {
        *self.disk_path.lock() = Some(p.to_path_buf());
    }
}

impl Drop for SystemMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Parses a "GPU Engine" counter instance name, e.g.
/// `pid_1234_luid_0x00000000_0x0000C5A7_phys_0_eng_0_engtype_3D`.
pub fn parse_gpu_instance(name: &str) -> Option<(u32, String)> {
    let pid = name.strip_prefix("pid_")?.split('_').next()?.parse().ok()?;
    let engtype = name.rsplit_once("engtype_").map(|(_, e)| e.to_string()).unwrap_or_default();
    Some((pid, engtype))
}

/// Combines per-instance utilisation into per-process and system GPU %, the way Task Manager does: an
/// engine is one hardware queue (an adapter plus an engine index), the processes using it add up on
/// that engine, and the figure reported is the busiest single engine. Adding up different engines
/// (a card has many 3D queues, and every adapter has its own) would read far above what is busy.
pub fn aggregate_gpu(samples: &[(String, f64)]) -> (HashMap<u32, f32>, f32) {
    aggregate_gpu_on(samples, None)
}

/// The adapter an instance name belongs to, e.g. `luid_0x00000000_0x0000F1B5`.
pub fn luid_of(name: &str) -> Option<String> {
    let rest = &name[name.find("luid_")?..];
    let mut parts = rest.split('_');
    Some(format!("{}_{}_{}", parts.next()?, parts.next()?, parts.next()?))
}

/// [`aggregate_gpu`] counting only `adapter`'s engines (all adapters when `None`): a machine also lists
/// the software "Basic Render Driver" and any integrated GPU, whose load isn't the graphics card's.
pub fn aggregate_gpu_on(samples: &[(String, f64)], adapter: Option<&str>) -> (HashMap<u32, f32>, f32) {
    let mut per_pid: HashMap<u32, HashMap<String, f64>> = HashMap::new();
    let mut per_engine: HashMap<String, f64> = HashMap::new();
    for (name, v) in samples {
        let Some((pid, _)) = parse_gpu_instance(name) else { continue };
        if adapter.is_some_and(|a| luid_of(name).as_deref() != Some(a)) {
            continue;
        }
        // Everything after "pid_<n>_" names the engine.
        let engine = name.splitn(3, '_').nth(2).unwrap_or("").to_string();
        *per_pid.entry(pid).or_default().entry(engine.clone()).or_default() += v;
        *per_engine.entry(engine).or_default() += v;
    }
    let per_pid = per_pid.into_iter().map(|(pid, e)| (pid, e.values().cloned().fold(0.0, f64::max).min(100.0) as f32)).collect();
    let total = per_engine.values().cloned().fold(0.0, f64::max).min(100.0) as f32;
    (per_pid, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_instance_parsing_and_aggregation() {
        assert_eq!(
            parse_gpu_instance("pid_1234_luid_0x00000000_0x0000C5A7_phys_0_eng_0_engtype_3D"),
            Some((1234, "3D".into()))
        );
        let samples = vec![
            ("pid_10_luid_0x0_0x1_phys_0_eng_0_engtype_3D".to_string(), 20.0),
            ("pid_10_luid_0x0_0x1_phys_0_eng_1_engtype_3D".to_string(), 5.0),
            ("pid_10_luid_0x0_0x1_phys_0_eng_3_engtype_VideoEncode".to_string(), 40.0),
            ("pid_22_luid_0x0_0x1_phys_0_eng_0_engtype_3D".to_string(), 30.0),
        ];
        // Many queues each lightly used must not add up to a busy GPU.
        let spread: Vec<(String, f64)> = (0..8).map(|i| (format!("pid_7_luid_0x0_0x1_phys_0_eng_{i}_engtype_3D"), 8.0)).collect();
        let (spread_pid, spread_total) = aggregate_gpu(&spread);
        assert_eq!((spread_pid[&7], spread_total), (8.0, 8.0));
        // A busy software adapter doesn't count towards the graphics card.
        let two = vec![
            ("pid_1_luid_0x0_0xAAAA_phys_0_eng_0_engtype_3D".to_string(), 69.0),
            ("pid_2_luid_0x0_0xBBBB_phys_0_eng_0_engtype_3D".to_string(), 3.0),
        ];
        assert_eq!(luid_of(&two[0].0).as_deref(), Some("luid_0x0_0xAAAA"));
        assert_eq!(aggregate_gpu_on(&two, Some("luid_0x0_0xBBBB")).1, 3.0);
        let (per_pid, total) = aggregate_gpu(&samples);
        assert_eq!(per_pid[&10], 40.0, "busiest engine for pid 10 (encoder)");
        assert_eq!(per_pid[&22], 30.0);
        assert_eq!(total, 50.0, "3D engine 0 carries 20+30 from two processes; engine 1's 5 is a different queue");
    }

    #[test]
    fn samples_own_process() {
        let m = SystemMonitor::start();
        m.set_disk_path(&std::env::temp_dir());
        // Burn a little CPU so there's something to see.
        let end = std::time::Instant::now() + Duration::from_millis(2300);
        let mut x = 0u64;
        while std::time::Instant::now() < end {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let s = m.snapshot();
        assert!(s.sys_mem_total > 0);
        assert!(s.own.mem_bytes > 0, "{s:?}");
        assert!(s.own.cpu > 5.0, "own CPU {}", s.own.cpu);
        assert!(s.disk_free.unwrap_or(0) > 0);
    }
}
