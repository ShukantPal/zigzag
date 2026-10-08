//! Verified, in-process relay updates.
//!
//! A GitHub Release is discovery only.  The release manifest is checked before
//! a candidate is made current and `gh attestation verify` is constrained by
//! the repository, workflow, source ref, and the bundled Sigstore trust root.

use relay_core::{Json, parse_json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const REPOSITORY: &str = "ShukantPal/zigzag";
const WORKFLOW: &str =
    "https://github.com/ShukantPal/zigzag/.github/workflows/release.yml@refs/heads/main";
const TARGET: &str = "aarch64-apple-darwin";
const BINARY: &str = "zigzag-macos-aarch64";
const MANIFEST: &str = "zigzag-macos-aarch64.manifest.json";
const TRUST_ROOT: &str = include_str!("../trust/sigstore-trusted-root.json");
const TEAM_ID: &str = "NH5F3PDHQ8";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    Enabled,
    Paused,
    Pin(String),
}

impl Policy {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "enabled" => Ok(Self::Enabled),
            "paused" => Ok(Self::Paused),
            _ => value
                .strip_prefix("pin:")
                .filter(|version| valid_version(version))
                .map(|version| Self::Pin(version.to_owned()))
                .ok_or_else(|| {
                    "update policy must be enabled, paused, or pin:<version>".to_owned()
                }),
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Self::Enabled => "enabled".to_owned(),
            Self::Paused => "paused".to_owned(),
            Self::Pin(version) => format!("pin:{version}"),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub directory: PathBuf,
    pub interval: Duration,
    pub policy: Policy,
    pub ready_file: Option<PathBuf>,
}

