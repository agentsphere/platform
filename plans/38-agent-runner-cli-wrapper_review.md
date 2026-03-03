# Review: 38-agent-runner-cli-wrapper

**Date:** 2026-03-03
**Scope:** 8 new Rust source files + Cargo.toml in `cli/agent-runner/` (~3,147 lines), 7 deleted files in `cli/platform-cli/`, 1 modified plan reference
**Overall:** PASS WITH FINDINGS

## Summary
- Solid standalone CLI binary with strong security posture (env_clear whitelist, reserved vars, input size limits, config isolation)
- All message types, control payloads, and conversion logic are wire-compatible with the platform (verified exhaustively by Agent 4)
- 2 high findings (cancel-safety, test flakiness), 9 medium findings (security blocklist gap, silent failures, missing test paths)
- 112 tests (110 passing + 2 ignored Valkey integration); good coverage overall with specific gaps identified
- Coverage tooling: standalone crate — `just cov-diff` not applicable; coverage assessed via manual code path analysis

## Critical & High Findings (must fix)

### R1: [HIGH] Cancel-safety in inner response loop's tokio::select!
- **File:** `cli/agent-runner/src/repl.rs:131-138`
- **Domain:** Rust Quality
- **Description:** The inner response streaming loop uses `tokio::select!` between `transport.recv()` and `tokio::signal::ctrl_c()`. The `recv()` method acquires a `Mutex<BufReader>` lock and calls `read_line`. If `ctrl_c` fires mid-read, the `recv()` future is dropped. The `BufReader`'s internal buffer retains already-read bytes, but the partially-assembled `line` String is dropped — potentially corrupting the next `read_line` call.
- **Risk:** After Ctrl+C during response streaming, the next message read could get a partial/corrupted NDJSON line, causing a parse error or wrong message.
- **Suggested fix:** Move `transport.recv()` into a dedicated spawned task with an mpsc channel (same pattern as stdin at line 56-65), so the actual `read_line` is never cancelled mid-operation. The select! then waits on the mpsc receiver (cancel-safe) instead of the raw read.

### R2: [HIGH] Env var tests modify process globals without synchronization
- **File:** `cli/agent-runner/src/main.rs:251-352`
- **Domain:** Tests
- **Description:** 7 tests (`no_auth_fails`, `oauth_takes_precedence`, `valkey_without_session_id_fails`, `session_id_without_valkey_ok`, `valkey_with_session_id`, and implicitly any test in the same binary) modify process-global env vars via `set_var`/`remove_var`. Cargo test runs tests in parallel within the same process, creating race conditions.
- **Risk:** Flaky test failures in CI — one test clears `ANTHROPIC_API_KEY` while another reads it.
- **Suggested fix:** Add `serial_test` as a dev-dependency. Mark all env-mutating tests with `#[serial]`. Alternatively, refactor `resolve_auth`/`resolve_pubsub` to accept values as parameters for testability.

## Medium Findings (should fix)

### R3: [MEDIUM] RESERVED_ENV_VARS missing proxy and Node.js security vars
- **File:** `cli/agent-runner/src/main.rs:22-33`
- **Domain:** Security
- **Description:** The `--extra-env` blocklist protects platform credentials but doesn't block proxy/Node.js/TLS vars. Claude CLI is a Node.js process — an attacker with `--extra-env` access could inject `HTTP_PROXY` (redirect API traffic through attacker proxy, capturing `ANTHROPIC_API_KEY`), `NODE_OPTIONS` (inject `--require` to load arbitrary code), or `NODE_EXTRA_CA_CERTS` (add attacker CA for MITM).
- **Risk:** Credential exfiltration via proxy injection or arbitrary code execution via NODE_OPTIONS in K8s co-tenancy scenario.
- **Suggested fix:** Add to RESERVED_ENV_VARS: `"HTTP_PROXY"`, `"HTTPS_PROXY"`, `"ALL_PROXY"`, `"NO_PROXY"`, `"http_proxy"`, `"https_proxy"`, `"all_proxy"`, `"no_proxy"`, `"NODE_OPTIONS"`, `"NODE_EXTRA_CA_CERTS"`, `"SSL_CERT_FILE"`, `"SSL_CERT_DIR"`.

