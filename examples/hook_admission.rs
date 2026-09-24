//! Real hook verification with forced pressure, without allocating memory or signalling processes.
//! BALLAST_HOME/pressure contains `normal`, `elevated`, or `critical`.
use ballast::attribution::AttributionSnapshot;
use ballast::daemon::{
    ProcessChanges, Snapshot, Status,
    files::{Config, Mode, Paths, RotatingLog},
    ipc::{self, Method, Reply, Response},
};
use ballast::guardian::Level;
use ballast::hooks::{Admission, Event, HookRequest, HookState};
use ballast::platform::{NativePlatform, Platform};
use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    paths.prepare()?;
    let config = Config::load(&paths)?;
    let mut decisions = RotatingLog::open(paths.base.join("log/decisions.jsonl"), &config)?;
    let platform = NativePlatform::new()?;
    let mut snapshot = Snapshot {
        status: Status {
            daemon_version: "admission-probe".into(),
            pid: std::process::id(),
            mode: Mode::Enforce,
            tick: 0,
            sampled_at_ms: 0,
            tick_interval_ms: 50,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded: false,
            process_count: 0,
            pressure_level: Level::Normal,
            batch_running: true,
            cleanup_pending: Vec::new(),
            last_error: None,
        },
        boot_id: platform.boot_id()?,
        capabilities: platform.capabilities(),
        processes: Vec::new(),
        changes: ProcessChanges::default(),
        pressure: Some(platform.pressure()?),
        attribution: AttributionSnapshot::default(),
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
    };
    let published = Arc::new(RwLock::new(Arc::new(serde_json::from_value::<Snapshot>(
        serde_json::to_value(&snapshot)?,
    )?)));
    let server = ipc::Server::bind(paths.clone())?;
    let (send, receive) = mpsc::sync_channel(128);
    std::thread::spawn(move || server.run(published, send));
    let mut admission = Admission::new(&config.heavy_commands);
    let mut state = HookState::default();
    loop {
        snapshot.status.pressure_level =
            match std::fs::read_to_string(paths.base.join("pressure"))?.trim() {
                "elevated" => Level::Elevated,
                "critical" => Level::Critical,
                _ => Level::Normal,
            };
        admission.tick(&snapshot, &mut state, &mut decisions, Instant::now());
        if let Ok(request) = receive.recv_timeout(Duration::from_millis(50)) {
            if let Method::Hook { payload } = &request.method {
                if let Ok(hook) = serde_json::from_value::<HookRequest>(payload.clone()) {
                    println!(
                        "{}",
                        serde_json::json!({"event": hook.event, "agent": hook.agent,
                        "shell": hook.shell_command().is_some(), "pre": hook.event == Event::PreToolUse})
                    );
                    admission.handle(
                        hook,
                        request,
                        &snapshot,
                        &mut state,
                        &mut decisions,
                        Instant::now(),
                    );
                    continue;
                }
            }
            let _ = request.reply.send(Response::new(Reply::Error {
                message: "probe accepts hooks only".into(),
            }));
        }
    }
}
