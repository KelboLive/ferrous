//! Windows Graphics Capture (Windows 10 1903+), the same API OBS uses for
//! its "Windows 10 (1903 and up)" capture method.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use ferrous_core::VideoFrame;
use parking_lot::Mutex;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::gpu_ring::Ring;
use crate::{MonitorInfo, Source, WindowInfo};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The name of the program that owns a window, e.g. `cod.exe`. `Window::process_name` asks Windows for
/// rights that anti-cheat protected and elevated games refuse, so those games never showed up; the
/// limited-information request below is allowed for them.
pub(crate) fn process_name_of(w: &Window) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    if let Ok(n) = w.process_name() {
        return Some(n);
    }
    let pid = w.process_id().ok()?;
    // SAFETY: plain Win32 calls; the handle is closed before returning and the buffer outlives the call.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
        path.rsplit(['\\', '/']).next().filter(|n| !n.is_empty()).map(str::to_string)
    }
}

#[derive(Clone)]
struct Shared {
    latest: Arc<Mutex<Option<VideoFrame>>>,
    /// The newest frame as a shared GPU texture (GPU-resident mode only).
    latest_gpu: Arc<Mutex<Option<VideoFrame>>>,
    alive: Arc<AtomicBool>,
    /// GPU-resident mode was asked for and has not failed.
    gpu: Arc<AtomicBool>,
    /// When the CPU frame was last asked for, in ms since `epoch`; in GPU mode the CPU copy only runs
    /// while someone wants it (a filter or consumer the GPU path can't serve).
    cpu_wanted: Arc<AtomicU64>,
    epoch: std::time::Instant,
}

impl Shared {
    fn cpu_needed(&self) -> bool {
        if !self.gpu.load(Ordering::Relaxed) {
            return true;
        }
        let last = self.cpu_wanted.load(Ordering::Relaxed);
        last != 0 && (self.epoch.elapsed().as_millis() as u64).saturating_sub(last) < 2000
    }
}

struct Handler {
    shared: Shared,
    pool: ferrous_core::FramePool,
    ring: Option<Ring>,
}

impl Handler {
    /// Copies the captured texture into the shared ring (GPU to GPU) and publishes it.
    fn publish_gpu(&mut self, frame: &Frame) -> Result<(), BoxError> {
        let (w, h) = (frame.width(), frame.height());
        if self.ring.as_ref().is_none_or(|r| r.size() != (w, h)) {
            self.ring = Some(Ring::new(frame.device(), w, h)?);
        }
        let ring = self.ring.as_mut().expect("ring just made");
        if let Some(f) = ring.publish(frame.device_context(), frame.as_raw_texture())? {
            *self.shared.latest_gpu.lock() = Some(f);
        }
        Ok(())
    }
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Shared;
    type Error = BoxError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { shared: ctx.flags, pool: ferrous_core::FramePool::default(), ring: None })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.shared.gpu.load(Ordering::Relaxed) {
            if let Err(e) = self.publish_gpu(frame) {
                // Whatever went wrong, the ordinary CPU path still works: use it from now on.
                log::warn!("GPU capture failed, using the CPU path: {e}");
                self.shared.gpu.store(false, Ordering::Relaxed);
                *self.shared.latest_gpu.lock() = None;
                self.ring = None;
            }
        }
        if !self.shared.cpu_needed() {
            return Ok(());
        }
        let mut buffer = frame.buffer()?;
        let (w, h) = (buffer.width(), buffer.height());
        let (row, pitch) = (w as usize * 4, buffer.row_pitch() as usize);
        // One copy, row by row from the mapped texture (dropping any row
        // padding) into a recycled buffer: no scratch copy, no fresh 8 MB
        // allocation per frame.
        let (mut pixels, _) = self.pool.take(row * h as usize);
        let raw = buffer.as_raw_buffer();
        for (y, dst) in pixels.chunks_exact_mut(row).enumerate() {
            dst.copy_from_slice(&raw[y * pitch..y * pitch + row]);
        }
        let f = VideoFrame::new(w, h, pixels);
        self.pool.track(&f);
        *self.shared.latest.lock() = Some(f);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.shared.alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}

pub struct WgcSource {
    shared: Shared,
    control: Option<CaptureControl<Handler, BoxError>>,
}

/// Windows delivers a frame per display refresh (144 a second on a 144 Hz
/// monitor) however slow the canvas is, and every one costs a copy. Asking
/// for no more than ~1.3x the canvas rate keeps all the frames a canvas can
/// use, at any rate: at 144 fps the limit sits above the monitor's own and
/// changes nothing. Needs Windows 11 22H2; older versions deliver every frame.
fn min_interval(canvas_fps: u32) -> MinimumUpdateIntervalSettings {
    if canvas_fps == 0 || !GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false) {
        return MinimumUpdateIntervalSettings::Default;
    }
    MinimumUpdateIntervalSettings::Custom(std::time::Duration::from_secs_f64(0.75 / f64::from(canvas_fps)))
}

