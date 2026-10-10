use std::io::Read;
use std::sync::OnceLock;
use zz::{AgentRecord, Json, Store};

pub(crate) fn unix_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}
pub(crate) fn relay_timestamp() -> String {
    zz::rfc3339_timestamp()
}
pub(crate) fn relay_clock() -> String {
    static CLOCK: OnceLock<String> = OnceLock::new();
    CLOCK
        .get_or_init(|| {
            let host = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "unknown-host".to_owned());
            // Linux exposes a real boot identifier; macOS has no equivalent stable
            // portable file in this no-dependency relay, so make that absence explicit.
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "boot-unknown".to_owned());
            // The instance suffix prevents a relay restart from being treated
            // as one continuous clock when a host boot identifier is absent.
            format!(
                "mac-relay:{host}:{boot}:instance-{}",
                random_hex_128().unwrap_or_else(|_| format!("pid-{}", std::process::id()))
            )
        })
        .clone()
}
pub(crate) fn random_hex_128() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| format!("could not generate identifier: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
pub(crate) fn new_execution_id() -> Result<String, String> {
    Ok(format!("relay-{}", random_hex_128()?))
}
pub(crate) fn relay_event(kind: &str, task_id: &str, execution_id: &str, payload: Json) -> Json {
    relay_event_at(kind, task_id, execution_id, relay_timestamp(), payload)
}
pub(crate) fn relay_event_at(
    kind: &str,
    task_id: &str,
    execution_id: &str,
    occurred_at: String,
    payload: Json,
) -> Json {
    Json::Object(vec![
        (
            "id".to_owned(),
            Json::String(format!("relay:{execution_id}:{kind}")),
        ),
        ("schema_version".to_owned(), Json::number(1)),
        ("task_id".to_owned(), Json::String(task_id.to_owned())),
        (
            "execution_id".to_owned(),
            Json::String(execution_id.to_owned()),
        ),
        ("kind".to_owned(), Json::String(kind.to_owned())),
        ("source".to_owned(), Json::String("mac-relay".to_owned())),
        ("occurred_at".to_owned(), Json::String(occurred_at)),
        ("clock".to_owned(), Json::String(relay_clock())),
        ("payload".to_owned(), payload),
    ])
}
pub(crate) fn persist_first_output(store: &Store, agent: &AgentRecord) -> Result<(), String> {
    let Some(occurred_at) = agent.first_output_at.clone() else {
        return Ok(());
    };
    store
        .add(relay_event_at(
            "first_output",
            &agent.task_id,
            &agent.execution_id,
            occurred_at,
            Json::Object(vec![
                ("agent_id".to_owned(), Json::String(agent.id.clone())),
                (
                    "stream".to_owned(),
                    Json::String(
                        agent
                            .first_output_stream
                            .clone()
                            .unwrap_or_else(|| "unknown".to_owned()),
                    ),
                ),
                (
                    "bytes".to_owned(),
                    Json::number(agent.first_output_bytes.unwrap_or_default()),
                ),
            ]),
        ))
        .map(|_| ())
}
pub(crate) fn replay_recovered_lifecycle(store: &Store, agent: &AgentRecord) -> Result<(), String> {
    store.add(relay_event(
        "process_spawned",
        &agent.task_id,
        &agent.execution_id,
        Json::Object(vec![
            ("agent_id".to_owned(), Json::String(agent.id.clone())),
            ("recovered".to_owned(), Json::Bool(true)),
        ]),
    ))?;
    persist_first_output(store, agent)
}
