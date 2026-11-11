//! With authentication enabled, the app has to be reachable *and* protected.
//!
//! The Python server authenticates per route, so the dashboard assets and the sign-in
//! endpoints stay open while everything else needs a user. The Rust port authenticates in
//! one middleware, and when that middleware exempted only `/health` the result was an app
//! nobody could enter: `GET /login` answered `401`, `POST /auth/register` answered `401`,
//! and `POST /auth/session` could never be reached — so the refresh cookie was never set and
//! every page reload logged the operator out.
//!
//! These tests pin both halves: what must stay open, and what must stay closed.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    router: axum::Router,
}

fn app_with_auth() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "false") };
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(
        mem0_desktop_rs::http::AppState::new(dir.path().to_path_buf()).expect("state"),
    );
    App {
        _dir: dir,
        router: mem0_desktop_rs::http::build_router_arc(state),
    }
}

async fn get(app: &App, uri: &str) -> StatusCode {
    app.router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn post(app: &App, uri: &str, body: &str) -> StatusCode {
    app.router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The pages you need in order to sign in must not answer 401. They may 404 when the
/// dashboard was never built into the binary, which is why this checks "not unauthorized"
/// rather than "200".
#[tokio::test]
async fn the_login_pages_are_reachable_without_a_credential() {
    let app = app_with_auth();
    for uri in ["/", "/login", "/setup", "/404", "/health"] {
        let status = get(&app, uri).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "{uri} must not require a token"
        );
    }
}

/// Signing up and signing in cannot require having signed in.
#[tokio::test]
async fn the_sign_in_endpoints_answer_without_a_credential() {
    let app = app_with_auth();
    // An empty body is a client error about the body, not about authentication.
    let status = post(&app, "/auth/register", "{}").await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "/auth/register: {status}");
    let status = post(&app, "/auth/login", "{}").await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "/auth/login: {status}");
    let status = get(&app, "/auth/setup-status").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "setup-status drives the first-run wizard"
    );
}

/// Everything that touches a user's data stays closed.
#[tokio::test]
async fn the_data_routes_still_require_a_credential() {
    let app = app_with_auth();
    for uri in [
        "/auth/me",
        "/memories",
        "/entities",
        "/requests",
        "/analytics/summary",
    ] {
        assert_eq!(
            get(&app, uri).await,
            StatusCode::UNAUTHORIZED,
            "{uri} must require a credential"
        );
    }
}

/// The first-run wizard has to be able to create the admin account that will then protect
/// everything else — otherwise enabling auth on a fresh data directory locks the app.
#[tokio::test]
async fn the_first_account_can_be_registered() {
    let app = app_with_auth();
    let status = post(
        &app,
        "/auth/register",
        r#"{"name":"Local","email":"local@example.com","password":"password123"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first registration must succeed");

    // And the new account can then be used to reach protected routes.
    let status = post(
        &app,
        "/auth/login",
        r#"{"email":"local@example.com","password":"password123"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
