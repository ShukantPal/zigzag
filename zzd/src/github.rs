use crate::github_api;
use crate::http::{error, query, reply};
use crate::review_loop;
use crate::server::Server;
use crate::session::require_gui_login_session;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zz::{Json, parse_json};

static WATCHED_PR_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub(crate) struct WatchedPr {
    pub(crate) repository: String,
    pub(crate) number: u64,
    pub(crate) branch: String,
    pub(crate) worktree: String,
    pub(crate) repository_path: String,
}

fn watched_pr_path() -> PathBuf {
    std::env::var_os("ZIGZAG_WATCHED_PRS_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".zigzag/watched-prs.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".zigzag/watched-prs.json"))
}

pub(crate) fn watched_prs() -> Result<Vec<WatchedPr>, String> {
    let path = watched_pr_path();
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("could not parse watched PR state: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("could not read watched PR state: {error}")),
    }
}

fn save_watched_prs(prs: &[WatchedPr]) -> Result<(), String> {
    let path = watched_pr_path();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create watched PR directory: {error}"))?;
    let bytes = serde_json::to_vec_pretty(prs).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("could not write watched PR state: {error}"))?;
    std::fs::rename(temporary, path)
        .map_err(|error| format!("could not persist watched PR state: {error}"))
}

fn add_watched_pr(pr: WatchedPr) -> Result<(), String> {
    let _guard = WATCHED_PR_WRITE
        .lock()
        .map_err(|_| "watched PR state lock poisoned")?;
    let mut prs = watched_prs()?;
    if let Some(existing) = prs
        .iter_mut()
        .find(|entry| entry.repository == pr.repository && entry.number == pr.number)
    {
        *existing = pr;
    } else {
        prs.push(pr);
    }
    save_watched_prs(&prs)
}

/// Discover and persist an open PR for an agent's pushed branch.
pub(crate) fn watch_agent_pr(agent: &zz::AgentRecord) -> Result<bool, String> {
    let Some(worktree) = agent.worktree_path.as_deref() else {
        return Ok(false);
    };
    let worktree_path = Path::new(worktree);
    let branch = git_text(worktree_path, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Ok(false);
    }
    let remote = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(worktree_path)
        .output()
        .map_err(|error| format!("could not inspect agent git remote: {error}"))?;
    if !remote.status.success() {
        return Ok(false);
    }
    let repository = github_repository(&String::from_utf8_lossy(&remote.stdout))
        .ok_or_else(|| "agent origin is not a GitHub repository".to_owned())?;
    let branch_ref = format!("refs/heads/{branch}");
    let pushed = Command::new("git")
        .args(["ls-remote", "--exit-code", "origin", &branch_ref])
        .current_dir(worktree_path)
        .output()
        .map_err(|error| format!("could not check pushed branch: {error}"))?;
    if !pushed.status.success() || pushed.stdout.is_empty() {
        return Ok(false);
    }
    require_gui_login_session()?;
    let pull_requests = github_api::get_all(&format!("repos/{repository}/pulls?state=open"))?;
    let Some(number) = pull_requests
        .iter()
        .find(|item| {
            item.pointer("/head/ref")
                .and_then(serde_json::Value::as_str)
                == Some(branch.as_str())
                && item
                    .pointer("/head/repo/full_name")
                    .and_then(serde_json::Value::as_str)
                    == Some(repository.as_str())
        })
        .and_then(|item| item.get("number").and_then(serde_json::Value::as_u64))
    else {
        return Ok(false);
    };
    let repo_path = git_text(worktree_path, &["rev-parse", "--show-toplevel"])?;
    add_watched_pr(WatchedPr {
        repository,
        number,
        branch,
        worktree: worktree.to_owned(),
        repository_path: repo_path,
    })?;
    log::info!("watching PR for agent {}", agent.id);
    Ok(true)
}

fn git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("git metadata lookup failed".to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) fn github_repository(origin: &str) -> Option<String> {
    let origin = origin.trim().trim_end_matches(".git");
    let path = origin
        .strip_prefix("git@github.com:")
        .or_else(|| origin.strip_prefix("https://github.com/"))
        .or_else(|| origin.strip_prefix("http://github.com/"))?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    (parts.next().is_none() && crate::config::valid_github_repo(&format!("{owner}/{repo}")))
        .then(|| format!("{owner}/{repo}"))
}

