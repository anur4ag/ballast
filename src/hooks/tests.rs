//! Scenario tests for the hook bridge (ticket 06): classification, lifecycle/label
//! correlation, and the admission queue's fairness, pacing and fail-open rules.
//!
//! `Admission`/`HookState` are driven directly with hand-built `HookRequest`/`Snapshot`
//! fixtures and a real `RotatingLog` under a temp `BALLAST_HOME` (mirroring
//! `guardian::tests`'s approach) -- never a reference model, never the real daemon loop
//! or a real socket.

use super::*;
use crate::attribution::{
    Agent, AgentState, AttributionSnapshot, MemorySummary, ProcessAttribution, ProcessRole,
    Workload, WorkloadClass,
};
use crate::daemon::files::{Config, Mode, Paths, RotatingLog};
use crate::daemon::ipc::{Method, PendingRequest, Reply, Response};
use crate::daemon::{ProcessChanges, Snapshot, Status};
use crate::guardian::Level;
use crate::platform::{Capabilities, PressureInputs, ProcessIdentity};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------

struct TestHome(Paths);
impl TestHome {
    fn new(tag: &str) -> Self {
        let path = PathBuf::from(format!("/tmp/blt-hooks-{tag}-{}", std::process::id()));
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
    fn decisions(&self) -> String {
        std::fs::read_to_string(self.0.base.join("log/decisions.jsonl")).unwrap_or_default()
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

/// `pressure: None` by default; use [`known_snapshot`] for hold-eligible fixtures, since
/// the admission queue treats a missing or discarded sample as an unknown pressure signal.
fn snapshot(level: Level, batch_running: bool, mode: Mode) -> Snapshot {
    snapshot_with(level, batch_running, mode, None, false)
}
fn known_snapshot(level: Level, batch_running: bool, mode: Mode) -> Snapshot {
    snapshot_with(
        level,
        batch_running,
        mode,
        Some(PressureInputs::default()),
        false,
    )
}
fn discarded_snapshot(level: Level, batch_running: bool, mode: Mode) -> Snapshot {
    snapshot_with(
        level,
        batch_running,
        mode,
        Some(PressureInputs::default()),
        true,
    )
}
fn snapshot_with(
    level: Level,
    batch_running: bool,
    mode: Mode,
    pressure: Option<PressureInputs>,
    sample_discarded: bool,
) -> Snapshot {
    Snapshot {
        status: Status {
            daemon_version: "test".into(),
            pid: 0,
            mode,
            tick: 0,
            sampled_at_ms: 0,
            tick_interval_ms: 1000,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded,
            process_count: 0,
            pressure_level: level,
            batch_running,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: "fake-boot".into(),
        capabilities: Capabilities {
            environment: false,
            listening_ports: false,
            memory_footprint: false,
            memory_psi: false,
            kernel_pressure: false,
            notifications: false,
            atomic_signals: false,
        },
        processes: Vec::new(),
        changes: ProcessChanges::default(),
        pressure,
        attribution: AttributionSnapshot::default(),
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
    }
}

fn hook_request(
    agent: AgentKind,
    session_id: &str,
    event: Event,
    command: Option<&str>,
) -> HookRequest {
    HookRequest {
        agent,
        session_id: session_id.into(),
        event,
        cwd: Some("/work".into()),
        command: command.map(str::to_owned),
        tool_name: command.map(|_| "Bash".to_owned()),
        tool_response: None,
    }
}

/// A `PendingRequest` fixture with a receiver the test can poll for the decision(s) sent
/// back over it, and the `cancelled` flag the daemon flips on a dropped connection.
fn connection() -> (PendingRequest, mpsc::Receiver<Response>, Arc<AtomicBool>) {
    let (reply, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = PendingRequest {
        evidence: Default::default(),
        method: Method::Hook {
            payload: serde_json::Value::Null,
        },
        reply,
        cancelled: cancelled.clone(),
    };
    (request, receiver, cancelled)
}

fn attribution_agent(id: &str, kind: &str, session_id: &str, state: AgentState) -> Agent {
    Agent {
        id: id.into(),
        owner_id: None,
        session_id: Some(session_id.into()),
        kind: kind.into(),
        root: None,
        cwd: None,
        state,
        ended_at_ms: None,
        memory: MemorySummary::default(),
    }
}
fn attribution_workload(id: &str, agent_id: &str, label: &str) -> Workload {
    Workload {
        id: id.into(),
        agent_id: agent_id.into(),
        root: ProcessIdentity {
            pid: 1,
            start_time: 1,
        },
        label: label.into(),
        class: WorkloadClass::Batch,
        first_seen_ms: 0,
        detached_pgid: None,
        memory: MemorySummary::default(),
    }
}

fn decision(response: Response) -> HookDecision {
    let Reply::Hook { decision } = response.reply else {
        panic!("expected a hook reply, got {:?}", response.reply);
    };
    decision
}

// ---------------------------------------------------------------------
// Classification (segments, built-ins, custom rules)
// ---------------------------------------------------------------------

#[test]
fn built_in_heavy_patterns_match_at_segment_boundaries() {
    let classifier = Classifier::new(&[]);
    assert!(classifier.heavy("npm install"));
    assert!(classifier.heavy("npm run build"));
    assert!(
        !classifier.heavy("npm start"),
        "unclassified commands default to light"
    );
    assert!(
        classifier.heavy("echo hi; npm install"),
        "any heavy segment makes the whole (segmented) command heavy"
    );
    assert!(
        classifier.heavy("/usr/local/bin/npm install"),
        "matches on the program's basename, not the full path"
    );
    assert!(
        classifier.heavy("FOO=1 npm install"),
        "leading env assignments are skipped"
    );
    assert!(
        classifier.heavy("env FOO=1 npm install"),
        "an env wrapper is skipped"
    );
    assert!(
        !classifier.heavy("npm install 'unterminated"),
        "an unresolvable (unterminated quote) command stays light"
    );
    assert!(
        !classifier.heavy("cat <<'EOF'\ncargo build\nEOF"),
        "a heredoc body is text, not a command; `cargo build` inside one must not trigger a hold"
    );
}

/// Regression: a `\`-newline shell line continuation must disappear (join the words on
/// either side of it), not survive as a stray token or act as a segment separator -- both
/// of which previously broke matching on a wrapped heavy command.
#[test]
fn backslash_newline_line_continuation_joins_words_without_a_stray_token() {
    let classifier = Classifier::new(&[]);
    assert!(
        classifier.heavy("npm \\\n  install"),
        "a line continuation between words must not act as a segment separator"
    );
    assert!(
        classifier.heavy("npm ins\\\ntall"),
        "a line continuation inside a word must not split it or inject a token"
    );
}

#[test]
fn custom_heavy_rules_extend_the_built_in_list() {
    let classifier = Classifier::new(&["my-tool slow-build".into()]);
    assert!(classifier.heavy("my-tool slow-build --release"));
    assert!(
        !classifier.heavy("my-tool fast-build"),
        "custom rules match their own segment only"
    );
    assert!(
        classifier.heavy("npm install"),
        "built-ins still apply alongside custom rules"
    );
}

// ---------------------------------------------------------------------
// Lifecycle state and PreToolUse label correlation
// ---------------------------------------------------------------------

#[test]
fn lifecycle_events_drive_agent_thinking_idle_state() {
    let mut state = HookState::default();
    let now = Instant::now();
    let mut snap = AttributionSnapshot {
        agents: vec![attribution_agent("a1", "claude", "s1", AgentState::Unknown)],
        ..Default::default()
    };

    state.receive(
        &hook_request(AgentKind::Claude, "s1", Event::SessionStart, None),
        now,
    );
    state.apply(&mut snap, now);
    assert_eq!(snap.agents[0].state, AgentState::Idle);

    state.receive(
        &hook_request(AgentKind::Claude, "s1", Event::PreToolUse, Some("echo hi")),
        now,
    );
    state.apply(&mut snap, now);
    assert_eq!(snap.agents[0].state, AgentState::Thinking);

    state.receive(
        &hook_request(AgentKind::Claude, "s1", Event::Stop, None),
        now,
    );
    state.apply(&mut snap, now);
    assert_eq!(snap.agents[0].state, AgentState::Idle);
}

#[test]
fn session_end_marks_idle_but_never_downgrades_an_already_ended_agent() {
    let mut state = HookState::default();
    let now = Instant::now();
    let mut snap = AttributionSnapshot {
        agents: vec![attribution_agent("a1", "claude", "s1", AgentState::Ended)],
        ..Default::default()
    };

    // A stray SessionEnd (the coordinator's research found `/clear` emits one) must not
    // downgrade an agent the root-exit detector already marked Ended.
    state.receive(
        &hook_request(AgentKind::Claude, "s1", Event::SessionEnd, None),
        now,
    );
    state.apply(&mut snap, now);
    assert_eq!(snap.agents[0].state, AgentState::Ended);
}

#[test]
fn pretooluse_label_attaches_to_a_workload_appearing_within_two_seconds() {
    let mut state = HookState::default();
    let now = Instant::now();
    state.receive(
        &hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        now,
    );
    let mut snap = AttributionSnapshot {
        agents: vec![attribution_agent(
            "a1",
            "claude",
            "s1",
            AgentState::Thinking,
        )],
        workloads: vec![attribution_workload("w1", "a1", "w1")],
        ..Default::default()
    };

    state.apply(&mut snap, now + Duration::from_millis(1_500));
    assert_eq!(snap.workloads[0].label, "npm install");
}

#[test]
fn pretooluse_label_expires_after_two_seconds_and_does_not_attach() {
    let mut state = HookState::default();
    let now = Instant::now();
    state.receive(
        &hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        now,
    );
    let mut snap = AttributionSnapshot {
        agents: vec![attribution_agent(
            "a1",
            "claude",
            "s1",
            AgentState::Thinking,
        )],
        workloads: vec![attribution_workload("w1", "a1", "original-label")],
        ..Default::default()
    };

    state.apply(&mut snap, now + Duration::from_millis(2_500));
    assert_eq!(snap.workloads[0].label, "original-label");
}

// ---------------------------------------------------------------------
// Admission: immediate decisions
// ---------------------------------------------------------------------

#[test]
fn light_command_is_admitted_immediately_regardless_of_pressure() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("light");
    let mut log = home.log();
    let snap = known_snapshot(Level::Critical, true, Mode::Enforce);
    let (connection, receiver, _cancelled) = connection();

    admission.handle(
        hook_request(AgentKind::Claude, "s1", Event::PreToolUse, Some("echo hi")),
        connection,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );

    let response = receiver
        .try_recv()
        .expect("a light command must decide immediately, never queue");
    assert_eq!(decision(response), HookDecision::Admit);
}

#[test]
fn heavy_command_admits_immediately_at_normal_pressure() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("normal");
    let mut log = home.log();
    let snap = known_snapshot(Level::Normal, true, Mode::Enforce);
    let (connection, receiver, _cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );

    let response = receiver
        .try_recv()
        .expect("Normal pressure must never hold");
    assert_eq!(decision(response), HookDecision::Admit);
}

#[test]
fn heavy_command_admits_immediately_when_no_batch_is_running_even_under_pressure() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("nobatch");
    let mut log = home.log();
    let snap = known_snapshot(Level::Elevated, false, Mode::Enforce);
    let (connection, receiver, _cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );

    let response = receiver
        .try_recv()
        .expect("no agent batch workload running must never hold");
    assert_eq!(decision(response), HookDecision::Admit);
}

#[test]
fn missing_or_discarded_pressure_sample_fails_open_instead_of_holding() {
    for snap in [
        snapshot(Level::Critical, true, Mode::Enforce),
        discarded_snapshot(Level::Critical, true, Mode::Enforce),
    ] {
        let mut admission = Admission::new(&[]);
        let mut state = HookState::default();
        let home = TestHome::new("unknownpressure");
        let mut log = home.log();
        let (connection, receiver, _cancelled) = connection();

        admission.handle(
            hook_request(
                AgentKind::Claude,
                "s1",
                Event::PreToolUse,
                Some("npm install"),
            ),
            connection,
            &snap,
            &mut state,
            &mut log,
            Instant::now(),
        );

        let response = receiver
            .try_recv()
            .expect("an unreliable pressure sample must fail open, not hold");
        assert_eq!(decision(response), HookDecision::Admit);
    }
}

#[test]
fn observe_mode_reaches_the_same_hold_decision_without_holding() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("observe");
    let mut log = home.log();
    let snap = known_snapshot(Level::Elevated, true, Mode::Observe);
    let (connection, receiver, _cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );

    let response = receiver
        .try_recv()
        .expect("observe mode must still answer immediately, never hold");
    assert_eq!(decision(response), HookDecision::Admit);
    assert!(
        home.decisions().contains("\"event\":\"admit\""),
        "observe mode must still record the decision it would have made: {}",
        home.decisions()
    );
}

// ---------------------------------------------------------------------
// Admission: the hold queue
// ---------------------------------------------------------------------

#[test]
fn heavy_command_holds_at_elevated_pressure_with_a_batch_running() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("hold");
    let mut log = home.log();
    let snap = known_snapshot(Level::Elevated, true, Mode::Enforce);
    let (connection, receiver, cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );

