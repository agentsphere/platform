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
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/preview/{session_id}", any(preview_proxy))
        .route("/preview/{session_id}/{*path}", any(preview_proxy))
}

/// Validate that a namespace string is safe for URL construction.
/// Allows only lowercase alphanumeric and hyphens (`[a-z0-9-]+`).
fn validate_namespace_format(ns: &str) -> bool {
    !ns.is_empty()
        && ns
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Build the backend URL for a preview request.
pub fn build_target_url(
    svc_name: &str,
    namespace: &str,
    path: &str,
    query: Option<&str>,
) -> String {
    let path = path.trim_start_matches('/');
    let base = format!("http://{svc_name}.{namespace}.svc.cluster.local:8000/{path}");
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
    state: &AppState,
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
    State(state): State<AppState>,
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
    let target_url = build_target_url(&svc_name, &namespace, path, query.as_deref());

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
    proxy_http(req, &target_url).await
}

/// Forward an HTTP request to the backend preview service.
async fn proxy_http(req: Request, target_url: &str) -> Result<Response, ApiError> {
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
    let mut resp_headers = strip_response_headers(backend_resp.headers());

    // Per-route X-Frame-Options: SAMEORIGIN (global is DENY, preview needs framing)
    resp_headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("SAMEORIGIN"),
    );

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

#[cfg(test)]
mod tests {
    use super::*;

    // -- build_target_url --

    #[test]
    fn test_build_backend_url() {
        let url = build_target_url("preview-abc12345", "my-ns", "index.html", None);
        assert_eq!(
            url,
            "http://preview-abc12345.my-ns.svc.cluster.local:8000/index.html"
        );
    }

    #[test]
    fn test_build_backend_url_empty_path() {
        let url = build_target_url("preview-abc12345", "my-ns", "", None);
        assert_eq!(url, "http://preview-abc12345.my-ns.svc.cluster.local:8000/");
    }

    #[test]
    fn test_build_backend_url_with_query_string() {
        let url = build_target_url(
            "preview-abc12345",
            "my-ns",
            "api/data",
            Some("page=1&limit=10"),
        );
        assert_eq!(
            url,
            "http://preview-abc12345.my-ns.svc.cluster.local:8000/api/data?page=1&limit=10"
        );
    }

    #[test]
    fn test_build_backend_url_strips_leading_slash() {
        let url = build_target_url("preview-abc12345", "my-ns", "/assets/app.js", None);
        assert_eq!(
            url,
            "http://preview-abc12345.my-ns.svc.cluster.local:8000/assets/app.js"
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
}
