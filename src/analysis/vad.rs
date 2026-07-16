//! Silero VAD, run with pure-Rust ONNX inference (tract) — no native
//! runtime, model embedded in the binary.
//!
//! This classifier is a VISUAL layer only: it labels sub-regions of the
//! energy-detected mic blocks as speech vs non-speech so the timeline can
//! wash out "mic activity that wasn't speech". It never moves block edges,
//! never gates the segmenter, and never feeds turn pairing — metrics are
//! identical with it on or off. Classification lags the live edge by one
//! 32 ms chunk plus hysteresis; that only delays the tint, not the data.

use tract_onnx::prelude::*;

// The OpenVINO 16 kHz export of Silero VAD: a flat graph with no ONNX `If`
// control flow (which pure-Rust tract cannot type-check) and fixed shapes —
// input [1, 64 context + 512 chunk], state [2,1,128], no `sr` input.
static MODEL_BYTES: &[u8] = include_bytes!("../../assets/silero_vad_openvino_16k_named.onnx");

const TARGET_SR: f64 = 16_000.0;
const CHUNK: usize = 512; // 32 ms at 16 kHz
const CONTEXT: usize = 64;

const SPEECH_START_PROB: f32 = 0.60;
const SPEECH_KEEP_PROB: f32 = 0.35;
/// Silence (prob < keep) needed to close a speech region.
const SPEECH_HANGOVER_MS: f64 = 240.0;
/// Speech regions shorter than this are discarded.
const MIN_SPEECH_MS: f64 = 96.0;

type Plan = std::sync::Arc<TypedSimplePlan>;

/// Linear resampler to 16 kHz that tracks which original sample indices each
/// output sample came from, so chunk probabilities map back onto the
/// sample-accurate app timeline.
struct Resampler {
    ratio: f64, // input samples per output sample
    out_count: u64,
    // previous input sample (for interpolation across buffer boundaries)
    prev: f32,
    prev_idx: i64, // absolute input index of `prev`; -1 before any input
}

impl Resampler {
    fn new(input_sr: f64) -> Self {
        Self {
            ratio: input_sr / TARGET_SR,
            out_count: 0,
            prev: 0.0,
            prev_idx: -1,
        }
    }

    /// Original-sample index for a 16 kHz output sample index.
    fn orig_index(&self, out_idx: u64) -> u64 {
        (out_idx as f64 * self.ratio) as u64
    }

    fn process(&mut self, input: &[f32], base_idx: u64, out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.prev_idx < 0 {
            // Stream may begin mid-session at an arbitrary absolute index:
            // align the first output with the first input we actually have.
            self.out_count = (base_idx as f64 / self.ratio).ceil() as u64;
        }
        let last_idx = base_idx as i64 + input.len() as i64 - 1;
        loop {
            let pos = self.out_count as f64 * self.ratio;
            let i0 = pos.floor() as i64;
            let i1 = i0 + 1;
            if i1 > last_idx {
                break;
            }
            let frac = (pos - i0 as f64) as f32;
            let s0 = if i0 < base_idx as i64 {
                if i0 == self.prev_idx {
                    self.prev
                } else {
                    0.0
                }
            } else {
                input[(i0 - base_idx as i64) as usize]
            };
            let s1 = input[(i1 - base_idx as i64) as usize];
            out.push(s0 + (s1 - s0) * frac);
            self.out_count += 1;
        }
        self.prev = input[input.len() - 1];
        self.prev_idx = last_idx;
    }
}

/// A classified chunk: original-sample span and speech probability.
pub struct ChunkProb {
    pub start_sample: u64,
    pub end_sample: u64,
    pub prob: f32,
}

pub struct SileroVad {
    plan: Plan,
    state: Tensor,
    context: Vec<f32>,
    resampler: Resampler,
    buf16k: Vec<f32>,
}

