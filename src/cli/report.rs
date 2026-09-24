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
    let mut out = format!(
        "Ballast report · {} through {} (local days)\n",
        report.from_day, report.through_day
    );
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
        for (kind, f) in &t.freezes_by_agent_kind {
            let action = if observe {
                format!("would have frozen {} workloads", f.count)
            } else {
                format!("freezes: {}", f.count)
            };
            out.push_str(&format!("  {kind} {action}\n    {}time: {} total · {} longest\n    Peak memory {}frozen: {}{}\n", if observe { "Simulated " } else { "Frozen " }, duration(f.total_ms), duration(f.longest_ms), if observe { "proposed " } else { "held " }, bytes(Some(f.peak_memory_bytes)), if f.incomplete_memory_samples > 0 { " (partial)" } else { "" }));
            for (pair, n) in &f.pressure_after_30s {
                out.push_str(&format!("    Pressure before -> after 30s: {pair}: {n}\n"));
            }
        }
        if observe {
            out.push_str(&format!(
                "  would have held {} heavy commands; no waits performed\n",
                t.holds
            ));
        } else {
            let median = t
                .median_wait()
                .map_or("unknown".into(), |v| format!("{v}s"));
            let worst = t
                .hold_wait_seconds
                .keys()
                .next_back()
                .map_or("unknown".into(), |v| format!("{v}s"));
            out.push_str(&format!("  Heavy commands held: {}\n    Wait: median {median} · worst {worst}\n    Timed out: {} · cancelled: {}\n", t.holds, t.timed_out_holds, t.cancelled_holds));
        }
        out.push_str(&format!("  Leftover processes reclaimed: {}\n    Memory in use when reclaimed: {} (last observed)\n    Processes with unknown memory: {}\n  Dev servers reported and left running: {}\n", t.reclaimed_processes, bytes(Some(t.reclaimed_memory_bytes)), t.reclaimed_memory_unknown, t.services_left_running));
        if observe {
            out.push_str(&format!(
                "  would have blocked {} cross-agent kills\n",
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
    let prefix = if observe {
        "today: would have "
    } else {
        "today: "
    };
    let mut parts = if observe {
        vec![format!("frozen {}", c.freezes), format!("held {}", c.holds)]
    } else {
        vec![
            format!(
                "{} freeze{}",
                c.freezes,
                if c.freezes == 1 { "" } else { "s" }
            ),
            format!("{} hold{}", c.holds, if c.holds == 1 { "" } else { "s" }),
        ]
    };
    if let Some(median) = c.median_wait_seconds {
        parts[1].push_str(&format!(" (median {median}s)"));
    }
    if !observe {
        parts.push(format!(
            "{} reclaimed",
            bytes(Some(c.reclaimed_memory_bytes))
        ));
    }
    parts.push(if observe {
        format!(
            "blocked {} kill{}",
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
