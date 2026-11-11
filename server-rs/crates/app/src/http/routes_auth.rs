//! `/auth/*` business routes: setup-status, register, login, refresh, me,
//! change-password, onboarding-complete, and the session sub-routes
//! (PUT/POST/DELETE /auth/session). Mirrors `server/routers/auth.py` +
//! `server/routers/session.py`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Json};
use axum::Extension;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::{self, hash_password, verify_password, AuthContext};
use super::error::{ApiError, ApiResult};
use super::AppState;
use crate::store_auth::AuthUser;

const ACCESS_TTL_SECS: u64 = 60 * 60; // 1h
const REFRESH_TTL_SECS: u64 = 60 * 60 * 24 * 30; // 30d
const MIN_PASSWORD_LEN: usize = 8;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    #[serde(alias = "old_password")]
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Deserialize)]
pub struct OnboardingCompleteRequest {
    pub use_case: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchMeRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

fn token_response(state: &AppState, user: &AuthUser) -> ApiResult<Value> {
    let access = auth::encode_token(
        &state.jwt_secret,
        &user.base.id,
        &user.base.username,
        &user.base.role,
        ACCESS_TTL_SECS,
        "access",
    )?;
    let refresh_jti = uuid::Uuid::new_v4().to_string();
    let refresh = auth::encode_token(
        &state.jwt_secret,
        &user.base.id,
        &user.base.username,
        &user.base.role,
        REFRESH_TTL_SECS,
        "refresh",
    )?;
    // re-encode refresh with the jti we persist (encode_token mints its own jti, so decode+rebuild)
    let mut claims = auth::decode_token(&state.jwt_secret, &refresh)?;
    claims.jti = refresh_jti.clone();
    let refresh = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .map_err(|e| ApiError::internal(format!("jwt encode: {e}")))?;
    let expires = chrono::Utc::now() + chrono::Duration::seconds(REFRESH_TTL_SECS as i64);
    {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.insert_jti(&refresh_jti, &user.base.id, &expires.to_rfc3339())?;
    }
    Ok(json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
    }))
}

fn user_response(u: &AuthUser) -> Value {
    json!({
        "id": u.base.id,
        "name": u.name,
        // Python's User.email is "" for the local-mode owner, never null — the
        // dashboard settings page calls email.trim() on this response.
        "email": u.email.clone().unwrap_or_default(),
        "role": u.base.role,
        "created_at": u.base.created_at,
    })
}

fn rate_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("local")
        .to_string()
}

pub async fn setup_status(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if state.auth_disabled {
        let configured = state_has_runtime(&state).await;
        return Ok(Json(
            json!({ "needsSetup": !configured, "localMode": true }),
        ));
    }
    let has = {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store.has_loginable_user()?
    };
    Ok(Json(json!({ "needsSetup": !has, "localMode": false })))
}

async fn state_has_runtime(state: &AppState) -> bool {
    state.memory.read().await.is_some()
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> ApiResult<Json<Value>> {
    state.rate_limiter.check(&rate_key(&headers), "register")?;
    if body.password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::bad_request(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters."
        )));
    }
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    if store.has_loginable_user()? {
        return Err(ApiError::forbidden(
            "Registration is closed. An admin account already exists.",
        ));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let user = AuthUser {
        base: crate::store::User {
            id: uuid::Uuid::new_v4().to_string(),
            username: body.email.clone(),
            password_hash: hash_password(&body.password)?,
            role: "admin".into(),
            created_at: now.clone(),
            updated_at: now,
        },
        name: body.name.clone(),
        email: Some(body.email.clone()),
        last_login_at: None,
    };
    match store.insert_auth_user(&user) {
        Ok(()) => {}
        Err(_) => {
            return Err(ApiError::forbidden(
                "Registration is closed. An admin account already exists.",
            ))
        }
    }
    drop(store);
    Ok(Json(token_response(&state, &user)?))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Json<Value>> {
    state.rate_limiter.check(&rate_key(&headers), "login")?;
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    let user = match store.get_user_by_email(&body.email)? {
        Some(u) => u,
        None => return Err(ApiError::unauthorized("Invalid email or password.")),
    };
    if !verify_password(&body.password, &user.base.password_hash) {
        return Err(ApiError::unauthorized("Invalid email or password."));
    }
    let _ = store.update_last_login(&user.base.id);
    drop(store);
    Ok(Json(token_response(&state, &user)?))
}

pub async fn refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RefreshRequest>,
) -> ApiResult<Json<Value>> {
    state.rate_limiter.check(&rate_key(&headers), "refresh")?;
    let claims = auth::decode_token(&state.jwt_secret, &body.refresh_token)
        .map_err(|_| ApiError::unauthorized("Invalid or expired token"))?;
    if claims.type_.as_deref() != Some("refresh") {
        return Err(ApiError::unauthorized("Invalid token type."));
    }
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    // Consuming the jti is the validity check: it fails for an unknown, already used, or
    // revoked token, so a rotated token cannot be replayed.
    if !store.consume_jti(&claims.jti)? {
        return Err(ApiError::unauthorized("Refresh token is no longer valid."));
    }
    let user = store.get_user_by_id(&claims.sub)?.map(|u| AuthUser {
        base: u.clone(),
        name: String::new(),
        email: None,
        last_login_at: None,
    });
    let user = match user {
        Some(u) => u,
        None => return Err(ApiError::unauthorized("User not found.")),
    };
    drop(store);
    Ok(Json(token_response(&state, &user)?))
}

