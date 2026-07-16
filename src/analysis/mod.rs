pub mod bins;
pub mod filter;
pub mod segmenter;
pub mod stats;
pub mod turns;
pub mod vad;

use crate::audio::{RingSet, SharedInputs, SyncEvent, LANE_MIC, LANE_SYS};
use crate::clock;
use crate::session::{Block, Phase, SharedState, SysStatus};
use bins::{Bin, FINE_BIN_MS};
use segmenter::{SegEvent, Segmenter};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use turns::TurnTracker;

/// Maps a stream's absolute sample index onto the app clock.
///
/// Each capture callback contributes a candidate offset (first-frame time
/// minus samples-so-far/rate). Scheduling delay only ever makes candidates
/// later, so a rolling minimum tracks the true offset; the window lets it
/// follow genuine sample-clock drift in both directions.
pub struct TimeMap {
    sample_rate: f64,
    candidates: VecDeque<f64>,
    offset_ns: f64,
    ready: bool,
}

const TIMEMAP_WINDOW: usize = 96;

impl TimeMap {
    pub fn new() -> Self {
        Self {
            sample_rate: 0.0,
            candidates: VecDeque::new(),
            offset_ns: 0.0,
            ready: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn on_sync(&mut self, ev: &SyncEvent) {
        if ev.sample_rate <= 0.0 {
            return;
        }
        if !self.ready || (ev.sample_rate - self.sample_rate).abs() > 0.5 {
            self.sample_rate = ev.sample_rate;
            self.candidates.clear();
        }
        let cand = ev.first_frame_ns as f64 - ev.samples_before as f64 / self.sample_rate * 1e9;
        self.candidates.push_back(cand);
        if self.candidates.len() > TIMEMAP_WINDOW {
            self.candidates.pop_front();
        }
        self.offset_ns = self
            .candidates
            .iter()
            .fold(f64::INFINITY, |a, &b| a.min(b));
        self.ready = true;
    }

    pub fn ready(&self) -> bool {
        self.ready && self.sample_rate > 0.0
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn sample_ns(&self, n: u64) -> u64 {
        if !self.ready() {
            return clock::now_ns();
        }
        (self.offset_ns + n as f64 / self.sample_rate * 1e9).max(0.0) as u64
    }
}

struct LaneProc {
    gen: u64,
    timemap: TimeMap,
    segmenter: Segmenter,
    consumed: u64, // absolute sample index of next sample to process
    scratch: Vec<f32>,
    // Detection-path band filter (see analysis::filter). Never applied to
    // the stored/displayed/recorded audio.
    det_filter: Option<filter::SpeechBandFilter>,
    det_scratch: Vec<f32>,
    // fine-bin accumulation
    bin_start_sample: u64,
    bin_min: f32,
    bin_max: f32,
    bin_count: usize,
    wav: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    /// WAV recording requested but writer not yet created (rate unknown).
    wav_pending: bool,
    seen_device_changes: u64,
}

impl LaneProc {
    fn new() -> Self {
        Self {
            gen: 0,
            timemap: TimeMap::new(),
            segmenter: Segmenter::new(),
            consumed: 0,
            scratch: Vec::with_capacity(48_000),
            det_filter: None,
            det_scratch: Vec::with_capacity(48_000),
            bin_start_sample: 0,
            bin_min: 0.0,
            bin_max: 0.0,
            bin_count: 0,
            wav: None,
            wav_pending: false,
            seen_device_changes: 0,
        }
    }

    fn reset_stream(&mut self, gen: u64) {
        self.gen = gen;
        self.timemap.reset();
        self.segmenter.reset();
        self.consumed = 0;
        self.det_filter = None;
        self.reset_session_accumulators();
    }

    fn reset_session_accumulators(&mut self) {
        self.bin_start_sample = self.consumed;
        self.bin_min = 0.0;
        self.bin_max = 0.0;
        self.bin_count = 0;
        // Keep the noise floor adapted while idle; only drop block state so
        // pre-session activity can't leak into the new session.
        self.segmenter.abort_block();
        self.finalize_wav();
    }

    fn finalize_wav(&mut self) {
        if let Some(w) = self.wav.take() {
            let _ = w.finalize();
        }
    }
}

pub struct AnalysisHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AnalysisHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn spawn(shared: SharedState, inputs: SharedInputs) -> AnalysisHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let join = std::thread::Builder::new()
        .name("analysis".into())
        .spawn(move || run(shared, inputs, stop2))
        .expect("spawn analysis thread");
    AnalysisHandle {
        stop,
        join: Some(join),
    }
}

fn drain(ring: &mut RingSet, proc_: &mut LaneProc) {
    // Sync events first so the TimeMap covers the samples we are about to read.
    while let Ok(ev) = ring.syncs.pop() {
        proc_.timemap.on_sync(&ev);
    }
    let n = ring.samples.slots();
    if n == 0 {
        return;
    }
    if let Ok(chunk) = ring.samples.read_chunk(n) {
        let (a, b) = chunk.as_slices();
        proc_.scratch.extend_from_slice(a);
        proc_.scratch.extend_from_slice(b);
        chunk.commit_all();
    }
}

fn run(shared: SharedState, inputs: SharedInputs, stop: Arc<AtomicBool>) {
    let mut procs = [LaneProc::new(), LaneProc::new()];
    let mut tracker = TurnTracker::new();
    let mut was_running = false;
    // Echo gate: bot-audio activity intervals (closed) plus the currently
    // open one. Interval-based so gating is correct for a mic window at time
    // t regardless of how samples are batched.
    let mut sys_intervals: Vec<(u64, u64)> = Vec::new();
    let mut sys_open_since: Option<u64> = None;
    // Session folder for WAV recording, captured at session start.
    let mut wav_dir: Option<std::path::PathBuf> = None;
    // Visual-only speech classification for the mic lane (Silero VAD).
    let mut mic_vad: Option<vad::SileroVad> = None;
    let mut speech_track = vad::SpeechTrack::default();
    let mut vad_failed = false;
    let mut vad_chunks: Vec<vad::ChunkProb> = Vec::new();
    let mut last_mic_gen: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));

