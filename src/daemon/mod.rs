pub mod files;
pub mod ipc;
#[cfg(test)]
mod tests;

use crate::attribution::{AttributionSnapshot, Attributor};
use crate::guardian::{FrozenWorkload, Guardian, Level, recovery};
use crate::platform::{
    Capabilities, NativePlatform, Platform, PressureInputs, Process, ProcessIdentity,
};
use files::{Config, Mode, Paths, RotatingLog};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub daemon_version: String,
    pub pid: u32,
    pub mode: Mode,
    pub tick: u64,
    pub sampled_at_ms: u64,
    pub tick_interval_ms: u64,
    pub tick_cpu_ns: u64,
    pub tick_wall_ns: u64,
    pub sample_discarded: bool,
    pub process_count: usize,
    #[serde(default)]
    pub pressure_level: Level,
    #[serde(default)]
    pub batch_running: bool,
    #[serde(default)]
    pub cleanup_pending: Vec<String>,
    pub last_error: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub status: Status,
    pub boot_id: String,
    pub capabilities: Capabilities,
    pub processes: Vec<Process>,
    pub changes: ProcessChanges,
    pub pressure: Option<PressureInputs>,
    pub attribution: AttributionSnapshot,
    #[serde(default)]
    pub frozen: Vec<FrozenWorkload>,
    #[serde(default)]
    pub held: Vec<crate::hooks::HeldCommand>,
    #[serde(default)]
    pub guardian: Option<crate::guardian::GuardianNote>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProcessChanges {
    pub started: Vec<ProcessIdentity>,
    pub exited: Vec<ProcessIdentity>,
    pub exec_changed: Vec<ProcessIdentity>,
}

/// The guardian changes cadence through this state, on the tick thread only.
pub struct Observer {
    previous: HashMap<ProcessIdentity, Option<String>>,
    current: HashSet<ProcessIdentity>,
    interval: Duration,
    last_tick: Option<(Instant, SystemTime)>,
}
impl Default for Observer {
    fn default() -> Self {
        Self {
            previous: HashMap::with_capacity(2048),
            current: HashSet::with_capacity(2048),
            interval: Duration::from_secs(1),
            last_tick: None,
        }
    }
}
impl Observer {
    pub fn set_fast_polling(&mut self, fast: bool) {
        self.interval = Duration::from_millis(if fast { 250 } else { 1000 });
    }
    pub fn interval(&self) -> Duration {
        self.interval
    }
    pub fn begin_tick(&mut self, now: Instant, wall: SystemTime) -> bool {
        let discard = self.last_tick.is_none_or(|(previous, previous_wall)| {
            now.duration_since(previous) > self.interval * 5
                || wall
                    .duration_since(previous_wall)
                    .map_or(true, |gap| gap > self.interval * 5)
        });
        self.last_tick = Some((now, wall));
        discard
    }
    pub fn diff(&mut self, processes: &[Process]) -> ProcessChanges {
        let mut changes = ProcessChanges::default();
        self.current.clear();
        for process in processes {
            match self.previous.get_mut(&process.identity) {
                None => {
                    changes.started.push(process.identity);
                    self.previous.insert(process.identity, process.exe.clone());
                }
                Some(exe) if *exe != process.exe => {
                    changes.exec_changed.push(process.identity);
                    exe.clone_from(&process.exe);
                }
                Some(_) => {}
            }
            self.current.insert(process.identity);
        }
        changes.exited.extend(
            self.previous
                .keys()
                .filter(|id| !self.current.contains(id))
                .copied(),
        );
        self.previous.retain(|id, _| self.current.contains(id));
        changes
    }
}

pub fn run(paths: Paths) -> io::Result<()> {
    run_with_targets(paths, HashSet::new(), HashSet::new())
}

