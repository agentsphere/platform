// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#![allow(dead_code, unused_imports)]
//! Reverse proxy for agent session preview iframes.
//!
//! Routes `/preview/{session_id}/{*path}` to the preview K8s Service
//! (`preview-{short_id}.{namespace}.svc.cluster.local:8000`) running
//! inside the agent session pod. Supports both HTTP and WebSocket (for HMR).

use std::sync::LazyLock;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::any;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

use crate::state::PlatformState;
use platform_auth::resolver;
use platform_types::{ApiError, AuthUser, Permission};

/// Shared reqwest client for preview proxying (reuses connections).
static PREVIEW_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("preview reqwest client")
});

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route("/preview/{session_id}", any(preview_proxy))
        .route("/preview/{session_id}/", any(preview_proxy))
        .route("/preview/{session_id}/{*path}", any(preview_proxy))
        .route(
            "/deploy-preview/{project_id}/{service_name}/{env}",
            any(deploy_preview_proxy),
        )
        .route(
            "/deploy-preview/{project_id}/{service_name}/{env}/",
            any(deploy_preview_proxy),
        )
        .route(
            "/deploy-preview/{project_id}/{service_name}/{env}/{*path}",
            any(deploy_preview_proxy),
        )
        // All preview responses (including errors) must allow framing by the platform UI.
        // This overrides the global X-Frame-Options: DENY set in main.rs.
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("SAMEORIGIN"),
        ))
        // Override the platform's restrictive CSP for preview iframes.
        // Project apps may load external scripts/styles (CDN, HTMX, Tailwind, etc.)
        // so the preview needs a permissive policy.
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src * 'unsafe-inline' 'unsafe-eval' data: blob:; \
                 frame-ancestors 'self'",
            ),
        ))
}

