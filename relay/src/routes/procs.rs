use crate::events::{persist_first_output, relay_event};
use crate::http::{error, reply};
use crate::proc::{
    ProcEntry, agent_transcript_path, kill_process_group, output_is_complete, proc_json,
    process_group_running, prune_procs, sync_durable_output,
};
use crate::server::Server;
use relay_core::{AgentRegistry, Json, Store};
use std::net::TcpStream;
use std::process::Command;
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
        sync_durable_output(entry, handle, &state.supervisor.registry, &state.store);
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
        sync_durable_output(entry, handle, &state.supervisor.registry, &state.store);
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
                if let Some(agent) = registry.get(handle) {
                    let watch_agent = agent.clone();
                    let _ = std::thread::Builder::new()
                        .name(format!("zigzag-watch-pr-{}", &handle[..8]))
                        .spawn(move || {
                            if let Err(error) = crate::github::watch_agent_pr(&watch_agent) {
                                log::warn!("agent completion PR discovery failed: {error}");
                            }
                        });
                }
                if state == "succeeded"
                    && let Some(agent) = registry.get(handle)
                {
                    let _ = std::thread::Builder::new()
                        .name(format!("zigzag-auto-pr-{}", &handle[..8]))
                        .spawn(move || auto_create_pr_if_needed(&agent));
                }
            } else {
                let _ = registry.mark_audit_degraded(handle);
            }
        }
    }
}

fn auto_create_pr_if_needed(agent: &relay_core::AgentRecord) {
    let Some(config_text) = agent.agent_config.as_deref() else {
        return;
    };
    let Ok(config) = relay_core::parse_json(config_text) else {
        return;
    };
    let Json::Object(fields) = config else { return };
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    if field("auto_pr") == Some(&Json::Bool(false)) {
        return;
    }
    let (Some(branch), Some(worktree), Some(prompt)) = (
        field("branch").and_then(Json::as_str),
        field("worktree").and_then(Json::as_str),
        field("prompt").and_then(Json::as_str),
    ) else {
        return;
    };
    let repo = field("project_dir")
        .and_then(Json::as_str)
        .unwrap_or(worktree);
    let summary = std::fs::read_to_string(std::path::Path::new(worktree).join("last-message.txt"))
        .ok()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| prompt.to_owned());
    let pushed = Command::new("git")
        .args([
            "ls-remote",
            "--exit-code",
            "origin",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(worktree)
        .output();
    match pushed {
        Ok(output) if output.status.success() => {}
        Ok(_) => return,
        Err(error) => {
            log::warn!(
                "auto_pr agent={} could not check pushed branch: {error}",
                agent.id
            );
            return;
        }
    }
    let existing = Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--state", "open", "--json", "number", "--jq", "length",
        ])
        .current_dir(repo)
        .output();
    match existing {
        Ok(output)
            if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "0" =>
        {
            return;
        }
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            log::warn!(
                "auto_pr agent={} gh pr list failed: {}",
                agent.id,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return;
        }
        Err(error) => {
            log::warn!(
                "auto_pr agent={} could not run gh pr list: {error}",
                agent.id
            );
            return;
        }
    }
    let transcript = agent_transcript_path(&agent.id)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| format!("agent {}", agent.id));
    let body = format!(
        "## What changed\n\n{summary}\n\n## Agent transcript\n\n[Transcript]({transcript})\n"
    );
    let created = Command::new("gh")
        .args([
            "pr", "create", "--head", branch, "--title", branch, "--body", &body,
        ])
        .current_dir(repo)
        .output();
    match created {
        Ok(output) if output.status.success() => {
            log::info!(
                "auto_pr agent={} created PR: {}",
                agent.id,
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => log::warn!(
            "auto_pr agent={} gh pr create failed: {}",
            agent.id,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => log::warn!(
            "auto_pr agent={} could not run gh pr create: {error}",
            agent.id
        ),
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
