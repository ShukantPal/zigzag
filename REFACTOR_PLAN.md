# Top-level Rust layout refactor plan

Status: research only. This document proposes a file move and the follow-up
reference updates; no source files or directories were moved for this plan.

## 1. Current top-level layout

The tracked top-level project folders and their roles are:

| Path | Contents and role |
| --- | --- |
| `relay/` | The `zigzag` daemon crate: HTTP/API routes, server, process and agent supervision, event/socket handling, provider and review-loop code, config schemas, prompts, integration tests, and the pinned Sigstore trust root. |
| `relay-core/` | The `relay-core` library crate: shared JSON, durable store, agent registry, parsing, and secret-file helpers. |
| `cli/` | The `zzapi` CLI crate and protocol integration tests. |
| `poller/` | The `poller` event long-poll consumer crate. |
| `dept/` | Python department/task tooling, relay status TUI, configuration, fixtures, tests, and department documentation/design notes. |
| `docs/` | User and developer documentation, including architecture, daemon, CLI, configuration, operations, and review-system guides. |
| `launchd/` | The macOS LaunchAgent plist template and installation instructions. |
| `scripts/` | Build/sign/verify/release helpers, end-to-end and launcher tests, and Git hook scripts. |
| `.github/` | GitHub Actions CI and ready-label auto-merge workflows. |

The root also contains the Cargo workspace (`Cargo.toml`, `Cargo.lock`),
README/install/operations/provenance material, `openapi.yaml`, and the review
loop design document. `.worktrees/` and `.zigzag/` are present locally but are
not tracked project source folders. Their local/worktree contents should not
be included in a source reorganization.

## 2. Cargo packages and dependency direction

The root workspace currently declares `members = ["relay-core", "relay",
"poller", "cli"]` with resolver 2 and shares version, edition, and license
metadata.

| Package | Manifest path | Targets | Workspace dependencies |
| --- | --- | --- | --- |
| `relay-core` | `relay-core/Cargo.toml` | Library crate `relay_core` | None on other workspace packages |
| `zigzag` | `relay/Cargo.toml` | Daemon binary `zigzag`, plus `e2e_binary` integration test | `relay-core` via `../relay-core` |
| `poller` | `poller/Cargo.toml` | Poller binary | `relay-core` via `../relay-core` |
| `zzapi` | `cli/Cargo.toml` | CLI binary `zzapi`, plus `protocol` integration test | None on other workspace packages |

Dependency graph:

```text
             ┌───────────┐
             │ relay-core│
             └─────▲─▲───┘
                   │ │
          ┌────────┘ └────────┐
          │                   │
     ┌────┴────┐         ┌────┴────┐
     │ zigzag  │         │ poller  │
     └─────────┘         └─────────┘

     ┌─────────┐
     │  zzapi  │  (no workspace-crate dependency)
     └─────────┘
```

Registry dependencies are independent of this graph. `zzapi` talks to the
daemon over its HTTP API rather than linking to its implementation crate.

## 3. Proposed organization

Move and rename the Rust crate directories as follows; keep Python, docs,
deployment, and general repository tooling at their current top-level paths.

| Current path | Proposed path | Package and executable names |
| --- | --- | --- |
| `relay-core/` | `zz/` | Rename package/crate `relay-core` / `relay_core` to `zz` / `zz` |
| `cli/` | `zzapi/` | Keep package and executable `zzapi` |
| `relay/` | `zzd/` | Keep package `zigzag` and executable `zigzag`; `zzd` is the source folder name |
| `poller/` | `poller/` | Keep package and executable `poller` |

Resulting top-level shape:

```text
zz/          shared core library
zzapi/       HTTP API client CLI
zzd/         daemon implementation and its trust/config assets
poller/      event poller
dept/        Python tooling and status view
docs/        repository documentation
launchd/     macOS LaunchAgent material
scripts/     repository scripts and hooks
.github/     workflows
```

Keeping the package and executable names for the daemon and CLI avoids
changing installed commands and release artifact names. The `zz` crate rename
is intentional and makes the shared crate name agree with its directory. If
preserving the Rust crate name is judged more valuable than a fully aligned
name, an alternative is to move `relay-core/` to `zz/` while retaining package
name `relay-core`; that reduces source edits but leaves a path/name mismatch.

### Cargo changes

1. Set workspace members to `zz`, `zzd`, `poller`, and `zzapi` (ordering is
   cosmetic); retain resolver 2 and shared workspace package metadata.
2. Move each crate's manifest with its source/tests/assets.
3. In `zzd/Cargo.toml` and `poller/Cargo.toml`, change the core dependency to
   `zz = { path = "../zz" }`.
4. In the moved core manifest, set `[package].name = "zz"`; source references
   become `use zz::...` and qualified `zz::...` names.
5. Regenerate/update `Cargo.lock` so its package and dependency entries use
   `zz` instead of `relay-core`. Registry dependency versions should not
   change as part of this move.
6. Keep the daemon package `zigzag`, binary name `zigzag`, CLI package/binary
   `zzapi`, and poller package/binary `poller`. Existing `cargo -p zigzag` and
   `cargo -p zzapi` release/build invocations then remain valid.

