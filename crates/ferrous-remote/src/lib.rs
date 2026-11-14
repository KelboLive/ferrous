//! A remote-control server speaking the obs-websocket 5.x protocol
//! (RPC version 1), so Stream Deck, Touch Portal, TikFinity, Streamer.bot
//! and similar tools can drive Ferrous.
//!
//! The server only handles the protocol. Requests are forwarded as
//! [`RemoteCall`]s to whoever owns the app state (the UI thread), which
//! replies with data or an error status.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::http::HeaderValue;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::{Message, WebSocket};

pub const DEFAULT_PORT: u16 = 4455;
pub const RPC_VERSION: u64 = 1;

/// obs-websocket request status codes used here.
pub mod status {
    pub const SUCCESS: u16 = 100;
    pub const MISSING_REQUEST_TYPE: u16 = 203;
    pub const UNKNOWN_REQUEST_TYPE: u16 = 204;
    pub const GENERIC_ERROR: u16 = 205;
    pub const MISSING_REQUEST_FIELD: u16 = 300;
    pub const INVALID_REQUEST_FIELD: u16 = 400;
    pub const OUTPUT_RUNNING: u16 = 500;
    pub const OUTPUT_NOT_RUNNING: u16 = 501;
    pub const STUDIO_MODE_NOT_ACTIVE: u16 = 506;
    pub const RESOURCE_NOT_FOUND: u16 = 600;
}

/// Event subscription categories (obs-websocket `EventSubscription`).
pub mod intent {
    pub const GENERAL: u64 = 1 << 0;
    pub const SCENES: u64 = 1 << 2;
    pub const INPUTS: u64 = 1 << 3;
    pub const TRANSITIONS: u64 = 1 << 4;
    pub const OUTPUTS: u64 = 1 << 6;
    pub const SCENE_ITEMS: u64 = 1 << 7;
    pub const UI: u64 = 1 << 10;
    pub const ALL: u64 = (1 << 11) - 1;
}

pub type Reply = Result<Value, (u16, String)>;

pub struct RemoteCall {
    pub request_type: String,
    pub data: Value,
    pub reply: Sender<Reply>,
}

/// `base64(sha256(base64(sha256(password + salt)) + challenge))`.
pub fn auth_response(password: &str, salt: &str, challenge: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let secret = b64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    b64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

/// Equal-length comparison that does not stop at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_token() -> String {
    // Not security-critical randomness beyond replay protection; mix time and address entropy.
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let x = Box::new(0u8);
    let addr = &*x as *const u8 as usize;
    let digest = Sha256::digest(format!("{t}-{addr}-{}", std::process::id()).as_bytes());
    base64::engine::general_purpose::STANDARD.encode(&digest[..24])
}

struct Client {
    tx: Sender<(u64, String)>,
}

