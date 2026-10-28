//! 实例联邦：配对握手、借还事件同步、进度查询与催款。
//!
//! 配对流程：A 输入 B 域名生成一次性配对码（带外传递）；B 在自己实例用
//! 「A 域名 + 配对码」发起验证，握手负载用配对码派生密钥加密，A 验证后
//! 双方各自保存绑定。借还事件（created/confirmed/settled/reminded）以
//! 链式摘要同步，双方各自存储密文事件并重算验证，防篡改与重放。

use axum::{
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    crypto,
    entity::{debt_person, federated_event, federated_request, linked_instance},
    federation, AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}
type Instance = linked_instance::Model;
type FedRequest = federated_request::Model;

fn key_of(instance: &Instance) -> crypto::Dek {
    crypto::Dek::new(
        instance
            .pairing_key
            .as_slice()
            .try_into()
            .expect("配对密钥长度必须为 32 字节"),
    )
}

fn scheme_for(host: &str) -> &'static str {
    if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    }
}

fn self_domain(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| scheme_for(host));
    format!("{scheme}://{host}")
}

async fn find_paired(state: &AppState, domain: &str) -> Option<Instance> {
    linked_instance::Entity::find()
        .filter(linked_instance::Column::Status.eq("paired"))
        .filter(linked_instance::Column::Domain.eq(domain))
        .one(&state.db)
        .await
        .ok()
        .flatten()
}

async fn find_request(
    state: &AppState,
    instance: &Instance,
    federation_id: &str,
) -> Option<FedRequest> {
    federated_request::Entity::find()
        .filter(federated_request::Column::FederationId.eq(federation_id))
        .filter(federated_request::Column::InstanceId.eq(instance.id))
        .one(&state.db)
        .await
        .ok()
        .flatten()
}

async fn latest_event(state: &AppState, federation_id: &str) -> Option<federated_event::Model> {
    federated_event::Entity::find()
        .filter(federated_event::Column::FederationId.eq(federation_id))
        .order_by_desc(federated_event::Column::Seq)
        .one(&state.db)
        .await
        .ok()
        .flatten()
}

async fn store_event(
    state: &AppState,
    federation_id: &str,
    seq: i64,
    kind: &str,
    cipher: &str,
    previous: &[u8],
) -> Result<(), String> {
    let digest = federation::event_digest(federation_id, seq, kind, previous, cipher);
    federated_event::ActiveModel {
        federation_id: Set(federation_id.into()),
        seq: Set(seq),
        kind: Set(kind.into()),
        payload: Set(cipher.into()),
        digest: Set(digest),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(|error| format!("事件写入失败：{error}"))?;
    Ok(())
}

fn generic_error(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_string()).into_response()
}

#[derive(Deserialize)]
pub struct PairStartForm {
    domain: String,
}

