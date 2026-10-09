use crate::http::{error, reply};
use crate::server::Server;
use relay_core::{Json, parse_json};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;

// --- Worktree management endpoints (agent-creation rollout, part 1 of 4) ---
/// Roots the relay may create or remove git worktrees under. Candidate paths
/// are canonicalized before the prefix check, so `..` segments and symlinks
/// cannot escape the root.
pub(crate) const WORKTREE_ALLOWED_ROOTS: [&str; 2] =
    ["/private/tmp/", "/Users/shukant/.codex/worktrees/"];
/// Root the `repo` parameter of worktree creation must live under, so callers
/// cannot point `git worktree add` at an arbitrary repository.
pub(crate) const WORKTREE_REPO_ROOT: &str = "/Users/shukant/Workspace/";
/// Failure from the worktree core logic: the HTTP status and the snake_case
/// error body the relay replies with. Handlers stay thin so tests can drive
/// `worktree_create_plan` / `worktree_delete_plan` directly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorktreeError {
    pub(crate) code: u16,
    pub(crate) message: &'static str,
}
/// Canonicalize each configured root, dropping roots that do not exist. An
/// empty result rejects every path (fail closed).
pub(crate) fn canonical_worktree_roots() -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = std::env::var_os("ZIGZAG_WORKTREE_ROOTS")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_else(|| WORKTREE_ALLOWED_ROOTS.iter().map(PathBuf::from).collect());
    roots
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}

/// Root accepted for repositories used by worktree and agent creation.
///
/// The environment override makes it possible to run an isolated relay on a
/// non-macOS host (including CI) without weakening the production default.
pub(crate) fn configured_worktree_repo_root() -> PathBuf {
    let root = std::env::var_os("ZIGZAG_WORKTREE_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(WORKTREE_REPO_ROOT));
    std::fs::canonicalize(&root).unwrap_or(root)
}
/// Canonicalize `path` (which must exist) and require it to sit under `roots`.
pub(crate) fn canonical_path_under_roots(
    path: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, WorktreeError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| WorktreeError {
        code: 400,
        message: "worktree_path_not_found",
    })?;
    if roots.iter().any(|root| canonical.starts_with(root)) {
        Ok(canonical)
    } else {
        Err(WorktreeError {
            code: 400,
            message: "worktree_path_outside_allowed_roots",
        })
    }
}
/// Resolve the worktree path for creation. The path itself may not exist yet,
/// so canonicalize the parent directory and re-attach the leaf: canonicalizing
/// the parent defeats `..` traversal and symlink escapes in every ancestor.
pub(crate) fn resolve_new_worktree_path(
    raw: &str,
    roots: &[PathBuf],
) -> Result<PathBuf, WorktreeError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_relative() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_path_must_be_absolute",
        });
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(WorktreeError {
            code: 400,
            message: "worktree_path_has_no_parent",
        })?;
    let leaf = path.file_name().ok_or(WorktreeError {
        code: 400,
        message: "worktree_path_has_no_name",
    })?;
    Ok(canonical_path_under_roots(parent, roots)?.join(leaf))
}
/// Resolve the worktree path for deletion: it must already exist.
pub(crate) fn resolve_existing_worktree_path(
    raw: &str,
    roots: &[PathBuf],
) -> Result<PathBuf, WorktreeError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_relative() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_path_must_be_absolute",
        });
    }
    canonical_path_under_roots(path, roots)
}
/// Resolve the `repo` parameter: it must exist and live under the workspace
/// root.
pub(crate) fn resolve_worktree_repo(raw: &str, repo_root: &Path) -> Result<PathBuf, WorktreeError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_relative() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_repo_must_be_absolute",
        });
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| WorktreeError {
        code: 400,
        message: "worktree_repo_not_found",
    })?;
    if canonical.starts_with(repo_root) {
        Ok(canonical)
    } else {
        Err(WorktreeError {
            code: 400,
            message: "worktree_repo_outside_workspace",
        })
    }
}
/// Reject branch names git would treat as options or refuse as ref names.
/// `git` itself is the final arbiter; this keeps hostile input from ever
/// reaching the command line.
pub(crate) fn valid_worktree_branch(branch: &str) -> bool {
    if branch.is_empty() || branch.len() > 255 {
        return false;
    }
    if branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with(".lock")
    {
        return false;
    }
    if branch.contains("..") || branch.contains("@{") {
        return false;
    }
    !branch
        .chars()
        .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}
