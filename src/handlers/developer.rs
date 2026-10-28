//! 开发者模式：系统指标、软件信息、调用栈、日志、解密数据库编辑与 SQL 执行。
//! 所有路由均受 `require_developer` 保护（需先解锁并通过设置页 Konami 序列开启）。

use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use sea_orm::{
    ConnectionTrait, DbBackend, QueryResult, Statement, Value,
};
use serde::Deserialize;

use crate::{crypto, AppState, SessionDek};

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn err500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}
#[derive(Template)]
#[template(path = "developer.html")]
struct DeveloperTemplate {
    version: String,
    commit_count: &'static str,
    commit_sha: &'static str,
    listen_addr: String,
    database_url: String,
    os: &'static str,
    arch: &'static str,
    started_seconds: u64,
    tables: Vec<String>,
}

pub async fn page(State(state): State<AppState>) -> HandlerResult<Html<String>> {
    let tables = state
        .db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        ))
        .await
        .map_err(err500)?
        .iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()
        .map_err(err500)?;
    Ok(Html(
        DeveloperTemplate {
            version: crate::version::display(),
            commit_count: crate::version::COMMIT_COUNT,
            commit_sha: crate::version::COMMIT_SHA,
            listen_addr: state.listen_addr.clone(),
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://haruka.db?mode=rwc".into()),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            started_seconds: state.started_at.elapsed().as_secs(),
            tables,
        }
        .render()
        .map_err(err500)?,
    ))
}

pub async fn metrics(State(state): State<AppState>) -> HandlerResult<Json<serde_json::Value>> {
    let pid = sysinfo::get_current_pid().map_err(err500)?;
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    // 进程 CPU 使用率需要两次采样间隔才有意义。
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    system.refresh_cpu_all();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let process = system.process(pid).ok_or_else(|| err500("无法读取当前进程"))?;
    let load = sysinfo::System::load_average();
    let cpu_percent = if system.cpus().is_empty() {
        0.0
    } else {
        system.cpus().iter().map(|cpu| cpu.cpu_usage()).sum::<f32>()
            / system.cpus().len() as f32
    };
    let session_count = state.developer_sessions.read().len();
    Ok(Json(serde_json::json!({
        "process": {
            "pid": pid.as_u32(),
            "rss_bytes": process.memory(),
            "virtual_bytes": process.virtual_memory(),
            "cpu_percent": process.cpu_usage(),
            "run_seconds": process.run_time(),
            "status": format!("{:?}", process.status()),
        },
        "system": {
            "total_memory": system.total_memory(),
            "used_memory": system.used_memory(),
            "available_memory": system.available_memory(),
            "cpu_percent": cpu_percent,
            "cores": system.cpus().len(),
            "load_one": load.one,
            "load_five": load.five,
            "load_fifteen": load.fifteen,
            "uptime_seconds": sysinfo::System::uptime(),
        },
        "app": {
            "started_seconds": state.started_at.elapsed().as_secs(),
            "developer_sessions": session_count,
        },
    })))
}

pub async fn logs() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "entries": crate::dev_log::entries() }))
}

pub async fn backtrace() -> Json<serde_json::Value> {
    // Rust 无法可移植地枚举其他线程栈；捕获当前请求处理线程的完整调用栈。
    let trace = format!("{:?}", backtrace::Backtrace::new());
    Json(serde_json::json!({ "thread": "当前请求处理线程", "backtrace": trace }))
}

pub async fn check_updates(State(state): State<AppState>) -> Json<crate::version::UpdateStatus> {
    Json(crate::version::check_updates(&state.fx_client).await)
}

/// 加密列类型：DEK 文本、DEK 整数分、独立密钥（无法在此解密）。
#[derive(Clone, Copy, PartialEq)]
enum EncKind {
    Text,
    Cents,
    External,
}

