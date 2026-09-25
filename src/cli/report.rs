use super::{bytes, request};
use crate::daemon::{
    files::{Mode, Paths},
    ipc::{Method, Reply},
};
use crate::report::{Counts, Report, Summary, Totals};
use std::io;

pub fn report(since: &str, json: bool) -> io::Result<()> {
    let days = since
        .trim_end_matches('d')
        .parse::<u16>()
        .map_err(io::Error::other)?;
    let report = match request(Method::Report { since_days: days }) {
        Ok(response) => match response.reply {
            Reply::Report { report } => *report,
            _ => return Err(io::Error::other("unexpected daemon report response")),
        },
        Err(_) => crate::report::read_or_empty(&Paths::from_env()?)
            .report(days, crate::daemon::unix_ms())?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", format(&report));
    }
    Ok(())
}
fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60)
    }
}
pub(super) fn format(report: &Report) -> String {
    let period = if report.from_day == report.through_day {
        format!("{} (today)", report.through_day)
    } else {
        format!(
            "{} through {} (local days)",
            report.from_day, report.through_day
        )
    };
    let mut out = format!("Ballast report · {period}\n");
    if report.days.is_empty() {
        out.push_str("No recorded activity in this period.\n");
        return out;
    }
    for (label, t, observe) in [
        ("Enforce", &report.totals.enforce, false),
        (
            "Observe mode (no signals sent)",
            &report.totals.observe,
            true,
        ),
    ] {
        if t == &Totals::default() {
            continue;
        }
        out.push_str(&format!(
            "\n{label}\n  Pressure: Elevated {} · Critical {} · sampled {}\n",
            duration(t.elevated_ms),
            duration(t.critical_ms),
            duration(t.observed_ms)
        ));
        let freezes: u64 = t.freezes_by_agent_kind.values().map(|f| f.count).sum();
        let kinds = t
            .freezes_by_agent_kind
            .iter()
            .filter(|(_, f)| f.count > 0)
            .map(|(kind, f)| format!("{} {}", super::clean(kind), f.count))
            .collect::<Vec<_>>()
            .join(", ");
        let breakdown = if kinds.is_empty() {
            String::new()
        } else {
            format!(" ({kinds})")
        };
        out.push_str(&format!(
            "  {}: {freezes}{breakdown}\n",
            if observe {
                "Would have frozen"
            } else {
                "Freezes"
            }
        ));
        if t.throttled_workload_ms > 0 {
            out.push_str(&format!(
                "  {}throttled workload-seconds: {:.1}\n",
                if observe { "Simulated " } else { "" },
                t.throttled_workload_ms as f64 / 1000.0
            ));
        }
        for (kind, f) in &t.freezes_by_agent_kind {
            let action = if observe {
                format!("would have frozen {} workloads", f.count)
            } else {
                format!("freezes: {}", f.count)
            };
            out.push_str(&format!("  {kind} {action}\n    {}time: {} total · {} longest\n    Peak memory {}frozen: {}{}\n", if observe { "Simulated " } else { "Frozen " }, duration(f.total_ms), duration(f.longest_ms), if observe { "proposed " } else { "held " }, bytes(Some(f.peak_memory_bytes)), if f.incomplete_memory_samples > 0 { " (partial)" } else { "" }));
            for (pair, n) in &f.pressure_after_30s {
                let pair = pair
                    .split("->")
                    .map(|level| match level {
                        "normal" => "Normal".into(),
                        "elevated" => "Elevated".into(),
                        "critical" => "Critical".into(),
                        "unknown" => "Unknown".into(),
                        _ => super::clean(level),
                    })
                    .collect::<Vec<String>>()
                    .join(" → ");
                out.push_str(&format!("    Pressure 30 s after freeze: {pair} ({n})\n"));
            }
        }
        if observe {
            out.push_str(&format!("  Would have held: {} heavy commands\n", t.holds));
        } else {
            let median = t
                .median_wait()
                .map_or("unknown".into(), |v| format!("{v}s"));
            let worst = t
                .hold_wait_seconds
                .keys()
                .next_back()
                .map_or("unknown".into(), |v| format!("{v}s"));
            out.push_str(&format!("  Heavy commands held: {}\n", t.holds));
            if t.holds > 0 || !t.hold_wait_seconds.is_empty() {
                out.push_str(&format!("    Wait: median {median} · worst {worst}\n    Timed out: {} · cancelled: {}\n", t.timed_out_holds, t.cancelled_holds));
            }
        }
        if t.reclaimed_processes == 0
            && t.services_left_running == 0
            && t.kills_blocked == 0
            && t.forced_resumes == 0
        {
            out.push_str("  No cleanups, dev-server reports, kill blocks or forced resumes.\n");
            continue;
        }
        out.push_str(&format!(
            "  Leftover processes reclaimed: {}\n",
            t.reclaimed_processes
        ));
        if t.reclaimed_processes > 0 {
            out.push_str(&format!("    Memory in use when reclaimed: {} (last observed)\n    Processes with unknown memory: {}\n", bytes(Some(t.reclaimed_memory_bytes)), t.reclaimed_memory_unknown));
        }
        out.push_str(&format!(
            "  Dev servers reported and left running: {}\n",
            t.services_left_running
        ));
        if observe {
            out.push_str(&format!(
                "  Would have blocked {} cross-agent kills\n",
                t.kills_blocked
            ));
        } else {
            out.push_str(&format!(
                "  Cross-agent kills blocked: {}\n",
                t.kills_blocked
            ));
        }
        out.push_str(&format!(
            "  {}resumes at the 10-minute cap: {}\n",
            if observe { "Simulated " } else { "Forced " },
            t.forced_resumes
        ));
    }
    out
}
pub(super) fn summary(summary: &Summary, mode: Mode, width: u16, now: u64) -> String {
    let empty = Counts::default();
    let c = if summary.day != crate::report::day_offset(now, 0) {
        &empty
    } else if matches!(mode, Mode::Observe) {
        &summary.observe
    } else {
        &summary.enforce
    };
    let observe = matches!(mode, Mode::Observe);
    let prefix = "today: ";
    let mut parts = Vec::new();
    if c.freezes > 0 {
        parts.push(if observe {
            format!("would have frozen {}", c.freezes)
        } else {
            format!(
                "{} freeze{}",
                c.freezes,
                if c.freezes == 1 { "" } else { "s" }
            )
        });
    }
    if c.holds > 0 {
        let mut hold = if observe {
            format!("would have held {}", c.holds)
        } else {
            format!("{} hold{}", c.holds, if c.holds == 1 { "" } else { "s" })
        };
        if let Some(median) = c.median_wait_seconds {
            hold.push_str(&format!(" (median {median}s)"));
        }
        parts.push(hold);
    }
    if c.reclaimed_memory_bytes > 0 {
        parts.push(format!(
            "{} reclaimed",
            bytes(Some(c.reclaimed_memory_bytes))
        ));
    }
    if c.reclaimed_processes > 0 && c.reclaimed_memory_bytes == 0 {
        parts.push(format!(
            "{} leftover{} reclaimed",
            c.reclaimed_processes,
            if c.reclaimed_processes == 1 { "" } else { "s" }
        ));
    }
    if c.services_left_running > 0 {
        parts.push(format!(
            "{} dev server{} reported",
            c.services_left_running,
            if c.services_left_running == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if c.forced_resumes > 0 {
        parts.push(if observe {
            format!("would have resumed {} at cap", c.forced_resumes)
        } else {
            format!(
                "{} forced resume{}",
                c.forced_resumes,
                if c.forced_resumes == 1 { "" } else { "s" }
            )
        });
    }
    if c.kills_blocked > 0 {
        parts.push(if observe {
            format!(
                "would have blocked {} kill{}",
                c.kills_blocked,
                if c.kills_blocked == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "{} kill{} blocked",
                c.kills_blocked,
                if c.kills_blocked == 1 { "" } else { "s" }
            )
        });
    }
    if parts.is_empty() && width >= 26 {
        return "today: no interventions yet".into();
    }
    while !parts.is_empty() {
        let line = format!("{prefix}{}", parts.join(" · "));
        if line.chars().count() <= usize::from(width) {
            return line;
        }
        parts.pop();
    }
    if width >= 17 {
        "today: see report".into()
    } else {
        String::new()
    }
}
