use super::{
    Snapshot, Status,
    files::{Paths, private_dir},
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};
use std::thread;
use std::time::{Duration, Instant};

pub const VERSION: u32 = 1;
const MAX_REQUEST: u64 = 64 * 1024;
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
pub type Published = Arc<RwLock<Arc<Snapshot>>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    #[serde(flatten)]
    pub method: Method,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Method {
    Status,
    Snapshot,
    Ps,
    Top,
    Resume { target: Option<String> },
    Stop { target: String },
    Gc,
    Hook { payload: serde_json::Value },
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    #[serde(flatten)]
    pub reply: Reply,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Cleanup {
        report: crate::cleanup::Report,
    },
    Hook {
        decision: crate::hooks::HookDecision,
    },
    Resumed {
        count: usize,
    },
    Status {
        status: Status,
    },
    Snapshot {
        snapshot: Arc<Snapshot>,
    },
    Error {
        message: String,
    },
}
impl Response {
    pub fn new(reply: Reply) -> Self {
        Self {
            version: VERSION,
            reply,
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Reply::Error {
            message: message.into(),
        })
    }
}

/// The loop can retain the reply sender for admission without blocking itself.
pub struct PendingRequest {
    pub method: Method,
    pub evidence: crate::hooks::HookEvidence,
    pub reply: mpsc::Sender<Response>,
    pub cancelled: Arc<AtomicBool>,
}

