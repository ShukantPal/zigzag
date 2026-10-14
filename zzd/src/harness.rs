//! Native, session-oriented harness controls.
//!
//! App-server threads are execution sessions, not security principals: callers
//! must authorize with the Zigzag agent ID before resolving a thread ID.
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Steer,
    Queue,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) enum HarnessEvent {
    Notification { value: Value, terminal_turn: bool },
}

#[allow(dead_code)]
pub(crate) trait HarnessSession: Send + Sync {
    fn send(&self, text: &str, delivery: Delivery) -> Result<(), String>;
    fn interrupt(&self) -> Result<(), String>;
}

struct Shared {
    writer: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<HarnessEvent>>>>,
    turns: Mutex<HashMap<String, String>>,
    queued: Mutex<HashMap<String, VecDeque<String>>>,
    approval_contexts: Mutex<HashMap<String, ApprovalContext>>,
    next_id: AtomicU64,
}

#[derive(Clone, Debug)]
struct ApprovalContext {
    agent_id: String,
    worktree: std::path::PathBuf,
    branch: String,
}

/// One Codex app-server per daemon isolation domain. The daemon's current
/// domain is its local user + Codex home/config/auth + trust policy.
pub(crate) struct CodexServer {
    shared: Arc<Shared>,
}

struct OpenCodeShared {
    base_url: String,
    child: Mutex<Child>,
    sessions: Mutex<std::collections::HashSet<String>>,
}

fn drain_opencode_pipe(pipe: impl std::io::Read + Send + 'static, label: &'static str) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            log::debug!("opencode {label}: {}", line.trim_end());
            line.clear();
        }
    });
}

/// One loopback-only OpenCode HTTP server per daemon isolation domain.
/// Individual agents own durable OpenCode sessions addressed by session ID.
pub(crate) struct OpenCodeServer {
    shared: Arc<OpenCodeShared>,
}

impl OpenCodeServer {
    pub(crate) fn start() -> Result<Arc<Self>, String> {
        let reservation = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("could not reserve OpenCode port: {error}"))?;
        let port = reservation
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        drop(reservation);
        let mut child = Command::new("opencode")
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start opencode serve: {error}"))?;
        drain_opencode_pipe(
            child.stdout.take().ok_or("OpenCode stdout unavailable")?,
            "stdout",
        );
        drain_opencode_pipe(
            child.stderr.take().ok_or("OpenCode stderr unavailable")?,
            "stderr",
        );
        let shared = Arc::new(OpenCodeShared {
            base_url: format!("http://127.0.0.1:{port}"),
            child: Mutex::new(child),
            sessions: Mutex::new(std::collections::HashSet::new()),
        });
        let server = Arc::new(Self { shared });
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if !server.is_running() {
                return Err("opencode serve exited during startup".to_owned());
            }
            if server.get("/api/health").is_ok() {
                return Ok(server);
            }
            if std::time::Instant::now() >= deadline {
                return Err("opencode serve health check timed out".to_owned());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(not(test))]
    pub(crate) fn start_session(
        self: &Arc<Self>,
        cwd: &str,
        prompt: &str,
        model: Option<&str>,
        approval: &str,
    ) -> Result<String, String> {
        let mut body = json!({"location":{"directory":cwd}});
        if let Some(model) = model
            && let Some((provider_id, model_id)) = model.split_once('/')
        {
            body["model"] = json!({"providerID":provider_id,"id":model_id});
        }
        let created = self.post("/api/session", body)?;
        let session_id = created
            .pointer("/data/id")
            .or_else(|| created.get("id"))
            .and_then(Value::as_str)
            .ok_or("OpenCode session create returned no session ID")?
            .to_owned();
        let permission = match approval {
            "full-auto" => json!([{"permission":"*","pattern":"*","action":"allow"}]),
            "auto-edit" => json!([
                {"permission":"*","pattern":"*","action":"ask"},
                {"permission":"read","pattern":"*","action":"allow"},
                {"permission":"edit","pattern":"*","action":"allow"},
                {"permission":"glob","pattern":"*","action":"allow"},
                {"permission":"grep","pattern":"*","action":"allow"},
                {"permission":"list","pattern":"*","action":"allow"}
            ]),
            _ => json!([{"permission":"*","pattern":"*","action":"ask"}]),
        };
        let encoded_cwd: String = url::form_urlencoded::byte_serialize(cwd.as_bytes()).collect();
        self.patch(
            &format!("/session/{session_id}?directory={encoded_cwd}"),
            json!({"permission":permission}),
        )?;
        self.resume_session(&session_id)?;
        self.session(session_id.clone())
            .send(prompt, Delivery::Steer)?;
        Ok(session_id)
    }

    pub(crate) fn is_running(&self) -> bool {
        self.shared
            .child
            .lock()
            .map(|mut child| child.try_wait().ok().flatten().is_none())
            .unwrap_or(false)
    }

    pub(crate) fn stop(&self) -> Result<(), String> {
        let mut child = self
            .shared
            .child
            .lock()
            .map_err(|_| "OpenCode child lock poisoned")?;
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_none()
        {
            child.kill().map_err(|error| error.to_string())?;
        }
        child.wait().map_err(|error| error.to_string())?;
        Ok(())
    }

    #[cfg(not(test))]
    pub(crate) fn pid(&self) -> u32 {
        self.shared
            .child
            .lock()
            .map(|child| child.id())
            .unwrap_or_default()
    }

    pub(crate) fn resume_session(&self, session_id: &str) -> Result<(), String> {
        self.get(&format!("/api/session/{session_id}"))?;
        self.shared
            .sessions
            .lock()
            .map_err(|_| "OpenCode session map poisoned")?
            .insert(session_id.to_owned());
        Ok(())
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let response = ureq::get(&format!("{}{path}", self.shared.base_url))
            .call()
            .map_err(|error| format!("OpenCode GET {path} failed: {error}"))?;
        response
            .into_json()
            .map_err(|error| format!("OpenCode GET {path} returned invalid JSON: {error}"))
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let response = ureq::post(&format!("{}{path}", self.shared.base_url))
            .send_json(body)
            .map_err(|error| format!("OpenCode POST {path} failed: {error}"))?;
        if response.status() == 204 {
            return Ok(Value::Null);
        }
        response
            .into_json()
            .map_err(|error| format!("OpenCode POST {path} returned invalid JSON: {error}"))
    }

