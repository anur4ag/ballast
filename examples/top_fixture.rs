//! Bounded TUI verification server with production guardian, admission, and IPC.
//! Synthetic pressure; only the three sleep children created here can be signalled.
//! Use a fresh BALLAST_HOME; write normal/elevated/critical to its pressure file.
use ballast::attribution::{
    Agent, AgentState, AttributionSnapshot, Attributor, MemorySummary, Owner, ProcessAttribution,
    ProcessRole, Workload, WorkloadClass,
};
use ballast::daemon::{
    ProcessChanges, Snapshot, Status,
    files::{Config, Mode, Paths, RotatingLog},
    ipc::{self, Method, Reply, Response},
};
use ballast::guardian::{Guardian, Level};
use ballast::hooks::{Admission, HookRequest, HookState};
use ballast::platform::*;
use std::{
    collections::HashSet,
    io,
    process::{Child, Command},
    sync::{Arc, RwLock, mpsc},
    time::{Duration, Instant},
};

struct Owned {
    native: NativePlatform,
    children: Vec<Child>,
}
impl Drop for Owned {
    fn drop(&mut self) {
        for child in &mut self.children {
            // Unreaped children cannot have their PIDs reused.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGCONT);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Platform for Owned {
    fn capabilities(&self) -> Capabilities {
        self.native.capabilities()
    }
    fn boot_id(&self) -> io::Result<String> {
        self.native.boot_id()
    }
    fn list_processes(
        &mut self,
        watched: &HashSet<ProcessIdentity>,
        metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        let mut p = self.native.list_processes(watched, metrics)?;
        p.retain(|p| {
            self.children
                .iter()
                .any(|c| c.id() as i32 == p.identity.pid)
        });
        Ok(p)
    }
    fn read_environment(&self, _: ProcessIdentity) -> Option<Environment> {
        None
    }
    fn process_metrics(&self, id: ProcessIdentity) -> Option<ProcessMetrics> {
        self.native.process_metrics(id)
    }
    fn process_liveness(&self, id: ProcessIdentity) -> ProcessLiveness {
        self.native.process_liveness(id)
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        self.native.pressure()
    }
    fn listening_ports(&self, _: ProcessIdentity) -> Option<Vec<u16>> {
        None
    }
    fn send_signal(&self, id: ProcessIdentity, signal: Signal) -> io::Result<()> {
        if !self.children.iter().any(|c| c.id() as i32 == id.pid) {
            return Err(io::Error::other("not a fixture child"));
        }
        self.native.send_signal(id, signal)
    }
    fn notify(&self, _: &str, _: &str) -> io::Result<bool> {
        Ok(false)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    paths.prepare()?;
    let config = Config::default();
    let mut log = RotatingLog::open(paths.base.join("log/decisions.jsonl"), &config)?;
    let mut platform = Owned {
        native: NativePlatform::new()?,
        children: Vec::new(),
    };
    for _ in 0..3 {
        platform
            .children
            .push(Command::new("sleep").arg("180").spawn()?);
    }
    let mut processes = platform.list_processes(&HashSet::new(), &HashSet::new())?;
    processes.sort_by_key(|p| p.identity);
    let identities: HashSet<_> = processes.iter().map(|p| p.identity).collect();
    if identities.len() != 3 {
        return Err("fixture could not observe its own children".into());
    }
    let mut guardian = Guardian::new(
        paths.clone(),
        platform.boot_id()?,
        Mode::Enforce,
        config.pressure,
    );
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut admission = Admission::new(&[]);
    let mut hook_state = HookState::default();
    let mut snapshot = Snapshot {
        status: Status {
            daemon_version: "top-fixture".into(),
            pid: std::process::id(),
            mode: Mode::Enforce,
            tick: 0,
            sampled_at_ms: ballast::daemon::unix_ms(),
            tick_interval_ms: 50,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded: false,
            process_count: 3,
            pressure_level: Level::Normal,
            batch_running: true,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: platform.boot_id()?,
        capabilities: platform.capabilities(),
        processes,
        changes: ProcessChanges::default(),
        pressure: None,
        attribution: AttributionSnapshot::default(),
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
    };
    snapshot.attribution.owners.push(Owner {
        id: "fixture".into(),
        name: Some("TUI verification fleet".into()),
    });
    for (id, kind, state) in [
        ("agent-a", "codex", AgentState::Thinking),
        ("agent-b", "claude", AgentState::Idle),
    ] {
        snapshot.attribution.agents.push(Agent {
            id: id.into(),
            owner_id: Some("fixture".into()),
            session_id: Some(id.into()),
            kind: kind.into(),
            root: None,
            cwd: None,
            state,
            ended_at_ms: None,
            memory: MemorySummary::default(),
        });
    }
    for (i, process) in snapshot.processes.iter().enumerate() {
        let id = format!("fixture-work-{}", i + 1);
        let agent = if i == 2 { "agent-b" } else { "agent-a" };
        snapshot.attribution.workloads.push(Workload {
            id: id.clone(),
            agent_id: agent.into(),
            root: process.identity,
            label: format!("synthetic workload {}", i + 1),
            class: WorkloadClass::Batch,
            first_seen_ms: ballast::daemon::unix_ms() + i as u64,
            detached_pgid: None,
            memory: MemorySummary {
                bytes: 0,
                complete: true,
                growth_30s_bytes: (i > 0).then_some(i as i64),
            },
        });
        snapshot.attribution.processes.push(ProcessAttribution {
            identity: process.identity,
            owner_id: Some("fixture".into()),
            agent_id: Some(agent.into()),
            workload_id: Some(id),
            role: ProcessRole::Workload,
            environment_known: true,
            listening_ports: None,
            ports_sampled_at_ms: None,
        });
    }
    let published = Arc::new(RwLock::new(Arc::new(snapshot)));
    let server = ipc::Server::bind(paths.clone())?;
    let (send, receive) = mpsc::sync_channel(128);
    let view = Arc::clone(&published);
    std::thread::spawn(move || server.run(view, send));
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(150) {
        let now = Instant::now();
        let mut snapshot: Snapshot =
            serde_json::from_value(serde_json::to_value(&**published.read().unwrap())?)?;
        snapshot.status.tick += 1;
        snapshot.status.sampled_at_ms = ballast::daemon::unix_ms();
        snapshot.processes = platform.list_processes(&identities, &identities)?;
        snapshot.processes.sort_by_key(|p| p.identity);
        // Synthetic footprints exercise filled meters without allocating real pressure.
        for (i, process) in snapshot.processes.iter_mut().enumerate() {
            if let Some(metrics) = &mut process.metrics {
                metrics.memory_bytes = (3 - i as u64) << 30;
            }
        }
        for agent in &mut snapshot.attribution.agents {
            agent.memory = MemorySummary {
                complete: true,
                ..Default::default()
            };
        }
        for w in &mut snapshot.attribution.workloads {
            w.memory.bytes = snapshot
                .processes
                .iter()
                .find(|p| p.identity == w.root)
                .and_then(|p| p.metrics)
                .map_or(0, |m| m.memory_bytes);
            snapshot
                .attribution
                .agents
                .iter_mut()
                .find(|a| a.id == w.agent_id)
                .unwrap()
                .memory
                .bytes += w.memory.bytes;
        }
        let forced = std::fs::read_to_string(paths.base.join("pressure")).unwrap_or_default();
        if forced.trim() == "exit" {
            break;
        }
        let level = match forced.trim() {
            "critical" => 4,
            "elevated" => 2,
            _ => 1,
        };
        snapshot.pressure = Some(PressureInputs {
            page_size: 4096,
            total_memory_bytes: Some(16 << 30),
            used_memory_bytes: Some(8 << 30),
            swap_used_bytes: Some(1 << 30),
            swap_total_bytes: Some(4 << 30),
            kernel_pressure_level: Some(level),
            ..Default::default()
        });
        guardian.tick(now, &snapshot, &mut platform, &mut attributor, &mut log)?;
        snapshot.status.pressure_level = guardian.level;
        snapshot.status.batch_running =
            guardian.batch_running(&snapshot.attribution, &snapshot.processes);
        snapshot.frozen = guardian.frozen.clone();
        snapshot.guardian = Some(guardian.note.clone());
        admission.tick(&snapshot, &mut hook_state, &mut log, now);
        // Keep the fixture below the next freeze while retaining Elevated pressure for holds.
        if !snapshot.frozen.is_empty() && forced.trim() == "critical" {
            std::fs::write(paths.base.join("pressure"), "elevated")?;
        }
        while let Ok(request) = receive.try_recv() {
            if let Method::Hook { payload } = &request.method {
                let hook: HookRequest = serde_json::from_value(payload.clone())?;
                admission.handle(hook, request, &snapshot, &mut hook_state, &mut log, now);
            } else if let Method::Resume { target } = &request.method {
                let count = guardian.resume(target.as_deref(), now, &platform, &mut log)?;
                let _ = request.reply.send(Response::new(Reply::Resumed { count }));
            }
        }
        snapshot.held = admission.held();
        *published.write().unwrap() = Arc::new(snapshot);
        std::thread::sleep(Duration::from_millis(50).saturating_sub(now.elapsed()));
    }
    guardian.resume(None, Instant::now(), &platform, &mut log)?;
    Ok(())
}
