//! Policy-driven command execution in the Mac's GUI login session.
//!
//! The policy is intentionally stored in the login keychain rather than in a
//! Zigzag flag or environment variable. An SSH session cannot read or modify
//! that item, while the owner can update it with `zigzag config set-allowlist`.

use keyring::Entry;
use relay_core::{Json, parse_json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const KEYCHAIN_SERVICE: &str = "zigzag";
const KEYCHAIN_ACCOUNT: &str = "exec-allowlist";
/// Upper bound for one synchronous execution; the HTTP client must allow a
/// slightly larger read timeout.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(300);
pub const OUTPUT_CAP: usize = 1024 * 1024;
const MAX_ARGS: usize = 64;
const MAX_ARGS_BYTES: usize = 8 * 1024;
const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    bins: BTreeMap<String, BinPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BinPolicy {
    path: String,
    commands: Vec<Vec<String>>,
    gh_read_repos: BTreeSet<String>,
}

pub struct ExecRequest {
    pub id: String,
    pub bin: String,
    pub args: Vec<String>,
}

pub struct ExecResult {
    pub id: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

impl ExecResult {
    pub fn to_json(&self) -> Json {
        Json::Object(vec![
            ("id".to_owned(), Json::String(self.id.clone())),
            (
                "exit_code".to_owned(),
                match self.exit_code {
                    Some(code) => Json::Number(code.to_string()),
                    None => Json::Null,
                },
            ),
            ("stdout".to_owned(), Json::String(self.stdout.clone())),
            ("stderr".to_owned(), Json::String(self.stderr.clone())),
            ("truncated".to_owned(), Json::Bool(self.truncated)),
            ("timed_out".to_owned(), Json::Bool(self.timed_out)),
        ])
    }
}

impl Policy {
    pub fn parse(text: &str) -> Result<Self, String> {
        let json = parse_json(text).map_err(|error| format!("invalid policy JSON: {error}"))?;
        let fields = object_fields(&json, "policy")?;
        require_only(fields, &["bins"], "policy")?;
        let bins = match field(fields, "bins") {
            Some(Json::Object(bins)) if !bins.is_empty() => bins,
            _ => return Err("policy bins must be a non-empty object".to_owned()),
        };
        let mut parsed = BTreeMap::new();
        for (name, value) in bins {
            if !valid_bin_name(name) {
                return Err(format!("invalid binary name: {name}"));
            }
            if parsed.contains_key(name) {
                return Err(format!("duplicate binary name: {name}"));
            }
            let fields = object_fields(value, "binary policy")?;
            require_allowed(
                fields,
                &["path", "commands", "gh_read_repos"],
                "binary policy",
            )?;
            let path = field(fields, "path")
                .and_then(Json::as_str)
                .filter(|path| !path.is_empty() && Path::new(path).is_absolute())
                .ok_or_else(|| format!("binary {name} path must be absolute and non-empty"))?;
            let commands = match field(fields, "commands") {
                Some(Json::Array(commands)) if !commands.is_empty() => commands,
                _ => return Err(format!("binary {name} commands must be a non-empty array")),
            };
            let mut parsed_commands = Vec::with_capacity(commands.len());
            for command in commands {
                let Json::Array(prefix) = command else {
                    return Err(format!("binary {name} command prefix must be an array"));
                };
                if prefix.is_empty() {
                    return Err(format!("binary {name} command prefix must not be empty"));
                }
                let prefix = prefix
                    .iter()
                    .map(|argument| argument.as_str().filter(|argument| !argument.is_empty()))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        format!("binary {name} command prefixes contain only non-empty strings")
                    })?
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                parsed_commands.push(prefix);
            }
            let gh_read_repos = match field(fields, "gh_read_repos") {
                None => BTreeSet::new(),
                Some(Json::Array(repos)) if !repos.is_empty() => repos
                    .iter()
                    .map(Json::as_str)
                    .collect::<Option<Vec<_>>>()
                    .filter(|repos| repos.iter().all(|repo| valid_github_repo(repo)))
                    .ok_or_else(|| {
                        format!("binary {name} gh_read_repos must contain GitHub OWNER/REPO names")
                    })?
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                _ => {
                    return Err(format!(
                        "binary {name} gh_read_repos must be a non-empty array"
                    ));
                }
            };
            if name == "gh" && gh_read_repos.is_empty() {
                return Err("binary gh requires a non-empty gh_read_repos array".to_owned());
            }
            if name != "gh" && !gh_read_repos.is_empty() {
                return Err(format!("binary {name} may not set gh_read_repos"));
            }
            parsed.insert(
                name.clone(),
                BinPolicy {
                    path: path.to_owned(),
                    commands: parsed_commands,
                    gh_read_repos,
                },
            );
        }
        Ok(Self { bins: parsed })
    }

    /// Stable, compact JSON is what is stored in the keychain.
    pub fn canonical_json(&self) -> String {
        let bins = self
            .bins
            .iter()
            .map(|(name, policy)| {
                let mut fields = vec![
                    ("path".to_owned(), Json::String(policy.path.clone())),
                    (
                        "commands".to_owned(),
                        Json::Array(
                            policy
                                .commands
                                .iter()
                                .map(|prefix| {
                                    Json::Array(prefix.iter().cloned().map(Json::String).collect())
                                })
                                .collect(),
                        ),
                    ),
                ];
                if !policy.gh_read_repos.is_empty() {
                    fields.push((
                        "gh_read_repos".to_owned(),
                        Json::Array(
                            policy
                                .gh_read_repos
                                .iter()
                                .cloned()
                                .map(Json::String)
                                .collect(),
                        ),
                    ));
                }
                (name.clone(), Json::Object(fields))
            })
            .collect();
        Json::Object(vec![("bins".to_owned(), Json::Object(bins))]).to_json()
    }

    pub fn allowed_path(&self, bin: &str, args: &[String]) -> Option<&str> {
        let policy = self.bins.get(bin)?;
        (policy.commands.iter().any(|prefix| args.starts_with(prefix))
            // `gh api` defaults to GET, but its flexible flags can otherwise
            // turn a superficially read-only allowlist prefix into a write.
            // Keep the policy useful for per-PR reads while making the
            // read-only property enforceable by this process.
            && (bin != "gh" || is_read_only_gh_command(args, &policy.gh_read_repos)))
        .then_some(policy.path.as_str())
    }
}

