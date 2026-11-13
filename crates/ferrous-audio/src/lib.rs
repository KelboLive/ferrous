//! Audio: WASAPI desktop loopback + microphone capture, resampling to 48 kHz
//! stereo, mixing with per-channel gain/mute, peak metering, and fan-out of
//! 10 ms chunks to every active output.

pub mod filters;
mod resample;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, Sender};
use ferrous_core::AudioSettings;
use parking_lot::Mutex;

pub use resample::LinearResampler;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 10 ms of audio per chunk.
pub const CHUNK_FRAMES: usize = 480;
/// Interleaved stereo f32 samples.
pub type Chunk = Arc<Vec<f32>>;

#[derive(Default)]
pub struct AtomicF32(AtomicU32);
impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// Number of independent output mixes (OBS has 6 audio tracks).
pub const MAX_TRACKS: usize = 6;

/// Audio monitoring mode (what OBS calls "Audio Monitoring").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorMode {
    Off = 0,
    MonitorOnly = 1,
    MonitorAndOutput = 2,
}

/// Levels accumulated since the UI last looked, per stereo side, so a meter
/// drawn at 60 Hz never misses a peak from the 100 Hz mixer.
#[derive(Default)]
pub struct LevelTap {
    peak: [AtomicF32; 2],
    sumsq: [AtomicF32; 2],
    count: AtomicU32,
}

/// Peak and RMS (linear, per side) since the previous `take`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Levels {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
}

impl LevelTap {
    fn feed(&self, interleaved: &[f32]) {
        let mut pk = [0f32; 2];
        let mut sq = [0f32; 2];
        for (i, s) in interleaved.iter().enumerate() {
            let side = i % 2;
            pk[side] = pk[side].max(s.abs());
            sq[side] += s * s;
        }
        for side in 0..2 {
            if pk[side] > self.peak[side].get() {
                self.peak[side].set(pk[side]);
            }
            self.sumsq[side].set(self.sumsq[side].get() + sq[side]);
        }
        self.count.fetch_add((interleaved.len() / 2) as u32, Ordering::Relaxed);
    }

    /// Levels since the last call; `None` if no audio arrived in between.
    pub fn take(&self) -> Option<Levels> {
        let n = self.count.swap(0, Ordering::Relaxed);
        if n == 0 {
            return None;
        }
        let mut l = Levels::default();
        for side in 0..2 {
            l.peak[side] = f32::from_bits(self.peak[side].0.swap(0, Ordering::Relaxed));
            let sq = f32::from_bits(self.sumsq[side].0.swap(0, Ordering::Relaxed));
            l.rms[side] = (sq / n as f32).sqrt();
        }
        Some(l)
    }
}

/// One mixer channel: a device, or a source that carries audio (media file,
/// capture card). Controls and meters are lock-free for the UI.
pub struct AudioChannel {
    pub id: String,
    pub name: Mutex<String>,
    volume: AtomicF32,
    muted: AtomicBool,
    peak: AtomicF32,
    /// Post-fader levels for the mixer meters.
    pub levels: LevelTap,
    active: AtomicBool,
    /// Bitmask of tracks this channel is mixed into (bit 0 = track 1).
    tracks: AtomicU32,
    monitor: AtomicU32,
    pub filters: Mutex<filters::FilterChain>,
    /// Sync offset: how long this channel is held back, in milliseconds.
    delay_ms: AtomicU32,
    delay: Mutex<DelayLine>,
    buffer: Buffer,
    permanent: bool,
}

