//! Verified, in-process relay updates.
//!
//! A GitHub Release is discovery only.  The release manifest is checked before
//! a candidate is made current and `gh attestation verify` is constrained by
//! the repository, workflow, source ref, and the bundled Sigstore trust root.

use relay_core::{Json, parse_json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const REPOSITORY: &str = "ShukantPal/zigzag";
const WORKFLOW: &str = "ShukantPal/zigzag/.github/workflows/ci.yml";
const TARGET: &str = "aarch64-apple-darwin";
const BINARY: &str = "zigzag-macos-aarch64";
const MANIFEST: &str = "zigzag-macos-aarch64.manifest.json";
const TRUST_ROOT: &str = include_str!("../trust/sigstore-trusted-root.json");
/// Apple Team ID pinned by the updater's code-signature requirement.
///
/// Ground truth from the issued signing certificate (leaf subject OU), not the
/// stale team ID in the certificate's display name: releases are signed with an
/// Apple Development certificate whose CN still reads
/// "Apple Development: Shukant Pal (NH5F3PDHQ8)" but whose subject OU -- the
/// authoritative team identifier -- is 7YZK8D3B48 (Leveled Platforms, Inc).
const TEAM_ID: &str = "7YZK8D3B48";

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
    spawn_gate: Mutex<()>,
    check_gate: Mutex<()>,
    runtime: Arc<dyn Runtime>,
    manual_context: Mutex<Option<ManualContext>>,
    latest_available: Mutex<Option<String>>,
}
struct ManualContext {
    active_work: Arc<ActiveWork>,
    audit: Arc<Audit>,
    original_args: Vec<String>,
    secret_file: PathBuf,
    health_port: u16,
}
type ActiveWork = dyn Fn() -> bool + Send + Sync;
type Audit = dyn Fn(&str, Json) + Send + Sync;

struct WatchdogConfig {
    ready: PathBuf,
    rollback: PathBuf,
    current_link: PathBuf,
    status_directory: PathBuf,
    candidate_version: String,
    candidate_pid: u32,
    secret_file: PathBuf,
    port: u16,
    relay_args: Vec<String>,
}

struct CommandOutput {
    stdout: String,
    stderr: String,
}

trait Runtime: Send + Sync {
    fn output(&self, command: &str, args: &[String]) -> Result<CommandOutput, String>;
    fn status(&self, command: &str, args: &[String]) -> Result<(), String>;
    fn spawn(&self, command: &Path, args: &[String]) -> Result<(), String>;
    fn terminate(&self, process: u32) -> Result<(), String>;
    fn exec(
        &self,
        command: &Path,
        args: &[String],
        environment: Option<(&str, &str)>,
    ) -> Result<(), String>;
}

struct SystemRuntime;

impl Runtime for SystemRuntime {
    fn output(&self, command: &str, args: &[String]) -> Result<CommandOutput, String> {
        let output = Command::new(command)
            .args(args)
            .output()
            .map_err(|e| format!("could not run {command}: {e}"))?;
        if !output.status.success() {
            return Err(format!("{command} failed"));
        }
        Ok(CommandOutput {
            stdout: String::from_utf8(output.stdout)
                .map_err(|_| format!("{command} returned non-UTF-8 output"))?,
            stderr: String::from_utf8(output.stderr)
                .map_err(|_| format!("{command} returned non-UTF-8 output"))?,
        })
    }