fn is_read_only_gh_command(args: &[String], repos: &BTreeSet<String>) -> bool {
    match args {
        [command, subcommand, rest @ ..]
            if command == "pr" && matches!(subcommand.as_str(), "list" | "view" | "checks") =>
        {
            gh_repo_argument(rest).is_some_and(|repo| repos.contains(repo))
                && !rest
                    .iter()
                    .any(|argument| argument == "--web" || argument.starts_with("--web="))
        }
        [command, rest @ ..] if command == "api" => is_read_only_gh_api(rest, repos),
        _ => false,
    }
}

/// `gh api` has write-capable flags which may appear before or after the
/// endpoint. Only permit the three REST resources a PR watchdog needs, and a
/// small set of output-only flags. With no method/body flags, `gh api` uses
/// its GET default.
fn is_read_only_gh_api(args: &[String], repos: &BTreeSet<String>) -> bool {
    let mut endpoint = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "--paginate" | "--slurp" | "--silent" | "--include" => index += 1,
            "--jq" | "--template" | "--cache" => {
                if index + 1 == args.len() {
                    return false;
                }
                index += 2;
            }
            _ if argument.starts_with('-') => return false,
            _ if endpoint.replace(argument.as_str()).is_some() => return false,
            _ => index += 1,
        }
    }
    endpoint
        .and_then(pr_watchdog_read_endpoint_repo)
        .is_some_and(|repo| repos.contains(&repo))
}

fn gh_repo_argument(args: &[String]) -> Option<&str> {
    let mut repo = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let candidate = if argument == "--repo" {
            index += 1;
            args.get(index).map(String::as_str)
        } else {
            argument.strip_prefix("--repo=")
        };
        if let Some(candidate) = candidate
            && (repo.replace(candidate).is_some() || !valid_github_repo(candidate))
        {
            return None;
        }
        index += 1;
    }
    repo
}

fn pr_watchdog_read_endpoint_repo(endpoint: &str) -> Option<String> {
    let segments: Vec<_> = endpoint
        .split('?')
        .next()
        .unwrap_or("")
        .split('/')
        .collect();
    match segments.as_slice() {
        ["repos", owner, repo, "issues", number, "comments"]
        | ["repos", owner, repo, "pulls", number, "comments"]
        | ["repos", owner, repo, "pulls", number, "reviews"]
            if valid_github_name(owner)
                && valid_github_name(repo)
                && number.parse::<u64>().is_ok_and(|number| number > 0) =>
        {
            Some(format!("{owner}/{repo}"))
        }
        ["repos", owner, repo, "pulls"]
            if valid_github_name(owner)
                && valid_github_name(repo)
                && endpoint.ends_with("?state=open&per_page=100") =>
        {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

fn valid_github_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_github_repo(value: &str) -> bool {
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    valid_github_name(owner) && valid_github_name(repo) && !repo.contains('/')
}

fn object_fields<'a>(json: &'a Json, name: &str) -> Result<&'a [(String, Json)], String> {
    match json {
        Json::Object(fields) => Ok(fields),
        _ => Err(format!("{name} must be an object")),
    }
}

fn field<'a>(fields: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    fields
        .iter()
        .find(|(field_name, _)| field_name == name)
        .map(|(_, value)| value)
}

