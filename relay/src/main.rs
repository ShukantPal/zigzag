use clap::{Parser, Subcommand};
use relay_core::{
    AgentRecord, AgentRegistry, Json, ReadResult, Store, parse_json, parse_rfc3339_millis,
    read_secret_file,
};
use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

mod exec;
mod review_loop;
mod update;

const MAX_BODY: usize = 64 * 1024;
/// Upper bound on simultaneous in-flight connections. The accept loop
/// sheds excess connections with 503 instead of spawning unbounded
/// threads: each thread carries a stack plus a 10s read timeout, so a
/// slow flood could otherwise exhaust memory or file descriptors.
const MAX_CONNECTIONS: usize = 32;
/// Upper bound on the request line plus all header lines, in bytes.
const MAX_HEADER_BLOCK_BYTES: usize = 8 * 1024;
/// Upper bound on the number of header lines in one request.
const MAX_HEADER_COUNT: usize = 100;
const MAX_FINISHED_PROCS: usize = 128;
const FINISHED_PROC_RETENTION: Duration = Duration::from_secs(60 * 60);
const COMPAT_OUTPUT_CAP: usize = 2 * 1024 * 1024;
#[cfg(any(target_os = "macos", test))]
const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
#[cfg(any(target_os = "macos", test))]
const SESSION_IS_REMOTE: u32 = 0x1000;

#[derive(Debug)]
struct Config {
    secret_file: PathBuf,
    control_secret_file: Option<PathBuf>,
    state_file: PathBuf,
    agent_registry_file: PathBuf,
    port: u16,
    tailscale_ip: Option<IpAddr>,
    max_events: usize,
    github_watch_repos: Vec<String>,
    github_watch_interval: Duration,
    update_directory: PathBuf,
    update_interval: Duration,
    update_policy: update::Policy,
    update_ready_file: Option<PathBuf>,
    review_state_file: PathBuf,
}
struct Server {
    secret: String,
    control_secret: Option<String>,
    store: Arc<Store>,
    supervisor: Supervisor,
    updater: Arc<update::Manager>,
    review_state_file: PathBuf,
    review_loop_shadow: bool,
    review_config: Mutex<Option<Arc<review_loop::ReviewLoopConfig>>>,
}

/// Live handles deliberately disappear on restart; the durable half lives in
/// `relay-core::AgentRegistry` and records the resulting orphan/loss state.
struct Supervisor {
    registry: Arc<AgentRegistry>,
    procs: Mutex<HashMap<String, ProcEntry>>,
}

struct ProcEntry {
    child: Child,
    process_group: i32,
    id: String,
    bin: String,
    subcommand: String,
    spawned_at: Instant,
    finished_at: Option<Instant>,
    termination_requested_at: Option<Instant>,
    termination_escalated: bool,
    leader_reaped: bool,
    exit_code: Option<i32>,
    stdout: Arc<Mutex<CappedOutput>>,
    stderr: Arc<Mutex<CappedOutput>>,
}

#[derive(Default)]
struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
    complete: bool,
}

impl CappedOutput {
    fn append(&mut self, bytes: &[u8]) {
        let room = COMPAT_OUTPUT_CAP.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.truncated |= bytes.len() > room;
    }

    fn snapshot(&self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.truncated,
        )
    }
}

enum ProcRoute<'a> {
    Poll(&'a str),
    Kill(&'a str),
}
enum AgentRoute<'a> {
    List,
    Status(&'a str),
    Logs(&'a str),
}
struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct SpawnRequest {
    command: exec::ExecRequest,
    execution_id: Option<String>,
}

#[derive(Debug)]
enum ReadRequestError {
    Message(String),
    ExecutionDenied,
    HeadersTooLarge,
}

const SERVER_USAGE: &str = "usage: zigzag --secret-file PATH --state-file PATH [--control-secret-file PATH] [--port 8765] [--max-events 1000] [--watch-repo OWNER/REPO] [--watch-interval 30] [--update-dir PATH] [--update-interval 3600] [--update-policy enabled|paused|pin:VERSION]";
const TIMELINE_USAGE: &str =
    "usage: zigzag timeline <task-id> --state-file PATH (or ZIGZAG_STATE_FILE)";
const CONFIG_USAGE: &str = "usage: zigzag config (get-allowlist | set-allowlist --file PATH)";

/// Server-mode flags. clap handles tokenizing, `--flag value` pairing, and
/// environment fallbacks; the domain validation in `server_config` keeps the
/// exact historical error strings so invoker behavior is unchanged.
#[derive(Parser, Debug)]
#[command(name = "zigzag", disable_help_flag = true, disable_version_flag = true)]
struct ServeArgs {
    #[arg(long = "secret-file", env = "ZIGZAG_SECRET_FILE")]
    secret_file: Option<PathBuf>,
    #[arg(long = "control-secret-file", env = "ZIGZAG_CONTROL_SECRET_FILE")]
    control_secret_file: Option<PathBuf>,
    #[arg(long = "state-file", env = "ZIGZAG_STATE_FILE")]
    state_file: Option<PathBuf>,
    #[arg(long = "port", default_value = "8765", value_parser = parse_port)]
    port: u16,
    #[arg(long = "tailscale-ip", value_parser = parse_ip_address)]
    tailscale_ip: Option<IpAddr>,
    #[arg(
        long = "max-events",
        default_value = "1000",
        value_parser = parse_max_events
    )]
    max_events: usize,
    #[arg(long = "watch-repo", value_parser = parse_github_repo)]
    github_watch_repos: Vec<String>,
    #[arg(
        long = "watch-interval",
        default_value = "30",
        value_parser = parse_watch_interval
    )]
    github_watch_interval_secs: u64,
    #[arg(long = "update-dir", env = "ZIGZAG_UPDATE_DIR")]
    update_directory: Option<PathBuf>,
    #[arg(
        long = "update-interval",
        env = "ZIGZAG_UPDATE_INTERVAL",
        default_value = "3600",
        value_parser = parse_update_interval
    )]
    update_interval_secs: u64,
    #[arg(
        long = "update-policy",
        env = "ZIGZAG_UPDATE_POLICY",
        default_value = "enabled",
        value_parser = update::Policy::parse
    )]
    update_policy: update::Policy,
    #[arg(long = "update-ready-file")]
    update_ready_file: Option<PathBuf>,
    /// Manual help flag: `--help` historically returns the usage string as an
    /// error (exit 1 via main), so clap's auto help stays disabled.
    #[arg(long = "help", short = 'h', action = clap::ArgAction::SetTrue)]
    help: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "zigzag timeline",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct TimelineArgs {
    task_id: Option<String>,
    #[arg(long = "state-file", env = "ZIGZAG_STATE_FILE")]
    state_file: Option<PathBuf>,
    /// Manual help flag preserving the historical usage-error behavior.
    #[arg(long = "help", short = 'h', action = clap::ArgAction::SetTrue)]
    help: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "zigzag config",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct ConfigArgs {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
    /// Manual help flag preserving the historical usage-error behavior.
    #[arg(long = "help", short = 'h', action = clap::ArgAction::SetTrue)]
    help: bool,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    #[command(name = "get-allowlist")]
    GetAllowlist,
    #[command(name = "set-allowlist")]
    SetAllowlist {
        #[arg(long = "file")]
        file: String,
    },
}

fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse()
        .map_err(|_| "--port must be a valid u16".to_owned())
}

fn parse_ip_address(value: &str) -> Result<IpAddr, String> {
    value
        .parse()
        .map_err(|_| "--tailscale-ip must be an IP address".to_owned())
}

fn parse_max_events(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| "--max-events must be a positive integer".to_owned())
}

fn parse_github_repo(value: &str) -> Result<String, String> {
    if valid_github_repo(value) {
        Ok(value.to_owned())
    } else {
        Err("--watch-repo must be an OWNER/REPO GitHub name".to_owned())
    }
}

fn parse_watch_interval(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| "--watch-interval must be an integer".to_owned())
}

