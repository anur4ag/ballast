//! Unit tests for `Attributor` (ticket 04), against the real algorithm in `super`.
//!
//! Attribution is a pure function of a `Vec<Process>` plus a `Platform`, so every case here
//! uses `FakePlatform` (in-memory, exact control over env/metrics/ports/age/cwd and call
//! counts) and hand-built `Process` trees, with synthetic `Instant`s so the 30s growth window
//! and slow cadences never need real sleeping. One real spawned OS process tree (native
//! coverage) and the fleet benchmark live in `tests/attribution_fleet.rs` instead.

use super::*;
use crate::platform::{
    Capabilities, Environment, Platform, PressureInputs, Process, ProcessIdentity, ProcessLiveness,
    ProcessMetrics, Signal,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn id(pid: i32, start_time: u64) -> ProcessIdentity {
    ProcessIdentity { pid, start_time }
}
fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn proc(identity: ProcessIdentity, ppid: i32, pgid: i32, exe: &str, argv: &[&str]) -> Process {
    proc_uid(identity, ppid, pgid, exe, argv, own_uid())
}
fn proc_uid(
    identity: ProcessIdentity,
    ppid: i32,
    pgid: i32,
    exe: &str,
    argv: &[&str],
    uid: u32,
) -> Process {
    Process {
        identity,
        ppid,
        pgid,
        uid,
        stopped: false,
        name: None,
        exe: Some(exe.into()),
        argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        metrics: None,
    }
}
/// A process whose identity is enumerated (present) but whose own executable/argv read failed
/// (e.g. a transient permission or race), as opposed to an absent PID.
fn proc_unreadable(identity: ProcessIdentity, ppid: i32, pgid: i32) -> Process {
    Process {
        identity,
        ppid,
        pgid,
        uid: own_uid(),
        stopped: false,
        name: None,
        exe: None,
        argv: None,
        metrics: None,
    }
}

#[derive(Default)]
struct FakePlatform {
    environments: HashMap<ProcessIdentity, Option<Environment>>,
    env_reads: RefCell<HashMap<ProcessIdentity, u32>>,
    metrics: HashMap<ProcessIdentity, ProcessMetrics>,
    ports: HashMap<ProcessIdentity, Option<Vec<u16>>>,
    port_reads: RefCell<HashMap<ProcessIdentity, u32>>,
    ages: HashMap<ProcessIdentity, Duration>,
    age_reads: RefCell<HashMap<ProcessIdentity, u32>>,
    cwds: HashMap<ProcessIdentity, PathBuf>,
    liveness: HashMap<ProcessIdentity, ProcessLiveness>,
    /// `Some(false)` (the default for a PID not listed here) means "confirmed absent from the
    /// last enumeration", matching an ordinary fixture's process table. Override with
    /// `pid_presence` for a test that specifically wants uncertainty (`None`) or a present-but-
    /// unreadable PID (`Some(true)`).
    presence: HashMap<i32, Option<bool>>,
}
impl FakePlatform {
    fn env(mut self, id: ProcessIdentity, env: &[(&str, &str)]) -> Self {
        self.environments.insert(
            id,
            Some(
                env.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        );
        self
    }
    fn unknown_env(mut self, id: ProcessIdentity) -> Self {
        self.environments.insert(id, None);
        self
    }
    fn metrics(mut self, id: ProcessIdentity, memory_bytes: u64) -> Self {
        self.metrics.insert(
            id,
            ProcessMetrics {
                memory_bytes,
                cpu_time_ns: 0,
            },
        );
        self
    }
    fn ports(mut self, id: ProcessIdentity, ports: &[u16]) -> Self {
        self.ports.insert(id, Some(ports.to_vec()));
        self
    }
    /// A sample that fails this tick (e.g. transient permission denial): `listening_ports`
    /// returns `None`, so any previously-sampled value for this identity must be kept as-is.
    fn unknown_ports(mut self, id: ProcessIdentity) -> Self {
        self.ports.insert(id, None);
        self
    }
    fn age(mut self, id: ProcessIdentity, age: Duration) -> Self {
        self.ages.insert(id, age);
        self
    }
    fn cwd(mut self, id: ProcessIdentity, cwd: &str) -> Self {
        self.cwds.insert(id, PathBuf::from(cwd));
        self
    }
    /// A root confirmed exited by the platform's own liveness evidence (e.g. its PID is absent
    /// from the latest enumeration, or reappeared under a different identity).
    fn gone(mut self, id: ProcessIdentity) -> Self {
        self.liveness.insert(id, ProcessLiveness::Gone);
        self
    }
    /// Overrides the default confirmed-absent presence of a referenced (not necessarily
    /// enumerated) PID: `None` for uncertain enumeration, `Some(true)` for present.
    fn pid_presence(mut self, pid: i32, value: Option<bool>) -> Self {
        self.presence.insert(pid, value);
        self
    }
    fn env_read_count(&self, id: ProcessIdentity) -> u32 {
        self.env_reads.borrow().get(&id).copied().unwrap_or(0)
    }
    fn port_read_count(&self, id: ProcessIdentity) -> u32 {
        self.port_reads.borrow().get(&id).copied().unwrap_or(0)
    }
    fn age_read_count(&self, id: ProcessIdentity) -> u32 {
        self.age_reads.borrow().get(&id).copied().unwrap_or(0)
    }
}
impl Platform for FakePlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            environment: true,
            listening_ports: true,
            memory_footprint: true,
            memory_psi: false,
            kernel_pressure: false,
            notifications: false,
            atomic_signals: true,
        }
    }
    fn boot_id(&self) -> io::Result<String> {
        Ok("fake-boot".into())
    }
    fn list_processes(
        &mut self,
        _watched: &HashSet<ProcessIdentity>,
        _metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        // Tests drive `Attributor::update` with an explicit process list instead;
        // the trait still requires this method.
        Ok(Vec::new())
    }
    fn read_environment(&self, process: ProcessIdentity) -> Option<Environment> {
        *self.env_reads.borrow_mut().entry(process).or_default() += 1;
        self.environments.get(&process).cloned().flatten()
    }
    fn process_metrics(&self, process: ProcessIdentity) -> Option<ProcessMetrics> {
        self.metrics.get(&process).copied()
    }
    fn process_age(&self, process: ProcessIdentity) -> Option<Duration> {
        *self.age_reads.borrow_mut().entry(process).or_default() += 1;
        self.ages.get(&process).copied()
    }
    fn process_liveness(&self, process: ProcessIdentity) -> ProcessLiveness {
        self.liveness
            .get(&process)
            .copied()
            .unwrap_or(ProcessLiveness::Unknown)
    }
    fn pid_is_present(&self, pid: i32) -> Option<bool> {
        self.presence.get(&pid).copied().unwrap_or(Some(false))
    }
    fn process_cwd(&self, process: ProcessIdentity) -> Option<PathBuf> {
        self.cwds.get(&process).cloned()
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        Ok(PressureInputs::default())
    }
    fn listening_ports(&self, process: ProcessIdentity) -> Option<Vec<u16>> {
        *self.port_reads.borrow_mut().entry(process).or_default() += 1;
        self.ports
            .get(&process)
            .cloned()
            .unwrap_or_else(|| Some(Vec::new()))
    }
    fn send_signal(&self, _process: ProcessIdentity, _signal: Signal) -> io::Result<()> {
        Ok(())
    }
    fn notify(&self, _title: &str, _body: &str) -> io::Result<bool> {
        Ok(true)
    }
}

fn attr(snapshot: &AttributionSnapshot, id: ProcessIdentity) -> &ProcessAttribution {
    snapshot
        .processes
        .iter()
        .find(|p| p.identity == id)
        .unwrap_or_else(|| panic!("no attribution recorded for {id:?}"))
}
fn agent_of<'a>(snapshot: &'a AttributionSnapshot, agent_id: &str) -> &'a Agent {
    snapshot
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .unwrap_or_else(|| panic!("no agent {agent_id} in snapshot"))
}
fn workload_of<'a>(snapshot: &'a AttributionSnapshot, workload_id: &str) -> &'a Workload {
    snapshot
        .workloads
        .iter()
        .find(|w| w.id == workload_id)
        .unwrap_or_else(|| panic!("no workload {workload_id} in snapshot"))
}

const CLAUDE: &str = "/usr/bin/claude";
const CODEX: &str = "/usr/bin/codex";

// ---------------------------------------------------------------------
// Marker attribution and inheritance.
// ---------------------------------------------------------------------

#[test]
fn claude_root_is_attributed_via_the_session_and_root_pid_markers() {
    let root = id(100, 1);
    let platform = FakePlatform::default().env(
        root,
        &[("CLAUDE_CODE_SESSION_ID", "sess-1"), ("CLAUDE_PID", "100")],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(root, 1, 100, CLAUDE, &[])],
        Instant::now(),
        0,
        false,
    );

    let attribution = attr(&snapshot, root);
    assert_eq!(attribution.role, ProcessRole::AgentRoot);
    let agent = agent_of(&snapshot, attribution.agent_id.as_deref().unwrap());
    assert_eq!(agent.kind, "claude");
    assert_eq!(agent.session_id.as_deref(), Some("sess-1"));
    assert_eq!(agent.root, Some(root));
}

#[test]
fn direct_children_of_an_agent_root_inherit_owner_and_agent() {
    let root = id(200, 1);
    let child = id(201, 1);
    let grandchild = id(202, 1);
    let platform = FakePlatform::default().env(
        root,
        &[
            ("BALLAST_OWNER", "owner-x"),
            ("CLAUDE_CODE_SESSION_ID", "sess-2"),
            ("CLAUDE_PID", "200"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 200, CLAUDE, &[]),
        proc(child, 200, 201, "/usr/local/bin/mcp-server", &[]),
        proc(grandchild, 201, 201, "/usr/local/bin/mcp-helper", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let root_attr = attr(&snapshot, root);
    let child_attr = attr(&snapshot, child);
    let grandchild_attr = attr(&snapshot, grandchild);
    assert_eq!(child_attr.role, ProcessRole::AgentInternal);
    assert_eq!(child_attr.agent_id, root_attr.agent_id);
    assert_eq!(child_attr.owner_id, root_attr.owner_id);
    assert_eq!(
        grandchild_attr.role,
        ProcessRole::AgentInternal,
        "a descendant of an agent-internal process must stay agent-internal too"
    );
    assert_eq!(grandchild_attr.agent_id, root_attr.agent_id);
}

// ---------------------------------------------------------------------
// Binary recognition: exact basename, and the data-backed path+argv0 rule.
// ---------------------------------------------------------------------

#[test]
fn claude_installed_under_a_versions_path_is_recognized_via_exe_path_and_argv0() {
    let root = id(250, 1);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(
            root,
            1,
            250,
            "/Users/dev/.claude/local/node_modules/@anthropic-ai/claude-code/claude/versions/1.2.3/cli.mjs",
            &["claude", "-p", "hello"],
        )],
        Instant::now(),
        0,
        false,
    );
    let attribution = attr(&snapshot, root);
    assert_eq!(
        attribution.role,
        ProcessRole::AgentRoot,
        "the versions-path + argv0=claude pattern must be recognized without any env marker"
    );
    assert_eq!(
        agent_of(&snapshot, attribution.agent_id.as_deref().unwrap()).kind,
        "claude"
    );
}

#[test]
fn same_versions_path_with_a_different_argv0_is_not_recognized_as_claude() {
    let root = id(251, 1);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(
            root,
            1,
            251,
            "/Users/dev/.claude/local/node_modules/@anthropic-ai/claude-code/claude/versions/1.2.3/cli.mjs",
            &["node", "cli.mjs"],
        )],
        Instant::now(),
        0,
        false,
    );
    assert_eq!(
        attr(&snapshot, root).role,
        ProcessRole::Unattributed,
        "the versions-path rule requires argv0=claude; a plain node invocation must not match"
    );
}

// ---------------------------------------------------------------------
// Owner-only roots: a generic agent, upgraded in place if a real kind shows up.
// ---------------------------------------------------------------------

