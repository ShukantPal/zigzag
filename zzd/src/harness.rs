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
    /// Explicitly restore a durable conversation after its agent reached a
    /// terminal state. Implementations must never replace it with a new thread.
    fn resume_thread(&self) -> Result<Option<mpsc::Receiver<HarnessEvent>>, String> {
        Err("harness does not support transcript resume".to_owned())
    }
    fn continue_turn(&self, text: &str) -> Result<Value, String> {
        let _ = text;
        Err("harness does not support synchronous continuation".to_owned())
    }
    fn interrupt(&self) -> Result<(), String>;
}

struct QueuedTurn {
    text: String,
    completion: Option<mpsc::Sender<Result<Value, String>>>,
}

struct Shared {
    writer: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<HarnessEvent>>>>,
    turns: Mutex<HashMap<String, String>>,
    queued: Mutex<HashMap<String, VecDeque<QueuedTurn>>>,
    completions: Mutex<HashMap<String, mpsc::Sender<Result<Value, String>>>>,
    early_completions: Mutex<std::collections::HashSet<String>>,
    approval_contexts: Mutex<HashMap<String, ApprovalContext>>,
    next_id: AtomicU64,
}

#[derive(Clone, Debug)]
struct ApprovalContext {
    agent_id: String,
    worktree: std::path::PathBuf,
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
    pub(crate) fn active_turn_id(&self, thread_id: &str) -> Option<String> {
        self.shared.turns.lock().ok()?.get(thread_id).cloned()
    }

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
            completions: Mutex::new(HashMap::new()),
            early_completions: Mutex::new(std::collections::HashSet::new()),
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
        prompt: &str,
        model: &str,
    ) -> Result<(String, mpsc::Receiver<HarnessEvent>), String> {
        let response = self.rpc("thread/start", json!({"cwd":cwd,"model":model}))?;
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
        let _ = agent_id;
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

    /// Permanently delete one persisted thread through app-server. The
    /// protocol closes the loaded thread before removing its rollout and
    /// associated metadata. Missing rollout/thread errors are idempotent.
    pub(crate) fn delete_thread(&self, thread_id: &str) -> Result<(), String> {
        match self.rpc("thread/delete", json!({"threadId":thread_id})) {
            Ok(_) => {}
            Err(error) if missing_thread_error(&error) => {}
            Err(error) => return Err(error),
        }
        if let Ok(mut map) = self.shared.subscribers.lock() {
            map.remove(thread_id);
        }
        if let Ok(mut map) = self.shared.turns.lock() {
            map.remove(thread_id);
        }
        if let Ok(mut map) = self.shared.queued.lock() {
            map.remove(thread_id);
        }
        if let Ok(mut map) = self.shared.completions.lock() {
            map.remove(thread_id);
        }
        if let Ok(mut map) = self.shared.approval_contexts.lock() {
            map.remove(thread_id);
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

fn missing_thread_error(error: &str) -> bool {
    let message = serde_json::from_str::<Value>(error)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| error.to_owned())
        .to_ascii_lowercase();
    message.contains("thread not found")
        || message.contains("no rollout found for thread")
        || message.contains("no codex thread found")
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
    fn resume_thread(&self) -> Result<Option<mpsc::Receiver<HarnessEvent>>, String> {
        let events = self.server.subscribe_thread(&self.thread_id)?;
        match self.server.resume_thread(&self.thread_id) {
            Ok(()) => Ok(Some(events)),
            Err(error) => {
                if let Ok(mut subscribers) = self.server.shared.subscribers.lock() {
                    subscribers.remove(&self.thread_id);
                }
                Err(error)
            }
        }
    }

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
                        .push_back(QueuedTurn {
                            text: text.to_owned(),
                            completion: None,
                        });
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
    fn continue_turn(&self, text: &str) -> Result<Value, String> {
        if !self.server.has_thread(&self.thread_id) {
            self.server
                .resume_thread(&self.thread_id)
                .map_err(|_| "agent_thread_unavailable".to_owned())?;
            self.server
                .shared
                .subscribers
                .lock()
                .map_err(|_| "session event map poisoned")?
                .entry(self.thread_id.clone())
                .or_default();
        }

        let (tx, rx) = mpsc::channel();
        let mut completion = Some(tx);
        // Serialize the active check with completion handling so a turn that
        // finishes between the check and enqueue cannot strand this request.
        let mut queued = self
            .server
            .shared
            .queued
            .lock()
            .map_err(|_| "turn queue poisoned")?;
        let active = self
            .server
            .shared
            .turns
            .lock()
            .map_err(|_| "turn map poisoned")?
            .contains_key(&self.thread_id);
        if active {
            queued
                .entry(self.thread_id.clone())
                .or_default()
                .push_back(QueuedTurn {
                    text: text.to_owned(),
                    completion: completion.take(),
                });
        } else {
            self.server
                .shared
                .turns
                .lock()
                .map_err(|_| "turn map poisoned")?
                .insert(self.thread_id.clone(), "__starting__".to_owned());
            self.server
                .shared
                .early_completions
                .lock()
                .map_err(|_| "early completion map poisoned")?
                .remove(&self.thread_id);
        }
        drop(queued);
        if let Some(tx) = completion {
            self.server
                .shared
                .completions
                .lock()
                .map_err(|_| "turn completion map poisoned")?
                .insert(self.thread_id.clone(), tx.clone());
            let turn = self.server.rpc(
                "turn/start",
                json!({"threadId":self.thread_id,"input":[{"type":"text","text":text}]}),
            );
            let turn = match turn {
                Ok(turn) => turn,
                Err(_) => {
                    fail_queued_turns(&self.server.shared, &self.thread_id, None);
                    return Err("agent_thread_unavailable".to_owned());
                }
            };
            let Some(turn_id) = turn.pointer("/turn/id").and_then(Value::as_str) else {
                fail_queued_turns(&self.server.shared, &self.thread_id, None);
                return Err("agent_thread_unavailable".to_owned());
            };
            reconcile_started_turn(&self.server.shared, &self.thread_id, turn_id);
        }
        rx.recv()
            .map_err(|_| "app-server ended before the follow-up turn completed".to_owned())?
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

fn command_text(params: &Value) -> String {
    match params.get("command") {
        Some(Value::String(command)) => command.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn requested_access_is_worktree_scoped(params: &Value, root: &std::path::Path) -> bool {
    if params.get("networkApprovalContext").is_some() {
        return false;
    }
    let Some(filesystem) = params.pointer("/additionalPermissions/fileSystem") else {
        return params.pointer("/additionalPermissions/network/enabled")
            != Some(&Value::Bool(true));
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
    params.pointer("/additionalPermissions/network/enabled") != Some(&Value::Bool(true))
}

fn command_approval(params: &Value, context: Option<&ApprovalContext>) -> ApprovalDecision {
    let command = command_text(params);
    let operation = command.chars().take(512).collect::<String>();
    let Some(context) = context else {
        return decision(false, "unknown-agent-deny", operation);
    };
    let Some(cwd) = params.get("cwd").and_then(Value::as_str) else {
        return decision(false, "missing-cwd-deny", operation);
    };
    if !path_is_within(std::path::Path::new(cwd), &context.worktree) {
        return decision(false, "cwd-outside-worktree-deny", operation);
    }
    if !requested_access_is_worktree_scoped(params, &context.worktree) {
        return decision(
            false,
            "network-or-outside-worktree-permission-deny",
            operation,
        );
    }

    let lower = command.to_ascii_lowercase();
    if [
        "keychain",
        "security ",
        ".ssh",
        ".aws",
        ".env",
        "credential",
        "access_token",
        "api_key",
        "password",
        "curl ",
        "wget ",
        " nc ",
        "ssh ",
        "scp ",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return decision(false, "credential-or-network-access-deny", operation);
    }
    if command.is_empty()
        || command
            .chars()
            .any(|ch| matches!(ch, ';' | '|' | '&' | '>' | '<' | '$' | '`' | '\n'))
    {
        return decision(false, "shell-composition-deny", operation);
    }

    let words = command.split_whitespace().collect::<Vec<_>>();
    let executable = words
        .first()
        .and_then(|word| std::path::Path::new(word).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if matches!(
        executable,
        "sh" | "bash" | "zsh" | "fish" | "env" | "sudo" | "doas" | "su"
    ) {
        return decision(false, "shell-or-privilege-escalation-deny", operation);
    }
    let args = &words[1..];
    if args.iter().any(|arg| {
        if arg.starts_with("-") && !arg.contains('=') {
            return false;
        }
        let pathish = arg.split_once('=').map(|(_, value)| value).unwrap_or(arg);
        let path = std::path::Path::new(pathish);
        if path.is_absolute() {
            !path_is_within(path, &context.worktree)
        } else {
            path.components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        }
    }) {
        return decision(false, "write-or-read-path-outside-worktree-deny", operation);
    }

    let read_only = match executable {
        "pwd" | "ls" | "cat" | "head" | "tail" | "rg" | "grep" | "git" => {
            executable != "git"
                || matches!(
                    args.first().copied(),
                    Some("status" | "diff" | "log" | "show" | "branch" | "rev-parse")
                )
        }
        "sed" => !args.iter().any(|arg| *arg == "-i" || arg.starts_with("-i")),
        "find" => !args
            .iter()
            .any(|arg| matches!(*arg, "-delete" | "-exec" | "-execdir")),
        _ => false,
    };
    if read_only {
        return decision(true, "worktree-read-only-command-allow", operation);
    }

    let git_write = executable == "git"
        && matches!(
            args.first().copied(),
            Some(
                "add"
                    | "commit"
                    | "checkout"
                    | "switch"
                    | "restore"
                    | "reset"
                    | "stash"
                    | "merge"
                    | "rebase"
                    | "tag"
            )
        );
    let build_or_test = match executable {
        "cargo" => matches!(
            args.first().copied(),
            Some("build" | "test" | "check" | "fmt" | "clippy")
        ),
        "go" => matches!(
            args.first().copied(),
            Some("build" | "test" | "vet" | "fmt")
        ),
        "npm" | "pnpm" | "yarn" => matches!(
            args.first().copied(),
            Some("build" | "test" | "lint" | "check" | "run")
        ),
        "pytest" | "bazel" | "make" => {
            matches!(executable, "pytest" | "make")
                || matches!(args.first().copied(), Some("build" | "test"))
        }
        _ => false,
    };
    let worktree_file_write = match executable {
        "sed" => args.iter().any(|arg| *arg == "-i" || arg.starts_with("-i")),
        "install" => true,
        "mktemp" => {
            args.iter().any(|arg| arg.starts_with("--tmpdir="))
                || args.windows(2).any(|pair| pair[0] == "-p")
        }
        _ => false,
    };
    if git_write || build_or_test || worktree_file_write {
        decision(
            true,
            if git_write {
                "worktree-git-operation-allow"
            } else if build_or_test {
                "worktree-build-or-test-allow"
            } else {
                "worktree-scoped-write-allow"
            },
            operation,
        )
    } else {
        decision(false, "default-deny", operation)
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

fn fail_queued_turns(
    shared: &Shared,
    thread_id: &str,
    current: Option<mpsc::Sender<Result<Value, String>>>,
) {
    if let Some(current) = current {
        let _ = current.send(Err("agent_thread_unavailable".to_owned()));
    }
    if let Some(completion) = shared
        .completions
        .lock()
        .ok()
        .and_then(|mut completions| completions.remove(thread_id))
    {
        let _ = completion.send(Err("agent_thread_unavailable".to_owned()));
    }
    let mut queued = shared.queued.lock().ok();
    if let Ok(mut turns) = shared.turns.lock()
        && turns.get(thread_id).is_some_and(|id| id == "__starting__")
    {
        turns.remove(thread_id);
    }
    if let Some(pending) = queued.as_mut().and_then(|queued| queued.remove(thread_id)) {
        for pending in pending {
            if let Some(completion) = pending.completion {
                let _ = completion.send(Err("agent_thread_unavailable".to_owned()));
            }
        }
    }
}

fn read_messages(stdout: impl std::io::Read, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        let bytes = match reader.read_line(&mut line) {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!("Codex app-server message stream read failed: {error}");
                break;
            }
        };
        if bytes == 0 {
            break;
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                log::warn!("dropping malformed Codex app-server message: {error}");
                line.clear();
                continue;
            }
        };
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
        } else if let Some(thread_id) = value.pointer("/params/threadId").and_then(Value::as_str) {
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            let mut terminal_turn = method != "turn/completed";
            if method == "turn/started" {
                if let Some(turn_id) = value.pointer("/params/turn/id").and_then(Value::as_str)
                    && let Ok(mut turns) = shared.turns.lock()
                {
                    turns.insert(thread_id.to_owned(), turn_id.to_owned());
                }
            } else if method == "turn/completed" {
                let mut queued_guard = shared.queued.lock().ok();
                if let Ok(mut turns) = shared.turns.lock() {
                    if turns.get(thread_id).map(String::as_str) == Some("__starting__")
                        && let Ok(mut early) = shared.early_completions.lock()
                    {
                        early.insert(thread_id.to_owned());
                    }
                    turns.remove(thread_id);
                }
                if let Some(completion) = shared
                    .completions
                    .lock()
                    .ok()
                    .and_then(|mut completions| completions.remove(thread_id))
                {
                    let result = value.pointer("/params/turn").cloned().unwrap_or_else(|| {
                        value.pointer("/params").cloned().unwrap_or(Value::Null)
                    });
                    let _ = completion.send(Ok(result));
                }
                let next = queued_guard
                    .as_mut()
                    .and_then(|queued| queued.get_mut(thread_id).and_then(VecDeque::pop_front));
                if next.is_some()
                    && let Ok(mut turns) = shared.turns.lock()
                {
                    // Reserve the thread while the queued turn is started
                    // on a worker; the reader thread must remain free to
                    // receive the JSON-RPC response.
                    turns.insert(thread_id.to_owned(), "__starting__".to_owned());
                    if let Ok(mut early) = shared.early_completions.lock() {
                        early.remove(thread_id);
                    }
                }
                drop(queued_guard);
                terminal_turn = next.is_none();
                if let Some(text) = next {
                    let shared = Arc::clone(&shared);
                    let thread_id = thread_id.to_owned();
                    thread::spawn(move || {
                        let server = CodexServer { shared };
                        if let Some(completion) = text.completion.as_ref()
                            && let Ok(mut completions) = server.shared.completions.lock()
                        {
                            completions.insert(thread_id.clone(), completion.clone());
                        }
                        if let Ok(turn) = server.rpc(
                                "turn/start",
                                json!({"threadId":thread_id,"input":[{"type":"text","text":text.text}]}),
                            ) {
                                if let Some(turn_id) = turn.pointer("/turn/id").and_then(Value::as_str) {
                                    reconcile_started_turn(&server.shared, &thread_id, turn_id);
                                } else {
                                    fail_queued_turns(&server.shared, &thread_id, text.completion);
                                }
                            } else {
                                fail_queued_turns(&server.shared, &thread_id, text.completion);
                            }
                    });
                }
            }
            match shared.subscribers.lock() {
                Ok(mut map) => match map.get_mut(thread_id) {
                    Some(list) => list.retain(|tx| {
                        match tx.send(HarnessEvent::Notification {
                            value: value.clone(),
                            terminal_turn,
                        }) {
                            Ok(()) => true,
                            Err(_) => {
                                log::warn!("Codex monitor channel closed for thread {thread_id}");
                                false
                            }
                        }
                    }),
                    None => log::warn!(
                        "dropping Codex notification for thread without a monitor: {thread_id}"
                    ),
                },
                Err(_) => log::error!("Codex subscriber map lock poisoned"),
            }
        } else {
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            log::warn!("dropping Codex notification without params.threadId: {method}");
        }
        line.clear();
    }
    if let Ok(mut pending) = shared.pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("app-server exited".to_owned()));
        }
    }
    if let Ok(mut completions) = shared.completions.lock() {
        for (_, tx) in completions.drain() {
            let _ = tx.send(Err("agent_thread_unavailable".to_owned()));
        }
    }
    if let Ok(mut queued) = shared.queued.lock() {
        for (_, turns) in queued.drain() {
            for turn in turns {
                if let Some(tx) = turn.completion {
                    let _ = tx.send(Err("agent_thread_unavailable".to_owned()));
                }
            }
        }
    }
}

/// Apply the turn/start response without resurrecting a turn whose terminal
/// notification arrived while the request was still in flight.
fn reconcile_started_turn(shared: &Shared, thread_id: &str, turn_id: &str) {
    if let Ok(mut turns) = shared.turns.lock() {
        let completed_early = shared
            .early_completions
            .lock()
            .map(|mut early| early.remove(thread_id))
            .unwrap_or(false);
        if completed_early {
            turns.remove(thread_id);
        } else {
            turns.insert(thread_id.to_owned(), turn_id.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalContext, CodexServer, HarnessEvent, QueuedTurn, Shared, approval_audit_line,
        approval_response, command_approval, missing_thread_error,
    };
    use serde_json::Value;
    use std::collections::{HashMap, VecDeque};
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn mock_server(script: &str, thread_id: &str) -> Arc<CodexServer> {
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
            subscribers: Mutex::new(HashMap::from([(thread_id.to_owned(), Vec::new())])),
            turns: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            completions: Mutex::new(HashMap::new()),
            early_completions: Mutex::new(std::collections::HashSet::new()),
            approval_contexts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || super::read_messages(stdout, reader_shared));
        Arc::new(CodexServer { shared })
    }

    #[test]
    fn deleting_a_thread_uses_app_server_and_keeps_other_threads_attached() {
        let script = r#"
            IFS= read -r request
            case "$request" in
                *'"method":"thread/delete"'*'"threadId":"thread-delete-target"'*)
                    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
                    ;;
                *)
                    printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"unexpected delete request"}}'
                    ;;
            esac
        "#;
        let server = mock_server(script, "thread-delete-target");
        server
            .shared
            .subscribers
            .lock()
            .unwrap()
            .insert("another-agents-thread".to_owned(), Vec::new());

        server.delete_thread("thread-delete-target").unwrap();

        let subscribers = server.shared.subscribers.lock().unwrap();
        assert!(!subscribers.contains_key("thread-delete-target"));
        assert!(subscribers.contains_key("another-agents-thread"));
    }

    #[test]
    fn deleting_a_thread_with_missing_rollout_is_idempotent() {
        let script = r#"
            IFS= read -r _request
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"no rollout found for thread id thread-already-gone"}}'
        "#;
        let server = mock_server(script, "thread-already-gone");
        server.delete_thread("thread-already-gone").unwrap();
        assert!(
            !server
                .shared
                .subscribers
                .lock()
                .unwrap()
                .contains_key("thread-already-gone")
        );
        assert!(missing_thread_error(
            r#"{"code":-32600,"message":"thread not found"}"#
        ));
        assert!(!missing_thread_error(
            r#"{"code":-32600,"message":"already has an active writer"}"#
        ));
    }

    #[test]
    fn continuation_delivers_completion_emitted_before_turn_start_response() {
        let script = r#"
            IFS= read -r request
            printf '%s\n' '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-fast","turn":{"id":"turn-fast","status":"completed","output":"done"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"turn":{"id":"turn-fast"}}}'
        "#;
        let server = mock_server(script, "thread-fast");
        let session = server.session("thread-fast".to_owned());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = done_tx.send(session.continue_turn("follow up"));
        });
        let result = match done_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result.unwrap(),
            Err(error) => {
                server.stop().unwrap();
                panic!("continuation did not receive its early completion: {error}");
            }
        };
        assert_eq!(
            result.pointer("/output").and_then(Value::as_str),
            Some("done")
        );
        assert!(
            !server
                .shared
                .turns
                .lock()
                .unwrap()
                .contains_key("thread-fast")
        );
        server.stop().unwrap();
    }

    #[test]
    fn queued_continuation_reaches_app_server_and_handles_early_completion() {
        let script = r#"
            IFS= read -r trigger
            printf '%s\n' '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-queue","turn":{"id":"turn-active","status":"completed"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
            IFS= read -r queued
            case "$queued" in *'"text":"queued prompt"'*) ;; *) exit 3 ;; esac
            printf '%s\n' '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-queue","turn":{"id":"turn-next","status":"completed","output":"queued done"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"turn":{"id":"turn-next"}}}'
        "#;
        let server = mock_server(script, "thread-queue");
        server
            .shared
            .turns
            .lock()
            .unwrap()
            .insert("thread-queue".to_owned(), "turn-active".to_owned());
        let session = server.session("thread-queue".to_owned());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = done_tx.send(session.continue_turn("queued prompt"));
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            if server
                .shared
                .queued
                .lock()
                .unwrap()
                .get("thread-queue")
                .is_some_and(|queued| !queued.is_empty())
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "continuation was not queued behind the active turn"
            );
            thread::yield_now();
        }
        server.rpc("test/trigger", serde_json::json!({})).unwrap();
        let result = match done_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result.unwrap(),
            Err(error) => {
                server.stop().unwrap();
                panic!("queued continuation did not receive its completion: {error}");
            }
        };
        assert_eq!(
            result.pointer("/output").and_then(Value::as_str),
            Some("queued done")
        );
        server.stop().unwrap();
    }

    #[test]
    fn continuation_fails_when_persisted_thread_cannot_be_resumed() {
        let script = r#"
            IFS= read -r request
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"thread not found"}}'
        "#;
        let server = mock_server(script, "missing-thread");
        server
            .shared
            .subscribers
            .lock()
            .unwrap()
            .remove("missing-thread");
        let error = server
            .session("missing-thread".to_owned())
            .continue_turn("follow up")
            .unwrap_err();
        assert_eq!(error, "agent_thread_unavailable");
        server.stop().unwrap();
    }

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
                queued: Mutex::new(HashMap::<String, VecDeque<QueuedTurn>>::new()),
                completions: Mutex::new(HashMap::new()),
                early_completions: Mutex::new(std::collections::HashSet::new()),
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
            queued: Mutex::new(HashMap::<String, VecDeque<QueuedTurn>>::new()),
            completions: Mutex::new(HashMap::new()),
            early_completions: Mutex::new(std::collections::HashSet::new()),
            approval_contexts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        thread::spawn(move || super::read_messages(stdout, reader_shared));
        let server = CodexServer { shared };

        let (thread_id, events) = server
            .start_thread("agent-fast", "/tmp", "finish immediately", "gpt-6-luna")
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
            queued: Mutex::new(HashMap::<String, VecDeque<QueuedTurn>>::new()),
            completions: Mutex::new(HashMap::new()),
            early_completions: Mutex::new(std::collections::HashSet::new()),
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
