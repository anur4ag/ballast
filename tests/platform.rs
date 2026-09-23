//! Integration tests for the platform layer (ticket 02). One binary so the
//! `child_helper` fixture and its support code are each compiled once.
//!
//! Every fixture child is this same test binary, re-executed with
//! `--ignored --exact child_helper` and a `BALLAST_TEST_CHILD_MODE` env var
//! picking its behavior (see `child_helper` below). Only `std::process::Command`
//! (safe fork+exec) is used to create processes; the "double fork" fixture
//! chains two such spawns instead of calling `libc::fork()` directly, so no
//! raw fork ever runs inside this multithreaded test process.

use ballast::platform::{NativePlatform, Platform, Process, ProcessIdentity, Signal};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};

const MODE_ENV: &str = "BALLAST_TEST_CHILD_MODE";
const MARKER_ENV: &str = "BALLAST_TEST_MARKER";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap so a fixture outlives its test only briefly if cleanup ever fails.
const CHILD_LIFETIME_CAP: Duration = Duration::from_secs(120);

fn find(processes: &[Process], pid: i32) -> Option<&Process> {
    processes.iter().find(|p| p.identity.pid == pid)
}

fn spawn_child(mode: &str, marker: &str) -> Child {
    let exe = std::env::current_exe().expect("current_exe for fixture re-exec");
    Command::new(exe)
        .args(["child_helper", "--exact", "--ignored", "--nocapture"])
        .env(MODE_ENV, mode)
        .env(MARKER_ENV, marker)
        .process_group(0) // pgid == pid, so pgid assertions are deterministic
        .stdin(Stdio::piped()) // some modes block on a barrier byte; unread otherwise
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn test fixture child")
}

/// Sends the one barrier byte a fixture mode is blocked reading from stdin,
/// so its next step (rename, exec, ...) cannot race the caller's snapshot.
fn release_barrier(child: &mut Child) {
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"go\n")
        .expect("write barrier byte");
}

/// Reads lines on a background thread (owns `stdout`, so no borrow outlives
/// this call) until one starts with `READY` or `BINDERR`, skipping libtest's
/// own preamble ("running 1 test", ...) printed ahead of it under `--nocapture`.
fn read_status_line(stdout: ChildStdout) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    let trimmed = line.trim_end().to_string();
                    if trimmed.starts_with("READY") || trimmed.starts_with("BINDERR") {
                        let _ = tx.send(trimmed);
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    rx.recv_timeout(READY_TIMEOUT)
        .unwrap_or_else(|e| panic!("fixture did not report status in time: {e}"))
}

fn ready_payload(line: &str) -> &str {
    line.strip_prefix("READY")
        .unwrap_or_else(|| panic!("fixture did not send READY, got: {line:?}"))
        .trim_start_matches(':')
}

/// Bounded poll instead of spinning unboundedly; fails with `what` past the deadline.
fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        if f() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for: {what}");
        }
        sleep(POLL_INTERVAL);
    }
}

/// Kills and reaps on drop, including when the test panics mid-assert.
/// Always SIGKILL: SIGTERM is ignorable/blockable by a stopped process.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Kills a process we only know the pid of (e.g. a reparented grandchild) on drop.
struct PidGuard(i32);
impl Drop for PidGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

// ---------------------------------------------------------------------
// Process listing: ppid/pgid/start_time, direct and double-forked env.
// ---------------------------------------------------------------------

