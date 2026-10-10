//! Read-only agent status dashboard migrated from dept/status.py.
use super::{ApiError, Client, Fail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const INTERNAL: &str = "relay-update";
const EVENT_PROMPT_LIMIT: usize = 100;
const EVENT_LIMIT: usize = 10_000;
const CODEX_TRANSCRIPT_LIMIT: usize = 16 * 1024 * 1024;
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
    std::process::Command::new("git")
        .args(["-C", worktree, "branch", "--show-current"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
    for r in &mut out {
        if r.worktree.is_empty() {
            r.worktree = wt.get(&r.task).cloned().unwrap_or_default()
        }
        if r.branch.is_empty() {
            r.branch = git_branch(&r.worktree);
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
            });
            continue;
        };
        let kind = s(&event, "type");
        if kind.starts_with("item.") {
            let Some(item) = event.get("item") else {
                items.push(TranscriptItem {
                    id: format!("diagnostic-{line_number}"),
                    item: Value::Null,
                    state: "diagnostic".into(),
                    raw: event,
                    diagnostic: Some(format!(
                        "{kind} record has no item at line {}",
                        line_number + 1
                    )),
                });
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
                items[index] = TranscriptItem {
                    id: key,
                    item: item.clone(),
                    state,
                    raw: event,
                    diagnostic: None,
                };
            } else {
                indices.insert(key.clone(), items.len());
                items.push(TranscriptItem {
                    id: key,
                    item: item.clone(),
                    state,
                    raw: event,
                    diagnostic: None,
                });
            }
        } else {
            // Lifecycle records have no thread item, but remain useful context and
            // must be inspectable just like item records.
            let label = match kind.as_str() {
                "thread.started" => "Thread started".to_owned(),
                "turn.started" => "Turn started".to_owned(),
                "turn.completed" => "Turn completed".to_owned(),
                "turn.failed" => format!(
                    "Turn failed: {}",
                    s(event.get("error").unwrap_or(&Value::Null), "message")
                ),
                "error" => format!("Error: {}", s(&event, "message")),
                _ => format!("Event: {}", if kind.is_empty() { "unknown" } else { &kind }),
            };
            items.push(TranscriptItem {
                id: format!("event-{line_number}"),
                item: Value::Null,
                state: kind,
                raw: event,
                diagnostic: Some(label),
            });
        }
    }
    items
}

fn transcript_label(item: &TranscriptItem) -> (String, String) {
    if let Some(label) = &item.diagnostic {
        return (label.clone(), String::new());
    }
    let kind = s(&item.item, "type");
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
        "reasoning" => "Reasoning summary (expand to inspect)".into(),
        "command_execution" => s(&item.item, "command"),
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
            "{} {}",
            if kind.is_empty() {
                "Unknown item"
            } else {
                &kind
            },
            item.item
        ),
    };
    (safe_text(&summary), state)
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

fn read_codex_transcript(agent_id: &str) -> (Vec<TranscriptItem>, Option<String>) {
    let Some(path) = codex_transcript_path(agent_id) else {
        return (
            Vec::new(),
            Some("No local Codex JSONL transcript for this execution".into()),
        );
    };
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.len() as usize > CODEX_TRANSCRIPT_LIMIT => {
            return (
                Vec::new(),
                Some(format!(
                    "Transcript exceeds the {} MiB display limit",
                    CODEX_TRANSCRIPT_LIMIT / 1024 / 1024
                )),
            );
        }
        Err(error) => return (Vec::new(), Some(format!("Transcript unavailable: {error}"))),
        _ => {}
    }
    match std::fs::read_to_string(path) {
        Ok(text) => (parse_codex_transcript(&text), None),
        Err(error) => (Vec::new(), Some(format!("Transcript unavailable: {error}"))),
    }
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

