use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set};
use serde::Deserialize;

use crate::{crypto, entity::category, AppState, SessionDek};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

struct CategoryRow {
    id: i64,
    name: String,
    is_food: bool,
}

#[derive(Template)]
#[template(path = "categories.html")]
struct CategoriesTemplate {
    income_categories: Vec<CategoryRow>,
    expense_categories: Vec<CategoryRow>,
}

#[derive(Template)]
#[template(path = "category_form.html")]
struct CategoryFormTemplate {
    action: String,
    name: String,
    kind: String,
    is_food: bool,
}

fn default_period() -> String {
    "month".into()
}

#[derive(Deserialize)]
pub struct CategoryFormData {
    name: String,
    kind: String,
    #[serde(default)]
    is_food: bool,
}

fn validate_form(form: &CategoryFormData) -> HandlerResult<()> {
    if form.name.trim().is_empty() {
        return Err(bad_request("分类名称不能为空"));
    }
    if form.kind != "income" && form.kind != "expense" {
        return Err(bad_request("分类类型无效"));
    }
    Ok(())
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
    if categories.into_iter().any(|category| {
        category.kind == kind
            && Some(category.id) != except_id
            && crypto::decrypt_string(dek, &category.name) == name
    }) {
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
            is_food: category.is_food,
        };
        if category.kind == "income" {
            income_categories.push(row);
        } else {
            expense_categories.push(row);
        }
    }
    let html = CategoriesTemplate {
        income_categories,
        expense_categories,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<CategoryFormData>,
) -> HandlerResult<Redirect> {
    validate_form(&form)?;
    let _balance_guard = state.balance_writes.lock().await;
    let name = form.name.trim();
    ensure_unique_name(&state, &dek, &form.kind, name, None).await?;
    let expense = form.kind == "expense";
    category::ActiveModel {
        kind: Set(form.kind),
        name: Set(crypto::encrypt(&dek, name.as_bytes())),
        is_food: Set(expense && form.is_food),
        count_limit: Set(String::new()),
        count_limit_period: Set(default_period()),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/categories"))
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let category = category::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分类不存在".into()))?;
    let expense = category.kind == "expense";
    let html = CategoryFormTemplate {
        action: format!("/categories/{id}/edit"),
        name: crypto::decrypt_string(&dek, &category.name),
        kind: category.kind,
        is_food: expense && category.is_food,
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<CategoryFormData>,
) -> HandlerResult<Redirect> {
    validate_form(&form)?;
    let _balance_guard = state.balance_writes.lock().await;
    let category = category::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分类不存在".into()))?;
    let name = form.name.trim();
    ensure_unique_name(&state, &dek, &form.kind, name, Some(id)).await?;
    let expense = form.kind == "expense";
    let mut active = category.into_active_model();
    active.kind = Set(form.kind);
    active.name = Set(crypto::encrypt(&dek, name.as_bytes()));
    active.is_food = Set(expense && form.is_food);
    if !expense {
        active.count_limit = Set(String::new());
        active.count_limit_period = Set(default_period());
    }
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/categories"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    category::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/categories"))
}
