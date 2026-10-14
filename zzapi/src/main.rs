//! zzapi: command-line client for the zigzag relay REST API.
//!
//! Replaces hand-rolled curl commands for interacting with the relay.
//! Single native binary, no runtime dependencies.
//!
//! Config (in precedence order):
//!   --hostname / ZIGZAG_HOSTNAME      relay host (default 100.101.237.83, port 8765)
//!   --token-file / ZIGZAG_TOKEN_FILE  file holding the bearer token
//!   ZIGZAG_TOKEN                      bearer token directly (overrides the file)
//!   ZIGZAG_PROXY                      HTTP proxy URL for reaching the relay (needed from the VM)
//!
//! Examples:
//!   zzapi health
//!   zzapi update
//!   zzapi admin check-update
//!   zzapi agents list
//!   zzapi agents create --prompt "Fix the flaky test" \
//!       --project-dir /Users/shukant/Workspace/leveled-inc/leveled --branch codex/fix-flaky
//!   zzapi agents logs <id> --follow
//!   zzapi agents transcript <id> --tail 4000
//!   zzapi exec --bin gh --args pr list --repo ShukantPal/zigzag
//!   zzapi events --follow
//!   zzapi events stream

use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// Release builds set this to the version encoded in the GitHub release tag.
// Keep Cargo's package version as the fallback for local builds.
const VERSION: &str = match option_env!("ZZAPI_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

mod status;

const DEFAULT_HOSTNAME: &str = "100.101.237.83";
const DEFAULT_PORT: u16 = 8765;
const DEFAULT_SOCKET_PORT: u16 = 8766;
const DEFAULT_TOKEN_FILE: &str = ".codex/zigzag.token";
const LATEST_ZZAPI_DOWNLOAD_BASE: &str =
    "https://github.com/ShukantPal/zigzag/releases/latest/download/";
/// The relay permits synchronous executions for up to five minutes. Leave a
/// little room for the response to cross the network after that deadline.
const EXEC_CLIENT_TIMEOUT_SECS: u64 = 330;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: u16,
    message: String,
}

#[derive(Debug)]
enum Fail {
    Api(ApiError),
    Config(String),
    Command,
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Fail::Api(e)
    }
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "zzapi",
    about = "Command-line client for the zigzag relay REST API",
    version = VERSION
)]
struct Cli {
    /// Relay hostname (or host:port; default port is 8765)
    #[arg(long, env = "ZIGZAG_HOSTNAME", default_value = DEFAULT_HOSTNAME)]
    hostname: String,

    /// File holding the bearer token
    #[arg(long, env = "ZIGZAG_TOKEN_FILE")]
    token_file: Option<String>,

    /// Print raw JSON instead of human-readable tables
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Download and install the latest zzapi binary from GitHub releases
    Update,
    /// Liveness probe
    Health,
    /// Administrative relay operations
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Manage agents
    Agents {
        #[command(subcommand)]
        cmd: AgentsCmd,
    },
    /// Live read-only agent and execution status dashboard
    Status {
        /// Print one snapshot without opening the interactive screen
        #[arg(long)]
        once: bool,
        /// Include relay-internal maintenance tasks
        #[arg(long)]
        all: bool,
        /// Show only rows whose agent is still in the registry
        #[arg(long)]
        live_only: bool,
        /// Show only rows in this state (running, succeeded, failed, not observed, ...)
        #[arg(long)]
        state: Option<String>,
        /// Refresh interval in seconds
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// Local dept task root used to resolve task directories
        #[arg(long)]
        task_root: Option<String>,
        /// Local relay state file used to load audit events and agent worktrees
        #[arg(long)]
        state_file: Option<String>,
    },
    /// Manage git worktrees
    Worktrees {
        #[command(subcommand)]
        cmd: WorktreesCmd,
    },
    /// Run an allowlisted command synchronously
    Exec {
        /// Allowlisted binary name/prefix
        #[arg(long)]
        bin: String,
        /// Arguments (everything after --args; put --id before --args)
        #[arg(long, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Correlation id (auto-generated if omitted)
        #[arg(long)]
        id: Option<String>,
    },
    /// Spawn a supervised background process
    Spawn {
        /// Allowlisted binary name/prefix
        #[arg(long)]
        bin: String,
        /// Arguments (everything after --args; put --id/--execution-id before --args)
        #[arg(long, num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Correlation id (auto-generated if omitted)
        #[arg(long)]
        id: Option<String>,
        /// Client correlation id for one attempt
        #[arg(long)]
        execution_id: Option<String>,
    },
    /// Inspect a spawned process
    Proc {
        #[command(subcommand)]
        cmd: ProcCmd,
    },
    /// Read the relay event store
    Events {
        /// Sequence number to read from
        #[arg(long)]
        after: Option<u64>,
        /// Long-poll timeout in seconds (max 55)
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// Epoch from a previous read
        #[arg(long, default_value = "")]
        epoch: String,
        /// Keep long-polling for new events
        #[arg(long)]
        follow: bool,
        /// Resume from the last sequence saved by this CLI
        #[arg(long, conflicts_with = "after")]
        since_last: bool,
        #[command(subcommand)]
        command: Option<EventsCmd>,
    },
    /// Review-gate report for a PR
    #[command(name = "review-gate")]
    ReviewGate {
        /// Repository in "owner/name" form
        #[arg(long)]
        repo: String,
        /// Pull request number
        #[arg(long)]
        pr: u64,
    },
}

#[derive(Subcommand, Debug)]
enum AdminCmd {
    /// Check for and apply an available relay update
    CheckUpdate,
}

#[derive(Subcommand, Debug)]
enum AgentsCmd {
    /// List registered agents
    List {
        /// Filter by state, e.g. running
        #[arg(long)]
        state: Option<String>,
        /// Filter by task id
        #[arg(long)]
        task_id: Option<String>,
        /// Filter by branch name
        #[arg(long, conflicts_with = "pr")]
        branch: Option<String>,
        /// Filter by pull request number
        #[arg(long)]
        pr: Option<u64>,
    },
    /// Show one agent's record
    Get {
        /// Agent handle (unique prefix accepted)
        id: String,
    },
    /// Create and start an agent using a supported AI CLI
    Create {
        /// Inline prompt text; an existing client-side file path is read as a prompt file
        #[arg(long)]
        prompt: Option<String>,
        /// Read prompt text from a file on the machine running zzapi
        #[arg(long, conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Project directory the agent works in
        #[arg(long)]
        project_dir: String,
        /// Branch for the agent's work
        #[arg(long)]
        branch: Option<String>,
        /// Run in the project directory without creating a worktree
        #[arg(long)]
        no_branch: bool,
        /// Pull request number; its head branch is resolved by the relay
        #[arg(long)]
        pr: Option<u64>,
        /// Worktree path (default /private/tmp/<branch-slug>/)
        #[arg(long)]
        worktree: Option<String>,
        /// CLI harness to run
        #[arg(long, value_parser = ["codex", "gemini", "opencode", "antigravity", "agy"], default_value = "codex")]
        harness: String,
        /// Model override
        #[arg(long)]
        model: Option<String>,
        /// Approval mode override
        #[arg(long)]
        approval_mode: Option<String>,
        /// Agent timeout in seconds
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
    /// SIGSTOP the agent's process group
    Pause {
        /// Agent handle (unique prefix accepted)
        id: String,
    },
    /// SIGCONT a paused agent
    Resume {
        /// Agent handle (unique prefix accepted)
        id: String,
    },
    /// Send a live instruction to an interactive agent.
    Message {
        /// Agent handle (unique prefix accepted)
        id: String,
        /// Interrupt the active turn with this instruction
        #[arg(long, conflicts_with = "queue", required_unless_present = "queue")]
        steer: Option<String>,
        /// Append this instruction as the next turn
        #[arg(long, conflicts_with = "steer", required_unless_present = "steer")]
        queue: Option<String>,
    },
    /// Continue a Codex thread, resuming terminal agents from their rollout
    ///
    /// Resume restores rollout conversation history, not exact mid-tool-call state.
    Continue {
        /// Agent handle (unique prefix accepted); omit when using --branch or --pr
        id: Option<String>,
        /// Target a live agent on this branch
        #[arg(long, conflicts_with = "pr", required_unless_present_any = ["pr", "id"])]
        branch: Option<String>,
        /// Target a live agent for this pull request
        #[arg(long, conflicts_with = "branch", required_unless_present_any = ["branch", "id"])]
        pr: Option<u64>,
        /// Follow-up prompt text
        #[arg(long)]
        prompt: String,
        /// Interrupt the active turn before sending this prompt; terminal agents resume from their rollout by default
        #[arg(long)]
        interrupt: bool,
    },
    /// Summarize the live agent and GitHub checks for one pull request
    Status {
        /// Pull request number
        #[arg(long)]
        pr: u64,
    },
    /// Gracefully stop an agent (leaves worktree)
    Stop {
        /// Agent handle (unique prefix accepted)
        id: String,
    },
    /// Remove a terminal agent record through the relay (live agents are refused)
    #[command(
        long_about = "Calls DELETE /v1/agents/{id}; refuses agents in the running or paused state. For terminal agents, the relay removes the durable agent record, diagnostic log spool, and Codex transcript/stderr files. It removes the worktree through the repository stop hook only when the worktree is clean and its current branch tip exactly matches origin; otherwise it keeps the worktree and branch and logs why. A watched pull request also retains its worktree. When removed, the worktree cleanup attempts to delete its branch unless it is main/master or checked out elsewhere. A missing recorded worktree does not block record removal. For orphaned agents, the relay attempts to stop the possibly live process and retains the record; it applies the same worktree safety checks."
    )]
    Delete {
        /// Agent handle (unique prefix accepted)
        id: Option<String>,
        /// Delete matching agents on this branch
        #[arg(long, conflicts_with = "pr")]
        branch: Option<String>,
        /// Delete matching agents associated with this pull request
        #[arg(long, conflicts_with = "branch")]
        pr: Option<u64>,
        /// Keep this many newest matching agents
        #[arg(long)]
        keep_last: Option<usize>,
        /// Only delete records older than this duration (e.g. 7d, 12h)
        #[arg(long)]
        older_than: Option<String>,
        /// Show the relay effects without sending DELETE
        #[arg(long)]
        dry_run: bool,
    },
    /// Read an agent's captured output
    Logs {
        /// Agent handle (unique prefix accepted)
        id: String,
        /// Which stream to read (default both)
        #[arg(long, value_parser = ["stdout", "stderr", "both"])]
        stream: Option<String>,
        /// Cursor offset to read from
        #[arg(long)]
        after: Option<u64>,
        /// Cap, in bytes, on newest records
        #[arg(long)]
        tail: Option<u64>,
        /// Long-poll for new records
        #[arg(long)]
        follow: bool,
        /// Prefix each line with [stdout]/[stderr]
        #[arg(long)]
        prefix: bool,
    },
    /// Read an agent's captured transcript
    Transcript {
        /// Agent handle (unique prefix accepted)
        id: String,
        /// Return the newest N transcript items
        #[arg(long)]
        tail: Option<u64>,
        /// Return transcript items after this sequence number
        #[arg(long, conflicts_with = "tail")]
        since: Option<u64>,
        /// Poll for and print newly appended transcript output
        #[arg(long)]
        follow: bool,
    },
}

