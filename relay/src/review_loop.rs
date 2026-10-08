use super::{Json, Server, exec, kill_process_group, new_execution_id, relay_event, spawn_proc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const CONFIG_SCHEMA: &str = include_str!("config-v1.json");
const RESULT_VERSION: u64 = 1;
const REVIEWER_BIN: &str = "codex-launch";

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersonalConfig {
    pub schema_version: u64,
    pub review_loop: ReviewLoopConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewLoopConfig {
    pub enabled: bool,
    pub intervals: Intervals,
    pub repositories: Vec<RepositoryPolicy>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Intervals {
    pub discovery_seconds: u64,
    pub review_seconds: u64,
    pub merge_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPolicy {
    pub repository: String,
    pub full_rounds_max: u64,
    pub verification_rounds_max: u64,
    pub lenses: Vec<String>,
    pub require_security_lens: bool,
    pub required_ci_checks: Vec<RequiredCheck>,
    pub trusted_verdict_identity: String,
    pub result_limits: ResultLimits,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RequiredCheck {
    pub label: String,
    pub name_pattern: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultLimits {
    pub max_findings_per_lens: usize,
    pub max_bytes_per_lens: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigViolation {
    pub path: String,
    pub reason: String,
}

impl std::fmt::Display for ConfigViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(formatter, "{}", self.reason)
        } else {
            write!(formatter, "{}: {}", self.path, self.reason)
        }
    }
}

pub fn default_config_path() -> Result<PathBuf, ConfigViolation> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".zigzag/config.yaml"))
        .ok_or_else(|| ConfigViolation {
            path: "review_loop".to_owned(),
            reason: "HOME is not set; cannot locate ~/.zigzag/config.yaml".to_owned(),
        })
}

pub fn load_config(path: &Path) -> Result<PersonalConfig, Vec<ConfigViolation>> {
    let text = fs::read_to_string(path).map_err(|error| {
        vec![ConfigViolation {
            path: "review_loop".to_owned(),
            reason: format!("could not read {}: {error}", path.display()),
        }]
    })?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|error| {
        vec![ConfigViolation {
            path: "".to_owned(),
            reason: format!("invalid YAML: {error}"),
        }]
    })?;
    let mut tag_errors = Vec::new();
    reject_yaml_tags(&yaml, "", &mut tag_errors);
    if !tag_errors.is_empty() {
        return Err(tag_errors);
    }
    let instance = serde_json::to_value(&yaml).map_err(|error| {
        vec![ConfigViolation {
            path: "".to_owned(),
            reason: format!("YAML cannot be represented as JSON: {error}"),
        }]
    })?;
    let schema: Value = serde_json::from_str(CONFIG_SCHEMA).expect("embedded schema is valid JSON");
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("embedded config schema is valid");
    let mut violations: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| ConfigViolation {
            path: json_path(error.instance_path().as_str()),
            reason: error.to_string(),
        })
        .collect();
    if violations.is_empty() {
        let mut repositories = BTreeSet::new();
        if let Some(items) = instance
            .pointer("/review_loop/repositories")
            .and_then(Value::as_array)
        {
            for (index, item) in items.iter().enumerate() {
                let Some(repository) = item.get("repository").and_then(Value::as_str) else {
                    continue;
                };
                if !repositories.insert(repository.to_owned()) {
                    violations.push(ConfigViolation {
                        path: format!("review_loop.repositories[{index}].repository"),
                        reason: format!("duplicate repository {repository:?}"),
                    });
                }
            }
        }
    }
    if !violations.is_empty() {
        violations
            .sort_by(|left, right| (&left.path, &left.reason).cmp(&(&right.path, &right.reason)));
        return Err(violations);
    }
    serde_json::from_value(instance).map_err(|error| {
        vec![ConfigViolation {
            path: "".to_owned(),
            reason: format!("configuration does not match its data model: {error}"),
        }]
    })
}

fn reject_yaml_tags(value: &serde_yaml::Value, path: &str, errors: &mut Vec<ConfigViolation>) {
    match value {
        serde_yaml::Value::Tagged(tagged) => errors.push(ConfigViolation {
            path: path.to_owned(),
            reason: format!("custom YAML tag {} is not allowed", tagged.tag),
        }),
        serde_yaml::Value::Sequence(values) => {
            for (index, value) in values.iter().enumerate() {
                reject_yaml_tags(value, &format!("{path}[{index}]"), errors);
            }
        }
        serde_yaml::Value::Mapping(values) => {
            for (key, value) in values {
                let key = key.as_str().unwrap_or("?");
                let child = if path.is_empty() {
                    key.to_owned()
                } else {
                    format!("{path}.{key}")
                };
                reject_yaml_tags(value, &child, errors);
            }
        }
        _ => {}
    }
}

