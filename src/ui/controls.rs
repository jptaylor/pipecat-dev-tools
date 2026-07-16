//! Top control bar: devices, start/stop, thresholds with live meters, toggles.

use super::{ViewState, ERR_RED, OK_GREEN, WARN_AMBER};
use crate::audio::{LANE_MIC, LANE_SYS};
use crate::config::Config;
use crate::session::{Phase, Shared, SysStatus};
use egui::{pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Stroke};

#[derive(Default)]
pub struct ControlsOutput {
    pub start_clicked: bool,
    pub stop_clicked: bool,
    pub apply_devices: bool,
    pub restart_bridge: bool,
    pub refresh_devices: bool,
    pub fit_clicked: bool,
}

pub fn show(
    ui: &mut egui::Ui,
    cfg: &mut Config,
    sh: &Shared,
    devices: &[String],
    view: &mut ViewState,
    now_ns: u64,
) -> ControlsOutput {
    let mut out = ControlsOutput::default();
    let running = sh.phase == Phase::Running;

    // ---------- row 1: devices + status + start/stop ----------
    ui.horizontal(|ui| {
        ui.label(RichText::new("Pipecat Audio Metrics").strong());
        ui.separator();

        ui.label("Mic:");
        let current = cfg
            .input_device
            .clone()
            .unwrap_or_else(|| "System default".into());
        egui::ComboBox::from_id_salt("input-device")
            .width(210.0)
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(cfg.input_device.is_none(), "System default")
                    .clicked()
                {
                    cfg.input_device = None;
                    out.apply_devices = true;
                }
                for name in devices {
                    let selected = cfg.input_device.as_deref() == Some(name.as_str());
                    if ui.selectable_label(selected, name).clicked() {
                        cfg.input_device = Some(name.clone());
                        out.apply_devices = true;
                    }
                }
            });
        if ui.button("⟳").on_hover_text("Refresh device list").clicked() {
            out.refresh_devices = true;
        }

        if cfg!(target_os = "linux") {
            ui.label("System source:");
            let current = cfg
                .linux_system_device
                .clone()
                .unwrap_or_else(|| "— select monitor —".into());
            egui::ComboBox::from_id_salt("sys-device")
                .width(210.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for name in devices {
                        let selected = cfg.linux_system_device.as_deref() == Some(name.as_str());
                        if ui.selectable_label(selected, name).clicked() {
                            cfg.linux_system_device = Some(name.clone());
                            out.apply_devices = true;
                        }
                    }
                });
        } else {
            let (dot, hover) = match &sh.sys_status {
                SysStatus::Ok => (OK_GREEN, "System audio tap running".to_string()),
                SysStatus::Unavailable(e) => (Color32::from_gray(140), e.clone()),
                SysStatus::Error(e) => (ERR_RED, e.clone()),
            };
            let (r, resp) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, dot);
            resp.on_hover_text(hover);
            ui.label("System audio: Core Audio tap");
        }

        // Right-aligned: timer + start/stop.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, fill) = if running {
                ("⏹ Stop", Color32::from_rgb(178, 52, 42))
            } else {
                ("▶ Start", Color32::from_rgb(46, 125, 50))
            };
            let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE).strong())
                .fill(fill)
                .min_size(vec2(92.0, 26.0));
            if ui.add(btn).clicked() {
                if running {
                    out.stop_clicked = true;
                } else {
                    out.start_clicked = true;
                }
            }
            let dur = sh.session_duration_ms(now_ns);
            ui.label(
                RichText::new(super::fmt_clock(dur, false))
                    .monospace()
                    .size(16.0),
            );
            // Bridge status chip.
            let (chip, color) = if !cfg.bridge.enabled {
                ("bridge off".to_string(), Color32::from_gray(130))
            } else if sh.bridge.running {
                (
                    format!("bridge :{} · {}", sh.bridge.port, sh.bridge.clients),
                    if sh.bridge.clients > 0 {
                        OK_GREEN
                    } else {
                        Color32::from_gray(140)
                    },
                )
            } else {
                (
                    sh.bridge
                        .error
                        .clone()
                        .unwrap_or_else(|| "bridge error".into()),
                    ERR_RED,
                )
            };
            ui.label(RichText::new(chip).color(color).size(11.0));
        });
    });

    ui.add_space(2.0);

    // ---------- row 2: thresholds/meters + toggles + view ----------
    ui.horizontal(|ui| {
        for (lane, label) in [(LANE_MIC, "Mic"), (LANE_SYS, "Sys")] {
            ui.label(label);
            let seg = if lane == LANE_MIC {
                &cfg.mic_segmenter
            } else {
                &cfg.sys_segmenter
            };
            let thr = if seg.auto_threshold {
                sh.lanes[lane].effective_threshold_db
            } else {
                seg.threshold_db
            };
            meter(ui, sh.lanes[lane].level_db, thr, 110.0);
            let seg = if lane == LANE_MIC {
                &mut cfg.mic_segmenter
            } else {
                &mut cfg.sys_segmenter
            };
            ui.add_enabled(
                !seg.auto_threshold,
                egui::Slider::new(&mut seg.threshold_db, -80.0..=-20.0)
                    .show_value(false),
            )
            .on_hover_text(format!("threshold {:.0} dBFS", seg.threshold_db));
            ui.add_space(4.0);
        }

        let mut auto = cfg.mic_segmenter.auto_threshold;
        if ui
            .checkbox(&mut auto, "Auto")
            .on_hover_text("Threshold follows the measured noise floor + 12 dB")
            .changed()
        {
            cfg.mic_segmenter.auto_threshold = auto;
            cfg.sys_segmenter.auto_threshold = auto;
        }

        let mut band = cfg.mic_segmenter.speech_band;
        if ui
            .checkbox(&mut band, "Speech band")
            .on_hover_text(
                "Detect activity on a 300–3400 Hz speech band, so fans, hum, and hiss \
                 stop drawing blocks.\n\
                 Detection-only: the raw audio, waveform, and WAV are untouched, and the \
                 constant filter delay is < 1 ms, so metrics are unaffected.\n\
                 Meters show the band-filtered level while enabled.",
            )
            .changed()
        {
            cfg.mic_segmenter.speech_band = band;
            cfg.sys_segmenter.speech_band = band;
        }

        let vad_resp = ui.checkbox(&mut cfg.vad_tint, "VAD tint").on_hover_text(
            "Silero VAD classifies mic activity: confirmed speech renders green, other \
             mic sound washes out gray.\n\
             Purely visual — block edges, turn pairing, and every metric are computed \
             from the energy blocks and are identical with this on or off. The tint can \
             lag the live edge by ~100 ms (classification delay only).",
        );
        if let Some(err) = &sh.vad_error {
            vad_resp.on_hover_text(format!("VAD unavailable: {err}"));
            ui.colored_label(WARN_AMBER, "⚠")
                .on_hover_text(format!("VAD unavailable: {err}"));
        }

        ui.separator();
        ui.checkbox(&mut cfg.echo_gate.enabled, "Echo gate")
            .on_hover_text(
                "Ignore mic activity while bot audio is playing (+tail). \
                 Analysis-only — the audio itself is never touched. \
                 Use headphones instead when possible.",
            );
        ui.checkbox(&mut cfg.record_wav, "Record WAV")
            .on_hover_text("Write mic.wav and system.wav into the session folder");

        ui.menu_button("Events ▾", |ui| {
            ui.set_min_width(170.0);
            ui.label(
                RichText::new("Show RTVI event categories\n(timeline markers + event list)")
                    .weak()
                    .size(10.5),
            );
            ui.separator();
            for cat in crate::bridge::protocol::EventCategory::ALL {
                let mut on = cfg.event_filter.enabled(cat);
                let label = RichText::new(cat.label()).color(super::category_color(cat));
                if ui.checkbox(&mut on, label).changed() {
                    cfg.event_filter.set(cat, on);
                }
            }
        });

        ui.menu_button("Advanced ▾", |ui| {
            ui.set_min_width(280.0);
            egui::Grid::new("advanced-grid")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Hangover (block close)");
                    ui.add(
                        egui::DragValue::new(&mut cfg.mic_segmenter.hangover_ms)
                            .range(50..=2000)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                    ui.label("Min block");
                    ui.add(
                        egui::DragValue::new(&mut cfg.mic_segmenter.min_block_ms)
                            .range(20..=1000)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                    ui.label("Bot group merge gap");
                    ui.add(
                        egui::DragValue::new(&mut cfg.merge_gap_ms)
                            .range(100..=5000)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                    ui.label("Echo gate tail");
                    ui.add(
                        egui::DragValue::new(&mut cfg.echo_gate.tail_ms)
                            .range(0..=2000)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                    ui.label("Bridge enabled");
                    if ui.checkbox(&mut cfg.bridge.enabled, "").changed() {
                        out.restart_bridge = true;
                    }
                    ui.end_row();
                    ui.label("Bridge port");
                    let before = cfg.bridge.port;
                    ui.add(egui::DragValue::new(&mut cfg.bridge.port).range(1024..=65535));
                    if cfg.bridge.port != before {
                        out.restart_bridge = true;
                    }
                    ui.end_row();
                    ui.label("RTVI event offset");
                    ui.add(
                        egui::DragValue::new(&mut cfg.rtvi_offset_ms)
                            .range(-2000.0..=2000.0)
                            .speed(1.0)
                            .suffix(" ms"),
                    )
                    .on_hover_text(
                        "Added to every bridge event timestamp — markers, event \
                         timing metrics, turn deltas, and exports. Websocket \
                         events land late; a negative value pulls them back onto \
                         the audio blocks. Keep 0 to measure the raw transport \
                         delay itself. Applies retroactively to the whole session.",
                    );
                    ui.end_row();
                });
            ui.separator();
            ui.label(
                RichText::new(
                    "Sys lane uses the same hangover/min-block; thresholds are per-lane.",
                )
                .size(10.5)
                .weak(),
            );
        });
        // Keep sys segmenter timing params in lockstep (thresholds stay per-lane).
        cfg.sys_segmenter.hangover_ms = cfg.mic_segmenter.hangover_ms;
        cfg.sys_segmenter.min_block_ms = cfg.mic_segmenter.min_block_ms;

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Fit").on_hover_text("Fit whole session").clicked() {
                out.fit_clicked = true;
            }
            ui.checkbox(&mut view.follow, "Follow");
        });
    });

    out
}

