use super::*;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::time::Instant;

pub struct NativePlatform {
    cache: ProcessCache,
    page_size: u64,
    ticks: u64,
    live: HashSet<ProcessIdentity>,
    stat_buffer: Vec<u8>,
    path_buffer: String,
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
            cache: ProcessCache::default(),
            page_size: page_size as u64,
            ticks: ticks as u64,
            live: HashSet::with_capacity(2048),
            stat_buffer: Vec::with_capacity(1024),
            path_buffer: String::with_capacity(64),
            atomic_signals,
        })
    }

    fn stat(
        &self,
        pid: i32,
        contents: &mut Vec<u8>,
        path: &mut String,
        now: Instant,
        want_metrics: impl FnOnce(ProcessIdentity) -> bool,
    ) -> Option<Process> {
        if pid <= 0 {
            return None;
        }
        path.clear();
        write!(path, "/proc/{pid}/stat").ok()?;
        let mut file = fs::File::open(&*path).ok()?;
        contents.clear();
        // Avoid File's size/position probes: procfs reports size zero anyway.
        (&mut file).take(u64::MAX).read_to_end(contents).ok()?;
        let suffix = contents.get(contents.iter().rposition(|&byte| byte == b')')? + 2..)?;
        let mut fields = [""; 20];
        for (slot, value) in fields
            .iter_mut()
            .zip(std::str::from_utf8(suffix).ok()?.split_whitespace())
        {
            *slot = value;
        }
        let number = |index: usize| fields.get(index)?.parse::<u64>().ok();
        let cpu_time_ns = number(11)?
            .saturating_add(number(12)?)
            .saturating_mul(1_000_000_000)
            / self.ticks;
        let identity = ProcessIdentity {
            pid,
            start_time: number(19)?,
        };
        let uid = match self
            .cache
            .0
            .get(&pid)
            .filter(|(deadline, cached)| now < *deadline && cached.identity == identity)
        {
            Some((_, cached)) => cached.uid,
            None => file.metadata().ok()?.uid(),
        };
        let resident_pages = want_metrics(identity)
            .then(|| {
                path.clear();
                write!(path, "/proc/{pid}/statm").ok()?;
                fs::read_to_string(&*path)
                    .ok()
                    .and_then(|data| data.split_whitespace().nth(1)?.parse::<u64>().ok())
            })
            .flatten();
        Some(Process {
            identity,
            ppid: number(1)? as i32,
            pgid: number(2)? as i32,
            uid,
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
        self.stat(
            id.pid,
            &mut Vec::with_capacity(1024),
            &mut String::with_capacity(64),
            Instant::now(),
            |_| false,
        )
        .is_some_and(|p| p.identity == id)
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

    fn list_processes(
        &mut self,
        watched: &HashSet<ProcessIdentity>,
        metrics: &HashSet<ProcessIdentity>,
    ) -> io::Result<Vec<Process>> {
        let selected_pids = watched.iter().chain(metrics).map(|id| id.pid).collect();
        let mut processes = Vec::with_capacity(self.cache.0.len());
        let entries = fs::read_dir("/proc")?;
        let mut buffer = std::mem::take(&mut self.stat_buffer);
        let mut path = std::mem::take(&mut self.path_buffer);
        self.live.clear();
        let now = Instant::now();
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            if let Some(process) = self.cache.get(pid, now, &selected_pids) {
                self.live.insert(process.identity);
                processes.push(process);
                continue;
            }
            let Some(mut process) =
                self.stat(pid, &mut buffer, &mut path, now, |id| metrics.contains(&id))
            else {
                continue;
            };
            path.clear();
            write!(&mut path, "/proc/{pid}/exe").unwrap();
            process.exe = fs::read_link(&path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            if self.cache.0.get(&pid).is_some_and(|(_, cached)| {
                cached.identity == process.identity && cached.exe != process.exe
            }) {
                path.clear();
                write!(&mut path, "/proc/{pid}").unwrap();
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                process.uid = metadata.uid();
            }
            let cached_argv = self.cache.0.get(&pid).filter(|(deadline, cached)| {
                now < *deadline
                    && cached.identity == process.identity
                    && cached.exe == process.exe
                    && cached
                        .argv
                        .as_ref()
                        .is_some_and(|args| args.iter().any(|arg| !arg.is_empty()))
            });
            if let Some((_, cached)) = cached_argv {
                process.argv.clone_from(&cached.argv);
            } else {
                path.clear();
                write!(&mut path, "/proc/{pid}/cmdline").unwrap();
                process.argv = fs::read(&path).ok().and_then(|bytes| {
                    // During exec, procfs can expose the new exe before cmdline is ready.
                    if bytes.is_empty() || bytes.last() != Some(&0) {
                        return None;
                    }
                    Some(
                        bytes[..bytes.len() - 1]
                            .split(|&b| b == 0)
                            .map(|arg| String::from_utf8_lossy(arg).into_owned())
                            .collect(),
                    )
                });
            }
            self.cache.insert(&process, now);
            self.live.insert(process.identity);
            processes.push(process);
        }
        self.stat_buffer = buffer;
        self.path_buffer = path;
        self.cache.retain(&self.live);
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
        let process = self.stat(
            id.pid,
            &mut Vec::with_capacity(1024),
            &mut String::with_capacity(64),
            Instant::now(),
            |_| true,
        )?;
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

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
