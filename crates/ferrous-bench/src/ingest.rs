//! Ingest server discovery and ranking.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;

pub const TWITCH_INGESTS_URL: &str = "https://ingest.twitch.tv/ingests";

#[derive(Clone, Debug, PartialEq)]
pub struct Ingest {
    pub name: String,
    /// Server URL without the stream key (what goes in the Server field).
    pub server: String,
    /// Twitch's own ordering; lower is preferred by Twitch.
    pub priority: i64,
    /// Redundancy endpoints are measured but never auto-selected.
    pub backup: bool,
}

#[derive(Clone, Debug)]
pub struct IngestResult {
    pub ingest: Ingest,
    /// Median TCP connect time.
    pub rtt_ms: Option<f32>,
    /// Max minus min over the samples.
    pub jitter_ms: Option<f32>,
    /// Sustained kbps from a bandwidth test, when run.
    pub bandwidth_kbps: Option<u32>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct TwitchIngest {
    name: String,
    url_template: String,
    #[serde(default)]
    priority: i64,
}

#[derive(Deserialize)]
struct TwitchIngests {
    ingests: Vec<TwitchIngest>,
}

/// Parses Twitch's `/ingests` response.
pub fn parse_twitch(json: &str) -> anyhow::Result<Vec<Ingest>> {
    let list: TwitchIngests = serde_json::from_str(json)?;
    Ok(list
        .ingests
        .into_iter()
        .map(|i| Ingest {
            name: i.name,
            server: i.url_template.replace("/{stream_key}", "").replace("{stream_key}", ""),
            priority: i.priority,
            backup: false,
        })
        .collect())
}

pub fn fetch_twitch() -> anyhow::Result<Vec<Ingest>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let text = agent.get(TWITCH_INGESTS_URL).call()?.body_mut().read_to_string()?;
    parse_twitch(&text)
}

/// YouTube's primary and backup RTMP ingest (the backup is for redundancy).
pub fn youtube() -> Vec<Ingest> {
    vec![
        Ingest { name: "YouTube primary".into(), server: "rtmp://a.rtmp.youtube.com/live2".into(), priority: 0, backup: false },
        Ingest { name: "YouTube backup (redundancy only)".into(), server: "rtmp://b.rtmp.youtube.com/live2?backup=1".into(), priority: 1, backup: true },
    ]
}

/// host and port of an rtmp(s)/srt URL.
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let hostport = rest.split(['/', '?']).next()?;
    let default = match scheme {
        "rtmps" => 443,
        "srt" => 9000,
        "https" => 443,
        "http" => 80,
        _ => 1935,
    };
    match hostport.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((hostport.to_string(), default)),
    }
}

