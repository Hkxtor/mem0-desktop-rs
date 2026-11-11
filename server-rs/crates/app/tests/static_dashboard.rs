//! Static dashboard serving: page routes, asset mounts, security headers, 404 fallback.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn serves_dashboard_pages_and_assets() {
    let dir = tempfile::tempdir().unwrap();
    let app = mem0_desktop_rs::app_for_test(dir.path().to_path_buf());

    // / -> index.html (dashboard export exists at server/dashboard/out via CARGO_MANIFEST_DIR)
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-frame-options").is_some(),
        "security headers present"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("<html") || body.contains("<!doctype"),
        "index.html served"
    );

    // /dashboard/memories (no trailing slash) and /dashboard/memories/ both 200
    for uri in [
        "/dashboard/memories",
        "/dashboard/memories/",
        "/login",
        "/setup",
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{uri} should serve 200, got {}",
            resp.status()
        );
    }

    // RSC payload
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dashboard/memories.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // /health still works alongside static
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