#[test]
fn lists_child_with_correct_ppid_pgid_and_stable_start_time() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let own_pid = std::process::id() as i32;
    let exe_path = std::env::current_exe().expect("current_exe");

    let mut child = spawn_child("idle", "ppid_pgid_start_time");
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    let mut first_start_time = None;
    wait_until(
        "spawned child to appear in list_processes",
        || match platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).cloned())
        {
            Some(p) => {
                assert_eq!(p.ppid, own_pid, "child's ppid should be this test process");
                assert_eq!(p.pgid, child_pid, "process_group(0) makes pgid == pid");
                assert_eq!(
                    p.exe.as_deref(),
                    exe_path.to_str(),
                    "exe should be this re-exec'd test binary"
                );
                assert!(
                    p.argv
                        .as_ref()
                        .is_some_and(|argv| argv.iter().any(|a| a == "child_helper")),
                    "argv should include the child_helper filter arg, got {:?}",
                    p.argv
                );
                first_start_time = Some(p.identity.start_time);
                true
            }
            None => false,
        },
    );

    sleep(Duration::from_millis(50));
    let processes = platform.list_processes().expect("second list_processes");
    let second = find(&processes, child_pid).expect("child still listed");
    assert_eq!(
        Some(second.identity.start_time),
        first_start_time,
        "start_time must stay stable"
    );
}

#[test]
fn reads_environment_of_a_direct_child() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let marker = "direct_child_env";
    let mut child = spawn_child("idle", marker);
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    let mut identity = None;
    wait_until("spawned child to appear in list_processes", || {
        identity = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.identity));
        identity.is_some()
    });

    let env = platform
        .read_environment(identity.unwrap())
        .expect("environment should be readable");
    assert_eq!(env.get(MARKER_ENV).map(String::as_str), Some(marker));
}

#[test]
fn reads_environment_of_a_double_forked_detached_grandchild() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let marker = "doublefork_env";
    let mut relay_guard = ChildGuard(spawn_child("doublefork_relay", marker));
    let relay_stdout = relay_guard.0.stdout.take().expect("piped stdout");

    let payload = read_status_line(relay_stdout);
    let grandchild_pid: i32 = ready_payload(&payload)
        .parse()
        .unwrap_or_else(|_| panic!("expected grandchild pid in READY line, got {payload:?}"));

    // Prove detachment for real: the relay has exited before we look at the
    // grandchild, so any env read below can only be reaching an orphan that
    // was reparented to init/launchd, not something still alive via the relay.
    // Guarded (not a bare wait()) so a panic before this point still cleans up.
    let status = relay_guard.0.wait().expect("wait on relay");
    assert!(
        status.success(),
        "relay should exit(0) after handing off the grandchild"
    );

    let _grandchild_guard = PidGuard(grandchild_pid);
    let mut identity = None;
    wait_until(
        "double-forked grandchild to appear in list_processes",
        || {
            identity = platform
                .list_processes()
                .ok()
                .and_then(|ps| find(&ps, grandchild_pid).map(|p| p.identity));
            identity.is_some()
        },
    );

    let env = platform
        .read_environment(identity.unwrap())
        .expect("environment should still be readable after reparenting");
    assert_eq!(env.get(MARKER_ENV).map(String::as_str), Some(marker));
}

#[test]
fn exe_and_argv_cache_refreshes_after_exec_replaces_the_process_image() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    // The fixture blocks on a stdin barrier before exec'ing into `sleep`
    // (same pid, in place): releasing it only after `before` is captured
    // removes the race entirely, instead of hoping a fixed sleep is long
    // enough for the test runner to get scheduled in time.
    let mut child = spawn_child("exec_barrier", "exec_barrier");
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    let mut before = None;
    wait_until("child to appear in list_processes before exec", || {
        before = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).cloned());
        before.is_some()
    });
    let before = before.unwrap();
    assert!(
        before
            .argv
            .as_ref()
            .is_some_and(|argv| argv.iter().any(|a| a == "child_helper")),
        "before exec, argv should still be this test binary's, got {:?}",
        before.argv
    );

    release_barrier(&mut guard.0);

    let mut after = None;
    wait_until("argv to reflect the exec'd program", || {
        after = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).cloned());
        after.as_ref().is_some_and(|p| {
            p.argv
                .as_ref()
                .is_some_and(|argv| argv.iter().any(|a| a == "3600"))
        })
    });
    let after = after.unwrap();

    assert_ne!(before.exe, after.exe, "exe must change after exec");
    assert_ne!(
        before.argv, after.argv,
        "argv must be refreshed alongside exe"
    );
}

