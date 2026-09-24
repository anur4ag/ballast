//! Regressions for the ticket 03 review findings: a stale lock holder
//! replacing the live daemon's socket (P1), and held-request cancellation
//! missed when unread pipelined bytes remain (P2).
//!
//! Forking while a directory lock is held duplicates its open file description.
//! Serialize fixture spawns against lock-release tests; the handshake below proves why.

use super::*;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn with_spawn_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = crate::daemon::tests::SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

/// An owned raw fd pair from `pipe()`, closed on drop so a panic mid-test can't leak either end.
struct Pipe {
    read: i32,
    write: i32,
}
impl Pipe {
    fn new() -> Self {
        let mut fds = [0i32; 2];
        assert_eq!(
            unsafe { libc::pipe(fds.as_mut_ptr()) },
            0,
            "pipe() failed: {}",
            io::Error::last_os_error()
        );
        Self {
            read: fds[0],
            write: fds[1],
        }
    }
    /// Blocks up to `timeout` for one byte, via `poll` first so a broken fixture times out
    /// instead of hanging the test suite. `Ok(())` means the byte arrived.
    fn read_one(&self, timeout: Duration) -> io::Result<()> {
        let mut poll = libc::pollfd {
            fd: self.read,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let polled = unsafe { libc::poll(&mut poll, 1, millis) };
        if polled <= 0 || poll.revents & libc::POLLIN == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no byte arrived on the pipe within the deadline",
            ));
        }
        let mut byte = 0u8;
        let n = unsafe { libc::read(self.read, (&mut byte as *mut u8).cast(), 1) };
        if n != 1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn write_one(&self) -> io::Result<()> {
        let byte = 1u8;
        let n = unsafe { libc::write(self.write, (&byte as *const u8).cast(), 1) };
        if n != 1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

struct TempHome(Paths);
impl TempHome {
    fn new(tag: &str) -> Self {
        let path = std::path::PathBuf::from(format!("/tmp/blt-ipc-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp BALLAST_HOME");
        Self(Paths { base: path })
    }
}
impl std::ops::Deref for TempHome {
    type Target = Paths;
    fn deref(&self) -> &Paths {
        &self.0
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0.base);
    }
}

fn pair() -> (BufReader<UnixStream>, UnixStream) {
    let (server_side, client_side) = UnixStream::pair().expect("UnixStream::pair");
    (BufReader::new(server_side), client_side)
}

fn accept_within(listener: &UnixListener, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(_) => return,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "listener never accepted a connection"
                );
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}

#[test]
fn queued_returns_the_loops_reply_while_the_client_stays_connected_and_pipelines() {
    let (reader, mut client) = pair();
    let (requests_tx, requests_rx) = mpsc::sync_channel(4);

    let handle = thread::spawn(move || queued(Method::Gc, &reader, &requests_tx));
    let pending = requests_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("pending request");

    // A live client may pipeline a second request before the first is
    // answered; that alone must never look like a disconnect.
    client
        .write_all(b"{\"version\":1,\"method\":\"status\"}\n")
        .expect("write pipelined request");
    client.flush().expect("flush pipelined request");
    thread::sleep(Duration::from_millis(250)); // outlast a couple of queued()'s poll cycles
    assert!(
        !pending.cancelled.load(Ordering::Relaxed),
        "a live, connected client must not be cancelled just because it pipelined a request"
    );

    pending
        .reply
        .send(Response::new(Reply::Error {
            message: "done".into(),
        }))
        .expect("send reply");
    let response = handle.join().expect("queued() thread panicked");
    assert!(matches!(response.reply, Reply::Error { message } if message == "done"));
}

#[test]
fn queued_cancels_promptly_on_a_plain_disconnect() {
    let (reader, client) = pair();
    let (requests_tx, _requests_rx) = mpsc::sync_channel(4);
    drop(client);

    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = done_tx.send(queued(Method::Gc, &reader, &requests_tx));
    });
    let response = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("a plain disconnect must cancel promptly");
    assert!(matches!(response.reply, Reply::Error { .. }));
}

#[test]
fn queued_cancels_promptly_when_a_pipelined_second_request_is_left_unread_after_close() {
    let (reader, mut client) = pair();
    let (requests_tx, requests_rx) = mpsc::sync_channel(4);
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = done_tx.send(queued(Method::Gc, &reader, &requests_tx));
    });

    // Wait for queued() to actually be holding, then pipeline a second
    // request and close before anyone ever replies - the exact interleaving
    // the review reproduced.
    let pending = requests_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("pending request");
    client
        .write_all(b"{\"version\":1,\"method\":\"status\"}\n")
        .expect("write pipelined request");
    client.flush().expect("flush pipelined request");
    drop(client);

    let response = done_rx.recv_timeout(Duration::from_secs(2)).expect(
        "a disconnect with unread pipelined bytes must still cancel the held request promptly, \
         not stall toward the 310s ceiling",
    );
    assert!(matches!(response.reply, Reply::Error { .. }));
    assert!(
        pending.cancelled.load(Ordering::Relaxed),
        "queued() must mark the held request cancelled, not just return an error reply"
    );
}