    let response = receiver
        .try_recv()
        .expect("a held request must get an immediate Hold acknowledgment");
    assert_eq!(decision(response), HookDecision::Hold);
    assert!(
        receiver.try_recv().is_err(),
        "no final decision until a later tick() admits the queued request"
    );
    assert!(
        home.decisions().contains("\"event\":\"hold\""),
        "hold must be recorded in decisions.jsonl: {}",
        home.decisions()
    );
    let held = admission.held();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].agent, "claude");
    assert_eq!(held[0].session_id, "s1");
    assert_eq!(held[0].label, "npm install");
    assert!(held[0].since_ms > 0);
    cancelled.store(true, Ordering::Relaxed);
    assert!(admission.held().is_empty());
}

/// Regression for review finding 1: a decision record used to carry the whole
/// `AttributionSnapshot`, so a snapshot with a thousand unattributed process rows blew a
/// single record past 189KB. The record must stay compact regardless of process count.
#[test]
fn hold_and_admit_records_stay_compact_with_a_thousand_process_rows() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("compact-log");
    let mut log = home.log();
    let mut snap = known_snapshot(Level::Elevated, true, Mode::Enforce);
    for pid in 1..=1000 {
        snap.attribution.processes.push(ProcessAttribution {
            identity: ProcessIdentity { pid, start_time: 1 },
            owner_id: None,
            agent_id: None,
            workload_id: None,
            role: ProcessRole::Unattributed,
            environment_known: false,
            listening_ports: None,
            ports_sampled_at_ms: None,
        });
    }

    let (held, held_receiver, _c1) = connection();
    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        held,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );
    assert_eq!(decision(held_receiver.recv().unwrap()), HookDecision::Hold);

    let (admitted, admitted_receiver, _c2) = connection();
    admission.handle(
        hook_request(AgentKind::Claude, "s2", Event::PreToolUse, Some("ls")),
        admitted,
        &snap,
        &mut state,
        &mut log,
        Instant::now(),
    );
    assert_eq!(
        decision(admitted_receiver.recv().unwrap()),
        HookDecision::Admit
    );

    let text = home.decisions();
    assert!(
        text.len() < 4096,
        "hold + admit records with 1,000 process rows in scope wrote {} bytes",
        text.len()
    );
    assert!(
        !text.contains("npm install"),
        "decision records must not echo the raw command: {text}"
    );

    let mut lines = text.lines();
    let hold: serde_json::Value = serde_json::from_str(lines.next().expect("hold record")).unwrap();
    let admit: serde_json::Value =
        serde_json::from_str(lines.next().expect("admit record")).unwrap();
    for record in [&hold, &admit] {
        let details = &record["details"];
        assert!(details.get("session_id").is_some(), "missing session_id");
        assert!(details.get("reason").is_some(), "missing reason");
        assert!(details.get("pressure").is_some(), "missing pressure");
        assert!(
            details.get("queue_position").is_some(),
            "missing queue_position"
        );
        assert!(
            details.get("attribution").is_none(),
            "must not carry the full attribution snapshot"
        );
    }
    assert_eq!(hold["details"]["queue_position"], serde_json::json!(1));
    assert_eq!(admit["details"]["queue_position"], serde_json::json!(0));
}

