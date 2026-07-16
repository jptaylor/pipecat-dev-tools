//! Session export: JSON (everything) and CSV (one row per turn). RTVI-derived
//! fields are null/empty when no bridge events were received.

use crate::analysis::{stats, turns};
use crate::audio::{LANE_MIC, LANE_SYS};
use crate::session::Shared;
use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

fn block_json(sh: &Shared, b: &crate::session::Block) -> serde_json::Value {
    json!({
        "start_ms": sh.rel_ms(b.start_ns),
        "end_ms": sh.rel_ms(b.end_ns),
    })
}

pub fn session_json(sh: &Shared, now_ns: u64) -> serde_json::Value {
    let latencies = sh.valid_latencies();
    let rig = sh.devices.rig_latency_ms();
    let turns_json: Vec<serde_json::Value> = sh
        .turns
        .iter()
        .map(|t| {
            let deltas = turns::rtvi_deltas(t, &sh.events);
            json!({
                "index": t.index,
                "user_start_ms": t.user_start_ns.map(|v| sh.rel_ms(v)),
                "user_end_ms": t.user_end_ns.map(|v| sh.rel_ms(v)),
                "bot_onset_ms": sh.rel_ms(t.bot_onset_ns),
                "bot_group_end_ms": t.bot_group_end_ns.map(|v| sh.rel_ms(v)),
                "latency_ms": t.latency_ms,
                "perceived_latency_ms": t.latency_ms.and_then(|l| rig.map(|r| l + r)),
                "user_response_ms": t.user_response_ms,
                "provisional": t.provisional,
                "flags": t.flags,
                "rtvi": {
                    "vad_stop_delta_ms": deltas.vad_stop_delta_ms,
                    "bot_start_delta_ms": deltas.bot_start_delta_ms,
                },
            })
        })
        .collect();

    json!({
        "app": "pipecat-audio-metrics",
        "version": env!("CARGO_PKG_VERSION"),
        "exported_at": chrono::Local::now().to_rfc3339(),
        "duration_ms": sh.session_duration_ms(now_ns),
        "config": {
            "mic_segmenter": sh.cfg.mic_segmenter,
            "sys_segmenter": sh.cfg.sys_segmenter,
            "merge_gap_ms": sh.cfg.merge_gap_ms,
            "echo_gate": sh.cfg.echo_gate,
            "input_device": sh.cfg.input_device,
        },
        "devices": {
            "input": sh.devices.input,
            "output": sh.devices.output,
            "rig_latency_ms": rig,
            "note": "raw latencies are tap-to-mic (pre-DAC to post-ADC); perceived ≈ raw + rig_latency_ms",
        },
        "stats": {
            "response_latency_ms": stats::summarize(&latencies),
        },
        "turns": turns_json,
        "interruptions": sh.interruptions.iter().map(|i| json!({
            "mic_open_ms": sh.rel_ms(i.mic_open_ns),
            "sys_stop_ms": i.sys_stop_ns.map(|v| sh.rel_ms(v)),
            "stop_ms": i.stop_ms,
            "register_ms": turns::interruption_register_ms(&sh.events, i.mic_open_ns),
        })).collect::<Vec<_>>(),
        "mic_blocks": sh.lanes[LANE_MIC].blocks.iter().map(|b| block_json(sh, b)).collect::<Vec<_>>(),
        // VAD-classified speech intervals (visual layer; blocks drive metrics)
        "mic_speech": sh.lanes[LANE_MIC].speech.iter().map(|b| block_json(sh, b)).collect::<Vec<_>>(),
        "system_blocks": sh.lanes[LANE_SYS].blocks.iter().map(|b| block_json(sh, b)).collect::<Vec<_>>(),
        "rtvi_events": sh.events.iter().map(|e| json!({
            "t_ms": sh.rel_ms(e.t_ns),
            "name": e.name,
            "source": e.source,
            "meta": e.meta,
        })).collect::<Vec<_>>(),
        "discontinuities_ms": sh.discontinuities.iter().map(|&t| sh.rel_ms(t)).collect::<Vec<_>>(),
    })
}

pub fn write_json(sh: &Shared, now_ns: u64, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).context("create session dir")?;
    let path = dir.join("session.json");
    let value = session_json(sh, now_ns);
    std::fs::write(&path, serde_json::to_string_pretty(&value)?).context("write session.json")?;
    Ok(path)
}

pub fn write_csv(sh: &Shared, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).context("create session dir")?;
    let path = dir.join("turns.csv");
    let mut w = csv::Writer::from_path(&path).context("create turns.csv")?;
    w.write_record([
        "turn",
        "user_start_ms",
        "user_end_ms",
        "bot_onset_ms",
        "bot_group_end_ms",
        "latency_ms",
        "perceived_latency_ms",
        "user_response_ms",
        "rtvi_vad_stop_delta_ms",
        "rtvi_bot_start_delta_ms",
        "flags",
    ])?;
    let rig = sh.devices.rig_latency_ms();
    let fmt = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_default();
    for t in &sh.turns {
        let deltas = turns::rtvi_deltas(t, &sh.events);
        w.write_record([
            t.index.to_string(),
            fmt(t.user_start_ns.map(|v| sh.rel_ms(v))),
            fmt(t.user_end_ns.map(|v| sh.rel_ms(v))),
            format!("{:.1}", sh.rel_ms(t.bot_onset_ns)),
            fmt(t.bot_group_end_ns.map(|v| sh.rel_ms(v))),
            fmt(t.latency_ms),
            fmt(t.latency_ms.and_then(|l| rig.map(|r| l + r))),
            fmt(t.user_response_ms),
            fmt(deltas.vad_stop_delta_ms),
            fmt(deltas.bot_start_delta_ms),
            t.flags.summary(),
        ])?;
    }
    w.flush()?;
    Ok(path)
}

pub fn session_dir_name() -> String {
    chrono::Local::now()
        .format("session-%Y%m%d-%H%M%S")
        .to_string()
}

pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