impl AudioChannel {
    fn new(id: &str, name: &str, permanent: bool) -> Self {
        Self {
            id: id.to_string(),
            name: Mutex::new(name.to_string()),
            volume: AtomicF32::new(1.0),
            muted: AtomicBool::new(false),
            peak: AtomicF32::default(),
            levels: LevelTap::default(),
            active: AtomicBool::new(false),
            tracks: AtomicU32::new(1),
            monitor: AtomicU32::new(MonitorMode::Off as u32),
            filters: Mutex::new(filters::FilterChain::default()),
            delay_ms: AtomicU32::new(0),
            delay: Mutex::new(DelayLine::default()),
            buffer: Arc::default(),
            permanent,
        }
    }
    pub fn volume(&self) -> f32 {
        self.volume.get()
    }
    pub fn set_volume(&self, v: f32) {
        self.volume.set(v.clamp(0.0, 2.0))
    }
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed)
    }
    /// Peak of the last chunk after filters and gain, 0.0 to 1.0+.
    pub fn peak(&self) -> f32 {
        self.peak.get()
    }
    /// Whether something is actually feeding this channel.
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    /// Audio queued for the mixer, in milliseconds (buffer health).
    pub fn buffered_ms(&self) -> f32 {
        self.buffer.lock().len() as f32 / CHANNELS as f32 / SAMPLE_RATE as f32 * 1000.0
    }

    pub fn delay_ms(&self) -> u32 {
        self.delay_ms.load(Ordering::Relaxed)
    }
    /// Holds the channel back (audio sync offset), 0 to `MAX_DELAY_MS`.
    pub fn set_delay_ms(&self, ms: u32) {
        self.delay_ms.store(ms.min(MAX_DELAY_MS), Ordering::Relaxed)
    }

    pub fn tracks(&self) -> u32 {
        self.tracks.load(Ordering::Relaxed)
    }
    pub fn set_tracks(&self, mask: u32) {
        self.tracks.store(mask & ((1 << MAX_TRACKS) - 1), Ordering::Relaxed)
    }
    pub fn monitor(&self) -> MonitorMode {
        match self.monitor.load(Ordering::Relaxed) {
            1 => MonitorMode::MonitorOnly,
            2 => MonitorMode::MonitorAndOutput,
            _ => MonitorMode::Off,
        }
    }
    pub fn set_monitor(&self, m: MonitorMode) {
        self.monitor.store(m as u32, Ordering::Relaxed)
    }
    /// Feeds interleaved 48 kHz stereo samples (for sources that already
    /// produce 48 kHz, e.g. FFmpeg-decoded media).
    pub fn push(&self, samples: &[f32]) {
        self.active.store(true, Ordering::Relaxed);
        push_capped(&self.buffer, samples);
    }
}

/// Longest sync offset a channel accepts.
pub const MAX_DELAY_MS: u32 = 5000;

/// Holds audio back by a fixed time, one mixer chunk at a time.
#[derive(Default)]
pub struct DelayLine {
    buf: VecDeque<f32>,
}

impl DelayLine {
    /// Replaces `io` with the same amount of audio, `delay` samples
    /// (interleaved, a multiple of the channel count) later.
    pub fn process(&mut self, io: &mut [f32], delay: usize) {
        if delay == 0 && self.buf.is_empty() {
            return;
        }
        self.buf.extend(io.iter().copied());
        // A shortened delay drops the audio that is now surplus.
        let keep = delay + io.len();
        if self.buf.len() > keep {
            let excess = self.buf.len() - keep;
            self.buf.drain(..excess - excess % CHANNELS);
        }
        let ready = self.buf.len().saturating_sub(delay).min(io.len());
        for (o, s) in io.iter_mut().zip(self.buf.drain(..ready)) {
            *o = s;
        }
        io[ready..].fill(0.0);
    }
}

fn push_capped(buf: &Buffer, samples: &[f32]) {
    let mut q = buf.lock();
    q.extend(samples.iter().copied());
    // Hard cap at 1 s in case the mixer stalls.
    let cap = SAMPLE_RATE as usize * CHANNELS;
    if q.len() > cap {
        let excess = q.len() - cap;
        q.drain(..excess - excess % CHANNELS);
    }
}

