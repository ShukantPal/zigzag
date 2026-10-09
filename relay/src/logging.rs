//! Structured logging for the relay daemon.
//!
//! All log output goes to stderr with a timestamp, level, and module target
//! via the `log` facade and `env_logger`. stdout is reserved for data output
//! (canonical policy JSON, timeline output, etc.) so log lines can never
//! corrupt a consumer reading the daemon's stdout.
//!
//! Level policy:
//! - ERROR: crashes, failures, unrecoverable errors
//! - WARN: degraded states (skipped records, fallback behavior, retries)
//! - INFO: lifecycle events (startup steps, exec/spawn requests, shutdown)
//! - DEBUG: detailed flow (request handling steps, per-PR watchdog ticks)

/// Initialize process-wide logging. Idempotent: safe to call from tests.
pub fn init() {
    let _ = env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp_secs()
        .try_init();
}

/// The redacted routing data for an exec request: the bin name and the first
/// argument (the subcommand). Later arguments may carry prompts or tokens and
/// are never logged.
pub fn exec_route(bin: &str, args: &[String]) -> String {
    let subcommand = args.first().map(String::as_str).unwrap_or("");
    format!("bin={bin} subcommand={subcommand}")
}
