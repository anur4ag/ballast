use super::{age, bytes, clean, count, number};
use crate::daemon::{
    Snapshot,
    files::Paths,
    ipc::{Client, Method, Reply},
};
use crate::platform::ProcessIdentity;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub(super) struct Cpu {
    previous: HashMap<ProcessIdentity, u64>,
    sampled_at: u64,
    boot_id: String,
    pub values: HashMap<ProcessIdentity, f64>,
}
impl Cpu {
    pub fn update(&mut self, snapshot: &Snapshot) {
        if snapshot.status.sampled_at_ms == self.sampled_at && snapshot.boot_id == self.boot_id {
            return;
        }
        self.values.clear();
        let elapsed = snapshot.status.sampled_at_ms.checked_sub(self.sampled_at);
        let valid = snapshot.boot_id == self.boot_id
            && !snapshot.status.sample_discarded
            && elapsed.is_some_and(|ms| ms > 0 && ms <= 5000);
        let mut next = HashMap::new();
        for p in &snapshot.processes {
            if let Some(metrics) = p.metrics {
                if valid {
                    if let Some(delta) = self
                        .previous
                        .get(&p.identity)
                        .and_then(|old| metrics.cpu_time_ns.checked_sub(*old))
                    {
                        self.values.insert(
                            p.identity,
                            delta as f64 / (elapsed.unwrap() as f64 * 10_000.0),
                        );
                    }
                }
                if !snapshot.status.sample_discarded {
                    next.insert(p.identity, metrics.cpu_time_ns);
                }
            }
        }
        self.previous = next;
        self.sampled_at = snapshot.status.sampled_at_ms;
        self.boot_id.clone_from(&snapshot.boot_id);
    }
    fn total(&self, ids: impl Iterator<Item = ProcessIdentity>) -> String {
        let mut total = 0.0;
        let mut any = false;
        let mut partial = false;
        for id in ids {
            if let Some(value) = self.values.get(&id) {
                total += value;
                any = true;
            } else {
                partial = true;
            }
        }
        if any {
            format!("{total:.1}{}", if partial { "~" } else { "" })
        } else {
            "?".into()
        }
    }
}

