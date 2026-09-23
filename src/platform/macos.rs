use super::*;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_void};
use std::mem::{size_of, size_of_val};
use std::process::Command;

unsafe extern "C" {
    fn ballast_listening_port(pid: i32, fd: i32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

pub struct NativePlatform {
    page_size: u64,
    arg_max: usize,
    pids: Vec<i32>,
    details: HashMap<ProcessIdentity, (Option<String>, Option<Vec<String>>)>,
}

impl NativePlatform {
    pub fn new() -> io::Result<Self> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err(io::Error::last_os_error());
        }
        let arg_max: i32 = sysctl_value(c"kern.argmax")?;
        if arg_max <= 0 {
            return Err(invalid_data());
        }
        Ok(Self {
            page_size: page_size as u64,
            arg_max: arg_max as usize,
            pids: vec![0; 2048],
            details: HashMap::new(),
        })
    }

    fn bsd(pid: i32) -> Option<libc::proc_bsdinfo> {
        if pid <= 0 {
            return None;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size_of_val(&info) as i32,
            )
        };
        (size == size_of_val(&info) as i32).then_some(info)
    }

    fn matches(id: ProcessIdentity) -> bool {
        Self::bsd(id.pid).is_some_and(|info| identity(&info) == id)
    }

    fn args(&self, pid: i32) -> Option<(Vec<String>, Option<Environment>)> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut bytes = vec![0; self.arg_max];
        let mut size = bytes.len();
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 || size >= bytes.len() {
            return None;
        }
        bytes.truncate(size);
        parse_args(&bytes)
    }

    fn metrics(pid: i32) -> Option<ProcessMetrics> {
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&mut usage as *mut libc::rusage_info_v2).cast(),
            )
        };
        (result == 0).then_some(ProcessMetrics {
            memory_bytes: usage.ri_phys_footprint,
            cpu_time_ns: usage.ri_user_time.saturating_add(usage.ri_system_time),
        })
    }
}

