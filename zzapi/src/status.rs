//! Read-only agent status dashboard migrated from dept/status.py.
use super::{ApiError, Client, Fail};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal as RatatuiTerminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const INTERNAL: &str = "relay-update";
const EVENT_PROMPT_LIMIT: usize = 100;
const EVENT_LIMIT: usize = 10_000;
// The TUI keeps at most one bounded byte window of transcript data in memory.
const TRANSCRIPT_WINDOW_BYTES: usize = 512 * 1024;
const STATUS_STATES: &[&str] = &[
    "not observed",
    "running",
    "succeeded",
    "failed",
    "interrupted",
    "unexpected_exit",
    "lost_after_restart",
    "stopped",
    "killed",
    "finished",
    "orphaned",
    "paused",
];
#[derive(Clone, Default)]
struct Execution {
    task: String,
    id: String,
    events: Vec<Value>,
    state: String,
    agent: String,
    command: String,
    pr: String,
    branch: String,
    started_at: String,
    exit_code: String,
    started: Option<i64>,
    worktree: String,
    audit_bad: bool,
    log_bad: bool,
}
fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(x)) => x.to_string(),
        Some(Value::Bool(x)) => x.to_string(),
        _ => String::new(),
    }
}
fn request_summary(event: &Value) -> Option<String> {
    if s(event, "kind") != "relay_request_started" {
        return None;
    }
    let payload = event.get("payload")?;
    if let Some(prompt) = payload.get("prompt").and_then(Value::as_str) {
        let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        let prompt = truncate(&prompt, EVENT_PROMPT_LIMIT);
        let model = payload.get("model").and_then(Value::as_str).unwrap_or("");
        return Some(if model.is_empty() {
            format!("prompt: {prompt}")
        } else {
            format!("prompt: {prompt} · model: {model}")
        });
    }
    let process = payload.get("process").and_then(Value::as_str)?;
    let command = match payload.get("command") {
        Some(Value::Array(args)) => args
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::String(command)) => command.clone(),
        _ => String::new(),
    };
    Some(if command.is_empty() {
        format!("process: {process}")
    } else {
        format!(
            "process: {process} · command: {}",
            truncate(&command, EVENT_PROMPT_LIMIT)
        )
    })
}
fn truncate(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let clipped = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yo = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    era * 146097 + yo * 365 + yo / 4 - yo / 100 + doy - 719468
}
fn timestamp(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(n) = v.as_f64() {
        return Some(n as i64);
    }
    let x = v.as_str()?;
    if let Ok(n) = x.parse::<i64>() {
        return Some(n);
    }
    if x.len() < 19 {
        return None;
    }
    let n = |a, b| x.get(a..b)?.parse::<i64>().ok();
    let mut t = days(n(0, 4)?, n(5, 7)?, n(8, 10)?) * 86400
        + n(11, 13)? * 3600
        + n(14, 16)? * 60
        + n(17, 19)?;
    let tz = x.get(19..).unwrap_or("");
    if let Some(i) = tz.find(['+', '-']) {
        let sign = if tz.as_bytes()[i] == b'+' { 1 } else { -1 };
        let (h, m) = tz[i + 1..].split_once(':').unwrap_or((&tz[i + 1..], "0"));
        t -= sign
            * (h.parse::<i64>().ok()? * 3600 + m.get(..2).unwrap_or("0").parse::<i64>().ok()? * 60)
    }
    Some(t)
}
fn dur(v: Option<i64>) -> String {
    let Some(mut x) = v else {
        return "not observed".into();
    };
    if x < 60 {
        return format!("{x}s");
    }
    let m = x / 60;
    x %= 60;
    if m < 60 {
        format!("{m}m {x:02}s")
    } else {
        format!("{}h {:02}m", m / 60, m % 60)
    }
}
fn short_time(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(x)) => x.chars().take(19).collect::<String>().replace('T', " "),
        Some(Value::Number(x)) => x.to_string(),
        _ => "?".into(),
    }
}
impl Execution {
    fn phase(&self) -> String {
        self.events
            .last()
            .map(|e| s(e, "kind"))
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| {
                if self.state.is_empty() {
                    "not observed".into()
                } else {
                    format!("agent {}", self.state)
                }
            })
    }
    fn latest(&self) -> String {
        self.events
            .last()
            .map(|e| short_time(e.get("occurred_at").or_else(|| e.get("received_at"))))
            .unwrap_or_else(|| "not observed".into())
    }
    fn current(&self, n: i64) -> Option<i64> {
        if self.state == "running" {
            return self.started.map(|v| (n - v).max(0));
        }
        let p = self.events.windows(2).last()?;
        elapsed(&p[0], &p[1])
    }
    fn total(&self, n: i64) -> (Option<i64>, bool) {
        if self.state == "running"
            && let Some(v) = self.current(n)
        {
            return (Some(v), false);
        }
        let (mut total, mut measured, mut cross) = (0, false, false);
        for p in self.events.windows(2) {
            if let Some(v) = elapsed(&p[0], &p[1]) {
                total += v;
                measured = true
            } else if !s(&p[0], "clock").is_empty()
                && !s(&p[1], "clock").is_empty()
                && s(&p[0], "clock") != s(&p[1], "clock")
            {
                cross = true
            }
        }
        (measured.then_some(total), cross)
    }
    fn flags(&self, lost: bool) -> String {
        let cross = self.total(now()).1;
        let mut x = vec![];
        if self.audit_bad {
            x.push("audit degraded")
        }
        if self.log_bad {
            x.push("agent log degraded")
        }
        if lost {
            x.push("relay events lost")
        }
        if cross {
            x.push("cross-clock boundary")
        }
        if x.is_empty() {
            "healthy".into()
        } else {
            x.join(", ")
        }
    }
}
fn elapsed(a: &Value, b: &Value) -> Option<i64> {
    let clock = s(a, "clock");
    if clock.is_empty() || clock != s(b, "clock") {
        return None;
    }
    let (x, y) = (
        timestamp(a.get("occurred_at"))?,
        timestamp(b.get("occurred_at"))?,
    );
    if y < x { None } else { Some(y - x) }
}
fn default_state() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/zigzag/events.json")
}
fn worktree_map(state: &Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(x) = std::fs::read_to_string(state.with_extension("agents.json"))
        && let Ok(v) = serde_json::from_str::<Value>(&x)
        && let Some(a) = v.get("agents").and_then(Value::as_array)
    {
        for r in a {
            let (t, w) = (s(r, "task_id"), s(r, "worktree_path"));
            if !t.is_empty() && !w.is_empty() {
                m.insert(t, w);
            }
        }
    }
    m
}
fn git_branch(worktree: &str) -> String {
    if worktree.is_empty() {
        return String::new();
    }
    let worktree = Path::new(worktree);
    let dot_git = worktree.join(".git");
    let git_dir = match std::fs::metadata(&dot_git) {
        Ok(metadata) if metadata.is_dir() => dot_git,
        _ => {
            let Ok(pointer) = std::fs::read_to_string(dot_git) else {
                return String::new();
            };
            let Some(path) = pointer.trim().strip_prefix("gitdir:") else {
                return String::new();
            };
            let path = PathBuf::from(path.trim());
            if path.is_absolute() {
                path
            } else {
                worktree.join(path)
            }
        }
    };
    std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_owned)
        })
        .unwrap_or_default()
}
fn audit(state: &Path) -> (Vec<Value>, Vec<String>) {
    let dir = state.with_extension("audit");
    let mut es = vec![];
    let mut ws = vec![];
    let Ok(files) = std::fs::read_dir(&dir) else {
        return (
            es,
            vec![format!("audit directory unavailable: {}", dir.display())],
        );
    };
    for f in files.flatten() {
        if f.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let name = f.file_name().to_string_lossy().to_string();
        match std::fs::read_to_string(f.path()) {
            Ok(data) => {
                for (i, line) in data.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(line) {
                        Ok(v) if v.is_object() => es.push(v),
                        Ok(_) => ws.push(format!(
                            "degraded audit log {name}:{}: event is not an object",
                            i + 1
                        )),
                        Err(e) => ws.push(format!("degraded audit log {name}:{}: {e}", i + 1)),
                    }
                }
            }
            Err(e) => ws.push(format!("degraded audit log {name}: {e}")),
        }
    }
    (es, ws)
}
fn build(
    mut events: Vec<Value>,
    agents: &[Value],
    wt: &HashMap<String, String>,
    all: bool,
) -> Vec<Execution> {
    let mut m: BTreeMap<String, Execution> = BTreeMap::new();
    events.sort_by_key(|e| (s(e, "received_at"), s(e, "occurred_at"), s(e, "sequence")));
    if events.len() > EVENT_LIMIT {
        events.drain(..events.len() - EVENT_LIMIT);
    }
    for e in events {
        let (id, task) = (s(&e, "execution_id"), s(&e, "task_id"));
        if id.is_empty() || (!all && task == INTERNAL) {
            continue;
        }
        let r = m.entry(id.clone()).or_insert_with(|| Execution {
            id,
            ..Default::default()
        });
        if !task.is_empty() {
            r.task = task
        }
        r.events.push(e)
    }
    for a in agents {
        let task = s(a, "task_id");
        if !all && task == INTERNAL {
            continue;
        }
        let mut id = s(a, "execution_id");
        if id.is_empty() {
            if !task.starts_with("agent-") {
                continue;
            }
            id = format!("agent-task:{task}")
        }
        let r = m.entry(id.clone()).or_insert_with(|| Execution {
            task: task.clone(),
            id,
            ..Default::default()
        });
        if task.starts_with("agent-") {
            r.task = task.clone()
        }
        r.state = s(a, "state");
        r.agent = s(a, "id");
        r.command = s(a, "command");
        let config = s(a, "agent_config");
        let config = serde_json::from_str::<Value>(&config).unwrap_or(Value::Null);
        r.pr = ["pr", "pull_request", "pr_number"]
            .iter()
            .map(|key| s(&config, key))
            .find(|value| !value.is_empty())
            .map(|value| format!("#{value}"))
            .unwrap_or_default();
        r.branch = s(&config, "branch");
        r.exit_code = if a.get("exit_code").is_some_and(Value::is_null) {
            String::new()
        } else {
            s(a, "exit_code")
        };
        r.started = timestamp(a.get("started_at"));
        r.started_at = s(a, "started_at");
        r.audit_bad = a
            .get("audit_degraded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        r.log_bad = a
            .get("log_degraded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        r.worktree = s(a, "worktree_path");
        if r.worktree.is_empty() {
            r.worktree = s(a, "worktree")
        }
        if r.worktree.is_empty() {
            r.worktree = wt.get(&task).cloned().unwrap_or_default()
        }
    }
    let mut out: Vec<_> = m.into_values().collect();
    let mut branches = HashMap::<String, String>::new();
    for r in &mut out {
        if r.worktree.is_empty() {
            r.worktree = wt.get(&r.task).cloned().unwrap_or_default()
        }
        if r.branch.is_empty() {
            r.branch = branches
                .entry(r.worktree.clone())
                .or_insert_with(|| git_branch(&r.worktree))
                .clone();
        }
    }
    out.sort_by(|a, b| {
        (a.state != "running")
            .cmp(&(b.state != "running"))
            .then_with(|| b.latest().cmp(&a.latest()))
    });
    out
}
fn resolve_selection(
    rows: &[Execution],
    selected_id: Option<&str>,
    previous_index: usize,
) -> usize {
    selected_id
        .and_then(|id| rows.iter().position(|row| row.id == id))
        .unwrap_or_else(|| previous_index.min(rows.len().saturating_sub(1)))
}
fn displayed_state(row: &Execution) -> &str {
    if row.state.is_empty() {
        "not observed"
    } else {
        &row.state
    }
}
fn filter_rows(rows: Vec<Execution>, live_only: bool, state: Option<&str>) -> Vec<Execution> {
    rows.into_iter()
        .filter(|row| !live_only || !row.agent.is_empty())
        .filter(|row| state.is_none_or(|state| displayed_state(row) == state))
        .collect()
}
fn validate_state(state: &str) -> Result<(), Fail> {
    if STATUS_STATES.contains(&state) {
        Ok(())
    } else {
        Err(Fail::Config(format!(
            "unknown status state '{state}' (valid states: {})",
            STATUS_STATES.join(", ")
        )))
    }
}
fn snapshot(
    c: &Client,
    state: &Path,
    all: bool,
) -> Result<(Vec<Execution>, Vec<String>, bool), ApiError> {
    let (mut es, mut warnings) = audit(state);
    let agents_response = c.request("GET", "/v1/agents", &[], None, 10);
    let agents = match agents_response {
        Ok(value) => value
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(error) => {
            warnings.push(format!("relay agents unavailable: {}", error.message));
            Vec::new()
        }
    };
    let response = c.request(
        "GET",
        "/v1/events",
        &[("after", "0".into()), ("timeout", "0".into())],
        None,
        10,
    );
    let response = match response {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(format!("relay events unavailable: {}", error.message));
            None
        }
    };
    let mut ids: HashSet<String> = es
        .iter()
        .map(|e| s(e, "id"))
        .filter(|x| !x.is_empty())
        .collect();
    for e in response
        .as_ref()
        .and_then(|value| value.get("events"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = s(e, "id");
        if id.is_empty() || ids.insert(id) {
            es.push(e.clone())
        }
    }
    let lost = response
        .as_ref()
        .and_then(|value| value.get("lost"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if lost {
        warnings.push("relay event retention lost earlier events".into())
    }
    let wt = worktree_map(state);
    Ok((build(es, &agents, &wt, all), warnings, lost))
}
fn task_dir(task: &str, root: &Path) -> String {
    let t = task.strip_prefix("codex-").unwrap_or(task);
    std::fs::read_to_string(root.join(t).join("dir.txt"))
        .unwrap_or_default()
        .trim()
        .into()
}
fn tail_path(path: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(path) <= max_width {
        return path.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let ellipsis_width = UnicodeWidthChar::width('…').unwrap_or(1);
    if max_width <= ellipsis_width {
        return "…".into();
    }
    let mut suffix = String::new();
    let mut width = 0;
    for ch in path.chars().rev() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width - ellipsis_width {
            break;
        }
        suffix.push(ch);
        width += char_width;
    }
    format!("…{}", suffix.chars().rev().collect::<String>())
}
fn print_once(rows: &[Execution], warnings: &[String], root: &Path, lost: bool) {
    println!("TASK\tPHASE\tPHASE ELAPSED\tOBSERVED TOTAL\tSTATE\tAGENT ID\tDIR\tLAST EVENT\tFLAGS");
    for e in rows {
        let (total, cross) = e.total(now());
        let dir = if e.worktree.is_empty() {
            task_dir(&e.task, root)
        } else {
            e.worktree.clone()
        };
        println!(
            "{}\t{}\t{}\t{}{}\t{}\t{}\t{}\t{}\t{}",
            e.task,
            e.phase(),
            dur(e.current(now())),
            dur(total),
            if cross { " (cross-clock)" } else { "" },
            if e.state.is_empty() {
                "not observed"
            } else {
                &e.state
            },
            if e.agent.is_empty() { "-" } else { &e.agent },
            dir,
            e.latest(),
            e.flags(lost)
        );
    }
    for w in warnings {
        eprintln!("WARNING: {w}")
    }
}
#[derive(Clone, Debug)]
struct TranscriptItem {
    id: String,
    item: Value,
    state: String,
    raw: Value,
    diagnostic: Option<String>,
    hidden: bool,
}

fn codex_transcript_path(agent_id: &str) -> Option<PathBuf> {
    if agent_id.len() != 32 || !agent_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".zigzag/agents/codex")
            .join(format!("{agent_id}.jsonl")),
    )
}

fn parse_codex_transcript(text: &str) -> Vec<TranscriptItem> {
    let mut items: Vec<TranscriptItem> = Vec::new();
    let mut indices = HashMap::<String, usize>::new();
    let mut pending_deltas = HashMap::<String, (String, Vec<Value>)>::new();
    let complete = if text.ends_with('\n') {
        text
    } else {
        text.rfind('\n').map_or("", |end| &text[..=end])
    };
    for (line_number, line) in complete.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            items.push(TranscriptItem {
                id: format!("diagnostic-{line_number}"),
                item: Value::Null,
                state: "diagnostic".into(),
                raw: Value::String(line.to_owned()),
                diagnostic: Some(format!(
                    "Malformed JSONL record at line {}",
                    line_number + 1
                )),
                hidden: false,
            });
            continue;
        };
        // Codex CLI JSONL uses `type`, while app-server notifications (which
        // the relay persists) use `method` and carry items under params.item.
        let kind = event
            .get("type")
            .or_else(|| event.get("method"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('/', ".");
        let params = event.get("params").unwrap_or(&Value::Null);
        if kind == "item.agentMessage.delta" || kind == "item.agent_message.delta" {
            let item_id = params
                .get("itemId")
                .or_else(|| params.get("item_id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let delta = s(params, "delta");
            if item_id.is_empty() {
                push_transcript_gap(&mut items, line_number, &event);
                continue;
            }
            if let Some(index) = indices.get(item_id).copied() {
                append_agent_message_delta(&mut items[index].item, &delta);
                append_transcript_raw(&mut items[index], event);
            } else {
                pending_deltas
                    .entry(item_id.to_owned())
                    .and_modify(|(text, events)| {
                        text.push_str(&delta);
                        events.push(event.clone());
                    })
                    .or_insert((delta, vec![event]));
            }
            continue;
        }
        if kind == "item.reasoning.summaryTextDelta" {
            let item_id = s(params, "itemId");
            let delta = s(params, "delta");
            let Some(index) = indices.get(&item_id).copied() else {
                push_transcript_gap(&mut items, line_number, &event);
                continue;
            };
            let text = s(&items[index].item, "text");
            items[index].item["text"] = Value::String(format!("{text}{delta}"));
            append_transcript_raw(&mut items[index], event);
            continue;
        }
        if kind.starts_with("item.commandExecution.") {
            let item_id = s(params, "itemId");
            if item_id.is_empty() {
                push_transcript_gap(&mut items, line_number, &event);
                continue;
            }
            let key = item_id.clone();
            let index = if let Some(index) = indices.get(&key).copied() {
                index
            } else if kind.ends_with(".started") {
                let index = items.len();
                items.push(TranscriptItem {
                    id: key.clone(),
                    item: serde_json::json!({"id":item_id,"type":"command_execution","status":"in_progress"}),
                    state: "started".into(),
                    raw: event.clone(),
                    diagnostic: None,
                    hidden: false,
                });
                indices.insert(key, index);
                index
            } else {
                push_transcript_gap(&mut items, line_number, &event);
                continue;
            };
            let item = &mut items[index].item;
            for key in ["command", "cwd", "exitCode", "aggregatedOutput", "output"] {
                if let Some(value) = params.get(key) {
                    let canonical = match key {
                        "exitCode" => "exit_code",
                        "aggregatedOutput" => "aggregated_output",
                        _ => key,
                    };
                    item[canonical] = value.clone();
                }
            }
            if kind.ends_with(".outputDelta") {
                let output = s(item, "aggregated_output");
                item["aggregated_output"] =
                    Value::String(format!("{output}{}", s(params, "delta")));
            }
            if kind.ends_with(".completed") {
                let status = s(params, "status");
                item["status"] = Value::String(if status.is_empty() {
                    "completed".into()
                } else {
                    status
                });
            }
            items[index].state = if kind.ends_with(".completed") {
                "completed".into()
            } else {
                "in_progress".into()
            };
            append_transcript_raw(&mut items[index], event);
            continue;
        }
        if kind == "turn.diff.updated" {
            let diff = params
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = "turn-diff".to_owned();
            if let Some(index) = indices.get(&id).copied() {
                items[index].item["diff"] = Value::String(diff.to_owned());
                append_transcript_raw(&mut items[index], event);
            } else {
                indices.insert(id.clone(), items.len());
                items.push(TranscriptItem {
                    id,
                    item: serde_json::json!({"type":"diff","diff":diff}),
                    state: "updated".into(),
                    raw: event,
                    diagnostic: None,
                    hidden: false,
                });
            }
            continue;
        }
        if kind.starts_with("item.")
            && (event.get("item").is_some()
                || params.get("item").is_some()
                || matches!(
                    kind.as_str(),
                    "item.started" | "item.updated" | "item.completed"
                ))
        {
            let Some(item) = event.get("item").or_else(|| event.pointer("/params/item")) else {
                push_transcript_gap(&mut items, line_number, &event);
                continue;
            };
            let item_id = s(item, "id");
            let key = if item_id.is_empty() {
                format!("anonymous-{line_number}")
            } else {
                item_id
            };
            let state = kind.strip_prefix("item.").unwrap_or(&kind).to_owned();
            if let Some(index) = indices.get(&key).copied() {
                let mut item = item.clone();
                if normalize_item_type(&s(&item, "type")) == "agent_message"
                    && s(&item, "text").is_empty()
                {
                    item["text"] = items[index]
                        .item
                        .get("text")
                        .cloned()
                        .unwrap_or(Value::String(String::new()));
                }
                items[index].item = item;
                items[index].state = state;
                items[index].diagnostic = None;
                items[index].hidden = false;
                append_transcript_raw(&mut items[index], event);
            } else {
                indices.insert(key.clone(), items.len());
                let mut item = item.clone();
                let mut raw = event;
                if let Some((delta, events)) = pending_deltas.remove(&key) {
                    append_agent_message_delta(&mut item, &delta);
                    raw = Value::Array(events.into_iter().chain(std::iter::once(raw)).collect());
                }
                items.push(TranscriptItem {
                    id: key,
                    item,
                    state,
                    raw,
                    diagnostic: None,
                    hidden: false,
                });
            }
        } else {
            let handled_lifecycle_event = matches!(
                kind.as_str(),
                "thread.started"
                    | "thread.name.updated"
                    | "thread.status.changed"
                    | "thread.tokenUsage.updated"
                    | "turn.started"
                    | "turn.completed"
                    | "turn.failed"
                    | "turn.diff.updated"
                    | "mcpServer.startupStatus.updated"
                    | "error"
            );
            let handled_item_event = matches!(
                kind.as_str(),
                "item.started"
                    | "item.updated"
                    | "item.completed"
                    | "item.agentMessage.delta"
                    | "item.agent_message.delta"
                    | "item.reasoning.summaryTextDelta"
                    | "item.commandExecution.started"
                    | "item.commandExecution.outputDelta"
                    | "item.commandExecution.completed"
            );
            let unknown = kind.is_empty() || (!handled_lifecycle_event && !handled_item_event);
            let label = if kind == "thread.status.changed" {
                let status = params.get("status").unwrap_or(&Value::Null);
                let status = status
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| s(status, "type"));
                Some(if status.is_empty() {
                    "Thread status changed".into()
                } else {
                    format!("Thread status: {status}")
                })
            } else if kind == "turn.failed" {
                Some(format!(
                    "Turn failed: {}",
                    s(event.get("error").unwrap_or(&Value::Null), "message")
                ))
            } else if kind == "error" {
                Some(format!("Error: {}", s(&event, "message")))
            } else if unknown || kind.starts_with("unknown") {
                let name = event
                    .get("method")
                    .or_else(|| event.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unrecognized app-server event");
                let payload = event.get("params").unwrap_or(&event);
                Some(format!(
                    "Event: {name} · {}",
                    truncate(&payload.to_string(), 120)
                ))
            } else {
                None
            };
            let hidden = !unknown && label.is_none();
            items.push(TranscriptItem {
                id: format!("event-{line_number}"),
                item: Value::Null,
                state: kind,
                raw: event,
                diagnostic: label,
                hidden,
            });
        }
    }
    for (item_id, (_, events)) in pending_deltas {
        let diagnostic = TranscriptItem {
            id: format!("orphan-delta-{item_id}"),
            item: Value::Null,
            state: "diagnostic".into(),
            raw: Value::Array(events),
            diagnostic: Some(format!(
                "Transcript delta references missing item {item_id} after reading the full transcript"
            )),
            hidden: false,
        };
        items.push(diagnostic);
    }
    items
}

fn append_agent_message_delta(item: &mut Value, delta: &str) {
    let current = s(item, "text");
    item["text"] = Value::String(format!("{current}{delta}"));
}

fn push_transcript_gap(items: &mut Vec<TranscriptItem>, line_number: usize, raw: &Value) {
    if let Some(last) = items.iter_mut().find(|item| item.id == "transcript-gap") {
        if let Some(events) = last.raw.as_array_mut() {
            events.push(raw.clone());
        }
        return;
    }
    items.push(TranscriptItem {
        id: "transcript-gap".into(),
        item: Value::Null,
        state: "gap".into(),
        raw: Value::Array(vec![raw.clone()]),
        diagnostic: Some(
            "Transcript gap: one or more events referenced an item that was not captured".into(),
        ),
        hidden: false,
    });
    let _ = line_number;
}

fn append_transcript_raw(item: &mut TranscriptItem, event: Value) {
    if let Some(events) = item.raw.as_array_mut() {
        events.push(event);
    } else {
        item.raw = Value::Array(vec![item.raw.clone(), event]);
    }
}

fn normalize_item_type(kind: &str) -> String {
    match kind {
        "agentMessage" => "agent_message".into(),
        "userMessage" => "user_message".into(),
        "commandExecution" => "command_execution".into(),
        "fileChange" => "file_change".into(),
        "mcpToolCall" => "mcp_tool_call".into(),
        "collabToolCall" => "collab_tool_call".into(),
        "webSearch" => "web_search".into(),
        "todoList" => "todo_list".into(),
        other => other.to_owned(),
    }
}

fn item_text(item: &Value) -> String {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    match item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .or_else(|| part.get("text").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => s(item, "message"),
    }
}

fn transcript_label(item: &TranscriptItem) -> (String, String) {
    if let Some(label) = &item.diagnostic {
        return (safe_text(label), String::new());
    }
    let kind = normalize_item_type(&s(&item.item, "type"));
    let state = match s(&item.item, "status").as_str() {
        "in_progress" => "running",
        "completed" => "done",
        "failed" => "failed",
        "declined" => "declined",
        _ => item.state.as_str(),
    }
    .to_owned();
    let summary = match kind.as_str() {
        "agent_message" => s(&item.item, "text"),
        "user_message" => format!(
            "You: {}",
            truncate(&item_text(&item.item), EVENT_PROMPT_LIMIT)
        ),
        "reasoning" => "Reasoning summary".into(),
        "command_execution" => {
            let command = s(&item.item, "command");
            let exit = s(&item.item, "exit_code");
            if exit.is_empty() {
                format!("{command} · running")
            } else {
                format!("{command} · exit {exit}")
            }
        }
        "diff" => "Changes".into(),
        "file_change" => {
            let n = item
                .item
                .get("changes")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("{n} file change(s)")
        }
        "mcp_tool_call" => format!("{}.{}", s(&item.item, "server"), s(&item.item, "tool")),
        "collab_tool_call" => s(&item.item, "tool"),
        "web_search" => format!("Search: {}", s(&item.item, "query")),
        "todo_list" => "Plan updated".into(),
        "error" => s(&item.item, "message"),
        _ => format!(
            "{}: {}",
            if kind.is_empty() { "Item" } else { &kind },
            truncate(&item.item.to_string(), EVENT_PROMPT_LIMIT)
        ),
    };
    (safe_text(&summary), state)
}

fn transcript_detail(item: &TranscriptItem) -> String {
    if item.item.is_null()
        && (item.raw.get("method").is_some()
            || item
                .raw
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| !kind.starts_with("item.")))
    {
        return serde_json::to_string_pretty(&item.raw).unwrap_or_else(|_| item.raw.to_string());
    }
    if let Some(label) = &item.diagnostic {
        return safe_text(label);
    }
    match normalize_item_type(&s(&item.item, "type")).as_str() {
        "command_execution" => {
            let output = s(&item.item, "aggregated_output");
            if output.is_empty() {
                "(no captured output)".into()
            } else {
                output
            }
        }
        "agent_message" | "reasoning" => s(&item.item, "text"),
        "user_message" => item_text(&item.item),
        "diff" => s(&item.item, "diff"),
        "file_change" => item
            .item
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .map(|change| {
                        let path = s(change, "path");
                        let diff = s(change, "diff");
                        if diff.is_empty() {
                            format!("--- {path} (diff unavailable)")
                        } else {
                            format!("--- {path}\n{diff}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "mcp_tool_call" => ["arguments", "result", "error"]
            .iter()
            .filter_map(|key| item.item.get(key).map(|value| format!("{key}: {}", value)))
            .collect::<Vec<_>>()
            .join("\n"),
        "collab_tool_call" => s(&item.item, "prompt"),
        _ => serde_json::to_string_pretty(&item.item).unwrap_or_else(|_| item.item.to_string()),
    }
}

fn transcript_indices(items: &[TranscriptItem], raw_view: bool) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (!item.hidden || raw_view).then_some(index))
        .collect()
}

fn move_transcript_selection(
    items: &[TranscriptItem],
    selected: usize,
    down: bool,
    raw_view: bool,
) -> usize {
    let indices = transcript_indices(items, raw_view);
    if indices.is_empty() {
        return 0;
    }
    let position = indices
        .iter()
        .position(|index| *index == selected)
        .unwrap_or_else(|| {
            if down {
                indices
                    .iter()
                    .position(|index| *index > selected)
                    .unwrap_or(indices.len() - 1)
            } else {
                indices
                    .iter()
                    .rposition(|index| *index < selected)
                    .unwrap_or(0)
            }
        });
    indices[if down {
        (position + 1).min(indices.len() - 1)
    } else {
        position.saturating_sub(1)
    }]
}

fn visible_transcript_selection(items: &[TranscriptItem], selected: usize) -> usize {
    let indices = transcript_indices(items, false);
    indices
        .iter()
        .copied()
        .find(|index| *index >= selected)
        .or_else(|| indices.last().copied())
        .unwrap_or(0)
}

fn scroll_by(current: u16, delta: i16, maximum: u16) -> u16 {
    (i32::from(current) + i32::from(delta)).clamp(0, i32::from(maximum)) as u16
}

fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '�'
            } else {
                c
            }
        })
        .collect()
}

fn align_transcript_offset(path: &Path, offset: u64) -> Option<u64> {
    if offset == 0 {
        return Some(0);
    }
    let mut file = std::fs::File::open(path).ok()?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset.saturating_sub(1))).ok()?;
    let mut previous = [0u8; 1];
    file.read_exact(&mut previous).ok()?;
    if previous[0] == b'\n' {
        Some(offset)
    } else {
        let mut byte = [0u8; 1];
        while file.read(&mut byte).ok()? == 1 {
            if byte[0] == b'\n' {
                return file.stream_position().ok();
            }
        }
        Some(file.stream_position().ok()?)
    }
}

