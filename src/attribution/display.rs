use super::ProcessRole;
use std::collections::HashMap;

/// Stable plain-text rows; commands are escaped to keep one process on one line.
pub fn format_ps(snapshot: &crate::daemon::Snapshot) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let view = &snapshot.attribution;
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
            agent.memory.bytes,
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
            workload.id,
            workload.agent_id.escape_default(),
            workload.class,
            workload.memory.bytes,
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
            "PROCESS {} start={} ppid={} pgid={} agent={} workload={} role={:?} memory={} exe={}",
            p.identity.pid,
            p.identity.start_time,
            p.ppid,
            p.pgid,
            a.and_then(|a| a.agent_id.as_deref())
                .unwrap_or("-")
                .escape_default(),
            a.and_then(|a| a.workload_id.as_deref()).unwrap_or("-"),
            a.map_or(ProcessRole::Unattributed, |a| a.role),
            p.metrics
                .map(|m| m.memory_bytes.to_string())
                .unwrap_or_else(|| "?".into()),
            p.exe.as_deref().unwrap_or("?").escape_default()
        )
        .unwrap();
    }
    out
}
