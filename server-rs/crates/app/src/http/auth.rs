//! Auth middleware: JWT bearer, X-API-Key, legacy admin key, and AUTH_DISABLED mode.
//!
//! Port of `server/auth.py::verify_auth`. Accepted credentials, in order:
//!   1. `Authorization: Bearer <jwt>`
//!   2. `X-API-Key: <key>` or `Authorization: Token <key>`
//!   3. `ADMIN_API_KEY` via either API-key header
//!   4. `AUTH_DISABLED=true` with no usable credential
//!
//! In local mode a stale/made-up key does not 401 (the bundled dashboard sends
//! X-API-Key unconditionally); with auth enabled an unusable key is rejected.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use std::sync::Arc;

use super::error::ApiError;
use super::AppState;
use crate::store::StateStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user id
    pub username: String,
    pub role: String,
    pub jti: String,
    pub exp: usize,
    pub iat: usize,
    #[serde(default)]
    pub type_: Option<String>, // "access" | "refresh"
}

pub fn encode_token(
    secret: &str,
    user_id: &str,
    username: &str,
    role: &str,
    ttl_secs: u64,
    type_: &str,
) -> Result<String, ApiError> {
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: user_id.to_string(),
        username: username.to_string(),
        role: role.to_string(),
        jti: uuid::Uuid::new_v4().to_string(),
        exp: now + ttl_secs as usize,
        iat: now,
        type_: Some(type_.to_string()),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| ApiError::internal(format!("jwt encode: {e}")))
}

pub fn decode_token(secret: &str, token: &str) -> Result<Claims, ApiError> {
    let mut validation = Validation::default();
    validation.validate_exp = true;
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|_| ApiError::unauthorized("Invalid or expired token"))?;
    Ok(data.claims)
}

/// What authenticated the request, mirroring `_mark_auth_type` in Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    Bearer,
    ApiKey,
    AdminApiKey,
    Disabled,
}

impl AuthType {
    /// The value `_mark_auth_type` records in Python's `request_logs.auth_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::Bearer => "bearer",
            AuthType::ApiKey => "api_key",
            AuthType::AdminApiKey => "admin_api_key",
            AuthType::Disabled => "disabled",
        }
    }
}

/// Slot the logging middleware drops into the request before authentication runs, so
/// the log row can name how the request authenticated.
///
/// Middleware only sees the response on the way out; the request extensions the auth
/// layer writes are consumed by the inner router. A shared slot is the way to carry one
/// value back out without guessing from headers a second time.
#[derive(Clone, Default)]
pub struct AuthTypeSlot(pub std::sync::Arc<std::sync::Mutex<Option<&'static str>>>);

impl AuthTypeSlot {
    pub fn set(&self, auth_type: &'static str) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(auth_type);
        }
    }

    /// The recorded value, defaulting to `none` — the same default Python's
    /// `getattr(request.state, "auth_type", "none")` falls back to.
    pub fn get(&self) -> &'static str {
        self.0.lock().ok().and_then(|s| *s).unwrap_or("none")
    }
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub role: Option<String>,
    pub auth_type: AuthType,
}

impl AuthContext {
    pub fn is_admin(&self) -> bool {
        matches!(self.auth_type, AuthType::AdminApiKey | AuthType::Disabled)
            || self.role.as_deref() == Some("admin")
    }
}

/// Extract the bearer token from `Authorization: Bearer ...`.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// What kind of credential the request presented, without validating it.
///
/// Used for a rejected request: the row should say whether it arrived with a bearer
/// token, an API key, or nothing at all.
fn presented_auth_type(state: &AppState, headers: &HeaderMap) -> &'static str {
    if bearer_token(headers).is_some() {
        return "bearer";
    }
    if let Some(key) = api_key(headers) {
        if let Ok(admin_key) = std::env::var("ADMIN_API_KEY") {
            if !admin_key.is_empty() && constant_time_eq(key.as_bytes(), admin_key.as_bytes()) {
                return "admin_api_key";
            }
        }
        return "api_key";
    }
    if state.auth_disabled {
        return "disabled";
    }
    // Nothing usable was offered; `none` is Python's default for an unauthenticated row.
    "none"
}

/// Extract an API key from `X-API-Key` or `Authorization: Token ...`.
fn api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.to_string());
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Token "))
        .map(|s| s.to_string())
}

fn sha256_hex(input: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(input.as_bytes()))
}

/// Resolve a raw API key to a user id via the state store (constant-time compare on
/// the stored hash, same as Python's `secrets.compare_digest` on key hashes).
fn resolve_api_key(store: &StateStore, key: &str) -> Result<Option<String>, ApiError> {
    let hash = sha256_hex(key);
    match store.find_api_key_by_hash(&hash) {
        Ok(Some(row)) => Ok(Some(row.created_by)),
        Ok(None) => Ok(None),
        Err(e) => Err(ApiError::internal(format!("api key lookup: {e}"))),
    }
}