    #[cfg(not(test))]
    fn patch(&self, path: &str, body: Value) -> Result<Value, String> {
        let response = ureq::patch(&format!("{}{path}", self.shared.base_url))
            .send_json(body)
            .map_err(|error| format!("OpenCode PATCH {path} failed: {error}"))?;
        response
            .into_json()
            .map_err(|error| format!("OpenCode PATCH {path} returned invalid JSON: {error}"))
    }

    pub(crate) fn session(self: &Arc<Self>, session_id: String) -> Arc<dyn HarnessSession> {
        Arc::new(OpenCodeSession {
            server: Arc::clone(self),
            session_id,
        })
    }
}

impl CodexServer {
    pub(crate) fn start() -> Result<Arc<Self>, String> {
        let mut child = Command::new("codex")
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start codex app-server: {e}"))?;
        let writer = child.stdin.take().ok_or("app-server stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("app-server stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("app-server stderr unavailable")?;
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                log::debug!("codex app-server: {}", line.trim_end());
                line.clear();
            }
        });
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            approval_contexts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || read_messages(stdout, reader_shared));
        let server = Arc::new(Self { shared });
        server.rpc(
            "initialize",
            json!({"clientInfo":{"name":"zigzag","version":env!("CARGO_PKG_VERSION")}}),
        )?;
        server.notify("initialized", json!({}))?;
        Ok(server)
    }

    #[allow(dead_code)]
    pub(crate) fn start_thread(
        &self,
        agent_id: &str,
        cwd: &str,
        branch: &str,
        prompt: &str,
    ) -> Result<(String, mpsc::Receiver<HarnessEvent>), String> {
        let response = self.rpc("thread/start", json!({"cwd":cwd}))?;
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .or_else(|| response.pointer("/threadId").and_then(Value::as_str))
            .ok_or("app-server thread/start returned no thread ID")?
            .to_owned();
        // Attach the listener before turn/start. A short turn can complete
        // before turn/start returns, so subscribing in the caller afterward
        // would silently drop its completion notification.
        let (event_tx, event_rx) = mpsc::channel();
        self.shared
            .subscribers
            .lock()
            .map_err(|_| "session event map poisoned")?
            .entry(thread_id.clone())
            .or_default()
            .push(event_tx);
        let worktree = std::fs::canonicalize(cwd).unwrap_or_else(|_| std::path::PathBuf::from(cwd));
        self.shared
            .approval_contexts
            .lock()
            .map_err(|_| "approval context map poisoned")?
            .insert(
                thread_id.clone(),
                ApprovalContext {
                    agent_id: agent_id.to_owned(),
                    worktree,
                    branch: branch.to_owned(),
                },
            );
        let turn = self.rpc(
            "turn/start",
            json!({"threadId":thread_id,"input":[{"type":"text","text":prompt}]}),
        )?;
        if let Some(turn_id) = turn.pointer("/turn/id").and_then(Value::as_str) {
            self.shared
                .turns
                .lock()
                .map_err(|_| "turn map poisoned")?
                .insert(thread_id.clone(), turn_id.to_owned());
        }
        Ok((thread_id, event_rx))
    }

    pub(crate) fn resume_thread(&self, thread_id: &str) -> Result<(), String> {
        let response = self.rpc("thread/resume", json!({"threadId":thread_id}))?;
        if let Some(event) = resumed_completion_event(thread_id, &response)
            && let Ok(mut subscribers) = self.shared.subscribers.lock()
            && let Some(list) = subscribers.get_mut(thread_id)
        {
            list.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
        Ok(())
    }

    /// Subscribe before resuming so notifications emitted during the resume
    /// RPC are delivered to the recovered agent's monitor.
    pub(crate) fn subscribe_thread(
        &self,
        thread_id: &str,
    ) -> Result<mpsc::Receiver<HarnessEvent>, String> {
        let (event_tx, event_rx) = mpsc::channel();
        self.shared
            .subscribers
            .lock()
            .map_err(|_| "session event map poisoned")?
            .entry(thread_id.to_owned())
            .or_default()
            .push(event_tx);
        Ok(event_rx)
    }
    #[allow(dead_code)]
    pub(crate) fn pid(&self) -> u32 {
        self.shared
            .child
            .lock()
            .map(|child| child.id())
            .unwrap_or_default()
    }
    pub(crate) fn is_running(&self) -> bool {
        self.shared
            .child
            .lock()
            .map(|mut child| child.try_wait().ok().flatten().is_none())
            .unwrap_or(false)
    }
    pub(crate) fn stop(&self) -> Result<(), String> {
        let mut child = self
            .shared
            .child
            .lock()
            .map_err(|_| "app-server child lock poisoned")?;
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_none()
        {
            child.kill().map_err(|error| error.to_string())?;
        }
        child.wait().map_err(|error| error.to_string())?;
        Ok(())
    }
    fn has_thread(&self, thread_id: &str) -> bool {
        self.shared
            .subscribers
            .lock()
            .map(|map| map.contains_key(thread_id))
            .unwrap_or(false)
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.shared
            .pending
            .lock()
            .map_err(|_| "app-server request map poisoned")?
            .insert(id, tx);
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let sent = (|| {
            let mut stdin = self
                .shared
                .writer
                .lock()
                .map_err(|_| "app-server stdin poisoned")?;
            serde_json::to_writer(&mut *stdin, &request).map_err(|e| e.to_string())?;
            stdin.write_all(b"\n").map_err(|e| e.to_string())?;
            stdin.flush().map_err(|e| e.to_string())
        })();
        if let Err(error) = sent {
            self.shared.pending.lock().ok().map(|mut p| p.remove(&id));
            return Err(error);
        }
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.shared.pending.lock().ok().map(|mut p| p.remove(&id));
                Err(format!("app-server {method} timed out"))
            }
        }
    }
    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let value = json!({"jsonrpc":"2.0","method":method,"params":params});
        let mut stdin = self
            .shared
            .writer
            .lock()
            .map_err(|_| "app-server stdin poisoned")?;
        serde_json::to_writer(&mut *stdin, &value).map_err(|e| e.to_string())?;
        stdin.write_all(b"\n").map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())
    }
    pub(crate) fn session(self: &Arc<Self>, thread_id: String) -> Arc<dyn HarnessSession> {
        Arc::new(CodexSession {
            server: Arc::clone(self),
            thread_id,
        })
    }
}

