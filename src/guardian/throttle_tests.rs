//! Scenario tests for `guardian::throttle` (ticket 17). Self-contained `FakePlatform` (this
//! module hangs off `throttle`, not `guardian::tests`, so it can't reuse that one). The real
//! native/core end-to-end scenario and the real CLI offline-recovery scenario both live in
//! `tests/guardian_recovery.rs` instead (see the comment near the bottom of this file).

use super::*;
use crate::attribution::{AttributionSnapshot, MemorySummary, ProcessAttribution, Workload};
use crate::daemon::files::Config;
use crate::daemon::{ProcessChanges, Status};
use crate::platform::{Capabilities, Environment, PressureInputs, ProcessMetrics, Signal};
use std::cell::RefCell;
use std::path::PathBuf;

// FakePlatform

#[derive(Default)]
struct FakePlatform {
    boot_id: String,
    rescan: Vec<Process>,
    bg: RefCell<HashMap<ProcessIdentity, bool>>,
    io_bytes: HashMap<ProcessIdentity, u64>,
    gone: HashSet<ProcessIdentity>,
    fail: HashSet<ProcessIdentity>,
    set_backgrounded_calls: RefCell<Vec<ProcessIdentity>>,
    /// When set, every `set_backgrounded(id, true)` asserts `id` is already in this journal --
    /// the same invariant `guardian::tests`'s FakePlatform checks for SIGSTOP.
    journal: Option<Paths>,
}
impl FakePlatform {
    fn new(boot_id: &str) -> Self {
        Self {
            boot_id: boot_id.into(),
            ..Self::default()
        }
    }
    fn rescan_with(mut self, processes: Vec<Process>) -> Self {
        self.rescan = processes;
        self
    }
    fn set_rescan(&mut self, processes: Vec<Process>) {
        self.rescan = processes;
    }
    fn backgrounded_from(self, id: ProcessIdentity, value: bool) -> Self {
        self.bg.borrow_mut().insert(id, value);
        self
    }
    /// Seeds state directly, bypassing `set_backgrounded_calls` -- for simulating a real OS
    /// fork-inherits-BG effect the test itself didn't cause, not a call Ballast made.
    fn seed_backgrounded(&mut self, id: ProcessIdentity, value: bool) {
        self.bg.borrow_mut().insert(id, value);
    }
    fn gone(mut self, id: ProcessIdentity) -> Self {
        self.gone.insert(id);
        self
    }
    fn fail(mut self, id: ProcessIdentity) -> Self {
        self.fail.insert(id);
        self
    }
    fn with_journal(mut self, paths: Paths) -> Self {
        self.journal = Some(paths);
        self
    }
    fn is_backgrounded(&self, id: ProcessIdentity) -> bool {
        self.bg.borrow().get(&id).copied().unwrap_or(false)
    }
    fn set_backgrounded_calls(&self) -> Vec<ProcessIdentity> {
        self.set_backgrounded_calls.borrow().clone()
    }
}
impl Platform for FakePlatform {
    fn supports_throttle(&self) -> bool {
        true
    }
    fn backgrounded(&self, id: ProcessIdentity) -> io::Result<bool> {
        if self.gone.contains(&id) {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        if self.fail.contains(&id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fake backgrounded failure",
            ));
        }
        Ok(self.is_backgrounded(id))
    }
    fn set_backgrounded(&self, id: ProcessIdentity, enabled: bool) -> io::Result<()> {
        self.set_backgrounded_calls.borrow_mut().push(id);
        if enabled && let Some(paths) = &self.journal {
            let saved = read(paths).expect("journal must precede every native apply");
            assert!(
                saved.workloads.iter().any(|w| w.processes.contains(&id)),
                "background-apply identity must already be journaled: {id:?}"
            );
        }
        if self.gone.contains(&id) {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        if self.fail.contains(&id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fake set_backgrounded failure",
            ));
        }
        self.bg.borrow_mut().insert(id, enabled);
        Ok(())
    }
    fn process_io_bytes(&self, id: ProcessIdentity) -> Option<u64> {
        self.io_bytes.get(&id).copied()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            environment: false,
            listening_ports: false,
            memory_footprint: false,
            memory_psi: false,
            kernel_pressure: false,
            notifications: false,
            atomic_signals: false,
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
    fn read_environment(&self, _process: ProcessIdentity) -> Option<Environment> {
        None
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
    fn send_signal(&self, _process: ProcessIdentity, _signal: Signal) -> io::Result<()> {
        panic!("throttle never sends Stop/Continue signals")
    }
    fn notify(&self, _title: &str, _body: &str) -> io::Result<bool> {
        Ok(true)
    }
}

// Fixtures

struct TestHome(Paths);
impl TestHome {
    fn new(tag: &str) -> Self {
        let path = PathBuf::from(format!("/tmp/blt-throttle-{tag}-{}", std::process::id()));
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

fn id(pid: i32, start_time: u64) -> ProcessIdentity {
    ProcessIdentity { pid, start_time }
}
fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn process(identity: ProcessIdentity, ppid: i32) -> Process {
    Process {
        identity,
        ppid,
        pgid: identity.pid,
        uid: own_uid(),
        stopped: false,
        name: None,
        exe: None,
        argv: None,
        metrics: Some(ProcessMetrics {
            memory_bytes: 0,
            cpu_time_ns: 0,
        }),
    }
}
fn process_with_cpu(identity: ProcessIdentity, ppid: i32, cpu_time_ns: u64) -> Process {
    Process {
        metrics: Some(ProcessMetrics {
            memory_bytes: 0,
            cpu_time_ns,
        }),
        ..process(identity, ppid)
    }
}
fn workload_attribution(identity: ProcessIdentity, workload_id: &str) -> ProcessAttribution {
    ProcessAttribution {
        identity,
        owner_id: None,
        agent_id: Some("agent".into()),
        workload_id: Some(workload_id.into()),
        role: ProcessRole::Workload,
        environment_known: true,
        listening_ports: None,
        ports_sampled_at_ms: None,
    }
}
fn internal_attribution(identity: ProcessIdentity) -> ProcessAttribution {
    ProcessAttribution {
        identity,
        owner_id: None,
        agent_id: Some("agent".into()),
        workload_id: None,
        role: ProcessRole::AgentInternal,
        environment_known: true,
        listening_ports: None,
        ports_sampled_at_ms: None,
    }
}
fn agent_root_attribution(identity: ProcessIdentity) -> ProcessAttribution {
    ProcessAttribution {
        role: ProcessRole::AgentRoot,
        ..internal_attribution(identity)
    }
}
fn workload(id: &str, root: ProcessIdentity, class: WorkloadClass) -> Workload {
    Workload {
        id: id.into(),
        agent_id: "agent".into(),
        root,
        label: id.into(),
        class,
        first_seen_ms: 0,
        detached_pgid: None,
        memory: MemorySummary {
            bytes: 0,
            complete: true,
            growth_30s_bytes: None,
            growth_bytes_per_sec: None,
        },
    }
}
fn attribution(
    workloads: Vec<Workload>,
    processes: Vec<ProcessAttribution>,
) -> AttributionSnapshot {
    AttributionSnapshot {
        owners: Vec::new(),
        agents: Vec::new(),
        workloads,
        processes,
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
        today: Default::default(),
    }
}

/// CPU-only pressure: `cpu_busy_fraction` settles at 0.95 (> the 0.9 default threshold) and
/// `load_per_core` at 2.0 (> the 1.0 default threshold); I/O stays flat at zero so `io_level`
/// never leaves `Normal`. The busy/total *ratio* is what crosses the threshold, so it holds
/// regardless of real wall-clock spacing between calls -- only the step deltas matter.
fn heavy_cpu(step: u64) -> PressureInputs {
    PressureInputs {
        throttle: Some(Inputs {
            cpu_busy_ticks: step * 950,
            cpu_total_ticks: step * 1000,
            cpu_count: 1,
            load_per_core: 2.0,
            io_time_ns: Some(0),
            io_bytes: Some(0),
            io_devices: 1,
        }),
        ..Default::default()
    }
}
/// A sample with no CPU busy delta at all -- `cpu_busy_fraction` reads 0.0, well under
/// threshold, so this always drives `cpu_level` back toward `Normal` (given the 10s exit
/// hysteresis is satisfied).
fn quiet_cpu() -> PressureInputs {
    PressureInputs {
        throttle: Some(Inputs {
            cpu_busy_ticks: 0,
            cpu_total_ticks: 1000,
            cpu_count: 1,
            load_per_core: 0.1,
            io_time_ns: Some(0),
            io_bytes: Some(0),
            io_devices: 1,
        }),
        ..Default::default()
    }
}

// 1. Ancestry identity safety (the native "identity mismatch" check).

#[test]
fn descendants_never_follows_into_a_pid_reused_by_an_unrelated_process() {
    // The journaled root's pid (100) has since been reused by an unrelated live process with a
    // different start_time; a child of *that* unrelated process must never be treated as a
    // descendant of the stale, journaled identity.
    let stale_root = id(100, 1);
    let live_unrelated = id(100, 2);
    let child_of_unrelated = id(101, 1);
    let processes = vec![
        process(live_unrelated, 1),
        process(child_of_unrelated, live_unrelated.pid),
    ];
    let found = descendants(&processes, &[stale_root]);
    assert_eq!(
        found,
        HashSet::from([stale_root]),
        "a reused pid must not extend a stale identity's ancestry"
    );
}

// 2. Recovery: only owned descendants are restored; preserved ones are never touched.

#[test]
fn recover_restores_only_owned_descendants_and_leaves_preserved_ones_untouched() {
    let home = TestHome::new("recover-preserved");
    let root = id(200, 1);
    let owned_child = id(201, 1);
    let preserved_child = id(202, 1); // already externally backgrounded before Ballast touched it
    let preserved_grandchild = id(203, 1); // a descendant of the preserved child

    let mut platform = FakePlatform::new("boot-1")
        .rescan_with(vec![
            process(root, 1),
            process(owned_child, root.pid),
            process(preserved_child, root.pid),
            process(preserved_grandchild, preserved_child.pid),
        ])
        .backgrounded_from(preserved_child, true)
        .backgrounded_from(preserved_grandchild, true);

    write(
        &home.0,
        "boot-1",
        &[ThrottledWorkload {
            workload_id: "w1".into(),
            root,
            processes: vec![root, owned_child],
            preserved: vec![preserved_child],
        }],
    )
    .unwrap();

    let mut log = home.log();
    let resumed = recover(&home.0, &mut platform, &mut log).expect("recovery must succeed");

    assert_eq!(resumed, 2, "root and its owned child must both be restored");
    assert!(!platform.is_backgrounded(root));
    assert!(!platform.is_backgrounded(owned_child));
    assert!(
        platform.is_backgrounded(preserved_child),
        "a pre-existing external BG must never be cleared"
    );
    assert!(
        platform.is_backgrounded(preserved_grandchild),
        "a descendant of a preserved process must stay untouched too"
    );
    assert!(
        !platform.set_backgrounded_calls().contains(&preserved_child)
            && !platform
                .set_backgrounded_calls()
                .contains(&preserved_grandchild),
        "recovery must never even call set_backgrounded on a preserved identity"
    );

    let journal = read(&home.0).expect("journal read");
    assert!(
        journal.workloads.is_empty(),
        "a fully-restored workload must be dropped from the journal"
    );
}

#[test]
fn recover_tolerates_a_member_that_already_exited_without_failing_the_whole_recovery() {
    // A race, not a bug: the member exited between the last scan and recovery acting on it.
    // Fail-open means this must be silently dropped, never surfaced as a recovery error.
    let home = TestHome::new("recover-gone");
    let root = id(500, 1);
    let exited_child = id(501, 1);
    let mut platform = FakePlatform::new("boot-1")
        .rescan_with(vec![process(root, 1), process(exited_child, root.pid)])
        .gone(exited_child);

    write(
        &home.0,
        "boot-1",
        &[ThrottledWorkload {
            workload_id: "w1".into(),
            root,
            processes: vec![root, exited_child],
            preserved: Vec::new(),
        }],
    )
    .unwrap();

    let mut log = home.log();
    let resumed = recover(&home.0, &mut platform, &mut log)
        .expect("an already-exited member must not fail recovery");
    assert_eq!(
        resumed, 1,
        "only the still-live member is actually restored"
    );
    assert!(!platform.is_backgrounded(root));

    let journal = read(&home.0).expect("journal read");
    assert!(
        journal.workloads.is_empty(),
        "the workload must still be fully cleared from the journal"
    );
}

// 3. Controller hysteresis: 2 samples to enter Elevated.

#[test]
fn cpu_level_needs_two_consecutive_over_threshold_samples_to_enter_elevated() {
    let home = TestHome::new("hysteresis-enter");
    let mut controller = Controller::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        true,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut log = home.log();
    let now = Instant::now();
    let empty = attribution(Vec::new(), Vec::new());

    controller
        .tick(
            now,
            &snapshot(Some(heavy_cpu(0)), empty.clone(), Vec::new()),
            &mut platform,
            &mut log,
        )
        .unwrap();
    assert_eq!(
        controller.view.cpu_level,
        Level::Normal,
        "a lone baseline sample has no rate to judge"
    );

    controller
        .tick(
            now + Duration::from_secs(1),
            &snapshot(Some(heavy_cpu(1)), empty.clone(), Vec::new()),
            &mut platform,
            &mut log,
        )
        .unwrap();
    assert_eq!(
        controller.view.cpu_level,
        Level::Normal,
        "one over-threshold rate sample must not promote yet"
    );

    controller
        .tick(
            now + Duration::from_secs(2),
            &snapshot(Some(heavy_cpu(2)), empty, Vec::new()),
            &mut platform,
            &mut log,
        )
        .unwrap();
    assert_eq!(
        controller.view.cpu_level,
        Level::Elevated,
        "a second consecutive over-threshold sample must promote"
    );
}

// 4. Fault gating: throttling only fires once the agent's own share of the busy CPU crosses    agent_resource_share, not merely because the host is Elevated.

fn run_to_elevated_with_agent_share(
    tag: &str,
    ns_per_step: u64,
) -> (
    Controller,
    FakePlatform,
    RotatingLog,
    TestHome,
    ProcessIdentity,
) {
    // Each caller passes its own tag: tests run in parallel within one process, and TestHome's
    // Drop removes its directory, so two tests sharing a path would race each other's cleanup.
    let home = TestHome::new(tag);
    let mut controller = Controller::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        true,
        Thresholds::default(),
    );
    let root = id(300, 1);
    let mut platform = FakePlatform::new("boot-1").rescan_with(vec![process(root, 1)]);
    let mut log = home.log();
    let now = Instant::now();
    let attrib = attribution(
        vec![workload("w1", root, WorkloadClass::Batch)],
        vec![workload_attribution(root, "w1")],
    );
    let step = |n: u64| {
        snapshot(
            Some(heavy_cpu(n)),
            attrib.clone(),
            vec![process_with_cpu(root, 1, n * ns_per_step)],
        )
    };

    for n in 0..=2 {
        controller
            .tick(
                now + Duration::from_secs(n),
                &step(n),
                &mut platform,
                &mut log,
            )
            .unwrap();
    }
    (controller, platform, log, home, root)
}

#[test]
fn an_elevated_host_does_not_throttle_a_workload_below_the_agent_share_threshold() {
    // ~0.1/0.95 ~= 10.5% share, well under the 30% default threshold.
    let (controller, _platform, _log, _home, _root) =
        run_to_elevated_with_agent_share("below-share", 100_000_000);
    assert_eq!(
        controller.view.cpu_level,
        Level::Elevated,
        "sanity: the host must actually be Elevated"
    );
    assert!(
        controller.view.workloads.is_empty(),
        "a workload under the agent-share threshold must not be throttled"
    );
}

#[test]
fn an_elevated_host_throttles_a_workload_at_or_above_the_agent_share_threshold() {
    // ~0.5/0.95 ~= 52.6% share, comfortably over the 30% default threshold.
    let (controller, platform, _log, _home, root) =
        run_to_elevated_with_agent_share("above-share", 500_000_000);
    assert_eq!(controller.view.workloads.len(), 1);
    assert_eq!(controller.view.workloads[0].workload_id, "w1");
    assert!(platform.is_backgrounded(root));
}

// 5. A workload no longer eligible (its member's role/attribution changed) is released even    while the host is still Elevated.

#[test]
fn a_workload_that_becomes_ineligible_is_released_even_while_still_elevated() {
    let (mut controller, platform, mut log, home, root) =
        run_to_elevated_with_agent_share("ineligible", 500_000_000);
    assert_eq!(
        controller.view.workloads.len(),
        1,
        "sanity: the workload must have been throttled first"
    );

    // The same process is still alive and observed, but reattributed away from the workload
    // (e.g. the workload finished and this pid was recycled into agent-internal bookkeeping).
    let now = Instant::now() + Duration::from_secs(10);
    let attrib = attribution(Vec::new(), vec![internal_attribution(root)]);
    let snap = snapshot(Some(heavy_cpu(3)), attrib, vec![process(root, 1)]);
    let mut platform = platform;
    controller
        .tick(now, &snap, &mut platform, &mut log)
        .unwrap();

    assert!(
        controller.view.workloads.is_empty(),
        "an ineligible workload must be released"
    );
    assert!(
        !platform.is_backgrounded(root),
        "the released process must have its background policy cleared"
    );
    let _ = home; // keep TestHome alive for the duration of the log file
}

// 6. A newly eligible member that was already backgrounded by something else is preserved, not    owned: it lands in `preserved`, never in `processes`, and set_backgrounded is never called    on it.

#[test]
fn a_preexisting_externally_backgrounded_member_is_preserved_not_owned() {
    let home = TestHome::new("preserve-append");
    let mut controller = Controller::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        true,
        Thresholds::default(),
    );
    let root = id(400, 1);
    let already_bg = id(401, 1);
    let mut platform = FakePlatform::new("boot-1")
        .rescan_with(vec![process(root, 1), process(already_bg, root.pid)])
        .backgrounded_from(already_bg, true);
    let mut log = home.log();
    let now = Instant::now();
    let attrib = attribution(
        vec![workload("w1", root, WorkloadClass::Batch)],
        vec![
            workload_attribution(root, "w1"),
            workload_attribution(already_bg, "w1"),
        ],
    );
    let step = |n: u64| {
        snapshot(
            Some(heavy_cpu(n)),
            attrib.clone(),
            vec![
                process_with_cpu(root, 1, n * 500_000_000),
                process(already_bg, root.pid),
            ],
        )
    };
    for n in 0..=2 {
        controller
            .tick(
                now + Duration::from_secs(n),
                &step(n),
                &mut platform,
                &mut log,
            )
            .unwrap();
    }

    assert_eq!(controller.view.workloads.len(), 1);
    let w = &controller.view.workloads[0];
    assert!(w.processes.contains(&root) && !w.processes.contains(&already_bg));
    assert!(w.preserved.contains(&already_bg));
    assert!(
        !platform.set_backgrounded_calls().contains(&already_bg),
        "a pre-existing external BG must never be set/cleared by throttling"
    );
}

// 7. The restoring latch: a failed release retries restoration on the very next tick before any    new throttling decision, even if pressure is still Elevated.

#[test]
fn a_failed_release_retries_restoration_before_any_new_throttle_decision() {
    let (mut controller, platform, mut log, _home, root) =
        run_to_elevated_with_agent_share("restoring-latch", 500_000_000);
    assert!(
        platform.is_backgrounded(root),
        "sanity: the workload must already be throttled"
    );

    // Force the next release to fail (simulating e.g. a transient platform error), then drop
    // back to Normal so `update` would otherwise call `release` directly and succeed.
    let mut failing = platform.fail(root);
    let now = Instant::now() + Duration::from_secs(20);
    let empty = attribution(Vec::new(), Vec::new());
    let quiet = snapshot(Some(quiet_cpu()), empty, Vec::new());
    let result = controller.tick(now, &quiet, &mut failing, &mut log);
    assert!(
        result.is_err(),
        "the forced set_backgrounded failure must surface as a tick error"
    );
    assert!(
        !controller.view.workloads.is_empty(),
        "the workload must remain tracked after a failed release"
    );

    // Next tick: a real platform (no forced failure) must restore the workload before anything
    // else runs, even though pressure has already dropped -- the latch, not fresh pressure
    // input, is what drives this retry.
    let mut recovered = FakePlatform::new("boot-1").rescan_with(vec![process(root, 1)]);
    // Reflect the still-backgrounded state the earlier failed attempt left behind.
    recovered = recovered.backgrounded_from(root, true);
    controller
        .tick(
            now + Duration::from_secs(1),
            &quiet,
            &mut recovered,
            &mut log,
        )
        .expect("the retried restoration must succeed");
    assert!(
        controller.view.workloads.is_empty(),
        "the latched retry must finish releasing the workload"
    );
    assert!(!recovered.is_backgrounded(root));
}

// 8. Decision log: throttle/unthrottle/transition events land with the right workload id.

#[test]
fn throttle_and_unthrottle_decisions_are_logged_with_the_workload_id() {
    let (mut controller, platform, mut log, home, root) =
        run_to_elevated_with_agent_share("decision-log", 500_000_000);
    let now = Instant::now() + Duration::from_secs(10);
    let empty = attribution(Vec::new(), Vec::new());
    let quiet = snapshot(Some(quiet_cpu()), empty, Vec::new());
    let mut platform = platform;
    controller
        .tick(now, &quiet, &mut platform, &mut log)
        .unwrap();
    let _ = root;

    let text = std::fs::read_to_string(home.0.base.join("log/decisions.jsonl")).unwrap();
    let events: Vec<(String, Option<String>)> = text
        .lines()
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            (
                row["event"].as_str().unwrap().to_owned(),
                row["details"]["decision"]["workload_id"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect();
    assert!(
        events
            .iter()
            .any(|(e, id)| e == "throttle" && id.as_deref() == Some("w1")),
        "a throttle decision for w1 must be logged: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|(e, id)| e == "unthrottle" && id.as_deref() == Some("w1")),
        "an unthrottle decision for w1 must be logged: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|(e, _)| e == "throttle_pressure_transition"),
        "a pressure transition must be logged: {events:?}"
    );
}

#[test]
fn io_level_needs_two_consecutive_over_threshold_samples_to_enter_elevated() {
    let home = TestHome::new("io-hysteresis-enter");
    let mut controller = Controller::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        true,
        Thresholds::default(),
    );
    let mut platform = FakePlatform::new("boot-1");
    let mut log = home.log();
    let now = Instant::now();
    let empty = attribution(Vec::new(), Vec::new());
    let heavy_io = |step: u64| PressureInputs {
        throttle: Some(Inputs {
            cpu_busy_ticks: 0,
            cpu_total_ticks: 1000,
            cpu_count: 1,
            load_per_core: 0.1,
            io_time_ns: Some(step * 900_000_000),
            io_bytes: Some(step),
            io_devices: 1,
        }),
        ..Default::default()
    };
    for (n, expect) in [(0, Level::Normal), (1, Level::Normal), (2, Level::Elevated)] {
        controller
            .tick(
                now + Duration::from_secs(n),
                &snapshot(Some(heavy_io(n)), empty.clone(), Vec::new()),
                &mut platform,
                &mut log,
            )
            .unwrap();
        assert_eq!(controller.view.io_level, expect, "sample {n}");
    }
}

#[test]
fn an_invalid_sample_releases_everything_immediately_without_waiting_for_exit_hysteresis() {
    let (mut controller, mut platform, mut log, _home, root) =
        run_to_elevated_with_agent_share("invalid-sample", 500_000_000);
    assert!(
        platform.is_backgrounded(root),
        "sanity: the workload must already be throttled"
    );

    // A discarded sample is invalid input, not "pressure returned to normal" -- `Elevated`
    // resets its level outright on a `None` reading, unlike the 10s exit hysteresis a real quiet
    // reading would need, so this must release well before 10s have passed.
    let now = Instant::now() + Duration::from_secs(1);
    let mut discarded = snapshot(
        Some(heavy_cpu(3)),
        attribution(Vec::new(), Vec::new()),
        Vec::new(),
    );
    discarded.status.sample_discarded = true;
    controller
        .tick(now, &discarded, &mut platform, &mut log)
        .unwrap();

    assert_eq!(
        controller.view.cpu_level,
        Level::Normal,
        "a discarded sample must reset the level immediately"
    );
    assert!(
        controller.view.workloads.is_empty(),
        "and release any throttled workload immediately"
    );
    assert!(!platform.is_backgrounded(root));
}

#[test]
fn the_journal_is_written_before_any_native_apply_call() {
    let home = TestHome::new("journal-before-call");
    let mut controller = Controller::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        true,
        Thresholds::default(),
    );
    let root = id(700, 1);
    let mut platform = FakePlatform::new("boot-1")
        .rescan_with(vec![process(root, 1)])
        .with_journal(home.0.clone());
    let mut log = home.log();
    let now = Instant::now();
    let attrib = attribution(
        vec![workload("w1", root, WorkloadClass::Batch)],
        vec![workload_attribution(root, "w1")],
    );
    let step = |n: u64| {
        snapshot(
            Some(heavy_cpu(n)),
            attrib.clone(),
            vec![process_with_cpu(root, 1, n * 500_000_000)],
        )
    };
    // FakePlatform's own set_backgrounded asserts the journal already lists `root` whenever it's
    // asked to apply; reaching here without panicking is the proof the ordering held.
    for n in 0..=2 {
        controller
            .tick(
                now + Duration::from_secs(n),
                &step(n),
                &mut platform,
                &mut log,
            )
            .unwrap();
    }
    assert!(platform.is_backgrounded(root));
}

#[test]
fn a_failed_member_during_restore_is_retained_in_the_journal_for_retry() {
    let home = TestHome::new("recover-failure");
    let root = id(800, 1);
    let stuck = id(801, 1);
    let mut platform = FakePlatform::new("boot-1")
        .rescan_with(vec![process(root, 1), process(stuck, root.pid)])
        .backgrounded_from(root, true)
        .backgrounded_from(stuck, true)
        .fail(stuck);
    write(
        &home.0,
        "boot-1",
        &[ThrottledWorkload {
            workload_id: "w1".into(),
            root,
            processes: vec![root, stuck],
            preserved: Vec::new(),
        }],
    )
    .unwrap();
    let mut log = home.log();
    let result = recover(&home.0, &mut platform, &mut log);
    assert!(
        result.is_err(),
        "a real (non-`gone`) failure must surface as a recovery error"
    );
    assert!(
        !platform.is_backgrounded(root),
        "the unaffected sibling must still be cleared on the fake platform"
    );
    assert!(
        platform.set_backgrounded_calls().contains(&root),
        "clearing the sibling must actually have been attempted, not just trivially already-false"
    );
    let journal = read(&home.0).expect("journal read");
    assert_eq!(
        journal.workloads.len(),
        1,
        "the workload must be retained for a later retry, not dropped"
    );
    assert!(
        journal.workloads[0].processes.contains(&stuck),
        "the failed member must still be journaled for retry"
    );
}

// Regression for the subtree-preservation fix: a nested AgentRoot that was already backgrounded
// by something else *before* Ballast ever saw this workload must be preserved forever (live pass
// and cold recovery alike); one that forks fresh *under* an already-throttled root inherits our
// own BG via the OS fork, not a foreign policy, so it must be cleared like any other owned
// descendant -- including by the same-tick "ineligible descendant found backgrounded -> release
// the whole workload" rule, which must not linger as a cooldown past that one tick.
#[test]
fn nested_agent_root_ownership_boundary_preserves_existing_bg_and_clears_inherited_bg() {
    let home = TestHome::new("nested-agent-root");
    let mut controller = Controller::new(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        true,
        Thresholds::default(),
    );
    let root = id(600, 1);
    let existing = id(601, 1); // already alive and already backgrounded before we ever see w1
    let mut platform = FakePlatform::new("boot-1")
        .rescan_with(vec![process(root, 1), process(existing, root.pid)])
        .backgrounded_from(existing, true);
    let mut log = home.log();
    let now = Instant::now();
    let attrib = attribution(
        vec![workload("w1", root, WorkloadClass::Batch)],
        vec![
            workload_attribution(root, "w1"),
            agent_root_attribution(existing),
        ],
    );
    let step = |n: u64| {
        snapshot(
            Some(heavy_cpu(n)),
            attrib.clone(),
            vec![
                process_with_cpu(root, 1, n * 500_000_000),
                process(existing, root.pid),
            ],
        )
    };
    for n in 0..=2 {
        controller
            .tick(
                now + Duration::from_secs(n),
                &step(n),
                &mut platform,
                &mut log,
            )
            .unwrap();
    }
    assert_eq!(
        controller.view.workloads.len(),
        1,
        "sanity: the workload must have been throttled"
    );
    assert!(controller.view.workloads[0].preserved.contains(&existing));
    assert!(
        !platform.set_backgrounded_calls().contains(&existing),
        "must never even call set_backgrounded on it"
    );

    // A new AgentRoot forks under the now-throttled root: it inherits OUR OWN BG via the OS fork.
    let late = id(602, 1);
    platform.set_rescan(vec![
        process(root, 1),
        process(existing, root.pid),
        process(late, root.pid),
    ]);
    platform.seed_backgrounded(late, true);
    let attrib2 = attribution(
        vec![workload("w1", root, WorkloadClass::Batch)],
        vec![
            workload_attribution(root, "w1"),
            agent_root_attribution(existing),
            agent_root_attribution(late),
        ],
    );
    let snap3 = snapshot(
        Some(heavy_cpu(3)),
        attrib2,
        vec![
            process_with_cpu(root, 1, 3 * 500_000_000),
            process(existing, root.pid),
            process(late, root.pid),
        ],
    );
    controller
        .tick(
            now + Duration::from_secs(3),
            &snap3,
            &mut platform,
            &mut log,
        )
        .unwrap();

    // An ineligible descendant found backgrounded releases the *whole* workload that same tick,
    // even while still Elevated -- clearing the inherited BG on `late` along with the root, but
    // never touching `existing`.
    assert!(
        controller.view.workloads.is_empty(),
        "the ownership violation must release the whole workload at once"
    );
    assert!(!platform.is_backgrounded(root));
    assert!(
        !platform.is_backgrounded(late),
        "an inherited BG on an owned descendant must be cleared on release"
    );
    assert!(
        platform.is_backgrounded(existing),
        "a preserved pre-existing BG must survive the release untouched"
    );

    // No lingering cooldown: still Elevated, the very next tick may re-throttle `root` and must
    // re-discover the same preserved/owned split correctly (`late` is no longer backgrounded, so
    // it's simply ignored rather than re-preserved).
    let snap4 = snapshot(
        Some(heavy_cpu(4)),
        attrib_with_late(&root, &existing, &late),
        vec![
            process_with_cpu(root, 1, 4 * 500_000_000),
            process(existing, root.pid),
            process(late, root.pid),
        ],
    );
    controller
        .tick(
            now + Duration::from_secs(4),
            &snap4,
            &mut platform,
            &mut log,
        )
        .unwrap();
    assert_eq!(
        controller.view.workloads.len(),
        1,
        "no cooldown: the very next tick may re-throttle"
    );
    assert!(platform.is_backgrounded(root));
    assert!(!platform.is_backgrounded(late));
    assert!(platform.is_backgrounded(existing));

    // Cold-start recovery of the resulting journal must also leave `existing` untouched.
    let mut fresh = FakePlatform::new("boot-1")
        .rescan_with(vec![process(root, 1), process(existing, root.pid)])
        .backgrounded_from(root, true)
        .backgrounded_from(existing, true);
    let resumed = recover(&home.0, &mut fresh, &mut log).expect("recovery must succeed");
    assert_eq!(resumed, 1);
    assert!(!fresh.is_backgrounded(root));
    assert!(
        fresh.is_backgrounded(existing),
        "recovery must never clear a preserved nested AgentRoot's pre-existing BG"
    );
}
fn attrib_with_late(
    root: &ProcessIdentity,
    existing: &ProcessIdentity,
    late: &ProcessIdentity,
) -> AttributionSnapshot {
    attribution(
        vec![workload("w1", *root, WorkloadClass::Batch)],
        vec![
            workload_attribution(*root, "w1"),
            agent_root_attribution(*existing),
            agent_root_attribution(*late),
        ],
    )
}

// The real native/core end-to-end scenario (NativePlatform set/clear round trip, stale-identity
// rejection, and a full Guardian.tick throttle-then-release cycle on a real owned process) and
// the real `ballast` binary's offline `resume --all` recovery now both live in
// `tests/guardian_recovery.rs` (`native_platform_and_guardian_own_process_identity_and_throttle_round_trip`
// and `cli_offline_resume_recovers_a_throttled_process_...`): they share the same
// "spawn a real owned process and drive it through Guardian::tick" fixture, and `CARGO_BIN_EXE_*`
// is only set for integration tests, not this crate's `--lib` unit tests, so neither can live here.
