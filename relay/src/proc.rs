use crate::events::{
    persist_first_output, random_hex_128, relay_event, relay_timestamp, unix_timestamp,
};
use crate::exec;
use crate::routes::procs::update_proc_status_with_handle;
use crate::server::{Server, Supervisor};
use crate::session::CappedOutput;
use relay_core::{AgentRecord, AgentRegistry, Json, Store};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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
    pub(crate) stdout_path: Option<PathBuf>,
    pub(crate) stderr_path: Option<PathBuf>,
    pub(crate) stdout_read: u64,
    pub(crate) stderr_read: u64,
}
pub(crate) struct SpawnedProc {
    pub(crate) id: String,
    pub(crate) handle: String,
}
/// Extra metadata recorded on the agent registry entry when spawning.
#[derive(Default)]
pub(crate) struct AgentSpawnDetails {
    pub(crate) worktree_path: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) harness_config: Option<String>,
    pub(crate) deadline_at: Option<String>,
}

/// Durable JSONL transcript for an API-created agent.
///
/// This is deliberately independent of the agent registry's bounded
/// diagnostics spool: users need the complete Codex event stream after the
/// process has exited or the relay has restarted.
pub(crate) fn agent_transcript_path(agent_id: &str) -> Option<PathBuf> {
    if agent_id.len() != 32 || !agent_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".zigzag/agents/codex")
            .join(format!("{agent_id}.jsonl")),
    )
}

/// Stderr is kept beside the JSONL transcript rather than in a pipe owned by
/// the relay.  A relay replacement (or a crash) closes every pipe it owns;
/// leaving a long-running Codex process with a pipe for stderr makes its next
/// diagnostic write raise SIGPIPE and abort the task.
pub(crate) fn agent_stderr_path(agent_id: &str) -> Option<PathBuf> {
    let transcript = agent_transcript_path(agent_id)?;
    Some(transcript.with_extension("stderr"))
}

fn create_agent_output(path: PathBuf) -> Result<File, String> {
    let parent = path
        .parent()
        .expect("agent transcript path always has a parent");
    fs::create_dir_all(parent).map_err(|_| "could not create agent transcript directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|_| "could not create agent output".to_owned())
}

fn create_agent_transcript(agent_id: &str) -> Result<File, String> {
    let path = agent_transcript_path(agent_id)
        .ok_or_else(|| "could not construct agent transcript path".to_owned())?;
    create_agent_output(path)
}

fn create_agent_stderr(agent_id: &str) -> Result<File, String> {
    let path = agent_stderr_path(agent_id)
        .ok_or_else(|| "could not construct agent stderr path".to_owned())?;
    create_agent_output(path)
}
pub(crate) fn spawn_proc(
    supervisor: &Supervisor,
    store: Arc<Store>,
    path: &Path,
    request: exec::ExecRequest,
    execution_id: String,
    details: AgentSpawnDetails,
) -> Result<SpawnedProc, String> {
    let stdout = Arc::new(Mutex::new(CappedOutput::default()));
    let stderr = Arc::new(Mutex::new(CappedOutput::default()));
    // Allocate the agent handle before launching so the child can write to a
    // filename keyed by that handle rather than the (non-unique) task id.
    let handle = {
        let mut table = supervisor
            .procs
            .lock()
            .map_err(|_| "process table lock poisoned".to_owned())?;
        prune_procs(&mut table, Instant::now());
        unique_handle(&table)?
    };
    let transcript_path = agent_transcript_path(&handle)
        .ok_or_else(|| "could not construct agent transcript path".to_owned())?;
    let stderr_path = agent_stderr_path(&handle)
        .ok_or_else(|| "could not construct agent stderr path".to_owned())?;
    let transcript = create_agent_transcript(&handle)?;
    let durable_stderr = match create_agent_stderr(&handle) {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&transcript_path);
            return Err(error);
        }
    };
    let stdout_stdio = File::try_clone(&transcript)
        .map(Stdio::from)
        .map_err(|_| "could not open agent transcript".to_owned())?;
    let stderr_stdio = File::try_clone(&durable_stderr)
        .map(Stdio::from)
        .map_err(|_| "could not open agent stderr".to_owned())?;
    let mut command = Command::new(path);
    if let Some(directory) = details.working_dir.as_deref() {
        command.current_dir(directory);
    }
    if let Some(config) = details.harness_config.as_deref() {
        command.env("OPENCODE_CONFIG_CONTENT", config);
    }
    command
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(stdout_stdio)
        // Both streams are durable, never relay-owned pipes.
        .stderr(stderr_stdio);
    // Do not merely create a process group: launchd and terminal teardown can
    // still deliver session-level hangups to it. A new session isolates the
    // task from the relay while retaining a stable PGID for pause/kill and
    // restart recovery. `setsid` also makes the leader PID its process-group
    // ID, as required by the durable registry.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| error.to_string())?;
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
    let id = request.id;
    let record = AgentRecord {
        id: handle.clone(),
        task_id: id.clone(),
        execution_id: execution_id.clone(),
        leader_pid: process_group,
        process_group,
        process_identity: Some(process_identity),
        worktree_path: details.worktree_path,
        started_at: unix_timestamp(),
        deadline_at: details.deadline_at,
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
        restarted_from: None,
        agent_config: None,
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
    // File streams do not need a relay reader to remain valid across a
    // restart. The reaper tails them into the bounded diagnostics spool.
    stdout
        .lock()
        .expect("stdout capture lock poisoned")
        .complete = true;
    stderr
        .lock()
        .expect("stderr capture lock poisoned")
        .complete = true;
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
            stdout_path: Some(transcript_path),
            stderr_path: Some(stderr_path),
            stdout_read: 0,
            stderr_read: 0,
        },
    );
    prune_procs(&mut table, Instant::now());
    Ok(SpawnedProc { id, handle })
}
pub(crate) fn output_is_complete(entry: &ProcEntry) -> bool {
    entry.stdout.lock().is_ok_and(|output| output.complete)
        && entry.stderr.lock().is_ok_and(|output| output.complete)
}