#[test]
fn owner_only_topmost_carrier_becomes_a_generic_agent_keyed_by_owner_id() {
    let root = id(300, 1);
    let platform = FakePlatform::default().env(
        root,
        &[
            ("BALLAST_OWNER", "owner-generic"),
            ("BALLAST_OWNER_NAME", "Generic Owner"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(root, 1, 300, "/usr/bin/some-tool", &[])],
        Instant::now(),
        0,
        false,
    );

    let attribution = attr(&snapshot, root);
    assert_eq!(attribution.role, ProcessRole::AgentRoot);
    let owner_id = attribution
        .owner_id
        .clone()
        .expect("owner marker must be recorded");
    let owner = snapshot.owners.iter().find(|o| o.id == owner_id).unwrap();
    assert_eq!(owner.name.as_deref(), Some("Generic Owner"));
    let agent = agent_of(&snapshot, attribution.agent_id.as_deref().unwrap());
    assert_eq!(agent.kind, "generic");
    assert_eq!(
        agent.session_id.as_deref(),
        Some("owner-generic"),
        "a kindless owner root becomes a generic agent keyed by the owner id"
    );
}

#[test]
fn a_process_carrying_any_agent_marker_never_falls_back_to_the_owner_only_generic_root() {
    let root = id(302, 1);
    // Carries both BALLAST_OWNER and an Agent-level marker; the Agent marker must win,
    // not the owner-only generic-root fallback.
    let platform = FakePlatform::default().env(
        root,
        &[
            ("BALLAST_OWNER", "owner-with-agent"),
            ("CODEX_THREAD_ID", "sess-with-owner"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(root, 1, 302, "/usr/bin/some-tool", &[])],
        Instant::now(),
        0,
        false,
    );

    let attribution = attr(&snapshot, root);
    let agent = agent_of(&snapshot, attribution.agent_id.as_deref().unwrap());
    assert_eq!(agent.kind, "codex");
    assert_eq!(agent.session_id.as_deref(), Some("sess-with-owner"));
    assert_eq!(agent.owner_id.as_deref(), Some("owner-with-agent"));
}

#[test]
fn a_later_kind_marker_upgrades_the_same_owner_root_in_place() {
    let root = id(301, 1);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let generic_platform = FakePlatform::default().env(root, &[("BALLAST_OWNER", "owner-upgrade")]);
    let first = attributor.update(
        &generic_platform,
        &mut [proc(root, 1, 301, "/usr/bin/some-tool", &[])],
        now,
        0,
        false,
    );
    let agent_id = attr(&first, root).agent_id.clone().unwrap();
    assert_eq!(agent_of(&first, &agent_id).kind, "generic");

    // Same root process now also exports the Claude session markers.
    let claude_platform = FakePlatform::default().env(
        root,
        &[
            ("BALLAST_OWNER", "owner-upgrade"),
            ("CLAUDE_CODE_SESSION_ID", "sess-upgrade"),
            ("CLAUDE_PID", "301"),
        ],
    );
    let second = attributor.update(
        &claude_platform,
        &mut [proc(root, 1, 301, CLAUDE, &[])],
        now,
        0,
        false,
    );
    let upgraded_agent_id = attr(&second, root).agent_id.clone().unwrap();
    assert_eq!(
        upgraded_agent_id, agent_id,
        "the root's agent identity must stay the same across the upgrade"
    );
    let agent = agent_of(&second, &agent_id);
    assert_eq!(agent.kind, "claude");
    assert_eq!(agent.session_id.as_deref(), Some("sess-upgrade"));
}

// ---------------------------------------------------------------------
// Root PID validation: binary check and start_time ordering.
// ---------------------------------------------------------------------

#[test]
fn a_claude_pid_marker_pointing_at_a_non_claude_binary_is_rejected_as_root() {
    let bogus_root = id(400, 1); // real process, but not a claude binary
    let claimant = id(401, 1);
    let platform = FakePlatform::default().env(
        claimant,
        &[
            ("CLAUDE_CODE_SESSION_ID", "sess-badroot"),
            ("CLAUDE_PID", "400"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(bogus_root, 1, 400, "/bin/sh", &[]),
        proc(claimant, 1, 401, "/usr/bin/something", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let claimant_attr = attr(&snapshot, claimant);
    let agent_id = claimant_attr
        .agent_id
        .clone()
        .expect("marker must still create an agent");
    let agent = agent_of(&snapshot, &agent_id);
    assert_eq!(
        agent.root, None,
        "an invalid CLAUDE_PID target must not become the agent's root"
    );
    assert_eq!(agent.kind, "claude");
}

#[test]
fn a_claude_pid_marker_pointing_at_a_younger_process_is_rejected_as_root() {
    let younger_claude = id(410, 5); // started after the claimant: cannot be its ancestor
    let claimant = id(411, 1);
    let platform = FakePlatform::default().env(
        claimant,
        &[
            ("CLAUDE_CODE_SESSION_ID", "sess-badorder"),
            ("CLAUDE_PID", "410"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(younger_claude, 1, 410, CLAUDE, &[]),
        proc(claimant, 1, 411, "/usr/bin/something", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let claude_root_attr = attr(&snapshot, younger_claude);
    let claimant_attr = attr(&snapshot, claimant);
    assert_eq!(claude_root_attr.role, ProcessRole::AgentRoot);
    assert_ne!(
        claimant_attr.agent_id, claude_root_attr.agent_id,
        "a start_time-inverted CLAUDE_PID target must not be accepted as the claimant's root"
    );
}

// ---------------------------------------------------------------------
// Detached marker without a root: a rootless agent, already ended at first sight.
// ---------------------------------------------------------------------

#[test]
fn a_marker_with_no_resolvable_root_creates_a_rootless_agent_already_ended() {
    let orphan = id(420, 1);
    let platform = FakePlatform::default().env(orphan, &[("CODEX_THREAD_ID", "sess-rootless")]); // no codex-binary ancestor anywhere
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let wall_ms = 12_345;
    let snapshot = attributor.update(
        &platform,
        &mut [proc(orphan, 1, orphan.pid, "/usr/bin/something", &[])],
        Instant::now(),
        wall_ms,
        false,
    );

    let attribution = attr(&snapshot, orphan);
    let agent_id = attribution
        .agent_id
        .clone()
        .expect("session marker must still create an agent");
    let agent = agent_of(&snapshot, &agent_id);
    assert_eq!(agent.root, None);
    assert_eq!(agent.kind, "codex");
    assert_eq!(agent.session_id.as_deref(), Some("sess-rootless"));
    assert_eq!(
        agent.ended_at_ms,
        Some(wall_ms),
        "a rootless agent must be already-ended, timestamped at first sight"
    );
}

// ---------------------------------------------------------------------
// Nested agents: a known agent binary always starts a new agent, even inside another agent's
// tree, and env markers inherited from the outer agent must not swallow the nested one.
// ---------------------------------------------------------------------

#[test]
fn a_nested_known_agent_binary_starts_its_own_agent_even_though_outer_markers_are_inherited() {
    // claude(root) -> sh -c (workload) -> codex(root, new agent) -> zsh -c (workload) -> leaf
    // Every descendant also carries the outer CLAUDE_* env, as real fork/exec inheritance
    // would leave behind, to prove the "nearest root wins" guard actually does the work.
    let claude_root = id(500, 1);
    let outer_shell = id(501, 1);
    let codex_root = id(502, 1);
    let inner_shell = id(503, 1);
    let leaf = id(504, 1);

    let outer_env: &[(&str, &str)] = &[
        ("CLAUDE_CODE_SESSION_ID", "sess-outer"),
        ("CLAUDE_PID", "500"),
    ];
    let mut inner_env: Vec<(&str, &str)> = outer_env.to_vec();
    inner_env.push(("CODEX_THREAD_ID", "sess-inner"));

    let platform = FakePlatform::default()
        .env(claude_root, outer_env)
        .env(outer_shell, outer_env)
        .env(codex_root, &inner_env)
        .env(inner_shell, &inner_env)
        .env(leaf, &inner_env);

    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(claude_root, 1, claude_root.pid, CLAUDE, &[]),
        proc(
            outer_shell,
            claude_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run codex"],
        ),
        proc(codex_root, outer_shell.pid, codex_root.pid, CODEX, &[]),
        proc(
            inner_shell,
            codex_root.pid,
            inner_shell.pid,
            "/bin/zsh",
            &["zsh", "-c", "leaf work"],
        ),
        proc(leaf, inner_shell.pid, inner_shell.pid, "/usr/bin/leaf", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let claude_attr = attr(&snapshot, claude_root);
    let outer_shell_attr = attr(&snapshot, outer_shell);
    let codex_attr = attr(&snapshot, codex_root);
    let inner_shell_attr = attr(&snapshot, inner_shell);
    let leaf_attr = attr(&snapshot, leaf);

    assert_eq!(claude_attr.role, ProcessRole::AgentRoot);
    assert_eq!(outer_shell_attr.role, ProcessRole::Workload);
    assert_eq!(outer_shell_attr.agent_id, claude_attr.agent_id);

    assert_eq!(
        codex_attr.role,
        ProcessRole::AgentRoot,
        "a known agent binary must start a new agent even nested inside another agent's tree"
    );
    assert_ne!(codex_attr.agent_id, claude_attr.agent_id);
    assert_eq!(
        agent_of(&snapshot, codex_attr.agent_id.as_deref().unwrap()).kind,
        "codex"
    );

    assert_eq!(inner_shell_attr.role, ProcessRole::Workload);
    assert_eq!(
        inner_shell_attr.agent_id, codex_attr.agent_id,
        "the inherited outer CLAUDE_* env must not swallow the nested codex agent"
    );
    assert_ne!(inner_shell_attr.workload_id, outer_shell_attr.workload_id);

    assert_eq!(leaf_attr.role, ProcessRole::Workload);
    assert_eq!(leaf_attr.agent_id, codex_attr.agent_id);
    assert_eq!(
        leaf_attr.workload_id, inner_shell_attr.workload_id,
        "a plain child of a workload root inherits that same workload"
    );
}

#[test]
fn a_previously_unattributed_process_execs_into_a_known_agent_binary() {
    let root = id(2000, 1);
    let child = id(2001, 1);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 2000, "/usr/bin/before", &[]),
        proc(child, 2000, 2000, "/usr/bin/before-child", &[]),
    ];

    let platform = FakePlatform::default();
    let before = attributor.update(&platform, &mut processes, now, 0, false);
    assert_eq!(attr(&before, root).role, ProcessRole::Unattributed);
    assert_eq!(attr(&before, child).role, ProcessRole::Unattributed);

    // Same identity, exec's into a known agent binary in place.
    processes[0] = proc(root, 1, 2000, CLAUDE, &[]);
    let after = attributor.update(
        &platform,
        &mut processes,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(attr(&after, root).role, ProcessRole::AgentRoot);
    let agent_id = attr(&after, root).agent_id.clone();
    assert_eq!(
        agent_of(&after, agent_id.as_deref().unwrap()).kind,
        "claude"
    );
    assert_eq!(
        attr(&after, child).agent_id,
        agent_id,
        "a child under a process that execs into a known agent binary must pick up the new agent"
    );
    assert_eq!(attr(&after, child).role, ProcessRole::AgentInternal);
}

#[test]
fn an_ordinary_child_that_execs_into_a_known_agent_binary_moves_its_existing_descendants_to_the_new_root()
 {
    // claude(root) -> sh -c (workload) -> ordinary child -> zsh -c (workload) -> leaf, all under
    // claude's agent at first. The ordinary child then execs into codex in place (same
    // identity): it must become its own AgentRoot, and its already-cached descendants must move
    // to the new codex agent, not keep pointing at their stale claude context.
    let claude_root = id(2100, 1);
    let outer_shell = id(2101, 1);
    let exec_child = id(2102, 1);
    let inner_shell = id(2103, 1);
    let leaf = id(2104, 1);
    let outer_env: &[(&str, &str)] = &[
        ("CLAUDE_CODE_SESSION_ID", "sess-exec-move"),
        ("CLAUDE_PID", "2100"),
    ];
    let mut processes = [
        proc(claude_root, 1, claude_root.pid, CLAUDE, &[]),
        proc(
            outer_shell,
            claude_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run worker"],
        ),
        proc(
            exec_child,
            outer_shell.pid,
            exec_child.pid,
            "/usr/bin/ordinary",
            &[],
        ),
        proc(
            inner_shell,
            exec_child.pid,
            inner_shell.pid,
            "/bin/zsh",
            &["zsh", "-c", "leaf work"],
        ),
        proc(leaf, inner_shell.pid, inner_shell.pid, "/usr/bin/leaf", &[]),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let platform = FakePlatform::default().env(claude_root, outer_env);
    let before = attributor.update(&platform, &mut processes, now, 0, false);
    let claude_agent = attr(&before, claude_root).agent_id.clone();
    assert_eq!(
        attr(&before, exec_child).agent_id,
        claude_agent,
        "before the exec, the ordinary child is still just internal to claude's agent"
    );
    assert_eq!(attr(&before, leaf).agent_id, claude_agent);

    processes[2] = proc(exec_child, outer_shell.pid, exec_child.pid, CODEX, &[]);
    let after = attributor.update(
        &platform,
        &mut processes,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let codex_attr = attr(&after, exec_child);
    assert_eq!(codex_attr.role, ProcessRole::AgentRoot);
    assert_eq!(
        agent_of(&after, codex_attr.agent_id.as_deref().unwrap()).kind,
        "codex"
    );
    assert_ne!(
        codex_attr.agent_id, claude_agent,
        "the exec'd process must start its own agent, not stay claude's descendant"
    );

    let inner_shell_attr = attr(&after, inner_shell);
    let leaf_attr = attr(&after, leaf);
    assert_eq!(
        inner_shell_attr.agent_id, codex_attr.agent_id,
        "an already-cached descendant must move to the nearest new root, not keep its stale \
         cached agent"
    );
    assert_eq!(inner_shell_attr.role, ProcessRole::Workload);
    assert_eq!(leaf_attr.agent_id, codex_attr.agent_id);
    assert_eq!(leaf_attr.workload_id, inner_shell_attr.workload_id);
}

// ---------------------------------------------------------------------
// Tool-call shell vs. agent-internal, and same-agent markers preserving ancestry.
// ---------------------------------------------------------------------

#[test]
fn tool_call_shell_starts_a_workload_other_direct_children_stay_internal() {
    let root = id(600, 1);
    let workload_shell = id(601, 1);
    let workload_grandchild = id(602, 1);
    let internal_child = id(603, 1);
    let internal_grandchild = id(604, 1);
    let platform = FakePlatform::default().env(
        root,
        &[("CLAUDE_CODE_SESSION_ID", "sess-3"), ("CLAUDE_PID", "600")],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 600, CLAUDE, &[]),
        proc(
            workload_shell,
            600,
            601,
            "/bin/bash",
            &["bash", "-c", "npm test"],
        ),
        proc(workload_grandchild, 601, 601, "/usr/bin/npm", &[]),
        proc(internal_child, 600, 603, "/usr/local/bin/mcp-server", &[]),
        proc(
            internal_grandchild,
            603,
            603,
            "/usr/local/bin/mcp-helper",
            &[],
        ),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let workload_attr = attr(&snapshot, workload_shell);
    assert_eq!(workload_attr.role, ProcessRole::Workload);
    let workload_id = workload_attr.workload_id.clone().unwrap();
    assert_eq!(
        attr(&snapshot, workload_grandchild).workload_id,
        Some(workload_id)
    );

    let internal_attr = attr(&snapshot, internal_child);
    assert_eq!(internal_attr.role, ProcessRole::AgentInternal);
    assert!(internal_attr.workload_id.is_none());
    let internal_grandchild_attr = attr(&snapshot, internal_grandchild);
    assert_eq!(internal_grandchild_attr.role, ProcessRole::AgentInternal);
    assert!(internal_grandchild_attr.workload_id.is_none());
}

/// Runs one root + one direct-child shell through `Attributor::update` and returns the
/// shell's role, for the compact tool-shell regressions below.
fn shell_child_role(exe: &str, argv: &[&str], additional_shells: Vec<String>) -> ProcessRole {
    let root = id(650, 1);
    let shell = id(651, 1);
    let platform = FakePlatform::default().env(
        root,
        &[
            ("CLAUDE_CODE_SESSION_ID", "sess-shell"),
            ("CLAUDE_PID", "650"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), additional_shells);
    let mut processes = [
        proc(root, 1, 650, CLAUDE, &[]),
        proc(shell, 650, 651, exe, argv),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);
    attr(&snapshot, shell).role
}

#[test]
fn dash_invoked_as_the_sh_alias_is_recognized_as_a_tool_call_shell() {
    // The real Linux failure this regresses: exe resolves to /usr/bin/dash (a `sh` symlink
    // target), but argv[0] is still "sh" as the caller invoked it.
    assert_eq!(
        shell_child_role("/usr/bin/dash", &["sh", "-c", "true"], Vec::new()),
        ProcessRole::Workload
    );
}

#[test]
fn bash_with_combined_login_and_c_flags_is_recognized() {
    assert_eq!(
        shell_child_role("/bin/bash", &["bash", "-lc", "echo hi"], Vec::new()),
        ProcessRole::Workload
    );
}

#[test]
fn zsh_with_a_plain_c_flag_is_recognized() {
    assert_eq!(
        shell_child_role("/bin/zsh", &["zsh", "-c", "echo hi"], Vec::new()),
        ProcessRole::Workload
    );
}

#[test]
fn a_custom_configured_shell_is_recognized_only_when_listed() {
    assert_eq!(
        shell_child_role(
            "/usr/local/bin/myshell",
            &["myshell", "-c", "echo hi"],
            Vec::new()
        ),
        ProcessRole::AgentInternal,
        "an unlisted shell binary must not be treated as a tool-call shell"
    );
    assert_eq!(
        shell_child_role(
            "/usr/local/bin/myshell",
            &["myshell", "-c", "echo hi"],
            vec!["myshell".into()]
        ),
        ProcessRole::Workload,
        "a shell added via config must be recognized the same way as a builtin"
    );
}

#[test]
fn a_same_agent_marker_on_a_descendant_does_not_reset_its_inherited_workload() {
    // A child that independently carries the same agent's session marker (e.g. re-exported by
    // a wrapper script) must keep flowing through the normal ancestry resolution, not get
    // treated as a brand-new detachment.
    let root = id(610, 1);
    let shell = id(611, 1);
    let marked_grandchild = id(612, 1);
    let env = [("CLAUDE_CODE_SESSION_ID", "sess-4"), ("CLAUDE_PID", "610")];
    let platform = FakePlatform::default()
        .env(root, &env)
        .env(marked_grandchild, &env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 610, CLAUDE, &[]),
        proc(shell, 610, 611, "/bin/sh", &["sh", "-c", "build"]),
        proc(marked_grandchild, 611, 611, "/usr/bin/cc", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let shell_attr = attr(&snapshot, shell);
    let grandchild_attr = attr(&snapshot, marked_grandchild);
    assert_eq!(grandchild_attr.role, ProcessRole::Workload);
    assert_eq!(
        grandchild_attr.workload_id, shell_attr.workload_id,
        "a same-agent marker on a descendant must not detach it from its parent's workload"
    );
}

// ---------------------------------------------------------------------
// Detached grouping by (agent, process group id).
// ---------------------------------------------------------------------

#[test]
fn detached_processes_group_by_agent_and_process_group_id() {
    let root = id(700, 1);
    let detached_x = id(701, 1);
    let detached_y = id(702, 1);
    let detached_z = id(703, 1); // shares pgid with X
    let root_env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-detach"),
        ("CLAUDE_PID", "700"),
    ];
    // Bogus CLAUDE_PID (no such process in this tick) so each detached process's own root
    // lookup fails and it falls back to joining the existing session's agent by session id.
    // The root is listed first below, since the session lookup only finds the root's agent
    // once the root's own marker pass has already run within this same tick.
    let detached_env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-detach"),
        ("CLAUDE_PID", "999999"),
    ];
    let platform = FakePlatform::default()
        .env(root, &root_env)
        .env(detached_x, &detached_env)
        .env(detached_y, &detached_env)
        .env(detached_z, &detached_env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 700, CLAUDE, &[]),
        proc(detached_x, 1, 701, "/usr/bin/tool-a", &[]), // ppid 1: first seen already detached
        proc(detached_y, 1, 702, "/usr/bin/tool-b", &[]),
        proc(detached_z, 1, 701, "/usr/bin/tool-c", &[]), // pgid 701, same as X
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let root_attr = attr(&snapshot, root);
    let x_attr = attr(&snapshot, detached_x);
    let y_attr = attr(&snapshot, detached_y);
    let z_attr = attr(&snapshot, detached_z);
    assert_eq!(x_attr.role, ProcessRole::Workload);
    assert_eq!(x_attr.agent_id, root_attr.agent_id);
    assert_eq!(
        x_attr.agent_id, y_attr.agent_id,
        "both join the same agent by session id"
    );
    assert_ne!(
        x_attr.workload_id, y_attr.workload_id,
        "different process groups must be different detached workloads"
    );
    assert_eq!(
        x_attr.workload_id, z_attr.workload_id,
        "the same (agent, pgid) must reuse the same detached workload"
    );
}

// ---------------------------------------------------------------------
// Detached retention: a process that was already attributed keeps its attribution once
// reparented, instead of being regrouped as a fresh first-seen detached process.
// ---------------------------------------------------------------------

#[test]
fn a_process_keeps_its_attribution_after_being_reparented_to_pid_1() {
    let root = id(800, 1);
    let child = id(801, 1);
    let platform = FakePlatform::default().env(
        root,
        &[("CLAUDE_CODE_SESSION_ID", "sess-5"), ("CLAUDE_PID", "800")],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let before = attributor.update(
        &platform,
        &mut [
            proc(root, 1, 800, CLAUDE, &[]),
            proc(child, 800, 801, "/usr/local/bin/mcp-server", &[]),
        ],
        now,
        0,
        false,
    );
    let before_attr = attr(&before, child).clone();
    assert_eq!(before_attr.role, ProcessRole::AgentInternal);

    // The child is now reparented to pid 1 (its original parent detached it), but the root
    // agent is still alive and present.
    let after = attributor.update(
        &platform,
        &mut [
            proc(root, 1, 800, CLAUDE, &[]),
            proc(child, 1, 801, "/usr/local/bin/mcp-server", &[]),
        ],
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let after_attr = attr(&after, child);
    assert_eq!(
        after_attr.role, before_attr.role,
        "reparenting to pid 1 must not turn an already-attributed process into a fresh detached workload"
    );
    assert_eq!(after_attr.agent_id, before_attr.agent_id);
    assert_eq!(after_attr.workload_id, before_attr.workload_id);
}

// ---------------------------------------------------------------------
// PID reuse safety.
// ---------------------------------------------------------------------

#[test]
fn a_reused_pid_with_a_new_start_time_does_not_inherit_stale_attribution() {
    let stale = id(900, 1);
    let reused = id(900, 2);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let attributed_platform = FakePlatform::default().env(
        stale,
        &[
            ("CLAUDE_CODE_SESSION_ID", "sess-reuse"),
            ("CLAUDE_PID", "900"),
        ],
    );
    let first = attributor.update(
        &attributed_platform,
        &mut [proc(stale, 1, 900, CLAUDE, &[])],
        now,
        0,
        false,
    );
    assert_eq!(attr(&first, stale).role, ProcessRole::AgentRoot);

    let unattributed_platform = FakePlatform::default();
    let second = attributor.update(
        &unattributed_platform,
        &mut [proc(reused, 1, 900, "/usr/bin/unrelated", &[])],
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let reused_attr = attr(&second, reused);
    assert_eq!(reused_attr.role, ProcessRole::Unattributed);
    assert!(reused_attr.agent_id.is_none());
    assert!(reused_attr.owner_id.is_none());
}

// ---------------------------------------------------------------------
// Safety: only the daemon's own uid is ever attributed. A foreign uid must never have its
// environment or ports read, and must never inherit or grant attribution.
// ---------------------------------------------------------------------

#[test]
fn a_foreign_uid_process_is_never_attributed_and_never_read() {
    let foreign_root = id(1050, 1);
    let foreign_child = id(1051, 1);
    let platform = FakePlatform::default()
        .env(
            foreign_root,
            &[
                ("CLAUDE_CODE_SESSION_ID", "sess-foreign"),
                ("CLAUDE_PID", "1050"),
            ],
        )
        .ports(foreign_root, &[1234]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc_uid(
            foreign_root,
            1,
            1050,
            CLAUDE,
            &[],
            own_uid().wrapping_add(1),
        ),
        proc_uid(
            foreign_child,
            1050,
            1050,
            "/usr/bin/child",
            &[],
            own_uid().wrapping_add(1),
        ),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let root_attr = attr(&snapshot, foreign_root);
    assert_eq!(root_attr.role, ProcessRole::Unattributed);
    assert!(root_attr.agent_id.is_none());
    assert!(root_attr.owner_id.is_none());
    assert!(
        !root_attr.environment_known,
        "a foreign uid's environment must never be read"
    );
    assert_eq!(platform.env_read_count(foreign_root), 0);
    assert_eq!(
        platform.port_read_count(foreign_root),
        0,
        "a foreign uid's ports must never be sampled"
    );

    let child_attr = attr(&snapshot, foreign_child);
    assert_eq!(
        child_attr.role,
        ProcessRole::Unattributed,
        "a foreign uid child must not inherit attribution even from a marker-bearing foreign parent"
    );
}

/// The foreign-uid boundary only ever suppresses attribution for the foreign process itself; it
/// does not otherwise corrupt resolution below it. A user-owned process re-emerging as a *child*
/// of a foreign-uid intermediary (the intermediary's parent died and it was reparented, or it
/// simply forked one) must resolve exactly as if the foreign hop were not there: an ordinary,
/// unmarked descendant stays Unattributed (there is nothing for it to inherit through the
/// foreign gap), while a real recognized agent binary is still recognized as its own AgentRoot
/// and its own children still resolve as its own Workload, same as any other AgentRoot.
#[test]
fn a_user_owned_agent_root_below_a_foreign_uid_intermediary_still_resolves_normally() {
    let user_ancestor = id(5900, 1);
    let foreign_intermediary = id(5901, 1);
    let unmarked_descendant = id(5902, 1);
    let agent_root = id(5903, 1);
    let shell = id(5904, 1);
    let platform = FakePlatform::default().env(
        agent_root,
        &[
            ("CLAUDE_CODE_SESSION_ID", "sess-across-foreign-boundary"),
            ("CLAUDE_PID", "5903"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(user_ancestor, 1, 5900, "/usr/bin/user-ancestor", &[]),
        proc_uid(
            foreign_intermediary,
            5900,
            5901,
            "/usr/bin/foreign-supervisor",
            &[],
            own_uid().wrapping_add(1),
        ),
        proc(
            unmarked_descendant,
            5901,
            5902,
            "/usr/bin/unmarked-worker",
            &[],
        ),
        proc(agent_root, 5901, 5903, CLAUDE, &[]),
        proc(shell, 5903, 5904, "/bin/bash", &["bash", "-c", "npm test"]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let foreign_attr = attr(&snapshot, foreign_intermediary);
    assert_eq!(foreign_attr.role, ProcessRole::Unattributed);
    assert!(
        !foreign_attr.environment_known,
        "the existing foreign-uid rule is unchanged: its own environment must still never be read"
    );

    assert_eq!(
        attr(&snapshot, unmarked_descendant).role,
        ProcessRole::Unattributed,
        "an ordinary user-owned descendant re-emerging below a foreign-uid intermediary has \
         nothing to inherit through the foreign gap and must stay Unattributed"
    );

    let root_attr = attr(&snapshot, agent_root);
    assert_eq!(
        root_attr.role,
        ProcessRole::AgentRoot,
        "a real recognized agent binary re-emerging below a foreign-uid intermediary must still \
         be recognized as its own AgentRoot -- the foreign boundary above it must not block it"
    );
    let agent_id = root_attr.agent_id.clone().unwrap();

    let shell_attr = attr(&snapshot, shell);
    assert_eq!(
        shell_attr.role,
        ProcessRole::Workload,
        "the agent root's own children must resolve as its own Workload exactly as normal"
    );
    assert_eq!(shell_attr.agent_id.as_deref(), Some(agent_id.as_str()));
}

// ---------------------------------------------------------------------
// Unattributed processes are never touched by anything.
// ---------------------------------------------------------------------

#[test]
fn a_process_with_no_markers_and_no_attributed_ancestry_stays_unattributed() {
    let orphan = id(1000, 1);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    for tick in 0..3u64 {
        let snapshot = attributor.update(
            &platform,
            &mut [proc(orphan, 1, 1000, "/usr/bin/plain", &[])],
            Instant::now() + Duration::from_secs(tick),
            tick * 1000,
            false,
        );
        let attribution = attr(&snapshot, orphan);
        assert_eq!(attribution.role, ProcessRole::Unattributed);
        assert!(attribution.owner_id.is_none());
        assert!(attribution.agent_id.is_none());
        assert!(attribution.workload_id.is_none());
    }
}

// ---------------------------------------------------------------------
// Ended roots: still reported, as long as at least one attributed leftover process survives.
// ---------------------------------------------------------------------

#[test]
fn an_agent_is_reported_ended_once_its_root_exits_while_a_leftover_process_survives() {
    let root = id(1100, 1);
    let leftover = id(1101, 1);
    let platform = FakePlatform::default()
        .env(
            root,
            &[
                ("CLAUDE_CODE_SESSION_ID", "sess-ended"),
                ("CLAUDE_PID", "1100"),
            ],
        )
        .gone(root); // confirmed exit, not just an omitted read, once the root leaves the table
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let alive = attributor.update(
        &platform,
        &mut [
            proc(root, 1, 1100, CLAUDE, &[]),
            proc(leftover, 1100, 1101, "/usr/local/bin/mcp-server", &[]),
        ],
        now,
        0,
        false,
    );
    let agent_id = attr(&alive, root).agent_id.clone().unwrap();
    assert!(agent_of(&alive, &agent_id).ended_at_ms.is_none());

    // The root exits; the leftover (agent-internal) process is still around, reparented to 1.
    let after = attributor.update(
        &platform,
        &mut [proc(leftover, 1, 1101, "/usr/local/bin/mcp-server", &[])],
        now + Duration::from_secs(1),
        5_000,
        false,
    );
    let agent = agent_of(&after, &agent_id);
    assert_eq!(
        agent.ended_at_ms,
        Some(5_000),
        "ended_at_ms must be set from the wall clock of the tick that observed the root's exit"
    );
    assert_eq!(agent.state, AgentState::Ended);
    assert_eq!(
        attr(&after, leftover).agent_id.as_deref(),
        Some(agent_id.as_str()),
        "the leftover process must keep its attribution so cleanup can still find it"
    );
}

// ---------------------------------------------------------------------
// Env caching: read once per identity, exec invalidates, None vs empty.
// ---------------------------------------------------------------------

#[test]
fn environment_is_read_once_per_identity_and_cached_across_ticks() {
    let target = id(1200, 1);
    let platform = FakePlatform::default().env(target, &[("BALLAST_OWNER", "owner-cache")]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    attributor.update(
        &platform,
        &mut [proc(target, 1, 1200, "/usr/bin/agent", &[])],
        now,
        0,
        false,
    );
    attributor.update(
        &platform,
        &mut [proc(target, 1, 1200, "/usr/bin/agent", &[])],
        now + Duration::from_secs(1),
        1000,
        false,
    );

    assert_eq!(
        platform.env_read_count(target),
        1,
        "an unchanged identity (same exe) must not trigger a second environment read"
    );
}

#[test]
fn unknown_environment_is_distinguished_from_known_empty_and_both_are_cached() {
    let unknown = id(1201, 1);
    let known_empty = id(1202, 1);
    let platform = FakePlatform::default()
        .unknown_env(unknown)
        .env(known_empty, &[]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(unknown, 1, unknown.pid, "/usr/bin/a", &[]),
        proc(known_empty, 1, known_empty.pid, "/usr/bin/b", &[]),
    ];

    let snapshot = attributor.update(&platform, &mut processes, now, 0, false);
    assert!(
        !attr(&snapshot, unknown).environment_known,
        "a None environment read must report environment_known = false"
    );
    assert!(
        attr(&snapshot, known_empty).environment_known,
        "a Some(empty) environment read must report environment_known = true"
    );

    attributor.update(
        &platform,
        &mut processes,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        platform.env_read_count(unknown),
        1,
        "an unknown environment must be cached too, not retried every tick"
    );
    assert_eq!(platform.env_read_count(known_empty), 1);
}

#[test]
fn exec_change_invalidates_the_cached_environment_and_forces_a_reread() {
    let target = id(1203, 1);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let before_platform = FakePlatform::default().unknown_env(target);
    attributor.update(
        &before_platform,
        &mut [proc(target, 1, target.pid, "/usr/bin/before", &[])],
        now,
        0,
        false,
    );
    assert_eq!(before_platform.env_read_count(target), 1);

    // Same identity (pid + start_time unchanged), different exe: exec happened in place.
    let after_platform =
        FakePlatform::default().env(target, &[("BALLAST_OWNER", "owner-postexec")]);
    let after = attributor.update(
        &after_platform,
        &mut [proc(target, 1, target.pid, "/usr/bin/after", &[])],
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        after_platform.env_read_count(target),
        1,
        "the post-exec environment must be re-read exactly once"
    );
    assert!(
        attr(&after, target).environment_known,
        "a re-read that succeeds must flip environment_known back to true"
    );
}

#[test]
fn a_none_environment_still_lets_a_process_inherit_its_attributed_parent() {
    // "environment_known = false" only describes *this* process's own env read; it must not
    // block inheritance, and it must not be conflated with a known-but-empty environment.
    let root = id(1204, 1);
    let child = id(1205, 1);
    let platform = FakePlatform::default()
        .env(
            root,
            &[
                ("CLAUDE_CODE_SESSION_ID", "sess-noenv"),
                ("CLAUDE_PID", "1204"),
            ],
        )
        .unknown_env(child);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 1204, CLAUDE, &[]),
        proc(child, 1204, 1205, "/usr/local/bin/mcp-server", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let child_attr = attr(&snapshot, child);
    assert!(!child_attr.environment_known);
    assert_eq!(child_attr.role, ProcessRole::AgentInternal);
    assert_eq!(child_attr.agent_id, attr(&snapshot, root).agent_id);
}

// ---------------------------------------------------------------------
// Aggregates and the 30s growth window (synthetic Instants, no real sleeping).
// ---------------------------------------------------------------------

#[test]
fn growth_over_30s_requires_an_unbroken_run_of_valid_samples() {
    let root = id(1300, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-growth"),
        ("CLAUDE_PID", "1300"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    for (offset_secs, memory, rate, growth) in [
        (0u64, 100u64, None, None),
        (1, 110, None, None),
        (2, 120, Some(10), None),
        (10, 200, Some(10), None),
        (20, 300, Some(10), None),
        (30, 400, Some(10), Some(300)),
        (35, 500, Some(11), Some(380)),
        (40, 200, Some(0), Some(0)),
        (41, 100, Some(-3), Some(-100)),
    ] {
        let platform = FakePlatform::default()
            .env(root, &env)
            .metrics(root, memory);
        let snapshot = attributor.update(
            &platform,
            &mut [proc(root, 1, 1300, CLAUDE, &[])],
            now + Duration::from_secs(offset_secs),
            offset_secs * 1000,
            false,
        );
        let agent_id = attr(&snapshot, root).agent_id.clone().unwrap();
        let agent = agent_of(&snapshot, &agent_id);
        assert_eq!(agent.memory.bytes, memory);
        assert!(
            agent.memory.complete,
            "metrics were supplied every tick, so this tick's own reading is complete"
        );
        assert_eq!(agent.memory.growth_30s_bytes, growth);
        assert_eq!(agent.memory.growth_bytes_per_sec, rate, "at {offset_secs}s");
    }
}

#[test]
fn unknown_metrics_reset_growth_and_completeness() {
    let root = id(1301, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-growth-gap"),
        ("CLAUDE_PID", "1301"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    for (offset_secs, memory) in [(0u64, 100u64), (10, 150), (20, 200), (30, 260)] {
        let platform = FakePlatform::default()
            .env(root, &env)
            .metrics(root, memory);
        attributor.update(
            &platform,
            &mut [proc(root, 1, 1301, CLAUDE, &[])],
            now + Duration::from_secs(offset_secs),
            offset_secs * 1000,
            false,
        );
    }

    // A gap in metrics breaks completeness, even though the run up to now was otherwise fine.
    let gap_platform = FakePlatform::default().env(root, &env); // no metrics registered
    let after_gap = attributor.update(
        &gap_platform,
        &mut [proc(root, 1, 1301, CLAUDE, &[])],
        now + Duration::from_secs(31),
        31_000,
        false,
    );
    let agent_id = attr(&after_gap, root).agent_id.clone().unwrap();
    let agent = agent_of(&after_gap, &agent_id);
    assert!(
        !agent.memory.complete,
        "a tick with unknown metrics must reset the growth window's completeness"
    );
    assert_eq!(agent.memory.growth_30s_bytes, None);
    assert_eq!(agent.memory.growth_bytes_per_sec, None);
}

#[test]
fn a_discarded_tick_resets_the_growth_baseline() {
    let root = id(1302, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-growth-discard"),
        ("CLAUDE_PID", "1302"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    for offset_secs in 0u64..=30 {
        let discard = offset_secs == 15; // a discarded sample lands mid-window
        let platform = FakePlatform::default()
            .env(root, &env)
            .metrics(root, 100 + offset_secs);
        let snapshot = attributor.update(
            &platform,
            &mut [proc(root, 1, 1302, CLAUDE, &[])],
            now + Duration::from_secs(offset_secs),
            offset_secs * 1000,
            discard,
        );
        let agent_id = attr(&snapshot, root).agent_id.clone().unwrap();
        let memory = &agent_of(&snapshot, &agent_id).memory;
        if (15..17).contains(&offset_secs) {
            assert_eq!(memory.growth_bytes_per_sec, None);
        } else if offset_secs >= 17 {
            assert_eq!(memory.growth_bytes_per_sec, Some(1));
        }
        if offset_secs == 30 {
            let agent_id = attr(&snapshot, root).agent_id.clone().unwrap();
            let agent = agent_of(&snapshot, &agent_id);
            assert_eq!(
                agent.memory.growth_30s_bytes, None,
                "a discard in the middle of the window must reset the baseline, so 15s later the window still isn't a full unbroken 30s"
            );
        }
    }
}

#[test]
fn workload_memory_aggregates_separately_from_its_agents_total() {
    let root = id(1303, 1);
    let shell = id(1304, 1);
    let leaf = id(1305, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-workload-mem"),
        ("CLAUDE_PID", "1303"),
    ];
    let platform = FakePlatform::default()
        .env(root, &env)
        .metrics(root, 50)
        .metrics(shell, 30)
        .metrics(leaf, 20);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 1303, CLAUDE, &[]),
        proc(shell, 1303, 1304, "/bin/sh", &["sh", "-c", "work"]),
        proc(leaf, 1304, 1304, "/usr/bin/leaf", &[]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let workload_id = attr(&snapshot, shell).workload_id.clone().unwrap();
    let workload = snapshot
        .workloads
        .iter()
        .find(|w| w.id == workload_id)
        .unwrap();
    assert_eq!(
        workload.memory.bytes, 50,
        "workload total is only its own two processes (shell + leaf)"
    );
    let agent_id = attr(&snapshot, root).agent_id.clone().unwrap();
    let agent = agent_of(&snapshot, &agent_id);
    assert_eq!(
        agent.memory.bytes, 100,
        "agent total is every attributed process: root + workload"
    );
}

// ---------------------------------------------------------------------
// Listening ports: sampled for attributed processes only, on a slow cadence.
// ---------------------------------------------------------------------

#[test]
fn ports_are_never_sampled_for_an_unattributed_process() {
    let target = id(1400, 1);
    let platform = FakePlatform::default().ports(target, &[9000]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    for offset_secs in 0..3u64 {
        attributor.update(
            &platform,
            &mut [proc(target, 1, 1400, "/usr/bin/unrelated", &[])],
            now + Duration::from_secs(offset_secs * 60),
            0,
            false,
        );
    }
    assert_eq!(
        platform.port_read_count(target),
        0,
        "ports must never be sampled for an unattributed process"
    );
}

#[test]
fn ports_for_a_workload_member_refresh_on_a_shared_slow_round_not_every_tick() {
    // Roots have no `workload_id` and are never sampled; only workload members are.
    let root = id(1401, 1);
    let shell = id(1402, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-ports"),
        ("CLAUDE_PID", "1401"),
    ];
    // Empty (not a listener): a non-empty sample would make the workload a sticky service,
    // which is excluded from further rounds entirely -- this test wants repeated resampling.
    let platform = FakePlatform::default().env(root, &env).ports(shell, &[]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 1401, CLAUDE, &[]),
        proc(shell, 1401, 1402, "/bin/sh", &["sh", "-c", "serve"]),
    ];

    attributor.update(&platform, &mut processes, now, 0, false);
    assert_eq!(
        platform.port_read_count(shell),
        1,
        "the first shared sampling round must sample a workload member's ports"
    );
    assert_eq!(
        platform.port_read_count(root),
        0,
        "roots have no workload and must never be sampled"
    );

    for offset_ms in [200u64, 400, 600, 800] {
        attributor.update(
            &platform,
            &mut processes,
            now + Duration::from_millis(offset_ms),
            offset_ms,
            false,
        );
    }
    assert_eq!(
        platform.port_read_count(shell),
        1,
        "sub-second ticks must not each trigger a fresh shared sampling round"
    );

    attributor.update(
        &platform,
        &mut processes,
        now + Duration::from_secs(10),
        10_000,
        false,
    );
    assert!(
        platform.port_read_count(shell) > 1,
        "a tick well past the shared round interval must resample ports"
    );
}

// ---------------------------------------------------------------------
// Workload class: service via listening ports, or via accumulated age past 10 minutes.
// ---------------------------------------------------------------------

#[test]
fn a_workload_with_a_listening_port_is_a_service() {
    let root = id(1500, 1);
    let shell = id(1501, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-service-port"),
        ("CLAUDE_PID", "1500"),
    ];
    let platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[8080]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 1500, CLAUDE, &[]),
        proc(shell, 1500, 1501, "/bin/sh", &["sh", "-c", "serve"]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let workload_id = attr(&snapshot, shell).workload_id.clone().unwrap();
    let workload = snapshot
        .workloads
        .iter()
        .find(|w| w.id == workload_id)
        .unwrap();
    assert_eq!(workload.class, WorkloadClass::Service);
}

#[test]
fn a_freshly_started_workload_with_no_listening_port_is_batch() {
    let root = id(1502, 1);
    let shell = id(1503, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-batch"),
        ("CLAUDE_PID", "1502"),
    ];
    // Explicit known-empty (not just "unconfigured"): a successfully sampled listener-less
    // process, not one whose ports sample simply hasn't happened yet. Age is also explicitly
    // young: an unknown age must stay provisionally Service, so Batch here requires a real
    // young-age sample, not just the absence of a port.
    let platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(60));
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 1502, CLAUDE, &[]),
        proc(shell, 1502, 1503, "/bin/sh", &["sh", "-c", "build"]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let workload_id = attr(&snapshot, shell).workload_id.clone().unwrap();
    let workload = snapshot
        .workloads
        .iter()
        .find(|w| w.id == workload_id)
        .unwrap();
    assert_eq!(workload.class, WorkloadClass::Batch);
}

#[test]
fn a_workload_whose_process_age_exceeds_10_minutes_is_a_service() {
    let root = id(1504, 1);
    let shell = id(1505, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-old"),
        ("CLAUDE_PID", "1504"),
    ];
    // The daemon just started observing it, but the OS reports it has already run 11 minutes.
    let platform = FakePlatform::default()
        .env(root, &env)
        .age(shell, Duration::from_secs(11 * 60));
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = [
        proc(root, 1, 1504, CLAUDE, &[]),
        proc(shell, 1504, 1505, "/bin/sh", &["sh", "-c", "long-runner"]),
    ];
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);

    let workload_id = attr(&snapshot, shell).workload_id.clone().unwrap();
    let workload = snapshot
        .workloads
        .iter()
        .find(|w| w.id == workload_id)
        .unwrap();
    assert_eq!(
        workload.class,
        WorkloadClass::Service,
        "process age alone, without a listening port, must be enough to classify as a service past 10 minutes"
    );
}

#[test]
fn workload_class_transitions_unknown_empty_listener_unknown_empty() {
    let root = id(1524, 1);
    let shell = id(1525, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-class-transitions"),
        ("CLAUDE_PID", "1524"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 1524, CLAUDE, &[]),
        proc(shell, 1524, 1525, "/bin/sh", &["sh", "-c", "serve"]),
    ];

    // Offsets land on the shared ~5s sampling round (round 0 fires immediately, then every 5s)
    // so every tick below actually triggers a fresh sample instead of coasting on a pending one.
    let mut class_at =
        |attributor: &mut Attributor, platform: &FakePlatform, offset_secs: u64| -> WorkloadClass {
            let snapshot = attributor.update(
                platform,
                &mut processes,
                now + Duration::from_secs(offset_secs),
                offset_secs * 1000,
                false,
            );
            let workload_id = attr(&snapshot, shell).workload_id.clone().unwrap();
            snapshot
                .workloads
                .iter()
                .find(|w| w.id == workload_id)
                .unwrap()
                .class
        };

    // t=0: round 0, a sample that hasn't resolved yet must not be assumed harmless.
    let platform = FakePlatform::default().env(root, &env).unknown_ports(shell);
    assert_eq!(
        class_at(&mut attributor, &platform, 0),
        WorkloadClass::Service,
        "an unsampled/unknown port state must default to Service, not Batch"
    );

    // t=5: round 1, a real, successful empty sample settles it back to Batch (age is also
    // explicitly young, since an unknown age alone would keep it provisionally Service).
    let platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(60));
    assert_eq!(
        class_at(&mut attributor, &platform, 5),
        WorkloadClass::Batch
    );

    // t=10: round 2, a real listener flips it to Service, sticky for the workload's life.
    let platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[8080])
        .age(shell, Duration::from_secs(60));
    assert_eq!(
        class_at(&mut attributor, &platform, 10),
        WorkloadClass::Service
    );

    // t=15 and t=20: a sticky-service workload is excluded from further sampling rounds
    // entirely, so it stays Service regardless of what a (never-consulted) sample would say.
    let platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(60));
    assert_eq!(
        class_at(&mut attributor, &platform, 15),
        WorkloadClass::Service,
        "once flagged a service via a real listener, a workload must never revert to Batch"
    );
    assert_eq!(
        class_at(&mut attributor, &platform, 20),
        WorkloadClass::Service
    );
    assert_eq!(
        platform.port_read_count(shell),
        0,
        "a confirmed sticky-service workload must be skipped by later sampling rounds entirely"
    );
}

#[test]
fn a_newly_joined_unsampled_member_makes_an_otherwise_settled_workload_temporarily_a_service() {
    let root = id(1508, 1);
    let shell = id(1509, 1);
    let latecomer = id(1510, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-latecomer"),
        ("CLAUDE_PID", "1508"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let settle_platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(60));
    let settled = attributor.update(
        &settle_platform,
        &mut [
            proc(root, 1, 1508, CLAUDE, &[]),
            proc(shell, 1508, 1509, "/bin/sh", &["sh", "-c", "serve"]),
        ],
        now,
        0,
        false,
    );
    let workload_id = attr(&settled, shell).workload_id.clone().unwrap();
    assert_eq!(
        settled
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Batch
    );

    // A new process joins the same workload mid-round (the shared round isn't due again until
    // t=5), so it stays unsampled this tick regardless of what the platform would report.
    let joined_platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(60))
        .unknown_ports(latecomer)
        .age(latecomer, Duration::from_secs(60));
    let mut processes = [
        proc(root, 1, 1508, CLAUDE, &[]),
        proc(shell, 1508, 1509, "/bin/sh", &["sh", "-c", "serve"]),
        proc(latecomer, 1509, 1509, "/usr/bin/worker", &[]),
    ];
    let joined = attributor.update(
        &joined_platform,
        &mut processes,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        joined
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Service,
        "a workload with any member not yet successfully sampled must be temporarily Service"
    );
    assert_eq!(
        joined_platform.port_read_count(latecomer),
        0,
        "a member joining mid-round must wait for the next shared round, not trigger its own"
    );

    // At t=5 the next shared round fires and samples every non-service member, including the
    // latecomer; once it resolves (successfully, empty), the workload settles back to Batch.
    let resolved_platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(60))
        .ports(latecomer, &[])
        .age(latecomer, Duration::from_secs(60));
    let resolved = attributor.update(
        &resolved_platform,
        &mut processes,
        now + Duration::from_secs(5),
        5000,
        false,
    );
    assert_eq!(
        resolved
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Batch
    );
}

#[test]
fn age_triggered_service_classification_is_also_sticky_through_a_later_empty_sample() {
    let root = id(1511, 1);
    let shell = id(1512, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-age-sticky"),
        ("CLAUDE_PID", "1511"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 1511, CLAUDE, &[]),
        proc(shell, 1511, 1512, "/bin/sh", &["sh", "-c", "long-runner"]),
    ];

    let old_platform = FakePlatform::default()
        .env(root, &env)
        .age(shell, Duration::from_secs(11 * 60))
        .ports(shell, &[]);
    let first = attributor.update(&old_platform, &mut processes, now, 0, false);
    let workload_id = attr(&first, shell).workload_id.clone().unwrap();
    assert_eq!(
        first
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Service
    );

    // `process_age` is only ever sampled once (at first sight), so this tick supplies no age at
    // all -- if age-based stickiness weren't real, losing the age input could look like Batch.
    let no_listener_platform = FakePlatform::default().env(root, &env).ports(shell, &[]);
    let second = attributor.update(
        &no_listener_platform,
        &mut processes,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        second
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Service,
        "an age-triggered Service classification must stick for the workload's life too"
    );
}

// ---------------------------------------------------------------------
// Agent cwd: sampled from the root process, on a slow cadence.
// ---------------------------------------------------------------------

#[test]
fn agent_cwd_is_sampled_from_the_root_process() {
    let root = id(1600, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-cwd"),
        ("CLAUDE_PID", "1600"),
    ];
    let platform = FakePlatform::default()
        .env(root, &env)
        .cwd(root, "/home/anurag/project");
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(root, 1, 1600, CLAUDE, &[])],
        Instant::now(),
        0,
        false,
    );

    let agent_id = attr(&snapshot, root).agent_id.clone().unwrap();
    assert_eq!(
        agent_of(&snapshot, &agent_id).cwd.as_deref(),
        Some("/home/anurag/project")
    );
}

// ---------------------------------------------------------------------
// watched()/metric_targets(): light contract smoke tests.
// ---------------------------------------------------------------------

#[test]
fn metric_targets_includes_attributed_agent_roots() {
    let root = id(1700, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-targets"),
        ("CLAUDE_PID", "1700"),
    ];
    let platform = FakePlatform::default().env(root, &env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    attributor.update(
        &platform,
        &mut [proc(root, 1, 1700, CLAUDE, &[])],
        Instant::now(),
        0,
        false,
    );
    assert!(attributor.metric_targets().contains(&root));
}

// ---------------------------------------------------------------------
// Cursor marker: no session id, ever, even though CURSOR_AGENT carries a value.
// ---------------------------------------------------------------------

#[test]
fn cursor_marker_never_assigns_a_session_id() {
    let root = id(1800, 1);
    let platform = FakePlatform::default().env(root, &[("CURSOR_AGENT", "some-cursor-value")]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(root, 1, 1800, "/usr/local/bin/cursor-agent", &[])],
        Instant::now(),
        0,
        false,
    );

    let attribution = attr(&snapshot, root);
    assert_eq!(attribution.role, ProcessRole::AgentRoot);
    let agent = agent_of(&snapshot, attribution.agent_id.as_deref().unwrap());
    assert_eq!(agent.kind, "cursor");
    assert_eq!(
        agent.session_id, None,
        "the cursor marker's session_id=false must never populate a session id"
    );
}

#[test]
fn a_cursor_marker_with_no_ancestor_cursor_binary_is_ignored() {
    // No root_pid_key and no ancestor whose binary matches cursor-agent/Cursor: the marker
    // can't resolve a root and has no session fallback (session_id=false), so it must not
    // fabricate an agent at all.
    let orphan = id(1801, 1);
    let platform = FakePlatform::default().env(orphan, &[("CURSOR_AGENT", "some-cursor-value")]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(orphan, 1, orphan.pid, "/usr/bin/something", &[])],
        Instant::now(),
        0,
        false,
    );

    let attribution = attr(&snapshot, orphan);
    assert_eq!(attribution.role, ProcessRole::Unattributed);
    assert!(attribution.agent_id.is_none());
}

// ---------------------------------------------------------------------
// A custom marker added via `Attributor::new(additions)`, not one of the builtins.
// ---------------------------------------------------------------------

#[test]
fn a_custom_configured_marker_attributes_its_own_agent_kind() {
    let root = id(1900, 1);
    let custom = Marker {
        key: "MY_HARNESS_TASK_ID".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("myharness".into()),
        root_pid_key: None,
        root_binaries: vec!["myharness-cli".into()],
        session_id: true,
    };
    let platform = FakePlatform::default().env(root, &[("MY_HARNESS_TASK_ID", "task-42")]);
    let mut attributor = Attributor::new(vec![custom], Vec::new());
    let snapshot = attributor.update(
        &platform,
        &mut [proc(root, 1, 1900, "/usr/local/bin/myharness-cli", &[])],
        Instant::now(),
        0,
        false,
    );

    let attribution = attr(&snapshot, root);
    assert_eq!(attribution.role, ProcessRole::AgentRoot);
    let agent = agent_of(&snapshot, attribution.agent_id.as_deref().unwrap());
    assert_eq!(agent.kind, "myharness");
    assert_eq!(agent.session_id.as_deref(), Some("task-42"));
}

#[test]
fn watched_always_includes_frozen_identities() {
    let frozen = HashSet::from([id(1701, 1)]);
    let attributor = Attributor::new(Vec::new(), Vec::new());
    let watched = attributor.watched(&frozen);
    assert!(
        watched.is_superset(&frozen),
        "watched() must always include every frozen identity"
    );
}

// ---------------------------------------------------------------------
// Fixup 1 regressions (ticket 04 review, five findings): reproduced failing against the
// pre-fixup algorithm (see baseline evidence in the epic scratch), now exercising the fix.
// ---------------------------------------------------------------------

#[test]
fn a_transiently_omitted_root_does_not_end_a_still_live_agent() {
    let root = id(1930, 1);
    let shell = id(1931, 2);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let mut full = [
        proc(root, 1, root.pid, CODEX, &[]),
        proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "work"]),
    ];
    let first = attributor.update(&platform, &mut full, now, 0, false);
    let agent_id = attr(&first, root).agent_id.clone().unwrap();
    assert_ne!(agent_of(&first, &agent_id).state, AgentState::Ended);

    // The root's own read failed this scan (e.g. a transient EACCES on /proc/<pid>/stat), so it
    // is entirely absent from the enumerated process table for one tick, even though the OS
    // process is still alive; the platform has no positive evidence either way (default Unknown
    // liveness), which must never be treated as confirmed exit.
    let mut without_root = [full[1].clone()];
    let missing = attributor.update(
        &platform,
        &mut without_root,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_ne!(
        agent_of(&missing, &agent_id).state,
        AgentState::Ended,
        "an uncertain (not confirmed-gone) omission must not end a live agent"
    );

    // The root reappears, still the same live identity.
    let returned = attributor.update(
        &platform,
        &mut full,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let agent = agent_of(&returned, &agent_id);
    assert_ne!(agent.state, AgentState::Ended);
    assert_eq!(agent.ended_at_ms, None);
}

#[test]
fn a_detached_workload_joins_its_live_roots_session_regardless_of_enumeration_order() {
    for orphan_first in [false, true] {
        let root = id(1940, 1);
        let shell = id(1941, 2);
        let orphan = id(1942, 3);
        let platform = FakePlatform::default()
            .env(shell, &[("CODEX_THREAD_ID", "live-session")])
            .env(orphan, &[("CODEX_THREAD_ID", "live-session")]);
        let mut table = vec![
            proc(root, 1, root.pid, CODEX, &[]),
            proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "work"]),
            proc(orphan, 1, orphan.pid, "/usr/bin/worker", &[]),
        ];
        if orphan_first {
            table.rotate_right(1);
        }
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        for tick in 0..2u64 {
            let snapshot = attributor.update(
                &platform,
                &mut table,
                now + Duration::from_secs(tick),
                tick * 1000,
                false,
            );
            let root_agent = attr(&snapshot, root).agent_id.clone().unwrap();
            let orphan_agent = attr(&snapshot, orphan).agent_id.clone().unwrap();
            assert_eq!(
                orphan_agent, root_agent,
                "orphan_first={orphan_first} tick={tick}: the detached worker must join the \
                 same live session as the root, not a separate duplicate"
            );
            assert_eq!(
                snapshot.agents.len(),
                1,
                "orphan_first={orphan_first} tick={tick}: only one agent must exist for the \
                 shared live session"
            );
        }
    }
}

#[test]
fn a_nested_marker_defined_root_becomes_its_own_agent_instead_of_staying_a_workload_member() {
    let outer = id(1950, 1);
    let shell = id(1951, 2);
    let custom_root = id(1952, 3);
    let child = id(1953, 4);
    let marker = Marker {
        key: "CUSTOM_SESSION".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("custom".into()),
        root_pid_key: Some("CUSTOM_PID".into()),
        root_binaries: vec!["custom-agent".into()],
        session_id: true,
    };
    let mut attributor = Attributor::new(vec![marker], Vec::new());
    let now = Instant::now();
    // Real fork/exec would leave the outer CODEX_THREAD_ID marker inherited all the way down;
    // the custom root and its child carry it too, alongside the child's own CUSTOM_SESSION
    // claim. Both are valid claims on the same process, so discovery must weigh every one of
    // them -- by nearest ancestry, after every candidate root (including the custom one) has
    // been registered -- rather than stopping at whichever marker happens to be listed first
    // (the inherited builtin one would otherwise silently win and swallow the nested agent).
    let outer_env: &[(&str, &str)] = &[("CODEX_THREAD_ID", "outer-session")];
    let mut table = vec![
        proc(outer, 1, outer.pid, CODEX, &[]),
        proc(
            shell,
            outer.pid,
            shell.pid,
            "/bin/sh",
            &["sh", "-c", "custom-agent"],
        ),
        proc(
            custom_root,
            shell.pid,
            custom_root.pid,
            "/bin/custom-agent",
            &[],
        ),
    ];
    let before = attributor.update(
        &FakePlatform::default()
            .env(outer, outer_env)
            .env(shell, outer_env)
            .env(custom_root, outer_env),
        &mut table,
        now,
        0,
        false,
    );
    assert_eq!(
        attr(&before, custom_root).role,
        ProcessRole::Workload,
        "before the custom marker appears, the custom root is just an ordinary workload member \
         of the inherited outer codex session"
    );

    table.push(proc(
        child,
        custom_root.pid,
        child.pid,
        "/bin/sh",
        &["sh", "-c", "job"],
    ));
    let platform = FakePlatform::default()
        .env(outer, outer_env)
        .env(shell, outer_env)
        .env(custom_root, outer_env)
        .env(
            child,
            &[
                ("CODEX_THREAD_ID", "outer-session"),
                ("CUSTOM_SESSION", "custom-live"),
                ("CUSTOM_PID", "1952"),
            ],
        );
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );

    let outer_attr = attr(&after, outer);
    let shell_attr = attr(&after, shell);
    let custom_attr = attr(&after, custom_root);
    let child_attr = attr(&after, child);
    let outer_agent = outer_attr.agent_id.clone().unwrap();
    let custom_agent = custom_attr.agent_id.clone().unwrap();
    assert_ne!(custom_agent, outer_agent);
    assert_eq!(
        custom_attr.role,
        ProcessRole::AgentRoot,
        "a configured marker root nearer than the outer agent must win over the nearest-root \
         guard, even though it also carries the inherited outer marker"
    );
    assert_eq!(
        agent_of(&after, &custom_agent).kind,
        "custom",
        "the custom root's own agent must come from the CUSTOM_SESSION claim, not the inherited \
         codex one"
    );
    assert_eq!(
        agent_of(&after, &custom_agent).session_id.as_deref(),
        Some("custom-live")
    );
    assert_eq!(
        shell_attr.agent_id.as_deref(),
        Some(outer_agent.as_str()),
        "the outer shell, which carries only the inherited marker, must stay with the outer agent"
    );
    assert_eq!(
        child_attr.agent_id.as_deref(),
        Some(custom_agent.as_str()),
        "the child must resolve to its nearer custom root, not the farther inherited outer one"
    );
}

#[test]
fn a_late_discovered_standalone_custom_root_is_promoted_immediately_not_after_a_delay() {
    let root = id(1960, 1);
    let child = id(1961, 2);
    let marker = Marker {
        key: "CUSTOM_SESSION".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("custom".into()),
        root_pid_key: Some("CUSTOM_PID".into()),
        root_binaries: vec!["custom-agent".into()],
        session_id: true,
    };
    let mut attributor = Attributor::new(vec![marker], Vec::new());
    let now = Instant::now();
    let mut table = vec![proc(root, 1, root.pid, "/bin/custom-agent", &[])];
    let before = attributor.update(&FakePlatform::default(), &mut table, now, 0, false);
    assert_eq!(
        attr(&before, root).role,
        ProcessRole::Unattributed,
        "with no marker seen yet, the custom binary is cached as an ordinary unattributed process"
    );

    table.push(proc(
        child,
        root.pid,
        child.pid,
        "/bin/sh",
        &["sh", "-c", "job"],
    ));
    let platform = FakePlatform::default().env(
        child,
        &[("CUSTOM_SESSION", "custom-live"), ("CUSTOM_PID", "1960")],
    );
    for tick in 1..3u64 {
        let snapshot = attributor.update(
            &platform,
            &mut table,
            now + Duration::from_secs(tick),
            tick * 1000,
            false,
        );
        assert_eq!(
            attr(&snapshot, root).role,
            ProcessRole::AgentRoot,
            "tick={tick}: a newly established marker root must invalidate its own stale cached \
             role immediately, not remain unattributed"
        );
        assert_eq!(
            attr(&snapshot, child).role,
            ProcessRole::Workload,
            "tick={tick}: the marker-establishing child becomes a workload under the new root"
        );
    }
}

#[test]
fn a_failed_initial_age_sample_is_retried_instead_of_settling_permanently_as_young_batch() {
    let root = id(1970, 1);
    let shell = id(1971, 2);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-age-retry"),
        ("CLAUDE_PID", "1970"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 1970, CLAUDE, &[]),
        proc(shell, 1970, 1971, "/bin/sh", &["sh", "-c", "long-runner"]),
    ];

    // First sight: the age sample itself fails (no age registered on the fake platform), with a
    // known-empty port sample.
    let unknown_platform = FakePlatform::default().env(root, &env).ports(shell, &[]);
    let first = attributor.update(&unknown_platform, &mut processes, now, 0, false);
    let workload_id = attr(&first, shell).workload_id.clone().unwrap();
    assert_eq!(
        first
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Service,
        "an unknown age must stay provisionally service, not settle as batch"
    );

    // The age becomes readable later (past the retry cadence) and turns out to be old.
    let recovered_platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(3600));
    let recovered = attributor.update(
        &recovered_platform,
        &mut processes,
        now + Duration::from_secs(6),
        6000,
        false,
    );
    assert_eq!(
        recovered
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Service,
        "the retried age sample confirms an old process, which must classify as service"
    );
}

#[test]
fn a_stale_claude_pid_marker_from_before_pid_reuse_does_not_hijack_the_new_roots_session() {
    let old_root = id(600, 10);
    let parent = id(601, 20);
    let new_root = id(600, 30); // same pid as old_root, reused with a later start_time
    let child = id(602, 40);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let old_env = [
        ("CLAUDE_CODE_SESSION_ID", "old-session"),
        ("CLAUDE_PID", "600"),
    ];
    let first_platform = FakePlatform::default()
        .env(old_root, &old_env)
        .env(parent, &old_env);
    attributor.update(
        &first_platform,
        &mut [
            proc(old_root, 1, old_root.pid, CLAUDE, &[]),
            proc(parent, 1, parent.pid, "/usr/bin/worker", &[]),
        ],
        now,
        0,
        false,
    );

    let new_env = [
        ("CLAUDE_CODE_SESSION_ID", "new-session"),
        ("CLAUDE_PID", "600"),
    ];
    let second_platform = FakePlatform::default()
        .env(new_root, &new_env)
        .env(parent, &old_env)
        .env(child, &old_env);
    let after = attributor.update(
        &second_platform,
        &mut [
            proc(new_root, 1, new_root.pid, CLAUDE, &[]),
            proc(parent, 1, parent.pid, "/usr/bin/worker", &[]),
            proc(child, parent.pid, parent.pid, "/usr/bin/worker", &[]),
        ],
        now + Duration::from_secs(1),
        1000,
        false,
    );

    let new_root_agent = attr(&after, new_root).agent_id.clone().unwrap();
    let child_agent = attr(&after, child).agent_id.clone();
    assert_ne!(
        child_agent.as_deref(),
        Some(new_root_agent.as_str()),
        "a descendant born after PID reuse, carrying only a stale marker, must not be \
         reassigned to the unrelated new live root's session"
    );
    assert_eq!(
        agent_of(&after, &new_root_agent).session_id.as_deref(),
        Some("new-session"),
        "the new live root's own established session must never be overwritten by a stale marker"
    );
}

// ---------------------------------------------------------------------
// Coordinator-approved extension of finding 1: a rootless marker whose referenced root PID is
// itself enumerated but unreadable (not merely absent) must stay provisionally Unknown, with
// every member of that provisional context (root and descendants alike) held as AgentInternal
// -- never Workload -- until the root resolves one way or the other.
// ---------------------------------------------------------------------

#[test]
fn a_rootless_marker_with_an_unreadable_self_referenced_root_stays_unknown_until_resolved() {
    let claimant = id(2000, 1);
    let child = id(2001, 2);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-self"),
        ("CLAUDE_PID", "2000"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    // Tick 0: the claimant's own identity is enumerated but its exe/argv read failed, so it
    // cannot be confirmed as the claude binary its own CLAUDE_PID=self marker names. Table
    // presence beats the platform's confirmed-absent default (positive supplied table wins).
    let mut table = [
        proc_unreadable(claimant, 1, claimant.pid),
        proc(
            child,
            claimant.pid,
            child.pid,
            "/bin/sh",
            &["sh", "-c", "job"],
        ),
    ];
    let platform = FakePlatform::default().env(claimant, &env);
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let session_agent_id = attr(&first, claimant).agent_id.clone().unwrap();
    let agent = agent_of(&first, &session_agent_id);
    assert_eq!(agent.state, AgentState::Unknown);
    assert_eq!(
        agent.ended_at_ms, None,
        "no cleanup grace while the root is merely unreadable"
    );
    assert_eq!(agent.root, None);
    assert_eq!(
        attr(&first, claimant).role,
        ProcessRole::AgentInternal,
        "an unresolved self-referenced root must not present as AgentRoot"
    );
    assert!(attr(&first, claimant).workload_id.is_none());
    assert_eq!(
        attr(&first, child).role,
        ProcessRole::AgentInternal,
        "every member of a provisional-Unknown context, not just the root, stays AgentInternal"
    );
    assert!(attr(&first, child).workload_id.is_none());

    // Tick 1: the claimant's executable is now readable and really is claude, so the root
    // resolves and the agent promotes to a normal AgentRoot.
    table[0] = proc(claimant, 1, claimant.pid, CLAUDE, &[]);
    let second = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );

    let claimant_attr = attr(&second, claimant);
    assert_eq!(claimant_attr.role, ProcessRole::AgentRoot);
    let root_agent_id = claimant_attr.agent_id.clone().unwrap();
    assert_eq!(
        agent_of(&second, &root_agent_id).root,
        Some(claimant),
        "the promoted agent must carry the now-confirmed root identity"
    );
}

#[test]
fn a_rootless_marker_whose_referenced_pid_is_confirmed_gone_ends_and_its_detached_member_resolves_normally()
 {
    let worker = id(1990, 1); // detached: parented directly to pid 1
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-gone"),
        ("CLAUDE_PID", "9999"), // never enumerated in this test
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [proc(worker, 1, worker.pid, "/usr/bin/worker", &[])];

    // Tick 0: the referenced PID's enumeration is itself uncertain (not yet confirmed either
    // way) -- the agent must stay provisionally Unknown, and its only member AgentInternal.
    let uncertain_platform = FakePlatform::default()
        .env(worker, &env)
        .pid_presence(9999, None);
    let first = attributor.update(&uncertain_platform, &mut table, now, 0, false);
    let agent_id = attr(&first, worker).agent_id.clone().unwrap();
    assert_eq!(agent_of(&first, &agent_id).state, AgentState::Unknown);
    assert_eq!(agent_of(&first, &agent_id).ended_at_ms, None);
    assert_eq!(attr(&first, worker).role, ProcessRole::AgentInternal);
    assert!(attr(&first, worker).workload_id.is_none());

    // Tick 1: the platform now positively confirms the referenced PID is absent -- the agent
    // must end, with a fresh grace timestamp, and its detached member resolves as an ordinary
    // top-level workload rather than staying stuck.
    let confirmed_platform = FakePlatform::default()
        .env(worker, &env)
        .pid_presence(9999, Some(false));
    let second = attributor.update(
        &confirmed_platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let agent = agent_of(&second, &agent_id);
    assert_eq!(agent.state, AgentState::Ended);
    assert_eq!(
        agent.ended_at_ms,
        Some(1000),
        "confirmed absence must set a fresh grace timestamp"
    );
    let worker_attr = attr(&second, worker);
    assert_eq!(
        worker_attr.role,
        ProcessRole::Workload,
        "once the agent is confirmedly ended, its detached member must resolve as a normal \
         top-level workload, not stay stuck as AgentInternal"
    );
    assert!(worker_attr.workload_id.is_some());
}

// ---------------------------------------------------------------------
// Finding 3, coordinator-approved last case: a marker-referenced root whose own identity is
// enumerated but unreadable stays a provisional (Unknown) root -- itself and its descendants,
// marked or not, AgentInternal -- until its executable resolves either to the configured
// custom binary (promotion to AgentRoot) or to something else (falls back to ordinary ancestry
// resolution under whatever outer agent it is nested in).
// ---------------------------------------------------------------------

#[test]
fn an_unreadable_marker_referenced_root_stays_provisional_until_its_executable_resolves() {
    let outer = id(2010, 1);
    let shell = id(2011, 2);
    let custom_root = id(2012, 3);
    let sibling = id(2013, 4); // unmarked descendant, no markers of its own
    let marked_child = id(2014, 5); // carries the marker naming custom_root as its root
    let marker = Marker {
        key: "CUSTOM_SESSION".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("custom".into()),
        root_pid_key: Some("CUSTOM_PID".into()),
        root_binaries: vec!["custom-agent".into()],
        session_id: true,
    };
    let mut attributor = Attributor::new(vec![marker], Vec::new());
    let now = Instant::now();
    let outer_env: &[(&str, &str)] = &[("CODEX_THREAD_ID", "outer-session")];

    let mut table = vec![
        proc(outer, 1, outer.pid, CODEX, &[]),
        proc(
            shell,
            outer.pid,
            shell.pid,
            "/bin/sh",
            &["sh", "-c", "custom-agent"],
        ),
        proc_unreadable(custom_root, shell.pid, custom_root.pid),
        proc(
            sibling,
            custom_root.pid,
            sibling.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            marked_child,
            custom_root.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
    ];
    let platform = FakePlatform::default()
        .env(outer, outer_env)
        .env(shell, outer_env)
        .env(custom_root, outer_env)
        .env(sibling, outer_env)
        .env(
            marked_child,
            &[
                ("CODEX_THREAD_ID", "outer-session"),
                ("CUSTOM_SESSION", "custom-live"),
                ("CUSTOM_PID", "2012"),
            ],
        );
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let provisional_id = attr(&first, custom_root).agent_id.clone().unwrap();
    let provisional = agent_of(&first, &provisional_id);
    assert_eq!(provisional.state, AgentState::Unknown);
    assert_eq!(provisional.root, None);
    assert_eq!(provisional.ended_at_ms, None);
    for (label, p) in [
        ("custom_root", custom_root),
        ("sibling", sibling),
        ("marked_child", marked_child),
    ] {
        let a = attr(&first, p);
        assert_eq!(
            a.role,
            ProcessRole::AgentInternal,
            "{label} must stay AgentInternal while the root it descends from is provisional"
        );
        assert!(
            a.workload_id.is_none(),
            "{label} must not carry a workload while provisional"
        );
        assert_eq!(a.agent_id.as_deref(), Some(provisional_id.as_str()));
    }
    assert_eq!(
        attr(&first, shell).role,
        ProcessRole::Workload,
        "the outer shell is unaffected: it never descends from the provisional root"
    );

    // Tick 1: a brand new, entirely unmarked descendant spawns under the still-unreadable root
    // -- the executable is still None. It must land in exactly the same provisional context as
    // the members that were already there, not something transitional or different.
    let mid_child = id(2016, 6);
    table.push(proc(
        mid_child,
        custom_root.pid,
        mid_child.pid,
        "/usr/bin/worker",
        &[],
    ));
    let mid = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        attr(&mid, custom_root).agent_id.as_deref(),
        Some(provisional_id.as_str()),
        "still provisional: the root's own agent must not have changed"
    );
    let mid_child_attr = attr(&mid, mid_child);
    assert_eq!(
        mid_child_attr.role,
        ProcessRole::AgentInternal,
        "a descendant spawned while the root is still unreadable must land AgentInternal too"
    );
    assert!(mid_child_attr.workload_id.is_none());
    assert_eq!(
        mid_child_attr.agent_id.as_deref(),
        Some(provisional_id.as_str())
    );

    // Tick 2: the executable resolves to the configured custom binary, and a brand new
    // descendant spawns under it in the same tick.
    let late_child = id(2015, 7);
    table[2] = proc(
        custom_root,
        shell.pid,
        custom_root.pid,
        "/bin/custom-agent",
        &[],
    );
    table.push(proc(
        late_child,
        custom_root.pid,
        late_child.pid,
        "/bin/sh",
        &["sh", "-c", "job2"],
    ));
    let second = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );

    let custom_root_attr = attr(&second, custom_root);
    assert_eq!(
        custom_root_attr.role,
        ProcessRole::AgentRoot,
        "once the executable is confirmed as the configured custom binary, the root promotes"
    );
    let real_agent_id = custom_root_attr.agent_id.clone().unwrap();
    let real_agent = agent_of(&second, &real_agent_id);
    assert_eq!(real_agent.root, Some(custom_root));
    assert_eq!(real_agent.kind, "custom");
    assert_eq!(
        attr(&second, marked_child).agent_id.as_deref(),
        Some(real_agent_id.as_str()),
        "the marked child must merge onto the now-real root's agent, not the stale provisional one"
    );
    let late_child_attr = attr(&second, late_child);
    assert_eq!(
        late_child_attr.agent_id.as_deref(),
        Some(real_agent_id.as_str()),
        "a descendant spawned the same tick the root resolves must still join the real agent"
    );
    assert_eq!(
        late_child_attr.role,
        ProcessRole::Workload,
        "a freshly spawned tool-shell child of a confirmed AgentRoot is an ordinary workload"
    );
}

#[test]
fn a_marker_referenced_root_that_reads_as_a_non_agent_binary_falls_back_to_the_outer_workload() {
    let outer = id(2020, 1);
    let shell = id(2021, 2);
    let custom_root = id(2022, 3);
    let marked_child = id(2023, 4);
    let marker = Marker {
        key: "CUSTOM_SESSION".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("custom".into()),
        root_pid_key: Some("CUSTOM_PID".into()),
        root_binaries: vec!["custom-agent".into()],
        session_id: true,
    };
    let mut attributor = Attributor::new(vec![marker], Vec::new());
    let now = Instant::now();
    let outer_env: &[(&str, &str)] = &[("CODEX_THREAD_ID", "outer-session")];

    let mut table = vec![
        proc(outer, 1, outer.pid, CODEX, &[]),
        proc(
            shell,
            outer.pid,
            shell.pid,
            "/bin/sh",
            &["sh", "-c", "custom-agent"],
        ),
        proc_unreadable(custom_root, shell.pid, custom_root.pid),
        proc(
            marked_child,
            custom_root.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
    ];
    let platform = FakePlatform::default()
        .env(outer, outer_env)
        .env(shell, outer_env)
        .env(custom_root, outer_env)
        .env(
            marked_child,
            &[
                ("CODEX_THREAD_ID", "outer-session"),
                ("CUSTOM_SESSION", "custom-live"),
                ("CUSTOM_PID", "2022"),
            ],
        );
    attributor.update(&platform, &mut table, now, 0, false);

    // The executable resolves, but to an ordinary (non-agent) binary -- not the configured
    // custom-agent. The provisional root must clear, and both it and its marked descendant must
    // fall back to being ordinary members of whatever outer agent they are nested under.
    table[2] = proc(
        custom_root,
        shell.pid,
        custom_root.pid,
        "/usr/bin/plain-tool",
        &[],
    );
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );

    let outer_agent = attr(&after, outer).agent_id.clone().unwrap();
    let shell_workload = attr(&after, shell).workload_id.clone();
    let custom_root_attr = attr(&after, custom_root);
    let marked_child_attr = attr(&after, marked_child);
    assert_eq!(custom_root_attr.role, ProcessRole::Workload);
    assert_eq!(
        custom_root_attr.agent_id.as_deref(),
        Some(outer_agent.as_str())
    );
    assert_eq!(custom_root_attr.workload_id, shell_workload);
    assert_eq!(marked_child_attr.role, ProcessRole::Workload);
    assert_eq!(
        marked_child_attr.agent_id.as_deref(),
        Some(outer_agent.as_str())
    );
    assert_eq!(marked_child_attr.workload_id, shell_workload);
}

// ---------------------------------------------------------------------
// Finding 2, additional variant: the detached member is observed a full tick BEFORE the
// attached session marker even appears in the process table (not just a same-tick reordering).
// ---------------------------------------------------------------------

#[test]
fn a_detached_workload_seen_a_tick_before_the_attached_root_still_joins_its_live_session() {
    let root = id(1945, 1);
    let shell = id(1946, 2);
    let orphan = id(1947, 3);
    let platform = FakePlatform::default()
        .env(shell, &[("CODEX_THREAD_ID", "live-session")])
        .env(orphan, &[("CODEX_THREAD_ID", "live-session")]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    // Tick 0: only the detached orphan is enumerated; the root and its shell haven't appeared
    // yet (e.g. this scan raced their startup).
    let first = attributor.update(
        &platform,
        &mut [proc(orphan, 1, orphan.pid, "/usr/bin/worker", &[])],
        now,
        0,
        false,
    );
    let orphan_agent_before = attr(&first, orphan).agent_id.clone().unwrap();
    assert_eq!(
        agent_of(&first, &orphan_agent_before).state,
        AgentState::Ended,
        "with no root evidence at all yet, the rootless session agent starts out already-ended"
    );

    // Tick 1: the root and its session-revealing shell now appear, alongside the still-present
    // orphan.
    let mut table = [
        proc(root, 1, root.pid, CODEX, &[]),
        proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "work"]),
        proc(orphan, 1, orphan.pid, "/usr/bin/worker", &[]),
    ];
    let second = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let root_agent = attr(&second, root).agent_id.clone().unwrap();
    let orphan_agent_after = attr(&second, orphan).agent_id.clone().unwrap();
    assert_eq!(
        orphan_agent_after, root_agent,
        "the orphan, seen a whole tick before the live root, must still reconcile onto it"
    );
    assert_eq!(second.agents.len(), 1);
    assert_ne!(
        agent_of(&second, &root_agent).state,
        AgentState::Ended,
        "the reconciled agent must no longer be the earlier already-ended rootless placeholder"
    );
}

// ---------------------------------------------------------------------
// Finding 4, additional variants: bounded retry cadence, and both recovery outcomes.
// ---------------------------------------------------------------------

#[test]
fn a_failed_age_sample_is_not_retried_before_the_five_second_cadence() {
    let root = id(1972, 1);
    let shell = id(1973, 2);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-age-cadence"),
        ("CLAUDE_PID", "1972"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 1972, CLAUDE, &[]),
        proc(shell, 1972, 1973, "/bin/sh", &["sh", "-c", "long-runner"]),
    ];

    let unknown_platform = FakePlatform::default().env(root, &env).ports(shell, &[]);
    attributor.update(&unknown_platform, &mut processes, now, 0, false);
    assert_eq!(
        unknown_platform.age_read_count(shell),
        1,
        "first sight must sample age once"
    );

    // Well before the 5s cadence: must not retry yet.
    let still_unknown_platform = FakePlatform::default().env(root, &env).ports(shell, &[]);
    attributor.update(
        &still_unknown_platform,
        &mut processes,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        still_unknown_platform.age_read_count(shell),
        0,
        "must not retry before the bounded cadence elapses"
    );

    // Past the cadence, and now readable: the retry must fire exactly once.
    let recovered_platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(30));
    attributor.update(
        &recovered_platform,
        &mut processes,
        now + Duration::from_secs(6),
        6000,
        false,
    );
    assert_eq!(
        recovered_platform.age_read_count(shell),
        1,
        "the retry must fire once the cadence has elapsed"
    );
}

#[test]
fn a_recovered_young_age_settles_the_workload_to_batch_not_service() {
    let root = id(1974, 1);
    let shell = id(1978, 2);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-age-recover-young"),
        ("CLAUDE_PID", "1974"),
    ];
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut processes = [
        proc(root, 1, 1974, CLAUDE, &[]),
        proc(shell, 1974, 1978, "/bin/sh", &["sh", "-c", "short-runner"]),
    ];

    let unknown_platform = FakePlatform::default().env(root, &env).ports(shell, &[]);
    let first = attributor.update(&unknown_platform, &mut processes, now, 0, false);
    let workload_id = attr(&first, shell).workload_id.clone().unwrap();
    assert_eq!(
        first
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Service,
        "an unknown age must stay provisionally service"
    );

    let recovered_platform = FakePlatform::default()
        .env(root, &env)
        .ports(shell, &[])
        .age(shell, Duration::from_secs(30));
    let recovered = attributor.update(
        &recovered_platform,
        &mut processes,
        now + Duration::from_secs(6),
        6000,
        false,
    );
    assert_eq!(
        recovered
            .workloads
            .iter()
            .find(|w| w.id == workload_id)
            .unwrap()
            .class,
        WorkloadClass::Batch,
        "a retried age sample that turns out young must settle the workload to batch"
    );
}

// ---------------------------------------------------------------------
// Finding 1, additional variant: persistent uncertainty across many ticks must never accumulate
// into a confirmed exit; only positive evidence does.
// ---------------------------------------------------------------------

#[test]
fn persistently_uncertain_root_liveness_never_ends_the_agent_only_confirmed_exit_does() {
    let root = id(1935, 1);
    let shell = id(1936, 2);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut full = [
        proc(root, 1, root.pid, CODEX, &[]),
        proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "work"]),
    ];
    let first = attributor.update(&platform, &mut full, now, 0, false);
    let agent_id = attr(&first, root).agent_id.clone().unwrap();

    let mut without_root = [full[1].clone()];
    for tick in 1..5u64 {
        let snapshot = attributor.update(
            &platform,
            &mut without_root,
            now + Duration::from_secs(tick),
            tick * 1000,
            false,
        );
        let agent = agent_of(&snapshot, &agent_id);
        assert_ne!(
            agent.state,
            AgentState::Ended,
            "tick={tick}: repeated uncertain omissions must never accumulate into a confirmed exit"
        );
        assert_eq!(agent.ended_at_ms, None);
    }

    let gone_platform = FakePlatform::default().gone(root);
    let ended = attributor.update(
        &gone_platform,
        &mut without_root,
        now + Duration::from_secs(5),
        5000,
        false,
    );
    let agent = agent_of(&ended, &agent_id);
    assert_eq!(agent.state, AgentState::Ended);
    assert_eq!(agent.ended_at_ms, Some(5000));

    // The same root identity returns to the full table: a confirmed-ended agent must revive,
    // not stay ended forever now that its root is alive again.
    let revived = attributor.update(
        &platform,
        &mut full,
        now + Duration::from_secs(6),
        6000,
        false,
    );
    let agent = agent_of(&revived, &agent_id);
    assert_eq!(agent.state, AgentState::Unknown);
    assert_eq!(agent.ended_at_ms, None);
}

// ---------------------------------------------------------------------
// Fixup-2, uniform candidate resolution regressions (ticket 04 fixup-2 / re-review findings).
//
// A process whose own exe read succeeded but whose argv read failed, distinct from a fully
// unreadable process (`proc_unreadable`): the platform confirms the binary path but not argv[0].
// ---------------------------------------------------------------------

const CLAUDE_VERSIONS_PATH: &str =
    "/Users/dev/.claude/local/node_modules/@anthropic-ai/claude-code/claude/versions/1.2.3/cli.mjs";

fn proc_exe_only(identity: ProcessIdentity, ppid: i32, pgid: i32, exe: &str) -> Process {
    Process {
        identity,
        ppid,
        pgid,
        uid: own_uid(),
        stopped: false,
        name: None,
        exe: Some(exe.into()),
        argv: None,
        metrics: None,
    }
}

// ---------------------------------------------------------------------
// Finding 1 (P1): an ancestry-resolved candidate root -- no root_pid_key, found by walking
// ancestors for the nearest binary in the marker's root_binaries -- that is itself enumerated
// but unreadable must land in the same provisional-Unknown handling as an explicit root_pid_key
// reference: never a rootless Ended agent, and never absorbed as a plain Workload member of an
// outer tree it happens to be nested under.
// ---------------------------------------------------------------------

#[test]
fn an_unreadable_ancestry_resolved_root_stays_provisional_standalone() {
    let unreadable_codex = id(4500, 1);
    let marked_shell = id(4501, 2);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let env = [("CODEX_THREAD_ID", "inner-session")];
    let platform = FakePlatform::default().env(marked_shell, &env);

    let mut table = [
        proc_unreadable(unreadable_codex, 1, unreadable_codex.pid),
        proc(
            marked_shell,
            unreadable_codex.pid,
            marked_shell.pid,
            "/bin/sh",
            &["sh", "-c", "inner job"],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let agent_id = attr(&first, marked_shell).agent_id.clone().unwrap();
    let agent = agent_of(&first, &agent_id);
    assert_eq!(
        agent.state,
        AgentState::Unknown,
        "an unreadable ancestry-resolved candidate must not create a rootless Ended agent"
    );
    assert_eq!(agent.ended_at_ms, None);
    assert_eq!(agent.root, None);
    assert_eq!(
        attr(&first, unreadable_codex).role,
        ProcessRole::AgentInternal,
        "the unreadable candidate ancestor itself must not be dropped or absorbed elsewhere"
    );
    assert_eq!(
        attr(&first, marked_shell).role,
        ProcessRole::AgentInternal,
        "the marker-bearing process must stay AgentInternal while its candidate ancestor is unresolved"
    );
    assert!(attr(&first, marked_shell).workload_id.is_none());

    // Repeated unchanged tick: the provisional state must not drift or flip back on its own.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        attr(&repeated, unreadable_codex).role,
        ProcessRole::AgentInternal
    );
    assert_eq!(
        attr(&repeated, marked_shell).role,
        ProcessRole::AgentInternal
    );
    assert_eq!(agent_of(&repeated, &agent_id).state, AgentState::Unknown);
    assert_eq!(agent_of(&repeated, &agent_id).ended_at_ms, None);

    // Recovery: the executable resolves to the real codex binary.
    table[0] = proc(unreadable_codex, 1, unreadable_codex.pid, CODEX, &[]);
    let recovered = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let root_attr = attr(&recovered, unreadable_codex);
    assert_eq!(root_attr.role, ProcessRole::AgentRoot);
    let root_agent_id = root_attr.agent_id.clone().unwrap();
    assert_eq!(
        agent_of(&recovered, &root_agent_id).root,
        Some(unreadable_codex)
    );
    assert_eq!(
        attr(&recovered, marked_shell).role,
        ProcessRole::Workload,
        "once the root is confirmed real, its marker-bearing tool-call-shell child becomes an ordinary workload"
    );
}

#[test]
fn an_unreadable_ancestry_resolved_root_stays_provisional_nested_under_an_outer_agent() {
    // claude(root) -> sh -c (outer workload) -> unreadable codex -> marked shell. The hidden
    // codex candidate and its marked child must not be absorbed as Workload members of the
    // outer claude shell; they must form their own distinct provisional context.
    let claude_root = id(4510, 1);
    let outer_shell = id(4511, 2);
    let unreadable_codex = id(4512, 3);
    let marked_shell = id(4513, 4);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let claude_env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-nested-outer"),
        ("CLAUDE_PID", "4510"),
    ];
    let codex_env = [("CODEX_THREAD_ID", "inner-nested")];
    let platform = FakePlatform::default()
        .env(claude_root, &claude_env)
        .env(marked_shell, &codex_env);

    let mut table = [
        proc(claude_root, 1, claude_root.pid, CLAUDE, &[]),
        proc(
            outer_shell,
            claude_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run codex"],
        ),
        proc_unreadable(unreadable_codex, outer_shell.pid, unreadable_codex.pid),
        proc(
            marked_shell,
            unreadable_codex.pid,
            marked_shell.pid,
            "/bin/sh",
            &["sh", "-c", "inner job"],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let claude_attr = attr(&first, claude_root);
    let outer_shell_attr = attr(&first, outer_shell);
    assert_eq!(outer_shell_attr.role, ProcessRole::Workload);
    assert_eq!(outer_shell_attr.agent_id, claude_attr.agent_id);

    let codex_root_attr = attr(&first, unreadable_codex);
    let marked_shell_attr = attr(&first, marked_shell);
    assert_eq!(
        codex_root_attr.role,
        ProcessRole::AgentInternal,
        "the hidden codex candidate must not be absorbed as a Workload of the outer claude shell"
    );
    assert_ne!(
        codex_root_attr.agent_id, claude_attr.agent_id,
        "an unreadable candidate ancestor still starts its own provisional context, distinct from the outer agent"
    );
    assert_eq!(
        marked_shell_attr.role,
        ProcessRole::AgentInternal,
        "the marker-bearing descendant must also stay out of the outer workload"
    );
    assert_eq!(marked_shell_attr.agent_id, codex_root_attr.agent_id);
    let inner_agent_id = codex_root_attr.agent_id.clone().unwrap();
    assert_eq!(agent_of(&first, &inner_agent_id).state, AgentState::Unknown);
    assert_eq!(agent_of(&first, &inner_agent_id).ended_at_ms, None);

    // Repeated unchanged tick.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        attr(&repeated, unreadable_codex).role,
        ProcessRole::AgentInternal
    );
    assert_eq!(
        attr(&repeated, marked_shell).role,
        ProcessRole::AgentInternal
    );

    // Recovery: the hidden codex process resolves to the real binary.
    table[2] = proc(
        unreadable_codex,
        outer_shell.pid,
        unreadable_codex.pid,
        CODEX,
        &[],
    );
    let recovered = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let recovered_codex_attr = attr(&recovered, unreadable_codex);
    assert_eq!(recovered_codex_attr.role, ProcessRole::AgentRoot);
    assert_eq!(
        agent_of(&recovered, &recovered_codex_attr.agent_id.clone().unwrap()).root,
        Some(unreadable_codex)
    );
    assert_eq!(attr(&recovered, marked_shell).role, ProcessRole::Workload);
}

// ---------------------------------------------------------------------
// Finding 2 (P2): entering provisional Unknown must invalidate every process cached under the
// demoted agent, including a detached process that carries no marker of its own and only
// inherited its workload through ancestry.
// ---------------------------------------------------------------------

#[test]
fn provisional_unknown_invalidates_a_detached_unmarked_cached_member() {
    let wrapper = id(4600, 1);
    let marked_member = id(4601, 1);
    let unmarked_child = id(4602, 1);
    let marker = Marker {
        key: "CUSTOM_SESSION".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("custom".into()),
        root_pid_key: Some("CUSTOM_PID".into()),
        root_binaries: vec!["custom-agent".into()],
        session_id: true,
    };
    let mut attributor = Attributor::new(vec![marker], Vec::new());
    let now = Instant::now();
    let member_env = [
        ("CUSTOM_SESSION", "custom-detached"),
        ("CUSTOM_PID", "4600"),
    ];
    let platform = FakePlatform::default().env(marked_member, &member_env);

    // Tick 0: the referenced wrapper is positively identified but is not the configured
    // custom-agent binary, so it is rejected as root. The marker-bearing process forms a
    // rootless Ended workload, and its plain child inherits that same workload by ancestry.
    let mut table = vec![
        proc(wrapper, 1, wrapper.pid, "/usr/bin/wrapper", &[]),
        proc(marked_member, 1, marked_member.pid, "/usr/bin/carrier", &[]),
        proc(
            unmarked_child,
            marked_member.pid,
            marked_member.pid,
            "/usr/bin/worker",
            &[],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let agent_id = attr(&first, marked_member).agent_id.clone().unwrap();
    assert_eq!(agent_of(&first, &agent_id).state, AgentState::Ended);
    assert_eq!(attr(&first, marked_member).role, ProcessRole::Workload);
    let marked_member_workload = attr(&first, marked_member).workload_id.clone();
    assert!(marked_member_workload.is_some());
    assert_eq!(
        attr(&first, unmarked_child).workload_id,
        marked_member_workload,
        "the plain child must inherit its marked parent's workload"
    );

    // Tick 1: the unmarked child is reparented to pid 1, still alive, nothing else changes --
    // it must keep its existing attribution rather than becoming a fresh detached process.
    table[2] = proc(unmarked_child, 1, marked_member.pid, "/usr/bin/worker", &[]);
    let reparented = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(
        attr(&reparented, unmarked_child).workload_id,
        marked_member_workload,
        "reparenting to pid 1 must not disturb the retained attribution before the trigger"
    );

    // Tick 2: the referenced wrapper becomes unreadable -- the agent must demote to provisional
    // Unknown, and every cached member, including the detached unmarked child, must invalidate
    // out of its stale Workload role.
    table[0] = proc_unreadable(wrapper, 1, wrapper.pid);
    let demoted = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let agent = agent_of(&demoted, &agent_id);
    assert_eq!(agent.state, AgentState::Unknown);
    assert_eq!(agent.ended_at_ms, None);
    assert_eq!(attr(&demoted, wrapper).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&demoted, marked_member).role,
        ProcessRole::AgentInternal
    );
    let unmarked_child_attr = attr(&demoted, unmarked_child);
    assert_eq!(
        unmarked_child_attr.role,
        ProcessRole::AgentInternal,
        "a detached, unmarked cached member must invalidate out of Workload too, not just the referenced candidate and the marked claimant"
    );
    assert!(
        unmarked_child_attr.workload_id.is_none(),
        "no member of a rootless Unknown agent may keep an actionable workload id"
    );

    // Repeated unchanged tick: the invalidation must hold, not just fire once and drift back.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(3),
        3000,
        false,
    );
    assert_eq!(
        attr(&repeated, unmarked_child).role,
        ProcessRole::AgentInternal
    );
    assert!(attr(&repeated, unmarked_child).workload_id.is_none());

    // Tick 4: the wrapper resolves again, but to another non-agent binary (a never-validated
    // candidate confirmed as ordinary). Per the deliberate fixup-3 semantics there is no saved
    // pre-provisional context to restore: every cached member re-resolves from current evidence
    // only. The marked claimant still carries its own marker, so it lands back on the same
    // (deterministic, identity-keyed) workload id purely by re-deriving it fresh -- not because
    // anything was restored. The unmarked, detached child carries no marker and, once reparented
    // off any ancestry path back to the claimant, has no current evidence connecting it to
    // anything: it must come back Unattributed, not reattached to a workload it has no live
    // relationship to. The wrapper itself -- never a root of anything, only ever a rejected
    // candidate -- also returns Unattributed.
    table[0] = proc(wrapper, 1, wrapper.pid, "/usr/bin/wrapper-v2", &[]);
    let returned = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(4),
        4000,
        false,
    );
    assert_eq!(
        attr(&returned, wrapper).role,
        ProcessRole::Unattributed,
        "a wrapper that resolves to another non-agent binary was never a root of anything and must return Unattributed, not a leftover Workload"
    );
    assert!(attr(&returned, wrapper).agent_id.is_none());
    assert_eq!(
        attr(&returned, marked_member).role,
        ProcessRole::Workload,
        "the marked claimant re-derives its workload from its own current marker evidence"
    );
    assert_eq!(
        attr(&returned, marked_member).workload_id,
        marked_member_workload,
        "workload ids are a pure function of the workload root's identity, so a freshly derived \
         workload for the same claimant lands on the same id -- not because anything was restored"
    );
    let unmarked_child_returned = attr(&returned, unmarked_child);
    assert_eq!(
        unmarked_child_returned.role,
        ProcessRole::Unattributed,
        "an unmarked, detached member has no current evidence connecting it to the marked \
         claimant's agent once provisional context is no longer saved: it must not be silently \
         reattached to a workload it has no live ancestry or marker relationship to"
    );
    assert!(unmarked_child_returned.agent_id.is_none());
    assert!(unmarked_child_returned.workload_id.is_none());

    // Repeated unchanged tick: the Unattributed state must hold, not drift back on its own.
    let returned_repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(5),
        5000,
        false,
    );
    assert_eq!(
        attr(&returned_repeated, wrapper).role,
        ProcessRole::Unattributed
    );
    assert_eq!(
        attr(&returned_repeated, marked_member).role,
        ProcessRole::Workload
    );
    assert_eq!(
        attr(&returned_repeated, marked_member).workload_id,
        marked_member_workload
    );
    assert_eq!(
        attr(&returned_repeated, unmarked_child).role,
        ProcessRole::Unattributed
    );
    assert!(
        attr(&returned_repeated, unmarked_child)
            .workload_id
            .is_none()
    );
}

/// The detached-history exception (`Cached::detached`) is not specific to reparenting onto pid
/// 1: any observed ppid change -- including onto some other still-live process that is itself
/// entirely `Unattributed` (a subreaper standing in after the original parent exited) -- must
/// retain a live member's established attribution rather than reset it just because the new
/// parent happens to carry no agent of its own.
#[test]
fn a_live_workload_reparented_onto_an_unattributed_subreaper_keeps_its_retained_attribution() {
    let root = id(700, 1);
    let workload_shell = id(701, 1);
    let subreaper = id(702, 1);
    let platform = FakePlatform::default().env(
        root,
        &[
            ("CLAUDE_CODE_SESSION_ID", "sess-subreaper"),
            ("CLAUDE_PID", "700"),
        ],
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut table = [
        proc(root, 1, 700, CLAUDE, &[]),
        proc(
            workload_shell,
            700,
            701,
            "/bin/bash",
            &["bash", "-c", "npm test"],
        ),
        proc(subreaper, 1, 702, "/usr/bin/unrelated-supervisor", &[]),
    ];
    let first = attributor.update(&platform, &mut table, Instant::now(), 0, false);
    let workload_attr = attr(&first, workload_shell);
    assert_eq!(workload_attr.role, ProcessRole::Workload);
    let agent_id = workload_attr.agent_id.clone().unwrap();
    let workload_id = workload_attr.workload_id.clone().unwrap();
    assert_eq!(
        attr(&first, subreaper).role,
        ProcessRole::Unattributed,
        "the subreaper standing in below must itself carry no agent for this case to mean \
         anything"
    );

    // The shell's original parent exits; it is reparented onto the unrelated, itself
    // unattributed subreaper process -- not pid 1 -- while remaining alive and otherwise
    // unchanged (same exe/argv, same identity).
    table[1] = proc(
        workload_shell,
        subreaper.pid,
        701,
        "/bin/bash",
        &["bash", "-c", "npm test"],
    );
    let reparented = attributor.update(
        &platform,
        &mut table,
        Instant::now() + Duration::from_secs(1),
        1000,
        false,
    );
    let reparented_attr = attr(&reparented, workload_shell);
    assert_eq!(
        reparented_attr.role,
        ProcessRole::Workload,
        "a live process reparented onto an unrelated, itself-unattributed subreaper must keep \
         its established attribution, exactly like the existing reparent-onto-pid-1 case, not \
         reset just because its new immediate parent carries no agent"
    );
    assert_eq!(reparented_attr.agent_id.as_deref(), Some(agent_id.as_str()));
    assert_eq!(reparented_attr.workload_id, Some(workload_id));
}

// ---------------------------------------------------------------------
// Finding 3 (P2): positive evidence about a pending reference's PID -- a replacement identity
// that started too late to be the referenced ancestor, or one owned by a foreign uid -- must
// resolve the pending Unknown reference to invalid, not wait indefinitely on the replacement's
// own executable readability.
// ---------------------------------------------------------------------

/// Starting shape for the pending reference before it gets disqualified: either the referenced
/// PID is already enumerated with a known (but unreadable) identity, or it never made it into
/// the process table at all and only the platform's own presence check vouches for it -- the
/// exact shape the original finding 3 reproduction used (PID present, identity itself
/// unreadable/unenumerated).
#[derive(Clone, Copy)]
enum PendingStart {
    KnownUnreadableIdentity,
    NoTableEntryPidPresenceOnly,
}
const PENDING_STARTS: [PendingStart; 2] = [
    PendingStart::KnownUnreadableIdentity,
    PendingStart::NoTableEntryPidPresenceOnly,
];

#[test]
fn a_pending_reference_replaced_by_a_too_new_identity_resolves_invalid() {
    let stale_ref_pid = 4700;
    let old_identity = id(stale_ref_pid, 10);
    let worker = id(4701, 20); // started after old_identity: a valid ancestor ordering
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-pending-reuse"),
        ("CLAUDE_PID", "4700"),
    ];

    for start in PENDING_STARTS {
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let mut table = match start {
            PendingStart::KnownUnreadableIdentity => vec![
                proc_unreadable(old_identity, 1, old_identity.pid),
                proc(worker, 1, worker.pid, "/usr/bin/worker", &[]),
            ],
            PendingStart::NoTableEntryPidPresenceOnly => {
                vec![proc(worker, 1, worker.pid, "/usr/bin/worker", &[])]
            }
        };
        let platform0 = match start {
            PendingStart::KnownUnreadableIdentity => FakePlatform::default().env(worker, &env),
            PendingStart::NoTableEntryPidPresenceOnly => FakePlatform::default()
                .env(worker, &env)
                .pid_presence(stale_ref_pid, Some(true)),
        };

        // Tick 0: the referenced PID is uncertain, either an enumerated-but-unreadable identity
        // or entirely absent from the table with only positive platform presence vouching for
        // it -- either way the agent must stay provisionally Unknown, the worker AgentInternal.
        let first = attributor.update(&platform0, &mut table, now, 0, false);
        let agent_id = attr(&first, worker).agent_id.clone().unwrap();
        assert_eq!(agent_of(&first, &agent_id).state, AgentState::Unknown);
        assert_eq!(attr(&first, worker).role, ProcessRole::AgentInternal);

        // Tick 1: the same PID now belongs to a newer, still-unreadable process -- too young to
        // be the worker's ancestor. This positively disqualifies the reference regardless of the
        // replacement's own readability, and regardless of how tick 0 represented the prior
        // uncertainty.
        let too_new = id(stale_ref_pid, 30);
        let mut table = vec![
            proc_unreadable(too_new, 1, too_new.pid),
            proc(worker, 1, worker.pid, "/usr/bin/worker", &[]),
        ];
        let platform1 = FakePlatform::default().env(worker, &env);
        let resolved = attributor.update(
            &platform1,
            &mut table,
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let agent = agent_of(&resolved, &agent_id);
        assert_eq!(
            agent.state,
            AgentState::Ended,
            "a replacement identity that started after the claimant must disqualify the pending reference outright"
        );
        assert_eq!(
            agent.ended_at_ms,
            Some(1000),
            "disqualification must set a fresh cleanup timestamp, not stay stuck waiting on the unrelated replacement's readability"
        );
        assert_eq!(
            attr(&resolved, worker).role,
            ProcessRole::Workload,
            "once disqualified, the worker resolves as an ordinary rootless workload member"
        );

        // Repeated unchanged ticks: must not flip back to Unknown while the replacement stays
        // unreadable and present.
        for tick in 2..4u64 {
            let repeated = attributor.update(
                &platform1,
                &mut table,
                now + Duration::from_secs(tick),
                tick * 1000,
                false,
            );
            assert_eq!(agent_of(&repeated, &agent_id).state, AgentState::Ended);
            assert_eq!(attr(&repeated, worker).role, ProcessRole::Workload);
        }
    }
}

#[test]
fn a_pending_reference_replaced_by_a_foreign_uid_identity_resolves_invalid() {
    let ref_pid = 4800;
    let old_identity = id(ref_pid, 10);
    let worker = id(4801, 20);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-pending-foreign"),
        ("CLAUDE_PID", "4800"),
    ];

    for start in PENDING_STARTS {
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let mut table = match start {
            PendingStart::KnownUnreadableIdentity => vec![
                proc_unreadable(old_identity, 1, old_identity.pid),
                proc(worker, 1, worker.pid, "/usr/bin/worker", &[]),
            ],
            PendingStart::NoTableEntryPidPresenceOnly => {
                vec![proc(worker, 1, worker.pid, "/usr/bin/worker", &[])]
            }
        };
        let platform0 = match start {
            PendingStart::KnownUnreadableIdentity => FakePlatform::default().env(worker, &env),
            PendingStart::NoTableEntryPidPresenceOnly => FakePlatform::default()
                .env(worker, &env)
                .pid_presence(ref_pid, Some(true)),
        };
        let first = attributor.update(&platform0, &mut table, now, 0, false);
        let agent_id = attr(&first, worker).agent_id.clone().unwrap();
        assert_eq!(agent_of(&first, &agent_id).state, AgentState::Unknown);

        // Tick 1: the same PID (same start_time, ordering still plausible) now belongs to a
        // foreign uid's process, still unreadable. A foreign uid can never be this daemon's own
        // agent root, no matter what its executable turns out to be, and regardless of how
        // tick 0 represented the prior uncertainty.
        let foreign_identity = id(ref_pid, 10);
        let mut table = vec![
            Process {
                identity: foreign_identity,
                ppid: 1,
                pgid: foreign_identity.pid,
                uid: own_uid().wrapping_add(1),
                stopped: false,
                name: None,
                exe: None,
                argv: None,
                metrics: None,
            },
            proc(worker, 1, worker.pid, "/usr/bin/worker", &[]),
        ];
        let platform1 = FakePlatform::default().env(worker, &env);
        let resolved = attributor.update(
            &platform1,
            &mut table,
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let agent = agent_of(&resolved, &agent_id);
        assert_eq!(
            agent.state,
            AgentState::Ended,
            "a foreign-uid replacement must disqualify the pending reference outright"
        );
        assert_eq!(agent.ended_at_ms, Some(1000));
        assert_eq!(attr(&resolved, worker).role, ProcessRole::Workload);
    }
}

// ---------------------------------------------------------------------
// Coordinator addendum: an already-established Valid root's evidence must survive later
// uncertain or disqualifying observations along one uniform validation path, the same one the
// findings above exercise -- carried through unknown observations, invalidated only by positive
// evidence, and its agent identity preserved either way.
// ---------------------------------------------------------------------

#[test]
fn an_established_valid_root_retains_its_role_and_agent_across_a_later_unreadable_observation() {
    let root = id(4900, 1);
    let child = id(4901, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-established"),
        ("CLAUDE_PID", "4900"),
    ];
    let platform = FakePlatform::default().env(root, &env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc(root, 1, root.pid, CLAUDE, &[]),
        proc(child, root.pid, child.pid, "/usr/local/bin/mcp-server", &[]),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let agent_id = attr(&first, root).agent_id.clone().unwrap();
    assert_eq!(attr(&first, root).role, ProcessRole::AgentRoot);
    assert_eq!(agent_of(&first, &agent_id).root, Some(root));

    // The root's executable read fails this tick (same identity: a transient permission race,
    // not an exit and not an exec into anything else). Already-established evidence must
    // survive it, not regress into provisional handling on every transient read failure.
    table[0] = proc_unreadable(root, 1, root.pid);
    let second = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let root_attr = attr(&second, root);
    assert_eq!(
        root_attr.role,
        ProcessRole::AgentRoot,
        "a transient read failure on an already-validated root must not demote it"
    );
    assert_eq!(root_attr.agent_id.as_deref(), Some(agent_id.as_str()));
    let agent = agent_of(&second, &agent_id);
    assert_eq!(agent.root, Some(root));
    assert_eq!(
        attr(&second, child).agent_id.as_deref(),
        Some(agent_id.as_str()),
        "membership under the established root must not be disturbed by its own transient unreadability"
    );
    assert_eq!(attr(&second, child).role, ProcessRole::AgentInternal);

    // Repeated unchanged tick.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    assert_eq!(attr(&repeated, root).role, ProcessRole::AgentRoot);
    assert_eq!(agent_of(&repeated, &agent_id).root, Some(root));
}

#[test]
fn an_established_valid_root_that_execs_into_a_non_agent_binary_ends_the_agent_but_preserves_its_identity()
 {
    let root = id(4910, 1);
    let child = id(4911, 1);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-exec-away"),
        ("CLAUDE_PID", "4910"),
    ];
    let platform = FakePlatform::default().env(root, &env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc(root, 1, root.pid, CLAUDE, &[]),
        proc(child, root.pid, child.pid, "/usr/local/bin/mcp-server", &[]),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let agent_id = attr(&first, root).agent_id.clone().unwrap();
    let session_id = agent_of(&first, &agent_id).session_id.clone();
    assert_eq!(attr(&first, root).role, ProcessRole::AgentRoot);

    // Same identity execs into an ordinary, positively-read non-agent binary: no more transient
    // uncertainty, this is confirmed disqualifying evidence.
    table[0] = proc(root, 1, root.pid, "/usr/bin/plain-tool", &[]);
    let second = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let agent = agent_of(&second, &agent_id);
    assert_eq!(
        agent.state,
        AgentState::Ended,
        "a confirmed non-agent exec must invalidate the root association and start the grace period"
    );
    assert_eq!(agent.ended_at_ms, Some(1000));
    assert_eq!(
        agent.id, agent_id,
        "the agent's own identity must be preserved, not replaced with a fresh one"
    );
    assert_eq!(
        agent.root, None,
        "the invalidated root association must be cleared from the agent"
    );
    assert_eq!(
        agent.session_id, session_id,
        "the session must be preserved across root invalidation"
    );
    assert_eq!(
        attr(&second, root).role,
        ProcessRole::Workload,
        "once its agent is confirmed ended, the former root resolves as an ordinary leftover workload, like any other detached process under an ended agent"
    );
    assert_eq!(
        attr(&second, child).agent_id.as_deref(),
        Some(agent_id.as_str()),
        "an already-attributed leftover child must keep its attribution through the grace period"
    );
    assert_eq!(
        attr(&second, child).role,
        ProcessRole::Workload,
        "the leftover child joins the same leftover workload as its former-root parent"
    );
    assert_eq!(
        attr(&second, child).workload_id,
        attr(&second, root).workload_id
    );
}

#[test]
fn an_established_valid_root_execing_into_another_recognized_agent_binary_stays_valid() {
    // A live claude root, installed under the versions path, in-place upgrades its own binary
    // (e.g. a self-update) to a newer versions-path build. Both representations are recognized
    // claude binaries: validity, role and agent identity must all carry through unchanged.
    let root = id(4920, 1);
    let child = id(4921, 1);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc(root, 1, root.pid, "/usr/bin/claude", &[]),
        proc(child, root.pid, child.pid, "/usr/local/bin/mcp-server", &[]),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let agent_id = attr(&first, root).agent_id.clone().unwrap();
    assert_eq!(attr(&first, root).role, ProcessRole::AgentRoot);
    assert_eq!(agent_of(&first, &agent_id).kind, "claude");

    table[0] = proc(
        root,
        1,
        root.pid,
        CLAUDE_VERSIONS_PATH,
        &["claude", "-p", "hello"],
    );
    let second = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let root_attr = attr(&second, root);
    assert_eq!(
        root_attr.role,
        ProcessRole::AgentRoot,
        "exec'ing into a different but still-recognized claude binary must not lose validity"
    );
    assert_eq!(
        root_attr.agent_id.as_deref(),
        Some(agent_id.as_str()),
        "the same recognized-kind exec must not be treated as a brand new agent"
    );
    assert_eq!(agent_of(&second, &agent_id).root, Some(root));
    assert_eq!(
        attr(&second, child).agent_id.as_deref(),
        Some(agent_id.as_str()),
        "membership must carry through an in-kind binary upgrade too"
    );
}

// ---------------------------------------------------------------------
// Coordinator addendum: `binary()` now reports a versions-path root with an unreadable argv[0]
// as genuinely unresolved (None), not a confirmed non-agent basename fallback. A first-seen
// versions-path process whose argv read failed, referenced by an explicit CLAUDE_PID claimant,
// must stay provisional until argv confirms (or refutes) the claude invocation.
// ---------------------------------------------------------------------

#[test]
fn a_versions_path_root_with_unreadable_argv_stays_provisional_until_argv_confirms_claude() {
    let root = id(4950, 1);
    let child = id(4951, 2);
    let env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-versions-argv"),
        ("CLAUDE_PID", "4950"),
    ];
    let platform = FakePlatform::default().env(child, &env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc_exe_only(root, 1, root.pid, CLAUDE_VERSIONS_PATH),
        proc(child, root.pid, child.pid, "/usr/local/bin/mcp-server", &[]),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let agent_id = attr(&first, child).agent_id.clone().unwrap();
    let agent = agent_of(&first, &agent_id);
    assert_eq!(
        agent.state,
        AgentState::Unknown,
        "an exe-path-readable but argv-unreadable versions-path candidate must stay provisional, not resolve to invalid via a basename fallback"
    );
    assert_eq!(agent.ended_at_ms, None);
    assert_eq!(agent.root, None);
    assert_eq!(attr(&first, root).role, ProcessRole::AgentInternal);
    assert_eq!(attr(&first, child).role, ProcessRole::AgentInternal);
    assert!(attr(&first, child).workload_id.is_none());

    // Repeated unchanged tick.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(attr(&repeated, root).role, ProcessRole::AgentInternal);
    assert_eq!(agent_of(&repeated, &agent_id).state, AgentState::Unknown);

    // Recovery: argv becomes readable and confirms the claude invocation.
    table[0] = proc(
        root,
        1,
        root.pid,
        CLAUDE_VERSIONS_PATH,
        &["claude", "-p", "hello"],
    );
    let recovered = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let root_attr = attr(&recovered, root);
    assert_eq!(root_attr.role, ProcessRole::AgentRoot);
    let root_agent_id = root_attr.agent_id.clone().unwrap();
    assert_eq!(agent_of(&recovered, &root_agent_id).root, Some(root));
    assert_eq!(
        attr(&recovered, child).role,
        ProcessRole::AgentInternal,
        "a plain (non-tool-shell) child of the now-confirmed root stays agent-internal"
    );
}

// ---------------------------------------------------------------------
// Fixup-2 review finding 1 (P1): a versioned-Claude candidate must become a candidate purely from
// the executable-rule match, even with no agent-level marker anywhere in the tree to supply it
// through a different route. Before the fix, root discovery only recorded a binary claim when
// `agent_kind` returned a confirmed match; an unreadable argv made `agent_kind` return None, so
// no claim was ever recorded, and ordinary ancestry resolution silently absorbed the real agent
// process as a plain member of whatever outer workload it happened to be nested under.
// ---------------------------------------------------------------------

#[test]
fn a_marker_free_versioned_candidate_with_unreadable_argv_stays_provisional_nested_under_an_outer_workload()
 {
    // codex(root) -> sh -c (outer workload) -> versioned-claude, argv unreadable, no marker
    // anywhere in the tree. The versioned process must not be laundered into the outer Codex
    // workload merely because it carries no marker and its own binary match is unconfirmed.
    let codex_root = id(4960, 1);
    let outer_shell = id(4961, 2);
    let versioned = id(4962, 3);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc(codex_root, 1, codex_root.pid, CODEX, &[]),
        proc(
            outer_shell,
            codex_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run"],
        ),
        proc_exe_only(
            versioned,
            outer_shell.pid,
            versioned.pid,
            CLAUDE_VERSIONS_PATH,
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let codex_attr = attr(&first, codex_root);
    assert_eq!(codex_attr.role, ProcessRole::AgentRoot);
    assert_eq!(attr(&first, outer_shell).role, ProcessRole::Workload);
    assert_eq!(attr(&first, outer_shell).agent_id, codex_attr.agent_id);

    let versioned_attr = attr(&first, versioned);
    assert_eq!(
        versioned_attr.role,
        ProcessRole::AgentInternal,
        "an unreadable-argv versioned candidate must not be absorbed as a plain Workload member \
         of the outer Codex shell merely because it has no marker of its own"
    );
    assert_ne!(
        versioned_attr.agent_id, codex_attr.agent_id,
        "the uncertain candidate starts its own provisional context, distinct from the outer agent"
    );
    let versioned_agent_id = versioned_attr.agent_id.clone().unwrap();
    assert_eq!(
        agent_of(&first, &versioned_agent_id).state,
        AgentState::Unknown,
        "an unread argv must not resolve the candidate to invalid nor drop it entirely"
    );
    assert_eq!(agent_of(&first, &versioned_agent_id).root, None);
    assert_eq!(agent_of(&first, &versioned_agent_id).ended_at_ms, None);

    // Repeated unchanged tick: the provisional state must not drift or flip back on its own.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(attr(&repeated, versioned).role, ProcessRole::AgentInternal);
    assert_eq!(
        agent_of(&repeated, &versioned_agent_id).state,
        AgentState::Unknown
    );

    // Recovery: argv becomes readable and confirms the claude invocation.
    table[2] = proc(
        versioned,
        outer_shell.pid,
        versioned.pid,
        CLAUDE_VERSIONS_PATH,
        &["claude", "-p", "hello"],
    );
    let recovered = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let recovered_attr = attr(&recovered, versioned);
    assert_eq!(recovered_attr.role, ProcessRole::AgentRoot);
    let recovered_agent_id = recovered_attr.agent_id.clone().unwrap();
    assert_eq!(
        agent_of(&recovered, &recovered_agent_id).root,
        Some(versioned)
    );
}

#[test]
fn a_marker_free_versioned_candidate_whose_argv_resolves_to_a_non_agent_binary_falls_back_to_the_outer_workload()
 {
    // Same shape, but the recovered argv reveals an ordinary program, not claude: the candidate
    // must invalidate cleanly and the process must fall back to ordinary ancestry resolution
    // (membership in the outer Codex workload), not stay stuck provisional.
    let codex_root = id(4970, 1);
    let outer_shell = id(4971, 2);
    let versioned = id(4972, 3);
    let platform = FakePlatform::default();
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc(codex_root, 1, codex_root.pid, CODEX, &[]),
        proc(
            outer_shell,
            codex_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run"],
        ),
        proc_exe_only(
            versioned,
            outer_shell.pid,
            versioned.pid,
            CLAUDE_VERSIONS_PATH,
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    assert_eq!(attr(&first, versioned).role, ProcessRole::AgentInternal);

    // The path matches the versioned-Claude pattern but argv0 names another interpreter.
    table[2] = proc(
        versioned,
        outer_shell.pid,
        versioned.pid,
        CLAUDE_VERSIONS_PATH,
        &["node", "unrelated.js"],
    );
    let resolved = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let outer_agent = attr(&resolved, codex_root).agent_id.clone();
    assert_eq!(
        attr(&resolved, versioned).role,
        ProcessRole::Workload,
        "a confirmed non-claude argv must invalidate the candidate and fall back to the outer \
         Codex workload via ordinary ancestry resolution, not stay stuck provisional"
    );
    assert_eq!(attr(&resolved, versioned).agent_id, outer_agent);
    assert_eq!(
        attr(&resolved, versioned).workload_id,
        attr(&resolved, outer_shell).workload_id
    );
}

// ---------------------------------------------------------------------
// Coordinator-resolved shared-reaper policy: an explicit PID reference (root_pid_key) to an
// unreadable candidate X protects X and every descendant of X, unconditionally -- already
// covered by `an_unreadable_marker_referenced_root_stays_provisional_until_its_executable_resolves`
// above, where the custom_root's unmarked sibling is swept in too.
//
// An ancestry-inferred reference (no root_pid_key) to an unreadable ancestor is narrower: it
// protects only the marked claimant, that claimant's own descendants, and the unreadable
// candidates on that claimant's own ancestry path up to the nearest readable agent root -- never
// an unrelated sibling subtree that merely happens to share the same unreadable ancestor. A
// shared unreadable process (e.g. a reaper/supervisor) must not become a blanket umbrella that
// launders unrelated children into the same provisional agent.
// ---------------------------------------------------------------------

#[test]
fn a_shared_unreadable_ancestor_only_protects_the_marked_ancestry_path_not_unrelated_subtrees() {
    let reaper = id(5300, 1);
    let marked_child = id(5301, 2);
    let marked_grandchild = id(5302, 3);
    let marked_child_b = id(5305, 2);
    let marked_grandchild_b = id(5306, 3);
    let unrelated_sibling = id(5303, 2);
    let unrelated_grandchild = id(5304, 3);
    let env = [("CODEX_THREAD_ID", "sess-shared-reaper")];
    // A second marked branch under the very same unreadable reaper, but a distinct session --
    // sharing the protected candidate must not merge the two into one context.
    let env_b = [("CODEX_THREAD_ID", "sess-shared-reaper-b")];
    let platform = FakePlatform::default()
        .env(marked_child, &env)
        .env(marked_child_b, &env_b);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let mut table = [
        proc_unreadable(reaper, 1, reaper.pid),
        proc(
            marked_child,
            reaper.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            marked_grandchild,
            marked_child.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            marked_child_b,
            reaper.pid,
            marked_child_b.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            marked_grandchild_b,
            marked_child_b.pid,
            marked_child_b.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            unrelated_sibling,
            reaper.pid,
            unrelated_sibling.pid,
            "/usr/bin/other-tool",
            &[],
        ),
        proc(
            unrelated_grandchild,
            unrelated_sibling.pid,
            unrelated_sibling.pid,
            "/usr/bin/other-tool",
            &[],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let marked_agent_id = attr(&first, marked_child).agent_id.clone().unwrap();
    let marked_agent_id_b = attr(&first, marked_child_b).agent_id.clone().unwrap();
    assert_ne!(
        marked_agent_id, marked_agent_id_b,
        "two branches with different session markers must stay separate provisional contexts even though they share the same unreadable ancestor"
    );
    assert_eq!(
        agent_of(&first, &marked_agent_id).state,
        AgentState::Unknown
    );
    assert_eq!(
        agent_of(&first, &marked_agent_id_b).state,
        AgentState::Unknown
    );
    // The reaper itself is protected (never absorbed into an outer/unrelated context, never
    // dropped), but sharing it as a candidate does not itself establish same-session ownership
    // across the two branches, so its own resolved agent is not asserted here.
    assert_eq!(attr(&first, reaper).role, ProcessRole::AgentInternal);
    for (label, p, expected_agent) in [
        ("marked_child", marked_child, &marked_agent_id),
        ("marked_grandchild", marked_grandchild, &marked_agent_id),
        ("marked_child_b", marked_child_b, &marked_agent_id_b),
        (
            "marked_grandchild_b",
            marked_grandchild_b,
            &marked_agent_id_b,
        ),
    ] {
        let a = attr(&first, p);
        assert_eq!(
            a.role,
            ProcessRole::AgentInternal,
            "{label} is on its own marked claimant's ancestry/descendant path and must be protected"
        );
        assert_eq!(
            a.agent_id.as_deref(),
            Some(expected_agent.as_str()),
            "{label}"
        );
    }
    for (label, p) in [
        ("unrelated_sibling", unrelated_sibling),
        ("unrelated_grandchild", unrelated_grandchild),
    ] {
        assert_eq!(
            attr(&first, p).role,
            ProcessRole::Unattributed,
            "{label} shares the unreadable reaper but carries no marker of its own and must not be swept into either marked claimant's provisional agent"
        );
        assert!(attr(&first, p).agent_id.is_none(), "{label}");
    }

    // Repeated unchanged tick: the scoping must hold, not just apply on first sight.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(attr(&repeated, reaper).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&repeated, marked_child).role,
        ProcessRole::AgentInternal
    );
    assert_eq!(
        attr(&repeated, marked_child_b).role,
        ProcessRole::AgentInternal
    );
    assert_ne!(
        attr(&repeated, marked_child).agent_id,
        attr(&repeated, marked_child_b).agent_id
    );
    assert_eq!(
        attr(&repeated, unrelated_sibling).role,
        ProcessRole::Unattributed
    );
    assert_eq!(
        attr(&repeated, unrelated_grandchild).role,
        ProcessRole::Unattributed
    );

    // Recovery: the reaper resolves to the real codex binary. Once it is a genuinely confirmed
    // root, it legitimately owns every descendant, marked or not -- the unrelated subtree is no
    // longer excluded, it is now honestly attributed rather than laundered by provisional cover.
    table[0] = proc(reaper, 1, reaper.pid, CODEX, &[]);
    let recovered = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    let reaper_attr = attr(&recovered, reaper);
    assert_eq!(reaper_attr.role, ProcessRole::AgentRoot);
    let real_agent_id = reaper_attr.agent_id.clone().unwrap();
    assert_eq!(
        attr(&recovered, marked_child).agent_id.as_deref(),
        Some(real_agent_id.as_str())
    );
    assert_eq!(
        attr(&recovered, unrelated_sibling).agent_id.as_deref(),
        Some(real_agent_id.as_str()),
        "once the root is genuinely confirmed, it owns the previously-excluded sibling subtree too"
    );
    assert_ne!(
        attr(&recovered, unrelated_sibling).role,
        ProcessRole::Unattributed
    );
}

#[test]
fn a_shared_unreadable_ancestor_nested_under_an_outer_agent_leaves_the_unrelated_sibling_in_the_outer_workload()
 {
    // claude(root) -> sh -c (outer workload) -> unreadable reaper -> [marked codex child,
    // unrelated unmarked sibling]. The marked path forms its own provisional context; the
    // unrelated sibling must keep resolving through the outer claude workload as if the
    // unreadable reaper were simply not a boundary for it.
    let claude_root = id(5310, 1);
    let outer_shell = id(5311, 2);
    let reaper = id(5312, 3);
    let marked_child = id(5313, 4);
    let marked_grandchild = id(5314, 5);
    let unrelated_sibling = id(5315, 4);
    let claude_env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-shared-reaper-nested"),
        ("CLAUDE_PID", "5310"),
    ];
    let codex_env = [("CODEX_THREAD_ID", "sess-shared-reaper-nested-inner")];
    let platform = FakePlatform::default()
        .env(claude_root, &claude_env)
        .env(marked_child, &codex_env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();

    let mut table = [
        proc(claude_root, 1, claude_root.pid, CLAUDE, &[]),
        proc(
            outer_shell,
            claude_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run reaper"],
        ),
        proc_unreadable(reaper, outer_shell.pid, reaper.pid),
        proc(
            marked_child,
            reaper.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            marked_grandchild,
            marked_child.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            unrelated_sibling,
            reaper.pid,
            unrelated_sibling.pid,
            "/usr/bin/other-tool",
            &[],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);

    let claude_attr = attr(&first, claude_root);
    let outer_shell_attr = attr(&first, outer_shell);
    assert_eq!(outer_shell_attr.role, ProcessRole::Workload);

    let marked_child_attr = attr(&first, marked_child);
    assert_ne!(
        marked_child_attr.agent_id, claude_attr.agent_id,
        "the marked descendant still starts its own provisional context inside the outer tree"
    );
    assert_eq!(attr(&first, reaper).role, ProcessRole::AgentInternal);
    assert_eq!(marked_child_attr.role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&first, marked_grandchild).agent_id,
        marked_child_attr.agent_id
    );

    let sibling_attr = attr(&first, unrelated_sibling);
    assert_eq!(
        sibling_attr.agent_id, claude_attr.agent_id,
        "the unrelated sibling must keep resolving through the outer agent, not the reaper's provisional context"
    );
    assert_eq!(
        sibling_attr.role,
        ProcessRole::Workload,
        "the unrelated sibling stays an ordinary member of the outer workload"
    );
    assert_eq!(
        sibling_attr.workload_id, outer_shell_attr.workload_id,
        "the unrelated sibling stays in the outer shell's original workload, unaffected by the unreadable reaper between them"
    );

    // Repeated unchanged tick.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    assert_eq!(attr(&repeated, reaper).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&repeated, unrelated_sibling).workload_id,
        outer_shell_attr.workload_id
    );
    assert_eq!(
        attr(&repeated, unrelated_sibling).agent_id,
        claude_attr.agent_id
    );
}

// ---------------------------------------------------------------------
// Coordinator addendum: the unreadable ancestor's own provisional protection is temporary
// cover, not a real attribution. When it resolves to an ordinary non-agent binary, it must
// unwind cleanly back to whatever it would have been without ever having been swept into the
// marked claimant's provisional context -- Unattributed standalone, or its prior outer workload
// when nested -- never a fresh leftover Workload of the now-ended marked agent. Unrelated
// siblings, never touched by the protection in the first place, must stay unaffected either way.
// ---------------------------------------------------------------------

#[test]
fn a_shared_unreadable_ancestor_that_resolves_to_a_non_agent_binary_returns_to_its_prior_unattributed_context()
 {
    let reaper = id(5320, 1);
    let marked_child = id(5321, 2);
    let unrelated_sibling = id(5322, 2);
    let env = [("CODEX_THREAD_ID", "sess-shared-reaper-nonagent")];
    let platform = FakePlatform::default().env(marked_child, &env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc_unreadable(reaper, 1, reaper.pid),
        proc(
            marked_child,
            reaper.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            unrelated_sibling,
            reaper.pid,
            unrelated_sibling.pid,
            "/usr/bin/other-tool",
            &[],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    assert_eq!(attr(&first, reaper).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&first, unrelated_sibling).role,
        ProcessRole::Unattributed
    );

    // The unreadable ancestor resolves, but to an ordinary non-agent binary -- its temporary
    // provisional protection must unwind cleanly, not leave it stranded as a fresh leftover
    // workload of the now-ended marked claimant's agent.
    table[0] = proc(reaper, 1, reaper.pid, "/usr/bin/plain-tool", &[]);
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let reaper_attr = attr(&after, reaper);
    assert_eq!(
        reaper_attr.role,
        ProcessRole::Unattributed,
        "once the unreadable candidate resolves to a non-agent binary, it must return to its pre-provisional context, not become a leftover Workload"
    );
    assert!(reaper_attr.agent_id.is_none());
    assert_eq!(
        attr(&after, unrelated_sibling).role,
        ProcessRole::Unattributed,
        "the unrelated sibling, never part of the protection, stays unaffected"
    );

    // Repeated unchanged tick: the unwind must hold, not just apply once.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    assert_eq!(attr(&repeated, reaper).role, ProcessRole::Unattributed);
    assert!(attr(&repeated, reaper).agent_id.is_none());
}

#[test]
fn a_shared_unreadable_ancestor_nested_under_an_outer_agent_that_resolves_to_a_non_agent_binary_returns_to_the_outer_workload()
 {
    let claude_root = id(5330, 1);
    let outer_shell = id(5331, 2);
    let reaper = id(5332, 3);
    let marked_child = id(5333, 4);
    let unrelated_sibling = id(5334, 4);
    let claude_env = [
        ("CLAUDE_CODE_SESSION_ID", "sess-nonagent-nested"),
        ("CLAUDE_PID", "5330"),
    ];
    let codex_env = [("CODEX_THREAD_ID", "sess-nonagent-nested-inner")];
    let platform = FakePlatform::default()
        .env(claude_root, &claude_env)
        .env(marked_child, &codex_env);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let mut table = [
        proc(claude_root, 1, claude_root.pid, CLAUDE, &[]),
        proc(
            outer_shell,
            claude_root.pid,
            outer_shell.pid,
            "/bin/sh",
            &["sh", "-c", "run reaper"],
        ),
        proc_unreadable(reaper, outer_shell.pid, reaper.pid),
        proc(
            marked_child,
            reaper.pid,
            marked_child.pid,
            "/usr/bin/worker",
            &[],
        ),
        proc(
            unrelated_sibling,
            reaper.pid,
            unrelated_sibling.pid,
            "/usr/bin/other-tool",
            &[],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let claude_agent_id = attr(&first, claude_root).agent_id.clone();
    let outer_shell_workload = attr(&first, outer_shell).workload_id.clone();
    assert_eq!(attr(&first, reaper).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&first, unrelated_sibling).workload_id,
        outer_shell_workload
    );

    // The reaper resolves to an ordinary non-agent binary -- it must fall back to the outer
    // agent's workload it was always nested under, not become a standalone leftover.
    table[2] = proc(
        reaper,
        outer_shell.pid,
        reaper.pid,
        "/usr/bin/plain-tool",
        &[],
    );
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let reaper_attr = attr(&after, reaper);
    assert_eq!(
        reaper_attr.role,
        ProcessRole::Workload,
        "back in the outer tree, a non-agent-resolved reaper is an ordinary member of the outer workload again"
    );
    assert_eq!(reaper_attr.agent_id, claude_agent_id);
    assert_eq!(reaper_attr.workload_id, outer_shell_workload);
    assert_eq!(
        attr(&after, unrelated_sibling).workload_id,
        outer_shell_workload,
        "the unrelated sibling stays unaffected either way"
    );

    // Repeated unchanged tick.
    let repeated = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    assert_eq!(attr(&repeated, reaper).role, ProcessRole::Workload);
    assert_eq!(attr(&repeated, reaper).workload_id, outer_shell_workload);
}

// ---------------------------------------------------------------------
// Fixup-2 review finding 2 (P1): entering and then leaving provisional Unknown must never panic,
// even when the provisional candidate's own outer root subtree is confirmed fully gone in the
// same window. The fixup-3 semantics deliberately drop the old saved-assignment restoration:
// there is nothing to restore, only a current-evidence re-resolution. A member that still carries
// its own marker recomputes to whatever that marker's evidence now supports (here: a rootless,
// Ended codex agent, since the referenced wrapper never turned out to be a real codex binary); a
// member with no marker of its own and no live ancestry path back to that agent gets no free ride
// and comes back Unattributed. Regression for the review's four-tick reproduction.
// ---------------------------------------------------------------------

#[test]
fn a_provisional_referenced_candidate_survives_its_outer_roots_confirmed_exit_without_panicking() {
    let outer = id(4990, 10);
    let shell = id(4991, 20);
    let wrapper = id(4992, 30);
    let marked = id(4993, 40);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let platform = FakePlatform::default().env(marked, &[("CODEX_THREAD_ID", "inner")]);

    // Tick 0: an ordinary claude workload -- outer(claude) -> shell(sh -c) -> wrapper.
    let mut table = vec![
        proc(outer, 1, outer.pid, CLAUDE, &[]),
        proc(shell, outer.pid, shell.pid, "/bin/sh", &["sh", "-c", "run"]),
        proc(wrapper, shell.pid, wrapper.pid, "/bin/wrapper", &[]),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let claude_agent_id = attr(&first, outer).agent_id.clone().unwrap();
    assert_eq!(attr(&first, wrapper).role, ProcessRole::Workload);
    assert_eq!(
        attr(&first, wrapper).agent_id,
        Some(claude_agent_id.clone())
    );

    // Tick 1: the wrapper's own executable becomes unreadable, and a codex-marked child appears
    // under it (built-in CODEX_THREAD_ID, no root_pid_key -- ancestry-inferred). None of the
    // marked child's own ancestry (itself, wrapper, shell, outer) is a real codex binary, but
    // wrapper's unreadable exe keeps its candidacy Unknown rather than Invalid, so it -- not the
    // marker-bearing process itself -- becomes the winning (still-uncertain) candidate. Both the
    // wrapper and the marked child go provisional under a new, rootless codex agent.
    table[2] = proc_unreadable(wrapper, shell.pid, wrapper.pid);
    table.push(proc(
        marked,
        wrapper.pid,
        marked.pid,
        "/usr/bin/worker",
        &[],
    ));
    let demoted = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let wrapper_attr = attr(&demoted, wrapper);
    assert_eq!(wrapper_attr.role, ProcessRole::AgentInternal);
    let codex_agent_id = wrapper_attr.agent_id.clone().unwrap();
    assert_ne!(codex_agent_id, claude_agent_id);
    assert_eq!(
        agent_of(&demoted, &codex_agent_id).state,
        AgentState::Unknown
    );
    assert_eq!(agent_of(&demoted, &codex_agent_id).root, None);
    assert_eq!(attr(&demoted, marked).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&demoted, marked).agent_id,
        Some(codex_agent_id.clone())
    );

    // Tick 2: outer and shell are confirmed fully gone (not merely unreadable) and removed from
    // the process table entirely; wrapper is reparented to pid 1. This is the exact shape that
    // pruned the outer Agent record while a provisional member still referenced it in the
    // fixup-2 review's original reproduction. Must not panic.
    table.remove(0);
    table.remove(0);
    table[0].ppid = 1;
    let gone_platform = platform.gone(outer).gone(shell);
    let outer_exited = attributor.update(
        &gone_platform,
        &mut table,
        now + Duration::from_secs(2),
        2000,
        false,
    );
    assert!(
        !outer_exited.agents.iter().any(|a| a.id == claude_agent_id),
        "the outer claude agent must be pruned once nothing references it any longer"
    );
    assert_eq!(
        attr(&outer_exited, wrapper).role,
        ProcessRole::AgentInternal
    );
    assert_eq!(attr(&outer_exited, marked).role, ProcessRole::AgentInternal);
    assert_eq!(
        attr(&outer_exited, wrapper).agent_id,
        Some(codex_agent_id.clone())
    );

    // Tick 3: the wrapper's executable resolves again, but to another ordinary, definitely
    // non-codex binary. Per the current fixup-3 semantics there is no saved pre-provisional
    // context to restore -- every cached member re-resolves purely from current evidence. The
    // wrapper was never a root of anything and returns Unattributed. The marked child still
    // carries its own marker, whose sole (now doubly-disqualified) candidate resolves to no
    // agent at all, so it falls back to the marker's session-keyed agent directly: rootless and
    // Ended, since nothing roots it -- not restored to any prior Workload membership.
    table[0] = proc(wrapper, 1, wrapper.pid, "/bin/wrapper-v2", &[]);
    let returned = attributor.update(
        &gone_platform,
        &mut table,
        now + Duration::from_secs(3),
        3000,
        false,
    );
    assert_eq!(attr(&returned, wrapper).role, ProcessRole::Unattributed);
    assert!(attr(&returned, wrapper).agent_id.is_none());
    let marked_attr = attr(&returned, marked);
    assert_eq!(
        marked_attr.role,
        ProcessRole::AgentInternal,
        "the marked child falls back to its own marker's session-keyed agent, not a restored \
         Workload membership"
    );
    assert!(marked_attr.workload_id.is_none());
    let marked_agent_id = marked_attr.agent_id.clone().unwrap();
    let marked_agent = agent_of(&returned, &marked_agent_id);
    assert_eq!(marked_agent.kind, "codex");
    assert_eq!(marked_agent.session_id.as_deref(), Some("inner"));
    assert_eq!(
        marked_agent.state,
        AgentState::Ended,
        "a rootless codex agent with no live candidate is Ended, not left dangling Unknown"
    );
    assert_eq!(marked_agent.root, None);

    // Repeated unchanged tick: the outcome must hold, not just fire once and drift back.
    let repeated = attributor.update(
        &gone_platform,
        &mut table,
        now + Duration::from_secs(4),
        4000,
        false,
    );
    assert_eq!(attr(&repeated, wrapper).role, ProcessRole::Unattributed);
    assert_eq!(attr(&repeated, marked).role, ProcessRole::AgentInternal);
    assert_eq!(attr(&repeated, marked).agent_id, Some(marked_agent_id));
}

// ---------------------------------------------------------------------
// Fixup-2 review finding 3 (P2): a candidate's initial metadata must be arbitrated by evidence
// priority (explicit PID reference, then ancestry inference, then inherited marker) before it is
// committed, independent of ancestry distance and of enumeration order. Before the fix, valid
// claims were sorted purely by ancestry distance, so a nearby ancestry-inferred claim could
// establish a root's session before a farther, more specific explicit reference was even
// processed; the explicit claim then lost to the "conflicting session" guard instead of winning
// it, sending future session-hook correlation to the wrong session.
// ---------------------------------------------------------------------

#[test]
fn an_explicit_pid_referenced_session_wins_over_a_nearer_ancestry_inferred_one_regardless_of_enumeration_order()
 {
    let root = id(4980, 1);
    let near = id(4981, 2);
    let explicit = id(4982, 3);
    let marker = Marker {
        key: "CUSTOM_SESSION".into(),
        level: MarkerLevel::Agent,
        name_key: None,
        kind: Some("codex".into()),
        root_pid_key: Some("CUSTOM_ROOT".into()),
        root_binaries: vec!["codex".into()],
        session_id: true,
    };
    let platform = FakePlatform::default()
        .env(near, &[("CODEX_THREAD_ID", "inferred")])
        .env(
            explicit,
            &[("CUSTOM_SESSION", "explicit"), ("CUSTOM_ROOT", "4980")],
        );

    for reversed in [false, true] {
        let mut attributor = Attributor::new(vec![marker.clone()], Vec::new());
        let now = Instant::now();
        let mut table = [
            proc(root, 1, root.pid, CODEX, &[]),
            proc(near, root.pid, near.pid, "/usr/bin/worker", &[]),
            proc(explicit, near.pid, explicit.pid, "/usr/bin/worker", &[]),
        ];
        if reversed {
            table.reverse();
        }
        for tick in 0..2u64 {
            let snapshot = attributor.update(
                &platform,
                &mut table,
                now + Duration::from_secs(tick),
                tick * 1000,
                false,
            );
            let root_agent_id = attr(&snapshot, root)
                .agent_id
                .clone()
                .unwrap_or_else(|| panic!("reversed={reversed} tick={tick}: root has no agent"));
            let root_agent = agent_of(&snapshot, &root_agent_id);
            assert_eq!(
                root_agent.session_id.as_deref(),
                Some("explicit"),
                "reversed={reversed} tick={tick}: the explicit PID reference must win the root's \
                 session over the nearer ancestry-inferred claim, in both enumeration orders"
            );
            assert_eq!(root_agent.kind, "codex");
        }
    }
}

// ---------------------------------------------------------------------
// Coordinator-proposed repro for the parent-before-old fallback-order fix (resolve()'s `agent`
// computation in mod.rs): a plain, never-detached descendant whose live parent's own attribution
// moves on (root confirmed gone, then the parent reparents to pid 1 and execs into a marker
// carrier for an unrelated rootless agent) must follow its parent's *current* attribution, not
// its own stale `old.agent_id` left over from before the parent moved on.
// ---------------------------------------------------------------------

#[test]
fn a_never_detached_descendant_follows_its_reparented_execd_parents_current_marker_not_its_own_stale_agent()
 {
    let root = id(4990, 1);
    let shell = id(4991, 2);
    let leaf = id(4992, 3);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let platform = FakePlatform::default();

    let mut table = vec![
        proc(root, 1, root.pid, CLAUDE, &[]),
        proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "run"]),
        proc(leaf, shell.pid, shell.pid, "/usr/bin/worker", &[]),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let claude_agent_id = attr(&first, root).agent_id.clone().unwrap();
    assert_eq!(attr(&first, shell).role, ProcessRole::Workload);
    let outer_workload = attr(&first, shell).workload_id.clone();
    assert_eq!(attr(&first, leaf).role, ProcessRole::Workload);
    assert_eq!(attr(&first, leaf).workload_id, outer_workload);

    // The root is confirmed fully gone (removed from the table, not merely unreadable). The
    // shell -- same identity throughout -- reparents to pid 1 and execs into an ordinary,
    // non-agent binary that now carries its own CODEX_THREAD_ID marker (a legitimate env change
    // on exec). With no codex binary anywhere in its now-empty ancestry, the shell's own claim
    // resolves to no candidate at all and it falls back to a brand new, rootless Ended
    // "codex"-kind agent keyed by that marker's session -- a real agent, distinct from the dead
    // claude one. The leaf keeps the exact same ppid (still the shell's pid) the whole time: it
    // is never itself detached, reparented, or execed.
    table.remove(0);
    table[0] = proc(shell, 1, shell.pid, "/bin/worker", &[]);
    let platform = platform.env(shell, &[("CODEX_THREAD_ID", "inner")]);
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );

    let shell_attr = attr(&after, shell);
    assert_eq!(
        shell_attr.role,
        ProcessRole::Workload,
        "the shell, now rootless (ppid==1) under its own fresh marker's rootless Ended agent, \
         becomes a Workload of that agent"
    );
    let inner_agent_id = shell_attr.agent_id.clone().unwrap();
    assert_ne!(inner_agent_id, claude_agent_id);
    assert_eq!(agent_of(&after, &inner_agent_id).kind, "codex");
    assert_eq!(
        agent_of(&after, &inner_agent_id).state,
        AgentState::Ended,
        "a rootless marker claim with no matching binary anywhere in its ancestry resolves to no \
         candidate at all, so its fallback agent is immediately Ended, not left Unknown"
    );

    let leaf_attr = attr(&after, leaf);
    assert_eq!(
        leaf_attr.agent_id,
        Some(inner_agent_id),
        "a never-detached descendant must follow its live parent's current agent, not cling to \
         its own stale pre-transition agent_id merely because that old agent record has not been \
         pruned yet within the same tick"
    );
    assert_eq!(
        leaf_attr.role,
        ProcessRole::Workload,
        "the leaf inherits Workload membership in the shell's new workload, not the dead claude \
         workload"
    );
    assert_eq!(leaf_attr.workload_id, shell_attr.workload_id);
}

// ---------------------------------------------------------------------
// Coordinator-requested regression: when a workload's own root process reparents+execs into a
// *different* agent while a second member of that same old workload had already detached (and
// therefore kept the *old* agent under the cached-detached ratchet), each process's
// `workload_id` must still resolve to a workload whose `agent_id` matches that same process's
// own `agent_id` -- the detached member must never end up pointing at the new root's workload.
// Workload ids now key on the owning agent as well as the root's own identity, so the two
// workloads here no longer share an id string either.
// ---------------------------------------------------------------------

#[test]
fn a_detached_sibling_and_its_reparented_execd_former_root_never_share_a_workload_record_across_agents()
 {
    let root = id(4993, 1);
    let shell = id(4994, 2);
    let leaf = id(4995, 3);
    let detached_sibling = id(4996, 4);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let platform = FakePlatform::default();

    let mut table = vec![
        proc(root, 1, root.pid, CLAUDE, &[]),
        proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "run"]),
        proc(leaf, shell.pid, shell.pid, "/usr/bin/worker", &[]),
        proc(
            detached_sibling,
            shell.pid,
            shell.pid,
            "/usr/bin/other-worker",
            &[],
        ),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let claude_agent_id = attr(&first, root).agent_id.clone().unwrap();
    let old_workload_id = attr(&first, shell).workload_id.clone().unwrap();
    assert_eq!(
        attr(&first, leaf).workload_id,
        Some(old_workload_id.clone())
    );
    assert_eq!(
        attr(&first, detached_sibling).workload_id,
        Some(old_workload_id.clone())
    );

    // The root is confirmed gone. `detached_sibling` reparents to pid 1 with no exec and no
    // marker of its own, so it keeps its old, now-cached, claude attribution untouched. In the
    // very same tick, the shell -- same identity throughout -- also reparents to pid 1 but
    // execs into a marker-carrying non-agent binary, producing a brand new rootless codex
    // agent/workload rooted at that same shell identity, exactly as in the test above.
    table.remove(0);
    table[0] = proc(shell, 1, shell.pid, "/bin/worker", &[]);
    table[2] = proc(
        detached_sibling,
        1,
        detached_sibling.pid,
        "/usr/bin/other-worker",
        &[],
    );
    let platform = platform.env(shell, &[("CODEX_THREAD_ID", "inner")]);
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );

    let shell_attr = attr(&after, shell);
    let inner_agent_id = shell_attr.agent_id.clone().unwrap();
    assert_ne!(inner_agent_id, claude_agent_id);
    let new_workload_id = shell_attr.workload_id.clone().unwrap();

    let sibling_attr = attr(&after, detached_sibling);
    assert_eq!(
        sibling_attr.agent_id,
        Some(claude_agent_id.clone()),
        "a genuinely detached member with no marker of its own keeps its cached pre-detach agent"
    );
    let sibling_workload_id = sibling_attr
        .workload_id
        .clone()
        .expect("a cached-detached member retains its workload membership too");

    // The interesting case is when the two workload ids happen to coincide (both rooted at the
    // shell's unchanged identity); either way, each side's own workload record must still name
    // that side's own agent, never the other one's.
    let sibling_workload = workload_of(&after, &sibling_workload_id);
    assert_eq!(
        sibling_workload.agent_id, claude_agent_id,
        "detached_sibling's workload_id ({sibling_workload_id}) must resolve to a workload \
         record owned by the claude agent it is actually attributed to, not get silently \
         repointed at the shell's new codex workload merely because the id strings coincide"
    );
    let new_workload = workload_of(&after, &new_workload_id);
    assert_eq!(new_workload.agent_id, inner_agent_id);

    let leaf_attr = attr(&after, leaf);
    assert_eq!(leaf_attr.agent_id, Some(inner_agent_id.clone()));
    assert_eq!(leaf_attr.workload_id, Some(new_workload_id.clone()));
    assert_eq!(
        workload_of(&after, &leaf_attr.workload_id.clone().unwrap()).agent_id,
        inner_agent_id
    );
}

// ---------------------------------------------------------------------
// Property-test-discovered regression: a direct child of an established root whose own exe is
// unreadable at first sighting gets a conservative AgentInternal default (nothing observable yet
// says it's a tool-call shell). Once its exe/argv become readable and reveal a real `sh -c`
// invocation, that role must be re-derived from the new positive evidence, not stay stuck at the
// earlier blind default.
// ---------------------------------------------------------------------

#[test]
fn a_direct_roots_child_upgrades_from_agent_internal_to_workload_once_its_shell_exe_becomes_readable()
 {
    let root = id(5001, 1);
    let shell = id(5002, 2);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let now = Instant::now();
    let platform = FakePlatform::default();

    let mut table = vec![
        proc(root, 1, root.pid, CODEX, &[]),
        proc_unreadable(shell, root.pid, shell.pid),
    ];
    let first = attributor.update(&platform, &mut table, now, 0, false);
    let agent_id = attr(&first, root).agent_id.clone().unwrap();
    assert_eq!(
        attr(&first, shell).role,
        ProcessRole::AgentInternal,
        "an unreadable direct child of an established root defaults to internal, not workload, \
         while its own exe is unknown"
    );

    table[1] = proc(shell, root.pid, shell.pid, "/bin/sh", &["sh", "-c", "run"]);
    let after = attributor.update(
        &platform,
        &mut table,
        now + Duration::from_secs(1),
        1000,
        false,
    );
    let shell_attr = attr(&after, shell);
    assert_eq!(shell_attr.agent_id, Some(agent_id));
    assert_eq!(
        shell_attr.role,
        ProcessRole::Workload,
        "once the shell's own exe/argv become readable and reveal a real tool-call shell, its \
         role must be re-derived from that positive evidence, not stay stuck at the earlier \
         blind AgentInternal default"
    );
}

// ---------------------------------------------------------------------
// Fixup-4 regressions: durable workload roots, bounded claims, and the cold-start/live-daemon
// stale-carrier ancestry guard. Named after the fixup-3 review's findings (see
// scratch/t04-review-fixup3/probes.rs for the originals); ported here as permanent regressions
// plus additional coverage the ticket asked for (custom-marker roots, pending targets, the
// live-daemon stale-carrier variant, the unmarked-older-ancestor ratchet, and the P1 marked-
// tool-shell three-way branch).
// ---------------------------------------------------------------------
mod fixup4_regressions {
    use super::*;

    fn root_and_shell() -> (ProcessIdentity, ProcessIdentity, ProcessIdentity) {
        (id(100, 1), id(101, 2), id(102, 3))
    }

    /// bash/zsh exec the single simple command of `-c` in place (no fork), so the tool shell
    /// identity survives with a new exe/argv. Its workload must survive with it.
    #[test]
    fn a_tool_shell_that_execs_its_command_in_place_keeps_its_workload() {
        let (root, shell, child) = root_and_shell();
        let platform = FakePlatform::default()
            .env(root, &[])
            .env(shell, &[])
            .env(child, &[]);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let s1 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 100, CLAUDE, &["claude"]),
                proc(shell, 100, 101, "/bin/bash", &["bash", "-c", "npm test"]),
            ],
            now,
            0,
            false,
        );
        assert_eq!(attr(&s1, shell).role, ProcessRole::Workload);
        let workload = attr(&s1, shell).workload_id.clone().unwrap();
        let s2 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 100, CLAUDE, &["claude"]),
                proc(shell, 100, 101, "/usr/bin/node", &["npm", "test"]),
                proc(child, 101, 101, "/usr/bin/node", &["node", "jest"]),
            ],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let s = attr(&s2, shell);
        let c = attr(&s2, child);
        assert_eq!(
            (
                s.role,
                s.workload_id.as_deref(),
                c.role,
                c.workload_id.as_deref()
            ),
            (
                ProcessRole::Workload,
                Some(workload.as_str()),
                ProcessRole::Workload,
                Some(workload.as_str())
            ),
            "after exec-in-place the tool shell and its child must stay in workload {workload}; \
             workloads now: {:?}",
            s2.workloads.iter().map(|w| &w.id).collect::<Vec<_>>()
        );
    }

    /// The exe/argv of an established tool shell fails to read for one tick.
    #[test]
    fn a_transiently_unreadable_tool_shell_keeps_its_workload() {
        let (root, shell, child) = root_and_shell();
        let platform = FakePlatform::default()
            .env(root, &[])
            .env(shell, &[])
            .env(child, &[]);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let table = |shell_proc: Process| {
            [
                proc(root, 1, 100, CLAUDE, &["claude"]),
                shell_proc,
                proc(child, 101, 101, "/usr/bin/node", &["node", "jest"]),
            ]
        };
        let s1 = attributor.update(
            &platform,
            &mut table(proc(
                shell,
                100,
                101,
                "/bin/bash",
                &["bash", "-c", "npm test"],
            )),
            now,
            0,
            false,
        );
        let workload = attr(&s1, shell).workload_id.clone().unwrap();
        assert_eq!(workload_of(&s1, &workload).first_seen_ms, 0);
        let s2 = attributor.update(
            &platform,
            &mut table(proc_unreadable(shell, 100, 101)),
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let roles2 = (attr(&s2, shell).role, attr(&s2, child).role);
        let record2 = s2
            .workloads
            .iter()
            .find(|w| w.id == workload)
            .map(|w| w.first_seen_ms);
        let s3 = attributor.update(
            &platform,
            &mut table(proc(
                shell,
                100,
                101,
                "/bin/bash",
                &["bash", "-c", "npm test"],
            )),
            now + Duration::from_secs(2),
            2000,
            false,
        );
        let record3 = s3
            .workloads
            .iter()
            .find(|w| w.id == workload)
            .map(|w| w.first_seen_ms);
        assert_eq!(
            (roles2, record2, record3),
            (
                (ProcessRole::Workload, ProcessRole::Workload),
                Some(0),
                Some(0)
            ),
            "unreadable tick: roles {roles2:?}, record first_seen {record2:?}; after recovery \
             first_seen {record3:?}"
        );
    }

    /// The established tool shell is missing from one scan (unknown liveness), then returns.
    #[test]
    fn a_transiently_omitted_tool_shell_keeps_its_workload_record() {
        let (root, shell, child) = root_and_shell();
        let platform = FakePlatform::default()
            .env(root, &[])
            .env(shell, &[])
            .env(child, &[])
            .pid_presence(101, None);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let s1 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 100, CLAUDE, &["claude"]),
                proc(shell, 100, 101, "/bin/bash", &["bash", "-c", "npm test"]),
                proc(child, 101, 101, "/usr/bin/node", &["node", "jest"]),
            ],
            now,
            0,
            false,
        );
        let workload = attr(&s1, shell).workload_id.clone().unwrap();
        let s2 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 100, CLAUDE, &["claude"]),
                proc(child, 101, 101, "/usr/bin/node", &["node", "jest"]),
            ],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let child2 = attr(&s2, child);
        let record2 = s2
            .workloads
            .iter()
            .find(|w| w.id == workload)
            .map(|w| w.first_seen_ms);
        let s3 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 100, CLAUDE, &["claude"]),
                proc(shell, 100, 101, "/bin/bash", &["bash", "-c", "npm test"]),
                proc(child, 101, 101, "/usr/bin/node", &["node", "jest"]),
            ],
            now + Duration::from_secs(2),
            2000,
            false,
        );
        let record3 = s3
            .workloads
            .iter()
            .find(|w| w.id == workload)
            .map(|w| w.first_seen_ms);
        assert_eq!(
            (child2.role, child2.workload_id.as_deref(), record2, record3),
            (
                ProcessRole::Workload,
                Some(workload.as_str()),
                Some(0),
                Some(0)
            ),
            "omitted tick: child role {:?} workload {:?}, record first_seen {record2:?}; after \
             return first_seen {record3:?}",
            child2.role,
            child2.workload_id
        );
    }

    /// One long-lived Claude agent running one short tool call per tick.
    #[test]
    fn claims_of_exited_claude_claimants_are_not_retained_for_the_agents_lifetime() {
        let root = id(100, 1);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let mut platform = FakePlatform::default().env(root, &[]);
        let mut previous: Option<ProcessIdentity> = None;
        let ticks = 300u64;
        for i in 1..=ticks {
            let shell = id(1000 + i as i32, 10 + i);
            platform = platform.env(
                shell,
                &[("CLAUDE_CODE_SESSION_ID", "sess"), ("CLAUDE_PID", "100")],
            );
            if let Some(prev) = previous {
                platform = platform.gone(prev);
            }
            let snapshot = attributor.update(
                &platform,
                &mut [
                    proc(root, 1, 100, CLAUDE, &["claude"]),
                    proc(shell, 100, shell.pid, "/bin/sh", &["sh", "-c", "true"]),
                ],
                now + Duration::from_secs(i),
                i * 1000,
                false,
            );
            assert_eq!(attr(&snapshot, shell).role, ProcessRole::Workload);
            previous = Some(shell);
        }
        let claims = attributor.claims.len();
        let watched = attributor.watched(&HashSet::new());
        let dead_watched = watched
            .iter()
            .filter(|w| w.pid >= 1000 && w.pid < 1000 + ticks as i32)
            .count();
        assert!(
            claims <= 4 && dead_watched <= 1,
            "after {ticks} one-shot tool calls: {claims} claims retained, {} watched identities \
             of which {dead_watched} belong to exited shells",
            watched.len()
        );
    }

    /// Codex variant: ancestry claims carry the claimant's own lineage as candidates.
    #[test]
    fn claims_of_exited_codex_claimants_do_not_grow_the_watched_set() {
        let root = id(100, 1);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let mut platform = FakePlatform::default().env(root, &[]);
        let mut previous: Option<ProcessIdentity> = None;
        let ticks = 300u64;
        for i in 1..=ticks {
            let shell = id(1000 + i as i32, 10 + i);
            platform = platform.env(shell, &[("CODEX_THREAD_ID", "thread-1")]);
            if let Some(prev) = previous {
                platform = platform.gone(prev);
            }
            let snapshot = attributor.update(
                &platform,
                &mut [
                    proc(root, 1, 100, CODEX, &["codex"]),
                    proc(shell, 100, shell.pid, "/bin/sh", &["sh", "-c", "true"]),
                ],
                now + Duration::from_secs(i),
                i * 1000,
                false,
            );
            assert_eq!(attr(&snapshot, shell).role, ProcessRole::Workload);
            previous = Some(shell);
        }
        let claims = attributor.claims.len();
        let watched = attributor.watched(&HashSet::new());
        let dead_watched = watched
            .iter()
            .filter(|w| w.pid >= 1000 && w.pid < 1000 + ticks as i32)
            .count();
        assert!(
            claims <= 4 && dead_watched <= 1,
            "after {ticks} one-shot tool calls: {claims} claims retained, {} watched identities \
             of which {dead_watched} belong to exited shells",
            watched.len()
        );
    }

    /// Many independent one-shot claimants all naming the same still-unresolved explicit target:
    /// pending-target evidence must be retained (it protects a real ambiguity), but retaining it
    /// for every historical claimant must not let claims/watched grow without bound either -- the
    /// P2 fix and "a pending explicit target must stay protected after its claimant exits" are
    /// not in tension.
    #[test]
    fn many_one_shot_claimants_naming_the_same_never_resolved_target_stay_claim_bounded() {
        let target = id(5500, 5);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let mut platform = FakePlatform::default();
        let mut previous: Option<ProcessIdentity> = None;
        let ticks = 300u64;
        for i in 1..=ticks {
            let claimant = id(2000 + i as i32, 20 + i);
            platform = platform.env(
                claimant,
                &[("CLAUDE_CODE_SESSION_ID", "sess"), ("CLAUDE_PID", "5500")],
            );
            if let Some(prev) = previous {
                platform = platform.gone(prev);
            }
            attributor.update(
                &platform,
                &mut [
                    proc(
                        claimant,
                        1,
                        claimant.pid,
                        "/usr/bin/node",
                        &["node", "claimant"],
                    ),
                    proc_unreadable(target, 1, target.pid),
                ],
                now + Duration::from_secs(i),
                i * 1000,
                false,
            );
            previous = Some(claimant);
        }
        let claims = attributor.claims.len();
        let watched = attributor.watched(&HashSet::new());
        assert!(
            claims <= 8 && watched.len() <= 8,
            "after {ticks} one-shot claimants all naming the same still-unresolved target, \
             claims and watched identities must stay bounded, not grow with historical claimant \
             count: {claims} claims, {} watched",
            watched.len()
        );
    }

    /// A custom (non-built-in) marker whose root is established only by a child claimant's
    /// reference must keep the root valid, by its own identity, after that claimant exits --
    /// and must not do so by retaining the claim forever.
    #[test]
    fn a_custom_marker_only_established_root_stays_valid_and_claims_bounded_after_its_establishing_claimant_exits()
     {
        let marker = Marker {
            key: "T04_ROOT_MARKER".into(),
            level: MarkerLevel::Agent,
            name_key: None,
            kind: Some("generic".into()),
            root_pid_key: Some("T04_ROOT_PID".into()),
            root_binaries: vec!["plain-root".into()],
            session_id: true,
        };
        let root = id(800, 5);
        let claimant = id(801, 6);
        let mut attributor = Attributor::new(vec![marker], Vec::new());
        let mut platform = FakePlatform::default().env(root, &[]).env(
            claimant,
            &[("T04_ROOT_MARKER", "custom-sess"), ("T04_ROOT_PID", "800")],
        );
        let now = Instant::now();
        let s1 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 800, "/usr/bin/plain-root", &["plain-root"]),
                proc(claimant, 800, 800, "/usr/bin/node", &["node", "worker"]),
            ],
            now,
            0,
            false,
        );
        let agent_id = attr(&s1, root).agent_id.clone().unwrap();
        assert_eq!(
            agent_of(&s1, &agent_id).session_id.as_deref(),
            Some("custom-sess"),
            "the custom marker must establish the root's session, for this probe to mean \
             anything"
        );

        platform = platform.gone(claimant);
        let s2 = attributor.update(
            &platform,
            &mut [proc(root, 1, 800, "/usr/bin/plain-root", &["plain-root"])],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let root_agent = agent_of(&s2, &agent_id);
        assert_eq!(
            root_agent.session_id.as_deref(),
            Some("custom-sess"),
            "the root's session must survive after the claimant that established it exits: {:?}",
            root_agent.session_id
        );
        assert_ne!(
            root_agent.state,
            AgentState::Ended,
            "the root process is still live"
        );
        assert!(
            attributor.claims.len() <= 1,
            "the establishing claim must not be retained forever just because the agent it \
             created is live: {} claims",
            attributor.claims.len()
        );
    }

    /// Cold-start reproduction from the fixup-3 review: the daemon starts after a Claude root PID
    /// was reused by a new session while a worker of the old session survived and later spawned a
    /// child with the old, inherited markers.
    #[test]
    fn first_observation_after_pid_reuse_does_not_let_a_stale_descendant_claim_the_new_root() {
        let new_root = id(600, 30);
        let old_worker = id(601, 20);
        let new_child = id(602, 40);
        let stale = [
            ("CLAUDE_CODE_SESSION_ID", "old-session"),
            ("CLAUDE_PID", "600"),
        ];
        let platform = FakePlatform::default()
            .env(new_root, &[])
            .env(old_worker, &stale)
            .env(new_child, &stale);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let s = attributor.update(
            &platform,
            &mut [
                proc(new_root, 1, 600, CLAUDE, &["claude"]),
                proc(old_worker, 1, 601, "/usr/bin/node", &["node", "worker"]),
                proc(new_child, 601, 601, "/usr/bin/node", &["node", "child"]),
            ],
            Instant::now(),
            0,
            false,
        );
        let root_agent = agent_of(&s, attr(&s, new_root).agent_id.as_deref().unwrap());
        let worker = attr(&s, old_worker);
        let child = attr(&s, new_child);
        assert!(
            root_agent.session_id.as_deref() != Some("old-session")
                && child.agent_id != Some(root_agent.id.clone()),
            "root session {:?}; worker agent {:?} role {:?}; child agent {:?} role {:?}",
            root_agent.session_id,
            worker.agent_id,
            worker.role,
            child.agent_id,
            child.role
        );
    }

    /// Live-daemon variant of the cold-start reproduction above: the same PID-reuse-plus-stale-
    /// marker shape, but occurring after the daemon has already completed a real observation
    /// cycle, so the fix must not be gated on this being the attributor's very first tick.
    #[test]
    fn pid_reuse_stale_carrier_is_rejected_after_the_daemon_has_already_been_running() {
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let idle_platform = FakePlatform::default();
        attributor.update(&idle_platform, &mut Vec::new(), now, 0, false);

        let new_root = id(600, 30);
        let old_worker = id(601, 20);
        let new_child = id(602, 40);
        let stale = [
            ("CLAUDE_CODE_SESSION_ID", "old-session"),
            ("CLAUDE_PID", "600"),
        ];
        let platform = FakePlatform::default()
            .env(new_root, &[])
            .env(old_worker, &stale)
            .env(new_child, &stale);
        let s = attributor.update(
            &platform,
            &mut [
                proc(new_root, 1, 600, CLAUDE, &["claude"]),
                proc(old_worker, 1, 601, "/usr/bin/node", &["node", "worker"]),
                proc(new_child, 601, 601, "/usr/bin/node", &["node", "child"]),
            ],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let root_agent = agent_of(&s, attr(&s, new_root).agent_id.as_deref().unwrap());
        let worker = attr(&s, old_worker);
        let child = attr(&s, new_child);
        assert!(
            root_agent.session_id.as_deref() != Some("old-session")
                && child.agent_id != Some(root_agent.id.clone()),
            "root session {:?}; worker agent {:?} role {:?}; child agent {:?} role {:?}",
            root_agent.session_id,
            worker.agent_id,
            worker.role,
            child.agent_id,
            child.role
        );
    }

    /// Ratchet against over-broadening the stale-carrier ancestry guard: an older, readable
    /// ancestor that carries no marker of its own (a subreaper, a supervisor) must never
    /// invalidate an otherwise legitimate claim just by being old and on the path. Only a
    /// positive same-marker/same-target stale carrier may do that (see the two tests above).
    #[test]
    fn an_unmarked_older_readable_ancestor_never_invalidates_an_otherwise_legitimate_claim() {
        let root = id(700, 50);
        let subreaper = id(650, 10);
        let child = id(701, 60);
        let platform = FakePlatform::default()
            .env(root, &[])
            .env(subreaper, &[])
            .env(
                child,
                &[
                    ("CLAUDE_CODE_SESSION_ID", "legit-session"),
                    ("CLAUDE_PID", "700"),
                ],
            );
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let s = attributor.update(
            &platform,
            &mut [
                proc(root, 1, 700, CLAUDE, &["claude"]),
                proc(subreaper, 1, 650, "/usr/bin/subreaperd", &["subreaperd"]),
                proc(
                    child,
                    subreaper.pid,
                    subreaper.pid,
                    "/usr/bin/node",
                    &["node", "child"],
                ),
            ],
            Instant::now(),
            0,
            false,
        );
        let root_agent = agent_of(&s, attr(&s, root).agent_id.as_deref().unwrap());
        assert_eq!(
            root_agent.session_id.as_deref(),
            Some("legit-session"),
            "an older readable ancestor with no marker of its own must never invalidate a claim \
             just by being older than the target root; only a positive same-marker/same-target \
             stale carrier may do that -- root session {:?}",
            root_agent.session_id
        );
    }

    fn root_and_ambiguous_shell() -> (ProcessIdentity, ProcessIdentity) {
        (id(2000, 1), id(2001, 2))
    }

    /// P1 three-way branch (1/3): a direct-child tool shell that also carries its own ancestry
    /// marker is a real ambiguity, not just an unreadable-shell hiccup (the plain unreadable-
    /// shell regression above): while its own exe is unreadable, whether it is really an ordinary
    /// command or secretly a nested agent binary cannot be told apart, so it must sit out as an
    /// undecided candidate (AgentInternal), never optimistically kept as Workload -- but its
    /// workload record (first_seen_ms) must still be retained underneath.
    #[test]
    fn a_marked_tool_shell_that_goes_unreadable_is_an_undecided_candidate_not_a_retained_workload()
    {
        let (root, shell) = root_and_ambiguous_shell();
        let env = &[("CODEX_THREAD_ID", "thread-x")];
        let platform = FakePlatform::default().env(root, &[]).env(shell, env);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();

        let s1 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, root.pid, CLAUDE, &["claude"]),
                proc(
                    shell,
                    root.pid,
                    shell.pid,
                    "/bin/bash",
                    &["bash", "-c", "npm test"],
                ),
            ],
            now,
            0,
            false,
        );
        assert_eq!(
            attr(&s1, shell).role,
            ProcessRole::Workload,
            "the shell's own exe positively rules out the ancestry marker's kind this tick, so \
             it must resolve as an ordinary workload member, for this probe to mean anything"
        );
        let workload = attr(&s1, shell).workload_id.clone().unwrap();

        let s2 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, root.pid, CLAUDE, &["claude"]),
                proc_unreadable(shell, root.pid, shell.pid),
            ],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        assert_eq!(
            attr(&s2, shell).role,
            ProcessRole::AgentInternal,
            "unreadable while carrying its own ancestry marker leaves the shell an undecided \
             candidate (it might actually be the marker's agent binary) -- it must not be \
             optimistically kept as Workload just because it was one before"
        );

        let s3 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, root.pid, CLAUDE, &["claude"]),
                proc(
                    shell,
                    root.pid,
                    shell.pid,
                    "/bin/bash",
                    &["bash", "-c", "npm test"],
                ),
            ],
            now + Duration::from_secs(2),
            2000,
            false,
        );
        let after = attr(&s3, shell);
        assert_eq!(after.role, ProcessRole::Workload);
        assert_eq!(
            workload_of(&s3, &workload).first_seen_ms,
            0,
            "once the shell resolves back to an ordinary command, it must rejoin its original \
             workload record, not a fresh one -- first_seen_ms must not reset just because the \
             identity spent a tick as an undecided candidate"
        );
    }

    /// P1 three-way branch (2/3, same setup, different branch): once the ambiguous shell's exe
    /// positively resolves to a recognized agent binary, it must become its own nested agent
    /// root, taking priority over the durable tool-shell bit from before.
    #[test]
    fn a_marked_tool_shell_that_resolves_to_the_markers_agent_binary_becomes_its_own_agent_root() {
        let (root, shell) = root_and_ambiguous_shell();
        let env = &[("CODEX_THREAD_ID", "thread-x")];
        let platform = FakePlatform::default().env(root, &[]).env(shell, env);
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();

        let s1 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, root.pid, CLAUDE, &["claude"]),
                proc(
                    shell,
                    root.pid,
                    shell.pid,
                    "/bin/bash",
                    &["bash", "-c", "npm test"],
                ),
            ],
            now,
            0,
            false,
        );
        let outer_agent = attr(&s1, root).agent_id.clone().unwrap();
        assert_eq!(attr(&s1, shell).role, ProcessRole::Workload);

        attributor.update(
            &platform,
            &mut [
                proc(root, 1, root.pid, CLAUDE, &["claude"]),
                proc_unreadable(shell, root.pid, shell.pid),
            ],
            now + Duration::from_secs(1),
            1000,
            false,
        );

        let s3 = attributor.update(
            &platform,
            &mut [
                proc(root, 1, root.pid, CLAUDE, &["claude"]),
                proc(shell, root.pid, shell.pid, CODEX, &["codex"]),
            ],
            now + Duration::from_secs(2),
            2000,
            false,
        );
        let shell_attr = attr(&s3, shell);
        assert_eq!(
            shell_attr.role,
            ProcessRole::AgentRoot,
            "once the shell's exe positively resolves to a recognized agent binary, it must \
             become its own nested agent root -- the durable tool-shell bit from before never \
             overrides a positive kind change"
        );
        assert_ne!(
            shell_attr.agent_id.as_deref(),
            Some(outer_agent.as_str()),
            "the nested agent root must be a distinct agent from the outer root, not folded back \
             into the outer agent's workload"
        );
    }

    /// Traced from property seed `16174314706627908250` (scratch/t04-fixup-4/seed-trace.log,
    /// pinned as a seed ratchet in `property::recovery_regressions` above): a Claude-session
    /// claim pointing at a genuinely Codex binary is positively `Invalid` while the target is
    /// observable (cross-kind, not merely uncertain). `validate`'s early-return-on-omission path
    /// only distinguishes "was this candidate ever `Valid`", not "was it ever positively
    /// `Invalid`", so once the target merely goes omitted the same claim reads back `Unknown` and
    /// the provisional-fallback lookup can then opportunistically attach it to the target's own,
    /// independently-retained real agent -- a claim the daemon already disproved must not spring
    /// back to valid just because its target stopped being observable.
    #[test]
    fn a_cross_kind_explicit_claim_invalidated_while_observable_must_not_reattach_once_its_target_is_omitted()
     {
        let claimant = id(300, 5);
        let target = id(301, 3);
        let mut platform = FakePlatform::default().env(
            claimant,
            &[("CLAUDE_CODE_SESSION_ID", "sess"), ("CLAUDE_PID", "301")],
        );
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();

        let s1 = attributor.update(
            &platform,
            &mut [
                proc(target, 1, 301, CODEX, &["codex"]),
                proc(claimant, 1, 300, "/usr/bin/node", &["node", "claimant"]),
            ],
            now,
            0,
            false,
        );
        assert_eq!(
            attr(&s1, target).role,
            ProcessRole::AgentRoot,
            "the target must establish as its own real codex root while observable, for this \
             probe to mean anything"
        );
        let target_agent = attr(&s1, target).agent_id.clone();
        assert_ne!(
            attr(&s1, claimant).agent_id,
            target_agent,
            "a Claude-session claim pointing at a genuinely Codex binary must be rejected while \
             the target is observable, not folded into the target's real codex agent"
        );

        platform = platform.pid_presence(target.pid, None);
        let s2 = attributor.update(
            &platform,
            &mut [proc(
                claimant,
                1,
                300,
                "/usr/bin/node",
                &["node", "claimant"],
            )],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        assert_ne!(
            attr(&s2, claimant).agent_id,
            target_agent,
            "once the target is merely omitted (not even a definitive answer), a claim already \
             positively invalidated while the target was observable must not spring back to \
             valid and attach to the target's own, independently-retained agent -- claimant \
             agent {:?}, target's established agent {:?}",
            attr(&s2, claimant).agent_id,
            target_agent
        );
    }

    /// P3 identity-rejected durability: a candidate positively contradicted by identity/UID/age
    /// evidence (start-time inversion here; the two tests below cover foreign-uid and PID reuse)
    /// must stay rejected once its target later goes merely omitted (unknown liveness), not
    /// resurrect into a provisional/Unknown attachment just because the contradicting evidence
    /// itself is no longer freshly observable.
    #[test]
    fn a_claude_pid_marker_pointing_at_a_younger_process_stays_rejected_once_the_target_is_later_omitted()
     {
        let younger_claude = id(410, 5);
        let claimant = id(411, 1);
        let mut platform = FakePlatform::default().env(
            claimant,
            &[
                ("CLAUDE_CODE_SESSION_ID", "sess-badorder2"),
                ("CLAUDE_PID", "410"),
            ],
        );
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let s1 = attributor.update(
            &platform,
            &mut [
                proc(younger_claude, 1, 410, CLAUDE, &[]),
                proc(claimant, 1, 411, "/usr/bin/something", &[]),
            ],
            now,
            0,
            false,
        );
        let claude_root_agent = attr(&s1, younger_claude).agent_id.clone();
        assert_eq!(attr(&s1, younger_claude).role, ProcessRole::AgentRoot);
        assert_ne!(
            attr(&s1, claimant).agent_id,
            claude_root_agent,
            "the start_time-inverted target must be rejected while observable, for this probe \
             to mean anything"
        );

        platform = platform.pid_presence(410, None);
        let s2 = attributor.update(
            &platform,
            &mut [proc(claimant, 1, 411, "/usr/bin/something", &[])],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        assert_ne!(
            attr(&s2, claimant).agent_id,
            claude_root_agent,
            "once the rejected target merely goes omitted, the claim must stay rejected, not \
             resurrect into a provisional attachment to that same identity"
        );
    }

    /// A `CLAUDE_PID` reference whose target is a foreign-uid process is not a live candidate
    /// (Ballast never attributes or reads across the uid boundary), so the claimant resolves as
    /// the "marker with no resolvable root" rootless case: an already-ended agent keyed by kind
    /// and session, root=None -- never the foreign identity's own root, and never plain
    /// unattributed either (the marker itself is still positive evidence of a session).
    #[test]
    fn a_claude_pid_marker_pointing_at_a_foreign_uid_process_resolves_a_rootless_ended_agent_not_the_foreign_root()
     {
        let foreign_root = id(1050, 1);
        let claimant = id(1060, 1);
        let mut platform = FakePlatform::default().env(
            claimant,
            &[
                ("CLAUDE_CODE_SESSION_ID", "sess-foreign2"),
                ("CLAUDE_PID", "1050"),
            ],
        );
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let s1 = attributor.update(
            &platform,
            &mut [
                proc_uid(
                    foreign_root,
                    1,
                    1050,
                    CLAUDE,
                    &[],
                    own_uid().wrapping_add(1),
                ),
                proc(claimant, 1, 1060, "/usr/bin/something", &[]),
            ],
            now,
            0,
            false,
        );
        let claimant_agent_id = attr(&s1, claimant).agent_id.clone();
        let claimant_agent = claimant_agent_id.as_ref().map(|id| agent_of(&s1, id));
        assert!(
            claimant_agent.is_some_and(|a| a.root.is_none() && a.state == AgentState::Ended),
            "a CLAUDE_PID reference to a foreign-uid process must resolve as a rootless, \
             already-ended agent, not bind to the foreign root's own identity -- claimant agent \
             {:?}",
            claimant_agent_id
        );

        platform = platform.pid_presence(1050, None);
        let s2 = attributor.update(
            &platform,
            &mut [proc(claimant, 1, 1060, "/usr/bin/something", &[])],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let claimant_agent2_id = attr(&s2, claimant).agent_id.clone();
        let claimant_agent2 = claimant_agent2_id.as_ref().map(|id| agent_of(&s2, id));
        assert!(
            claimant_agent2.is_some_and(|a| a.root.is_none() && a.state == AgentState::Ended),
            "once the never-valid foreign-uid target merely goes omitted, the claimant must stay \
             on its rootless ended agent, not spring into a live attachment -- claimant agent \
             {:?}",
            claimant_agent2_id
        );
    }

    /// A different, younger identity now occupying a bound `CLAUDE_PID` target is positive
    /// confirmation the original root is gone: the original agent must end (root=None,
    /// state=Ended) and the claimant must never bind to the replacement as if it were a live
    /// continuation of the original session -- and this must hold up through a later omission of
    /// the replacement too, not just the tick it first appears.
    #[test]
    fn a_claude_pid_markers_target_pid_reused_by_an_unrelated_process_ends_the_original_agent_and_never_binds_the_replacement()
     {
        let original = id(900, 1);
        let claimant = id(901, 1);
        let mut platform = FakePlatform::default().env(
            claimant,
            &[
                ("CLAUDE_CODE_SESSION_ID", "sess-reuse2"),
                ("CLAUDE_PID", "900"),
            ],
        );
        let mut attributor = Attributor::new(Vec::new(), Vec::new());
        let now = Instant::now();
        let s1 = attributor.update(
            &platform,
            &mut [
                proc(original, 1, 900, CLAUDE, &[]),
                proc(claimant, 1, 901, "/usr/bin/something", &[]),
            ],
            now,
            0,
            false,
        );
        let original_agent_id = attr(&s1, original).agent_id.clone().unwrap();
        assert_eq!(attr(&s1, original).role, ProcessRole::AgentRoot);
        assert_eq!(
            attr(&s1, claimant).agent_id.as_deref(),
            Some(original_agent_id.as_str()),
            "the claimant must bind to the original identity while it is observable, for this \
             probe to mean anything"
        );

        let reused = id(900, 2);
        let s2 = attributor.update(
            &platform,
            &mut [
                proc(reused, 1, 900, "/usr/bin/unrelated", &[]),
                proc(claimant, 1, 901, "/usr/bin/something", &[]),
            ],
            now + Duration::from_secs(1),
            1000,
            false,
        );
        let original_after_reuse = agent_of(&s2, &original_agent_id);
        assert_eq!(
            (original_after_reuse.root, original_after_reuse.state),
            (None, AgentState::Ended),
            "a different, younger identity now occupying the bound pid confirms the original \
             root gone -- the original agent must end (root=None, state=Ended), not silently \
             keep reporting a live root"
        );
        if let Some(id) = &attr(&s2, claimant).agent_id {
            let agent = agent_of(&s2, id);
            assert_ne!(
                agent.root,
                Some(reused),
                "the claimant must never bind to the replacement identity as if it were a live \
                 continuation of the original session -- agent {id} root {:?}",
                agent.root
            );
        }

        platform = platform.pid_presence(900, None);
        let s3 = attributor.update(
            &platform,
            &mut [proc(claimant, 1, 901, "/usr/bin/something", &[])],
            now + Duration::from_secs(2),
            2000,
            false,
        );
        let original_after_omission = agent_of(&s3, &original_agent_id);
        assert_eq!(
            (original_after_omission.root, original_after_omission.state),
            (None, AgentState::Ended),
            "the original agent must remain ended through a later omission of the replacement, \
             not revert to some uncertain or live state"
        );
        if let Some(id) = &attr(&s3, claimant).agent_id {
            let agent = agent_of(&s3, id);
            assert_ne!(
                agent.root,
                Some(reused),
                "the claimant must never bind to the (now omitted) replacement identity either"
            );
        }
    }
}

// ---------------------------------------------------------------------
// Fixup-3 property test: generated process forests, run through several ticks with markers,
// known/unknown binaries, reparenting, exits, PID reuse and per-read failures, checked for the
// invariants the fixup-2 review's individual findings only sampled one hand-picked case each of.
//
// The scenario "tree" is built deterministically from a u64 seed by a tiny local PRNG rather than
// by composing dozens of proptest strategies, which keeps the generator readable at the cost of
// proptest's usual per-field shrinking; a smaller seed is not assumed to fold to a simpler case,
// so failures are diagnosed from the printed scenario dump, not from seed shrinking. Two
// deliberate scope limits: every generated process shares its own pgid (pgid-based
// detached-workload grouping has dedicated unit coverage) and every process runs as the same uid
// (foreign-uid handling likewise has dedicated unit coverage).
// ---------------------------------------------------------------------

mod property {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;

    /// Small, fast, seedable PRNG -- not for security use, only for deriving a reproducible
    /// scenario from a single integer.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn range(&mut self, bound: u64) -> u64 {
            if bound == 0 {
                0
            } else {
                self.next_u64() % bound
            }
        }
        fn chance(&mut self, percent: u64) -> bool {
            self.range(100) < percent
        }
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Kind {
        Claude,
        Codex,
        Cursor,
        VersionedClaudeMissingArgv,
        Plain,
        ToolShell,
    }
    fn pick_kind(rng: &mut Rng) -> Kind {
        match rng.range(100) {
            0..30 => Kind::Plain,
            30..45 => Kind::ToolShell,
            45..60 => Kind::Claude,
            60..75 => Kind::Codex,
            75..85 => Kind::Cursor,
            _ => Kind::VersionedClaudeMissingArgv,
        }
    }
    /// The kind of known-agent-family binary this node truly is, ground truth from the
    /// generator itself (not derived from any snapshot): `None` for `Plain`/`ToolShell`.
    fn true_candidate_kind(kind: Kind) -> Option<&'static str> {
        match kind {
            Kind::Claude | Kind::VersionedClaudeMissingArgv => Some("claude"),
            Kind::Codex => Some("codex"),
            Kind::Cursor => Some("cursor"),
            Kind::Plain | Kind::ToolShell => None,
        }
    }
    /// (true exe, true argv) once fully readable/recovered.
    fn true_binary(kind: Kind, index: usize) -> (String, Vec<String>) {
        match kind {
            Kind::Claude => (CLAUDE.into(), vec![]),
            Kind::Codex => (CODEX.into(), vec![]),
            Kind::Cursor => ("/usr/bin/cursor-agent".into(), vec![]),
            Kind::VersionedClaudeMissingArgv => {
                (CLAUDE_VERSIONS_PATH.into(), vec!["claude".into()])
            }
            Kind::Plain => (format!("/usr/bin/plain-{index}"), vec![]),
            Kind::ToolShell => (
                "/bin/sh".into(),
                vec!["sh".into(), "-c".into(), "run".into()],
            ),
        }
    }

    /// A custom marker whose `root_pid_key` makes it explicit and same-kind ("codex") as the
    /// built-in `CODEX_THREAD_ID` ancestry marker, so the generator can actually produce the
    /// review's competing explicit-vs-inferred-same-kind shape instead of only cross-kind
    /// competition.
    fn codex_explicit_marker() -> Marker {
        Marker {
            key: "PROPTEST_CODEX_SESSION".into(),
            level: MarkerLevel::Agent,
            name_key: None,
            kind: Some("codex".into()),
            root_pid_key: Some("PROPTEST_CODEX_ROOT".into()),
            root_binaries: vec!["codex".into()],
            session_id: true,
        }
    }
    fn new_attributor() -> Attributor {
        Attributor::new(vec![codex_explicit_marker()], Vec::new())
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum MarkerChoice {
        None,
        ClaudeExplicit,
        CodexAncestry,
        CodexExplicit,
    }
    impl MarkerChoice {
        fn is_explicit(self) -> bool {
            matches!(self, Self::ClaudeExplicit | Self::CodexExplicit)
        }
    }

    struct Node {
        kind: Kind,
        parent: Option<usize>,
        marker: MarkerChoice,
        explicit_target: Option<usize>,
        /// Runs under a different uid than the daemon's own: production never reads its
        /// environment or attributes it, and an ancestry walk must stop at it rather than pass
        /// through -- independent of whatever its own exe/argv would otherwise suggest.
        foreign_uid: bool,
        /// Its own environment read never succeeds, for its entire observed lifetime -- distinct
        /// from a transient per-tick failure, matching production's own "read once per identity,
        /// cache forever" contract (a failed read is cached too, never retried absent an exec).
        env_permanently_unknown: bool,
    }

    fn node_pid(index: usize) -> i32 {
        7000 + index as i32 * 10
    }
    /// Birth order matches generation order (a node's parent index is always smaller than its
    /// own), so start times increase with index: this exercises the real
    /// `parent.start_time <= child.start_time` ancestry check instead of trivially satisfying it
    /// with a shared constant.
    fn node_start(index: usize) -> u64 {
        1 + index as u64
    }
    fn node_id(index: usize) -> ProcessIdentity {
        id(node_pid(index), node_start(index))
    }
    /// A PID-reuse replacement is a different process at the same OS-recycled pid: its start
    /// time must be strictly newer than any original node's (and than any node that might still
    /// hold an explicit reference to the old occupant), which this offset guarantees regardless
    /// of forest size.
    fn reused_id(index: usize) -> ProcessIdentity {
        id(node_pid(index), 1_000_000 + index as u64)
    }

    fn build_nodes(rng: &mut Rng) -> Vec<Node> {
        let len = 3 + rng.range(6) as usize; // 3..=8
        let mut nodes = Vec::with_capacity(len);
        for i in 0..len {
            let parent = if i > 0 && rng.chance(70) {
                Some(rng.range(i as u64) as usize)
            } else {
                None
            };
            nodes.push(Node {
                kind: pick_kind(rng),
                parent,
                marker: MarkerChoice::None,
                explicit_target: None,
                foreign_uid: rng.chance(10),
                env_permanently_unknown: rng.chance(10),
            });
        }
        for node in nodes.iter_mut() {
            node.marker = match rng.range(100) {
                0..15 => MarkerChoice::ClaudeExplicit,
                15..30 => MarkerChoice::CodexAncestry,
                30..45 => MarkerChoice::CodexExplicit,
                _ => MarkerChoice::None,
            };
            if node.marker.is_explicit() {
                node.explicit_target = Some(rng.range(len as u64) as usize);
            }
        }
        nodes
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Presence {
        /// `omitted`: this tick's enumeration simply didn't include the process at all (a
        /// transient gap, not a confirmed exit) -- distinct from `unreadable`, where the process
        /// is listed but its own exe/argv read failed.
        Live {
            unreadable: bool,
            detached: bool,
            omitted: bool,
        },
        Exited,
    }
    /// Whether observable evidence this tick leaves the node's own state genuinely undetermined
    /// (its exe read failed, or it was skipped by enumeration entirely) -- as opposed to exited,
    /// which is a positive, confirmed absence.
    fn currently_unresolved(presence: Presence) -> bool {
        matches!(
            presence,
            Presence::Live {
                unreadable: true,
                ..
            } | Presence::Live { omitted: true, .. }
        )
    }

    struct Scenario {
        nodes: Vec<Node>,
        presence: Vec<Presence>,
        reuse_active: Vec<bool>,
        gone: Vec<ProcessIdentity>,
        /// Set on some exits alongside `gone`: while true, `build_table_and_platform` withholds
        /// that identity's confirmed-Gone liveness from every *mutation* tick's platform (its
        /// liveness reads back Unknown, like any other not-yet-confirmed exit), and only supplies
        /// it at the final convergence tick -- the "PID reuse without confirmed-Gone evidence"
        /// class from the fixup-4 ticket: the only evidence available during those ticks is
        /// whatever a same-pid replacement's differing start_time reveals through ordinary
        /// identity-mismatch validation, never an explicit liveness confirmation.
        confirmed_delayed: Vec<bool>,
    }
    impl Scenario {
        fn new(nodes: Vec<Node>) -> Self {
            let len = nodes.len();
            Self {
                nodes,
                presence: vec![
                    Presence::Live {
                        unreadable: false,
                        detached: false,
                        omitted: false,
                    };
                    len
                ],
                reuse_active: vec![false; len],
                gone: Vec::new(),
                confirmed_delayed: vec![false; len],
            }
        }
        /// Applies one round of random per-node events; a node that exits this round is
        /// eligible to have its pid reused by an unrelated replacement from the next tick on.
        /// Once permanently reparented to pid 1 or exited, a node stays that way: neither event
        /// is reversed by a later "recovery" tick (see `build_table_and_platform`).
        fn mutate(&mut self, rng: &mut Rng) {
            for i in 0..self.nodes.len() {
                let Presence::Live {
                    unreadable,
                    detached,
                    omitted,
                } = &mut self.presence[i]
                else {
                    continue;
                };
                match rng.range(100) {
                    0..8 => {
                        self.gone.push(node_id(i));
                        self.presence[i] = Presence::Exited;
                        if rng.chance(35) {
                            self.confirmed_delayed[i] = true;
                        }
                        if rng.chance(40) {
                            self.reuse_active[i] = true;
                        }
                    }
                    8..18 => *detached = true,
                    18..30 => *unreadable = true,
                    30..42 => *unreadable = false,
                    // A transient enumeration gap: this tick's scan simply missed the process
                    // (unknown liveness/PID presence), distinct from a confirmed exit.
                    42..50 => *omitted = true,
                    50..58 => *omitted = false,
                    _ => {}
                }
            }
        }
        /// Applies one round of read-only events: a node's exe/argv become unreadable or
        /// recover, or it drops out of / back into enumeration. No exit, detach, or any other
        /// topology/marker change -- for the fixed-topology recovery property, where the only
        /// thing that ever varies is whether reads and enumeration succeed.
        fn mutate_reads_only(&mut self, rng: &mut Rng) {
            for i in 0..self.nodes.len() {
                let Presence::Live {
                    unreadable,
                    omitted,
                    ..
                } = &mut self.presence[i]
                else {
                    continue;
                };
                match rng.range(100) {
                    0..30 => *unreadable = true,
                    30..55 => *unreadable = false,
                    55..70 => *omitted = true,
                    70..85 => *omitted = false,
                    _ => {}
                }
            }
        }
        /// Every non-exited node keeps exactly the topology (ppid) its last mutation left it
        /// with -- a "recovery" tick restores readability only, never reverses a real reparent
        /// or exit, which would be an artificial topology reset with no realistic counterpart
        /// (a process cannot un-reparent itself back to a specific prior parent). Every
        /// referenced-but-absent identity gets explicit Gone liveness so the "every read
        /// succeeded" premise of a convergence tick is real, not vacuous.
        fn build_convergence_table_and_platform(&self) -> (Vec<Process>, FakePlatform) {
            self.build_table_and_platform(true)
        }
        fn build_table_and_platform(&self, convergence: bool) -> (Vec<Process>, FakePlatform) {
            let mut table = Vec::new();
            let mut platform = FakePlatform::default();
            for id in &self.gone {
                let delayed = (0..self.nodes.len())
                    .find(|&i| node_id(i) == *id)
                    .is_some_and(|i| self.confirmed_delayed[i]);
                if !delayed || convergence {
                    platform = platform.gone(*id);
                }
            }
            for (i, node) in self.nodes.iter().enumerate() {
                if let Presence::Live {
                    unreadable,
                    detached,
                    omitted,
                } = self.presence[i]
                {
                    let identity = node_id(i);
                    // A live process this tick's enumeration simply didn't report: unlike a
                    // confirmed exit (`self.gone`), presence itself is uncertain -- it is left
                    // out of the table entirely and its PID presence reads back `None`, not the
                    // platform's default confirmed-absent.
                    if omitted && !convergence {
                        platform = platform.pid_presence(identity.pid, None);
                        continue;
                    }
                    let ppid = if detached {
                        1
                    } else {
                        node.parent.map_or(1, node_pid)
                    };
                    let fully_unreadable = unreadable && !convergence;
                    let (exe, argv) = true_binary(node.kind, i);
                    let has_argv = convergence || node.kind != Kind::VersionedClaudeMissingArgv;
                    table.push(Process {
                        identity,
                        ppid,
                        pgid: identity.pid,
                        uid: if node.foreign_uid {
                            own_uid().wrapping_add(1)
                        } else {
                            own_uid()
                        },
                        stopped: false,
                        name: None,
                        exe: (!fully_unreadable).then(|| exe.clone()),
                        argv: (!fully_unreadable && has_argv).then(|| argv.clone()),
                        metrics: None,
                    });
                    match node.marker {
                        MarkerChoice::ClaudeExplicit => {
                            let target = node.explicit_target.unwrap_or(i);
                            platform = platform.env(
                                identity,
                                &[
                                    ("CLAUDE_CODE_SESSION_ID", &format!("sess-{i}")),
                                    ("CLAUDE_PID", &format!("{}", node_pid(target))),
                                ],
                            );
                        }
                        MarkerChoice::CodexAncestry => {
                            platform = platform
                                .env(identity, &[("CODEX_THREAD_ID", &format!("thread-{i}"))]);
                        }
                        MarkerChoice::CodexExplicit => {
                            let target = node.explicit_target.unwrap_or(i);
                            platform = platform.env(
                                identity,
                                &[
                                    ("PROPTEST_CODEX_SESSION", &format!("explicit-{i}")),
                                    ("PROPTEST_CODEX_ROOT", &format!("{}", node_pid(target))),
                                ],
                            );
                        }
                        // A known-empty environment (not FakePlatform's unset default, which
                        // reads back as unknown) -- a real "every read succeeds" state must not
                        // leave an unmarked process's own environment read still failing.
                        MarkerChoice::None => {
                            platform = platform.env(identity, &[]);
                        }
                    }
                    // A foreign-uid process's environment is never read at all in production
                    // (the uid filter runs before any env access); one whose own read never
                    // succeeds (env_permanently_unknown) never gets past that either, no matter
                    // what marker it would otherwise carry -- both override whatever the match
                    // above just set, including at convergence, since neither ever recovers.
                    if node.foreign_uid || node.env_permanently_unknown {
                        platform = platform.unknown_env(identity);
                    }
                    // A known, stable age/memory sample per identity -- this generator never
                    // models an age/metrics read failure, only exe/argv/liveness, so these must
                    // always read back as known rather than silently leaving them unknown too.
                    platform = platform.age(identity, Duration::from_secs(3600));
                    platform = platform.metrics(identity, 0);
                }
                if self.reuse_active[i] {
                    let reused = reused_id(i);
                    table.push(Process {
                        identity: reused,
                        ppid: 1,
                        pgid: node_pid(i),
                        uid: own_uid(),
                        stopped: false,
                        name: None,
                        exe: Some(format!("/usr/bin/plain-reused-{i}")),
                        argv: Some(vec![]),
                        metrics: None,
                    });
                    platform = platform.env(reused, &[]);
                    platform = platform.age(reused, Duration::from_secs(3600));
                    platform = platform.metrics(reused, 0);
                }
            }
            (table, platform)
        }
    }

    /// No process may hold `Workload` while its agent is still rootless-and-Unknown: findings 1
    /// and 2 were both, at heart, a violation of exactly this.
    fn check_no_workload_under_an_uncertain_agent(
        snapshot: &AttributionSnapshot,
    ) -> Result<(), TestCaseError> {
        for p in &snapshot.processes {
            if p.role != ProcessRole::Workload {
                continue;
            }
            let Some(agent_id) = &p.agent_id else {
                continue;
            };
            let Some(agent) = snapshot.agents.iter().find(|a| &a.id == agent_id) else {
                continue;
            };
            prop_assert!(
                !(agent.root.is_none() && agent.state == AgentState::Unknown),
                "pid {} is Workload under agent {} which is still rootless and Unknown",
                p.identity.pid,
                agent.id
            );
        }
        Ok(())
    }

    /// Structural invariant, independent of the generator: every id a process points at must
    /// resolve to a real record, and a workload's own `agent_id` must always agree with the
    /// `agent_id` of every process that names that workload.
    fn check_workload_and_agent_ids_resolve_and_agree(
        snapshot: &AttributionSnapshot,
    ) -> Result<(), TestCaseError> {
        for p in &snapshot.processes {
            if let Some(agent_id) = &p.agent_id {
                prop_assert!(
                    snapshot.agents.iter().any(|a| &a.id == agent_id),
                    "pid {}: agent_id {} does not resolve to any agent record",
                    p.identity.pid,
                    agent_id
                );
            }
            let Some(workload_id) = &p.workload_id else {
                continue;
            };
            let Some(workload) = snapshot.workloads.iter().find(|w| &w.id == workload_id) else {
                return Err(TestCaseError::fail(format!(
                    "pid {}: workload_id {} does not resolve to any workload record",
                    p.identity.pid, workload_id
                )));
            };
            prop_assert_eq!(
                Some(workload.agent_id.clone()),
                p.agent_id.clone(),
                "pid {}: workload {} is owned by agent {}, but the process itself is \
                 attributed to a different agent {:?} -- two distinct workloads sharing this \
                 id (one retained, one freshly resolved) must not overwrite each other's record",
                p.identity.pid,
                workload_id,
                workload.agent_id,
                p.agent_id
            );
        }
        Ok(())
    }

    /// `ever_shown_candidate[i]` tracks, from observable evidence only, whether node `i` has at
    /// any tick up to now actually shown a readable exe matching its own kind's real binary
    /// path. An exe that has never once been readable, with no marker of its own or as another
    /// node's explicit target, gives the real attributor nothing to go on, so it is never
    /// required to protect one (see `compute_protected_nodes`).
    fn update_ever_shown_candidate(scenario: &Scenario, ever_shown_candidate: &mut [bool]) {
        for (i, node) in scenario.nodes.iter().enumerate() {
            let Presence::Live {
                unreadable,
                omitted,
                ..
            } = scenario.presence[i]
            else {
                continue;
            };
            if true_candidate_kind(node.kind).is_none() {
                continue;
            }
            if !unreadable && !omitted {
                ever_shown_candidate[i] = true;
            }
        }
    }
    /// `ever_observed[i]` tracks, sticky, whether node `i` has at any tick up to now actually
    /// appeared in the process table at all (`omitted: false`) -- independent of whether its exe
    /// matched a recognized kind. A marker lives in environment variables, which the daemon can
    /// only have read on a tick where the process was actually enumerated; a node omitted from
    /// every tick so far has never had its marker read, so it cannot yet be the source of any
    /// claim, explicit or ancestry (see `compute_protected_nodes`). Once observed, a marker's
    /// claim persists through a later omission, exactly like established candidate evidence.
    fn update_ever_observed(scenario: &Scenario, ever_observed: &mut [bool]) {
        for (i, presence) in scenario.presence.iter().enumerate() {
            if let Presence::Live { omitted: false, .. } = presence {
                ever_observed[i] = true;
            }
        }
    }
    /// Whether node `i`'s own environment has actually been read successfully at least once --
    /// distinct from `ever_observed[i]` (its *row* has been seen), which a foreign-uid process or
    /// one whose read never succeeds can satisfy while still never revealing a marker: production
    /// never reads environment across the uid boundary at all, and a failed read is cached and
    /// never retried absent an exec. `foreign_uid`/`env_permanently_unknown` are fixed per-node
    /// properties, so this needs no separate sticky tracker of its own -- `ever_observed[i]`
    /// already is one. Anywhere `ever_observed` was really standing in for "this node's own
    /// marker could have been read" (as opposed to "this node's identity/start_time is known"),
    /// this is the correct signal to use instead.
    fn env_known(scenario: &Scenario, ever_observed: &[bool], i: usize) -> bool {
        ever_observed[i]
            && !scenario.nodes[i].foreign_uid
            && !scenario.nodes[i].env_permanently_unknown
    }
    // Keep each observed explicit claim separate. Known target identities retain pending
    // evidence after claimant exit until a positive read or confirmed death resolves it.
    // An unbound PID after claimant exit needs a surviving-context model, tracked as follow-up.
    /// Mirrors production's stale-carrier ancestry guard (`Claim`'s age check extended to the
    /// claimant's own observed ancestry, roots.rs `discover_roots`): a claim is a positive stale
    /// carrier -- and so invalid regardless of the target's own current readability -- when a
    /// process on the claimant's real ancestry path, strictly between it and the referenced root,
    /// carries the exact same marker and the exact same explicit target, and itself started
    /// before that root. Such an ancestor proves the root cannot actually be an ancestor of this
    /// lineage, independent of whatever the target itself currently reads as. A self-referencing
    /// claim (target == claimant) is not exempt: the walk still follows the claimant's own real
    /// ancestry looking for a carrier that names the claimant itself as root.
    ///
    /// The target's own start time is only usable evidence once the target has actually been
    /// observed at least once (`ever_observed[target]`) -- the generator's hidden index must not
    /// stand in for it, exactly like the born-after-claimant check above. Every per-identity
    /// field the daemon uses here -- ppid link and env alike -- is *cached*, retained memory
    /// (`Cached` holds "the last observed uid, exe, ppid and pgid"), not a fresh read this tick:
    /// an ancestor that is merely omitted or unreadable this tick still contributes its last
    /// observed ppid/env, exactly like any other retained evidence in this module. Only a
    /// positively confirmed exit drops an identity's cache entry and so actually breaks the
    /// walk -- along with reaching the target itself, or a node never yet observed at all (no
    /// cached ppid to walk through). Detachment is the one exception that still applies going
    /// forward, not backward: it is itself positive, currently-observed evidence of a real
    /// topology change (the row must be present and readable to report ppid=1 at all), so a
    /// detached ancestor's own current parent is genuinely gone and the walk cannot continue past
    /// it using the generator's historical parent field -- but detachment does not retroactively
    /// invalidate the *ancestor's own* cached identity as a potential carrier, checked before that
    /// stop.
    fn claim_is_stale_carrier(
        scenario: &Scenario,
        ever_observed: &[bool],
        claimant: usize,
    ) -> bool {
        let node = &scenario.nodes[claimant];
        if !node.marker.is_explicit() {
            return false;
        }
        let target = node.explicit_target.unwrap_or(claimant);
        if !ever_observed[target] {
            return false;
        }
        if matches!(
            scenario.presence[claimant],
            Presence::Live { detached: true, .. }
        ) {
            return false;
        }
        let target_start = node_start(target);
        let mut current = node.parent;
        while let Some(a) = current {
            if a == target {
                break;
            }
            // A foreign-uid ancestor is a hard boundary in production (`ancestors` itself stops
            // there), independent of whether its own ppid link would otherwise be knowable. An
            // exited ancestor stops the walk only once its exit is actual evidence -- a confirmed
            // Gone liveness, or an observed pid-reuse replacement now occupying its slot -- not
            // merely the generator's own hidden ground truth: production retains the last
            // observed ppid/env for an identity whose liveness is still merely Unknown, exactly
            // like an omission, and keeps searching through it.
            let confirmed_exit = matches!(scenario.presence[a], Presence::Exited)
                && (!scenario.confirmed_delayed[a] || scenario.reuse_active[a]);
            if !ever_observed[a] || confirmed_exit || scenario.nodes[a].foreign_uid {
                break;
            }
            // Using `a` as evidence needs its *environment*, not just its row, to have actually
            // been read -- a node whose own env read never succeeded contributes no marker
            // evidence, but its ppid link is still real and the walk may continue past it.
            if env_known(scenario, ever_observed, a) {
                let carrier = &scenario.nodes[a];
                let same_claim =
                    carrier.marker == node.marker && carrier.explicit_target.unwrap_or(a) == target;
                if same_claim && node_start(a) < target_start {
                    return true;
                }
            }
            if matches!(scenario.presence[a], Presence::Live { detached: true, .. }) {
                break;
            }
            current = scenario.nodes[a].parent;
        }
        false
    }

    fn update_claimant_pending_target_evidence(
        scenario: &Scenario,
        ever_observed: &[bool],
        claimant_pending_target_evidence: &mut [bool],
    ) {
        for (i, node) in scenario.nodes.iter().enumerate() {
            if !node.marker.is_explicit() {
                continue;
            }
            let target = node.explicit_target.unwrap_or(i);
            if target == i {
                continue;
            }
            if !matches!(scenario.presence[i], Presence::Live { .. }) && !ever_observed[target] {
                claimant_pending_target_evidence[i] = false;
                continue;
            }
            if ever_observed[target] && node_start(target) > node_start(i) {
                claimant_pending_target_evidence[i] = false;
                continue;
            }
            if ever_observed[target] && scenario.nodes[target].foreign_uid {
                claimant_pending_target_evidence[i] = false;
                continue;
            }
            if matches!(scenario.presence[target], Presence::Exited) {
                claimant_pending_target_evidence[i] = false;
                continue;
            }
            if matches!(
                scenario.presence[target],
                Presence::Live {
                    unreadable: false,
                    omitted: false,
                    ..
                }
            ) && scenario.nodes[target].kind != Kind::VersionedClaudeMissingArgv
            {
                claimant_pending_target_evidence[i] = false;
                continue;
            }
            // Once a stale carrier positively disproves this claim, any pending-target evidence
            // already recorded from an earlier tick (before the carrier's evidence existed, or
            // before this walk accounted for it) must expire immediately, not sit stuck true --
            // a positively invalidated claim protects nothing, exactly like the read-as-wrong-
            // kind expiration case elsewhere in this module.
            if claim_is_stale_carrier(scenario, ever_observed, i) {
                claimant_pending_target_evidence[i] = false;
                continue;
            }
            if env_known(scenario, ever_observed, i)
                && matches!(scenario.presence[i], Presence::Live { .. })
                && currently_unresolved(scenario.presence[target])
            {
                claimant_pending_target_evidence[i] = true;
            }
        }
    }
    /// Ground-truth oracle independent of the snapshot's own self-reported agent state: which
    /// live nodes may not be `Workload` of some *other*, already-valid agent because their own
    /// current resolution is still ambiguous. `check_no_workload_under_an_uncertain_agent` alone
    /// cannot catch a dropped or pending claim reassigned to a different, unrelated, entirely
    /// valid outer agent.
    ///
    /// Evidence rules -- each a positive, observable fact, never inferred from the generator's
    /// own hidden ground truth:
    /// - An own-binary candidate (previously observed matching a known agent kind, or a
    ///   versioned-claude-missing-argv match, which never fully validates pre-convergence) whose
    ///   own read currently fails or is omitted stays pending on its own identity.
    /// - An explicit marker's target, once observed while itself unresolved, stays pending on
    ///   that target's identity for as long as the target itself stays unresolved -- independent
    ///   of whether the original claimant is still alive (production's `Claim::retained` keeps
    ///   this evidence too). A self-referencing claim degenerates to the claimant's own identity.
    ///   A claimant with independent binary evidence of its own is never barred just because its
    ///   own claim's target is pending.
    /// - An ancestry marker's claimant (marker observed at least once, sticky) is itself the
    ///   nearest candidate in its own root search: if its own read is currently unresolved,
    ///   protection is already on the claimant's own identity, no ancestor walk needed. Otherwise
    ///   the walk follows the claimant's *currently observed* PPID chain, protecting every
    ///   unresolved candidate found along the way (unreadable-but-listed keeps walking; a missing
    ///   row protects that identity and stops there, since nothing further behind it is
    ///   observable) up to the nearest positively validated root.
    /// - Protection propagates down through a protected node's own live, non-detached
    ///   descendants, except an ancestor protected purely as someone else's ancestry-walk
    ///   boundary, whose *other* children/cousins must not inherit it.
    ///
    /// `expected_root` is the one identity a propagated `Workload` may validly point at while
    /// still pending (a validated own-binary seed's own identity); everything else pending
    /// carries `never_workload` instead. Only a positively validated candidate may retain
    /// validity through a later gap and authorize `Workload`; a marker alone never counts as
    /// validation.
    fn compute_protected_nodes(
        scenario: &Scenario,
        ever_shown_candidate: &[bool],
        ever_observed: &[bool],
        claimant_pending_target_evidence: &[bool],
    ) -> (Vec<bool>, Vec<bool>, Vec<Option<ProcessIdentity>>) {
        let n = scenario.nodes.len();
        let mut protected = vec![false; n];
        let mut never_workload = vec![false; n];
        let mut expected_root: Vec<Option<ProcessIdentity>> = vec![None; n];
        // Propagates protection (and whatever authorization it carries, or lack thereof) down
        // through live, non-detached descendants. Left false for an ancestry-marker hit, whose
        // protection is scoped to exactly the marker-bearer and the one ambiguous ancestor.
        let mut propagates = vec![false; n];
        // Candidate evidence observed at all (own exe read matching a recognized kind), which is
        // enough to gate whether a node is even considered by these branches, but not enough on
        // its own to authorize `Workload` for anyone -- see `validated` below.
        let ever_observed_candidate = |i: usize| {
            true_candidate_kind(scenario.nodes[i].kind).is_some()
                && ever_shown_candidate[i]
                && !scenario.nodes[i].foreign_uid
        };
        // Positively validated: a real, confirmed candidate whose identity may be retained
        // through a later observation gap and may authorize `Workload` for itself or others.
        // Versioned-claude-missing-argv is deliberately excluded: it has candidate evidence (exe
        // matched) but can never fully validate (argv required, always missing pre-convergence).
        let validated = |i: usize| {
            ever_observed_candidate(i) && scenario.nodes[i].kind != Kind::VersionedClaudeMissingArgv
        };

        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            if !matches!(scenario.presence[i], Presence::Live { .. }) || !ever_observed_candidate(i)
            {
                continue;
            }
            if currently_unresolved(scenario.presence[i])
                || scenario.nodes[i].kind == Kind::VersionedClaudeMissingArgv
            {
                protected[i] = true;
                never_workload[i] = true;
                propagates[i] = true;
                if validated(i) {
                    expected_root[i] = Some(node_id(i));
                }
            }
        }

        for (i, node) in scenario.nodes.iter().enumerate() {
            // Last observed PPID is not modeled for a detach hidden by omission.
            if matches!(
                scenario.presence[i],
                Presence::Live {
                    detached: true,
                    omitted: true,
                    ..
                }
            ) {
                continue;
            }
            if !node.marker.is_explicit()
                || !matches!(scenario.presence[i], Presence::Live { .. })
                || !env_known(scenario, ever_observed, i)
            {
                continue;
            }
            let target = node.explicit_target.unwrap_or(i);
            if target == i {
                // A self-referencing explicit claim has exactly one candidate: the claimant
                // itself (production's root-pid-key search degenerates to its own pid). If its
                // own read has already validated it, it is its own resolved root; otherwise its
                // own currently-unresolved read leaves the claim (and its descendants) pending
                // on that same identity, exactly like an own-binary-pending seed -- unless a
                // stale carrier on the claimant's own real ancestry (a different node naming the
                // claimant itself as root, but itself older) positively disproves it; self-
                // reference is not exempt from that guard.
                if !validated(i)
                    && currently_unresolved(scenario.presence[i])
                    && !claim_is_stale_carrier(scenario, ever_observed, i)
                {
                    protected[i] = true;
                    never_workload[i] = true;
                    propagates[i] = true;
                }
                continue;
            }
            if validated(target) {
                continue;
            }
            // An explicit reference to a target observably born after the claimant is
            // positively invalid from the identities alone -- but only once the target's real
            // start time has actually been observed at least once. A target omitted from every
            // tick so far has never had its identity read at all, so its start time is not yet
            // knowable evidence; the hidden generator index used to compute `node_start` must
            // not stand in for it.
            if ever_observed[target] && node_start(target) > node_start(i) {
                continue;
            }
            // A target running under a foreign uid is positively invalid the moment its row
            // (and so its uid) has actually been observed -- production's own uid boundary
            // rejects it outright, same as the observably-born-after case above, regardless of
            // whether its exe/env happen to be currently readable.
            if ever_observed[target] && scenario.nodes[target].foreign_uid {
                continue;
            }
            // A stale carrier on the claimant's own ancestry (the same marker, the same target,
            // observed strictly between the claimant and the target, and itself older than the
            // target) positively disproves the claim regardless of the target's own current
            // readability -- production's ancestry-guard rejects it outright, so the oracle must
            // not treat the target as merely pending on this claim's account either.
            if claim_is_stale_carrier(scenario, ever_observed, i) {
                continue;
            }
            // A target already positively validated at an earlier tick stays resolved through a
            // later transient read failure (validated evidence is retained, not re-litigated);
            // only a target that has never yet validated leaves the claim naming it genuinely
            // ambiguous while unreadable/omitted.
            if currently_unresolved(scenario.presence[target]) {
                protected[target] = true;
                never_workload[target] = true;
                propagates[target] = true;
                // The claimant names the target as root, not itself, but the target is still
                // merely pending (Unknown validity, never yet confirmed) -- a pending candidate
                // authorizes nothing, so the claimant is equally barred from `Workload`, not
                // optimistically promoted under the target's still-unconfirmed identity. Unless
                // the claimant is *itself* already an independently validated real candidate of
                // its own (separate binary evidence, unrelated to what its marker's claim says
                // about the target) -- its own identity is already resolved regardless of how
                // that claim turns out, so it is left alone here entirely.
                if !validated(i) {
                    protected[i] = true;
                    never_workload[i] = true;
                    propagates[i] = true;
                }
            }
        }

        for (i, node) in scenario.nodes.iter().enumerate() {
            // Whether the marker-bearer even *carries* this marker at all only needs to have been
            // observed once, sticky, exactly like an explicit claim's cached env value -- a node
            // that has never been observed has never had its env read and so cannot yet be the
            // source of any claim. A *fresh* ancestor walk still needs this tick's own current
            // ppid links (below), but the claimant's own currently-omitted self is itself the
            // nearest unresolved candidate in that case and the walk never has to start at all
            // (see `currently_unresolved(i)` just below) -- so requiring the claimant to be
            // currently observed here, before even checking its own candidacy, wrongly skipped
            // protecting it (and its descendants) the moment it went omitted.
            if node.marker != MarkerChoice::CodexAncestry
                || !matches!(scenario.presence[i], Presence::Live { .. })
                || !env_known(scenario, ever_observed, i)
            {
                continue;
            }
            // A marker-bearer that is itself already a positively validated real candidate is
            // its own root -- it needs no ancestry search at all, so it neither needs protecting
            // itself (the own-binary-candidate branch above already covers it if it later goes
            // transiently unresolved) nor treats any ancestor as ambiguous on its behalf.
            if validated(i) {
                continue;
            }
            // Production's own candidate search starts at the claimant itself, not its parent:
            // if the claimant's own read currently fails, its own resolution is the nearest
            // ambiguity and the search stops right there, before even considering ancestors. Its
            // own subtree structurally belongs to this same still-pending claim, so (unlike an
            // ambiguous ancestor further up, whose *other* unrelated children/cousins must not
            // inherit protection) this protection does propagate to the claimant's descendants.
            if currently_unresolved(scenario.presence[i]) {
                protected[i] = true;
                never_workload[i] = true;
                propagates[i] = true;
                continue;
            }
            // The walk starts from `i`'s *current* ancestry, not the generator's historical
            // parent field: a node that has itself detached (reparented to pid 1) has no real
            // ancestor at this tick at all, regardless of who it used to report to.
            if matches!(scenario.presence[i], Presence::Live { detached: true, .. }) {
                continue;
            }
            let mut current = node.parent;
            while let Some(a) = current {
                // A foreign-uid ancestor is a hard boundary in production (`ancestors` stops
                // there outright) once its row has actually been observed and so its uid is
                // known/cached -- an ancestor never yet observed has an unknown uid, so this
                // falls through to the ordinary omitted/unreadable handling below instead.
                if scenario.nodes[a].foreign_uid && ever_observed[a] {
                    break;
                }
                if validated(a) {
                    break;
                }
                // An omitted ancestor's own row is absent this tick, but its identity (pid +
                // start time) is still known through the direct child's own observed ppid link,
                // even though that identity's *own* parent link is not (a's own row would have
                // to be observed to read it): protect it, then stop -- never walk past it using
                // the generator's hidden historical parent field.
                if matches!(scenario.presence[a], Presence::Live { omitted: true, .. }) {
                    protected[i] = true;
                    never_workload[i] = true;
                    propagates[i] = true;
                    protected[a] = true;
                    never_workload[a] = true;
                    break;
                }
                // An unreadable-but-listed ancestor is a *different* unresolved candidate on the
                // same fully observed path, not a boundary: its own ppid link is real observed
                // evidence (its row is present), so the walk keeps going past it, protecting
                // every unresolved candidate in turn, up to the nearest validated root or an
                // actual boundary (missing row, exit, detach).
                if matches!(
                    scenario.presence[a],
                    Presence::Live {
                        unreadable: true,
                        ..
                    }
                ) {
                    protected[i] = true;
                    never_workload[i] = true;
                    propagates[i] = true;
                    protected[a] = true;
                    never_workload[a] = true;
                }
                if matches!(
                    scenario.presence[a],
                    Presence::Live { detached: true, .. } | Presence::Exited
                ) {
                    break;
                }
                current = scenario.nodes[a].parent;
            }
        }

        // Each claimant's own retained pending-target evidence (see
        // `update_claimant_pending_target_evidence`) is applied independently -- distinct
        // claimants sharing the same target each carry their own un-collapsed constraint, so one
        // claimant's evidence being positively invalidated never expires another's. The target's
        // own current resolution is still re-checked here: a target that has since validated is
        // already covered by the normal validated-candidate path, and only a target still
        // currently unresolved is protected by this evidence.
        for (i, node) in scenario.nodes.iter().enumerate() {
            if matches!(
                scenario.presence[i],
                Presence::Live {
                    detached: true,
                    omitted: true,
                    ..
                }
            ) || !claimant_pending_target_evidence[i]
            {
                continue;
            }
            let target = node.explicit_target.unwrap_or(i);
            if target == i || validated(target) {
                continue;
            }
            if currently_unresolved(scenario.presence[target]) {
                protected[target] = true;
                never_workload[target] = true;
                propagates[target] = true;
            }
        }

        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..n {
                if protected[i] || validated(i) {
                    continue;
                }
                // Deriving protection for `i` through `scenario.nodes[i].parent` uses that same
                // edge candidate search already relies on: it is only actually knowable once `i`
                // itself has been observed this tick (its own real ppid), so an omitted `i`
                // cannot be newly protected this way -- doing so would derive an edge through an
                // unobserved intermediary using the generator's hidden parent field, not any
                // evidence the daemon could actually have this tick. A protected parent that is
                // itself omitted still protects its own *directly observed* children normally;
                // only `i`'s own omission blocks this specific inheritance step.
                if !matches!(
                    scenario.presence[i],
                    Presence::Live {
                        detached: false,
                        omitted: false,
                        ..
                    }
                ) {
                    continue;
                }
                if let Some(parent) = scenario.nodes[i].parent {
                    if protected[parent] && propagates[parent] {
                        protected[i] = true;
                        propagates[i] = true;
                        if let Some(root) = expected_root[parent] {
                            expected_root[i] = Some(root);
                        } else {
                            never_workload[i] = true;
                        }
                        changed = true;
                    }
                }
            }
        }
        (protected, never_workload, expected_root)
    }
    fn check_protected_processes_are_never_absorbed_as_workload(
        scenario: &Scenario,
        snapshot: &AttributionSnapshot,
        ever_shown_candidate: &[bool],
        ever_observed: &[bool],
        claimant_pending_target_evidence: &[bool],
        convergence: bool,
    ) -> Result<(), TestCaseError> {
        if convergence {
            return Ok(());
        }
        let (protected, never_workload, expected_root) = compute_protected_nodes(
            scenario,
            ever_shown_candidate,
            ever_observed,
            claimant_pending_target_evidence,
        );
        for (i, &is_protected) in protected.iter().enumerate() {
            if !is_protected {
                continue;
            }
            let Some(p) = snapshot.processes.iter().find(|p| p.identity == node_id(i)) else {
                continue;
            };
            if p.role != ProcessRole::Workload {
                continue;
            }
            if never_workload[i] {
                prop_assert!(
                    false,
                    "node {i} (pid {}) is itself an unresolved candidate/claim-target/ancestry-\
                     marker seed -- its own candidacy is unresolved this tick -- but was \
                     absorbed as Workload rather than left pending",
                    node_pid(i)
                );
            }
            // Otherwise this node may validly be Workload of exactly the agent rooted at its
            // seed's expected identity -- ordinary attribution continuing through a transient
            // hiccup (the seed's own process row may itself be this tick's omitted/unreadable
            // one and so absent from `snapshot.processes`, but its established root identity is
            // retained). Landing under any other agent (a different root, or none) is the leak.
            let root_matches = expected_root[i].is_some_and(|expected| {
                p.agent_id.as_ref().is_some_and(|id| {
                    snapshot
                        .agents
                        .iter()
                        .any(|a| &a.id == id && a.root == Some(expected))
                })
            });
            prop_assert!(
                root_matches,
                "node {i} (pid {}) is protected only via an unresolved candidate/claim rooted \
                 at {:?}, but was absorbed as Workload of an agent ({:?}) not actually rooted \
                 there",
                node_pid(i),
                expected_root[i],
                p.agent_id
            );
        }
        Ok(())
    }

    fn memory_equal(a: &MemorySummary, b: &MemorySummary) -> bool {
        a.bytes == b.bytes
            && a.complete == b.complete
            && a.growth_30s_bytes == b.growth_30s_bytes
            && a.growth_bytes_per_sec == b.growth_bytes_per_sec
    }
    fn processes_equal(a: &ProcessAttribution, b: &ProcessAttribution) -> bool {
        a.identity == b.identity
            && a.owner_id == b.owner_id
            && a.agent_id == b.agent_id
            && a.workload_id == b.workload_id
            && a.role == b.role
            && a.environment_known == b.environment_known
            && a.listening_ports == b.listening_ports
            && a.ports_sampled_at_ms == b.ports_sampled_at_ms
    }
    fn agents_equal(a: &Agent, b: &Agent) -> bool {
        a.id == b.id
            && a.owner_id == b.owner_id
            && a.session_id == b.session_id
            && a.kind == b.kind
            && a.root == b.root
            && a.cwd == b.cwd
            && a.state == b.state
            && a.ended_at_ms == b.ended_at_ms
            && memory_equal(&a.memory, &b.memory)
    }
    fn workloads_equal(a: &Workload, b: &Workload) -> bool {
        a.id == b.id
            && a.agent_id == b.agent_id
            && a.root == b.root
            && a.label == b.label
            && a.class == b.class
            && a.first_seen_ms == b.first_seen_ms
            && a.detached_pgid == b.detached_pgid
            && memory_equal(&a.memory, &b.memory)
    }
    /// Full structural equality after normalizing only vector order: the resolver's dirty-set
    /// processing sorts by identity for deterministic first allocation, so two runs of the exact
    /// same tick sequence that differ only in each tick's process-array enumeration order must
    /// land on identical snapshots, not just an equivalent partition.
    fn assert_snapshots_equal_modulo_order(
        a: &AttributionSnapshot,
        b: &AttributionSnapshot,
        context: &str,
    ) -> Result<(), TestCaseError> {
        let mut ap = a.processes.clone();
        let mut bp = b.processes.clone();
        ap.sort_by_key(|p| (p.identity.pid, p.identity.start_time));
        bp.sort_by_key(|p| (p.identity.pid, p.identity.start_time));
        prop_assert_eq!(ap.len(), bp.len(), "{}: process count differs", context);
        for (x, y) in ap.iter().zip(bp.iter()) {
            prop_assert!(
                processes_equal(x, y),
                "{}: process pid {} differs by enumeration order",
                context,
                x.identity.pid
            );
        }
        let mut aa = a.agents.clone();
        let mut ba = b.agents.clone();
        aa.sort_by(|x, y| x.id.cmp(&y.id));
        ba.sort_by(|x, y| x.id.cmp(&y.id));
        prop_assert_eq!(aa.len(), ba.len(), "{}: agent count differs", context);
        for (x, y) in aa.iter().zip(ba.iter()) {
            prop_assert!(
                agents_equal(x, y),
                "{}: agent {} differs by enumeration order",
                context,
                x.id
            );
        }
        let mut aw = a.workloads.clone();
        let mut bw = b.workloads.clone();
        aw.sort_by(|x, y| x.id.cmp(&y.id));
        bw.sort_by(|x, y| x.id.cmp(&y.id));
        prop_assert_eq!(aw.len(), bw.len(), "{}: workload count differs", context);
        for (x, y) in aw.iter().zip(bw.iter()) {
            prop_assert!(
                workloads_equal(x, y),
                "{}: workload {} differs by enumeration order",
                context,
                x.id
            );
        }
        Ok(())
    }

    /// A conceptual-membership signature, deliberately blind to id strings and to
    /// history-dependent scalars (first_seen_ms/ended_at_ms/cwd/memory/port timing): the
    /// convergence check compares this, not the raw snapshot, since a from-scratch resolution
    /// legitimately starts its own sampling cadences from zero.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Signature {
        role: ProcessRole,
        agent_kind: Option<String>,
        agent_root: Option<ProcessIdentity>,
        agent_session: Option<String>,
        agent_state: Option<AgentState>,
        workload_root: Option<ProcessIdentity>,
    }
    impl Signature {
        /// Same signature, ignoring `agent_session`: session is retained once established (a
        /// deliberate ratchet), so a fixed-topology recovery run may legitimately keep an
        /// earlier-established session even where a from-scratch fresh run, seeing every
        /// conflicting marker at once, would pick a different one. Everything else -- role,
        /// agent kind/root/state, workload partition -- has no such excuse and must match.
        fn matches_ignoring_session(&self, other: &Self) -> bool {
            self.role == other.role
                && self.agent_kind == other.agent_kind
                && self.agent_root == other.agent_root
                && self.agent_state == other.agent_state
                && self.workload_root == other.workload_root
        }
    }
    fn signature(s: &AttributionSnapshot, target: ProcessIdentity) -> Option<Signature> {
        let p = s.processes.iter().find(|p| p.identity == target)?;
        let agent = p
            .agent_id
            .as_ref()
            .and_then(|aid| s.agents.iter().find(|a| &a.id == aid));
        let workload = p
            .workload_id
            .as_ref()
            .and_then(|wid| s.workloads.iter().find(|w| &w.id == wid));
        Some(Signature {
            role: p.role,
            agent_kind: agent.map(|a| a.kind.clone()),
            agent_root: agent.and_then(|a| a.root),
            agent_session: agent.and_then(|a| a.session_id.clone()),
            agent_state: agent.map(|a| a.state),
            workload_root: workload.map(|w| w.root),
        })
    }
    /// Session, once established on an agent, must never change at a later tick -- checked
    /// against the incremental run's own history, independent of what a fresh run would pick.
    fn check_session_once_established_never_changes(
        snapshot: &AttributionSnapshot,
        established_sessions: &mut HashMap<String, String>,
    ) -> Result<(), TestCaseError> {
        for a in &snapshot.agents {
            let Some(session) = &a.session_id else {
                continue;
            };
            match established_sessions.get(&a.id) {
                Some(prior) => {
                    prop_assert_eq!(
                        prior,
                        session,
                        "agent {}: session must never change once established (was {:?}, now {:?})",
                        a.id,
                        prior,
                        session
                    );
                }
                None => {
                    established_sessions.insert(a.id.clone(), session.clone());
                }
            }
        }
        Ok(())
    }
    /// Every claim must have resolved past unknown validity once every read this attributor has
    /// seen has succeeded -- a stronger, internal-state check than the snapshot-level
    /// rootless-and-Unknown never-stuck guarantee, since it also covers a claim that never made
    /// it into any agent at all.
    fn check_no_unknown_candidate_claims(attributor: &Attributor) -> Result<(), TestCaseError> {
        for claim in attributor.claims.values() {
            prop_assert!(
                !claim.has_unknown_candidate(),
                "a claim still has an unknown-candidate validity after every read succeeded"
            );
        }
        Ok(())
    }

    /// Independent check, against the generator's own ground truth rather than anything the
    /// snapshot itself reports: with this generator's fixed-per-node binary kind, an established
    /// root may end only on a confirmed `Exited` identity, never while still `Live` in any
    /// substate (unreadable and omitted included -- neither is confirmed absence). Two
    /// complementary sources of "established", so a bug on the very first sighting is caught too
    /// and not just a later regression from a previously-observed role: (a) a stable, id-format
    /// ground truth (`a:{pid}:{start}`, the format a real rooted agent always uses) checked
    /// directly against this tick's own observable binary evidence, independent of any prior
    /// snapshot; and (b) `remembered_roots`, externally tracking each agent's last-seen
    /// `AgentRoot` node across ticks (threaded by the caller), since production clears
    /// `agent.root` to `None` once an agent ends and the snapshot alone can no longer name it.
    fn check_no_premature_ending(
        scenario: &Scenario,
        snapshot: &AttributionSnapshot,
        ever_shown_candidate: &[bool],
        remembered_roots: &mut HashMap<String, usize>,
    ) -> Result<(), TestCaseError> {
        for p in &snapshot.processes {
            if p.role != ProcessRole::AgentRoot {
                continue;
            }
            let Some(agent_id) = &p.agent_id else {
                continue;
            };
            if let Some(i) = (0..scenario.nodes.len()).find(|&i| node_id(i) == p.identity) {
                remembered_roots.insert(agent_id.clone(), i);
            }
        }
        for a in &snapshot.agents {
            if a.state != AgentState::Ended {
                continue;
            }
            #[allow(clippy::needless_range_loop)]
            for i in 0..scenario.nodes.len() {
                if true_candidate_kind(scenario.nodes[i].kind).is_none() || !ever_shown_candidate[i]
                {
                    continue;
                }
                if a.id != format!("a:{}:{}", node_pid(i), node_start(i)) {
                    continue;
                }
                prop_assert!(
                    !matches!(scenario.presence[i], Presence::Live { .. }),
                    "agent {} ended while node {} (observably a real established root by its \
                     own binary evidence, from this tick's own reads alone) is still Live -- an \
                     established root may end only on a confirmed exit",
                    a.id,
                    i
                );
            }
            if let Some(&root_index) = remembered_roots.get(&a.id) {
                prop_assert!(
                    !matches!(scenario.presence[root_index], Presence::Live { .. }),
                    "agent {} ended while its remembered root (node {}, pid {}) is still Live \
                     this tick -- an established root may end only on a confirmed exit, never \
                     merely unreadable or omitted",
                    a.id,
                    root_index,
                    node_pid(root_index)
                );
            }
        }
        Ok(())
    }

    const MUTATION_TICKS: u64 = 4;

    fn check_scenario(seed: u64, order_seed: u64) -> Result<(), TestCaseError> {
        let mut rng = Rng::new(seed);
        let nodes = build_nodes(&mut rng);
        let mut scenario = Scenario::new(nodes);

        let mut order_rng = Rng::new(order_seed);
        let mut incremental_a = new_attributor();
        let mut incremental_b = new_attributor();
        let base_now = Instant::now();
        let mut wall_ms = 0u64;
        let mut ever_shown_candidate = vec![false; scenario.nodes.len()];
        let mut ever_observed = vec![false; scenario.nodes.len()];
        let mut claimant_pending_target_evidence = vec![false; scenario.nodes.len()];
        let mut established_sessions_a = HashMap::new();
        let mut established_sessions_b = HashMap::new();
        let mut remembered_roots_a = HashMap::new();
        let mut remembered_roots_b = HashMap::new();

        // Several ticks: mutate, then run both an in-order and a shuffled-order incremental
        // attributor over the exact same evidence, checking invariants at every tick.
        for tick in 0..MUTATION_TICKS {
            scenario.mutate(&mut rng);
            update_ever_shown_candidate(&scenario, &mut ever_shown_candidate);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut claimant_pending_target_evidence,
            );
            let now = base_now + Duration::from_secs(tick);
            let (mut table_a, platform) = scenario.build_table_and_platform(false);
            let mut table_b = table_a.clone();
            shuffle(&mut table_b, &mut order_rng);

            let snap_a = incremental_a.update(&platform, &mut table_a, now, wall_ms, false);
            check_no_workload_under_an_uncertain_agent(&snap_a)?;
            check_workload_and_agent_ids_resolve_and_agree(&snap_a)?;
            // Dynamic protection needs last-observed PPIDs; fixed-topology recovery still checks it.
            check_session_once_established_never_changes(&snap_a, &mut established_sessions_a)?;
            check_no_premature_ending(
                &scenario,
                &snap_a,
                &ever_shown_candidate,
                &mut remembered_roots_a,
            )?;
            let snap_b = incremental_b.update(&platform, &mut table_b, now, wall_ms, false);
            check_no_workload_under_an_uncertain_agent(&snap_b)?;
            check_workload_and_agent_ids_resolve_and_agree(&snap_b)?;
            check_session_once_established_never_changes(&snap_b, &mut established_sessions_b)?;
            check_no_premature_ending(
                &scenario,
                &snap_b,
                &ever_shown_candidate,
                &mut remembered_roots_b,
            )?;
            assert_snapshots_equal_modulo_order(&snap_a, &snap_b, "mid-sequence tick")?;

            wall_ms += 1000;
        }

        // Convergence tick: every read succeeds, every gone identity is confirmed gone, and
        // topology stays exactly what the mutation ticks left it as (see
        // `build_table_and_platform`: a "recovery" tick never reverses a real reparent or exit).
        let now = base_now + Duration::from_secs(MUTATION_TICKS);
        let (mut table_a, platform) = scenario.build_convergence_table_and_platform();
        let mut table_b = table_a.clone();
        shuffle(&mut table_b, &mut order_rng);
        let converged_a = incremental_a.update(&platform, &mut table_a, now, wall_ms, false);
        check_no_workload_under_an_uncertain_agent(&converged_a)?;
        check_workload_and_agent_ids_resolve_and_agree(&converged_a)?;
        check_protected_processes_are_never_absorbed_as_workload(
            &scenario,
            &converged_a,
            &ever_shown_candidate,
            &ever_observed,
            &claimant_pending_target_evidence,
            true,
        )?;
        check_session_once_established_never_changes(&converged_a, &mut established_sessions_a)?;
        check_no_premature_ending(
            &scenario,
            &converged_a,
            &ever_shown_candidate,
            &mut remembered_roots_a,
        )?;
        let converged_b = incremental_b.update(&platform, &mut table_b, now, wall_ms, false);
        check_no_workload_under_an_uncertain_agent(&converged_b)?;
        check_workload_and_agent_ids_resolve_and_agree(&converged_b)?;
        check_session_once_established_never_changes(&converged_b, &mut established_sessions_b)?;
        check_no_premature_ending(
            &scenario,
            &converged_b,
            &ever_shown_candidate,
            &mut remembered_roots_b,
        )?;
        assert_snapshots_equal_modulo_order(&converged_a, &converged_b, "convergence tick")?;

        // Unconditional never-stuck guarantee: once every read (including liveness/pid
        // presence) succeeds, no agent may remain rootless-and-Unknown, and no claim may still
        // carry unknown-candidate validity.
        for a in &converged_a.agents {
            prop_assert!(
                !(a.root.is_none() && a.state == AgentState::Unknown),
                "agent {} is still rootless-and-Unknown after every read succeeded",
                a.id
            );
        }
        check_no_unknown_candidate_claims(&incremental_a)?;
        check_no_unknown_candidate_claims(&incremental_b)?;

        // Convergence-vs-fresh strict semantic equality is asserted by the fixed-topology
        // `check_recovery_scenario` variant instead, unconditionally and for every node, with no
        // inherited/broken-ancestry exemption. Arbitrary mutation histories here only need to
        // keep every unconditional invariant checked above at every tick.
        Ok(())
    }

    /// Fixed-topology recovery-only variant: the generated tree and every marker value are
    /// chosen once and never mutated afterward -- only readability flips, tick to tick, from
    /// unreadable back to readable and back again. Since topology never actually changes, no
    /// node's ancestry is ever broken, so unlike `check_scenario` there is no inherited-node
    /// exemption here: once every read succeeds, the incremental run must match a fresh
    /// single-shot run exactly, for every node, with no ratchet excusing a difference.
    fn check_recovery_scenario(seed: u64) -> Result<(), TestCaseError> {
        let mut rng = Rng::new(seed);
        let nodes = build_nodes(&mut rng);
        let len = nodes.len();
        let mut scenario = Scenario::new(nodes);
        let mut incremental = new_attributor();
        let base_now = Instant::now();
        let mut wall_ms = 0u64;
        let mut ever_shown_candidate = vec![false; len];
        let mut ever_observed = vec![false; len];
        let mut claimant_pending_target_evidence = vec![false; len];
        let mut established_sessions = HashMap::new();
        let mut remembered_roots = HashMap::new();

        for tick in 0..MUTATION_TICKS {
            scenario.mutate_reads_only(&mut rng);
            update_ever_shown_candidate(&scenario, &mut ever_shown_candidate);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut claimant_pending_target_evidence,
            );
            let now = base_now + Duration::from_secs(tick);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let snap = incremental.update(&platform, &mut table, now, wall_ms, false);
            check_no_workload_under_an_uncertain_agent(&snap)?;
            check_workload_and_agent_ids_resolve_and_agree(&snap)?;
            check_protected_processes_are_never_absorbed_as_workload(
                &scenario,
                &snap,
                &ever_shown_candidate,
                &ever_observed,
                &claimant_pending_target_evidence,
                false,
            )?;
            check_session_once_established_never_changes(&snap, &mut established_sessions)?;
            check_no_premature_ending(
                &scenario,
                &snap,
                &ever_shown_candidate,
                &mut remembered_roots,
            )?;
            wall_ms += 1000;
        }

        let now = base_now + Duration::from_secs(MUTATION_TICKS);
        let (mut table, platform) = scenario.build_convergence_table_and_platform();
        let converged = incremental.update(&platform, &mut table, now, wall_ms, false);
        check_no_workload_under_an_uncertain_agent(&converged)?;
        check_workload_and_agent_ids_resolve_and_agree(&converged)?;
        check_protected_processes_are_never_absorbed_as_workload(
            &scenario,
            &converged,
            &ever_shown_candidate,
            &ever_observed,
            &claimant_pending_target_evidence,
            true,
        )?;
        check_session_once_established_never_changes(&converged, &mut established_sessions)?;
        check_no_premature_ending(
            &scenario,
            &converged,
            &ever_shown_candidate,
            &mut remembered_roots,
        )?;
        check_no_unknown_candidate_claims(&incremental)?;

        let (mut fresh_table, fresh_platform) = scenario.build_convergence_table_and_platform();
        let mut fresh = new_attributor();
        let fresh_snapshot = fresh.update(&fresh_platform, &mut fresh_table, now, 0, false);
        check_workload_and_agent_ids_resolve_and_agree(&fresh_snapshot)?;

        for i in 0..len {
            let target = node_id(i);
            let inc = signature(&converged, target);
            let fr = signature(&fresh_snapshot, target);
            let matches = match (&inc, &fr) {
                (Some(inc), Some(fr)) => inc.matches_ignoring_session(fr),
                (None, None) => true,
                _ => false,
            };
            prop_assert!(
                matches,
                "node {} (pid {}): fixed-topology recovery-only run must converge exactly to \
                 fresh for every node once every read succeeds (ignoring session, which is \
                 retained once established rather than recomputed) (inc={:?} fr={:?})",
                i,
                node_pid(i),
                inc,
                fr
            );
        }
        check_membership_partitions_match(len, &converged, &fresh_snapshot)?;
        Ok(())
    }

    /// Per-node signature equality alone is blind to *which* other nodes share an id: two
    /// separately-invalid rootless agents merged into one, or one real workload split across two
    /// ids, can still leave every individual node's own kind/root/state/role/workload-root
    /// matching fresh. This compares the actual equivalence relations instead -- for every pair
    /// of live nodes, whether they share an agent and whether they share a workload must agree
    /// between the incremental and fresh runs, independent of the id strings involved.
    fn check_membership_partitions_match(
        len: usize,
        inc: &AttributionSnapshot,
        fresh: &AttributionSnapshot,
    ) -> Result<(), TestCaseError> {
        for i in 0..len {
            for j in (i + 1)..len {
                let (ti, tj) = (node_id(i), node_id(j));
                let (Some(inc_i), Some(inc_j), Some(fr_i), Some(fr_j)) = (
                    inc.processes.iter().find(|p| p.identity == ti),
                    inc.processes.iter().find(|p| p.identity == tj),
                    fresh.processes.iter().find(|p| p.identity == ti),
                    fresh.processes.iter().find(|p| p.identity == tj),
                ) else {
                    continue;
                };
                let inc_same_agent = inc_i.agent_id.is_some() && inc_i.agent_id == inc_j.agent_id;
                let fr_same_agent = fr_i.agent_id.is_some() && fr_i.agent_id == fr_j.agent_id;
                prop_assert_eq!(
                    inc_same_agent,
                    fr_same_agent,
                    "nodes {} and {}: same-agent membership must match fresh (inc={} fr={})",
                    i,
                    j,
                    inc_same_agent,
                    fr_same_agent
                );
                let inc_same_workload =
                    inc_i.workload_id.is_some() && inc_i.workload_id == inc_j.workload_id;
                let fr_same_workload =
                    fr_i.workload_id.is_some() && fr_i.workload_id == fr_j.workload_id;
                prop_assert_eq!(
                    inc_same_workload,
                    fr_same_workload,
                    "nodes {} and {}: same-workload membership must match fresh (inc={} fr={})",
                    i,
                    j,
                    inc_same_workload,
                    fr_same_workload
                );
            }
        }
        Ok(())
    }

    fn shuffle(table: &mut [Process], rng: &mut Rng) {
        let len = table.len();
        for i in (1..len).rev() {
            let j = rng.range((i + 1) as u64) as usize;
            table.swap(i, j);
        }
    }

    /// Named regressions for specific seeds the generator has found real production bugs with,
    /// kept explicit rather than relying solely on `proptest-regressions/attribution/tests.txt`
    /// (which only remembers the seed proptest happened to shrink to on its last run of the
    /// batch `proptest!` case, and is lost if that file is ever regenerated from scratch).
    mod recovery_regressions {
        use super::*;

        /// Fixup-4 seed ratchet: traced (scratch/t04-fixup-4/seed-trace.log) to a claim whose own
        /// kind is positively invalidated while its target is fully observable (a Claude-session
        /// marker pointing at a genuinely Codex binary), which must not resolve Valid again just
        /// because the target later goes omitted -- see the minimal, hand-built reproduction in
        /// `fixup4_regressions` above. Pinned red until production closes this gap; do not add an
        /// oracle exemption for it.
        #[test]
        fn cross_kind_claim_invalidated_while_observable_must_not_reattach_once_its_target_is_omitted_seed_16174314706627908250()
         {
            check_recovery_scenario(16174314706627908250).unwrap();
        }
        /// Found by the property run after the stale-carrier oracle fix above: the oracle's walk
        /// stopped at an omitted ancestor instead of using its retained ppid/env, so it missed a
        /// carrier that was only observable on an earlier tick. See
        /// scratch/t04-fixup-4/root-raw-seeds.log (coordinator's evidence log) for the raw dump.
        #[test]
        fn stale_carrier_evidence_survives_the_carriers_own_later_omission_seed_17894688641632127852()
         {
            check_recovery_scenario(17894688641632127852).unwrap();
        }
        /// Newly surfaced by a plain default-seed property run (unrelated to the PID-reuse-
        /// without-confirmed-Gone generator addition just above it in `mutate` -- this path only
        /// exercises `mutate_reads_only`, which never touches `gone`/`confirmed_delayed`, so it
        /// is structurally unreachable through that change). Fixed-topology recovery: the
        /// incremental run ends up with an agent still reporting a live root (state Unknown), the
        /// fresh single-shot run at the same converged table reports it Ended (root=None) for the
        /// same kind/session. Not yet root-caused; reported rather than guessed at further.
        #[test]
        fn incremental_and_fresh_disagree_on_agent_ended_state_seed_825268439977706024() {
            check_recovery_scenario(825268439977706024).unwrap();
        }
        #[test]
        fn agent_internal_role_upgrades_to_workload_once_a_tool_shells_exe_becomes_readable() {
            check_recovery_scenario(4450404270294731092).unwrap();
        }
        #[test]
        fn a_tool_shell_child_of_a_path_only_parent_does_not_leak_into_the_outer_roots_workload() {
            check_recovery_scenario(16569949278308012414).unwrap();
        }
        #[test]
        fn an_ancestor_is_never_swept_into_a_valid_descendants_agent_as_its_workload() {
            check_recovery_scenario(13085138417032130506).unwrap();
        }
        #[test]
        fn a_workload_role_is_not_stuck_once_the_parent_relationship_that_justified_it_ends() {
            check_recovery_scenario(16840071826477135464).unwrap();
        }
        #[test]
        fn a_node_omitted_for_several_ticks_recovers_to_fresh_once_it_reappears_in_the_table() {
            check_recovery_scenario(16373780471871086024).unwrap();
        }
        /// Minimal, hand-built reproduction of the same root cause pinned by seed
        /// `16373780471871086024` above: a plain child, provisionally attributed via its root's
        /// own ancestry marker, drops out of enumeration (`omitted`) while the root is still
        /// unreadable; the root then becomes readable (validates, and its identity flips from
        /// the marker's provisional session grouping to its own real `a:{pid}:{start}`) while
        /// the child is still omitted; the child later reappears with its exe/parent unchanged.
        /// Root validation must not permanently drop the still-omitted child's pending
        /// resolution -- once the child's own read succeeds again, it must be rediscovered under
        /// its root's agent, not left `Unattributed` forever.
        #[test]
        fn omitted_child_of_a_root_that_validates_mid_omission_is_rediscovered_once_it_reappears() {
            let nodes = vec![
                Node {
                    kind: Kind::Claude,
                    parent: None,
                    marker: MarkerChoice::CodexAncestry,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[0] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            let mut attributor = new_attributor();
            let (mut table, platform) = scenario.build_table_and_platform(false);
            attributor.update(&platform, &mut table, Instant::now(), 0, false);

            // The child drops out of enumeration while the root is still unreadable.
            scenario.presence[1] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: true,
            };
            let (mut table, platform) = scenario.build_table_and_platform(false);
            attributor.update(&platform, &mut table, Instant::now(), 1000, false);

            // The root becomes readable (validates) while the child is still omitted.
            scenario.presence[0] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: false,
            };
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let snap = attributor.update(&platform, &mut table, Instant::now(), 2000, false);
            assert_eq!(
                snap.processes
                    .iter()
                    .find(|p| p.identity == node_id(0))
                    .unwrap()
                    .role,
                ProcessRole::AgentRoot
            );

            // One more tick with the child still omitted, exactly matching the seed's sequence.
            let (mut table, platform) = scenario.build_table_and_platform(false);
            attributor.update(&platform, &mut table, Instant::now(), 3000, false);

            // The child reappears, fully readable, exe/parent unchanged from before.
            scenario.presence[1] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: false,
            };
            let (mut table, platform) = scenario.build_convergence_table_and_platform();
            let snap = attributor.update(&platform, &mut table, Instant::now(), 4000, false);
            let p = snap
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap();
            assert_eq!(
                p.role,
                ProcessRole::AgentInternal,
                "a child that reappears after being omitted through its root's own validation \
                 must be rediscovered under its established root's agent, not left Unattributed"
            );
            assert_eq!(
                p.agent_id.as_deref(),
                Some(format!("a:{}:{}", node_pid(0), node_start(0)).as_str())
            );
        }
        #[test]
        fn a_tool_shell_recognized_while_its_root_is_omitted_upgrades_once_the_root_returns() {
            check_recovery_scenario(16373780471871086038).unwrap();
        }
        /// The shared-`lineage_root`/dirty-propagation-by-observed-PPID fix: a claim's root
        /// candidate transitions Invalid -> Unknown at omission and the resolve that should
        /// follow its child stops at a missing parent, leaving the child's stale Workload
        /// retained under a provisional agent that then gets pruned as unused.
        #[test]
        fn a_child_of_a_root_whose_own_resolve_stops_at_a_missing_parent_still_recovers() {
            check_recovery_scenario(13697448676912192970).unwrap();
        }
        #[test]
        fn a_second_seed_of_the_same_missing_parent_dirty_propagation_class_recovers() {
            check_recovery_scenario(3618724402699227764).unwrap();
        }
        /// The general reappearance-invalidation fix: a claimant's own exe/parent/cache stay
        /// unchanged while it is omitted, but an unrelated ancestor validates during that same
        /// omission; the dirty predicate that would normally re-resolve the claimant never fires
        /// once it returns with unchanged cached exe/parent, so its resolve is silently skipped.
        #[test]
        fn a_claimant_omitted_while_an_unrelated_ancestor_validates_still_recovers() {
            check_recovery_scenario(6280910674545751510).unwrap();
        }
        /// The session-membership fix: a late-discovered explicit claim naming an already
        /// -established root, under a different session token than the root's own, used to be
        /// rejected as a conflicting session and spawn a separate, ultimately orphaned rootless
        /// agent instead of simply joining the existing root's agent as Workload.
        #[test]
        fn a_late_explicit_claim_with_a_different_session_on_an_established_root_recovers() {
            check_recovery_scenario(17093178278508533976).unwrap();
        }
        /// Minimal, hand-built reproduction of the same root cause: a Codex root establishes its
        /// own ancestry-derived session first (tick 0); only later does a separate process's
        /// explicit marker name that same root pid, under its own different session token (tick
        /// 1). The late claimant must join the SAME already-established root agent as Workload --
        /// root membership retained, the root's own already-established session left unchanged --
        /// regardless of which order the two rows happen to appear in the enumerated table.
        #[test]
        fn a_late_explicit_different_session_claim_on_an_established_root_keeps_root_membership() {
            struct Outcome {
                root_agent_id: String,
                root_session: Option<String>,
                root_identity: Option<ProcessIdentity>,
                claimant_role: ProcessRole,
                claimant_agent_id: Option<String>,
            }
            fn run(swap_order: bool) -> Outcome {
                let nodes = vec![
                    Node {
                        kind: Kind::Codex,
                        parent: None,
                        marker: MarkerChoice::CodexAncestry,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: None,
                        marker: MarkerChoice::CodexExplicit,
                        explicit_target: Some(0),
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ];
                let mut scenario = Scenario::new(nodes);
                scenario.presence[1] = Presence::Live {
                    unreadable: false,
                    detached: false,
                    omitted: true,
                };
                let mut attributor = new_attributor();
                let (mut table, platform) = scenario.build_table_and_platform(false);
                let snap0 = attributor.update(&platform, &mut table, Instant::now(), 0, false);
                let root_agent_id = format!("a:{}:{}", node_pid(0), node_start(0));
                let established = snap0.agents.iter().find(|a| a.id == root_agent_id).unwrap();
                assert_eq!(
                    established.session_id.as_deref(),
                    Some("thread-0"),
                    "the root must already have its own ancestry-derived session established \
                     before the late claimant appears, for this probe to mean anything"
                );

                scenario.presence[1] = Presence::Live {
                    unreadable: false,
                    detached: false,
                    omitted: false,
                };
                let (mut table, platform) = scenario.build_table_and_platform(false);
                if swap_order {
                    table.swap(0, 1);
                }
                let snap1 = attributor.update(
                    &platform,
                    &mut table,
                    Instant::now() + Duration::from_secs(1),
                    1000,
                    false,
                );
                let root_agent = snap1.agents.iter().find(|a| a.id == root_agent_id).unwrap();
                let claimant = snap1
                    .processes
                    .iter()
                    .find(|p| p.identity == node_id(1))
                    .unwrap();
                Outcome {
                    root_agent_id,
                    root_session: root_agent.session_id.clone(),
                    root_identity: root_agent.root,
                    claimant_role: claimant.role,
                    claimant_agent_id: claimant.agent_id.clone(),
                }
            }
            for swap_order in [false, true] {
                let outcome = run(swap_order);
                assert_eq!(
                    outcome.root_session.as_deref(),
                    Some("thread-0"),
                    "swap_order={swap_order}: the root's own already-established session must \
                     not change just because a later, different-session explicit claim also \
                     names it"
                );
                assert_eq!(
                    outcome.root_identity,
                    Some(node_id(0)),
                    "swap_order={swap_order}: the root agent must keep its own root identity"
                );
                assert_eq!(
                    outcome.claimant_role,
                    ProcessRole::Workload,
                    "swap_order={swap_order}: the late explicit claimant must resolve to \
                     Workload of the established root, not stay pending"
                );
                assert_eq!(
                    outcome.claimant_agent_id.as_deref(),
                    Some(outcome.root_agent_id.as_str()),
                    "swap_order={swap_order}: the late claimant must join the SAME established \
                     root agent, not spawn a separate rootless one"
                );
            }
        }
        /// Minimal, hand-built reproduction of the same root cause pinned by seed
        /// `16373780471871086038` above: a tool-shell's own exe/argv become readable (confirming
        /// it really is a tool shell) at the exact tick its established root is omitted from
        /// enumeration. The root was already positively validated the tick before and retains
        /// `Valid` through the omission (the approved retained-root rule), so the child's own
        /// observed PPID plus its own now-confirmed `sh -c` boundary are already positive
        /// evidence under that same retained root -- it must resolve to `Workload` immediately,
        /// not wait for the root's row to reappear. Once the root reappears, it must stay
        /// `Workload` (not regress or get recreated under a different workload).
        #[test]
        fn tool_shell_confirmed_while_root_omitted_upgrades_to_workload_once_root_reappears() {
            let nodes = vec![
                Node {
                    kind: Kind::Cursor,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[1] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            let mut attributor = new_attributor();
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let snap0 = attributor.update(&platform, &mut table, Instant::now(), 0, false);
            assert_eq!(
                snap0
                    .processes
                    .iter()
                    .find(|p| p.identity == node_id(0))
                    .unwrap()
                    .role,
                ProcessRole::AgentRoot
            );
            assert_eq!(
                snap0
                    .processes
                    .iter()
                    .find(|p| p.identity == node_id(1))
                    .unwrap()
                    .role,
                ProcessRole::AgentInternal
            );

            // The child's own exe/argv become readable (confirming it as a real tool shell) at
            // the exact tick its root drops out of enumeration.
            scenario.presence[0] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: true,
            };
            scenario.presence[1] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: false,
            };
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let snap1 = attributor.update(&platform, &mut table, Instant::now(), 1000, false);
            let agent0 = snap1
                .agents
                .iter()
                .find(|a| a.id == format!("a:{}:{}", node_pid(0), node_start(0)))
                .unwrap();
            assert!(
                agent0.root.is_some(),
                "the root's own identity must remain positively validated through its omission, \
                 not reset to pending"
            );
            let p1 = snap1
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap();
            assert_eq!(
                p1.role,
                ProcessRole::Workload,
                "the child's own confirmed direct-root tool-shell boundary is already positive \
                 evidence under the root's retained Valid identity, even while the root's own \
                 row is this tick's omitted one -- it must resolve to Workload immediately, not \
                 wait for the root to reappear"
            );
            assert_eq!(p1.agent_id.as_deref(), Some(agent0.id.as_str()));

            // The root reappears; the child stays readable, exe/parent unchanged.
            scenario.presence[0] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: false,
            };
            let (mut table, platform) = scenario.build_convergence_table_and_platform();
            let snap2 = attributor.update(&platform, &mut table, Instant::now(), 2000, false);
            let p = snap2
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap();
            assert_eq!(
                p.role,
                ProcessRole::Workload,
                "once the root reappears and the child's own direct-root tool-shell boundary is \
                 confirmed, it must upgrade to Workload, not stay stuck at the AgentInternal \
                 classification retained from the tick the root was absent"
            );
            assert_eq!(
                p.agent_id.as_deref(),
                Some(format!("a:{}:{}", node_pid(0), node_start(0)).as_str())
            );
        }
        /// Found immediately after adding the foreign-uid generator dimension: an explicit
        /// claim's target with a foreign uid was treated as merely pending (same as an
        /// unreadable/omitted target) instead of positively invalid the moment its row -- and so
        /// its uid -- was observed, matching the already-established contract in
        /// `a_claude_pid_marker_pointing_at_a_foreign_uid_process_resolves_a_rootless_ended_agent_not_the_foreign_root`.
        /// Both `compute_protected_nodes` and `update_claimant_pending_target_evidence` were
        /// missing this check.
        #[test]
        fn an_explicit_claims_foreign_uid_target_is_positively_invalid_once_observed_not_merely_pending_seed_2946276931638059726()
         {
            check_recovery_scenario(2946276931638059726).unwrap();
        }
    }

    /// Fresh reviewer's four probes (see the fixup-3 property-review artifact), converted from
    /// "the old helpers accept this bad output" demonstrations into permanent regressions that
    /// assert the new oracles reject it.
    mod oracle_regressions {
        use super::*;

        #[test]
        fn protected_oracle_rejects_explicit_candidate_and_descendant_reassigned_to_outer_workload()
        {
            let nodes = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(2),
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(2),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[2] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            let mut ever = vec![false; 4];
            let mut ever_observed = vec![false; 4];
            let mut ever_referenced = vec![false; 4];
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snapshot =
                new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            let outer = snapshot
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap()
                .clone();
            assert_eq!(outer.role, ProcessRole::Workload);
            for i in [2, 3] {
                let p = snapshot
                    .processes
                    .iter_mut()
                    .find(|p| p.identity == node_id(i))
                    .unwrap();
                assert_eq!(p.role, ProcessRole::AgentInternal);
                p.role = ProcessRole::Workload;
                p.agent_id = outer.agent_id.clone();
                p.workload_id = outer.workload_id.clone();
            }
            assert!(
                check_protected_processes_are_never_absorbed_as_workload(
                    &scenario,
                    &snapshot,
                    &ever,
                    &ever_observed,
                    &ever_referenced,
                    false,
                )
                .is_err(),
                "the independent oracle must reject a protected explicit-marker candidate and \
                 its claimant reassigned to the outer workload"
            );
        }

        #[test]
        fn premature_ending_check_rejects_ended_agent_with_live_readable_root() {
            let nodes = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let scenario = Scenario::new(nodes);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snapshot =
                new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            assert!(snapshot.agents[0].root.is_some());
            assert_ne!(snapshot.agents[0].state, AgentState::Ended);
            let mut ever = vec![false; 3];
            update_ever_shown_candidate(&scenario, &mut ever);
            let mut remembered_roots = HashMap::new();
            check_no_premature_ending(&scenario, &snapshot, &ever, &mut remembered_roots).unwrap();
            snapshot.agents[0].state = AgentState::Ended;
            snapshot.agents[0].root = None;
            snapshot.agents[0].ended_at_ms = Some(0);
            assert!(
                check_no_premature_ending(&scenario, &snapshot, &ever, &mut remembered_roots)
                    .is_err(),
                "the premature-ending oracle must reject an agent ending while its remembered \
                 root is still live and reading back as a real matching binary"
            );
        }

        #[test]
        fn premature_ending_check_rejects_ended_agent_with_omitted_root() {
            let nodes = vec![Node {
                kind: Kind::Codex,
                parent: None,
                marker: MarkerChoice::None,
                explicit_target: None,
                foreign_uid: false,
                env_permanently_unknown: false,
            }];
            let mut scenario = Scenario::new(nodes);
            let mut attributor = new_attributor();
            let mut ever = vec![false; 1];
            let mut remembered_roots = HashMap::new();
            update_ever_shown_candidate(&scenario, &mut ever);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let snap0 = attributor.update(&platform, &mut table, Instant::now(), 0, false);
            check_no_premature_ending(&scenario, &snap0, &ever, &mut remembered_roots).unwrap();
            assert_eq!(snap0.agents[0].root, Some(node_id(0)));

            scenario.presence[0] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: true,
            };
            update_ever_shown_candidate(&scenario, &mut ever);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snap1 = attributor.update(&platform, &mut table, Instant::now(), 1000, false);
            assert_ne!(
                snap1.agents[0].state,
                AgentState::Ended,
                "production correctly keeps the agent alive through a mere enumeration gap"
            );
            snap1.agents[0].state = AgentState::Ended;
            snap1.agents[0].root = None;
            snap1.agents[0].ended_at_ms = Some(0);
            assert!(
                check_no_premature_ending(&scenario, &snap1, &ever, &mut remembered_roots).is_err(),
                "the premature-ending oracle must reject an agent ending while its established \
                 root is merely omitted from this tick's enumeration, not confirmed exited"
            );
        }

        #[test]
        fn premature_ending_check_rejects_ended_agent_with_unreadable_root() {
            let nodes = vec![Node {
                kind: Kind::Claude,
                parent: None,
                marker: MarkerChoice::None,
                explicit_target: None,
                foreign_uid: false,
                env_permanently_unknown: false,
            }];
            let mut scenario = Scenario::new(nodes);
            let mut attributor = new_attributor();
            let mut ever = vec![false; 1];
            let mut remembered_roots = HashMap::new();
            update_ever_shown_candidate(&scenario, &mut ever);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let snap0 = attributor.update(&platform, &mut table, Instant::now(), 0, false);
            check_no_premature_ending(&scenario, &snap0, &ever, &mut remembered_roots).unwrap();
            assert_eq!(snap0.agents[0].root, Some(node_id(0)));

            scenario.presence[0] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            update_ever_shown_candidate(&scenario, &mut ever);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snap1 = attributor.update(&platform, &mut table, Instant::now(), 1000, false);
            assert_ne!(
                snap1.agents[0].state,
                AgentState::Ended,
                "production correctly keeps the agent alive through a transient read failure"
            );
            snap1.agents[0].state = AgentState::Ended;
            snap1.agents[0].root = None;
            snap1.agents[0].ended_at_ms = Some(0);
            assert!(
                check_no_premature_ending(&scenario, &snap1, &ever, &mut remembered_roots).is_err(),
                "the premature-ending oracle must reject an agent ending while its established \
                 root's exe/argv read merely fails this tick, not confirmed exited"
            );
        }

        #[test]
        fn premature_ending_check_rejects_first_sight_ended_root_without_prior_history() {
            let nodes = vec![Node {
                kind: Kind::Claude,
                parent: None,
                marker: MarkerChoice::None,
                explicit_target: None,
                foreign_uid: false,
                env_permanently_unknown: false,
            }];
            let scenario = Scenario::new(nodes);
            let mut ever = vec![false; 1];
            update_ever_shown_candidate(&scenario, &mut ever);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snapshot =
                new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            snapshot.agents[0].state = AgentState::Ended;
            snapshot.agents[0].root = None;
            snapshot.agents[0].ended_at_ms = Some(0);
            // Deliberately empty: nothing has ever been remembered from a prior call, so only
            // this tick's own id-format ground-truth check can catch it.
            let mut remembered_roots = HashMap::new();
            assert!(
                check_no_premature_ending(&scenario, &snapshot, &ever, &mut remembered_roots)
                    .is_err(),
                "must reject purely from this tick's own observable binary evidence, without \
                 relying on a previously-remembered AgentRoot role"
            );
        }

        #[test]
        fn membership_partition_check_rejects_merged_rootless_agents() {
            let nodes = vec![
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(0),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(1),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let scenario = Scenario::new(nodes);
            let (mut table, platform) = scenario.build_convergence_table_and_platform();
            let fresh = new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            let first = fresh
                .processes
                .iter()
                .find(|p| p.identity == node_id(0))
                .unwrap()
                .agent_id
                .clone()
                .unwrap();
            let second = fresh
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap()
                .agent_id
                .clone()
                .unwrap();
            assert_ne!(first, second);
            let mut merged = fresh.clone();
            for p in &mut merged.processes {
                if p.agent_id.as_ref() == Some(&second) {
                    p.agent_id = Some(first.clone());
                }
            }
            for w in &mut merged.workloads {
                if w.agent_id == second {
                    w.agent_id = first.clone();
                }
            }
            merged.agents.retain(|a| a.id != second);
            assert!(
                check_membership_partitions_match(3, &merged, &fresh).is_err(),
                "the partition check must reject two independently-invalid rootless agents \
                 merged into one"
            );
        }

        #[test]
        fn membership_partition_check_rejects_split_workload() {
            let nodes = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let scenario = Scenario::new(nodes);
            let (mut table, platform) = scenario.build_convergence_table_and_platform();
            let fresh = new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            assert_eq!(fresh.workloads.len(), 1);
            let mut split = fresh.clone();
            let mut duplicated = split.workloads[0].clone();
            duplicated.id.push_str(":incorrectly-split");
            split
                .processes
                .iter_mut()
                .find(|p| p.identity == node_id(3))
                .unwrap()
                .workload_id = Some(duplicated.id.clone());
            split.workloads.push(duplicated);
            assert!(
                check_membership_partitions_match(4, &split, &fresh).is_err(),
                "the partition check must reject a real shared workload split across two ids"
            );
        }

        /// Shared shape for the focused-review marker-protection probes below: a validated Codex
        /// root (0) with a real ToolShell workload (1), plus caller-supplied nodes hanging off
        /// it. Builds and runs one tick, asserts each `victim` was actually left `AgentInternal`
        /// by real production, then reassigns every victim to the outer shell's own Workload
        /// (consistent agent/workload references) and asserts
        /// `check_protected_processes_are_never_absorbed_as_workload` rejects that reassignment.
        fn assert_protected_oracle_rejects_reassignment_to_outer_workload(
            mut nodes: Vec<Node>,
            presence_overrides: &[(usize, Presence)],
            victims: &[usize],
        ) {
            let n = nodes.len();
            let outer = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            for node in &mut nodes {
                if let Some(p) = &mut node.parent {
                    *p += 2;
                }
                if let Some(t) = &mut node.explicit_target {
                    *t += 2;
                }
            }
            let mut all = outer;
            all.extend(nodes);
            let mut scenario = Scenario::new(all);
            for &(i, presence) in presence_overrides {
                scenario.presence[i + 2] = presence;
            }
            let mut ever = vec![false; n + 2];
            let mut ever_observed = vec![false; n + 2];
            let mut ever_referenced = vec![false; n + 2];
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snapshot =
                new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            let outer_workload = snapshot
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap()
                .clone();
            assert_eq!(outer_workload.role, ProcessRole::Workload);
            for &v in victims {
                let p = snapshot
                    .processes
                    .iter_mut()
                    .find(|p| p.identity == node_id(v + 2))
                    .unwrap();
                assert_eq!(
                    p.role,
                    ProcessRole::AgentInternal,
                    "production must actually protect victim node {v} for this probe to mean \
                     anything"
                );
                p.role = ProcessRole::Workload;
                p.agent_id = outer_workload.agent_id.clone();
                p.workload_id = outer_workload.workload_id.clone();
            }
            assert!(
                check_protected_processes_are_never_absorbed_as_workload(
                    &scenario,
                    &snapshot,
                    &ever,
                    &ever_observed,
                    &ever_referenced,
                    false,
                )
                .is_err(),
                "the independent oracle must reject victims {victims:?} reassigned to the outer \
                 workload"
            );
        }

        #[test]
        fn protected_oracle_rejects_self_referencing_explicit_claim_with_own_read_failure() {
            assert_protected_oracle_rejects_reassignment_to_outer_workload(
                vec![
                    Node {
                        kind: Kind::Plain,
                        parent: Some(1),
                        marker: MarkerChoice::CodexExplicit,
                        explicit_target: Some(0),
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(0),
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ],
                &[(
                    0,
                    Presence::Live {
                        unreadable: true,
                        detached: false,
                        omitted: false,
                    },
                )],
                &[0, 1],
            );
        }

        #[test]
        fn protected_oracle_rejects_ancestry_claimant_with_own_unreadable_executable() {
            assert_protected_oracle_rejects_reassignment_to_outer_workload(
                vec![
                    Node {
                        kind: Kind::Plain,
                        parent: None,
                        marker: MarkerChoice::CodexAncestry,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(0),
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ],
                &[(
                    0,
                    Presence::Live {
                        unreadable: true,
                        detached: false,
                        omitted: false,
                    },
                )],
                &[0, 1],
            );
        }

        #[test]
        fn protected_oracle_rejects_ancestry_claimants_own_descendant() {
            assert_protected_oracle_rejects_reassignment_to_outer_workload(
                vec![
                    Node {
                        kind: Kind::Plain,
                        parent: None,
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(0),
                        marker: MarkerChoice::CodexAncestry,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(1),
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ],
                &[(
                    0,
                    Presence::Live {
                        unreadable: true,
                        detached: false,
                        omitted: false,
                    },
                )],
                &[2],
            );
        }

        #[test]
        fn protected_oracle_rejects_claimant_of_a_never_observed_newer_target() {
            assert_protected_oracle_rejects_reassignment_to_outer_workload(
                vec![
                    Node {
                        kind: Kind::Plain,
                        parent: None,
                        marker: MarkerChoice::CodexExplicit,
                        explicit_target: Some(1),
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: None,
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ],
                &[(
                    1,
                    Presence::Live {
                        unreadable: false,
                        detached: false,
                        omitted: true,
                    },
                )],
                &[0],
            );
        }

        #[test]
        fn protected_oracle_rejects_ancestry_claimant_whose_immediate_parent_is_omitted() {
            assert_protected_oracle_rejects_reassignment_to_outer_workload(
                vec![
                    Node {
                        kind: Kind::Plain,
                        parent: None,
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(0),
                        marker: MarkerChoice::CodexAncestry,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(1),
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ],
                &[(
                    0,
                    Presence::Live {
                        unreadable: false,
                        detached: false,
                        omitted: true,
                    },
                )],
                &[1, 2],
            );
        }

        /// Finding 3 of the fixup-3 property re-review: the propagation loop must never derive a
        /// leaf's protection by reading a never-observed intermediate node's own (generator-only,
        /// hidden) parent field. Two scenarios that produce byte-identical *observed* rows -- the
        /// never-observed intermediate's own historical parent is the only difference, and it
        /// never appears in any table this tick -- must therefore compute identical protection
        /// for the visible leaf whose own observed PPID names that intermediate.
        #[test]
        fn protected_oracle_never_derives_a_leafs_protection_from_an_unobserved_intermediates_hidden_parent_link()
         {
            fn leaf_protection(intermediate_parent: Option<usize>) -> bool {
                let nodes = vec![
                    Node {
                        kind: Kind::VersionedClaudeMissingArgv,
                        parent: None,
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: intermediate_parent,
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                    Node {
                        kind: Kind::Plain,
                        parent: Some(1),
                        marker: MarkerChoice::None,
                        explicit_target: None,
                        foreign_uid: false,
                        env_permanently_unknown: false,
                    },
                ];
                let mut scenario = Scenario::new(nodes);
                scenario.presence[1] = Presence::Live {
                    unreadable: false,
                    detached: false,
                    omitted: true,
                };
                let mut ever = vec![false; 3];
                let mut ever_observed = vec![false; 3];
                let mut ever_referenced = vec![false; 3];
                update_ever_shown_candidate(&scenario, &mut ever);
                update_ever_observed(&scenario, &mut ever_observed);
                update_claimant_pending_target_evidence(
                    &scenario,
                    &ever_observed,
                    &mut ever_referenced,
                );
                let (protected, _, _) =
                    compute_protected_nodes(&scenario, &ever, &ever_observed, &ever_referenced);
                protected[2]
            }
            assert!(
                !leaf_protection(Some(0)),
                "a leaf reachable only through a never-observed intermediate must not be \
                 protected merely because the generator's hidden ground truth happens to root \
                 that intermediate at a pending candidate"
            );
            assert_eq!(
                leaf_protection(Some(0)),
                leaf_protection(None),
                "changing only the never-observed intermediate's own hidden parent, with every \
                 observed row unchanged, must not change whether the oracle protects the leaf"
            );
        }

        /// A second re-review finding: an ancestry claimant that was previously observed (its
        /// marker already read and cached) but is *now* omitted must still protect its own
        /// descendants, even though the claimant's own row is absent this tick. The old entry
        /// gate required the claimant to be currently observed before even checking its own
        /// candidacy, so it skipped protecting it (and its child) the moment it went omitted.
        #[test]
        fn protected_oracle_rejects_workload_under_a_previously_observed_now_omitted_ancestry_claimant()
         {
            let nodes = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::CodexAncestry,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(2),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            let mut ever = vec![false; 4];
            let mut ever_observed = vec![false; 4];
            let mut ever_referenced = vec![false; 4];
            let mut attributor = new_attributor();
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let first = attributor.update(&platform, &mut table, Instant::now(), 0, false);
            assert_eq!(
                attr(&first, node_id(3)).role,
                ProcessRole::Workload,
                "production must actually start the claimant's subtree as Workload while its \
                 own ancestry resolves normally, for this probe to mean anything"
            );

            // The claimant goes omitted; it was already observed once, so its marker is cached,
            // but its own row is absent this tick.
            scenario.presence[2] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: true,
            };
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            assert!(ever_observed[2]);
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut second = attributor.update(
                &platform,
                &mut table,
                Instant::now() + Duration::from_secs(1),
                1000,
                false,
            );
            let outer = attr(&second, node_id(1)).clone();
            let victim = second
                .processes
                .iter_mut()
                .find(|p| p.identity == node_id(3))
                .unwrap();
            assert_eq!(
                victim.role,
                ProcessRole::AgentInternal,
                "production must actually protect the victim once its claimant goes omitted, \
                 for this probe to mean anything"
            );
            victim.role = ProcessRole::Workload;
            victim.agent_id = outer.agent_id.clone();
            victim.workload_id = outer.workload_id.clone();
            assert!(
                check_protected_processes_are_never_absorbed_as_workload(
                    &scenario,
                    &second,
                    &ever,
                    &ever_observed,
                    &ever_referenced,
                    false,
                )
                .is_err(),
                "the independent oracle must reject the victim reassigned to the outer workload \
                 once its own ancestry claimant, though previously observed, is now omitted"
            );
        }

        /// Second final-review finding: the ancestry search must protect *every* unresolved
        /// candidate on a fully observed PPID path up to the nearest validated root, not just the
        /// first one it meets. Chain: validated Codex root -> tool shell -> unreadable Plain A ->
        /// unreadable Plain B -> readable ancestry claimant. Every row is present; only A and B's
        /// own exe reads fail.
        #[test]
        fn protected_oracle_rejects_a_farther_unresolved_candidate_on_a_fully_observed_ancestry_path()
         {
            let nodes = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(2),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(3),
                    marker: MarkerChoice::CodexAncestry,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            for i in [2, 3] {
                scenario.presence[i] = Presence::Live {
                    unreadable: true,
                    detached: false,
                    omitted: false,
                };
            }
            let mut ever = vec![false; 5];
            let mut ever_observed = vec![false; 5];
            let mut ever_referenced = vec![false; 5];
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut snapshot =
                new_attributor().update(&platform, &mut table, Instant::now(), 0, false);
            let outer = snapshot
                .processes
                .iter()
                .find(|p| p.identity == node_id(1))
                .unwrap()
                .clone();
            assert_eq!(outer.role, ProcessRole::Workload);
            let victim = snapshot
                .processes
                .iter_mut()
                .find(|p| p.identity == node_id(2))
                .unwrap();
            assert_eq!(
                victim.role,
                ProcessRole::AgentInternal,
                "production must actually protect the farther unresolved candidate for this \
                 probe to mean anything"
            );
            victim.role = ProcessRole::Workload;
            victim.agent_id = outer.agent_id.clone();
            victim.workload_id = outer.workload_id.clone();
            assert!(
                check_protected_processes_are_never_absorbed_as_workload(
                    &scenario,
                    &snapshot,
                    &ever,
                    &ever_observed,
                    &ever_referenced,
                    false,
                )
                .is_err(),
                "the independent oracle must reject the farther unresolved candidate on a fully \
                 observed ancestry path reassigned to the outer workload"
            );
        }

        /// Third final-review finding: an explicit target's pending evidence, once observed,
        /// must outlive the claimant that produced it. An unreadable Plain target lies below the
        /// outer Codex tool shell with two Plain children: one names the target via an observed
        /// explicit marker, the other carries no marker. After the claimant exits, the target
        /// stays live and unreadable and its unmarked sibling stays live -- both must still be
        /// rejected if reassigned to the outer workload, matching `Claim::retained`.
        #[test]
        fn protected_oracle_rejects_retained_explicit_target_evidence_after_claimant_exit() {
            let nodes = vec![
                Node {
                    kind: Kind::Codex,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::ToolShell,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(2),
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(2),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(2),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[2] = Presence::Live {
                unreadable: true,
                omitted: false,
                detached: false,
            };
            let mut ever = vec![false; 5];
            let mut ever_observed = vec![false; 5];
            let mut ever_referenced = vec![false; 5];
            let mut attributor = new_attributor();
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let first = attributor.update(&platform, &mut table, Instant::now(), 0, false);
            assert_eq!(
                attr(&first, node_id(4)).role,
                ProcessRole::AgentInternal,
                "production must actually protect the unmarked sibling while the claimant is \
                 still alive, for this probe to mean anything"
            );

            scenario.presence[3] = Presence::Exited;
            scenario.gone.push(node_id(3));
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(
                &scenario,
                &ever_observed,
                &mut ever_referenced,
            );
            let (mut table, platform) = scenario.build_table_and_platform(false);
            let mut second = attributor.update(
                &platform,
                &mut table,
                Instant::now() + Duration::from_secs(1),
                1000,
                false,
            );
            let outer = attr(&second, node_id(1)).clone();
            for i in [2, 4] {
                let victim = second
                    .processes
                    .iter_mut()
                    .find(|p| p.identity == node_id(i))
                    .unwrap();
                assert_eq!(
                    victim.role,
                    ProcessRole::AgentInternal,
                    "production must actually protect victim {i} once the claimant has exited, \
                     for this probe to mean anything"
                );
                victim.role = ProcessRole::Workload;
                victim.agent_id = outer.agent_id.clone();
                victim.workload_id = outer.workload_id.clone();
            }
            assert!(
                check_protected_processes_are_never_absorbed_as_workload(
                    &scenario,
                    &second,
                    &ever,
                    &ever_observed,
                    &ever_referenced,
                    false,
                )
                .is_err(),
                "the independent oracle must reject the target and its surviving sibling \
                 reassigned to the outer workload once the claimant that named the target has \
                 exited"
            );
        }

        /// Traced from seed 7209977609258981192: once a target has been positively read with an
        /// unambiguous kind that does not match the claim -- proving the claim was never valid --
        /// retained pending-target evidence must clear immediately and must not be resurrected by
        /// the same target later going unreadable again with no new information.
        #[test]
        fn protected_oracle_leaves_a_positively_invalidated_target_unprotected_after_claimant_exit()
        {
            let nodes = vec![
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Claude,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(0),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[0] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            let mut ever = vec![false; 2];
            let mut ever_observed = vec![false; 2];
            let mut pending = vec![false; 2];
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(&scenario, &ever_observed, &mut pending);
            let (protected, ..) =
                compute_protected_nodes(&scenario, &ever, &ever_observed, &pending);
            assert!(
                protected[0],
                "target must be protected while unreadable and the claimant is live, for this \
                 probe to mean anything"
            );

            // Target becomes readable, confirmed Plain (does not match the "codex" claim);
            // claimant exits the same tick.
            scenario.presence[0] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: false,
            };
            scenario.presence[1] = Presence::Exited;
            scenario.gone.push(node_id(1));
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(&scenario, &ever_observed, &mut pending);
            assert!(
                !pending[1],
                "a positively read, non-matching target must clear the claimant's evidence \
                 immediately"
            );

            // Target goes unreadable again; nothing new has been observed about it since the
            // positive read above.
            scenario.presence[0] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(&scenario, &ever_observed, &mut pending);
            let (protected, never_workload, _) =
                compute_protected_nodes(&scenario, &ever, &ever_observed, &pending);
            assert!(
                !protected[0] && !never_workload[0],
                "expired evidence must not be resurrected merely because the target goes \
                 unreadable again"
            );
        }

        /// Distinct claimants sharing the same target must never be collapsed into one slot: an
        /// earlier-born claimant whose claim is positively disproven by the target's observed
        /// start time must not expire a later-born claimant's still-valid evidence about the same
        /// target.
        #[test]
        fn protected_oracle_keeps_a_later_claimants_evidence_once_an_earlier_claimants_is_invalidated_by_age()
         {
            let nodes = vec![
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(1),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(1),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[1] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            let mut ever = vec![false; 3];
            let mut ever_observed = vec![false; 3];
            let mut pending = vec![false; 3];
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(&scenario, &ever_observed, &mut pending);
            assert!(
                !pending[0],
                "claimant 0 was observably born before the target, so the target's own observed \
                 start positively disproves this claim"
            );
            assert!(
                pending[2],
                "claimant 2 was born after the target, so ordering alone cannot disprove its \
                 claim and its evidence must establish"
            );
            let (protected, never_workload, _) =
                compute_protected_nodes(&scenario, &ever, &ever_observed, &pending);
            assert!(
                protected[1] && never_workload[1],
                "the target must stay protected on the strength of claimant 2's still-valid \
                 evidence, independent of claimant 0's invalidated one"
            );
        }

        /// A target confirmed gone (exited) must expire the pending evidence naming it, not merely
        /// be masked by `currently_unresolved` at consumption -- a distinct identity that later
        /// reused the same slot must never inherit stale evidence left set from before the exit.
        #[test]
        fn protected_oracle_pending_target_evidence_expires_once_the_target_is_confirmed_gone() {
            let nodes = vec![
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Claude,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(0),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            scenario.presence[0] = Presence::Live {
                unreadable: true,
                detached: false,
                omitted: false,
            };
            let mut ever = vec![false; 2];
            let mut ever_observed = vec![false; 2];
            let mut pending = vec![false; 2];
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(&scenario, &ever_observed, &mut pending);
            assert!(
                pending[1],
                "evidence must establish while the target is unresolved and the claimant is \
                 live, for this probe to mean anything"
            );

            scenario.presence[0] = Presence::Exited;
            scenario.gone.push(node_id(0));
            update_ever_shown_candidate(&scenario, &mut ever);
            update_ever_observed(&scenario, &mut ever_observed);
            update_claimant_pending_target_evidence(&scenario, &ever_observed, &mut pending);
            assert!(
                !pending[1],
                "a confirmed-gone target must expire the claimant's pending evidence immediately, \
                 not just be masked by currently_unresolved at consumption"
            );
        }

        /// Traced from seed `17894688641632127852`: a stale carrier that was observed on an
        /// earlier tick and is merely omitted this tick still counts -- production retains an
        /// identity's last observed ppid/env in `Cached`, so an omission (unlike a confirmed
        /// exit) never erases it.
        #[test]
        fn claim_is_stale_carrier_uses_the_carriers_retained_evidence_through_its_own_later_omission()
         {
            let nodes = vec![
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(2),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(0),
                    marker: MarkerChoice::None,
                    explicit_target: None,
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(1),
                    marker: MarkerChoice::CodexExplicit,
                    explicit_target: Some(2),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let mut scenario = Scenario::new(nodes);
            let mut ever_observed = vec![false; 3];
            // Establish node0 in the cache first (its own row observed, env readable).
            update_ever_observed(&scenario, &mut ever_observed);
            assert!(
                claim_is_stale_carrier(&scenario, &ever_observed, 2),
                "node0 shares node2's exact marker and target, and started before it, while \
                 fully observable -- the carrier must already be detected here, for this probe \
                 to mean anything"
            );

            // node0 (the carrier) goes omitted; node1 (the link between node0 and node2) and
            // node2 (the claimant) stay live. The carrier's own row is absent this tick, but its
            // last observed ppid/env must still be usable.
            scenario.presence[0] = Presence::Live {
                unreadable: false,
                detached: false,
                omitted: true,
            };
            update_ever_observed(&scenario, &mut ever_observed);
            assert!(
                claim_is_stale_carrier(&scenario, &ever_observed, 2),
                "the carrier's own later omission must not erase its already-retained evidence"
            );
        }

        /// Traced from seed `6055241528473048448` (order_seed=0): a self-referencing explicit
        /// claim (target == claimant) is not exempt from the stale-carrier guard -- a different
        /// node on the claimant's own ancestry can still name the claimant itself as root while
        /// having started first.
        #[test]
        fn claim_is_stale_carrier_applies_to_a_self_referencing_claim_too() {
            let nodes = vec![
                Node {
                    kind: Kind::Plain,
                    parent: None,
                    marker: MarkerChoice::ClaudeExplicit,
                    explicit_target: Some(1),
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
                Node {
                    kind: Kind::Plain,
                    parent: Some(0),
                    marker: MarkerChoice::ClaudeExplicit,
                    explicit_target: None, // defaults to self (node 1)
                    foreign_uid: false,
                    env_permanently_unknown: false,
                },
            ];
            let scenario = Scenario::new(nodes);
            let mut ever_observed = vec![false; 2];
            update_ever_observed(&scenario, &mut ever_observed);
            assert!(
                claim_is_stale_carrier(&scenario, &ever_observed, 1),
                "node0 names node1 as root via the identical marker and started first -- node1's \
                 own self-referencing claim must still be recognized as a stale carrier, not \
                 exempted just because its target equals itself"
            );
        }
    }

    /// Named regressions for `check_scenario` (the full generated-forest invariants property),
    /// analogous to `recovery_regressions` above but for `attribution_invariants_hold_across_generated_process_forests`.
    mod invariants_regressions {
        use super::*;

        #[test]
        fn hidden_detach_does_not_override_retained_stale_carrier_evidence() {
            check_scenario(16165595305885321366, 0).unwrap();
            check_scenario(2866176950147402298, 0).unwrap();
        }

        /// Found by the property run after the stale-carrier oracle fix above: a self-
        /// referencing explicit claim was wrongly exempted from the stale-carrier guard. See
        /// scratch/t04-fixup-4/root-raw-seeds.log (coordinator's evidence log) for the raw dump.
        #[test]
        fn stale_carrier_applies_to_self_referencing_claims_seed_6055241528473048448() {
            check_scenario(6055241528473048448, 0).unwrap();
        }
        /// Same root cause and fix as the recovery seed ratchet in `recovery_regressions`: an
        /// explicit claim's foreign-uid target was treated as merely pending instead of
        /// positively invalid once its row (and so its uid) was observed.
        #[test]
        fn an_explicit_claims_foreign_uid_target_is_positively_invalid_once_observed_seed_10085948392207250504()
         {
            check_scenario(10085948392207250504, 0).unwrap();
        }
        /// Traced (scratch/t04-fixup-4/root-raw-145835.log, coordinator's evidence log; test-
        /// author trace notes in scratch/t04-fixup-4/test-author-trace-seed14583579529671133188.log)
        /// to `claim_is_stale_carrier`'s ancestor walk stopping on ANY `Presence::Exited`,
        /// discarding an ancestor-carrier's retained ppid/env evidence even though its exit was
        /// never confirmed (still merely Unknown liveness, exactly like an omission) and no
        /// pid-reuse replacement had appeared at its slot yet -- production keeps searching
        /// through an identity whose liveness is still Unknown.
        #[test]
        fn claim_is_stale_carrier_retains_an_ancestors_evidence_through_its_own_unconfirmed_exit_seed_14583579529671133188()
         {
            check_scenario(14583579529671133188, 0).unwrap();
        }
    }

    /// `PROPTEST_CASES` still wins when set (checked directly, since `ProptestConfig::default()`
    /// -- which does honor it -- gets overwritten by an explicit `cases` field below it): a
    /// meaningful CI default of 2048 cases otherwise, well above proptest's own default of 256.
    /// For a real stress run: `PROPTEST_CASES=100000 cargo test --release --lib
    /// attribution::tests::property`.
    fn ci_proptest_config() -> ProptestConfig {
        let mut config = ProptestConfig::default();
        if std::env::var_os("PROPTEST_CASES").is_none() {
            config.cases = 2048;
        }
        config
    }

    proptest! {
        #![proptest_config(ci_proptest_config())]
        #[test]
        fn attribution_invariants_hold_across_generated_process_forests(
            seed in any::<u64>(),
            order_seed in any::<u64>(),
        ) {
            check_scenario(seed, order_seed)?;
        }

        #[test]
        fn attribution_recovers_to_fresh_once_reads_succeed_on_fixed_topology(
            seed in any::<u64>(),
        ) {
            check_recovery_scenario(seed)?;
        }
    }
}
