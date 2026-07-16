//! Turn pairing: matches the end of user speech to the onset of bot audio.
//! Bot blocks separated by less than merge_gap_ms (TTS sentence pauses) are
//! grouped into one bot response. Anomalies (barge-in, double talk, bot
//! self-continuation) are flagged, never silently dropped.

use super::segmenter::SegEvent;
use crate::audio::{LANE_MIC, LANE_SYS};
use crate::session::BridgeEvent;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TurnFlags {
    /// User started speaking while bot audio was playing (interruption).
    pub barge_in: bool,
    /// User and bot audio overlapped substantially.
    pub double_talk: bool,
    /// Bot spoke without any user speech since the previous bot response.
    pub no_user_speech: bool,
    /// An output-device change happened near this turn; timing suspect.
    pub after_discontinuity: bool,
}

impl TurnFlags {
    pub fn any(&self) -> bool {
        self.barge_in || self.double_talk || self.no_user_speech || self.after_discontinuity
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.barge_in {
            parts.push("barge-in");
        }
        if self.double_talk {
            parts.push("double-talk");
        }
        if self.no_user_speech {
            parts.push("no-user-speech");
        }
        if self.after_discontinuity {
            parts.push("device-change");
        }
        parts.join(", ")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnRecord {
    pub index: usize,
    /// End of the user utterance that triggered this response.
    pub user_end_ns: Option<u64>,
    /// Start of the user utterance (for reference/exports).
    pub user_start_ns: Option<u64>,
    /// First bot audio of the response group.
    pub bot_onset_ns: u64,
    /// End of the last bot block in the group (updates as the group extends).
    pub bot_group_end_ns: Option<u64>,
    /// The headline metric: user stopped → bot audio started.
    pub latency_ms: Option<f64>,
    /// Gap between previous bot response end and this turn's user speech start.
    pub user_response_ms: Option<f64>,
    /// True until the bot's first block survives min-block filtering.
    pub provisional: bool,
    pub flags: TurnFlags,
}

/// A barge-in and its outcome: mic audio started while bot audio was
/// playing; the interruption "triggered" when system audio actually stopped.
/// Measured from audio ground truth, like turns.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct InterruptionRecord {
    /// Mic audio onset that barged in over bot audio.
    pub mic_open_ns: u64,
    /// When system audio actually stopped (None while still playing).
    pub sys_stop_ns: Option<u64>,
    /// mic_open → sys_stop: how long the bot kept talking over the user.
    pub stop_ms: Option<f64>,
}

/// A barging mic block shorter than this that ends while the bot is still
/// talking is treated as echo/noise, not a real interruption attempt, and
/// its interruption record is retracted.
const MIN_BARGE_MS: f64 = 250.0;

#[derive(Debug, Clone, Copy)]
struct UserBlock {
    start_ns: u64,
    end_ns: u64,
    /// The utterance began while bot audio was audible (a barge-in).
    during_bot: bool,
}

#[derive(Debug, Clone, Copy)]
struct OpenUser {
    start_ns: u64,
    during_bot: bool,
    /// Index of the interruption record this open created, if any.
    interruption_idx: Option<usize>,
}

struct BotGroup {
    onset_ns: u64,
    last_end_ns: Option<u64>,
    open: bool,
    turn_index: usize,
    closed_blocks: usize,
    /// A mic block was still open when this group started — usually only
    /// hangover keeping it open, its Close backdated to before the onset.
    /// The turn's user pairing is resolved when that block closes/cancels.
    awaiting_user: bool,
    /// Latest closed user block at group start: the pairing fallback if the
    /// pending block turns out to be a cancelled blip.
    fallback_user: Option<UserBlock>,
}

#[derive(Default)]
pub struct TurnTracker {
    user_blocks: Vec<UserBlock>,
    user_open: Option<OpenUser>,
    group: Option<BotGroup>,
    prev_group_onset_ns: Option<u64>,
    pending_user_response_ms: Option<f64>,
    pending_discontinuity: bool,
    /// Index into the interruptions vec of the in-flight barge-in.
    pending_interruption: Option<usize>,
}

impl TurnTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn note_discontinuity(&mut self) {
        self.pending_discontinuity = true;
    }