        // ---- drain rings (short lock on inputs) ----
        {
            let mut ins = inputs.lock();
            if let Some(ring) = ins.mic.as_mut() {
                if ring.gen != procs[LANE_MIC].gen {
                    procs[LANE_MIC].reset_stream(ring.gen);
                }
                drain(ring, &mut procs[LANE_MIC]);
            }
            if let Some(ring) = ins.sys.as_mut() {
                if ring.gen != procs[LANE_SYS].gen {
                    procs[LANE_SYS].reset_stream(ring.gen);
                }
                drain(ring, &mut procs[LANE_SYS]);
            }
        }
        if procs[LANE_MIC].gen != last_mic_gen {
            // New mic stream: sample indexing restarted, rebuild the VAD.
            last_mic_gen = procs[LANE_MIC].gen;
            mic_vad = None;
            speech_track.reset();
        }

        // ---- process under the shared lock ----
        let mut sh = shared.lock();
        let now = clock::now_ns();

        // Session phase transitions.
        let running = sh.phase == Phase::Running;
        if running && !was_running {
            tracker.reset();
            sys_intervals.clear();
            sys_open_since = None;
            for p in &mut procs {
                p.reset_session_accumulators();
            }
            wav_dir = sh.cfg.record_wav.then(|| sh.session_dir.clone()).flatten();
            for p in &mut procs {
                p.wav_pending = wav_dir.is_some();
            }
            mic_vad = None; // rebuilt lazily with the current sample rate
            speech_track.reset();
            vad_failed = false;
        }
        if !running && was_running {
            // Session just stopped: close open blocks and finalize WAVs.
            let mut tick_events: Vec<(usize, u64, SegEvent)> = Vec::new();
            for lane in [LANE_SYS, LANE_MIC] {
                let mut evs = Vec::new();
                procs[lane].segmenter.finalize(seg_cfg(&sh.cfg, lane), &mut evs);
                queue_events(&mut tick_events, lane, evs, now);
                procs[lane].wav_pending = false;
                procs[lane].finalize_wav();
            }
            let mut vevs = Vec::new();
            speech_track.finalize(&mut vevs);
            apply_speech_events(&mut sh.lanes[LANE_MIC], vevs);
            mic_vad = None;
            apply_events(&mut sh, &mut tracker, tick_events);
        }
        was_running = running;

