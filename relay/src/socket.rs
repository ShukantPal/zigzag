//! Length-prefixed JSON subscriptions for interactive relay clients.
//!
//! This is deliberately a small TCP protocol rather than WebSocket: every
//! frame is a four-byte big-endian length followed by one UTF-8 JSON object.
//! The first frame is `{"type":"auth","authorization":"Bearer ..."}`.

use crate::auth::authorized;
use crate::routes::agents::agent_logs_json;
use crate::server::{ConnectionLimiter, Server};
use relay_core::{Json, Store};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone)]
struct TopicPublisher {
    state: Arc<Server>,
    sender: SyncSender<Value>,
    alive: Arc<AtomicBool>,
    events_dropped: Arc<AtomicBool>,
    subscriptions: Arc<Mutex<HashSet<String>>>,
}

/// Accept socket clients using the same global connection budget as HTTP.
pub(crate) fn serve(listener: TcpListener, state: Arc<Server>, limiter: Arc<ConnectionLimiter>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match limiter.try_acquire() {
                Some(permit) => {
                    let state = Arc::clone(&state);
                    thread::spawn(move || {
                        let _permit = permit;
                        if let Err(error) = handle(stream, state) {
                            log::debug!("socket connection ended: {error}");
                        }
                    });
                }
                None => {
                    log::warn!("socket connection shed: already at connection limit");
                    let _ = stream.shutdown(Shutdown::Both);
                }
            },
            Err(error) => log::warn!("socket accept error: {error}"),
        }
    }
}

