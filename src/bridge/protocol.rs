//! Bridge wire protocol. Anything that can open a WebSocket can feed events:
//!
//! ```json
//! {"v":1, "type":"event", "name":"user_stopped_speaking", "source":"pipecat", "meta":{...}}
//! ```
//!
//! Unknown names are accepted and rendered generically. For compatibility,
//! raw RTVI-style messages (`{"type":"user-started-speaking", ...}`) are also
//! accepted: the type becomes the event name. `{"type":"ping"}` gets a pong
//! with the server receive time (ms since app launch).

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct InboundMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

pub enum Parsed {
    Event {
        name: String,
        source: String,
        meta: serde_json::Value,
    },
    Ping,
    Hello,
    Ignored,
}

/// Normalize event names: RTVI uses kebab-case, Pipecat frames use CamelCase;
/// we canonicalize to snake_case.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower = false;
    for c in name.chars() {
        match c {
            '-' | ' ' | '.' => {
                if !out.ends_with('_') {
                    out.push('_');
                }
                prev_lower = false;
            }
            c if c.is_uppercase() => {
                if prev_lower && !out.ends_with('_') {
                    out.push('_');
                }
                out.extend(c.to_lowercase());
                prev_lower = false;
            }
            c => {
                out.push(c);
                prev_lower = c.is_lowercase() || c.is_numeric();
            }
        }
    }
    out
}

pub fn parse(text: &str) -> Parsed {
    let Ok(msg) = serde_json::from_str::<InboundMsg>(text) else {
        return Parsed::Ignored;
    };
    match msg.msg_type.as_str() {
        "event" => {
            let Some(name) = msg.name else {
                return Parsed::Ignored;
            };
            Parsed::Event {
                name: normalize_name(&name),
                source: msg.source.unwrap_or_else(|| "bridge".into()),
                meta: msg.meta.unwrap_or(serde_json::Value::Null),
            }
        }
        "ping" => Parsed::Ping,
        "hello" => Parsed::Hello,
        // RTVI raw message compatibility: treat the type as the event name.
        other if !other.is_empty() => Parsed::Event {
            name: normalize_name(other),
            source: msg.source.unwrap_or_else(|| "rtvi-raw".into()),
            meta: msg.meta.unwrap_or(serde_json::Value::Null),
        },
        _ => Parsed::Ignored,
    }
}

/// Category used for marker colors and filtering in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCategory {
    User,
    Bot,
    Tts,
    Llm,
    Stt,
    Metrics,
    Other,
}

impl EventCategory {
    pub const ALL: [EventCategory; 7] = [
        EventCategory::User,
        EventCategory::Bot,
        EventCategory::Tts,
        EventCategory::Llm,
        EventCategory::Stt,
        EventCategory::Metrics,
        EventCategory::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EventCategory::User => "User",
            EventCategory::Bot => "Bot",
            EventCategory::Tts => "TTS",
            EventCategory::Llm => "LLM",
            EventCategory::Stt => "STT / transcripts",
            EventCategory::Metrics => "Metrics",
            EventCategory::Other => "Other",
        }
    }
}

pub fn categorize(name: &str) -> EventCategory {
    // Metrics first: names like `llm_metrics` / `tts_metrics` are metrics
    // about a service, not service lifecycle events — they must follow the
    // Metrics filter toggle.
    if name.contains("metric") {
        EventCategory::Metrics
    } else if name.starts_with("user_") {
        EventCategory::User
    } else if name.contains("tts") {
        EventCategory::Tts
    } else if name.contains("llm") {
        EventCategory::Llm
    } else if name.contains("stt") || name.contains("transcript") {
        EventCategory::Stt
    } else if name.starts_with("bot_") {
        EventCategory::Bot
    } else {
        EventCategory::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_name("user-started-speaking"), "user_started_speaking");
        assert_eq!(normalize_name("UserStoppedSpeakingFrame"), "user_stopped_speaking_frame");
        assert_eq!(normalize_name("bot_started_speaking"), "bot_started_speaking");
        assert_eq!(normalize_name("botTtsStarted"), "bot_tts_started");
    }

    #[test]
    fn parses_event() {
        let p = parse(r#"{"v":1,"type":"event","name":"user-stopped-speaking","meta":{"x":1}}"#);
        match p {
            Parsed::Event { name, .. } => assert_eq!(name, "user_stopped_speaking"),
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn metrics_win_over_service_substrings() {
        assert_eq!(categorize("metrics"), EventCategory::Metrics);
        assert_eq!(categorize("llm_metrics"), EventCategory::Metrics);
        assert_eq!(categorize("tts_metrics"), EventCategory::Metrics);
        assert_eq!(categorize("bot_llm_started"), EventCategory::Llm);
        assert_eq!(categorize("user_stopped_speaking"), EventCategory::User);
    }

    #[test]
    fn rtvi_raw_compat() {
        let p = parse(r#"{"type":"bot-started-speaking"}"#);
        match p {
            Parsed::Event { name, source, .. } => {
                assert_eq!(name, "bot_started_speaking");
                assert_eq!(source, "rtvi-raw");
            }
            _ => panic!("expected event"),
        }
    }
}
