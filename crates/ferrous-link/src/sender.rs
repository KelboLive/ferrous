//! Sending side: accepts receivers, encodes frames once, fans them out.

use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use ferrous_core::{EncoderSettings, LinkCodec, RateControl, VideoFrame};
use parking_lot::Mutex;

use crate::codec::{self, LosslessEncoder};
use crate::discovery::{self, Announcement};
use crate::protocol::{self, kind, Cipher, CodecKind, Hello};

#[derive(Clone, Debug)]
pub struct SenderConfig {
    pub name: String,
    pub codec: LinkCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub port: u16,
    pub pin: String,
    pub audio: bool,
    pub ffmpeg: PathBuf,
}

/// Live numbers for the UI and the stats dock.
#[derive(Default)]
pub struct SenderStats {
    pub clients: AtomicU32,
    pub frames_sent: AtomicU64,
    /// Frames a receiver couldn't take in time (per receiver, summed).
    pub frames_dropped: AtomicU64,
    /// Frames skipped because the encoder was still busy.
    pub frames_skipped: AtomicU64,
    pub bytes_sent: AtomicU64,
    /// Current JPEG quality (adaptive).
    pub quality: AtomicU32,
    pub kbps: AtomicU32,
}

/// One receiver's outgoing queues.
///
/// Video goes through a one-frame slot: a new frame replaces one that hasn't
/// been sent yet, so a receiver that falls behind gets the newest picture
/// instead of a growing backlog (that backlog was what made latency climb to
/// hundreds of milliseconds). Everything else (audio, pongs, the hardware
/// codec's byte stream) is queued in order, bounded by bytes.
struct Client {
    tx: Sender<Arc<Vec<u8>>>,
    queued: Arc<AtomicU64>,
    video: Arc<Mutex<std::collections::VecDeque<Arc<Vec<u8>>>>>,
    wake: Sender<()>,
    /// Lossless: this receiver missed a delta and needs a keyframe first.
    needs_key: bool,
    /// Hardware stream: hasn't been given a keyframe yet, so gets nothing
    /// until the next one (a decoder joining mid-GOP shows nothing).
    stream_wait: bool,
    stream: TcpStream,
}

/// Byte budget for a receiver's ordered queue (hardware stream, audio). A
/// receiver this far behind has stalled; it is reconnected.
const QUEUE_BUDGET: u64 = 48 * 1024 * 1024;

/// Video frames that may wait for one receiver (about 50 ms at 60 fps).
/// A momentary network hiccup is absorbed without losing a frame; a
/// receiver further behind sheds its oldest frames instead of piling up lag.
const VIDEO_QUEUE: usize = 3;

/// How a video message may be treated when a receiver is behind.
#[derive(Clone, Copy, PartialEq)]
enum Video {
    /// Independent frame (JPEG): replace whatever is waiting.
    Independent,
    /// Lossless: a keyframe, or a delta that only makes sense after the
    /// previous frame.
    Key,
    Delta,
    /// "Same picture as before": never displaces a frame still waiting.
    Repeat,
}

type Clients = Arc<Mutex<Vec<Client>>>;

pub struct LinkSender {
    pub port: u16,
    pub stats: Arc<SenderStats>,
    frames: Sender<VideoFrame>,
    clients: Clients,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    encoder_proc: Option<Arc<Mutex<Child>>>,
}

fn codec_kind(c: &LinkCodec) -> CodecKind {
    match c {
        LinkCodec::Jpeg { .. } => CodecKind::Jpeg,
        LinkCodec::Lossless => CodecKind::Lossless,
        LinkCodec::Hardware { .. } => CodecKind::Stream,
    }
}

/// Queues `body` for every receiver, in order, without blocking. Only a
/// receiver that has stalled completely (over [`QUEUE_BUDGET`]) is dropped.
fn broadcast(clients: &Clients, stats: &SenderStats, body: Vec<u8>) {
    let body = Arc::new(body);
    let len = body.len() as u64;
    let mut cs = clients.lock();
    cs.retain(|c| {
        if c.queued.load(Ordering::Relaxed) > QUEUE_BUDGET {
            log::warn!("Ferrous Link: a receiver stopped reading; disconnecting it");
            let _ = c.stream.shutdown(Shutdown::Both);
            return false;
        }
        c.queued.fetch_add(len, Ordering::Relaxed);
        if c.tx.send(body.clone()).is_err() {
            return false;
        }
        stats.bytes_sent.fetch_add(len, Ordering::Relaxed);
        true
    });
    stats.clients.store(cs.len() as u32, Ordering::Relaxed);
}