/// Build the same event consumed by the notification monitor when resume
/// returns an idle thread whose latest persisted turn is terminal.
pub(crate) fn resumed_completion_event(thread_id: &str, response: &Value) -> Option<HarnessEvent> {
    if response
        .pointer("/thread/status/type")
        .and_then(Value::as_str)
        != Some("idle")
    {
        return None;
    }
    let turn = response.pointer("/thread/turns")?.as_array()?.last()?;
    matches!(
        turn.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "interrupted")
    )
    .then(|| HarnessEvent::Notification {
        value: json!({
            "method":"turn/completed",
            "params":{"threadId":thread_id,"turn":turn}
        }),
        terminal_turn: true,
    })
}

struct CodexSession {
    server: Arc<CodexServer>,
    thread_id: String,
}
impl HarnessSession for CodexSession {
    fn send(&self, text: &str, delivery: Delivery) -> Result<(), String> {
        if !self.server.has_thread(&self.thread_id) {
            self.server.resume_thread(&self.thread_id)?;
            self.server
                .shared
                .subscribers
                .lock()
                .map_err(|_| "session event map poisoned")?
                .entry(self.thread_id.clone())
                .or_default();
        }
        match delivery {
            Delivery::Steer => {
                let turn_id = self
                    .server
                    .shared
                    .turns
                    .lock()
                    .map_err(|_| "turn map poisoned")?
                    .get(&self.thread_id)
                    .cloned()
                    .ok_or("agent has no active Codex turn")?;
                self.server.rpc(
                    "turn/steer",
                    json!({"threadId":self.thread_id,"expectedTurnId":turn_id,"input":[{"type":"text","text":text}]}),
                )?;
            }
            Delivery::Queue => {
                let active = self
                    .server
                    .shared
                    .turns
                    .lock()
                    .map_err(|_| "turn map poisoned")?
                    .contains_key(&self.thread_id);
                if active {
                    self.server
                        .shared
                        .queued
                        .lock()
                        .map_err(|_| "turn queue poisoned")?
                        .entry(self.thread_id.clone())
                        .or_default()
                        .push_back(text.to_owned());
                } else {
                    let turn = self.server.rpc(
                        "turn/start",
                        json!({"threadId":self.thread_id,"input":[{"type":"text","text":text}]}),
                    )?;
                    if let Some(turn_id) = turn.pointer("/turn/id").and_then(Value::as_str) {
                        self.server
                            .shared
                            .turns
                            .lock()
                            .map_err(|_| "turn map poisoned")?
                            .insert(self.thread_id.clone(), turn_id.to_owned());
                    }
                }
            }
        }
        Ok(())
    }
    fn interrupt(&self) -> Result<(), String> {
        // A completed turn is already idle. Treat stopping that session as
        // successful, while still propagating failures from a real interrupt
        // request below so the registry keeps tracking sessions we failed to
        // stop.
        let turn_id = match self
            .server
            .shared
            .turns
            .lock()
            .map_err(|_| "turn map poisoned")?
            .get(&self.thread_id)
            .cloned()
        {
            Some(turn_id) => turn_id,
            None => return Ok(()),
        };
        self.server
            .rpc(
                "turn/interrupt",
                json!({"threadId":self.thread_id,"turnId":turn_id}),
            )
            .map(|_| ())
    }
}

struct OpenCodeSession {
    server: Arc<OpenCodeServer>,
    session_id: String,
}

impl HarnessSession for OpenCodeSession {
    fn send(&self, text: &str, delivery: Delivery) -> Result<(), String> {
        if !self
            .server
            .shared
            .sessions
            .lock()
            .map_err(|_| "OpenCode session map poisoned")?
            .contains(&self.session_id)
        {
            // A daemon restart creates a fresh HTTP server. OpenCode sessions
            // themselves are durable, so verify the ID and reattach it.
            self.server
                .get(&format!("/api/session/{}", self.session_id))?;
            self.server
                .shared
                .sessions
                .lock()
                .map_err(|_| "OpenCode session map poisoned")?
                .insert(self.session_id.clone());
        }
        self.server.post(
            &format!("/api/session/{}/prompt", self.session_id),
            json!({
                "prompt":{"text":text},
                "delivery":match delivery { Delivery::Steer => "steer", Delivery::Queue => "queue" }
            }),
        )?;
        Ok(())
    }

