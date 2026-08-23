use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set};
use serde::Deserialize;

use crate::{
    crypto,
    entity::{category, recovery},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct CategoryRow {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    income_categories: Vec<CategoryRow>,
    expense_categories: Vec<CategoryRow>,
    recovery_configured: bool,
}

#[derive(Template)]
#[template(path = "category_form.html")]
struct CategoryFormTemplate {
    action: String,
    name: String,
    kind: String,
}

#[derive(Deserialize)]
pub struct CategoryFormData {
    name: String,
    kind: String,
}

fn valid_kind(kind: &str) -> bool {
    kind == "income" || kind == "expense"
}

async fn ensure_unique_name(
    state: &AppState,
    dek: &crypto::Dek,
    kind: &str,
    name: &str,
    except_id: Option<i64>,
) -> HandlerResult<()> {
    let categories = category::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let duplicate = categories.into_iter().any(|category| {
        category.kind == kind
            && Some(category.id) != except_id
            && crypto::decrypt_string(dek, &category.name) == name
    });
    if duplicate {
        return Err(bad_request("同类型下已存在同名分类"));
    }
    Ok(())
}

pub async fn show(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let categories = category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut income_categories = Vec::new();
    let mut expense_categories = Vec::new();
    for category in categories {
        let row = CategoryRow {
            id: category.id,
            name: crypto::decrypt_string(&dek, &category.name),
        };
        if category.kind == "income" {
            income_categories.push(row);
        } else {
            expense_categories.push(row);
        }
    }
    let recovery_configured = recovery::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_some();
    let html = SettingsTemplate {
        income_categories,
        expense_categories,
        recovery_configured,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create_category(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<CategoryFormData>,
) -> HandlerResult<Redirect> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(bad_request("分类名称不能为空"));
    }
    if !valid_kind(&form.kind) {
        return Err(bad_request("分类类型无效"));
    }
    ensure_unique_name(&state, &dek, &form.kind, name, None).await?;
    category::ActiveModel {
        kind: Set(form.kind),
        name: Set(crypto::encrypt(&dek, name.as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/settings"))
}

pub async fn edit_category_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let category = category::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分类不存在".into()))?;
    let html = CategoryFormTemplate {
        action: format!("/settings/categories/{id}/edit"),
        name: crypto::decrypt_string(&dek, &category.name),
        kind: category.kind,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update_category(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<CategoryFormData>,
) -> HandlerResult<Redirect> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(bad_request("分类名称不能为空"));
    }
    if !valid_kind(&form.kind) {
        return Err(bad_request("分类类型无效"));
    }
    ensure_unique_name(&state, &dek, &form.kind, name, Some(id)).await?;
    let category = category::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分类不存在".into()))?;
    let mut active = category.into_active_model();
    active.kind = Set(form.kind);
    active.name = Set(crypto::encrypt(&dek, name.as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/settings"))
}

pub async fn delete_category(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    category::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/settings"))
}