    fn bot_audible(&self) -> bool {
        self.group.as_ref().map(|g| g.open).unwrap_or(false)
    }

    pub fn on_event(
        &mut self,
        lane: usize,
        event: SegEvent,
        merge_gap_ms: u64,
        turns: &mut Vec<TurnRecord>,
        interruptions: &mut Vec<InterruptionRecord>,
    ) {
        match lane {
            LANE_MIC => self.on_user_event(event, turns, interruptions),
            LANE_SYS => self.on_bot_event(event, merge_gap_ms, turns, interruptions),
            _ => {}
        }
    }

    fn on_user_event(
        &mut self,
        event: SegEvent,
        turns: &mut [TurnRecord],
        interruptions: &mut Vec<InterruptionRecord>,
    ) {
        match event {
            SegEvent::Open { t_ns } => {
                let during_bot = self.bot_audible();
                let mut interruption_idx = None;
                if during_bot && self.pending_interruption.is_none() {
                    // Record the barge-in; the record completes when system
                    // audio actually stops (next sys Close).
                    interruptions.push(InterruptionRecord {
                        mic_open_ns: t_ns,
                        sys_stop_ns: None,
                        stop_ms: None,
                    });
                    let idx = interruptions.len() - 1;
                    self.pending_interruption = Some(idx);
                    interruption_idx = Some(idx);
                }
                if let Some(g) = &self.group {
                    if !g.open {
                        if let Some(end) = g.last_end_ns {
                            if t_ns > end {
                                self.pending_user_response_ms =
                                    Some((t_ns - end) as f64 / 1e6);
                            }
                        }
                    }
                }
                self.user_open = Some(OpenUser {
                    start_ns: t_ns,
                    during_bot,
                    interruption_idx,
                });
            }
            SegEvent::Close { t_ns } => {
                if let Some(open) = self.user_open.take() {
                    let blk = UserBlock {
                        start_ns: open.start_ns,
                        end_ns: t_ns,
                        during_bot: open.during_bot,
                    };
                    self.user_blocks.push(blk);
                    let dur_ms = t_ns.saturating_sub(open.start_ns) as f64 / 1e6;
                    // A short blip that ended while the bot kept talking is
                    // echo or noise, not a barge: retract its record. (A
                    // record already completed by a sys close stays — the
                    // bot really did stop right after it.)
                    if let Some(idx) = open.interruption_idx {
                        if dur_ms < MIN_BARGE_MS && self.pending_interruption == Some(idx) {
                            if idx + 1 == interruptions.len() {
                                interruptions.pop();
                            }
                            self.pending_interruption = None;
                        }
                    }
                    self.resolve_awaiting_user(Some(blk), turns);
                    if open.during_bot && self.bot_audible() && dur_ms >= MIN_BARGE_MS {
                        // Sustained user audio under this group's audio:
                        // double talk on the currently-playing turn (short
                        // blips are echo/noise, same rule as interruptions).
                        if let Some(g) = &self.group {
                            if t_ns >= g.onset_ns {
                                if let Some(t) = turns.get_mut(g.turn_index) {
                                    t.flags.double_talk = true;
                                }
                            }
                        }
                    }
                }
            }
            SegEvent::Cancel => {
                // The mic block was a blip: retract the interruption it
                // opened (it is necessarily the last record, whether or not
                // it was already finalized by a sys close).
                if let Some(open) = self.user_open.take() {
                    if let Some(idx) = open.interruption_idx {
                        if idx + 1 == interruptions.len() {
                            interruptions.pop();
                        }
                        if self.pending_interruption == Some(idx) {
                            self.pending_interruption = None;
                        }
                    }
                    self.resolve_awaiting_user(None, turns);
                }
            }
        }
    }

