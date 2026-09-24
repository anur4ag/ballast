//! Black-box integration tests for `ballast hook claude|codex` (ticket 06).
//!
//! Most scenarios drive the CLI as a real subprocess against a scripted fake daemon: a
//! bare `UnixListener` this file binds at the test's `BALLAST_HOME` socket and controls
//! byte-for-byte, so admission decisions (admit/hold/deny) and failure modes
//! (down/slow/garbage) are exact and independent of real system pressure or ticket 07's
//! classification data. One test at the end talks to a real spawned `ballast daemon` to
//! check the healthy-path latency budget.
//!
//! Isolation follows `tests/daemon.rs`: every test gets its own `BALLAST_HOME` under
//! `/tmp` (not `std::env::temp_dir()`, whose macOS `TMPDIR` can blow the 104-byte unix
//! socket path limit).
//!
//! Fixtures below are derived from `spikes/out/*hook.jsonl`, with session ids,
//! transcript paths and `cwd` replaced by placeholders.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------
// Fixtures: recorded Claude/Codex PreToolUse and SessionStart payloads,
// sanitized. See spikes/out/claude-hold-hook.jsonl and codex-hold-hook.jsonl.
// ---------------------------------------------------------------------

const CLAUDE_PRETOOLUSE_LIGHT: &str = r#"{
  "session_id": "11111111-1111-4111-8111-111111111111",
  "transcript_path": "/home/user/.claude/projects/example/11111111-1111-4111-8111-111111111111.jsonl",
  "cwd": "/home/user/project",
  "prompt_id": "22222222-2222-4222-8222-222222222222",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {"command": "echo hello", "timeout": 3000, "description": "Say hello"},
  "tool_use_id": "toolu_0000000000000000000000000000"
}"#;

const CODEX_PRETOOLUSE_LIGHT: &str = r#"{
  "session_id": "33333333-3333-4333-8333-333333333333",
  "turn_id": "44444444-4444-4444-8444-444444444444",
  "transcript_path": null,
  "cwd": "/home/user/project",
  "hook_event_name": "PreToolUse",
  "model": "example-model",
  "permission_mode": "bypassPermissions",
  "tool_name": "Bash",
  "tool_input": {"command": "echo hello"},
  "tool_use_id": "exec-0000000000000000000000000000"
}"#;

const CLAUDE_PRETOOLUSE_KILL: &str = r#"{
  "session_id": "11111111-1111-4111-8111-111111111111",
  "transcript_path": "/home/user/.claude/projects/example/11111111-1111-4111-8111-111111111111.jsonl",
  "cwd": "/home/user/project",
  "prompt_id": "22222222-2222-4222-8222-222222222222",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {"command": "pkill node"},
  "tool_use_id": "toolu_0000000000000000000000000001"
}"#;

const CODEX_PRETOOLUSE_KILL: &str = r#"{
  "session_id": "33333333-3333-4333-8333-333333333333",
  "turn_id": "44444444-4444-4444-8444-444444444444",
  "transcript_path": null,
  "cwd": "/home/user/project",
  "hook_event_name": "PreToolUse",
  "model": "example-model",
  "permission_mode": "bypassPermissions",
  "tool_name": "Bash",
  "tool_input": {"command": "pkill node"},
  "tool_use_id": "exec-0000000000000000000000000001"
}"#;

const CLAUDE_SESSION_START: &str = r#"{
  "session_id": "55555555-5555-4555-8555-555555555555",
  "cwd": "/home/user/project",
  "hook_event_name": "SessionStart"
}"#;

// ---------------------------------------------------------------------
// A temp BALLAST_HOME, short enough for the unix socket path limit.
// ---------------------------------------------------------------------

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
        let path = PathBuf::from(format!("/tmp/blt-hk-{tag}-{unique}"));
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

// ---------------------------------------------------------------------
// A scripted fake daemon: binds the socket, accepts one connection, hands
// the request line and a writer to `respond`, and returns the parsed
// request for the test to inspect.
// ---------------------------------------------------------------------

