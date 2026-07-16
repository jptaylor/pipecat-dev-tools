pub mod controls;
pub mod live_metrics;
pub mod results;
pub mod timeline;

use crate::audio::LANE_MIC;
use egui::Color32;

/// Shared status/grading colors: good, borderline, bad. Used for latency
/// grading, meters, and error text so the whole UI speaks one language.
pub const OK_GREEN: Color32 = Color32::from_rgb(56, 166, 87);
pub const WARN_AMBER: Color32 = Color32::from_rgb(219, 158, 0);
pub const ERR_RED: Color32 = Color32::from_rgb(214, 72, 61);

/// Timeline viewport: what time range is on screen.
pub struct ViewState {
    pub view_start_ms: f64,
    pub px_per_ms: f32,
    pub follow: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            view_start_ms: -200.0,
            px_per_ms: 0.06, // 60 px per second
            follow: true,
        }
    }
}

impl ViewState {
    pub fn fit(&mut self, duration_ms: f64, width_px: f32) {
        let span = duration_ms.max(1000.0) * 1.05;
        self.px_per_ms = (width_px as f64 / span).clamp(0.0015, 4.0) as f32;
        self.view_start_ms = -span * 0.02;
        self.follow = false;
    }

    pub fn zoom_to_span(&mut self, start_ms: f64, end_ms: f64, width_px: f32) {
        let span = (end_ms - start_ms).max(200.0) + 4000.0;
        self.px_per_ms = (width_px as f64 / span).clamp(0.0015, 4.0) as f32;
        self.view_start_ms = start_ms - span * 0.25;
        self.follow = false;
    }
}

/// Transient visual effects: lane flashes on speaker switches and a pulse on
/// the headline latency number when a new turn is measured. Purely cosmetic —
/// derived from shared state, never feeding back into it.
#[derive(Default)]
pub struct Effects {
    prev_open: [bool; 2],
    last_speaker: Option<usize>,
    lane_flash: [Option<std::time::Instant>; 2],
    turn_flash: Option<std::time::Instant>,
    last_turn_count: usize,
}

const LANE_FLASH_MS: f32 = 650.0;
const TURN_FLASH_MS: f32 = 900.0;

impl Effects {
    /// Call once per frame with the shared state locked.
    pub fn update(&mut self, sh: &crate::session::Shared) {
        if sh.phase != crate::session::Phase::Running {
            self.prev_open = [false; 2];
            return;
        }
        for lane in 0..2 {
            let open = sh.lanes[lane].open_start_ns.is_some();
            let rising = open && !self.prev_open[lane];
            if rising && self.last_speaker != Some(lane) {
                self.lane_flash[lane] = Some(std::time::Instant::now());
            }
            if rising {
                self.last_speaker = Some(lane);
            }
            self.prev_open[lane] = open;
        }
        if sh.turns.len() > self.last_turn_count
            && sh.turns.last().and_then(|t| t.latency_ms).is_some()
        {
            self.turn_flash = Some(std::time::Instant::now());
        }
        self.last_turn_count = sh.turns.len();
    }

    fn intensity(at: Option<std::time::Instant>, duration_ms: f32) -> f32 {
        let Some(at) = at else { return 0.0 };
        let t = at.elapsed().as_secs_f32() * 1000.0;
        if t >= duration_ms {
            0.0
        } else {
            let x = 1.0 - t / duration_ms;
            x * x
        }
    }

    /// 0..1 flash intensity for a lane (speaker just switched to it).
    pub fn lane_intensity(&self, lane: usize) -> f32 {
        Self::intensity(self.lane_flash[lane], LANE_FLASH_MS)
    }

    /// 0..1 pulse intensity for the headline latency number.
    pub fn turn_intensity(&self) -> f32 {
        Self::intensity(self.turn_flash, TURN_FLASH_MS)
    }
}

/// Gamma-space lerp between two colors.
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub struct LanePalette {
    pub fill: Color32,
    pub bar: Color32,
    pub label: Color32,
}

