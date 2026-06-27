mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "caissa", about = "Caissa — agent image toolbox + sandbox runner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build an agent image from a Fondament definition.
    /// Tags the image as caissa-sandbox:<generation>.
    Build {
        /// Generation name (must match a Fondament definition, e.g. "guilhem").
        generation: String,
        /// Override the Fondament repo path (default: from caissa.toml or ../Fondament).
        #[arg(long)]
        fondament_path: Option<String>,
    },
    /// Push a built agent image to the configured registry.
    Push {
        /// Generation name (e.g. "guilhem").
        generation: String,
        /// Override the registry prefix (e.g. "ghcr.io/miegjorn").
        #[arg(long)]
        registry: Option<String>,
    },
    /// Spawn an interactive Claude Code session as a named agent.
    ///
    /// AGENT can be:
    ///   guilhem             — org agent (generation image, no domain injection)
    ///   farga               — Farga domain agent (uses current generation image)
    ///   farga/architect     — Farga domain + architect facet
    ///   farga/developer     — Farga domain + developer facet
    ///   farga/qa            — Farga domain + QA facet
    ///
    /// Facet names: architect, developer, qa, infra, db, security, moderator
    Spawn {
        /// Agent spec: generation name, domain name, or domain/facet.
        agent: String,
        /// Farga project for live context. Defaults to the domain name if specified.
        #[arg(long)]
        project: Option<String>,
        /// Session identifier. Creates an isolated workspace mounted at /workspace.
        #[arg(long)]
        session: Option<String>,
        /// Override the generation image to use (default: from caissa.toml).
        #[arg(long)]
        generation: Option<String>,
    },
    /// Start the Guilhem webhook listener daemon.
    /// Accepts POST /trigger/chronicle to run a non-interactive chronicle session.
    Listen {
        /// Port to listen on (default: 8080).
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    /// Run the agent dispatcher — MCP server that creates k8s Jobs for sub-agents.
    /// Runs in k8s with a ServiceAccount that can create Jobs in the agents namespace.
    /// Guilhem calls this via the dispatcher MCP server to invoke domain/facet agents.
    Dispatch {
        /// Port to listen on (default: 9090).
        #[arg(long, default_value_t = 9090)]
        port: u16,
    },
    /// Run a command inside the Caissa sandbox container.
    Sandbox {
        /// Session identifier (e.g. Matrix room ID). Creates an isolated workspace
        /// at <workspaces_dir>/<session> and mounts it at /workspace in the container.
        #[arg(long)]
        session: Option<String>,
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
    /// Run the SRE watchdog — no-LLM health probe loop.
    /// Checks /health on all stack services and Farga signal presence.
    /// Writes a bug-signal to Farga on any anomaly.
    /// Runs as a sidecar in the Guilhem pod (independent of the main process).
    Watch,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let cfg = caissa_core::config::load_config().unwrap_or_default();
    match cli.command {
        Commands::Build { generation, fondament_path } => {
            commands::build::run(&generation, fondament_path.as_deref()).await
        }
        Commands::Push { generation, registry } => {
            commands::push::run(&generation, registry.as_deref()).await
        }
        Commands::Spawn { agent, project, session, generation } => {
            commands::spawn::run(&agent, project.as_deref(), session.as_deref(), generation.as_deref()).await
        }
        Commands::Listen { port } => commands::listen::run(port).await,
        Commands::Dispatch { port } => commands::dispatch::run(port).await,
        Commands::Sandbox { session, args } => {
            commands::sandbox::run(session.as_deref(), &args).await
        }
        Commands::Report { farga_url, project } => {
            let url = if farga_url == "http://localhost:7500" { cfg.farga_url } else { farga_url };
            let proj = if project == "default" { cfg.project } else { project };
            commands::report::run(&url, &proj).await
        }
        Commands::Watch => commands::watch::run().await,
    }
}