        let session_start = sh.session_start_ns;
        let mut tick_events: Vec<(usize, u64, SegEvent)> = Vec::new();

        // Process sys lane first so the echo gate sees fresh bot activity.
        for lane in [LANE_SYS, LANE_MIC] {
            let p = &mut procs[lane];
            if p.scratch.is_empty() {
                // No new samples this tick.
                continue;
            }
            let samples = std::mem::take(&mut p.scratch);
            let sr = p.timemap.sample_rate();

            // Cloned because `sh` is mutably borrowed below.
            let cfg = seg_cfg(&sh.cfg, lane).clone();

            // Detection path: optionally band-limit to speech frequencies.
            // Everything visual/recorded (bins, WAV) stays raw; only the
            // meter and the segmenter see the filtered signal, and both see
            // the SAME signal so the threshold ticks stay truthful.
            if cfg.speech_band && sr > 0.0 {
                let rebuild = match &p.det_filter {
                    Some(f) => (f.sample_rate() - sr).abs() > 0.5,
                    None => true,
                };
                if rebuild {
                    p.det_filter = Some(filter::SpeechBandFilter::new(sr));
                }
            } else {
                p.det_filter = None;
            }
            let det: &[f32] = if let Some(f) = &mut p.det_filter {
                f.process_into(&samples, &mut p.det_scratch);
                &p.det_scratch
            } else {
                &samples
            };

            // Level meter (always, even when idle): RMS over the last ≤30ms
            // of the detection signal.
            let tail = if sr > 0.0 {
                (sr * 0.03) as usize
            } else {
                1024
            };
            let start = det.len().saturating_sub(tail.max(1));
            let mean_sq: f64 = det[start..]
                .iter()
                .map(|s| (*s as f64) * (*s as f64))
                .sum::<f64>()
                / (det.len() - start).max(1) as f64;
            sh.lanes[lane].level_db = (10.0 * (mean_sq + 1e-12).log10()) as f32;
            sh.lanes[lane].effective_threshold_db = p.segmenter.effective_threshold_db;
            if p.timemap.ready() {
                sh.lanes[lane].last_sample_ns =
                    p.timemap.sample_ns(p.consumed + samples.len() as u64);
            }

            if p.timemap.ready() && sr > 0.0 {
                if running {
                    // WAV: create lazily once the sample rate is known.
                    if p.wav_pending && p.wav.is_none() {
                        p.wav_pending = false;
                        if let Some(dir) = &wav_dir {
                            let name = if lane == LANE_MIC { "mic.wav" } else { "system.wav" };
                            let spec = hound::WavSpec {
                                channels: 1,
                                sample_rate: sr as u32,
                                bits_per_sample: 32,
                                sample_format: hound::SampleFormat::Float,
                            };
                            match hound::WavWriter::create(dir.join(name), spec) {
                                Ok(w) => p.wav = Some(w),
                                Err(e) => sh.wav_error = Some(format!("wav create failed: {e}")),
                            }
                        }
                    }
                    if let Some(w) = &mut p.wav {
                        let mut failed = false;
                        for &s in &samples {
                            if w.write_sample(s).is_err() {
                                failed = true;
                                break;
                            }
                        }
                        if failed {
                            sh.wav_error = Some("wav write failed (disk full?)".into());
                            if let Some(w) = p.wav.take() {
                                let _ = w.finalize();
                            }
                        }
                    }

                    // Waveform bins.
                    let fine_samples = ((sr * FINE_BIN_MS as f64 / 1000.0) as usize).max(1);
                    if p.bin_count == 0 && p.bin_start_sample < p.consumed {
                        p.bin_start_sample = p.consumed;
                    }
                    for (i, &s) in samples.iter().enumerate() {
                        if p.bin_count == 0 {
                            p.bin_min = s;
                            p.bin_max = s;
                        } else {
                            p.bin_min = p.bin_min.min(s);
                            p.bin_max = p.bin_max.max(s);
                        }
                        p.bin_count += 1;
                        if p.bin_count >= fine_samples {
                            let t = p.timemap.sample_ns(p.bin_start_sample);
                            if t >= session_start {
                                let idx =
                                    ((t - session_start) / (FINE_BIN_MS * 1_000_000)) as usize;
                                sh.lanes[lane].bins.push_fine_at(
                                    idx,
                                    Bin {
                                        min: p.bin_min,
                                        max: p.bin_max,
                                    },
                                );
                            }
                            p.bin_start_sample = p.consumed + i as u64 + 1;
                            p.bin_count = 0;
                        }
                    }
                }

                // Segmentation (on the detection signal). Runs while idle
                // too, so the auto-threshold noise floor is already adapted
                // when a session starts; idle events are discarded.
                let gate_enabled = running && sh.cfg.echo_gate.enabled && lane == LANE_MIC;
                let tail_ns = sh.cfg.echo_gate.tail_ms * 1_000_000;
                let mut evs = Vec::new();
                {
                    let base = p.consumed;
                    let tm = &p.timemap;
                    let sample_ns = |n: u64| tm.sample_ns(n);
                    let intervals = &sys_intervals;
                    let open_since = sys_open_since;
                    let gated = |t_ns: u64| {
                        if !gate_enabled {
                            return false;
                        }
                        if let Some(s) = open_since {
                            if t_ns >= s {
                                return true;
                            }
                        }
                        intervals
                            .iter()
                            .any(|&(s, e)| t_ns >= s && t_ns <= e + tail_ns)
                    };
                    p.segmenter
                        .process(det, base, sr, &cfg, &sample_ns, &gated, &mut evs);
                }
                if running {
                    if lane == LANE_SYS {
                        for e in &evs {
                            match *e {
                                SegEvent::Open { t_ns } => sys_open_since = Some(t_ns),
                                SegEvent::Close { t_ns } => {
                                    let start = sys_open_since.take().unwrap_or(t_ns);
                                    sys_intervals.push((start, t_ns));
                                }
                                SegEvent::Cancel => {
                                    // Even a sub-min-block bot blip is real sound
                                    // that can echo: keep a short interval.
                                    if let Some(s) = sys_open_since.take() {
                                        sys_intervals.push((s, s + 100_000_000));
                                    }
                                }
                            }
                        }
                    }
                    queue_events(&mut tick_events, lane, evs, now);

                    // Visual speech classification (mic lane only): Silero VAD
                    // tints speech vs other mic activity. Never touches block
                    // edges, turns, or any metric.
                    if lane == LANE_MIC {
                        if sh.cfg.vad_tint && !vad_failed {
                            let needs_new = mic_vad
                                .as_ref()
                                .map(|v| (v.input_sample_rate() - sr).abs() > 0.5)
                                .unwrap_or(true);
                            if needs_new {
                                match vad::SileroVad::new(sr) {
                                    Ok(v) => {
                                        mic_vad = Some(v);
                                        speech_track.reset();
                                        sh.vad_error = None;
                                    }
                                    Err(e) => {
                                        vad_failed = true;
                                        sh.vad_error = Some(e.to_string());
                                    }
                                }
                            }
                            if let Some(v) = &mut mic_vad {
                                vad_chunks.clear();
                                if let Err(e) = v.process(det, p.consumed, &mut vad_chunks) {
                                    vad_failed = true;
                                    sh.vad_error = Some(e.to_string());
                                    mic_vad = None;
                                }
                                let mut vevs = Vec::new();
                                for c in &vad_chunks {
                                    let t0 =
                                        p.timemap.sample_ns(c.start_sample).max(session_start);
                                    let t1 = p.timemap.sample_ns(c.end_sample).max(session_start);
                                    speech_track.on_chunk(t0, t1, c.prob, &mut vevs);
                                }
                                apply_speech_events(&mut sh.lanes[LANE_MIC], vevs);
                            }
                        } else if !sh.cfg.vad_tint {
                            mic_vad = None;
                        }
                    }
                }
            }
            p.consumed += samples.len() as u64;
        }

