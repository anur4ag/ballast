use super::*;
use crate::attribution::{Agent, AttributionSnapshot, MemorySummary, ProcessAttribution, Workload};
use crate::daemon::{
    ProcessChanges, Status,
    files::{Config, Paths},
};
use crate::guardian::{FrozenWorkload, Level, Thresholds, recovery};
use crate::platform::{
    Capabilities, Environment, NativePlatform, PressureInputs, Process, ProcessMetrics,
};
use std::cell::RefCell;

#[derive(Default)]
struct Fake {
    signals: RefCell<Vec<(ProcessIdentity, String)>>,
    notifications: RefCell<Vec<String>>,
    gone: HashSet<ProcessIdentity>,
    unknown: HashSet<ProcessIdentity>,
    fail: RefCell<bool>,
    exit_during_signal: bool,
    environments: HashMap<ProcessIdentity, Environment>,
}
impl Platform for Fake {
    fn capabilities(&self) -> Capabilities {
        NativePlatform::new().unwrap().capabilities()
    }
    fn boot_id(&self) -> io::Result<String> {
        Ok("test".into())
    }
    fn list_processes(
        &mut self,
        _: &HashSet<ProcessIdentity>,
        _: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        Ok(Vec::new())
    }
    fn read_environment(&self, id: ProcessIdentity) -> Option<Environment> {
        self.environments.get(&id).cloned()
    }
    fn process_metrics(&self, _: ProcessIdentity) -> Option<ProcessMetrics> {
        None
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        Ok(PressureInputs::default())
    }
    fn listening_ports(&self, _: ProcessIdentity) -> Option<Vec<u16>> {
        Some(Vec::new())
    }
    fn process_liveness(&self, id: ProcessIdentity) -> ProcessLiveness {
        if self.gone.contains(&id) {
            ProcessLiveness::Gone
        } else if self.unknown.contains(&id) {
            ProcessLiveness::Unknown
        } else {
            ProcessLiveness::Alive
        }
    }
    fn send_signal(&self, id: ProcessIdentity, signal: Signal) -> io::Result<()> {
        if self.exit_during_signal {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        if *self.fail.borrow() {
            return Err(io::Error::other("unreadable identity"));
        }
        assert!(!self.gone.contains(&id), "must not signal a dead identity");
        self.signals.borrow_mut().push((id, format!("{signal:?}")));
        Ok(())
    }
    fn notify(&self, _: &str, body: &str) -> io::Result<bool> {
        self.notifications.borrow_mut().push(body.into());
        Ok(true)
    }
}
fn id(pid: i32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time: 123,
    }
}
fn snapshot() -> Snapshot {
    let attribution = AttributionSnapshot {
        agents: vec![Agent {
            id: "a".into(),
            root: Some(id(10)),
            state: AgentState::Ended,
            ended_at_ms: Some(1000),
            kind: "generic".into(),
            session_id: None,
            owner_id: None,
            cwd: None,
            memory: MemorySummary::default(),
        }],
        workloads: vec![
            ("batch", 20, WorkloadClass::Batch),
            ("service", 30, WorkloadClass::Service),
        ]
        .into_iter()
        .map(|(name, pid, class)| Workload {
            id: name.into(),
            agent_id: "a".into(),
            root: id(pid),
            class,
            label: name.into(),
            first_seen_ms: 0,
            detached_pgid: None,
            memory: MemorySummary::default(),
        })
        .collect(),
        processes: vec![
            (10, ProcessRole::AgentRoot, None),
            (20, ProcessRole::Workload, Some("batch")),
            (21, ProcessRole::Workload, Some("batch")),
            (30, ProcessRole::Workload, Some("service")),
            (40, ProcessRole::AgentInternal, None),
            (50, ProcessRole::Unattributed, None),
        ]
        .into_iter()
        .map(|(pid, role, workload)| ProcessAttribution {
            identity: id(pid),
            role,
            workload_id: workload.map(str::to_owned),
            agent_id: (pid != 50).then(|| "a".into()),
            owner_id: None,
            environment_known: true,
            listening_ports: Some(if pid == 30 { vec![8080] } else { vec![] }),
            ports_sampled_at_ms: Some(1000),
        })
        .collect(),
        ..AttributionSnapshot::default()
    };
    Snapshot {
        processes: attribution
            .processes
            .iter()
            .map(|p| Process {
                identity: p.identity,
                ppid: 1,
                pgid: p.identity.pid,
                uid: unsafe { libc::geteuid() },
                stopped: false,
                name: None,
                exe: Some("/bin/node".into()),
                argv: None,
                metrics: None,
            })
            .collect(),
        attribution,
        status: Status {
            daemon_version: "test".into(),
            pid: 1,
            mode: Mode::Enforce,
            tick: 1,
            sampled_at_ms: 31000,
            tick_interval_ms: 1000,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded: false,
            process_count: 6,
            pressure_level: Level::Normal,
            batch_running: true,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: "test".into(),
        capabilities: Fake::default().capabilities(),
        changes: ProcessChanges::default(),
        pressure: None,
        frozen: vec![],
        held: Vec::new(),
        guardian: None,
        today: Default::default(),
    }
}
struct Harness {
    paths: Paths,
    cleanup: Cleanup,
    guardian: Guardian,
    log: RotatingLog,
}
impl Harness {
    fn new(mode: Mode) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let paths = Paths {
            base: std::env::temp_dir().join(format!(
                "blt-clean-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )),
        };
        paths.prepare().unwrap();
        let log =
            RotatingLog::open(paths.base.join("log/decisions.jsonl"), &Config::default()).unwrap();
        Self {
            cleanup: Cleanup::new(mode, Duration::from_secs(30)),
            guardian: Guardian::new(paths.clone(), "test".into(), mode, Thresholds::default()),
            paths,
            log,
        }
    }
    fn tick(&mut self, now: Instant, snapshot: &Snapshot, p: &impl Platform) {
        self.cleanup
            .tick(now, snapshot, &mut self.guardian, p, &mut self.log);
    }
    fn request(&mut self, target: Option<&str>, snapshot: &Snapshot, p: &impl Platform) -> Report {
        self.cleanup
            .request(target, snapshot, &mut self.guardian, p, &mut self.log)
            .unwrap()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.paths.base);
    }
}

#[test]
fn grace_reclaims_batch_and_internal_but_reports_service_once() {
    let mut h = Harness::new(Mode::Enforce);
    let mut s = snapshot();
    let mut p = Fake::default();
    let now = Instant::now();
    s.status.sampled_at_ms = 30999;
    h.tick(now, &s, &p);
    assert!(p.signals.borrow().is_empty());
    s.status.sampled_at_ms = 31000;
    h.tick(now, &s, &p);
    let mut signalled: Vec<_> = p.signals.borrow().iter().map(|(id, _)| id.pid).collect();
    signalled.sort_unstable();
    assert_eq!(signalled, [20, 21, 40]);
    assert!(p.notifications.borrow()[0].contains("8080"));
    assert!(p.notifications.borrow()[0].contains("ballast stop service"));
    h.tick(now + Duration::from_secs(4), &s, &p);
    assert_eq!(p.signals.borrow().len(), 3);
    p.gone.extend([id(20), id(21), id(40)]);
    s.attribution
        .processes
        .retain(|a| !p.gone.contains(&a.identity));
    h.tick(now + Duration::from_secs(5), &s, &p);
    assert_eq!(
        p.notifications
            .borrow()
            .iter()
            .filter(|n| n.contains("survives"))
            .count(),
        1
    );
    assert!(
        p.notifications
            .borrow()
            .iter()
            .any(|n| n.contains("Reclaimed"))
    );
    let decisions = std::fs::read_to_string(h.paths.base.join("log/decisions.jsonl")).unwrap();
    let service_events: Vec<serde_json::Value> = decisions
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|v| v["event"] == "service_reported")
        .collect();
    assert_eq!(service_events.len(), 1);
    assert_eq!(service_events[0]["details"]["decision"]["count"], 1);
}

