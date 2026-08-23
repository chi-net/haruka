use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use sea_orm::{ActiveModelTrait, EntityTrait, PaginatorTrait, Set};
use serde::Deserialize;

use crate::{crypto, entity::{account, meta}, AppState};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
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

async fn has_meta(state: &AppState) -> bool {
    meta::Entity::find_by_id(1).one(&state.db).await.ok().flatten().is_some()
}

pub async fn setup_form(State(state): State<AppState>) -> Response {
    if has_meta(&state).await {
        return Redirect::to("/unlock").into_response();
    }
    let html = SetupTemplate { error: String::new() }.render().expect("模板渲染失败");
    Html(html).into_response()
}

pub async fn setup(
    State(state): State<AppState>,
    Form(form): Form<SetupFormData>,
) -> HandlerResult<Response> {
    if has_meta(&state).await {
        return Ok(Redirect::to("/unlock").into_response());
    }
    let render_err = |msg: &str| {
        SetupTemplate { error: msg.to_string() }
            .render()
            .map(|h| Html(h).into_response())
            .map_err(err500)
    };
    if form.password.len() < 6 {
        return render_err("密码至少 6 位");
    }
    if form.password != form.confirm {
        return render_err("两次输入的密码不一致");
    }

    let salt = crypto::random_bytes::<{ crypto::SALT_LEN }>();
    let kek = crypto::derive_kek(&form.password, &salt);
    let dek = crypto::Dek::new(crypto::random_bytes::<{ crypto::DEK_LEN }>());
    let (nonce, wrapped) = crypto::wrap_dek(&dek, &kek[..]);

    meta::ActiveModel {
        id: Set(1),
        salt: Set(salt.to_vec()),
        dek_nonce: Set(nonce),
        wrapped_dek: Set(wrapped),
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;

    *state.dek.write().unwrap() = Some(dek);
    ensure_default_account(&state).await?;
    Ok(Redirect::to("/").into_response())
}

pub async fn unlock_form(State(state): State<AppState>) -> Response {
    if !has_meta(&state).await {
        return Redirect::to("/setup").into_response();
    }
    let html = UnlockTemplate { error: String::new() }.render().expect("模板渲染失败");
    Html(html).into_response()
}

pub async fn unlock(
    State(state): State<AppState>,
    Form(form): Form<UnlockFormData>,
) -> HandlerResult<Response> {
    let m = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;

    let kek = crypto::derive_kek(&form.password, &m.salt);
    match crypto::unwrap_dek(&m.dek_nonce, &m.wrapped_dek, &kek) {
        Some(dek) => {
            *state.dek.write().unwrap() = Some(dek);
            ensure_default_account(&state).await?;
            Ok(Redirect::to("/").into_response())
        }
        None => {
            let html = UnlockTemplate { error: "密码错误".to_string() }
                .render()
                .map_err(err500)?;
            Ok(Html(html).into_response())
        }
    }
}

/// 解锁后若没有任何账户，创建默认账户（名称需用 DEK 加密，故不能在 db 初始化时做）
async fn ensure_default_account(state: &AppState) -> HandlerResult<()> {
    let dek = state.dek.read().unwrap().clone().ok_or((StatusCode::UNAUTHORIZED, "未解锁".into()))?;
    if account::Entity::find().count(&state.db).await.map_err(err500)? == 0 {
        account::ActiveModel {
            name: Set(crypto::encrypt(&dek, "默认账户".as_bytes())),
            note: Set(crypto::encrypt(&dek, "".as_bytes())),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .map_err(err500)?;
    }
    Ok(())
}
