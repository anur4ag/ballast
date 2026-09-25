//! Real-daemon recovery checks using test-owned children, seeded journals and temporary homes.
//! Daemons run in observe mode; marked fixtures re-exec this test binary so macOS can read their environment.

#[cfg(target_os = "macos")]
use ballast::attribution::{
    AttributionSnapshot, Attributor, MemorySummary, ProcessAttribution, ProcessRole, Workload,
    WorkloadClass,
};
#[cfg(target_os = "macos")]
use ballast::daemon::files::{Config, Mode, Paths, RotatingLog};
#[cfg(target_os = "macos")]
use ballast::daemon::{ProcessChanges, Snapshot, Status};
#[cfg(target_os = "macos")]
use ballast::guardian::throttle::Inputs as ThrottleInputs;
#[cfg(target_os = "macos")]
use ballast::guardian::{Guardian, Thresholds as GuardianThresholds};
#[cfg(target_os = "macos")]
use ballast::platform::Process;
#[cfg(target_os = "macos")]
use ballast::platform::{Capabilities, PressureInputs, ProcessMetrics};
use ballast::platform::{NativePlatform, Platform, ProcessIdentity};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Generous: covers a real `bind()` plus a full startup recovery pass (a `list_processes` scan
/// of every process on the machine plus one environment read per stopped candidate) on this
/// suite's possibly busy shared machine, not just an idle-machine round trip.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A `BALLAST_HOME` under `/tmp`, pre-seeded with an `observe`-mode `config.toml` so the real
/// daemon spawned against it can never freeze anything for real (see the module doc comment).
struct TempHome {
    path: PathBuf,
    marker: String,
}
impl TempHome {
    fn with_config(tag: &str, config: impl FnOnce(&str) -> String) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "{:x}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = PathBuf::from(format!("/tmp/blt-grec-{tag}-{unique}"));
        assert!(
            path.as_os_str().len() < 60,
            "test home path must stay short for the unix socket path limit: {path:?}"
        );
        fs::create_dir_all(&path).expect("create temp BALLAST_HOME");
        let marker = format!("BALLAST_RECOVERY_{}", unique.replace('-', "_"));
        fs::write(path.join("config.toml"), config(&marker)).expect("write config.toml");
        TempHome { path, marker }
    }
    fn new(tag: &str) -> Self {
        Self::with_config(tag, |marker| {
            format!(
                "mode = \"observe\"\nnotifications = false\nrecovery_sweep_markers = [{marker:?}]\n\
                 [[markers]]\nkey = {marker:?}\nlevel = \"agent\"\nkind = \"test\"\n"
            )
        })
    }
    /// No markers, no sweep at all: isolates a throttle-only recovery scenario from the
    /// independent frozen-workload marker sweep this file's other fixtures exercise.
    #[cfg(target_os = "macos")]
    fn new_throttle_only(tag: &str) -> Self {
        Self::with_config(tag, |_marker| {
            "notifications = false\nrecovery_sweep_markers = []\n".to_string()
        })
    }
    fn frozen_json_path(&self) -> PathBuf {
        self.path.join("state").join("frozen.json")
    }
    fn write_frozen_json(&self, boot_id: &str, workloads: &[serde_json::Value]) {
        fs::create_dir_all(self.path.join("state")).expect("create state dir");
        fs::write(
            self.frozen_json_path(),
            serde_json::json!({"boot_id": boot_id, "workloads": workloads}).to_string(),
        )
        .expect("seed frozen.json");
    }
    fn read_frozen_json(&self) -> serde_json::Value {
        let bytes = fs::read(self.frozen_json_path()).expect("read frozen.json");
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("frozen.json must be valid JSON after recovery: {e}"))
    }
    #[cfg(target_os = "macos")]
    fn throttled_json_path(&self) -> PathBuf {
        self.path.join("state").join("throttled.json")
    }
    #[cfg(target_os = "macos")]
    fn read_throttled_json(&self) -> serde_json::Value {
        let bytes = fs::read(self.throttled_json_path()).expect("read throttled.json");
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("throttled.json must be valid JSON after recovery: {e}"))
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("retained recovery diagnostics: {}", self.path.display());
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn current_boot_id() -> String {
    NativePlatform::new()
        .expect("NativePlatform::new")
        .boot_id()
        .expect("boot_id")
}
/// A fresh scan's own `(pid, start_time)` for `pid`, matching what a just-started real daemon
/// would independently compute on this same machine and boot.
fn identity_of(pid: i32) -> ProcessIdentity {
    NativePlatform::new()
        .expect("NativePlatform::new")
        .list_processes(&Default::default(), &Default::default())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity.pid == pid)
        .unwrap_or_else(|| panic!("pid {pid} not found in a fresh scan"))
        .identity
}
fn frozen_workload_entry(label: &str, root: ProcessIdentity) -> serde_json::Value {
    serde_json::json!({
        "workload_id": format!("recovery-test:{label}"),
        "root": root,
        "processes": [root],
        "frozen_at_ms": 0,
    })
}

