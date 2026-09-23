use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Process {
    pub identity: ProcessIdentity,
    pub ppid: i32,
    pub pgid: i32,
    pub uid: u32,
    pub stopped: bool,
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

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Capabilities {
    pub environment: bool,
    pub listening_ports: bool,
    pub memory_footprint: bool,
    pub memory_psi: bool,
    pub kernel_pressure: bool,
    pub notifications: bool,
    pub atomic_signals: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PressureInputs {
    pub page_size: u64,
    pub total_memory_bytes: Option<u64>,
    pub used_memory_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub kernel_pressure_level: Option<u32>,
    pub pageouts: Option<u64>,
    pub swapins: Option<u64>,
    pub swapouts: Option<u64>,
    pub psi_some_avg10: Option<f64>,
    pub psi_full_avg10: Option<f64>,
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
    fn capabilities(&self) -> Capabilities;
    /// Persist alongside process identities; discard saved entries from a different boot.
    fn boot_id(&self) -> io::Result<String>;
    /// Skips vanished/inaccessible processes. argv is cached until identity or exe changes.
    /// ponytail: same-binary exec can leave argv stale; read argv each scan if required.
    fn list_processes(&mut self) -> io::Result<Vec<Process>>;
    /// None means unknown, including permission denied, truncation or identity mismatch.
    fn read_environment(&self, process: ProcessIdentity) -> Option<Environment>;
    fn process_metrics(&self, process: ProcessIdentity) -> Option<ProcessMetrics>;
    fn pressure(&self) -> io::Result<PressureInputs>;
    /// Only call for attributed processes, on demand or on a slow cadence.
    fn listening_ports(&self, process: ProcessIdentity) -> Option<Vec<u16>>;
    /// Rejects invalid PIDs and changed identities before signalling.
    fn send_signal(&self, process: ProcessIdentity, signal: Signal) -> io::Result<()>;
    /// False means unavailable; the caller should keep the message in its log/UI.
    fn notify(&self, title: &str, body: &str) -> io::Result<bool>;
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
}
