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
    pub leader_pid: i32,
    pub process_group: i32,
    pub started_at: String,
    pub deadline_at: Option<String>,
    pub command: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub log_degraded: bool,
    pub redacted: bool,
    pub stdout_next: u64,
    pub stderr_next: u64,
    pub stdout_dropped_before: u64,
    pub stderr_dropped_before: u64,
}

impl AgentRecord {
    pub fn status_json(&self) -> Json {
        Json::Object(vec![
            ("id".to_owned(), Json::String(self.id.clone())),
            ("task_id".to_owned(), Json::String(self.task_id.clone())),
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
            ("redacted".to_owned(), Json::Bool(self.redacted)),
            (
                "stdout_dropped_before".to_owned(),
                Json::number(self.stdout_dropped_before),
            ),
            (
                "stderr_dropped_before".to_owned(),
                Json::number(self.stderr_dropped_before),
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
            let _ = fs::remove_file(self.log_path(id));
        }
        if !removed.is_empty() {
            self.save(&updated)?;
            *entries = updated;
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
        let (next, dropped) = if stream == "stdout" {
            (&mut entry.stdout_next, &mut entry.stdout_dropped_before)
        } else {
            (&mut entry.stderr_next, &mut entry.stderr_dropped_before)
        };
        let cursor = *next;
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
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
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
            let content = fs::read_to_string(&log_path).unwrap_or_default();
            let keep_from = content.len().saturating_sub(AGENT_LOG_CAP as usize);
            let keep_from = content[keep_from..]
                .find('\n')
                .map_or(content.len(), |offset| keep_from + offset + 1);
            let kept = &content[keep_from..];
            if let Some(Json::Object(fields)) = kept.lines().next().and_then(|v| parse_json(v).ok())
                && let Some(value) = Json::Object(fields).object("cursor").and_then(Json::as_u64)
            {
                *dropped = (*dropped).max(value);
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
        let entry = self.get(id)?;
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
        let (next_cursor, dropped_before) = match stream {
            "stderr" => (entry.stderr_next, entry.stderr_dropped_before),
            "stdout" => (entry.stdout_next, entry.stdout_dropped_before),
            _ => (
                entry.stdout_next.max(entry.stderr_next),
                entry.stdout_dropped_before.max(entry.stderr_dropped_before),
            ),
        };
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
            redacted: get("redacted").and_then(Json::as_bool).unwrap_or(false),
            stdout_next: integer("stdout_next")?,
            stderr_next: integer("stderr_next")?,
            stdout_dropped_before: integer("stdout_dropped_before")?,
            stderr_dropped_before: integer("stderr_dropped_before")?,
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
    inner: Mutex<Inner>,
    changed: Condvar,
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
            path,
            limit,
            inner: Mutex::new(inner),
            changed: Condvar::new(),
        })
    }

    pub fn add(&self, payload: Json) -> Result<(Event, bool), String> {
        let id = event_id(&payload)?;
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
            received_at: timestamp(),
            payload,
        };
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
fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
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

    fn agent(id: &str) -> AgentRecord {
        AgentRecord {
            id: id.to_owned(),
            task_id: "task-1".to_owned(),
            leader_pid: 42,
            process_group: 42,
            started_at: "1".to_owned(),
            deadline_at: None,
            command: "codex exec".to_owned(),
            state: "running".to_owned(),
            exit_code: None,
            log_degraded: false,
            redacted: false,
            stdout_next: 0,
            stderr_next: 0,
            stdout_dropped_before: 0,
            stderr_dropped_before: 0,
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
