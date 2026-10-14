//! Shared durable queue and JSON support for Zigzag.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Durable, deliberately small record for a supervised agent.  Command
/// arguments and prompt text are intentionally not retained here.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRecord {
    pub id: String,
    pub task_id: String,
    /// A relay-generated attempt identifier.  `task_id` may be retried, while
    /// this value identifies one concrete supervised process.
    pub execution_id: String,
    pub leader_pid: i32,
    pub process_group: i32,
    /// OS-reported process birth identity. Unlike a PID/PGID, this changes
    /// when the operating system reuses a numeric process identifier.
    pub process_identity: Option<String>,
    /// Filesystem path of the agent's worktree, when the agent was created
    /// with one. Deleting an agent never removes its worktree.
    pub worktree_path: Option<String>,
    pub started_at: String,
    pub deadline_at: Option<String>,
    pub command: String,
    pub state: String,
    /// RFC 3339 timestamp of the most recent pause; `None` when the agent is
    /// not paused.  A paused agent's process group is stopped with SIGSTOP.
    pub paused_at: Option<String>,
    pub exit_code: Option<i32>,
    pub log_degraded: bool,
    pub audit_degraded: bool,
    pub redacted: bool,
    pub stdout_next: u64,
    pub stderr_next: u64,
    pub stdout_dropped_before: u64,
    pub stderr_dropped_before: u64,
    pub log_next: u64,
    pub log_dropped_before: u64,
    pub first_output_at: Option<String>,
    pub first_output_stream: Option<String>,
    pub first_output_bytes: Option<u64>,
    /// Handle of the agent this one was restarted from, if any.
    pub restarted_from: Option<String>,
    /// JSON blob of the original agent-creation parameters
    /// (prompt, project_dir, branch, worktree, model, approval_mode,
    /// timeout_secs), enabling `POST /v1/agents/{id}/restart`.
    pub agent_config: Option<String>,
    /// Native harness thread/session identifier. A thread ID is a routing key,
    /// not a tenant security boundary.
    pub harness_session_id: Option<String>,
    pub harness: Option<String>,
}

impl AgentRecord {
    pub fn status_json(&self) -> Json {
        Json::Object(vec![
            ("id".to_owned(), Json::String(self.id.clone())),
            ("task_id".to_owned(), Json::String(self.task_id.clone())),
            (
                "execution_id".to_owned(),
                Json::String(self.execution_id.clone()),
            ),
            ("state".to_owned(), Json::String(self.state.clone())),
            (
                "paused_at".to_owned(),
                self.paused_at
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
            (
                "started_at".to_owned(),
                Json::String(self.started_at.clone()),
            ),
            (
                "deadline_at".to_owned(),
                self.deadline_at
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
            ("command".to_owned(), Json::String(self.command.clone())),
            (
                "exit_code".to_owned(),
                self.exit_code
                    .map(|v| Json::Number(v.to_string()))
                    .unwrap_or(Json::Null),
            ),
            ("log_degraded".to_owned(), Json::Bool(self.log_degraded)),
            ("audit_degraded".to_owned(), Json::Bool(self.audit_degraded)),
            ("redacted".to_owned(), Json::Bool(self.redacted)),
            (
                "stdout_dropped_before".to_owned(),
                Json::number(self.stdout_dropped_before),
            ),
            (
                "stderr_dropped_before".to_owned(),
                Json::number(self.stderr_dropped_before),
            ),
            (
                "dropped_before".to_owned(),
                Json::number(self.log_dropped_before),
            ),
            (
                "restarted_from".to_owned(),
                self.restarted_from
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
            (
                "restartable".to_owned(),
                Json::Bool(self.agent_config.is_some()),
            ),
            (
                "harness_session_id".to_owned(),
                self.harness_session_id
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
            (
                "harness".to_owned(),
                self.harness.clone().map(Json::String).unwrap_or(Json::Null),
            ),
        ])
    }
}

/// Registry state is atomically replaced independently of the event queue.
/// State transitions are persisted by `transition` before callers emit their
/// matching lifecycle event.
pub struct AgentRegistry {
    path: PathBuf,
    spool_dir: PathBuf,
    inner: Mutex<std::collections::BTreeMap<String, AgentRecord>>,
}

const AGENT_LOG_CAP: u64 = 32 * 1024 * 1024;
const AGENT_RETENTION: u64 = 7 * 24 * 60 * 60;

impl AgentRegistry {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let spool_dir = path.with_extension("agent-logs");
        fs::create_dir_all(&spool_dir)
            .map_err(|_| "could not initialize agent diagnostics".to_owned())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&spool_dir, fs::Permissions::from_mode(0o700));
        }
        let records = match fs::read_to_string(&path) {
            Ok(text) => {
                let (records, skipped) = decode_agents(&text);
                for warning in skipped {
                    eprintln!("zigzag: skipping agent registry record: {}", warning);
                }
                records
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(_) => return Err("could not read agent registry".to_owned()),
        };
        Ok(Self {
            path,
            spool_dir,
            inner: Mutex::new(records),
        })
    }

    pub fn register(&self, record: AgentRecord) -> Result<(), String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        if entries.contains_key(&record.id) {
            return Err("duplicate agent id".to_owned());
        }
        let mut updated = entries.clone();
        updated.insert(record.id.clone(), record);
        self.save(&updated)?;
        *entries = updated;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<AgentRecord> {
        self.inner.lock().ok()?.get(id).cloned()
    }

    pub fn list(&self, state: Option<&str>, task_id: Option<&str>) -> Vec<AgentRecord> {
        self.inner
            .lock()
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| {
                        state.is_none_or(|value| entry.state == value)
                            && task_id.is_none_or(|value| entry.task_id == value)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn transition(
        &self,
        id: &str,
        state: &str,
        exit_code: Option<i32>,
    ) -> Result<Option<AgentRecord>, String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let Some(old) = entries.get(id) else {
            return Ok(None);
        };
        if old.state == state && old.exit_code == exit_code {
            return Ok(Some(old.clone()));
        }
        let mut updated = entries.clone();
        let entry = updated.get_mut(id).expect("entry cloned");
        entry.state = state.to_owned();
        entry.exit_code = exit_code;
        let result = entry.clone();
        self.save(&updated)?;
        *entries = updated;
        Ok(Some(result))
    }

    /// Persist the first observed byte so a transient archive failure can be
    /// retried by later output or the terminal reaper.
    pub fn record_first_output(
        &self,
        id: &str,
        occurred_at: &str,
        stream: &str,
        bytes: u64,
    ) -> Result<Option<AgentRecord>, String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let Some(old) = entries.get(id) else {
            return Ok(None);
        };
        if old.first_output_at.is_some() {
            return Ok(Some(old.clone()));
        }
        let mut updated = entries.clone();
        let entry = updated.get_mut(id).expect("entry cloned");
        entry.first_output_at = Some(occurred_at.to_owned());
        entry.first_output_stream = Some(stream.to_owned());
        entry.first_output_bytes = Some(bytes);
        let result = entry.clone();
        self.save(&updated)?;
        *entries = updated;
        Ok(Some(result))
    }

    pub fn mark_audit_degraded(&self, id: &str) -> Result<(), String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let Some(old) = entries.get(id) else {
            return Ok(());
        };
        if old.audit_degraded {
            return Ok(());
        }
        let mut updated = entries.clone();
        updated.get_mut(id).expect("entry cloned").audit_degraded = true;
        self.save(&updated)?;
        *entries = updated;
        Ok(())
    }

    /// Record (or clear, with `None`) the agent's pause marker.  The marker
    /// is persisted so a relay restart can re-apply SIGSTOP to a paused
    /// agent whose process group is still alive.
    pub fn set_paused_at(&self, id: &str, paused_at: Option<String>) -> Result<(), String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let Some(old) = entries.get(id) else {
            return Ok(());
        };
        if old.paused_at == paused_at {
            return Ok(());
        }
        let mut updated = entries.clone();
        updated.get_mut(id).expect("entry cloned").paused_at = paused_at;
        self.save(&updated)?;
        *entries = updated;
        Ok(())
    }