#[test]
fn gc_bypasses_grace_lists_orphans_and_never_touches_live_agents() {
    let mut h = Harness::new(Mode::Enforce);
    let mut s = snapshot();
    let p = Fake::default();
    s.status.sampled_at_ms = 1001;
    let report = h.request(None, &s, &p);
    assert_eq!(report.services[0].pids, [30]);
    assert_eq!(report.orphans[0].identity, id(50));
    assert!(
        p.signals
            .borrow()
            .iter()
            .all(|(id, _)| [20, 21, 40].contains(&id.pid))
    );
    let p = Fake::default();
    let mut h = Harness::new(Mode::Enforce);
    s.attribution.agents[0].state = AgentState::Unknown;
    assert!(h.request(None, &s, &p).scheduled.is_empty());
    assert!(p.signals.borrow().is_empty());
}

#[test]
fn stop_includes_services_excludes_roots_and_internals_and_escalates_survivors() {
    let mut h = Harness::new(Mode::Enforce);
    let mut s = snapshot();
    let mut p = Fake::default();
    s.attribution.agents[0].state = AgentState::Thinking;
    assert_eq!(h.request(Some("a"), &s, &p).scheduled.len(), 2);
    assert_eq!(p.signals.borrow().len(), 3);
    assert!(
        p.signals
            .borrow()
            .iter()
            .all(|(id, signal)| [20, 21, 30].contains(&id.pid) && signal == "Terminate")
    );
    p.gone.insert(id(21));
    s.attribution.processes.retain(|a| a.identity != id(21));
    h.tick(Instant::now() + Duration::from_secs(5), &s, &p);
    let mut killed: Vec<_> = p
        .signals
        .borrow()
        .iter()
        .filter(|(_, sig)| sig == "Kill")
        .map(|(id, _)| id.pid)
        .collect();
    killed.sort_unstable();
    assert_eq!(killed, [20, 30]);
}

