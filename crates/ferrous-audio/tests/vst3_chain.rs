//! A VST3 effect in a filter chain: loads from config, processes, keeps
//! running (not reloaded) when only its saved state changes.

use std::path::PathBuf;
use std::process::Command;

use ferrous_audio::filters::FilterChain;
use ferrous_core::filters::{AudioFilter, Filter};

fn bundle() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(Command::new(env!("CARGO")).current_dir(&root).args(["build", "-q", "-p", "vst3-test-gain"]).status().unwrap().success());
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or(root.join("target"));
    let b = std::env::temp_dir().join("ferrous-vst3-chain").join("FerrousTestGain.vst3");
    let (lib, arch, ext) = if cfg!(windows) { ("vst3_test_gain.dll", "x86_64-win", "vst3") } else { ("libvst3_test_gain.so", "x86_64-linux", "so") };
    let dir = b.join("Contents").join(arch);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target.join("debug").join(lib), dir.join(format!("FerrousTestGain.{ext}"))).unwrap();
    b
}

#[test]
fn vst3_filter_in_chain() {
    let b = bundle();
    let class = ferrous_vst3::scan(&b).unwrap().remove(0);
    let f = AudioFilter::Vst3 { path: b.display().to_string(), cid: class.cid.clone(), name: class.name.clone(), state: String::new() };
    let mut config = vec![Filter { enabled: true, filter: AudioFilter::Gain { db: 0.0 } }, Filter { enabled: true, filter: f }];
    let mut chain = FilterChain::default();
    chain.set(&config);
    let p = chain.vst3_mut(1).expect("loaded");
    p.set_param(0, 0.25);
    let state = p.save_state();
    let addr = p as *const _ as usize;

    let mut buf = vec![0.8f32; 960];
    chain.process(&mut buf);
    assert!((buf[0] - 0.2).abs() < 1e-6, "{}", buf[0]);

    // Persist the state into the config: the instance is kept, not reloaded.
    if let AudioFilter::Vst3 { state: s, .. } = &mut config[1].filter {
        *s = state;
    }
    chain.set(&config);
    assert_eq!(chain.vst3_mut(1).map(|p| p as *const _ as usize), Some(addr));

    // A new chain built from the saved config restores the plugin's state.
    let mut fresh = FilterChain::default();
    fresh.set(&config);
    let mut buf = vec![0.8f32; 960];
    fresh.process(&mut buf);
    assert!((buf[0] - 0.2).abs() < 1e-6, "restored: {}", buf[0]);

    // Disabling the gain filter in front doesn't reload the plugin either.
    config[0].enabled = false;
    chain.set(&config);
    assert_eq!(chain.vst3_mut(1).map(|p| p as *const _ as usize), Some(addr));
}