pub fn lane_palette(dark: bool, lane: usize) -> LanePalette {
    if lane == LANE_MIC {
        if dark {
            LanePalette {
                fill: Color32::from_rgb(42, 62, 32),
                bar: Color32::from_rgb(174, 213, 129),
                label: Color32::from_rgb(174, 213, 129),
            }
        } else {
            LanePalette {
                fill: Color32::from_rgb(220, 237, 200),
                bar: Color32::from_rgb(51, 105, 30),
                label: Color32::from_rgb(85, 139, 47),
            }
        }
    } else if dark {
        LanePalette {
            fill: Color32::from_rgb(28, 42, 70),
            bar: Color32::from_rgb(144, 180, 250),
            label: Color32::from_rgb(144, 180, 250),
        }
    } else {
        LanePalette {
            fill: Color32::from_rgb(219, 234, 254),
            bar: Color32::from_rgb(48, 79, 158),
            label: Color32::from_rgb(59, 91, 165),
        }
    }
}

/// (fill, accent) for interruption blocks in the row between the lanes.
pub fn interruption_colors(dark: bool) -> (Color32, Color32) {
    if dark {
        (Color32::from_rgb(74, 62, 16), Color32::from_rgb(250, 204, 21))
    } else {
        (Color32::from_rgb(253, 230, 138), Color32::from_rgb(146, 107, 0))
    }
}

pub fn latency_color(latency_ms: f64) -> Color32 {
    if latency_ms < 500.0 {
        OK_GREEN
    } else if latency_ms < 1000.0 {
        WARN_AMBER
    } else {
        ERR_RED
    }
}

pub fn category_color(cat: crate::bridge::protocol::EventCategory) -> Color32 {
    use crate::bridge::protocol::EventCategory as C;
    match cat {
        C::User => Color32::from_rgb(76, 175, 80),
        C::Bot => Color32::from_rgb(66, 133, 244),
        C::Tts => Color32::from_rgb(255, 152, 0),
        C::Llm => Color32::from_rgb(171, 71, 188),
        C::Stt => Color32::from_rgb(0, 150, 136),
        C::Metrics => Color32::from_rgb(140, 140, 140),
        C::Other => Color32::from_rgb(233, 30, 99),
    }
}

/// Compact scrollable list of received bridge events, honoring the
/// per-category filter. Shown in the right panel (live and results).
pub fn event_list(ui: &mut egui::Ui, sh: &crate::session::Shared, max_height: f32) {
    use crate::bridge::protocol::categorize;
    use egui::RichText;

    let filter = &sh.cfg.event_filter;
    let shown: Vec<&crate::session::BridgeEvent> = sh
        .events
        .iter()
        .filter(|e| filter.enabled(categorize(&e.name)))
        .collect();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Events").strong());
        ui.label(
            RichText::new(format!("{} of {} shown", shown.len(), sh.events.len()))
                .weak()
                .size(10.5),
        )
        .on_hover_text("Filter categories via Events ▾ in the top bar");
    });
    ui.add_space(2.0);
    egui::ScrollArea::vertical()
        .id_salt("bridge-event-list")
        .max_height(max_height)
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for e in shown {
                let cat = categorize(&e.name);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(fmt_clock(sh.rel_ms(sh.event_ns(e)), true))
                            .monospace()
                            .weak()
                            .size(10.5),
                    );
                    ui.label(
                        RichText::new(&e.name)
                            .size(11.0)
                            .color(category_color(cat).gamma_multiply(0.95)),
                    )
                    .on_hover_text(format!("source: {}", e.source));
                });
            }
        });
}

