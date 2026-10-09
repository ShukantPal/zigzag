use crate::events::{
    new_execution_id, random_hex_128, relay_event, relay_timestamp, unix_timestamp,
};
use crate::exec;
use crate::http::{error, query, reply};
use crate::proc::{
    AgentSpawnDetails, agent_transcript_path, force_kill_process_group, kill_process_group,
    managed_agent_running, process_group_running, recovered_agent_identity_matches, spawn_proc,
};
#[cfg(not(test))]
use crate::routes::worktrees::{WORKTREE_REPO_ROOT, canonical_worktree_roots};
use crate::routes::worktrees::{
    WorktreeError, git_output, resolve_new_worktree_path, resolve_worktree_repo,
    valid_worktree_branch, worktree_branch_checked_out, worktree_branch_exists,
};
use crate::server::{Server, Supervisor};
use relay_core::{AgentRecord, Json, parse_json};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Grace period after SIGTERM before escalating to SIGKILL.
const AGENT_STOP_GRACE: Duration = Duration::from_secs(10);
/// Grace period after SIGKILL before giving up.
const AGENT_KILL_GRACE: Duration = Duration::from_secs(5);

#[cfg(not(test))]
fn agent_worktree_roots() -> Vec<PathBuf> {
    canonical_worktree_roots()
}

#[cfg(test)]
fn agent_worktree_roots() -> Vec<PathBuf> {
    vec![
        std::env::temp_dir()
            .canonicalize()
            .expect("test temporary directory must exist"),
    ]
}

#[cfg(not(test))]
fn agent_worktree_repo_root() -> PathBuf {
    std::fs::canonicalize(WORKTREE_REPO_ROOT).unwrap_or_else(|_| PathBuf::from(WORKTREE_REPO_ROOT))
}

#[cfg(test)]
fn agent_worktree_repo_root() -> PathBuf {
    std::env::temp_dir()
        .canonicalize()
        .expect("test temporary directory must exist")
}

/// The relay owns the Codex command used for high-level agent creation.
/// Unlike `/v1/exec`, callers never supply this binary or its arguments, so
/// the arbitrary-command execution policy does not apply here.
#[cfg(not(test))]
fn agent_codex_path() -> &'static Path {
    Path::new("codex")
}

#[cfg(test)]
fn agent_codex_path() -> &'static Path {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::OnceLock;

    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("zigzag-test-codex-{}", std::process::id()));
        std::fs::write(
            &path,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    *persist-transcript*)\n      printf '{\"type\":\"item.completed\",\"text\":\"persisted-agent-output\"}\\n'\n      printf 'persisted-agent-stderr\\n' >&2\n      exit 0\n      ;;\n  esac\ndone\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .expect("could not create test Codex runner");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("could not make test Codex runner executable");
        path
    })
    .as_path()
}