    fn interrupt(&self) -> Result<(), String> {
        ureq::post(&format!(
            "{}/api/session/{}/interrupt",
            self.server.shared.base_url, self.session_id
        ))
        .call()
        .map(|_| ())
        .map_err(|error| format!("OpenCode interrupt failed: {error}"))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ApprovalDecision {
    allow: bool,
    rule: &'static str,
    operation: String,
}

fn decision(allow: bool, rule: &'static str, operation: String) -> ApprovalDecision {
    ApprovalDecision {
        allow,
        rule,
        operation,
    }
}

fn approval_audit_line(
    id: &Value,
    agent: &str,
    operation: &str,
    decision: &str,
    rule: &str,
) -> String {
    format!(
        "codex approval id={id} agent={agent} operation={operation:?} decision={decision} policy_rule={rule}"
    )
}

fn path_is_within(path: &std::path::Path, root: &std::path::Path) -> bool {
    fn resolved(path: &std::path::Path) -> std::path::PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        if let Ok(path) = std::fs::canonicalize(&absolute) {
            return path;
        }
        let mut missing = Vec::new();
        let mut existing = absolute.as_path();
        while !existing.exists() {
            let Some(name) = existing.file_name() else {
                break;
            };
            missing.push(name.to_os_string());
            let Some(parent) = existing.parent() else {
                break;
            };
            existing = parent;
        }
        let mut resolved =
            std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
        for name in missing.into_iter().rev() {
            if name == ".." {
                resolved.pop();
            } else if name != "." {
                resolved.push(name);
            }
        }
        resolved
    }
    resolved(path).starts_with(resolved(root))
}

fn command_words(params: &Value) -> Option<Vec<String>> {
    match params.get("command")? {
        Value::Array(parts) => parts
            .iter()
            .map(|part| part.as_str().map(str::to_owned))
            .collect(),
        // App-server versions that send a string get a deliberately small
        // grammar: no quoting, escapes, or shell operators are interpreted.
        Value::String(command)
            if !command.is_empty()
                && !command.chars().any(|ch| {
                    matches!(
                        ch,
                        '\'' | '"' | '\\' | ';' | '|' | '&' | '>' | '<' | '$' | '`' | '\n' | '\r'
                    )
                }) =>
        {
            Some(command.split_whitespace().map(str::to_owned).collect())
        }
        _ => None,
    }
}

fn approval_audit_operation(words: Option<&[String]>, params: &Value) -> String {
    let Some(words) = words else {
        let raw = params
            .get("command")
            .map(Value::to_string)
            .unwrap_or_else(|| "<missing command>".to_owned());
        let lower = raw.to_ascii_lowercase();
        return if [
            "token",
            "password",
            "secret",
            "credential",
            "authorization",
            "api_key",
            "api-key",
            ".env",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            "<command redacted: credential-like content>".to_owned()
        } else {
            raw
        };
    };
    let mut redact_next = false;
    words
        .iter()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            let sensitive_name = [
                "token",
                "password",
                "secret",
                "credential",
                "authorization",
                "api_key",
                "api-key",
            ]
            .iter()
            .any(|name| lower.contains(name));
            if redact_next {
                redact_next = false;
                return "[REDACTED]".to_owned();
            }
            if sensitive_name && !lower.contains('=') {
                redact_next = true;
                return word.clone();
            }
            if sensitive_name && word.contains('=') {
                return format!("{}=[REDACTED]", word.split_once('=').unwrap().0);
            }
            if [
                "ghp_",
                "gho_",
                "ghu_",
                "ghs_",
                "ghr_",
                "github_pat_",
                "akia",
                "sk-",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            {
                return "[REDACTED]".to_owned();
            }
            word.clone()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_outside_path(words: &[String]) -> bool {
    words.iter().skip(1).any(|word| {
        let candidate = word
            .split_once('=')
            .map_or(word.as_str(), |(_, value)| value);
        let path = std::path::Path::new(candidate);
        (path.is_absolute() && candidate != "/tmp")
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
    })
}

fn git_push_targets_own_branch(args: &[String], branch: &str) -> bool {
    if branch.is_empty() || matches!(branch, "main" | "master") {
        return false;
    }
    let mut rest = args;
    if rest.first().is_some_and(|arg| arg == "--force-with-lease") {
        rest = &rest[1..];
    }
    if rest.len() != 2 || rest[0] != "origin" || rest[1].starts_with([':', '+']) {
        return false;
    }
    let destination = rest[1]
        .rsplit_once(':')
        .map_or(rest[1].as_str(), |(_, destination)| destination)
        .strip_prefix("refs/heads/")
        .unwrap_or_else(|| {
            rest[1]
                .rsplit_once(':')
                .map_or(rest[1].as_str(), |(_, destination)| destination)
        });
    destination == branch
}

fn mktemp_is_tmp_scoped(args: &[String]) -> bool {
    let mut tmpdir = false;
    let mut absolute_template = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-d" | "-u" | "-q" => index += 1,
            "-p" | "--tmpdir" => {
                if args.get(index + 1).map(String::as_str) != Some("/tmp") {
                    return false;
                }
                tmpdir = true;
                index += 2;
            }
            "--tmpdir=/tmp" => {
                tmpdir = true;
                index += 1;
            }
            "-t" => {
                let Some(template) = args.get(index + 1) else {
                    return false;
                };
                if template.contains('/') || template.contains("..") {
                    return false;
                }
                index += 2;
            }
            value if value.starts_with("/tmp/") => {
                let path = std::path::Path::new(value);
                if path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return false;
                }
                absolute_template = true;
                index += 1;
            }
            value
                if value.starts_with('/')
                    || value.starts_with('-')
                    || value.contains('/')
                    || value.contains("..") =>
            {
                return false;
            }
            _ => index += 1,
        }
    }
    tmpdir || absolute_template
}

fn gh_read_command(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("pr")
            if args
                .get(1)
                .is_some_and(|sub| matches!(sub.as_str(), "view" | "checks")) =>
        {
            args.iter().skip(2).all(|arg| {
                if arg == "--repo" {
                    return false;
                }
                !matches!(arg.as_str(), "--web" | "--delete-branch")
            }) && !args.iter().skip(2).any(|arg| arg.starts_with("--repo="))
        }
        Some("run") if args.get(1).is_some_and(|sub| sub == "view") => args
            .iter()
            .skip(2)
            .all(|arg| !matches!(arg.as_str(), "--web" | "--log")),
        Some("api") => {
            let mut endpoint = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--paginate" | "--slurp" | "--silent" => i += 1,
                    "--jq" | "--template" => {
                        if i + 1 >= args.len() {
                            return false;
                        }
                        i += 2;
                    }
                    flag if flag.starts_with('-') => return false,
                    value if endpoint.replace(value).is_some() => return false,
                    _ => i += 1,
                }
            }
            endpoint.is_some_and(|path| {
                let lower = path.to_ascii_lowercase();
                !["secret", "token", "credential", ".env", "contents"]
                    .iter()
                    .any(|needle| lower.contains(needle))
                    && !path.contains('%')
                    && !path.contains('\\')
                    && !path.split('/').any(|segment| matches!(segment, "." | ".."))
                    && path.starts_with("repos/")
                    && path.split('/').count() >= 4
            })
        }
        _ => false,
    }
}

fn gh_create_for_own_branch(args: &[String], branch: &str) -> bool {
    if args.first().map(String::as_str) != Some("pr")
        || args.get(1).map(String::as_str) != Some("create")
        || branch.is_empty()
        || matches!(branch, "main" | "master")
    {
        return false;
    }
    let mut head = None;
    let mut i = 2;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(value) = arg.strip_prefix("--head=") {
            if head.replace(value).is_some() {
                return false;
            }
            i += 1;
            continue;
        }
        let value_option = matches!(arg, "--head" | "--title" | "--body" | "--base");
        if value_option {
            let Some(value) = args.get(i + 1) else {
                return false;
            };
            if arg == "--head" && head.replace(value.as_str()).is_some() {
                return false;
            }
            i += 2;
            continue;
        }
        if matches!(arg, "--draft" | "--fill") {
            i += 1;
            continue;
        }
        // File inputs, repo overrides, web launch, and unknown flags fail closed.
        return false;
    }
    head.is_none_or(|head| head == branch)
}