/// Per-event-type timing vs audio ground truth: for each RTVI event name,
/// distribution stats of the delta from the previous audio block edge to the
/// event — the same measurement the timeline hover shows, RTVI offset
/// included. Each name expands to its per-occurrence deltas, so drift across
/// turns is visible.
pub fn event_timing(ui: &mut egui::Ui, sh: &crate::session::Shared, max_height: f32) {
    use crate::analysis::{stats, turns};
    use crate::audio::LANE_MIC;
    use crate::bridge::protocol::categorize;
    use egui::RichText;

    let groups = turns::event_timings(sh);
    if groups.is_empty() {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Event timing").strong());
        ui.label(
            RichText::new("Δ to previous audio edge")
                .weak()
                .size(10.5),
        )
        .on_hover_text(
            "For each event type: time from the nearest preceding audio block \
             edge (either lane) to the event, RTVI offset applied. Expand a \
             name to follow the delta occurrence by occurrence.",
        );
        if sh.cfg.rtvi_offset_ms != 0.0 {
            ui.label(
                RichText::new(format!("offset {:+.0} ms", sh.cfg.rtvi_offset_ms))
                    .size(10.5)
                    .color(WARN_AMBER),
            )
            .on_hover_text("Manual RTVI event offset is active (Advanced ▾)");
        }
    });
    ui.add_space(2.0);
    egui::ScrollArea::vertical()
        .id_salt("event-timing")
        .max_height(max_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for (name, occs) in &groups {
                let color = category_color(categorize(name)).gamma_multiply(0.95);
                let deltas: Vec<f64> = occs
                    .iter()
                    .filter_map(|o| o.edge.map(|e| e.delta_ms))
                    .collect();
                let s = stats::summarize(&deltas);
                let header = match deltas.last() {
                    Some(last) => format!(
                        "{name}  ·  n {}  ·  last {last:.0}  ·  p50 {:.0} ms",
                        s.count, s.p50
                    ),
                    None => format!("{name}  ·  n {}  ·  no audio reference", occs.len()),
                };
                egui::CollapsingHeader::new(RichText::new(header).size(11.0).color(color))
                    .id_salt(name)
                    .show(ui, |ui| {
                        if s.count >= 2 {
                            ui.label(
                                RichText::new(format!(
                                    "mean {:.0} · min {:.0} · max {:.0} · σ {:.0} ms",
                                    s.mean, s.min, s.max, s.stdev
                                ))
                                .monospace()
                                .weak()
                                .size(10.5),
                            );
                        }
                        egui::Grid::new(format!("event-timing-{name}"))
                            .num_columns(4)
                            .striped(true)
                            .spacing([10.0, 2.0])
                            .show(ui, |ui| {
                                for h in ["#", "t", "Δ", "after"] {
                                    ui.label(RichText::new(h).weak().size(10.0));
                                }
                                ui.end_row();
                                for (i, o) in occs.iter().enumerate() {
                                    ui.label(
                                        RichText::new(format!("{}", i + 1))
                                            .monospace()
                                            .size(10.5),
                                    );
                                    ui.label(
                                        RichText::new(fmt_clock(sh.rel_ms(o.t_ns), true))
                                            .monospace()
                                            .weak()
                                            .size(10.5),
                                    );
                                    match o.edge {
                                        Some(e) => {
                                            ui.label(
                                                RichText::new(format!("{:.0} ms", e.delta_ms))
                                                    .monospace()
                                                    .size(10.5),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} {}",
                                                    if e.lane == LANE_MIC { "mic" } else { "sys" },
                                                    if e.is_end { "end" } else { "start" },
                                                ))
                                                .weak()
                                                .size(10.5),
                                            );
                                        }
                                        None => {
                                            ui.label(RichText::new("—").weak());
                                            ui.label("");
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            }
        });
}

/// "m:ss" (or "m:ss.t" when `tenths`).
pub fn fmt_clock(ms: f64, tenths: bool) -> String {
    let neg = ms < 0.0;
    let total_s = ms.abs() / 1000.0;
    let m = (total_s / 60.0).floor() as i64;
    let s = total_s - m as f64 * 60.0;
    let sign = if neg { "-" } else { "" };
    if tenths {
        format!("{sign}{m}:{s:04.1}")
    } else {
        format!("{sign}{m}:{:02}", s.floor() as i64)
    }
}

pub fn fmt_opt_ms(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "N/A".into())
}