pub struct Server {
    paths: Paths,
    lock: File,
    listener: UnixListener,
    socket_id: (u64, u64),
}
impl Server {
    pub fn bind(paths: Paths) -> io::Result<Self> {
        let lock = lock(&paths)?;
        let (listener, socket_id) = bind(&paths, &lock)?;
        Ok(Self {
            paths,
            lock,
            listener,
            socket_id,
        })
    }
    pub fn run(
        mut self,
        published: Published,
        requests: SyncSender<PendingRequest>,
    ) -> io::Result<()> {
        let mut clients = Vec::with_capacity(64);
        let mut health_check = Instant::now();
        loop {
            if health_check.elapsed() >= Duration::from_secs(1) {
                self.repair()?;
                health_check = Instant::now();
            }
            clients.retain(|client: &thread::JoinHandle<()>| !client.is_finished());
            match self.listener.accept() {
                Ok((stream, _)) if clients.len() < 64 => {
                    let published = Arc::clone(&published);
                    let requests = requests.clone();
                    clients.push(
                        thread::Builder::new()
                            .name("ipc-client".into())
                            .stack_size(256 * 1024)
                            .spawn(move || {
                                let _ = serve(stream, published, requests);
                            })?,
                    );
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut poll = libc::pollfd {
                        fd: self.listener.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let result = unsafe { libc::poll(&mut poll, 1, 1000) };
                    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    fn repair(&mut self) -> io::Result<()> {
        check_owner(&self.paths, &self.lock)?;
        private_dir(&self.paths.base)?;
        private_dir(&self.paths.base.join("run"))?;
        if !socket_identity(&self.paths).is_ok_and(|id| id == self.socket_id) {
            (self.listener, self.socket_id) = bind(&self.paths, &self.lock)?;
        }
        Ok(())
    }
}
pub(crate) fn lock(paths: &Paths) -> io::Result<File> {
    // ponytail: deleting/recreating the entire BALLAST_HOME changes this inode and is out of scope.
    // Lock the stable base, so replacing run/ cannot create a second owner.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&paths.base)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other(format!(
            "daemon already running or lock unavailable: {}",
            io::Error::last_os_error()
        )));
    }
    check_owner(paths, &file)?;
    Ok(file)
}
fn check_owner(paths: &Paths, lock: &File) -> io::Result<()> {
    let current = fs::symlink_metadata(&paths.base)?;
    let held = lock.metadata()?;
    if !current.is_dir() || current.dev() != held.dev() || current.ino() != held.ino() {
        return Err(io::Error::other("daemon base directory was replaced"));
    }
    Ok(())
}
fn socket_identity(paths: &Paths) -> io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(paths.socket())?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::other("daemon socket path is not a socket"));
    }
    Ok((metadata.dev(), metadata.ino()))
}
fn bind(paths: &Paths, lock: &File) -> io::Result<(UnixListener, (u64, u64))> {
    check_owner(paths, lock)?;
    match fs::remove_file(paths.socket()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(paths.socket())?;
    fs::set_permissions(paths.socket(), fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok((listener, socket_identity(paths)?))
}

/// Kernel-supplied peer identity, never a PID claimed in a hook payload.
pub fn peer_pid(stream: &UnixStream) -> Option<i32> {
    #[cfg(target_os = "linux")]
    let (level, option, mut value) = (libc::SOL_SOCKET, libc::SO_PEERCRED, unsafe {
        std::mem::zeroed::<libc::ucred>()
    });
    #[cfg(target_os = "macos")]
    let (level, option, mut value) = (libc::SOL_LOCAL, libc::LOCAL_PEERPID, 0 as libc::pid_t);
    let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            level,
            option,
            std::ptr::addr_of_mut!(value).cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of_val(&value) {
        return None;
    }
    #[cfg(target_os = "linux")]
    let pid = value.pid;
    #[cfg(target_os = "macos")]
    let pid = value;
    (pid > 0).then_some(pid)
}

fn serve(
    stream: UnixStream,
    published: Published,
    requests: SyncSender<PendingRequest>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let peer = peer_pid(&stream);
    let mut reader = BufReader::new(stream);
    let mut line = Vec::with_capacity(4096);
    loop {
        if !read_frame(&mut reader, &mut line, MAX_REQUEST)? {
            return Ok(());
        }
        let response = match serde_json::from_slice::<Request>(&line) {
            Err(_) => Response::error("invalid request"),
            Ok(request) if request.version != VERSION => {
                Response::error("unsupported protocol version")
            }
            Ok(request) => match request.method {
                Method::Status => Response::new(Reply::Status {
                    status: published.read().unwrap().status.clone(),
                }),
                Method::Snapshot | Method::Ps | Method::Top => {
                    // Release the lock before serialization or socket writes.
                    let snapshot = Arc::clone(&published.read().unwrap());
                    Response::new(Reply::Snapshot { snapshot })
                }
                method => {
                    let evidence = if let Method::Hook { payload } = &method {
                        serde_json::from_value::<crate::hooks::HookRequest>(payload.clone())
                            .ok()
                            .map(|hook| {
                                let snapshot = Arc::clone(&published.read().unwrap());
                                crate::hooks::lookup(&hook, &snapshot, peer)
                            })
                            .unwrap_or_default()
                    } else {
                        Default::default()
                    };
                    queued(method, evidence, &reader, &requests)
                }
            },
        };
        write_message(reader.get_mut(), &response)?;
    }
}
fn queued(
    method: Method,
    evidence: crate::hooks::HookEvidence,
    reader: &BufReader<UnixStream>,
    requests: &SyncSender<PendingRequest>,
) -> Response {
    let (reply, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    if requests
        .try_send(PendingRequest {
            method,
            evidence,
            reply,
            cancelled: cancelled.clone(),
        })
        .is_err()
    {
        return Response::error("daemon request queue full");
    }
    let started = Instant::now();
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(response)
                if matches!(
                    response.reply,
                    Reply::Hook {
                        decision: crate::hooks::HookDecision::Hold
                    }
                ) =>
            {
                if write_message(reader.get_ref(), &response).is_err() {
                    cancelled.store(true, Ordering::Relaxed);
                    return Response::error("hook disconnected");
                }
            }
            Ok(response) => return response,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Response::error("daemon loop unavailable");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // A closed peer can still have unread pipelined data, so peek alone is insufficient.
        let mut poll = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN, // macOS needs a requested event to report hangup.
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut poll, 1, 0) };
        let hung_up = poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
            || (polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted);
        let mut byte = 0u8;
        let disconnected = hung_up
            || unsafe {
                libc::recv(
                    reader.get_ref().as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            } == 0;
        if disconnected || started.elapsed() > Duration::from_secs(310) {
            cancelled.store(true, Ordering::Relaxed);
            return Response::error("request cancelled or expired");
        }
    }
}
fn read_frame(reader: &mut impl BufRead, line: &mut Vec<u8>, limit: u64) -> io::Result<bool> {
    line.clear();
    let count = reader.take(limit + 1).read_until(b'\n', line)?;
    if count == 0 {
        return Ok(false);
    }
    if count as u64 > limit || line.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete or oversized IPC message",
        ));
    }
    Ok(true)
}
fn write_message(stream: &UnixStream, value: &impl Serialize) -> io::Result<()> {
    let mut writer = BufWriter::new(stream);
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub struct Client {
    reader: BufReader<UnixStream>,
}
impl Client {
    pub fn connect(paths: &Paths, timeout: Duration) -> io::Result<Self> {
        let stream = connect_timeout(&paths.socket(), timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }
    pub fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(Some(timeout))?;
        self.reader.get_ref().set_write_timeout(Some(timeout))
    }
    pub fn request(&mut self, method: Method) -> io::Result<Response> {
        self.send(method)?;
        self.receive()
    }
    pub fn send(&mut self, method: Method) -> io::Result<()> {
        write_message(
            self.reader.get_mut(),
            &Request {
                version: VERSION,
                method,
            },
        )
    }
    pub fn receive(&mut self) -> io::Result<Response> {
        let mut line = Vec::new();
        if !read_frame(&mut self.reader, &mut line, MAX_RESPONSE)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon disconnected",
            ));
        }
        let response: Response = serde_json::from_slice(&line)?;
        if response.version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported daemon protocol version",
            ));
        }
        Ok(response)
    }
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;

fn connect_timeout(path: &std::path::Path, timeout: Duration) -> io::Result<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) || timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid socket path or timeout",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (out, &byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = byte as _;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        // AF_UNIX EAGAIN means the backlog is full, not a pending connection.
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                timeout.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if ready == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon connect timed out",
            ));
        }
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
