use super::{Event, HookRequest};
use crate::attribution::{Agent, AgentState, AttributionSnapshot, MemorySummary};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

struct Session {
    kind: String,
    id: String,
    cwd: Option<String>,
    state: AgentState,
    seen: Instant,
}
#[derive(Default)]
pub struct HookState {
    sessions: HashMap<String, Session>,
    pending_labels: HashMap<String, VecDeque<(Instant, String)>>,
    labels: HashMap<String, String>,
    workloads: HashSet<String>,
}
impl HookState {
    pub fn receive(&mut self, request: &HookRequest, now: Instant) {
        let state = match request.event {
            Event::SessionStart | Event::Stop | Event::SessionEnd => AgentState::Idle,
            Event::UserPromptSubmit | Event::PreToolUse | Event::PostToolUse => {
                AgentState::Thinking
            }
        };
        self.sessions.insert(
            request.key(),
            Session {
                kind: request.agent.as_str().into(),
                id: request.session_id.clone(),
                cwd: request.cwd.clone(),
                state,
                seen: now,
            },
        );
        if request.event == Event::PreToolUse {
            if let Some(command) = request.shell_command() {
                self.pending_labels
                    .entry(request.key())
                    .or_default()
                    .push_back((now, command.to_owned()));
            }
        }
    }
    pub fn apply(&mut self, snapshot: &mut AttributionSnapshot, now: Instant) {
        for labels in self.pending_labels.values_mut() {
            labels
                .retain(|(then, _)| now.saturating_duration_since(*then) <= Duration::from_secs(2));
        }
        self.pending_labels.retain(|_, labels| !labels.is_empty());
        let keys: HashMap<_, _> = snapshot
            .agents
            .iter()
            .filter_map(|agent| {
                agent
                    .session_id
                    .as_ref()
                    .map(|id| (agent.id.clone(), format!("{}:{id}", agent.kind)))
            })
            .collect();
        let present: HashSet<_> = keys.values().cloned().collect();
        self.sessions.retain(|key, session| {
            present.contains(key)
                || now.saturating_duration_since(session.seen) < Duration::from_secs(30)
        });
        for agent in &mut snapshot.agents {
            if let Some(session) = keys.get(&agent.id).and_then(|key| self.sessions.get(key)) {
                // Root disappearance stays authoritative. SessionEnd (including /clear) is only a hint.
                if agent.state != AgentState::Ended {
                    agent.state = session.state;
                }
                if agent.cwd.is_none() {
                    agent.cwd.clone_from(&session.cwd);
                }
            }
        }
        for (key, session) in &self.sessions {
            if !present.contains(key) {
                snapshot.agents.push(Agent {
                    id: format!("hook:{key}"),
                    owner_id: None,
                    session_id: Some(session.id.clone()),
                    kind: session.kind.clone(),
                    root: None,
                    cwd: session.cwd.clone(),
                    state: session.state,
                    ended_at_ms: None,
                    memory: MemorySummary::default(),
                });
            }
        }
        for workload in &mut snapshot.workloads {
            if !self.workloads.contains(&workload.id) {
                if let Some(queue) = keys
                    .get(&workload.agent_id)
                    .and_then(|key| self.pending_labels.get_mut(key))
                {
                    if let Some((_, label)) = queue.pop_front() {
                        self.labels.insert(workload.id.clone(), label);
                    }
                }
            }
            if let Some(label) = self.labels.get(&workload.id) {
                workload.label.clone_from(label);
            }
        }
        self.workloads = snapshot.workloads.iter().map(|w| w.id.clone()).collect();
        self.labels.retain(|id, _| self.workloads.contains(id));
    }
}
