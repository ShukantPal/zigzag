//! Shared durable queue and deliberately small JSON support for Zigzag.

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
    pub started_at: String,
    pub deadline_at: Option<String>,
    pub command: String,
    pub state: String,
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
            Ok(text) => decode_agents(&text)?,
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

    /// Mark formerly live agents honestly after a relay restart.  The caller
    /// supplies a non-signalling group probe; no lost pipe is ever reattached.
    pub fn recover<F>(&self, group_running: F) -> Result<Vec<AgentRecord>, String>
    where
        F: Fn(i32) -> bool,
    {
        let mut entries = self
            .inner
            .lock()
            .map_err(|_| "agent registry lock poisoned".to_owned())?;
        let mut updated = entries.clone();
        let mut changed = Vec::new();
        for entry in updated.values_mut() {
            if entry.state == "running" {
                entry.state = if group_running(entry.process_group) {
                    "orphaned".to_owned()
                } else {
                    "lost_after_restart".to_owned()
                };
                changed.push(entry.clone());
            }
        }
        if !changed.is_empty() {
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
    Json::Object(vec![
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
    ])
}
fn decode_agents(text: &str) -> Result<std::collections::BTreeMap<String, AgentRecord>, String> {
    let values = match parse_json(text)
        .ok()
        .and_then(|v| v.object("agents").cloned())
    {
        Some(Json::Array(values)) => values,
        _ => return Err("invalid agent registry".to_owned()),
    };
    let mut entries = std::collections::BTreeMap::new();
    for value in values {
        let get = |key: &str| value.object(key);
        let text = |key| {
            get(key)
                .and_then(Json::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "invalid agent registry".to_owned())
        };
        let integer = |key| {
            get(key)
                .and_then(Json::as_u64)
                .ok_or_else(|| "invalid agent registry".to_owned())
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
            leader_pid: integer("leader_pid")? as i32,
            process_group: integer("process_group")? as i32,
            started_at: text("started_at")?,
            deadline_at: match get("deadline_at") {
                Some(Json::String(v)) => Some(v.clone()),
                Some(Json::Null) => None,
                _ => return Err("invalid agent registry".to_owned()),
            },
            command: text("command")?,
            state: text("state")?,
            exit_code: get("exit_code").and_then(Json::as_u64).map(|v| v as i32),
            log_degraded: get("log_degraded").and_then(Json::as_bool).unwrap_or(false),
            audit_degraded: get("audit_degraded")
                .and_then(Json::as_bool)
                .unwrap_or(false),
            redacted: get("redacted").and_then(Json::as_bool).unwrap_or(false),
            stdout_next: integer("stdout_next")?,
            stderr_next: integer("stderr_next")?,
            stdout_dropped_before: integer("stdout_dropped_before")?,
            stderr_dropped_before: integer("stderr_dropped_before")?,
            log_next: integer("log_next")?,
            log_dropped_before: integer("log_dropped_before")?,
            first_output_at: match get("first_output_at") {
                Some(Json::String(value)) => Some(value.clone()),
                Some(Json::Null) | None => None,
                _ => return Err("invalid agent registry".to_owned()),
            },
            first_output_stream: match get("first_output_stream") {
                Some(Json::String(value)) => Some(value.clone()),
                Some(Json::Null) | None => None,
                _ => return Err("invalid agent registry".to_owned()),
            },
            first_output_bytes: match get("first_output_bytes") {
                Some(value) => Some(
                    value
                        .as_u64()
                        .ok_or_else(|| "invalid agent registry".to_owned())?,
                ),
                None => None,
            },
        };
        entries.insert(record.id.clone(), record);
    }
    Ok(entries)
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
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(value) => value.to_string(),
            Self::Number(value) => value.clone(),
            Self::String(value) => quote(value),
            Self::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(Self::to_json)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Object(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(key, value)| format!("{}:{}", quote(key), value.to_json()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    pub fn number(value: u64) -> Self {
        Self::Number(value.to_string())
    }
}

pub fn parse_json(input: &str) -> Result<Json, String> {
    let mut parser = Parser {
        input: input.as_bytes(),
        position: 0,
    };
    parser.space();
    let value = parser.value()?;
    parser.space();
    if parser.position != parser.input.len() {
        return Err("trailing data after JSON value".to_owned());
    }
    Ok(value)
}

pub fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character if character <= '\u{1f}' => {
                result.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => result.push(character),
        }
    }
    result.push('"');
    result
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

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self
            .input
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.space();
        match self.input.get(self.position) {
            Some(b'n') => {
                self.word(b"null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.word(b"true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.word(b"false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object_value(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err("expected JSON value".to_owned()),
        }
    }

    fn word(&mut self, word: &[u8]) -> Result<(), String> {
        if self.input.get(self.position..self.position + word.len()) == Some(word) {
            self.position += word.len();
            Ok(())
        } else {
            Err("invalid literal".to_owned())
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.position += 1;
        self.space();
        let mut values = Vec::new();
        if self.take(b']') {
            return Ok(Json::Array(values));
        }
        loop {
            values.push(self.value()?);
            self.space();
            if self.take(b']') {
                return Ok(Json::Array(values));
            }
            self.require(b',')?;
        }
    }

    fn object_value(&mut self) -> Result<Json, String> {
        self.position += 1;
        self.space();
        let mut fields = Vec::new();
        if self.take(b'}') {
            return Ok(Json::Object(fields));
        }
        loop {
            self.space();
            if self.input.get(self.position) != Some(&b'"') {
                return Err("object key must be a string".to_owned());
            }
            let key = self.string()?;
            self.space();
            self.require(b':')?;
            fields.push((key, self.value()?));
            self.space();
            if self.take(b'}') {
                return Ok(Json::Object(fields));
            }
            self.require(b',')?;
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.position;
        self.take(b'-');
        if self.take(b'0') {
        } else {
            self.digits()?;
        }
        if self.take(b'.') {
            self.digits()?;
        }
        if self.take(b'e') || self.take(b'E') {
            self.take(b'+');
            self.take(b'-');
            self.digits()?;
        }
        Ok(Json::Number(
            std::str::from_utf8(&self.input[start..self.position])
                .map_err(|_| "invalid number".to_owned())?
                .to_owned(),
        ))
    }

    fn digits(&mut self) -> Result<(), String> {
        let start = self.position;
        while self
            .input
            .get(self.position)
            .is_some_and(u8::is_ascii_digit)
        {
            self.position += 1;
        }
        if start == self.position {
            Err("expected digit".to_owned())
        } else {
            Ok(())
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.require(b'"')?;
        let mut value = String::new();
        loop {
            let byte = *self
                .input
                .get(self.position)
                .ok_or_else(|| "unterminated string".to_owned())?;
            self.position += 1;
            match byte {
                b'"' => return Ok(value),
                b'\\' => {
                    let escaped = *self
                        .input
                        .get(self.position)
                        .ok_or_else(|| "unfinished escape".to_owned())?;
                    self.position += 1;
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{08}'),
                        b'f' => value.push('\u{0c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let character = if (0xd800..=0xdbff).contains(&first) {
                                if !self.take(b'\\') || !self.take(b'u') {
                                    return Err("high surrogate without low surrogate".to_owned());
                                }
                                let second = self.hex4()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err("invalid low surrogate".to_owned());
                                }
                                char::from_u32(0x10000 + ((first - 0xd800) << 10) + second - 0xdc00)
                            } else {
                                char::from_u32(first)
                            }
                            .ok_or_else(|| "invalid unicode escape".to_owned())?;
                            value.push(character);
                        }
                        _ => return Err("invalid escape".to_owned()),
                    }
                }
                0..=0x1f => return Err("control character in string".to_owned()),
                _ => {
                    let width =
                        utf8_width(byte).ok_or_else(|| "invalid UTF-8 in string".to_owned())?;
                    let start = self.position - 1;
                    let end = start + width;
                    let text = std::str::from_utf8(
                        self.input
                            .get(start..end)
                            .ok_or_else(|| "truncated UTF-8".to_owned())?,
                    )
                    .map_err(|_| "invalid UTF-8 in string".to_owned())?;
                    value.push_str(text);
                    self.position = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let bytes = self
            .input
            .get(self.position..self.position + 4)
            .ok_or_else(|| "short unicode escape".to_owned())?;
        self.position += 4;
        std::str::from_utf8(bytes)
            .map_err(|_| "invalid unicode escape".to_owned())
            .and_then(|text| {
                u32::from_str_radix(text, 16).map_err(|_| "invalid unicode escape".to_owned())
            })
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.input.get(self.position) == Some(&expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }
    fn require(&mut self, expected: u8) -> Result<(), String> {
        self.space();
        if self.take(expected) {
            Ok(())
        } else {
            Err(format!("expected {}", expected as char))
        }
    }
}

fn utf8_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
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
    audit: AuditStore,
    inner: Mutex<Inner>,
    changed: Condvar,
}

/// Append-only execution archives.  They deliberately do not share the
/// delivery queue's retention policy: queue eviction is normal live-polling
/// behaviour, whereas these files answer historical timing questions.
const AUDIT_CAP_BYTES: u64 = 20 * 1024 * 1024;

struct AuditStore {
    directory: PathBuf,
    lock: Mutex<()>,
}

impl AuditStore {
    fn open(state_path: &Path) -> Result<Self, String> {
        let directory = state_path.with_extension("audit");
        fs::create_dir_all(&directory)
            .map_err(|_| "could not initialize execution audit store".to_owned())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&directory, fs::Permissions::from_mode(0o700));
        }
        Ok(Self {
            directory,
            lock: Mutex::new(()),
        })
    }

    fn append(&self, event: &Event) -> Result<(), String> {
        let Some(execution_id) = event
            .payload
            .object("execution_id")
            .and_then(Json::as_str)
            .filter(|value| !value.is_empty())
        else {
            return Ok(());
        };
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "execution audit lock poisoned".to_owned())?;
        let path = self
            .directory
            .join(format!("{}.jsonl", audit_filename(execution_id)));
        if fs::read_to_string(&path).ok().is_some_and(|contents| {
            contents
                .lines()
                .filter_map(|line| parse_json(line).ok())
                .any(|existing| {
                    existing.object("id").and_then(Json::as_str) == Some(event.id.as_str())
                })
        }) {
            return Ok(());
        }
        let line = event.response_json().to_json() + "\n";
        let result = (|| -> std::io::Result<()> {
            if !path.exists() {
                let _ = create_private(&path)?;
            }
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()?;
            Ok(())
        })();
        result.map_err(|_| "could_not_persist_execution_audit".to_owned())?;
        self.prune()?;
        Ok(())
    }

    fn events_for_task(&self, task_id: &str) -> Result<Vec<Json>, String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "execution audit lock poisoned".to_owned())?;
        let mut events = Vec::new();
        for entry in fs::read_dir(&self.directory)
            .map_err(|_| "could not read execution audit store".to_owned())?
        {
            let entry = entry.map_err(|_| "could not read execution audit store".to_owned())?;
            if !entry
                .file_type()
                .map_err(|_| "could not read execution audit store".to_owned())?
                .is_file()
            {
                continue;
            }
            let contents = fs::read_to_string(entry.path())
                .map_err(|_| "could not read execution audit log".to_owned())?;
            events.extend(
                contents
                    .lines()
                    .filter_map(|line| parse_json(line).ok())
                    .filter(|event| {
                        event.object("task_id").and_then(Json::as_str) == Some(task_id)
                    }),
            );
        }
        events.sort_by(|left, right| {
            left.object("received_at")
                .and_then(Json::as_str)
                .cmp(&right.object("received_at").and_then(Json::as_str))
                .then_with(|| {
                    left.object("sequence")
                        .and_then(Json::as_u64)
                        .cmp(&right.object("sequence").and_then(Json::as_u64))
                })
        });
        Ok(events)
    }

    fn prune(&self) -> Result<(), String> {
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.directory)
            .map_err(|_| "could not read execution audit store".to_owned())?
        {
            let entry = entry.map_err(|_| "could not read execution audit store".to_owned())?;
            let metadata = entry
                .metadata()
                .map_err(|_| "could not read execution audit store".to_owned())?;
            if metadata.is_file() {
                total = total.saturating_add(metadata.len());
                let oldest = fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|contents| {
                        contents
                            .lines()
                            .next()
                            .and_then(|line| parse_json(line).ok())
                    })
                    .and_then(|event| {
                        event
                            .object("received_at")
                            .and_then(Json::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| {
                        metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                            .map(|time| format!("{:020}", time.as_secs()))
                            .unwrap_or_default()
                    });
                files.push((oldest, entry.path(), metadata.len()));
            }
        }
        files.sort_by_key(|(oldest, path, _)| (oldest.clone(), path.clone()));
        for (_, path, length) in files {
            if total <= AUDIT_CAP_BYTES {
                break;
            }
            fs::remove_file(path)
                .map_err(|_| "could not prune execution audit store".to_owned())?;
            total = total.saturating_sub(length);
        }
        Ok(())
    }
}

fn audit_filename(execution_id: &str) -> String {
    execution_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        let inner = match fs::read_to_string(&path) {
            Ok(contents) => decode_state(&contents, limit)?,
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
        Ok(Self {
            audit: AuditStore::open(&path)?,
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
        self.audit.append(&event)?;
        updated.next_sequence += 1;
        updated.events.push_back(event.clone());
        if updated.events.len() > self.limit {
            updated.events.pop_front();
        }
        self.save(&updated)?;
        *inner = updated;
        self.changed.notify_all();
        Ok((event, false))
    }

    /// Reads the durable archive; this intentionally ignores the live queue.
    pub fn timeline(&self, task_id: &str) -> Result<Vec<Json>, String> {
        self.audit.events_for_task(task_id)
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
        let first = inner
            .events
            .front()
            .map_or(inner.next_sequence, |event| event.sequence);
        let lost = after < first.saturating_sub(1);
        Ok(ReadResult {
            epoch: inner.epoch.clone(),
            reset,
            lost,
            events: inner
                .events
                .iter()
                .filter(|event| event.sequence > after)
                .cloned()
                .collect(),
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

    fn agent(id: &str) -> AgentRecord {
        AgentRecord {
            id: id.to_owned(),
            task_id: "task-1".to_owned(),
            execution_id: "execution-1".to_owned(),
            leader_pid: 42,
            process_group: 42,
            started_at: "1".to_owned(),
            deadline_at: None,
            command: "codex exec".to_owned(),
            state: "running".to_owned(),
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
        }
    }

    #[test]
    fn agent_registry_recovers_running_groups_and_redacts_durable_logs() {
        let file = path("agents");
        let registry = AgentRegistry::open(&file).unwrap();
        registry.register(agent("live")).unwrap();
        let mut gone = agent("gone");
        gone.process_group = 99;
        registry.register(gone).unwrap();
        let changed = registry.recover(|group| group == 42).unwrap();
        assert_eq!(changed.len(), 2);
        assert_eq!(registry.get("live").unwrap().state, "orphaned");
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
        assert_eq!(store.read(0, "", Duration::ZERO).unwrap().events.len(), 1);
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
    fn execution_audit_reopens_and_keeps_execution_files_separate() {
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
        assert_eq!(
            fs::read_dir(file.with_extension("audit")).unwrap().count(),
            2
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("audit"));
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
    fn failed_live_state_save_does_not_duplicate_audit_on_retry() {
        let file = path("audit-retry");
        let store = Store::open(&file, 10).unwrap();
        fs::create_dir(&file).unwrap();
        let payload = audit_event("retry", "task_dispatched");
        assert!(store.add(payload.clone()).is_err());
        fs::remove_dir(&file).unwrap();
        assert!(store.add(payload).is_ok());
        assert_eq!(store.timeline("task-1").unwrap().len(), 1);
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("audit"));
    }

    #[test]
    fn audit_retention_prunes_the_oldest_execution_first() {
        let file = path("audit-cap");
        let store = Store::open(&file, 1).unwrap();
        let mut first = audit_event("first", "task_dispatched");
        let mut second = audit_event("second", "task_dispatched");
        let blob = Json::String("x".repeat((AUDIT_CAP_BYTES / 2 + 1024) as usize));
        for (event, execution_id) in [(&mut first, "old"), (&mut second, "new")] {
            if let Json::Object(fields) = event {
                for (name, value) in fields {
                    if name == "execution_id" {
                        *value = Json::String(execution_id.to_owned());
                    }
                    if name == "payload" {
                        *value = Json::Object(vec![("blob".to_owned(), blob.clone())]);
                    }
                }
            }
        }
        store.add(first).unwrap();
        store.add(second).unwrap();
        let retained = store.timeline("task-1").unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].object("execution_id").and_then(Json::as_str),
            Some("new")
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(file.with_extension("audit"));
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
    fn bounded_queue_marks_eviction_as_lost() {
        let file = path("eviction");
        let store = Store::open(&file, 2).unwrap();
        store.add(event("one")).unwrap();
        store.add(event("two")).unwrap();
        store.add(event("three")).unwrap();
        let result = store.read(0, "", Duration::ZERO).unwrap();
        assert!(result.lost);
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
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
    fn failed_persist_does_not_accept_or_expose_event() {
        let parent = path("read-only-directory");
        fs::create_dir(&parent).unwrap();
        let store = Store::open(parent.join("events.json"), 10).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        }
        assert!(store.add(event("not-durable")).is_err());
        let result = store.read(0, "", Duration::ZERO).unwrap();
        assert!(result.events.is_empty());
        assert_eq!(result.next, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        }
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
