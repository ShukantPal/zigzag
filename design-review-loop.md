# Mac-Owned Codex Review Loop

**Status:** Proposed

**Decision:** Move the Zigzag repository's review lifecycle from VM cron jobs into the Zigzag Mac daemon. Keep notification and cross-repository orchestration on the VM. Store review-loop policy as personal machine configuration in `~/.zigzag/config.yaml`, outside every repository.

## Why change

Review decisions and review work should have one owner. Today, VM-side Python programs dispatch reviewers, poll results, and enforce the merge gate while the governed Codex processes run on the Mac. The split adds a network boundary and makes duplicate dispatch, stale state, and cleanup harder to reason about.

The daemon already has durable events, an agent registry, process supervision, bounded logs, and process-group termination. It should own the review state machine as well. Its policy is specific to the person and machine running that daemon: repository access, polling cadence, trusted GitHub identity, and result limits are operational preferences, not source-controlled project behavior. Keeping them out of Zigzag also avoids coupling daemon startup to a repository checkout.

## Target architecture

For each configured repository, the daemon owns one durable state machine keyed by repository, pull request, and head commit. It:

1. dispatches independent correctness, simplicity, and tests reviewers, plus security where required;
2. records every reviewer against the exact head and polls durable agent state;
3. accepts only structured, head-matching verdicts within configured limits;
4. evaluates required CI and all required lens approvals on the current head;
5. resumes the owning session when findings require changes, then reviews the new head; and
6. terminates remaining reviewer process groups when the pull request merges.

State is persisted before side effects, and stable idempotency keys make restarts converge. A new head supersedes the old round and never inherits approvals. Reviewer output remains untrusted: the daemon admits only the versioned result format and sends bounded findings, not raw transcripts, to the write-capable owner.

The implementation is Rust inside the daemon; Python is absent from the Mac runtime path. Review state is Mac-local and single-writer. GitHub remains the external record for heads, checks, verdicts, and merge status.

## Configuration contract

The daemon reads `~/.zigzag/config.yaml` once at startup. The file is plain, hand-written YAML: there is no Python authoring layer, generated JSON, committed artifact, or CI freshness check. It contains policy but no credentials. Editing it takes effect on the next daemon restart; version 1 does not hot-reload.

The daemon parses YAML with duplicate keys and custom tags rejected, converts the result to the JSON data model, and validates it against the JSON Schema in Appendix A. Unknown fields are rejected. The validator must enforce the schema's `regex` format. Cross-field requirements, including the security lens rule, are part of the schema rather than application defaults.

All version 1 fields are required; there are no optional fields. `enabled: false` is the explicit way to keep a complete policy installed without starting the loop. This favors an auditable file over hidden defaults. Appendix B is the complete file for the current Zigzag setup.

If the file is missing, unreadable, malformed, or schema-invalid, the daemon does not start the review loop. It logs each violation with its JSON path and reason—for example, `review_loop.repositories[0].lenses: must contain "security"`—and continues its unrelated duties. Invalid review configuration is therefore fail-closed for reviews but not fatal to the daemon.

## Migration without lost reviews

Cutover is a transfer of authority, not two active pollers. First, ship the daemon state machine disabled and compare its shadow decisions with the VM tools. Hand-write Appendix B at `~/.zigzag/config.yaml`; no configuration export or materialization step is needed.

At cutover, stop new VM dispatches and let active VM-owned rounds drain. Then stop the VM review jobs, enable the YAML policy, and restart the daemon. The daemon rescans open pull requests and GitHub's current heads, checks, and verdicts before dispatching. Stable keys prevent the rescan from duplicating work. Retain the disabled VM jobs through one complete review cycle, then remove them.

## What remains on the VM

Chat surfacing remains on the VM: it consumes review-ready and decision-needed events but owns no review transitions. Cross-repository work, including Leveled orchestration, also remains there. Neither concern appears in the Mac review-loop configuration.

## Risks and decisions

- **Split brain:** the single-writer cutover is mandatory; file ownership is not a runtime lock.
- **Mac sleep or network loss:** reviews pause and recover from durable state plus a GitHub rescan. The tradeoff is latency, not dropped work.
- **Privilege and prompt injection:** use repository-scoped GitHub access, validate structured results, and never forward raw reviewer transcripts to a write-capable owner.
- **Bad configuration:** the review loop stays off, the daemon stays available, and path-specific errors make repair local and explicit.
- **Machine loss:** the configuration is intentionally personal and is not recovered from the repository. Recovery means recreating the small YAML file from the documented schema and current policy.

## Appendix A: version 1 schema

The following JSON Schema is authoritative for the YAML document. The field names and types are visible in `properties`; every object lists all fields in `required` and sets `additionalProperties` to `false`.