### R4: [MEDIUM] Pub/sub init event failure silently swallowed
- **File:** `cli/agent-runner/src/repl.rs:46-47`
- **Domain:** Rust Quality
- **Description:** `ps.publish_event(&event).await.ok()` discards the error. If Valkey auth fails on first publish, the agent runs but the platform has no visibility — session appears stuck in "started but never heard from".
- **Suggested fix:** At minimum log the error: `.unwrap_or_else(|e| eprintln!("[warn] failed to publish init event: {e}"))`. Consider making the initial publish a hard error since it indicates the pub/sub channel is broken before the session starts.

### R5: [MEDIUM] Subscriber task silently dies on disconnect
- **File:** `cli/agent-runner/src/pubsub.rs:244-276`
- **Domain:** Rust Quality
- **Description:** The spawned subscriber task exits silently when `message_rx.recv()` returns `Err` (Valkey disconnect). No more input will ever arrive — the REPL appears to hang.
- **Suggested fix:** Log a warning on task exit: `eprintln!("[warn] pub/sub subscriber disconnected")`. Consider sending a sentinel value through the tx channel to signal disconnection.

### R6: [MEDIUM] convert_assistant returns first thinking block, ignores subsequent text/tool_use
- **File:** `cli/agent-runner/src/pubsub.rs:75-94`
- **Domain:** Rust Quality / Compatibility
- **Description:** If an assistant message contains both thinking AND text/tool_use blocks, `convert_assistant` returns early on the first thinking block (line 89 `return Some(...)`) and never processes subsequent blocks. A message with `[thinking, text, tool_use]` only emits the thinking event. Note: this matches the platform's `cli_message_to_progress()` exactly (same early-return logic), so it's a faithful port of an existing limitation.
- **Suggested fix:** Document the trade-off as a comment. If one-event-per-message is intentional, state it explicitly. The platform and agent-runner are consistent, so no compatibility issue.

### R7: [MEDIUM] Missing tests for wait_for_init remaining paths
- **File:** `cli/agent-runner/src/repl.rs:19-26`
- **Domain:** Tests
- **Description:** `wait_for_init` has 5 match arms but only 2 are tested (success and timeout). Untested: "CLI exited before init" (line 23), "wrong message type before init" (line 20-22), "read error" (line 24).
- **Suggested fix:** Add 2 tests using the existing cat transport pattern: `init_eof_before_system_message` (spawn process that immediately exits) and `init_wrong_message_type` (write an assistant message first).

### R8: [MEDIUM] Missing tests for build_args with system_prompt and allowed_tools
- **File:** `cli/agent-runner/src/transport.rs:315-328`
- **Domain:** Tests
- **Description:** `build_args` has distinct code paths for `system_prompt`, `append_system_prompt`, and `allowed_tools` that have zero test coverage. These are real CLI flags that would break silently.
- **Suggested fix:** Add `build_args_with_system_prompt`, `build_args_with_append_system_prompt`, `build_args_with_allowed_tools` tests.

### R9: [MEDIUM] Missing tests for empty-string env vars in resolve_auth/resolve_pubsub
- **File:** `cli/agent-runner/src/main.rs:88-94,143-147`
- **Domain:** Tests
- **Description:** The `!token.is_empty()` and `!url.is_empty()` guards are explicit branches that lack tests. K8s commonly sets env vars to `""` when values are omitted — this is a real-world edge case.
- **Suggested fix:** Add tests: `auth_empty_oauth_falls_to_api_key`, `auth_empty_both_fails`, `pubsub_empty_valkey_url_returns_none`, `pubsub_empty_session_id_fails`.

### R10: [MEDIUM] render.rs tests only assert no-panic
- **File:** `cli/agent-runner/src/render.rs:138-355`
- **Domain:** Tests
- **Description:** All 15 render tests call `render_message(&msg)` and check only that it doesn't panic. The rendering logic could emit completely wrong output and all tests would pass.
- **Suggested fix:** Accept as-is for v1 — fixing requires refactoring render functions to accept `&mut impl Write`. Add a comment documenting this as a known limitation.

### R11: [MEDIUM] Blanket #[allow(dead_code)] on entire modules
- **File:** `cli/agent-runner/src/main.rs:1-3,9`
- **Domain:** Rust Quality
- **Description:** `#[allow(dead_code)]` on `control`, `error`, and `transport` modules suppresses warnings for ALL unused code, not just the specific forked-but-unused items. New dead code won't trigger warnings.
- **Suggested fix:** Move `#[allow(dead_code)]` to specific unused items within each module rather than blanket-suppressing. Alternatively, accept and document — these are forked files that are intentionally kept in sync with the platform originals.

