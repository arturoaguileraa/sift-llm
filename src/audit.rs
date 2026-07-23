use crate::policy::{AuditRecord, Action, Mode};
use colored::Colorize;

pub fn log_audit(record: &AuditRecord) {
    let mode_str = match record.mode {
        Mode::Shadow => "[SHADOW]".yellow().bold(),
        Mode::Enforce => "[ENFORCE]".red().bold(),
    };

    let action_str = match record.action_taken {
        Action::Pass => "PASSED".green().bold(),
        Action::Redact => "REDACTED".cyan().bold(),
        Action::Block => "BLOCKED".red().bold(),
    };

    let display_value = if record.original_text.len() > 30 {
        format!("{}...", &record.original_text[..27])
    } else {
        record.original_text.clone()
    };

    println!(
        "{} {} Category: {}, Value: '{}'",
        mode_str,
        action_str,
        record.category.magenta(),
        display_value.blue()
    );
}
