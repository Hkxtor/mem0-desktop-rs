//! The dashboard's session dance: `PUT` stores a refresh token in an `HttpOnly` cookie,
//! `POST` trades it for an access token, `DELETE` drops it.
//!
//! The bug these tests exist for: the dashboard sends no body on `POST /auth/session`
//! (the cookie *is* the credential), while the handler demanded JSON — so every refresh
//! answered `415`, and with authentication enabled the browser could never renew a session.
//! A page reload logged the operator out.
//!
//! Authentication is on for every test in this file, which is the configuration that
//! exposes the problem: in local mode (`AUTH_DISABLED=true`) nothing needs a token, so a
//! broken refresh path is invisible. Each `tests/*.rs` is its own process, so flipping the
//! variable here cannot race another test binary.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

const REFRESH_COOKIE: &str = "mem0_refresh_token";

struct App {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    router: axum::Router,
    state: std::sync::Arc<mem0_desktop_rs::http::AppState>,
    secret: String,
}

/// A router with authentication **enabled**, and one user to authenticate as.
fn app_with_auth() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "false") };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let state =
        std::sync::Arc::new(mem0_desktop_rs::http::AppState::new(path.clone()).expect("state"));
    let secret = state
        .store
        .lock()
        .expect("store")
        .get_or_create_jwt_secret()
        .expect("secret");
    let now = chrono::Utc::now().to_rfc3339();
    state
        .store
        .lock()
        .expect("store")
        .insert_auth_user(&mem0_desktop_rs::store_auth::AuthUser {
            base: mem0_desktop_rs::store::User {
                id: "u-1".into(),
                username: "alice".into(),
                password_hash: "not-a-real-hash".into(),
                role: "admin".into(),
                created_at: now.clone(),
                updated_at: now,
            },
            name: "Alice".into(),
            email: Some("alice@example.com".into()),
            last_login_at: None,
        })
        .expect("insert user");
    let router = mem0_desktop_rs::http::build_router_arc(state.clone());
    App {
        _dir: dir,
        dir: path,
        router,
        state,
        secret,
    }
}

fn refresh_token(app: &App) -> String {
    let token = mem0_desktop_rs::http::auth::encode_token(
        &app.secret,
        "u-1",
        "alice",
        "admin",
        60 * 60 * 24 * 30,
        "refresh",
    )
    .expect("refresh token");
    // `/auth/login` registers the jti it minted; a token without a row would be rejected on
    // first use, so the fixture has to do the same to stand in for a real login.
    let claims = mem0_desktop_rs::http::auth::decode_token(&app.secret, &token).expect("claims");
    let expires = chrono::Utc::now() + chrono::Duration::days(30);
    app.state
        .store
        .lock()
        .expect("store")
        .insert_jti(&claims.jti, "u-1", &expires.to_rfc3339())
        .expect("insert jti");
    token
}

async fn call(
    app: &App,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if cookie.is_some() {
        builder = builder.header(
            "cookie",
            format!("{REFRESH_COOKIE}={}", cookie.unwrap_or("")),
        );
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.router.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let set_cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        set_cookie,
    )
}

/// The exact call the dashboard makes with no session: `POST` with no body.
///
/// It must be a `401` that says why, not a `415` about a missing content type. A 415 tells
/// the operator nothing and is what the browser silently treated as "logged out".
#[tokio::test]
async fn posting_without_a_cookie_is_a_401_not_a_415() {
    let app = app_with_auth();
    let (status, body, set_cookie) = call(&app, "POST", "/auth/session", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["detail"], json!("No session."));
    // The dead cookie is dropped so the next page load goes straight to the login screen.
    let cookie = set_cookie.expect("a Set-Cookie header");
    assert!(cookie.contains("Max-Age=0"), "{cookie}");
}

