//! Example plugin showing all three plugin kinds. Copy this to start your own.

use std::ffi::{c_char, c_void, CStr};

use ferrous_plugin_api::*;

fn settings(json: *const c_char) -> serde_json::Value {
    if json.is_null() {
        return serde_json::Value::Null;
    }
    let s = unsafe { CStr::from_ptr(json) }.to_string_lossy();
    serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
}

// --- Gradient source -------------------------------------------------------

struct Gradient {
    width: u32,
    height: u32,
    speed: f64,
}

unsafe extern "C" fn gradient_create(json: *const c_char) -> *mut c_void {
    let s = settings(json);
    let g = Gradient {
        width: s["width"].as_u64().unwrap_or(640) as u32,
        height: s["height"].as_u64().unwrap_or(360) as u32,
        speed: s["speed"].as_f64().unwrap_or(1.0),
    };
    Box::into_raw(Box::new(g)) as *mut c_void
}

unsafe extern "C" fn gradient_destroy(p: *mut c_void) {
    drop(Box::from_raw(p as *mut Gradient));
}

unsafe extern "C" fn gradient_size(p: *mut c_void, w: *mut u32, h: *mut u32) {
    let g = &*(p as *const Gradient);
    *w = g.width;
    *h = g.height;
}

unsafe extern "C" fn gradient_render(p: *mut c_void, f: *mut FerrousFrame, time_ns: u64) {
    let g = &*(p as *const Gradient);
    let f = &mut *f;
    let shift = ((time_ns as f64 / 1e9) * g.speed * 64.0) as u32;
    for y in 0..f.height {
        let row = std::slice::from_raw_parts_mut(f.data.add((y * f.stride) as usize), (f.width * 4) as usize);
        for x in 0..f.width {
            let i = (x * 4) as usize;
            row[i] = (y * 255 / f.height.max(1)) as u8; // B
            row[i + 1] = 64; // G
            row[i + 2] = ((x * 255 / f.width.max(1) + shift) % 256) as u8; // R
            row[i + 3] = 255;
        }
    }
}

// --- Invert filter ---------------------------------------------------------

unsafe extern "C" fn invert_create(_: *const c_char) -> *mut c_void {
    Box::into_raw(Box::new(0u8)) as *mut c_void
}

unsafe extern "C" fn invert_destroy(p: *mut c_void) {
    drop(Box::from_raw(p as *mut u8));
}

unsafe extern "C" fn invert_render(_: *mut c_void, f: *mut FerrousFrame, _: u64) {
    let f = &mut *f;
    for y in 0..f.height {
        let row = std::slice::from_raw_parts_mut(f.data.add((y * f.stride) as usize), (f.width * 4) as usize);
        for px in row.chunks_exact_mut(4) {
            px[0] = 255 - px[0];
            px[1] = 255 - px[1];
            px[2] = 255 - px[2];
        }
    }
}

// --- Gain audio filter -----------------------------------------------------

unsafe extern "C" fn gain_create(json: *const c_char) -> *mut c_void {
    let db = settings(json)["db"].as_f64().unwrap_or(-6.0) as f32;
    Box::into_raw(Box::new(10f32.powf(db / 20.0))) as *mut c_void
}

unsafe extern "C" fn gain_destroy(p: *mut c_void) {
    drop(Box::from_raw(p as *mut f32));
}

unsafe extern "C" fn gain_process(p: *mut c_void, samples: *mut f32, frames: u32) {
    let g = *(p as *const f32);
    for s in std::slice::from_raw_parts_mut(samples, frames as usize * 2) {
        *s *= g;
    }
}

static PLUGINS: [FerrousPluginDesc; 3] = [
    FerrousPluginDesc {
        id: c"example.gradient".as_ptr(),
        name: c"Example: Gradient".as_ptr(),
        kind: FERROUS_KIND_VIDEO_SOURCE,
        create: gradient_create,
        destroy: gradient_destroy,
        video_size: Some(gradient_size),
        video_render: Some(gradient_render),
        audio_process: None,
    },
    FerrousPluginDesc {
        id: c"example.invert".as_ptr(),
        name: c"Example: Invert Colors".as_ptr(),
        kind: FERROUS_KIND_VIDEO_FILTER,
        create: invert_create,
        destroy: invert_destroy,
        video_size: None,
        video_render: Some(invert_render),
        audio_process: None,
    },
    FerrousPluginDesc {
        id: c"example.gain".as_ptr(),
        name: c"Example: Gain".as_ptr(),
        kind: FERROUS_KIND_AUDIO_FILTER,
        create: gain_create,
        destroy: gain_destroy,
        video_size: None,
        video_render: None,
        audio_process: Some(gain_process),
    },
];

static MODULE: FerrousPluginModule =
    FerrousPluginModule { abi_version: FERROUS_PLUGIN_ABI_VERSION, count: 3, plugins: PLUGINS.as_ptr() };

#[no_mangle]
pub extern "C" fn ferrous_plugin_module() -> *const FerrousPluginModule {
    &MODULE
}
