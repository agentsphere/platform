# Crate Architecture Audit — 2026-04-15

## Executive Summary

The 22-crate workspace has a **clean, acyclic dependency DAG** with good layering (foundation → libs → bins). However, the crate restructure is **incomplete**: 6 critical service traits have no production implementation (lost when `src/` was deleted), 3 god traits need decomposition, and significant duplication exists across config types, error types, and authentication code. The top priority is restoring production trait implementations and decomposing `GitServerServices` (20 methods) and `ReconcilerServices` (16 methods).

## Crate Inventory

| Crate | Type | LOC (src) | LOC (test) | Workspace Deps | Pub Items | Traits Defined | Traits Implemented |
|-------|------|-----------|------------|----------------|-----------|----------------|-------------------|
| platform-types | foundation | 4,012 | 254 | 0 | 27 | 15 | 1 (AuditLog) |
| platform-auth | lib | 1,023 | 999 | 1 | 17 | 0 | 3 (PgPermChecker, PgPermResolver, PgWorkspaceMembership) |
| platform-observe | lib | 8,579 | 1,234 | 2 | 13 | 0 | 0 |
| platform-secrets | lib | 2,031 | 1,358 | 1 | 5 | 0 | 0 (SecretsResolver mock only) |
| platform-k8s | lib | 1,461 | 592 | 0 | 4 | 0 | 0 |
| platform-git | lib | 8,691 | 167 | 2 | 45 | 8 | 8 (all Pg* + Cli* types) |
| platform-registry | lib | 3,531 | 1,212 | 2 | 18 | 0 | 1 (RegistryCredentials) |
| platform-agent | lib | 15,175 | 1,182 | 5 | 24 | 2 | 1 (ClaudeCodeProvider) |
| platform-pipeline | lib | 14,305 | 1,374 | 4 | 13 | 1 | 2 (ConcretePipelineServices, MockPipelineServices) |
| platform-ops-repo | lib | 1,558 | 561 | 2 | 3 | 0 | 1 (OpsRepoService) |
| platform-deployer | lib | 9,208 | 485 | 1 | 13 | 1 | 1 (DeployerService, MockReconcilerServices) |
| platform-webhook | lib | 539 | 0 | 1 | 6 | 0 | 1 (WebhookDispatch) |
| platform-notify | lib | 329 | 0 | 2 | 6 | 0 | 1 (SmtpNotificationDispatcher) |
| platform-mesh | lib | 1,398 | 0 | 3 | 14 | 0 | 0 |
| platform-operator | lib | 1,736 | 945 | 1 | 3 | 0 | 0 |
| platform-next | bin | 20,570 | 0 | 14 | 10 | 0 | 0 |
| platform-ingest | bin | 893 | 1,283 | 3 | 5 | 0 | 0 |
| platform-agent-runner | bin | 6,253 | 0 | 0 | 4 | 0 | 0 |
| platform-k8s-watcher | bin | 936 | 0 | 1 | 3 | 0 | 0 |
| platform-proxy | bin | 11,222 | 0 | 0 | 2 | 0 | 0 |
| platform-proxy-init | bin | 189 | 0 | 0 | 0 | 0 | 0 |
| platform-seed | bin | 410 | 0 | 1 | 0 | 0 | 0 |

**Totals**: 114,049 LOC src, 9,646 LOC tests, 28 traits defined

## Dependency DAG

