<<<<<<< HEAD
use crate::events::relay_timestamp;
use crate::http::{error, query, reply};
use crate::proc::{kill_process_group, process_group_running, recovered_agent_identity_matches};
=======
use crate::events::{
    new_execution_id, random_hex_128, relay_event, relay_timestamp, unix_timestamp,
};
use crate::exec;
use crate::http::{denial_json, error, query, reply};
use crate::proc::{AgentSpawnDetails, spawn_proc};
use crate::routes::worktrees::{
    WORKTREE_REPO_ROOT, WorktreeError, canonical_worktree_roots, git_output,
    resolve_new_worktree_path, resolve_worktree_repo, valid_worktree_branch,
    worktree_branch_checked_out, worktree_branch_exists,
};
>>>>>>> 5643798 (Port agent-create endpoint to modular relay structure)
use crate::server::Server;
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

pub(crate) enum AgentRoute<'a> {
    List,
    Create,
    Status(&'a str),
    Logs(&'a str),
    Transcript(&'a str),
    Pause(&'a str),
    Resume(&'a str),
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
    policy: Result<exec::Policy, String>,
) -> Result<(), String> {
    match route {
        AgentRoute::Create => agent_create_request(stream, state, body, policy),
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
    let logs = registry.logs_json(id, "both", 0, tail)?;

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

/// Resolve the dept task directory for a relay task_id.
/// Relay task IDs look like `codex-t-821762`; dept dirs are `t-821762`.
fn dept_task_dir(task_id: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let dir_name = task_id.strip_prefix("codex-").unwrap_or(task_id);
    let dir = home.join(".codex/dept").join(dir_name);
    dir.is_dir().then_some(dir)
}

<<<<<<< HEAD
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
        stop_process_group(id, pgid)
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
fn stop_process_group(id: &str, pgid: i32) -> (bool, bool) {
    if !kill_process_group(pgid) {
        // The signal was refused: the group vanished between the identity
        // check and the signal. Treat as already dead.
        return (true, true);
    }
    if wait_for_group_exit(pgid, AGENT_STOP_GRACE) {
        return (true, true);
    }
    log::warn!("agent_stop id={id} pgid={pgid}: SIGTERM ignored, sending SIGKILL");
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    (wait_for_group_exit(pgid, AGENT_KILL_GRACE), false)
}

fn wait_for_group_exit(pgid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_group_running(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
=======
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
    let parsed = parse_json(text).map_err(|_| "invalid_agent_create_request")?;
    let Json::Object(fields) = parsed else {
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
            None => {
                if required {
                    Err("invalid_agent_create_request")
                } else {
                    Ok(None)
                }
            }
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

/// Derive the default worktree directory from a branch name:
/// `codex/my-feature` becomes `/private/tmp/codex-my-feature/`.
pub(crate) fn default_agent_worktree(branch: &str) -> String {
    format!("/private/tmp/{}/", branch.replace('/', "-"))
}

/// Model identifier validation, mirroring dept.py's MODEL_RE
/// (`[A-Za-z0-9][A-Za-z0-9._:/-]*`).
pub(crate) fn valid_agent_model(model: &str) -> bool {
    let mut chars = model.chars();
    if !matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric()) {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
}

/// Worktree creation for the agent-create flow: validation failures reuse the
/// worktree endpoint codes, while git failures carry the stderr detail the
/// endpoint reports.
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

/// `POST /v1/agents`: resolve a worktree, create it, then launch a supervised
/// `codex exec` agent through the same spawn machinery as `/v1/spawn`. The
/// agent is registered with its worktree path so `DELETE /v1/worktrees` can
/// refuse to remove a live agent's worktree.
fn agent_create_request(
    stream: &mut TcpStream,
    state: &Server,
    body: Vec<u8>,
    policy: Result<exec::Policy, String>,
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
    // Like spawn_request: record receipt before any admission decision.
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
    // Step 1 — cap concurrent running agents.
    let running = state.supervisor.registry.list(Some("running"), None).len();
    if running >= state.max_agents {
        log::warn!(
            "agent_create refused: {running} running agents (max {})",
            state.max_agents
        );
        return reply(
            stream,
            503,
            Json::Object(vec![
                (
                    "error".to_owned(),
                    Json::String("too_many_agents".to_owned()),
                ),
                ("max".to_owned(), Json::number(state.max_agents as u64)),
            ]),
        );
    }
    // Step 2 — resolve the worktree path and the owning repository.
    if !valid_worktree_branch(&request.branch) {
        return reply(stream, 400, error("worktree_invalid_branch"));
    }
    let roots = canonical_worktree_roots();
    let repo_root = std::fs::canonicalize(WORKTREE_REPO_ROOT)
        .unwrap_or_else(|_| PathBuf::from(WORKTREE_REPO_ROOT));
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
    // Resolve the prompt before any side effect. A path prompt is read now;
    // inline text is written into the worktree after creation.
    let inline_prompt = !Path::new(&request.prompt).is_file();
    let file_prompt = if inline_prompt {
        None
    } else {
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
    };
    // Build the codex argv like dept.py: `codex exec --json <approval>
    // --skip-git-repo-check [-m model] -C <worktree> -o last-message.txt <prompt>`.
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
            log::warn!("agent_create refused: invalid model");
            return reply(stream, 400, error("invalid_model"));
        }
        codex_args.push("-m".to_owned());
        codex_args.push(model.clone());
    }
    let worktree_str = worktree_path.to_string_lossy().into_owned();
    codex_args.push("-C".to_owned());
    codex_args.push(worktree_str.clone());
    codex_args.push("-o".to_owned());
    codex_args.push(format!("{worktree_str}/last-message.txt"));
    // Policy check before any filesystem side effect: a denied codex must not
    // leave a worktree behind. The prompt text is the final argv element, so
    // the allowlist prefix check on the leading flags is unaffected by it.
    let policy = match policy {
        Ok(policy) => policy,
        Err(message) => {
            log::error!("agent_create policy read failed: {message}");
            return reply(stream, 500, error("could_not_read_execution_policy"));
        }
    };
    let codex_path = match policy.verified_path("codex", &codex_args) {
        Ok(path) => path,
        Err(exec::VerifyError::Denied) => {
            return reply(stream, 200, denial_json(&task_id));
        }
        Err(exec::VerifyError::Unverifiable(reason)) => {
            log::error!("agent_create binary verification failed: {reason}");
            return reply(stream, 500, error("could_not_verify_binary"));
        }
    };
    // Step 3 — create the worktree. No agent is registered unless this succeeds.
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
    // Step 4 — prompt file.
    let prompt_text = match file_prompt {
        Some(text) => text,
        None => {
            let prompt_file = worktree_path.join(".codex-prompt.md");
            if let Err(message) = std::fs::write(&prompt_file, &request.prompt) {
                log::error!("agent_create could not write prompt file: {message}");
                return reply(stream, 500, error("could_not_write_prompt"));
            }
            request.prompt.clone()
        }
    };
    codex_args.push(prompt_text);
    // Admit the spawn through the update drain gate, mirroring /v1/spawn.
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
    // Steps 5+6 — spawn through the supervised-spawn machinery and register
    // the agent with its worktree path.
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
        &codex_path,
        command,
        execution_id.clone(),
        AgentSpawnDetails {
            worktree_path: Some(worktree_str.clone()),
            deadline_at,
        },
    ) {
        Ok(handle) => {
            log::info!(
                "agent_create id={} branch={} worktree={}",
                handle.handle,
                request.branch,
                worktree_str
            );
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
                log::error!("could not persist agent spawn failure audit event");
            }
            log::error!("agent_create spawn failed for id={task_id}: {message}");
            reply(stream, 500, error("could_not_spawn_process"))
        }
>>>>>>> 5643798 (Port agent-create endpoint to modular relay structure)
    }
}
