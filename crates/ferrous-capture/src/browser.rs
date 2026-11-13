//! Browser source: a headless Chromium-family browser (Microsoft Edge is on
//! every Windows 10/11 PC) driven over the Chrome DevTools Protocol.
//! Frames come from `Page.startScreencast` as PNGs with a transparent page
//! background, so overlays composite like they do in OBS.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ferrous_core::VideoFrame;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::Source;

/// Finds Edge or Chrome. `FERROUS_BROWSER` overrides.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FERROUS_BROWSER") {
        return Some(PathBuf::from(p));
    }
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    } else {
        &["/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/usr/bin/microsoft-edge"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

struct Cdp {
    ws: Ws,
    next_id: u64,
}

impl Cdp {
    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> anyhow::Result<u64> {
        self.next_id += 1;
        let mut msg = json!({ "id": self.next_id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws.send(Message::text(msg.to_string()))?;
        Ok(self.next_id)
    }

    /// Sends a command and waits for its result (events received meanwhile are dropped).
    fn call(&mut self, session: Option<&str>, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.send(session, method, params)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e.into()),
            };
            let Message::Text(t) = msg else { continue };
            let v: Value = serde_json::from_str(&t)?;
            if v["id"].as_u64() == Some(id) {
                if let Some(err) = v.get("error") {
                    anyhow::bail!("{method}: {err}");
                }
                return Ok(v["result"].clone());
            }
        }
        anyhow::bail!("{method}: no reply")
    }
}

/// Decodes a PNG to straight-alpha BGRA.
fn decode_png(bytes: &[u8]) -> Option<VideoFrame> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut data = img.into_raw();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(VideoFrame::new(w, h, data))
}

pub struct BrowserSource {
    latest: Arc<Mutex<Option<VideoFrame>>>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    child: Child,
    profile: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl BrowserSource {
    pub fn start(url: &str, width: u32, height: u32, fps: u32, css: &str) -> anyhow::Result<Self> {
        let exe = find_browser().ok_or_else(|| anyhow::anyhow!("Microsoft Edge or Google Chrome not found"))?;
        let (width, height) = (width.clamp(16, 3840), height.clamp(16, 2160));
        let profile = std::env::temp_dir().join(format!("ferrous-browser-{}-{}", std::process::id(), rand_suffix()));
        let mut cmd = Command::new(&exe);
        cmd.args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--hide-scrollbars",
            "--mute-audio",
            "--autoplay-policy=no-user-gesture-required",
            "--disable-background-timer-throttling",
            "--disable-renderer-backgrounding",
            "--disable-backgrounding-occluded-windows",
        ])
        .arg(format!("--window-size={width},{height}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        #[cfg(not(windows))]
        if unsafe { libc_geteuid() } == 0 {
            cmd.arg("--no-sandbox"); // Chromium refuses to run as root otherwise.
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("couldn't start {}: {e}", exe.display()))?;

        // Chromium prints "DevTools listening on ws://…" once ready.
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = crossbeam_channel::bounded::<String>(1);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(i) = line.find("ws://") {
                    let _ = tx.send(line[i..].trim().to_string());
                }
            }
        });
        let ws_url = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(u) => u,
            Err(_) => {
                let _ = child.kill();
                anyhow::bail!("browser didn't start");
            }
        };

        let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (l2, a2, s2) = (latest.clone(), alive.clone(), stop.clone());
        let (url, css) = (url.to_string(), css.to_string());
        let thread = std::thread::Builder::new().name("browser-source".into()).spawn(move || {
            if let Err(e) = run(&ws_url, &url, width, height, fps, &css, &l2, &s2) {
                log::warn!("browser source: {e}");
                #[cfg(test)]
                eprintln!("browser source error: {e}");
            }
            a2.store(false, Ordering::Relaxed);
        })?;
        Ok(Self { latest, alive, stop, child, profile, thread: Some(thread) })
    }
}

#[cfg(not(windows))]
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn run(
    ws_url: &str,
    url: &str,
    width: u32,
    height: u32,
    fps: u32,
    css: &str,
    latest: &Mutex<Option<VideoFrame>>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let (ws, _) = tungstenite::connect(ws_url)?;
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_millis(200)))?;
    }
    let mut cdp = Cdp { ws, next_id: 0 };
    let target = cdp.call(None, "Target.createTarget", json!({ "url": "about:blank" }))?;
    let target_id = target["targetId"].as_str().unwrap_or_default().to_string();
    let attached = cdp.call(None, "Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }))?;
    let session = attached["sessionId"].as_str().unwrap_or_default().to_string();
    let s = Some(session.as_str());
    cdp.call(s, "Page.enable", json!({}))?;
    cdp.call(s, "Emulation.setDeviceMetricsOverride", json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }))?;
    // Transparent page background, like OBS's browser source.
    cdp.call(s, "Emulation.setDefaultBackgroundColorOverride", json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }))?;
    if !css.trim().is_empty() {
        let js = format!(
            "(()=>{{const add=()=>{{const s=document.createElement('style');s.textContent={};(document.head||document.documentElement).appendChild(s)}};if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',add);else add();}})()",
            serde_json::to_string(css)?
        );
        cdp.call(s, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": js }))?;
    }
    // Chromium sends a frame per paint; skip frames to honour the fps cap.
    let every_nth = if fps == 0 || fps >= 60 { 1 } else { (60 / fps).max(1) };
    cdp.call(s, "Page.startScreencast", json!({ "format": "png", "maxWidth": width, "maxHeight": height, "everyNthFrame": every_nth }))?;
    // Navigate last: the screencast must already be running on the page.
    cdp.send(s, "Page.navigate", json!({ "url": url }))?;

    while !stop.load(Ordering::Relaxed) {
        let msg = match cdp.ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e.into()),
        };
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t)?;
        if v["method"] == "Page.screencastFrame" {
            let p = &v["params"];
            if let Some(ack) = p["sessionId"].as_i64() {
                cdp.send(s, "Page.screencastFrameAck", json!({ "sessionId": ack }))?;
            }
            if let Some(b64) = p["data"].as_str() {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                    if let Some(f) = decode_png(&bytes) {
                        *latest.lock() = Some(f);
                    }
                }
            }
        }
    }
    Ok(())
}

impl Source for BrowserSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        self.latest.lock().clone()
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn pids(&self) -> Vec<u32> {
        vec![self.child.id()]
    }
}

impl Drop for BrowserSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_page_with_transparent_background() {
        if find_browser().is_none() {
            eprintln!("no browser; skipping");
            return;
        }
        // A red box on an otherwise empty (transparent) page.
        let page = "data:text/html,<html><body style='margin:0'><div style='width:100px;height:50px;background:%23ff0000'></div></body></html>";
        let mut src = BrowserSource::start(page, 320, 180, 30, "").expect("start browser");
        let mut frame = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(f) = src.latest_frame() {
                // Wait for the first painted frame that contains the box.
                if f.data[2] > 200 {
                    frame = Some(f);
                    break;
                }
            }
        }
        let f = frame.expect("no frame with content");
        assert_eq!((f.width, f.height), (320, 180));
        assert_eq!(&f.data[..4], &[0, 0, 255, 255], "red box at top-left");
        let i = ((150 * 320 + 300) * 4) as usize;
        assert_eq!(f.data[i + 3], 0, "page background is transparent");
    }
}
