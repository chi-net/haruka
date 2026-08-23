use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form,
};
use chrono::NaiveDateTime;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder, Set,
};
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

use crate::{
    crypto,
    entity::{account, debt_person, debt_record},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;
const TIME_FMT: &str = "%Y-%m-%dT%H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct DebtPersonRow {
    id: i64,
    name: String,
    note: String,
    receivable: String,
    payable: String,
}

#[derive(Template)]
#[template(path = "debt_people.html")]
struct DebtPeopleTemplate {
    people: Vec<DebtPersonRow>,
}

#[derive(Template)]
#[template(path = "debt_person_form.html")]
struct DebtPersonFormTemplate {
    heading: String,
    action: String,
    name: String,
    note: String,
}

#[derive(Deserialize)]
pub struct DebtRecordFormData {
    person_id: i64,
    account_id: i64,
    kind: String,
    amount: String,
    note: String,
    happened_at: String,
}

#[derive(Deserialize)]
pub struct DeleteRecordFormData {
    #[serde(default)]
    redirect_to: Option<String>,
}

#[derive(Deserialize)]
pub struct DebtPersonFormData {
    name: String,
    note: String,
}

fn valid_kind(kind: &str) -> bool {
    matches!(
        kind,
        "lend" | "borrow" | "repayment_received" | "repayment_paid"
    )
}

fn account_delta(kind: &str, amount: i64) -> HandlerResult<i64> {
    if matches!(kind, "borrow" | "repayment_received") {
        Ok(amount)
    } else {
        amount
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))
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

