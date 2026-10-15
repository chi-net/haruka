use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use argon2::Argon2;
use askama::Template;
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Form, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

use crate::{crypto, entity::privacy_credential, AppState};

const IDLE: Duration = Duration::from_secs(60);
const MAX_FAILURES: u8 = 10;
const HEADER: &str = "x-haruka-privacy";

type PrivacyResult<T> = Result<T, Response>;

#[derive(Clone, Copy)]
pub struct PrivacyAccess(pub bool);

struct Grant {
    session: String,
    last_activity: Instant,
}

#[derive(Default)]
pub(crate) struct PrivacyState {
    grants: HashMap<String, Grant>,
    failures: u8,
    cooldown_until: Option<Instant>,
}

impl PrivacyState {
    fn valid(&mut self, token: &str, session: &str, now: Instant) -> bool {
        self.grants
            .retain(|_, grant| now.duration_since(grant.last_activity) < IDLE);
        self.grants
            .get(token)
            .is_some_and(|grant| grant.session == session)
    }

    fn check_attempts(&mut self, now: Instant) -> PrivacyResult<()> {
        if let Some(until) = self.cooldown_until {
            if now < until {
                let seconds = until.duration_since(now).as_secs().saturating_add(1);
                let mut response = no_store((StatusCode::TOO_MANY_REQUESTS, Json(json!({
                    "ok": false, "error": "隐私密码错误次数过多，请稍后重试", "retry_after": seconds
                }))).into_response());
                if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
                return Err(response);
            }
            self.cooldown_until = None;
            self.failures = 0;
        }
        Ok(())
    }

    fn failed(&mut self, now: Instant) -> Response {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.cooldown_until = Some(now + IDLE);
            return self.check_attempts(now).unwrap_err();
        }
        failure(StatusCode::UNAUTHORIZED, "隐私密码错误")
    }

    fn issue(&mut self, session: String, now: Instant) -> String {
        self.grants
            .retain(|_, grant| now.duration_since(grant.last_activity) < IDLE);
        let token = URL_SAFE_NO_PAD.encode(crypto::random_bytes::<32>());
        self.grants.insert(
            token.clone(),
            Grant {
                session,
                last_activity: now,
            },
        );
        self.failures = 0;
        self.cooldown_until = None;
        token
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn failure(status: StatusCode, message: impl std::fmt::Display) -> Response {
    no_store(
        (
            status,
            Json(json!({"ok": false, "error": message.to_string()})),
        )
            .into_response(),
    )
}

fn storage_error(error: impl std::fmt::Display) -> Response {
    failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("隐私密码存储失败：{error}"),
    )
}

fn success(token: Option<String>) -> Response {
    no_store(
        Json(json!({"ok": true, "token": token, "idle_seconds": IDLE.as_secs()})).into_response(),
    )
}

fn session<'a>(state: &AppState, headers: &'a HeaderMap) -> Option<&'a str> {
    let token = AppState::session_token(headers)?;
    state
        .sessions
        .read()
        .ok()?
        .contains_key(token)
        .then_some(token)
}

fn require_session<'a>(state: &AppState, headers: &'a HeaderMap) -> PrivacyResult<&'a str> {
    session(state, headers).ok_or_else(|| failure(StatusCode::UNAUTHORIZED, "请先解锁主应用"))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(HEADER)?
        .to_str()
        .ok()
        .filter(|token| !token.is_empty())
}

/// Checking authorization never renews a grant; only genuine client activity may do so.
pub async fn is_unlocked(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = bearer(headers) else {
        return false;
    };
    let mut privacy = state.privacy.lock().await;
    let Some(session) = session(state, headers) else {
        privacy.grants.remove(token);
        return false;
    };
    privacy.valid(token, session, Instant::now())
}

async fn credential(state: &AppState) -> PrivacyResult<Option<privacy_credential::Model>> {
    privacy_credential::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(storage_error)
}

