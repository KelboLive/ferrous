//! Turning [`EncoderSettings`] into FFmpeg command-line arguments.

use ferrous_core::{AudioCodec, EncoderSettings, RateControl};

use crate::catalog::{self, Vendor};

fn push(args: &mut Vec<String>, items: &[&str]) {
    args.extend(items.iter().map(|s| s.to_string()));
}

/// Pixel format the encoder input should be converted to.
pub fn encoder_pix_fmt(encoder: &str) -> &'static str {
    match catalog::find(encoder) {
        Some(e) if e.vendor.is_hardware() => "nv12",
        _ => "yuv420p",
    }
}

/// Whether this encoder can produce HDR10 (10-bit HEVC or AV1; H.264 and
/// Media Foundation's HEVC are 8-bit here).
pub fn supports_hdr(encoder: &str) -> bool {
    matches!(catalog::find(encoder), Some(e) if matches!(e.codec, catalog::Codec::Hevc | catalog::Codec::Av1) && e.vendor != Vendor::MediaFoundation)
}

/// Whether these settings really produce HDR (asked for and supported).
pub fn hdr_active(s: &EncoderSettings) -> bool {
    s.hdr && supports_hdr(&s.encoder)
}

/// [`encoder_pix_fmt`] for HDR output: 10-bit.
pub fn encoder_pix_fmt_for(s: &EncoderSettings) -> &'static str {
    match (hdr_active(s), encoder_pix_fmt(&s.encoder)) {
        (true, "nv12") => "p010le",
        (true, _) => "yuv420p10le",
        (false, f) => f,
    }
}

/// Encoders that take frames already on the GPU: FFmpeg must create a
/// device and upload to it. Returns (global args placed before the inputs,
/// filter appended after the pixel-format conversion).
pub fn hw_upload(encoder: &str) -> Option<(Vec<String>, &'static str)> {
    let dev = match catalog::find(encoder)?.vendor {
        Vendor::D3d12 => "d3d12va=ember",
        Vendor::Vulkan => "vulkan=ember",
        _ => return None,
    };
    Some((vec!["-init_hw_device".into(), dev.into(), "-filter_hw_device".into(), "ember".into()], "hwupload"))
}