// Throttle (background-priority) fixtures: shared by the native Guardian round trip and the CLI
// offline-recovery test below, so the "drive Guardian::tick against one real owned process until
// it throttles" scenario is written once, not duplicated between an in-process assertion and a
// pre-crash setup step.
#[cfg(target_os = "macos")]
const THROTTLE_WORKLOAD_ID: &str = "throttle-fixture";

/// CPU-only pressure whose busy/total ratio settles at 0.95 (over the 0.9 default threshold)
/// with `load_per_core` at 2.0 (over the 1.0 default): two consecutive samples promote the host
/// to Elevated.
#[cfg(target_os = "macos")]
fn heavy_cpu(step: u64) -> PressureInputs {
    PressureInputs {
        throttle: Some(ThrottleInputs {
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
#[cfg(target_os = "macos")]
fn quiet_cpu() -> PressureInputs {
    PressureInputs {
        throttle: Some(ThrottleInputs {
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
/// A synthetic `Snapshot` naming `identity` as the sole member of a Batch workload with
/// `cpu_time_ns` set to `ns` (the fault-gating agent-CPU-share signal), or an empty attribution
/// snapshot when `identity` is `None` (used to drive the quiet release tick).
#[cfg(target_os = "macos")]
fn throttle_snapshot(
    capabilities: Capabilities,
    pressure: PressureInputs,
    identity: Option<ProcessIdentity>,
    ns: u64,
) -> Snapshot {
    let processes: Vec<Process> = identity
        .into_iter()
        .map(|id| Process {
            identity: id,
            ppid: 1,
            pgid: id.pid,
            uid: unsafe { libc::geteuid() },
            stopped: false,
            name: None,
            exe: None,
            argv: None,
            metrics: Some(ProcessMetrics {
                memory_bytes: 0,
                cpu_time_ns: ns,
            }),
        })
        .collect();
    let workloads = identity
        .into_iter()
        .map(|id| Workload {
            id: THROTTLE_WORKLOAD_ID.into(),
            agent_id: "agent".into(),
            root: id,
            label: THROTTLE_WORKLOAD_ID.into(),
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
    let attribution_processes = identity
        .into_iter()
        .map(|id| ProcessAttribution {
            identity: id,
            owner_id: None,
            agent_id: Some("agent".into()),
            workload_id: Some(THROTTLE_WORKLOAD_ID.into()),
            role: ProcessRole::Workload,
            environment_known: true,
            listening_ports: None,
            ports_sampled_at_ms: None,
        })
        .collect();
    Snapshot {
        today: Default::default(),
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
            pressure_level: Default::default(),
            batch_running: false,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: "test".into(),
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
/// Drives the real `Guardian::tick` against `identity` with synthetic heavy-CPU pressure until
/// it throttles for real: writes the real `state/throttled.json` journal (Enforce mode) and sets
/// the real `EXT_DARWINBG` policy via `platform.set_backgrounded`, exactly as the live daemon
/// would. Returns the live `Guardian`/`Attributor`/`RotatingLog` so a caller that wants to keep
/// driving ticks (e.g. to release again) can; a caller that only wants the on-disk journal and
/// real OS state -- simulating a crash right after throttling -- can just drop the tuple.
#[cfg(target_os = "macos")]
fn throttle_owned_via_guardian(
    home: &Path,
    identity: ProcessIdentity,
    platform: &mut NativePlatform,
) -> (Guardian, Attributor, RotatingLog) {
    let paths = Paths {
        base: home.to_path_buf(),
    };
    paths.prepare().expect("prepare paths");
    let mut guardian = Guardian::new(
        paths,
        platform.boot_id().expect("boot_id"),
        Mode::Enforce,
        GuardianThresholds::default(),
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut log = RotatingLog::open(home.join("log/decisions.jsonl"), &Config::default())
        .expect("open decisions log");
    let now = Instant::now();
    for n in 0..=2u64 {
        let snapshot = throttle_snapshot(
            platform.capabilities(),
            heavy_cpu(n),
            Some(identity),
            n * 500_000_000,
        );
        guardian
            .tick(
                now + Duration::from_secs(n),
                &snapshot,
                platform,
                &mut attributor,
                &mut log,
            )
            .expect("guardian tick");
    }
    assert_eq!(
        guardian.throttle.view.workloads.len(),
        1,
        "the owned process must have been throttled via the real core path"
    );
    (guardian, attributor, log)
}

fn process_state(pid: i32) -> Option<char> {
    let output = Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .chars()
        .next()
}
fn is_stopped(pid: i32) -> bool {
    process_state(pid) == Some('T')
}
/// `DaemonGuard::start` already waits for a real reply, which only happens after recovery has
/// run (see its comment), so this bound only needs to absorb ordinary `ps`/filesystem
/// observation lag on a possibly busy shared machine, not the recovery race itself.
const PROCESS_STATE_TIMEOUT: Duration = Duration::from_secs(15);

fn wait_until(pid: i32, description: &str, want: impl Fn(Option<char>) -> bool) {
    let deadline = Instant::now() + PROCESS_STATE_TIMEOUT;
    loop {
        let state = process_state(pid);
        if want(state) {
            return;
        }
        if Instant::now() >= deadline {
            let mut platform = NativePlatform::new().unwrap();
            if let Some(process) = platform
                .list_processes(&Default::default(), &Default::default())
                .unwrap()
                .into_iter()
                .find(|p| p.identity.pid == pid)
            {
                eprintln!(
                    "failed owned process: {process:?}; environment: {:?}",
                    platform.read_environment(process.identity)
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} did not reach {description} in time (last ps state: {state:?})"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}
fn wait_stopped(pid: i32) {
    wait_until(pid, "stopped (ps state T)", |s| s == Some('T'));
}
fn wait_resumed(pid: i32) {
    wait_until(pid, "resumed (ps state other than T)", |s| {
        s.is_some_and(|c| c != 'T')
    });
}

/// A re-exec of this test binary into `idle_fixture`, with a cleared environment plus
/// a unique marker key, so the daemon's independent stopped-process marker sweep can find it -- never
/// a real user's process. Deliberately not `Command::new("sleep")`: a hardened-runtime system
/// binary like `/bin/sleep` rejects `read_environment` for a caller that did not spawn it as a
/// direct exec target of *its own* privileged path, which the sweep depends on; a locally built
/// binary re-executing itself (the same pattern `tests/attribution_fleet.rs` and
/// `tests/platform.rs` already use for their own marked/env-readable fixtures) does not have that
/// restriction.
struct OwnedSleep(Child);
impl OwnedSleep {
    fn spawn(marker: &str) -> Self {
        let child = Command::new(std::env::current_exe().expect("current_exe"))
            .args(["idle_fixture", "--exact", "--ignored", "--nocapture"])
            .env_clear()
            .env(marker, "guardian-recovery-test")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn owned sleep");
        OwnedSleep(child)
    }
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
    fn stop(&self) {
        assert_eq!(
            unsafe { libc::kill(self.pid(), libc::SIGSTOP) },
            0,
            "SIGSTOP owned sleep"
        );
        wait_stopped(self.pid());
    }
}
impl Drop for OwnedSleep {
    fn drop(&mut self) {
        // SIGKILL terminates even a still-stopped process; wait() reaps it either way.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `sleep 3600` with no Ballast marker at all: used only to prove a stale/mismatched journal
/// entry is never blindly signalled, regardless of whether the pid is still reachable.
struct BareSleep(Child);
impl BareSleep {
    fn spawn() -> Self {
        let child = Command::new("/bin/sleep")
            .arg("3600")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bare sleep");
        BareSleep(child)
    }
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
    fn stop(&self) {
        assert_eq!(
            unsafe { libc::kill(self.pid(), libc::SIGSTOP) },
            0,
            "SIGSTOP bare sleep"
        );
        wait_stopped(self.pid());
    }
}
impl Drop for BareSleep {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A real `ballast daemon` child bound to a given `BALLAST_HOME`. Kills and reaps on drop
/// (including on panic); `kill()` does the same explicitly, mid-test, before restarting.
struct DaemonGuard {
    child: Child,
    socket: PathBuf,
}
impl DaemonGuard {
    fn start(home: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_ballast"))
            .arg("daemon")
            .env("BALLAST_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(home.join("daemon-stderr.log"))
                    .expect("open daemon stderr log"),
            )
            .spawn()
            .expect("spawn ballast daemon");
        let guard = DaemonGuard {
            child,
            socket: home.join("run").join("ballastd.sock"),
        };
        // A bound listening socket only proves `bind()` ran, which happens *before* startup
        // recovery; only a real request/response round trip proves the daemon has served its
        // first tick, which only ever starts after `recovery::recover()` has already returned
        // (see `run_with_targets`: recovery, then the tick loop, then the accept thread spawns
        // on the first tick). Waiting on a raw connect alone is exactly the race that made this
        // suite's own timing flaky: it can succeed the instant the listener is bound, long
        // before recovery has actually resumed anything.
        wait_for_first_reply(&guard.socket);
        guard
    }
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
/// Retries a full status request/response round trip, not just a raw connect (see the comment
/// on `DaemonGuard::start` for why that distinction matters here).
fn wait_for_first_reply(socket: &Path) {
    use std::io::{BufRead, BufReader, Write};
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let attempt = (|| -> std::io::Result<()> {
            let mut stream = UnixStream::connect(socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            stream.write_all(br#"{"version": 1, "method": "status"}"#)?;
            stream.write_all(b"\n")?;
            stream.flush()?;
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line)?;
            if line.trim().is_empty() {
                return Err(std::io::Error::other("empty reply"));
            }
            Ok(())
        })();
        match attempt {
            Ok(()) => return,
            Err(_) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Err(e) => panic!("no status reply from {socket:?} within {CONNECT_TIMEOUT:?}: {e}"),
        }
    }
}
/// Only reached via `OwnedSleep::spawn`'s re-exec: idles far longer than any test needs and
/// never exits on its own, so every real caller kills and reaps it through `Drop`.
#[test]
#[ignore]
fn idle_fixture() {
    std::thread::sleep(Duration::from_secs(3600));
}

fn run_cli(home: &Path, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .args(args)
        .env("BALLAST_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn ballast {args:?}: {e}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return child.wait_with_output().expect("collect CLI output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ballast {args:?} did not exit within 10s");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[test]
fn daemon_restart_recovers_a_valid_then_corrupt_then_missing_journal_in_sequence() {
    let home = TempHome::new("sequential");
    let boot = current_boot_id();
    let daemon = DaemonGuard::start(&home.path);

    // 1. A valid journal naming a real stopped process under the current boot: recovered via
    // the journal itself.
    let valid = OwnedSleep::spawn(&home.marker);
    valid.stop();
    home.write_frozen_json(
        &boot,
        &[frozen_workload_entry("valid", identity_of(valid.pid()))],
    );
    daemon.kill();
    let daemon = DaemonGuard::start(&home.path);
    eprintln!("recovery phase: valid; owned pid {}", valid.pid());
    wait_resumed(valid.pid());
    assert!(
        home.read_frozen_json()["workloads"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "a fully recovered journal must be rewritten empty"
    );

    // 2. A corrupt journal: unreadable, but the independent stopped-marker sweep must still
    // run and find this owned, stopped, marked process regardless.
    let corrupt = OwnedSleep::spawn(&home.marker);
    corrupt.stop();
    fs::write(home.frozen_json_path(), b"{ this is not valid json").expect("corrupt frozen.json");
    daemon.kill();
    let daemon = DaemonGuard::start(&home.path);
    eprintln!("recovery phase: corrupt; owned pid {}", corrupt.pid());
    wait_resumed(corrupt.pid());
    assert!(
        home.read_frozen_json()["workloads"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "recovery must leave a valid, empty journal behind even after starting from a corrupt one"
    );

    // 3. A missing journal entirely: same guarantee, via the same independent sweep.
    let missing = OwnedSleep::spawn(&home.marker);
    missing.stop();
    match fs::remove_file(home.frozen_json_path()) {
        Ok(()) | Err(_) => {}
    }
    daemon.kill();
    let daemon = DaemonGuard::start(&home.path);
    eprintln!("recovery phase: missing; owned pid {}", missing.pid());
    wait_resumed(missing.pid());
    assert!(
        home.read_frozen_json()["workloads"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "recovery must leave a valid, empty journal behind even after starting with none at all"
    );
    daemon.kill();
}

#[test]
fn a_journal_entry_from_a_different_boot_is_discarded_but_the_marker_sweep_still_runs() {
    let home = TempHome::new("boot-mismatch");
    let stray = OwnedSleep::spawn(&home.marker);
    stray.stop();
    let unmarked = BareSleep::spawn();
    unmarked.stop();
    home.write_frozen_json(
        "not-the-real-boot-id",
        &[
            frozen_workload_entry("wrong-boot", identity_of(stray.pid())),
            frozen_workload_entry("wrong-boot-unmarked", identity_of(unmarked.pid())),
        ],
    );

    let daemon = DaemonGuard::start(&home.path);
    // The journal entry itself must be discarded (a different boot), but the owned, stopped,
    // marked process must still come back via the independent sweep.
    wait_resumed(stray.pid());
    assert!(
        is_stopped(unmarked.pid()),
        "wrong-boot entries must not be signalled"
    );
    assert!(
        home.read_frozen_json()["workloads"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    daemon.kill();
}

#[test]
fn a_stale_journal_entry_whose_identity_no_longer_matches_is_never_signalled() {
    // `NativePlatform::send_signal` rejects an identity mismatch with a `NotFound` error (see
    // `platform::gone`), and `recovery::signal` treats `NotFound` the same as "already exited":
    // tolerable, not a failure, so the stale entry is dropped from the journal exactly like a
    // genuinely gone process would be. That journal bookkeeping is expected, not a bug -- the
    // actual safety property this test exists to prove is that the *real* process at that pid,
    // whose identity does not match, is never sent a live signal at all.
    let home = TempHome::new("reused-identity");
    let boot = current_boot_id();

    // No `BALLAST_OWNER` marker on purpose: this isolates the journal's own identity check from
    // the independent marker sweep, which would otherwise resume this process on its own merits.
    let stale = BareSleep::spawn();
    stale.stop();
    let real_identity = identity_of(stale.pid());
    // Unambiguously different, not a small nudge: the platform's own start_time granularity is
    // an internal detail this test should not have to know precisely.
    let mismatched_identity = ProcessIdentity {
        pid: real_identity.pid,
        start_time: 1,
    };
    home.write_frozen_json(
        &boot,
        &[frozen_workload_entry("reused", mismatched_identity)],
    );

    let daemon = DaemonGuard::start(&home.path);
    // `DaemonGuard::start` only returns once recovery has already run to completion, so this
    // observes its outcome directly rather than racing it.
    assert!(
        is_stopped(stale.pid()),
        "a journal entry whose start_time no longer matches the live pid must never be signalled"
    );
    daemon.kill();
}

#[test]
fn cli_offline_resume_recovers_directly_when_no_daemon_is_reachable() {
    let home = TempHome::new("cli-offline");
    let boot = current_boot_id();
    let owned = OwnedSleep::spawn(&home.marker);
    owned.stop();
    home.write_frozen_json(
        &boot,
        &[frozen_workload_entry(
            "cli-offline",
            identity_of(owned.pid()),
        )],
    );

    for args in [
        &["--help"][..],
        &["--version"],
        &["status", "--json"],
        &["ps", "--json"],
        &["top"],
    ] {
        let output = run_cli(&home.path, args);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ballast resume --all"),
            "{args:?}"
        );
        assert!(
            is_stopped(owned.pid()),
            "read-only commands must not signal the frozen child"
        );
    }

    let output = run_cli(&home.path, &["resume", "--all"]);
    assert!(
        output.status.success(),
        "offline `ballast resume --all` must succeed with no daemon running: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Resumed"),
        "stdout should report how many entries were resumed: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("WARNING:"));
    wait_resumed(owned.pid());
    assert!(
        home.read_frozen_json()["workloads"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    let daemon = DaemonGuard::start(&home.path);
    let bare = BareSleep::spawn();
    bare.stop();
    home.write_frozen_json(
        &boot,
        &[frozen_workload_entry(
            "not-in-live-state",
            identity_of(bare.pid()),
        )],
    );
    let online = run_cli(&home.path, &["resume", "--all"]);
    assert!(online.status.success());
    assert!(String::from_utf8_lossy(&online.stdout).contains("Resumed 0"));
    assert!(
        is_stopped(bare.pid()),
        "reachable daemon must handle resume, never direct recovery"
    );
    daemon.kill();
    assert!(run_cli(&home.path, &["resume", "--all"]).status.success());
    wait_resumed(bare.pid());
}

/// The real platform layer (`NativePlatform::set_backgrounded`/`backgrounded`) plus the real
/// core `Guardian` policy, both driven against one real owned process: a set/clear round trip, a
/// stale-identity rejection, then a full throttle-via-pressure-then-release cycle through
/// `Guardian::tick` (not a bare `Controller`), verified against the real `EXT_DARWINBG` flag.
#[cfg(target_os = "macos")]
#[test]
fn native_platform_and_guardian_own_process_identity_and_throttle_round_trip() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let owned = BareSleep::spawn();
    let identity = identity_of(owned.pid());

    // Force a known baseline first rather than assuming a freshly spawned process starts
    // un-backgrounded: it could inherit the test harness's own policy.
    platform.set_backgrounded(identity, false).unwrap();
    assert!(!platform.backgrounded(identity).unwrap());
    platform.set_backgrounded(identity, true).unwrap();
    assert!(platform.backgrounded(identity).unwrap());
    platform.set_backgrounded(identity, false).unwrap();
    assert!(
        !platform.backgrounded(identity).unwrap(),
        "clearing must fully restore the un-backgrounded state"
    );

    // A stale identity (same pid, wrong start_time) must be rejected, never actioned.
    let stale = ProcessIdentity {
        pid: identity.pid,
        start_time: identity.start_time.wrapping_add(1),
    };
    let err = platform.set_backgrounded(stale, true).unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "a mismatched identity must be rejected"
    );
    assert!(
        !platform.backgrounded(identity).unwrap(),
        "a rejected mismatch must never leak through to the real process"
    );

    let home = TempHome::new_throttle_only("native-guardian");
    let (mut guardian, mut attributor, mut log) =
        throttle_owned_via_guardian(&home.path, identity, &mut platform);
    assert!(
        platform.backgrounded(identity).unwrap(),
        "Guardian must have set the real OS background policy"
    );

    let quiet = throttle_snapshot(platform.capabilities(), quiet_cpu(), None, 0);
    guardian
        .tick(
            Instant::now() + Duration::from_secs(10),
            &quiet,
            &mut platform,
            &mut attributor,
            &mut log,
        )
        .expect("guardian tick");
    assert!(guardian.throttle.view.workloads.is_empty());
    assert!(
        !platform.backgrounded(identity).unwrap(),
        "release must clear the real OS background policy"
    );
}

/// The throttle journal's own offline-recovery path (`throttle::recover`, called first inside
/// `guardian::recovery::recover`): a real, offline `ballast resume --all` must clear a real
/// process's `EXT_DARWINBG` background policy, exactly like `cli_offline_resume_recovers_...`
/// above proves for a frozen (SIGSTOP'd) workload -- the two journals are recovered by the same
/// CLI call but through independent code paths, so this needs its own real-binary proof.
#[cfg(target_os = "macos")]
#[test]
fn cli_offline_resume_recovers_a_throttled_process_without_a_reachable_daemon() {
    let home = TempHome::new_throttle_only("cli-throttle");
    let owned = BareSleep::spawn();
    let identity = identity_of(owned.pid());
    let mut platform = NativePlatform::new().expect("NativePlatform::new");

    // Throttle it for real via the core Guardian path (writes the real journal, sets the real
    // EXT_DARWINBG policy), then drop the in-process Guardian/log without releasing -- simulating
    // a crash right after throttling, not a hand-written journal.
    drop(throttle_owned_via_guardian(
        &home.path,
        identity,
        &mut platform,
    ));

    let output = run_cli(&home.path, &["resume", "--all"]);
    assert!(
        output.status.success(),
        "offline `ballast resume --all` must recover a throttled process with no daemon running: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !platform.backgrounded(identity).unwrap(),
        "the real background policy must be cleared by offline recovery"
    );
    assert!(
        home.read_throttled_json()["workloads"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "a fully recovered throttle journal must be rewritten empty, not left with stale entries"
    );
}

#[test]
fn cli_offline_resume_is_blocked_by_a_concurrent_lock_holder() {
    let home = TempHome::new("cli-lock");

    // Hold the same directory flock `ipc::lock` (and so `resume_command`'s offline fallback)
    // takes, simulating a concurrent daemon start or a second offline recovery in flight.
    let lock_dir = std::fs::File::open(&home.path).expect("open BALLAST_HOME for locking");
    assert_eq!(
        unsafe { libc::flock(lock_dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "test must be the first to lock a freshly created BALLAST_HOME"
    );

    let blocked = run_cli(&home.path, &["resume", "--all"]);
    assert!(
        !blocked.status.success(),
        "offline resume must fail while the directory lock is held elsewhere, not silently race it"
    );

    drop(lock_dir);
    let unblocked = run_cli(&home.path, &["resume", "--all"]);
    assert!(
        unblocked.status.success(),
        "once the lock is released, offline resume must succeed: {}",
        String::from_utf8_lossy(&unblocked.stderr)
    );
}

#[test]
fn scoped_sweep_leaves_other_agent_markers_stopped() {
    let home = TempHome::new("scope");
    let owned = OwnedSleep::spawn(&home.marker);
    let other = OwnedSleep::spawn("CODEX_THREAD_ID");
    owned.stop();
    other.stop();

    let daemon = DaemonGuard::start(&home.path);
    wait_resumed(owned.pid());
    assert!(is_stopped(other.pid()), "out-of-scope marker was resumed");
    daemon.kill();

    owned.stop();
    assert!(run_cli(&home.path, &["resume", "--all"]).status.success());
    wait_resumed(owned.pid());
    assert!(is_stopped(other.pid()), "offline sweep ignored its scope");
}

#[test]
fn uninstall_recovers_marked_work_without_service_or_journal() {
    let home = TempHome::new("uninstall");
    let owned = OwnedSleep::spawn(&home.marker);
    owned.stop();
    let run = |flag: &str| {
        Command::new(env!("CARGO_BIN_EXE_ballast"))
            .args(["uninstall", flag, "--json"])
            .env("HOME", &home.path)
            .env("BALLAST_HOME", &home.path)
            .env("CLAUDE_CONFIG_DIR", home.path.join("claude"))
            .env("CODEX_HOME", home.path.join("codex"))
            .env("BALLAST_SERVICE_DIR", home.path.join("services"))
            .env("BALLAST_SERVICE_LABEL", &home.marker)
            .env("PATH", "")
            .output()
            .unwrap()
    };
    assert!(run("--dry-run").status.success());
    assert!(is_stopped(owned.pid()), "preview resumed work");
    assert!(!home.frozen_json_path().exists());
    let applied = run("--yes");
    assert!(
        !is_stopped(owned.pid()),
        "uninstall left marked work frozen: {}",
        String::from_utf8_lossy(&applied.stdout)
    );
    assert!(applied.status.success());
    let result: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(result["items"][0]["status"], "applied");
    let repeated = run("--yes");
    assert_eq!(repeated.status.code(), Some(2));
}