pub(crate) enum AgentRoute<'a> {
    List,
    Create,
    Status(&'a str),
    Logs(&'a str),
    Transcript(&'a str),
    Pause(&'a str),
    Resume(&'a str),
}

/// POST-only route: `/v1/agents/{id}/restart`.
pub(crate) fn agent_restart_route(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1/agents/")?;
    let id = rest.strip_suffix("/restart")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}
pub(crate) fn agent_route<'a>(method: &'a str, path: &'a str) -> Option<AgentRoute<'a>> {
    if path == "/v1/agents" {
        return match method {
            "GET" => Some(AgentRoute::List),
            "POST" => Some(AgentRoute::Create),
            _ => None,
        };
    }
    let rest = path.strip_prefix("/v1/agents/")?;
    if let Some(id) = rest.strip_suffix("/logs") {
        return (!id.is_empty() && !id.contains('/')).then_some(AgentRoute::Logs(id));
    }
    if let Some(id) = rest.strip_suffix("/transcript") {
        return (!id.is_empty() && !id.contains('/')).then_some(AgentRoute::Transcript(id));
    }
    if let Some(id) = rest.strip_suffix("/pause") {
        return (!id.is_empty() && !id.contains('/')).then_some(AgentRoute::Pause(id));
    }
    if let Some(id) = rest.strip_suffix("/resume") {
        return (!id.is_empty() && !id.contains('/')).then_some(AgentRoute::Resume(id));
    }
    (!rest.is_empty() && !rest.contains('/')).then_some(AgentRoute::Status(rest))
}
pub(crate) fn agent_request(
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
                let Some(logs) =
                    agent_logs_json(&state.supervisor.registry, id, stream_name, after, tail)
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
        AgentRoute::Transcript(id) => {
            let tail = match values
                .get("tail")
                .map(|value| value.parse::<usize>())
                .transpose()
            {
                Ok(value) => value,
                Err(_) => return reply(stream, 400, error("invalid_transcript_query")),
            };
            match transcript_json(&state.supervisor.registry, id, tail) {
                Some(json) => reply(stream, 200, json),
                None => reply(stream, 404, error("unknown_agent")),
            }
        }
        // Pause, resume, and create are POST-only routes.
        AgentRoute::Pause(_) | AgentRoute::Resume(_) | AgentRoute::Create => {
            reply(stream, 404, error("not_found"))
        }
    }
}

/// Handle POST requests to agent routes (create/pause/resume).
pub(crate) fn agent_post_request(
    stream: &mut TcpStream,
    state: &Server,
    route: AgentRoute<'_>,
    body: Vec<u8>,
) -> Result<(), String> {
    match route {
        AgentRoute::Create => agent_create_request(stream, state, body),
        AgentRoute::Pause(id) => pause_agent_request(stream, state, id),
        AgentRoute::Resume(id) => resume_agent_request(stream, state, id),
        _ => reply(stream, 404, error("not_found")),
    }
}

fn pause_agent_request(stream: &mut TcpStream, state: &Server, id: &str) -> Result<(), String> {
    let agent = match state.supervisor.registry.get(id) {
        Some(agent) => agent,
        None => return reply(stream, 404, error("unknown_agent")),
    };
    if agent.state != "running" {
        return reply(stream, 409, error("agent_not_running"));
    }
    if agent.paused_at.is_some() {
        return reply(stream, 409, error("already_paused"));
    }
    let process_group = agent.process_group;
    // Signal the whole process group, matching how agents are spawned and killed.
    if unsafe { libc::kill(-process_group, libc::SIGSTOP) } != 0 {
        // The group may already be gone; the reaper's orphan logic marks the
        // agent honestly, but the pause marker is still recorded.
        log::warn!("agent_pause id={id} pgid={process_group}: SIGSTOP failed");
    }
    let paused_at = relay_timestamp();
    state
        .supervisor
        .registry
        .set_paused_at(id, Some(paused_at.clone()))?;
    log::info!("agent_pause id={id} pgid={process_group}");
    reply(
        stream,
        200,
        Json::Object(vec![
            ("id".to_owned(), Json::String(id.to_owned())),
            ("paused".to_owned(), Json::Bool(true)),
            ("paused_at".to_owned(), Json::String(paused_at)),
        ]),
    )
}

fn resume_agent_request(stream: &mut TcpStream, state: &Server, id: &str) -> Result<(), String> {
    let agent = match state.supervisor.registry.get(id) {
        Some(agent) => agent,
        None => return reply(stream, 404, error("unknown_agent")),
    };
    if agent.paused_at.is_none() {
        return reply(stream, 409, error("not_paused"));
    }
    let process_group = agent.process_group;
    if unsafe { libc::kill(-process_group, libc::SIGCONT) } != 0 {
        log::warn!("agent_resume id={id} pgid={process_group}: SIGCONT failed");
    }
    state.supervisor.registry.set_paused_at(id, None)?;
    log::info!("agent_resume id={id} pgid={process_group}");
    reply(
        stream,
        200,
        Json::Object(vec![
            ("id".to_owned(), Json::String(id.to_owned())),
            ("paused".to_owned(), Json::Bool(false)),
        ]),
    )
}

/// Build the full transcript for an agent: record metadata, prompt,
/// last message, and concatenated stdout/stderr.
fn transcript_json(
    registry: &relay_core::AgentRegistry,
    id: &str,
    tail: Option<usize>,
) -> Option<Json> {
    let agent = registry.get(id)?;
    let logs = agent_logs_json(registry, id, "both", 0, tail)?;

    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    if let Some(Json::Array(records)) = logs.object("records") {
        for record in records {
            let stream_name = record.object("stream").and_then(Json::as_str).unwrap_or("");
            let data = record.object("data").and_then(Json::as_str).unwrap_or("");
            match stream_name {
                "stdout" => stdout_text.push_str(data),
                "stderr" => stderr_text.push_str(data),
                _ => {}
            }
        }
    }

    // The relay intentionally does not retain prompt text (see relay-core).
    // For dept-managed Codex tasks, the prompt lives in the task directory.
    let task_dir = dept_task_dir(agent.task_id.as_str());
    let prompt = task_dir
        .as_ref()
        .and_then(|dir| std::fs::read_to_string(dir.join("prompt.txt")).ok());
    let last_message = task_dir
        .as_ref()
        .and_then(|dir| std::fs::read_to_string(dir.join("last-message.txt")).ok());

    Some(Json::Object(vec![
        ("id".to_owned(), Json::String(agent.id.clone())),
        ("task_id".to_owned(), Json::String(agent.task_id.clone())),
        (
            "execution_id".to_owned(),
            Json::String(agent.execution_id.clone()),
        ),
        ("command".to_owned(), Json::String(agent.command.clone())),
        ("state".to_owned(), Json::String(agent.state.clone())),
        (
            "started_at".to_owned(),
            Json::String(agent.started_at.clone()),
        ),
        (
            "exit_code".to_owned(),
            agent
                .exit_code
                .map(|v| Json::Number(v.to_string()))
                .unwrap_or(Json::Null),
        ),
        (
            "prompt".to_owned(),
            prompt.map(Json::String).unwrap_or(Json::Null),
        ),
        (
            "last_message".to_owned(),
            last_message.map(Json::String).unwrap_or(Json::Null),
        ),
        ("stdout".to_owned(), Json::String(stdout_text)),
        ("stderr".to_owned(), Json::String(stderr_text)),
        ("log_degraded".to_owned(), Json::Bool(agent.log_degraded)),
    ]))
}

/// Read API-created Codex stdout from its durable JSONL transcript. Stderr
/// continues to use the existing separate diagnostics spool. Older and
/// generic agents have no transcript file and retain the original spool-only
/// behavior.
fn agent_logs_json(
    registry: &relay_core::AgentRegistry,
    id: &str,
    stream: &str,
    after: u64,
    tail: Option<usize>,
) -> Option<Json> {
    let agent = registry.get(id)?;
    let transcript = agent_transcript_path(id).and_then(|path| std::fs::read_to_string(path).ok());
    let Some(transcript) = transcript else {
        return registry.logs_json(id, stream, after, tail);
    };

    let stderr_logs = registry.logs_json(id, "stderr", after, tail)?;
    let mut records = Vec::new();
    if matches!(stream, "stdout" | "both") && after == 0 {
        let data = match tail {
            Some(limit) if transcript.len() > limit => {
                let mut start = transcript.len() - limit;
                while !transcript.is_char_boundary(start) {
                    start += 1;
                }
                transcript[start..].to_owned()
            }
            _ => transcript,
        };
        if !data.is_empty() {
            records.push(Json::Object(vec![
                ("stream".to_owned(), Json::String("stdout".to_owned())),
                ("cursor".to_owned(), Json::number(0)),
                ("data".to_owned(), Json::String(data)),
            ]));
        }
    }
    if matches!(stream, "stderr" | "both")
        && let Some(Json::Array(stderr_records)) = stderr_logs.object("records")
    {
        records.extend(stderr_records.iter().cloned());
    }
    Some(Json::Object(vec![
        ("records".to_owned(), Json::Array(records)),
        (
            "next_cursor".to_owned(),
            Json::number(
                agent.log_next.max(
                    agent_transcript_path(id)
                        .and_then(|path| std::fs::metadata(path).ok())
                        .map_or(0, |metadata| metadata.len()),
                ),
            ),
        ),
        (
            "dropped_before".to_owned(),
            Json::number(agent.log_dropped_before),
        ),
        ("complete".to_owned(), Json::Bool(agent.state != "running")),
        ("log_degraded".to_owned(), Json::Bool(agent.log_degraded)),
    ]))
}

/// Resolve the dept task directory for a relay task_id.
/// Relay task IDs look like `codex-t-821762`; dept dirs are `t-821762`.
fn dept_task_dir(task_id: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let dir_name = task_id.strip_prefix("codex-").unwrap_or(task_id);
    let dir = home.join(".codex/dept").join(dir_name);
    dir.is_dir().then_some(dir)
}

/// Request body for `POST /v1/agents`: launch a supervised Codex agent in a
/// fresh worktree.
pub(crate) struct AgentCreateRequest {
    pub(crate) prompt: String,
    pub(crate) project_dir: String,
    pub(crate) branch: String,
    pub(crate) worktree: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) approval_mode: Option<String>,
    pub(crate) timeout_secs: Option<u64>,
}

pub(crate) fn parse_agent_create_request(body: &[u8]) -> Result<AgentCreateRequest, &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| "invalid_agent_create_request")?;
    let Json::Object(fields) = parse_json(text).map_err(|_| "invalid_agent_create_request")? else {
        return Err("invalid_agent_create_request");
    };
    if fields.iter().any(|(name, _)| {
        !matches!(
            name.as_str(),
            "prompt"
                | "project_dir"
                | "branch"
                | "worktree"
                | "model"
                | "approval_mode"
                | "timeout_secs"
        )
    }) {
        return Err("invalid_agent_create_request");
    }
    let string_field = |name: &str, required: bool| -> Result<Option<String>, &'static str> {
        match fields.iter().find(|(key, _)| key == name) {
            None if required => Err("invalid_agent_create_request"),
            None => Ok(None),
            Some((_, value)) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .map(Some)
                .ok_or("invalid_agent_create_request"),
        }
    };
    let timeout_secs = match fields.iter().find(|(key, _)| key == "timeout_secs") {
        None => None,
        Some((_, value)) => Some(value.as_u64().ok_or("invalid_agent_create_request")?),
    };
    Ok(AgentCreateRequest {
        prompt: string_field("prompt", true)?.expect("required field"),
        project_dir: string_field("project_dir", true)?.expect("required field"),
        branch: string_field("branch", true)?.expect("required field"),
        worktree: string_field("worktree", false)?,
        model: string_field("model", false)?,
        approval_mode: string_field("approval_mode", false)?,
        timeout_secs,
    })
}

