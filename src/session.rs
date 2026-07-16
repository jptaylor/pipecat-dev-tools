//! Shared state between the audio/analysis threads, the bridge server, and
//! the UI. One mutex, short critical sections.

use crate::analysis::bins::LaneBins;
use crate::analysis::turns::{InterruptionRecord, TurnRecord};
use crate::audio::DeviceInfo;
use crate::config::Config;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy)]
pub struct Block {
    pub start_ns: u64,
    pub end_ns: u64,
}

/// An event received over the bridge WebSocket, stamped on arrival.
#[derive(Debug, Clone)]
pub struct BridgeEvent {
    pub t_ns: u64,
    pub name: String,
    pub source: String,
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct BridgeStatus {
    pub running: bool,
    pub port: u16,
    pub clients: usize,
    pub last_event: Option<(String, u64)>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SysStatus {
    Ok,
    Unavailable(String),
    Error(String),
}

impl Default for SysStatus {
    fn default() -> Self {
        SysStatus::Unavailable("not started".into())
    }
}

#[derive(Default)]
pub struct LaneState {
    pub bins: LaneBins,
    pub blocks: Vec<Block>,
    pub open_start_ns: Option<u64>,
    /// VAD-classified speech intervals (mic lane only). Purely visual: the
    /// timeline tints these green and washes out the rest of a block; block
    /// edges, turns, and all metrics come from `blocks` alone.
    pub speech: Vec<Block>,
    pub speech_open_ns: Option<u64>,
    pub level_db: f32,
    pub effective_threshold_db: f32,
    pub dropped: u64,
    /// ns of the most recent processed sample (lane "now").
    pub last_sample_ns: u64,
}

impl LaneState {
    pub fn reset_session(&mut self) {
        self.bins.clear();
        self.blocks.clear();
        self.open_start_ns = None;
        self.speech.clear();
        self.speech_open_ns = None;
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceSnapshot {
    pub input: Option<DeviceInfo>,
    pub output: Option<DeviceInfo>,
}

impl DeviceSnapshot {
    /// Sum of reported input+output latency: the amount by which the raw
    /// measured gap understates what the human perceives.
    pub fn rig_latency_ms(&self) -> Option<f64> {
        match (&self.input, &self.output) {
            (Some(i), Some(o)) => Some(i.total_latency_ms + o.total_latency_ms),
            _ => None,
        }
    }
}

pub struct Shared {
    pub phase: Phase,
    pub session_start_ns: u64,
    pub session_end_ns: u64,
    pub lanes: [LaneState; 2],
    pub turns: Vec<TurnRecord>,
    /// Barge-ins: mic audio over bot audio, and when the bot actually stopped.
    pub interruptions: Vec<InterruptionRecord>,
    pub events: Vec<BridgeEvent>,
    pub discontinuities: Vec<u64>,
    pub bridge: BridgeStatus,
    pub cfg: Config,
    pub sys_status: SysStatus,
    pub mic_status: Option<String>, // error message if mic failed
    pub devices: DeviceSnapshot,
    pub session_dir: Option<PathBuf>,
    pub wav_error: Option<String>,
    /// Input devices, enumerated by the audio-control thread.
    pub available_devices: Vec<String>,
    /// True while the audio-control thread is (re)starting streams —
    /// possibly blocked on a permission prompt.
    pub audio_busy: bool,
    /// Set if the VAD model failed to load/run (tint falls back to plain).
    pub vad_error: Option<String>,
}

impl Shared {
    pub fn new(cfg: Config) -> Self {
        Self {
            phase: Phase::Idle,
            session_start_ns: 0,
            session_end_ns: 0,
            lanes: [LaneState::default(), LaneState::default()],
            turns: Vec::new(),
            interruptions: Vec::new(),
            events: Vec::new(),
            discontinuities: Vec::new(),
            bridge: BridgeStatus::default(),
            cfg,
            sys_status: SysStatus::default(),
            mic_status: None,
            devices: DeviceSnapshot::default(),
            session_dir: None,
            wav_error: None,
            available_devices: Vec::new(),
            audio_busy: false,
            vad_error: None,
        }
    }

    /// Wipe all data from the previous session (timeline, turns, events).
    pub fn clear_session_data(&mut self) {
        self.session_start_ns = 0;
        self.session_end_ns = 0;
        self.turns.clear();
        self.interruptions.clear();
        self.events.clear();
        self.discontinuities.clear();
        self.wav_error = None;
        for lane in &mut self.lanes {
            lane.reset_session();
        }
    }

    /// Session-relative ms for a timestamp.
    pub fn rel_ms(&self, t_ns: u64) -> f64 {
        t_ns.saturating_sub(self.session_start_ns) as f64 / 1e6
    }

    pub fn session_duration_ms(&self, now_ns: u64) -> f64 {
        let end = match self.phase {
            Phase::Running => now_ns,
            _ => self.session_end_ns,
        };
        end.saturating_sub(self.session_start_ns) as f64 / 1e6
    }

    /// Latencies of valid, confirmed turns (for stats).
    pub fn valid_latencies(&self) -> Vec<f64> {
        self.turns
            .iter()
            .filter(|t| !t.provisional)
            .filter_map(|t| t.latency_ms)
            .collect()
    }
}

pub type SharedState = Arc<Mutex<Shared>>;

pub fn new_shared(cfg: Config) -> SharedState {
    Arc::new(Mutex::new(Shared::new(cfg)))
}