/// Level meter with a threshold tick. Range −80..0 dBFS.
fn meter(ui: &mut egui::Ui, level_db: f32, threshold_db: f32, width: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let dark = ui.visuals().dark_mode;
    let bg = if dark {
        Color32::from_gray(40)
    } else {
        Color32::from_gray(225)
    };
    painter.rect_filled(rect, 3.0, bg);
    let norm = |db: f32| ((db + 80.0) / 80.0).clamp(0.0, 1.0);
    let v = norm(level_db);
    if v > 0.001 {
        let color = if level_db > -12.0 {
            ERR_RED
        } else if level_db > -30.0 {
            WARN_AMBER
        } else {
            OK_GREEN
        };
        let fill = Rect::from_min_size(rect.min, vec2(rect.width() * v, rect.height()));
        painter.rect_filled(fill, 3.0, color.gamma_multiply(0.85));
    }
    // threshold tick
    let tx = rect.left() + rect.width() * norm(threshold_db);
    painter.line_segment(
        [pos2(tx, rect.top()), pos2(tx, rect.bottom())],
        Stroke::new(1.5_f32, if dark { Color32::WHITE } else { Color32::BLACK }),
    );
    if resp.hovered() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("{level_db:.0} dB"),
            FontId::monospace(9.0),
            if dark { Color32::WHITE } else { Color32::BLACK },
        );
    }
    resp.on_hover_text(format!(
        "level {level_db:.1} dBFS · threshold {threshold_db:.1} dBFS"
    ));
}
