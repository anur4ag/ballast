use super::*;
use serde_json::json;
use std::sync::mpsc;

fn event(e: &mut Engine, at: u64, name: &str, details: Value) {
    e.apply(Message::Decision {
        at,
        event: name.into(),
        details,
    });
}
fn sample(e: &mut Engine, at: u64, level: Option<&str>, throttled: &[&str]) {
    e.apply(Message::Sample {
        at,
        observe: false,
        level: level.map(str::to_owned),
        frozen: vec![("private-workload".into(), 1024, true)],
        throttled: throttled.iter().map(|id| (*id).to_owned()).collect(),
    });
}
#[test]
fn recorded_outcomes_cover_every_metric_without_retaining_identifiers() {
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(Store::default());
    sample(&mut e, at, Some("elevated"), &[]);
    event(
        &mut e,
        at,
        "freeze",
        json!({"mode":"enforce","level":"elevated", "decision":{"workload_id":"private-workload","agent_kind":"claude"}}),
    );
    for i in 1..=30 {
        sample(
            &mut e,
            at + i * 1000,
            Some(if i < 15 { "critical" } else { "normal" }),
            &[],
        );
    }
    event(
        &mut e,
        at + 30_000,
        "resume",
        json!({"mode":"enforce","reason":"max_freeze","workload":{"workload_id":"private-workload"}}),
    );
    event(&mut e, at, "hold", json!({"mode":"enforce"}));
    event(&mut e, at, "hold", json!({"mode":"enforce"}));
    event(
        &mut e,
        at,
        "hold_completed",
        json!({"mode":"enforce","wait_ms":40999,"reason":"normal pressure"}),
    );
    event(
        &mut e,
        at,
        "hold_completed",
        json!({"mode":"enforce","wait_ms":999999,"reason":"max hold"}),
    );
    event(&mut e, at, "deny", json!({"mode":"enforce"}));
    event(
        &mut e,
        at,
        "service_reported",
        json!({"mode":"enforce","decision":{"count":2}}),
    );
    for (pid, memory, state) in [
        (101, Some(512), "ended"),
        (102, None, "ended"),
        (103, Some(999), "thinking"),
    ] {
        let process = json!({"pid":pid,"start_time":1});
        event(
            &mut e,
            at,
            "clean",
            json!({"mode":"enforce","decision":{"process":process,"signal":"Terminate","error":null,"memory_bytes":memory,"agent":{"state":state}}}),
        );
        event(
            &mut e,
            at,
            "clean",
            json!({"mode":"enforce","decision":{"process":process,"signal":"Kill","error":null,"memory_bytes":null,"agent":{"state":state}}}),
        );
        event(
            &mut e,
            at,
            "clean_reclaimed",
            json!({"mode":"enforce","decision":{"processes":[process]}}),
        );
    }
    let report = e.store.report(1, at).unwrap();
    let t = &report.totals.enforce;
    assert_eq!(
        (t.observed_ms, t.elevated_ms, t.critical_ms),
        (30000, 1000, 14000)
    );
    let f = &t.freezes_by_agent_kind["claude"];
    assert_eq!(
        (f.count, f.total_ms, f.longest_ms, f.peak_memory_bytes),
        (1, 30000, 30000, 1024)
    );
    assert_eq!(
        f.pressure_after_30s,
        BTreeMap::from([("elevated->normal".into(), 1)])
    );
    assert_eq!(
        (t.holds, t.median_wait(), t.timed_out_holds),
        (2, Some(170), 1)
    );
    assert_eq!(
        (
            t.reclaimed_processes,
            t.reclaimed_memory_bytes,
            t.reclaimed_memory_unknown
        ),
        (2, 512, 1)
    );
    assert_eq!(
        (t.services_left_running, t.kills_blocked, t.forced_resumes),
        (2, 1, 1)
    );
    let json = serde_json::to_string(&e.store).unwrap();
    assert!(!json.contains("private-workload") && !json.contains("\"processes\""));
    assert_eq!(
        serde_json::to_value(&report)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "days",
            "from_day",
            "hold_waits",
            "schema_version",
            "since_days",
            "through_day",
            "totals",
            "updated_at_ms"
        ]
    );
}
#[test]
fn local_midnight_splits_durations_prunes_and_restart_keeps_unknown_comparisons() {
    let midnight = next_midnight(crate::daemon::unix_ms());
    let mut e = Engine::new(Store::default());
    sample(&mut e, midnight - 1000, Some("critical"), &[]);
    event(
        &mut e,
        midnight - 1000,
        "freeze",
        json!({"level":"critical", "decision":{"workload_id":"private-workload","agent_kind":"codex"}}),
    );
    sample(&mut e, midnight + 1000, Some("critical"), &[]);
    assert_eq!(
        e.store.days[&day_offset(midnight - 1, 0)]
            .enforce
            .critical_ms,
        1000
    );
    assert_eq!(
        e.store.days[&day_offset(midnight, 0)].enforce.critical_ms,
        1000
    );
    for offset in -100..=0 {
        e.store
            .days
            .entry(day_offset(midnight, offset))
            .or_default();
    }
    e.store.prune(midnight);
    assert_eq!(e.store.days.len(), 90);
    let mut restarted = Engine::new(e.store);
    sample(&mut restarted, midnight + 60_000, Some("normal"), &[]);
    let t = restarted
        .store
        .report(7, midnight + 60_000)
        .unwrap()
        .totals
        .enforce;
    assert_eq!(t.critical_ms, 2000);
    assert_eq!(
        t.freezes_by_agent_kind["codex"].pressure_after_30s["critical->unknown"],
        1
    );
}
#[test]
fn invalid_samples_and_clock_gaps_do_not_invent_pressure_time() {
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(Store::default());
    sample(&mut e, at, Some("critical"), &[]);
    sample(&mut e, at + 6000, Some("critical"), &[]);
    sample(&mut e, at + 7000, None, &[]);
    sample(&mut e, at + 8000, Some("normal"), &[]);
    sample(&mut e, at + 5000, Some("normal"), &[]);
    assert!(e.store.days.is_empty());
}
#[test]
fn throttled_workload_ms_sums_overlapping_workloads_as_workload_seconds() {
    // Two workloads throttled at once must both keep accruing -- this is workload-seconds,
    // not deduplicated wall-clock time, so the total can (and here does) exceed elapsed time.
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(Store::default());
    sample(&mut e, at, Some("normal"), &["w1"]);
    sample(&mut e, at + 1000, Some("normal"), &["w1", "w2"]);
    sample(&mut e, at + 2000, Some("normal"), &["w1", "w2"]);
    sample(&mut e, at + 3000, Some("normal"), &["w2"]); // w1 released here
    let totals = &e.store.report(1, at + 3000).unwrap().totals.enforce;
    // w1: at..at+3000 = 3000ms (credited through the sample where it disappears).
    // w2: at+1000..at+3000 = 2000ms. 3000ms of wall clock, 5000ms of workload-seconds.
    assert_eq!(totals.throttled_workload_ms, 5000);
}
#[test]
fn throttled_workload_ms_does_not_extrapolate_across_a_crash_gap() {
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(Store::default());
    sample(&mut e, at, Some("normal"), &["w1"]);
    // A 100s gap between samples -- e.g. the daemon was down -- must not be invented as
    // throttled time, the same way a >5s pressure-level gap invents nothing above.
    sample(&mut e, at + 100_000, Some("normal"), &["w1"]);
    assert_eq!(
        e.store
            .report(1, at + 100_000)
            .unwrap()
            .totals
            .enforce
            .throttled_workload_ms,
        0,
        "a >5s gap between samples must not be extrapolated as throttled time"
    );
    // Accounting must resume normally once samples are close together again.
    sample(&mut e, at + 101_000, Some("normal"), &["w1"]);
    assert_eq!(
        e.store
            .report(1, at + 101_000)
            .unwrap()
            .totals
            .enforce
            .throttled_workload_ms,
        1000
    );
}
#[test]
fn observe_and_enforce_are_separate_and_old_schema_defaults() {
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(serde_json::from_str("{}").unwrap());
    for name in ["hold", "deny"] {
        event(&mut e, at, name, json!({"mode":"observe"}));
    }
    assert_eq!(e.store.report(7, at).unwrap().totals.observe.holds, 1);
    assert_eq!(
        e.store.report(7, at).unwrap().totals.enforce,
        Totals::default()
    );
    assert!(e.store.report(2, at).is_err());
    let old: Totals = serde_json::from_str("{\"holds\":2}").unwrap();
    assert_eq!(old.holds, 2);
    assert_eq!(old.kills_blocked, 0);
}
#[test]
fn stats_atomic_roundtrip_corruption_and_missing_file() {
    let dir = std::env::temp_dir().join(format!(
        "ballast-report-{}-{}",
        std::process::id(),
        crate::daemon::unix_ms()
    ));
    let paths = Paths { base: dir.clone() };
    paths.prepare().unwrap();
    assert!(read(&paths).unwrap().days.is_empty());
    let mut store = Store::default();
    store.totals(crate::daemon::unix_ms(), false).holds = 3;
    write(&paths, &store).unwrap();
    assert_eq!(read(&paths).unwrap().days, store.days);
    assert!(!dir.join("state/stats.json.tmp").exists());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(dir.join("state/stats.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::write(dir.join("state/stats.json"), b"bad json").unwrap();
    assert!(read_or_empty(&paths).days.is_empty());
    write(&paths, &Store::default()).unwrap();
    assert!(read(&paths).is_ok());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn local_calendar_respects_dst() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "report::tests::timezone_boundaries_child"])
        .env("TZ", "America/New_York")
        .env("BALLAST_REPORT_DST_TEST", "1")
        .status()
        .unwrap();
    assert!(status.success());
}
#[test]
fn timezone_boundaries_child() {
    if std::env::var_os("BALLAST_REPORT_DST_TEST").is_none() {
        return;
    }
    fn midnight(month: i32, day: i32) -> u64 {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 126;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_isdst = -1;
        unsafe { libc::mktime(&mut tm) as u64 * 1000 }
    }
    for (month, day, hours) in [(3, 8, 23), (11, 1, 25)] {
        let at = midnight(month, day);
        assert_eq!(next_midnight(at) - at, hours * 3600 * 1000);
        assert_eq!(day_offset(at, -1), day_offset(at - 1, 0));
        assert_eq!(day_offset(at, 1), day_offset(next_midnight(at), 0));
    }
}

