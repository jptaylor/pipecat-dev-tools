//! The two stacked waveform timelines: activity blocks containing waveform
//! bars, latency gap annotations between lanes, RTVI event markers, ruler,
//! zoom/pan/follow.

use super::{category_color, fmt_clock, lane_palette, latency_color, Effects, ViewState, ERR_RED};
use crate::analysis::bins::LaneBins;
use crate::analysis::turns;
use crate::audio::{LANE_MIC, LANE_SYS};
use crate::bridge::protocol::categorize;
use crate::session::{Phase, Shared};
use egui::{pos2, vec2, Align2, Color32, FontId, Rect, Sense, Stroke};

const RULER_H: f32 = 22.0;
const LANE_GAP: f32 = 34.0; // room for gap annotations + interruption row
const BLOCK_PAD: f32 = 4.0;

pub fn show(ui: &mut egui::Ui, sh: &Shared, view: &mut ViewState, effects: &Effects, now_ns: u64) {
    let avail = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let dark = ui.visuals().dark_mode;
    let painter = ui.painter_at(rect);

    let now_rel_ms = sh.session_duration_ms(now_ns);

    // ---- interactions ----
    let visible_ms = (rect.width() / view.px_per_ms) as f64;
    if response.hovered() {
        let (scroll, zoom) = ui.input(|i| (i.raw_scroll_delta, i.zoom_delta()));
        let mut factor = zoom;
        if scroll.y.abs() > 0.1 {
            factor *= (1.0 + scroll.y * 0.0025).clamp(0.5, 2.0);
        }
        if (factor - 1.0).abs() > 1e-4 {
            let anchor = response
                .hover_pos()
                .map(|p| {
                    (
                        view.view_start_ms + ((p.x - rect.left()) / view.px_per_ms) as f64,
                        p.x - rect.left(),
                    )
                })
                .unwrap_or((view.view_start_ms + visible_ms / 2.0, rect.width() / 2.0));
            let new_ppm = (view.px_per_ms * factor).clamp(0.0015, 4.0);
            view.view_start_ms = anchor.0 - (anchor.1 / new_ppm) as f64;
            view.px_per_ms = new_ppm;
        }
        if scroll.x.abs() > 0.1 {
            view.view_start_ms -= (scroll.x / view.px_per_ms) as f64;
            view.follow = false;
        }
    }
    if response.dragged() {
        let d = response.drag_delta();
        if d.x.abs() > 0.0 {
            view.view_start_ms -= (d.x / view.px_per_ms) as f64;
            view.follow = false;
        }
    }
    if response.double_clicked() {
        view.follow = true;
    }

    let visible_ms = (rect.width() / view.px_per_ms) as f64;
    if view.follow && sh.phase == Phase::Running {
        view.view_start_ms = now_rel_ms - visible_ms * 0.85;
    }
    view.view_start_ms = view.view_start_ms.max(-visible_ms);
    let view_start = view.view_start_ms;
    let view_end = view_start + visible_ms;
    let ppm = view.px_per_ms;
    let x_of = |t_ms: f64| rect.left() + ((t_ms - view_start) * ppm as f64) as f32;

    // ---- layout ----
    let lanes_top = rect.top() + RULER_H;
    let lane_h = (rect.height() - RULER_H - LANE_GAP - 8.0) / 2.0;
    let lane_rects = [
        Rect::from_min_size(pos2(rect.left(), lanes_top), vec2(rect.width(), lane_h)),
        Rect::from_min_size(
            pos2(rect.left(), lanes_top + lane_h + LANE_GAP),
            vec2(rect.width(), lane_h),
        ),
    ];

    // ---- ruler ----
    draw_ruler(&painter, rect, view_start, view_end, ppm, dark);

    // ---- lanes ----
    let weak = if dark {
        Color32::from_gray(70)
    } else {
        Color32::from_gray(190)
    };
    for (lane, lane_rect) in [(LANE_MIC, lane_rects[0]), (LANE_SYS, lane_rects[1])] {
        let pal = lane_palette(dark, lane);
        // dotted baseline
        let cy = lane_rect.center().y;
        let mut x = lane_rect.left();
        while x < lane_rect.right() {
            painter.line_segment(
                [pos2(x, cy), pos2(x + 3.0, cy)],
                Stroke::new(1.0, weak),
            );
            x += 8.0;
        }
        // lane label
        painter.text(
            pos2(lane_rect.left() + 6.0, lane_rect.top() + 2.0),
            Align2::LEFT_TOP,
            if lane == LANE_MIC { "MIC" } else { "SYSTEM" },
            FontId::proportional(10.0),
            pal.label.gamma_multiply(0.9),
        );

        // blocks (closed + open)
        let lane_state = &sh.lanes[lane];
        let mut ranges: Vec<(f64, f64, bool)> = lane_state
            .blocks
            .iter()
            .map(|b| (sh.rel_ms(b.start_ns), sh.rel_ms(b.end_ns), false))
            .collect();
        if let Some(open) = lane_state.open_start_ns {
            ranges.push((sh.rel_ms(open), now_rel_ms, true));
        }

        // VAD speech intervals (mic lane, visual only): speech renders in
        // full color, the rest of a block washes out gray. Falls back to
        // plain blocks when the VAD model failed.
        let vad_active = lane == LANE_MIC && sh.cfg.vad_tint && sh.vad_error.is_none();
        let mut speech_ranges: Vec<(f64, f64)> = Vec::new();
        if vad_active {
            speech_ranges = lane_state
                .speech
                .iter()
                .map(|b| (sh.rel_ms(b.start_ns), sh.rel_ms(b.end_ns)))
                .collect();
            if let Some(open) = lane_state.speech_open_ns {
                speech_ranges.push((sh.rel_ms(open), now_rel_ms));
            }
        }
        let gray_fill = if dark {
            Color32::from_gray(55)
        } else {
            Color32::from_gray(225)
        };
        let gray_bar = if dark {
            Color32::from_gray(120)
        } else {
            Color32::from_gray(150)
        };

        for (start_ms, end_ms, is_open) in ranges {
            if end_ms < view_start || start_ms > view_end {
                continue;
            }
            let x0 = x_of(start_ms).max(rect.left() - 20.0);
            let x1 = x_of(end_ms).min(rect.right() + 20.0);
            let block_rect = Rect::from_min_max(
                pos2(x0, lane_rect.top() + BLOCK_PAD),
                pos2(x1.max(x0 + 2.0), lane_rect.bottom() - BLOCK_PAD),
            );
            let open_dim = if is_open { 0.85 } else { 1.0 };
            if vad_active {
                // Base tier: "there was mic activity" — washed out.
                painter.rect_filled(block_rect, 6.0, gray_fill.gamma_multiply(open_dim));
                draw_bins(
                    &painter, block_rect, &lane_state.bins, start_ms, end_ms, view_start,
                    view_end, ppm, x_of, gray_bar,
                );
                // Speech tier: VAD-confirmed speech in lane color.
                for &(s0, s1) in &speech_ranges {
                    let lo = s0.max(start_ms);
                    let hi = s1.min(end_ms);
                    if hi <= lo {
                        continue;
                    }
                    let sx0 = x_of(lo).max(block_rect.left());
                    let sx1 = x_of(hi).min(block_rect.right());
                    let speech_rect = Rect::from_min_max(
                        pos2(sx0, block_rect.top()),
                        pos2(sx1.max(sx0 + 1.0), block_rect.bottom()),
                    );
                    painter.rect_filled(speech_rect, 4.0, pal.fill.gamma_multiply(open_dim));
                    draw_bins(
                        &painter, speech_rect, &lane_state.bins, lo, hi, view_start,
                        view_end, ppm, x_of, pal.bar,
                    );
                }
            } else {
                painter.rect_filled(block_rect, 6.0, pal.fill.gamma_multiply(open_dim));
                draw_bins(
                    &painter, block_rect, &lane_state.bins, start_ms, end_ms, view_start,
                    view_end, ppm, x_of, pal.bar,
                );
            }
        }

        // Turn-switch flash: the lane that just became the active speaker
        // lights up and fades out.
        let flash = effects.lane_intensity(lane);
        if flash > 0.0 {
            painter.rect_filled(lane_rect, 8.0, pal.bar.gamma_multiply(flash * 0.16));
            painter.rect_stroke(
                lane_rect,
                8.0,
                Stroke::new(1.0 + 2.5 * flash, pal.bar.gamma_multiply(0.25 + flash * 0.75)),
                egui::StrokeKind::Inside,
            );
        }
    }

    let pointer = response.hover_pos();

    // ---- latency gap annotations between the lanes ----
    let gap_y = lane_rects[0].bottom() + LANE_GAP / 2.0;
    for t in &sh.turns {
        let (Some(user_end), Some(latency)) = (t.user_end_ns, t.latency_ms) else {
            continue;
        };
        let x1 = x_of(sh.rel_ms(user_end));
        let x2 = x_of(sh.rel_ms(t.bot_onset_ns));
        if x2 < rect.left() || x1 > rect.right() {
            continue;
        }
        let color = latency_color(latency);
        if x2 - x1 >= 14.0 {
            painter.line_segment(
                [pos2(x1 + 1.0, gap_y), pos2(x2 - 1.0, gap_y)],
                Stroke::new(1.2, color),
            );
            for x in [x1 + 1.0, x2 - 1.0] {
                painter.line_segment(
                    [pos2(x, gap_y - 4.0), pos2(x, gap_y + 4.0)],
                    Stroke::new(1.2, color),
                );
            }
        }
        if x2 - x1 >= 30.0 || sh.phase != Phase::Running {
            painter.text(
                pos2((x1 + x2) / 2.0, gap_y - 4.0),
                Align2::CENTER_BOTTOM,
                format!("{latency:.0} ms"),
                FontId::monospace(10.5),
                color,
            );
        }
    }

    // ---- interruption row (between the lanes) ----
    // A yellow block spans from the barging mic audio to the moment system
    // audio actually stopped; open-ended while the bot is still talking.
    let (itr_fill, itr_accent) = super::interruption_colors(dark);
    let mut itr_hover: Option<usize> = None;
    for (i, itr) in sh.interruptions.iter().enumerate() {
        let start_ms = sh.rel_ms(itr.mic_open_ns);
        let end_ms = itr
            .sys_stop_ns
            .map(|v| sh.rel_ms(v))
            .unwrap_or(now_rel_ms);
        if end_ms < view_start || start_ms > view_end {
            continue;
        }
        let x0 = x_of(start_ms).max(rect.left() - 20.0);
        let x1 = x_of(end_ms).min(rect.right() + 20.0);
        let block = Rect::from_min_max(
            pos2(x0, gap_y - 7.0),
            pos2(x1.max(x0 + 3.0), gap_y + 7.0),
        );
        let open_dim = if itr.sys_stop_ns.is_none() { 0.8 } else { 1.0 };
        painter.rect_filled(block, 3.0, itr_fill.gamma_multiply(open_dim));
        painter.rect_stroke(
            block,
            3.0,
            Stroke::new(1.0, itr_accent.gamma_multiply(0.8)),
            egui::StrokeKind::Inside,
        );
        if let Some(stop) = itr.stop_ms {
            let register = turns::interruption_register_ms(&sh.events, itr.mic_open_ns);
            let label = match register {
                Some(r) if block.width() >= 150.0 => {
                    Some(format!("reg {r:.0} · stop {stop:.0} ms"))
                }
                _ if block.width() >= 46.0 => Some(format!("{stop:.0} ms")),
                _ => None,
            };
            if let Some(label) = label {
                painter.text(
                    block.center(),
                    Align2::CENTER_CENTER,
                    label,
                    FontId::monospace(9.5),
                    itr_accent,
                );
            }
        }
        if pointer.map(|p| block.contains(p)).unwrap_or(false) {
            itr_hover = Some(i);
        }
    }

    // ---- discontinuities (output device changes) ----
    for &d in &sh.discontinuities {
        let x = x_of(sh.rel_ms(d));
        if x < rect.left() || x > rect.right() {
            continue;
        }
        let color = ERR_RED;
        draw_dashed_vline(&painter, x, lanes_top, rect.bottom() - 4.0, color, 2.0);
        painter.text(
            pos2(x, lanes_top - 1.0),
            Align2::CENTER_BOTTOM,
            "⚠",
            FontId::proportional(10.0),
            color,
        );
    }

    // ---- hover cursor: light vertical slice + timestamp chip in the ruler ----
    if let Some(p) = pointer {
        if rect.contains(p) {
            let t_ms = view_start + ((p.x - rect.left()) / ppm) as f64;
            draw_dashed_vline(&painter, p.x, lanes_top, rect.bottom() - 4.0, weak, 1.0);
            let (bg, fg) = if dark {
                (Color32::from_gray(30), Color32::from_gray(220))
            } else {
                (Color32::from_gray(250), Color32::from_gray(40))
            };
            let galley = painter.layout_no_wrap(
                fmt_clock(t_ms, true),
                FontId::monospace(9.5),
                fg,
            );
            let half_w = galley.size().x / 2.0 + 5.0;
            let cx = p.x.clamp(rect.left() + half_w, rect.right() - half_w);
            let chip = Rect::from_center_size(
                pos2(cx, rect.top() + RULER_H / 2.0 - 1.0),
                vec2(half_w * 2.0, galley.size().y + 5.0),
            );
            painter.rect_filled(chip, 3.0, bg);
            painter.rect_stroke(
                chip,
                3.0,
                Stroke::new(1.0, Color32::from_gray(if dark { 80 } else { 200 })),
                egui::StrokeKind::Inside,
            );
            let text_pos = chip.center() - galley.size() / 2.0;
            painter.galley(text_pos, galley, fg);
        }
    }

    // ---- RTVI event markers ----
    let mut hover_event: Option<(f32, usize)> = None;
    for (i, e) in sh.events.iter().enumerate() {
        if !sh.cfg.event_filter.enabled(categorize(&e.name)) {
            continue;
        }
        let x = x_of(sh.rel_ms(e.t_ns));
        if x < rect.left() || x > rect.right() {
            continue;
        }
        let color = category_color(categorize(&e.name)).gamma_multiply(0.9);
        draw_dashed_vline(&painter, x, lanes_top, rect.bottom() - 4.0, color, 1.0);
        painter.circle_filled(pos2(x, lanes_top + 3.0), 2.5, color);
        if let Some(p) = pointer {
            let dist = (p.x - x).abs();
            if dist < 5.0 && hover_event.map(|(d, _)| dist < d).unwrap_or(true) {
                hover_event = Some((dist, i));
            }
        }
    }
    if let (Some((_, idx)), Some(p)) = (hover_event, pointer) {
        let e = &sh.events[idx];
        let mut lines = vec![
            e.name.clone(),
            format!("t = {}", fmt_clock(sh.rel_ms(e.t_ns), true)),
            format!("source: {}", e.source),
        ];
        // Latency from the previous audio block edge to this event: drawn
        // like the turn gap markers, in the row between the lanes.
        if let Some((edge_ns, lane, is_end)) = prev_audio_edge(sh, e.t_ns) {
            let delta_ms = e.t_ns.saturating_sub(edge_ns) as f64 / 1e6;
            let color = category_color(categorize(&e.name));
            let x1 = x_of(sh.rel_ms(edge_ns));
            let x2 = x_of(sh.rel_ms(e.t_ns));
            if x2 - x1 >= 2.0 {
                painter.line_segment(
                    [pos2(x1 + 1.0, gap_y), pos2(x2 - 1.0, gap_y)],
                    Stroke::new(1.2, color),
                );
                for x in [x1 + 1.0, x2 - 1.0] {
                    painter.line_segment(
                        [pos2(x, gap_y - 4.0), pos2(x, gap_y + 4.0)],
                        Stroke::new(1.2, color),
                    );
                }
                painter.text(
                    pos2((x1 + x2) / 2.0, gap_y - 6.0),
                    Align2::CENTER_BOTTOM,
                    format!("{delta_ms:.0} ms"),
                    FontId::monospace(10.5),
                    color,
                );
            }
            lines.push(format!(
                "+{delta_ms:.0} ms after {} {}",
                if lane == LANE_MIC { "mic" } else { "system" },
                if is_end { "audio end" } else { "audio start" },
            ));
        }
        draw_tooltip(&painter, p, rect, dark, &lines);
    } else if let (Some(i), Some(p)) = (itr_hover, pointer) {
        let itr = &sh.interruptions[i];
        let register = turns::interruption_register_ms(&sh.events, itr.mic_open_ns);
        draw_tooltip(
            &painter,
            p,
            rect,
            dark,
            &[
                "Interruption (barge-in)".to_string(),
                format!(
                    "user audio at {}",
                    fmt_clock(sh.rel_ms(itr.mic_open_ns), true)
                ),
                match register {
                    Some(r) => format!("pipecat registered: {r:+.0} ms"),
                    None => "pipecat registered: N/A".into(),
                },
                match itr.stop_ms {
                    Some(s) => format!("bot audio stopped: +{s:.0} ms"),
                    None => "bot audio stopped: still playing…".into(),
                },
            ],
        );
    }

    // ---- now playhead ----
    if sh.phase == Phase::Running {
        let x = x_of(now_rel_ms);
        if x >= rect.left() && x <= rect.right() {
            let accent = if dark {
                Color32::from_gray(220)
            } else {
                Color32::from_gray(60)
            };
            painter.line_segment(
                [pos2(x, lanes_top), pos2(x, rect.bottom() - 4.0)],
                Stroke::new(1.0, accent.gamma_multiply(0.7)),
            );
        }
    }

    // ---- idle hint ----
    if sh.phase == Phase::Idle && sh.turns.is_empty() && sh.lanes[LANE_MIC].blocks.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Check your input device and threshold (meters above), then press Start",
            FontId::proportional(14.0),
            if dark {
                Color32::from_gray(150)
            } else {
                Color32::from_gray(120)
            },
        );
    }
}