fn parse_update_interval(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| "--update-interval must be an integer".to_owned())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zigzag: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("config") {
        return run_config(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("timeline") {
        return run_timeline(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("updates") {
        return update::run_control(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("update-watchdog") {
        return update::run_watchdog(&arguments[1..]);
    }
    let config = server_config(arguments.clone())?;
    let secret = read_secret_file(&config.secret_file)?;
    let control_secret = config
        .control_secret_file
        .as_deref()
        .map(read_secret_file)
        .transpose()?;
    let updater = Arc::new(update::Manager::new(update::Config {
        directory: config.update_directory.clone(),
        interval: config.update_interval,
        policy: config.update_policy.clone(),
        ready_file: config.update_ready_file.clone(),
    }));
    let review_loop_shadow = env::var("ZIGZAG_REVIEW_LOOP_SHADOW").as_deref() == Ok("1");
    let state = Arc::new(Server {
        secret,
        control_secret,
        store: Arc::new(Store::open(config.state_file, config.max_events)?),
        supervisor: Supervisor {
            registry: Arc::new(AgentRegistry::open(config.agent_registry_file)?),
            procs: Mutex::new(HashMap::new()),
        },
        updater: Arc::clone(&updater),
        review_state_file: config.review_state_file.clone(),
        review_loop_shadow,
        review_config: Mutex::new(None),
    });
    for agent in state
        .supervisor
        .registry
        .recover(recovered_agent_identity_matches)?
    {
        replay_recovered_lifecycle(&state.store, &agent)?;
    }
    start_reaper(Arc::clone(&state));
    let mut review_loop_authoritative = false;
    match review_loop::default_config_path() {
        Ok(path) => match review_loop::load_config(&path) {
            Ok(personal) if personal.review_loop.enabled => {
                let review_config = Arc::new(personal.review_loop);
                match review_loop::start(
                    Arc::clone(&state),
                    (*review_config).clone(),
                    config.review_state_file.clone(),
                    review_loop_shadow,
                ) {
                    Ok(()) => {
                        *state
                            .review_config
                            .lock()
                            .map_err(|_| "review config lock poisoned".to_owned())? =
                            Some(review_config);
                        review_loop_authoritative = !review_loop_shadow;
                    }
                    Err(error) => eprintln!("review loop disabled: {error}"),
                }
            }
            Ok(_) => eprintln!("review loop disabled by ~/.zigzag/config.yaml"),
            Err(violations) => {
                eprintln!("review loop disabled: invalid ~/.zigzag/config.yaml");
                for violation in violations {
                    eprintln!("review loop config: {violation}");
                }
            }
        },
        Err(violation) => eprintln!("review loop disabled: {violation}"),
    }
    if should_start_legacy_watch(review_loop_authoritative, &config.github_watch_repos) {
        let state = Arc::clone(&state);
        let repos = config.github_watch_repos.clone();
        let interval = config.github_watch_interval;
        thread::spawn(move || github_watch_loop(state, repos, interval));
    }
    let tailnet = config.tailscale_ip.unwrap_or(resolve_tailscale_ip()?);
    let addresses = [
        SocketAddr::new(IpAddr::from([127, 0, 0, 1]), config.port),
        SocketAddr::new(tailnet, config.port),
    ];
    // One limiter for both listeners: the cap bounds total handler threads,
    // not threads per socket.
    let limiter = Arc::new(ConnectionLimiter::new(MAX_CONNECTIONS));
    for address in addresses {
        let server = tiny_http::Server::http(address)
            .map_err(|error| format!("could not bind {address}: {error}"))?;
        let state = Arc::clone(&state);
        let limiter = Arc::clone(&limiter);
        println!("zigzag listening on http://{address}");
        thread::spawn(move || serve(server, state, limiter));
    }
    // The replacement only signals readiness after it has opened durable state
    // and rebound both listeners. The watchdog rolls back if this does not
    // happen; no launchctl restart is involved.
    let replacement_version = env::var("ZIGZAG_UPDATE_VERSION").ok();
    updater.acknowledge_ready(replacement_version.as_deref())?;
    if let Some(version) = replacement_version {
        let execution_id = new_execution_id()?;
        state.store.add(relay_event(
            "relay_update_applied",
            "relay-update",
            &execution_id,
            Json::Object(vec![("new_version".to_owned(), Json::String(version))]),
        ))?;
    }
    let update_state = Arc::clone(&state);
    let update_audit_state = Arc::clone(&state);
    let update_execution = Mutex::new(None::<String>);
    let update_arguments = update::persistent_server_args(&arguments);
    updater.start(
        Arc::new(move || {
            !update_state
                .supervisor
                .registry
                .list(Some("running"), None)
                .is_empty()
        }),
        Arc::new(move |kind, payload| {
            let Ok(mut execution_id) = update_execution.lock() else {
                return;
            };
            if kind == "relay_update_check_started" || execution_id.is_none() {
                *execution_id = new_execution_id().ok();
            }
            if let Some(execution_id) = execution_id.as_deref() {
                let _ = update_audit_state.store.add(relay_event(
                    kind,
                    "relay-update",
                    execution_id,
                    payload,
                ));
            }
        }),
        update_arguments,
        config.secret_file.clone(),
        config.port,
    );
    loop {
        thread::park();
    }
}

fn run_timeline(arguments: &[String]) -> Result<(), String> {
    let args = TimelineArgs::try_parse_from(
        std::iter::once("zigzag".to_owned()).chain(arguments.iter().cloned()),
    )
    .map_err(|error| error.to_string())?;
    if args.help {
        return Err(TIMELINE_USAGE.to_owned());
    }
    let task_id = args
        .task_id
        .ok_or_else(|| "timeline requires a task id".to_owned())?;
    let state_file = args
        .state_file
        .ok_or_else(|| "--state-file or ZIGZAG_STATE_FILE is required for timeline".to_owned())?;
    let events = Store::open(state_file, 1_000)?.timeline(&task_id)?;
    print!("{}", timeline_output(&task_id, &events));
    Ok(())
}

fn timeline_output(task_id: &str, events: &[Json]) -> String {
    if events.is_empty() {
        return format!("No durable audit events for task {task_id}.\n");
    }
    let mut output = format!("Timeline for {task_id}\n");
    for event in events {
        output.push_str(&format!(
            "{}  {}  {}  {}",
            event_text(event, "occurred_at"),
            event_text(event, "source"),
            event_text(event, "kind"),
            event_text(event, "clock"),
        ));
        output.push('\n');
    }
    output.push_str("\nPhase durations (only same-clock facts are subtracted):\n");
    for (label, start, end) in [
        ("dispatch", "task_dispatched", "relay_request_started"),
        ("relay/launch", "relay_accepted", "process_spawned"),
        ("agent work", "process_spawned", "process_completed"),
        ("agent failure", "process_spawned", "process_failed"),
        ("time to first output", "process_spawned", "first_output"),
        ("delivery/poll health", "poll_started", "poller_received"),
        ("review", "review_wait_started", "review_work_started"),
        ("human wait", "human_wait_started", "human_wait_ended"),
    ] {
        if let Some((left, right)) = phase_events(events, start, end) {
            match same_clock_duration(left, right) {
                Some(duration) => {
                    output.push_str(&format!("{label}: {}\n", format_duration(duration)))
                }
                None => output.push_str(&format!(
                    "{label}: cross-clock/unknown ({} → {})\n",
                    event_text(left, "occurred_at"),
                    event_text(right, "occurred_at")
                )),
            }
        }
    }
    output
}

fn event_text<'a>(event: &'a Json, field: &str) -> &'a str {
    event.object(field).and_then(Json::as_str).unwrap_or("?")
}

fn phase_events<'a>(events: &'a [Json], start: &str, end: &str) -> Option<(&'a Json, &'a Json)> {
    for (index, left) in events.iter().enumerate() {
        if event_text(left, "kind") != start {
            continue;
        }
        let execution_id = event_text(left, "execution_id");
        if let Some(right) = events[index + 1..].iter().find(|event| {
            event_text(event, "kind") == end && event_text(event, "execution_id") == execution_id
        }) {
            return Some((left, right));
        }
    }
    None
}

fn same_clock_duration(left: &Json, right: &Json) -> Option<u64> {
    (event_text(left, "clock") == event_text(right, "clock"))
        .then(|| {
            timestamp_millis(event_text(right, "occurred_at"))?
                .checked_sub(timestamp_millis(event_text(left, "occurred_at"))?)
        })
        .flatten()
}

fn timestamp_millis(value: &str) -> Option<u64> {
    parse_rfc3339_millis(value)
}

fn format_duration(millis: u64) -> String {
    format!("{}.{:03}s", millis / 1_000, millis % 1_000)
}

fn server_config(arguments: Vec<String>) -> Result<Config, String> {
    let args = ServeArgs::try_parse_from(std::iter::once("zigzag".to_owned()).chain(arguments))
        .map_err(|error| error.to_string())?;
    if args.help {
        return Err(SERVER_USAGE.to_owned());
    }
    let secret_file = args
        .secret_file
        .ok_or_else(|| "--secret-file or ZIGZAG_SECRET_FILE is required".to_owned())?;
    let state_file = args
        .state_file
        .ok_or_else(|| "--state-file or ZIGZAG_STATE_FILE is required".to_owned())?;
    if args.max_events == 0 {
        return Err("--max-events must be greater than zero".to_owned());
    }
    if let Some(address) = args.tailscale_ip
        && !is_tailscale_ipv4(address)
    {
        return Err("--tailscale-ip must be a Tailscale IPv4 address".to_owned());
    }
    if !(30..=3600).contains(&args.github_watch_interval_secs) {
        return Err("--watch-interval must be between 30 and 3600 seconds".to_owned());
    }
    // The old hand-rolled parser only applied this bound to the flag, not the
    // environment variable; applying it uniformly turns a misconfiguration
    // into a loud startup error either way.
    if args.update_interval_secs > 24 * 60 * 60 {
        return Err("--update-interval must be at most 86400 seconds".to_owned());
    }
    let mut github_watch_repos = Vec::new();
    for repo in args.github_watch_repos {
        if !github_watch_repos.contains(&repo) {
            github_watch_repos.push(repo);
        }
    }
    let agent_registry_file = state_file.with_extension("agents.json");
    let update_directory = args.update_directory.unwrap_or_else(|| {
        state_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("relay")
    });
    let review_state_file = state_file.with_extension("reviews.json");
    Ok(Config {
        secret_file,
        control_secret_file: args.control_secret_file,
        state_file,
        agent_registry_file,
        port: args.port,
        tailscale_ip: args.tailscale_ip,
        max_events: args.max_events,
        github_watch_repos,
        github_watch_interval: Duration::from_secs(args.github_watch_interval_secs),
        update_directory,
        update_interval: Duration::from_secs(args.update_interval_secs),
        update_policy: args.update_policy,
        update_ready_file: args.update_ready_file,
        review_state_file,
    })
}

fn valid_github_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && owner
            .bytes()
            .chain(name.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn should_start_legacy_watch(review_loop_authoritative: bool, repositories: &[String]) -> bool {
    !review_loop_authoritative && !repositories.is_empty()
}

fn github_watch_loop(state: Arc<Server>, repos: Vec<String>, interval: Duration) {
    loop {
        for repo in &repos {
            match github_open_pull_requests(repo) {
                Ok(pull_requests) => {
                    for number in pull_requests {
                        let id = format!("github-pr-opened:{repo}:{number}");
                        let payload = Json::Object(vec![
                            ("id".to_owned(), Json::String(id.clone())),
                            (
                                "kind".to_owned(),
                                Json::String("github_pr_opened".to_owned()),
                            ),
                            ("repository".to_owned(), Json::String(repo.clone())),
                            ("pull_request".to_owned(), Json::number(number)),
                            (
                                "url".to_owned(),
                                Json::String(format!("https://github.com/{repo}/pull/{number}")),
                            ),
                        ]);
                        match state.store.add(payload) {
                            Ok((_, false)) => {
                                eprintln!("queued GitHub PR watchdog event for {repo}#{number}")
                            }
                            Ok((_, true)) => {}
                            Err(_) => eprintln!(
                                "could not persist GitHub PR watchdog event for {repo}#{number}"
                            ),
                        }
                    }
                }
                Err(error) => eprintln!("GitHub PR watch for {repo} failed: {error}"),
            }
        }
        thread::sleep(interval);
    }
}

pub(crate) fn github_open_pull_requests(repo: &str) -> Result<Vec<u64>, String> {
    let policy = require_gui_login_session().and_then(|_| exec::load_policy())?;
    let request = exec::ExecRequest {
        id: format!("github-pr-scan-{repo}"),
        bin: "gh".to_owned(),
        args: vec![
            "api".to_owned(),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
            format!("repos/{repo}/pulls?state=open&per_page=100"),
        ],
    };
    let path = policy
        .allowed_path(&request.bin, &request.args)
        .ok_or_else(|| "the gh policy does not allow the PR scan".to_owned())?;
    let result = exec::run(path, request);
    if result.timed_out || result.truncated || result.exit_code != Some(0) {
        return Err("GitHub PR discovery did not complete successfully".to_owned());
    }
    parse_github_open_pull_requests(&result.stdout)
}

fn parse_github_open_pull_requests(output: &str) -> Result<Vec<u64>, String> {
    let value = parse_json(output)
        .map_err(|_| "GitHub PR discovery did not return the expected JSON".to_owned())?;
    let Json::Array(pull_requests) = value else {
        return Err("GitHub PR discovery did not return a JSON array".to_owned());
    };
    let pull_requests: Vec<_> = pull_requests
        .iter()
        .flat_map(|page| match page {
            Json::Array(pull_requests) => pull_requests.iter().collect(),
            pull_request => vec![pull_request],
        })
        .collect();
    pull_requests
        .iter()
        .map(|pull_request| {
            let number = pull_request
                .object("number")
                .and_then(Json::as_u64)
                .filter(|number| *number > 0)
                .ok_or_else(|| "GitHub PR discovery result is missing a PR number".to_owned())?;
            Ok(number)
        })
        .collect()
}

fn run_config(arguments: &[String]) -> Result<(), String> {
    require_gui_login_session()?;
    let args = ConfigArgs::try_parse_from(
        std::iter::once("zigzag".to_owned()).chain(arguments.iter().cloned()),
    )
    .map_err(|error| error.to_string())?;
    match config_command(args)? {
        ConfigCommand::GetAllowlist => {
            let policy = exec::load_policy()?;
            println!("{}", policy.canonical_json());
            Ok(())
        }
        ConfigCommand::SetAllowlist { file } => {
            let contents = std::fs::read_to_string(&file)
                .map_err(|error| format!("could not read allowlist file {file}: {error}"))?;
            let policy = exec::Policy::parse(&contents)?;
            exec::store_policy(&policy)?;
            println!("{}", policy.canonical_json());
            Ok(())
        }
    }
}

/// Validates the parsed `config` subcommand, preserving the old hand-rolled
/// usage errors for missing/extra arguments.
fn config_command(args: ConfigArgs) -> Result<ConfigCommand, String> {
    if args.help {
        return Err(CONFIG_USAGE.to_owned());
    }
    match args.command {
        None => Err(CONFIG_USAGE.to_owned()),
        Some(ConfigCommand::SetAllowlist { ref file }) if file.is_empty() => {
            Err(CONFIG_USAGE.to_owned())
        }
        Some(command) => Ok(command),
    }
}

#[cfg(any(target_os = "macos", test))]
fn is_local_gui_session(status: i32, attributes: u32) -> bool {
    status == 0
        && attributes & SESSION_HAS_GRAPHIC_ACCESS != 0
        && attributes & SESSION_IS_REMOTE == 0
}

/// Policy updates are intentionally an owner action from the local Aqua
/// session, never an SSH action. Keychain access alone does not establish
/// which terminal invoked this executable, so check the caller's session too.
#[cfg(target_os = "macos")]
fn require_gui_login_session() -> Result<(), String> {
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
    }

    let mut session_id = 0;
    let mut attributes = 0;
    // `callerSecuritySession` asks macOS about this process's session.
    let status =
        unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut session_id, &mut attributes) };
    if is_local_gui_session(status, attributes) {
        Ok(())
    } else {
        Err(
            "privileged daemon operations require Shukant's local macOS GUI login session"
                .to_owned(),
        )
    }
}

#[cfg(not(target_os = "macos"))]
fn require_gui_login_session() -> Result<(), String> {
    Err("privileged daemon operations require Shukant's local macOS GUI login session".to_owned())
}

fn resolve_tailscale_ip() -> Result<IpAddr, String> {
    let output = Command::new("tailscale").args(["ip", "-4"]).output().map_err(|error| format!("could not run tailscale ip -4: {error}; use --tailscale-ip only for explicit test/development overrides"))?;
    if !output.status.success() {
        return Err("tailscale ip -4 failed; Zigzag will not bind broadly".to_owned());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "tailscale ip -4 produced non-UTF-8 output".to_owned())?;
    stdout
        .lines()
        .next()
        .ok_or_else(|| "tailscale ip -4 returned no address".to_owned())?
        .parse()
        .map_err(|_| "tailscale ip -4 returned an invalid address".to_owned())
}

fn is_tailscale_ipv4(address: IpAddr) -> bool {
    matches!(address, IpAddr::V4(address) if address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
}

/// Admission control for inbound connections. The permit is held for the
/// whole handler thread and released on drop, so at most `max` connections
/// are ever in flight at once.
struct ConnectionLimiter {
    active: Mutex<usize>,
    max: usize,
}

struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
}

impl ConnectionLimiter {
    fn new(max: usize) -> Self {
        Self {
            active: Mutex::new(0),
            max,
        }
    }

    /// Best-effort admission: a permit while fewer than `max` connections are
    /// in flight, `None` when the server is saturated.
    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut active = self.active.lock().expect("connection limiter poisoned");
        if *active >= self.max {
            return None;
        }
        *active += 1;
        Some(ConnectionPermit {
            limiter: Arc::clone(self),
        })
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut active = self
            .limiter
            .active
            .lock()
            .expect("connection limiter poisoned");
        *active = active.saturating_sub(1);
    }
}

fn serve(server: tiny_http::Server, state: Arc<Server>, limiter: Arc<ConnectionLimiter>) {
    for request in server.incoming_requests() {
        match limiter.try_acquire() {
            Some(permit) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _permit = permit;
                    let _ = handle(request, state);
                });
            }
            None => {
                eprintln!("zigzag connection shed: already at {MAX_CONNECTIONS} connections");
                let body = error("too_many_connections").to_json();
                let _ = request.respond(
                    tiny_http::Response::from_data(body.as_bytes())
                        .with_status_code(tiny_http::StatusCode::from(503u16)),
                );
            }
        }
    }
}

fn handle(request: tiny_http::Request, state: Arc<Server>) -> Result<(), String> {
    handle_with_policy(request, state, || {
        require_gui_login_session().and_then(|_| exec::load_policy())
    })
}

fn handle_with_policy<F>(
    request: tiny_http::Request,
    state: Arc<Server>,
    load_policy: F,
) -> Result<(), String>
where
    F: Fn() -> Result<exec::Policy, String>,
{
    handle_with_services(request, state, load_policy, review_loop::gate_report)
}