// ---------------------------------------------------------------------
// Resource metrics, listening ports, pressure.
// ---------------------------------------------------------------------

#[test]
fn memory_and_cpu_metrics_are_nonzero_for_a_running_child() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let mut child = spawn_child("busy", "memory_and_cpu_nonzero");
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    sleep(Duration::from_millis(150)); // let it actually burn some CPU

    let mut identity = None;
    wait_until("busy child to appear in list_processes", || {
        identity = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.identity));
        identity.is_some()
    });

    let metrics = platform
        .process_metrics(identity.unwrap())
        .expect("metrics should be readable");
    assert!(
        metrics.memory_bytes > 0,
        "a running process must have nonzero RSS"
    );
    assert!(
        metrics.cpu_time_ns > 0,
        "a busy-spinning process must have accrued CPU time"
    );
}

#[test]
fn reports_a_listening_ipv4_port() {
    check_listening_port("listen4", "listening_ipv4");
}

#[test]
fn reports_a_listening_ipv6_port() {
    check_listening_port("listen6", "listening_ipv6");
}

fn check_listening_port(mode: &str, marker: &str) {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let mut child = spawn_child(mode, marker);
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = ChildGuard(child);

    let status = read_status_line(stdout);
    if let Some(rest) = status.strip_prefix("BINDERR:") {
        let (kind, msg) = rest.split_once(':').unwrap_or((rest, ""));
        // Assumption: the sandbox may lack an IPv6 loopback (or otherwise
        // refuse the bind for environment reasons); anything else is a
        // real platform-layer defect and must fail the test.
        assert!(
            matches!(
                kind,
                "AddrNotAvailable" | "Unsupported" | "PermissionDenied"
            ),
            "unexpected bind failure for {mode}: {kind}: {msg}"
        );
        eprintln!("skipping {mode}: could not bind loopback socket ({kind}): {msg}");
        return;
    }
    let port: u16 = ready_payload(&status)
        .parse()
        .unwrap_or_else(|_| panic!("expected port in READY line, got {status:?}"));

    let mut identity = None;
    wait_until("listening child to appear in list_processes", || {
        identity = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.identity));
        identity.is_some()
    });

    let ports = platform
        .listening_ports(identity.unwrap())
        .expect("listening_ports should be readable");
    assert!(ports.contains(&port), "expected port {port} in {ports:?}");
}

#[test]
fn pressure_inputs_are_readable_without_root() {
    let platform = NativePlatform::new().expect("NativePlatform::new");
    let capabilities = platform.capabilities();
    let pressure = platform
        .pressure()
        .expect("pressure() must not require root");

    assert!(pressure.page_size > 0, "page_size must be positive");
    assert!(
        pressure.total_memory_bytes.is_some_and(|b| b > 0),
        "total_memory_bytes must be a positive Some"
    );
    assert!(pressure.swapins.is_some(), "swapins must be readable");
    assert!(pressure.swapouts.is_some(), "swapouts must be readable");
    if capabilities.memory_psi {
        assert!(
            pressure.psi_some_avg10.is_some() && pressure.psi_full_avg10.is_some(),
            "memory_psi capability implies psi_some_avg10/psi_full_avg10"
        );
    }
    if capabilities.kernel_pressure {
        assert!(
            pressure.kernel_pressure_level.is_some(),
            "kernel_pressure capability implies kernel_pressure_level"
        );
    }
}

#[test]
fn boot_id_is_nonempty_and_stable_across_platform_instances() {
    let a = NativePlatform::new()
        .expect("NativePlatform::new")
        .boot_id()
        .expect("boot_id");
    let b = NativePlatform::new()
        .expect("NativePlatform::new")
        .boot_id()
        .expect("boot_id");
    assert!(!a.is_empty(), "boot_id must be nonempty");
    assert_eq!(a, b, "boot_id must be stable across platform instances");
}

