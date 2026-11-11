//! M4-T2: start_server port fallback + CLI arg parsing.

use mem0_desktop_rs::http::embedded;
use std::net::SocketAddr;

#[tokio::test]
async fn start_server_binds_preferred_port() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), Some(0))
        .await
        .unwrap();
    assert_eq!(
        addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_ne!(addr.port(), 0);
}

#[tokio::test]
async fn start_server_falls_back_when_preferred_taken() {
    let dir = tempfile::tempdir().unwrap();
    // Occupy a port.
    let blocker = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let taken = blocker.local_addr().unwrap().port();
    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), Some(taken))
        .await
        .unwrap();
    assert_ne!(
        addr.port(),
        taken,
        "must fall back to a free ephemeral port"
    );
    drop(blocker);
}

#[tokio::test]
async fn start_server_serves_health() {
    let dir = tempfile::tempdir().unwrap();
    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), None)
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[test]
fn cli_port_parsing() {
    let args = vec![
        "mem0-desktop".to_string(),
        "--port".to_string(),
        "9999".to_string(),
    ];
    assert_eq!(mem0_desktop_rs::cli_port(&args), Some(9999));
    let args = vec!["mem0-desktop".to_string(), "--headless".to_string()];
    assert_eq!(mem0_desktop_rs::cli_port(&args), None);
    assert!(mem0_desktop_rs::headless_requested(&args));
    let args = vec!["mem0-desktop".to_string()];
    assert!(!mem0_desktop_rs::headless_requested(&args));
}

#[tokio::test]
async fn start_server_defaults_auth_disabled_on_loopback() {
    // Mirror of the Windows bug: a desktop launch with NO env vars must not 401.
    // start_server sets AUTH_DISABLED=true when the var is absent (server only
    // binds 127.0.0.1, so it is unreachable from the network anyway).
    unsafe { std::env::remove_var("AUTH_DISABLED") };
    let dir = tempfile::tempdir().unwrap();
    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), None)
        .await
        .unwrap();
    let client = reqwest::Client::new();
    // /auth/me without any credential: must NOT be 401 in local default mode.
    let resp = client
        .get(format!("http://{addr}/auth/me"))
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        401,
        "loopback default must relax auth, got {}",
        resp.status()
    );
    // An explicitly-set AUTH_DISABLED=false must be respected (opt-out).
    unsafe { std::env::set_var("AUTH_DISABLED", "false") };
    let dir2 = tempfile::tempdir().unwrap();
    let (_h2, addr2) = mem0_desktop_rs::start_server(dir2.path(), None)
        .await
        .unwrap();
    let resp2 = client
        .get(format!("http://{addr2}/auth/me"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp2.status(),
        401,
        "explicit AUTH_DISABLED=false must keep auth on"
    );
    unsafe { std::env::remove_var("AUTH_DISABLED") };
}

#[tokio::test]
async fn packaged_binary_serves_dashboard_from_embedded_copy() {
    // Simulates the shipped artifact: no source checkout, no MEM0_DASHBOARD_DIR,
    // data dir in a tempdir. `/` must still be 200 with real HTML, and assets
    // under /_next/ must resolve — i.e. static_files falls back to the embedded
    // export (build.rs copies server/dashboard/out; stub on fresh clones).
    unsafe { std::env::remove_var("MEM0_DASHBOARD_DIR") };
    let dir = tempfile::tempdir().unwrap();
    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), None)
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let root = client.get(format!("http://{addr}/")).send().await.unwrap();
    assert_eq!(root.status(), 200);
    let body = root.text().await.unwrap();
    assert!(
        body.contains("<html"),
        "embedded / must serve HTML, got: {}",
        &body[..body.len().min(80)]
    );
    // a real asset path from the export
    let asset = client
        .get(format!("http://{addr}/favicon.ico"))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), 200);
    // unknown path -> 404 with the export's 404.html
    let nf = client
        .get(format!("http://{addr}/definitely-not-a-page"))
        .send()
        .await
        .unwrap();
    assert_eq!(nf.status(), 404);
}

#[tokio::test]
async fn embedded_root_route_has_no_path_extractor() {
    // Regression: registering "/" with a Path<String> extractor makes axum 500
    // with "Wrong number of path arguments for `Path`. Expected 1 but got 0".
    // The root route must use a plain handler.
    // Router<Arc<AppState>> needs a state to become a Service; embedded_routes
    // ignores it, so feed a dummy state built from a temp dir.
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let state = std::sync::Arc::new(
        mem0_desktop_rs::http::AppState::new(dir.path().to_path_buf()).unwrap(),
    );
    let app = mem0_desktop_rs::http::static_files::embedded_routes().with_state(state);
    let mut app = app;
    for (uri, want) in [
        ("/", 200u16),
        ("/setup", 200),
        ("/setup/", 200),
        ("/setup.txt", 200),
        ("/dashboard/memories", 200),
        ("/dashboard/memories/", 200),
        ("/index.txt", 200),
        ("/favicon.ico", 200),
        ("/no-such-page", 404),
    ] {
        let resp = tower::ServiceExt::oneshot(
            &mut app,
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            want,
            "embedded route {uri} returned {} (want {want})",
            resp.status()
        );
    }
}