```
                          ┌─────────────────────┐
                          │    platform-types    │ ← foundation, 0 deps
                          └──────────┬──────────┘
           ┌──────────┬──────────┬───┼───┬──────────┬──────────┐
           ▼          ▼          ▼   │   ▼          ▼          │
       ┌──────┐  ┌────────┐ ┌──────┐│┌───────┐┌────────┐      │
       │ auth │  │secrets │ │deploy│││webhook││operator│      │
       │(1dep)│  │(1 dep) │ │(1dep)│││(1 dep)││(1 dep) │      │
       └──┬───┘  └──┬─────┘ └──────┘│└───────┘└────────┘      │
          │         │               │                     ┌────┴────┐
     ┌────┼─────────┤               │                     │  k8s    │
     │    │         │               │                     │ (0 dep) │
     ▼    ▼         │               │                     └────┬────┘
  ┌────────┐        │  ┌──────┐     │                          │
  │observe │        │  │notify│     │                          │
  │(2 deps)│        │  │(2dep)│     │       ┌──────────────────┤
  └────────┘        │  └──────┘     │       │                  │
     │         ┌────┼───────────────┤       │                  │
     ▼         ▼    ▼               ▼       ▼                  │
  ┌──────┐  ┌─────────┐      ┌──────────┐  ┌──────┐           │
  │ git  │  │  mesh   │      │ pipeline │  │agent │           │
  │(2dep)│  │ (3 dep) │      │ (4 deps) │  │(5dep)│           │
  └──┬───┘  └─────────┘      └──────────┘  └──┬───┘           │
     │                                         │               │
     ▼                                         ▼               │
  ┌─────────┐                            ┌──────────┐          │
  │ops-repo │                            │registry │          │
  │ (2 dep) │                            │ (2 dep) │          │
  └─────────┘                            └──────────┘          │
     │                                         │               │
     └──────────────────┬──────────────────────┘               │
                        ▼                                      │
              ┌──────────────────┐                             │
              │  BINARY CRATES   │                             │
              ├──────────────────┤                             │
              │ platform-next    │◄────────────────────────────┘
              │  (14 deps)       │
              │ platform-ingest  │ (3 deps: types, auth, observe)
              │ platform-seed    │ (1 dep: auth)
              │ k8s-watcher      │ (1 dep: observe)
              │ agent-runner     │ (0 deps — standalone)
              │ proxy            │ (0 deps — standalone)
              │ proxy-init       │ (0 deps — standalone)
              └──────────────────┘
```

No cycles detected. Clean layered DAG.

## Findings

### Critical (5)

**[CRITICAL] C1: GitServerServices — god trait with ~20 methods, zero implementations**

- **Crate**: platform-git/src/server_services.rs
- **Issue**: 15 own methods + 5 supertrait requirements (`GitAuthenticator + GitAccessControl + ProjectResolver + BranchProtectionProvider + PostReceiveHandler`). Mixes authentication rate limiting, SSH key management, path resolution, permission checking, audit logging, GPG key lookup, LFS presigning, and workspace boundary checking. The production implementation was in the deleted `src/` directory.
- **Fix**: Split into 3-4 focused traits (`GitRateLimiter`, `GitRepoPathResolver`, `LfsStorage`, `GitSignatureCache`). Re-create the production implementation in the binary crate.

**[CRITICAL] C2: ReconcilerServices — god trait with 16 methods, only mock impl**

- **Crate**: platform-deployer/src/state.rs
- **Issue**: Bundles ops repo I/O, webhook dispatch, secret decryption, namespace management, token generation, event publishing, and alert evaluation into one trait. `check_condition()` and `evaluate_metric()` have no business in a deployer services trait. Only `MockReconcilerServices` exists.
- **Fix**: Decompose into `CoreReconcilerServices` (10 methods) and `AnalysisServices` (6 methods). Follow the `ConcretePipelineServices` composition pattern.

**[CRITICAL] C3: DynWebhookDispatcher — zero implementations anywhere**

- **Crate**: platform-agent/src/state.rs
- **Issue**: Wrapper trait for `WebhookDispatcher` to enable dyn dispatch. No struct implements it. The impl was in deleted `src/`.
- **Fix**: Either re-create the impl in the binary, or refactor `AgentState` to use generics like `PipelineState<Svc>`.

**[CRITICAL] C4: Multiple traits with no production implementation**

