use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sift")]
#[command(about = "A local PII gateway for AI coding agents", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the Sift gateway proxy daemon
    Serve {
        /// Path to policy configuration file
        #[arg(short, long, default_value = "policies.yaml")]
        config: String,

        /// Listening port for local gateway
        #[arg(short, long, default_value_t = 8787)]
        port: u16,
    },
    /// Scan a single file for sensitive data and PII
    Scan {
        /// Path to file to scan
        file: String,

        /// Path to policy configuration file
        #[arg(short, long, default_value = "policies.yaml")]
        config: String,
    },
    /// Upstream provider management
    Provider {
        #[command(subcommand)]
        subcommand: Option<ProviderCommands>,
    },
    /// List all models exposed behind Sift gateway
    Models,
}

#[derive(Subcommand)]
enum ProviderCommands {
    /// Add a new upstream LLM provider
    Add {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        key_env: Option<String>,
    },
    /// List registered upstream providers
    List,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config, port } => {
            println!("Starting Sift gateway on http://localhost:{} [config: {}]", port, config);
        }
        Commands::Scan { file, config } => {
            println!("Scanning file '{}' with policy '{}'...", file, config);
        }
        Commands::Provider { subcommand } => match subcommand {
            Some(ProviderCommands::Add { url, key_env }) => {
                println!("Adding provider (url: {:?}, key_env: {:?})...", url, key_env);
            }
            Some(ProviderCommands::List) | None => {
                println!("Listing registered providers...");
            }
        },
        Commands::Models => {
            println!("Exposed models:");
            println!("  claude-sonnet-4-6      PII secured by Sift");
            println!("  gpt-4o                 PII secured by Sift");
        }
    }
}
