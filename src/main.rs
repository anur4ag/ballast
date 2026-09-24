use ballast::platform::{NativePlatform, Platform};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Daemon,
    Hook {
        #[arg(value_enum)]
        agent: Agent,
        /// Match the installed hook timeout; Ballast exits ten seconds before it.
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    Top,
    /// Facts recorded over local calendar days, including today.
    Report {
        #[arg(long, default_value = "7d", value_parser = ["1d", "7d", "30d", "90d"])]
        since: String,
        #[arg(long)]
        json: bool,
    },
    Ps {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Gc,
    Stop {
        target: String,
    },
    Resume {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        target: Option<String>,
        #[arg(long)]
        all: bool,
    },
    Install,
    Uninstall {
        /// Remove retained Ballast configuration, state and logs.
        #[arg(long)]
        purge: bool,
    },
    Doctor {
        /// Submit a test desktop notification.
        #[arg(long)]
        notify: bool,
    },
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}
#[derive(Clone, ValueEnum)]
enum Agent {
    Claude,
    Codex,
}
#[derive(Subcommand)]
enum DebugCommand {
    /// Dump capabilities, pressure inputs and process data. Environment is never printed.
    Platform,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("hook")) {
        ballast::daemon::warn_if_stranded();
    }
    let command = Cli::parse().command;
    match command {
        Command::Install => ballast::install::Installation::from_env()?.install()?,
        Command::Uninstall { purge } => {
            ballast::install::Installation::from_env()?.uninstall(purge)?
        }
        Command::Doctor { notify } => {
            if !ballast::install::Installation::from_env()?.doctor(notify) {
                std::process::exit(1);
            }
        }
        Command::Daemon => ballast::daemon::run(ballast::daemon::files::Paths::from_env()?)?,
        Command::Resume { target, .. } => {
            let count = ballast::daemon::resume_command(
                &ballast::daemon::files::Paths::from_env()?,
                target.as_deref(),
            )?;
            println!(
                "Resumed {}.",
                ballast::cli::count(count, "frozen entry", "frozen entries")
            );
        }
        Command::Gc => ballast::cleanup::command(None)?,
        Command::Stop { target } => ballast::cleanup::command(Some(target))?,
        Command::Report { since, json } => ballast::cli::report(&since, json)?,
        Command::Top => ballast::cli::run()?,
        Command::Ps { json } => ballast::cli::ps(json)?,
        Command::Status { json } => ballast::cli::status(json)?,
        Command::Debug {
            command: DebugCommand::Platform,
        } => {
            let mut platform = NativePlatform::new()?;
            let mut processes =
                platform.list_processes(&Default::default(), &Default::default())?;
            for process in &mut processes {
                process.metrics = platform.process_metrics(process.identity);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "capabilities": platform.capabilities(),
                "boot_id": platform.boot_id()?,
                    "pressure": platform.pressure()?,
                    "processes": processes,
                }))?
            );
        }
        Command::Hook {
            agent,
            timeout_seconds,
        } => ballast::hooks::run(
            match agent {
                Agent::Claude => ballast::hooks::AgentKind::Claude,
                Agent::Codex => ballast::hooks::AgentKind::Codex,
            },
            timeout_seconds,
        ),
    }
    Ok(())
}