- **Crates**: platform-types (definitions), missing impls everywhere
- **Issue**: `SecretsResolver`, `NotificationDispatcher`, `MergeRequestHandler`, `PostReceiveSideEffects` — all only have mock/test implementations. Production code was in deleted `src/`.
- **Fix**: Re-create implementations in the binary crate or extract into dedicated library crates.

**[CRITICAL] C5: CliError duplicated between platform-agent and platform-agent-runner**

- **Files**: `platform-agent/src/claude_cli/error.rs`, `platform-agent-runner/src/error.rs`
- **Issue**: Comment says "keep in sync manually." Runner adds extra `PubSubError(String)` variant. Manual sync is fragile.
- **Fix**: Extract shared `CliError` into `platform-types` or a dedicated `platform-cli-types` crate. Runner extends via wrapper if needed.

### High (11)

**[HIGH] H1: No newtype wrappers for domain IDs**

- **Crate**: All crates
- **Issue**: Despite CLAUDE.md documenting `UserId(Uuid)` pattern, zero newtypes exist. All structs use raw `Uuid` for `user_id`, `project_id`, `session_id`, etc. A `project_id` can be passed where `user_id` is expected.
- **Fix**: Introduce `ProjectId(Uuid)`, `UserId(Uuid)`, `SessionId(Uuid)` in `platform-types`. Migrate incrementally.

**[HIGH] H2: Config type duplication across 6 domain crates**

- **Crates**: platform-types vs platform-agent, platform-deployer, platform-observe, platform-registry, platform-operator, platform-mesh
- **Issue**: Same-named config structs with different fields in platform-types::config vs domain crates. `DeployerConfig` has 2 fields in platform-types but 17 in platform-deployer. Changes must be applied in two places.
- **Fix**: Domain crates own canonical config. Remove platform-types duplicates. `PlatformConfig::load()` constructs domain config types directly.

**[HIGH] H3: 6 unused workspace deps in platform-next**

- **Crate**: platform-next (bins/platform)
- **Issue**: `platform-pipeline`, `platform-observe`, `platform-git`, `platform-notify`, `platform-webhook`, `platform-registry` declared but unused (API modules commented out). Increases compile time.
- **Fix**: Remove until corresponding API modules are wired up.

**[HIGH] H4: anyhow::Result in 30+ public library functions**

- **Crates**: platform-secrets, platform-auth, platform-notify, platform-k8s, platform-registry, platform-git, platform-operator, platform-types
- **Issue**: Library public APIs return `anyhow::Result` instead of typed errors, preventing callers from pattern-matching on specific failures.
- **Fix**: Define typed error enums for each crate's public API. Reserve `anyhow` for `Other` catch-all variants.

**[HIGH] H5: DeployerError and PipelineError lack ApiError conversion**

- **Crates**: platform-deployer, platform-pipeline
- **Issue**: `DeployerError` (17 variants) and `PipelineError` (7 variants) have no `From<...> for ApiError`. The conversion was in deleted `src/`. Callers must manually map or lose semantic HTTP status codes.
- **Fix**: Implement `From<DeployerError> for ApiError` and `From<PipelineError> for ApiError`.

**[HIGH] H6: Duplicate AuthUser extractor in platform-observe**

- **Files**: `platform-observe/src/auth.rs` vs `bins/platform/src/middleware.rs`
- **Issue**: ~50 lines of near-identical code for `FromRequestParts<ObserveState> for AuthUser`. Observe version lacks trust_proxy_cidrs support.
- **Fix**: Extract generic `authenticate()` function into `platform-auth`. Both extractors become 5-line wrappers.

**[HIGH] H7: Orphaned crate platform-ops-repo**

- **Crate**: platform-ops-repo
- **Issue**: Not depended upon by any other workspace crate. Only consumed by its own integration test. `ReconcilerServices` trait provides equivalent methods, bypassing this crate.
- **Fix**: Wire as dependency of platform-deployer (concrete impl behind `ReconcilerServices`) or archive.

