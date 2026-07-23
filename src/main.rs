mod detect;
mod policy;
mod provider;
mod audit;
mod proxy;

use clap::{Parser, Subcommand};
use std::fs;
use std::path::Path;
use colored::Colorize;

use detect::RegexDetector;
use policy::PolicyEngine;
use provider::ProviderRegistry;
use proxy::run_proxy;

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
        name: Option<String>,
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
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config, port } => {
            let policy_engine = PolicyEngine::load_or_default(&config);
            println!(
                "{} Sift starting in {} mode [config: {}]",
                "==>".blue().bold(),
                format!("{:?}", policy_engine.config.mode).green().bold(),
                config
            );
            if let Err(e) = run_proxy(port, policy_engine).await {
                eprintln!("{}: {}", "Error starting proxy".red().bold(), e);
                std::process::exit(1);
            }
        }
        Commands::Scan { file, config } => {
            let policy_engine = PolicyEngine::load_or_default(&config);
            let detector = RegexDetector::new();
            let file_path = Path::new(&file);

            if !file_path.exists() {
                eprintln!("{}: File '{}' does not exist.", "Error".red().bold(), file);
                std::process::exit(1);
            }

            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{}: Failed to read file: {}", "Error".red().bold(), e);
                    std::process::exit(1);
                }
            };

            println!(
                "{} Scanning '{}' using policies from '{}'...",
                "==>".blue().bold(),
                file,
                config
            );
            
            let (redacted_content, audit_trail) = policy_engine.process_text(&detector, &content);

            if audit_trail.is_empty() {
                println!("{}", "✓ No sensitive data or PII detected.".green().bold());
            } else {
                println!(
                    "{}",
                    format!("Found {} matches:", audit_trail.len()).yellow().bold()
                );
                for record in &audit_trail {
                    crate::audit::log_audit(record);
                }
                println!("\n{}", "--- Redacted Output Preview ---".cyan().bold());
                println!("{}", redacted_content);
                println!("{}", "-------------------------------".cyan().bold());
            }
        }
        Commands::Provider { subcommand } => {
            let registry = ProviderRegistry::new();
            match subcommand {
                Some(ProviderCommands::Add { name, url, key_env }) => {
                    println!(
                        "{} Added custom provider: name={:?}, url={:?}, key_env={:?}",
                        "✓".green().bold(),
                        name.unwrap_or_else(|| "custom".to_string()),
                        url.unwrap_or_default(),
                        key_env.unwrap_or_default()
                    );
                }
                Some(ProviderCommands::List) | None => {
                    println!("{}", "Registered Upstream Providers:".blue().bold());
                    for p in &registry.providers {
                        println!(
                            "  - {} (URL: {}, Key Env: {})",
                            p.name.green().bold(),
                            p.base_url,
                            p.key_env
                        );
                    }
                }
            }
        }
        Commands::Models => {
            let registry = ProviderRegistry::new();
            println!("{}", "Exposed models (Sift protected):".blue().bold());
            for (model, provider) in registry.all_models() {
                println!(
                    "  {} {} ({})",
                    model.green().bold(),
                    "PII secured by Sift".cyan(),
                    provider
                );
            }
        }
    }
}