// ---------------------------------------------------------------------
// Environment boundaries: SIP-hidden env, auxiliary strings, non-UTF-8 names.
// ---------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn sip_enabled() -> bool {
    Command::new("csrutil")
        .arg("status")
        .output()
        .ok()
        .is_some_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains("enabled")
        })
}

#[cfg(target_os = "macos")]
#[test]
fn macos_restricted_binary_reports_environment_as_unknown() {
    if !sip_enabled() {
        // Explicit, not silent: without SIP, /bin/sleep isn't actually
        // restricted, so the premise of this test doesn't hold here.
        eprintln!(
            "skipping macos_restricted_binary_reports_environment_as_unknown: \
             csrutil reports SIP is not enabled on this host"
        );
        return;
    }

    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let child = Command::new("/bin/sleep")
        .arg("3600")
        .env("BALLAST_REVIEW_MARKER", "known")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sleep");
    let child_pid = child.id() as i32;
    let _guard = ChildGuard(child);

    let mut identity = None;
    let mut argv = None;
    wait_until(
        "restricted child to appear in list_processes",
        || match platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).cloned())
        {
            Some(p) => {
                identity = Some(p.identity);
                argv = p.argv.clone();
                true
            }
            None => false,
        },
    );
    assert!(
        argv.as_ref()
            .is_some_and(|argv| argv.iter().any(|a| a == "3600")),
        "argv must still be readable for a SIP-restricted binary, got {argv:?}"
    );

    let env = platform.read_environment(identity.unwrap());
    assert!(
        env.is_none(),
        "a SIP-restricted binary's environment must read as unknown (None), got {env:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_unrestricted_child_with_cleared_env_reports_known_empty() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args(["env_clear_child", "--exact", "--ignored", "--nocapture"])
        .env_clear()
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn env_clear_child");
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    let mut identity = None;
    wait_until("env_clear child to appear in list_processes", || {
        identity = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.identity));
        identity.is_some()
    });

    let env = platform.read_environment(identity.unwrap());
    assert_eq!(
        env.as_ref().map(|e| e.len()),
        Some(0),
        "an unrestricted process with a genuinely empty environment must read as \
         known-empty, not padded with macOS auxiliary strings; got {env:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_child_renamed_to_invalid_utf8_stays_listed_and_signalable() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let mut child = spawn_child("linux_rename", "linux_rename");
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    let mut identity = None;
    wait_until(
        "child to appear in list_processes under its normal name",
        || {
            identity = platform
                .list_processes()
                .ok()
                .and_then(|ps| find(&ps, child_pid).map(|p| p.identity));
            identity.is_some()
        },
    );
    let identity = identity.unwrap();

    release_barrier(&mut guard.0);

    // Confirm the rename actually landed on the process's own comm (not
    // some worker thread's) via the raw file, independent of our own API,
    // before asserting anything about how the platform layer sees it.
    wait_until("/proc/<pid>/comm to show the invalid-UTF-8 name", || {
        std::fs::read(format!("/proc/{child_pid}/comm"))
            .is_ok_and(|bytes| bytes.starts_with(&[0xFF, 0xFE, b'x']))
    });

    // The rename itself isn't observable through the public API (comm/name
    // isn't part of Process); what must hold is that the same identity
    // stays listed after the rename instead of vanishing because
    // /proc/<pid>/stat became invalid UTF-8.
    wait_until(
        "renamed child to still be listed under the same identity",
        || {
            platform
                .list_processes()
                .ok()
                .is_some_and(|ps| find(&ps, child_pid).is_some_and(|p| p.identity == identity))
        },
    );

    platform
        .send_signal(identity, Signal::Stop)
        .expect("SIGSTOP should still work after an invalid-UTF-8 rename");
    wait_until("renamed child to report stopped", || {
        platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.stopped))
            == Some(true)
    });

    platform
        .send_signal(identity, Signal::Continue)
        .expect("SIGCONT should still work after an invalid-UTF-8 rename");
    wait_until("renamed child to report running again", || {
        platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.stopped))
            == Some(false)
    });
}