**[HIGH] H8: Duplicate match_glob_pattern function**

- **Files**: `platform-types/src/validation.rs`, `platform-git/src/validation.rs`
- **Issue**: 100% identical implementation in two locations.
- **Fix**: Remove from platform-git, use platform-types version.

**[HIGH] H9: CliError → AgentError context loss**

- **File**: `platform-agent/src/claude_cli/error.rs`
- **Issue**: All `CliError` variants converted to `AgentError::Other(err.into())` → every CLI error becomes HTTP 500. `CliNotFound` should be 503, `InitTimeout` should be 504.
- **Fix**: Map specific variants to appropriate `AgentError` variants.

**[HIGH] H10: PipelineServices trait — 10 methods, significant overlap with foundation traits**

- **File**: `platform-pipeline/src/state.rs`
- **Issue**: Re-declares methods already on `WebhookDispatcher`, `OpsRepoManager`, `ManifestApplier`, `RegistryCredentialProvider`. `fire_webhooks` defined in 5 places; ops repo methods in 3 places.
- **Fix**: Use supertrait composition instead of re-declaring. `ConcretePipelineServices` demonstrates the delegation is already 1:1.

**[HIGH] H11: master_key stored 6 times, 4 without Debug redaction**

- **Crates**: platform-types (redacted), platform-deployer, platform-pipeline, platform-mesh, platform-agent, platform-operator (all `#[derive(Debug)]`)
- **Issue**: Derived `Debug` on domain configs will print the master key in logs.
- **Fix**: Custom `Debug` impls that redact `master_key`, or receive key via trait/function instead of storing.

### Medium (14)

**[MEDIUM] M1: platform-git depends on platform-auth for single hash_token() call**

- Heavyweight dep (argon2, webauthn-rs) for a 3-line SHA-256 wrapper in `db_services.rs:140`.
- **Fix**: Move `hash_token` to platform-types.

**[MEDIUM] M2: platform-seed depends on platform-auth for single function**

- Same pattern as M1. `generate_api_token()` is ~10 lines.
- **Fix**: Move to platform-types or inline.

**[MEDIUM] M3: platform-notify depends on platform-auth for single check_rate() call**

- Rate limiter only needs `fred` + `ApiError`, both in platform-types.
- **Fix**: Move `check_rate` to platform-types.

**[MEDIUM] M4: Proto/message type duplication across 3 pairs**

- OTLP proto types duplicated in platform-observe and platform-proxy
- CLI messages duplicated in platform-agent and platform-agent-runner
- Control payloads duplicated in platform-agent and platform-agent-runner
- **Fix**: Shared crates for proto types and CLI protocol types.

**[MEDIUM] M5: String where enum would be safer (~35 fields)**

- `severity`, `strategy`, `visibility`, `condition`, `aggregation`, `status` fields use String for known closed sets.
- **Fix**: Replace with typed enums. Priority: fields crossing crate boundaries.

**[MEDIUM] M6: Missing #[non_exhaustive] on cross-crate enums**

- Zero uses across 55+ public enums. Key candidates: `PlatformEvent` (15 variants), `ApiError`, `Permission` (22 variants), `ProgressKind`.
- **Fix**: Add `#[non_exhaustive]` to frequently-extended enums.

**[MEDIUM] M7: WEBHOOK_CLIENT and WEBHOOK_SEMAPHORE duplicated**

- Static instances in both `platform-webhook/dispatch.rs` and `bins/platform/api/webhooks.rs`.
- Two independent semaphores = 100 concurrent webhooks instead of 50.
- **Fix**: Remove binary duplicates, use platform-webhook exclusively.

**[MEDIUM] M8: executor.rs is 8,098 lines**

- Largest file in workspace. 55+ functions.
- **Fix**: Split into sub-modules: `executor/loop.rs`, `executor/steps.rs`, `executor/pods.rs`, `executor/deploy.rs`, `executor/artifacts.rs`.