/// Derive the default worktree directory from a branch name.
pub(crate) fn default_agent_worktree(branch: &str) -> String {
    format!("/private/tmp/{}/", branch.replace('/', "-"))
}

/// Model identifier validation, mirroring dept.py's MODEL_RE.
pub(crate) fn valid_agent_model(model: &str) -> bool {
    let mut chars = model.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
}

/// Worktree creation errors distinguish validation from a failed git command.
#[derive(Debug)]
pub(crate) enum AgentWorktreeFailure {
    Validation(WorktreeError),
    GitFailed(String),
}

pub(crate) fn agent_create_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
) -> Result<(), AgentWorktreeFailure> {
    let validation = AgentWorktreeFailure::Validation;
    if !git_output(repo, &["rev-parse", "--git-dir"])
        .map_err(validation)?
        .status
        .success()
    {
        return Err(validation(WorktreeError {
            code: 400,
            message: "worktree_repo_not_a_git_repo",
        }));
    }
    if worktree_branch_checked_out(repo, branch).map_err(validation)? {
        return Err(validation(WorktreeError {
            code: 400,
            message: "worktree_branch_already_checked_out",
        }));
    }
    let path_str = path.to_str().ok_or(validation(WorktreeError {
        code: 400,
        message: "worktree_path_not_unicode",
    }))?;
    let output = if worktree_branch_exists(repo, branch).map_err(validation)? {
        git_output(repo, &["worktree", "add", path_str, branch]).map_err(validation)?
    } else {
        git_output(repo, &["worktree", "add", "-b", branch, path_str]).map_err(validation)?
    };
    if !output.status.success() {
        return Err(AgentWorktreeFailure::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    log::info!(
        "agent_create worktree path={} branch={branch}",
        path.display()
    );
    Ok(())
}

/// `POST /v1/agents`: create a worktree and launch a supervised `codex exec`.
fn agent_create_request(
    stream: &mut TcpStream,
    state: &Server,
    body: Vec<u8>,
) -> Result<(), String> {
    if state.updater.is_draining() {
        return reply(stream, 503, error("updates_draining"));
    }
    let request = match parse_agent_create_request(&body) {
        Ok(request) => request,
        Err(message) => return reply(stream, 400, error(message)),
    };
    let execution_id = match new_execution_id() {
        Ok(id) => id,
        Err(message) => {
            log::error!("agent_create could not mint execution id: {message}");
            return reply(stream, 500, error("could_not_create_agent"));
        }
    };
    let task_id = match random_hex_128() {
        Ok(hex) => format!("agent-{}", &hex[..12]),
        Err(message) => {
            log::error!("agent_create could not mint task id: {message}");
            return reply(stream, 500, error("could_not_create_agent"));
        }
    };
    if state
        .store
        .add(relay_event(
            "relay_request_started",
            &task_id,
            &execution_id,
            Json::Object(vec![]),
        ))
        .is_err()
    {
        return reply(stream, 500, error("could_not_persist_event"));
    }
    if !valid_worktree_branch(&request.branch) {
        return reply(stream, 400, error("worktree_invalid_branch"));
    }
    let roots = agent_worktree_roots();
    let repo_root = agent_worktree_repo_root();
    let worktree_raw = request
        .worktree
        .clone()
        .unwrap_or_else(|| default_agent_worktree(&request.branch));
    let worktree_path = match resolve_new_worktree_path(&worktree_raw, &roots) {
        Ok(path) => path,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let repo = match resolve_worktree_repo(&request.project_dir, &repo_root) {
        Ok(repo) => repo,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let file_prompt = if Path::new(&request.prompt).is_file() {
        match std::fs::read_to_string(&request.prompt) {
            Ok(text) => Some(text),
            Err(message) => {
                log::error!(
                    "agent_create could not read prompt file {}: {message}",
                    request.prompt
                );
                return reply(stream, 500, error("could_not_read_prompt"));
            }
        }
    } else {
        None
    };
    let approval_flag = match request.approval_mode.as_deref() {
        None => "--approve-for-me",
        Some("suggest") => "--suggest",
        Some("auto-edit") => "--auto-edit",
        Some("full-auto") => "--full-auto",
        Some(other) => {
            log::warn!("agent_create refused: unknown approval_mode={other}");
            return reply(stream, 400, error("invalid_approval_mode"));
        }
    };
    let mut codex_args = vec![
        "exec".to_owned(),
        "--json".to_owned(),
        approval_flag.to_owned(),
        "--skip-git-repo-check".to_owned(),
    ];
    if let Some(model) = &request.model {
        if !valid_agent_model(model) {
            return reply(stream, 400, error("invalid_model"));
        }
        codex_args.extend(["-m".to_owned(), model.clone()]);
    }
    let worktree_str = worktree_path.to_string_lossy().into_owned();
    codex_args.extend([
        "-C".to_owned(),
        worktree_str.clone(),
        "-o".to_owned(),
        format!("{worktree_str}/last-message.txt"),
    ]);
    match agent_create_worktree(&repo, &worktree_path, &request.branch) {
        Ok(()) => {}
        Err(AgentWorktreeFailure::Validation(failure)) => {
            return reply(stream, failure.code, error(failure.message));
        }
        Err(AgentWorktreeFailure::GitFailed(detail)) => {
            log::error!(
                "agent_create worktree add failed for branch={}: {detail}",
                request.branch
            );
            return reply(
                stream,
                500,
                Json::Object(vec![
                    (
                        "error".to_owned(),
                        Json::String("worktree_failed".to_owned()),
                    ),
                    ("detail".to_owned(), Json::String(detail)),
                ]),
            );
        }
    }
    let prompt_text = match file_prompt {
        Some(text) => text,
        None => {
            if let Err(message) =
                std::fs::write(worktree_path.join(".codex-prompt.md"), &request.prompt)
            {
                log::error!("agent_create could not write prompt file: {message}");
                return reply(stream, 500, error("could_not_write_prompt"));
            }
            request.prompt.clone()
        }
    };
    codex_args.push(prompt_text.clone());
    let _spawn_admission = match state.updater.spawn_admission() {
        Ok(Some(guard)) => guard,
        Ok(None) => return reply(stream, 503, error("updates_draining")),
        Err(message) => {
            log::error!("agent_create admission failed: {message}");
            return reply(stream, 500, error("could_not_admit_process"));
        }
    };
    if state
        .store
        .add(relay_event(
            "relay_accepted",
            &task_id,
            &execution_id,
            Json::Object(vec![]),
        ))
        .is_err()
    {
        return reply(stream, 500, error("could_not_persist_event"));
    }
    let deadline_at = request.timeout_secs.map(|secs| {
        unix_timestamp()
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_add(secs)
            .to_string()
    });
    let command = exec::ExecRequest {
        id: task_id.clone(),
        bin: "codex".to_owned(),
        args: codex_args,
    };
    match spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        agent_codex_path(),
        command,
        execution_id.clone(),
        AgentSpawnDetails {
            worktree_path: Some(worktree_str.clone()),
            deadline_at,
            persist_transcript: true,
        },
    ) {
        Ok(handle) => {
            // Persist the text actually passed to Codex.  In particular, a
            // prompt-file request must remain restartable after the caller's
            // file has changed or disappeared.
            let config = persisted_agent_config(&request, &worktree_str, &prompt_text);
            if !matches!(
                state
                    .supervisor
                    .registry
                    .set_agent_config(&handle.handle, &config),
                Ok(Some(_))
            ) {
                // A restartable API agent must not run before its durable
                // launch metadata has been committed.
                if let Some(agent) = state.supervisor.registry.get(&handle.handle) {
                    let _ = force_kill_process_group(agent.process_group);
                }
                let _ = state.supervisor.registry.transition(
                    &handle.handle,
                    "agent_config_persist_failed",
                    None,
                );
                return reply(stream, 500, error("could_not_persist_agent_config"));
            }
            reply(
                stream,
                200,
                Json::Object(vec![
                    ("id".to_owned(), Json::String(handle.handle)),
                    ("worktree".to_owned(), Json::String(worktree_str)),
                ]),
            )
        }
        Err(message) => {
            let _ = state.store.add(relay_event(
                "process_failed",
                &task_id,
                &execution_id,
                Json::Object(vec![(
                    "reason".to_owned(),
                    Json::String("spawn_failed".to_owned()),
                )]),
            ));
            log::error!("agent_create spawn failed for id={task_id}: {message}");
            reply(stream, 500, error("could_not_spawn_process"))
        }
    }
}

pub(crate) fn persisted_agent_config(
    request: &AgentCreateRequest,
    worktree: &str,
    prompt: &str,
) -> String {
    Json::Object(vec![
        ("prompt".to_owned(), Json::String(prompt.to_owned())),
        (
            "project_dir".to_owned(),
            Json::String(request.project_dir.clone()),
        ),
        ("branch".to_owned(), Json::String(request.branch.clone())),
        ("worktree".to_owned(), Json::String(worktree.to_owned())),
        (
            "model".to_owned(),
            request
                .model
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "approval_mode".to_owned(),
            request
                .approval_mode
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "timeout_secs".to_owned(),
            request.timeout_secs.map(Json::number).unwrap_or(Json::Null),
        ),
    ])
    .to_json()
}

pub(crate) struct AgentRestartConfig {
    pub(crate) prompt: String,
    worktree: String,
    model: Option<String>,
    approval_mode: Option<String>,
    pub(crate) timeout_secs: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RestartMode {
    Fresh,
    Resume,
}

struct RestartRequest {
    mode: RestartMode,
    prompt: Option<String>,
}

fn parse_restart_request(body: &[u8]) -> Result<RestartRequest, &'static str> {
    if body.is_empty() {
        return Ok(RestartRequest {
            mode: RestartMode::Fresh,
            prompt: None,
        });
    }
    let text = std::str::from_utf8(body).map_err(|_| "invalid_restart_request")?;
    let Json::Object(fields) = parse_json(text).map_err(|_| "invalid_restart_request")? else {
        return Err("invalid_restart_request");
    };
    if fields
        .iter()
        .any(|(name, _)| !matches!(name.as_str(), "mode" | "prompt"))
    {
        return Err("invalid_restart_request");
    }
    let mode = match fields.iter().find(|(key, _)| key == "mode") {
        None => RestartMode::Fresh,
        Some((_, value)) => match value.as_str() {
            Some("fresh") => RestartMode::Fresh,
            Some("resume") => RestartMode::Resume,
            _ => return Err("invalid_restart_request"),
        },
    };
    let prompt = match fields.iter().find(|(key, _)| key == "prompt") {
        None => None,
        Some((_, value)) => Some(
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or("invalid_restart_request")?,
        ),
    };
    Ok(RestartRequest { mode, prompt })
}

pub(crate) fn restart_config(text: &str) -> Result<AgentRestartConfig, &'static str> {
    let Json::Object(fields) = parse_json(text).map_err(|_| "invalid_agent_config")? else {
        return Err("invalid_agent_config");
    };
    let required = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or("invalid_agent_config")
    };
    let optional = |name: &str| -> Result<Option<String>, &'static str> {
        match fields.iter().find(|(key, _)| key == name) {
            None | Some((_, Json::Null)) => Ok(None),
            Some((_, value)) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .map(Some)
                .ok_or("invalid_agent_config"),
        }
    };
    // Validate the complete creation schema; only launch-relevant fields are
    // retained here, avoiding dead duplicate state.
    required("project_dir")?;
    required("branch")?;
    let timeout_secs = match fields.iter().find(|(key, _)| key == "timeout_secs") {
        None | Some((_, Json::Null)) => None,
        Some((_, value)) => Some(value.as_u64().ok_or("invalid_agent_config")?),
    };
    Ok(AgentRestartConfig {
        prompt: required("prompt")?,
        worktree: required("worktree")?,
        model: optional("model")?,
        approval_mode: optional("approval_mode")?,
        timeout_secs,
    })
}

