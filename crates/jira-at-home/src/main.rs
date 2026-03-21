mod mcp;
mod store;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
#[cfg(test)]
use libmcp_testkit as _;

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Per-project issue notebook MCP with a hardened host/worker spine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the stdio MCP host.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Run the durable stdio host.
    Serve(McpServeArgs),
    /// Run the disposable worker process.
    Worker(McpWorkerArgs),
}

#[derive(Args)]
struct McpServeArgs {
    /// Optional project path to bind immediately on startup.
    #[arg(long)]
    project: Option<PathBuf>,
}

#[derive(Args)]
struct McpWorkerArgs {
    /// Bound project root.
    #[arg(long)]
    project: PathBuf,
    /// Logical worker generation assigned by the host.
    #[arg(long)]
    generation: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp { command } => match command {
            McpCommand::Serve(args) => mcp::run_host(args.project)?,
            McpCommand::Worker(args) => mcp::run_worker(args.project, args.generation)?,
        },
    }
    Ok(())
}