/// 各表的加密列清单（AGENTS.md 字段级加密约定）；未列出的列为明文。
fn encrypted_column(table: &str, column: &str) -> Option<EncKind> {
    use EncKind::{Cents, External, Text};
    const TEXT: EncKind = Text;
    let kind = match (table, column) {
        ("accounts", "name" | "note" | "sms_names") => TEXT,
        ("accounts", "balance_offset") => Cents,
        ("account_details", "card_number" | "account_username") => TEXT,
        ("account_details", "credit_limit") => Cents,
        ("balance_adjustments", "from_balance" | "to_balance") => Cents,
        ("bills", "category" | "note") => TEXT,
        ("bills", "amount") => Cents,
        ("bill_shares", "token" | "label") => TEXT,
        ("bill_shares", "snapshot") => External,
        ("budgets", "daily_amount" | "weekly_amount" | "monthly_amount") => Cents,
        ("transfers", "note") => TEXT,
        ("transfers", "amount" | "to_amount" | "fee_rate_bps" | "fee_amount") => Cents,
        ("debt_people", "name" | "note") => TEXT,
        ("debt_records", "note") => TEXT,
        ("debt_records", "amount") => Cents,
        ("categories", "name") => TEXT,
        ("categories", "count_limit") => Cents,
        ("subscriptions", "name" | "category" | "note") => TEXT,
        ("subscriptions", "amount") => Cents,
        ("preferences", "owner_name") => TEXT,
        ("recurring_investments", "name" | "note") => TEXT,
        ("recurring_investments", "amount" | "fee_rate_bps") => Cents,
        ("investment_executions", "base_amount" | "multiplier_bps") => Cents,
        ("investment_sms_events", "raw" | "detail" | "sender" | "parsed") => TEXT,
        ("sms_templates", "name" | "sender" | "bank_tag" | "pattern") => TEXT,
        ("passkeys", "name") => TEXT,
        ("installment_plans", "annual_rate_bps" | "fee" | "principal" | "refund_reviewed_amount") => {
            Cents
        }
        ("installment_items", "principal" | "interest" | "fee" | "total") => Cents,
        ("financial_plans", "payload") => TEXT,
        ("debt_requests", "token" | "label") => TEXT,
        ("debt_requests", "settled_amount") => Cents,
        (
            "debt_requests",
            "snapshot" | "submission" | "repayment_submission" | "settlement",
        ) => External,
        ("recovery", _) => External,
        _ => return None,
    };
    Some(kind)
}

struct ColumnInfo {
    name: String,
    declared_type: String,
    pk: bool,
    encrypted: Option<EncKind>,
}

async fn table_columns(db: &sea_orm::DatabaseConnection, table: &str) -> HandlerResult<Vec<ColumnInfo>> {
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(bad_request("表名不合法"));
    }
    let rows = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA table_info(\"{table}\")"),
        ))
        .await
        .map_err(err500)?;
    let columns = rows
        .iter()
        .map(|row| {
            Ok(ColumnInfo {
                name: row.try_get::<String>("", "name").map_err(err500)?,
                declared_type: row.try_get::<String>("", "type").map_err(err500)?,
                pk: row.try_get::<i64>("", "pk").map_err(err500)? > 0,
                encrypted: None,
            })
        })
        .collect::<HandlerResult<Vec<_>>>()?;
    if columns.is_empty() {
        return Err(bad_request("数据表不存在"));
    }
    let mut columns = columns;
    for column in &mut columns {
        column.encrypted = encrypted_column(table, &column.name);
    }
    Ok(columns)
}