fn read_transcript_window(
    path: &Path,
    offset: u64,
) -> (Vec<TranscriptItem>, Option<String>, u64, bool) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return (
            Vec::new(),
            Some("Transcript unavailable locally".into()),
            offset,
            false,
        );
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return (
            Vec::new(),
            Some("Transcript unavailable locally".into()),
            offset,
            false,
        );
    };
    let Some(start) = align_transcript_offset(path, offset.min(size)) else {
        return (
            Vec::new(),
            Some("Transcript unavailable locally".into()),
            offset,
            false,
        );
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (
            Vec::new(),
            Some("Transcript unavailable locally".into()),
            start,
            false,
        );
    }
    // Read no more than one window plus one byte; only parse complete JSONL records.
    let mut bytes = vec![0; TRANSCRIPT_WINDOW_BYTES + 1];
    let count = file.read(&mut bytes).unwrap_or(0);
    bytes.truncate(count);
    let has_more = start.saturating_add(count as u64) < size;
    if has_more {
        if let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') {
            bytes.truncate(end + 1);
        } else {
            return (
                Vec::new(),
                Some("Transcript contains a record larger than the display window".into()),
                start,
                false,
            );
        }
    }
    let next = start.saturating_add(bytes.len() as u64);
    let text = String::from_utf8_lossy(&bytes);
    (parse_codex_transcript(&text), None, next, has_more)
}

