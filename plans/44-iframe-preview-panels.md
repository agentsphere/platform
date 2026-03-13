# Plan 44: Iframe Preview Panels for Agent Sessions

## Context

Agent sessions currently provide text-only feedback via SSE event streaming. Developers can't see visual results of UI work until the session completes and changes are deployed. This is a significant friction point for front-end development workflows where rapid visual feedback drives iteration.

The platform already supports a browser sidecar pattern (Chromium on port 9222 for "ui"/"test" roles), but that's for the *agent* to browse — not for the *human* to see live previews. This plan adds human-facing visual feedback: live iframe panels embedded in the session view, powered by dev servers (vite, webpack, etc.) running inside the agent pod.

**Current state:**
- Session pods spawn in per-session namespaces (`{slug}-s-{short_id}`)
- No K8s Services are created for session pods (only the Pod)
- NetworkPolicy blocks all ingress to session namespaces
- `X-Frame-Options: DENY` globally — iframes not allowed
- Session view: two-column layout (events + sidebar), plus a sliding AgentChatPanel
- SSE streams `ProgressEvent` with kinds: Thinking, ToolCall, ToolResult, Milestone, Error, Completed, WaitingForInput, Text
- Agent CLAUDE.md template tells apps to listen on port 8080 (for deployed apps, not dev preview)

## Design Principles

- **Zero-config default**: Every pod session gets a preview Service on port 8000 automatically. The agent just `npx vite --host 0.0.0.0 --port 8000` and the iframe appears.
- **Same-origin proxy**: All preview traffic routes through the platform (`/preview/{session_id}/...`), avoiding CORS/CSP issues and reusing cookie-based auth.
- **Multi-iframe support**: K8s informer discovers Services with port named `iframe` in session namespaces — supports monorepos with multiple UIs.
- **Hot reload native**: WebSocket proxying enables vite HMR / webpack-dev-server out of the box.
- **Ephemeral**: Iframe panels are discovered from K8s state, not persisted in DB. Panels appear/disappear as Services come and go.

---

## PR 1: Preview Service + Reverse Proxy + NetworkPolicy

Creates the infrastructure: a default K8s Service alongside every session pod, a reverse proxy endpoint on the platform, and NetworkPolicy updates to allow ingress from the platform.

- [x] Types & errors defined
- [x] Migration applied (no migration needed — K8s resources only)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [x] Integration/E2E tests passing
- [x] Quality gate passed

### No migration needed

Preview Services are K8s resources, not DB rows. The `agent_sessions.session_namespace` column (from migration `20260309010001`) already stores the namespace name needed for proxy routing. No new `sqlx::query!()` macros are introduced — no `.sqlx/` cache update needed.

### Code Changes

| File | Change |
|---|---|
| `Cargo.toml` | Add `"ws"` to axum features: `features = ["macros", "multipart", "ws"]` |
| `src/agent/claude_code/pod.rs` | Add `ContainerPort` for port 8000 named `preview` on main container; add `PREVIEW_PORT` to reserved env vars + inject |
| `src/agent/service.rs` | After pod creation, create K8s Service `preview-{short_id}` in session namespace; update reaper to delete Service on cleanup |
| `src/api/preview.rs` | **New file**: reverse proxy handler (HTTP via reqwest + WebSocket via axum ws) |
| `src/main.rs` | Merge `preview::router()` at top level (alongside git/registry); add per-route `X-Frame-Options: SAMEORIGIN` on preview routes only |
| `src/deployer/namespace.rs` | Add `build_session_network_policy()` — separate from `build_network_policy()` — with ingress rule allowing platform namespace on port 8000 |
| `ui/index.html` | Update CSP: add `frame-src 'self'` |

### Detail: Pod port + Service creation

In `src/agent/claude_code/pod.rs`, add a container port to the main container:

```rust
// In build_main_container(), add to the ports vec:
ContainerPort {
    name: Some("preview".into()),
    container_port: 8000,
    protocol: Some("TCP".into()),
    ..Default::default()
}
```

Add `PREVIEW_PORT` to reserved env vars and inject:

```rust
// In RESERVED_ENV_VARS:
"PREVIEW_PORT",

// In build_env_vars():
vars.push(env_var("PREVIEW_PORT", "8000"));
```

In `src/agent/service.rs::create_session()`, after creating the pod (around line 308), create a K8s Service:

```rust
// Create preview Service for iframe access
let svc_name = format!("preview-{short_id}");
let svc_json = serde_json::json!({
    "apiVersion": "v1",
    "kind": "Service",
    "metadata": {
        "name": svc_name,
        "namespace": &session_ns,
        "labels": {
            "platform.io/component": "iframe-preview",
            "platform.io/session": session_id.to_string(),
        }
    },
    "spec": {
        "selector": {
            "platform.io/session": session_id.to_string(),
        },
        "ports": [{
            "name": "iframe",
            "port": 8000,
            "targetPort": 8000,
            "protocol": "TCP"
        }]
    }
});
// Apply via server-side apply (same pattern as RBAC objects in deployer/namespace.rs)
```

The Service selector matches the pod's existing label `platform.io/session: {session_id}` (set at `pod.rs:105`).

**Reaper cleanup**: In `reap_terminated_sessions()` / `stop_session()`, the session namespace is deleted via `delete_namespace()`, which cascades and deletes all resources including the Service. No extra cleanup code needed.

### Detail: NetworkPolicy — session-specific function

The existing `build_network_policy()` is also used for project dev namespaces where preview ingress is NOT needed. Create a separate `build_session_network_policy()` function (or add a `allow_preview_ingress: bool` parameter). The session variant adds:

