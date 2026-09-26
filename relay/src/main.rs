mod auth;
mod comment_router;
mod config;
mod events;
mod exec;
mod github;
mod http;
mod logging;
mod proc;
mod provider;
mod review_loop;
mod routes;
mod server;
mod session;
mod update;

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod tests;

use crate::config::{run_config, server_config};
use crate::events::{new_execution_id, relay_event, replay_recovered_lifecycle};
use crate::github::{github_watch_loop, should_start_legacy_watch};
use crate::proc::{recovered_agent_identity_matches, start_reaper};
use crate::routes::events::run_timeline;
use crate::server::{ConnectionLimiter, MAX_CONNECTIONS, Server, Supervisor, serve};
use crate::session::resolve_tailscale_ip;
use relay_core::{AgentRegistry, Json, Store, read_secret_file};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::{env, thread};

fn main() {
    logging::init();
    if let Err(error) = run() {
        log::error!("startup failed: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("config") {
        return run_config(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("timeline") {
        return run_timeline(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("updates") {
        return update::run_control(&arguments[1..]);
    }
    if arguments.first().map(String::as_str) == Some("update-watchdog") {
        return update::run_watchdog(&arguments[1..]);
    }
    let config = server_config(arguments.clone())?;
    log::info!(
        "loaded server config: port={} state_file={} agent_registry_file={} max_events={}",
        config.port,
        config.state_file.display(),
        config.agent_registry_file.display(),
        config.max_events
    );
    let secret = read_secret_file(&config.secret_file)?;
    log::info!("loaded daemon secret from {}", config.secret_file.display());
    let control_secret = config
        .control_secret_file
        .as_deref()
        .map(read_secret_file)
        .transpose()?;
    if let Some(path) = config.control_secret_file.as_deref() {
        log::info!("loaded control secret from {}", path.display());
    }
    let updater = Arc::new(update::Manager::new(update::Config {
        directory: config.update_directory.clone(),
        interval: config.update_interval,
        policy: config.update_policy.clone(),
        ready_file: config.update_ready_file.clone(),
    }));
    let review_loop_shadow = env::var("ZIGZAG_REVIEW_LOOP_SHADOW").as_deref() == Ok("1");
    let state_file = config.state_file.clone();
    let store = Arc::new(Store::open(state_file.clone(), config.max_events)?);
    log::info!(
        "opened event store {} (max_events={})",
        state_file.display(),
        config.max_events
    );
    let agent_registry_file = config.agent_registry_file.clone();
    let registry = Arc::new(AgentRegistry::open(agent_registry_file.clone())?);
    let agent_records = registry.list(None, None).len();
    log::info!(
        "opened agent registry {} with {agent_records} records",
        agent_registry_file.display()
    );
    let state = Arc::new(Server {
        secret,
        control_secret,
        store,
        supervisor: Supervisor {
            registry,
            procs: Mutex::new(HashMap::new()),
        },
        updater: Arc::clone(&updater),
        review_state_file: config.review_state_file.clone(),
        review_loop_shadow,
        review_config: Mutex::new(None),
    });
    let recovered = state
        .supervisor
        .registry
        .recover(recovered_agent_identity_matches)?;
    // Re-apply SIGSTOP to agents that were paused before the restart: their
    // process groups may still be alive. Dead groups are left for the recover
    // pass above, which marks the agents honestly.
    let mut repaused = 0;
    for agent in state.supervisor.registry.list(None, None) {
        if agent.state == "running" && agent.paused_at.is_some() {
            if unsafe { libc::kill(-agent.process_group, libc::SIGSTOP) } == 0 {
                repaused += 1;
            } else {
                log::warn!(
                    "could not reapply pause to agent {}: process group {} gone",
                    agent.id,
                    agent.process_group
                );
            }
        }
    }
    log::info!("reapplied pause to {repaused} agents on startup");
    log::info!("recovered {} agent records from registry", recovered.len());
    for agent in recovered {
        replay_recovered_lifecycle(&state.store, &agent)?;
    }
    start_reaper(Arc::clone(&state));
    let mut review_loop_authoritative = false;
    match review_loop::default_config_path() {
        Ok(path) => match review_loop::load_config(&path) {
            Ok(personal) if personal.review_loop.enabled => {
                let review_config = Arc::new(personal.review_loop);
                match review_loop::start(
                    Arc::clone(&state),
                    (*review_config).clone(),
                    config.review_state_file.clone(),
                    review_loop_shadow,
                ) {
                    Ok(()) => {
                        *state
                            .review_config
                            .lock()
                            .map_err(|_| "review config lock poisoned".to_owned())? =
                            Some(review_config);
                        review_loop_authoritative = !review_loop_shadow;
                    }
                    Err(error) => log::warn!("review loop disabled: {error}"),
                }
            }
            Ok(_) => log::info!("review loop disabled by ~/.zigzag/config.yaml"),
            Err(violations) => {
                log::warn!("review loop disabled: invalid ~/.zigzag/config.yaml");
                for violation in violations {
                    log::warn!("review loop config: {violation}");
                }
            }
        },
        Err(violation) => log::warn!("review loop disabled: {violation}"),
    }
    if should_start_legacy_watch(review_loop_authoritative, &config.github_watch_repos) {
        let state = Arc::clone(&state);
        let repos = config.github_watch_repos.clone();
        let interval = config.github_watch_interval;
        thread::spawn(move || github_watch_loop(state, repos, interval));
        log::info!(
            "started GitHub PR watch loop for {} repositories (interval={:?})",
            config.github_watch_repos.len(),
            config.github_watch_interval
        );
    }
    if config.comment_router.enabled() {
        let state = Arc::clone(&state);
        let router = config.comment_router.clone();
        let shadow = router.is_shadow();
        thread::spawn(move || comment_router::watch_loop(state, router));
        log::info!("started GitHub PR comment router (shadow={shadow})");
    }
    let tailnet = config.tailscale_ip.unwrap_or(resolve_tailscale_ip()?);
    let addresses = [
        SocketAddr::new(IpAddr::from([127, 0, 0, 1]), config.port),
        SocketAddr::new(tailnet, config.port),
    ];
    // One limiter for both listeners: the cap bounds total handler threads,
    // not threads per socket.
    let limiter = Arc::new(ConnectionLimiter::new(MAX_CONNECTIONS));
    for address in addresses {
        let listener = TcpListener::bind(address)
            .map_err(|error| format!("could not bind {address}: {error}"))?;
        let state = Arc::clone(&state);
        let limiter = Arc::clone(&limiter);
        log::info!("listening on http://{address}");
        thread::spawn(move || serve(listener, state, limiter));
    }
    // The replacement only signals readiness after it has opened durable state
    // and rebound both listeners. The watchdog rolls back if this does not
    // happen; no launchctl restart is involved.
    let replacement_version = env::var("ZIGZAG_UPDATE_VERSION").ok();
    updater.acknowledge_ready(replacement_version.as_deref())?;
    log::info!("signaled readiness to update watchdog");
    if let Some(version) = replacement_version {
        let execution_id = new_execution_id()?;
        state.store.add(relay_event(
            "relay_update_applied",
            "relay-update",
            &execution_id,
            Json::Object(vec![("new_version".to_owned(), Json::String(version))]),
        ))?;
    }
    let update_state = Arc::clone(&state);
    let update_audit_state = Arc::clone(&state);
    let update_execution = Mutex::new(None::<String>);
    let update_arguments = update::persistent_server_args(&arguments);
    updater.start(
        Arc::new(move || {
            !update_state
                .supervisor
                .registry
                .list(Some("running"), None)
                .is_empty()
        }),
        Arc::new(move |kind, payload| {
            let Ok(mut execution_id) = update_execution.lock() else {
                return;
            };
            if kind == "relay_update_check_started" || execution_id.is_none() {
                *execution_id = new_execution_id().ok();
            }
            if let Some(execution_id) = execution_id.as_deref() {
                let _ = update_audit_state.store.add(relay_event(
                    kind,
                    "relay-update",
                    execution_id,
                    payload,
                ));
            }
        }),
        update_arguments,
        config.secret_file.clone(),
        config.port,
    );
    log::info!(
        "started update manager (directory={}, interval={:?})",
        config.update_directory.display(),
        config.update_interval
    );
    log::info!("zigzag startup complete");
    loop {
        thread::park();
    }
}