fn read_codex_transcript(
    client: &Client,
    agent_id: &str,
    offset: u64,
) -> (Vec<TranscriptItem>, Option<String>, u64, bool) {
    let Some(path) = codex_transcript_path(agent_id) else {
        return read_remote_transcript(client, agent_id, offset);
    };
    if path.exists() {
        read_transcript_window(&path, offset)
    } else {
        read_remote_transcript(client, agent_id, offset)
    }
}

fn read_remote_transcript(
    client: &Client,
    agent_id: &str,
    offset: u64,
) -> (Vec<TranscriptItem>, Option<String>, u64, bool) {
    // The relay's `after` cursor can return every remaining byte, so keep the
    // fallback to a bounded tail until it offers range reads.
    let _ = offset;
    if let Ok(transcript) = client.get(
        &format!("/v1/agents/{agent_id}/transcript"),
        &[("tail", TRANSCRIPT_WINDOW_BYTES.to_string())],
    ) && let Some(text) = transcript.get("stdout").and_then(Value::as_str)
    {
        return (parse_codex_transcript(text), None, 0, false);
    }
    (
        Vec::new(),
        Some("Transcript unavailable locally and from the relay".into()),
        offset,
        false,
    )
}

struct TranscriptCache {
    agent_id: String,
    local_signature: Option<(u64, Option<SystemTime>)>,
    checked_at: Instant,
    items: Arc<Vec<TranscriptItem>>,
    offset: u64,
    next_offset: u64,
    has_more: bool,
    notice: Option<String>,
}