#[test]
fn escalation_rechecks_class_and_root_role_and_observe_never_signals() {
    for change_root in [false, true] {
        let mut h = Harness::new(Mode::Enforce);
        let mut s = snapshot();
        let p = Fake::default();
        h.tick(Instant::now(), &s, &p);
        p.signals.borrow_mut().clear();
        if change_root {
            s.attribution.agents[0].state = AgentState::Thinking;
        } else {
            s.attribution.workloads[0].class = WorkloadClass::Service;
        }
        h.tick(Instant::now() + Duration::from_secs(6), &s, &p);
        assert!(p.signals.borrow().iter().all(|(id, _)| id.pid == 40));
    }
    let mut h = Harness::new(Mode::Observe);
    let s = snapshot();
    let p = Fake::default();
    assert!(h.request(Some("a"), &s, &p).observe);
    h.tick(Instant::now() + Duration::from_secs(6), &s, &p);
    assert!(p.signals.borrow().is_empty());
    assert!(p.notifications.borrow().is_empty());
    let log = std::fs::read_to_string(h.paths.base.join("log/decisions.jsonl")).unwrap();
    assert!(log.contains("Terminate") && log.contains("Kill") && log.contains("observe"));
}

#[test]
fn failed_manual_signals_retry_and_late_children_get_their_own_grace() {
    let mut h = Harness::new(Mode::Enforce);
    let mut s = snapshot();
    let p = Fake::default();
    s.attribution.agents[0].state = AgentState::Thinking;
    *p.fail.borrow_mut() = true;
    h.request(Some("batch"), &s, &p);
    assert!(!h.cleanup.take_errors().is_empty());
    *p.fail.borrow_mut() = false;
    let now = Instant::now();
    h.tick(now, &s, &p);
    assert_eq!(p.signals.borrow().len(), 2);
    let mut late = s.attribution.processes[1].clone();
    late.identity = id(22);
    s.attribution.processes.push(late);
    h.tick(now + Duration::from_secs(5), &s, &p);
    assert!(p.signals.borrow().contains(&(id(22), "Terminate".into())));
    assert!(!p.signals.borrow().contains(&(id(22), "Kill".into())));
    h.tick(now + Duration::from_secs(10), &s, &p);
    assert!(p.signals.borrow().contains(&(id(22), "Kill".into())));
}

