use super::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::process::Command;

pub struct NativePlatform {
    page_size: u64,
    ticks: u64,
    details: HashMap<ProcessIdentity, (Option<String>, Option<Vec<String>>)>,
    atomic_signals: bool,
}

impl NativePlatform {
    pub fn new() -> io::Result<Self> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if page_size <= 0 || ticks <= 0 {
            return Err(io::Error::last_os_error());
        }
        let atomic_signals = match pidfd(std::process::id() as i32) {
            Ok(_) => true,
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => false,
            Err(error) => return Err(error),
        };
        Ok(Self {
            page_size: page_size as u64,
            ticks: ticks as u64,
            details: HashMap::new(),
            atomic_signals,
        })
    }

    fn stat(&self, pid: i32) -> Option<Process> {
        if pid <= 0 {
            return None;
        }
        let path = format!("/proc/{pid}/stat");
        let contents = fs::read(&path).ok()?;
        let suffix = contents.get(contents.iter().rposition(|&byte| byte == b')')? + 2..)?;
        let fields: Vec<_> = std::str::from_utf8(suffix)
            .ok()?
            .split_whitespace()
            .collect();
        let number = |index: usize| fields.get(index)?.parse::<u64>().ok();
        let cpu_time_ns = number(11)?
            .saturating_add(number(12)?)
            .saturating_mul(1_000_000_000)
            / self.ticks;
        let resident_pages = fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|data| data.split_whitespace().nth(1)?.parse::<u64>().ok());
        Some(Process {
            identity: ProcessIdentity {
                pid,
                start_time: number(19)?,
            },
            ppid: number(1)? as i32,
            pgid: number(2)? as i32,
            uid: fs::metadata(path).ok()?.uid(),
            stopped: matches!(fields.first().copied(), Some("T" | "t")),
            exe: None,
            argv: None,
            metrics: resident_pages.map(|pages| ProcessMetrics {
                memory_bytes: pages.saturating_mul(self.page_size),
                cpu_time_ns,
            }),
        })
    }

    fn matches(&self, id: ProcessIdentity) -> bool {
        self.stat(id.pid).is_some_and(|p| p.identity == id)
    }
}