/// Validate that a namespace string is safe for URL construction.
/// Allows only lowercase alphanumeric and hyphens (`[a-z0-9-]+`).
pub fn validate_namespace_format(ns: &str) -> bool {
    !ns.is_empty()
        && ns
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Build the backend URL for a preview request.
///
/// When `proxy_base_url` is set (dev mode), routes through an nginx proxy inside the
/// Kind cluster that can resolve K8s DNS. The path encodes `{service}.{namespace}` so
/// the proxy can route to the correct backend.
pub fn build_target_url(
    svc_name: &str,
    namespace: &str,
    path: &str,
    query: Option<&str>,
    proxy_base_url: Option<&str>,
    port: Option<u16>,
) -> String {
    let port = port.unwrap_or(8000);
    let path = path.trim_start_matches('/');
    let base = match proxy_base_url {
        Some(proxy) => format!(
            "{}/{svc_name}.{namespace}.{port}/{path}",
            proxy.trim_end_matches('/')
        ),
        None => format!("http://{svc_name}.{namespace}.svc.cluster.local:{port}/{path}"),
    };
    match query {
        Some(q) if !q.is_empty() => format!("{base}?{q}"),
        _ => base,
    }
}

/// Strip sensitive headers from the outgoing request (don't leak platform auth to agent pods).
fn strip_request_headers(headers: &HeaderMap) -> HeaderMap {
    let blocked: &[HeaderName] = &[
        HeaderName::from_static("authorization"),
        HeaderName::from_static("cookie"),
        HeaderName::from_static("host"),
    ];
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if !blocked.contains(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Strip dangerous headers from the backend response before returning to browser.
fn strip_response_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let blocked: &[&str] = &[
        "set-cookie",
        "content-security-policy",
        "content-security-policy-report-only",
        "strict-transport-security",
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-allow-methods",
        "access-control-allow-headers",
        "access-control-expose-headers",
        "x-frame-options",
    ];
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if !blocked.contains(&name.as_str())
            && let Ok(v) = HeaderValue::from_bytes(value.as_bytes())
        {
            out.append(name.clone(), v);
        }
    }
    out
}

/// Resolve session details needed for proxying. Returns `(namespace, short_id)`.
async fn resolve_session(
    state: &PlatformState,
    auth: &AuthUser,
    session_id: Uuid,
) -> Result<(String, String), ApiError> {
    let session = sqlx::query!(
        r#"SELECT user_id, project_id, status, session_namespace
           FROM agent_sessions WHERE id = $1"#,
        session_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("session".into()))?;

    if session.status != "running" {
        return Err(ApiError::BadRequest("session is not running".into()));
    }

    // Auth: owner can always access; otherwise check project read
    if session.user_id != auth.user_id {
        if let Some(project_id) = session.project_id {
            let allowed = resolver::has_permission_scoped(
                &state.pool,
                &state.valkey,
                auth.user_id,
                Some(project_id),
                Permission::ProjectRead,
                auth.token_scopes.as_deref(),
            )
            .await
            .map_err(ApiError::Internal)?;
            if !allowed {
                return Err(ApiError::NotFound("session".into()));
            }
        } else {
            return Err(ApiError::NotFound("session".into()));
        }
    }

    let namespace = session
        .session_namespace
        .ok_or_else(|| ApiError::BadRequest("session has no namespace".into()))?;

    if !validate_namespace_format(&namespace) {
        return Err(ApiError::BadRequest("invalid namespace format".into()));
    }

    let short_id = session_id.to_string()[..8].to_string();
    Ok((namespace, short_id))
}

/// Check if a request is a WebSocket upgrade.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Main preview proxy handler. Handles both HTTP and WebSocket upgrade requests.
///
/// Uses `Request` to access both headers and body. WebSocket upgrades are detected
/// via the `Upgrade: websocket` header and handled via `WebSocketUpgrade`.
#[tracing::instrument(skip(state, auth, req), fields(session_id), err)]
async fn preview_proxy(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(params): Path<PreviewPath>,
    req: Request,
) -> Result<Response, ApiError> {
    let session_id = params.session_id;
    tracing::Span::current().record("session_id", tracing::field::display(session_id));
    let (namespace, short_id) = resolve_session(&state, &auth, session_id).await?;

    let svc_name = format!("preview-{short_id}");
    let path = params.path.as_deref().unwrap_or("");
    let query = req.uri().query().map(String::from);
    let target_url = build_target_url(
        &svc_name,
        &namespace,
        path,
        query.as_deref(),
        state.config.deployer.preview_proxy_url.as_deref(),
        None,
    );
    tracing::info!(
        %target_url,
        proxy_configured = state.config.deployer.preview_proxy_url.is_some(),
        "preview proxy request"
    );

    // WebSocket upgrade path (for HMR)
    if is_websocket_upgrade(req.headers()) {
        let ws_url = target_url.replacen("http://", "ws://", 1);
        let ws = WebSocketUpgrade::from_request(req, &state)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "websocket upgrade failed");
                ApiError::BadRequest("websocket upgrade failed".into())
            })?;
        return Ok(ws.on_upgrade(move |client_ws| async move {
            if let Err(e) = bridge_websocket(client_ws, &ws_url).await {
                tracing::debug!(error = %e, "websocket bridge closed");
            }
        }));
    }

    // HTTP proxy path
    proxy_http_with_base(req, &target_url, None).await
}

/// Forward an HTTP request to the backend preview service.
#[allow(dead_code)]
async fn proxy_http(req: Request, target_url: &str) -> Result<Response, ApiError> {
    proxy_http_with_base(req, target_url, None).await
}

