//! Parser for Claude Code `--output-format stream-json` JSONL events.
//!
//! Each line of stdout is a self-contained JSON object. We classify them
//! into a small set of [`ParsedEvent`] variants that the session manager
//! can accumulate into displayable [`ChatItem`]s.
//!
//! Unknown or unrecognised events are silently discarded so that new
//! event types added by future Claude Code versions don't crash Quay.

use serde_json::Value;

// ── Public display model ────────────────────────────────────────────

/// One item in the chat conversation — the flat model that Slint renders.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ChatItem {
    /// The user's prompt.
    UserPrompt(String),
    /// Accumulated assistant text (grows as deltas arrive).
    AssistantText(String),
    /// A tool invocation (name + serialised input).
    ToolUse { name: String, input: String },
    /// The result returned by a tool execution.
    ToolResult { output: String, is_error: bool },
    /// A lifecycle status line (session started, cost summary, error).
    Status(String),
}

// ── Internal event enum ─────────────────────────────────────────────

/// Parsed event from one JSONL line. Drives the accumulation logic in
/// [`super::json_session::JsonSession::poll`].
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ParsedEvent {
    /// Session ID captured from any event envelope.
    SessionId(String),
    /// A chunk of streaming assistant text.
    TextDelta(String),
    /// Start of a tool_use content block.
    ToolUseStart { id: String, name: String },
    /// A chunk of the tool's input JSON (accumulated).
    InputJsonDelta(String),
    /// A tool_result content block with its text output.
    ToolResultStart { content: String, is_error: bool },
    /// A content block finished.
    ContentBlockStop { index: u64 },
    /// The agent turn finished — includes summary stats.
    Result {
        session_id: String,
        is_error: bool,
        total_cost_usd: f64,
        input_tokens: u64,
        output_tokens: u64,
        num_turns: u32,
    },
    /// Thinking delta — extended thinking text chunk.
    ThinkingDelta(String),
    /// Anything we don't recognise — logged, not displayed.
    Unknown,
}

// ── Parser ──────────────────────────────────────────────────────────

/// Parse a single JSONL line into a [`ParsedEvent`].
/// Returns `None` only for completely unparseable lines (not valid JSON).
pub fn parse_line(line: &str) -> Option<ParsedEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let top_type = v.get("type")?.as_str()?;

    // Capture session_id from any event that carries it.
    let session_id = v
        .get("session_id")
        .and_then(|s| s.as_str())
        .map(String::from);

    match top_type {
        "stream_event" => {
            let evt = parse_stream_event(v.get("event")?);
            // If this event itself is Unknown but we got a session_id,
            // still emit the SessionId event.
            match (&evt, &session_id) {
                (ParsedEvent::Unknown, Some(id)) => Some(ParsedEvent::SessionId(id.clone())),
                _ => {
                    // If we haven't captured a session_id via a
                    // SessionId event yet, and this event has one,
                    // the caller will handle it by checking the
                    // session_id on the envelope.
                    Some(evt)
                }
            }
        }
        "assistant" => {
            // Complete assistant message — we already handle streaming
            // deltas so we only care about the session_id here.
            session_id.map(ParsedEvent::SessionId)
        }
        "result" => {
            let sid = session_id.unwrap_or_default();
            Some(ParsedEvent::Result {
                session_id: sid,
                is_error: v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false),
                total_cost_usd: v
                    .get("total_cost_usd")
                    .and_then(|n| n.as_f64())
                    .unwrap_or(0.0),
                input_tokens: v
                    .pointer("/usage/input_tokens")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0),
                output_tokens: v
                    .pointer("/usage/output_tokens")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0),
                num_turns: v
                    .get("num_turns")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32,
            })
        }
        "system" => session_id.map(ParsedEvent::SessionId),
        _ => Some(ParsedEvent::Unknown),
    }
}

/// Parse the nested `event` object inside a `stream_event` envelope.
fn parse_stream_event(event: &Value) -> ParsedEvent {
    let Some(event_type) = event.get("type").and_then(|t| t.as_str()) else {
        return ParsedEvent::Unknown;
    };
    match event_type {
        "content_block_start" => {
            let block = match event.get("content_block") {
                Some(b) => b,
                None => return ParsedEvent::Unknown,
            };
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match block_type {
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    ParsedEvent::ToolUseStart { id, name }
                }
                "tool_result" => {
                    // Extract text from the content array.
                    let content = block
                        .get("content")
                        .and_then(|c| c.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| {
                                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        item.get("text").and_then(|t| t.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    let is_error = block
                        .get("is_error")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    ParsedEvent::ToolResultStart { content, is_error }
                }
                _ => ParsedEvent::Unknown,
            }
        }
        "content_block_delta" => {
            let delta = match event.get("delta") {
                Some(d) => d,
                None => return ParsedEvent::Unknown,
            };
            let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match delta_type {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    ParsedEvent::TextDelta(text)
                }
                "input_json_delta" => {
                    let json = delta
                        .get("partial_json")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    ParsedEvent::InputJsonDelta(json)
                }
                "thinking_delta" => {
                    let thinking = delta
                        .get("thinking")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    ParsedEvent::ThinkingDelta(thinking)
                }
                _ => ParsedEvent::Unknown,
            }
        }
        "content_block_stop" => {
            let index = event.get("index").and_then(|n| n.as_u64()).unwrap_or(0);
            ParsedEvent::ContentBlockStop { index }
        }
        "message_start" | "message_delta" | "message_stop" | "ping" => ParsedEvent::Unknown,
        _ => ParsedEvent::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let line = r#"{"type":"stream_event","session_id":"s1","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#;
        match parse_line(line) {
            Some(ParsedEvent::TextDelta(t)) => assert_eq!(t, "Hello"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_start() {
        let line = r#"{"type":"stream_event","session_id":"s1","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu1","name":"Read"}}}"#;
        match parse_line(line) {
            Some(ParsedEvent::ToolUseStart { id, name }) => {
                assert_eq!(id, "tu1");
                assert_eq!(name, "Read");
            }
            other => panic!("expected ToolUseStart, got {other:?}"),
        }
    }

    #[test]
    fn parse_result() {
        let line = r#"{"type":"result","session_id":"s1","is_error":false,"total_cost_usd":0.01,"usage":{"input_tokens":100,"output_tokens":50},"num_turns":2}"#;
        match parse_line(line) {
            Some(ParsedEvent::Result {
                session_id,
                is_error,
                total_cost_usd,
                input_tokens,
                output_tokens,
                num_turns,
            }) => {
                assert_eq!(session_id, "s1");
                assert!(!is_error);
                assert!((total_cost_usd - 0.01).abs() < 1e-6);
                assert_eq!(input_tokens, 100);
                assert_eq!(output_tokens, 50);
                assert_eq!(num_turns, 2);
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_line("not json").is_none());
    }

    #[test]
    fn parse_unknown_type_returns_unknown() {
        let line = r#"{"type":"future_event","data":123}"#;
        match parse_line(line) {
            Some(ParsedEvent::Unknown) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