#[derive(Subcommand, Debug)]
enum WorktreesCmd {
    /// Create a git worktree
    Create {
        /// Worktree path
        #[arg(long)]
        path: String,
        /// Branch name
        #[arg(long)]
        branch: String,
        /// Repo path the worktree is created from
        #[arg(long)]
        repo: String,
    },
    /// Remove a git worktree
    Delete {
        /// Worktree path
        #[arg(long)]
        path: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProcCmd {
    /// Poll a process handle
    Get {
        /// Proc handle from spawn
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum EventsCmd {
    /// Stream newly persisted events over the relay's bidirectional socket
    Stream {
        /// Relay socket port (default 8766)
        #[arg(long, env = "ZIGZAG_SOCKET_PORT", default_value_t = DEFAULT_SOCKET_PORT)]
        socket_port: u16,
    },
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
    json: bool,
}

impl Client {
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&serde_json::Value>,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}{}", self.base, path);
        let mut req = match method {
            "GET" => self.agent.get(&url),
            "POST" => self.agent.post(&url),
            "DELETE" => self.agent.delete(&url),
            _ => {
                return Err(ApiError {
                    status: 0,
                    message: format!("unsupported method {method}"),
                });
            }
        };
        for (k, v) in query {
            req = req.query(k, v);
        }
        let req = req
            .timeout(Duration::from_secs(timeout_secs))
            .set("Authorization", &format!("Bearer {}", self.token));
        let resp = match body {
            Some(b) => req
                .set("Content-Type", "application/json")
                .send_json(b.clone()),
            None => req.call(),
        };
        match resp {
            Ok(r) => {
                let text = r.into_string().map_err(|e| ApiError {
                    status: 0,
                    message: format!("read response: {e}"),
                })?;
                let t = text.trim();
                if t.is_empty() {
                    Ok(serde_json::Value::Object(Default::default()))
                } else {
                    serde_json::from_str(t).map_err(|e| ApiError {
                        status: 0,
                        message: format!("invalid JSON response: {e}"),
                    })
                }
            }
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .and_then(|e| e.as_str())
                            .map(|s| s.to_owned())
                    })
                    .unwrap_or_else(|| {
                        let t = text.trim();
                        if t.is_empty() {
                            "request failed".to_owned()
                        } else {
                            t.chars().take(300).collect()
                        }
                    });
                Err(ApiError {
                    status: code,
                    message: msg,
                })
            }
            Err(ureq::Error::Transport(t)) => Err(ApiError {
                status: 0,
                message: format!("cannot reach relay at {} ({})", self.base, t),
            }),
        }
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<serde_json::Value, ApiError> {
        self.request("GET", path, query, None, 120)
    }

    fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, ApiError> {
        self.post_with_timeout(path, body, 120)
    }

    fn post_with_timeout(
        &self,
        path: &str,
        body: &serde_json::Value,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, ApiError> {
        self.request("POST", path, &[], Some(body), timeout_secs)
    }
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn resolve_token(token_file: Option<&str>) -> Result<String, Fail> {
    if let Ok(t) = std::env::var("ZIGZAG_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    // Canonical locations: ~/.codex/zigzag/zigzag.token (Mac) then
    // ~/.codex/zigzag.token (VM).
    let candidates = [
        format!("{home}/.codex/zigzag/zigzag.token"),
        format!("{home}/{DEFAULT_TOKEN_FILE}"),
    ];
    let path = token_file
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            candidates
                .iter()
                .find(|p| std::path::Path::new(p).exists())
                .cloned()
        })
        .unwrap_or_else(|| candidates[0].clone());
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        path.clone()
    };
    let contents = read_private_token(&expanded)?;
    let token = contents.trim().to_string();
    if token.is_empty() {
        return Err(Fail::Config(format!("token file {expanded} is empty")));
    }
    Ok(token)
}

fn read_private_token(path: &str) -> Result<String, Fail> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        Fail::Config(format!(
            "cannot read token file {path}: {e}\nset ZIGZAG_TOKEN or --token-file"
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = file
            .metadata()
            .map_err(|e| Fail::Config(format!("cannot inspect token file {path}: {e}")))?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(Fail::Config(format!(
                "token file {path} must be owned by the current user and have mode 0600 (run: chmod 600 {path})"
            )));
        }
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(|e| {
        Fail::Config(format!(
            "cannot read token file {path}: {e}\nset ZIGZAG_TOKEN or --token-file"
        ))
    })?;
    Ok(contents)
}

fn make_client(cli: &Cli) -> Result<Client, Fail> {
    let base = relay_base(&cli.hostname);
    let token = resolve_token(cli.token_file.as_deref())?;
    let mut builder = ureq::AgentBuilder::new();
    if let Ok(proxy) = std::env::var("ZIGZAG_PROXY") {
        let proxy = proxy.trim().to_string();
        if !proxy.is_empty() {
            let p = ureq::Proxy::new(&proxy)
                .map_err(|e| Fail::Config(format!("invalid proxy URL: {e}")))?;
            builder = builder.proxy(p);
        }
    }
    Ok(Client {
        base,
        token,
        agent: builder.build(),
        json: cli.json,
    })
}

fn relay_base(hostname: &str) -> String {
    let has_port = hostname
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if has_port {
        format!("http://{hostname}")
    } else {
        format!("http://{hostname}:{DEFAULT_PORT}")
    }
}

