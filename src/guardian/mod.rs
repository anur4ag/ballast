pub mod recovery;
#[cfg(test)]
mod tests;

use crate::attribution::{AttributionSnapshot, Attributor, ProcessRole, Workload, WorkloadClass};
use crate::daemon::{
    Snapshot,
    files::{Mode, Paths, RotatingLog},
};
use crate::platform::{Platform, PressureInputs, Process, ProcessIdentity, Signal};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::time::{Duration, Instant};

const PRESSURE_RATE_WINDOW: Duration = Duration::from_secs(5);

const COOLDOWN: Duration = Duration::from_secs(5);
const MAX_FREEZE: Duration = Duration::from_secs(600);
const INELIGIBLE: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Normal,
    Elevated,
    Critical,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub macos_elevated_mib_per_sec: f64,
    pub macos_critical_mib_per_sec: f64,
    pub linux_elevated_some: f64,
    pub linux_critical_some: f64,
    pub linux_critical_full: f64,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            macos_elevated_mib_per_sec: 64.0,
            macos_critical_mib_per_sec: 256.0,
            linux_elevated_some: 10.0,
            linux_critical_some: 40.0,
            linux_critical_full: 5.0,
        }
    }
}
impl Thresholds {
    pub fn valid(&self) -> bool {
        [
            self.macos_elevated_mib_per_sec,
            self.macos_critical_mib_per_sec,
            self.linux_elevated_some,
            self.linux_critical_some,
            self.linux_critical_full,
        ]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
            && self.macos_elevated_mib_per_sec <= self.macos_critical_mib_per_sec
            && self.linux_elevated_some <= self.linux_critical_some
            && self.linux_critical_some <= 100.0
            && self.linux_critical_full <= 100.0
    }
}

#[derive(Default)]
struct RateWindow(VecDeque<(Instant, u64)>);
impl RateWindow {
    fn sample(&mut self, now: Instant, counter: Option<u64>) -> Option<f64> {
        let Some(counter) = counter else {
            self.0.clear();
            return None;
        };
        if self
            .0
            .back()
            .is_some_and(|&(then, old)| now <= then || counter < old)
        {
            self.0.clear();
        }
        self.0.push_back((now, counter));
        while self
            .0
            .get(1)
            .is_some_and(|&(then, _)| now.duration_since(then) >= PRESSURE_RATE_WINDOW)
        {
            self.0.pop_front();
        }
        let &(then, old) = self.0.front()?;
        let elapsed = now.duration_since(then).as_secs_f64();
        if elapsed == 0.0 {
            return None;
        }
        let window = PRESSURE_RATE_WINDOW.as_secs_f64();
        let mut delta = (counter - old) as f64;
        if elapsed > window {
            let &(next, next_counter) = &self.0[1];
            delta -= (next_counter - old) as f64 * (elapsed - window)
                / next.duration_since(then).as_secs_f64();
        }
        Some(delta / elapsed.min(window))
    }
}