fn require_only(fields: &[(String, Json)], allowed: &[&str], name: &str) -> Result<(), String> {
    if fields
        .iter()
        .any(|(field_name, _)| !allowed.contains(&field_name.as_str()))
    {
        return Err(format!("{name} contains an unknown field"));
    }
    if fields.len() != allowed.len()
        || allowed
            .iter()
            .any(|field_name| field(fields, field_name).is_none())
    {
        return Err(format!("{name} is missing a required field"));
    }
    if fields
        .iter()
        .enumerate()
        .any(|(index, (name, _))| fields[..index].iter().any(|(previous, _)| previous == name))
    {
        return Err(format!("{name} contains a duplicate field"));
    }
    Ok(())
}

fn require_allowed(fields: &[(String, Json)], allowed: &[&str], name: &str) -> Result<(), String> {
    if fields
        .iter()
        .any(|(field_name, _)| !allowed.contains(&field_name.as_str()))
        || fields.iter().enumerate().any(|(index, (field_name, _))| {
            fields[..index]
                .iter()
                .any(|(previous, _)| previous == field_name)
        })
    {
        return Err(format!("{name} contains an unknown or duplicate field"));
    }
    Ok(())
}

fn valid_bin_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub fn parse_request(body: &Json) -> Result<ExecRequest, String> {
    let fields = object_fields(body, "request")?;
    require_only(fields, &["id", "bin", "args"], "request")?;
    let id = field(fields, "id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
        .ok_or_else(|| "request id must be a non-empty string of at most 128 bytes".to_owned())?;
    let bin = field(fields, "bin")
        .and_then(Json::as_str)
        .filter(|bin| valid_bin_name(bin))
        .ok_or_else(|| "request bin must be a valid binary name".to_owned())?;
    let args = match field(fields, "args") {
        Some(Json::Array(args)) if args.len() <= MAX_ARGS => args
            .iter()
            .map(|arg| arg.as_str().filter(|arg| !arg.is_empty()))
            .collect::<Option<Vec<_>>>(),
        _ => None,
    }
    .ok_or_else(|| "request args must contain at most 64 non-empty strings".to_owned())?;
    if args.iter().map(|arg| arg.len()).sum::<usize>() > MAX_ARGS_BYTES {
        return Err("request args exceed 8 KiB".to_owned());
    }
    Ok(ExecRequest {
        id: id.to_owned(),
        bin: bin.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
    })
}

/// Return a safe identifier for opaque denials. Invalid/missing IDs must not
/// cause a detailed parsing error to be exposed.
pub fn request_id(body: &Json) -> String {
    body.object("id")
        .and_then(Json::as_str)
        .filter(|id| id.len() <= MAX_ID_BYTES)
        .unwrap_or_default()
        .to_owned()
}

pub fn load_policy() -> Result<Policy, String> {
    let entry = keychain_entry()?;
    let policy = entry
        .get_password()
        .map_err(|error| format!("could not read exec allowlist from keychain: {error}"))?;
    Policy::parse(&policy).map_err(|error| format!("invalid exec allowlist in keychain: {error}"))
}

pub fn store_policy(policy: &Policy) -> Result<(), String> {
    keychain_entry()?
        .set_password(&policy.canonical_json())
        .map_err(|error| format!("could not write exec allowlist to keychain: {error}"))
}

fn keychain_entry() -> Result<Entry, String> {
    Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|error| format!("could not access exec allowlist keychain item: {error}"))
}

pub fn run(path: &str, request: ExecRequest) -> ExecResult {
    run_with_timeout(path, request, EXEC_TIMEOUT)
}

fn run_with_timeout(path: &str, request: ExecRequest, timeout: Duration) -> ExecResult {
    let mut child = match Command::new(path)
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ExecResult {
                id: request.id,
                exit_code: None,
                stdout: String::new(),
                // The path is policy data. Do not reveal it to callers even
                // when an allowed command cannot be spawned.
                stderr: format!("could not spawn configured binary: {error}"),
                truncated: false,
                timed_out: false,
            };
        }
    };
    // Drain both pipes on separate threads so a chatty child can never block
    // on a full pipe while the parent is waiting for it to exit.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(|| drain_limited(stdout));
    let stderr_reader = thread::spawn(|| drain_limited(stderr));
    let start = Instant::now();
    let mut exit_code = None;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break false;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                eprintln!("exec child wait failed: {error}");
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
        }
    };
    let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or_default();
    ExecResult {
        id: request.id,
        exit_code: if timed_out { None } else { exit_code },
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
        timed_out,
    }
}