pub struct Manager {
    config: Config,
    draining: Arc<AtomicBool>,
}
type ActiveWork = dyn Fn() -> bool + Send + Sync;
type Audit = dyn Fn(&str, Json) + Send + Sync;
type WatchdogArgs = (PathBuf, PathBuf, PathBuf, PathBuf, u16, Vec<String>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub policy: Policy,
    pub accepted_version: Option<String>,
    pub last_check: Option<String>,
    pub last_result: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Manifest {
    version: String,
    target: String,
    sha256: String,
    commit: String,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            draining: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    pub fn status(&self) -> Status {
        if !status_path(&self.config.directory).exists() {
            return Status {
                policy: self.config.policy.clone(),
                accepted_version: None,
                last_check: None,
                last_result: None,
            };
        }
        read_status(&self.config.directory).unwrap_or_else(|_| Status {
            policy: self.config.policy.clone(),
            accepted_version: None,
            last_check: None,
            last_result: Some("state_unavailable".to_owned()),
        })
    }

    /// Called only after listeners are bound in the replacement process.
    pub fn acknowledge_ready(&self) -> Result<(), String> {
        let Some(path) = &self.config.ready_file else {
            return Ok(());
        };
        atomic_write(path, b"ready\n")?;
        let current = self.config.directory.join("current");
        let current =
            fs::canonicalize(current).map_err(|_| "current release is unavailable".to_owned())?;
        replace_symlink(&current, &self.config.directory.join("last-known-good"))?;
        let mut status = self.status();
        status.last_result = Some("applied".to_owned());
        save_status(&self.config.directory, &status)?;
        Ok(())
    }

    pub fn start(
        self: Arc<Self>,
        active_work: Arc<ActiveWork>,
        audit: Arc<Audit>,
        original_args: Vec<String>,
        secret_file: PathBuf,
        health_port: u16,
    ) {
        if self.config.interval.is_zero() {
            return;
        }
        std::thread::spawn(move || {
            loop {
                if let Err(error) = self.check_and_apply(
                    active_work.as_ref(),
                    audit.as_ref(),
                    &original_args,
                    &secret_file,
                    health_port,
                ) {
                    eprintln!("relay update check failed: {error}");
                }
                std::thread::sleep(self.config.interval);
            }
        });
    }

    fn check_and_apply(
        &self,
        active_work: &dyn Fn() -> bool,
        audit: &dyn Fn(&str, Json),
        original_args: &[String],
        secret_file: &Path,
        health_port: u16,
    ) -> Result<(), String> {
        let mut status = self.status();
        if matches!(status.policy, Policy::Paused) {
            status.last_check = Some(relay_core::rfc3339_timestamp());
            status.last_result = Some("paused".to_owned());
            save_status(&self.config.directory, &status)?;
            return Ok(());
        }
        status.last_check = Some(relay_core::rfc3339_timestamp());
        audit("relay_update_started", Json::Object(vec![]));
        let result = self.fetch_candidate(&status)?;
        let Some((manifest, candidate)) = result else {
            status.last_result = Some("no_new_release".to_owned());
            save_status(&self.config.directory, &status)?;
            return Ok(());
        };
        if matches!(&status.policy, Policy::Pin(version) if version != &manifest.version) {
            status.last_result = Some("not_pinned_version".to_owned());
            save_status(&self.config.directory, &status)?;
            return Ok(());
        }
        audit(
            "relay_update_started",
            update_payload(
                &status.accepted_version,
                &manifest.version,
                &manifest.sha256,
            ),
        );
        // Set this before the drain so simultaneous authenticated spawns are
        // rejected, while reads and event delivery remain available.
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        while active_work() {
            std::thread::sleep(Duration::from_millis(200));
        }
        let apply = self.activate_and_exec(
            &manifest,
            &candidate,
            original_args,
            secret_file,
            health_port,
        );
        if let Err(error) = &apply {
            self.draining.store(false, Ordering::Release);
            status.last_result = Some(format!("apply_failed:{error}"));
            save_status(&self.config.directory, &status)?;
            audit(
                "relay_update_failed",
                update_payload(
                    &status.accepted_version,
                    &manifest.version,
                    &manifest.sha256,
                ),
            );
        }
        apply
    }

    fn fetch_candidate(&self, status: &Status) -> Result<Option<(Manifest, PathBuf)>, String> {
        let release = command_output(
            "curl",
            [
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "https://api.github.com/repos/ShukantPal/zigzag/releases/latest",
            ],
        )?;
        let release =
            parse_json(&release).map_err(|_| "invalid GitHub release metadata".to_owned())?;
        let tag = release
            .object("tag_name")
            .and_then(Json::as_str)
            .filter(|v| valid_version(v))
            .ok_or_else(|| "release has invalid version".to_owned())?;
        if status
            .accepted_version
            .as_deref()
            .is_some_and(|old| version_cmp(tag, old).is_le())
        {
            return Ok(None);
        }
        let asset_url = release_asset_url(&release, MANIFEST)?;
        let manifest_text = command_output(
            "curl",
            [
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                &asset_url,
            ],
        )?;
        let manifest = parse_manifest(&manifest_text)?;
        if manifest.version != tag || manifest.target != TARGET {
            return Err("release manifest version or target does not match".to_owned());
        }
        let binary_url = release_asset_url(&release, BINARY)?;
        let candidate_dir = self
            .config
            .directory
            .join("versions")
            .join(&manifest.version);
        fs::create_dir_all(&candidate_dir)
            .map_err(|e| format!("could not create update directory: {e}"))?;
        let candidate = candidate_dir.join("zigzag");
        download(&binary_url, &candidate)?;
        if sha256_file(&candidate)? != manifest.sha256 {
            let _ = fs::remove_file(&candidate);
            return Err("release binary digest does not match manifest".to_owned());
        }
        verify_codesign(&candidate)?;
        verify_attestation(&candidate, &manifest.commit, &self.config.directory)?;
        Ok(Some((manifest, candidate)))
    }

    fn activate_and_exec(
        &self,
        manifest: &Manifest,
        candidate: &Path,
        original_args: &[String],
        secret_file: &Path,
        health_port: u16,
    ) -> Result<(), String> {
        let previous = self.ensure_last_known_good()?;
        let current = self.config.directory.join("current");
        replace_symlink(candidate, &current)?;
        let ready = self.config.directory.join("candidate-ready");
        let _ = fs::remove_file(&ready);
        let mut args = original_args.to_vec();
        args.push("--update-ready-file".to_owned());
        args.push(ready.to_string_lossy().into_owned());
        let watchdog = std::env::current_exe().map_err(|e| e.to_string())?;
        Command::new(&watchdog)
            .arg("update-watchdog")
            .arg("--ready-file")
            .arg(&ready)
            .arg("--rollback")
            .arg(&previous)
            .arg("--current-link")
            .arg(&current)
            .arg("--secret-file")
            .arg(secret_file)
            .arg("--port")
            .arg(health_port.to_string())
            .arg("--")
            .args(&args)
            .spawn()
            .map_err(|e| format!("could not start update watchdog: {e}"))?;
        let mut status = self.status();
        status.accepted_version = Some(manifest.version.clone());
        status.last_result = Some("exec_pending_health_check".to_owned());
        save_status(&self.config.directory, &status)?;
        // Replacing this process preserves the LaunchAgent GUI session and its
        // Keychain access.  Never bootstrap/bootout the agent here.
        let error = Command::new(candidate)
            .env("ZIGZAG_UPDATE_VERSION", &manifest.version)
            .args(args)
            .exec();
        Err(format!("could not exec verified candidate: {error}"))
    }

    fn ensure_last_known_good(&self) -> Result<PathBuf, String> {
        let lkg = self.config.directory.join("last-known-good");
        if lkg.exists() {
            return fs::canonicalize(lkg).map_err(|e| e.to_string());
        }
        let bootstrap = self.config.directory.join("bootstrap").join("zigzag");
        if !bootstrap.exists() {
            fs::create_dir_all(bootstrap.parent().expect("parent"))
                .map_err(|e| format!("could not initialize rollback directory: {e}"))?;
            fs::copy(
                std::env::current_exe().map_err(|e| e.to_string())?,
                &bootstrap,
            )
            .map_err(|e| format!("could not preserve rollback binary: {e}"))?;
            fs::set_permissions(&bootstrap, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        replace_symlink(&bootstrap, &lkg)?;
        Ok(bootstrap)
    }
}

pub fn run_control(arguments: &[String]) -> Result<(), String> {
    let (directory, command) = parse_control(arguments)?;
    let mut status = read_status(&directory)?;
    match command.as_slice() {
        [command] if command == "status" => {}
        [command] if command == "pause" => status.policy = Policy::Paused,
        [command] if command == "resume" => status.policy = Policy::Enabled,
        [command, version] if command == "pin" && valid_version(version) => {
            status.policy = Policy::Pin(version.to_owned())
        }
        [command] if command == "unpin" => status.policy = Policy::Enabled,
        _ => {
            return Err(
                "usage: zigzag updates --dir PATH (status|pause|resume|pin VERSION|unpin)"
                    .to_owned(),
            );
        }
    }
    save_status(&directory, &status)?;
    println!("policy={}", status.policy.as_str());
    if let Some(version) = status.accepted_version {
        println!("accepted_version={version}");
    }
    if let Some(check) = status.last_check {
        println!("last_check={check}");
    }
    if let Some(result) = status.last_result {
        println!("last_result={result}");
    }
    Ok(())
}

pub fn run_watchdog(arguments: &[String]) -> Result<(), String> {
    let (ready, rollback, current_link, secret_file, port, relay_args) = parse_watchdog(arguments)?;
    let token =
        fs::read_to_string(secret_file).map_err(|_| "could not read watchdog token".to_owned())?;
    for _ in 0..60 {
        if ready.exists() && healthy(port, token.trim()) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!("candidate did not become healthy; rolling back");
    replace_symlink(&rollback, &current_link)?;
    let error = Command::new(rollback).args(relay_args).exec();
    Err(format!("could not exec rollback binary: {error}"))
}

fn release_asset_url(release: &Json, expected: &str) -> Result<String, String> {
    let Json::Array(assets) = release
        .object("assets")
        .ok_or_else(|| "release has no assets".to_owned())?
    else {
        return Err("release assets are invalid".to_owned());
    };
    assets
        .iter()
        .find_map(|asset| {
            (asset.object("name").and_then(Json::as_str) == Some(expected))
                .then(|| asset.object("browser_download_url").and_then(Json::as_str))
                .flatten()
                .map(str::to_owned)
        })
        .ok_or_else(|| format!("release is missing {expected}"))
}

fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let value = parse_json(text).map_err(|_| "manifest is not JSON".to_owned())?;
    let required = |name: &str| {
        value
            .object(name)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("manifest lacks {name}"))
    };
    let manifest = Manifest {
        version: required("version")?,
        target: required("target")?,
        sha256: required("sha256")?,
        commit: required("commit")?,
    };
    if !valid_version(&manifest.version)
        || !valid_sha256(&manifest.sha256)
        || !valid_commit(&manifest.commit)
    {
        return Err("manifest contains invalid fields".to_owned());
    }
    Ok(manifest)
}

fn verify_codesign(candidate: &Path) -> Result<(), String> {
    command_status(
        "codesign",
        [
            "--verify",
            "--strict",
            "--deep",
            "--verbose=2",
            &candidate.to_string_lossy(),
        ],
    )?;
    let output = Command::new("codesign")
        .args(["-d", "--verbose=4", &candidate.to_string_lossy()])
        .output()
        .map_err(|e| e.to_string())?;
    let detail = String::from_utf8_lossy(&output.stderr);
    if !detail.contains("Identifier=com.shukantpal.zigzag")
        || !detail.contains(&format!("TeamIdentifier={TEAM_ID}"))
    {
        return Err("candidate does not have the expected code-signing identity".to_owned());
    }
    Ok(())
}

fn verify_attestation(candidate: &Path, commit: &str, directory: &Path) -> Result<(), String> {
    let root = directory.join("sigstore-trusted-root.json");
    if !root.exists() {
        atomic_write(&root, TRUST_ROOT.as_bytes())?;
    }
    command_status(
        "gh",
        [
            "attestation",
            "verify",
            &candidate.to_string_lossy(),
            "--repo",
            REPOSITORY,
            "--signer-workflow",
            WORKFLOW,
            "--source-ref",
            "refs/heads/main",
            "--source-digest",
            commit,
            "--predicate-type",
            "https://slsa.dev/provenance/v1",
            "--custom-trusted-root",
            &root.to_string_lossy(),
        ],
    )
}

fn download(url: &str, output: &Path) -> Result<(), String> {
    let temporary = output.with_extension("download");
    let _ = fs::remove_file(&temporary);
    command_status(
        "curl",
        [
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
            &temporary.to_string_lossy(),
            url,
        ],
    )?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    fs::rename(temporary, output).map_err(|e| e.to_string())
}

fn command_output<'a>(
    command: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, String> {
    let output = Command::new(command)
        .args(args)
        .output()
        .map_err(|e| format!("could not run {command}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{command} failed"));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{command} returned non-UTF-8 output"))
}
fn command_status<'a>(
    command: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let status = Command::new(command)
        .args(args)
        .status()
        .map_err(|e| format!("could not run {command}: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{command} verification failed"))
}

fn update_payload(old: &Option<String>, new: &str, digest: &str) -> Json {
    Json::Object(vec![
        (
            "old_version".to_owned(),
            old.clone().map(Json::String).unwrap_or(Json::Null),
        ),
        ("new_version".to_owned(), Json::String(new.to_owned())),
        ("sha256".to_owned(), Json::String(digest.to_owned())),
    ])
}

fn parse_control(arguments: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    let Some((flag, directory, rest)) = arguments
        .split_first()
        .and_then(|(flag, rest)| rest.split_first().map(|(dir, rest)| (flag, dir, rest)))
    else {
        return Err("usage: zigzag updates --dir PATH COMMAND".to_owned());
    };
    if flag != "--dir" || directory.is_empty() {
        return Err("usage: zigzag updates --dir PATH COMMAND".to_owned());
    }
    Ok((PathBuf::from(directory), rest.to_vec()))
}
fn parse_watchdog(arguments: &[String]) -> Result<WatchdogArgs, String> {
    let mut values = arguments.iter();
    let mut ready = None;
    let mut rollback = None;
    let mut current_link = None;
    let mut secret = None;
    let mut port = None;
    while let Some(arg) = values.next() {
        if arg == "--" {
            break;
        }
        let value = values
            .next()
            .ok_or_else(|| "invalid update watchdog arguments".to_owned())?;
        match arg.as_str() {
            "--ready-file" => ready = Some(PathBuf::from(value)),
            "--rollback" => rollback = Some(PathBuf::from(value)),
            "--current-link" => current_link = Some(PathBuf::from(value)),
            "--secret-file" => secret = Some(PathBuf::from(value)),
            "--port" => port = Some(value.parse::<u16>().map_err(|_| "invalid watchdog port")?),
            _ => return Err("invalid update watchdog arguments".to_owned()),
        }
    }
    Ok((
        ready.ok_or("missing watchdog ready file")?,
        rollback.ok_or("missing rollback binary")?,
        current_link.ok_or("missing current release link")?,
        secret.ok_or("missing watchdog secret")?,
        port.ok_or("missing watchdog port")?,
        values.cloned().collect(),
    ))
}

fn healthy(port: u16, token: &str) -> bool {
    use std::net::TcpStream;
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = format!(
        "GET /v1/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.starts_with("HTTP/1.1 200")
}

fn status_path(directory: &Path) -> PathBuf {
    directory.join("update-status.json")
}
fn read_status(directory: &Path) -> Result<Status, String> {
    let path = status_path(directory);
    let text = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Status {
                policy: Policy::Enabled,
                accepted_version: None,
                last_check: None,
                last_result: None,
            });
        }
        Err(_) => return Err("could not read update status".to_owned()),
    };
    let value = parse_json(&text).map_err(|_| "invalid update status".to_owned())?;
    let policy = Policy::parse(
        value
            .object("policy")
            .and_then(Json::as_str)
            .unwrap_or("enabled"),
    )?;
    let string = |name| value.object(name).and_then(Json::as_str).map(str::to_owned);
    Ok(Status {
        policy,
        accepted_version: string("accepted_version"),
        last_check: string("last_check"),
        last_result: string("last_result"),
    })
}
fn save_status(directory: &Path, status: &Status) -> Result<(), String> {
    atomic_write(
        &status_path(directory),
        Json::Object(vec![
            ("policy".to_owned(), Json::String(status.policy.as_str())),
            (
                "accepted_version".to_owned(),
                status
                    .accepted_version
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
            (
                "last_check".to_owned(),
                status
                    .last_check
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
            (
                "last_result".to_owned(),
                status
                    .last_result
                    .clone()
                    .map(Json::String)
                    .unwrap_or(Json::Null),
            ),
        ])
        .to_json()
        .as_bytes(),
    )
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())
}
fn replace_symlink(target: &Path, link: &Path) -> Result<(), String> {
    let parent = link
        .parent()
        .ok_or_else(|| "link has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(
        ".{}.new",
        link.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = fs::remove_file(&temporary);
    symlink(target, &temporary).map_err(|e| e.to_string())?;
    fs::rename(temporary, link).map_err(|e| e.to_string())
}

fn valid_version(value: &str) -> bool {
    let v = value.strip_prefix('v').unwrap_or(value);
    v.split('.').count() == 3
        && v.split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}
fn version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |v: &str| {
        v.strip_prefix('v')
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parse(left).cmp(&parse(right))
}
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finish().iter().map(|b| format!("{b:02x}")).collect())
}
struct Sha256 {
    state: [u32; 8],
    data: Vec<u8>,
    length: u64,
}
impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            data: Vec::new(),
            length: 0,
        }
    }
    fn update(&mut self, input: &[u8]) {
        self.length += input.len() as u64;
        self.data.extend_from_slice(input);
        while self.data.len() >= 64 {
            let block: self::Block = self.data[..64].try_into().expect("block");
            self.block(&block);
            self.data.drain(..64);
        }
    }
    fn finish(mut self) -> [u8; 32] {
        let bits = self.length * 8;
        self.data.push(0x80);
        while self.data.len() % 64 != 56 {
            self.data.push(0);
        }
        self.data.extend_from_slice(&bits.to_be_bytes());
        while !self.data.is_empty() {
            let block: Block = self.data[..64].try_into().expect("block");
            self.block(&block);
            self.data.drain(..64);
        }
        let mut output = [0; 32];
        for (i, value) in self.state.iter().enumerate() {
            output[i * 4..i * 4 + 4].copy_from_slice(&value.to_be_bytes());
        }
        output
    }
    fn block(&mut self, block: &Block) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let offset = i * 4;
            *word = u32::from_be_bytes(block[offset..offset + 4].try_into().expect("word"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
            self.state[5],
            self.state[6],
            self.state[7],
        );
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}
type Block = [u8; 64];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_only_safe_manifest() {
        let m=parse_manifest(r#"{"version":"v1.2.3","target":"aarch64-apple-darwin","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#).unwrap();
        assert_eq!(m.version, "v1.2.3");
        assert!(
            parse_manifest(r#"{"version":"later","target":"x","sha256":"no","commit":"no"}"#)
                .is_err()
        );
    }
    #[test]
    fn sha256_matches_known_vector() {
        let path = std::env::temp_dir().join("zigzag-sha256-test");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_file(path);
    }
    #[test]
    fn status_controls_persist() {
        let dir = std::env::temp_dir().join(format!("zigzag-update-status-{}", std::process::id()));
        let args = vec![
            "--dir".to_owned(),
            dir.to_string_lossy().into_owned(),
            "pin".to_owned(),
            "v2.0.1".to_owned(),
        ];
        run_control(&args).unwrap();
        assert_eq!(
            read_status(&dir).unwrap().policy,
            Policy::Pin("v2.0.1".to_owned())
        );
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn rollback_pointer_is_replaced_atomically() {
        let dir =
            std::env::temp_dir().join(format!("zigzag-update-pointer-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old");
        let new = dir.join("new");
        fs::write(&old, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        let link = dir.join("current");
        replace_symlink(&old, &link).unwrap();
        replace_symlink(&new, &link).unwrap();
        assert_eq!(fs::read_link(link).unwrap(), new);
        let _ = fs::remove_dir_all(dir);
    }
}
