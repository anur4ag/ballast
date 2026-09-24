//! Native-process coverage for attribution (ticket 04), complementing the synthetic
//! `FakePlatform` unit tests in `src/attribution/tests.rs`: one real process tree through a
//! copy of this binary renamed `claude` (binary recognition requires the literal name), and an
//! ignored fleet benchmark measuring real daemon CPU cost under many attributed and
//! unattributed processes.
//!
//! Fleet fillers are spawned by a detached relay (env cleared, reparented to pid 1 before the
//! daemon starts) so none of them inherit this test's own real agent ancestry. Everything the
//! relay spawns stays in its own process group, so one `killpg` on that group id cleans up the
//! whole fleet regardless of reparenting.

use ballast::attribution::Attributor;
use ballast::daemon::files::Paths;
use ballast::daemon::ipc::{Client, Method, Reply};
use ballast::platform::{NativePlatform, Platform};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};

const MODE_ENV: &str = "BALLAST_FLEET_TEST_MODE";
const HELPER_EXE_ENV: &str = "BALLAST_FLEET_HELPER_EXE";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_LIFETIME_CAP: Duration = Duration::from_secs(180);

/// Kills an entire process group on drop, so a killed root's grandchildren (spawned without
/// their own `process_group`, and so sharing its pgid) are reclaimed too, even on panic.
struct PgidGuard(i32);
impl Drop for PgidGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_ready_line(stdout: ChildStdout) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    let trimmed = line.trim_end().to_string();
                    if trimmed.starts_with("READY") {
                        let _ = tx.send(trimmed);
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    rx.recv_timeout(READY_TIMEOUT)
        .unwrap_or_else(|e| panic!("fleet fixture did not report ready in time: {e}"))
}
fn wait_until(what: &str, deadline: Duration, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + deadline;
    loop {
        if f() {
            return;
        }
        assert!(Instant::now() < until, "timed out waiting for: {what}");
        sleep(Duration::from_millis(20));
    }
}

/// A short-lived scratch directory, removed on drop.
struct ScratchDir(PathBuf);
impl ScratchDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "blt-fleet-{tag}-{}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        ScratchDir(path)
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copies this test binary to `dir/name` so it can be spawned under a chosen basename: a real
/// file, not a symlink, since `/proc/<pid>/exe` resolves a symlink back to its original target
/// but reports a hardlink or copy's own path as-is.
fn copy_self_as(dir: &std::path::Path, name: &str) -> PathBuf {
    let target = dir.join(name);
    let exe = std::env::current_exe().expect("current_exe to copy for fixture rename");
    std::fs::copy(&exe, &target).expect("copy test binary for fixture rename");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("chmod copied fixture");
    target
}

// ---------------------------------------------------------------------
// One real process tree: a claude-named root, a tool-call shell workload, an internal child.
// ---------------------------------------------------------------------

#[test]
fn a_real_claude_named_process_tree_is_attributed_end_to_end() {
    let scratch = ScratchDir::new("tree");
    let claude_bin = copy_self_as(&scratch.0, "claude");

    let mut root = Command::new(&claude_bin)
        .args(["tree_root_fixture", "--exact", "--ignored", "--nocapture"])
        .env(MODE_ENV, "tree_root")
        .env(
            HELPER_EXE_ENV,
            std::env::current_exe().expect("current_exe"),
        )
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn claude-named root fixture");
    let root_pid = root.id() as i32;
    let _group_guard = PgidGuard(root_pid); // covers root + shell + internal child, panics included
    let stdout = root.stdout.take().expect("piped stdout");
    let ready = read_ready_line(stdout);
    let mut parts = ready
        .strip_prefix("READY:")
        .expect("root must report child pids")
        .split(',');
    let shell_pid: i32 = parts.next().unwrap().parse().unwrap();
    let internal_pid: i32 = parts.next().unwrap().parse().unwrap();

    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let mut root_identity = None;
    let mut shell_identity = None;
    let mut internal_identity = None;
    wait_until(
        "root, shell workload and internal child to appear",
        Duration::from_secs(10),
        || {
            let processes = platform
                .list_processes(&Default::default(), &Default::default())
                .unwrap_or_default();
            root_identity = processes
                .iter()
                .find(|p| p.identity.pid == root_pid)
                .map(|p| p.identity);
            shell_identity = processes
                .iter()
                .find(|p| p.identity.pid == shell_pid)
                .map(|p| p.identity);
            internal_identity = processes
                .iter()
                .find(|p| p.identity.pid == internal_pid)
                .map(|p| p.identity);
            root_identity.is_some() && shell_identity.is_some() && internal_identity.is_some()
        },
    );

    let watched = std::collections::HashSet::from([
        root_identity.unwrap(),
        shell_identity.unwrap(),
        internal_identity.unwrap(),
    ]);
    let mut attributor = Attributor::new(Vec::new(), Vec::new());
    let mut processes = platform
        .list_processes(&watched, &Default::default())
        .expect("list_processes");
    let snapshot = attributor.update(&platform, &mut processes, Instant::now(), 0, false);
    let find = |id| {
        snapshot
            .processes
            .iter()
            .find(|p| p.identity == id)
            .unwrap()
    };

    let root_attr = find(root_identity.unwrap());
    assert_eq!(
        root_attr.role,
        ballast::attribution::ProcessRole::AgentRoot,
        "a process literally named `claude` must be recognized as an agent root"
    );
    let agent = snapshot
        .agents
        .iter()
        .find(|a| Some(a.id.as_str()) == root_attr.agent_id.as_deref())
        .unwrap();
    assert_eq!(agent.kind, "claude");

    let shell_attr = find(shell_identity.unwrap());
    assert_eq!(
        shell_attr.role,
        ballast::attribution::ProcessRole::Workload,
        "a real `sh -c` child of the claude root must be a workload"
    );

    let internal_attr = find(internal_identity.unwrap());
    assert_eq!(
        internal_attr.role,
        ballast::attribution::ProcessRole::AgentInternal,
        "a real plain child of the claude root must be agent-internal"
    );

    root.kill().ok();
    root.wait().ok();
}