#[derive(Default)]
struct PressureState {
    valid: bool,
    level: Level,
    higher: Option<Level>,
    below_since: Option<Instant>,
    page_size: u64,
    pageouts: RateWindow,
    swapouts: RateWindow,
    psi_some: RateWindow,
    psi_full: RateWindow,
    psi_some_percent: Option<f64>,
    psi_full_percent: Option<f64>,
    pageout_mib_per_sec: Option<f64>,
    swapout_mib_per_sec: Option<f64>,
}
impl PressureState {
    fn sample(
        &mut self,
        now: Instant,
        input: Option<&PressureInputs>,
        thresholds: &Thresholds,
        macos: bool,
    ) -> Level {
        self.valid = false;
        self.pageout_mib_per_sec = None;
        self.swapout_mib_per_sec = None;
        self.psi_some_percent = None;
        self.psi_full_percent = None;
        let Some(input) = input else {
            self.psi_some.0.clear();
            self.psi_full.0.clear();
            self.pageouts.0.clear();
            self.swapouts.0.clear();
            self.higher = None;
            self.below_since = None;
            return self.level;
        };
        if !macos || input.page_size != self.page_size {
            self.pageouts.0.clear();
            self.swapouts.0.clear();
        }
        self.page_size = input.page_size;
        if macos {
            let mib_per_page = input.page_size as f64 / 1048576.0;
            self.pageout_mib_per_sec = self
                .pageouts
                .sample(now, input.pageouts)
                .map(|rate| rate * mib_per_page);
            self.swapout_mib_per_sec = self
                .swapouts
                .sample(now, input.swapouts)
                .map(|rate| rate * mib_per_page);
        } else {
            let percent = |rate: Option<f64>, avg10: Option<f64>| {
                rate.map(|us_per_sec| (us_per_sec / 10_000.0).clamp(0.0, 100.0))
                    .or(avg10.filter(|v| v.is_finite() && (0.0..=100.0).contains(v)))
            };
            self.psi_some_percent = percent(
                self.psi_some.sample(now, input.psi_some_total_us),
                input.psi_some_avg10,
            );
            self.psi_full_percent = percent(
                self.psi_full.sample(now, input.psi_full_total_us),
                input.psi_full_avg10,
            );
        }
        let psi_known = self.psi_some_percent.is_some() || self.psi_full_percent.is_some();
        if !matches!(input.kernel_pressure_level, Some(1 | 2 | 4))
            && !psi_known
            && self.pageout_mib_per_sec.is_none()
            && self.swapout_mib_per_sec.is_none()
        {
            self.higher = None;
            self.below_since = None;
            return self.level;
        }
        self.valid = true;
        let level = if input.kernel_pressure_level.is_some_and(|v| v & 4 != 0)
            || self
                .swapout_mib_per_sec
                .is_some_and(|v| v > thresholds.macos_critical_mib_per_sec)
            || self
                .psi_some_percent
                .is_some_and(|v| v > thresholds.linux_critical_some)
            || self
                .psi_full_percent
                .is_some_and(|v| v > thresholds.linux_critical_full)
        {
            Level::Critical
        } else if input.kernel_pressure_level.is_some_and(|v| v & 2 != 0)
            || self
                .pageout_mib_per_sec
                .zip(self.swapout_mib_per_sec)
                .is_some_and(|(p, s)| p + s > thresholds.macos_elevated_mib_per_sec)
            || self
                .psi_some_percent
                .is_some_and(|v| v > thresholds.linux_elevated_some)
        {
            Level::Elevated
        } else {
            Level::Normal
        };
        if macos && input.kernel_pressure_level == Some(4) {
            self.level = Level::Critical;
            self.higher = None;
            self.below_since = None;
        } else if level > self.level {
            self.below_since = None;
            if self.higher == Some(level) {
                self.level = level;
                self.higher = None;
            } else {
                self.higher = Some(level);
            }
        } else {
            self.higher = None;
            if level < self.level {
                let since = self.below_since.get_or_insert(now);
                if now.saturating_duration_since(*since) >= Duration::from_secs(10) {
                    self.level = if self.level == Level::Critical {
                        Level::Elevated
                    } else {
                        Level::Normal
                    };
                    self.below_since = None;
                }
            } else {
                self.below_since = None;
            }
        }
        self.level
    }
}

/// Current policy explanation and the exact pressure rates used by the guardian.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GuardianNote {
    pub kind: String,
    pub message: String,
    pub sampled_at_ms: u64,
    pub agent_memory_share: Option<f64>,
    pub pageout_mib_per_sec: Option<f64>,
    pub swapout_mib_per_sec: Option<f64>,
    #[serde(default)]
    pub psi_some_percent: Option<f64>,
    #[serde(default)]
    pub psi_full_percent: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrozenWorkload {
    pub workload_id: String,
    pub root: ProcessIdentity,
    pub processes: Vec<ProcessIdentity>,
    pub frozen_at_ms: u64,
    #[serde(skip, default = "Instant::now")]
    since: Instant,
}
impl FrozenWorkload {
    fn recovery(id: ProcessIdentity) -> Self {
        Self {
            workload_id: format!("recovery:{}:{}", id.pid, id.start_time),
            root: id,
            processes: vec![id],
            frozen_at_ms: crate::daemon::unix_ms(),
            since: Instant::now(),
        }
    }
}

