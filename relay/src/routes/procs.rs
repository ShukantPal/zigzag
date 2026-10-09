use crate::events::{persist_first_output, relay_event};
use crate::http::{error, reply};
use crate::proc::{
    ProcEntry, kill_process_group, output_is_complete, proc_json, process_group_running,
    prune_procs,
};
use crate::server::Server;
use relay_core::{AgentRegistry, Json, Store};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub(crate) enum ProcRoute<'a> {
    Poll(&'a str),
    Kill(&'a str),
}
pub(crate) fn poll_proc(
    stream: &mut TcpStream,
    state: &Server,
    handle: &str,
) -> Result<(), String> {
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
pub(crate) fn kill_proc(
    stream: &mut TcpStream,
    state: &Server,
    handle: &str,
) -> Result<(), String> {
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
pub(crate) fn update_proc_status_with_handle(
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
pub(crate) fn proc_route(path: &str) -> Option<ProcRoute<'_>> {
    let path = path.strip_prefix("/v1/proc/")?;
    if let Some(handle) = path.strip_suffix("/kill") {
        (!handle.is_empty() && !handle.contains('/')).then_some(ProcRoute::Kill(handle))
    } else {
        (!path.is_empty() && !path.contains('/')).then_some(ProcRoute::Poll(path))
    }
}
