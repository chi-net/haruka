use super::accounts;
use crate::{
    crypto, currency,
    entity::{account, balance_adjustment},
    investment_funds, AppState, SessionDek,
};
use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn bad(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.into())
}

async fn group(state: &AppState, id: i64) -> HandlerResult<account::Model> {
    let a = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "投资分组不存在".into()))?;
    if a.kind != "investment" || a.parent_id.is_some() {
        return Err(bad("请选择投资分组"));
    }
    Ok(a)
}
async fn fund(state: &AppState, id: i64) -> HandlerResult<account::Model> {
    let a = account::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "基金不存在".into()))?;
    if a.kind != "investment_fund" {
        return Err(bad("请选择具体基金"));
    }
    let parent = group(
        state,
        a.parent_id.ok_or_else(|| err500("基金缺少投资分组"))?,
    )
    .await?;
    if parent.currency != a.currency {
        return Err(err500("基金货币与投资分组不一致"));
    }
    Ok(a)
}
async fn children(state: &AppState, root: &account::Model) -> HandlerResult<Vec<account::Model>> {
    let funds = account::Entity::find()
        .filter(account::Column::ParentId.eq(root.id))
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    if funds
        .iter()
        .any(|a| a.kind != "investment_fund" || a.currency != root.currency)
    {
        return Err(err500("基金分组或货币不一致"));
    }
    Ok(funds)
}
struct FundRow {
    id: i64,
    name: String,
    value: String,
    target: String,
    baseline: i64,
    aliases: String,
    note: String,
    last_calibrated: String,
    overdue: bool,
}
async fn rows(
    state: &AppState,
    dek: &crypto::Dek,
    root: &account::Model,
) -> HandlerResult<(Vec<FundRow>, i64)> {
    let mut rows = Vec::new();
    let mut total = 0i64;
    for a in children(state, root).await? {
        let value = accounts::current_balance(state, dek, a.id).await?;
        total = total
            .checked_add(value)
            .ok_or_else(|| err500("持仓总价值超出范围"))?;
        let last = a.last_calibrated_at.unwrap_or(a.created_at);
        let mut aliases = investment_funds::sms_names(dek, &a)?;
        aliases.remove(0);
        rows.push(FundRow {
            id: a.id,
            name: crypto::decrypt_string(dek, &a.name),
            value: currency::format(value, &a.currency),
            target: super::fmt_cents(value),
            baseline: value,
            aliases: aliases.join("、"),
            note: crypto::decrypt_string(dek, &a.note),
            last_calibrated: last.format("%Y-%m-%d").to_string(),
            overdue: value > 0 && (chrono::Utc::now() - last).num_days() >= 30,
        });
    }
    Ok((rows, total))
}
#[derive(Template)]
#[template(path = "funds.html")]
struct FundsTemplate {
    id: i64,
    name: String,
    currency: String,
    total: String,
    funds: Vec<FundRow>,
}
pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let root = group(&state, id).await?;
    let (funds, total) = rows(&state, &dek, &root).await?;
    Ok(Html(
        FundsTemplate {
            id,
            name: crypto::decrypt_string(&dek, &root.name),
            currency: root.currency.clone(),
            total: currency::format(total, &root.currency),
            funds,
        }
        .render()
        .map_err(err500)?,
    ))
}
#[derive(Template)]
#[template(path = "fund_form.html")]
struct FundFormTemplate {
    heading: String,
    action: String,
    parent_id: i64,
    parent_name: String,
    currency: String,
    name: String,
    aliases: String,
    note: String,
}
#[derive(Deserialize)]
pub struct FundFormData {
    name: String,
    #[serde(default)]
    aliases: String,
    #[serde(default)]
    note: String,
}
fn encrypted_aliases(dek: &crypto::Dek, text: &str) -> HandlerResult<String> {
    let mut aliases = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let normalized: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if seen.insert(normalized) {
            aliases.push(line.to_owned());
        }
    }
    Ok(crypto::encrypt(
        dek,
        &serde_json::to_vec(&aliases).map_err(err500)?,
    ))
}
pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let root = group(&state, id).await?;
    Ok(Html(
        FundFormTemplate {
            heading: "添加基金".into(),
            action: format!("/accounts/{id}/funds"),
            parent_id: id,
            parent_name: crypto::decrypt_string(&dek, &root.name),
            currency: root.currency,
            name: String::new(),
            aliases: String::new(),
            note: String::new(),
        }
        .render()
        .map_err(err500)?,
    ))
}
pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<FundFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad("基金名称不能为空"));
    }
    let _guard = state.balance_writes.lock().await;
    let root = group(&state, id).await?;
    account::ActiveModel {
        name: Set(crypto::encrypt(&dek, form.name.trim().as_bytes())),
        kind: Set("investment_fund".into()),
        currency: Set(root.currency),
        parent_id: Set(Some(id)),
        balance_offset: Set(crypto::encrypt_cents(&dek, 0)),
        sms_names: Set(encrypted_aliases(&dek, &form.aliases)?),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to(&format!("/accounts/{id}/funds")))
}
pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let a = fund(&state, id).await?;
    let root = group(&state, a.parent_id.unwrap()).await?;
    let mut aliases = investment_funds::sms_names(&dek, &a)?;
    aliases.remove(0);
    Ok(Html(
        FundFormTemplate {
            heading: "编辑基金".into(),
            action: format!("/funds/{id}/edit"),
            parent_id: root.id,
            parent_name: crypto::decrypt_string(&dek, &root.name),
            currency: root.currency,
            name: crypto::decrypt_string(&dek, &a.name),
            aliases: aliases.join("\n"),
            note: crypto::decrypt_string(&dek, &a.note),
        }
        .render()
        .map_err(err500)?,
    ))
}
pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<FundFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad("基金名称不能为空"));
    }
    let _guard = state.balance_writes.lock().await;
    let a = fund(&state, id).await?;
    let parent_id = a.parent_id.unwrap();
    let mut active = a.into_active_model();
    active.name = Set(crypto::encrypt(&dek, form.name.trim().as_bytes()));
    active.sms_names = Set(encrypted_aliases(&dek, &form.aliases)?);
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to(&format!("/accounts/{parent_id}/funds")))
}
pub async fn delete(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let a = fund(&state, id).await?;
    investment_funds::ensure_unused(&state, &dek, id).await?;
    account::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to(&format!(
        "/accounts/{}/funds",
        a.parent_id.unwrap()
    )))
}
#[derive(Template)]
#[template(path = "fund_valuation.html")]
struct ValuationTemplate {
    id: i64,
    name: String,
    currency: String,
    total: String,
    funds: Vec<FundRow>,
}
pub async fn valuation_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let root = group(&state, id).await?;
    let (funds, total) = rows(&state, &dek, &root).await?;
    Ok(Html(
        ValuationTemplate {
            id,
            name: crypto::decrypt_string(&dek, &root.name),
            currency: root.currency.clone(),
            total: currency::format(total, &root.currency),
            funds,
        }
        .render()
        .map_err(err500)?,
    ))
}
fn parse_target(text: &str) -> HandlerResult<i64> {
    let decimal = Decimal::from_str(text.trim()).map_err(|_| bad("持仓价值格式不正确"))?;
    let cents = decimal
        .checked_mul(Decimal::from(100))
        .ok_or_else(|| bad("持仓价值超出范围"))?;
    if cents.fract() != Decimal::ZERO || cents < Decimal::ZERO {
        return Err(bad("持仓价值必须非负且最多两位小数"));
    }
    cents.to_i64().ok_or_else(|| bad("持仓价值超出范围"))
}
pub async fn calibrate(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> HandlerResult<Redirect> {
    let _guard = state.balance_writes.lock().await;
    let root = group(&state, id).await?;
    let funds = children(&state, &root).await?;
    let ids: HashSet<_> = funds.iter().map(|a| a.id).collect();
    for key in form.keys() {
        let row_id = ["target_", "baseline_", "reviewed_"]
            .into_iter()
            .find_map(|prefix| key.strip_prefix(prefix))
            .ok_or_else(|| bad("估值字段无效"))?
            .parse::<i64>()
            .map_err(|_| bad("估值基金无效"))?;
        if !ids.contains(&row_id) {
            return Err(bad("基金不属于当前投资分组"));
        }
    }
    let tx = state.db.begin().await.map_err(err500)?;
    let now = chrono::Utc::now();
    let mut total = 0i64;
    for a in funds {
        let target_text = form
            .get(&format!("target_{}", a.id))
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let reviewed = form.contains_key(&format!("reviewed_{}", a.id));
        let change = if target_text.is_empty() && !reviewed {
            None
        } else {
            let baseline = form
                .get(&format!("baseline_{}", a.id))
                .ok_or_else(|| bad("缺少估值基准，请重新打开页面"))?
                .parse::<i64>()
                .map_err(|_| bad("估值基准无效"))?;
            let target = if target_text.is_empty() {
                baseline
            } else {
                parse_target(target_text)?
            };
            if target < 0 {
                return Err(bad("持仓价值必须非负"));
            }
            (target != baseline || reviewed).then_some((baseline, target))
        };
        let fresh = accounts::current_balance_on(&tx, &dek, a.id).await?;
        total = total
            .checked_add(change.map_or(fresh, |(_, target)| target))
            .ok_or_else(|| bad("投资分组总价值超出范围，本批次未保存"))?;
        let Some((baseline, target)) = change else {
            continue;
        };
        if fresh != baseline {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "基金 {} 已有新流水或估值变化；本批次未保存，请刷新后核对",
                    crypto::decrypt_string(&dek, &a.name)
                ),
            ));
        }
        let offset = crypto::decrypt_cents(&dek, &a.balance_offset)
            .checked_add(
                target
                    .checked_sub(fresh)
                    .ok_or_else(|| bad("估值差额超出范围"))?,
            )
            .ok_or_else(|| bad("持仓价值超出范围"))?;
        let account_id = a.id;
        let mut active = a.into_active_model();
        if target != fresh {
            active.balance_offset = Set(crypto::encrypt_cents(&dek, offset));
        }
        active.last_calibrated_at = Set(Some(now));
        active.update(&tx).await.map_err(err500)?;
        balance_adjustment::ActiveModel {
            account_id: Set(account_id),
            from_balance: Set(crypto::encrypt_cents(&dek, fresh)),
            to_balance: Set(crypto::encrypt_cents(&dek, target)),
            happened_at: Set(now.naive_utc()),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&tx)
        .await
        .map_err(err500)?;
    }
    tx.commit().await.map_err(err500)?;
    Ok(Redirect::to(&format!("/accounts/{id}/funds")))
}
#[derive(Default, Deserialize)]
pub struct RemindersQuery {
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
}
struct ReminderRow {
    parent_id: i64,
    name: String,
    parent_name: String,
    value: String,
    days_since: i64,
}
#[derive(Template)]
#[template(path = "fund_reminders.html")]
struct RemindersTemplate {
    reminders: Vec<ReminderRow>,
    per_page: usize,
    pagination: super::PaginationView,
}
pub async fn reminders(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<RemindersQuery>,
) -> HandlerResult<Html<String>> {
    let reminders = investment_funds::due_valuations(&state, &dek).await?;
    let count = reminders.len();
    let pagination = super::pagination(count, query.page, query.per_page);
    let reminders = reminders
        .into_iter()
        .skip(pagination.start)
        .take(pagination.per_page)
        .map(|r| ReminderRow {
            parent_id: r.parent_id,
            name: r.name,
            parent_name: r.parent_name,
            value: currency::format(r.amount, &r.currency),
            days_since: r.days_since,
        })
        .collect();
    Ok(Html(
        RemindersTemplate {
            reminders,
            per_page: pagination.per_page,
            pagination: super::pagination_view(
                &pagination,
                count,
                "/funds/valuation-reminders",
                "只基金",
                std::iter::empty::<(String, String)>(),
            ),
        }
        .render()
        .map_err(err500)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valuation_values_are_exact_nonnegative_cents() {
        assert_eq!(parse_target("0").unwrap(), 0);
        assert_eq!(parse_target(" 12.34 ").unwrap(), 1234);
        assert_eq!(parse_target("92233720368547758.07").unwrap(), i64::MAX);
        for invalid in ["-1", "0.001", "NaN", "", "92233720368547758.08"] {
            assert!(parse_target(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn aliases_are_encrypted_and_normalized_without_losing_spelling() {
        let dek = crypto::Dek::new([7; crypto::DEK_LEN]);
        let encrypted = encrypted_aliases(&dek, " 招商 基金 \n招商基金\n别名\n").unwrap();
        let aliases: Vec<String> =
            serde_json::from_slice(&crypto::decrypt(&dek, &encrypted).unwrap()).unwrap();
        assert_eq!(aliases, vec!["招商 基金", "别名"]);
    }
}