fn network_command_is_allowlisted(words: &[String], branch: &str) -> bool {
    let Some(executable) = words
        .first()
        .and_then(|word| std::path::Path::new(word).file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let args = &words[1..];
    match executable {
        "git" if args.first().is_some_and(|arg| arg == "push") => {
            git_push_targets_own_branch(&args[1..], branch)
        }
        "gh" => gh_create_for_own_branch(args, branch) || gh_read_command(args),
        _ => false,
    }
}

fn package_manager_command_is_safe(executable: &str, args: &[String]) -> bool {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return false;
    };
    if matches!(subcommand, "test" | "build" | "lint" | "check") {
        return true;
    }
    matches!(subcommand, "run")
        && args
            .get(1)
            .is_some_and(|script| matches!(script.as_str(), "build" | "test" | "lint" | "check"))
        && matches!(executable, "npm" | "pnpm" | "yarn")
}

fn requested_access_is_worktree_scoped(
    params: &Value,
    root: &std::path::Path,
    allow_network: bool,
) -> bool {
    let requested_network = params.get("networkApprovalContext").is_some()
        || params.pointer("/additionalPermissions/network/enabled") == Some(&Value::Bool(true));
    let Some(filesystem) = params.pointer("/additionalPermissions/fileSystem") else {
        return !requested_network || allow_network;
    };
    for key in ["read", "write"] {
        if let Some(paths) = filesystem.get(key).and_then(Value::as_array)
            && paths.iter().any(|path| {
                path.as_str()
                    .is_none_or(|path| !path_is_within(std::path::Path::new(path), root))
            })
        {
            return false;
        }
    }
    if let Some(entries) = filesystem.get("entries").and_then(Value::as_array)
        && entries.iter().any(|entry| {
            let path = entry.pointer("/path/path").and_then(Value::as_str);
            path.is_none_or(|path| !path_is_within(std::path::Path::new(path), root))
        })
    {
        return false;
    }
    !requested_network || allow_network
}

fn command_approval(params: &Value, context: Option<&ApprovalContext>) -> ApprovalDecision {
    let words = command_words(params);
    let command = words
        .as_ref()
        .map(|words| words.join(" "))
        .unwrap_or_default();
    let operation = approval_audit_operation(words.as_deref(), params);
    let Some(context) = context else {
        return decision(false, "unknown-agent-deny", operation);
    };
    let Some(cwd) = params.get("cwd").and_then(Value::as_str) else {
        return decision(false, "missing-cwd-deny", operation);
    };
    if !path_is_within(std::path::Path::new(cwd), &context.worktree) {
        return decision(false, "cwd-outside-worktree-deny", operation);
    }
    let Some(words) = words else {
        return decision(false, "invalid-command-encoding-deny", operation);
    };
    if !requested_access_is_worktree_scoped(
        params,
        &context.worktree,
        network_command_is_allowlisted(&words, &context.branch),
    ) {
        return decision(
            false,
            "network-or-outside-worktree-permission-deny",
            operation,
        );
    }
    let lower = command.to_ascii_lowercase();
    if [
        "keychain",
        "security",
        ".ssh",
        ".aws",
        ".env",
        "credential",
        "authorization",
        "access_token",
        "api_key",
        "password",
        "gh_token",
        ".codex",
        ".config/gh",
        ".netrc",
        ".npmrc",
        "id_rsa",
        "id_ed25519",
        "secret",
        "token",
        "private_key",
        "private-key",
        "auth.json",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return decision(false, "credential-or-network-access-deny", operation);
    }
    if words.is_empty() {
        return decision(false, "shell-composition-deny", operation);
    }
    let executable = words
        .first()
        .and_then(|word| std::path::Path::new(word).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if words.first().is_some_and(|word| word.contains('/')) {
        return decision(false, "explicit-executable-path-deny", operation);
    }
    if matches!(
        executable,
        "sh" | "bash" | "zsh" | "fish" | "env" | "sudo" | "doas" | "su"
    ) {
        return decision(false, "shell-or-privilege-escalation-deny", operation);
    }
    let args = words[1..].to_vec();

    if executable == "git" && args.first().is_some_and(|arg| arg == "push") {
        let allowed = git_push_targets_own_branch(&args[1..], &context.branch);
        return decision(
            allowed,
            if allowed {
                "own-branch-git-push-allow"
            } else {
                "git-push-outside-own-branch-deny"
            },
            operation,
        );
    }
    if executable == "gh" {
        let allowed = gh_create_for_own_branch(&args, &context.branch) || gh_read_command(&args);
        return decision(
            allowed,
            if allowed {
                "scoped-gh-command-allow"
            } else {
                "gh-command-deny"
            },
            operation,
        );
    }
    if executable == "mktemp" {
        let in_tmp = mktemp_is_tmp_scoped(&args);
        return decision(
            in_tmp,
            if in_tmp {
                "tmp-scratch-allow"
            } else {
                "mktemp-outside-tmp-deny"
            },
            operation,
        );
    }
    if has_outside_path(&words) {
        return decision(false, "path-outside-worktree-deny", operation);
    }

    let read_only = match executable {
        "pwd" => args.is_empty(),
        "git" => match args.first().map(String::as_str) {
            Some("status" | "diff" | "log" | "show" | "rev-parse" | "ls-files") => true,
            Some("branch") => {
                args.len() == 1
                    || args[1..].iter().all(|arg| {
                        matches!(
                            arg.as_str(),
                            "--list" | "--all" | "-a" | "-r" | "--remotes" | "--show-current"
                        )
                    })
            }
            Some("remote") => {
                matches!(args.as_slice(), [remote] if remote == "remote")
                    || matches!(args.as_slice(), [remote, verbose] if remote == "remote" && matches!(verbose.as_str(), "-v" | "--verbose"))
            }
            _ => false,
        },
        _ => false,
    };
    if read_only {
        return decision(true, "worktree-read-only-command-allow", operation);
    }

    let build_or_test = match executable {
        "cargo" => matches!(
            args.first().map(String::as_str),
            Some("build" | "test" | "check" | "fmt" | "clippy")
        ),
        "go" => matches!(
            args.first().map(String::as_str),
            Some("build" | "test" | "vet" | "fmt")
        ),
        "npm" | "pnpm" | "yarn" => package_manager_command_is_safe(executable, &args),
        "bazel" => matches!(args.first().map(String::as_str), Some("build" | "test")),
        "pytest" => args.is_empty(),
        _ => false,
    };
    if build_or_test {
        decision(true, "worktree-build-or-test-allow", operation)
    } else {
        let rule = if executable == "git"
            && args
                .first()
                .is_some_and(|arg| matches!(arg.as_str(), "branch" | "worktree"))
        {
            "destructive-git-operation-deny"
        } else if executable == "gh"
            && args
                .get(1)
                .is_some_and(|arg| matches!(arg.as_str(), "merge" | "close" | "delete"))
        {
            "gh-destructive-operation-deny"
        } else {
            "default-deny"
        };
        decision(false, rule, operation)
    }
}

fn approval_response(
    method: &str,
    request_id: &Value,
    params: &Value,
    context: Option<&ApprovalContext>,
) -> Result<Value, &'static str> {
    if method == "item/commandExecution/requestApproval" || method == "execCommandApproval" {
        let decision = command_approval(params, context);
        let label = if decision.allow { "allow" } else { "deny" };
        let agent = context
            .map(|ctx| ctx.agent_id.as_str())
            .unwrap_or("unknown");
        log::warn!(
            "{}",
            approval_audit_line(
                params.get("approvalId").unwrap_or(request_id),
                agent,
                &decision.operation,
                label,
                decision.rule
            )
        );
        return Ok(json!({"decision": match (method, decision.allow) {
            ("execCommandApproval", true) => "approved",
            ("execCommandApproval", false) => "denied",
            (_, true) => "accept",
            (_, false) => "decline",
        }}));
    }
    if method == "item/fileChange/requestApproval" {
        let root = params.get("grantRoot").and_then(Value::as_str);
        let allowed = context
            .zip(root)
            .is_some_and(|(ctx, root)| path_is_within(std::path::Path::new(root), &ctx.worktree));
        let rule = if allowed {
            "worktree-file-change-allow"
        } else {
            "file-change-outside-worktree-or-unknown-deny"
        };
        let agent = context
            .map(|ctx| ctx.agent_id.as_str())
            .unwrap_or("unknown");
        log::warn!(
            "{}",
            approval_audit_line(
                params.get("approvalId").unwrap_or(request_id),
                agent,
                &format!("file-change grantRoot={root:?}"),
                if allowed { "allow" } else { "deny" },
                rule
            )
        );
        return Ok(json!({"decision": if allowed { "accept" } else { "decline" }}));
    }
    if method.ends_with("Approval") || method.contains("requestApproval") {
        let agent = context
            .map(|ctx| ctx.agent_id.as_str())
            .unwrap_or("unknown");
        log::warn!(
            "{}",
            approval_audit_line(
                params.get("approvalId").unwrap_or(request_id),
                agent,
                method,
                "deny",
                "unsupported-approval-method-deny"
            )
        );
        return Err("approval denied by relay policy");
    }
    Err("unsupported app-server request")
}

