// egui 0.37 deprecates Id::new in favour of IdSalt; both work the same here.
#![allow(deprecated)]
// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod add_source;
mod app;
mod crash;
mod docks;
mod filters_ui;
mod brand;
mod help;
mod history;
mod presets_ui;
mod hotkeys;
mod layout;
mod mixer;
mod obs_ui;
mod palette;
mod preflight;
mod remote;
mod settings_ui;
mod setup;
mod fx;
mod theme;
mod vst3_ui;
mod webdock;
mod widgets;
mod wizard;

fn main() -> eframe::Result {
    // Helper mode: list one VST3 bundle's effects as JSON and exit. The app
    // scans plugins this way so a plugin that crashes on load can't take
    // Ferrous down with it.
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--scan-vst3" {
        match ferrous_vst3::scan(std::path::Path::new(&args[2])) {
            Ok(classes) => println!("{}", serde_json::to_string(&classes).unwrap_or_default()),
            Err(e) => {
                eprintln!("{e:#}");
                std::process::exit(2);
            }
        }
        std::process::exit(0);
    }
    {
        let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
        // Release builds on Windows have no console, so stderr goes nowhere:
        // log to a file next to the settings instead (replaced each launch).
        // The previous session's log is kept as ferrous.previous.log.
        if cfg!(all(windows, not(debug_assertions))) {
            if let Ok(file) = std::fs::File::create(crash::rotate_log()) {
                logger.target(env_logger::Target::Pipe(Box::new(file)));
            }
        }
        logger.init();
    }
    crash::install_panic_hook();
    #[cfg(windows)]
    crash::install_native_crash_log();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Ferrous")
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([960.0, 600.0])
            .with_app_id("ferrous")
            .with_icon(brand::window_icon()),
        // Remember window size/position, floating dock positions and sizes,
        // and side/bottom panel sizes between sessions.
        persist_window: true,
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    eframe::run_native(
        "Ferrous",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app::FerrousApp::new(cc)))
        }),
    )
}

/// The window draws with Direct3D 12 on Windows, like the compositor, instead of Vulkan (eframe's
/// first choice): one graphics API fewer sharing the GPU with the game, as OBS does. Vulkan stays as
/// the fallback, and `WGPU_BACKEND` still overrides.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup};
    use eframe::wgpu;
    let mut config = WgpuConfiguration::default();
    if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
        if let WgpuSetup::CreateNew(create) = &mut config.wgpu_setup {
            create.native_adapter_selector = Some(std::sync::Arc::new(|adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
                let usable = |a: &&wgpu::Adapter| surface.is_none_or(|s| a.is_surface_supported(s));
                let rank = |a: &&wgpu::Adapter| {
                    let info = a.get_info();
                    let gpu = match info.device_type {
                        wgpu::DeviceType::DiscreteGpu => 0,
                        wgpu::DeviceType::IntegratedGpu => 1,
                        wgpu::DeviceType::VirtualGpu | wgpu::DeviceType::Other => 2,
                        wgpu::DeviceType::Cpu => 3,
                    };
                    let api = match info.backend {
                        wgpu::Backend::Dx12 => 0,
                        wgpu::Backend::Vulkan => 1,
                        _ => 2,
                    };
                    (gpu, api)
                };
                let pick = adapters.iter().filter(usable).min_by_key(rank).cloned().ok_or_else(|| "no graphics adapter can draw to this window".to_string())?;
                log::info!("window drawn with {:?} on {}", pick.get_info().backend, pick.get_info().name);
                Ok(pick)
            }));
        }
    }
    config
}
