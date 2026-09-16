use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Duration, NaiveDateTime, Utc};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, str::FromStr};
use zeroize::Zeroizing;

use crate::{
    crypto,
    entity::{account, account_detail, debt_person, debt_record, debt_request},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const REQUEST_KEY_CONTEXT: &[u8] = b"haruka debt request key v1\0";
const DIGEST_CONTEXT: &[u8] = b"haruka debt request verification chain v1\0";

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct RequestSnapshot {
    version: u8,
    kind: String,
    kind_label: String,
    amount_cents: i64,
    currency: String,
    #[serde(default)]
    recipient_name: String,
    account_name: String,
    account_kind: String,
    identifier_label: String,
    identifier_masked: String,
    identifier: String,
    note: String,
    created_at: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct PayerSubmission {
    version: u8,
    payer_name: String,
    payer_contact: String,
    payer_account: String,
    amount_cents: i64,
    note: String,
    paid_at: String,
}

struct AccountOption {
    id: i64,
    name: String,
}

struct PersonOption {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "debt_request_form.html")]
struct DebtRequestFormTemplate {
    accounts: Vec<AccountOption>,
    people: Vec<PersonOption>,
    default_expires_at: String,
}

struct RequestView {
    id: i64,
    path: String,
    label: String,
    account_name: String,
    identifier_label: String,
    identifier_masked: String,
    note: String,
    status: String,
    status_label: String,
    expired: bool,
    expires_at: String,
    created_at: String,
    payer_name: String,
    payer_contact: String,
    payer_account: String,
    paid_amount: String,
    payer_note: String,
    paid_at: String,
    has_submission: bool,
    can_confirm: bool,
    can_revoke: bool,
    confirmed_debt_record_id: i64,
    verification_ok: bool,
    verification_code: String,
}

#[derive(Template)]
#[template(path = "debt_requests.html")]
struct DebtRequestsTemplate {
    requests: Vec<RequestView>,
}

#[derive(Template)]
#[template(path = "debt_request_public.html")]
struct DebtRequestPublicTemplate {
    token: String,
    request: RequestSnapshot,
    requested_amount: String,
    amount_input: String,
    expires_at: String,
}

#[derive(Template)]
#[template(path = "debt_request_receipt.html")]
struct DebtRequestReceiptTemplate {
    request: RequestSnapshot,
    submission: PayerSubmission,
    paid_amount: String,
    expires_at: String,
    confirmed: bool,
    verification_code: String,
}

#[derive(Template)]
#[template(path = "shared_bill_unavailable.html")]
struct UnavailableTemplate {}

#[derive(Deserialize)]
pub struct CreateRequestForm {
    kind: String,
    #[serde(default)]
    person_id: i64,
    account_id: i64,
    amount: String,
    recipient_name: String,
    #[serde(default)]
    note: String,
    expires_at: String,
}

#[derive(Deserialize)]
pub struct SubmitRequestForm {
    payer_name: String,
    #[serde(default)]
    payer_contact: String,
    #[serde(default)]
    payer_account: String,
    amount: String,
    #[serde(default)]
    note: String,
    paid_at: String,
}

#[derive(Serialize)]
pub struct AccountIdentifierResponse {
    ok: bool,
    label: String,
    value: String,
}

fn unavailable_identifier_response() -> Response {
    public_response(
        Json(serde_json::json!({
            "ok": false,
            "error": "分享不可用"
        }))
        .into_response(),
    )
}

fn public_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow, noarchive"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

fn render_public<T: Template>(template: T) -> HandlerResult<Response> {
    Ok(public_response(
        Html(template.render().map_err(err500)?).into_response(),
    ))
}

fn request_key(token: &[u8], salt: &[u8]) -> crypto::Dek {
    let mut material = Zeroizing::new(Vec::with_capacity(REQUEST_KEY_CONTEXT.len() + token.len()));
    material.extend_from_slice(REQUEST_KEY_CONTEXT);
    material.extend_from_slice(token);
    crypto::derive_key(material.as_slice(), salt)
}

fn decode_token(token: &str) -> Option<Zeroizing<Vec<u8>>> {
    if token.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
    (bytes.len() == 32).then(|| Zeroizing::new(bytes))
}

fn token_hash(token: &[u8]) -> Vec<u8> {
    Sha256::digest(token).to_vec()
}

fn digest(parts: &[&[u8]]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(DIGEST_CONTEXT);
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().to_vec()
}

fn created_digest(
    token_hash: &[u8],
    account_id: i64,
    person_id: Option<i64>,
    kind: &str,
    snapshot: &str,
    expires_at: NaiveDateTime,
    created_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    digest(&[
        b"created",
        token_hash,
        &account_id.to_be_bytes(),
        &person_id.unwrap_or_default().to_be_bytes(),
        kind.as_bytes(),
        snapshot.as_bytes(),
        expires_at.format(TIME_FMT).to_string().as_bytes(),
        created_at.to_rfc3339().as_bytes(),
    ])
}

fn submitted_digest(
    previous: &[u8],
    submission: &str,
    submitted_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    digest(&[
        b"submitted",
        previous,
        submission.as_bytes(),
        submitted_at.to_rfc3339().as_bytes(),
    ])
}

fn confirmed_digest(
    previous: &[u8],
    record_id: i64,
    confirmed_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    digest(&[
        b"confirmed",
        previous,
        &record_id.to_be_bytes(),
        confirmed_at.to_rfc3339().as_bytes(),
    ])
}

fn digest_code(value: &[u8]) -> String {
    value
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|chunk| chunk.join(""))
        .collect::<Vec<_>>()
        .join("-")
}