/// Forward an HTTP request, optionally injecting a `<base>` tag into HTML responses.
///
/// When `base_href` is `Some`, HTML responses get `<base href="...">` injected after
/// `<head>` so that absolute paths (e.g. `/static/style.css`) resolve relative to the
/// proxy prefix instead of the platform root.
async fn proxy_http_with_base(
    req: Request,
    target_url: &str,
    base_href: Option<&str>,
) -> Result<Response, ApiError> {
    let method = req.method().clone();
    let forwarded_headers = strip_request_headers(req.headers());

    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|e| ApiError::BadRequest(format!("failed to read request body: {e}")))?;

    let backend_resp = PREVIEW_CLIENT
        .request(method, target_url)
        .headers(
            forwarded_headers
                .into_iter()
                .filter_map(|(k, v)| {
                    let name = k?;
                    let val = reqwest::header::HeaderValue::from_bytes(v.as_bytes()).ok()?;
                    Some((name, val))
                })
                .collect(),
        )
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "preview backend unreachable");
            ApiError::BadGateway("preview backend unreachable".into())
        })?;

    let status =
        StatusCode::from_u16(backend_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = strip_response_headers(backend_resp.headers());

    // Inject <base> tag into HTML responses for correct path resolution in iframes
    let is_html = resp_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/html"));

    if let (true, Some(prefix)) = (is_html, base_href) {
        let bytes = backend_resp
            .bytes()
            .await
            .map_err(|e| ApiError::BadGateway(format!("failed to read response: {e}")))?;
        let html = String::from_utf8_lossy(&bytes);

        // Rewrite absolute paths in HTML so they route through the proxy.
        // /static/style.css → /deploy-preview/{id}/{svc}/static/style.css
        // /cart              → /deploy-preview/{id}/{svc}/cart
        // But DON'T rewrite external URLs (https://...) or the proxy prefix itself.
        let modified = html
            .replace("href=\"/", &format!("href=\"{prefix}"))
            .replace("src=\"/", &format!("src=\"{prefix}"))
            .replace("action=\"/", &format!("action=\"{prefix}"))
            .replace("hx-get=\"/", &format!("hx-get=\"{prefix}"))
            .replace("hx-post=\"/", &format!("hx-post=\"{prefix}"))
            .replace("hx-delete=\"/", &format!("hx-delete=\"{prefix}"))
            .replace("hx-patch=\"/", &format!("hx-patch=\"{prefix}"))
            .replace("hx-put=\"/", &format!("hx-put=\"{prefix}"));

        let mut modified_headers = resp_headers.clone();
        modified_headers.remove("content-length");
        let mut response = Response::new(Body::from(modified));
        *response.status_mut() = status;
        *response.headers_mut() = modified_headers;
        return Ok(response);
    }

    let body = Body::from_stream(backend_resp.bytes_stream());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    Ok(response)
}

/// Bridge two WebSocket connections (client <-> backend) at the byte level.
async fn bridge_websocket(client_ws: WebSocket, backend_url: &str) -> Result<(), anyhow::Error> {
    use futures_util::{SinkExt, StreamExt};

    let (backend_ws, _) = tokio_tungstenite::connect_async(backend_url).await?;
    let (mut client_tx, mut client_rx) = client_ws.split();
    let (mut backend_tx, mut backend_rx) = backend_ws.split();

    // client -> backend: convert axum Message to tungstenite Message
    let mut c2b = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_rx.next().await {
            let ts_msg = axum_to_tungstenite(msg);
            if backend_tx.send(ts_msg).await.is_err() {
                break;
            }
        }
    });

    // backend -> client: convert tungstenite Message to axum Message
    let mut b2c = tokio::spawn(async move {
        while let Some(Ok(msg)) = backend_rx.next().await {
            if let Some(axum_msg) = tungstenite_to_axum(msg)
                && client_tx.send(axum_msg).await.is_err()
            {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut c2b => { b2c.abort(); },
        _ = &mut b2c => { c2b.abort(); },
    }

    Ok(())
}

/// Convert axum WebSocket message to tungstenite message.
fn axum_to_tungstenite(msg: Message) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message as TsMsg;
    match msg {
        Message::Text(text) => TsMsg::Text(text.to_string().into()),
        Message::Binary(data) => TsMsg::Binary(data),
        Message::Ping(data) => TsMsg::Ping(data),
        Message::Pong(data) => TsMsg::Pong(data),
        Message::Close(frame) => {
            TsMsg::Close(
                frame.map(|f| tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(
                        f.code,
                    ),
                    reason: f.reason.to_string().into(),
                }),
            )
        }
    }
}

