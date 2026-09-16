use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Duration, NaiveDateTime, Utc};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use zeroize::Zeroizing;

use crate::{
    crypto,
    entity::{account, account_detail, bill, bill_share},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

const TIME_FMT: &str = "%Y-%m-%dT%H:%M";
const SHARE_KEY_CONTEXT: &[u8] = b"haruka bill share key v1\0";

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

#[derive(Clone)]
struct BillSummaryView {
    kind: String,
    amount: String,
    account: String,
    category: String,
    note: String,
    happened_at: String,
}

#[derive(Clone)]
struct ShareView {
    id: i64,
    source_bill_id: i64,
    source_exists: bool,
    label: String,
    path: String,
    has_password: bool,
    expires_at: String,
    created_at: String,
    expired: bool,
}

#[derive(Template)]
#[template(path = "bill_share.html")]
struct BillShareTemplate {
    bill_id: i64,
    bill: BillSummaryView,
    default_expires_at: String,
    created_path: String,
    shares: Vec<ShareView>,
}

#[derive(Template)]
#[template(path = "shares.html")]
struct SharesTemplate {
    shares: Vec<ShareView>,
}

#[derive(Template)]
#[template(path = "shared_bill.html")]
struct SharedBillTemplate {
    bill: SharedBillSnapshot,
    expires_at: String,
}

#[derive(Template)]
#[template(path = "shared_bill_unlock.html")]
struct SharedBillUnlockTemplate {
    token: String,
    error: String,
    expires_at: String,
}

#[derive(Template)]
#[template(path = "shared_bill_unavailable.html")]
struct SharedBillUnavailableTemplate {}

#[derive(Serialize, Deserialize)]
struct SharedBillSnapshot {
    version: u8,
    kind: String,
    amount: String,
    account: String,
    category: String,
    note: String,
    happened_at: String,
}

#[derive(Default, Deserialize)]
pub struct SharePageQuery {
    #[serde(default)]
    created: i64,
}

#[derive(Deserialize)]
pub struct CreateShareForm {
    expires_at: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

#[derive(Deserialize)]
pub struct UnlockShareForm {
    #[serde(default)]
    password: String,
}

#[derive(Deserialize)]
pub struct DeleteShareForm {
    #[serde(default)]
    return_to: String,
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
            "default-src 'none'; style-src 'self'; script-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

fn render_public<T: Template>(template: T) -> HandlerResult<Response> {
    let html = template.render().map_err(err500)?;
    Ok(public_response(Html(html).into_response()))
}

fn unavailable_response() -> HandlerResult<Response> {
    render_public(SharedBillUnavailableTemplate {})
}

fn share_key(token: &[u8], password: &str, salt: &[u8]) -> crypto::Dek {
    let mut material = Zeroizing::new(Vec::with_capacity(
        SHARE_KEY_CONTEXT.len() + token.len() + password.len(),
    ));
    material.extend_from_slice(SHARE_KEY_CONTEXT);
    material.extend_from_slice(token);
    material.extend_from_slice(password.as_bytes());
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

async fn find_valid_share(
    state: &AppState,
    token: &str,
) -> HandlerResult<Option<(bill_share::Model, Zeroizing<Vec<u8>>)>> {
    let Some(token_bytes) = decode_token(token) else {
        return Ok(None);
    };
    let share = bill_share::Entity::find()
        .filter(bill_share::Column::TokenHash.eq(token_hash(token_bytes.as_slice())))
        .one(&state.db)
        .await
        .map_err(err500)?;
    Ok(share
        .filter(|share| share.expires_at > Utc::now().naive_utc())
        .map(|share| (share, token_bytes)))
}

async fn bill_summary(
    state: &AppState,
    dek: &crypto::Dek,
    bill_id: i64,
) -> HandlerResult<(BillSummaryView, String)> {
    let item = bill::Entity::find_by_id(bill_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单不存在".into()))?;
    let account = account::Entity::find_by_id(item.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "账单账户不存在".into()))?;
    let detail = account_detail::Entity::find_by_id(account.id)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let kind = if item.kind == "income" {
        "收入"
    } else {
        "支出"
    }
    .to_string();
    let amount =
        crate::currency::format(crypto::decrypt_cents(dek, &item.amount), &account.currency);
    let category = crypto::decrypt_string(dek, &item.category);
    let view = BillSummaryView {
        kind: kind.clone(),
        amount: amount.clone(),
        account: super::bills::account_display_name(dek, &account, detail.as_ref()),
        category: category.clone(),
        note: crypto::decrypt_string(dek, &item.note),
        happened_at: item.happened_at.format(TIME_FMT).to_string(),
    };
    Ok((view, format!("{kind} · {amount} · {category}")))
}

fn snapshot_from_view(view: &BillSummaryView) -> SharedBillSnapshot {
    SharedBillSnapshot {
        version: 1,
        kind: view.kind.clone(),
        amount: view.amount.clone(),
        account: view.account.clone(),
        category: view.category.clone(),
        note: view.note.clone(),
        happened_at: view.happened_at.clone(),
    }
}

async fn share_views(state: &AppState, dek: &crypto::Dek) -> HandlerResult<Vec<ShareView>> {
    let now = Utc::now().naive_utc();
    let source_ids = bill::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|bill| bill.id)
        .collect::<HashSet<_>>();
    Ok(bill_share::Entity::find()
        .order_by_desc(bill_share::Column::CreatedAt)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter_map(|share| {
            let token = crypto::decrypt_string(dek, &share.token);
            (!token.is_empty()).then(|| ShareView {
                id: share.id,
                source_bill_id: share.source_bill_id,
                source_exists: source_ids.contains(&share.source_bill_id),
                label: crypto::decrypt_string(dek, &share.label),
                path: format!("/s/{token}"),
                has_password: share.has_password,
                expires_at: share.expires_at.format(TIME_FMT).to_string(),
                created_at: share.created_at.format(TIME_FMT).to_string(),
                expired: share.expires_at <= now,
            })
        })
        .collect())
}

pub async fn bill_share_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(bill_id): Path<i64>,
    Query(query): Query<SharePageQuery>,
) -> HandlerResult<Html<String>> {
    let (bill, _) = bill_summary(&state, &dek, bill_id).await?;
    let shares = share_views(&state, &dek)
        .await?
        .into_iter()
        .filter(|share| share.source_bill_id == bill_id)
        .collect::<Vec<_>>();
    let created_path = shares
        .iter()
        .find(|share| share.id == query.created)
        .map(|share| share.path.clone())
        .unwrap_or_default();
    let html = BillShareTemplate {
        bill_id,
        bill,
        default_expires_at: (Utc::now() + Duration::days(7))
            .format(TIME_FMT)
            .to_string(),
        created_path,
        shares,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let html = SharesTemplate {
        shares: share_views(&state, &dek).await?,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(bill_id): Path<i64>,
    Form(form): Form<CreateShareForm>,
) -> HandlerResult<Redirect> {
    let expires_at = NaiveDateTime::parse_from_str(form.expires_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("有效期结束时间格式不正确"))?;
    if expires_at <= Utc::now().naive_utc() {
        return Err(bad_request("有效期结束时间必须晚于现在"));
    }
    let password = Zeroizing::new(form.password);
    let confirm = Zeroizing::new(form.confirm);
    if password.as_str() != confirm.as_str() {
        return Err(bad_request("两次输入的分享密码不一致"));
    }
    if password.len() > 256 {
        return Err(bad_request("分享密码不能超过 256 个字符"));
    }

    let (view, label) = bill_summary(&state, &dek, bill_id).await?;
    let snapshot = serde_json::to_vec(&snapshot_from_view(&view)).map_err(err500)?;
    let token_bytes = Zeroizing::new(crypto::random_bytes::<32>());
    let token = URL_SAFE_NO_PAD.encode(token_bytes.as_slice());
    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>();
    let key = share_key(token_bytes.as_slice(), password.as_str(), &salt);
    let created = bill_share::ActiveModel {
        source_bill_id: Set(bill_id),
        token_hash: Set(token_hash(token_bytes.as_slice())),
        token: Set(crypto::encrypt(&dek, token.as_bytes())),
        label: Set(crypto::encrypt(&dek, label.as_bytes())),
        salt: Set(salt.to_vec()),
        snapshot: Set(crypto::encrypt(&key, &snapshot)),
        has_password: Set(!password.is_empty()),
        expires_at: Set(expires_at),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to(&format!(
        "/bills/{bill_id}/share?created={}",
        created.id
    )))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteShareForm>,
) -> HandlerResult<Redirect> {
    let share = bill_share::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分享记录不存在".into()))?;
    let source_bill_id = share.source_bill_id;
    bill_share::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    if form.return_to == "bill" {
        Ok(Redirect::to(&format!("/bills/{source_bill_id}/share")))
    } else {
        Ok(Redirect::to("/shares"))
    }
}

fn password_prompt(
    share: &bill_share::Model,
    token: String,
    error: String,
) -> HandlerResult<Response> {
    render_public(SharedBillUnlockTemplate {
        token,
        error,
        expires_at: share.expires_at.format(TIME_FMT).to_string(),
    })
}

fn decrypted_share(
    share: &bill_share::Model,
    token: &[u8],
    password: &str,
) -> Option<SharedBillSnapshot> {
    let key = share_key(token, password, &share.salt);
    let plaintext = crypto::decrypt(&key, &share.snapshot)?;
    let snapshot = serde_json::from_slice::<SharedBillSnapshot>(&plaintext).ok()?;
    (snapshot.version == 1).then_some(snapshot)
}

pub async fn open(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> HandlerResult<Response> {
    let Some((share, token_bytes)) = find_valid_share(&state, &token).await? else {
        return unavailable_response();
    };
    if share.has_password {
        return password_prompt(&share, token, String::new());
    }
    let Some(bill) = decrypted_share(&share, token_bytes.as_slice(), "") else {
        return unavailable_response();
    };
    render_public(SharedBillTemplate {
        bill,
        expires_at: share.expires_at.format(TIME_FMT).to_string(),
    })
}

pub async fn unlock(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Form(form): Form<UnlockShareForm>,
) -> HandlerResult<Response> {
    let Some((share, token_bytes)) = find_valid_share(&state, &token).await? else {
        return unavailable_response();
    };
    if form.password.len() > 256 {
        return password_prompt(&share, token, "分享密码不正确".into());
    }
    let password = Zeroizing::new(form.password);
    let Some(bill) = decrypted_share(
        &share,
        token_bytes.as_slice(),
        if share.has_password {
            password.as_str()
        } else {
            ""
        },
    ) else {
        return password_prompt(&share, token, "分享密码不正确".into());
    };
    render_public(SharedBillTemplate {
        bill,
        expires_at: share.expires_at.format(TIME_FMT).to_string(),
    })
}
