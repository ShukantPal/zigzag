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
//!   zzapi agents list
//!   zzapi agents create --prompt "Fix the flaky test" \
//!       --project-dir /Users/shukant/Workspace/leveled-inc/leveled --branch codex/fix-flaky
//!   zzapi agents logs <id> --follow
//!   zzapi exec --bin gh --args pr list --repo ShukantPal/zigzag
//!   zzapi events --follow

use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::time::Duration;

const DEFAULT_HOSTNAME: &str = "100.101.237.83";
const DEFAULT_PORT: u16 = 8765;
const DEFAULT_TOKEN_FILE: &str = ".codex/zigzag.token";

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: u16,
    message: String,
}

enum Fail {
    Api(ApiError),
    Config(String),
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
    version
)]
struct Cli {
    /// Relay hostname (port is always 8765)
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
    /// Liveness probe
    Health,
    /// Manage agents
    Agents {
        #[command(subcommand)]
        cmd: AgentsCmd,
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
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// Long-poll timeout in seconds (max 55)
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// Epoch from a previous read
        #[arg(long, default_value = "")]
        epoch: String,
        /// Keep long-polling for new events
        #[arg(long)]
        follow: bool,
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
enum AgentsCmd {
    /// List registered agents
    List {
        /// Filter by state, e.g. running
        #[arg(long)]
        state: Option<String>,
        /// Filter by task id
        #[arg(long)]
        task_id: Option<String>,
    },
    /// Show one agent's record
    Get {
        /// Agent handle (unique prefix accepted)
        id: String,
    },
    /// Create and start a Codex agent
    Create {
        /// Inline prompt text or prompt-file path
        #[arg(long)]
        prompt: String,
        /// Project directory the agent works in
        #[arg(long)]
        project_dir: String,
        /// Branch for the agent's work
        #[arg(long)]
        branch: String,
        /// Worktree path (default /private/tmp/<branch-slug>/)
        #[arg(long)]
        worktree: Option<String>,
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
    /// Gracefully stop an agent (leaves worktree)
    Stop {
        /// Agent handle (unique prefix accepted)
        id: String,
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
        self.request("POST", path, &[], Some(body), 120)
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
    let mut contents = String::new();
    std::fs::File::open(&expanded)
        .and_then(|mut f| f.read_to_string(&mut contents))
        .map_err(|e| {
            Fail::Config(format!(
                "cannot read token file {expanded}: {e}\nset ZIGZAG_TOKEN or --token-file"
            ))
        })?;
    let token = contents.trim().to_string();
    if token.is_empty() {
        return Err(Fail::Config(format!("token file {expanded} is empty")));
    }
    Ok(token)
}

fn make_client(cli: &Cli) -> Result<Client, Fail> {
    let base = format!("http://{}:{DEFAULT_PORT}", cli.hostname);
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
        println!("{}", serde_json::to_string_pretty(payload).unwrap());
    } else {
        table();
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
    let matches: Vec<String> = resp
        .get("agents")
        .and_then(|a| a.as_array())
        .map(|agents| {
            agents
                .iter()
                .filter_map(|a| a.get("id").and_then(|i| i.as_str()))
                .filter(|id| id.starts_with(prefix))
                .map(|id| id.to_string())
                .collect()
        })
        .unwrap_or_default();
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

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

fn cmd_agents_list(
    client: &Client,
    state: Option<&str>,
    task_id: Option<&str>,
) -> Result<(), Fail> {
    let mut q: Vec<(&str, String)> = Vec::new();
    if let Some(st) = state {
        q.push(("state", st.to_string()));
    }
    if let Some(t) = task_id {
        q.push(("task_id", t.to_string()));
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
    branch: &str,
    worktree: Option<&str>,
    model: Option<&str>,
    approval_mode: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<(), Fail> {
    let mut body = serde_json::json!({
        "prompt": prompt,
        "project_dir": project_dir,
        "branch": branch,
    });
    if let Some(w) = worktree {
        body["worktree"] = serde_json::Value::String(w.to_string());
    }
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
        println!("worktree: {}", s(&resp, "worktree"));
    });
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

fn cmd_agents_stop(client: &Client, id: &str) -> Result<(), Fail> {
    let id = resolve_agent_id(client, id)?;
    let resp = client.request("DELETE", &format!("/v1/agents/{id}"), &[], None, 120)?;
    emit(client, &resp, || {
        println!("stopped {}", s(&resp, "id"));
    });
    Ok(())
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
            println!("{}", serde_json::to_string_pretty(&resp).unwrap());
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
    let resp = client.post("/v1/exec", &body)?;
    if s(&resp, "error") == "denied" {
        println!("denied (id={})", s(&resp, "id"));
        std::process::exit(1);
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
    let exit_code = resp.get("exit_code").and_then(|e| e.as_i64());
    if !matches!(exit_code, Some(0) | None) {
        std::process::exit(1);
    }
    Ok(())
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
    after: u64,
    timeout: u64,
    epoch: &str,
    follow: bool,
) -> Result<(), Fail> {
    let mut after = after;
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
            println!("{}", serde_json::to_string_pretty(&resp).unwrap());
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
        if resp.get("reset").and_then(|r| r.as_bool()).unwrap_or(false) {
            eprintln!("warning: event store reset; restarting from 0");
            after = 0;
            epoch.clear();
            if !follow {
                break;
            }
            continue;
        }
        after = after_u64(resp.get("next")).unwrap_or(after);
        epoch = s(&resp, "epoch");
        if !follow {
            break;
        }
    }
    Ok(())
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
    let client = make_client(&cli)?;
    // clap's trailing_var_arg keeps a leading "--" out, but strip it anyway
    // for ergonomics (matches the old Python client).
    let strip_dd = |args: &mut Vec<String>| {
        if args.first().map(|x| x == "--").unwrap_or(false) {
            args.remove(0);
        }
    };
    match cli.command {
        Commands::Health => cmd_health(&client),
        Commands::Agents { cmd } => match cmd {
            AgentsCmd::List { state, task_id } => {
                cmd_agents_list(&client, state.as_deref(), task_id.as_deref())
            }
            AgentsCmd::Get { id } => cmd_agents_get(&client, &id),
            AgentsCmd::Create {
                prompt,
                project_dir,
                branch,
                worktree,
                model,
                approval_mode,
                timeout_secs,
            } => cmd_agents_create(
                &client,
                &prompt,
                &project_dir,
                &branch,
                worktree.as_deref(),
                model.as_deref(),
                approval_mode.as_deref(),
                timeout_secs,
            ),
            AgentsCmd::Pause { id } => cmd_agents_pause(&client, &id),
            AgentsCmd::Resume { id } => cmd_agents_resume(&client, &id),
            AgentsCmd::Stop { id } => cmd_agents_stop(&client, &id),
            AgentsCmd::Logs {
                id,
                stream,
                after,
                tail,
                follow,
                prefix,
            } => cmd_agents_logs(&client, &id, stream.as_deref(), after, tail, follow, prefix),
        },
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
        } => cmd_events(&client, after, timeout, &epoch, follow),
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
    }
    // Note: Ctrl-C during --follow terminates via the default SIGINT
    // disposition; the shell reports exit code 130.
}
