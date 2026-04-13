//! `JsonSession` — manages a non-interactive Claude Code process that
//! emits `stream-json` JSONL on stdout.
//!
//! Replaces `PtySession` for Claude Code sessions. Instead of an
//! interactive PTY, each "turn" is a separate `claude -p "prompt"
//! --output-format stream-json --session-id <id>` invocation. The
//! `--session-id` flag gives Claude conversation continuity across
//! turns so the user can have a multi-turn conversation.
//!
//! Concurrency model:
//!   - A background reader thread reads stdout line-by-line, parses
//!     each line via `stream_json::parse_line`, and pushes the result
//!     into a bounded `crossbeam_channel`.
//!   - The UI thread calls `poll()` on a Slint timer (~60 Hz) which
//!     drains the channel and accumulates events into the `items` vec.

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, bounded};

use super::stream_json::{ChatItem, ParsedEvent, parse_line};
use crate::kanban::SessionState;

/// Non-PTY session backed by Claude Code's `stream-json` output.
#[allow(dead_code)]
pub struct JsonSession {
    /// Background child process (current turn). `None` when between
    /// turns (awaiting user's next prompt).
    child: Option<Child>,
    /// Channel receiving parsed events from the reader thread.
    rx: Receiver<ParsedEvent>,
    /// Flat chat history — the model that drives the Slint ChatView.
    pub items: Vec<ChatItem>,
    /// Claude session ID, captured from the first event that carries one.
    pub session_id: Option<String>,
    /// Current session lifecycle state.
    pub state: SessionState,
    /// Accumulated tool input JSON for the current tool_use block.
    pending_tool_input: String,
    /// Name of the tool being accumulated.
    pending_tool_name: String,
    /// Working directory for spawned processes.
    cwd: PathBuf,
    /// Path to the `claude` binary.
    binary: PathBuf,
    /// Permission flags (e.g. `["--permission-mode", "acceptEdits", ...]`).
    permission_argv: Vec<String>,
    /// Total cost across all turns.
    pub total_cost_usd: f64,
    /// Cumulative token counts.
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl JsonSession {
    /// Create a new session. Does NOT spawn a process — call
    /// [`send_prompt`] to start the first turn.
    pub fn new(binary: PathBuf, cwd: PathBuf, permission_argv: Vec<String>) -> Self {
        let (_tx, rx) = bounded::<ParsedEvent>(1);
        Self {
            child: None,
            rx,
            items: Vec::new(),
            session_id: None,
            state: SessionState::Idle,
            pending_tool_input: String::new(),
            pending_tool_name: String::new(),
            cwd,
            binary,
            permission_argv,
            total_cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// Start a new turn by spawning `claude -p "prompt" --output-format
    /// stream-json [--session-id <id>]`.
    pub fn send_prompt(&mut self, prompt: &str) -> Result<()> {
        // Add the user prompt to the chat history.
        self.items.push(ChatItem::UserPrompt(prompt.to_string()));
        self.state = SessionState::Busy;

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p").arg(prompt);
        cmd.arg("--output-format").arg("stream-json");

        // Reuse session_id for conversation continuity.
        if let Some(ref id) = self.session_id {
            cmd.arg("--session-id").arg(id);
        }

        // Permission flags.
        for arg in &self.permission_argv {
            cmd.arg(arg);
        }

        cmd.current_dir(&self.cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        // Baseline terminal env.
        cmd.env("TERM", "dumb");
        cmd.env(
            "LANG",
            std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
        );

        let mut child = cmd.spawn().context("failed to spawn claude")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture claude stdout")?;

        let (tx, rx) = bounded::<ParsedEvent>(4096);
        self.rx = rx;
        self.child = Some(child);

        // Background reader thread — reads stdout line-by-line.
        thread::Builder::new()
            .name("quay-json-reader".into())
            .spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                for line_result in reader.lines() {
                    let Ok(line) = line_result else { break };
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(event) = parse_line(&line)
                        && tx.send(event).is_err()
                    {
                        break;
                    }
                }
            })
            .context("failed to spawn reader thread")?;

        Ok(())
    }

    /// Drain pending events from the reader channel and accumulate them
    /// into `self.items`. Returns `true` if anything changed (the UI
    /// should refresh).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;

        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            match event {
                ParsedEvent::SessionId(id) => {
                    if self.session_id.is_none() {
                        self.session_id = Some(id);
                    }
                }
                ParsedEvent::TextDelta(text) => {
                    // Append to the last AssistantText item, or create one.
                    match self.items.last_mut() {
                        Some(ChatItem::AssistantText(s)) => s.push_str(&text),
                        _ => self.items.push(ChatItem::AssistantText(text)),
                    }
                }
                ParsedEvent::ToolUseStart { name, .. } => {
                    self.pending_tool_name = name;
                    self.pending_tool_input.clear();
                }
                ParsedEvent::InputJsonDelta(json) => {
                    self.pending_tool_input.push_str(&json);
                }
                ParsedEvent::ContentBlockStop { .. } => {
                    // If we were accumulating a tool_use, finalise it.
                    if !self.pending_tool_name.is_empty() {
                        self.items.push(ChatItem::ToolUse {
                            name: std::mem::take(&mut self.pending_tool_name),
                            input: std::mem::take(&mut self.pending_tool_input),
                        });
                    }
                }
                ParsedEvent::Result {
                    session_id,
                    is_error,
                    total_cost_usd,
                    input_tokens,
                    output_tokens,
                    num_turns,
                } => {
                    if self.session_id.is_none() && !session_id.is_empty() {
                        self.session_id = Some(session_id);
                    }
                    self.total_cost_usd += total_cost_usd;
                    self.input_tokens += input_tokens;
                    self.output_tokens += output_tokens;

                    let status = if is_error {
                        "Error during execution".to_string()
                    } else {
                        format!(
                            "{num_turns} turn(s) · ${total_cost_usd:.4} · \
                             {input_tokens} in / {output_tokens} out"
                        )
                    };
                    self.items.push(ChatItem::Status(status));
                    self.state = if is_error {
                        SessionState::Error
                    } else {
                        SessionState::Awaiting
                    };
                }
                ParsedEvent::ToolResultStart { content, is_error } => {
                    self.items.push(ChatItem::ToolResult {
                        output: content,
                        is_error,
                    });
                }
                ParsedEvent::ThinkingDelta(_) | ParsedEvent::Unknown => {}
            }
        }

        // Check if child process exited (without a result event).
        if self.state == SessionState::Busy
            && let Some(ref mut child) = self.child
            && let Ok(Some(_status)) = child.try_wait()
        {
            self.state = SessionState::Awaiting;
        }

        changed
    }

    /// Whether the child process has exited.
    #[allow(dead_code)]
    pub fn is_exited(&mut self) -> bool {
        match self.child {
            Some(ref mut c) => matches!(c.try_wait(), Ok(Some(_))),
            None => true,
        }
    }

    /// OS PID of the current child process (for process tracking).
    #[allow(dead_code)]
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Kill the running child process.
    #[allow(dead_code)]
    pub fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
        self.state = SessionState::Stopped;
    }
}

impl Drop for JsonSession {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
