mod auth;
mod config;
mod events;
mod exec;
mod http;
mod logging;
mod review_loop;
mod update;

use crate::auth::authorized;
use crate::config::{run_config, server_config};
use crate::events::{
    new_execution_id, persist_first_output, random_hex_128, relay_event, relay_timestamp,
    replay_recovered_lifecycle, unix_timestamp,
};
use crate::http::{
    ReadRequestError, denial_json, denied, error, get_query, query, read_json, read_request, reply,
};
use relay_core::{
    AgentRecord, AgentRegistry, Json, Store, parse_json, parse_rfc3339_millis, read_secret_file,
};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

/// Upper bound on simultaneous in-flight connections. The accept loop
/// sheds excess connections with 503 instead of spawning unbounded
/// threads: each thread carries a stack plus a 10s read timeout, so a
/// slow flood could otherwise exhaust memory or file descriptors.
const MAX_CONNECTIONS: usize = 32;
const MAX_FINISHED_PROCS: usize = 128;
const FINISHED_PROC_RETENTION: Duration = Duration::from_secs(60 * 60);
const COMPAT_OUTPUT_CAP: usize = 2 * 1024 * 1024;
#[cfg(any(target_os = "macos", test))]
const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
#[cfg(any(target_os = "macos", test))]
const SESSION_IS_REMOTE: u32 = 0x1000;
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
struct SpawnRequest {
    command: exec::ExecRequest,
    execution_id: Option<String>,
}
fn main() {
    logging::init();
    if let Err(error) = run() {
        log::error!("startup failed: {error}");
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
    log::info!(
        "loaded server config: port={} state_file={} agent_registry_file={} max_events={}",
        config.port,
        config.state_file.display(),
        config.agent_registry_file.display(),
        config.max_events
    );
    let secret = read_secret_file(&config.secret_file)?;
    log::info!("loaded daemon secret from {}", config.secret_file.display());
    let control_secret = config
        .control_secret_file
        .as_deref()
        .map(read_secret_file)
        .transpose()?;
    if let Some(path) = config.control_secret_file.as_deref() {
        log::info!("loaded control secret from {}", path.display());
    }
    let updater = Arc::new(update::Manager::new(update::Config {
        directory: config.update_directory.clone(),
        interval: config.update_interval,
        policy: config.update_policy.clone(),
        ready_file: config.update_ready_file.clone(),
    }));
    let review_loop_shadow = env::var("ZIGZAG_REVIEW_LOOP_SHADOW").as_deref() == Ok("1");
    let state_file = config.state_file.clone();
    let store = Arc::new(Store::open(state_file.clone(), config.max_events)?);
    log::info!(
        "opened event store {} (max_events={})",
        state_file.display(),
        config.max_events
    );
    let agent_registry_file = config.agent_registry_file.clone();
    let registry = Arc::new(AgentRegistry::open(agent_registry_file.clone())?);
    let agent_records = registry.list(None, None).len();
    log::info!(
        "opened agent registry {} with {agent_records} records",
        agent_registry_file.display()
    );
    let state = Arc::new(Server {
        secret,
        control_secret,
        store,
        supervisor: Supervisor {
            registry,
            procs: Mutex::new(HashMap::new()),
        },
        updater: Arc::clone(&updater),
        review_state_file: config.review_state_file.clone(),
        review_loop_shadow,
        review_config: Mutex::new(None),
    });
    let recovered = state
        .supervisor
        .registry
        .recover(recovered_agent_identity_matches)?;
    log::info!("recovered {} agent records from registry", recovered.len());
    for agent in recovered {
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
                    Err(error) => log::warn!("review loop disabled: {error}"),
                }
            }
            Ok(_) => log::info!("review loop disabled by ~/.zigzag/config.yaml"),
            Err(violations) => {
                log::warn!("review loop disabled: invalid ~/.zigzag/config.yaml");
                for violation in violations {
                    log::warn!("review loop config: {violation}");
                }
            }
        },
        Err(violation) => log::warn!("review loop disabled: {violation}"),
    }
    if should_start_legacy_watch(review_loop_authoritative, &config.github_watch_repos) {
        let state = Arc::clone(&state);
        let repos = config.github_watch_repos.clone();
        let interval = config.github_watch_interval;
        thread::spawn(move || github_watch_loop(state, repos, interval));
        log::info!(
            "started GitHub PR watch loop for {} repositories (interval={:?})",
            config.github_watch_repos.len(),
            config.github_watch_interval
        );
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
        let listener = TcpListener::bind(address)
            .map_err(|error| format!("could not bind {address}: {error}"))?;
        let state = Arc::clone(&state);
        let limiter = Arc::clone(&limiter);
        log::info!("listening on http://{address}");
        thread::spawn(move || serve(listener, state, limiter));
    }
    // The replacement only signals readiness after it has opened durable state
    // and rebound both listeners. The watchdog rolls back if this does not
    // happen; no launchctl restart is involved.
    let replacement_version = env::var("ZIGZAG_UPDATE_VERSION").ok();
    updater.acknowledge_ready(replacement_version.as_deref())?;
    log::info!("signaled readiness to update watchdog");
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
    log::info!(
        "started update manager (directory={}, interval={:?})",
        config.update_directory.display(),
        config.update_interval
    );
    log::info!("zigzag startup complete");
    loop {
        thread::park();
    }
}
fn run_timeline(arguments: &[String]) -> Result<(), String> {
    let mut state_file = env::var_os("ZIGZAG_STATE_FILE").map(PathBuf::from);
    let mut task_id = None;
    let mut values = arguments.iter();
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--state-file" => {
                state_file = Some(PathBuf::from(
                    values
                        .next()
                        .ok_or_else(|| "--state-file requires a value".to_owned())?,
                ));
            }
            "--help" | "-h" => {
                return Err(
                    "usage: zigzag timeline <task-id> --state-file PATH (or ZIGZAG_STATE_FILE)"
                        .to_owned(),
                );
            }
            value if !value.starts_with('-') && task_id.is_none() => {
                task_id = Some(value.to_owned())
            }
            value => return Err(format!("unknown timeline argument: {value}")),
        }
    }
    let task_id = task_id.ok_or_else(|| "timeline requires a task id".to_owned())?;
    let state_file = state_file
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
                                log::debug!("queued GitHub PR watchdog event for {repo}#{number}")
                            }
                            Ok((_, true)) => {}
                            Err(_) => log::warn!(
                                "could not persist GitHub PR watchdog event for {repo}#{number}"
                            ),
                        }
                    }
                }
                Err(error) => log::warn!("GitHub PR watch for {repo} failed: {error}"),
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
        .verified_path(&request.bin, &request.args)
        .map_err(|error| format!("the gh policy does not allow the PR scan: {error}"))?;
    let result = exec::run(&path, request);
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
fn serve(listener: TcpListener, state: Arc<Server>, limiter: Arc<ConnectionLimiter>) {
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => match limiter.try_acquire() {
                Some(permit) => {
                    let state = Arc::clone(&state);
                    thread::spawn(move || {
                        let _permit = permit;
                        let _ = handle(stream, state);
                    });
                }
                None => {
                    log::warn!("connection shed: already at {MAX_CONNECTIONS} connections");
                    let _ = reply(&mut stream, 503, error("too_many_connections"));
                }
            },
            Err(error) => log::warn!("accept error: {error}"),
        }
    }
}
fn handle(stream: TcpStream, state: Arc<Server>) -> Result<(), String> {
    handle_with_policy(stream, state, || {
        require_gui_login_session().and_then(|_| exec::load_policy())
    })
}
fn handle_with_policy<F>(
    stream: TcpStream,
    state: Arc<Server>,
    load_policy: F,
) -> Result<(), String>
where
    F: Fn() -> Result<exec::Policy, String>,
{
    handle_with_services(stream, state, load_policy, review_loop::gate_report)
}
fn handle_with_services<F, G>(
    mut stream: TcpStream,
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
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(ReadRequestError::ExecutionDenied) => {
            denied(&mut stream, "")?;
            return Ok(());
        }
        Err(ReadRequestError::HeadersTooLarge) => {
            reply(
                &mut stream,
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
                &mut stream,
                400,
                Json::Object(vec![("error".to_owned(), Json::String(error))]),
            )?;
            return Ok(());
        }
    };
    let request_path = request.target.split('?').next().unwrap_or("");
    // Log the incoming request with source IP for observability.
    let source = stream
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    let _request_guard = logging::begin_request(&request.method, request_path, &source);
    let control_route =
        request.method == "POST" && matches!(proc_route(request_path), Some(ProcRoute::Kill(_)));
    let Some(required_secret) = (if control_route {
        state.control_secret.as_deref()
    } else {
        Some(state.secret.as_str())
    }) else {
        return reply(&mut stream, 404, error("not_found"));
    };
    if !authorized(
        request
            .headers
            .get("authorization")
            .map(String::as_str)
            .unwrap_or(""),
        required_secret,
    ) {
        log::warn!("auth_failed path={request_path} source={source}");
        reply(&mut stream, 401, error("unauthorized"))?;
        return Ok(());
    }
    match (request.method.as_str(), request_path) {
        ("GET", "/v1/health") => reply(
            &mut stream,
            200,
            Json::Object(vec![("status".to_owned(), Json::String("ok".to_owned()))]),
        ),
        ("POST", "/v1/events") => post(&mut stream, &state, request.body),
        ("GET", "/v1/events") => get(&mut stream, &state, &request.target),
        ("POST", "/v1/exec") => exec_request(&mut stream, request.body),
        ("POST", "/v1/spawn") => spawn_request(&mut stream, &state, request.body, load_policy()),
        ("POST", "/v1/worktrees") => worktree_create(&mut stream, request.body),
        ("DELETE", "/v1/worktrees") => worktree_delete(&mut stream, &state, request.body),
        ("GET", "/v1/review-gate") => {
            review_gate_request(&mut stream, &state, &request.target, gate_report)
        }
        ("GET", path) if agent_route(path).is_some() => agent_request(
            &mut stream,
            &state,
            &request.target,
            agent_route(path).expect("checked"),
        ),
        ("GET", path) => match proc_route(path) {
            Some(ProcRoute::Poll(handle)) => poll_proc(&mut stream, &state, handle),
            Some(ProcRoute::Kill(_)) => reply(&mut stream, 404, error("not_found")),
            None => reply(&mut stream, 404, error("not_found")),
        },
        ("POST", path) => match proc_route(path) {
            Some(ProcRoute::Kill(handle)) => kill_proc(&mut stream, &state, handle),
            Some(ProcRoute::Poll(_)) => reply(&mut stream, 404, error("not_found")),
            None => reply(&mut stream, 404, error("not_found")),
        },
        _ => reply(&mut stream, 404, error("not_found")),
    }
}
fn review_gate_request<G>(
    stream: &mut TcpStream,
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
        Err(()) => return reply(stream, 400, error("invalid_review_gate_query")),
    };
    let config = match state.review_config.lock() {
        Ok(config) => config.clone(),
        Err(_) => return reply(stream, 500, error("review_gate_failed")),
    };
    let Some(config) = config else {
        return reply(stream, 500, error("review_gate_failed"));
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
            reply(stream, 200, response)
        }
        Err(error_message) => {
            log::warn!("review gate failed for {repository}#{number}: {error_message}");
            reply(stream, 500, error("review_gate_failed"))
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
    stream: &mut TcpStream,
    state: &Server,
    target: &str,
    route: AgentRoute<'_>,
) -> Result<(), String> {
    let values = match query(target) {
        Ok(values) => values,
        Err(_) => return reply(stream, 400, error("invalid_agent_query")),
    };
    match route {
        AgentRoute::List => {
            let allowed = ["state", "task_id"];
            if values.keys().any(|key| !allowed.contains(&key.as_str())) {
                return reply(stream, 400, error("invalid_agent_query"));
            }
            let agents = state.supervisor.registry.list(
                values.get("state").map(String::as_str),
                values.get("task_id").map(String::as_str),
            );
            reply(
                stream,
                200,
                Json::Object(vec![(
                    "agents".to_owned(),
                    Json::Array(agents.iter().map(AgentRecord::status_json).collect()),
                )]),
            )
        }
        AgentRoute::Status(id) => match state.supervisor.registry.get(id) {
            Some(agent) => reply(stream, 200, agent.status_json()),
            None => reply(stream, 404, error("unknown_agent")),
        },
        AgentRoute::Logs(id) => {
            let allowed = ["stream", "after", "tail", "follow"];
            if values.keys().any(|key| !allowed.contains(&key.as_str())) {
                return reply(stream, 400, error("invalid_log_query"));
            }
            let stream_name = values.get("stream").map(String::as_str).unwrap_or("both");
            if !matches!(stream_name, "stdout" | "stderr" | "both") {
                return reply(stream, 400, error("invalid_log_query"));
            }
            let after = match values
                .get("after")
                .map_or(Ok(0), |value| value.parse::<u64>())
            {
                Ok(value) => value,
                Err(_) => return reply(stream, 400, error("invalid_log_query")),
            };
            let tail = match values
                .get("tail")
                .map(|value| value.parse::<usize>())
                .transpose()
            {
                Ok(value) => value,
                Err(_) => return reply(stream, 400, error("invalid_log_query")),
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
                    Err(_) => return reply(stream, 400, error("invalid_log_query")),
                };
            let deadline = Instant::now() + Duration::from_secs(50);
            loop {
                let Some(logs) = state
                    .supervisor
                    .registry
                    .logs_json(id, stream_name, after, tail)
                else {
                    return reply(stream, 404, error("unknown_agent"));
                };
                let has_records = logs.object("records").is_some_and(
                    |records| matches!(records, Json::Array(records) if !records.is_empty()),
                );
                if !follow || has_records || Instant::now() >= deadline {
                    return reply(stream, 200, logs);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
fn post(stream: &mut TcpStream, state: &Server, body: Vec<u8>) -> Result<(), String> {
    let body = match String::from_utf8(body) {
        Ok(body) => body,
        Err(_) => {
            reply(
                stream,
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
                stream,
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
            stream,
            400,
            error("body_must_be_an_object_with_nonempty_id"),
        )?;
        return Ok(());
    }
    match state.store.add(payload) {
        Ok((event, duplicate)) => reply(
            stream,
            if duplicate { 200 } else { 201 },
            Json::Object(vec![
                ("duplicate".to_owned(), Json::Bool(duplicate)),
                ("event".to_owned(), event.response_json()),
            ]),
        ),
        Err(_) => reply(stream, 500, error("could_not_persist_event")),
    }
}
// --- Worktree management endpoints (agent-creation rollout, part 1 of 4) ---
/// Roots the relay may create or remove git worktrees under. Candidate paths
/// are canonicalized before the prefix check, so `..` segments and symlinks
/// cannot escape the root.
const WORKTREE_ALLOWED_ROOTS: [&str; 2] = ["/private/tmp/", "/Users/shukant/.codex/worktrees/"];
/// Root the `repo` parameter of worktree creation must live under, so callers
/// cannot point `git worktree add` at an arbitrary repository.
const WORKTREE_REPO_ROOT: &str = "/Users/shukant/Workspace/";
/// Failure from the worktree core logic: the HTTP status and the snake_case
/// error body the relay replies with. Handlers stay thin so tests can drive
/// `worktree_create_plan` / `worktree_delete_plan` directly.
#[derive(Clone, Copy, Debug)]
struct WorktreeError {
    code: u16,
    message: &'static str,
}
/// Canonicalize each configured root, dropping roots that do not exist. An
/// empty result rejects every path (fail closed).
fn canonical_worktree_roots() -> Vec<PathBuf> {
    WORKTREE_ALLOWED_ROOTS
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}
/// Canonicalize `path` (which must exist) and require it to sit under `roots`.
fn canonical_path_under_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, WorktreeError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| WorktreeError {
        code: 400,
        message: "worktree_path_not_found",
    })?;
    if roots.iter().any(|root| canonical.starts_with(root)) {
        Ok(canonical)
    } else {
        Err(WorktreeError {
            code: 400,
            message: "worktree_path_outside_allowed_roots",
        })
    }
}
/// Resolve the worktree path for creation. The path itself may not exist yet,
/// so canonicalize the parent directory and re-attach the leaf: canonicalizing
/// the parent defeats `..` traversal and symlink escapes in every ancestor.
fn resolve_new_worktree_path(raw: &str, roots: &[PathBuf]) -> Result<PathBuf, WorktreeError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_relative() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_path_must_be_absolute",
        });
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(WorktreeError {
            code: 400,
            message: "worktree_path_has_no_parent",
        })?;
    let leaf = path.file_name().ok_or(WorktreeError {
        code: 400,
        message: "worktree_path_has_no_name",
    })?;
    Ok(canonical_path_under_roots(parent, roots)?.join(leaf))
}
/// Resolve the worktree path for deletion: it must already exist.
fn resolve_existing_worktree_path(raw: &str, roots: &[PathBuf]) -> Result<PathBuf, WorktreeError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_relative() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_path_must_be_absolute",
        });
    }
    canonical_path_under_roots(path, roots)
}
/// Resolve the `repo` parameter: it must exist and live under the workspace
/// root.
fn resolve_worktree_repo(raw: &str, repo_root: &Path) -> Result<PathBuf, WorktreeError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_relative() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_repo_must_be_absolute",
        });
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| WorktreeError {
        code: 400,
        message: "worktree_repo_not_found",
    })?;
    if canonical.starts_with(repo_root) {
        Ok(canonical)
    } else {
        Err(WorktreeError {
            code: 400,
            message: "worktree_repo_outside_workspace",
        })
    }
}
/// Reject branch names git would treat as options or refuse as ref names.
/// `git` itself is the final arbiter; this keeps hostile input from ever
/// reaching the command line.
fn valid_worktree_branch(branch: &str) -> bool {
    if branch.is_empty() || branch.len() > 255 {
        return false;
    }
    if branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with(".lock")
    {
        return false;
    }
    if branch.contains("..") || branch.contains("@{") {
        return false;
    }
    !branch
        .chars()
        .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}
fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output, WorktreeError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| {
            log::error!("worktree git invocation failed: {error}");
            WorktreeError {
                code: 500,
                message: "worktree_git_failed",
            }
        })
}
/// True when `refs/heads/<branch>` exists. Exit 0 means present, exit 1 means
/// absent; anything else is a genuine git failure.
fn worktree_branch_exists(repo: &Path, branch: &str) -> Result<bool, WorktreeError> {
    let output = git_output(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(WorktreeError {
            code: 500,
            message: "worktree_git_failed",
        }),
    }
}
/// True when the branch is already checked out in some worktree, which `git
/// worktree add` would refuse.
fn worktree_branch_checked_out(repo: &Path, branch: &str) -> Result<bool, WorktreeError> {
    let output = git_output(repo, &["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        return Err(WorktreeError {
            code: 500,
            message: "worktree_git_failed",
        });
    }
    let wanted = format!("branch refs/heads/{branch}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == wanted))
}
/// Core of `POST /v1/worktrees`: validate, then run `git worktree add`,
/// creating the branch when it does not exist yet. Returns the 200 body.
fn worktree_create_plan(
    path_raw: &str,
    branch: &str,
    repo_raw: &str,
    roots: &[PathBuf],
    repo_root: &Path,
) -> Result<Json, WorktreeError> {
    let path = resolve_new_worktree_path(path_raw, roots)?;
    let repo = resolve_worktree_repo(repo_raw, repo_root)?;
    if !valid_worktree_branch(branch) {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_invalid_branch",
        });
    }
    if !git_output(&repo, &["rev-parse", "--git-dir"])?
        .status
        .success()
    {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_repo_not_a_git_repo",
        });
    }
    if worktree_branch_checked_out(&repo, branch)? {
        log::warn!("worktree_create refused: branch {branch} already checked out");
        return Err(WorktreeError {
            code: 400,
            message: "worktree_branch_already_checked_out",
        });
    }
    let path_str = path.to_str().ok_or(WorktreeError {
        code: 400,
        message: "worktree_path_not_unicode",
    })?;
    let output = if worktree_branch_exists(&repo, branch)? {
        log::info!(
            "worktree_create path={} branch={branch} existing_branch=true",
            path.display()
        );
        git_output(&repo, &["worktree", "add", path_str, branch])?
    } else {
        log::info!(
            "worktree_create path={} branch={branch} existing_branch=false",
            path.display()
        );
        git_output(&repo, &["worktree", "add", "-b", branch, path_str])?
    };
    if !output.status.success() {
        log::warn!(
            "worktree_create git failed for branch={branch}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Err(WorktreeError {
            code: 400,
            message: "worktree_git_add_failed",
        });
    }
    Ok(Json::Object(vec![
        (
            "path".to_owned(),
            Json::String(path.to_string_lossy().into_owned()),
        ),
        ("branch".to_owned(), Json::String(branch.to_owned())),
    ]))
}
/// Core of `DELETE /v1/worktrees`. `agents` carries `(state, command)` pairs
/// from the agent registry for the live-attachment check. Returns the 200 body.
fn worktree_delete_plan(
    path_raw: &str,
    roots: &[PathBuf],
    agents: &[(&str, &str)],
) -> Result<Json, WorktreeError> {
    let path = resolve_existing_worktree_path(path_raw, roots)?;
    let path_str = path.to_string_lossy();
    // TODO: match on an explicit worktree_path field on the agent record once
    // the agent-creation endpoints record it; command-substring matching is a
    // stopgap until then.
    let attached = agents.iter().any(|(state, command)| {
        matches!(*state, "running" | "orphaned")
            && (command.contains(&*path_str) || command.contains(path_raw))
    });
    if attached {
        log::warn!(
            "worktree_delete refused: live agent attached to {}",
            path.display()
        );
        return Err(WorktreeError {
            code: 409,
            message: "worktree_in_use",
        });
    }
    // `git worktree remove` runs from the owning repo: resolve the main repo
    // through the worktree's common git dir.
    let common = git_output(&path, &["rev-parse", "--git-common-dir"])?;
    if !common.status.success() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_not_a_git_worktree",
        });
    }
    let common_dir = String::from_utf8_lossy(&common.stdout);
    let common_dir = common_dir.trim();
    let common_dir = if Path::new(common_dir).is_absolute() {
        PathBuf::from(common_dir)
    } else {
        path.join(common_dir)
    };
    let common_dir = std::fs::canonicalize(&common_dir).map_err(|_| WorktreeError {
        code: 400,
        message: "worktree_not_a_git_worktree",
    })?;
    if common_dir.file_name().is_none_or(|name| name != ".git") {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_not_a_git_worktree",
        });
    }
    let repo = common_dir
        .parent()
        .ok_or(WorktreeError {
            code: 400,
            message: "worktree_not_a_git_worktree",
        })?
        .to_path_buf();
    log::info!("worktree_delete path={}", path.display());
    let remove = git_output(&repo, &["worktree", "remove", "--force", &path_str])?;
    if !remove.status.success() {
        log::warn!(
            "worktree_delete remove failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&remove.stderr).trim()
        );
        return Err(WorktreeError {
            code: 400,
            message: "worktree_git_remove_failed",
        });
    }
    // Prune stale administrative entries (e.g. worktrees deleted by hand).
    if let Err(error) = git_output(&repo, &["worktree", "prune"]) {
        log::warn!("worktree_delete prune failed: {}", error.message);
    }
    Ok(Json::Object(vec![
        ("removed".to_owned(), Json::Bool(true)),
        ("path".to_owned(), Json::String(path_str.into_owned())),
    ]))
}
fn worktree_request_fields(
    body: &[u8],
    allowed: &[&str],
) -> Result<Vec<(String, Json)>, WorktreeError> {
    let text = std::str::from_utf8(body).map_err(|_| WorktreeError {
        code: 400,
        message: "invalid_worktree_request",
    })?;
    let parsed = parse_json(text).map_err(|_| WorktreeError {
        code: 400,
        message: "invalid_worktree_request",
    })?;
    let Json::Object(fields) = parsed else {
        return Err(WorktreeError {
            code: 400,
            message: "invalid_worktree_request",
        });
    };
    if fields
        .iter()
        .any(|(name, _)| !allowed.contains(&name.as_str()))
    {
        return Err(WorktreeError {
            code: 400,
            message: "invalid_worktree_request",
        });
    }
    Ok(fields)
}
fn worktree_string_field(fields: &[(String, Json)], name: &str) -> Result<String, WorktreeError> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(WorktreeError {
            code: 400,
            message: "invalid_worktree_request",
        })
}
fn worktree_create(stream: &mut TcpStream, body: Vec<u8>) -> Result<(), String> {
    let fields = match worktree_request_fields(&body, &["path", "branch", "repo"]) {
        Ok(fields) => fields,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let (path, branch, repo) = match (
        worktree_string_field(&fields, "path"),
        worktree_string_field(&fields, "branch"),
        worktree_string_field(&fields, "repo"),
    ) {
        (Ok(path), Ok(branch), Ok(repo)) => (path, branch, repo),
        _ => return reply(stream, 400, error("invalid_worktree_request")),
    };
    let roots = canonical_worktree_roots();
    let repo_root = std::fs::canonicalize(WORKTREE_REPO_ROOT)
        .unwrap_or_else(|_| PathBuf::from(WORKTREE_REPO_ROOT));
    match worktree_create_plan(&path, &branch, &repo, &roots, &repo_root) {
        Ok(response) => reply(stream, 200, response),
        Err(failure) => reply(stream, failure.code, error(failure.message)),
    }
}
fn worktree_delete(stream: &mut TcpStream, state: &Server, body: Vec<u8>) -> Result<(), String> {
    let fields = match worktree_request_fields(&body, &["path"]) {
        Ok(fields) => fields,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let path = match worktree_string_field(&fields, "path") {
        Ok(path) => path,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let agents = state.supervisor.registry.list(None, None);
    let commands: Vec<(&str, &str)> = agents
        .iter()
        .map(|agent| (agent.state.as_str(), agent.command.as_str()))
        .collect();
    match worktree_delete_plan(&path, &canonical_worktree_roots(), &commands) {
        Ok(response) => reply(stream, 200, response),
        Err(failure) => reply(stream, failure.code, error(failure.message)),
    }
}
fn exec_request(stream: &mut TcpStream, body: Vec<u8>) -> Result<(), String> {
    let request = match parse_exec_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(stream, 200, denial),
    };
    // Prompts can be sensitive, so logs contain only the redacted routing data.
    let exec_route = logging::exec_route(&request.bin, &request.args);
    let exec_source = stream
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    log::info!("exec id={} {exec_route} source={exec_source}", request.id);
    let policy = match require_gui_login_session().and_then(|_| exec::load_policy()) {
        Ok(policy) => policy,
        Err(message) => {
            log::error!("exec policy read failed: {message}");
            return reply(stream, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy.verified_path(&request.bin, &request.args) {
        Ok(path) => path,
        Err(exec::VerifyError::Denied) => {
            return reply(stream, 200, denial_json(&request.id));
        }
        Err(exec::VerifyError::Unverifiable(error)) => {
            eprintln!("exec binary verification failed: {error}");
            // Mirror the spawn-failure shape: 200 with a result whose stderr
            // explains the failure without naming the path.
            let result = exec::ExecResult {
                id: request.id.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: "could not verify configured binary".to_owned(),
                truncated: false,
                timed_out: false,
            };
            return reply(stream, 200, result.to_json());
        }
    };
    let started = Instant::now();
    let result = exec::run(&path, request);
    let duration_ms = started.elapsed().as_millis();
    // Log failures at ERROR level, successes at INFO.
    let failed = result.timed_out || matches!(result.exit_code, Some(code) if code != 0);
    if failed {
        log::error!(
            "exec_failed id={} {exec_route} exit_code={:?} timed_out={} duration_ms={} source={exec_source}",
            result.id,
            result.exit_code,
            result.timed_out,
            duration_ms,
        );
    } else {
        log::info!(
            "exec id={} {exec_route} finished duration_ms={} exit_code={:?} source={exec_source}",
            result.id,
            duration_ms,
            result.exit_code,
        );
    }
    reply(stream, 200, result.to_json())
}
fn spawn_request(
    stream: &mut TcpStream,
    state: &Server,
    body: Vec<u8>,
    policy: Result<exec::Policy, String>,
) -> Result<(), String> {
    if state.updater.is_draining() {
        return reply(stream, 503, error("updates_draining"));
    }
    let request = match parse_spawn_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(stream, 200, denial),
    };
    log::info!(
        "spawn id={} {}",
        request.command.id,
        logging::exec_route(&request.command.bin, &request.command.args)
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
        return reply(stream, 500, error("could_not_persist_event"));
    }
    let policy = match policy {
        Ok(policy) => policy,
        Err(message) => {
            log::error!("spawn policy read failed: {message}");
            return reply(stream, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy.verified_path(&request.command.bin, &request.command.args) {
        Ok(path) => path,
        Err(exec::VerifyError::Denied) => {
            return reply(stream, 200, denial_json(&request.command.id));
        }
        Err(exec::VerifyError::Unverifiable(reason)) => {
            eprintln!("spawn binary verification failed: {reason}");
            // A failed spawn is a failed spawn, whether the binary was
            // missing or failed integrity verification: keep the audit event.
            if state
                .store
                .add(relay_event(
                    "process_failed",
                    &request.command.id,
                    &execution_id,
                    Json::Object(vec![(
                        "reason".to_owned(),
                        Json::String("binary_verification_failed".to_owned()),
                    )]),
                ))
                .is_err()
            {
                eprintln!("could not persist spawn failure audit event");
            }
            return reply(stream, 500, error("could_not_verify_binary"));
        }
    };
    // The updater takes the same gate while setting `draining`, so a child
    // cannot appear between the drain check and its durable registry record.
    let _spawn_admission = match state.updater.spawn_admission() {
        Ok(Some(guard)) => guard,
        Ok(None) => return reply(stream, 503, error("updates_draining")),
        Err(message) => {
            log::error!("spawn admission failed: {message}");
            return reply(stream, 500, error("could_not_admit_process"));
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
        return reply(stream, 500, error("could_not_persist_event"));
    }
    let task_id = request.command.id.clone();
    match spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        &path,
        request.command,
        execution_id.clone(),
    ) {
        Ok(handle) => {
            log::info!("spawn id={} handle={}", handle.id, handle.handle);
            reply(
                stream,
                200,
                Json::Object(vec![
                    ("id".to_owned(), Json::String(handle.id)),
                    ("proc".to_owned(), Json::String(handle.handle)),
                ]),
            )
        }
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
                log::error!("could not persist spawn failure audit event");
            }
            log::error!("spawn failed for id={task_id}");
            reply(stream, 500, error("could_not_spawn_process"))
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
    path: &Path,
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
fn poll_proc(stream: &mut TcpStream, state: &Server, handle: &str) -> Result<(), String> {
    let result = {
        let mut table = state
            .supervisor
            .procs
            .lock()
            .map_err(|_| "process table lock poisoned".to_owned())?;
        prune_procs(&mut table, Instant::now());
        let Some(entry) = table.get_mut(handle) else {
            return reply(stream, 404, error("unknown_proc"));
        };
        update_proc_status_with_handle(entry, handle, &state.supervisor.registry, &state.store);
        log::debug!(
            "poll id={} bin={} subcommand={}",
            entry.id,
            entry.bin,
            entry.subcommand
        );
        proc_json(entry)
    };
    reply(stream, 200, result)
}
fn kill_proc(stream: &mut TcpStream, state: &Server, handle: &str) -> Result<(), String> {
    let result = {
        let mut table = state
            .supervisor
            .procs
            .lock()
            .map_err(|_| "process table lock poisoned".to_owned())?;
        prune_procs(&mut table, Instant::now());
        let Some(entry) = table.get_mut(handle) else {
            return reply(stream, 404, error("unknown_proc"));
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
        log::info!(
            "kill id={} bin={} subcommand={} killed={killed}",
            entry.id,
            entry.bin,
            entry.subcommand
        );
        Json::Object(vec![
            ("id".to_owned(), Json::String(entry.id.clone())),
            ("killed".to_owned(), Json::Bool(killed)),
        ])
    };
    reply(stream, 200, result)
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
fn get(stream: &mut TcpStream, state: &Server, target: &str) -> Result<(), String> {
    let (after, timeout, epoch) = match get_query(target) {
        Ok(query) => query,
        Err(message) => {
            reply(stream, 400, error(&message))?;
            return Ok(());
        }
    };
    let result = state
        .store
        .read(after, &epoch, Duration::from_secs(timeout));
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            reply(stream, 500, error("could_not_read_events"))?;
            return Ok(());
        }
    };
    reply(stream, 200, read_json(result))
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
    use crate::config::{allowlist_file, is_get_allowlist, valid_github_repo};
    use crate::events::relay_event_at;
    use crate::exec;
    use crate::http::{
        MAX_BODY, MAX_HEADER_BLOCK_BYTES, MAX_HEADER_COUNT, denial_response, percent_decode,
    };
    use crate::review_loop;
    use crate::update;
    use std::io::Write;

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
    fn get_allowlist_matches_only_its_exact_arguments() {
        assert!(is_get_allowlist(&["get-allowlist".to_owned()]));
        for invalid in [
            Vec::new(),
            vec!["get-allowlist".to_owned(), "--file".to_owned()],
            vec!["set-allowlist".to_owned()],
        ] {
            assert!(!is_get_allowlist(&invalid));
        }
    }

    #[test]
    fn allowlist_updater_accepts_only_its_exact_arguments() {
        let valid = vec![
            "set-allowlist".to_owned(),
            "--file".to_owned(),
            "/secure/policy.json".to_owned(),
        ];
        assert_eq!(allowlist_file(&valid).unwrap(), "/secure/policy.json");
        for invalid in [
            vec![],
            vec!["set-allowlist".to_owned()],
            vec!["set-allowlist".to_owned(), "--file".to_owned()],
            vec![
                "set-allowlist".to_owned(),
                "--other".to_owned(),
                "/secure/policy.json".to_owned(),
            ],
            vec![
                "set-allowlist".to_owned(),
                "--file".to_owned(),
                "".to_owned(),
            ],
        ] {
            assert!(allowlist_file(&invalid).is_err());
        }
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
            let denial = match policy.verified_path(&request.bin, &request.args) {
                Err(exec::VerifyError::Denied) => denial_json(&request.id),
                other => panic!("expected an opaque denial, got {other:?}"),
            };
            assert_eq!(denial.to_json(), expected);
            assert!(!denial.to_json().contains("configured-binary"));
            let (status, response) = denial_response(&request.id);
            assert_eq!(status, 200);
            assert_eq!(response.to_json(), expected);
        }
    }

    #[test]
    fn oversized_exec_request_is_an_opaque_denial_before_body_allocation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            )
            .unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        assert!(matches!(
            read_request(&mut server),
            Err(ReadRequestError::ExecutionDenied)
        ));
    }

    #[test]
    fn truncated_exec_request_is_an_opaque_denial() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: 1\r\n\r\n"
            )
            .unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        assert!(matches!(
            read_request(&mut server),
            Err(ReadRequestError::ExecutionDenied)
        ));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\n").unwrap();
            for index in 0..=MAX_HEADER_COUNT {
                write!(stream, "X-Flood-{index}: value\r\n").unwrap();
            }
            write!(stream, "\r\n").unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        assert!(matches!(
            read_request(&mut server),
            Err(ReadRequestError::HeadersTooLarge)
        ));
    }

    #[test]
    fn exactly_one_hundred_headers_still_parse() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\n").unwrap();
            for index in 0..MAX_HEADER_COUNT {
                write!(stream, "X-Ok-{index}: value\r\n").unwrap();
            }
            write!(stream, "\r\n").unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        let request = read_request(&mut server).unwrap();
        assert_eq!(request.headers.len(), MAX_HEADER_COUNT);
    }

    #[test]
    fn header_block_over_eight_kib_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\nX-Big: ").unwrap();
            stream
                .write_all(&vec![b'a'; MAX_HEADER_BLOCK_BYTES])
                .unwrap();
            write!(stream, "\r\n\r\n").unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        assert!(matches!(
            read_request(&mut server),
            Err(ReadRequestError::HeadersTooLarge)
        ));
    }

    #[test]
    fn unterminated_header_line_cannot_outgrow_the_budget() {
        // The client never sends a line terminator; the server must stop at
        // the budget instead of buffering the line without bound.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/health HTTP/1.1\r\nX-Big: ").unwrap();
            stream
                .write_all(&vec![b'a'; 2 * MAX_HEADER_BLOCK_BYTES])
                .unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        let started = Instant::now();
        let result = read_request(&mut server);
        assert!(matches!(result, Err(ReadRequestError::HeadersTooLarge)));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "server waited for a terminator instead of enforcing the budget"
        );
        client.join().unwrap();
    }

    #[test]
    fn reply_reports_431_as_request_header_fields_too_large() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            let response = String::from_utf8(response).unwrap();
            assert!(
                response.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"),
                "unexpected status line: {response}"
            );
        });
        let (mut server, _) = listener.accept().unwrap();
        reply(&mut server, 431, error("request header fields too large")).unwrap();
        // Close so the client's read_to_end sees EOF.
        drop(server);
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
            Path::new("/bin/sh"),
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let request = format!(
            "{method} {target} HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
            token,
            body.len()
        );
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (server, _) = listener.accept().unwrap();
        handle_with_services(server, state, || Ok(policy.clone()), gate_report).unwrap();
        client.join().unwrap()
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
    fn worktree_test_base(prefix: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zigzag-{prefix}-{}-{}",
            std::process::id(),
            unique_handle(&HashMap::new()).unwrap()
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    fn worktree_test_roots(base: &Path) -> Vec<PathBuf> {
        let allowed = base.join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        vec![allowed.canonicalize().unwrap()]
    }

    fn worktree_test_repo(base: &Path) -> PathBuf {
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "zigzag-test@example.com"]);
        git(&["config", "user.name", "zigzag-test"]);
        git(&["commit", "--allow-empty", "-m", "init"]);
        repo
    }

    #[test]
    fn worktree_new_path_rejects_traversal_and_outside_roots() {
        let base = worktree_test_base("wt-roots");
        let roots = worktree_test_roots(&base);
        let joined = |parts: &[&str]| {
            let mut path = base.clone();
            for part in parts {
                path.push(part);
            }
            path.to_string_lossy().into_owned()
        };
        // A plain path under the root resolves to the canonical root + leaf.
        assert_eq!(
            resolve_new_worktree_path(&joined(&["allowed", "wt1"]), &roots).unwrap(),
            roots[0].join("wt1")
        );
        // `..` traversal that escapes the root is rejected.
        assert!(
            resolve_new_worktree_path(&joined(&["allowed", "..", "other", "wt"]), &roots).is_err()
        );
        // A sibling directory outside the root is rejected.
        assert!(resolve_new_worktree_path(&joined(&["other", "wt"]), &roots).is_err());
        // Relative paths are rejected outright.
        assert!(resolve_new_worktree_path("allowed/wt", &roots).is_err());
        assert!(resolve_new_worktree_path("", &roots).is_err());
        // A symlink inside the root pointing outside cannot smuggle a path out:
        // the parent canonicalizes outside the root.
        std::os::unix::fs::symlink(base.join("other"), base.join("allowed").join("evil")).unwrap();
        assert!(resolve_new_worktree_path(&joined(&["allowed", "evil", "wt"]), &roots).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_existing_path_requires_existence_under_roots() {
        let base = worktree_test_base("wt-existing");
        let roots = worktree_test_roots(&base);
        let inside = base.join("allowed").join("wt");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(
            resolve_existing_worktree_path(inside.to_str().unwrap(), &roots).unwrap(),
            roots[0].join("wt")
        );
        // Missing path.
        assert!(
            resolve_existing_worktree_path(
                &base.join("allowed").join("nope").to_string_lossy(),
                &roots
            )
            .is_err()
        );
        // Existing but outside the roots.
        assert!(
            resolve_existing_worktree_path(&base.join("other").to_string_lossy(), &roots).is_err()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_branch_validation_rejects_hostile_names() {
        assert!(valid_worktree_branch("feature/my-branch"));
        assert!(valid_worktree_branch("codex/agent-endpoints-01-worktrees"));
        assert!(!valid_worktree_branch(""));
        assert!(!valid_worktree_branch("--help"));
        assert!(!valid_worktree_branch("-b"));
        assert!(!valid_worktree_branch("../escape"));
        assert!(!valid_worktree_branch("a b"));
        assert!(!valid_worktree_branch("a:b"));
        assert!(!valid_worktree_branch("branch.lock"));
        assert!(!valid_worktree_branch("branch@{1}"));
        assert!(!valid_worktree_branch("/leading"));
        assert!(!valid_worktree_branch("trailing/"));
    }

    #[test]
    fn worktree_repo_must_live_under_workspace_root() {
        let base = worktree_test_base("wt-repo");
        let roots = worktree_test_roots(&base);
        let _ = roots;
        let repo_root = base.canonicalize().unwrap();
        let inside = base.join("allowed").join("repo");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(resolve_worktree_repo(inside.to_str().unwrap(), &repo_root).is_ok());
        // Existing but outside the repo root.
        let outside_base = worktree_test_base("wt-repo-outside");
        std::fs::create_dir_all(&outside_base).unwrap();
        assert!(resolve_worktree_repo(outside_base.to_str().unwrap(), &repo_root).is_err());
        let _ = std::fs::remove_dir_all(&outside_base);
        // Missing entirely.
        assert!(
            resolve_worktree_repo(
                &base.join("allowed").join("nope").to_string_lossy(),
                &repo_root
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_create_and_delete_roundtrip() {
        let base = worktree_test_base("wt-e2e");
        let wt_root = base.join("worktrees");
        std::fs::create_dir_all(&wt_root).unwrap();
        let repo = worktree_test_repo(&base);
        let roots = vec![wt_root.canonicalize().unwrap()];
        let repo_root = base.canonicalize().unwrap();
        let plan = |path: &str, branch: &str| {
            worktree_create_plan(path, branch, repo.to_str().unwrap(), &roots, &repo_root)
        };

        // Create on a new branch.
        let wt = wt_root.join("feature-a");
        let wt_canonical = wt_root.canonicalize().unwrap().join("feature-a");
        let response = plan(wt.to_str().unwrap(), "feature-a").unwrap();
        assert_eq!(
            response.object("branch"),
            Some(&Json::String("feature-a".to_owned()))
        );
        assert_eq!(
            response.object("path").and_then(Json::as_str),
            wt_canonical.to_str()
        );
        assert!(wt.join(".git").exists());

        // The same branch cannot back a second worktree.
        let err = plan(wt_root.join("feature-a2").to_str().unwrap(), "feature-a").unwrap_err();
        assert_eq!(
            (err.code, err.message),
            (400, "worktree_branch_already_checked_out")
        );

        // A live agent attached to the worktree blocks deletion.
        let agent_command = format!("codex exec --cd {}", wt.display());
        let agents: Vec<(&str, &str)> = vec![("running", agent_command.as_str())];
        let err = worktree_delete_plan(wt.to_str().unwrap(), &roots, &agents).unwrap_err();
        assert_eq!((err.code, err.message), (409, "worktree_in_use"));
        assert!(wt.exists(), "refused delete must leave the worktree alone");

        // A finished agent does not block.
        let agents: Vec<(&str, &str)> = vec![("succeeded", agent_command.as_str())];
        let response = worktree_delete_plan(wt.to_str().unwrap(), &roots, &agents).unwrap();
        assert_eq!(response.object("removed"), Some(&Json::Bool(true)));
        assert!(!wt.exists());

        // Creating on an existing branch checks it out instead of creating it.
        let output = Command::new("git")
            .args(["branch", "existing"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        let wt2 = wt_root.join("feature-b");
        let response = plan(wt2.to_str().unwrap(), "existing").unwrap();
        assert_eq!(
            response.object("branch"),
            Some(&Json::String("existing".to_owned()))
        );
        assert!(wt2.join(".git").exists());

        // Deleting a path that is gone fails instead of claiming success.
        worktree_delete_plan(wt2.to_str().unwrap(), &roots, &[]).unwrap();
        assert!(worktree_delete_plan(wt2.to_str().unwrap(), &roots, &[]).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_endpoints_reject_outside_roots_over_http() {
        let (state, state_path) = test_server();
        let policy =
            exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#)
                .unwrap();
        // A path no canonical root can contain is rejected with 400, through
        // the full authenticated routing path.
        let response = request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/worktrees",
            r#"{"path":"/definitely-not-a-worktree-root/wt","branch":"b","repo":"/definitely-not-a-worktree-root/repo"}"#,
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        assert!(
            response.ends_with(r#"{"error":"worktree_path_not_found"}"#)
                || response.ends_with(r#"{"error":"worktree_path_outside_allowed_roots"}"#),
            "{response}"
        );
        let response = request_once(
            Arc::clone(&state),
            &policy,
            "DELETE",
            "/v1/worktrees",
            r#"{"path":"/definitely-not-a-worktree-root/wt"}"#,
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        // Malformed bodies are rejected before any git runs.
        let response = request_once(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/worktrees",
            r#"{"path":"/x"}"#,
        );
        assert!(
            response.ends_with(r#"{"error":"invalid_worktree_request"}"#),
            "{response}"
        );
        // The bearer check still guards the new routes.
        let unauthorized = request_once_with_gate_token(
            Arc::clone(&state),
            &policy,
            "POST",
            "/v1/worktrees",
            r#"{"path":"/x","branch":"b","repo":"/y"}"#,
            "wrong-token",
            |_, _, _, _, _| panic!("unauthorized worktree request reached the backend"),
        );
        assert!(
            unauthorized.starts_with("HTTP/1.1 401 Unauthorized"),
            "{unauthorized}"
        );
        drop(state);
        let _ = std::fs::remove_file(state_path);
    }
}