fn shared_plan() -> anyhow::Result<Plan> {
    static PLAN: std::sync::OnceLock<Result<Plan, String>> = std::sync::OnceLock::new();
    PLAN.get_or_init(|| {
        tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(MODEL_BYTES))
            .and_then(|m| m.with_input_fact(0, f32::fact([1, CONTEXT + CHUNK]).into()))
            .and_then(|m| m.with_input_fact(1, f32::fact([2, 1, 128]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|e| format!("{e:?}"))
    })
    .clone()
    .map_err(|e| anyhow::anyhow!("silero model load failed: {e}"))
}

impl SileroVad {
    pub fn new(input_sr: f64) -> anyhow::Result<Self> {
        Ok(Self {
            plan: shared_plan()?,
            state: Tensor::zero::<f32>(&[2, 1, 128])?,
            context: vec![0.0; CONTEXT],
            resampler: Resampler::new(input_sr),
            buf16k: Vec::with_capacity(CHUNK * 4),
        })
    }

    pub fn input_sample_rate(&self) -> f64 {
        self.resampler.ratio * TARGET_SR
    }

    /// Feed detection-path samples (any rate); returns per-chunk speech
    /// probabilities with their original-sample spans.
    pub fn process(
        &mut self,
        samples: &[f32],
        base_idx: u64,
        out: &mut Vec<ChunkProb>,
    ) -> anyhow::Result<()> {
        self.resampler.process(samples, base_idx, &mut self.buf16k);
        while self.buf16k.len() >= CHUNK {
            let mut input = Vec::with_capacity(CONTEXT + CHUNK);
            input.extend_from_slice(&self.context);
            input.extend_from_slice(&self.buf16k[..CHUNK]);
            self.context.copy_from_slice(&self.buf16k[CHUNK - CONTEXT..CHUNK]);

            let input_t = tract_ndarray::Array2::from_shape_vec((1, CONTEXT + CHUNK), input)?;
            let result = self.plan.run(tvec!(
                Tensor::from(input_t).into(),
                self.state.clone().into()
            ))?;
            // Identify outputs by shape (prob is a single value, the RNN
            // state is [2,1,128]) rather than trusting their order.
            let (prob_idx, state_idx) = if result[0].len() == 1 { (0, 1) } else { (1, 0) };
            let prob: f32 = *result[prob_idx]
                .to_plain_array_view::<f32>()?
                .iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("empty prob output"))?;
            self.state = result[state_idx].clone().into_tensor();

            // buf16k[0] sits at 16 kHz output index out_count − len: the
            // resampler aligns out_count with the absolute input index, so
            // chunk spans stay absolute when the stream begins mid-session
            // (the mic runs from app launch; the VAD is built at Start).
            let start_out = self.resampler.out_count - self.buf16k.len() as u64;
            let end_out = start_out + CHUNK as u64;
            out.push(ChunkProb {
                start_sample: self.resampler.orig_index(start_out),
                end_sample: self.resampler.orig_index(end_out),
                prob,
            });
            self.buf16k.drain(..CHUNK);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VadEvent {
    Open { t_ns: u64 },
    Close { t_ns: u64 },
    Cancel,
}

/// Hysteresis over chunk probabilities → speech intervals (visual only).
#[derive(Default)]
pub struct SpeechTrack {
    open_ns: Option<u64>,
    last_speech_end_ns: u64,
}

impl SpeechTrack {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn on_chunk(&mut self, start_ns: u64, end_ns: u64, prob: f32, out: &mut Vec<VadEvent>) {
        match self.open_ns {
            None => {
                if prob >= SPEECH_START_PROB {
                    self.open_ns = Some(start_ns);
                    self.last_speech_end_ns = end_ns;
                    out.push(VadEvent::Open { t_ns: start_ns });
                }
            }
            Some(open_ns) => {
                if prob >= SPEECH_KEEP_PROB {
                    self.last_speech_end_ns = end_ns;
                } else {
                    let silence_ms =
                        end_ns.saturating_sub(self.last_speech_end_ns) as f64 / 1e6;
                    if silence_ms >= SPEECH_HANGOVER_MS {
                        out.push(self.close_or_cancel(open_ns));
                        self.open_ns = None;
                    }
                }
            }
        }
    }

    pub fn finalize(&mut self, out: &mut Vec<VadEvent>) {
        if let Some(open_ns) = self.open_ns.take() {
            out.push(self.close_or_cancel(open_ns));
        }
    }

    /// Close the region if it met MIN_SPEECH_MS, otherwise cancel it as a blip.
    fn close_or_cancel(&self, open_ns: u64) -> VadEvent {
        let dur_ms = self.last_speech_end_ns.saturating_sub(open_ns) as f64 / 1e6;
        if dur_ms >= MIN_SPEECH_MS {
            VadEvent::Close {
                t_ns: self.last_speech_end_ns,
            }
        } else {
            VadEvent::Cancel
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_loads_and_runs() {
        let mut vad = SileroVad::new(48_000.0).expect("silero model should load under tract");
        // 200ms of white-ish noise at 48k → several 16k chunks.
        let samples: Vec<f32> = (0..9600)
            .map(|i| ((i * 2654435761u64 as usize) as f32 / u32::MAX as f32 - 0.5) * 0.1)
            .collect();
        let mut probs = Vec::new();
        vad.process(&samples, 0, &mut probs).expect("inference");
        assert!(!probs.is_empty(), "should emit chunks");
        for c in &probs {
            assert!((0.0..=1.0).contains(&c.prob), "prob {}", c.prob);
            assert!(c.end_sample > c.start_sample);
        }
        // Noise should not look like speech.
        let max = probs.iter().map(|c| c.prob).fold(0.0f32, f32::max);
        assert!(max < SPEECH_START_PROB, "noise max prob {max}");
    }

    #[test]
    fn chunk_spans_track_absolute_input_index() {
        // The mic stream runs long before a session starts, so the first
        // process() call arrives with a large base index. Chunk spans must
        // land at that index, not at stream sample 0.
        let mut vad = SileroVad::new(48_000.0).expect("model");
        let base: u64 = 5 * 60 * 48_000; // stream began 5 minutes earlier
        let samples: Vec<f32> = (0..4800).map(|i| 0.1 * (i as f32 * 0.3).sin()).collect();
        let mut probs = Vec::new();
        vad.process(&samples, base, &mut probs).expect("inference");
        assert!(!probs.is_empty(), "100ms at 48k should yield chunks");
        let first = &probs[0];
        assert!(
            first.start_sample >= base && first.start_sample < base + 4800,
            "first chunk starts at {} (base {base})",
            first.start_sample
        );
        for pair in probs.windows(2) {
            assert_eq!(pair[0].end_sample, pair[1].start_sample, "spans contiguous");
        }
    }

    #[test]
    fn resampler_maps_indices() {
        let mut r = Resampler::new(48_000.0);
        let mut out = Vec::new();
        let input = vec![0.5f32; 4800]; // 100ms at 48k
        r.process(&input, 0, &mut out);
        assert!((out.len() as i64 - 1600).abs() <= 2, "got {} samples", out.len());
        assert_eq!(r.orig_index(1600), 4800);
    }

    #[test]
    fn speech_track_hysteresis() {
        let mut track = SpeechTrack::default();
        let mut evs = Vec::new();
        let ms = |m: u64| m * 1_000_000;
        // 10 chunks of speech (32ms each), then silence.
        for i in 0..10u64 {
            track.on_chunk(ms(i * 32), ms((i + 1) * 32), 0.9, &mut evs);
        }
        assert_eq!(evs, vec![VadEvent::Open { t_ns: 0 }]);
        for i in 10..20u64 {
            track.on_chunk(ms(i * 32), ms((i + 1) * 32), 0.1, &mut evs);
        }
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1], VadEvent::Close { t_ns: ms(320) });
    }

    #[test]
    fn speech_track_cancels_blips() {
        let mut track = SpeechTrack::default();
        let mut evs = Vec::new();
        let ms = |m: u64| m * 1_000_000;
        // Single 32ms chunk over threshold: shorter than MIN_SPEECH_MS.
        track.on_chunk(0, ms(32), 0.9, &mut evs);
        for i in 1..12u64 {
            track.on_chunk(ms(i * 32), ms((i + 1) * 32), 0.05, &mut evs);
        }
        assert_eq!(evs, vec![VadEvent::Open { t_ns: 0 }, VadEvent::Cancel]);
    }
}
