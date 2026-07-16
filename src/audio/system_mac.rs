//! FFI wrapper around the Objective-C Core Audio tap shim
//! (macos/shim/system_tap.m). Captures the system output mix with hardware
//! host timestamps.

use super::{next_gen, DeviceInfo, RingSet, SyncEvent};
use crate::clock;
use anyhow::{anyhow, Result};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const SAMPLE_RING_CAPACITY: usize = 1 << 19;
const SYNC_RING_CAPACITY: usize = 4096;

type DataCb = extern "C" fn(*mut c_void, *const f32, u32, u32, f64, u64);
type EventCb = extern "C" fn(*mut c_void, i32);

#[repr(C)]
struct RawDeviceInfo {
    name: [c_char; 256],
    device_latency_ms: f64,
    safety_offset_ms: f64,
    buffer_ms: f64,
    stream_latency_ms: f64,
    total_latency_ms: f64,
    sample_rate: f64,
    transport: u32,
    is_bluetooth: i32,
}

impl RawDeviceInfo {
    fn zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

extern "C" {
    fn sysaudio_start(
        data_cb: DataCb,
        event_cb: EventCb,
        ctx: *mut c_void,
        err: *mut c_char,
        err_len: i32,
    ) -> i32;
    fn sysaudio_stop();
    fn sysaudio_supported() -> i32;
    fn sysaudio_default_output_info(out: *mut RawDeviceInfo) -> i32;
    fn sysaudio_input_info_by_name(name: *const c_char, out: *mut RawDeviceInfo) -> i32;
}

struct TapCtx {
    samples: rtrb::Producer<f32>,
    syncs: rtrb::Producer<SyncEvent>,
    total_frames: u64,
    dropped: Arc<AtomicU64>,
    device_changed: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

extern "C" fn tap_data_cb(
    ctx: *mut c_void,
    interleaved: *const f32,
    frames: u32,
    channels: u32,
    sample_rate: f64,
    host_time: u64,
) {
    if ctx.is_null() || interleaved.is_null() || frames == 0 || channels == 0 {
        return;
    }
    let state = unsafe { &mut *(ctx as *mut TapCtx) };
    let data = unsafe { std::slice::from_raw_parts(interleaved, (frames * channels) as usize) };

    let _ = state.syncs.push(SyncEvent {
        samples_before: state.total_frames,
        first_frame_ns: clock::host_time_to_ns(host_time),
        sample_rate,
    });

    let ch = channels as usize;
    let mut lost = 0u64;
    for frame in data.chunks_exact(ch) {
        let mono = frame.iter().sum::<f32>() / ch as f32;
        if state.samples.push(mono).is_err() {
            lost += 1;
        }
    }
    if lost > 0 {
        state.dropped.fetch_add(lost, Ordering::Relaxed);
    }
    state.total_frames += frames as u64;
}

extern "C" fn tap_event_cb(ctx: *mut c_void, event_code: i32) {
    if ctx.is_null() {
        return;
    }
    let state = unsafe { &*(ctx as *mut TapCtx) };
    match event_code {
        1 => {
            state.device_changed.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            state.failed.store(true, Ordering::Relaxed);
        }
    }
}

/// Handle for the running system tap. Dropping stops the capture.
pub struct SysTap {
    ctx: *mut TapCtx,
    stopped: bool,
}

// The ctx pointer is only touched by the Core Audio callbacks and freed
// never (see Drop); the handle itself is safe to move across threads.
unsafe impl Send for SysTap {}

impl SysTap {
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        unsafe { sysaudio_stop() };
        // Intentionally leak ctx: an in-flight IOProc callback on the Core
        // Audio queue could still be touching it for a few microseconds after
        // sysaudio_stop returns. Taps are started at most a handful of times
        // per app run, so the leak is bounded and tiny.
        self.ctx = std::ptr::null_mut();
    }
}

impl Drop for SysTap {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn supported() -> bool {
    unsafe { sysaudio_supported() != 0 }
}

pub fn start() -> Result<(SysTap, RingSet)> {
    if !supported() {
        return Err(anyhow!("system-audio tap requires macOS 14.4 or later"));
    }
    let (sample_prod, sample_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RING_CAPACITY);
    let (sync_prod, sync_cons) = rtrb::RingBuffer::<SyncEvent>::new(SYNC_RING_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let device_changed = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicBool::new(false));

    let ctx = Box::into_raw(Box::new(TapCtx {
        samples: sample_prod,
        syncs: sync_prod,
        total_frames: 0,
        dropped: dropped.clone(),
        device_changed: device_changed.clone(),
        failed: failed.clone(),
    }));

    let mut err = [0i8; 512];
    let rc = unsafe {
        sysaudio_start(
            tap_data_cb,
            tap_event_cb,
            ctx as *mut c_void,
            err.as_mut_ptr() as *mut c_char,
            err.len() as i32,
        )
    };
    if rc != 0 {
        // Callbacks never ran; safe to reclaim.
        drop(unsafe { Box::from_raw(ctx) });
        let msg = unsafe { CStr::from_ptr(err.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned();
        return Err(anyhow!("{msg} (code {rc})"));
    }

    Ok((
        SysTap {
            ctx,
            stopped: false,
        },
        RingSet {
            samples: sample_cons,
            syncs: sync_cons,
            gen: next_gen(),
            dropped,
            device_changed: Some(device_changed),
            failed: Some(failed),
        },
    ))
}

fn transport_name(fourcc: u32) -> String {
    let s = match fourcc {
        0x626C_746E => "Built-in",     // 'bltn'
        0x7573_6220 => "USB",          // 'usb '
        0x626C_7565 => "Bluetooth",    // 'blue'
        0x626C_6561 => "Bluetooth LE", // 'blea'
        0x6864_6D69 => "HDMI",         // 'hdmi'
        0x6470_7274 => "DisplayPort",  // 'dprt'
        0x6169_7270 => "AirPlay",      // 'airp'
        0x7669_7274 => "Virtual",      // 'virt'
        0x6772_7570 => "Aggregate",    // 'grup'
        0x7468_756E => "Thunderbolt",  // 'thun'
        0x3133_3934 => "FireWire",     // '1394'
        0x7063_6920 => "PCI",          // 'pci '
        0x6561_7662 => "AVB",          // 'eavb'
        0 => "Unknown",
        _ => {
            let b = fourcc.to_be_bytes();
            return String::from_utf8_lossy(&b).trim().to_string();
        }
    };
    s.to_string()
}

fn convert_info(raw: &RawDeviceInfo) -> DeviceInfo {
    let name = unsafe { CStr::from_ptr(raw.name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    DeviceInfo {
        name,
        device_latency_ms: raw.device_latency_ms,
        safety_offset_ms: raw.safety_offset_ms,
        buffer_ms: raw.buffer_ms,
        stream_latency_ms: raw.stream_latency_ms,
        total_latency_ms: raw.total_latency_ms,
        sample_rate: raw.sample_rate,
        transport: transport_name(raw.transport),
        is_bluetooth: raw.is_bluetooth != 0,
    }
}

pub fn default_output_info() -> Option<DeviceInfo> {
    let mut raw = RawDeviceInfo::zeroed();
    let rc = unsafe { sysaudio_default_output_info(&mut raw) };
    (rc == 0).then(|| convert_info(&raw))
}

pub fn input_info_by_name(name: &str) -> Option<DeviceInfo> {
    let cname = CString::new(name).ok()?;
    let mut raw = RawDeviceInfo::zeroed();
    let rc = unsafe { sysaudio_input_info_by_name(cname.as_ptr(), &mut raw) };
    (rc == 0).then(|| convert_info(&raw))
}