pub struct RemoteServer {
    port: u16,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<Client>>>,
    connected: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl RemoteServer {
    /// Listens on `0.0.0.0:port` (port 0 picks a free one). `wake` is called
    /// whenever a request is queued, so a sleeping UI can process it.
    pub fn start(
        port: u16,
        password: Option<String>,
        calls: Sender<RemoteCall>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let clients: Arc<Mutex<Vec<Client>>> = Arc::default();
        let connected = Arc::new(AtomicUsize::new(0));
        let (s2, c2, n2) = (stop.clone(), clients.clone(), connected.clone());
        let password = password.filter(|p| !p.is_empty());
        let thread = thread::Builder::new().name("remote-accept".into()).spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        log::info!("remote client connected from {addr}");
                        // Bounded per client: a stalled client drops updates instead of growing memory.
                        let (tx, rx) = crossbeam_channel::bounded(256);
                        c2.lock().push(Client { tx });
                        let (calls, wake, password, stop, n) = (calls.clone(), wake.clone(), password.clone(), s2.clone(), n2.clone());
                        thread::spawn(move || {
                            n.fetch_add(1, Ordering::Relaxed);
                            if let Err(e) = serve(stream, password.as_deref(), &calls, &wake, &rx, &stop) {
                                log::info!("remote client: {e}");
                            }
                            n.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
                    Err(e) => {
                        log::warn!("remote accept: {e}");
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
        Ok(Self { port, stop, clients, connected, thread: Some(thread) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn client_count(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }

    /// Broadcasts an event (op 5) to subscribed clients.
    pub fn emit(&self, event_type: &str, event_intent: u64, data: Value) {
        let msg = json!({ "op": 5, "d": { "eventType": event_type, "eventIntent": event_intent, "eventData": data } }).to_string();
        self.clients.lock().retain(|c| !matches!(c.tx.try_send((event_intent, msg.clone())), Err(crossbeam_channel::TrySendError::Disconnected(_))));
    }
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn send(ws: &mut WebSocket<TcpStream>, v: Value) -> anyhow::Result<()> {
    ws.send(Message::text(v.to_string()))?;
    Ok(())
}

fn close(ws: &mut WebSocket<TcpStream>, code: u16, reason: &str) {
    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.to_string().into() }));
    let _ = ws.flush();
}

fn call(calls: &Sender<RemoteCall>, wake: &Arc<dyn Fn() + Send + Sync>, request_type: &str, data: Value) -> Reply {
    let (tx, rx) = crossbeam_channel::bounded(1);
    if calls.send(RemoteCall { request_type: request_type.to_string(), data, reply: tx }).is_err() {
        return Err((status::GENERIC_ERROR, "app is shutting down".into()));
    }
    wake();
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| Err((status::GENERIC_ERROR, "request timed out".into())))
}

fn response(request_type: &str, request_id: &Value, reply: Reply) -> Value {
    let mut d = json!({ "requestType": request_type, "requestId": request_id });
    match reply {
        Ok(data) => {
            d["requestStatus"] = json!({ "result": true, "code": status::SUCCESS });
            if !data.is_null() {
                d["responseData"] = data;
            }
        }
        Err((code, comment)) => {
            d["requestStatus"] = json!({ "result": false, "code": code, "comment": comment });
        }
    }
    d
}

fn serve(
    stream: TcpStream,
    password: Option<&str>,
    calls: &Sender<RemoteCall>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    events: &Receiver<(u64, String)>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    // A peer that connects and says nothing must not hold a thread forever.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    // obs-websocket clients negotiate the `obswebsocket.json` subprotocol.
    // (The callback's error type is fixed by tungstenite.)
    #[allow(clippy::result_large_err)]
    let mut ws = tungstenite::accept_hdr(stream, |req: &Request, mut resp: Response| {
        if let Some(p) = req.headers().get("Sec-WebSocket-Protocol").and_then(|v| v.to_str().ok()) {
            if p.split(',').any(|s| s.trim() == "obswebsocket.json") {
                resp.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("obswebsocket.json"));
            }
        }
        Ok(resp)
    })
    .map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(50)))?;

    let (challenge, salt) = (random_token(), random_token());
    let mut hello = json!({ "obsWebSocketVersion": "5.5.0", "rpcVersion": RPC_VERSION });
    if password.is_some() {
        hello["authentication"] = json!({ "challenge": challenge, "salt": salt });
    }
    send(&mut ws, json!({ "op": 0, "d": hello }))?;

    let started = std::time::Instant::now();
    let mut identified = false;
    let mut subscriptions = intent::ALL;
    while !stop.load(Ordering::Relaxed) {
        // Events queue per connection from the moment it connects; an unidentified client must not let them pile up.
        while let Ok((intent, msg)) = events.try_recv() {
            if identified && subscriptions & intent != 0 {
                ws.send(Message::text(msg))?;
            }
        }
        if !identified && started.elapsed() > Duration::from_secs(10) {
            close(&mut ws, 4007, "not identified in time");
            return Ok(());
        }
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            close(&mut ws, 4002, "message is not valid JSON");
            return Ok(());
        };
        let op = v["op"].as_u64().unwrap_or(u64::MAX);
        let d = &v["d"];
        match op {
            // Reidentify only updates an identified session. Accepting it first would skip the password.
            3 if !identified => {
                close(&mut ws, 4007, "not identified");
                return Ok(());
            }
            1 | 3 => {
                // Identify / Reidentify
                if op == 1 {
                    if d["rpcVersion"].as_u64().unwrap_or(0) != RPC_VERSION {
                        close(&mut ws, 4010, "unsupported RPC version");
                        return Ok(());
                    }
                    if let Some(pw) = password {
                        let expected = auth_response(pw, &salt, &challenge);
                        if !constant_time_eq(d["authentication"].as_str().unwrap_or("").as_bytes(), expected.as_bytes()) {
                            close(&mut ws, 4009, "authentication failed");
                            return Ok(());
                        }
                    }
                }
                if let Some(s) = d["eventSubscriptions"].as_u64() {
                    subscriptions = s;
                }
                identified = true;
                send(&mut ws, json!({ "op": 2, "d": { "negotiatedRpcVersion": RPC_VERSION } }))?;
            }
            _ if !identified => {
                close(&mut ws, 4007, "not identified");
                return Ok(());
            }
            6 => {
                let Some(rt) = d["requestType"].as_str() else {
                    let r = response("", &d["requestId"], Err((status::MISSING_REQUEST_TYPE, "missing requestType".into())));
                    send(&mut ws, json!({ "op": 7, "d": r }))?;
                    continue;
                };
                let reply = call(calls, wake, rt, d.get("requestData").cloned().unwrap_or(json!({})));
                send(&mut ws, json!({ "op": 7, "d": response(rt, &d["requestId"], reply) }))?;
            }
            8 => {
                // RequestBatch, executed serially.
                let halt = d["haltOnFailure"].as_bool().unwrap_or(false);
                let mut results = Vec::new();
                for r in d["requests"].as_array().cloned().unwrap_or_default() {
                    let rt = r["requestType"].as_str().unwrap_or_default().to_string();
                    let reply = if rt == "Sleep" {
                        thread::sleep(Duration::from_millis(r["requestData"]["sleepMillis"].as_u64().unwrap_or(0).min(50_000)));
                        Ok(Value::Null)
                    } else {
                        call(calls, wake, &rt, r.get("requestData").cloned().unwrap_or(json!({})))
                    };
                    let failed = reply.is_err();
                    results.push(response(&rt, &r["requestId"], reply));
                    if failed && halt {
                        break;
                    }
                }
                send(&mut ws, json!({ "op": 9, "d": { "requestId": d["requestId"], "results": results } }))?;
            }
            _ => {
                close(&mut ws, 4004, "unknown op code");
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tungstenite::client::IntoClientRequest;

    fn responder() -> (Sender<RemoteCall>, std::thread::JoinHandle<()>) {
        let (tx, rx) = crossbeam_channel::unbounded::<RemoteCall>();
        let h = std::thread::spawn(move || {
            for c in rx {
                let reply = match c.request_type.as_str() {
                    "GetCurrentProgramScene" => Ok(json!({ "sceneName": "Main", "currentProgramSceneName": "Main" })),
                    "SetCurrentProgramScene" => match c.data["sceneName"].as_str() {
                        Some(_) => Ok(Value::Null),
                        None => Err((status::MISSING_REQUEST_FIELD, "sceneName".into())),
                    },
                    _ => Err((status::UNKNOWN_REQUEST_TYPE, "unknown".into())),
                };
                let _ = c.reply.send(reply);
            }
        });
        (tx, h)
    }

    fn connect(port: u16) -> WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>> {
        let mut req = format!("ws://127.0.0.1:{port}").into_client_request().unwrap();
        req.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("obswebsocket.json"));
        let (ws, resp) = tungstenite::connect(req).unwrap();
        assert_eq!(resp.headers().get("Sec-WebSocket-Protocol").unwrap(), "obswebsocket.json");
        ws
    }

    fn read_json(ws: &mut WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>) -> Value {
        loop {
            if let Message::Text(t) = ws.read().unwrap() {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    #[test]
    fn auth_matches_reference_algorithm() {
        // Independent computation of the documented algorithm.
        let b64 = base64::engine::general_purpose::STANDARD;
        let secret = b64.encode(Sha256::digest(b"supersecretsalt"));
        let expected = b64.encode(Sha256::digest(format!("{secret}challenge").as_bytes()));
        assert_eq!(auth_response("supersecret", "salt", "challenge"), expected);
    }

    #[test]
    fn handshake_auth_requests_batches_and_events() {
        let (calls, _h) = responder();
        let server = RemoteServer::start(0, Some("hunter2".into()), calls, Arc::new(|| {})).unwrap();
        let mut ws = connect(server.port());
        let hello = read_json(&mut ws);
        assert_eq!(hello["op"], 0);
        let auth = &hello["d"]["authentication"];
        let token = auth_response("hunter2", auth["salt"].as_str().unwrap(), auth["challenge"].as_str().unwrap());
        ws.send(Message::text(json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": token } }).to_string())).unwrap();
        assert_eq!(read_json(&mut ws)["op"], 2);

        ws.send(Message::text(json!({ "op": 6, "d": { "requestType": "GetCurrentProgramScene", "requestId": "a" } }).to_string())).unwrap();
        let r = read_json(&mut ws);
        assert_eq!(r["op"], 7);
        assert_eq!(r["d"]["requestId"], "a");
        assert_eq!(r["d"]["requestStatus"]["code"], 100);
        assert_eq!(r["d"]["responseData"]["sceneName"], "Main");

        ws.send(Message::text(json!({ "op": 6, "d": { "requestType": "Nope", "requestId": "b" } }).to_string())).unwrap();
        assert_eq!(read_json(&mut ws)["d"]["requestStatus"]["code"], 204);

        ws.send(Message::text(json!({ "op": 8, "d": { "requestId": "batch", "requests": [
            { "requestType": "SetCurrentProgramScene", "requestData": { "sceneName": "BRB" } },
            { "requestType": "Sleep", "requestData": { "sleepMillis": 10 } },
            { "requestType": "SetCurrentProgramScene", "requestData": {} }
        ] } }).to_string())).unwrap();
        let b = read_json(&mut ws);
        assert_eq!(b["op"], 9);
        let res = b["d"]["results"].as_array().unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[2]["requestStatus"]["code"], 300);

        // Events reach identified clients.
        std::thread::sleep(Duration::from_millis(100));
        server.emit("CurrentProgramSceneChanged", intent::SCENES, json!({ "sceneName": "BRB" }));
        let e = read_json(&mut ws);
        assert_eq!(e["op"], 5);
        assert_eq!(e["d"]["eventType"], "CurrentProgramSceneChanged");
    }

    #[test]
    fn wrong_password_is_rejected() {
        let (calls, _h) = responder();
        let server = RemoteServer::start(0, Some("right".into()), calls, Arc::new(|| {})).unwrap();
        let mut ws = connect(server.port());
        let _hello = read_json(&mut ws);
        ws.send(Message::text(json!({ "op": 1, "d": { "rpcVersion": 1, "authentication": "wrong" } }).to_string())).unwrap();
        loop {
            match ws.read() {
                Ok(Message::Close(Some(f))) => {
                    assert_eq!(u16::from(f.code), 4009);
                    break;
                }
                Ok(_) => continue,
                Err(e) => panic!("expected close frame, got {e}"),
            }
        }
    }

    /// Reidentify (op 3) must not be a way around the password: before the
    /// first successful Identify the server has to refuse it.
    #[test]
    fn reidentify_cannot_skip_authentication() {
        let (calls, _h) = responder();
        let server = RemoteServer::start(0, Some("right".into()), calls, Arc::new(|| {})).unwrap();
        let mut ws = connect(server.port());
        let _hello = read_json(&mut ws);
        ws.send(Message::text(json!({ "op": 3, "d": {} }).to_string())).unwrap();
        ws.send(Message::text(
            json!({ "op": 6, "d": { "requestType": "GetCurrentProgramScene", "requestId": "x" } }).to_string(),
        ))
        .unwrap();
        loop {
            match ws.read() {
                Ok(Message::Close(_)) | Err(_) => break, // refused: good
                Ok(Message::Text(t)) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    assert_ne!(v["op"], 7, "an unauthenticated client got a request response: {v}");
                    assert_ne!(v["op"], 2, "Reidentify identified an unauthenticated client");
                }
                Ok(_) => continue,
            }
        }
    }
}