**[MEDIUM] M9: Infrastructure resources repeated in every state struct**

- `PgPool`, `fred::Pool`, `kube::Client`, `opendal::Operator` duplicated in 8+ state types.
- **Fix**: Shared `InfraClients { pool, valkey, kube, minio }` struct.

**[MEDIUM] M10: PipelineConfig/DeployerConfig cloned by value on every handler**

- ~10 owned Strings allocated per clone. Other states use `Arc<Config>`.
- **Fix**: Wrap in `Arc` within state types.

**[MEDIUM] M11: reqwest 0.12/0.13 split in workspace**

- Two separate reqwest/hyper dependency trees compiled.
- **Fix**: Upgrade all crates to reqwest 0.13.

**[MEDIUM] M12: Glob re-exports in platform-git and platform-auth**

- `pub use traits::*` and `pub use types::*` in platform-git. `pub use platform_types::auth_user::*` in platform-auth. Every other crate uses named re-exports.
- **Fix**: Replace with explicit named items.

**[MEDIUM] M13: Over-exposed modules in platform-observe, platform-agent**

- `partitions`, `error`, `correlation` in observe have zero external consumers.
- `manager_prompt`, `preview_watcher` in agent are internal details.
- **Fix**: Change to `pub(crate) mod`.

**[MEDIUM] M14: Hidden DB coupling — git push handler writes directly to merge_requests table**

- `PgPostReceiveHandler::on_push()` and `on_mr_sync()` execute SQL on `merge_requests` without a trait boundary.
- **Fix**: Add `MergeRequestUpdater` trait to make the DB coupling explicit.

### Low (14)

**[LOW] L1: Inconsistent `default-features = false` across platform-types consumers**

**[LOW] L2: Test helpers duplicated across crates (valkey_pool, kube_client setup)**

- **Fix**: Consider `platform-test-utils` dev-dependency crate.

**[LOW] L3: `#[allow(dead_code)]` on `pub mod claude_cli` in platform-agent**

**[LOW] L4: Blanket clippy `too_many_arguments` + `too_many_lines` in platform-pipeline**

**[LOW] L5: DeployerError and OpsRepoError have 7 overlapping String variants**

- **Fix**: Add `#[from] OpsRepoError` variant to DeployerError.

**[LOW] L6: GitError::IntoResponse leaks internal error details (stderr, IO errors)**

- **Fix**: Log and return generic message for 500-level variants.

**[LOW] L7: MeshError::Secrets accepts any anyhow::Error via `#[from]`**

- **Fix**: Rename to `Other`.

**[LOW] L8: `UploadSession` uses String IDs instead of Uuid**

**[LOW] L9: Redundant `ReleasePhase::parse()` alongside `FromStr` impl**

**[LOW] L10: Triple path to `slugify_branch` (platform-types, platform-types crate root, platform-pipeline)**

**[LOW] L11: Tests embedded in platform-pipeline/src/lib.rs (230 lines)**

**[LOW] L12: Mixed lock types — std::sync::RwLock (OperatorState) vs tokio::sync::RwLock (PlatformState) for same HealthSnapshot**

**[LOW] L13: DEV_MODE static in webhooks.rs duplicates PlatformConfig logic**

**[LOW] L14: PlatformState uses `Arc<RwLock<HashMap<Uuid, serde_json::Value>>>` for cli_sessions instead of typed CliSessionManager**

### Info / Positive Patterns (8)

**[INFO] I1: Clean dependency DAG — no cycles, strict layering**

Foundation → leaf libs → composite libs → bins. No lib-to-bin reverse deps.

**[INFO] I2: All 28 traits use native RPITIT (no async-trait crate)**

Zero heap allocations from boxed futures. Modern Rust idiom.

**[INFO] I3: No unsafe code in library crates**

`unsafe_code = "forbid"` everywhere except `platform-agent-runner` which uses `deny` for `env::set_var`.

