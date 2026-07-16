use crate::bridge::protocol::EventCategory;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which RTVI event categories are shown (timeline markers + event list).
/// Metrics are noisy and off by default; everything else is on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EventFilterConfig {
    pub user: bool,
    pub bot: bool,
    pub tts: bool,
    pub llm: bool,
    pub stt: bool,
    pub metrics: bool,
    pub other: bool,
}

impl Default for EventFilterConfig {
    fn default() -> Self {
        Self {
            user: true,
            bot: true,
            tts: true,
            llm: true,
            stt: true,
            metrics: false,
            other: true,
        }
    }
}

impl EventFilterConfig {
    pub fn enabled(&self, cat: EventCategory) -> bool {
        match cat {
            EventCategory::User => self.user,
            EventCategory::Bot => self.bot,
            EventCategory::Tts => self.tts,
            EventCategory::Llm => self.llm,
            EventCategory::Stt => self.stt,
            EventCategory::Metrics => self.metrics,
            EventCategory::Other => self.other,
        }
    }

    pub fn set(&mut self, cat: EventCategory, on: bool) {
        match cat {
            EventCategory::User => self.user = on,
            EventCategory::Bot => self.bot = on,
            EventCategory::Tts => self.tts = on,
            EventCategory::Llm => self.llm = on,
            EventCategory::Stt => self.stt = on,
            EventCategory::Metrics => self.metrics = on,
            EventCategory::Other => self.other = on,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EchoGateConfig {
    pub enabled: bool,
    /// Keep ignoring mic activity for this long after bot audio stops.
    pub tail_ms: u64,
}

impl Default for EchoGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tail_ms: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BridgeConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8123,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SegmenterConfig {
    /// Activity threshold in dBFS (RMS over 10 ms windows).
    pub threshold_db: f32,
    /// Auto threshold: rolling noise floor + margin.
    pub auto_threshold: bool,
    /// Silence needed to close a block (block end is backdated to last crossing).
    pub hangover_ms: u64,
    /// Blocks shorter than this are discarded (clicks, pops).
    pub min_block_ms: u64,
    /// Detect activity on a 300–3400 Hz speech band (detection path only;
    /// the captured/displayed/recorded audio is untouched). Rejects fans,
    /// hum, and hiss that would otherwise draw misleading blocks.
    pub speech_band: bool,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            auto_threshold: false,
            hangover_ms: 250,
            min_block_ms: 100,
            speech_band: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Mic input device name (None = system default).
    pub input_device: Option<String>,
    /// Linux only: input device used as the system-audio source (a monitor source).
    pub linux_system_device: Option<String>,
    pub mic_segmenter: SegmenterConfig,
    pub sys_segmenter: SegmenterConfig,
    /// Merge bot blocks separated by less than this into one bot turn group.
    pub merge_gap_ms: u64,
    pub echo_gate: EchoGateConfig,
    pub bridge: BridgeConfig,
    /// Tint VAD-classified speech green and wash out other mic activity
    /// gray on the timeline. Purely visual; metrics unchanged.
    pub vad_tint: bool,
    /// Per-category visibility of RTVI events (markers + event list).
    pub event_filter: EventFilterConfig,
    /// Manual correction added to every bridge event timestamp before
    /// display and analysis (markers, event metrics, turn deltas, exports).
    /// Websocket events land late; a negative value pulls them back onto the
    /// audio blocks. 0 keeps raw arrival times — the transport delay itself
    /// is often what's being measured.
    pub rtvi_offset_ms: f64,
    /// Record per-lane WAV files during sessions.
    pub record_wav: bool,
    /// Where session folders are created. None = ~/Documents/PipecatAudioMetrics.
    pub export_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            input_device: None,
            linux_system_device: None,
            mic_segmenter: SegmenterConfig::default(),
            sys_segmenter: SegmenterConfig {
                // System audio is digital: noise floor is essentially silence,
                // so a lower threshold catches soft TTS onsets.
                threshold_db: -50.0,
                ..SegmenterConfig::default()
            },
            merge_gap_ms: 1000,
            echo_gate: EchoGateConfig::default(),
            bridge: BridgeConfig::default(),
            vad_tint: true,
            event_filter: EventFilterConfig::default(),
            rtvi_offset_ms: 0.0,
            record_wav: false,
            export_dir: None,
        }
    }
}

impl Config {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("pipecat-audio-metrics").join("config.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, s);
        }
    }

    pub fn export_root(&self) -> PathBuf {
        self.export_dir.clone().unwrap_or_else(|| {
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("PipecatAudioMetrics")
        })
    }
}
