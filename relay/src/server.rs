use crate::auth::authorized;
use crate::exec;
use crate::github::review_gate_request;
use crate::http::{ReadRequestError, denied, error, get_query, read_json, read_request, reply};
use crate::logging;
use crate::proc::ProcEntry;
use crate::review_loop;
use crate::routes::agents::{
    AgentRoute, agent_delete, agent_post_request, agent_request, agent_restart_request,
    agent_restart_route, agent_route,
};
use crate::routes::exec::{exec_request, spawn_request};
use crate::routes::procs::{ProcRoute, kill_proc, poll_proc, proc_route};
use crate::routes::providers::providers_request;
use crate::routes::worktrees::{worktree_create, worktree_delete};
use crate::session::require_gui_login_session;
use crate::update;
use relay_core::{AgentRegistry, Json, Store, parse_json};
use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Upper bound on simultaneous in-flight connections. The accept loop
/// sheds excess connections with 503 instead of spawning unbounded
/// threads: each thread carries a stack plus a 10s read timeout, so a
/// slow flood could otherwise exhaust memory or file descriptors.
pub(crate) const MAX_CONNECTIONS: usize = 32;
pub(crate) struct Server {
    pub(crate) secret: String,
    pub(crate) control_secret: Option<String>,
    pub(crate) store: Arc<Store>,
    pub(crate) supervisor: Supervisor,
    pub(crate) updater: Arc<update::Manager>,
    pub(crate) review_state_file: PathBuf,
    pub(crate) review_loop_shadow: bool,
    pub(crate) review_config: Mutex<Option<Arc<review_loop::ReviewLoopConfig>>>,
}
/// Live handles deliberately disappear on restart; the durable half lives in
/// `relay-core::AgentRegistry` and records the resulting orphan/loss state.
pub(crate) struct Supervisor {
    pub(crate) registry: Arc<AgentRegistry>,
    pub(crate) procs: Mutex<HashMap<String, ProcEntry>>,
}
/// Admission control for inbound connections. The permit is held for the
/// whole handler thread and released on drop, so at most `max` connections
/// are ever in flight at once.
pub(crate) struct ConnectionLimiter {
    pub(crate) active: Mutex<usize>,
    pub(crate) max: usize,
}

pub(crate) struct ConnectionPermit {
    pub(crate) limiter: Arc<ConnectionLimiter>,
}

impl ConnectionLimiter {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            active: Mutex::new(0),
            max,
        }
    }

    /// Best-effort admission: a permit while fewer than `max` connections are
    /// in flight, `None` when the server is saturated.
    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
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
pub(crate) fn serve(listener: TcpListener, state: Arc<Server>, limiter: Arc<ConnectionLimiter>) {
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
pub(crate) fn handle(stream: TcpStream, state: Arc<Server>) -> Result<(), String> {
    handle_with_policy(stream, state, || {
        require_gui_login_session().and_then(|_| exec::load_policy())
    })
}
pub(crate) fn handle_with_policy<F>(
    stream: TcpStream,
    state: Arc<Server>,
    load_policy: F,
) -> Result<(), String>
where
    F: Fn() -> Result<exec::Policy, String>,
{
    handle_with_services(stream, state, load_policy, review_loop::gate_report)
}
pub(crate) fn handle_with_services<F, G>(
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
        ("POST", "/v1/exec") => exec_request(&mut stream, request.body, load_policy()),
        ("POST", "/v1/spawn") => spawn_request(&mut stream, &state, request.body, load_policy()),
        ("POST", "/v1/worktrees") => worktree_create(&mut stream, request.body),
        ("DELETE", "/v1/worktrees") => worktree_delete(&mut stream, &state, request.body),
        ("GET", "/v1/providers") => providers_request(&mut stream),
        ("DELETE", path) => match agent_route("DELETE", path) {
            Some(AgentRoute::Status(id)) => agent_delete(&mut stream, &state, id),
            _ => reply(&mut stream, 404, error("not_found")),
        },
        ("GET", "/v1/review-gate") => {
            review_gate_request(&mut stream, &state, &request.target, gate_report)
        }
        ("GET", path) if agent_route("GET", path).is_some() => agent_request(
            &mut stream,
            &state,
            &request.target,
            agent_route("GET", path).expect("checked"),
        ),
        ("GET", path) => match proc_route(path) {
            Some(ProcRoute::Poll(handle)) => poll_proc(&mut stream, &state, handle),
            Some(ProcRoute::Kill(_)) => reply(&mut stream, 404, error("not_found")),
            None => reply(&mut stream, 404, error("not_found")),
        },
        ("POST", path) if agent_route("POST", path).is_some() => agent_post_request(
            &mut stream,
            &state,
            agent_route("POST", path).expect("checked"),
            request.body,
        ),
        ("POST", path) => match agent_restart_route(path) {
            Some(id) => agent_restart_request(&mut stream, &state, id, request.body),
            None => match proc_route(path) {
                Some(ProcRoute::Kill(handle)) => kill_proc(&mut stream, &state, handle),
                Some(ProcRoute::Poll(_)) => reply(&mut stream, 404, error("not_found")),
                None => reply(&mut stream, 404, error("not_found")),
            },
        },
        _ => reply(&mut stream, 404, error("not_found")),
    }
}
pub(crate) fn post(stream: &mut TcpStream, state: &Server, body: Vec<u8>) -> Result<(), String> {
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
pub(crate) fn get(stream: &mut TcpStream, state: &Server, target: &str) -> Result<(), String> {
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