fn read_messages(stdout: impl std::io::Read, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if let Some(method) = value.get("method").and_then(Value::as_str)
                && let Some(id) = value.get("id")
            {
                let thread_id = value.pointer("/params/threadId").and_then(Value::as_str);
                let context = thread_id.and_then(|thread_id| {
                    shared
                        .approval_contexts
                        .lock()
                        .ok()
                        .and_then(|map| map.get(thread_id).cloned())
                });
                let result = approval_response(
                    method,
                    id,
                    value.get("params").unwrap_or(&Value::Null),
                    context.as_ref(),
                );
                let response = match result {
                    Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                    Err(message) => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}})
                    }
                };
                if let Ok(mut writer) = shared.writer.lock() {
                    let _ = serde_json::to_writer(&mut *writer, &response);
                    let _ = writer.write_all(b"\n");
                    let _ = writer.flush();
                }
            } else if let Some(id) = value.get("id").and_then(Value::as_u64) {
                if let Ok(mut pending) = shared.pending.lock()
                    && let Some(tx) = pending.remove(&id)
                {
                    let result = if let Some(error) = value.get("error") {
                        Err(error.to_string())
                    } else {
                        Ok(value.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(result);
                }
            } else if let Some(thread_id) =
                value.pointer("/params/threadId").and_then(Value::as_str)
            {
                let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                let mut terminal_turn = method != "turn/completed";
                if method == "turn/started" {
                    if let Some(turn_id) = value.pointer("/params/turn/id").and_then(Value::as_str)
                        && let Ok(mut turns) = shared.turns.lock()
                    {
                        turns.insert(thread_id.to_owned(), turn_id.to_owned());
                    }
                } else if method == "turn/completed" {
                    if let Ok(mut turns) = shared.turns.lock() {
                        turns.remove(thread_id);
                    }
                    let next = shared.queued.lock().ok().and_then(|mut queued| {
                        queued.get_mut(thread_id).and_then(VecDeque::pop_front)
                    });
                    terminal_turn = next.is_none();
                    if let Some(text) = next {
                        let shared = Arc::clone(&shared);
                        let thread_id = thread_id.to_owned();
                        thread::spawn(move || {
                            let server = CodexServer { shared };
                            if let Ok(turn) = server.rpc(
                                "turn/start",
                                json!({"threadId":thread_id,"input":[{"type":"text","text":text}]}),
                            ) && let Some(turn_id) =
                                turn.pointer("/turn/id").and_then(Value::as_str)
                                && let Ok(mut turns) = server.shared.turns.lock()
                            {
                                turns.insert(thread_id, turn_id.to_owned());
                            }
                        });
                    }
                }
                if let Ok(mut map) = shared.subscribers.lock()
                    && let Some(list) = map.get_mut(thread_id)
                {
                    list.retain(|tx| {
                        tx.send(HarnessEvent::Notification {
                            value: value.clone(),
                            terminal_turn,
                        })
                        .is_ok()
                    });
                }
            }
        }
        line.clear();
    }
    if let Ok(mut pending) = shared.pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("app-server exited".to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalContext, CodexServer, HarnessEvent, Shared, approval_audit_line, approval_response,
        command_approval,
    };
    use serde_json::Value;
    use std::collections::{HashMap, VecDeque};
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn interrupting_idle_codex_session_succeeds_without_rpc() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let writer = child.stdin.take().unwrap();
        let server = Arc::new(CodexServer {
            shared: Arc::new(Shared {
                writer: Mutex::new(writer),
                child: Mutex::new(child),
                pending: Mutex::new(HashMap::new()),
                subscribers: Mutex::new(HashMap::new()),
                turns: Mutex::new(HashMap::new()),
                queued: Mutex::new(HashMap::<String, VecDeque<String>>::new()),
                approval_contexts: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }),
        });

        server
            .session("completed-thread".to_owned())
            .interrupt()
            .unwrap();
        server.stop().unwrap();
    }

    #[test]
    fn codex_completion_during_turn_start_is_buffered_for_the_subscriber() {
        let script = r#"
            IFS= read -r _thread_start
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"thread":{"id":"thread-fast"}}}'
            IFS= read -r _turn_start
            printf '%s\n' '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-fast","turn":{"status":"completed"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"turn":{"id":"turn-fast"}}}'
        "#;
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let writer = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::<String, VecDeque<String>>::new()),
            approval_contexts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || super::read_messages(stdout, reader_shared));
        let server = CodexServer { shared };

        let (thread_id, events) = server
            .start_thread(
                "agent-fast",
                "/tmp",
                "codex/agent-fast",
                "finish immediately",
            )
            .unwrap();
        assert_eq!(thread_id, "thread-fast");
        let event = events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let HarnessEvent::Notification {
            value,
            terminal_turn,
        } = event;
        assert!(terminal_turn);
        assert_eq!(
            value.get("method").and_then(Value::as_str),
            Some("turn/completed")
        );
        server.stop().unwrap();
    }

    #[test]
    fn codex_resume_reconciles_a_persisted_terminal_turn() {
        let script = r#"
            IFS= read -r _resume
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"thread":{"id":"thread-finished","status":{"type":"idle"},"turns":[{"id":"turn-finished","status":"completed"}]}}}'
            IFS= read -r _stop
        "#;
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let writer = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::<String, VecDeque<String>>::new()),
            approval_contexts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || super::read_messages(stdout, reader_shared));
        let server = CodexServer { shared };

        let events = server.subscribe_thread("thread-finished").unwrap();
        server.resume_thread("thread-finished").unwrap();
        let HarnessEvent::Notification {
            value,
            terminal_turn,
        } = events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(terminal_turn);
        assert_eq!(
            value.get("method").and_then(Value::as_str),
            Some("turn/completed")
        );
        assert_eq!(
            value.pointer("/params/turn/status").and_then(Value::as_str),
            Some("completed")
        );
        server.stop().unwrap();
    }

    fn test_context() -> ApprovalContext {
        ApprovalContext {
            agent_id: "agent-42".to_owned(),
            worktree: std::path::PathBuf::from("/workspace/repo"),
            branch: "codex/relay-approval-policy".to_owned(),
        }
    }

    #[test]
    fn unrecognized_approval_operation_is_denied_by_default() {
        let decision = command_approval(
            &serde_json::json!({"command":"python tool.py", "cwd":"/workspace/repo"}),
            Some(&test_context()),
        );
        assert!(!decision.allow);
        assert_eq!(decision.rule, "default-deny");
        assert_eq!(
            approval_response(
                "item/commandExecution/requestApproval",
                &serde_json::json!(7),
                &serde_json::json!({"command":"python tool.py", "cwd":"/workspace/repo"}),
                Some(&test_context())
            )
            .unwrap()["decision"],
            "decline"
        );
    }

    #[test]
    fn safe_build_and_test_operation_is_allowed_with_worktree_scope() {
        let decision = command_approval(
            &serde_json::json!({"command":"cargo test", "cwd":"/workspace/repo"}),
            Some(&test_context()),
        );
        assert!(decision.allow);
        assert_eq!(decision.rule, "worktree-build-or-test-allow");
        assert_eq!(
            approval_response(
                "item/commandExecution/requestApproval",
                &serde_json::json!(8),
                &serde_json::json!({"command":"cargo test", "cwd":"/workspace/repo"}),
                Some(&test_context())
            )
            .unwrap()["decision"],
            "accept"
        );
    }

    #[test]
    fn keychain_and_out_of_worktree_operations_are_denied() {
        for (command, cwd, extra) in [
            (
                "security find-generic-password",
                "/workspace/repo",
                serde_json::json!({}),
            ),
            ("cargo test", "/tmp", serde_json::json!({})),
            (
                "cargo test",
                "/workspace/repo",
                serde_json::json!({"additionalPermissions":{"fileSystem":{"write":["/tmp/out"]}}}),
            ),
        ] {
            let mut params = serde_json::json!({"command":command, "cwd":cwd});
            if let Some(obj) = extra.as_object() {
                for (key, value) in obj {
                    params[key] = value.clone();
                }
            }
            assert!(
                !command_approval(&params, Some(&test_context())).allow,
                "{params}"
            );
        }
    }

    #[test]
    fn own_branch_push_pr_create_and_tmp_mktemp_are_allowlisted() {
        let context = test_context();
        for (command, expected_rule) in [
            (
                serde_json::json!(["git", "push", "origin", "codex/relay-approval-policy"]),
                "own-branch-git-push-allow",
            ),
            (
                serde_json::json!([
                    "git",
                    "push",
                    "--force-with-lease",
                    "origin",
                    "codex/relay-approval-policy"
                ]),
                "own-branch-git-push-allow",
            ),
            (
                serde_json::json!([
                    "gh",
                    "pr",
                    "create",
                    "--head",
                    "codex/relay-approval-policy",
                    "--title",
                    "relay policy"
                ]),
                "scoped-gh-command-allow",
            ),
            (
                serde_json::json!(["gh", "pr", "view", "17"]),
                "scoped-gh-command-allow",
            ),
            (
                serde_json::json!(["gh", "run", "view", "23"]),
                "scoped-gh-command-allow",
            ),
            (
                serde_json::json!(["gh", "api", "repos/example/project/pulls/17"]),
                "scoped-gh-command-allow",
            ),
            (
                serde_json::json!(["mktemp", "-d", "/tmp/zigzag.XXXXXX"]),
                "tmp-scratch-allow",
            ),
        ] {
            let result = command_approval(
                &serde_json::json!({"command":command, "cwd":"/workspace/repo"}),
                Some(&context),
            );
            assert!(result.allow, "{}: {}", result.rule, result.operation);
            assert_eq!(result.rule, expected_rule);
        }
        let network_push = command_approval(
            &serde_json::json!({
                "command":["git", "push", "origin", "codex/relay-approval-policy"],
                "cwd":"/workspace/repo",
                "additionalPermissions":{"network":{"enabled":true}}
            }),
            Some(&context),
        );
        assert!(network_push.allow, "{}", network_push.rule);
        let network_pr = command_approval(
            &serde_json::json!({
                "command":["gh", "pr", "create", "--title", "relay policy"],
                "cwd":"/workspace/repo",
                "networkApprovalContext":{"host":"github.com"}
            }),
            Some(&context),
        );
        assert!(network_pr.allow, "{}", network_pr.rule);
        let unsafe_network = command_approval(
            &serde_json::json!({
                "command":["cargo", "test"],
                "cwd":"/workspace/repo",
                "additionalPermissions":{"network":{"enabled":true}}
            }),
            Some(&context),
        );
        assert!(!unsafe_network.allow);
    }

    #[test]
    fn main_push_credentials_destructive_gh_and_worktree_removal_are_denied() {
        let context = test_context();
        for command in [
            serde_json::json!(["git", "push", "origin", "main"]),
            serde_json::json!(["git", "push", "--force-with-lease", "origin", "main"]),
            serde_json::json!(["git", "push", "origin", "codex/other-branch"]),
            serde_json::json!([
                "git",
                "push",
                "--delete",
                "origin",
                "codex/relay-approval-policy"
            ]),
            serde_json::json!(["git", "push", "origin", ":codex/relay-approval-policy"]),
            serde_json::json!(["./git", "push", "origin", "codex/relay-approval-policy"]),
            serde_json::json!(["cat", "/Users/worker/.env"]),
            serde_json::json!(["cat", "private-key"]),
            serde_json::json!(["gh", "pr", "merge", "17"]),
            serde_json::json!(["gh", "pr", "close", "17"]),
            serde_json::json!(["gh", "pr", "delete", "17"]),
            serde_json::json!(["gh", "pr", "create", "--head", "codex/other-branch"]),
            serde_json::json!(["npm", "run", "deploy"]),
            serde_json::json!(["gh", "api", "repos/example/project/actions/se%63rets"]),
            serde_json::json!(["git", "branch", "-D", "old-branch"]),
            serde_json::json!(["git", "worktree", "remove", "/tmp/other"]),
            serde_json::json!(["mktemp", "-p", "/tmp", "../../outside.XXXXXX"]),
        ] {
            let result = command_approval(
                &serde_json::json!({"command":command, "cwd":"/workspace/repo"}),
                Some(&context),
            );
            assert!(
                !result.allow,
                "{} was allowed by {}",
                result.operation, result.rule
            );
        }
    }

    #[test]
    fn audit_command_redacts_inline_credentials() {
        let result = command_approval(
            &serde_json::json!({
                "command":["env", "GH_TOKEN=ghp_example-secret-value"],
                "cwd":"/workspace/repo"
            }),
            Some(&test_context()),
        );
        assert!(!result.allow);
        assert!(result.operation.contains("GH_TOKEN=[REDACTED]"));
        assert!(!result.operation.contains("ghp_example-secret-value"));
    }

    #[test]
    fn denied_approval_request_gets_a_fast_jsonrpc_answer() {
        let answer_file = std::env::temp_dir().join(format!(
            "zigzag-approval-answer-{}-{}.json",
            std::process::id(),
            super::AtomicU64::new(1).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let script = format!(
            "printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"item/commandExecution/requestApproval\",\"params\":{{\"threadId\":\"thread-fast-deny\",\"command\":[\"python\",\"tool.py\"],\"cwd\":\"/workspace/repo\"}}}}'; IFS= read -r answer; printf '%s\\n' \"$answer\" > '{}'",
            answer_file.display()
        );
        let mut child = Command::new("/bin/sh")
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let writer = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::<String, VecDeque<String>>::new()),
            approval_contexts: Mutex::new(HashMap::from([(
                "thread-fast-deny".to_owned(),
                test_context(),
            )])),
            next_id: AtomicU64::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || super::read_messages(stdout, reader_shared));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let answer = loop {
            if let Ok(answer) = std::fs::read_to_string(&answer_file) {
                break answer;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "approval request stalled without a reply"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        };
        let response: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(response["id"], 77);
        assert_eq!(
            response.pointer("/result/decision").and_then(Value::as_str),
            Some("decline")
        );
        let _ = std::fs::remove_file(answer_file);
        let mut child = shared.child.lock().unwrap();
        let _ = child.wait();
    }

    #[test]
    fn every_approval_audit_line_has_id_agent_operation_decision_and_rule() {
        let response = approval_response(
            "item/commandExecution/requestApproval",
            &serde_json::json!(91),
            &serde_json::json!({"command":"cargo test", "cwd":"/workspace/repo"}),
            Some(&test_context()),
        )
        .unwrap();
        let decision = command_approval(
            &serde_json::json!({"command":"cargo test", "cwd":"/workspace/repo"}),
            Some(&test_context()),
        );
        let line = approval_audit_line(
            &serde_json::json!(91),
            "agent-42",
            &decision.operation,
            response["decision"].as_str().unwrap(),
            decision.rule,
        );
        for field in [
            "id=91",
            "agent=agent-42",
            "cargo test",
            "decision=accept",
            "policy_rule=worktree-build-or-test-allow",
        ] {
            assert!(line.contains(field), "missing {field:?} in {line:?}");
        }
    }
}