impl Platform for NativePlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            environment: true,
            listening_ports: true,
            memory_footprint: false,
            memory_psi: fs::metadata("/proc/pressure/memory").is_ok(),
            kernel_pressure: false,
            notifications: has_notify_send(),
            atomic_signals: self.atomic_signals,
        }
    }

    fn boot_id(&self) -> io::Result<String> {
        Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned())
    }

    fn list_processes(&mut self) -> io::Result<Vec<Process>> {
        let mut processes = Vec::with_capacity(self.details.len());
        let mut live = HashSet::with_capacity(self.details.len());
        for entry in fs::read_dir("/proc")?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            let Some(mut process) = self.stat(pid) else {
                continue;
            };
            let exe = fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            if self
                .details
                .get(&process.identity)
                .is_none_or(|cached| cached.0 != exe)
            {
                let argv = fs::read(format!("/proc/{pid}/cmdline"))
                    .ok()
                    .and_then(|bytes| {
                        if !bytes.is_empty() && bytes.last() != Some(&0) {
                            return None;
                        }
                        Some(
                            bytes
                                .strip_suffix(&[0])
                                .unwrap_or(&bytes)
                                .split(|&b| b == 0)
                                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                                .collect(),
                        )
                    });
                self.details.insert(process.identity, (exe, argv));
            }
            let details = self
                .details
                .get(&process.identity)
                .expect("inserted process details");
            process.exe.clone_from(&details.0);
            process.argv.clone_from(&details.1);
            live.insert(process.identity);
            processes.push(process);
        }
        self.details.retain(|id, _| live.contains(id));
        Ok(processes)
    }

    fn read_environment(&self, id: ProcessIdentity) -> Option<Environment> {
        if !self.matches(id) {
            return None;
        }
        let bytes = fs::read(format!("/proc/{}/environ", id.pid)).ok()?;
        let environment = parse_environment(&bytes)?;
        self.matches(id).then_some(environment)
    }

    fn process_metrics(&self, id: ProcessIdentity) -> Option<ProcessMetrics> {
        let process = self.stat(id.pid)?;
        (process.identity == id).then_some(process.metrics?)
    }

    fn pressure(&self) -> io::Result<PressureInputs> {
        let vmstat = fs::read_to_string("/proc/vmstat")?;
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let mut inputs = PressureInputs {
            page_size: self.page_size,
            ..Default::default()
        };
        let values: HashMap<_, _> = meminfo
            .lines()
            .filter_map(|line| {
                let (key, rest) = line.split_once(':')?;
                Some((
                    key,
                    rest.split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()?
                        .saturating_mul(1024),
                ))
            })
            .collect();
        inputs.total_memory_bytes = values.get("MemTotal").copied();
        inputs.used_memory_bytes = values
            .get("MemTotal")
            .zip(values.get("MemAvailable"))
            .map(|(total, available)| total.saturating_sub(*available));
        inputs.swap_used_bytes = values
            .get("SwapTotal")
            .zip(values.get("SwapFree"))
            .map(|(total, free)| total.saturating_sub(*free));
        for line in vmstat.lines() {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("pswpin") => inputs.swapins = parts.next().and_then(|n| n.parse().ok()),
                Some("pswpout") => inputs.swapouts = parts.next().and_then(|n| n.parse().ok()),
                _ => {}
            }
        }
        if let Ok(psi) = fs::read_to_string("/proc/pressure/memory") {
            for line in psi.lines() {
                let mut parts = line.split_whitespace();
                let kind = parts.next();
                let avg10 = parts
                    .find_map(|s| s.strip_prefix("avg10="))
                    .and_then(|n| n.parse().ok());
                match kind {
                    Some("some") => inputs.psi_some_avg10 = avg10,
                    Some("full") => inputs.psi_full_avg10 = avg10,
                    _ => {}
                }
            }
        }
        Ok(inputs)
    }

    fn listening_ports(&self, id: ProcessIdentity) -> Option<Vec<u16>> {
        if !self.matches(id) {
            return None;
        }
        let fds = fs::read_dir(format!("/proc/{}/fd", id.pid)).ok()?;
        let mut inodes = HashSet::new();
        for fd in fds.flatten() {
            match fs::read_link(fd.path()) {
                Ok(link) => {
                    let link = link.to_str()?;
                    if let Some(inode) = link
                        .strip_prefix("socket:[")
                        .and_then(|s| s.strip_suffix(']'))
                    {
                        inodes.insert(inode.to_owned());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            }
        }
        let mut ports = Vec::new();
        for table in ["tcp", "tcp6"] {
            let data = match fs::read_to_string(format!("/proc/{}/net/{table}", id.pid)) {
                Ok(data) => data,
                Err(e) if table == "tcp6" && e.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            for line in data.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() > 9 && fields[3] == "0A" && inodes.contains(fields[9]) {
                    let (_, port) = fields[1].rsplit_once(':')?;
                    ports.push(u16::from_str_radix(port, 16).ok()?);
                }
            }
        }
        ports.sort_unstable();
        ports.dedup();
        self.matches(id).then_some(ports)
    }

    fn send_signal(&self, id: ProcessIdentity, signal: Signal) -> io::Result<()> {
        if id.pid <= 0 {
            return Err(gone());
        }
        if self.atomic_signals {
            let fd = pidfd(id.pid)?;
            if !self.matches(id) {
                return Err(gone());
            }
            if unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal.raw(),
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
        } else {
            // Old kernels lack pidfds; revalidation cannot close the final PID reuse race.
            if !self.matches(id) {
                return Err(gone());
            }
            if unsafe { libc::kill(id.pid, signal.raw()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn notify(&self, title: &str, body: &str) -> io::Result<bool> {
        match Command::new("notify-send")
            .args(["--", title, body])
            .status()
        {
            Ok(status) => Ok(status.success()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn pidfd(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

fn has_notify_send() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            fs::metadata(dir.join("notify-send"))
                .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        })
    })
}