fn read_codex_transcript_cached(
    client: &Client,
    agent_id: &str,
    cache: &mut Option<TranscriptCache>,
    offset: u64,
) -> (Arc<Vec<TranscriptItem>>, Option<String>, u64, bool) {
    let local_signature = codex_transcript_path(agent_id)
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| (metadata.len(), metadata.modified().ok()));
    if let Some(cached) = cache.as_ref()
        && cached.agent_id == agent_id
        && cached.offset == offset
        && ((local_signature.is_some() && cached.local_signature == local_signature)
            || cached.checked_at.elapsed() < Duration::from_secs(1))
    {
        return (
            Arc::clone(&cached.items),
            cached.notice.clone(),
            cached.next_offset,
            cached.has_more,
        );
    }
    let (items, notice, next_offset, has_more) = read_codex_transcript(client, agent_id, offset);
    let items = Arc::new(items);
    *cache = Some(TranscriptCache {
        agent_id: agent_id.to_owned(),
        local_signature,
        checked_at: Instant::now(),
        items: Arc::clone(&items),
        offset,
        next_offset,
        has_more,
        notice: notice.clone(),
    });
    (items, notice, next_offset, has_more)
}

fn read_agent_prompt(client: &Client, agent_id: &str) -> Option<String> {
    if agent_id.is_empty() {
        return None;
    }
    client
        .get(
            &format!("/v1/agents/{agent_id}/transcript"),
            &[("tail", "1".into())],
        )
        .ok()?
        .get("prompt")
        .and_then(Value::as_str)
        .map(safe_text)
}

