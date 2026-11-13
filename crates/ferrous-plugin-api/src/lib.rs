//! The Ferrous plugin ABI, version 1.
//!
//! A plugin is a DLL that exports one C function:
//!
//! ```c
//! const FerrousPluginModule* ferrous_plugin_module(void);
//! ```
//!
//! returning a static table of plugin descriptions. Everything crosses the
//! boundary as plain C types, so plugins can be written in Rust, C, C++ or
//! anything else that can export C functions. The same definitions as a C
//! header are in `docs/PLUGINS.md`.
//!
//! Rules:
//! - Pixels are 8-bit BGRA, straight (non-premultiplied) alpha, row stride in bytes.
//! - Audio is interleaved stereo f32 at 48 kHz.
//! - Settings are a UTF-8 JSON object string, owned by the host for the call only.
//! - An instance is only ever used from one thread at a time.
//! - Plugins must not unwind (panic) across the boundary.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};

pub const FERROUS_PLUGIN_ABI_VERSION: u32 = 1;
pub const FERROUS_PLUGIN_ENTRY: &[u8] = b"ferrous_plugin_module\0";

pub const FERROUS_KIND_VIDEO_SOURCE: u32 = 1;
pub const FERROUS_KIND_VIDEO_FILTER: u32 = 2;
pub const FERROUS_KIND_AUDIO_FILTER: u32 = 3;

#[repr(C)]
pub struct FerrousFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: *mut u8,
}

pub type CreateFn = unsafe extern "C" fn(settings_json: *const c_char) -> *mut c_void;
pub type DestroyFn = unsafe extern "C" fn(instance: *mut c_void);
/// Video sources: the size of the next frame.
pub type VideoSizeFn = unsafe extern "C" fn(instance: *mut c_void, width: *mut u32, height: *mut u32);
/// Video sources fill `frame`; video filters modify it in place.
/// `time_ns` is nanoseconds since the instance was created.
pub type VideoRenderFn = unsafe extern "C" fn(instance: *mut c_void, frame: *mut FerrousFrame, time_ns: u64);
/// Audio filters process `frames` stereo frames in place.
pub type AudioProcessFn = unsafe extern "C" fn(instance: *mut c_void, samples: *mut f32, frames: u32);

#[repr(C)]
pub struct FerrousPluginDesc {
    /// Unique, stable id such as "acme.blur".
    pub id: *const c_char,
    /// Display name.
    pub name: *const c_char,
    pub kind: u32,
    pub create: CreateFn,
    pub destroy: DestroyFn,
    pub video_size: Option<VideoSizeFn>,
    pub video_render: Option<VideoRenderFn>,
    pub audio_process: Option<AudioProcessFn>,
}

// The table lives in static memory inside the plugin.
unsafe impl Sync for FerrousPluginDesc {}

#[repr(C)]
pub struct FerrousPluginModule {
    pub abi_version: u32,
    pub count: u32,
    pub plugins: *const FerrousPluginDesc,
}

unsafe impl Sync for FerrousPluginModule {}

pub type ModuleFn = unsafe extern "C" fn() -> *const FerrousPluginModule;
