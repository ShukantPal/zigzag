use crate::events::{new_execution_id, relay_event};
use crate::exec;
use crate::http::{denial_json, error, reply};
use crate::logging;
use crate::proc::{AgentSpawnDetails, spawn_proc};
use crate::server::Server;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;
use zz::{Json, parse_json};

pub(crate) struct SpawnRequest {
    pub(crate) command: exec::ExecRequest,
    pub(crate) execution_id: Option<String>,
}
pub(crate) fn exec_request(
    stream: &mut TcpStream,
    body: Vec<u8>,
    policy: Result<exec::Policy, String>,
) -> Result<(), String> {
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
    let policy = match policy {
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
pub(crate) fn spawn_request(
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
            Json::Object(vec![
                (
                    "process".to_owned(),
                    Json::String(request.command.bin.clone()),
                ),
                (
                    "command".to_owned(),
                    Json::Array(
                        request
                            .command
                            .args
                            .iter()
                            .cloned()
                            .map(Json::String)
                            .collect(),
                    ),
                ),
            ]),
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
        AgentSpawnDetails::default(),
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
pub(crate) fn parse_exec_request(body: &[u8]) -> Result<exec::ExecRequest, Json> {
    let parsed = std::str::from_utf8(body)
        .ok()
        .and_then(|text| parse_json(text).ok());
    let Some(parsed) = parsed else {
        return Err(denial_json(""));
    };
    let denied_id = exec::request_id(&parsed);
    exec::parse_request(&parsed).map_err(|_| denial_json(&denied_id))
}
pub(crate) fn parse_spawn_request(body: &[u8]) -> Result<SpawnRequest, Json> {
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
