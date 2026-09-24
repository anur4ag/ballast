use super::*;
use crate::attribution::{AttributionSnapshot, MemorySummary, Workload, WorkloadClass};
use crate::daemon::files::Mode;
use crate::daemon::{ProcessChanges, Status};
use crate::guardian::Level;
use crate::platform::{Capabilities, ProcessIdentity};

// Two real workload ids whose SHA256 digests share their first six hex
// characters, found by brute-forcing a birthday collision over
// `w:agent:{n}:1700000000000000` with Python's hashlib (not mocked hashing).
const COLLIDING_A: &str = "w:agent:2401:1700000000000000";
const COLLIDING_B: &str = "w:agent:10520:1700000000000000";

#[test]
fn six_hex_handles_by_default() {
    let handles = WorkloadHandles::new(["alpha", "bravo", "charlie"]);
    assert_eq!(handles.get("alpha"), "8ed3f6");
    assert_eq!(handles.get("bravo"), "f144a6");
    assert_eq!(handles.get("charlie"), "b9dd96");
}

#[test]
fn handles_are_deterministic_independent_of_input_order() {
    let ids = ["alpha", "bravo", "charlie"];
    let reference = WorkloadHandles::new(ids);
    for order in [["charlie", "alpha", "bravo"], ["bravo", "charlie", "alpha"]] {
        let handles = WorkloadHandles::new(order);
        for id in ids {
            assert_eq!(handles.get(id), reference.get(id), "order: {order:?}");
        }
    }
}

#[test]
fn lengthens_only_the_colliding_ids() {
    let handles = WorkloadHandles::new([COLLIDING_A, COLLIDING_B, "alpha"]);
    assert_eq!(handles.get(COLLIDING_A), "484abad");
    assert_eq!(handles.get(COLLIDING_B), "484aba3");
    // Unaffected id keeps the default six-character handle.
    assert_eq!(handles.get("alpha"), "8ed3f6");
}

#[test]
fn get_falls_back_to_the_raw_id_when_unknown() {
    let handles = WorkloadHandles::new(["alpha"]);
    assert_eq!(handles.get("not-tracked"), "not-tracked");
}

#[test]
fn resolve_accepts_full_ids_even_when_a_handle_exists() {
    let handles = WorkloadHandles::new(["alpha", "bravo"]);
    assert_eq!(handles.resolve("alpha").unwrap(), "alpha");
}

#[test]
fn resolve_accepts_an_unambiguous_handle_prefix() {
    let handles = WorkloadHandles::new(["alpha", "bravo", "charlie"]);
    assert_eq!(handles.resolve("8ed3f6").unwrap(), "alpha");
}

#[test]
fn resolve_rejects_prefixes_shorter_than_six_chars() {
    let handles = WorkloadHandles::new(["alpha"]);
    let err = handles.resolve("8ed3f").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn resolve_reports_unknown_targets() {
    let handles = WorkloadHandles::new(["alpha"]);
    let err = handles.resolve("zzzzzz").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(
        err.to_string(),
        "workload not found; use ballast ps for handles"
    );
}

#[test]
fn resolve_rejects_an_ambiguous_six_char_prefix_with_candidates() {
    let handles = WorkloadHandles::new([COLLIDING_A, COLLIDING_B, "alpha"]);
    let err = handles.resolve("484aba").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let message = err.to_string();
    assert!(message.starts_with("ambiguous workload handle, candidates: "));
    assert!(message.contains("484abad"));
    assert!(message.contains("484aba3"));
}

#[test]
fn resolve_still_accepts_a_stale_lengthened_handle_once_its_collision_peer_is_gone() {
    // Only A remains tracked, so its displayed handle shrinks back to six chars...
    let handles = WorkloadHandles::new([COLLIDING_A]);
    assert_eq!(handles.get(COLLIDING_A), "484aba");
    // ...but a client that cached the seven-char handle from when B was still
    // around can still resolve it: `resolve` checks the full SHA256 prefix,
    // not just the currently displayed (possibly shortened) handle.
    assert_eq!(handles.resolve("484abad").unwrap(), COLLIDING_A);
    // B's old handle is not a prefix of A's hash, so it stays unresolvable.
    assert!(handles.resolve("484aba3").is_err());
}

fn snapshot_with(workload_ids: &[&str], cleanup_pending: &[&str]) -> Snapshot {
    Snapshot {
        status: Status {
            daemon_version: "test".into(),
            pid: 1,
            mode: Mode::Enforce,
            tick: 0,
            sampled_at_ms: 0,
            tick_interval_ms: 0,
            tick_cpu_ns: 0,
            tick_wall_ns: 0,
            sample_discarded: false,
            process_count: 0,
            pressure_level: Level::Normal,
            batch_running: false,
            cleanup_pending: cleanup_pending.iter().map(|s| (*s).to_owned()).collect(),
            last_error: None,
        },
        boot_id: "test".into(),
        capabilities: Capabilities {
            environment: false,
            listening_ports: false,
            memory_footprint: false,
            memory_psi: false,
            kernel_pressure: false,
            notifications: false,
            atomic_signals: false,
        },
        processes: Vec::new(),
        changes: ProcessChanges::default(),
        pressure: None,
        attribution: AttributionSnapshot {
            workloads: workload_ids
                .iter()
                .map(|id| Workload {
                    id: (*id).to_owned(),
                    agent_id: "agent".into(),
                    root: ProcessIdentity {
                        pid: 1,
                        start_time: 1,
                    },
                    label: (*id).to_owned(),
                    class: WorkloadClass::Batch,
                    first_seen_ms: 0,
                    detached_pgid: None,
                    memory: MemorySummary::default(),
                })
                .collect(),
            ..AttributionSnapshot::default()
        },
        frozen: Vec::new(),
        held: Vec::new(),
        guardian: None,
        today: Default::default(),
    }
}

#[test]
fn for_snapshot_keeps_pending_workloads_resolvable_after_they_exit() {
    // "exited" is only in status.cleanup_pending, not attribution.workloads
    // anymore, but its handle must stay stable and resolvable while pending.
    let snapshot = snapshot_with(&["alpha"], &["exited", "internal:agent"]);
    let handles = WorkloadHandles::for_snapshot(&snapshot);
    assert_ne!(handles.get("exited"), "exited");
    assert_eq!(handles.resolve(handles.get("exited")).unwrap(), "exited");
    // The synthetic "internal:*" pending key is not a resolvable workload id.
    assert_eq!(handles.get("internal:agent"), "internal:agent");
}