fn update_proxy_url<'a>(
    values: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Option<String> {
    let values: Vec<_> = values.into_iter().collect();
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
        .into_iter()
        .find_map(|name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| *value)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn make_update_agent(proxy_url: Option<&str>) -> Result<ureq::Agent, Fail> {
    let mut builder = ureq::AgentBuilder::new();
    if let Some(proxy_url) = proxy_url {
        let proxy = ureq::Proxy::new(proxy_url)
            .map_err(|e| Fail::Config(format!("invalid update proxy URL: {e}")))?;
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

fn latest_zzapi_url(os: &str, arch: &str) -> Result<String, Fail> {
    let asset = match (os, arch) {
        ("macos", "aarch64") => "zzapi-macos-aarch64",
        ("linux", "x86_64") => "zzapi-linux-x86_64",
        ("macos", "x86_64") => {
            return Err(Fail::Config(
                "the latest release does not include a macOS x86_64 zzapi binary".into(),
            ));
        }
        ("linux", "aarch64") => {
            return Err(Fail::Config(
                "the latest release does not include a Linux aarch64 zzapi binary".into(),
            ));
        }
        _ => {
            return Err(Fail::Config(format!(
                "automatic zzapi updates are not supported on {os}/{arch}"
            )));
        }
    };
    Ok(format!("{LATEST_ZZAPI_DOWNLOAD_BASE}{asset}"))
}

fn cmd_update() -> Result<(), Fail> {
    let executable = std::env::current_exe()
        .map_err(|e| Fail::Config(format!("cannot locate current zzapi executable: {e}")))?;
    let parent = executable
        .parent()
        .ok_or_else(|| Fail::Config("current zzapi executable has no parent directory".into()))?;
    let temp = parent.join(format!(".zzapi-update-{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let result = (|| {
        let proxy_url = update_proxy_url(
            ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
                .map(|name| {
                    let value = std::env::var(name).ok();
                    (name, value)
                })
                .iter()
                .map(|(name, value)| (*name, value.as_deref())),
        );
        let agent = make_update_agent(proxy_url.as_deref())?;
        let download_url = latest_zzapi_url(std::env::consts::OS, std::env::consts::ARCH)?;
        let response = agent
            .get(&download_url)
            .timeout(Duration::from_secs(120))
            .call()
            .map_err(|e| Fail::Config(format!("cannot download latest zzapi: {e}")))?;
        let mut reader = response.into_reader();
        let mut binary = Vec::new();
        reader
            .read_to_end(&mut binary)
            .map_err(|e| Fail::Config(format!("cannot read downloaded zzapi: {e}")))?;
        if binary.is_empty() {
            return Err(Fail::Config("downloaded zzapi is empty".into()));
        }
        std::fs::write(&temp, binary)
            .map_err(|e| Fail::Config(format!("cannot save downloaded zzapi: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).map_err(
                |e| Fail::Config(format!("cannot make downloaded zzapi executable: {e}")),
            )?;
        }
        let version = std::process::Command::new(&temp)
            .arg("--version")
            .output()
            .map_err(|e| {
                Fail::Config(format!("downloaded zzapi cannot run on this system: {e}"))
            })?;
        if !version.status.success() {
            return Err(Fail::Config(
                "downloaded zzapi failed its --version check".into(),
            ));
        }
        let version = String::from_utf8_lossy(&version.stdout).trim().to_owned();
        if version.is_empty() {
            return Err(Fail::Config(
                "downloaded zzapi did not report a version".into(),
            ));
        }
        std::fs::rename(&temp, &executable)
            .map_err(|e| Fail::Config(format!("cannot replace {}: {e}", executable.display())))?;
        println!("updated successfully ({version})");
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

// ---------------------------------------------------------------------------
// output helpers
// ---------------------------------------------------------------------------

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(c.len());
            }
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<w$}", c, w = widths.get(i).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!(
        "{}",
        line(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>())
    );
    println!(
        "{}",
        line(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>())
    );
    for r in rows {
        println!("{}", line(r));
    }
}

/// Render an epoch-seconds value as "MM-DD HH:MM" in local time; pass other
/// strings through (ISO timestamps get truncated to 19 chars).
fn fmt_ts(v: Option<&serde_json::Value>) -> String {
    let s = match v {
        Some(serde_json::Value::String(x)) => x.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => return String::new(),
    };
    if s.is_empty() {
        return String::new();
    }
    if let Ok(secs) = s.parse::<i64>() {
        return fmt_epoch(secs);
    }
    s.chars().take(19).collect::<String>().replace('T', " ")
}

fn fmt_epoch(secs: i64) -> String {
    // Howard Hinnant's civil-from-days, rendered in local time via localtime_r.
    let tm = unsafe {
        let t = secs as libc::time_t;
        let mut out: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut out).is_null() {
            return secs.to_string();
        }
        out
    };
    format!(
        "{:02}-{:02} {:02}:{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn s(v: &serde_json::Value, key: &str) -> String {
    match v.get(key) {
        Some(serde_json::Value::String(x)) => x.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn after_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(x)) => x.parse().ok(),
        _ => None,
    }
}

fn emit(client: &Client, payload: &serde_json::Value, table: impl FnOnce()) {
    if client.json {
        println!("{}", json_output(payload, false));
    } else {
        table();
    }
}

/// A followed stream is JSON Lines so each response remains independently
/// parseable. One-shot commands retain their readable, pretty JSON output.
fn json_output(payload: &serde_json::Value, streaming: bool) -> String {
    if streaming {
        serde_json::to_string(payload).unwrap()
    } else {
        serde_json::to_string_pretty(payload).unwrap()
    }
}

fn new_id() -> String {
    // 6 random bytes from /dev/urandom, hex-encoded; falls back to pid+time.
    let mut buf = [0u8; 6];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if ok {
        format!(
            "cli-{}",
            buf.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("cli-{}-{nanos}", std::process::id())
    }
}

/// Accept a unique id prefix; fall back to exact lookup.
fn resolve_agent_id(client: &Client, prefix: &str) -> Result<String, ApiError> {
    match client.get(&format!("/v1/agents/{prefix}"), &[]) {
        Ok(_) => return Ok(prefix.to_string()),
        Err(e) if e.status != 404 => return Err(e),
        Err(_) => {}
    }
    let resp = client.get("/v1/agents", &[])?;
    let agents = resp
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    resolve_agent_id_from_list(prefix, agents)
}

fn resolve_agent_id_from_list(
    prefix: &str,
    agents: &[serde_json::Value],
) -> Result<String, ApiError> {
    let matches: Vec<String> = agents
        .iter()
        .filter_map(|agent| agent.get("id").and_then(serde_json::Value::as_str))
        .filter(|id| id.starts_with(prefix))
        .map(str::to_owned)
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => Err(ApiError {
            status: 404,
            message: "unknown_agent".to_string(),
        }),
        n => Err(ApiError {
            status: 0,
            message: format!(
                "ambiguous prefix {prefix:?} matches {n} agents: {}",
                matches
                    .iter()
                    .take(5)
                    .map(|m| m.chars().take(12).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

/// Resolve an existing ID or unique ID prefix first, then an exact branch name.
/// Branch targets only select running agents so a stale registry entry can
/// never make a follow-up appear to succeed.
fn resolve_agent_target(client: &Client, target: &str) -> Result<String, ApiError> {
    match resolve_agent_id(client, target) {
        Ok(id) => return Ok(id),
        Err(error) if error.status == 404 => {}
        Err(error) => return Err(error),
    }

    let response = client.get("/v1/agents", &[])?;
    let agents = response
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    select_live_agent_for_branch(target, agents)
}

fn select_live_agent_for_branch(
    branch: &str,
    agents: &[serde_json::Value],
) -> Result<String, ApiError> {
    let matches: Vec<&serde_json::Value> = agents
        .iter()
        .filter(|agent| agent.get("branch").and_then(serde_json::Value::as_str) == Some(branch))
        .collect();
    let running: Vec<&serde_json::Value> = matches
        .iter()
        .copied()
        .filter(|agent| agent.get("state").and_then(serde_json::Value::as_str) == Some("running"))
        .collect();

    match running.as_slice() {
        [agent] => Ok(s(agent, "id")),
        [] => Err(ApiError {
            status: 404,
            message: format!("no live agent on branch {branch:?}"),
        }),
        _ => Err(ApiError {
            status: 0,
            message: format!(
                "multiple live agents on branch {branch:?}; specify an agent ID: {}",
                running
                    .iter()
                    .map(|agent| format!("{} ({})", s(agent, "id"), s(agent, "state")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

fn cmd_agents_list(
    client: &Client,
    state: Option<&str>,
    task_id: Option<&str>,
    branch: Option<&str>,
    pr: Option<u64>,
) -> Result<(), Fail> {
    let mut q: Vec<(&str, String)> = Vec::new();
    if let Some(st) = state {
        q.push(("state", st.to_string()));
    }
    if let Some(t) = task_id {
        q.push(("task_id", t.to_string()));
    }
    if let Some(branch) = branch {
        q.push(("branch", branch.to_string()));
    }
    if let Some(pr) = pr {
        q.push(("pr", pr.to_string()));
    }
    let resp = client.get("/v1/agents", &q)?;
    emit(client, &resp, || {
        let agents = resp
            .get("agents")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        if agents.is_empty() {
            println!("(no agents)");
            return;
        }
        let rows: Vec<Vec<String>> = agents
            .iter()
            .map(|a| {
                vec![
                    s(a, "id").chars().take(12).collect(),
                    s(a, "task_id"),
                    s(a, "state"),
                    s(a, "command").chars().take(40).collect(),
                    fmt_ts(a.get("started_at")),
                    s(a, "exit_code"),
                ]
            })
            .collect();
        print_table(
            &["ID", "TASK", "STATE", "COMMAND", "STARTED", "EXIT"],
            &rows,
        );
    });
    Ok(())
}

fn cmd_agents_get(client: &Client, id: &str) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let resp = client.get(&format!("/v1/agents/{id}"), &[])?;
    emit(client, &resp, || {
        for k in [
            "id",
            "task_id",
            "execution_id",
            "state",
            "command",
            "started_at",
            "deadline_at",
            "exit_code",
        ] {
            let v = if k == "started_at" || k == "deadline_at" {
                fmt_ts(resp.get(k))
            } else {
                s(&resp, k)
            };
            println!("{k:<13} {v}");
        }
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_agents_create(
    client: &Client,
    prompt: &str,
    project_dir: &str,
    branch: Option<&str>,
    no_branch: bool,
    pr: Option<u64>,
    worktree: Option<&str>,
    harness: &str,
    model: Option<&str>,
    approval_mode: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<(), Fail> {
    validate_agents_create_options(branch, no_branch, pr)?;
    let mut body = serde_json::json!({
        "prompt": prompt,
        "project_dir": project_dir,
        "no_branch": no_branch,
    });
    if let Some(branch) = branch {
        body["branch"] = serde_json::Value::String(branch.to_owned());
    }
    if let Some(pr) = pr {
        body["pr"] = serde_json::Value::Number(pr.into());
    }
    if let Some(w) = worktree {
        body["worktree"] = serde_json::Value::String(w.to_string());
    }
    body["harness"] = serde_json::Value::String(harness.to_owned());
    if let Some(m) = model {
        body["model"] = serde_json::Value::String(m.to_string());
    }
    if let Some(a) = approval_mode {
        body["approval_mode"] = serde_json::Value::String(a.to_string());
    }
    if let Some(t) = timeout_secs {
        body["timeout_secs"] = serde_json::Value::Number(t.into());
    }
    let resp = client.post("/v1/agents", &body)?;
    emit(client, &resp, || {
        println!("agent:    {}", s(&resp, "id"));
        if let Some(worktree) = resp.get("worktree").and_then(serde_json::Value::as_str) {
            println!("worktree: {worktree}");
        } else if let Some(working_dir) =
            resp.get("working_dir").and_then(serde_json::Value::as_str)
        {
            println!("working directory: {working_dir}");
        }
    });
    Ok(())
}

fn resolve_agent_prompt(prompt: Option<&str>, prompt_file: Option<&str>) -> Result<String, Fail> {
    match (prompt, prompt_file) {
        (Some(_), Some(_)) => Err(Fail::Config(
            "cannot specify both --prompt and --prompt-file".to_owned(),
        )),
        (None, None) => Err(Fail::Config(
            "must specify either --prompt or --prompt-file".to_owned(),
        )),
        (Some(prompt), None) => {
            let path = std::path::Path::new(prompt);
            if path.is_file() {
                eprintln!(
                    "debug: reading prompt from client-side file {}",
                    path.display()
                );
                std::fs::read_to_string(path).map_err(|e| {
                    Fail::Config(format!(
                        "could not read prompt file {}: {e}",
                        path.display()
                    ))
                })
            } else {
                Ok(prompt.to_owned())
            }
        }
        (None, Some(path)) => {
            eprintln!("debug: reading prompt from client-side file {path}");
            std::fs::read_to_string(path)
                .map_err(|e| Fail::Config(format!("could not read --prompt-file {path}: {e}")))
        }
    }
}

fn validate_agents_create_options(
    branch: Option<&str>,
    no_branch: bool,
    pr: Option<u64>,
) -> Result<(), Fail> {
    if branch.is_some() && no_branch {
        return Err(Fail::Config(
            "cannot specify both --branch and --no-branch".to_owned(),
        ));
    }
    if branch.is_some() && pr.is_some() {
        return Err(Fail::Config(
            "cannot specify both --branch and --pr".to_owned(),
        ));
    }
    if no_branch && pr.is_some() {
        return Err(Fail::Config(
            "cannot specify both --pr and --no-branch".to_owned(),
        ));
    }
    if branch.is_none() && !no_branch && pr.is_none() {
        return Err(Fail::Config(
            "must specify --branch or --no-branch".to_owned(),
        ));
    }
    Ok(())
}

fn cmd_agents_pause(client: &Client, id: &str) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let resp = client.post(
        &format!("/v1/agents/{id}/pause"),
        &serde_json::Value::Object(Default::default()),
    )?;
    emit(client, &resp, || {
        println!("paused {} at {}", s(&resp, "id"), s(&resp, "paused_at"));
    });
    Ok(())
}

fn cmd_agents_resume(client: &Client, id: &str) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let resp = client.post(
        &format!("/v1/agents/{id}/resume"),
        &serde_json::Value::Object(Default::default()),
    )?;
    emit(client, &resp, || {
        println!("resumed {} (paused={})", s(&resp, "id"), s(&resp, "paused"));
    });
    Ok(())
}

fn cmd_agents_message(
    client: &Client,
    id: &str,
    steer: Option<&str>,
    queue: Option<&str>,
) -> Result<(), Fail> {
    let id = resolve_agent_target(client, id)?;
    let (text, delivery) = match (steer, queue) {
        (Some(text), None) => (text, "steer"),
        (None, Some(text)) => (text, "queue"),
        _ => {
            return Err(Fail::Config(
                "provide exactly one of --steer or --queue".to_owned(),
            ));
        }
    };
    let body = serde_json::json!({"text":text,"delivery":delivery});
    let response = client.post(&format!("/v1/agents/{id}/messages"), &body)?;
    emit(client, &response, || {
        println!(
            "message accepted for {} ({delivery})",
            s(&response, "agent_id")
        )
    });
    Ok(())
}

fn cmd_agents_continue_resolved(
    client: &Client,
    id: &str,
    prompt: &str,
    interrupt: bool,
) -> Result<(), Fail> {
    let body = if interrupt {
        serde_json::json!({"prompt":prompt,"interrupt":true})
    } else {
        serde_json::json!({"prompt":prompt})
    };
    let response = client.post_with_timeout(&format!("/v1/agents/{id}/continue"), &body, 1800)?;
    emit(client, &response, || {
        println!("follow-up turn completed for {}", s(&response, "agent_id"));
        if let Some(turn) = response.get("turn") {
            println!("{}", serde_json::to_string_pretty(turn).unwrap_or_default());
        }
    });
    Ok(())
}

fn cmd_agents_continue_target(
    client: &Client,
    id: Option<&str>,
    branch: Option<&str>,
    pr: Option<u64>,
    prompt: &str,
    interrupt: bool,
) -> Result<(), Fail> {
    let target = match (id, branch, pr) {
        (Some(id), None, None) => resolve_agent_target(client, id)?,
        (None, Some(branch), None) => resolve_agent_target(client, branch)?,
        (None, None, Some(pr)) => {
            let response = client.get("/v1/agents", &[("pr", pr.to_string())])?;
            let agents = response
                .get("agents")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or_default();
            select_live_agent_for_pr(pr, agents)?
        }
        _ => {
            return Err(Fail::Config(
                "provide an agent ID, --branch, or --pr".to_owned(),
            ));
        }
    };
    cmd_agents_continue_resolved(client, &target, prompt, interrupt)
}

fn cmd_agents_status(client: &Client, pr: u64) -> Result<(), Fail> {
    let response = client.get("/v1/agents", &[("pr", pr.to_string())])?;
    let agents = response
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let agent_id = select_agent_for_pr_status(pr, agents)?;
    let agent = client.get(&format!("/v1/agents/{agent_id}"), &[])?;
    let transcript = client.get(
        &format!("/v1/agents/{agent_id}/transcript"),
        &[("tail_items", "200".to_owned())],
    )?;
    let final_answer = transcript
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .and_then(last_final_answer)
        .unwrap_or_default();
    let repository = response
        .get("repository")
        .and_then(serde_json::Value::as_str)
        .filter(|repository| !repository.is_empty())
        .ok_or_else(|| {
            Fail::Api(ApiError {
                status: 404,
                message: "could not resolve pull request repository".to_owned(),
            })
        })?;
    let gh = client.post_with_timeout("/v1/exec", &serde_json::json!({"id":new_id(),"bin":"gh","args":["pr","view",pr.to_string(),"--repo",repository,"--json","headRefOid,statusCheckRollup,url,state,commits"]}), EXEC_CLIENT_TIMEOUT_SECS)?;
    if !exec_succeeded(&gh) {
        return Err(Fail::Api(ApiError {
            status: 0,
            message: "could not read pull request status".to_owned(),
        }));
    }
    let github: serde_json::Value =
        serde_json::from_str(s(&gh, "stdout").trim()).unwrap_or_default();
    let last_commit = github
        .get("commits")
        .and_then(serde_json::Value::as_array)
        .and_then(|commits| commits.last())
        .cloned();
    if client.json {
        println!(
            "{}",
            serde_json::json!({"agent":agent,"last_final_answer":final_answer,"last_commit":last_commit,"pr":{"head_sha":github.get("headRefOid"),"state":github.get("state"),"url":github.get("url"),"checks":github.get("statusCheckRollup")}})
        );
    } else {
        println!(
            "agent: {} ({}) branch={} turn_in_flight={}",
            s(&agent, "id"),
            s(&agent, "state"),
            s(&agent, "branch"),
            s(&agent, "turn_in_flight")
        );
        println!(
            "PR #{pr}: {} head={}",
            s(&github, "state"),
            s(&github, "headRefOid")
        );
        if let Some(commit) = last_commit.as_ref() {
            println!(
                "last commit: {} {}",
                commit
                    .pointer("oid")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
                commit
                    .pointer("messageHeadline")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
            );
        }
        println!(
            "checks: {}",
            github
                .get("statusCheckRollup")
                .map(serde_json::Value::to_string)
                .unwrap_or_default()
        );
        if !final_answer.is_empty() {
            println!("\nlast final answer:\n{final_answer}");
        }
    }
    Ok(())
}

fn select_agent_for_pr_status(pr: u64, agents: &[serde_json::Value]) -> Result<String, Fail> {
    let running: Vec<&serde_json::Value> = agents
        .iter()
        .filter(|agent| agent.get("state").and_then(serde_json::Value::as_str) == Some("running"))
        .collect();
    let candidates = if running.is_empty() {
        agents.iter().collect::<Vec<_>>()
    } else {
        running
    };
    match candidates.as_slice() {
        [agent] => Ok(s(agent, "id")),
        [] => Err(Fail::Api(ApiError {
            status: 404,
            message: format!("no agent found for PR #{pr}"),
        })),
        _ => Err(Fail::Config(format!(
            "multiple agents found for PR #{pr}; specify an agent ID"
        ))),
    }
}

fn last_final_answer(transcript: &str) -> Option<String> {
    transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| {
            let item = record.get("item")?;
            let kind = item.get("type")?.as_str()?;
            if !matches!(kind, "agent_message" | "message") {
                return None;
            }
            item.get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .next_back()
}

fn select_live_agent_for_pr(pr: u64, agents: &[serde_json::Value]) -> Result<String, ApiError> {
    let running: Vec<&serde_json::Value> = agents
        .iter()
        .filter(|agent| agent.get("state").and_then(serde_json::Value::as_str) == Some("running"))
        .collect();
    match running.as_slice() {
        [agent] => Ok(s(agent, "id")),
        [] => Err(ApiError {
            status: 404,
            message: format!("no live agent for PR #{pr}"),
        }),
        _ => Err(ApiError {
            status: 0,
            message: format!(
                "multiple live agents for PR #{pr}; specify an agent ID: {}",
                running
                    .iter()
                    .map(|agent| s(agent, "id"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

fn cmd_agents_stop(client: &Client, id: &str) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let resp = client.request("DELETE", &format!("/v1/agents/{id}"), &[], None, 120)?;
    emit(client, &resp, || {
        if resp.get("deleted").and_then(|value| value.as_bool()) == Some(true) {
            println!("deleted {}", s(&resp, "id"));
        } else {
            println!("stopped {}", s(&resp, "id"));
        }
    });
    Ok(())
}

fn cmd_agents_delete(client: &Client, id: &str, dry_run: bool) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let agent = client.get(&format!("/v1/agents/{id}"), &[])?;
    let state = s(&agent, "state");
    if agent_state_is_live_for_delete(&state) {
        return Err(Fail::Config(format!(
            "refusing to delete {state} agent {id}; use `zzapi agents stop {id}` first"
        )));
    }

    if dry_run {
        println!("Dry run for agent {id} (state: {state}):");
        if state == "orphaned" {
            println!(
                "- The relay attempts to stop the possibly live process and retains its agent record with a stopped/killed state."
            );
            println!(
                "- After a successful stop, it removes the worktree through the repository stop hook only when clean and its branch tip exactly matches origin; a watched pull request also retains the worktree."
            );
            println!(
                "- Dirty, unpushed, or unverifiable worktrees are kept with their branch and a relay warning."
            );
        } else {
            println!(
                "- The relay removes the agent record, diagnostic log spool, and Codex transcript/stderr files."
            );
            println!(
                "- It removes the worktree through the repository stop hook only when clean and the current branch tip exactly matches origin; otherwise it keeps the worktree and branch and logs why."
            );
            if state == "lost_after_restart" {
                println!("- A missing worktree does not block record removal.");
            } else {
                println!(
                    "- A watched pull request also retains its worktree; a missing worktree does not block record removal."
                );
            }
        }
        println!(
            "- When it removes the worktree, it attempts to delete its branch unless it is main/master, checked out elsewhere, or cleanup fails."
        );
        println!("No DELETE request was sent.");
        return Ok(());
    }

    let resp = client.request("DELETE", &format!("/v1/agents/{id}"), &[], None, 120)?;
    emit(client, &resp, || {
        if resp.get("deleted").and_then(|value| value.as_bool()) == Some(true) {
            println!(
                "deleted {} (worktree_removed={}, transcript_removed={})",
                s(&resp, "id"),
                s(&resp, "worktree_removed"),
                s(&resp, "transcript_removed")
            );
        } else {
            println!(
                "stop completed for {} (stopped={})",
                s(&resp, "id"),
                s(&resp, "stopped")
            );
        }
    });
    Ok(())
}

fn cmd_agents_delete_bulk(
    client: &Client,
    id: Option<&str>,
    branch: Option<&str>,
    pr: Option<u64>,
    keep_last: Option<usize>,
    older_than: Option<&str>,
    dry_run: bool,
) -> Result<(), Fail> {
    if let Some(id) = id {
        if branch.is_some() || pr.is_some() || keep_last.is_some() || older_than.is_some() {
            return Err(Fail::Config(
                "bulk delete filters require --branch or --pr without an agent ID".to_owned(),
            ));
        }
        return cmd_agents_delete(client, id, dry_run);
    }
    if branch.is_none() && pr.is_none() {
        return Err(Fail::Config(
            "provide an agent ID, --branch, or --pr".to_owned(),
        ));
    }
    let mut query = Vec::new();
    if let Some(branch) = branch {
        query.push(("branch", branch.to_owned()));
    }
    if let Some(pr) = pr {
        query.push(("pr", pr.to_string()));
    }
    if let Some(keep_last) = keep_last {
        query.push(("keep_last", keep_last.to_string()));
    }
    if let Some(older_than) = older_than {
        query.push(("older_than", older_than.to_owned()));
    }
    if dry_run {
        query.push(("dry_run", "1".to_owned()));
    }
    let response = client.request("DELETE", "/v1/agents", &query, None, 1800)?;
    emit(client, &response, || {
        println!(
            "{} agents (deleted={}, skipped={})",
            if dry_run { "would delete" } else { "deleted" },
            s(&response, "deleted_count"),
            s(&response, "skipped_count")
        );
        if let Some(ids) = response
            .get("deleted")
            .and_then(serde_json::Value::as_array)
        {
            for id in ids {
                if let Some(id) = id.as_str() {
                    println!("  {id}");
                }
            }
        }
    });
    Ok(())
}

fn agent_state_is_live_for_delete(state: &str) -> bool {
    matches!(state, "running" | "paused")
}

#[allow(clippy::too_many_arguments)]
fn cmd_agents_logs(
    client: &Client,
    id: &str,
    stream: Option<&str>,
    after: Option<u64>,
    tail: Option<u64>,
    follow: bool,
    prefix: bool,
) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let mut after = after.unwrap_or(0);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let mut q: Vec<(&str, String)> = vec![("after", after.to_string())];
        if let Some(st) = stream {
            q.push(("stream", st.to_string()));
        }
        if let Some(t) = tail {
            q.push(("tail", t.to_string()));
        }
        if follow {
            q.push(("follow", "1".to_string()));
        }
        let resp = client.get(&format!("/v1/agents/{id}/logs"), &q)?;
        if client.json {
            println!("{}", json_output(&resp, follow));
        } else if let Some(records) = resp.get("records").and_then(|r| r.as_array()) {
            for rec in records {
                let stream_name = s(rec, "stream");
                let data = s(rec, "data");
                if prefix {
                    write!(out, "[{stream_name}] {data}").ok();
                } else {
                    write!(out, "{data}").ok();
                }
            }
            out.flush().ok();
        }
        if log_retention_lost(&resp, after) {
            eprintln!(
                "warning: agent log retention lost earlier records; output starts at the oldest retained record"
            );
            return Err(Fail::Command);
        }
        after = after_u64(resp.get("next_cursor")).unwrap_or(after);
        let complete = resp
            .get("complete")
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        if !follow || complete {
            break;
        }
    }
    Ok(())
}

fn cmd_agents_transcript(
    client: &Client,
    id: &str,
    tail: Option<u64>,
    since: Option<u64>,
    follow: bool,
) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let mut previous = None;
    loop {
        let mut q = Vec::new();
        if let Some(cursor) = previous.as_ref().and_then(transcript_cursor) {
            q.push(("after", cursor.to_string()));
        } else if let Some(tail) = tail {
            q.push(("tail_items", tail.to_string()));
        }
        if previous.is_none()
            && let Some(since) = since
        {
            q.push(("since", since.to_string()));
        }
        let response = client.get(&format!("/v1/agents/{id}/transcript"), &q)?;
        if client.json {
            println!("{}", json_output(&response, follow));
        } else if let Some(previous) = previous.as_ref() {
            print_transcript_updates(&response);
            if !transcript_log_degraded(previous) && transcript_log_degraded(&response) {
                warn_transcript_degraded();
            }
        } else {
            print_transcript(&response);
        }

        let complete = s(&response, "state") != "running";
        previous = Some(response);
        if !follow || complete {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn print_transcript(transcript: &serde_json::Value) {
    for key in [
        "id",
        "task_id",
        "execution_id",
        "state",
        "started_at",
        "exit_code",
    ] {
        let value = if key == "started_at" {
            fmt_ts(transcript.get(key))
        } else {
            s(transcript, key)
        };
        println!("{key:<13} {value}");
    }
    let command = s(transcript, "command");
    if !command.is_empty() {
        println!("command       {command}");
    }
    for (section, field) in [
        ("prompt", "prompt"),
        ("last message", "last_message"),
        ("stdout", "stdout"),
        ("stderr", "stderr"),
    ] {
        print_transcript_section(section, transcript_text(transcript, field));
    }
    if transcript_log_degraded(transcript) {
        warn_transcript_degraded();
    }
}

fn transcript_text<'a>(transcript: &'a serde_json::Value, key: &str) -> &'a str {
    transcript
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn print_transcript_section(name: &str, text: &str) {
    if !text.is_empty() {
        println!("\n--- {name} ---");
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
}

fn print_transcript_updates(current: &serde_json::Value) {
    for key in ["stdout", "stderr"] {
        print_transcript_section(key, transcript_text(current, key));
    }
    std::io::stdout().flush().ok();
}

fn transcript_cursor(transcript: &serde_json::Value) -> Option<u64> {
    after_u64(transcript.get("next_cursor"))
}

fn transcript_log_degraded(transcript: &serde_json::Value) -> bool {
    transcript
        .get("log_degraded")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn warn_transcript_degraded() {
    eprintln!("warning: agent transcript is incomplete because log capture degraded");
}

fn log_retention_lost(response: &serde_json::Value, after: u64) -> bool {
    after_u64(response.get("dropped_before")).is_some_and(|dropped_before| after < dropped_before)
}

// ---------------------------------------------------------------------------
// worktrees
// ---------------------------------------------------------------------------

fn cmd_worktrees_create(client: &Client, path: &str, branch: &str, repo: &str) -> Result<(), Fail> {
    let body = serde_json::json!({"path": path, "branch": branch, "repo": repo});
    let resp = client.post("/v1/worktrees", &body)?;
    emit(client, &resp, || {
        println!(
            "worktree: {} (branch {})",
            s(&resp, "path"),
            s(&resp, "branch")
        );
    });
    Ok(())
}

fn cmd_worktrees_delete(client: &Client, path: &str) -> Result<(), Fail> {
    let body = serde_json::json!({"path": path});
    let resp = client.request("DELETE", "/v1/worktrees", &[], Some(&body), 120)?;
    emit(client, &resp, || {
        let p = s(&resp, "path");
        let removed = resp
            .get("removed")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        if removed {
            println!("removed {p}");
        } else {
            println!("not removed: {p}");
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// exec / spawn / proc
// ---------------------------------------------------------------------------

fn cmd_exec(client: &Client, bin: &str, args: &[String], id: Option<&str>) -> Result<(), Fail> {
    let req_id = id.map(|x| x.to_string()).unwrap_or_else(new_id);
    let body = serde_json::json!({
        "id": req_id,
        "bin": bin,
        "args": args,
    });
    let resp = client.post_with_timeout("/v1/exec", &body, EXEC_CLIENT_TIMEOUT_SECS)?;
    if s(&resp, "error") == "denied" {
        println!("denied (id={})", s(&resp, "id"));
        return Err(Fail::Command);
    }
    emit(client, &resp, || {
        let stdout = s(&resp, "stdout");
        if !stdout.is_empty() {
            print!("{stdout}");
        }
        let stderr = s(&resp, "stderr");
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
        eprintln!(
            "exit={} timed_out={} truncated={}",
            s(&resp, "exit_code"),
            s(&resp, "timed_out"),
            s(&resp, "truncated")
        );
    });
    if !exec_succeeded(&resp) {
        return Err(Fail::Command);
    }
    Ok(())
}

fn exec_succeeded(response: &serde_json::Value) -> bool {
    response.get("exit_code").and_then(|code| code.as_i64()) == Some(0)
        && !response
            .get("timed_out")
            .and_then(|timed_out| timed_out.as_bool())
            .unwrap_or(false)
}

fn cmd_spawn(
    client: &Client,
    bin: &str,
    args: &[String],
    id: Option<&str>,
    execution_id: Option<&str>,
) -> Result<(), Fail> {
    let mut body = serde_json::json!({
        "id": id.map(|x| x.to_string()).unwrap_or_else(new_id),
        "bin": bin,
        "args": args,
    });
    if let Some(e) = execution_id {
        body["execution_id"] = serde_json::Value::String(e.to_string());
    }
    let resp = client.post("/v1/spawn", &body)?;
    if s(&resp, "error") == "denied" {
        println!("denied (id={})", s(&resp, "id"));
        std::process::exit(1);
    }
    emit(client, &resp, || {
        println!("proc: {}", s(&resp, "proc"));
    });
    Ok(())
}

fn cmd_proc_get(client: &Client, id: &str) -> Result<(), Fail> {
    let resp = client.get(&format!("/v1/proc/{id}"), &[])?;
    emit(client, &resp, || {
        println!("running:   {}", s(&resp, "running"));
        println!("exit_code: {}", s(&resp, "exit_code"));
        let stdout = s(&resp, "stdout");
        if !stdout.is_empty() {
            println!("--- stdout ---");
            print!("{stdout}");
        }
        let stderr = s(&resp, "stderr");
        if !stderr.is_empty() {
            println!("--- stderr ---");
            eprint!("{stderr}");
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

fn cmd_events(
    client: &Client,
    after: Option<u64>,
    timeout: u64,
    epoch: &str,
    follow: bool,
    since_last: bool,
) -> Result<(), Fail> {
    let cursor_path = events_cursor_path();
    let mut after = if after.is_none() && (since_last || follow) {
        read_event_cursor(&cursor_path).unwrap_or(0)
    } else {
        after.unwrap_or(0)
    };
    let mut epoch = epoch.to_string();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let q: Vec<(&str, String)> = vec![
            ("after", after.to_string()),
            ("timeout", timeout.to_string()),
            ("epoch", epoch.clone()),
        ];
        let resp = client.request("GET", "/v1/events", &q, None, timeout + 30)?;
        if client.json {
            println!("{}", json_output(&resp, follow));
        } else if let Some(events) = resp.get("events").and_then(|e| e.as_array()) {
            for ev in events {
                let at = s(ev, "occurred_at")
                    .chars()
                    .take(19)
                    .collect::<String>()
                    .replace('T', " ");
                writeln!(out, "{at} {:<22} task={}", s(ev, "kind"), s(ev, "task_id")).ok();
            }
            out.flush().ok();
        }
        let progress = event_progress(&resp, after, &epoch);
        if progress.reset {
            eprintln!("warning: event store reset; resumed at its current cursor");
        }
        if progress.lost {
            eprintln!(
                "warning: event retention lost earlier events; output starts at the oldest retained event"
            );
            return Err(Fail::Command);
        }
        after = progress.after;
        if since_last || follow {
            write_event_cursor(&cursor_path, after)?;
        }
        epoch = progress.epoch;
        if !follow {
            break;
        }
    }
    Ok(())
}

fn events_cursor_path() -> std::path::PathBuf {
    std::env::var_os("ZIGZAG_EVENTS_CURSOR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".codex/zzapi-events.cursor"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".zzapi-events.cursor"))
}

fn read_event_cursor(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_event_cursor(path: &std::path::Path, cursor: u64) -> Result<(), Fail> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Fail::Config(format!("could not create event cursor directory: {e}")))?;
    }
    std::fs::write(path, cursor.to_string())
        .map_err(|e| Fail::Config(format!("could not save event cursor: {e}")))
}

const MAX_SOCKET_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Stream the `events` topic from the relay's authenticated TCP protocol.
/// Unlike HTTP follow, the relay pushes each newly persisted event immediately.
fn cmd_events_stream(client: &Client, socket_port: u16) -> Result<(), Fail> {
    let host = socket_host(&client.base).map_err(|error| {
        eprintln!("error: {error}");
        Fail::Command
    })?;
    let mut stream = TcpStream::connect((host, socket_port)).map_err(|error| {
        eprintln!("error: cannot reach relay socket at {host}:{socket_port}: {error}");
        Fail::Command
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| {
            eprintln!("error: could not configure relay socket: {error}");
            Fail::Command
        })?;
    write_socket_frame(
        &mut stream,
        &serde_json::json!({"type":"auth","authorization":format!("Bearer {}", client.token)}),
    )
    .map_err(|error| {
        eprintln!("error: relay socket authentication failed: {error}");
        Fail::Command
    })?;
    let authentication = read_socket_frame(&mut stream).map_err(|error| {
        eprintln!("error: relay socket authentication failed: {error}");
        Fail::Command
    })?;
    if authentication.get("type").and_then(|value| value.as_str()) != Some("authenticated") {
        eprintln!("error: relay socket rejected authentication");
        return Err(Fail::Command);
    }
    write_socket_frame(
        &mut stream,
        &serde_json::json!({"type":"subscribe","topic":"events"}),
    )
    .map_err(|error| {
        eprintln!("error: could not subscribe to relay events: {error}");
        Fail::Command
    })?;
    stream.set_read_timeout(None).map_err(|error| {
        eprintln!("error: could not configure relay socket: {error}");
        Fail::Command
    })?;

    loop {
        let frame = read_socket_frame(&mut stream).map_err(|error| {
            eprintln!("error: relay event stream ended: {error}");
            Fail::Command
        })?;
        if client.json {
            println!("{}", serde_json::to_string(&frame).unwrap());
            continue;
        }
        if frame.get("topic").and_then(|value| value.as_str()) == Some("events") {
            if frame.get("dropped").and_then(|value| value.as_bool()) == Some(true) {
                eprintln!(
                    "warning: relay socket dropped events; resynchronize with `zzapi events`"
                );
            } else if let Some(event) = frame.get("event") {
                let at = s(event, "occurred_at")
                    .chars()
                    .take(19)
                    .collect::<String>()
                    .replace('T', " ");
                println!("{at} {:<22} task={}", s(event, "kind"), s(event, "task_id"));
            }
        } else if frame.get("type").and_then(|value| value.as_str()) == Some("error") {
            eprintln!("warning: relay socket: {}", s(&frame, "error"));
        }
    }
}

fn socket_host(base: &str) -> Result<&str, String> {
    let authority = base
        .strip_prefix("http://")
        .ok_or_else(|| format!("invalid relay URL {base:?}"))?;
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| format!("relay URL has no port: {base:?}"))?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(format!("invalid relay URL {base:?}"));
    }
    Ok(host)
}

fn write_socket_frame(stream: &mut TcpStream, value: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if body.is_empty() || body.len() > MAX_SOCKET_FRAME_BYTES {
        return Err("invalid outbound frame length".to_owned());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| error.to_string())
}

fn read_socket_frame(stream: &mut TcpStream) -> Result<serde_json::Value, String> {
    let mut prefix = [0u8; 4];
    stream
        .read_exact(&mut prefix)
        .map_err(|error| error.to_string())?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_SOCKET_FRAME_BYTES {
        return Err("invalid frame length".to_owned());
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body).map_err(|_| "invalid JSON frame".to_owned())
}

#[derive(Debug, PartialEq, Eq)]
struct EventProgress {
    after: u64,
    epoch: String,
    reset: bool,
    lost: bool,
}

/// Advance from the server response itself. On an epoch reset, that response
/// already contains the retained events from the new epoch; reusing its
/// cursor prevents a follow request from printing them a second time.
fn event_progress(response: &serde_json::Value, after: u64, epoch: &str) -> EventProgress {
    EventProgress {
        after: after_u64(response.get("next")).unwrap_or(after),
        epoch: response
            .get("epoch")
            .and_then(|value| value.as_str())
            .unwrap_or(epoch)
            .to_string(),
        reset: response
            .get("reset")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        lost: response
            .get("lost")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

// ---------------------------------------------------------------------------
// misc
// ---------------------------------------------------------------------------

fn cmd_health(client: &Client) -> Result<(), Fail> {
    let resp = client.get("/v1/health", &[])?;
    emit(client, &resp, || {
        if s(&resp, "status") == "ok" {
            println!("ok");
        } else {
            println!("{resp}");
        }
    });
    Ok(())
}

fn cmd_admin_check_update(client: &Client) -> Result<(), Fail> {
    let resp = client.post(
        "/v1/update/check",
        &serde_json::Value::Object(Default::default()),
    )?;
    emit(client, &resp, || {
        let current = display_version(resp.get("current_version"), "unknown");
        let latest = display_version(resp.get("latest_available_version"), "none");
        let applied = resp
            .get("update_applied")
            .and_then(serde_json::Value::as_bool)
            .map(|value| if value { "yes" } else { "no" })
            .unwrap_or("unknown");
        println!("current version: {current}");
        println!("latest version:  {latest}");
        println!("update applied:  {applied}");
    });
    Ok(())
}

fn display_version(value: Option<&serde_json::Value>, missing: &str) -> String {
    value
        .and_then(serde_json::Value::as_str)
        .unwrap_or(missing)
        .to_owned()
}

fn cmd_review_gate(client: &Client, repo: &str, pr: u64) -> Result<(), Fail> {
    let q: Vec<(&str, String)> = vec![
        ("repository", repo.to_string()),
        ("pull_request", pr.to_string()),
    ];
    let resp = client.get("/v1/review-gate", &q)?;
    emit(client, &resp, || {
        let pretty = serde_json::to_string_pretty(&resp).unwrap();
        println!("{}", pretty.chars().take(4000).collect::<String>());
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn report_api_error(e: &ApiError) {
    if e.status == 401 {
        eprintln!("error: auth failed (bad or missing bearer token)");
    } else if e.status == 404 {
        eprintln!("error: not found: {}", e.message);
    } else if e.status == 0 {
        eprintln!("error: {}", e.message);
    } else {
        eprintln!("error: HTTP {}: {}", e.status, e.message);
    }
}

fn run(cli: Cli) -> Result<(), Fail> {
    if matches!(&cli.command, Commands::Update) {
        return cmd_update();
    }
    let mut resolved_agent_prompt = None;
    if let Commands::Agents {
        cmd:
            AgentsCmd::Create {
                branch,
                no_branch,
                pr,
                prompt,
                prompt_file,
                ..
            },
    } = &cli.command
    {
        validate_agents_create_options(branch.as_deref(), *no_branch, *pr)?;
        resolved_agent_prompt = Some(resolve_agent_prompt(
            prompt.as_deref(),
            prompt_file.as_deref(),
        )?);
    }
    let client = make_client(&cli)?;
    // clap's trailing_var_arg keeps a leading "--" out, but strip it anyway
    // for ergonomics (matches the old Python client).
    let strip_dd = |args: &mut Vec<String>| {
        if args.first().map(|x| x == "--").unwrap_or(false) {
            args.remove(0);
        }
    };
    match cli.command {
        Commands::Update => unreachable!("update command handled before relay client setup"),
        Commands::Health => cmd_health(&client),
        Commands::Admin { cmd } => match cmd {
            AdminCmd::CheckUpdate => cmd_admin_check_update(&client),
        },
        Commands::Agents { cmd } => match cmd {
            AgentsCmd::List {
                state,
                task_id,
                branch,
                pr,
            } => cmd_agents_list(
                &client,
                state.as_deref(),
                task_id.as_deref(),
                branch.as_deref(),
                pr,
            ),
            AgentsCmd::Get { id } => cmd_agents_get(&client, &id),
            AgentsCmd::Create {
                prompt: _,
                prompt_file: _,
                project_dir,
                branch,
                no_branch,
                pr,
                worktree,
                harness,
                model,
                approval_mode,
                timeout_secs,
            } => cmd_agents_create(
                &client,
                resolved_agent_prompt
                    .as_deref()
                    .expect("agent create prompt resolved before client setup"),
                &project_dir,
                branch.as_deref(),
                no_branch,
                pr,
                worktree.as_deref(),
                &harness,
                model.as_deref(),
                approval_mode.as_deref(),
                timeout_secs,
            ),
            AgentsCmd::Pause { id } => cmd_agents_pause(&client, &id),
            AgentsCmd::Resume { id } => cmd_agents_resume(&client, &id),
            AgentsCmd::Message { id, steer, queue } => {
                cmd_agents_message(&client, &id, steer.as_deref(), queue.as_deref())
            }
            AgentsCmd::Continue {
                id,
                branch,
                pr,
                prompt,
                interrupt,
            } => cmd_agents_continue_target(
                &client,
                id.as_deref(),
                branch.as_deref(),
                pr,
                &prompt,
                interrupt,
            ),
            AgentsCmd::Status { pr } => cmd_agents_status(&client, pr),
            AgentsCmd::Stop { id } => cmd_agents_stop(&client, &id),
            AgentsCmd::Delete {
                id,
                branch,
                pr,
                keep_last,
                older_than,
                dry_run,
            } => cmd_agents_delete_bulk(
                &client,
                id.as_deref(),
                branch.as_deref(),
                pr,
                keep_last,
                older_than.as_deref(),
                dry_run,
            ),
            AgentsCmd::Logs {
                id,
                stream,
                after,
                tail,
                follow,
                prefix,
            } => cmd_agents_logs(&client, &id, stream.as_deref(), after, tail, follow, prefix),
            AgentsCmd::Transcript {
                id,
                tail,
                since,
                follow,
            } => cmd_agents_transcript(&client, &id, tail, since, follow),
        },
        Commands::Status {
            once,
            all,
            live_only,
            state,
            interval,
            task_root,
            state_file,
        } => status::cmd_status(
            &client,
            status::StatusOptions {
                once,
                all,
                live_only,
                state_filter: state.as_deref(),
                interval,
                task_root: task_root.as_deref(),
                state_override: state_file.as_deref(),
            },
        ),
        Commands::Worktrees { cmd } => match cmd {
            WorktreesCmd::Create { path, branch, repo } => {
                cmd_worktrees_create(&client, &path, &branch, &repo)
            }
            WorktreesCmd::Delete { path } => cmd_worktrees_delete(&client, &path),
        },
        Commands::Exec { bin, mut args, id } => {
            strip_dd(&mut args);
            cmd_exec(&client, &bin, &args, id.as_deref())
        }
        Commands::Spawn {
            bin,
            mut args,
            id,
            execution_id,
        } => {
            strip_dd(&mut args);
            cmd_spawn(&client, &bin, &args, id.as_deref(), execution_id.as_deref())
        }
        Commands::Proc { cmd } => match cmd {
            ProcCmd::Get { id } => cmd_proc_get(&client, &id),
        },
        Commands::Events {
            after,
            timeout,
            epoch,
            follow,
            since_last,
            command,
        } => match command {
            Some(EventsCmd::Stream { socket_port }) => cmd_events_stream(&client, socket_port),
            None => cmd_events(&client, after, timeout, &epoch, follow, since_last),
        },
        Commands::ReviewGate { repo, pr } => cmd_review_gate(&client, &repo, pr),
    }
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => {}
        Err(Fail::Api(e)) => {
            report_api_error(&e);
            std::process::exit(1);
        }
        Err(Fail::Config(msg)) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
        Err(Fail::Command) => std::process::exit(1),
    }
    // Note: Ctrl-C during --follow terminates via the default SIGINT
    // disposition; the shell reports exit code 130.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_file_reads_client_side_content() {
        let path = std::env::temp_dir().join(format!(
            "zzapi-prompt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "file prompt").unwrap();
        let path = path.to_string_lossy().into_owned();

        assert_eq!(
            resolve_agent_prompt(None, Some(&path)).unwrap(),
            "file prompt"
        );
        assert_eq!(
            resolve_agent_prompt(Some(&path), None).unwrap(),
            "file prompt"
        );
        assert_eq!(
            resolve_agent_prompt(Some("inline prompt"), None).unwrap(),
            "inline prompt"
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_missing_prompt_file_is_a_clear_config_error() {
        let path = std::env::temp_dir()
            .join(format!("zzapi-missing-prompt-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        assert!(matches!(
            resolve_agent_prompt(None, Some(&path)),
            Err(Fail::Config(message)) if message.contains("could not read --prompt-file")
        ));
    }

    #[test]
    fn agent_delete_cli_guard_rejects_running_and_paused_agents() {
        assert!(agent_state_is_live_for_delete("running"));
        assert!(agent_state_is_live_for_delete("paused"));
        assert!(!agent_state_is_live_for_delete("succeeded"));
        assert!(!agent_state_is_live_for_delete("orphaned"));
    }

    #[test]
    fn transcript_follow_uses_the_relay_cursor() {
        assert_eq!(
            transcript_cursor(&serde_json::json!({"next_cursor": 7})),
            Some(7)
        );
        assert_eq!(transcript_cursor(&serde_json::json!({})), None);
    }

    #[test]
    fn agent_dx_options_parse_for_tail_since_and_filters() {
        let cli = Cli::try_parse_from(["zzapi", "agents", "transcript", "agent-1", "--tail", "5"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Agents {
                cmd: AgentsCmd::Transcript { tail: Some(5), .. }
            }
        ));
        let cli =
            Cli::try_parse_from(["zzapi", "agents", "transcript", "agent-1", "--since", "42"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Agents {
                cmd: AgentsCmd::Transcript {
                    since: Some(42),
                    ..
                }
            }
        ));
        assert!(
            Cli::try_parse_from([
                "zzapi", "agents", "list", "--state", "running", "--branch", "codex/x", "--pr", "5"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "zzapi", "agents", "list", "--state", "running", "--branch", "codex/x"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "zzapi", "agents", "continue", "--branch", "codex/x", "--prompt", "go"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["zzapi", "agents", "status", "--pr", "5"]).is_ok());
        assert!(Cli::try_parse_from(["zzapi", "events", "--since-last"]).is_ok());
        assert!(
            Cli::try_parse_from(["zzapi", "agents", "continue", "--pr", "5", "--prompt", "go"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "zzapi",
                "agents",
                "delete",
                "--pr",
                "5",
                "--keep-last",
                "2",
                "--older-than",
                "7d"
            ])
            .is_ok()
        );
    }

    #[test]
    fn exec_only_succeeds_with_an_explicit_zero_exit_code() {
        assert!(exec_succeeded(&serde_json::json!({"exit_code": 0})));
        assert!(!exec_succeeded(&serde_json::json!({"exit_code": 1})));
        assert!(!exec_succeeded(&serde_json::json!({"exit_code": null})));
        assert!(!exec_succeeded(
            &serde_json::json!({"timed_out": true, "exit_code": 0})
        ));
        assert!(!exec_succeeded(&serde_json::json!({})));
    }

    #[test]
    fn exec_timeout_covers_the_relay_execution_window() {
        assert!(std::hint::black_box(EXEC_CLIENT_TIMEOUT_SECS) > 300);
    }

    #[test]
    fn epoch_reset_uses_the_response_cursor_without_replaying_events() {
        let response = serde_json::json!({
            "epoch": "new-epoch",
            "reset": true,
            "lost": false,
            "next": 9,
            "events": [{"sequence": 8}, {"sequence": 9}],
        });
        assert_eq!(
            event_progress(&response, 42, "old-epoch"),
            EventProgress {
                after: 9,
                epoch: "new-epoch".to_string(),
                reset: true,
                lost: false,
            }
        );
    }

    #[test]
    fn retention_loss_is_exposed_by_event_progress() {
        let response = serde_json::json!({"epoch": "current", "lost": true, "next": 12});
        assert!(event_progress(&response, 2, "current").lost);
    }

    #[test]
    fn log_retention_loss_only_applies_before_the_retained_cursor() {
        let response = serde_json::json!({"dropped_before": 8});
        assert!(log_retention_lost(&response, 0));
        assert!(!log_retention_lost(&response, 8));
        assert!(!log_retention_lost(&response, 9));
    }

    #[test]
    fn relay_base_accepts_an_explicit_port_for_local_or_proxied_relays() {
        assert_eq!(relay_base("relay.example"), "http://relay.example:8765");
        assert_eq!(relay_base("127.0.0.1:19876"), "http://127.0.0.1:19876");
    }

    #[test]
    fn update_selects_release_assets_for_published_platforms() {
        assert_eq!(
            latest_zzapi_url("linux", "x86_64").unwrap(),
            "https://github.com/ShukantPal/zigzag/releases/latest/download/zzapi-linux-x86_64"
        );
        assert_eq!(
            latest_zzapi_url("macos", "aarch64").unwrap(),
            "https://github.com/ShukantPal/zigzag/releases/latest/download/zzapi-macos-aarch64"
        );
    }

    #[test]
    fn update_reports_platforms_without_a_published_binary() {
        assert!(matches!(
            latest_zzapi_url("macos", "x86_64"),
            Err(Fail::Config(message)) if message.contains("macOS x86_64")
        ));
        assert!(matches!(
            latest_zzapi_url("linux", "aarch64"),
            Err(Fail::Config(message)) if message.contains("Linux aarch64")
        ));
        assert!(matches!(
            latest_zzapi_url("windows", "x86_64"),
            Err(Fail::Config(message)) if message.contains("windows/x86_64")
        ));
    }

    #[test]
    fn update_proxy_prefers_https_and_builds_authenticated_http_proxy() {
        let proxy = update_proxy_url([
            ("HTTP_PROXY", Some("http://http-proxy:8080")),
            ("HTTPS_PROXY", Some("http://user:secret@proxy.example:3128")),
        ]);
        assert_eq!(
            proxy.as_deref(),
            Some("http://user:secret@proxy.example:3128")
        );
        assert!(make_update_agent(proxy.as_deref()).is_ok());

        assert_eq!(
            update_proxy_url([
                ("HTTPS_PROXY", None),
                ("HTTP_PROXY", Some("http://http-proxy:8080")),
            ])
            .as_deref(),
            Some("http://http-proxy:8080")
        );
    }

    #[test]
    fn socket_host_uses_the_relay_host_not_its_http_port() {
        assert_eq!(
            socket_host("http://relay.example:8765"),
            Ok("relay.example")
        );
        assert_eq!(socket_host("http://127.0.0.1:19876"), Ok("127.0.0.1"));
        assert!(socket_host("https://relay.example:8765").is_err());
    }

    #[test]
    fn followed_json_is_json_lines_not_pretty_multi_document_output() {
        let first = json_output(&serde_json::json!({"next": 1}), true);
        let second = json_output(&serde_json::json!({"next": 2}), true);
        for line in [first, second] {
            assert!(serde_json::from_str::<serde_json::Value>(&line).is_ok());
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn agent_create_requires_an_explicit_execution_mode() {
        assert!(matches!(
            validate_agents_create_options(None, false, None),
            Err(Fail::Config(message)) if message == "must specify --branch or --no-branch"
        ));
        assert!(matches!(
            validate_agents_create_options(Some("codex/x"), true, None),
            Err(Fail::Config(message)) if message == "cannot specify both --branch and --no-branch"
        ));
        assert!(matches!(
            validate_agents_create_options(Some("codex/x"), false, Some(123)),
            Err(Fail::Config(message)) if message == "cannot specify both --branch and --pr"
        ));
        assert!(validate_agents_create_options(None, true, None).is_ok());
        assert!(validate_agents_create_options(None, false, Some(123)).is_ok());
    }

    #[test]
    fn agent_message_requires_exactly_one_delivery_mode() {
        assert!(
            Cli::try_parse_from([
                "zzapi",
                "agents",
                "message",
                "abc",
                "--steer",
                "change direction"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "zzapi",
                "agents",
                "message",
                "abc",
                "--queue",
                "after this turn"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["zzapi", "agents", "message", "abc"]).is_err());
        assert!(
            Cli::try_parse_from([
                "zzapi", "agents", "message", "abc", "--steer", "x", "--queue", "y"
            ])
            .is_err()
        );
    }

    #[test]
    fn agent_continue_requires_id_and_prompt() {
        assert!(
            Cli::try_parse_from([
                "zzapi",
                "agents",
                "continue",
                "abc",
                "--prompt",
                "follow up"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["zzapi", "agents", "continue", "abc"]).is_err());
        assert!(
            Cli::try_parse_from([
                "zzapi",
                "agents",
                "continue",
                "abc",
                "--prompt",
                "follow up",
                "--interrupt"
            ])
            .is_ok()
        );
    }

    #[test]
    fn branch_target_prefers_the_running_agent_over_stale_matches() {
        let agents = serde_json::json!([
            {"id":"old-agent", "branch":"feature/work", "state":"completed"},
            {"id":"live-agent", "branch":"feature/work", "state":"running"}
        ]);
        assert_eq!(
            select_live_agent_for_branch("feature/work", agents.as_array().unwrap()).unwrap(),
            "live-agent"
        );
    }

    #[test]
    fn branch_target_reports_ambiguous_live_agents_with_their_ids() {
        let agents = serde_json::json!([
            {"id":"agent-one", "branch":"feature/work", "state":"running"},
            {"id":"agent-two", "branch":"feature/work", "state":"running"}
        ]);
        let error =
            select_live_agent_for_branch("feature/work", agents.as_array().unwrap()).unwrap_err();
        assert!(error.message.contains("multiple live agents on branch"));
        assert!(error.message.contains("agent-one"));
        assert!(error.message.contains("agent-two"));
    }

    #[test]
    fn branch_target_without_a_running_agent_fails_clearly() {
        let agents = serde_json::json!([
            {"id":"old-agent", "branch":"feature/work", "state":"completed"}
        ]);
        let error =
            select_live_agent_for_branch("feature/work", agents.as_array().unwrap()).unwrap_err();
        assert_eq!(error.message, "no live agent on branch \"feature/work\"");
    }

    #[test]
    fn agent_id_exact_prefix_and_ambiguous_prefix_resolution_is_preserved() {
        let agents = serde_json::json!([
            {"id":"agent-123456"},
            {"id":"agent-987654"}
        ]);
        let agents = agents.as_array().unwrap();
        assert_eq!(
            resolve_agent_id_from_list("agent-123456", agents).unwrap(),
            "agent-123456"
        );
        assert_eq!(
            resolve_agent_id_from_list("agent-123", agents).unwrap(),
            "agent-123456"
        );
        assert_eq!(
            resolve_agent_id_from_list("agent-", agents)
                .unwrap_err()
                .message,
            "ambiguous prefix \"agent-\" matches 2 agents: agent-123456, agent-987654"
        );
        assert_eq!(
            resolve_agent_id_from_list("missing", agents)
                .unwrap_err()
                .message,
            "unknown_agent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_file_must_be_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "zzapi-private-token-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "secret\\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_private_token(path.to_str().unwrap()).unwrap(),
            "secret\\n"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_private_token(path.to_str().unwrap()),
            Err(Fail::Config(message)) if message.contains("mode 0600")
        ));
        std::fs::remove_file(path).unwrap();
    }
}
