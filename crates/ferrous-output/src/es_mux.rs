//! Wraps the GPU encoder's compressed frames in a minimal container with exact timestamps before they
//! go to FFmpeg: MPEG-TS for H.264/HEVC, IVF for AV1.
//!
//! A bare elementary stream has no timestamps, and recent FFmpeg schedules its inputs by timestamp:
//! with none on the video it stopped reading the audio input after about a second, so recordings had
//! no sound. With these containers every frame carries `n / fps`, exactly as the raw-frame path does.

/// Which container, from the encoder's elementary-stream format name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EsKind {
    H264,
    Hevc,
    Av1,
}

impl EsKind {
    pub fn from_format(f: &str) -> Option<Self> {
        match f {
            "h264" => Some(Self::H264),
            "hevc" => Some(Self::Hevc),
            "obu" | "av1" => Some(Self::Av1),
            _ => None,
        }
    }

    /// The FFmpeg input format for what [`EsMux`] writes.
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Self::H264 | Self::Hevc => "mpegts",
            Self::Av1 => "ivf",
        }
    }
}

const TS: usize = 188;
const PID_PMT: u16 = 0x1000;
const PID_VIDEO: u16 = 0x100;

/// Turns compressed frames (one access unit each, in order, one per 1/fps) into a byte stream.
pub struct EsMux {
    kind: EsKind,
    fps: u32,
    size: (u32, u32),
    frame: u64,
    cc_video: u8,
    cc_pat: u8,
    cc_pmt: u8,
}

impl EsMux {
    pub fn new(kind: EsKind, fps: u32, size: (u32, u32)) -> Self {
        Self { kind, fps: fps.max(1), size, frame: 0, cc_video: 0, cc_pat: 0, cc_pmt: 0 }
    }

    /// Appends the next frame (in decode order) to `out`. `pts`/`dts` count frames from the first;
    /// with B-frames `dts` trails `pts` and may start negative.
    pub fn frame(&mut self, data: &[u8], pts: u64, dts: i64, keyframe: bool, out: &mut Vec<u8>) {
        match self.kind {
            EsKind::Av1 => self.ivf(data, pts, out),
            EsKind::H264 | EsKind::Hevc => self.ts(data, pts, dts, keyframe, out),
        }
        self.frame += 1;
    }

    /// For streams without B-frames: frames in order, every one a display frame.
    pub fn next(&mut self, data: &[u8], keyframe: bool, out: &mut Vec<u8>) {
        let n = self.frame;
        self.frame(data, n, n as i64, keyframe, out);
    }

