//! Right panel while idle/running: live latency numbers, bridge status,
//! device latency info. RTVI fields show N/A when no bridge client has sent
//! events — the tool is fully functional without them.

use super::{latency_color, lerp_color, ERR_RED, WARN_AMBER};
use crate::analysis::stats;
use crate::audio::{DeviceInfo, LANE_MIC, LANE_SYS};
use crate::session::{Phase, Shared};
use egui::{Color32, RichText};

pub fn show(ui: &mut egui::Ui, sh: &Shared, now_ns: u64, turn_pulse: f32) {
    ui.heading("Live");
    ui.add_space(6.0);

    // Headline: latest turn latency. Pulses when a new turn lands.
    let last = sh.turns.iter().rev().find_map(|t| t.latency_ms);
    match last {
        Some(l) => {
            let base = latency_color(l);
            let flash_to = if ui.visuals().dark_mode {
                Color32::WHITE
            } else {
                Color32::BLACK
            };
            ui.label(
                RichText::new(format!("{l:.0} ms"))
                    .size(44.0 + 8.0 * turn_pulse)
                    .strong()
                    .color(lerp_color(base, flash_to, turn_pulse * 0.55)),
            );
            ui.label(RichText::new("last response latency (user stop → bot audio)").weak());
        }
        None => {
            ui.label(RichText::new("—").size(44.0).weak());
            ui.label(RichText::new("waiting for the first turn…").weak());
        }
    }
    ui.add_space(10.0);

    let latencies = sh.valid_latencies();
    let s = stats::summarize(&latencies);
    egui::Grid::new("live-stats")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            ui.label("Turns");
            ui.label(RichText::new(format!("{}", s.count)).monospace());
            ui.end_row();
            ui.label("Mean");
            ui.label(RichText::new(fmt_stat(s.mean, s.count)).monospace());
            ui.end_row();
            ui.label("P50");
            ui.label(RichText::new(fmt_stat(s.p50, s.count)).monospace());
            ui.end_row();
            ui.label("P95");
            ui.label(RichText::new(fmt_stat(s.p95, s.count)).monospace());
            ui.end_row();
            ui.label("Mic blocks");
            ui.label(
                RichText::new(format!("{}", sh.lanes[LANE_MIC].blocks.len())).monospace(),
            );
            ui.end_row();
            ui.label("Bot blocks");
            ui.label(
                RichText::new(format!("{}", sh.lanes[LANE_SYS].blocks.len())).monospace(),
            );
            ui.end_row();
            ui.label("Interruptions");
            let last_stop = sh.interruptions.iter().rev().find_map(|i| i.stop_ms);
            let txt = match last_stop {
                Some(s) => format!("{} · last stop {s:.0} ms", sh.interruptions.len()),
                None => format!("{}", sh.interruptions.len()),
            };
            ui.label(RichText::new(txt).monospace());
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.separator();

    // ---- RTVI bridge ----
    ui.label(RichText::new("RTVI bridge").strong());
    ui.add_space(2.0);
    if !sh.cfg.bridge.enabled {
        ui.label(RichText::new("disabled").weak());
    } else if !sh.bridge.running {
        ui.colored_label(
            ERR_RED,
            sh.bridge.error.as_deref().unwrap_or("not running"),
        );
    } else {
        ui.label(format!(
            "ws://localhost:{} — {} client{}",
            sh.bridge.port,
            sh.bridge.clients,
            if sh.bridge.clients == 1 { "" } else { "s" }
        ));
        match &sh.bridge.last_event {
            Some((name, t)) => {
                let ago = (now_ns.saturating_sub(*t)) as f64 / 1e9;
                ui.label(
                    RichText::new(format!("last: {name} ({ago:.0}s ago)"))
                        .monospace()
                        .size(11.0),
                );
            }
            None => {
                ui.label(RichText::new("events: N/A (nothing received)").weak());
            }
        }
        if sh.phase == Phase::Running {
            ui.label(format!("{} events this session", sh.events.len()));
        }
    }

    if !sh.events.is_empty() {
        ui.add_space(8.0);
        super::event_list(ui, sh, 150.0);
    }

    ui.add_space(12.0);
    ui.separator();

    // ---- Devices / latency honesty ----
    ui.label(RichText::new("Device latency").strong());
    ui.add_space(2.0);
    device_row(ui, "In", &sh.devices.input);
    device_row(ui, "Out", &sh.devices.output);
    if let Some(rig) = sh.devices.rig_latency_ms() {
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "Raw gaps understate perceived latency by ≈{rig:.0} ms (ADC+DAC path)"
            ))
            .size(11.0)
            .weak(),
        );
    }
    let bt = sh
        .devices
        .input
        .as_ref()
        .map(|d| d.is_bluetooth)
        .unwrap_or(false)
        || sh
            .devices
            .output
            .as_ref()
            .map(|d| d.is_bluetooth)
            .unwrap_or(false);
    if bt {
        ui.add_space(4.0);
        ui.colored_label(
            WARN_AMBER,
            "⚠ Bluetooth audio device: adds latency the framework never sees. \
             Prefer wired devices for benchmarking.",
        );
    }

    if sh.phase == Phase::Running {
        ui.add_space(12.0);
        let flagged = sh.turns.iter().filter(|t| t.flags.any()).count();
        if flagged > 0 {
            ui.label(
                RichText::new(format!("{flagged} turn(s) flagged (barge-in / overlap)"))
                    .size(11.0)
                    .weak(),
            );
        }
    }
}

fn fmt_stat(v: f64, n: usize) -> String {
    if n == 0 {
        "N/A".into()
    } else {
        format!("{v:.0} ms")
    }
}

fn device_row(ui: &mut egui::Ui, label: &str, dev: &Option<DeviceInfo>) {
    match dev {
        Some(d) => {
            let bt = if d.is_bluetooth { " ⚠" } else { "" };
            ui.label(
                RichText::new(format!(
                    "{label}: {} · {} · {:.1} ms{bt}",
                    truncate(&d.name, 26),
                    d.transport,
                    d.total_latency_ms
                ))
                .size(11.5),
            )
            .on_hover_text(format!(
                "{}\ndevice {:.2} ms + safety {:.2} ms + buffer {:.2} ms + stream {:.2} ms\n@ {:.0} Hz",
                d.name,
                d.device_latency_ms,
                d.safety_offset_ms,
                d.buffer_ms,
                d.stream_latency_ms,
                d.sample_rate
            ));
        }
        None => {
            ui.label(RichText::new(format!("{label}: N/A")).size(11.5).weak());
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}
