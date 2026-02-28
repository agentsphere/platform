# Review: 34-ai-devops-experience Phase 6

**Date:** 2026-02-28
**Scope:** `src/store/eventbus.rs`, `src/observe/alert.rs`, `tests/eventbus_integration.rs`
**Overall:** PASS WITH FINDINGS

## Summary
- Phase 6 adds AlertFired event type, alert→ops-agent handler with 4-layer rate limiting, and event publishing from the alert evaluator. Implementation is clean, well-documented, and all guard clauses are tested.
- 2 high, 5 medium findings — primarily race conditions in the cooldown/limit checks and untested code paths
- 9 tests added (4 unit, 5 integration), 3 gaps identified (untested paths for no-admin, spawn-failure, warning severity)
- Touched-line coverage: ~85% estimated (the handler's core early-exit paths covered; spawn success/failure paths partially covered)

## Critical & High Findings (must fix)

### R1: [HIGH] `handle_alert_fired` exceeds 100-line clippy threshold
- **File:** `src/store/eventbus.rs:565-676`
- **Domain:** Rust Quality
- **Description:** The function body is ~104 lines, exceeding the `too_many_lines` clippy threshold of 100. Clippy currently passes because the function is just at the boundary, but any additions will fail.
- **Risk:** Future modifications push it over 100 and break CI.
- **Suggested fix:** Extract steps 5-7 (admin lookup + cooldown SET + agent spawn, lines 612-673) into a helper function like `spawn_ops_agent_for_alert()`. This drops the handler to ~50 lines and the helper to ~60.

### R2: [HIGH] NULL `agent_user_id` makes INNER JOIN skip pending sessions in concurrent limit query
- **File:** `src/store/eventbus.rs:595-601`
- **Domain:** Database
- **Description:** The 4-way INNER JOIN (`agent_sessions → users → user_roles → roles`) requires `agent_user_id` to be non-NULL. But `agent_user_id` is NULL during the brief window after `agent_sessions` INSERT (status='pending') and before `create_agent_identity()` completes. Sessions in this transient state are excluded from the COUNT, allowing the concurrent limit to be exceeded.
- **Risk:** Under burst conditions (multiple alerts fire simultaneously for one project), more than 3 ops agents could be spawned.
- **Suggested fix:** Simplest: add `s.agent_user_id IS NULL` as an OR condition to count pending sessions that haven't been provisioned yet. Cleanest long-term: add an `agent_role TEXT` column to `agent_sessions` populated at INSERT time, eliminating the 4-way JOIN entirely.

## Medium Findings (should fix)

### R3: [MEDIUM] Cooldown check-then-set TOCTOU race condition
- **File:** `src/store/eventbus.rs:587-634`
- **Domain:** Security
- **Description:** The cooldown uses a non-atomic `EXISTS` check (line 588) followed by a separate `SET` (line 627). Between these, a second concurrent event for the same alert can pass the check, bypassing the cooldown.
- **Risk:** Duplicate ops agents spawned for the same alert during concurrent event processing.
- **Suggested fix:** Replace `EXISTS` + `SET` with a single atomic `SET NX` (set-if-not-exists):
  ```rust
  let was_set: Option<String> = state.valkey.next()
      .set(&cooldown_key, "1", Some(Expiration::EX(900)), Some(SetOptions::NX), false)
      .await?;
  if was_set.is_none() { return Ok(()); } // already existed
  ```

### R4: [MEDIUM] Concurrent limit TOCTOU between COUNT and spawn
- **File:** `src/store/eventbus.rs:594-658`
- **Domain:** Security
- **Description:** The COUNT query and `create_session` call are not atomic. Two concurrent handlers for different alerts on the same project can both read `active_ops=2`, both pass the `< 3` check, and both spawn.
- **Risk:** Per-project concurrent limit (3) can be exceeded. Partially mitigated by R3 fix (same-alert duplicates) but not for different-alert-same-project races.
- **Suggested fix:** Add a per-project spawn lock via Valkey `SET NX` on key `ops-spawn-lock:{project_id}` with short TTL (~30s), acquired before COUNT and released after spawn.

### R5: [MEDIUM] `fire_alert` and `resolve_alert` errors silently discarded
- **File:** `src/observe/alert.rs:803,818`
- **Domain:** Rust Quality
- **Description:** Both use `let _ = ...` to discard errors. If `fire_alert()` INSERT fails, the `AlertFired` event is still published, creating an inconsistency between the alert_events audit trail and the event bus.
- **Risk:** Alert firings not recorded in DB but ops agents still spawned — no audit trail.
- **Suggested fix:** Log errors instead of discarding:
  ```rust
  if let Err(e) = fire_alert(&app_state.pool, rule_info.id, value).await {
      tracing::error!(error = %e, rule_id = %rule_info.id, "failed to persist alert firing");
  }
  ```

### R6: [MEDIUM] Missing tests for paths 5 (no admin), 7 (spawn failure), and "warning" severity
- **File:** `tests/eventbus_integration.rs`
- **Domain:** Tests
- **Description:** Three code paths lack dedicated tests:
  1. Path 5 (no admin user, line 618-621): Never tested because `test_state()` always creates admin.
  2. Path 7 (spawn failure clears cooldown, line 668-672): `alert_fired_sets_cooldown_on_attempt` doesn't assert on cooldown state.
  3. "warning" severity: All passing tests use "critical" — the other half of the severity gate is untested.
- **Suggested tests:**
  - `alert_fired_no_admin_user_skips_spawn` — deactivate admin after test_state, fire alert, verify no session
  - `alert_fired_warning_severity_proceeds` — use severity "warning", verify handler proceeds past gate

### R7: [MEDIUM] Concurrent limit query doesn't check `users.is_active` or `projects.is_active`
- **File:** `src/store/eventbus.rs:595-601`
- **Domain:** Database
- **Description:** The JOIN doesn't filter on `users.is_active = true`. Stale sessions for deactivated agent users could inflate the count, preventing new ops agents. Similarly, soft-deleted projects could still have counted sessions.
- **Risk:** Orphan sessions from failed cleanup block new ops agent spawns.
- **Suggested fix:** Add `AND u.is_active = true` to the WHERE clause. Alternatively, this is naturally addressed by R2's suggested `agent_role` column approach.

## Low Findings (optional)

- [LOW] R8: `src/store/eventbus.rs:637` — Prompt uses `{value:?}` which renders as `Some(95.5)` or `None` in agent prompt. → Use `value.map_or("absent".to_string(), |v| format!("{v}"))` for cleaner output.
- [LOW] R9: `src/store/eventbus.rs:612-614` — Admin user lookup hardcodes `name = 'admin'`. If admin is renamed, all ops agent spawns silently fail. → Consider using `user_type = 'system'` or a config env var for the spawner ID.
- [LOW] R10: `src/observe/alert.rs:793` — Uses `&crate::store::AppState` instead of `&AppState` despite `AppState` being imported at file top. → Use `&AppState` for consistency.
- [LOW] R11: `src/store/eventbus.rs:668-672` — Cooldown fully cleared on spawn failure enables retry every 30s if failure is persistent. → Set a shorter cooldown (e.g., 3 min) on failure instead of full delete, for backoff.
- [LOW] R12: `src/store/eventbus.rs:637-645` — Alert name interpolated into agent prompt without sanitization. Malicious alert rule names could attempt prompt injection. → Wrap in delimiters and/or truncate to 100 chars.
- [LOW] R13: `tests/eventbus_integration.rs` — No test for per-rule cooldown isolation (same project, different rule_id). → Add test verifying rule_id_B proceeds when rule_id_A has cooldown set.

## Coverage — Touched Lines

| File | Lines changed | Estimated covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/store/eventbus.rs` | ~135 (new handler + dispatch + tests) | ~115 | ~85% | 618-621 (no admin), 668-672 (spawn fail cleanup) |
| `src/observe/alert.rs` | ~35 (refactor + publish) | ~20 | ~57% | 803-815 (publish path — no integration test for alert evaluator publish) |

**Note:** Diff-cover could not run because changes are uncommitted. Coverage percentages are estimated from test-path analysis. The `finalize` skill should run `just cov-diff` after committing to get precise numbers.

### Uncovered Paths
- `src/store/eventbus.rs:618-621` — Admin user not found path. Needs test that deactivates admin first.
- `src/store/eventbus.rs:668-672` — Spawn failure cooldown-clear path. Hard to test in integration (K8s may succeed). Consider E2E with poisoned config.
- `src/observe/alert.rs:803-815` — AlertFired event publish path after fire_alert. Requires running evaluate_all with a real alert rule that triggers. No existing test calls evaluate_all with matching conditions.

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | `?` propagation correct; spawn failure caught gracefully |
| Auth & permissions | PASS | Internal event bus only; ops role scoped appropriately |
| Input validation | PASS | All inputs from DB (trusted); no HTTP-facing endpoints added |
| Audit logging | N/A | No HTTP mutations; agent spawn creates audit trail via create_session |
| Tracing instrumentation | PASS | `#[tracing::instrument]` on handler with correct skip/fields |
| Clippy compliance | PASS | Currently passes; R1 notes borderline line count |
| Test patterns | PASS | All 5 integration tests follow project conventions |
| Migration safety | N/A | No new migrations |
| Touched-line coverage | NEEDS WORK | ~85% eventbus, ~57% alert.rs — R6 addresses gaps |
