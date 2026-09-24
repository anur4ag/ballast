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
    Ps,
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
    Uninstall,
    Doctor,
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
    let command = Cli::parse().command;
    if !matches!(command, Command::Hook { .. }) {
        ballast::daemon::warn_if_stranded();
    }
    match command {
        Command::Daemon => ballast::daemon::run(ballast::daemon::files::Paths::from_env()?)?,
        Command::Resume { target, .. } => {
            let count = ballast::daemon::resume_command(
                &ballast::daemon::files::Paths::from_env()?,
                target.as_deref(),
            )?;
            println!("Resumed {count} frozen entries.");
        }
        Command::Gc => ballast::cleanup::command(None)?,
        Command::Stop { target } => ballast::cleanup::command(Some(target))?,
        Command::Ps => {
            use ballast::daemon::{
                files::Paths,
                ipc::{Client, Method, Reply},
            };
            let response = Client::connect(&Paths::from_env()?, std::time::Duration::from_secs(1))
                .and_then(|mut client| client.request(Method::Ps))
                .map_err(|error| format!("daemon unreachable: {error}"))?;
            let Reply::Snapshot { snapshot } = response.reply else {
                return Err("unexpected daemon snapshot response".into());
            };
            print!("{}", ballast::attribution::format_ps(&snapshot));
        }
        Command::Status { json } => {
            use ballast::daemon::{
                files::Paths,
                ipc::{Client, Method, Reply},
            };
            let response = Client::connect(&Paths::from_env()?, std::time::Duration::from_secs(1))
                .and_then(|mut client| client.request(Method::Status))
                .map_err(|error| format!("daemon unreachable: {error}"))?;
            let Reply::Status { status } = &response.reply else {
                return Err("unexpected daemon status response".into());
            };
            if json {
                println!("{}", serde_json::to_string(&response)?);
            } else {
                println!(
                    "Ballast {} running (pid {})\ntick {}: {:.3} ms CPU, {:.3} ms wall, {} ms interval; {} processes",
                    status.daemon_version,
                    status.pid,
                    status.tick,
                    status.tick_cpu_ns as f64 / 1_000_000.0,
                    status.tick_wall_ns as f64 / 1_000_000.0,
                    status.tick_interval_ms,
                    status.process_count
                );
                println!(
                    "Pressure: {:?}; agent batch running: {}",
                    status.pressure_level, status.batch_running
                );
                if !status.cleanup_pending.is_empty() {
                    println!("Cleanup pending: {}", status.cleanup_pending.join(", "));
                }
                if let Some(error) = &status.last_error {
                    println!("Last observation failed: {error}");
                }
            }
        }
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
        _ => return Err("this subcommand is not implemented yet".into()),
    }
    Ok(())
}