// ---------------------------------------------------------------------
// Signals: SIGSTOP/SIGCONT round trip, stale-identity rejection.
// ---------------------------------------------------------------------

#[test]
fn sigstop_sigcont_round_trip() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let mut child = spawn_child("idle", "sigstop_sigcont");
    let child_pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped stdout");
    let _guard = ChildGuard(child);
    ready_payload(&read_status_line(stdout));

    let mut identity = None;
    wait_until("child to appear in list_processes", || {
        match platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).cloned())
        {
            Some(p) => {
                assert!(!p.stopped, "freshly spawned child should not start stopped");
                identity = Some(p.identity);
                true
            }
            None => false,
        }
    });
    let identity = identity.unwrap();

    platform
        .send_signal(identity, Signal::Stop)
        .expect("SIGSTOP should succeed on a live child");
    wait_until("child to report stopped after SIGSTOP", || {
        platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.stopped))
            == Some(true)
    });

    platform
        .send_signal(identity, Signal::Continue)
        .expect("SIGCONT should succeed on a stopped child");
    wait_until("child to report running again after SIGCONT", || {
        platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.stopped))
            == Some(false)
    });

    assert!(
        platform.process_metrics(identity).is_some(),
        "child must still be alive after the round trip"
    );
}

#[test]
fn stale_identity_cannot_signal_or_read() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");

    // Invalid pids must never resolve, independent of any real process.
    for pid in [0, -1] {
        let bogus = ProcessIdentity { pid, start_time: 0 };
        assert!(
            platform.read_environment(bogus).is_none(),
            "pid {pid} must not read env"
        );
        assert!(
            platform.process_metrics(bogus).is_none(),
            "pid {pid} must not read metrics"
        );
        assert!(
            platform.listening_ports(bogus).is_none(),
            "pid {pid} must not read ports"
        );
        assert!(
            platform.send_signal(bogus, Signal::Continue).is_err(),
            "pid {pid} must not be signalable"
        );
    }

    let mut guard = ChildGuard(spawn_child("idle", "stale_identity"));
    let child_pid = guard.0.id() as i32;
    let stdout = guard.0.stdout.take().expect("piped stdout");
    ready_payload(&read_status_line(stdout));

    let mut identity = None;
    wait_until("child to appear in list_processes", || {
        identity = platform
            .list_processes()
            .ok()
            .and_then(|ps| find(&ps, child_pid).map(|p| p.identity));
        identity.is_some()
    });
    let identity = identity.unwrap();

    // A real, still-running pid with the wrong start_time must also be
    // rejected: this is what actually stops PID reuse from redirecting an
    // action, not just "the process happens to be gone".
    let wrong_start_time = ProcessIdentity {
        pid: identity.pid,
        start_time: identity.start_time.wrapping_add(1),
    };
    assert!(
        platform.read_environment(wrong_start_time).is_none(),
        "mismatched start_time must not read env"
    );
    assert!(
        platform.process_metrics(wrong_start_time).is_none(),
        "mismatched start_time must not read metrics"
    );
    assert!(
        platform.listening_ports(wrong_start_time).is_none(),
        "mismatched start_time must not read ports"
    );
    assert!(
        platform
            .send_signal(wrong_start_time, Signal::Continue)
            .is_err(),
        "mismatched start_time must not be signalable"
    );

    // Killing and reaping through the guard is what makes `identity` itself
    // stale, checked below; kept in the guard so a panic above still cleans up.
    guard.0.kill().expect("kill");
    guard.0.wait().expect("wait (reap) the killed child");

    wait_until("platform to observe the identity is gone", || {
        platform.read_environment(identity).is_none()
    });

    assert!(
        platform.read_environment(identity).is_none(),
        "stale identity must not read env"
    );
    assert!(
        platform.process_metrics(identity).is_none(),
        "stale identity must not read metrics"
    );
    assert!(
        platform.listening_ports(identity).is_none(),
        "stale identity must not read ports"
    );
    assert!(
        platform.send_signal(identity, Signal::Continue).is_err(),
        "stale identity must not be signalable"
    );
}

