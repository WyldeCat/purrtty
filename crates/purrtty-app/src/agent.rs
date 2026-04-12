//! Spawns the Claude CLI with streaming JSON output and parses events.
//!
//! `AgentSession::spawn` runs `claude -p "<prompt>"` with
//! `--output-format stream-json` so we get structured events (text
//! deltas, tool use, results) instead of raw markdown. Each event is
//! parsed into an `AgentOutput` carrying both ANSI-formatted terminal
//! text and a `BlockUpdate` for the block state machine.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, warn};

/// What a single stream-json event produces.
pub struct AgentOutput {
    /// ANSI text to echo into the terminal grid. `None` for events
    /// that don't produce visible output.
    pub text: Option<String>,
    /// Structured update for the Block state machine.
    pub update: BlockUpdate,
}

/// Structured block update extracted from a stream-json event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockUpdate {
    None,
    TextDelta(String),
    ToolStart { name: String },
    ToolInput(String),
    ContentBlockStop,
}

/// A running Claude CLI session. Call [`AgentSession::kill`] to abort.
pub struct AgentSession {
    child: Option<Child>,
    _reader_thread: JoinHandle<()>,
}

impl AgentSession {
    /// Spawn `claude -p <prompt>` with streaming JSON output and full
    /// tool access. Parsed events are delivered via `on_event`.
    /// `on_exit` fires after the process terminates.
    pub fn spawn<F, G>(prompt: &str, cwd: &Path, on_event: F, on_exit: G) -> Result<Self>
    where
        F: FnMut(AgentOutput) + Send + 'static,
        G: FnOnce(i32) + Send + 'static,
    {
        debug!(?cwd, "spawning claude agent (stream-json)");

        let mut child = Command::new("claude")
            .args([
                "-p",
                prompt,
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--allowedTools",
                "Bash,Read,Edit,Write,Grep,Glob",
            ])
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("spawn claude CLI — is `claude` in $PATH?")?;

        let stdout = child.stdout.take().context("take claude stdout")?;
        let stderr = child.stderr.take().context("take claude stderr")?;

        let reader_thread = thread::Builder::new()
            .name("purrtty-agent-reader".into())
            .spawn(move || {
                Self::reader_loop(stdout, stderr, on_event, on_exit);
            })
            .context("spawn agent reader thread")?;

        Ok(Self {
            child: Some(child),
            _reader_thread: reader_thread,
        })
    }

    /// Ask the child to terminate. Best-effort; does not block.
    pub fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }

    fn reader_loop<F, G>(
        stdout: impl Read,
        mut stderr: impl Read,
        mut on_event: F,
        on_exit: G,
    ) where
        F: FnMut(AgentOutput),
        G: FnOnce(i32),
    {
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(err) => {
                    warn!(?err, "agent stdout read error");
                    break;
                }
            };
            if line.is_empty() {
                continue;
            }
            let json: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let output = parse_event(&json);
            if output.text.is_some() || output.update != BlockUpdate::None {
                on_event(output);
            }
        }

        // Drain stderr and log.
        let mut stderr_buf = Vec::new();
        if let Ok(n) = stderr.read_to_end(&mut stderr_buf) {
            if n > 0 {
                if let Ok(s) = std::str::from_utf8(&stderr_buf) {
                    for line in s.lines() {
                        debug!(line, "agent stderr");
                    }
                }
            }
        }

        on_exit(0);
    }
}

/// Parse a stream-json line into an `AgentOutput` carrying both
/// formatted terminal text and a structured block update.
fn parse_event(json: &Value) -> AgentOutput {
    let none = AgentOutput {
        text: None,
        update: BlockUpdate::None,
    };
    let Some(event_type) = json.get("type").and_then(|t| t.as_str()) else {
        return none;
    };

    match event_type {
        "stream_event" => {
            let Some(event) = json.get("event") else {
                return none;
            };
            let event_kind = event
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("");

            match event_kind {
                "content_block_delta" => {
                    let Some(delta) = event.get("delta") else {
                        return none;
                    };
                    let delta_type = delta.get("type").and_then(|t| t.as_str());
                    match delta_type {
                        Some("text_delta") => {
                            let raw = delta
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            let ansi = raw.replace('\n', "\r\n");
                            AgentOutput {
                                text: Some(ansi),
                                update: BlockUpdate::TextDelta(raw.to_string()),
                            }
                        }
                        Some("input_json_delta") => {
                            let partial = delta
                                .get("partial_json")
                                .and_then(|p| p.as_str())
                                .unwrap_or("");
                            AgentOutput {
                                text: Some(format!("\x1b[2m{}\x1b[0m", partial)),
                                update: BlockUpdate::ToolInput(partial.to_string()),
                            }
                        }
                        _ => none,
                    }
                }

                "content_block_start" => {
                    let Some(block) = event.get("content_block") else {
                        return none;
                    };
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let tool_name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        AgentOutput {
                            text: Some(format!(
                                "\r\n\x1b[1;33m⚡ {}\x1b[0m ",
                                tool_name
                            )),
                            update: BlockUpdate::ToolStart {
                                name: tool_name,
                            },
                        }
                    } else {
                        none
                    }
                }

                "content_block_stop" => AgentOutput {
                    text: Some("\r\n".to_string()),
                    update: BlockUpdate::ContentBlockStop,
                },

                _ => none,
            }
        }

        "system" => {
            let subtype = json.get("subtype").and_then(|s| s.as_str());
            if subtype == Some("init") {
                if let Some(sid) = json.get("session_id").and_then(|s| s.as_str()) {
                    debug!(session_id = sid, "agent session started");
                }
            }
            none
        }

        "result" | "assistant" | "rate_limit_event" => none,
        _ => none,
    }
}
