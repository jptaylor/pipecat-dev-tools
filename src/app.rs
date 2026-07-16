//! Top-level eframe app: wires panels together and manages the session
//! lifecycle. All Core Audio work happens on the audio-control thread so the
//! UI never blocks on permission prompts.

use crate::analysis::{self, AnalysisHandle};
use crate::audio::control::{AudioControl, DeviceSelection};
use crate::audio::{LaneInputs, SharedInputs};
use crate::bridge::server::{self, BridgeHandle};
use crate::clock;
use crate::config::Config;
use crate::export;
use crate::session::{self, Phase, SharedState, SysStatus};
use crate::ui::{self, Effects, ViewState};
use std::time::{Duration, Instant};

pub struct App {
    cfg: Config,
    shared: SharedState,
    _analysis: AnalysisHandle,
    audio: AudioControl,
    bridge: Option<BridgeHandle>,
    view: ViewState,
    effects: Effects,
    toast: Option<(String, Instant)>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        let shared = session::new_shared(cfg.clone());
        let inputs: SharedInputs =
            std::sync::Arc::new(parking_lot::Mutex::new(LaneInputs::default()));
        let analysis = analysis::spawn(shared.clone(), inputs.clone());
        let audio = AudioControl::spawn(shared.clone(), inputs);

        let mut app = Self {
            cfg,
            shared,
            _analysis: analysis,
            audio,
            bridge: None,
            view: ViewState::default(),
            effects: Effects::default(),
            toast: None,
        };
        app.audio.apply(app.selection());
        app.restart_bridge();
        app
    }

    fn selection(&self) -> DeviceSelection {
        DeviceSelection {
            input_device: self.cfg.input_device.clone(),
            linux_system_device: self.cfg.linux_system_device.clone(),
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn restart_bridge(&mut self) {
        self.bridge = None;
        {
            let mut sh = self.shared.lock();
            sh.bridge.running = false;
            sh.bridge.error = None;
            sh.bridge.clients = 0;
            sh.bridge.port = self.cfg.bridge.port;
        }
        if !self.cfg.bridge.enabled {
            return;
        }
        match server::start(self.cfg.bridge.port, self.shared.clone()) {
            Ok(handle) => self.bridge = Some(handle),
            Err(e) => {
                self.shared.lock().bridge.error = Some(e);
            }
        }
    }

    fn start_session(&mut self) {
        let dir = self.cfg.export_root().join(export::session_dir_name());
        let dir_ok = std::fs::create_dir_all(&dir).is_ok();

        let mut sh = self.shared.lock();
        sh.clear_session_data();
        sh.session_start_ns = clock::now_ns();
        sh.session_dir = dir_ok.then_some(dir);
        sh.phase = Phase::Running;
        drop(sh);

        self.view = ViewState::default();
        self.effects = Effects::default();
    }

    fn stop_session(&mut self) {
        {
            let mut sh = self.shared.lock();
            if sh.phase == Phase::Running {
                sh.session_end_ns = clock::now_ns();
                sh.phase = Phase::Stopped;
            }
        }
        // Give the analysis thread a moment to close open blocks before the
        // auto-export snapshot (it finalizes on its next ~10ms tick).
        std::thread::sleep(Duration::from_millis(40));
        self.do_export(true, true, false);
        self.toast("Session saved");
    }

    fn new_session(&mut self) {
        let mut sh = self.shared.lock();
        sh.clear_session_data();
        sh.session_dir = None;
        sh.phase = Phase::Idle;
        drop(sh);
        self.view = ViewState::default();
        self.effects = Effects::default();
    }

    fn do_export(&mut self, json: bool, csv: bool, toast: bool) {
        let sh = self.shared.lock();
        let Some(dir) = sh.session_dir.clone() else {
            drop(sh);
            self.toast("No session folder");
            return;
        };
        let now = clock::now_ns();
        let mut messages = Vec::new();
        if json {
            match export::write_json(&sh, now, &dir) {
                Ok(p) => messages.push(format!(
                    "wrote {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )),
                Err(e) => messages.push(format!("JSON export failed: {e}")),
            }
        }
        if csv {
            match export::write_csv(&sh, &dir) {
                Ok(p) => messages.push(format!(
                    "wrote {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )),
                Err(e) => messages.push(format!("CSV export failed: {e}")),
            }
        }
        drop(sh);
        if toast && !messages.is_empty() {
            self.toast(messages.join(" · "));
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let sh = self.shared.lock();
        let mut problems: Vec<String> = Vec::new();
        if let Some(m) = &sh.mic_status {
            problems.push(format!("mic: {m}"));
        }
        if let SysStatus::Error(e) = &sh.sys_status {
            problems.push(format!("system audio: {e}"));
        }
        if let Some(w) = &sh.wav_error {
            problems.push(format!("wav: {w}"));
        }
        let dropped: u64 = sh.lanes.iter().map(|l| l.dropped).sum();
        if dropped > 0 {
            problems.push(format!("{dropped} samples dropped"));
        }
        let busy = sh.audio_busy;
        let session_dir = sh.session_dir.clone();
        let phase = sh.phase;
        drop(sh);

        ui.horizontal(|ui| {
            if busy {
                ui.spinner();
                ui.label(
                    egui::RichText::new(
                        "starting audio (answer the permission prompt if one appeared)…",
                    )
                    .size(11.0),
                );
            } else if problems.is_empty() {
                ui.label(
                    egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .weak()
                        .size(10.5),
                );
            } else {
                ui.colored_label(
                    ui::ERR_RED,
                    egui::RichText::new(problems.join("  ·  ")).size(11.0),
                );
            }
            if let Some((msg, at)) = &self.toast {
                if at.elapsed() < Duration::from_secs(5) {
                    ui.separator();
                    ui.label(egui::RichText::new(msg.clone()).size(11.0));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(dir) = session_dir {
                    if phase != Phase::Idle {
                        let name = dir
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if ui
                            .link(egui::RichText::new(name).size(10.5))
                            .on_hover_text(dir.display().to_string())
                            .clicked()
                        {
                            export::reveal_in_file_manager(&dir);
                        }
                    }
                }
            });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now_ns = clock::now_ns();
        let cfg_before = self.cfg.clone();

        // Space toggles start/stop (unless typing in a widget).
        let space = ctx.input(|i| i.key_pressed(egui::Key::Space)) && !ctx.wants_keyboard_input();

        let phase = {
            let sh = self.shared.lock();
            self.effects.update(&sh);
            sh.phase
        };

        let mut controls_out = ui::controls::ControlsOutput::default();
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            let sh = self.shared.lock();
            let devices = sh.available_devices.clone();
            controls_out =
                ui::controls::show(ui, &mut self.cfg, &sh, &devices, &mut self.view, now_ns);
            drop(sh);
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            self.status_bar(ui);
        });

        let mut results_out = ui::results::ResultsOutput::default();
        egui::SidePanel::right("metrics")
            .resizable(true)
            .default_width(340.0)
            .min_width(280.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                let sh = self.shared.lock();
                match sh.phase {
                    Phase::Stopped => {
                        results_out = ui::results::show(ui, &sh, now_ns);
                    }
                    _ => {
                        ui::live_metrics::show(ui, &sh, now_ns, self.effects.turn_intensity());
                    }
                }
            });

        let mut timeline_width = 800.0f32;
        egui::CentralPanel::default().show(ctx, |ui| {
            timeline_width = ui.available_width();
            let sh = self.shared.lock();
            ui::timeline::show(ui, &sh, &mut self.view, &self.effects, now_ns);
        });

        // ---- handle actions (locks released) ----
        if controls_out.refresh_devices {
            self.audio.refresh_list();
        }
        if controls_out.apply_devices {
            self.audio.apply(self.selection());
        }
        if controls_out.restart_bridge {
            self.restart_bridge();
        }
        if controls_out.fit_clicked {
            let dur = self.shared.lock().session_duration_ms(now_ns);
            self.view.fit(dur, timeline_width);
        }
        if controls_out.start_clicked || (space && phase != Phase::Running) {
            self.start_session();
        } else if controls_out.stop_clicked || (space && phase == Phase::Running) {
            self.stop_session();
        }

        if let Some(idx) = results_out.zoom_to_turn {
            let span = {
                let sh = self.shared.lock();
                sh.turns.get(idx).map(|t| {
                    let start = t
                        .user_end_ns
                        .map(|v| sh.rel_ms(v))
                        .unwrap_or_else(|| sh.rel_ms(t.bot_onset_ns) - 1000.0);
                    let end = sh.rel_ms(t.bot_group_end_ns.unwrap_or(t.bot_onset_ns));
                    (start, end)
                })
            };
            if let Some((start, end)) = span {
                self.view.zoom_to_span(start, end, timeline_width);
            }
        }
        if results_out.export_json || results_out.export_csv {
            self.do_export(results_out.export_json, results_out.export_csv, true);
        }
        if results_out.open_folder {
            let dir = self.shared.lock().session_dir.clone();
            if let Some(dir) = dir {
                export::reveal_in_file_manager(&dir);
            }
        }
        if results_out.new_session {
            self.new_session();
        }

        // ---- config sync ----
        if self.cfg != cfg_before {
            self.shared.lock().cfg = self.cfg.clone();
            self.cfg.save();
        }

        // Meters/timeline are live: repaint continuously, faster while running.
        let dt = if phase == Phase::Running { 16 } else { 50 };
        ctx.request_repaint_after(Duration::from_millis(dt));
    }
}