    /// Record restart linkage on a freshly spawned agent: which agent it was
    /// restarted from, and carry over the original creation config so the new
    /// agent can itself be restarted later.
    pub fn set_restart_metadata(
        &self,
        id: &str,
        restarted_from: &str,
        agent_config: Option<&str>,
    ) -> Result<Option<AgentRecord>, String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        if !entries.contains_key(id) {
            return Ok(None);
        }
        let mut updated = entries.clone();
        let entry = updated.get_mut(id).expect("entry cloned");
        entry.restarted_from = Some(restarted_from.to_owned());
        entry.agent_config = agent_config.map(str::to_owned);
        let result = entry.clone();
        self.save(&updated)?;
        *entries = updated;
        Ok(Some(result))
    }

    /// Persist the original creation parameters for a newly-created agent so
    /// a later restart can reconstruct its managed Codex invocation.
    pub fn set_agent_config(
        &self,
        id: &str,
        agent_config: &str,
    ) -> Result<Option<AgentRecord>, String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let Some(old) = entries.get(id) else {
            return Ok(None);
        };
        if old.agent_config.as_deref() == Some(agent_config) {
            return Ok(Some(old.clone()));
        }
        let mut updated = entries.clone();
        let entry = updated.get_mut(id).expect("entry cloned");
        entry.agent_config = Some(agent_config.to_owned());
        let result = entry.clone();
        self.save(&updated)?;
        *entries = updated;
        Ok(Some(result))
    }
    pub fn set_harness_session(
        &self,
        id: &str,
        harness: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned")?;
        let Some(old) = entries.get(id) else {
            return Ok(());
        };
        if old.harness.as_deref() == Some(harness)
            && old.harness_session_id.as_deref() == Some(session_id)
        {
            return Ok(());
        }
        let mut updated = entries.clone();
        let entry = updated.get_mut(id).expect("entry cloned");
        entry.harness = Some(harness.to_owned());
        entry.harness_session_id = Some(session_id.to_owned());
        self.save(&updated)?;
        *entries = updated;
        Ok(())
    }
    /// Recover formerly live agents after a relay restart. The caller supplies
    /// a non-signalling identity probe. Live, identity-verified processes remain
    /// running; native harness sessions are resumed through their durable
    /// session history instead of probing the shared app-server process. The
    /// returned records are replayed into the lifecycle event store.
    pub fn recover<F>(&self, process_is_current: F) -> Result<Vec<AgentRecord>, String>
    where
        F: Fn(&AgentRecord) -> bool,
    {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let mut updated = entries.clone();
        let mut changed = Vec::new();
        let mut state_changed = false;
        for entry in updated.values_mut() {
            if matches!(entry.state.as_str(), "running" | "orphaned") {
                // Native sessions are recovered through their durable harness
                // history after the app-server reconnects; their routing key
                // is not an OS process identity.
                if entry.harness_session_id.is_some() {
                    continue;
                }
                let recovered_state = if process_is_current(entry) {
                    "running"
                } else {
                    "lost_after_restart"
                };
                if entry.state != recovered_state {
                    entry.state = recovered_state.to_owned();
                    state_changed = true;
                }
                changed.push(entry.clone());
            }
        }
        if state_changed {
            self.save(&updated)?;
            *entries = updated;
        }
        Ok(changed)
    }

    pub fn prune(&self, now_seconds: u64) -> Result<Vec<String>, String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let mut updated = entries.clone();
        let removed: Vec<_> = updated
            .iter()
            .filter(|(_, entry)| {
                entry.state != "running"
                    && entry.state != "orphaned"
                    && entry
                        .started_at
                        .parse::<u64>()
                        .is_ok_and(|started| now_seconds.saturating_sub(started) > AGENT_RETENTION)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &removed {
            updated.remove(id);
        }
        if !removed.is_empty() {
            self.save(&updated)?;
            *entries = updated;
            for id in &removed {
                let _ = fs::remove_file(self.log_path(id));
            }
        }
        Ok(removed)
    }

    pub fn append_log(&self, id: &str, stream: &str, bytes: &[u8]) -> Result<(), String> {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let mut updated = entries.clone();
        let Some(entry) = updated.get_mut(id) else {
            return Ok(());
        };
        let next = if stream == "stdout" {
            &mut entry.stdout_next
        } else {
            &mut entry.stderr_next
        };
        let cursor = entry.log_next;
        entry.log_next += bytes.len() as u64;
        *next += bytes.len() as u64;
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        let redacted = redact(&mut text);
        entry.redacted |= redacted;
        let line = Json::Object(vec![
            ("stream".to_owned(), Json::String(stream.to_owned())),
            ("cursor".to_owned(), Json::number(cursor)),
            ("data".to_owned(), Json::String(text)),
        ])
        .to_json()
            + "\n";
        let log_path = self.log_path(id);
        let write = (|| -> std::io::Result<()> {
            if !log_path.exists() {
                let _ = create_private(&log_path)?;
            }
            let mut file = OpenOptions::new().append(true).open(&log_path)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()
        })();
        if write.is_err() {
            entry.log_degraded = true;
            self.save(&updated)?;
            *entries = updated;
            return Err("agent log spool is unavailable".to_owned());
        }
        let length = fs::metadata(&log_path).map(|v| v.len()).unwrap_or(0);
        if length > AGENT_LOG_CAP {
            let content = fs::read(&log_path).unwrap_or_default();
            let start = content.len().saturating_sub(AGENT_LOG_CAP as usize);
            let keep_from = content[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(content.len(), |offset| start + offset + 1);
            let kept = &content[keep_from..];
            if let Some(Json::Object(fields)) = std::str::from_utf8(kept)
                .ok()
                .and_then(|v| v.lines().next())
                .and_then(|v| parse_json(v).ok())
                && let Some(value) = Json::Object(fields).object("cursor").and_then(Json::as_u64)
            {
                entry.log_dropped_before = entry.log_dropped_before.max(value);
            }
            let temporary = log_path.with_extension("tmp");
            fs::write(&temporary, kept).map_err(|_| "agent log spool is unavailable".to_owned())?;
            fs::rename(temporary, &log_path)
                .map_err(|_| "agent log spool is unavailable".to_owned())?;
        }
        self.save(&updated)?;
        *entries = updated;
        Ok(())
    }

    pub fn logs_json(
        &self,
        id: &str,
        stream: &str,
        after: u64,
        tail: Option<usize>,
    ) -> Option<Json> {
        let entries = self.inner.lock().ok()?;
        let entry = entries.get(id)?.clone();
        let content = fs::read_to_string(self.log_path(id)).unwrap_or_default();
        let mut values: Vec<Json> = content
            .lines()
            .filter_map(|line| parse_json(line).ok())
            .filter(|record| {
                let correct_stream = stream == "both"
                    || record.object("stream").and_then(Json::as_str) == Some(stream);
                correct_stream
                    && record
                        .object("cursor")
                        .and_then(Json::as_u64)
                        .is_some_and(|cursor| cursor >= after)
            })
            .collect();
        if let Some(tail) = tail {
            let mut count = 0;
            values.reverse();
            values.retain(|record| {
                count += record
                    .object("data")
                    .and_then(Json::as_str)
                    .map_or(0, str::len);
                count <= tail
            });
            values.reverse();
        }
        let (next_cursor, dropped_before) = (entry.log_next, entry.log_dropped_before);
        Some(Json::Object(vec![
            ("records".to_owned(), Json::Array(values)),
            ("next_cursor".to_owned(), Json::number(next_cursor)),
            ("dropped_before".to_owned(), Json::number(dropped_before)),
            ("complete".to_owned(), Json::Bool(entry.state != "running")),
            ("log_degraded".to_owned(), Json::Bool(entry.log_degraded)),
        ]))
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.spool_dir.join(format!("{id}.jsonl"))
    }
    fn save(
        &self,
        entries: &std::collections::BTreeMap<String, AgentRecord>,
    ) -> Result<(), String> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|_| "could not persist agent registry".to_owned())?;
        let temporary = parent.join(format!(
            ".zigzag-agents-{}-{}.tmp",
            std::process::id(),
            TEMPORARY_FILE_SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let json = Json::Object(vec![(
            "agents".to_owned(),
            Json::Array(entries.values().map(agent_json).collect()),
        )])
        .to_json();
        let result = (|| -> Result<(), String> {
            let mut file = create_private(&temporary)
                .map_err(|_| "could not persist agent registry".to_owned())?;
            file.write_all(json.as_bytes())
                .map_err(|_| "could not persist agent registry".to_owned())?;
            file.sync_all()
                .map_err(|_| "could not persist agent registry".to_owned())?;
            fs::rename(&temporary, &self.path)
                .map_err(|_| "could not persist agent registry".to_owned())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

fn agent_json(entry: &AgentRecord) -> Json {
    let fields: Vec<(String, Json)> = vec![
        ("id".to_owned(), Json::String(entry.id.clone())),
        ("task_id".to_owned(), Json::String(entry.task_id.clone())),
        (
            "execution_id".to_owned(),
            Json::String(entry.execution_id.clone()),
        ),
        (
            "leader_pid".to_owned(),
            Json::Number(entry.leader_pid.to_string()),
        ),
        (
            "process_group".to_owned(),
            Json::Number(entry.process_group.to_string()),
        ),
        (
            "process_identity".to_owned(),
            entry
                .process_identity
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "worktree_path".to_owned(),
            entry
                .worktree_path
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "started_at".to_owned(),
            Json::String(entry.started_at.clone()),
        ),
        (
            "deadline_at".to_owned(),
            entry
                .deadline_at
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        ("command".to_owned(), Json::String(entry.command.clone())),
        ("state".to_owned(), Json::String(entry.state.clone())),
        (
            "paused_at".to_owned(),
            entry
                .paused_at
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "exit_code".to_owned(),
            entry
                .exit_code
                .map(|v| Json::Number(v.to_string()))
                .unwrap_or(Json::Null),
        ),
        ("log_degraded".to_owned(), Json::Bool(entry.log_degraded)),
        (
            "audit_degraded".to_owned(),
            Json::Bool(entry.audit_degraded),
        ),
        ("redacted".to_owned(), Json::Bool(entry.redacted)),
        ("stdout_next".to_owned(), Json::number(entry.stdout_next)),
        ("stderr_next".to_owned(), Json::number(entry.stderr_next)),
        (
            "stdout_dropped_before".to_owned(),
            Json::number(entry.stdout_dropped_before),
        ),
        (
            "stderr_dropped_before".to_owned(),
            Json::number(entry.stderr_dropped_before),
        ),
        ("log_next".to_owned(), Json::number(entry.log_next)),
        (
            "log_dropped_before".to_owned(),
            Json::number(entry.log_dropped_before),
        ),
        (
            "first_output_at".to_owned(),
            entry
                .first_output_at
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "first_output_stream".to_owned(),
            entry
                .first_output_stream
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "first_output_bytes".to_owned(),
            entry
                .first_output_bytes
                .map(Json::number)
                .unwrap_or(Json::Null),
        ),
        (
            "restarted_from".to_owned(),
            entry
                .restarted_from
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "agent_config".to_owned(),
            entry
                .agent_config
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "harness_session_id".to_owned(),
            entry
                .harness_session_id
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
        (
            "harness".to_owned(),
            entry
                .harness
                .clone()
                .map(Json::String)
                .unwrap_or(Json::Null),
        ),
    ];
    // Omit null fields instead of writing explicit nulls.
    // The reader handles both forms, but omitting avoids round-trip
    // issues with parsers that do not expect explicit nulls.
    Json::Object(
        fields
            .into_iter()
            .filter(|(_, v)| !matches!(v, Json::Null))
            .collect(),
    )
}

fn decode_agents(text: &str) -> (std::collections::BTreeMap<String, AgentRecord>, Vec<String>) {
    let mut skipped: Vec<String> = Vec::new();
    let values = match parse_json(text)
        .ok()
        .and_then(|v| v.object("agents").cloned())
    {
        Some(Json::Array(values)) => values,
        _ => {
            skipped.push("registry root: missing or invalid 'agents' array".to_owned());
            return (std::collections::BTreeMap::new(), skipped);
        }
    };
    let mut entries = std::collections::BTreeMap::new();
    for (idx, value) in values.iter().enumerate() {
        // Extract agent ID for error messages (best effort)
        let agent_id = value
            .object("id")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("<unknown id at index {}>", idx));
        match decode_agent_record(value, &agent_id) {
            Ok(record) => {
                entries.insert(record.id.clone(), record);
            }
            Err(reason) => {
                skipped.push(format!("agent '{}': {}", agent_id, reason));
            }
        }
    }
    (entries, skipped)
}

fn decode_agent_record(value: &Json, agent_id: &str) -> Result<AgentRecord, String> {
    let get = |key: &str| value.object(key);
    let err = |field: &str, reason: &str| -> String {
        format!("field '{}': {} (agent '{}')", field, reason, agent_id)
    };
    let text = |key: &str| {
        get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| err(key, "expected string, got null/missing/wrong type"))
    };
    // Registries written by older daemons encode some numeric fields as
    // strings.  Accept either form.
    let integer = |value: Option<&Json>| {
        value.and_then(|value| match value {
            Json::Number(_) => value.as_u64(),
            Json::String(text) => text.parse::<u64>().ok(),
            _ => None,
        })
    };
    let required_integer = |key: &str| {
        integer(get(key)).ok_or_else(|| err(key, "expected integer, got null/missing/wrong type"))
    };
    let optional_integer = |key: &str| match get(key) {
        Some(Json::Null) | None => Ok(None),
        value => integer(value)
            .map(Some)
            .ok_or_else(|| err(key, "expected integer or null")),
    };
    // Helper for optional string fields: accepts string, null, or missing
    let optional_string = |key: &str| match get(key) {
        Some(Json::String(value)) => Ok(Some(value.clone())),
        Some(Json::Null) | None => Ok(None),
        _ => Err(err(key, "expected string or null")),
    };
    let record = AgentRecord {
        id: text("id")?,
        task_id: text("task_id")?,
        // Registry files written before audit trails had no execution
        // identifier.  The agent handle is a safe one-to-one fallback.
        execution_id: get("execution_id")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| text("id").unwrap_or_default()),
        leader_pid: required_integer("leader_pid")? as i32,
        process_group: required_integer("process_group")? as i32,
        process_identity: optional_string("process_identity")?,
        worktree_path: optional_string("worktree_path")?,
        started_at: match get("started_at") {
            Some(Json::String(value)) => value.clone(),
            Some(Json::Number(value)) => value.clone(),
            _ => return Err(err("started_at", "expected string or number")),
        },
        deadline_at: optional_string("deadline_at")?,
        command: text("command")?,
        state: text("state")?,
        paused_at: optional_string("paused_at")?,
        exit_code: get("exit_code").and_then(Json::as_u64).map(|v| v as i32),
        log_degraded: get("log_degraded").and_then(Json::as_bool).unwrap_or(false),
        audit_degraded: get("audit_degraded")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        redacted: get("redacted").and_then(Json::as_bool).unwrap_or(false),
        stdout_next: required_integer("stdout_next")?,
        stderr_next: required_integer("stderr_next")?,
        stdout_dropped_before: required_integer("stdout_dropped_before")?,
        stderr_dropped_before: required_integer("stderr_dropped_before")?,
        log_next: required_integer("log_next")?,
        log_dropped_before: required_integer("log_dropped_before")?,
        first_output_at: optional_string("first_output_at")?,
        first_output_stream: optional_string("first_output_stream")?,
        first_output_bytes: optional_integer("first_output_bytes")?,
        restarted_from: optional_string("restarted_from")?,
        agent_config: optional_string("agent_config")?,
        harness_session_id: optional_string("harness_session_id")?,
        harness: optional_string("harness")?,
    };
    Ok(record)
}

fn redact(text: &mut String) -> bool {
    let original = text.clone();
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        *text = "[REDACTED PRIVATE KEY]".to_owned();
        return true;
    }
    for prefix in ["Bearer ", "bearer ", "sk-", "AKIA"] {
        while let Some(start) = text.find(prefix) {
            let end = text[start + prefix.len()..]
                .find(char::is_whitespace)
                .map(|v| start + prefix.len() + v)
                .unwrap_or(text.len());
            text.replace_range(start..end, "[REDACTED]");
        }
    }
    for key in ["API_KEY=", "api_key=", "token="] {
        if let Some(start) = text.find(key) {
            let end = text[start + key.len()..]
                .find(char::is_whitespace)
                .map(|v| start + key.len() + v)
                .unwrap_or(text.len());
            text.replace_range(start..end, "[REDACTED]");
        }
    }
    *text != original
}

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object(&self, name: &str) -> Option<&Json> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(value) = self {
            Some(value)
        } else {
            None
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        if let Self::Number(value) = self {
            value.parse().ok()
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(value) = self {
            Some(*value)
        } else {
            None
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&json_value(self)).expect("Json values are serializable")
    }

    pub fn number(value: u64) -> Self {
        Self::Number(value.to_string())
    }
}

pub fn parse_json(input: &str) -> Result<Json, String> {
    serde_json::from_str(input)
        .map(json_from_value)
        .map_err(|error| error.to_string())
}

pub fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("strings are serializable")
}

fn json_value(value: &Json) -> serde_json::Value {
    match value {
        Json::Null => serde_json::Value::Null,
        Json::Bool(value) => serde_json::Value::Bool(*value),
        Json::Number(value) => match serde_json::from_str(value) {
            Ok(serde_json::Value::Number(number)) => serde_json::Value::Number(number),
            _ => serde_json::Value::String(value.clone()),
        },
        Json::String(value) => serde_json::Value::String(value.clone()),
        Json::Array(values) => serde_json::Value::Array(values.iter().map(json_value).collect()),
        Json::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), json_value(value)))
                .collect(),
        ),
    }
}

