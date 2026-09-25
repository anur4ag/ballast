use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::NativePlatform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::NativePlatform;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Ballast supports macOS and Linux");

/// Start time is an opaque OS-native token, stable across daemon restarts.
/// Linux uses ticks since boot; macOS uses microseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub start_time: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessLiveness {
    Alive,
    Gone,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Process {
    pub identity: ProcessIdentity,
    pub ppid: i32,
    pub pgid: i32,
    pub uid: u32,
    pub stopped: bool,
    #[serde(default)]
    pub name: Option<String>,
    pub exe: Option<String>,
    pub argv: Option<Vec<String>>,
    pub metrics: Option<ProcessMetrics>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProcessMetrics {
    pub memory_bytes: u64,
    /// Cumulative user + system CPU time; callers compute deltas.
    pub cpu_time_ns: u64,
}

pub type Environment = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub environment: bool,
    pub listening_ports: bool,
    pub memory_footprint: bool,
    pub memory_psi: bool,
    pub kernel_pressure: bool,
    pub notifications: bool,
    pub atomic_signals: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PressureInputs {
    pub page_size: u64,
    pub total_memory_bytes: Option<u64>,
    pub used_memory_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    #[serde(default)]
    pub swap_total_bytes: Option<u64>,
    pub kernel_pressure_level: Option<u32>,
    pub pageouts: Option<u64>,
    pub swapins: Option<u64>,
    pub swapouts: Option<u64>,
    pub psi_some_avg10: Option<f64>,
    pub psi_full_avg10: Option<f64>,
    #[serde(default)]
    pub psi_some_total_us: Option<u64>,
    #[serde(default)]
    pub psi_full_total_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle: Option<crate::guardian::throttle::Inputs>,
}

#[derive(Clone, Copy, Debug)]
pub enum Signal {
    Stop,
    Continue,
    Terminate,
    Kill,
}
impl Signal {
    fn raw(self) -> i32 {
        match self {
            Self::Stop => libc::SIGSTOP,
            Self::Continue => libc::SIGCONT,
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

pub trait Platform {
    fn supports_throttle(&self) -> bool {
        false
    }
    fn backgrounded(&self, _id: ProcessIdentity) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "throttling unavailable",
        ))
    }
    fn set_backgrounded(&self, _id: ProcessIdentity, _enabled: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "throttling unavailable",
        ))
    }
    fn process_io_bytes(&self, _id: ProcessIdentity) -> Option<u64> {
        None
    }
    fn capabilities(&self) -> Capabilities;
    /// Persist alongside process identities; discard saved entries from a different boot.
    fn boot_id(&self) -> io::Result<String>;
    /// Enumerates PIDs every scan. Watched and metric targets refresh every scan;
    /// other known processes refresh on a staggered five-second cadence.
    /// Metrics are collected only for exact selected identities, never returned from cache.
    /// macOS argv may stay cached until exe changes.
    fn list_processes(
        &mut self,
        watched: &HashSet<ProcessIdentity>,
        metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>>;
    /// None means unknown, including permission denied, truncation or identity mismatch.
    fn read_environment(&self, process: ProcessIdentity) -> Option<Environment>;
    /// One native read of identity and parent, including processes too new for a snapshot.
    fn process_parent(&self, _pid: i32) -> Option<(ProcessIdentity, i32)> {
        None
    }
    /// Fresh arguments with identity checks before and after reading.
    fn read_arguments(&self, _process: ProcessIdentity) -> Option<Vec<String>> {
        None
    }
    fn process_metrics(&self, process: ProcessIdentity) -> Option<ProcessMetrics>;
    /// PID enumeration evidence without requiring a readable process identity.
    fn pid_is_present(&self, _pid: i32) -> Option<bool> {
        None
    }
    /// Evidence from the latest successful scan; an unreadable enumerated PID is unknown.
    fn process_liveness(&self, _process: ProcessIdentity) -> ProcessLiveness {
        ProcessLiveness::Unknown
    }

    /// Sampled lazily for attributed processes; None means unknown or identity changed.
    fn process_age(&self, _process: ProcessIdentity) -> Option<Duration> {
        None
    }
    /// Exact OS cwd, sampled only for agent roots on the slow cadence.
    fn process_cwd(&self, _process: ProcessIdentity) -> Option<std::path::PathBuf> {
        None
    }
    fn pressure(&self) -> io::Result<PressureInputs>;
    /// Only call for attributed processes, on demand or on a slow cadence.
    fn listening_ports(&self, process: ProcessIdentity) -> Option<Vec<u16>>;
    /// One sampling round. Platforms may share namespace-wide socket tables across targets.
    fn listening_ports_batch(
        &self,
        processes: &[ProcessIdentity],
    ) -> HashMap<ProcessIdentity, Option<Vec<u16>>> {
        processes
            .iter()
            .map(|&id| (id, self.listening_ports(id)))
            .collect()
    }
    /// Rejects invalid PIDs and changed identities before signalling.
    /// NotFound means a confirmed exit or identity mismatch; unreadable identities use another error kind.
    fn send_signal(&self, process: ProcessIdentity, signal: Signal) -> io::Result<()>;
    /// True means submitted, not necessarily displayed. Delivery must not block the tick loop.
    /// False means unavailable; the caller should keep the message in its log/UI.
    fn notify(&self, title: &str, body: &str) -> io::Result<bool>;
}

