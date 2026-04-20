use chrono::{FixedOffset, Utc};
use tracing::{error, info, warn};

fn now_tag() -> String {
    // Use WIB (UTC+7) for all bot log timestamps.
    let wib = FixedOffset::east_opt(7 * 60 * 60).expect("valid fixed offset");
    Utc::now()
        .with_timezone(&wib)
        .format("%Y-%m-%d %H:%M:%S%.3f WIB")
        .to_string()
}

pub fn log_info(scope: &str, message: &str) {
    info!(target: "bot_rust", "{} INFO {} {}", now_tag(), scope, message);
}

pub fn log_warn(scope: &str, message: &str) {
    warn!(target: "bot_rust", "{} WARN {} {}", now_tag(), scope, message);
}

pub fn log_error(scope: &str, message: &str) {
    error!(target: "bot_rust", "{} ERROR {} {}", now_tag(), scope, message);
}

pub fn log_cycle_separator(label: &str) {
    let divider = "=".repeat(72);
    info!(target: "bot_rust", "\n{}\nCYCLE {}\n{}", divider, label, divider);
}