        // Tap health: discontinuities and failures.
        {
            let ins = inputs.lock();
            if let Some(sys) = &ins.sys {
                if let Some(changes) = &sys.device_changed {
                    let c = changes.load(Ordering::Relaxed);
                    if c > procs[LANE_SYS].seen_device_changes {
                        procs[LANE_SYS].seen_device_changes = c;
                        if running {
                            sh.discontinuities.push(now);
                            tracker.note_discontinuity();
                            // Force-close any open sys block; timing across the
                            // device change is not trustworthy.
                            let mut evs = Vec::new();
                            procs[LANE_SYS].segmenter.finalize(&sh.cfg.sys_segmenter, &mut evs);
                            queue_events(&mut tick_events, LANE_SYS, evs, now);
                            if let Some(s) = sys_open_since.take() {
                                sys_intervals.push((s, now));
                            }
                        }
                    }
                }
                if let Some(failed) = &sys.failed {
                    if failed.load(Ordering::Relaxed) && sh.sys_status == SysStatus::Ok {
                        sh.sys_status =
                            SysStatus::Error("system tap failed after device change".into());
                    }
                }
                sh.lanes[LANE_SYS].dropped = sys.dropped.load(Ordering::Relaxed);
            }
            if let Some(mic) = &ins.mic {
                sh.lanes[LANE_MIC].dropped = mic.dropped.load(Ordering::Relaxed);
            }
        }