### Source imports and path references

Update all Rust imports and qualified names from `relay_core::` to `zz::` in
the daemon and poller. Update comments referring to the crate name. The
`zzapi` crate has no source dependency on the core.

Update literal repository paths wherever they identify moved files. Known
references include:

- `.github/workflows/ci.yml`: legacy registry fixture from
  `relay-core/tests/fixtures/...` to `zz/tests/fixtures/...`; release trust
  root from `relay/trust/...` to `zzd/trust/...`.
- `scripts/sign-ci-release.sh`, CI comments, docs, and `PROVENANCE.md`:
  `relay/src/...` paths to `zzd/src/...` and `relay/trust/...` to
  `zzd/trust/...`.
- Architecture/operations/relay documentation, install links, and
  `dept/status.py` comments that describe crate paths/names.
- The daemon test fixture that embeds `relay/src/main.rs` as a changed-file
  path: update it to `zzd/src/main.rs` if it is intended to represent a live
  repository path.
- `README.md`, `docs/zzapi.md`, and `INSTALL.md` links/references for
  `cli/README.md` should point to `zzapi/README.md`.

Do not change runtime state paths such as `~/.codex/zigzag/relay/`, the
LaunchAgent label, updater directory, API routes, or shipped binary names;
those are product/runtime interfaces rather than source-folder references.

### CI, release, and developer workflow updates

- Package selectors (`-p zigzag`, `-p zzapi`) can remain stable. Workspace-wide
  build, test, clippy, and fmt commands should continue to discover all
  packages from the root manifest.
- Update the two literal CI asset paths above. The workflow currently has no
  path filters, so no filter adjustment is required; if path filters are added
  later, include all four crate directories and root Cargo files.
- Preserve `target/debug/zigzag`, `target/debug/zzapi`, release output names,
  and the e2e script's binary arguments. The e2e script itself derives the
  repository root from its location and should not need changes for crate
  moves.
- Check release signing and verification scripts for comments and trust-root
  paths. `scripts/sign-release.sh` builds the `zigzag` package by default and
  consumes `target/release/zigzag`; those remain stable.
- Keep `launchd/com.shukantpal.zigzag.plist`'s configured executable path
  semantics intact. It points to an installed absolute binary, not a source
  folder.
- Refresh architecture tables, source links, contributor instructions, and
  any generated or published documentation that quotes old paths.

## 4. Risks and validation points

| Area | Risk | Mitigation / check |
| --- | --- | --- |
| Cargo workspace | A missing member path, stale path dependency, or incomplete core crate rename prevents metadata/build resolution. | Update root members and both path dependencies together; inspect `cargo metadata`; build and test the full workspace. |
| Rust imports | `relay_core::` remains in a less obvious module, test, or doc test after renaming the crate. | Search the full tracked tree for `relay_core` and `relay-core`; compile all targets. |
| CI smoke steps | The legacy registry fixture copy references the old core directory. | Update its path and confirm the daemon smoke step still uses the moved fixture. |
| Release trust chain | CI supplies an old trust-root path, so published release verification fails even if binaries build. | Update trust-root path and exercise `scripts/verify-release.sh` on a release candidate. |
| Release outputs/install | Renaming the Cargo package or binary could break `cargo -p`, `target/...` paths, assets, updater expectations, or LaunchAgent configuration. | Keep package/binary/artifact identifiers stable as proposed; verify release commands and plist installation instructions. |
| Docs and repo tooling | Old source links, operational instructions, code comments, or examples point to missing paths and mislead maintainers. | Search every tracked text file for old directory names; update README, docs, install, operations, provenance, and design material. |
| Review/test fixtures | Synthetic GitHub patches can include source paths whose meaning changes with the layout. | Update fixtures only when they describe paths in this repository; preserve intentional historical examples and external runtime paths. |
| Git history/review | A large rename plus package/import updates may obscure semantic changes or reduce rename detection. | Make the move mechanically, stage moves before edits where practical, and review the diff with rename detection enabled. |

Suggested post-move validation (not run for this research-only task):

```sh
cargo metadata --no-deps --format-version 1
cargo build --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all --check
bash scripts/e2e-full-stack.sh target/debug/zigzag target/debug/zzapi
rg -n 'relay-core|relay/src|relay/trust|cli/' --glob '!Cargo.lock' .
```

Also run the CI release trust-root verification path with a suitable release
candidate; do not publish a release solely to validate a source move.

## 5. Recommended implementation sequence

1. Move `relay-core/` to `zz/`, `cli/` to `zzapi/`, and `relay/` to `zzd/`.
2. Update workspace members, package/path dependency configuration, the core
   crate name, imports, and `Cargo.lock`.
3. Update CI/release/test fixture paths and docs/comments/source references.
4. Search tracked files for old crate and source-directory names, inspecting
   each match to distinguish source paths from runtime paths and historical
   material.
5. Run the workspace and end-to-end validation above; verify signing scripts,
   release trust-root verification, binary names, and LaunchAgent guidance.
6. Review the complete diff for accidental changes to runtime paths or API
   behavior, then submit the layout change for review.
