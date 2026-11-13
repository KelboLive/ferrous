//! The pre-flight check and plain-language errors.
//!
//! Before Start Recording / Start Streaming is acted on from the buttons,
//! the things that most often go wrong are looked at (no stream key, a muted
//! microphone, a monitor that's gone, a nearly full drive…). Each problem
//! says what is wrong in plain words and, where one click can fix it, offers
//! that click. Raw engine errors get the same treatment in [`friendly`].

use std::path::Path;

use ferrous_core::{RateControl, Settings};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Record,
    Stream,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Severity {
    /// Starting would fail or produce something useless.
    Error,
    /// Starting works, but this is probably not what you want.
    Warn,
}

/// A one-click remedy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fix {
    /// Open Settings on a tab: "stream", "recording", "video", "audio", "advanced".
    Settings(&'static str),
    /// Open the properties of a source.
    Source(u64),
    /// Unmute a mixer channel ("mic", "desktop").
    Unmute(&'static str),
    /// Open the Add Source window.
    AddSource,
}

impl Fix {
    pub fn label(&self) -> &'static str {
        match self {
            Fix::Settings("stream") => "Open Stream settings",
            Fix::Settings("recording") => "Open Recording settings",
            Fix::Settings("video") => "Open Video settings",
            Fix::Settings("audio") => "Open Audio settings",
            Fix::Settings(_) => "Open settings",
            Fix::Source(_) => "Fix this source",
            Fix::Unmute(_) => "Unmute",
            Fix::AddSource => "Add a source",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Check {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub fix: Option<Fix>,
}

fn problem(severity: Severity, title: impl Into<String>, detail: impl Into<String>, fix: Option<Fix>) -> Check {
    Check { severity, title: title.into(), detail: detail.into(), fix }
}

/// What one source in the scene being shown looks like.
#[derive(Clone, Debug)]
pub struct ItemInfo {
    pub id: u64,
    pub name: String,
    pub visible: bool,
    /// The engine's complaint about this source, if it has one.
    pub error: Option<String>,
    /// A display capture's monitor number.
    pub display: Option<usize>,
    /// Draws something (not an audio-only source).
    pub video: bool,
}

pub struct Inputs<'a> {
    pub kind: Kind,
    pub settings: &'a Settings,
    /// Encoders that passed their test, once the probe has run.
    pub working_encoders: Option<Vec<String>>,
    pub ffmpeg_found: bool,
    pub items: Vec<ItemInfo>,
    pub monitors: usize,
    /// `Some(muted)` for a mixer channel that exists and is in use.
    pub mic_muted: Option<bool>,
    pub desktop_muted: Option<bool>,
    pub free_bytes: Option<u64>,
    pub folder_ok: bool,
}

pub fn run(i: &Inputs) -> Vec<Check> {
    use Severity::{Error, Warn};
    let mut out = Vec::new();
    let s = i.settings;

    // ---- the picture
    let shown: Vec<&ItemInfo> = i.items.iter().filter(|it| it.video && it.visible).collect();
    if i.items.is_empty() {
        out.push(problem(Warn, "Your scene is empty", "You would record a blank picture. Add a screen, window, game or camera first.", Some(Fix::AddSource)));
    } else if shown.is_empty() {
        out.push(problem(Warn, "Everything in this scene is hidden", "Click the eye next to a source in the Sources list to show it.", None));
    }
    for it in &i.items {
        if let Some(n) = it.display.filter(|n| *n >= i.monitors.max(1)) {
            out.push(problem(Error, format!("“{}” points at a monitor that isn't there", it.name), format!("It is set to display {}, but only {} found. Pick another display in its properties.", n + 1, i.monitors), Some(Fix::Source(it.id))));
        } else if let Some(e) = &it.error {
            out.push(problem(Warn, format!("“{}” isn't working", it.name), friendly(e).map(|f| f.0).unwrap_or_else(|| e.clone()), Some(Fix::Source(it.id))));
        }
    }

    // ---- sound
    match (i.mic_muted, i.desktop_muted) {
        (Some(true), Some(true)) => out.push(problem(Warn, "All your audio is muted", "Nobody will hear anything. Unmute the microphone or desktop audio in the Audio Mixer.", Some(Fix::Unmute("mic")))),
        (Some(true), _) => out.push(problem(Warn, "Your microphone is muted", "Click the speaker in the Audio Mixer to unmute it, or ignore this if you want no voice.", Some(Fix::Unmute("mic")))),
        _ => {}
    }

    // ---- the encoder
    let enc = match i.kind {
        Kind::Stream => &s.stream.encoder,
        Kind::Record if s.record.same_as_stream => &s.stream.encoder,
        Kind::Record => &s.record.encoder,
    };
    let tab = if i.kind == Kind::Stream { "stream" } else { "recording" };
    if !i.ffmpeg_found && !enc.encoder.starts_with("mf_") {
        out.push(problem(Error, "FFmpeg wasn't found", "Ferrous needs ffmpeg.exe to encode. Put it next to Ferrous.exe, or point to it in Settings → Advanced.", Some(Fix::Settings("advanced"))));
    } else if let Some(w) = &i.working_encoders {
        if !w.contains(&enc.encoder) {
            out.push(problem(Error, "The chosen video encoder doesn't work on this PC", format!("“{}” failed its test (often a missing graphics driver or card). Pick another quality profile or Software (x264).", enc.encoder), Some(Fix::Settings(tab))));
        }
    }

    match i.kind {
        Kind::Record => {
            if !i.folder_ok {
                out.push(problem(Error, "Can't save to the recording folder", format!("Ferrous can't write to {}. Choose another folder.", s.record.folder.display()), Some(Fix::Settings("recording"))));
            } else if let Some(b) = i.free_bytes {
                let gb = b as f64 / 1e9;
                let rate = per_hour_gb(enc.bitrate_kbps, s.record.audio_bitrate_kbps).max(1.0);
                if gb < 1.0 {
                    out.push(problem(Error, "Your drive is almost full", format!("Only {gb:.1} GB is free where recordings are saved."), Some(Fix::Settings("recording"))));
                } else if enc.rate_control != RateControl::Quality && gb < rate * 0.5 {
                    out.push(problem(Warn, "Not much room left", format!("{gb:.0} GB free is under 30 minutes of recording at this quality (about {rate:.0} GB per hour)."), Some(Fix::Settings("recording"))));
                }
            }
        }
        Kind::Stream => {
            let rtmp_key_missing = s.stream.key.trim().is_empty() && s.stream.server.starts_with("rtmp");
            let any_dest = s.destinations.iter().any(|d| d.enabled);
            if rtmp_key_missing && !any_dest {
                out.push(problem(Error, "No stream key yet", "Copy your stream key from the streaming site's dashboard and paste it into Settings → Stream.", Some(Fix::Settings("stream"))));
            }
            if s.stream.server.trim().is_empty() && !any_dest {
                out.push(problem(Error, "No server address", "Pick your streaming service in Settings → Stream.", Some(Fix::Settings("stream"))));
            }
            let twitch = s.stream.server.contains("twitch");
            if twitch && enc.bitrate_kbps > 8500 {
                out.push(problem(Warn, "Bitrate is above Twitch's limit", format!("{} kbps is more than Twitch accepts reliably (about 8500). Lower it to avoid disconnects.", enc.bitrate_kbps), Some(Fix::Settings("stream"))));
            }
            if twitch && enc.rate_control != RateControl::Cbr {
                out.push(problem(Warn, "Twitch prefers a constant bitrate", "Set Rate control to CBR in the stream encoder for the smoothest stream.", Some(Fix::Settings("stream"))));
            }
            let need = enc.bitrate_kbps + s.stream.audio_bitrate_kbps;
            if need > 12_000 {
                out.push(problem(Warn, "That needs a fast upload", format!("About {:.0} Mbps of upload speed, with some spare. Check your connection or lower the bitrate.", need as f32 * 1.3 / 1000.0), Some(Fix::Settings("stream"))));
            }
        }
    }
    out.sort_by_key(|c| c.severity);
    out
}

/// Gigabytes written per hour at a constant bitrate.
pub fn per_hour_gb(video_kbps: u32, audio_kbps: u32) -> f64 {
    (video_kbps + audio_kbps) as f64 * 1000.0 * 3600.0 / 8.0 / 1e9
}

/// Can recordings be written to `dir` (created if needed)?
pub fn folder_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".ferrous-write-test");
    let ok = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

/// Free space on the drive holding `dir`.
#[cfg(windows)]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let mut d = dir;
    while !d.exists() {
        d = d.parent()?;
    }
    let wide: Vec<u16> = d.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut avail = 0u64;
    let (mut total, mut free) = (0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and the out pointers are valid for the call.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(avail)
}

#[cfg(not(windows))]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let mut d = dir;
    while !d.exists() {
        d = d.parent()?;
    }
    let out = std::process::Command::new("df").args(["-Pk"]).arg(d).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}

