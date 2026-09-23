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
    },
    Top,
    Ps,
    Status,
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
    match Cli::parse().command {
        Command::Debug {
            command: DebugCommand::Platform,
        } => {
            let mut platform = NativePlatform::new()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "capabilities": platform.capabilities(),
                "boot_id": platform.boot_id()?,
                    "pressure": platform.pressure()?,
                    "processes": platform.list_processes()?,
                }))?
            );
        }
        // Hook failures must not block the agent while the bridge is unimplemented.
        Command::Hook { .. } => {}
        _ => return Err("this subcommand is not implemented yet".into()),
    }
    Ok(())
}
