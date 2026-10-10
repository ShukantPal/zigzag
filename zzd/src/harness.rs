//! Native, session-oriented harness controls.
//!
//! App-server threads are execution sessions, not security principals: callers
//! must authorize with the Zigzag agent ID before resolving a thread ID.
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
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
