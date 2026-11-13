//! Built-in recording presets, generated for the encoders that work on this
//! PC. Each level trades speed for picture quality the same way on every
//! encoder, even though the knobs underneath differ.

use ferrous_core::{AudioCodec, EncoderSettings, RateControl, RecordSettings, RecordingFormat, RecordingPreset};

use crate::catalog::{Codec, EncoderInfo, Vendor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Slowest practical preset, lowest quantiser: the cleanest picture.
    MaxQuality,
    /// The default: good picture at a cost the GPU/CPU shrugs off.
    Balanced,
    /// Fastest preset: least load while gaming, a bit softer.
    Light,
    /// Balanced speed, higher quantiser: smaller files.
    Small,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::MaxQuality, Level::Balanced, Level::Light, Level::Small];

    pub fn label(self) -> &'static str {
        match self {
            Level::MaxQuality => "Max quality",
            Level::Balanced => "Balanced",
            Level::Light => "Light (low load)",
            Level::Small => "Small files",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Level::MaxQuality => "Slowest preset and lowest quantiser. Cleanest picture, highest load and biggest files.",
            Level::Balanced => "A good picture for a modest cost. The recommended starting point.",
            Level::Light => "Fastest preset, no B-frames. Least load while gaming, slightly softer.",
            Level::Small => "Balanced speed with a higher quantiser: roughly a third smaller files.",
        }
    }
}

/// The encoder preset name for a level (empty when the encoder has none).
fn preset_name(e: &EncoderInfo, level: Level) -> &'static str {
    use Level::*;
    let pick = |max: &'static str, bal: &'static str, light: &'static str| match level {
        MaxQuality => max,
        Balanced | Small => bal,
        Light => light,
    };
    match e.name {
        n if n.ends_with("_nvenc") => pick("p7", "p5", "p3"),
        n if n.ends_with("_amf") => pick(if e.codec == Codec::Av1 { "high_quality" } else { "quality" }, "quality", "speed"),
        n if n.ends_with("_qsv") => pick("slow", "medium", "faster"),
        "libx264" => pick("slow", "fast", "veryfast"),
        "libx265" => pick("medium", "fast", "veryfast"),
        "libsvtav1" => pick("4", "6", "10"),
        "libaom-av1" => pick("4", "6", "8"),
        _ => "",
    }
}

/// Encoder settings for `encoder` at `level`.
pub fn encoder_settings(e: &EncoderInfo, level: Level) -> EncoderSettings {
    let quality = match level {
        Level::MaxQuality => 18,
        Level::Balanced => 21,
        Level::Light => 23,
        Level::Small => 26,
    };
    let b_frames = match (level, e.vendor) {
        (Level::Light, _) => 0,
        (_, Vendor::Amd | Vendor::MediaFoundation) => 0,
        (Level::MaxQuality, _) => 3,
        _ => 2,
    };
    EncoderSettings {
        encoder: e.name.to_string(),
        rate_control: RateControl::Quality,
        bitrate_kbps: 40_000,
        quality,
        preset: preset_name(e, level).to_string(),
        keyframe_interval_secs: 2,
        b_frames,
        custom_args: String::new(),
        hdr: false,
        hdr_white_nits: 203,
    }
}

fn preset(name: String, encoder: EncoderSettings) -> RecordingPreset {
    RecordingPreset { name, encoder, format: RecordingFormat::Mkv, audio_codec: AudioCodec::Aac, audio_bitrate_kbps: 192, output_size: None }
}

