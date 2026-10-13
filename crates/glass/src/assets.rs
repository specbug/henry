//! The page, its script and style, and two faces, built into the binary.
//! R Sans and R Mono (MIT, github.com/specbug/iconfig) are subset to Latin.

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;

/// Nothing but Glass's own files, and never inside another site's frame.
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; \
                      connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

fn file(kind: &'static str, cache: &'static str, body: &'static [u8]) -> impl IntoResponse + Clone {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, cache),
            (header::CONTENT_SECURITY_POLICY, POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
}

pub fn routes() -> Router {
    // The page and script change with Henry; the faces never do.
    const FRESH: &str = "no-cache";
    const FOREVER: &str = "public, max-age=31536000, immutable";
    Router::new()
        .route("/", get(|| async { file("text/html; charset=utf-8", FRESH, include_bytes!("../assets/index.html")) }))
        .route(
            "/glass.js",
            get(|| async { file("text/javascript; charset=utf-8", FRESH, include_bytes!("../assets/glass.js")) }),
        )
        .route(
            "/glass.css",
            get(|| async { file("text/css; charset=utf-8", FRESH, include_bytes!("../assets/glass.css")) }),
        )
        .route("/icon.svg", get(|| async { file("image/svg+xml", FRESH, include_bytes!("../assets/icon.svg")) }))
        .route(
            "/fonts/rsans.woff2",
            get(|| async { file("font/woff2", FOREVER, include_bytes!("../assets/fonts/rsans.woff2")) }),
        )
        .route(
            "/fonts/rmono.woff2",
            get(|| async { file("font/woff2", FOREVER, include_bytes!("../assets/fonts/rmono.woff2")) }),
        )
}
