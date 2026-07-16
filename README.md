# Pipecat Audio Metrics

**Audio-level turn latency benchmarking for voice agents.**

Framework metrics (TTFB, RTVI events) measure what the *pipeline believes*.
This tool measures what the *human experiences*: it captures the raw
microphone and a tap of system audio output on one monotonic clock, and
measures the true gap between the moment your voice stops arriving at the
machine and the moment the bot's audio starts playing out of it.

Two live waveform timelines (mic + system audio) grow as you talk, broken
into activity blocks. Every user-stop → bot-start gap is annotated on the
timeline; stop the session and you get the full distribution — mean, p50,
p90, p95, p99, histogram, and a per-turn table — plus JSON/CSV export.

Optionally, RTVI events from a Pipecat bot or client are overlaid on the
timeline via a local WebSocket bridge, so you can see per-turn deltas between
framework belief and audio truth (VAD stop lag, playout lag). **The bridge is
strictly optional** — without it everything works and RTVI fields read N/A.

- Native compiled app (Rust + egui) — no browser, no webview, nothing in the
  audio path that adds latency
- macOS 14.4+ (primary; Apple Silicon & Intel), Linux best-effort
- The mic path is completely raw: no AEC, AGC, VAD, or noise suppression is
  ever applied. Activity detection is analysis over the captured samples,
  with a configurable threshold — this is a measurement tool, not a voice app

---

## Install (macOS)

Grab `pipecat-audio-metrics-macos.zip` from
[Releases](../../releases), unzip, and move **Pipecat Audio Metrics.app**
wherever you like. The build is ad-hoc signed (not notarized), so on first
launch:

- **Right-click the app → Open → Open**, or
- `xattr -d com.apple.quarantine "Pipecat Audio Metrics.app"`

On first run macOS will ask for two permissions:

1. **Microphone** — to hear you
2. **System audio recording** (Privacy & Security → Screen & System Audio
   Recording → *System Audio Recording Only*) — to hear the bot

No drivers, no BlackHole, no Multi-Output devices: system audio is captured
with the native Core Audio process-tap API (macOS 14.4+).

### Build from source

```bash
cargo build --release          # plain binary (TCC prompts attribute to your terminal)
./scripts/bundle_macos.sh      # proper .app bundle + ad-hoc signature (recommended)
```

## Usage

1. **Pick your mic** in the toolbar (multi-mic setups supported; the
   selection persists). The level meters run all the time — check both lanes
   move before you start.
2. **Set the threshold** per lane (or tick *Auto*): the white tick on each
   meter is the activity threshold. Speech should push the meter past it;
   room noise should not. The **Speech band** toggle (on by default) makes
   detection listen only to 300–3400 Hz, so fans, mains hum, and hiss stop
   drawing blocks — it is detection-only (raw audio, waveform, and WAVs are
   untouched) and its constant filter delay is under 1 ms, so metrics are
   unaffected. Meters show the band-filtered level while it's on.
   **VAD tint** (on by default) adds a second, purely visual tier on the mic
   lane: Silero VAD (running in-process, pure Rust) renders confirmed speech
   in green and washes out other mic activity in gray, so "the mic picked
   something up" and "the user said something" are distinguishable at a
   glance. It never moves block edges or feeds turn pairing — metrics are
   identical with it on or off; the tint itself may trail the live edge by
   ~100 ms of classification delay.
3. Press **Start** (or space), talk to your agent, press **Stop**. On every
   turn switch the newly active lane flashes, and the headline latency
   number pulses when a new turn is measured.
4. Read the results panel; click a turn row to zoom the timeline to it.
   Sessions auto-export to `~/Documents/PipecatAudioMetrics/session-…/`
   (JSON + CSV, plus per-lane WAVs if *Record WAV* is on).

**Wear headphones.** The mic path has no echo cancellation by design, so on
speakers the mic hears the bot and pollutes the mic lane. If you must use
speakers, enable **Echo gate** (Advanced) — it ignores mic activity while bot
audio is playing (analysis-time only; the audio is untouched).

### Reading the numbers

- **Response latency** (headline): last mic activity above threshold →
  first system-audio sample of the bot's reply. Bot audio blocks separated by
  less than the *merge gap* (default 1 s) count as one response, so TTS
  sentence pauses don't create phantom turns.
- **Raw vs perceived**: the tap sees bot audio *before* the DAC and the mic
  sees you *after* the ADC, so the raw gap understates what a human hears by
  roughly the input+output device latency. The app reads the devices'
  reported latencies, shows them, and computes an estimated perceived value.