pub(crate) fn restart_argv(
    config: &AgentRestartConfig,
    mode: RestartMode,
    prompt: &str,
    session: Option<&str>,
) -> Result<Vec<String>, &'static str> {
    let approval = match config.approval_mode.as_deref() {
        None => "--approve-for-me",
        Some("suggest") => "--suggest",
        Some("auto-edit") => "--auto-edit",
        Some("full-auto") => "--full-auto",
        Some(_) => return Err("invalid_approval_mode"),
    };
    let mut args = vec![
        "exec".to_owned(),
        "--json".to_owned(),
        approval.to_owned(),
        "--skip-git-repo-check".to_owned(),
    ];
    if let Some(model) = &config.model {
        if !valid_agent_model(model) {
            return Err("invalid_model");
        }
        args.extend(["-m".to_owned(), model.clone()]);
    }
    match mode {
        RestartMode::Fresh => args.extend([
            "-C".to_owned(),
            config.worktree.clone(),
            "-o".to_owned(),
            format!("{}/last-message.txt", config.worktree),
            prompt.to_owned(),
        ]),
        RestartMode::Resume => args.extend([
            "resume".to_owned(),
            session.expect("resume session").to_owned(),
            prompt.to_owned(),
            "-o".to_owned(),
            format!("{}/last-message.txt", config.worktree),
        ]),
    }
    Ok(args)
}