fn drain_limited(mut pipe: impl Read) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                // Past the cap, keep draining into the void so the child is
                // never blocked on a full pipe.
                if kept.len() < OUTPUT_CAP {
                    let room = OUTPUT_CAP - kept.len();
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    if n > room {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (kept, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(text: &str) -> Policy {
        Policy::parse(text).unwrap()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn policy_accepts_and_canonicalizes_a_valid_schema() {
        let parsed = policy(
            r#"{"bins":{"z-tool":{"path":"/opt/z","commands":[["run"]]},"jules":{"path":"/Users/shukant/.npm-global/bin/jules","commands":[["new"],["remote","list"],["remote","pull"],["remote","new"]]}}}"#,
        );
        assert_eq!(
            parsed.canonical_json(),
            r#"{"bins":{"jules":{"path":"/Users/shukant/.npm-global/bin/jules","commands":[["new"],["remote","list"],["remote","pull"],["remote","new"]]},"z-tool":{"path":"/opt/z","commands":[["run"]]}}}"#
        );
    }

    #[test]
    fn policy_rejects_bad_schemas() {
        for text in [
            r#"{}"#,
            r#"{"bins":{}}"#,
            r#"{"bins":{"Jules":{"path":"/bin/echo","commands":[["x"]]}}}"#,
            r#"{"bins":{"jules":{"path":"relative/jules","commands":[["x"]]}}}"#,
            r#"{"bins":{"jules":{"path":"","commands":[["x"]]}}}"#,
            r#"{"bins":{"jules":{"path":"/bin/echo","commands":[]}}}"#,
            r#"{"bins":{"jules":{"path":"/bin/echo","commands":[[]]}}}"#,
            r#"{"bins":{"jules":{"path":"/bin/echo","commands":[[""]]}}}"#,
            r#"{"bins":{"jules":{"path":"/bin/echo","commands":[[7]]}}}"#,
        ] {
            assert!(Policy::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn argv_prefix_matching_is_exact_and_rejects_login_logout_and_unknown_bins() {
        let policy = policy(
            r#"{"bins":{"jules":{"path":"/bin/echo","commands":[["new"],["remote","list"],["remote","pull"],["remote","new"]]}}}"#,
        );
        assert_eq!(
            policy.allowed_path("jules", &args(&["new", "--repo", "a/b"])),
            Some("/bin/echo")
        );
        assert_eq!(
            policy.allowed_path("jules", &args(&["remote", "list", "--repo"])),
            Some("/bin/echo")
        );
        assert_eq!(policy.allowed_path("jules", &args(&["remote"])), None);
        assert_eq!(
            policy.allowed_path("jules", &args(&["remote", "delete"])),
            None
        );
        assert_eq!(policy.allowed_path("jules", &args(&["login"])), None);
        assert_eq!(policy.allowed_path("jules", &args(&["logout"])), None);
        assert_eq!(policy.allowed_path("unknown", &args(&["new"])), None);
    }

    #[test]
    fn gh_policy_permits_only_pr_reads_and_safe_api_reads() {
        let policy = policy(
            r#"{"bins":{"gh":{"path":"/opt/homebrew/bin/gh","commands":[["pr","list"],["pr","view"],["pr","checks"],["api"]],"gh_read_repos":["leveled-inc/leveled"]}}}"#,
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&["pr", "checks", "42", "--repo", "leveled-inc/leveled"]),
                )
                .is_some()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        "--paginate",
                        "--slurp",
                        "repos/leveled-inc/leveled/pulls?state=open&per_page=100",
                    ]),
                )
                .is_some()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        "repos/leveled-inc/leveled/issues/42/comments",
                        "--paginate",
                    ]),
                )
                .is_some()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        "--jq",
                        ".[]",
                        "repos/leveled-inc/leveled/pulls/42/reviews"
                    ]),
                )
                .is_some()
        );
        assert!(
            policy
                .allowed_path("gh", &args(&["pr", "view", "42", "--web"]))
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        "repos/leveled-inc/leveled/issues/42/comments",
                        "--method=POST",
                    ]),
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        "repos/leveled-inc/leveled/issues/42/comments",
                        "--raw-field=x",
                    ]),
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "pr",
                        "view",
                        "42",
                        "--repo=leveled-inc/leveled",
                        "--web=true"
                    ]),
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&["pr", "view", "42", "--repo", "other-org/private"]),
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        "repos/leveled-inc/leveled/issues/42/comments",
                        "--method",
                        "POST",
                    ]),
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&["api", "repos/leveled-inc/leveled/issues/42/reactions"])
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path("gh", &args(&["issue", "close", "42"]))
                .is_none()
        );
    }

    #[test]
    fn request_validation_rejects_empty_strings_and_large_arguments() {
        let valid = Json::Object(vec![
            ("id".to_owned(), Json::String("req-1".to_owned())),
            ("bin".to_owned(), Json::String("jules".to_owned())),
            (
                "args".to_owned(),
                Json::Array(vec![Json::String("new".to_owned())]),
            ),
        ]);
        assert!(parse_request(&valid).is_ok());
        let empty = Json::Object(vec![
            ("id".to_owned(), Json::String("req-1".to_owned())),
            ("bin".to_owned(), Json::String("jules".to_owned())),
            (
                "args".to_owned(),
                Json::Array(vec![Json::String("".to_owned())]),
            ),
        ]);
        assert!(parse_request(&empty).is_err());
        let too_long = Json::Object(vec![
            ("id".to_owned(), Json::String("req-1".to_owned())),
            ("bin".to_owned(), Json::String("jules".to_owned())),
            (
                "args".to_owned(),
                Json::Array(vec![Json::String("x".repeat(MAX_ARGS_BYTES + 1))]),
            ),
        ]);
        assert!(parse_request(&too_long).is_err());
    }

    #[test]
    fn request_validation_enforces_argument_and_id_byte_boundaries() {
        let make_request = |id: String, argument_count: usize| {
            Json::Object(vec![
                ("id".to_owned(), Json::String(id)),
                ("bin".to_owned(), Json::String("jules".to_owned())),
                (
                    "args".to_owned(),
                    Json::Array(
                        (0..argument_count)
                            .map(|_| Json::String("x".to_owned()))
                            .collect(),
                    ),
                ),
            ])
        };
        assert!(parse_request(&make_request("x".repeat(MAX_ID_BYTES), MAX_ARGS)).is_ok());
        assert!(parse_request(&make_request("x".repeat(MAX_ID_BYTES + 1), MAX_ARGS)).is_err());
        assert!(
            parse_request(&make_request("x".repeat(MAX_ID_BYTES - 1) + "é", MAX_ARGS)).is_err()
        );
        assert!(parse_request(&make_request("request".to_owned(), MAX_ARGS + 1)).is_err());
    }

    #[test]
    fn execution_captures_output_and_exit_code() {
        let request = ExecRequest {
            id: "echo-1".to_owned(),
            bin: "echo".to_owned(),
            args: args(&["new"]),
        };
        let result = run_with_timeout("/bin/echo", request, Duration::from_secs(10));
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "new\n");
        assert!(!result.timed_out);
        assert!(!result.truncated);
    }

    #[test]
    fn execution_reports_a_missing_binary_without_panicking() {
        let request = ExecRequest {
            id: "missing-1".to_owned(),
            bin: "missing".to_owned(),
            args: args(&["new"]),
        };
        let result = run_with_timeout(
            "/nonexistent/exec-test-binary",
            request,
            Duration::from_secs(10),
        );
        assert_eq!(result.exit_code, None);
        assert!(result.stderr.contains("could not spawn"));
        assert!(!result.stderr.contains("/nonexistent/exec-test-binary"));
        assert!(!result.timed_out);
    }

    #[test]
    fn execution_kills_a_child_that_outlives_the_timeout() {
        let request = ExecRequest {
            id: "sleep-1".to_owned(),
            bin: "sleep".to_owned(),
            args: args(&["30"]),
        };
        let start = Instant::now();
        let result = run_with_timeout("/bin/sleep", request, Duration::from_millis(300));
        assert!(result.timed_out);
        assert_eq!(result.exit_code, None);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn drain_limited_caps_output_but_keeps_draining() {
        let big: Vec<u8> = (0..OUTPUT_CAP + 100).map(|i| (i % 251) as u8).collect();
        let (kept, truncated) = drain_limited(&big[..]);
        assert_eq!(kept.len(), OUTPUT_CAP);
        assert!(truncated);
        let (kept, truncated) = drain_limited(&b"hello"[..]);
        assert_eq!(kept, b"hello");
        assert!(!truncated);
    }
}
