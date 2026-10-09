use crate::exec;
use crate::session::{is_tailscale_ipv4, require_gui_login_session};
use crate::update;
use std::env;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) struct Config {
    pub(crate) secret_file: PathBuf,
    pub(crate) control_secret_file: Option<PathBuf>,
    pub(crate) state_file: PathBuf,
    pub(crate) agent_registry_file: PathBuf,
    pub(crate) port: u16,
    pub(crate) tailscale_ip: Option<IpAddr>,
    pub(crate) max_events: usize,
    pub(crate) github_watch_repos: Vec<String>,
    pub(crate) github_watch_interval: Duration,
    pub(crate) update_directory: PathBuf,
    pub(crate) update_interval: Duration,
    pub(crate) update_policy: update::Policy,
    pub(crate) update_ready_file: Option<PathBuf>,
    pub(crate) review_state_file: PathBuf,
}
pub(crate) fn server_config(arguments: Vec<String>) -> Result<Config, String> {
    let mut secret_file = env::var_os("ZIGZAG_SECRET_FILE").map(PathBuf::from);
    let mut state_file = env::var_os("ZIGZAG_STATE_FILE").map(PathBuf::from);
    let mut control_secret_file = env::var_os("ZIGZAG_CONTROL_SECRET_FILE").map(PathBuf::from);
    let mut port = 8765;
    let mut tailscale_ip = None;
    let mut max_events = 1000;
    let mut github_watch_repos = Vec::new();
    let mut github_watch_interval = Duration::from_secs(30);
    let mut update_directory = env::var_os("ZIGZAG_UPDATE_DIR").map(PathBuf::from);
    let mut update_interval = env::var("ZIGZAG_UPDATE_INTERVAL")
        .ok()
        .map(|value| value.parse::<u64>().map(Duration::from_secs))
        .transpose()
        .map_err(|_| "ZIGZAG_UPDATE_INTERVAL must be an integer".to_owned())?
        .unwrap_or(Duration::from_secs(60 * 60));
    let mut update_policy = env::var("ZIGZAG_UPDATE_POLICY")
        .ok()
        .map(|value| update::Policy::parse(&value))
        .transpose()?
        .unwrap_or(update::Policy::Enabled);
    let mut update_ready_file = None;
    let mut values = arguments.into_iter();
    while let Some(argument) = values.next() {
        let value = |values: &mut std::vec::IntoIter<String>, name: &str| {
            values
                .next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match argument.as_str() {
            "--secret-file" => secret_file = Some(PathBuf::from(value(&mut values, "--secret-file")?)),
            "--control-secret-file" => control_secret_file = Some(PathBuf::from(value(&mut values, "--control-secret-file")?)),
            "--state-file" => state_file = Some(PathBuf::from(value(&mut values, "--state-file")?)),
            "--port" => port = value(&mut values, "--port")?.parse().map_err(|_| "--port must be a valid u16".to_owned())?,
            "--tailscale-ip" => {
                let address = value(&mut values, "--tailscale-ip")?
                    .parse()
                    .map_err(|_| "--tailscale-ip must be an IP address".to_owned())?;
                if !is_tailscale_ipv4(address) {
                    return Err("--tailscale-ip must be a Tailscale IPv4 address".to_owned());
                }
                tailscale_ip = Some(address);
            }
            "--max-events" => max_events = value(&mut values, "--max-events")?.parse().map_err(|_| "--max-events must be a positive integer".to_owned())?,
            "--watch-repo" => {
                let repo = value(&mut values, "--watch-repo")?;
                if !valid_github_repo(&repo) {
                    return Err("--watch-repo must be an OWNER/REPO GitHub name".to_owned());
                }
                if !github_watch_repos.contains(&repo) {
                    github_watch_repos.push(repo);
                }
            }
            "--watch-interval" => {
                let seconds = value(&mut values, "--watch-interval")?
                    .parse::<u64>()
                    .map_err(|_| "--watch-interval must be an integer".to_owned())?;
                if !(30..=3600).contains(&seconds) {
                    return Err("--watch-interval must be between 30 and 3600 seconds".to_owned());
                }
                github_watch_interval = Duration::from_secs(seconds);
            }
            "--update-dir" => update_directory = Some(PathBuf::from(value(&mut values, "--update-dir")?)),
            "--update-interval" => {
                let seconds = value(&mut values, "--update-interval")?.parse::<u64>()
                    .map_err(|_| "--update-interval must be an integer".to_owned())?;
                if seconds > 24 * 60 * 60 { return Err("--update-interval must be at most 86400 seconds".to_owned()); }
                update_interval = Duration::from_secs(seconds);
            }
            "--update-policy" => update_policy = update::Policy::parse(&value(&mut values, "--update-policy")?)?,
            "--update-ready-file" => update_ready_file = Some(PathBuf::from(value(&mut values, "--update-ready-file")?)),
            "--help" | "-h" => return Err("usage: zigzag --secret-file PATH --state-file PATH [--control-secret-file PATH] [--port 8765] [--max-events 1000] [--watch-repo OWNER/REPO] [--watch-interval 30] [--update-dir PATH] [--update-interval 3600] [--update-policy enabled|paused|pin:VERSION]".to_owned()),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let secret_file =
        secret_file.ok_or_else(|| "--secret-file or ZIGZAG_SECRET_FILE is required".to_owned())?;
    let state_file =
        state_file.ok_or_else(|| "--state-file or ZIGZAG_STATE_FILE is required".to_owned())?;
    if max_events == 0 {
        return Err("--max-events must be greater than zero".to_owned());
    }
    let agent_registry_file = state_file.with_extension("agents.json");
    let update_directory = update_directory.unwrap_or_else(|| {
        state_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("relay")
    });
    let review_state_file = state_file.with_extension("reviews.json");
    Ok(Config {
        secret_file,
        control_secret_file,
        state_file,
        agent_registry_file,
        port,
        tailscale_ip,
        max_events,
        github_watch_repos,
        github_watch_interval,
        update_directory,
        update_interval,
        update_policy,
        update_ready_file,
        review_state_file,
    })
}
pub(crate) fn valid_github_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && owner
            .bytes()
            .chain(name.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
pub(crate) fn run_config(arguments: &[String]) -> Result<(), String> {
    require_gui_login_session()?;
    if is_get_allowlist(arguments) {
        let policy = exec::load_policy()?;
        println!("{}", policy.canonical_json());
        return Ok(());
    }
    let file = allowlist_file(arguments)?;
    let contents = std::fs::read_to_string(file)
        .map_err(|error| format!("could not read allowlist file {file}: {error}"))?;
    let policy = exec::Policy::parse(&contents)?;
    exec::store_policy(&policy)?;
    println!("{}", policy.canonical_json());
    Ok(())
}
pub(crate) fn is_get_allowlist(arguments: &[String]) -> bool {
    arguments.len() == 1 && arguments[0] == "get-allowlist"
}
pub(crate) fn allowlist_file(arguments: &[String]) -> Result<&str, String> {
    let [command, flag, file] = arguments else {
        return Err("usage: zigzag config (get-allowlist | set-allowlist --file PATH)".to_owned());
    };
    if command != "set-allowlist" || flag != "--file" || file.is_empty() {
        return Err("usage: zigzag config (get-allowlist | set-allowlist --file PATH)".to_owned());
    }
    Ok(file)
}
