//! Policy-driven command execution in the Mac's GUI login session.
//!
//! The policy is intentionally stored in the login keychain rather than in a
//! Zigzag flag or environment variable. An SSH session cannot read or modify
//! that item, while the owner can update it with `zigzag config set-allowlist`.

use keyring::Entry;
use relay_core::{Json, parse_json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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

/// A binary pinned at policy load time: the symlink-resolved path plus the
/// file's (device, inode) identity. Re-checked before every spawn, so a
/// symlink swap or file replacement in the binary's directory cannot silently
/// redirect execution to an attacker-controlled file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BinaryIdentity {
    canonical: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BinPolicy {
    path: String,
    commands: Vec<Vec<String>>,
    gh_read_repos: BTreeSet<String>,
    /// `None` when the path did not resolve at load time; spawning is then
    /// refused outright instead of trusting whatever appears later.
    identity: Option<BinaryIdentity>,
}

/// Resolve the configured path once, at policy load: canonicalize away
/// symlinks and record the file's (device, inode) identity for later
/// re-verification.
fn resolve_binary_identity(path: &str) -> Option<BinaryIdentity> {
    let canonical = fs::canonicalize(path).ok()?;
    let metadata = fs::metadata(&canonical).ok()?;
    Some(BinaryIdentity {
        canonical,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Re-resolve and re-stat the binary, refusing to spawn unless the file
/// behind the policy path is the same one recorded at load time.
///
/// Errors deliberately never name the path: it is policy data and must not
/// leak to API callers. A residual check-then-spawn window remains (macOS
/// has no `openat2`-style symlink-safe exec); this shrinks the attack window
/// from the daemon's whole lifetime to the microseconds before spawn.
fn verify_binary_identity(identity: &BinaryIdentity) -> Result<PathBuf, String> {
    // A swapped symlink, or a new symlink component anywhere in the path,
    // changes what canonicalization resolves to.
    let canonical = fs::canonicalize(&identity.canonical)
        .map_err(|_| "configured binary could not be re-resolved".to_owned())?;
    if canonical != identity.canonical {
        return Err("configured binary path changed since policy load".to_owned());
    }
    // Same path, different file: replaced between policy load and exec.
    let metadata = fs::metadata(&canonical)
        .map_err(|_| "configured binary could not be re-read".to_owned())?;
    if metadata.dev() != identity.device || metadata.ino() != identity.inode {
        return Err("configured binary was replaced since policy load".to_owned());
    }
    Ok(canonical)
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

/// Why `Policy::verified_path` refused to produce a spawn path.
#[derive(Debug)]
pub enum VerifyError {
    /// The bin/args are not allowlisted. Answer with the opaque denial.
    Denied,
    /// The bin/args are allowlisted, but the binary failed integrity
    /// verification. A server-side failure: log the reason, report
    /// generically.
    Unverifiable(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Denied => write!(f, "command not allowlisted"),
            VerifyError::Unverifiable(reason) => write!(f, "{reason}"),
        }
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
                    identity: resolve_binary_identity(path),
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

    /// Pure allowlist predicate: does the policy allow these args? The
    /// returned path is the *configured* path, unverified. Anything that
    /// spawns must go through `verified_path` instead.
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

    /// Allowlist check plus binary integrity verification, for every spawn
    /// path. Returns the canonical binary path to hand to `Command`.
    ///
    /// `Denied` means the bin/args are not allowlisted: answer with the
    /// opaque denial. `Unverifiable` means they are allowlisted but the
    /// binary failed integrity verification: a server-side failure, reported
    /// as such without naming the path.
    pub fn verified_path(&self, bin: &str, args: &[String]) -> Result<PathBuf, VerifyError> {
        if self.allowed_path(bin, args).is_none() {
            return Err(VerifyError::Denied);
        }
        let identity = self
            .bins
            .get(bin)
            .and_then(|policy| policy.identity.as_ref())
            .ok_or_else(|| {
                VerifyError::Unverifiable(
                    "binary identity was not resolvable at policy load".to_owned(),
                )
            })?;
        verify_binary_identity(identity).map_err(VerifyError::Unverifiable)
    }

    /// Internal review-loop publication uses the same repository-scoped `gh`
    /// identity without exposing a general GitHub write prefix on `/v1/exec`.
    pub fn trusted_gh_path_for_repo(&self, repo: &str) -> Option<PathBuf> {
        let policy = self.bins.get("gh")?;
        if !policy.gh_read_repos.contains(repo) {
            return None;
        }
        policy
            .identity
            .as_ref()
            .and_then(|identity| verify_binary_identity(identity).ok())
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
        ["repos", owner, repo, "compare", comparison]
            if valid_github_name(owner)
                && valid_github_name(repo)
                && comparison
                    .split_once("...")
                    .is_some_and(|(base, head)| valid_git_oid(base) && valid_git_oid(head)) =>
        {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

fn valid_git_oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

pub fn run(path: &Path, request: ExecRequest) -> ExecResult {
    run_with_timeout(path, request, EXEC_TIMEOUT)
}

fn run_with_timeout(path: &Path, request: ExecRequest, timeout: Duration) -> ExecResult {
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

    // ------------------------------------------------------------------
    // gh allowlist parser hardening. `is_read_only_gh_command` /
    // `is_read_only_gh_api` are the only thing standing between the
    // allowlisted `gh` binary and a write, so the parser's deny-by-default
    // behavior is pinned here: edge-case tables plus deterministic
    // property-style sweeps (a PRNG with a fixed seed — reproducible "fuzz").
    // ------------------------------------------------------------------

    fn gh_repos() -> BTreeSet<String> {
        BTreeSet::from(["leveled-inc/leveled".to_owned()])
    }

    fn gh_argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn assert_gh_denied(argv: &[&str]) {
        assert!(
            !is_read_only_gh_command(&gh_argv(argv), &gh_repos()),
            "gh allowlist accepted a non-read-only command: {argv:?}"
        );
    }

    fn assert_gh_allowed(argv: &[&str]) {
        assert!(
            is_read_only_gh_command(&gh_argv(argv), &gh_repos()),
            "gh allowlist rejected a read-only command: {argv:?}"
        );
    }

    #[test]
    fn gh_write_attempts_are_denied_despite_matching_prefix() {
        // The allowlist prefix `[["api"]]` matches, but the read-only parser
        // is the backstop: a prefix match alone must never authorize.
        let policy = policy(
            r#"{"bins":{"gh":{"path":"/bin/echo","commands":[["api"]],"gh_read_repos":["leveled-inc/leveled"]}}}"#,
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &gh_argv(&[
                        "api",
                        "--method",
                        "POST",
                        "repos/leveled-inc/leveled/pulls/1/comments"
                    ])
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &gh_argv(&["api", "repos/leveled-inc/leveled/pulls/1/comments"])
                )
                .is_some()
        );
    }

    #[test]
    fn gh_api_rejects_every_write_capable_flag_in_every_position() {
        let endpoint = "repos/leveled-inc/leveled/pulls/1/comments";
        // Flags that can turn `gh api` into a write (method/body/headers) or
        // otherwise escape the read-only contract.
        let write_flags = [
            "--method",
            "-X",
            "--input",
            "-F",
            "--field",
            "-f",
            "--raw-field",
            "--magic-field",
            "--header",
            "-H",
            "--verbose",
            "-v",
            "--hostname",
            "--show-headers",
            "-i",
            "--insecure",
            "--request",
        ];
        for flag in write_flags {
            // Before the endpoint, after it, and behind an allowed flag.
            assert_gh_denied(&["api", flag, endpoint]);
            assert_gh_denied(&["api", endpoint, flag]);
            assert_gh_denied(&["api", "--paginate", flag, endpoint]);
            // With a plausible value attached.
            assert_gh_denied(&["api", flag, "GET", endpoint]);
            assert_gh_denied(&["api", flag, "x=y", endpoint]);
        }
        // Combined short flags and the `=` form are not allowlisted either.
        assert_gh_denied(&["api", "-sX", endpoint]);
        assert_gh_denied(&["api", "-abc", endpoint]);
        assert_gh_denied(&["api", "--jq=.foo", endpoint]);
        assert_gh_denied(&["api", "--silent=false", endpoint]);
    }

    #[test]
    fn gh_api_allows_only_the_documented_read_shapes() {
        let compare = format!(
            "repos/leveled-inc/leveled/compare/{}...{}",
            "a".repeat(40),
            "b".repeat(40)
        );
        let endpoints = [
            "repos/leveled-inc/leveled/pulls/1/comments",
            "repos/leveled-inc/leveled/issues/42/comments",
            "repos/leveled-inc/leveled/pulls/7/reviews",
            "repos/leveled-inc/leveled/pulls?state=open&per_page=100",
            compare.as_str(),
        ];
        for endpoint in endpoints {
            assert_gh_allowed(&["api", endpoint]);
            // Allowed flags in any position, alone and combined.
            assert_gh_allowed(&["api", "--paginate", endpoint]);
            assert_gh_allowed(&["api", endpoint, "--paginate", "--slurp"]);
            assert_gh_allowed(&["api", "--silent", "--include", endpoint]);
            assert_gh_allowed(&["api", "--jq", ".foo", endpoint]);
            assert_gh_allowed(&["api", "--template", "{{.x}}", endpoint]);
            assert_gh_allowed(&["api", "--cache", "1h", endpoint]);
            assert_gh_allowed(&[
                "api",
                "--paginate",
                "--jq",
                ".",
                "--slurp",
                endpoint,
                "--silent",
            ]);
        }
    }

    #[test]
    fn gh_api_value_flags_consume_but_never_reinterpret() {
        let endpoint = "repos/leveled-inc/leveled/pulls/1/comments";
        // A dangerous-looking token in *value* position is data, not a flag:
        // the single linear pass never re-parses it, so it cannot smuggle a
        // write.
        assert_gh_allowed(&["api", "--jq", "--method", endpoint]);
        assert_gh_allowed(&["api", "--template", "-X", endpoint]);
        assert_gh_allowed(&["api", "--cache", "--paginate", endpoint]);
        // ...but a missing value is a hard deny, not a skip.
        assert_gh_denied(&["api", "--jq"]);
        assert_gh_denied(&["api", endpoint, "--template"]);
        assert_gh_denied(&["api", "--cache"]);
    }

    #[test]
    fn gh_api_rejects_separators_empty_and_extra_positionals() {
        let endpoint = "repos/leveled-inc/leveled/pulls/1/comments";
        assert_gh_denied(&["api", "--", endpoint]);
        assert_gh_denied(&["api", "-", endpoint]);
        assert_gh_denied(&["api", "", endpoint]);
        assert_gh_denied(&["api", endpoint, endpoint]);
        assert_gh_denied(&["api", "--paginate", endpoint, "extra"]);
        assert_gh_denied(&["api"]);
        assert_gh_denied(&[]);
    }

    #[test]
    fn gh_api_rejects_malformed_endpoints() {
        let bad = [
            // Wrong resource shapes.
            "repos/leveled-inc/leveled/issues",
            "repos/leveled-inc/leveled/pulls",
            "repos/leveled-inc/leveled/pulls?state=open",
            "repos/leveled-inc/leveled/pulls?state=open&per_page=100&extra=1",
            "repos/leveled-inc/leveled/pulls/1",
            "repos/leveled-inc/leveled/pulls/1/files",
            "repos/leveled-inc/leveled/actions/runs",
            // Bad numbers and oids.
            "repos/leveled-inc/leveled/pulls/0/comments",
            "repos/leveled-inc/leveled/pulls/abc/comments",
            "repos/leveled-inc/leveled/pulls/1/comments/",
            "repos/leveled-inc/leveled/compare/main...head",
            "repos/leveled-inc/leveled/compare/xyz...abc",
            // Traversal / encoding tricks.
            "repos/leveled-inc/leveled/pulls/../pulls/1/comments",
            "repos/leveled-inc%2Fleveled/pulls/1/comments",
            "/repos/leveled-inc/leveled/pulls/1/comments",
            "repos//leveled/pulls/1/comments",
            // Valid shape, but the repo is not allowlisted (exact match).
            "repos/other-org/other-repo/pulls/1/comments",
            "repos/Leveled-Inc/Leveled/pulls/1/comments",
        ];
        for endpoint in bad {
            assert_gh_denied(&["api", endpoint]);
        }
        // ...while the exact documented shapes next to them are allowed.
        assert_gh_allowed(&["api", "repos/leveled-inc/leveled/pulls/1/comments"]);
        assert_gh_allowed(&[
            "api",
            "repos/leveled-inc/leveled/pulls?state=open&per_page=100",
        ]);
    }

    #[test]
    fn gh_pr_branch_edges() {
        let repo = "leveled-inc/leveled";
        // Happy paths.
        assert_gh_allowed(&["pr", "list", "--repo", repo]);
        assert_gh_allowed(&["pr", "view", "42", "--repo", repo]);
        assert_gh_allowed(&["pr", "checks", "--repo", repo]);
        assert_gh_allowed(&["pr", "list", &format!("--repo={repo}")]);
        // --web opens a browser: never allowlisted.
        assert_gh_denied(&["pr", "view", "42", "--repo", repo, "--web"]);
        assert_gh_denied(&["pr", "list", "--repo", repo, "--web=true"]);
        assert_gh_denied(&["pr", "list", "--repo", repo, "--web=1"]);
        // Repo scoping is exact and mandatory.
        assert_gh_denied(&["pr", "list"]);
        assert_gh_denied(&["pr", "list", "--repo", "other/repo"]);
        assert_gh_denied(&["pr", "list", "--repo", repo, "--repo", repo]);
        assert_gh_denied(&["pr", "list", "--repo"]);
        assert_gh_denied(&["pr", "list", "--repo="]);
        assert_gh_denied(&["pr", "list", "--repo", "--web"]);
        assert_gh_denied(&["pr", "list", "--repo", "not a repo"]);
        assert_gh_denied(&["pr", "list", "--repo", "ownér/repo"]);
        // Only the three read-only subcommands (case-sensitive).
        for sub in [
            "merge", "close", "comment", "edit", "create", "checkout", "diff", "status", "LIST",
        ] {
            assert_gh_denied(&["pr", sub, "--repo", repo]);
        }
        assert_gh_denied(&["pr"]);
        assert_gh_denied(&["issue", "list", "--repo", repo]);
        assert_gh_denied(&["repo", "view", "--repo", repo]);
    }

    #[test]
    fn gh_parser_handles_unicode_safely() {
        // Unicode never validates as a repo/endpoint name...
        assert_gh_denied(&["api", "repos/leveled-inc/leveled/pulls/1/cómments"]);
        assert_gh_denied(&["pr", "list", "--repo", "leveled-inc/levéléd"]);
        // ...but is harmless as a --jq/--template value (output formatting).
        assert_gh_allowed(&[
            "api",
            "--jq",
            ".títle",
            "repos/leveled-inc/leveled/pulls/1/comments",
        ]);
    }

    /// Deterministic PRNG so the sweep below is reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    /// Positions the parser treats as flags (not values swallowed by
    /// --jq/--template/--cache), per the documented grammar.
    fn flag_positions(argv: &[String]) -> Vec<usize> {
        let mut positions = Vec::new();
        let mut index = 0;
        while index < argv.len() {
            match argv[index].as_str() {
                "--jq" | "--template" | "--cache" => index += 2,
                _ => {
                    if argv[index].starts_with('-') {
                        positions.push(index);
                    }
                    index += 1;
                }
            }
        }
        positions
    }

    #[test]
    fn gh_api_fuzz_no_smuggled_flags_are_ever_accepted() {
        // The allowlisted flag set, spelled out. If the parser ever learns a
        // new flag, this test fails until the flag is justified here: flag
        // expansion on this parser is a security decision.
        const KNOWN_GOOD_FLAGS: &[&str] = &[
            "--paginate",
            "--slurp",
            "--silent",
            "--include",
            "--jq",
            "--template",
            "--cache",
        ];
        let tokens = [
            "--method",
            "-X",
            "--input",
            "-F",
            "--field",
            "-f",
            "--raw-field",
            "--header",
            "-H",
            "--verbose",
            "--paginate",
            "--slurp",
            "--silent",
            "--include",
            "--jq",
            "--template",
            "--cache",
            "--",
            "-",
            "",
            "--web",
            "GET",
            "POST",
            "-sX",
            "--jq=",
            "repos/leveled-inc/leveled/pulls/1/comments",
            "repos/leveled-inc/leveled/issues/9/comments",
            "repos/leveled-inc/leveled/pulls?state=open&per_page=100",
            "repos/other/repo/pulls/1/comments",
            "not-an-endpoint",
            "--repo",
            "o/r",
        ];
        let mut rng = Lcg(0x9E3779B97F4A7C15);
        for _ in 0..20_000 {
            let len = 1 + (rng.next() % 5) as usize;
            let argv: Vec<String> = (0..len)
                .map(|_| tokens[(rng.next() as usize) % tokens.len()].to_string())
                .collect();
            let mut full = vec!["api".to_string()];
            full.extend(argv.iter().cloned());
            if is_read_only_gh_command(&full, &gh_repos()) {
                // Anything the parser took as a flag must be a known-good one.
                for position in flag_positions(&argv) {
                    assert!(
                        KNOWN_GOOD_FLAGS.contains(&argv[position].as_str()),
                        "smuggled flag accepted: {argv:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn gh_api_fuzz_valid_shapes_are_always_accepted() {
        // Guards against over-blocking regressions: every grammar-valid
        // combination of allowed flags must stay accepted.
        let endpoints = [
            "repos/leveled-inc/leveled/pulls/1/comments",
            "repos/leveled-inc/leveled/issues/2/comments",
            "repos/leveled-inc/leveled/pulls/3/reviews",
            "repos/leveled-inc/leveled/pulls?state=open&per_page=100",
        ];
        let bare = ["--paginate", "--slurp", "--silent", "--include"];
        let valued = ["--jq", "--template", "--cache"];
        let values = ["x", ".foo", "--method", "-X", ""];
        let mut rng = Lcg(0xC2B280121);
        for _ in 0..5_000 {
            // Shuffle (flag, value) pairs as units: shuffling bare tokens
            // would divorce values from their flags and generate shapes the
            // grammar itself rejects.
            let mut items: Vec<Vec<String>> = Vec::new();
            for flag in bare {
                if rng.next().is_multiple_of(2) {
                    items.push(vec![flag.to_string()]);
                }
            }
            for flag in valued {
                if rng.next().is_multiple_of(3) {
                    items.push(vec![
                        flag.to_string(),
                        values[(rng.next() as usize) % values.len()].to_string(),
                    ]);
                }
            }
            // Fisher-Yates shuffle with the LCG.
            for i in (1..items.len()).rev() {
                let j = (rng.next() as usize) % (i + 1);
                items.swap(i, j);
            }
            let mut argv: Vec<String> = items.into_iter().flatten().collect();
            argv.push(endpoints[(rng.next() as usize) % endpoints.len()].to_string());
            // The endpoint may also lead, but never as a swallowed value.
            if rng.next().is_multiple_of(2) && argv.len() > 1 {
                let endpoint = argv.pop().unwrap();
                let mut position = (rng.next() as usize) % (argv.len() + 1);
                if position > 0
                    && matches!(
                        argv[position - 1].as_str(),
                        "--jq" | "--template" | "--cache"
                    )
                {
                    position = argv.len();
                }
                argv.insert(position, endpoint);
            }
            let mut full = vec!["api".to_string()];
            full.extend(argv.iter().cloned());
            assert!(
                is_read_only_gh_command(&full, &gh_repos()),
                "valid read-only shape rejected: {argv:?}"
            );
        }
    }

    fn temp_bin_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zigzag-sec02-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bin_policy_json(path: &Path) -> String {
        format!(
            r#"{{"bins":{{"ztool":{{"path":"{}","commands":[["run"]]}}}}}}"#,
            path.display()
        )
    }

    fn gh_policy_json(path: &Path) -> String {
        format!(
            r#"{{"bins":{{"gh":{{"path":"{}","commands":[["pr","list"],["api"]],"gh_read_repos":["leveled-inc/leveled"]}}}}}}"#,
            path.display()
        )
    }

    #[test]
    fn verified_path_canonicalizes_a_symlinked_binary_at_policy_load() {
        let dir = temp_bin_dir("symlink");
        let real = dir.join("real-bin");
        std::fs::write(&real, "v1").unwrap();
        let link = dir.join("link-bin");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let policy = policy(&bin_policy_json(&link));
        // The spawn path is the fully resolved binary, not the symlink.
        assert_eq!(
            policy.verified_path("ztool", &args(&["run"])).unwrap(),
            std::fs::canonicalize(&real).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verified_path_rejects_a_binary_replaced_after_policy_load() {
        let dir = temp_bin_dir("replaced");
        let bin = dir.join("bin");
        std::fs::write(&bin, "v1").unwrap();
        let policy = policy(&bin_policy_json(&bin));
        assert!(policy.verified_path("ztool", &args(&["run"])).is_ok());
        // Same path, new file: the recorded (device, inode) no longer matches.
        std::fs::remove_file(&bin).unwrap();
        std::fs::write(&bin, "v2").unwrap();
        assert!(policy.verified_path("ztool", &args(&["run"])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verified_path_survives_a_symlink_retarget_after_policy_load() {
        let dir = temp_bin_dir("retarget");
        let target_a = dir.join("target-a");
        let target_b = dir.join("target-b");
        std::fs::write(&target_a, "a").unwrap();
        std::fs::write(&target_b, "b").unwrap();
        let link = dir.join("link-bin");
        std::os::unix::fs::symlink(&target_a, &link).unwrap();
        let policy = policy(&bin_policy_json(&link));
        let pinned = std::fs::canonicalize(&target_a).unwrap();
        assert_eq!(
            policy.verified_path("ztool", &args(&["run"])).unwrap(),
            pinned
        );
        // Retargeting the symlink after load cannot redirect the spawn: the
        // policy pinned the resolved file at load, and the link is never
        // used again.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target_b, &link).unwrap();
        assert_eq!(
            policy.verified_path("ztool", &args(&["run"])).unwrap(),
            pinned
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verified_path_rejects_a_binary_missing_at_policy_load() {
        let dir = temp_bin_dir("missing");
        let policy = policy(&bin_policy_json(&dir.join("no-such-bin")));
        // Parse succeeds (the binary may be installed later), but spawning is
        // refused: there is no load-time identity to verify against.
        assert!(policy.verified_path("ztool", &args(&["run"])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verified_path_still_enforces_prefixes_and_unknown_bins() {
        let dir = temp_bin_dir("prefix");
        let bin = dir.join("bin");
        std::fs::write(&bin, "v1").unwrap();
        let policy = policy(&bin_policy_json(&bin));
        assert!(
            policy
                .verified_path("ztool", &args(&["run", "extra"]))
                .is_ok()
        );
        assert!(policy.verified_path("ztool", &args(&["stop"])).is_err());
        assert!(policy.verified_path("unknown", &args(&["run"])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_binary_identity_rejects_inode_and_path_mismatch() {
        let dir = temp_bin_dir("identity");
        let bin = dir.join("bin");
        std::fs::write(&bin, "v1").unwrap();
        let canonical = std::fs::canonicalize(&bin).unwrap();
        let metadata = std::fs::metadata(&canonical).unwrap();
        let good = BinaryIdentity {
            canonical: canonical.clone(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        assert!(verify_binary_identity(&good).is_ok());
        // Same path, different file.
        let tampered_inode = BinaryIdentity {
            inode: metadata.ino().wrapping_add(1),
            ..good.clone()
        };
        assert!(verify_binary_identity(&tampered_inode).is_err());
        // Same identity fields, but the path now resolves elsewhere.
        let other = dir.join("other");
        std::fs::write(&other, "other").unwrap();
        let tampered_path = BinaryIdentity {
            canonical: std::fs::canonicalize(&other).unwrap(),
            ..good.clone()
        };
        assert!(verify_binary_identity(&tampered_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trusted_gh_path_is_verified_before_it_is_returned() {
        let dir = temp_bin_dir("gh-trusted");
        let gh = dir.join("gh");
        std::fs::write(&gh, "fake").unwrap();
        let policy = policy(&gh_policy_json(&gh));
        assert_eq!(
            policy.trusted_gh_path_for_repo("leveled-inc/leveled"),
            Some(std::fs::canonicalize(&gh).unwrap())
        );
        assert_eq!(policy.trusted_gh_path_for_repo("other/repo"), None);
        // Replacing the binary after policy load invalidates the trust.
        std::fs::remove_file(&gh).unwrap();
        std::fs::write(&gh, "evil").unwrap();
        assert_eq!(policy.trusted_gh_path_for_repo("leveled-inc/leveled"), None);
        let _ = std::fs::remove_dir_all(&dir);
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
        let base = "a".repeat(40);
        let head = "b".repeat(40);
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        &format!("repos/leveled-inc/leveled/compare/{base}...{head}"),
                        "--jq",
                        ".files",
                    ]),
                )
                .is_some()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&["api", "repos/leveled-inc/leveled/compare/main...head"]),
                )
                .is_none()
        );
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&[
                        "api",
                        &format!("repos/leveled-inc/leveled/compare/main...{head}"),
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
                        &format!("repos/leveled-inc/leveled/compare/{base}...head"),
                    ]),
                )
                .is_none()
        );
        // The configured gh path does not exist on this machine, so the
        // verified lookup refuses it instead of returning a blind path.
        assert_eq!(policy.trusted_gh_path_for_repo("leveled-inc/leveled"), None);
        assert!(
            policy
                .allowed_path(
                    "gh",
                    &args(&["pr", "comment", "42", "--repo", "leveled-inc/leveled"]),
                )
                .is_none()
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
        let result = run_with_timeout(Path::new("/bin/echo"), request, Duration::from_secs(10));
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
            Path::new("/nonexistent/exec-test-binary"),
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
        let result = run_with_timeout(Path::new("/bin/sleep"), request, Duration::from_millis(300));
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
