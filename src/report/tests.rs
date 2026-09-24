use super::*;
use serde_json::json;

fn event(e: &mut Engine, at: u64, name: &str, details: Value) {
    e.apply(Message::Decision {
        at,
        event: name.into(),
        details,
    });
}
fn sample(e: &mut Engine, at: u64, level: Option<&str>) {
    e.apply(Message::Sample {
        at,
        observe: false,
        level: level.map(str::to_owned),
        frozen: vec![("private-workload".into(), 1024, true)],
    });
}
#[test]
fn recorded_outcomes_cover_every_metric_without_retaining_identifiers() {
    let at = crate::daemon::unix_ms();
    let mut e = Engine::new(Store::default());
    sample(&mut e, at, Some("elevated"));
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
    sample(&mut e, midnight - 1000, Some("critical"));
    event(
        &mut e,
        midnight - 1000,
        "freeze",
        json!({"level":"critical", "decision":{"workload_id":"private-workload","agent_kind":"codex"}}),
    );
    sample(&mut e, midnight + 1000, Some("critical"));
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
    sample(&mut restarted, midnight + 60_000, Some("normal"));
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
    sample(&mut e, at, Some("critical"));
    sample(&mut e, at + 6000, Some("critical"));
    sample(&mut e, at + 7000, None);
    sample(&mut e, at + 8000, Some("normal"));
    sample(&mut e, at + 5000, Some("normal"));
    assert!(e.store.days.is_empty());
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
    let (send, receive) = mpsc::channel();
    let worker = Worker {
        send,
        today: Arc::new(RwLock::new(Summary::default())),
    };
    worker.sample(&snapshot);
    let Message::Sample { frozen, .. } = receive.recv().unwrap() else {
        panic!("expected sample");
    };
    assert_eq!(frozen, [("missing".into(), 0, false)]);
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
    sample(&mut e, at + 700_000, Some("normal"));
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
