//! Real-daemon recovery checks using test-owned children, seeded journals and temporary homes.
//! Daemons run in observe mode; marked fixtures re-exec this test binary so macOS can read their environment.

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
    fn new(tag: &str) -> Self {
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
        fs::write(
            path.join("config.toml"),
            format!(
                "mode = \"observe\"\nnotifications = false\nrecovery_sweep_markers = [{marker:?}]\n\
                 [[markers]]\nkey = {marker:?}\nlevel = \"agent\"\nkind = \"test\"\n"
            ),
        )
        .expect("write isolated observe-mode config.toml");
        TempHome { path, marker }
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
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
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
            .stderr(Stdio::null())
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
