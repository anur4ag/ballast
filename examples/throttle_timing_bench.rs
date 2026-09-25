//! Pre/post-fix timing benchmark for ticket 17 throttle apply/release (no CPU/disk load
//! generated -- pressure inputs are fabricated). Not part of the product; a one-off measurement
//! tool. Spawns 10 owned idle sleepers x 10 workloads, drives the real Guardian/NativePlatform
//! path with fabricated Elevated pressure, and reports apply/release/steady-state timing plus a
//! breakdown of platform.list_processes/backgrounded/set_backgrounded elapsed time vs residual,
//! captured separately per phase.

use ballast::attribution::{
    AttributionSnapshot, Attributor, MemorySummary, ProcessAttribution, ProcessRole, Workload,
    WorkloadClass,
};
use ballast::daemon::files::{Config, Mode, Paths, RotatingLog};
use ballast::daemon::{ProcessChanges, Snapshot, Status};
use ballast::guardian::throttle::Inputs;
use ballast::guardian::{Guardian, Thresholds as GuardianThresholds};
use ballast::platform::{
    Capabilities, Environment, NativePlatform, Platform, PressureInputs, Process, ProcessIdentity,
    ProcessMetrics, Signal,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const WORKLOADS: usize = 10;
const MEMBERS_PER_WORKLOAD: usize = 10;

#[derive(Default, Clone, Copy)]
struct Counter {
    total: Duration,
    calls: u64,
}

struct TimingPlatform {
    inner: NativePlatform,
    list_processes: RefCell<Counter>,
    backgrounded: RefCell<Counter>,
    set_backgrounded: RefCell<Counter>,
}
impl TimingPlatform {
    fn new(mut inner: NativePlatform) -> Self {
        inner.notifications = false;
        Self {
            inner,
            list_processes: RefCell::default(),
            backgrounded: RefCell::default(),
            set_backgrounded: RefCell::default(),
        }
    }
    fn reset_counters(&self) {
        *self.list_processes.borrow_mut() = Counter::default();
        *self.backgrounded.borrow_mut() = Counter::default();
        *self.set_backgrounded.borrow_mut() = Counter::default();
    }
    fn totals(&self) -> (Counter, Counter, Counter) {
        (
            *self.list_processes.borrow(),
            *self.backgrounded.borrow(),
            *self.set_backgrounded.borrow(),
        )
    }
}
impl Platform for TimingPlatform {
    fn supports_throttle(&self) -> bool {
        self.inner.supports_throttle()
    }
    fn backgrounded(&self, id: ProcessIdentity) -> io::Result<bool> {
        let start = Instant::now();
        let result = self.inner.backgrounded(id);
        let mut c = self.backgrounded.borrow_mut();
        c.total += start.elapsed();
        c.calls += 1;
        result
    }
    fn set_backgrounded(&self, id: ProcessIdentity, enabled: bool) -> io::Result<()> {
        let start = Instant::now();
        let result = self.inner.set_backgrounded(id, enabled);
        let mut c = self.set_backgrounded.borrow_mut();
        c.total += start.elapsed();
        c.calls += 1;
        result
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn boot_id(&self) -> io::Result<String> {
        self.inner.boot_id()
    }
    fn list_processes(
        &mut self,
        watched: &HashSet<ProcessIdentity>,
        metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        let start = Instant::now();
        let result = self.inner.list_processes(watched, metrics);
        let mut c = self.list_processes.borrow_mut();
        c.total += start.elapsed();
        c.calls += 1;
        result
    }
    fn read_environment(&self, process: ProcessIdentity) -> Option<Environment> {
        self.inner.read_environment(process)
    }
    fn process_metrics(&self, process: ProcessIdentity) -> Option<ProcessMetrics> {
        self.inner.process_metrics(process)
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        self.inner.pressure()
    }
    fn listening_ports(&self, process: ProcessIdentity) -> Option<Vec<u16>> {
        self.inner.listening_ports(process)
    }
    fn send_signal(&self, process: ProcessIdentity, signal: Signal) -> io::Result<()> {
        self.inner.send_signal(process, signal)
    }
    fn notify(&self, title: &str, body: &str) -> io::Result<bool> {
        self.inner.notify(title, body)
    }
}

/// Killed and reaped on drop, including on panic.
struct OwnedSleeper(Child);
impl Drop for OwnedSleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Removes the temp BALLAST_HOME on drop, including on panic or an early `?` return.
struct TempHome(PathBuf);
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn heavy_cpu(step: u64) -> PressureInputs {
    PressureInputs {
        throttle: Some(Inputs {
            cpu_busy_ticks: step * 950,
            cpu_total_ticks: step * 1000,
            cpu_count: 1,
            load_per_core: 2.0,
        }),
        ..Default::default()
    }
}
fn quiet_cpu() -> PressureInputs {
    PressureInputs {
        throttle: Some(Inputs {
            cpu_busy_ticks: 0,
            cpu_total_ticks: 1000,
            cpu_count: 1,
            load_per_core: 0.1,
        }),
        ..Default::default()
    }
}

fn snapshot(
    pressure: PressureInputs,
    members: &[(ProcessIdentity, String, u64)],
    capabilities: Capabilities,
) -> Snapshot {
    let processes: Vec<Process> = members
        .iter()
        .map(|(id, _, ns)| Process {
            identity: *id,
            ppid: 1,
            pgid: id.pid,
            uid: unsafe { libc::geteuid() },
            stopped: false,
            name: None,
            exe: None,
            argv: None,
            metrics: Some(ProcessMetrics {
                memory_bytes: 0,
                cpu_time_ns: *ns,
            }),
        })
        .collect();
    let workloads: Vec<Workload> = members
        .iter()
        .map(|(_, wid, _)| wid.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|wid| Workload {
            id: wid.into(),
            agent_id: "agent".into(),
            root: members.iter().find(|(_, w, _)| w == wid).unwrap().0,
            label: wid.into(),
            class: WorkloadClass::Batch,
            first_seen_ms: 0,
            detached_pgid: None,
            memory: MemorySummary {
                bytes: 0,
                complete: true,
                growth_30s_bytes: None,
                growth_bytes_per_sec: None,
            },
        })
        .collect();
    let attribution_processes: Vec<ProcessAttribution> = members
        .iter()
        .map(|(id, wid, _)| ProcessAttribution {
            identity: *id,
            owner_id: None,
            agent_id: Some("agent".into()),
            workload_id: Some(wid.clone()),
            role: ProcessRole::Workload,
            environment_known: true,
            listening_ports: None,
            ports_sampled_at_ms: None,
        })
        .collect();
    Snapshot {
        today: Default::default(),
        status: Status {
            daemon_version: "bench".into(),
            pid: 0,
            mode: Mode::Enforce,
            tick: 0,
            sampled_at_ms: 0,
            tick_interval_ms: 1000,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded: false,
            process_count: processes.len(),
            pressure_level: Default::default(),
            batch_running: false,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: "bench".into(),
        capabilities,
        processes,
        changes: ProcessChanges::default(),
        pressure: Some(pressure),
        attribution: AttributionSnapshot {
            owners: Vec::new(),
            agents: Vec::new(),
            workloads,
            processes: attribution_processes,
        },
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
    }
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((values.len() - 1) as f64 * p).round() as usize;
    values[idx]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn native_json(lp: Counter, bg: Counter, sbg: Counter) -> serde_json::Value {
    serde_json::json!({
        "list_processes": {"total_ms": ms(lp.total), "calls": lp.calls},
        "backgrounded": {"total_ms": ms(bg.total), "calls": bg.calls},
        "set_backgrounded": {"total_ms": ms(sbg.total), "calls": sbg.calls},
    })
}

fn main() -> io::Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "macOS throttle benchmark",
        ));
    }
    let home = TempHome(
        std::env::temp_dir().join(format!("ballast-t17-timing-bench-{}", std::process::id())),
    );
    std::fs::create_dir_all(&home.0)?;
    let paths = Paths {
        base: home.0.clone(),
    };
    paths.prepare()?;
    let config = Config {
        notifications: false,
        recovery_sweep_markers: Some(Vec::new()),
        ..Config::default()
    };
    let mut log = RotatingLog::open(home.0.join("log/decisions.jsonl"), &config)?;

    // 10 workloads x 10 owned idle sleepers each -- no CPU/disk load, just real owned pids.
    let mut sleepers = Vec::new();
    let mut members: Vec<(ProcessIdentity, String, u64)> = Vec::new();
    let workload_ids: Vec<String> = (0..WORKLOADS).map(|i| format!("w{i}")).collect();
    let mut platform = TimingPlatform::new(NativePlatform::new()?);
    for wid in &workload_ids {
        for _ in 0..MEMBERS_PER_WORKLOAD {
            let child = Command::new("sleep").arg("120").spawn()?;
            let pid = child.id() as i32;
            sleepers.push(OwnedSleeper(child));
            let identity = platform
                .list_processes(&HashSet::new(), &HashSet::new())?
                .into_iter()
                .find(|p| p.identity.pid == pid)
                .expect("owned sleeper must be visible")
                .identity;
            members.push((identity, wid.clone(), 0));
        }
    }
    println!(
        "spawned {} owned idle sleepers across {WORKLOADS} workloads",
        sleepers.len()
    );

    let caps = platform.capabilities();
    let mut guardian = Guardian::new(
        paths.clone(),
        "bench-boot".into(),
        Mode::Enforce,
        GuardianThresholds::default(),
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let demand_at = |n: u64| -> Vec<(ProcessIdentity, String, u64)> {
        members
            .iter()
            .map(|(id, w, _)| (*id, w.clone(), n * 500_000_000))
            .collect()
    };

    // Warm-up: members must already be present and accumulating their own CPU time across both
    // warm-up ticks, or their own rate has no baseline yet on the first Elevated tick.
    platform.reset_counters();
    for n in 0..=1u64 {
        let snap = snapshot(heavy_cpu(n), &demand_at(n), caps);
        guardian.tick(
            now + Duration::from_secs(n),
            &snap,
            &mut platform,
            &mut attributor,
            &mut log,
        )?;
    }

    // Apply: the tick that adds all 100 members and applies 100 real set_backgrounded(true)s.
    platform.reset_counters();
    let apply_start = Instant::now();
    let snap = snapshot(heavy_cpu(2), &demand_at(2), caps);
    guardian.tick(
        now + Duration::from_secs(2),
        &snap,
        &mut platform,
        &mut attributor,
        &mut log,
    )?;
    let apply_elapsed = apply_start.elapsed();
    let (apply_lp, apply_bg, apply_sbg) = platform.totals();
    assert_eq!(
        guardian.throttle.view.workloads.len(),
        WORKLOADS,
        "all workloads must have thrown"
    );

    // Steady-state Enforce: sustained demand, same 100-member population as apply -- still
    // throttled, membership unchanged, measuring per-tick decision cost with no new native applies.
    const STEADY_TICKS: u64 = 20;
    const RELEASE_TICK: u64 = 3 + STEADY_TICKS;

    platform.reset_counters();
    let mut enforce_steady = Vec::new();
    for n in 3..RELEASE_TICK {
        let snap = snapshot(heavy_cpu(n), &demand_at(n), caps);
        let start = Instant::now();
        guardian.tick(
            now + Duration::from_secs(n),
            &snap,
            &mut platform,
            &mut attributor,
            &mut log,
        )?;
        enforce_steady.push(ms(start.elapsed()));
    }
    let (enforce_steady_lp, enforce_steady_bg, enforce_steady_sbg) = platform.totals();
    assert_eq!(
        enforce_steady_sbg.calls, 0,
        "an unchanged member must never be reapplied during steady state"
    );

    // Release: quiet pressure, no members left in attribution -> every workload releases.
    platform.reset_counters();
    let release_start = Instant::now();
    let quiet = snapshot(quiet_cpu(), &[], caps);
    guardian.tick(
        now + Duration::from_secs(RELEASE_TICK),
        &quiet,
        &mut platform,
        &mut attributor,
        &mut log,
    )?;
    let release_elapsed = release_start.elapsed();
    let (release_lp, release_bg, release_sbg) = platform.totals();
    assert!(
        guardian.throttle.view.workloads.is_empty(),
        "all workloads must have released"
    );

    // Observe: a separate Guardian, the same sustained-demand scenario, for a like-for-like
    // decision-only comparison against the Enforce steady-state above.
    let mut observe_guardian = Guardian::new(
        paths.clone(),
        "bench-boot-observe".into(),
        Mode::Observe,
        GuardianThresholds::default(),
    );
    platform.reset_counters();
    for n in 0..=1u64 {
        let snap = snapshot(heavy_cpu(n), &demand_at(n), caps);
        observe_guardian.tick(
            now + Duration::from_secs(n),
            &snap,
            &mut platform,
            &mut attributor,
            &mut log,
        )?;
    }
    let snap = snapshot(heavy_cpu(2), &demand_at(2), caps);
    observe_guardian.tick(
        now + Duration::from_secs(2),
        &snap,
        &mut platform,
        &mut attributor,
        &mut log,
    )?;
    assert_eq!(
        observe_guardian.throttle.view.workloads.len(),
        WORKLOADS,
        "observe must also propose all workloads"
    );

    platform.reset_counters();
    let mut observe_steady = Vec::new();
    for n in 3..RELEASE_TICK {
        let snap = snapshot(heavy_cpu(n), &demand_at(n), caps);
        let start = Instant::now();
        observe_guardian.tick(
            now + Duration::from_secs(n),
            &snap,
            &mut platform,
            &mut attributor,
            &mut log,
        )?;
        observe_steady.push(ms(start.elapsed()));
    }
    let (observe_steady_lp, observe_steady_bg, observe_steady_sbg) = platform.totals();

    println!(
        "{}",
        serde_json::json!({
            "workloads": WORKLOADS, "members_per_workload": MEMBERS_PER_WORKLOAD,
            "apply_ms": ms(apply_elapsed),
            "apply_native_residual_ms": ms(apply_elapsed.saturating_sub(apply_lp.total + apply_bg.total + apply_sbg.total)),
            "apply_native": native_json(apply_lp, apply_bg, apply_sbg),
            "release_ms": ms(release_elapsed),
            "release_native_residual_ms": ms(release_elapsed.saturating_sub(release_lp.total + release_bg.total + release_sbg.total)),
            "release_native": native_json(release_lp, release_bg, release_sbg),
            "enforce_steady_p50_ms": percentile(&mut enforce_steady.clone(), 0.5),
            "enforce_steady_p99_ms": percentile(&mut enforce_steady.clone(), 0.99),
            "enforce_steady_native": native_json(enforce_steady_lp, enforce_steady_bg, enforce_steady_sbg),
            "observe_steady_p50_ms": percentile(&mut observe_steady.clone(), 0.5),
            "observe_steady_p99_ms": percentile(&mut observe_steady.clone(), 0.99),
            "observe_steady_native": native_json(observe_steady_lp, observe_steady_bg, observe_steady_sbg),
        })
    );

    Ok(())
}