pub struct Guardian {
    pub level: Level,
    pub frozen: Vec<FrozenWorkload>,
    pub note: GuardianNote,
    paths: Paths,
    boot_id: String,
    mode: Mode,
    thresholds: Thresholds,
    pressure: PressureState,
    invalid_since: Option<Instant>,
    errors: Vec<String>,
    last_freeze: Option<Instant>,
    last_resume: Option<Instant>,
    ineligible: HashMap<String, Instant>,
    resumed_this_tick: HashSet<ProcessIdentity>,
    notifications: HashMap<&'static str, Instant>,
    last_stand_down: Option<&'static str>,
    episode_notified: bool,
    descriptions: HashMap<String, crate::notifications::Work>,
    evidence: serde_json::Value,
}
impl Guardian {
    pub fn new(paths: Paths, boot_id: String, mode: Mode, thresholds: Thresholds) -> Self {
        Self {
            level: Level::Normal,
            frozen: Vec::new(),
            note: GuardianNote::default(),
            paths,
            boot_id,
            mode,
            thresholds,
            pressure: PressureState::default(),
            invalid_since: None,
            errors: Vec::new(),
            last_freeze: None,
            last_resume: None,
            ineligible: HashMap::new(),
            resumed_this_tick: HashSet::new(),
            notifications: HashMap::new(),
            last_stand_down: None,
            episode_notified: false,
            descriptions: HashMap::new(),
            evidence: serde_json::Value::Null,
        }
    }
    /// Drain non-fatal decision-log and notification failures for the daemon log.
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    fn decision(&mut self, log: &mut RotatingLog, event: &str, details: serde_json::Value) {
        if let Err(error) = log.decision(event, details) {
            self.errors.push(format!("decision log ({event}): {error}"));
        }
    }