#[tokio::test]
async fn embedded_assets_all_referenced_files_servable() {
    // Regression for the analytics/settings client-side crash: the exported HTML
    // preloads fonts under /_next/static/media/... — if the embedded asset
    // route 404s them, hydration dies. Sweep every asset referenced by the
    // exported index.html and assert each resolves in the embedded copy.
    let html = embedded::get("index.html").expect("embedded index");
    let html = std::str::from_utf8(html).unwrap();
    let mut refs = std::collections::BTreeSet::new();
    for cap in html.split("/_next/static/").skip(1) {
        // asset URLs end at a quote/space; route-group dirs like (auth) contain
        // parens, which are legal in the file name — keep them, stop at quotes.
        // stop at the first char that cannot appear in a Next asset filename;
        // critically, an escaped quote (\\") inside a JS string in the HTML
        // must not leak into the path (that produced bogus refs like ...s.p.t)
        let path: String = cap
            .chars()
            .take_while(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | '(' | ')')
            })
            .collect();
        if !path.contains('.') {
            continue; // not a file ref (e.g. a prefix of a longer name)
        }
        refs.insert(format!("_next/static/{path}"));
    }
    assert!(
        refs.len() > 10,
        "expected many asset refs, got {}",
        refs.len()
    );
    for r in &refs {
        assert!(
            embedded::get(r).is_some(),
            "embedded export is missing referenced asset {r}"
        );
    }
}

#[test]
fn analytics_summary_shape_matches_dashboard_contract() {
    // The dashboard analytics page reads data.top_paths without a null guard
    // (`data?.top_paths.length`); a missing key is undefined.length -> crash.
    let dir = tempfile::tempdir().unwrap();
    let s = mem0_desktop_rs::store::StateStore::open(&dir.path().join("mem0.db")).unwrap();
    let v = s.analytics_summary(12).unwrap();
    assert!(
        v.get("top_paths").and_then(|t| t.as_array()).is_some(),
        "analytics/summary must include top_paths array, got {v}"
    );
    assert!(v.get("daily").and_then(|d| d.as_array()).is_some());
    assert!(v.get("operations").is_some());
    assert!(v.get("success_rate").is_some());
}

#[tokio::test]
async fn local_user_me_response_has_empty_string_email_not_null() {
    // Settings page does `email.trim()` on the /auth/me response; Python's
    // local user has email="" and the Rust port must match (null crashes React).
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), None)
        .await
        .unwrap();
    let res = reqwest::get(format!("http://{addr}/auth/me"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body["email"],
        serde_json::json!(""),
        "/auth/me email must be \"\" not null for the local user, got {body}"
    );
}

const MOCK_CONFIG: &str = r#"{
  "version": "v1.1",
  "llm": {"provider": "mock", "config": {}},
  "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
  "vector_store": {"provider": "sqlite-vec", "config": {"collection_name": "mem0", "embedding_model_dims": 10}}
}"#;

#[tokio::test]
async fn restart_with_persisted_config_restores_the_runtime() {
    // Field regression: after configuring providers and restarting the app, every memory
    // call answered 503 runtime_not_configured even though config.json was on disk, and
    // setup-status said needsSetup — so the wizard re-ran, its prefilled Providers form
    // was not "dirty", /configure was never re-posted, and the Quick Test kept failing.
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), MOCK_CONFIG).unwrap();

    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), None)
        .await
        .unwrap();

    let status: serde_json::Value = reqwest::get(format!("http://{addr}/auth/setup-status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["needsSetup"],
        serde_json::json!(false),
        "a stored config must count as configured after restart: {status}"
    );

    let res = reqwest::Client::new()
        .post(format!("http://{addr}/memories"))
        .json(&serde_json::json!({
            "messages": [{"role": "user", "content": "after restart"}],
            "user_id": "u1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        200,
        "memory calls must work after a restart, got {} {}",
        res.status(),
        res.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn unusable_persisted_config_keeps_server_up_and_names_the_cause() {
    // The other half of the same rule: a config that cannot be built must not take the
    // process down (portable app — /setup has to stay reachable), and the 503 must say
    // what is wrong instead of claiming nothing is configured.
    unsafe { std::env::set_var("AUTH_DISABLED", "true") };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"llm": {"provider": "no-such-llm", "config": {}},
            "embedder": {"provider": "mock", "config": {"embedding_dims": 10}},
            "vector_store": {"config": {"embedding_model_dims": 10}}}"#,
    )
    .unwrap();

    let (_h, addr) = mem0_desktop_rs::start_server(dir.path(), None)
        .await
        .unwrap();

    assert_eq!(
        reqwest::get(format!("http://{addr}/health"))
            .await
            .unwrap()
            .status(),
        200,
        "an unusable config must not prevent boot"
    );
    assert_eq!(
        reqwest::get(format!("http://{addr}/setup"))
            .await
            .unwrap()
            .status(),
        200,
        "the setup page must stay reachable to fix the config"
    );

    let res = reqwest::Client::new()
        .post(format!("http://{addr}/memories"))
        .json(&serde_json::json!({
            "messages": [{"role": "user", "content": "x"}],
            "user_id": "u1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    let body = res.text().await.unwrap();
    assert!(
        body.contains("no-such-llm"),
        "503 must name the real cause, got: {body}"
    );
}
