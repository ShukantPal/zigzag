//! Native GitHub PR-comment routing.
//!
//! The router deliberately reads GitHub through the same read-only `gh`
//! policy as the PR watchdog. It only launches Codex after persisting a
//! GraphQL-node-id event and runs live by default. The legacy
//! `--comment-router-live` flag remains accepted for compatibility.

use crate::config::valid_github_repo;
use crate::events::new_execution_id;
use crate::exec;
use crate::proc::{AgentSpawnDetails, kill_process_group, process_group_running, spawn_proc};
use crate::server::{Server, Supervisor};
use crate::session::require_gui_login_session;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use zz::{Json, parse_json};

const ROUTED_OWNER: &str = "ShukantPal";
const BOT_MARKER: &str = "> 🤖";
const BURST_INTERVAL: Duration = Duration::from_secs(30);
const PROMPT: &str = include_str!("../prompts/comment_address.md");

#[derive(Clone)]
pub(crate) struct Config {
    state_file: PathBuf,
    command_entries: Vec<WatchedPr>,
    shadow: bool,
    quiet_interval: Duration,
    burst_window: Duration,
}

impl Config {
    pub(crate) fn new(
        state_file: PathBuf,
        command_entries: Vec<String>,
        shadow: bool,
        quiet_interval: Duration,
        burst_window: Duration,
    ) -> Result<Self, String> {
        let command_entries = command_entries
            .iter()
            .map(|entry| parse_watch_entry(entry))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            state_file,
            command_entries,
            shadow,
            quiet_interval,
            burst_window,
        })
    }
}

/// A deliberately shared gate for *session* operations, rather than a lock
/// local to this watcher.  A session remains claimed for the lifetime of the
/// detached resume process; a later watcher can use the same gate.
pub(crate) struct SessionGate {
    path: Option<PathBuf>,
    inner: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    active: HashMap<String, String>,
    dispatched: HashSet<String>,
}

impl SessionGate {
    pub(crate) fn open(path: PathBuf) -> Result<Self, String> {
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => parse_gate_state(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GateState::default(),
            Err(_) => return Err("could not read comment-router ledger".to_owned()),
        };
        Ok(Self {
            path: Some(path),
            inner: Mutex::new(state),
        })
    }

    pub(crate) fn in_memory() -> Self {
        Self {
            path: None,
            inner: Mutex::new(GateState::default()),
        }
    }

    fn claim(&self, session: &str) -> bool {
        self.inner
            .lock()
            .map(|mut state| {
                state
                    .active
                    .insert(session.to_owned(), String::new())
                    .is_none()
            })
            .unwrap_or(false)
    }

    fn is_dispatched(&self, id: &str) -> bool {
        self.inner
            .lock()
            .is_ok_and(|state| state.dispatched.contains(id))
    }

