//! Static dashboard serving, mirroring `server/dashboard_static.py`:
//! page routes (with/without trailing slash + `.txt` RSC payload + `/index.txt`),
//! asset directory mounts, security headers, and the 404 fallback.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::Router;
use tower_http::services::ServeDir;

use super::{embedded, AppState};

pub const PAGE_ROUTES: &[&str] = &[
    "",
    "login",
    "setup",
    "dashboard/analytics",
    "dashboard/api-keys",
    "dashboard/categories",
    "dashboard/configuration",
    "dashboard/entities",
    "dashboard/export",
    "dashboard/logs",
    "dashboard/memories",
    "dashboard/requests",
    "dashboard/settings",
    "404",
];

pub const ASSET_DIRS: &[&str] = &["_next"]; // fonts/images/icons live under _next/static in this export

/// Resolve the dashboard export directory: `MEM0_DASHBOARD_DIR` wins, else
/// `<data_dir>/../dashboard/out` next to the server checkout, else bundled.
fn dashboard_dir(state: &AppState) -> Option<PathBuf> {
    if let Ok(d) = std::env::var("MEM0_DASHBOARD_DIR") {
        if !d.is_empty() {
            let p = PathBuf::from(d);
            if p.join("index.html").is_file() {
                return Some(p);
            }
        }
    }
    // source checkout: server/dashboard/out relative to the crate
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    let mut candidates = vec![
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../server/dashboard/out"),
        state.data_dir.join("dashboard/out"),
        state.data_dir.join("out"),
    ];
    if let Some(d) = exe_dir {
        candidates.push(d.join("dashboard/out"));
        candidates.push(d.join("out"));
    }
    candidates
        .into_iter()
        .find(|p| p.join("index.html").is_file())
}

fn security_headers(response: &mut Response) {
    let h = response.headers_mut();
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
}

/// Add security headers to every response (layer applied in build_router).
pub async fn security_headers_middleware(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    security_headers(&mut response);
    response
}

async fn serve_page(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(page): axum::extract::Path<String>,
) -> Response {
    let dir = dashboard_dir(&state);
    if dir.is_none() {
        // No on-disk export (packaged exe): serve from the embedded copy.
        return serve_page_embedded(&page);
    }
    let dir = dir.unwrap();
    let file = if page.is_empty() {
        dir.join("index.html")
    } else {
        let as_dir = dir.join(&page).join("index.html");
        if as_dir.is_file() {
            as_dir
        } else {
            dir.join(format!("{page}.html"))
        }
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, Body::from(bytes)).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp
        }
        Err(_) => {
            // 404 page fallback, mirroring _not_found_response
            let nf = dir.join("404.html");
            match tokio::fs::read(&nf).await {
                Ok(bytes) => (StatusCode::NOT_FOUND, Body::from(bytes)).into_response(),
                Err(_) => (StatusCode::NOT_FOUND, Html("Not found")).into_response(),
            }
        }
    }
}

/// Serve a `.txt` RSC payload (Next client-side navigation) — same content as the page
/// for our purposes; Python serves the raw .txt file from the export.
async fn serve_payload(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(page): axum::extract::Path<String>,
) -> Response {
    let dir = dashboard_dir(&state);
    if dir.is_none() {
        return serve_payload_embedded(&page);
    }
    let dir = dir.unwrap();
    let file = {
        let as_dir = dir.join(&page).join("index.txt");
        if as_dir.is_file() {
            as_dir
        } else {
            dir.join(format!("{page}.txt"))
        }
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, Body::from(bytes)).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/x-component"),
            );
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

/// Build static routes bound to the real state. Called from build_router.
pub fn static_routes(state: &AppState) -> Router<Arc<AppState>> {
    let mut r: Router<Arc<AppState>> = Router::new();
    let dir = dashboard_dir(state);
    let Some(dir) = dir else {
        // Packaged exe with no checkout on disk: serve the embedded dashboard.
        return embedded_routes();
    };
    // asset mounts
    for d in ASSET_DIRS {
        let asset = dir.join(d);
        if asset.is_dir() {
            r = r.nest_service(&format!("/{d}"), ServeDir::new(asset));
        }
    }
    // page routes: /page, /page/, /page.txt, /page/index.txt
    for page in PAGE_ROUTES {
        let base = if page.is_empty() {
            "/".to_string()
        } else {
            format!("/{page}")
        };
        let p1 = page.to_string();
        let p1 = if page.is_empty() {
            String::new()
        } else {
            p1.clone()
        };
        r = r.route(
            &base,
            axum::routing::get(move |State(s): State<Arc<AppState>>| async move {
                serve_page(State(s), axum::extract::Path(p1.clone())).await
            }),
        );
        if page.is_empty() {
            r = r.route(
                "/index.txt",
                axum::routing::get(|State(s): State<Arc<AppState>>| async move {
                    serve_payload(State(s), axum::extract::Path(String::new())).await
                }),
            );
        } else {
            let p2 = format!("{base}/");
            let p1b = page.to_string();
            r = r.route(
                &p2,
                axum::routing::get(move |State(s): State<Arc<AppState>>| async move {
                    serve_page(State(s), axum::extract::Path(p1b.clone())).await
                }),
            );
            let p3 = format!("{base}.txt");
            let p1c = page.to_string();
            r = r.route(
                &p3,
                axum::routing::get(move |State(s): State<Arc<AppState>>| async move {
                    serve_payload(State(s), axum::extract::Path(p1c.clone())).await
                }),
            );
            let p4 = format!("{base}/index.txt");
            let p1d = page.to_string();
            r = r.route(
                &p4,
                axum::routing::get(move |State(s): State<Arc<AppState>>| async move {
                    serve_payload(State(s), axum::extract::Path(p1d.clone())).await
                }),
            );
        }
    }
    // catch-all: serve real files from the export root; anything else gets 404.html with a
    // 404 status (matching Python's _not_found_response).
    let dir2 = dir.clone();
    r = r.fallback_service(
        ServeDir::new(dir.clone()).not_found_service(axum::routing::get(move || {
            let dir3 = dir2.clone();
            async move {
                let nf = dir3.join("404.html");
                match tokio::fs::read(&nf).await {
                    Ok(bytes) => (StatusCode::NOT_FOUND, Body::from(bytes)).into_response(),
                    Err(_) => (StatusCode::NOT_FOUND, Html("Not found")).into_response(),
                }
            }
        })),
    );
    r
}

