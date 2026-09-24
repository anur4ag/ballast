//! Regressions for the ticket 03 review findings: a stale lock holder
//! replacing the live daemon's socket (P1), and held-request cancellation
//! missed when unread pipelined bytes remain (P2).

use super::*;

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
    let server = Server::bind(home.0.clone()).expect("bind after the lock is released");
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
