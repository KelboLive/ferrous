//! Receiving side: connects (by discovered name or address), decodes, and
//! reconnects automatically.

use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ferrous_core::VideoFrame;
use parking_lot::Mutex;

use crate::codec::{self, LosslessDecoder};
use crate::discovery;
use crate::protocol::{self, kind, Cipher, CodecKind, Hello};

pub type AudioSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

#[derive(Default)]
pub struct ReceiverStats {
    pub connected: AtomicBool,
    pub sender_name: Mutex<String>,
    pub codec: Mutex<String>,
    pub kbps: AtomicU32,
    pub fps: AtomicU32,
    /// One-way latency estimate in ms (capture timestamp to decoded).
    pub latency_ms: AtomicU32,
    pub rtt_ms: AtomicU32,
    pub frames: AtomicU64,
    pub decode_errors: AtomicU64,
    pub error: Mutex<Option<String>>,
}

/// Stats of live receivers by target, for the UI.
type Registry = Mutex<Vec<(String, std::sync::Weak<ReceiverStats>)>>;

fn registry() -> &'static Registry {
    static R: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// Stats for the receiver connected to `target`, if one is running.
pub fn stats_for(target: &str) -> Option<Arc<ReceiverStats>> {
    let mut r = registry().lock();
    r.retain(|(_, w)| w.strong_count() > 0);
    r.iter().find(|(t, _)| t == target).and_then(|(_, w)| w.upgrade())
}

/// Decoded frames waiting for their moment, as OBS's async sources (which
/// obs-teleport feeds) do it. Each frame has a due time on a local clock;
/// every render tick shows the newest frame that is due. Network and decode
/// jitter is absorbed by a small delay, so frames come out at the sender's
/// even cadence instead of in bursts.
pub(crate) struct Timeline {
    epoch: Instant,
    queue: std::collections::VecDeque<(u64, Option<VideoFrame>)>,
    current: Option<VideoFrame>,
}

/// More waiting than this means nobody is taking frames (the source isn't
/// on screen): drop the oldest rather than hold seconds of video.
const TIMELINE_MAX: usize = 240;

impl Timeline {
    fn new() -> Self {
        Self { epoch: Instant::now(), queue: Default::default(), current: None }
    }

    /// Microseconds on the local presentation clock.
    pub(crate) fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// `frame: None` repeats the picture already showing.
    fn push(&mut self, due: u64, frame: Option<VideoFrame>) {
        self.queue.push_back((due, frame));
        while self.queue.len() > TIMELINE_MAX {
            if let Some((_, Some(f))) = self.queue.pop_front() {
                self.current = Some(f);
            }
        }
    }

    /// The newest frame whose time has come.
    fn take_due(&mut self) -> Option<VideoFrame> {
        let now = self.now_us();
        while self.queue.front().is_some_and(|(due, _)| *due <= now) {
            if let Some((_, Some(f))) = self.queue.pop_front() {
                self.current = Some(f);
            }
        }
        self.current.clone()
    }
}

pub struct LinkReceiver {
    pub stats: Arc<ReceiverStats>,
    timeline: Arc<Mutex<Timeline>>,
    stop: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<JoinHandle<()>>,
}

/// `target` is a discovered sender name, an IP address or hostname
/// (default port), or `host:port`.
fn resolve(target: &str) -> Option<String> {
    let t = target.trim();
    if let Some(a) = discovery::discovered().into_iter().find(|a| a.name == t) {
        return Some(a.address());
    }
    if let Some(addr) = discovery::last_known_address(t) {
        return Some(addr);
    }
    if t.contains(':') {
        return Some(t.to_string());
    }
    // A sender's display name ("Gaming PC") that hasn't been heard yet is
    // not a hostname: wait for it rather than failing a DNS lookup.
    if t.is_empty() || !looks_like_host(t) {
        return None;
    }
    Some(format!("{t}:{}", crate::DEFAULT_PORT))
}