static QUIT: AtomicBool = AtomicBool::new(false);
extern "C" fn request_quit(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

pub fn run() -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "ballast top needs a terminal; use ballast ps --json for scripts",
        ));
    }
    let paths = Paths::from_env()?;
    // Socket I/O never blocks keyboard handling, even when the daemon is unresponsive.
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("top-snapshot".into())
        .spawn(move || {
            let mut client = None;
            loop {
                let began = Instant::now();
                let mut result = (|| {
                    if client.is_none() {
                        client = Some(Client::connect(&paths, Duration::from_millis(750))?);
                    }
                    match client.as_mut().unwrap().request(Method::Top)?.reply {
                        Reply::Snapshot { snapshot } => Ok(snapshot),
                        Reply::Error { message } => Err(io::Error::other(message)),
                        _ => Err(io::Error::other("unexpected daemon response")),
                    }
                })();
                if result.is_err() {
                    client = None;
                    if crate::guardian::recovery::needs_recovery(&paths) {
                        result = Err(io::Error::other(
                            "frozen or throttled work remains; run ballast resume --all",
                        ));
                    }
                }
                let interval = result
                    .as_ref()
                    .map_or(1000, |s| s.status.tick_interval_ms.max(1000));
                match send.try_send(result) {
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                    _ => std::thread::sleep(
                        Duration::from_millis(interval).saturating_sub(began.elapsed()),
                    ),
                }
            }
        })?;
    QUIT.store(false, Ordering::Relaxed);
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        if unsafe { libc::signal(signal, request_quit as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(io::Error::last_os_error());
        }
    }
    let mut terminal = ratatui::try_init()?;
    let result = (|| {
        let mut snapshot: Option<Arc<Snapshot>> = None;
        let mut error = None;
        let mut cpu = Cpu::default();
        let mut scroll = 0;
        let mut redraw = true;
        let mut last_draw = Instant::now();
        let mut previous_view = None;
        while !QUIT.load(Ordering::Relaxed) {
            while let Ok(result) = receive.try_recv() {
                match result {
                    Ok(next) => {
                        cpu.update(&next);
                        snapshot = Some(next);
                        error = None;
                    }
                    Err(e) => error = Some(format!("Daemon unreachable: {e}. Retrying...")),
                }
                redraw = true;
            }
            if redraw || last_draw.elapsed() >= Duration::from_secs(1) {
                let size = terminal.size()?;
                let next = view(
                    snapshot.as_deref(),
                    &cpu,
                    error.as_deref(),
                    size.width,
                    crate::daemon::unix_ms(),
                );
                if previous_view
                    .as_ref()
                    .is_none_or(|(old, old_size, old_scroll)| {
                        old != &next || *old_size != size || *old_scroll != scroll
                    })
                {
                    terminal.draw(|frame| render(frame, &next, &mut scroll))?;
                    previous_view = Some((next, size, scroll));
                }
                redraw = false;
                last_draw = Instant::now();
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break;
                            }
                            KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                            KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                            KeyCode::PageDown | KeyCode::Char(' ') => {
                                scroll =
                                    scroll.saturating_add(terminal.size()?.height.saturating_sub(5))
                            }
                            KeyCode::PageUp => {
                                scroll =
                                    scroll.saturating_sub(terminal.size()?.height.saturating_sub(5))
                            }
                            KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                            KeyCode::End | KeyCode::Char('G') => scroll = u16::MAX,
                            _ => {}
                        }
                        redraw = true;
                    }
                    Event::Resize(_, _) => redraw = true,
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

// These accents exceed 4.5:1 on both black and white; text labels also convey state.
const NORMAL: Color = Color::Rgb(32, 134, 77);
const ELEVATED: Color = Color::Rgb(151, 111, 0);
const CRITICAL: Color = Color::Rgb(208, 68, 61);
const FROZEN: Color = Color::Rgb(50, 121, 186);
const HELD: Color = Color::Rgb(163, 90, 165);

fn heading(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into()).style(Style::default().add_modifier(Modifier::BOLD))
}
fn accent(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn cell(text: &str, width: usize, right: bool) -> String {
    let text = clean(text);
    let clipped = Line::from(text.as_str()).width() > width;
    let budget = width.saturating_sub(usize::from(clipped));
    let mut content = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let n = Line::from(grapheme).width();
        if used + n > budget {
            break;
        }
        content.push_str(grapheme);
        used += n;
    }
    let text = content;
    let suffix = if clipped && width > 0 { "…" } else { "" };
    let padding = " ".repeat(width.saturating_sub(used + suffix.len().min(1)));
    if right {
        format!("{padding}{text}{suffix}")
    } else {
        format!("{text}{suffix}{padding}")
    }
}

#[derive(PartialEq)]
struct View {
    header: Vec<Line<'static>>,
    lines: Vec<Line<'static>>,
}

fn meter(
    name: &str,
    used: Option<u64>,
    total: Option<u64>,
    width: usize,
    color: Color,
) -> Line<'static> {
    if total == Some(0) {
        return Line::from(format!("{name:<4}  no swap"));
    }
    let label = format!("{} / {}", bytes(used), bytes(total));
    let bars = width.saturating_sub(label.len() + 10).clamp(3, 40);
    let filled = used
        .zip(total)
        .filter(|(_, total)| *total > 0)
        .map(|(used, total)| {
            ((used.min(total) as f64 / total as f64) * bars as f64).round() as usize
        });
    let inside = match filled {
        Some(n) => vec![
            accent("|".repeat(n), color),
            Span::raw("·".repeat(bars - n)),
        ],
        None => vec![Span::raw(cell("?", bars, false))],
    };
    let mut spans = vec![Span::raw(format!("{name:<4} ["))];
    spans.extend(inside);
    spans.push(Span::raw(format!("] {label}")));
    Line::from(spans)
}