// ---------------------------------------------------------------------------
// Embedded dashboard (packaged-exe fallback)
// ---------------------------------------------------------------------------

fn embedded_bytes_for_page(page: &str) -> Option<String> {
    if page.is_empty() {
        Some("index.html".to_string())
    } else if embedded::get(&format!("{page}/index.html")).is_some() {
        Some(format!("{page}/index.html"))
    } else if embedded::get(&format!("{page}.html")).is_some() {
        Some(format!("{page}.html"))
    } else {
        None
    }
}

fn serve_page_embedded(page: &str) -> Response {
    let Some(file) = embedded_bytes_for_page(page) else {
        return match embedded::get("404.html") {
            Some(bytes) => (StatusCode::NOT_FOUND, Body::from(bytes.to_vec())).into_response(),
            None => (StatusCode::NOT_FOUND, Html("Not found")).into_response(),
        };
    };
    match embedded::get(&file) {
        Some(bytes) => {
            let mut resp = (StatusCode::OK, Body::from(bytes.to_vec())).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp
        }
        None => (StatusCode::NOT_FOUND, Html("Not found")).into_response(),
    }
}

fn serve_payload_embedded(page: &str) -> Response {
    let file = if page.is_empty() {
        "index.txt".to_string()
    } else if embedded::get(&format!("{page}/index.txt")).is_some() {
        format!("{page}/index.txt")
    } else {
        format!("{page}.txt")
    };
    match embedded::get(&file) {
        Some(bytes) => {
            let mut resp = (StatusCode::OK, Body::from(bytes.to_vec())).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/x-component"),
            );
            resp
        }
        None => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

/// Serve an asset path (`_next/...`, `fonts/...`, etc.) from the embedded export.
fn serve_asset_embedded(path: &str) -> Response {
    match embedded::get(path) {
        Some(bytes) => {
            let mut resp = (StatusCode::OK, Body::from(bytes.to_vec())).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(embedded::mime(path)),
            );
            resp
        }
        None => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

/// Routes used when no on-disk dashboard exists: same surface as the disk-backed
/// variant, but every byte comes from the binary-embedded export.
pub fn embedded_routes() -> Router<Arc<AppState>> {
    let mut r: Router<Arc<AppState>> = Router::new();
    for page in PAGE_ROUTES {
        let base = if page.is_empty() {
            "/".to_string()
        } else {
            format!("/{page}")
        };
        // None of the page routes carry a path parameter ("/setup" is a static
        // segment, not a capture), so handlers must NOT use a Path extractor —
        // that 500s with "Expected 1 but got 0". Capture the page name in the
        // closure instead, exactly like the disk-backed variant.
        let page_owned = page.to_string();
        r = r.route(
            &base,
            axum::routing::get(move || {
                let p = page_owned.clone();
                async move { serve_page_embedded(&p) }
            }),
        );
        if page.is_empty() {
            r = r.route(
                "/index.txt",
                axum::routing::get(|| async move { serve_payload_embedded("") }),
            );
        } else {
            let p2 = page.to_string();
            r = r.route(
                &format!("{base}/"),
                axum::routing::get(move || {
                    let p = p2.clone();
                    async move { serve_page_embedded(&p) }
                }),
            );
            let p3 = page.to_string();
            r = r.route(
                &format!("{base}.txt"),
                axum::routing::get(move || {
                    let p = p3.clone();
                    async move { serve_payload_embedded(&p) }
                }),
            );
            let p4 = page.to_string();
            r = r.route(
                &format!("{base}/index.txt"),
                axum::routing::get(move || {
                    let p = p4.clone();
                    async move { serve_payload_embedded(&p) }
                }),
            );
        }
    }
    // asset dirs + catch-all from embedded
    for d in ASSET_DIRS {
        let prefix = d.to_string();
        r = r.route(
            &format!("/{d}/{{*path}}"),
            axum::routing::get(
                move |axum::extract::Path(wild): axum::extract::Path<String>,
                      uri: axum::http::Uri| {
                    // Probe showed axum 0.8's Path<String> wildcard capture keeps the
                    // RAW (still percent-encoded) segment, while Uri.path() is also
                    // raw. Next asset names are [A-Za-z0-9._-], so decode rarely
                    // matters; use the raw uri path minus the prefix to be safe.
                    let raw = uri.path().trim_start_matches('/');
                    let full = raw
                        .strip_prefix(&format!("{prefix}/"))
                        .map(|rest| format!("{prefix}/{rest}"))
                        .unwrap_or_else(|| format!("{prefix}/{wild}"));
                    async move { serve_asset_embedded(&full) }
                },
            ),
        );
    }
    r = r.fallback(axum::routing::get(|uri: axum::http::Uri| async move {
        let path = uri.path().trim_start_matches('/');
        if embedded::get(path).is_some() {
            serve_asset_embedded(path)
        } else {
            match embedded::get("404.html") {
                Some(bytes) => (StatusCode::NOT_FOUND, Body::from(bytes.to_vec())).into_response(),
                None => (StatusCode::NOT_FOUND, Html("Not found")).into_response(),
            }
        }
    }));
    r
}
