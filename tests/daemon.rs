//! Black-box integration tests for the daemon and its IPC (ticket 03).
//!
//! Talks to `ballast daemon` / `ballast status` only through the process
//! boundary (spawn, unix socket, exit status), never an internal API, so
//! these stay valid regardless of how the daemon is modularized internally.
//!
//! Isolation: every test gets its own `BALLAST_HOME` under `/tmp` (not
//! `std::env::temp_dir()`, whose macOS `TMPDIR` can blow the 104-byte unix
//! socket path limit) and never touches the real `~/.ballast`. Spawned
//! daemons are RAII-killed and reaped on drop, including on panic.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A `BALLAST_HOME` under `/tmp`, short enough to leave room for
/// `/run/ballastd.sock` under macOS's 104-byte `sockaddr_un` limit.
struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "{:x}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = PathBuf::from(format!("/tmp/blt-{tag}-{unique}"));
        assert!(
            path.as_os_str().len() < 60,
            "test home path must stay short for the unix socket path limit: {path:?}"
        );
        std::fs::create_dir_all(&path).expect("create temp BALLAST_HOME");
        TempHome { path }
    }

    fn socket_path(&self) -> PathBuf {
        self.path.join("run").join("ballastd.sock")
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A running `ballast daemon` child bound to a [`TempHome`]. Kills and reaps
/// on drop (including on panic).
struct DaemonGuard {
    child: Child,
    home: TempHome,
}

impl DaemonGuard {
    fn spawn(tag: &str) -> Self {
        let home = TempHome::new(tag);
        std::fs::write(
            home.path.join("config.toml"),
            "mode = \"observe\"\nnotifications = false\nrecovery_sweep_markers = []\n",
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ballast"))
            .arg("daemon")
            .env("BALLAST_HOME", &home.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ballast daemon");
        DaemonGuard { child, home }
    }

    fn start(tag: &str) -> Self {
        let guard = Self::spawn(tag);
        guard.wait_ready();
        guard
    }

    fn wait_ready(&self) {
        let _ = connect_with_timeout(&self.home.socket_path(), CONNECT_TIMEOUT);
    }

    fn connect(&self) -> UnixStream {
        connect_with_timeout(&self.home.socket_path(), CONNECT_TIMEOUT)
    }

    fn status_request(&self) -> Value {
        let mut stream = self.connect();
        send_request(&mut stream, &json!({"version": 1, "method": "status"}))
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn connect_with_timeout(socket: &Path, timeout: Duration) -> UnixStream {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(REQUEST_TIMEOUT))
                    .expect("set_read_timeout");
                stream
                    .set_write_timeout(Some(REQUEST_TIMEOUT))
                    .expect("set_write_timeout");
                return stream;
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Err(e) => panic!("could not connect to {socket:?} within {timeout:?}: {e}"),
        }
    }
}

/// Writes one NDJSON request line and reads one NDJSON response line back.
fn send_request(stream: &mut UnixStream, request: &Value) -> Value {
    send_raw_line(stream, &request.to_string());
    let line = read_line(stream).unwrap_or_else(|| panic!("no response line for {request}"));
    serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("response was not valid JSON: {line:?}: {e}"))
}

fn send_raw_line(stream: &mut UnixStream, line: &str) {
    stream.write_all(line.as_bytes()).expect("write request");
    stream.write_all(b"\n").expect("write newline");
    stream.flush().expect("flush request");
}

/// `None` on timeout/EOF rather than panicking, so a closed-connection
/// response can be told apart from a malformed one.
fn read_line(stream: &UnixStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().expect("try_clone stream"));
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        Err(_) => None,
    }
}

fn mode_bits(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {path:?}: {e}"))
        .permissions()
        .mode()
        & 0o777
}

/// Runs a `ballast` CLI subcommand against `home`, bounded so a hang in the
/// binary fails the test instead of stalling the whole suite.
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