fn fake_daemon(
    home: &TempHome,
    respond: impl FnOnce(&Value, &mut UnixStream) + Send + 'static,
) -> thread::JoinHandle<Value> {
    std::fs::create_dir_all(home.socket_path().parent().unwrap()).expect("create run dir");
    let listener = UnixListener::bind(home.socket_path()).expect("bind fake daemon socket");
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("hook did not connect within {CONNECT_TIMEOUT:?}: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(CLI_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(CLI_TIMEOUT)).unwrap();
        let mut writer = stream.try_clone().expect("clone fake daemon stream");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read hook request line");
        let request: Value =
            serde_json::from_str(line.trim_end()).expect("parse hook request as JSON");
        respond(&request, &mut writer);
        request
    })
}
fn write_frame(stream: &mut UnixStream, value: &Value) {
    let mut text = value.to_string();
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .expect("write fake daemon frame");
    stream.flush().expect("flush fake daemon frame");
}
fn admit_frame() -> Value {
    json!({"version": 1, "type": "hook", "decision": {"decision": "admit"}})
}
fn hold_frame() -> Value {
    json!({"version": 1, "type": "hook", "decision": {"decision": "hold"}})
}
fn deny_frame(reason: &str) -> Value {
    json!({"version": 1, "type": "hook", "decision": {"decision": "deny", "reason": reason}})
}

// ---------------------------------------------------------------------
// Running `ballast hook <agent>` as a bounded subprocess.
// ---------------------------------------------------------------------

