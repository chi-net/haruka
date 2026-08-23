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
const DISPLAY_FMT: &str = "%Y-%m-%d %H:%M";

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

struct PersonOption {
    id: i64,
    name: String,
}

struct AccountOption {
    id: i64,
    name: String,
}

struct DebtRow {
    id: i64,
    person_name: String,
    account_name: String,
    kind_label: String,
    money_class: String,
    money_sign: String,
    amount: String,
    note: String,
    happened_at: String,
}

struct DebtPersonRow {
    id: i64,
    name: String,
    note: String,
    receivable: String,
    payable: String,
}

#[derive(Template)]
#[template(path = "debts.html")]
struct DebtsTemplate {
    people: Vec<PersonOption>,
    accounts: Vec<AccountOption>,
    records: Vec<DebtRow>,
    happened_at: String,
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

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "lend" => "我借给对方",
        "borrow" => "我向对方借入",
        "repayment_received" => "对方还给我",
        "repayment_paid" => "我还给对方",
        _ => "未知",
    }
}

fn account_money_direction(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "borrow" | "repayment_received" => ("+", "text-green-600"),
        _ => ("-", "text-red-600"),
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

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    let people = debt_person::Entity::find()
        .order_by_asc(debt_person::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let accounts = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let person_names: HashMap<i64, String> = people
        .iter()
        .map(|person| (person.id, crypto::decrypt_string(&dek, &person.name)))
        .collect();
    let account_names: HashMap<i64, String> = accounts
        .iter()
        .map(|account| (account.id, crypto::decrypt_string(&dek, &account.name)))
        .collect();
    let records = debt_record::Entity::find()
        .order_by_desc(debt_record::Column::HappenedAt)
        .order_by_desc(debt_record::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|record| {
            let (money_sign, money_class) = account_money_direction(&record.kind);
            DebtRow {
                id: record.id,
                person_name: person_names
                    .get(&record.person_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除对象".into()),
                account_name: account_names
                    .get(&record.account_id)
                    .cloned()
                    .unwrap_or_else(|| "已删除账户".into()),
                kind_label: kind_label(&record.kind).into(),
                money_class: money_class.into(),
                money_sign: money_sign.into(),
                amount: super::fmt_cents(crypto::decrypt_cents(&dek, &record.amount)),
                note: crypto::decrypt_string(&dek, &record.note),
                happened_at: record.happened_at.format(DISPLAY_FMT).to_string(),
            }
        })
        .collect();
    let html = DebtsTemplate {
        people: people
            .into_iter()
            .map(|person| PersonOption {
                id: person.id,
                name: person_names.get(&person.id).cloned().unwrap_or_default(),
            })
            .collect(),
        accounts: accounts
            .into_iter()
            .map(|account| AccountOption {
                id: account.id,
                name: account_names.get(&account.id).cloned().unwrap_or_default(),
            })
            .collect(),
        records,
        happened_at: chrono::Local::now()
            .naive_local()
            .format(TIME_FMT)
            .to_string(),
    }
    .render()
    .map_err(err500)?;
    Ok(Html(html))
}

pub async fn create_record(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<DebtRecordFormData>,
) -> HandlerResult<Redirect> {
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
    Ok(Redirect::to("/debts"))
}

pub async fn delete_record(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    debt_record::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/debts"))
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
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    debt_person::Entity::delete_by_id(id)
        .exec(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/debt-people"))
}
