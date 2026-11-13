//! Recording benchmark: encodes the same test footage with each candidate
//! settings set and measures speed, size and picture quality, so settings
//! can be chosen on evidence.
//!
//! - **Footage** is rendered once to a lossless temp file (so generating it
//!   never slows the encoders being timed) from either a built-in test
//!   scene (a zooming fractal blended with a test card: fine detail plus
//!   constant motion, harder than most games) or a clip the user picks.
//! - **Speed** is FFmpeg's reported encode speed against the clip's frame rate.
//! - **Quality** compares each encode with the lossless reference: SSIM and
//!   PSNR always, VMAF when this FFmpeg build has `libvmaf`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrous_core::{EncoderSettings, RateControl};
use ferrous_encode::{args, catalog, ffmpeg};

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// The built-in test scene.
    Synthetic,
    /// A video the user picked (a gameplay clip is the best test).
    Clip(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Footage length per trial.
    pub secs: f32,
    pub source: Source,
}

#[derive(Clone, Debug)]
pub struct Trial {
    pub label: String,
    pub encoder: EncoderSettings,
}

#[derive(Clone, Debug)]
pub struct TrialResult {
    pub label: String,
    pub encoder: EncoderSettings,
    /// Frames per second the encoder sustained.
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub ssim: f32,
    pub psnr: f32,
    pub vmaf: Option<f32>,
    /// Target frame rate of the footage, for "realtime" checks.
    pub target_fps: u32,
}

impl TrialResult {
    /// × real time: 1.0 means exactly keeping up.
    pub fn speed(&self) -> f32 {
        self.fps / self.target_fps.max(1) as f32
    }

    /// Whether it keeps up with room to spare for the game and the rest of
    /// Ferrous (25% headroom).
    pub fn realtime(&self) -> bool {
        self.speed() >= 1.25
    }

    /// One comparable number: VMAF (0-100) when measured, otherwise SSIM
    /// in decibels (−10·log10(1−SSIM); higher is better, 20 dB ≈ 0.99).
    pub fn quality(&self) -> f32 {
        self.vmaf.unwrap_or_else(|| ssim_db(self.ssim))
    }

    /// Smallest quality gap that is worth caring about, in `quality()` units.
    pub fn noticeable(&self) -> f32 {
        if self.vmaf.is_some() { 1.0 } else { 0.5 }
    }
}

pub fn ssim_db(ssim: f32) -> f32 {
    (-10.0 * (1.0 - ssim.clamp(0.0, 0.999_999)).log10()).min(60.0)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picks {
    /// Index of the best-looking trial that still keeps up in real time.
    pub best_quality: Option<usize>,
    /// The lightest trial within a noticeable difference of the best.
    pub best_value: Option<usize>,
    /// The fastest trial that keeps up.
    pub lightest: Option<usize>,
}

/// Chooses from the trials that keep up in real time.
pub fn recommend(results: &[TrialResult]) -> Picks {
    let ok: Vec<usize> = (0..results.len()).filter(|&i| results[i].realtime()).collect();
    let cmp = |a: &f32, b: &f32| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
    let best_quality = ok.iter().copied().max_by(|&a, &b| cmp(&results[a].quality(), &results[b].quality()));
    let lightest = ok.iter().copied().max_by(|&a, &b| cmp(&results[a].fps, &results[b].fps));
    let best_value = best_quality.and_then(|b| {
        let floor = results[b].quality() - results[b].noticeable();
        ok.iter().copied().filter(|&i| results[i].quality() >= floor).max_by(|&a, &b| cmp(&results[a].fps, &results[b].fps))
    });
    Picks { best_quality, best_value, lightest }
}

/// The same settings forced to one bitrate (VBR), so the comparison shows
/// how efficiently each encoder uses bits rather than how many it spends.
pub fn at_bitrate(e: &EncoderSettings, kbps: u32) -> EncoderSettings {
    EncoderSettings { rate_control: RateControl::Vbr, bitrate_kbps: kbps, ..e.clone() }
}

/// Whether this FFmpeg can compute VMAF.
pub fn has_vmaf(ffmpeg_path: &Path) -> bool {
    ffmpeg::command(ffmpeg_path)
        .args(["-hide_banner", "-filters"])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("libvmaf"))
        .unwrap_or(false)
}

/// lavfi graph of the built-in test scene.
fn synthetic_graph(spec: &Spec) -> String {
    let (w, h, f) = (spec.width, spec.height, spec.fps);
    format!("mandelbrot=s={w}x{h}:r={f}:maxiter=300:end_scale=0.01[a];testsrc2=s={w}x{h}:r={f}[b];[a][b]blend=all_mode=average,format=yuv420p")
}

