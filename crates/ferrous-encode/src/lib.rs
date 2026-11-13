//! Encoders: what exists, what works on this machine, and how to configure it.
//!
//! Encoding is delegated to FFmpeg, which gives access to every encoder
//! OBS supports (x264, x265, SVT-AV1, AOM, NVENC, Quick Sync, AMF, Media
//! Foundation) through one stable interface.

pub mod args;
pub mod autoconfig;
pub mod catalog;
pub mod ffmpeg;
pub mod presets;
pub mod probe;

pub use catalog::{Codec, EncoderInfo, Vendor, ENCODERS};
pub use probe::{probe, ProbeResult};
