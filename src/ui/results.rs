//! Results panel shown after Stop: distribution stats, histogram, per-turn
//! table (click a row to zoom the timeline), export buttons.

use super::{fmt_clock, fmt_opt_ms, latency_color, WARN_AMBER};
use crate::analysis::{stats, turns};
use crate::session::Shared;
use egui::{pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Stroke};

#[derive(Default)]
pub struct ResultsOutput {
    pub zoom_to_turn: Option<usize>,
    pub export_json: bool,
    pub export_csv: bool,
    pub open_folder: bool,
    pub new_session: bool,
}

pub fn show(ui: &mut egui::Ui, sh: &Shared, now_ns: u64) -> ResultsOutput {
    let mut out = ResultsOutput::default();

    ui.heading("Session results");
    let interruptions = if sh.interruptions.is_empty() {
        String::new()
    } else {
        format!(" · {} interruption(s)", sh.interruptions.len())
    };
    ui.label(
        RichText::new(format!(
            "{} · {} turns{}",
            fmt_clock(sh.session_duration_ms(now_ns), false),
            sh.turns.len(),
            interruptions
        ))
        .weak(),
    );
    ui.add_space(8.0);

    let latencies = sh.valid_latencies();
    let s = stats::summarize(&latencies);
    let rig = sh.devices.rig_latency_ms();

    if latencies.is_empty() {
        ui.label("No complete turns were measured.");
    } else {
        // Headline p50.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{:.0} ms", s.p50))
                    .size(38.0)
                    .strong()
                    .color(latency_color(s.p50)),
            );
            ui.vertical(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("median response latency").weak());
                if let Some(r) = rig {
                    ui.label(
                        RichText::new(format!("≈{:.0} ms perceived (+{r:.0} rig)", s.p50 + r))
                            .size(11.0)
                            .weak(),
                    );
                }
            });
        });
        ui.add_space(6.0);

        egui::Grid::new("results-stats")
            .num_columns(4)
            .spacing([14.0, 3.0])
            .show(ui, |ui| {
                let cell = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).weak().size(11.0));
                    ui.label(RichText::new(v).monospace().size(12.0));
                };
                cell(ui, "n", format!("{}", s.count));
                cell(ui, "mean", format!("{:.0} ms", s.mean));
                ui.end_row();
                cell(ui, "min", format!("{:.0} ms", s.min));
                cell(ui, "max", format!("{:.0} ms", s.max));
                ui.end_row();
                cell(ui, "p90", format!("{:.0} ms", s.p90));
                cell(ui, "p95", format!("{:.0} ms", s.p95));
                ui.end_row();
                cell(ui, "p99", format!("{:.0} ms", s.p99));
                cell(ui, "σ", format!("{:.0} ms", s.stdev));
                ui.end_row();
            });

        ui.add_space(8.0);
        draw_histogram(ui, &latencies);
    }

    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Export JSON").clicked() {
            out.export_json = true;
        }
        if ui.button("Export CSV").clicked() {
            out.export_csv = true;
        }
        if ui.button("Open folder").clicked() {
            out.open_folder = true;
        }
        if ui.button("New session").clicked() {
            out.new_session = true;
        }
    });

    if !sh.events.is_empty() {
        ui.add_space(10.0);
        ui.separator();
        super::event_list(ui, sh, 150.0);
        ui.add_space(8.0);
        super::event_timing(ui, sh, 200.0);
    }

    ui.add_space(10.0);
    ui.separator();
    ui.label(RichText::new("Turns").strong());
    let has_rtvi = !sh.events.is_empty();
    if !has_rtvi {
        ui.label(
            RichText::new("RTVI columns: N/A (no bridge events received)")
                .size(10.5)
                .weak(),
        );
    }
    ui.add_space(4.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("turns-table")
                .num_columns(7)
                .striped(true)
                .spacing([10.0, 3.0])
                .show(ui, |ui| {
                    for h in ["#", "user end", "latency", "perceived", "Δvad", "Δplayout", "flags"] {
                        ui.label(RichText::new(h).weak().size(10.5));
                    }
                    ui.end_row();

                    for t in &sh.turns {
                        let deltas = turns::rtvi_deltas(t, &sh.events, sh.cfg.rtvi_offset_ms);
                        let clicked = ui
                            .selectable_label(false, RichText::new(format!("{}", t.index + 1)).monospace())
                            .clicked();
                        ui.label(
                            RichText::new(
                                t.user_end_ns
                                    .map(|v| fmt_clock(sh.rel_ms(v), true))
                                    .unwrap_or_else(|| "—".into()),
                            )
                            .monospace()
                            .size(11.5),
                        );
                        match t.latency_ms {
                            Some(l) => {
                                ui.label(
                                    RichText::new(format!("{l:.0} ms"))
                                        .monospace()
                                        .color(latency_color(l)),
                                );
                            }
                            None => {
                                ui.label(RichText::new("—").weak());
                            }
                        }
                        ui.label(
                            RichText::new(fmt_opt_ms(
                                t.latency_ms.and_then(|l| rig.map(|r| l + r)),
                            ))
                            .monospace()
                            .size(11.5),
                        );
                        ui.label(
                            RichText::new(fmt_opt_ms(deltas.vad_stop_delta_ms))
                                .monospace()
                                .size(11.5),
                        );
                        ui.label(
                            RichText::new(fmt_opt_ms(deltas.bot_start_delta_ms))
                                .monospace()
                                .size(11.5),
                        );
                        let flags = t.flags.summary();
                        if flags.is_empty() {
                            ui.label("");
                        } else {
                            ui.label(RichText::new(flags).size(10.5).color(WARN_AMBER));
                        }
                        ui.end_row();
                        if clicked {
                            out.zoom_to_turn = Some(t.index);
                        }
                    }
                });
        });

    out
}

