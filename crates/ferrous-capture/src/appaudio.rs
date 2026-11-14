//! Per-application audio via WASAPI process loopback (Windows 10 2004+),
//! the same mechanism as OBS's "Application Audio Capture".

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use ferrous_core::VideoFrame;
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
use windows_capture::window::Window;

use crate::{AudioSink, Source};

fn pid_for(process: &str) -> Option<u32> {
    let want = process.to_ascii_lowercase();
    Window::enumerate().ok()?.into_iter().find_map(|w| {
        let name = w.process_name().ok()?.to_ascii_lowercase();
        (name == want).then(|| w.process_id().ok()).flatten()
    })
}

pub struct AppAudioSource {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AppAudioSource {
    pub fn start(process: &str, sink: AudioSink) -> anyhow::Result<Self> {
        let pid = pid_for(process).ok_or_else(|| anyhow::anyhow!("\"{process}\" isn't running"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (s2, a2) = (stop.clone(), alive.clone());
        let thread = std::thread::Builder::new().name("app-audio".into()).spawn(move || {
            if let Err(e) = capture(pid, sink, &s2) {
                log::warn!("application audio: {e}");
            }
            a2.store(false, Ordering::Relaxed);
        })?;
        Ok(Self { stop, alive, thread: Some(thread) })
    }
}

fn capture(pid: u32, sink: AudioSink, stop: &AtomicBool) -> Result<(), Box<dyn std::error::Error>> {
    let _ = wasapi::initialize_mta();
    // 48 kHz stereo float: exactly what the mixer wants, converted by Windows.
    let format = WaveFormat::new(32, 32, SampleType::Float, 48_000, 2, None);
    let mut client = AudioClient::new_application_loopback_client(pid, true)?;
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
    client.initialize_client(&format, Direction::Capture, mode, None)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut samples = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            capture.read_from_device_to_deque(&mut bytes)?;
            samples.clear();
            // Whole samples only; a partial one stays queued for the next packet.
            let whole = bytes.len() / 4 * 4;
            let (a, b) = bytes.as_slices();
            let mut it = a.iter().chain(b.iter()).copied().take(whole);
            while let (Some(b0), Some(b1), Some(b2), Some(b3)) = (it.next(), it.next(), it.next(), it.next()) {
                samples.push(f32::from_le_bytes([b0, b1, b2, b3]));
            }
            bytes.drain(..whole);
            sink(&samples);
        }
        // Silent apps deliver no events; just keep waiting.
        let _ = event.wait_for_event(200);
    }
    let _ = client.stop_stream();
    Ok(())
}

impl Source for AppAudioSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        None // audio only
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for AppAudioSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