**[INFO] I4: No transitive dependency re-exports (no `pub use sqlx::*`)**

Excellent encapsulation discipline.

**[INFO] I5: No `Box<dyn Any>` type erasure anywhere**

Clean generics and concrete trait objects throughout.

**[INFO] I6: All async chains are non-blocking**

No `block_on`, `std::thread::spawn`, or `std::sync::Mutex` in async paths.

**[INFO] I7: No chain exceeds 4 crate hops**

Auth: 3, Pipeline: 4, Observe: 3, Git: 3-4, Deploy: 4.

**[INFO] I8: Copy/Clone derives are correct throughout**

`Copy` only on small enums. State types clone cheaply via Arc-wrapped fields.

## Trait Catalog

| Trait | Defined In | Implemented By | Methods | Status |
|-------|-----------|----------------|---------|--------|
| AuditLogger | types/traits.rs | AuditLog (types) | 1 | OK |
| SecretsResolver | types/traits.rs | Mock only | 3 | **NO PROD IMPL** |
| NotificationDispatcher | types/traits.rs | Mock only | 1 | **NO PROD IMPL** |
| WorkspaceMembershipChecker | types/traits.rs | PgWorkspaceMembership (auth) | 1 | OK |
| WebhookDispatcher | types/traits.rs | WebhookDispatch (webhook) | 1 | OK |
| TaskHeartbeat | types/traits.rs | TaskRegistry (types), NoopHeartbeat (pipeline) | 3 | OK |
| MergeRequestHandler | types/traits.rs | Mock only | 1 | **NO PROD IMPL** |
| OpsRepoManager | types/traits.rs | OpsRepoService (ops-repo) | 5 | OK |
| ManifestApplier | types/traits.rs | DeployerService (deployer) | 1 | OK |
| RegistryCredentialProvider | types/traits.rs | RegistryCredentials (registry) | 2 | OK |
| GitCoreRead | types/git_traits.rs | CliGitRepo (git) | 6 | OK |
| GitWriter | types/git_traits.rs | CliGitWorktreeWriter (git) | 3 | OK |
| GitMerger | types/git_traits.rs | CliGitMerger (git) | 3 | OK |
| PermissionChecker | types/auth_user.rs | PgPermissionChecker (auth) | 2 | OK |
| PermissionResolver | types/auth_user.rs | PgPermissionChecker (auth) | 3 | OK |
| DynWebhookDispatcher | agent/state.rs | **NONE** | 1 | **ZERO IMPLS** |
| PostReceiveSideEffects | git/db_services.rs | Mock only | 2 | **NO PROD IMPL** |
| GitServerServices | git/server_services.rs | **NONE** | ~20 | **GOD TRAIT, ZERO IMPLS** |
| ReconcilerServices | deployer/state.rs | MockReconcilerServices | 16 | **GOD TRAIT, MOCK ONLY** |
| AgentProvider | agent/provider.rs | ClaudeCodeProvider (agent) | 3 | OK (2 methods dead) |
| PipelineServices | pipeline/state.rs | ConcretePipelineServices, Mock | 10 | OK (high overlap) |
| GitRepo | git/traits.rs | CliGitRepo (git) | 11 total | OK (well-factored) |
| GitRepoManager | git/traits.rs | CliGitRepoManager (git) | 3 | OK |
| PostReceiveHandler | git/traits.rs | PgPostReceiveHandler (git) | 3 | OK |
| BranchProtectionProvider | git/traits.rs | PgBranchProtectionProvider (git) | 1 | OK |
| GitAuthenticator | git/traits.rs | PgGitAuthenticator (git) | 2 | OK |
| GitAccessControl | git/traits.rs | PgGitAccessControl (git) | 2 | OK |
| ProjectResolver | git/traits.rs | PgProjectResolver (git) | 1 | OK |

## Error Type Map