fn handle_with_services<F, G>(
    http_request: tiny_http::Request,
    state: Arc<Server>,
    load_policy: F,
    gate_report: G,
) -> Result<(), String>
where
    F: Fn() -> Result<exec::Policy, String>,
    G: Fn(
        &str,
        u64,
        &std::path::Path,
        bool,
        &review_loop::ReviewLoopConfig,
    ) -> Result<serde_json::Value, String>,
{
    let incoming = receive(http_request);
    let mut responder = incoming.responder;
    let request = match incoming.parsed {
        Ok(request) => request,
        Err(ReadRequestError::ExecutionDenied) => {
            denied(&mut responder, "")?;
            return Ok(());
        }
        Err(ReadRequestError::HeadersTooLarge) => {
            reply(
                &mut responder,
                431,
                Json::Object(vec![(
                    "error".to_owned(),
                    Json::String("request header fields too large".to_owned()),
                )]),
            )?;
            return Ok(());
        }
        Err(ReadRequestError::Message(error)) => {
            reply(
                &mut responder,
                400,
                Json::Object(vec![("error".to_owned(), Json::String(error))]),
            )?;
            return Ok(());
        }
    };
    let request_path = request.target.split('?').next().unwrap_or("");
    let control_route =
        request.method == "POST" && matches!(proc_route(request_path), Some(ProcRoute::Kill(_)));
    let Some(required_secret) = (if control_route {
        state.control_secret.as_deref()
    } else {
        Some(state.secret.as_str())
    }) else {
        return reply(&mut responder, 404, error("not_found"));
    };
    if !authorized(
        request
            .headers
            .get("authorization")
            .map(String::as_str)
            .unwrap_or(""),
        required_secret,
    ) {
        reply(&mut responder, 401, error("unauthorized"))?;
        return Ok(());
    }
    match (request.method.as_str(), request_path) {
        ("GET", "/v1/health") => reply(
            &mut responder,
            200,
            Json::Object(vec![("status".to_owned(), Json::String("ok".to_owned()))]),
        ),
        ("POST", "/v1/events") => post(&mut responder, &state, request.body),
        ("GET", "/v1/events") => get(&mut responder, &state, &request.target),
        ("POST", "/v1/exec") => exec_request(&mut responder, request.body),
        ("POST", "/v1/spawn") => spawn_request(&mut responder, &state, request.body, load_policy()),
        ("GET", "/v1/review-gate") => {
            review_gate_request(&mut responder, &state, &request.target, gate_report)
        }
        ("GET", path) if agent_route(path).is_some() => agent_request(
            &mut responder,
            &state,
            &request.target,
            agent_route(path).expect("checked"),
        ),
        ("GET", path) => match proc_route(path) {
            Some(ProcRoute::Poll(handle)) => poll_proc(&mut responder, &state, handle),
            Some(ProcRoute::Kill(_)) => reply(&mut responder, 404, error("not_found")),
            None => reply(&mut responder, 404, error("not_found")),
        },
        ("POST", path) => match proc_route(path) {
            Some(ProcRoute::Kill(handle)) => kill_proc(&mut responder, &state, handle),
            Some(ProcRoute::Poll(_)) => reply(&mut responder, 404, error("not_found")),
            None => reply(&mut responder, 404, error("not_found")),
        },
        _ => reply(&mut responder, 404, error("not_found")),
    }
}

fn review_gate_request<G>(
    responder: &mut HttpResponder,
    state: &Server,
    target: &str,
    gate_report: G,
) -> Result<(), String>
where
    G: Fn(
        &str,
        u64,
        &std::path::Path,
        bool,
        &review_loop::ReviewLoopConfig,
    ) -> Result<serde_json::Value, String>,
{
    let (repository, number) = match review_gate_parameters(target) {
        Ok(parameters) => parameters,
        Err(()) => return reply(responder, 400, error("invalid_review_gate_query")),
    };
    let config = match state.review_config.lock() {
        Ok(config) => config.clone(),
        Err(_) => return reply(responder, 500, error("review_gate_failed")),
    };
    let Some(config) = config else {
        return reply(responder, 500, error("review_gate_failed"));
    };
    match gate_report(
        &repository,
        number,
        &state.review_state_file,
        state.review_loop_shadow,
        &config,
    ) {
        Ok(report) => {
            let encoded = serde_json::to_string(&report)
                .map_err(|error| format!("could not encode review gate report: {error}"))?;
            let response = parse_json(&encoded)
                .map_err(|_| "could not convert review gate report".to_owned())?;
            reply(responder, 200, response)
        }
        Err(error_message) => {
            eprintln!("review gate failed for {repository}#{number}: {error_message}");
            reply(responder, 500, error("review_gate_failed"))
        }
    }
}

fn review_gate_parameters(target: &str) -> Result<(String, u64), ()> {
    let values = query(target).map_err(|_| ())?;
    let allowed = ["repository", "pull_request"];
    if values.len() != allowed.len() || values.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(());
    }
    let repository = values
        .get("repository")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or(())?;
    let number = values
        .get("pull_request")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|number| *number > 0)
        .ok_or(())?;
    Ok((repository, number))
}

fn agent_route(path: &str) -> Option<AgentRoute<'_>> {
    if path == "/v1/agents" {
        return Some(AgentRoute::List);
    }
    let rest = path.strip_prefix("/v1/agents/")?;
    if let Some(id) = rest.strip_suffix("/logs") {
        return (!id.is_empty() && !id.contains('/')).then_some(AgentRoute::Logs(id));
    }
    (!rest.is_empty() && !rest.contains('/')).then_some(AgentRoute::Status(rest))
}

fn agent_request(
    responder: &mut HttpResponder,
    state: &Server,
    target: &str,
    route: AgentRoute<'_>,
) -> Result<(), String> {
    let values = match query(target) {
        Ok(values) => values,
        Err(_) => return reply(responder, 400, error("invalid_agent_query")),
    };
    match route {
        AgentRoute::List => {
            let allowed = ["state", "task_id"];
            if values.keys().any(|key| !allowed.contains(&key.as_str())) {
                return reply(responder, 400, error("invalid_agent_query"));
            }
            let agents = state.supervisor.registry.list(
                values.get("state").map(String::as_str),
                values.get("task_id").map(String::as_str),
            );
            reply(
                responder,
                200,
                Json::Object(vec![(
                    "agents".to_owned(),
                    Json::Array(agents.iter().map(AgentRecord::status_json).collect()),
                )]),
            )
        }
        AgentRoute::Status(id) => match state.supervisor.registry.get(id) {
            Some(agent) => reply(responder, 200, agent.status_json()),
            None => reply(responder, 404, error("unknown_agent")),
        },
        AgentRoute::Logs(id) => {
            let allowed = ["stream", "after", "tail", "follow"];
            if values.keys().any(|key| !allowed.contains(&key.as_str())) {
                return reply(responder, 400, error("invalid_log_query"));
            }
            let stream_name = values.get("stream").map(String::as_str).unwrap_or("both");
            if !matches!(stream_name, "stdout" | "stderr" | "both") {
                return reply(responder, 400, error("invalid_log_query"));
            }
            let after = match values
                .get("after")
                .map_or(Ok(0), |value| value.parse::<u64>())
            {
                Ok(value) => value,
                Err(_) => return reply(responder, 400, error("invalid_log_query")),
            };
            let tail = match values
                .get("tail")
                .map(|value| value.parse::<usize>())
                .transpose()
            {
                Ok(value) => value,
                Err(_) => return reply(responder, 400, error("invalid_log_query")),
            };
            let follow =
                match values
                    .get("follow")
                    .map_or(Ok(false), |value| match value.as_str() {
                        "0" => Ok(false),
                        "1" => Ok(true),
                        _ => Err(()),
                    }) {
                    Ok(value) => value,
                    Err(_) => return reply(responder, 400, error("invalid_log_query")),
                };
            let deadline = Instant::now() + Duration::from_secs(50);
            loop {
                let Some(logs) = state
                    .supervisor
                    .registry
                    .logs_json(id, stream_name, after, tail)
                else {
                    return reply(responder, 404, error("unknown_agent"));
                };
                let has_records = logs.object("records").is_some_and(
                    |records| matches!(records, Json::Array(records) if !records.is_empty()),
                );
                if !follow || has_records || Instant::now() >= deadline {
                    return reply(responder, 200, logs);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn post(responder: &mut HttpResponder, state: &Server, body: Vec<u8>) -> Result<(), String> {
    let body = match String::from_utf8(body) {
        Ok(body) => body,
        Err(_) => {
            reply(
                responder,
                400,
                error("body_must_be_an_object_with_nonempty_id"),
            )?;
            return Ok(());
        }
    };
    let payload = match parse_json(&body) {
        Ok(Json::Object(fields)) => Json::Object(fields),
        _ => {
            reply(
                responder,
                400,
                error("body_must_be_an_object_with_nonempty_id"),
            )?;
            return Ok(());
        }
    };
    if payload
        .object("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        reply(
            responder,
            400,
            error("body_must_be_an_object_with_nonempty_id"),
        )?;
        return Ok(());
    }
    match state.store.add(payload) {
        Ok((event, duplicate)) => reply(
            responder,
            if duplicate { 200 } else { 201 },
            Json::Object(vec![
                ("duplicate".to_owned(), Json::Bool(duplicate)),
                ("event".to_owned(), event.response_json()),
            ]),
        ),
        Err(_) => reply(responder, 500, error("could_not_persist_event")),
    }
}

fn exec_request(responder: &mut HttpResponder, body: Vec<u8>) -> Result<(), String> {
    let request = match parse_exec_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(responder, 200, denial),
    };
    // Prompts can be sensitive, so logs contain only this minimal routing data.
    eprintln!(
        "exec id={} bin={} subcommand={}",
        request.id,
        request.bin,
        request.args.first().map(String::as_str).unwrap_or("")
    );
    let policy = match require_gui_login_session().and_then(|_| exec::load_policy()) {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("exec policy read failed: {message}");
            return reply(responder, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy_path_or_denial(&policy, &request) {
        Ok(path) => path,
        Err(denial) => return reply(responder, 200, denial),
    };
    let result = exec::run(path, request);
    reply(responder, 200, result.to_json())
}

fn spawn_request(
    responder: &mut HttpResponder,
    state: &Server,
    body: Vec<u8>,
    policy: Result<exec::Policy, String>,
) -> Result<(), String> {
    if state.updater.is_draining() {
        return reply(responder, 503, error("updates_draining"));
    }
    let request = match parse_spawn_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(responder, 200, denial),
    };
    eprintln!(
        "spawn id={} bin={} subcommand={}",
        request.command.id,
        request.command.bin,
        request
            .command
            .args
            .first()
            .map(String::as_str)
            .unwrap_or("")
    );
    let execution_id = match request.execution_id.clone() {
        Some(execution_id) => execution_id,
        None => new_execution_id()?,
    };
    // This fact intentionally precedes policy evaluation: it records that the
    // authenticated relay received a request, not that it chose to launch it.
    if state
        .store
        .add(relay_event(
            "relay_request_started",
            &request.command.id,
            &execution_id,
            Json::Object(vec![]),
        ))
        .is_err()
    {
        return reply(responder, 500, error("could_not_persist_event"));
    }
    let policy = match policy {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("spawn policy read failed: {message}");
            return reply(responder, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy_path_or_denial(&policy, &request.command) {
        Ok(path) => path,
        Err(denial) => return reply(responder, 200, denial),
    };
    // The updater takes the same gate while setting `draining`, so a child
    // cannot appear between the drain check and its durable registry record.
    let _spawn_admission = match state.updater.spawn_admission() {
        Ok(Some(guard)) => guard,
        Ok(None) => return reply(responder, 503, error("updates_draining")),
        Err(message) => {
            eprintln!("spawn admission failed: {message}");
            return reply(responder, 500, error("could_not_admit_process"));
        }
    };
    if state
        .store
        .add(relay_event(
            "relay_accepted",
            &request.command.id,
            &execution_id,
            Json::Object(vec![]),
        ))
        .is_err()
    {
        return reply(responder, 500, error("could_not_persist_event"));
    }
    let task_id = request.command.id.clone();
    match spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        path,
        request.command,
        execution_id.clone(),
    ) {
        Ok(handle) => reply(
            responder,
            200,
            Json::Object(vec![
                ("id".to_owned(), Json::String(handle.id)),
                ("proc".to_owned(), Json::String(handle.handle)),
            ]),
        ),
        Err(_) => {
            if state
                .store
                .add(relay_event(
                    "process_failed",
                    &task_id,
                    &execution_id,
                    Json::Object(vec![(
                        "reason".to_owned(),
                        Json::String("spawn_failed".to_owned()),
                    )]),
                ))
                .is_err()
            {
                eprintln!("could not persist spawn failure audit event");
            }
            eprintln!("spawn failed");
            reply(responder, 500, error("could_not_spawn_process"))
        }
    }
}

struct SpawnedProc {
    id: String,
    handle: String,
}

fn spawn_proc(
    supervisor: &Supervisor,
    store: Arc<Store>,
    path: &str,
    request: exec::ExecRequest,
    execution_id: String,
) -> Result<SpawnedProc, String> {
    let stdout = Arc::new(Mutex::new(CappedOutput::default()));
    let stderr = Arc::new(Mutex::new(CappedOutput::default()));
    let mut child = Command::new(path)
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A dedicated process group makes kill requests cover the command's
        // descendants without involving the relay itself.
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    let child_stdout = child.stdout.take().expect("stdout was piped");
    let child_stderr = child.stderr.take().expect("stderr was piped");
    let process_group = child.id() as i32;
    let Some(process_identity) = process_identity(process_group) else {
        let _ = force_kill_process_group(process_group);
        return Err("could not record spawned process identity".to_owned());
    };
    let mut table = supervisor
        .procs
        .lock()
        .map_err(|_| "process table lock poisoned".to_owned())?;
    prune_procs(&mut table, Instant::now());
    let handle = unique_handle(&table)?;
    let id = request.id;
    let record = AgentRecord {
        id: handle.clone(),
        task_id: id.clone(),
        execution_id: execution_id.clone(),
        leader_pid: process_group,
        process_group,
        process_identity: Some(process_identity),
        started_at: unix_timestamp(),
        deadline_at: None,
        command: format!(
            "{} {}",
            request.bin,
            request.args.first().map(String::as_str).unwrap_or("")
        ),
        state: "running".to_owned(),
        exit_code: None,
        log_degraded: false,
        audit_degraded: false,
        redacted: false,
        stdout_next: 0,
        stderr_next: 0,
        stdout_dropped_before: 0,
        stderr_dropped_before: 0,
        log_next: 0,
        log_dropped_before: 0,
        first_output_at: None,
        first_output_stream: None,
        first_output_bytes: None,
    };
    // The registry transition commits before this spawn can be acknowledged.
    if let Err(error) = supervisor.registry.register(record) {
        let _ = kill_process_group(process_group);
        return Err(error);
    }
    if let Err(error) = store.add(relay_event(
        "process_spawned",
        &id,
        &execution_id,
        Json::Object(vec![("agent_id".to_owned(), Json::String(handle.clone()))]),
    )) {
        let _ = supervisor
            .registry
            .transition(&handle, "audit_failed", None);
        let _ = kill_process_group(process_group);
        return Err(error);
    }
    drain_to_capture(
        child_stdout,
        Arc::clone(&stdout),
        Arc::clone(&supervisor.registry),
        Arc::clone(&store),
        handle.clone(),
        "stdout",
    );
    drain_to_capture(
        child_stderr,
        Arc::clone(&stderr),
        Arc::clone(&supervisor.registry),
        store,
        handle.clone(),
        "stderr",
    );
    table.insert(
        handle.clone(),
        ProcEntry {
            child,
            process_group,
            id: id.clone(),
            bin: request.bin,
            subcommand: request.args.first().cloned().unwrap_or_default(),
            spawned_at: Instant::now(),
            finished_at: None,
            termination_requested_at: None,
            termination_escalated: false,
            leader_reaped: false,
            exit_code: None,
            stdout,
            stderr,
        },
    );
    prune_procs(&mut table, Instant::now());
    Ok(SpawnedProc { id, handle })
}

fn drain_to_capture(
    mut pipe: impl Read + Send + 'static,
    capture: Arc<Mutex<CappedOutput>>,
    registry: Arc<AgentRegistry>,
    store: Arc<Store>,
    agent_id: String,
    stream: &'static str,
) {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut output) = capture.lock() {
                        output.append(&chunk[..n]);
                    } else {
                        break;
                    }
                    // A spool failure never stops pipe draining; it is recorded
                    // as log_degraded and retried on the next chunk.
                    let _ = registry.append_log(&agent_id, stream, &chunk[..n]);
                    if let Ok(Some(agent)) = registry.record_first_output(
                        &agent_id,
                        &relay_timestamp(),
                        stream,
                        n as u64,
                    ) && persist_first_output(&store, &agent).is_err()
                    {
                        let _ = registry.mark_audit_degraded(&agent_id);
                    }
                }
            }
        }
        if let Ok(mut output) = capture.lock() {
            output.complete = true;
        }
    });
}