/// Bounded wait for a child's exit status; kills and panics past `timeout`.
fn wait_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process did not exit within {timeout:?}");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[test]
fn foreground_daemon_answers_a_status_request() {
    let daemon = DaemonGuard::start("status");
    let response = daemon.status_request();

    assert_eq!(response["version"], 1);
    assert_eq!(response["type"], "status");
    let status = &response["status"];
    assert!(
        status["tick"].as_u64().is_some(),
        "tick must be u64: {status}"
    );
    assert!(
        status["tick_interval_ms"].as_u64().is_some_and(|ms| ms > 0),
        "tick_interval_ms must be a positive u64: {status}"
    );
    assert!(
        status["tick_cpu_ns"].as_u64().is_some(),
        "tick_cpu_ns must be u64: {status}"
    );
    assert!(
        status["tick_wall_ns"].as_u64().is_some(),
        "tick_wall_ns must be u64: {status}"
    );
    assert!(
        status["sample_discarded"].as_bool().is_some(),
        "sample_discarded must be bool: {status}"
    );
    assert!(
        status["process_count"].as_u64().is_some(),
        "process_count must be usize-shaped: {status}"
    );
    assert!(
        status["daemon_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "daemon_version must be a nonempty string: {status}"
    );
}

#[test]
fn snapshot_request_returns_status_and_processes() {
    let daemon = DaemonGuard::start("snapshot");
    let mut stream = daemon.connect();
    let response = send_request(&mut stream, &json!({"version": 1, "method": "snapshot"}));

    assert_eq!(response["type"], "snapshot");
    let snapshot = &response["snapshot"];
    assert!(
        snapshot["status"]["tick"].as_u64().is_some(),
        "snapshot.status must look like a status payload: {snapshot}"
    );
    let processes = snapshot["processes"]
        .as_array()
        .unwrap_or_else(|| panic!("snapshot.processes must be an array: {snapshot}"));
    assert!(!processes.is_empty(), "process table should list something");
}

#[test]
fn multiple_requests_are_answered_on_one_connection() {
    let daemon = DaemonGuard::start("multireq");
    let mut stream = daemon.connect();

    let first = send_request(&mut stream, &json!({"version": 1, "method": "status"}));
    let second = send_request(&mut stream, &json!({"version": 1, "method": "status"}));
    assert_eq!(first["type"], "status");
    assert_eq!(second["type"], "status");
    assert!(
        second["status"]["tick"].as_u64() >= first["status"]["tick"].as_u64(),
        "tick must not go backwards across requests on the same connection"
    );
}

#[test]
fn unsupported_protocol_version_is_rejected_as_error() {
    let daemon = DaemonGuard::start("badversion");
    let mut stream = daemon.connect();

    let response = send_request(&mut stream, &json!({"version": 999, "method": "status"}));
    assert_eq!(response["type"], "error");
    assert!(
        response["message"].as_str().is_some_and(|m| !m.is_empty()),
        "error response must carry a nonempty message: {response}"
    );

    let follow_up = send_request(&mut stream, &json!({"version": 1, "method": "status"}));
    assert_eq!(
        follow_up["type"], "status",
        "daemon must keep serving valid requests after an unsupported-version error"
    );
}

#[test]
fn unknown_method_is_rejected_as_error_without_killing_the_daemon() {
    let daemon = DaemonGuard::start("badmethod");
    let mut stream = daemon.connect();

    let response = send_request(
        &mut stream,
        &json!({"version": 1, "method": "not_a_method"}),
    );
    assert_eq!(response["type"], "error");

    let follow_up = daemon.status_request();
    assert_eq!(follow_up["type"], "status");
}

#[test]
fn gc_returns_a_report_and_cleanup_cli_uses_the_daemon() {
    let daemon = DaemonGuard::start("cleanup");
    let mut stream = daemon.connect();
    let response = send_request(&mut stream, &json!({"version": 1, "method": "gc"}));
    assert_eq!(response["version"], 1);
    assert_eq!(response["type"], "cleanup");
    assert_eq!(response["report"]["observe"], true);
    assert!(response["report"]["services"].is_array());
    assert!(response["report"]["orphans"].is_array());
    assert!(response["report"]["pending"].is_array());
    assert!(daemon.status_request()["status"]["cleanup_pending"].is_array());
    let gc = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("gc")
        .env("BALLAST_HOME", &daemon.home.path)
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(String::from_utf8_lossy(&gc.stdout).contains("Would stop"));
    let stop = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .args(["stop", "missing-test-workload"])
        .env("BALLAST_HOME", &daemon.home.path)
        .output()
        .unwrap();
    assert!(!stop.status.success());
    assert!(String::from_utf8_lossy(&stop.stderr).contains("not found"));
}

#[test]
fn a_slow_reader_does_not_stall_other_clients_ticks() {
    let daemon = DaemonGuard::start("slowreader");

    // Queue 200 snapshot requests as one batched write (not 200 individual
    // writes/flushes, which on Linux's smaller default socket buffers can
    // each block past the 5s write timeout) without ever reading a reply,
    // so the daemon's writes to this connection back up and eventually
    // block. A write timeout here is expected backpressure from a reader
    // that never drains, not a test failure.
    let mut slow = daemon.connect();
    let mut batch = String::new();
    for _ in 0..200 {
        batch.push_str(&json!({"version": 1, "method": "snapshot"}).to_string());
        batch.push('\n');
    }
    match slow.write_all(batch.as_bytes()) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        Err(e) => panic!("unexpected error queuing the slow reader's batched requests: {e}"),
    }

    let first = daemon.status_request();
    let tick_interval_ms = first["status"]["tick_interval_ms"]
        .as_u64()
        .expect("tick_interval_ms");
    std::thread::sleep(Duration::from_millis(tick_interval_ms * 3 + 200));
    let second = daemon.status_request();

    assert!(
        second["status"]["tick"].as_u64() > first["status"]["tick"].as_u64(),
        "a backed-up slow reader must not stall ticks observed on another connection"
    );
}