    fn bind(&self, session: &str, handle: &str) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "comment-router ledger lock poisoned".to_owned())?;
        state.active.insert(session.to_owned(), handle.to_owned());
        self.save(&state)
    }

    fn mark_dispatched(&self, ids: impl IntoIterator<Item = String>) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "comment-router ledger lock poisoned".to_owned())?;
        state.dispatched.extend(ids);
        self.save(&state)
    }

    fn release(&self, session: &str) {
        if let Ok(mut state) = self.inner.lock() {
            state.active.remove(session);
            let _ = self.save(&state);
        }
    }

    fn recovered_claims(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .map(|state| {
                state
                    .active
                    .iter()
                    .filter(|(_, handle)| !handle.is_empty())
                    .map(|(session, handle)| (session.clone(), handle.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save(&self, state: &GateState) -> Result<(), String> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|_| "could not create comment-router ledger".to_owned())?;
        let temporary = parent.join(format!(".comment-router-{}.tmp", std::process::id()));
        let value = Json::Object(vec![
            (
                "active".to_owned(),
                Json::Object(
                    state
                        .active
                        .iter()
                        .map(|(k, v)| (k.clone(), Json::String(v.clone())))
                        .collect(),
                ),
            ),
            (
                "dispatched".to_owned(),
                Json::Array(state.dispatched.iter().cloned().map(Json::String).collect()),
            ),
        ])
        .to_json();
        std::fs::write(&temporary, value)
            .map_err(|_| "could not write comment-router ledger".to_owned())?;
        std::fs::rename(temporary, path)
            .map_err(|_| "could not replace comment-router ledger".to_owned())
    }
}

static SESSION_GATE: OnceLock<Arc<SessionGate>> = OnceLock::new();

pub(crate) fn configure_session_gate(path: PathBuf) -> Result<(), String> {
    let gate = Arc::new(SessionGate::open(path)?);
    SESSION_GATE
        .set(gate)
        .map_err(|_| "comment-router ledger was configured more than once".to_owned())
}

fn session_gate() -> Arc<SessionGate> {
    Arc::clone(SESSION_GATE.get_or_init(|| Arc::new(SessionGate::in_memory())))
}

fn parse_gate_state(text: &str) -> Result<GateState, String> {
    let value = parse_json(text).map_err(|_| "invalid comment-router ledger".to_owned())?;
    let active = match value.object("active") {
        Some(Json::Object(entries)) => entries
            .iter()
            .map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
            .collect::<Option<HashMap<_, _>>>(),
        _ => None,
    }
    .ok_or_else(|| "invalid comment-router ledger".to_owned())?;
    let dispatched = match value.object("dispatched") {
        Some(Json::Array(ids)) => ids.iter().map(Json::as_str).collect::<Option<HashSet<_>>>(),
        _ => None,
    }
    .ok_or_else(|| "invalid comment-router ledger".to_owned())?
    .into_iter()
    .map(str::to_owned)
    .collect();
    Ok(GateState { active, dispatched })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchedPr {
    repository: String,
    number: u64,
    session_id: String,
}

#[derive(Clone, Debug)]
struct Comment {
    surface: &'static str,
    node_id: String,
    author: String,
    body: String,
    url: String,
    created_at: String,
}

impl Comment {
    /// Phase 1 intentionally routes only Shukant's feedback. Other review
    /// authors are out of scope; marker-bearing worker replies are excluded.
    fn is_owner_feedback(&self) -> bool {
        self.author.eq_ignore_ascii_case(ROUTED_OWNER)
            && !self
                .body
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with(BOT_MARKER)
    }

    fn event_id(&self) -> String {
        // `node_id` is canonical across REST and GraphQL. Never build the
        // event identity from REST's numeric `id`: the two forms are aliases
        // for one GitHub object and would otherwise dispatch twice.
        format!("github-pr-comment:{}", self.node_id)
    }
}

pub(crate) fn watch_loop(state: Arc<Server>, config: Config) {
    let mut burst_until = Instant::now();
    loop {
        let watched = match load_watched_prs(&config) {
            Ok(watched) => watched,
            Err(error) => {
                eprintln!("GitHub comment-router state failed: {error}");
                Vec::new()
            }
        };
        let interval = watch_cycle(
            watched,
            Instant::now(),
            &mut burst_until,
            config.burst_window,
            config.quiet_interval,
            |watched_pr| scan_pr(&state, watched_pr, config.shadow),
        );
        thread::sleep(interval);
    }
}

fn watch_cycle(
    watched: Vec<WatchedPr>,
    now: Instant,
    burst_until: &mut Instant,
    burst_window: Duration,
    quiet_interval: Duration,
    mut scan: impl FnMut(&WatchedPr) -> Result<bool, String>,
) -> Duration {
    for watched_pr in watched {
        match scan(&watched_pr) {
            Ok(true) => *burst_until = now + burst_window,
            Ok(false) => {}
            Err(error) => eprintln!(
                "GitHub comment watch for {}#{} failed: {error}",
                watched_pr.repository, watched_pr.number
            ),
        }
    }
    poll_interval(now, *burst_until, quiet_interval)
}

fn poll_interval(now: Instant, burst_until: Instant, quiet_interval: Duration) -> Duration {
    if now < burst_until {
        BURST_INTERVAL
    } else {
        quiet_interval
    }
}

fn scan_pr(state: &Arc<Server>, watched: &WatchedPr, shadow: bool) -> Result<bool, String> {
    let comments = github_comments(&watched.repository, watched.number)?;
    let gate = session_gate();
    scan_comments(
        watched,
        &comments,
        shadow,
        &gate,
        ScanHooks {
            persist: |event| {
                state
                    .store
                    .add(event)
                    .map(|_| ())
                    .map_err(|error| format!("could not persist comment event: {error}"))
            },
            launch: |candidates: &[Comment]| {
                launch_comment_resume(state, watched, &comments, candidates)
            },
            on_started: |handle: &str| {
                release_when_finished(
                    Arc::clone(&gate),
                    Arc::clone(&state.supervisor.registry),
                    watched.session_id.clone(),
                    handle.to_owned(),
                );
            },
            on_bind_failure: |handle: &str| terminate_spawned(&state.supervisor, handle),
        },
    )
}

fn launch_comment_resume(
    state: &Arc<Server>,
    watched: &WatchedPr,
    comments: &[Comment],
    candidates: &[Comment],
) -> Result<String, String> {
    let prompt = comment_prompt(watched, comments);
    let request = exec::ExecRequest {
        id: format!("github-comment-{}", candidates[0].node_id),
        bin: "codex".to_owned(),
        args: vec![
            "exec".to_owned(),
            "--json".to_owned(),
            "--approve-for-me".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "resume".to_owned(),
            watched.session_id.clone(),
            prompt,
        ],
    };
    let policy = require_gui_login_session().and_then(|_| exec::load_policy())?;
    let path = policy
        .verified_path(&request.bin, &request.args)
        .map_err(|error| {
            format!("the execution policy does not allow codex comment resumes: {error}")
        });
    let path = path?;
    let execution_id = new_execution_id()?;
    spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        &path,
        request,
        execution_id,
        AgentSpawnDetails::default(),
    )
    .map(|spawned| spawned.handle)
    .map_err(|error| format!("could not spawn comment resume: {error}"))
}

struct ScanHooks<P, L, S, B> {
    persist: P,
    launch: L,
    on_started: S,
    on_bind_failure: B,
}

fn scan_comments<P, L, S, B>(
    watched: &WatchedPr,
    comments: &[Comment],
    shadow: bool,
    gate: &SessionGate,
    hooks: ScanHooks<P, L, S, B>,
) -> Result<bool, String>
where
    P: FnMut(Json) -> Result<(), String>,
    L: for<'a> FnOnce(&'a [Comment]) -> Result<String, String>,
    S: for<'a> FnOnce(&'a str),
    B: for<'a> FnOnce(&'a str),
{
    let ScanHooks {
        mut persist,
        launch,
        on_started,
        on_bind_failure,
    } = hooks;
    let candidates = routable_comments(comments, gate);
    if shadow {
        return shadow_dispatch(gate, watched, &candidates, persist);
    }
    if candidates.is_empty() || !gate.claim(&watched.session_id) {
        return Ok(false);
    }
    // The event must be durable before launching; the dispatch ledger is
    // updated only after a successful spawn, so pre-launch failures retry.
    for comment in &candidates {
        if let Err(error) = persist(comment_event(watched, comment)) {
            gate.release(&watched.session_id);
            return Err(error);
        }
    }
    let handle = match launch(&candidates) {
        Ok(handle) => handle,
        Err(error) => {
            gate.release(&watched.session_id);
            return Err(error);
        }
    };
    on_started(&handle);
    if let Err(error) = gate.bind(&watched.session_id, &handle) {
        on_bind_failure(&handle);
        gate.release(&watched.session_id);
        return Err(error);
    }
    if let Err(error) = gate.mark_dispatched(candidates.iter().map(Comment::event_id)) {
        // The completion monitor releases the claim; retrying after it exits
        // is safer than stranding this session forever.
        eprintln!("could not mark GitHub comment dispatch complete: {error}");
    }
    Ok(true)
}

