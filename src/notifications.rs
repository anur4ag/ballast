use crate::attribution::{Workload, WorkloadHandles};
use crate::daemon::Snapshot;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub(crate) struct Work {
    pub label: String,
    pub agent: String,
    pub handle: String,
    pub bytes: Option<u64>,
    pub ports: Vec<u16>,
}

impl Work {
    pub fn from_snapshot(snapshot: &Snapshot, workload: &Workload) -> Self {
        let mut ports: Vec<_> = snapshot
            .attribution
            .processes
            .iter()
            .filter(|p| p.workload_id.as_deref() == Some(&workload.id))
            .flat_map(|p| p.listening_ports.iter().flatten().copied())
            .collect();
        ports.sort_unstable();
        ports.dedup();
        Self {
            label: if workload.label == format!("pid {}", workload.root.pid) {
                "workload".into()
            } else {
                label(&workload.label)
            },
            agent: agent_name(
                snapshot
                    .attribution
                    .agents
                    .iter()
                    .find(|a| a.id == workload.agent_id)
                    .map_or("agent", |a| a.kind.as_str()),
            ),
            handle: WorkloadHandles::for_snapshot(snapshot)
                .get(&workload.id)
                .to_owned(),
            bytes: workload.memory.complete.then_some(workload.memory.bytes),
            ports,
        }
    }
}

pub(crate) fn agent_name(kind: &str) -> String {
    match kind {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        "cursor" => "Cursor".into(),
        "generic" | "unknown" | "agent" => "agent".into(),
        other => label(other),
    }
}

pub(crate) fn label(value: &str) -> String {
    let text = crate::cli::clean(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return "workload".into();
    }
    if text.chars().count() <= 40 {
        return text;
    }
    let prefix: String = text.chars().take(39).collect();
    let end = prefix.rfind(' ').unwrap_or(prefix.len());
    format!("{}…", &prefix[..end])
}

pub(crate) fn paused(work: &Work) -> String {
    format!(
        "Paused `{}` ({}) to free memory · resumes automatically",
        work.label, work.agent
    )
}

pub(crate) fn resumed(work: &Work, maximum: bool) -> String {
    if maximum {
        format!(
            "Resumed `{}` ({}) after 10 min · paused work is never held longer",
            work.label, work.agent
        )
    } else {
        format!(
            "Resumed `{}` ({}) · protected from pauses for 5 min",
            work.label, work.agent
        )
    }
}

pub(crate) fn cleanup(agent: &str, ended: bool, reclaimed: &[Work], services: &[Work]) -> String {
    let session = format!(
        "{}{} session",
        if ended {
            "an ended "
        } else if agent == "agent" {
            "an "
        } else {
            "a "
        },
        agent
    );
    let mut parts = Vec::new();
    if !reclaimed.is_empty() {
        let what = if reclaimed.len() == 1 {
            format!("a leftover `{}`", reclaimed[0].label)
        } else {
            format!("{} leftovers ({})", reclaimed.len(), names(reclaimed))
        };
        let memory = reclaimed
            .iter()
            .try_fold(0u64, |sum, w| Some(sum.saturating_add(w.bytes?)))
            .filter(|bytes| *bytes > 0)
            .map(|bytes| format!(" · {}", crate::cli::bytes(Some(bytes))))
            .unwrap_or_default();
        parts.push(format!("Reclaimed {what} from {session}{memory}"));
    }
    if !services.is_empty() {
        let what = if services.len() == 1 {
            format!("Dev server `{}`", services[0].label)
        } else {
            format!("{} dev servers ({})", services.len(), names(services))
        };
        let ports: std::collections::BTreeSet<_> =
            services.iter().flat_map(|s| s.ports.iter()).collect();
        let ports = if ports.is_empty() {
            "running".into()
        } else {
            format!(
                "on {}{}",
                ports
                    .iter()
                    .take(3)
                    .map(|p| format!(":{p}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if ports.len() > 3 { ", …" } else { "" }
            )
        };
        parts.push(format!(
            "{what}{} still {ports}",
            if reclaimed.is_empty() {
                format!(" from {session}")
            } else {
                String::new()
            }
        ));
        parts.push(if services.len() == 1 {
            format!("ballast stop {}", services[0].handle)
        } else {
            "ballast ps".into()
        });
    }
    parts.join(" · ")
}

fn names(work: &[Work]) -> String {
    let mut text = work
        .iter()
        .take(2)
        .map(|w| format!("`{}`", w.label))
        .collect::<Vec<_>>()
        .join(", ");
    if work.len() > 2 {
        text.push_str(&format!(", {} more", work.len() - 2));
    }
    text
}

pub(crate) fn sample() -> String {
    format!(
        "Sample: {}",
        paused(&Work {
            label: "npm test".into(),
            agent: "Claude".into(),
            handle: String::new(),
            bytes: None,
            ports: Vec::new()
        })
    )
}

pub(crate) fn unstopped(work: &Work, count: usize) -> String {
    format!(
        "{} could not be stopped · `{}` ({}){}",
        crate::cli::count(count, "leftover process", "leftover processes"),
        work.label,
        work.agent,
        if work.handle.is_empty() {
            String::new()
        } else {
            format!(" [{}]", work.handle)
        }
    )
}
