# Review: 37-subscription-llm-auth

**Date:** 2026-03-02
**Scope:** 6-PR implementation — CLI subprocess wrapper, subscription auth, credential storage, session execution modes, platform commands, CLI binary

**Changed files:**
- `src/agent/claude_code/adapter.rs`, `src/agent/claude_code/pod.rs`
- `src/agent/mod.rs`, `src/agent/provider.rs`, `src/agent/service.rs`
- `src/api/mod.rs`, `src/api/sessions.rs`
- `src/auth/mod.rs`, `src/config.rs`, `src/main.rs`
- `src/pipeline/executor.rs`, `src/registry/mod.rs`
- `src/store/bootstrap.rs`, `src/store/mod.rs`

**New files:**
- `src/agent/claude_cli/{control,error,messages,mod,session,transport}.rs`
- `src/agent/commands.rs`, `src/api/cli_auth.rs`, `src/api/commands.rs`
- `src/auth/cli_creds.rs`, `src/registry/pull_secret.rs`
- `cli/platform-cli/src/{main,client,config,stream,commands}.rs`
- `migrations/2026030202*.sql`, `migrations/2026030203*.sql`, `migrations/2026030205*.sql`
- `tests/cli_auth_integration.rs`, `tests/commands_integration.rs`

**Overall:** PASS WITH FINDINGS

## Summary
- Strong implementation across 6 PRs with good architecture separation (CLI subprocess, credential encryption, command resolution with project-scoped override, standalone CLI binary)
- 3 critical, 5 high findings; most relate to authorization gaps on commands API and a UTF-8 slicing panic
- 32 integration tests + 18 unit tests added; several handler paths need additional coverage
- Touched-line coverage: 80% on modified files, new files range 0-100%

## Critical & High Findings (must fix)

### R1: [CRITICAL] `truncate_prompt` panics on multi-byte UTF-8
- **File:** `src/api/sessions.rs:805-810`
- **Domain:** Rust Quality
- **Description:** `&prompt[..max_len]` slices on byte boundary. Will panic on multi-byte characters (emoji, CJK).
- **Risk:** Runtime panic on any non-ASCII prompt text
- **Suggested fix:** Use `.chars().take(max_len).collect::<String>()` or use `.char_indices()` to find a safe boundary (the correct pattern is already used on line 388 in the same file).

### R2: [CRITICAL] CLI binary `&id[..8]` panics on short strings
- **File:** `cli/platform-cli/src/main.rs:300`
- **Domain:** Rust Quality
- **Description:** `&id[..8]` will panic if the session ID string is shorter than 8 characters (e.g., if `unwrap_or("?")` yields `"?"`).
- **Risk:** CLI crash when listing sessions with unexpected API response
- **Suggested fix:** Use `id.get(..8).unwrap_or(id)` for safe slicing.

### R3: [CRITICAL] Commands API missing project read permission checks (IDOR)
- **File:** `src/api/commands.rs:173-293,400-419`
- **Domain:** Security
- **Description:** `list_commands`, `get_command`, and `resolve_command_handler` allow any authenticated user to access project-scoped commands without checking project read permission. `resolve_command` leaks the full prompt_template content.
- **Risk:** Any authenticated user can enumerate and read prompt templates for private projects they don't have access to.
- **Suggested fix:** Add `require_project_read(&state, &auth, pid).await?` when `project_id` is present in list/get/resolve handlers. Return 404 for private resources.

### R4: [HIGH] `update_session` missing project write permission check
- **File:** `src/api/sessions.rs:618-657`
- **Domain:** Security
- **Description:** Any session owner can link their session to ANY active project by UUID without verifying they have `ProjectWrite` on that project. Also missing audit logging.
- **Risk:** Session-to-project association bypass; un-audited mutation
- **Suggested fix:** Add `has_permission(... Permission::ProjectWrite)` check after project existence verification. Add audit log entry with action `"agent_session.update"`.

### R5: [HIGH] CLI token in WebSocket URL query parameter
- **File:** `cli/platform-cli/src/main.rs:211`
- **Domain:** Security
- **Description:** API token passed via `?token=` query string in WebSocket URL. Leaks to proxy logs, browser history, and server access logs.
- **Risk:** Token exposure in logs and network intermediaries
- **Suggested fix:** Pass token via first WebSocket message after connection, or use a short-lived session token obtained via an authenticated HTTP request.

### R6: [HIGH] `create_session` exceeds 100-line and 7-param limits
- **File:** `src/agent/service.rs:38`
- **Domain:** Rust Quality
- **Description:** Function is ~170 lines with 8 parameters. Uses `#[allow(clippy::too_many_arguments)]` and `#[allow(clippy::too_many_lines)]`.
- **Risk:** Maintainability; violates project conventions
- **Suggested fix:** Create `CreateSessionParams` struct for parameters. Extract helpers: `resolve_auth_credentials()`, `build_and_apply_pod()`.

