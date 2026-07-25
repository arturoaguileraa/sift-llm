use crate::policy::{AuditRecord, Action, Mode};
use colored::Colorize;

/// Truncates a value for display so secrets/PII never fully hit the logs.
fn short(text: &str) -> String {
    if text.chars().count() > 30 {
        let head: String = text.chars().take(27).collect();
        format!("{head}...")
    } else {
        text.to_string()
    }
}

/// Logs an outbound detection: what left (or would leave) the machine and how.
/// The `↑` marks the request direction; pseudonymized entries show the token the
/// value was swapped for, so they pair up with the `↓ REHYDRATED` lines.
pub fn log_audit(record: &AuditRecord) {
    let mode_str = match record.mode {
        Mode::Shadow => "[SHADOW]".yellow().bold(),
        Mode::Enforce => "[ENFORCE]".red().bold(),
    };

    let action_str = match record.action_taken {
        Action::Pass => "PASSED".green().bold(),
        Action::Redact => "REDACTED".cyan().bold(),
        Action::Pseudonymize => "PSEUDONYMIZED".blue().bold(),
        Action::Block => "BLOCKED".red().bold(),
    };

    let value = short(&record.original_text);
    match &record.token {
        Some(token) => println!(
            "{} {} {} {} '{}' {} {}",
            mode_str,
            "↑".dimmed(),
            action_str,
            record.category.magenta(),
            value.blue(),
            "→".dimmed(),
            token.yellow().bold(),
        ),
        None => println!(
            "{} {} {} Category: {}, Value: '{}'",
            mode_str,
            "↑".dimmed(),
            action_str,
            record.category.magenta(),
            value.blue(),
        ),
    }
}

/// Logs an inbound rehydration: a token in the model's response was restored to its
/// real value. The `↓` marks the response direction.
pub fn log_reveal(token: &str, value: &str) {
    println!(
        "{} {} {} {} {} '{}'",
        "[ENFORCE]".green().bold(),
        "↓".dimmed(),
        "REHYDRATED".magenta().bold(),
        token.yellow().bold(),
        "→".dimmed(),
        short(value).blue(),
    );
}
