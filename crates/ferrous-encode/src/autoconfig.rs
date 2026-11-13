//! Auto-configuration: picks resolution, frame rate, encoder and bitrate
//! from what the PC can do and what the user wants.

use ferrous_core::{EncoderSettings, RateControl, Settings};

use crate::catalog::{Codec, EncoderInfo, Vendor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Goal {
    Streaming,
    Recording,
}

#[derive(Clone, Debug)]
pub struct Inputs<'a> {
    pub goal: Goal,
    /// Encoders that passed the hardware test.
    pub working: &'a [&'static EncoderInfo],
    /// Native size of the main display.
    pub display: (u32, u32),
    pub prefer_fps: u32,
    /// Measured or user-entered upload speed (streaming only).
    pub upload_kbps: Option<u32>,
    /// Platform streaming cap in kbps (e.g. many platforms recommend ≤ 6000–8000 for H.264).
    pub platform_max_kbps: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recommendation {
    pub base: (u32, u32),
    pub output: (u32, u32),
    pub fps: u32,
    pub encoder: EncoderSettings,
    /// Why each choice was made, for the summary page.
    pub notes: Vec<String>,
}

fn vendor_rank(v: Vendor) -> u32 {
    match v {
        Vendor::Nvidia => 0,
        Vendor::Amd => 1,
        Vendor::Intel => 2,
        // Vendor-neutral GPU paths still beat the CPU (and are the only
        // hardware option on e.g. Snapdragon laptops).
        Vendor::D3d12 => 3,
        Vendor::Vulkan => 4,
        Vendor::Software => 5,
        Vendor::MediaFoundation => 6,
    }
}