/// Convert tungstenite message to axum WebSocket message.
fn tungstenite_to_axum(msg: tokio_tungstenite::tungstenite::Message) -> Option<Message> {
    use tokio_tungstenite::tungstenite::Message as TsMsg;
    match msg {
        TsMsg::Text(text) => Some(Message::Text(text.as_str().into())),
        TsMsg::Binary(data) => Some(Message::Binary(data)),
        TsMsg::Ping(data) => Some(Message::Ping(data)),
        TsMsg::Pong(data) => Some(Message::Pong(data)),
        TsMsg::Close(frame) => Some(Message::Close(frame.map(|f| {
            axum::extract::ws::CloseFrame {
                code: f.code.into(),
                reason: f.reason.as_str().into(),
            }
        }))),
        TsMsg::Frame(_) => None, // raw frames are not exposed
    }
}

/// Path parameters for preview routes.
#[derive(Debug, serde::Deserialize)]
struct PreviewPath {
    session_id: Uuid,
    #[serde(default)]
    path: Option<String>,
}

/// Path parameters for deploy preview routes.
/// Format: `/deploy-preview/{project_id}/{service_name}/{env}/{*path}`
#[derive(Debug, serde::Deserialize)]
struct DeployPreviewPath {
    project_id: Uuid,
    service_name: String,
    env: String,
    #[serde(default)]
    path: Option<String>,
}