fn extract_codex_session_id(registry: &relay_core::AgentRegistry, id: &str) -> Option<String> {
    let logs = agent_logs_json(registry, id, "stdout", 0, None)?;
    let Json::Array(records) = logs.object("records")? else {
        return None;
    };
    let mut output = String::new();
    for record in records {
        if let Some(data) = record.object("data").and_then(Json::as_str) {
            output.push_str(data);
        }
    }
    serde_json::Deserializer::from_str(&output)
        .into_iter::<serde_json::Value>()
        .filter_map(Result::ok)
        .find_map(|record| session_id_in_json(&record).map(str::to_owned))
}

fn session_id_in_json(value: &serde_json::Value) -> Option<&str> {
    let object = value.as_object()?;
    for key in ["thread_id", "session_id", "threadId", "sessionId"] {
        if let Some(id) = object.get(key).and_then(serde_json::Value::as_str)
            && !id.is_empty()
            && id.len() < 128
        {
            return Some(id);
        }
    }
    // Codex event streams also carry the identifier as `thread.id` or
    // `payload.id`; recognize those shapes without treating unrelated `id`
    // fields elsewhere in an event as resumable sessions.
    for container in ["thread", "payload"] {
        if let Some(id) = object
            .get(container)
            .and_then(serde_json::Value::as_object)
            .and_then(|nested| nested.get("id"))
            .and_then(serde_json::Value::as_str)
            && !id.is_empty()
            && id.len() < 128
        {
            return Some(id);
        }
    }
    object.values().find_map(session_id_in_json)
}

