//! Regression tests for the shared `ProcessCache` in `list_processes`:
//! a watched identity always gets a full refresh; an expired deadline
//! recovers a stale exe even when unwatched. No real waits: an "expired"
//! deadline is forced by setting it into the past.

use super::*;
use std::time::{Duration, Instant};

fn self_identity(platform: &mut NativePlatform) -> ProcessIdentity {
    let pid = std::process::id() as i32;
    platform
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity.pid == pid)
        .expect("this test process must be listed")
        .identity
}

fn cached_entry(platform: &mut NativePlatform, pid: i32) -> &mut (Instant, Process) {
    platform
        .cache
        .0
        .get_mut(&pid)
        .expect("a prior scan must have cached this pid")
}

#[test]
fn watched_identity_recovers_missing_or_empty_argv_with_a_fresh_deadline() {
    for seeded_argv in [None, Some(vec![String::new()])] {
        let mut platform = NativePlatform::new().expect("NativePlatform::new");
        let identity = self_identity(&mut platform);

        let entry = cached_entry(&mut platform, identity.pid);
        entry.1.argv = seeded_argv;
        entry.0 = Instant::now() + Duration::from_secs(999); // unexpired: only "watched" should force the refresh

        let refreshed = platform
            .list_processes(&HashSet::from([identity]), &HashSet::new())
            .expect("list_processes")
            .into_iter()
            .find(|p| p.identity == identity)
            .expect("this test process must still be listed");
        let argv = refreshed
            .argv
            .expect("a watched identity must recover argv even with an unexpired cache deadline");
        assert!(
            argv.iter().any(|arg| !arg.is_empty()),
            "recovered argv must be the real, nonempty cmdline, got {argv:?}"
        );
    }
}

#[test]
fn expired_deadline_recovers_a_stale_cached_exe_even_unwatched() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let identity = self_identity(&mut platform);
    let real_exe = cached_entry(&mut platform, identity.pid).1.exe.clone();

    let entry = cached_entry(&mut platform, identity.pid);
    entry.1.exe = Some("/not/the/real/exe".to_string());
    entry.0 = Instant::now() - Duration::from_secs(1);

    let refreshed = platform
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity == identity)
        .expect("this test process must still be listed");
    assert_eq!(
        refreshed.exe, real_exe,
        "an expired cache deadline must recover the real exe, not just stop being the fake one"
    );
}

#[test]
fn expired_deadline_also_recovers_a_stale_cached_uid() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let identity = self_identity(&mut platform);
    let real_uid = cached_entry(&mut platform, identity.pid).1.uid;

    let entry = cached_entry(&mut platform, identity.pid);
    entry.1.uid = real_uid.wrapping_add(1);
    entry.0 = Instant::now() - Duration::from_secs(1);

    let refreshed = platform
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity == identity)
        .expect("this test process must still be listed");
    assert_eq!(
        refreshed.uid, real_uid,
        "an expired cache deadline must also restore the real uid, not just the exe"
    );
}

#[test]
fn watched_identity_recovers_a_corrupted_exe_and_uid_with_a_fresh_deadline() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let identity = self_identity(&mut platform);
    let (real_exe, real_uid) = {
        let entry = cached_entry(&mut platform, identity.pid);
        (entry.1.exe.clone(), entry.1.uid)
    };

    let entry = cached_entry(&mut platform, identity.pid);
    entry.1.exe = Some("/not/the/real/exe".to_string());
    entry.1.uid = real_uid.wrapping_add(1);
    entry.0 = Instant::now() + Duration::from_secs(999); // unexpired: only "watched" should force this

    let refreshed = platform
        .list_processes(&HashSet::from([identity]), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity == identity)
        .expect("this test process must still be listed");
    assert_eq!(refreshed.exe, real_exe, "watched must recover the real exe");
    assert_eq!(
        refreshed.uid, real_uid,
        "a watched, changed exe must also refresh uid, even under an unexpired deadline"
    );
}

#[test]
fn expired_deadline_recovers_a_stale_but_nonempty_cached_argv() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let identity = self_identity(&mut platform);
    let real_argv = cached_entry(&mut platform, identity.pid).1.argv.clone();

    let entry = cached_entry(&mut platform, identity.pid);
    entry.1.argv = Some(vec!["not-the-real-argv".to_string()]);
    entry.0 = Instant::now() - Duration::from_secs(1);

    let refreshed = platform
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity == identity)
        .expect("this test process must still be listed");
    assert_eq!(
        refreshed.argv, real_argv,
        "an expired deadline must not keep a stale cached argv just because it is nonempty"
    );
}

#[test]
fn watched_selection_recovers_the_live_identity_for_a_reused_pid() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let real_identity = self_identity(&mut platform);

    // Simulate a PID-reuse artifact: the cache still holds an old process
    // for this pid, under a different (stale) start_time.
    let stale_identity = ProcessIdentity {
        pid: real_identity.pid,
        start_time: real_identity.start_time.wrapping_add(1),
    };
    let entry = cached_entry(&mut platform, real_identity.pid);
    entry.1.identity = stale_identity;
    entry.0 = Instant::now() + Duration::from_secs(999); // unexpired: only being selected should force this

    let refreshed = platform
        .list_processes(&HashSet::from([real_identity]), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity.pid == real_identity.pid)
        .expect("this test process must still be listed");
    assert_eq!(
        refreshed.identity, real_identity,
        "watching the live identity for a reused pid must return the live identity on the \
         first scan, not the stale one cached under the same pid"
    );
}

#[test]
fn metrics_selection_recovers_the_live_identity_and_metrics_for_a_reused_pid() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let real_identity = self_identity(&mut platform);

    let stale_identity = ProcessIdentity {
        pid: real_identity.pid,
        start_time: real_identity.start_time.wrapping_add(1),
    };
    let entry = cached_entry(&mut platform, real_identity.pid);
    entry.1.identity = stale_identity;
    entry.0 = Instant::now() + Duration::from_secs(999);

    let refreshed = platform
        .list_processes(&HashSet::new(), &HashSet::from([real_identity]))
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity.pid == real_identity.pid)
        .expect("this test process must still be listed");
    assert_eq!(
        refreshed.identity, real_identity,
        "selecting the live identity for metrics on a reused pid must return the live identity \
         on the first scan"
    );
    assert!(
        refreshed.metrics.is_some(),
        "the live identity selected for metrics must receive sampled metrics on the same scan"
    );
}

#[test]
fn unexpired_deadline_keeps_a_stale_cached_exe_when_unwatched() {
    let mut platform = NativePlatform::new().expect("NativePlatform::new");
    let identity = self_identity(&mut platform);

    let entry = cached_entry(&mut platform, identity.pid);
    entry.1.exe = Some("/not/the/real/exe".to_string());
    entry.0 = Instant::now() + Duration::from_secs(999);

    let refreshed = platform
        .list_processes(&HashSet::new(), &HashSet::new())
        .expect("list_processes")
        .into_iter()
        .find(|p| p.identity == identity)
        .expect("this test process must still be listed");
    assert_eq!(
        refreshed.exe.as_deref(),
        Some("/not/the/real/exe"),
        "an unexpired, unwatched cache entry must be served as-is, proving the cache is real"
    );
}