fn view(snapshot: Option<&Snapshot>, cpu: &Cpu, error: Option<&str>, width: u16, now: u64) -> View {
    let Some(s) = snapshot else {
        return View {
            header: vec![
                heading("BALLAST"),
                Line::from(accent(
                    error
                        .map(clean)
                        .unwrap_or_else(|| "Connecting to daemon…".into()),
                    CRITICAL,
                )),
            ],
            lines: vec![Line::from("Start with: ballast daemon")],
        };
    };
    let color = match s.status.pressure_level {
        crate::guardian::Level::Normal => NORMAL,
        crate::guardian::Level::Elevated => ELEVATED,
        crate::guardian::Level::Critical => CRITICAL,
    };
    let mut header = vec![Line::from(vec![
        Span::styled("BALLAST  ", Style::default().add_modifier(Modifier::BOLD)),
        accent(
            format!("{:?}", s.status.pressure_level).to_uppercase(),
            color,
        ),
        Span::raw(format!(
            " · {:?} · {} frozen · {} held",
            s.status.mode,
            s.frozen.len(),
            s.held.len()
        )),
    ])];
    if let Some(t) = s.guardian.as_ref().and_then(|n| n.throttle.as_ref()) {
        header.push(Line::from(format!(
            "CPU {:?} · {} {}",
            t.cpu_level,
            t.workloads.len(),
            if matches!(s.status.mode, crate::daemon::files::Mode::Observe) {
                "would throttle"
            } else {
                "throttled"
            }
        )));
    }
    let p = s.pressure.as_ref();
    let mem = meter(
        "MEM",
        p.and_then(|p| p.used_memory_bytes),
        p.and_then(|p| p.total_memory_bytes),
        if width >= 110 {
            (width as usize - 3) / 2
        } else {
            width as usize
        },
        color,
    );
    let swap = meter(
        "SWAP",
        p.and_then(|p| p.swap_used_bytes),
        p.and_then(|p| p.swap_total_bytes),
        if width >= 110 {
            (width as usize - 3) / 2
        } else {
            width as usize
        },
        HELD,
    );
    if width >= 110 {
        let padding = (width as usize / 2).saturating_sub(mem.width());
        let mut line = mem;
        line.spans.push(Span::raw(" ".repeat(padding)));
        line.spans.extend(swap.spans);
        header.push(line);
    } else {
        header.extend([mem, swap]);
    }
    if let Some(p) = p {
        let mut inputs = Vec::new();
        if let Some(kernel) = p.kernel_pressure_level {
            inputs.push(format!(
                "kernel {}",
                match kernel {
                    1 => "normal",
                    2 => "warn",
                    4 => "critical",
                    _ => "unknown",
                }
            ));
        }
        let some = s
            .guardian
            .as_ref()
            .and_then(|n| n.psi_some_percent)
            .or(p.psi_some_avg10);
        let full = s
            .guardian
            .as_ref()
            .and_then(|n| n.psi_full_percent)
            .or(p.psi_full_avg10);
        if some.is_some() || full.is_some() {
            inputs.push(format!(
                "PSI some {}% / full {}%",
                number(some),
                number(full)
            ));
        }
        if p.kernel_pressure_level.is_some() {
            inputs.push(format!(
                "pageout {} MiB/s · swapout {} MiB/s",
                number(s.guardian.as_ref().and_then(|n| n.pageout_mib_per_sec)),
                number(s.guardian.as_ref().and_then(|n| n.swapout_mib_per_sec))
            ));
        }
        header.push(Line::from(inputs.join(" · ")));
    }
    if let Some(error) = error {
        header.push(Line::from(accent(clean(error), CRITICAL)));
    } else if now.saturating_sub(s.status.sampled_at_ms) > 3000 {
        header.push(Line::from(accent(
            "STALE snapshot; waiting for a fresh sample",
            CRITICAL,
        )));
    } else if s.status.sample_discarded || s.pressure.is_none() {
        header.push(Line::from(accent(
            "Pressure unavailable; level is last known",
            ELEVATED,
        )));
    }
    if let Some(note) = &s.guardian {
        header.push(Line::from(format!("Guardian: {}", clean(&note.message))));
    }
    if let Some(error) = &s.status.last_error {
        header.push(Line::from(accent(
            format!("Last error: {}", clean(error)),
            CRITICAL,
        )));
    }
    let summary = super::report::summary(&s.today, s.status.mode, width, now);
    if !summary.is_empty() {
        header.push(Line::from(summary));
    }
    View {
        header,
        lines: lines(s, cpu, width, now),
    }
}

