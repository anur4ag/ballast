//! Scenario tests for `Guardian` (ticket 05): pressure hysteresis, freeze/resume decisions,
//! crash-journal persistence ordering and observe-mode parity.
//!
//! Every test drives the real state machine end to end through `Guardian::tick`/`resume` with
//! hand-built `Snapshot`/`AttributionSnapshot` fixtures and `FakePlatform` (in-memory, exact
//! control over signals/notifications; never spawns or touches a real process) -- never a
//! reference model, never a real user process. The one exception is
//! `late_children_are_persisted_before_their_own_stop_signal`, which needs the real `Attributor`
//! to reattribute a freshly-forked child during the repeated freeze pass; see its comment.
//!
//! `Guardian::tick` always samples pressure first, so every scenario below that needs a freeze
//! opens with `warm_to_critical`, which spends three ticks driving the macOS swap-rate hysteresis
//! from `Normal` to `Critical` against an empty snapshot before the scenario's own tick runs.

use super::*;
use crate::attribution::{
    Agent, AgentState, AttributionSnapshot, Attributor, MemorySummary, ProcessAttribution,
    ProcessRole, Workload, WorkloadClass,
};
use crate::daemon::files::{Config, Mode, Paths, RotatingLog};
use crate::daemon::{ProcessChanges, Snapshot, Status};
use crate::platform::{
    Capabilities, Environment, Platform, PressureInputs, Process, ProcessIdentity, ProcessMetrics,
    Signal,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------
// FakePlatform
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordedSignal {
    Stop,
    Continue,
}

#[derive(Default)]
struct FakePlatform {
    signals: RefCell<Vec<(ProcessIdentity, RecordedSignal)>>,
    fail: HashSet<ProcessIdentity>,
    notifications: RefCell<Vec<(String, String)>>,
    boot_id: String,
    journal: Option<Paths>,
    /// Returned by `list_processes`, for the repeated-pass rescan inside `freeze`.
    rescan: Vec<Process>,
    environments: HashMap<ProcessIdentity, Environment>,
}
impl FakePlatform {
    fn new(boot_id: &str) -> Self {
        Self {
            boot_id: boot_id.into(),
            ..Self::default()
        }
    }
    /// The next `send_signal` for this identity fails instead of succeeding, e.g. to drive a
    /// failed-resume scenario.
    fn fail_signal(mut self, id: ProcessIdentity) -> Self {
        self.fail.insert(id);
        self
    }
    fn rescan_with(mut self, processes: Vec<Process>) -> Self {
        self.rescan = processes;
        self
    }
    fn env(mut self, id: ProcessIdentity, env: &[(&str, &str)]) -> Self {
        self.environments.insert(
            id,
            env.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        self
    }
    fn stopped(&self) -> Vec<ProcessIdentity> {
        self.signals
            .borrow()
            .iter()
            .filter(|(_, s)| *s == RecordedSignal::Stop)
            .map(|(id, _)| *id)
            .collect()
    }
    fn continued(&self) -> Vec<ProcessIdentity> {
        self.signals
            .borrow()
            .iter()
            .filter(|(_, s)| *s == RecordedSignal::Continue)
            .map(|(id, _)| *id)
            .collect()
    }
    fn signal_count(&self) -> usize {
        self.signals.borrow().len()
    }
    fn notifications(&self) -> Vec<(String, String)> {
        self.notifications.borrow().clone()
    }
}
impl Platform for FakePlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            environment: true,
            listening_ports: true,
            memory_footprint: true,
            memory_psi: true,
            kernel_pressure: true,
            notifications: true,
            atomic_signals: true,
        }
    }
    fn boot_id(&self) -> io::Result<String> {
        Ok(self.boot_id.clone())
    }
    fn list_processes(
        &mut self,
        _watched: &HashSet<ProcessIdentity>,
        _metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        Ok(self.rescan.clone())
    }
    fn read_environment(&self, process: ProcessIdentity) -> Option<Environment> {
        self.environments.get(&process).cloned()
    }
    fn process_metrics(&self, _process: ProcessIdentity) -> Option<ProcessMetrics> {
        None
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        Ok(PressureInputs::default())
    }
    fn listening_ports(&self, _process: ProcessIdentity) -> Option<Vec<u16>> {
        Some(Vec::new())
    }
    fn send_signal(&self, process: ProcessIdentity, signal: Signal) -> io::Result<()> {
        if self.fail.contains(&process) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fake signal failure",
            ));
        }
        if matches!(signal, Signal::Stop) {
            if let Some(paths) = &self.journal {
                let saved = recovery::read(paths).expect("journal must precede every stop");
                assert!(
                    saved
                        .workloads
                        .iter()
                        .any(|w| w.processes.contains(&process)),
                    "SIGSTOP identity must already be journaled: {process:?}"
                );
            }
        }
        let recorded = match signal {
            Signal::Stop => RecordedSignal::Stop,
            Signal::Continue => RecordedSignal::Continue,
            other => panic!("guardian must only ever send Stop/Continue, got {other:?}"),
        };
        self.signals.borrow_mut().push((process, recorded));
        Ok(())
    }
    fn notify(&self, title: &str, body: &str) -> io::Result<bool> {
        self.notifications
            .borrow_mut()
            .push((title.into(), body.into()));
        Ok(true)
    }
}

// ---------------------------------------------------------------------
// Fixtures: identities, hand-built processes and attribution, snapshots.
// ---------------------------------------------------------------------