/// A raw error in words a person can act on: (what to tell them, a fix).
/// `None` when nothing recognisable was in it.
pub fn friendly(raw: &str) -> Option<(String, Option<Fix>)> {
    let l = raw.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    let r = if has(&["server returned 4", "unauthorized", "forbidden", "invalid stream key", "authentication failed", "stream key"]) {
        ("The streaming service turned down your stream key. Copy it again from your dashboard into Settings → Stream.", Some(Fix::Settings("stream")))
    } else if has(&["connection refused", "timed out", "could not resolve", "name or service not known", "network is unreachable", "no route to host", "failed to connect", "getaddrinfo"]) {
        ("Couldn't reach the streaming server. Check your internet connection and the server address in Settings → Stream.", Some(Fix::Settings("stream")))
    } else if l.contains("ffmpeg") && has(&["not found", "no such file", "cannot find", "program not found", "failed to start", "couldn't start", "could not start"]) {
        ("FFmpeg wasn't found. Put ffmpeg.exe next to Ferrous.exe, or point to it in Settings → Advanced.", Some(Fix::Settings("advanced")))
    } else if has(&["nvenc", "openencodesession", "no capable devices", "cuda", "amf", "qsv", "mfx"]) {
        ("Your graphics card's video encoder isn't available. Choose a different quality profile or Software (x264) in Settings → Recording.", Some(Fix::Settings("recording")))
    } else if has(&["permission denied", "access is denied", "read-only file system"]) {
        ("Ferrous isn't allowed to save there. Choose another recording folder.", Some(Fix::Settings("recording")))
    } else if has(&["no space left", "disk full", "not enough space"]) {
        ("Your drive is full. Free up some space or pick another folder in Settings → Recording.", Some(Fix::Settings("recording")))
    } else if has(&["broken pipe", "encoder exited", "encoder stopped", "pipe closed"]) {
        ("The encoder stopped unexpectedly. Try the Light quality profile, or Software (x264).", Some(Fix::Settings("recording")))
    } else {
        return None;
    };
    Some((r.0.to_string(), r.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(kind: Kind, s: &Settings) -> Inputs<'_> {
        Inputs {
            kind,
            settings: s,
            working_encoders: None,
            ffmpeg_found: true,
            items: vec![ItemInfo { id: 1, name: "Screen".into(), visible: true, error: None, display: Some(0), video: true }],
            monitors: 1,
            mic_muted: Some(false),
            desktop_muted: Some(false),
            free_bytes: Some(500_000_000_000),
            folder_ok: true,
        }
    }

    #[test]
    fn healthy_setup_has_no_problems() {
        let s = Settings::default();
        assert!(run(&base(Kind::Record, &s)).is_empty());
    }

    #[test]
    fn missing_key_blocks_streaming_and_points_at_settings() {
        let s = Settings::default();
        let c = run(&base(Kind::Stream, &s));
        assert_eq!(c[0].severity, Severity::Error);
        assert_eq!(c[0].fix, Some(Fix::Settings("stream")));
    }

    #[test]
    fn empty_scene_muted_mic_and_gone_monitor() {
        let s = Settings::default();
        let mut i = base(Kind::Record, &s);
        i.items.clear();
        i.mic_muted = Some(true);
        let c = run(&i);
        assert!(c.iter().any(|c| c.title.contains("empty") && c.fix == Some(Fix::AddSource)));
        assert!(c.iter().any(|c| c.fix == Some(Fix::Unmute("mic"))));
        let mut i = base(Kind::Record, &s);
        i.items[0].display = Some(2);
        let c = run(&i);
        assert_eq!(c[0].severity, Severity::Error);
        assert_eq!(c[0].fix, Some(Fix::Source(1)));
    }

    #[test]
    fn broken_encoder_and_full_drive_are_errors() {
        let s = Settings::default();
        let mut i = base(Kind::Record, &s);
        i.working_encoders = Some(vec!["something_else".into()]);
        i.free_bytes = Some(200_000_000);
        let c = run(&i);
        assert_eq!(c.iter().filter(|c| c.severity == Severity::Error).count(), 2);
    }

    #[test]
    fn errors_sort_before_warnings() {
        let s = Settings::default();
        let mut i = base(Kind::Stream, &s);
        i.mic_muted = Some(true);
        let c = run(&i);
        assert!(c.windows(2).all(|w| w[0].severity <= w[1].severity));
    }

    #[test]
    fn friendly_errors() {
        assert!(friendly("rtmp: server returned 403 Forbidden").unwrap().0.contains("stream key"));
        assert!(friendly("Connection refused").unwrap().0.contains("reach"));
        assert!(friendly("OpenEncodeSessionEx failed: no capable devices found").unwrap().1 == Some(Fix::Settings("recording")));
        assert!(friendly("something odd happened").is_none());
    }

    #[test]
    fn size_per_hour() {
        assert!((per_hour_gb(6000, 160) - 2.772).abs() < 0.01);
    }

    #[test]
    fn folder_check_works() {
        let d = std::env::temp_dir().join("ferrous-preflight-test");
        assert!(folder_writable(&d));
        assert!(free_bytes(&d).is_some());
        let _ = std::fs::remove_dir_all(d);
    }
}