| YAML path | Type | Requirement |
|---|---|---|
| `schema_version` | integer, exactly `1` | Required |
| `review_loop.enabled` | boolean | Required |
| `review_loop.intervals.{discovery,review,merge}_seconds` | integer, 30–3600 | All required |
| `review_loop.repositories[]` | repository object | At least one required |
| `.repository` | `owner/name` string | Required; unique across entries |
| `.full_rounds_max`, `.verification_rounds_max` | non-negative integer | Both required |
| `.lenses` | unique, non-empty enum array | Required; must contain `security` when requested |
| `.require_security_lens` | boolean | Required |
| `.required_ci_checks[]` | `{label: string, name_pattern: regex}` | Required and non-empty |
| `.trusted_verdict_identity` | non-empty string | Required |
| `.result_limits` | positive integer byte and finding limits | Both fields required |

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://zigzag.local/schemas/config-v1.json",
  "title": "Zigzag personal daemon configuration",
  "type": "object",
  "additionalProperties": false,
  "required": ["schema_version", "review_loop"],
  "properties": {
    "schema_version": {"type": "integer", "const": 1},
    "review_loop": {
      "type": "object",
      "additionalProperties": false,
      "required": ["enabled", "intervals", "repositories"],
      "properties": {
        "enabled": {"type": "boolean"},
        "intervals": {
          "type": "object",
          "additionalProperties": false,
          "required": ["discovery_seconds", "review_seconds", "merge_seconds"],
          "properties": {
            "discovery_seconds": {"type": "integer", "minimum": 30, "maximum": 3600},
            "review_seconds": {"type": "integer", "minimum": 30, "maximum": 3600},
            "merge_seconds": {"type": "integer", "minimum": 30, "maximum": 3600}
          }
        },
        "repositories": {
          "type": "array",
          "minItems": 1,
          "items": {"$ref": "#/$defs/repository"}
        }
      }
    }
  },
  "$defs": {
    "repository": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "repository",
        "full_rounds_max",
        "verification_rounds_max",
        "lenses",
        "require_security_lens",
        "required_ci_checks",
        "trusted_verdict_identity",
        "result_limits"
      ],
      "properties": {
        "repository": {
          "type": "string",
          "pattern": "^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$"
        },
        "full_rounds_max": {"type": "integer", "minimum": 0},
        "verification_rounds_max": {"type": "integer", "minimum": 0},
        "lenses": {
          "type": "array",
          "minItems": 1,
          "uniqueItems": true,
          "items": {
            "enum": ["correctness", "simplicity", "tests", "security", "performance"]
          }
        },
        "require_security_lens": {"type": "boolean"},
        "required_ci_checks": {
          "type": "array",
          "minItems": 1,
          "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["label", "name_pattern"],
            "properties": {
              "label": {"type": "string", "minLength": 1},
              "name_pattern": {"type": "string", "minLength": 1, "format": "regex"}
            }
          }
        },
        "trusted_verdict_identity": {"type": "string", "minLength": 1},
        "result_limits": {
          "type": "object",
          "additionalProperties": false,
          "required": ["max_findings_per_lens", "max_bytes_per_lens"],
          "properties": {
            "max_findings_per_lens": {"type": "integer", "minimum": 1},
            "max_bytes_per_lens": {"type": "integer", "minimum": 1}
          }
        }
      },
      "allOf": [
        {
          "if": {
            "properties": {"require_security_lens": {"const": true}},
            "required": ["require_security_lens"]
          },
          "then": {
            "properties": {"lenses": {"contains": {"const": "security"}}}
          }
        }
      ]
    }
  }
}
```

Semantics not expressible portably in JSON Schema remain deterministic: repository names must be unique; CI patterns are matched case-insensitively against GitHub check names; every matching check must complete successfully; and no match fails the gate. Oversized reviewer results fail their lens rather than being truncated into an apparent approval.

## Appendix B: current `~/.zigzag/config.yaml`

```yaml
schema_version: 1

review_loop:
  enabled: true
  intervals:
    discovery_seconds: 300
    review_seconds: 600
    merge_seconds: 300
  repositories:
    - repository: ShukantPal/zigzag
      full_rounds_max: 2
      verification_rounds_max: 2
      lenses:
        - correctness
        - simplicity
        - tests
        - security
      require_security_lens: true
      required_ci_checks:
        - label: semgrep
          name_pattern: semgrep
        - label: BuildBuddy
          name_pattern: buildbuddy
      trusted_verdict_identity: ShukantPal
      result_limits:
        max_findings_per_lens: 20
        max_bytes_per_lens: 16384
```

The round caps, lenses, cadence, and CI names preserve the current Zigzag policy. The trusted identity and result limits close gaps that must be explicit before activation.

## Code basis

Verified against the current department configuration, review-round watcher, approval gate, and Rust daemon. The daemon currently discovers pull requests from command-line repository and interval arguments; implementing this design replaces those review-loop arguments with the machine-local YAML contract above.
