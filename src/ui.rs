use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "ui/dist/"]
struct UiAssets;

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("html") => "text/html",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
}

fn serve(uri: &Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Serve exact file match
    if !path.is_empty()
        && let Some(file) = UiAssets::get(path)
    {
        let cache = if path == "index.html" {
            "no-cache"
        } else {
            "public, max-age=86400"
        };
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime_for(path)),
                (header::CACHE_CONTROL, cache),
            ],
            file.data.into_owned(),
        )
            .into_response();
    }

    // SPA fallback: serve index.html for client-side routing
    match UiAssets::get("index.html") {
        Some(file) => (
            [(header::CACHE_CONTROL, "no-cache")],
            Html(file.data.into_owned()),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Axum requires an async handler for fallback
#[allow(clippy::unused_async)]
pub async fn static_handler(uri: Uri) -> Response {
    serve(&uri)
}
