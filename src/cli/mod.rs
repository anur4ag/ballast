mod report;
pub use report::report;
#[cfg(test)]
mod tests;
mod top;

use crate::daemon::{
    Snapshot,
    files::Paths,
    ipc::{Client, Method, Reply, Response},
};
use std::io;
use std::time::Duration;

pub use top::run;

pub fn request(method: Method) -> io::Result<Response> {
    let response =
        Client::connect(&Paths::from_env()?, Duration::from_millis(750))?.request(method)?;
    if let Reply::Error { message } = &response.reply {
        return Err(io::Error::other(message.clone()));
    }
    Ok(response)
}

pub fn ps(json: bool) -> io::Result<()> {
    let response = request(Method::Ps)?;
    let Reply::Snapshot { snapshot } = &response.reply else {
        return Err(io::Error::other("unexpected daemon snapshot response"));
    };
    if json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        print!("{}", crate::attribution::format_ps(snapshot));
    }
    Ok(())
}

pub fn status(json: bool) -> io::Result<()> {
    let response = request(if json {
        Method::Status
    } else {
        Method::Snapshot
    })?;
    if json {
        if !matches!(response.reply, Reply::Status { .. }) {
            return Err(io::Error::other("unexpected daemon status response"));
        }
        println!("{}", serde_json::to_string(&response)?);
    } else if let Reply::Snapshot { snapshot } = response.reply {
        println!(
            "Ballast {} running (pid {})",
            snapshot.status.daemon_version, snapshot.status.pid
        );
        for line in summary(&snapshot) {
            println!("{line}");
        }
        println!(
            "tick {}: {:.3} ms CPU, {:.3} ms wall, {} ms interval; {}",
            snapshot.status.tick,
            snapshot.status.tick_cpu_ns as f64 / 1e6,
            snapshot.status.tick_wall_ns as f64 / 1e6,
            snapshot.status.tick_interval_ms,
            count(snapshot.status.process_count, "process", "processes")
        );
    } else {
        return Err(io::Error::other("unexpected daemon snapshot response"));
    }
    Ok(())
}

fn summary(snapshot: &Snapshot) -> Vec<String> {
    let status = &snapshot.status;
    let mut lines = vec![format!(
        "Pressure: {:?} | mode: {:?} | {} {} | {} held",
        status.pressure_level,
        status.mode,
        snapshot.frozen.len(),
        if matches!(status.mode, crate::daemon::files::Mode::Observe) {
            if snapshot.frozen.len() == 1 {
                "simulated freeze"
            } else {
                "simulated freezes"
            }
        } else {
            "frozen"
        },
        snapshot.held.len()
    )];
    if let Some(p) = &snapshot.pressure {
        lines.push(format!(
            "Memory: {} / {} | swap: {}",
            bytes(p.used_memory_bytes),
            bytes(p.total_memory_bytes),
            bytes(p.swap_used_bytes)
        ));
        if let Some(kernel) = p.kernel_pressure_level {
            lines.push(format!(
                "Kernel: {} | pageout: {} MiB/s | swapout: {} MiB/s",
                match kernel {
                    1 => "normal",
                    2 => "warn",
                    4 => "critical",
                    _ => "unknown",
                },
                number(
                    snapshot
                        .guardian
                        .as_ref()
                        .and_then(|n| n.pageout_mib_per_sec)
                ),
                number(
                    snapshot
                        .guardian
                        .as_ref()
                        .and_then(|n| n.swapout_mib_per_sec)
                )
            ));
        }
        if p.psi_some_avg10.is_some() || p.psi_full_avg10.is_some() {
            lines.push(format!(
                "Memory PSI (10s): some {}% | full {}%",
                number(p.psi_some_avg10),
                number(p.psi_full_avg10)
            ));
        }
    }
    if status.sample_discarded || snapshot.pressure.is_none() {
        lines.push("Pressure sample unavailable; displayed level is the last known level.".into());
    }
    if let Some(note) = &snapshot.guardian {
        lines.push(format!(
            "Guardian: {}{}",
            clean(&note.message),
            note.agent_memory_share
                .map(|share| format!(" (agent memory {:.1}%)", share * 100.0))
                .unwrap_or_default()
        ));
    }
    if !status.cleanup_pending.is_empty() {
        lines.push(format!("Cleanup pending: {}", {
            let handles = crate::attribution::WorkloadHandles::for_snapshot(snapshot);
            status
                .cleanup_pending
                .iter()
                .map(|id| {
                    if id.starts_with("internal:") {
                        "agent helpers"
                    } else {
                        handles.get(id)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }));
    }
    if let Some(error) = &status.last_error {
        lines.push(format!("Last error: {}", clean(error)));
    }
    lines
}

pub(crate) fn bytes(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "?".into();
    };
    if value >= 1 << 30 {
        format!("{:.1} GiB", value as f64 / (1u64 << 30) as f64)
    } else if value >= 1 << 20 {
        format!("{:.1} MiB", value as f64 / (1u64 << 20) as f64)
    } else if value >= 1 << 10 {
        format!("{:.1} KiB", value as f64 / (1u64 << 10) as f64)
    } else {
        format!("{value} B")
    }
}
fn number(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "?".into())
}
fn age(now: u64, since: u64) -> String {
    let seconds = now.saturating_sub(since) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}
// Process labels are untrusted terminal text, including ANSI escapes and bidi controls.
pub(crate) fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

pub fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}