    fn status(&self, command: &str, args: &[String]) -> Result<(), String> {
        let status = Command::new(command)
            .args(args)
            .status()
            .map_err(|e| format!("could not run {command}: {e}"))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("{command} verification failed"))
    }

    fn spawn(&self, command: &Path, args: &[String]) -> Result<(), String> {
        Command::new(command)
            .args(args)
            .env_remove("ZIGZAG_UPDATE_VERSION")
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("could not start update watchdog: {e}"))
    }

    fn terminate(&self, process: u32) -> Result<(), String> {
        let process = process as libc::pid_t;
        // The watchdog is spawned by the process it supervises. Refuse to
        // signal a recycled PID if that parent relationship has been lost.
        if unsafe { libc::getppid() } != process {
            return Err("update watchdog no longer supervises the candidate".to_owned());
        }
        if unsafe { libc::kill(process, libc::SIGTERM) } != 0 {
            return Err("could not stop unhealthy candidate".to_owned());
        }
        for _ in 0..50 {
            if unsafe { libc::getppid() } != process {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if unsafe { libc::kill(process, libc::SIGKILL) } != 0 {
            return Err("unhealthy candidate did not stop".to_owned());
        }
        for _ in 0..50 {
            if unsafe { libc::getppid() } != process {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("unhealthy candidate did not exit".to_owned())
    }

    fn exec(
        &self,
        command: &Path,
        args: &[String],
        environment: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let mut process = Command::new(command);
        process.args(args);
        if let Some((name, value)) = environment {
            process.env(name, value);
        }
        let error = process.exec();
        Err(format!("could not exec {}: {error}", command.display()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub policy: Policy,
    pub accepted_version: Option<String>,
    pub last_check: Option<String>,
    pub last_result: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub current_version: String,
    pub latest_available_version: Option<String>,
    pub update_applied: bool,
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
            spawn_gate: Mutex::new(()),
            check_gate: Mutex::new(()),
            runtime: Arc::new(SystemRuntime),
            manual_context: Mutex::new(None),
            latest_available: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_runtime(config: Config, runtime: Arc<dyn Runtime>) -> Self {
        Self {
            config,
            draining: Arc::new(AtomicBool::new(false)),
            spawn_gate: Mutex::new(()),
            check_gate: Mutex::new(()),
            runtime,
            manual_context: Mutex::new(None),
            latest_available: Mutex::new(None),
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Serializes the drain transition with child creation and registry
    /// registration. Callers hold this guard until the running record commits.
    pub fn spawn_admission(&self) -> Result<Option<MutexGuard<'_, ()>>, String> {
        let guard = self
            .spawn_gate
            .lock()
            .map_err(|_| "update spawn gate lock poisoned".to_owned())?;
        Ok((!self.is_draining()).then_some(guard))
    }

    pub(crate) fn begin_drain(&self) -> Result<bool, String> {
        let _gate = self
            .spawn_gate
            .lock()
            .map_err(|_| "update spawn gate lock poisoned".to_owned())?;
        Ok(self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok())
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
    ///
    /// The watchdog promotes the candidate only after independently observing
    /// authenticated health. A process that binds then crashes must not become
    /// last-known-good merely because it wrote this readiness marker.
    pub fn acknowledge_ready(&self, version: Option<&str>) -> Result<(), String> {
        let Some(path) = &self.config.ready_file else {
            return Ok(());
        };
        let version = version
            .filter(|value| valid_version(value))
            .ok_or_else(|| "replacement version is unavailable".to_owned())?;
        let _ = version;
        atomic_write(path, b"ready\n")?;
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
        if let Ok(mut context) = self.manual_context.lock() {
            *context = Some(ManualContext {
                active_work: Arc::clone(&active_work),
                audit: Arc::clone(&audit),
                original_args: original_args.clone(),
                secret_file: secret_file.clone(),
                health_port,
            });
        }
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
                    log::warn!("relay update check failed: {error}");
                }
                std::thread::sleep(self.config.interval);
            }
        });
    }

    /// Perform an update check immediately using the same policy and
    /// verification path as the scheduled updater.
    pub fn check_now(&self) -> Result<CheckResult, String> {
        let (active_work, audit, original_args, secret_file, health_port) = {
            let context = self
                .manual_context
                .lock()
                .map_err(|_| "update context lock poisoned".to_owned())?;
            let context = context
                .as_ref()
                .ok_or_else(|| "update manager is not ready".to_owned())?;
            (
                Arc::clone(&context.active_work),
                Arc::clone(&context.audit),
                context.original_args.clone(),
                context.secret_file.clone(),
                context.health_port,
            )
        };
        let outcome = self.check_and_apply(
            active_work.as_ref(),
            audit.as_ref(),
            &original_args,
            &secret_file,
            health_port,
        );
        outcome?;
        let status = self.status();
        let latest_available_version = self
            .latest_available
            .lock()
            .map_err(|_| "update version lock poisoned".to_owned())?
            .clone();
        Ok(CheckResult {
            current_version: std::env::var("ZIGZAG_UPDATE_VERSION")
                .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned()),
            latest_available_version,
            update_applied: status.last_result.as_deref() == Some("applied"),
        })
    }

    fn check_and_apply(
        &self,
        active_work: &dyn Fn() -> bool,
        audit: &dyn Fn(&str, Json),
        original_args: &[String],
        secret_file: &Path,
        health_port: u16,
    ) -> Result<(), String> {
        let _check = self
            .check_gate
            .lock()
            .map_err(|_| "update check lock poisoned".to_owned())?;
        let mut status = self.status();
        if matches!(status.policy, Policy::Paused) {
            status.last_check = Some(relay_core::rfc3339_timestamp());
            status.last_result = Some("paused".to_owned());
            save_status(&self.config.directory, &status)?;
            return Ok(());
        }
        status.last_check = Some(relay_core::rfc3339_timestamp());
        audit("relay_update_check_started", Json::Object(vec![]));
        let result = match self.fetch_candidate(&status) {
            Ok(result) => result,
            Err(error) => {
                status.last_result = Some(format!("check_failed:{error}"));
                save_status(&self.config.directory, &status)?;
                audit(
                    "relay_update_failed",
                    Json::Object(vec![("reason".to_owned(), Json::String(error.clone()))]),
                );
                return Err(error);
            }
        };
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
        if !self.begin_drain()? {
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
                failure_payload(
                    &status.accepted_version,
                    &manifest.version,
                    &manifest.sha256,
                    error,
                ),
            );
        }
        apply
    }

    fn fetch_candidate(&self, status: &Status) -> Result<Option<(Manifest, PathBuf)>, String> {
        let release = command_output(
            self.runtime.as_ref(),
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
        *self
            .latest_available
            .lock()
            .map_err(|_| "update version lock poisoned".to_owned())? = Some(tag.to_owned());
        if status
            .accepted_version
            .as_deref()
            .is_some_and(|old| version_cmp(tag, old).is_le())
        {
            return Ok(None);
        }
        let asset_url = release_asset_url(&release, MANIFEST)?;
        let manifest_path = self.config.directory.join("candidate-manifest.json");
        download(self.runtime.as_ref(), &asset_url, &manifest_path)?;
        // The manifest is itself an attested subject. This binds its version to
        // the trusted workflow before release metadata can influence monotonicity.
        verify_attestation(
            self.runtime.as_ref(),
            &manifest_path,
            None,
            &self.config.directory,
        )?;
        let manifest_text = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("could not read release manifest: {e}"))?;
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
        download(self.runtime.as_ref(), &binary_url, &candidate)?;
        if sha256_file(&candidate)? != manifest.sha256 {
            let _ = fs::remove_file(&candidate);
            return Err("release binary digest does not match manifest".to_owned());
        }
        verify_codesign(self.runtime.as_ref(), &candidate)?;
        verify_attestation(
            self.runtime.as_ref(),
            &candidate,
            Some(&manifest.commit),
            &self.config.directory,
        )?;
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
        let mut candidate_args = original_args.to_vec();
        candidate_args.push("--update-ready-file".to_owned());
        candidate_args.push(ready.to_string_lossy().into_owned());
        let watchdog = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut status = self.status();
        status.last_result = Some("exec_pending_health_check".to_owned());
        save_status(&self.config.directory, &status)?;
        let mut watchdog_args = vec![
            "update-watchdog".to_owned(),
            "--ready-file".to_owned(),
            ready.to_string_lossy().into_owned(),
            "--rollback".to_owned(),
            previous.to_string_lossy().into_owned(),
            "--current-link".to_owned(),
            current.to_string_lossy().into_owned(),
            "--status-dir".to_owned(),
            self.config.directory.to_string_lossy().into_owned(),
            "--candidate-version".to_owned(),
            manifest.version.clone(),
            "--candidate-pid".to_owned(),
            std::process::id().to_string(),
            "--secret-file".to_owned(),
            secret_file.to_string_lossy().into_owned(),
            "--port".to_owned(),
            health_port.to_string(),
            "--".to_owned(),
        ];
        // Rollback must receive the original arguments, never the candidate's
        // readiness flag, or the old image could acknowledge a failed update.
        watchdog_args.extend_from_slice(original_args);
        if let Err(error) = self.runtime.spawn(&watchdog, &watchdog_args) {
            replace_symlink(&previous, &current)?;
            return Err(error);
        }
        // Replacing this process preserves the LaunchAgent GUI session and its
        // Keychain access.  Never bootstrap/bootout the agent here.
        if let Err(error) = self.runtime.exec(
            candidate,
            &candidate_args,
            Some(("ZIGZAG_UPDATE_VERSION", &manifest.version)),
        ) {
            replace_symlink(&previous, &current)?;
            return Err(error);
        }
        Ok(())
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

pub fn persistent_server_args(arguments: &[String]) -> Vec<String> {
    let mut kept = Vec::with_capacity(arguments.len());
    let mut values = arguments.iter();
    while let Some(argument) = values.next() {
        if argument == "--update-ready-file" {
            let _ = values.next();
        } else {
            kept.push(argument.clone());
        }
    }
    kept
}

pub fn run_watchdog(arguments: &[String]) -> Result<(), String> {
    run_watchdog_with(
        parse_watchdog(arguments)?,
        &SystemRuntime,
        60,
        || std::thread::sleep(Duration::from_secs(1)),
        healthy,
    )
}

fn run_watchdog_with(
    config: WatchdogConfig,
    runtime: &dyn Runtime,
    attempts: usize,
    wait: impl Fn(),
    health_check: impl Fn(u16, &str) -> bool,
) -> Result<(), String> {
    let token = fs::read_to_string(&config.secret_file)
        .map_err(|_| "could not read watchdog token".to_owned())?;
    for _ in 0..attempts {
        if config.ready.exists() && health_check(config.port, token.trim()) {
            return promote_candidate(&config);
        }
        wait();
    }
    log::error!("candidate did not become healthy; rolling back");
    replace_symlink(&config.rollback, &config.current_link)?;
    let mut status = read_status(&config.status_directory)?;
    status.last_result = Some(format!("health_check_failed:{}", config.candidate_version));
    save_status(&config.status_directory, &status)?;
    // `current` now points at the rollback image, but the candidate is still
    // the LaunchAgent process and owns the listeners. Stop it before replacing
    // this watchdog child with the rollback image.
    runtime.terminate(config.candidate_pid)?;
    runtime.exec(&config.rollback, &config.relay_args, None)
}

fn promote_candidate(config: &WatchdogConfig) -> Result<(), String> {
    let candidate = fs::canonicalize(&config.current_link)
        .map_err(|_| "current candidate is unavailable after health check".to_owned())?;
    replace_symlink(&candidate, &config.status_directory.join("last-known-good"))?;
    let mut status = read_status(&config.status_directory)?;
    status.accepted_version = Some(config.candidate_version.clone());
    status.last_result = Some("applied".to_owned());
    save_status(&config.status_directory, &status)
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

fn verify_codesign(runtime: &dyn Runtime, candidate: &Path) -> Result<(), String> {
    // NOTE: `-R` takes a bare requirement expression. A "designated =>" prefix
    // (as printed by `codesign -d -r-`) is a syntax error and fails every
    // verification; `anchor apple generic` is the correct anchor for
    // Apple-issued developer certificates (verified against release v0.1.206).
    let requirement = format!(
        "anchor apple generic and identifier \"com.shukantpal.zigzag\" and certificate leaf[subject.OU] = \"{TEAM_ID}\""
    );
    command_status(
        runtime,
        "codesign",
        [
            "--verify",
            "--strict",
            "--deep",
            "--verbose=2",
            &format!("-R={requirement}"),
            &candidate.to_string_lossy(),
        ],
    )?;
    let output = runtime.output(
        "codesign",
        &[
            "-d".to_owned(),
            "--verbose=4".to_owned(),
            candidate.to_string_lossy().into_owned(),
        ],
    )?;
    let detail = output.stderr;
    if !detail.contains("Identifier=com.shukantpal.zigzag")
        || !detail.contains(&format!("TeamIdentifier={TEAM_ID}"))
    {
        return Err("candidate does not have the expected code-signing identity".to_owned());
    }
    Ok(())
}

fn verify_attestation(
    runtime: &dyn Runtime,
    candidate: &Path,
    commit: Option<&str>,
    directory: &Path,
) -> Result<(), String> {
    let root = directory.join("sigstore-trusted-root.json");
    // Always use the reviewed roots embedded in the running verified image.
    atomic_write(&root, TRUST_ROOT.as_bytes())?;
    let mut args = vec![
        "attestation".to_owned(),
        "verify".to_owned(),
        candidate.to_string_lossy().into_owned(),
        "--repo".to_owned(),
        REPOSITORY.to_owned(),
        "--signer-workflow".to_owned(),
        WORKFLOW.to_owned(),
        "--source-ref".to_owned(),
        "refs/heads/main".to_owned(),
    ];
    if let Some(commit) = commit {
        args.extend(["--source-digest".to_owned(), commit.to_owned()]);
    }
    args.extend([
        "--predicate-type".to_owned(),
        "https://slsa.dev/provenance/v1".to_owned(),
        "--custom-trusted-root".to_owned(),
        root.to_string_lossy().into_owned(),
    ]);
    runtime.status("gh", &args)
}

fn download(runtime: &dyn Runtime, url: &str, output: &Path) -> Result<(), String> {
    let parent = output
        .parent()
        .ok_or_else(|| "download path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| format!("could not create update directory: {e}"))?;
    let temporary = output.with_extension("download");
    let _ = fs::remove_file(&temporary);
    command_status(
        runtime,
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
    runtime: &dyn Runtime,
    command: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, String> {
    let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
    runtime.output(command, &args).map(|output| output.stdout)
}
fn command_status<'a>(
    runtime: &dyn Runtime,
    command: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
    runtime.status(command, &args)
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

fn failure_payload(old: &Option<String>, new: &str, digest: &str, reason: &str) -> Json {
    let Json::Object(mut values) = update_payload(old, new, digest) else {
        unreachable!("update payload is an object")
    };
    values.push(("reason".to_owned(), Json::String(reason.to_owned())));
    Json::Object(values)
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
fn parse_watchdog(arguments: &[String]) -> Result<WatchdogConfig, String> {
    let mut values = arguments.iter();
    let mut ready = None;
    let mut rollback = None;
    let mut current_link = None;
    let mut status_directory = None;
    let mut candidate_version = None;
    let mut candidate_pid = None;
    let mut credential_path = None;
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
            "--status-dir" => status_directory = Some(PathBuf::from(value)),
            "--candidate-version" if valid_version(value) => {
                candidate_version = Some(value.clone())
            }
            "--candidate-pid" => {
                candidate_pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| "invalid watchdog candidate pid")?,
                )
            }
            "--secret-file" => credential_path = Some(PathBuf::from(value)),
            "--port" => port = Some(value.parse::<u16>().map_err(|_| "invalid watchdog port")?),
            _ => return Err("invalid update watchdog arguments".to_owned()),
        }
    }
    Ok(WatchdogConfig {
        ready: ready.ok_or("missing watchdog ready file")?,
        rollback: rollback.ok_or("missing rollback binary")?,
        current_link: current_link.ok_or("missing current release link")?,
        status_directory: status_directory.ok_or("missing watchdog status directory")?,
        candidate_version: candidate_version.ok_or("missing watchdog candidate version")?,
        candidate_pid: candidate_pid.ok_or("missing watchdog candidate pid")?,
        secret_file: credential_path.ok_or("missing watchdog credential file")?,
        port: port.ok_or("missing watchdog port")?,
        relay_args: values.cloned().collect(),
    })
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
    let target = fs::canonicalize(target)
        .map_err(|e| format!("could not resolve symlink target {}: {e}", target.display()))?;
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
    let mut file =
        fs::File::open(path).map_err(|e| format!("could not open file for hashing: {e}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("could not hash file: {e}"))?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    #[derive(Clone, Debug)]
    struct Call {
        operation: &'static str,
        command: String,
        args: Vec<String>,
    }

    struct MockRuntime {
        release: String,
        downloads: Mutex<VecDeque<Vec<u8>>>,
        codesign_detail: String,
        calls: Mutex<Vec<Call>>,
        status_index: AtomicUsize,
        fail_status_at: Option<usize>,
        fail_spawn: bool,
    }

    impl MockRuntime {
        fn release(manifest: &str, binary: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                release: r#"{"tag_name":"v2.0.0","assets":[{"name":"zigzag-macos-aarch64.manifest.json","browser_download_url":"https://example.test/manifest"},{"name":"zigzag-macos-aarch64","browser_download_url":"https://example.test/binary"}]}"#.to_owned(),
                downloads: Mutex::new(VecDeque::from([
                    manifest.as_bytes().to_vec(),
                    binary.to_vec(),
                ])),
                codesign_detail:
                    "Identifier=com.shukantpal.zigzag\nTeamIdentifier=7YZK8D3B48\n".to_owned(),
                calls: Mutex::new(Vec::new()),
                status_index: AtomicUsize::new(0),
                fail_status_at: None,
                fail_spawn: false,
            })
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn exec_calls(&self) -> Vec<Call> {
            self.calls()
                .into_iter()
                .filter(|call| call.operation == "exec")
                .collect()
        }
    }

    impl Runtime for MockRuntime {
        fn output(&self, command: &str, args: &[String]) -> Result<CommandOutput, String> {
            self.calls.lock().unwrap().push(Call {
                operation: "output",
                command: command.to_owned(),
                args: args.to_vec(),
            });
            match command {
                "curl" => Ok(CommandOutput {
                    stdout: self.release.clone(),
                    stderr: String::new(),
                }),
                "codesign" => Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: self.codesign_detail.clone(),
                }),
                _ => Err(format!("unexpected output command: {command}")),
            }
        }

        fn status(&self, command: &str, args: &[String]) -> Result<(), String> {
            let index = self.status_index.fetch_add(1, AtomicOrdering::SeqCst);
            self.calls.lock().unwrap().push(Call {
                operation: "status",
                command: command.to_owned(),
                args: args.to_vec(),
            });
            if command == "curl" {
                let output_index = args
                    .iter()
                    .position(|arg| arg == "--output")
                    .expect("curl output argument")
                    + 1;
                let bytes = self
                    .downloads
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("queued download");
                fs::write(&args[output_index], bytes).unwrap();
            }
            if self.fail_status_at == Some(index) {
                return Err(if command == "gh" {
                    "attestation verification failed".to_owned()
                } else {
                    format!("{command} verification failed")
                });
            }
            Ok(())
        }

        fn spawn(&self, command: &Path, args: &[String]) -> Result<(), String> {
            self.calls.lock().unwrap().push(Call {
                operation: "spawn",
                command: command.to_string_lossy().into_owned(),
                args: args.to_vec(),
            });
            if self.fail_spawn {
                Err("watchdog spawn failed".to_owned())
            } else {
                Ok(())
            }
        }

        fn terminate(&self, process: u32) -> Result<(), String> {
            self.calls.lock().unwrap().push(Call {
                operation: "terminate",
                command: process.to_string(),
                args: vec![],
            });
            Ok(())
        }

        fn exec(
            &self,
            command: &Path,
            args: &[String],
            environment: Option<(&str, &str)>,
        ) -> Result<(), String> {
            let mut recorded = args.to_vec();
            if let Some((name, value)) = environment {
                recorded.push(format!("{name}={value}"));
            }
            self.calls.lock().unwrap().push(Call {
                operation: "exec",
                command: command.to_string_lossy().into_owned(),
                args: recorded,
            });
            Err("exec intercepted".to_owned())
        }
    }

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zigzag-update-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn manager(dir: &Path, runtime: Arc<dyn Runtime>, ready_file: Option<PathBuf>) -> Manager {
        Manager::with_runtime(
            Config {
                directory: dir.to_path_buf(),
                interval: Duration::ZERO,
                policy: Policy::Enabled,
                ready_file,
            },
            runtime,
        )
    }

    fn manifest(version: &str, target: &str, digest: &str) -> String {
        format!(
            r#"{{"version":"{version}","target":"{target}","sha256":"{digest}","commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#
        )
    }

    fn empty_status() -> Status {
        Status {
            policy: Policy::Enabled,
            accepted_version: Some("v1.0.0".to_owned()),
            last_check: None,
            last_result: None,
        }
    }

    #[test]
    fn parses_only_safe_manifest() {
        let parsed = parse_manifest(&manifest(
            "v1.2.3",
            TARGET,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ))
        .unwrap();
        assert_eq!(parsed.version, "v1.2.3");
        assert!(
            parse_manifest(r#"{"version":"later","target":"x","sha256":"no","commit":"no"}"#)
                .is_err()
        );
    }

    #[test]
    fn fetch_candidate_verifies_every_authenticated_boundary() {
        let dir = temp_dir("fetch-ok");
        let digest = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd".to_owned();
        let runtime = MockRuntime::release(&manifest("v2.0.0", TARGET, &digest), b"binary");
        let updater = manager(&dir, runtime.clone(), None);

        let (verified, candidate) = updater
            .fetch_candidate(&empty_status())
            .unwrap()
            .expect("new candidate");
        assert_eq!(verified.version, "v2.0.0");
        assert_eq!(fs::read(candidate).unwrap(), b"binary");

        let calls = runtime.calls();
        let attestations = calls
            .iter()
            .filter(|call| call.command == "gh")
            .collect::<Vec<_>>();
        assert_eq!(attestations.len(), 2);
        for call in &attestations {
            let joined = call.args.join(" ");
            assert!(joined.contains("--repo ShukantPal/zigzag"));
            assert!(
                joined.contains("--signer-workflow ShukantPal/zigzag/.github/workflows/ci.yml")
            );
            assert!(joined.contains("--source-ref refs/heads/main"));
            assert!(joined.contains("--custom-trusted-root"));
        }
        assert!(
            !attestations[0]
                .args
                .iter()
                .any(|arg| arg == "--source-digest"),
            "the manifest is authenticated before its claimed commit is trusted"
        );
        assert!(attestations[1].args.windows(2).any(|args| {
            args == [
                "--source-digest",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ]
        }));
        let codesign = calls
            .iter()
            .find(|call| call.command == "codesign" && call.operation == "status")
            .unwrap();
        assert!(codesign.args.iter().any(|arg| {
            arg.contains("anchor apple generic")
                && !arg.contains("designated =>")
                && arg.contains("identifier \"com.shukantpal.zigzag\"")
                && arg.contains("certificate leaf[subject.OU] = \"7YZK8D3B48\"")
        }));
        let root = fs::read_to_string(dir.join("sigstore-trusted-root.json")).unwrap();
        assert!(root.contains("fulcio.githubapp.com"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fetch_candidate_rejects_tag_target_digest_codesign_and_attestation_mismatches() {
        let digest = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd".to_owned();
        let cases = [
            (
                "v1.9.0",
                TARGET,
                digest.clone(),
                None,
                None,
                "version or target",
            ),
            (
                "v2.0.0",
                "x86_64-apple-darwin",
                digest.clone(),
                None,
                None,
                "version or target",
            ),
            ("v2.0.0", TARGET, "b".repeat(64), None, None, "digest"),
            (
                "v2.0.0",
                TARGET,
                digest.clone(),
                Some("bad identity"),
                None,
                "code-signing",
            ),
            (
                "v2.0.0",
                TARGET,
                digest.clone(),
                None,
                Some(4),
                "attestation",
            ),
        ];
        for (tag, target, manifest_digest, detail, fail_at, expected) in cases {
            let dir = temp_dir(expected);
            let mut runtime =
                MockRuntime::release(&manifest(tag, target, &manifest_digest), b"binary");
            let inner = Arc::get_mut(&mut runtime).unwrap();
            if let Some(detail) = detail {
                inner.codesign_detail = detail.to_owned();
            }
            inner.fail_status_at = fail_at;
            let updater = manager(&dir, runtime, None);
            let error = updater.fetch_candidate(&empty_status()).unwrap_err();
            assert!(error.contains(expected), "{error}");
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn failed_discovery_persists_status_and_emits_an_audit_fact() {
        let dir = temp_dir("failed-check");
        let runtime = Arc::new(MockRuntime {
            release: "not json".to_owned(),
            downloads: Mutex::new(VecDeque::new()),
            codesign_detail: String::new(),
            calls: Mutex::new(Vec::new()),
            status_index: AtomicUsize::new(0),
            fail_status_at: None,
            fail_spawn: false,
        });
        let updater = manager(&dir, runtime, None);
        save_status(&dir, &empty_status()).unwrap();
        let events = Mutex::new(Vec::new());
        let result = updater.check_and_apply(
            &|| false,
            &|kind, _| events.lock().unwrap().push(kind.to_owned()),
            &[],
            &dir.join("secret"),
            1,
        );
        assert!(result.is_err());
        let status = read_status(&dir).unwrap();
        assert!(status.last_check.is_some());
        assert!(status.last_result.unwrap().starts_with("check_failed:"));
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["relay_update_check_started", "relay_update_failed"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn drain_waits_for_admitted_spawn_and_active_work_before_exec() {
        let dir = temp_dir("drain");
        let digest = "9a3a45d01531a20e89ac6ae10b0b0beb0492acd7216a368aa062d1a5fecaf9cd".to_owned();
        let runtime = MockRuntime::release(&manifest("v2.0.0", TARGET, &digest), b"binary");
        let updater = Arc::new(manager(&dir, runtime.clone(), None));
        save_status(&dir, &empty_status()).unwrap();

        let admission = updater.spawn_admission().unwrap().unwrap();
        let (drained_tx, drained_rx) = mpsc::channel();
        let drain_updater = Arc::clone(&updater);
        let drain = thread::spawn(move || {
            let result = drain_updater.begin_drain().unwrap();
            drained_tx.send(result).unwrap();
        });
        assert!(drained_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(admission);
        assert!(drained_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        drain.join().unwrap();
        assert!(updater.spawn_admission().unwrap().is_none());

        updater.draining.store(false, Ordering::Release);
        let active = Arc::new(AtomicBool::new(true));
        let active_for_check = Arc::clone(&active);
        let check_updater = Arc::clone(&updater);
        let secret = dir.join("secret");
        let check = thread::spawn(move || {
            check_updater.check_and_apply(
                &|| active_for_check.load(Ordering::Acquire),
                &|_, _| {},
                &[],
                &secret,
                1,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !updater.is_draining() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(updater.is_draining());
        assert!(runtime.exec_calls().is_empty());
        active.store(false, Ordering::Release);
        assert!(check.join().unwrap().is_err());
        assert_eq!(runtime.exec_calls().len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn watchdog_spawn_failure_restores_current_pointer() {
        let dir = temp_dir("watchdog-spawn");
        let previous = dir.join("previous");
        let candidate = dir.join("candidate");
        fs::write(&previous, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        replace_symlink(&previous, &dir.join("last-known-good")).unwrap();
        let mut runtime = MockRuntime::release("", b"");
        Arc::get_mut(&mut runtime).unwrap().fail_spawn = true;
        let updater = manager(&dir, runtime, None);
        let result = updater.activate_and_exec(
            &Manifest {
                version: "v2.0.0".to_owned(),
                target: TARGET.to_owned(),
                sha256: "a".repeat(64),
                commit: "b".repeat(40),
            },
            &candidate,
            &[],
            &dir.join("secret"),
            1,
        );
        assert!(result.unwrap_err().contains("watchdog spawn failed"));
        assert_eq!(
            fs::canonicalize(dir.join("current")).unwrap(),
            fs::canonicalize(previous).unwrap()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn watchdog_promotes_only_healthy_candidates_and_stops_unhealthy_ones() {
        let dir = temp_dir("watchdog");
        let previous = dir.join("previous");
        let candidate = dir.join("candidate");
        let current = dir.join("current");
        let ready = dir.join("ready");
        let secret = dir.join("secret");
        fs::write(&previous, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        fs::write(&secret, b"token\n").unwrap();
        replace_symlink(&candidate, &current).unwrap();
        save_status(&dir, &empty_status()).unwrap();

        let runtime = MockRuntime::release("", b"");
        let updater = manager(&dir, runtime.clone(), Some(ready.clone()));
        updater.acknowledge_ready(Some("v2.0.0")).unwrap();
        assert!(ready.exists());
        assert_eq!(
            read_status(&dir).unwrap().accepted_version.as_deref(),
            Some("v1.0.0")
        );
        assert!(!dir.join("last-known-good").exists());

        run_watchdog_with(
            WatchdogConfig {
                ready: ready.clone(),
                rollback: previous.clone(),
                current_link: current.clone(),
                status_directory: dir.clone(),
                candidate_version: "v2.0.0".to_owned(),
                candidate_pid: 42,
                secret_file: secret.clone(),
                port: 1,
                relay_args: vec![],
            },
            runtime.as_ref(),
            1,
            || {},
            |_, _| true,
        )
        .unwrap();
        let applied = read_status(&dir).unwrap();
        assert_eq!(applied.accepted_version.as_deref(), Some("v2.0.0"));
        assert_eq!(applied.last_result.as_deref(), Some("applied"));
        assert_eq!(
            fs::canonicalize(dir.join("last-known-good")).unwrap(),
            fs::canonicalize(&candidate).unwrap()
        );

        // Recreate the pending state for an unhealthy replacement. The prior
        // version remains accepted and the candidate is stopped before the
        // watchdog execs the rollback image.
        let mut pending = empty_status();
        pending.last_result = Some("exec_pending_health_check".to_owned());
        save_status(&dir, &pending).unwrap();
        fs::remove_file(&ready).unwrap();
        replace_symlink(&candidate, &current).unwrap();
        replace_symlink(&previous, &dir.join("last-known-good")).unwrap();
        let result = run_watchdog_with(
            WatchdogConfig {
                ready,
                rollback: previous.clone(),
                current_link: current.clone(),
                status_directory: dir.clone(),
                candidate_version: "v2.0.0".to_owned(),
                candidate_pid: 42,
                secret_file: secret,
                port: 1,
                relay_args: vec!["--port".to_owned(), "8765".to_owned()],
            },
            runtime.as_ref(),
            1,
            || {},
            |_, _| false,
        );
        assert!(result.unwrap_err().contains("exec intercepted"));
        assert_eq!(
            fs::canonicalize(current).unwrap(),
            fs::canonicalize(previous).unwrap()
        );
        let rolled_back = read_status(&dir).unwrap();
        assert_eq!(rolled_back.accepted_version.as_deref(), Some("v1.0.0"));
        assert_eq!(
            rolled_back.last_result.as_deref(),
            Some("health_check_failed:v2.0.0")
        );
        assert!(
            runtime
                .calls()
                .iter()
                .any(|call| { call.operation == "terminate" && call.command == "42" })
        );
        let rollback_exec = runtime.exec_calls().pop().unwrap();
        assert!(
            !rollback_exec
                .args
                .iter()
                .any(|arg| arg == "--update-ready-file")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn health_probe_requires_the_authenticated_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = [0; 1024];
                let length = stream.read(&mut bytes).unwrap();
                let request = String::from_utf8_lossy(&bytes[..length]);
                let status = if request.contains("Authorization: Bearer token\r\n") {
                    "200 OK"
                } else {
                    "401 Unauthorized"
                };
                stream
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .unwrap();
            }
        });
        assert!(!healthy(port, "wrong"));
        assert!(healthy(port, "token"));
        server.join().unwrap();
    }

    #[test]
    fn sha256_matches_known_vectors() {
        let dir = temp_dir("sha");
        let path = dir.join("value");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let empty = dir.join("empty");
        fs::write(&empty, b"").unwrap();
        assert_eq!(
            sha256_file(&empty).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sha256_reports_unreadable_file() {
        let error = sha256_file(&temp_dir("sha-missing").join("does-not-exist")).unwrap_err();
        assert!(
            error.contains("could not open file for hashing"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn status_controls_persist() {
        let dir = temp_dir("status");
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
    fn readiness_argument_is_not_forwarded_to_later_candidates_or_rollbacks() {
        assert_eq!(
            persistent_server_args(&[
                "--port".to_owned(),
                "8765".to_owned(),
                "--update-ready-file".to_owned(),
                "/tmp/old-ready".to_owned(),
                "--max-events".to_owned(),
                "100".to_owned(),
            ]),
            ["--port", "8765", "--max-events", "100"]
        );
    }

    #[test]
    fn rollback_pointer_is_replaced_atomically_with_an_absolute_target() {
        let dir = temp_dir("pointer");
        let old = dir.join("old");
        let new = dir.join("new");
        fs::write(&old, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        let link = dir.join("current");
        replace_symlink(&old, &link).unwrap();
        replace_symlink(&new, &link).unwrap();
        assert_eq!(fs::read_link(link).unwrap(), fs::canonicalize(new).unwrap());
        let _ = fs::remove_dir_all(dir);
    }
}