/// Latest audio block edge (start or end, either lane) at or before `t_ns`:
/// the "previous audio block" a hovered RTVI event is measured against.
fn prev_audio_edge(sh: &Shared, t_ns: u64) -> Option<(u64, usize, bool)> {
    let mut best: Option<(u64, usize, bool)> = None;
    for lane in [LANE_MIC, LANE_SYS] {
        let state = &sh.lanes[lane];
        for b in &state.blocks {
            for (edge, is_end) in [(b.start_ns, false), (b.end_ns, true)] {
                if edge <= t_ns && best.map(|(t, _, _)| edge > t).unwrap_or(true) {
                    best = Some((edge, lane, is_end));
                }
            }
        }
        if let Some(open) = state.open_start_ns {
            if open <= t_ns && best.map(|(t, _, _)| open > t).unwrap_or(true) {
                best = Some((open, lane, false));
            }
        }
    }
    best
}

#[allow(clippy::too_many_arguments)]
fn draw_bins(
    painter: &egui::Painter,
    block_rect: Rect,
    bins: &LaneBins,
    start_ms: f64,
    end_ms: f64,
    view_start: f64,
    view_end: f64,
    ppm: f32,
    x_of: impl Fn(f64) -> f32,
    color: Color32,
) {
    let level = LaneBins::level_for_zoom(ppm);
    let bin_ms = LaneBins::bin_ms(level) as f64;
    let data = bins.level(level);
    if data.is_empty() {
        return;
    }
    let clipped = painter.with_clip_rect(block_rect.intersect(painter.clip_rect()));
    let cy = block_rect.center().y;
    let half = block_rect.height() / 2.0 - 1.0;
    let lo = start_ms.max(view_start).max(0.0);
    let hi = end_ms.min(view_end);
    if hi <= lo {
        return;
    }
    let i0 = (lo / bin_ms) as usize;
    let i1 = ((hi / bin_ms) as usize + 1).min(data.len());
    let bar_w = (bin_ms as f32 * ppm * 0.55).clamp(1.0, 6.0);
    let amp = |v: f32| v.clamp(0.0, 1.0).sqrt();
    for (idx, b) in data.iter().enumerate().take(i1).skip(i0) {
        if b.is_empty() {
            continue;
        }
        let x = x_of(idx as f64 * bin_ms + bin_ms / 2.0);
        let up = amp(b.max) * half;
        let down = amp(-b.min) * half;
        let (mut y0, mut y1) = (cy - up, cy + down);
        if y1 - y0 < 1.5 {
            y0 = cy - 0.75;
            y1 = cy + 0.75;
        }
        clipped.line_segment([pos2(x, y0), pos2(x, y1)], Stroke::new(bar_w, color));
    }
}

