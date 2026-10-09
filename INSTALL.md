# Install Zigzag on macOS

This guide builds and installs the Zigzag relay and its `zzapi` command-line
client from source. The relay runs as a per-user macOS LaunchAgent. The
repository also includes a Python department status TUI; see
[`docs/dept.md`](docs/dept.md) for that tool.

## 1. Prerequisites

- A Mac with a GUI user account. The LaunchAgent must run in that user's login
  session so macOS Keychain access works.
- Xcode Command Line Tools (`xcode-select --install`) and a current stable
  Rust toolchain installed with [rustup](https://rustup.rs/). The workspace
  uses Rust edition 2024.
- Git and OpenSSL (the `openssl` command is used to create a random token).
- Tailscale installed and connected if other tailnet devices need to reach the
  relay. The daemon runs `tailscale ip -4` at startup; ensure `tailscale` is on
  the LaunchAgent's `PATH`.

Clone the repository and enter its root:

```sh
git clone https://github.com/ShukantPal/zigzag.git
cd zigzag
```

## 2. Build the relay and CLI

Build the release binaries from the repository root:

```sh
cargo build --release -p zigzag
cargo build --release -p zzapi
```

The binaries are `target/release/zigzag` (the relay daemon) and
`target/release/zzapi` (the API client). Install `zzapi` somewhere on your
`PATH`, for example:

```sh
mkdir -p ~/bin
cp target/release/zzapi ~/bin/
```

Add `~/bin` to your shell's `PATH` if needed. Keep the relay binary at a stable
absolute path because `launchd` does not use your interactive shell's `PATH`.
If you rebuild it later, re-signing with the same signing identity may be
needed to preserve Keychain access; see the release signing section in
[`README.md`](README.md#release-signing-and-deploy-mac).

## 3. Create the relay token and state directory

The token is a bearer credential for the relay API. Keep it private and do not
put it in the plist, shell history, or source control.

```sh
mkdir -p ~/.codex/zigzag ~/Library/LaunchAgents
chmod 700 ~/.codex/zigzag
umask 077
openssl rand -hex 32 > ~/.codex/zigzag/zigzag.token
chmod 600 ~/.codex/zigzag/zigzag.token
```

The relay creates its state file at `~/.codex/zigzag/events.json` on first
launch. The token file must not be group- or world-readable and must contain
at least 32 bytes.

## 4. Install the LaunchAgent

Edit `launchd/com.shukantpal.zigzag.plist` and replace every
`/REPLACE/...` or `/Users/REPLACE` placeholder with absolute paths for your
account and checkout. In `ProgramArguments`, use the relay binary followed by
these arguments:

```text
/absolute/path/to/zigzag/target/release/zigzag
--secret-file
/Users/YOU/.codex/zigzag/zigzag.token
--state-file
/Users/YOU/.codex/zigzag/events.json
```

The checked-in template also contains example `--watch-repo` arguments for
`leveled-inc/leveled`. Remove those arguments unless you intend to enable that
legacy repository watcher. Preserve the log paths or change them to writable
absolute paths. If `tailscale` is not in launchd's default `PATH`, add an
`EnvironmentVariables` dictionary with a `PATH` entry that includes its
directory.

Copy the edited plist and start it from the Mac's logged-in GUI session:

```sh
cp launchd/com.shukantpal.zigzag.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" \
  ~/Library/LaunchAgents/com.shukantpal.zigzag.plist
```

To apply later plist changes, run `launchctl bootout` for this job and
`launchctl bootstrap` it again from the same GUI session. Do not bootstrap or
restart the service over SSH; Keychain operations can hang outside the GUI
session.

## 5. Configure `zzapi`

For local use on the Mac, point `zzapi` at loopback and give it the token file.
The CLI accepts `--hostname` and `--token-file` as global options before the
subcommand:

```sh
zzapi --hostname 127.0.0.1:8765 \
  --token-file ~/.codex/zigzag/zigzag.token health
```

To avoid repeating flags, set environment variables in your shell:

```sh
export ZIGZAG_HOSTNAME=127.0.0.1:8765
export ZIGZAG_TOKEN_FILE="$HOME/.codex/zigzag/zigzag.token"
```

When connecting from another machine, use the Mac's Tailscale IPv4 address as
`ZIGZAG_HOSTNAME` (with port `8765`), and configure Tailscale ACLs to allow only
the devices that need access. The relay binds to loopback and its Tailscale
address; it must not be exposed on a public interface. See
[`README.md`](README.md#network-security-tailscale-acls) for the recommended
ACL policy. `ZIGZAG_PROXY` can be set when a client needs an HTTP proxy.

The CLI token lookup also supports `ZIGZAG_TOKEN`, `ZIGZAG_TOKEN_FILE`, and
the default token locations documented in [`cli/README.md`](cli/README.md).
Prefer the private token file over placing the token value in an environment
variable.

## 6. Verify the installation

Check that launchd loaded the job, inspect the daemon's error log if needed,
then make an authenticated health request:

```sh
launchctl print "gui/$(id -u)/com.shukantpal.zigzag"
tail -n 50 ~/.codex/zigzag/zigzag.error.log
zzapi --hostname 127.0.0.1:8765 \
  --token-file ~/.codex/zigzag/zigzag.token health
```

The health command should succeed. You can also confirm that the listener is
local and on the tailnet by checking the daemon log and running
`tailscale ip -4`. A client on an ACL-allowed tailnet device should reach the
relay; an unauthenticated request should return HTTP `401`.

## Troubleshooting

- **LaunchAgent is missing or exits:** confirm the plist has no placeholders,
  all executable and file paths are absolute, and the log directory exists
  and is writable. Inspect `launchctl print` and
  `~/.codex/zigzag/zigzag.error.log`.
- **Daemon cannot find Tailscale:** run `command -v tailscale`; add its
  containing directory to the LaunchAgent `PATH`, then reload the job.
- **Health returns 401:** check that `zzapi` and the daemon use the same token
  file. Keep its permissions at `0600` and regenerate it only if you also
  update the daemon's token file and restart the service.
- **Health cannot connect on the Mac:** verify the LaunchAgent is loaded and
  listening on port 8765. Check the error log and make sure the local client
  uses `127.0.0.1:8765`.
- **Tailnet clients cannot connect:** compare the current `tailscale ip -4`
  with the configured ACL destination, confirm the client is allowed by the
  ACL, and use the Mac's current Tailscale IP with port 8765.
- **Keychain allowlist commands hang or fail:** run them from the Mac's GUI
  login session, not SSH. Details are in
  [`launchd/INSTALL.md`](launchd/INSTALL.md#exec-endpoint).

For additional daemon operations and CLI commands, see the
[`documentation index`](docs/README.md), [`launchd/INSTALL.md`](launchd/INSTALL.md),
and [`cli/README.md`](cli/README.md).
