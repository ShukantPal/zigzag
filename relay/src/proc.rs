use crate::events::{
    persist_first_output, random_hex_128, relay_event, relay_timestamp, unix_timestamp,
};
use crate::exec;
use crate::routes::procs::update_proc_status_with_handle;
use crate::server::{Server, Supervisor};
use crate::session::CappedOutput;
use relay_core::{AgentRecord, AgentRegistry, Json, Store};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const MAX_FINISHED_PROCS: usize = 128;
pub(crate) const FINISHED_PROC_RETENTION: Duration = Duration::from_secs(60 * 60);
pub(crate) const COMPAT_OUTPUT_CAP: usize = 2 * 1024 * 1024;
pub(crate) struct ProcEntry {
    pub(crate) child: Child,
    pub(crate) process_group: i32,
    pub(crate) id: String,
    pub(crate) bin: String,
    pub(crate) subcommand: String,
    pub(crate) spawned_at: Instant,
    pub(crate) finished_at: Option<Instant>,
    pub(crate) termination_requested_at: Option<Instant>,
    pub(crate) termination_escalated: bool,
    pub(crate) leader_reaped: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Arc<Mutex<CappedOutput>>,
    pub(crate) stderr: Arc<Mutex<CappedOutput>>,
}
pub(crate) struct SpawnedProc {
    pub(crate) id: String,
    pub(crate) handle: String,
}
pub(crate) fn spawn_proc(
    supervisor: &Supervisor,
    store: Arc<Store>,
    path: &Path,
    request: exec::ExecRequest,
    execution_id: String,
) -> Result<SpawnedProc, String> {
    let stdout = Arc::new(Mutex::new(CappedOutput::default()));
    let stderr = Arc::new(Mutex::new(CappedOutput::default()));
    let mut child = Command::new(path)
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A dedicated process group makes kill requests cover the command's
        // descendants without involving the relay itself.
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    let child_stdout = child.stdout.take().expect("stdout was piped");
    let child_stderr = child.stderr.take().expect("stderr was piped");
    let process_group = child.id() as i32;
    let Some(process_identity) = process_identity(process_group) else {
        let _ = force_kill_process_group(process_group);
        return Err("could not record spawned process identity".to_owned());
    };
    let mut table = supervisor
        .procs
        .lock()
        .map_err(|_| "process table lock poisoned".to_owned())?;
    prune_procs(&mut table, Instant::now());
    let handle = unique_handle(&table)?;
    let id = request.id;
    let record = AgentRecord {
        id: handle.clone(),
        task_id: id.clone(),
        execution_id: execution_id.clone(),
        leader_pid: process_group,
        process_group,
        process_identity: Some(process_identity),
        started_at: unix_timestamp(),
        deadline_at: None,
        command: format!(
            "{} {}",
            request.bin,
            request.args.first().map(String::as_str).unwrap_or("")
        ),
        state: "running".to_owned(),
        paused_at: None,
        exit_code: None,
        log_degraded: false,
        audit_degraded: false,
        redacted: false,
        stdout_next: 0,
        stderr_next: 0,
        stdout_dropped_before: 0,
        stderr_dropped_before: 0,
        log_next: 0,
        log_dropped_before: 0,
        first_output_at: None,
        first_output_stream: None,
        first_output_bytes: None,
    };
    // The registry transition commits before this spawn can be acknowledged.
    if let Err(error) = supervisor.registry.register(record) {
        let _ = kill_process_group(process_group);
        return Err(error);
    }
    if let Err(error) = store.add(relay_event(
        "process_spawned",
        &id,
        &execution_id,
        Json::Object(vec![("agent_id".to_owned(), Json::String(handle.clone()))]),
    )) {
        let _ = supervisor
            .registry
            .transition(&handle, "audit_failed", None);
        let _ = kill_process_group(process_group);
        return Err(error);
    }
    drain_to_capture(
        child_stdout,
        Arc::clone(&stdout),
        Arc::clone(&supervisor.registry),
        Arc::clone(&store),
        handle.clone(),
        "stdout",
    );
    drain_to_capture(
        child_stderr,
        Arc::clone(&stderr),
        Arc::clone(&supervisor.registry),
        store,
        handle.clone(),
        "stderr",
    );
    table.insert(
        handle.clone(),
        ProcEntry {
            child,
            process_group,
            id: id.clone(),
            bin: request.bin,
            subcommand: request.args.first().cloned().unwrap_or_default(),
            spawned_at: Instant::now(),
            finished_at: None,
            termination_requested_at: None,
            termination_escalated: false,
            leader_reaped: false,
            exit_code: None,
            stdout,
            stderr,
        },
    );
    prune_procs(&mut table, Instant::now());
    Ok(SpawnedProc { id, handle })
}
pub(crate) fn drain_to_capture(
    mut pipe: impl Read + Send + 'static,
    capture: Arc<Mutex<CappedOutput>>,
    registry: Arc<AgentRegistry>,
    store: Arc<Store>,
    agent_id: String,
    stream: &'static str,
) {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut output) = capture.lock() {
                        output.append(&chunk[..n]);
                    } else {
                        break;
                    }
                    // A spool failure never stops pipe draining; it is recorded
                    // as log_degraded and retried on the next chunk.
                    let _ = registry.append_log(&agent_id, stream, &chunk[..n]);
                    if let Ok(Some(agent)) = registry.record_first_output(
                        &agent_id,
                        &relay_timestamp(),
                        stream,
                        n as u64,
                    ) && persist_first_output(&store, &agent).is_err()
                    {
                        let _ = registry.mark_audit_degraded(&agent_id);
                    }
                }
            }
        }
        if let Ok(mut output) = capture.lock() {
            output.complete = true;
        }
    });
}
pub(crate) fn output_is_complete(entry: &ProcEntry) -> bool {
    entry.stdout.lock().is_ok_and(|output| output.complete)
        && entry.stderr.lock().is_ok_and(|output| output.complete)
}
pub(crate) fn proc_json(entry: &ProcEntry) -> Json {
    let (stdout, stdout_truncated) = entry
        .stdout
        .lock()
        .map(|output| output.snapshot())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = entry
        .stderr
        .lock()
        .map(|output| output.snapshot())
        .unwrap_or_default();
    Json::Object(vec![
        ("id".to_owned(), Json::String(entry.id.clone())),
        (
            "running".to_owned(),
            Json::Bool(entry.finished_at.is_none()),
        ),
        (
            "exit_code".to_owned(),
            entry
                .exit_code
                .map_or(Json::Null, |code| Json::Number(code.to_string())),
        ),
        ("stdout".to_owned(), Json::String(stdout)),
        ("stderr".to_owned(), Json::String(stderr)),
        (
            "truncated".to_owned(),
            Json::Bool(stdout_truncated || stderr_truncated),
        ),
    ])
}
pub(crate) fn kill_process_group(process_group: i32) -> bool {
    // `process_group(0)` above creates a group whose id is the child PID.
    unsafe { libc::kill(-process_group, libc::SIGTERM) == 0 }
}
pub(crate) fn force_kill_process_group(process_group: i32) -> bool {
    // Review-loop cleanup is terminal: obsolete reviewers and owners must not
    // survive supersession or merge, including shells that ignore SIGTERM.
    let terminated = kill_process_group(process_group);
    let killed = unsafe { libc::kill(-process_group, libc::SIGKILL) == 0 };
    terminated || killed
}
pub(crate) fn process_group_running(process_group: i32) -> bool {
    unsafe { libc::kill(-process_group, 0) == 0 }
}
#[cfg(target_os = "macos")]
pub(crate) fn process_identity(pid: i32) -> Option<String> {
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as i32,
        )
    };
    (written as usize == size)
        .then(|| format!("macos:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}
#[cfg(target_os = "linux")]
pub(crate) fn process_identity(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_command = stat.get(stat.rfind(')')? + 2..)?;
    let start_ticks = after_command.split_whitespace().nth(19)?;
    Some(format!("linux:{start_ticks}"))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn process_identity(_pid: i32) -> Option<String> {
    None
}
pub(crate) fn recovered_agent_identity_matches(agent: &AgentRecord) -> bool {
    process_group_running(agent.process_group)
        && agent
            .process_identity
            .as_deref()
            .zip(process_identity(agent.leader_pid).as_deref())
            .is_some_and(|(expected, current)| expected == current)
}
pub(crate) fn managed_agent_running(agent: &AgentRecord) -> bool {
    managed_agent_running_with(agent, recovered_agent_identity_matches)
}
pub(crate) fn managed_agent_running_with(
    agent: &AgentRecord,
    orphan_is_current: impl Fn(&AgentRecord) -> bool,
) -> bool {
    match agent.state.as_str() {
        "running" => true,
        "orphaned" => orphan_is_current(agent),
        _ => false,
    }
}
pub(crate) fn unique_handle(entries: &HashMap<String, ProcEntry>) -> Result<String, String> {
    loop {
        let handle = random_hex_128()?;
        if !entries.contains_key(&handle) {
            return Ok(handle);
        }
    }
}
pub(crate) fn prune_procs(entries: &mut HashMap<String, ProcEntry>, now: Instant) {
    // The independent reaper does durable transitions. This compatibility
    // pruning pass only bounds completed in-memory handles.
    entries.retain(|_, entry| {
        entry
            .finished_at
            .is_none_or(|finished| now.duration_since(finished) <= FINISHED_PROC_RETENTION)
    });
    let mut finished: Vec<_> = entries
        .iter()
        .filter_map(|(handle, entry)| entry.finished_at.map(|finished| (handle.clone(), finished)))
        .collect();
    finished.sort_by_key(|(handle, finished)| (*finished, entries[handle].spawned_at));
    let excess = finished.len().saturating_sub(MAX_FINISHED_PROCS);
    for (handle, _) in finished.into_iter().take(excess) {
        entries.remove(&handle);
    }
}
pub(crate) fn start_reaper(state: Arc<Server>) {
    thread::spawn(move || {
        loop {
            if let Ok(mut entries) = state.supervisor.procs.lock() {
                for (handle, entry) in entries.iter_mut() {
                    update_proc_status_with_handle(
                        entry,
                        handle,
                        &state.supervisor.registry,
                        &state.store,
                    );
                }
                prune_procs(&mut entries, Instant::now());
            }
            let _ = state
                .supervisor
                .registry
                .prune(unix_timestamp().parse().unwrap_or_default());
            thread::sleep(Duration::from_millis(200));
        }
    });
}