/// The set of mixer channels. Source channels disappear automatically once
/// the source that feeds them is dropped.
#[derive(Default)]
pub struct Mixer {
    channels: Mutex<Vec<Arc<AudioChannel>>>,
    /// Per-track master peaks (L/R), for meters.
    pub master_peak: [AtomicF32; 2],
    /// Track 1 levels for a master meter.
    pub master_levels: LevelTap,
}

impl Mixer {
    /// Snapshot of channels in display order.
    pub fn channels(&self) -> Vec<Arc<AudioChannel>> {
        self.channels.lock().clone()
    }
    pub fn channel(&self, id: &str) -> Option<Arc<AudioChannel>> {
        self.channels.lock().iter().find(|c| c.id == id).cloned()
    }
    /// Returns the existing channel with this id or creates a source channel.
    pub fn source_channel(&self, id: &str, name: &str) -> Arc<AudioChannel> {
        let mut chans = self.channels.lock();
        if let Some(c) = chans.iter().find(|c| c.id == id) {
            *c.name.lock() = name.to_string();
            return c.clone();
        }
        let c = Arc::new(AudioChannel::new(id, name, false));
        chans.push(c.clone());
        c
    }
    fn add_permanent(&self, id: &str, name: &str, volume: f32) -> Arc<AudioChannel> {
        let c = Arc::new(AudioChannel::new(id, name, true));
        c.set_volume(volume);
        self.channels.lock().push(c.clone());
        c
    }
}

type Buffer = Arc<Mutex<VecDeque<f32>>>;

/// Receives mixed audio for one track.
struct Subscriber {
    track: usize,
    tx: Sender<Chunk>,
}

pub struct AudioEngine {
    pub mixer: Arc<Mixer>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl AudioEngine {
    pub fn start(settings: &AudioSettings) -> Self {
        let mixer = Arc::new(Mixer::default());
        let desktop = mixer.add_permanent("desktop", "Desktop Audio", settings.desktop_volume);
        let mic = mixer.add_permanent("mic", "Mic/Aux", settings.mic_volume);
        let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));

