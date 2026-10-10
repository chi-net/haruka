use askama::Template;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set};
use serde::Deserialize;

use crate::{
    crypto, currency,
    entity::{passkey, preference, recovery},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct PasskeyRow {
    id: i64,
    name: String,
    created_at: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    currencies: &'static [currency::CurrencyOption],
    default_currency: String,
    owner_name: String,
    sms_token_custom: bool,
    recovery_configured: bool,
    passkeys: Vec<PasskeyRow>,
}

#[derive(Deserialize)]
pub struct CurrencyFormData {
    default_currency: String,
}

#[derive(Deserialize)]
pub struct OwnerNameFormData {
    owner_name: String,
}

#[derive(Deserialize)]
pub struct SmsTokenFormData {
    #[serde(default)]
    token: String,
    action: String,
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let recovery_configured = recovery::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some();
    let passkeys = passkey::Entity::find()
        .order_by_asc(passkey::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|passkey| PasskeyRow {
            id: passkey.id,
            name: crypto::decrypt_string(&dek, &passkey.name),
            created_at: passkey.created_at.format("%Y-%m-%dT%H:%M").to_string(),
        })
        .collect();
    let preferences = preference::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?;
    let owner_name = preferences
        .as_ref()
        .map(|item| crypto::decrypt_string(&dek, &item.owner_name))
        .unwrap_or_default();
    let sms_token_custom = preferences
        .as_ref()
        .is_some_and(|item| !item.sms_api_token_hash.is_empty());
    let html = SettingsTemplate {
        currencies: currency::CURRENCIES,
        default_currency: currency::default_currency(&state).await.map_err(err500)?,
        owner_name,
        sms_token_custom,
        recovery_configured,
        passkeys,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update_currency(
    State(state): State<AppState>,
    Form(form): Form<CurrencyFormData>,
) -> HandlerResult<Redirect> {
    let code = form.default_currency.trim().to_uppercase();
    if !currency::valid(&code) {
        return Err(bad_request("默认货币无效"));
    }
    preference::ActiveModel {
        id: Set(1),
        default_currency: Set(code),
        ..Default::default()
    }
    .update(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/settings"))
}

pub async fn update_owner_name(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<OwnerNameFormData>,
) -> HandlerResult<Redirect> {
    let name = form.owner_name.trim();
    if name.chars().count() > 80 {
        return Err(bad_request("本人姓名不能超过 80 个字符"));
    }
    let preferences = preference::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("默认设置不存在"))?;
    let mut active = preferences.into_active_model();
    active.owner_name = Set(crypto::encrypt(&dek, name.as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/settings"))
}

pub async fn update_sms_token(
    State(state): State<AppState>,
    Form(form): Form<SmsTokenFormData>,
) -> HandlerResult<Redirect> {
    let hash = match form.action.as_str() {
        "save" => {
            let token = form.token.trim();
            if token.chars().count() < 8 {
                return Err(bad_request("短信接口 Token 至少需要 8 个字符"));
            }
            if token.chars().count() > 256 {
                return Err(bad_request("短信接口 Token 不能超过 256 个字符"));
            }
            crate::sms_api_token_hash(token)
        }
        "reset" => String::new(),
        _ => return Err(bad_request("短信接口 Token 操作无效")),
    };
    let preferences = preference::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("默认设置不存在"))?;
    let mut active = preferences.into_active_model();
    active.sms_api_token_hash = Set(hash);
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/settings"))
}