    /// Complete a turn whose user pairing was deferred because a mic block
    /// was still open when its bot group started. `closed` is that block
    /// once it closes, or None if it was a cancelled blip.
    fn resolve_awaiting_user(&mut self, closed: Option<UserBlock>, turns: &mut [TurnRecord]) {
        let Some(g) = &mut self.group else { return };
        if !g.awaiting_user {
            return;
        }
        g.awaiting_user = false;
        let Some(t) = turns.get_mut(g.turn_index) else { return };
        // Hangover backdating usually puts the pending block's end before
        // the bot onset: a normal turn. A sustained genuine overlap keeps
        // latency undefined; a cancelled or straddling blip (echo/noise)
        // falls back to the last closed block.
        let user = match closed {
            Some(b) if b.end_ns <= g.onset_ns => Some(b),
            Some(b) if (b.end_ns - b.start_ns) as f64 / 1e6 >= MIN_BARGE_MS => {
                t.flags.double_talk = true;
                return;
            }
            _ => g.fallback_user,
        };
        match user {
            Some(b) => {
                t.user_end_ns = Some(b.end_ns);
                t.user_start_ns = Some(b.start_ns);
                t.latency_ms = Some(g.onset_ns.saturating_sub(b.end_ns) as f64 / 1e6);
                t.flags.barge_in = b.during_bot;
                t.flags.no_user_speech = false;
            }
            None => {
                t.flags.no_user_speech = true;
            }
        }
    }

    fn on_bot_event(
        &mut self,
        event: SegEvent,
        merge_gap_ms: u64,
        turns: &mut Vec<TurnRecord>,
        interruptions: &mut Vec<InterruptionRecord>,
    ) {
        match event {
            SegEvent::Open { t_ns } => {
                // Extend the current group if the gap since its last audio is
                // small (TTS sentence pause).
                if let Some(g) = &mut self.group {
                    let ref_end = g.last_end_ns.unwrap_or(g.onset_ns);
                    if !g.open && t_ns.saturating_sub(ref_end) <= merge_gap_ms * 1_000_000 {
                        g.open = true;
                        return;
                    }
                    if g.open {
                        // Shouldn't happen (segmenter is a state machine), but
                        // treat as continuation.
                        return;
                    }
                }
                self.start_new_group(t_ns, turns);
            }
            SegEvent::Close { t_ns } => {
                if let Some(g) = &mut self.group {
                    g.open = false;
                    g.last_end_ns = Some(t_ns);
                    g.closed_blocks += 1;
                    if let Some(t) = turns.get_mut(g.turn_index) {
                        t.provisional = false;
                        t.bot_group_end_ns = Some(t_ns);
                    }
                }
                // System audio stopped: the pending interruption triggered.
                if let Some(idx) = self.pending_interruption.take() {
                    let retract = interruptions
                        .get(idx)
                        .map(|r| t_ns <= r.mic_open_ns)
                        .unwrap_or(true);
                    if retract {
                        // The close was backdated to before the mic onset:
                        // the bot audio had already stopped, not a real
                        // interruption.
                        if idx + 1 == interruptions.len() {
                            interruptions.pop();
                        }
                    } else if let Some(r) = interruptions.get_mut(idx) {
                        r.sys_stop_ns = Some(t_ns);
                        r.stop_ms = Some((t_ns - r.mic_open_ns) as f64 / 1e6);
                    }
                }
            }
            SegEvent::Cancel => {
                // The bot audio the user barged over was itself a blip.
                if let Some(idx) = self.pending_interruption.take() {
                    if idx + 1 == interruptions.len() {
                        interruptions.pop();
                    }
                }
                if let Some(g) = &mut self.group {
                    if g.open && g.closed_blocks == 0 {
                        // The group's very first block was a blip: retract the
                        // provisional turn entirely.
                        let idx = g.turn_index;
                        if idx == turns.len().saturating_sub(1)
                            && turns.get(idx).map(|t| t.provisional).unwrap_or(false)
                        {
                            turns.pop();
                        }
                        self.group = None;
                    } else if g.open {
                        // A cancelled extension block: group stays as it was.
                        g.open = false;
                    }
                }
            }
        }
    }