fn routable_comments(comments: &[Comment], gate: &SessionGate) -> Vec<Comment> {
    comments
        .iter()
        .filter(|comment| comment.is_owner_feedback())
        .filter(|comment| !gate.is_dispatched(&comment.event_id()))
        .cloned()
        .collect()
}

fn shadow_dispatch(
    gate: &SessionGate,
    watched: &WatchedPr,
    candidates: &[Comment],
    mut persist: impl FnMut(Json) -> Result<(), String>,
) -> Result<bool, String> {
    if candidates.is_empty() || !gate.claim(&watched.session_id) {
        return Ok(false);
    }
    let result = (|| {
        for comment in candidates {
            persist(comment_event(watched, comment))?;
        }
        gate.mark_dispatched(candidates.iter().map(Comment::event_id))
    })();
    if let Err(error) = result {
        gate.release(&watched.session_id);
        return Err(error);
    }
    eprintln!(
        "shadow: would resume a session for {}#{} on {} new GitHub comment(s)",
        watched.repository,
        watched.number,
        candidates.len()
    );
    gate.release(&watched.session_id);
    Ok(true)
}

fn comment_event(watched: &WatchedPr, comment: &Comment) -> Json {
    Json::Object(vec![
        ("id".to_owned(), Json::String(comment.event_id())),
        (
            "kind".to_owned(),
            Json::String("github_pr_comment".to_owned()),
        ),
        (
            "repository".to_owned(),
            Json::String(watched.repository.clone()),
        ),
        ("pull_request".to_owned(), Json::number(watched.number)),
        (
            "session_id".to_owned(),
            Json::String(watched.session_id.clone()),
        ),
        ("node_id".to_owned(), Json::String(comment.node_id.clone())),
        (
            "surface".to_owned(),
            Json::String(comment.surface.to_owned()),
        ),
    ])
}

