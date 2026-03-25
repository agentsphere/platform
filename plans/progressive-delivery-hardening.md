# Plan: Progressive Delivery Hardening

## Context

The versioned canary deployment infrastructure (VERSION parsing, image tagging, git tags, two-PR demo flow, shared Envoy Gateway) is implemented but the runtime behavior has critical gaps:

1. **Analysis auto-passes** — `min_requests` (default 100) is defined in `CanaryRolloutConfig` but never checked in `analysis.rs`. Empty progress gates → instant `Pass`. Gates with no metric data → `Fail` after 3 attempts → rollback. Neither is correct.
2. **No deploy queuing** — New ops repo commits create new releases instantly, even if one is in-progress. Two concurrent canary releases in the same environment is undefined behavior.
3. **Demo doesn't self-complete** — The demo project creates PR1 and PR2 but the staging→prod promotion and the "observe canary in staging" experience require manual test assertions rather than happening automatically on `just run`.
4. **Prod `ErrImagePull`** — Promote-staging copies manifests but image refs may use staging-specific tags/registries that prod can't pull.
5. **Test validates control plane, not data plane** — The E2E test checks DB rows transition through phases but never verifies real traffic flows through the gateway or that analysis verdicts are based on actual metrics.

**Current code state:** `src/deployer/analysis.rs` runs every 15s, evaluates `progress_gates` and `rollback_triggers` via `crate::observe::alert::evaluate_metric()`, but skips the `min_requests` check entirely (the field is dead config). The reconciler in `handle_canary_progress()` reads verdicts and advances steps or triggers rollback.

## Design Principles

- **Canary in staging, rolling to prod** — Staging is the risk-mitigation environment. Canary traffic splitting happens there. Production gets the promoted winner via rolling deploy. Configurable per spec via `stages` field.
- **Cancel & replace** — New release cancels any in-progress release for the same target. Always deploy latest. Matches Argo Rollouts / Flagger behavior.
- **min_requests is a hard gate** — Analysis returns `Inconclusive` (not Pass or Fail) when traffic volume is below threshold. Reconciler treats `Inconclusive` as "wait" (same as no verdict). Canary holds until real data arrives.
- **Demo is self-driving** — On `just run`, the demo project auto-promotes v0.1 to prod after staging completes, then PR2 creates a live canary with traffic generator. A user opening the UI sees a canary deployment in progress.
- **Test observes what `just run` produces** — The E2E test calls `create_demo_project()` and then only observes/polls. All promotion logic, PR2 creation, and canary progression happen in platform code, not test code.

---

## PR 1: Analysis Hardening — `min_requests` gate + cancel-and-replace

Core runtime fixes: enforce traffic volume threshold, handle concurrent deploys.

- [x] Types & errors defined
- [x] Migration applied (none needed)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

Existing `rollout_config` JSONB already stores `min_requests`. The `deploy_releases.phase` CHECK constraint already includes `'cancelled'`.

### Code Changes