fn poll_proc(responder: &mut HttpResponder, state: &Server, handle: &str) -> Result<(), String> {
    let result = {
        let mut table = state
            .supervisor
            .procs
            .lock()
            .map_err(|_| "process table lock poisoned".to_owned())?;
        prune_procs(&mut table, Instant::now());
        let Some(entry) = table.get_mut(handle) else {
            return reply(responder, 404, error("unknown_proc"));
        };
        update_proc_status_with_handle(entry, handle, &state.supervisor.registry, &state.store);
        eprintln!(
            "poll id={} bin={} subcommand={}",
            entry.id, entry.bin, entry.subcommand
        );
        proc_json(entry)
    };
    reply(responder, 200, result)
}

fn kill_proc(responder: &mut HttpResponder, state: &Server, handle: &str) -> Result<(), String> {
    let result = {
        let mut table = state
            .supervisor
            .procs
            .lock()
            .map_err(|_| "process table lock poisoned".to_owned())?;
        prune_procs(&mut table, Instant::now());
        let Some(entry) = table.get_mut(handle) else {
            return reply(responder, 404, error("unknown_proc"));
        };
        update_proc_status_with_handle(entry, handle, &state.supervisor.registry, &state.store);
        let killed = if entry.finished_at.is_none() {
            kill_process_group(entry.process_group)
        } else {
            false
        };
        if killed {
            entry
                .termination_requested_at
                .get_or_insert_with(Instant::now);
            // Reap promptly when the signal is delivered before a subsequent
            // poll, but do not block an HTTP request waiting for cleanup.
            update_proc_status_with_handle(entry, handle, &state.supervisor.registry, &state.store);
        }
        eprintln!(
            "kill id={} bin={} subcommand={}",
            entry.id, entry.bin, entry.subcommand
        );
        Json::Object(vec![
            ("id".to_owned(), Json::String(entry.id.clone())),
            ("killed".to_owned(), Json::Bool(killed)),
        ])
    };
    reply(responder, 200, result)
}

fn output_is_complete(entry: &ProcEntry) -> bool {
    entry.stdout.lock().is_ok_and(|output| output.complete)
        && entry.stderr.lock().is_ok_and(|output| output.complete)
}

fn proc_json(entry: &ProcEntry) -> Json {
    let (stdout, stdout_truncated) = entry
        .stdout
        .lock()
        .map(|output| output.snapshot())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = entry
        .stderr
        .lock()
        .map(|output| output.snapshot())
        .unwrap_or_default();
    Json::Object(vec![
        ("id".to_owned(), Json::String(entry.id.clone())),
        (
            "running".to_owned(),
            Json::Bool(entry.finished_at.is_none()),
        ),
        (
            "exit_code".to_owned(),
            entry
                .exit_code
                .map_or(Json::Null, |code| Json::Number(code.to_string())),
        ),
        ("stdout".to_owned(), Json::String(stdout)),
        ("stderr".to_owned(), Json::String(stderr)),
        (
            "truncated".to_owned(),
            Json::Bool(stdout_truncated || stderr_truncated),
        ),
    ])
}

fn kill_process_group(process_group: i32) -> bool {
    // `process_group(0)` above creates a group whose id is the child PID.
    unsafe { libc::kill(-process_group, libc::SIGTERM) == 0 }
}

fn force_kill_process_group(process_group: i32) -> bool {
    // Review-loop cleanup is terminal: obsolete reviewers and owners must not
    // survive supersession or merge, including shells that ignore SIGTERM.
    let terminated = kill_process_group(process_group);
    let killed = unsafe { libc::kill(-process_group, libc::SIGKILL) == 0 };
    terminated || killed
}

fn process_group_running(process_group: i32) -> bool {
    unsafe { libc::kill(-process_group, 0) == 0 }
}

#[cfg(target_os = "macos")]
fn process_identity(pid: i32) -> Option<String> {
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as i32,
        )
    };
    (written as usize == size)
        .then(|| format!("macos:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(target_os = "linux")]
fn process_identity(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_command = stat.get(stat.rfind(')')? + 2..)?;
    let start_ticks = after_command.split_whitespace().nth(19)?;
    Some(format!("linux:{start_ticks}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_identity(_pid: i32) -> Option<String> {
    None
}

fn recovered_agent_identity_matches(agent: &AgentRecord) -> bool {
    process_group_running(agent.process_group)
        && agent
            .process_identity
            .as_deref()
            .zip(process_identity(agent.leader_pid).as_deref())
            .is_some_and(|(expected, current)| expected == current)
}

fn managed_agent_running(agent: &AgentRecord) -> bool {
    managed_agent_running_with(agent, recovered_agent_identity_matches)
}

fn managed_agent_running_with(
    agent: &AgentRecord,
    orphan_is_current: impl Fn(&AgentRecord) -> bool,
) -> bool {
    match agent.state.as_str() {
        "running" => true,
        "orphaned" => orphan_is_current(agent),
        _ => false,
    }
}

fn unique_handle(entries: &HashMap<String, ProcEntry>) -> Result<String, String> {
    loop {
        let handle = random_hex_128()?;
        if !entries.contains_key(&handle) {
            return Ok(handle);
        }
    }
}

fn prune_procs(entries: &mut HashMap<String, ProcEntry>, now: Instant) {
    // The independent reaper does durable transitions. This compatibility
    // pruning pass only bounds completed in-memory handles.
    entries.retain(|_, entry| {
        entry
            .finished_at
            .is_none_or(|finished| now.duration_since(finished) <= FINISHED_PROC_RETENTION)
    });
    let mut finished: Vec<_> = entries
        .iter()
        .filter_map(|(handle, entry)| entry.finished_at.map(|finished| (handle.clone(), finished)))
        .collect();
    finished.sort_by_key(|(handle, finished)| (*finished, entries[handle].spawned_at));
    let excess = finished.len().saturating_sub(MAX_FINISHED_PROCS);
    for (handle, _) in finished.into_iter().take(excess) {
        entries.remove(&handle);
    }
}

fn unix_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn relay_timestamp() -> String {
    relay_core::rfc3339_timestamp()
}

fn relay_clock() -> String {
    static CLOCK: OnceLock<String> = OnceLock::new();
    CLOCK
        .get_or_init(|| {
            let host = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "unknown-host".to_owned());
            // Linux exposes a real boot identifier; macOS has no equivalent stable
            // portable file in this no-dependency relay, so make that absence explicit.
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "boot-unknown".to_owned());
            // The instance suffix prevents a relay restart from being treated
            // as one continuous clock when a host boot identifier is absent.
            format!(
                "mac-relay:{host}:{boot}:instance-{}",
                random_hex_128().unwrap_or_else(|_| format!("pid-{}", std::process::id()))
            )
        })
        .clone()
}

fn random_hex_128() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| format!("could not generate identifier: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn new_execution_id() -> Result<String, String> {
    Ok(format!("relay-{}", random_hex_128()?))
}

fn relay_event(kind: &str, task_id: &str, execution_id: &str, payload: Json) -> Json {
    relay_event_at(kind, task_id, execution_id, relay_timestamp(), payload)
}

fn relay_event_at(
    kind: &str,
    task_id: &str,
    execution_id: &str,
    occurred_at: String,
    payload: Json,
) -> Json {
    Json::Object(vec![
        (
            "id".to_owned(),
            Json::String(format!("relay:{execution_id}:{kind}")),
        ),
        ("schema_version".to_owned(), Json::number(1)),
        ("task_id".to_owned(), Json::String(task_id.to_owned())),
        (
            "execution_id".to_owned(),
            Json::String(execution_id.to_owned()),
        ),
        ("kind".to_owned(), Json::String(kind.to_owned())),
        ("source".to_owned(), Json::String("mac-relay".to_owned())),
        ("occurred_at".to_owned(), Json::String(occurred_at)),
        ("clock".to_owned(), Json::String(relay_clock())),
        ("payload".to_owned(), payload),
    ])
}

fn persist_first_output(store: &Store, agent: &AgentRecord) -> Result<(), String> {
    let Some(occurred_at) = agent.first_output_at.clone() else {
        return Ok(());
    };
    store
        .add(relay_event_at(
            "first_output",
            &agent.task_id,
            &agent.execution_id,
            occurred_at,
            Json::Object(vec![
                ("agent_id".to_owned(), Json::String(agent.id.clone())),
                (
                    "stream".to_owned(),
                    Json::String(
                        agent
                            .first_output_stream
                            .clone()
                            .unwrap_or_else(|| "unknown".to_owned()),
                    ),
                ),
                (
                    "bytes".to_owned(),
                    Json::number(agent.first_output_bytes.unwrap_or_default()),
                ),
            ]),
        ))
        .map(|_| ())
}

fn replay_recovered_lifecycle(store: &Store, agent: &AgentRecord) -> Result<(), String> {
    store.add(relay_event(
        "process_spawned",
        &agent.task_id,
        &agent.execution_id,
        Json::Object(vec![
            ("agent_id".to_owned(), Json::String(agent.id.clone())),
            ("recovered".to_owned(), Json::Bool(true)),
        ]),
    ))?;
    persist_first_output(store, agent)
}

fn start_reaper(state: Arc<Server>) {
    thread::spawn(move || {
        loop {
            if let Ok(mut entries) = state.supervisor.procs.lock() {
                for (handle, entry) in entries.iter_mut() {
                    update_proc_status_with_handle(
                        entry,
                        handle,
                        &state.supervisor.registry,
                        &state.store,
                    );
                }
                prune_procs(&mut entries, Instant::now());
            }
            let _ = state
                .supervisor
                .registry
                .prune(unix_timestamp().parse().unwrap_or_default());
            thread::sleep(Duration::from_millis(200));
        }
    });
}

fn update_proc_status_with_handle(
    entry: &mut ProcEntry,
    handle: &str,
    registry: &AgentRegistry,
    store: &Store,
) {
    if entry.finished_at.is_some() {
        return;
    }
    if entry
        .termination_requested_at
        .is_some_and(|requested| requested.elapsed() >= Duration::from_millis(500))
        && !entry.termination_escalated
    {
        // Shells waiting on descendants do not consistently exit after a
        // group-wide SIGTERM on macOS. Escalate the whole group after a short
        // grace period so explicit kill requests always converge.
        unsafe {
            libc::kill(-entry.process_group, libc::SIGKILL);
        }
        entry.termination_escalated = true;
    }
    if !entry.leader_reaped {
        match entry.child.try_wait() {
            Ok(Some(status)) => {
                entry.exit_code = status.code();
                entry.leader_reaped = true;
            }
            Ok(None) => return,
            Err(_) => return,
        }
    }
    // A fully killed group can remain visible to kill(2) while an orphaned
    // descendant is still a zombie. Once SIGKILL was sent, a reaped leader
    // and closed output pipes prove there are no live managed writers left.
    if (!process_group_running(entry.process_group) || entry.termination_escalated)
        && output_is_complete(entry)
    {
        let state = match entry.exit_code {
            Some(0) => "succeeded",
            Some(_) => "failed",
            None => "unexpected_exit",
        };
        if let Some(agent) = registry.get(handle) {
            if persist_first_output(store, &agent).is_err() {
                let _ = registry.mark_audit_degraded(handle);
                return;
            }
            let kind = if state == "succeeded" {
                "process_completed"
            } else {
                "process_failed"
            };
            let mut payload = vec![
                ("agent_id".to_owned(), Json::String(agent.id.clone())),
                ("state".to_owned(), Json::String(state.to_owned())),
            ];
            if let Some(exit_code) = entry.exit_code {
                payload.push(("exit_code".to_owned(), Json::Number(exit_code.to_string())));
            }
            payload.push(("stdout_bytes".to_owned(), Json::number(agent.stdout_next)));
            payload.push(("stderr_bytes".to_owned(), Json::number(agent.stderr_next)));
            if store
                .add(relay_event(
                    kind,
                    &agent.task_id,
                    &agent.execution_id,
                    Json::Object(payload),
                ))
                .is_err()
            {
                let _ = registry.mark_audit_degraded(handle);
                return;
            }
            if registry.transition(handle, state, entry.exit_code).is_ok() {
                entry.finished_at = Some(Instant::now());
            } else {
                let _ = registry.mark_audit_degraded(handle);
            }
        }
    }
}

fn proc_route(path: &str) -> Option<ProcRoute<'_>> {
    let path = path.strip_prefix("/v1/proc/")?;
    if let Some(handle) = path.strip_suffix("/kill") {
        (!handle.is_empty() && !handle.contains('/')).then_some(ProcRoute::Kill(handle))
    } else {
        (!path.is_empty() && !path.contains('/')).then_some(ProcRoute::Poll(path))
    }
}

fn parse_exec_request(body: &[u8]) -> Result<exec::ExecRequest, Json> {
    let parsed = std::str::from_utf8(body)
        .ok()
        .and_then(|text| parse_json(text).ok());
    let Some(parsed) = parsed else {
        return Err(denial_json(""));
    };
    let denied_id = exec::request_id(&parsed);
    exec::parse_request(&parsed).map_err(|_| denial_json(&denied_id))
}

fn parse_spawn_request(body: &[u8]) -> Result<SpawnRequest, Json> {
    let parsed = std::str::from_utf8(body)
        .ok()
        .and_then(|text| parse_json(text).ok());
    let Some(Json::Object(fields)) = parsed else {
        return Err(denial_json(""));
    };
    let denied_id = Json::Object(fields.clone());
    let denied_id = exec::request_id(&denied_id);
    if fields
        .iter()
        .any(|(name, _)| !matches!(name.as_str(), "id" | "bin" | "args" | "execution_id"))
        || fields
            .iter()
            .enumerate()
            .any(|(index, (name, _))| fields[..index].iter().any(|(previous, _)| previous == name))
    {
        return Err(denial_json(&denied_id));
    }
    let execution_id = match fields
        .iter()
        .find(|(name, _)| name == "execution_id")
        .map(|(_, value)| value)
    {
        None => None,
        Some(value) => value
            .as_str()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
            .map(str::to_owned)
            .ok_or_else(|| denial_json(&denied_id))
            .map(Some)?,
    };
    let command = exec::parse_request(&Json::Object(
        fields
            .into_iter()
            .filter(|(name, _)| name != "execution_id")
            .collect(),
    ))
    .map_err(|_| denial_json(&denied_id))?;
    Ok(SpawnRequest {
        command,
        execution_id,
    })
}

fn denied(responder: &mut HttpResponder, id: &str) -> Result<(), String> {
    let (status, body) = denial_response(id);
    reply(responder, status, body)
}

fn policy_path_or_denial<'a>(
    policy: &'a exec::Policy,
    request: &exec::ExecRequest,
) -> Result<&'a str, Json> {
    policy
        .allowed_path(&request.bin, &request.args)
        .ok_or_else(|| denial_json(&request.id))
}

