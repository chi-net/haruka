use askama::Template;
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
    Form, Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{
    crypto,
    entity::{account, account_detail, category, recurring_investment, sms_template},
    sms_templates::{decode_template, match_template, validate_config, TemplateConfig},
    AppState, SessionDek,
};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn bad_request(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

fn action_label(action: &str) -> &'static str {
    match action {
        "audit" => "仅保留审计记录",
        "investment_success" => "定投扣款成功",
        "investment_failure" => "定投扣款失败",
        "transfer_out" => "转出候选（手动确认）",
        "transfer_in" => "转入候选（手动确认）",
        "expense" => "自动生成支出账单",
        "income" => "自动生成收入账单",
        _ => "未知动作",
    }
}

#[derive(Default, Deserialize)]
pub struct TemplateQuery {
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    page: usize,
    #[serde(default)]
    per_page: usize,
}

#[derive(Default, Deserialize)]
pub struct TemplateFormData {
    #[serde(default)]
    name: String,
    #[serde(default)]
    sender: String,
    #[serde(default)]
    bank_tag: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    plan_id: String,
    #[serde(default)]
    category_id: String,
    #[serde(default)]
    enabled: Option<String>,
}

#[derive(Deserialize)]
pub struct PreviewData {
    #[serde(flatten)]
    config: TemplateFormData,
    #[serde(default)]
    time: String,
    #[serde(default)]
    sender_sample: String,
    #[serde(default)]
    raw: String,
}

#[derive(Serialize)]
pub struct PreviewResponse {
    ok: bool,
    matched: bool,
    amount: String,
    content: String,
    template_name: String,
    action: String,
    last4: String,
    fund: String,
}

struct TemplateRow {
    id: i64,
    name: String,
    sender: String,
    bank_tag: String,
    pattern: String,
    action_label: String,
    references: String,
    enabled: bool,
}

#[derive(Template)]
#[template(path = "sms_templates.html")]
struct TemplatesPage {
    templates: Vec<TemplateRow>,
    keyword: String,
    status: String,
    per_page: usize,
    pagination: super::PaginationView,
}

struct ReferenceOption {
    id: i64,
    name: String,
    kind: String,
    selected: bool,
}

struct References {
    accounts: Vec<ReferenceOption>,
    plans: Vec<ReferenceOption>,
    categories: Vec<ReferenceOption>,
}

#[derive(Template)]
#[template(path = "sms_template_form.html")]
struct TemplateFormPage {
    heading: String,
    submit_action: String,
    config: TemplateConfig,
    accounts: Vec<ReferenceOption>,
    plans: Vec<ReferenceOption>,
    categories: Vec<ReferenceOption>,
    preview_time: String,
}

fn parse_id(value: &str, label: &str) -> HandlerResult<Option<i64>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let id = value
        .trim()
        .parse::<i64>()
        .map_err(|_| bad_request(format!("{label}编号无效，请从列表选择")))?;
    if id <= 0 {
        return Err(bad_request(format!("{label}编号必须大于 0")));
    }
    Ok(Some(id))
}

fn parse_config(id: i64, form: TemplateFormData) -> HandlerResult<TemplateConfig> {
    let action = form.action.trim().to_string();
    // 不属于当前动作的旧选择不会成为隐式关联，也不要求解析。
    let account_id = if matches!(
        action.as_str(),
        "transfer_out" | "transfer_in" | "expense" | "income"
    ) {
        parse_id(&form.account_id, "账户")?
    } else {
        None
    };
    let plan_id = if matches!(action.as_str(), "investment_success" | "investment_failure") {
        parse_id(&form.plan_id, "定投计划")?
    } else {
        None
    };
    let category_id = if matches!(action.as_str(), "expense" | "income") {
        parse_id(&form.category_id, "分类")?
    } else {
        None
    };
    let bank_tag = form.bank_tag.trim();
    let bank_tag = bank_tag
        .strip_prefix('【')
        .and_then(|tag| tag.strip_suffix('】'))
        .unwrap_or(bank_tag);
    let config = TemplateConfig {
        id,
        name: form.name.trim().to_string(),
        sender: form.sender.trim().to_string(),
        bank_tag: bank_tag.trim().to_string(),
        pattern: form.pattern.trim().to_string(),
        action,
        account_id,
        plan_id,
        category_id,
        enabled: form.enabled.is_some(),
    };
    validate_config(&config).map_err(bad_request)?;
    Ok(config)
}

