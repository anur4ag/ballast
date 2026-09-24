use super::plan::Plan;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    widgets::{Paragraph, Widget, Wrap},
};
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

static QUIT: AtomicBool = AtomicBool::new(false);
extern "C" fn quit(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}
struct Session {
    alternate: bool,
    signals: Vec<(i32, libc::sighandler_t)>,
}
impl Session {
    fn new(alternate: bool) -> io::Result<Self> {
        QUIT.store(false, Ordering::Relaxed);
        let mut session = Self {
            alternate,
            signals: vec![],
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let previous = unsafe { libc::signal(signal, quit as *const () as libc::sighandler_t) };
            if previous == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            session.signals.push((signal, previous));
        }
        terminal::enable_raw_mode()?;
        if alternate {
            execute!(io::stdout(), EnterAlternateScreen)?;
        }
        Ok(session)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.alternate {
            let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        }
        let _ = terminal::disable_raw_mode();
        for (signal, previous) in &self.signals {
            unsafe {
                libc::signal(*signal, *previous);
            }
        }
    }
}
fn key() -> io::Result<Option<KeyEvent>> {
    while !QUIT.load(Ordering::Relaxed) {
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(None);
                    }
                    return Ok(Some(key));
                }
            }
        }
    }
    Ok(None)
}

pub(super) fn plan_text(plan: &Plan, home: &Path) -> String {
    let mut text = if plan.operation == "install" {
        "Ballast keeps your machine responsive while coding agents work.\n".into()
    } else {
        "Uninstall Ballast\n".to_string()
    };
    let found: Vec<_> = plan
        .detected_agents
        .iter()
        .map(|a| {
            if a == "claude" {
                "Claude Code"
            } else {
                "Codex"
            }
        })
        .collect();
    text += &format!(
        "Found: {}\n\nBallast will:\n",
        if found.is_empty() {
            "No supported agents detected".into()
        } else {
            found.join("   ")
        }
    );
    for (index, item) in plan.items.iter().enumerate() {
        text += &format!(
            "  {}. [{}] {}\n",
            index + 1,
            if item.selected { "x" } else { " " },
            item.purpose
        );
        for file in &item.files {
            text += &format!("     {}\n", file.path.display());
            if let Some(backup) = &file.backup {
                text += &format!(
                    "     Backup: {}\n",
                    backup.file_name().unwrap().to_string_lossy()
                );
            } else if file.before.is_none() && file.after.is_some() {
                text += "     Creates this file\n";
            }
        }
        if plan.operation == "install" && matches!(item.id.as_str(), "claude" | "codex") {
            text += if item.id == "claude" {
                "     + PreToolUse (Bash, Monitor): holds heavy commands under memory pressure\n"
            } else {
                "     + PreToolUse (Bash): holds heavy commands under memory pressure\n"
            };
            text += "     + PostToolUse (Bash): explains port collisions\n     + 4 lifecycle events: shows agent state in ballast top\n";
        }
        if matches!(item.id.as_str(), "claude" | "codex") {
            text += &match item.existing_hooks_kept {
                0 => "     Keeps all your other settings\n".into(),
                1 => "     Keeps your 1 other hook and all other settings\n".into(),
                count => format!("     Keeps your {count} other hooks and all other settings\n"),
            };
        }
        if !item.detail.is_empty() {
            text += &format!("     {}\n", item.detail);
        }
        if !item.changed {
            text += if plan.operation == "uninstall" && item.id == "service" {
                "     Service already removed; checks for anything still paused.\n"
            } else {
                "     Nothing needs changing.\n"
            };
        }
        if plan.operation == "install" && item.id == "codex" {
            text += "     Approval needed: open Codex /hooks.\n";
        }
    }
    if plan.operation == "install" {
        if plan.items.iter().any(|i| i.id == "service" && !i.selected) {
            text += "\nWARNING: Nothing works without a running Ballast service.\n";
        }
        text += "\nNever sends data anywhere, kills your own apps, or kills a running agent.\nUndo anytime: ballast uninstall\n";
    }
    text.replace(&format!("{}/", home.display()), "~/")
        .split('\n')
        .map(crate::cli::clean)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn wrapped(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut prefix = line.len() - line.trim_start_matches(' ').len();
        for marker in ["+ ", "Backup: "] {
            if line[prefix..].starts_with(marker) {
                prefix += marker.len();
                break;
            }
        }
        if let Some((marker, _)) = line.split_once("] ") {
            if marker
                .trim_start()
                .split_once(". [")
                .is_some_and(|(number, _)| number.parse::<usize>().is_ok())
            {
                prefix = marker.len() + 2;
            }
        }
        let indent = prefix.max(2).min(width.saturating_sub(1) as usize);
        let body_width = width - indent as u16;
        let paragraph = Paragraph::new(&line[prefix..]).wrap(Wrap { trim: false });
        let height = paragraph.line_count(body_width).min(u16::MAX as usize) as u16;
        let mut buffer = Buffer::empty(Rect::new(0, 0, body_width, height.max(1)));
        paragraph.render(buffer.area, &mut buffer);
        for (row_index, row) in buffer.content.chunks(body_width as usize).enumerate() {
            let content = row.iter().map(|cell| cell.symbol()).collect::<String>();
            let leading = if row_index == 0 {
                line[..prefix].to_owned()
            } else {
                " ".repeat(indent)
            };
            lines.push(
                format!("{leading}{}", content.trim_end())
                    .trim_end()
                    .to_owned(),
            );
        }
    }
    lines
}

