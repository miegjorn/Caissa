mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "caissa", about = "Caissa container toolbox — PII proxy + sandbox runner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command inside the Caissa sandbox container.
    Sandbox {
        /// Arguments forwarded verbatim to `docker run <image> <args...>`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Check reachability of the Farga signals endpoint.
    Report {
        #[arg(long, default_value = "http://localhost:7500")]
        farga_url: String,
        #[arg(long, default_value = "default")]
        project: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let cfg = caissa_core::config::load_config().unwrap_or_default();
    match cli.command {
        Commands::Sandbox { args } => commands::sandbox::run(&args).await,
        Commands::Report { farga_url, project } => {
            let url = if farga_url == "http://localhost:7500" { cfg.farga_url } else { farga_url };
            let proj = if project == "default" { cfg.project } else { project };
            commands::report::run(&url, &proj).await
        }
    }
}
