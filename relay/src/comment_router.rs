//! Native GitHub PR-comment routing.
//!
//! The router deliberately reads GitHub through the same read-only `gh`
//! policy as the PR watchdog.  It only launches Codex after persisting a
//! GraphQL-node-id event, and defaults to shadow mode while it is compared to
//! the incumbent watcher.

use crate::{Server, exec, new_execution_id, spawn_proc};
use relay_core::{Json, parse_json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ROUTED_OWNER: &str = "ShukantPal";
const BOT_MARKER: &str = "> 🤖";
const BURST_INTERVAL: Duration = Duration::from_secs(30);
const PROMPT: &str = include_str!("../prompts/comment_address.md");

#[derive(Clone)]
pub(crate) struct Config {
    state_file: PathBuf,
    command_entries: Vec<String>,
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
        for entry in &command_entries {
            parse_watch_entry(entry)?;
        }
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
    path: PathBuf,
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
            path,
            inner: Mutex::new(state),
        })
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
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
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
        std::fs::rename(temporary, &self.path)
            .map_err(|_| "could not replace comment-router ledger".to_owned())
    }
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
        for watched_pr in watched {
            match scan_pr(&state, &watched_pr, config.shadow) {
                Ok(true) => burst_until = Instant::now() + config.burst_window,
                Ok(false) => {}
                Err(error) => eprintln!(
                    "GitHub comment watch for {}#{} failed: {error}",
                    watched_pr.repository, watched_pr.number
                ),
            }
        }
        let interval = poll_interval(Instant::now(), burst_until, config.quiet_interval);
        thread::sleep(interval);
    }
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
    let candidates: Vec<_> = comments
        .iter()
        .filter(|comment| comment.is_owner_feedback())
        .filter(|comment| !state.session_gate.is_dispatched(&comment.event_id()))
        .cloned()
        .collect();
    if candidates.is_empty() || !state.session_gate.claim(&watched.session_id) {
        return Ok(false);
    }

    if shadow {
        let result = (|| {
            for comment in &candidates {
                state
                    .store
                    .add(comment_event(watched, comment))
                    .map_err(|error| format!("could not persist comment event: {error}"))?;
            }
            state
                .session_gate
                .mark_dispatched(candidates.iter().map(Comment::event_id))
        })();
        if let Err(error) = result {
            state.session_gate.release(&watched.session_id);
            return Err(error);
        }
        eprintln!(
            "shadow: would resume session {} for {}#{} on {} new GitHub comment(s)",
            watched.session_id,
            watched.repository,
            watched.number,
            candidates.len()
        );
        state.session_gate.release(&watched.session_id);
        return Ok(true);
    }

    let prompt = comment_prompt(watched, &comments);
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
    let policy = match crate::require_gui_login_session().and_then(|_| exec::load_policy()) {
        Ok(policy) => policy,
        Err(error) => {
            state.session_gate.release(&watched.session_id);
            return Err(error);
        }
    };
    let path = policy
        .allowed_path(&request.bin, &request.args)
        .ok_or_else(|| "the execution policy does not allow codex comment resumes".to_owned());
    let path = match path {
        Ok(path) => path,
        Err(error) => {
            state.session_gate.release(&watched.session_id);
            return Err(error);
        }
    };
    let execution_id = match new_execution_id() {
        Ok(id) => id,
        Err(error) => {
            state.session_gate.release(&watched.session_id);
            return Err(error);
        }
    };
    // Persist the event before launching. The ledger is only marked dispatched
    // after a successful spawn, so any pre-launch failure remains retryable.
    for comment in &candidates {
        if let Err(error) = state.store.add(comment_event(watched, comment)) {
            state.session_gate.release(&watched.session_id);
            return Err(format!("could not persist comment event: {error}"));
        }
    }
    let spawned = match spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        path,
        request,
        execution_id,
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            state.session_gate.release(&watched.session_id);
            return Err(format!("could not spawn comment resume: {error}"));
        }
    };
    release_when_finished(
        Arc::clone(&state.session_gate),
        Arc::clone(&state.supervisor.registry),
        watched.session_id.clone(),
        spawned.handle.clone(),
    );
    if let Err(error) = state
        .session_gate
        .bind(&watched.session_id, &spawned.handle)
    {
        terminate_spawned(&state.supervisor, &spawned.handle);
        state.session_gate.release(&watched.session_id);
        return Err(error);
    }
    if let Err(error) = state
        .session_gate
        .mark_dispatched(candidates.iter().map(Comment::event_id))
    {
        // The supervisor monitor installed above still releases this claim.
        // Retrying after it exits is safer than stranding a session forever.
        eprintln!("could not mark GitHub comment dispatch complete: {error}");
    }
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

fn terminate_spawned(supervisor: &crate::Supervisor, handle: &str) {
    if let Ok(entries) = supervisor.procs.lock()
        && let Some(entry) = entries.get(handle)
    {
        let _ = crate::kill_process_group(entry.process_group);
    }
}

fn release_when_finished(
    gate: Arc<SessionGate>,
    registry: Arc<relay_core::AgentRegistry>,
    session_id: String,
    handle: String,
) {
    thread::spawn(move || {
        loop {
            let done = registry
                .get(&handle)
                .map(|agent| !crate::process_group_running(agent.process_group))
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
    for (session_id, handle) in state.session_gate.recovered_claims() {
        let running = state
            .supervisor
            .registry
            .get(&handle)
            .is_some_and(|agent| crate::process_group_running(agent.process_group));
        if running {
            release_when_finished(
                Arc::clone(&state.session_gate),
                Arc::clone(&state.supervisor.registry),
                session_id,
                handle,
            );
        } else {
            state.session_gate.release(&session_id);
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
        comments.extend(parse_comments(
            surface,
            &crate::github_api(
                &endpoint,
                &format!("github-comment-scan-{}", endpoint.replace('/', "-")),
            )?,
        )?);
    }
    comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    Ok(comments)
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
        let entry = parse_watch_entry(entry)?;
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
                if !crate::valid_github_repo(repository) {
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
    if !crate::valid_github_repo(repository) {
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
        .filter(|value| crate::valid_github_repo(value))
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
    if !crate::valid_github_repo(repository) {
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
}
