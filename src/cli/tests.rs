use super::*;
use crate::daemon::ipc::Response;
use crate::platform::{Process, ProcessIdentity, ProcessMetrics};
use ratatui::{Terminal, backend::TestBackend};

fn snapshot() -> Snapshot {
    serde_json::from_value(serde_json::json!({
        "status": {"daemon_version":"0.1.0", "pid":42,"mode":"enforce","tick":10,
            "sampled_at_ms":60000,"tick_interval_ms":1000,"tick_cpu_ns":1000000,
            "tick_wall_ns":2000000,"sample_discarded":false,"process_count":2,
            "pressure_level":"critical","batch_running":true,"cleanup_pending":["w-cleanup"],"last_error":null},
        "boot_id":"test", "capabilities":{"environment":true,"listening_ports":true,
            "memory_footprint":true,"memory_psi":false,"kernel_pressure":true,"notifications":false,"atomic_signals":false},
        "processes":[],"changes":{"started":[],"exited":[],"exec_changed":[]},
        "pressure":{"page_size":4096,"total_memory_bytes":17179869184u64,"used_memory_bytes":12884901888u64,
            "swap_used_bytes":1073741824,"swap_total_bytes":4294967296u64,"kernel_pressure_level":4},
        "attribution":{"owners":[{"id":"owner","name":"Test fleet"}],
            "agents":[{"id":"agent-a","kind":"codex","owner_id":"owner","session_id":"session-a",
                "state":"thinking","memory":{"bytes":2147483648u64,"complete":true,"growth_30s_bytes":0}}],
            "workloads":[{"id":"w-build","agent_id":"agent-a","root":{"pid":100,"start_time":1},
                "label":"synthetic build","class":"batch","first_seen_ms":1000,"detached_pgid":null,
                "memory":{"bytes":1073741824,"complete":false,"growth_30s_bytes":null}}],"processes":[]},
        "frozen":[{"workload_id":"w-build","root":{"pid":100,"start_time":1},"processes":[],"frozen_at_ms":30000}],
        "held":[{"agent":"claude","session_id":"session-b","label":"synthetic tests","reason":"memory pressure; waiting for admission","since_ms":45000}],
        "guardian":{"kind":"cooldown","message":"Waiting five seconds before another freeze decision.",
            "sampled_at_ms":60000,"agent_memory_share":null,"pageout_mib_per_sec":12.5,"swapout_mib_per_sec":300.0}
    })).unwrap()
}