/// SQLite 动态类型：依次按文本、整数、浮点、BLOB 解码，NULL 统一显示为 "NULL"。
fn read_cell(row: &QueryResult, column: &str) -> String {
    if let Ok(value) = row.try_get::<Option<String>>("", column) {
        return value.unwrap_or_else(|| "NULL".into());
    }
    if let Ok(value) = row.try_get::<Option<i64>>("", column) {
        return value
            .map(|value| value.to_string())
            .unwrap_or_else(|| "NULL".into());
    }
    if let Ok(value) = row.try_get::<Option<f64>>("", column) {
        return value
            .map(|value| value.to_string())
            .unwrap_or_else(|| "NULL".into());
    }
    if let Ok(value) = row.try_get::<Option<Vec<u8>>>("", column) {
        return value
            .map(|bytes| {
                if bytes.is_empty() {
                    String::new()
                } else {
                    format!("0x{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
                }
            })
            .unwrap_or_else(|| "NULL".into());
    }
    "<不支持的类型>".into()
}

fn cell_display(dek: &crypto::Dek, column: &ColumnInfo, raw: String) -> String {
    match column.encrypted {
        Some(EncKind::External) if !raw.is_empty() && raw != "NULL" => "<独立密钥密文>".into(),
        Some(EncKind::Text) => {
            if raw.is_empty() {
                raw
            } else {
                crypto::decrypt_string(dek, &raw)
            }
        }
        Some(EncKind::Cents) => {
            if raw.is_empty() {
                raw
            } else {
                crypto::decrypt_cents(dek, &raw).to_string()
            }
        }
        _ => raw,
    }
}

#[derive(Template)]
#[template(path = "developer_db.html")]
struct TableTemplate {
    table: String,
    columns: Vec<ColumnView>,
    rows: Vec<RowView>,
    pk: String,
    page: usize,
    total_pages: usize,
    total_records: usize,
}

struct ColumnView {
    name: String,
    label: String,
    encrypted: bool,
    is_pk: bool,
}

struct RowView {
    id: i64,
    cells: Vec<String>,
}

#[derive(Deserialize)]
pub struct TableQuery {
    page: Option<usize>,
}

pub async fn table_view(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(table): Path<String>,
    axum::extract::Query(query): axum::extract::Query<TableQuery>,
) -> HandlerResult<Html<String>> {
    let columns = table_columns(&state.db, &table).await?;
    let total = state
        .db
        .query_one(Statement::from_string(
            DbBackend::Sqlite,
            format!("SELECT COUNT(*) AS value FROM \"{table}\""),
        ))
        .await
        .map_err(err500)?
        .ok_or_else(|| err500("无法统计表行数"))?
        .try_get::<i64>("", "value")
        .map_err(err500)? as usize;
    let per_page = 50usize;
    let total_pages = total.max(1).div_ceil(per_page);
    let page = query.page.unwrap_or(1).clamp(1, total_pages);
    let rows = state
        .db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "SELECT * FROM \"{table}\" ORDER BY rowid LIMIT {per_page} OFFSET {}",
                (page - 1) * per_page
            ),
        ))
        .await
        .map_err(err500)?;
    let pk = columns
        .iter()
        .find(|column| column.pk)
        .map(|column| column.name.clone())
        .unwrap_or_else(|| "rowid".into());
    let pk_index = columns.iter().position(|column| column.name == pk);
    let mut views = Vec::new();
    for row in &rows {
        let values = columns
            .iter()
            .map(|column| cell_display(&dek, column, read_cell(row, column.name.as_str())))
            .collect::<Vec<_>>();
        let id = pk_index
            .and_then(|_| row.try_get::<i64>("", pk.as_str()).ok())
            .unwrap_or_default();
        views.push(RowView { id, cells: values });
    }
    Ok(Html(
        TableTemplate {
            table,
            columns: columns
                .iter()
                .map(|column| ColumnView {
                    name: column.name.clone(),
                    label: format!(
                        "{} · {}{}",
                        column.name,
                        column.declared_type,
                        match column.encrypted {
                            Some(EncKind::Text) | Some(EncKind::Cents) => " · DEK加密",
                            Some(EncKind::External) => " · 独立密钥",
                            None => "",
                        }
                    ),
                    encrypted: column.encrypted.is_some(),
                    is_pk: column.name == pk,
                })
                .collect(),
            rows: views,
            pk,
            page,
            total_pages,
            total_records: total,
        }
        .render()
        .map_err(err500)?,
    ))
}

#[derive(Deserialize)]
pub struct CellForm {
    id: i64,
    column: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    null: bool,
}

