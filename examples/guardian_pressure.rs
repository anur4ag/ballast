//! Bounded native-pressure probe. Only descendants started here enter attribution.
//! Run with a fresh BALLAST_HOME and BALLAST_OWNER, and a memory-worker executable argument.
use ballast::attribution::Attributor;
use ballast::daemon::{
    Observer, ProcessChanges, Snapshot, Status,
    files::{Config, Mode, Paths, RotatingLog},
};
use ballast::guardian::{Guardian, Level};
use ballast::platform::*;
use std::collections::HashSet;
use std::io;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

struct OwnedPlatform {
    native: NativePlatform,
    root: i32,
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
        let mut all = self.native.list_processes(watched, metrics)?;
        let mut pids = HashSet::from([self.root]);
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
        if id.pid == self.root || !self.owned.contains(&id) {
            return Err(io::Error::other("probe rejected non-worker signal"));
        }
        self.native.send_signal(id, signal)
    }
    fn notify(&self, _title: &str, _body: &str) -> io::Result<bool> {
        Ok(false)
    }
}
struct Workers(Vec<Child>);
impl Drop for Workers {
    fn drop(&mut self) {
        for child in &mut self.0 {
            // These private process groups were created by this probe, never user groups.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGCONT);
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
        }
        for child in &mut self.0 {
            let _ = child.wait();
        }
    }
}
fn cpu_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t);
    }
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    let worker = std::env::args()
        .nth(1)
        .ok_or("supply a bounded memory-worker executable")?;
    let paths = Paths::from_env()?;
    paths.prepare()?;
    let config = Config::load(&paths)?;
    let mut log = RotatingLog::open(paths.base.join("log/decisions.jsonl"), &config)?;
    let mut platform = OwnedPlatform {
        native: NativePlatform::new()?,
        root: std::process::id() as i32,
        owned: HashSet::new(),
    };
    let boot_id = platform.boot_id()?;
    let mut guardian = Guardian::new(paths, boot_id.clone(), Mode::Enforce, config.pressure);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut workers = Workers(Vec::new());
    for _ in 0..4 {
        workers.0.push(
            Command::new("/bin/sh")
                .args(["-c", "sleep 35; exec \"$1\"", "probe", &worker])
                .process_group(0)
                .spawn()?,
        );
    }
    println!(
        "{}",
        serde_json::json!({"owned_worker_groups": workers.0.iter().map(Child::id).collect::<Vec<_>>()})
    );
    let began = Instant::now();
    let mut observer = Observer::default();
    let mut max_level = Level::Normal;
    let mut freezes = 0;
    let mut previous_frozen = HashSet::new();
    let mut critical_since = None;
    let mut samples = Vec::new();
    let mut ticks = 0;
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        while began.elapsed() < Duration::from_secs(135) {
            let now = Instant::now();
            let cpu = cpu_ns();
            let discard = observer.begin_tick(now, SystemTime::now());
            let mut processes = platform.list_processes(
                &attributor.watched(&guardian.watched()),
                &attributor.metric_targets(),
            )?;
            let raw = platform.pressure()?;
            let pressure = (!discard).then_some(raw.clone());
            let attribution = attributor.update(
                &platform,
                &mut processes,
                now,
                ballast::daemon::unix_ms(),
                discard,
            );
            let snapshot = Snapshot {
                status: Status {
                    daemon_version: "probe".into(),
                    pid: std::process::id(),
                    mode: Mode::Enforce,
                    tick: ticks,
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
                pressure,
                attribution,
                frozen: Vec::new(),
                held: Vec::new(),
                guardian: None,
            };
            guardian.tick(now, &snapshot, &mut platform, &mut attributor, &mut log)?;
            max_level = max_level.max(guardian.level);
            let frozen: HashSet<_> = guardian
                .frozen
                .iter()
                .map(|w| w.workload_id.clone())
                .collect();
            freezes += frozen.difference(&previous_frozen).count();
            previous_frozen = frozen;
            samples.push(cpu_ns().saturating_sub(cpu));
            ticks += 1;
            println!(
                "{}",
                serde_json::json!({"elapsed_s": began.elapsed().as_secs_f64(), "level": guardian.level,
                "kernel": raw.kernel_pressure_level, "swapouts": raw.swapouts, "pageouts": raw.pageouts,
                "page_size": raw.page_size, "used_bytes": raw.used_memory_bytes,
                "agent_bytes": snapshot.attribution.agents.iter().map(|a| a.memory.bytes).sum::<u64>(),
                "frozen": guardian.frozen.len(), "freezes": freezes, "cpu_ns": samples.last()})
            );
            if raw.kernel_pressure_level == Some(4) {
                let since = critical_since.get_or_insert(now);
                if now.duration_since(*since) >= Duration::from_secs(12) {
                    break;
                }
            } else {
                critical_since = None;
            }
            if freezes >= 2 {
                break;
            }
            observer.set_fast_polling(guardian.level != Level::Normal);
            std::thread::sleep(observer.interval().saturating_sub(now.elapsed()));
        }
        Ok(())
    })();
    let resumed = guardian.resume(None, Instant::now(), &platform, &mut log)?;
    let stopped_after_resume = platform
        .list_processes(&platform.owned.clone(), &HashSet::new())?
        .iter()
        .filter(|p| p.stopped)
        .count();
    drop(workers);
    samples.sort_unstable();
    println!(
        "{}",
        serde_json::json!({"summary": true, "max_level": max_level, "freezes": freezes,
        "resumed_workloads": resumed, "stopped_after_resume": stopped_after_resume,
        "median_cpu_ms": samples[samples.len()/2] as f64/1e6, "ticks": ticks})
    );
    result?;
    if freezes == 0 {
        return Err("bounded run did not establish guardian freezes under native pressure".into());
    }
    if stopped_after_resume != 0 {
        return Err("a probe process remained stopped".into());
    }
    Ok(())
}
