//! Energy-based activity segmentation. This is analysis over the captured
//! samples — it never touches the capture path (no VAD, AEC, or gain is ever
//! applied to the audio). Block edges are backdated to the actual first/last
//! threshold crossing so hangover time does not inflate measured latencies.

use crate::config::SegmenterConfig;

pub const WINDOW_MS: f64 = 10.0;
const NOISE_MARGIN_DB: f32 = 12.0;
const AUTO_FLOOR_DB: f32 = -65.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SegEvent {
    /// Activity started (provisional until Close; may be cancelled).
    Open { t_ns: u64 },
    /// Activity ended; `t_ns` is the last threshold crossing, not open+hangover.
    Close { t_ns: u64 },
    /// The open block was shorter than min_block_ms — discard it.
    Cancel,
}

enum State {
    Idle,
    Active { open_ns: u64, last_above_ns: u64 },
}

pub struct Segmenter {
    state: State,
    window_samples: usize,
    sum_sq: f64,
    n_in_window: usize,
    window_start_sample: u64,
    first_above_in_window: Option<u64>,
    last_above_in_window: Option<u64>,
    noise_floor_db: f32,
    pub effective_threshold_db: f32,
}

impl Segmenter {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            window_samples: 0,
            sum_sq: 0.0,
            n_in_window: 0,
            window_start_sample: 0,
            first_above_in_window: None,
            last_above_in_window: None,
            noise_floor_db: -70.0,
            effective_threshold_db: -45.0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Forget any in-progress block (no event emitted) while keeping the
    /// adapted noise floor and threshold. Used at session start: activity
    /// that began before the session must not leak in, but the floor
    /// estimate built up while idle must survive — a full reset drops the
    /// threshold below room ambient and opens a bogus block the moment the
    /// session starts.
    pub fn abort_block(&mut self) {
        self.state = State::Idle;
    }

    /// Feed samples. `base_sample` is the absolute index of `samples[0]` in
    /// the stream; `sample_ns` maps absolute sample index → app-clock ns.
    /// `gated(t_ns)` returns true when activity at that time must be ignored
    /// (echo gate). Events are appended to `out`.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        samples: &[f32],
        base_sample: u64,
        sample_rate: f64,
        cfg: &SegmenterConfig,
        sample_ns: &impl Fn(u64) -> u64,
        gated: &impl Fn(u64) -> bool,
        out: &mut Vec<SegEvent>,
    ) {
        if sample_rate <= 0.0 {
            return;
        }
        if self.window_samples == 0 {
            self.window_samples = ((sample_rate * WINDOW_MS / 1000.0) as usize).max(1);
            self.window_start_sample = base_sample;
        }
        let amp_threshold = db_to_amp(self.effective_threshold_db);

        for (i, &s) in samples.iter().enumerate() {
            let abs_idx = base_sample + i as u64;
            self.sum_sq += (s as f64) * (s as f64);
            if s.abs() >= amp_threshold {
                if self.first_above_in_window.is_none() {
                    self.first_above_in_window = Some(abs_idx);
                }
                self.last_above_in_window = Some(abs_idx);
            }
            self.n_in_window += 1;
            if self.n_in_window >= self.window_samples {
                self.finish_window(abs_idx + 1, cfg, sample_ns, gated, out);
            }
        }
    }

    fn finish_window(
        &mut self,
        next_sample: u64,
        cfg: &SegmenterConfig,
        sample_ns: &impl Fn(u64) -> u64,
        gated: &impl Fn(u64) -> bool,
        out: &mut Vec<SegEvent>,
    ) {
        let mean_sq = self.sum_sq / self.n_in_window.max(1) as f64;
        let rms_db = 10.0 * (mean_sq + 1e-12).log10() as f32;

        // Rolling noise-floor estimate: sink quickly toward quiet windows,
        // creep upward slowly so speech does not drag the floor up.
        if rms_db < self.noise_floor_db + 3.0 {
            self.noise_floor_db += 0.05 * (rms_db - self.noise_floor_db);
        } else {
            self.noise_floor_db += 0.02;
        }
        self.effective_threshold_db = if cfg.auto_threshold {
            (self.noise_floor_db + NOISE_MARGIN_DB).max(AUTO_FLOOR_DB)
        } else {
            cfg.threshold_db
        };

        let window_end_ns = sample_ns(next_sample);
        let over = rms_db >= self.effective_threshold_db && !gated(window_end_ns);

        match &mut self.state {
            State::Idle => {
                if over {
                    let open_sample = self
                        .first_above_in_window
                        .unwrap_or(self.window_start_sample);
                    let open_ns = sample_ns(open_sample);
                    let last_above_ns =
                        sample_ns(self.last_above_in_window.unwrap_or(open_sample));
                    out.push(SegEvent::Open { t_ns: open_ns });
                    self.state = State::Active {
                        open_ns,
                        last_above_ns,
                    };
                }
            }
            State::Active {
                open_ns,
                last_above_ns,
            } => {
                if over {
                    if let Some(last) = self.last_above_in_window {
                        *last_above_ns = sample_ns(last);
                    }
                } else {
                    // Below threshold this window — but a loud tail inside it
                    // still counts toward the block edge.
                    if let Some(last) = self.last_above_in_window {
                        *last_above_ns = (*last_above_ns).max(sample_ns(last));
                    }
                    let silence_ns = window_end_ns.saturating_sub(*last_above_ns);
                    if silence_ns >= cfg.hangover_ms * 1_000_000 {
                        out.push(close_or_cancel(*open_ns, *last_above_ns, cfg));
                        self.state = State::Idle;
                    }
                }
            }
        }

        self.sum_sq = 0.0;
        self.n_in_window = 0;
        self.window_start_sample = next_sample;
        self.first_above_in_window = None;
        self.last_above_in_window = None;
    }

    /// Close any open block at end of session.
    pub fn finalize(&mut self, cfg: &SegmenterConfig, out: &mut Vec<SegEvent>) {
        if let State::Active {
            open_ns,
            last_above_ns,
        } = self.state
        {
            out.push(close_or_cancel(open_ns, last_above_ns, cfg));
        }
        self.state = State::Idle;
    }
}

