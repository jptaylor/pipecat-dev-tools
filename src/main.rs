#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod app;
mod audio;
mod bridge;
mod clock;
mod config;
mod export;
mod session;
mod ui;

use config::Config;

fn main() -> eframe::Result<()> {
    clock::init();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("pipecat-audio-metrics {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--list-devices") {
        list_devices();
        return Ok(());
    }
    if args.iter().any(|a| a == "--diagnose") {
        diagnose();
        return Ok(());
    }

    let cfg = Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([980.0, 620.0])
            .with_title("Pipecat Audio Metrics"),
        ..Default::default()
    };
    eframe::run_native(
        "Pipecat Audio Metrics",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, cfg)))),
    )
}

fn list_devices() {
    println!("Input devices:");
    let default = audio::capture::default_input_device_name();
    for name in audio::capture::list_input_devices() {
        let marker = if Some(&name) == default.as_ref() {
            " (default)"
        } else {
            ""
        };
        println!("  - {name}{marker}");
    }
    #[cfg(target_os = "macos")]
    {
        println!(
            "System audio tap: {}",
            if audio::system_mac::supported() {
                "supported (Core Audio process tap)"
            } else {
                "NOT supported (requires macOS 14.4+)"
            }
        );
        if let Some(out) = audio::default_output_info() {
            println!(
                "Default output: {} · {} · reported latency {:.1} ms",
                out.name, out.transport, out.total_latency_ms
            );
        }
    }
}

/// Headless capture check: runs mic + system tap for a few seconds and prints
/// levels, so permissions and the tap can be verified without the GUI.
fn diagnose() {
    use std::time::{Duration, Instant};

    println!("pipecat-audio-metrics --diagnose");
    println!("=================================");
    list_devices();
    println!();

    let mut mic_ring = match audio::capture::start(None) {
        Ok((cap, ring)) => {
            println!(
                "mic: capturing from '{}' @ {:.0} Hz ({} ch)",
                cap.device_name, cap.sample_rate, cap.channels
            );
            // Keep the stream alive for the duration of the test.
            std::mem::forget(cap);
            Some(ring)
        }
        Err(e) => {
            println!("mic: FAILED — {e}");
            None
        }
    };

    #[cfg(target_os = "macos")]
    let mut sys_ring = match audio::system_mac::start() {
        Ok((tap, ring)) => {
            println!("system: tap running");
            std::mem::forget(tap);
            Some(ring)
        }
        Err(e) => {
            println!("system: FAILED — {e}");
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    let mut sys_ring: Option<audio::RingSet> = None;

    println!();
    println!("levels for 5 seconds (speak, and play some audio):");
    let start = Instant::now();
    let mut next_print = Duration::from_millis(500);
    let drain = |ring: &mut Option<audio::RingSet>| -> Option<f32> {
        let ring = ring.as_mut()?;
        while ring.syncs.pop().is_ok() {}
        let n = ring.samples.slots();
        if n == 0 {
            return Some(-120.0);
        }
        let mut sum_sq = 0.0f64;
        let mut count = 0usize;
        if let Ok(chunk) = ring.samples.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            for s in a.iter().chain(b.iter()) {
                sum_sq += (*s as f64) * (*s as f64);
                count += 1;
            }
            chunk.commit_all();
        }
        if count == 0 {
            return Some(-120.0);
        }
        Some((10.0 * (sum_sq / count as f64 + 1e-12).log10()) as f32)
    };
    while start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
        if start.elapsed() >= next_print {
            next_print += Duration::from_millis(500);
            let mic_db = drain(&mut mic_ring);
            let sys_db = drain(&mut sys_ring);
            let f = |v: Option<f32>| {
                v.map(|db| format!("{db:6.1} dB")).unwrap_or_else(|| "   n/a  ".into())
            };
            println!(
                "  t={:4.1}s  mic {}   system {}",
                start.elapsed().as_secs_f32(),
                f(mic_db),
                f(sys_db)
            );
        }
    }
    println!();
    println!("done. If either lane shows n/a or stays at -120 dB while there is");
    println!("real audio, check Privacy & Security permissions (Microphone and");
    println!("Screen & System Audio Recording → System Audio Recording Only).");
}