fn draw_ruler(
    painter: &egui::Painter,
    rect: Rect,
    view_start: f64,
    view_end: f64,
    ppm: f32,
    dark: bool,
) {
    let text_color = if dark {
        Color32::from_gray(150)
    } else {
        Color32::from_gray(110)
    };
    let tick_color = if dark {
        Color32::from_gray(90)
    } else {
        Color32::from_gray(180)
    };
    let steps = [
        100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 15_000.0, 30_000.0, 60_000.0,
        120_000.0, 300_000.0, 600_000.0,
    ];
    let step = steps
        .iter()
        .copied()
        .find(|s| s * ppm as f64 >= 70.0)
        .unwrap_or(600_000.0);
    let minor = step / 5.0;
    let y1 = rect.top() + RULER_H - 2.0;
    let mut t = (view_start / minor).floor() * minor;
    while t <= view_end {
        if t >= 0.0 {
            let x = rect.left() + ((t - view_start) * ppm as f64) as f32;
            let is_major = (t / step - (t / step).round()).abs() < 1e-6;
            let h = if is_major { 7.0 } else { 4.0 };
            painter.line_segment([pos2(x, y1 - h), pos2(x, y1)], Stroke::new(1.0, tick_color));
            if is_major {
                painter.text(
                    pos2(x + 3.0, rect.top() + 1.0),
                    Align2::LEFT_TOP,
                    fmt_clock(t, step < 1000.0),
                    FontId::monospace(9.5),
                    text_color,
                );
            }
        }
        t += minor;
    }
    painter.line_segment(
        [pos2(rect.left(), y1), pos2(rect.right(), y1)],
        Stroke::new(1.0, tick_color),
    );
}

