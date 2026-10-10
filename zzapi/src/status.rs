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
const OUTPUT_TAIL: u64 = 32 * 1024 * 1024;
const EVENT_LIMIT: usize = 10_000;
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
            r.branch = git_branch(&r.worktree)
        }
    }
    out.sort_by(|a, b| {
        (a.state != "running")
            .cmp(&(b.state != "running"))
            .then_with(|| b.latest().cmp(&a.latest()))
    });
    out
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
    if max_width == UnicodeWidthChar::width('…').unwrap_or(1) {
        return "…".into();
    }

    let suffix_width = max_width - UnicodeWidthChar::width('…').unwrap_or(1);
    let mut suffix = String::new();
    let mut width = 0;
    for ch in path.chars().rev() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > suffix_width {
            break;
        }
        suffix.push(ch);
        width += char_width;
    }
    suffix = suffix.chars().rev().collect();
    format!("…{suffix}")
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
fn output(c: &Client, e: &Execution) -> String {
    if e.agent.is_empty() {
        return "No retained supervised agent matches this execution.".into();
    }
    match c.get(
        &format!("/v1/agents/{}/logs", e.agent),
        &[("stream", "both".into()), ("tail", OUTPUT_TAIL.to_string())],
    ) {
        Ok(v) => {
            let text = v
                .get("records")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|r| s(r, "data")).collect::<String>())
                .unwrap_or_default();
            let l: Vec<_> = text.lines().collect();
            if l.is_empty() {
                "No spool output yet.".into()
            } else {
                l[l.len().saturating_sub(40)..].join("\n")
            }
        }
        Err(e) => format!("Relay output unavailable: {}", e.message),
    }
}