fn json_path(pointer: &str) -> String {
    let mut output = String::new();
    for segment in pointer.split('/').skip(1) {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        if segment.bytes().all(|byte| byte.is_ascii_digit()) {
            output.push('[');
            output.push_str(&segment);
            output.push(']');
        } else {
            if !output.is_empty() {
                output.push('.');
            }
            output.push_str(&segment);
        }
    }
    output
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RoundPhase {
    Dispatching,
    Reviewing,
    Findings,
    Ready,
    Attention,
    Superseded,
    Merged,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReviewerState {
    lens: String,
    attempt: u64,
    task_id: String,
    agent_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AdmittedVerdict {
    verdict: Verdict,
    head: String,
    findings: Vec<String>,
    created_at: String,
    comment_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Approve,
    ChangesRequested,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct OwnerContext {
    session_id: String,
    project_dir: PathBuf,
    department_task_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReviewRound {
    repository: String,
    pull_request: u64,
    head: String,
    branch: String,
    verification: bool,
    phase: RoundPhase,
    reviewers: BTreeMap<String, ReviewerState>,
    verdicts: BTreeMap<String, AdmittedVerdict>,
    owner: Option<OwnerContext>,
    owner_task_id: Option<String>,
    owner_agent_id: Option<String>,
    gate_reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DurableState {
    #[serde(default = "state_schema_version")]
    schema_version: u64,
    #[serde(default)]
    rounds: BTreeMap<String, ReviewRound>,
}

fn state_schema_version() -> u64 {
    1
}

struct StateStore {
    path: PathBuf,
    state: DurableState,
}

impl StateStore {
    fn open(path: PathBuf) -> Result<Self, String> {
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid review-loop state: {error}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DurableState {
                schema_version: state_schema_version(),
                ..DurableState::default()
            },
            Err(error) => return Err(format!("could not read review-loop state: {error}")),
        };
        if state.schema_version != state_schema_version() {
            return Err(format!(
                "unsupported review-loop state version {}",
                state.schema_version
            ));
        }
        Ok(Self { path, state })
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create review state directory: {error}"))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.state)
            .map_err(|error| format!("could not encode review-loop state: {error}"))?;
        let temporary = self.path.with_extension("tmp");
        write_private(temporary.clone(), &bytes)?;
        fs::rename(&temporary, &self.path)
            .map_err(|error| format!("could not commit review-loop state: {error}"))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestSnapshot {
    head_ref_oid: String,
    head_ref_name: String,
    state: String,
    merged_at: Option<String>,
    #[serde(default)]
    comments: Vec<Comment>,
    #[serde(default)]
    status_check_rollup: Vec<Check>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    #[serde(default)]
    author: Option<Author>,
    body: String,
    created_at: String,
    id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Author {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Check {
    name: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GateDecision {
    ready: bool,
    reasons: Vec<String>,
}

pub fn start(state: Arc<Server>, config: ReviewLoopConfig, state_path: PathBuf, shadow: bool) {
    thread::spawn(move || {
        let mut store = match StateStore::open(state_path) {
            Ok(store) => store,
            Err(error) => {
                eprintln!("review loop disabled: {error}");
                return;
            }
        };
        let mut discovery_due = Instant::now();
        let mut review_due = Instant::now();
        let mut merge_due = Instant::now();
        eprintln!(
            "review loop started{} for {} repositories",
            if shadow { " in shadow mode" } else { "" },
            config.repositories.len()
        );
        loop {
            let now = Instant::now();
            if now >= discovery_due {
                if let Err(error) = discover(&state, &config, &mut store, shadow) {
                    eprintln!("review discovery failed: {error}");
                }
                discovery_due = now + Duration::from_secs(config.intervals.discovery_seconds);
            }
            if now >= review_due {
                if let Err(error) = poll_reviews(&state, &config, &mut store, shadow) {
                    eprintln!("review poll failed: {error}");
                }
                review_due = now + Duration::from_secs(config.intervals.review_seconds);
            }
            if now >= merge_due {
                if let Err(error) = poll_merges(&state, &config, &mut store, shadow) {
                    eprintln!("review merge poll failed: {error}");
                }
                merge_due = now + Duration::from_secs(config.intervals.merge_seconds);
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn discover(
    server: &Arc<Server>,
    config: &ReviewLoopConfig,
    store: &mut StateStore,
    shadow: bool,
) -> Result<(), String> {
    for policy in &config.repositories {
        for (number, _) in super::github_open_pull_requests(&policy.repository)? {
            let snapshot = fetch_pr(&policy.repository, number)?;
            let key = round_key(&policy.repository, number, &snapshot.head_ref_oid);
            if store.state.rounds.contains_key(&key) {
                continue;
            }
            // The first observed head gets the full-round budget. Every later
            // head for the same PR is a verification round, regardless of
            // whether the change landed before the prior verdict poll.
            let verification =
                store.state.rounds.values().any(|round| {
                    round.repository == policy.repository && round.pull_request == number
                });
            let mut superseded_agents = Vec::new();
            for round in store.state.rounds.values_mut().filter(|round| {
                round.repository == policy.repository
                    && round.pull_request == number
                    && !matches!(round.phase, RoundPhase::Merged | RoundPhase::Superseded)
            }) {
                round.phase = RoundPhase::Superseded;
                superseded_agents.extend(
                    round
                        .reviewers
                        .values()
                        .filter_map(|reviewer| reviewer.agent_id.clone()),
                );
            }
            let owner = find_owner_context(&policy.repository, &snapshot.head_ref_name);
            store.state.rounds.insert(
                key.clone(),
                ReviewRound {
                    repository: policy.repository.clone(),
                    pull_request: number,
                    head: snapshot.head_ref_oid.clone(),
                    branch: snapshot.head_ref_name.clone(),
                    verification,
                    phase: RoundPhase::Dispatching,
                    reviewers: BTreeMap::new(),
                    verdicts: BTreeMap::new(),
                    owner,
                    owner_task_id: None,
                    owner_agent_id: None,
                    gate_reasons: Vec::new(),
                },
            );
            store.save()?;
            if !shadow {
                kill_agents(server, &superseded_agents);
            }
            if shadow {
                emit_decision(server, &key, "shadow_round_observed", true, Vec::new())?;
                continue;
            }
            dispatch_missing_reviewers(server, policy, store, &key)?;
        }
    }
    Ok(())
}

fn dispatch_missing_reviewers(
    server: &Arc<Server>,
    policy: &RepositoryPolicy,
    store: &mut StateStore,
    key: &str,
) -> Result<(), String> {
    let (repository, number, head, verification) = {
        let round = store
            .state
            .rounds
            .get(key)
            .ok_or_else(|| "review round disappeared".to_owned())?;
        (
            round.repository.clone(),
            round.pull_request,
            round.head.clone(),
            round.verification,
        )
    };
    let maximum = if verification {
        policy.verification_rounds_max
    } else {
        policy.full_rounds_max
    };
    if maximum == 0 {
        let round = store.state.rounds.get_mut(key).expect("round exists");
        round.phase = RoundPhase::Attention;
        round.gate_reasons = vec!["review policy allows zero rounds".to_owned()];
        store.save()?;
        return Ok(());
    }
    for lens in &policy.lenses {
        let attempt = store
            .state
            .rounds
            .get(key)
            .and_then(|round| round.reviewers.get(lens))
            .map_or(1, |reviewer| reviewer.attempt + 1);
        if attempt > maximum {
            continue;
        }
        let task_id = reviewer_task_id(&repository, number, &head, lens, attempt);
        {
            let round = store.state.rounds.get_mut(key).expect("round exists");
            let active = round
                .reviewers
                .get(lens)
                .and_then(|reviewer| reviewer.agent_id.as_deref())
                .and_then(|agent_id| server.supervisor.registry.get(agent_id))
                .is_some_and(|agent| matches!(agent.state.as_str(), "running" | "orphaned"));
            if round.verdicts.contains_key(lens) || active {
                continue;
            }
            round.reviewers.insert(
                lens.clone(),
                ReviewerState {
                    lens: lens.clone(),
                    attempt,
                    task_id: task_id.clone(),
                    agent_id: None,
                },
            );
            store.save()?;
        }
        let prompt = reviewer_prompt(&repository, number, &head, lens);
        let project_dir = review_workspace(&task_id)?;
        let agent_id = spawn_codex_task(server, &task_id, &project_dir, None, &prompt)?;
        let round = store.state.rounds.get_mut(key).expect("round exists");
        if let Some(reviewer) = round.reviewers.get_mut(lens) {
            reviewer.agent_id = Some(agent_id);
        }
        round.phase = RoundPhase::Reviewing;
        store.save()?;
    }
    Ok(())
}

fn poll_reviews(
    server: &Arc<Server>,
    config: &ReviewLoopConfig,
    store: &mut StateStore,
    shadow: bool,
) -> Result<(), String> {
    let keys: Vec<_> = store
        .state
        .rounds
        .iter()
        .filter(|(_, round)| {
            matches!(
                round.phase,
                RoundPhase::Dispatching
                    | RoundPhase::Reviewing
                    | RoundPhase::Findings
                    | RoundPhase::Attention
            )
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        let (repository, number, expected_head) = {
            let round = &store.state.rounds[&key];
            (
                round.repository.clone(),
                round.pull_request,
                round.head.clone(),
            )
        };
        let Some(policy) = config
            .repositories
            .iter()
            .find(|policy| policy.repository == repository)
        else {
            continue;
        };
        let snapshot = fetch_pr(&repository, number)?;
        if snapshot.head_ref_oid != expected_head {
            let agent_ids: Vec<_> = store.state.rounds[&key]
                .reviewers
                .values()
                .filter_map(|reviewer| reviewer.agent_id.clone())
                .collect();
            store
                .state
                .rounds
                .get_mut(&key)
                .expect("round exists")
                .phase = RoundPhase::Superseded;
            store.save()?;
            if !shadow {
                kill_agents(server, &agent_ids);
            }
            continue;
        }
        let verdicts = latest_verdicts(policy, &expected_head, &snapshot.comments);
        {
            let round = store.state.rounds.get_mut(&key).expect("round exists");
            round.verdicts = verdicts;
        }
        store.save()?;
        let changes: Vec<_> = store.state.rounds[&key]
            .verdicts
            .iter()
            .filter(|(_, verdict)| verdict.verdict == Verdict::ChangesRequested)
            .map(|(lens, verdict)| (lens.clone(), verdict.findings.clone()))
            .collect();
        if !changes.is_empty() {
            store
                .state
                .rounds
                .get_mut(&key)
                .expect("round exists")
                .phase = RoundPhase::Findings;
            store.save()?;
            emit_decision(
                server,
                &key,
                "review_findings",
                shadow,
                changes
                    .iter()
                    .flat_map(|(lens, findings)| {
                        findings
                            .iter()
                            .map(move |finding| format!("[{lens}] {finding}"))
                    })
                    .collect(),
            )?;
            if !shadow {
                resume_owner(server, store, &key, &changes)?;
            }
            continue;
        }
        let all_approved = policy.lenses.iter().all(|lens| {
            store.state.rounds[&key]
                .verdicts
                .get(lens)
                .is_some_and(|verdict| verdict.verdict == Verdict::Approve)
        });
        if all_approved {
            let decision = evaluate_gate(
                policy,
                &expected_head,
                &snapshot,
                &store.state.rounds[&key].verdicts,
            );
            {
                let round = store.state.rounds.get_mut(&key).expect("round exists");
                round.gate_reasons = decision.reasons.clone();
                if decision.ready {
                    round.phase = RoundPhase::Ready;
                }
            }
            store.save()?;
            emit_decision(
                server,
                &key,
                if decision.ready {
                    "review_ready"
                } else {
                    "review_gate_waiting"
                },
                shadow,
                decision.reasons,
            )?;
        } else if !shadow {
            dispatch_missing_reviewers(server, policy, store, &key)?;
        }
    }
    Ok(())
}

fn poll_merges(
    server: &Arc<Server>,
    config: &ReviewLoopConfig,
    store: &mut StateStore,
    shadow: bool,
) -> Result<(), String> {
    let keys: Vec<_> = store
        .state
        .rounds
        .iter()
        .filter(|(_, round)| !matches!(round.phase, RoundPhase::Merged | RoundPhase::Superseded))
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        let (repository, number) = {
            let round = &store.state.rounds[&key];
            (round.repository.clone(), round.pull_request)
        };
        if !config
            .repositories
            .iter()
            .any(|policy| policy.repository == repository)
        {
            continue;
        }
        let snapshot = fetch_pr(&repository, number)?;
        if snapshot.merged_at.is_none() && !snapshot.state.eq_ignore_ascii_case("merged") {
            continue;
        }
        let agent_ids: Vec<_> = store.state.rounds[&key]
            .reviewers
            .values()
            .filter_map(|reviewer| reviewer.agent_id.clone())
            .collect();
        store
            .state
            .rounds
            .get_mut(&key)
            .expect("round exists")
            .phase = RoundPhase::Merged;
        store.save()?;
        if !shadow {
            kill_agents(server, &agent_ids);
        }
        emit_decision(server, &key, "review_merged", shadow, Vec::new())?;
    }
    Ok(())
}

fn fetch_pr(repository: &str, number: u64) -> Result<PullRequestSnapshot, String> {
    let args = vec![
        "pr".to_owned(),
        "view".to_owned(),
        number.to_string(),
        "--repo".to_owned(),
        repository.to_owned(),
        "--json".to_owned(),
        "headRefOid,headRefName,state,mergedAt,comments,statusCheckRollup".to_owned(),
    ];
    let result = run_allowed("gh", args, format!("review-pr-{repository}-{number}"))?;
    serde_json::from_str(&result)
        .map_err(|error| format!("GitHub PR response was not expected JSON: {error}"))
}

fn run_allowed(bin: &str, args: Vec<String>, id: String) -> Result<String, String> {
    let policy = exec::load_policy()?;
    let path = policy
        .allowed_path(bin, &args)
        .ok_or_else(|| format!("execution policy does not allow {bin}"))?;
    let result = exec::run(
        path,
        exec::ExecRequest {
            id,
            bin: bin.to_owned(),
            args,
        },
    );
    if result.timed_out || result.truncated || result.exit_code != Some(0) {
        return Err(format!("{bin} did not complete successfully"));
    }
    Ok(result.stdout)
}

fn latest_verdicts(
    policy: &RepositoryPolicy,
    head: &str,
    comments: &[Comment],
) -> BTreeMap<String, AdmittedVerdict> {
    let marker = Regex::new(r"^> 🤖 Codex \(AI assistant\) — \[([a-z]+)\] review verdict$")
        .expect("verdict marker regex is valid");
    let mut candidates: BTreeMap<String, (String, String, Option<AdmittedVerdict>)> =
        BTreeMap::new();
    for comment in comments {
        if comment.author.as_ref().map(|author| author.login.as_str())
            != Some(policy.trusted_verdict_identity.as_str())
        {
            continue;
        }
        let Some(lens) = comment
            .body
            .lines()
            .next()
            .and_then(|line| marker.captures(line))
            .and_then(|captures| captures.get(1))
            .map(|lens| lens.as_str().to_owned())
        else {
            continue;
        };
        if !policy.lenses.contains(&lens) {
            continue;
        }
        let parsed =
            parse_verdict(&comment.body, head, &policy.result_limits).map(|(_, verdict)| {
                AdmittedVerdict {
                    created_at: comment.created_at.clone(),
                    comment_id: comment.id.clone(),
                    ..verdict
                }
            });
        let replace = candidates.get(&lens).is_none_or(|(created_at, id, _)| {
            (&comment.created_at, &comment.id) > (created_at, id)
        });
        if replace {
            candidates.insert(
                lens,
                (comment.created_at.clone(), comment.id.clone(), parsed),
            );
        }
    }
    candidates
        .into_iter()
        .filter_map(|(lens, (_, _, verdict))| verdict.map(|verdict| (lens, verdict)))
        .collect()
}

fn parse_verdict(
    body: &str,
    expected_head: &str,
    limits: &ResultLimits,
) -> Option<(String, AdmittedVerdict)> {
    if body.len() > limits.max_bytes_per_lens {
        return None;
    }
    let lines: Vec<_> = body.lines().collect();
    if lines.len() < 5 || lines[1] != format!("RESULT-VERSION: {RESULT_VERSION}") {
        return None;
    }
    let marker = Regex::new(r"^> 🤖 Codex \(AI assistant\) — \[([a-z]+)\] review verdict$")
        .expect("verdict marker regex is valid");
    let lens = marker.captures(lines[0])?.get(1)?.as_str().to_owned();
    let verdict = match lines[2] {
        "VERDICT: APPROVE" => Verdict::Approve,
        "VERDICT: CHANGES REQUESTED" => Verdict::ChangesRequested,
        _ => return None,
    };
    let head = lines[3].strip_prefix("HEAD: ")?.to_ascii_lowercase();
    if head.len() != 40
        || !head.bytes().all(|byte| byte.is_ascii_hexdigit())
        || head != expected_head.to_ascii_lowercase()
        || lines[4] != "FINDINGS:"
    {
        return None;
    }
    let findings: Vec<_> = lines[5..]
        .iter()
        .map(|line| line.strip_prefix("- ").map(str::to_owned))
        .collect::<Option<_>>()?;
    if findings.len() > limits.max_findings_per_lens
        || findings.iter().any(|finding| finding.is_empty())
        || (verdict == Verdict::Approve && !findings.is_empty())
        || (verdict == Verdict::ChangesRequested && findings.is_empty())
    {
        return None;
    }
    Some((
        lens,
        AdmittedVerdict {
            verdict,
            head,
            findings,
            created_at: String::new(),
            comment_id: String::new(),
        },
    ))
}

fn evaluate_gate(
    policy: &RepositoryPolicy,
    head: &str,
    snapshot: &PullRequestSnapshot,
    verdicts: &BTreeMap<String, AdmittedVerdict>,
) -> GateDecision {
    let mut reasons = Vec::new();
    for lens in &policy.lenses {
        match verdicts.get(lens) {
            None => reasons.push(format!("no admitted [{lens}] verdict")),
            Some(verdict) if verdict.head != head => {
                reasons.push(format!("[{lens}] verdict is for a stale head"))
            }
            Some(verdict) if verdict.verdict != Verdict::Approve => {
                reasons.push(format!("[{lens}] verdict requests changes"))
            }
            Some(_) => {}
        }
    }
    for required in &policy.required_ci_checks {
        let pattern = match Regex::new(&format!("(?i:{})", required.name_pattern)) {
            Ok(pattern) => pattern,
            Err(_) => {
                reasons.push(format!("invalid configured CI pattern: {}", required.label));
                continue;
            }
        };
        let matches: Vec<_> = snapshot
            .status_check_rollup
            .iter()
            .filter(|check| pattern.is_match(&check.name))
            .collect();
        if matches.is_empty() {
            reasons.push(format!("required check missing: {}", required.label));
        }
        for check in matches {
            if !check
                .status
                .as_deref()
                .is_some_and(|status| status.eq_ignore_ascii_case("completed"))
                || !check
                    .conclusion
                    .as_deref()
                    .is_some_and(|conclusion| conclusion.eq_ignore_ascii_case("success"))
            {
                reasons.push(format!("required check not green: {}", check.name));
            }
        }
    }
    GateDecision {
        ready: reasons.is_empty(),
        reasons,
    }
}

fn resume_owner(
    server: &Arc<Server>,
    store: &mut StateStore,
    key: &str,
    findings: &[(String, Vec<String>)],
) -> Result<(), String> {
    let (repository, number, head, owner, already_dispatched) = {
        let round = &store.state.rounds[key];
        (
            round.repository.clone(),
            round.pull_request,
            round.head.clone(),
            round.owner.clone(),
            round.owner_agent_id.is_some(),
        )
    };
    if already_dispatched {
        return Ok(());
    }
    let Some(owner) = owner else {
        let round = store.state.rounds.get_mut(key).expect("round exists");
        round.phase = RoundPhase::Attention;
        round.gate_reasons = vec!["could not resolve the owning Codex session".to_owned()];
        store.save()?;
        return Ok(());
    };
    let source_task_id = format!("codex-{}", owner.department_task_id);
    if server
        .supervisor
        .registry
        .list(None, Some(&source_task_id))
        .iter()
        .any(|agent| {
            agent.state == "running"
                || (agent.state == "orphaned" && super::process_group_running(agent.process_group))
        })
    {
        return Ok(());
    }
    let task_id = owner_task_id(&repository, number, &head);
    store
        .state
        .rounds
        .get_mut(key)
        .expect("round exists")
        .owner_task_id = Some(task_id.clone());
    store.save()?;
    let prompt = owner_prompt(&repository, number, &head, findings);
    let agent_id = spawn_codex_task(
        server,
        &task_id,
        &owner.project_dir,
        Some(&owner.session_id),
        &prompt,
    )?;
    store
        .state
        .rounds
        .get_mut(key)
        .expect("round exists")
        .owner_agent_id = Some(agent_id);
    store.save()
}

fn spawn_codex_task(
    server: &Arc<Server>,
    task_id: &str,
    project_dir: &Path,
    session_id: Option<&str>,
    prompt: &str,
) -> Result<String, String> {
    if let Some(existing) = server
        .supervisor
        .registry
        .list(None, Some(task_id))
        .into_iter()
        .max_by(|left, right| left.started_at.cmp(&right.started_at))
    {
        return Ok(existing.id);
    }
    let root = home_dir()
        .ok_or_else(|| "HOME is not set".to_owned())?
        .join(".zigzag/review-tasks")
        .join(task_id);
    fs::create_dir_all(&root).map_err(|error| format!("could not create review task: {error}"))?;
    fs::create_dir_all(project_dir)
        .map_err(|error| format!("could not create review workspace: {error}"))?;
    write_private(root.join("prompt.txt"), prompt.as_bytes())?;
    write_private(
        root.join("dir.txt"),
        project_dir.to_string_lossy().as_bytes(),
    )?;
    if let Some(session_id) = session_id {
        write_private(root.join("resume.txt"), session_id.as_bytes())?;
    }
    let args = vec![
        if session_id.is_some() {
            "resume".to_owned()
        } else {
            "run".to_owned()
        },
        root.to_string_lossy().into_owned(),
    ];
    let policy = exec::load_policy()?;
    let path = policy
        .allowed_path(REVIEWER_BIN, &args)
        .ok_or_else(|| "execution policy does not allow codex-launch reviews".to_owned())?;
    let execution_id = new_execution_id()?;
    server.store.add(relay_event(
        "relay_request_started",
        task_id,
        &execution_id,
        Json::Object(vec![]),
    ))?;
    server.store.add(relay_event(
        "relay_accepted",
        task_id,
        &execution_id,
        Json::Object(vec![]),
    ))?;
    let spawned = spawn_proc(
        &server.supervisor,
        Arc::clone(&server.store),
        path,
        exec::ExecRequest {
            id: task_id.to_owned(),
            bin: REVIEWER_BIN.to_owned(),
            args,
        },
        execution_id,
    )?;
    Ok(spawned.handle)
}

fn write_private(path: PathBuf, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("could not create review task input: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("could not write review task input: {error}"))?;
    file.sync_data()
        .map_err(|error| format!("could not sync review task input: {error}"))
}

fn emit_decision(
    server: &Arc<Server>,
    key: &str,
    kind: &str,
    shadow: bool,
    reasons: Vec<String>,
) -> Result<(), String> {
    let execution = format!("review-{}", stable_identifier(key, 96));
    server
        .store
        .add(relay_event(
            kind,
            key,
            &execution,
            Json::Object(vec![
                ("shadow".to_owned(), Json::Bool(shadow)),
                (
                    "reasons".to_owned(),
                    Json::Array(reasons.into_iter().map(Json::String).collect()),
                ),
            ]),
        ))
        .map(|_| ())
}

fn find_owner_context(repository: &str, branch: &str) -> Option<OwnerContext> {
    let department = home_dir()?.join(".codex/dept");
    let mut candidates = Vec::new();
    for entry in fs::read_dir(department).ok()?.flatten() {
        let task_dir = entry.path();
        let department_task_id = entry.file_name().to_string_lossy().into_owned();
        let Some(project_dir) = fs::read_to_string(task_dir.join("dir.txt")).ok() else {
            continue;
        };
        let project_dir = PathBuf::from(project_dir.trim());
        if !project_dir.is_dir()
            || git_output(&project_dir, &["branch", "--show-current"]).as_deref() != Some(branch)
        {
            continue;
        }
        if !git_matches_repository(&project_dir, repository) {
            continue;
        }
        let Some(session_id) = fs::read_to_string(task_dir.join("resume.txt"))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| session_from_events(&task_dir.join("events.jsonl")))
        else {
            continue;
        };
        let Some(modified) = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
        else {
            continue;
        };
        candidates.push((
            modified,
            OwnerContext {
                session_id,
                project_dir,
                department_task_id,
            },
        ));
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates.pop().map(|(_, owner)| owner)
}

fn git_output(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_matches_repository(directory: &Path, repository: &str) -> bool {
    let Some(remote) = git_output(directory, &["remote", "get-url", "origin"]) else {
        return false;
    };
    let normalized = remote
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .replace(':', "/");
    normalized.ends_with(&format!("/{repository}"))
}

fn session_from_events(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for pointer in ["/thread_id", "/thread/id", "/payload/id", "/session_id"] {
            if let Some(session) = value.pointer(pointer).and_then(Value::as_str)
                && !session.is_empty()
            {
                return Some(session.to_owned());
            }
        }
    }
    None
}

fn kill_agents(server: &Server, agent_ids: &[String]) {
    for agent_id in agent_ids {
        if let Some(agent) = server.supervisor.registry.get(agent_id)
            && matches!(agent.state.as_str(), "running" | "orphaned")
        {
            let _ = kill_process_group(agent.process_group);
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn review_workspace(task_id: &str) -> Result<PathBuf, String> {
    home_dir()
        .map(|home| home.join(".zigzag/review-workspaces").join(task_id))
        .ok_or_else(|| "HOME is not set".to_owned())
}

fn reviewer_prompt(repository: &str, number: u64, head: &str, lens: &str) -> String {
    format!(
        "Review {repository} PR #{number} at exact head {head} through the {lens} lens. Read-only: never push, edit, merge, or formally approve. Review only this exact head. Post one top-level PR comment under the trusted GitHub identity in exactly this versioned format:\n\n> 🤖 Codex (AI assistant) — [{lens}] review verdict\nRESULT-VERSION: 1\nVERDICT: APPROVE | CHANGES REQUESTED\nHEAD: {head}\nFINDINGS:\n- one concise actionable finding per line\n\nFor APPROVE, leave FINDINGS: empty. For CHANGES REQUESTED, include 1–5 concrete findings. Do not add prose outside the format."
    )
}

fn owner_prompt(
    repository: &str,
    number: u64,
    head: &str,
    findings: &[(String, Vec<String>)],
) -> String {
    let findings = findings
        .iter()
        .flat_map(|(lens, findings)| {
            findings
                .iter()
                .map(move |finding| format!("- [{lens}] {finding}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Resume work on {repository} PR #{number}. The review loop admitted the following structured findings for head {head}:\n\n{findings}\n\nVerify and address every finding, add or update tests, push the same PR branch, and wait for the new head's independent review round. Do not reuse approvals from {head}."
    )
}

fn round_key(repository: &str, number: u64, head: &str) -> String {
    format!("{repository}#{number}@{}", head.to_ascii_lowercase())
}

fn reviewer_task_id(repository: &str, number: u64, head: &str, lens: &str, attempt: u64) -> String {
    format!(
        "review-{}-{number}-{}-{lens}-{attempt}",
        stable_identifier(repository, 40),
        stable_fragment(head, 12)
    )
}

fn owner_task_id(repository: &str, number: u64, head: &str) -> String {
    format!(
        "review-owner-{}-{number}-{}",
        stable_identifier(repository, 40),
        stable_fragment(head, 12)
    )
}

fn stable_fragment(value: &str, maximum: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(maximum)
        .collect()
}

fn stable_identifier(value: &str, maximum: usize) -> String {
    // This deterministic suffix is an idempotency aid, not a security
    // boundary. It keeps sanitized or truncated repository names distinct.
    let hash = value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    let prefix_length = maximum.saturating_sub(17);
    format!("{}-{hash:016x}", stable_fragment(value, prefix_length))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_yaml(enabled: bool) -> String {
        format!(
            r#"schema_version: 1
review_loop:
  enabled: {enabled}
  intervals:
    discovery_seconds: 300
    review_seconds: 600
    merge_seconds: 300
  repositories:
    - repository: ShukantPal/zigzag
      full_rounds_max: 2
      verification_rounds_max: 2
      lenses: [correctness, simplicity, tests, security]
      require_security_lens: true
      required_ci_checks:
        - label: semgrep
          name_pattern: semgrep
        - label: BuildBuddy
          name_pattern: buildbuddy
      trusted_verdict_identity: ShukantPal
      result_limits:
        max_findings_per_lens: 20
        max_bytes_per_lens: 16384
"#
        )
    }

    fn load_text(text: &str) -> Result<PersonalConfig, Vec<ConfigViolation>> {
        let path = std::env::temp_dir().join(format!(
            "zigzag-config-{}.yaml",
            super::super::random_hex_128().unwrap()
        ));
        fs::write(&path, text).unwrap();
        let result = load_config(&path);
        let _ = fs::remove_file(path);
        result
    }

    fn policy() -> RepositoryPolicy {
        load_text(&valid_yaml(true))
            .unwrap()
            .review_loop
            .repositories
            .remove(0)
    }

    fn snapshot(head: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            head_ref_oid: head.to_owned(),
            head_ref_name: "codex/branch".to_owned(),
            state: "OPEN".to_owned(),
            merged_at: None,
            comments: Vec::new(),
            status_check_rollup: vec![
                Check {
                    name: "Semgrep".to_owned(),
                    conclusion: Some("SUCCESS".to_owned()),
                    status: Some("COMPLETED".to_owned()),
                },
                Check {
                    name: "BuildBuddy / test".to_owned(),
                    conclusion: Some("SUCCESS".to_owned()),
                    status: Some("COMPLETED".to_owned()),
                },
            ],
        }
    }

    #[test]
    fn config_schema_accepts_appendix_b_and_rejects_unknowns_tags_duplicates_and_bad_regex() {
        let config = load_text(&valid_yaml(true)).unwrap();
        assert!(config.review_loop.enabled);
        for invalid in [
            valid_yaml(true).replace("  enabled: true", "  enabled: true\n  surprise: 1"),
            valid_yaml(true).replace("name_pattern: semgrep", "name_pattern: !custom semgrep"),
            valid_yaml(true).replace("name_pattern: semgrep", "name_pattern: '[unterminated'"),
            valid_yaml(true).replace("  enabled: true", "  enabled: true\n  enabled: false"),
        ] {
            assert!(
                load_text(&invalid).is_err(),
                "accepted invalid YAML: {invalid}"
            );
        }
    }

    #[test]
    fn config_reports_paths_and_semantic_repository_uniqueness() {
        let invalid = valid_yaml(true).replace("    merge_seconds: 300", "    merge_seconds: 10");
        let violations = load_text(&invalid).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|error| error.path == "review_loop.intervals.merge_seconds")
        );
        let duplicate = valid_yaml(true).replace(
            "        max_bytes_per_lens: 16384",
            "        max_bytes_per_lens: 16384\n    - repository: ShukantPal/zigzag\n      full_rounds_max: 2\n      verification_rounds_max: 2\n      lenses: [security]\n      require_security_lens: true\n      required_ci_checks: [{label: CI, name_pattern: ci}]\n      trusted_verdict_identity: ShukantPal\n      result_limits: {max_findings_per_lens: 1, max_bytes_per_lens: 1}",
        );
        let violations = load_text(&duplicate).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|error| error.reason.contains("duplicate repository"))
        );
    }

    #[test]
    fn verdict_parser_requires_version_trusted_shape_current_head_and_limits() {
        let head = "a".repeat(40);
        let limits = policy().result_limits;
        let approve = format!(
            "> 🤖 Codex (AI assistant) — [correctness] review verdict\nRESULT-VERSION: 1\nVERDICT: APPROVE\nHEAD: {head}\nFINDINGS:"
        );
        assert_eq!(
            parse_verdict(&approve, &head, &limits).unwrap().1.verdict,
            Verdict::Approve
        );
        assert!(
            parse_verdict(
                &approve.replace("RESULT-VERSION: 1", "RESULT-VERSION: 2"),
                &head,
                &limits
            )
            .is_none()
        );
        assert!(parse_verdict(&approve.replace(&head, &"b".repeat(40)), &head, &limits).is_none());
        assert!(parse_verdict(&format!("{approve}\n- hidden finding"), &head, &limits).is_none());
        let changes = approve.replace("VERDICT: APPROVE", "VERDICT: CHANGES REQUESTED")
            + "\n- handle the error path";
        assert_eq!(
            parse_verdict(&changes, &head, &limits)
                .unwrap()
                .1
                .findings
                .len(),
            1
        );
    }

    #[test]
    fn latest_verdicts_ignore_untrusted_stale_and_oversized_comments() {
        let head = "a".repeat(40);
        let body = format!(
            "> 🤖 Codex (AI assistant) — [tests] review verdict\nRESULT-VERSION: 1\nVERDICT: APPROVE\nHEAD: {head}\nFINDINGS:"
        );
        let comments = vec![
            Comment {
                author: Some(Author {
                    login: "other".to_owned(),
                }),
                body: body.clone(),
                created_at: "2026-01-01".to_owned(),
                id: "1".to_owned(),
            },
            Comment {
                author: Some(Author {
                    login: "ShukantPal".to_owned(),
                }),
                body,
                created_at: "2026-01-02".to_owned(),
                id: "2".to_owned(),
            },
        ];
        let verdicts = latest_verdicts(&policy(), &head, &comments);
        assert_eq!(verdicts.len(), 1);
        assert!(verdicts.contains_key("tests"));

        let mut limited = policy();
        limited.result_limits.max_bytes_per_lens = 200;
        let mut with_oversized_latest = comments;
        with_oversized_latest.push(Comment {
            author: Some(Author {
                login: "ShukantPal".to_owned(),
            }),
            body: format!(
                "> 🤖 Codex (AI assistant) — [tests] review verdict\n{}",
                "x".repeat(201)
            ),
            created_at: "2026-01-03".to_owned(),
            id: "3".to_owned(),
        });
        assert!(latest_verdicts(&limited, &head, &with_oversized_latest).is_empty());
    }

    #[test]
    fn gate_requires_every_current_head_approval_and_every_matching_ci_check() {
        let policy = policy();
        let head = "a".repeat(40);
        let mut verdicts = BTreeMap::new();
        for lens in &policy.lenses {
            verdicts.insert(
                lens.clone(),
                AdmittedVerdict {
                    verdict: Verdict::Approve,
                    head: head.clone(),
                    findings: Vec::new(),
                    created_at: String::new(),
                    comment_id: String::new(),
                },
            );
        }
        assert!(evaluate_gate(&policy, &head, &snapshot(&head), &verdicts).ready);
        verdicts.get_mut("tests").unwrap().head = "b".repeat(40);
        assert!(!evaluate_gate(&policy, &head, &snapshot(&head), &verdicts).ready);
        verdicts.get_mut("tests").unwrap().head = head.clone();
        let mut failed = snapshot(&head);
        failed.status_check_rollup.push(Check {
            name: "Semgrep shard 2".to_owned(),
            conclusion: Some("FAILURE".to_owned()),
            status: Some("COMPLETED".to_owned()),
        });
        assert!(!evaluate_gate(&policy, &head, &failed, &verdicts).ready);
    }

    #[test]
    fn durable_key_includes_repository_pr_and_exact_head() {
        let first = round_key("owner/repo", 7, &"a".repeat(40));
        let second = round_key("owner/repo", 7, &"b".repeat(40));
        assert_ne!(first, second);
        assert!(first.starts_with("owner/repo#7@"));
    }
}
