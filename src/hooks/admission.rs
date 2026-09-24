use super::{Classifier, Event, HookDecision, HookRequest, HookState};
use crate::daemon::{
    Snapshot,
    files::{Mode, RotatingLog},
    ipc::{PendingRequest, Reply, Response},
};
use crate::guardian::Level;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

struct Held {
    request: HookRequest,
    connection: PendingRequest,
    since: Instant,
    wall: SystemTime,
}
pub struct Admission {
    classifier: Classifier,
    queues: VecDeque<(String, VecDeque<Held>)>,
    last_admit: Option<Instant>,
}
impl Admission {
    pub fn new(additions: &[String]) -> Self {
        Self {
            classifier: Classifier::new(additions),
            queues: VecDeque::new(),
            last_admit: None,
        }
    }
    pub fn handle(
        &mut self,
        request: HookRequest,
        connection: PendingRequest,
        snapshot: &Snapshot,
        state: &mut HookState,
        log: &mut RotatingLog,
        now: Instant,
    ) {
        if let Some(reason) = super::protection::deny(&request, snapshot, &connection.evidence) {
            record(log, "deny", &request, snapshot, &reason, 0);
            if matches!(snapshot.status.mode, Mode::Enforce) {
                let _ = connection.reply.send(Response::new(Reply::Hook {
                    decision: HookDecision::Deny { reason },
                }));
                return;
            }
        }
        if let Some(context) = super::protection::hint(&request, snapshot, &connection.evidence) {
            record(log, "hint", &request, snapshot, &context, 0);
            state.receive(&request, now);
            let _ = connection.reply.send(Response::new(Reply::Hook {
                decision: HookDecision::Hint { context },
            }));
            return;
        }
        let heavy = request.event == Event::PreToolUse
            && request
                .shell_command()
                .is_some_and(|command| self.classifier.heavy(command));
        let known = snapshot.pressure.is_some() && !snapshot.status.sample_discarded;
        let hold = heavy
            && known
            && snapshot.status.pressure_level != Level::Normal
            && snapshot.status.batch_running;
        if hold {
            let key = request.key();
            let index = self
                .queues
                .iter()
                .position(|(agent, _)| *agent == key)
                .unwrap_or(self.queues.len());
            let round = self.queues.get(index).map_or(0, |(_, queue)| queue.len());
            let position = 1 + self
                .queues
                .iter()
                .enumerate()
                .map(|(i, (_, queue))| queue.len().min(round + usize::from(i < index)))
                .sum::<usize>();
            record(log, "hold", &request, snapshot, "memory pressure", position);
            if matches!(snapshot.status.mode, Mode::Enforce) {
                if connection
                    .reply
                    .send(Response::new(Reply::Hook {
                        decision: HookDecision::Hold,
                    }))
                    .is_err()
                {
                    return;
                }
                let held = Held {
                    request,
                    connection,
                    since: now,
                    wall: SystemTime::now(),
                };
                if let Some((_, queue)) = self.queues.iter_mut().find(|(agent, _)| *agent == key) {
                    queue.push_back(held);
                } else {
                    self.queues.push_back((key, VecDeque::from([held])));
                }
                return;
            }
        }
        state.receive(&request, now);
        if request.event == Event::PreToolUse {
            record(
                log,
                "admit",
                &request,
                snapshot,
                if hold { "observe mode" } else { "immediate" },
                0,
            );
        }
        let _ = connection.reply.send(Response::new(Reply::Hook {
            decision: HookDecision::Admit,
        }));
    }
    pub fn tick(
        &mut self,
        snapshot: &Snapshot,
        state: &mut HookState,
        log: &mut RotatingLog,
        now: Instant,
    ) {
        for (_, queue) in &mut self.queues {
            queue.retain(|held| !held.connection.cancelled.load(Ordering::Relaxed));
        }
        self.queues.retain(|(_, queue)| !queue.is_empty());
        // Expired requests cannot be starved by another agent's place in the rotation.
        for (_, queue) in &mut self.queues {
            while queue.front().is_some_and(|held| {
                now.saturating_duration_since(held.since) >= Duration::from_secs(300)
                    || held
                        .wall
                        .elapsed()
                        .is_ok_and(|elapsed| elapsed >= Duration::from_secs(300))
            }) {
                let held = queue.pop_front().unwrap();
                admit(held, snapshot, state, log, now, "max hold");
                self.last_admit = Some(now);
            }
        }
        self.queues.retain(|(_, queue)| !queue.is_empty());
        let ready = snapshot.pressure.is_none()
            || snapshot.status.sample_discarded
            || snapshot.status.pressure_level == Level::Normal
            || !snapshot.status.batch_running;
        let cooldown = self
            .last_admit
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5));
        if ready && cooldown {
            if let Some((key, mut queue)) = self.queues.pop_front() {
                admit(
                    queue.pop_front().unwrap(),
                    snapshot,
                    state,
                    log,
                    now,
                    if snapshot.pressure.is_none() || snapshot.status.sample_discarded {
                        "unknown pressure"
                    } else if snapshot.status.pressure_level == Level::Normal {
                        "normal pressure"
                    } else {
                        "no batch running"
                    },
                );
                self.last_admit = Some(now);
                if !queue.is_empty() {
                    self.queues.push_back((key, queue));
                }
            }
        }
    }
}
fn admit(
    held: Held,
    snapshot: &Snapshot,
    state: &mut HookState,
    log: &mut RotatingLog,
    now: Instant,
    reason: &str,
) {
    if held.connection.cancelled.load(Ordering::Relaxed) {
        return;
    }
    if held
        .connection
        .reply
        .send(Response::new(Reply::Hook {
            decision: HookDecision::Admit,
        }))
        .is_ok()
    {
        state.receive(&held.request, now);
        record(log, "admit", &held.request, snapshot, reason, 0);
    }
}
fn record(
    log: &mut RotatingLog,
    event: &str,
    request: &HookRequest,
    snapshot: &Snapshot,
    reason: &str,
    queue_position: usize,
) {
    if let Err(error) = log.decision(
        event,
        serde_json::json!({
            "agent": request.agent, "session_id": request.session_id, "reason": reason,
            "mode": snapshot.status.mode, "pressure_level": snapshot.status.pressure_level,
            "pressure": snapshot.pressure, "batch_running": snapshot.status.batch_running,
            "sampled_at_ms": snapshot.status.sampled_at_ms, "queue_position": queue_position,
        }),
    ) {
        eprintln!("admission decision log: {error}");
    }
}