### R7: [HIGH] `registry/pull_secret.rs` has 0% test coverage
- **File:** `src/registry/pull_secret.rs`
- **Domain:** Tests
- **Description:** 0/32 lines covered. No unit or integration tests for pull secret creation. Creates K8s secrets and API tokens.
- **Risk:** Regressions in pull secret logic undetected
- **Suggested fix:** Add unit tests for Docker config JSON structure generation and secret naming logic. K8s API calls are covered by E2E only (acceptable exception).

### R8: [HIGH] Commands API missing handler tests (not-found, validation)
- **File:** `tests/commands_integration.rs`
- **Domain:** Tests
- **Description:** Missing tests: get/update/delete nonexistent command (404), empty/oversized template rejected (400), resolve without `/` prefix (400), list with project includes global commands, project-scoped permission checks.
- **Risk:** Handler edge cases untested
- **Suggested fix:** Add integration tests:
  - `get_nonexistent_command_404`
  - `update_nonexistent_command_404`
  - `delete_nonexistent_command_404`
  - `create_command_empty_template_rejected`
  - `create_command_oversized_template_rejected`
  - `resolve_command_missing_slash_returns_400`
  - `list_commands_with_project_includes_global`

## Medium Findings (should fix)

### R9: [MEDIUM] No length validation on `description` field in commands
- **File:** `src/api/commands.rs:127`
- **Description:** `CreateCommandRequest.description` and `UpdateCommandRequest.description` have no length check. Guidelines require 0-10,000 chars.
- **Suggested fix:** Add `validation::check_length("description", &desc, 0, 10_000)?;` in create and update handlers.

### R10: [MEDIUM] No length validation on `ResolveCommandRequest.input`
- **File:** `src/api/commands.rs:421`
- **Description:** The `input` string has no size bound. A multi-megabyte string could be parsed and substituted.
- **Suggested fix:** Add `validation::check_length("input", &body.input, 1, 100_000)?;`

### R11: [MEDIUM] `require_command_write` returns 403 instead of 404 for private resources
- **File:** `src/api/commands.rs:64-97`
- **Description:** Unauthorized access to project-scoped commands returns `Forbidden` (403), leaking resource existence.
- **Suggested fix:** Change to `ApiError::NotFound("command".into())` per security patterns.

### R12: [MEDIUM] `list_commands` with project_id doesn't check `is_active` on project
- **File:** `src/api/commands.rs:185-208`
- **Description:** Commands for soft-deleted projects are still returned in list queries.
- **Suggested fix:** Add project existence check (`AND is_active = true`) at the top of the project-scoped branch.

### R13: [MEDIUM] CLI config `Debug` derive leaks API token
- **File:** `cli/platform-cli/src/config.rs:18`
- **Description:** `ServerConfig` derives `Debug`, which includes the plaintext API token.
- **Suggested fix:** Implement custom `Debug` that redacts the token field.

### R14: [MEDIUM] `resolve_cli_oauth_token` silently swallows decryption errors
- **File:** `src/agent/service.rs:648-658`
- **Description:** Decryption failure logged at WARN but returns `None`, silently falling back to no auth.
- **Suggested fix:** Distinguish "no credentials" (debug-level, expected) from "decryption failed" (error-level, unexpected). Consider propagating the error.

### R15: [MEDIUM] `transport.rs` `wait()` silently swallows JoinHandle panic
- **File:** `src/agent/claude_cli/transport.rs:216`
- **Description:** `task.await.unwrap_or_default()` swallows panics from the stderr capture task.
- **Suggested fix:** Use `unwrap_or_else(|e| { tracing::warn!(...); String::new() })`.

### R16: [MEDIUM] `update_session` missing audit logging
- **File:** `src/api/sessions.rs:618-657`
- **Description:** Session update mutation (linking to project) is not audit-logged.
- **Suggested fix:** Add `write_audit()` call with action `"agent_session.update"`.

## Low Findings (optional)

