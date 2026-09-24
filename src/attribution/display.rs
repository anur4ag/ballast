use super::ProcessRole;
use std::collections::HashMap;

/// Stable plain-text rows; commands are escaped to keep one process on one line.
pub fn format_ps(snapshot: &crate::daemon::Snapshot) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let view = &snapshot.attribution;
    let handles = super::WorkloadHandles::for_snapshot(snapshot);
    for owner in &view.owners {
        writeln!(
            out,
            "OWNER {} {}",
            owner.id.escape_default(),
            owner.name.as_deref().unwrap_or("").escape_default()
        )
        .unwrap();
    }
    for agent in &view.agents {
        writeln!(
            out,
            "AGENT {} kind={} session={} owner={} root={} state={:?} memory={}{} cwd={}",
            agent.id.escape_default(),
            agent.kind.escape_default(),
            agent.session_id.as_deref().unwrap_or("-").escape_default(),
            agent.owner_id.as_deref().unwrap_or("-").escape_default(),
            agent
                .root
                .map(|r| r.pid.to_string())
                .unwrap_or_else(|| "-".into()),
            agent.state,
            crate::cli::bytes(Some(agent.memory.bytes)),
            if agent.memory.complete {
                ""
            } else {
                " (partial)"
            },
            agent.cwd.as_deref().unwrap_or("-").escape_default()
        )
        .unwrap();
    }
    for workload in &view.workloads {
        writeln!(
            out,
            "WORKLOAD {} agent={} class={:?} memory={}{} frozen={} label={}",
            handles.get(&workload.id),
            workload.agent_id.escape_default(),
            workload.class,
            crate::cli::bytes(Some(workload.memory.bytes)),
            if workload.memory.complete {
                ""
            } else {
                " (partial)"
            },
            snapshot.frozen.iter().any(|w| w.workload_id == workload.id),
            workload.label.escape_default()
        )
        .unwrap();
    }
    let assignments: HashMap<_, _> = view.processes.iter().map(|p| (p.identity, p)).collect();
    let mut processes: Vec<_> = snapshot.processes.iter().collect();
    processes.sort_unstable_by_key(|p| p.identity);
    for p in processes {
        let a = assignments.get(&p.identity);
        writeln!(
            out,
            "PROCESS {} start={} ppid={} pgid={} agent={} workload={} role={:?} memory={} cpu_ns={} exe={}",
            p.identity.pid,
            p.identity.start_time,
            p.ppid,
            p.pgid,
            a.and_then(|a| a.agent_id.as_deref())
                .unwrap_or("-")
                .escape_default(),
            a.and_then(|a| a.workload_id.as_deref()).map(|id| handles.get(id)).unwrap_or("-"),
            a.map_or(ProcessRole::Unattributed, |a| a.role),
            p.metrics
                .map(|m| crate::cli::bytes(Some(m.memory_bytes)))
                .unwrap_or_else(|| "?".into()),
            p.metrics.map(|m| m.cpu_time_ns.to_string()).unwrap_or_else(|| "?".into()),
            p.exe.as_deref().unwrap_or("?").escape_default()
        )
        .unwrap();
    }
    for frozen in &snapshot.frozen {
        writeln!(
            out,
            "FROZEN {} since_ms={} duration_ms={} mode={:?} reason=guardian_memory_pressure",
            handles.get(&frozen.workload_id),
            frozen.frozen_at_ms,
            snapshot
                .status
                .sampled_at_ms
                .saturating_sub(frozen.frozen_at_ms),
            snapshot.status.mode
        )
        .unwrap();
    }
    for held in &snapshot.held {
        writeln!(
            out,
            "HELD agent={} session={} since_ms={} wait_ms={} reason={} label={}",
            held.agent.escape_default(),
            held.session_id.escape_default(),
            held.since_ms,
            snapshot.status.sampled_at_ms.saturating_sub(held.since_ms),
            held.reason.escape_default(),
            held.label.escape_default()
        )
        .unwrap();
    }
    if let Some(note) = &snapshot.guardian {
        writeln!(
            out,
            "GUARDIAN kind={} message={}",
            note.kind.escape_default(),
            note.message.escape_default()
        )
        .unwrap();
    }
    for id in &snapshot.status.cleanup_pending {
        writeln!(
            out,
            "CLEANUP_PENDING {}",
            if id.starts_with("internal:") {
                "agent helpers"
            } else {
                handles.get(id)
            }
        )
        .unwrap();
    }
    out
}