pub(crate) fn handle(mut stream: TcpStream, state: Arc<Server>) -> Result<(), String> {
    stream
        .set_read_timeout(Some(AUTH_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let auth = read_frame(&mut stream)?;
    let authorization = auth
        .get("authorization")
        .and_then(Value::as_str)
        .unwrap_or("");
    if auth.get("type").and_then(Value::as_str) != Some("auth")
        || !authorized(authorization, &state.secret)
    {
        let _ = write_frame(&mut stream, &json!({"type":"error","error":"unauthorized"}));
        return Err("unauthorized socket client".to_owned());
    }
    write_frame(&mut stream, &json!({"type":"authenticated"}))?;
    stream
        .set_read_timeout(None)
        .map_err(|error| error.to_string())?;

    let writer = stream.try_clone().map_err(|error| error.to_string())?;
    let (sender, receiver) = sync_channel(EVENT_QUEUE_CAPACITY);
    let alive = Arc::new(AtomicBool::new(true));
    let events_dropped = Arc::new(AtomicBool::new(false));
    let writer_alive = Arc::clone(&alive);
    let writer_dropped = Arc::clone(&events_dropped);
    let writer_thread =
        thread::spawn(move || write_loop(writer, receiver, writer_alive, writer_dropped));

    let subscriptions = Arc::new(Mutex::new(HashSet::new()));
    let publisher = TopicPublisher {
        state: Arc::clone(&state),
        sender: sender.clone(),
        alive: Arc::clone(&alive),
        events_dropped: Arc::clone(&events_dropped),
        subscriptions: Arc::clone(&subscriptions),
    };
    while alive.load(Ordering::Acquire) {
        let frame = match read_frame(&mut stream) {
            Ok(frame) => frame,
            Err(error) => {
                alive.store(false, Ordering::Release);
                log::debug!("socket reader ended: {error}");
                break;
            }
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("subscribe") => {
                let Some(topic) = frame.get("topic").and_then(Value::as_str) else {
                    let _ = enqueue(&sender, json!({"type":"error","error":"invalid_topic"}));
                    continue;
                };
                if !valid_topic(topic) {
                    let _ = enqueue(
                        &sender,
                        json!({"type":"error","error":"invalid_topic","topic":topic}),
                    );
                    continue;
                }
                // Snapshot the event cursor before this topic becomes visible
                // to publishers.  A client normally supplies its HTTP cursor
                // and epoch; clients without one start at this exact snapshot.
                // Either way, an event cannot land between admission and the
                // cursor from which the publisher reads.
                let event_cursor = if topic == "events" {
                    match event_cursor(&frame, &state) {
                        Ok(cursor) => Some(cursor),
                        Err(()) => {
                            let _ = enqueue(
                                &sender,
                                json!({"type":"error","error":"invalid_cursor","topic":"events"}),
                            );
                            continue;
                        }
                    }
                } else {
                    None
                };
                let log_after = topic.strip_prefix("logs.").map(|id| {
                    state
                        .supervisor
                        .registry
                        .get(id)
                        .map_or(0, |agent| agent.log_next)
                });
                let added = subscriptions
                    .lock()
                    .map_err(|_| "socket subscriptions lock poisoned".to_owned())?
                    .insert(topic.to_owned());
                if added {
                    let _ = enqueue(&sender, json!({"type":"subscribed","topic":topic}));
                    start_topic(topic.to_owned(), publisher.clone(), event_cursor, log_after);
                }
            }
            Some("unsubscribe") => {
                // Publisher threads check this shared set between reads, so a
                // tail stops promptly without affecting other topics.
                if let Some(topic) = frame.get("topic").and_then(Value::as_str) {
                    if let Ok(mut subscriptions) = subscriptions.lock() {
                        subscriptions.remove(topic);
                    }
                    let _ = enqueue(&sender, json!({"type":"unsubscribed","topic":topic}));
                }
            }
            Some("ping") => {
                let _ = enqueue(&sender, json!({"type":"pong"}));
            }
            _ => {
                let _ = enqueue(&sender, json!({"type":"error","error":"invalid_message"}));
            }
        }
    }
    alive.store(false, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = writer_thread.join();
    Ok(())
}

fn event_cursor(frame: &Value, state: &Server) -> Result<(u64, String), ()> {
    let after = frame.get("after");
    let epoch = frame.get("epoch");
    match (after, epoch) {
        (Some(after), Some(epoch)) => Ok((
            after.as_u64().ok_or(())?,
            epoch.as_str().ok_or(())?.to_owned(),
        )),
        (None, None) => state
            .store
            .read(u64::MAX, "", Duration::ZERO)
            .map(|read| (read.next, read.epoch))
            .map_err(|_| ()),
        _ => Err(()),
    }
}

fn valid_topic(topic: &str) -> bool {
    topic == "agents"
        || topic == "events"
        || topic
            .strip_prefix("logs.")
            .is_some_and(|id| !id.is_empty() && id.len() <= 256 && !id.contains(['/', '\\']))
}

fn start_topic(
    topic: String,
    publisher: TopicPublisher,
    event_cursor: Option<(u64, String)>,
    log_after: Option<u64>,
) {
    thread::spawn(move || match topic.as_str() {
        "agents" => publish_agents(
            publisher.state,
            publisher.sender,
            publisher.alive,
            publisher.subscriptions,
            topic,
        ),
        "events" => publish_events(
            publisher.state.store.clone(),
            publisher.sender,
            publisher.alive,
            publisher.events_dropped,
            publisher.subscriptions,
            topic,
            event_cursor.expect("event cursor captured"),
        ),
        _ => publish_logs(
            topic
                .strip_prefix("logs.")
                .expect("validated topic")
                .to_owned(),
            publisher.state,
            publisher.sender,
            publisher.alive,
            publisher.subscriptions,
            topic,
            log_after.expect("log cursor captured"),
        ),
    });
}

fn publish_agents(
    state: Arc<Server>,
    sender: SyncSender<Value>,
    alive: Arc<AtomicBool>,
    subscriptions: Arc<Mutex<HashSet<String>>>,
    topic: String,
) {
    let mut sent = None;
    while active(&alive, &subscriptions, &topic) {
        let agents: Vec<Value> = state
            .supervisor
            .registry
            .list(None, None)
            .iter()
            .map(|agent| core_to_value(agent.status_json()))
            .collect();
        let snapshot = json!({"topic":"agents","snapshot":true,"agents":agents});
        let fingerprint = snapshot.to_string();
        if sent.as_ref() != Some(&fingerprint) && enqueue(&sender, snapshot) {
            sent = Some(fingerprint);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn publish_events(
    store: Arc<Store>,
    sender: SyncSender<Value>,
    alive: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    subscriptions: Arc<Mutex<HashSet<String>>>,
    topic: String,
    (mut after, mut epoch): (u64, String),
) {
    while active(&alive, &subscriptions, &topic) {
        let Ok(read) = store.read(after, &epoch, Duration::from_secs(1)) else {
            dropped.store(true, Ordering::Release);
            continue;
        };
        if read.reset || read.lost {
            dropped.store(true, Ordering::Release);
        }
        for event in read.events {
            let frame = json!({"topic":"events","event":core_to_value(event.response_json())});
            if !enqueue(&sender, frame) {
                dropped.store(true, Ordering::Release);
            }
        }
        after = read.next;
        epoch = read.epoch;
    }
}

fn publish_logs(
    id: String,
    state: Arc<Server>,
    sender: SyncSender<Value>,
    alive: Arc<AtomicBool>,
    subscriptions: Arc<Mutex<HashSet<String>>>,
    topic: String,
    mut after: u64,
) {
    while active(&alive, &subscriptions, &topic) {
        let Some(logs) = agent_logs_json(&state.supervisor.registry, &id, "both", after, None)
        else {
            let _ = enqueue(
                &sender,
                json!({"type":"error","error":"unknown_agent","topic":format!("logs.{id}")}),
            );
            return;
        };
        let logs = core_to_value(logs);
        if let Some(records) = logs.get("records").and_then(Value::as_array) {
            for record in records {
                if !enqueue(
                    &sender,
                    json!({"topic":format!("logs.{id}"),"record":record}),
                ) {
                    // Logs are optional diagnostic tails. A slow client can fetch a
                    // durable range through HTTP rather than holding the relay hostage.
                    return;
                }
            }
        }
        after = logs
            .get("next_cursor")
            .and_then(Value::as_u64)
            .unwrap_or(after);
        thread::sleep(Duration::from_millis(100));
    }
}

fn active(alive: &AtomicBool, subscriptions: &Mutex<HashSet<String>>, topic: &str) -> bool {
    alive.load(Ordering::Acquire)
        && subscriptions
            .lock()
            .is_ok_and(|subscriptions| subscriptions.contains(topic))
}

fn enqueue(sender: &SyncSender<Value>, value: Value) -> bool {
    match sender.try_send(value) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    }
}

fn write_loop(
    mut stream: TcpStream,
    receiver: Receiver<Value>,
    alive: Arc<AtomicBool>,
    events_dropped: Arc<AtomicBool>,
) {
    while alive.load(Ordering::Acquire) {
        let frame = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => frame,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if events_dropped.swap(false, Ordering::AcqRel)
            && write_frame(&mut stream, &json!({"topic":"events","dropped":true})).is_err()
        {
            alive.store(false, Ordering::Release);
            break;
        }
        if write_frame(&mut stream, &frame).is_err() {
            alive.store(false, Ordering::Release);
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn core_to_value(value: Json) -> Value {
    serde_json::from_str(&value.to_json()).expect("relay Json serializes as valid JSON")
}

pub(crate) fn read_frame(stream: &mut TcpStream) -> Result<Value, String> {
    let mut prefix = [0u8; 4];
    read_exact(stream, &mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err("invalid frame length".to_owned());
    }
    let mut body = vec![0; length];
    read_exact(stream, &mut body)?;
    serde_json::from_slice(&body).map_err(|_| "invalid JSON frame".to_owned())
}

pub(crate) fn write_frame(stream: &mut TcpStream, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if body.len() > MAX_FRAME_BYTES {
        return Err("frame too large".to_owned());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| error.to_string())
}

fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), String> {
    stream
        .read_exact(buffer)
        .map_err(|error| match error.kind() {
            ErrorKind::UnexpectedEof => "truncated frame".to_owned(),
            ErrorKind::TimedOut | ErrorKind::WouldBlock => "socket read timed out".to_owned(),
            _ => error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_core::{AgentRecord, Json};
    use std::net::TcpListener;

    #[test]
    fn framing_is_big_endian_json_and_rejects_bad_lengths() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(&[0, 0, 0, 11]).unwrap();
            stream.write_all(br#"{"ok":true}"#).unwrap();
            read_frame(&mut stream).unwrap()
        });
        let (mut server, _) = listener.accept().unwrap();
        assert_eq!(read_frame(&mut server).unwrap(), json!({"ok":true}));
        write_frame(&mut server, &json!({"reply":"ok"})).unwrap();
        assert_eq!(client.join().unwrap(), json!({"reply":"ok"}));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(&(0u32).to_be_bytes()).unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        assert_eq!(
            read_frame(&mut server),
            Err("invalid frame length".to_owned())
        );
        client.join().unwrap();
    }

    #[test]
    fn socket_authenticates_then_pushes_agent_and_event_topics() {
        let (state, state_path) = crate::tests::test_server();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handler_state = Arc::clone(&state);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, handler_state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write_frame(
            &mut client,
            &json!({"type":"auth","authorization":format!("Bearer {}", "x".repeat(32))}),
        )
        .unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap(),
            json!({"type":"authenticated"})
        );
        write_frame(&mut client, &json!({"type":"subscribe","topic":"agents"})).unwrap();
        write_frame(&mut client, &json!({"type":"subscribe","topic":"events"})).unwrap();

        let mut saw_agents = false;
        let mut saw_events_subscription = false;
        for _ in 0..4 {
            let frame = read_frame(&mut client).unwrap();
            saw_agents |= frame.get("topic") == Some(&Value::String("agents".to_owned()));
            saw_events_subscription |= frame == json!({"type":"subscribed","topic":"events"});
            if saw_agents && saw_events_subscription {
                break;
            }
        }
        assert!(saw_agents);
        assert!(saw_events_subscription);
        state
            .store
            .add(Json::Object(vec![(
                "id".to_owned(),
                Json::String("socket-event".to_owned()),
            )]))
            .unwrap();
        let mut event = None;
        for _ in 0..3 {
            let frame = read_frame(&mut client).unwrap();
            if frame.get("topic") == Some(&Value::String("events".to_owned()))
                && frame.get("event").is_some()
            {
                event = Some(frame);
                break;
            }
        }
        assert_eq!(
            event
                .as_ref()
                .and_then(|frame| frame.get("event"))
                .and_then(|event| event.get("id"))
                .and_then(Value::as_str),
            Some("socket-event")
        );
        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn socket_replays_events_from_the_http_handoff_cursor() {
        let (state, state_path) = crate::tests::test_server();
        let epoch = state.store.read(0, "", Duration::ZERO).unwrap().epoch;
        state
            .store
            .add(Json::Object(vec![(
                "id".to_owned(),
                Json::String("handoff-event".to_owned()),
            )]))
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handler_state = Arc::clone(&state);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, handler_state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write_frame(
            &mut client,
            &json!({"type":"auth","authorization":format!("Bearer {}", "x".repeat(32))}),
        )
        .unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap(),
            json!({"type":"authenticated"})
        );
        write_frame(
            &mut client,
            &json!({"type":"subscribe","topic":"events","after":0,"epoch":epoch}),
        )
        .unwrap();
        let replayed = (0..3)
            .map(|_| read_frame(&mut client).unwrap())
            .find(|frame| frame.get("event").is_some())
            .unwrap();
        assert_eq!(
            replayed.pointer("/event/id").and_then(Value::as_str),
            Some("handoff-event")
        );
        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn socket_reports_a_bounded_event_queue_drop_before_the_next_frame() {
        let (state, state_path) = crate::tests::test_server();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = sync_channel(EVENT_QUEUE_CAPACITY);
        for _ in 0..EVENT_QUEUE_CAPACITY {
            sender
                .send(json!({"topic":"events","event":{"id":"queued"}}))
                .unwrap();
        }
        let alive = Arc::new(AtomicBool::new(true));
        let dropped = Arc::new(AtomicBool::new(false));
        let subscriptions = Arc::new(Mutex::new(HashSet::from(["events".to_owned()])));
        let epoch = state.store.read(0, "", Duration::ZERO).unwrap().epoch;
        state
            .store
            .add(Json::Object(vec![(
                "id".to_owned(),
                Json::String("overflow-event".to_owned()),
            )]))
            .unwrap();
        let publisher_state = state.store.clone();
        let publisher_sender = sender.clone();
        let publisher_alive = Arc::clone(&alive);
        let publisher_dropped = Arc::clone(&dropped);
        let publisher_subscriptions = Arc::clone(&subscriptions);
        thread::spawn(move || {
            publish_events(
                publisher_state,
                publisher_sender,
                publisher_alive,
                publisher_dropped,
                publisher_subscriptions,
                "events".to_owned(),
                (0, epoch),
            );
        });
        for _ in 0..100 {
            if dropped.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dropped.load(Ordering::Acquire),
            "the event publisher did not report the full queue"
        );
        let writer_alive = Arc::clone(&alive);
        let writer_dropped = Arc::clone(&dropped);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            write_loop(stream, receiver, writer_alive, writer_dropped);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap(),
            json!({"topic":"events","dropped":true})
        );
        assert_eq!(
            read_frame(&mut client)
                .unwrap()
                .pointer("/event/id")
                .and_then(Value::as_str),
            Some("queued")
        );
        alive.store(false, Ordering::Release);
        subscriptions.lock().unwrap().clear();
        drop(sender);
        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn socket_streams_agent_logs_and_rejects_unknown_log_agents() {
        let (state, state_path) = crate::tests::test_server();
        let id = "a".repeat(32);
        let transcript_path = crate::proc::agent_transcript_path(&id).unwrap();
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(&transcript_path, "initial API transcript\n").unwrap();
        state
            .supervisor
            .registry
            .register(AgentRecord {
                id: id.clone(),
                task_id: "task".to_owned(),
                execution_id: "execution".to_owned(),
                leader_pid: 1,
                process_group: 1,
                process_identity: None,
                worktree_path: None,
                started_at: "0".to_owned(),
                deadline_at: None,
                command: "test".to_owned(),
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
            })
            .unwrap();
        // API-created agents retain this durable transcript, but their live
        // stdout is also now recorded in the ordered spool used by sockets.
        state
            .supervisor
            .registry
            .append_log(&id, "stdout", b"initial API transcript\n")
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handler_state = Arc::clone(&state);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, handler_state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write_frame(
            &mut client,
            &json!({"type":"auth","authorization":format!("Bearer {}", "x".repeat(32))}),
        )
        .unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap(),
            json!({"type":"authenticated"})
        );
        write_frame(
            &mut client,
            &json!({"type":"subscribe","topic":format!("logs.{id}")}),
        )
        .unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap(),
            json!({"type":"subscribed","topic":format!("logs.{id}")})
        );
        state
            .supervisor
            .registry
            .append_log(&id, "stdout", b"socket-log\n")
            .unwrap();
        let record = (0..4)
            .map(|_| read_frame(&mut client).unwrap())
            .find(|frame| frame.get("record").is_some())
            .unwrap();
        assert_eq!(
            record.pointer("/record/data").and_then(Value::as_str),
            Some("socket-log\n")
        );
        write_frame(
            &mut client,
            &json!({"type":"subscribe","topic":"logs.unknown"}),
        )
        .unwrap();
        let unknown = (0..4)
            .map(|_| read_frame(&mut client).unwrap())
            .find(|frame| frame.get("error") == Some(&Value::String("unknown_agent".to_owned())))
            .unwrap();
        assert_eq!(
            unknown.get("topic").and_then(Value::as_str),
            Some("logs.unknown")
        );
        drop(client);
        server.join().unwrap();
        let _ = std::fs::remove_file(state_path.with_extension("agents"));
        let _ = std::fs::remove_dir_all(state_path.with_extension("agent-logs"));
        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(transcript_path);
    }

    #[test]
    fn socket_rejects_a_non_bearer_first_frame() {
        let (state, state_path) = crate::tests::test_server();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert!(handle(stream, state).is_err());
        });
        let mut client = TcpStream::connect(address).unwrap();
        write_frame(&mut client, &json!({"type":"auth","authorization":"wrong"})).unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap(),
            json!({"type":"error","error":"unauthorized"})
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(state_path);
    }
}