        let monitor: Arc<Mutex<Option<Sender<Chunk>>>> = Arc::default();
        // cpal streams live on their own thread for their whole life.
        let device_thread = {
            let stop = stop.clone();
            let monitor = monitor.clone();
            let settings = settings.clone();
            thread::Builder::new()
                .name("audio-devices".into())
                .spawn(move || {
                    let host = cpal::default_host();
                    let mut streams = Vec::new();
                    if settings.desktop_enabled {
                        match open_desktop(&host, settings.desktop_device.as_deref(), desktop.buffer.clone()) {
                            Ok(s) => {
                                desktop.active.store(true, Ordering::Relaxed);
                                streams.push(s)
                            }
                            Err(e) => log::warn!("desktop audio unavailable: {e}"),
                        }
                    }
                    if settings.mic_enabled {
                        match open_mic(&host, settings.mic_device.as_deref(), mic.buffer.clone()) {
                            Ok(s) => {
                                mic.active.store(true, Ordering::Relaxed);
                                streams.push(s)
                            }
                            Err(e) => log::warn!("microphone unavailable: {e}"),
                        }
                    }
                    // Monitoring output: plays the mix of channels set to monitor.
                    let (mon_tx, mon_rx) = crossbeam_channel::bounded::<Chunk>(50);
                    match open_monitor(&host, settings.monitor_device.as_deref(), mon_rx) {
                        Ok(s) => {
                            *monitor.lock() = Some(mon_tx);
                            streams.push(s);
                        }
                        Err(e) => log::info!("audio monitoring unavailable: {e}"),
                    }
                    while !stop.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(50));
                    }
                    drop(streams);
                })
                .expect("spawn audio device thread")
        };

        let mix_thread = {
            let (stop, mixer, subs, mon) = (stop.clone(), mixer.clone(), subscribers.clone(), monitor.clone());
            thread::Builder::new()
                .name("audio-mixer".into())
                .spawn(move || mix_loop(stop, mixer, subs, mon))
                .expect("spawn audio mixer thread")
        };

        Self { mixer, subscribers, stop, threads: vec![device_thread, mix_thread] }
    }

    /// Mixed 10 ms chunks for one track (0-based). Drop the receiver to unsubscribe.
    pub fn subscribe_track(&self, track: usize) -> Receiver<Chunk> {
        // ~2 s of slack before an output that stalls starts losing audio.
        let (tx, rx) = crossbeam_channel::bounded(200);
        self.subscribers.lock().push(Subscriber { track: track.min(MAX_TRACKS - 1), tx });
        rx
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn mix_loop(
    stop: Arc<AtomicBool>,
    mixer: Arc<Mixer>,
    subs: Arc<Mutex<Vec<Subscriber>>>,
    monitor: Arc<Mutex<Option<Sender<Chunk>>>>,
) {
    let period = Duration::from_millis(10);
    let mut next = Instant::now() + period;
    let n = CHUNK_FRAMES * CHANNELS;
    // Keep at most 60 ms queued per channel so clock drift can't build latency.
    let max_queue = n * 6;
    let mut scratch = vec![0f32; n];
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        }
        next += period;
        // If we fell far behind (system sleep), resync instead of bursting.
        if Instant::now() > next + period * 10 {
            next = Instant::now() + period;
        }

        // Drop source channels whose source is gone.
        mixer.channels.lock().retain(|c| c.permanent || Arc::strong_count(c) > 1);
        let channels = mixer.channels();

        // Only mix tracks someone consumes; track 1 always (master meters).
        let wanted = subs.lock().iter().fold(1u32, |m, sub| m | 1 << sub.track);
        let mut tracks: Vec<Vec<f32>> = (0..MAX_TRACKS).map(|t| if wanted & (1 << t) != 0 { vec![0f32; n] } else { Vec::new() }).collect();
        let mut mon = vec![0f32; n];
        let mut any_monitor = false;
        for ch in &channels {
            drain(&ch.buffer, &mut scratch, max_queue);
            let delay = ch.delay_ms() as usize * SAMPLE_RATE as usize / 1000 * CHANNELS;
            ch.delay.lock().process(&mut scratch, delay);
            ch.filters.lock().process(&mut scratch);
            let gain = if ch.muted() { 0.0 } else { ch.volume() };
            let mut pk = 0f32;
            for s in scratch.iter_mut() {
                *s *= gain;
                pk = pk.max(s.abs());
            }
            decay(&ch.peak, pk);
            ch.levels.feed(&scratch);
            let mode = ch.monitor();
            if mode != MonitorMode::Off {
                any_monitor = true;
                for (m, s) in mon.iter_mut().zip(&scratch) {
                    *m += *s;
                }
            }
            if mode == MonitorMode::MonitorOnly {
                continue;
            }
            let mask = ch.tracks() & wanted;
            for (t, mix) in tracks.iter_mut().enumerate() {
                if mask & (1 << t) != 0 {
                    for (m, s) in mix.iter_mut().zip(&scratch) {
                        *m += *s;
                    }
                }
            }
        }
        let mut peaks = [0f32; 2];
        let chunks: Vec<Chunk> = tracks
            .into_iter()
            .enumerate()
            .map(|(t, mut mix)| {
                for (i, s) in mix.iter_mut().enumerate() {
                    *s = s.clamp(-1.0, 1.0);
                    if t == 0 {
                        peaks[i % 2] = peaks[i % 2].max(s.abs());
                    }
                }
                Arc::new(mix)
            })
            .collect();
        mixer.master_levels.feed(&chunks[0]);
        decay(&mixer.master_peak[0], peaks[0]);
        decay(&mixer.master_peak[1], peaks[1]);

        subs.lock().retain(|sub| match sub.tx.try_send(chunks[sub.track].clone()) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => true,
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        });
        let mut m = monitor.lock();
        if let Some(tx) = m.as_ref() {
            if !any_monitor {
                mon.fill(0.0);
            }
            for s in mon.iter_mut() {
                *s = s.clamp(-1.0, 1.0);
            }
            if let Err(crossbeam_channel::TrySendError::Disconnected(_)) = tx.try_send(Arc::new(mon)) {
                *m = None;
            }
        }
    }
}