pub(crate) fn agent_restart_request(
    stream: &mut TcpStream,
    state: &Server,
    id: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    let request = match parse_restart_request(&body) {
        Ok(request) => request,
        Err(code) => return reply(stream, 400, error(code)),
    };
    let Some(agent) = state.supervisor.registry.get(id) else {
        return reply(stream, 404, error("unknown_agent"));
    };
    if managed_agent_running(&agent) || agent.state == "paused" {
        return reply(stream, 409, error("agent_not_stopped"));
    }
    let Some(config_text) = agent.agent_config.as_deref() else {
        return reply(stream, 400, error("restart_config_missing"));
    };
    let config = match restart_config(config_text) {
        Ok(config) => config,
        Err(code) => return reply(stream, 500, error(code)),
    };
    if !Path::new(&config.worktree).is_absolute() || !Path::new(&config.worktree).is_dir() {
        return reply(stream, 400, error("worktree_missing"));
    }
    let session = if request.mode == RestartMode::Resume {
        match extract_codex_session_id(&state.supervisor.registry, id) {
            Some(session) => Some(session),
            None => return reply(stream, 400, error("no_resumable_session")),
        }
    } else {
        None
    };
    let prompt = request.prompt.unwrap_or_else(|| match request.mode {
        RestartMode::Fresh => config.prompt.clone(),
        RestartMode::Resume => "Continue where you left off. Review the current worktree and proceed with the original task.".to_owned(),
    });
    let args = match restart_argv(&config, request.mode, &prompt, session.as_deref()) {
        Ok(args) => args,
        Err(code) => return reply(stream, 400, error(code)),
    };
    let _admission = match state.updater.spawn_admission() {
        Ok(Some(guard)) => guard,
        Ok(None) => return reply(stream, 503, error("updates_draining")),
        Err(_) => return reply(stream, 503, error("could_not_admit_process")),
    };
    let execution_id = new_execution_id().map_err(|_| "could_not_create_execution".to_owned())?;
    if state
        .store
        .add(relay_event(
            "relay_accepted",
            &agent.task_id,
            &execution_id,
            Json::Object(vec![]),
        ))
        .is_err()
    {
        return reply(stream, 500, error("could_not_persist_event"));
    }
    let spawned = match spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        agent_codex_path(),
        exec::ExecRequest {
            id: agent.task_id.clone(),
            bin: "codex".to_owned(),
            args,
        },
        execution_id,
        AgentSpawnDetails {
            worktree_path: Some(config.worktree.clone()),
            deadline_at: config.timeout_secs.map(|secs| {
                unix_timestamp()
                    .parse::<u64>()
                    .unwrap_or(0)
                    .saturating_add(secs)
                    .to_string()
            }),
            persist_transcript: true,
        },
    ) {
        Ok(spawned) => spawned,
        Err(_) => return reply(stream, 500, error("restart_spawn_failed")),
    };
    if !matches!(
        state
            .supervisor
            .registry
            .set_restart_metadata(&spawned.handle, id, Some(config_text)),
        Ok(Some(_))
    ) {
        if let Some(spawned_agent) = state.supervisor.registry.get(&spawned.handle) {
            let _ = force_kill_process_group(spawned_agent.process_group);
        }
        let _ =
            state
                .supervisor
                .registry
                .transition(&spawned.handle, "restart_metadata_failed", None);
        return reply(stream, 500, error("could_not_persist_restart_metadata"));
    }
    reply(
        stream,
        200,
        Json::Object(vec![
            ("id".to_owned(), Json::String(spawned.handle)),
            ("restarted_from".to_owned(), Json::String(id.to_owned())),
            ("worktree".to_owned(), Json::String(config.worktree)),
            (
                "mode".to_owned(),
                Json::String(
                    match request.mode {
                        RestartMode::Fresh => "fresh",
                        RestartMode::Resume => "resume",
                    }
                    .to_owned(),
                ),
            ),
        ]),
    )
}