/// Deploy preview proxy handler. Routes to K8s Services labeled
/// `platform.io/component=iframe-preview` in the project's deploy namespace.
#[tracing::instrument(skip(state, auth, req), fields(project_id, service_name), err)]
async fn deploy_preview_proxy(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(params): Path<DeployPreviewPath>,
    req: Request,
) -> Result<Response, ApiError> {
    let project_id = params.project_id;
    let service_name = &params.service_name;
    tracing::Span::current().record("project_id", tracing::field::display(project_id));
    tracing::Span::current().record("service_name", tracing::field::display(service_name));

    // Validate service_name format (reuse namespace validation — same charset)
    if !validate_namespace_format(service_name) {
        return Err(ApiError::BadRequest("invalid service name".into()));
    }

    // Look up project namespace_slug
    let project = sqlx::query!(
        r#"SELECT namespace_slug FROM projects WHERE id = $1 AND is_active = true"#,
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let slug = &project.namespace_slug;
    if slug.is_empty() {
        return Err(ApiError::BadRequest("project has no namespace".into()));
    }

    // Auth check
    super::helpers::require_project_read(&state, &auth, project_id).await?;

    // Compute deploy namespace
    let deployer_config = state.config.to_deployer_config();
    let namespace =
        platform_deployer::reconciler::target_namespace(&deployer_config, slug, &params.env);
    if !validate_namespace_format(&namespace) {
        return Err(ApiError::BadRequest("invalid namespace format".into()));
    }

    // Verify K8s Service exists with correct label
    let svc_api: kube::Api<k8s_openapi::api::core::v1::Service> =
        kube::Api::namespaced(state.kube.clone(), &namespace);
    let svc = svc_api.get(service_name).await.map_err(|e| match e {
        kube::Error::Api(ref resp) if resp.code == 404 => ApiError::NotFound("service".into()),
        other => ApiError::Internal(other.into()),
    })?;

    // Verify label
    let labels = svc.metadata.labels.as_ref();
    let has_label = labels
        .and_then(|l| l.get("platform.io/component"))
        .is_some_and(|v| v == "iframe-preview");
    if !has_label {
        return Err(ApiError::NotFound("service".into()));
    }

    // Extract iframe port (fallback 8000)
    let port = svc
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .into_iter()
        .flatten()
        .find(|p| p.name.as_deref() == Some("iframe"))
        .and_then(|p| u16::try_from(p.port).ok())
        .unwrap_or(8000);

    let path = params.path.as_deref().unwrap_or("");
    let req_query = req.uri().query().map(String::from);
    let target_url = build_target_url(
        service_name,
        &namespace,
        path,
        req_query.as_deref(),
        state.config.deployer.preview_proxy_url.as_deref(),
        Some(port),
    );
    tracing::debug!(%target_url, "deploy preview proxy request");

    // WebSocket upgrade path
    if is_websocket_upgrade(req.headers()) {
        let ws_url = target_url.replacen("http://", "ws://", 1);
        let ws = WebSocketUpgrade::from_request(req, &state)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "websocket upgrade failed");
                ApiError::BadRequest("websocket upgrade failed".into())
            })?;
        return Ok(ws.on_upgrade(move |client_ws| async move {
            if let Err(e) = bridge_websocket(client_ws, &ws_url).await {
                tracing::debug!(error = %e, "websocket bridge closed");
            }
        }));
    }

    let env = &params.env;
    let base = format!("/deploy-preview/{project_id}/{service_name}/{env}/");
    proxy_http_with_base(req, &target_url, Some(&base)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;

    // -- build_target_url --

    #[test]
    fn test_build_backend_url() {
        let url = build_target_url("preview-abc12345", "my-ns", "index.html", None, None, None);
        assert_eq!(
            url,
            "http://preview-abc12345.my-ns.svc.cluster.local:8000/index.html"
        );
    }

    #[test]
    fn test_build_backend_url_empty_path() {
        let url = build_target_url("preview-abc12345", "my-ns", "", None, None, None);
        assert_eq!(url, "http://preview-abc12345.my-ns.svc.cluster.local:8000/");
    }

    #[test]
    fn test_build_backend_url_with_query_string() {
        let url = build_target_url(
            "preview-abc12345",
            "my-ns",
            "api/data",
            Some("page=1&limit=10"),
            None,
            None,
        );
        assert_eq!(
            url,
            "http://preview-abc12345.my-ns.svc.cluster.local:8000/api/data?page=1&limit=10"
        );
    }

    #[test]
    fn test_build_backend_url_strips_leading_slash() {
        let url = build_target_url(
            "preview-abc12345",
            "my-ns",
            "/assets/app.js",
            None,
            None,
            None,
        );
        assert_eq!(
            url,
            "http://preview-abc12345.my-ns.svc.cluster.local:8000/assets/app.js"
        );
    }

    #[test]
    fn test_build_backend_url_with_proxy() {
        let url = build_target_url(
            "preview-abc12345",
            "my-ns",
            "index.html",
            None,
            Some("http://172.18.0.2:31500"),
            None,
        );
        assert_eq!(
            url,
            "http://172.18.0.2:31500/preview-abc12345.my-ns.8000/index.html"
        );
    }

    #[test]
    fn test_build_backend_url_with_proxy_trailing_slash() {
        let url = build_target_url(
            "preview-abc12345",
            "my-ns",
            "api/data",
            Some("q=1"),
            Some("http://172.18.0.2:31500/"),
            None,
        );
        assert_eq!(
            url,
            "http://172.18.0.2:31500/preview-abc12345.my-ns.8000/api/data?q=1"
        );
    }

    #[test]
    fn test_build_backend_url_with_proxy_empty_path() {
        let url = build_target_url(
            "preview-abc12345",
            "my-ns",
            "",
            None,
            Some("http://172.18.0.2:31500"),
            None,
        );
        assert_eq!(url, "http://172.18.0.2:31500/preview-abc12345.my-ns.8000/");
    }

    #[test]
    fn test_build_backend_url_with_proxy_custom_port() {
        let url = build_target_url(
            "my-app",
            "prod-ns",
            "index.html",
            None,
            Some("http://172.18.0.2:31500"),
            Some(8080),
        );
        assert_eq!(
            url,
            "http://172.18.0.2:31500/my-app.prod-ns.8080/index.html"
        );
    }

    // -- strip_request_headers --

    #[test]
    fn test_strip_request_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("cookie", "session=abc".parse().unwrap());
        headers.insert("host", "platform.local".parse().unwrap());
        headers.insert("accept", "text/html".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("x-custom", "value".parse().unwrap());

        let stripped = strip_request_headers(&headers);
        assert!(stripped.get("authorization").is_none());
        assert!(stripped.get("cookie").is_none());
        assert!(stripped.get("host").is_none());
        assert_eq!(stripped.get("accept").unwrap(), "text/html");
        assert_eq!(stripped.get("content-type").unwrap(), "application/json");
        assert_eq!(stripped.get("x-custom").unwrap(), "value");
    }

    // -- strip_response_headers --

    #[test]
    fn test_strip_response_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("set-cookie", "session=evil".parse().unwrap());
        headers.insert("content-security-policy", "default-src *".parse().unwrap());
        headers.insert("strict-transport-security", "max-age=0".parse().unwrap());
        headers.insert("access-control-allow-origin", "*".parse().unwrap());
        headers.insert("x-frame-options", "DENY".parse().unwrap());
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.insert("cache-control", "no-cache".parse().unwrap());

        let stripped = strip_response_headers(&headers);
        assert!(stripped.get("set-cookie").is_none());
        assert!(stripped.get("content-security-policy").is_none());
        assert!(stripped.get("strict-transport-security").is_none());
        assert!(stripped.get("access-control-allow-origin").is_none());
        assert!(stripped.get("x-frame-options").is_none());
        assert_eq!(stripped.get("content-type").unwrap(), "text/html");
        assert_eq!(stripped.get("cache-control").unwrap(), "no-cache");
    }

    // -- validate_namespace_format --

    #[test]
    fn test_validate_namespace_format() {
        // Valid
        assert!(validate_namespace_format("my-app-s-abc12345"));
        assert!(validate_namespace_format("test-ns"));
        assert!(validate_namespace_format("a"));
        assert!(validate_namespace_format("abc123"));

        // Invalid
        assert!(!validate_namespace_format(""));
        assert!(!validate_namespace_format("my/app"));
        assert!(!validate_namespace_format("my..app"));
        assert!(!validate_namespace_format("has space"));
        assert!(!validate_namespace_format("UPPER"));
        assert!(!validate_namespace_format("my_ns"));
    }

    // -- build_target_url with custom port --

    #[test]
    fn test_build_target_url_with_custom_port() {
        let url = build_target_url(
            "my-svc",
            "my-app-prod",
            "index.html",
            None,
            None,
            Some(3000),
        );
        assert_eq!(
            url,
            "http://my-svc.my-app-prod.svc.cluster.local:3000/index.html"
        );
    }

    #[test]
    fn test_build_target_url_default_port_none() {
        let url = build_target_url("my-svc", "my-app-prod", "index.html", None, None, None);
        assert_eq!(
            url,
            "http://my-svc.my-app-prod.svc.cluster.local:8000/index.html"
        );
    }

    // -- is_websocket_upgrade --

    #[test]
    fn test_is_websocket_upgrade_true() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "websocket".parse().unwrap());
        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn test_is_websocket_upgrade_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "WebSocket".parse().unwrap());
        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn test_is_websocket_upgrade_false_no_header() {
        let headers = HeaderMap::new();
        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn test_is_websocket_upgrade_false_wrong_value() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "h2c".parse().unwrap());
        assert!(!is_websocket_upgrade(&headers));
    }

    // -- strip_response_headers additional blocked headers --

    #[test]
    fn test_strip_response_headers_cors_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("access-control-allow-credentials", "true".parse().unwrap());
        headers.insert("access-control-allow-methods", "GET, POST".parse().unwrap());
        headers.insert(
            "access-control-allow-headers",
            "Content-Type".parse().unwrap(),
        );
        headers.insert("access-control-expose-headers", "X-Custom".parse().unwrap());
        headers.insert(
            "content-security-policy-report-only",
            "default-src 'self'".parse().unwrap(),
        );
        headers.insert("x-safe", "keep".parse().unwrap());

        let stripped = strip_response_headers(&headers);
        assert!(stripped.get("access-control-allow-credentials").is_none());
        assert!(stripped.get("access-control-allow-methods").is_none());
        assert!(stripped.get("access-control-allow-headers").is_none());
        assert!(stripped.get("access-control-expose-headers").is_none());
        assert!(
            stripped
                .get("content-security-policy-report-only")
                .is_none()
        );
        assert_eq!(stripped.get("x-safe").unwrap(), "keep");
    }

    // -- build_target_url with empty query --

    #[test]
    fn test_build_target_url_with_empty_query() {
        let url = build_target_url("my-svc", "my-ns", "path", Some(""), None, None);
        // Empty query should not append ?
        assert_eq!(url, "http://my-svc.my-ns.svc.cluster.local:8000/path");
    }

    // -- axum_to_tungstenite message types --

    #[test]
    fn test_axum_to_tungstenite_text() {
        let msg = Message::Text("hello".into());
        let ts = axum_to_tungstenite(msg);
        assert!(ts.is_text());
        assert_eq!(ts.to_text().unwrap(), "hello");
    }

    #[test]
    fn test_axum_to_tungstenite_binary() {
        let data: Bytes = vec![1u8, 2, 3].into();
        let msg = Message::Binary(data.clone());
        let ts = axum_to_tungstenite(msg);
        assert!(ts.is_binary());
    }

    #[test]
    fn test_axum_to_tungstenite_ping() {
        let data: Bytes = vec![42u8].into();
        let msg = Message::Ping(data);
        let ts = axum_to_tungstenite(msg);
        assert!(ts.is_ping());
    }

    #[test]
    fn test_axum_to_tungstenite_pong() {
        let data: Bytes = vec![0u8].into();
        let msg = Message::Pong(data);
        let ts = axum_to_tungstenite(msg);
        assert!(ts.is_pong());
    }

    #[test]
    fn test_axum_to_tungstenite_close_with_frame() {
        let msg = Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "normal".into(),
        }));
        let ts = axum_to_tungstenite(msg);
        assert!(ts.is_close());
    }

    #[test]
    fn test_axum_to_tungstenite_close_without_frame() {
        let msg = Message::Close(None);
        let ts = axum_to_tungstenite(msg);
        assert!(ts.is_close());
    }

    // -- tungstenite_to_axum message types --

    #[test]
    fn test_tungstenite_to_axum_text() {
        use tokio_tungstenite::tungstenite::Message as TsMsg;
        let ts = TsMsg::Text("world".into());
        let axum_msg = tungstenite_to_axum(ts).expect("should convert");
        match axum_msg {
            Message::Text(t) => assert_eq!(t.as_str(), "world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn test_tungstenite_to_axum_binary() {
        use tokio_tungstenite::tungstenite::Message as TsMsg;
        let data: Vec<u8> = vec![4, 5, 6];
        let ts = TsMsg::Binary(data.clone().into());
        let axum_msg = tungstenite_to_axum(ts).expect("should convert");
        match axum_msg {
            Message::Binary(d) => assert_eq!(d.as_ref(), &data),
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn test_tungstenite_to_axum_ping() {
        use tokio_tungstenite::tungstenite::Message as TsMsg;
        let data: Vec<u8> = vec![7];
        let ts = TsMsg::Ping(data.into());
        let axum_msg = tungstenite_to_axum(ts).expect("should convert");
        assert!(matches!(axum_msg, Message::Ping(_)));
    }

    #[test]
    fn test_tungstenite_to_axum_pong() {
        use tokio_tungstenite::tungstenite::Message as TsMsg;
        let data: Vec<u8> = vec![8];
        let ts = TsMsg::Pong(data.into());
        let axum_msg = tungstenite_to_axum(ts).expect("should convert");
        assert!(matches!(axum_msg, Message::Pong(_)));
    }

    #[test]
    fn test_tungstenite_to_axum_close_with_frame() {
        use tokio_tungstenite::tungstenite::Message as TsMsg;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let ts = TsMsg::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        }));
        let axum_msg = tungstenite_to_axum(ts).expect("should convert");
        match axum_msg {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.reason.as_str(), "bye");
            }
            other => panic!("expected Close with frame, got {other:?}"),
        }
    }

    #[test]
    fn test_tungstenite_to_axum_close_without_frame() {
        use tokio_tungstenite::tungstenite::Message as TsMsg;
        let ts = TsMsg::Close(None);
        let axum_msg = tungstenite_to_axum(ts).expect("should convert");
        assert!(matches!(axum_msg, Message::Close(None)));
    }
}