fn draw_histogram(ui: &mut egui::Ui, values: &[f64]) {
    let bins = (values.len() / 2).clamp(6, 24);
    let (edges, counts) = stats::histogram(values, bins);
    if counts.is_empty() {
        return;
    }
    let max_count = *counts.iter().max().unwrap_or(&1) as f32;
    let width = ui.available_width().min(360.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 96.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let dark = ui.visuals().dark_mode;
    painter.rect_filled(
        rect,
        4.0,
        if dark {
            Color32::from_gray(32)
        } else {
            Color32::from_gray(240)
        },
    );
    let plot = rect.shrink2(vec2(6.0, 14.0));
    let bar_w = plot.width() / counts.len() as f32;
    let mut hover: Option<(usize, Rect)> = None;
    for (i, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let h = (c as f32 / max_count) * plot.height();
        let center_val = (edges[i] + edges[i + 1]) / 2.0;
        let bar = Rect::from_min_max(
            pos2(plot.left() + i as f32 * bar_w + 1.0, plot.bottom() - h),
            pos2(plot.left() + (i + 1) as f32 * bar_w - 1.0, plot.bottom()),
        );
        painter.rect_filled(bar, 2.0, latency_color(center_val).gamma_multiply(0.9));
        if let Some(p) = resp.hover_pos() {
            if p.x >= bar.left() && p.x <= bar.right() {
                hover = Some((i, bar));
            }
        }
    }
    let text = if dark {
        Color32::from_gray(160)
    } else {
        Color32::from_gray(100)
    };
    painter.text(
        pos2(rect.left() + 6.0, rect.bottom() - 2.0),
        Align2::LEFT_BOTTOM,
        format!("{:.0} ms", edges.first().copied().unwrap_or(0.0)),
        FontId::monospace(9.0),
        text,
    );
    painter.text(
        pos2(rect.right() - 6.0, rect.bottom() - 2.0),
        Align2::RIGHT_BOTTOM,
        format!("{:.0} ms", edges.last().copied().unwrap_or(0.0)),
        FontId::monospace(9.0),
        text,
    );
    if let Some((i, bar)) = hover {
        painter.rect_stroke(
            bar,
            2.0,
            Stroke::new(1.0, if dark { Color32::WHITE } else { Color32::BLACK }),
            egui::StrokeKind::Outside,
        );
        painter.text(
            pos2(rect.center().x, rect.top() + 2.0),
            Align2::CENTER_TOP,
            format!(
                "{} turn(s) in {:.0}–{:.0} ms",
                counts[i],
                edges[i],
                edges[i + 1]
            ),
            FontId::monospace(9.5),
            text,
        );
    }
}