/// `DELETE /v1/agents/{id}`: gracefully stop a registered agent's process
/// group and deregister it. The worktree is deliberately left in place; use
/// `DELETE /v1/worktrees` to remove it.
pub(crate) fn agent_delete(stream: &mut TcpStream, state: &Server, id: &str) -> Result<(), String> {
    let Some(agent) = state.supervisor.registry.get(id) else {
        return reply(stream, 404, error("unknown_agent"));
    };
    if !matches!(agent.state.as_str(), "running" | "orphaned") {
        return reply(stream, 409, error("agent_not_running"));
    }
    let pgid = agent.process_group;
    let worktree = agent.worktree_path.clone();
    // Security: never signal a process group that does not belong to this
    // registered agent. A stale record (PID/PGID reuse, or a leader that has
    // already exited) is treated as already dead: deregister without
    // signalling. The pgid > 0 guard keeps kill(-pgid, ..) from ever
    // resolving to the relay's own process group on a corrupt record.
    let (stopped, graceful) = if pgid > 0 && recovered_agent_identity_matches(&agent) {
        stop_process_group(id, pgid, &state.supervisor)
    } else {
        (true, true)
    };
    if stopped {
        // Mark any live proc-table entry finished so the reaper does not
        // overwrite the terminal state with its own exit classification.
        if let Ok(mut table) = state.supervisor.procs.lock()
            && let Some(entry) = table.get_mut(id)
        {
            let _ = entry.child.try_wait();
            entry.finished_at = Some(Instant::now());
        }
        let terminal = if graceful { "stopped" } else { "killed" };
        let _ = state.supervisor.registry.transition(id, terminal, None);
    } else {
        log::error!(
            "agent_stop id={id} pgid={pgid}: process group survived SIGKILL; left registered"
        );
    }
    log::info!("agent_stop id={id} pgid={pgid} graceful={graceful}");
    match &worktree {
        Some(path) => log::info!("agent_stop id={id}: worktree retained at {path}"),
        None => log::info!("agent_stop id={id}: no worktree recorded"),
    }
    reply(
        stream,
        200,
        Json::Object(vec![
            ("id".to_owned(), Json::String(id.to_owned())),
            ("stopped".to_owned(), Json::Bool(stopped)),
            (
                "worktree".to_owned(),
                worktree.map(Json::String).unwrap_or(Json::Null),
            ),
        ]),
    )
}

