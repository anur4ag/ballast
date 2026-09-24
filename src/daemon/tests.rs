//! Unit test for the watchdog (ticket 03): a stalled heartbeat must exit the
//! process around 30s. Runs the check in a re-exec'd child so the exit
//! itself (`std::process::exit(1)`) doesn't take down the test harness.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// A concurrent fork can retain another test's flock until exec closes its inherited fd.
pub(super) static SPAWN_LOCK: Mutex<()> = Mutex::new(());
fn with_spawn_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn watchdog_exits_the_process_after_a_stalled_heartbeat() {
    let exe = std::env::current_exe().expect("current_exe for watchdog fixture re-exec");
    let mut guard = ChildGuard(with_spawn_lock(|| {
        Command::new(exe)
            .args([
                "daemon::tests::watchdog_stalled_heartbeat_fixture",
                "--exact",
                "--ignored",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn watchdog fixture")
    }));

    let started = Instant::now();
    let deadline = started + Duration::from_secs(40);
    let status = loop {
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "watchdog did not exit within 40s of a stalled heartbeat"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    assert!(
        started.elapsed() >= Duration::from_secs(25),
        "watchdog exited suspiciously early ({:?}), not because of the 30s stall check",
        started.elapsed()
    );
    assert_eq!(
        status.code(),
        Some(1),
        "a stalled heartbeat must exit the process with code 1"
    );
}

/// Only reached via the re-exec above: starts the watchdog against a
/// heartbeat that never advances, then blocks forever so the watchdog's own
/// `std::process::exit(1)` is what ends this process.
#[test]
#[ignore]
fn watchdog_stalled_heartbeat_fixture() {
    super::start_watchdog(Arc::new(AtomicU64::new(0))).expect("start_watchdog");
    std::thread::sleep(Duration::from_secs(120));
}

/// Paced real-daemon benchmark at ~1000 total processes, once with an empty
/// watch set and once watching 100 real filler identities. Not run by
/// default; execute explicitly and alone, e.g.:
/// `cargo test --lib --release -- --ignored --nocapture daemon_benchmark_at_about_1000_processes`
const BENCH_WATCH_ENV: &str = "BALLAST_BENCH_WATCH";

/// A short-lived `/tmp` `BALLAST_HOME`, removed on drop.
struct BenchHome(std::path::PathBuf);
impl BenchHome {
    fn new(tag: &str) -> Self {
        let path = std::path::PathBuf::from(format!("/tmp/blt-bench-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create bench BALLAST_HOME");
        BenchHome(path)
    }
}
impl Drop for BenchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore]
fn daemon_benchmark_at_about_1000_processes() {
    use crate::platform::{NativePlatform, Platform, ProcessIdentity};
    use std::collections::HashSet;

    let target = 1000usize;
    let baseline = NativePlatform::new()
        .expect("NativePlatform::new")
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("baseline list_processes")
        .len();
    let mut fillers = Vec::with_capacity(target.saturating_sub(baseline));
    for _ in 0..target.saturating_sub(baseline) {
        match Command::new("sleep")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => fillers.push(ChildGuard(child)),
            Err(e) => {
                println!("stopped spawning fillers after {} ({e})", fillers.len());
                break;
            }
        }
    }
    println!("spawned {} filler processes toward {target}", fillers.len());

    let filler_pids: HashSet<i32> = fillers.iter().map(|f| f.0.id() as i32).collect();
    let watched_100: Vec<ProcessIdentity> = NativePlatform::new()
        .expect("NativePlatform::new")
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("list_processes for filler identities")
        .into_iter()
        .filter(|p| filler_pids.contains(&p.identity.pid))
        .take(100)
        .map(|p| p.identity)
        .collect();
    assert_eq!(
        watched_100.len(),
        100,
        "must select exactly 100 filler identities, not silently benchmark a partial watched set"
    );

    run_benchmark_pass("empty watched set", &[]);
    run_benchmark_pass("100 watched", &watched_100);
}

fn run_benchmark_pass(label: &str, watched: &[crate::platform::ProcessIdentity]) {
    let home = BenchHome::new(if watched.is_empty() {
        "empty"
    } else {
        "watched100"
    });
    let exe = std::env::current_exe().expect("current_exe for benchmark fixture re-exec");
    let daemon = ChildGuard(with_spawn_lock(|| {
        Command::new(exe)
            .args([
                "daemon::tests::daemon_benchmark_fixture",
                "--exact",
                "--ignored",
            ])
            .env("BALLAST_HOME", &home.0)
            .env(
                BENCH_WATCH_ENV,
                serde_json::to_string(watched).expect("serialize watched identities"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon benchmark fixture")
    }));

    let paths = super::files::Paths {
        base: home.0.clone(),
    };
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        match super::ipc::Client::connect(&paths, Duration::from_secs(1)) {
            Ok(client) => break client,
            Err(_) if Instant::now() < connect_deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("{label}: daemon fixture did not become reachable: {e}"),
        }
    };

    let mut ticks_seen = std::collections::BTreeSet::new();
    let mut cpu_ns_samples = Vec::new();
    let mut process_count = 0usize;
    let sample_deadline = Instant::now() + Duration::from_secs(120);
    while ticks_seen.len() < 20 {
        assert!(
            Instant::now() < sample_deadline,
            "{label}: did not observe 20 unique ticks within 120s"
        );
        let response = client
            .request(super::ipc::Method::Status)
            .expect("status request");
        if let super::ipc::Reply::Status { status } = response.reply {
            if status.tick > 1 && ticks_seen.insert(status.tick) {
                cpu_ns_samples.push(status.tick_cpu_ns);
                process_count = status.process_count;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut sorted = cpu_ns_samples.clone();
    sorted.sort_unstable();
    let mean_ns = sorted.iter().sum::<u64>() / sorted.len() as u64;
    println!(
        "{label}: {} warmed ticks at {process_count} processes: median {:.3} ms, mean {:.3} ms, max {:.3} ms",
        sorted.len(),
        sorted[sorted.len() / 2] as f64 / 1_000_000.0,
        mean_ns as f64 / 1_000_000.0,
        sorted.last().copied().unwrap_or(0) as f64 / 1_000_000.0,
    );

    let ps = Command::new("ps")
        .args(["-o", "rss=", "-p", &daemon.0.id().to_string()])
        .output()
        .expect("run ps");
    let rss_kb: u64 = String::from_utf8_lossy(&ps.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{label}: could not parse `ps -o rss=` output: {e}"));
    println!(
        "{label}: daemon RSS via ps -o rss=: {:.2} MB",
        rss_kb as f64 / 1024.0
    );
}

/// Only reached via re-exec from `run_benchmark_pass`: runs the real daemon
/// loop against a real `BALLAST_HOME` and watch set, until killed.
#[test]
#[ignore]
fn daemon_benchmark_fixture() {
    use crate::platform::ProcessIdentity;
    use std::collections::HashSet;

    let paths = super::files::Paths::from_env().expect("BALLAST_HOME must be set");
    let watched: HashSet<ProcessIdentity> = std::env::var(BENCH_WATCH_ENV)
        .ok()
        .and_then(|json| serde_json::from_str::<Vec<ProcessIdentity>>(&json).ok())
        .unwrap_or_default()
        .into_iter()
        .collect();
    super::run_with_targets(paths, watched, HashSet::new()).expect("run_with_targets");
}