// ---------------------------------------------------------------------
// Ignored scan benchmark: full list_processes() + pressure() at ~1000 processes.
// ---------------------------------------------------------------------

#[test]
#[ignore]
fn scan_benchmark_at_about_1000_processes() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let target_total = 1000usize;

    let baseline = platform
        .list_processes()
        .expect("baseline list_processes")
        .len();
    println!("baseline process count before spawning fillers: {baseline}");

    // Real `sleep` children, not fixture re-execs: 1000x re-executing this
    // (much heavier) test binary would itself be a resource-limit risk we
    // don't need, and a scan should see ordinary processes anyway.
    let mut fillers = Vec::new();
    let want = target_total.saturating_sub(baseline);
    for _ in 0..want {
        match Command::new("sleep")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => fillers.push(ChildGuard(child)),
            Err(e) => {
                // Assumption: on macOS a per-user process/thread limit can
                // make reaching exactly ~1000 impossible; report and proceed
                // with whatever we achieved rather than treat it as a failure.
                println!(
                    "stopped spawning fillers after {} (spawn failed: {e})",
                    fillers.len()
                );
                break;
            }
        }
    }
    // Let spawned children finish landing before the timed scan.
    sleep(Duration::from_millis(200));

    let actual_total = platform
        .list_processes()
        .expect("post-spawn list_processes")
        .len();
    println!(
        "spawned {} filler processes; actual total observed: {actual_total} (target {target_total})",
        fillers.len()
    );
    if actual_total < target_total {
        println!(
            "note: fell short of {target_total}; likely the host's per-user process limit, not a scan defect"
        );
    }

    // Warm up (page-in code paths, caches) before timing.
    for _ in 0..3 {
        let _ = platform.list_processes();
        let _ = platform.pressure();
    }

    let iterations = 20u32;
    let mut usage_before = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let mut usage_after = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc_before = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage_before.as_mut_ptr()) };
    assert_eq!(
        rc_before,
        0,
        "getrusage(before) failed: {}",
        std::io::Error::last_os_error()
    );
    let wall_start = Instant::now();
    for _ in 0..iterations {
        let _ = platform.list_processes().expect("timed list_processes");
        let _ = platform.pressure().expect("timed pressure");
    }
    let wall_elapsed = wall_start.elapsed();
    let rc_after = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage_after.as_mut_ptr()) };
    assert_eq!(
        rc_after,
        0,
        "getrusage(after) failed: {}",
        std::io::Error::last_os_error()
    );

    let cpu_ns = |u: &libc::rusage| -> u64 {
        let to_ns =
            |tv: libc::timeval| tv.tv_sec as u64 * 1_000_000_000 + tv.tv_usec as u64 * 1_000;
        to_ns(u.ru_utime) + to_ns(u.ru_stime)
    };
    let before = unsafe { usage_before.assume_init() };
    let after = unsafe { usage_after.assume_init() };
    let cpu_ns_total = cpu_ns(&after).saturating_sub(cpu_ns(&before));

    println!(
        "scan+pressure x{iterations} at {actual_total} processes: {:.2}ms wall total, {}ns/scan CPU, {:.2}ms/scan wall",
        wall_elapsed.as_secs_f64() * 1000.0,
        cpu_ns_total / iterations as u64,
        wall_elapsed.as_secs_f64() * 1000.0 / iterations as f64,
    );
}