#[test]
fn malformed_json_gets_an_error_reply_and_the_daemon_survives() {
    let daemon = DaemonGuard::start("malformed");
    let mut stream = daemon.connect();

    send_raw_line(&mut stream, "{not json at all");
    let response: Value = serde_json::from_str(
        &read_line(&stream)
            .expect("daemon should reply to a merely-malformed (not oversized) request"),
    )
    .expect("response should be valid JSON");
    assert_eq!(response["type"], "error");

    let follow_up = send_request(&mut stream, &json!({"version": 1, "method": "status"}));
    assert_eq!(
        follow_up["type"], "status",
        "the same connection must stay usable after a malformed request"
    );
}

#[test]
fn oversized_request_line_does_not_kill_the_daemon() {
    let daemon = DaemonGuard::start("oversized");
    let mut stream = daemon.connect();

    // Past the daemon's 64 KiB per-request budget; it is allowed to drop the
    // connection outright instead of replying.
    let padding = "x".repeat(256 * 1024);
    let oversized = json!({"version": 1, "method": "status", "padding": padding}).to_string();
    let _ = stream.write_all(oversized.as_bytes());
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
    let _ = read_line(&stream);
    drop(stream);

    let response = daemon.status_request();
    assert_eq!(
        response["type"], "status",
        "daemon must still answer a fresh connection after an oversized request"
    );
}

#[test]
fn a_hung_client_does_not_stall_tick_advance() {
    let daemon = DaemonGuard::start("hungclient");

    // Hold a connection open with a request that never gets its newline.
    let mut hung = daemon.connect();
    hung.write_all(b"{\"version\": 1")
        .expect("write partial request to hung connection");
    hung.flush().expect("flush partial request");

    let first = daemon.status_request();
    let tick_interval_ms = first["status"]["tick_interval_ms"]
        .as_u64()
        .expect("tick_interval_ms");
    let first_tick = first["status"]["tick"].as_u64().expect("tick");

    // Comfortably under the server's 5s per-connection IO timeout so `hung`
    // is still the same open connection when we finish it off below.
    std::thread::sleep(Duration::from_millis(tick_interval_ms * 3 + 200));

    let second = daemon.status_request();
    let second_tick = second["status"]["tick"].as_u64().expect("tick");
    assert!(
        second_tick > first_tick,
        "tick must keep advancing while a client is hung: {first_tick} -> {second_tick}"
    );

    hung.write_all(b", \"method\": \"status\"}\n")
        .expect("complete the previously-partial request");
    hung.flush().expect("flush completed request");
    assert!(
        read_line(&hung).is_some(),
        "the previously-hung connection must still be able to complete a request"
    );
}