## Low Findings (optional)

- **R12:** [LOW] `cli/agent-runner/src/transport.rs:108-114` — stderr capture truncation has off-by-one (trailing newline without content at exactly 4095 bytes). Cosmetic.
- **R13:** [LOW] `cli/agent-runner/src/repl.rs:33` — No `Drop` impl for `SubprocessTransport`; child process could be orphaned on panic. Mitigated by K8s pod termination. Consider `impl Drop` calling `child.start_kill()`.
- **R14:** [LOW] `cli/agent-runner/src/main.rs:82` — `AuthToken` enum has no `Debug` derive (intentional for security). Add a comment: `// No Debug derive — contains secrets`.
- **R15:** [LOW] `cli/agent-runner/src/main.rs:132` — `PubSubConfig` derives `Debug`; `url` field may contain embedded Valkey password. Consider custom Debug that redacts URL.
- **R16:** [LOW] `cli/agent-runner/src/render.rs:107-117` — `notify_desktop()` has potential AppleScript injection surface. Currently only hardcoded strings are passed. Document that parameters must be hardcoded.
- **R17:** [LOW] `cli/agent-runner/src/transport.rs:174` — `recv()` logs invalid NDJSON line content to stderr. Could theoretically contain sensitive data. Consider truncating: `&line[..line.len().min(200)]`.
- **R18:** [LOW] `cli/agent-runner/Cargo.lock` — not git-tracked. Binary projects should commit Cargo.lock for reproducible builds.
- **R19:** [LOW] Missing tests for `parse_extra_env` edge cases: empty array, value containing `=`, empty key, empty value, all 10 reserved vars (only 2 tested).
- **R20:** [LOW] Missing tests for `truncate_str` edge cases: empty string, max_len=0, multi-byte Unicode characters.

## Coverage — Touched Lines

Coverage analysis was performed via manual code path review (standalone crate — `just cov-diff` not applicable).

| File | Production lines | Branches tested | Untested branches | Notes |
|---|---|---|---|---|
| error.rs | ~40 | 7/11 display variants | 4 Source chain variants | Low risk (thiserror-derived) |
| messages.rs | ~170 | All deserialize paths | parse_cli_message for result/user types | Copied from platform, well-tested |
| control.rs | ~75 | All 5 paths | Optional field absence | Copied, well-tested |
| transport.rs | ~300 | 23 tests, core paths | system_prompt, allowed_tools, append_system_prompt args | 3 untested build_args branches |
| pubsub.rs | ~280 | All 7 kinds, all conversions | Multi-block priority, fallback defaults | Good coverage overall |
| render.rs | ~130 | All 4 message types | Unknown block types, missing optional fields | No output assertions |
| repl.rs | ~55 | 2/5 wait_for_init arms, dispatch | EOF, wrong-type, read-error arms of wait_for_init | 3 untested wait_for_init arms |
| main.rs | ~90 | Auth, pubsub, parsing | Empty-string env vars, all reserved vars | Env var race risk |

### Uncovered Paths (key gaps)
- `repl.rs:20-24` — 3 match arms in `wait_for_init`: wrong message type, EOF, read error
- `transport.rs:315-328` — `build_args` branches for `system_prompt`, `append_system_prompt`, `allowed_tools`
- `main.rs:88-94` — Empty-string credential fallthrough logic
- `pubsub.rs:244-276` — Subscriber task disconnect/exit path (requires real Valkey)

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | No `.unwrap()` in production code, thiserror used correctly |
| Credential security | PASS | env_clear + whitelist, no CLI arg secrets, reserved var blocklist |
| Input validation | PASS | 1MB pub/sub limit, reserved env var blocklist |
| Subprocess isolation | PASS | env_clear, config isolation via tempfile |
| Tracing/observability | PASS | eprintln to stderr (appropriate for standalone CLI) |
| Clippy compliance | PASS | `cargo clippy -- -D warnings` clean |
| Test patterns | PASS WITH GAPS | 112 tests, good coverage; 9 medium gaps identified |
| Wire compatibility | PASS | Exhaustive comparison confirms exact match with platform |
| Cancel safety | NEEDS FIX | R1: inner select! loop has cancel-safety issue |
| Test isolation | NEEDS FIX | R2: env var tests race in parallel execution |
| Security blocklist | NEEDS FIX | R3: proxy/Node.js vars missing from RESERVED_ENV_VARS |
