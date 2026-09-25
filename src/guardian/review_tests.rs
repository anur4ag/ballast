//! Regression tests for the ticket 05 review fixes: non-fatal decision-log/notify errors queued
//! through `Guardian::take_errors()`, a freeze-pass failure rolling back only the in-progress
//! victim, FIFO resume-on-unreadable-pressure after a 30s outage, and the identity-revalidation
//! helper (`platform::validate_identity`) distinguishing "unreadable" from "confirmed gone/other".
//!
//! Loaded as a submodule of `tests` (`super::*` pulls in every fixture and helper already defined
//! there -- `FakePlatform`, `two_batches`-style builders, `warm_to_critical`, `TestHome`, etc.).

use super::*;

fn failing_log(home: &TestHome) -> RotatingLog {
    let config = Config {
        log_max_bytes: 1,
        ..Config::default()
    };
    RotatingLog::open(home.0.base.join("log/decisions.jsonl"), &config).unwrap()
}

fn two_batches() -> (
    AttributionSnapshot,
    Vec<Process>,
    ProcessIdentity,
    ProcessIdentity,
) {
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
    (attribution, processes, first, second)
}

// ---------------------------------------------------------------------
// (1) Decision-log failures are non-fatal; a freeze-pass failure rolls back only its own victim.
// ---------------------------------------------------------------------

struct FailListProcesses(FakePlatform, std::cell::Cell<bool>);
impl Platform for FailListProcesses {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn boot_id(&self) -> io::Result<String> {
        self.0.boot_id()
    }
    fn list_processes(
        &mut self,
        watched: &HashSet<ProcessIdentity>,
        metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        if self.1.get() {
            return Err(io::Error::other("list_processes unavailable"));
        }
        self.0.list_processes(watched, metrics)
    }
    fn read_environment(&self, p: ProcessIdentity) -> Option<Environment> {
        self.0.read_environment(p)
    }
    fn process_metrics(&self, p: ProcessIdentity) -> Option<ProcessMetrics> {
        self.0.process_metrics(p)
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        self.0.pressure()
    }
    fn listening_ports(&self, p: ProcessIdentity) -> Option<Vec<u16>> {
        self.0.listening_ports(p)
    }
    fn send_signal(&self, p: ProcessIdentity, s: Signal) -> io::Result<()> {
        self.0.send_signal(p, s)
    }
    fn notify(&self, t: &str, b: &str) -> io::Result<bool> {
        self.0.notify(t, b)
    }
}

fn warm_to_critical_generic(
    guardian: &mut Guardian,
    platform: &mut impl Platform,
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
    assert_eq!(guardian.level, Level::Critical);
    (start + Duration::from_secs(2), 2)
}