fn apply_outstanding(
    kind: &str,
    amount: i64,
    receivable: &mut i64,
    payable: &mut i64,
) -> HandlerResult<()> {
    match kind {
        "lend" => {
            *receivable = receivable
                .checked_add(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        "repayment_received" => {
            *receivable = receivable
                .checked_sub(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        "borrow" => {
            *payable = payable
                .checked_add(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        "repayment_paid" => {
            *payable = payable
                .checked_sub(amount)
                .ok_or_else(|| err500("借贷金额超出范围"))?
        }
        _ => {}
    }
    Ok(())
}

async fn person_outstanding(
    state: &AppState,
    dek: &crypto::Dek,
    person_id: i64,
) -> HandlerResult<(i64, i64)> {
    let records = debt_record::Entity::find()
        .filter(debt_record::Column::PersonId.eq(person_id))
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut receivable = 0;
    let mut payable = 0;
    for record in records {
        apply_outstanding(
            &record.kind,
            crypto::decrypt_cents(dek, &record.amount),
            &mut receivable,
            &mut payable,
        )?;
    }
    Ok((receivable, payable))
}

pub async fn create_record(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<DebtRecordFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    if !valid_kind(&form.kind) {
        return Err(bad_request("借还类型无效"));
    }
    if debt_person::Entity::find_by_id(form.person_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err(bad_request("借贷对象不存在"));
    }
    if account::Entity::find_by_id(form.account_id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err(bad_request("账户不存在"));
    }
    let amount = parse_amount(&form.amount)?;
    let (receivable, payable) = person_outstanding(&state, &dek, form.person_id).await?;
    if form.kind == "repayment_received" && amount > receivable {
        return Err(bad_request("还款金额超过对方尚欠金额"));
    }
    if form.kind == "repayment_paid" && amount > payable {
        return Err(bad_request("还款金额超过尚欠对方金额"));
    }
    super::accounts::ensure_balance_delta(
        &state,
        &dek,
        form.account_id,
        account_delta(&form.kind, amount)?,
    )
    .await?;
    let happened_at = NaiveDateTime::parse_from_str(form.happened_at.trim(), TIME_FMT)
        .map_err(|_| bad_request("时间格式不正确"))?;
    debt_record::ActiveModel {
        person_id: Set(form.person_id),
        account_id: Set(form.account_id),
        kind: Set(form.kind),
        amount: Set(crypto::encrypt_cents(&dek, amount)),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        happened_at: Set(happened_at),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/dashboard"))
}

pub async fn delete_record(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DeleteRecordFormData>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    let record = debt_record::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借还记录不存在".into()))?;
    let delta = account_delta(&record.kind, crypto::decrypt_cents(&dek, &record.amount))?
        .checked_neg()
        .ok_or_else(|| bad_request("金额超出范围"))?;
    super::accounts::ensure_balance_delta(&state, &dek, record.account_id, delta).await?;
    debt_record::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to(
        if form.redirect_to.as_deref() == Some("/bills") {
            "/bills"
        } else {
            "/dashboard"
        },
    ))
}

pub async fn people(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let people = debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let records = debt_record::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?;
    let mut outstanding: HashMap<i64, (i64, i64)> = HashMap::new();
    for record in records {
        let entry = outstanding.entry(record.person_id).or_default();
        apply_outstanding(
            &record.kind,
            crypto::decrypt_cents(&dek, &record.amount),
            &mut entry.0,
            &mut entry.1,
        )?;
    }
    let rows = people
        .into_iter()
        .map(|person| {
            let (receivable, payable) = outstanding.get(&person.id).copied().unwrap_or_default();
            DebtPersonRow {
                id: person.id,
                name: crypto::decrypt_string(&dek, &person.name),
                note: crypto::decrypt_string(&dek, &person.note),
                receivable: super::fmt_cents(receivable),
                payable: super::fmt_cents(payable),
            }
        })
        .collect();
    let html = DebtPeopleTemplate { people: rows }
        .render()
        .map_err(err500)?;
    Ok(Html(html))
}

pub async fn new_person_form() -> Html<String> {
    Html(
        DebtPersonFormTemplate {
            heading: "新增借贷对象".into(),
            action: "/debt-people".into(),
            name: String::new(),
            note: String::new(),
        }
        .render()
        .expect("模板渲染失败"),
    )
}

pub async fn create_person(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<DebtPersonFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad_request("姓名不能为空"));
    }
    debt_person::ActiveModel {
        name: Set(crypto::encrypt(&dek, form.name.trim().as_bytes())),
        note: Set(crypto::encrypt(&dek, form.note.trim().as_bytes())),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&state.db)
    .await
    .map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}

pub async fn edit_person_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let person = debt_person::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借贷对象不存在".into()))?;
    let html = DebtPersonFormTemplate {
        heading: "编辑借贷对象".into(),
        action: format!("/debt-people/{id}/edit"),
        name: crypto::decrypt_string(&dek, &person.name),
        note: crypto::decrypt_string(&dek, &person.note),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn update_person(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<DebtPersonFormData>,
) -> HandlerResult<Redirect> {
    if form.name.trim().is_empty() {
        return Err(bad_request("姓名不能为空"));
    }
    let person = debt_person::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "借贷对象不存在".into()))?;
    let mut active = person.into_active_model();
    active.name = Set(crypto::encrypt(&dek, form.name.trim().as_bytes()));
    active.note = Set(crypto::encrypt(&dek, form.note.trim().as_bytes()));
    active.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}

pub async fn delete_person(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let _balance_guard = state.balance_writes.lock().await;
    if debt_person::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, "借贷对象不存在".into()));
    }
    let mut balance_changes: HashMap<i64, i64> = HashMap::new();
    for record in debt_record::Entity::find()
        .filter(debt_record::Column::PersonId.eq(id))
        .all(&state.db)
        .await
        .map_err(err500)?
    {
        let delta = account_delta(&record.kind, crypto::decrypt_cents(&dek, &record.amount))?
            .checked_neg()
            .ok_or_else(|| bad_request("金额超出范围"))?;
        let current = balance_changes
            .get(&record.account_id)
            .copied()
            .unwrap_or_default();
        balance_changes.insert(
            record.account_id,
            current
                .checked_add(delta)
                .ok_or_else(|| bad_request("余额超出范围"))?,
        );
    }
    for (account_id, delta) in balance_changes {
        super::accounts::ensure_balance_delta(&state, &dek, account_id, delta).await?;
    }
    debt_person::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}
