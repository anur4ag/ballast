use super::{age, bytes, clean, number, summary};
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
    text::Line,
    widgets::{Paragraph, Wrap},
};
use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

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
        let valid =
            !snapshot.status.sample_discarded && elapsed.is_some_and(|ms| ms > 0 && ms <= 5000);
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
    fn total(&self, ids: impl Iterator<Item = ProcessIdentity>) -> Option<f64> {
        let mut total = 0.0;
        let mut any = false;
        for id in ids {
            total += self.values.get(&id)?;
            any = true;
        }
        any.then_some(total)
    }
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
                            "frozen work remains; run ballast resume --all",
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
    let mut terminal = ratatui::try_init()?;
    let result = (|| {
        let mut snapshot: Option<Arc<Snapshot>> = None;
        let mut error = None;
        let mut cpu = Cpu::default();
        let mut scroll = 0;
        let mut redraw = true;
        let mut last_draw = Instant::now();
        let mut previous_view = None;
        loop {
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

fn heading(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into()).style(Style::default().add_modifier(Modifier::BOLD))
}
fn alert(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into()).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

#[derive(PartialEq)]
struct View {
    title: String,
    banner: Line<'static>,
    lines: Vec<Line<'static>>,
}
fn view(snapshot: Option<&Snapshot>, cpu: &Cpu, error: Option<&str>, width: u16, now: u64) -> View {
    let title = snapshot.map_or_else(
        || "BALLAST connecting...".into(),
        |s| {
            if width < 32 {
                format!(
                    "{:?} F{} H{}",
                    s.status.pressure_level,
                    s.frozen.len(),
                    s.held.len()
                )
            } else if width < 64 {
                format!(
                    "BALLAST {:?} F:{} H:{}",
                    s.status.pressure_level,
                    s.frozen.len(),
                    s.held.len()
                )
            } else {
                format!(
                    "BALLAST  {:?}  {:?}  {} frozen  {} held",
                    s.status.mode,
                    s.status.pressure_level,
                    s.frozen.len(),
                    s.held.len()
                )
            }
        },
    );
    let stale = snapshot.is_some_and(|s| now.saturating_sub(s.status.sampled_at_ms) > 3000);
    let banner = if let Some(error) = error {
        alert(clean(error))
    } else if stale {
        alert("STALE snapshot; waiting for a fresh daemon sample")
    } else {
        Line::from(if width >= 64 {
            "Live fleet | CPU: one core = 100% | ? unknown, ~ partial"
        } else {
            "Live | ? unknown, ~ partial"
        })
    };
    let lines = snapshot
        .map(|s| lines(s, cpu, width, now))
        .unwrap_or_else(|| vec![Line::from("Start with: ballast daemon")]);
    View {
        title,
        banner,
        lines,
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
    frame.render_widget(
        Paragraph::new(heading(view.title.clone())),
        Rect::new(0, 0, area.width, 1),
    );
    if area.height < 3 {
        return;
    }
    frame.render_widget(
        Paragraph::new(view.banner.clone()),
        Rect::new(0, 1, area.width, 1),
    );
    let body = Rect::new(0, 2, area.width, area.height.saturating_sub(3));
    let paragraph = Paragraph::new(view.lines.clone()).wrap(Wrap { trim: false });
    // line_count accounts for Unicode area.width and wrapping, so End reaches all details.
    let count = paragraph.line_count(body.width).min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(count.saturating_sub(body.height));
    frame.render_widget(paragraph.scroll((*scroll, 0)), body);
    let footer = if area.width >= 64 {
        format!(
            " q quit  j/k scroll  PgUp/PgDn  Home/End  |  rows {}-{} / {}",
            *scroll + 1,
            scroll.saturating_add(body.height).min(count),
            count
        )
    } else {
        "q quit  j/k scroll  g/G start/end".into()
    };
    frame.render_widget(
        Paragraph::new(heading(footer)),
        Rect::new(0, area.height - 1, area.width, 1),
    );
}

fn lines(snapshot: &Snapshot, cpu: &Cpu, width: u16, now: u64) -> Vec<Line<'static>> {
    let mut out: Vec<_> = summary(snapshot)
        .into_iter()
        .skip(1)
        .map(Line::from)
        .collect();
    if width < 64 {
        out.insert(
            0,
            Line::from(format!(
                "Mode: {:?} | CPU: one core = 100%",
                snapshot.status.mode
            )),
        );
    }
    if !snapshot.frozen.is_empty() || !snapshot.held.is_empty() {
        out.push(Line::default());
        out.push(heading("PAUSED & WAITING"));
        for frozen in &snapshot.frozen {
            out.push(alert(format!(
                "{} {}  for {}",
                if matches!(snapshot.status.mode, crate::daemon::files::Mode::Observe) {
                    "SIMULATED FREEZE"
                } else {
                    "FROZEN"
                },
                clean(&frozen.workload_id),
                age(now, frozen.frozen_at_ms)
            )));
            out.push(Line::from(
                if matches!(snapshot.status.mode, crate::daemon::files::Mode::Observe) {
                    "  Observe mode: no signal sent. Reason: memory pressure."
                } else {
                    "  Guardian: memory pressure. Resume: ballast resume <id>"
                },
            ));
        }
        for held in &snapshot.held {
            out.push(alert(format!(
                "HELD {}:{}  for {}",
                clean(&held.agent),
                clean(&held.session_id),
                age(now, held.since_ms)
            )));
            out.push(Line::from(format!("  Admission: {}", clean(&held.reason))));
            out.push(Line::from(format!("  {}", clean(&held.label))));
        }
    }
    out.push(Line::default());
    out.push(heading(format!(
        "FLEET  {} owners / {} agents / {} workloads",
        snapshot.attribution.owners.len(),
        snapshot.attribution.agents.len(),
        snapshot.attribution.workloads.len()
    )));
    let mut agents: Vec<_> = snapshot.attribution.agents.iter().collect();
    agents.sort_by_key(|a| (&a.owner_id, &a.id));
    let mut last_owner = None;
    for agent in agents {
        if last_owner != Some(&agent.owner_id) {
            let owner = snapshot
                .attribution
                .owners
                .iter()
                .find(|o| Some(&o.id) == agent.owner_id.as_ref());
            out.push(heading(format!(
                "OWNER {}",
                owner
                    .map(|o| clean(o.name.as_deref().unwrap_or(&o.id)))
                    .unwrap_or_else(|| agent
                        .owner_id
                        .as_deref()
                        .map(clean)
                        .unwrap_or_else(|| "unassigned".into()))
            )));
            last_owner = Some(&agent.owner_id);
        }
        out.push(heading(format!(
            "  {} [{}] {}",
            clean(&agent.kind),
            format!("{:?}", agent.state).to_lowercase(),
            clean(&agent.id)
        )));
        let used_cpu = cpu.total(
            snapshot
                .attribution
                .processes
                .iter()
                .filter(|p| p.agent_id.as_deref() == Some(&agent.id))
                .map(|p| p.identity),
        );
        out.push(Line::from(format!(
            "  CPU {}%  memory {}{}",
            number(used_cpu),
            bytes(Some(agent.memory.bytes)),
            if agent.memory.complete { "" } else { "~" }
        )));
        let mut workloads: Vec<_> = snapshot
            .attribution
            .workloads
            .iter()
            .filter(|w| w.agent_id == agent.id)
            .collect();
        workloads.sort_by_key(|w| &w.id);
        if !workloads.is_empty() && width >= 64 {
            out.push(heading(format!(
                "    {:<7} {:>7} {:>11}  WORKLOAD",
                "CLASS", "CPU%", "MEMORY"
            )));
        }
        for w in workloads {
            let percent = cpu.total(
                snapshot
                    .attribution
                    .processes
                    .iter()
                    .filter(|p| p.workload_id.as_deref() == Some(&w.id))
                    .map(|p| p.identity),
            );
            let memory = format!(
                "{}{}",
                bytes(Some(w.memory.bytes)),
                if w.memory.complete { "" } else { "~" }
            );
            let class = format!("{:?}", w.class).to_lowercase();
            let frozen = snapshot.frozen.iter().any(|f| f.workload_id == w.id);
            let state = if !frozen {
                ""
            } else if matches!(snapshot.status.mode, crate::daemon::files::Mode::Observe) {
                " [WOULD FREEZE]"
            } else {
                " [FROZEN]"
            };
            let row = if width >= 64 {
                format!(
                    "    {class:<7} {:>7} {memory:>11}  {}{state}",
                    number(percent),
                    clean(&w.label)
                )
            } else {
                format!("    {class} | {}% | {memory}{state}", number(percent))
            };
            out.push(if frozen { alert(row) } else { Line::from(row) });
            if width < 64 {
                out.push(Line::from(format!("    {}", clean(&w.label))));
            }
            out.push(Line::from(format!("    id: {}", clean(&w.id))));
        }
    }
    if snapshot.attribution.agents.is_empty() {
        out.push(Line::from(
            "No agents detected. Waiting for agent activity.",
        ));
    }
    out.push(Line::default());
    out.push(Line::from(
        "Controls: ballast resume <id>|--all   ballast stop <id>",
    ));
    out
}