#[test]
fn tick_expires_max_hold_after_five_minutes_regardless_of_pressure() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("maxhold");
    let mut log = home.log();
    let start = Instant::now();
    let held = known_snapshot(Level::Critical, true, Mode::Enforce);
    let (connection, receiver, _cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &held,
        &mut state,
        &mut log,
        start,
    );
    receiver.try_recv().expect("hold ack");

    admission.tick(
        &held,
        &mut state,
        &mut log,
        start + Duration::from_secs(299),
    );
    assert!(
        receiver.try_recv().is_err(),
        "must not admit before the 300s hold ceiling, even though pressure never clears"
    );

    admission.tick(
        &held,
        &mut state,
        &mut log,
        start + Duration::from_secs(300),
    );
    let response = receiver
        .try_recv()
        .expect("must admit once the 300s max-hold ceiling is reached");
    assert_eq!(decision(response), HookDecision::Admit);
    assert!(home.decisions().contains("max hold"));
}

#[test]
fn tick_drops_a_cancelled_connection_without_sending_a_decision() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("cancel");
    let mut log = home.log();
    let now = Instant::now();
    let elevated = known_snapshot(Level::Elevated, true, Mode::Enforce);
    let (connection, receiver, cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &elevated,
        &mut state,
        &mut log,
        now,
    );
    receiver.try_recv().expect("hold ack");

    cancelled.store(true, Ordering::Relaxed);
    let normal = known_snapshot(Level::Normal, false, Mode::Enforce);
    admission.tick(&normal, &mut state, &mut log, now + Duration::from_secs(5));

    assert!(
        receiver.try_recv().is_err(),
        "a dropped connection must never receive a decision, even once admission is ready"
    );
}

