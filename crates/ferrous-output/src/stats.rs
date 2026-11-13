use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;

/// Live numbers for the status bar, updated from FFmpeg's progress output.
#[derive(Default)]
pub struct OutputStats {
    pub running: AtomicBool,
    pub frames_written: AtomicU64,
    /// Frames the encoder couldn't keep up with ("skipped frames" in OBS).
    pub frames_skipped: AtomicU64,
    /// Bytes of video handed to FFmpeg.
    pub bytes_piped: AtomicU64,
    bitrate_kbps_bits: AtomicU32,
    speed_bits: AtomicU32,
    error: Mutex<Option<String>>,
    /// Last non-progress line FFmpeg printed.
    last_line: Mutex<String>,
}

impl OutputStats {
    pub fn bitrate_kbps(&self) -> f32 {
        f32::from_bits(self.bitrate_kbps_bits.load(Ordering::Relaxed))
    }

    /// Encoder speed relative to real time; below ~0.98 means overloaded.
    pub fn speed(&self) -> f32 {
        f32::from_bits(self.speed_bits.load(Ordering::Relaxed))
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    pub fn fail(&self, why: &str) {
        let mut e = self.error.lock();
        if e.is_none() {
            let detail = self.last_error();
            *e = Some(if detail.is_empty() { why.to_string() } else { format!("{why}: {detail}") });
        }
        self.running.store(false, Ordering::Relaxed);
    }

    /// Last non-progress line FFmpeg printed.
    pub fn last_error(&self) -> String {
        self.last_line.lock().clone()
    }

    pub(crate) fn ingest_line(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if line.starts_with("frame=") || line.contains(" bitrate=") {
            if let Some(v) = field(line, "bitrate=") {
                if let Ok(kbps) = v.trim_end_matches("kbits/s").parse::<f32>() {
                    self.bitrate_kbps_bits.store(kbps.to_bits(), Ordering::Relaxed);
                }
            }
            if let Some(v) = field(line, "speed=") {
                if let Ok(s) = v.trim_end_matches('x').parse::<f32>() {
                    self.speed_bits.store(s.to_bits(), Ordering::Relaxed);
                }
            }
            return;
        }
        log::warn!("ffmpeg: {line}");
        *self.last_line.lock() = line.to_string();
    }
}

/// Value after `key` in an FFmpeg progress line, which pads with spaces
/// (`bitrate=5996.1kbits/s`, `speed=   1x`).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = &line[line.find(key)? + key.len()..];
    rest.split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress() {
        let s = OutputStats::default();
        s.ingest_line("frame=  600 fps= 60 q=23.0 size=    4352KiB time=00:00:10.00 bitrate=3565.1kbits/s dup=0 drop=0 speed=1.01x");
        assert!((s.bitrate_kbps() - 3565.1).abs() < 0.1);
        assert!((s.speed() - 1.01).abs() < 0.001);
        s.ingest_line("[rtmp @ 0x1] Server error: Already publishing");
        assert!(s.last_error().contains("Already publishing"));
    }

    #[test]
    fn speed_with_padding() {
        let s = OutputStats::default();
        s.ingest_line("frame=1 fps=0.0 q=0.0 size=0KiB time=00:00:00.00 bitrate=N/A speed=   1x");
        assert!((s.speed() - 1.0).abs() < 0.001);
    }
}