#[test]
fn frozen_workload_resumes_and_clears_journal_before_term() {
    let mut h = Harness::new(Mode::Enforce);
    let s = snapshot();
    let p = Fake::default();
    h.guardian.frozen.push(serde_json::from_value::<FrozenWorkload>(serde_json::json!({"workload_id":"batch", "root":id(20), "processes":[id(20),id(21)], "frozen_at_ms":1000})).unwrap());
    recovery::write(&h.paths, "test", &h.guardian.frozen).unwrap();
    h.request(Some("batch"), &s, &p);
    assert_eq!(
        &p.signals.borrow()[..2],
        [(id(20), "Continue".into()), (id(21), "Continue".into())]
    );
    assert!(h.guardian.frozen.is_empty());
    assert!(recovery::read(&h.paths).unwrap().workloads.is_empty());
}

#[test]
#[ignore]
#[allow(clippy::zombie_processes)] // This fixture intentionally leaves children for cleanup.
fn native_root_fixture() {
    use std::process::{Command, Stdio};
    let Ok(dir) = std::env::var("BALLAST_CLEANUP_TEST_DIR") else {
        return;
    };
    // Detached background tools survive the shell session's hangup.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    // Keep batch membership stable across discovery and freeze; child churn can leave
    // an unreadable member provisionally classified as a service.
    let fifo = std::ffi::CString::new(format!("{dir}/batch-wait")).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut batch = Command::new("/bin/sh");
    batch.args(["-c", "exec 3<> \"$BALLAST_CLEANUP_TEST_DIR/batch-wait\"; trap 'printf done > \"$BALLAST_CLEANUP_TEST_DIR/handled\"; exit 0' TERM; printf ready > \"$BALLAST_CLEANUP_TEST_DIR/batch-ready\"; read -r line <&3"]);
    let mut service = Command::new("/bin/sh");
    service.args(["-c", "\"$BALLAST_CLEANUP_TEST_EXE\" cleanup::tests::native_listener_fixture --exact --ignored --nocapture & wait"]);
    for command in [&mut batch, &mut service] {
        // Leave the children behind when this stand-in session exits.
        let _child = command
            .env("CLAUDE_PID", std::process::id().to_string())
            .env("CLAUDE_CODE_SESSION_ID", "cleanup-native-test")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
    }
    std::fs::write(std::path::Path::new(&dir).join("root-ready"), b"ready").unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}

#[test]
#[ignore]
fn native_listener_fixture() {
    let Ok(dir) = std::env::var("BALLAST_CLEANUP_TEST_DIR") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        std::path::Path::new(&dir).join("port"),
        listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(60));
    drop(listener);
}