/// Two failure modes around a freeze, both of which used to be conflated into "undo everything":
///
/// - A decision-log write failure (disk full, read-only home, rotation error) must never abort
///   the tick or undo a freeze -- it is queued as a warning through `take_errors()` while both an
///   already-established freeze and the new one stay in place.
/// - A freeze *pass* failure (here, the rescan `list_processes` call mid-freeze) must roll back
///   only the workload it was in the middle of freezing. An already-established freeze from an
///   earlier, successful tick is left completely untouched, and the rolled-back workload is not
///   marked ineligible -- it is still a normal freeze candidate on the very next tick.
#[test]
fn freeze_failures_are_isolated_to_the_workload_that_actually_failed() {
    // Part A: a healthy first freeze, then a decision-log failure during the second freeze.
    let home = TestHome::new("review-log-failure");
    let mut guardian = new_guardian(
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
    let (attribution, processes, _first, _second) = two_batches();

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("first freeze with a healthy log");
    assert_eq!(guardian.frozen.len(), 1);
    assert!(
        guardian.take_errors().is_empty(),
        "a healthy log must not queue any warnings"
    );

    // The decision log now fails on every write. Journalling the freeze goes through
    // `recovery::write`, not this log, so the freeze itself must still complete.
    let mut failing = failing_log(&home);
    now += Duration::from_secs(5);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut failing)
        .expect("a failing decision log must not fail the tick");
    assert_eq!(
        guardian.frozen.len(),
        2,
        "both the established and the new freeze must survive a log failure"
    );
    assert!(
        platform.continued().is_empty(),
        "a decision-log failure must never resume anything"
    );
    let errors = guardian.take_errors();
    assert!(
        !errors.is_empty(),
        "the log failure must be surfaced through take_errors"
    );
    assert!(
        errors.iter().any(|e| e.contains("decision log")),
        "errors: {errors:?}"
    );

    // Part B: a fresh guardian/platform, this time failing `list_processes` mid-freeze -- proving
    // that failure rolls back only the workload it was in the middle of freezing.
    let home = TestHome::new("review-freeze-pass-failure");
    let mut guardian = new_guardian(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (attribution, processes, first, second) = two_batches();

    let mut platform = FailListProcesses(FakePlatform::new("boot-1"), std::cell::Cell::new(false));
    let (mut now, mut step) =
        warm_to_critical_generic(&mut guardian, &mut platform, &mut attributor, &mut log);

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("first freeze with a healthy platform");
    assert_eq!(guardian.frozen.len(), 1);
    let established = guardian.frozen[0].workload_id.clone();
    let established_root = guardian.frozen[0].root;

    platform.1.set(true);
    now += Duration::from_secs(5);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect_err("a failed freeze pass must surface as a tick error");

    assert_eq!(
        guardian.frozen.len(),
        1,
        "only the already-established freeze must remain"
    );
    assert_eq!(
        guardian.frozen[0].workload_id, established,
        "the established victim must be untouched"
    );
    let new_victim_root = if established_root == first {
        second
    } else {
        first
    };
    assert_eq!(
        platform.0.continued(),
        vec![new_victim_root],
        "the in-progress victim must be resumed, and only it"
    );
    assert!(
        guardian.ineligible.is_empty(),
        "a rolled-back freeze must never be marked ineligible"
    );
}

// ---------------------------------------------------------------------
// (2) 30s of unreadable pressure admits Normal and resumes FIFO, 5s apart, no ineligibility.
// ---------------------------------------------------------------------

/// After the pressure signal itself becomes unreadable (not merely "quiet") for 30 continuous
/// seconds, the guardian must treat that as an admission signal rather than staying stuck at a
/// stale Critical: `level` becomes `Normal` and any frozen workloads resume FIFO, one every five
/// seconds, tagged `pressure_unknown`. Unlike a forced max-freeze resume, none of this marks a
/// workload ineligible for refreezing. A single valid sample in the middle of the outage must
/// reset the 30s clock rather than letting it carry across the blip.
#[test]
fn thirty_seconds_of_unreadable_pressure_admits_normal_and_resumes_fifo() {
    let home = TestHome::new("review-pressure-unknown-fifo");
    let mut guardian = new_guardian(
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
    let (attribution, processes, _first, _second) = two_batches();

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("first freeze");
    now += Duration::from_secs(5);
    step += 1;
    let snap = snapshot(
        Some(heavy_swap(step)),
        attribution.clone(),
        processes.clone(),
    );
    guardian
        .tick(now, &snap, &mut platform, &mut attributor, &mut log)
        .expect("second freeze");
    assert_eq!(
        guardian.frozen.len(),
        2,
        "both workloads must be frozen before the outage starts"
    );
    let fifo_order: Vec<_> = guardian.frozen.iter().map(|w| w.root).collect();

    let invalid = snapshot(None, AttributionSnapshot::default(), Vec::new());

    let t1 = now + Duration::from_secs(1);
    guardian
        .tick(t1, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("first unreadable sample");
    assert_eq!(guardian.frozen.len(), 2);
    assert!(guardian.ineligible.is_empty());

    let t2 = t1 + Duration::from_secs(20);
    guardian
        .tick(t2, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("20s into the outage");
    assert_eq!(
        guardian.frozen.len(),
        2,
        "must not resume before the 30s boundary"
    );

    // A valid sample in the middle of the outage must reset the 30s clock entirely.
    let t3 = t2 + Duration::from_secs(1);
    guardian
        .tick(
            t3,
            &quiet_normal_snapshot(),
            &mut platform,
            &mut attributor,
            &mut log,
        )
        .expect("a valid sample resets the clock");
    assert_eq!(guardian.frozen.len(), 2);

    let t4 = t3 + Duration::from_secs(1);
    guardian
        .tick(t4, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("first unreadable sample since the reset");
    assert_eq!(guardian.frozen.len(), 2);

    let t5 = t4 + Duration::from_secs(29);
    guardian
        .tick(t5, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("29s since the reset");
    assert_eq!(
        guardian.frozen.len(),
        2,
        "the clock must have restarted at the valid sample, not accumulated across it"
    );

    let t6 = t4 + Duration::from_secs(30);
    guardian
        .tick(t6, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("30s since the reset");
    assert_eq!(
        guardian.level,
        Level::Normal,
        "30s of unreadable pressure must admit Normal"
    );
    assert_eq!(
        guardian.frozen.len(),
        1,
        "the oldest frozen workload resumes at the 30s boundary"
    );
    assert!(
        guardian.ineligible.is_empty(),
        "an unknown-pressure resume must never mark ineligibility"
    );

    let too_soon = t6 + Duration::from_secs(4);
    guardian
        .tick(too_soon, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("fifo spacing");
    assert_eq!(
        guardian.frozen.len(),
        1,
        "fifo resumes must stay five seconds apart"
    );

    let t7 = t6 + Duration::from_secs(5);
    guardian
        .tick(t7, &invalid, &mut platform, &mut attributor, &mut log)
        .expect("second fifo resume");
    assert!(
        guardian.frozen.is_empty(),
        "both workloads must have resumed fifo"
    );
    assert!(guardian.ineligible.is_empty());
    assert_eq!(
        platform.continued(),
        fifo_order,
        "fifo order must match freeze order"
    );

    let log_contents = std::fs::read_to_string(home.0.base.join("log/decisions.jsonl")).unwrap();
    assert!(
        log_contents.contains("pressure_unknown"),
        "the resume reason must be recorded as pressure_unknown"
    );
}

// ---------------------------------------------------------------------
// (3) validate_identity: unreadable is preserved for retry, a confirmed mismatch is dropped.
// ---------------------------------------------------------------------

enum ContinueFailure {
    Unreadable,
    Mismatch,
}

struct FakeIdentityCheck(FakePlatform, ProcessIdentity, ContinueFailure);
impl Platform for FakeIdentityCheck {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn boot_id(&self) -> io::Result<String> {
        self.0.boot_id()
    }
    fn list_processes(
        &mut self,
        w: &HashSet<ProcessIdentity>,
        m: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        self.0.list_processes(w, m)
    }
    fn read_environment(&self, p: ProcessIdentity) -> Option<Environment> {
        self.0.read_environment(p)
    }
    fn process_metrics(&self, p: ProcessIdentity) -> Option<ProcessMetrics> {
        self.0.process_metrics(p)
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        self.0.pressure()
    }
    fn listening_ports(&self, p: ProcessIdentity) -> Option<Vec<u16>> {
        self.0.listening_ports(p)
    }
    fn send_signal(&self, p: ProcessIdentity, s: Signal) -> io::Result<()> {
        if p == self.1 && matches!(s, Signal::Continue) {
            return match self.2 {
                // Models the real "kill(pid,0)-only" unreadable path: the identity re-read
                // failed, but the target's PID (here, this very test process) is confirmed
                // alive, so it must never be treated as "gone".
                ContinueFailure::Unreadable => crate::platform::validate_identity(
                    ProcessIdentity {
                        pid: std::process::id() as i32,
                        start_time: 1,
                    },
                    None,
                ),
                // A confirmed, differing identity: the PID was reused by an unrelated process.
                ContinueFailure::Mismatch => crate::platform::validate_identity(
                    p,
                    Some(ProcessIdentity {
                        pid: p.pid,
                        start_time: p.start_time + 1,
                    }),
                ),
            };
        }
        self.0.send_signal(p, s)
    }
    fn notify(&self, t: &str, b: &str) -> io::Result<bool> {
        self.0.notify(t, b)
    }
}

/// `platform::validate_identity` distinguishes a target whose PID re-read genuinely failed
/// (still alive per `kill(pid, 0)`, but its identity could not be confirmed) from one that is
/// provably gone or has been replaced by a different process. `resume` -- and, at cold start,
/// `recovery::recover` -- must preserve the saved entry on the former (it might still be the
/// right process; a later attempt can retry) and safely drop it on the latter (a confirmed exit
/// or a reused PID is never something to keep retrying).
#[test]
fn resume_preserves_an_unreadable_identity_and_drops_a_confirmed_mismatch() {
    let home = TestHome::new("review-identity-revalidation");
    let mut guardian = new_guardian(
        home.0.clone(),
        "boot-1".into(),
        Mode::Enforce,
        Thresholds::default(),
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = home.log();
    let (attribution, processes, first, _second) = two_batches();
    let victim = first;
    let mut inner = FakePlatform::new("boot-1");
    let (mut now, mut step) =
        warm_to_critical(&mut guardian, &mut inner, &mut attributor, &mut log);

    let mut one = attribution.clone();
    one.workloads.retain(|w| w.root == victim);
    one.processes.retain(|p| p.identity == victim);
    let one_processes: Vec<_> = processes
        .iter()
        .filter(|p| p.identity == victim)
        .cloned()
        .collect();

    now += Duration::from_secs(1);
    step += 1;
    let snap = snapshot(Some(heavy_swap(step)), one, one_processes);
    guardian
        .tick(now, &snap, &mut inner, &mut attributor, &mut log)
        .expect("freeze");
    assert_eq!(guardian.frozen.len(), 1);
    let frozen_id = guardian.frozen[0].workload_id.clone();

    // Part A: unreadable -- the entry must survive, both in memory and on disk, for a later retry.
    let platform = FakeIdentityCheck(inner, victim, ContinueFailure::Unreadable);
    let resumed_at = now + Duration::from_secs(1);
    let result = guardian.resume(Some(&frozen_id), resumed_at, &platform, &mut log);
    assert!(
        result.is_err(),
        "an unreadable identity on resume must surface as a real failure, not silently succeed"
    );
    assert_eq!(
        guardian.frozen.len(),
        1,
        "the frozen entry must stay in memory when its identity is unreadable"
    );
    let journal = recovery::read(&home.0).expect("journal must still be present");
    assert_eq!(
        journal.workloads.len(),
        1,
        "the journal must still hold the entry so a later resume can retry"
    );
    assert!(
        platform.0.continued().is_empty(),
        "must not record a resume that never actually happened"
    );

    // Part B: a confirmed, differing identity is a real exit -- resume must succeed and drop it
    // from both the in-memory list and the journal.
    let mismatch = FakeIdentityCheck(platform.0, victim, ContinueFailure::Mismatch);
    let result = guardian.resume(
        Some(&frozen_id),
        resumed_at + Duration::from_secs(1),
        &mismatch,
        &mut log,
    );
    assert!(
        result.is_ok(),
        "a confirmed identity mismatch must be treated as a safe drop, not an error: {result:?}"
    );
    assert!(guardian.frozen.is_empty());
    let journal = recovery::read(&home.0).expect("journal read");
    assert!(
        journal.workloads.is_empty(),
        "a confirmed mismatch must drop the entry from the journal too"
    );

    // Part C: the same distinction must hold for a cold-start recovery pass, independent of any
    // live Guardian -- an unreadable identity must not be treated as evidence the process exited.
    let recovery_home = TestHome::new("review-startup-recovery-unreadable");
    recovery::write(
        &recovery_home.0,
        "boot-1",
        &[FrozenWorkload::recovery(victim)],
    )
    .expect("seed the journal");
    let mut recovery_platform = FakeIdentityCheck(
        FakePlatform::new("boot-1"),
        victim,
        ContinueFailure::Unreadable,
    );
    let mut recovery_log = recovery_home.log();
    let result = recovery::recover(
        &recovery_home.0,
        &mut recovery_platform,
        &[],
        None,
        &mut recovery_log,
    );
    assert!(
        result.is_err(),
        "an unreadable identity during startup recovery must surface as a real failure"
    );
    let journal = recovery::read(&recovery_home.0)
        .expect("journal must still be present after a failed recovery");
    assert_eq!(
        journal.workloads.len(),
        1,
        "startup recovery must not drop an entry it could not confirm as gone"
    );
}