/// Only reached via re-exec: spawns a real `/bin/sh -c` workload child and a real plain
/// internal child, reports both pids, then sleeps so the parent can observe the tree.
#[test]
#[ignore]
fn tree_root_fixture() {
    let mut stdout = std::io::stdout();
    // A trailing no-op keeps the shell alive as a real process instead of it tail-call
    // exec'ing directly into `sleep` (some `sh` implementations elide the fork for a single
    // simple command), which would erase the `-c` argv the tool-call-shell check depends on.
    let shell = Command::new("/bin/sh")
        .args(["-c", "sleep 60; :"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn workload shell child");
    // Not `current_exe()`: this process is itself running from the "claude"-named copy, and
    // re-invoking that same path would make the internal child look like a nested claude root.
    let exe = std::env::var(HELPER_EXE_ENV).expect("helper exe path");
    let internal = Command::new(exe)
        .args(["idle_fixture", "--exact", "--ignored", "--nocapture"])
        .env(MODE_ENV, "idle")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn internal child");
    writeln!(stdout, "READY:{},{}", shell.id(), internal.id()).unwrap();
    stdout.flush().unwrap();
    std::mem::forget(shell);
    std::mem::forget(internal);
    sleep(CHILD_LIFETIME_CAP);
}

#[test]
#[ignore]
fn idle_fixture() {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").unwrap();
    stdout.flush().unwrap();
    sleep(CHILD_LIFETIME_CAP);
}

// ---------------------------------------------------------------------
// Fleet benchmark: ~10 owner-marked root trees (~110 attributed processes) plus enough plain
// `sleep` noise to reach ~1000 total, observed by a real daemon. Not run by default:
// `cargo test --release --test attribution_fleet -- --ignored --nocapture attributed_fleet_benchmark`
// ---------------------------------------------------------------------

const ROOT_COUNT_ENV: &str = "BALLAST_FLEET_ROOT_COUNT";
const NOISE_COUNT_ENV: &str = "BALLAST_FLEET_NOISE_COUNT";
const RELAY_TAG_ENV: &str = "BALLAST_FLEET_RELAY_TAG";
const TARGET_TOTAL_PROCESSES: usize = 1000;

#[test]
#[ignore]
fn attributed_fleet_benchmark_at_scale() {
    let root_count: usize = std::env::var(ROOT_COUNT_ENV)
        .map(|value| value.parse().expect("root count must be numeric"))
        .unwrap_or(10);
    assert!((1..=50).contains(&root_count));
    let scratch = ScratchDir::new("bench");
    let worker_bin = copy_self_as(&scratch.0, "blt-fleet-worker");
    let tag = format!("fleet-{}", std::process::id());

    let baseline = NativePlatform::new()
        .expect("NativePlatform::new")
        .list_processes(&Default::default(), &Default::default())
        .expect("baseline list_processes")
        .len();
    let noise_count = TARGET_TOTAL_PROCESSES
        .saturating_sub(baseline)
        .saturating_sub(root_count * 11)
        .min(900);

    let mut relay = Command::new(&worker_bin)
        .args(["relay_fixture", "--exact", "--ignored", "--nocapture"])
        .env_clear()
        .env(MODE_ENV, "relay")
        .env(ROOT_COUNT_ENV, root_count.to_string())
        .env(NOISE_COUNT_ENV, noise_count.to_string())
        .env(RELAY_TAG_ENV, &tag)
        .env(HELPER_EXE_ENV, &worker_bin)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fleet relay");
    let relay_pid = relay.id() as i32;
    // Everything the relay spawns (roots, their shells/workers/internals, and noise) stays in
    // this one process group even after being reparented to pid 1, so one killpg reclaims the
    // whole fleet regardless of what the rest of this test does or panics on.
    let _group_guard = PgidGuard(relay_pid);
    let relay_stdout = relay.stdout.take().expect("piped stdout");
    let ready = read_ready_line(relay_stdout);
    let root_pids: Vec<i32> = ready
        .strip_prefix("READY:")
        .expect("relay must report root pids")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().expect("root pid must be numeric"))
        .collect();
    assert_eq!(
        root_pids.len(),
        root_count,
        "relay must have spawned every requested root"
    );
    let status = relay.wait().expect("wait on relay");
    assert!(
        status.success(),
        "relay must exit cleanly after spawning its fleet"
    );
    println!(
        "spawned {root_count} owner-marked root trees and {noise_count} unattributed noise fillers (baseline {baseline})"
    );

    let home = ScratchDir::new("daemon-home");
    let daemon = ChildGuard(
        Command::new(std::env::current_exe().expect("current_exe"))
            .args(["daemon_fixture", "--exact", "--ignored", "--nocapture"])
            .env("BALLAST_HOME", &home.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon fixture"),
    );

    let paths = Paths {
        base: home.0.clone(),
    };
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        match Client::connect(&paths, Duration::from_secs(1)) {
            Ok(client) => break client,
            Err(_) if Instant::now() < connect_deadline => sleep(Duration::from_millis(50)),
            Err(e) => panic!("daemon fixture did not become reachable: {e}"),
        }
    };

    let mut ticks_seen = std::collections::BTreeSet::new();
    let mut cpu_ns_samples = Vec::new();
    let mut port_round_samples = Vec::new();
    let mut last_port_sample = None;
    let sample_deadline = Instant::now() + Duration::from_secs(120);
    while ticks_seen.len() < 20 {
        assert!(
            Instant::now() < sample_deadline,
            "did not observe 20 unique ticks within 120s"
        );
        let response = client.request(Method::Status).expect("status request");
        if let Reply::Status { status } = response.reply {
            if status.tick > 1 && ticks_seen.contains(&status.tick) {
                sleep(Duration::from_millis(50));
                continue;
            }
            let reply = client.request(Method::Snapshot).expect("round snapshot");
            if let Reply::Snapshot { snapshot } = reply.reply {
                let sampled = snapshot
                    .attribution
                    .processes
                    .iter()
                    .filter_map(|p| p.ports_sampled_at_ms)
                    .max();
                if snapshot.status.tick > 1 && ticks_seen.insert(snapshot.status.tick) {
                    cpu_ns_samples.push(snapshot.status.tick_cpu_ns);
                    if sampled != last_port_sample {
                        port_round_samples.push(snapshot.status.tick_cpu_ns);
                    }
                }
                last_port_sample = sampled;
            }
        }
        sleep(Duration::from_millis(50));
    }

    let response = client.request(Method::Snapshot).expect("snapshot request");
    let Reply::Snapshot { snapshot } = response.reply else {
        panic!("expected a snapshot reply");
    };
    let owned_agent_ids: std::collections::HashSet<&str> = snapshot
        .attribution
        .agents
        .iter()
        .filter(|a| a.owner_id.as_deref().is_some_and(|id| id.starts_with(&tag)))
        .map(|a| a.id.as_str())
        .collect();
    let attributed_total = snapshot
        .attribution
        .processes
        .iter()
        .filter(|p| p.agent_id.is_some())
        .count();
    let attributed_owned = snapshot
        .attribution
        .processes
        .iter()
        .filter(|p| {
            p.agent_id
                .as_deref()
                .is_some_and(|id| owned_agent_ids.contains(id))
        })
        .count();

    let mut sorted = cpu_ns_samples.clone();
    sorted.sort_unstable();
    let mean_ns = sorted.iter().sum::<u64>() / sorted.len() as u64;
    println!(
        "fleet benchmark: {} warmed ticks at {} processes, {} attributed processes total ({} owned by this benchmark run): \
         median {:.3} ms, mean {:.3} ms, max {:.3} ms CPU/tick",
        sorted.len(),
        snapshot.status.process_count,
        attributed_total,
        attributed_owned,
        sorted[sorted.len() / 2] as f64 / 1_000_000.0,
        mean_ns as f64 / 1_000_000.0,
        sorted.last().copied().unwrap_or(0) as f64 / 1_000_000.0,
    );
    println!(
        "port round ticks: {}, max {:.3} ms CPU/tick",
        port_round_samples.len(),
        port_round_samples.iter().max().copied().unwrap_or(0) as f64 / 1_000_000.0
    );
    let rss = Command::new("ps")
        .args(["-o", "rss=", "-p", &daemon.0.id().to_string()])
        .output()
        .expect("daemon RSS");
    let rss_kb: u64 = String::from_utf8_lossy(&rss.stdout)
        .trim()
        .parse()
        .expect("RSS in KiB");
    println!("fleet daemon RSS: {:.2} MiB", rss_kb as f64 / 1024.0);
    assert!(
        attributed_owned >= root_count * 11,
        "expected all benchmark-owned attributed processes, got {attributed_owned} (of {attributed_total} total attributed)"
    );

    // Noise guarantee: every fixture process (roots, their descendants, and noise fillers)
    // shares the relay's own process group, reparenting aside. An attributed process sharing
    // that group but not owned by this run would mean noise got swept into someone else's
    // agent -- exactly what a shared-reaper leak would produce.
    let raw_pgid: std::collections::HashMap<_, _> = snapshot
        .processes
        .iter()
        .map(|p| (p.identity, p.pgid))
        .collect();
    let relay_group_unowned: Vec<_> = snapshot
        .attribution
        .processes
        .iter()
        .filter(|p| p.agent_id.is_some())
        .filter(|p| raw_pgid.get(&p.identity) == Some(&relay_pid))
        .filter(|p| {
            !p.agent_id
                .as_deref()
                .is_some_and(|id| owned_agent_ids.contains(id))
        })
        .map(|p| p.identity)
        .collect();
    assert!(
        relay_group_unowned.is_empty(),
        "every attributed process sharing the relay's process group must belong to this benchmark's owned agents; unowned: {relay_group_unowned:?}"
    );
}

/// Only reached via re-exec, with a scrubbed environment: spawns `ROOT_COUNT_ENV` owner-marked
/// root trees and `NOISE_COUNT_ENV` plain `sleep` fillers, all left in this process's own group,
/// reports the root pids, then exits immediately so everything is reparented to pid 1 before
/// the daemon starts (breaking any inherited ancestry from this test's own real agent).
#[test]
#[ignore]
fn relay_fixture() {
    let root_count: usize = std::env::var(ROOT_COUNT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .expect("root count");
    let noise_count: usize = std::env::var(NOISE_COUNT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .expect("noise count");
    let tag = std::env::var(RELAY_TAG_ENV).expect("relay tag");
    let exe = std::env::var(HELPER_EXE_ENV).expect("helper exe path");

    let mut root_pids = Vec::with_capacity(root_count);
    for i in 0..root_count {
        let child = Command::new(&exe)
            .args(["root_fixture", "--exact", "--ignored", "--nocapture"])
            .env_clear()
            .env(MODE_ENV, "root")
            .env(HELPER_EXE_ENV, &exe)
            .env("BALLAST_OWNER", format!("{tag}-{i}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn root {i}: {e}"));
        root_pids.push(child.id());
        std::mem::forget(child);
    }
    for _ in 0..noise_count {
        match Command::new("sleep")
            .arg("3600")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => std::mem::forget(child),
            Err(_) => break,
        }
    }
    println!(
        "READY:{}",
        root_pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    std::process::exit(0);
}

/// Only reached via re-exec, already owner-marked via its own environment: spawns a persistent
/// `sh -c` workload (several background workers under it) plus a few plain internal children.
#[test]
#[ignore]
fn root_fixture() {
    Command::new("/bin/sh")
        .args([
            "-c",
            "sleep 60 & sleep 60 & sleep 60 & sleep 60 & sleep 60 & sleep 60 & wait",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(std::mem::forget)
        .expect("spawn root's workload shell");
    let exe = std::env::var(HELPER_EXE_ENV).expect("helper exe path");
    for _ in 0..3 {
        Command::new(&exe)
            .args(["idle_fixture", "--exact", "--ignored", "--nocapture"])
            .env(MODE_ENV, "idle")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(std::mem::forget)
            .expect("spawn root's internal child");
    }
    sleep(CHILD_LIFETIME_CAP);
}

/// Only reached via re-exec: runs the real daemon loop against a scratch `BALLAST_HOME`.
#[test]
#[ignore]
fn daemon_fixture() {
    let paths = Paths::from_env().expect("BALLAST_HOME must be set");
    std::fs::write(paths.base.join("config.toml"), "mode = \"observe\"\n")
        .expect("observe benchmark");
    ballast::daemon::run(paths).expect("daemon run");
}