/// Video encoder arguments (everything from `-c:v` on).
pub fn video_args(s: &EncoderSettings, fps: u32) -> Vec<String> {
    let mut a = Vec::new();
    let info = catalog::find(&s.encoder);
    let vendor = info.map(|i| i.vendor).unwrap_or(Vendor::Software);
    let br = format!("{}k", s.bitrate_kbps);
    let peak = format!("{}k", s.bitrate_kbps * 3 / 2);
    let buf = format!("{}k", s.bitrate_kbps * 2);
    let q = s.quality.to_string();
    let gop = (fps * s.keyframe_interval_secs.max(1)).to_string();

    push(&mut a, &["-c:v", &s.encoder]);
    let preset = if s.preset.is_empty() { info.map(|i| i.default_preset).unwrap_or("") } else { &s.preset };

    match s.encoder.as_str() {
        "libx264" | "libx265" => {
            if !preset.is_empty() {
                push(&mut a, &["-preset", preset]);
            }
            match s.rate_control {
                RateControl::Cbr => {
                    push(&mut a, &["-b:v", &br, "-minrate", &br, "-maxrate", &br, "-bufsize", &br]);
                    if s.encoder == "libx264" {
                        push(&mut a, &["-x264-params", "nal-hrd=cbr:force-cfr=1"]);
                    }
                }
                RateControl::Vbr => push(&mut a, &["-b:v", &br, "-maxrate", &peak, "-bufsize", &buf]),
                RateControl::Quality => push(&mut a, &["-crf", &q]),
                RateControl::Cqp => push(&mut a, &["-qp", &q]),
            }
            push(&mut a, &["-bf", &s.b_frames.to_string()]);
        }
        "libsvtav1" => {
            if !preset.is_empty() {
                push(&mut a, &["-preset", preset]);
            }
            // tune=0 is the visual-quality tune (sharper than the PSNR default).
            match s.rate_control {
                // SVT-AV1 rc modes: 0 = CRF/CQP, 1 = VBR, 2 = CBR.
                RateControl::Cbr => push(&mut a, &["-b:v", &br, "-svtav1-params", "tune=0:rc=2"]),
                RateControl::Vbr => push(&mut a, &["-b:v", &br, "-svtav1-params", "tune=0:rc=1"]),
                RateControl::Quality | RateControl::Cqp => push(&mut a, &["-crf", &q, "-svtav1-params", "tune=0"]),
            }
        }
        "libaom-av1" => {
            if !preset.is_empty() {
                push(&mut a, &["-cpu-used", preset]);
            }
            push(&mut a, &["-row-mt", "1", "-usage", "realtime"]);
            match s.rate_control {
                RateControl::Cbr => push(&mut a, &["-b:v", &br, "-minrate", &br, "-maxrate", &br]),
                RateControl::Vbr => push(&mut a, &["-b:v", &br, "-maxrate", &peak]),
                RateControl::Quality | RateControl::Cqp => push(&mut a, &["-crf", &q, "-b:v", "0"]),
            }
        }
        _ => match vendor {
            Vendor::Nvidia => {
                if !preset.is_empty() {
                    push(&mut a, &["-preset", preset]);
                }
                push(&mut a, &["-tune", "hq"]);
                match s.rate_control {
                    RateControl::Cbr => {
                        push(&mut a, &["-rc", "cbr", "-b:v", &br, "-maxrate", &br, "-bufsize", &br])
                    }
                    RateControl::Vbr => {
                        push(&mut a, &["-rc", "vbr", "-b:v", &br, "-maxrate", &peak, "-bufsize", &buf])
                    }
                    RateControl::Quality => push(&mut a, &["-rc", "vbr", "-cq", &q, "-b:v", "0"]),
                    RateControl::Cqp => push(&mut a, &["-rc", "constqp", "-qp", &q]),
                }
                push(&mut a, &["-spatial-aq", "1", "-bf", &s.b_frames.to_string()]);
                let codec = info.map(|i| i.codec);
                // Two-pass rate control sees the whole frame before spending
                // bits: full-resolution first pass on the slow presets.
                push(&mut a, &["-multipass", if matches!(preset, "p6" | "p7") { "fullres" } else { "qres" }]);
                // Quality/VBR runs have latency to spare: look ahead so bits
                // go to the scenes that need them, and let AQ follow motion.
                if matches!(s.rate_control, RateControl::Quality | RateControl::Vbr) {
                    push(&mut a, &["-rc-lookahead", "20", "-temporal-aq", "1"]);
                }
                match codec {
                    // FFmpeg's default H.264 profile is Main; High is better and universal.
                    Some(catalog::Codec::H264) => push(&mut a, &["-profile:v", "high"]),
                    // B-frames as references (Turing and newer, which B-frames need anyway).
                    Some(catalog::Codec::Hevc) if s.b_frames >= 2 => push(&mut a, &["-b_ref_mode", "middle"]),
                    // 10-bit internal precision for 8-bit input: less banding, better compression.
                    Some(catalog::Codec::Av1) => {
                        if s.b_frames >= 2 {
                            push(&mut a, &["-b_ref_mode", "middle"]);
                        }
                        push(&mut a, &["-highbitdepth", "1"]);
                    }
                    _ => {}
                }
            }
            Vendor::Intel => {
                if !preset.is_empty() {
                    push(&mut a, &["-preset", preset]);
                }
                match s.rate_control {
                    // QSV selects CBR when maxrate == bitrate.
                    RateControl::Cbr => {
                        push(&mut a, &["-b:v", &br, "-maxrate", &br, "-bufsize", &buf])
                    }
                    RateControl::Vbr => {
                        push(&mut a, &["-b:v", &br, "-maxrate", &peak, "-bufsize", &buf])
                    }
                    RateControl::Quality => push(&mut a, &["-global_quality", &q]),
                    RateControl::Cqp => push(&mut a, &["-q:v", &q]),
                }
                push(&mut a, &["-bf", &s.b_frames.to_string()]);
                if let Some(catalog::Codec::H264 | catalog::Codec::Hevc) = info.map(|i| i.codec) {
                    // Content-adaptive I/B placement: scene cuts get keyframes, static scenes more B-frames.
                    push(&mut a, &["-adaptive_i", "1", "-adaptive_b", "1"]);
                    if s.b_frames > 0 {
                        push(&mut a, &["-b_strategy", "1"]);
                    }
                }
                if s.encoder == "h264_qsv" {
                    push(&mut a, &["-profile:v", "high"]);
                }
            }
            Vendor::Amd => {
                if !preset.is_empty() {
                    push(&mut a, &["-quality", preset]);
                }
                match info.map(|i| i.codec) {
                    Some(catalog::Codec::H264) => push(&mut a, &["-profile:v", "high", "-vbaq", "1"]),
                    Some(catalog::Codec::Hevc) => push(&mut a, &["-vbaq", "1"]),
                    _ => {}
                }
                match s.rate_control {
                    RateControl::Cbr => {
                        push(&mut a, &["-rc", "cbr", "-b:v", &br, "-maxrate", &br, "-bufsize", &br])
                    }
                    RateControl::Vbr => push(
                        &mut a,
                        &["-rc", "vbr_peak", "-b:v", &br, "-maxrate", &peak, "-bufsize", &buf],
                    ),
                    // AMF has no CRF; constant QP on every frame type is the closest.
                    RateControl::Quality | RateControl::Cqp => push(
                        &mut a,
                        &["-rc", "cqp", "-qp_i", &q, "-qp_p", &q, "-qp_b", &q],
                    ),
                }
            }
            Vendor::D3d12 | Vendor::Vulkan => {
                // FFmpeg's shared hardware-encode rate control; D3D12 spells
                // the modes in capitals, Vulkan in lower case.
                let mode = |m: &str| if vendor == Vendor::D3d12 { m.to_string() } else { m.to_ascii_lowercase() };
                match s.rate_control {
                    RateControl::Cbr => push(&mut a, &["-rc_mode", &mode("CBR"), "-b:v", &br, "-maxrate", &br, "-bufsize", &br]),
                    RateControl::Vbr => push(&mut a, &["-rc_mode", &mode("VBR"), "-b:v", &br, "-maxrate", &peak, "-bufsize", &buf]),
                    RateControl::Quality | RateControl::Cqp => push(&mut a, &["-rc_mode", &mode("CQP"), "-qp", &q]),
                }
                if s.b_frames > 0 {
                    push(&mut a, &["-bf", &s.b_frames.to_string()]);
                }
            }
            Vendor::MediaFoundation => {
                push(&mut a, &["-hw_encoding", "1"]);
                match s.rate_control {
                    RateControl::Cbr => push(&mut a, &["-rate_control", "cbr", "-b:v", &br]),
                    RateControl::Vbr => push(&mut a, &["-rate_control", "pc_vbr", "-b:v", &br]),
                    RateControl::Quality | RateControl::Cqp => {
                        push(&mut a, &["-rate_control", "quality", "-quality", &q.to_string()])
                    }
                }
            }
            Vendor::Software => push(&mut a, &["-b:v", &br]),
        },
    }

    push(&mut a, &["-g", &gop, "-keyint_min", &gop]);
    if hdr_active(s) {
        if matches!(s.encoder.as_str(), "hevc_nvenc" | "hevc_qsv") {
            push(&mut a, &["-profile:v", "main10"]);
        }
        push(&mut a, &[
            "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc", "-color_range", "tv",
        ]);
    } else {
        push(&mut a, &[
            "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv",
        ]);
    }
    a.extend(split_custom(&s.custom_args));
    a
}