/// The built-in presets: the four levels on the best working encoder, and
/// "Small files" on the most compact codec that works (AV1, else HEVC).
pub fn builtin(working: &[&'static EncoderInfo]) -> Vec<RecordingPreset> {
    use crate::autoconfig::{pick_encoder, Goal};
    let Some(best) = pick_encoder(working, Goal::Recording) else { return Vec::new() };
    Level::ALL
        .iter()
        .map(|&level| {
            let e = if level == Level::Small { compact(working, best) } else { best };
            preset(format!("{} ({})", level.label(), e.name), encoder_settings(e, level))
        })
        .collect()
}

/// Same vendor's AV1, else HEVC, when available and not slower than `best`
/// by a software penalty; otherwise `best`.
fn compact(working: &[&'static EncoderInfo], best: &'static EncoderInfo) -> &'static EncoderInfo {
    for codec in [Codec::Av1, Codec::Hevc] {
        if let Some(e) = working.iter().copied().find(|e| e.vendor == best.vendor && e.codec == codec && e.name != "libaom-av1") {
            if best.vendor.is_hardware() || e.codec == best.codec {
                return e;
            }
        }
    }
    best
}

/// Names the encoder settings plainly for list rows.
pub fn summary(e: &EncoderSettings) -> String {
    let rate = match e.rate_control {
        RateControl::Cbr => format!("CBR {} kbps", e.bitrate_kbps),
        RateControl::Vbr => format!("VBR {} kbps", e.bitrate_kbps),
        RateControl::Quality => format!("quality {}", e.quality),
        RateControl::Cqp => format!("QP {}", e.quality),
    };
    let preset = if e.preset.is_empty() { String::new() } else { format!(", preset {}", e.preset) };
    format!("{}, {rate}{preset}{}", e.encoder, if e.hdr { ", HDR10" } else { "" })
}

/// Whether `r` currently matches `p` (for highlighting the active preset).
pub fn matches(p: &RecordingPreset, r: &RecordSettings) -> bool {
    !r.same_as_stream && p.encoder == r.encoder && p.format == r.format && p.audio_codec == r.audio_codec && p.audio_bitrate_kbps == r.audio_bitrate_kbps && p.output_size == r.output_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::find;

    #[test]
    fn levels_order_speed_and_quality() {
        let nv = find("hevc_nvenc").unwrap();
        let (max, bal, light) = (encoder_settings(nv, Level::MaxQuality), encoder_settings(nv, Level::Balanced), encoder_settings(nv, Level::Light));
        assert_eq!((max.preset.as_str(), bal.preset.as_str(), light.preset.as_str()), ("p7", "p5", "p3"));
        assert!(max.quality < bal.quality && bal.quality < light.quality);
        assert_eq!(light.b_frames, 0);
        assert_eq!(encoder_settings(find("av1_amf").unwrap(), Level::MaxQuality).preset, "high_quality");
        assert_eq!(encoder_settings(find("hevc_vulkan").unwrap(), Level::Balanced).preset, "");
    }

    #[test]
    fn builtin_uses_the_best_encoder_and_a_compact_codec_for_small() {
        let w = [find("h264_nvenc").unwrap(), find("hevc_nvenc").unwrap(), find("av1_nvenc").unwrap(), find("libx264").unwrap()];
        let p = builtin(&w);
        assert_eq!(p.len(), 4);
        assert!(p[0].encoder.encoder == "hevc_nvenc" && p[0].name.starts_with("Max quality"));
        assert_eq!(p[3].encoder.encoder, "av1_nvenc");
        assert!(builtin(&[]).is_empty());
        // CPU-only: stays on libx264 rather than slow software HEVC.
        assert_eq!(builtin(&[find("libx264").unwrap(), find("libx265").unwrap()])[3].encoder.encoder, "libx264");
    }

    #[test]
    fn apply_and_snapshot_round_trip() {
        let p = builtin(&[find("libx264").unwrap()]).remove(0);
        let mut r = RecordSettings { same_as_stream: true, ..Default::default() };
        p.apply_to(&mut r);
        assert!(!r.same_as_stream && matches(&p, &r));
        let snap = RecordingPreset::from_record("mine", &r, &EncoderSettings::default());
        assert_eq!(snap.encoder, p.encoder);
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<RecordingPreset>(&json).unwrap(), snap);
    }
}
