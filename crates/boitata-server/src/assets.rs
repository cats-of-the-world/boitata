//! Serving the embedded web UI.
//!
//! The React app is built to `frontend/dist` and baked into the binary with
//! `rust-embed`, so the server ships as a single self-contained executable. When
//! the UI hasn't been built yet, the API still works and the root returns a hint.

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// Serve an embedded asset by path, falling back to `index.html` so client-side
/// routes resolve (SPA behaviour). API routes are handled before this fallback.
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        return serve(path, content);
    }
    // Unknown path: hand back the SPA shell if we have one, else a build hint.
    match Assets::get("index.html") {
        Some(content) => serve("index.html", content),
        None => (
            StatusCode::NOT_FOUND,
            "boitata-server: web UI not built. Run `npm install && npm run build` \
             in crates/boitata-server/frontend, then rebuild the server.",
        )
            .into_response(),
    }
}

fn serve(path: &str, content: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(header::CONTENT_TYPE, mime.as_ref())],
        Body::from(content.data.into_owned()),
    )
        .into_response()
}
