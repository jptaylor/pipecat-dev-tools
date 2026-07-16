//! Dedicated audio-control thread. Owns the capture streams (cpal streams
//! are !Send) and performs every potentially-blocking Core Audio call —
//! device enumeration and stream startup can block on the macOS permission
//! prompt, and must never freeze the UI thread.
//!
//! Lock ordering: the analysis thread takes `shared` then `inputs`; this
//! thread never holds `inputs` while taking `shared`.

use super::{capture, SharedInputs};
use crate::session::{SharedState, SysStatus};
use std::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct DeviceSelection {
    pub input_device: Option<String>,
    // Only consumed by the cfg(target_os = "linux") apply path.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub linux_system_device: Option<String>,
}

enum Cmd {
    Apply(DeviceSelection),
    RefreshList,
    Shutdown,
}

pub struct AudioControl {
    tx: mpsc::Sender<Cmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AudioControl {
    pub fn spawn(shared: SharedState, inputs: SharedInputs) -> Self {
        let (tx, rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("audio-control".into())
            .spawn(move || run(rx, shared, inputs))
            .expect("spawn audio-control thread");
        Self {
            tx,
            join: Some(join),
        }
    }

    pub fn apply(&self, sel: DeviceSelection) {
        let _ = self.tx.send(Cmd::Apply(sel));
    }

    pub fn refresh_list(&self) {
        let _ = self.tx.send(Cmd::RefreshList);
    }
}

impl Drop for AudioControl {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run(rx: mpsc::Receiver<Cmd>, shared: SharedState, inputs: SharedInputs) {
    // Streams live on this thread for the app's lifetime.
    let mut mic: Option<capture::CpalCapture> = None;
    #[cfg(target_os = "macos")]
    let mut tap: Option<super::system_mac::SysTap> = None;
    #[cfg(target_os = "linux")]
    let mut sys_cap: Option<capture::CpalCapture> = None;

    loop {
        // Wait for a command; periodically refresh device latency info
        // (cheap property reads, but kept off the UI thread anyway).
        let cmd = match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                refresh_device_info(&shared, mic.as_ref().map(|m| m.device_name.clone()));
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        match cmd {
            Cmd::Shutdown => break,
            Cmd::RefreshList => {
                shared.lock().audio_busy = true;
                let list = capture::list_input_devices();
                let mut sh = shared.lock();
                sh.available_devices = list;
                sh.audio_busy = false;
            }
            Cmd::Apply(sel) => {
                shared.lock().audio_busy = true;

                // Device list (may block on the first mic-permission prompt).
                let list = capture::list_input_devices();
                shared.lock().available_devices = list;

                // --- mic ---
                mic = None;
                match capture::start(sel.input_device.as_deref()) {
                    Ok((cap, ring)) => {
                        inputs.lock().mic = Some(ring);
                        shared.lock().mic_status = None;
                        mic = Some(cap);
                    }
                    Err(e) => {
                        inputs.lock().mic = None;
                        shared.lock().mic_status = Some(e.to_string());
                    }
                }

                // --- system audio ---
                #[cfg(target_os = "macos")]
                {
                    // The tap is independent of device selection and rebuilds
                    // itself on output-device changes: only start it once.
                    if tap.is_none() {
                        match super::system_mac::start() {
                            Ok((t, ring)) => {
                                inputs.lock().sys = Some(ring);
                                shared.lock().sys_status = SysStatus::Ok;
                                tap = Some(t);
                            }
                            Err(e) => {
                                inputs.lock().sys = None;
                                shared.lock().sys_status = SysStatus::Error(e.to_string());
                            }
                        }
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    sys_cap = None;
                    match &sel.linux_system_device {
                        Some(name) => match capture::start(Some(name)) {
                            Ok((cap, ring)) => {
                                inputs.lock().sys = Some(ring);
                                shared.lock().sys_status = SysStatus::Ok;
                                sys_cap = Some(cap);
                            }
                            Err(e) => {
                                inputs.lock().sys = None;
                                shared.lock().sys_status = SysStatus::Error(e.to_string());
                            }
                        },
                        None => {
                            inputs.lock().sys = None;
                            shared.lock().sys_status = SysStatus::Unavailable(
                                "select the PulseAudio/PipeWire monitor source as the system source"
                                    .into(),
                            );
                        }
                    }
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                {
                    shared.lock().sys_status = SysStatus::Unavailable(
                        "system audio capture is not supported on this OS".into(),
                    );
                }

                refresh_device_info(&shared, mic.as_ref().map(|m| m.device_name.clone()));
                shared.lock().audio_busy = false;
            }
        }
    }
}

fn refresh_device_info(shared: &SharedState, mic_name: Option<String>) {
    let input = mic_name.as_deref().and_then(super::input_info_by_name);
    let output = super::default_output_info();
    let mut sh = shared.lock();
    sh.devices.input = input;
    sh.devices.output = output;
}
