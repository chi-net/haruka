use std::time::{Duration, Instant};

use axum::{
    extract::{Extension, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel, QueryFilter,
    Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use webauthn_rs::prelude::{Passkey, PublicKeyCredential, RegisterPublicKeyCredential, Uuid};
use zeroize::Zeroizing;

use crate::{
    crypto,
    entity::{meta, passkey},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);
const USER_ID: u128 = 0x6861_7275_6b61_0000_0000_0000_0000_0001;
// WebAuthn PRF 的输入不需要保密；固定且带域分隔的输入可让同一凭据稳定地产生 KEK。
const PRF_INPUT: &[u8] = b"haruka passkey DEK wrapping key v1";
const MAX_COMPATIBLE_WRAPPERS: usize = 8;

#[derive(Serialize, Deserialize)]
struct CompatibleWrapper {
    dek_nonce: String,
    wrapped_dek: String,
}

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

fn flow_id() -> String {
    URL_SAFE_NO_PAD.encode(crypto::random_bytes::<32>())
}

fn decode_prf(value: &str) -> HandlerResult<[u8; crypto::DEK_LEN]> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| bad_request("Passkey PRF 输出格式无效"))?;
    bytes
        .try_into()
        .map_err(|_| bad_request("Passkey PRF 输出长度无效"))
}

fn passkey_from_row(row: &passkey::Model) -> HandlerResult<Passkey> {
    serde_json::from_str(&row.credential)
        .map_err(|error| err500(format!("Passkey 数据损坏: {error}")))
}

fn compatible_wrappers(row: &passkey::Model) -> HandlerResult<Vec<CompatibleWrapper>> {
    serde_json::from_str(&row.dek_wrappers)
        .map_err(|error| err500(format!("Passkey 兼容密钥数据损坏: {error}")))
}

fn unwrap_passkey_dek(row: &passkey::Model, kek: &[u8]) -> HandlerResult<Option<crypto::Dek>> {
    if let Some(dek) = crypto::unwrap_dek(&row.dek_nonce, &row.wrapped_dek, kek) {
        return Ok(Some(dek));
    }
    for wrapper in compatible_wrappers(row)? {
        let nonce = URL_SAFE_NO_PAD
            .decode(wrapper.dek_nonce)
            .map_err(|_| err500("Passkey 兼容 nonce 数据损坏"))?;
        let wrapped = URL_SAFE_NO_PAD
            .decode(wrapper.wrapped_dek)
            .map_err(|_| err500("Passkey 兼容密钥数据损坏"))?;
        if let Some(dek) = crypto::unwrap_dek(&nonce, &wrapped, kek) {
            return Ok(Some(dek));
        }
    }
    Ok(None)
}

async fn add_compatible_wrapper(
    db: &DatabaseConnection,
    passkey_id: i64,
    dek: &crypto::Dek,
    kek: &[u8],
) -> HandlerResult<()> {
    // 在事务内重读，避免两台同步设备绑定时用旧快照覆盖另一台的包裹。
    let transaction = db.begin().await.map_err(err500)?;
    let row = passkey::Entity::find_by_id(passkey_id)
        .one(&transaction)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("Passkey 不存在"))?;
    if unwrap_passkey_dek(&row, kek)?.is_some() {
        return Ok(());
    }
    let mut wrappers = compatible_wrappers(&row)?;
    if wrappers.len() >= MAX_COMPATIBLE_WRAPPERS {
        return Err(bad_request(
            "这个 Passkey 的设备兼容绑定数量已达上限，请用主密码解锁后在设置中重新添加 Passkey",
        ));
    }
    let (nonce, wrapped) = crypto::wrap_dek(dek, kek);
    wrappers.push(CompatibleWrapper {
        dek_nonce: URL_SAFE_NO_PAD.encode(nonce),
        wrapped_dek: URL_SAFE_NO_PAD.encode(wrapped),
    });
    let mut active = row.into_active_model();
    active.dek_wrappers = Set(serde_json::to_string(&wrappers).map_err(err500)?);
    active.update(&transaction).await.map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(())
}

