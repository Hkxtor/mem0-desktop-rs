//! `/auth/*` business routes: setup-status, register, login, refresh, me,
//! change-password, onboarding-complete, and the session sub-routes
//! (PUT/POST/DELETE /auth/session). Mirrors `server/routers/auth.py` +
//! `server/routers/session.py`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Json;
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
    if store.is_jti_revoked(&claims.jti)? {
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
    // revoke old jti
    store
        .conn
        .execute(
            "UPDATE refresh_token_jtis SET revoked=1 WHERE jti=?1",
            [claims.jti.as_str()],
        )
        .ok();
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

// ---- /auth/session (cookie-oriented variants used by the dashboard) ----

pub async fn put_session() -> ApiResult<Json<Value>> {
    // PUT /auth/session in Python refreshes the session cookie; for the REST port we
    // accept it and return 200 with the same shape as POST.
    Ok(Json(json!({ "message": "Session refreshed" })))
}

pub async fn post_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RefreshRequest>,
) -> ApiResult<Json<Value>> {
    state.rate_limiter.check(&rate_key(&headers), "refresh")?;
    refresh(State(state), headers, Json(body)).await
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if let Some(auth_type) = Some(auth.auth_type) {
        let _ = auth_type;
    }
    // revoke all jtis for the user
    if let Some(uid) = &auth.user_id {
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        store
            .conn
            .execute(
                "UPDATE refresh_token_jtis SET revoked=1 WHERE user_id=?1",
                [uid.as_str()],
            )
            .ok();
    }
    Ok(Json(json!({ "message": "Session ended" })))
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
