use super::{Level, RateWindow, recovery};
use crate::attribution::{ProcessRole, WorkloadClass};
use crate::daemon::{
    Snapshot,
    files::{Mode, Paths, RotatingLog},
};
use crate::platform::{Platform, Process, ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inputs {
    pub cpu_busy_ticks: u64,
    pub cpu_total_ticks: u64,
    pub cpu_count: u32,
    pub load_per_core: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub cpu_busy_fraction: f64,
    pub cpu_load_per_core: f64,
    pub agent_resource_share: f64,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            cpu_busy_fraction: 0.9,
            cpu_load_per_core: 1.0,
            agent_resource_share: 0.3,
        }
    }
}
impl Thresholds {
    pub fn valid(&self) -> bool {
        [self.cpu_busy_fraction, self.agent_resource_share]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
            && self.cpu_load_per_core.is_finite()
            && self.cpu_load_per_core > 0.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThrottledWorkload {
    pub workload_id: String,
    pub root: ProcessIdentity,
    pub processes: Vec<ProcessIdentity>,
    #[serde(default)]
    pub preserved: Vec<ProcessIdentity>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct View {
    pub cpu_level: Level,
    pub cpu_busy_fraction: Option<f64>,
    pub agent_cpu_share: Option<f64>,
    pub throttled_cpu_share: Option<f64>,
    pub workloads: Vec<ThrottledWorkload>,
}

#[derive(Default)]
struct Elevated {
    level: Level,
    above: bool,
    below: Option<Instant>,
}
impl Elevated {
    fn sample(&mut self, now: Instant, high: Option<bool>) -> Level {
        match high {
            None => *self = Self::default(),
            Some(true) => {
                if self.above {
                    self.level = Level::Elevated;
                }
                self.above = true;
                self.below = None;
            }
            Some(false) => {
                self.above = false;
                if now.saturating_duration_since(*self.below.get_or_insert(now))
                    >= Duration::from_secs(10)
                {
                    self.level = Level::Normal;
                }
            }
        }
        self.level
    }
}

#[derive(Serialize, Deserialize)]
struct Journal {
    boot_id: String,
    workloads: Vec<ThrottledWorkload>,
}
fn write(paths: &Paths, boot_id: &str, workloads: &[ThrottledWorkload]) -> io::Result<()> {
    recovery::write_json(
        paths,
        "throttled",
        &serde_json::json!({"boot_id":boot_id,"workloads":workloads}),
    )
}
fn write_all(
    paths: &Paths,
    boot: &str,
    selected: &[ThrottledWorkload],
    untouched: &[ThrottledWorkload],
) -> io::Result<()> {
    let all: Vec<_> = selected.iter().chain(untouched).cloned().collect();
    write(paths, boot, &all)
}
fn read(paths: &Paths) -> io::Result<Journal> {
    Ok(serde_json::from_slice(&std::fs::read(
        paths.base.join("state/throttled.json"),
    )?)?)
}
pub fn needs_recovery(paths: &Paths) -> bool {
    match read(paths) {
        Ok(journal) => !journal.workloads.is_empty(),
        Err(_) => {
            std::fs::metadata(paths.base.join("state/throttled.json")).is_ok_and(|m| m.len() > 0)
        }
    }
}
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn descendants(processes: &[Process], seeds: &[ProcessIdentity]) -> HashSet<ProcessIdentity> {
    let live: HashMap<_, _> = processes.iter().map(|p| (p.identity.pid, p)).collect();
    let mut owned: HashSet<_> = seeds.iter().copied().collect();
    loop {
        let before = owned.len();
        for p in processes {
            if p.uid == unsafe { libc::geteuid() }
                && live
                    .get(&p.ppid)
                    .is_some_and(|parent| owned.contains(&parent.identity))
            {
                owned.insert(p.identity);
            }
        }
        if owned.len() == before {
            return owned;
        }
    }
}
fn owned_descendants(processes: &[Process], w: &ThrottledWorkload) -> HashSet<ProcessIdentity> {
    let preserved = descendants(processes, &w.preserved);
    descendants(processes, &w.processes)
        .difference(&preserved)
        .copied()
        .collect()
}

fn fresh_processes(platform: &mut impl Platform) -> io::Result<Vec<Process>> {
    let processes = platform.list_processes(&HashSet::new(), &HashSet::new())?;
    let watched = processes.iter().map(|p| p.identity).collect();
    platform.list_processes(&watched, &HashSet::new())
}

// Keep ancestry anchors until a second scan finds no inherited external BG left.
fn restore(
    paths: &Paths,
    boot: &str,
    workloads: &mut Vec<ThrottledWorkload>,
    platform: &mut impl Platform,
    untouched: &[ThrottledWorkload],
) -> io::Result<usize> {
    if workloads.is_empty() {
        return Ok(0);
    }
    let processes = fresh_processes(platform);
    let mut error = processes
        .as_ref()
        .err()
        .map(|e| io::Error::other(e.to_string()));
    if let Ok(processes) = processes {
        for w in workloads.iter_mut() {
            w.processes = owned_descendants(&processes, w).into_iter().collect();
        }
    }
    // Restoration is safe even if expanding or persisting the new set failed.
    if let Err(e) = write_all(paths, boot, workloads, untouched) {
        error = Some(e);
    }
    let mut count = 0;
    for w in workloads.iter() {
        for &id in &w.processes {
            match platform.set_backgrounded(id, false) {
                Ok(()) => count += 1,
                Err(e) if gone(&e) => {}
                Err(e) => error = Some(e),
            }
        }
    }
    if let Ok(processes) = fresh_processes(platform) {
        let mut remaining = Vec::new();
        for w in workloads.iter() {
            let mut pending = Vec::new();
            for id in owned_descendants(&processes, w) {
                match platform.backgrounded(id) {
                    Ok(true) => pending.push(id),
                    Ok(false) => {}
                    Err(e) if gone(&e) => {}
                    Err(e) => {
                        pending.push(id);
                        error = Some(e);
                    }
                }
            }
            if !pending.is_empty() {
                pending.extend(w.processes.iter().copied());
                pending.sort_unstable();
                pending.dedup();
                remaining.push(ThrottledWorkload {
                    processes: pending,
                    ..w.clone()
                });
            }
        }
        write_all(paths, boot, &remaining, untouched)?;
        *workloads = remaining;
    } else if error.is_none() {
        error = Some(io::Error::other(
            "cannot verify inherited background-policy restoration",
        ));
    }
    if !workloads.is_empty() && error.is_none() {
        error = Some(io::Error::other(
            "background descendants remain; restoration will retry",
        ));
    }
    error.map_or(Ok(count), Err)
}

/// Caller holds the Ballast directory lock; no marker-based policy sweep.
pub fn recover(
    paths: &Paths,
    platform: &mut impl Platform,
    log: &mut RotatingLog,
) -> io::Result<usize> {
    let boot = platform.boot_id()?;
    let mut journal = match read(paths) {
        Ok(journal) => journal,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            let _ = log.decision(
                "throttle_recovery_unreadable",
                serde_json::json!({"error":e.to_string()}),
            );
            return Err(e);
        }
    };
    if journal.boot_id != boot {
        write(paths, &boot, &[])?;
        return Ok(0);
    }
    let result = restore(paths, &boot, &mut journal.workloads, platform, &[]);
    let _ = log.decision(
        "throttle_recovery",
        serde_json::json!({"remaining":journal.workloads.len()}),
    );
    result
}

pub struct Controller {
    pub view: View,
    pub enabled: bool,
    paths: Paths,
    boot: String,
    mode: Mode,
    thresholds: Thresholds,
    cpu: Elevated,
    busy: RateWindow,
    total: RateWindow,
    cores: Option<u32>,
    cpu_rates: HashMap<ProcessIdentity, RateWindow>,
    ineligible: HashMap<String, Instant>,
    restoring: bool,
}
impl Controller {
    pub fn new(
        paths: Paths,
        boot: String,
        mode: Mode,
        enabled: bool,
        thresholds: Thresholds,
    ) -> Self {
        Self {
            view: View::default(),
            enabled,
            paths,
            boot,
            mode,
            thresholds,
            cpu: Elevated::default(),
            busy: RateWindow::default(),
            total: RateWindow::default(),
            cores: None,
            cpu_rates: HashMap::new(),
            ineligible: HashMap::new(),
            restoring: false,
        }
    }
    pub fn load_journal(&mut self) -> io::Result<()> {
        match read(&self.paths) {
            Ok(journal) if journal.boot_id == self.boot => self.view.workloads = journal.workloads,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }
    pub fn watched(&self) -> impl Iterator<Item = ProcessIdentity> + '_ {
        self.view
            .workloads
            .iter()
            .flat_map(|w| w.processes.iter().copied())
    }
    fn record(&self, log: &mut RotatingLog, event: &str, id: &str) -> io::Result<()> {
        log.decision(
            event,
            serde_json::json!({"mode":self.mode,
            "decision":{"workload_id":id}, "evidence":self.view}),
        )
    }
    fn release(
        &mut self,
        target: Option<&str>,
        platform: &mut impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<usize> {
        let selected: Vec<_> = self
            .view
            .workloads
            .iter()
            .filter(|w| target.is_none_or(|id| id == w.workload_id))
            .cloned()
            .collect();
        if selected.is_empty() {
            return Ok(0);
        }
        let ids: HashSet<_> = selected.iter().map(|w| w.workload_id.clone()).collect();
        let mut remaining = selected;
        // Persist the full journal, including unselected workloads, throughout restoration.
        let unselected: Vec<_> = self
            .view
            .workloads
            .iter()
            .filter(|w| !ids.contains(&w.workload_id))
            .cloned()
            .collect();
        let result = if matches!(self.mode, Mode::Enforce) {
            restore(
                &self.paths,
                &self.boot,
                &mut remaining,
                platform,
                &unselected,
            )
        } else {
            remaining.clear();
            Ok(0)
        };
        let restored: Vec<_> = ids
            .into_iter()
            .filter(|id| !remaining.iter().any(|w| &w.workload_id == id))
            .collect();
        self.view.workloads = unselected.into_iter().chain(remaining).collect();
        for id in &restored {
            self.record(log, "unthrottle", id)?;
        }
        result.map(|_| restored.len())
    }
    pub fn resume(
        &mut self,
        target: Option<&str>,
        now: Instant,
        platform: &mut impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<usize> {
        for w in &self.view.workloads {
            if target.is_none_or(|id| id == w.workload_id) {
                self.ineligible
                    .insert(w.workload_id.clone(), now + Duration::from_secs(300));
            }
        }
        let result = self.release(target, platform, log);
        self.restoring |= result.is_err();
        result
    }
    pub fn tick(
        &mut self,
        now: Instant,
        snapshot: &Snapshot,
        platform: &mut impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<()> {
        if self.restoring {
            self.release(None, platform, log)?;
            self.restoring = false;
            return Ok(());
        }
        let result = self.update(now, snapshot, platform, log);
        if result.is_err() {
            self.restoring = self.release(None, platform, log).is_err();
        }
        result
    }
    fn update(
        &mut self,
        now: Instant,
        snapshot: &Snapshot,
        platform: &mut impl Platform,
        log: &mut RotatingLog,
    ) -> io::Result<()> {
        if !platform.supports_throttle() {
            return Ok(());
        }
        self.ineligible.retain(|_, until| now < *until);
        let input = snapshot
            .pressure
            .as_ref()
            .and_then(|p| p.throttle.as_ref())
            .filter(|_| {
                self.enabled && platform.supports_throttle() && !snapshot.status.sample_discarded
            })
            .filter(|p| p.cpu_count > 0 && p.load_per_core.is_finite());
        let old_level = self.view.cpu_level;
        if self.cores != input.map(|p| p.cpu_count) {
            self.busy = RateWindow::default();
            self.total = RateWindow::default();
            self.cpu_rates.clear();
            self.cores = input.map(|p| p.cpu_count);
        }
        let busy = self
            .busy
            .sample(now, input.map(|p| p.cpu_busy_ticks), WINDOW);
        let total = self
            .total
            .sample(now, input.map(|p| p.cpu_total_ticks), WINDOW);
        self.view.cpu_busy_fraction = busy
            .zip(total)
            .filter(|(_, t)| *t > 0.0)
            .map(|(b, t)| (b / t).clamp(0.0, 1.0));
        let host_high = self.view.cpu_busy_fraction.zip(input).map(|(busy, p)| {
            busy > self.thresholds.cpu_busy_fraction
                && p.load_per_core > self.thresholds.cpu_load_per_core
        });
        let batches: HashSet<_> = snapshot
            .attribution
            .workloads
            .iter()
            .filter(|w| w.class == WorkloadClass::Batch)
            .map(|w| w.id.as_str())
            .collect();
        let eligible: HashMap<_, _> = snapshot
            .attribution
            .processes
            .iter()
            .filter(|a| a.role == ProcessRole::Workload)
            .filter_map(|a| {
                a.workload_id
                    .as_deref()
                    .filter(|w| batches.contains(w))
                    .map(|w| (a.identity, w))
            })
            .collect();
        let mut live = HashSet::new();
        let owned: HashSet<_> = self.watched().collect();
        let (mut cpu_share, mut owned_cpu) = (0.0, 0.0);
        for p in &snapshot.processes {
            if p.uid != unsafe { libc::geteuid() } || !eligible.contains_key(&p.identity) {
                continue;
            }
            live.insert(p.identity);
            if let Some(v) = self.cpu_rates.entry(p.identity).or_default().sample(
                now,
                input.and(p.metrics.map(|m| m.cpu_time_ns)),
                WINDOW,
            ) {
                cpu_share += v / 1e9;
                if owned.contains(&p.identity) {
                    owned_cpu += v / 1e9;
                }
            }
        }
        self.cpu_rates.retain(|id, _| live.contains(id));
        let host_cpu = self
            .view
            .cpu_busy_fraction
            .zip(input)
            .filter(|(busy, _)| *busy > 0.0)
            .map(|(busy, p)| busy * f64::from(p.cpu_count));
        self.view.agent_cpu_share = host_cpu.map(|cpu| (cpu_share / cpu).clamp(0.0, 1.0));
        self.view.throttled_cpu_share = host_cpu.map(|cpu| (owned_cpu / cpu).clamp(0.0, 1.0));
        // Throttling reduces host busy time before the workload's demand goes away.
        self.view.cpu_level = self.cpu.sample(
            now,
            host_high.map(|high| {
                high || (old_level == Level::Elevated
                    && self
                        .view
                        .throttled_cpu_share
                        .is_some_and(|share| share >= self.thresholds.agent_resource_share))
            }),
        );
        if old_level != self.view.cpu_level {
            self.record(log, "throttle_pressure_transition", "")?;
        }
        if self.view.cpu_level == Level::Normal {
            self.release(None, platform, log)?;
            return Ok(());
        }
        let fault = host_high == Some(true)
            && self
                .view
                .agent_cpu_share
                .is_some_and(|share| share >= self.thresholds.agent_resource_share);
        let observed: HashSet<_> = snapshot.processes.iter().map(|p| p.identity).collect();
        let mut obsolete = HashSet::new();
        for w in &self.view.workloads {
            if !batches.contains(w.workload_id.as_str()) {
                obsolete.insert(w.workload_id.clone());
                continue;
            }
            for id in owned_descendants(&snapshot.processes, w) {
                if !observed.contains(&id)
                    || eligible.get(&id).copied() == Some(w.workload_id.as_str())
                {
                    continue;
                }
                // A protected child may inherit BG before its first attribution sample.
                match platform.backgrounded(id) {
                    Ok(bg) if bg || w.processes.contains(&id) => {
                        obsolete.insert(w.workload_id.clone());
                    }
                    Ok(_) => {}
                    Err(e) if gone(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        for id in &obsolete {
            self.release(Some(id), platform, log)?;
        }
        let mut added_members = Vec::new();
        let mut new_workloads = Vec::new();
        let mut changed = false;
        for w in snapshot
            .attribution
            .workloads
            .iter()
            .filter(|w| batches.contains(w.id.as_str()))
        {
            if self.ineligible.contains_key(&w.id) || obsolete.contains(&w.id) {
                continue;
            }
            let prior = self
                .view
                .workloads
                .iter()
                .position(|t| t.workload_id == w.id);
            if !fault && prior.is_none() {
                continue;
            }
            let old: Vec<_> = prior
                .map(|i| self.view.workloads[i].processes.clone())
                .unwrap_or_default();
            let inherited = descendants(&snapshot.processes, &old);
            let mut preserved = prior
                .map(|i| self.view.workloads[i].preserved.clone())
                .unwrap_or_default();
            let members: Vec<_> = live
                .iter()
                .copied()
                .filter(|id| eligible.get(id).copied() == Some(w.id.as_str()))
                .collect();
            let mut added = Vec::new();
            // Recovery visits the whole subtree, including protected nested agents.
            // Record their existing policy before applying BG to any ancestor.
            for id in descendants(&snapshot.processes, &members) {
                if old.contains(&id) {
                    continue;
                }
                match platform.backgrounded(id) {
                    Ok(true) if !inherited.contains(&id) => preserved.push(id),
                    Ok(_) if members.contains(&id) => added.push(id),
                    Ok(_) => {}
                    Err(e) if gone(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            preserved.sort_unstable();
            preserved.dedup();
            let excluded = descendants(&snapshot.processes, &preserved);
            added.retain(|id| !excluded.contains(id));
            if added.is_empty() {
                if let Some(i) = prior {
                    if self.view.workloads[i].preserved != preserved {
                        self.view.workloads[i].preserved = preserved;
                        changed = true;
                    }
                }
                continue;
            }
            let index = prior.unwrap_or_else(|| {
                self.view.workloads.push(ThrottledWorkload {
                    workload_id: w.id.clone(),
                    root: w.root,
                    processes: Vec::new(),
                    preserved: Vec::new(),
                });
                self.view.workloads.len() - 1
            });
            self.view.workloads[index].preserved = preserved;
            self.view.workloads[index].processes.extend(&added);
            changed = true;
            added_members.extend(added);
            if prior.is_none() {
                new_workloads.push(w.id.clone());
            }
        }
        if changed {
            if matches!(self.mode, Mode::Enforce) {
                write(&self.paths, &self.boot, &self.view.workloads)?;
            }
            if matches!(self.mode, Mode::Enforce) {
                for id in added_members {
                    if let Err(e) = platform.set_backgrounded(id, true) {
                        if !gone(&e) {
                            return Err(e);
                        }
                    }
                }
            }
            for id in new_workloads {
                self.record(log, "throttle", &id)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;