    fn ivf(&mut self, data: &[u8], pts: u64, out: &mut Vec<u8>) {
        if self.frame == 0 {
            out.extend_from_slice(b"DKIF");
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&32u16.to_le_bytes());
            out.extend_from_slice(b"AV01");
            out.extend_from_slice(&(self.size.0.min(65535) as u16).to_le_bytes());
            out.extend_from_slice(&(self.size.1.min(65535) as u16).to_le_bytes());
            out.extend_from_slice(&self.fps.to_le_bytes()); // time base denominator
            out.extend_from_slice(&1u32.to_le_bytes()); // numerator
            out.extend_from_slice(&0u32.to_le_bytes()); // frame count (unknown)
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&pts.to_le_bytes());
        out.extend_from_slice(data);
    }

    fn ts(&mut self, data: &[u8], pts: u64, dts: i64, keyframe: bool, out: &mut Vec<u8>) {
        // Tables at the start and twice a second, so a reader always finds them quickly.
        if self.frame.is_multiple_of(u64::from(self.fps.div_ceil(2))) {
            self.tables(out);
        }
        // 90 kHz clock, offset half a second so decode times before the first frame (B-frames) stay
        // positive.
        let fps = i64::from(self.fps);
        let to90 = |n: i64| (45_000 + n * 90_000 / fps).max(0) as u64;
        let (pts, dts) = (to90(pts as i64), to90(dts.min(pts as i64)));
        let mut pes = Vec::with_capacity(data.len() + 19);
        pes.extend_from_slice(&[0, 0, 1, 0xE0, 0, 0]); // video stream, unbounded length
        if dts == pts {
            pes.extend_from_slice(&[0x80, 0x80, 5]);
            pes.extend_from_slice(&pts_bytes(0x2, pts));
        } else {
            pes.extend_from_slice(&[0x80, 0xC0, 10]);
            pes.extend_from_slice(&pts_bytes(0x3, pts));
            pes.extend_from_slice(&pts_bytes(0x1, dts));
        }
        pes.extend_from_slice(data);

        let mut rest: &[u8] = &pes;
        let mut first = true;
        while !rest.is_empty() {
            let mut pkt = [0xFFu8; TS];
            pkt[0] = 0x47;
            pkt[1] = (if first { 0x40 } else { 0 }) | ((PID_VIDEO >> 8) as u8 & 0x1F);
            pkt[2] = PID_VIDEO as u8;
            // Adaptation field: the PCR on a PES start, stuffing when the payload doesn't fill it.
            let mut af: Vec<u8> = Vec::new();
            if first {
                // PCR, and the random-access flag on keyframes (where a receiver can start).
                af.push(if keyframe { 0x50 } else { 0x10 });
                af.extend_from_slice(&pcr_bytes(dts.saturating_sub(90_000 / 20)));
            }
            let room = |af: &Vec<u8>| TS - 4 - if af.is_empty() { 0 } else { 1 + af.len() };
            if rest.len() < room(&af) {
                let need = room(&af) - rest.len();
                if af.is_empty() {
                    // An adaptation field of length L takes L+1 bytes.
                    if need == 1 {
                        // Length 0: just the length byte.
                        pkt[3] = 0x30 | (self.cc_video & 0x0F);
                        pkt[4] = 0;
                        pkt[5..5 + rest.len()].copy_from_slice(rest);
                        out.extend_from_slice(&pkt);
                        self.cc_video = self.cc_video.wrapping_add(1);
                        break;
                    }
                    af.push(0x00);
                    af.extend(std::iter::repeat_n(0xFF, need - 2));
                } else {
                    af.extend(std::iter::repeat_n(0xFF, need));
                }
            }
            let payload_at = if af.is_empty() {
                pkt[3] = 0x10 | (self.cc_video & 0x0F);
                4
            } else {
                pkt[3] = 0x30 | (self.cc_video & 0x0F);
                pkt[4] = af.len() as u8;
                pkt[5..5 + af.len()].copy_from_slice(&af);
                5 + af.len()
            };
            let n = (TS - payload_at).min(rest.len());
            pkt[payload_at..payload_at + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            out.extend_from_slice(&pkt);
            self.cc_video = self.cc_video.wrapping_add(1);
            first = false;
        }
    }

    fn tables(&mut self, out: &mut Vec<u8>) {
        // PAT: program 1 -> PMT.
        let mut pat = vec![0x00, 0xB0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE0 | (PID_PMT >> 8) as u8, PID_PMT as u8];
        finish_section(&mut pat);
        section_packet(0, &mut self.cc_pat, &pat, out);
        // PMT: one video stream, which also carries the PCR.
        let stream_type = if self.kind == EsKind::Hevc { 0x24 } else { 0x1B };
        let mut pmt = vec![
            0x02,
            0xB0,
            0,
            0x00,
            0x01,
            0xC1,
            0x00,
            0x00,
            0xE0 | (PID_VIDEO >> 8) as u8,
            PID_VIDEO as u8,
            0xF0,
            0x00,
            stream_type,
            0xE0 | (PID_VIDEO >> 8) as u8,
            PID_VIDEO as u8,
            0xF0,
            0x00,
        ];
        finish_section(&mut pmt);
        section_packet(PID_PMT, &mut self.cc_pmt, &pmt, out);
    }
}

fn pts_bytes(prefix: u8, pts: u64) -> [u8; 5] {
    let p = pts & 0x1_FFFF_FFFF;
    [
        (prefix << 4) | (((p >> 30) as u8 & 0x07) << 1) | 1,
        (p >> 22) as u8,
        (((p >> 15) as u8 & 0x7F) << 1) | 1,
        (p >> 7) as u8,
        ((p as u8 & 0x7F) << 1) | 1,
    ]
}

fn pcr_bytes(base: u64) -> [u8; 6] {
    let b = base & 0x1_FFFF_FFFF;
    [(b >> 25) as u8, (b >> 17) as u8, (b >> 9) as u8, (b >> 1) as u8, ((b as u8 & 1) << 7) | 0x7E, 0]
}

/// Fills in the section length and appends the CRC.
fn finish_section(s: &mut Vec<u8>) {
    let len = s.len() - 3 + 4;
    s[1] = 0xB0 | ((len >> 8) as u8 & 0x0F);
    s[2] = len as u8;
    let crc = crc32_mpeg2(s);
    s.extend_from_slice(&crc.to_be_bytes());
}

fn section_packet(pid: u16, cc: &mut u8, section: &[u8], out: &mut Vec<u8>) {
    let mut pkt = [0xFFu8; TS];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = pid as u8;
    pkt[3] = 0x10 | (*cc & 0x0F);
    pkt[4] = 0; // pointer field
    pkt[5..5 + section.len()].copy_from_slice(section);
    out.extend_from_slice(&pkt);
    *cc = cc.wrapping_add(1);
}

fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn every_ts_packet_is_188_bytes_with_sync() {
        let mut m = EsMux::new(EsKind::Hevc, 60, (1920, 1080));
        let mut out = Vec::new();
        for n in [1usize, 2, 183, 184, 185, 1000, 50_000] {
            m.next(&vec![7u8; n], false, &mut out);
        }
        assert_eq!(out.len() % TS, 0);
        assert!(out.chunks(TS).all(|p| p[0] == 0x47));
    }

    /// B-frames: real x264 output with 2 B-frames, its decode-order packets given the encoder's own
    /// pts/dts, must come back from FFmpeg with every frame, in display order, at the right time.
    #[test]
    fn b_frames_keep_their_timing() {
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("esmux-b-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (es, mkv) = (dir.join("v.h264"), dir.join("ref.ts"));
        // Plain names, run inside the folder: a Windows path's `:` and `\` mean something else in a tee list.
        let tee = "[f=h264]v.h264|[f=mpegts]ref.ts";
        let ok = Command::new("ffmpeg")
            .current_dir(&dir)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x180:r=30:d=2", "-c:v", "libx264", "-bf", "2", "-x264-params", "aud=1", "-map", "0", "-f", "tee", tee])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("skipping: no ffmpeg/libx264");
            return;
        }
        // The encoder's timestamps, per packet in decode order.
        let probe = Command::new("ffprobe").args(["-v", "error", "-show_entries", "packet=pts_time,dts_time,flags", "-of", "csv=p=0"]).arg(&mkv).output().unwrap();
        let times: Vec<(f64, f64, bool)> = String::from_utf8_lossy(&probe.stdout)
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                Some((f.first()?.parse().ok()?, f.get(1)?.parse().ok()?, f.get(2)?.contains('K')))
            })
            .collect();
        let data = std::fs::read(&es).unwrap();
        let mut starts = Vec::new();
        for i in 0..data.len().saturating_sub(4) {
            if data[i..i + 3] == [0, 0, 1] && data[i + 3] & 0x1F == 9 {
                starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
            }
        }
        starts.push(data.len());
        assert_eq!(starts.len() - 1, times.len(), "access units vs packets");
        let base = times.iter().map(|t| t.0).fold(f64::MAX, f64::min);
        let mut m = EsMux::new(EsKind::H264, 30, (320, 180));
        let mut out = Vec::new();
        for (w, &(pts, dts, key)) in starts.windows(2).zip(&times) {
            let p = ((pts - base) * 30.0).round() as u64;
            let d = ((dts - base) * 30.0).round() as i64;
            m.frame(&data[w[0]..w[1]], p, d, key, &mut out);
        }
        let ts = dir.join("v.ts");
        std::fs::write(&ts, &out).unwrap();
        let mux = dir.join("out.mkv");
        let st = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y", "-f", "mpegts", "-i"]).arg(&ts).args(["-c", "copy"]).arg(&mux).status().unwrap();
        assert!(st.success());
        // Decoding gives every frame once, at 1/30 s steps from 0.
        let frames = Command::new("ffprobe").args(["-v", "error", "-show_entries", "frame=pts_time", "-of", "csv=p=0"]).arg(&mux).output().unwrap();
        let t: Vec<f64> = String::from_utf8_lossy(&frames.stdout).lines().filter_map(|l| l.split(',').next()?.trim().parse().ok()).collect();
        assert_eq!(t.len(), 60, "decoded frames");
        for (i, w) in t.windows(2).enumerate() {
            assert!((w[1] - w[0] - 1.0 / 30.0).abs() < 0.002, "frame {i}: {} -> {}", w[0], w[1]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Muxes real encoder output through FFmpeg and checks frame count and duration (skipped when
    /// FFmpeg or the encoders aren't installed).
    #[test]
    fn ffmpeg_reads_exact_timing() {
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("esmux-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        for (kind, enc, params, aud) in [(EsKind::H264, "libx264", "-x264-params", "aud=1"), (EsKind::Hevc, "libx265", "-x265-params", "aud=1:log-level=0")] {
            let es = dir.join("v.es");
            let ok = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x180:r=30:d=2", "-c:v", enc, "-bf", "0", params, aud, "-f", if kind == EsKind::H264 { "h264" } else { "hevc" }])
                .arg(&es)
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                eprintln!("skipping {kind:?}: no ffmpeg/{enc}");
                continue;
            }
            let data = std::fs::read(&es).unwrap();
            let aud_type = if kind == EsKind::H264 { 9 } else { 35 };
            let mut starts = Vec::new();
            for i in 0..data.len().saturating_sub(4) {
                if data[i..i + 3] == [0, 0, 1] {
                    let t = if kind == EsKind::H264 { data[i + 3] & 0x1F } else { (data[i + 3] >> 1) & 0x3F };
                    if t == aud_type {
                        starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
                    }
                }
            }
            starts.push(data.len());
            let mut m = EsMux::new(kind, 30, (320, 180));
            let mut out = Vec::new();
            for (k, w) in starts.windows(2).enumerate() {
                m.next(&data[w[0]..w[1]], k == 0, &mut out);
            }
            let ts = dir.join("v.ts");
            std::fs::write(&ts, &out).unwrap();
            let mkv = dir.join("v.mkv");
            let st = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y", "-f", "mpegts", "-i"]).arg(&ts).args(["-c", "copy"]).arg(&mkv).status().unwrap();
            assert!(st.success(), "{kind:?}: ffmpeg couldn't read the stream");
            let probe = Command::new("ffprobe")
                .args(["-v", "error", "-count_packets", "-show_entries", "stream=nb_read_packets:format=duration", "-of", "csv=p=0"])
                .arg(&mkv)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&probe.stdout);
            assert!(text.contains("60"), "{kind:?}: expected 60 frames, got {text}");
            let dur: f64 = text.lines().last().unwrap().trim().parse().unwrap();
            assert!((dur - 2.0).abs() < 0.05, "{kind:?}: duration {dur}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