pub(crate) fn watched_pr_cleanup_loop(state: Arc<Server>, interval: Duration) {
    loop {
        match watched_prs() {
            Ok(prs) => {
                for pr in prs {
                    if let Err(error) = cleanup_if_merged(&state, &pr) {
                        log::warn!(
                            "watched PR cleanup for {}#{} failed: {error}",
                            pr.repository,
                            pr.number
                        );
                    }
                }
            }
            Err(error) => log::warn!("could not load watched PR state: {error}"),
        }
        thread::sleep(interval);
    }
}

fn cleanup_if_merged(state: &Server, pr: &WatchedPr) -> Result<(), String> {
    require_gui_login_session()?;
    let value = match github_api::get(&format!("repos/{}/pulls/{}", pr.repository, pr.number)) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    if value.get("merged").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(());
    }
    let agents = state.supervisor.registry.list(None, None);
    for agent in agents.iter().filter(|agent| {
        agent.worktree_path.as_deref() == Some(pr.worktree.as_str())
            || agent.worktree_path.as_deref().is_some_and(|path| {
                Path::new(path).exists()
                    && git_text(Path::new(path), &["branch", "--show-current"])
                        .ok()
                        .as_deref()
                        == Some(pr.branch.as_str())
            })
    }) {
        if agent.state == "running" || agent.state == "orphaned" {
            if agent.process_group > 0 && crate::proc::recovered_agent_identity_matches(agent) {
                crate::proc::force_kill_process_group(agent.process_group);
            }
            if let Ok(mut processes) = state.supervisor.procs.lock()
                && let Some(entry) = processes.get_mut(&agent.id)
            {
                let _ = entry.child.try_wait();
                entry.finished_at = Some(std::time::Instant::now());
            }
            let _ = state
                .supervisor
                .registry
                .transition(&agent.id, "stopped", None);
        }
    }
    let path = Path::new(&pr.worktree);
    if path.exists() {
        let roots = crate::routes::worktrees::canonical_worktree_roots();
        crate::routes::worktrees::cleanup_agent_worktree(&pr.worktree, &roots)
            .map_err(|error| error.message.to_owned())?;
    }
    let _guard = WATCHED_PR_WRITE
        .lock()
        .map_err(|_| "watched PR state lock poisoned")?;
    let mut remaining = watched_prs()?;
    remaining.retain(|entry| !(entry.repository == pr.repository && entry.number == pr.number));
    save_watched_prs(&remaining)?;
    log::info!(
        "cleaned worktree for merged PR {}#{}",
        pr.repository,
        pr.number
    );
    Ok(())
}

