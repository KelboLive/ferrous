//! Audio filter DSP. Every filter works in place on interleaved 48 kHz
//! stereo f32 and keeps its own state between chunks.

use ferrous_core::filters::{AudioFilter, AudioFilters};
use nnnoiseless::DenoiseState;

use crate::SAMPLE_RATE;

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-9).log10()
}

/// One-pole smoothing coefficient for a time constant in ms.
fn coef(ms: f32) -> f32 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms / 1000.0 * SAMPLE_RATE as f32)).exp()
    }
}

enum State {
    Gain { gain: f32 },
    Suppress { l: Box<DenoiseState<'static>>, r: Box<DenoiseState<'static>>, inl: Vec<f32>, inr: Vec<f32>, out: Vec<f32> },
    Gate { open: f32, close: f32, attack: f32, release: f32, hold: u32, held: u32, is_open: bool, level: f32 },
    Comp { ratio: f32, thr: f32, attack: f32, release: f32, makeup: f32, env_db: f32 },
    Plugin(Option<ferrous_plugins::Instance>),
    Vst3(Option<Box<ferrous_vst3::Plugin>>),
}

impl State {
    fn new(f: &AudioFilter) -> Self {
        match *f {
            AudioFilter::Plugin { ref plugin, ref settings } => State::Plugin(
                ferrous_plugins::Instance::new(plugin, settings, ferrous_plugins::Kind::AudioFilter)
                    .map_err(|e| log::warn!("audio plugin: {e}"))
                    .ok(),
            ),
            AudioFilter::Vst3 { ref path, ref cid, ref name, ref state } => State::Vst3(
                ferrous_vst3::Plugin::load(std::path::Path::new(path), cid, SAMPLE_RATE as f64, crate::CHUNK_FRAMES, (!state.is_empty()).then_some(state.as_str()))
                    .map(Box::new)
                    .map_err(|e| log::warn!("VST3 {name}: {e:#}"))
                    .ok(),
            ),
            AudioFilter::Gain { db } => State::Gain { gain: db_to_lin(db) },
            AudioFilter::NoiseSuppression => State::Suppress {
                l: DenoiseState::new(),
                r: DenoiseState::new(),
                inl: Vec::new(),
                inr: Vec::new(),
                out: Vec::new(),
            },
            AudioFilter::NoiseGate { open_db, close_db, attack_ms, hold_ms, release_ms } => State::Gate {
                open: db_to_lin(open_db),
                close: db_to_lin(close_db.min(open_db)),
                // Linear ramp per sample, like OBS.
                attack: 1.0 / (attack_ms.max(0.1) / 1000.0 * SAMPLE_RATE as f32),
                release: 1.0 / (release_ms.max(0.1) / 1000.0 * SAMPLE_RATE as f32),
                hold: (hold_ms.max(0.0) / 1000.0 * SAMPLE_RATE as f32) as u32,
                held: 0,
                is_open: false,
                level: 0.0,
            },
            AudioFilter::Compressor { ratio, threshold_db, attack_ms, release_ms, makeup_db } => State::Comp {
                ratio: ratio.max(1.0),
                thr: threshold_db,
                attack: coef(attack_ms),
                release: coef(release_ms),
                makeup: db_to_lin(makeup_db),
                env_db: -120.0,
            },
            // A limiter is a compressor with an infinite ratio and a
            // near-instant attack, followed by a safety clip below.
            AudioFilter::Limiter { threshold_db, release_ms } => State::Comp {
                ratio: f32::INFINITY,
                thr: threshold_db,
                attack: 0.0,
                release: coef(release_ms),
                makeup: 1.0,
                env_db: -120.0,
            },
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        match self {
            State::Gain { gain } => buf.iter_mut().for_each(|s| *s *= *gain),
            State::Plugin(inst) => {
                if let Some(i) = inst {
                    i.filter_audio(buf);
                }
            }
            State::Vst3(p) => {
                if let Some(p) = p {
                    p.process(buf);
                }
            }
            State::Suppress { l, r, inl, inr, out } => {
                // RNNoise works on 480-sample mono frames in 16-bit scale.
                for fr in buf.chunks_exact(2) {
                    inl.push(fr[0] * 32768.0);
                    inr.push(fr[1] * 32768.0);
                }
                let n = DenoiseState::FRAME_SIZE;
                let mut ol = [0f32; DenoiseState::FRAME_SIZE];
                let mut or = [0f32; DenoiseState::FRAME_SIZE];
                while inl.len() >= n {
                    l.process_frame(&mut ol, &inl[..n]);
                    r.process_frame(&mut or, &inr[..n]);
                    inl.drain(..n);
                    inr.drain(..n);
                    for i in 0..n {
                        out.push(ol[i] / 32768.0);
                        out.push(or[i] / 32768.0);
                    }
                }
                // Our chunks are exactly one RNNoise frame, so this adds no
                // latency in practice; other sizes are padded until filled.
                let take = out.len().min(buf.len());
                let pad = buf.len() - take;
                buf[..pad].fill(0.0);
                buf[pad..].copy_from_slice(&out[..take]);
                out.drain(..take);
            }
            State::Gate { open, close, attack, release, hold, held, is_open, level } => {
                for fr in buf.chunks_exact_mut(2) {
                    let peak = fr[0].abs().max(fr[1].abs());
                    if peak >= *open {
                        *is_open = true;
                        *held = 0;
                    } else if *is_open && peak < *close {
                        *held += 1;
                        if *held > *hold {
                            *is_open = false;
                        }
                    }
                    if *is_open {
                        *level = (*level + *attack).min(1.0);
                    } else {
                        *level = (*level - *release).max(0.0);
                    }
                    fr[0] *= *level;
                    fr[1] *= *level;
                }
            }
            State::Comp { ratio, thr, attack, release, makeup, env_db } => {
                let limiter = ratio.is_infinite();
                for fr in buf.chunks_exact_mut(2) {
                    let peak_db = lin_to_db(fr[0].abs().max(fr[1].abs()));
                    let c = if peak_db > *env_db { *attack } else { *release };
                    *env_db = c * *env_db + (1.0 - c) * peak_db;
                    let over = *env_db - *thr;
                    let reduction = if over > 0.0 { over * (1.0 - 1.0 / *ratio) } else { 0.0 };
                    let g = db_to_lin(-reduction) * *makeup;
                    fr[0] *= g;
                    fr[1] *= g;
                    if limiter {
                        let ceil = db_to_lin(*thr);
                        fr[0] = fr[0].clamp(-ceil, ceil);
                        fr[1] = fr[1].clamp(-ceil, ceil);
                    }
                }
            }
        }
    }
}

/// A channel's filters, rebuilt only when the settings change.
#[derive(Default)]
pub struct FilterChain {
    config: AudioFilters,
    states: Vec<State>,
}

impl FilterChain {
    pub fn config(&self) -> &AudioFilters {
        &self.config
    }

