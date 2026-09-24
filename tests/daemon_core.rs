//! Focused tests for daemon-internal mechanisms (ticket 03) that don't need
//! a running daemon process: `Observer` diffing/cadence, `Config::load`,
//! and `RotatingLog` rotation and decision records.

use ballast::daemon::Observer;
use ballast::daemon::files::{Config, Paths, RotatingLog};
use ballast::platform::{Process, ProcessIdentity};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// A throwaway `/tmp` directory, not the real `~/.ballast`.
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::path::PathBuf::from(format!(
            "/tmp/blt-core-{tag}-{:x}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn process(pid: i32, start_time: u64, exe: &str) -> Process {
    Process {
        identity: ProcessIdentity { pid, start_time },
        ppid: 1,
        pgid: pid,
        uid: 501,
        stopped: false,
        name: None,
        exe: Some(exe.into()),
        argv: None,
        metrics: None,
    }
}

// ---- Observer::diff -------------------------------------------------

#[test]
fn diff_reports_started_exec_changed_then_exited() {
    let mut observer = Observer::default();

    let started = observer.diff(&[process(100, 1, "/bin/a")]);
    assert_eq!(
        started.started,
        vec![ProcessIdentity {
            pid: 100,
            start_time: 1
        }]
    );
    assert!(started.exec_changed.is_empty() && started.exited.is_empty());

    let changed = observer.diff(&[process(100, 1, "/bin/b")]);
    assert_eq!(
        changed.exec_changed,
        vec![ProcessIdentity {
            pid: 100,
            start_time: 1
        }]
    );
    assert!(changed.started.is_empty() && changed.exited.is_empty());

    let exited = observer.diff(&[]);
    assert_eq!(
        exited.exited,
        vec![ProcessIdentity {
            pid: 100,
            start_time: 1
        }]
    );
}

#[test]
fn diff_treats_a_reused_pid_with_a_new_start_time_as_a_distinct_identity() {
    let mut observer = Observer::default();
    let old = ProcessIdentity {
        pid: 100,
        start_time: 1,
    };
    let new = ProcessIdentity {
        pid: 100,
        start_time: 2,
    };

    observer.diff(&[process(old.pid, old.start_time, "/bin/old")]);
    let gone = observer.diff(&[]);
    assert_eq!(gone.exited, vec![old]);

    // Same pid, later start_time: must be `started`, not folded into the
    // previous identity as an exec change.
    let reused = observer.diff(&[process(new.pid, new.start_time, "/bin/new")]);
    assert_eq!(reused.started, vec![new]);
    assert!(reused.exec_changed.is_empty());
}

// ---- Observer::begin_tick -------------------------------------------

#[test]
fn begin_tick_discards_first_sample_and_after_a_long_gap() {
    let mut observer = Observer::default();
    let now = Instant::now();
    let wall = SystemTime::now();

    assert!(observer.begin_tick(now, wall), "first tick has no baseline");
    assert!(
        !observer.begin_tick(
            now + Duration::from_millis(10),
            wall + Duration::from_millis(10)
        ),
        "a normal-sized gap must not be discarded"
    );

    let interval = observer.interval();
    let gap = interval * 6;
    assert!(
        observer.begin_tick(now + gap, wall + gap),
        "a gap over 5x the interval (e.g. sleep/wake) must be discarded"
    );
}

#[test]
fn begin_tick_discards_on_either_clock_jumping_independently() {
    let mut observer = Observer::default();
    let now = Instant::now();
    let wall = SystemTime::now();
    observer.begin_tick(now, wall);

    let interval = observer.interval();
    assert!(
        observer.begin_tick(now + interval, wall + interval * 6),
        "wall clock jumping alone must still discard"
    );
    observer.begin_tick(now + interval, wall + interval);
    assert!(
        observer.begin_tick(now + interval * 8, wall + interval * 2),
        "monotonic clock jumping alone must still discard"
    );
}

#[test]
fn set_fast_polling_switches_cadence_and_the_discard_threshold_with_it() {
    let mut observer = Observer::default();
    assert_eq!(observer.interval(), Duration::from_secs(1));

    observer.set_fast_polling(true);
    assert_eq!(observer.interval(), Duration::from_millis(250));

    let now = Instant::now();
    let wall = SystemTime::now();
    observer.begin_tick(now, wall);
    assert!(
        !observer.begin_tick(
            now + Duration::from_millis(300),
            wall + Duration::from_millis(300)
        ),
        "300ms is under 5x the 250ms fast interval"
    );
    assert!(
        observer.begin_tick(now + Duration::from_secs(2), wall + Duration::from_secs(2)),
        "2s is over 5x the 250ms fast interval"
    );
}

// ---- Config::load ------------------------------------------------------

fn paths(dir: &TempDir) -> Paths {
    Paths {
        base: dir.0.clone(),
    }
}

#[test]
fn config_load_defaults_when_no_file_is_present() {
    let dir = TempDir::new("cfg-default");
    let config = Config::load(&paths(&dir)).expect("load default config");
    assert!(matches!(config.mode, ballast::daemon::files::Mode::Enforce));
    assert_eq!(config.cleanup_grace_seconds, 30);
    assert!(config.recovery_sweep_markers.is_none());
    assert_eq!(config.log_max_bytes, 5 * 1024 * 1024);
    assert_eq!(config.log_rotations, 3);
}

#[test]
fn config_load_fills_defaults_for_fields_left_out() {
    let dir = TempDir::new("cfg-partial");
    std::fs::write(dir.0.join("config.toml"), "mode = \"observe\"\n").expect("write config.toml");
    let config = Config::load(&paths(&dir)).expect("load partial config");
    assert!(matches!(config.mode, ballast::daemon::files::Mode::Observe));
    assert_eq!(config.cleanup_grace_seconds, 30);
    assert_eq!(config.log_max_bytes, 5 * 1024 * 1024);
    assert_eq!(config.log_rotations, 3);
}

#[test]
fn config_load_rejects_invalid_toml_and_out_of_range_values() {
    let cases = [
        ("mode = 123\n", "wrong type for mode"),
        ("cleanup_grace_seconds = -1\n", "negative cleanup grace"),
        ("unknown_field = 1\n", "unknown field"),
        (
            "recovery_sweep_markers = [\"UNREGISTERED\"]\n",
            "unknown recovery marker",
        ),
        ("log_max_bytes = 0\n", "zero log_max_bytes"),
        ("log_rotations = 0\n", "log_rotations below range"),
        ("log_rotations = 11\n", "log_rotations above range"),
    ];
    for (contents, why) in cases {
        let dir = TempDir::new("cfg-invalid");
        std::fs::write(dir.0.join("config.toml"), contents).expect("write config.toml");
        assert!(Config::load(&paths(&dir)).is_err(), "should reject: {why}");
    }
}

// ---- RotatingLog ---------------------------------------------------------

#[test]
fn rotating_log_rotation_keeps_exactly_log_rotations_archives_in_order() {
    let dir = TempDir::new("log-rotate");
    // Each fixed-length record ("line-A\n" etc, 7 bytes) exactly fills one
    // file, so every write after the first forces a rotation.
    let config = Config {
        log_max_bytes: 7,
        log_rotations: 2,
        ..Config::load(&paths(&dir)).unwrap()
    };
    let path = dir.0.join("daemon.log");
    let mut log = RotatingLog::open(path.clone(), &config).expect("open log");

    for record in ["line-A", "line-B", "line-C", "line-D"] {
        log.write_line(record).expect("write");
    }

    let read = |suffix: &str| std::fs::read_to_string(format!("{}{suffix}", path.display()));
    assert_eq!(
        read("").unwrap().trim(),
        "line-D",
        "active file must hold the newest record"
    );
    assert_eq!(
        read(".1").unwrap().trim(),
        "line-C",
        "1st archive must hold the 2nd-newest"
    );
    assert_eq!(
        read(".2").unwrap().trim(),
        "line-B",
        "2nd archive must hold the 3rd-newest"
    );
    assert!(
        read(".3").is_err(),
        "with log_rotations = 2, a 3rd archive must not exist (line-A must be gone, not kept)"
    );
}

#[test]
fn rotating_log_decision_writes_a_timestamped_json_record() {
    let dir = TempDir::new("log-decision");
    let config = Config::load(&paths(&dir)).unwrap();
    let path = dir.0.join("decisions.jsonl");
    let mut log = RotatingLog::open(path.clone(), &config).expect("open log");

    log.decision("freeze", serde_json::json!({"pid": 123}))
        .expect("write decision");

    let contents = std::fs::read_to_string(&path).unwrap();
    let record: serde_json::Value = serde_json::from_str(contents.trim()).expect("valid JSON");
    assert_eq!(record["event"], "freeze");
    assert_eq!(record["details"]["pid"], 123);
    assert!(record["timestamp_ms"].as_u64().is_some());
}
