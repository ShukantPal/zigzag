use crate::events::relay_timestamp;
use crate::http::{error, query, reply};
use crate::server::Server;
use relay_core::{AgentRecord, Json};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) enum AgentRoute<'a> {
    List,
    Status(&'a str),
    Logs(&'a str),
    Transcript(&'a str),
    Pause(&'a str),
    Resume(&'a str),
}
pub(crate) fn agent_route(path: &str) -> Option<AgentRoute<'_>> {
    if path == "/v1/agents" {
        return Some(AgentRoute::List);
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
        // Pause and resume are POST-only routes.
        AgentRoute::Pause(_) | AgentRoute::Resume(_) => reply(stream, 404, error("not_found")),
    }
}

/// Handle POST requests to agent routes (pause/resume).
pub(crate) fn agent_post_request(
    stream: &mut TcpStream,
    state: &Server,
    route: AgentRoute<'_>,
) -> Result<(), String> {
    match route {
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