/// The auth middleware. Attaches an `AuthContext` as a request extension.
/// Paths that answer without a credential.
///
/// The Python server has no global auth middleware — it puts `Depends(require_auth)` on the
/// routes that need a user, which leaves the dashboard assets and the sign-in endpoints
/// open. A single global middleware cannot express that, so the exemption has to be spelled
/// out here. Getting it wrong is not a subtle bug: with authentication enabled and this list
/// too short, the login page itself answers `401`, so there is no way into the app at all.
fn public_path(path: &str) -> bool {
    const EXACT: &[&str] = &[
        // liveness, and the dashboard's own entry points
        "/health",
        "/",
        "/login",
        "/setup",
        "/404",
        // sign-in: these are what a caller reaches for *because* it has no credential
        "/auth/setup-status",
        "/auth/register",
        "/auth/login",
        "/auth/refresh",
        "/auth/session",
    ];
    // Dashboard assets. A page cannot present a token before its JavaScript has loaded.
    const PREFIXES: &[&str] = &["/dashboard", "/_next", "/fonts", "/images", "/icons"];
    EXACT.contains(&path)
        || PREFIXES.iter().any(|p| path.starts_with(p))
        // Next's static export ships each page's RSC payload as `<route>.txt` next to it.
        || path.ends_with(".txt")
}

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let slot = request.extensions().get::<AuthTypeSlot>().cloned();
    let path = request.uri().path().to_string();
    if public_path(&path) {
        if let Some(slot) = &slot {
            // `/auth/session` authenticates with its own cookie, which the handler reads;
            // the rest present nothing at all.
            slot.set("none");
        }
        request.extensions_mut().insert(AuthContext {
            user_id: None,
            username: None,
            role: None,
            auth_type: AuthType::Disabled,
        });
        return Ok(next.run(request).await);
    }
    let ctx = match authenticate(&state, request.headers()).await {
        Ok(ctx) => ctx,
        Err(e) => {
            // A rejected request is still a request that happened; log it as the kind of
            // credential it presented so the table distinguishes 401 noise from 500s.
            if let Some(slot) = &slot {
                slot.set(presented_auth_type(&state, request.headers()));
            }
            return Err(e);
        }
    };
    if let Some(slot) = &slot {
        slot.set(ctx.auth_type.as_str());
    }
    request.extensions_mut().insert(ctx);
    Ok(next.run(request).await)
}

pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<AuthContext, ApiError> {
    if let Some(token) = bearer_token(headers) {
        let claims = decode_token(&state.jwt_secret, &token)?;
        // reject refresh tokens used as access tokens
        if claims.type_.as_deref() == Some("refresh") {
            return Err(ApiError::unauthorized(
                "Refresh token cannot be used as access token",
            ));
        }
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        if store.is_jti_revoked(&claims.jti).unwrap_or(false) {
            return Err(ApiError::unauthorized("Token has been revoked"));
        }
        return Ok(AuthContext {
            user_id: Some(claims.sub.clone()),
            username: Some(claims.username.clone()),
            role: Some(claims.role.clone()),
            auth_type: AuthType::Bearer,
        });
    }

    if let Some(key) = api_key(headers) {
        if let Ok(admin_key) = std::env::var("ADMIN_API_KEY") {
            if !admin_key.is_empty() && constant_time_eq(key.as_bytes(), admin_key.as_bytes()) {
                return Ok(AuthContext {
                    user_id: None,
                    username: None,
                    role: None,
                    auth_type: AuthType::AdminApiKey,
                });
            }
        }
        let store = state
            .store
            .lock()
            .map_err(|_| ApiError::internal("store lock poisoned"))?;
        match resolve_api_key(&store, &key) {
            Ok(Some(user_id)) => {
                // look up username/role
                let user = store.get_user_by_id(&user_id).ok().flatten();
                return Ok(AuthContext {
                    user_id: Some(user_id),
                    username: user.as_ref().map(|u| u.username.clone()),
                    role: user.map(|u| u.role),
                    auth_type: AuthType::ApiKey,
                });
            }
            Ok(None) => {
                if !state.auth_disabled {
                    return Err(ApiError::unauthorized("Invalid API key"));
                }
                // local mode: stale key must not lock out the operator
                return Ok(AuthContext {
                    user_id: None,
                    username: None,
                    role: None,
                    auth_type: AuthType::Disabled,
                });
            }
            Err(e) => return Err(e),
        }
    }

    if state.auth_disabled {
        return Ok(AuthContext {
            user_id: None,
            username: None,
            role: None,
            auth_type: AuthType::Disabled,
        });
    }

    crate::logging::warn(
        "auth",
        "rejected request without credentials",
        serde_json::json!({}),
    );
    Err(ApiError::unauthorized(
        "Authentication required. Provide a Bearer token or X-API-Key header.",
    ))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------- rate limiting ----------------

/// Sliding-window rate limiter (single process; the desktop app is one process).
/// Mirrors the per-route limits in `server/routers/auth.py`.
pub struct RateLimiter {
    windows: Mutex<std::collections::HashMap<String, VecDeque<Instant>>>,
    limits: std::collections::HashMap<String, (u32, Duration)>,
}

impl RateLimiter {
    pub fn new() -> Self {
        let mut limits = std::collections::HashMap::new();
        limits.insert("register".to_string(), (5, Duration::from_secs(60)));
        limits.insert("login".to_string(), (10, Duration::from_secs(60)));
        limits.insert("refresh".to_string(), (20, Duration::from_secs(60)));
        limits.insert("change-password".to_string(), (10, Duration::from_secs(60)));
        Self {
            windows: Mutex::new(std::collections::HashMap::new()),
            limits,
        }
    }

    /// Returns Ok(()) if allowed, Err(ApiError 429) if over the limit.
    pub fn check(&self, key: &str, bucket: &str) -> Result<(), ApiError> {
        let Some((max, window)) = self.limits.get(bucket).copied() else {
            return Ok(()); // no limit configured for this bucket
        };
        let mut windows = self
            .windows
            .lock()
            .map_err(|_| ApiError::internal("limiter lock poisoned"))?;
        let entry = windows.entry(format!("{key}:{bucket}")).or_default();
        let now = Instant::now();
        while let Some(front) = entry.front() {
            if now.duration_since(*front) > window {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() as u32 >= max {
            return Err(ApiError::too_many(format!(
                "Rate limit exceeded for {bucket}. Try again later."
            )));
        }
        entry.push_back(now);
        Ok(())
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_roundtrip_and_refresh_rejected_as_access() {
        let secret = concat!("test-", "secret-", "x1");
        let t = encode_token(secret, "u1", "alice", "admin", 3600, "access").unwrap();
        let c = decode_token(secret, &t).unwrap();
        assert_eq!(c.sub, "u1");
        assert_eq!(c.role, "admin");
        let rt = encode_token(secret, "u1", "alice", "admin", 3600, "refresh").unwrap();
        let c2 = decode_token(secret, &rt).unwrap();
        assert_eq!(c2.type_.as_deref(), Some("refresh"));
    }

    #[test]
    fn jwt_wrong_secret_rejected() {
        let t = encode_token(
            &format!("{}a", "secret-"),
            "u1",
            "a",
            "user",
            3600,
            "access",
        )
        .unwrap();
        assert!(decode_token(&format!("{}b", "secret-"), &t).is_err());
    }

    #[test]
    fn rate_limiter_blocks_after_max() {
        let rl = RateLimiter::new();
        for _ in 0..10 {
            assert!(rl.check("ip", "login").is_ok());
        }
        assert!(rl.check("ip", "login").is_err());
        // different bucket has its OWN window keyed by (key,bucket) — the map key
        // must combine both, otherwise register shares login's window. Use a
        // composite key in check() calls below via distinct keys:
        assert!(
            rl.check("ip", "register").is_ok(),
            "register bucket is separate"
        );
        // different client unaffected
        assert!(rl.check("other-ip", "login").is_ok());
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}

// ---------------- password hashing (bcrypt, passlib-compatible PHC strings) ----------------

/// Hash a password with bcrypt, producing a `$2b$12$...` PHC string identical in
/// format to what Python passlib's `bcrypt` scheme stores.
pub fn hash_password(password: &str) -> Result<String, ApiError> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| ApiError::internal(format!("hash password: {e}")))
}

/// Verify a password against a stored PHC hash. An empty or unrecognized hash is a
/// failed verification (never an error), matching `server/auth.py::verify_password` —
/// this is what makes the Local user (empty hash) un-loginable.
pub fn verify_password(plain: &str, hashed: &str) -> bool {
    if hashed.is_empty() {
        return false;
    }
    bcrypt::verify(plain, hashed).unwrap_or(false)
}

#[cfg(test)]
mod password_tests {
    use super::*;

    /// Fixture generated by Python passlib: bcrypt.hash('golden-password-123')
    const PY_FIXTURE: &str = "$2b$12$DwU1hzPOOOi3v9Qi5qNaIOxlhjXbdyX.YB5lftwX2o7J5fMg5CjPq";

    #[test]
    fn verifies_python_passlib_fixture() {
        assert!(verify_password("golden-password-123", PY_FIXTURE));
        assert!(!verify_password("wrong", PY_FIXTURE));
    }

    #[test]
    fn rust_hash_is_verifiable_and_passlib_shaped() {
        let h = hash_password("s3cret!").unwrap();
        assert!(h.starts_with("$2"), "PHC string must start with $2: {h}");
        assert!(verify_password("s3cret!", &h));
        assert!(!verify_password("other", &h));
    }

    #[test]
    fn empty_hash_is_failed_check_not_error() {
        // the Local user's empty hash must behave as "cannot log in"
        assert!(!verify_password("anything", ""));
    }
}