pub async fn update_cell(
    State(state): State<AppState>,
    Extension(SessionDek(dek)): Extension<SessionDek>,
    Path(table): Path<String>,
    Json(form): Json<CellForm>,
) -> HandlerResult<Response> {
    if !form
        .column
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(bad_request("列名不合法"));
    }
    let columns = table_columns(&state.db, &table).await?;
    let column = columns
        .iter()
        .find(|column| column.name == form.column)
        .ok_or_else(|| bad_request("数据列不存在"))?;
    if column.pk {
        return Err(bad_request("主键列不允许修改"));
    }
    let pk = columns
        .iter()
        .find(|column| column.pk)
        .map(|column| column.name.clone())
        .ok_or_else(|| bad_request("该表没有单列主键，请使用 SQL 执行器修改"))?;
    let value = if form.null {
        if column.encrypted.is_some() {
            return Err(bad_request("加密列不支持直接置 NULL"));
        }
        Value::String(None)
    } else {
        match column.encrypted {
            Some(EncKind::Text) => Value::from(crypto::encrypt(&dek, form.value.as_bytes())),
            Some(EncKind::Cents) => {
                if form.value.trim().is_empty() {
                    Value::from(String::new())
                } else {
                    let cents = form
                        .value
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| bad_request("加密整数列需要整数分值"))?;
                    Value::from(crypto::encrypt_cents(&dek, cents))
                }
            }
            Some(EncKind::External) => {
                return Err(bad_request("独立密钥加密的列不能在此编辑"));
            }
            None => {
                let upper = column.declared_type.to_ascii_uppercase();
                if upper.contains("INT") {
                    Value::from(
                        form.value
                            .trim()
                            .parse::<i64>()
                            .map_err(|_| bad_request("该列需要整数值"))?,
                    )
                } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB")
                {
                    Value::from(
                        form.value
                            .trim()
                            .parse::<f64>()
                            .map_err(|_| bad_request("该列需要数值"))?,
                    )
                } else {
                    Value::from(form.value.clone())
                }
            }
        }
    };
    let sql = format!("UPDATE \"{table}\" SET \"{}\" = ? WHERE \"{pk}\" = ?", form.column);
    let result = state
        .db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            vec![value, Value::from(form.id)],
        ))
        .await
        .map_err(err500)?;
    if result.rows_affected() != 1 {
        return Err(bad_request("目标行不存在，未修改任何记录"));
    }
    crate::dev_log::warn(format!(
        "开发者修改数据库：{table}.{}（行 {}）",
        form.column, form.id
    ));
    Ok((StatusCode::OK, "已保存").into_response())
}

#[derive(Deserialize)]
pub struct SqlForm {
    sql: String,
    #[serde(default)]
    confirm: bool,
}

pub async fn execute_sql(
    State(state): State<AppState>,
    Json(form): Json<SqlForm>,
) -> HandlerResult<Json<serde_json::Value>> {
    let sql = form.sql.trim();
    if sql.is_empty() {
        return Err(bad_request("SQL 语句不能为空"));
    }
    if sql.matches(';').count() > 1 || sql.trim_end_matches(';').contains(';') {
        return Err(bad_request("一次只执行一条 SQL 语句"));
    }
    let keyword = sql
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_query = matches!(keyword.as_str(), "SELECT" | "WITH" | "EXPLAIN" | "PRAGMA" | "VALUES");
    if !is_query && !form.confirm {
        return Err(bad_request("写入或结构变更语句需要勾选确认后才能执行"));
    }
    crate::dev_log::warn(format!("开发者执行 SQL：{sql}"));
    if is_query {
        let rows = state
            .db
            .query_all(Statement::from_string(DbBackend::Sqlite, sql.to_string()))
            .await
            .map_err(|error| bad_request(&format!("查询失败：{error}")))?;
        let columns = rows
            .first()
            .map(|row| row.column_names().to_vec())
            .unwrap_or_default();
        let truncated = rows.len() > 500;
        let data = rows
            .iter()
            .take(500)
            .map(|row| {
                columns
                    .iter()
                    .map(|name| {
                        read_cell(row, name.as_str())
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        Ok(Json(serde_json::json!({
            "ok": true,
            "kind": "query",
            "columns": columns,
            "rows": data,
            "truncated": truncated,
        })))
    } else {
        let result = state
            .db
            .execute(Statement::from_string(DbBackend::Sqlite, sql.to_string()))
            .await
            .map_err(|error| bad_request(&format!("执行失败：{error}")))?;
        Ok(Json(serde_json::json!({
            "ok": true,
            "kind": "execute",
            "affected": result.rows_affected(),
        })))
    }
}