| File | Change |
|---|---|
| `src/deployer/analysis.rs` | Enforce `min_requests`: query total request count for project before evaluating gates. If below threshold, return `Inconclusive` verdict instead of `Pass`/`Fail`. |
| `src/deployer/analysis.rs` | Add `count_project_requests()` helper — queries `observe.metrics` store for total HTTP request count scoped to `platform.project_id` within the gate's time window. |
| `src/deployer/reconciler.rs` | In `handle_canary_progress()`: treat `Inconclusive` verdict same as `None` (wait, don't advance or fail). |
| `src/store/eventbus.rs` | In `handle_ops_repo_updated()`: before creating a new release, cancel any in-progress release for the same target. `UPDATE deploy_releases SET phase = 'cancelled' WHERE target_id = $1 AND phase IN ('pending','progressing','holding','paused') AND id != $2`. |
| `src/deployer/reconciler.rs` | In `handle_pending()` for canary: after `wait_healthy()`, verify the release hasn't been cancelled by a concurrent supersede before applying gateway resources. Re-read phase from DB before proceeding. |

### Analysis `min_requests` enforcement (in `evaluate_progress_gates`)

```rust
// Before evaluating gates, check min_requests threshold
let min_requests: u64 = rollout_config
    .get("min_requests")
    .and_then(serde_json::Value::as_u64)
    .unwrap_or(100);

if min_requests > 0 {
    let request_count = count_project_requests(state, project_id, window).await?;
    if request_count < min_requests {
        return Ok((AnalysisVerdict::Inconclusive, vec![serde_json::json!({
            "reason": "insufficient_traffic",
            "min_requests": min_requests,
            "actual_requests": request_count,
        })]));
    }
}
```

### Cancel-and-replace (in `handle_ops_repo_updated`)

```rust
// Cancel any in-progress releases for this target before creating new one
sqlx::query(
    "UPDATE deploy_releases SET phase = 'cancelled', completed_at = now()
     WHERE target_id = $1 AND phase IN ('pending','progressing','holding','paused')",
)
.bind(target_id)
.execute(&state.pool)
.await?;
```

### Reconciler `Inconclusive` handling (in `handle_canary_progress`)

```rust
match verdict.as_deref() {
    Some("pass") => { /* advance step */ }
    Some("fail") => { /* check max_failures */ }
    Some("inconclusive") => {
        // Insufficient traffic — wait for more data, don't count as failure
        tracing::info!(%release_id, "analysis inconclusive, waiting for traffic");
    }
    _ => {} // No verdict yet — wait
}
```

### Test Outline — PR 1

**New behaviors to test:**
- `min_requests` blocks progression when traffic is zero — unit test (mock metric query returning 0)
- `min_requests` allows progression when traffic exceeds threshold — unit test
- `Inconclusive` verdict does not increment fail count — unit test on reconciler logic
- Cancel-and-replace: creating a new release cancels in-progress one — integration test
- Cancelled release is skipped by reconciler — integration test

**Error paths to test:**
- Metric query failure during `count_project_requests` → `Inconclusive` (not crash) — unit test

**Existing tests affected:**
- `tests/deployment_integration.rs` — may need updates if cancel logic changes setup assumptions
- Analysis unit tests in `src/deployer/analysis.rs` — add new test cases

**Estimated test count:** ~6 unit + 2 integration

---

## PR 2: Deploy stages + demo auto-promotion

Per-spec environment targeting and self-driving demo flow.

- [x] Types & errors defined
- [x] Migration applied (none needed)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

The `deploy.specs[].stages` field is configuration in `.platform.yaml`, not a DB column. The existing `deploy_releases.strategy` and `deploy_targets.environment` columns handle per-environment strategy.

### Design: `deploy.specs[].stages`

Add optional `stages` field to `DeploySpec`:

```yaml
deploy:
  specs:
    - name: api
      type: canary
      stages: [staging]       # canary only in staging; prod gets rolling
      canary:
        stable_service: app-v0-1
        canary_service: app-v0-2
        steps: [10, 25, 50, 100]
```

**Defaults by type:**
- `rolling` → `stages: [staging, production]` (rolling everywhere)
- `canary` → `stages: [staging]` (canary in staging only, rolling in prod)
- `ab_test` → `stages: [staging, production]` (AB test in both)

When `handle_ops_repo_updated()` creates a release, it checks if the current `environment` is in the spec's `stages`. If not, it falls back to `rolling` strategy for that environment.

### Code Changes

| File | Change |
|---|---|
| `src/pipeline/definition.rs` | Add `stages: Option<Vec<String>>` to `DeploySpec` with `#[serde(default)]`. Add `fn effective_strategy(&self, environment: &str) -> &str` method. |
| `src/store/eventbus.rs` | In `resolve_deploy_config_from_specs()`: accept `environment` parameter. If spec has `stages` and current environment is not in the list, return `("rolling", json!({}))` instead of the spec's strategy. |
| `src/store/eventbus.rs` | Update `handle_ops_repo_updated()` to pass `environment` to `resolve_deploy_config_from_specs()`. |
| `src/onboarding/demo_project.rs` | Add auto-promotion logic: after v0.1 staging completes, auto-promote to production. Implemented as a new `auto_promote_after_staging()` function spawned from the eventbus when a `ReleaseCompleted` event fires for the demo project's staging target. |
| `src/store/eventbus.rs` | Handle `ReleasePromoted` event for demo project: if the promoted release is staging + demo project, call `promote_staging()` automatically. |
| `src/onboarding/demo_project.rs` | In `create_demo_pr2()`: remove the test-only trigger. PR2 creation already happens via post-merge hook. Add a platform_setting `demo_v0.1_promoted` that gates PR2: PR2 is only created after v0.1 prod promotion completes (not immediately after v0.1 MR merges). |
| `src/onboarding/templates/platform_v0.1.yaml` | Remove `deploy.specs` entirely — rolling is the default, no spec needed. Keep `deploy.variables` for per-env values. |
| `src/onboarding/templates/platform_v0.2.yaml` | Add `stages: [staging]` to the canary spec so prod gets rolling. Lower `min_requests` to 10 for faster demo progression. Lower `interval` to 30 for demo speed. |

### Demo auto-promotion flow

```
startup → create_demo_project()
  → PR1 (v0.1) created on feature/shop-app-v0.1
  → MR pipeline runs → auto-merge
  → push pipeline (gitops_sync) → staging release (rolling)
  → staging completes → eventbus sees ReleasePromoted for demo staging
  → auto-promote: calls promote_staging() → prod release (rolling)
  → prod completes → sets demo_v0.1_promoted=true
  → post-promote hook: creates PR2 (v0.2 canary)
  → PR2 MR pipeline → auto-merge
  → push pipeline (gitops_sync) → staging release (canary, because stages=[staging])
  → traffic generator deploys alongside canary
  → analysis loop: waits for min_requests, evaluates error_rate gate
  → canary steps advance: 10% → 25% → 50% → 100% → promoted
  → staging canary complete
  (user sees live canary progression in the UI)
```

### Revised `platform_v0.1.yaml`

```yaml
pipeline:
  on:
    push:
      branches: ["main"]
    mr:
      actions: [opened, synchronized]
  steps:
    - name: build-app
      type: imagebuild
      imageName: app
      dockerfile: Dockerfile
    - name: build-dev
      type: imagebuild
      imageName: dev
      dockerfile: Dockerfile.dev
    - name: build-test
      type: imagebuild
      imageName: test
      dockerfile: Dockerfile.test
      only:
        events: [mr]
    - name: e2e
      depends_on: [build-app, build-test]
      only:
        events: [mr]
      deploy_test:
        test_image: $REGISTRY/$PROJECT/test:$COMMIT_SHA
        manifests: testinfra/
        readiness_timeout: 120
        wait_for_services: [platform-demo-app, platform-demo-db]
    - name: sync-ops-repo
      type: gitops_sync
      depends_on: [build-app]
      only:
        events: [push]
        branches: ["main"]
      gitops:
        copy: ["deploy/", ".platform.yaml"]
    - name: watch-deploy
      type: deploy_watch
      depends_on: [sync-ops-repo]
      only:
        events: [push]
        branches: ["main"]
      deploy_watch:
        environment: staging
        timeout: 300

flags:
  - key: new_checkout_flow
    default_value: false
    description: "Enable the new checkout experience"
  - key: dark_mode
    default_value: false
    description: "Enable dark mode theme"

deploy:
  variables:
    staging: deploy/variables_staging.yaml
    production: deploy/variables_prod.yaml
```

No `specs` — rolling is the default for all environments.

### Revised `platform_v0.2.yaml`

```yaml
# ... same pipeline section as v0.1 ...

deploy:
  variables:
    staging: deploy/variables_staging.yaml
    production: deploy/variables_prod.yaml
  specs:
    - name: api
      type: canary
      stages: [staging]
      canary:
        stable_service: platform-demo-app-v0-1
        canary_service: platform-demo-app-v0-2
        steps: [10, 25, 50, 100]
        interval: 30
        min_requests: 10
        progress_gates:
          - metric: error_rate
            condition: lt
            threshold: 0.05
```

`stages: [staging]` means prod gets rolling deploy (just applies the manifests). `min_requests: 10` and `interval: 30` for fast demo progression.

### PR2 creation gate

Currently PR2 is created immediately when PR1's MR merges (via post-merge hook). This is too early — PR1 hasn't deployed yet. Change the gate:

1. Remove the `feature/shop-app-v0.1` branch check from `run_post_merge_side_effects()` in `merge_requests.rs`
2. Add a `ReleaseCompleted` event handler in `eventbus.rs`: when demo project's production release completes, create PR2
3. Store `demo_prod_promoted` setting to prevent duplicate PR2 creation

### Test Outline — PR 2

**New behaviors to test:**
- `DeploySpec.stages` parsing — unit test
- `effective_strategy()` returns canary for staging, rolling for prod — unit test
- `resolve_deploy_config_from_specs()` with environment param — unit test
- Demo auto-promotion fires after staging complete — E2E
- PR2 created only after prod promotion — E2E

**Existing tests affected:**
- `tests/e2e_demo.rs::demo_full_lifecycle` — restructure to observe instead of drive
- `src/onboarding/demo_project.rs` tests — update for new template content

**Estimated test count:** ~4 unit + 3 E2E (lifecycle stages)

---

## PR 3: E2E Test — real traffic, real analysis, full lifecycle

The test becomes an observer of the self-driving demo.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

### Code Changes

| File | Change |
|---|---|
| `tests/e2e_demo.rs` | Rewrite `demo_full_lifecycle` as an observer test. Call `create_demo_project()`, spawn background tasks, then poll/verify each stage without driving promotions or creating PRs. |
| `src/onboarding/templates/deploy/traffic-generator.yaml` | Ensure traffic generator hits `/healthz` and `/product/1` on both services frequently enough (every 1s instead of 2s) to reach `min_requests: 10` quickly. |

### Revised test flow

```rust
// Stage 1: Create demo project (same as today)
create_demo_project(&state, admin_id).await;

// Stage 2: Poll — MR pipeline completes
poll_until("PR1 MR pipeline terminal", 300, ...);

// Stage 3: Poll — auto-merge fires
poll_until("PR1 merged", 60, ...);

// Stage 4: Poll — push pipeline completes (gitops_sync)
poll_until("push pipeline terminal", 300, ...);

// Stage 5: Poll — v0.1 staging release completes (rolling)
poll_until("staging release completed", 180, ...);
assert_strategy("staging", "rolling");

// Stage 6: Poll — v0.1 production release completes (auto-promoted)
poll_until("prod release completed", 180, ...);
assert_strategy("prod", "rolling");
// Verify git tag v0.1.0 exists

// Stage 7: Poll — PR2 created (auto, after prod completes)
poll_until("PR2 exists", 60, ...);
assert_branch("feature/shop-app-v0.2");

// Stage 8: Poll — PR2 pipeline completes + auto-merges
poll_until("PR2 merged", 300, ...);

// Stage 9: Poll — v0.2 push pipeline completes
poll_until("v0.2 push pipeline", 300, ...);

// Stage 10: Poll — v0.2 staging canary release created
poll_until("canary release progressing", 60, ...);
assert_strategy("staging", "canary");

// Stage 11: Poll — traffic generator running
let traffic_gen_running = check_pod_running(&state, staging_ns, "traffic-gen");
assert!(traffic_gen_running);

// Stage 12: Poll — canary steps advance (real analysis with traffic data)
poll_until("canary traffic_weight > 10", 120, ...);  // past step 0

// Stage 13: Poll — canary completes (all steps passed)
poll_until("canary release completed", 300, ...);

// Stage 14: Verify — v0.1 deployment downscaled
let v01_replicas = get_deployment_replicas(&state, staging_ns, "platform-demo-app-v0-1");
assert_eq!(v01_replicas, 0);

// Stage 15: Verify — git tag v0.2.0 exists
assert_git_tag(&repo_path, "v0.2.0");

// Stage 16: Poll — v0.2 production release (rolling, from stages config)
poll_until("v0.2 prod release completed", 180, ...);
assert_strategy("prod", "rolling");
```

### Key verification points

1. **Real traffic**: traffic-gen pod is `Running`, services are receiving requests
2. **Real analysis**: `rollout_analyses` rows have `verdict='pass'` with non-null `metric_results` containing actual values
3. **min_requests enforced**: early analyses show `verdict='inconclusive'` before traffic arrives
4. **Canary steps advance**: `traffic_weight` increases through 10→25→50→100
5. **Old version downscaled**: v0-1 deployment has 0 replicas after promotion
6. **Prod uses rolling**: production release has `strategy='rolling'` (not canary)

### Test Outline — PR 3

**New behaviors to test:**
- Full self-driving lifecycle (the 16-stage flow above) — E2E
- Traffic generator produces measurable request volume — E2E (verify pod Running + analysis has data)
- Analysis `inconclusive` → `pass` transition as traffic ramps — E2E (check rollout_analyses rows)

**Existing tests affected:**
- `tests/e2e_demo.rs::demo_full_lifecycle` — complete rewrite
- `tests/e2e_demo.rs::demo_project_creation` — update branch assertion from `feature/shop-app-v0.1`

**Estimated test count:** 1 large E2E test (replaces existing `demo_full_lifecycle`)

---

## Implementation Order

```
PR 1 (analysis + cancel-replace) — foundational runtime fixes
  ↓
PR 2 (stages + auto-promotion) — demo self-driving logic, depends on PR 1 for correct canary behavior
  ↓
PR 3 (E2E test rewrite) — validates everything, depends on PR 1 + PR 2
```

## Verification

1. `just test-unit` — analysis `min_requests`, `Inconclusive` verdict, `stages` parsing
2. `just test-integration` — cancel-and-replace, `resolve_deploy_config_from_specs` with environment
3. `just test-e2e-bin e2e_demo demo_full_lifecycle` — full self-driving lifecycle with real traffic
4. `just run` — observe demo: v0.1 deploys to staging → auto-promotes to prod → PR2 canary in staging with traffic generator → canary progresses through steps → user sees live canary in UI
