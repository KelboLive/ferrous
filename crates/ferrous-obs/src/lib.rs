//! OBS plugin bridge (Ferrous side).
//!
//! OBS plugins are hosted by `ferrous-obs-host`, a helper process that loads
//! the real libobs from an OBS installation, initialises it without a UI,
//! loads the installed plugins and runs one plugin source on its own canvas.
//! Ferrous talks to it with JSON lines on stdin/stdout (list types, create,
//! properties, update, button clicks) and receives the source's video
//! (BGRA) and audio (48 kHz stereo float) over a loopback socket.
//!
//! A helper process keeps a crashing plugin from taking Ferrous down: the
//! source goes offline and the engine restarts it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
#[cfg(not(windows))]
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use ferrous_core::VideoFrame;
use parking_lot::Mutex;

pub mod protocol;
pub use protocol::*;

/// The helper next to Ferrous's executable.
pub fn host_path() -> PathBuf {
    if let Some(p) = std::env::var_os("FERROUS_OBS_HOST") {
        return p.into();
    }
    let name = if cfg!(windows) { "ferrous-obs-host.exe" } else { "ferrous-obs-host" };
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).unwrap_or_else(|| name.into())
}

/// Where OBS is installed (`FERROUS_OBS_DIR`, else the standard location).
pub fn obs_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("FERROUS_OBS_DIR") {
        return Some(d.into());
    }
    #[cfg(windows)]
    {
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| "C:\\Program Files".into());
        let d = pf.join("obs-studio");
        d.join("bin").join("64bit").join("obs.dll").exists().then_some(d)
    }
    #[cfg(not(windows))]
    {
        ["/usr/lib/x86_64-linux-gnu/libobs.so.0", "/usr/lib/libobs.so.0", "/usr/lib64/libobs.so.0"]
            .iter()
            .find(|p| Path::new(p).exists())
            .map(|_| PathBuf::from("/usr"))
    }
}

/// A running host process.
pub struct Host {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    replies: Mutex<std::sync::mpsc::Receiver<Reply>>,
    alive: Arc<AtomicBool>,
    pub latest: Arc<Mutex<Option<VideoFrame>>>,
}

pub type AudioSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