```rust
"ingress": [{
    "from": [{
        "namespaceSelector": {
            "matchLabels": {
                "kubernetes.io/metadata.name": platform_namespace
            }
        }
    }],
    "ports": [{
        "port": 8000,
        "protocol": "TCP"
    }]
}]
```

In `ensure_session_namespace()`, call `build_session_network_policy()` instead of `build_network_policy()`.

### Detail: Reverse proxy (`src/api/preview.rs`)

New module. **Port is restricted to 8000 only** (matching NetworkPolicy). This simplifies security and avoids confusing timeouts on blocked ports.

```rust
use axum::{
    extract::{Path, State, ws::WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use crate::{auth::middleware::AuthUser, error::ApiError, store::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/preview/{session_id}", any(preview_proxy))
        .route("/preview/{session_id}/{*path}", any(preview_proxy))
}
```

**Note**: Routes are mounted in `main.rs` at the top level (NOT in `api/mod.rs`), alongside `git_protocol_router()` and `registry::router()` which also use non-`/api/` prefixes. This is required because the SPA fallback catches unmatched paths — concrete routes must be merged before `.fallback()`.

```rust
// In main.rs, before .with_state(state):
.merge(api::preview::router())
```

**HTTP proxy flow:**

1. Extract `session_id`, `path` from URL (port is always 8000)
2. Authenticate via `AuthUser` (cookie-based, same-origin)
3. Look up session from DB — verify it exists and is `running`
4. Check: user owns the session OR has project read access; return **404** (not 403) if denied
5. Get `session_namespace` from session row; return 400 if NULL (cli_subprocess sessions have no namespace)
6. Validate `session_namespace` matches `[a-z0-9-]+` (defense-in-depth against SSRF)
7. Validate Service selector still matches `platform.io/session: {session_id}` (prevents agent from redirecting proxy via modified Service)
8. Build target URL: `http://preview-{short_id}.{namespace}.svc.cluster.local:8000/{path}`
9. If request has `Upgrade: websocket` header → WebSocket proxy path
10. Otherwise: forward via `reqwest` with streaming body
11. **Strip dangerous response headers** before returning: `Set-Cookie`, `Content-Security-Policy`, `Strict-Transport-Security`, `Access-Control-*`
12. Add `X-Frame-Options: SAMEORIGIN` to the response (per-route, NOT global)

**WebSocket proxy flow:**