/// Resolve the current user from the AuthContext (bearer) or local mode.
async fn current_user(state: &AppState, auth: &AuthContext) -> ApiResult<AuthUser> {
    if let Some(uid) = &auth.user_id {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        let base = store
            .get_user_by_id(uid)?
            .ok_or_else(|| ApiError::unauthorized("User not found."))?;
        // fetch extended row by id
        let ext: Option<AuthUser> = store
            .conn
            .query_row(
                "SELECT name, email, last_login_at FROM users WHERE id=?1",
                [uid.as_str()],
                |r| {
                    Ok(AuthUser {
                        base: base.clone(),
                        name: r.get(0)?,
                        email: r.get(1)?,
                        last_login_at: r.get(2)?,
                    })
                },
            )
            .ok();
        drop(store);
        if let Some(u) = ext {
            return Ok(u);
        }
        return Ok(AuthUser {
            base,
            name: String::new(),
            email: None,
            last_login_at: None,
        });
    }
    // local mode: the Local user
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    store.ensure_local_user()?;
    let local = store
        .get_user_by_username("Local")?
        .ok_or_else(|| ApiError::internal("Local user missing"))?;
    drop(store);
    Ok(AuthUser {
        base: local,
        name: "Local".into(),
        email: None,
        last_login_at: None,
    })
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let user = current_user(&state, &auth).await?;
    Ok(Json(user_response(&user)))
}

pub async fn patch_me(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<PatchMeRequest>,
) -> ApiResult<Json<Value>> {
    let user = current_user(&state, &auth).await?;
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    let name = body.name.unwrap_or_else(|| user.name.clone());
    let email = body
        .email
        .or_else(|| user.email.clone())
        .unwrap_or_default();
    store.update_user_name_email(&user.base.id, &name, &email)?;
    drop(store);
    let mut updated = user;
    updated.name = name;
    updated.email = Some(email);
    Ok(Json(user_response(&updated)))
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<ChangePasswordRequest>,
) -> ApiResult<Json<Value>> {
    state
        .rate_limiter
        .check(&rate_key(&headers), "change-password")?;
    if body.new_password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::bad_request(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters."
        )));
    }
    let user = current_user(&state, &auth).await?;
    if !verify_password(&body.current_password, &user.base.password_hash) {
        return Err(ApiError::unauthorized("Invalid old password."));
    }
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    store.update_password_hash(&user.base.id, &hash_password(&body.new_password)?)?;
    drop(store);
    Ok(Json(json!({ "message": "Password changed successfully" })))
}

pub async fn onboarding_complete(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Json(_body): Json<OnboardingCompleteRequest>,
) -> ApiResult<Json<Value>> {
    // store the use case in settings for the dashboard to read
    let store = state
        .store
        .lock()
        .map_err(|_| ApiError::internal("store lock poisoned"))?;
    store.set_setting("onboarding_use_case", _body.use_case.as_str())?;
    drop(store);
    Ok(Json(json!({ "message": "Onboarding completed" })))
}

// ---- /auth/session: the browser's half of the session dance ----
//
// The dashboard keeps its access token in memory and its refresh token in an `HttpOnly`
// cookie it can neither read nor write. That makes these three endpoints the only way the
// browser can hold a session, and none of them can require a bearer token: `PUT` runs
// before anything is stored, `POST` is what a client calls *because* its access token
// expired, and `DELETE` has to work when the token is already dead. They are public in
// `auth_middleware` for exactly that reason, and carry their own credential — the cookie.

/// Name and lifetime of the refresh cookie. The name is the Python server's
/// (`REFRESH_COOKIE_NAME`), so a profile that switched builds keeps its session.
pub const REFRESH_COOKIE_NAME: &str = "mem0_refresh_token";
const REFRESH_COOKIE_MAX_AGE: u64 = REFRESH_TTL_SECS;