        if running {
            apply_events(&mut sh, &mut tracker, tick_events);
        }
    }
}

/// Per-lane segmenter config (mic vs system).
fn seg_cfg(cfg: &crate::config::Config, lane: usize) -> &crate::config::SegmenterConfig {
    if lane == LANE_MIC {
        &cfg.mic_segmenter
    } else {
        &cfg.sys_segmenter
    }
}

/// Stamp segmentation events with their event time (Cancel carries none, so
/// it gets `now_ns`) and queue them for `apply_events`.
fn queue_events(
    out: &mut Vec<(usize, u64, SegEvent)>,
    lane: usize,
    evs: Vec<SegEvent>,
    now_ns: u64,
) {
    for e in evs {
        let t = match e {
            SegEvent::Open { t_ns } | SegEvent::Close { t_ns } => t_ns,
            SegEvent::Cancel => now_ns,
        };
        out.push((lane, t, e));
    }
}

/// Apply VAD speech events to the mic lane's visual speech intervals.
fn apply_speech_events(lane: &mut crate::session::LaneState, events: Vec<vad::VadEvent>) {
    for e in events {
        match e {
            vad::VadEvent::Open { t_ns } => lane.speech_open_ns = Some(t_ns),
            vad::VadEvent::Close { t_ns } => {
                if let Some(start_ns) = lane.speech_open_ns.take() {
                    lane.speech.push(Block {
                        start_ns,
                        end_ns: t_ns,
                    });
                }
            }
            vad::VadEvent::Cancel => lane.speech_open_ns = None,
        }
    }
}