async fn derive(password: Zeroizing<String>, salt: Vec<u8>) -> PrivacyResult<Zeroizing<[u8; 32]>> {
    tokio::task::spawn_blocking(move || {
        let mut verifier = Zeroizing::new([0u8; 32]);
        Argon2::default()
            .hash_password_into(password.as_bytes(), &salt, &mut *verifier)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(verifier)
    })
    .await
    .map_err(|error| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("隐私密码派生失败：{error}"),
        )
    })?
    .map_err(|error| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("隐私密码派生失败：{error}"),
        )
    })
}

async fn verify(
    password: Zeroizing<String>,
    row: &privacy_credential::Model,
) -> PrivacyResult<bool> {
    if row.salt.len() != crypto::SALT_LEN || row.verifier.len() != 32 {
        return Err(storage_error("隐私凭据格式无效"));
    }
    let supplied = derive(password, row.salt.clone()).await?;
    Ok(crate::constant_time_eq(supplied.as_slice(), &row.verifier))
}

fn validate(password: &str, confirm: &str) -> PrivacyResult<()> {
    if password.chars().count() < 6 {
        return Err(failure(StatusCode::BAD_REQUEST, "隐私密码至少 6 位"));
    }
    if !crate::constant_time_eq(password.as_bytes(), confirm.as_bytes()) {
        return Err(failure(StatusCode::BAD_REQUEST, "两次输入的隐私密码不一致"));
    }
    Ok(())
}

#[derive(Template)]
#[template(path = "privacy.html")]
struct PrivacyTemplate {
    configured: bool,
    unlocked: bool,
}

pub async fn show(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_session(&state, &headers) {
        return response;
    }
    let configured = match credential(&state).await {
        Ok(row) => row.is_some(),
        Err(response) => return response,
    };
    let template = PrivacyTemplate {
        configured,
        unlocked: is_unlocked(&state, &headers).await,
    };
    match template.render() {
        Ok(html) => no_store(Html(html).into_response()),
        Err(error) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("隐私页面渲染失败：{error}"),
        ),
    }
}

#[derive(Deserialize)]
pub struct SetupForm {
    password: String,
    confirm: String,
}

pub async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    let password = Zeroizing::new(form.password);
    let confirm = Zeroizing::new(form.confirm);
    if let Err(response) = require_session(&state, &headers) {
        return response;
    }
    if let Err(response) = validate(&password, &confirm) {
        return response;
    }
    // Serialize the singleton check and INSERT with all password operations.
    let mut privacy = state.privacy.lock().await;
    match credential(&state).await {
        Ok(Some(_)) => {
            return failure(StatusCode::CONFLICT, "已设置隐私密码，请使用原隐私密码解锁")
        }
        Ok(None) => {}
        Err(response) => return response,
    }
    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>().to_vec();
    let verifier = match derive(password, salt.clone()).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let session = match require_session(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(error) = (privacy_credential::ActiveModel {
        id: Set(1),
        salt: Set(salt),
        verifier: Set(verifier.to_vec()),
    })
    .insert(&state.db)
    .await
    {
        return storage_error(error);
    }
    // An ordinary lock during the database write must not grant privacy access.
    if let Err(response) = require_session(&state, &headers) {
        return response;
    }
    success(Some(privacy.issue(session.to_owned(), Instant::now())))
}

#[derive(Deserialize)]
pub struct UnlockForm {
    password: String,
}

pub async fn unlock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UnlockForm>,
) -> Response {
    let password = Zeroizing::new(form.password);
    if let Err(response) = require_session(&state, &headers) {
        return response;
    }
    let mut privacy = state.privacy.lock().await;
    if let Err(response) = privacy.check_attempts(Instant::now()) {
        return response;
    }
    let row = match credential(&state).await {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::CONFLICT, "请先设置隐私密码"),
        Err(response) => return response,
    };
    match verify(password, &row).await {
        Ok(true) => {}
        Ok(false) => return privacy.failed(Instant::now()),
        Err(response) => return response,
    }
    let session = match require_session(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    success(Some(privacy.issue(session.to_owned(), Instant::now())))
}