// ---------------------------------------------------------------------
// Fixture process. Only reached via `spawn_child`'s re-exec.
// ---------------------------------------------------------------------

#[test]
#[ignore]
fn child_helper() {
    let mode = std::env::var(MODE_ENV).unwrap_or_default();
    let mut stdout = std::io::stdout();
    match mode.as_str() {
        "idle" => {
            writeln!(stdout, "READY").unwrap();
            stdout.flush().unwrap();
            sleep(CHILD_LIFETIME_CAP);
        }
        "busy" => {
            writeln!(stdout, "READY").unwrap();
            stdout.flush().unwrap();
            spin_until(Instant::now() + Duration::from_secs(5));
            sleep(CHILD_LIFETIME_CAP);
        }
        "listen4" => run_listener("127.0.0.1:0"),
        "listen6" => run_listener("[::1]:0"),
        "exec_barrier" => {
            writeln!(stdout, "READY").unwrap();
            stdout.flush().unwrap();
            read_barrier();
            // Replaces this process image in place (same pid): exec only
            // returns on failure.
            let err = Command::new("sleep").arg("3600").exec();
            panic!("exec sleep failed: {err}");
        }
        #[cfg(target_os = "linux")]
        "linux_rename" => {
            writeln!(stdout, "READY").unwrap();
            stdout.flush().unwrap();
            read_barrier();
            // libtest runs this on a worker thread, not the process's main
            // thread. prctl(PR_SET_NAME) always renames the calling
            // (current) thread, so it would rename the worker and leave
            // /proc/<pid>/stat's comm (the thread-group leader's) untouched.
            // Writing /proc/self/comm instead renames whichever task the
            // resolved path names, i.e. the leader, regardless of which
            // thread performs the write.
            std::fs::write("/proc/self/comm", [0xFFu8, 0xFE, b'x']).expect("write /proc/self/comm");
            sleep(CHILD_LIFETIME_CAP);
        }
        "doublefork_relay" => {
            let exe = std::env::current_exe().expect("current_exe");
            let grandchild = Command::new(exe)
                .args(["child_helper", "--exact", "--ignored", "--nocapture"])
                .env(MODE_ENV, "idle")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn double-fork grandchild");
            writeln!(stdout, "READY:{}", grandchild.id()).unwrap();
            stdout.flush().unwrap();
            // Deliberately orphan it: no wait(), just exit, so it is
            // reparented to init/launchd immediately (the "double fork" effect).
            std::mem::forget(grandchild);
            std::process::exit(0);
        }
        other => panic!("unknown fixture mode: {other}"),
    }
}

/// Blocks the fixture until the test writes and drops its barrier byte.
fn read_barrier() {
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read_exact(&mut buf);
}

/// A dispatch-free fixture used only with `.env_clear()`, so its real
/// environment is genuinely empty rather than carrying `MODE_ENV`.
#[test]
#[ignore]
fn env_clear_child() {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").unwrap();
    stdout.flush().unwrap();
    sleep(CHILD_LIFETIME_CAP);
}

fn spin_until(deadline: Instant) {
    let mut x: u64 = 0;
    while Instant::now() < deadline {
        for _ in 0..10_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
    }
    std::hint::black_box(x);
}

fn run_listener(addr: &str) {
    let mut stdout = std::io::stdout();
    match TcpListener::bind(addr) {
        Ok(listener) => {
            let port = listener.local_addr().unwrap().port();
            writeln!(stdout, "READY:{port}").unwrap();
            stdout.flush().unwrap();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let _: std::io::Result<TcpStream> = stream;
                }
            });
            sleep(CHILD_LIFETIME_CAP);
        }
        Err(e) => {
            writeln!(stdout, "BINDERR:{:?}:{e}", e.kind()).unwrap();
            stdout.flush().unwrap();
            std::process::exit(1);
        }
    }
}
