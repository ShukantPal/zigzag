# Zigzag release trust chain

Every push to `main` publishes a signed, attested release. The relay's
auto-updater verifies the full chain before installing anything — a release
binary is trusted only if it is cryptographically proven to have been built
by this repo's release workflow from a known commit.

## What a release produces

1. `zigzag-macos-aarch64` — the relay binary, Apple-signed
   (`--identifier com.shukantpal.zigzag`, `--options runtime`, `--timestamp`).
2. `zigzag-macos-aarch64.manifest.json` — `{version, target, sha256, commit}`.
3. Two SLSA v1 provenance attestations via `actions/attest-build-provenance`
   (one per artifact). Each attestation is a Sigstore DSSE envelope:
   keyless-signed by Fulcio using the workflow's GitHub OIDC identity and
   recorded in the Rekor transparency log.

## What the updater verifies (zzd/src/update.rs)

1. **Manifest attestation.** Verified with `gh attestation verify`, constrained
   to repo `ShukantPal/zigzag`, signer workflow
   `.github/workflows/ci.yml`, source ref `refs/heads/main`, predicate
   `https://slsa.dev/provenance/v1`, against the Sigstore trust root pinned in
   `zzd/trust/sigstore-trusted-root.json`. No source digest is pinned at this
   stage: the manifest is authenticated before its claimed version or commit
   is trusted.
2. **Manifest contents.** `version` must equal the release tag and `target`
   must be `aarch64-apple-darwin`.
3. **Binary digest.** SHA-256 of the downloaded binary must match the manifest.
4. **Apple codesign requirement.**
   `anchor apple generic and identifier "com.shukantpal.zigzag" and certificate leaf[subject.OU] = "7YZK8D3B48"`
   plus a `codesign -d` identity check for the same identifier/team.
   Empirically derived facts (verified against a shipped release):
   - `anchor apple generic`, not `anchor apple`: plain `anchor apple` fails
     for certificates chaining through Apple Worldwide Developer Relations
     CA G3.
   - The TeamIdentifier is `7YZK8D3B48`. The `(NH5F3PDHQ8)` parenthetical in
     the "Apple Development" certificate's CN is stale; `codesign -d` reports
     the real subject OU / TeamIdentifier.
   - The requirement must not be prefixed with `=designated =>` — that is
     `codesign -d -r-` display syntax and a syntax error as `-R` input.
5. **Binary attestation.** Verified like the manifest's, but with
   `--source-digest` pinned to the manifest's commit. This closes the loop:
   the manifest is trusted first, then the binary is proven to have been
   built from the manifest's commit by the trusted workflow.

## Verifying a release manually

```bash
bash scripts/verify-release.sh --tag v0.1.210   # defaults to latest
```

The script performs the same five checks standalone. CI runs it against every
published release (the "Verify release trust chain" step) before relays can
pick the release up.

## Files

- `.github/workflows/ci.yml` — release job: build, sign, manifest, attest,
  publish, then verify.
- `scripts/sign-ci-release.sh` — Apple signing.
- `scripts/verify-release.sh` — standalone trust-chain verification.
- `zzd/src/update.rs` — the updater's in-process verification.
- `zzd/trust/sigstore-trusted-root.json` — pinned Sigstore trust root.