pub(super) fn print_text(text: &str) {
    let width = terminal::size().map_or(80, |(w, _)| w);
    let safe = text
        .split('\n')
        .map(crate::cli::clean)
        .collect::<Vec<_>>()
        .join("\n");
    let emphasize = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    for mut line in wrapped(&safe, width) {
        if emphasize {
            if line.trim_start().starts_with("Approval needed:") {
                line = format!("\x1b[1m{line}\x1b[22m");
            } else {
                for command in [
                    "ballast top",
                    "ballast uninstall",
                    "ballast install",
                    " /hooks",
                ] {
                    line = line.replace(command, &format!("\x1b[1m{command}\x1b[22m"));
                }
            }
        }
        println!("{line}");
    }
}

pub(super) fn print_plan(plan: &Plan, home: &Path) {
    print_text(&plan_text(plan, home));
}

pub(super) fn diff(plan: &Plan) -> String {
    let mut text = String::new();
    for item in plan.items.iter().filter(|i| i.selected) {
        text += &format!("{}\n", item.purpose);
        for file in &item.files {
            text += &file.diff;
        }
        if !item.changed {
            text += if plan.operation == "uninstall" && item.id == "service" {
                "No service file change; recovery still runs.\n"
            } else {
                "Nothing to change.\n"
            };
        }
    }
    text
}

pub(super) fn confirm(plan: &mut Plan, home: &Path) -> io::Result<bool> {
    loop {
        print_plan(plan, home);
        println!(
            "\nContinue? [Y]es / {}[d]iff / [n]o",
            if plan.operation == "install" {
                "[c]hoose / "
            } else {
                ""
            }
        );
        io::stdout().flush()?;
        let input = {
            let _session = Session::new(false)?;
            key()?
        };
        match input.map(|k| k.code) {
            Some(KeyCode::Enter | KeyCode::Char('y' | 'Y')) => return Ok(true),
            Some(KeyCode::Char('d' | 'D')) => {
                if !view(plan, false)? {
                    return Ok(false);
                }
            }
            Some(KeyCode::Char('c' | 'C')) if plan.operation == "install" => {
                if !view(plan, true)? {
                    return Ok(false);
                }
            }
            Some(KeyCode::Esc | KeyCode::Char('n' | 'N')) | None => return Ok(false),
            _ => {}
        }
    }
}

fn view(plan: &mut Plan, choose: bool) -> io::Result<bool> {
    let _session = Session::new(true)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut cursor = 0;
    let mut scroll = 0;
    loop {
        let text = if choose {
            let mut text = "Choose what Ballast changes\n\n".to_string();
            for (index, item) in plan.items.iter().enumerate() {
                text += &format!(
                    "{} [{}] {}\n",
                    if cursor == index { ">" } else { " " },
                    if item.selected { "x" } else { " " },
                    item.purpose
                );
            }
            if plan.items.iter().any(|i| i.id == "service" && !i.selected) {
                text += "\nWARNING: Nothing works without a running Ballast service.\n";
            }
            text
        } else {
            diff(plan)
        };
        // JSON and paths may contain terminal controls; never send them as terminal commands.
        let text: String = text
            .chars()
            .map(|c| {
                if c == '\n' || c == '\t' || !c.is_control() {
                    c
                } else {
                    '�'
                }
            })
            .collect();
        terminal.draw(|frame| {
            let area = frame.area();
            let body = Rect {
                height: area.height.saturating_sub(2),
                ..area
            };
            let paragraph = Paragraph::new(text.as_str()).wrap(Wrap { trim: false });
            scroll = scroll.min(
                paragraph
                    .line_count(area.width)
                    .saturating_sub(body.height as usize)
                    .min(u16::MAX as usize) as u16,
            );
            frame.render_widget(paragraph.scroll((scroll, 0)), body);
            let footer = if choose {
                "Arrows: move  Space: toggle  Enter: confirm  Esc: back"
            } else {
                "Arrows / PgUp / PgDn: scroll  Enter / Esc: back"
            };
            frame.render_widget(
                Paragraph::new(footer).wrap(Wrap { trim: false }),
                Rect {
                    y: body.bottom(),
                    height: area.height - body.height,
                    ..area
                },
            );
        })?;
        let Some(input) = key()? else {
            return Ok(false);
        };
        match input.code {
            KeyCode::Enter | KeyCode::Esc => {
                plan.refresh_actions();
                return Ok(true);
            }
            KeyCode::Char(' ') if choose => {
                plan.items[cursor].selected = !plan.items[cursor].selected
            }
            KeyCode::Up | KeyCode::Char('k') if choose => cursor = cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if choose => {
                cursor = (cursor + 1).min(plan.items.len() - 1)
            }
            KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
            KeyCode::PageDown => {
                scroll = scroll.saturating_add(terminal.size()?.height.saturating_sub(2))
            }
            KeyCode::PageUp => {
                scroll = scroll.saturating_sub(terminal.size()?.height.saturating_sub(2))
            }
            KeyCode::Home => scroll = 0,
            KeyCode::End => scroll = u16::MAX,
            _ => {}
        }
    }
}