#[test]
fn tick_admits_fifo_round_robin_across_agents_five_seconds_apart_at_normal_pressure() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("fifo");
    let mut log = home.log();
    let start = Instant::now();
    let elevated = known_snapshot(Level::Elevated, true, Mode::Enforce);

    let (claude_first, claude_receiver, _c1) = connection();
    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        claude_first,
        &elevated,
        &mut state,
        &mut log,
        start,
    );
    claude_receiver.try_recv().expect("hold ack");

    let (codex_first, codex_receiver, _c2) = connection();
    admission.handle(
        hook_request(
            AgentKind::Codex,
            "s2",
            Event::PreToolUse,
            Some("cargo build"),
        ),
        codex_first,
        &elevated,
        &mut state,
        &mut log,
        start,
    );
    codex_receiver.try_recv().expect("hold ack");

    // Pressure clears; the first tick admits Claude's (first-in-line) request and paces.
    let normal = known_snapshot(Level::Normal, false, Mode::Enforce);
    admission.tick(&normal, &mut state, &mut log, start);
    assert_eq!(
        decision(claude_receiver.try_recv().expect("claude admitted")),
        HookDecision::Admit
    );
    assert!(
        codex_receiver.try_recv().is_err(),
        "codex must wait its FIFO turn"
    );

    // Claude queues a second heavy request before the next release.
    let (claude_second, claude_receiver2, _c3) = connection();
    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        claude_second,
        &elevated,
        &mut state,
        &mut log,
        start + Duration::from_secs(1),
    );
    claude_receiver2.try_recv().expect("hold ack");

    // Under 5s since the last admit: no release yet, even though pressure is Normal.
    admission.tick(
        &normal,
        &mut state,
        &mut log,
        start + Duration::from_secs(2),
    );
    assert!(codex_receiver.try_recv().is_err());
    assert!(claude_receiver2.try_recv().is_err());

    // At 5s: codex is admitted next, not claude's second request -- round robin, not FIFO
    // within one agent.
    admission.tick(
        &normal,
        &mut state,
        &mut log,
        start + Duration::from_secs(5),
    );
    assert_eq!(
        decision(codex_receiver.try_recv().expect("codex admitted")),
        HookDecision::Admit
    );
    assert!(
        claude_receiver2.try_recv().is_err(),
        "claude's second request still waits its turn"
    );

    admission.tick(
        &normal,
        &mut state,
        &mut log,
        start + Duration::from_secs(10),
    );
    assert_eq!(
        decision(
            claude_receiver2
                .try_recv()
                .expect("claude's second request eventually admitted")
        ),
        HookDecision::Admit
    );
}