pub async fn activity(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut privacy = state.privacy.lock().await;
    let (Some(session), Some(token)) = (session(&state, &headers), bearer(&headers)) else {
        return failure(StatusCode::UNAUTHORIZED, "隐私模式已锁定");
    };
    let now = Instant::now();
    if !privacy.valid(token, session, now) {
        return failure(StatusCode::UNAUTHORIZED, "隐私模式已锁定");
    }
    if let Some(grant) = privacy.grants.get_mut(token) {
        grant.last_activity = now;
    }
    success(None)
}

pub async fn lock(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut privacy = state.privacy.lock().await;
    if let Some(token) = bearer(&headers) {
        privacy.grants.remove(token);
    }
    success(None)
}

#[derive(Deserialize)]
pub struct ChangeForm {
    current_password: String,
    password: String,
    confirm: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ChangeForm>,
) -> Response {
    let current_password = Zeroizing::new(form.current_password);
    let password = Zeroizing::new(form.password);
    let confirm = Zeroizing::new(form.confirm);
    if let Err(response) = validate(&password, &confirm) {
        return response;
    }
    let mut privacy = state.privacy.lock().await;
    let (Some(session_token), Some(token)) = (session(&state, &headers), bearer(&headers)) else {
        return failure(StatusCode::UNAUTHORIZED, "请先解锁隐私模式");
    };
    if !privacy.valid(token, session_token, Instant::now()) {
        return failure(StatusCode::UNAUTHORIZED, "请先解锁隐私模式");
    }
    if let Err(response) = privacy.check_attempts(Instant::now()) {
        return response;
    }
    let row = match credential(&state).await {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::CONFLICT, "尚未设置隐私密码"),
        Err(response) => return response,
    };
    match verify(current_password, &row).await {
        Ok(true) => {}
        Ok(false) => return privacy.failed(Instant::now()),
        Err(response) => return response,
    }
    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>().to_vec();
    let verifier = match derive(password, salt.clone()).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if session(&state, &headers).is_none() || !privacy.valid(token, session_token, Instant::now()) {
        return failure(StatusCode::UNAUTHORIZED, "隐私模式已锁定，请重新解锁");
    }
    let mut updated = row.into_active_model();
    updated.salt = Set(salt);
    updated.verifier = Set(verifier.to_vec());
    if let Err(error) = updated.update(&state.db).await {
        return storage_error(error);
    }
    privacy.grants.clear();
    privacy.failures = 0;
    privacy.cooldown_until = None;
    success(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_expires_at_sixty_seconds_without_renewing_on_read() {
        let start = Instant::now();
        let mut privacy = PrivacyState::default();
        let token = privacy.issue("session-a".into(), start);
        assert!(privacy.valid(&token, "session-a", start + Duration::from_secs(59)));
        assert!(!privacy.valid(&token, "session-a", start + IDLE));
    }

    #[test]
    fn grant_is_bound_to_issuing_main_session() {
        let start = Instant::now();
        let mut privacy = PrivacyState::default();
        let token = privacy.issue("session-a".into(), start);
        assert!(!privacy.valid(&token, "session-b", start));
        assert!(privacy.valid(&token, "session-a", start));
        assert!(!privacy.valid("", "session-a", start));
    }

    #[test]
    fn privacy_throttling_has_its_own_sixty_second_cooldown() {
        let start = Instant::now();
        let mut privacy = PrivacyState::default();
        let mut independent = PrivacyState::default();
        for _ in 0..9 {
            assert_eq!(privacy.failed(start).status(), StatusCode::UNAUTHORIZED);
        }
        assert_eq!(
            privacy.failed(start).status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(privacy
            .check_attempts(start + Duration::from_secs(59))
            .is_err());
        assert!(independent.check_attempts(start).is_ok());
        assert!(privacy.check_attempts(start + IDLE).is_ok());
        assert_eq!(privacy.failures, 0);
    }
}