fn id(pid: i32, start_time: u64) -> ProcessIdentity {
    ProcessIdentity { pid, start_time }
}
fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn process(identity: ProcessIdentity, ppid: i32, memory_bytes: u64) -> Process {
    Process {
        identity,
        ppid,
        pgid: identity.pid,
        uid: own_uid(),
        stopped: false,
        exe: Some("/usr/bin/fake-workload".into()),
        argv: Some(vec!["fake-workload".into()]),
        metrics: Some(ProcessMetrics {
            memory_bytes,
            cpu_time_ns: 0,
        }),
    }
}
fn workload_attribution(
    identity: ProcessIdentity,
    agent_id: &str,
    workload_id: &str,
) -> ProcessAttribution {
    ProcessAttribution {
        identity,
        owner_id: None,
        agent_id: Some(agent_id.into()),
        workload_id: Some(workload_id.into()),
        role: ProcessRole::Workload,
        environment_known: true,
        listening_ports: None,
        ports_sampled_at_ms: None,
    }
}
fn internal_attribution(identity: ProcessIdentity, agent_id: &str) -> ProcessAttribution {
    ProcessAttribution {
        identity,
        owner_id: None,
        agent_id: Some(agent_id.into()),
        workload_id: None,
        role: ProcessRole::AgentInternal,
        environment_known: true,
        listening_ports: None,
        ports_sampled_at_ms: None,
    }
}
fn unattributed_attribution(identity: ProcessIdentity) -> ProcessAttribution {
    ProcessAttribution {
        identity,
        owner_id: None,
        agent_id: None,
        workload_id: None,
        role: ProcessRole::Unattributed,
        environment_known: true,
        listening_ports: None,
        ports_sampled_at_ms: None,
    }
}
fn agent(agent_id: &str, root: ProcessIdentity, memory_bytes: u64) -> Agent {
    Agent {
        id: agent_id.into(),
        owner_id: None,
        session_id: None,
        kind: "claude".into(),
        root: Some(root),
        cwd: None,
        state: AgentState::Idle,
        ended_at_ms: None,
        memory: MemorySummary {
            bytes: memory_bytes,
            complete: true,
            growth_30s_bytes: None,
        },
    }
}
#[allow(clippy::too_many_arguments)]
fn workload(
    workload_id: &str,
    agent_id: &str,
    root: ProcessIdentity,
    class: WorkloadClass,
    first_seen_ms: u64,
    bytes: u64,
    growth_30s_bytes: Option<i64>,
) -> Workload {
    Workload {
        id: workload_id.into(),
        agent_id: agent_id.into(),
        root,
        label: workload_id.into(),
        class,
        first_seen_ms,
        detached_pgid: None,
        memory: MemorySummary {
            bytes,
            complete: true,
            growth_30s_bytes,
        },
    }
}
fn snapshot(
    pressure: Option<PressureInputs>,
    attribution: AttributionSnapshot,
    processes: Vec<Process>,
) -> Snapshot {
    Snapshot {
        status: Status {
            daemon_version: "test".into(),
            pid: 0,
            mode: Mode::Enforce,
            tick: 0,
            sampled_at_ms: 0,
            tick_interval_ms: 1000,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded: false,
            process_count: processes.len(),
            pressure_level: Level::Normal,
            batch_running: false,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: "fake-boot".into(),
        capabilities: FakePlatform::default().capabilities(),
        processes,
        changes: ProcessChanges::default(),
        pressure,
        attribution,
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
    }
}

/// Cumulative pageout/swapout counters whose *delta* between consecutive 1s-spaced calls is far
/// above both macOS defaults (E=64, C=256 MiB/s at a 4096-byte page size): `step * 200_000` pages
/// is a ~781 MiB/s delta per step, so any two consecutive steps clear both thresholds regardless
/// of `step`'s absolute value. `used_memory_bytes` is fixed at 8 GiB so callers can size agent
/// memory against a known base for the 30%-fault-share check.
fn heavy_swap(step: u64) -> PressureInputs {
    PressureInputs {
        page_size: 4096,
        used_memory_bytes: Some(8 * GIB),
        pageouts: Some(step * 200_000),
        swapouts: Some(step * 200_000),
        ..PressureInputs::default()
    }
}

/// Drives three 1s-spaced ticks against an empty snapshot (a baseline sample, then two
/// consecutive rate samples over threshold) so `guardian.level` reaches `Critical` via the same
/// hysteresis every real tick goes through, before a scenario's own tick runs. An empty
/// snapshot can only ever produce a harmless `non_agent_pressure` stand-down, never a freeze.
/// Returns the `Instant` of the last warm-up tick and the swap step it used, so callers can
/// continue the same rising counters (`heavy_swap(step + 1)`, ...) to keep pressure Critical.
fn warm_to_critical(
    guardian: &mut Guardian,
    platform: &mut FakePlatform,
    attributor: &mut Attributor,
    log: &mut RotatingLog,
) -> (Instant, u64) {
    let start = Instant::now();
    for step in 0..3u64 {
        let now = start + Duration::from_secs(step);
        let empty = snapshot(
            Some(heavy_swap(step)),
            AttributionSnapshot::default(),
            Vec::new(),
        );
        guardian
            .tick(now, &empty, platform, attributor, log)
            .expect("warm-up tick");
    }
    assert_eq!(
        guardian.level,
        Level::Critical,
        "warm-up must reach Critical before the scenario's own tick"
    );
    (start + Duration::from_secs(2), 2)
}

fn quiet_normal_snapshot() -> Snapshot {
    snapshot(
        Some(PressureInputs {
            page_size: 4096,
            kernel_pressure_level: Some(1),
            ..PressureInputs::default()
        }),
        AttributionSnapshot::default(),
        Vec::new(),
    )
}

/// Walks the guardian back down from `Critical` to `Normal`: the exit hysteresis demotes only
/// one level per 10 continuous seconds below it, so `Critical -> Normal` needs two separate 10s
/// countdowns (`Critical -> Elevated`, then `Elevated -> Normal`), each restarting its own timer
/// once the previous one fires. Uses an empty snapshot throughout: whether any workload is
/// frozen is guardian-internal bookkeeping, independent of what a given tick's snapshot
/// describes. Returns the `Instant` of the tick that first reads `Level::Normal` -- which, by
/// `tick`'s own logic, already resumed the oldest frozen workload (if any) as a side effect of
/// reaching Normal, since `last_resume` starts unset and so never blocks a first resume.
fn cool_to_normal(
    guardian: &mut Guardian,
    platform: &mut FakePlatform,
    attributor: &mut Attributor,
    log: &mut RotatingLog,
    start: Instant,
) -> Instant {
    let mut now = start;
    guardian
        .tick(now, &quiet_normal_snapshot(), platform, attributor, log)
        .expect("begin critical->elevated countdown");
    now += Duration::from_secs(10);
    guardian
        .tick(now, &quiet_normal_snapshot(), platform, attributor, log)
        .expect("demote to elevated");
    assert_eq!(
        guardian.level,
        Level::Elevated,
        "must demote exactly one level after 10s"
    );
    now += Duration::from_secs(1);
    guardian
        .tick(now, &quiet_normal_snapshot(), platform, attributor, log)
        .expect("begin elevated->normal countdown");
    now += Duration::from_secs(10);
    guardian
        .tick(now, &quiet_normal_snapshot(), platform, attributor, log)
        .expect("demote to normal");
    assert_eq!(guardian.level, Level::Normal);
    now
}

struct TestHome(Paths);
impl TestHome {
    fn new(tag: &str) -> Self {
        let path = PathBuf::from(format!("/tmp/blt-guardian-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp BALLAST_HOME");
        let home = Self(Paths { base: path });
        home.0.prepare().expect("prepare paths");
        home
    }
    fn log(&self) -> RotatingLog {
        RotatingLog::open(self.0.base.join("log/decisions.jsonl"), &Config::default())
            .expect("open decisions log")
    }
}
impl std::ops::Deref for TestHome {
    type Target = Paths;
    fn deref(&self) -> &Paths {
        &self.0
    }
}
impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0.base);
    }
}

// ---------------------------------------------------------------------
// Pressure hysteresis: entry, exit, and a missing sample resetting both.
// ---------------------------------------------------------------------