async fn validate_references(state: &AppState, config: &TemplateConfig) -> HandlerResult<()> {
    if let Some(id) = config.account_id {
        let item = account::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("所选账户不存在，请重新选择"))?;
        crate::investment_funds::validate_money_account(state, &item).await?;
    }
    if let Some(id) = config.plan_id {
        let plan = recurring_investment::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("所选定投计划不存在，请重新选择"))?;
        if !plan.active || plan.strategy != "sms" {
            return Err(bad_request("只能绑定启用中的短信实扣定投计划"));
        }
        let from = account::Entity::find_by_id(plan.from_account_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("定投计划扣款账户已删除，请先修正计划"))?;
        let fund = account::Entity::find_by_id(plan.fund_account_id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("定投计划基金已删除，请先修正计划"))?;
        super::investments::validate_plan_accounts(state, &from, &fund, &plan.strategy).await?;
    }
    if let Some(id) = config.category_id {
        let item = category::Entity::find_by_id(id)
            .one(&state.db)
            .await
            .map_err(err500)?
            .ok_or_else(|| bad_request("所选分类不存在，请重新选择"))?;
        if item.kind != config.action {
            return Err(bad_request("分类类型必须与收入／支出动作一致"));
        }
    }
    Ok(())
}

async fn load_references(
    state: &AppState,
    dek: &crypto::Dek,
    config: &TemplateConfig,
) -> HandlerResult<References> {
    let details: HashMap<i64, account_detail::Model> = account_detail::Entity::find()
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|item| (item.account_id, item))
        .collect();
    let account_models = account::Entity::find()
        .order_by_asc(account::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?;
    let names = crate::investment_funds::display_names(dek, &account_models);
    let accounts = account_models
        .iter()
        .filter(|item| crate::investment_funds::is_money_account(item))
        .map(|item| ReferenceOption {
            id: item.id,
            name: if item.kind == "investment_fund" {
                names.get(&item.id).cloned().unwrap_or_default()
            } else {
                super::bills::account_display_name(dek, item, details.get(&item.id))
            },
            kind: item.kind.clone(),
            selected: config.account_id == Some(item.id),
        })
        .collect();
    let plans = recurring_investment::Entity::find()
        .order_by_asc(recurring_investment::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|item| item.active && item.strategy == "sms")
        .map(|item| ReferenceOption {
            id: item.id,
            name: format!(
                "{} · {}",
                crypto::decrypt_string(dek, &item.name),
                names
                    .get(&item.fund_account_id)
                    .map(String::as_str)
                    .unwrap_or("已删除基金")
            ),
            kind: String::new(),
            selected: config.plan_id == Some(item.id),
        })
        .collect();
    let categories = category::Entity::find()
        .order_by_asc(category::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .filter(|item| matches!(item.kind.as_str(), "income" | "expense"))
        .map(|item| ReferenceOption {
            id: item.id,
            name: crypto::decrypt_string(dek, &item.name),
            kind: item.kind,
            selected: config.category_id == Some(item.id),
        })
        .collect();
    Ok(References {
        accounts,
        plans,
        categories,
    })
}

fn empty_config() -> TemplateConfig {
    TemplateConfig {
        id: 0,
        name: String::new(),
        sender: String::new(),
        bank_tag: String::new(),
        pattern: String::new(),
        action: "audit".into(),
        account_id: None,
        plan_id: None,
        category_id: None,
        enabled: true,
    }
}

fn describe_reference(options: &[ReferenceOption], id: Option<i64>, label: &str) -> Option<String> {
    id.map(|id| {
        options
            .iter()
            .find(|item| item.id == id)
            .map(|item| format!("{label}：{}", item.name))
            .unwrap_or_else(|| format!("{label}：#{id}（已删除或不可用）"))
    })
}

pub async fn list(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Query(query): Query<TemplateQuery>,
) -> HandlerResult<Html<String>> {
    if !matches!(query.status.as_str(), "" | "enabled" | "disabled") {
        return Err(bad_request("模板状态筛选无效"));
    }
    let refs = load_references(&state, &dek, &empty_config()).await?;
    let keyword = query.keyword.trim().to_lowercase();
    let rows = sms_template::Entity::find()
        .order_by_desc(sms_template::Column::Id)
        .all(&state.db)
        .await
        .map_err(err500)?
        .into_iter()
        .map(|item| decode_template(&dek, &item))
        .map(|config| {
            let references = [
                describe_reference(&refs.accounts, config.account_id, "账户"),
                describe_reference(&refs.plans, config.plan_id, "定投"),
                describe_reference(&refs.categories, config.category_id, "分类"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("；");
            TemplateRow {
                id: config.id,
                name: config.name,
                sender: config.sender,
                bank_tag: config.bank_tag,
                pattern: config.pattern,
                action_label: action_label(&config.action).into(),
                references,
                enabled: config.enabled,
            }
        })
        .filter(|row| {
            (query.status.is_empty() || row.enabled == (query.status == "enabled"))
                && (keyword.is_empty()
                    || format!(
                        "{} {} {} {} {} {}",
                        row.name,
                        row.sender,
                        row.bank_tag,
                        row.pattern,
                        row.action_label,
                        row.references
                    )
                    .to_lowercase()
                    .contains(&keyword))
        })
        .collect::<Vec<_>>();
    let total = rows.len();
    let pagination = super::pagination(total, query.page, query.per_page);
    let page = TemplatesPage {
        templates: rows
            .into_iter()
            .skip(pagination.start)
            .take(pagination.per_page)
            .collect(),
        keyword: query.keyword.clone(),
        status: query.status.clone(),
        per_page: pagination.per_page,
        pagination: super::pagination_view(
            &pagination,
            total,
            "/sms/templates",
            "个模板",
            [("keyword", query.keyword), ("status", query.status)],
        ),
    };
    Ok(Html(page.render().map_err(err500)?))
}

async fn get_template(state: &AppState, id: i64) -> HandlerResult<sms_template::Model> {
    sms_template::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(err500)?
        .ok_or((StatusCode::NOT_FOUND, "短信模板不存在".into()))
}

async fn render_form(
    state: &AppState,
    dek: &crypto::Dek,
    config: TemplateConfig,
) -> HandlerResult<Html<String>> {
    let mut refs = load_references(state, dek, &config).await?;
    for (options, id) in [
        (&mut refs.accounts, config.account_id),
        (&mut refs.plans, config.plan_id),
        (&mut refs.categories, config.category_id),
    ] {
        if let Some(id) = id {
            if !options.iter().any(|item| item.id == id) {
                options.push(ReferenceOption {
                    id,
                    name: format!("#{id}（已删除或不可用，请重新选择）"),
                    kind: config.action.clone(),
                    selected: true,
                });
            }
        }
    }
    let editing = config.id != 0;
    let page = TemplateFormPage {
        heading: if editing {
            "编辑短信模板"
        } else {
            "新增短信模板"
        }
        .into(),
        submit_action: if editing {
            format!("/sms/templates/{}/edit", config.id)
        } else {
            "/sms/templates".into()
        },
        config,
        accounts: refs.accounts,
        plans: refs.plans,
        categories: refs.categories,
        preview_time: Utc::now()
            .with_timezone(&chrono_tz::Asia::Shanghai)
            .to_rfc3339(),
    };
    Ok(Html(page.render().map_err(err500)?))
}

pub async fn new_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
) -> HandlerResult<Html<String>> {
    render_form(&state, &dek, empty_config()).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Html<String>> {
    let row = get_template(&state, id).await?;
    render_form(&state, &dek, decode_template(&dek, &row)).await
}

fn apply_config(model: &mut sms_template::ActiveModel, dek: &crypto::Dek, config: &TemplateConfig) {
    model.name = Set(crypto::encrypt(dek, config.name.as_bytes()));
    model.sender = Set(crypto::encrypt(dek, config.sender.as_bytes()));
    model.bank_tag = Set(crypto::encrypt(dek, config.bank_tag.as_bytes()));
    model.pattern = Set(crypto::encrypt(dek, config.pattern.as_bytes()));
    model.action = Set(config.action.clone());
    model.account_id = Set(config.account_id);
    model.plan_id = Set(config.plan_id);
    model.category_id = Set(config.category_id);
    model.enabled = Set(config.enabled);
}

pub async fn create(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Form(form): Form<TemplateFormData>,
) -> HandlerResult<Redirect> {
    let config = parse_config(0, form)?;
    validate_references(&state, &config).await?;
    let mut model = sms_template::ActiveModel {
        created_at: Set(Utc::now()),
        ..Default::default()
    };
    apply_config(&mut model, &dek, &config);
    model.insert(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/sms/templates"))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
    Form(form): Form<TemplateFormData>,
) -> HandlerResult<Redirect> {
    let mut model = get_template(&state, id).await?.into_active_model();
    let config = parse_config(id, form)?;
    validate_references(&state, &config).await?;
    apply_config(&mut model, &dek, &config);
    model.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/sms/templates"))
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> HandlerResult<Redirect> {
    get_template(&state, id)
        .await?
        .into_active_model()
        .delete(&state.db)
        .await
        .map_err(err500)?;
    Ok(Redirect::to("/sms/templates"))
}

pub async fn toggle(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(id): Path<i64>,
) -> HandlerResult<Redirect> {
    let row = get_template(&state, id).await?;
    let mut config = decode_template(&dek, &row);
    config.enabled = !config.enabled;
    if config.enabled {
        validate_config(&config).map_err(bad_request)?;
        validate_references(&state, &config).await?;
    }
    let mut model = row.into_active_model();
    model.enabled = Set(config.enabled);
    model.update(&state.db).await.map_err(err500)?;
    Ok(Redirect::to("/sms/templates"))
}

pub async fn preview(
    State(state): State<AppState>,
    Extension(SessionDek(_dek)): Extension<SessionDek>,
    Json(data): Json<PreviewData>,
) -> HandlerResult<Json<PreviewResponse>> {
    let mut config = parse_config(0, data.config)?;
    validate_references(&state, &config).await?;
    // 预览单个草稿，不受保存后的启用状态影响；此接口只查询，不写入任何表。
    config.enabled = true;
    let time = DateTime::parse_from_rfc3339(data.time.trim())
        .map_err(|_| {
            bad_request("样例时间必须为带时区的 ISO8601 格式，例如 2026-10-08T12:30:00+08:00")
        })?
        .with_timezone(&Utc);
    if data.raw.trim().is_empty() || data.raw.len() > 4096 {
        return Err(bad_request("样例短信不能为空，且不能超过 4096 字节"));
    }
    if data.sender_sample.trim().chars().count() > 128 {
        return Err(bad_request("样例发送号码不能超过 128 个字符"));
    }
    let matched =
        match_template(&config, data.sender_sample.trim(), &data.raw, time).map_err(bad_request)?;
    let response = match matched {
        Some(item) => PreviewResponse {
            ok: true,
            matched: true,
            amount: item.amount.map(super::fmt_cents).unwrap_or_default(),
            content: item.content,
            template_name: item.template_name,
            action: item.action,
            last4: item.last4,
            fund: item.fund,
        },
        None => PreviewResponse {
            ok: true,
            matched: false,
            amount: String::new(),
            content: String::new(),
            template_name: config.name,
            action: config.action,
            last4: String::new(),
            fund: String::new(),
        },
    };
    Ok(Json(response))
}