fn render(s: &Snapshot, width: u16, height: u16, scroll: &mut u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| top::draw(f, Some(s), &top::Cpu::default(), None, scroll, 60000))
        .unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .all(|cell| cell.bg == ratatui::style::Color::Reset)
    );
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| {
            row.iter()
                .map(|c| c.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn layouts_wrap_without_losing_reasons_and_scroll_to_every_row() {
    let s = snapshot();
    for width in [20, 40, 63, 80, 120, 160] {
        let screen = render(&s, width, 100, &mut 0);
        assert!(screen.contains("FROZEN"), "{width}: {screen}");
        assert!(screen.contains("HELD"));
        assert!(screen.contains("Guardian:"));
        assert!(!screen.contains('\u{1b}'));
        let mut end = u16::MAX;
        let screen = render(&s, width, 12, &mut end);
        assert!(
            screen.contains("ballast stop") || width < 64,
            "{width}: {screen}"
        );
        assert!(end < u16::MAX);
    }
    for width in [80, 120, 160] {
        let wide = render(&s, width, 36, &mut 0);
        assert!(wide.contains("synthetic build"), "{wide}");
        assert!(wide.contains("w-build"));
        assert!(wide.contains("30s"));
        assert!(wide.contains("15s"));
        assert!(wide.contains("1 owner · 1 agent · 1 workload"));
        assert_eq!(wide.matches("STATE").count(), 1);
        assert!(!wide.contains("id:"));
        if width >= 110 {
            assert!(wide.contains("LABEL"));
        }
    }
    let mut observed = snapshot();
    observed.status.mode = crate::daemon::files::Mode::Observe;
    let screen = render(&observed, 120, 40, &mut 0);
    assert!(screen.contains("SIMULATED FREEZE"));
    assert!(screen.contains("no signal sent"));
    observed.pressure.as_mut().unwrap().swap_total_bytes = Some(0);
    assert!(render(&observed, 80, 40, &mut 0).contains("no swap"));
    assert_eq!(age(6000, 0), "6s");
    assert_eq!(age(65000, 0), "1m05s");
    assert_eq!(age(7_380_000, 0), "2h03m");
    assert_eq!(
        clean("a\x1b[2J\n\t\u{202e}界e\u{301}"),
        "a [2J   界e\u{301}"
    );
    for (width, height) in [(1, 1), (10, 2), (1, 3)] {
        render(&s, width, height, &mut 0);
    }
}

#[test]
fn json_envelopes_and_additive_snapshot_fields_are_stable() {
    let s = snapshot();
    let status = serde_json::to_value(Response::new(Reply::Status {
        status: s.status.clone(),
    }))
    .unwrap();
    assert_eq!(status["version"], 1);
    assert_eq!(status["type"], "status");
    assert_eq!(
        status
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["status", "type", "version"]
    );
    assert_eq!(
        status["status"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "batch_running",
            "cleanup_pending",
            "daemon_version",
            "last_error",
            "mode",
            "pid",
            "pressure_level",
            "process_count",
            "sample_discarded",
            "sampled_at_ms",
            "tick",
            "tick_cpu_ns",
            "tick_interval_ms",
            "tick_wall_ns"
        ]
    );
    let mut ps = serde_json::to_value(Response::new(Reply::Snapshot {
        snapshot: std::sync::Arc::new(s),
    }))
    .unwrap();
    assert_eq!(ps["type"], "snapshot");
    assert_eq!(ps["snapshot"]["held"][0]["since_ms"], 45000);
    assert_eq!(ps["snapshot"]["guardian"]["swapout_mib_per_sec"], 300.0);
    assert_eq!(
        ps["snapshot"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "attribution",
            "boot_id",
            "capabilities",
            "changes",
            "frozen",
            "guardian",
            "held",
            "pressure",
            "processes",
            "status"
        ]
    );
    ps["snapshot"].as_object_mut().unwrap().remove("held");
    ps["snapshot"].as_object_mut().unwrap().remove("guardian");
    ps["snapshot"]["pressure"]
        .as_object_mut()
        .unwrap()
        .remove("swap_total_bytes");
    let old: Response = serde_json::from_value(ps).unwrap();
    let Reply::Snapshot { snapshot } = old.reply else {
        panic!()
    };
    assert!(snapshot.held.is_empty());
    assert!(snapshot.guardian.is_none());
    assert!(
        snapshot
            .pressure
            .as_ref()
            .unwrap()
            .swap_total_bytes
            .is_none()
    );
}

#[test]
fn cpu_deltas_reset_on_gaps_missing_samples_and_pid_reuse() {
    let mut s = snapshot();
    s.processes.push(Process {
        identity: ProcessIdentity {
            pid: 100,
            start_time: 1,
        },
        ppid: 0,
        pgid: 100,
        uid: 1,
        stopped: false,
        name: None,
        exe: None,
        argv: None,
        metrics: Some(ProcessMetrics {
            memory_bytes: 1,
            cpu_time_ns: 1_000_000_000,
        }),
    });
    let mut cpu = top::Cpu::default();
    cpu.update(&s);
    assert!(cpu.values.is_empty());
    s.status.sampled_at_ms += 1000;
    s.processes[0].metrics.as_mut().unwrap().cpu_time_ns += 500_000_000;
    cpu.update(&s);
    assert_eq!(cpu.values[&s.processes[0].identity], 50.0);
    cpu.update(&s);
    assert_eq!(cpu.values[&s.processes[0].identity], 50.0);
    s.status.sampled_at_ms += 1000;
    s.processes[0].identity.start_time = 2;
    cpu.update(&s);
    assert!(cpu.values.is_empty());
    s.status.sample_discarded = true;
    s.status.sampled_at_ms += 1000;
    cpu.update(&s);
    assert!(cpu.values.is_empty());
    s.status.sample_discarded = false;
    s.status.sampled_at_ms += 1000;
    cpu.update(&s);
    assert!(cpu.values.is_empty());
    s.status.sampled_at_ms += 6000;
    cpu.update(&s);
    assert!(cpu.values.is_empty());
    s.status.sampled_at_ms += 1000;
    s.boot_id = "another-boot".into();
    s.processes[0].metrics.as_mut().unwrap().cpu_time_ns += 500_000_000;
    cpu.update(&s);
    assert!(
        cpu.values.is_empty(),
        "same numeric identity across boots must not produce a CPU delta"
    );
}

#[test]
fn top_projection_keeps_machine_totals_and_only_fleet_metrics() {
    let mut s = snapshot();
    for pid in [100, 200] {
        s.processes.push(Process {
            identity: ProcessIdentity { pid, start_time: 1 },
            ppid: 0,
            pgid: pid,
            uid: 1,
            stopped: false,
            name: None,
            exe: Some("fixture".into()),
            argv: Some(vec!["private".into()]),
            metrics: Some(ProcessMetrics {
                memory_bytes: 123,
                cpu_time_ns: 456,
            }),
        });
    }
    s.attribution
        .processes
        .push(crate::attribution::ProcessAttribution {
            identity: s.processes[0].identity,
            owner_id: None,
            agent_id: Some("agent-a".into()),
            workload_id: Some("w-build".into()),
            role: crate::attribution::ProcessRole::Workload,
            environment_known: true,
            listening_ports: None,
            ports_sampled_at_ms: None,
        });
    let projected = crate::daemon::ipc::top_snapshot(&s);
    assert_eq!(projected.status.process_count, 2);
    assert_eq!(projected.processes.len(), 1);
    assert_eq!(projected.processes[0].identity.pid, 100);
    assert!(projected.processes[0].argv.is_none());
    assert!(projected.processes[0].exe.is_none());
    assert_eq!(projected.processes[0].metrics.unwrap().cpu_time_ns, 456);
    assert_eq!(
        projected.pressure.unwrap().total_memory_bytes,
        s.pressure.unwrap().total_memory_bytes
    );
    assert!(s.processes[0].argv.is_some());
    assert_eq!(projected.held.len(), 1);
    assert_eq!(projected.frozen.len(), 1);
}