/// Regression for review finding 2: the no-batch release used to skip the five second
/// spacing entirely, so a `batch_running` signal that lagged one port round (up to 5s)
/// let every held command in every session drain within a handful of 250ms ticks. Pacing
/// must now apply to the no-batch release the same as any other, at every pressure level.
#[test]
fn tick_paces_no_batch_releases_five_seconds_apart_regardless_of_pressure_level() {
    for level in [Level::Elevated, Level::Critical] {
        let mut admission = Admission::new(&[]);
        let mut state = HookState::default();
        let home = TestHome::new(&format!("nopace-{level:?}"));
        let mut log = home.log();
        let start = Instant::now();
        let with_batch = known_snapshot(level, true, Mode::Enforce);

        let (claude, claude_receiver, _c1) = connection();
        admission.handle(
            hook_request(
                AgentKind::Claude,
                "s1",
                Event::PreToolUse,
                Some("npm install"),
            ),
            claude,
            &with_batch,
            &mut state,
            &mut log,
            start,
        );
        claude_receiver.try_recv().expect("hold ack");

        let (codex, codex_receiver, _c2) = connection();
        admission.handle(
            hook_request(
                AgentKind::Codex,
                "s2",
                Event::PreToolUse,
                Some("cargo build"),
            ),
            codex,
            &with_batch,
            &mut state,
            &mut log,
            start,
        );
        codex_receiver.try_recv().expect("hold ack");

        // The batch workload finishes; pressure stays at `level`. The no-batch release must
        // still pace at five seconds, not drain the queue on every 250ms tick.
        let no_batch = known_snapshot(level, false, Mode::Enforce);

        admission.tick(&no_batch, &mut state, &mut log, start);
        assert_eq!(
            decision(
                claude_receiver
                    .try_recv()
                    .expect("first release admits immediately")
            ),
            HookDecision::Admit,
            "at {level:?}"
        );

        for ms in [250, 500, 4999] {
            admission.tick(
                &no_batch,
                &mut state,
                &mut log,
                start + Duration::from_millis(ms),
            );
            assert!(
                codex_receiver.try_recv().is_err(),
                "no second release before the five second pacing window at {level:?}, {ms}ms in"
            );
        }

        admission.tick(
            &no_batch,
            &mut state,
            &mut log,
            start + Duration::from_secs(5),
        );
        assert_eq!(
            decision(
                codex_receiver
                    .try_recv()
                    .expect("second release admits once paced")
            ),
            HookDecision::Admit,
            "at {level:?}"
        );
    }
}

#[test]
fn tick_treats_unknown_pressure_as_release_ready() {
    let mut admission = Admission::new(&[]);
    let mut state = HookState::default();
    let home = TestHome::new("unknownready");
    let mut log = home.log();
    let start = Instant::now();
    let elevated = known_snapshot(Level::Elevated, true, Mode::Enforce);
    let (connection, receiver, _cancelled) = connection();

    admission.handle(
        hook_request(
            AgentKind::Claude,
            "s1",
            Event::PreToolUse,
            Some("npm install"),
        ),
        connection,
        &elevated,
        &mut state,
        &mut log,
        start,
    );
    receiver.try_recv().expect("hold ack");

    // The pressure sample goes missing entirely on the very next tick.
    let unknown = snapshot(Level::Elevated, true, Mode::Enforce);
    admission.tick(
        &unknown,
        &mut state,
        &mut log,
        start + Duration::from_millis(1),
    );

    let response = receiver.try_recv().expect(
        "an unknown pressure sample must be treated as release-ready, not as still-elevated",
    );
    assert_eq!(decision(response), HookDecision::Admit);
}