/// Sends a chunk of the hardware stream. Receivers that have just joined get
/// it only from a keyframe on, preceded by the stream's tables (`join`).
fn broadcast_stream(clients: &Clients, stats: &SenderStats, all: Vec<u8>, join: Option<Vec<u8>>) {
    let all = Arc::new(protocol::frame(kind::STREAM, &all));
    let join = join.map(|j| Arc::new(protocol::frame(kind::STREAM, &j)));
    let mut cs = clients.lock();
    cs.retain_mut(|c| {
        if c.queued.load(Ordering::Relaxed) > QUEUE_BUDGET {
            log::warn!("Ferrous Link: a receiver stopped reading; disconnecting it");
            let _ = c.stream.shutdown(Shutdown::Both);
            return false;
        }
        let body = if c.stream_wait {
            match &join {
                Some(j) => {
                    c.stream_wait = false;
                    j.clone()
                }
                None => return true,
            }
        } else {
            all.clone()
        };
        c.queued.fetch_add(body.len() as u64, Ordering::Relaxed);
        stats.bytes_sent.fetch_add(body.len() as u64, Ordering::Relaxed);
        c.tx.send(body).is_ok()
    });
    stats.clients.store(cs.len() as u32, Ordering::Relaxed);
}

/// Queues a video frame for every receiver (see [`VIDEO_QUEUE`]).
/// Returns true if any receiver needs a keyframe (lossless only).
fn broadcast_video(clients: &Clients, stats: &SenderStats, body: Vec<u8>, kind: Video) -> bool {
    let body = Arc::new(body);
    let len = body.len() as u64;
    let mut want_key = false;
    let mut cs = clients.lock();
    cs.retain_mut(|c| {
        if kind == Video::Delta && c.needs_key {
            want_key = true;
            return true; // a delta is useless until it has a keyframe
        }
        let replaced = {
            let mut q = c.video.lock();
            if kind == Video::Repeat && !q.is_empty() {
                return true; // the pending frame already carries the picture
            }
            q.push_back(body.clone());
            if q.len() > VIDEO_QUEUE { q.pop_front() } else { None }
        };
        if replaced.is_some() {
            stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            if matches!(kind, Video::Key | Video::Delta) {
                // The replaced frame was part of the delta chain.
                c.needs_key = kind == Video::Delta;
                want_key |= c.needs_key;
            }
        }
        if kind == Video::Key {
            c.needs_key = false;
        }
        stats.bytes_sent.fetch_add(len, Ordering::Relaxed);
        !matches!(c.wake.try_send(()), Err(TrySendError::Disconnected(_)))
    });
    stats.clients.store(cs.len() as u32, Ordering::Relaxed);
    want_key
}

/// Ports senders have just released (see [`LinkSender::start`]).
fn released() -> &'static Mutex<Vec<(u16, Instant)>> {
    static R: std::sync::OnceLock<Mutex<Vec<(u16, Instant)>>> = std::sync::OnceLock::new();
    R.get_or_init(Mutex::default)
}