impl Host {
    /// Starts a host (with a media connection if `audio`/video are wanted).
    pub fn spawn(audio: Option<AudioSink>) -> anyhow::Result<Arc<Host>> {
        let exe = host_path();
        let obs = obs_dir().ok_or_else(|| anyhow!("OBS Studio isn't installed (or set FERROUS_OBS_DIR)"))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let mut child = Command::new(&exe)
            .arg("--obs-dir")
            .arg(&obs)
            .arg("--media-port")
            .arg(port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // libobs logs a lot; keep it out of Ferrous's console but on disk for support.
            .stderr(std::fs::File::create(std::env::temp_dir().join("ferrous-obs-host.log")).map(Stdio::from).unwrap_or(Stdio::null()))
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let alive = alive.clone();
            std::thread::Builder::new().name("obs-host-control".into()).spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    match serde_json::from_str::<Reply>(&line) {
                        Ok(r) => {
                            if tx.send(r).is_err() {
                                break;
                            }
                        }
                        Err(_) => log::info!("obs-host: {line}"),
                    }
                }
                alive.store(false, Ordering::Relaxed);
            })?;
        }
        let latest = Arc::new(Mutex::new(None));
        {
            let (latest, alive) = (latest.clone(), alive.clone());
            listener.set_nonblocking(false)?;
            std::thread::Builder::new().name("obs-host-media".into()).spawn(move || {
                let Ok((sock, _)) = listener.accept() else { return };
                media_loop(sock, latest, audio);
                alive.store(false, Ordering::Relaxed);
            })?;
        }
        let host = Arc::new(Host { child: Mutex::new(child), stdin: Mutex::new(stdin), replies: Mutex::new(rx), alive, latest });
        // The host says hello once libobs is up (loading plugins takes a moment).
        match host.wait_reply(Duration::from_secs(30))? {
            Reply::Ready { version } => log::info!("OBS plugin host ready (libobs {version})"),
            Reply::Error { message } => bail!("OBS plugin host: {message}"),
            other => bail!("unexpected reply {other:?}"),
        }
        Ok(host)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed) && matches!(self.child.lock().try_wait(), Ok(None))
    }

    pub fn pid(&self) -> u32 {
        self.child.lock().id()
    }

    fn wait_reply(&self, timeout: Duration) -> anyhow::Result<Reply> {
        self.replies.lock().recv_timeout(timeout).map_err(|_| anyhow!("the OBS plugin host didn't answer"))
    }

    /// Sends a command and waits for its reply.
    pub fn request(&self, cmd: &Command_) -> anyhow::Result<Reply> {
        let line = serde_json::to_string(cmd)? + "\n";
        self.stdin.lock().write_all(line.as_bytes()).context("the OBS plugin host has stopped")?;
        match self.wait_reply(Duration::from_secs(10))? {
            Reply::Error { message } => Err(anyhow!(message)),
            r => Ok(r),
        }
    }

    pub fn list(&self) -> anyhow::Result<(Vec<TypeInfo>, Vec<TypeInfo>)> {
        match self.request(&Command_::List)? {
            Reply::Types { inputs, filters } => Ok((inputs, filters)),
            r => Err(anyhow!("unexpected reply {r:?}")),
        }
    }

    pub fn properties(&self) -> anyhow::Result<(Vec<Property>, serde_json::Value)> {
        match self.request(&Command_::Properties)? {
            Reply::Properties { properties, settings } => Ok((properties, settings)),
            r => Err(anyhow!("unexpected reply {r:?}")),
        }
    }

    pub fn update(&self, settings: &serde_json::Value) -> anyhow::Result<()> {
        self.request(&Command_::Update { settings: settings.clone() }).map(|_| ())
    }

    pub fn click(&self, property: &str) -> anyhow::Result<()> {
        self.request(&Command_::Click { property: property.into() }).map(|_| ())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.stdin.lock().write_all(b"{\"cmd\":\"quit\"}\n");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut c = self.child.lock();
        while Instant::now() < deadline {
            if let Ok(Some(_)) = c.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn media_loop(mut sock: TcpStream, latest: Arc<Mutex<Option<VideoFrame>>>, audio: Option<AudioSink>) {
    let _ = sock.set_nodelay(true);
    let mut head = [0u8; MEDIA_HEADER];
    loop {
        if sock.read_exact(&mut head).is_err() {
            return;
        }
        let Some(h) = MediaHeader::parse(&head) else { return };
        let mut buf = vec![0u8; h.len as usize];
        if sock.read_exact(&mut buf).is_err() {
            return;
        }
        match h.kind {
            KIND_VIDEO if buf.len() == (h.width * h.height * 4) as usize => {
                *latest.lock() = Some(VideoFrame::new(h.width, h.height, buf));
            }
            KIND_AUDIO => {
                if let Some(a) = &audio {
                    let s: Vec<f32> = buf.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                    a(&s);
                }
            }
            _ => {}
        }
    }
}

/// Lists installed OBS plugin source and filter types (starts a host briefly).
pub fn list_types() -> anyhow::Result<(Vec<TypeInfo>, Vec<TypeInfo>)> {
    Host::spawn(None)?.list()
}

/// An OBS plugin source running in its own host.
pub struct ObsSource {
    pub host: Arc<Host>,
}

impl ObsSource {
    pub fn start(type_id: &str, settings: &str, audio: Option<AudioSink>) -> anyhow::Result<ObsSource> {
        let host = Host::spawn(audio)?;
        let settings: serde_json::Value = if settings.trim().is_empty() { serde_json::json!({}) } else { serde_json::from_str(settings)? };
        host.request(&Command_::Create { id: type_id.into(), settings })?;
        Ok(ObsSource { host })
    }

    pub fn latest_frame(&self) -> Option<VideoFrame> {
        self.host.latest.lock().clone()
    }
}

/// Live hosts by scene item, so the properties panel can talk to the running plugin.
pub fn registry() -> &'static Mutex<std::collections::HashMap<u64, std::sync::Weak<Host>>> {
    static R: std::sync::LazyLock<Mutex<std::collections::HashMap<u64, std::sync::Weak<Host>>>> = std::sync::LazyLock::new(Default::default);
    &R
}