#[test]
fn second_instance_fails_without_breaking_the_first() {
    let first = DaemonGuard::start("singleinstance");

    let mut second = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("daemon")
        .env("BALLAST_HOME", &first.home.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second ballast daemon");
    let status = wait_exit(&mut second, Duration::from_secs(10));
    assert!(
        !status.success(),
        "a second daemon on the same BALLAST_HOME must refuse to start"
    );

    let response = first.status_request();
    assert_eq!(
        response["type"], "status",
        "the first daemon must keep serving after a second instance was rejected"
    );
}

#[test]
fn killing_and_restarting_the_daemon_rebinds_a_stale_socket() {
    let mut daemon = DaemonGuard::start("stalesocket");
    let socket_path = daemon.home.socket_path();

    // SIGKILL releases the flock but leaves the socket file on disk, so the
    // next daemon sees a stale socket with no live holder.
    daemon.child.kill().expect("SIGKILL first daemon");
    daemon.child.wait().expect("reap first daemon");
    assert!(
        socket_path.exists(),
        "killing the daemon must not itself remove the stale socket file"
    );

    let home_path = daemon.home.path.clone();
    let restarted = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("daemon")
        .env("BALLAST_HOME", &home_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn restarted daemon");
    let restarted_guard = DaemonGuard {
        child: restarted,
        home: TempHome { path: home_path },
    };
    restarted_guard.wait_ready();

    let response = restarted_guard.status_request();
    assert_eq!(response["type"], "status");

    // `restarted_guard` now owns this home directory's cleanup.
    std::mem::forget(daemon);
}

#[test]
fn deleted_run_directory_is_recreated_while_the_daemon_is_alive() {
    let daemon = DaemonGuard::start("deletedrun");
    let run_dir = daemon.home.path.join("run");
    assert!(run_dir.exists());

    std::fs::remove_dir_all(&run_dir).expect("delete run directory out from under the daemon");
    assert!(!daemon.home.socket_path().exists());

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if UnixStream::connect(daemon.home.socket_path()).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("daemon did not recreate its socket after the run directory was deleted");
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let response = daemon.status_request();
    assert_eq!(response["type"], "status");
}

#[test]
fn run_directory_and_socket_have_restrictive_permissions() {
    let daemon = DaemonGuard::start("perms");
    let run_dir = daemon.home.path.join("run");

    assert_eq!(mode_bits(&run_dir), 0o700, "run/ directory must be 0700");
    assert_eq!(
        mode_bits(&daemon.home.socket_path()),
        0o600,
        "the socket file must be 0600"
    );

    // Single-instance ownership is a lifetime flock on BALLAST_HOME itself,
    // not a separate lockfile under run/.
    assert_eq!(
        mode_bits(&daemon.home.path),
        0o700,
        "BALLAST_HOME must be 0700, since its flock is the single-instance lock"
    );
}

#[test]
fn cli_status_json_matches_the_socket_status_contract() {
    let daemon = DaemonGuard::start("clijson");
    let socket_status = daemon.status_request();

    let output = run_cli(&daemon.home.path, &["status", "--json"]);
    assert!(
        output.status.success(),
        "ballast status --json must succeed while the daemon is reachable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli_status: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "ballast status --json did not print JSON: {:?}: {e}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    assert_eq!(cli_status["type"], "status");
    assert_eq!(
        cli_status["status"]["daemon_version"],
        socket_status["status"]["daemon_version"]
    );
}

#[test]
fn cli_status_human_readable_reports_reachable_daemon() {
    let daemon = DaemonGuard::start("clihuman");

    let output = run_cli(&daemon.home.path, &["status"]);
    assert!(
        output.status.success(),
        "ballast status must succeed while the daemon is reachable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.trim().is_empty(), "status output must not be empty");
    assert!(
        !stdout.trim_start().starts_with('{'),
        "plain `ballast status` must not print raw JSON: {stdout:?}"
    );
}

#[test]
fn cli_status_without_a_daemon_fails_open_instead_of_hanging() {
    let home = TempHome::new("noclidaemon");

    let output = run_cli(&home.path, &["status"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.trim().is_empty(),
        "ballast status with no daemon running must still print something, not hang or panic"
    );
}

#[test]
fn top_exits_when_its_terminal_hangs_up() {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let home = TempHome::new("tophup");
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    // Otherwise top inherits the master, and closing ours would never hang up the pty.
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) },
        -1
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_ballast"));
    command
        .arg("top")
        .env("BALLAST_HOME", &home.path)
        .stdin(slave.try_clone().expect("dup pty"))
        .stdout(slave.try_clone().expect("dup pty"))
        .stderr(slave);
    // Make the pty the child's controlling terminal, as a terminal emulator does.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn ballast top");
    drop(command);

    // Wait for a drawn frame so the hangup lands inside top's event loop.
    let mut output = std::fs::File::from(master);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&seen).contains("Daemon unreachable") {
        assert!(Instant::now() < deadline, "ballast top drew no frame");
        let mut ready = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&output),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut ready, 1, 100) } > 0 {
            let mut chunk = [0u8; 4096];
            let n = std::io::Read::read(&mut output, &mut chunk).expect("read frame");
            seen.extend_from_slice(&chunk[..n]);
        }
    }

    // Closing the terminal hangs up the pty; top must exit instead of spinning.
    drop(output);
    let status = wait_exit(&mut child, Duration::from_secs(5));
    assert!(
        status.success(),
        "top should exit cleanly on hangup: {status:?}"
    );
}