fn denial_response(id: &str) -> (u16, Json) {
    (200, denial_json(id))
}

fn denial_json(id: &str) -> Json {
    Json::Object(vec![
        ("id".to_owned(), Json::String(id.to_owned())),
        ("error".to_owned(), Json::String("denied".to_owned())),
    ])
}

fn get(responder: &mut HttpResponder, state: &Server, target: &str) -> Result<(), String> {
    let (after, timeout, epoch) = match get_query(target) {
        Ok(query) => query,
        Err(message) => {
            reply(responder, 400, error(&message))?;
            return Ok(());
        }
    };
    let result = state
        .store
        .read(after, &epoch, Duration::from_secs(timeout));
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            reply(responder, 500, error("could_not_read_events"))?;
            return Ok(());
        }
    };
    reply(responder, 200, read_json(result))
}

fn get_query(target: &str) -> Result<(u64, u64, String), String> {
    let query = query(target)?;
    let after = query.get("after").map_or(Ok(0), |value| {
        value
            .parse::<u64>()
            .map_err(|_| "after must be a non-negative integer".to_owned())
    })?;
    let timeout = query.get("timeout").map_or(Ok(50), |value| {
        value
            .parse::<u64>()
            .map_err(|_| "timeout must be an integer".to_owned())
    })?;
    if timeout > 55 {
        return Err("timeout must be between 0 and 55".to_owned());
    }
    Ok((
        after,
        timeout,
        query.get("epoch").cloned().unwrap_or_default(),
    ))
}

fn read_json(result: ReadResult) -> Json {
    Json::Object(vec![
        ("epoch".to_owned(), Json::String(result.epoch)),
        ("reset".to_owned(), Json::Bool(result.reset)),
        ("lost".to_owned(), Json::Bool(result.lost)),
        (
            "events".to_owned(),
            Json::Array(
                result
                    .events
                    .iter()
                    .map(|event| event.response_json())
                    .collect(),
            ),
        ),
        ("next".to_owned(), Json::number(result.next)),
    ])
}

/// Owns the tiny_http request while the handler runs. `respond` takes the
/// request out, so a handler can only ever send one response: a second reply
/// is a loud internal error, never a second write on the wire.
struct HttpResponder {
    request: Option<tiny_http::Request>,
}

struct Incoming {
    parsed: Result<Request, ReadRequestError>,
    responder: HttpResponder,
}

fn receive(mut http_request: tiny_http::Request) -> Incoming {
    let parsed = parse_request(&mut http_request);
    Incoming {
        parsed,
        responder: HttpResponder {
            request: Some(http_request),
        },
    }
}

fn parse_request(http: &mut tiny_http::Request) -> Result<Request, ReadRequestError> {
    let method = http.method().as_str().to_owned();
    let target = http.url().to_owned();
    let mut headers = HashMap::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    // DoS hardening (#36): tiny_http parses headers with no caps of its own,
    // so enforce the merged header-block budget (request line included) and
    // header-count cap here. Violations fail closed with 431, as before.
    let mut header_block_bytes = method.len() + target.len();
    let mut header_count = 0usize;
    for header in http.headers() {
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(ReadRequestError::HeadersTooLarge);
        }
        header_block_bytes += header.field.as_str().as_str().len() + header.value.as_str().len();
        if header_block_bytes > MAX_HEADER_BLOCK_BYTES {
            return Err(ReadRequestError::HeadersTooLarge);
        }
        if header.field.equiv("content-length") {
            let length = header
                .value
                .as_str()
                .trim()
                .parse::<usize>()
                .map_err(|_| ReadRequestError::Message("invalid content length".to_owned()))?;
            if content_length.is_some_and(|previous| previous != length) {
                // Conflicting duplicate Content-Length values are a classic
                // request-smuggling vector; fail closed instead of picking one.
                return Err(ReadRequestError::Message(
                    "invalid content length".to_owned(),
                ));
            }
            content_length = Some(length);
        } else if header.field.equiv("transfer-encoding") {
            chunked = true;
        }
        headers.insert(
            header.field.as_str().as_str().to_ascii_lowercase(),
            header.value.as_str().trim().to_owned(),
        );
    }
    // POST /v1/exec and /v1/spawn evaluate the execution policy; any body
    // framing anomaly there stays an opaque denial, never a descriptive error.
    let exec_or_spawn = method == "POST"
        && matches!(
            target.split('?').next(),
            Some("/v1/exec") | Some("/v1/spawn")
        );
    let framing_error = |message: &str| {
        if exec_or_spawn {
            ReadRequestError::ExecutionDenied
        } else {
            ReadRequestError::Message(message.to_owned())
        }
    };
    if chunked {
        // The old server ignored Transfer-Encoding and therefore never
        // understood chunked bodies; reject the framing outright instead of
        // reinterpreting it behind the caller's back.
        return Err(framing_error("transfer encoding not supported"));
    }
    let length = content_length.unwrap_or(0);
    if length > MAX_BODY {
        // Drain the framed body with a fixed-size buffer so the connection
        // stays in sync for keep-alive. The denial itself never allocates
        // for the body, preserving the old "before body allocation" property.
        drain_body(http.as_reader(), length);
        return Err(framing_error("request body too large"));
    }
    let mut body = vec![0u8; length];
    if http.as_reader().read_exact(&mut body).is_err() {
        return Err(framing_error("short request body"));
    }
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

fn drain_body(reader: &mut dyn Read, mut remaining: usize) {
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        match reader.read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(n) => remaining -= n,
            Err(_) => break,
        }
    }
}

fn query(target: &str) -> Result<HashMap<String, String>, String> {
    let Some((_, raw)) = target.split_once('?') else {
        return Ok(HashMap::new());
    };
    let pairs: Vec<_> = raw
        .split('&')
        .filter(|value| !value.is_empty())
        .map(|item| {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            Ok::<_, String>((percent_decode(key)?, percent_decode(value)?))
        })
        .collect::<Result<_, _>>()?;
    let mut values = HashMap::new();
    for (key, value) in pairs {
        if values.insert(key, value).is_some() {
            return Err("duplicate query parameter".to_owned());
        }
    }
    Ok(values)
}
fn percent_decode(input: &str) -> Result<String, String> {
    // Query strings use form-encoding, where a literal '+' means space.
    // Translate '+' first so an encoded "%2B" still decodes to '+'.
    let translated = input.replace('+', " ");
    // The percent-encoding crate passes malformed '%' sequences through
    // untouched instead of failing, so validate escapes up front. This keeps
    // the old strict contract: query values feed agent/log lookups and event
    // reads on the auth boundary, and bad input must be a 400, never silently
    // accepted.
    let mut bytes = translated.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let valid = bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                && bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit());
            if !valid {
                return Err("invalid URL encoding".to_owned());
            }
        }
    }
    percent_encoding::percent_decode_str(&translated)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| "invalid URL encoding".to_owned())
}
fn authorized(supplied: &str, secret: &str) -> bool {
    let expected = format!("Bearer {secret}");
    let mut difference = expected.len() ^ supplied.len();
    for (index, left) in expected.bytes().enumerate() {
        difference |= (left ^ supplied.as_bytes().get(index).copied().unwrap_or(0)) as usize;
    }
    difference == 0
}
fn error(message: &str) -> Json {
    Json::Object(vec![("error".to_owned(), Json::String(message.to_owned()))])
}
fn reply(responder: &mut HttpResponder, code: u16, value: Json) -> Result<(), String> {
    responder.respond(code, value)
}

impl HttpResponder {
    fn respond(&mut self, code: u16, value: Json) -> Result<(), String> {
        let request = self
            .request
            .take()
            .ok_or_else(|| "response already sent".to_owned())?;
        let body = value.to_json();
        // tiny_http's default reason phrases match the old server's table
        // exactly for every status code this relay emits.
        let response = tiny_http::Response::from_data(body.as_bytes())
            .with_status_code(tiny_http::StatusCode::from(code))
            .with_header(response_header("Content-Type", "application/json")?)
            .with_header(response_header("Content-Length", &body.len().to_string())?)
            .with_header(response_header("Cache-Control", "no-store")?)
            .with_header(response_header("Connection", "close")?);
        request.respond(response).map_err(|error| error.to_string())
    }
}

fn response_header(name: &str, value: &str) -> Result<tiny_http::Header, String> {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
        .map_err(|_| "could not build response header".to_owned())
}