fn transcript_path(e: &Execution, root: &Path) -> PathBuf {
    if !e.worktree.is_empty() {
        return PathBuf::from(&e.worktree).join("last-message.txt");
    }
    let task = e.task.strip_prefix("codex-").unwrap_or(&e.task);
    root.join(task).join("last-message.txt")
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
    let mut table_state = ratatui::widgets::TableState::default();
    let mut detail_scroll = 0u16;
    let (mut detail, mut transcript) = (false, false);
    let mut output_text = String::new();
    let mut tick = Instant::now();
    loop {
        draw(
            &mut terminal,
            &mut table_state,
            &rows,
            &warnings,
            &root,
            selected,
            detail_scroll,
            detail,
            transcript,
            &output_text,
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
                Some(KeyCode::Char('q' | 'Q')) | Some(KeyCode::Esc) => {
                    if detail || transcript {
                        detail = false;
                        transcript = false
                    } else {
                        break;
                    }
                }
                Some(KeyCode::Char('j')) | Some(KeyCode::Down) => {
                    selected = (selected + 1).min(rows.len().saturating_sub(1));
                    detail_scroll = 0;
                }
                Some(KeyCode::Char('k')) | Some(KeyCode::Up) => {
                    selected = selected.saturating_sub(1);
                    detail_scroll = 0;
                }
                Some(KeyCode::Char('h')) | Some(KeyCode::Left) => {
                    detail_scroll = detail_scroll.saturating_sub(1);
                }
                Some(KeyCode::Char('l')) | Some(KeyCode::Right) => {
                    detail_scroll = detail_scroll.saturating_add(1);
                }
                Some(KeyCode::Char('t')) => {
                    transcript = !transcript;
                    detail = true;
                    detail_scroll = 0;
                    if transcript && !rows.is_empty() {
                        let path = transcript_path(&rows[selected], &root);
                        output_text = format!(
                            "Transcript for {} / {}\nSource: {}\nCommand: tail -F '{}'\nRelay output (last 40 lines):\n{}",
                            rows[selected].task,
                            rows[selected].id,
                            path.display(),
                            path.display().to_string().replace('\'', "'\\''"),
                            output(c, &rows[selected])
                        )
                    }
                }
                Some(KeyCode::Enter) => {
                    detail = !detail;
                    transcript = false
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
                    selected = selected.min(rows.len().saturating_sub(1))
                }
                Err(e) => warnings = vec![format!("relay status unavailable: {}", e.message)],
            }
            tick = Instant::now()
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
    root: &Path,
    selected: usize,
    detail_scroll: u16,
    detail: bool,
    transcript: bool,
    out_text: &str,
    lost: bool,
) {
    let _ = terminal.terminal.draw(|frame| {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Min(6),
            ])
            .split(frame.area());
        let dir_column_width =
            (areas[1].width.saturating_sub(2) as usize * 13 / 100).saturating_sub(2);
        let title = if transcript {
            "zzapi status — output  q/Esc back"
        } else {
            "zzapi status — ↑↓/j/k select  Enter history  t output  ←→/h/l scroll details  q quit"
        };
        frame.render_widget(
            Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
            areas[0],
        );

        let table_rows = rows.iter().map(|e| {
            let dir = if e.worktree.is_empty() {
                task_dir(&e.task, root)
            } else {
                e.worktree.clone()
            };
            let started = if e.started_at.is_empty() {
                "?".into()
            } else {
                e.started_at
                    .chars()
                    .take(19)
                    .collect::<String>()
                    .replace('T', " ")
            };
            Row::new(vec![
                Cell::from(e.id.as_str()),
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
                Cell::from(if e.state.is_empty() {
                    "not observed"
                } else {
                    &e.state
                }),
                Cell::from(if e.command.is_empty() {
                    "-"
                } else {
                    &e.command
                }),
                Cell::from(started),
                Cell::from(if e.exit_code.is_empty() {
                    "-"
                } else {
                    &e.exit_code
                }),
                Cell::from(tail_path(&dir, dir_column_width)),
                Cell::from(e.latest()),
            ])
        });
        let header = Row::new([
            "ID",
            "TASK",
            "PR / BRANCH",
            "STATE",
            "COMMAND",
            "STARTED",
            "EXIT",
            "DIR",
            "LAST EVENT",
        ])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(0);
        let widths = [
            Constraint::Percentage(11),
            Constraint::Percentage(12),
            Constraint::Percentage(17),
            Constraint::Percentage(9),
            Constraint::Percentage(16),
            Constraint::Percentage(10),
            Constraint::Percentage(5),
            Constraint::Percentage(10),
            Constraint::Percentage(10),
        ];
        let table = Table::new(table_rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(" Executions "))
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("» ");
        table_state.select((!rows.is_empty()).then_some(selected));
        frame.render_stateful_widget(table, areas[1], table_state);

        let mut lines = Vec::new();
        if rows.is_empty() {
            lines.push(Line::from("No execution events observed."));
        } else if transcript {
            let e = &rows[selected];
            lines.push(Line::from(format!("Output for {} / {}", e.task, e.id)));
            lines.extend(out_text.lines().map(Line::from));
        } else if detail {
            let e = &rows[selected];
            lines.push(Line::from(format!(
                "{} / {} — {}",
                e.task,
                e.id,
                e.flags(lost)
            )));
            let start = e.events.len().saturating_sub(12);
            for i in start..e.events.len() {
                let v = &e.events[i];
                if i > start {
                    let previous = &e.events[i - 1];
                    if !s(previous, "clock").is_empty()
                        && !s(v, "clock").is_empty()
                        && s(previous, "clock") != s(v, "clock")
                    {
                        lines.push(Line::from(format!(
                            "↳ cross-clock: {} → {} (not subtracted)",
                            s(previous, "occurred_at"),
                            s(v, "occurred_at")
                        )));
                    } else if elapsed(previous, v).is_none() {
                        lines.push(Line::from(
                            "↳ timing not observed: invalid, missing, or out-of-order timestamp",
                        ));
                    }
                }
                lines.push(Line::from(format!(
                    "{}  {}  [{}]",
                    short_time(v.get("occurred_at")),
                    s(v, "kind"),
                    s(v, "source")
                )));
            }
        } else {
            lines.push(Line::from(
                "Select an execution and press Enter for event history or t for output.",
            ));
        }
        for warning in warnings {
            lines.push(Line::from(vec![Span::styled(
                format!("WARNING: {warning}"),
                Style::default().fg(Color::Red),
            )]));
        }
        let detail_title = if transcript {
            " Output "
        } else if detail {
            " Event history "
        } else {
            " Details "
        };
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(detail_title))
            .wrap(Wrap { trim: true })
            .scroll((detail_scroll, 0));
        frame.render_widget(paragraph, areas[2]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn truncates_paths_from_the_left() {
        assert_eq!(tail_path("/repo/worktree/task-123", 12), "…ee/task-123");
        assert_eq!(tail_path("/repo/task", 20), "/repo/task");
        assert_eq!(tail_path("/repo/task", 1), "…");
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
}