#[cfg(test)]
pub(super) fn draw(
    frame: &mut Frame,
    snapshot: Option<&Snapshot>,
    cpu: &Cpu,
    error: Option<&str>,
    scroll: &mut u16,
    now: u64,
) {
    render(
        frame,
        &view(snapshot, cpu, error, frame.area().width, now),
        scroll,
    );
}

fn render(frame: &mut Frame, view: &View, scroll: &mut u16) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let header = Paragraph::new(view.header.clone()).wrap(Wrap { trim: false });
    let header_height = (header.line_count(area.width) as u16).min(area.height.saturating_sub(3));
    frame.render_widget(header, Rect::new(0, 0, area.width, header_height));
    let mut body = view.lines.clone();
    let mut paragraph = Paragraph::new(body.clone()).wrap(Wrap { trim: false });
    let mut count = paragraph.line_count(area.width).min(u16::MAX as usize) as u16;
    let fleet = body
        .iter()
        .position(|line| line.to_string().starts_with("FLEET  "));
    let gaps = 1 + u16::from(fleet.is_some_and(|index| index > 0));
    if u32::from(header_height) + u32::from(count) + u32::from(gaps) + 3 <= u32::from(area.height) {
        if let Some(index) = fleet.filter(|index| *index > 0) {
            body.insert(index, Line::default());
        }
        body.insert(0, Line::default());
        paragraph = Paragraph::new(body).wrap(Wrap { trim: false });
        count += gaps;
    }
    let height = area.height.saturating_sub(header_height + 3).min(count);
    *scroll = (*scroll).min(count.saturating_sub(height));
    frame.render_widget(
        paragraph.scroll((*scroll, 0)),
        Rect::new(0, header_height, area.width, height),
    );
    let fit = |items: &[&str]| {
        let mut text = String::new();
        for item in items {
            let next = if text.is_empty() {
                (*item).to_owned()
            } else {
                format!("{text} · {item}")
            };
            if !text.is_empty() && Line::from(next.as_str()).width() > area.width as usize {
                break;
            }
            text = next;
        }
        Line::from(text)
    };
    let rows = format!(
        "rows {}-{} / {count}",
        scroll.saturating_add(1).min(count),
        scroll.saturating_add(height).min(count)
    );
    let footer = vec![
        Line::from("─".repeat(area.width as usize)),
        fit(&[
            "q quit",
            "j/k scroll",
            "PgUp/PgDn",
            "Home/End",
            "100%=1 core",
            "? unknown",
            "~ partial",
        ]),
        fit(&["ballast resume <id>|--all", "ballast stop <id>", &rows]),
    ];
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(
            0,
            header_height + height,
            area.width,
            area.height.saturating_sub(header_height + height),
        ),
    );
}

fn row(
    values: &[String],
    widths: &[usize],
    bold: bool,
    state_color: Option<Color>,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (text, width)) in values.iter().zip(widths).enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let value = cell(text, *width, (2..=4).contains(&i));
        spans.push(match state_color.filter(|_| i == 0) {
            Some(color) => accent(value, color),
            None => Span::raw(value),
        });
    }
    let mut line = Line::from(spans);
    if bold {
        line = line.style(Style::default().add_modifier(Modifier::BOLD));
    }
    line
}