async fn store_registered_passkey(
    state: &AppState,
    dek: &crypto::Dek,
    credential: Passkey,
    name: &str,
    prf_result: &str,
) -> HandlerResult<()> {
    let kek = decode_prf(prf_result)?;
    let (dek_nonce, wrapped_dek) = crypto::wrap_dek(dek, &kek);
    let credential_id = credential.cred_id().as_ref().to_vec();
    let credential = serde_json::to_string(&credential).map_err(err500)?;
    passkey::ActiveModel {
        credential_id: Set(credential_id),
        credential: Set(credential),
        name: Set(crypto::encrypt(dek, name.as_bytes())),
        dek_nonce: Set(dek_nonce),
        wrapped_dek: Set(wrapped_dek),
        dek_wrappers: Set("[]".into()),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(())
}

async fn all_passkeys(state: &AppState) -> HandlerResult<Vec<(passkey::Model, Passkey)>> {
    let rows = passkey::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    rows.into_iter()
        .map(|row| passkey_from_row(&row).map(|credential| (row, credential)))
        .collect()
}

#[derive(Deserialize)]
pub struct StartRegistration {
    name: String,
}

pub async fn start_registration(
    State(state): State<AppState>,
    Json(form): Json<StartRegistration>,
) -> HandlerResult<Json<Value>> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(bad_request("请填写 Passkey 名称"));
    }
    if name.chars().count() > 80 {
        return Err(bad_request("Passkey 名称不能超过 80 个字符"));
    }

    let stored = all_passkeys(&state).await?;
    let excluded = (!stored.is_empty()).then(|| {
        stored
            .iter()
            .map(|(_, credential)| credential.cred_id().clone())
            .collect()
    });
    let (options, registration) = state
        .webauthn
        .start_passkey_registration(Uuid::from_u128(USER_ID), "haruka", "haruka", excluded)
        .map_err(|error| err500(format!("创建 Passkey 注册请求失败: {error}")))?;

    let id = flow_id();
    let mut flows = state.passkey_registrations.lock().await;
    flows.retain(|_, (created, _, _)| created.elapsed() < CEREMONY_TTL);
    if flows.len() >= 64 {
        flows.clear();
    }
    flows.insert(id.clone(), (Instant::now(), registration, name.to_string()));
    Ok(Json(json!({
        "flow_id": id,
        "options": options,
        "prf_input": URL_SAFE_NO_PAD.encode(PRF_INPUT),
    })))
}

#[derive(Deserialize)]
pub struct FinishRegistration {
    flow_id: String,
    credential: RegisterPublicKeyCredential,
    prf_enabled: bool,
    #[serde(default)]
    prf_result: Option<String>,
}