    fn start_new_group(&mut self, onset_ns: u64, turns: &mut Vec<TurnRecord>) {
        // Finalize bookkeeping of the previous group.
        if let Some(prev) = self.group.take() {
            // Pairing still deferred on the previous group with the user
            // still talking: that turn really was talked over throughout.
            if prev.awaiting_user && self.user_open.is_some() {
                if let Some(t) = turns.get_mut(prev.turn_index) {
                    t.flags.double_talk = true;
                }
            }
            self.prev_group_onset_ns = Some(prev.onset_ns);
        }

        // The user utterance that triggered this response: latest user block
        // ending before the bot onset, and after the previous bot response
        // began (otherwise it already "belonged" to an earlier exchange).
        let min_end = self.prev_group_onset_ns.unwrap_or(0);
        let user = self
            .user_blocks
            .iter()
            .rev()
            .find(|b| b.end_ns <= onset_ns && b.end_ns > min_end)
            .copied();

        // A mic block still open at onset is usually just hangover — its
        // Close will be backdated to before the onset — or a blip that will
        // cancel. Defer pairing to resolve_awaiting_user instead of guessing
        // double-talk now (that guess erased latencies on real turns).
        let awaiting_user = self.user_open.is_some();

        let flags = TurnFlags {
            barge_in: !awaiting_user && user.map(|b| b.during_bot).unwrap_or(false),
            double_talk: false,
            no_user_speech: user.is_none() && !awaiting_user,
            after_discontinuity: std::mem::take(&mut self.pending_discontinuity),
        };
        let (user_end_ns, user_start_ns, latency_ms) = match (user, awaiting_user) {
            (Some(b), false) => (
                Some(b.end_ns),
                Some(b.start_ns),
                Some(onset_ns.saturating_sub(b.end_ns) as f64 / 1e6),
            ),
            // Provisional fallback shown while the pending block resolves.
            (Some(b), true) => (Some(b.end_ns), Some(b.start_ns), None),
            (None, _) => (None, None, None),
        };

        let index = turns.len();
        turns.push(TurnRecord {
            index,
            user_end_ns,
            user_start_ns,
            bot_onset_ns: onset_ns,
            bot_group_end_ns: None,
            latency_ms,
            user_response_ms: std::mem::take(&mut self.pending_user_response_ms),
            provisional: true,
            flags,
        });

        self.group = Some(BotGroup {
            onset_ns,
            last_end_ns: None,
            open: true,
            turn_index: index,
            closed_blocks: 0,
            awaiting_user,
            fallback_user: user,
        });
    }
}

/// Deltas between framework-reported (RTVI) events and audio ground truth.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RtviDeltas {
    /// rtvi user_stopped_speaking − measured mic block end (VAD stop lag).
    pub vad_stop_delta_ms: Option<f64>,
    /// measured bot audio onset − rtvi bot_started_speaking (playout lag).
    pub bot_start_delta_ms: Option<f64>,
}

const RTVI_MATCH_WINDOW_MS: f64 = 2000.0;

fn nearest_event_delta(events: &[BridgeEvent], names: &[&str], t_ns: u64) -> Option<f64> {
    let t_ms = t_ns as f64 / 1e6;
    events
        .iter()
        .filter(|e| names.contains(&e.name.as_str()))
        .map(|e| e.t_ns as f64 / 1e6 - t_ms)
        .filter(|d| d.abs() <= RTVI_MATCH_WINDOW_MS)
        .min_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap())
}

/// How long the framework took to register the interrupting mic audio:
/// nearest `user_started_speaking` event relative to the mic onset. None when
/// no bridge event matches (RTVI optional).
pub fn interruption_register_ms(events: &[BridgeEvent], mic_open_ns: u64) -> Option<f64> {
    nearest_event_delta(events, &["user_started_speaking"], mic_open_ns)
}