1. Accept the WebSocket upgrade via `axum::extract::ws::WebSocketUpgrade`
2. Connect to backend via `tokio_tungstenite::connect_async(target_url)` (use the version already in the lock file via kube's `ws` feature)
3. Spawn two tasks: client→backend and backend→client
4. Both tasks run until either side closes

**Key implementation notes:**
- Strip `Authorization` and `Cookie` headers when forwarding (don't leak platform auth to agent pods)
- Set `Host` header to the target service
- Forward `X-Forwarded-For`, `X-Forwarded-Proto` headers
- Forward query string from original request
- Timeout: 30s connect, 120s read (dev servers can be slow to respond during compilation)
- Use `reqwest::Client` stored as `std::sync::LazyLock` static (reuse connections)
- Extract helpers: `build_target_url()`, `proxy_http()`, `proxy_websocket()`, `strip_request_headers()`, `strip_response_headers()` to stay under 100 lines per function (clippy `too_many_lines`)

**Auth model:**
- Session owner can always access
- Users with `ProjectRead` on the session's project can access (for team review)
- No access for unauthenticated users (iframes are same-origin, cookies sent automatically)
- `AuthUser` extractor works on `/preview/*` routes — it's a generic `FromRequestParts<AppState>` not gated by path

### Detail: Header changes

**X-Frame-Options**: Keep `DENY` globally in `main.rs`. Apply `SAMEORIGIN` only on preview responses (inside the proxy handler, not as a global layer). This preserves clickjacking protection for login, admin, and settings pages.

**CSP in `ui/index.html`**:

```html
<!-- Before: -->
content="default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws: wss:"
<!-- After: -->
content="default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' ws: wss:; frame-src 'self'"
```

### Security: iframe sandbox and same-origin risk

The iframe uses `sandbox="allow-scripts allow-same-origin allow-forms allow-popups"`. Because the preview content is proxied through the same origin, `allow-same-origin` means the iframe's JavaScript can:
- Make credentialed `fetch()` calls to `/api/*` as the user
- Access `localStorage`/`sessionStorage` of the main app

**Mitigations:**
- The session cookie MUST be `HttpOnly` (verify in `src/auth/` — prevents direct cookie theft via `document.cookie`)
- The agent pod running the dev server is controlled by the authenticated user themselves — this is an accepted trust boundary
- Dangerous response headers (`Set-Cookie`, CORS headers) are stripped by the proxy
- Future hardening: serve preview from a separate subdomain (`preview-{id}.platform.local`)

### Tests to write FIRST — PR 1

**Unit tests — `src/api/preview.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_build_backend_url` | `build_target_url("preview-abc12345", "my-ns", "index.html")` returns correct `http://...svc.cluster.local:8000/index.html` | Unit |
| `test_build_backend_url_empty_path` | Empty path produces trailing `/` | Unit |
| `test_build_backend_url_with_query_string` | Query parameters are preserved | Unit |
| `test_strip_request_headers` | Removes `Authorization` + `Cookie`, preserves `Accept`, `Content-Type`, custom headers | Unit |
| `test_strip_response_headers` | Removes `Set-Cookie`, `Content-Security-Policy`, `Strict-Transport-Security`, `Access-Control-*`; preserves `Content-Type`, `Cache-Control` | Unit |
| `test_validate_namespace_format` | `[a-z0-9-]+` passes; strings with `/`, `..`, spaces fail | Unit |

**Unit tests — `src/deployer/namespace.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_session_network_policy_ingress_allows_platform` | `build_session_network_policy("my-app", "platform")` includes ingress rule for platform namespace on port 8000 TCP | Unit |
| `test_session_network_policy_egress_unchanged` | Egress rules (platform API, DNS, internet) remain the same as `build_network_policy()` | Unit |
| `test_project_network_policy_still_denies_ingress` | Existing `build_network_policy()` still has no ingress rules (unchanged) | Unit |

**Unit tests — `src/agent/claude_code/pod.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_pod_has_preview_container_port` | `build_agent_pod()` produces pod with main container port named `preview` at 8000 | Unit |
| `test_pod_has_preview_port_env_var` | `build_agent_pod()` injects `PREVIEW_PORT=8000` | Unit |

**Integration tests — `tests/preview_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `proxy_auth_no_token_returns_401` | `GET /preview/{session_id}/` without auth → 401 | Integration |
| `proxy_session_not_found_returns_404` | `GET /preview/{random_uuid}/` with auth → 404 | Integration |
| `proxy_session_not_running_returns_400` | Session with status `stopped` → 400 | Integration |
| `proxy_session_no_namespace_returns_400` | Session with `session_namespace = NULL` → 400 | Integration |
| `proxy_owner_can_access` | Session owner → passes auth, gets 502 (no backend) | Integration |
| `proxy_project_reader_can_access` | Non-owner with `ProjectRead` → passes auth, gets 502 | Integration |
| `proxy_non_owner_non_reader_returns_404` | No access → 404 (not 403) | Integration |
| `proxy_backend_unreachable_returns_502` | Session with namespace but no Service responding → 502 | Integration |
| `session_creation_creates_preview_service` | After `create_session()`, K8s Service `preview-{short_id}` exists with correct labels and port | Integration |

**E2E tests — `tests/e2e_agent.rs`**

| Test | Validates | Layer |
|---|---|---|
| `agent_session_has_preview_service` | Full flow: create project + session → K8s Service exists with correct selector + port 8000 | E2E |

**Total: 11 unit + 9 integration + 1 E2E = 21 tests**

### Existing tests to UPDATE — PR 1

| Test file | Change | Reason |
|---|---|---|
| `src/deployer/namespace.rs` unit tests | Update `network_policy_ingress_deny_all` → still passes (project namespaces unchanged); add new test for session variant | New session-specific NetworkPolicy |
| `src/agent/claude_code/pod.rs` unit tests | Add assertions for `preview` port and `PREVIEW_PORT` env var in existing `build_pod_*` tests | Pod spec now includes preview port |
| `tests/e2e_agent.rs` | Add assertion for preview Service in existing session creation tests | Service now created alongside pod |

### Branch coverage checklist — PR 1

| Branch/Path | Test that covers it |
|---|---|
| `validate_namespace_format` — valid | `test_validate_namespace_format` |
| `validate_namespace_format` — invalid chars | `test_validate_namespace_format` |
| Proxy: no auth | `proxy_auth_no_token_returns_401` |
| Proxy: session not found | `proxy_session_not_found_returns_404` |
| Proxy: session not running | `proxy_session_not_running_returns_400` |
| Proxy: session_namespace is NULL | `proxy_session_no_namespace_returns_400` |
| Proxy: owner access OK | `proxy_owner_can_access` |
| Proxy: project reader access OK | `proxy_project_reader_can_access` |
| Proxy: non-owner/non-reader denied | `proxy_non_owner_non_reader_returns_404` |
| Proxy: backend unreachable | `proxy_backend_unreachable_returns_502` |
| Proxy: WebSocket upgrade path | Deferred to E2E (requires live TCP + pod) |
| Proxy: HTTP forward path | `proxy_owner_can_access` (hits forward, gets 502) |
| Header stripping: request headers | `test_strip_request_headers` |
| Header stripping: response headers | `test_strip_response_headers` |
| NetworkPolicy: session ingress | `test_session_network_policy_ingress_allows_platform` |
| NetworkPolicy: project still denies | `test_project_network_policy_still_denies_ingress` |
| Pod: preview port | `test_pod_has_preview_container_port` |
| Pod: PREVIEW_PORT env | `test_pod_has_preview_port_env_var` |
| Service creation | `session_creation_creates_preview_service` |

### Coverage target: 100% of touched lines — PR 1

| Code path | Covered by test | Tier |
|---|---|---|
| `preview.rs` — `build_target_url()` | `test_build_backend_url*` (3 tests) | Unit |
| `preview.rs` — `strip_request_headers()` | `test_strip_request_headers` | Unit |
| `preview.rs` — `strip_response_headers()` | `test_strip_response_headers` | Unit |
| `preview.rs` — `validate_namespace_format()` | `test_validate_namespace_format` | Unit |
| `preview.rs` — `preview_proxy()` auth | `proxy_auth_no_token_returns_401` | Integration |
| `preview.rs` — `preview_proxy()` session lookup | `proxy_session_not_found_returns_404` | Integration |
| `preview.rs` — `preview_proxy()` status check | `proxy_session_not_running_returns_400` | Integration |
| `preview.rs` — `preview_proxy()` namespace check | `proxy_session_no_namespace_returns_400` | Integration |
| `preview.rs` — `preview_proxy()` owner check | `proxy_owner_can_access` | Integration |
| `preview.rs` — `preview_proxy()` project_read | `proxy_project_reader_can_access` | Integration |
| `preview.rs` — `preview_proxy()` denied | `proxy_non_owner_non_reader_returns_404` | Integration |
| `preview.rs` — `preview_proxy()` HTTP forward + 502 | `proxy_backend_unreachable_returns_502` | Integration |
| `preview.rs` — `preview_proxy()` WebSocket path | `agent_session_has_preview_service` (E2E, or deferred) | E2E |
| `service.rs` — Service creation | `session_creation_creates_preview_service` | Integration |
| `pod.rs` — preview port + env var | `test_pod_has_preview_*` | Unit |
| `namespace.rs` — `build_session_network_policy()` | `test_session_network_policy_*` | Unit |

**Exceptions:**
- `main.rs` router wiring — covered by integration tests hitting the `/preview/` endpoint
- WebSocket bidirectional bridging — full test requires live pod; deferred to manual/E2E

---

## PR 2: K8s Service Informer + SSE Integration

Watches for Services in session namespaces and pushes iframe discovery events through the existing SSE pipeline.

- [x] Types & errors defined
- [x] Migration applied (no migration needed — K8s resources only)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [x] Integration/E2E tests passing
- [x] Quality gate passed

### No migration needed

Iframe panel state is ephemeral — discovered from K8s, pushed via Valkey pub/sub.

### Code Changes

| File | Change |
|---|---|
| `src/agent/provider.rs` | Add `IframeAvailable` and `IframeRemoved` variants to `ProgressKind` (before `Unknown`) |
| `src/agent/preview_watcher.rs` | **New file**: K8s Service informer background task |
| `src/agent/mod.rs` | Add `pub mod preview_watcher;` |
| `src/api/sessions.rs` | Add `GET /api/projects/{id}/sessions/{session_id}/iframes` endpoint + route |
| `src/main.rs` | Spawn `preview_watcher::run()` in `spawn_background_tasks()` |
| `ui/src/lib/types.ts` | Add `'IframeAvailable' | 'IframeRemoved'` to `ProgressEvent.kind` union |
| `ui/src/pages/SessionDetail.tsx` | Add `iframe_available`/`iframe_removed` to `normalizeKind()` map |
| `ui/src/components/AgentChatPanel.tsx` | Add `iframe_available`/`iframe_removed` to `normalizeKind()` map |

### Detail: ProgressKind extension

In `src/agent/provider.rs` — the enum has `#[serde(rename_all = "snake_case")]`, so `IframeAvailable` serializes as `"iframe_available"`. The existing `#[serde(other)] Unknown` catch-all means old clients gracefully degrade.

```rust
pub enum ProgressKind {
    Thinking,
    ToolCall,
    ToolResult,
    Milestone,
    Error,
    Completed,
    WaitingForInput,
    Text,
    IframeAvailable,   // NEW — service discovered
    IframeRemoved,     // NEW — service deleted
    #[serde(other)]
    Unknown,
}
```

**Pub/sub compatibility confirmed**: `pubsub_bridge::publish_event()` accepts any `ProgressEvent` without kind filtering. The persistence subscriber only checks for `Completed | Error` as terminal events. New kinds pass through and are persisted/replayed correctly.

**No exhaustive matches exist** on `ProgressKind` in the codebase — all existing match patterns use `|` subsets, not exhaustive arms. Adding variants won't break existing code.

The `ProgressEvent.metadata` for iframe events carries:

```json
{
  "service_name": "preview-abc12345",
  "port": 8000,
  "port_name": "iframe",
  "preview_url": "/preview/{session_id}/"
}
```

### Detail: K8s Service informer (`src/agent/preview_watcher.rs`)

**Important**: kube-runtime 3.x uses different event types than 0.x. The correct variants are `Event::Apply(K)`, `Event::Delete(K)`, `Event::Init`, `Event::InitApply(K)`, `Event::InitDone` — NOT `Applied`/`Deleted`/`Restarted`.

```rust
use kube::runtime::watcher;
use kube::api::Api;
use k8s_openapi::api::core::v1::Service;
use futures::TryStreamExt;

pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    loop {
        let api: Api<Service> = Api::all(state.kube.clone());
        let wc = watcher::Config::default()
            .labels("platform.io/component=iframe-preview");
        let stream = watcher(api, wc);
        tokio::pin!(stream);

        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                event = stream.try_next() => {
                    match event {
                        Ok(Some(watcher::Event::Apply(svc)))
                        | Ok(Some(watcher::Event::InitApply(svc))) => {
                            handle_service_applied(&state, &svc).await;
                        }
                        Ok(Some(watcher::Event::Delete(svc))) => {
                            handle_service_deleted(&state, &svc).await;
                        }
                        Ok(Some(watcher::Event::Init | watcher::Event::InitDone)) => {
                            // Initial list bookkeeping — no action needed
                        }
                        Ok(None) => break, // stream ended, recreate
                        Err(e) => {
                            tracing::warn!(error = %e, "preview watcher error, restarting");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            break; // break inner loop to recreate stream
                        }
                    }
                }
            }
        }
    }
}
```

**Key changes from original plan:**
- Uses correct kube 3.x event types (`Apply`/`Delete`/`InitApply`/`Init`/`InitDone`)
- `InitApply` is handled same as `Apply` — ensures existing Services are discovered on watcher startup/restart
- On error, breaks inner loop to recreate the watcher stream (outer loop recreates it)
- No unused `ListParams` variable

**`handle_service_applied()`:**
1. Extract `platform.io/session` label → `session_id`
2. For each port with name `"iframe"`:
   - Build `ProgressEvent { kind: IframeAvailable, message: "Preview available on port {port}", metadata: {...} }`
   - Publish to session's Valkey channel via `pubsub_bridge::publish_event()`

**`handle_service_deleted()`:**
1. Same label extraction
2. Publish `IframeRemoved` event

**Extract helper functions** (unit-testable):
- `extract_session_id(svc: &Service) -> Option<Uuid>` — read label
- `extract_iframe_ports(svc: &Service) -> Vec<(i32, String)>` — filter ports named `"iframe"`
- `build_iframe_event(kind: ProgressKind, service_name: &str, port: i32, session_id: Uuid) -> ProgressEvent`

### Detail: List iframes endpoint

In `src/api/sessions.rs`, add:

```rust
// GET /api/projects/{id}/sessions/{session_id}/iframes
async fn list_iframes(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<IframePanel>>, ApiError> {
    require_project_read(&state, &auth, id).await?;
    let session = service::fetch_session(&state.pool, session_id).await.map_err(ApiError::from)?;
    if session.project_id != Some(id) { return Err(ApiError::NotFound("session".into())); }

    let ns = session.session_namespace.as_deref()
        .ok_or_else(|| ApiError::NotFound("session namespace".into()))?;
    let api: kube::Api<k8s_openapi::api::core::v1::Service> =
        kube::Api::namespaced(state.kube.clone(), ns);
    let lp = kube::api::ListParams::default()
        .labels("platform.io/component=iframe-preview");
    let svcs = api.list(&lp).await.map_err(|e| ApiError::Internal(e.into()))?;

    let panels: Vec<IframePanel> = svcs.items.iter()
        .filter_map(|svc| {
            let spec = svc.spec.as_ref()?;
            let ports = spec.ports.as_ref()?;
            let name = svc.metadata.name.clone().unwrap_or_default();
            Some(ports.iter()
                .filter(|p| p.name.as_deref() == Some("iframe"))
                .map(move |p| IframePanel {
                    service_name: name.clone(),
                    port: p.port,  // i32, matching k8s-openapi
                    port_name: "iframe".into(),
                    preview_url: format!("/preview/{session_id}/"),
                })
                .collect::<Vec<_>>())
        })
        .flatten()
        .collect();

    Ok(Json(panels))
}

#[derive(Debug, serde::Serialize)]
pub struct IframePanel {
    pub service_name: String,
    pub port: i32,           // matches k8s-openapi ServicePort.port type
    pub port_name: String,
    pub preview_url: String,
}
```

**Fixes from review:**
- `IframePanel.port` is `i32` (matches `ServicePort.port`), not `u16` — avoids unsafe `as u16` cast
- Uses `filter_map` + `flatten` instead of confusing `flat_map` + `flatten` double-flatten
- Removed unused `short_id` variable

### Tests to write FIRST — PR 2

**Unit tests — `src/agent/provider.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_iframe_available_serializes_snake_case` | `serde_json::to_string(&ProgressKind::IframeAvailable)` → `"iframe_available"` | Unit |
| `test_iframe_removed_serializes_snake_case` | `serde_json::to_string(&ProgressKind::IframeRemoved)` → `"iframe_removed"` | Unit |
| `test_iframe_available_roundtrip` | Serialize → deserialize produces same value | Unit |
| `test_iframe_removed_roundtrip` | Serialize → deserialize produces same value | Unit |
| `test_progress_event_iframe_with_metadata` | Full `ProgressEvent` with `IframeAvailable` + metadata serializes correctly | Unit |

**Unit tests — `src/agent/preview_watcher.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_extract_session_id_from_labels` | Service with `platform.io/session: {uuid}` → `Some(uuid)` | Unit |
| `test_extract_session_id_missing_label` | Service without label → `None` | Unit |
| `test_extract_iframe_ports` | Service with ports `[{name: "iframe", port: 8000}, {name: "http", port: 80}]` → `[(8000, "iframe")]` | Unit |
| `test_extract_iframe_ports_none` | Service with no `iframe` port → empty vec | Unit |
| `test_build_iframe_event` | Produces correct `ProgressEvent` with metadata | Unit |

**Integration tests — `tests/preview_integration.rs` (append to PR 1 file)**

| Test | Validates | Layer |
|---|---|---|
| `list_iframes_returns_panels` | Create K8s namespace + Service with iframe label, insert session with that namespace, GET → panels returned | Integration |
| `list_iframes_empty_no_services` | Session with namespace but no Services → empty array | Integration |
| `list_iframes_requires_project_read` | Non-owner without ProjectRead → 404 | Integration |
| `list_iframes_no_namespace_returns_404` | Session with `session_namespace = NULL` → 404 | Integration |
| `list_iframes_wrong_project_returns_404` | Session in project A, request to project B → 404 | Integration |

**E2E tests — `tests/e2e_agent.rs` (append)**

| Test | Validates | Layer |
|---|---|---|
| `agent_session_iframes_endpoint` | Create session → GET iframes → at least one panel with port 8000 | E2E |

**Total: 10 unit + 5 integration + 1 E2E = 16 tests**

### Existing tests to UPDATE — PR 2

| Test file | Change | Reason |
|---|---|---|
| `src/agent/provider.rs` existing tests | No changes — existing `ProgressKind` tests unaffected | New variants added, `#[serde(other)]` unchanged |
| `ui/src/pages/SessionDetail.tsx` | Add to `normalizeKind()` map: `iframe_available: 'IframeAvailable', iframe_removed: 'IframeRemoved'` | Without this, new events render as plain text |
| `ui/src/components/AgentChatPanel.tsx` | Same `normalizeKind()` update | Same reason |

### Branch coverage checklist — PR 2

| Branch/Path | Test that covers it |
|---|---|
| `IframeAvailable` serialization | `test_iframe_available_serializes_snake_case` |
| `IframeRemoved` serialization | `test_iframe_removed_serializes_snake_case` |
| `extract_session_id` — present | `test_extract_session_id_from_labels` |
| `extract_session_id` — missing | `test_extract_session_id_missing_label` |
| `extract_iframe_ports` — has iframe | `test_extract_iframe_ports` |
| `extract_iframe_ports` — no iframe | `test_extract_iframe_ports_none` |
| `list_iframes` — services found | `list_iframes_returns_panels` |
| `list_iframes` — no services | `list_iframes_empty_no_services` |
| `list_iframes` — auth denied | `list_iframes_requires_project_read` |
| `list_iframes` — no namespace | `list_iframes_no_namespace_returns_404` |
| `list_iframes` — wrong project | `list_iframes_wrong_project_returns_404` |
| Watcher `Apply` event | Unit-tested via `build_iframe_event` helper |
| Watcher `Delete` event | Unit-tested via `build_iframe_event` with `IframeRemoved` |
| Watcher error recovery | Manual verification (watcher logs + reconnects) |

---

## PR 3: Frontend — Session View Redesign + Iframe Panels

Transforms the session view from a single-purpose event stream into a multi-panel workspace with live iframe previews.

- [x] Types & errors defined (no new types — IframePanel already in types.ts from PR 2)
- [x] Migration applied (no migration — UI only)
- [x] Tests written (red phase) (UI — manual/visual testing only)
- [x] Implementation complete (green phase)
- [x] Integration/E2E tests passing
- [x] Quality gate passed

> **Deviation:** Skipped `SessionSelector.tsx` — session bar is simple enough to inline in `SessionDetail.tsx`. Skipped separate `api.ts` helper — inline `api.get<IframePanel[]>()` is simpler for a single call.

### Code Changes

| File | Change |
|---|---|
| `ui/src/pages/SessionDetail.tsx` | Redesign layout: session bar + multi-panel workspace; SSE iframe events trigger iframe list refresh |
| `ui/src/components/IframePanel.tsx` | **New file**: Iframe preview panel component with tab switching, refresh, open-in-tab |
| `ui/src/style.css` | Add workspace layout, iframe panel, session bar, responsive styles |

### Detail: Session view layout redesign

Current layout:
```
┌─────────────────────────────────┬──────────┐
│  Events (scrollable)            │ Sidebar  │
│  ─────────────────────          │ Info     │
│  Input field                    │          │
└─────────────────────────────────┴──────────┘
```

New layout:
```
┌────────────────────────────────────────────────────────┐
│  Session: abc12345 ▾  │  Preview (1)  │  ● Running    │  ← Session bar
├────────────────────────┬───────────────────────────────┤
│                        │                               │
│  Agent Events          │  Iframe Preview               │
│  (scrollable)          │  ┌───────────────────────┐    │
│                        │  │                       │    │
│  [T] Thinking...       │  │  (live dev server)    │    │
│  [>] Edit file.tsx     │  │                       │    │
│  [<] Done              │  │                       │    │
│  [+] UI updated        │  │                       │    │
│                        │  └───────────────────────┘    │
│  ─────────────────     │                               │
│  > Send message...     │  [Refresh] [Open in tab]      │
└────────────────────────┴───────────────────────────────┘
```

- When no iframes: events panel takes full width (current behavior)
- CSS Grid: `grid-template-rows: auto 1fr; grid-template-columns: 1fr 1fr;`
- Responsive: stacks vertically on mobile (`@media max-width: 768px`)

### Detail: IframePanel component

```tsx
// ui/src/components/IframePanel.tsx
import { h } from 'preact';
import { useState, useRef } from 'preact/hooks';

interface Props {
  panels: IframePanel[];
}

export function IframePreview({ panels }: Props) {
  const [activeTab, setActiveTab] = useState(0);
  const iframeRef = useRef<HTMLIFrameElement>(null);

  if (panels.length === 0) return null;

  const active = panels[activeTab] || panels[0];

  const refreshIframe = () => {
    if (iframeRef.current) {
      iframeRef.current.src = active.preview_url;
    }
  };

  return (
    <div class="session-preview-panel">
      {panels.length > 1 && (
        <div class="preview-tabs">
          {panels.map((p, i) => (
            <button
              class={`preview-tab ${i === activeTab ? 'active' : ''}`}
              onClick={() => setActiveTab(i)}
            >
              :{p.port}
            </button>
          ))}
        </div>
      )}
      <div class="preview-toolbar">
        <button class="btn btn-sm" onClick={refreshIframe}>Refresh</button>
        <a class="btn btn-sm" href={active.preview_url} target="_blank">Open in tab</a>
      </div>
      <iframe
        ref={iframeRef}
        class="preview-iframe"
        src={active.preview_url}
        sandbox="allow-scripts allow-same-origin allow-forms allow-popups"
      />
    </div>
  );
}
```

### Detail: SessionDetail.tsx changes

Key changes:
1. Update `normalizeKind()` to map `iframe_available` → `'IframeAvailable'` and `iframe_removed` → `'IframeRemoved'`
2. On SSE event with kind `iframe_available` or `iframe_removed`: refresh iframe list via `GET .../iframes`
3. Switch from `session-layout` grid to `session-workspace` grid
4. Conditionally show preview panel when `iframes.length > 0`

### Test Outline — PR 3

No automated backend tests. All changes are UI (Preact components, CSS, types). Validation is manual/visual:
- Iframe panel renders when iframes available
- Iframe panel hidden when no iframes (full-width events)
- Multiple iframe tabs switch correctly
- SSE iframe events trigger iframe list refresh
- Responsive layout stacks vertically on mobile
- Session bar shows correct status/branch/preview count

---

## PR 4: Agent Instructions + Templates

Updates the agent CLAUDE.md template and adds preview-specific setup instructions.

- [x] Types & errors defined (N/A — no new types)
- [x] Migration applied (N/A — no migration)
- [x] Tests written (red phase)
- [x] Implementation complete (green phase)
- [x] Integration/E2E tests passing (N/A — template/prompt only)
- [x] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/git/templates/CLAUDE.md` | Add "Visual Preview" section with dev server instructions |
| `src/agent/create_app_prompt.rs` | Add preview port instruction to coding agent prompt |

### Detail: CLAUDE.md template addition

Add new section after "Application Requirements":

```markdown
## Visual Preview (Dev Server)

The platform provides a live preview iframe in the session view. To use it:

1. **Start a dev server on port 8000**, binding to all interfaces:

   **Vite (React/Vue/Svelte/Preact):**
   ```bash
   npx vite --host 0.0.0.0 --port 8000 --base './'
   ```

   **Next.js:**
   ```bash
   npx next dev -H 0.0.0.0 -p 8000
   ```

   **Webpack Dev Server:**
   ```bash
   npx webpack serve --host 0.0.0.0 --port 8000 --public-path './'
   ```

   **Python (static files):**
   ```bash
   python3 -m http.server 8000 --bind 0.0.0.0
   ```

2. **Use relative base paths** (`base: './'` for vite, `publicPath: './'` for webpack). This ensures assets load correctly through the platform proxy.

3. **Port 8000 is reserved** for preview. The `PREVIEW_PORT` env var is set to `8000`.

4. The preview automatically appears in the session view once the dev server starts responding.

5. Hot Module Replacement (HMR) works automatically — the platform proxies WebSocket connections.

6. **Additional preview ports**: To expose more UIs (monorepo), create K8s Services in the session namespace with label `platform.io/component: iframe-preview` and a port named `iframe`. They will be auto-discovered.
```

### Detail: create_app_prompt.rs addition

Add to the coding agent requirements:

```
- Start a dev server on port 8000 (use `PREVIEW_PORT` env var) with `--host 0.0.0.0` and relative base path
- The dev server should run in the background while you continue working
```

### Tests to write FIRST — PR 4

**Unit tests — `src/git/templates.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_template_claude_md_has_visual_preview_section` | Template contains "Visual Preview" and "port 8000" and "PREVIEW_PORT" | Unit |
| `test_template_claude_md_has_vite_instructions` | Template contains `--host 0.0.0.0` and `--port 8000` | Unit |
| `test_template_claude_md_has_relative_base` | Template contains `base: './'` | Unit |

**Unit tests — `src/agent/create_app_prompt.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_system_prompt_mentions_preview_port` | System prompt contains "port 8000" or "PREVIEW_PORT" | Unit |
| `test_system_prompt_mentions_dev_server` | System prompt contains "dev server" | Unit |

**Total: 5 unit tests**

---

## Cross-Cutting Concerns

### Security

| Concern | Addressed |
|---|---|
| Auth on proxy | `AuthUser` extractor — session owner or project reader |
| No auth leakage | Strip `Authorization` + `Cookie` headers when forwarding to pod |
| Response header sanitization | Strip `Set-Cookie`, `CSP`, `CORS`, `HSTS` from proxied responses |
| Port restriction | Hardcoded to 8000 (matches NetworkPolicy) |
| NetworkPolicy | Session namespaces: ingress from platform namespace on port 8000 only; project namespaces: unchanged (deny-all) |
| Iframe sandbox | `allow-scripts allow-same-origin allow-forms allow-popups` |
| CSP | `frame-src 'self'` — only same-origin iframes |
| X-Frame-Options | `DENY` globally; `SAMEORIGIN` only on preview responses (per-route) |
| SSRF | Proxy connects only to `*.svc.cluster.local`; namespace format validated with `[a-z0-9-]+` |
| Service selector validation | Verify Service selector matches `platform.io/session: {session_id}` before proxying |
| Private resources | Return 404 (not 403) for inaccessible sessions |

### Security notes for implementation

- **HttpOnly cookies**: Verify the session cookie has `HttpOnly` flag in `src/auth/`. The `allow-same-origin` sandbox gives iframe JS access to non-HttpOnly cookies.
- **Agent trust boundary**: The dev server content is controlled by the authenticated user's agent — this is an accepted trust boundary (user trusts their own agent).
- **Future hardening**: Serve preview from a separate subdomain to fully isolate cookie/localStorage access.
- **Cluster RBAC**: The platform's ServiceAccount needs `list`/`watch` permissions on Services cluster-wide for the informer. Verify or update `deploy/base/rbac.yaml`.

### Observability

- `preview_proxy` handler instrumented with `tracing::instrument`
- Preview watcher logs Service discovery/removal events at INFO level
- Proxy errors logged with session_id (no sensitive data in logs)
- Audit: each proxy request emits structured log with `session_id`, `user_id`, `path` (no query params)

### AppState impact

No changes to `AppState`. The preview watcher uses the existing `kube` client and `valkey` pool.

### Gotchas

- **kube-runtime 3.x event types**: Use `Event::Apply`/`Event::Delete`/`Event::InitApply`/`Event::Init`/`Event::InitDone` — NOT the old `Applied`/`Deleted`/`Restarted` names
- **axum `ws` feature**: Must be added to `Cargo.toml` for `WebSocketUpgrade` and `any()` routing
- **tokio-tungstenite version**: Use version `0.28` (already in lock file via kube's `ws` feature), not `0.26`
- **reqwest streaming**: Use `.bytes_stream()` for streaming response bodies through the proxy
- **`Patch::Apply` for Service**: Same server-side apply pattern used for RBAC objects in `deployer/namespace.rs`
- **Clippy `too_many_lines`**: Proxy handler must extract helpers (`build_target_url()`, `proxy_http()`, `proxy_websocket()`, `strip_request_headers()`, `strip_response_headers()`)
- **`ServicePort.port` is `i32`**: Use `i32` in `IframePanel`, not `u16` — avoids unsafe cast

### Dependency changes

| Crate | Change | Purpose |
|---|---|---|
| `axum` | Add `"ws"` feature | WebSocket upgrade support + `any()` routing |
| `tokio-tungstenite` | Already in lock file via kube `ws` — no explicit dep needed unless client-side `connect_async()` requires it; if so, use version `0.28` | WebSocket client for proxy backend |

---

## Test Plan Summary

### Coverage target: 100% of touched lines

Every new or modified line of code must be covered by at least one test (unit, integration, or E2E). The test strategy above maps each code path to a specific test. `review` and `finalize` will verify with `just cov-unit` / `just cov-total`.

### New test counts by PR

| PR | Unit | Integration | E2E | Total |
|---|---|---|---|---|
| PR 1 | 11 | 9 | 1 | 21 |
| PR 2 | 10 | 5 | 1 | 16 |
| PR 3 | 0 | 0 | 0 | 0 |
| PR 4 | 5 | 0 | 0 | 5 |
| **Total** | **26** | **14** | **2** | **42** |

### Coverage goals by module

| Module | Current tests | After plan |
|---|---|---|
| `src/api/preview.rs` | 0 (new file) | +6 unit + 9 integration |
| `src/agent/preview_watcher.rs` | 0 (new file) | +5 unit |
| `src/agent/provider.rs` | existing | +5 unit (new variants) |
| `src/deployer/namespace.rs` | existing | +3 unit (session policy) |
| `src/agent/claude_code/pod.rs` | existing | +2 unit (port + env var) |
| `src/api/sessions.rs` | existing | +5 integration (iframes) |
| `src/git/templates.rs` | existing | +3 unit (template content) |
| `src/agent/create_app_prompt.rs` | existing | +2 unit (prompt content) |

---

## Verification

After all 4 PRs are merged, the following end-to-end scenario should work:

1. Create an agent session with prompt: "Create a simple React app and start the dev server on port 8000"
2. The platform creates a pod + preview Service in the session namespace
3. The K8s informer detects the preview Service → publishes `IframeAvailable` via SSE
4. The session view shows an iframe panel alongside the event stream
5. As the agent codes, the iframe shows live updates via vite HMR
6. When the agent creates additional Services with `iframe` ports, new tabs appear
7. The preview stops when the session ends (namespace cleanup deletes the Service)

---

## Plan Review Findings

**Date:** 2026-03-13
**Status:** APPROVED WITH CONCERNS

### Codebase Reality Check

Issues found and corrected in-place above:

1. **kube-runtime 3.x API**: Plan originally used `Event::Applied`/`Deleted`/`Restarted` — corrected to `Apply`/`Delete`/`Init`/`InitApply`/`InitDone`
2. **axum `ws` feature missing**: Plan claimed WebSocket support was already available — corrected to require adding `"ws"` feature
3. **tokio-tungstenite version**: Plan specified 0.26, lock file has 0.28 — corrected
4. **`IframePanel.port: u16`**: k8s-openapi `ServicePort.port` is `i32` — corrected to `i32`
5. **Double flatten in `list_iframes`**: Confusing `flat_map` + `flatten` — corrected to `filter_map` + `flatten`
6. **Unused variables**: `short_id` and `lp` in code snippets — removed
7. **X-Frame-Options global change**: Plan proposed changing `DENY` to `SAMEORIGIN` globally — corrected to per-route on preview responses only
8. **NetworkPolicy scope**: `build_network_policy()` used for both project and session namespaces — corrected to create separate `build_session_network_policy()`
9. **Router placement**: Plan merged preview router in `api/mod.rs` — corrected to merge in `main.rs` alongside git/registry routers
10. **UI `normalizeKind()` maps**: Plan didn't update these — corrected to include in PR 2 and PR 3 code changes
11. **Session reaper cleanup**: Plan didn't address Service cleanup — confirmed namespace deletion cascades

### Remaining Concerns

1. **`allow-same-origin` iframe sandbox**: Same-origin proxy means iframe JS can make credentialed API calls as the user. This is an accepted trust boundary (user's own agent), but future hardening should use a separate subdomain. Verify session cookie has `HttpOnly`.
2. **Cluster-wide Service watch**: The informer uses `Api::all()` — requires the platform's ServiceAccount to have `list`/`watch` permissions on Services cluster-wide. Verify `deploy/base/rbac.yaml` and add a ClusterRole if needed.
3. **Agent can modify Service selector**: The agent's RBAC allows `services: [*]` — it could redirect the preview Service to a different pod. The plan adds Service selector validation in the proxy handler to mitigate.
4. **WebSocket proxy complexity**: Full bidirectional WebSocket bridging is non-trivial. If it proves too complex for PR 1, it can be deferred to a follow-up PR without blocking the core feature (HTTP proxy + manual refresh works for static content).

### Simplification Opportunities

1. **Port restriction to 8000 only**: Original plan allowed 1024-65535 but NetworkPolicy only opens 8000. Simplified to hardcode 8000 — removes validation complexity and eliminates the port mismatch confusion.
2. **No `tokio-tungstenite` direct dep**: If axum's `ws` feature provides `WebSocketUpgrade` and the backend WebSocket connection can use `tokio-tungstenite` through kube's transitive dep, no explicit Cargo.toml addition needed. Test during implementation.

### Security Notes

- Response header stripping is critical — a malicious dev server could override `Set-Cookie` to hijack the session
- SSRF defense-in-depth: validate namespace format `[a-z0-9-]+` before URL construction
- The proxy creates a "pass-through" for arbitrary content from the agent pod — log all proxy requests for audit trail
