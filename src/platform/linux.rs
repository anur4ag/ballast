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
    enumerated: Option<HashSet<i32>>,
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
            enumerated: None,
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
        // procfs emits stat in one short read; avoid a second syscall just to discover EOF.
        contents.resize(4096, 0);
        let length = loop {
            match file.read(contents) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result.ok()?,
            }
        };
        contents.truncate(length);
        if length == 4096 || contents.last() != Some(&b'\n') {
            return None;
        }
        let suffix = contents.get(contents.iter().rposition(|&byte| byte == b')')? + 2..)?;
        let mut fields = [""; 22];
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
        let resident_pages = want_metrics(identity).then(|| number(21)).flatten();
        Some(Process {
            identity,
            ppid: number(1)? as i32,
            pgid: number(2)? as i32,
            uid,
            stopped: matches!(fields.first().copied(), Some("T" | "t")),
            name: contents.iter().position(|&b| b == b'(').and_then(|start| {
                let end = contents.iter().rposition(|&b| b == b')')?;
                std::str::from_utf8(&contents[start + 1..end])
                    .ok()
                    .map(str::to_owned)
            }),
            exe: None,
            argv: None,
            metrics: resident_pages.map(|pages| ProcessMetrics {
                memory_bytes: pages.saturating_mul(self.page_size),
                cpu_time_ns,
            }),
        })
    }

    fn read_identity(&self, pid: i32) -> Option<ProcessIdentity> {
        self.stat(
            pid,
            &mut Vec::with_capacity(1024),
            &mut String::with_capacity(64),
            Instant::now(),
            |_| false,
        )
        .map(|p| p.identity)
    }
    fn matches(&self, id: ProcessIdentity) -> bool {
        self.read_identity(id.pid) == Some(id)
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
        let mut enumerated = HashSet::new();
        for entry in entries {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            enumerated.insert(pid);
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
        self.enumerated = Some(enumerated);
        Ok(processes)
    }

    fn pid_is_present(&self, pid: i32) -> Option<bool> {
        self.enumerated.as_ref().map(|pids| pids.contains(&pid))
    }

    fn process_liveness(&self, id: ProcessIdentity) -> ProcessLiveness {
        if self.live.contains(&id) {
            ProcessLiveness::Alive
        } else if self
            .enumerated
            .as_ref()
            .is_some_and(|pids| !pids.contains(&id.pid))
            || self
                .cache
                .0
                .get(&id.pid)
                .is_some_and(|(_, p)| p.identity != id)
        {
            ProcessLiveness::Gone
        } else {
            ProcessLiveness::Unknown
        }
    }

    fn process_parent(&self, pid: i32) -> Option<(ProcessIdentity, i32)> {
        self.stat(
            pid,
            &mut Vec::with_capacity(1024),
            &mut String::with_capacity(64),
            Instant::now(),
            |_| false,
        )
        .map(|process| (process.identity, process.ppid))
    }

    fn read_arguments(&self, id: ProcessIdentity) -> Option<Vec<String>> {
        if !self.matches(id) {
            return None;
        }
        let bytes = fs::read(format!("/proc/{}/cmdline", id.pid)).ok()?;
        let bytes = bytes.strip_suffix(&[0])?;
        let argv = bytes
            .split(|&b| b == 0)
            .map(|arg| std::str::from_utf8(arg).map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        self.matches(id).then_some(argv)
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

    fn process_age(&self, id: ProcessIdentity) -> Option<Duration> {
        let uptime: f64 = fs::read_to_string("/proc/uptime")
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let age =
            Duration::try_from_secs_f64(uptime - id.start_time as f64 / self.ticks as f64).ok()?;
        self.matches(id).then_some(age)
    }

    fn process_cwd(&self, id: ProcessIdentity) -> Option<std::path::PathBuf> {
        if !self.matches(id) {
            return None;
        }
        let cwd = fs::read_link(format!("/proc/{}/cwd", id.pid)).ok()?;
        self.matches(id).then_some(cwd)
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
        inputs.swap_total_bytes = values.get("SwapTotal").copied();
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
        self.listening_ports_batch(&[id]).remove(&id).flatten()
    }

    fn listening_ports_batch(
        &self,
        processes: &[ProcessIdentity],
    ) -> HashMap<ProcessIdentity, Option<Vec<u16>>> {
        let mut tables = HashMap::new();
        processes
            .iter()
            .map(|&id| {
                let ports = (|| {
                    if !self.matches(id) {
                        return None;
                    }
                    let inodes = socket_inodes(id.pid)?;
                    let mut ports = Vec::new();
                    if !inodes.is_empty() {
                        let namespace = fs::metadata(format!("/proc/{}/ns/net", id.pid)).ok()?;
                        let table = tables
                            .entry((namespace.dev(), namespace.ino()))
                            .or_insert_with(|| tcp_listeners(id.pid))
                            .as_ref()?;
                        ports.extend(inodes.iter().filter_map(|inode| table.get(inode)).copied());
                        ports.sort_unstable();
                        ports.dedup();
                    }
                    self.matches(id).then_some(ports)
                })();
                (id, ports)
            })
            .collect()
    }

    fn send_signal(&self, id: ProcessIdentity, signal: Signal) -> io::Result<()> {
        if id.pid <= 0 {
            return Err(gone());
        }
        if self.atomic_signals {
            let fd = pidfd(id.pid)?;
            super::validate_identity(id, self.read_identity(id.pid))?;
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
            super::validate_identity(id, self.read_identity(id.pid))?;
            if unsafe { libc::kill(id.pid, signal.raw()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn notify(&self, title: &str, body: &str) -> io::Result<bool> {
        let mut command = Command::new("notify-send");
        command.args(["--", title, body]);
        super::submit_notification(command)
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

fn socket_inodes(pid: i32) -> Option<HashSet<u64>> {
    let mut inodes = HashSet::new();
    for fd in fs::read_dir(format!("/proc/{pid}/fd")).ok()? {
        match fs::read_link(fd.ok()?.path()) {
            Ok(link) => {
                if let Some(inode) = link
                    .to_str()?
                    .strip_prefix("socket:[")
                    .and_then(|s| s.strip_suffix(']'))
                {
                    inodes.insert(inode.parse().ok()?);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    Some(inodes)
}

fn tcp_listeners(pid: i32) -> Option<HashMap<u64, u16>> {
    let mut listeners = HashMap::new();
    for table in ["tcp", "tcp6"] {
        let data = match fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
            Ok(data) => data,
            Err(error) if table == "tcp6" && error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        for line in data.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() > 9 && fields[3] == "0A" {
                let (_, port) = fields[1].rsplit_once(':')?;
                listeners.insert(fields[9].parse().ok()?, u16::from_str_radix(port, 16).ok()?);
            }
        }
    }
    Some(listeners)
}