    pub fn watched(&self) -> HashSet<ProcessIdentity> {
        self.frozen
            .iter()
            .flat_map(|w| w.processes.iter().copied())
            .collect()
    }
    fn running_workloads<'a>(
        &self,
        attribution: &'a AttributionSnapshot,
        processes: &[Process],
    ) -> HashSet<&'a str> {
        let running: HashSet<_> = processes
            .iter()
            .filter(|p| !p.stopped || self.resumed_this_tick.contains(&p.identity))
            .map(|p| p.identity)
            .collect();
        let frozen: HashSet<_> = self.frozen.iter().map(|w| w.workload_id.as_str()).collect();
        attribution
            .processes
            .iter()
            .filter(|a| a.role == ProcessRole::Workload && running.contains(&a.identity))
            .filter_map(|a| a.workload_id.as_deref())
            .filter(|id| !frozen.contains(id))
            .collect()
    }
    pub fn batch_running(&self, attribution: &AttributionSnapshot, processes: &[Process]) -> bool {
        let running = self.running_workloads(attribution, processes);
        attribution
            .workloads
            .iter()
            .any(|w| w.class == WorkloadClass::Batch && running.contains(w.id.as_str()))
    }
    pub fn tick(
        &mut self,
        now: Instant,
        snapshot: &Snapshot,
        platform: &mut impl Platform,
        attributor: &mut Attributor,
        log: &mut RotatingLog,
    ) -> io::Result<()> {
        self.resumed_this_tick.clear();
        self.descriptions
            .retain(|id, _| self.frozen.iter().any(|w| &w.workload_id == id));
        for workload in &snapshot.attribution.workloads {
            if self.frozen.iter().any(|w| w.workload_id == workload.id) {
                self.descriptions.insert(
                    workload.id.clone(),
                    crate::notifications::Work::from_snapshot(snapshot, workload),
                );
            }
        }
        let previous = self.level;
        self.level = self.pressure.sample(
            now,
            snapshot.pressure.as_ref(),
            &self.thresholds,
            snapshot.capabilities.kernel_pressure,
        );
        let pressure_unknown = if self.pressure.valid {
            self.invalid_since = None;
            false
        } else {
            now.saturating_duration_since(*self.invalid_since.get_or_insert(now))
                >= Duration::from_secs(30)
        };
        if pressure_unknown {
            self.level = Level::Normal;
            self.pressure.level = Level::Normal;
        }
        self.note = GuardianNote {
            kind: "monitoring".into(),
            message: "Monitoring memory pressure.".into(),
            sampled_at_ms: snapshot.status.sampled_at_ms,
            pageout_mib_per_sec: self.pressure.pageout_mib_per_sec,
            swapout_mib_per_sec: self.pressure.swapout_mib_per_sec,
            psi_some_percent: self.pressure.psi_some_percent,
            psi_full_percent: self.pressure.psi_full_percent,
            ..GuardianNote::default()
        };
        let workloads: Vec<_> = snapshot.attribution.workloads.iter().map(|w| serde_json::json!({
            "id": w.id, "root": w.root, "class": w.class, "memory": w.memory, "first_seen_ms": w.first_seen_ms,
        })).collect();
        self.evidence = serde_json::json!({"sampled_at_ms": snapshot.status.sampled_at_ms,
            "pressure": snapshot.pressure, "pageout_mib_per_sec": self.pressure.pageout_mib_per_sec,
            "swapout_mib_per_sec": self.pressure.swapout_mib_per_sec,
            "psi_some_percent": self.pressure.psi_some_percent, "psi_full_percent": self.pressure.psi_full_percent, "workloads": workloads,
            "agent_memory_bytes": snapshot.attribution.agents.iter().map(|a| a.memory.bytes).fold(0u64, u64::saturating_add)});
        if self.level != previous {
            self.record(
                log,
                "pressure_transition",
                serde_json::json!({"from": previous, "to": self.level}),
            );
        }
        if self.level == Level::Normal {
            self.episode_notified = false;
        }
        self.ineligible.retain(|_, until| now < *until);
        let expired: Vec<_> = self
            .frozen
            .iter()
            .filter(|w| {
                now.saturating_duration_since(w.since) >= MAX_FREEZE
                    || snapshot.status.sampled_at_ms.saturating_sub(w.frozen_at_ms)
                        >= MAX_FREEZE.as_millis() as u64
            })
            .map(|w| w.workload_id.clone())
            .collect();
        for id in expired {
            self.resume_one(&id, true, "max_freeze", now, platform, log)?;
        }
        if !self.pressure.valid && !pressure_unknown {
            self.explain(
                "unknown_pressure",
                "Pressure sample unavailable; no new freezes.",
            );
            return Ok(());
        }
        if self.level == Level::Normal {
            self.explain(
                "normal",
                "No new holds or freezes; paused work resumes in order.",
            );
            self.last_stand_down = None;
            if self
                .last_resume
                .is_none_or(|then| now.saturating_duration_since(then) >= COOLDOWN)
            {
                if let Some(id) = self.frozen.first().map(|w| w.workload_id.clone()) {
                    let reason = if pressure_unknown {
                        "pressure_unknown"
                    } else {
                        "normal"
                    };
                    self.resume_one(&id, false, reason, now, platform, log)?;
                }
            }
            return Ok(());
        }
        if self.level != Level::Critical {
            self.explain(
                "elevated",
                "Heavy commands may wait; running work continues.",
            );
            return Ok(());
        }
        if self
            .last_freeze
            .is_some_and(|then| now.saturating_duration_since(then) < COOLDOWN)
        {
            self.explain(
                "cooldown",
                "Waiting five seconds before another freeze decision.",
            );
            return Ok(());
        }
        let attributed: u128 = snapshot
            .attribution
            .agents
            .iter()
            .map(|a| u128::from(a.memory.bytes))
            .sum();
        let used = snapshot.pressure.as_ref().and_then(|p| p.used_memory_bytes);
        self.note.agent_memory_share = used
            .filter(|used| *used > 0)
            .map(|used| attributed as f64 / used as f64);
        if used.is_none_or(|used| used == 0 || attributed * 10 < u128::from(used) * 3) {
            self.stand_down("non_agent_pressure", log)?;
            self.notify(
                "non_agent_pressure",
                "Memory pressure is coming from non-agent apps.",
                now,
                platform,
                log,
            );
            return Ok(());
        }
        let running = self.running_workloads(&snapshot.attribution, &snapshot.processes);
        let candidates: Vec<_> = snapshot
            .attribution
            .workloads
            .iter()
            .filter(|w| !self.ineligible.contains_key(&w.id) && running.contains(w.id.as_str()))
            .collect();
        let Some(victim) = candidates.iter().copied().max_by_key(|w| {
            (
                w.class == WorkloadClass::Batch,
                w.memory.growth_bytes_per_sec,
                w.first_seen_ms,
                &w.id,
            )
        }) else {
            return self.stand_down("no_eligible_workload", log);
        };
        let batches = snapshot
            .attribution
            .workloads
            .iter()
            .filter(|w| w.class == WorkloadClass::Batch && running.contains(w.id.as_str()))
            .count();
        if victim.class == WorkloadClass::Batch
            && batches == 1
            && (victim
                .memory
                .growth_bytes_per_sec
                .is_none_or(|rate| rate <= 0)
                || candidates.iter().any(|w| {
                    w.memory.growth_bytes_per_sec.is_none()
                        || w.memory.growth_bytes_per_sec > victim.memory.growth_bytes_per_sec
                }))
        {
            return self.stand_down("last_batch_not_fastest", log);
        }
        self.last_stand_down = None;
        self.last_freeze = Some(now);
        if let Err(error) = self.freeze(victim, snapshot, now, platform, attributor) {
            self.explain(
                "freeze_failed",
                "Could not freeze workload; check last error.",
            );
            self.record(
                log,
                "freeze_failed",
                serde_json::json!({"workload_id": victim.id}),
            );
            return Err(error);
        }
        self.explain(
            "froze",
            &format!(
                "{} {} to relieve memory pressure.",
                if matches!(self.mode, Mode::Observe) {
                    "Would pause"
                } else {
                    "Paused"
                },
                crate::notifications::label(&victim.label)
            ),
        );
        self.record(log, "freeze", serde_json::json!({"workload_id": victim.id,
            "agent_kind": snapshot.attribution.agents.iter().find(|a| a.id == victim.agent_id).map(|a| &a.kind)}));
        if !self.episode_notified {
            let description = crate::notifications::Work::from_snapshot(snapshot, victim);
            self.episode_notified = self.notify(
                "freeze",
                &crate::notifications::paused(&description),
                now,
                platform,
                log,
            );
        }
        Ok(())
    }
    fn explain(&mut self, kind: &str, message: &str) {
        self.note.kind = kind.into();
        self.note.message = message.into();
    }
    fn stand_down(&mut self, reason: &'static str, log: &mut RotatingLog) -> io::Result<()> {
        let message = match reason {
            "non_agent_pressure" if self.note.agent_memory_share.is_some() => {
                "Memory pressure is coming from non-agent apps; no new freeze."
            }
            "non_agent_pressure" => "Agent memory share unknown; no new freeze.",
            "last_batch_not_fastest" => "Last batch lacks fastest-growth evidence; no new freeze.",
            _ => "No eligible workload to freeze.",
        };
        self.explain(reason, message);
        if self.last_stand_down != Some(reason) {
            self.record(log, "freeze_skipped", serde_json::json!({"reason": reason}));
            self.last_stand_down = Some(reason);
        }
        Ok(())
    }
    fn record(&mut self, log: &mut RotatingLog, event: &str, decision: serde_json::Value) {
        self.decision(
            log,
            event,
            serde_json::json!({"mode": self.mode, "level": self.level,
            "evidence": self.evidence, "decision": decision}),
        )
    }

    fn freeze(
        &mut self,
        victim: &Workload,
        snapshot: &Snapshot,
        now: Instant,
        platform: &mut impl Platform,
        attributor: &mut Attributor,
    ) -> io::Result<()> {
        let mut members = members_for_pass(
            &snapshot.attribution,
            &snapshot.processes,
            &victim.id,
            victim.root,
        );
        if members.is_empty() {
            return Err(io::Error::other("workload has no observable members"));
        }
        self.descriptions.insert(
            victim.id.clone(),
            crate::notifications::Work::from_snapshot(snapshot, victim),
        );
        self.frozen.push(FrozenWorkload {
            workload_id: victim.id.clone(),
            root: victim.root,
            processes: members.clone(),
            frozen_at_ms: crate::daemon::unix_ms(),
            since: now,
        });
        if matches!(self.mode, Mode::Observe) {
            return Ok(());
        }
        let result = (|| {
            // Each pass journals its complete membership before signalling any newly discovered child.
            for pass in 0..3 {
                recovery::write(&self.paths, &self.boot_id, &self.frozen)?;
                for &id in &members {
                    recovery::signal(platform, id, Signal::Stop)?;
                }
                if pass == 2 {
                    break;
                }
                let watched = attributor.watched(&self.watched());
                let mut processes =
                    platform.list_processes(&watched, &attributor.metric_targets())?;
                let attribution = attributor.update(
                    platform,
                    &mut processes,
                    now,
                    crate::daemon::unix_ms(),
                    false,
                );
                let current = members_for_pass(&attribution, &processes, &victim.id, victim.root);
                let frozen = self.frozen.last_mut().unwrap();
                let new: Vec<_> = current
                    .iter()
                    .filter(|id| !frozen.processes.contains(id))
                    .copied()
                    .collect();
                if new.is_empty() {
                    break;
                }
                frozen.processes.extend(new);
                members = current;
            }
            Ok(())
        })();
        if result.is_err() {
            if let Err(error) = self.thaw(self.frozen.len() - 1, platform) {
                self.errors.push(format!("freeze rollback: {error}"));
            }
        }
        result
    }

    fn thaw(&mut self, index: usize, platform: &impl Platform) -> io::Result<FrozenWorkload> {
        let workload = self.frozen[index].clone();
        if matches!(self.mode, Mode::Enforce) {
            let mut failure = None;
            for &id in &workload.processes {
                if let Err(e) = recovery::signal(platform, id, Signal::Continue) {
                    failure = Some(e);
                }
            }
            if let Some(e) = failure {
                return Err(e);
            }
            let remaining: Vec<_> = self
                .frozen
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index)
                .map(|(_, w)| w.clone())
                .collect();
            recovery::write(&self.paths, &self.boot_id, &remaining)?;
        }
        self.resumed_this_tick.extend(&workload.processes);
        self.frozen.remove(index);
        Ok(workload)
    }

    pub fn resume_for_cleanup(
        &mut self,
        id: &str,
        now: Instant,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<()> {
        self.ineligible.insert(id.to_owned(), now + INELIGIBLE);
        self.resume_one(id, false, "cleanup", now, platform, log)
    }

    pub fn resume(
        &mut self,
        target: Option<&str>,
        now: Instant,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<usize> {
        let target = target
            .map(|target| {
                crate::attribution::WorkloadHandles::new(
                    self.frozen.iter().map(|w| w.workload_id.as_str()),
                )
                .resolve(target)
            })
            .transpose()?;
        let ids: Vec<_> = self
            .frozen
            .iter()
            .filter(|w| target.as_deref().is_none_or(|id| w.workload_id == id))
            .map(|w| w.workload_id.clone())
            .collect();
        if target.is_some() && ids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "frozen workload not found",
            ));
        }
        for id in &ids {
            self.resume_one(id, true, "manual", now, platform, log)?;
        }
        Ok(ids.len())
    }
    fn resume_one(
        &mut self,
        id: &str,
        forced: bool,
        reason: &str,
        now: Instant,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<()> {
        let Some(index) = self.frozen.iter().position(|w| w.workload_id == id) else {
            return Ok(());
        };
        let workload = self.thaw(index, platform)?;
        self.last_resume = Some(now);
        if forced {
            self.ineligible.insert(id.to_owned(), now + INELIGIBLE);
        }
        let description =
            self.descriptions
                .remove(id)
                .unwrap_or_else(|| crate::notifications::Work {
                    label: "workload".into(),
                    agent: "agent".into(),
                    handle: String::new(),
                    bytes: None,
                    ports: Vec::new(),
                });
        self.explain(
            "resumed",
            &format!("Resumed {} ({reason}).", description.label),
        );
        self.decision(log, "resume", serde_json::json!({"mode": self.mode, "level": self.level, "workload": workload, "reason": reason, "evidence": self.evidence}));
        if forced {
            self.notify(
                if reason == "max_freeze" {
                    "max_freeze"
                } else {
                    "forced_resume"
                },
                &crate::notifications::resumed(&description, reason == "max_freeze"),
                now,
                platform,
                log,
            );
        }
        Ok(())
    }
    fn notify(
        &mut self,
        kind: &'static str,
        body: &str,
        now: Instant,
        platform: &impl Platform,
        log: &mut RotatingLog,
    ) -> bool {
        if kind != "max_freeze"
            && self
                .notifications
                .get(kind)
                .is_some_and(|then| now.saturating_duration_since(*then) < Duration::from_secs(60))
        {
            return false;
        }
        self.notifications.insert(kind, now);
        self.decision(log, "notify", serde_json::json!({"mode": self.mode, "kind": kind, "message": body, "evidence": self.evidence}));
        if matches!(self.mode, Mode::Enforce) {
            match platform.notify("Ballast", body) {
                Ok(delivered) => delivered,
                Err(e) => {
                    self.errors.push(format!("notification ({kind}): {e}"));
                    false
                }
            }
        } else {
            true
        }
    }
}

fn members_for_pass(
    attribution: &AttributionSnapshot,
    processes: &[Process],
    workload: &str,
    root: ProcessIdentity,
) -> Vec<ProcessIdentity> {
    let live: HashSet<_> = processes.iter().map(|p| p.identity).collect();
    let mut ids: Vec<_> = attribution
        .processes
        .iter()
        .filter(|a| {
            a.role == ProcessRole::Workload
                && a.workload_id.as_deref() == Some(workload)
                && live.contains(&a.identity)
        })
        .map(|a| a.identity)
        .collect();
    ids.sort_unstable_by_key(|id| (*id != root, *id));
    ids.dedup();
    ids
}
