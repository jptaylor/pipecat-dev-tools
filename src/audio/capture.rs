//! cpal-based raw capture. Used for the microphone on all platforms and for
//! the system-audio monitor source on Linux. The stream is opened with the
//! device's default config — no resampling, no processing of any kind.

use super::{next_gen, RingSet, SyncEvent};
use crate::clock;
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const SAMPLE_RING_CAPACITY: usize = 1 << 19; // ~10s at 48 kHz
const SYNC_RING_CAPACITY: usize = 4096;

pub struct CpalCapture {
    // Kept alive for the duration of the capture; dropping stops the stream.
    _stream: cpal::Stream,
    pub sample_rate: f64,
    pub channels: u16,
    pub device_name: String,
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Ok(n) = d.name() {
                names.push(n);
            }
        }
    }
    names
}

pub fn default_input_device_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()
        .and_then(|d| d.name().ok())
}

/// Start capturing from the named input device (or the default). Returns the
/// stream handle (keep it alive; it is not Send on macOS) and the consumer
/// rings for the analysis thread.
pub fn start(device_name: Option<&str>) -> Result<(CpalCapture, RingSet)> {
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => host
            .input_devices()
            .context("enumerating input devices")?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .or_else(|| host.default_input_device())
            .ok_or_else(|| anyhow!("input device '{name}' not found and no default available"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
    };
    let resolved_name = device.name().unwrap_or_else(|_| "unknown".into());

    let config = device
        .default_input_config()
        .context("querying default input config")?;
    let sample_rate = config.sample_rate().0 as f64;
    let channels = config.channels();

    let (sample_prod, sample_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RING_CAPACITY);
    let (sync_prod, sync_cons) = rtrb::RingBuffer::<SyncEvent>::new(SYNC_RING_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(
            &device, &config.into(), sample_prod, sync_prod, dropped.clone(), sample_rate,
        )?,
        cpal::SampleFormat::I16 => build_stream::<i16>(
            &device, &config.into(), sample_prod, sync_prod, dropped.clone(), sample_rate,
        )?,
        cpal::SampleFormat::U16 => build_stream::<u16>(
            &device, &config.into(), sample_prod, sync_prod, dropped.clone(), sample_rate,
        )?,
        cpal::SampleFormat::I32 => build_stream::<i32>(
            &device, &config.into(), sample_prod, sync_prod, dropped.clone(), sample_rate,
        )?,
        cpal::SampleFormat::F64 => build_stream::<f64>(
            &device, &config.into(), sample_prod, sync_prod, dropped.clone(), sample_rate,
        )?,
        other => return Err(anyhow!("unsupported input sample format {other:?}")),
    };
    stream.play().context("starting input stream")?;

    Ok((
        CpalCapture {
            _stream: stream,
            sample_rate,
            channels,
            device_name: resolved_name,
        },
        RingSet {
            samples: sample_cons,
            syncs: sync_cons,
            gen: next_gen(),
            dropped,
            device_changed: None,
            failed: None,
        },
    ))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sample_prod: rtrb::Producer<f32>,
    mut sync_prod: rtrb::Producer<SyncEvent>,
    dropped: Arc<AtomicU64>,
    sample_rate: f64,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let mut total_frames: u64 = 0;

    let stream = device.build_input_stream(
        config,
        move |data: &[T], _info: &cpal::InputCallbackInfo| {
            let frames = if channels > 0 { data.len() / channels } else { 0 };
            if frames == 0 {
                return;
            }
            // Estimate the app-clock time of the buffer's first frame: the
            // callback fires (with some scheduling delay) once the buffer has
            // been filled, so first-frame time ≈ now − buffer duration. The
            // analysis thread's TimeMap min-filters out the scheduling delay.
            let now = clock::now_ns();
            let buf_ns = (frames as f64 / sample_rate * 1e9) as u64;
            let first_frame_ns = now.saturating_sub(buf_ns);

            let _ = sync_prod.push(SyncEvent {
                samples_before: total_frames,
                first_frame_ns,
                sample_rate,
            });

            let mut lost = 0u64;
            for frame in data.chunks_exact(channels.max(1)) {
                let mut acc = 0.0f32;
                for s in frame {
                    acc += f32::from_sample(*s);
                }
                let mono = acc / channels.max(1) as f32;
                if sample_prod.push(mono).is_err() {
                    lost += 1;
                }
            }
            if lost > 0 {
                dropped.fetch_add(lost, Ordering::Relaxed);
            }
            total_frames += frames as u64;
        },
        move |err| {
            eprintln!("[audio] input stream error: {err}");
        },
        None,
    )?;
    Ok(stream)
}