pub(super) struct StatusOptions<'a> {
    pub(super) once: bool,
    pub(super) all: bool,
    pub(super) live_only: bool,
    pub(super) state_filter: Option<&'a str>,
    pub(super) interval: u64,
    pub(super) task_root: Option<&'a str>,
    pub(super) state_override: Option<&'a str>,
}

pub(super) fn cmd_status(c: &Client, options: StatusOptions<'_>) -> Result<(), Fail> {
    let StatusOptions {
        once,
        all,
        live_only,
        state_filter,
        interval,
        task_root,
        state_override,
    } = options;
    if let Some(state) = state_filter {
        validate_state(state)?;
    }
    let state = state_override
        .map(PathBuf::from)
        .unwrap_or_else(default_state);
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let root = task_root
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("CODEX_DEPT_REMOTE_DIR")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| home.join(".codex/dept"));
    let (rows, w, lost) = snapshot(c, &state, all)?;
    let rows = filter_rows(rows, live_only, state_filter);
    if once {
        print_once(&rows, &w, &root, lost);
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(Fail::Config(
            "interactive status requires a terminal; use `zzapi status --once` for a snapshot"
                .into(),
        ));
    }
    let mut terminal = TerminalSession::enter()
        .map_err(|e| Fail::Config(format!("cannot enter terminal mode: {e}")))?;
    let (mut rows, mut warnings, mut lost) = (rows, w, lost);
    let mut selected = 0usize;
    let mut selected_id = rows.get(selected).map(|row| row.id.clone());
    let mut table_state = ratatui::widgets::TableState::default();
    let mut transcript_cache = None;
    let mut transcript_offset = 0u64;
    let (mut transcript_items, mut transcript_notice, mut transcript_next, mut transcript_more) =
        rows.get(selected)
            .map(|row| {
                read_codex_transcript_cached(
                    c,
                    &row.agent,
                    &mut transcript_cache,
                    transcript_offset,
                )
            })
            .unwrap_or_default();
    let mut transcript_prompt = rows
        .get(selected)
        .and_then(|row| read_agent_prompt(c, &row.agent));
    let (mut transcript_selected, mut transcript_scroll, mut transcript_focus, mut raw_view) =
        (0usize, 0u16, false, false);
    transcript_selected = visible_transcript_selection(&transcript_items, transcript_selected);
    let mut expanded = HashSet::<String>::new();
    let mut tick = Instant::now();
    let mut transcript_tick = Instant::now();
    let mut max_transcript_scroll = 0u16;
    loop {
        draw(
            &mut terminal,
            &mut table_state,
            &rows,
            &warnings,
            selected,
            &transcript_items,
            transcript_notice.as_deref(),
            transcript_prompt.as_deref(),
            transcript_selected,
            &mut transcript_scroll,
            &mut max_transcript_scroll,
            transcript_focus,
            raw_view,
            &expanded,
            lost,
        );
        if event::poll(Duration::from_millis(33)).unwrap_or(false) {
            let input = match event::read() {
                Ok(Event::Key(key))
                    if key.kind != KeyEventKind::Release
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c') =>
                {
                    Some(Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char('q'),
                        KeyModifiers::CONTROL,
                    )))
                }
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => Some(Event::Key(key)),
                Ok(Event::Mouse(mouse)) => Some(Event::Mouse(mouse)),
                _ => None,
            };
            match input {
                Some(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        transcript_scroll = scroll_by(transcript_scroll, 3, max_transcript_scroll)
                    }
                    MouseEventKind::ScrollUp => {
                        transcript_scroll = scroll_by(transcript_scroll, -3, max_transcript_scroll)
                    }
                    _ => {}
                },
                Some(Event::Key(key)) => match key.code {
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => break,
                    KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                        transcript_focus = !transcript_focus;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if transcript_focus {
                            transcript_selected = move_transcript_selection(
                                &transcript_items,
                                transcript_selected,
                                true,
                                raw_view,
                            );
                            transcript_scroll = transcript_scroll.saturating_add(1);
                        } else {
                            selected = (selected + 1).min(rows.len().saturating_sub(1));
                            selected_id = rows.get(selected).map(|row| row.id.clone());
                            transcript_offset = 0;
                            (
                                transcript_items,
                                transcript_notice,
                                transcript_next,
                                transcript_more,
                            ) = rows
                                .get(selected)
                                .map(|row| {
                                    read_codex_transcript_cached(
                                        c,
                                        &row.agent,
                                        &mut transcript_cache,
                                        transcript_offset,
                                    )
                                })
                                .unwrap_or_default();
                            transcript_prompt = rows
                                .get(selected)
                                .and_then(|row| read_agent_prompt(c, &row.agent));
                            transcript_selected =
                                visible_transcript_selection(&transcript_items, 0);
                            transcript_scroll = 0;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if transcript_focus {
                            transcript_selected = move_transcript_selection(
                                &transcript_items,
                                transcript_selected,
                                false,
                                raw_view,
                            );
                            transcript_scroll = transcript_scroll.saturating_sub(1);
                        } else {
                            selected = selected.saturating_sub(1);
                            selected_id = rows.get(selected).map(|row| row.id.clone());
                            transcript_offset = 0;
                            (
                                transcript_items,
                                transcript_notice,
                                transcript_next,
                                transcript_more,
                            ) = rows
                                .get(selected)
                                .map(|row| {
                                    read_codex_transcript_cached(
                                        c,
                                        &row.agent,
                                        &mut transcript_cache,
                                        transcript_offset,
                                    )
                                })
                                .unwrap_or_default();
                            transcript_prompt = rows
                                .get(selected)
                                .and_then(|row| read_agent_prompt(c, &row.agent));
                            transcript_selected =
                                visible_transcript_selection(&transcript_items, 0);
                            transcript_scroll = 0;
                        }
                    }
                    KeyCode::PageDown | KeyCode::Char(']') => {
                        if transcript_scroll >= max_transcript_scroll && transcript_more {
                            transcript_offset = transcript_next;
                            if let Some(row) = rows.get(selected) {
                                (
                                    transcript_items,
                                    transcript_notice,
                                    transcript_next,
                                    transcript_more,
                                ) = read_codex_transcript_cached(
                                    c,
                                    &row.agent,
                                    &mut transcript_cache,
                                    transcript_offset,
                                );
                                transcript_selected =
                                    visible_transcript_selection(&transcript_items, 0);
                                transcript_scroll = 0;
                            }
                        } else {
                            transcript_scroll = transcript_scroll
                                .saturating_add(10)
                                .min(max_transcript_scroll);
                        }
                    }
                    KeyCode::PageUp | KeyCode::Char('[') => {
                        if transcript_scroll == 0 && transcript_offset > 0 {
                            let previous =
                                transcript_offset.saturating_sub(TRANSCRIPT_WINDOW_BYTES as u64);
                            transcript_offset = rows
                                .get(selected)
                                .and_then(|row| codex_transcript_path(&row.agent))
                                .and_then(|path| align_transcript_offset(&path, previous))
                                .unwrap_or(previous);
                            if let Some(row) = rows.get(selected) {
                                (
                                    transcript_items,
                                    transcript_notice,
                                    transcript_next,
                                    transcript_more,
                                ) = read_codex_transcript_cached(
                                    c,
                                    &row.agent,
                                    &mut transcript_cache,
                                    transcript_offset,
                                );
                                transcript_selected =
                                    visible_transcript_selection(&transcript_items, 0);
                                transcript_scroll = max_transcript_scroll;
                            }
                        } else {
                            transcript_scroll = transcript_scroll.saturating_sub(10);
                        }
                    }
                    KeyCode::Char('v') => {
                        raw_view = !raw_view;
                        if !raw_view {
                            transcript_selected = visible_transcript_selection(
                                &transcript_items,
                                transcript_selected,
                            );
                        }
                    }
                    KeyCode::Enter | KeyCode::Char(' ') if transcript_focus => {
                        if let Some(item) = transcript_items.get(transcript_selected)
                            && (raw_view || !item.hidden)
                            && !expanded.insert(item.id.clone())
                        {
                            expanded.remove(&item.id);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if tick.elapsed() >= Duration::from_secs(interval.max(1)) {
            let previous_agent = rows.get(selected).map(|row| row.agent.clone());
            match snapshot(c, &state, all) {
                Ok((r, w, lo)) => {
                    rows = filter_rows(r, live_only, state_filter);
                    warnings = w;
                    lost = lo;
                    selected = resolve_selection(&rows, selected_id.as_deref(), selected);
                    selected_id = rows.get(selected).map(|row| row.id.clone());
                    let current_agent = rows.get(selected).map(|row| row.agent.clone());
                    if current_agent != previous_agent {
                        transcript_offset = 0;
                        (
                            transcript_items,
                            transcript_notice,
                            transcript_next,
                            transcript_more,
                        ) = rows
                            .get(selected)
                            .map(|row| {
                                read_codex_transcript_cached(
                                    c,
                                    &row.agent,
                                    &mut transcript_cache,
                                    transcript_offset,
                                )
                            })
                            .unwrap_or_default();
                        transcript_prompt = rows
                            .get(selected)
                            .and_then(|row| read_agent_prompt(c, &row.agent));
                        transcript_scroll = 0;
                    }
                    transcript_selected = if raw_view {
                        transcript_selected.min(transcript_items.len().saturating_sub(1))
                    } else {
                        visible_transcript_selection(&transcript_items, transcript_selected)
                    };
                }
                Err(e) => warnings = vec![format!("relay status unavailable: {}", e.message)],
            }
            transcript_tick = Instant::now();
            tick = Instant::now()
        }
        // Refresh the local JSONL independently from the slower relay snapshot.
        // Codex appends transcript records while the agent is running, so tying
        // this read to the status interval makes the transcript feel stale.
        if transcript_tick.elapsed() >= Duration::from_millis(500) {
            if let Some(row) = rows.get(selected) {
                (
                    transcript_items,
                    transcript_notice,
                    transcript_next,
                    transcript_more,
                ) = read_codex_transcript_cached(
                    c,
                    &row.agent,
                    &mut transcript_cache,
                    transcript_offset,
                );
                transcript_selected = if raw_view {
                    transcript_selected.min(transcript_items.len().saturating_sub(1))
                } else {
                    visible_transcript_selection(&transcript_items, transcript_selected)
                };
            }
            transcript_tick = Instant::now();
        }
    }
    Ok(())
}
struct TerminalSession {
    terminal: RatatuiTerminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            crossterm::cursor::Hide
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match RatatuiTerminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(
                    io::stdout(),
                    crossterm::cursor::Show,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            crossterm::cursor::Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    terminal: &mut TerminalSession,
    table_state: &mut ratatui::widgets::TableState,
    rows: &[Execution],
    warnings: &[String],
    selected: usize,
    transcript_items: &[TranscriptItem],
    transcript_notice: Option<&str>,
    transcript_prompt: Option<&str>,
    transcript_selected: usize,
    transcript_scroll: &mut u16,
    max_transcript_scroll: &mut u16,
    transcript_focus: bool,
    raw_view: bool,
    expanded: &HashSet<String>,
    lost: bool,
) {
    let _ = terminal.terminal.draw(|frame| {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(5)])
            .split(frame.area());
        frame.render_widget(
            Paragraph::new("zzapi status — ↑↓ select pane  Enter expand  v raw JSON  Tab switch  PgUp/PgDn scroll  q quit")
                .style(Style::default().add_modifier(Modifier::BOLD)),
            areas[0],
        );
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(areas[1]);
        let table_rows = rows.iter().map(|e| {
            Row::new(vec![
                Cell::from(e.task.as_str()),
                Cell::from(if e.pr.is_empty() && e.branch.is_empty() {
                    "-".to_owned()
                } else if e.pr.is_empty() {
                    e.branch.clone()
                } else if e.branch.is_empty() {
                    e.pr.clone()
                } else {
                    format!("{} {}", e.pr, e.branch)
                }),
                Cell::from(if e.state.is_empty() { "unknown" } else { &e.state }),
            ])
        });
        let header = Row::new(["AGENT / TASK", "PR / BRANCH", "STATE"]).style(
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        );
        let table = Table::new(
            table_rows,
            [Constraint::Percentage(48), Constraint::Percentage(33), Constraint::Percentage(19)],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Agents "))
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("» ");
        table_state.select((!rows.is_empty()).then_some(selected));
        frame.render_stateful_widget(table, panes[0], table_state);

        let mut lines = Vec::new();
        if let Some(e) = rows.get(selected) {
            let path = codex_transcript_path(&e.agent)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "local JSONL transcript unavailable".into());
            let worktree = if e.worktree.is_empty() {
                "-"
            } else {
                &e.worktree
            };
            lines.push(Line::from(format!("Worktree: {worktree}")));
            lines.push(Line::from(format!(
                "Branch: {}",
                if e.branch.is_empty() { "-" } else { &e.branch }
            )));
            lines.push(Line::from(format!(
                "PR: {}",
                if e.pr.is_empty() { "-" } else { &e.pr }
            )));
            lines.push(Line::from(format!(
                "Agent ID: {}",
                if e.agent.is_empty() { "-" } else { &e.agent }
            )));
            lines.push(Line::from(format!(
                "{} / {} · {} · {}",
                e.task,
                e.id,
                e.state,
                e.flags(lost)
            )));
            lines.push(Line::from(format!("Source: {}", tail_path(&path, panes[1].width.saturating_sub(12) as usize))));
            if let Some(summary) = e.events.iter().rev().find_map(request_summary) {
                lines.push(Line::from(format!("Request: {summary}")));
            }
            if let Some(notice) = transcript_notice {
                lines.push(Line::from(Span::styled(safe_text(notice), Style::default().fg(Color::Yellow))));
            }
            if let Some(prompt) = transcript_prompt {
                lines.push(Line::from(Span::styled(
                    "YOU",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                )));
                lines.extend(prompt.lines().map(|line| Line::from(line.to_owned())));
            }
            for (index, item) in transcript_items.iter().enumerate() {
                if item.hidden && !raw_view {
                    continue;
                }
                let (mut summary, state) = transcript_label(item);
                if raw_view && item.hidden {
                    summary = format!("Protocol event: {}", item.state);
                }
                summary = safe_text(&summary);
                let state = safe_text(&state);
                let kind = normalize_item_type(&s(&item.item, "type"));
                let marker = if kind.ends_with("tool_call") || kind == "command_execution" { "▸" } else if kind == "diff" || kind == "file_change" { "±" } else { "•" };
                let selected_style = if index == transcript_selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else { Style::default() };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} "), selected_style),
                    Span::styled(format!("[{state}] "), Style::default().fg(Color::Yellow)),
                    Span::styled(summary, selected_style),
                ]));
                if raw_view || expanded.contains(&item.id) {
                    let detail = if raw_view {
                        serde_json::to_string_pretty(&item.raw).unwrap_or_else(|_| item.raw.to_string())
                    } else {
                        transcript_detail(item)
                    };
                    let diff_mode = kind == "diff" || kind == "file_change";
                    lines.extend(safe_text(&detail).lines().map(|line| {
                        if diff_mode && line.starts_with('+') {
                            Line::from(Span::styled(line.to_owned(), Style::default().fg(Color::Green)))
                        } else if diff_mode && line.starts_with('-') {
                            Line::from(Span::styled(line.to_owned(), Style::default().fg(Color::Red)))
                        } else if diff_mode && line.starts_with("@@") {
                            Line::from(Span::styled(line.to_owned(), Style::default().fg(Color::Cyan)))
                        } else {
                            Line::from(line.to_owned())
                        }
                    }));
                }
            }
            if transcript_items.iter().all(|item| item.hidden) && transcript_notice.is_none() && !raw_view {
                lines.push(Line::from("No transcript messages yet."));
            } else if transcript_items.is_empty() && transcript_notice.is_none() {
                lines.push(Line::from("No Codex events yet."));
            }
        } else {
            lines.push(Line::from("No agent executions observed."));
        }
        for warning in warnings {
            lines.push(Line::from(Span::styled(format!("WARNING: {warning}"), Style::default().fg(Color::Red))));
        }
        let title = if transcript_focus { " Transcript · focused " } else { " Transcript " };
        let inner_width = panes[1].width.saturating_sub(2).max(1) as usize;
        let content_lines = lines
            .iter()
            .map(|line| {
                let width = line
                    .spans
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>();
                width.max(1).div_ceil(inner_width)
            })
            .sum::<usize>();
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: true });
        let content_height = panes[1].height.saturating_sub(2) as usize;
        *max_transcript_scroll = content_lines.saturating_sub(content_height).min(u16::MAX as usize) as u16;
        *transcript_scroll = (*transcript_scroll).min(*max_transcript_scroll);
        let paragraph = paragraph.scroll((*transcript_scroll, 0));
        frame.render_widget(paragraph, panes[1]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_fixture() -> Vec<Execution> {
        vec![
            Execution {
                id: "live-running".into(),
                state: "running".into(),
                agent: "agent-1".into(),
                ..Default::default()
            },
            Execution {
                id: "live-succeeded".into(),
                state: "succeeded".into(),
                agent: "agent-2".into(),
                ..Default::default()
            },
            Execution {
                id: "historical-failed".into(),
                state: "failed".into(),
                ..Default::default()
            },
            Execution {
                id: "historical-unobserved".into(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn live_only_keeps_rows_with_registered_agent_ids() {
        let rows = filter_rows(filter_fixture(), true, None);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["live-running", "live-succeeded"]
        );
    }

    #[test]
    fn state_filter_matches_displayed_state_including_not_observed() {
        let rows = filter_rows(filter_fixture(), false, Some("not observed"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "historical-unobserved");

        let rows = filter_rows(filter_fixture(), false, Some("failed"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "historical-failed");
    }

    #[test]
    fn live_only_and_state_filters_compose_with_and() {
        let rows = filter_rows(filter_fixture(), true, Some("succeeded"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "live-succeeded");

        let rows = filter_rows(filter_fixture(), true, Some("failed"));
        assert!(rows.is_empty());
    }

    #[test]
    fn no_status_filters_preserve_all_rows_in_order() {
        let original = filter_fixture();
        let filtered = filter_rows(original.clone(), false, None);
        assert_eq!(filtered.len(), original.len());
        assert_eq!(
            filtered
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn transcript_window_pages_a_file_larger_than_sixteen_mib() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "zzapi-transcript-window-{}.jsonl",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        let payload = "x".repeat(10_000);
        for index in 0..1_900 {
            writeln!(file, "{{\"type\":\"item.completed\",\"item\":{{\"id\":\"item-{index}\",\"type\":\"agentMessage\",\"text\":\"{payload}\"}}}}").unwrap();
        }
        drop(file);
        assert!(std::fs::metadata(&path).unwrap().len() > 16 * 1024 * 1024);

        let (first, notice, next, more) = read_transcript_window(&path, 0);
        assert!(notice.is_none());
        assert!(more);
        assert!(next < std::fs::metadata(&path).unwrap().len());
        assert!(
            first.len() < 100,
            "a page should contain only a bounded portion of the transcript"
        );
        let (second, _, second_next, second_more) = read_transcript_window(&path, next);
        assert!(!second.is_empty());
        assert!(second_next > next);
        assert!(second_more);
        assert_ne!(first[0].id, second[0].id);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn transcript_window_alignment_starts_at_a_record_boundary() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "zzapi-transcript-align-{}.jsonl",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "{{\"type\":\"thread.started\"}}").unwrap();
        writeln!(file, "{{\"type\":\"turn.started\"}}").unwrap();
        drop(file);
        let offset = align_transcript_offset(&path, 4).unwrap();
        let (items, _, _, _) = read_transcript_window(&path, offset);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, "turn.started");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_unknown_status_states_with_one_line_error() {
        let error = validate_state("mysterious").unwrap_err();
        let Fail::Config(message) = error else {
            panic!("expected configuration error");
        };
        assert!(!message.contains('\n'));
        assert!(message.starts_with("unknown status state 'mysterious'"));
    }

    #[test]
    fn preserves_selected_execution_when_new_rows_are_inserted_before_it() {
        let rows = vec![
            Execution {
                id: "new-agent".into(),
                ..Default::default()
            },
            Execution {
                id: "selected-agent".into(),
                ..Default::default()
            },
            Execution {
                id: "older-agent".into(),
                ..Default::default()
            },
        ];

        assert_eq!(resolve_selection(&rows, Some("selected-agent"), 1), 1);
        assert_eq!(resolve_selection(&rows, Some("selected-agent"), 0), 1);
        assert_eq!(resolve_selection(&rows, Some("removed-agent"), 2), 2);
    }

    #[test]
    fn parses_rfc3339_and_unix_started_at_values() {
        assert_eq!(
            timestamp(Some(&serde_json::json!("2026-10-08T00:00:00Z"))),
            Some(1_791_417_600)
        );
        assert_eq!(
            timestamp(Some(&serde_json::json!(1_791_417_600))),
            Some(1_791_417_600)
        );
    }

    #[test]
    fn computes_elapsed_only_for_matching_clocks() {
        let a = serde_json::json!({"occurred_at":"2026-10-08T00:00:00Z","clock":"host-a"});
        let b = serde_json::json!({"occurred_at":"2026-10-08T00:00:12Z","clock":"host-a"});
        let c = serde_json::json!({"occurred_at":"2026-10-08T00:00:12Z","clock":"host-b"});
        assert_eq!(elapsed(&a, &b), Some(12));
        assert_eq!(elapsed(&a, &c), None);
    }

    #[test]
    fn combines_events_with_agent_state_and_worktree() {
        let events = vec![
            serde_json::json!({"id":"e1","task_id":"agent-1","execution_id":"run-1","kind":"process_spawned","occurred_at":"2026-10-08T00:00:00Z","clock":"host"}),
        ];
        let agents = vec![
            serde_json::json!({"id":"agent-handle","task_id":"agent-1","execution_id":"run-1","state":"orphaned","command":"codex exec","started_at":"2026-10-08T00:00:00Z","exit_code":1,"audit_degraded":true,"agent_config":"{\"pr\":123,\"branch\":\"codex/fix-thing\"}"}),
        ];
        let wt = HashMap::from([("agent-1".to_owned(), "/tmp/agent-1".to_owned())]);
        let rows = build(events, &agents, &wt, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].phase(), "process_spawned");
        assert_eq!(rows[0].state, "orphaned");
        assert_eq!(rows[0].agent, "agent-handle");
        assert_eq!(rows[0].command, "codex exec");
        assert_eq!(rows[0].started_at, "2026-10-08T00:00:00Z");
        assert_eq!(rows[0].exit_code, "1");
        assert_eq!(rows[0].worktree, "/tmp/agent-1");
        assert_eq!(rows[0].pr, "#123");
        assert_eq!(rows[0].branch, "codex/fix-thing");
        assert!(rows[0].flags(false).contains("audit degraded"));
    }

    #[test]
    fn request_summaries_are_compact_and_support_prompt_or_process() {
        let prompt = serde_json::json!({
            "kind":"relay_request_started",
            "payload":{"prompt":"  review   this change  ","model":"gpt-6"}
        });
        assert_eq!(
            request_summary(&prompt).as_deref(),
            Some("prompt: review this change · model: gpt-6")
        );
        let process = serde_json::json!({
            "kind":"relay_request_started",
            "payload":{"process":"codex","command":["run","--fast"]}
        });
        assert_eq!(
            request_summary(&process).as_deref(),
            Some("process: codex · command: run --fast")
        );
    }

    #[test]
    fn path_tail_truncation_uses_terminal_display_width() {
        assert_eq!(tail_path("/repo/worktree/task-123", 12), "…ee/task-123");
        assert_eq!(tail_path("/repo/task", 20), "/repo/task");
        assert_eq!(tail_path("/repo/task", 1), "…");
    }

    #[test]
    fn mouse_scroll_moves_in_small_bounded_steps() {
        let mut scroll = 12;
        scroll = scroll_by(scroll, 3, 20);
        assert_eq!(scroll, 15);
        for _ in 0..100 {
            scroll = scroll_by(scroll, 3, 20);
        }
        assert_eq!(scroll, 20);
        for _ in 0..100 {
            scroll = scroll_by(scroll, -3, 20);
        }
        assert_eq!(scroll, 0);
    }

    #[test]
    fn reads_branch_from_git_head_without_spawning_git() {
        let dir = std::env::temp_dir().join(format!(
            "zzapi-status-branch-{}-{}",
            std::process::id(),
            now()
        ));
        let normal = dir.join("normal");
        std::fs::create_dir_all(normal.join(".git")).unwrap();
        std::fs::write(normal.join(".git/HEAD"), "ref: refs/heads/codex/status\n").unwrap();
        assert_eq!(git_branch(normal.to_str().unwrap()), "codex/status");

        let linked = dir.join("linked");
        let git_dir = dir.join("common/worktrees/linked");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: ../common/worktrees/linked\n").unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/codex/linked\n").unwrap();
        assert_eq!(git_branch(linked.to_str().unwrap()), "codex/linked");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_transcript_item_renders_its_payload_as_a_fallback() {
        let items = parse_codex_transcript(
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"future-1\",\"type\":\"futureWidget\",\"value\":\"kept\"}}\n",
        );
        assert_eq!(items.len(), 1);
        assert!(transcript_label(&items[0]).0.contains("futureWidget"));
        assert!(transcript_label(&items[0]).0.contains("kept"));
        assert!(transcript_detail(&items[0]).contains("kept"));
    }

    #[test]
    fn unhandled_item_namespace_events_remain_visible_with_payloads() {
        let items = parse_codex_transcript(
            "{\"method\":\"item/fileChange/requestApproval\",\"params\":{\"itemId\":\"file-1\",\"reason\":\"outside workspace\"}}\n",
        );
        assert_eq!(items.len(), 1);
        assert!(!items[0].hidden);
        assert!(
            transcript_label(&items[0])
                .0
                .contains("item/fileChange/requestApproval")
        );
        assert!(transcript_detail(&items[0]).contains("outside workspace"));
    }

    #[test]
    fn reconciles_item_updates_and_keeps_malformed_records_visible() {
        let data = concat!(
            "{\"type\":\"item.started\",\"item\":{\"id\":\"cmd-1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"status\":\"in_progress\"}}\n",
            "{\"type\":\"item.updated\",\"item\":{\"id\":\"cmd-1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"aggregated_output\":\"ok\",\"status\":\"in_progress\"}}\n",
            "bad json\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"cmd-1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"exit_code\":0,\"status\":\"completed\"}}\n",
        );
        let items = parse_codex_transcript(data);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].state, "completed");
        assert_eq!(s(&items[0].item, "exit_code"), "0");
        assert!(
            items[1]
                .diagnostic
                .as_deref()
                .unwrap()
                .contains("Malformed")
        );
    }

    #[test]
    fn parses_codex_app_server_notification_methods() {
        let data = concat!(
            "{\"method\":\"item/started\",\"params\":{\"item\":{\"id\":\"msg-1\",\"type\":\"agent_message\",\"text\":\"working\",\"status\":\"in_progress\"}}}\n",
            "{\"method\":\"item/completed\",\"params\":{\"item\":{\"id\":\"msg-1\",\"type\":\"agent_message\",\"text\":\"done\",\"status\":\"completed\"}}}\n",
            "{\"method\":\"turn/completed\",\"params\":{\"turn\":{\"status\":\"completed\"}}}\n",
        );
        let items = parse_codex_transcript(data);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].state, "completed");
        assert_eq!(s(&items[0].item, "text"), "done");
        assert_eq!(items[1].state, "turn.completed");
        assert!(items[1].hidden);
    }

    #[test]
    fn renders_app_server_stream_as_clean_transcript_items() {
        let fixture = concat!(
            r#"{"method":"item/started","params":{"item":{"id":"msg-1","type":"agentMessage","text":"","status":"in_progress"}}}"#,
            "\n",
            r#"{"method":"item/completed","params":{"item":{"id":"user-1","type":"userMessage","content":[{"type":"text","text":"Please explain the change."}],"status":"completed"}}}"#,
            "\n",
            r#"{"method":"item/agentMessage/delta","params":{"itemId":"msg-1","delta":"The fix is "}}"#,
            "\n",
            r#"{"method":"item/agentMessage/delta","params":{"itemId":"msg-1","delta":"ready."}}"#,
            "\n",
            r#"{"method":"item/completed","params":{"item":{"id":"msg-1","type":"agentMessage","status":"completed"}}}"#,
            "\n",
            r#"{"method":"item/commandExecution/started","params":{"itemId":"cmd-1","command":"cargo test -p zzapi"}}"#,
            "\n",
            r#"{"method":"item/commandExecution/outputDelta","params":{"itemId":"cmd-1","delta":"all tests passed"}}"#,
            "\n",
            r#"{"method":"item/commandExecution/completed","params":{"itemId":"cmd-1","exitCode":0}}"#,
            "\n",
            r#"{"method":"turn/diff/updated","params":{"diff":"diff --git a/README.md b/README.md\n+clean transcript"}}"#,
            "\n",
            r#"{"method":"thread/tokenUsage/updated","params":{"total":{"tokens":42}}}"#,
            "\n",
            r#"{"method":"mcpServer/startupStatus/updated","params":{"name":"example","status":"ready"}}"#,
            "\n",
            r#"{"method":"item/started","params":{"itemId":"lost-1"}}"#,
            "\n",
            r#"{"method":"item/updated","params":{"itemId":"lost-2"}}"#,
            "\n",
            r#"{"method":"thread/status/changed","params":{"status":{"type":"idle"}}}"#,
            "\n",
            r#"{"method":"future/workerInsight","params":{"message":"kept concise"}}"#,
            "\n",
        );
        let items = parse_codex_transcript(fixture);
        let visible: Vec<_> = items.iter().filter(|item| !item.hidden).collect();
        assert_eq!(
            visible.len(),
            7,
            "visible: {:?}",
            visible
                .iter()
                .map(|item| (item.id.as_str(), item.hidden, transcript_label(item).0))
                .collect::<Vec<_>>()
        );

        assert_eq!(s(&visible[0].item, "text"), "The fix is ready.");
        assert_eq!(transcript_label(visible[0]).0, "The fix is ready.");
        let raw_events = visible[0].raw.as_array().unwrap();
        assert_eq!(raw_events.len(), 4);
        assert_eq!(raw_events[0]["method"], "item/started");
        assert_eq!(raw_events[1]["params"]["delta"], "The fix is ");
        assert_eq!(raw_events[2]["params"]["delta"], "ready.");
        assert_eq!(raw_events[3]["method"], "item/completed");

        assert_eq!(
            transcript_label(visible[1]).0,
            "You: Please explain the change."
        );
        assert_eq!(transcript_detail(visible[1]), "Please explain the change.");

        assert_eq!(
            transcript_label(visible[2]).0,
            "cargo test -p zzapi · exit 0"
        );
        assert_eq!(transcript_detail(visible[2]), "all tests passed");
        assert!(transcript_label(visible[3]).0.contains("Changes"));
        assert!(transcript_detail(visible[3]).contains("+clean transcript"));
        assert_eq!(
            visible[4].diagnostic.as_deref(),
            Some("Transcript gap: one or more events referenced an item that was not captured")
        );
        assert!(
            transcript_label(visible[5])
                .0
                .contains("Thread status: idle")
        );
        assert!(
            transcript_label(visible[6])
                .0
                .contains("Event: future/workerInsight")
        );
        assert!(transcript_label(visible[6]).0.contains("kept concise"));

        let hidden_protocol_events = items.iter().filter(|item| item.hidden).count();
        assert_eq!(hidden_protocol_events, 2);
        assert!(
            visible
                .iter()
                .all(|item| !transcript_label(item).0.contains("Unknown event:"))
        );
    }

    #[test]
    fn flags_orphan_message_deltas_and_unknown_events_in_known_namespaces() {
        let data = concat!(
            r#"{"method":"item/agentMessage/delta","params":{"itemId":"missing","delta":"orphan"}}"#,
            "\n",
            r#"{"method":"turn/futureUpdate","params":{}}"#,
            "\n",
            r#"{"method":"mcpServer/futureStatus","params":{}}"#,
            "\n",
        );
        let items = parse_codex_transcript(data);
        assert_eq!(items.len(), 3);
        assert!(
            transcript_label(&items[0])
                .0
                .contains("Event: turn/futureUpdate")
        );
        assert!(transcript_label(&items[0]).0.contains("{}"));
        assert!(
            transcript_label(&items[1])
                .0
                .contains("Event: mcpServer/futureStatus")
        );
        assert_eq!(
            transcript_label(&items[2]).0,
            "Transcript delta references missing item missing after reading the full transcript"
        );
        assert!(!items[0].hidden);
        assert!(!items[1].hidden);
    }

    #[test]
    fn reconciles_message_deltas_that_precede_their_item() {
        let items = parse_codex_transcript(concat!(
            "{\"method\":\"item/agentMessage/delta\",\"params\":{\"itemId\":\"msg-1\",\"delta\":\"hello \"}}\n",
            "{\"method\":\"item/agentMessage/delta\",\"params\":{\"itemId\":\"msg-1\",\"delta\":\"world\"}}\n",
            "{\"method\":\"item/started\",\"params\":{\"item\":{\"id\":\"msg-1\",\"type\":\"agentMessage\",\"text\":\"\"}}}\n",
        ));
        assert_eq!(items.len(), 1);
        assert_eq!(s(&items[0].item, "text"), "hello world");
        let raw = items[0].raw.as_array().unwrap();
        assert_eq!(raw.len(), 3);
        assert_eq!(raw[0]["params"]["delta"], "hello ");
        assert_eq!(raw[1]["params"]["delta"], "world");
        assert_eq!(raw[2]["method"], "item/started");
    }

    #[test]
    fn sanitizes_terminal_controls_in_event_names_and_error_messages() {
        let data = concat!(
            r#"{"method":"turn/\u001b]0;owned\u0007\u001b[31mfuture","params":{}}"#,
            "\n",
            r#"{"method":"error","message":"bad\u001b]0;title\u0007\u001b[2J"}"#,
            "\n",
        );
        let items = parse_codex_transcript(data);
        for item in &items {
            let summary = transcript_label(item).0;
            assert!(!summary.chars().any(|ch| ch == '\u{1b}' || ch == '\u{7}'));
        }
        assert!(
            transcript_label(&items[0])
                .0
                .contains("�]0;owned��[31mfuture")
        );
        assert!(transcript_label(&items[1]).0.contains("bad�]0;title��[2J"));
    }

    #[test]
    fn ignores_an_incomplete_final_jsonl_record() {
        let items = parse_codex_transcript(
            "{\"type\":\"turn.started\"}\n{\"type\":\"item.started\",\"item\":",
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, "turn.started");
    }

    #[test]
    fn rejects_invalid_agent_ids_for_transcript_paths() {
        assert!(codex_transcript_path("../etc/passwd").is_none());
        assert!(codex_transcript_path("g0000000000000000000000000000000").is_none());
    }
}