fn verify_chain(request: &debt_request::Model) -> bool {
    if request.request_digest
        != created_digest(
            &request.token_hash,
            request.account_id,
            request.person_id,
            &request.kind,
            &request.snapshot,
            request.expires_at,
            request.created_at,
        )
    {
        return false;
    }
    if !request.submission.is_empty() {
        let Some(submitted_at) = request.submitted_at else {
            return false;
        };
        if request.submission_digest
            != submitted_digest(&request.request_digest, &request.submission, submitted_at)
        {
            return false;
        }
    }
    if request.status == "confirmed" {
        let (Some(record_id), Some(confirmed_at)) =
            (request.confirmed_debt_record_id, request.confirmed_at)
        else {
            return false;
        };
        if request.confirmation_digest
            != confirmed_digest(&request.submission_digest, record_id, confirmed_at)
        {
            return false;
        }
    }
    true
}

fn chain_head(request: &debt_request::Model) -> &[u8] {
    if !request.confirmation_digest.is_empty() {
        &request.confirmation_digest
    } else if !request.submission_digest.is_empty() {
        &request.submission_digest
    } else {
        &request.request_digest
    }
}

fn parse_amount(value: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(value.trim())
        .map_err(|_| bad_request("金额格式不正确"))?
        .round_dp(2);
    if decimal <= Decimal::ZERO {
        return Err(bad_request("金额必须大于 0"));
    }
    (decimal * Decimal::from(100))
        .to_i64()
        .ok_or_else(|| bad_request("金额超出范围"))
}

fn valid_kind(kind: &str) -> bool {
    matches!(kind, "borrow" | "repayment_received")
}

fn kind_label(kind: &str) -> &'static str {
    if kind == "repayment_received" {
        "请对方归还欠款"
    } else {
        "向对方借款"
    }
}

fn account_kind_label(kind: &str) -> &'static str {
    match kind {
        "payment" => "支付账户",
        "bank" => "银行账户",
        "stored_value" => "储值卡",
        "credit_card" => "信用卡",
        "credit_service" => "信贷服务",
        "investment" => "投资账户",
        _ => "其他账户",
    }
}

fn shared_account_identifier(
    dek: &crypto::Dek,
    detail: Option<&account_detail::Model>,
) -> Option<(String, String, String)> {
    let detail = detail?;
    let card = crypto::decrypt_string(dek, &detail.card_number);
    if !card.is_empty() {
        return Some(("卡号".into(), super::mask_card_number(&card), card));
    }
    let username = crypto::decrypt_string(dek, &detail.account_username);
    (!username.is_empty()).then(|| {
        (
            "账户用户名".into(),
            super::mask_account_username(&username),
            username,
        )
    })
}

