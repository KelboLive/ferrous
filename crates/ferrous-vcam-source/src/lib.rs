//! Ferrous Virtual Camera: a Media Foundation camera source.
//!
//! Windows 11's Frame Server loads this DLL when an app opens the camera
//! Ferrous registered with `MFCreateVirtualCamera`. It creates the shared
//! frame region (see `ferrous-vcam`), and answers each sample request with the
//! newest frame Ferrous wrote there: 1920x1080 NV12 at 30 fps, or an idle
//! card while Ferrous isn't sending.
//!
//! Structure follows Microsoft's VCamSample: an activator (IMFActivate),
//! the media source (IMFMediaSourceEx, IMFGetService, IKsControl) and one
//! video stream (IMFMediaStream2, IKsControl).
//!
//! Windows-only and untested on real hardware in this build environment;
//! type-checked against the Windows target.

#[cfg(windows)]
mod source;

#[cfg(windows)]
pub use source::*;