- **Bluetooth warning**: BT devices add 100–300 ms that no framework metric
  can see. The app flags BT transport on either device — benchmark on wired
  audio when you can.
- Barge-ins, double-talk, and bot self-continuations are flagged per turn,
  not silently dropped.
- **Interruptions**: when mic audio starts while bot audio is playing, a
  yellow block appears in the row between the two lanes spanning from the
  barging mic onset to the moment system audio actually stopped. Hovering it
  shows how long the framework took to register the speech (nearest RTVI
  `user_started_speaking`, when bridged) and how long the bot kept talking
  before stopping. Interruptions are included in the JSON export.

## RTVI bridge (optional)

The app hosts a WebSocket server on `ws://0.0.0.0:8123` (configurable under
*Advanced*, can be disabled). Anything that can open a WebSocket can send it
events; they appear as vertical markers on the timeline and produce per-turn
deltas in the table and exports:

- **Δvad** = RTVI `user_stopped_speaking` − measured mic-block end
  (how far behind reality the pipeline's VAD is: stop_secs + transport)
- **Δplayout** = measured bot audio onset − RTVI `bot_started_speaking`
  (how long after the framework "spoke" the sound actually exists)

### Protocol

```json
{"v": 1, "type": "event", "name": "user_stopped_speaking", "source": "pipecat", "meta": {}}
```

Events are timestamped on arrival (sub-ms on localhost/LAN). Unknown names
are fine — they still render as markers. Raw RTVI-style messages
(`{"type": "bot-started-speaking"}`) are also accepted.

Event categories (user / bot / TTS / LLM / STT / metrics / other) can be
toggled via *Events ▾* in the top bar; disabled categories disappear from the
timeline markers and the event list in the right panel. Metrics events are
off by default. Hovering a marker draws a latency measurement back to the
previous audio block edge, so you can read the gap between audio ground truth
and the event landing. Smoke-test with:

```bash
echo '{"v":1,"type":"event","name":"user_stopped_speaking"}' | websocat ws://localhost:8123
```

### Pipecat bot (server-side)

A ready-to-run test bot with the bridge already wired lives in
[`bot/`](bot/README.md) (`cd bot/server && uv sync && cp .env.example .env`,
add keys, `uv run bot.py`, open http://localhost:7860).

For your own bot, copy
[`adapters/pipecat_observer.py`](adapters/pipecat_observer.py) next to it and
add one line:

```python
from pipecat_observer import AudioMetricsBridgeObserver

task = PipelineTask(
    pipeline,
    params=PipelineParams(enable_metrics=True),
    observers=[AudioMetricsBridgeObserver("ws://<metrics-machine>:8123")],
)
```

Forwards `user_started/stopped_speaking`, `bot_started/stopped_speaking`,
TTS start/stop, and `MetricsFrame` TTFB data. Fire-and-forget: if the app
isn't running your bot is unaffected.

### RTVI JS client (web client-side)

```js
import { attachAudioMetricsBridge } from "./rtvi_client_bridge.js";
const detach = attachAudioMetricsBridge(client, { url: "ws://localhost:8123" });
```

## Linux

System audio comes from your PulseAudio/PipeWire **monitor source**:

1. Build: `cargo build --release` (needs `libasound2-dev`).
2. In the toolbar, set **System source** to the `Monitor of …` device for
   your output. If it isn't listed, check `pactl list short sources | grep
   monitor` and make sure the pulse/pipewire ALSA plugin is available.

Device-latency reporting and the Core Audio tap are macOS-only; on Linux the
device panel shows N/A and everything else works the same.

## CLI

```bash
pipecat-audio-metrics --list-devices   # input devices + tap support + output latency
pipecat-audio-metrics --diagnose       # 5s headless capture check (mic + system levels)
```

`--diagnose` is the quickest way to verify permissions: both lanes should
show real dB levels while you speak and play any audio.

## How it measures (accuracy notes)

- One monotonic clock (`mach_absolute_time` / `CLOCK_MONOTONIC`) for
  everything. The system tap delivers hardware timestamps per buffer; mic
  timestamps are calibrated with a min-filter over callback times that
  rejects scheduling jitter and tracks sample-clock drift. Block edges are
  sample-accurate and **backdated to the actual threshold crossings** — the
  250 ms close-hangover never inflates a latency number.
- Detection granularity is a 10 ms RMS window; end-to-end timing error is
  ~1–2 ms, far below the tens-of-ms differences you're hunting.
- If the default output device changes mid-session (AirPods connect…), the
  tap restarts, the timeline shows a ⚠ discontinuity, and affected turns are
  flagged.

## License

MIT