fn protection_snapshot() -> Snapshot {
    let mut snapshot = known_snapshot(Level::Elevated, true, Mode::Enforce);
    snapshot.attribution.agents = vec![
        attribution_agent("a", "claude", "s1", AgentState::Thinking),
        attribution_agent("b", "codex", "s2", AgentState::Thinking),
    ];
    snapshot.attribution.agents[0].root = Some(ProcessIdentity {
        pid: 11,
        start_time: 1,
    });
    snapshot.attribution.agents[1].root = Some(ProcessIdentity {
        pid: 12,
        start_time: 1,
    });
    for (pid, name, agent, ports) in [
        (11, "node", Some("a"), vec![3000]),
        (12, "node", Some("b"), vec![3001]),
        (13, "node", None, vec![3002]),
        (14, "node-helper", Some("a"), vec![]),
        (15, "renamed-server", Some("a"), vec![]),
    ] {
        let identity = ProcessIdentity { pid, start_time: 1 };
        snapshot.processes.push(crate::platform::Process {
            identity,
            ppid: 1,
            pgid: pid,
            uid: 1000,
            stopped: false,
            name: Some(name.into()),
            exe: Some("/bin/node".into()),
            argv: Some(vec![name.into(), "server-script".into()]),
            metrics: None,
        });
        snapshot.attribution.processes.push(ProcessAttribution {
            identity,
            owner_id: None,
            agent_id: agent.map(str::to_owned),
            workload_id: None,
            role: ProcessRole::Workload,
            environment_known: true,
            listening_ports: Some(ports),
            ports_sampled_at_ms: Some(1),
        });
    }
    snapshot
}

#[test]
fn kill_protection_literal_commands_and_fail_open_boundaries() {
    let snapshot = protection_snapshot();
    for (command, denied) in [
        ("kill 11", true),
        ("kill 12 13", false),
        ("kill 999", false),
        ("kill -TERM 11 12", true),
        ("kill -9 11", true),
        ("kill -s SIGINT 11", true),
        ("kill -- 11", true),
        ("kill -0 11", false),
        ("kill -CONT 11", false),
        ("kill -STOP 11", false),
        ("kill -USR1 11", false),
        ("kill -USR2 11", false),
        ("kill -WINCH 11", false),
        ("kill -INFO 11", false),
        ("kill -HUP 11", true),
        ("kill -QUIT 11", true),
        ("kill -ABRT 11", true),
        ("kill -15 11", true),
        ("kill -1 11", true),
        ("kill -2 11", true),
        ("kill -3 11", true),
        ("kill -6 11", true),
        ("kill -s CONT 11", false),
        ("kill -1", false),
        ("kill 0", false),
        ("kill -- -11", false),
        ("kill 11 $PID", false),
        ("kill $(pgrep node)", false),
        ("kill `pgrep node`", false),
        ("pkill node", true),
        ("pkill -x node", true),
        ("pkill -x node-helper", true),
        ("pkill -f server-script", true),
        ("pkill -fx 'node server-script'", true),
        ("pkill -x renamed-server", true),
        ("pkill -f 'server.*'", false),
        ("pkill '^node$'", false),
        ("pkill -u 1000 node", false),
        ("pkill $NAME", false),
        ("pkill -STOP node", false),
        ("pkill -TERM missing", false),
        ("killall node", true),
        ("killall nod", false),
        ("killall -9 node", true),
        ("killall -CONT node", false),
        ("killall node -STOP", false),
        ("pkill -term node", true),
        ("killall -f server-script", false),
        ("fuser -k 3000/tcp", cfg!(target_os = "linux")),
        ("fuser -k -n tcp 3000", cfg!(target_os = "linux")),
        ("fuser -k -TERM 3000/tcp", cfg!(target_os = "linux")),
        ("fuser -k -STOP 3000/tcp", false),
        ("fuser 3000/tcp", false),
        ("fuser -k /tmp/file", false),
        ("fuser -k 3000/udp", false),
        ("lsof -ti:3000 | xargs kill", true),
        ("lsof -t -i:3000 | xargs kill -9", true),
        ("lsof -ti:3000 | xargs -r kill -s TERM", true),
        ("lsof -ti:3000 | xargs kill -0", false),
        ("lsof -ti:3001 | xargs kill", false),
        ("lsof -ti:3002 | xargs kill", false),
        ("lsof -ti:$PORT | xargs kill", false),
        ("echo 11 | xargs kill", false),
        ("lsof -ti:3000; xargs kill", false),
        ("echo 'pkill node'", false),
        ("echo 'lsof -ti:3000 | xargs kill'", false),
        ("echo fine && pkill node", true),
        ("echo fine; kill 11", true),
        ("command /bin/kill 11", true),
        ("env FLAG=x pkill node", true),
        ("pkill 'no\\de'", false),
        ("pkill \"no\\de\"", false),
        ("cat <<EOF\npkill node\nEOF", false),
        ("sh -c 'pkill node'", false),
        ("kill 11 > $OUTPUT", false),
        ("# kill 11\necho fine", false),
        ("echo fine # comment\nkill 11", true),
        ("pkill no\\\nde", true),
    ] {
        let request = hook_request(AgentKind::Codex, "s2", Event::PreToolUse, Some(command));
        assert_eq!(
            super::protection::deny(&request, &snapshot, &test_evidence(&snapshot)).is_some(),
            denied,
            "{command}"
        );
    }
    let request = hook_request(
        AgentKind::Codex,
        "s2",
        Event::PreToolUse,
        Some("pkill node"),
    );
    let reason = super::protection::deny(&request, &snapshot, &test_evidence(&snapshot)).unwrap();
    assert!(
        reason.contains("2 processes belonging to 1 other agent."),
        "{reason}"
    );
    assert!(
        reason.contains("PIDs are 12; kill those directly"),
        "{reason}"
    );
    let mut stale = protection_snapshot();
    stale.attribution.processes[0].identity.start_time = 2;
    let request = hook_request(AgentKind::Codex, "s2", Event::PreToolUse, Some("kill 11"));
    assert!(super::protection::deny(&request, &stale, &test_evidence(&stale)).is_none());
}