/// Meter ballistics: instant attack, ~300 ms release.
fn decay(meter: &AtomicF32, new_peak: f32) {
    let old = meter.get();
    meter.set(if new_peak >= old { new_peak } else { old * 0.93 + new_peak * 0.07 });
}

fn drain(buf: &Buffer, out: &mut [f32], max_queue: usize) {
    let mut q = buf.lock();
    if q.len() > max_queue {
        let excess = q.len() - max_queue;
        q.drain(..excess - excess % CHANNELS);
    }
    let take = q.len().min(out.len());
    for (o, s) in out.iter_mut().zip(q.drain(..take)) {
        *o = s;
    }
    out[take..].fill(0.0);
}

// ---------------------------------------------------------------------------
// Devices

pub fn device_label(d: &cpal::Device) -> String {
    d.description().map(|desc| desc.name().to_string()).unwrap_or_else(|_| d.to_string())
}

/// Names of output devices (the ones desktop audio can be captured from).
pub fn list_output_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.map(|d| device_label(&d)).collect())
        .unwrap_or_default()
}

pub fn list_input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.map(|d| device_label(&d)).collect())
        .unwrap_or_default()
}

fn open_desktop(host: &cpal::Host, name: Option<&str>, buf: Buffer) -> anyhow::Result<cpal::Stream> {
    let device = match name {
        Some(n) => host.output_devices()?.find(|d| device_label(d) == n),
        None => host.default_output_device(),
    }
    .ok_or_else(|| anyhow::anyhow!("output device not found"))?;
    // On WASAPI, an input stream on an output device is a loopback capture.
    let config = device.default_output_config()?;
    log::info!("desktop audio: {} ({config:?})", device_label(&device));
    build_stream(&device, config, buf)
}

fn open_mic(host: &cpal::Host, name: Option<&str>, buf: Buffer) -> anyhow::Result<cpal::Stream> {
    let device = match name {
        Some(n) => host.input_devices()?.find(|d| device_label(d) == n),
        None => host.default_input_device(),
    }
    .ok_or_else(|| anyhow::anyhow!("input device not found"))?;
    let config = device.default_input_config()?;
    log::info!("microphone: {} ({config:?})", device_label(&device));
    build_stream(&device, config, buf)
}