    pub fn set(&mut self, config: &AudioFilters) {
        if *config == self.config {
            return;
        }
        // Keep running instances whose filter is unchanged (a VST3 whose only
        // change is its saved state counts as unchanged), so editing one
        // filter doesn't restart, or reload, the others.
        let mut old: Vec<(ferrous_core::filters::AudioFilter, Option<State>)> =
            self.config.iter().filter(|f| f.enabled).map(|f| f.filter.clone()).zip(self.states.drain(..).map(Some)).collect();
        let states = config
            .iter()
            .filter(|f| f.enabled)
            .map(|f| {
                let reuse = old.iter_mut().find(|(o, st)| st.is_some() && o.same_instance(&f.filter)).and_then(|(_, st)| st.take());
                reuse.unwrap_or_else(|| State::new(&f.filter))
            })
            .collect();
        self.config = config.clone();
        self.states = states;
    }

    /// The live VST3 instance behind config entry `index`, if it loaded.
    pub fn vst3_mut(&mut self, index: usize) -> Option<&mut ferrous_vst3::Plugin> {
        let f = self.config.get(index)?;
        if !f.enabled {
            return None;
        }
        let si = self.config[..index].iter().filter(|f| f.enabled).count();
        match self.states.get_mut(si)? {
            State::Vst3(Some(p)) => Some(p),
            _ => None,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for s in &mut self.states {
            s.process(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrous_core::filters::Filter;

    fn chain(f: AudioFilter) -> FilterChain {
        let mut c = FilterChain::default();
        c.set(&vec![Filter::new(f)]);
        c
    }

    fn sine(amp: f32, frames: usize, phase: &mut f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let s = amp * phase.sin();
            *phase += 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
            v.push(s);
            v.push(s);
        }
        v
    }

    fn peak(v: &[f32]) -> f32 {
        v.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn gain() {
        let mut c = chain(AudioFilter::Gain { db: -6.0 });
        let mut b = vec![1.0f32; 960];
        c.process(&mut b);
        assert!((b[0] - 0.501).abs() < 0.01);
    }

    #[test]
    fn gate_closes_on_quiet_and_opens_on_loud() {
        let mut c = chain(AudioFilter::NoiseGate { open_db: -26.0, close_db: -32.0, attack_ms: 5.0, hold_ms: 50.0, release_ms: 50.0 });
        let mut ph = 0.0;
        let mut quiet = Vec::new();
        for _ in 0..30 {
            let mut b = sine(0.01, 480, &mut ph); // -40 dB
            c.process(&mut b);
            quiet = b;
        }
        assert!(peak(&quiet) < 1e-6, "gate leaked {}", peak(&quiet));
        let mut loud = Vec::new();
        for _ in 0..5 {
            let mut b = sine(0.5, 480, &mut ph);
            c.process(&mut b);
            loud = b;
        }
        assert!(peak(&loud) > 0.49);
    }

    #[test]
    fn compressor_reduces_by_ratio() {
        // 0 dBFS-ish sine into -20 dB threshold, 4:1 => about -15 dB out.
        let mut c = chain(AudioFilter::Compressor { ratio: 4.0, threshold_db: -20.0, attack_ms: 1.0, release_ms: 50.0, makeup_db: 0.0 });
        let mut ph = 0.0;
        let mut last = Vec::new();
        for _ in 0..100 {
            let mut b = sine(1.0, 480, &mut ph);
            c.process(&mut b);
            last = b;
        }
        let out_db = lin_to_db(peak(&last));
        assert!((-17.0..-11.0).contains(&out_db), "{out_db}");
    }

    #[test]
    fn limiter_holds_ceiling() {
        let mut c = chain(AudioFilter::Limiter { threshold_db: -6.0, release_ms: 60.0 });
        let mut ph = 0.0;
        for _ in 0..20 {
            let mut b = sine(1.0, 480, &mut ph);
            c.process(&mut b);
            assert!(peak(&b) <= db_to_lin(-6.0) + 1e-4);
        }
    }

    #[test]
    fn noise_suppression_removes_white_noise() {
        let mut c = chain(AudioFilter::NoiseSuppression);
        let mut seed = 12345u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let mut in_energy = 0.0;
        let mut out_energy = 0.0;
        let mut lp = 0f32;
        for i in 0..200 {
            // Fan/hum-like noise: low-passed white noise, -30 dBFS-ish.
            let mut b = vec![0f32; 960];
            for fr in b.chunks_exact_mut(2) {
                lp = lp * 0.97 + rnd() * 0.03;
                fr[0] = lp * 0.5;
                fr[1] = lp * 0.5;
            }
            let e_in: f32 = b.iter().map(|s| s * s).sum();
            c.process(&mut b);
            if i > 50 {
                in_energy += e_in;
                out_energy += b.iter().map(|s| s * s).sum::<f32>();
            }
        }
        let reduction_db = 10.0 * (out_energy / in_energy).log10();
        assert!(reduction_db < -10.0, "only {reduction_db:.1} dB");
    }
}