#[cfg(test)]
pub(crate) fn test_updater() -> Arc<update::Manager> {
    Arc::new(update::Manager::new(update::Config {
        directory: std::env::temp_dir().join("zigzag-test-updates"),
        interval: Duration::ZERO,
        policy: update::Policy::Enabled,
        ready_file: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream;

    #[test]
    fn malformed_get_query_is_a_bad_request() {
        assert_eq!(
            get_query("/v1/events?after=not-a-number").unwrap_err(),
            "after must be a non-negative integer"
        );
        assert_eq!(
            get_query("/v1/events?epoch=%ZZ").unwrap_err(),
            "invalid URL encoding"
        );
    }

    #[test]
    fn percent_decode_handles_form_encoding_edge_cases() {
        for (input, expected) in [
            ("", ""),
            ("abc", "abc"),
            ("hello+world", "hello world"),
            ("+", " "),
            ("%2B", "+"),
            ("%2b", "+"),
            ("%41%42%63", "ABc"),
            ("%E2%82%AC", "\u{20ac}"),
            ("a%20b%09c", "a b\tc"),
            ("%25", "%"),
            ("100%25", "100%"),
        ] {
            assert_eq!(percent_decode(input).unwrap(), expected, "{input:?}");
        }
        // Malformed escapes and invalid UTF-8 stay hard errors: query values
        // reach authenticated lookups, so bad input must never decode to
        // something surprising.
        for input in [
            "%", "%2", "a%", "%zz", "%2G", "G%2", "%%41", "%FF", "a%FFb", "%E2%82", "%C3%28",
        ] {
            assert_eq!(
                percent_decode(input).unwrap_err(),
                "invalid URL encoding",
                "{input:?}"
            );
        }
    }

    #[test]
    fn query_decoding_matches_url_crate_form_decoding() {
        // Cross-check our strict decoder against the `url` crate's form
        // decoder on well-formed inputs; ours additionally rejects malformed
        // escapes instead of passing them through.
        for raw in [
            "a=1&b=2",
            "a=hello+world",
            "a=%2B%25%3D%26",
            "a=%E2%82%AC",
            "empty=&flag",
            "k=%20+%09",
            "a=1&b=%41%42",
        ] {
            let expected: HashMap<String, String> = url::form_urlencoded::parse(raw.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            assert_eq!(
                query(&format!("/v1/events?{raw}")).unwrap(),
                expected,
                "{raw:?}"
            );
        }
        assert_eq!(
            query("/v1/events?a=%ZZ").unwrap_err(),
            "invalid URL encoding"
        );
        assert_eq!(
            query("/v1/events?a=1&a=2").unwrap_err(),
            "duplicate query parameter"
        );
    }

    #[test]
    fn review_gate_query_requires_one_repository_and_positive_pr() {
        assert_eq!(
            review_gate_parameters(
                "/v1/review-gate?repository=ShukantPal%2Fzigzag&pull_request=22"
            ),
            Ok(("ShukantPal/zigzag".to_owned(), 22))
        );
        for target in [
            "/v1/review-gate",
            "/v1/review-gate?repository=ShukantPal%2Fzigzag&pull_request=0",
            "/v1/review-gate?repository=&pull_request=22",
            "/v1/review-gate?repository=ShukantPal%2Fzigzag&pull_request=22&extra=1",
            "/v1/review-gate?repository=one%2Frepo&repository=two%2Frepo&pull_request=22",
            "/v1/review-gate?repository=one%2Frepo&pull_request=22&pull_request=23",
            "/v1/review-gate?repository=%ZZ&pull_request=22",
        ] {
            assert_eq!(review_gate_parameters(target), Err(()));
        }
    }

    #[test]
    fn authenticated_review_gate_route_forwards_mode_and_state_path() {
        let (state, state_path) = test_server();
        let policy =
            exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#)
                .unwrap();
        let head = "a".repeat(40);
        let base = "b".repeat(40);
        let key = format!("owner/repo#22@{head}");
        let durable_state = serde_json::json!({
            "schema_version": 1,
            "rounds": {
                (key): {
                    "repository": "owner/repo",
                    "pull_request": 22,
                    "head": head,
                    "base": base,
                    "generation": 1,
                    "verification": false,
                    "phase": "reviewing",
                    "reviewers": {},
                    "verdicts": {},
                    "excluded_comment_ids": [],
                    "pending_comment_deletions": [],
                    "owner": null,
                    "owner_agent_id": null,
                    "gate_reasons": [],
                }
            }
        });
        std::fs::write(
            &state.review_state_file,
            serde_json::to_vec(&durable_state).unwrap(),
        )
        .unwrap();
        let expected_state_path = state.review_state_file.clone();

        let response = request_once_with_gate(
            Arc::clone(&state),
            &policy,
            "GET",
            "/v1/review-gate?repository=owner%2Frepo&pull_request=22",
            "",
            |repository, pull_request, review_state_path, shadow, _config| {
                assert_eq!(repository, "owner/repo");
                assert_eq!(pull_request, 22);
                assert_eq!(review_state_path, expected_state_path);
                assert!(!shadow);
                let persisted: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(review_state_path).unwrap()).unwrap();
                assert!(persisted["rounds"].is_object());
                Ok(serde_json::json!({
                    "pass": true,
                    "head": "a".repeat(40),
                }))
            },
        );

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let report = response_json(response);
        assert_eq!(report.object("pass"), Some(&Json::Bool(true)));
        assert_eq!(report.object("head"), Some(&Json::String("a".repeat(40))));

        let unauthorized = request_once_with_gate_token(
            Arc::clone(&state),
            &policy,
            "GET",
            "/v1/review-gate?repository=owner%2Frepo&pull_request=22",
            "",
            "wrong-token",
            |_, _, _, _, _| panic!("unauthorized gate request reached the backend"),
        );
        assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized"));

        drop(state);
        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(expected_state_path);
    }

    #[test]
    fn bearer_comparison_requires_the_full_token() {
        assert!(authorized(
            &format!("Bearer {}", "x".repeat(32)),
            &"x".repeat(32)
        ));
        assert!(!authorized("Bearer x", &"x".repeat(32)));
        assert!(!authorized(
            &format!("Bearer {}suffix", "x".repeat(32)),
            &"x".repeat(32)
        ));
    }

    #[test]
    fn explicit_bind_address_must_be_tailscale_ipv4() {
        assert!(is_tailscale_ipv4("100.101.237.83".parse().unwrap()));
        assert!(!is_tailscale_ipv4("0.0.0.0".parse().unwrap()));
        assert!(!is_tailscale_ipv4("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn config_parses_only_its_documented_forms() {
        let get = ConfigArgs::try_parse_from(["zigzag", "get-allowlist"]).unwrap();
        assert!(matches!(
            config_command(get).unwrap(),
            ConfigCommand::GetAllowlist
        ));
        let set = ConfigArgs::try_parse_from([
            "zigzag",
            "set-allowlist",
            "--file",
            "/secure/policy.json",
        ])
        .unwrap();
        match config_command(set).unwrap() {
            ConfigCommand::SetAllowlist { file } => assert_eq!(file, "/secure/policy.json"),
            ConfigCommand::GetAllowlist => panic!("parsed the wrong subcommand"),
        }
        // Bare `config`, unknown subcommands, missing/empty --file, and stray
        // flags are all usage errors, exactly like the old hand-rolled parser.
        for invalid in [
            vec!["zigzag"],
            vec!["zigzag", "get-allowlist", "--file", "x"],
            vec!["zigzag", "set-allowlist"],
            vec!["zigzag", "set-allowlist", "--other", "x"],
            vec!["zigzag", "bogus"],
        ] {
            let rejected = match ConfigArgs::try_parse_from(invalid.clone()) {
                Err(_) => true,
                Ok(parsed) => config_command(parsed).is_err(),
            };
            assert!(rejected, "{invalid:?}");
        }
        let empty = ConfigArgs::try_parse_from(["zigzag", "set-allowlist", "--file", ""]).unwrap();
        assert_eq!(config_command(empty).unwrap_err(), CONFIG_USAGE);
        for flag in ["--help", "-h"] {
            let help = ConfigArgs::try_parse_from(["zigzag", flag]).unwrap();
            assert_eq!(config_command(help).unwrap_err(), CONFIG_USAGE);
        }
    }

    #[test]
    fn serve_args_keep_flags_defaults_and_validation() {
        let base = || {
            vec![
                "--secret-file".to_owned(),
                "/token".to_owned(),
                "--state-file".to_owned(),
                "/state".to_owned(),
            ]
        };
        let config = server_config(base()).unwrap();
        assert_eq!(config.secret_file, PathBuf::from("/token"));
        assert_eq!(config.state_file, PathBuf::from("/state"));
        assert_eq!(config.port, 8765);
        assert_eq!(config.max_events, 1000);
        assert!(config.github_watch_repos.is_empty());
        assert_eq!(config.github_watch_interval, Duration::from_secs(30));
        assert_eq!(config.update_interval, Duration::from_secs(3600));
        assert!(matches!(config.update_policy, update::Policy::Enabled));
        assert_eq!(
            config.agent_registry_file,
            PathBuf::from("/state").with_extension("agents.json")
        );
        assert_eq!(config.update_directory, PathBuf::from("/").join("relay"));

        // Every flag still validates with its historical error message.
        // clap wraps value-parser failures, so those assert `contains`;
        // post-parse domain checks keep their exact strings.
        for (arguments, message) in [
            (
                vec!["--port".to_owned(), "not-a-port".to_owned()],
                "--port must be a valid u16",
            ),
            (
                vec!["--tailscale-ip".to_owned(), "not-an-ip".to_owned()],
                "--tailscale-ip must be an IP address",
            ),
            (
                vec!["--max-events".to_owned(), "many".to_owned()],
                "--max-events must be a positive integer",
            ),
            (
                vec!["--watch-repo".to_owned(), "not-a-repo".to_owned()],
                "--watch-repo must be an OWNER/REPO GitHub name",
            ),
            (
                vec!["--watch-interval".to_owned(), "soon".to_owned()],
                "--watch-interval must be an integer",
            ),
            (
                vec!["--update-interval".to_owned(), "soon".to_owned()],
                "--update-interval must be an integer",
            ),
            (
                vec!["--update-policy".to_owned(), "sometimes".to_owned()],
                "update policy must be enabled, paused, or pin:<version>",
            ),
        ] {
            let mut full = base();
            full.extend(arguments);
            let error = server_config(full).unwrap_err();
            assert!(error.contains(message), "{error:?}");
        }
        for (arguments, message) in [
            (
                vec!["--tailscale-ip".to_owned(), "8.8.8.8".to_owned()],
                "--tailscale-ip must be a Tailscale IPv4 address",
            ),
            (
                vec!["--max-events".to_owned(), "0".to_owned()],
                "--max-events must be greater than zero",
            ),
            (
                vec!["--watch-interval".to_owned(), "29".to_owned()],
                "--watch-interval must be between 30 and 3600 seconds",
            ),
            (
                vec!["--update-interval".to_owned(), "86401".to_owned()],
                "--update-interval must be at most 86400 seconds",
            ),
        ] {
            let mut full = base();
            full.extend(arguments);
            assert_eq!(server_config(full).unwrap_err(), message);
        }

        // Accepted values flow through to the config, with --watch-repo deduped.
        let mut arguments = base();
        arguments.extend([
            "--port".to_owned(),
            "9000".to_owned(),
            "--tailscale-ip".to_owned(),
            "100.101.237.83".to_owned(),
            "--watch-repo".to_owned(),
            "leveled-inc/leveled".to_owned(),
            "--watch-repo".to_owned(),
            "leveled-inc/leveled".to_owned(),
            "--update-policy".to_owned(),
            "pin:1.2.3".to_owned(),
        ]);
        let config = server_config(arguments).unwrap();
        assert_eq!(config.port, 9000);
        assert_eq!(config.tailscale_ip, Some("100.101.237.83".parse().unwrap()));
        assert_eq!(config.github_watch_repos, ["leveled-inc/leveled"]);
        assert!(matches!(
            config.update_policy,
            update::Policy::Pin(ref version) if version == "1.2.3"
        ));

        // Missing secrets, unknown flags, and --help keep their old shapes.
        assert_eq!(
            server_config(vec!["--state-file".to_owned(), "/s".to_owned()]).unwrap_err(),
            "--secret-file or ZIGZAG_SECRET_FILE is required"
        );
        assert_eq!(
            server_config(vec!["--secret-file".to_owned(), "/t".to_owned()]).unwrap_err(),
            "--state-file or ZIGZAG_STATE_FILE is required"
        );
        assert!(server_config(vec!["--bogus".to_owned()]).is_err());
        for flag in ["--help", "-h"] {
            assert_eq!(
                server_config(vec![flag.to_owned()]).unwrap_err(),
                SERVER_USAGE
            );
        }
    }

    #[test]
    fn serve_args_fall_back_to_environment() {
        // SAFETY: no other test reads these variables without also passing
        // explicit flags, which take precedence over the environment.
        unsafe {
            std::env::set_var("ZIGZAG_SECRET_FILE", "/env-token");
            std::env::set_var("ZIGZAG_STATE_FILE", "/env-state");
            std::env::set_var("ZIGZAG_UPDATE_DIR", "/env-updates");
        }
        let config = server_config(Vec::new()).unwrap();
        assert_eq!(config.secret_file, PathBuf::from("/env-token"));
        assert_eq!(config.state_file, PathBuf::from("/env-state"));
        assert_eq!(config.update_directory, PathBuf::from("/env-updates"));
        // Flags still win over the environment.
        let config =
            server_config(vec!["--secret-file".to_owned(), "/flag-token".to_owned()]).unwrap();
        assert_eq!(config.secret_file, PathBuf::from("/flag-token"));
        unsafe {
            std::env::remove_var("ZIGZAG_SECRET_FILE");
            std::env::remove_var("ZIGZAG_STATE_FILE");
            std::env::remove_var("ZIGZAG_UPDATE_DIR");
        }
    }

    #[test]
    fn timeline_args_parse_positional_task_id_and_state_file() {
        let args =
            TimelineArgs::try_parse_from(["zigzag", "task-1", "--state-file", "/s"]).unwrap();
        assert!(!args.help);
        assert_eq!(args.task_id.as_deref(), Some("task-1"));
        assert_eq!(args.state_file, Some(PathBuf::from("/s")));
        // --state-file may precede the task id, like the old parser allowed.
        let args =
            TimelineArgs::try_parse_from(["zigzag", "--state-file", "/s", "task-1"]).unwrap();
        assert_eq!(args.task_id.as_deref(), Some("task-1"));
        // --help keeps the historical usage error.
        for flag in ["--help", "-h"] {
            let args = TimelineArgs::try_parse_from(["zigzag", flag]).unwrap();
            assert!(args.help);
        }
        // Unknown flags, extra positionals, and valueless --state-file fail.
        assert!(TimelineArgs::try_parse_from(["zigzag", "a", "b"]).is_err());
        assert!(TimelineArgs::try_parse_from(["zigzag", "--bogus"]).is_err());
        assert!(TimelineArgs::try_parse_from(["zigzag", "--state-file"]).is_err());
    }

    #[test]
    fn gui_session_check_rejects_remote_or_non_graphical_sessions() {
        assert!(is_local_gui_session(0, SESSION_HAS_GRAPHIC_ACCESS));
        assert!(!is_local_gui_session(
            0,
            SESSION_HAS_GRAPHIC_ACCESS | SESSION_IS_REMOTE
        ));
        assert!(!is_local_gui_session(0, 0));
        assert!(!is_local_gui_session(-1, SESSION_HAS_GRAPHIC_ACCESS));
    }

    #[test]
    fn exec_denials_are_opaque_and_never_include_policy_data() {
        let expected = r#"{"id":"request-1","error":"denied"}"#;
        let invalid_json =
            match parse_exec_request(br#"{"id":"request-1","bin":"jules","args":["new"]"#) {
                Err(denial) => denial,
                Ok(_) => panic!("malformed request was accepted"),
            };
        assert_eq!(invalid_json.to_json(), r#"{"id":"","error":"denied"}"#);
        let malformed_schema =
            match parse_exec_request(br#"{"id":"request-1","bin":"jules","args":"new"}"#) {
                Err(denial) => denial,
                Ok(_) => panic!("malformed request was accepted"),
            };
        assert_eq!(malformed_schema.to_json(), expected);

        let policy = exec::Policy::parse(
            r#"{"bins":{"jules":{"path":"/private/configured-binary","commands":[["new"]]}}}"#,
        )
        .unwrap();
        for body in [
            br#"{"id":"request-1","bin":"unknown","args":["new"]}"#.as_slice(),
            br#"{"id":"request-1","bin":"jules","args":["login"]}"#.as_slice(),
        ] {
            let request = parse_exec_request(body).unwrap();
            let denial = policy_path_or_denial(&policy, &request)
                .unwrap_err()
                .to_json();
            assert_eq!(denial, expected);
            assert!(!denial.contains("configured-binary"));
            let (status, response) = denial_response(&request.id);
            assert_eq!(status, 200);
            assert_eq!(response.to_json(), expected);
        }
    }

    #[test]
    fn oversized_exec_request_is_an_opaque_denial_before_body_allocation() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_BODY + 1
            )
            .unwrap();
            // No body follows; close the write side so the server's drain
            // observes EOF instead of blocking.
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let mut http_request = server.recv().expect("test server received the request");
        // The size gate fires before any body-sized allocation is made.
        assert!(matches!(
            parse_request(&mut http_request),
            Err(ReadRequestError::ExecutionDenied)
        ));
        let mut responder = HttpResponder {
            request: Some(http_request),
        };
        denied(&mut responder, "").unwrap();
        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with(r#"{"id":"","error":"denied"}"#));
    }

    #[test]
    fn truncated_small_body_resets_the_connection() {
        // tiny_http eagerly buffers bodies up to 1024 bytes while parsing,
        // so a truncated small body aborts the connection before any request
        // reaches the handler. Fail-closed: no response, no execution.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: 32\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let arrived = server
            .recv_timeout(Duration::from_secs(5))
            .expect("recv did not error");
        assert!(
            arrived.is_none(),
            "truncated body must not reach the handler"
        );
        assert!(
            client.join().unwrap().is_empty(),
            "no response on truncated body"
        );
    }

    #[test]
    fn truncated_large_exec_body_is_an_opaque_denial() {
        // Bodies larger than tiny_http's 1024-byte eager buffer stay lazy,
        // so a truncated read surfaces in the handler as an opaque denial,
        // exactly like the old server.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let mut http_request = server.recv().expect("test server received the request");
        assert!(matches!(
            parse_request(&mut http_request),
            Err(ReadRequestError::ExecutionDenied)
        ));
        let mut responder = HttpResponder {
            request: Some(http_request),
        };
        denied(&mut responder, "").unwrap();
        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with(r#"{"id":"","error":"denied"}"#));
    }

    #[test]
    fn connection_limiter_admits_up_to_the_cap_and_releases_on_drop() {
        let limiter = Arc::new(ConnectionLimiter::new(2));
        let first = limiter.try_acquire();
        assert!(first.is_some());
        let second = limiter.try_acquire();
        assert!(second.is_some());
        assert!(limiter.try_acquire().is_none());
        drop(first);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn connection_limiter_never_exceeds_the_cap_concurrently() {
        let limiter = Arc::new(ConnectionLimiter::new(MAX_CONNECTIONS));
        let (done, results) = std::sync::mpsc::channel();
        // Every thread races for the same bounded pool. Admitted permits
        // travel back through the channel, so they stay held until the main
        // thread has collected them all.
        for _ in 0..MAX_CONNECTIONS * 2 {
            let limiter = Arc::clone(&limiter);
            let done = done.clone();
            thread::spawn(move || {
                done.send(limiter.try_acquire()).unwrap();
            });
        }
        drop(done);
        let permits: Vec<Option<ConnectionPermit>> = results.iter().collect();
        assert_eq!(
            permits.iter().filter(|permit| permit.is_some()).count(),
            MAX_CONNECTIONS
        );
        drop(permits);
        assert_eq!(*limiter.active.lock().unwrap(), 0);
    }

    #[test]
    fn more_than_one_hundred_headers_are_rejected() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\n").unwrap();
            for index in 0..=MAX_HEADER_COUNT {
                write!(stream, "X-Flood-{index}: value\r\n").unwrap();
            }
            write!(stream, "\r\n").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut http_request = server.recv().expect("test server received the request");
        client.join().unwrap();
        assert!(matches!(
            parse_request(&mut http_request),
            Err(ReadRequestError::HeadersTooLarge)
        ));
    }

    #[test]
    fn exactly_one_hundred_headers_still_parse() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\n").unwrap();
            for index in 0..MAX_HEADER_COUNT {
                write!(stream, "X-Ok-{index}: value\r\n").unwrap();
            }
            write!(stream, "\r\n").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut http_request = server.recv().expect("test server received the request");
        client.join().unwrap();
        let request = parse_request(&mut http_request).unwrap();
        assert_eq!(request.headers.len(), MAX_HEADER_COUNT);
    }

    #[test]
    fn header_block_over_eight_kib_is_rejected() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\nX-Big: ").unwrap();
            stream
                .write_all(&vec![b'a'; MAX_HEADER_BLOCK_BYTES])
                .unwrap();
            write!(stream, "\r\n\r\n").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut http_request = server.recv().expect("test server received the request");
        client.join().unwrap();
        assert!(matches!(
            parse_request(&mut http_request),
            Err(ReadRequestError::HeadersTooLarge)
        ));
    }

    #[test]
    fn oversized_header_block_is_rejected_promptly() {
        // tiny_http parses the header block before we see the request, so the
        // old "never wait for a line terminator" property now lives in
        // tiny_http; what this guards is that an over-budget block is still
        // rejected instead of being processed.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\nX-Big: ").unwrap();
            stream
                .write_all(&vec![b'a'; 2 * MAX_HEADER_BLOCK_BYTES])
                .unwrap();
            write!(stream, "\r\n\r\n").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let started = Instant::now();
        let mut http_request = server.recv().expect("test server received the request");
        let result = parse_request(&mut http_request);
        assert!(matches!(result, Err(ReadRequestError::HeadersTooLarge)));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "over-budget header block was not rejected promptly"
        );
        client.join().unwrap();
    }

    #[test]
    fn reply_reports_431_as_request_header_fields_too_large() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\n\r\n").unwrap();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).unwrap();
                line.push(byte[0]);
                if line.ends_with(b"\r\n") {
                    break;
                }
            }
            let line = String::from_utf8(line).unwrap();
            assert_eq!(line, "HTTP/1.1 431 Request Header Fields Too Large\r\n");
        });
        let http_request = server.recv().expect("test server received the request");
        let mut responder = HttpResponder {
            request: Some(http_request),
        };
        reply(
            &mut responder,
            431,
            error("request header fields too large"),
        )
        .unwrap();
        client.join().unwrap();
    }

    #[test]
    fn github_watch_repo_validation_rejects_unscoped_or_malformed_names() {
        assert!(valid_github_repo("leveled-inc/leveled"));
        assert!(valid_github_repo("owner.name/repo_name-2"));
        assert!(!valid_github_repo("leveled"));
        assert!(!valid_github_repo("owner/repo/extra"));
        assert!(!valid_github_repo("owner/repo space"));
    }

    #[test]
    fn review_state_and_startup_mode_keep_shadow_observational_until_cutover() {
        let base_arguments = || {
            vec![
                "--secret-file".to_owned(),
                "/token".to_owned(),
                "--state-file".to_owned(),
                "/state".to_owned(),
            ]
        };
        let config = server_config(base_arguments()).unwrap();
        assert_eq!(
            config.review_state_file,
            PathBuf::from("/state").with_extension("reviews.json")
        );

        let mut arguments = base_arguments();
        arguments.extend([
            "--watch-repo".to_owned(),
            "leveled-inc/leveled".to_owned(),
            "--watch-interval".to_owned(),
            "60".to_owned(),
        ]);
        let config = server_config(arguments).unwrap();
        assert_eq!(config.github_watch_repos, ["leveled-inc/leveled"]);
        assert_eq!(config.github_watch_interval, Duration::from_secs(60));
        let shadow_authoritative = review_loop::authoritative_mode(true);
        assert!(!shadow_authoritative);
        assert!(should_start_legacy_watch(
            shadow_authoritative,
            &config.github_watch_repos
        ));
        let cutover_authoritative = review_loop::authoritative_mode(false);
        assert!(cutover_authoritative);
        assert!(!should_start_legacy_watch(
            cutover_authoritative,
            &config.github_watch_repos
        ));
    }

    #[test]
    fn github_pr_scan_requires_positive_numbers() {
        assert_eq!(
            parse_github_open_pull_requests(
                r#"[[{"number":42,"html_url":"https://github.com/leveled-inc/leveled/pull/42"}]]"#,
            )
            .unwrap(),
            vec![42]
        );
        assert!(parse_github_open_pull_requests(r#"{"number":42}"#).is_err());
        assert!(
            parse_github_open_pull_requests(r#"[{"number":0,"url":"https://example.test"}]"#)
                .is_err()
        );
    }

    #[test]
    fn process_handles_are_unique_128_bit_hex_values() {
        let mut entries = HashMap::new();
        entries.insert("0".repeat(32), completed_entry());
        let mut handles = std::collections::HashSet::new();
        for _ in 0..256 {
            let handle = unique_handle(&entries).unwrap();
            assert_eq!(handle.len(), 32);
            assert!(handle.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(!entries.contains_key(&handle));
            assert!(handles.insert(handle));
        }
    }

    #[test]
    fn spawn_is_rejected_while_update_drain_is_active() {
        let (state, state_path) = test_server();
        state.updater.begin_drain().unwrap();
        let policy =
            exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#)
                .unwrap();
        let response = request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/spawn",
            r#"{"id":"during-drain","bin":"sh","args":["-c","true"]}"#,
        );
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.ends_with(r#"{"error":"updates_draining"}"#));
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn captured_output_stops_at_the_exec_output_cap() {
        let mut output = CappedOutput::default();
        output.append(&vec![b'x'; COMPAT_OUTPUT_CAP]);
        output.append(b"extra");
        let (captured, truncated) = output.snapshot();
        assert_eq!(captured.len(), COMPAT_OUTPUT_CAP);
        assert!(truncated);
    }

    #[test]
    fn timeline_durations_never_cross_clock_boundaries() {
        let left = relay_event_at(
            "process_spawned",
            "task",
            "execution",
            "2026-01-02T03:04:05.006Z".to_owned(),
            Json::Object(vec![]),
        );
        let right = relay_event_at(
            "first_output",
            "task",
            "execution",
            "2026-01-02T03:04:07.009Z".to_owned(),
            Json::Object(vec![]),
        );
        assert_eq!(same_clock_duration(&left, &right), Some(2_003));
        let mut cross_clock = right.clone();
        if let Json::Object(fields) = &mut cross_clock {
            fields.retain(|(name, _)| name != "clock");
            fields.push(("clock".to_owned(), Json::String("vm:boot".to_owned())));
        }
        assert_eq!(same_clock_duration(&left, &cross_clock), None);
        let unrelated_completion = relay_event_at(
            "process_completed",
            "task",
            "other-execution",
            "2026-01-02T03:04:08.000Z".to_owned(),
            Json::Object(vec![]),
        );
        assert!(
            phase_events(
                &[left.clone(), unrelated_completion],
                "process_spawned",
                "process_completed"
            )
            .is_none()
        );
        let rendered = timeline_output("task", &[left, cross_clock]);
        assert!(rendered.contains("Timeline for task"));
        assert!(rendered.contains("cross-clock/unknown"));
        assert_eq!(
            timeline_output("missing", &[]),
            "No durable audit events for task missing.\n"
        );
    }

    #[test]
    fn recovery_replays_a_persisted_first_output_fact() {
        let path = std::env::temp_dir().join(format!(
            "zigzag-recovery-audit-{}",
            unique_handle(&HashMap::new()).unwrap()
        ));
        let store = Store::open(&path, 10).unwrap();
        let agent = AgentRecord {
            id: "agent".to_owned(),
            task_id: "task".to_owned(),
            execution_id: "execution".to_owned(),
            leader_pid: 1,
            process_group: 1,
            process_identity: Some("test:1".to_owned()),
            started_at: "1".to_owned(),
            deadline_at: None,
            command: "sh -c".to_owned(),
            state: "orphaned".to_owned(),
            exit_code: None,
            log_degraded: false,
            audit_degraded: true,
            redacted: false,
            stdout_next: 3,
            stderr_next: 0,
            stdout_dropped_before: 0,
            stderr_dropped_before: 0,
            log_next: 3,
            log_dropped_before: 0,
            first_output_at: Some("2026-01-02T03:04:05.006Z".to_owned()),
            first_output_stream: Some("stdout".to_owned()),
            first_output_bytes: Some(3),
        };
        replay_recovered_lifecycle(&store, &agent).unwrap();
        let events = store.timeline("task").unwrap();
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|event| event.object("kind").and_then(Json::as_str))
            .collect();
        assert_eq!(kinds, ["process_spawned", "first_output"]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(path.with_extension("audit"));
    }

    #[test]
    fn spawn_request_accepts_only_safe_execution_correlation_ids() {
        let request = parse_spawn_request(
            br#"{"id":"task","execution_id":"attempt-01_A","bin":"sh","args":["-c","true"]}"#,
        )
        .unwrap();
        assert_eq!(request.execution_id.as_deref(), Some("attempt-01_A"));
        for value in ["", "contains space", "slash/value", &"x".repeat(129)] {
            let body = format!(
                r#"{{"id":"task","execution_id":"{value}","bin":"sh","args":["-c","true"]}}"#
            );
            assert!(parse_spawn_request(body.as_bytes()).is_err(), "{value:?}");
        }
        // The synchronous command parser deliberately retains its exact old
        // request shape; spawn-only metadata never reaches argv execution.
        assert!(
            parse_exec_request(
                br#"{"id":"task","execution_id":"attempt","bin":"sh","args":["-c","true"]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn prune_drops_finished_entries_past_the_retention_window() {
        let registry_path = std::env::temp_dir().join(format!(
            "zigzag-test-agents-{}",
            unique_handle(&HashMap::new()).unwrap()
        ));
        let supervisor = Supervisor {
            registry: Arc::new(AgentRegistry::open(&registry_path).unwrap()),
            procs: Mutex::new(HashMap::new()),
        };
        let request = exec::ExecRequest {
            id: "old".to_owned(),
            bin: "sh".to_owned(),
            args: vec!["-c".to_owned(), "exit 0".to_owned()],
        };
        let event_path = registry_path.with_extension("events");
        let handle = spawn_proc(
            &supervisor,
            Arc::new(Store::open(&event_path, 10).unwrap()),
            "/bin/sh",
            request,
            "execution-old".to_owned(),
        )
        .unwrap()
        .handle;
        let mut entries = supervisor.procs.lock().unwrap();
        let entry = entries.get_mut(&handle).unwrap();
        let _ = entry.child.wait();
        entry.finished_at = Some(Instant::now() - FINISHED_PROC_RETENTION - Duration::from_secs(1));
        prune_procs(&mut entries, Instant::now());
        assert!(entries.is_empty());
        let _ = std::fs::remove_file(registry_path);
        let _ = std::fs::remove_file(event_path);
    }

    #[test]
    fn prune_keeps_at_most_128_finished_processes() {
        let mut entries = HashMap::new();
        for index in 0..=MAX_FINISHED_PROCS {
            entries.insert(format!("{index:032x}"), completed_entry());
        }
        prune_procs(&mut entries, Instant::now());
        assert_eq!(entries.len(), MAX_FINISHED_PROCS);
    }

    #[test]
    fn detached_process_endpoints_authenticate_and_manage_process_trees() {
        let policy =
            exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#)
                .unwrap();
        let (state, state_path) = test_server();
        let denied = request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/spawn",
            r#"{"id":"bad","bin":"sh","args":["-c","true"],"extra":true}"#,
        );
        assert!(denied.starts_with("HTTP/1.1 200 OK"));
        assert!(denied.ends_with(r#"{"id":"bad","error":"denied"}"#));
        let policy_denied = request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/spawn",
            r#"{"id":"policy-denied","bin":"sh","args":["not-allowed"]}"#,
        );
        assert!(policy_denied.ends_with(r#"{"id":"policy-denied","error":"denied"}"#));
        let denied_events = state.store.timeline("policy-denied").unwrap();
        assert_eq!(denied_events.len(), 1);
        assert_eq!(
            denied_events[0].object("kind").and_then(Json::as_str),
            Some("relay_request_started")
        );

        let handle = spawn_for_test(&state, &policy, "echo", "printf hello");
        let completed = poll_until_complete(&state, &policy, &handle);
        assert_eq!(
            completed.object("exit_code"),
            Some(&Json::Number("0".to_owned()))
        );
        assert_eq!(
            completed.object("stdout"),
            Some(&Json::String("hello".to_owned()))
        );
        // New diagnostics are read-only and use the same authenticated relay
        // path; the legacy proc response above remains unchanged.
        let agent = response_json(request_once(
            Arc::clone(&state),
            &policy,
            "GET",
            &format!("/v1/agents/{handle}"),
            "",
        ));
        assert_eq!(agent.object("id"), Some(&Json::String(handle.clone())));
        assert_eq!(
            agent.object("state"),
            Some(&Json::String("succeeded".to_owned()))
        );
        let logs = response_json(request_once(
            Arc::clone(&state),
            &policy,
            "GET",
            &format!("/v1/agents/{handle}/logs?stream=stdout&after=0"),
            "",
        ));
        assert!(logs.to_json().contains("hello"));
        let audit = state.store.timeline("echo").unwrap();
        let kinds: Vec<_> = audit
            .iter()
            .filter_map(|event| event.object("kind").and_then(Json::as_str))
            .collect();
        for required in [
            "relay_request_started",
            "relay_accepted",
            "process_spawned",
            "first_output",
            "process_completed",
        ] {
            assert!(kinds.contains(&required), "missing audit event {required}");
        }
        let execution_ids: std::collections::HashSet<_> = audit
            .iter()
            .filter_map(|event| event.object("execution_id").and_then(Json::as_str))
            .collect();
        assert_eq!(execution_ids.len(), 1);
        assert!(execution_ids.iter().next().is_some_and(|id| !id.is_empty()));
        let missing_bin_policy = exec::Policy::parse(
            r#"{"bins":{"missing":{"path":"/definitely/not/installed","commands":[["-c"]]}}}"#,
        )
        .unwrap();
        let failed_spawn = request_once(
            Arc::clone(&state),
            &missing_bin_policy,
            "POST",
            "/v1/spawn",
            r#"{"id":"spawn-failure","bin":"missing","args":["-c","true"]}"#,
        );
        assert!(failed_spawn.starts_with("HTTP/1.1 500 Internal Server Error"));
        assert!(
            state
                .store
                .timeline("spawn-failure")
                .unwrap()
                .iter()
                .any(|event| {
                    event.object("kind").and_then(Json::as_str) == Some("process_failed")
                })
        );
        let explicit = response_json(request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/spawn",
            r#"{"id":"explicit","execution_id":"vm-attempt-7","bin":"sh","args":["-c","printf err >&2; exit 7"]}"#,
        ));
        let explicit_handle = explicit.object("proc").and_then(Json::as_str).unwrap();
        let failed = poll_until_complete(&state, &policy, explicit_handle);
        assert_eq!(
            failed.object("exit_code"),
            Some(&Json::Number("7".to_owned()))
        );
        let explicit_events = state.store.timeline("explicit").unwrap();
        assert!(explicit_events.iter().any(|event| {
            event.object("execution_id").and_then(Json::as_str) == Some("vm-attempt-7")
                && event.object("kind").and_then(Json::as_str) == Some("process_failed")
        }));
        for event in &explicit_events {
            for field in [
                "schema_version",
                "id",
                "task_id",
                "execution_id",
                "kind",
                "source",
                "occurred_at",
                "clock",
                "payload",
            ] {
                assert!(event.object(field).is_some(), "missing {field}");
            }
        }
        assert_eq!(
            explicit_events
                .iter()
                .filter(|event| event.object("kind").and_then(Json::as_str) == Some("first_output"))
                .count(),
            1
        );

        // The shell and its background child remain in the daemon-created
        // process group on both Linux and macOS. The endpoint cannot report
        // completion until the group signal terminates both processes and
        // closes the inherited output pipes.
        let handle = spawn_for_test(&state, &policy, "sleep", "sleep 60 & wait");
        let running = response_json(request_once(
            Arc::clone(&state),
            &policy,
            "GET",
            &format!("/v1/proc/{handle}"),
            "",
        ));
        assert_eq!(running.object("running"), Some(&Json::Bool(true)));
        let recovered = state.supervisor.registry.get(&handle).unwrap();
        assert!(recovered.process_identity.is_some());
        assert!(recovered_agent_identity_matches(&recovered));
        let mut reused_pid = recovered.clone();
        reused_pid.process_identity = Some("different-process-birth".to_owned());
        assert!(!recovered_agent_identity_matches(&reused_pid));
        let killed = response_json(request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            &format!("/v1/proc/{handle}/kill"),
            "",
        ));
        assert_eq!(killed.object("id"), Some(&Json::String("sleep".to_owned())));
        assert_eq!(killed.object("killed"), Some(&Json::Bool(true)));
        let completed = poll_until_complete(&state, &policy, &handle);
        assert_eq!(completed.object("running"), Some(&Json::Bool(false)));

        for (method, target) in [
            ("GET", "/v1/proc/0123456789abcdef0123456789abcdef"),
            ("POST", "/v1/proc/0123456789abcdef0123456789abcdef/kill"),
        ] {
            let response = request_once(Arc::clone(&state), &policy, method, target, "");
            assert!(response.starts_with("HTTP/1.1 404 Not Found"));
            assert!(response.ends_with(r#"{"error":"unknown_proc"}"#));
        }
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }

    fn test_server() -> (Arc<Server>, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "zigzag-proc-test-{}",
            unique_handle(&HashMap::new()).unwrap()
        ));
        (
            Arc::new(Server {
                secret: "x".repeat(32),
                control_secret: Some("x".repeat(32)),
                store: Arc::new(Store::open(&path, 1).unwrap()),
                supervisor: Supervisor {
                    registry: Arc::new(AgentRegistry::open(path.with_extension("agents")).unwrap()),
                    procs: Mutex::new(HashMap::new()),
                },
                updater: Arc::new(update::Manager::new(update::Config {
                    directory: path.with_extension("updates"),
                    interval: Duration::ZERO,
                    policy: update::Policy::Enabled,
                    ready_file: None,
                })),
                review_state_file: path.with_extension("review-state"),
                review_loop_shadow: false,
                review_config: Mutex::new(Some(Arc::new(test_review_config()))),
            }),
            path,
        )
    }

    fn test_review_config() -> review_loop::ReviewLoopConfig {
        review_loop::ReviewLoopConfig {
            enabled: true,
            intervals: review_loop::Intervals {
                discovery_seconds: 1,
                review_seconds: 1,
                merge_seconds: 1,
            },
            repositories: Vec::new(),
        }
    }

    fn completed_entry() -> ProcEntry {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let process_group = child.id() as i32;
        let _ = child.wait();
        ProcEntry {
            child,
            process_group,
            id: "finished".to_owned(),
            bin: "sh".to_owned(),
            subcommand: "-c".to_owned(),
            spawned_at: Instant::now(),
            finished_at: Some(Instant::now()),
            termination_requested_at: None,
            termination_escalated: false,
            leader_reaped: true,
            exit_code: Some(0),
            stdout: Arc::new(Mutex::new(CappedOutput {
                complete: true,
                ..CappedOutput::default()
            })),
            stderr: Arc::new(Mutex::new(CappedOutput {
                complete: true,
                ..CappedOutput::default()
            })),
        }
    }

    fn request_once(
        state: Arc<Server>,
        policy: &exec::Policy,
        method: &str,
        target: &str,
        body: &str,
    ) -> String {
        request_once_with_gate(
            state,
            policy,
            method,
            target,
            body,
            review_loop::gate_report,
        )
    }

    fn request_once_with_gate<G>(
        state: Arc<Server>,
        policy: &exec::Policy,
        method: &str,
        target: &str,
        body: &str,
        gate_report: G,
    ) -> String
    where
        G: Fn(
            &str,
            u64,
            &std::path::Path,
            bool,
            &review_loop::ReviewLoopConfig,
        ) -> Result<serde_json::Value, String>,
    {
        request_once_with_gate_token(
            state,
            policy,
            method,
            target,
            body,
            &"x".repeat(32),
            gate_report,
        )
    }

    fn request_once_with_gate_token<G>(
        state: Arc<Server>,
        policy: &exec::Policy,
        method: &str,
        target: &str,
        body: &str,
        token: &str,
        gate_report: G,
    ) -> String
    where
        G: Fn(
            &str,
            u64,
            &std::path::Path,
            bool,
            &review_loop::ReviewLoopConfig,
        ) -> Result<serde_json::Value, String>,
    {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        // `Connection: close` keeps the one-request-per-connection shape the
        // old server had, so the client can read the response to EOF.
        let request = format!(
            "{method} {target} HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let http_request = server.recv().expect("test server received the request");
        handle_with_services(http_request, state, || Ok(policy.clone()), gate_report).unwrap();
        client.join().unwrap()
    }

    /// Sends a fully raw request (no Authorization or Content-Length added)
    /// at a real tiny_http server and returns the raw response. Used for
    /// HTTP-layer integration tests: auth ordering, framing anomalies, and
    /// malformed input.
    fn raw_request_once(state: Arc<Server>, policy: &exec::Policy, raw: &str) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let raw = raw.to_owned();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        // tiny_http itself answers malformed request lines/headers with an
        // empty 400 and closes; those never reach the handler.
        if let Some(http_request) = server
            .recv_timeout(Duration::from_secs(5))
            .expect("recv did not error")
        {
            handle_with_policy(http_request, state, || Ok(policy.clone())).unwrap();
        }
        client.join().unwrap()
    }

    fn test_policy() -> exec::Policy {
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap()
    }

    fn test_token() -> String {
        "x".repeat(32)
    }

    #[test]
    fn http_layer_rejects_malformed_input_with_400() {
        let (state, state_path) = test_server();
        let policy = test_policy();
        for raw in [
            "GARBAGE\r\n\r\n",
            "GET\r\n\r\n",
            "GET /v1/health\r\n\r\n",
            "GET /v1/health HTTP/1.1\r\nNo-Colon-Here\r\n\r\n",
        ] {
            let response = raw_request_once(Arc::clone(&state), &policy, raw);
            assert!(
                response.starts_with("HTTP/1.1 400"),
                "{raw:?} -> {response:?}"
            );
        }
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn http_layer_enforces_auth_before_routing() {
        let (state, state_path) = test_server();
        let policy = test_policy();
        let token = test_token();
        // No credentials on a real route: 401, not 404.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            "GET /v1/health HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "{response:?}"
        );
        assert!(response.ends_with(r#"{"error":"unauthorized"}"#));
        // Wrong token: 401.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            "GET /v1/health HTTP/1.1\r\nAuthorization: Bearer wrong\r\nConnection: close\r\n\r\n",
        );
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "{response:?}"
        );
        // Authenticated unknown route: 404. Auth runs before routing.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "GET /nope HTTP/1.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found"),
            "{response:?}"
        );
        assert!(response.ends_with(r#"{"error":"not_found"}"#));
        // Wrong method on a real route: 404, like the old server.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "DELETE /v1/health HTTP/1.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found"),
            "{response:?}"
        );
        // Health check works end to end.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "GET /v1/health HTTP/1.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:?}");
        assert!(response.ends_with(r#"{"status":"ok"}"#));
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn http_layer_rejects_framing_anomalies() {
        let (state, state_path) = test_server();
        let policy = test_policy();
        let token = test_token();
        // Conflicting duplicate Content-Length values: fail closed, never pick one.
        // (The 5-byte body lets tiny_http's eager small-body read complete so
        // the request reaches the conflict check; without it the truncated
        // body aborts the connection first, which is equally fail-closed.)
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "POST /v1/events HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 5\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response:?}"
        );
        assert!(response.ends_with(r#"{"error":"invalid content length"}"#));
        // Unparseable Content-Length: 400.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "POST /v1/events HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: bogus\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response:?}"
        );
        assert!(response.ends_with(r#"{"error":"invalid content length"}"#));
        // Transfer-Encoding was never understood by the old server; reject it.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "POST /v1/events HTTP/1.1\r\nAuthorization: Bearer {token}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response:?}"
        );
        assert!(response.ends_with(r#"{"error":"transfer encoding not supported"}"#));
        // On the exec boundary the same anomaly stays an opaque denial.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "POST /v1/exec HTTP/1.1\r\nAuthorization: Bearer {token}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:?}");
        assert!(response.ends_with(r#"{"id":"","error":"denied"}"#));
        // Oversized non-exec body: 400, like the old server.
        let response = raw_request_once(
            Arc::clone(&state),
            &policy,
            &format!(
                "POST /v1/events HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_BODY + 1
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response:?}"
        );
        assert!(response.ends_with(r#"{"error":"request body too large"}"#));
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn http_layer_supports_keep_alive_across_requests() {
        let (state, state_path) = test_server();
        let policy = test_policy();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let token = test_token();
        let client = thread::spawn(move || {
            let stream = TcpStream::connect(address).unwrap();
            let mut reader = std::io::BufReader::new(&stream);
            let mut results = Vec::new();
            for _ in 0..2 {
                (&stream)
                    .write_all(
                        format!(
                            "GET /v1/health HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                let mut status = String::new();
                std::io::BufRead::read_line(&mut reader, &mut status).unwrap();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; content_length];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                results.push((status, body));
            }
            results
        });
        for _ in 0..2 {
            let http_request = server
                .recv_timeout(Duration::from_secs(5))
                .expect("recv did not error")
                .expect("request arrived");
            let state = Arc::clone(&state);
            let policy = policy.clone();
            handle_with_policy(http_request, state, move || Ok(policy.clone())).unwrap();
        }
        for (status, body) in client.join().unwrap() {
            assert!(status.starts_with("HTTP/1.1 200 OK"), "{status:?}");
            assert_eq!(body, br#"{"status":"ok"}"#.as_slice());
        }
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }

    fn response_json(response: String) -> Json {
        parse_json(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    fn spawn_for_test(
        state: &Arc<Server>,
        policy: &exec::Policy,
        id: &str,
        command: &str,
    ) -> String {
        let response = response_json(request_once(
            Arc::clone(state),
            policy,
            "POST",
            "/v1/spawn",
            &format!(r#"{{"id":"{id}","bin":"sh","args":["-c","{command}"]}}"#),
        ));
        response
            .object("proc")
            .and_then(Json::as_str)
            .unwrap()
            .to_owned()
    }

    fn poll_until_complete(state: &Arc<Server>, policy: &exec::Policy, handle: &str) -> Json {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let response = response_json(request_once(
                Arc::clone(state),
                policy,
                "GET",
                &format!("/v1/proc/{handle}"),
                "",
            ));
            if response.object("running") == Some(&Json::Bool(false)) {
                return response;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("process did not finish in time");
    }
}