pub async fn finish_registration(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Json(form): Json<FinishRegistration>,
) -> HandlerResult<Json<Value>> {
    let entry = state
        .passkey_registrations
        .lock()
        .await
        .remove(&form.flow_id);
    let Some((created, registration, name)) = entry else {
        return Err(bad_request("Passkey 注册请求不存在或已使用，请重试"));
    };
    if created.elapsed() >= CEREMONY_TTL {
        return Err(bad_request("Passkey 注册请求已过期，请重试"));
    }
    if !form.prf_enabled {
        return Err(bad_request(
            "此浏览器或认证器不支持 WebAuthn PRF，无法用于解锁加密数据",
        ));
    }

    let credential = state
        .webauthn
        .finish_passkey_registration(&form.credential, &registration)
        .map_err(|error| bad_request(format!("Passkey 注册验证失败: {error}")))?;
    let credential_id = credential.cred_id().as_ref().to_vec();
    if passkey::Entity::find()
        .filter(passkey::Column::CredentialId.eq(credential_id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some()
    {
        return Err(bad_request("该 Passkey 已注册"));
    }

    if let Some(prf_result) = form.prf_result.filter(|value| !value.is_empty()) {
        store_registered_passkey(&state, &dek, credential, &name, &prf_result).await?;
        return Ok(Json(json!({ "ok": true, "complete": true })));
    }

    // 部分认证器在创建时只报告 PRF 已启用而不返回结果，此时追加一次同路径认证取得 KEK。
    let (options, authentication) = state
        .webauthn
        .start_passkey_authentication(std::slice::from_ref(&credential))
        .map_err(|error| err500(format!("创建 Passkey 确认请求失败: {error}")))?;
    let id = flow_id();
    let mut flows = state.passkey_enrollments.lock().await;
    flows.retain(|_, (created, _, _, _)| created.elapsed() < CEREMONY_TTL);
    if flows.len() >= 64 {
        flows.clear();
    }
    flows.insert(
        id.clone(),
        (Instant::now(), authentication, credential, name),
    );
    Ok(Json(json!({
        "complete": false,
        "flow_id": id,
        "options": options,
        "prf_input": URL_SAFE_NO_PAD.encode(PRF_INPUT),
    })))
}

#[derive(Deserialize)]
pub struct CompleteRegistration {
    flow_id: String,
    credential: PublicKeyCredential,
    prf_result: String,
}

pub async fn complete_registration(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Json(form): Json<CompleteRegistration>,
) -> HandlerResult<Json<Value>> {
    let entry = state.passkey_enrollments.lock().await.remove(&form.flow_id);
    let Some((created, authentication, mut credential, name)) = entry else {
        return Err(bad_request("Passkey 确认请求不存在或已使用，请重试"));
    };
    if created.elapsed() >= CEREMONY_TTL {
        return Err(bad_request("Passkey 确认请求已过期，请重试"));
    }
    let result = state
        .webauthn
        .finish_passkey_authentication(&form.credential, &authentication)
        .map_err(|error| bad_request(format!("Passkey 确认失败: {error}")))?;
    if result.cred_id() != credential.cred_id() {
        return Err(bad_request("Passkey 凭据不匹配"));
    }
    credential.update_credential(&result);

    store_registered_passkey(&state, &dek, credential, &name, &form.prf_result).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn start_authentication(State(state): State<AppState>) -> HandlerResult<Json<Value>> {
    let stored = all_passkeys(&state).await?;
    if stored.is_empty() {
        return Err((StatusCode::NOT_FOUND, "尚未设置 Passkey".into()));
    }
    let credentials: Vec<_> = stored
        .into_iter()
        .map(|(_, credential)| credential)
        .collect();
    let (options, authentication) = state
        .webauthn
        .start_passkey_authentication(&credentials)
        .map_err(|error| err500(format!("创建 Passkey 登录请求失败: {error}")))?;
    let id = flow_id();
    let mut flows = state.passkey_authentications.lock().await;
    flows.retain(|_, (created, _)| created.elapsed() < CEREMONY_TTL);
    if flows.len() >= 64 {
        flows.clear();
    }
    flows.insert(id.clone(), (Instant::now(), authentication));
    Ok(Json(json!({
        "flow_id": id,
        "options": options,
        "prf_input": URL_SAFE_NO_PAD.encode(PRF_INPUT),
    })))
}

#[derive(Deserialize)]
pub struct FinishAuthentication {
    flow_id: String,
    credential: PublicKeyCredential,
    prf_result: String,
}

pub async fn finish_authentication(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<FinishAuthentication>,
) -> HandlerResult<Response> {
    let entry = state
        .passkey_authentications
        .lock()
        .await
        .remove(&form.flow_id);
    let Some((created, authentication)) = entry else {
        return Err(bad_request("Passkey 登录请求不存在或已使用，请重试"));
    };
    if created.elapsed() >= CEREMONY_TTL {
        return Err(bad_request("Passkey 登录请求已过期，请重试"));
    }
    let result = state
        .webauthn
        .finish_passkey_authentication(&form.credential, &authentication)
        .map_err(|error| bad_request(format!("Passkey 登录验证失败: {error}")))?;
    let credential_id = result.cred_id().as_ref().to_vec();
    let row = passkey::Entity::find()
        .filter(passkey::Column::CredentialId.eq(credential_id))
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("Passkey 不存在"))?;
    let mut credential = passkey_from_row(&row)?;
    if credential.update_credential(&result) == Some(true) {
        let encoded = serde_json::to_string(&credential).map_err(err500)?;
        let mut active = row.clone().into_active_model();
        active.credential = Set(encoded);
        active.update(&state.db).await.map_err(err500)?;
    }
    let kek = crypto::Dek::new(decode_prf(&form.prf_result)?);
    let Some(dek) = unwrap_passkey_dek(&row, kek.as_slice())? else {
        let id = flow_id();
        let mut repairs = state.passkey_repairs.lock().await;
        repairs.retain(|_, (created, _, _)| created.elapsed() < CEREMONY_TTL);
        if repairs.len() >= 64 {
            repairs.clear();
        }
        repairs.insert(id.clone(), (Instant::now(), row.id, kek));
        let mut response = Json(json!({
            "repair_required": true,
            "repair_flow_id": id,
            "error": "Passkey 签名已验证，但当前设备或凭据提供方返回的 PRF 无法解开已有密钥包裹。请在五分钟内用主密码绑定本次 PRF；原设备的包裹会保留"
        }))
        .into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            "no-store".parse().expect("固定响应头无效"),
        );
        return Ok(response);
    };
    crate::handlers::auth::ensure_default_account(&state, &dek).await?;
    state.remove_session(&headers);
    let mut response = Json(json!({ "redirect": "/dashboard" })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, state.create_session(&dek));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("固定响应头无效"),
    );
    Ok(response)
}

#[derive(Deserialize)]
pub struct RepairAuthentication {
    flow_id: String,
    password: String,
}