fn decrypt_snapshot(request: &debt_request::Model, token: &[u8]) -> Option<RequestSnapshot> {
    let key = request_key(token, &request.salt);
    let plaintext = crypto::decrypt(&key, &request.snapshot)?;
    let snapshot = serde_json::from_slice::<RequestSnapshot>(&plaintext).ok()?;
    (snapshot.version == 1).then_some(snapshot)
}

fn decrypt_submission(request: &debt_request::Model, token: &[u8]) -> Option<PayerSubmission> {
    if request.submission.is_empty() {
        return None;
    }
    let key = request_key(token, &request.salt);
    let plaintext = crypto::decrypt(&key, &request.submission)?;
    let submission = serde_json::from_slice::<PayerSubmission>(&plaintext).ok()?;
    (submission.version == 1).then_some(submission)
}

async fn find_public_request(
    state: &AppState,
    token: &str,
) -> HandlerResult<Option<(debt_request::Model, Zeroizing<Vec<u8>>)>> {
    let Some(bytes) = decode_token(token) else {
        return Ok(None);
    };
    let request = debt_request::Entity::find()
        .filter(debt_request::Column::TokenHash.eq(token_hash(bytes.as_slice())))
        .one(&state.db)
        .await
        .map_err(err500)?;
    Ok(request
        .filter(|item| item.status != "revoked" && item.expires_at > Utc::now().naive_utc())
        .map(|item| (item, bytes)))
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|item| shared_account_identifier(&dek, details.get(&item.id)).is_some())
        .map(|item| AccountOption {
            id: item.id,
            name: super::bills::account_display_name(&dek, &item, details.get(&item.id)),
        })
        .collect();
    let people = debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|person| PersonOption {
            id: person.id,
            name: crypto::decrypt_string(&dek, &person.name),
        })
        .collect();
    Ok(Html(
        DebtRequestFormTemplate {
            accounts,
            people,
            default_expires_at: (Utc::now() + Duration::days(7))
                .format(TIME_FMT)
                .to_string(),
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<CreateRequestForm>,
) -> HandlerResult<Redirect> {
    if !valid_kind(&form.kind) {
        return Err(bad_request("请求类型无效"));
    }
    if form.kind == "repayment_received" && form.person_id == 0 {
        return Err(bad_request("请对方还款时必须选择已有借贷对象"));
    }
    let person_id = (form.person_id != 0).then_some(form.person_id);
    if let Some(id) = person_id {
        if debt_person::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .is_none()
        {
            return Err(bad_request("借贷对象不存在"));
        }
    }
    let account = account::Entity::find_by_id(form.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("收款账户不存在"))?;
    let detail = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let (identifier_label, identifier_masked, identifier) =
        shared_account_identifier(&dek, detail.as_ref())
            .ok_or_else(|| bad_request("该账户没有可分享的卡号或账户用户名"))?;
    let amount_cents = parse_amount(&form.amount)?;
    let recipient_name = form.recipient_name.trim();
    if recipient_name.is_empty() || recipient_name.chars().count() > 100 {
        return Err(bad_request("请填写 1 到 100 个字符的收款人姓名或户名"));
    }
    let expires_at = NaiveDateTime::parse_from_str(form.expires_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("有效期结束时间格式不正确"))?;
    if expires_at <= Utc::now().naive_utc() {
        return Err(bad_request("有效期结束时间必须晚于现在"));
    }
    let created_at = Utc::now();
    let snapshot = RequestSnapshot {
        version: 1,
        kind: form.kind.clone(),
        kind_label: kind_label(&form.kind).into(),
        amount_cents,
        currency: account.currency.clone(),
        recipient_name: recipient_name.into(),
        account_name: crypto::decrypt_string(&dek, &account.name),
        account_kind: account_kind_label(&account.kind).into(),
        identifier_label,
        identifier_masked,
        identifier,
        note: form.note.trim().to_string(),
        created_at: created_at.format(TIME_FMT).to_string(),
    };
    let token_bytes = Zeroizing::new(crypto::random_bytes::<32>());
    let token = URL_SAFE_NO_PAD.encode(token_bytes.as_slice());
    let token_hash = token_hash(token_bytes.as_slice());
    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>();
    let key = request_key(token_bytes.as_slice(), &salt);
    let snapshot_cipher = crypto::encrypt(&key, &serde_json::to_vec(&snapshot).map_err(err500)?);
    let request_digest = created_digest(
        &token_hash,
        account.id,
        person_id,
        &form.kind,
        &snapshot_cipher,
        expires_at,
        created_at,
    );
    let label = format!(
        "{} · {}",
        snapshot.kind_label,
        crate::currency::format(amount_cents, &snapshot.currency)
    );
    let created = debt_request::ActiveModel {
        person_id: Set(person_id),
        account_id: Set(account.id),
        kind: Set(form.kind),
        token_hash: Set(token_hash),
        token: Set(crypto::encrypt(&dek, token.as_bytes())),
        label: Set(crypto::encrypt(&dek, label.as_bytes())),
        salt: Set(salt.to_vec()),
        snapshot: Set(snapshot_cipher),
        submission: Set(String::new()),
        request_digest: Set(request_digest),
        submission_digest: Set(Vec::new()),
        confirmation_digest: Set(Vec::new()),
        status: Set("open".into()),
        expires_at: Set(expires_at),
        submitted_at: Set(None),
        confirmed_at: Set(None),
        confirmed_debt_record_id: Set(None),
        created_at: Set(created_at),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to(&format!(
        "/debt-requests?created={}",
        created.id
    )))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let now = Utc::now().naive_utc();
    let mut views = Vec::new();
    for request in debt_request::Entity::find()
        .order_by_desc(debt_request::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let token = crypto::decrypt_string(&dek, &request.token);
        let Some(token_bytes) = decode_token(&token) else {
            continue;
        };
        let snapshot = decrypt_snapshot(&request, token_bytes.as_slice()).unwrap_or_default();
        let submission = decrypt_submission(&request, token_bytes.as_slice());
        let expired = request.expires_at <= now;
        let status_label = match request.status.as_str() {
            "submitted" => "等待确认",
            "confirmed" => "已确认到账",
            "revoked" => "已撤销",
            _ if expired => "已过期",
            _ => "等待打款人填写",
        };
        let has_submission = submission.is_some();
        let submitted = submission.unwrap_or_default();
        views.push(RequestView {
            id: request.id,
            path: format!("/r/{token}"),
            label: crypto::decrypt_string(&dek, &request.label),
            account_name: snapshot.account_name,
            identifier_label: snapshot.identifier_label,
            identifier_masked: snapshot.identifier_masked,
            note: snapshot.note,
            status: request.status.clone(),
            status_label: status_label.into(),
            expired,
            expires_at: request.expires_at.format(TIME_FMT).to_string(),
            created_at: request.created_at.format(TIME_FMT).to_string(),
            payer_name: submitted.payer_name,
            payer_contact: submitted.payer_contact,
            payer_account: submitted.payer_account,
            paid_amount: if has_submission {
                crate::currency::format(submitted.amount_cents, &snapshot.currency)
            } else {
                String::new()
            },
            payer_note: submitted.note,
            paid_at: submitted.paid_at,
            has_submission,
            can_confirm: request.status == "submitted" && verify_chain(&request),
            can_revoke: matches!(request.status.as_str(), "open" | "submitted"),
            confirmed_debt_record_id: request.confirmed_debt_record_id.unwrap_or_default(),
            verification_ok: verify_chain(&request),
            verification_code: digest_code(chain_head(&request)),
        });
    }
    Ok(Html(
        DebtRequestsTemplate { requests: views }
            .render()
            .map_err(err500)?,
    ))
}

pub async fn open(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> HandlerResult<Response> {
    let Some((request, token_bytes)) = find_public_request(&state, &token).await? else {
        return render_public(UnavailableTemplate {});
    };
    if !verify_chain(&request) {
        return render_public(UnavailableTemplate {});
    }
    let Some(snapshot) = decrypt_snapshot(&request, token_bytes.as_slice()) else {
        return render_public(UnavailableTemplate {});
    };
    if request.status == "submitted" || request.status == "confirmed" {
        let Some(submission) = decrypt_submission(&request, token_bytes.as_slice()) else {
            return render_public(UnavailableTemplate {});
        };
        return render_public(DebtRequestReceiptTemplate {
            paid_amount: crate::currency::format(submission.amount_cents, &snapshot.currency),
            request: snapshot,
            submission,
            expires_at: request.expires_at.format(TIME_FMT).to_string(),
            confirmed: request.status == "confirmed",
            verification_code: digest_code(chain_head(&request)),
        });
    }
    render_public(DebtRequestPublicTemplate {
        token,
        requested_amount: crate::currency::format(snapshot.amount_cents, &snapshot.currency),
        amount_input: super::fmt_cents(snapshot.amount_cents),
        request: snapshot,
        expires_at: request.expires_at.format(TIME_FMT).to_string(),
    })
}

pub async fn account_identifier(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> HandlerResult<Response> {
    let Some((request, token_bytes)) = find_public_request(&state, &token).await? else {
        return Ok(unavailable_identifier_response());
    };
    if !verify_chain(&request) {
        return Ok(unavailable_identifier_response());
    }
    let Some(snapshot) = decrypt_snapshot(&request, token_bytes.as_slice()) else {
        return Ok(unavailable_identifier_response());
    };
    Ok(public_response(
        Json(AccountIdentifierResponse {
            ok: true,
            label: snapshot.identifier_label,
            value: snapshot.identifier,
        })
        .into_response(),
    ))
}

pub async fn submit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Form(form): Form<SubmitRequestForm>,
) -> HandlerResult<Response> {
    let _write_guard = state.balance_writes.lock().await;
    let Some((request, token_bytes)) = find_public_request(&state, &token).await? else {
        return render_public(UnavailableTemplate {});
    };
    if request.status != "open" || !verify_chain(&request) {
        return render_public(UnavailableTemplate {});
    }
    let Some(snapshot) = decrypt_snapshot(&request, token_bytes.as_slice()) else {
        return render_public(UnavailableTemplate {});
    };
    let payer_name = form.payer_name.trim();
    if payer_name.is_empty() || payer_name.chars().count() > 100 {
        return Err(bad_request("请填写 1 到 100 个字符的打款人姓名"));
    }
    if form.payer_contact.chars().count() > 200
        || form.payer_account.chars().count() > 200
        || form.note.chars().count() > 1000
    {
        return Err(bad_request("提交的信息过长"));
    }
    let amount_cents = parse_amount(&form.amount)?;
    let paid_at = NaiveDateTime::parse_from_str(form.paid_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("打款时间格式不正确"))?;
    let submission = PayerSubmission {
        version: 1,
        payer_name: payer_name.into(),
        payer_contact: form.payer_contact.trim().into(),
        payer_account: form.payer_account.trim().into(),
        amount_cents,
        note: form.note.trim().into(),
        paid_at: paid_at.format(TIME_FMT).to_string(),
    };
    let key = request_key(token_bytes.as_slice(), &request.salt);
    let cipher = crypto::encrypt(&key, &serde_json::to_vec(&submission).map_err(err500)?);
    let submitted_at = Utc::now();
    let submission_digest = submitted_digest(&request.request_digest, &cipher, submitted_at);
    let expires_at = request.expires_at.format(TIME_FMT).to_string();
    let mut active = request.into_active_model();
    active.submission = Set(cipher);
    active.submission_digest = Set(submission_digest.clone());
    active.status = Set("submitted".into());
    active.submitted_at = Set(Some(submitted_at));
    active.update(&state.db).await.map_err(err500)?;
    render_public(DebtRequestReceiptTemplate {
        paid_amount: crate::currency::format(amount_cents, &snapshot.currency),
        request: snapshot,
        submission,
        expires_at,
        confirmed: false,
        verification_code: digest_code(&submission_digest),
    })
}

pub async fn confirm(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let request = debt_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if request.status != "submitted" || !verify_chain(&request) {
        return Err(bad_request("请求尚未提交、已经处理或验证链不完整"));
    }
    let token = crypto::decrypt_string(&dek, &request.token);
    let token_bytes = decode_token(&token).ok_or_else(|| err500("请求令牌无法解密"))?;
    let snapshot = decrypt_snapshot(&request, token_bytes.as_slice())
        .ok_or_else(|| err500("请求快照无法解密"))?;
    let submission = decrypt_submission(&request, token_bytes.as_slice())
        .ok_or_else(|| err500("打款信息无法解密"))?;
    let account = account::Entity::find_by_id(request.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("收款账户已不存在"))?;
    if account.currency != snapshot.currency {
        return Err(bad_request("收款账户货币已经变化，不能确认"));
    }
    if request.kind == "repayment_received" {
        let person_id = request
            .person_id
            .ok_or_else(|| bad_request("还款请求没有关联借贷对象"))?;
        let (receivable, _) = super::debts::person_outstanding(&state, &dek, person_id).await?;
        let amount_in_default = crate::currency::convert_cents(
            &state,
            submission.amount_cents,
            &account.currency,
            &crate::currency::default_currency(&state)
                .await
                .map_err(err500)?,
            chrono::Local::now().date_naive(),
        )
        .await
        .map_err(err500)?;
        if amount_in_default > receivable {
            return Err(bad_request("打款金额超过对方当前尚欠金额"));
        }
    }
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        request.account_id,
        submission.amount_cents,
    )
    .await?;

    let transaction = state.db.begin().await.map_err(err500)?;
    let person_id = if let Some(person_id) = request.person_id {
        if debt_person::Entity::find_by_id(person_id)
            .one(&transaction)
            .await
            .map_err(err500)?
            .is_none()
        {
            return Err(bad_request("关联的借贷对象已不存在"));
        }
        person_id
    } else {
        let mut person_note = Vec::new();
        if !submission.payer_contact.is_empty() {
            person_note.push(format!("联系方式：{}", submission.payer_contact));
        }
        if !submission.payer_account.is_empty() {
            person_note.push(format!("打款账户：{}", submission.payer_account));
        }
        debt_person::ActiveModel {
            name: Set(crypto::encrypt(&dek, submission.payer_name.as_bytes())),
            note: Set(crypto::encrypt(&dek, person_note.join("\n").as_bytes())),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&transaction)
        .await
        .map_err(err500)?
        .id
    };
    let note = [snapshot.note, submission.note]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let record = debt_record::ActiveModel {
        person_id: Set(person_id),
        account_id: Set(request.account_id),
        kind: Set(request.kind.clone()),
        amount: Set(crypto::encrypt_cents(&dek, submission.amount_cents)),
        note: Set(crypto::encrypt(&dek, note.as_bytes())),
        happened_at: Set(NaiveDateTime::parse_from_str(&submission.paid_at, TIME_FMT)
            .map_err(|_| err500("打款时间无法解析"))?),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    let confirmed_at = Utc::now();
    let confirmation_digest = confirmed_digest(&request.submission_digest, record.id, confirmed_at);
    let mut active = request.into_active_model();
    active.status = Set("confirmed".into());
    active.confirmed_at = Set(Some(confirmed_at));
    active.confirmed_debt_record_id = Set(Some(record.id));
    active.confirmation_digest = Set(confirmation_digest);
    active.update(&transaction).await.map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to("/debt-requests"))
}

pub async fn revoke(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    let request = debt_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if !matches!(request.status.as_str(), "open" | "submitted") {
        return Err(bad_request("该请求不能再撤销"));
    }
    let mut active = request.into_active_model();
    active.status = Set("revoked".into());
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/debt-requests"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    debt_request::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/debt-requests"))
}