#[test]
fn a_missing_frozen_workload_is_an_unknown_memory_sample() {
    let snapshot: Snapshot = serde_json::from_value(json!({
        "status":{"daemon_version":"test","pid":1,"mode":"enforce","tick":1,
            "sampled_at_ms":60000,"tick_interval_ms":1000,"tick_cpu_ns":0,
            "tick_wall_ns":0,"sample_discarded":false,"process_count":0},
        "boot_id":"test", "capabilities":{"environment":false,"listening_ports":false,
            "memory_footprint":false,"memory_psi":false,"kernel_pressure":false,
            "notifications":false,"atomic_signals":false},
        "processes":[],"changes":{"started":[],"exited":[],"exec_changed":[]},"pressure":null,
        "attribution":{"owners":[],"agents":[],"workloads":[],"processes":[]},
        "frozen":[{"workload_id":"missing","root":{"pid":1,"start_time":1},"processes":[],"frozen_at_ms":30000}]
    })).unwrap();
    let worker = Worker::with_writer(Store::default(), |_| Ok(())).unwrap();
    worker.recorder.record(Message::Decision { at: 30000, event: "freeze".into(), details: json!({"mode":"enforce","level":"critical","decision":{"workload_id":"missing","agent_kind":"claude"}}) });
    worker.sample(&snapshot);
    let engine = worker.recorder.engine.lock().unwrap();
    let totals = engine.store.report(1, 60000).unwrap().totals.enforce;
    let freeze = &totals.freezes_by_agent_kind["claude"];
    assert_eq!(
        (freeze.peak_memory_bytes, freeze.incomplete_memory_samples),
        (0, 1)
    );
}

