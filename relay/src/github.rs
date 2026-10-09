use crate::exec;
use crate::http::{error, query, reply};
use crate::review_loop;
use crate::server::Server;
use crate::session::require_gui_login_session;
use relay_core::{Json, parse_json};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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
    let policy = require_gui_login_session().and_then(|_| exec::load_policy())?;
    let request = exec::ExecRequest {
        id: format!("github-pr-scan-{repo}"),
        bin: "gh".to_owned(),
        args: vec![
            "api".to_owned(),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
            format!("repos/{repo}/pulls?state=open&per_page=100"),
        ],
    };
    let path = policy
        .verified_path(&request.bin, &request.args)
        .map_err(|error| format!("the gh policy does not allow the PR scan: {error}"))?;
    let result = exec::run(&path, request);
    if result.timed_out || result.truncated || result.exit_code != Some(0) {
        return Err("GitHub PR discovery did not complete successfully".to_owned());
    }
    parse_github_open_pull_requests(&result.stdout)
}
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