// Notification helpers may wait for a desktop bus. Reap them off the tick thread,
// with a deadline so a missing desktop cannot accumulate stuck children.
fn submit_notification(mut command: std::process::Command, enabled: bool) -> io::Result<bool> {
    if !enabled {
        return Ok(false);
    }
    use std::process::Stdio;
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    });
    Ok(true)
}

// ponytail: unwatched PID reuse/state changes can lag five seconds plus one scan.
// Watch identities before making policy decisions; signals always revalidate independently.
#[derive(Default)]
struct ProcessCache(HashMap<i32, (Instant, Process)>);
impl ProcessCache {
    fn get(&self, pid: i32, now: Instant, selected_pids: &HashSet<i32>) -> Option<Process> {
        let (deadline, process) = self.0.get(&pid)?;
        (now < *deadline && !selected_pids.contains(&pid)).then(|| process.clone())
    }
    fn insert(&mut self, process: &Process, now: Instant) {
        let deadline = self.0.get(&process.identity.pid).map_or_else(
            || now + Duration::from_secs(1 + process.identity.pid as u64 % 5),
            |(deadline, _)| {
                if now >= *deadline {
                    now + Duration::from_secs(5)
                } else {
                    *deadline
                }
            },
        );
        let mut cached = process.clone();
        cached.metrics = None;
        self.0.insert(process.identity.pid, (deadline, cached));
    }
    fn retain(&mut self, live: &HashSet<ProcessIdentity>) {
        self.0
            .retain(|_, (_, process)| live.contains(&process.identity));
    }
}

#[cfg(target_os = "macos")]
fn invalid_data() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid process data")
}
fn gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "process identity no longer matches",
    )
}

/// An unreadable identity is not proof of exit; preserve recovery evidence while the PID exists.
pub(crate) fn validate_identity(
    expected: ProcessIdentity,
    actual: Option<ProcessIdentity>,
) -> io::Result<()> {
    if expected.pid <= 0 {
        return Err(gone());
    }
    match actual {
        Some(id) if id == expected => Ok(()),
        Some(_) => Err(gone()),
        None => {
            if unsafe { libc::kill(expected.pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                Err(gone())
            } else {
                Err(io::Error::other("process identity unreadable"))
            }
        }
    }
}

fn parse_environment(bytes: &[u8]) -> Option<Environment> {
    if !bytes.is_empty() && bytes.last() != Some(&0) {
        return None;
    }
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let split = entry.iter().position(|&b| b == b'=')?;
            Some((
                String::from_utf8(entry[..split].to_vec()).ok()?,
                String::from_utf8(entry[split + 1..].to_vec()).ok()?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_notifications_never_spawn_the_helper() {
        let command = std::process::Command::new("invalid\0notification-helper");
        assert!(!submit_notification(command, false).unwrap());
    }

    #[test]
    fn parses_empty_environment() {
        assert_eq!(parse_environment(&[]), Some(Environment::new()));
    }

    #[test]
    fn missing_trailing_nul_is_none() {
        assert_eq!(parse_environment(b"FOO=bar"), None);
    }

    #[test]
    fn entry_missing_equals_is_none() {
        assert_eq!(parse_environment(b"FOO=bar\0NOTANENTRY\0"), None);
    }

    #[test]
    fn parses_multiple_entries() {
        let env = parse_environment(b"FOO=bar\0BAZ=qux\0").expect("well-formed environment");
        assert_eq!(env.len(), 2);
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn value_containing_equals_is_kept_whole() {
        let env = parse_environment(b"URL=http://a?b=c\0").expect("well-formed environment");
        assert_eq!(env.get("URL").map(String::as_str), Some("http://a?b=c"));
    }

    #[test]
    fn invalid_utf8_is_none() {
        // One byte in the key is not valid UTF-8: the whole buffer becomes
        // unknown rather than silently dropping or mangling just that entry.
        let bytes = [0xFFu8, b'=', b'x', 0];
        assert_eq!(parse_environment(&bytes), None);
    }

    fn cached_process(identity: ProcessIdentity) -> Process {
        Process {
            identity,
            ppid: 1,
            pgid: identity.pid,
            uid: 0,
            stopped: false,
            name: None,
            exe: None,
            argv: None,
            metrics: None,
        }
    }

    #[test]
    fn selecting_a_reused_pid_bypasses_its_stale_cached_identity() {
        let mut cache = ProcessCache::default();
        let now = Instant::now();
        let stale = ProcessIdentity {
            pid: 123,
            start_time: 1,
        };
        cache.insert(&cached_process(stale), now);

        // `get` only knows the pid union; watched-vs-metrics selection is
        // built by the caller (see linux_tests.rs for that end-to-end path).
        let selected = HashSet::from([123]);
        let got = cache.get(123, now, &selected);
        assert!(
            got.is_none(),
            "selecting a reused pid must bypass its stale cached identity, got {got:?}"
        );
    }
}