#[test]
fn native_ended_session_thaws_build_handler_and_preserves_then_stops_server() {
    use crate::attribution::Attributor;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut h = Harness::new(Mode::Enforce);
    h.cleanup = Cleanup::new(Mode::Enforce, Duration::from_millis(300));
    let exe = std::env::current_exe().unwrap();
    let root_exe = h.paths.base.join("claude");
    std::fs::copy(&exe, &root_exe).unwrap();
    let root = Command::new(root_exe)
        .args([
            "cleanup::tests::native_root_fixture",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(
            "BALLAST_OWNER",
            format!("cleanup-test-{}", std::process::id()),
        )
        .env("BALLAST_CLEANUP_TEST_DIR", &h.paths.base)
        .env("BALLAST_CLEANUP_TEST_EXE", exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    struct Group(std::process::Child);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
    let mut group = Group(root);
    let mut platform = NativePlatform::new().unwrap();
    platform.notifications = false;
    let mut attributor = Attributor::new(vec![], vec![]);
    let mut s = snapshot();
    let root_pid = group.0.id() as i32;
    let mut observe = |s: &mut Snapshot, platform: &mut NativePlatform| {
        let watched = attributor.watched(&HashSet::new());
        let mut processes = platform
            .list_processes(&watched, &attributor.metric_targets())
            .unwrap();
        processes.retain(|p| p.pgid == root_pid);
        s.status.sampled_at_ms = crate::daemon::unix_ms();
        s.attribution = attributor.update(
            platform,
            &mut processes,
            Instant::now(),
            s.status.sampled_at_ms,
            false,
        );
        s.processes = processes;
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        observe(&mut s, &mut platform);
        if s.attribution.workloads.len() == 2
            && s.attribution
                .workloads
                .iter()
                .any(|w| w.class == WorkloadClass::Batch)
            && s.attribution
                .processes
                .iter()
                .any(|p| p.listening_ports.as_ref().is_some_and(|v| !v.is_empty()))
            && h.paths.base.join("batch-ready").exists()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stand-in attribution did not converge: {:?}",
            s.attribution
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let batch = s
        .attribution
        .workloads
        .iter()
        .find(|w| w.class == WorkloadClass::Batch)
        .unwrap()
        .clone();
    let service = s
        .attribution
        .workloads
        .iter()
        .find(|w| w.class == WorkloadClass::Service)
        .unwrap()
        .id
        .clone();
    let mut members: Vec<_> = s
        .attribution
        .processes
        .iter()
        .filter(|p| p.workload_id.as_ref() == Some(&batch.id))
        .map(|p| p.identity)
        .collect();
    members.sort_unstable_by_key(|id| (*id != batch.root, *id));
    h.guardian.frozen.push(serde_json::from_value(serde_json::json!({"workload_id": batch.id, "root": batch.root, "processes": members, "frozen_at_ms": crate::daemon::unix_ms()})).unwrap());
    recovery::write(&h.paths, "test", &h.guardian.frozen).unwrap();
    for id in members {
        match platform.send_signal(id, Signal::Stop) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => panic!("stop fixture: {e}"),
        }
    }
    // Finish the agent, leaving both tool-call workloads behind.
    drop(group.0.stdin.take());
    group.0.wait().unwrap();
    // Orphaning a stopped process group can send SIGCONT on POSIX.
    // Stop it again after reparenting so cleanup must thaw a genuinely frozen tree.
    for id in h.guardian.frozen[0].processes.clone() {
        let _ = platform.send_signal(id, Signal::Stop);
    }
    observe(&mut s, &mut platform);
    assert!(
        s.processes
            .iter()
            .any(|p| p.identity == batch.root && p.stopped)
    );
    assert!(
        s.attribution
            .agents
            .iter()
            .all(|a| a.state == AgentState::Ended)
    );
    h.tick(Instant::now(), &s, &platform);
    assert!(
        !h.paths.base.join("handled").exists(),
        "grace must elapse first"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !h.paths.base.join("handled").exists() {
        observe(&mut s, &mut platform);
        h.tick(Instant::now(), &s, &platform);
        assert!(
            Instant::now() < deadline,
            "SIGTERM handler never ran; attribution={:?}; errors={:?}; log={}",
            s.attribution,
            h.cleanup.take_errors(),
            std::fs::read_to_string(h.paths.base.join("log/decisions.jsonl")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(h.guardian.frozen.is_empty());
    assert!(recovery::read(&h.paths).unwrap().workloads.is_empty());
    let port: u16 = std::fs::read_to_string(h.paths.base.join("port"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "service must survive automatic cleanup"
    );
    observe(&mut s, &mut platform);
    let report = h.request(None, &s, &platform);
    assert!(
        report
            .services
            .iter()
            .any(|w| w.workload_id == service && w.ports.contains(&port))
    );
    h.request(Some(&service), &s, &platform);
    let deadline = Instant::now() + Duration::from_secs(8);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
        observe(&mut s, &mut platform);
        h.tick(Instant::now(), &s, &platform);
        assert!(
            Instant::now() < deadline,
            "explicit stop did not stop server"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let errors = h.cleanup.take_errors();
    assert!(errors.is_empty(), "cleanup errors: {errors:?}");
}

#[test]
fn failed_manual_term_or_thaw_survives_an_unreadable_scan() {
    for frozen in [false, true] {
        let mut h = Harness::new(Mode::Enforce);
        let mut s = snapshot();
        s.attribution.agents[0].state = AgentState::Thinking;
        s.attribution.processes.retain(|p| p.identity != id(21));
        if frozen {
            h.guardian.frozen.push(serde_json::from_value(serde_json::json!({"workload_id":"batch", "root":id(20), "processes":[id(20)], "frozen_at_ms":1000})).unwrap());
        }
        let mut p = Fake::default();
        *p.fail.borrow_mut() = true;
        h.request(Some("batch"), &s, &p);
        assert!(p.signals.borrow().is_empty());
        let member = s
            .attribution
            .processes
            .iter()
            .find(|p| p.identity == id(20))
            .unwrap()
            .clone();
        s.attribution.processes.retain(|p| p.identity != id(20));
        p.unknown.insert(id(20));
        let now = Instant::now();
        h.tick(now + Duration::from_secs(1), &s, &p);
        assert!(
            h.cleanup.watched().contains(&id(20)),
            "failed stop intent must remain watched through an unknown scan"
        );
        p.unknown.clear();
        *p.fail.borrow_mut() = false;
        s.attribution.processes.push(member);
        h.tick(now + Duration::from_secs(2), &s, &p);
        assert!(p.signals.borrow().contains(&(id(20), "Terminate".into())));
        h.tick(now + Duration::from_secs(6), &s, &p);
        assert!(!p.signals.borrow().contains(&(id(20), "Kill".into())));
        h.tick(now + Duration::from_secs(7), &s, &p);
        assert!(p.signals.borrow().contains(&(id(20), "Kill".into())));
    }
}

fn stop_tree() -> Vec<Process> {
    [
        (10, 1, "/usr/bin/claude", vec!["claude"]),
        (20, 10, "/bin/sh", vec!["sh", "-c", "cargo build"]),
        (21, 20, "/usr/bin/rustc", vec!["rustc"]),
    ]
    .into_iter()
    .map(|(pid, ppid, exe, argv)| Process {
        identity: id(pid),
        ppid,
        pgid: 20,
        uid: unsafe { libc::geteuid() },
        stopped: false,
        name: None,
        exe: Some(exe.into()),
        argv: Some(argv.into_iter().map(str::to_owned).collect()),
        metrics: None,
    })
    .collect()
}

fn update_stop_tree(
    a: &mut crate::attribution::Attributor,
    p: &Fake,
    mut rows: Vec<Process>,
    now: Instant,
    wall_ms: u64,
) -> Snapshot {
    let mut s = snapshot();
    s.attribution = a.update(p, &mut rows, now, wall_ms, false);
    s.processes = rows;
    s.status.sampled_at_ms = wall_ms;
    s
}

#[test]
fn stop_requested_during_omitted_scan_is_not_lost() {
    let mut a = crate::attribution::Attributor::new(vec![], vec![]);
    let mut h = Harness::new(Mode::Enforce);
    let mut p = Fake::default();
    let now = Instant::now();
    let s = update_stop_tree(&mut a, &p, stop_tree(), now, 1000);
    assert_eq!(s.attribution.workloads.len(), 1);
    let workload = s.attribution.workloads[0].id.clone();
    p.unknown.extend([id(20), id(21)]);
    let s = update_stop_tree(
        &mut a,
        &p,
        vec![stop_tree()[0].clone()],
        now + Duration::from_secs(1),
        2000,
    );
    assert!(s.attribution.workloads.iter().any(|w| w.id == workload));
    assert!(
        s.attribution
            .processes
            .iter()
            .all(|p| p.role != ProcessRole::Workload)
    );
    let report = h.request(Some(&workload), &s, &p);
    assert_eq!(report.scheduled, std::slice::from_ref(&workload));
    assert_eq!(report.pending, std::slice::from_ref(&workload));
    assert_eq!(
        h.request(None, &s, &p).pending,
        std::slice::from_ref(&workload)
    );
    assert!(p.signals.borrow().is_empty());
    p.unknown.clear();
    let s = update_stop_tree(&mut a, &p, stop_tree(), now + Duration::from_secs(2), 3000);
    assert!(
        s.attribution
            .agents
            .iter()
            .all(|a| a.state != AgentState::Ended)
    );
    h.tick(now + Duration::from_secs(2), &s, &p);
    assert!(
        p.signals.borrow().contains(&(id(20), "Terminate".into())),
        "accepted stop was lost: {:?}",
        p.signals.borrow()
    );
}

#[test]
fn stop_survives_readable_identity_with_unknown_executable() {
    let mut a = crate::attribution::Attributor::new(vec![], vec![]);
    let mut h = Harness::new(Mode::Enforce);
    let mut p = Fake::default();
    p.environments
        .insert(id(20), [("CODEX_THREAD_ID".into(), "review".into())].into());
    let now = Instant::now();
    let mut rows = stop_tree()[..2].to_vec();
    rows[0].exe = Some("/usr/bin/codex".into());
    rows[0].argv = Some(vec!["codex".into()]);
    let s = update_stop_tree(&mut a, &p, rows.clone(), now, 1000);
    assert_eq!(s.attribution.workloads.len(), 1);
    let workload = s.attribution.workloads[0].id.clone();
    h.request(Some(&workload), &s, &p);
    assert!(p.signals.borrow().contains(&(id(20), "Terminate".into())));
    let mut unreadable = rows.clone();
    unreadable[1].exe = None;
    unreadable[1].argv = None;
    let s = update_stop_tree(&mut a, &p, unreadable, now + Duration::from_secs(1), 2000);
    assert_eq!(p.process_liveness(id(20)), ProcessLiveness::Alive);
    assert!(
        s.attribution
            .processes
            .iter()
            .any(|p| p.identity == id(20) && p.role == ProcessRole::AgentInternal)
    );
    h.tick(now + Duration::from_secs(1), &s, &p);
    assert_eq!(h.cleanup.pending_targets(), std::slice::from_ref(&workload));
    // TERM already authorized this exact identity; changing membership cannot erase escalation.
    h.tick(now + Duration::from_secs(6), &s, &p);
    assert!(
        p.signals.borrow().contains(&(id(20), "Kill".into())),
        "accepted stop escalation was lost: {:?}; agents: {:?}",
        p.signals.borrow(),
        s.attribution.agents
    );
    let s = update_stop_tree(&mut a, &p, rows, now + Duration::from_secs(7), 8000);
    h.tick(now + Duration::from_secs(7), &s, &p);
    assert_eq!(
        p.signals
            .borrow()
            .iter()
            .filter(|(pid, sig)| *pid == id(20) && sig == "Kill")
            .count(),
        1
    );
    p.gone.insert(id(20));
    let s = update_stop_tree(
        &mut a,
        &p,
        vec![stop_tree()[0].clone()],
        now + Duration::from_secs(8),
        9000,
    );
    h.tick(now + Duration::from_secs(8), &s, &p);
    assert!(h.cleanup.pending_targets().is_empty());
}

#[test]
fn exit_between_observation_and_signal_is_not_a_cleanup_error() {
    let mut h = Harness::new(Mode::Enforce);
    let mut s = snapshot();
    s.attribution.agents[0].state = AgentState::Thinking;
    let p = Fake {
        exit_during_signal: true,
        ..Fake::default()
    };
    h.request(Some("batch"), &s, &p);
    assert!(p.signals.borrow().is_empty());
    let errors = h.cleanup.take_errors();
    assert!(
        errors.is_empty(),
        "confirmed signal-time exits are not errors: {errors:?}"
    );
}
