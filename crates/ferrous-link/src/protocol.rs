//! Wire format: `[u32 big-endian length][body]`. The body is
//! `[u8 type][payload]`, AEAD-sealed when the link has a PIN.

use std::io::{Read, Write};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 2: lossless frames are LZ4-compressed in parallel chunks; REPEAT messages.
/// 3: whole-frame libjpeg-turbo JPEG; timestamps in microseconds; the
///    hardware codec carries NV12 end to end.
pub const PROTO_VERSION: u32 = 3;
/// Upper bound on one message (a lossless 4K keyframe is ~33 MB raw).
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

pub mod kind {
    pub const AUTH: u8 = 1;
    pub const VIDEO: u8 = 2;
    pub const STREAM: u8 = 3;
    pub const AUDIO: u8 = 4;
    pub const PING: u8 = 5;
    pub const PONG: u8 = 6;
    /// The picture hasn't changed since the last frame: `[u64 timestamp_ms]`.
    pub const REPEAT: u8 = 7;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodecKind {
    Jpeg,
    Lossless,
    /// FFmpeg-encoded byte stream (H.264/HEVC in MPEG-TS).
    Stream,
}

/// First message, sent in the clear by the sender.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: CodecKind,
    pub audio: bool,
    pub encrypted: bool,
    /// Hex salt for key derivation.
    pub salt: String,
}

pub fn derive_key(pin: &str, salt: &str) -> [u8; 32] {
    Sha256::digest(format!("ferrous-link-v1|{pin}|{salt}").as_bytes()).into()
}

/// Per-direction AEAD with a counter nonce. `None` = plaintext link.
pub struct Cipher {
    aead: Option<ChaCha20Poly1305>,
    direction: u8,
    counter: u64,
}

impl Cipher {
    pub fn new(key: Option<[u8; 32]>, direction: u8) -> Self {
        Self { aead: key.map(|k| ChaCha20Poly1305::new(Key::from_slice(&k))), direction, counter: 0 }
    }

    fn nonce(&mut self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0] = self.direction;
        n[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        n
    }

    pub fn seal(&mut self, body: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        let n = self.nonce();
        match &self.aead {
            Some(a) => a.encrypt(Nonce::from_slice(&n), body.as_slice()).map_err(|_| anyhow::anyhow!("encrypt failed")),
            None => Ok(body),
        }
    }

    pub fn open(&mut self, body: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        let n = self.nonce();
        match &self.aead {
            Some(a) => a.decrypt(Nonce::from_slice(&n), body.as_slice()).map_err(|_| anyhow::anyhow!("wrong PIN or corrupted data")),
            None => Ok(body),
        }
    }
}

/// Big socket buffers: a 1080p JPEG frame is several hundred KB, and the
/// OS defaults (64 KB on some Windows setups) stall the sender mid-frame.
pub fn tune_socket(s: &std::net::TcpStream) {
    let sock = socket2::SockRef::from(s);
    let _ = sock.set_send_buffer_size(4 * 1024 * 1024);
    let _ = sock.set_recv_buffer_size(4 * 1024 * 1024);
    let _ = s.set_nodelay(true);
}

/// Writes length-prefixed messages, sealing them when the link has a PIN.
/// The length and body go out in one write (with no-delay on, two writes
/// sent a separate 4-byte packet per message), and plaintext bodies are
/// written straight from the shared buffer, without a copy.
pub struct MessageWriter<W: Write> {
    inner: std::io::BufWriter<W>,
}

impl<W: Write> MessageWriter<W> {
    pub fn new(w: W) -> Self {
        Self { inner: std::io::BufWriter::with_capacity(256 * 1024, w) }
    }

    pub fn send(&mut self, cipher: &mut Cipher, body: &[u8]) -> anyhow::Result<()> {
        if cipher.aead.is_some() {
            let sealed = cipher.seal(body.to_vec())?;
            write_raw(&mut self.inner, &sealed)?;
        } else {
            cipher.counter += 1; // keep nonce counters in step either way
            write_raw(&mut self.inner, body)?;
        }
        self.inner.flush()?;
        Ok(())
    }

    /// The underlying writer (anything unflushed is discarded).
    pub fn into_inner(self) -> W {
        self.inner.into_parts().0
    }
}

pub fn write_raw(w: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(body)
}

pub fn read_raw(r: &mut impl Read) -> anyhow::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE {
        anyhow::bail!("message too large ({len} bytes)");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 1);
    v.push(kind);
    v.extend_from_slice(payload);
    v
}

/// Wall-clock microseconds (frame timestamps, latency and pacing).
pub fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_wrong_pin() {
        let salt = random_hex(16);
        let mut tx = Cipher::new(Some(derive_key("1234", &salt)), 1);
        let mut rx = Cipher::new(Some(derive_key("1234", &salt)), 1);
        let mut bad = Cipher::new(Some(derive_key("9999", &salt)), 1);
        let sealed = tx.seal(frame(kind::PING, b"hello")).unwrap();
        assert_ne!(&sealed[1..6], b"hello", "ciphertext isn't plaintext");
        assert!(bad.open(sealed.clone()).is_err());
        assert_eq!(rx.open(sealed).unwrap(), frame(kind::PING, b"hello"));
        // Counters advance in lockstep.
        let s2 = tx.seal(vec![9]).unwrap();
        assert_eq!(rx.open(s2).unwrap(), vec![9]);
    }
}
