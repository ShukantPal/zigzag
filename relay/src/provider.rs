//! Agent provider abstraction.
//!
//! The relay can spawn agents from multiple CLIs (Codex, Gemini, OpenCode,
//! Grok). Each provider knows how to invoke its CLI non-interactively:
//! how to pass the prompt, which flags enable headless/JSON mode, and
//! how to point it at a working directory.
//!
//! Providers build an argv; the relay still verifies the binary against
//! the exec policy allowlist before spawning. A provider is just a
//! command-line recipe, not a security boundary.

use std::path::Path;

/// Look up a binary on PATH using only the standard library.
fn bin_on_path(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file()
            && let Ok(meta) = std::fs::metadata(&candidate)
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                return true;
            }
        }
    }
    false
}

/// Options for spawning an agent with a specific provider.
///
/// Consumed by the upcoming POST /v1/agents endpoint.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct AgentOpts {
    /// The prompt text to send to the agent.
    pub prompt: String,
    /// Working directory for the agent.
    pub project_dir: String,
    /// Optional model override (provider-specific format).
    pub model: Option<String>,
    /// Optional timeout in seconds.
    pub timeout_secs: Option<u64>,
}

/// A provider knows how to spawn its CLI non-interactively.
pub(crate) trait Provider {
    /// Stable name used in the API (e.g. "codex", "gemini").
    fn name(&self) -> &'static str;

    /// Binary name as allowlisted in the exec policy.
    fn bin(&self) -> &'static str;

    /// Build the argv (excluding the binary itself) for a non-interactive run.
    ///
    /// The prompt is passed in the way each CLI expects it. Implementations
    /// must not interpolate unsanitized input into shell; the relay spawns
    /// with no shell, so argv entries are passed literally.
    /// Builds the argv for a non-interactive run. Used by POST /v1/agents.
    #[allow(dead_code)]
    fn spawn_argv(&self, opts: &AgentOpts) -> Result<Vec<String>, String>;

    /// Whether the provider's binary appears to be installed.
    fn is_available(&self) -> bool {
        bin_on_path(self.bin())
    }
}

/// Parse a provider name from the API into a boxed provider.
/// Unknown names are an error; callers map this to a 400.
pub(crate) fn provider_from_name(name: &str) -> Result<Box<dyn Provider>, String> {
    match name {
        "codex" => Ok(Box::new(CodexProvider)),
        "gemini" => Ok(Box::new(GeminiProvider)),
        "opencode" => Ok(Box::new(OpenCodeProvider)),
        "grok" => Ok(Box::new(GrokProvider)),
        _ => Err(format!("unknown provider: {name}")),
    }
}

/// All known provider names, for API documentation and validation.
pub(crate) const PROVIDER_NAMES: &[&str] = &["codex", "gemini", "opencode", "grok"];

/// Default provider when the API request omits one.
pub(crate) const DEFAULT_PROVIDER: &str = "codex";

// --- Codex ---

pub(crate) struct CodexProvider;

impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }
    fn bin(&self) -> &'static str {
        "codex"
    }
    fn spawn_argv(&self, opts: &AgentOpts) -> Result<Vec<String>, String> {
        // `codex exec` reads the prompt from argv (or stdin). --json gives
        // structured thread events on stdout. --approve-for-me implies the
        // workspace-write sandbox; -C sets the working directory.
        let mut argv = vec![
            "exec".to_owned(),
            "--json".to_owned(),
            "--approve-for-me".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "-C".to_owned(),
            opts.project_dir.clone(),
        ];
        if let Some(model) = &opts.model {
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        argv.push(opts.prompt.clone());
        Ok(argv)
    }
    fn is_available(&self) -> bool {
        // Codex may be on PATH via npm or Nix; check a few known locations.
        bin_on_path("codex")
            || Path::new("/run/current-system/sw/bin/codex").exists()
            || Path::new("/opt/homebrew/bin/codex").exists()
    }
}

// --- Gemini CLI ---

pub(crate) struct GeminiProvider;

impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }
    fn bin(&self) -> &'static str {
        "gemini"
    }
    fn spawn_argv(&self, opts: &AgentOpts) -> Result<Vec<String>, String> {
        // `gemini -p` runs a single non-interactive prompt and exits.
        // --output-format json gives structured output.
        let mut argv = vec!["-p".to_owned(), opts.prompt.clone()];
        if let Some(model) = &opts.model {
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        // Gemini CLI respects cwd; the relay sets it via spawn options.
        let _ = &opts.project_dir;
        Ok(argv)
    }
}

// --- OpenCode ---

pub(crate) struct OpenCodeProvider;

impl Provider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }
    fn bin(&self) -> &'static str {
        "opencode"
    }
    fn spawn_argv(&self, opts: &AgentOpts) -> Result<Vec<String>, String> {
        // `opencode run` with `--` before the prompt so prompt text is never
        // parsed as flags. Stdin must be closed (the relay does this) or
        // `opencode run` waits for an interactive session.
        // NOTE: for the attested pilot flow, use the `opencode-launch`
        // wrapper script (PR #10) instead of invoking `opencode` directly.
        let mut argv = vec!["run".to_owned()];
        if let Some(model) = &opts.model {
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        argv.push("--".to_owned());
        argv.push(opts.prompt.clone());
        Ok(argv)
    }
    fn is_available(&self) -> bool {
        bin_on_path("opencode") || Path::new("/run/current-system/sw/bin/opencode").exists()
    }
}

// --- Grok ---

pub(crate) struct GrokProvider;

impl Provider for GrokProvider {
    fn name(&self) -> &'static str {
        "grok"
    }
    fn bin(&self) -> &'static str {
        "grok"
    }
    fn spawn_argv(&self, opts: &AgentOpts) -> Result<Vec<String>, String> {
        // `grok -p` runs a single-turn prompt headlessly and exits.
        // --output-format json gives structured output.
        let mut argv = vec![
            "-p".to_owned(),
            opts.prompt.clone(),
            "--output-format".to_owned(),
            "json".to_owned(),
            "--always-approve".to_owned(),
        ];
        if let Some(model) = &opts.model {
            argv.push("--model".to_owned());
            argv.push(model.clone());
        }
        let _ = &opts.project_dir;
        Ok(argv)
    }
    fn is_available(&self) -> bool {
        bin_on_path("grok") || Path::new("/run/current-system/sw/bin/grok").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> AgentOpts {
        AgentOpts {
            prompt: "hello".to_owned(),
            project_dir: "/tmp/proj".to_owned(),
            model: None,
            timeout_secs: None,
        }
    }

    #[test]
    fn provider_names_round_trip() {
        for name in PROVIDER_NAMES {
            let p = provider_from_name(name).expect("known provider");
            assert_eq!(p.name(), *name);
        }
    }

    #[test]
    fn unknown_provider_errors() {
        assert!(provider_from_name("claude").is_err());
    }

    #[test]
    fn codex_argv_shape() {
        let argv = CodexProvider.spawn_argv(&opts()).unwrap();
        assert_eq!(argv[0], "exec");
        assert!(argv.contains(&"--json".to_owned()));
        assert!(argv.contains(&"-C".to_owned()));
        assert!(argv.contains(&"/tmp/proj".to_owned()));
        assert_eq!(argv.last().unwrap(), "hello");
    }

    #[test]
    fn opencode_argv_uses_double_dash() {
        let argv = OpenCodeProvider.spawn_argv(&opts()).unwrap();
        assert_eq!(argv[0], "run");
        let dash = argv.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(argv[dash + 1], "hello");
    }

    #[test]
    fn grok_argv_headless() {
        let argv = GrokProvider.spawn_argv(&opts()).unwrap();
        assert!(argv.contains(&"-p".to_owned()));
        assert!(argv.contains(&"--output-format".to_owned()));
        assert!(argv.contains(&"json".to_owned()));
    }

    #[test]
    fn gemini_argv_prompt_flag() {
        let argv = GeminiProvider.spawn_argv(&opts()).unwrap();
        assert_eq!(argv[0], "-p");
        assert_eq!(argv[1], "hello");
    }

    #[test]
    fn model_flag_propagates() {
        let mut o = opts();
        o.model = Some("gpt-5".to_owned());
        let argv = CodexProvider.spawn_argv(&o).unwrap();
        let i = argv.iter().position(|a| a == "--model").unwrap();
        assert_eq!(argv[i + 1], "gpt-5");
    }
}