impl LinkSender {
    pub fn start(cfg: SenderConfig) -> anyhow::Result<Self> {
        // The configured port, or the default one, or the next free one so
        // several links can coexist. A sender being replaced (an encoder
        // change restarts it) may not have let go of its port yet, and
        // receivers are looking for the same one, so wait for it briefly
        // rather than moving to another.
        let listener = {
            let first = if cfg.port != 0 { cfg.port } else { crate::DEFAULT_PORT };
            let mut l = None;
            let released_lately = || released().lock().iter().any(|(p, t)| *p == first && t.elapsed() < Duration::from_secs(5));
            let give_up = Instant::now() + Duration::from_secs(3);
            loop {
                match TcpListener::bind(("0.0.0.0", first)) {
                    Ok(x) => {
                        l = Some(x);
                        break;
                    }
                    // Only a port we just let go of is worth waiting for.
                    Err(_) if released_lately() && Instant::now() < give_up => thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            match l {
                Some(l) => l,
                None if cfg.port != 0 => TcpListener::bind(("0.0.0.0", cfg.port))?,
                None => (crate::DEFAULT_PORT + 1..crate::DEFAULT_PORT + 50)
                    .find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok())
                    .ok_or_else(|| anyhow::anyhow!("no free port for Ferrous Link"))?,
            }
        };
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stats = Arc::new(SenderStats::default());
        let clients: Clients = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let force_key = Arc::new(AtomicBool::new(false));
        // The key is derived per connection (fresh salt in each Hello). One key shared by every
        // connection would reuse AEAD nonces, since the counters restart at 0 on each one.
        let pin = cfg.pin.clone();
        let hello = Hello {
            proto: protocol::PROTO_VERSION,
            name: cfg.name.clone(),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            codec: codec_kind(&cfg.codec),
            audio: cfg.audio,
            encrypted: !pin.is_empty(),
            salt: String::new(),
        };
        let mut threads = Vec::new();

        // Accept + handshake.
        {
            let (clients, stop, force_key, stats) = (clients.clone(), stop.clone(), force_key.clone(), stats.clone());
            let hello = Arc::new(hello);
            let pending = Arc::new(AtomicUsize::new(0));
            let last_warned: Arc<Mutex<std::collections::HashMap<std::net::IpAddr, Instant>>> = Arc::default();
            threads.push(thread::Builder::new().name("link-accept".into()).spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, addr)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_nodelay(true);
                            // Handshakes run off the accept thread: a peer that connects and goes quiet holds
                            // only its own (time-limited) handshake, not everyone else's. A cap keeps a flood
                            // of such peers from piling up threads.
                            if pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING_HANDSHAKES {
                                pending.fetch_sub(1, Ordering::Relaxed);
                                continue;
                            }
                            let (hello, pin, clients, force_key, stats, in_thread, last_warned) =
                                (hello.clone(), pin.clone(), clients.clone(), force_key.clone(), stats.clone(), pending.clone(), last_warned.clone());
                            let spawned = thread::Builder::new().name("link-handshake".into()).spawn(move || {
                                let result = handshake(stream, &hello, &pin);
                                in_thread.fetch_sub(1, Ordering::Relaxed);
                                match result {
                                    Ok((stream, rx_cipher, tx_cipher)) => {
                                        log::info!("Ferrous Link: receiver {addr} connected");
                                        add_client(&clients, stream, rx_cipher, tx_cipher, stats);
                                        force_key.store(true, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        // A receiver that keeps retrying would log this every
                                        // couple of seconds: once a minute per address is enough.
                                        let now = Instant::now();
                                        let mut seen = last_warned.lock();
                                        seen.retain(|_, t| now - *t < Duration::from_secs(60));
                                        if seen.insert(addr.ip(), now).is_none() {
                                            let eof = e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof);
                                            if eof {
                                                log::warn!(
                                                    "Ferrous Link: receiver {} hung up during the handshake. It is probably running a different \
                                                     Ferrous version; install the same build on both PCs. (Repeats are not logged for a minute.)",
                                                    addr.ip()
                                                );
                                            } else {
                                                log::warn!("Ferrous Link: receiver {addr} rejected: {e} (repeats are not logged for a minute)");
                                            }
                                        }
                                    }
                                }
                            });
                            if spawned.is_err() {
                                pending.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(30)),
                        Err(_) => thread::sleep(Duration::from_millis(200)),
                    }
                }
            })?);
        }

        // Discovery beacons.
        {
            let stop = stop.clone();
            let ann = Announcement {
                ember_link: protocol::PROTO_VERSION,
                id: protocol::random_hex(8),
                name: cfg.name.clone(),
                port,
                codec: codec_kind(&cfg.codec),
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
                encrypted: !cfg.pin.is_empty(),
                host: String::new(),
            };
            threads.push(thread::Builder::new().name("link-beacon".into()).spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    discovery::announce(&ann);
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            })?);
        }

        // Encoder.
        let (frames_tx, frames_rx) = crossbeam_channel::bounded::<VideoFrame>(1);
        let mut encoder_proc = None;
        match cfg.codec.clone() {
            LinkCodec::Jpeg { quality, adaptive, full_chroma } => {
                // obs-teleport's design: several workers each encode a whole
                // frame, so throughput scales with cores while every frame
                // is one simple JPEG; an orderer sends them strictly in order.
                // Like obs-teleport (a goroutine per frame on every core): all cores but one, and room for a
                // short burst to queue instead of frames being skipped while every worker is busy.
                let workers = thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(1)).clamp(2, 16);
                let (job_tx, job_rx) = crossbeam_channel::bounded::<(u64, u64, VideoFrame, u8)>(workers * 2);
                let (done_tx, done_rx) = crossbeam_channel::unbounded::<(u64, Vec<u8>, Video)>();
                let (w, h) = (cfg.width, cfg.height);
                for i in 0..workers {
                    let (job_rx, done_tx) = (job_rx.clone(), done_tx.clone());
                    threads.push(thread::Builder::new().name(format!("link-jpeg-{i}")).spawn(move || {
                        let mut enc = match codec::JpegEncoder::with_chroma(full_chroma) {
                            Ok(e) => e,
                            Err(e) => return log::error!("Ferrous Link: JPEG encoder: {e}"),
                        };
                        for (seq, ts, f, q) in job_rx {
                            let f = codec::scale(&f, w, h);
                            match enc.encode(&f, q, ts) {
                                Ok(payload) => {
                                    let _ = done_tx.send((seq, protocol::frame(kind::VIDEO, &payload), Video::Independent));
                                }
                                // Keep the sequence intact: an empty entry is skipped.
                                Err(e) => {
                                    log::warn!("Ferrous Link: JPEG encode failed: {e}");
                                    let _ = done_tx.send((seq, Vec::new(), Video::Independent));
                                }
                            }
                        }
                    })?);
                }
                {
                    let (clients, stats) = (clients.clone(), stats.clone());
                    threads.push(thread::Builder::new().name("link-order".into()).spawn(move || {
                        let mut pending = std::collections::BTreeMap::new();
                        let mut next = 0u64;
                        for (seq, body, kind) in done_rx {
                            pending.insert(seq, (body, kind));
                            while let Some((body, kind)) = pending.remove(&next) {
                                next += 1;
                                if !body.is_empty() {
                                    broadcast_video(&clients, &stats, body, kind);
                                    stats.frames_sent.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    })?);
                }
                let (stats, force_key) = (stats.clone(), force_key.clone());
                stats.quality.store(quality as u32, Ordering::Relaxed);
                threads.push(thread::Builder::new().name("link-dispatch".into()).spawn(move || {
                    let mut q = quality;
                    let mut last_adjust = Instant::now();
                    let mut drops_at_adjust = 0;
                    let mut clean_secs = 0;
                    let mut last: Option<Arc<Vec<u8>>> = None;
                    for (seq, f) in frames_rx.into_iter().enumerate() {
                        let (seq, ts) = (seq as u64, protocol::now_us());
                        // Unchanged picture (a static scene): a tiny repeat
                        // instead of a new JPEG. A receiver that just joined
                        // needs a real picture first.
                        let joined = force_key.swap(false, Ordering::Relaxed);
                        if !joined && last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &f.data)) {
                            let _ = done_tx.send((seq, protocol::frame(kind::REPEAT, &ts.to_be_bytes()), Video::Repeat));
                        } else {
                            last = Some(f.data.clone());
                            // Blocks while every worker is busy; meanwhile new
                            // frames are skipped at push_frame (and counted).
                            if job_tx.send((seq, ts, f, q)).is_err() {
                                break;
                            }
                        }
                        // Adaptive quality, once a second.
                        if adaptive && last_adjust.elapsed() >= Duration::from_secs(1) {
                            last_adjust = Instant::now();
                            let drops = stats.frames_dropped.load(Ordering::Relaxed);
                            if drops > drops_at_adjust {
                                q = q.saturating_sub(8).max(35);
                                clean_secs = 0;
                            } else {
                                clean_secs += 1;
                                if clean_secs >= 3 {
                                    q = (q + 3).min(quality);
                                }
                            }
                            drops_at_adjust = drops;
                            stats.quality.store(q as u32, Ordering::Relaxed);
                        }
                    }
                })?);
            }
            LinkCodec::Lossless => {
                let (clients, stats, force_key) = (clients.clone(), stats.clone(), force_key.clone());
                let (w, h) = (cfg.width, cfg.height);
                threads.push(thread::Builder::new().name("link-encode".into()).spawn(move || {
                    let mut enc = LosslessEncoder::default();
                    let mut last: Option<Arc<Vec<u8>>> = None;
                    for f in frames_rx {
                        let unchanged = last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &f.data));
                        if unchanged && !force_key.load(Ordering::Relaxed) {
                            // Unchanged picture: a repeat (which never displaces a
                            // pending delta, so the chain stays intact).
                            broadcast_video(&clients, &stats, protocol::frame(kind::REPEAT, &protocol::now_us().to_be_bytes()), Video::Repeat);
                            stats.frames_sent.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        last = Some(f.data.clone());
                        let f = codec::scale(&f, w, h);
                        let want = force_key.swap(false, Ordering::Relaxed);
                        let (payload, key) = enc.encode(&f, protocol::now_us(), want);
                        let body = protocol::frame(kind::VIDEO, &payload);
                        if broadcast_video(&clients, &stats, body, if key { Video::Key } else { Video::Delta }) {
                            force_key.store(true, Ordering::Relaxed);
                        }
                        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
                    }
                })?);
            }
            LinkCodec::Hardware { encoder, bitrate_kbps } => {
                let fps = cfg.fps.max(1);
                let es = EncoderSettings {
                    encoder: encoder.clone(),
                    // VBR: a CBR buffer starves keyframes and fine detail.
                    rate_control: RateControl::Vbr,
                    bitrate_kbps,
                    quality: 23,
                    preset: String::new(),
                    keyframe_interval_secs: 1,
                    b_frames: 0,
                    custom_args: String::new(),
                    hdr: false,
                    hdr_white_nits: 203,
                };
                // Ferrous converts (and scales) to NV12 on all cores, so the pipe
                // carries 1.5 bytes per pixel instead of 4 and FFmpeg's
                // single-threaded converter is skipped; hardware encoders
                // take NV12 as it is.
                let (w, h) = ((cfg.width & !1).max(2), (cfg.height & !1).max(2));
                let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "nv12"].iter().map(|s| s.to_string()).collect();
                args.extend(["-s".into(), format!("{w}x{h}"), "-r".into(), fps.to_string(), "-i".into(), "pipe:0".into()]);
                let want = ferrous_encode::args::encoder_pix_fmt(&encoder);
                if want != "nv12" {
                    args.extend(["-vf".into(), format!("format={want}")]);
                }
                args.extend(ferrous_encode::args::video_args(&es, fps));
                // Low-latency tuning per encoder family (later options win).
                if encoder == "libx264" || encoder == "libx265" {
                    args.extend(["-tune".into(), "zerolatency".into()]);
                } else if encoder.ends_with("_nvenc") {
                    // The recording settings above add a 20-frame lookahead, which holds frames back and
                    // releases them in bursts (stutter on the receiver); `-delay 0` makes NVENC hand each
                    // frame out as soon as it is encoded. "hq" with no B-frames or lookahead keeps the latency
                    // low and looks clearly better than the "ll" tune at the same bitrate.
                    args.extend(
                        ["-tune", "hq", "-zerolatency", "1", "-delay", "0", "-rc-lookahead", "0", "-temporal-aq", "0", "-bf", "0"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                } else if encoder.ends_with("_qsv") || encoder.ends_with("_amf") {
                    args.extend(["-bf", "0"].iter().map(|s| s.to_string()));
                }
                args.extend(["-f", "mpegts", "-flush_packets", "1", "-muxdelay", "0", "pipe:1"].iter().map(|s| s.to_string()));
                let mut child = ferrous_encode::ffmpeg::command(&cfg.ffmpeg)
                    .args(&args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("couldn't start FFmpeg for Ferrous Link: {e}"))?;
                let mut stdin = child.stdin.take().unwrap();
                let mut stdout = child.stdout.take().unwrap();
                let child = Arc::new(Mutex::new(child));
                encoder_proc = Some(child);
                let stats2 = stats.clone();
                threads.push(thread::Builder::new().name("link-encode".into()).spawn(move || {
                    let mut conv: Option<ferrous_output::convert::Converter> = None;
                    let mut nv12 = Vec::new();
                    let mut last: Option<Arc<Vec<u8>>> = None;
                    for f in frames_rx {
                        // The GPU compositor may have converted it already.
                        if let Some(ready) = f.nv12.as_ref().filter(|n| (f.width, f.height) == (w, h) && n.len() == ferrous_output::convert::frame_bytes(w, h)) {
                            if stdin.write_all(ready).is_err() {
                                break;
                            }
                            last = None;
                            nv12.clear();
                            stats2.frames_sent.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        // An unchanged frame still has to be fed (the encoder
                        // runs at a constant rate), but needn't be converted.
                        if !last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &f.data)) || nv12.is_empty() {
                            if conv.as_ref().is_none_or(|c| c.output_size() != (w, h) || c.input_size() != (f.width, f.height)) {
                                conv = Some(ferrous_output::convert::Converter::new((f.width, f.height), (w, h), ferrous_output::convert::YuvLayout::Nv12));
                            }
                            conv.as_ref().unwrap().convert(&f.data, &mut nv12);
                            last = Some(f.data.clone());
                        }
                        if stdin.write_all(&nv12).is_err() {
                            break;
                        }
                        stats2.frames_sent.fetch_add(1, Ordering::Relaxed);
                    }
                })?);
                let (clients, stats) = (clients.clone(), stats.clone());
                threads.push(thread::Builder::new().name("link-stream".into()).spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut scan = crate::ts::Scanner::default();
                    loop {
                        match stdout.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let packets = scan.feed(&buf[..n]);
                                if packets.is_empty() {
                                    continue;
                                }
                                // Where a keyframe starts in this chunk, a new
                                // receiver can join: tables first, then from there.
                                let join = packets.iter().position(|p| p.1 == crate::ts::Packet::Keyframe).and_then(|at| {
                                    let (pat, pmt) = (scan.pat?, scan.pmt?);
                                    let mut j = Vec::with_capacity(crate::ts::PACKET * (packets.len() - at + 2));
                                    j.extend_from_slice(&pat);
                                    j.extend_from_slice(&pmt);
                                    for p in &packets[at..] {
                                        j.extend_from_slice(&p.0);
                                    }
                                    Some(j)
                                });
                                let mut all = Vec::with_capacity(packets.len() * crate::ts::PACKET + 8);
                                all.extend_from_slice(&protocol::now_us().to_be_bytes());
                                for p in &packets {
                                    all.extend_from_slice(&p.0);
                                }
                                let join = join.map(|mut j| {
                                    let mut v = protocol::now_us().to_be_bytes().to_vec();
                                    v.append(&mut j);
                                    v
                                });
                                broadcast_stream(&clients, &stats, all, join);
                            }
                        }
                    }
                })?);
            }
        }

        // Bandwidth meter.
        {
            let (stats, stop) = (stats.clone(), stop.clone());
            threads.push(thread::spawn(move || {
                let mut last = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    let now = stats.bytes_sent.load(Ordering::Relaxed);
                    stats.kbps.store(((now - last) * 8 / 1000) as u32, Ordering::Relaxed);
                    last = now;
                }
            }));
        }

        Ok(Self { port, stats, frames: frames_tx, clients, stop, threads, encoder_proc })
    }

    /// Offers a frame; skipped if the encoder is still busy with the last one.
    pub fn push_frame(&self, frame: &VideoFrame) {
        // Nobody watching: don't spend CPU encoding, hardware or not. (The
        // hardware encoder just waits on its pipe, and a receiver that joins
        // starts on the next keyframe, at most a second later.)
        if self.stats.clients.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Err(TrySendError::Full(_)) = self.frames.try_send(frame.clone()) {
            self.stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Interleaved 48 kHz stereo f32, sent as 16-bit PCM.
    pub fn push_audio(&self, samples: &[f32]) {
        if self.stats.clients.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut payload = protocol::now_us().to_be_bytes().to_vec();
        payload.reserve(samples.len() * 2);
        for s in samples {
            payload.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        broadcast(&self.clients, &self.stats, protocol::frame(kind::AUDIO, &payload));
    }
}

impl Drop for LinkSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for c in self.clients.lock().drain(..) {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        // Closing the frame channel ends the encoder thread.
        let (dead_tx, _) = crossbeam_channel::bounded(1);
        self.frames = dead_tx;
        if let Some(p) = &self.encoder_proc {
            let _ = p.lock().kill();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let mut r = released().lock();
        r.retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
        r.push((self.port, Instant::now()));
    }
}

/// Handshakes allowed at once; further connections are dropped until one finishes.
const MAX_PENDING_HANDSHAKES: usize = 8;

/// Sends the Hello (with a fresh salt, so every connection has its own key) and checks the receiver's
/// first message. Fails after 5 s of silence.
fn handshake(mut stream: TcpStream, hello: &Hello, pin: &str) -> anyhow::Result<(TcpStream, Cipher, Cipher)> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut hello = hello.clone();
    hello.salt = protocol::random_hex(16);
    let key = (!pin.is_empty()).then(|| protocol::derive_key(pin, &hello.salt));
    protocol::write_raw(&mut stream, &serde_json::to_vec(&hello)?)?;
    // Direction 1 = sender→receiver, 2 = receiver→sender.
    let tx = Cipher::new(key, 1);
    let mut rx = Cipher::new(key, 2);
    let auth = rx.open(protocol::read_raw(&mut stream)?)?;
    if auth.first() != Some(&kind::AUTH) {
        anyhow::bail!("bad handshake");
    }
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok((stream, rx, tx))
}

fn add_client(clients: &Clients, stream: TcpStream, mut rx_cipher: Cipher, mut tx_cipher: Cipher, stats: Arc<SenderStats>) {
    protocol::tune_socket(&stream);
    let (tx, rx) = crossbeam_channel::unbounded::<Arc<Vec<u8>>>();
    let (wake, woken) = crossbeam_channel::bounded::<()>(1);
    let queued = Arc::new(AtomicU64::new(0));
    let video: Arc<Mutex<std::collections::VecDeque<Arc<Vec<u8>>>>> = Arc::default();
    let Ok(writer) = stream.try_clone() else { return };
    let Ok(reader) = stream.try_clone() else { return };
    {
        let (queued, video, wake_self) = (queued.clone(), video.clone(), wake.clone());
        thread::Builder::new()
            .name("link-send".into())
            .spawn(move || {
                let mut w = protocol::MessageWriter::new(writer);
                loop {
                    // Ordered messages (audio, stream, pongs) go first; then
                    // the newest video frame, if one is waiting.
                    let body = crossbeam_channel::select! {
                        recv(rx) -> m => match m {
                            Ok(b) => {
                                queued.fetch_sub(b.len() as u64, Ordering::Relaxed);
                                b
                            }
                            Err(_) => break,
                        },
                        recv(woken) -> m => match m {
                            Ok(()) => {
                                let next = video.lock().pop_front();
                                match next {
                                    Some(b) => {
                                        // More waiting: come back for it after this one.
                                        if !video.lock().is_empty() {
                                            let _ = wake_self.try_send(());
                                        }
                                        b
                                    }
                                    None => continue,
                                }
                            }
                            Err(_) => break,
                        },
                    };
                    if w.send(&mut tx_cipher, &body).is_err() {
                        break;
                    }
                }
                let _ = w.into_inner().shutdown(Shutdown::Both);
            })
            .ok();
    }
    // Reader: answers pings so the receiver can measure latency.
    let (pong_tx, pong_queued) = (tx.clone(), queued.clone());
    thread::spawn(move || {
        let mut r = BufReader::new(reader);
        while let Ok(raw) = protocol::read_raw(&mut r) {
            let Ok(body) = rx_cipher.open(raw) else { break };
            if body.first() == Some(&kind::PING) && body.len() >= 9 {
                let mut p = body[1..9].to_vec();
                p.extend_from_slice(&protocol::now_us().to_be_bytes());
                let msg = Arc::new(protocol::frame(kind::PONG, &p));
                pong_queued.fetch_add(msg.len() as u64, Ordering::Relaxed);
                if pong_tx.send(msg).is_err() {
                    break;
                }
            }
        }
    });
    let mut cs = clients.lock();
    cs.push(Client { tx, queued, video, wake, needs_key: false, stream_wait: true, stream });
    stats.clients.store(cs.len() as u32, Ordering::Relaxed);
}
