//! OBS plugin sources: the plugin runs on libobs in `ferrous-obs-host`.
//! Starting a host takes a few seconds (libobs loads every plugin), so it
//! starts in the background; the source shows nothing until it's ready.

use std::sync::Arc;

use ferrous_core::{SourceConfig, VideoFrame};
use ferrous_obs::{Host, ObsSource};
use parking_lot::Mutex;

use crate::{AudioSink, Source};

enum State {
    Starting,
    Running(ObsSource),
    Failed,
}

pub struct ObsPluginSource {
    state: Arc<Mutex<State>>,
    item_id: u64,
}

impl ObsPluginSource {
    pub fn start(type_id: &str, settings: &str, audio: Option<AudioSink>, item_id: u64) -> anyhow::Result<Self> {
        if ferrous_obs::obs_dir().is_none() {
            anyhow::bail!("OBS Studio isn't installed (OBS plugin sources run on its libobs)");
        }
        if !ferrous_obs::host_path().exists() {
            anyhow::bail!("ferrous-obs-host is missing next to Ferrous");
        }
        let state = Arc::new(Mutex::new(State::Starting));
        let (st, type_id, settings) = (state.clone(), type_id.to_string(), settings.to_string());
        std::thread::Builder::new()
            .name("obs-plugin-start".into())
            .spawn(move || {
                let sink: Option<ferrous_obs::AudioSink> = audio.map(|a| {
                    let f: ferrous_obs::AudioSink = Arc::new(move |s: &[f32]| a(s));
                    f
                });
                match ObsSource::start(&type_id, &settings, sink) {
                    Ok(s) => {
                        if item_id != 0 {
                            ferrous_obs::registry().lock().insert(item_id, Arc::downgrade(&s.host));
                        }
                        *st.lock() = State::Running(s);
                    }
                    Err(e) => {
                        log::warn!("OBS plugin source {type_id}: {e:#}");
                        *st.lock() = State::Failed;
                    }
                }
            })
            .ok();
        Ok(Self { state, item_id })
    }

    /// The running host, for the properties panel.
    pub fn host(&self) -> Option<Arc<Host>> {
        match &*self.state.lock() {
            State::Running(s) => Some(s.host.clone()),
            _ => None,
        }
    }
}

impl Drop for ObsPluginSource {
    fn drop(&mut self) {
        if self.item_id != 0 {
            ferrous_obs::registry().lock().remove(&self.item_id);
        }
    }
}

impl Source for ObsPluginSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        match &*self.state.lock() {
            State::Running(s) => s.latest_frame(),
            _ => None,
        }
    }

    fn is_alive(&self) -> bool {
        match &*self.state.lock() {
            State::Starting => true,
            State::Running(s) => s.host.is_alive(),
            State::Failed => false,
        }
    }

    fn pids(&self) -> Vec<u32> {
        self.host().map(|h| vec![h.pid()]).unwrap_or_default()
    }

    fn update(&mut self, config: &SourceConfig) -> bool {
        let SourceConfig::ObsPlugin { settings, .. } = config else { return false };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(if settings.trim().is_empty() { "{}" } else { settings }) else { return true };
        match &*self.state.lock() {
            State::Running(s) => {
                let h = s.host.clone();
                // Off the render thread: the host answers within milliseconds, but never block a frame on it.
                std::thread::spawn(move || {
                    if let Err(e) = h.update(&v) {
                        log::warn!("OBS plugin update: {e:#}");
                    }
                });
                true
            }
            // Still starting: it'll start with the old settings; restart to apply the new ones.
            _ => false,
        }
    }
}