pub(crate) fn should_start_legacy_watch(
    review_loop_authoritative: bool,
    repositories: &[String],
) -> bool {
    !review_loop_authoritative && !repositories.is_empty()
}
pub(crate) fn github_watch_loop(state: Arc<Server>, repos: Vec<String>, interval: Duration) {
    loop {
        for repo in &repos {
            match github_open_pull_requests(repo) {
                Ok(pull_requests) => {
                    for number in pull_requests {
                        let id = format!("github-pr-opened:{repo}:{number}");
                        let payload = Json::Object(vec![
                            ("id".to_owned(), Json::String(id.clone())),
                            (
                                "kind".to_owned(),
                                Json::String("github_pr_opened".to_owned()),
                            ),
                            ("repository".to_owned(), Json::String(repo.clone())),
                            ("pull_request".to_owned(), Json::number(number)),
                            (
                                "url".to_owned(),
                                Json::String(format!("https://github.com/{repo}/pull/{number}")),
                            ),
                        ]);
                        match state.store.add(payload) {
                            Ok((_, false)) => {
                                log::debug!("queued GitHub PR watchdog event for {repo}#{number}")
                            }
                            Ok((_, true)) => {}
                            Err(_) => log::warn!(
                                "could not persist GitHub PR watchdog event for {repo}#{number}"
                            ),
                        }
                    }
                }
                Err(error) => log::warn!("GitHub PR watch for {repo} failed: {error}"),
            }
        }
        thread::sleep(interval);
    }
}
pub(crate) fn github_open_pull_requests(repo: &str) -> Result<Vec<u64>, String> {
    if !crate::config::valid_github_repo(repo) {
        return Err("GitHub PR discovery received an invalid repository".to_owned());
    }
    require_gui_login_session()?;
    let pull_requests = github_api::get_all(&format!("repos/{repo}/pulls?state=open"))?;
    let numbers = pull_requests
        .iter()
        .map(|pr| {
            pr.get("number")
                .and_then(serde_json::Value::as_u64)
                .filter(|number| *number > 0)
                .ok_or_else(|| "GitHub PR discovery result is missing a PR number".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(numbers)
}
#[cfg(test)]
pub(crate) fn parse_github_open_pull_requests(output: &str) -> Result<Vec<u64>, String> {
    let value = parse_json(output)
        .map_err(|_| "GitHub PR discovery did not return the expected JSON".to_owned())?;
    let Json::Array(pull_requests) = value else {
        return Err("GitHub PR discovery did not return a JSON array".to_owned());
    };
    let pull_requests: Vec<_> = pull_requests
        .iter()
        .flat_map(|page| match page {
            Json::Array(pull_requests) => pull_requests.iter().collect(),
            pull_request => vec![pull_request],
        })
        .collect();
    pull_requests
        .iter()
        .map(|pull_request| {
            let number = pull_request
                .object("number")
                .and_then(Json::as_u64)
                .filter(|number| *number > 0)
                .ok_or_else(|| "GitHub PR discovery result is missing a PR number".to_owned())?;
            Ok(number)
        })
        .collect()
}
pub(crate) fn review_gate_request<G>(
    stream: &mut TcpStream,
    state: &Server,
    target: &str,
    gate_report: G,
) -> Result<(), String>
where
    G: Fn(
        &str,
        u64,
        &std::path::Path,
        bool,
        &review_loop::ReviewLoopConfig,
    ) -> Result<serde_json::Value, String>,
{
    let (repository, number) = match review_gate_parameters(target) {
        Ok(parameters) => parameters,
        Err(()) => return reply(stream, 400, error("invalid_review_gate_query")),
    };
    let config = match state.review_config.lock() {
        Ok(config) => config.clone(),
        Err(_) => return reply(stream, 500, error("review_gate_failed")),
    };
    let Some(config) = config else {
        return reply(stream, 500, error("review_gate_failed"));
    };
    match gate_report(
        &repository,
        number,
        &state.review_state_file,
        state.review_loop_shadow,
        &config,
    ) {
        Ok(report) => {
            let encoded = serde_json::to_string(&report)
                .map_err(|error| format!("could not encode review gate report: {error}"))?;
            let response = parse_json(&encoded)
                .map_err(|_| "could not convert review gate report".to_owned())?;
            reply(stream, 200, response)
        }
        Err(error_message) => {
            log::warn!("review gate failed for {repository}#{number}: {error_message}");
            reply(stream, 500, error("review_gate_failed"))
        }
    }
}
pub(crate) fn review_gate_parameters(target: &str) -> Result<(String, u64), ()> {
    let values = query(target).map_err(|_| ())?;
    let allowed = ["repository", "pull_request"];
    if values.len() != allowed.len() || values.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(());
    }
    let repository = values
        .get("repository")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or(())?;
    let number = values
        .get("pull_request")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|number| *number > 0)
        .ok_or(())?;
    Ok((repository, number))
}

#[cfg(test)]
mod watched_pr_tests {
    use super::*;

    #[test]
    fn github_repository_accepts_ssh_and_https_origins() {
        assert_eq!(
            github_repository("git@github.com:ShukantPal/zigzag.git"),
            Some("ShukantPal/zigzag".to_owned())
        );
        assert_eq!(
            github_repository("https://github.com/owner/repo"),
            Some("owner/repo".to_owned())
        );
        assert_eq!(github_repository("https://example.com/owner/repo"), None);
        assert_eq!(
            github_repository("git@github.com:owner/repo/extra.git"),
            None
        );
    }

    #[test]
    fn watched_pr_state_round_trips() {
        let pr = WatchedPr {
            repository: "owner/repo".to_owned(),
            number: 7,
            branch: "codex/feature".to_owned(),
            worktree: "/private/tmp/codex-feature".to_owned(),
            repository_path: "/workspace/repo".to_owned(),
        };
        let encoded = serde_json::to_vec(&vec![pr.clone()]).unwrap();
        let decoded: Vec<WatchedPr> = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, vec![pr]);
    }
}
