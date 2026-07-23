mod detect;
mod policy;
mod provider;
mod audit;
mod proxy;

use clap::{Parser, Subcommand};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use colored::Colorize;

use detect::RegexDetector;
use policy::PolicyEngine;
use provider::{discover_models, preset, Provider, ProviderRegistry};
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
            match subcommand {
                Some(ProviderCommands::Add { name, url, key_env }) => {
                    provider_add(name, url, key_env).await;
                }
                Some(ProviderCommands::List) | None => {
                    let registry = ProviderRegistry::new();
                    println!("{}", "Registered Upstream Providers:".blue().bold());
                    for p in &registry.providers {
                        println!(
                            "  - {} (URL: {}, Key Env: {}, {} models)",
                            p.name.green().bold(),
                            p.base_url,
                            if p.key_env.is_empty() { "-" } else { &p.key_env },
                            p.models.len()
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

/// Registers (or updates) an upstream provider and persists it to disk.
/// Resolves the endpoint from flags, a known preset, or an interactive picker,
/// then discovers the provider's models from its `/models` endpoint.
async fn provider_add(name: Option<String>, url: Option<String>, key_env: Option<String>) {
    let resolved = if let Some(u) = url {
        // Explicit custom endpoint.
        let n = name.unwrap_or_else(|| derive_name(&u));
        Some((n, u, key_env.clone().unwrap_or_default()))
    } else if let Some(n) = name {
        // Known provider by name (e.g. `provider add --name groq`).
        match preset(&n) {
            Some((base_url, default_key)) => Some((n, base_url, key_env.clone().unwrap_or(default_key))),
            None => {
                eprintln!(
                    "{}: unknown provider '{}'. Pass --url for a custom endpoint.",
                    "Error".red().bold(),
                    n
                );
                return;
            }
        }
    } else {
        // No flags: interactive picker.
        pick_provider_interactive()
    };

    let (name, base_url, key_env) = match resolved {
        Some(v) => v,
        None => return,
    };

    // Discover models from the provider's /models endpoint.
    let api_key = if key_env.is_empty() {
        None
    } else {
        std::env::var(&key_env).ok()
    };
    print!(
        "{} discovering models from {} ... ",
        "==>".blue().bold(),
        base_url
    );
    io::stdout().flush().ok();
    let models = match discover_models(&base_url, api_key.as_deref()).await {
        Ok(m) if !m.is_empty() => {
            println!("{}", format!("found {}", m.len()).green());
            m
        }
        Ok(_) => {
            println!("{}", "none returned".yellow());
            Vec::new()
        }
        Err(e) => {
            println!("{}", format!("skipped ({})", e).yellow());
            Vec::new()
        }
    };

    let mut registry = ProviderRegistry::load();
    registry.upsert(Provider {
        name: name.clone(),
        base_url,
        key_env,
        models: models.clone(),
    });

    match registry.save() {
        Ok(()) => println!(
            "{} provider '{}' saved ({} models, key held locally) -> {}",
            "✓".green().bold(),
            name.green().bold(),
            models.len(),
            ProviderRegistry::config_path().display()
        ),
        Err(e) => eprintln!("{}: failed to save providers: {}", "Error".red().bold(), e),
    }
}

/// Simple numbered picker of popular providers plus a custom-URL option.
fn pick_provider_interactive() -> Option<(String, String, String)> {
    let presets = ["anthropic", "openai", "google", "groq", "mistral", "ollama"];
    println!("{}", "Add a provider:".blue().bold());
    for (i, p) in presets.iter().enumerate() {
        println!("  {}. {}", i + 1, p);
    }
    println!("  {}. custom (paste URL)", presets.len() + 1);

    let choice = prompt_line("> ");
    let idx: usize = choice.parse().ok()?;

    if idx >= 1 && idx <= presets.len() {
        let name = presets[idx - 1].to_string();
        let (base_url, key_env) = preset(&name)?;
        Some((name, base_url, key_env))
    } else if idx == presets.len() + 1 {
        let base_url = prompt_line("Endpoint URL: ");
        if base_url.is_empty() {
            return None;
        }
        let name_input = prompt_line("Name (optional): ");
        let name = if name_input.is_empty() {
            derive_name(&base_url)
        } else {
            name_input
        };
        let key_env = prompt_line("API key env var (optional): ");
        Some((name, base_url, key_env))
    } else {
        None
    }
}

fn prompt_line(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    input.trim().to_string()
}

/// Derives a short provider name from a URL host (e.g. api.groq.com -> groq).
fn derive_name(url: &str) -> String {
    let host = url
        .split("://")
        .last()
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("custom");
    let labels: Vec<&str> = host.split(':').next().unwrap_or(host).split('.').collect();
    if labels.len() >= 2 {
        labels[labels.len() - 2].to_string()
    } else {
        "custom".to_string()
    }
}