fn json_from_value(value: serde_json::Value) -> Json {
    match value {
        serde_json::Value::Null => Json::Null,
        serde_json::Value::Bool(value) => Json::Bool(value),
        serde_json::Value::Number(value) => Json::Number(value.to_string()),
        serde_json::Value::String(value) => Json::String(value),
        serde_json::Value::Array(values) => {
            Json::Array(values.into_iter().map(json_from_value).collect())
        }
        serde_json::Value::Object(fields) => Json::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, json_from_value(value)))
                .collect(),
        ),
    }
}

/// Reads a shared bearer token without ever putting it in a command line.
pub fn read_secret_file(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map_err(|error| format!("could not inspect secret file {}: {error}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "secret file {} must not be group/world accessible",
                path.display()
            ));
        }
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("could not read secret file {}: {error}", path.display()))?;
    let secret = contents.trim_end_matches(['\r', '\n']).to_owned();
    if secret.len() < 32 {
        return Err("secret must contain at least 32 bytes".to_owned());
    }
    Ok(secret)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub id: String,
    pub received_at: String,
    pub payload: Json,
}

impl Event {
    pub fn response_json(&self) -> Json {
        let Json::Object(mut fields) = self.payload.clone() else {
            unreachable!("events are objects");
        };
        fields.push((
            "received_at".to_owned(),
            Json::String(self.received_at.clone()),
        ));
        fields.push(("sequence".to_owned(), Json::number(self.sequence)));
        Json::Object(fields)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadResult {
    pub epoch: String,
    pub reset: bool,
    pub lost: bool,
    pub events: Vec<Event>,
    pub next: u64,
}

pub struct Store {
    path: PathBuf,
    limit: usize,
    stream: EventStream,
    inner: Mutex<Inner>,
    changed: Condvar,
}

/// Primary event stream limits. Timeline queries read these same retained
/// segments, while delivery queue eviction remains an in-memory concern.
const EVENT_STREAM_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
const EVENT_STREAM_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The append-only source of truth for relay events. The in-memory deque is
/// only a hot cache; readers fall back to these retained segments whenever
/// their cursor predates the cache window.
struct EventStream {
    directory: PathBuf,
    initialized: bool,
    lock: Mutex<()>,
}

impl EventStream {
    fn open(state_path: &Path) -> Result<Self, String> {
        let directory = state_path.with_extension("stream");
        fs::create_dir_all(&directory)
            .map_err(|_| "could not initialize durable event stream".to_owned())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700));
        }
        let initialized = directory.join("initialized").exists();
        Ok(Self {
            directory,
            initialized,
            lock: Mutex::new(()),
        })
    }

    fn mark_initialized(&self) -> Result<(), String> {
        let marker = self.directory.join("initialized");
        if !marker.exists() {
            create_private(&marker)
                .map_err(|_| "could not initialize durable event stream".to_owned())?;
        }
        Ok(())
    }

    fn files(&self) -> Result<Vec<PathBuf>, String> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.directory)
            .map_err(|_| "could not read durable event stream".to_owned())?
        {
            let entry = entry.map_err(|_| "could not read durable event stream".to_owned())?;
            if entry
                .file_type()
                .map_err(|_| "could not inspect durable event stream".to_owned())?
                .is_file()
                && entry.file_name().to_string_lossy().ends_with(".jsonl")
            {
                files.push(entry.path());
            }
        }
        files.sort();
        Ok(files)
    }

    fn read_all(&self) -> Result<Vec<Event>, String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "event stream lock poisoned".to_owned())?;
        let mut events = Vec::new();
        for path in self.files()? {
            repair_incomplete_tail(&path)?;
            let contents = fs::read_to_string(path)
                .map_err(|_| "could not read durable event segment".to_owned())?;
            for line in contents.lines() {
                let value =
                    parse_json(line).map_err(|_| "invalid durable event segment".to_owned())?;
                events.push(decode_event(&value)?);
            }
        }
        events.sort_by_key(|event| event.sequence);
        Ok(events)
    }

    fn append(&self, event: &Event) -> Result<(), String> {
        {
            let _guard = self
                .lock
                .lock()
                .map_err(|_| "event stream lock poisoned".to_owned())?;
            let files = self.files()?;
            let path = if let Some(path) = files.last().filter(|path| {
                let fresh = segment_created_at(path)
                    .and_then(|created| SystemTime::now().duration_since(created).ok())
                    .is_some_and(|age| age < EVENT_STREAM_RETENTION);
                fresh
                    && fs::metadata(path)
                        .map(|metadata| metadata.len() < EVENT_STREAM_SEGMENT_BYTES)
                        .unwrap_or(false)
            }) {
                path.clone()
            } else {
                let created = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                self.directory.join(format!(
                    "segment-{:020}-{created:010}.jsonl",
                    event.sequence
                ))
            };
            let line = event.response_json().to_json() + "\n";
            if !path.exists() {
                create_private(&path)
                    .map_err(|_| "could not create durable event segment".to_owned())?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|_| "could not open durable event segment".to_owned())?;
            file.write_all(line.as_bytes())
                .map_err(|_| "could not append durable event".to_owned())?;
            file.sync_data()
                .map_err(|_| "could not sync durable event".to_owned())?;
        }
        // Retention cleanup is maintenance, not part of the append commit:
        // inability to delete an expired segment must not reject a durable
        // event or make callers retry it under a new sequence.
        let _ = self.prune(SystemTime::now());
        Ok(())
    }

    fn prune(&self, now: SystemTime) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "event stream lock poisoned".to_owned())?;
        let cutoff = now
            .checked_sub(EVENT_STREAM_RETENTION)
            .unwrap_or(UNIX_EPOCH);
        for path in self.files()? {
            let metadata = fs::metadata(&path)
                .map_err(|_| "could not inspect durable event segment".to_owned())?;
            let created = segment_created_at(&path).or_else(|| metadata.modified().ok());
            if created.is_some_and(|created| created < cutoff) {
                fs::remove_file(path)
                    .map_err(|_| "could not prune expired event segment".to_owned())?;
            }
        }
        Ok(())
    }
}