pub fn rtvi_deltas(turn: &TurnRecord, events: &[BridgeEvent]) -> RtviDeltas {
    let vad_stop_delta_ms = turn
        .user_end_ns
        .and_then(|end| nearest_event_delta(events, &["user_stopped_speaking"], end));
    // Positive = audio came out after the framework thought the bot started.
    let bot_start_delta_ms =
        nearest_event_delta(events, &["bot_started_speaking"], turn.bot_onset_ns).map(|d| -d);
    RtviDeltas {
        vad_stop_delta_ms,
        bot_start_delta_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn ev(
        tracker: &mut TurnTracker,
        lane: usize,
        e: SegEvent,
        turns: &mut Vec<TurnRecord>,
        itr: &mut Vec<InterruptionRecord>,
    ) {
        tracker.on_event(lane, e, 1000, turns, itr);
    }

    #[test]
    fn basic_turn_latency() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 3000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 3800 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].latency_ms, Some(800.0));
        assert!(turns[0].provisional);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 6000 * MS }, &mut turns, &mut itr);
        assert!(!turns[0].provisional);
        assert_eq!(turns[0].bot_group_end_ns, Some(6000 * MS));
    }

    #[test]
    fn tts_sentence_pause_merges_into_one_group() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 0 }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 2000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 2500 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 4000 * MS }, &mut turns, &mut itr);
        // 600ms pause < merge_gap 1000ms → same group, no new turn
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 4600 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 7000 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].bot_group_end_ns, Some(7000 * MS));
    }

    #[test]
    fn multi_turn_conversation() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        // turn 1
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 0 }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 2000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 2700 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 5000 * MS }, &mut turns, &mut itr);
        // turn 2: user replies 900ms after bot finished
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 5900 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 8000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 9000 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].latency_ms, Some(700.0));
        assert_eq!(turns[1].latency_ms, Some(1000.0));
        assert_eq!(turns[1].user_response_ms, Some(900.0));
        assert!(!turns[1].flags.any());
    }

    #[test]
    fn barge_in_flagged() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 0 }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1500 * MS }, &mut turns, &mut itr);
        // user barges in while bot is talking
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 3000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 3400 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 4500 * MS }, &mut turns, &mut itr);
        // bot responds to the barge
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 5200 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 2);
        assert!(turns[1].flags.barge_in);
        assert_eq!(turns[1].latency_ms, Some(700.0));
        // Interruption measured: mic opened at 3000, sys audio stopped at 3400.
        assert_eq!(itr.len(), 1);
        assert_eq!(itr[0].mic_open_ns, 3000 * MS);
        assert_eq!(itr[0].sys_stop_ns, Some(3400 * MS));
        assert_eq!(itr[0].stop_ms, Some(400.0));
    }

    #[test]
    fn cancelled_barge_blip_retracts_interruption() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 0 }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1500 * MS }, &mut turns, &mut itr);
        // mic blip during bot audio: interruption recorded then retracted
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 3000 * MS }, &mut turns, &mut itr);
        assert_eq!(itr.len(), 1);
        ev(&mut tr, LANE_MIC, SegEvent::Cancel, &mut turns, &mut itr);
        assert!(itr.is_empty());
        // a later real sys close does not resurrect it
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 5000 * MS }, &mut turns, &mut itr);
        assert!(itr.is_empty());
    }

    #[test]
    fn interruption_open_until_sys_close() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 2000 * MS }, &mut turns, &mut itr);
        // still in flight while bot keeps talking
        assert_eq!(itr.len(), 1);
        assert_eq!(itr[0].sys_stop_ns, None);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 2600 * MS }, &mut turns, &mut itr);
        assert_eq!(itr[0].sys_stop_ns, None);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 3100 * MS }, &mut turns, &mut itr);
        assert_eq!(itr[0].stop_ms, Some(1100.0));
    }

    #[test]
    fn bot_solo_flagged_no_user_speech() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].flags.no_user_speech);
        assert_eq!(turns[0].latency_ms, None);
    }

    /// The mic Close event arrives after the bot onset (hangover delay) but
    /// is backdated to before it: the turn must still get its latency.
    #[test]
    fn late_mic_close_still_yields_latency() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 3000 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].latency_ms, None, "deferred until the mic close");
        // Close arrives late, backdated to 2800 (before the 3000 onset).
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 2800 * MS }, &mut turns, &mut itr);
        assert_eq!(turns[0].latency_ms, Some(200.0));
        assert_eq!(turns[0].user_end_ns, Some(2800 * MS));
        assert!(!turns[0].flags.any(), "flags: {:?}", turns[0].flags);
    }

    /// Echo/noise opens the mic just before the bot-onset event and later
    /// cancels: the turn pairs with the real preceding utterance.
    #[test]
    fn cancelled_blip_at_onset_falls_back_to_last_block() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 2000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 2950 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 3000 * MS }, &mut turns, &mut itr);
        assert_eq!(turns[0].latency_ms, None);
        ev(&mut tr, LANE_MIC, SegEvent::Cancel, &mut turns, &mut itr);
        assert_eq!(turns[0].latency_ms, Some(1000.0));
        assert_eq!(turns[0].user_end_ns, Some(2000 * MS));
        assert!(!turns[0].flags.any(), "flags: {:?}", turns[0].flags);
    }

    /// A mic block genuinely spanning the bot onset is double talk and the
    /// latency stays undefined.
    #[test]
    fn overlapping_close_marks_double_talk() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 3000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 3500 * MS }, &mut turns, &mut itr);
        assert_eq!(turns[0].latency_ms, None);
        assert!(turns[0].flags.double_talk);
        assert!(!turns[0].flags.barge_in);
    }

    /// A short mic blip during bot audio (speaker echo) must not leave a
    /// dangling interruption record or flag the next turn as a barge-in.
    #[test]
    fn short_echo_blip_retracts_interruption_and_barge() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 0 }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1500 * MS }, &mut turns, &mut itr);
        // 145ms echo blip during bot audio: recorded then retracted on close.
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 4950 * MS }, &mut turns, &mut itr);
        assert_eq!(itr.len(), 1);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 5095 * MS }, &mut turns, &mut itr);
        assert!(itr.is_empty(), "blip interruption should retract: {itr:?}");
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 13962 * MS }, &mut turns, &mut itr);
        assert!(itr.is_empty(), "sys close must not resurrect it");
        // Real user turn after the bot finished: no barge-in.
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 15069 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 15527 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 17161 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].latency_ms, Some(17161.0 - 15527.0));
        assert!(!turns[1].flags.barge_in, "flags: {:?}", turns[1].flags);
        assert!(!turns[1].flags.double_talk);
    }

    /// A real barge (long enough) keeps its interruption record and flags
    /// the turn it triggers.
    #[test]
    fn real_barge_keeps_record_and_flag() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 3000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Close { t_ns: 3400 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 4500 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 5200 * MS }, &mut turns, &mut itr);
        assert_eq!(itr.len(), 1);
        assert_eq!(itr[0].stop_ms, Some(400.0));
        assert!(turns[1].flags.barge_in);
        assert_eq!(turns[1].latency_ms, Some(700.0));
    }

    #[test]
    fn cancelled_bot_blip_retracts_turn() {
        let mut tr = TurnTracker::new();
        let mut turns = Vec::new();
        let mut itr = Vec::new();
        ev(&mut tr, LANE_MIC, SegEvent::Open { t_ns: 0 }, &mut turns, &mut itr);
        ev(&mut tr, LANE_MIC, SegEvent::Close { t_ns: 1000 * MS }, &mut turns, &mut itr);
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 1500 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 1);
        ev(&mut tr, LANE_SYS, SegEvent::Cancel, &mut turns, &mut itr);
        assert_eq!(turns.len(), 0);
        // Real response afterwards pairs with the same user block.
        ev(&mut tr, LANE_SYS, SegEvent::Open { t_ns: 2000 * MS }, &mut turns, &mut itr);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].latency_ms, Some(1000.0));
    }
}