/// Read one cookie out of a request's `Cookie` header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Attach a `Set-Cookie` that stores the refresh token, or removes it when `token` is None.
///
/// `Secure` is deliberately absent: this server only ever listens on plain HTTP over
/// loopback, where a `Secure` cookie would never be sent back and the session would silently
/// never work. A deployment behind a TLS proxy would need it, and would also need to be a
/// deployment this app does not currently support.
fn with_refresh_cookie(
    mut response: axum::response::Response,
    token: Option<&str>,
) -> axum::response::Response {
    let cookie = match token {
        Some(token) => format!(
            "{name}={token}; Max-Age={max_age}; Path=/; HttpOnly; SameSite=Lax",
            name = REFRESH_COOKIE_NAME,
            max_age = REFRESH_COOKIE_MAX_AGE,
        ),
        // `Max-Age=0` removes it in every browser; the value is left empty rather than
        // omitted so the attributes match the one that set it.
        None => format!(
            "{name}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
            name = REFRESH_COOKIE_NAME,
        ),
    };
    if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    response
}

/// A JSON response that also clears the cookie — used for every session failure, so a dead
/// token does not linger and make the next page load retry it.
fn session_failure(status: axum::http::StatusCode, detail: &str) -> axum::response::Response {
    let body = (status, Json(json!({ "detail": detail }))).into_response();
    with_refresh_cookie(body, None)
}

#[derive(Debug, Deserialize)]
pub struct StoreSessionRequest {
    pub refresh_token: String,
}

/// `PUT /auth/session` — park the token `/auth/login` just minted in a cookie.
///
/// The token is not validated: it was issued by this server a moment ago, and the jti is
/// single-use, so validating here would consume it before the browser ever exchanges it.
pub async fn put_session(
    Json(body): Json<StoreSessionRequest>,
) -> ApiResult<axum::response::Response> {
    if body.refresh_token.is_empty() {
        return Err(ApiError::bad_request("Missing refresh_token."));
    }
    let response = Json(json!({ "ok": true })).into_response();
    Ok(with_refresh_cookie(response, Some(&body.refresh_token)))
}

/// `POST /auth/session` — trade the cookie for a fresh access token, rotating the cookie.
///
/// Rotation is not optional: the exchange revokes the presented jti, so a response that
/// returned a new access token but left the old cookie in place would break the *next*
/// refresh.
pub async fn post_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<axum::response::Response> {
    let Some(refresh_token) = cookie_value(&headers, REFRESH_COOKIE_NAME) else {
        return Ok(session_failure(
            axum::http::StatusCode::UNAUTHORIZED,
            "No session.",
        ));
    };
    state.rate_limiter.check(&rate_key(&headers), "refresh")?;
    match refresh(
        State(state),
        headers,
        Json(RefreshRequest { refresh_token }),
    )
    .await
    {
        Ok(Json(payload)) => {
            let rotated = payload
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let access = payload.get("access_token").cloned().unwrap_or(Value::Null);
            let body =
                Json(json!({ "access_token": access, "token_type": "bearer" })).into_response();
            Ok(with_refresh_cookie(body, rotated.as_deref()))
        }
        // Expired, revoked, already rotated, or the user is gone: the cookie is dead
        // weight, so drop it and let the dashboard show the login screen.
        Err(e) => Ok(session_failure(e.status, &e.detail)),
    }
}

/// `DELETE /auth/session` — log out. Idempotent: there may be nothing to drop.
///
/// The presented refresh token is revoked when it decodes cleanly, which the Python server
/// does not do (it has no verified token to revoke). Revoking is strictly better: a copied
/// cookie stops working the moment its owner logs out.
pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<axum::response::Response> {
    if let Some(token) = cookie_value(&headers, REFRESH_COOKIE_NAME) {
        if let Ok(claims) = auth::decode_token(&state.jwt_secret, &token) {
            if let Ok(store) = state.store.lock() {
                let _ = store.conn.execute(
                    "UPDATE refresh_token_jtis SET revoked=1 WHERE jti=?1",
                    [claims.jti.as_str()],
                );
            }
        }
    }
    let response = Json(json!({ "ok": true })).into_response();
    Ok(with_refresh_cookie(response, None))
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post, put};
    axum::Router::new()
        .route("/auth/setup-status", get(setup_status))
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/refresh", post(refresh))
        .route("/auth/me", get(me).patch(patch_me))
        .route("/auth/change-password", post(change_password))
        .route("/auth/onboarding-complete", post(onboarding_complete))
        .route(
            "/auth/session",
            put(put_session).post(post_session).delete(delete_session),
        )
}