| Error Type | Crate | Variants | Converts From | Converts To |
|-----------|-------|----------|---------------|-------------|
| ApiError | platform-types | 10 | sqlx, fred, kube, opendal, anyhow | IntoResponse (JSON) |
| GitError | platform-types | 13 | io, anyhow | IntoResponse (plain text) |
| ObserveError | platform-observe | 8 | sqlx, opendal, Arrow, Parquet, anyhow | ApiError |
| DeployerError | platform-deployer | 17 | sqlx, kube, anyhow | **NONE** |
| PipelineError | platform-pipeline | 7 | sqlx, kube, opendal, anyhow | **NONE** |
| AgentError | platform-agent | 10 | sqlx, kube, anyhow | ApiError |
| CliError (agent) | platform-agent | 10 | — | AgentError::Other |
| CliError (runner) | platform-agent-runner | 11 | — | — (standalone) |
| RegistryError | platform-registry | 13 | sqlx, opendal, anyhow | IntoResponse (OCI JSON) |
| MeshError | platform-mesh | 6 | sqlx, anyhow | ApiError |
| K8sError | platform-k8s | 3 | kube, anyhow | — |
| OpsRepoError | platform-ops-repo | 9 | sqlx, anyhow, GitError | — |

## State Composition

```
                    PlatformState (bins/platform)
                    ┌─────────────────────────────┐
                    │ pool: PgPool                 │
                    │ valkey: fred::Pool            │
                    │ minio: opendal::Operator      │
                    │ kube: kube::Client            │
                    │ config: Arc<PlatformConfig>   │
                    │ pipeline_notify: Arc<Notify>  │
                    │ deploy_notify: Arc<Notify>    │
                    │ task_registry: Arc<TaskReg>   │
                    │ health: Arc<RwLock<Snapshot>>  │
                    │ secret_requests: Arc<RwLock<>> │  ← serde_json::Value placeholder
                    │ cli_sessions: Arc<RwLock<>>   │  ← serde_json::Value placeholder
                    └─────────────────────────────┘
                    NOT YET composing domain substates ↓

    AgentState       PipelineState<Svc>    DeployerState<Svc>    ObserveState
    ├─pool           ├─pool                ├─pool                ├─pool
    ├─valkey         ├─valkey              ├─valkey              ├─valkey
    ├─kube           ├─kube                ├─kube                ├─minio
    ├─minio          ├─minio               ├─minio               ├─config
    └─config         ├─config              ├─config              └─alert_router
                     ├─pipeline_notify     └─deploy_notify
                     └─task_heartbeat

    RegistryState    NotifyState    OperatorState    MeshState    IngestState
    ├─pool           ├─pool         ├─pool           ├─kube       ├─pool
    ├─minio          ├─valkey       ├─valkey         ├─config     ├─valkey
    ├─kube           └─config       ├─kube           └─mesh_ca    └─trust_proxy
    ├─valkey                        ├─minio
    └─config                        ├─config
                                    └─task_registry
```

## Key Call Chains

### Chain 1: Auth (3 crates)
```
HTTP Request → bins/platform/middleware.rs → platform-auth/extract.rs
                                           → platform-auth/lookup.rs (DB)
                                           → platform-auth/resolver.rs (Valkey cache + DB)
                                                     ↓ uses
                                           platform-types/auth_user.rs (AuthUser, Permission)
```
Assessment: Clean. Well-defined trait boundaries (`PermissionChecker`, `PermissionResolver`).

### Chain 2: Pipeline (4 crates)
```
trigger_on_push() → platform-pipeline/trigger.rs
                  → platform-pipeline/executor.rs (background loop)
                    → PipelineServices trait (10 methods → concrete delegates to 5 foundation traits)
                    → platform-k8s/namespace.rs (ensure_namespace)
                    → platform-auth/token.rs (hash_token, generate_api_token)
                    → K8s Pod creation → wait → log capture → finalize
```
Assessment: Good trait composition via `ConcretePipelineServices`. executor.rs at 8K lines needs splitting.