#[test]
fn protection_precedes_holds_and_observe_only_logs() {
    for (mode, tag) in [
        (Mode::Enforce, "protect"),
        (Mode::Observe, "observe-protect"),
    ] {
        let mut snapshot = protection_snapshot();
        snapshot.status.mode = mode;
        let home = TestHome::new(tag);
        let mut log = home.log();
        let (mut connection, receive, _cancelled) = connection();
        connection.evidence = test_evidence(&snapshot);
        Admission::new(&[]).handle(
            hook_request(
                AgentKind::Codex,
                "s2",
                Event::PreToolUse,
                Some("pkill node; cargo build"),
            ),
            connection,
            &snapshot,
            &mut HookState::default(),
            &mut log,
            Instant::now(),
        );
        let result = decision(receive.try_recv().unwrap());
        assert_eq!(
            matches!(result, HookDecision::Deny { .. }),
            matches!(mode, Mode::Enforce)
        );
        let log = home.decisions();
        assert!(log.contains("\"event\":\"deny\""));
        assert!(!log.contains("pkill node"));
    }
}

#[test]
fn port_hints_on_both_agents_and_structured_outputs() {
    let snapshot = protection_snapshot();
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        for (output, expected) in [
            (
                serde_json::json!("listen EADDRINUSE: address already in use :::3000"),
                true,
            ),
            (
                serde_json::json!({"stderr": "address already in use 127.0.0.1:3000"}),
                true,
            ),
            (
                serde_json::json!({"output": [{"text": "Port 3000 is in use"}]}),
                true,
            ),
            (
                serde_json::json!({"stderr": "EADDRINUSE\n  port: 3000"}),
                true,
            ),
            (serde_json::json!("EADDRINUSE pid 3000"), false),
            (serde_json::json!("Port 5555 is in use"), false),
            (serde_json::json!("listening on port 3000"), false),
        ] {
            let mut request =
                hook_request(agent.clone(), "s2", Event::PostToolUse, Some("npm start"));
            request.tool_response = Some(output);
            let hint = super::protection::hint(&request, &snapshot, &test_evidence(&snapshot));
            assert_eq!(hint.is_some(), expected, "{:?}", request.tool_response);
            if let Some(hint) = hint {
                assert!(
                    hint.contains("Port 3000")
                        && hint.contains("PID 11")
                        && hint.contains("agent a"),
                    "{hint}"
                );
            }
        }
    }
    let home = TestHome::new("port-log");
    let mut request = hook_request(
        AgentKind::Codex,
        "s2",
        Event::PostToolUse,
        Some("npm start"),
    );
    request.tool_response = Some(serde_json::json!("Port 3000 is in use"));
    let (mut connection, receive, _cancelled) = connection();
    connection.evidence.ports = Some(test_port_owners(&snapshot));
    Admission::new(&[]).handle(
        request,
        connection,
        &snapshot,
        &mut HookState::default(),
        &mut home.log(),
        Instant::now(),
    );
    assert!(matches!(
        decision(receive.try_recv().unwrap()),
        HookDecision::Hint { .. }
    ));
    assert!(home.decisions().contains("\"event\":\"hint\""));
}