/// SIGTERM a verified-owned process group, escalating to SIGKILL after
/// [`AGENT_STOP_GRACE`]. Returns `(stopped, graceful)`; `stopped` is false
/// only when the group survived SIGKILL plus [`AGENT_KILL_GRACE`].
fn stop_process_group(id: &str, pgid: i32, supervisor: &Supervisor) -> (bool, bool) {
    if !kill_process_group(pgid) {
        // The signal was refused: the group vanished between the identity
        // check and the signal. Treat as already dead.
        return (true, true);
    }
    if wait_for_group_exit(id, pgid, AGENT_STOP_GRACE, supervisor) {
        return (true, true);
    }
    log::warn!("agent_stop id={id} pgid={pgid}: SIGTERM ignored, sending SIGKILL");
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    (
        wait_for_group_exit(id, pgid, AGENT_KILL_GRACE, supervisor),
        false,
    )
}

fn wait_for_group_exit(id: &str, pgid: i32, timeout: Duration, supervisor: &Supervisor) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        // Linux retains an exited child as a zombie in its process group until
        // the parent reaps it. Reap the supervised leader while waiting so the
        // group can disappear before the grace period expires.
        if let Ok(mut table) = supervisor.procs.lock()
            && let Some(entry) = table.get_mut(id)
        {
            let _ = entry.child.try_wait();
        }
        if !process_group_running(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}