#[test]
fn a_late_confirmed_exit_keeps_its_last_observed_memory() {
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(Store::default());
    let process = json!({"pid":101,"start_time":1});
    event(
        &mut e,
        at,
        "clean",
        json!({"mode":"enforce","decision":{"process":process,"signal":"Kill","error":null,"memory_bytes":512,"agent":{"state":"ended"}}}),
    );
    sample(&mut e, at + 700_000, Some("normal"), &[]);
    event(
        &mut e,
        at + 700_000,
        "clean_reclaimed",
        json!({"mode":"enforce","decision":{"processes":[process]}}),
    );
    let totals = e.store.report(1, at + 700_000).unwrap().totals.enforce;
    assert_eq!(
        (totals.reclaimed_processes, totals.reclaimed_memory_bytes),
        (1, 512)
    );
}

#[test]
fn idle_samples_coalesce_and_clean_shutdown_flushes_without_losing_totals() {
    let paths = Paths {
        base: std::env::temp_dir().join(format!("ballast-report-idle-{}", std::process::id())),
    };
    paths.prepare().unwrap();
    let worker_paths = paths.clone();
    let (written, writes) = mpsc::channel();
    let worker = Worker::with_writer(Store::default(), move |store| {
        write(&worker_paths, store)?;
        written.send(()).unwrap();
        Ok(())
    })
    .unwrap();
    let at = crate::daemon::unix_ms() / 3_600_000 * 3_600_000;
    worker.recorder.record(Message::Decision {
        at,
        event: "hold".into(),
        details: json!({"mode":"enforce"}),
    });
    // A decision is durable immediately, without waiting for another sample.
    writes.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        read(&paths)
            .unwrap()
            .report(1, at)
            .unwrap()
            .totals
            .enforce
            .holds,
        1
    );
    for tick in 0..=60 {
        worker.recorder.record(Message::Sample {
            at: at + tick * 1000,
            observe: false,
            level: Some("normal".into()),
            frozen: vec![],
            throttled: vec![],
        });
    }
    // Dropping the owner flushes even if a decision producer still holds a recorder.
    let producer = worker.recorder.clone();
    drop(worker);
    assert_eq!(
        writes.try_iter().count(),
        1,
        "idle samples must share one final write"
    );
    let totals = read(&paths)
        .unwrap()
        .report(1, at + 60_000)
        .unwrap()
        .totals
        .enforce;
    assert_eq!((totals.observed_ms, totals.holds), (60_000, 1));
    drop(producer);
    fs::remove_dir_all(paths.base).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn day_keys_follow_timezone_changes() {
    if std::env::var_os("BALLAST_REPORT_TZ_CHANGE_TEST").is_some() {
        // This subprocess runs only this test; no other thread accesses the environment.
        let at = 1_781_499_600_000; // 2026-06-15 05:00 UTC.
        assert_eq!(day_offset(at, 0), "2026-06-14");
        unsafe {
            std::env::set_var("TZ", "Asia/Tokyo");
        }
        assert_eq!(day_offset(at, 0), "2026-06-15");
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "report::tests::day_keys_follow_timezone_changes"])
        .env("TZ", "Pacific/Honolulu")
        .env("BALLAST_REPORT_TZ_CHANGE_TEST", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn stalled_writer_does_not_block_ticks_or_lose_cumulative_updates() {
    let (started, writing) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let (saved, stores) = mpsc::channel();
    let mut first = true;
    let worker = Worker::with_writer(Store::default(), move |store| {
        if first {
            first = false;
            started.send(()).unwrap();
            resume.recv().unwrap();
        }
        saved.send(store.clone()).unwrap();
        Ok(())
    })
    .unwrap();
    let at = crate::daemon::unix_ms() / 3_600_000 * 3_600_000;
    worker.recorder.record(Message::Decision {
        at,
        event: "hold".into(),
        details: json!({"mode":"enforce"}),
    });
    writing.recv_timeout(Duration::from_secs(5)).unwrap();
    let recorder = worker.recorder.clone();
    let (done, finished) = mpsc::channel();
    let ticks = std::thread::spawn(move || {
        for tick in 0..=1000 {
            recorder.record(Message::Sample {
                at: at + tick * 1000,
                observe: false,
                level: Some("normal".into()),
                frozen: vec![],
                throttled: vec![],
            });
            recorder.record(Message::Decision {
                at: at + tick * 1000,
                event: "hold".into(),
                details: json!({"mode":"enforce"}),
            });
        }
        done.send(()).unwrap();
    });
    let progressed = finished.recv_timeout(Duration::from_secs(5));
    // Always release the writer before asserting, so failure cannot strand a thread.
    release.send(()).unwrap();
    ticks.join().unwrap();
    drop(worker);
    assert!(progressed.is_ok(), "ticks blocked on stalled storage");
    let stores: Vec<_> = stores.try_iter().collect();
    assert_eq!(
        stores.len(),
        2,
        "only the newest cumulative value is pending"
    );
    let totals = stores
        .last()
        .unwrap()
        .report(1, at + 1_000_000)
        .unwrap()
        .totals
        .enforce;
    assert_eq!((totals.observed_ms, totals.holds), (1_000_000, 1002));
}