/// Median TCP connect time and jitter over `samples` attempts.
pub fn measure_rtt(url: &str, samples: usize) -> Result<(f32, f32), String> {
    let (host, port) = host_port(url).ok_or("bad server URL")?;
    let addr = (host.as_str(), port).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("can't resolve")?;
    let mut times = Vec::new();
    for _ in 0..samples {
        let t = Instant::now();
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(s) => {
                times.push(t.elapsed().as_secs_f32() * 1000.0);
                drop(s);
            }
            Err(e) => return Err(e.to_string()),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    times.sort_by(f32::total_cmp);
    let median = times[times.len() / 2];
    Ok((median, times[times.len() - 1] - times[0]))
}

/// Measures every ingest in parallel (8 at a time) and sorts best first:
/// reachable primaries by RTT, with jitter as a tie-breaker.
pub fn rank(ingests: Vec<Ingest>, samples: usize) -> Vec<IngestResult> {
    let mut results: Vec<IngestResult> = Vec::new();
    for chunk in ingests.chunks(8) {
        let handles: Vec<_> = chunk
            .iter()
            .cloned()
            .map(|ing| {
                std::thread::spawn(move || {
                    let r = measure_rtt(&ing.server, samples);
                    IngestResult {
                        ingest: ing,
                        rtt_ms: r.as_ref().ok().map(|x| x.0),
                        jitter_ms: r.as_ref().ok().map(|x| x.1),
                        bandwidth_kbps: None,
                        error: r.err(),
                    }
                })
            })
            .collect();
        results.extend(handles.into_iter().filter_map(|h| h.join().ok()));
    }
    sort(&mut results);
    results
}

pub fn score(r: &IngestResult) -> f32 {
    match (r.rtt_ms, r.ingest.backup) {
        (Some(rtt), false) => rtt + r.jitter_ms.unwrap_or(0.0) * 0.5 - r.bandwidth_kbps.map(|b| b as f32 / 1000.0).unwrap_or(0.0),
        (Some(rtt), true) => 10_000.0 + rtt,
        (None, _) => f32::INFINITY,
    }
}

pub fn sort(results: &mut [IngestResult]) {
    results.sort_by(|a, b| score(a).total_cmp(&score(b)));
}

/// The server to use: the best-scoring reachable non-backup ingest.
pub fn fastest(results: &[IngestResult]) -> Option<&IngestResult> {
    results.iter().find(|r| r.rtt_ms.is_some() && !r.ingest.backup)
}

/// Twitch bandwidth test: streams a test pattern to the ingest with
/// `?bandwidthtest=true` (Twitch accepts it without going live) and
/// returns the kbps the connection sustained.
pub fn twitch_bandwidth_test(ffmpeg_path: &Path, server: &str, key: &str, target_kbps: u32, secs: u32) -> anyhow::Result<u32> {
    let url = format!("{}/{}?bandwidthtest=true", server.trim_end_matches('/'), key.trim());
    stream_test(ffmpeg_path, &url, target_kbps, secs)
}

/// Pushes a CBR test stream for `secs` and reports the average kbps achieved.
pub fn stream_test(ffmpeg_path: &Path, url: &str, target_kbps: u32, secs: u32) -> anyhow::Result<u32> {
    let br = format!("{target_kbps}k");
    let out = ferrous_encode::ffmpeg::command(ffmpeg_path)
        .args(["-hide_banner", "-nostdin", "-stats", "-re", "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=30"])
        .args(["-t", &secs.to_string(), "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"])
        .args(["-b:v", &br, "-minrate", &br, "-maxrate", &br, "-bufsize", &br, "-x264-params", "nal-hrd=cbr:filler=1"])
        .args(["-pix_fmt", "yuv420p", "-g", "60", "-f", "flv", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let text = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        anyhow::bail!("{}", text.lines().last().unwrap_or("stream test failed"));
    }
    // speed < 1.0 means the connection (not the encoder at ultrafast) held it back.
    let speed = crate::parse_speed(&text).unwrap_or(1.0).min(1.0);
    Ok((target_kbps as f32 * speed) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_twitch_list() {
        let json = r#"{"ingests":[
            {"_id":0,"availability":1.0,"default":false,"name":"US East: Ashburn, VA (5)","url_template":"rtmp://iad05.contribute.live-video.net/app/{stream_key}","priority":1},
            {"_id":1,"availability":1.0,"default":false,"name":"Europe: Frankfurt","url_template":"rtmp://fra.contribute.live-video.net/app/{stream_key}","priority":2}
        ]}"#;
        let l = parse_twitch(json).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].server, "rtmp://iad05.contribute.live-video.net/app");
        assert_eq!(host_port(&l[0].server), Some(("iad05.contribute.live-video.net".into(), 1935)));
        assert_eq!(host_port("rtmps://a.rtmps.youtube.com:443/live2"), Some(("a.rtmps.youtube.com".into(), 443)));
    }

    #[test]
    fn ranks_by_latency_and_never_picks_backup() {
        // Two local "ingests": one answers immediately, one is closed.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || for s in l.incoming() { drop(s) });
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ingests = vec![
            Ingest { name: "down".into(), server: format!("rtmp://127.0.0.1:{closed}/app"), priority: 0, backup: false },
            Ingest { name: "backup".into(), server: format!("rtmp://127.0.0.1:{port}/app"), priority: 1, backup: true },
            Ingest { name: "up".into(), server: format!("rtmp://127.0.0.1:{port}/app"), priority: 2, backup: false },
        ];
        let r = rank(ingests, 3);
        assert_eq!(r[0].ingest.name, "up");
        assert!(r[0].rtt_ms.unwrap() < 50.0);
        assert_eq!(fastest(&r).unwrap().ingest.name, "up");
        assert!(r.iter().find(|x| x.ingest.name == "down").unwrap().error.is_some());
    }

    /// Bandwidth test against a local FFmpeg RTMP listener.
    #[test]
    fn stream_test_measures_throughput() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let mut server = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-listen", "1", "-timeout", "15", "-i", "rtmp://127.0.0.1:19366/app/key", "-f", "null", "-"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(700));
        let kbps = stream_test(Path::new("ffmpeg"), "rtmp://127.0.0.1:19366/app/key", 3000, 3).unwrap();
        let _ = server.wait();
        println!("stream test: {kbps} kbps");
        assert!(kbps >= 2500, "{kbps}");
    }
}