#[test]
fn daemon_refuses_to_start_as_root() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping daemon_refuses_to_start_as_root: test process is not running as root");
        return;
    }

    let home = TempHome::new("rootrefusal");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("daemon")
        .env("BALLAST_HOME", &home.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ballast daemon as root");
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert!(!status.success(), "daemon must refuse to run as root");
    assert!(
        !home.socket_path().exists(),
        "a daemon that refuses to start as root must not bind a socket"
    );
}

/// Not run by default (`cargo test` skips `#[ignore]`d tests): spawns ~1000
/// real processes and a live daemon side by side. Run explicitly and alone
/// with `cargo test --test daemon --release -- --ignored --nocapture
/// daemon_benchmark_at_about_1000_processes`.
#[test]
#[ignore]
fn daemon_benchmark_at_about_1000_processes() {
    use ballast::platform::{NativePlatform, Platform};

    struct FillerGuard(Child);
    impl Drop for FillerGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let target = 1000usize;
    let baseline = NativePlatform::new()
        .expect("NativePlatform::new")
        .list_processes(&Default::default(), &Default::default())
        .expect("baseline list_processes")
        .len();
    let want = target.saturating_sub(baseline);
    println!(
        "{baseline} processes already on the machine; spawning {want} fillers toward {target}"
    );

    let mut fillers = Vec::with_capacity(want);
    for _ in 0..want {
        match Command::new("sleep")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => fillers.push(FillerGuard(child)),
            Err(e) => {
                println!(
                    "stopped spawning fillers after {} (spawn failed: {e})",
                    fillers.len()
                );
                break;
            }
        }
    }
    println!("spawned {} filler processes", fillers.len());

    let daemon = DaemonGuard::start("benchmark1000");
    let daemon_pid = daemon.child.id();

    let mut ticks_seen = std::collections::BTreeSet::new();
    let mut tick_cpu_ns_samples = Vec::new();
    let mut last_process_count = 0u64;
    let deadline = Instant::now() + Duration::from_secs(120);
    while ticks_seen.len() < 20 {
        assert!(
            Instant::now() < deadline,
            "did not observe 20 unique ticks within 120s ({} seen)",
            ticks_seen.len()
        );
        let response = daemon.status_request();
        let status = &response["status"];
        let tick = status["tick"].as_u64().expect("tick");
        // Skip tick 1: always discarded (no prior tick to measure a gap against).
        if tick > 1 && ticks_seen.insert(tick) {
            tick_cpu_ns_samples.push(status["tick_cpu_ns"].as_u64().expect("tick_cpu_ns"));
            last_process_count = status["process_count"].as_u64().expect("process_count");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("daemon observed process_count: {last_process_count}");

    let mut sorted = tick_cpu_ns_samples.clone();
    sorted.sort_unstable();
    let median_ns = sorted[sorted.len() / 2];
    let mean_ns = sorted.iter().sum::<u64>() / sorted.len() as u64;
    println!(
        "tick_cpu_ns over {} warmed ticks at {} processes: median {:.3} ms, mean {:.3} ms, max {:.3} ms",
        sorted.len(),
        last_process_count,
        median_ns as f64 / 1_000_000.0,
        mean_ns as f64 / 1_000_000.0,
        sorted.last().copied().unwrap_or(0) as f64 / 1_000_000.0,
    );

    // `process_metrics` on macOS reports `ri_phys_footprint` ("memory
    // footprint", what Activity Monitor shows), not classic RSS, so it is
    // reported alongside a real `ps -o rss=` reading rather than in place
    // of one.
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    if let Some(identity) = platform
        .list_processes(&Default::default(), &Default::default())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity.pid as u32 == daemon_pid)
        .map(|p| p.identity)
    {
        if let Some(metrics) = platform.process_metrics(identity) {
            println!(
                "daemon memory_bytes via process_metrics: {:.2} MB",
                metrics.memory_bytes as f64 / (1024.0 * 1024.0)
            );
        }
    }

    let ps = Command::new("ps")
        .args(["-o", "rss=", "-p", &daemon_pid.to_string()])
        .output()
        .expect("run ps");
    let rss_kb: u64 = String::from_utf8_lossy(&ps.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("could not parse `ps -o rss=` output: {e}"));
    assert!(rss_kb > 0, "daemon RSS via ps must be nonzero");
    println!(
        "daemon actual RSS via ps -o rss=: {:.2} MB",
        rss_kb as f64 / 1024.0
    );
}

