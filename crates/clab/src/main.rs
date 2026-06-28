use anyhow::Result;
use clab_core::ClabStore;
use clab_daemon::{serve, AutoIndexConfig, AutoIndexDaemon};
use clap::{Parser, Subcommand};
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Parser)]
#[command(name = "clab")]
#[command(about = "Clab native engine")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one tool call.
    Cli { tool: String, json: Option<String> },
    /// Run or inspect the auto-index daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Run the standalone HTTP server.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        #[arg(long, default_value_t = 7780)]
        port: u16,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Daemon {
            command: DaemonCommand::Status,
        }) => {
            let daemon = AutoIndexDaemon::new(AutoIndexConfig::default());
            println!("{}", serde_json::to_string(&daemon.status())?);
        }
        Some(Command::Serve { host, port }) => {
            let daemon = AutoIndexDaemon::new(AutoIndexConfig::default());
            serve(SocketAddr::new(host, port), daemon).await?;
        }
        Some(Command::Cli { tool, json }) => {
            let args = json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_else(|| serde_json::json!({}));
            let result = ClabStore::from_env()?.dispatch(&tool, args)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        None => {
            println!("clab v2");
        }
    }
    Ok(())
}
