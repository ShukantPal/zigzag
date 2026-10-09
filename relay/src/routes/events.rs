use relay_core::{Json, Store, parse_rfc3339_millis};
use std::env;
use std::path::PathBuf;

pub(crate) fn run_timeline(arguments: &[String]) -> Result<(), String> {
    let mut state_file = env::var_os("ZIGZAG_STATE_FILE").map(PathBuf::from);
    let mut task_id = None;
    let mut values = arguments.iter();
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--state-file" => {
                state_file = Some(PathBuf::from(
                    values
                        .next()
                        .ok_or_else(|| "--state-file requires a value".to_owned())?,
                ));
            }
            "--help" | "-h" => {
                return Err(
                    "usage: zigzag timeline <task-id> --state-file PATH (or ZIGZAG_STATE_FILE)"
                        .to_owned(),
                );
            }
            value if !value.starts_with('-') && task_id.is_none() => {
                task_id = Some(value.to_owned())
            }
            value => return Err(format!("unknown timeline argument: {value}")),
        }
    }
    let task_id = task_id.ok_or_else(|| "timeline requires a task id".to_owned())?;
    let state_file = state_file
        .ok_or_else(|| "--state-file or ZIGZAG_STATE_FILE is required for timeline".to_owned())?;
    let events = Store::open(state_file, 1_000)?.timeline(&task_id)?;
    print!("{}", timeline_output(&task_id, &events));
    Ok(())
}
pub(crate) fn timeline_output(task_id: &str, events: &[Json]) -> String {
    if events.is_empty() {
        return format!("No durable audit events for task {task_id}.\n");
    }
    let mut output = format!("Timeline for {task_id}\n");
    for event in events {
        output.push_str(&format!(
            "{}  {}  {}  {}",
            event_text(event, "occurred_at"),
            event_text(event, "source"),
            event_text(event, "kind"),
            event_text(event, "clock"),
        ));
        output.push('\n');
    }
    output.push_str("\nPhase durations (only same-clock facts are subtracted):\n");
    for (label, start, end) in [
        ("dispatch", "task_dispatched", "relay_request_started"),
        ("relay/launch", "relay_accepted", "process_spawned"),
        ("agent work", "process_spawned", "process_completed"),
        ("agent failure", "process_spawned", "process_failed"),
        ("time to first output", "process_spawned", "first_output"),
        ("delivery/poll health", "poll_started", "poller_received"),
        ("review", "review_wait_started", "review_work_started"),
        ("human wait", "human_wait_started", "human_wait_ended"),
    ] {
        if let Some((left, right)) = phase_events(events, start, end) {
            match same_clock_duration(left, right) {
                Some(duration) => {
                    output.push_str(&format!("{label}: {}\n", format_duration(duration)))
                }
                None => output.push_str(&format!(
                    "{label}: cross-clock/unknown ({} → {})\n",
                    event_text(left, "occurred_at"),
                    event_text(right, "occurred_at")
                )),
            }
        }
    }
    output
}
pub(crate) fn event_text<'a>(event: &'a Json, field: &str) -> &'a str {
    event.object(field).and_then(Json::as_str).unwrap_or("?")
}
pub(crate) fn phase_events<'a>(
    events: &'a [Json],
    start: &str,
    end: &str,
) -> Option<(&'a Json, &'a Json)> {
    for (index, left) in events.iter().enumerate() {
        if event_text(left, "kind") != start {
            continue;
        }
        let execution_id = event_text(left, "execution_id");
        if let Some(right) = events[index + 1..].iter().find(|event| {
            event_text(event, "kind") == end && event_text(event, "execution_id") == execution_id
        }) {
            return Some((left, right));
        }
    }
    None
}
pub(crate) fn same_clock_duration(left: &Json, right: &Json) -> Option<u64> {
    (event_text(left, "clock") == event_text(right, "clock"))
        .then(|| {
            timestamp_millis(event_text(right, "occurred_at"))?
                .checked_sub(timestamp_millis(event_text(left, "occurred_at"))?)
        })
        .flatten()
}
pub(crate) fn timestamp_millis(value: &str) -> Option<u64> {
    parse_rfc3339_millis(value)
}
pub(crate) fn format_duration(millis: u64) -> String {
    format!("{}.{:03}s", millis / 1_000, millis % 1_000)
}