### Chain 3: Observe (3 crates)
```
OTLP HTTP → platform-observe/ingest.rs (decode protobuf)
           → mpsc channels → background flush tasks
           → platform-observe/store.rs (batch DB insert)
           → platform-observe/query.rs (read API)
             → platform-auth/resolver.rs (permission checks)
```
Assessment: Well-contained. Duplicate auth extractor is the main issue.

### Chain 4: Git Push (3-4 crates)
```
receive-pack → platform-git/hooks.rs (parse pack commands)
             → platform-git/server_services.rs (GitServerServices trait)
             → platform-git/db_services.rs (PgPostReceiveHandler)
               → PostReceiveSideEffects::trigger_pipeline() → main binary → platform-pipeline
               → direct SQL UPDATE on merge_requests (hidden coupling)
```
Assessment: Hidden DB coupling to MRs is the main concern. GitServerServices trait needs decomposition.

### Chain 5: Deploy (4 crates)
```
reconcile() → platform-deployer/reconciler.rs (FOR UPDATE SKIP LOCKED)
            → platform-deployer/renderer.rs (minijinja templates)
            → platform-deployer/applier.rs (K8s server-side apply)
            → ReconcilerServices trait (16 methods → main binary)
              → platform-ops-repo (CliGitRepo from platform-git)
```
Assessment: ReconcilerServices is the main problem. ops-repo crate appears orphaned from the binary.

## Recommendations

### Immediate (CRITICAL/HIGH fixes)

1. **Re-create production trait implementations** for `GitServerServices`, `ReconcilerServices`, `DynWebhookDispatcher`, `SecretsResolver`, `NotificationDispatcher`, `MergeRequestHandler`, `PostReceiveSideEffects` in the binary crate. These are blocking the restructure completion.

2. **Decompose god traits**: Split `GitServerServices` (~20 methods → 4 sub-traits) and `ReconcilerServices` (16 methods → 2-3 sub-traits). Follow the `ConcretePipelineServices` composition pattern.

3. **Extract shared CliError** into a common crate to eliminate the "keep in sync manually" duplication between platform-agent and platform-agent-runner.

4. **Add `From<DeployerError/PipelineError> for ApiError`** conversions. These were lost in the restructure.

5. **Remove 6 unused workspace deps** from platform-next's Cargo.toml to reduce compile times.

6. **Add custom Debug to configs holding master_key** to prevent secret leakage in logs.

### Near-term (MEDIUM fixes, next sprint)

1. **Move `hash_token`, `check_rate`, `generate_api_token`** to platform-types to cut heavyweight platform-auth deps from platform-git, platform-seed, platform-notify.

2. **Consolidate config types** — each domain crate owns its config; remove duplicates from platform-types::config.

3. **Split executor.rs** (8K lines) into sub-modules.

4. **Extract shared InfraClients struct** to reduce state composition boilerplate.

5. **Replace String fields** with typed enums for `severity`, `strategy`, `visibility`, `condition`, `aggregation`.

6. **Wire platform-ops-repo** into platform-deployer or archive the orphaned crate.

7. **Unify reqwest** to 0.13 across the workspace.

### Long-term (architectural improvements)

1. **Introduce newtype ID wrappers** (`UserId`, `ProjectId`, `SessionId`, etc.) in platform-types. Migrate incrementally starting with `AuthUser` and event types.

2. **Add `#[non_exhaustive]`** to `PlatformEvent`, `ApiError`, `Permission`, `ProgressKind`.

3. **Replace `anyhow::Result`** in 30+ public library functions with typed error enums.

4. **Extract shared proto/CLI protocol crate** for types duplicated between observe/proxy and agent/agent-runner.

5. **Audit `pub(crate)` discipline** — mark internal helpers appropriately as modules grow.

6. **Add `MergeRequestUpdater` trait** to make git→MR DB coupling explicit and testable.