- [LOW] R17: `src/api/cli_auth.rs:82` — Token value has no upper-bound length check → add `validation::check_length("token", &body.token, 1, 10_000)?;`
- [LOW] R18: `src/agent/claude_cli/transport.rs:295` — `build_args`/`build_env` are `pub` but only used internally → make `pub(crate)`
- [LOW] R19: `src/agent/claude_cli/transport.rs:106-112` — Nested if blocks could use collapsible form
- [LOW] R20: `src/agent/claude_cli/transport.rs:380-386` — `build_env` doesn't filter reserved env vars from `extra_env` (unlike pod.rs `RESERVED_ENV_VARS`)
- [LOW] R21: `src/agent/claude_cli/session.rs:73` — Hardcoded broadcast channel capacity 256 → make configurable or document sizing
- [LOW] R22: `src/registry/pull_secret.rs:31` — 7 parameters (at threshold) → consider params struct
- [LOW] R23: `src/registry/pull_secret.rs:55` — `fetch_one` without error context for missing user → use `fetch_optional`
- [LOW] R24: `src/registry/pull_secret.rs:71` — `label_value[..8]` byte slice could split multibyte → use `.get(..8).unwrap_or(label_value)`
- [LOW] R25: `src/auth/cli_creds.rs:27` — `#[allow(dead_code)]` on `DecryptedCredential` — verify if actually needed
- [LOW] R26: `cli/platform-cli/src/main.rs:268` — `_project` parameter unused in `list_sessions`
- [LOW] R27: `src/agent/claude_cli/session.rs` — Missing `#[tracing::instrument]` on session manager methods

## Coverage — Touched Lines

### Modified files (from diff-cover):

| File | Lines changed | Lines covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/agent/claude_code/adapter.rs` | 4 | 4 | 100% | — |
| `src/agent/claude_code/pod.rs` | ~150 | ~150 | 100% | — |
| `src/agent/service.rs` | 80 | 14 | 17.5% | 185, 653-655, 697-736 |
| `src/api/mod.rs` | 4 | 4 | 100% | — |
| `src/api/sessions.rs` | 120 | 16 | 13.0% | 880-945, 982-1001 |
| `src/config.rs` | 4 | 4 | 100% | — |
| `src/pipeline/executor.rs` | 20 | 20 | 100% | — |
| `src/store/bootstrap.rs` | 31 | 30 | 96.8% | 457 |

**Total modified: 421 lines, 340 covered, 81 missing → 80%**

### New files:

| File | Total lines | Lines covered | Coverage % |
|---|---|---|---|
| `src/agent/commands.rs` | 135 | 134 | 99.3% |
| `src/api/cli_auth.rs` | 39 | 34 | 87.2% |
| `src/api/commands.rs` | 111 | 99 | 89.2% |
| `src/auth/cli_creds.rs` | 92 | 91 | 98.9% |
| `src/registry/pull_secret.rs` | 32 | 0 | 0% |
| `src/agent/claude_cli/control.rs` | 57 | 57 | 100% |
| `src/agent/claude_cli/error.rs` | 29 | 29 | 100% |
| `src/agent/claude_cli/messages.rs` | 145 | 139 | 95.9% |
| `src/agent/claude_cli/session.rs` | 324 | 292 | 90.1% |
| `src/agent/claude_cli/transport.rs` | 526 | 414 | 78.7% |

### Uncovered Paths

- `src/agent/service.rs:653-736` — CLI subprocess session lifecycle (create, send_message, stop). Requires real CLI subprocess; covered only via E2E path when CLI binary present. **Exception: infra-dependent code path.**
- `src/api/sessions.rs:880-945` — WebSocket streaming for CLI sessions (`stream_broadcast_to_ws`, `stream_pod_logs_to_ws`). Requires WebSocket test client. **Exception: WebSocket handlers typically tested E2E only.**
- `src/api/sessions.rs:982-1001` — `send_message` handler dispatching to CLI sessions. Same exception as above.
- `src/registry/pull_secret.rs` — 0% coverage. Creates K8s secrets. **Not an acceptable exception — at least the Docker config JSON generation should have unit tests (R7).**
- `src/store/bootstrap.rs:457` — platform-runner project creation for registry. **Exception: bootstrap-only code path.**
- `src/agent/claude_cli/transport.rs` — 78.7%. Missing coverage on actual subprocess spawn/kill/wait and OS-level operations. **Exception: requires real Claude CLI binary.**

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | thiserror used, proper ApiError mapping, no unwrap in production |
| Auth & permissions | FAIL | Commands API missing project read checks (R3), update_session missing project write check (R4) |
| Input validation | FAIL | Missing description (R9) and input (R10) length checks, token length (R17) |
| Audit logging | FAIL | update_session not audited (R16) |
| Tracing instrumentation | PASS | Good instrumentation on handlers and service functions |
| Clippy compliance | PASS | All clippy warnings suppressed with documented allows |
| Test patterns | PASS | Follows project patterns correctly, good test helpers |
| Migration safety | PASS | Clean UP/DOWN pairs, correct versioning, proper indexes |
| Touched-line coverage | FAIL | 80% on modified files; pull_secret.rs at 0% |