fn terminate_spawned(supervisor: &Supervisor, handle: &str) {
    if let Ok(entries) = supervisor.procs.lock()
        && let Some(entry) = entries.get(handle)
    {
        let _ = kill_process_group(entry.process_group);
    }
}

fn release_when_finished(
    gate: Arc<SessionGate>,
    registry: Arc<zz::AgentRegistry>,
    session_id: String,
    handle: String,
) {
    thread::spawn(move || {
        loop {
            let done = registry
                .get(&handle)
                .map(|agent| !process_group_running(agent.process_group))
                .unwrap_or(true);
            if done {
                gate.release(&session_id);
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

pub(crate) fn recover_session_claims(state: Arc<Server>) {
    let gate = session_gate();
    for (session_id, handle) in gate.recovered_claims() {
        let running = state
            .supervisor
            .registry
            .get(&handle)
            .is_some_and(|agent| process_group_running(agent.process_group));
        if running {
            release_when_finished(
                Arc::clone(&gate),
                Arc::clone(&state.supervisor.registry),
                session_id,
                handle,
            );
        } else {
            gate.release(&session_id);
        }
    }
}

fn github_comments(repo: &str, number: u64) -> Result<Vec<Comment>, String> {
    let mut comments = Vec::new();
    for (surface, endpoint) in [
        ("issue", format!("repos/{repo}/issues/{number}/comments")),
        (
            "review_comment",
            format!("repos/{repo}/pulls/{number}/comments"),
        ),
        ("review", format!("repos/{repo}/pulls/{number}/reviews")),
    ] {
        comments.extend(parse_comments(surface, &github_get(&endpoint)?)?);
    }
    comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    Ok(comments)
}

fn github_get(endpoint: &str) -> Result<String, String> {
    let policy = require_gui_login_session().and_then(|_| exec::load_policy())?;
    let request = exec::ExecRequest {
        id: format!("github-comment-scan-{}", endpoint.replace('/', "-")),
        bin: "gh".to_owned(),
        args: vec![
            "api".to_owned(),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
            endpoint.to_owned(),
        ],
    };
    let path = policy
        .verified_path(&request.bin, &request.args)
        .map_err(|error| format!("the gh policy does not allow the comment scan: {error}"))?;
    let result = exec::run(&path, request);
    if result.timed_out || result.truncated || result.exit_code != Some(0) {
        return Err("GitHub comment scan did not complete successfully".to_owned());
    }
    Ok(result.stdout)
}

fn parse_comments(surface: &'static str, output: &str) -> Result<Vec<Comment>, String> {
    let value = parse_json(output)
        .map_err(|_| "GitHub comment scan did not return the expected JSON".to_owned())?;
    let Json::Array(pages) = value else {
        return Err("GitHub comment scan did not return an array".to_owned());
    };
    let values: Vec<_> = pages
        .iter()
        .flat_map(|page| match page {
            Json::Array(items) => items.iter().collect(),
            item => vec![item],
        })
        .collect();
    values
        .into_iter()
        .filter_map(|item| parse_comment(surface, item))
        .collect::<Result<Vec<_>, _>>()
}

fn parse_comment(surface: &'static str, value: &Json) -> Option<Result<Comment, String>> {
    let body = value.object("body")?.as_str()?.to_owned();
    let author = value.object("user")?.object("login")?.as_str()?.to_owned();
    let node_id = match canonical_node_id(
        value.object("id").and_then(Json::as_u64),
        value.object("node_id").and_then(Json::as_str),
    ) {
        Ok(node_id) => node_id,
        Err(error) => return Some(Err(error)),
    };
    Some(Ok(Comment {
        surface,
        node_id,
        author,
        body,
        url: value
            .object("html_url")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: value
            .object("created_at")
            .or_else(|| value.object("submitted_at"))
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
    }))
}

/// GitHub REST responses contain both a numeric `id` and the GraphQL
/// `node_id`. The same object can be returned through a GraphQL path later, so
/// deliberately discard the numeric alias and retain the one shared identity.
fn canonical_node_id(rest_id: Option<u64>, node_id: Option<&str>) -> Result<String, String> {
    match node_id.filter(|id| !id.is_empty()) {
        Some(node_id) => Ok(node_id.to_owned()),
        None if rest_id.is_some() => {
            Err("GitHub REST comment is missing its GraphQL node_id".to_owned())
        }
        None => Err("GitHub comment is missing its GraphQL node_id".to_owned()),
    }
}

fn comment_prompt(watched: &WatchedPr, comments: &[Comment]) -> String {
    let mut context = String::new();
    for comment in comments {
        context.push_str(&format!(
            "\n[{} by {}] {}\n{}\n{}\n",
            comment.surface, comment.author, comment.url, comment.created_at, comment.body
        ));
    }
    // The relay accepts at most 8 KiB of argv text. Preserve UTF-8 boundaries
    // when a very long thread must be shortened.
    let context = truncate_utf8_tail(&context, 5_500);
    PROMPT
        .replace("{{repository}}", &watched.repository)
        .replace("{{pull_request}}", &watched.number.to_string())
        .replace("{{comment_thread}}", context)
}

fn truncate_utf8_tail(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut start = value.len() - max;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

fn load_watched_prs(config: &Config) -> Result<Vec<WatchedPr>, String> {
    let mut watched: HashMap<(String, u64), WatchedPr> = HashMap::new();
    for entry in &config.command_entries {
        let entry = entry.clone();
        watched.insert((entry.repository.clone(), entry.number), entry);
    }
    match std::fs::read_to_string(&config.state_file) {
        Ok(contents) => {
            for entry in parse_state(&contents)? {
                watched.insert((entry.repository.clone(), entry.number), entry);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("could not read comment-router state file".to_owned()),
    }
    Ok(watched.into_values().collect())
}

fn parse_state(input: &str) -> Result<Vec<WatchedPr>, String> {
    let value = parse_json(input).map_err(|_| "comment-router state is not JSON".to_owned())?;
    let items = match &value {
        Json::Array(items) => items,
        Json::Object(fields) => {
            if let Some(Json::Array(items)) = value
                .object("watched_pull_requests")
                .or_else(|| value.object("pull_requests"))
            {
                return items.iter().map(parse_state_entry).collect();
            }
            if let (Some(repository), Some(Json::Object(sessions))) = (
                value.object("repository").and_then(Json::as_str),
                value.object("sessions"),
            ) {
                if !valid_github_repo(repository) {
                    return Err("comment-router state has an invalid repository".to_owned());
                }
                return sessions
                    .iter()
                    .map(|(number, session)| {
                        legacy_session_for_repository(repository, number, session)
                    })
                    .collect();
            }
            // Accept the VM watcher's compact `pr_sessions.json` form too:
            // {"owner/repo#42": "session"} (or an object carrying
            // `session_id`). This makes moving the file to the Mac a data
            // migration, not a required Python conversion step.
            return fields
                .iter()
                .map(|(key, value)| parse_legacy_session_entry(key, value))
                .collect();
        }
        _ => return Err("comment-router state must be an object or array".to_owned()),
    };
    items.iter().map(parse_state_entry).collect()
}

fn legacy_session_for_repository(
    repository: &str,
    number: &str,
    value: &Json,
) -> Result<WatchedPr, String> {
    let session_id = session_id(value)?;
    let number = positive_pr_number(number)?;
    Ok(WatchedPr {
        repository: repository.to_owned(),
        number,
        session_id: session_id.to_owned(),
    })
}

fn parse_legacy_session_entry(key: &str, value: &Json) -> Result<WatchedPr, String> {
    let (repository, number) = key
        .rsplit_once('#')
        .ok_or_else(|| "comment-router state needs a watched_pull_requests array".to_owned())?;
    let session_id = session_id(value)?;
    let number = positive_pr_number(number)?;
    if !valid_github_repo(repository) {
        return Err("legacy PR session has an invalid repository".to_owned());
    }
    Ok(WatchedPr {
        repository: repository.to_owned(),
        number,
        session_id: session_id.to_owned(),
    })
}

fn session_id(value: &Json) -> Result<&str, String> {
    value
        .as_str()
        .or_else(|| value.object("session_id").and_then(Json::as_str))
        .or_else(|| value.object("session").and_then(Json::as_str))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "legacy PR session is missing session_id".to_owned())
}

fn positive_pr_number(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| "legacy PR session has an invalid pull request number".to_owned())
}

fn parse_state_entry(value: &Json) -> Result<WatchedPr, String> {
    let repository = value
        .object("repository")
        .and_then(Json::as_str)
        .filter(|value| valid_github_repo(value))
        .ok_or_else(|| "watched PR requires a valid repository".to_owned())?;
    let number = value
        .object("pull_request")
        .or_else(|| value.object("number"))
        .and_then(Json::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| "watched PR requires a positive pull_request".to_owned())?;
    let session_id = value
        .object("session_id")
        .or_else(|| value.object("session"))
        .and_then(Json::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "watched PR requires a session_id".to_owned())?;
    Ok(WatchedPr {
        repository: repository.to_owned(),
        number,
        session_id: session_id.to_owned(),
    })
}

fn parse_watch_entry(value: &str) -> Result<WatchedPr, String> {
    let (pr, session_id) = value
        .split_once(':')
        .filter(|(_, session)| !session.is_empty())
        .ok_or_else(|| "--watch-pr must be OWNER/REPO#NUMBER:SESSION".to_owned())?;
    let (repository, number) = pr
        .rsplit_once('#')
        .ok_or_else(|| "--watch-pr must be OWNER/REPO#NUMBER:SESSION".to_owned())?;
    if !valid_github_repo(repository) {
        return Err("--watch-pr must contain a valid OWNER/REPO".to_owned());
    }
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| "--watch-pr must contain a positive PR number".to_owned())?;
    Ok(WatchedPr {
        repository: repository.to_owned(),
        number,
        session_id: session_id.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zz::Store;

    #[test]
    fn graphql_node_id_is_the_only_comment_identity() {
        let comment = parse_comment(
            "issue",
            &parse_json(r#"{"id":5788565620,"node_id":"IC_kwDONdbzCM8AAAABWQaAdA","body":"please fix","user":{"login":"ShukantPal"}}"#).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            comment.event_id(),
            "github-pr-comment:IC_kwDONdbzCM8AAAABWQaAdA"
        );
        assert_eq!(
            canonical_node_id(Some(5788565620), Some("IC_kwDONdbzCM8AAAABWQaAdA")).unwrap(),
            "IC_kwDONdbzCM8AAAABWQaAdA"
        );
        assert!(canonical_node_id(Some(5788565620), None).is_err());
    }

    #[test]
    fn ignores_marker_bearing_bot_replies() {
        let mut comment = Comment {
            surface: "issue",
            node_id: "id".to_owned(),
            author: "ShukantPal".to_owned(),
            body: "> 🤖 Codex reply\nDone".to_owned(),
            url: String::new(),
            created_at: String::new(),
        };
        assert!(!comment.is_owner_feedback());
        comment.body = "Please handle this.".to_owned();
        assert!(comment.is_owner_feedback());
    }

    #[test]
    fn accepts_reloadable_pr_session_state() {
        let entries = parse_state(r#"{"watched_pull_requests":[{"repository":"ShukantPal/zigzag","pull_request":12,"session_id":"abc"}]}"#).unwrap();
        assert_eq!(
            entries,
            vec![WatchedPr {
                repository: "ShukantPal/zigzag".to_owned(),
                number: 12,
                session_id: "abc".to_owned()
            }]
        );
        assert!(parse_watch_entry("ShukantPal/zigzag#12:abc").is_ok());
        assert!(parse_watch_entry("not-a-pr").is_err());
        assert_eq!(
            parse_state(r#"{"ShukantPal/zigzag#12":{"session_id":"abc"}}"#).unwrap(),
            entries
        );
        assert_eq!(
            parse_state(
                r#"{"repository":"ShukantPal/zigzag","sessions":{"12":{"session_id":"abc"}}}"#
            )
            .unwrap(),
            entries
        );
    }

    #[test]
    fn prompt_includes_the_thread_and_keeps_the_newest_feedback() {
        let watched = WatchedPr {
            repository: "ShukantPal/zigzag".to_owned(),
            number: 12,
            session_id: "abc".to_owned(),
        };
        let prompt = comment_prompt(
            &watched,
            &[Comment {
                surface: "review",
                node_id: "id".to_owned(),
                author: "ShukantPal".to_owned(),
                body: "Please address this feedback.".to_owned(),
                url: "https://example.test/comment".to_owned(),
                created_at: "2026-09-26T12:00:00Z".to_owned(),
            }],
        );
        assert!(prompt.contains("ShukantPal/zigzag pull request #12"));
        assert!(prompt.contains("Please address this feedback."));
        assert_eq!(truncate_utf8_tail("ééé", 3), "é");
    }

    #[test]
    fn ledger_keeps_deduplication_after_reopen_and_releases_failed_claims() {
        let path =
            std::env::temp_dir().join(format!("zigzag-comment-ledger-{}", std::process::id()));
        let gate = SessionGate::open(path.clone()).unwrap();
        assert!(gate.claim("session"));
        gate.release("session"); // a pre-spawn failure is retryable
        assert!(gate.claim("session"));
        gate.bind("session", "agent").unwrap();
        gate.mark_dispatched(["github-pr-comment:node".to_owned()])
            .unwrap();
        drop(gate);
        let reopened = SessionGate::open(path.clone()).unwrap();
        assert!(reopened.is_dispatched("github-pr-comment:node"));
        assert_eq!(
            reopened.recovered_claims(),
            vec![("session".to_owned(), "agent".to_owned())]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parses_all_surfaces_and_selects_burst_after_routable_feedback() {
        let issue = parse_comments(
            "issue",
            r#"[[{"id":1,"node_id":"IC_a","body":"hello","user":{"login":"ShukantPal"}}]]"#,
        )
        .unwrap();
        let inline = parse_comments(
            "review_comment",
            r#"[[{"id":2,"node_id":"PRRC_b","body":"> 🤖 bot","user":{"login":"ShukantPal"}}]]"#,
        )
        .unwrap();
        let review = parse_comments("review", r#"[[{"id":3,"node_id":"PRR_c","body":"mobile feedback","submitted_at":"2026-09-26T12:00:00Z","user":{"login":"ShukantPal"}}]]"#).unwrap();
        assert!(issue[0].is_owner_feedback());
        assert!(!inline[0].is_owner_feedback());
        assert!(review[0].is_owner_feedback());
        let watched = WatchedPr {
            repository: "ShukantPal/zigzag".to_owned(),
            number: 13,
            session_id: "session".to_owned(),
        };
        assert_eq!(
            comment_event(&watched, &issue[0])
                .object("surface")
                .and_then(Json::as_str),
            Some("issue")
        );
        assert_eq!(
            comment_event(&watched, &review[0])
                .object("surface")
                .and_then(Json::as_str),
            Some("review")
        );
        let now = Instant::now();
        assert_eq!(
            poll_interval(now, now + Duration::from_secs(1), Duration::from_secs(300)),
            BURST_INTERVAL
        );
        assert_eq!(
            poll_interval(now, now, Duration::from_secs(300)),
            Duration::from_secs(300)
        );
    }

    fn watched() -> WatchedPr {
        WatchedPr {
            repository: "ShukantPal/zigzag".to_owned(),
            number: 13,
            session_id: "session".to_owned(),
        }
    }

    fn owner_comment(surface: &'static str, node_id: &str) -> Comment {
        Comment {
            surface,
            node_id: node_id.to_owned(),
            author: ROUTED_OWNER.to_owned(),
            body: "please fix".to_owned(),
            url: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn shadow_failures_release_the_claim_for_retry() {
        let watched = watched();
        let comment = owner_comment("issue", "node");
        let gate = SessionGate::in_memory();
        assert!(
            shadow_dispatch(&gate, &watched, std::slice::from_ref(&comment), |_| Err(
                "disk failed".to_owned()
            ))
            .is_err()
        );
        assert!(gate.claim(&watched.session_id));
        gate.release(&watched.session_id);

        // A ledger write failure follows the same release path. The in-memory
        // state is still removed even though the deliberately invalid ledger
        // path cannot be replaced.
        let broken = SessionGate {
            path: Some(PathBuf::from("/dev/null/comment-router")),
            inner: Mutex::new(GateState::default()),
        };
        assert!(shadow_dispatch(&broken, &watched, &[comment], |_| Ok(())).is_err());
        assert!(broken.claim(&watched.session_id));
    }

    #[test]
    fn live_scan_never_launches_after_persistence_failure_and_releases_claims() {
        use std::cell::Cell;

        let watched = watched();
        let comment = owner_comment("issue", "node");
        let gate = SessionGate::in_memory();
        let launches = Cell::new(0);
        assert!(
            scan_comments(
                &watched,
                std::slice::from_ref(&comment),
                false,
                &gate,
                ScanHooks {
                    persist: |_| Err("disk failed".to_owned()),
                    launch: |_: &[Comment]| {
                        launches.set(launches.get() + 1);
                        Ok("agent".to_owned())
                    },
                    on_started: |_: &str| {},
                    on_bind_failure: |_: &str| {},
                },
            )
            .is_err()
        );
        assert_eq!(launches.get(), 0);
        assert!(gate.claim(&watched.session_id));
        gate.release(&watched.session_id);

        assert!(
            scan_comments(
                &watched,
                std::slice::from_ref(&comment),
                false,
                &gate,
                ScanHooks {
                    persist: |_| Ok(()),
                    launch: |_: &[Comment]| Err("spawn failed".to_owned()),
                    on_started: |_: &str| {},
                    on_bind_failure: |_: &str| {},
                },
            )
            .is_err()
        );
        assert!(gate.claim(&watched.session_id));
        gate.release(&watched.session_id);

        let broken = SessionGate {
            path: Some(PathBuf::from("/dev/null/comment-router")),
            inner: Mutex::new(GateState::default()),
        };
        let started = Cell::new(0);
        let terminated = Cell::new(0);
        assert!(
            scan_comments(
                &watched,
                &[comment],
                false,
                &broken,
                ScanHooks {
                    persist: |_| Ok(()),
                    launch: |_: &[Comment]| Ok("agent".to_owned()),
                    on_started: |_: &str| started.set(started.get() + 1),
                    on_bind_failure: |_: &str| terminated.set(terminated.get() + 1),
                },
            )
            .is_err()
        );
        assert_eq!(started.get(), 1);
        assert_eq!(terminated.get(), 1);
        assert!(broken.claim(&watched.session_id));
    }

    #[test]
    fn durable_dedup_survives_event_eviction_and_router_reopen() {
        let base = std::env::temp_dir().join(format!(
            "zigzag-comment-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let event_path = base.join("events.json");
        let ledger_path = base.join("router.json");
        let watched = watched();
        let comment = owner_comment("review_comment", "PRRC_node");
        let store = Store::open(&event_path, 1).unwrap();
        let gate = SessionGate::open(ledger_path.clone()).unwrap();
        assert!(
            scan_comments(
                &watched,
                std::slice::from_ref(&comment),
                true,
                &gate,
                ScanHooks {
                    persist: |event| store.add(event).map(|_| ()),
                    launch: |_: &[Comment]| panic!("shadow scans never launch"),
                    on_started: |_: &str| {},
                    on_bind_failure: |_: &str| {},
                },
            )
            .unwrap()
        );
        // Evict the delivery event; routing identity remains in its own ledger.
        store
            .add(Json::Object(vec![(
                "id".to_owned(),
                Json::String("noise".to_owned()),
            )]))
            .unwrap();
        drop(store);
        drop(gate);

        let reopened_store = Store::open(&event_path, 1).unwrap();
        let reopened_gate = SessionGate::open(ledger_path).unwrap();
        assert!(
            !scan_comments(
                &watched,
                &[comment],
                true,
                &reopened_gate,
                ScanHooks {
                    persist: |event| reopened_store.add(event).map(|_| ()),
                    launch: |_: &[Comment]| panic!("shadow scans never launch"),
                    on_started: |_: &str| {},
                    on_bind_failure: |_: &str| {},
                },
            )
            .unwrap()
        );
        assert_eq!(
            reopened_store
                .read(0, "", Duration::ZERO)
                .unwrap()
                .events
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn paginated_all_surface_feedback_routes_through_a_burst_poll_cycle() {
        let issue = parse_comments("issue", r#"[[{"id":1,"node_id":"IC_1","body":"one","user":{"login":"ShukantPal"}}],[{"id":9}]]"#).unwrap();
        let inline = parse_comments(
            "review_comment",
            r#"[[{"id":2,"node_id":"PRRC_2","body":"two","user":{"login":"ShukantPal"}},{"id":4,"node_id":"PRRC_bot","body":"> 🤖 done","user":{"login":"ShukantPal"}}]]"#,
        )
        .unwrap();
        let review = parse_comments(
            "review",
            r#"[[{"id":3,"node_id":"PRR_3","body":"three","user":{"login":"ShukantPal"}}]]"#,
        )
        .unwrap();
        let comments = [issue, inline, review].concat();
        let gate = SessionGate::in_memory();
        let watched = watched();
        let base = std::env::temp_dir().join(format!(
            "zigzag-comment-surfaces-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(base.join("events.json"), 10).unwrap();
        let now = Instant::now();
        let mut burst_until = now;
        assert!(
            watch_cycle(
                vec![watched],
                now,
                &mut burst_until,
                Duration::from_secs(300),
                Duration::from_secs(300),
                |current| {
                    scan_comments(
                        current,
                        &comments,
                        true,
                        &gate,
                        ScanHooks {
                            persist: |event| store.add(event).map(|_| ()),
                            launch: |_: &[Comment]| panic!("shadow scans never launch"),
                            on_started: |_: &str| {},
                            on_bind_failure: |_: &str| {},
                        },
                    )
                },
            ) == BURST_INTERVAL
        );
        assert_eq!(
            watch_cycle(
                Vec::new(),
                now + Duration::from_secs(301),
                &mut burst_until,
                Duration::from_secs(300),
                Duration::from_secs(300),
                |_| Ok(false),
            ),
            Duration::from_secs(300)
        );
        let events = store.read(0, "", Duration::ZERO).unwrap();
        let surfaces: Vec<_> = events
            .events
            .iter()
            .map(|event| {
                event
                    .payload
                    .object("surface")
                    .and_then(Json::as_str)
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(surfaces, ["issue", "review_comment", "review"]);
        let _ = std::fs::remove_dir_all(base);
    }
}