/// A process can fail during `write_all` before an event is committed. Remove
/// only an unterminated final JSONL record so it cannot poison later appends;
/// every complete newline-terminated event remains untouched.
fn repair_incomplete_tail(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|_| "could not read durable event segment".to_owned())?;
    if bytes.last() == Some(&b'\n') || bytes.is_empty() {
        return Ok(());
    }
    let valid_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|_| "could not repair durable event segment".to_owned())?;
    file.set_len(valid_len as u64)
        .and_then(|_| file.sync_data())
        .map_err(|_| "could not repair durable event segment".to_owned())
}

fn segment_created_at(path: &Path) -> Option<SystemTime> {
    let name = path.file_stem()?.to_str()?;
    let seconds = name.split('-').nth(2)?.parse::<u64>().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

static TEMPORARY_FILE_SERIAL: AtomicU64 = AtomicU64::new(0);
struct Inner {
    epoch: String,
    next_sequence: u64,
    events: VecDeque<Event>,
}

impl Store {
    pub fn open(path: impl Into<PathBuf>, limit: usize) -> Result<Self, String> {
        if limit == 0 {
            return Err("max-events must be greater than zero".to_owned());
        }
        let path = path.into();
        let mut persisted = match fs::read_to_string(&path) {
            // Import must see the complete legacy snapshot even if the new
            // live cache limit is smaller than the snapshot's event count.
            Ok(contents) => decode_state(&contents, usize::MAX)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Inner {
                epoch: new_epoch(),
                next_sequence: 1,
                events: VecDeque::new(),
            },
            Err(error) => {
                return Err(format!(
                    "could not read state file {}: {error}",
                    path.display()
                ));
            }
        };
        let stream = EventStream::open(&path)?;
        stream.prune(SystemTime::now())?;
        let mut archived = stream.read_all()?;
        if !stream.initialized {
            // Keep the snapshot as the recovery source until every event has
            // been imported and verified. If opening was interrupted earlier,
            // resume by skipping only exact events already present.
            for event in &persisted.events {
                match archived
                    .iter()
                    .find(|existing| existing.id == event.id || existing.sequence == event.sequence)
                {
                    Some(existing) if existing == event => {}
                    Some(_) => return Err("legacy event stream import conflict".to_owned()),
                    None => {
                        stream.append(event)?;
                        archived.push(event.clone());
                    }
                }
            }
            archived = stream.read_all()?;
            for event in &persisted.events {
                if !archived.iter().any(|existing| existing == event) {
                    return Err("legacy event stream import verification failed".to_owned());
                }
            }
            stream.mark_initialized()?;
        }
        let mut inner = if !archived.is_empty() {
            let mut events: VecDeque<Event> = archived.iter().rev().take(limit).cloned().collect();
            events.make_contiguous().reverse();
            Inner {
                epoch: persisted.epoch,
                next_sequence: archived
                    .last()
                    .map_or(1, |event| event.sequence.saturating_add(1)),
                events,
            }
        } else {
            if stream.initialized {
                persisted.events.clear();
            }
            persisted
        };
        if inner.next_sequence == 0 {
            inner.next_sequence = 1;
        }
        Ok(Self {
            stream,
            path,
            limit,
            inner: Mutex::new(inner),
            changed: Condvar::new(),
        })
    }

    pub fn add(&self, payload: Json) -> Result<(Event, bool), String> {
        let id = event_id(&payload)?;
        validate_audit_envelope(&payload)?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "queue lock poisoned".to_owned())?;
        if let Some(existing) = inner.events.iter().find(|event| event.id == id) {
            return Ok((existing.clone(), true));
        }
        if let Some(existing) = self
            .stream
            .read_all()?
            .into_iter()
            .find(|event| event.id == id)
        {
            return Ok((existing, true));
        }
        // Do not expose a new event until its complete state has been made
        // durable. A failed write must leave retries eligible to be accepted.
        let mut updated = Inner {
            epoch: inner.epoch.clone(),
            next_sequence: inner.next_sequence,
            events: inner.events.clone(),
        };
        let event = Event {
            sequence: updated.next_sequence,
            id,
            received_at: rfc3339_timestamp(),
            payload,
        };
        // Archive the exact envelope (including relay sequence/receipt time)
        // before making it observable through the bounded delivery queue.
        self.stream.append(&event)?;
        updated.next_sequence += 1;
        updated.events.push_back(event.clone());
        if updated.events.len() > self.limit {
            updated.events.pop_front();
        }
        // The snapshot only seeds the hot cache and preserves the epoch for
        // legacy clients. Segment append above is the durability boundary.
        let _ = self.save(&updated);
        *inner = updated;
        self.changed.notify_all();
        Ok((event, false))
    }

    /// Reads the durable archive; this intentionally ignores the live queue.
    pub fn timeline(&self, task_id: &str) -> Result<Vec<Json>, String> {
        Ok(self
            .stream
            .read_all()?
            .into_iter()
            .filter(|event| event.payload.object("task_id").and_then(Json::as_str) == Some(task_id))
            .map(|event| event.response_json())
            .collect())
    }

    pub fn read(
        &self,
        after: u64,
        requested_epoch: &str,
        timeout: Duration,
    ) -> Result<ReadResult, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "queue lock poisoned".to_owned())?;
        let reset = !requested_epoch.is_empty() && requested_epoch != inner.epoch;
        let after = if reset { 0 } else { after };
        let deadline = Instant::now() + timeout;
        while !reset && !available(&inner, after) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let (guard, wait) = self
                .changed
                .wait_timeout(inner, left)
                .map_err(|_| "queue lock poisoned".to_owned())?;
            inner = guard;
            if wait.timed_out() {
                break;
            }
        }
        let cached_first = inner
            .events
            .front()
            .map_or(inner.next_sequence, |event| event.sequence);
        let (events, lost) = if after.saturating_add(1) < cached_first {
            let archived = self.stream.read_all()?;
            let first = archived
                .first()
                .map_or(inner.next_sequence, |event| event.sequence);
            (
                archived
                    .into_iter()
                    .filter(|event| event.sequence > after)
                    .collect(),
                after < first.saturating_sub(1),
            )
        } else {
            (
                inner
                    .events
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned()
                    .collect(),
                false,
            )
        };
        Ok(ReadResult {
            epoch: inner.epoch.clone(),
            reset,
            lost,
            events,
            next: inner.next_sequence - 1,
        })
    }

    fn save(&self, inner: &Inner) -> Result<(), String> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create state directory: {error}"))?;
        let temporary = parent.join(format!(
            ".zigzag-{}-{}-{}.tmp",
            std::process::id(),
            inner.next_sequence,
            TEMPORARY_FILE_SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let write_result = (|| -> Result<(), String> {
            let mut output = create_private(&temporary)
                .map_err(|error| format!("could not create temporary state file: {error}"))?;
            output
                .write_all(state_json(inner).to_json().as_bytes())
                .map_err(|error| format!("could not write state file: {error}"))?;
            output
                .write_all(b"\n")
                .map_err(|error| format!("could not write state file: {error}"))?;
            output
                .sync_all()
                .map_err(|error| format!("could not sync state file: {error}"))?;
            fs::rename(&temporary, &self.path)
                .map_err(|error| format!("could not replace state file: {error}"))?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

fn available(inner: &Inner, after: u64) -> bool {
    inner.events.iter().any(|event| event.sequence > after)
        || after
            < inner
                .events
                .front()
                .map_or(inner.next_sequence, |event| event.sequence)
                .saturating_sub(1)
}
fn event_id(payload: &Json) -> Result<String, String> {
    payload
        .object("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "body_must_be_an_object_with_nonempty_id".to_owned())
}
fn validate_audit_envelope(payload: &Json) -> Result<(), String> {
    let has_execution = payload.object("execution_id").is_some();
    let has_schema = payload.object("schema_version").is_some();
    if !has_execution && !has_schema {
        // Legacy live-queue messages remain wire compatible.  They have no
        // execution archive because there is no safe execution partition key.
        return Ok(());
    }
    let text = |field: &str| {
        payload
            .object(field)
            .and_then(Json::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("invalid_audit_event_{field}"))
    };
    if payload.object("schema_version").and_then(Json::as_u64) != Some(1) {
        return Err("invalid_audit_event_schema_version".to_owned());
    }
    text("task_id")?;
    text("execution_id")?;
    text("kind")?;
    let source = text("source")?;
    if !matches!(source, "vm-department" | "mac-relay" | "vm-poller") {
        return Err("invalid_audit_event_source".to_owned());
    }
    let occurred_at = text("occurred_at")?;
    if parse_rfc3339_millis(occurred_at).is_none() {
        return Err("invalid_audit_event_occurred_at".to_owned());
    }
    text("clock")?;
    if !matches!(payload.object("payload"), Some(Json::Object(_))) {
        return Err("invalid_audit_event_payload".to_owned());
    }
    Ok(())
}

/// Parses a strict RFC 3339 UTC millisecond timestamp into Unix milliseconds.
/// This is shared by schema validation and the timeline so they cannot drift.
pub fn parse_rfc3339_millis(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if !(bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes
            .iter()
            .enumerate()
            .filter(|(index, _)| !matches!(*index, 4 | 7 | 10 | 13 | 16 | 19 | 23))
            .all(|(_, byte)| byte.is_ascii_digit()))
    {
        return None;
    }
    let number = |start, end| {
        std::str::from_utf8(&bytes[start..end])
            .ok()?
            .parse::<u64>()
            .ok()
    };
    let year = number(0, 4)? as i64;
    let month = number(5, 7)? as i64;
    let day = number(8, 10)? as i64;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let millis = number(20, 23)?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let adjusted_month = month + if month <= 2 { 9 } else { -3 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let doy = (153 * adjusted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days)
        .ok()?
        .checked_mul(86_400_000)?
        .checked_add((hour * 3_600 + minute * 60 + second) * 1_000)?
        .checked_add(millis)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// RFC 3339 UTC with millisecond precision, without a time-formatting crate.
pub fn rfc3339_timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_date(elapsed.as_secs() / 86_400);
    let second_of_day = elapsed.as_secs() % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60,
        elapsed.subsec_millis()
    )
}

// Howard Hinnant's civil-from-days algorithm, with 1970-01-01 as day zero.
fn civil_date(days_since_epoch: u64) -> (i64, u32, u32) {
    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}
fn new_epoch() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", now ^ ((std::process::id() as u128) << 64))
}

fn state_json(inner: &Inner) -> Json {
    Json::Object(vec![
        ("epoch".to_owned(), Json::String(inner.epoch.clone())),
        (
            "next_sequence".to_owned(),
            Json::number(inner.next_sequence),
        ),
        (
            "events".to_owned(),
            Json::Array(
                inner
                    .events
                    .iter()
                    .map(|event| {
                        Json::Object(vec![
                            ("sequence".to_owned(), Json::number(event.sequence)),
                            ("id".to_owned(), Json::String(event.id.clone())),
                            (
                                "received_at".to_owned(),
                                Json::String(event.received_at.clone()),
                            ),
                            ("payload".to_owned(), event.payload.clone()),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn decode_event(value: &Json) -> Result<Event, String> {
    let Json::Object(fields) = value else {
        return Err("invalid durable event: expected object".to_owned());
    };
    let sequence = value
        .object("sequence")
        .and_then(Json::as_u64)
        .ok_or_else(|| "invalid durable event sequence".to_owned())?;
    let id = value
        .object("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "invalid durable event id".to_owned())?
        .to_owned();
    let received_at = value
        .object("received_at")
        .and_then(Json::as_str)
        .ok_or_else(|| "invalid durable event received_at".to_owned())?
        .to_owned();
    let payload = Json::Object(
        fields
            .iter()
            .filter(|(key, _)| key != "sequence" && key != "received_at")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    if !matches!(payload.object("id"), Some(Json::String(_))) {
        return Err("invalid durable event payload".to_owned());
    }
    Ok(Event {
        sequence,
        id,
        received_at,
        payload,
    })
}

fn decode_state(contents: &str, limit: usize) -> Result<Inner, String> {
    let value = parse_json(contents).map_err(|error| format!("invalid state file: {error}"))?;
    let epoch = value
        .object("epoch")
        .and_then(Json::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "invalid state file: epoch".to_owned())?
        .to_owned();
    let next_sequence = value
        .object("next_sequence")
        .and_then(Json::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| "invalid state file: next_sequence".to_owned())?;
    let values = match value.object("events") {
        Some(Json::Array(values)) => values,
        _ => return Err("invalid state file: events".to_owned()),
    };
    let mut events = VecDeque::new();
    for value in values.iter().rev().take(limit).rev() {
        let sequence = value
            .object("sequence")
            .and_then(Json::as_u64)
            .ok_or_else(|| "invalid state file event sequence".to_owned())?;
        let id = value
            .object("id")
            .and_then(Json::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "invalid state file event id".to_owned())?
            .to_owned();
        let received_at = value
            .object("received_at")
            .and_then(Json::as_str)
            .ok_or_else(|| "invalid state file event received_at".to_owned())?
            .to_owned();
        let payload = value
            .object("payload")
            .filter(|payload| matches!(payload, Json::Object(_)))
            .ok_or_else(|| "invalid state file event payload".to_owned())?
            .clone();
        events.push_back(Event {
            sequence,
            id,
            received_at,
            payload,
        });
    }
    Ok(Inner {
        epoch,
        next_sequence,
        events,
    })
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zigzag-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn event(id: &str) -> Json {
        Json::Object(vec![("id".to_owned(), Json::String(id.to_owned()))])
    }

    fn audit_event(id: &str, kind: &str) -> Json {
        Json::Object(vec![
            ("id".to_owned(), Json::String(id.to_owned())),
            ("schema_version".to_owned(), Json::number(1)),
            ("task_id".to_owned(), Json::String("task-1".to_owned())),
            (
                "execution_id".to_owned(),
                Json::String("execution-1".to_owned()),
            ),
            ("kind".to_owned(), Json::String(kind.to_owned())),
            (
                "source".to_owned(),
                Json::String("vm-department".to_owned()),
            ),
            (
                "occurred_at".to_owned(),
                Json::String("2026-09-23T12:34:56.789Z".to_owned()),
            ),
            ("clock".to_owned(), Json::String("vm:boot-1".to_owned())),
            ("payload".to_owned(), Json::Object(vec![])),
        ])
    }

    #[test]
    fn json_round_trips_through_serde_json() {
        let value = Json::Object(vec![
            ("message".to_owned(), Json::String("hello".to_owned())),
            (
                "values".to_owned(),
                Json::Array(vec![Json::Bool(true), Json::number(42), Json::Null]),
            ),
        ]);

        assert_eq!(parse_json(&value.to_json()).unwrap(), value);
    }

    #[test]
    fn json_null_fields_remain_explicit_nulls() {
        let value = parse_json(r#"{"a":null,"b":1}"#).unwrap();
        let optional_a = match value.object("a") {
            Some(Json::Null) | None => None,
            Some(value) => value.as_str().map(str::to_owned),
        };

        assert_eq!(optional_a, None);
        assert_eq!(value.object("a"), Some(&Json::Null));
        assert_eq!(
            parse_json(&value.to_json()).unwrap().object("a"),
            Some(&Json::Null)
        );
    }

    #[test]
    fn json_decodes_unicode_and_standard_escapes() {
        let value = parse_json(r#"{"text":"\uD83D\uDE00 \" \\ \b\f\n\r\t"}"#).unwrap();

        assert_eq!(
            value.object("text").and_then(Json::as_str),
            Some("😀 \" \\ \u{08}\u{0c}\n\r\t")
        );
    }

    #[test]
    fn json_parses_nested_objects_and_arrays() {
        let value = parse_json(r#"{"outer":[{"inner":[{"leaf":true}]}]}"#).unwrap();
        let Json::Array(outer) = value.object("outer").unwrap() else {
            panic!("outer should be an array");
        };
        let Json::Array(inner) = outer[0].object("inner").unwrap() else {
            panic!("inner should be an array");
        };

        assert_eq!(inner[0].object("leaf"), Some(&Json::Bool(true)));
    }

    #[test]
    fn json_handles_u64_numbers_and_rejects_malformed_ones() {
        assert_eq!(
            parse_json("18446744073709551615").unwrap().as_u64(),
            Some(u64::MAX)
        );
        assert_eq!(parse_json("0").unwrap().as_u64(), Some(0));
        for number in ["01", "1.", "1e", "-", "--1", "1e+-2"] {
            assert!(parse_json(number).is_err(), "{number} should be rejected");
        }
    }

    #[test]
    fn json_rejects_trailing_and_truncated_input_without_panicking() {
        assert!(parse_json("null true").is_err());
        for input in ["{", "{\"a\":", "[1,", "\"unterminated", "{\"a\": invalid}"] {
            assert!(parse_json(input).is_err(), "{input:?} should be rejected");
        }
    }

    fn agent(id: &str) -> AgentRecord {
        AgentRecord {
            id: id.to_owned(),
            task_id: "task-1".to_owned(),
            execution_id: "execution-1".to_owned(),
            leader_pid: 42,
            process_group: 42,
            process_identity: Some("test:42".to_owned()),
            worktree_path: None,
            started_at: "1".to_owned(),
            deadline_at: None,
            command: "codex exec".to_owned(),
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
            harness_session_id: None,
            harness: None,
        }
    }

    #[test]
    fn agent_registry_recovers_running_groups_and_redacts_durable_logs() {
        let file = path("agents");
        let registry = AgentRegistry::open(&file).unwrap();
        registry.register(agent("live")).unwrap();
        let mut gone = agent("gone");
        gone.process_group = 99;
        gone.process_identity = Some("test:99".to_owned());
        registry.register(gone).unwrap();
        let mut legacy_orphan = agent("legacy-orphan");
        legacy_orphan.state = "orphaned".to_owned();
        registry.register(legacy_orphan).unwrap();
        let changed = registry
            .recover(|record| record.process_identity.as_deref() == Some("test:42"))
            .unwrap();
        assert_eq!(changed.len(), 3);
        assert_eq!(registry.get("live").unwrap().state, "running");
        assert_eq!(registry.get("legacy-orphan").unwrap().state, "running");
        assert_eq!(registry.get("gone").unwrap().state, "lost_after_restart");
        registry
            .append_log("live", "stdout", b"Bearer secret-value\n")
            .unwrap();
        let logs = registry
            .logs_json("live", "stdout", 0, None)
            .unwrap()
            .to_json();
        assert!(logs.contains("[REDACTED]"));
        assert!(!logs.contains("secret-value"));
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("agent-logs"));
    }

    #[test]
    fn agent_restart_metadata_links_new_agent_to_old_and_persists() {
        let file = path("agents-restart");
        let registry = AgentRegistry::open(&file).unwrap();
        registry.register(agent("old")).unwrap();
        registry.register(agent("new")).unwrap();
        let config = "CONFIG-BLOB";
        registry
            .set_restart_metadata("new", "old", Some(config))
            .unwrap();
        let fetched = registry.get("new").unwrap();
        assert_eq!(fetched.restarted_from.as_deref(), Some("old"));
        assert_eq!(fetched.agent_config.as_deref(), Some(config));
        assert_eq!(registry.get("old").unwrap().restarted_from, None);
        let status = fetched.status_json().to_json();
        assert!(status.contains("restarted_from"));
        assert!(status.contains("restartable"));
        drop(registry);
        let reopened = AgentRegistry::open(&file).unwrap();
        let refetched = reopened.get("new").unwrap();
        assert_eq!(refetched.restarted_from.as_deref(), Some("old"));
        assert_eq!(refetched.agent_config.as_deref(), Some(config));
        assert!(
            reopened
                .set_restart_metadata("missing", "old", None)
                .unwrap()
                .is_none()
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("agent-logs"));
    }
    #[test]
    fn decode_agents_accepts_legacy_string_counters() {
        let (agents, skipped) =
            decode_agents(include_str!("../tests/fixtures/legacy-events.agents.json"));
        assert!(
            skipped.is_empty(),
            "unexpected skipped records: {:?}",
            skipped
        );
        let record = agents.get("legacy-0003").unwrap();
        assert_eq!(record.stdout_next, 0);
        assert_eq!(record.stderr_next, 64);
        assert_eq!(record.stdout_dropped_before, 0);
        assert_eq!(record.stderr_dropped_before, 0);
        assert_eq!(record.log_next, 0);
        assert_eq!(record.log_dropped_before, 0);
        assert_eq!(record.first_output_bytes, Some(7));
    }

    #[test]
    fn decode_agents_handles_nulls_and_skips_bad_records() {
        // Registry with explicit nulls for optional fields (as written by
        // older versions) must parse successfully.
        let json_with_nulls = r#"{
            "agents": [
                {
                    "id": "agent-1",
                    "task_id": "task-1",
                    "execution_id": "exec-1",
                    "leader_pid": 123,
                    "process_group": 456,
                    "process_identity": null,
                    "worktree_path": "/tmp/wt-1",
                    "started_at": "2026-10-08T00:00:00Z",
                    "deadline_at": null,
                    "command": "echo hi",
                    "state": "running",
                    "exit_code": null,
                    "log_degraded": false,
                    "audit_degraded": false,
                    "redacted": false,
                    "stdout_next": 0,
                    "stderr_next": 0,
                    "stdout_dropped_before": 0,
                    "stderr_dropped_before": 0,
                    "log_next": 0,
                    "log_dropped_before": 0,
                    "first_output_at": null,
                    "first_output_stream": null,
                    "first_output_bytes": null
                },
                {
                    "id": "bad-agent",
                    "task_id": "task-bad"
                }
            ]
        }"#;
        let (agents, skipped) = decode_agents(json_with_nulls);
        // The good record with nulls must parse
        assert_eq!(agents.len(), 1);
        let record = agents.get("agent-1").expect("agent-1 should parse");
        assert_eq!(record.id, "agent-1");
        assert_eq!(record.deadline_at, None);
        assert_eq!(record.process_identity, None);
        assert_eq!(record.worktree_path, Some("/tmp/wt-1".to_owned()));
        assert_eq!(record.exit_code, None);
        // The bad record must be skipped (not crash the whole registry)
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("bad-agent"),
            "skip message should identify the agent: {}",
            skipped[0]
        );
    }

    #[test]
    fn agent_json_omits_null_fields() {
        // The writer must omit null fields instead of writing explicit nulls.
        let record = AgentRecord {
            id: "test-1".to_owned(),
            task_id: "task-1".to_owned(),
            execution_id: "exec-1".to_owned(),
            leader_pid: 123,
            process_group: 456,
            process_identity: None,
            worktree_path: None,
            started_at: "2026-10-08T00:00:00Z".to_owned(),
            deadline_at: None,
            command: "echo".to_owned(),
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
            harness_session_id: None,
            harness: None,
        };
        let json = agent_json(&record);
        let text = json.to_json();
        // None of the optional fields should appear as explicit nulls
        assert!(
            !text.contains("null"),
            "writer should omit nulls, got: {}",
            text
        );
        // Required fields must still be present
        assert!(text.contains("test-1"));
    }

    #[test]
    fn set_paused_at_persists_and_clears() {
        let file = path("agents-pause");
        let registry = AgentRegistry::open(&file).unwrap();
        registry.register(agent("a1")).unwrap();
        assert_eq!(registry.get("a1").unwrap().paused_at, None);
        let stamp = "2026-10-09T01:02:03.456Z".to_owned();
        registry.set_paused_at("a1", Some(stamp.clone())).unwrap();
        assert_eq!(registry.get("a1").unwrap().paused_at, Some(stamp.clone()));
        // The marker survives a registry reload (daemon restart).
        drop(registry);
        let registry = AgentRegistry::open(&file).unwrap();
        assert_eq!(registry.get("a1").unwrap().paused_at, Some(stamp));
        // A repeated set is a no-op that still succeeds.
        registry
            .set_paused_at("a1", Some("2026-10-09T01:02:03.456Z".to_owned()))
            .unwrap();
        registry.set_paused_at("a1", None).unwrap();
        assert_eq!(registry.get("a1").unwrap().paused_at, None);
        // Unknown ids are a no-op, not an error.
        registry
            .set_paused_at("missing", Some("x".to_owned()))
            .unwrap();
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("agent-logs"));
    }

    #[test]
    fn native_harness_thread_id_round_trips_and_is_preserved_for_resume() {
        let file = path("agents-native-thread");
        let registry = AgentRegistry::open(&file).unwrap();
        let mut record = agent("native");
        record.harness = Some("codex".to_owned());
        record.harness_session_id = Some("thread-abc".to_owned());
        registry.register(record).unwrap();
        let still_running = registry.recover(|_| false).unwrap();
        assert!(still_running.is_empty());
        let reloaded = AgentRegistry::open(&file).unwrap();
        let record = reloaded.get("native").unwrap();
        assert_eq!(record.state, "running");
        assert_eq!(record.harness.as_deref(), Some("codex"));
        assert_eq!(record.harness_session_id.as_deref(), Some("thread-abc"));
    }

    #[test]
    fn paused_at_round_trips_through_agent_json() {
        let mut record = agent("paused-1");
        record.paused_at = Some("2026-10-09T01:02:03.456Z".to_owned());
        let text = agent_json(&record).to_json();
        assert!(text.contains("2026-10-09T01:02:03.456Z"));
        let status = record.status_json().to_json();
        assert!(status.contains("2026-10-09T01:02:03.456Z"));
        let wrapped = format!("{{\"agents\": [{text}]}}");
        let (agents, skipped) = decode_agents(&wrapped);
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");
        let decoded = agents.get("paused-1").expect("record survives round trip");
        assert_eq!(decoded.paused_at, record.paused_at);
    }

    #[test]
    fn decode_agents_accepts_null_or_missing_paused_at() {
        // Legacy registries have no paused_at field; older writers may emit
        // an explicit null.  Both forms decode to None.
        let template = |paused_fragment: &str| {
            format!(
                "{{\"agents\": [{{\"id\": \"agent-1\", \"task_id\": \"task-1\", \
                 \"execution_id\": \"exec-1\", \"leader_pid\": 123, \"process_group\": 456, \
                 \"started_at\": \"2026-10-08T00:00:00Z\", \"command\": \"echo hi\", \
                 \"state\": \"running\", \"stdout_next\": 0, \"stderr_next\": 0, \
                 \"stdout_dropped_before\": 0, \"stderr_dropped_before\": 0, \
                 \"log_next\": 0, \"log_dropped_before\": 0{paused_fragment}}}]}}"
            )
        };
        let (agents, skipped) = decode_agents(&template(", \"paused_at\": null"));
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");
        assert_eq!(agents.get("agent-1").unwrap().paused_at, None);
        let (agents, skipped) = decode_agents(&template(""));
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");
        assert_eq!(agents.get("agent-1").unwrap().paused_at, None);
    }

    #[test]
    fn agent_log_cursor_orders_interleaved_streams() {
        let file = path("agent-cursors");
        let registry = AgentRegistry::open(&file).unwrap();
        registry.register(agent("one")).unwrap();
        registry.append_log("one", "stdout", b"first").unwrap();
        registry.append_log("one", "stderr", b"second").unwrap();
        let logs = registry.logs_json("one", "both", 5, None).unwrap();
        assert_eq!(logs.object("next_cursor"), Some(&Json::number(11)));
        let Json::Array(records) = logs.object("records").unwrap() else {
            panic!("records")
        };
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].object("data"),
            Some(&Json::String("second".to_owned()))
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("agent-logs"));
    }

    #[test]
    fn repost_is_idempotent() {
        let file = path("idempotent");
        let store = Store::open(&file, 10).unwrap();
        let (first, duplicate) = store.add(event("job-1")).unwrap();
        assert!(!duplicate);
        let (second, duplicate) = store.add(event("job-1")).unwrap();
        assert!(duplicate);
        assert_eq!(first, second);
        assert_eq!(store.read(0, "", Duration::ZERO).unwrap().events.len(), 1);
        let _ = fs::remove_file(file);
    }

    #[test]
    fn execution_audit_survives_live_queue_eviction() {
        let file = path("execution-audit");
        let store = Store::open(&file, 1).unwrap();
        store.add(audit_event("one", "task_dispatched")).unwrap();
        store.add(audit_event("two", "poll_started")).unwrap();
        assert_eq!(store.read(0, "", Duration::ZERO).unwrap().events.len(), 2);
        let timeline = store.timeline("task-1").unwrap();
        assert_eq!(timeline.len(), 2);
        assert_eq!(
            timeline[0].object("kind"),
            Some(&Json::String("task_dispatched".to_owned()))
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("audit"));
    }

    #[test]
    fn timeline_reopens_from_the_primary_event_stream() {
        let file = path("execution-reopen");
        let store = Store::open(&file, 10).unwrap();
        store.add(audit_event("one", "task_dispatched")).unwrap();
        let mut second = audit_event("two", "poll_started");
        if let Json::Object(fields) = &mut second {
            for (name, value) in fields {
                if name == "execution_id" {
                    *value = Json::String("execution-2".to_owned());
                }
            }
        }
        store.add(second).unwrap();
        drop(store);
        let reopened = Store::open(&file, 10).unwrap();
        assert_eq!(reopened.timeline("task-1").unwrap().len(), 2);
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn interrupted_legacy_import_resumes_without_gaps_or_duplicates() {
        let file = path("legacy-import-resume");
        let first = Event {
            sequence: 1,
            id: "legacy-one".to_owned(),
            received_at: "2026-10-01T00:00:00.000Z".to_owned(),
            payload: audit_event("legacy-one", "task_dispatched"),
        };
        let second = Event {
            sequence: 2,
            id: "legacy-two".to_owned(),
            received_at: "2026-10-01T00:00:01.000Z".to_owned(),
            payload: audit_event("legacy-two", "poll_started"),
        };
        let snapshot = Inner {
            epoch: "legacy-epoch".to_owned(),
            next_sequence: 3,
            events: VecDeque::from([first.clone(), second.clone()]),
        };
        fs::write(&file, state_json(&snapshot).to_json()).unwrap();

        // Model a process stopping after it durably appends only the first
        // legacy event, before writing the initialization marker.
        let stream = EventStream::open(&file).unwrap();
        stream.append(&first).unwrap();
        drop(stream);

        let reopened = Store::open(&file, 1).unwrap();
        let archived = reopened.stream.read_all().unwrap();
        assert_eq!(archived, vec![first.clone(), second.clone()]);
        assert_eq!(
            reopened.read(0, "", Duration::ZERO).unwrap().events.len(),
            2
        );
        drop(reopened);

        let reopened_again = Store::open(&file, 1).unwrap();
        assert_eq!(
            reopened_again.stream.read_all().unwrap(),
            vec![first, second]
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("stream"));
    }

    #[test]
    fn audit_validation_rejects_invalid_calendar_and_schema_fields() {
        for (field, value) in [
            ("source", Json::String("other".to_owned())),
            ("schema_version", Json::number(2)),
            (
                "occurred_at",
                Json::String("2026-99-99T99:99:99.999Z".to_owned()),
            ),
            ("payload", Json::Array(vec![])),
        ] {
            let mut value_to_test = audit_event("bad", "task_dispatched");
            if let Json::Object(fields) = &mut value_to_test {
                for (name, existing) in fields {
                    if name == field {
                        *existing = value.clone();
                    }
                }
            }
            assert!(validate_audit_envelope(&value_to_test).is_err(), "{field}");
        }
        assert!(parse_rfc3339_millis("2024-02-29T23:59:59.999Z").is_some());
        assert!(parse_rfc3339_millis("2023-02-29T23:59:59.999Z").is_none());
        let generated = rfc3339_timestamp();
        assert!(parse_rfc3339_millis(&generated).is_some());
    }

    #[test]
    fn live_state_snapshot_failure_does_not_hide_durable_event() {
        let file = path("audit-retry");
        let store = Store::open(&file, 10).unwrap();
        fs::create_dir(&file).unwrap();
        let payload = audit_event("retry", "task_dispatched");
        assert!(store.add(payload.clone()).is_ok());
        fs::remove_dir(&file).unwrap();
        assert!(store.add(payload).unwrap().1);
        assert_eq!(store.read(0, "", Duration::ZERO).unwrap().events.len(), 1);
        assert_eq!(store.timeline("task-1").unwrap().len(), 1);
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("audit"));
    }

    #[test]
    fn timeline_stays_consistent_when_legacy_audit_path_is_unwritable() {
        let file = path("audit-write-failure");
        let store = Store::open(&file, 10).unwrap();
        // An old independent audit writer would fail against this path. The
        // timeline now reads the same durable stream that accepts the event.
        fs::write(file.with_extension("audit"), "blocked").unwrap();
        let (accepted, duplicate) = store
            .add(audit_event("audit-failure", "task_dispatched"))
            .unwrap();
        assert!(!duplicate);
        let timeline = store.timeline("task-1").unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0], accepted.response_json());
        let _ = fs::remove_file(&file);
        let _ = fs::remove_file(file.with_extension("audit"));
    }

    #[test]
    fn malformed_audit_envelope_is_rejected_without_affecting_legacy_events() {
        let file = path("audit-validation");
        let store = Store::open(&file, 10).unwrap();
        let mut invalid = audit_event("bad", "task_dispatched");
        if let Json::Object(fields) = &mut invalid {
            fields.retain(|(name, _)| name != "clock");
        }
        assert!(store.add(invalid).is_err());
        assert!(store.add(event("legacy")).is_ok());
        let _ = fs::remove_file(file);
    }

    #[test]
    fn cursor_and_epoch_survive_restart() {
        let file = path("resume");
        let store = Store::open(&file, 10).unwrap();
        store.add(event("job-1")).unwrap();
        store.add(event("job-2")).unwrap();
        let first = store.read(0, "", Duration::ZERO).unwrap();
        drop(store);
        let resumed = Store::open(&file, 10)
            .unwrap()
            .read(1, &first.epoch, Duration::ZERO)
            .unwrap();
        assert!(!resumed.reset);
        assert_eq!(resumed.next, 2);
        assert_eq!(resumed.events[0].id, "job-2");
        let _ = fs::remove_file(file);
    }

    #[test]
    fn bounded_cache_overflow_reads_every_event_from_durable_stream() {
        let file = path("eviction");
        let store = Store::open(&file, 2).unwrap();
        store.add(event("one")).unwrap();
        store.add(event("two")).unwrap();
        store.add(event("three")).unwrap();
        let result = store.read(0, "", Duration::ZERO).unwrap();
        assert!(!result.lost);
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(store.stream.files().unwrap().iter().any(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("segment-00000000000000000001-")
        }));
        let _ = fs::remove_dir_all(file.with_extension("stream"));
        let _ = fs::remove_file(file);
    }

    #[test]
    fn durable_stream_reopens_after_relay_restart_and_late_reader_gets_item_start() {
        let file = path("stream-restart");
        let first_store = Store::open(&file, 1).unwrap();
        first_store.add(event("item-start")).unwrap();
        first_store.add(event("item-delta")).unwrap();
        let epoch = first_store.read(0, "", Duration::ZERO).unwrap().epoch;
        drop(first_store);

        let restarted = Store::open(&file, 1).unwrap();
        let late = restarted.read(0, &epoch, Duration::ZERO).unwrap();
        assert!(!late.reset);
        assert!(!late.lost);
        assert_eq!(
            late.events
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            vec!["item-start", "item-delta"]
        );
        drop(restarted);
        let _ = fs::remove_dir_all(file.with_extension("stream"));
        let _ = fs::remove_dir_all(file.with_extension("audit"));
        let _ = fs::remove_file(file);
    }

    #[test]
    fn retention_gc_removes_whole_segments_without_reimporting_snapshot() {
        let file = path("stream-retention");
        let store = Store::open(&file, 10).unwrap();
        store.add(event("retained-before-gc")).unwrap();
        let epoch = store.read(0, "", Duration::ZERO).unwrap().epoch;
        let future = SystemTime::now() + EVENT_STREAM_RETENTION + Duration::from_secs(2);
        store.stream.prune(future).unwrap();
        assert!(store.stream.files().unwrap().is_empty());
        drop(store);

        let reopened = Store::open(&file, 10).unwrap();
        let expired = reopened.read(0, &epoch, Duration::ZERO).unwrap();
        assert!(expired.lost);
        assert!(expired.events.is_empty());
        assert_eq!(expired.next, 1);
        drop(reopened);
        let _ = fs::remove_dir_all(file.with_extension("stream"));
        let _ = fs::remove_dir_all(file.with_extension("audit"));
        let _ = fs::remove_file(file);
    }

    #[test]
    fn restart_repairs_only_an_uncommitted_partial_segment_tail() {
        let file = path("stream-partial-tail");
        let store = Store::open(&file, 10).unwrap();
        store.add(event("complete")).unwrap();
        let segment = store.stream.files().unwrap().pop().unwrap();
        OpenOptions::new()
            .append(true)
            .open(&segment)
            .unwrap()
            .write_all(b"{\"id\":\"partial")
            .unwrap();
        drop(store);

        let reopened = Store::open(&file, 10).unwrap();
        assert_eq!(
            reopened.read(0, "", Duration::ZERO).unwrap().events.len(),
            1
        );
        reopened.add(event("next")).unwrap();
        assert_eq!(
            reopened.read(0, "", Duration::ZERO).unwrap().events.len(),
            2
        );
        drop(reopened);
        let _ = fs::remove_dir_all(file.with_extension("stream"));
        let _ = fs::remove_dir_all(file.with_extension("audit"));
        let _ = fs::remove_file(file);
    }

    #[test]
    fn post_wakes_waiting_reader() {
        let file = path("wake");
        let store = Arc::new(Store::open(&file, 10).unwrap());
        let reader = Arc::clone(&store);
        let waiting = thread::spawn(move || {
            let start = Instant::now();
            let result = reader.read(0, "", Duration::from_secs(2)).unwrap();
            (start.elapsed(), result)
        });
        thread::sleep(Duration::from_millis(40));
        store.add(event("wake")).unwrap();
        let (elapsed, result) = waiting.join().unwrap();
        assert!(elapsed < Duration::from_millis(500));
        assert_eq!(result.events[0].id, "wake");
        let _ = fs::remove_file(file);
    }

    #[test]
    fn stream_append_failure_does_not_expose_event() {
        let parent = path("read-only-directory");
        fs::create_dir(&parent).unwrap();
        let store = Store::open(parent.join("events.json"), 10).unwrap();
        let stream_dir = parent.join("events.stream");
        fs::rename(&stream_dir, parent.join("stream-backup")).unwrap();
        fs::write(&stream_dir, "block stream directory").unwrap();
        assert!(store.add(event("not-durable")).is_err());
        drop(store);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn epoch_mismatch_returns_immediately_when_empty() {
        let file = path("epoch-reset");
        let store = Store::open(&file, 10).unwrap();
        let start = Instant::now();
        let result = store
            .read(42, "previous-epoch", Duration::from_secs(2))
            .unwrap();
        assert!(result.reset);
        assert!(result.events.is_empty());
        assert!(start.elapsed() < Duration::from_millis(100));
        let _ = fs::remove_file(file);
    }

    #[test]
    fn idempotency_survives_restart_while_event_is_retained() {
        let file = path("restart-idempotency");
        let store = Store::open(&file, 2).unwrap();
        let (first, duplicate) = store.add(event("job-1")).unwrap();
        assert!(!duplicate);
        drop(store);
        let (second, duplicate) = Store::open(&file, 2).unwrap().add(event("job-1")).unwrap();
        assert!(duplicate);
        assert_eq!(first, second);
        let _ = fs::remove_file(file);
    }

    #[test]
    #[cfg(unix)]
    fn secret_file_requires_private_permissions_and_a_long_token() {
        use std::os::unix::fs::PermissionsExt;

        let file = path("secret");
        fs::write(&file, format!("{}\n", "x".repeat(32))).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_secret_file(&file).unwrap(), "x".repeat(32));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(read_secret_file(&file).is_err());
        let _ = fs::remove_file(file);
    }
}
