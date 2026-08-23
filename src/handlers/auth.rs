use askama::Template;
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, EntityTrait, IntoActiveModel, PaginatorTrait, Set,
};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::{
    crypto,
    entity::{account, category, meta, passkey, recovery},
    AppState,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
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

fn set_session_cookie(mut response: Response, state: &AppState, dek: &crypto::Dek) -> Response {
    response
        .headers_mut()
        .insert(header::SET_COOKIE, state.create_session(dek));
    no_store(response)
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: String,
}

#[derive(Template)]
#[template(path = "unlock.html")]
struct UnlockTemplate {
    error: String,
    has_passkeys: bool,
}

#[derive(Template)]
#[template(path = "recovery_phrase.html")]
struct RecoveryPhraseTemplate {
    heading: String,
    phrase: String,
    next_url: String,
}

#[derive(Template)]
#[template(path = "recover.html")]
struct RecoverTemplate {
    error: String,
}

#[derive(Deserialize)]
pub struct SetupFormData {
    password: String,
    confirm: String,
}

#[derive(Deserialize)]
pub struct UnlockFormData {
    password: String,
}

#[derive(Deserialize)]
pub struct RecoveryPasswordFormData {
    password: String,
}

#[derive(Deserialize)]
pub struct RecoverFormData {
    mnemonic: String,
    password: String,
    confirm: String,
}

async fn has_meta(state: &AppState) -> bool {
    meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn has_recovery(state: &AppState) -> bool {
    recovery::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn store_new_recovery(state: &AppState, dek: &crypto::Dek) -> HandlerResult<String> {
    let mnemonic = crypto::generate_recovery_mnemonic();
    let recovery_kek = crypto::derive_recovery_kek(&mnemonic);
    let (nonce, wrapped) = crypto::wrap_dek(dek, recovery_kek.as_slice());

    recovery::Entity::insert(recovery::ActiveModel {
        id: Set(1),
        dek_nonce: Set(nonce),
        wrapped_dek: Set(wrapped),
    })
    .on_conflict(
        OnConflict::column(recovery::Column::Id)
            .update_columns([recovery::Column::DekNonce, recovery::Column::WrappedDek])
            .to_owned(),
    )
    .exec(&state.db)
    .await
    .map_err(err500)?;

    Ok(mnemonic.to_string())
}

pub async fn setup_form(State(state): State<AppState>) -> Response {
    if has_meta(&state).await {
        return Redirect::to("/unlock").into_response();
    }
    let html = SetupTemplate {
        error: String::new(),
    }
    .render()
    .expect("模板渲染失败");
    no_store(Html(html).into_response())
}

pub async fn setup(
    State(state): State<AppState>,
    Form(form): Form<SetupFormData>,
) -> HandlerResult<Response> {
    if has_meta(&state).await {
        return Ok(Redirect::to("/unlock").into_response());
    }
    let render_err = |msg: &str| {
        SetupTemplate {
            error: msg.to_string(),
        }
        .render()
        .map(|html| no_store(Html(html).into_response()))
        .map_err(err500)
    };
    let password = Zeroizing::new(form.password);
    let confirm = Zeroizing::new(form.confirm);
    if password.len() < 6 {
        return render_err("密码至少 6 位");
    }
    if password.as_str() != confirm.as_str() {
        return render_err("两次输入的密码不一致");
    }

    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>();
    let kek = crypto::derive_kek(password.as_str(), &salt);
    let dek = crypto::Dek::new(crypto::random_bytes::<{ crypto::DEK_LEN }>());
    let (nonce, wrapped) = crypto::wrap_dek(&dek, kek.as_slice());

    meta::ActiveModel {
        id: Set(1),
        salt: Set(salt.to_vec()),
        dek_nonce: Set(nonce),
        wrapped_dek: Set(wrapped),
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;

    let phrase = store_new_recovery(&state, &dek).await?;
    ensure_default_account(&state, &dek).await?;
    ensure_default_categories(&state, &dek).await?;
    let html = RecoveryPhraseTemplate {
        heading: "保存恢复助记词".into(),
        phrase,
        next_url: "/dashboard".into(),
    }
    .render()
    .map_err(err500)?;
    Ok(set_session_cookie(Html(html).into_response(), &state, &dek))
}

pub async fn unlock_form(State(state): State<AppState>) -> HandlerResult<Response> {
    if !has_meta(&state).await {
        return Ok(Redirect::to("/setup").into_response());
    }
    let html = UnlockTemplate {
        error: String::new(),
        has_passkeys: passkey::Entity::find()
            .count(&state.db)
            .await
            .map_err(err500)?
            > 0,
    }
    .render()
    .map_err(err500)?;
    Ok(no_store(Html(html).into_response()))
}

pub async fn unlock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UnlockFormData>,
) -> HandlerResult<Response> {
    let password = Zeroizing::new(form.password);
    let m = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;

    let kek = crypto::derive_kek(password.as_str(), &m.salt);
    match crypto::unwrap_dek(&m.dek_nonce, &m.wrapped_dek, kek.as_slice()) {
        Some(dek) => {
            ensure_default_account(&state, &dek).await?;
            state.remove_session(&headers);
            Ok(set_session_cookie(
                Redirect::to("/dashboard").into_response(),
                &state,
                &dek,
            ))
        }
        None => {
            let html = UnlockTemplate {
                error: "密码错误".to_string(),
                has_passkeys: passkey::Entity::find()
                    .count(&state.db)
                    .await
                    .map_err(err500)?
                    > 0,
            }
            .render()
            .map_err(err500)?;
            Ok(no_store(Html(html).into_response()))
        }
    }
}

pub async fn generate_recovery(
    State(state): State<AppState>,
    Form(form): Form<RecoveryPasswordFormData>,
) -> HandlerResult<Response> {
    let password = Zeroizing::new(form.password);
    let meta = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;
    let kek = crypto::derive_kek(password.as_str(), &meta.salt);
    let dek = crypto::unwrap_dek(&meta.dek_nonce, &meta.wrapped_dek, kek.as_slice())
        .ok_or((StatusCode::UNAUTHORIZED, "主密码错误".into()))?;
    let phrase = store_new_recovery(&state, &dek).await?;
    let html = RecoveryPhraseTemplate {
        heading: "新的恢复助记词".into(),
        phrase,
        next_url: "/settings".into(),
    }
    .render()
    .map_err(err500)?;
    Ok(no_store(Html(html).into_response()))
}

pub async fn recover_form(State(state): State<AppState>) -> Response {
    if !has_meta(&state).await {
        return Redirect::to("/setup").into_response();
    }
    let error = if has_recovery(&state).await {
        String::new()
    } else {
        "尚未设置恢复助记词，请先使用主密码解锁并在设置中生成。".into()
    };
    let html = RecoverTemplate { error }.render().expect("模板渲染失败");
    no_store(Html(html).into_response())
}

pub async fn recover(
    State(state): State<AppState>,
    Form(form): Form<RecoverFormData>,
) -> HandlerResult<Response> {
    let render_err = |msg: &str| {
        RecoverTemplate {
            error: msg.to_string(),
        }
        .render()
        .map(|html| no_store(Html(html).into_response()))
        .map_err(err500)
    };
    let password = Zeroizing::new(form.password);
    let confirm = Zeroizing::new(form.confirm);
    if password.len() < 6 {
        return render_err("新密码至少 6 位");
    }
    if password.as_str() != confirm.as_str() {
        return render_err("两次输入的新密码不一致");
    }
    let Some(mnemonic) = crypto::parse_recovery_mnemonic(&form.mnemonic) else {
        return render_err("助记词格式或校验和不正确");
    };
    let recovery_row = recovery::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置恢复助记词".into()))?;
    let recovery_kek = crypto::derive_recovery_kek(&mnemonic);
    let Some(dek) = crypto::unwrap_dek(
        &recovery_row.dek_nonce,
        &recovery_row.wrapped_dek,
        recovery_kek.as_slice(),
    ) else {
        return render_err("助记词错误");
    };

    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>();
    let kek = crypto::derive_kek(password.as_str(), &salt);
    let (nonce, wrapped) = crypto::wrap_dek(&dek, kek.as_slice());
    let m = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;
    let mut active = m.into_active_model();
    active.salt = Set(salt.to_vec());
    active.dek_nonce = Set(nonce);
    active.wrapped_dek = Set(wrapped);
    active.update(&state.db).await.map_err(err500)?;

    ensure_default_account(&state, &dek).await?;
    state.clear_sessions();
    Ok(set_session_cookie(
        Redirect::to("/dashboard").into_response(),
        &state,
        &dek,
    ))
}

pub async fn lock(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state.remove_session(&headers);
    let mut response = Redirect::to("/unlock").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, AppState::clear_session_cookie());
    no_store(response)
}

/// 解锁后若没有任何账户，创建默认账户（名称需用 DEK 加密，故不能在 db 初始化时做）
pub(crate) async fn ensure_default_account(
    state: &AppState,
    dek: &crypto::Dek,
) -> HandlerResult<()> {
    if account::Entity::find()
        .count(&state.db)
        .await
        .map_err(err500)?
        == 0
    {
        account::ActiveModel {
            name: Set(crypto::encrypt(dek, "默认账户".as_bytes())),
            kind: Set("other".into()),
            currency: Set(crate::currency::FALLBACK_CURRENCY.into()),
            balance_offset: Set(crypto::encrypt_cents(dek, 0)),
            note: Set(crypto::encrypt(dek, "".as_bytes())),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(())
}

async fn ensure_default_categories(state: &AppState, dek: &crypto::Dek) -> HandlerResult<()> {
    if category::Entity::find()
        .count(&state.db)
        .await
        .map_err(err500)?
        > 0
    {
        return Ok(());
    }
    for (kind, name, is_food) in [
        ("expense", "餐饮", true),
        ("expense", "交通", false),
        ("expense", "购物", false),
        ("expense", "居住", false),
        ("expense", "娱乐", false),
        ("expense", "其他支出", false),
        ("income", "工资", false),
        ("income", "其他收入", false),
    ] {
        category::ActiveModel {
            kind: Set(kind.into()),
            name: Set(crypto::encrypt(dek, name.as_bytes())),
            is_food: Set(is_food),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(())
}