fn cursor(on: bool) -> CursorCaptureSettings {
    if !GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false) {
        return CursorCaptureSettings::Default;
    }
    if on {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    }
}

impl WgcSource {
    fn start<T>(item: T, capture_cursor: bool, canvas_fps: u32) -> anyhow::Result<Self>
    where
        T: TryInto<windows_capture::settings::GraphicsCaptureItemType> + Send + 'static,
    {
        let shared = Shared {
            latest: Arc::new(Mutex::new(None)),
            latest_gpu: Arc::new(Mutex::new(None)),
            alive: Arc::new(AtomicBool::new(true)),
            gpu: Arc::new(AtomicBool::new(crate::gpu_capture())),
            cpu_wanted: Arc::new(AtomicU64::new(0)),
            epoch: std::time::Instant::now(),
        };
        let settings = Settings::new(
            item,
            cursor(capture_cursor),
            // Hide the yellow capture border where Windows allows it (Win 11).
            if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
                DrawBorderSettings::WithoutBorder
            } else {
                DrawBorderSettings::Default
            },
            SecondaryWindowSettings::Default,
            min_interval(canvas_fps),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            shared.clone(),
        );
        let control = Handler::start_free_threaded(settings)
            .map_err(|e| anyhow::anyhow!("failed to start capture: {e}"))?;
        Ok(Self { shared, control: Some(control) })
    }

    /// `index` is zero-based, in enumeration order.
    pub fn monitor(index: usize, capture_cursor: bool, canvas_fps: u32) -> anyhow::Result<Self> {
        let monitors = Monitor::enumerate().map_err(|e| anyhow::anyhow!("{e}"))?;
        let monitor = monitors
            .get(index)
            .copied()
            .or_else(|| Monitor::primary().ok())
            .ok_or_else(|| anyhow::anyhow!("no monitor found"))?;
        Self::start(monitor, capture_cursor, canvas_fps)
    }

    pub fn window(title_contains: &str, capture_cursor: bool, canvas_fps: u32) -> anyhow::Result<Self> {
        if title_contains.trim().is_empty() {
            anyhow::bail!("no window selected");
        }
        let window = Window::from_contains_name(title_contains)
            .map_err(|_| anyhow::anyhow!("window \"{title_contains}\" not found"))?;
        Self::start(window, capture_cursor, canvas_fps)
    }
}

impl Source for WgcSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        if self.shared.gpu.load(Ordering::Relaxed) {
            // A consumer that can't use the GPU frame: start (and keep) making CPU frames.
            self.shared.cpu_wanted.store((self.shared.epoch.elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
        }
        self.shared.latest.lock().clone()
    }

    fn latest_gpu_frame(&mut self) -> Option<VideoFrame> {
        if !self.shared.gpu.load(Ordering::Relaxed) {
            return None;
        }
        self.shared.latest_gpu.lock().clone()
    }

    fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
            && self.control.as_ref().is_some_and(|c| !c.is_finished())
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            if let Err(e) = control.stop() {
                log::warn!("capture stop: {e}");
            }
        }
    }
}

/// Programs that are never "the game" when detecting automatically.
// (ApplicationFrameHost is not listed: Game Pass / Microsoft Store games run in its windows, and a window that
// covers the whole monitor is not the Settings app anyway.)
const NOT_GAMES: [&str; 7] = ["explorer.exe", "searchhost.exe", "startmenuexperiencehost.exe", "shellexperiencehost.exe", "textinputhost.exe", "lockapp.exe", "dwm.exe"];

/// A few pixels of slack: borderless games are often a pixel or two short of the monitor.
const FULLSCREEN_SLACK: i32 = 4;

fn covers_monitor(w: &Window) -> bool {
    let (Some(m), Ok(r)) = (w.monitor(), w.rect()) else { return false };
    let (mw, mh) = (m.width().unwrap_or(0) as i32, m.height().unwrap_or(0) as i32);
    mw > 0 && (r.right - r.left) >= mw - FULLSCREEN_SLACK && (r.bottom - r.top) >= mh - FULLSCREEN_SLACK
}

/// For the log when nothing qualifies: the biggest visible windows and the size of the monitor they are on.
fn describe_big_windows() -> String {
    let mut v: Vec<(i64, String)> = Window::enumerate()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.is_valid())
        .filter_map(|w| {
            let area = w.width().ok()? as i64 * w.height().ok()? as i64;
            let mon = w.monitor().and_then(|m| Some((m.width().ok()?, m.height().ok()?))).unwrap_or((0, 0));
            Some((area, format!("{} {}x{} (monitor {}x{})", process_name_of(&w).unwrap_or_default(), w.width().unwrap_or(0), w.height().unwrap_or(0), mon.0, mon.1)))
        })
        .collect();
    v.sort_by_key(|(a, _)| std::cmp::Reverse(*a));
    v.into_iter().take(4).map(|(_, d)| d).collect::<Vec<_>>().join("; ")
}