fn open_monitor(host: &cpal::Host, name: Option<&str>, rx: Receiver<Chunk>) -> anyhow::Result<cpal::Stream> {
    let device = match name {
        Some(n) => host.output_devices()?.find(|d| device_label(d) == n),
        None => host.default_output_device(),
    }
    .ok_or_else(|| anyhow::anyhow!("output device not found"))?;
    let config = device.default_output_config()?;
    if config.sample_format() != SampleFormat::F32 {
        anyhow::bail!("monitor device format {} not supported", config.sample_format());
    }
    let channels = config.channels() as usize;
    let mut resampler = LinearResampler::new(SAMPLE_RATE, config.sample_rate());
    let mut queue: VecDeque<f32> = VecDeque::new();
    let mut tmp = Vec::new();
    let stream = device.build_output_stream(
        config.config(),
        move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
            while let Ok(chunk) = rx.try_recv() {
                tmp.clear();
                resampler.process(&chunk, &mut tmp);
                queue.extend(tmp.iter().copied());
            }
            // Keep latency bounded (~100 ms).
            let cap = (SAMPLE_RATE as usize / 10) * CHANNELS;
            while queue.len() > cap {
                queue.pop_front();
            }
            for frame in out.chunks_exact_mut(channels.max(1)) {
                let l = queue.pop_front().unwrap_or(0.0);
                let r = queue.pop_front().unwrap_or(0.0);
                for (i, s) in frame.iter_mut().enumerate() {
                    *s = if i % 2 == 0 { l } else { r };
                }
            }
        },
        |e| log::warn!("monitor stream error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

fn build_stream(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    buf: Buffer,
) -> anyhow::Result<cpal::Stream> {
    let stream = match config.sample_format() {
        SampleFormat::F32 => typed::<f32>(device, &config, buf)?,
        SampleFormat::I16 => typed::<i16>(device, &config, buf)?,
        SampleFormat::I32 => typed::<i32>(device, &config, buf)?,
        SampleFormat::U16 => typed::<u16>(device, &config, buf)?,
        SampleFormat::U8 => typed::<u8>(device, &config, buf)?,
        other => anyhow::bail!("unsupported sample format {other}"),
    };
    stream.play()?;
    Ok(stream)
}

fn typed<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    buf: Buffer,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = config.channels() as usize;
    let mut resampler = LinearResampler::new(config.sample_rate(), SAMPLE_RATE);
    let mut stereo: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let stream = device.build_input_stream(
        config.config(),
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            stereo.clear();
            for frame in data.chunks_exact(channels.max(1)) {
                let l = frame[0].to_sample::<f32>();
                let r = if channels > 1 { frame[1].to_sample::<f32>() } else { l };
                stereo.push(l);
                stereo.push(r);
            }
            resampled.clear();
            resampler.process(&stereo, &mut resampled);
            push_capped(&buf, &resampled);
        },
        |e| log::warn!("audio stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod level_tap_tests {
    use super::*;

    #[test]
    fn tap_reports_max_peak_and_rms_since_last_take() {
        let t = LevelTap::default();
        assert!(t.take().is_none());
        // Left: constant 0.5; right: alternating ±1.0 then quiet.
        let a: Vec<f32> = (0..480).flat_map(|i| [0.5, if i % 2 == 0 { 1.0 } else { -1.0 }]).collect();
        let b: Vec<f32> = (0..480).flat_map(|_| [0.5, 0.0]).collect();
        t.feed(&a);
        t.feed(&b);
        let l = t.take().unwrap();
        assert!((l.peak[0] - 0.5).abs() < 1e-6 && (l.peak[1] - 1.0).abs() < 1e-6);
        assert!((l.rms[0] - 0.5).abs() < 1e-4);
        assert!((l.rms[1] - (0.5f32).sqrt()).abs() < 1e-3, "{}", l.rms[1]);
        assert!(t.take().is_none(), "take resets");
    }
}

#[cfg(test)]
mod delay_tests {
    use super::*;

    #[test]
    fn delay_line_holds_audio_back_by_exactly_the_delay() {
        let mut d = DelayLine::default();
        let delay = 8; // samples (4 frames)
        let mut out = Vec::new();
        for chunk in 0..6 {
            let mut io = vec![(chunk * 4 + 1) as f32; 4];
            d.process(&mut io, delay);
            out.extend(io);
        }
        // 8 samples of silence, then the input again from its start.
        assert_eq!(&out[..8], &[0.0; 8]);
        assert_eq!(&out[8..12], &[1.0; 4]);
        assert_eq!(&out[12..16], &[5.0; 4]);
    }

    #[test]
    fn zero_delay_is_a_no_op_and_shortening_catches_up() {
        let mut d = DelayLine::default();
        let mut io = vec![1.0; 4];
        d.process(&mut io, 0);
        assert_eq!(io, vec![1.0; 4]);
        for _ in 0..4 {
            d.process(&mut [1.0; 4], 8);
        }
        // Back to no delay: the held audio is released or dropped, never grows.
        for _ in 0..4 {
            d.process(&mut [2.0; 4], 0);
        }
        assert!(d.buf.len() <= 4);
        let mut io = vec![3.0; 4];
        d.process(&mut io, 0);
        assert_eq!(io, vec![3.0; 4]);
    }
}
