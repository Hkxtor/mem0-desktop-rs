//! Desktop shell preferences over HTTP: start at login / stay in the tray / start hidden.
//!
//! The shell reads `desktop.json` directly, so the contract that matters is that a PUT
//! leaves that file in the state the next launch will read.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    router: axum::Router,
}

fn app() -> App {
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let router = mem0_desktop_rs::app_for_test(path.clone());
    App {
        _dir: dir,
        dir: path,
        router,
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn defaults_are_off_and_report_what_the_platform_supports() {
    let a = app();
    let (status, body) = call(&a.router, "GET", "/settings/desktop", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["autostart"], json!(false));
    assert_eq!(body["minimize_to_tray"], json!(false));
    assert_eq!(body["start_silent"], json!(false));
    // The login entry is a Windows registry value; this test runs on Linux in CI.
    assert_eq!(body["supported"]["autostart"], json!(cfg!(windows)));
    // Tray options need the GUI shell; this router is the headless one.
    assert_eq!(body["supported"]["tray"], json!(false));
    assert!(body["error"].is_null(), "{body}");
}

#[tokio::test]
async fn toggles_persist_to_the_file_the_shell_reads() {
    let a = app();
    let (status, body) = call(
        &a.router,
        "PUT",
        "/settings/desktop",
        Some(json!({"minimize_to_tray": true, "start_silent": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["minimize_to_tray"], json!(true));
    assert_eq!(body["start_silent"], json!(true));
    // Omitted fields keep their value, so one switch can be sent on its own.
    assert_eq!(body["autostart"], json!(false));

    let raw = std::fs::read_to_string(a.dir.join("desktop.json")).unwrap();
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["minimize_to_tray"], json!(true), "{raw}");
    assert_eq!(parsed["start_silent"], json!(true), "{raw}");

    // A restart reads the same file: a fresh router over the same directory.
    let restarted = mem0_desktop_rs::app_for_test(a.dir.clone());
    let (_, body) = call(&restarted, "GET", "/settings/desktop", None).await;
    assert_eq!(body["minimize_to_tray"], json!(true), "{body}");
    assert_eq!(body["start_silent"], json!(true), "{body}");
}

#[tokio::test]
async fn turning_a_toggle_back_off_is_recorded() {
    let a = app();
    call(
        &a.router,
        "PUT",
        "/settings/desktop",
        Some(json!({"start_silent": true})),
    )
    .await;
    let (status, body) = call(
        &a.router,
        "PUT",
        "/settings/desktop",
        Some(json!({"start_silent": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["start_silent"], json!(false));
    let raw = std::fs::read_to_string(a.dir.join("desktop.json")).unwrap();
    assert!(raw.contains("\"start_silent\": false"), "{raw}");
}

#[tokio::test]
async fn the_settings_file_is_not_the_provider_config() {
    let a = app();
    call(
        &a.router,
        "PUT",
        "/settings/desktop",
        Some(json!({"start_silent": true})),
    )
    .await;
    // Saving providers later must not wipe the desktop preferences.
    call(
        &a.router,
        "POST",
        "/configure",
        Some(json!({"version": "v1.1"})),
    )
    .await;
    let (_, body) = call(&a.router, "GET", "/settings/desktop", None).await;
    assert_eq!(body["start_silent"], json!(true), "{body}");
}
