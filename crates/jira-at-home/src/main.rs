mod front_matter;
mod mcp;
mod store;

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
    Serve,
    /// Run the disposable worker process.
    Worker(McpWorkerArgs),
}

#[derive(Args)]
struct McpWorkerArgs {
    /// Logical worker generation assigned by the host.
    #[arg(long)]
    generation: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp { command } => match command {
            McpCommand::Serve => mcp::run_host()?,
            McpCommand::Worker(args) => mcp::run_worker(args.generation)?,
        },
    }
    Ok(())
}