/// Audio encoder arguments.
pub fn audio_args(codec: AudioCodec, bitrate_kbps: u32) -> Vec<String> {
    let br = format!("{bitrate_kbps}k");
    let mut a = Vec::new();
    match codec {
        AudioCodec::Aac => push(&mut a, &["-c:a", "aac", "-b:a", &br]),
        AudioCodec::Opus => push(&mut a, &["-c:a", "libopus", "-b:a", &br]),
        AudioCodec::Flac => push(&mut a, &["-c:a", "flac"]),
        AudioCodec::Pcm => push(&mut a, &["-c:a", "pcm_s16le"]),
    }
    a
}

/// Splits user-supplied extra args on whitespace, honouring double quotes.
pub fn split_custom(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(s: &EncoderSettings) -> String {
        video_args(s, 60).join(" ")
    }

    #[test]
    fn x264_cbr() {
        let s = EncoderSettings::streaming_default();
        let j = joined(&s);
        assert!(j.contains("-c:v libx264 -preset veryfast"));
        assert!(j.contains("-b:v 6000k -minrate 6000k -maxrate 6000k"));
        assert!(j.contains("nal-hrd=cbr"));
        assert!(j.contains("-g 120"));
    }

    #[test]
    fn nvenc_cq() {
        let s = EncoderSettings {
            encoder: "hevc_nvenc".into(),
            rate_control: RateControl::Quality,
            quality: 19,
            preset: String::new(),
            ..EncoderSettings::streaming_default()
        };
        let j = joined(&s);
        assert!(j.contains("-preset p5"), "{j}");
        assert!(j.contains("-rc vbr -cq 19 -b:v 0"), "{j}");
        assert_eq!(encoder_pix_fmt("hevc_nvenc"), "nv12");
    }

    #[test]
    fn hdr_needs_hevc_or_av1_and_tags_the_stream() {
        let mut s = EncoderSettings { encoder: "hevc_nvenc".into(), hdr: true, ..EncoderSettings::streaming_default() };
        assert_eq!(encoder_pix_fmt_for(&s), "p010le");
        let j = joined(&s);
        assert!(j.contains("-profile:v main10"), "{j}");
        assert!(j.contains("-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv"), "{j}");
        s.encoder = "libsvtav1".into();
        assert_eq!(encoder_pix_fmt_for(&s), "yuv420p10le");
        s.encoder = "h264_nvenc".into();
        assert_eq!(encoder_pix_fmt_for(&s), "nv12");
        assert!(joined(&s).contains("-colorspace bt709"));
        assert!(!supports_hdr("libx264") && !supports_hdr("hevc_mf") && supports_hdr("av1_qsv"));
    }

    #[test]
    fn quality_options_per_encoder() {
        let mk = |enc: &str, rc| EncoderSettings { encoder: enc.into(), rate_control: rc, b_frames: 2, ..EncoderSettings::streaming_default() };
        let j = joined(&mk("h264_nvenc", RateControl::Cbr));
        assert!(j.contains("-multipass qres") && j.contains("-profile:v high") && !j.contains("-rc-lookahead"), "{j}");
        let j = joined(&EncoderSettings { preset: "p7".into(), ..mk("hevc_nvenc", RateControl::Quality) });
        assert!(j.contains("-multipass fullres -rc-lookahead 20 -temporal-aq 1") && j.contains("-b_ref_mode middle"), "{j}");
        let j = joined(&mk("av1_nvenc", RateControl::Vbr));
        assert!(j.contains("-highbitdepth 1") && j.contains("-b_ref_mode middle"), "{j}");
        let j = joined(&mk("h264_qsv", RateControl::Cbr));
        assert!(j.contains("-adaptive_i 1 -adaptive_b 1 -b_strategy 1") && j.contains("-profile:v high"), "{j}");
        let j = joined(&mk("h264_amf", RateControl::Cbr));
        assert!(j.contains("-profile:v high -vbaq 1"), "{j}");
        assert!(joined(&mk("libsvtav1", RateControl::Cbr)).contains("-svtav1-params tune=0:rc=2"));
        assert!(joined(&mk("libsvtav1", RateControl::Quality)).contains("-crf 23 -svtav1-params tune=0"));
    }

    #[test]
    fn amf_and_qsv() {
        let mut s = EncoderSettings { encoder: "av1_amf".into(), preset: "quality".into(), ..EncoderSettings::streaming_default() };
        assert!(joined(&s).contains("-quality quality -rc cbr"));
        s.encoder = "h264_qsv".into();
        s.preset = "medium".into();
        s.rate_control = RateControl::Quality;
        assert!(joined(&s).contains("-global_quality 23"));
    }

    #[test]
    fn custom_args_split() {
        assert_eq!(split_custom(r#"-tune zerolatency -x264opts "a=1 b=2""#), vec![
            "-tune", "zerolatency", "-x264opts", "a=1 b=2"
        ]);
    }
}