/// The whole round trip the browser performs, and the reason it has to work: the refresh
/// exchange revokes the presented token, so the rotated one must come back in the cookie.
#[tokio::test]
async fn a_stored_token_can_be_exchanged_and_is_rotated() {
    let app = app_with_auth();
    let token = refresh_token(&app);

    let (status, body, set_cookie) = call(
        &app,
        "PUT",
        "/auth/session",
        Some(json!({ "refresh_token": token })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    let cookie = set_cookie.expect("PUT must set the cookie");
    assert!(
        cookie.starts_with(&format!("{REFRESH_COOKIE}={token}")),
        "{cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");

    // The browser's exchange: cookie only, no body, no bearer token.
    let (status, body, set_cookie) = call(&app, "POST", "/auth/session", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let access = body["access_token"].as_str().expect("an access token");
    assert!(!access.is_empty());
    assert_eq!(body["token_type"], json!("bearer"));
    let rotated = set_cookie.expect("POST must rotate the cookie");
    assert!(rotated.contains("HttpOnly"), "{rotated}");
    assert!(
        !rotated.starts_with(&format!("{REFRESH_COOKIE}={token};")),
        "the presented token must be replaced, not echoed back: {rotated}"
    );

    // The access token works on a protected route.
    let request = Request::builder()
        .method("GET")
        .uri("/auth/me")
        .header("authorization", format!("Bearer {access}"))
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A rotated token is single-use: replaying the old one fails and the cookie is dropped.
#[tokio::test]
async fn a_consumed_token_cannot_be_replayed() {
    let app = app_with_auth();
    let token = refresh_token(&app);
    let (status, _, _) = call(&app, "POST", "/auth/session", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body, set_cookie) = call(&app, "POST", "/auth/session", None, Some(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(set_cookie.unwrap_or_default().contains("Max-Age=0"));
}

/// Logging out clears the cookie, and does not need a valid access token to do it.
#[tokio::test]
async fn deleting_the_session_clears_the_cookie() {
    let app = app_with_auth();
    let token = refresh_token(&app);
    let (status, body, set_cookie) =
        call(&app, "DELETE", "/auth/session", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    assert!(set_cookie.unwrap_or_default().contains("Max-Age=0"));

    // Revoked as well as cleared: the cookie is dead even if a copy survives.
    let (status, _, _) = call(&app, "POST", "/auth/session", None, Some(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// `PUT` with an empty token is a client error, not a cookie holding nothing.
#[tokio::test]
async fn storing_an_empty_token_is_rejected() {
    let app = app_with_auth();
    let (status, body, set_cookie) = call(
        &app,
        "PUT",
        "/auth/session",
        Some(json!({ "refresh_token": "" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        set_cookie.is_none(),
        "a rejected store must not touch the cookie"
    );
}

/// These three endpoints are reachable without a bearer token by design — that is the whole
/// point, and it is also the thing a future "lock down every route" change could break.
#[tokio::test]
async fn the_session_endpoints_do_not_require_a_bearer_token() {
    let app = app_with_auth();
    // With auth on, an ordinary protected route rejects an anonymous caller...
    let (status, _, _) = call(&app, "GET", "/auth/me", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // ...while the session endpoints answer for themselves.
    let (status, _, _) = call(&app, "POST", "/auth/session", None, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "401 about the session, not about auth"
    );
    let (status, _, _) = call(&app, "DELETE", "/auth/session", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

/// The refresh token in the cookie must not be readable by the page's JavaScript: an
/// `HttpOnly` flag missing here would expose a 30-day credential to any XSS.
#[tokio::test]
async fn the_cookie_is_http_only_and_not_secure() {
    let app = app_with_auth();
    let token = refresh_token(&app);
    let (_, _, set_cookie) = call(
        &app,
        "PUT",
        "/auth/session",
        Some(json!({ "refresh_token": token })),
        None,
    )
    .await;
    let cookie = set_cookie.expect("cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    // `Secure` is absent on purpose: this server speaks plain HTTP on loopback, where a
    // Secure cookie is never sent back and the session silently stops working.
    assert!(!cookie.contains("Secure"), "{cookie}");
    let _ = &app.dir;
}