#[test]
fn cached_or_unknown_port_ownership_fails_open() {
    let snapshot = protection_snapshot();
    let mut request = hook_request(
        AgentKind::Codex,
        "s2",
        Event::PreToolUse,
        Some("lsof -ti:3000 | xargs kill"),
    );
    assert!(super::protection::deny(&request, &snapshot, &HookEvidence::default()).is_none());
    request.event = Event::PostToolUse;
    request.tool_response = Some(serde_json::json!("Port 3000 is in use"));
    assert!(super::protection::hint(&request, &snapshot, &HookEvidence::default()).is_none());
    assert!(
        super::protection::hint(
            &request,
            &snapshot,
            &HookEvidence {
                ports: Some(PortOwners::new()),
                ..Default::default()
            }
        )
        .is_none()
    );
}
fn test_port_owners(snapshot: &Snapshot) -> PortOwners {
    let mut owners = PortOwners::new();
    for p in &snapshot.attribution.processes {
        for &port in p.listening_ports.iter().flatten() {
            owners.entry(port).or_default().insert(p.identity);
        }
    }
    owners
}

fn test_evidence(snapshot: &Snapshot) -> HookEvidence {
    HookEvidence {
        caller: snapshot
            .attribution
            .agents
            .iter()
            .find(|a| a.id == "b")
            .and_then(|a| a.root),
        ports: Some(test_port_owners(snapshot)),
        arguments: snapshot
            .processes
            .iter()
            .filter_map(|p| Some((p.identity, p.argv.clone()?)))
            .collect(),
    }
}

#[test]
fn name_checks_require_fresh_arguments_from_other_agents() {
    let snapshot = protection_snapshot();
    let request = hook_request(
        AgentKind::Codex,
        "s2",
        Event::PreToolUse,
        Some("pkill node"),
    );
    assert!(super::protection::deny(&request, &snapshot, &HookEvidence::default()).is_none());
    assert!(super::protection::deny(&request, &snapshot, &test_evidence(&snapshot)).is_some());
}

#[test]
fn ended_agent_leftovers_are_allowed_and_hints_offer_stop() {
    let mut snapshot = protection_snapshot();
    snapshot.attribution.agents[0].state = AgentState::Ended;
    snapshot.attribution.processes[0].workload_id = Some("leftover".into());
    for command in ["kill 11", "pkill node", "lsof -ti:3000 | xargs kill"] {
        let request = hook_request(AgentKind::Codex, "s2", Event::PreToolUse, Some(command));
        assert!(super::protection::deny(&request, &snapshot, &test_evidence(&snapshot)).is_none());
        assert!(lookup(&request, &snapshot, None).arguments.is_empty());
    }
    let mut request = hook_request(
        AgentKind::Codex,
        "s2",
        Event::PostToolUse,
        Some("start server"),
    );
    request.tool_response = Some(serde_json::json!("Port 3000 is in use"));
    let hint = super::protection::hint(&request, &snapshot, &test_evidence(&snapshot)).unwrap();
    assert!(
        hint.contains("agent ended") && hint.contains("ballast stop leftover"),
        "{hint}"
    );
}

#[test]
fn socket_ancestry_survives_session_changes_and_unknown_callers_fail_open() {
    use crate::platform::{NativePlatform, Platform};
    use std::os::unix::net::UnixStream;
    let platform = NativePlatform::new().unwrap();
    let pid = std::process::id() as i32;
    let (_, parent_pid) = platform.process_parent(pid).unwrap();
    let (parent, _) = platform.process_parent(parent_pid).unwrap();
    let (peer, _client) = UnixStream::pair().unwrap();
    let peer = crate::daemon::ipc::peer_pid(&peer);
    assert_eq!(peer, Some(pid));
    let mut snapshot = protection_snapshot();
    snapshot.attribution.agents[1].root = Some(parent);
    let mut process = snapshot.processes[1].clone();
    process.identity = parent;
    snapshot.processes.push(process);
    let mut attribution = snapshot.attribution.processes[1].clone();
    attribution.identity = parent;
    snapshot.attribution.processes.push(attribution);
    assert!(!snapshot.processes.iter().any(|p| p.identity.pid == pid));
    for (kind, session) in [(AgentKind::Codex, "new-thread"), (AgentKind::Claude, "s1")] {
        for (command, denied) in [("kill 12", false), ("kill 11", true)] {
            let request = hook_request(kind.clone(), session, Event::PreToolUse, Some(command));
            let evidence = lookup(&request, &snapshot, peer);
            assert_eq!(evidence.caller, Some(parent));
            assert_eq!(
                super::protection::deny(&request, &snapshot, &evidence).is_some(),
                denied
            );
        }
    }
    let mut request = hook_request(AgentKind::Codex, "s2", Event::PreToolUse, Some("kill 11"));
    assert_eq!(lookup(&request, &snapshot, None).caller, Some(parent));
    request.session_id = "unknown".into();
    let unknown = lookup(&request, &snapshot, None);
    assert!(unknown.caller.is_none());
    assert!(super::protection::deny(&request, &snapshot, &unknown).is_none());
    // Readable, unattributed ancestry must not be overridden by a claimed known session.
    snapshot.attribution.processes.pop();
    request.session_id = "s2".into();
    assert!(lookup(&request, &snapshot, peer).caller.is_none());
}
