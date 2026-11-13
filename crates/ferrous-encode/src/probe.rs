//! Finding out which encoders actually work on this PC.
//!
//! FFmpeg builds list NVENC/QSV/AMF even when the hardware is absent, so
//! every listed encoder gets a tiny real test encode before it's offered.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::thread;

use crate::catalog::{EncoderInfo, ENCODERS};
use crate::ffmpeg;

#[derive(Clone, Debug)]
pub struct ProbeResult {
    pub ffmpeg_version: String,
    /// Encoders that passed a test encode, in catalog order.
    pub working: Vec<&'static EncoderInfo>,
    /// Encoders FFmpeg lists but which failed the test (e.g. no GPU), with reason.
    pub failed: Vec<(&'static EncoderInfo, String)>,
    pub audio: AudioSupport,
}

#[derive(Clone, Debug, Default)]
pub struct AudioSupport {
    pub opus: bool,
    pub flac: bool,
}

/// Parses `ffmpeg -encoders` output into a set of encoder names.
pub fn parse_encoder_list(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut in_table = false;
    for line in text.lines() {
        if line.trim_start().starts_with("------") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(flags), Some(name)) = (parts.next(), parts.next()) else { continue };
        if flags.len() == 6 {
            names.insert(name.to_string());
        }
    }
    names
}

fn test_encode(ffmpeg_path: &Path, enc: &EncoderInfo) -> Result<(), String> {
    let pix_fmt = if enc.vendor.is_hardware() { "nv12" } else { "yuv420p" };
    let mut cmd = ffmpeg::command(ffmpeg_path);
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    let hw = crate::args::hw_upload(enc.name);
    if let Some((global, _)) = &hw {
        cmd.args(global);
    }
    cmd.args(["-f", "lavfi", "-i", "color=c=black:s=640x360:r=30:d=0.2"]);
    match &hw {
        Some((_, upload)) => cmd.args(["-vf", &format!("format={pix_fmt},{upload}")]),
        None => cmd.args(["-pix_fmt", pix_fmt]),
    };
    let out = cmd
        .args(["-c:v", enc.name, "-frames:v", "5", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().last().unwrap_or("encoder failed").trim().to_string())
    }
}

/// Lists and test-encodes everything in the catalog. Takes a second or two;
/// run it off the UI thread.
pub fn probe(ffmpeg_path: &Path) -> anyhow::Result<ProbeResult> {
    let ffmpeg_version = ffmpeg::version(ffmpeg_path)?;
    let out = ffmpeg::command(ffmpeg_path)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output()?;
    let listed = parse_encoder_list(&String::from_utf8_lossy(&out.stdout));

    let candidates: Vec<&'static EncoderInfo> =
        ENCODERS.iter().filter(|e| listed.contains(e.name)).collect();

    // Hardware sessions are limited on consumer GPUs, so run tests per vendor
    // in parallel but encoders of the same vendor sequentially.
    let mut by_vendor: Vec<Vec<&'static EncoderInfo>> = Vec::new();
    for enc in &candidates {
        match by_vendor.iter_mut().find(|g| g[0].vendor == enc.vendor) {
            Some(g) => g.push(enc),
            None => by_vendor.push(vec![enc]),
        }
    }
    let results: Vec<(&'static EncoderInfo, Result<(), String>)> = thread::scope(|s| {
        let handles: Vec<_> = by_vendor
            .into_iter()
            .map(|group| {
                s.spawn(move || {
                    group.into_iter().map(|e| (e, test_encode(ffmpeg_path, e))).collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });

    let mut working = Vec::new();
    let mut failed = Vec::new();
    for enc in ENCODERS {
        if let Some((_, r)) = results.iter().find(|(e, _)| e.name == enc.name) {
            match r {
                Ok(()) => working.push(enc),
                Err(why) => failed.push((enc, why.clone())),
            }
        }
    }

    Ok(ProbeResult {
        ffmpeg_version,
        working,
        failed,
        audio: AudioSupport { opus: listed.contains("libopus"), flac: listed.contains("flac") },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_encoder_table() {
        let text = "Encoders:\n V..... = Video\n ------\n V....D libx264              libx264 H.264\n V....D h264_nvenc           NVIDIA NVENC\n A....D aac                  AAC\n";
        let set = parse_encoder_list(text);
        assert!(set.contains("libx264"));
        assert!(set.contains("h264_nvenc"));
        assert!(set.contains("aac"));
        assert!(!set.contains("V....."));
    }
}
