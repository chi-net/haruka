use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
};
use serde::Deserialize;

use crate::{
    category_limits::{self, CountStatus},
    crypto,
    entity::{bill, category},
    AppState, SessionDek,
};

use super::ClientTimeZone;

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
    status: Option<CountStatus>,
}

#[derive(Template)]
#[template(path = "categories.html")]
struct CategoriesTemplate {
    income_categories: Vec<CategoryRow>,
    expense_categories: Vec<CategoryRow>,
    time_zone: String,
}

#[derive(Template)]
#[template(path = "category_form.html")]
struct CategoryFormTemplate {
    action: String,
    name: String,
    kind: String,
    is_food: bool,
    count_limit: String,
    count_limit_period: String,
    time_zone: String,
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
    #[serde(default)]
    count_limit: String,
    #[serde(default = "default_period")]
    count_limit_period: String,
}

fn validate_form(form: &CategoryFormData) -> HandlerResult<Option<u32>> {
    if form.name.trim().is_empty() {
        return Err(bad_request("分类名称不能为空"));
    }
    if form.kind != "income" && form.kind != "expense" {
        return Err(bad_request("分类类型无效"));
    }
    category_limits::parse_limit(&form.kind, &form.count_limit_period, &form.count_limit)
        .map_err(bad_request)
}

fn encrypt_limit(dek: &crypto::Dek, limit: Option<u32>) -> String {
    limit
        .map(|count| crypto::encrypt(dek, count.to_string().as_bytes()))
        .unwrap_or_default()
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
    Extension(time_zone): Extension<ClientTimeZone>,
) -> HandlerResult<Html<String>> {
    let categories = category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let bills = if categories
        .iter()
        .any(|category| category.kind == "expense" && !category.count_limit.is_empty())
    {
        bill::Entity::find()
            .filter(bill::Column::Kind.eq("expense"))
            .filter(bill::Column::CategoryId.gt(0))
            .all(&state.db)
            .await
            .map_err(err500)?
    } else {
        Vec::new()
    };
    let mut statuses: HashMap<_, _> = category_limits::statuses(
        &dek,
        time_zone,
        time_zone.today(),
        &categories,
        bills.iter(),
    )
    .map_err(err500)?
    .into_iter()
    .map(|status| (status.id, status))
    .collect();
    let mut income_categories = Vec::new();
    let mut expense_categories = Vec::new();
    for category in categories {
        let mut status = statuses.remove(&category.id);
        let name = match status.as_mut() {
            Some(status) => std::mem::take(&mut status.name),
            None => crypto::decrypt_string(&dek, &category.name),
        };
        let row = CategoryRow {
            id: category.id,
            name,
            is_food: category.is_food,
            status,
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
        time_zone: time_zone.0.name().into(),
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
    let limit = validate_form(&form)?;
    let _balance_guard = state.balance_writes.lock().await;
    let name = form.name.trim();
    ensure_unique_name(&state, &dek, &form.kind, name, None).await?;
    let expense = form.kind == "expense";
    category::ActiveModel {
        kind: Set(form.kind),
        name: Set(crypto::encrypt(&dek, name.as_bytes())),
        is_food: Set(expense && form.is_food),
        count_limit: Set(encrypt_limit(&dek, limit)),
        count_limit_period: Set(if expense {
            form.count_limit_period
        } else {
            default_period()
        }),
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
    Extension(time_zone): Extension<ClientTimeZone>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let category = category::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "分类不存在".into()))?;
    let limit = category_limits::decrypt_limit(&dek, &category).map_err(err500)?;
    let expense = category.kind == "expense";
    let html = CategoryFormTemplate {
        action: format!("/categories/{id}/edit"),
        name: crypto::decrypt_string(&dek, &category.name),
        kind: category.kind,
        is_food: expense && category.is_food,
        count_limit: limit.map(|count| count.to_string()).unwrap_or_default(),
        count_limit_period: if expense {
            category.count_limit_period
        } else {
            default_period()
        },
        time_zone: time_zone.0.name().into(),
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
    let limit = validate_form(&form)?;
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
    active.count_limit = Set(encrypt_limit(&dek, limit));
    active.count_limit_period = Set(if expense {
        form.count_limit_period
    } else {
        default_period()
    });
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
