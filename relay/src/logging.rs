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

use std::cell::RefCell;
use std::time::Instant;

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

/// Per-request tracking info, stored in a thread-local for the duration of
/// request handling. The relay uses a thread-per-connection model, so a
/// thread-local is safe and avoids threading request context through every
/// handler function signature.
struct RequestInfo {
    path: String,
    start: Instant,
}

thread_local! {
    static CURRENT_REQUEST: RefCell<Option<RequestInfo>> = const { RefCell::new(None) };
}

/// RAII guard that clears the thread-local request info when dropped.
/// Create one at the start of request handling; all early returns will
/// clean up automatically.
pub struct RequestGuard;

impl Drop for RequestGuard {
    fn drop(&mut self) {
        CURRENT_REQUEST.with(|r| {
            *r.borrow_mut() = None;
        });
    }
}

/// Log an incoming request and start tracking it for response logging.
/// Returns a guard that clears the tracking state when dropped.
pub fn begin_request(method: &str, path: &str, source: &str) -> RequestGuard {
    log::info!("request method={method} path={path} source={source}");
    CURRENT_REQUEST.with(|r| {
        *r.borrow_mut() = Some(RequestInfo {
            path: path.to_owned(),
            start: Instant::now(),
        });
    });
    RequestGuard
}

/// Log a completed response with status code and duration. Called from
/// `reply()` so every response is logged exactly once. If no request is
/// being tracked (e.g. in tests), this is a no-op.
pub fn log_response(status: u16, source: &str) {
    CURRENT_REQUEST.with(|r| {
        if let Some(info) = r.borrow().as_ref() {
            let duration_ms = info.start.elapsed().as_millis();
            log::info!(
                "response path={} status={} duration_ms={} source={}",
                info.path,
                status,
                duration_ms,
                source
            );
        }
    });
}