fn lines(s: &Snapshot, cpu: &Cpu, width: u16, now: u64) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let handles = crate::attribution::WorkloadHandles::for_snapshot(s);
    let observe = matches!(s.status.mode, crate::daemon::files::Mode::Observe);
    if !s.frozen.is_empty() || !s.held.is_empty() {
        out.push(heading("PAUSED & WAITING"));
        for frozen in &s.frozen {
            let work = s
                .attribution
                .workloads
                .iter()
                .find(|w| w.id == frozen.workload_id);
            let agent = work.and_then(|w| s.attribution.agents.iter().find(|a| a.id == w.agent_id));
            let name = agent
                .map(|a| format!("{}/{}", a.kind, a.id))
                .unwrap_or_else(|| "unknown agent".into());
            let label = work.map_or("workload", |w| w.label.as_str());
            out.push(Line::from(vec![
                accent(
                    if observe {
                        "SIMULATED FREEZE  "
                    } else {
                        "FROZEN  "
                    },
                    FROZEN,
                ),
                Span::raw(format!(
                    "[{}] {}  {}  {}  {}",
                    handles.get(&frozen.workload_id),
                    clean(&name),
                    clean(label),
                    age(now, frozen.frozen_at_ms),
                    if observe {
                        "memory pressure; no signal sent"
                    } else {
                        "memory pressure"
                    }
                )),
            ]));
        }
        for held in &s.held {
            out.push(Line::from(vec![
                accent("HELD    ", HELD),
                Span::raw(format!(
                    "{}/{}  {}  {}  {}",
                    clean(&held.agent),
                    clean(&held.session_id),
                    clean(&held.label),
                    age(now, held.since_ms),
                    clean(&held.reason).replace("; waiting for admission", "")
                )),
            ]));
        }
    }
    if !s.status.cleanup_pending.is_empty() {
        out.push(Line::from(format!(
            "Cleanup pending: {}",
            clean(&s.status.cleanup_pending.join(", "))
        )));
    }
    out.push(heading(format!(
        "FLEET  {} · {} · {}",
        count(s.attribution.owners.len(), "owner", "owners"),
        count(s.attribution.agents.len(), "agent", "agents"),
        count(s.attribution.workloads.len(), "workload", "workloads")
    )));
    let wide = width >= 110;
    let widths = if wide {
        let id = s
            .attribution
            .agents
            .iter()
            .map(|a| a.id.as_str())
            .chain(s.attribution.workloads.iter().map(|w| handles.get(&w.id)))
            .map(|id| Line::from(clean(id)).width())
            .max()
            .unwrap_or(2)
            .clamp(2, 40);
        vec![10, 7, 7, 11, 7, width as usize - 48 - id, id]
    } else if width >= 64 {
        vec![8, 6, 7, 7, 6, width as usize - 39]
    } else {
        vec![]
    };
    if !widths.is_empty() {
        let headers = if wide {
            vec!["STATE", "CLASS", "CPU%", "MEM", "AGE", "LABEL", "ID"]
        } else {
            vec!["STATE", "CLASS", "CPU%", "MEM", "AGE", "WORKLOAD / ID"]
        };
        out.push(row(
            &headers.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            &widths,
            true,
            None,
        ));
    }
    let memory = |m: &crate::attribution::MemorySummary| {
        format!(
            "{}{}",
            bytes(Some(m.bytes))
                .replace(" GiB", "G")
                .replace(" MiB", "M"),
            if m.complete { "" } else { "~" }
        )
    };
    let mut agents: Vec<_> = s.attribution.agents.iter().collect();
    agents.sort_by_key(|a| (&a.owner_id, &a.id));
    let mut last_owner = None;
    for (agent_index, agent) in agents.iter().enumerate() {
        let has_sibling = agents
            .get(agent_index + 1)
            .is_some_and(|next| next.owner_id == agent.owner_id);
        let branch = if has_sibling { "├" } else { "└" };
        if last_owner != Some(&agent.owner_id) {
            let owner = s
                .attribution
                .owners
                .iter()
                .find(|o| Some(&o.id) == agent.owner_id.as_ref());
            let name = format!(
                "▾ {}",
                clean(
                    owner
                        .map(|o| o.name.as_deref().unwrap_or(&o.id))
                        .unwrap_or_else(|| agent.owner_id.as_deref().unwrap_or("unassigned"))
                )
            );
            if widths.is_empty() {
                out.push(heading(name));
            } else {
                let mut values = vec![String::new(); 5];
                values.push(name);
                if wide {
                    values.push(String::new());
                }
                out.push(row(&values, &widths, true, None));
            }
            last_owner = Some(&agent.owner_id);
        }
        let percent = cpu.total(
            s.attribution
                .processes
                .iter()
                .filter(|p| p.agent_id.as_deref() == Some(&agent.id))
                .map(|p| p.identity),
        );
        let mut values = vec![
            format!("{:?}", agent.state).to_lowercase(),
            "agent".into(),
            percent,
            memory(&agent.memory),
            "-".into(),
        ];
        if wide {
            values.extend([format!("{branch} {}", agent.kind), agent.id.clone()]);
        } else {
            values.push(format!("{branch} {} [{}]", agent.kind, agent.id));
        }
        if widths.is_empty() {
            out.push(heading(format!(
                "{branch} {} [{}] {}  {}% {}",
                clean(&agent.kind),
                values[0],
                clean(&agent.id),
                values[2],
                values[3]
            )));
        } else {
            out.push(row(&values, &widths, true, None));
        }
        let mut workloads: Vec<_> = s
            .attribution
            .workloads
            .iter()
            .filter(|w| w.agent_id == agent.id)
            .collect();
        workloads.sort_by_key(|w| &w.id);
        for (work_index, w) in workloads.iter().enumerate() {
            let prefix = format!(
                "{} {}",
                if has_sibling { "│" } else { " " },
                if work_index + 1 == workloads.len() {
                    "└"
                } else {
                    "├"
                }
            );
            let percent = cpu.total(
                s.attribution
                    .processes
                    .iter()
                    .filter(|p| p.workload_id.as_deref() == Some(&w.id))
                    .map(|p| p.identity),
            );
            let frozen = s.frozen.iter().any(|f| f.workload_id == w.id);
            let state = if frozen {
                if observe { "SIMULATED" } else { "FROZEN" }
            } else if s
                .guardian
                .as_ref()
                .and_then(|n| n.throttle.as_ref())
                .is_some_and(|t| t.workloads.iter().any(|t| t.workload_id == w.id))
            {
                if observe {
                    "WOULD THROTTLE"
                } else {
                    "THROTTLED"
                }
            } else {
                "running"
            };
            let mut values = vec![
                state.into(),
                format!("{:?}", w.class).to_lowercase(),
                percent,
                memory(&w.memory),
                age(now, w.first_seen_ms),
            ];
            if wide {
                values.extend([
                    format!("{prefix} {}", w.label),
                    handles.get(&w.id).to_owned(),
                ]);
            } else {
                values.push(format!("{prefix} [{}] {}", handles.get(&w.id), w.label));
            }
            if widths.is_empty() {
                out.push(Line::from(vec![
                    accent(
                        format!("{prefix} {state}"),
                        if frozen { FROZEN } else { NORMAL },
                    ),
                    Span::raw(format!(
                        " {} {}% {} {}",
                        values[1], values[2], values[3], values[4]
                    )),
                ]));
                out.push(Line::from(format!(
                    "{}  [{}] {}",
                    if has_sibling { "│" } else { " " },
                    handles.get(&w.id),
                    clean(&w.label)
                )));
            } else {
                out.push(row(&values, &widths, false, frozen.then_some(FROZEN)));
            }
        }
    }
    if s.attribution.agents.is_empty() {
        out.push(Line::from(
            "No agents detected. Waiting for agent activity.",
        ));
    }
    out
}

