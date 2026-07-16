pub mod capture;
pub mod control;
#[cfg(target_os = "macos")]
pub mod system_mac;

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

pub const LANE_MIC: usize = 0;
pub const LANE_SYS: usize = 1;

/// Emitted by capture callbacks once per audio buffer so the analysis thread
/// can map sample counts onto the app clock.
#[derive(Debug, Clone, Copy)]
pub struct SyncEvent {
    /// Total samples (mono frames) pushed to the ring before this buffer.
    pub samples_before: u64,
    /// Best estimate of the app-clock time of this buffer's first frame.
    /// For the macOS tap this is the hardware timestamp; for cpal it is
    /// callback time minus buffer duration.
    pub first_frame_ns: u64,
    pub sample_rate: f64,
}

/// Consumer side of one capture stream, handed to the analysis thread.
pub struct RingSet {
    pub samples: rtrb::Consumer<f32>,
    pub syncs: rtrb::Consumer<SyncEvent>,
    /// Bumped for every new stream so the analysis thread resets lane state.
    pub gen: u64,
    pub dropped: Arc<AtomicU64>,
    /// System-tap only: incremented when the output device changed and the
    /// tap was restarted (timeline discontinuity).
    pub device_changed: Option<Arc<AtomicU64>>,
    /// System-tap only: set when the tap died and could not be restarted.
    pub failed: Option<Arc<AtomicBool>>,
}

#[derive(Default)]
pub struct LaneInputs {
    pub mic: Option<RingSet>,
    pub sys: Option<RingSet>,
}

pub type SharedInputs = Arc<parking_lot::Mutex<LaneInputs>>;

pub fn next_gen() -> u64 {
    static GEN: AtomicU64 = AtomicU64::new(1);
    GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Reported latency info for a physical device (macOS only; None elsewhere).
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub device_latency_ms: f64,
    pub safety_offset_ms: f64,
    pub buffer_ms: f64,
    pub stream_latency_ms: f64,
    pub total_latency_ms: f64,
    pub sample_rate: f64,
    pub transport: String,
    pub is_bluetooth: bool,
}

#[cfg(target_os = "macos")]
pub fn default_output_info() -> Option<DeviceInfo> {
    system_mac::default_output_info()
}

#[cfg(not(target_os = "macos"))]
pub fn default_output_info() -> Option<DeviceInfo> {
    None
}

#[cfg(target_os = "macos")]
pub fn input_info_by_name(name: &str) -> Option<DeviceInfo> {
    system_mac::input_info_by_name(name)
}

#[cfg(not(target_os = "macos"))]
pub fn input_info_by_name(_name: &str) -> Option<DeviceInfo> {
    None
}
