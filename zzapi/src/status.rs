//! Read-only agent status dashboard migrated from dept/status.py.
use super::{ApiError, Client, Fail};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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
        r.started = timestamp(a.get("started_at"));
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
fn compact(x: &str, n: usize) -> String {
    let c: Vec<_> = x.chars().collect();
    if c.len() <= n {
        return x.into();
    }
    if n <= 3 {
        return ".".repeat(n);
    }
    format!("...{}", c[c.len() + 3 - n..].iter().collect::<String>())
}
fn line(e: &Execution, n: i64, root: &Path, lost: bool) -> String {
    let (total, cross) = e.total(n);
    let wt = if e.worktree.is_empty() {
        task_dir(&e.task, root)
    } else {
        e.worktree.clone()
    };
    format!(
        "{:<20} {:<24} {:<14} {:<15} {:<11} {:<20} {:<30} {:<24} {}",
        e.task.chars().take(20).collect::<String>(),
        e.phase().chars().take(24).collect::<String>(),
        dur(e.current(n)),
        format!("{}{}", dur(total), if cross { "*" } else { "" }),
        if e.state.is_empty() {
            "not observed".into()
        } else {
            e.state.chars().take(11).collect::<String>()
        },
        if e.agent.is_empty() {
            "-".into()
        } else {
            e.agent.chars().take(20).collect::<String>()
        },
        compact(&wt, 30),
        e.latest(),
        e.flags(lost)
    )
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
    let _term =
        Terminal::enter().map_err(|e| Fail::Config(format!("cannot enter terminal mode: {e}")))?;
    let (mut rows, mut warnings, mut lost) = (rows, w, lost);
    let (mut selected, mut offset, mut hscroll) = (0usize, 0usize, 0usize);
    let (mut detail, mut transcript) = (false, false);
    let mut output_text = String::new();
    let mut tick = Instant::now();
    loop {
        draw(
            &rows,
            &warnings,
            &root,
            selected,
            offset,
            hscroll,
            detail,
            transcript,
            &output_text,
            lost,
        );
        match key(100) {
            Some('q') | Some('Q') | Some('\x1b') => {
                if detail || transcript {
                    detail = false;
                    transcript = false
                } else {
                    break;
                }
            }
            Some('j') => selected = (selected + 1).min(rows.len().saturating_sub(1)),
            Some('k') => selected = selected.saturating_sub(1),
            Some('h') => hscroll = hscroll.saturating_sub(8),
            Some('l') => hscroll += 8,
            Some('t') => {
                transcript = !transcript;
                detail = true;
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
            Some('\n') | Some('\r') => {
                detail = !detail;
                transcript = false
            }
            _ => {}
        }
        if selected < offset {
            offset = selected
        }
        if selected >= offset + 10 {
            offset = selected.saturating_sub(9)
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
fn size() -> (usize, usize) {
    unsafe {
        let mut w: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut w) == 0 {
            return (w.ws_row as usize, w.ws_col as usize);
        }
    }
    (24, 100)
}
#[allow(clippy::too_many_arguments)]
fn draw(
    rows: &[Execution],
    warnings: &[String],
    root: &Path,
    selected: usize,
    offset: usize,
    hscroll: usize,
    detail: bool,
    transcript: bool,
    out_text: &str,
    lost: bool,
) {
    let (h, w) = size();
    let mut o = String::from("\x1b[2J\x1b[H");
    o.push_str(if transcript {
        "zzapi status — output  q/Esc back"
    } else {
        "zzapi status — read-only  ↑↓/j/k select  ←→/h/l scroll  Enter history  t output  q quit"
    });
    o.push('\n');
    let heads = "TASK                 PHASE                    PHASE ELAPSED  OBSERVED TOTAL  STATE       AGENT ID             DIR                            LAST EVENT  FLAGS";
    o.push_str(
        &heads
            .chars()
            .skip(hscroll)
            .take(w.saturating_sub(1))
            .collect::<String>(),
    );
    o.push('\n');
    for (i, e) in rows
        .iter()
        .enumerate()
        .skip(offset)
        .take((h / 2).saturating_sub(3).max(1))
    {
        if i == selected {
            o.push_str("\x1b[7m")
        }
        o.push_str(
            &line(e, now(), root, lost)
                .chars()
                .skip(hscroll)
                .take(w.saturating_sub(1))
                .collect::<String>(),
        );
        if i == selected {
            o.push_str("\x1b[0m")
        }
        o.push('\n')
    }
    o.push_str(&"-".repeat(w.saturating_sub(1)));
    o.push('\n');
    if rows.is_empty() {
        o.push_str("No execution events observed.\n")
    } else if transcript {
        o.push_str(&format!(
            "Output for {} / {}\n{}\n",
            rows[selected].task, rows[selected].id, out_text
        ))
    } else if detail {
        let e = &rows[selected];
        o.push_str(&format!("{} / {} — {}\n", e.task, e.id, e.flags(lost)));
        let start = e.events.len().saturating_sub(12);
        for i in start..e.events.len() {
            let v = &e.events[i];
            if i > start {
                let previous = &e.events[i - 1];
                if !s(previous, "clock").is_empty()
                    && !s(v, "clock").is_empty()
                    && s(previous, "clock") != s(v, "clock")
                {
                    o.push_str(&format!(
                        "  ↳ cross-clock: {} → {} (not subtracted)\n",
                        s(previous, "occurred_at"),
                        s(v, "occurred_at")
                    ));
                } else if elapsed(previous, v).is_none() {
                    o.push_str("  ↳ timing not observed: invalid, missing, or out-of-order same-clock timestamp\n");
                }
            }
            o.push_str(&format!(
                "  {}  {}  [{}]\n",
                short_time(v.get("occurred_at")),
                s(v, "kind"),
                s(v, "source")
            ))
        }
    } else {
        o.push_str("Select an execution and press Enter for event history or t for output.\n")
    }
    for x in warnings {
        o.push_str(&format!("WARNING: {x}\n"))
    }
    let _ = io::stdout().write_all(o.as_bytes());
    let _ = io::stdout().flush();
}
fn key(ms: i32) -> Option<char> {
    unsafe {
        let mut p = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        if libc::poll(&mut p, 1, ms) < 1 {
            return None;
        }
        let mut b = [0u8; 1];
        if libc::read(0, b.as_mut_ptr() as *mut _, 1) != 1 {
            return None;
        }
        if b[0] == 27 {
            let mut a = [0u8; 2];
            let mut p = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut p, 1, 30) < 1 {
                return Some('\x1b');
            }
            if libc::read(0, a.as_mut_ptr() as *mut _, 1) != 1 {
                return Some('\x1b');
            }
            if libc::poll(&mut p, 1, 30) < 1 {
                return Some('\x1b');
            }
            if libc::read(0, a[1..].as_mut_ptr() as *mut _, 1) != 1 {
                return Some('\x1b');
            }
            return match a {
                [91, 65] => Some('k'),
                [91, 66] => Some('j'),
                [91, 67] => Some('l'),
                [91, 68] => Some('h'),
                _ => Some('\x1b'),
            };
        }
        if b[0] == 3 {
            return Some('q');
        }
        Some(b[0] as char)
    }
}
struct Terminal {
    old: libc::termios,
}
impl Terminal {
    fn enter() -> io::Result<Self> {
        unsafe {
            let mut old: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut old) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = old;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            let _ = io::stdout().write_all(b"\x1b[?1049h\x1b[?25l");
            Ok(Self { old })
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::tcsetattr(0, libc::TCSANOW, &self.old);
        }
        let _ = io::stdout().write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
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
    fn combines_events_with_agent_state_and_worktree() {
        let events = vec![
            serde_json::json!({"id":"e1","task_id":"agent-1","execution_id":"run-1","kind":"process_spawned","occurred_at":"2026-10-08T00:00:00Z","clock":"host"}),
        ];
        let agents = vec![
            serde_json::json!({"id":"agent-handle","task_id":"agent-1","execution_id":"run-1","state":"orphaned","started_at":"2026-10-08T00:00:00Z","audit_degraded":true}),
        ];
        let wt = HashMap::from([("agent-1".to_owned(), "/tmp/agent-1".to_owned())]);
        let rows = build(events, &agents, &wt, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].phase(), "process_spawned");
        assert_eq!(rows[0].state, "orphaned");
        assert_eq!(rows[0].agent, "agent-handle");
        assert_eq!(rows[0].worktree, "/tmp/agent-1");
        assert!(rows[0].flags(false).contains("audit degraded"));
    }
}