fn run_with_targets(
    paths: Paths,
    extra_watched: HashSet<ProcessIdentity>,
    extra_metrics: HashSet<ProcessIdentity>,
) -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "run ballast daemon as your user, never root",
        ));
    }
    // Set before spawning threads so socket creation is private from the outset.
    unsafe {
        libc::umask(0o077);
    }
    paths.prepare()?;
    let loaded_config = Config::load(&paths);
    let config = loaded_config.as_ref().cloned().unwrap_or_default();
    let mut log = RotatingLog::open(paths.base.join("log/daemon.log"), &config)?;
    let (server, mut decisions, mut platform, boot_id) = (|| {
        let server = ipc::Server::bind(paths.clone())?;
        let mut decisions = RotatingLog::open(paths.base.join("log/decisions.jsonl"), &config)?;
        log.write_line(&format!(
            "{} daemon {} starting",
            unix_ms(),
            env!("CARGO_PKG_VERSION")
        ))?;
        let mut platform = NativePlatform::new()?;
        let boot_id = platform.boot_id()?;
        recovery::recover(&paths, &mut platform, &config.markers, &mut decisions)?;
        loaded_config?;
        Ok((server, decisions, platform, boot_id))
    })()
    .inspect_err(|error: &io::Error| {
        let _ = log.write_line(&format!("{} daemon startup failed: {error}", unix_ms()));
    })?;
    let mut guardian = Guardian::new(
        paths.clone(),
        boot_id.clone(),
        config.mode,
        config.pressure.clone(),
    );
    let mut cleanup = crate::cleanup::Cleanup::new(
        config.mode,
        Duration::from_secs(config.cleanup_grace_seconds),
    );
    let capabilities = platform.capabilities();
    let mut status = Status {
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        mode: config.mode,
        tick: 0,
        sampled_at_ms: 0,
        tick_interval_ms: 1000,
        tick_cpu_ns: 0,
        tick_wall_ns: 0,
        sample_discarded: true,
        process_count: 0,
        pressure_level: Level::Normal,
        batch_running: false,
        cleanup_pending: Vec::new(),
        last_error: None,
    };
    let published = Arc::new(RwLock::new(Arc::new(Snapshot {
        status: status.clone(),
        boot_id: boot_id.clone(),
        capabilities,
        processes: Vec::new(),
        changes: ProcessChanges::default(),
        pressure: None,
        attribution: AttributionSnapshot::default(),
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
    })));
    let (send, requests) = mpsc::sync_channel(128);
    let mut socket_server = Some((server, send));
    let heartbeat = Arc::new(AtomicU64::new(0));
    start_watchdog(Arc::clone(&heartbeat))?;
    tick_qos()?;
    let mut observer = Observer::default();
    let mut attributor = Attributor::new(config.markers.clone(), config.shells.clone());
    let mut admission = crate::hooks::Admission::new(&config.heavy_commands);
    let mut hook_state = crate::hooks::HookState::default();
    let mut next_tick = Instant::now();
    let mut observation_valid = false;
    loop {
        if Instant::now() >= next_tick {
            let started = Instant::now();
            let cpu = thread_cpu_ns()?;
            let discard = observer.begin_tick(started, SystemTime::now());
            status.tick += 1;
            status.sampled_at_ms = unix_ms();
            status.tick_interval_ms = observer.interval().as_millis() as u64;
            status.sample_discarded = discard;
            status.last_error = None;
            let mut frozen_watched = guardian.watched();
            frozen_watched.extend(&extra_watched);
            frozen_watched.extend(cleanup.watched());
            let watched = attributor.watched(&frozen_watched);
            let mut metric_targets = attributor.metric_targets();
            metric_targets.extend(&extra_metrics);
            let observed = platform.list_processes(&watched, &metric_targets);
            let pressure = platform.pressure();
            let snapshot = match (observed, pressure) {
                (Ok(processes), pressure) => {
                    let changes = observer.diff(&processes);
                    status.process_count = processes.len();
                    let pressure = match pressure {
                        Ok(pressure) if !discard => Some(pressure),
                        Ok(_) => None,
                        Err(error) => {
                            status.last_error = Some(error.to_string());
                            None
                        }
                    };
                    Some((processes, changes, pressure))
                }
                (Err(error), _) => {
                    status.last_error = Some(error.to_string());
                    None
                }
            };
            if let Some(error) = &status.last_error {
                status.sample_discarded = true;
                if let Err(write_error) =
                    log.write_line(&format!("{} observation failed: {error}", unix_ms()))
                {
                    eprintln!("daemon log failed: {write_error}");
                }
            }
            observation_valid = snapshot.is_some();
            let mut next = if let Some((mut processes, changes, pressure)) = snapshot {
                let attribution = attributor.update(
                    &platform,
                    &mut processes,
                    started,
                    status.sampled_at_ms,
                    status.sample_discarded,
                );
                Snapshot {
                    status: status.clone(),
                    boot_id: boot_id.clone(),
                    capabilities,
                    processes,
                    changes,
                    pressure,
                    attribution,
                    frozen: Vec::new(),
                    held: Vec::new(),
                    guardian: None,
                }
            } else {
                attributor.reset_growth();
                // A failed scan must not manufacture exits or reset attribution.
                let previous = published.read().unwrap();
                Snapshot {
                    status: status.clone(),
                    boot_id: boot_id.clone(),
                    capabilities,
                    processes: previous.processes.clone(),
                    attribution: previous.attribution.clone(),
                    changes: ProcessChanges::default(),
                    pressure: None,
                    frozen: Vec::new(),
                    held: Vec::new(),
                    guardian: None,
                }
            };
            hook_state.apply(&mut next.attribution, started);
            if observation_valid {
                cleanup.tick(started, &next, &mut guardian, &platform, &mut decisions);
            }
            let result = guardian.tick(
                started,
                &next,
                &mut platform,
                &mut attributor,
                &mut decisions,
            );
            let mut errors: Vec<_> = guardian
                .take_errors()
                .into_iter()
                .map(|e| format!("guardian: {e}"))
                .collect();
            errors.extend(cleanup.take_errors());
            if let Err(error) = result {
                errors.push(format!("guardian: {error}"));
            }
            for error in errors {
                next.status.last_error.get_or_insert_with(|| error.clone());
                if let Err(write_error) = log.write_line(&format!("{} {error}", unix_ms())) {
                    eprintln!("{error}; daemon log failed: {write_error}");
                }
            }
            observer.set_fast_polling(guardian.level != Level::Normal);
            next.status.tick_interval_ms = observer.interval().as_millis() as u64;
            next.status.pressure_level = guardian.level;
            next.status.batch_running = guardian.batch_running(&next.attribution, &next.processes);
            next.status.cleanup_pending = cleanup.pending_targets();
            next.frozen = guardian.frozen.clone();
            admission.tick(&next, &mut hook_state, &mut decisions, started);
            next.held = admission.held();
            next.guardian = Some(guardian.note.clone());
            {
                let mut view = published.write().unwrap();
                *view = Arc::new(next);
                // Include snapshot construction and retirement before publishing timing.
                let next = Arc::get_mut(&mut view).expect("new snapshot has no readers yet");
                next.status.tick_cpu_ns = thread_cpu_ns()?.saturating_sub(cpu);
                next.status.tick_wall_ns = started.elapsed().as_nanos() as u64;
            }
            heartbeat.fetch_add(1, Ordering::Relaxed);
            if let Some((server, send)) = socket_server.take() {
                let view = Arc::clone(&published);
                thread::Builder::new().name("ipc".into()).spawn(move || {
                    if let Err(error) = server.run(view, send) {
                        eprintln!("daemon socket server failed: {error}");
                        std::process::exit(1);
                    }
                })?;
            }
            next_tick = started + observer.interval();
        }
        match requests.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(request) => {
                if !request.cancelled.load(Ordering::Relaxed) {
                    if let ipc::Method::Hook { payload } = &request.method {
                        match serde_json::from_value::<crate::hooks::HookRequest>(payload.clone()) {
                            Ok(hook) if !hook.session_id.is_empty() => {
                                if hook.event == crate::hooks::Event::SessionEnd {
                                    next_tick = Instant::now();
                                }
                                let snapshot = Arc::clone(&published.read().unwrap());
                                admission.handle(
                                    hook,
                                    request,
                                    &snapshot,
                                    &mut hook_state,
                                    &mut decisions,
                                    Instant::now(),
                                );
                            }
                            _ => {
                                let _ = request
                                    .reply
                                    .send(ipc::Response::error("invalid hook request"));
                            }
                        }
                        continue;
                    }
                    let response = match request.method {
                        ipc::Method::Resume { target } => match guardian.resume(
                            target.as_deref(),
                            Instant::now(),
                            &platform,
                            &mut decisions,
                        ) {
                            Ok(count) => {
                                next_tick = Instant::now();
                                ipc::Response::new(ipc::Reply::Resumed { count })
                            }
                            Err(e) => ipc::Response::error(e.to_string()),
                        },
                        ipc::Method::Gc | ipc::Method::Stop { .. } if !observation_valid => {
                            ipc::Response::error(
                                "process observation unavailable; retry after a successful scan",
                            )
                        }
                        method @ (ipc::Method::Gc | ipc::Method::Stop { .. }) => {
                            let view = published.read().unwrap().clone();
                            let target = match &method {
                                ipc::Method::Stop { target } => Some(target.as_str()),
                                _ => None,
                            };
                            match cleanup.request(
                                target,
                                &view,
                                &mut guardian,
                                &platform,
                                &mut decisions,
                            ) {
                                Ok(report) => {
                                    next_tick = Instant::now();
                                    ipc::Response::new(ipc::Reply::Cleanup { report })
                                }
                                Err(e) => ipc::Response::error(e.to_string()),
                            }
                        }
                        _ => ipc::Response::error("request is not implemented yet"),
                    };
                    let _ = request.reply.send(response);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("socket server stopped"));
            }
        }
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn thread_cpu_ns() -> io::Result<u64> {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}
fn tick_qos() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let result = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        let mut actual = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
        let mut priority = 0;
        let result = unsafe {
            libc::pthread_get_qos_class_np(libc::pthread_self(), &mut actual, &mut priority)
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        if actual as u32 != libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE as u32 {
            return Err(io::Error::other(
                "tick thread did not acquire user-interactive QoS",
            ));
        }
    }
    Ok(())
}
fn start_watchdog(heartbeat: Arc<AtomicU64>) -> io::Result<()> {
    thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            let mut observed = 0;
            let mut progress = Instant::now();
            let mut previous_wake = (Instant::now(), SystemTime::now());
            loop {
                thread::sleep(Duration::from_secs(1));
                let now = Instant::now();
                let wall = SystemTime::now();
                let tick = heartbeat.load(Ordering::Relaxed);
                // A sleeping machine cannot make progress. Give the loop a fresh grace period on wake.
                if tick != observed
                    || now.duration_since(previous_wake.0) > Duration::from_secs(5)
                    || wall
                        .duration_since(previous_wake.1)
                        .map_or(true, |gap| gap > Duration::from_secs(5))
                {
                    observed = tick;
                    progress = now;
                }
                previous_wake = (now, wall);
                if now.duration_since(progress) >= Duration::from_secs(30) {
                    eprintln!("daemon watchdog: tick stalled for 30 seconds");
                    std::process::exit(1);
                }
            }
        })?;
    Ok(())
}

