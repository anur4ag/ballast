mod admission;
mod classify;
mod protection;
mod state;
#[cfg(test)]
mod tests;
pub use admission::Admission;
pub use classify::Classifier;
pub use protection::{HookEvidence, PortOwners, lookup};
pub use state::HookState;

use crate::daemon::{
    files::Paths,
    ipc::{Client, Method, Reply},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
}
impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    PreToolUse,
    PostToolUse,
    SessionStart,
    UserPromptSubmit,
    Stop,
    SessionEnd,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookRequest {
    pub agent: AgentKind,
    pub session_id: String,
    pub event: Event,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub tool_name: Option<String>,
    pub tool_response: Option<Value>,
}
impl HookRequest {
    pub fn parse(agent: AgentKind, value: Value) -> Option<Self> {
        let session_id = value.get("session_id")?.as_str()?.to_owned();
        if session_id.is_empty() {
            return None;
        }
        let event = serde_json::from_value(value.get("hook_event_name")?.clone()).ok()?;
        Some(Self {
            agent,
            session_id,
            event,
            cwd: value.get("cwd").and_then(Value::as_str).map(str::to_owned),
            command: value
                .pointer("/tool_input/command")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool_name: value
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tool_response: value.get("tool_response").cloned(),
        })
    }
    pub fn shell_command(&self) -> Option<&str> {
        matches!(
            self.tool_name.as_deref(),
            Some("Bash" | "Monitor" | "PowerShell")
        )
        .then_some(self.command.as_deref())
        .flatten()
    }
    pub fn key(&self) -> String {
        format!("{}:{}", self.agent.as_str(), self.session_id)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Admit,
    Hold,
    Deny { reason: String },
    Hint { context: String },
}

/// The caller exits the hook process after this returns, including any blocked I/O worker.
/// The outer deadline covers stdin, partial socket frames, and every other I/O operation.
pub fn run(agent: AgentKind, timeout_seconds: u64) {
    let started = Instant::now();
    let budget = Duration::from_secs(timeout_seconds.saturating_sub(10).min(590));
    if budget.is_zero() {
        return;
    }
    let (send, receive) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("hook-io".into())
        .spawn(move || {
            let result = (|| -> io::Result<()> {
                let mut input = Vec::new();
                io::stdin().take(64 * 1024 + 1).read_to_end(&mut input)?;
                if input.len() > 64 * 1024 {
                    return Ok(());
                }
                let value = serde_json::from_slice(&input)?;
                let Some(request) = HookRequest::parse(agent, value) else {
                    return Ok(());
                };
                let pre = request.event == Event::PreToolUse;
                let post = request.event == Event::PostToolUse;
                let mut client = Client::connect(&Paths::from_env()?, Duration::from_millis(50))?;
                client.set_timeout(budget)?;
                client.send(Method::Hook {
                    payload: serde_json::to_value(request)?,
                })?;
                let mut held = false;
                loop {
                    let Reply::Hook { decision } = client.receive()?.reply else {
                        return Ok(());
                    };
                    if matches!(decision, HookDecision::Hold) {
                        if !pre || held {
                            return Ok(());
                        }
                        held = true;
                    } else if !pre && matches!(decision, HookDecision::Deny { .. }) {
                        return Ok(());
                    }
                    if !post && matches!(decision, HookDecision::Hint { .. }) {
                        return Ok(());
                    }
                    let done = !matches!(decision, HookDecision::Hold);
                    if send.send(decision).is_err() || done {
                        return Ok(());
                    }
                }
            })();
            let _ = result; // Every failure is silence, including malformed daemon responses.
        });
    if worker.is_err() {
        return;
    }
    let Ok(mut decision) = receive.recv_timeout(
        budget
            .min(Duration::from_millis(200))
            .saturating_sub(started.elapsed()),
    ) else {
        return;
    };
    if decision == HookDecision::Hold {
        let Ok(final_decision) = receive.recv_timeout(budget.saturating_sub(started.elapsed()))
        else {
            return;
        };
        decision = final_decision;
    }
    let output = match decision {
        HookDecision::Deny { reason } => serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason
        }}),
        HookDecision::Hint { context } => serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PostToolUse", "additionalContext": context
        }}),
        _ => return,
    };
    let _ = writeln!(io::stdout(), "{output}");
}