/// Renders the lossless reference footage into `dir`.
pub fn prepare_reference(ffmpeg_path: &Path, spec: &Spec, dir: &Path) -> anyhow::Result<PathBuf> {
    let out = dir.join("reference.mkv");
    let mut cmd = ffmpeg::command(ffmpeg_path);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
    match &spec.source {
        Source::Synthetic => {
            cmd.args(["-f", "lavfi", "-i"]).arg(synthetic_graph(spec));
        }
        Source::Clip(p) => {
            cmd.arg("-i").arg(p);
        }
    }
    cmd.args(["-t", &spec.secs.to_string()]);
    if matches!(spec.source, Source::Clip(_)) {
        cmd.args(["-vf", &format!("scale={}:{}:flags=bicubic,fps={},format=yuv420p", spec.width, spec.height, spec.fps)]);
    }
    let res = cmd.args(["-an", "-c:v", "utvideo"]).arg(&out).stdin(Stdio::null()).output()?;
    if !res.status.success() || !out.exists() {
        anyhow::bail!("couldn't prepare the test footage: {}", String::from_utf8_lossy(&res.stderr).lines().last().unwrap_or("unknown error"));
    }
    Ok(out)
}

/// Encodes the reference with `trial` and measures it.
pub fn run_trial(ffmpeg_path: &Path, spec: &Spec, reference: &Path, trial: &Trial, vmaf: bool) -> anyhow::Result<TrialResult> {
    let dir = reference.parent().unwrap_or(Path::new("."));
    let encoded = dir.join("trial.mkv");
    let _ = std::fs::remove_file(&encoded);
    let es = &trial.encoder;
    let hw = args::hw_upload(&es.encoder);
    let mut vf = format!("format={}", args::encoder_pix_fmt_for(es));
    if let Some((_, f)) = &hw {
        vf.push(',');
        vf.push_str(f);
    }
    let mut cmd = ffmpeg::command(ffmpeg_path);
    cmd.args(["-hide_banner", "-nostdin", "-stats", "-y"]);
    if let Some((global, _)) = &hw {
        cmd.args(global);
    }
    cmd.arg("-i").arg(reference).args(["-an", "-vf", &vf]).args(args::video_args(es, spec.fps)).arg(&encoded);
    let out = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
    let text = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        anyhow::bail!("{} failed: {}", es.encoder, text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default());
    }
    let speed = crate::parse_speed(&text).ok_or_else(|| anyhow::anyhow!("no speed reported"))?;
    let bytes = std::fs::metadata(&encoded)?.len() as f32;
    let bitrate_kbps = bytes * 8.0 / spec.secs.max(0.1) / 1000.0;

    let measure = |lavfi: &str| -> anyhow::Result<String> {
        let o = ffmpeg::command(ffmpeg_path)
            .args(["-hide_banner", "-nostdin", "-i"])
            .arg(&encoded)
            .arg("-i")
            .arg(reference)
            .args(["-lavfi", lavfi, "-f", "null", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        Ok(String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let norm = "[0:v]format=yuv420p[d];[1:v]format=yuv420p[r];";
    let ssim = parse_ssim(&measure(&format!("{norm}[d][r]ssim"))?).ok_or_else(|| anyhow::anyhow!("couldn't measure SSIM"))?;
    let psnr = parse_psnr(&measure(&format!("{norm}[d][r]psnr"))?).unwrap_or(0.0);
    let vmaf = if vmaf { parse_vmaf(&measure(&format!("{norm}[d][r]libvmaf=n_threads=4:n_subsample=2"))?) } else { None };
    let _ = std::fs::remove_file(&encoded);
    Ok(TrialResult { label: trial.label.clone(), encoder: es.clone(), fps: speed * spec.fps as f32, bitrate_kbps, ssim, psnr, vmaf, target_fps: spec.fps })
}

fn number_after(text: &str, key: &str) -> Option<f32> {
    let rest = text.rsplit(key).next().filter(|_| text.contains(key))?;
    rest.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).find(|t| !t.is_empty())?.parse().ok()
}

pub fn parse_ssim(stderr: &str) -> Option<f32> {
    number_after(stderr, " All:")
}

pub fn parse_psnr(stderr: &str) -> Option<f32> {
    number_after(stderr, "average:")
}

pub fn parse_vmaf(stderr: &str) -> Option<f32> {
    number_after(stderr, "VMAF score: ")
}

#[derive(Clone, Debug)]
pub enum Event {
    Preparing,
    Started { index: usize, total: usize, label: String },
    Finished { index: usize, result: Result<TrialResult, String> },
}

/// Runs every trial in turn (one at a time, so timings are honest).
/// Stops early when `cancel` is set. Returns the results that completed.
pub fn run_all(ffmpeg_path: &Path, spec: &Spec, trials: &[Trial], cancel: &AtomicBool, mut on_event: impl FnMut(Event)) -> anyhow::Result<Vec<TrialResult>> {
    let dir = std::env::temp_dir().join(format!("ferrous-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let mut run = || -> anyhow::Result<Vec<TrialResult>> {
        on_event(Event::Preparing);
        let reference = prepare_reference(ffmpeg_path, spec, &dir)?;
        let vmaf = has_vmaf(ffmpeg_path);
        let mut done = Vec::new();
        for (i, t) in trials.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            on_event(Event::Started { index: i, total: trials.len(), label: t.label.clone() });
            let r = run_trial(ffmpeg_path, spec, &reference, t, vmaf);
            on_event(Event::Finished { index: i, result: r.as_ref().map(|x| x.clone()).map_err(|e| e.to_string()) });
            if let Ok(r) = r {
                done.push(r);
            }
        }
        Ok(done)
    };
    let result = run();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Trials for every working encoder's presets at one rate control: for the
/// "find the best encoder/preset" sweep. `levels` picks which preset names
/// to try (first, middle, last of the encoder's own list when `None`).
pub fn sweep(working: &[&'static catalog::EncoderInfo], base: &EncoderSettings) -> Vec<Trial> {
    let mut trials = Vec::new();
    for e in working.iter().filter(|e| e.name != "libaom-av1") {
        let names: Vec<&str> = if e.presets.is_empty() { vec![""] } else { vec![e.presets[0], e.presets[e.presets.len() / 2], e.presets[e.presets.len() - 1]] };
        for name in names {
            let mut es = EncoderSettings { encoder: e.name.to_string(), preset: name.to_string(), hdr: false, ..base.clone() };
            if e.vendor.is_hardware() && matches!(es.rate_control, RateControl::Cbr) {
                es.b_frames = base.b_frames;
            }
            let label = if name.is_empty() { e.name.to_string() } else { format!("{} {name}", e.name) };
            trials.push(Trial { label, encoder: es });
        }
    }
    trials
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filter_summaries() {
        let s = "[Parsed_ssim_2 @ 0x1] SSIM Y:0.991 (20.4) U:0.99 (20.0) V:0.99 (20.0) All:0.990321 (20.1)\n";
        assert_eq!(parse_ssim(s), Some(0.990321));
        let p = "[Parsed_psnr_2 @ 0x1] PSNR y:41.2 u:44.0 v:44.1 average:42.05 min:38.0 max:48.0\n";
        assert_eq!(parse_psnr(p), Some(42.05));
        assert_eq!(parse_vmaf("[libvmaf @ 0x1] VMAF score: 95.250000\n"), Some(95.25));
        assert_eq!(parse_ssim("nothing here"), None);
    }

    fn result(label: &str, fps: f32, ssim: f32) -> TrialResult {
        TrialResult { label: label.into(), encoder: EncoderSettings::default(), fps, bitrate_kbps: 1.0, ssim, psnr: 40.0, vmaf: None, target_fps: 60 }
    }

    #[test]
    fn recommendations_ignore_trials_that_cannot_keep_up() {
        let r = [
            result("slow-best", 50.0, 0.999), // too slow: below real time
            result("quality", 90.0, 0.995),
            result("value", 240.0, 0.9945),
            result("fast-soft", 400.0, 0.98),
        ];
        let p = recommend(&r);
        assert_eq!(p.best_quality, Some(1));
        // within 0.5 dB of the best, and much faster
        assert_eq!(p.best_value, Some(2));
        assert_eq!(p.lightest, Some(3));
        assert_eq!(recommend(&[result("x", 10.0, 0.9)]), Picks::default());
    }

    #[test]
    fn ssim_decibels() {
        assert!((ssim_db(0.99) - 20.0).abs() < 0.01);
        assert!(ssim_db(1.0) <= 60.0);
    }

    #[test]
    fn sweep_tries_a_few_presets_per_encoder() {
        let w = [catalog::find("libx264").unwrap(), catalog::find("hevc_vulkan").unwrap(), catalog::find("libaom-av1").unwrap()];
        let t = sweep(&w, &EncoderSettings::recording_default());
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].encoder.preset, "ultrafast");
        assert_eq!(t[3].label, "hevc_vulkan");
    }

    /// End to end with the real FFmpeg, when there is one with libx264.
    #[test]
    fn a_better_setting_measures_better_and_slower() {
        let ff = Path::new("ffmpeg");
        if ffmpeg::version(ff).is_err() {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let spec = Spec { width: 320, height: 180, fps: 30, secs: 1.0, source: Source::Synthetic };
        let mk = |q: u32, preset: &str| Trial {
            label: format!("crf{q}"),
            encoder: EncoderSettings { encoder: "libx264".into(), rate_control: RateControl::Quality, quality: q, preset: preset.into(), b_frames: 2, ..EncoderSettings::default() },
        };
        let trials = [mk(16, "veryfast"), mk(38, "veryfast")];
        let mut events = 0;
        let r = match run_all(ff, &spec, &trials, &AtomicBool::new(false), |_| events += 1) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ffmpeg can't run the benchmark here ({e}); skipping");
                return;
            }
        };
        assert_eq!(r.len(), 2);
        assert_eq!(events, 5);
        assert!(r[0].quality() > r[1].quality() + 3.0, "{} vs {}", r[0].quality(), r[1].quality());
        assert!(r[0].bitrate_kbps > r[1].bitrate_kbps);
        assert!(r.iter().all(|x| x.fps > 0.0 && x.ssim > 0.0 && x.ssim <= 1.0 && x.psnr > 10.0));
    }
}