/// The window to capture: the named program's biggest window, or (automatic)
/// the full-screen or borderless-full-screen app in front, else any other one.
fn find_game(program: &str) -> Option<Window> {
    let own = std::process::id();
    let usable = |w: &Window| w.is_valid() && w.process_id().is_ok_and(|p| p != own) && w.title().is_ok_and(|t| !t.trim().is_empty());
    if !program.trim().is_empty() {
        let want = program.trim().to_lowercase();
        return Window::enumerate()
            .unwrap_or_default()
            .into_iter()
            .filter(|w| usable(w) && process_name_of(w).is_some_and(|p| p.to_lowercase() == want))
            .max_by_key(|w| w.width().unwrap_or(0) as i64 * w.height().unwrap_or(0) as i64);
    }
    let not_shell = |w: &Window| process_name_of(w).is_none_or(|p| !NOT_GAMES.contains(&p.to_lowercase().as_str()));
    let front = Window::foreground().ok().filter(|w| usable(w) && not_shell(w) && covers_monitor(w));
    front.or_else(|| Window::enumerate().unwrap_or_default().into_iter().find(|w| usable(w) && not_shell(w) && covers_monitor(w)))
}

/// Game Capture: finds the game itself and follows it, restarting the
/// capture when the game closes, restarts or a different one comes up.
pub struct GameSource {
    program: String,
    cursor: bool,
    fps: u32,
    inner: Option<(WgcSource, u32, String)>,
    last_check: std::time::Instant,
    last_report: Option<std::time::Instant>,
}

impl GameSource {
    pub fn new(program: &str, cursor: bool, fps: u32) -> Self {
        Self { program: program.to_string(), cursor, fps, inner: None, last_check: std::time::Instant::now() - std::time::Duration::from_secs(10), last_report: None }
    }

    /// Looks for the game about once a second (cheap, but not every frame).
    fn follow(&mut self) {
        let dead = self.inner.as_ref().is_some_and(|(s, ..)| !s.is_alive());
        if !dead && self.last_check.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last_check = std::time::Instant::now();
        if dead {
            self.inner = None;
        }
        // A named program is only re-searched when nothing is captured; the
        // automatic mode also switches to a different game that comes up.
        if self.inner.is_some() && !self.program.trim().is_empty() {
            return;
        }
        let Some(w) = find_game(&self.program) else {
            // Say why now and then, so "it isn't detecting my game" can be answered from the log.
            if self.last_report.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(30)) {
                self.last_report = Some(std::time::Instant::now());
                if self.program.trim().is_empty() {
                    log::info!("game capture: no full-screen window found; biggest windows: {}", describe_big_windows());
                } else {
                    log::info!("game capture: no window from {} found; biggest windows: {}", self.program, describe_big_windows());
                }
            }
            return;
        };
        let ident = (w.process_id().unwrap_or(0), w.title().unwrap_or_default());
        if self.inner.as_ref().is_some_and(|(_, pid, title)| *pid == ident.0 && *title == ident.1) {
            return;
        }
        log::info!("game capture: capturing {} \"{}\"", process_name_of(&w).unwrap_or_default(), ident.1.chars().filter(|c| !c.is_control() && !matches!(c, '\u{200b}'..='\u{200f}' | '\u{feff}')).collect::<String>());
        match WgcSource::start(w, self.cursor, self.fps) {
            Ok(s) => self.inner = Some((s, ident.0, ident.1)),
            Err(e) => log::warn!("game capture: {e}"),
        }
    }
}

impl Source for GameSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        self.follow();
        self.inner.as_mut().and_then(|(s, ..)| s.latest_frame())
    }

    fn latest_gpu_frame(&mut self) -> Option<VideoFrame> {
        self.follow();
        self.inner.as_mut().and_then(|(s, ..)| s.latest_gpu_frame())
    }

    fn is_alive(&self) -> bool {
        true // it waits for a game to appear instead of failing
    }

    fn update(&mut self, config: &ferrous_core::SourceConfig) -> bool {
        if let ferrous_core::SourceConfig::GameCapture { program, capture_cursor } = config {
            if *program != self.program || *capture_cursor != self.cursor {
                self.program = program.clone();
                self.cursor = *capture_cursor;
                self.inner = None;
                self.last_check = std::time::Instant::now() - std::time::Duration::from_secs(10);
            }
            return true;
        }
        false
    }
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    Monitor::enumerate()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, m)| MonitorInfo {
            index,
            name: m
                .name()
                .unwrap_or_else(|_| m.device_name().unwrap_or_else(|_| format!("Display {}", index + 1))),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
        })
        .collect()
}

pub fn list_windows() -> Vec<WindowInfo> {
    Window::enumerate()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.is_valid())
        .filter_map(|w| {
            let title = w.title().ok()?;
            if title.trim().is_empty() {
                return None;
            }
            Some(WindowInfo { title, process: process_name_of(&w).unwrap_or_default() })
        })
        .collect()
}