pub(crate) fn git_output(
    repo: &Path,
    args: &[&str],
) -> Result<std::process::Output, WorktreeError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| {
            log::error!("worktree git invocation failed: {error}");
            WorktreeError {
                code: 500,
                message: "worktree_git_failed",
            }
        })
}
/// True when `refs/heads/<branch>` exists. Exit 0 means present, exit 1 means
/// absent; anything else is a genuine git failure.
pub(crate) fn worktree_branch_exists(repo: &Path, branch: &str) -> Result<bool, WorktreeError> {
    let output = git_output(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(WorktreeError {
            code: 500,
            message: "worktree_git_failed",
        }),
    }
}
/// Return the worktree path where the branch is checked out, if any.
pub(crate) fn worktree_branch_checkout_path(
    repo: &Path,
    branch: &str,
) -> Result<Option<PathBuf>, WorktreeError> {
    let output = git_output(repo, &["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        return Err(WorktreeError {
            code: 500,
            message: "worktree_git_failed",
        });
    }
    let mut path = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(worktree_path) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(worktree_path));
        } else if line == format!("branch refs/heads/{branch}") {
            return Ok(path);
        } else if line.is_empty() {
            path = None;
        }
    }
    Ok(None)
}
/// True when the branch is already checked out in some worktree, which `git
/// worktree add` would refuse.
pub(crate) fn worktree_branch_checked_out(
    repo: &Path,
    branch: &str,
) -> Result<bool, WorktreeError> {
    Ok(worktree_branch_checkout_path(repo, branch)?.is_some())
}
/// Core of `POST /v1/worktrees`: validate, then run `git worktree add`,
/// creating the branch when it does not exist yet. Returns the 200 body.
pub(crate) fn worktree_create_plan(
    path_raw: &str,
    branch: &str,
    repo_raw: &str,
    roots: &[PathBuf],
    repo_root: &Path,
) -> Result<Json, WorktreeError> {
    let path = resolve_new_worktree_path(path_raw, roots)?;
    let repo = resolve_worktree_repo(repo_raw, repo_root)?;
    if !valid_worktree_branch(branch) {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_invalid_branch",
        });
    }
    if !git_output(&repo, &["rev-parse", "--git-dir"])?
        .status
        .success()
    {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_repo_not_a_git_repo",
        });
    }
    if worktree_branch_checked_out(&repo, branch)? {
        log::warn!("worktree_create refused: branch {branch} already checked out");
        return Err(WorktreeError {
            code: 400,
            message: "worktree_branch_already_checked_out",
        });
    }
    let path_str = path.to_str().ok_or(WorktreeError {
        code: 400,
        message: "worktree_path_not_unicode",
    })?;
    let output = if worktree_branch_exists(&repo, branch)? {
        log::info!(
            "worktree_create path={} branch={branch} existing_branch=true",
            path.display()
        );
        git_output(&repo, &["worktree", "add", path_str, branch])?
    } else {
        log::info!(
            "worktree_create path={} branch={branch} existing_branch=false",
            path.display()
        );
        git_output(&repo, &["worktree", "add", "-b", branch, path_str])?
    };
    if !output.status.success() {
        log::warn!(
            "worktree_create git failed for branch={branch}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Err(WorktreeError {
            code: 400,
            message: "worktree_git_add_failed",
        });
    }
    Ok(Json::Object(vec![
        (
            "path".to_owned(),
            Json::String(path.to_string_lossy().into_owned()),
        ),
        ("branch".to_owned(), Json::String(branch.to_owned())),
    ]))
}
/// Core of `DELETE /v1/worktrees`. `agents` carries `(state, command)` pairs
/// from the agent registry for the live-attachment check. Returns the 200 body.
pub(crate) fn worktree_delete_plan(
    path_raw: &str,
    roots: &[PathBuf],
    agents: &[(&str, &str)],
) -> Result<Json, WorktreeError> {
    let path = resolve_existing_worktree_path(path_raw, roots)?;
    let path_str = path.to_string_lossy();
    // TODO: match on an explicit worktree_path field on the agent record once
    // the agent-creation endpoints record it; command-substring matching is a
    // stopgap until then.
    let attached = agents.iter().any(|(state, command)| {
        matches!(*state, "running" | "orphaned")
            && (command.contains(&*path_str) || command.contains(path_raw))
    });
    if attached {
        log::warn!(
            "worktree_delete refused: live agent attached to {}",
            path.display()
        );
        return Err(WorktreeError {
            code: 409,
            message: "worktree_in_use",
        });
    }
    // `git worktree remove` runs from the owning repo: resolve the main repo
    // through the worktree's common git dir.
    let common = git_output(&path, &["rev-parse", "--git-common-dir"])?;
    if !common.status.success() {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_not_a_git_worktree",
        });
    }
    let common_dir = String::from_utf8_lossy(&common.stdout);
    let common_dir = common_dir.trim();
    let common_dir = if Path::new(common_dir).is_absolute() {
        PathBuf::from(common_dir)
    } else {
        path.join(common_dir)
    };
    let common_dir = std::fs::canonicalize(&common_dir).map_err(|_| WorktreeError {
        code: 400,
        message: "worktree_not_a_git_worktree",
    })?;
    if common_dir.file_name().is_none_or(|name| name != ".git") {
        return Err(WorktreeError {
            code: 400,
            message: "worktree_not_a_git_worktree",
        });
    }
    let repo = common_dir
        .parent()
        .ok_or(WorktreeError {
            code: 400,
            message: "worktree_not_a_git_worktree",
        })?
        .to_path_buf();
    log::info!("worktree_delete path={}", path.display());
    let remove = git_output(&repo, &["worktree", "remove", "--force", &path_str])?;
    if !remove.status.success() {
        log::warn!(
            "worktree_delete remove failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&remove.stderr).trim()
        );
        return Err(WorktreeError {
            code: 400,
            message: "worktree_git_remove_failed",
        });
    }
    // Prune stale administrative entries (e.g. worktrees deleted by hand).
    if let Err(error) = git_output(&repo, &["worktree", "prune"]) {
        log::warn!("worktree_delete prune failed: {}", error.message);
    }
    Ok(Json::Object(vec![
        ("removed".to_owned(), Json::Bool(true)),
        ("path".to_owned(), Json::String(path_str.into_owned())),
    ]))
}
pub(crate) fn worktree_request_fields(
    body: &[u8],
    allowed: &[&str],
) -> Result<Vec<(String, Json)>, WorktreeError> {
    let text = std::str::from_utf8(body).map_err(|_| WorktreeError {
        code: 400,
        message: "invalid_worktree_request",
    })?;
    let parsed = parse_json(text).map_err(|_| WorktreeError {
        code: 400,
        message: "invalid_worktree_request",
    })?;
    let Json::Object(fields) = parsed else {
        return Err(WorktreeError {
            code: 400,
            message: "invalid_worktree_request",
        });
    };
    if fields
        .iter()
        .any(|(name, _)| !allowed.contains(&name.as_str()))
    {
        return Err(WorktreeError {
            code: 400,
            message: "invalid_worktree_request",
        });
    }
    Ok(fields)
}
pub(crate) fn worktree_string_field(
    fields: &[(String, Json)],
    name: &str,
) -> Result<String, WorktreeError> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(WorktreeError {
            code: 400,
            message: "invalid_worktree_request",
        })
}
pub(crate) fn worktree_create(stream: &mut TcpStream, body: Vec<u8>) -> Result<(), String> {
    let fields = match worktree_request_fields(&body, &["path", "branch", "repo"]) {
        Ok(fields) => fields,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let (path, branch, repo) = match (
        worktree_string_field(&fields, "path"),
        worktree_string_field(&fields, "branch"),
        worktree_string_field(&fields, "repo"),
    ) {
        (Ok(path), Ok(branch), Ok(repo)) => (path, branch, repo),
        _ => return reply(stream, 400, error("invalid_worktree_request")),
    };
    let roots = canonical_worktree_roots();
    let repo_root = configured_worktree_repo_root();
    match worktree_create_plan(&path, &branch, &repo, &roots, &repo_root) {
        Ok(response) => reply(stream, 200, response),
        Err(failure) => reply(stream, failure.code, error(failure.message)),
    }
}
pub(crate) fn worktree_delete(
    stream: &mut TcpStream,
    state: &Server,
    body: Vec<u8>,
) -> Result<(), String> {
    let fields = match worktree_request_fields(&body, &["path"]) {
        Ok(fields) => fields,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let path = match worktree_string_field(&fields, "path") {
        Ok(path) => path,
        Err(failure) => return reply(stream, failure.code, error(failure.message)),
    };
    let agents = state.supervisor.registry.list(None, None);
    let commands: Vec<(&str, &str)> = agents
        .iter()
        .map(|agent| (agent.state.as_str(), agent.command.as_str()))
        .collect();
    match worktree_delete_plan(&path, &canonical_worktree_roots(), &commands) {
        Ok(response) => reply(stream, 200, response),
        Err(failure) => reply(stream, failure.code, error(failure.message)),
    }
}
