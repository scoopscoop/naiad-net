//! The embedded web UI. `rust-embed` bakes `ui/dist` into release binaries and
//! reads it from disk in debug builds, so `naiad daemon` serves the Svelte UI by
//! default with no flags. `--ui-dir` overrides this with a live `ServeDir`.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist"]
struct UiAssets;

/// Router fallback: serve an embedded UI asset by request path, with `index.html`
/// as the SPA fallback for unmatched paths.
pub(crate) async fn embedded_ui(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    serve(path)
        .or_else(|| serve("index.html"))
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// Build a response for one embedded file, or `None` if it isn't present.
fn serve(path: &str) -> Option<Response> {
    let file = UiAssets::get(path)?;
    let mime = file.metadata.mimetype().to_string();
    Some(
        (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, cache_control(path).to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response(),
    )
}

/// Cache policy for an embedded asset. Vite emits content-hashed files under
/// `assets/`, so those are immutable and can be cached indefinitely. Everything
/// else — chiefly the `index.html` SPA shell — must be revalidated on every load:
/// the shell pins the current asset hashes, so a cached stale shell keeps pointing
/// at an old bundle (WebView2 caches aggressively, and the daemon's ephemeral port
/// can be reused), which is exactly how a rebuilt UI silently renders stale.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

#[cfg(test)]
mod tests {
    use super::cache_control;

    #[test]
    fn hashed_assets_are_immutable_shell_is_revalidated() {
        assert_eq!(
            cache_control("assets/index-BsvrqbJb.js"),
            "public, max-age=31536000, immutable"
        );
        // The shell and any other unhashed root file must never be cached stale.
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("favicon.ico"), "no-cache");
    }
}
