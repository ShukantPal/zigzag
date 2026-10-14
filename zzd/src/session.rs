use crate::proc::COMPAT_OUTPUT_CAP;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::process::Command;

#[cfg(any(target_os = "macos", test))]
pub(crate) const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
#[cfg(any(target_os = "macos", test))]
pub(crate) const SESSION_IS_REMOTE: u32 = 0x1000;
#[derive(Default)]
pub(crate) struct CappedOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
    pub(crate) complete: bool,
}

impl CappedOutput {
    pub(crate) fn append(&mut self, bytes: &[u8]) {
        let room = COMPAT_OUTPUT_CAP.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.truncated |= bytes.len() > room;
    }

    pub(crate) fn snapshot(&self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.truncated,
        )
    }
}
#[cfg(any(target_os = "macos", test))]
pub(crate) fn is_local_gui_session(status: i32, attributes: u32) -> bool {
    status == 0
        && attributes & SESSION_HAS_GRAPHIC_ACCESS != 0
        && attributes & SESSION_IS_REMOTE == 0
}
/// Policy updates are intentionally an owner action from the local Aqua
/// session, never an SSH action. Keychain access alone does not establish
/// which terminal invoked this executable, so check the caller's session too.
#[cfg(target_os = "macos")]
pub(crate) fn require_gui_login_session() -> Result<(), String> {
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
    }

    let mut session_id = 0;
    let mut attributes = 0;
    // `callerSecuritySession` asks macOS about this process's session.
    let status =
        unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut session_id, &mut attributes) };
    if is_local_gui_session(status, attributes) {
        Ok(())
    } else {
        Err(
            "privileged daemon operations require Shukant's local macOS GUI login session"
                .to_owned(),
        )
    }
}
/// Policy editing is a local owner action. A GUI login session by itself is
/// inherited by every relay child, so also require stdin to be the foreground
/// terminal. `/v1/exec` and `/v1/spawn` explicitly replace child stdin with
/// `/dev/null`, which makes this check fail for relay-launched commands.
pub(crate) fn require_interactive_gui_login_session() -> Result<(), String> {
    if !is_foreground_terminal(std::io::stdin().is_terminal()) {
        return Err("allowlist changes require a foreground local terminal".to_owned());
    }
    require_gui_login_session()
}

fn is_foreground_terminal(stdin_is_terminal: bool) -> bool {
    // `tcgetpgrp` verifies that this process group currently owns the
    // terminal, so a background job cannot mutate policy through a reused fd.
    let foreground_group = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
    interactive_terminal_allowed(stdin_is_terminal, foreground_group, unsafe {
        libc::getpgrp()
    })
}

fn interactive_terminal_allowed(
    stdin_is_terminal: bool,
    foreground_group: i32,
    process_group: i32,
) -> bool {
    stdin_is_terminal && foreground_group >= 0 && foreground_group == process_group
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn require_gui_login_session() -> Result<(), String> {
    Err("privileged daemon operations require Shukant's local macOS GUI login session".to_owned())
}
pub(crate) fn resolve_tailscale_ip() -> Result<IpAddr, String> {
    let output = Command::new("tailscale").args(["ip", "-4"]).output().map_err(|error| format!("could not run tailscale ip -4: {error}; use --tailscale-ip only for explicit test/development overrides"))?;
    if !output.status.success() {
        return Err("tailscale ip -4 failed; Zigzag will not bind broadly".to_owned());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "tailscale ip -4 produced non-UTF-8 output".to_owned())?;
    stdout
        .lines()
        .next()
        .ok_or_else(|| "tailscale ip -4 returned no address".to_owned())?
        .parse()
        .map_err(|_| "tailscale ip -4 returned an invalid address".to_owned())
}
pub(crate) fn is_tailscale_ipv4(address: IpAddr) -> bool {
    matches!(address, IpAddr::V4(address) if address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
}

#[cfg(test)]
mod terminal_tests {
    use super::interactive_terminal_allowed;

    #[test]
    fn policy_editor_requires_foreground_tty_stdin() {
        assert!(interactive_terminal_allowed(true, 17, 17));
        assert!(!interactive_terminal_allowed(false, 17, 17));
        assert!(!interactive_terminal_allowed(true, 16, 17));
        assert!(!interactive_terminal_allowed(true, -1, 17));
    }
}