fn draw_dashed_vline(
    painter: &egui::Painter,
    x: f32,
    y0: f32,
    y1: f32,
    color: Color32,
    width: f32,
) {
    let mut y = y0;
    while y < y1 {
        let seg_end = (y + 4.0).min(y1);
        painter.line_segment([pos2(x, y), pos2(x, seg_end)], Stroke::new(width, color));
        y += 7.0;
    }
}

fn draw_tooltip(painter: &egui::Painter, at: egui::Pos2, bounds: Rect, dark: bool, lines: &[String]) {
    let font = FontId::proportional(11.0);
    let pad = 6.0;
    let line_h = 15.0;
    let widest = lines
        .iter()
        .map(|l| l.len() as f32 * 6.2)
        .fold(60.0, f32::max);
    let size = vec2(widest + pad * 2.0, lines.len() as f32 * line_h + pad * 2.0);
    let mut pos = at + vec2(12.0, 8.0);
    if pos.x + size.x > bounds.right() {
        pos.x = at.x - size.x - 8.0;
    }
    if pos.y + size.y > bounds.bottom() {
        pos.y = at.y - size.y - 8.0;
    }
    let tooltip_rect = Rect::from_min_size(pos, size);
    let (bg, fg) = if dark {
        (Color32::from_gray(30), Color32::from_gray(220))
    } else {
        (Color32::from_gray(250), Color32::from_gray(40))
    };
    painter.rect_filled(tooltip_rect, 4.0, bg);
    painter.rect_stroke(
        tooltip_rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(if dark { 80 } else { 200 })),
        egui::StrokeKind::Inside,
    );
    for (i, line) in lines.iter().enumerate() {
        painter.text(
            pos2(tooltip_rect.left() + pad, tooltip_rect.top() + pad + i as f32 * line_h),
            Align2::LEFT_TOP,
            line,
            font.clone(),
            fg,
        );
    }
}