/// Best encoder for the goal: hardware first; for recording, prefer HEVC/AV1 on hardware.
pub fn pick_encoder(working: &[&'static EncoderInfo], goal: Goal) -> Option<&'static EncoderInfo> {
    let codec_rank = |c: Codec| match (goal, c) {
        // H.264 is universally accepted by streaming platforms.
        (Goal::Streaming, Codec::H264) => 0,
        (Goal::Streaming, _) => 9,
        (Goal::Recording, Codec::Hevc) => 0,
        (Goal::Recording, Codec::Av1) => 1,
        (Goal::Recording, Codec::H264) => 2,
    };
    working
        .iter()
        .copied()
        .filter(|e| goal == Goal::Recording || e.codec == Codec::H264)
        .filter(|e| e.name != "libaom-av1") // far too slow for live use
        .min_by_key(|e| {
            // Software HEVC/AV1 is too heavy for most CPUs live; rank it after hardware H.264.
            let sw_penalty = if !e.vendor.is_hardware() && e.codec != Codec::H264 { 10 } else { 0 };
            (vendor_rank(e.vendor) * 10 + codec_rank(e.codec) + sw_penalty, e.name)
        })
}

/// Bitrate needed for decent quality at a given output (H.264, kbps).
fn ideal_kbps(w: u32, h: u32, fps: u32) -> u32 {
    let px = (w * h) as f32 / (1920.0 * 1080.0);
    let motion = if fps > 30 { 1.5 } else { 1.0 };
    (4000.0 * px * motion).round() as u32
}

pub fn recommend(i: &Inputs<'_>) -> Recommendation {
    let mut notes = Vec::new();
    let base = i.display;
    let enc = pick_encoder(i.working, i.goal);
    let mut encoder = EncoderSettings::streaming_default();
    match enc {
        Some(e) => {
            encoder.encoder = e.name.to_string();
            encoder.preset = e.default_preset.to_string();
            notes.push(format!("Encoder: {} (passed the encoder test on this PC)", e.display_name()));
        }
        None => notes.push("No encoder passed the test; check FFmpeg in Settings > Advanced".into()),
    }
    let hardware = enc.is_some_and(|e| e.vendor.is_hardware());

    // Candidate outputs from the display down; keep the aspect ratio.
    let aspect = base.0 as f32 / base.1.max(1) as f32;
    let heights = [2160u32, 1440, 1080, 936, 864, 720, 480];
    let mut fps = i.prefer_fps.clamp(24, 120);
    let output;
    match i.goal {
        Goal::Recording => {
            output = base;
            encoder.rate_control = RateControl::Quality;
            encoder.quality = if hardware { 22 } else { 20 };
            encoder.bitrate_kbps = 40_000;
            notes.push(format!("Recording at full resolution {}×{}, constant quality", base.0, base.1));
            if !hardware && base.1 > 1080 && fps > 30 {
                fps = 30;
                notes.push("Software encoding above 1080p: capped at 30 fps to keep up".into());
            }
        }
        Goal::Streaming => {
            let budget = i.upload_kbps.map(|u| (u as f32 * 0.75) as u32).unwrap_or(6000).min(i.platform_max_kbps).max(1000);
            let mut chosen = None;
            for &h in heights.iter().filter(|&&h| h <= base.1) {
                let w = ((h as f32 * aspect).round() as u32) & !1;
                // Software encoders: stay at or below 1080p for live use.
                if !hardware && h > 1080 {
                    continue;
                }
                if ideal_kbps(w, h, fps) <= budget {
                    chosen = Some((w, h));
                    break;
                }
                // Prefer dropping to 30 fps at 720p before going below 720p.
                if h == 720 && fps > 30 && ideal_kbps(w, h, 30) <= budget {
                    fps = 30;
                    chosen = Some((w, h));
                    notes.push("Frame rate lowered to 30 so 720p fits your upload".into());
                    break;
                }
            }
            output = chosen.unwrap_or((((480.0 * aspect) as u32) & !1, 480));
            encoder.rate_control = RateControl::Cbr;
            encoder.bitrate_kbps = budget.min(ideal_kbps(output.0, output.1, fps).max(2500)).min(budget);
            encoder.keyframe_interval_secs = 2;
            match i.upload_kbps {
                Some(u) => notes.push(format!("Bitrate {} kbps: 75% of your {u} kbps upload, platform cap {}", encoder.bitrate_kbps, i.platform_max_kbps)),
                None => notes.push(format!("Bitrate {} kbps (upload speed unknown; assumed 8 Mbps)", encoder.bitrate_kbps)),
            }
            notes.push(format!("Streaming at {}×{} {fps} fps", output.0, output.1));
        }
    }
    Recommendation { base, output, fps, encoder, notes }
}

/// Applies a recommendation to a copy of `settings`.
pub fn apply(settings: &Settings, r: &Recommendation, goal: Goal) -> Settings {
    let mut s = settings.clone();
    s.video.base_width = r.base.0;
    s.video.base_height = r.base.1;
    s.video.output_width = r.output.0;
    s.video.output_height = r.output.1;
    s.video.fps = r.fps;
    match goal {
        Goal::Streaming => s.stream.encoder = r.encoder.clone(),
        Goal::Recording => {
            s.record.encoder = r.encoder.clone();
            s.record.same_as_stream = false;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::find;

    fn enc(names: &[&str]) -> Vec<&'static EncoderInfo> {
        names.iter().map(|n| find(n).unwrap()).collect()
    }

    #[test]
    fn prefers_hardware_and_right_codec() {
        let all = enc(&["libx264", "libx265", "h264_nvenc", "hevc_nvenc", "av1_nvenc", "h264_qsv"]);
        assert_eq!(pick_encoder(&all, Goal::Streaming).unwrap().name, "h264_nvenc");
        assert_eq!(pick_encoder(&all, Goal::Recording).unwrap().name, "hevc_nvenc");
        let sw = enc(&["libx264", "libx265", "libsvtav1"]);
        assert_eq!(pick_encoder(&sw, Goal::Recording).unwrap().name, "libx264");
        let amd = enc(&["libx264", "h264_amf", "av1_amf"]);
        assert_eq!(pick_encoder(&amd, Goal::Recording).unwrap().name, "av1_amf");
    }

    #[test]
    fn streaming_fits_upload() {
        let w = enc(&["libx264", "h264_nvenc"]);
        let fast = recommend(&Inputs { goal: Goal::Streaming, working: &w, display: (2560, 1440), prefer_fps: 60, upload_kbps: Some(20_000), platform_max_kbps: 8000 });
        // 1440p60 needs more than the 8 Mbps platform cap, so it scales to 1080p60.
        assert_eq!((fast.output, fast.fps), ((1920, 1080), 60));
        assert!(fast.encoder.bitrate_kbps <= 8000);

        let slow = recommend(&Inputs { goal: Goal::Streaming, working: &w, display: (1920, 1080), prefer_fps: 60, upload_kbps: Some(4000), platform_max_kbps: 8000 });
        assert!(slow.output.1 <= 720, "{:?}", slow.output);
        assert!(slow.encoder.bitrate_kbps <= 3000);
        assert_eq!(slow.encoder.rate_control, RateControl::Cbr);

        let cpu = recommend(&Inputs { goal: Goal::Streaming, working: &enc(&["libx264"]), display: (3840, 2160), prefer_fps: 60, upload_kbps: Some(50_000), platform_max_kbps: 8000 });
        assert!(cpu.output.1 <= 1080, "software stays ≤1080p live: {:?}", cpu.output);
    }

    #[test]
    fn recording_uses_quality_mode_at_native() {
        let r = recommend(&Inputs { goal: Goal::Recording, working: &enc(&["hevc_nvenc"]), display: (2560, 1440), prefer_fps: 60, upload_kbps: None, platform_max_kbps: 0 });
        assert_eq!(r.output, (2560, 1440));
        assert_eq!(r.encoder.rate_control, RateControl::Quality);
        assert_eq!(r.fps, 60);
    }
}
