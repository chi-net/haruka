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
pub(crate) const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
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
    currency: String,
}

struct PersonOption {
    id: i64,
    name: String,
}

struct OriginOption {
    id: i64,
    person_id: i64,
    kind: String,
    currency: String,
    label: String,
}

struct RepaymentAccountOption {
    id: i64,
    name: String,
    currency: String,
}

#[derive(Template)]
#[template(path = "debt_request_form.html")]
struct DebtRequestFormTemplate {
    accounts: Vec<AccountOption>,
    people: Vec<PersonOption>,
    origins: Vec<OriginOption>,
    default_paid_at: String,
    default_expires_at: String,
}

struct RequestView {
    id: i64,
    path: String,
    label: String,
    account_name: String,
    note: String,
    status_label: String,
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
    can_print: bool,
    can_submit_repayment: bool,
    can_settle: bool,
    settled: bool,
    settlement_amount: String,
    settlement_note: String,
    settled_at: String,
    origin_settled: bool,
    can_revoke: bool,
    repayment_pending: bool,
    repaid: bool,
    repayment_at: String,
    repayment_note: String,
    repayment_account_name: String,
    confirmed_debt_record_id: i64,
    verification_ok: bool,
    verification_code: String,
    requires_confirmation: bool,
    incoming: bool,
    origin_debt_record_id: i64,
}

#[derive(Template)]
#[template(path = "debt_requests.html")]
struct DebtRequestsTemplate {
    requests: Vec<RequestView>,
    repayment_accounts: Vec<RepaymentAccountOption>,
    default_repayment_at: String,
    linked_instances: Vec<super::federation::InstanceView>,
    federated: Vec<super::federation::FedRequestView>,
}

#[derive(Template)]
#[template(path = "debt_request_public.html")]
struct DebtRequestPublicTemplate {
    token: String,
    request: RequestSnapshot,
    requested_amount: String,
    amount_input: String,
    expires_at: String,
    incoming: bool,
}

struct ProofTime {
    label: &'static str,
    utc: String,
    local: String,
}

fn proof_time(
    label: &'static str,
    utc: NaiveDateTime,
    time_zone: super::ClientTimeZone,
) -> ProofTime {
    ProofTime {
        label,
        utc: utc.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        local: time_zone
            .local_datetime(utc)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
    }
}

#[derive(Template)]
#[template(path = "debt_request_receipt.html")]
struct DebtRequestReceiptTemplate {
    token: String,
    request: RequestSnapshot,
    submission: PayerSubmission,
    paid_amount: String,
    requested_amount: String,
    confirmed: bool,
    is_loan: bool,
    repayment_pending: bool,
    repaid: bool,
    repayment_note: String,
    repayment_amount: String,
    settled: bool,
    settlement_amount: String,
    settlement_note: String,
    verification_code: String,
    requires_confirmation: bool,
    incoming: bool,
    origin_debt_record_id: i64,
    confirmed_debt_record_id: i64,
    repayment_debt_record_id: i64,
    proof_version: i32,
    times: Vec<ProofTime>,
    time_zone: &'static str,
    style_nonce: String,
}

#[derive(Template)]
#[template(path = "shared_bill_unavailable.html")]
struct UnavailableTemplate {}

#[derive(Deserialize)]
pub struct CreateRequestForm {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) person_id: i64,
    pub(crate) account_id: i64,
    pub(crate) amount: String,
    pub(crate) recipient_name: String,
    #[serde(default)]
    pub(crate) note: String,
    pub(crate) expires_at: String,
    /// Omitted by legacy clients means the old, confirmation-required policy.
    #[serde(default)]
    pub(crate) requires_confirmation: Option<bool>,
    #[serde(default)]
    pub(crate) origin_debt_record_id: Option<i64>,
    #[serde(default)]
    pub(crate) paid_at: String,
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