#[cfg(test)]
#[test]
fn unicode_columns_and_semantic_accents_remain_legible() {
    for text in ["界界界", "e\u{301}cole", "👩‍💻 build", "a\x1b[2J"] {
        for width in 0..12 {
            for right in [false, true] {
                assert_eq!(Line::from(cell(text, width, right)).width(), width);
            }
        }
    }
    for color in [NORMAL, ELEVATED, CRITICAL, FROZEN, HELD] {
        let Color::Rgb(r, g, b) = color else {
            panic!("explicit accent expected")
        };
        let linear = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
        assert!((luminance + 0.05) / 0.05 >= 4.5);
        assert!(1.05 / (luminance + 0.05) >= 4.5);
    }
}

#[cfg(test)]
#[test]
fn cpu_totals_keep_known_members_and_mark_partial() {
    let known = ProcessIdentity {
        pid: 1,
        start_time: 1,
    };
    let new_or_unreadable = ProcessIdentity {
        pid: 2,
        start_time: 1,
    };
    let mut cpu = Cpu::default();
    cpu.values.insert(known, 50.0);
    assert_eq!(cpu.total([known].into_iter()), "50.0");
    assert_eq!(cpu.total([known, new_or_unreadable].into_iter()), "50.0~");
    assert_eq!(cpu.total([new_or_unreadable].into_iter()), "?");
    assert_eq!(cpu.total(std::iter::empty()), "?");
    cpu.values.insert(known, 1000.0);
    assert_eq!(
        cell(&cpu.total([known, new_or_unreadable].into_iter()), 7, true),
        "1000.0~"
    );
}
