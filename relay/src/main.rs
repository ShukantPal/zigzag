use relay_core::{
    AgentRecord, AgentRegistry, Json, ReadResult, Store, parse_json, read_secret_file,
};
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod exec;

const MAX_BODY: usize = 64 * 1024;
const MAX_FINISHED_PROCS: usize = 128;
const FINISHED_PROC_RETENTION: Duration = Duration::from_secs(60 * 60);
const COMPAT_OUTPUT_CAP: usize = 2 * 1024 * 1024;
#[cfg(any(target_os = "macos", test))]
const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
#[cfg(any(target_os = "macos", test))]
const SESSION_IS_REMOTE: u32 = 0x1000;

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
}
struct Server {
    secret: String,
    control_secret: Option<String>,
    store: Store,
    supervisor: Supervisor,
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

impl Drop for ProcEntry {
    fn drop(&mut self) {
        if self.finished_at.is_none() {
            let _ = kill_process_group(self.process_group);
        }
    }
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

enum ReadRequestError {
    Message(String),
    ExecutionDenied,
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
    let config = server_config(arguments)?;
    let secret = read_secret_file(&config.secret_file)?;
    let control_secret = config
        .control_secret_file
        .as_deref()
        .map(read_secret_file)
        .transpose()?;
    let state = Arc::new(Server {
        secret,
        control_secret,
        store: Store::open(config.state_file, config.max_events)?,
        supervisor: Supervisor {
            registry: Arc::new(AgentRegistry::open(config.agent_registry_file)?),
            procs: Mutex::new(HashMap::new()),
        },
    });
    for agent in state.supervisor.registry.recover(process_group_running)? {
        let _ = state.store.add(agent_event("agent_recovered", &agent));
    }
    start_reaper(Arc::clone(&state));
    if !config.github_watch_repos.is_empty() {
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
    for address in addresses {
        let listener = TcpListener::bind(address)
            .map_err(|error| format!("could not bind {address}: {error}"))?;
        let state = Arc::clone(&state);
        println!("zigzag listening on http://{address}");
        thread::spawn(move || serve(listener, state));
    }
    loop {
        thread::park();
    }
}

fn server_config(arguments: Vec<String>) -> Result<Config, String> {
    let mut secret_file = env::var_os("ZIGZAG_SECRET_FILE").map(PathBuf::from);
    let mut state_file = env::var_os("ZIGZAG_STATE_FILE").map(PathBuf::from);
    let mut control_secret_file = env::var_os("ZIGZAG_CONTROL_SECRET_FILE").map(PathBuf::from);
    let mut port = 8765;
    let mut tailscale_ip = None;
    let mut max_events = 1000;
    let mut github_watch_repos = Vec::new();
    let mut github_watch_interval = Duration::from_secs(30);
    let mut values = arguments.into_iter();
    while let Some(argument) = values.next() {
        let value = |values: &mut std::vec::IntoIter<String>, name: &str| {
            values
                .next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match argument.as_str() {
            "--secret-file" => secret_file = Some(PathBuf::from(value(&mut values, "--secret-file")?)),
            "--control-secret-file" => control_secret_file = Some(PathBuf::from(value(&mut values, "--control-secret-file")?)),
            "--state-file" => state_file = Some(PathBuf::from(value(&mut values, "--state-file")?)),
            "--port" => port = value(&mut values, "--port")?.parse().map_err(|_| "--port must be a valid u16".to_owned())?,
            "--tailscale-ip" => {
                let address = value(&mut values, "--tailscale-ip")?
                    .parse()
                    .map_err(|_| "--tailscale-ip must be an IP address".to_owned())?;
                if !is_tailscale_ipv4(address) {
                    return Err("--tailscale-ip must be a Tailscale IPv4 address".to_owned());
                }
                tailscale_ip = Some(address);
            }
            "--max-events" => max_events = value(&mut values, "--max-events")?.parse().map_err(|_| "--max-events must be a positive integer".to_owned())?,
            "--watch-repo" => {
                let repo = value(&mut values, "--watch-repo")?;
                if !valid_github_repo(&repo) {
                    return Err("--watch-repo must be an OWNER/REPO GitHub name".to_owned());
                }
                if !github_watch_repos.contains(&repo) {
                    github_watch_repos.push(repo);
                }
            }
            "--watch-interval" => {
                let seconds = value(&mut values, "--watch-interval")?
                    .parse::<u64>()
                    .map_err(|_| "--watch-interval must be an integer".to_owned())?;
                if !(30..=3600).contains(&seconds) {
                    return Err("--watch-interval must be between 30 and 3600 seconds".to_owned());
                }
                github_watch_interval = Duration::from_secs(seconds);
            }
            "--help" | "-h" => return Err("usage: zigzag --secret-file PATH --state-file PATH [--control-secret-file PATH] [--port 8765] [--max-events 1000] [--watch-repo OWNER/REPO] [--watch-interval 30]".to_owned()),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let secret_file =
        secret_file.ok_or_else(|| "--secret-file or ZIGZAG_SECRET_FILE is required".to_owned())?;
    let state_file =
        state_file.ok_or_else(|| "--state-file or ZIGZAG_STATE_FILE is required".to_owned())?;
    if max_events == 0 {
        return Err("--max-events must be greater than zero".to_owned());
    }
    let agent_registry_file = state_file.with_extension("agents.json");
    Ok(Config {
        secret_file,
        control_secret_file,
        state_file,
        agent_registry_file,
        port,
        tailscale_ip,
        max_events,
        github_watch_repos,
        github_watch_interval,
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

fn github_watch_loop(state: Arc<Server>, repos: Vec<String>, interval: Duration) {
    loop {
        for repo in &repos {
            match github_open_pull_requests(repo) {
                Ok(pull_requests) => {
                    for (number, url) in pull_requests {
                        let id = format!("github-pr-opened:{repo}:{number}");
                        let payload = Json::Object(vec![
                            ("id".to_owned(), Json::String(id.clone())),
                            (
                                "kind".to_owned(),
                                Json::String("github_pr_opened".to_owned()),
                            ),
                            ("repository".to_owned(), Json::String(repo.clone())),
                            ("pull_request".to_owned(), Json::number(number)),
                            ("url".to_owned(), Json::String(url)),
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

fn github_open_pull_requests(repo: &str) -> Result<Vec<(u64, String)>, String> {
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

fn parse_github_open_pull_requests(output: &str) -> Result<Vec<(u64, String)>, String> {
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
            let url = pull_request
                .object("html_url")
                .or_else(|| pull_request.object("url"))
                .and_then(Json::as_str)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| "GitHub PR discovery result is missing a PR URL".to_owned())?;
            Ok((number, url.to_owned()))
        })
        .collect()
}

fn run_config(arguments: &[String]) -> Result<(), String> {
    require_gui_login_session()?;
    if is_get_allowlist(arguments) {
        let policy = exec::load_policy()?;
        println!("{}", policy.canonical_json());
        return Ok(());
    }
    let file = allowlist_file(arguments)?;
    let contents = std::fs::read_to_string(file)
        .map_err(|error| format!("could not read allowlist file {file}: {error}"))?;
    let policy = exec::Policy::parse(&contents)?;
    exec::store_policy(&policy)?;
    println!("{}", policy.canonical_json());
    Ok(())
}

fn is_get_allowlist(arguments: &[String]) -> bool {
    arguments.len() == 1 && arguments[0] == "get-allowlist"
}

fn allowlist_file(arguments: &[String]) -> Result<&str, String> {
    let [command, flag, file] = arguments else {
        return Err("usage: zigzag config (get-allowlist | set-allowlist --file PATH)".to_owned());
    };
    if command != "set-allowlist" || flag != "--file" || file.is_empty() {
        return Err("usage: zigzag config (get-allowlist | set-allowlist --file PATH)".to_owned());
    }
    Ok(file)
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
        Err("set-allowlist must run from Shukant's local macOS GUI login session".to_owned())
    }
}

#[cfg(not(target_os = "macos"))]
fn require_gui_login_session() -> Result<(), String> {
    Err("set-allowlist must run from Shukant's local macOS GUI login session".to_owned())
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

fn serve(listener: TcpListener, state: Arc<Server>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _ = handle(stream, state);
                });
            }
            Err(error) => eprintln!("zigzag accept error: {error}"),
        }
    }
}

fn handle(stream: TcpStream, state: Arc<Server>) -> Result<(), String> {
    handle_with_policy(stream, state, || {
        require_gui_login_session().and_then(|_| exec::load_policy())
    })
}

fn handle_with_policy<F>(
    mut stream: TcpStream,
    state: Arc<Server>,
    load_policy: F,
) -> Result<(), String>
where
    F: Fn() -> Result<exec::Policy, String>,
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
        reply(&mut stream, 401, error("unauthorized"))?;
        return Ok(());
    }
    match (request.method.as_str(), request_path) {
        ("POST", "/v1/events") => post(&mut stream, &state, request.body),
        ("GET", "/v1/events") => get(&mut stream, &state, &request.target),
        ("POST", "/v1/exec") => exec_request(&mut stream, request.body),
        ("POST", "/v1/spawn") => spawn_request(&mut stream, &state, request.body, load_policy()),
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

fn exec_request(stream: &mut TcpStream, body: Vec<u8>) -> Result<(), String> {
    let request = match parse_exec_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(stream, 200, denial),
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
            return reply(stream, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy_path_or_denial(&policy, &request) {
        Ok(path) => path,
        Err(denial) => return reply(stream, 200, denial),
    };
    let result = exec::run(path, request);
    reply(stream, 200, result.to_json())
}

fn spawn_request(
    stream: &mut TcpStream,
    state: &Server,
    body: Vec<u8>,
    policy: Result<exec::Policy, String>,
) -> Result<(), String> {
    let request = match parse_exec_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(stream, 200, denial),
    };
    eprintln!(
        "spawn id={} bin={} subcommand={}",
        request.id,
        request.bin,
        request.args.first().map(String::as_str).unwrap_or("")
    );
    let policy = match policy {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("spawn policy read failed: {message}");
            return reply(stream, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy_path_or_denial(&policy, &request) {
        Ok(path) => path,
        Err(denial) => return reply(stream, 200, denial),
    };
    match spawn_proc(&state.supervisor, path, request) {
        Ok(handle) => reply(
            stream,
            200,
            Json::Object(vec![
                ("id".to_owned(), Json::String(handle.id)),
                ("proc".to_owned(), Json::String(handle.handle)),
            ]),
        ),
        Err(_) => {
            eprintln!("spawn failed");
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
    path: &str,
    request: exec::ExecRequest,
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
        leader_pid: process_group,
        process_group,
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
        redacted: false,
        stdout_next: 0,
        stderr_next: 0,
        stdout_dropped_before: 0,
        stderr_dropped_before: 0,
    };
    // The registry transition commits before this spawn can be acknowledged.
    if let Err(error) = supervisor.registry.register(record) {
        let _ = kill_process_group(process_group);
        return Err(error);
    }
    drain_to_capture(
        child_stdout,
        Arc::clone(&stdout),
        Arc::clone(&supervisor.registry),
        handle.clone(),
        "stdout",
    );
    drain_to_capture(
        child_stderr,
        Arc::clone(&stderr),
        Arc::clone(&supervisor.registry),
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
        eprintln!(
            "poll id={} bin={} subcommand={}",
            entry.id, entry.bin, entry.subcommand
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

fn process_group_running(process_group: i32) -> bool {
    unsafe { libc::kill(-process_group, 0) == 0 }
}

fn unique_handle(entries: &HashMap<String, ProcEntry>) -> Result<String, String> {
    loop {
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut source| source.read_exact(&mut bytes))
            .map_err(|error| format!("could not generate process handle: {error}"))?;
        let handle = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
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

fn agent_event(kind: &str, agent: &AgentRecord) -> Json {
    Json::Object(vec![
        (
            "id".to_owned(),
            Json::String(format!("agent:{}:{}", agent.id, kind)),
        ),
        ("kind".to_owned(), Json::String(kind.to_owned())),
        ("agent_id".to_owned(), Json::String(agent.id.clone())),
        ("task_id".to_owned(), Json::String(agent.task_id.clone())),
        ("state".to_owned(), Json::String(agent.state.clone())),
    ])
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
    if !process_group_running(entry.process_group) && output_is_complete(entry) {
        entry.finished_at = Some(Instant::now());
        let state = match entry.exit_code {
            Some(0) => "succeeded",
            Some(_) => "failed",
            None => "unexpected_exit",
        };
        if let Ok(Some(agent)) = registry.transition(handle, state, entry.exit_code) {
            let _ = store.add(agent_event("agent_completed", &agent));
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

fn denied(stream: &mut TcpStream, id: &str) -> Result<(), String> {
    let (status, body) = denial_response(id);
    reply(stream, status, body)
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

fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .map_err(|_| ReadRequestError::Message("could not read request line".to_owned()))?;
    let mut parts = first.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?
        .to_owned();
    if parts.next().is_none() {
        return Err(ReadRequestError::Message(
            "malformed request line".to_owned(),
        ));
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|_| ReadRequestError::Message("could not read headers".to_owned()))?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(|| ReadRequestError::Message("malformed header".to_owned()))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .map_err(|_| ReadRequestError::Message("invalid content length".to_owned()))
    })?;
    if length > MAX_BODY {
        if method == "POST"
            && matches!(
                target.split('?').next(),
                Some("/v1/exec") | Some("/v1/spawn")
            )
        {
            return Err(ReadRequestError::ExecutionDenied);
        }
        return Err(ReadRequestError::Message(
            "request body too large".to_owned(),
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|_| {
        if method == "POST"
            && matches!(
                target.split('?').next(),
                Some("/v1/exec") | Some("/v1/spawn")
            )
        {
            ReadRequestError::ExecutionDenied
        } else {
            ReadRequestError::Message("short request body".to_owned())
        }
    })?;
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

fn query(target: &str) -> Result<HashMap<String, String>, String> {
    let Some((_, raw)) = target.split_once('?') else {
        return Ok(HashMap::new());
    };
    raw.split('&')
        .filter(|value| !value.is_empty())
        .map(|item| {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            Ok((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}
fn percent_decode(input: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut chars = input.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let high = chars
                .next()
                .ok_or_else(|| "invalid URL encoding".to_owned())?;
            let low = chars
                .next()
                .ok_or_else(|| "invalid URL encoding".to_owned())?;
            bytes.push((hex(high)? << 4) | hex(low)?);
        } else if byte == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| "invalid URL encoding".to_owned())
}
fn hex(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("invalid URL encoding".to_owned()),
    }
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
fn reply(stream: &mut TcpStream, code: u16, value: Json) -> Result<(), String> {
    let body = value.to_json();
    let reason = match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    stream.write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn github_watch_repo_validation_rejects_unscoped_or_malformed_names() {
        assert!(valid_github_repo("leveled-inc/leveled"));
        assert!(valid_github_repo("owner.name/repo_name-2"));
        assert!(!valid_github_repo("leveled"));
        assert!(!valid_github_repo("owner/repo/extra"));
        assert!(!valid_github_repo("owner/repo space"));
    }

    #[test]
    fn github_watch_configuration_is_opt_in_and_rate_limited() {
        let base_arguments = || {
            vec![
                "--secret-file".to_owned(),
                "/token".to_owned(),
                "--state-file".to_owned(),
                "/state".to_owned(),
            ]
        };
        let config = server_config(base_arguments()).unwrap();
        assert!(config.github_watch_repos.is_empty());
        assert_eq!(config.github_watch_interval, Duration::from_secs(30));

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

        let mut arguments = base_arguments();
        arguments.extend(["--watch-interval".to_owned(), "29".to_owned()]);
        assert!(server_config(arguments).is_err());
    }

    #[test]
    fn github_pr_scan_requires_numbers_and_urls() {
        assert_eq!(
            parse_github_open_pull_requests(
                r#"[[{"number":42,"html_url":"https://github.com/leveled-inc/leveled/pull/42"}]]"#,
            )
            .unwrap(),
            vec![(
                42,
                "https://github.com/leveled-inc/leveled/pull/42".to_owned()
            )]
        );
        assert!(parse_github_open_pull_requests(r#"{"number":42}"#).is_err());
        assert!(parse_github_open_pull_requests(r#"[{"number":42,"url":""}]"#).is_err());
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
    fn captured_output_stops_at_the_exec_output_cap() {
        let mut output = CappedOutput::default();
        output.append(&vec![b'x'; COMPAT_OUTPUT_CAP]);
        output.append(b"extra");
        let (captured, truncated) = output.snapshot();
        assert_eq!(captured.len(), COMPAT_OUTPUT_CAP);
        assert!(truncated);
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
        let handle = spawn_proc(&supervisor, "/bin/sh", request).unwrap().handle;
        let mut entries = supervisor.procs.lock().unwrap();
        let entry = entries.get_mut(&handle).unwrap();
        let _ = entry.child.wait();
        entry.finished_at = Some(Instant::now() - FINISHED_PROC_RETENTION - Duration::from_secs(1));
        prune_procs(&mut entries, Instant::now());
        assert!(entries.is_empty());
        let _ = std::fs::remove_file(registry_path);
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

        // The shell leader exits immediately, leaving the sleep descendant in
        // the dedicated process group. It must still be visible and killable.
        let handle = spawn_for_test(&state, &policy, "sleep", "sleep 60 & exit");
        let running = response_json(request_once(
            Arc::clone(&state),
            &policy,
            "GET",
            &format!("/v1/proc/{handle}"),
            "",
        ));
        assert_eq!(running.object("running"), Some(&Json::Bool(true)));
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
                store: Store::open(&path, 1).unwrap(),
                supervisor: Supervisor {
                    registry: Arc::new(AgentRegistry::open(path.with_extension("agents")).unwrap()),
                    procs: Mutex::new(HashMap::new()),
                },
            }),
            path,
        )
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let request = format!(
            "{method} {target} HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
            "x".repeat(32),
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
        handle_with_policy(server, state, || Ok(policy.clone())).unwrap();
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
        for _ in 0..100 {
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