#[test]
fn a_held_lock_on_the_home_directory_blocks_a_contender_across_a_run_directory_replacement() {
    let home = TempHome::new("lock-survives-run-replacement");
    home.prepare().expect("prepare BALLAST_HOME");

    // Serialized against every other `Command::spawn()` in this test binary: a fork racing
    // with our own lock fd being open would transiently keep it held via the forked child's
    // inherited duplicate, well after our own `drop(held)` below. See the module doc and
    // `a_forked_but_not_yet_exec_d_child_keeps_the_directory_lock_held` for the proof.
    let server = with_spawn_lock(|| {
        // Models "daemon A" pausing right after acquiring its lock, before bind.
        let held = lock(&home).expect("acquire lock");

        // Recovery/startup deletes and recreates run/ while A still holds its lock.
        std::fs::remove_dir_all(home.base.join("run")).expect("remove run/");
        std::fs::create_dir_all(home.base.join("run")).expect("recreate run/");

        assert!(
            Server::bind(home.0.clone()).is_err(),
            "a live lock on BALLAST_HOME must block a contender even after run/ is replaced"
        );

        let base_meta = std::fs::metadata(&home.base).expect("stat BALLAST_HOME");
        let held_meta = held.metadata().expect("stat held lock file");
        assert_eq!(
            (held_meta.dev(), held_meta.ino()),
            (base_meta.dev(), base_meta.ino()),
            "the held lock must be on BALLAST_HOME itself, not a separate lockfile inside run/"
        );

        drop(held);
        Server::bind(home.0.clone()).expect("bind after the lock is released")
    });
    let client = UnixStream::connect(home.socket()).expect("connect to the successor's socket");
    accept_within(&server.listener, Duration::from_secs(2));
    drop(client);
    drop(server);
}

#[test]
fn repair_rebinds_when_the_socket_pathname_is_replaced_by_a_foreign_listener() {
    let home = TempHome::new("repair-foreign-socket");
    home.prepare().expect("prepare BALLAST_HOME");
    let mut server = Server::bind(home.0.clone()).expect("bind server");

    fs::remove_file(home.socket()).expect("remove socket");
    let foreign = UnixListener::bind(home.socket()).expect("bind foreign listener");
    let foreign_ino = fs::metadata(home.socket())
        .expect("stat foreign socket")
        .ino();

    server.repair().expect("repair");

    let repaired_ino = fs::metadata(home.socket())
        .expect("stat socket after repair")
        .ino();
    assert_ne!(
        repaired_ino, foreign_ino,
        "repair() must rebind its own socket, not leave a foreign listener's inode in place"
    );

    let client = UnixStream::connect(home.socket()).expect("connect to the repaired socket");
    accept_within(&server.listener, Duration::from_secs(2));
    drop(client);
    drop(foreign);
}

#[test]
fn the_directory_lock_and_listener_fds_are_close_on_exec() {
    let home = TempHome::new("cloexec");
    home.prepare().expect("prepare BALLAST_HOME");
    let server = Server::bind(home.0.clone()).expect("bind server");

    for (name, fd) in [
        ("directory lock", server.lock.as_raw_fd()),
        ("listener", server.listener.as_raw_fd()),
    ] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            flags >= 0,
            "fcntl(F_GETFD) on the {name} fd failed: {}",
            io::Error::last_os_error()
        );
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "the {name} fd must be FD_CLOEXEC, so a forked-then-exec'd child releases its \
             inherited duplicate instead of holding the flock open indefinitely"
        );
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// CLOEXEC takes effect during exec, not fork. Pin the child before exec, then wait
// for userspace readiness after exec before checking that the inherited lock is gone.
#[test]
fn a_forked_but_not_yet_exec_d_child_keeps_the_directory_lock_held() {
    // Other fixture spawns must not inherit this test's lock.
    let _guard = crate::daemon::tests::SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let home = TempHome::new("fork-exec-window");
    home.prepare().expect("prepare BALLAST_HOME");
    let held = lock(&home).expect("acquire lock");

    let ready = Pipe::new();
    let go = Pipe::new();
    let ready_w = ready.write;
    let go_r = go.read;

    let spawn_thread = thread::spawn(move || {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf R; exec sleep 5"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(move || {
                // Only async-signal-safe calls after fork; timeout prevents an orphan on panic.
                let byte = 1u8;
                if libc::write(ready_w, (&byte as *const u8).cast(), 1) != 1 {
                    libc::_exit(125);
                }
                let mut poll = libc::pollfd {
                    fd: go_r,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if libc::poll(&mut poll, 1, 5000) <= 0 || poll.revents & libc::POLLIN == 0 {
                    libc::_exit(125);
                }
                let mut ack = 0u8;
                if libc::read(go_r, (&mut ack as *mut u8).cast(), 1) != 1 {
                    libc::_exit(125);
                }
                Ok(())
            });
        }
        command.spawn().map(ChildGuard)
    });

    ready
        .read_one(Duration::from_secs(2))
        .expect("child did not report reaching its pre_exec pause");

    drop(held);
    assert!(
        Server::bind(home.0.clone()).is_err(),
        "a forked-but-not-yet-exec'd child must keep the directory lock held even after the \
         original holder's own fd is dropped -- this is the ipc_tests.rs:155 flake's cause"
    );

    // spawn's exec-error pipe can close before other CLOEXEC descriptors. A byte
    // written by the new program proves that exec's descriptor cleanup has finished.
    go.write_one().expect("send go to the paused child");
    let mut child = spawn_thread
        .join()
        .expect("spawn thread panicked")
        .expect("spawn handshaken child");

    let mut ready = [0];
    child
        .0
        .stdout
        .as_mut()
        .expect("piped child stdout")
        .read_exact(&mut ready)
        .expect("read post-exec readiness");
    assert_eq!(ready, [b'R']);
    assert_eq!(
        child.0.try_wait().expect("try_wait"),
        None,
        "the child must still be alive post-exec, so the rebind below is attributable to \
         FD_CLOEXEC at exec, not to the process having exited"
    );
    assert!(
        Server::bind(home.0.clone()).is_ok(),
        "the lock must be acquirable again once the child has exec'd, even while still alive"
    );
}