#[test]
fn macos_swap_rate_enters_elevated_only_after_two_consecutive_rate_samples() {
    // The first `sample()` only ever establishes a baseline (no previous reading means no
    // rate), so it takes three total calls -- baseline, then two consecutive over-threshold
    // rate readings -- to promote the level, even though that is "two samples" of rate data.
    let mut state = PressureState::default();
    let thresholds = Thresholds::default();
    let start = Instant::now();
    // 15_000 pages/step at a 4096-byte page size is a ~58.6 MiB/s delta per consecutive pair:
    // pageout+swapout sums to ~117 MiB/s (clears the 64 MiB/s Elevated threshold) while swapout
    // alone stays at ~58.6 MiB/s (well under the 256 MiB/s Critical threshold), so these samples
    // can only ever promote to Elevated, never jump straight to Critical.
    let heavy = |step: u64| PressureInputs {
        page_size: 4096,
        pageouts: Some(step * 15_000),
        swapouts: Some(step * 15_000),
        ..PressureInputs::default()
    };
    assert_eq!(
        state.sample(start, Some(&heavy(0)), &thresholds),
        Level::Normal,
        "a lone baseline sample has no rate to judge"
    );
    assert_eq!(
        state.sample(start + Duration::from_secs(1), Some(&heavy(1)), &thresholds),
        Level::Normal,
        "one over-threshold rate sample must not promote the level yet"
    );
    assert_eq!(
        state.sample(start + Duration::from_secs(2), Some(&heavy(2)), &thresholds),
        Level::Elevated,
        "a second consecutive over-threshold rate sample must promote the level"
    );
}

#[test]
fn linux_psi_enters_elevated_after_two_consecutive_samples() {
    // PSI is read directly (no delta needed), so entry takes exactly the two samples the
    // governing spec describes -- unlike the macOS rate case above, which needs a third call
    // just to establish its first rate.
    let mut state = PressureState::default();
    let thresholds = Thresholds::default();
    let start = Instant::now();
    let psi = |some: f64| PressureInputs {
        page_size: 4096,
        psi_some_avg10: Some(some),
        ..PressureInputs::default()
    };
    assert_eq!(
        state.sample(start, Some(&psi(15.0)), &thresholds),
        Level::Normal,
        "one sample above the linux elevated threshold must not promote the level yet"
    );
    assert_eq!(
        state.sample(
            start + Duration::from_secs(1),
            Some(&psi(15.0)),
            &thresholds
        ),
        Level::Elevated,
        "a second consecutive sample above the linux elevated threshold must promote it"
    );
}

#[test]
fn level_drops_by_one_only_after_ten_seconds_below_it() {
    let mut state = PressureState::default();
    let thresholds = Thresholds::default();
    let start = Instant::now();
    // 15_000 pages/step at a 4096-byte page size is a ~58.6 MiB/s delta per consecutive pair:
    // pageout+swapout sums to ~117 MiB/s (clears the 64 MiB/s Elevated threshold) while swapout
    // alone stays at ~58.6 MiB/s (well under the 256 MiB/s Critical threshold), so these samples
    // can only ever promote to Elevated, never jump straight to Critical.
    let heavy = |step: u64| PressureInputs {
        page_size: 4096,
        pageouts: Some(step * 15_000),
        swapouts: Some(step * 15_000),
        ..PressureInputs::default()
    };
    // A zeroed swap-rate reading is indistinguishable from "no signal at all" (see
    // `a_missing_sample_resets_both_entry_and_exit_hysteresis`), so a genuinely Normal, *valid*
    // reading is expressed via `kernel_pressure_level` instead, per the guardian's own contract.
    let quiet = || PressureInputs {
        page_size: 4096,
        kernel_pressure_level: Some(1),
        ..PressureInputs::default()
    };
    state.sample(start, Some(&heavy(0)), &thresholds);
    state.sample(start + Duration::from_secs(1), Some(&heavy(1)), &thresholds);
    assert_eq!(
        state.sample(start + Duration::from_secs(2), Some(&heavy(2)), &thresholds),
        Level::Elevated
    );

    // The rate drops to zero immediately, but the level must not follow for 10s.
    let below_since = start + Duration::from_secs(3);
    assert_eq!(
        state.sample(below_since, Some(&quiet()), &thresholds),
        Level::Elevated,
        "dropping below threshold must not demote the level immediately"
    );
    assert_eq!(
        state.sample(
            below_since + Duration::from_secs(9),
            Some(&quiet()),
            &thresholds
        ),
        Level::Elevated,
        "9s below threshold is still short of the 10s exit hysteresis"
    );
    assert_eq!(
        state.sample(
            below_since + Duration::from_secs(10),
            Some(&quiet()),
            &thresholds
        ),
        Level::Normal,
        "10s continuously below threshold must demote exactly one level"
    );
}

#[test]
fn a_missing_sample_resets_both_entry_and_exit_hysteresis() {
    let mut state = PressureState::default();
    let thresholds = Thresholds::default();
    let start = Instant::now();
    // 15_000 pages/step at a 4096-byte page size is a ~58.6 MiB/s delta per consecutive pair:
    // pageout+swapout sums to ~117 MiB/s (clears the 64 MiB/s Elevated threshold) while swapout
    // alone stays at ~58.6 MiB/s (well under the 256 MiB/s Critical threshold), so these samples
    // can only ever promote to Elevated, never jump straight to Critical.
    let heavy = |step: u64| PressureInputs {
        page_size: 4096,
        pageouts: Some(step * 15_000),
        swapouts: Some(step * 15_000),
        ..PressureInputs::default()
    };

    // One over-threshold rate sample builds up entry progress; a missing sample must discard it,
    // so the very next over-threshold pair needs its own two consecutive readings again.
    state.sample(start, Some(&heavy(0)), &thresholds);
    state.sample(start + Duration::from_secs(1), Some(&heavy(1)), &thresholds);
    assert_eq!(
        state.sample(start + Duration::from_secs(2), None, &thresholds),
        Level::Normal,
        "a missing sample must return the unchanged level, not panic or guess"
    );
    state.sample(start + Duration::from_secs(3), Some(&heavy(0)), &thresholds);
    assert_eq!(
        state.sample(start + Duration::from_secs(4), Some(&heavy(1)), &thresholds),
        Level::Normal,
        "the missing sample must have discarded the earlier rate baseline entirely"
    );

    // Symmetrically, a missing sample while counting down an exit must restart that countdown.
    let elevated_at = state.sample(start + Duration::from_secs(5), Some(&heavy(2)), &thresholds);
    assert_eq!(elevated_at, Level::Elevated);
    let quiet = PressureInputs {
        page_size: 4096,
        kernel_pressure_level: Some(1),
        ..PressureInputs::default()
    };
    let below_since = start + Duration::from_secs(6);
    state.sample(below_since, Some(&quiet), &thresholds);
    state.sample(below_since + Duration::from_secs(9), None, &thresholds);
    assert_eq!(
        state.sample(
            below_since + Duration::from_secs(10),
            Some(&quiet),
            &thresholds
        ),
        Level::Elevated,
        "the missing sample mid-countdown must have restarted the 10s exit timer"
    );
}

