//! Native observation and identity-checked signals for spikes/run_scenarios.py.
use ballast::attribution::Attributor;
use ballast::daemon::{
    Observer, ProcessChanges, Snapshot, Status,
    files::{Config, Mode, Paths, RotatingLog},
    ipc::{Client, Method, Published, Reply, Response, Server},
};
use ballast::guardian::{Guardian, Level};
use ballast::platform::*;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    process::Command,
    sync::{Arc, RwLock, atomic::Ordering, mpsc},
    time::{Duration, Instant, SystemTime},
};
fn identities(path: &Path) -> io::Result<HashSet<ProcessIdentity>> {
    let text = fs::read_to_string(path)?;
    // An append may still be in flight. The next scan picks up its complete line.
    text.split_inclusive('\n')
        .filter(|s| s.ends_with('\n'))
        .map(|s| serde_json::from_str(s).map_err(io::Error::other))
        .collect()
}
fn register(path: &Path, id: ProcessIdentity) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{}\n", serde_json::to_string(&id)?).as_bytes())
}
struct OwnedPlatform {
    native: NativePlatform,
    registry: std::path::PathBuf,
    owned: HashSet<ProcessIdentity>,
}
impl Platform for OwnedPlatform {
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
        let registered = identities(&self.registry)?;
        let mut metric_targets = metrics.clone();
        metric_targets.extend(&registered);
        metric_targets.extend(&self.owned);
        let mut all = self.native.list_processes(watched, &metric_targets)?;
        let mut pids: HashSet<_> = all
            .iter()
            .filter(|p| registered.contains(&p.identity) || self.owned.contains(&p.identity))
            .map(|p| p.identity.pid)
            .collect();
        loop {
            let before = pids.len();
            for p in &all {
                if pids.contains(&p.ppid) || self.owned.contains(&p.identity) {
                    pids.insert(p.identity.pid);
                }
            }
            if before == pids.len() {
                break;
            }
        }
        all.retain(|p| pids.contains(&p.identity.pid));
        for p in &all {
            if !registered.contains(&p.identity) {
                register(&self.registry, p.identity)?;
            }
        }
        self.owned = all.iter().map(|p| p.identity).collect();
        Ok(all)
    }
    fn read_environment(&self, id: ProcessIdentity) -> Option<Environment> {
        self.native.read_environment(id)
    }
    fn process_metrics(&self, id: ProcessIdentity) -> Option<ProcessMetrics> {
        self.native.process_metrics(id)
    }
    fn process_age(&self, id: ProcessIdentity) -> Option<Duration> {
        self.native.process_age(id)
    }
    fn process_cwd(&self, id: ProcessIdentity) -> Option<std::path::PathBuf> {
        self.native.process_cwd(id)
    }
    fn process_liveness(&self, id: ProcessIdentity) -> ProcessLiveness {
        self.native.process_liveness(id)
    }
    fn pid_is_present(&self, pid: i32) -> Option<bool> {
        self.native.pid_is_present(pid)
    }
    fn pressure(&self) -> io::Result<PressureInputs> {
        self.native.pressure()
    }
    fn listening_ports(&self, id: ProcessIdentity) -> Option<Vec<u16>> {
        self.native.listening_ports(id)
    }
    fn send_signal(&self, id: ProcessIdentity, signal: Signal) -> io::Result<()> {
        if !self.owned.contains(&id) {
            return Err(io::Error::other("probe rejected non-worker signal"));
        }
        self.native.send_signal(id, signal)
    }
    fn notify(&self, _title: &str, _body: &str) -> io::Result<bool> {
        Ok(false)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mut native = NativePlatform::new()?;
    native.notifications = false;
    match args.get(1).map(String::as_str) {
        Some("pressure") => { println!("{}", serde_json::to_string(&native.pressure()?)?); }
        Some("register") => {
            let pid: i32 = args[3].parse()?;
            let id = native.list_processes(&HashSet::new(), &HashSet::new())?.into_iter().find(|p| p.identity.pid == pid).ok_or("process disappeared before registration")?.identity;
            register(Path::new(&args[2]), id)?;
        }
        Some("agent") => agent(&args[2], &args[3], &args[4])?,
        Some("exec") => {
            let plan: serde_json::Value = serde_json::from_str(&fs::read_to_string(args.last().ok_or("plan")?)?)?;
            let id = native.list_processes(&HashSet::new(), &HashSet::new())?.into_iter().find(|p| p.identity.pid == std::process::id() as i32).ok_or("app identity")?.identity;
            register(Path::new(plan["registry"].as_str().ok_or("registry")?), id)?;
            let status = Command::new(&args[2]).args(&args[3..]).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Some("signal") => {
            let ids = identities(Path::new(&args[2]))?;
            native.list_processes(&ids, &HashSet::new())?;
            let signal = match args[3].as_str() { "resume" => Signal::Continue, "term" => Signal::Terminate, "kill" => Signal::Kill, _ => return Err("unknown signal".into()) };
            for id in ids { if native.process_liveness(id) == ProcessLiveness::Alive { let _ = native.send_signal(id, signal); } }
        }
        Some("verify") => {
            native.list_processes(&identities(Path::new(&args[2]))?, &HashSet::new())?;
            let remaining: Vec<_> = identities(Path::new(&args[2]))?.into_iter().filter(|id| native.process_liveness(*id) != ProcessLiveness::Gone).collect();
            println!("{}", serde_json::to_string(&remaining)?);
            if !remaining.is_empty() { return Err("owned processes remain or cannot be verified".into()); }
        }
        Some("monitor") => monitor(&args[2], &args[3], &args[4], args[5].parse()?)?,
        _ => return Err("pressure | register REG PID | agent CMD... | signal REG resume/term/kill | verify REG | monitor REG STOP baseline/scoped/daemon SECONDS".into()),
    }
    Ok(())
}
fn monitor(
    registry: &str,
    stop: &str,
    mode: &str,
    seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::from_env()?;
    paths.prepare()?;
    let config = Config::load(&paths)?;
    let mut platform = OwnedPlatform {
        native: NativePlatform::new()?,
        registry: registry.into(),
        owned: HashSet::new(),
    };
    platform.native.notifications = false;
    let boot_id = platform.boot_id()?;
    let mut guardian = Guardian::new(
        paths.clone(),
        boot_id.clone(),
        if mode == "passive" {
            Mode::Observe
        } else {
            Mode::Enforce
        },
        config.pressure.clone(),
    );
    let mut log = RotatingLog::open(paths.base.join("log/decisions.jsonl"), &config)?;
    let mut attributor = Attributor::new(config.markers, config.shells);
    let mut observer = Observer::default();
    let start = Instant::now();
    let mut tick = 0;
    let mut next_tick = Instant::now();
    let mut admission = ballast::hooks::Admission::new(&config.heavy_commands);
    let mut hook_state = ballast::hooks::HookState::default();
    let (send, requests) = mpsc::sync_channel(128);
    let mut server = if mode == "scoped_ipc" {
        Some((Server::bind(paths.clone())?, send))
    } else {
        None
    };
    let mut published: Option<Published> = None;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        while start.elapsed().as_secs() < seconds && !Path::new(stop).exists() {
            let now = Instant::now();
            let due = now >= next_tick;
            let discard = due && observer.begin_tick(now, SystemTime::now());
            let mut processes = platform.list_processes(
                &attributor.watched(&guardian.watched()),
                &attributor.metric_targets(),
            )?;
            let raw = platform.pressure()?;
            let attribution = attributor.update(
                &platform,
                &mut processes,
                now,
                ballast::daemon::unix_ms(),
                discard,
            );
            let mut snapshot = Snapshot {
                status: Status {
                    daemon_version: "scenario".into(),
                    pid: std::process::id(),
                    mode: Mode::Enforce,
                    tick,
                    sampled_at_ms: ballast::daemon::unix_ms(),
                    tick_interval_ms: observer.interval().as_millis() as u64,
                    tick_cpu_ns: 0,
                    tick_wall_ns: 0,
                    sample_discarded: discard,
                    process_count: processes.len(),
                    pressure_level: guardian.level,
                    batch_running: false,
                    cleanup_pending: Vec::new(),
                    last_error: None,
                },
                boot_id: boot_id.clone(),
                capabilities: platform.capabilities(),
                processes,
                changes: ProcessChanges::default(),
                pressure: (!discard).then_some(raw.clone()),
                attribution,
                frozen: Vec::new(),
                held: Vec::new(),
                guardian: None,
                today: Default::default(),
            };
            if mode == "scoped_ipc" {
                hook_state.apply(&mut snapshot.attribution, now);
            }
            let mut level = None;
            let mut frozen = 0;
            if matches!(mode, "scoped" | "scoped_ipc" | "passive") {
                if due {
                    guardian.tick(now, &snapshot, &mut platform, &mut attributor, &mut log)?;
                    observer.set_fast_polling(guardian.level != Level::Normal);
                    next_tick = now + observer.interval();
                }
                level = Some(guardian.level);
                frozen = guardian.frozen.len();
            } else if mode == "daemon" {
                match Client::connect(&paths, Duration::from_millis(300))
                    .and_then(|mut c| c.request(Method::Snapshot))
                {
                    Ok(response) => {
                        if let Reply::Snapshot { snapshot } = response.reply {
                            level = Some(snapshot.status.pressure_level);
                            frozen = snapshot.frozen.len();
                        }
                    }
                    Err(e) => eprintln!("daemon snapshot: {e}"),
                }
            }
            if mode == "scoped_ipc" && due {
                snapshot.status.pressure_level = guardian.level;
                snapshot.status.batch_running =
                    guardian.batch_running(&snapshot.attribution, &snapshot.processes);
                snapshot.frozen = guardian.frozen.clone();
                admission.tick(&snapshot, &mut hook_state, &mut log, now);
                snapshot.held = admission.held();
                snapshot.guardian = Some(guardian.note.clone());
            }
            println!(
                "{}",
                serde_json::json!({"time_ms": ballast::daemon::unix_ms(), "elapsed_s": now.duration_since(start).as_secs_f64(), "guardian_tick": due, "discarded": discard, "note": guardian.note, "inputs":raw,"level":level,"frozen":frozen,"attribution":snapshot.attribution, "processes":snapshot.processes})
            );
            io::stdout().flush()?;
            if mode == "scoped_ipc" && due {
                let current = Arc::new(snapshot);
                if let Some(view) = &published {
                    *view.write().unwrap() = current;
                } else {
                    published = Some(Arc::new(RwLock::new(current)));
                }
                if let Some((server, send)) = server.take() {
                    let view = Arc::clone(published.as_ref().unwrap());
                    std::thread::spawn(move || {
                        if let Err(e) = server.run(view, send) {
                            eprintln!("scoped IPC: {e}");
                            std::process::exit(1);
                        }
                    });
                }
            }
            tick += 1;
            let sample_deadline = now + Duration::from_millis(250);
            if mode == "scoped_ipc" {
                while Instant::now() < sample_deadline {
                    match requests
                        .recv_timeout(sample_deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(request) if !request.cancelled.load(Ordering::Relaxed) => {
                            if let Method::Hook { payload } = &request.method {
                                match serde_json::from_value::<ballast::hooks::HookRequest>(
                                    payload.clone(),
                                ) {
                                    Ok(hook) if !hook.session_id.is_empty() => {
                                        let view = Arc::clone(
                                            &published.as_ref().unwrap().read().unwrap(),
                                        );
                                        admission.handle(
                                            hook,
                                            request,
                                            &view,
                                            &mut hook_state,
                                            &mut log,
                                            Instant::now(),
                                        );
                                    }
                                    _ => {
                                        let _ = request.reply.send(Response::error("invalid hook"));
                                    }
                                }
                            } else {
                                let _ = request.reply.send(Response::error(
                                    "measurement server supports hooks and snapshots",
                                ));
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            } else {
                std::thread::sleep(sample_deadline.saturating_duration_since(Instant::now()));
            }
        }
        Ok(())
    })();
    if matches!(mode, "scoped" | "scoped_ipc") {
        guardian.resume(None, Instant::now(), &platform, &mut log)?;
    }
    result
}

fn agent(python: &str, script: &str, plan_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let plan: serde_json::Value = serde_json::from_str(&fs::read_to_string(plan_path)?)?;
    let deadline = plan["deadline"].as_f64().ok_or("deadline")?;
    let start = plan["start"].as_f64().ok_or("start")?;
    let scenario = plan["scenario"].as_u64().ok_or("scenario")?;
    let dry = plan["dry"].as_bool().ok_or("dry")?;
    let stop = Path::new(plan["stop"].as_str().ok_or("stop")?);
    let registry = Path::new(plan["registry"].as_str().ok_or("registry")?);
    let wall = || {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    };
    let active = || wall() < deadline && !stop.exists();
    let mut native = NativePlatform::new()?;
    let self_id = native
        .list_processes(&HashSet::new(), &HashSet::new())?
        .into_iter()
        .find(|p| p.identity.pid == std::process::id() as i32)
        .ok_or("agent identity")?
        .identity;
    register(registry, self_id)?;
    let mut kinds = match scenario {
        1 => vec!["memory"; 4],
        2 => vec!["memory"],
        3 => vec!["cpu"; plan["cores"].as_u64().ok_or("cores")? as usize],
        4 => vec!["server", "client"],
        5 => vec!["mcp"],
        6 => vec!["launcher"],
        7 => vec!["disk"],
        _ => return Err("scenario".into()),
    };
    if plan["cpu_hold_memory"] == true {
        kinds.push("memory");
    }
    // Drop is also the error path: these groups belong to direct unreaped children.
    struct Children(Vec<(std::process::Child, bool)>);
    impl Drop for Children {
        fn drop(&mut self) {
            for (child, group) in &mut self.0 {
                if child.try_wait().ok().flatten().is_none() {
                    unsafe {
                        libc::kill(
                            if *group {
                                -(child.id() as i32)
                            } else {
                                child.id() as i32
                            },
                            libc::SIGCONT,
                        );
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(300));
            for (child, group) in &mut self.0 {
                // try_wait reaps only after cooperative workers have finished.
                if child.try_wait().ok().flatten().is_none() {
                    unsafe {
                        libc::kill(
                            if *group {
                                -(child.id() as i32)
                            } else {
                                child.id() as i32
                            },
                            libc::SIGTERM,
                        );
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(300));
            for (child, group) in &mut self.0 {
                if child.try_wait().ok().flatten().is_none() {
                    unsafe {
                        libc::kill(
                            if *group {
                                -(child.id() as i32)
                            } else {
                                child.id() as i32
                            },
                            libc::SIGKILL,
                        );
                    }
                }
                let _ = child.wait();
            }
        }
    }
    let mut children = Children(Vec::new());
    for (index, kind) in kinds.iter().enumerate() {
        while active() && scenario == 1 && !dry && wall() < start + index as f64 * 8.0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !active() {
            break;
        }
        if plan["hooks"] == true {
            let mut hook = Command::new(python)
                .args([script, "--hook", "--plan", plan_path])
                .env("CLAUDE_PID", self_id.pid.to_string())
                .stdin(Stdio::null())
                .spawn()?;
            // Admission can hold; polling bounds it to the scenario's lifetime.
            loop {
                if let Some(status) = hook.try_wait()? {
                    if !status.success() {
                        return Err("hook failed".into());
                    }
                    break;
                }
                if !active() {
                    hook.kill()?;
                    hook.wait()?;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !active() {
                break;
            }
        }
        let group = *kind != "mcp";
        let mut cmd = if group {
            let mut c = Command::new("/bin/sh");
            c.args([
                "-c",
                "\"$@\" & worker=$!; wait \"$worker\"",
                "scenario",
                python,
            ]);
            c
        } else {
            Command::new(python)
        };
        cmd.args([
            script,
            "--worker",
            kind,
            "--plan",
            plan_path,
            "--index",
            &index.to_string(),
        ])
        .env("CLAUDE_PID", self_id.pid.to_string());
        if group {
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        let pid = child.id() as i32;
        children.0.push((child, group));
        if let Some(p) = native
            .list_processes(&HashSet::new(), &HashSet::new())?
            .into_iter()
            .find(|p| p.identity.pid == pid)
        {
            register(registry, p.identity)?;
        }
    }
    while active() {
        std::thread::sleep(Duration::from_millis(50));
    }
    // A stopped worker must run again to reach its own deadline and release memory.
    for (child, group) in &children.0 {
        unsafe {
            libc::kill(
                if *group {
                    -(child.id() as i32)
                } else {
                    child.id() as i32
                },
                libc::SIGCONT,
            );
        }
    }
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let mut complete = true;
        for (child, _) in &mut children.0 {
            match child.try_wait()? {
                Some(status) if !status.success() => {
                    return Err(format!("worker group {} exited {status}", child.id()).into());
                }
                None => complete = false,
                _ => {}
            }
        }
        if complete {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err("workers did not finish after resume".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