#[derive(Deserialize)]
pub struct SubmitRepaymentForm {
    account_id: i64,
    repaid_at: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    requires_confirmation: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct RepaymentSubmission {
    version: u8,
    amount_cents: i64,
    repaid_at: String,
    note: String,
}

#[derive(Deserialize)]
pub struct SettleForm {
    #[serde(default)]
    note: String,
}

#[derive(Serialize, Deserialize)]
struct Settlement {
    version: u8,
    amount_cents: i64,
    note: String,
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

fn render_receipt(
    request: &debt_request::Model,
    token: String,
    token_bytes: &[u8],
    snapshot: RequestSnapshot,
    submission: PayerSubmission,
    time_zone: super::ClientTimeZone,
) -> HandlerResult<Response> {
    let Ok(paid_at) = NaiveDateTime::parse_from_str(&submission.paid_at, TIME_FMT) else {
        return render_public(UnavailableTemplate {});
    };
    let mut times = vec![
        proof_time("请求创建", request.created_at.naive_utc(), time_zone),
        proof_time(
            if request.requires_confirmation {
                "实际收付（对方声明）"
            } else {
                "实际收付（账主单方陈述）"
            },
            paid_at,
            time_zone,
        ),
    ];
    if let Some(at) = request.submitted_at {
        times.push(proof_time(
            if request.requires_confirmation {
                "对方提交 / 确认收付"
            } else {
                "账主登记单方陈述"
            },
            at.naive_utc(),
            time_zone,
        ));
    }
    if let Some(at) = request.confirmed_at {
        times.push(proof_time("账主核实入账", at.naive_utc(), time_zone));
    }
    let repayment = if matches!(request.status.as_str(), "repayment_submitted" | "repaid") {
        let Some(repayment) = decrypt_repayment(request, token_bytes) else {
            return render_public(UnavailableTemplate {});
        };
        let Ok(at) = NaiveDateTime::parse_from_str(&repayment.repaid_at, TIME_FMT) else {
            return render_public(UnavailableTemplate {});
        };
        times.push(proof_time("历史还款收付（账主登记）", at, time_zone));
        Some(repayment)
    } else {
        None
    };
    if let Some(at) = request.repayment_submitted_at {
        times.push(proof_time("历史还款登记", at.naive_utc(), time_zone));
    }
    if let Some(at) = request.verified_at {
        times.push(proof_time(
            "对方确认收到历史还款",
            at.naive_utc(),
            time_zone,
        ));
    }
    let settlement = if let Some(at) = request.settled_at {
        let Some(settlement) = decrypt_settlement(request, token_bytes) else {
            return render_public(UnavailableTemplate {});
        };
        times.push(proof_time("账主单方强制平账", at.naive_utc(), time_zone));
        Some(settlement)
    } else {
        None
    };
    times.push(proof_time("原链接有效期", request.expires_at, time_zone));
    let style_nonce = URL_SAFE_NO_PAD.encode(crypto::random_bytes::<16>());
    // Authorize only this response's business-template style block, not arbitrary inline CSS.
    let policy = HeaderValue::from_str(&format!(
        "default-src 'none'; style-src 'self' 'nonce-{style_nonce}'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    ))
    .map_err(err500)?;
    let mut response = render_public(DebtRequestReceiptTemplate {
        token,
        paid_amount: crate::currency::format(submission.amount_cents, &snapshot.currency),
        requested_amount: crate::currency::format(snapshot.amount_cents, &snapshot.currency),
        settled: request.settled_at.is_some(),
        settlement_amount: settlement
            .as_ref()
            .map(|item| crate::currency::format(item.amount_cents, &snapshot.currency))
            .unwrap_or_default(),
        settlement_note: settlement.map(|item| item.note).unwrap_or_default(),
        repayment_amount: repayment
            .as_ref()
            .map(|repayment| crate::currency::format(repayment.amount_cents, &snapshot.currency))
            .unwrap_or_default(),
        repayment_note: repayment
            .map(|repayment| repayment.note)
            .unwrap_or_default(),
        is_loan: matches!(request.kind.as_str(), "borrow" | "lend"),
        request: snapshot,
        submission,
        confirmed: request.status != "submitted",
        repayment_pending: request.status == "repayment_submitted",
        repaid: request.status == "repaid",
        verification_code: digest_code(chain_head(request)),
        requires_confirmation: request.requires_confirmation,
        incoming: incoming(&request.kind),
        origin_debt_record_id: request.origin_debt_record_id.unwrap_or_default(),
        confirmed_debt_record_id: request.confirmed_debt_record_id.unwrap_or_default(),
        repayment_debt_record_id: request.repayment_debt_record_id.unwrap_or_default(),
        proof_version: request.proof_version,
        times,
        time_zone: time_zone.0.name(),
        style_nonce,
    })?;
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, policy);
    Ok(response)
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

fn policy_digest(
    legacy: &[u8],
    requires_confirmation: bool,
    origin_debt_record_id: Option<i64>,
) -> Vec<u8> {
    digest(&[
        b"created-v2",
        legacy,
        &[u8::from(requires_confirmation)],
        &origin_debt_record_id.unwrap_or_default().to_be_bytes(),
    ])
}

fn booking_digest(
    requires_confirmation: bool,
    previous: &[u8],
    record_id: i64,
    confirmed_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    if requires_confirmation {
        confirmed_digest(previous, record_id, confirmed_at)
    } else {
        digest(&[
            b"unilateral-recorded",
            previous,
            &record_id.to_be_bytes(),
            confirmed_at.to_rfc3339().as_bytes(),
        ])
    }
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

fn repayment_digest(
    previous: &[u8],
    repayment: &str,
    repayment_account_id: i64,
    repayment_record_id: i64,
    submitted_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    digest(&[
        b"repayment-submitted",
        previous,
        repayment.as_bytes(),
        &repayment_account_id.to_be_bytes(),
        &repayment_record_id.to_be_bytes(),
        submitted_at.to_rfc3339().as_bytes(),
    ])
}

fn verified_digest(previous: &[u8], verified_at: chrono::DateTime<Utc>) -> Vec<u8> {
    digest(&[
        b"repayment-received",
        previous,
        verified_at.to_rfc3339().as_bytes(),
    ])
}

fn settlement_digest(
    previous: &[u8],
    settlement: &str,
    settled_amount: &str,
    settled_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    digest(&[
        b"unilateral-settlement",
        previous,
        settlement.as_bytes(),
        settled_amount.as_bytes(),
        settled_at.to_rfc3339().as_bytes(),
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
    let legacy = created_digest(
        &request.token_hash,
        request.account_id,
        request.person_id,
        &request.kind,
        &request.snapshot,
        request.expires_at,
        request.created_at,
    );
    let expected = match request.proof_version {
        1 if request.requires_confirmation && request.origin_debt_record_id.is_none() => legacy,
        2 => policy_digest(
            &legacy,
            request.requires_confirmation,
            request.origin_debt_record_id,
        ),
        _ => return false,
    };
    if request.request_digest != expected
        || !valid_kind(&request.kind)
        || !matches!(
            request.status.as_str(),
            "open"
                | "submitted"
                | "confirmed"
                | "recorded"
                | "repayment_submitted"
                | "repaid"
                | "revoked"
        )
        || (!request.requires_confirmation && request.status != "recorded")
        || (request.requires_confirmation && request.status == "recorded")
        || (request.proof_version == 2
            && (is_repayment(&request.kind) != request.origin_debt_record_id.is_some()))
    {
        return false;
    }
    if request.status != "open" && request.status != "revoked" && request.submission.is_empty() {
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
    if matches!(
        request.status.as_str(),
        "confirmed" | "recorded" | "repayment_submitted" | "repaid"
    ) {
        let (Some(record_id), Some(confirmed_at)) =
            (request.confirmed_debt_record_id, request.confirmed_at)
        else {
            return false;
        };
        if request.confirmation_digest
            != booking_digest(
                request.requires_confirmation,
                &request.submission_digest,
                record_id,
                confirmed_at,
            )
        {
            return false;
        }
    }
    if matches!(request.status.as_str(), "repayment_submitted" | "repaid") {
        let (Some(submitted_at), Some(account_id), Some(record_id)) = (
            request.repayment_submitted_at,
            request.repayment_account_id,
            request.repayment_debt_record_id,
        ) else {
            return false;
        };
        if request.repayment_submission.is_empty()
            || request.repayment_digest
                != repayment_digest(
                    &request.confirmation_digest,
                    &request.repayment_submission,
                    account_id,
                    record_id,
                    submitted_at,
                )
        {
            return false;
        }
    }
    if request.status == "repaid" {
        let Some(verified_at) = request.verified_at else {
            return false;
        };
        if request.verification_digest != verified_digest(&request.repayment_digest, verified_at) {
            return false;
        }
    }
    if let Some(at) = request.settled_at {
        if !matches!(request.kind.as_str(), "borrow" | "lend")
            || request.confirmed_debt_record_id.is_none()
            || !matches!(
                request.status.as_str(),
                "confirmed" | "recorded" | "repayment_submitted" | "repaid"
            )
            || request.settlement.is_empty()
            || request.settled_amount.is_empty()
            || request.settlement_digest
                != settlement_digest(
                    booking_chain_head(request),
                    &request.settlement,
                    &request.settled_amount,
                    at,
                )
        {
            return false;
        }
    } else if !request.settlement.is_empty()
        || !request.settled_amount.is_empty()
        || !request.settlement_digest.is_empty()
    {
        return false;
    }
    true
}

fn chain_head(request: &debt_request::Model) -> &[u8] {
    if request.settled_at.is_some() {
        &request.settlement_digest
    } else {
        booking_chain_head(request)
    }
}

fn booking_chain_head(request: &debt_request::Model) -> &[u8] {
    if !request.verification_digest.is_empty() {
        &request.verification_digest
    } else if !request.repayment_digest.is_empty() {
        &request.repayment_digest
    } else if !request.confirmation_digest.is_empty() {
        &request.confirmation_digest
    } else if !request.submission_digest.is_empty() {
        &request.submission_digest
    } else {
        &request.request_digest
    }
}

pub(crate) fn parse_amount(value: &str) -> HandlerResult<i64> {
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
    matches!(
        kind,
        "borrow" | "lend" | "repayment_received" | "repayment_paid"
    )
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "borrow" => "我向对方借款",
        "lend" => "我借给对方",
        "repayment_received" => "对方还给我",
        "repayment_paid" => "我还给对方",
        _ => "借还请求",
    }
}

fn incoming(kind: &str) -> bool {
    matches!(kind, "borrow" | "repayment_received")
}

fn is_repayment(kind: &str) -> bool {
    matches!(kind, "repayment_received" | "repayment_paid")
}

fn account_delta(kind: &str, amount: i64) -> HandlerResult<i64> {
    if incoming(kind) {
        Ok(amount)
    } else {
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))
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
        "investment_fund" => "基金",
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
    let mut snapshot = serde_json::from_slice::<RequestSnapshot>(&plaintext).ok()?;
    if !matches!(snapshot.version, 1 | 2) {
        return None;
    }
    // Render old encrypted snapshots with today's display rules without changing
    // a byte of their ciphertext or their verification chain.
    let suffix = if snapshot.identifier_label == "卡号" {
        super::mask_card_number(&snapshot.identifier)
    } else {
        super::mask_account_username(&snapshot.identifier)
    };
    if snapshot.version == 1 && !suffix.is_empty() {
        snapshot.account_name = format!("{}({suffix})", snapshot.account_name);
    }
    snapshot.identifier_masked = suffix;
    snapshot.kind_label = kind_label(&snapshot.kind).into();
    Some(snapshot)
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

fn decrypt_repayment(request: &debt_request::Model, token: &[u8]) -> Option<RepaymentSubmission> {
    if request.repayment_submission.is_empty() {
        return None;
    }
    let key = request_key(token, &request.salt);
    let plaintext = crypto::decrypt(&key, &request.repayment_submission)?;
    let repayment = serde_json::from_slice::<RepaymentSubmission>(&plaintext).ok()?;
    (repayment.version == 1).then_some(repayment)
}

fn decrypt_settlement(request: &debt_request::Model, token: &[u8]) -> Option<Settlement> {
    let key = request_key(token, &request.salt);
    let plaintext = crypto::decrypt(&key, &request.settlement)?;
    let settlement: Settlement = serde_json::from_slice(&plaintext).ok()?;
    (settlement.version == 1 && settlement.amount_cents > 0).then_some(settlement)
}

pub(super) async fn settled_amounts(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<HashMap<i64, i64>> {
    let mut amounts = HashMap::new();
    for request in debt_request::Entity::find()
        .filter(debt_request::Column::SettledAt.is_not_null())
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        if !verify_chain(&request) {
            return Err(err500("借款结清验证链异常"));
        }
        let amount = crypto::decrypt_string(dek, &request.settled_amount)
            .parse::<i64>()
            .map_err(|_| err500("借款结清金额无法解密"))?;
        let id = request
            .confirmed_debt_record_id
            .ok_or_else(|| err500("结清记录缺少原借款"))?;
        if amount < 0 || amounts.insert(id, amount).is_some() {
            return Err(err500("借款结清金额或关联异常"));
        }
    }
    Ok(amounts)
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
    let Some(request) = request.filter(|item| {
        item.status != "revoked"
            && (item.expires_at > Utc::now().naive_utc()
                || matches!(
                    item.status.as_str(),
                    "confirmed" | "recorded" | "repayment_submitted" | "repaid"
                ))
    }) else {
        return Ok(None);
    };
    if request.confirmed_debt_record_id.is_none() {
        if let Some(origin) = request.origin_debt_record_id {
            if debt_request::Entity::find()
                .filter(debt_request::Column::ConfirmedDebtRecordId.eq(origin))
                .filter(debt_request::Column::SettledAt.is_not_null())
                .one(&state.db)
                .await
                .map_err(err500)?
                .is_some()
            {
                return Ok(None);
            }
        }
    }
    Ok(Some((request, bytes)))
}

/// The caller holds balance_writes. Historical unbound repayments consume the
/// same-currency person cap, but are never reassigned to an arbitrary old loan.
async fn repayment_capacity(
    state: &AppState,
    dek: &crypto::Dek,
    kind: &str,
    person_id: i64,
    currency: &str,
    origin_id: Option<i64>,
) -> HandlerResult<i64> {
    let original_kind = match kind {
        "repayment_received" => "lend",
        "repayment_paid" => "borrow",
        _ => return Err(bad_request("不是还款请求")),
    };
    let accounts: HashMap<i64, account::Model> = account::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|item| (item.id, item))
        .collect();
    let records = debt_record::Entity::find()
        .filter(debt_record::Column::PersonId.eq(person_id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let origin = if let Some(id) = origin_id {
        let item = records
            .iter()
            .find(|record| record.id == id)
            .ok_or_else(|| bad_request("原借款不存在或不属于该借贷对象"))?;
        if item.kind != original_kind
            || accounts
                .get(&item.account_id)
                .map(|account| account.currency.as_str())
                != Some(currency)
        {
            return Err(bad_request("原借款方向或货币与还款不匹配"));
        }
        Some(item)
    } else {
        None
    };
    let settlements = settled_amounts(state, dek).await?;
    if origin.is_some_and(|item| settlements.contains_key(&item.id)) {
        return Ok(0);
    }
    let mut outstanding = 0_i64;
    let mut repayment_amounts = HashMap::new();
    for record in &records {
        if accounts
            .get(&record.account_id)
            .map(|account| account.currency.as_str())
            != Some(currency)
        {
            continue;
        }
        let amount = crypto::decrypt_cents(dek, &record.amount)
            .checked_sub(settlements.get(&record.id).copied().unwrap_or_default())
            .ok_or_else(|| err500("借款结清金额超出范围"))?;
        if record.kind == original_kind {
            outstanding = outstanding
                .checked_add(amount)
                .ok_or_else(|| bad_request("借款金额超出范围"))?;
        } else if record.kind == kind {
            outstanding = outstanding
                .checked_sub(amount)
                .ok_or_else(|| bad_request("还款金额超出范围"))?;
            repayment_amounts.insert(record.id, amount);
        }
    }
    let Some(origin) = origin else {
        return Ok(outstanding.max(0));
    };
    let mut allocated = 0_i64;
    let mut counted = std::collections::HashSet::new();
    for request in debt_request::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let relevant = request
            .confirmed_debt_record_id
            .map(|id| id == origin.id || repayment_amounts.contains_key(&id))
            .unwrap_or(false)
            || request
                .repayment_debt_record_id
                .map(|id| repayment_amounts.contains_key(&id))
                .unwrap_or(false)
            || request.origin_debt_record_id == Some(origin.id);
        if relevant && !verify_chain(&request) {
            return Err(bad_request("关联请求验证链异常，不能继续还款"));
        }
        let record_id = if request.origin_debt_record_id == Some(origin.id) {
            request.confirmed_debt_record_id
        } else if request.confirmed_debt_record_id == Some(origin.id) {
            request.repayment_debt_record_id
        } else {
            None
        };
        if let Some(id) = record_id {
            if let Some(amount) = repayment_amounts.get(&id) {
                if counted.insert(id) {
                    allocated = allocated
                        .checked_add(*amount)
                        .ok_or_else(|| bad_request("还款金额超出范围"))?;
                }
            }
        }
    }
    let remaining = crypto::decrypt_cents(dek, &origin.amount)
        .checked_sub(allocated)
        .ok_or_else(|| bad_request("还款金额超出范围"))?;
    Ok(outstanding.min(remaining).max(0))
}

async fn insert_record(
    transaction: &sea_orm::DatabaseTransaction,
    dek: &crypto::Dek,
    person_id: i64,
    account_id: i64,
    kind: &str,
    amount: i64,
    note: &str,
    happened_at: NaiveDateTime,
) -> HandlerResult<debt_record::Model> {
    if debt_person::Entity::find_by_id(person_id)
        .one(transaction)
        .await
        .map_err(err500)?
        .is_none()
        || account::Entity::find_by_id(account_id)
            .one(transaction)
            .await
            .map_err(err500)?
            .is_none()
    {
        return Err(bad_request("关联的借贷对象或账户已不存在"));
    }
    debt_record::ActiveModel {
        person_id: Set(person_id),
        account_id: Set(account_id),
        kind: Set(kind.into()),
        amount: Set(crypto::encrypt_cents(dek, amount)),
        note: Set(crypto::encrypt(dek, note.as_bytes())),
        happened_at: Set(happened_at),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(transaction)
    .await
    .map_err(err500)
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
    let account_models = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut account_names = super::bills::account_display_names(&dek, &account_models, &details);
    let accounts = account_models
        .into_iter()
        .filter(crate::investment_funds::is_money_account)
        .map(|item| AccountOption {
            id: item.id,
            name: account_names.remove(&item.id).unwrap_or_default(),
            currency: item.currency,
        })
        .collect();
    let people: Vec<PersonOption> = debt_person::Entity::find()
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
    let settled_origins = settled_amounts(&state, &dek).await?;
    let mut origins = Vec::new();
    for record in debt_record::Entity::find()
        .filter(debt_record::Column::Kind.is_in(["borrow", "lend"]))
        .order_by_desc(debt_record::Column::HappenedAt)
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        if settled_origins.contains_key(&record.id) {
            continue;
        }
        let Some(account) = account::Entity::find_by_id(record.account_id)
            .one(&state.db)
            .await
            .map_err(err500)?
        else {
            continue;
        };
        let person = people.iter().find(|person| person.id == record.person_id);
        origins.push(OriginOption {
            id: record.id,
            person_id: record.person_id,
            kind: record.kind.clone(),
            currency: account.currency.clone(),
            label: format!(
                "#{} · {} · {} · {}",
                record.id,
                person
                    .map(|person| person.name.as_str())
                    .unwrap_or("未知对象"),
                kind_label(&record.kind),
                crate::currency::format(
                    crypto::decrypt_cents(&dek, &record.amount),
                    &account.currency
                )
            ),
        });
    }
    Ok(Html(
        DebtRequestFormTemplate {
            accounts,
            people,
            origins,
            default_paid_at: Utc::now().format(TIME_FMT).to_string(),
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
    let _write_guard = state.balance_writes.lock().await;
    if !valid_kind(&form.kind) {
        return Err(bad_request("请求类型无效"));
    }
    let requires_confirmation = form.requires_confirmation.unwrap_or(true);
    if (is_repayment(&form.kind) || !requires_confirmation) && form.person_id == 0 {
        return Err(bad_request("还款及单方记录必须选择已有借贷对象"));
    }
    if is_repayment(&form.kind) != form.origin_debt_record_id.is_some() {
        return Err(bad_request("还款必须关联原借款；新借款不得指定原借款"));
    }
    let person_id = (form.person_id != 0).then_some(form.person_id);
    let person = if let Some(id) = person_id {
        Some(
            debt_person::Entity::find_by_id(id)
                .one(&state.db)
                .await
                .map_err(err500)?
                .ok_or_else(|| bad_request("借贷对象不存在"))?,
        )
    } else {
        None
    };
    let account = account::Entity::find_by_id(form.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("资金账户不存在"))?;
    crate::investment_funds::validate_money_account(&state, &account).await?;
    let detail = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    // Outgoing requests share only the canonical display suffix, never the
    // owner's complete payment identifier, even through the protected copy API.
    let (identifier_label, identifier_masked, identifier) = if incoming(&form.kind) {
        shared_account_identifier(&dek, detail.as_ref()).unwrap_or_default()
    } else {
        (String::new(), String::new(), String::new())
    };
    if requires_confirmation && incoming(&form.kind) && identifier.is_empty() {
        return Err(bad_request(
            "需要对方打款的账户必须有可分享的卡号或账户用户名",
        ));
    }
    let amount_cents = parse_amount(&form.amount)?;
    let recipient_name = form.recipient_name.trim();
    if recipient_name.is_empty() || recipient_name.chars().count() > 100 {
        return Err(bad_request("请填写 1 到 100 个字符的姓名或户名"));
    }
    let expires_at = NaiveDateTime::parse_from_str(form.expires_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("有效期结束时间格式不正确"))?;
    if expires_at <= Utc::now().naive_utc() {
        return Err(bad_request("有效期结束时间必须晚于现在"));
    }
    if form.note.chars().count() > 1000 {
        return Err(bad_request("说明不能超过 1000 个字符"));
    }
    if is_repayment(&form.kind)
        && amount_cents
            > repayment_capacity(
                &state,
                &dek,
                &form.kind,
                form.person_id,
                &account.currency,
                form.origin_debt_record_id,
            )
            .await?
    {
        return Err(bad_request("还款金额超过原借款或该对象同币种尚欠金额"));
    }
    let paid_at = if !requires_confirmation {
        Some(
            NaiveDateTime::parse_from_str(form.paid_at.trim(), TIME_FMT)
                .map_err(|_| bad_request("实际收付时间格式不正确"))?,
        )
    } else {
        None
    };
    if !requires_confirmation {
        super::accounts::ensure_balance_delta(
            &state,
            &dek,
            account.id,
            account_delta(&form.kind, amount_cents)?,
        )
        .await?;
    }
    let account_name = super::bills::account_display_names(
        &dek,
        std::slice::from_ref(&account),
        &detail
            .into_iter()
            .map(|detail| (detail.account_id, detail))
            .collect(),
    )
    .remove(&account.id)
    .unwrap_or_default();
    let created_at = Utc::now();
    let snapshot = RequestSnapshot {
        version: 2,
        kind: form.kind.clone(),
        kind_label: kind_label(&form.kind).into(),
        amount_cents,
        currency: account.currency.clone(),
        recipient_name: recipient_name.into(),
        account_name,
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
    let legacy_digest = created_digest(
        &token_hash,
        account.id,
        person_id,
        &form.kind,
        &snapshot_cipher,
        expires_at,
        created_at,
    );
    let request_digest = policy_digest(
        &legacy_digest,
        requires_confirmation,
        form.origin_debt_record_id,
    );
    let transaction = state.db.begin().await.map_err(err500)?;
    // Open intents take the same lock and transaction as money-writing requests.
    if account::Entity::find_by_id(account.id)
        .one(&transaction)
        .await
        .map_err(err500)?
        .is_none()
        || (person_id.is_some()
            && debt_person::Entity::find_by_id(form.person_id)
                .one(&transaction)
                .await
                .map_err(err500)?
                .is_none())
    {
        return Err(bad_request("关联的借贷对象或账户已不存在"));
    }
    let (submission_cipher, submission_hash, booked_record, booked_at, confirmation_hash) =
        if let Some(paid_at) = paid_at {
            let submission = PayerSubmission {
                version: 1,
                payer_name: crypto::decrypt_string(&dek, &person.as_ref().unwrap().name),
                amount_cents,
                note: form.note.trim().into(),
                paid_at: paid_at.format(TIME_FMT).to_string(),
                ..Default::default()
            };
            let cipher = crypto::encrypt(&key, &serde_json::to_vec(&submission).map_err(err500)?);
            let hash = submitted_digest(&request_digest, &cipher, created_at);
            let record = insert_record(
                &transaction,
                &dek,
                form.person_id,
                account.id,
                &form.kind,
                amount_cents,
                form.note.trim(),
                paid_at,
            )
            .await?;
            let booked = booking_digest(false, &hash, record.id, created_at);
            (cipher, hash, Some(record.id), Some(created_at), booked)
        } else {
            (String::new(), Vec::new(), None, None, Vec::new())
        };
    let label = format!(
        "{} · {}",
        snapshot.kind_label,
        crate::currency::format(amount_cents, &snapshot.currency)
    );
    let created = debt_request::ActiveModel {
        person_id: Set(person_id),
        account_id: Set(account.id),
        kind: Set(form.kind),
        requires_confirmation: Set(requires_confirmation),
        origin_debt_record_id: Set(form.origin_debt_record_id),
        proof_version: Set(2),
        token_hash: Set(token_hash),
        token: Set(crypto::encrypt(&dek, token.as_bytes())),
        label: Set(crypto::encrypt(&dek, label.as_bytes())),
        salt: Set(salt.to_vec()),
        snapshot: Set(snapshot_cipher),
        submission: Set(submission_cipher),
        request_digest: Set(request_digest),
        submission_digest: Set(submission_hash),
        confirmation_digest: Set(confirmation_hash),
        repayment_digest: Set(Vec::new()),
        verification_digest: Set(Vec::new()),
        status: Set(if requires_confirmation {
            "open"
        } else {
            "recorded"
        }
        .into()),
        expires_at: Set(expires_at),
        submitted_at: Set(booked_at),
        confirmed_at: Set(booked_at),
        repayment_submitted_at: Set(None),
        verified_at: Set(None),
        confirmed_debt_record_id: Set(booked_record),
        repayment_debt_record_id: Set(None),
        repayment_account_id: Set(None),
        repayment_submission: Set(String::new()),
        created_at: Set(created_at),
        ..Default::default()
    }
    .insert(&transaction)
    .await
    .map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
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
    let account_details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|detail| (detail.account_id, detail))
        .collect();
    let account_models = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut account_names =
        super::bills::account_display_names(&dek, &account_models, &account_details);
    let repayment_accounts = account_models
        .into_iter()
        .filter(crate::investment_funds::is_money_account)
        .map(|account| RepaymentAccountOption {
            id: account.id,
            name: account_names.remove(&account.id).unwrap_or_default(),
            currency: account.currency,
        })
        .collect::<Vec<_>>();
    let repayment_account_names = repayment_accounts
        .iter()
        .map(|account| (account.id, account.name.clone()))
        .collect::<HashMap<_, _>>();
    let mut views = Vec::new();
    let requests = debt_request::Entity::find()
        .order_by_desc(debt_request::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let settled_origins: std::collections::HashSet<_> = requests
        .iter()
        .filter(|request| request.settled_at.is_some())
        .filter_map(|request| request.confirmed_debt_record_id)
        .collect();
    for request in requests {
        let token = crypto::decrypt_string(&dek, &request.token);
        let Some(token_bytes) = decode_token(&token) else {
            continue;
        };
        let snapshot = decrypt_snapshot(&request, token_bytes.as_slice()).unwrap_or_default();
        let submission = decrypt_submission(&request, token_bytes.as_slice());
        let repayment = decrypt_repayment(&request, token_bytes.as_slice());
        let settled = request.settled_at.is_some();
        let settlement = if settled {
            decrypt_settlement(&request, token_bytes.as_slice())
        } else {
            None
        };
        let origin_settled = request
            .origin_debt_record_id
            .is_some_and(|id| settled_origins.contains(&id));
        let verified = verify_chain(&request) && (!settled || settlement.is_some());
        let expired = request.expires_at <= now;
        let status_label = match request.status.as_str() {
            _ if settled => "已强制平账（账主单方结清，非实际还款）",
            _ if origin_settled && request.confirmed_debt_record_id.is_none() => {
                "原借款已强制平账，本请求不能入账"
            }
            "recorded" => "单方记录，未经对方确认",
            "submitted" if incoming(&request.kind) => "对方已提交打款，待账主核实到账",
            "submitted" => "对方已确认收款，待账主核实付出",
            "confirmed" if !is_repayment(&request.kind) => "原借款已核实入账，还款另见关联请求",
            "confirmed" => "本次还款已由账主核实入账",
            "repayment_submitted" => "历史还款已登记，等待对方确认",
            "repaid" => "历史还款已由对方确认收到",
            "revoked" => "已撤销",
            _ if expired => "已过期",
            _ => "等待对方提交或确认",
        };
        let has_submission = submission.is_some();
        let submitted = submission.unwrap_or_default();
        let has_repayment = repayment.is_some();
        let repayment = repayment.unwrap_or_default();
        views.push(RequestView {
            id: request.id,
            path: format!("/r/{token}"),
            label: crypto::decrypt_string(&dek, &request.label),
            account_name: snapshot.account_name,
            note: snapshot.note,
            status_label: status_label.into(),
            expires_at: request.expires_at.format(TIME_FMT).to_string(),
            created_at: request.created_at.format(TIME_FMT).to_string(),
            payer_name: submitted.payer_name,
            payer_contact: submitted.payer_contact,
            payer_account: super::mask_account_username(&submitted.payer_account),
            paid_amount: if has_submission {
                crate::currency::format(submitted.amount_cents, &snapshot.currency)
            } else {
                String::new()
            },
            payer_note: submitted.note,
            paid_at: submitted.paid_at,
            has_submission,
            can_confirm: request.requires_confirmation
                && request.status == "submitted"
                && !origin_settled
                && verified,
            can_print: has_submission
                && verified
                && (!origin_settled || request.confirmed_debt_record_id.is_some())
                && (matches!(
                    request.status.as_str(),
                    "confirmed" | "recorded" | "repayment_submitted" | "repaid"
                ) || (request.status == "submitted" && !expired)),
            can_submit_repayment: matches!(request.kind.as_str(), "borrow" | "lend")
                && matches!(request.status.as_str(), "confirmed" | "recorded")
                && !settled
                && verified,
            can_settle: matches!(request.kind.as_str(), "borrow" | "lend")
                && request.confirmed_debt_record_id.is_some()
                && matches!(
                    request.status.as_str(),
                    "confirmed" | "recorded" | "repayment_submitted" | "repaid"
                )
                && !settled
                && verified,
            settled,
            settlement_amount: settlement
                .as_ref()
                .map(|item| crate::currency::format(item.amount_cents, &snapshot.currency))
                .unwrap_or_default(),
            settlement_note: settlement.map(|item| item.note).unwrap_or_default(),
            settled_at: request
                .settled_at
                .map(|at| at.format(TIME_FMT).to_string())
                .unwrap_or_default(),
            origin_settled,
            can_revoke: matches!(request.status.as_str(), "open" | "submitted"),
            repayment_pending: request.status == "repayment_submitted",
            repaid: request.status == "repaid",
            repayment_at: if has_repayment {
                repayment.repaid_at
            } else {
                String::new()
            },
            repayment_note: repayment.note,
            repayment_account_name: request
                .repayment_account_id
                .and_then(|id| repayment_account_names.get(&id))
                .cloned()
                .unwrap_or_default(),
            confirmed_debt_record_id: request.confirmed_debt_record_id.unwrap_or_default(),
            verification_ok: verified,
            verification_code: digest_code(chain_head(&request)),
            requires_confirmation: request.requires_confirmation,
            incoming: incoming(&request.kind),
            origin_debt_record_id: request.origin_debt_record_id.unwrap_or_default(),
        });
    }
    let (linked_instances, federated) = super::federation::load_views(&state).await?;
    Ok(Html(
        DebtRequestsTemplate {
            requests: views,
            repayment_accounts,
            default_repayment_at: Utc::now().format(TIME_FMT).to_string(),
            linked_instances,
            federated,
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn open(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Extension(time_zone): Extension<super::ClientTimeZone>,
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
    if matches!(
        request.status.as_str(),
        "submitted" | "confirmed" | "recorded" | "repayment_submitted" | "repaid"
    ) {
        let Some(submission) = decrypt_submission(&request, token_bytes.as_slice()) else {
            return render_public(UnavailableTemplate {});
        };
        return render_receipt(
            &request,
            token,
            token_bytes.as_slice(),
            snapshot,
            submission,
            time_zone,
        );
    }
    render_public(DebtRequestPublicTemplate {
        token,
        requested_amount: crate::currency::format(snapshot.amount_cents, &snapshot.currency),
        amount_input: super::fmt_cents(snapshot.amount_cents),
        incoming: incoming(&request.kind),
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
    if snapshot.identifier.is_empty() {
        return Ok(unavailable_identifier_response());
    }
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
    Extension(time_zone): Extension<super::ClientTimeZone>,
    Form(form): Form<SubmitRequestForm>,
) -> HandlerResult<Response> {
    let _write_guard = state.balance_writes.lock().await;
    let Some((request, token_bytes)) = find_public_request(&state, &token).await? else {
        return render_public(UnavailableTemplate {});
    };
    if !request.requires_confirmation || !verify_chain(&request) {
        return render_public(UnavailableTemplate {});
    }
    if matches!(
        request.status.as_str(),
        "submitted" | "confirmed" | "repayment_submitted" | "repaid"
    ) {
        return Ok(public_response(
            Redirect::to(&format!("/r/{token}")).into_response(),
        ));
    }
    if request.status != "open" {
        return render_public(UnavailableTemplate {});
    }
    let Some(snapshot) = decrypt_snapshot(&request, token_bytes.as_slice()) else {
        return render_public(UnavailableTemplate {});
    };
    let payer_name = form.payer_name.trim();
    if payer_name.is_empty() || payer_name.chars().count() > 100 {
        return Err(bad_request("请填写 1 到 100 个字符的对方姓名"));
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
    let mut active = request.into_active_model();
    active.submission = Set(cipher);
    active.submission_digest = Set(submission_digest);
    active.status = Set("submitted".into());
    active.submitted_at = Set(Some(submitted_at));
    let request = active.update(&state.db).await.map_err(err500)?;
    render_receipt(
        &request,
        token,
        token_bytes.as_slice(),
        snapshot,
        submission,
        time_zone,
    )
}

/// The old loan shortcut now creates a new origin-bound request. Confirmation
/// requires a counterparty submission followed by owner verification; the
/// explicit unilateral policy alone may book immediately.
pub async fn submit_repayment(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<SubmitRepaymentForm>,
) -> HandlerResult<Redirect> {
    let guard = state.balance_writes.lock().await;
    let request = debt_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if !matches!(request.kind.as_str(), "borrow" | "lend")
        || !matches!(request.status.as_str(), "confirmed" | "recorded")
        || request.settled_at.is_some()
        || !verify_chain(&request)
    {
        return Err(bad_request("只有已入账的原借款可以登记还款"));
    }
    let token = crypto::decrypt_string(&dek, &request.token);
    let bytes = decode_token(&token).ok_or_else(|| err500("请求令牌无法解密"))?;
    let submission =
        decrypt_submission(&request, bytes.as_slice()).ok_or_else(|| err500("借款信息无法解密"))?;
    let original = debt_record::Entity::find_by_id(
        request
            .confirmed_debt_record_id
            .ok_or_else(|| bad_request("原借款流水不存在"))?,
    )
    .one(&state.db)
    .await
    .map_err(err500)?
    .ok_or_else(|| bad_request("原借款流水不存在"))?;
    let original_account = account::Entity::find_by_id(original.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原借款账户不存在"))?;
    let kind = if request.kind == "borrow" {
        "repayment_paid"
    } else {
        "repayment_received"
    };
    let amount = if form.amount.trim().is_empty() {
        super::fmt_cents(
            repayment_capacity(
                &state,
                &dek,
                kind,
                original.person_id,
                &original_account.currency,
                Some(original.id),
            )
            .await?,
        )
    } else {
        form.amount
    };
    let create_form = CreateRequestForm {
        kind: kind.into(),
        person_id: original.person_id,
        account_id: form.account_id,
        amount,
        recipient_name: submission.payer_name,
        note: form.note,
        expires_at: (Utc::now() + Duration::days(7))
            .format(TIME_FMT)
            .to_string(),
        requires_confirmation: form.requires_confirmation,
        origin_debt_record_id: Some(original.id),
        paid_at: form.repaid_at,
    };
    drop(guard);
    create(State(state), Extension(SessionDek(dek)), Form(create_form)).await
}

/// 单方放弃剩余债权 / 不再记录应付款，不产生任何资金流水。
pub async fn settle(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<SettleForm>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let request = debt_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if !verify_chain(&request)
        || !matches!(request.kind.as_str(), "borrow" | "lend")
        || !matches!(
            request.status.as_str(),
            "confirmed" | "recorded" | "repayment_submitted" | "repaid"
        )
        || request.confirmed_debt_record_id.is_none()
    {
        return Err(bad_request("只有验证链完整且已入账的原借款可以强制平账"));
    }
    if request.settled_at.is_some() {
        return Ok(Redirect::to("/debt-requests"));
    }
    let note = form.note.trim();
    if note.chars().count() > 1000 {
        return Err(bad_request("平账说明不能超过 1000 个字符"));
    }
    let original = debt_record::Entity::find_by_id(request.confirmed_debt_record_id.unwrap())
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原借款流水不存在"))?;
    if original.kind != request.kind || original.account_id != request.account_id {
        return Err(bad_request("请求与原借款流水不一致"));
    }
    let account = account::Entity::find_by_id(original.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| bad_request("原借款账户不存在"))?;
    let amount = repayment_capacity(
        &state,
        &dek,
        if original.kind == "borrow" {
            "repayment_paid"
        } else {
            "repayment_received"
        },
        original.person_id,
        &account.currency,
        Some(original.id),
    )
    .await?;
    // 历史还款流水可能已把对象净额或本笔剩余抵消为零：仍允许以零金额
    // 单方结清，给已入账原借款打上不可撤销的终结标记，不重复扣减汇总。
    if amount < 0 {
        return Err(err500("待结清金额不能为负"));
    }
    let token = crypto::decrypt_string(&dek, &request.token);
    let bytes = decode_token(&token).ok_or_else(|| err500("请求令牌无法解密"))?;
    let key = request_key(bytes.as_slice(), &request.salt);
    let settlement = crypto::encrypt(
        &key,
        &serde_json::to_vec(&Settlement {
            version: 1,
            amount_cents: amount,
            note: note.into(),
        })
        .map_err(err500)?,
    );
    let settled_amount = crypto::encrypt_cents(&dek, amount);
    let at = Utc::now();
    let hash = settlement_digest(
        booking_chain_head(&request),
        &settlement,
        &settled_amount,
        at,
    );
    let transaction = state.db.begin().await.map_err(err500)?;
    let mut active = request.into_active_model();
    active.settlement = Set(settlement);
    active.settled_amount = Set(settled_amount);
    active.settled_at = Set(Some(at));
    active.settlement_digest = Set(hash);
    active.update(&transaction).await.map_err(err500)?;
    transaction.commit().await.map_err(err500)?;
    Ok(Redirect::to("/debt-requests"))
}

/// 出借人在借款人登记还款后，通过原公开链接确认自己确已收到还款。
pub async fn verify(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> HandlerResult<Response> {
    let _write_guard = state.balance_writes.lock().await;
    let Some((request, _token_bytes)) = find_public_request(&state, &token).await? else {
        return render_public(UnavailableTemplate {});
    };
    if request.proof_version != 1 || !request.requires_confirmation {
        return render_public(UnavailableTemplate {});
    }
    if request.settled_at.is_some() && verify_chain(&request) {
        return Ok(public_response(
            Redirect::to(&format!("/r/{token}")).into_response(),
        ));
    }
    if request.status == "repaid" && verify_chain(&request) {
        return Ok(public_response(
            Redirect::to(&format!("/r/{token}")).into_response(),
        ));
    }
    if request.kind != "borrow"
        || request.status != "repayment_submitted"
        || !verify_chain(&request)
    {
        return render_public(UnavailableTemplate {});
    }
    let verified_at = Utc::now();
    let verification_digest = verified_digest(&request.repayment_digest, verified_at);
    let mut active = request.into_active_model();
    active.status = Set("repaid".into());
    active.verified_at = Set(Some(verified_at));
    active.verification_digest = Set(verification_digest);
    active.update(&state.db).await.map_err(err500)?;
    Ok(public_response(
        Redirect::to(&format!("/r/{token}")).into_response(),
    ))
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
    if !request.requires_confirmation || !verify_chain(&request) {
        return Err(bad_request("单方记录不能变为对方确认，或验证链不完整"));
    }
    if matches!(
        request.status.as_str(),
        "confirmed" | "repayment_submitted" | "repaid"
    ) {
        return Ok(Redirect::to("/debt-requests"));
    }
    if request.status != "submitted" {
        return Err(bad_request("请求尚未由对方提交"));
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
        .ok_or_else(|| bad_request("资金账户已不存在"))?;
    if account.currency != snapshot.currency {
        return Err(bad_request("账户货币已经变化，不能确认"));
    }
    crate::investment_funds::validate_money_account(&state, &account).await?;
    if is_repayment(&request.kind) {
        let person_id = request
            .person_id
            .ok_or_else(|| bad_request("还款请求没有关联借贷对象"))?;
        if submission.amount_cents
            > repayment_capacity(
                &state,
                &dek,
                &request.kind,
                person_id,
                &account.currency,
                request.origin_debt_record_id,
            )
            .await?
        {
            return Err(bad_request("实际还款超过原借款或该对象同币种尚欠金额"));
        }
    }
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        request.account_id,
        account_delta(&request.kind, submission.amount_cents)?,
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
            person_note.push(format!("对方账户：{}", submission.payer_account));
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
    let record = insert_record(
        &transaction,
        &dek,
        person_id,
        request.account_id,
        &request.kind,
        submission.amount_cents,
        &note,
        NaiveDateTime::parse_from_str(&submission.paid_at, TIME_FMT)
            .map_err(|_| err500("实际收付时间无法解析"))?,
    )
    .await?;
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
    let _guard = state.balance_writes.lock().await;
    let request = debt_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if !matches!(request.status.as_str(), "open" | "submitted") || !verify_chain(&request) {
        return Err(bad_request("该请求不能再撤销"));
    }
    let mut active = request.into_active_model();
    active.status = Set("revoked".into());
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/debt-requests"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let request = debt_request::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还请求不存在".into()))?;
    if !verify_chain(&request)
        || request.confirmed_debt_record_id.is_some()
        || request.repayment_debt_record_id.is_some()
    {
        return Err(bad_request("已绑定流水的借还请求与证明不能删除"));
    }
    debt_request::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/debt-requests"))
}
