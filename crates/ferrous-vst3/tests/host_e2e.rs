//! Hosts a real VST3 plugin (the workspace's test gain plugin, built as a
//! cdylib and wrapped in a .vst3 bundle) through the whole lifecycle.

use std::path::PathBuf;
use std::process::Command;

/// Builds the test plugin and wraps it in a bundle, once per test binary.
/// Tests run in parallel; building and copying separately raced, and one
/// test could load the library while another was still copying it ("file
/// too short"). The copy is also renamed into place, so it is never partial.
fn bundle() -> PathBuf {
    static BUNDLE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUNDLE.get_or_init(build_bundle).clone()
}

fn build_bundle() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let st = Command::new(env!("CARGO")).current_dir(&root).args(["build", "-q", "-p", "vst3-test-gain"]).status().unwrap();
    assert!(st.success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or(root.join("target"));
    let (lib, arch, ext) = if cfg!(windows) {
        ("vst3_test_gain.dll", "x86_64-win", "vst3")
    } else {
        ("libvst3_test_gain.so", if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" }, "so")
    };
    let b = std::env::temp_dir().join(format!("ferrous-vst3-e2e-{}", std::process::id())).join("FerrousTestGain.vst3");
    let dir = b.join("Contents").join(arch);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join(format!("FerrousTestGain.{ext}"));
    let tmp = dir.join("copying.tmp");
    std::fs::copy(target.join("debug").join(lib), &tmp).unwrap();
    std::fs::rename(&tmp, &dest).unwrap();
    b
}

fn tone(n: usize) -> Vec<f32> {
    (0..n).flat_map(|i| {
        let v = (i as f32 * 0.05).sin() * 0.5;
        [v, -v * 0.5]
    }).collect()
}

#[test]
fn scan_load_process_params_state() {
    let b = bundle();
    let classes = ferrous_vst3::scan(&b).unwrap();
    assert_eq!(classes.len(), 1, "{classes:?}");
    let c = &classes[0];
    assert_eq!(c.name, "Ferrous Test Gain");
    assert_eq!(c.vendor, "Ferrous");

    let mut p = ferrous_vst3::Plugin::load(&b, &c.cid, 48_000.0, 480, None).unwrap();
    let params = p.params();
    assert_eq!(params.len(), 2, "{params:?}");
    assert_eq!(params[0].title, "Gain");
    assert_eq!(params[1].steps, 1);
    assert_eq!(params[0].display, "0.0 dB");

    // Unity gain: unchanged.
    let src = tone(480);
    let mut buf = src.clone();
    p.process(&mut buf);
    assert_eq!(buf, src);

    // Half gain from the host's side reaches the processor at the next block.
    let before = p.edit_count();
    p.set_param(0, 0.5);
    assert!(p.edit_count() > before);
    let mut buf = src.clone();
    p.process(&mut buf);
    for (o, i) in buf.iter().zip(&src) {
        assert!((o - i * 0.5).abs() < 1e-6);
    }
    assert_eq!(p.params()[0].display, "-6.0 dB");

    // Invert (a stepped parameter).
    p.set_param(1, 1.0);
    let mut buf = src.clone();
    p.process(&mut buf);
    assert!((buf[10] + src[10] * 0.5).abs() < 1e-6);

    // Odd block sizes are split to the plugin's maximum.
    let mut long = tone(1000);
    let orig = long.clone();
    p.process(&mut long);
    assert!((long[1999] + orig[1999] * 0.5).abs() < 1e-6);

    // State round trip into a fresh instance.
    let state = p.save_state();
    assert!(state.starts_with("c:"), "{state}");
    drop(p);
    let mut q = ferrous_vst3::Plugin::load(&b, &c.cid, 48_000.0, 480, Some(&state)).unwrap();
    let ps = q.params();
    assert!((ps[0].value - 0.5).abs() < 1e-9 && ps[1].value == 1.0, "controller got the state: {ps:?}");
    let mut buf = src.clone();
    q.process(&mut buf);
    assert!((buf[10] + src[10] * 0.5).abs() < 1e-6, "processor got the state");
    assert!(!q.has_editor() || cfg!(windows));
}

#[test]
fn rejects_bad_input() {
    assert!(ferrous_vst3::Plugin::load(std::path::Path::new("/nonexistent.vst3"), "00000000000000000000000000000000", 48_000.0, 480, None).is_err());
    let b = bundle();
    assert!(ferrous_vst3::Plugin::load(&b, "00000000000000000000000000000000", 48_000.0, 480, None).is_err());
}
