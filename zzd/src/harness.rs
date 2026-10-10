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
    Notification(Value),
}

#[allow(dead_code)]
pub(crate) trait HarnessSession: Send + Sync {
    fn send(&self, text: &str, delivery: Delivery) -> Result<(), String>;
    fn answer(&self, request_id: &str, answer: Value) -> Result<(), String>;
    fn interrupt(&self) -> Result<(), String>;
    fn events(&self) -> mpsc::Receiver<HarnessEvent>;
}

struct Shared {
    writer: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<HarnessEvent>>>>,
    turns: Mutex<HashMap<String, String>>,
    queued: Mutex<HashMap<String, VecDeque<String>>>,
    next_id: AtomicU64,
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
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<HarnessEvent>>>>,
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
            subscribers: Mutex::new(HashMap::new()),
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
        let should_listen = self
            .shared
            .sessions
            .lock()
            .map_err(|_| "OpenCode session map poisoned")?
            .insert(session_id.to_owned());
        self.shared
            .subscribers
            .lock()
            .map_err(|_| "OpenCode subscriber map poisoned")?
            .entry(session_id.to_owned())
            .or_default();
        if should_listen {
            let shared = Arc::clone(&self.shared);
            let session_id = session_id.to_owned();
            thread::spawn(move || read_opencode_events(shared, session_id));
        }
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
    ) -> Result<String, String> {
        let response = self.rpc("thread/start", json!({"cwd":cwd}))?;
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .or_else(|| response.pointer("/threadId").and_then(Value::as_str))
            .ok_or("app-server thread/start returned no thread ID")?
            .to_owned();
        self.shared
            .subscribers
            .lock()
            .map_err(|_| "session event map poisoned")?
            .entry(thread_id.clone())
            .or_default();
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
        Ok(thread_id)
    }

    pub(crate) fn resume_thread(&self, thread_id: &str) -> Result<(), String> {
        self.rpc("thread/resume", json!({"threadId":thread_id}))
            .map(|_| ())
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
    pub(crate) fn has_active_turn(&self, thread_id: &str) -> bool {
        self.shared
            .turns
            .lock()
            .map(|turns| turns.contains_key(thread_id))
            .unwrap_or(false)
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
    fn answer(&self, request_id: &str, answer: Value) -> Result<(), String> {
        self.server
            .rpc(
                "server/answer",
                json!({"requestId":request_id,"answer":answer}),
            )
            .map(|_| ())
    }
    fn interrupt(&self) -> Result<(), String> {
        let turn_id = self
            .server
            .shared
            .turns
            .lock()
            .map_err(|_| "turn map poisoned")?
            .get(&self.thread_id)
            .cloned()
            .ok_or("agent has no active Codex turn")?;
        self.server
            .rpc(
                "turn/interrupt",
                json!({"threadId":self.thread_id,"turnId":turn_id}),
            )
            .map(|_| ())
    }
    fn events(&self) -> mpsc::Receiver<HarnessEvent> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.server.shared.subscribers.lock() {
            subscribers
                .entry(self.thread_id.clone())
                .or_default()
                .push(tx);
        }
        rx
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

    fn answer(&self, _request_id: &str, _answer: Value) -> Result<(), String> {
        Err("OpenCode question replies are not implemented".to_owned())
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

    fn events(&self) -> mpsc::Receiver<HarnessEvent> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.server.shared.subscribers.lock() {
            subscribers
                .entry(self.session_id.clone())
                .or_default()
                .push(tx);
        }
        rx
    }
}

fn read_opencode_events(shared: Arc<OpenCodeShared>, session_id: String) {
    let path = format!("/api/session/{session_id}/event");
    loop {
        let response = ureq::get(&format!("{}{path}", shared.base_url)).call();
        if let Ok(response) = response {
            let mut reader = BufReader::new(response.into_reader());
            let mut line = String::new();
            let mut data = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Some(value) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value.trim());
                } else if line.trim().is_empty() && !data.is_empty() {
                    if let Ok(value) = serde_json::from_str::<Value>(&data)
                        && let Ok(mut subscribers) = shared.subscribers.lock()
                        && let Some(list) = subscribers.get_mut(&session_id)
                    {
                        list.retain(|tx| {
                            tx.send(HarnessEvent::Notification(value.clone())).is_ok()
                        });
                    }
                    data.clear();
                }
                line.clear();
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn read_messages(stdout: impl std::io::Read, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if let Some(id) = value.get("id").and_then(Value::as_u64) {
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
                    list.retain(|tx| tx.send(HarnessEvent::Notification(value.clone())).is_ok());
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
