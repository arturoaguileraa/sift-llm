mod detect;
mod policy;
mod provider;
mod audit;
mod proxy;
mod opencode;

use clap::{Parser, Subcommand};
use dialoguer::{theme::ColorfulTheme, Input, Select};
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

        /// Run in the background (detached daemon)
        #[arg(short = 'd', long)]
        daemon: bool,
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
    /// Check status of Sift gateway
    Status {
        /// Port of the gateway to check
        #[arg(short, long, default_value_t = 8787)]
        port: u16,
    },
    /// Write the registry's models into opencode's config (provider "sift-llm")
    SyncOpencode {
        /// Path to opencode config (defaults to ~/.config/opencode/opencode.jsonc)
        #[arg(long)]
        path: Option<String>,
    },
    /// Stop a background Sift gateway started with --daemon
    Stop,
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
        #[arg(long)]
        api_key: Option<String>,
    },
    /// List registered upstream providers
    List,
    /// Remove a registered upstream provider by name
    Remove {
        /// Name of the provider to remove
        name: String,
    },
}

fn main() {
    let cli = Cli::parse();

    // Daemonize before the async runtime starts: the fork must happen while the
    // process is still single-threaded (tokio threads don't survive a fork).
    if let Commands::Serve { daemon: true, port, .. } = &cli.command {
        ensure_port_free(*port);
        daemonize_background();
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to build the tokio runtime");
    rt.block_on(run(cli));
}

async fn run(cli: Cli) {
    match cli.command {
        Commands::Serve { config, port, daemon } => {
            let policy_path = Path::new(&config);
            let policy_engine = if policy_path.exists() {
                PolicyEngine::load_or_default(&config)
            } else {
                println!(
                    "{} Config file '{}' not found. Running with default secure policies.",
                    "Warning:".yellow().bold(),
                    config
                );
                PolicyEngine::new(policy::PolicyConfig::default())
            };
            println!(
                "{} Sift starting in {} mode [config: {}]",
                "==>".blue().bold(),
                format!("{:?}", policy_engine.config.mode).green().bold(),
                config
            );
            if !daemon {
                println!(
                    "{} add {} to run it in the background (then {} / {}).",
                    "Tip:".yellow().bold(),
                    "-d".cyan(),
                    "sift stop".cyan(),
                    "sift status".cyan()
                );
            }
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
                Some(ProviderCommands::Add { name, url, key_env, api_key }) => {
                    provider_add(name, url, key_env, api_key).await;
                }
                Some(ProviderCommands::Remove { name }) => {
                    let mut registry = ProviderRegistry::load();
                    if registry.remove(&name) {
                        match registry.save() {
                            Ok(()) => {
                                println!("{} removed provider '{}'", "✓".green().bold(), name.green().bold());
                                auto_sync_opencode();
                            }
                            Err(e) => eprintln!("{}: failed to save providers: {}", "Error".red().bold(), e),
                        }
                    } else {
                        eprintln!("{}: no provider named '{}'", "Warning:".yellow().bold(), name);
                    }
                }
                Some(ProviderCommands::List) | None => {
                    let registry = ProviderRegistry::new();
                    println!("{}", "Registered Upstream Providers:".blue().bold());
                    if registry.providers.is_empty() {
                        println!("  (none yet — add one with: {})", "sift provider add".cyan());
                    }
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
        Commands::Status { port } => {
            let client = reqwest::Client::new();
            let url = format!("http://127.0.0.1:{}/health", port);
            match client.get(&url).send().await {
                Ok(res) => {
                    if res.status().is_success() {
                        if let Ok(json) = res.json::<serde_json::Value>().await {
                            if json["name"] == "sift-llm" {
                                let pid = json["pid"].as_u64().unwrap_or(0);
                                println!(
                                    "{} Sift gateway is {} (active)",
                                    "✓".green().bold(),
                                    "RUNNING".green().bold()
                                );
                                println!("  - Address: http://localhost:{}", port);
                                println!("  - Process ID (PID): {}", pid);
                                return;
                            }
                        }
                    }
                    println!(
                        "{} Something is running on port {}, but it is not Sift gateway.",
                        "Warning:".yellow().bold(),
                        port
                    );
                }
                Err(_) => {
                    println!(
                        "{} Sift gateway is {} (inactive)",
                        "✗".red().bold(),
                        "NOT RUNNING".red().bold()
                    );
                    println!("  - To start the gateway, run: {}", "sift serve".cyan());
                }
            }
        }
        Commands::SyncOpencode { path } => {
            match opencode::sync_opencode(path.as_deref()) {
                Ok((n, p)) => {
                    println!(
                        "{} wrote {} models to {}",
                        "✓".green().bold(),
                        n,
                        p.display()
                    );
                    println!("  Restart opencode to see them under the 'Sift LLM' provider.");
                }
                Err(e) => {
                    eprintln!("{}: {}", "Error".red().bold(), e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Stop => {
            let pidfile = sift_dir().join("sift.pid");
            match std::fs::read_to_string(&pidfile) {
                Ok(s) => {
                    let pid = s.trim().to_string();
                    let stopped = std::process::Command::new("kill")
                        .arg(&pid)
                        .status()
                        .map(|st| st.success())
                        .unwrap_or(false);
                    let _ = std::fs::remove_file(&pidfile);
                    if stopped {
                        println!("{} stopped Sift (pid {})", "✓".green().bold(), pid);
                    } else {
                        println!(
                            "{} no running process for pid {} (removed stale pid file)",
                            "Note:".yellow().bold(),
                            pid
                        );
                    }
                }
                Err(_) => println!(
                    "{} no background Sift found (no pid file at {})",
                    "Note:".yellow().bold(),
                    pidfile.display()
                ),
            }
        }
    }
}

/// Registers (or updates) an upstream provider and persists it to disk.
/// Resolves the endpoint from flags, a known preset, or an interactive picker,
/// then discovers the provider's models from its `/models` endpoint.
async fn provider_add(name: Option<String>, url: Option<String>, key_env: Option<String>, api_key: Option<String>) {
    let resolved = if let Some(u) = url {
        // Explicit custom endpoint.
        let n = name.unwrap_or_else(|| derive_name(&u));
        Some((n, u, key_env.clone().unwrap_or_default(), api_key))
    } else if let Some(n) = name {
        // Known provider by name (e.g. `provider add --name groq`).
        match preset(&n) {
            Some((base_url, default_key)) => Some((n, base_url, key_env.clone().unwrap_or(default_key), api_key)),
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

    let (name, base_url, key_env, api_key) = match resolved {
        Some(v) => v,
        None => return,
    };

    // Discover models from the provider's /models endpoint.
    let discovery_key = api_key.clone().or_else(|| {
        if key_env.is_empty() {
            None
        } else {
            std::env::var(&key_env).ok()
        }
    });
    print!(
        "{} discovering models from {} ... ",
        "==>".blue().bold(),
        base_url
    );
    io::stdout().flush().ok();
    let models = match discover_models(&base_url, discovery_key.as_deref()).await {
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
        api_key,
        models: models.clone(),
    });

    match registry.save() {
        Ok(()) => {
            println!(
                "{} provider '{}' saved ({} models) -> {}",
                "✓".green().bold(),
                name.green().bold(),
                models.len(),
                ProviderRegistry::config_path().display()
            );
            auto_sync_opencode();
        }
        Err(e) => eprintln!("{}: failed to save providers: {}", "Error".red().bold(), e),
    }
}

/// Best-effort sync of opencode's config after the registry changes. Silently
/// skips if opencode is not set up; only warns on real failures.
fn auto_sync_opencode() {
    if !opencode::is_configured() {
        return;
    }
    match opencode::sync_opencode(None) {
        Ok((n, p)) => println!(
            "{} opencode synced: {} models -> {} (restart opencode to see them)",
            "✓".green().bold(),
            n,
            p.display()
        ),
        Err(e) => eprintln!("{} opencode not synced: {}", "Note:".yellow().bold(), e),
    }
}

/// Arrow-key picker of popular providers plus a custom-URL option.
fn pick_provider_interactive() -> Option<(String, String, String, Option<String>)> {
    // Internal keys stay lowercase (for `preset()`); labels are shown capitalised.
    let presets = ["anthropic", "openai", "google", "groq", "mistral", "ollama"];
    let labels = ["Anthropic", "OpenAI", "Google", "Groq", "Mistral", "Ollama"];
    let mut items: Vec<&str> = labels.to_vec();
    items.push("Custom (paste URL)");

    let theme = ColorfulTheme::default();
    let selection = Select::with_theme(&theme)
        .with_prompt("Add a provider")
        .items(&items)
        .default(0)
        .interact_opt()
        .ok()??; // Esc cancels -> None

    let (name, base_url, key_env) = if selection < presets.len() {
        let name = presets[selection].to_string();
        let (base_url, key_env) = preset(&name)?;
        (name, base_url, key_env)
    } else {
        let base_url: String = Input::with_theme(&theme)
            .with_prompt("Endpoint URL")
            .interact_text()
            .ok()?;
        if base_url.trim().is_empty() {
            return None;
        }
        let name_input: String = Input::with_theme(&theme)
            .with_prompt("Name (leave empty to derive from the URL)")
            .allow_empty(true)
            .interact_text()
            .ok()?;
        let name = if name_input.trim().is_empty() {
            derive_name(&base_url)
        } else {
            name_input
        };
        let key_env: String = Input::with_theme(&theme)
            .with_prompt("API key env var (optional)")
            .allow_empty(true)
            .interact_text()
            .ok()?;
        (name, base_url, key_env)
    };

    let api_key = if name != "ollama" {
        let key: String = Input::with_theme(&theme)
            .with_prompt("API key (paste, or leave empty to use the env var)")
            .allow_empty(true)
            .interact_text()
            .ok()?;
        if key.trim().is_empty() {
            None
        } else {
            Some(key)
        }
    } else {
        None
    };

    Some((name, base_url, key_env, api_key))
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

/// Directory where Sift keeps its runtime files (~/.config/sift).
fn sift_dir() -> std::path::PathBuf {
    ProviderRegistry::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Exits with a clear message if `port` is already taken (checked before forking).
fn ensure_port_free(port: u16) {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => drop(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            proxy::report_port_in_use(port);
            std::process::exit(1);
        }
        Err(_) => {} // other errors surface later when the proxy binds
    }
}

/// Detaches the process into the background, writing a pid file and a log file
/// under ~/.config/sift. Only the detached child returns from here.
fn daemonize_background() {
    let dir = sift_dir();
    std::fs::create_dir_all(&dir).ok();
    let log_path = dir.join("sift.log");
    let out = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{}: cannot create log file {}: {}",
                "Error".red().bold(),
                log_path.display(),
                e
            );
            std::process::exit(1);
        }
    };
    let err = out.try_clone().expect("clone log handle");
    println!(
        "{} Sift is running in the background.\n  Logs:   {}\n  Stop:   {}\n  Status: {}",
        "✓".green().bold(),
        log_path.display(),
        "sift stop".cyan(),
        "sift status".cyan()
    );
    let daemon = daemonize::Daemonize::new()
        .pid_file(dir.join("sift.pid"))
        .working_directory(std::env::current_dir().unwrap_or_else(|_| ".".into()))
        .stdout(out)
        .stderr(err);
    if let Err(e) = daemon.start() {
        eprintln!("{}: failed to start daemon: {}", "Error".red().bold(), e);
        std::process::exit(1);
    }
}
