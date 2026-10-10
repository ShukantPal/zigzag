# Review system

The `zigzag` daemon owns the Rust review loop and review gate. Configuration is
read from `~/.zigzag/config.yaml`; it defines the repositories, review lenses,
CI requirements, and human actors used by the gate. Invalid configuration
disables review processing while leaving the HTTP relay available.

The authenticated `GET /v1/review-gate` route reports whether the configured
review requirements are satisfied for a pull request. Use
`zzapi review-gate --repo OWNER/REPO --pr NUMBER` to query that result. The
daemon's review loop tracks pull request review work and can run in shadow mode
with `ZIGZAG_REVIEW_LOOP_SHADOW=1`, which observes decisions without reviewer
dispatch or owner-resume side effects.

See [configuration and security](configuration.md) for policy location and
[relay API details](relay.md) for the route contract.