/// Close the block if it met min_block_ms, otherwise cancel it as a blip.
fn close_or_cancel(open_ns: u64, last_above_ns: u64, cfg: &SegmenterConfig) -> SegEvent {
    if last_above_ns.saturating_sub(open_ns) >= cfg.min_block_ms * 1_000_000 {
        SegEvent::Close { t_ns: last_above_ns }
    } else {
        SegEvent::Cancel
    }
}

pub fn db_to_amp(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        signal: &[f32],
        sr: f64,
        cfg: &SegmenterConfig,
    ) -> Vec<SegEvent> {
        let mut seg = Segmenter::new();
        let mut out = Vec::new();
        let sample_ns = |n: u64| (n as f64 / sr * 1e9) as u64;
        let gated = |_t: u64| false;
        seg.process(signal, 0, sr, cfg, &sample_ns, &gated, &mut out);
        seg.finalize(cfg, &mut out);
        out
    }

    fn burst_signal(sr: usize, pre_ms: u64, burst_ms: u64, post_ms: u64, amp: f32) -> Vec<f32> {
        let ms = |m: u64| m as usize * sr / 1000;
        let mut v = vec![0.0f32; ms(pre_ms)];
        for i in 0..ms(burst_ms) {
            v.push(amp * (i as f32 * 0.3).sin());
        }
        v.extend(vec![0.0f32; ms(post_ms)]);
        v
    }

    #[test]
    fn detects_burst_with_precise_edges() {
        let sr = 48000usize;
        let cfg = SegmenterConfig::default();
        let signal = burst_signal(sr, 500, 400, 800, 0.5);
        let events = run(&signal, sr as f64, &cfg);
        assert_eq!(events.len(), 2, "events: {events:?}");
        let SegEvent::Open { t_ns: open } = events[0] else {
            panic!("expected open, got {events:?}")
        };
        let SegEvent::Close { t_ns: close } = events[1] else {
            panic!("expected close, got {events:?}")
        };
        let open_ms = open as f64 / 1e6;
        let close_ms = close as f64 / 1e6;
        assert!((open_ms - 500.0).abs() < 15.0, "open at {open_ms}");
        assert!((close_ms - 900.0).abs() < 15.0, "close at {close_ms}");
    }

    #[test]
    fn short_click_is_cancelled() {
        let sr = 48000usize;
        let cfg = SegmenterConfig::default(); // min_block 100ms
        let signal = burst_signal(sr, 300, 40, 600, 0.8);
        let events = run(&signal, sr as f64, &cfg);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], SegEvent::Open { .. }));
        assert_eq!(events[1], SegEvent::Cancel);
    }

    #[test]
    fn intra_speech_pause_shorter_than_hangover_stays_one_block() {
        let sr = 48000usize;
        let cfg = SegmenterConfig::default(); // hangover 250ms
        let ms = |m: usize| m * sr / 1000;
        let mut signal = vec![0.0f32; ms(200)];
        for i in 0..ms(300) {
            signal.push(0.5 * (i as f32 * 0.3).sin());
        }
        signal.extend(vec![0.0f32; ms(150)]); // pause < hangover
        for i in 0..ms(300) {
            signal.push(0.5 * (i as f32 * 0.3).sin());
        }
        signal.extend(vec![0.0f32; ms(600)]);
        let events = run(&signal, sr as f64, &cfg);
        let opens = events
            .iter()
            .filter(|e| matches!(e, SegEvent::Open { .. }))
            .count();
        assert_eq!(opens, 1, "events: {events:?}");
    }

    /// Deterministic uniform noise at the given peak amplitude.
    fn noise(n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let h = (i as u32).wrapping_mul(2654435761);
                ((h >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 2.0 * amp
            })
            .collect()
    }

    /// Session start must not re-open blocks on room ambient: the noise
    /// floor adapted while idle survives abort_block(), so the first block
    /// of a session starts at speech onset, not at the Start press.
    #[test]
    fn abort_block_keeps_adapted_floor() {
        let sr = 48_000.0;
        let cfg = SegmenterConfig {
            auto_threshold: true,
            ..SegmenterConfig::default()
        };
        let sample_ns = |n: u64| (n as f64 / sr * 1e9) as u64;
        let gated = |_t: u64| false;
        // Ambient at ~-40 dB RMS opens a block on a freshly reset segmenter
        // (threshold starts near -58 dB) — the failure mode being prevented.
        let ambient = noise((sr * 2.0) as usize, 0.0173);
        let mut fresh = Segmenter::new();
        let mut out = Vec::new();
        fresh.process(&ambient, 0, sr, &cfg, &sample_ns, &gated, &mut out);
        assert!(
            out.iter().any(|e| matches!(e, SegEvent::Open { .. })),
            "ambient should trip a cold segmenter: {out:?}"
        );

        // Warm up on 20s of the same ambient, then "start a session".
        let mut seg = Segmenter::new();
        let mut consumed: u64 = 0;
        let mut warm = Vec::new();
        let warmup = noise((sr * 20.0) as usize, 0.0173);
        seg.process(&warmup, consumed, sr, &cfg, &sample_ns, &gated, &mut warm);
        consumed += warmup.len() as u64;
        seg.abort_block();

        // Ambient after start: no blocks.
        let mut out = Vec::new();
        seg.process(&ambient, consumed, sr, &cfg, &sample_ns, &gated, &mut out);
        consumed += ambient.len() as u64;
        assert!(out.is_empty(), "ambient after warm start: {out:?}");

        // Speech still opens a block, backdated to its own onset.
        let speech_at_ms = consumed as f64 / sr * 1000.0;
        let mut signal: Vec<f32> = (0..(sr * 0.4) as usize)
            .map(|i| 0.5 * (i as f32 * 0.3).sin())
            .collect();
        signal.extend(noise((sr * 0.8) as usize, 0.0173));
        seg.process(&signal, consumed, sr, &cfg, &sample_ns, &gated, &mut out);
        seg.finalize(&cfg, &mut out);
        let SegEvent::Open { t_ns } = out[0] else {
            panic!("expected open, got {out:?}")
        };
        let open_ms = t_ns as f64 / 1e6;
        assert!(
            (open_ms - speech_at_ms).abs() < 15.0,
            "open at {open_ms}, speech at {speech_at_ms}"
        );
    }

    #[test]
    fn gate_suppresses_activity() {
        let sr = 48000usize;
        let cfg = SegmenterConfig::default();
        let signal = burst_signal(sr, 200, 400, 400, 0.5);
        let mut seg = Segmenter::new();
        let mut out = Vec::new();
        let sample_ns = |n: u64| (n as f64 / sr as f64 * 1e9) as u64;
        let gated = |_t: u64| true;
        seg.process(&signal, 0, sr as f64, &cfg, &sample_ns, &gated, &mut out);
        seg.finalize(&cfg, &mut out);
        assert!(out.is_empty(), "events: {out:?}");
    }
}