/// Turns socket errors into advice; anything else is shown as-is.
fn explain(addr: &str, e: &anyhow::Error) -> String {
    use std::io::ErrorKind::*;
    match e.downcast_ref::<std::io::Error>().map(|io| io.kind()) {
        Some(ConnectionRefused) => format!(
            "{addr} refused the connection: no Ferrous Link output is listening there. On the sending PC, check the output is \
             enabled, and use an address listed under Settings → Ferrous Link → \"Receivers use\"."
        ),
        Some(TimedOut | HostUnreachable | NetworkUnreachable) => format!(
            "Can't reach {addr}. Check both PCs are on the same network, and allow Ferrous through Windows Firewall \
             on the sending PC."
        ),
        _ => e.to_string(),
    }
}

/// IP addresses and DNS/computer names: letters, digits, '-' and '.'.
fn looks_like_host(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

impl LinkReceiver {
    pub fn start(target: &str, pin: &str, ffmpeg: PathBuf, audio: Option<AudioSink>) -> Self {
        discovery::start_listening();
        let stats = Arc::new(ReceiverStats::default());
        let timeline = Arc::new(Mutex::new(Timeline::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stream: Arc<Mutex<Option<TcpStream>>> = Arc::default();
        let (t, p) = (target.to_string(), pin.to_string());
        let (s2, l2, st2, sm2) = (stats.clone(), timeline.clone(), stop.clone(), stream.clone());
        let thread = thread::Builder::new()
            .name("link-receive".into())
            .spawn(move || {
                // Reconnect loop.
                while !st2.load(Ordering::Relaxed) {
                    match resolve(&t) {
                        Some(addr) => {
                            if let Err(e) = session(&addr, &p, &ffmpeg, audio.clone(), &s2, &l2, &st2, &sm2) {
                                *s2.error.lock() = Some(explain(&addr, &e));
                            }
                        }
                        None => {
                            *s2.error.lock() = Some(format!(
                                "Looking for \"{t}\" on the network… If it doesn't appear, type the sending PC's IP address \
                                 (ipconfig → IPv4 Address) and allow Ferrous through Windows Firewall on both PCs."
                            ))
                        }
                    }
                    s2.connected.store(false, Ordering::Relaxed);
                    for _ in 0..20 {
                        if st2.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn link receiver");
        registry().lock().push((target.to_string(), Arc::downgrade(&stats)));
        Self { stats, timeline, stop, stream, thread: Some(thread) }
    }

    /// The frame to show now (called once per render tick).
    pub fn latest_frame(&self) -> Option<VideoFrame> {
        self.timeline.lock().take_due()
    }
}

impl Drop for LinkReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.stream.lock().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The PCM part of an audio message (after its 8-byte timestamp); empty if the message is too short.
fn audio_pcm(p: &[u8]) -> &[u8] {
    p.get(8..).unwrap_or(&[])
}

/// Stops the pinger thread however `session` is left, including early `?` returns.
struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Feeds an FFmpeg decoder for byte-stream (hardware codec) links. FFmpeg
/// decodes on the GPU where it can and hands back NV12 (1.5 bytes per pixel
/// through the pipe instead of 4), which Ferrous converts on all cores.
struct StreamDecoder {
    child: Child,
    stdin: ChildStdin,
}

impl StreamDecoder {
    fn start(ffmpeg: &std::path::Path, w: u32, h: u32, fps: u32, timeline: Arc<Mutex<Timeline>>, stats: Arc<ReceiverStats>) -> anyhow::Result<Self> {
        let (w, h) = ((w & !1).max(2), (h & !1).max(2));
        anyhow::ensure!(codec::frame_bytes(w, h).is_some(), "unusable picture size {w}x{h}");
        let mut child = ferrous_encode::ffmpeg::command(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0"])
            // The stream starts on PAT + PMT + keyframe, so the parameters are in the first few
            // packets; waiting to fill a bigger probe would stall a quiet scene for seconds.
            .args(["-hwaccel", "auto"])
            // One output frame per decoded frame: without passthrough, FFmpeg
            // duplicates frames to fill timestamp gaps.
            .args(["-f", "mpegts", "-i", "pipe:0", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "nv12"])
            .args(["-s", &format!("{w}x{h}"), "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        thread::Builder::new().name("link-stream-dec".into()).spawn(move || {
            let mut nv12 = vec![0u8; w as usize * h as usize * 3 / 2];
            let mut pool = ferrous_core::FramePool::default();
            // The decoder hands frames out unevenly (network bursts, decode bunching). Showing each one the
            // moment it arrives made the picture judder, so play them out one frame period apart, a frame
            // behind, and catch up (drop the backlog) if we ever fall more than a few frames behind.
            let period = 1_000_000 / u64::from(fps.max(1));
            let mut last_due: Option<u64> = None;
            loop {
                if stdout.read_exact(&mut nv12).is_err() {
                    break;
                }
                let (mut bgra, _) = pool.take(w as usize * h as usize * 4);
                codec::nv12_to_bgra(&nv12, w as usize, h as usize, &mut bgra);
                let f = VideoFrame::new(w, h, bgra);
                pool.track(&f);
                let mut t = timeline.lock();
                let ready = t.now_us();
                let due = match last_due {
                    Some(prev) if prev + period >= ready && prev + period <= ready + 4 * period => prev + period,
                    _ => ready + period,
                };
                last_due = Some(due);
                t.push(due, Some(f));
                drop(t);
                stats.frames.fetch_add(1, Ordering::Relaxed);
            }
        })?;
        Ok(Self { child, stdin })
    }
}

impl Drop for StreamDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A frame (or repeat) on its way from the network to the timeline.
struct Decoded {
    seq: u64,
    /// Sender timestamp (µs, sender's clock) and local arrival (µs, timeline clock).
    ts: u64,
    arrived: u64,
    /// The decoded picture; `None` for a repeat, or when decoding failed.
    frame: Option<VideoFrame>,
    /// "Same picture as before" (REPEAT message).
    repeat: bool,
}

/// Chooses when each frame is shown: at its sender timestamp, shifted onto
/// the local clock by the smallest transit seen recently, plus a jitter
/// delay. The delay starts at one frame and adapts: it grows when frames
/// arrive (decoded) after their time, and shrinks slowly after a clean run.
struct Pacer {
    period_us: i64,
    delay_us: i64,
    min_now: i64,
    min_prev: i64,
    window: Instant,
    late: u32,
    frames: u32,
    clean_secs: u32,
    eval: Instant,
}

const MAX_DELAY_US: i64 = 150_000;

impl Pacer {
    fn new(fps: u32) -> Self {
        let period_us = 1_000_000 / i64::from(fps.max(1));
        Self {
            period_us,
            delay_us: period_us,
            min_now: i64::MAX,
            min_prev: i64::MAX,
            window: Instant::now(),
            late: 0,
            frames: 0,
            clean_secs: 0,
            eval: Instant::now(),
        }
    }

    /// Local due time for a frame sent at `ts` that arrived at `arrived`
    /// and is ready (decoded) at `ready`.
    fn due(&mut self, ts: u64, arrived: u64, ready: u64) -> u64 {
        let transit = arrived as i64 - ts as i64;
        self.min_now = self.min_now.min(transit);
        if self.window.elapsed() >= Duration::from_secs(2) {
            // Sliding minimum over 2–4 s, so clock drift is followed.
            self.min_prev = self.min_now;
            self.min_now = transit;
            self.window = Instant::now();
        }
        let base = self.min_now.min(self.min_prev);
        let due = ts as i64 + base + self.delay_us;
        self.frames += 1;
        let late = (ready as i64) > due;
        if late {
            self.late += 1;
        }
        if self.eval.elapsed() >= Duration::from_secs(1) {
            self.eval = Instant::now();
            if self.late * 100 > self.frames || self.late >= 3 {
                self.delay_us = (self.delay_us + self.period_us / 2).min(MAX_DELAY_US);
                self.clean_secs = 0;
            } else {
                self.clean_secs += 1;
                if self.clean_secs >= 5 && self.delay_us > self.period_us / 2 {
                    self.delay_us -= 1000;
                }
            }
            self.late = 0;
            self.frames = 0;
        }
        due.max(ready as i64) as u64
    }
}

#[allow(clippy::too_many_arguments)]
fn session(
    addr: &str,
    pin: &str,
    ffmpeg: &std::path::Path,
    audio: Option<AudioSink>,
    stats: &Arc<ReceiverStats>,
    timeline: &Arc<Mutex<Timeline>>,
    stop: &AtomicBool,
    stream_slot: &Mutex<Option<TcpStream>>,
) -> anyhow::Result<()> {
    let sock = addr.to_socket_addrs()?.next().ok_or_else(|| anyhow::anyhow!("can't resolve {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&sock, Duration::from_secs(3))?;
    protocol::tune_socket(&stream);
    *stream_slot.lock() = Some(stream.try_clone()?);
    let mut reader = BufReader::with_capacity(1024 * 1024, stream.try_clone()?);
    let hello: Hello = serde_json::from_slice(&protocol::read_raw(&mut reader)?)?;
    if hello.proto != protocol::PROTO_VERSION {
        anyhow::bail!(
            "\"{}\" runs a different Ferrous version (link protocol {}, this one speaks {}). Install the same build on both PCs.",
            hello.name,
            hello.proto,
            protocol::PROTO_VERSION
        );
    }
    if codec::frame_bytes(hello.width, hello.height).is_none() || hello.fps == 0 || hello.fps > 1000 {
        anyhow::bail!("\"{}\" announced a picture size this PC can't use ({}x{} at {} fps)", hello.name, hello.width, hello.height, hello.fps);
    }
    if hello.encrypted && pin.is_empty() {
        anyhow::bail!("\"{}\" needs a PIN", hello.name);
    }
    let key = hello.encrypted.then(|| protocol::derive_key(pin, &hello.salt));
    let mut rx = Cipher::new(key, 1);
    let tx = Arc::new(Mutex::new(Cipher::new(key, 2)));
    {
        let sealed = tx.lock().seal(protocol::frame(kind::AUTH, b"{}"))?;
        protocol::write_raw(&mut stream, &sealed)?;
    }
    *stats.sender_name.lock() = hello.name.clone();
    *stats.codec.lock() = format!("{:?}", hello.codec);
    *stats.error.lock() = None;

    // Pinger: one round trip per second for RTT and clock offset (µs).
    let offset = Arc::new(Mutex::new(0i64));
    let alive = Arc::new(AtomicBool::new(true));
    let _alive_guard = StopOnDrop(alive.clone());
    {
        let (tx, alive) = (tx.clone(), alive.clone());
        let mut w = stream.try_clone()?;
        thread::spawn(move || {
            while alive.load(Ordering::Relaxed) {
                let Ok(sealed) = tx.lock().seal(protocol::frame(kind::PING, &protocol::now_us().to_be_bytes())) else { break };
                if protocol::write_raw(&mut w, &sealed).is_err() {
                    break;
                }
                let _ = w.flush();
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

    // Decoding happens off the socket thread, on several workers for JPEG
    // (each frame whole, as obs-teleport does) or one for lossless (deltas
    // must go in order). A presenter puts results back in order and gives
    // each its due time.
    let workers = match hello.codec {
        CodecKind::Jpeg => thread::available_parallelism().map_or(4, |n| n.get() / 2).clamp(2, 8),
        _ => 1,
    };
    let (job_tx, job_rx) = crossbeam_channel::bounded::<(u64, u64, Vec<u8>)>(workers * 2);
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Decoded>();
    let mut decoders = Vec::new();
    if hello.codec != CodecKind::Stream {
        for i in 0..workers {
            let (job_rx, done_tx, codec_kind) = (job_rx.clone(), done_tx.clone(), hello.codec);
            decoders.push(thread::Builder::new().name(format!("link-decode-{i}")).spawn(move || {
                let mut jpeg = codec::JpegDecoder::new().ok();
                let mut lossless = LosslessDecoder::default();
                for (seq, arrived, body) in job_rx {
                    let parsed = codec::parse_header(&body);
                    let frame = parsed.and_then(|(_, w, h, data)| match codec_kind {
                        CodecKind::Jpeg => jpeg.as_mut()?.decode(data, w, h),
                        CodecKind::Lossless => lossless.decode(data, w, h),
                        CodecKind::Stream => None,
                    });
                    let ts = parsed.map_or(0, |p| p.0);
                    if done_tx.send(Decoded { seq, ts, arrived, frame, repeat: false }).is_err() {
                        break;
                    }
                }
            })?);
        }
    }
    drop((job_rx, done_tx.clone()));
    let presenter = {
        let (timeline, stats, offset, fps) = (timeline.clone(), stats.clone(), offset.clone(), hello.fps);
        thread::Builder::new().name("link-present".into()).spawn(move || {
            let mut pacer = Pacer::new(fps);
            let mut pending = std::collections::BTreeMap::new();
            let mut next = 0u64;
            for d in done_rx {
                pending.insert(d.seq, d);
                while let Some(d) = pending.remove(&next) {
                    next += 1;
                    if d.frame.is_none() && !d.repeat {
                        stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let mut t = timeline.lock();
                    let ready = t.now_us();
                    let due = pacer.due(d.ts, d.arrived, ready);
                    // A repeat only means something once a picture is showing.
                    if d.repeat && t.current.is_none() && t.queue.is_empty() {
                        continue;
                    }
                    t.push(due, d.frame);
                    drop(t);
                    stats.frames.fetch_add(1, Ordering::Relaxed);
                    // Capture to on-screen: network + decode + jitter delay.
                    let shown_wall = protocol::now_us() as i64 + (due as i64 - ready as i64);
                    let lat = shown_wall + *offset.lock() - d.ts as i64;
                    stats.latency_ms.store((lat.max(0) / 1000) as u32, Ordering::Relaxed);
                }
            }
        })?
    };
    let mut seq = 0u64;
    let mut stream_dec: Option<StreamDecoder> = None;
    let mut bytes_window = (Instant::now(), 0u64, stats.frames.load(Ordering::Relaxed));
    let mut first = true;
    // Hardware stream: bytes arriving but no picture coming out means the
    // decoder never found the stream's parameters; reconnecting joins afresh.
    let mut stream_watch = (Instant::now(), stats.frames.load(Ordering::Relaxed));
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let raw = match protocol::read_raw(&mut reader) {
            Ok(r) => r,
            Err(e) => break Err(e),
        };
        let arrived = timeline.lock().now_us();
        bytes_window.1 += raw.len() as u64 + 4;
        let body = match rx.open(raw) {
            Ok(b) => b,
            Err(e) => break Err(if first { anyhow::anyhow!("wrong PIN for \"{}\"", hello.name) } else { e }),
        };
        if first {
            first = false;
            stats.connected.store(true, Ordering::Relaxed);
        }
        // A zero-length message is legal framing: it must not index past the end.
        let Some((&k, p)) = body.split_first() else { continue };
        match k {
            kind::VIDEO => {
                let job = (seq, arrived, p.to_vec());
                if hello.codec == CodecKind::Lossless {
                    // Deltas can't be skipped.
                    if job_tx.send(job).is_err() {
                        break Err(anyhow::anyhow!("decoder stopped"));
                    }
                    seq += 1;
                } else {
                    match job_tx.try_send(job) {
                        Ok(()) => seq += 1,
                        // Every decoder busy and a backlog waiting: this PC
                        // can't decode this fast; skip rather than lag.
                        Err(crossbeam_channel::TrySendError::Full(_)) => {
                            stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => break Err(anyhow::anyhow!("decoder stopped")),
                    }
                }
            }
            kind::REPEAT if p.len() >= 8 => {
                // Same picture as before, at this frame's time.
                let ts = u64::from_be_bytes(p[0..8].try_into().unwrap());
                if done_tx.send(Decoded { seq, ts, arrived, frame: None, repeat: true }).is_err() {
                    break Err(anyhow::anyhow!("presenter stopped"));
                }
                seq += 1;
            }
            kind::STREAM => {
                if p.len() <= 8 {
                    continue;
                }
                if stream_dec.is_none() {
                    stream_dec = Some(StreamDecoder::start(ffmpeg, hello.width, hello.height, hello.fps, timeline.clone(), stats.clone())?);
                }
                if let Some(d) = &mut stream_dec {
                    if d.stdin.write_all(&p[8..]).is_err() {
                        break Err(anyhow::anyhow!("decoder stopped"));
                    }
                }
                let frames = stats.frames.load(Ordering::Relaxed);
                if frames != stream_watch.1 {
                    stream_watch = (Instant::now(), frames);
                } else if stream_watch.0.elapsed() > Duration::from_secs(5) {
                    break Err(anyhow::anyhow!("no picture from the hardware stream; reconnecting"));
                }
            }
            kind::AUDIO => {
                if let Some(sink) = &audio {
                    let samples: Vec<f32> = audio_pcm(p).chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
                    sink(&samples);
                }
            }
            kind::PONG if p.len() >= 16 => {
                let sent = u64::from_be_bytes(p[0..8].try_into().unwrap());
                let remote = u64::from_be_bytes(p[8..16].try_into().unwrap());
                let now = protocol::now_us();
                let rtt = now.saturating_sub(sent);
                stats.rtt_ms.store((rtt / 1000) as u32, Ordering::Relaxed);
                // remote clock at `now` ≈ remote + rtt/2
                *offset.lock() = remote as i64 + (rtt / 2) as i64 - now as i64;
            }
            _ => {}
        }
        if bytes_window.0.elapsed() >= Duration::from_secs(1) {
            let secs = bytes_window.0.elapsed().as_secs_f64();
            stats.kbps.store((bytes_window.1 as f64 * 8.0 / 1000.0 / secs) as u32, Ordering::Relaxed);
            let frames = stats.frames.load(Ordering::Relaxed);
            stats.fps.store(((frames - bytes_window.2) as f64 / secs).round() as u32, Ordering::Relaxed);
            bytes_window = (Instant::now(), 0, frames);
        }
    };
    alive.store(false, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);
    drop((job_tx, done_tx));
    for d in decoders {
        let _ = d.join();
    }
    let _ = presenter.join();
    timeline.lock().queue.clear();
    result
}

#[cfg(test)]
mod hostile_input_tests {
    use super::*;

    #[test]
    fn short_audio_messages_are_empty_not_fatal() {
        assert!(audio_pcm(&[]).is_empty());
        assert!(audio_pcm(&[0; 7]).is_empty());
        assert_eq!(audio_pcm(&[0; 12]).len(), 4);
    }

    #[test]
    fn stopping_the_session_stops_the_pinger() {
        let alive = Arc::new(AtomicBool::new(true));
        drop(StopOnDrop(alive.clone()));
        assert!(!alive.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn display_names_wait_instead_of_dns_lookup() {
        assert_eq!(resolve("Gaming PC"), None, "a sender name with a space is not a hostname");
        assert_eq!(resolve("192.168.1.20").as_deref(), Some("192.168.1.20:47778"));
        assert_eq!(resolve("gaming-pc").as_deref(), Some("gaming-pc:47778"));
        assert_eq!(resolve("10.0.0.5:47779").as_deref(), Some("10.0.0.5:47779"));
        assert_eq!(resolve("  "), None);
    }
}