#[test]
fn threshold_defaults_match_the_verification_spike_placeholders() {
    let thresholds = Thresholds::default();
    assert_eq!(thresholds.macos_elevated_mib_per_sec, 64.0);
    assert_eq!(thresholds.macos_critical_mib_per_sec, 256.0);
    assert_eq!(thresholds.linux_elevated_some, 10.0);
    assert_eq!(thresholds.linux_critical_some, 40.0);
    assert_eq!(thresholds.linux_critical_full, 5.0);
    assert!(thresholds.valid());
}

#[test]
fn tick_never_freezes_on_a_sample_with_no_valid_pressure_signal() {
    // Regression: a sample carrying no kernel pressure level, no PSI and no computable swap
    // rate -- e.g. `used_memory_bytes` alone, from a partial or discarded scan -- must never
    // drive a freeze decision, even while the guardian is still at a stale `Critical` from an
    // earlier, valid reading.
    let home = TestHome::new("invalid-pressure-no-freeze");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, _) = warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(1700, 1);
    let work_root = id(1701, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:17", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:17",
        "a:17",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:17", "w:17"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];

    let unknown_but_used_memory = PressureInputs {
        used_memory_bytes: Some(8 * GIB),
        ..PressureInputs::default()
    };
    now += Duration::from_secs(1);
    let snap = snapshot(Some(unknown_but_used_memory), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick with an unknown pressure sample");

    assert!(
        guardian.frozen.is_empty(),
        "an unknown pressure sample must never trigger a freeze, even while still at stale Critical"
    );
    assert!(platform.stopped().is_empty());
    assert_eq!(
        guardian.level,
        Level::Critical,
        "an unknown sample resets hysteresis dwell but must not drop the level outright"
    );
}

// ---------------------------------------------------------------------
// Critical-level freeze decisions.
// ---------------------------------------------------------------------

#[test]
fn critical_freeze_stands_down_when_pressure_is_not_agents_fault() {
    let home = TestHome::new("fault-standdown");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(100, 1);
    let work_root = id(101, 1);
    let mut attribution = AttributionSnapshot::default();
    // 1 GiB of the fixed 8 GiB `used_memory_bytes` is well under the 30% fault threshold.
    attribution.agents.push(agent("a:1", agent_root, GIB));
    attribution.workloads.push(workload(
        "w:1",
        "a:1",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:1", "w:1"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert!(
        guardian.frozen.is_empty(),
        "must not freeze when pressure is not agents' fault"
    );
    assert_eq!(guardian.note.kind, "non_agent_pressure");
    assert_eq!(guardian.note.agent_memory_share, Some(0.125));
    assert!(guardian.note.message.contains("non-agent apps"));
    assert!(platform.stopped().is_empty());
    assert!(
        platform
            .notifications()
            .iter()
            .any(|(_, body)| body.contains("non-agent apps")),
        "must notify once that pressure is coming from non-agent apps"
    );
}

#[test]
fn critical_freeze_proceeds_when_agents_hold_at_least_thirty_percent() {
    let home = TestHome::new("fault-proceeds");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(200, 1);
    let work_root = id(201, 1);
    let mut attribution = AttributionSnapshot::default();
    // Half of the fixed 8 GiB `used_memory_bytes` clears the 30% fault-share threshold with
    // plenty of margin (avoiding an exact-boundary value, which is a truncated-division trap).
    attribution.agents.push(agent("a:2", agent_root, 4 * GIB));
    attribution.workloads.push(workload(
        "w:2",
        "a:2",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:2", "w:2"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert_eq!(
        guardian.frozen.len(),
        1,
        "must freeze once the fault share clears 30%"
    );
    assert_eq!(guardian.frozen[0].workload_id, "w:2");
    assert_eq!(platform.stopped(), vec![work_root]);
}

#[test]
fn one_freeze_per_five_second_cooldown() {
    let home = TestHome::new("cooldown");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(300, 1);
    let first = id(301, 1);
    let second = id(302, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:3", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:first",
        "a:3",
        first,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution.workloads.push(workload(
        "w:second",
        "a:3",
        second,
        WorkloadClass::Batch,
        1,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(first, "a:3", "w:first"));
    attribution
        .processes
        .push(workload_attribution(second, "a:3", "w:second"));
    let processes = vec![
        process(first, agent_root.pid, GIB),
        process(second, agent_root.pid, GIB),
    ];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("first tick");
    assert_eq!(
        guardian.frozen.len(),
        1,
        "the fastest-tiebroken workload freezes first"
    );
    let first_frozen = guardian.frozen[0].workload_id.clone();

    // Well within the 5s cooldown: the still-eligible other workload must not also freeze.
    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick inside cooldown");
    assert_eq!(
        guardian.frozen.len(),
        1,
        "cooldown must block a second freeze"
    );

    // Past the cooldown: the remaining eligible workload freezes.
    now += Duration::from_secs(5);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick past cooldown");
    assert_eq!(
        guardian.frozen.len(),
        2,
        "past cooldown the other workload may freeze too"
    );
    assert!(
        guardian
            .frozen
            .iter()
            .any(|w| w.workload_id != first_frozen)
    );
}

// ---------------------------------------------------------------------
// Victim choice: batch before service, newest tiebreak.
// ---------------------------------------------------------------------

#[test]
fn victim_choice_prefers_batch_over_service_when_both_are_expendable() {
    let home = TestHome::new("victim-batch-over-service");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(400, 1);
    let other_batch = id(401, 1);
    let second_batch = id(403, 1);
    let service = id(402, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:4", agent_root, 6 * GIB));
    // A second batch keeps freezing the fast one from being the "last" batch, so the
    // last-workload gate never applies here -- this test is purely about victim choice.
    attribution.workloads.push(workload(
        "w:other-batch",
        "a:4",
        other_batch,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1),
    ));
    attribution.workloads.push(workload(
        "w:second-batch",
        "a:4",
        second_batch,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(0),
    ));
    attribution.workloads.push(workload(
        "w:service",
        "a:4",
        service,
        WorkloadClass::Service,
        0,
        GIB,
        Some(1_000_000_000),
    ));
    attribution
        .processes
        .push(workload_attribution(other_batch, "a:4", "w:other-batch"));
    attribution
        .processes
        .push(workload_attribution(second_batch, "a:4", "w:second-batch"));
    attribution
        .processes
        .push(workload_attribution(service, "a:4", "w:service"));
    let processes = vec![
        process(other_batch, agent_root.pid, GIB),
        process(second_batch, agent_root.pid, GIB),
        process(service, agent_root.pid, GIB),
    ];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert_eq!(guardian.frozen.len(), 1);
    assert_eq!(
        guardian.frozen[0].workload_id, "w:other-batch",
        "a batch workload must be chosen over a far-faster-growing service"
    );
}

#[test]
fn victim_choice_among_batches_ties_broken_by_newest() {
    let home = TestHome::new("victim-newest-tiebreak");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(500, 1);
    let older = id(501, 1);
    let newer = id(502, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:5", agent_root, 6 * GIB));
    // Same growth, different `first_seen_ms`: the newer one must win the tie.
    attribution.workloads.push(workload(
        "w:older",
        "a:5",
        older,
        WorkloadClass::Batch,
        1_000,
        GIB,
        Some(5_000),
    ));
    attribution.workloads.push(workload(
        "w:newer",
        "a:5",
        newer,
        WorkloadClass::Batch,
        2_000,
        GIB,
        Some(5_000),
    ));
    attribution
        .processes
        .push(workload_attribution(older, "a:5", "w:older"));
    attribution
        .processes
        .push(workload_attribution(newer, "a:5", "w:newer"));
    let processes = vec![
        process(older, agent_root.pid, GIB),
        process(newer, agent_root.pid, GIB),
    ];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert_eq!(guardian.frozen.len(), 1);
    assert_eq!(guardian.frozen[0].workload_id, "w:newer");
}

// ---------------------------------------------------------------------
// Last-workload rule: requires known growth for every eligible workload, both ways.
// ---------------------------------------------------------------------

#[test]
fn last_batch_stands_down_when_any_eligible_workloads_growth_is_unknown() {
    let home = TestHome::new("last-batch-unknown-growth");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(600, 1);
    let only_batch = id(601, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:6", agent_root, 6 * GIB));
    // The sole batch's own growth is unknown (still warming up): can't prove it is fastest.
    attribution.workloads.push(workload(
        "w:only",
        "a:6",
        only_batch,
        WorkloadClass::Batch,
        0,
        GIB,
        None,
    ));
    attribution
        .processes
        .push(workload_attribution(only_batch, "a:6", "w:only"));
    let processes = vec![process(only_batch, agent_root.pid, GIB)];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert!(
        guardian.frozen.is_empty(),
        "freezing the only batch workload requires knowing it is the fastest grower"
    );
}

#[test]
fn last_batch_is_blocked_by_a_faster_growing_eligible_service() {
    let home = TestHome::new("last-batch-slower-than-service");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(700, 1);
    let only_batch = id(701, 1);
    let service = id(702, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:7", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:only",
        "a:7",
        only_batch,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(100),
    ));
    attribution.workloads.push(workload(
        "w:service",
        "a:7",
        service,
        WorkloadClass::Service,
        0,
        GIB,
        Some(500),
    ));
    attribution
        .processes
        .push(workload_attribution(only_batch, "a:7", "w:only"));
    attribution
        .processes
        .push(workload_attribution(service, "a:7", "w:service"));
    let processes = vec![
        process(only_batch, agent_root.pid, GIB),
        process(service, agent_root.pid, GIB),
    ];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert!(
        guardian.frozen.is_empty(),
        "the eligible service is growing faster, so the lone batch is not provably the cause"
    );
}

#[test]
fn last_batch_freezes_when_it_is_provably_the_fastest_eligible_workload() {
    let home = TestHome::new("last-batch-is-fastest");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(800, 1);
    let only_batch = id(801, 1);
    let service = id(802, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:8", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:only",
        "a:8",
        only_batch,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(500),
    ));
    attribution.workloads.push(workload(
        "w:service",
        "a:8",
        service,
        WorkloadClass::Service,
        0,
        GIB,
        Some(100),
    ));
    attribution
        .processes
        .push(workload_attribution(only_batch, "a:8", "w:only"));
    attribution
        .processes
        .push(workload_attribution(service, "a:8", "w:service"));
    let processes = vec![
        process(only_batch, agent_root.pid, GIB),
        process(service, agent_root.pid, GIB),
    ];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert_eq!(guardian.frozen.len(), 1);
    assert_eq!(guardian.frozen[0].workload_id, "w:only");
}

// ---------------------------------------------------------------------
// Eligibility: agent, agent-internal and unattributed processes are never targets.
// ---------------------------------------------------------------------

#[test]
fn never_freezes_agent_root_agent_internal_or_unattributed_processes() {
    let home = TestHome::new("eligibility-roles");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(900, 1);
    let work_root = id(901, 1);
    // A workload descendant the real attributor classified as agent-internal (e.g. an MCP
    // server the build spawned) and an unrelated stray process the scan happened to enumerate.
    let internal_child = id(902, 1);
    let stray = id(903, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:9", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:9",
        "a:9",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:9", "w:9"));
    attribution
        .processes
        .push(internal_attribution(internal_child, "a:9"));
    attribution.processes.push(unattributed_attribution(stray));
    let processes = vec![
        process(work_root, agent_root.pid, GIB),
        process(internal_child, work_root.pid, 0),
        process(stray, 1, 0),
    ];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick");

    assert_eq!(guardian.frozen.len(), 1);
    assert_eq!(
        guardian.frozen[0].processes,
        vec![work_root],
        "only the workload-attributed member may be frozen"
    );
    assert_eq!(platform.stopped(), vec![work_root]);
    assert!(!platform.stopped().contains(&internal_child));
    assert!(!platform.stopped().contains(&stray));
    assert!(!platform.stopped().contains(&agent_root));
}

// ---------------------------------------------------------------------
// Max freeze and the five-minute ineligibility it and manual resume both cause.
// ---------------------------------------------------------------------

#[test]
fn max_freeze_forces_resume_after_ten_minutes_and_marks_ineligible_for_five() {
    let home = TestHome::new("max-freeze");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(1000, 1);
    let work_root = id(1001, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:10", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:10",
        "a:10",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:10", "w:10"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("freeze tick");
    assert_eq!(guardian.frozen.len(), 1);
    let frozen_since = now;

    // Just under 10 minutes: still frozen regardless of pressure.
    let almost = frozen_since + Duration::from_secs(599);
    let none = snapshot(None, AttributionSnapshot::default(), Vec::new());
    guardian
        .tick(almost, &none, &mut platform, &mut attributor, &mut log)
        .expect("tick just under max freeze");
    assert_eq!(
        guardian.frozen.len(),
        1,
        "must not force-resume before 10 minutes"
    );

    // At 10 minutes: forced resume, regardless of pressure.
    let expiry = frozen_since + Duration::from_secs(600);
    guardian
        .tick(expiry, &none, &mut platform, &mut attributor, &mut log)
        .expect("tick at max freeze");
    assert!(guardian.frozen.is_empty(), "max freeze must force a resume");
    assert_eq!(platform.continued(), vec![work_root]);
    assert!(
        platform
            .notifications()
            .iter()
            .any(|(_, body)| body.contains("Resumed")),
        "a forced resume must notify"
    );
}

#[test]
fn max_freeze_also_expires_on_a_wall_clock_jump_even_when_monotonic_time_barely_moved() {
    // Some platforms' `Instant` excludes time spent suspended, so a real 10+ minute machine
    // sleep can advance monotonic `now` by only a few seconds. `tick` also force-expires a max
    // freeze once the snapshot's wall-clock `sampled_at_ms` has moved >=600s past `frozen_at_ms`
    // (`frozen_at_ms` is real wall-clock time, set when `freeze` runs, not derived from `now`),
    // independent of the monotonic branch, so a forced resume still bounds the pause across a
    // suspend/wake even then.
    let home = TestHome::new("max-freeze-wall-clock");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(1200, 1);
    let work_root = id(1201, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:12", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:12",
        "a:12",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:12", "w:12"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("freeze tick");
    assert_eq!(guardian.frozen.len(), 1);
    let frozen_at_ms = guardian.frozen[0].frozen_at_ms;

    // Monotonic time barely moves -- well under the 10-minute bound -- but wall-clock time has
    // jumped forward by exactly the max-freeze bound, as it would across a real suspend/wake.
    let woke = now + Duration::from_secs(5);
    let mut none = snap;
    none.pressure = None;
    none.processes[0].stopped = true;
    none.status.sampled_at_ms = frozen_at_ms + 600_000;
    guardian
        .tick(woke, &none, &mut platform, &mut attributor, &mut log)
        .expect("tick after wall-clock jump");
    assert!(
        guardian.frozen.is_empty(),
        "a >=600s wall-clock jump must force a resume even when monotonic time barely advanced"
    );
    assert_eq!(platform.continued(), vec![work_root]);
    assert!(guardian.batch_running(&none.attribution, &none.processes));
    assert_eq!(guardian.ineligible.get("w:12"), Some(&(woke + INELIGIBLE)));
}

#[test]
fn manual_resume_marks_the_workload_ineligible_for_five_minutes() {
    let home = TestHome::new("manual-resume-ineligible");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(1100, 1);
    let work_root = id(1101, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:11", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:11",
        "a:11",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:11", "w:11"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("freeze tick");
    assert_eq!(guardian.frozen.len(), 1);

    let resumed_at = now + Duration::from_secs(1);
    guardian
        .resume(Some("w:11"), resumed_at, &platform, &mut log)
        .expect("manual resume");
    assert!(guardian.frozen.is_empty());
    assert_eq!(platform.continued(), vec![work_root]);

    // Still eligible-looking (running, un-frozen) but within 5 minutes of its forced resume:
    // must not be refrozen even though pressure and fault share still clear every other gate.
    now = resumed_at + Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick inside ineligibility window");
    assert!(
        guardian.frozen.is_empty(),
        "must not refreeze a workload force-resumed under 5 minutes ago"
    );

    // Past 5 minutes: eligible again.
    now = resumed_at + Duration::from_secs(301);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("tick past ineligibility window");
    assert_eq!(
        guardian.frozen.len(),
        1,
        "past 5 minutes the workload is eligible again"
    );
}

// ---------------------------------------------------------------------
// FIFO resume at Normal, one at a time, five seconds apart.
// ---------------------------------------------------------------------

#[test]
fn resumes_frozen_workloads_fifo_one_at_a_time_five_seconds_apart() {
    let home = TestHome::new("fifo-resume");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let agent_root = id(1200, 1);
    let early = id(1201, 1);
    let late = id(1202, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:12", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:early",
        "a:12",
        early,
        WorkloadClass::Batch,
        100,
        GIB,
        Some(1000),
    ));
    attribution.workloads.push(workload(
        "w:late",
        "a:12",
        late,
        WorkloadClass::Batch,
        200,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(early, "a:12", "w:early"));
    attribution
        .processes
        .push(workload_attribution(late, "a:12", "w:late"));
    let processes = vec![
        process(early, agent_root.pid, GIB),
        process(late, agent_root.pid, GIB),
    ];

    // Equal growth, so victim choice ties on `first_seen_ms` and picks the newer workload
    // first (see `victim_choice_among_batches_ties_broken_by_newest`) -- `w:late` therefore
    // becomes the *older* frozen entry, resumed before `w:early`.
    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("freeze the newest-tiebroken candidate");
    assert_eq!(guardian.frozen.len(), 1);
    assert_eq!(guardian.frozen[0].workload_id, "w:late");

    // Past cooldown, with the other workload still eligible (freezing one does not stop the
    // other from being observed as still running here -- membership tracking is exercised
    // elsewhere).
    now += Duration::from_secs(5);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("freeze the remaining candidate");
    assert_eq!(
        guardian.frozen.len(),
        2,
        "both workloads must now be frozen"
    );
    let fifo_order: Vec<_> = guardian
        .frozen
        .iter()
        .map(|w| w.workload_id.clone())
        .collect();
    assert_eq!(fifo_order, vec!["w:late", "w:early"]);

    // Walk pressure back down to Normal: `cool_to_normal` reaches Normal only after both 10s
    // exit-hysteresis stages complete, and that same final tick already resumes the
    // oldest-frozen (`w:late`) workload as a side effect, since nothing has resumed yet.
    let cooled_at = cool_to_normal(
        &mut guardian,
        &mut platform,
        &mut attributor,
        &mut log,
        now + Duration::from_secs(1),
    );
    assert_eq!(
        guardian.frozen.len(),
        1,
        "only the oldest-frozen workload resumes"
    );
    assert_eq!(guardian.frozen[0].workload_id, "w:early");
    assert_eq!(platform.continued(), vec![late]);

    // A resume signal and the platform's own next process scan are not atomic: the very next
    // listing can still observe `late` as stopped even though `SIGCONT` already went out. This
    // tick's `resumed_this_tick` must make `batch_running` read `late` as running immediately
    // anyway, off that same stale, still-stopped snapshot, rather than waiting for a fresh scan.
    let mut stale_attribution = AttributionSnapshot::default();
    stale_attribution
        .agents
        .push(agent("a:12", agent_root, 6 * GIB));
    stale_attribution.workloads.push(workload(
        "w:late",
        "a:12",
        late,
        WorkloadClass::Batch,
        200,
        GIB,
        Some(1000),
    ));
    stale_attribution
        .processes
        .push(workload_attribution(late, "a:12", "w:late"));
    let stale_processes = vec![Process {
        stopped: true,
        ..process(late, agent_root.pid, GIB)
    }];
    assert!(
        guardian.batch_running(&stale_attribution, &stale_processes),
        "a just-resumed batch workload must read as running even off a still-stopped snapshot"
    );

    // Immediately again: the 5s inter-resume cooldown must block the second resume.
    let blocked_at = cooled_at + Duration::from_secs(1);
    guardian
        .tick(
            blocked_at,
            &quiet_normal_snapshot(),
            &mut platform,
            &mut attributor,
            &mut log,
        )
        .expect("resume tick inside cooldown");
    assert_eq!(
        guardian.frozen.len(),
        1,
        "must not resume a second workload inside 5s"
    );

    // Past 5s since the first resume: the remaining workload resumes too.
    let past_cooldown = cooled_at + Duration::from_secs(5);
    guardian
        .tick(
            past_cooldown,
            &quiet_normal_snapshot(),
            &mut platform,
            &mut attributor,
            &mut log,
        )
        .expect("second resume tick");
    assert!(guardian.frozen.is_empty());
    assert_eq!(platform.continued(), vec![late, early]);
}

// ---------------------------------------------------------------------
// Observe mode: identical decisions, no signals, no persistence, no OS notification.
// ---------------------------------------------------------------------

#[test]
fn observe_mode_reaches_the_same_freeze_decision_without_acting_on_it() {
    let enforce_home = TestHome::new("observe-parity-enforce");
    let mut enforce_guardian = Guardian::new(
        enforce_home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut enforce_platform = FakePlatform::new("boot-1");
    let mut enforce_attributor = Attributor::new(Vec::new(), Vec::new());
    let mut enforce_log = enforce_home.log();
    let (mut enforce_now, mut enforce_step) = warm_to_critical(
        &mut enforce_guardian,
        &mut enforce_platform,
        &mut enforce_attributor,
        &mut enforce_log,
    );

    let observe_home = TestHome::new("observe-parity-observe");
    let mut observe_guardian = Guardian::new(
        observe_home.0.clone(),
        "boot-1".into(),
        Mode::Observe,
        Thresholds::default(),
    );
    let mut observe_platform = FakePlatform::new("boot-1");
    let mut observe_attributor = Attributor::new(Vec::new(), Vec::new());
    let mut observe_log = observe_home.log();
    let (mut observe_now, mut observe_step) = warm_to_critical(
        &mut observe_guardian,
        &mut observe_platform,
        &mut observe_attributor,
        &mut observe_log,
    );

    let agent_root = id(1300, 1);
    let work_root = id(1301, 1);
    let build = |now: &mut Instant, step: &mut u64| {
        *now += Duration::from_secs(1);
        *step += 1;
        let mut attribution = AttributionSnapshot::default();
        attribution.agents.push(agent("a:13", agent_root, 6 * GIB));
        attribution.workloads.push(workload(
            "w:13",
            "a:13",
            work_root,
            WorkloadClass::Batch,
            0,
            GIB,
            Some(1000),
        ));
        attribution
            .processes
            .push(workload_attribution(work_root, "a:13", "w:13"));
        let processes = vec![process(work_root, agent_root.pid, GIB)];
        (
            *now,
            snapshot(Some(heavy_swap(*step)), attribution, processes),
        )
    };

    let (enforce_now2, enforce_snap) = build(&mut enforce_now, &mut enforce_step);
    enforce_guardian
        .tick(
            enforce_now2,
            &enforce_snap,
            &mut enforce_platform,
            &mut enforce_attributor,
            &mut enforce_log,
        )
        .expect("enforce tick");
    let (observe_now2, observe_snap) = build(&mut observe_now, &mut observe_step);
    observe_guardian
        .tick(
            observe_now2,
            &observe_snap,
            &mut observe_platform,
            &mut observe_attributor,
            &mut observe_log,
        )
        .expect("observe tick");

    assert_eq!(enforce_guardian.level, observe_guardian.level);
    assert_eq!(
        enforce_guardian.frozen.len(),
        observe_guardian.frozen.len(),
        "observe mode must record the same freeze decision as enforce"
    );
    assert_eq!(
        enforce_guardian.frozen[0].workload_id,
        observe_guardian.frozen[0].workload_id
    );

    assert_eq!(
        enforce_platform.stopped(),
        vec![work_root],
        "enforce must actually signal"
    );
    assert!(
        observe_platform.stopped().is_empty(),
        "observe must never signal"
    );
    assert!(
        observe_platform.notifications().is_empty(),
        "observe must never notify the OS"
    );
    assert!(
        std::fs::metadata(observe_home.0.base.join("state/frozen.json")).is_err(),
        "observe must never write the crash journal"
    );
    assert!(
        std::fs::metadata(enforce_home.0.base.join("state/frozen.json")).is_ok(),
        "enforce must have written the crash journal"
    );
    enforce_guardian
        .resume(None, enforce_now2, &enforce_platform, &mut enforce_log)
        .unwrap();
    observe_guardian
        .resume(None, observe_now2, &observe_platform, &mut observe_log)
        .unwrap();
    assert_eq!(observe_platform.signal_count(), 0);
    assert!(observe_platform.notifications().is_empty());
    assert!(!observe_home.0.base.join("state/frozen.json").exists());
    let decisions = |home: &TestHome| {
        std::fs::read_to_string(home.0.base.join("log/decisions.jsonl"))
            .unwrap()
            .lines()
            .map(|line| {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                (
                    row["event"].clone(),
                    row["details"]["decision"].clone(),
                    row["details"]["reason"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(decisions(&enforce_home), decisions(&observe_home));
}

// ---------------------------------------------------------------------
// Persistence ordering and crash-recovery-adjacent failure handling.
// ---------------------------------------------------------------------

/// A freshly forked child appearing between freeze passes must be journaled to
/// `state/frozen.json` before it is ever sent `SIGSTOP` -- so a crash between the write and
/// the signal can never lose track of a process that might already be stopped.
///
/// This is the one scenario that needs the *real* `Attributor` rather than hand-built
/// attribution: the repeated pass re-derives membership from a fresh process scan, and only the
/// real marker/shell ancestry rules can recognize the late child as belonging to the same
/// workload as the process that was already frozen.
#[test]
fn late_children_are_persisted_before_their_own_stop_signal() {
    let home = TestHome::new("late-child-persist");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());

    let claude_root = id(1400, 1);
    let work_root = id(1401, 1);
    let late_child = id(1402, 1);
    let claude_env = &[
        ("CLAUDE_CODE_SESSION_ID", "sess-guardian"),
        ("CLAUDE_PID", "1400"),
    ];
    let mut platform = FakePlatform::new("boot-1").env(claude_root, claude_env);
    platform.journal = Some(home.0.clone());

    // The CLAUDE marker's root is only confirmed when the claimant's own exe also matches a
    // recognized "claude" binary (see `markers.toml`'s `root_binaries`); a generic exe leaves
    // the root unconfirmed and the root process falls back to starting its own bogus workload.
    let claude_root_process = |identity: ProcessIdentity| Process {
        exe: Some("/usr/bin/claude".into()),
        argv: Some(Vec::new()),
        ..process(identity, 1, 0)
    };
    let workload_shell_process = |identity: ProcessIdentity, ppid: i32| Process {
        exe: Some("/bin/bash".into()),
        argv: Some(vec!["bash".into(), "-c".into(), "long-build".into()]),
        ..process(identity, ppid, GIB)
    };

    // Prime the real attributor so `work_root` is already resolved as a running `Workload`
    // before the guardian ever sees it, matching how the daemon's own tick loop always calls
    // `attributor.update` before `guardian.tick`.
    let mut priming = [
        claude_root_process(claude_root),
        workload_shell_process(work_root, claude_root.pid),
    ];
    let primed = attributor.update(&platform, &mut priming, Instant::now(), 0, false);
    assert_eq!(
        primed
            .processes
            .iter()
            .find(|a| a.identity == claude_root)
            .expect("claude_root must be attributed")
            .role,
        ProcessRole::AgentRoot,
        "fixture sanity check: the agent root itself must never be a workload"
    );
    let work_attr = primed
        .processes
        .iter()
        .find(|a| a.identity == work_root)
        .expect("work_root must be attributed");
    assert_eq!(
        work_attr.role,
        ProcessRole::Workload,
        "fixture sanity check"
    );
    let workload_id = work_attr.workload_id.clone().unwrap();

    // Between the freeze's first and second pass, `late_child` (forked from `work_root`) shows
    // up in the rescan -- the real attributor must recognize it as the same workload by ancestry.
    platform = platform.rescan_with(vec![
        claude_root_process(claude_root),
        workload_shell_process(work_root, claude_root.pid),
        process(late_child, work_root.pid, GIB),
    ]);

    let processes = vec![process(work_root, claude_root.pid, GIB)];
    let snap = snapshot(None, primed, processes);
    guardian
        .freeze(
            snap.attribution
                .workloads
                .iter()
                .find(|w| w.id == workload_id)
                .expect("workload present in the primed snapshot"),
            &snap,
            Instant::now(),
            &mut platform,
            &mut attributor,
        )
        .expect("freeze");

    assert_eq!(
        guardian.frozen[0].processes.iter().collect::<HashSet<_>>(),
        HashSet::from([&work_root, &late_child]),
        "the late child must end up frozen alongside the original member"
    );

    let saved = recovery::read(&home.0).expect("frozen.json must exist after an enforce freeze");
    assert_eq!(saved.boot_id, "boot-1");
    let journaled: HashSet<_> = saved
        .workloads
        .iter()
        .find(|w| w.workload_id == workload_id)
        .expect("workload journaled")
        .processes
        .iter()
        .collect();
    assert_eq!(journaled, HashSet::from([&work_root, &late_child]));
    assert_eq!(
        platform.stopped().into_iter().collect::<HashSet<_>>(),
        HashSet::from([work_root, late_child])
    );
}

#[test]
fn a_failed_resume_signal_leaves_the_frozen_journal_entry_in_place() {
    let home = TestHome::new("failed-resume-retains-journal");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();

    let agent_root = id(1500, 1);
    let work_root = id(1501, 1);
    let mut platform = FakePlatform::new("boot-1");
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut platform, &mut attributor, &mut log);

    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:15", agent_root, 6 * GIB));
    attribution.workloads.push(workload(
        "w:15",
        "a:15",
        work_root,
        WorkloadClass::Batch,
        0,
        GIB,
        Some(1000),
    ));
    attribution
        .processes
        .push(workload_attribution(work_root, "a:15", "w:15"));
    let processes = vec![process(work_root, agent_root.pid, GIB)];
    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), attribution, processes);
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("freeze tick");
    assert_eq!(guardian.frozen.len(), 1);
    let signals_before_resume = platform.signal_count();

    // From here on, resuming `work_root` fails (e.g. a transient permission error).
    let failing_platform = FakePlatform::new("boot-1").fail_signal(work_root);
    let error = guardian
        .resume(
            Some("w:15"),
            now + Duration::from_secs(1),
            &failing_platform,
            &mut log,
        )
        .expect_err("a failed signal must surface as an error");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

    assert_eq!(
        guardian.frozen.len(),
        1,
        "a failed resume must not drop the in-memory entry"
    );
    assert_eq!(guardian.frozen[0].workload_id, "w:15");
    let saved = recovery::read(&home.0).expect("frozen.json must still exist");
    assert_eq!(
        saved
            .workloads
            .iter()
            .map(|w| w.workload_id.clone())
            .collect::<Vec<_>>(),
        vec!["w:15".to_string()],
        "the on-disk journal must still list the workload the failed resume could not clear"
    );
    assert_eq!(
        platform.signal_count(),
        signals_before_resume,
        "the failure came from a different platform handle; the original recorded no more calls"
    );
}

// ---------------------------------------------------------------------
// `watched()` and `batch_running()`.
// ---------------------------------------------------------------------

#[test]
fn watched_returns_every_frozen_process_across_all_workloads() {
    // `FrozenWorkload.since` is private to `guardian`, not `pub`, but this test module is a
    // descendant of it and may construct one directly -- no real freeze needed for this fixture.
    let home = TestHome::new("watched");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    assert!(guardian.watched().is_empty());
    guardian.frozen.push(FrozenWorkload {
        workload_id: "w:a".into(),
        root: id(1, 1),
        processes: vec![id(1, 1), id(2, 1)],
        frozen_at_ms: 0,
        since: Instant::now(),
    });
    guardian.frozen.push(FrozenWorkload {
        workload_id: "w:b".into(),
        root: id(3, 1),
        processes: vec![id(3, 1)],
        frozen_at_ms: 0,
        since: Instant::now(),
    });
    assert_eq!(
        guardian.watched(),
        HashSet::from([id(1, 1), id(2, 1), id(3, 1)])
    );
}

#[test]
fn batch_running_is_true_only_while_a_batch_workload_is_actually_running() {
    let home = TestHome::new("batch-running");
    let guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );

    let agent_root = id(1600, 1);
    let batch_root = id(1601, 1);
    let mut attribution = AttributionSnapshot::default();
    attribution.agents.push(agent("a:16", agent_root, GIB));
    attribution.workloads.push(workload(
        "w:16",
        "a:16",
        batch_root,
        WorkloadClass::Batch,
        0,
        GIB,
        None,
    ));
    attribution
        .processes
        .push(workload_attribution(batch_root, "a:16", "w:16"));
    let running = vec![process(batch_root, agent_root.pid, GIB)];
    assert!(guardian.batch_running(&attribution, &running));

    let mut stopped_process = process(batch_root, agent_root.pid, GIB);
    stopped_process.stopped = true;
    assert!(
        !guardian.batch_running(&attribution, &[stopped_process]),
        "a stopped member does not count as running"
    );

    let mut service_only = AttributionSnapshot::default();
    service_only.agents.push(agent("a:16b", agent_root, GIB));
    service_only.workloads.push(workload(
        "w:16b",
        "a:16b",
        batch_root,
        WorkloadClass::Service,
        0,
        GIB,
        None,
    ));
    service_only
        .processes
        .push(workload_attribution(batch_root, "a:16b", "w:16b"));
    assert!(
        !guardian.batch_running(&service_only, &running),
        "a running service alone must not count as a running batch"
    );
}

#[test]
fn notifications_are_limited_per_kind_to_one_per_minute() {
    let home = TestHome::new("notification-interval");
    let mut guardian = Guardian::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let platform = FakePlatform::new("boot-1");
    let mut log = home.log();
    let now = Instant::now();
    for seconds in [0, 1, 59, 60] {
        guardian.notify(
            "freeze",
            "paused",
            now + Duration::from_secs(seconds),
            &platform,
            &mut log,
        );
    }
    guardian.notify(
        "forced_resume",
        "resumed",
        now + Duration::from_secs(60),
        &platform,
        &mut log,
    );
    assert_eq!(platform.notifications().len(), 3);
}

#[path = "review_tests.rs"]
mod review_tests;