/// Apply segmentation events (sorted by time across lanes) to the shared
/// block lists and the turn tracker.
fn apply_events(
    sh: &mut crate::session::Shared,
    tracker: &mut TurnTracker,
    mut events: Vec<(usize, u64, SegEvent)>,
) {
    if events.is_empty() {
        return;
    }
    events.sort_by_key(|(_, t, _)| *t);
    let merge_gap = sh.cfg.merge_gap_ms;
    for (lane, _t, ev) in events {
        match ev {
            SegEvent::Open { t_ns } => {
                sh.lanes[lane].open_start_ns = Some(t_ns);
            }
            SegEvent::Close { t_ns } => {
                if let Some(start) = sh.lanes[lane].open_start_ns.take() {
                    sh.lanes[lane].blocks.push(Block {
                        start_ns: start,
                        end_ns: t_ns,
                    });
                }
            }
            SegEvent::Cancel => {
                sh.lanes[lane].open_start_ns = None;
            }
        }
        tracker.on_event(lane, ev, merge_gap, &mut sh.turns, &mut sh.interruptions);
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::audio::{LaneInputs, RingSet};
    use crate::config::Config;
    use crate::session::{self, Phase};
    use std::time::Duration;

    const SR: f64 = 16_000.0;

    /// Build a fake capture stream: mono samples + sync events mapping sample
    /// counts onto a virtual timeline where sample n is at n/SR seconds.
    fn make_ring(signal: &[f32]) -> RingSet {
        let (mut sp, sc) = rtrb::RingBuffer::<f32>::new(1 << 20);
        let (mut yp, yc) = rtrb::RingBuffer::<SyncEvent>::new(4096);
        let chunk = (SR * 0.1) as usize; // 100ms buffers
        let mut n: u64 = 0;
        for c in signal.chunks(chunk) {
            let _ = yp.push(SyncEvent {
                samples_before: n,
                first_frame_ns: (n as f64 / SR * 1e9) as u64,
                sample_rate: SR,
            });
            for &s in c {
                sp.push(s).unwrap();
            }
            n += c.len() as u64;
        }
        RingSet {
            samples: sc,
            syncs: yc,
            gen: crate::audio::next_gen(),
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            device_changed: None,
            failed: None,
        }
    }

    fn place_burst(signal: &mut [f32], start_ms: u64, end_ms: u64) {
        let a = (start_ms as f64 / 1000.0 * SR) as usize;
        let b = (end_ms as f64 / 1000.0 * SR) as usize;
        for (k, s) in signal[a..b].iter_mut().enumerate() {
            *s = 0.5 * (k as f32 * 0.35).sin();
        }
    }

    /// Full engine test: synthetic two-lane conversation with known gaps runs
    /// through the real analysis thread; measured latencies must match.
    #[test]
    fn synthetic_conversation_measures_correct_latencies() {
        let total = (SR * 11.0) as usize;
        let mut mic = vec![0.0f32; total];
        let mut sys = vec![0.0f32; total];
        // turn 1: user 500–2500ms, bot 3300–5000ms  → latency 800ms
        place_burst(&mut mic, 500, 2500);
        place_burst(&mut sys, 3300, 5000);
        // turn 2: user 6000–7000ms, bot 7600–9000ms → latency 600ms
        place_burst(&mut mic, 6000, 7000);
        place_burst(&mut sys, 7600, 9000);

        let shared = session::new_shared(Config::default());
        {
            let mut sh = shared.lock();
            sh.phase = Phase::Running;
            sh.session_start_ns = 0;
        }
        let inputs: crate::audio::SharedInputs =
            std::sync::Arc::new(parking_lot::Mutex::new(LaneInputs {
                mic: Some(make_ring(&mic)),
                sys: Some(make_ring(&sys)),
            }));

        let handle = spawn(shared.clone(), inputs);
        // Analysis drains everything within a few 10ms ticks.
        std::thread::sleep(Duration::from_millis(300));
        shared.lock().phase = Phase::Stopped;
        std::thread::sleep(Duration::from_millis(100));
        drop(handle);

        let sh = shared.lock();
        assert_eq!(
            sh.lanes[LANE_MIC].blocks.len(),
            2,
            "mic blocks: {:?}",
            sh.lanes[LANE_MIC].blocks
        );
        assert_eq!(sh.lanes[LANE_SYS].blocks.len(), 2);
        assert_eq!(sh.turns.len(), 2, "turns: {:#?}", sh.turns);

        let l0 = sh.turns[0].latency_ms.expect("turn 0 latency");
        let l1 = sh.turns[1].latency_ms.expect("turn 1 latency");
        assert!((l0 - 800.0).abs() < 25.0, "turn 0 latency {l0}");
        assert!((l1 - 600.0).abs() < 25.0, "turn 1 latency {l1}");
        assert!(!sh.turns[0].flags.any());
        assert!(!sh.turns[1].flags.any());
        // user response time for turn 2: bot ended 5000, user started 6000
        let ur = sh.turns[1].user_response_ms.expect("user response");
        assert!((ur - 1000.0).abs() < 25.0, "user response {ur}");
        // waveform bins were built for both lanes
        assert!(!sh.lanes[LANE_MIC].bins.level(0).is_empty());
        assert!(!sh.lanes[LANE_SYS].bins.level(0).is_empty());
    }

    /// Continuous background rumble loud enough to cross the energy
    /// threshold must NOT draw blocks when the speech-band filter is on,
    /// while real speech on top of it still segments with correct edges.
    #[test]
    fn speech_band_filter_rejects_background_rumble() {
        let total = (SR * 7.0) as usize;
        // 60 Hz rumble at -29 dBFS RMS — well above the -45 dB threshold.
        let mut mic: Vec<f32> = (0..total)
            .map(|i| 0.05 * (2.0 * std::f32::consts::PI * 60.0 * i as f32 / SR as f32).sin())
            .collect();
        let mut sys = vec![0.0f32; total];
        // Speech (in-band) rides on top of the rumble.
        let a = (0.5 * SR) as usize;
        let b = (1.5 * SR) as usize;
        for (k, s) in mic[a..b].iter_mut().enumerate() {
            *s += 0.5 * (k as f32 * 0.35).sin();
        }
        place_burst(&mut sys, 2200, 4000);

        let shared = session::new_shared(Config::default()); // speech_band on by default
        {
            let mut sh = shared.lock();
            sh.phase = Phase::Running;
            sh.session_start_ns = 0;
        }
        let inputs: crate::audio::SharedInputs =
            std::sync::Arc::new(parking_lot::Mutex::new(LaneInputs {
                mic: Some(make_ring(&mic)),
                sys: Some(make_ring(&sys)),
            }));
        let handle = spawn(shared.clone(), inputs);
        std::thread::sleep(Duration::from_millis(300));
        shared.lock().phase = Phase::Stopped;
        std::thread::sleep(Duration::from_millis(100));
        drop(handle);

        let sh = shared.lock();
        assert_eq!(
            sh.lanes[LANE_MIC].blocks.len(),
            1,
            "rumble must not create blocks; got {:?}",
            sh.lanes[LANE_MIC].blocks
        );
        let blk = sh.lanes[LANE_MIC].blocks[0];
        let start_ms = blk.start_ns as f64 / 1e6;
        let end_ms = blk.end_ns as f64 / 1e6;
        assert!((start_ms - 500.0).abs() < 25.0, "block start {start_ms}");
        assert!((end_ms - 1500.0).abs() < 25.0, "block end {end_ms}");
        assert_eq!(sh.turns.len(), 1);
        let l = sh.turns[0].latency_ms.expect("latency");
        assert!((l - 700.0).abs() < 25.0, "latency {l}");
    }

    /// The echo gate must suppress mic blocks that overlap bot audio.
    #[test]
    fn echo_gate_suppresses_mic_during_bot_audio() {
        let total = (SR * 8.0) as usize;
        let mut mic = vec![0.0f32; total];
        let mut sys = vec![0.0f32; total];
        place_burst(&mut mic, 500, 1500); // real user speech
        place_burst(&mut sys, 2200, 4500); // bot reply
        place_burst(&mut mic, 2300, 4400); // echo of bot into the mic

        let mut cfg = Config::default();
        cfg.echo_gate.enabled = true;
        cfg.echo_gate.tail_ms = 300;

        let shared = session::new_shared(cfg);
        {
            let mut sh = shared.lock();
            sh.phase = Phase::Running;
            sh.session_start_ns = 0;
        }
        let inputs: crate::audio::SharedInputs =
            std::sync::Arc::new(parking_lot::Mutex::new(LaneInputs {
                mic: Some(make_ring(&mic)),
                sys: Some(make_ring(&sys)),
            }));
        let handle = spawn(shared.clone(), inputs);
        std::thread::sleep(Duration::from_millis(300));
        shared.lock().phase = Phase::Stopped;
        std::thread::sleep(Duration::from_millis(100));
        drop(handle);

        let sh = shared.lock();
        assert_eq!(
            sh.lanes[LANE_MIC].blocks.len(),
            1,
            "echo should be gated out; mic blocks: {:?}",
            sh.lanes[LANE_MIC].blocks
        );
        assert_eq!(sh.turns.len(), 1);
        let l = sh.turns[0].latency_ms.expect("latency");
        assert!((l - 700.0).abs() < 25.0, "latency {l}");
    }
}