impl Platform for NativePlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            environment: true,
            listening_ports: true,
            memory_footprint: true,
            memory_psi: false,
            kernel_pressure: sysctl_value::<u32>(c"kern.memorystatus_vm_pressure_level").is_ok(),
            notifications: std::path::Path::new("/usr/bin/osascript").exists(),
            atomic_signals: false,
        }
    }

    fn boot_id(&self) -> io::Result<String> {
        let mut bytes = [0u8; 128];
        let mut size = bytes.len();
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 {
            let uuid = CStr::from_bytes_until_nul(&bytes[..size]).map_err(|_| invalid_data())?;
            return Ok(uuid.to_string_lossy().into_owned());
        }
        let boot: libc::timeval = sysctl_value(c"kern.boottime")?;
        Ok(format!("{}:{}", boot.tv_sec, boot.tv_usec))
    }

    fn list_processes(&mut self) -> io::Result<Vec<Process>> {
        let count = loop {
            let count = unsafe {
                libc::proc_listallpids(
                    self.pids.as_mut_ptr().cast(),
                    (self.pids.len() * size_of::<i32>()) as i32,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if (count as usize) < self.pids.len() {
                break count as usize;
            }
            self.pids.resize(self.pids.len() * 2, 0);
        };
        let mut processes = Vec::with_capacity(count);
        let mut live = HashSet::with_capacity(count);
        for index in 0..count {
            let pid = self.pids[index];
            let Some(info) = Self::bsd(pid) else {
                continue;
            };
            let id = identity(&info);
            let exe = executable(pid);
            if self.details.get(&id).is_none_or(|cached| cached.0 != exe) {
                let argv = self.args(pid).map(|(argv, _)| argv);
                self.details.insert(id, (exe, argv));
            }
            let (exe, argv) = self.details.get(&id).expect("inserted process details");
            processes.push(Process {
                identity: id,
                ppid: info.pbi_ppid as i32,
                pgid: info.pbi_pgid as i32,
                uid: info.pbi_uid,
                stopped: info.pbi_status == 4,
                exe: exe.clone(),
                argv: argv.clone(),
                metrics: Self::metrics(pid),
            });
            live.insert(id);
        }
        self.details.retain(|id, _| live.contains(id));
        Ok(processes)
    }

    fn read_environment(&self, id: ProcessIdentity) -> Option<Environment> {
        if !Self::matches(id) {
            return None;
        }
        let (_, env) = self.args(id.pid)?;
        Self::matches(id).then_some(env?)
    }

    fn process_metrics(&self, id: ProcessIdentity) -> Option<ProcessMetrics> {
        if !Self::matches(id) {
            return None;
        }
        let metrics = Self::metrics(id.pid)?;
        Self::matches(id).then_some(metrics)
    }

    fn pressure(&self) -> io::Result<PressureInputs> {
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        #[allow(deprecated)]
        let host = unsafe { libc::mach_host_self() };
        let result = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&mut stats as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        };
        #[allow(deprecated)]
        unsafe {
            mach_port_deallocate(libc::mach_task_self(), host);
        }
        if result != 0 {
            return Err(io::Error::other(format!("host_statistics64: {result}")));
        }
        let total = sysctl_value::<u64>(c"hw.memsize").ok();
        let used_pages = (u64::from(stats.internal_page_count)
            + u64::from(stats.wire_count)
            + u64::from(stats.compressor_page_count))
        .saturating_sub(u64::from(stats.purgeable_count));
        let swap = sysctl_value::<libc::xsw_usage>(c"vm.swapusage").ok();
        Ok(PressureInputs {
            page_size: self.page_size,
            total_memory_bytes: total,
            used_memory_bytes: Some(used_pages.saturating_mul(self.page_size)),
            swap_used_bytes: swap.map(|s| s.xsu_used),
            kernel_pressure_level: sysctl_value(c"kern.memorystatus_vm_pressure_level").ok(),
            pageouts: Some(stats.pageouts),
            swapins: Some(stats.swapins),
            swapouts: Some(stats.swapouts),
            ..Default::default()
        })
    }

    fn listening_ports(&self, id: ProcessIdentity) -> Option<Vec<u16>> {
        if !Self::matches(id) {
            return None;
        }
        let needed = unsafe {
            libc::proc_pidinfo(id.pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
        };
        if needed < 0 {
            return None;
        }
        let mut fds: Vec<libc::proc_fdinfo> = (0..needed as usize / size_of::<libc::proc_fdinfo>()
            + 32)
            .map(|_| unsafe { std::mem::zeroed() })
            .collect();
        let mut ports = Vec::new();
        let size = loop {
            let capacity = size_of_val(fds.as_slice());
            let size = unsafe {
                libc::proc_pidinfo(
                    id.pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    fds.as_mut_ptr().cast(),
                    capacity as i32,
                )
            };
            if size <= 0 {
                return None;
            }
            if size as usize != capacity {
                break size as usize;
            }
            fds.resize_with(fds.len() * 2, || unsafe { std::mem::zeroed() });
        };
        for fd in &fds[..size / size_of::<libc::proc_fdinfo>()] {
            if fd.proc_fdtype != 2 {
                continue;
            } // PROX_FDTYPE_SOCKET
            let port = unsafe { ballast_listening_port(id.pid, fd.proc_fd) };
            if port < 0 {
                let error = io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::EBADF | libc::ENOENT)) {
                    continue;
                }
                return None;
            }
            if port > 0 {
                ports.push(port as u16);
            }
        }
        ports.sort_unstable();
        ports.dedup();
        Self::matches(id).then_some(ports)
    }

    fn send_signal(&self, id: ProcessIdentity, signal: Signal) -> io::Result<()> {
        // macOS has no pidfd: immediate revalidation leaves a small exit/PID-reuse race.
        if !Self::matches(id) {
            return Err(gone());
        }
        if unsafe { libc::kill(id.pid, signal.raw()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn notify(&self, title: &str, body: &str) -> io::Result<bool> {
        let script = "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run";
        Ok(Command::new("/usr/bin/osascript")
            .args(["-e", script, "--", title, body])
            .status()?
            .success())
    }
}

fn identity(info: &libc::proc_bsdinfo) -> ProcessIdentity {
    ProcessIdentity {
        pid: info.pbi_pid as i32,
        start_time: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
    }
}

fn executable(pid: i32) -> Option<String> {
    let mut bytes = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let size = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    (size > 0).then(|| {
        String::from_utf8_lossy(&bytes[..size as usize])
            .trim_end_matches('\0')
            .to_owned()
    })
}

fn sysctl_value<T>(name: &CStr) -> io::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>();
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast::<c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if size != size_of::<T>() {
        return Err(invalid_data());
    }
    Ok(unsafe { value.assume_init() })
}

fn parse_args(bytes: &[u8]) -> Option<(Vec<String>, Option<Environment>)> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let mut rest = bytes.get(4..)?;
    rest = rest.get(rest.iter().position(|&b| b == 0)? + 1..)?;
    while rest.first() == Some(&0) {
        rest = &rest[1..];
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0)?;
        argv.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + 1..];
    }
    // SIP may omit the entire environment while still returning complete argv.
    // An empty string terminates the environment before Apple's auxiliary data.
    let env_end = if rest.first() == Some(&0) {
        Some(0)
    } else {
        rest.windows(2)
            .position(|pair| pair == [0, 0])
            .map(|end| end + 1)
    };
    let env = env_end.and_then(|end| parse_environment(&rest[..end]));
    Some((argv, env))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a KERN_PROCARGS2-shaped buffer: argc, exec_path\0, `pad_zeros`
    /// padding bytes, then argv[0..argc]\0 each, then raw env bytes as given
    /// (so a test can omit env or leave it malformed on purpose).
    fn procargs2(
        argc: i32,
        exec_path: &str,
        pad_zeros: usize,
        argv: &[&str],
        env: &[u8],
    ) -> Vec<u8> {
        let mut bytes = argc.to_ne_bytes().to_vec();
        bytes.extend_from_slice(exec_path.as_bytes());
        bytes.push(0);
        bytes.extend(std::iter::repeat_n(0u8, pad_zeros));
        for a in argv {
            bytes.extend_from_slice(a.as_bytes());
            bytes.push(0);
        }
        bytes.extend_from_slice(env);
        bytes
    }

    #[test]
    fn parses_argv_and_env_past_exec_path_and_padding() {
        let mut bytes = procargs2(
            2,
            "/bin/true",
            3,
            &["/bin/true", "--flag"],
            b"FOO=bar\0BAZ=qux\0",
        );
        bytes.push(0); // envp terminator: empty string
        let (argv, env) = parse_args(&bytes).expect("well-formed procargs2 buffer");
        let env = env.expect("a terminated env section is known");
        assert_eq!(argv, vec!["/bin/true".to_string(), "--flag".to_string()]);
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn truncated_argv_is_none() {
        // argc claims 2 entries but only one argv string is present and
        // nothing (not even an env section) follows it.
        let bytes = procargs2(2, "/bin/true", 0, &["/bin/true"], b"");
        assert!(parse_args(&bytes).is_none());
    }

    #[test]
    fn omitted_environment_is_none_with_argv_known() {
        // Nothing at all follows argv: SIP can truncate the sysctl result
        // right there. argv must still come back; env is unknown, not empty.
        let bytes = procargs2(1, "/bin/true", 0, &["/bin/true"], b"");
        let (argv, env) = parse_args(&bytes).expect("argv should still parse");
        assert_eq!(argv, vec!["/bin/true".to_string()]);
        assert!(env.is_none(), "got {env:?}");
    }

    #[test]
    fn incomplete_env_is_none_with_argv_known() {
        // argv is complete, but the env bytes have no NUL at all: env is
        // unterminated, so it is unknown rather than failing the whole parse.
        let bytes = procargs2(1, "/bin/true", 0, &["/bin/true"], b"FOO=bar");
        let (argv, env) = parse_args(&bytes).expect("argv should still parse");
        assert_eq!(argv, vec!["/bin/true".to_string()]);
        assert!(env.is_none(), "got {env:?}");
    }

    #[test]
    fn single_nul_is_a_known_empty_environment() {
        // The envp array is terminated immediately (one empty string, no
        // entries before it), same as a genuinely empty environment; Apple
        // auxiliary strings follow but must not be picked up as entries.
        let bytes = procargs2(1, "/bin/true", 0, &["/bin/true"], b"\0ptr_munge=\0");
        let (_, env) = parse_args(&bytes).expect("well-formed procargs2 buffer");
        let env = env.expect("a terminated (even if empty) env section is known");
        assert!(env.is_empty(), "got {env:?}");
    }

    #[test]
    fn auxiliary_strings_after_the_environment_terminator_are_excluded() {
        // Real KERN_PROCARGS2 buffers end the envp array with an empty
        // string (the double-NUL after the last "K=V\0" entry), then append
        // Apple's own auxiliary strings (ptr_munge, main_stack, ...): those
        // are not environment variables and must not leak into the result.
        let mut bytes = procargs2(1, "/bin/true", 0, &["/bin/true"], b"FOO=bar\0");
        bytes.push(0); // envp terminator: empty string
        bytes.extend_from_slice(b"ptr_munge=\0main_stack=\0executable_file=0x1,0x2\0");
        let (_, env) = parse_args(&bytes).expect("well-formed procargs2 buffer");
        let env = env.expect("a terminated env section is known");
        assert_eq!(
            env.len(),
            1,
            "auxiliary strings must not be counted as environment entries, got {env:?}"
        );
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert!(!env.contains_key("ptr_munge"), "got {env:?}");
        assert!(!env.contains_key("main_stack"), "got {env:?}");
        assert!(!env.contains_key("executable_file"), "got {env:?}");
    }
}