pub(super) fn cmd_status(
    c: &Client,
    once: bool,
    all: bool,
    interval: u64,
    task_root: Option<&str>,
    state_override: Option<&str>,
) -> Result<(), Fail> {
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
    let (mut transcript_items, mut transcript_notice) = rows
        .get(selected)
        .map(|row| read_codex_transcript(&row.agent))
        .unwrap_or_default();
    let mut transcript_prompt = rows
        .get(selected)
        .and_then(|row| read_agent_prompt(c, &row.agent));
    let (mut transcript_selected, mut transcript_scroll, mut transcript_focus, mut raw_view) =
        (0usize, 0u16, false, false);
    let mut expanded = HashSet::<String>::new();
    let mut tick = Instant::now();
    let mut transcript_tick = Instant::now();
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
            transcript_scroll,
            transcript_focus,
            raw_view,
            &expanded,
            lost,
        );
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            let key = match event::read() {
                Ok(Event::Key(key))
                    if key.kind != KeyEventKind::Release
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c') =>
                {
                    Some(KeyCode::Char('q'))
                }
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => Some(key.code),
                _ => None,
            };
            match key {
                Some(KeyCode::Char('q' | 'Q')) | Some(KeyCode::Esc) => break,
                Some(KeyCode::Tab) | Some(KeyCode::Left) | Some(KeyCode::Right) => {
                    transcript_focus = !transcript_focus;
                }
                Some(KeyCode::Char('j')) | Some(KeyCode::Down) => {
                    if transcript_focus {
                        transcript_selected =
                            (transcript_selected + 1).min(transcript_items.len().saturating_sub(1));
                        transcript_scroll = transcript_scroll.saturating_add(1);
                    } else {
                        selected = (selected + 1).min(rows.len().saturating_sub(1));
                        selected_id = rows.get(selected).map(|row| row.id.clone());
                        (transcript_items, transcript_notice) = rows
                            .get(selected)
                            .map(|row| read_codex_transcript(&row.agent))
                            .unwrap_or_default();
                        transcript_prompt = rows
                            .get(selected)
                            .and_then(|row| read_agent_prompt(c, &row.agent));
                        transcript_selected = 0;
                        transcript_scroll = 0;
                    }
                }
                Some(KeyCode::Char('k')) | Some(KeyCode::Up) => {
                    if transcript_focus {
                        transcript_selected = transcript_selected.saturating_sub(1);
                        transcript_scroll = transcript_scroll.saturating_sub(1);
                    } else {
                        selected = selected.saturating_sub(1);
                        selected_id = rows.get(selected).map(|row| row.id.clone());
                        (transcript_items, transcript_notice) = rows
                            .get(selected)
                            .map(|row| read_codex_transcript(&row.agent))
                            .unwrap_or_default();
                        transcript_prompt = rows
                            .get(selected)
                            .and_then(|row| read_agent_prompt(c, &row.agent));
                        transcript_selected = 0;
                        transcript_scroll = 0;
                    }
                }
                Some(KeyCode::PageDown) | Some(KeyCode::Char(']')) => {
                    transcript_scroll = transcript_scroll.saturating_add(10)
                }
                Some(KeyCode::PageUp) | Some(KeyCode::Char('[')) => {
                    transcript_scroll = transcript_scroll.saturating_sub(10)
                }
                Some(KeyCode::Char('v')) => raw_view = !raw_view,
                Some(KeyCode::Enter) | Some(KeyCode::Char(' ')) if transcript_focus => {
                    if let Some(item) = transcript_items.get(transcript_selected)
                        && !item.diagnostic.as_ref().is_some_and(|text| {
                            text.starts_with("Thread started") || text.starts_with("Turn started")
                        })
                        && !expanded.insert(item.id.clone())
                    {
                        expanded.remove(&item.id);
                    }
                }
                _ => {}
            }
        }
        if tick.elapsed() >= Duration::from_secs(interval.max(1)) {
            match snapshot(c, &state, all) {
                Ok((r, w, lo)) => {
                    rows = r;
                    warnings = w;
                    lost = lo;
                    selected = resolve_selection(&rows, selected_id.as_deref(), selected);
                    selected_id = rows.get(selected).map(|row| row.id.clone());
                    (transcript_items, transcript_notice) = rows
                        .get(selected)
                        .map(|row| read_codex_transcript(&row.agent))
                        .unwrap_or_default();
                    transcript_prompt = rows
                        .get(selected)
                        .and_then(|row| read_agent_prompt(c, &row.agent));
                    transcript_selected =
                        transcript_selected.min(transcript_items.len().saturating_sub(1));
                }
                Err(e) => warnings = vec![format!("relay status unavailable: {}", e.message)],
            }
            tick = Instant::now()
        }
        // Refresh the local JSONL independently from the slower relay snapshot.
        // Codex appends transcript records while the agent is running, so tying
        // this read to the status interval makes the transcript feel stale.
        if transcript_tick.elapsed() >= Duration::from_millis(500) {
            if let Some(row) = rows.get(selected) {
                (transcript_items, transcript_notice) = read_codex_transcript(&row.agent);
                transcript_selected =
                    transcript_selected.min(transcript_items.len().saturating_sub(1));
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
        if let Err(error) = execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match RatatuiTerminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), crossterm::cursor::Show, LeaveAlternateScreen);
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
    transcript_scroll: u16,
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
                lines.push(Line::from(Span::styled(notice, Style::default().fg(Color::Yellow))));
            }
            if let Some(prompt) = transcript_prompt {
                lines.push(Line::from(Span::styled(
                    "YOU",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                )));
                lines.extend(prompt.lines().map(|line| Line::from(line.to_owned())));
            }
            for (index, item) in transcript_items.iter().enumerate() {
                let (summary, state) = transcript_label(item);
                let kind = s(&item.item, "type");
                let marker = if kind.ends_with("tool_call") || kind == "command_execution" { "▸" } else { "•" };
                let selected_style = if index == transcript_selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else { Style::default() };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} "), selected_style),
                    Span::styled(format!("[{state}] "), Style::default().fg(Color::Yellow)),
                    Span::styled(summary, selected_style),
                ]));
                if expanded.contains(&item.id) {
                    let detail = if raw_view || item.diagnostic.is_some() {
                        serde_json::to_string_pretty(&item.raw).unwrap_or_else(|_| item.raw.to_string())
                    } else {
                        let kind = s(&item.item, "type");
                        match kind.as_str() {
                            "command_execution" => {
                                let output = s(&item.item, "aggregated_output");
                                format!("{}\nexit code: {}", if output.is_empty() { "(no captured output)" } else { &output }, s(&item.item, "exit_code"))
                            }
                            "agent_message" | "reasoning" => s(&item.item, "text"),
                            "mcp_tool_call" => ["arguments", "result", "error"].iter()
                                .filter_map(|key| item.item.get(key).map(|value| format!("{key}: {value}")))
                                .collect::<Vec<_>>().join("\n"),
                            "file_change" => item.item.get("changes").map(Value::to_string).unwrap_or_default(),
                            _ => serde_json::to_string_pretty(&item.item).unwrap_or_else(|_| item.item.to_string()),
                        }
                    };
                    lines.extend(safe_text(&detail).lines().map(|line| Line::from(line.to_owned())));
                }
            }
            if transcript_items.is_empty() && transcript_notice.is_none() {
                lines.push(Line::from("No Codex events yet."));
            }
        } else {
            lines.push(Line::from("No agent executions observed."));
        }
        for warning in warnings {
            lines.push(Line::from(Span::styled(format!("WARNING: {warning}"), Style::default().fg(Color::Red))));
        }
        let title = if transcript_focus { " Transcript · focused " } else { " Transcript " };
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: true })
            .scroll((transcript_scroll, 0));
        frame.render_widget(paragraph, panes[1]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