/// Copy newly appended durable output into the compatibility capture and
/// registry spool. The files remain the source of truth across a relay crash;
/// this tailer merely restores live log streaming while the relay is present.
pub(crate) fn sync_durable_output(
    entry: &mut ProcEntry,
    handle: &str,
    registry: &AgentRegistry,
    store: &Store,
) {
    sync_durable_stream(
        entry.stdout_path.as_deref(),
        &mut entry.stdout_read,
        &entry.stdout,
        registry,
        store,
        handle,
        "stdout",
    );
    sync_durable_stream(
        entry.stderr_path.as_deref(),
        &mut entry.stderr_read,
        &entry.stderr,
        registry,
        store,
        handle,
        "stderr",
    );
}

fn sync_durable_stream(
    path: Option<&Path>,
    read: &mut u64,
    capture: &Arc<Mutex<CappedOutput>>,
    registry: &AgentRegistry,
    store: &Store,
    agent_id: &str,
    stream: &'static str,
) {
    let Some(path) = path else {
        return;
    };
    let Ok(mut file) = File::open(path) else {
        return;
    };
    if file.seek(SeekFrom::Start(*read)).is_err() {
        return;
    }
    let mut chunk = [0u8; 8192];
    loop {
        let Ok(count) = file.read(&mut chunk) else {
            return;
        };
        if count == 0 {
            return;
        }
        *read += count as u64;
        if let Ok(mut output) = capture.lock() {
            output.append(&chunk[..count]);
        } else {
            return;
        }
        let _ = registry.append_log(agent_id, stream, &chunk[..count]);
        if let Ok(Some(agent)) =
            registry.record_first_output(agent_id, &relay_timestamp(), stream, count as u64)
            && persist_first_output(store, &agent).is_err()
        {
            let _ = registry.mark_audit_degraded(agent_id);
        }
    }
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
                    sync_durable_output(entry, handle, &state.supervisor.registry, &state.store);
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