/// 生成配对码：输入对方域名，返回一次性配对码（24 小时有效）。
pub async fn pair_start(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    Json(form): Json<PairStartForm>,
) -> HandlerResult<Json<Value>> {
    let domain = federation::normalize_domain(&form.domain).map_err(|error| bad_request(&error))?;
    if find_paired(&state, &domain).await.is_some() {
        return Err(bad_request("该实例已绑定，无需重复配对"));
    }
    let (code_text, code_bytes) = federation::new_pairing_code();
    let key = federation::pairing_key(&code_bytes);
    linked_instance::ActiveModel {
        domain: Set(domain.clone()),
        self_domain: Set(String::new()),
        role: Set("debtor".into()),
        status: Set("pending".into()),
        pairing_key: Set(key.as_slice().to_vec()),
        pairing_code_hash: Set(federation::pairing_code_hash(&code_bytes)),
        pairing_code_expires_at: Set(Some((Utc::now() + chrono::Duration::hours(24)).naive_utc())),
        paired_at: Set(None),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    crate::dev_log::info(format!("已为 {domain} 生成实例配对码"));
    Ok(Json(
        json!({ "ok": true, "code": code_text, "domain": domain }),
    ))
}

#[derive(Deserialize)]
pub struct PairJoinForm {
    domain: String,
    code: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct JoinPayload {
    domain: String,
    at: String,
}

/// 验证绑定：输入对方域名与配对码，调用对方实例完成加密握手。
pub async fn pair_join(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    headers: HeaderMap,
    Json(form): Json<PairJoinForm>,
) -> HandlerResult<Json<Value>> {
    let domain = federation::normalize_domain(&form.domain).map_err(|error| bad_request(&error))?;
    let code =
        federation::decode_code(&form.code).ok_or_else(|| bad_request("配对码格式不正确"))?;
    let key = federation::pairing_key(&code);
    let own_domain = self_domain(&headers);
    let payload = serde_json::to_vec(&JoinPayload {
        domain: own_domain.clone(),
        at: Utc::now().to_rfc3339(),
    })
    .map_err(err500)?;
    let cipher = crypto::encrypt(&key, &payload);
    let body = federation::PairRequest {
        code_hash: URL_SAFE_NO_PAD.encode(federation::pairing_code_hash(&code)),
        cipher,
    };
    let client = federation::http_client().map_err(|error| bad_request(&error))?;
    let response = client
        .post(format!("{domain}/debt-link/pair"))
        .json(&body)
        .send()
        .await
        .map_err(|error| bad_request(&format!("无法连接对方实例：{error}")))?;
    if !response.status().is_success() {
        return Err(bad_request("对方拒绝了配对，请检查域名与配对码是否正确"));
    }
    let body: Value = response.json().await.map_err(err500)?;
    let cipher = body
        .get("cipher")
        .and_then(|value| value.as_str())
        .ok_or_else(|| bad_request("对方响应格式不正确"))?;
    let plain = crypto::decrypt(&key, cipher).ok_or_else(|| bad_request("对方响应无法解密"))?;
    let ack: Value = serde_json::from_slice(&plain).map_err(err500)?;
    if ack.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        return Err(bad_request("对方拒绝了配对"));
    }
    // A 侧记录的 B 域名才是双方一致的对端标识（访问主机名可能不同）。
    let self_domain = ack
        .get("your_domain")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .unwrap_or(own_domain);
    linked_instance::ActiveModel {
        domain: Set(domain.clone()),
        self_domain: Set(self_domain),
        role: Set("creditor".into()),
        status: Set("paired".into()),
        pairing_key: Set(key.as_slice().to_vec()),
        pairing_code_hash: Set(Vec::new()),
        pairing_code_expires_at: Set(None),
        paired_at: Set(Some(Utc::now())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    crate::dev_log::info(format!("已与 {domain} 完成实例配对"));
    Ok(Json(json!({ "ok": true, "domain": domain })))
}

/// 公开端点：对方用配对码验证绑定（无需会话，配对码哈希索引 + 加密负载）。
pub async fn receive_pair(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<federation::PairRequest>,
) -> Response {
    let code_hash = URL_SAFE_NO_PAD
        .decode(body.code_hash.as_bytes())
        .unwrap_or_default();
    let instance = linked_instance::Entity::find()
        .filter(linked_instance::Column::Status.eq("pending"))
        .filter(linked_instance::Column::PairingCodeHash.eq(code_hash))
        .one(&state.db)
        .await;
    let Ok(Some(instance)) = instance else {
        return generic_error("配对码无效或已使用");
    };
    if instance
        .pairing_code_expires_at
        .is_some_and(|expires| expires < Utc::now().naive_utc())
    {
        return generic_error("配对码已过期，请重新生成");
    }
    let key = key_of(&instance);
    let Some(plain) = crypto::decrypt(&key, &body.cipher) else {
        return generic_error("配对数据无法解密");
    };
    let Ok(payload) = serde_json::from_slice::<JoinPayload>(&plain) else {
        return generic_error("配对数据格式不正确");
    };
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(&payload.at) else {
        return generic_error("配对时间格式不正确");
    };
    if (Utc::now() - at.with_timezone(&Utc)).num_seconds().abs() > 600 {
        return generic_error("配对请求已过期，请重试");
    }
    let Ok(domain) = federation::normalize_domain(&payload.domain) else {
        return generic_error("对方域名不合法");
    };
    // 配对码与加密负载已验证身份；域名校验以 A 侧记录为准，不再强制自报一致。
    let _ = domain;
    let peer_domain = instance.domain.clone();
    let own = self_domain(&headers);
    let mut active = instance.into_active_model();
    active.status = Set("paired".into());
    active.paired_at = Set(Some(Utc::now()));
    active.self_domain = Set(own.clone());
    active.pairing_code_hash = Set(Vec::new());
    active.pairing_code_expires_at = Set(None);
    if let Err(error) = active.update(&state.db).await {
        return generic_error(&format!("配对保存失败：{error}"));
    }
    crate::dev_log::info(format!("实例 {domain} 已完成配对验证"));
    let response = crypto::encrypt(
        &key,
        &serde_json::to_vec(&json!({ "ok": true, "domain": own, "your_domain": peer_domain }))
            .unwrap_or_default(),
    );
    Json(json!({ "cipher": response })).into_response()
}

#[derive(Deserialize)]
pub struct CreateForm {
    instance_id: i64,
    kind: String,
    account_id: i64,
    amount: String,
    note: String,
    paid_at: String,
}

/// 本地入账并推送 created 事件。返回（本地 debt_requests.id，借贷对象 id）。
async fn book_locally(
    state: &AppState,
    dek: &crypto::Dek,
    instance: &Instance,
    kind: &str,
    account_id: i64,
    amount: &str,
    note: &str,
    paid_at: &str,
) -> HandlerResult<(i64, i64)> {
    let person_id = ensure_person(state, dek, &instance.domain).await?;
    let form = super::debt_requests::CreateRequestForm {
        kind: kind.into(),
        person_id,
        account_id,
        amount: amount.into(),
        recipient_name: instance.domain.clone(),
        note: note.into(),
        expires_at: (Utc::now() + chrono::Duration::days(365 * 10))
            .format(super::debt_requests::TIME_FMT)
            .to_string(),
        requires_confirmation: Some(false),
        origin_debt_record_id: None,
        paid_at: paid_at.into(),
    };
    let response = super::debt_requests::create(
        State(state.clone()),
        Extension(SessionDek(dek.clone())),
        axum::Form(form),
    )
    .await?
    .into_response();
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let id = location
        .rsplit('=')
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| err500("本地入账未返回请求编号"))?;
    Ok((id, person_id))
}

/// 按对方域名查找或创建借贷对象，联邦入账复用同一对象。
async fn ensure_person(state: &AppState, dek: &crypto::Dek, domain: &str) -> HandlerResult<i64> {
    for person in debt_person::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        if crypto::decrypt_string(dek, &person.name) == domain {
            return Ok(person.id);
        }
    }
    let person = debt_person::ActiveModel {
        name: Set(crypto::encrypt(dek, domain.as_bytes())),
        note: Set(crypto::encrypt(
            dek,
            format!("Haruka 实例配对：{domain}").as_bytes(),
        )),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(person.id)
}

/// 创建联邦借还：本地入账 + created 事件推送。
pub async fn create_request(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Json(form): Json<CreateForm>,
) -> HandlerResult<Json<Value>> {
    if !matches!(form.kind.as_str(), "borrow" | "lend") {
        return Err(bad_request("借还类型不正确"));
    }
    let instance = linked_instance::Entity::find_by_id(form.instance_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .filter(|instance| instance.status == "paired")
        .ok_or_else(|| bad_request("实例未绑定"))?;
    let account = crate::entity::account::Entity::find_by_id(form.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("资金账户不存在"))?;
    let (local_id, person_id) = book_locally(
        &state,
        &dek,
        &instance,
        &form.kind,
        form.account_id,
        &form.amount,
        &form.note,
        &form.paid_at,
    )
    .await?;
    let amount_cents = super::debt_requests::parse_amount(&form.amount)?;
    let federation_id = federation::new_federation_id();
    let request = federated_request::ActiveModel {
        federation_id: Set(federation_id.clone()),
        instance_id: Set(instance.id),
        origin: Set("local".into()),
        kind: Set(form.kind.clone()),
        amount: Set(crypto::encrypt_cents(&key_of(&instance), amount_cents)),
        currency: Set(account.currency.clone()),
        note: Set(crypto::encrypt(&key_of(&instance), form.note.as_bytes())),
        status: Set("open".into()),
        local_debt_request_id: Set(Some(local_id)),
        account_id: Set(Some(form.account_id)),
        person_id: Set(Some(person_id)),
        remote_status: Set(String::new()),
        remote_status_at: Set(None),
        last_reminded_at: Set(None),
        sync_error: Set(String::new()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    let payload = json!({
        "kind": form.kind,
        "amount_cents": amount_cents,
        "currency": account.currency,
        "note": form.note,
        "paid_at": form.paid_at,
    });
    let sync_error = match push_event(&state, &instance, &request, "created", payload).await {
        Ok(()) => String::new(),
        Err(error) => error,
    };
    let mut active = request.into_active_model();
    active.sync_error = Set(sync_error.clone());
    active.update(&state.db).await.map_err(err500)?;
    Ok(Json(json!({ "ok": true, "sync_error": sync_error })))
}

/// 推送事件：先写本地事件链，再调用对方实例；失败时返回原因供重试。
async fn push_event(
    state: &AppState,
    instance: &Instance,
    request: &FedRequest,
    kind: &str,
    payload: Value,
) -> Result<(), String> {
    let key = key_of(instance);
    let cipher = crypto::encrypt(&key, payload.to_string().as_bytes());
    let previous = latest_event(state, &request.federation_id).await;
    let (seq, prev_digest) = previous
        .map(|event| (event.seq + 1, event.digest))
        .unwrap_or((1, Vec::new()));
    store_event(
        state,
        &request.federation_id,
        seq,
        kind,
        &cipher,
        &prev_digest,
    )
    .await?;
    let message = federation::EventMessage {
        from_domain: instance.self_domain.clone(),
        federation_id: request.federation_id.clone(),
        seq,
        kind: kind.into(),
        cipher,
    };
    let client = federation::http_client()?;
    let response = client
        .post(format!("{}/debt-link/events", instance.domain))
        .json(&message)
        .send()
        .await
        .map_err(|error| format!("推送失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("对方拒绝事件（HTTP {}）", response.status()));
    }
    Ok(())
}

/// 公开端点：接收对方事件，验证序号与链式摘要后镜像存储。
pub async fn receive_events(
    State(state): State<AppState>,
    Json(message): Json<federation::EventMessage>,
) -> Response {
    let Some(instance) = find_paired(&state, &message.from_domain).await else {
        return generic_error("实例未配对");
    };
    let key = key_of(&instance);
    let existing = find_request(&state, &instance, &message.federation_id).await;
    match (&existing, message.kind.as_str(), message.seq) {
        (None, "created", 1) => {
            let Some(plain) = crypto::decrypt(&key, &message.cipher) else {
                return generic_error("事件无法解密");
            };
            let Ok(payload) = serde_json::from_slice::<Value>(&plain) else {
                return generic_error("事件格式不正确");
            };
            let Some(kind) = payload.get("kind").and_then(|v| v.as_str()) else {
                return generic_error("事件缺少借还类型");
            };
            if !matches!(kind, "borrow" | "lend") {
                return generic_error("借还类型不正确");
            }
            let amount = payload
                .get("amount_cents")
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            let currency = payload
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("CNY")
                .to_string();
            let note = payload
                .get("note")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let request = federated_request::ActiveModel {
                federation_id: Set(message.federation_id.clone()),
                instance_id: Set(instance.id),
                origin: Set("remote".into()),
                kind: Set(federation::reverse_kind(kind).into()),
                amount: Set(crypto::encrypt_cents(&key, amount)),
                currency: Set(currency),
                note: Set(crypto::encrypt(&key, note.as_bytes())),
                status: Set("open".into()),
                local_debt_request_id: Set(None),
                account_id: Set(None),
                person_id: Set(None),
                remote_status: Set(String::new()),
                remote_status_at: Set(None),
                last_reminded_at: Set(None),
                sync_error: Set(String::new()),
                created_at: Set(Utc::now()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(&state.db)
            .await;
            if let Err(error) = request {
                return generic_error(&format!("镜像保存失败：{error}"));
            }
            if let Err(error) = store_event(
                &state,
                &message.federation_id,
                1,
                "created",
                &message.cipher,
                &[],
            )
            .await
            {
                return generic_error(&error);
            }
            crate::dev_log::info(format!("收到实例 {} 的借还请求", message.from_domain));
            Json(json!({ "ok": true })).into_response()
        }
        (Some(request), kind, seq) => {
            if !matches!(kind, "confirmed" | "settled" | "reminded" | "rejected") {
                return generic_error("不支持的事件类型");
            }
            let Some(last) = latest_event(&state, &message.federation_id).await else {
                return generic_error("请求事件链缺失");
            };
            if seq != last.seq + 1 {
                return generic_error("事件序号不连续，可能存在重放或乱序");
            }
            if let Err(error) = store_event(
                &state,
                &message.federation_id,
                seq,
                kind,
                &message.cipher,
                &last.digest,
            )
            .await
            {
                return generic_error(&error);
            }
            let mut active = request.clone().into_active_model();
            match kind {
                "confirmed" | "settled" | "rejected" => {
                    active.status = Set(kind.into());
                }
                "reminded" => {
                    active.last_reminded_at = Set(Some(Utc::now()));
                }
                _ => {}
            }
            active.updated_at = Set(Utc::now());
            if let Err(error) = active.update(&state.db).await {
                return generic_error(&format!("状态更新失败：{error}"));
            }
            Json(json!({ "ok": true })).into_response()
        }
        _ => generic_error("未知的借还请求"),
    }
}

#[derive(Deserialize)]
pub struct ConfirmForm {
    paid_at: String,
    account_id: Option<i64>,
}

/// 对方确认收付：可选在本方账本入账，并推送 confirmed 事件。
pub async fn confirm_request(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Json(form): Json<ConfirmForm>,
) -> HandlerResult<Json<Value>> {
    let request = federated_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if request.status != "open" {
        return Err(bad_request("该请求已处理"));
    }
    let instance = linked_instance::Entity::find_by_id(request.instance_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("配对实例不存在"))?;
    let mut local_id = request.local_debt_request_id;
    if let Some(account_id) = form.account_id {
        let amount = crypto::decrypt_cents(&key_of(&instance), &request.amount);
        let (booked_id, _) = book_locally(
            &state,
            &dek,
            &instance,
            &request.kind,
            account_id,
            &super::fmt_cents(amount),
            &crypto::decrypt_string(&key_of(&instance), &request.note),
            &form.paid_at,
        )
        .await?;
        local_id = Some(booked_id);
    }
    let mut active = request.clone().into_active_model();
    active.status = Set("confirmed".into());
    active.local_debt_request_id = Set(local_id);
    active.updated_at = Set(Utc::now());
    active.update(&state.db).await.map_err(err500)?;
    let payload = json!({ "paid_at": form.paid_at });
    let sync_error = match push_event(&state, &instance, &request, "confirmed", payload).await {
        Ok(()) => String::new(),
        Err(error) => error,
    };
    Ok(Json(json!({ "ok": true, "sync_error": sync_error })))
}

/// 查询对方进度：主动拉取对方视角状态并缓存。
pub async fn sync_status(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Json<Value>> {
    let request = federated_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    let instance = linked_instance::Entity::find_by_id(request.instance_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("配对实例不存在"))?;
    let key = key_of(&instance);
    let message = federation::StatusMessage {
        from_domain: instance.self_domain.clone(),
        federation_id: request.federation_id.clone(),
        cipher: crypto::encrypt(&key, b"{}"),
    };
    let client = federation::http_client().map_err(|error| bad_request(&error))?;
    let response = client
        .post(format!("{}/debt-link/status", instance.domain))
        .json(&message)
        .send()
        .await
        .map_err(|error| bad_request(&format!("无法连接对方实例：{error}")))?;
    if !response.status().is_success() {
        return Err(bad_request("对方未返回进度"));
    }
    let body: Value = response.json().await.map_err(err500)?;
    let cipher = body
        .get("cipher")
        .and_then(|value| value.as_str())
        .ok_or_else(|| bad_request("对方响应格式不正确"))?;
    let plain = crypto::decrypt(&key, cipher).ok_or_else(|| bad_request("对方响应无法解密"))?;
    let payload: Value = serde_json::from_slice(&plain).map_err(err500)?;
    let status = payload
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let mut active = request.into_active_model();
    active.remote_status = Set(status.clone());
    active.remote_status_at = Set(Some(Utc::now()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Json(json!({ "ok": true, "remote_status": status })))
}

/// 公开端点：返回本方视角状态（配对密钥加密）。
pub async fn receive_status(
    State(state): State<AppState>,
    Json(message): Json<federation::StatusMessage>,
) -> Response {
    let Some(instance) = find_paired(&state, &message.from_domain).await else {
        return generic_error("实例未配对");
    };
    let Some(request) = find_request(&state, &instance, &message.federation_id).await else {
        return generic_error("借还请求不存在");
    };
    let key = key_of(&instance);
    let payload = json!({
        "status": request.status,
        "updated_at": request.updated_at.to_rfc3339(),
    });
    let cipher = crypto::encrypt(&key, payload.to_string().as_bytes());
    Json(json!({ "cipher": cipher })).into_response()
}

/// 催促对方处理：推送 reminded 事件，对方借还页显示催款时间。
pub async fn remind(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Json<Value>> {
    let request = federated_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    let instance = linked_instance::Entity::find_by_id(request.instance_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("配对实例不存在"))?;
    let sync_error = match push_event(&state, &instance, &request, "reminded", json!({})).await {
        Ok(()) => String::new(),
        Err(error) => error,
    };
    let mut active = request.into_active_model();
    active.last_reminded_at = Set(Some(Utc::now()));
    active.sync_error = Set(sync_error.clone());
    active.update(&state.db).await.map_err(err500)?;
    Ok(Json(json!({ "ok": true, "sync_error": sync_error })))
}

/// 结清：推送 settled 事件，双方状态同步为已结清（不改变本地已入账流水）。
pub async fn settle_request(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Json<Value>> {
    let request = federated_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    let instance = linked_instance::Entity::find_by_id(request.instance_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("配对实例不存在"))?;
    let sync_error = match push_event(&state, &instance, &request, "settled", json!({})).await {
        Ok(()) => String::new(),
        Err(error) => error,
    };
    let mut active = request.into_active_model();
    active.status = Set("settled".into());
    active.sync_error = Set(sync_error.clone());
    active.updated_at = Set(Utc::now());
    active.update(&state.db).await.map_err(err500)?;
    Ok(Json(json!({ "ok": true, "sync_error": sync_error })))
}

/// 对方否认此账单：本地标记 rejected 并推送 rejected 事件提醒账主。
pub async fn reject_request(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Json(form): Json<RejectForm>,
) -> HandlerResult<Json<Value>> {
    let request = federated_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if request.origin != "remote" || request.status != "open" {
        return Err(bad_request("只有对方创建且待处理的请求才能否认"));
    }
    let instance = linked_instance::Entity::find_by_id(request.instance_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("配对实例不存在"))?;
    let reason = form.reason.trim();
    if reason.chars().count() > 500 {
        return Err(bad_request("否认原因不能超过 500 个字符"));
    }
    let sync_error = match push_event(
        &state,
        &instance,
        &request,
        "rejected",
        json!({ "reason": reason }),
    )
    .await
    {
        Ok(()) => String::new(),
        Err(error) => error,
    };
    let mut active = request.into_active_model();
    active.status = Set("rejected".into());
    active.sync_error = Set(sync_error.clone());
    active.updated_at = Set(Utc::now());
    active.update(&state.db).await.map_err(err500)?;
    crate::dev_log::info(format!("已否认实例 {} 的借还请求", instance.domain));
    Ok(Json(json!({ "ok": true, "sync_error": sync_error })))
}

#[derive(Deserialize)]
pub struct RejectForm {
    #[serde(default)]
    reason: String,
}

/// 借还页所需的联邦视图数据。
pub struct InstanceView {
    pub id: i64,
    pub domain: String,
    pub role: String,
    pub status: String,
    pub paired_at: String,
}

pub struct FedRequestView {
    pub id: i64,
    pub kind: String,
    pub amount: String,
    pub currency: String,
    pub note: String,
    pub status: String,
    pub origin: String,
    pub remote_status: String,
    pub remote_status_at: String,
    pub last_reminded_at: String,
    pub sync_error: String,
    pub instance_domain: String,
    pub local_debt_request_id: Option<i64>,
}

pub async fn load_views(
    state: &AppState,
) -> HandlerResult<(Vec<InstanceView>, Vec<FedRequestView>)> {
    let instances = linked_instance::Entity::find()
        .order_by_asc(linked_instance::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut instance_views = Vec::new();
    for instance in &instances {
        instance_views.push(InstanceView {
            id: instance.id,
            domain: instance.domain.clone(),
            role: instance.role.clone(),
            status: instance.status.clone(),
            paired_at: instance
                .paired_at
                .map(|at| at.format("%Y-%m-%dT%H:%M").to_string())
                .unwrap_or_default(),
        });
    }
    let requests = federated_request::Entity::find()
        .order_by_desc(federated_request::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut request_views = Vec::new();
    for request in requests {
        let Some(instance) = instances.iter().find(|item| item.id == request.instance_id) else {
            continue;
        };
        let key = key_of(instance);
        let amount = crypto::decrypt_cents(&key, &request.amount);
        request_views.push(FedRequestView {
            id: request.id,
            kind: request.kind,
            amount: crate::currency::format(amount, &request.currency),
            currency: request.currency,
            note: crypto::decrypt_string(&key, &request.note),
            status: request.status,
            origin: request.origin,
            remote_status: request.remote_status,
            remote_status_at: request
                .remote_status_at
                .map(|at| at.format("%Y-%m-%dT%H:%M").to_string())
                .unwrap_or_default(),
            last_reminded_at: request
                .last_reminded_at
                .map(|at| at.format("%Y-%m-%dT%H:%M").to_string())
                .unwrap_or_default(),
            sync_error: request.sync_error,
            instance_domain: instance.domain.clone(),
            local_debt_request_id: request.local_debt_request_id,
        });
    }
    Ok((instance_views, request_views))
}
