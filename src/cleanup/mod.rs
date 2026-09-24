use crate::attribution::{AgentState, ProcessRole, WorkloadClass};
use crate::daemon::{
    Snapshot,
    files::{Mode, RotatingLog},
};
use crate::guardian::Guardian;
use crate::platform::{Platform, ProcessIdentity, ProcessLiveness, Signal};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub observe: bool,
    pub scheduled: Vec<String>,
    pub pending: Vec<String>,
    pub services: Vec<Service>,
    pub orphans: Vec<Orphan>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Service {
    pub workload_id: String,
    pub pids: Vec<i32>,
    pub ports: Vec<u16>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Orphan {
    pub identity: ProcessIdentity,
    pub executable: String,
}

struct Termination {
    agent_id: String,
    workload_id: Option<String>,
    automatic: bool,
    members: HashMap<ProcessIdentity, (Option<Instant>, bool)>,
}

pub struct Cleanup {
    mode: Mode,
    grace: Duration,
    pending: HashMap<String, Termination>,
    reported_services: HashSet<String>,
    counted_services: HashSet<String>,
    reclaimed: Vec<String>,
    last_notification: HashMap<&'static str, Instant>,
    errors: Vec<String>,
}
impl Cleanup {
    pub fn new(mode: Mode, grace: Duration) -> Self {
        Self {
            mode,
            grace,
            pending: HashMap::new(),
            reported_services: HashSet::new(),
            counted_services: HashSet::new(),
            reclaimed: Vec::new(),
            last_notification: HashMap::new(),
            errors: Vec::new(),
        }
    }

    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    pub fn watched(&self) -> HashSet<ProcessIdentity> {
        self.pending
            .values()
            .flat_map(|t| t.members.keys().copied())
            .collect()
    }

    pub fn pending_targets(&self) -> Vec<String> {
        let mut targets: Vec<_> = self.pending.keys().cloned().collect();
        targets.sort_unstable();
        targets
    }

    /// Called only after a successful process scan and current attribution derivation.
    pub fn tick(
        &mut self,
        now: Instant,
        snapshot: &Snapshot,
        guardian: &mut Guardian,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) {
        self.collect(now, snapshot, false, platform, log);
        self.advance(now, snapshot, guardian, platform, log);
    }

    pub fn request(
        &mut self,
        target: Option<&str>,
        snapshot: &Snapshot,
        guardian: &mut Guardian,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<Report> {
        let now = Instant::now();
        let mut report = if let Some(target) = target {
            let agent = snapshot.attribution.agents.iter().any(|a| a.id == target);
            let workloads: Vec<_> = snapshot
                .attribution
                .workloads
                .iter()
                .filter(|w| w.id == target || (agent && w.agent_id == target))
                .collect();
            if !agent && workloads.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "agent or workload not found; use ballast ps for IDs",
                ));
            }
            let mut report = Report {
                observe: matches!(self.mode, Mode::Observe),
                ..Report::default()
            };
            for w in workloads {
                self.schedule(w.id.clone(), w.agent_id.clone(), Some(w.id.clone()), false);
                report.scheduled.push(w.id.clone());
            }
            report
        } else {
            self.collect(now, snapshot, true, platform, log)
        };
        if target.is_none() {
            let unattributed: HashSet<_> = snapshot
                .attribution
                .processes
                .iter()
                .filter(|p| p.role == ProcessRole::Unattributed)
                .map(|p| p.identity)
                .collect();
            report.orphans = snapshot
                .processes
                .iter()
                .filter(|p| p.ppid == 1 && unattributed.contains(&p.identity))
                .filter_map(|p| {
                    let exe = p.exe.as_deref()?.rsplit('/').next()?;
                    matches!(
                        exe,
                        "node"
                            | "nodejs"
                            | "vite"
                            | "chromium"
                            | "chrome"
                            | "Google Chrome"
                            | "cargo"
                            | "rustc"
                            | "npm"
                            | "pnpm"
                            | "tsc"
                            | "webpack"
                    )
                    .then(|| Orphan {
                        identity: p.identity,
                        executable: exe.to_owned(),
                    })
                })
                .collect();
        }
        self.advance(now, snapshot, guardian, platform, log);
        report.pending = self.pending_targets();
        self.record(
            log,
            "cleanup_request",
            serde_json::json!({"target": target, "report": report}),
        );
        Ok(report)
    }

    fn schedule(
        &mut self,
        key: String,
        agent_id: String,
        workload_id: Option<String>,
        automatic: bool,
    ) {
        self.pending
            .entry(key)
            .and_modify(|t| t.automatic &= automatic)
            .or_insert(Termination {
                agent_id,
                workload_id,
                automatic,
                members: HashMap::new(),
            });
    }

    fn collect(
        &mut self,
        now: Instant,
        snapshot: &Snapshot,
        immediate: bool,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> Report {
        let mut report = Report {
            observe: matches!(self.mode, Mode::Observe),
            ..Report::default()
        };
        self.reported_services
            .retain(|id| snapshot.attribution.workloads.iter().any(|w| &w.id == id));
        for agent in &snapshot.attribution.agents {
            if agent.state != AgentState::Ended
                || (!immediate
                    && !agent.ended_at_ms.is_some_and(|ended| {
                        u128::from(snapshot.status.sampled_at_ms.saturating_sub(ended))
                            >= self.grace.as_millis()
                    }))
            {
                continue;
            }
            for w in snapshot
                .attribution
                .workloads
                .iter()
                .filter(|w| w.agent_id == agent.id)
            {
                if w.class == WorkloadClass::Batch {
                    self.schedule(w.id.clone(), agent.id.clone(), Some(w.id.clone()), true);
                    report.scheduled.push(w.id.clone());
                } else {
                    let members: Vec<_> = snapshot
                        .attribution
                        .processes
                        .iter()
                        .filter(|p| {
                            p.workload_id.as_deref() == Some(&w.id)
                                && p.role == ProcessRole::Workload
                        })
                        .collect();
                    let mut ports: Vec<_> = members
                        .iter()
                        .flat_map(|p| p.listening_ports.iter().flatten().copied())
                        .collect();
                    ports.sort_unstable();
                    ports.dedup();
                    report.services.push(Service {
                        workload_id: w.id.clone(),
                        pids: members.iter().map(|p| p.identity.pid).collect(),
                        ports,
                    });
                }
            }
            if snapshot.attribution.processes.iter().any(|p| {
                p.agent_id.as_deref() == Some(&agent.id) && p.role == ProcessRole::AgentInternal
            }) {
                let key = format!("internal:{}", agent.id);
                self.schedule(key.clone(), agent.id.clone(), None, true);
                report.scheduled.push(key);
            }
        }
        self.counted_services
            .retain(|id| snapshot.attribution.workloads.iter().any(|w| &w.id == id));
        let newly_reported = report
            .services
            .iter()
            .filter(|s| self.counted_services.insert(s.workload_id.clone()))
            .count();
        if newly_reported > 0 {
            self.record(
                log,
                "service_reported",
                serde_json::json!({"count": newly_reported}),
            );
        }
        let new: Vec<_> = report
            .services
            .iter()
            .filter(|s| !self.reported_services.contains(&s.workload_id))
            .collect();
        if !new.is_empty() && self.can_notify("services", now) {
            let body = new
                .iter()
                .map(|s| {
                    format!(
                        "Service {} survives (PID {:?}, ports {:?}); ballast stop {}",
                        s.workload_id, s.pids, s.ports, s.workload_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.notify("services", &body, now, platform, log);
            self.reported_services
                .extend(new.iter().map(|s| s.workload_id.clone()));
        }
        report
    }

    fn advance(
        &mut self,
        now: Instant,
        snapshot: &Snapshot,
        guardian: &mut Guardian,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) {
        let mut pending = std::mem::take(&mut self.pending);
        let mut active = HashSet::new();
        for (key, termination) in &mut pending {
            let agent = snapshot
                .attribution
                .agents
                .iter()
                .find(|a| a.id == termination.agent_id);
            let workload = termination
                .workload_id
                .as_ref()
                .and_then(|id| snapshot.attribution.workloads.iter().find(|w| &w.id == id));
            let authorized = !termination.automatic
                || (agent.is_some_and(|a| a.state == AgentState::Ended)
                    && (termination.workload_id.is_none()
                        || workload.is_some_and(|w| w.class == WorkloadClass::Batch)));
            let members: HashSet<_> = snapshot
                .attribution
                .processes
                .iter()
                .filter(|p| {
                    authorized
                        && p.agent_id.as_deref() == Some(&termination.agent_id)
                        && match &termination.workload_id {
                            Some(id) => {
                                p.role == ProcessRole::Workload
                                    && p.workload_id.as_ref() == Some(id)
                            }
                            None => p.role == ProcessRole::AgentInternal,
                        }
                        && !snapshot
                            .attribution
                            .agents
                            .iter()
                            .any(|a| a.root == Some(p.identity))
                })
                .map(|p| p.identity)
                .collect();
            let gone: Vec<_> = termination
                .members
                .iter()
                .filter(|(id, (sent, _))| {
                    sent.is_some() && platform.process_liveness(**id) == ProcessLiveness::Gone
                })
                .map(|(&id, _)| id)
                .collect();
            if !gone.is_empty() && matches!(self.mode, Mode::Enforce) {
                self.record(
                    log,
                    "clean_reclaimed",
                    serde_json::json!({"target": key, "processes": gone}),
                );
                self.reclaimed
                    .push(format!("{} ({} processes)", key, gone.len()));
            }
            termination.members.retain(|id, _| {
                platform.process_liveness(*id) != ProcessLiveness::Gone
                    && (!termination.automatic || members.contains(id))
            });
            // The workload record survives provisional attribution and omitted rows.
            // Keep accepted user intent even before any member could be selected.
            if !termination.members.is_empty() || (!termination.automatic && workload.is_some()) {
                active.insert(key.clone());
            }
            let mut targets = members.clone();
            if !termination.automatic {
                targets.extend(termination.members.iter().filter_map(|(&id, (sent, _))| {
                    (sent.is_some()
                        && !snapshot
                            .attribution
                            .agents
                            .iter()
                            .any(|a| a.root == Some(id)))
                    .then_some(id)
                }));
            }
            if targets.is_empty() {
                continue;
            }
            active.insert(key.clone());
            // Remember intent before thaw/TERM: an unreadable scan must not discard a retry.
            for &id in &members {
                termination.members.entry(id).or_insert((None, false));
            }
            if let Some(id) = &termination.workload_id {
                // A journal member may have exec'd into a nested agent since the freeze.
                if guardian
                    .frozen
                    .iter()
                    .filter(|w| &w.workload_id == id)
                    .any(|w| {
                        w.processes.iter().any(|pid| {
                            snapshot
                                .attribution
                                .agents
                                .iter()
                                .any(|a| a.root == Some(*pid))
                        })
                    })
                {
                    self.errors.push(format!(
                        "cleanup {key}: frozen membership includes an agent root"
                    ));
                    continue;
                }
                if let Err(e) = guardian.resume_for_cleanup(id, now, platform, log) {
                    self.errors.push(format!("cleanup resume {key}: {e}"));
                    continue;
                }
            }
            let mut targets: Vec<_> = targets.into_iter().collect();
            targets.sort_unstable_by_key(|id| (workload.is_none_or(|w| w.root != *id), *id));
            for id in targets {
                if let Some(&(Some(sent), killed)) = termination.members.get(&id) {
                    if !killed
                        && now.saturating_duration_since(sent) >= Duration::from_secs(5)
                        && self.signal(id, Signal::Kill, key, snapshot, platform, log)
                    {
                        termination.members.insert(id, (Some(sent), true));
                    }
                } else {
                    if snapshot
                        .processes
                        .iter()
                        .any(|p| p.identity == id && p.stopped)
                        && !self.signal(id, Signal::Continue, key, snapshot, platform, log)
                    {
                        continue;
                    }
                    if self.signal(id, Signal::Terminate, key, snapshot, platform, log) {
                        termination.members.insert(id, (Some(now), false));
                    }
                }
            }
        }
        pending.retain(|key, _| active.contains(key));
        self.pending = pending;
        if !self.reclaimed.is_empty() && self.can_notify("reclaimed", now) {
            let body = format!("Reclaimed {}", self.reclaimed.join(", "));
            self.notify("reclaimed", &body, now, platform, log);
            self.reclaimed.clear();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn signal(
        &mut self,
        id: ProcessIdentity,
        signal: Signal,
        target: &str,
        snapshot: &Snapshot,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> bool {
        let result = if matches!(self.mode, Mode::Observe) {
            Ok(())
        } else {
            platform.send_signal(id, signal)
        };
        let agent_id = snapshot
            .attribution
            .processes
            .iter()
            .find(|p| p.identity == id)
            .and_then(|p| p.agent_id.as_ref());
        let agent = snapshot
            .attribution
            .agents
            .iter()
            .find(|a| Some(&a.id) == agent_id);
        self.record(log, "clean", serde_json::json!({"target": target, "process": id,
            "signal": format!("{signal:?}"), "error": result.as_ref().err().map(ToString::to_string),
            "pressure": snapshot.pressure, "sampled_at_ms": snapshot.status.sampled_at_ms,
            "memory_bytes": snapshot.processes.iter().find(|p| p.identity == id).and_then(|p| p.metrics).map(|m| m.memory_bytes),
            "agent": agent.map(|a| serde_json::json!({"id": a.id, "state": a.state, "ended_at_ms": a.ended_at_ms})),
            "grace_seconds": self.grace.as_secs(),
            "workload": snapshot.attribution.workloads.iter().find(|w| w.id == target).map(|w|
                serde_json::json!({"id": w.id, "class": w.class, "root": w.root, "memory": w.memory}))}));
        match result {
            Ok(()) => true,
            Err(e)
                if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) =>
            {
                false
            }
            Err(e) => {
                self.errors
                    .push(format!("cleanup {target} PID {}: {e}", id.pid));
                false
            }
        }
    }
    fn record(&mut self, log: &mut RotatingLog, event: &str, details: serde_json::Value) {
        if let Err(e) = log.decision(
            event,
            serde_json::json!({"mode": self.mode, "decision": details}),
        ) {
            self.errors.push(format!("cleanup log: {e}"));
        }
    }
    fn can_notify(&self, kind: &'static str, now: Instant) -> bool {
        self.last_notification
            .get(kind)
            .is_none_or(|sent| now.saturating_duration_since(*sent) >= Duration::from_secs(60))
    }
    fn notify(
        &mut self,
        kind: &'static str,
        body: &str,
        now: Instant,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) {
        self.last_notification.insert(kind, now);
        self.record(
            log,
            "notify",
            serde_json::json!({"kind": kind, "message": body}),
        );
        if matches!(self.mode, Mode::Enforce) {
            if let Err(e) = platform.notify("Ballast", body) {
                self.errors.push(format!("cleanup notification: {e}"));
            }
        }
    }
}

pub fn command(target: Option<String>) -> io::Result<()> {
    use crate::daemon::{
        files::Paths,
        ipc::{Client, Method, Reply},
    };
    let mut client = Client::connect(&Paths::from_env()?, Duration::from_secs(3))?;
    let response = client.request(target.map_or(Method::Gc, |target| Method::Stop { target }))?;
    let report = match response.reply {
        Reply::Cleanup { report } => report,
        Reply::Error { message } => return Err(io::Error::other(message)),
        _ => return Err(io::Error::other("unexpected cleanup response")),
    };
    println!(
        "{} {} targets; survivors receive SIGKILL after 5 seconds.",
        if report.observe {
            "Would stop"
        } else {
            "Stopping"
        },
        report.scheduled.len()
    );
    for target in report.pending {
        println!("Cleanup pending: {target}");
    }
    for service in report.services {
        println!(
            "Service {}: PID {:?}, ports {:?}; ballast stop {}",
            service.workload_id, service.pids, service.ports, service.workload_id
        );
    }
    for orphan in report.orphans {
        println!(
            "Unattributed orphan: {} PID {} (untouched)",
            orphan.executable, orphan.identity.pid
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
