//! **Ferrous Link**: send video and audio between Ferrous instances on
//! a local network, e.g. from a gaming PC to a streaming PC. It follows the
//! idea of the obs-teleport plugin, with more options:
//!
//! - automatic discovery (UDP multicast beacons), or connect by address
//! - per-link codec: parallel JPEG (lowest latency), lossless LZ4 delta
//!   (keeps alpha, ideal for overlays), or hardware H.264/HEVC via FFmpeg
//!   (lowest bandwidth)
//! - adaptive quality: JPEG quality backs off when a receiver can't keep up
//! - send the program output, any scene, or a single source
//! - per-link resolution, frame rate and audio-track choice
//! - optional pairing PIN, which also encrypts the link (ChaCha20-Poly1305)
//! - live stats on both ends: bandwidth, latency, dropped frames

pub mod codec;
pub mod discovery;
pub mod protocol;
pub mod receiver;
pub mod sender;
pub mod ts;

pub use discovery::{discovered, Announcement};
pub use receiver::{LinkReceiver, ReceiverStats};
pub use sender::{LinkSender, SenderStats};

pub const DEFAULT_PORT: u16 = 47778;