pub async fn repair_authentication(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<RepairAuthentication>,
) -> HandlerResult<Response> {
    let entry = {
        let mut repairs = state.passkey_repairs.lock().await;
        repairs.retain(|_, (created, _, _)| created.elapsed() < CEREMONY_TTL);
        repairs.get(&form.flow_id).cloned()
    };
    let Some((created, passkey_id, passkey_kek)) = entry else {
        return Err(bad_request(
            "Passkey 兼容修复请求不存在或已过期，请重新使用 Passkey",
        ));
    };
    if created.elapsed() >= CEREMONY_TTL {
        return Err(bad_request(
            "Passkey 兼容修复请求已过期，请重新使用 Passkey",
        ));
    }

    let password = Zeroizing::new(form.password);
    let metadata = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;
    let password_kek = crypto::derive_kek(password.as_str(), &metadata.salt);
    let dek = crypto::unwrap_dek(
        &metadata.dek_nonce,
        &metadata.wrapped_dek,
        password_kek.as_slice(),
    )
    .ok_or((StatusCode::UNAUTHORIZED, "主密码错误".into()))?;
    add_compatible_wrapper(&state.db, passkey_id, &dek, passkey_kek.as_slice()).await?;
    state.passkey_repairs.lock().await.remove(&form.flow_id);

    crate::handlers::auth::ensure_default_account(&state, &dek).await?;
    state.remove_session(&headers);
    let mut response = Json(json!({ "redirect": "/dashboard" })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, state.create_session(&dek));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("固定响应头无效"),
    );
    Ok(response)
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> HandlerResult<axum::response::Redirect> {
    passkey::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(axum::response::Redirect::to("/settings"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    #[test]
    fn prf_requires_exactly_32_decoded_bytes() {
        for length in [0, 31, 33] {
            assert!(decode_prf(&URL_SAFE_NO_PAD.encode(vec![7; length])).is_err());
        }
        assert!(decode_prf("not/base64!").is_err());
        assert_eq!(
            decode_prf(&URL_SAFE_NO_PAD.encode([7; 32])).unwrap(),
            [7; 32]
        );
    }

    async fn wrapper_fixture() -> (DatabaseConnection, passkey::Model, crypto::Dek) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        let table = Schema::new(backend).create_table_from_entity(passkey::Entity);
        db.execute(backend.build(&table)).await.unwrap();
        let dek = crypto::Dek::new([19; crypto::DEK_LEN]);
        let (nonce, wrapped) = crypto::wrap_dek(&dek, &[1; crypto::DEK_LEN]);
        let row = passkey::ActiveModel {
            credential_id: Set(vec![42]),
            credential: Set("{}".into()),
            name: Set(crypto::encrypt(&dek, b"synced passkey")),
            dek_nonce: Set(nonce),
            wrapped_dek: Set(wrapped),
            dek_wrappers: Set("[]".into()),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        (db, row, dek)
    }

    #[tokio::test]
    async fn device_bindings_preserve_original_and_previous_wrappers() {
        let (db, original, dek) = wrapper_fixture().await;
        for key in [2, 3, 2] {
            add_compatible_wrapper(&db, original.id, &dek, &[key; crypto::DEK_LEN])
                .await
                .unwrap();
        }
        let row = passkey::Entity::find_by_id(original.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.dek_nonce, original.dek_nonce);
        assert_eq!(row.wrapped_dek, original.wrapped_dek);
        assert_eq!(compatible_wrappers(&row).unwrap().len(), 2);
        for key in [1, 2, 3] {
            assert_eq!(
                unwrap_passkey_dek(&row, &[key; crypto::DEK_LEN])
                    .unwrap()
                    .unwrap()
                    .as_slice(),
                dek.as_slice()
            );
        }
        assert!(unwrap_passkey_dek(&row, &[4; crypto::DEK_LEN])
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn binding_limit_does_not_remove_working_wrappers() {
        let (db, original, dek) = wrapper_fixture().await;
        for key in 2..=(MAX_COMPATIBLE_WRAPPERS as u8 + 1) {
            add_compatible_wrapper(&db, original.id, &dek, &[key; crypto::DEK_LEN])
                .await
                .unwrap();
        }
        assert!(
            add_compatible_wrapper(&db, original.id, &dek, &[99; crypto::DEK_LEN])
                .await
                .is_err()
        );
        let row = passkey::Entity::find_by_id(original.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        for key in 1..=(MAX_COMPATIBLE_WRAPPERS as u8 + 1) {
            assert_eq!(
                unwrap_passkey_dek(&row, &[key; crypto::DEK_LEN])
                    .unwrap()
                    .unwrap()
                    .as_slice(),
                dek.as_slice()
            );
        }
        assert!(unwrap_passkey_dek(&row, &[99; crypto::DEK_LEN])
            .unwrap()
            .is_none());
    }
}