pub fn warn_if_stranded() {
    let Ok(paths) = Paths::from_env() else {
        return;
    };
    if recovery::needs_recovery(&paths)
        && ipc::Client::connect(&paths, Duration::from_millis(250))
            .and_then(|mut client| client.request(ipc::Method::Status))
            .is_err()
    {
        eprintln!(
            "WARNING: Ballast has frozen work and the daemon is unreachable. Run `ballast resume --all` to recover it."
        );
    }
}

pub fn resume_command(paths: &Paths, target: Option<&str>) -> io::Result<usize> {
    if let Ok(mut client) = ipc::Client::connect(paths, Duration::from_secs(1)) {
        if let Ok(response) = client.request(ipc::Method::Resume {
            target: target.map(str::to_owned),
        }) {
            return match response.reply {
                ipc::Reply::Resumed { count } => Ok(count),
                ipc::Reply::Error { message } => Err(io::Error::other(message)),
                _ => Err(io::Error::other("unexpected resume response")),
            };
        }
    }
    // The directory lock arbitrates a concurrent or slow daemon.
    paths.prepare()?;
    let _lock = ipc::lock(paths)?;
    let config = Config::load(paths).unwrap_or_default();
    let mut platform = NativePlatform::new()?;
    let mut log = RotatingLog::open(paths.base.join("log/decisions.jsonl"), &config)?;
    if target.is_none() {
        return recovery::recover(paths, &mut platform, &config.markers, &mut log);
    }
    let boot_id = platform.boot_id()?;
    let saved = recovery::read(paths)?;
    let mut guardian = Guardian::new(
        paths.clone(),
        boot_id.clone(),
        Mode::Enforce,
        config.pressure,
    );
    if saved.boot_id == boot_id {
        guardian.frozen = saved.workloads;
    }
    let result = guardian.resume(target, Instant::now(), &platform, &mut log);
    for error in guardian.take_errors() {
        eprintln!("guardian: {error}");
    }
    result
}