#[test]
fn socket_bind_startup_failure_is_written_to_daemon_log() {
    let home = TempHome::new("bind-log");
    let long_base = home.path.join("x".repeat(110));
    let output = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("daemon")
        .env("BALLAST_HOME", &long_base)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let log = std::fs::read_to_string(long_base.join("log/daemon.log")).unwrap();
    assert!(log.contains("daemon startup failed:"), "{log}");
    assert!(log.contains("path"), "{log}");
}

#[test]
fn replacing_the_invoked_symlink_exits_cleanly_for_the_service_manager() {
    let home = TempHome::new("replace");
    std::fs::write(
        home.path.join("config.toml"),
        "mode = \"observe\"\nnotifications = false\nrecovery_sweep_markers = []\n",
    )
    .unwrap();
    let binary = home.path.join("ballast");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ballast"), &binary).unwrap();
    let child = Command::new(&binary)
        .arg("daemon")
        .env("BALLAST_HOME", &home.path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = DaemonGuard { child, home };
    daemon.wait_ready();
    assert_eq!(daemon.status_request()["type"], "status");
    let replacement = daemon.home.path.join("replacement");
    std::fs::copy(env!("CARGO_BIN_EXE_ballast"), &replacement).unwrap();
    let link = daemon.home.path.join("new-link");
    std::os::unix::fs::symlink(&replacement, &link).unwrap();
    std::fs::rename(link, binary).unwrap();
    assert!(wait_exit(&mut daemon.child, Duration::from_secs(10)).success());
}