fn run_hook(
    home: &TempHome,
    agent: &str,
    stdin: &str,
    extra_args: &[&str],
) -> (std::process::Output, Duration) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("hook")
        .arg(agent)
        .args(extra_args)
        .env("BALLAST_HOME", &home.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn ballast hook {agent}: {e}"));
    {
        let mut pipe = child.stdin.take().expect("hook stdin");
        pipe.write_all(stdin.as_bytes()).expect("write hook stdin");
    }
    let started = Instant::now();
    let deadline = started + CLI_TIMEOUT;
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            let elapsed = started.elapsed();
            return (
                child.wait_with_output().expect("collect hook output"),
                elapsed,
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ballast hook {agent} did not exit within {CLI_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------
// Normalization: recorded per-agent payloads become one internal request.
// ---------------------------------------------------------------------

#[test]
fn claude_pretooluse_is_normalized_before_being_admitted() {
    let home = TempHome::new("claude-normalize");
    let daemon = fake_daemon(&home, |_request, stream| {
        write_frame(stream, &admit_frame())
    });

    let (output, elapsed) = run_hook(&home, "claude", CLAUDE_PRETOOLUSE_LIGHT, &[]);
    let request = daemon.join().expect("fake daemon thread panicked");

    assert_eq!(request["method"], "hook");
    let payload = &request["payload"];
    assert_eq!(payload["agent"], "claude");
    assert_eq!(payload["event"], "PreToolUse");
    assert_eq!(payload["tool_name"], "Bash");
    assert_eq!(payload["command"], "echo hello");
    assert_eq!(payload["cwd"], "/home/user/project");
    assert_eq!(
        payload["session_id"],
        "11111111-1111-4111-8111-111111111111"
    );

    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "an admitted command must produce no output: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "a healthy admit should be fast, not held or stuck, took {elapsed:?}"
    );
}

#[test]
fn codex_pretooluse_is_normalized_before_being_admitted() {
    let home = TempHome::new("codex-normalize");
    let daemon = fake_daemon(&home, |_request, stream| {
        write_frame(stream, &admit_frame())
    });

    let (output, _elapsed) = run_hook(&home, "codex", CODEX_PRETOOLUSE_LIGHT, &[]);
    let request = daemon.join().expect("fake daemon thread panicked");

    let payload = &request["payload"];
    assert_eq!(payload["agent"], "codex");
    assert_eq!(payload["event"], "PreToolUse");
    assert_eq!(payload["tool_name"], "Bash");
    assert_eq!(payload["command"], "echo hello");
    assert_eq!(
        payload["session_id"],
        "33333333-3333-4333-8333-333333333333"
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn session_start_lifecycle_event_normalizes_without_tool_fields() {
    let home = TempHome::new("sessionstart");
    let daemon = fake_daemon(&home, |_request, stream| {
        write_frame(stream, &admit_frame())
    });

    let (output, _elapsed) = run_hook(&home, "claude", CLAUDE_SESSION_START, &[]);
    let request = daemon.join().expect("fake daemon thread panicked");

    let payload = &request["payload"];
    assert_eq!(payload["event"], "SessionStart");
    assert!(payload["tool_name"].is_null());
    assert!(payload["command"].is_null());
    assert_eq!(
        payload["session_id"],
        "55555555-5555-4555-8555-555555555555"
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

// ---------------------------------------------------------------------
// Fail-open: down, silent past the ceiling, and garbage.
// ---------------------------------------------------------------------

#[test]
fn daemon_unreachable_fails_open_silently() {
    let home = TempHome::new("down");

    let (output, elapsed) = run_hook(&home, "claude", CLAUDE_PRETOOLUSE_LIGHT, &[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "an unreachable daemon must fail open quickly, not hang, took {elapsed:?}"
    );
}

#[test]
fn daemon_accepts_but_never_responds_fails_open_at_the_ceiling() {
    let home = TempHome::new("silent");
    // Comfortably past the 200ms non-hold ceiling, and past every timing margin used
    // below, so a pass here can only mean the hook gave up on its own budget.
    let _daemon = fake_daemon(&home, |_request, _stream| {
        thread::sleep(Duration::from_secs(3));
    });

    let (output, elapsed) = run_hook(&home, "claude", CLAUDE_PRETOOLUSE_LIGHT, &[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "must give up around the 200ms non-hold ceiling rather than wait for the daemon, took {elapsed:?}"
    );
}

#[test]
fn garbage_response_fails_open_silently() {
    let home = TempHome::new("garbage");
    let _daemon = fake_daemon(&home, |_request, stream| {
        stream
            .write_all(b"not json at all\n")
            .expect("write garbage");
        let _ = stream.flush();
    });

    let (output, elapsed) = run_hook(&home, "claude", CLAUDE_PRETOOLUSE_LIGHT, &[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(elapsed < Duration::from_secs(2));
}

#[test]
fn slow_trickled_daemon_response_fails_open_at_the_ceiling() {
    let home = TempHome::new("trickle");
    // One byte every 100ms: the full frame (well under 100 bytes) would take several
    // seconds to arrive, far past the 200ms non-hold ceiling.
    let _daemon = fake_daemon(&home, |_request, stream| {
        let frame = format!("{}\n", admit_frame());
        for byte in frame.as_bytes() {
            if stream.write_all(std::slice::from_ref(byte)).is_err() {
                return;
            }
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(100));
        }
    });

    let (output, elapsed) = run_hook(&home, "claude", CLAUDE_PRETOOLUSE_LIGHT, &[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "a response trickling in far slower than the ceiling must still fail open early, not wait for the full frame, took {elapsed:?}"
    );
}

#[test]
fn malformed_stdin_fails_open_even_if_stdin_never_closes() {
    let home = TempHome::new("malformedstdin");
    // No fake daemon: malformed stdin must fail before ever attempting to connect.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ballast"))
        .arg("hook")
        .arg("claude")
        .env("BALLAST_HOME", &home.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ballast hook claude");
    child
        .stdin
        .as_mut()
        .expect("hook stdin")
        .write_all(b"{not valid json")
        .expect("write malformed stdin");
    // Deliberately never close stdin (no EOF), so a stuck stdin read can't itself explain
    // an early exit: only the hook's own outer deadline can.

    let started = Instant::now();
    let deadline = started + CLI_TIMEOUT;
    let output = loop {
        if let Some(_status) = child.try_wait().expect("try_wait") {
            break child.wait_with_output().expect("collect hook output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "ballast hook claude did not exit within {CLI_TIMEOUT:?} despite malformed, unterminated stdin"
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    let elapsed = started.elapsed();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "malformed/unterminated stdin must not block exit past the hook's own deadline, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------
// Hold: streamed intermediate then final decision, and its own deadline.
// ---------------------------------------------------------------------

#[test]
fn hold_then_admit_waits_for_the_final_decision_then_stays_silent() {
    let home = TempHome::new("holdadmit");
    let _daemon = fake_daemon(&home, |_request, stream| {
        write_frame(stream, &hold_frame());
        thread::sleep(Duration::from_millis(150));
        write_frame(stream, &admit_frame());
    });

    let (output, elapsed) = run_hook(&home, "codex", CODEX_PRETOOLUSE_LIGHT, &[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed >= Duration::from_millis(120),
        "must actually wait for the held decision, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "must not wait past its own deadline once admitted, took {elapsed:?}"
    );
}

#[test]
fn hold_past_its_own_deadline_fails_open() {
    let home = TempHome::new("deadline");
    let _daemon = fake_daemon(&home, |_request, stream| {
        write_frame(stream, &hold_frame());
        thread::sleep(Duration::from_secs(6));
    });

    // --timeout-seconds 12 => own deadline is 12 - 10 = 2s: comfortably before the fake
    // daemon's 6s hang and far short of the daemon's real 5 minute max hold.
    let (output, elapsed) = run_hook(
        &home,
        "claude",
        CLAUDE_PRETOOLUSE_LIGHT,
        &["--timeout-seconds", "12"],
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        elapsed >= Duration::from_millis(1_800),
        "must wait out its own deadline, not give up immediately, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "must not wait for the daemon past its own deadline, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------
// Deny: the reason reaches the model, and nothing ever allows or rewrites.
// ---------------------------------------------------------------------

#[test]
fn deny_reports_the_reason_and_never_allows_or_rewrites_claude() {
    let home = TempHome::new("denyclaude");
    let reason = "Ballast blocked this: pkill node would also kill 2 processes belonging to \
        another agent. Your own node processes are PIDs 100 and 101; kill those directly."
        .to_string();
    let response_reason = reason.clone();
    let _daemon = fake_daemon(&home, move |_request, stream| {
        write_frame(stream, &deny_frame(&response_reason))
    });

    let (output, _elapsed) = run_hook(&home, "claude", CLAUDE_PRETOOLUSE_KILL, &[]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("deny output was not JSON: {stdout:?}: {e}"));
    assert_eq!(
        parsed,
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }})
    );
}

#[test]
fn deny_reports_the_reason_and_never_allows_or_rewrites_codex() {
    let home = TempHome::new("denycodex");
    let reason = "Ballast blocked this: pkill node would also kill 2 processes belonging to \
        another agent. Your own node processes are PIDs 100 and 101; kill those directly."
        .to_string();
    let response_reason = reason.clone();
    let _daemon = fake_daemon(&home, move |_request, stream| {
        write_frame(stream, &deny_frame(&response_reason))
    });

    let (output, _elapsed) = run_hook(&home, "codex", CODEX_PRETOOLUSE_KILL, &[]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("deny output was not JSON: {stdout:?}: {e}"));
    assert_eq!(
        parsed["hookSpecificOutput"]["permissionDecision"], "deny",
        "must never emit allow"
    );
    assert_eq!(
        parsed["hookSpecificOutput"]["permissionDecisionReason"],
        reason
    );
    assert!(
        parsed["hookSpecificOutput"].get("updatedInput").is_none(),
        "must never rewrite the command"
    );
}

// ---------------------------------------------------------------------
// One end-to-end check against the real daemon: healthy-path latency.
// ---------------------------------------------------------------------

struct DaemonGuard {
    child: Child,
    home: TempHome,
}
impl DaemonGuard {
    fn start(tag: &str) -> Self {
        Self::start_config(tag, "mode = \"observe\"\n")
    }
    fn start_config(tag: &str, config: &str) -> Self {
        let home = TempHome::new(tag);
        // Observe mode: this spawns a real daemon against the real host process table, and
        // the guardian must never signal (freeze/stop) an unrelated real process just
        // because the ambient host happens to be under ("Critical") pressure while this
        // test runs. Admission's own decisions are unaffected -- observe mode only
        // suppresses acting on a freeze, not the admit/hold/deny path this test checks.
        std::fs::write(
            home.path.join("config.toml"),
            format!("notifications = false\nrecovery_sweep_markers = []\n{config}"),
        )
        .expect("write temp config.toml");
        let child = Command::new(env!("CARGO_BIN_EXE_ballast"))
            .arg("daemon")
            .env("BALLAST_HOME", &home.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ballast daemon");
        let guard = DaemonGuard { child, home };
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            if UnixStream::connect(guard.home.socket_path()).is_ok() {
                return guard;
            }
            if Instant::now() >= deadline {
                panic!("daemon did not become ready within {CONNECT_TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn healthy_daemon_admits_a_light_command_quickly() {
    let daemon = DaemonGuard::start("healthy");

    let (output, elapsed) = run_hook(&daemon.home, "claude", CLAUDE_PRETOOLUSE_LIGHT, &[]);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    // The done-when criterion is single-digit-millisecond round trips; this bound is far
    // looser to absorb process-spawn overhead under test load while still catching any
    // real stall (e.g. an accidental hold or a full 200ms ceiling wait).
    assert!(
        elapsed < Duration::from_secs(1),
        "a healthy-path admit should be fast, took {elapsed:?}"
    );
}

#[test]
fn post_tool_hints_are_additional_context_only_on_the_right_event() {
    for agent in ["claude", "codex"] {
        for event in ["PostToolUse", "PreToolUse", "SessionStart"] {
            let home = TempHome::new("hint");
            let daemon = fake_daemon(&home, |_request, stream| {
                write_frame(
                    stream,
                    &json!({"version": 1, "type": "hook", "decision": {
                        "decision": "hint", "context": "Port 3000 is held by another agent."
                    }}),
                );
            });
            let (output, _) = run_hook(
                &home,
                agent,
                &json!({
                    "session_id": "hint-session", "hook_event_name": event,
                    "tool_name": "Bash", "tool_input": {"command": "npm start"},
                    "tool_response": {"stderr": "Port 3000 is in use"}
                })
                .to_string(),
                &[],
            );
            daemon.join().unwrap();
            assert!(output.status.success());
            if event == "PostToolUse" {
                assert_eq!(
                    serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                    json!({
                        "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "Port 3000 is held by another agent."}
                    })
                );
            } else {
                assert!(output.stdout.is_empty());
            }
        }
    }
}

struct OwnedServer(Child);
impl Drop for OwnedServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn server_fixture(
    home: &TempHome,
    exe: &std::path::Path,
    marker: &str,
    session: &str,
) -> (OwnedServer, u16) {
    let ready = home.path.join(session);
    let mut command = Command::new(exe);
    command
        .args(["port_server_fixture", "--exact", "--ignored"])
        .env_clear()
        .env(marker, session)
        .env("BALLAST_TEST_READY", &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = OwnedServer(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(port) = std::fs::read_to_string(&ready) {
            if let Ok(port) = port.parse() {
                return (child, port);
            }
        }
        assert!(Instant::now() < deadline, "server fixture readiness");
        thread::sleep(Duration::from_millis(10));
    }
}
#[test]
#[ignore]
fn port_server_fixture() {
    let Ok(ready) = std::env::var("BALLAST_TEST_READY") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(ready, listener.local_addr().unwrap().port().to_string()).unwrap();
    thread::sleep(Duration::from_secs(60));
    drop(listener);
}

#[test]
fn two_agent_port_server_survives_cross_agent_hooks_and_reports_collision() {
    use ballast::daemon::files::{Config, Mode, Paths, RotatingLog};
    use ballast::daemon::ipc::{Client, Method, PendingRequest, Reply};
    use ballast::hooks::{Admission, HookRequest, HookState};
    use std::sync::{Arc, atomic::AtomicBool, mpsc};
    let home = TempHome::new("owned-servers");
    let exe = home.path.join("node");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let (mut a, port_a) = server_fixture(&home, &exe, "BALLAST_TEST_CLAUDE", "agent-a");
    let (mut b, _port_b) = server_fixture(&home, &exe, "BALLAST_TEST_CODEX", "agent-b");
    let daemon = DaemonGuard::start_config(
        "port-observer",
        r#"
mode = "observe"
[[markers]]
key = "BALLAST_TEST_CLAUDE"
level = "agent"
kind = "claude"
root_binaries = ["node"]
[[markers]]
key = "BALLAST_TEST_CODEX"
level = "agent"
kind = "codex"
root_binaries = ["node"]
"#,
    );
    let paths = Paths {
        base: daemon.home.path.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(12);
    let snapshot = loop {
        let response = Client::connect(&paths, Duration::from_secs(1))
            .unwrap()
            .request(Method::Snapshot)
            .unwrap();
        if let Reply::Snapshot { snapshot } = response.reply {
            if snapshot.attribution.agents.iter().any(|agent| {
                agent.kind == "claude" && agent.session_id.as_deref() == Some("agent-a")
            }) && snapshot
                .attribution
                .agents
                .iter()
                .any(|a| a.kind == "codex" && a.session_id.as_deref() == Some("agent-b"))
            {
                let mut snapshot = Arc::try_unwrap(snapshot).unwrap();
                snapshot.status.mode = Mode::Enforce;
                break Arc::new(snapshot);
            }
        }
        assert!(
            Instant::now() < deadline,
            "native ownership and port snapshot"
        );
        thread::sleep(Duration::from_millis(30));
    };
    // The real observer stays in observe mode. Only hook policy is enforced, so this
    // test can never freeze or clean up an unrelated process on a pressured host.
    for (agent, session, event, command, deny) in [
        (
            "codex",
            "agent-b",
            "PreToolUse",
            "pkill node".to_owned(),
            true,
        ),
        (
            "codex",
            "agent-b",
            "PreToolUse",
            format!("lsof -ti:{port_a} | xargs kill"),
            true,
        ),
        (
            "claude",
            "agent-a",
            "PreToolUse",
            format!("lsof -t -i:{port_a} | xargs kill"),
            false,
        ),
        (
            "claude",
            "agent-b",
            "PostToolUse",
            "start server".to_owned(),
            false,
        ),
        (
            "codex",
            "agent-b",
            "PostToolUse",
            "start server".to_owned(),
            false,
        ),
    ] {
        let policy_home = TempHome::new("port-policy");
        let mut policy_snapshot: ballast::daemon::Snapshot =
            serde_json::from_value(serde_json::to_value(snapshot.as_ref()).unwrap()).unwrap();
        // Model which agent launched this hook while exercising real peer credentials
        // and the new hook process's native parent chain.
        let caller = policy_snapshot
            .attribution
            .agents
            .iter()
            .find(|a| a.kind == agent && a.session_id.as_deref() == Some(session))
            .map(|a| a.id.clone());
        policy_snapshot
            .attribution
            .processes
            .iter_mut()
            .find(|p| p.identity.pid == std::process::id() as i32)
            .unwrap()
            .agent_id = caller;
        let log_path = policy_home.path.join("decisions.jsonl");
        let policy = fake_daemon(&policy_home, move |wire, stream| {
            let request: HookRequest = serde_json::from_value(wire["payload"].clone()).unwrap();
            let (reply, receiver) = mpsc::channel();
            let pending = PendingRequest {
                evidence: ballast::hooks::lookup(
                    &request,
                    &policy_snapshot,
                    ballast::daemon::ipc::peer_pid(stream),
                ),
                method: Method::Hook {
                    payload: Value::Null,
                },
                reply,
                cancelled: Arc::new(AtomicBool::new(false)),
            };
            Admission::new(&[]).handle(
                request,
                pending,
                &policy_snapshot,
                &mut HookState::default(),
                &mut RotatingLog::open(log_path, &Config::default()).unwrap(),
                Instant::now(),
            );
            write_frame(
                stream,
                &serde_json::to_value(receiver.recv().unwrap()).unwrap(),
            );
        });
        let collision = std::net::TcpListener::bind(("127.0.0.1", port_a)).unwrap_err();
        assert_eq!(collision.kind(), std::io::ErrorKind::AddrInUse);
        let input = json!({"session_id": session, "hook_event_name": event, "tool_name": "Bash", "tool_input": {"command": command}, "tool_response": {"stderr": format!("EADDRINUSE: {collision}, port: {port_a}")}});
        let (output, elapsed) = run_hook(&policy_home, agent, &input.to_string(), &[]);
        if event == "PreToolUse" {
            println!(
                "native {} hook (deny={deny}): {:.3} ms",
                if command == "pkill node" {
                    "name"
                } else {
                    "port"
                },
                elapsed.as_secs_f64() * 1000.0
            );
        }
        policy.join().unwrap();
        assert!(output.status.success());
        if deny {
            let output: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
            if command == "pkill node" {
                assert!(output.to_string().contains(&b.0.id().to_string()));
            }
        } else if event == "PostToolUse" {
            let output: Value = serde_json::from_slice(&output.stdout).unwrap();
            let context = output["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap();
            assert!(
                context.contains(&format!("Port {port_a}"))
                    && context.contains(&format!("PID {}", a.0.id()))
            );
        } else {
            assert!(output.stdout.is_empty());
        }
        assert!(a.0.try_wait().unwrap().is_none() && b.0.try_wait().unwrap().is_none());
        assert!(std::net::TcpStream::connect(("127.0.0.1", port_a)).is_ok());
    }
    // Exercise the real daemon IPC worker's on-demand lookup as well as policy enforcement.
    // Agent roots have no cached ports, so this cannot pass using classification samples.
    for agent in ["claude", "codex"] {
        let input = json!({"session_id": "agent-b", "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "start server"}, "tool_response": {"stderr": format!("Port {port_a} is in use")}});
        let (output, elapsed) = run_hook(&daemon.home, agent, &input.to_string(), &[]);
        let output: Value = serde_json::from_slice(&output.stdout).unwrap();
        let context = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains(&format!("PID {}", a.0.id())));
        println!(
            "{agent} native on-demand hint: {:.3} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

#[test]
fn native_name_sampling_matches_pgrep_and_killall() {
    use ballast::platform::{NativePlatform, Platform};
    let home = TempHome::new("native-name");
    let name = format!("blt{}longprocessname", std::process::id());
    let exe = home.path.join(&name);
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let (child, _) = server_fixture(&home, &exe, "BALLAST_TEST_UNUSED", "native");
    let mut platform = NativePlatform::new().unwrap();
    let processes = platform
        .list_processes(&Default::default(), &Default::default())
        .unwrap();
    let process = processes
        .iter()
        .find(|p| p.identity.pid == child.0.id() as i32)
        .unwrap();
    let expected = if cfg!(target_os = "linux") {
        &name[..15]
    } else {
        &name
    };
    if cfg!(target_os = "linux") {
        assert_eq!(process.name.as_deref(), Some(expected));
    }
    let pgrep = Command::new("pgrep")
        .args(["-x", expected])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&pgrep.stdout)
            .split_whitespace()
            .any(|pid| pid == child.0.id().to_string())
    );
    let flags = if cfg!(target_os = "macos") {
        "-s"
    } else {
        "-0"
    };
    let killall = Command::new("killall")
        .args([flags, "-v", &name])
        .output()
        .unwrap();
    assert!(
        killall.status.success(),
        "{}",
        String::from_utf8_lossy(&killall.stderr)
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&killall.stdout),
        String::from_utf8_lossy(&killall.stderr)
    );
    assert!(
        output.contains(&child.0.id().to_string()),
        "native killall did not identify test process"
    );
}
