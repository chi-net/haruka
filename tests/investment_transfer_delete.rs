use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{Datelike, Duration as Days, Utc, Weekday};
use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::{json, Value};

#[allow(dead_code)]
#[path = "../src/crypto.rs"]
mod crypto;

const PASSWORD: &str = "investment-transfer-delete-regression-only";
const TOKEN: &str = "investment-transfer-delete-sms-token";
const WHEN: &str = "2026-10-08 07:00:00";
const CREATED: &str = "2026-10-08T07:00:00+00:00";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestApp {
    child: Child,
    directory: PathBuf,
    db: DatabaseConnection,
    client: Client,
    url: String,
    cookie: String,
    dek: crypto::Dek,
    source: i64,
    fund: i64,
}

struct Execution {
    plan: i64,
    transfer: i64,
    fee: Option<i64>,
}

impl TestApp {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "haruka-investment-transfer-delete-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.join("haruka.db").display()
        );
        let child = Command::new(env!("CARGO_BIN_EXE_haruka"))
            .args(["--listen", &address.to_string()])
            .env("DATABASE_URL", &database_url)
            .env("SMS_API_TOKEN", TOKEN)
            .env_remove("PASSKEY_ORIGIN")
            .env_remove("PASSKEY_RP_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let url = format!("http://{address}");
        let deadline = Instant::now() + Duration::from_secs(20);
        while client.get(format!("{url}/setup")).send().await.is_err() {
            assert!(Instant::now() < deadline, "server startup failed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let response = client
            .post(format!("{url}/setup"))
            .form(&[("password", PASSWORD), ("confirm", PASSWORD)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let db = Database::connect(&database_url).await.unwrap();
        let meta = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT salt, dek_nonce, wrapped_dek FROM meta WHERE id = 1".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let salt: Vec<u8> = meta.try_get("", "salt").unwrap();
        let nonce: Vec<u8> = meta.try_get("", "dek_nonce").unwrap();
        let wrapped: Vec<u8> = meta.try_get("", "wrapped_dek").unwrap();
        let kek = crypto::derive_kek(PASSWORD, &salt);
        let dek = crypto::unwrap_dek(&nonce, &wrapped, kek.as_slice()).unwrap();
        let mut app = Self {
            child,
            directory,
            db,
            client,
            url,
            cookie,
            dek,
            source: 0,
            fund: 0,
        };
        app.source = app.account("bank").await;
        let group = app.account("investment").await;
        app.fund = app
            .scalar(&format!(
                "SELECT id AS value FROM accounts WHERE parent_id = {group}"
            ))
            .await;
        app.sql(
            "UPDATE accounts SET name = ? WHERE id = ?",
            vec![
                crypto::encrypt(&app.dek, "删除测试基金".as_bytes()).into(),
                app.fund.into(),
            ],
        )
        .await;
        app.set_offset(app.source, 100_000).await;
        app
    }

    async fn post(&self, path: &str, fields: &[(&str, String)]) -> Response {
        self.client
            .post(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .header(header::ACCEPT, "application/json")
            .form(fields)
            .send()
            .await
            .unwrap()
    }

    async fn sql(&self, sql: &str, values: Vec<sea_orm::Value>) {
        self.db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                sql,
                values,
            ))
            .await
            .unwrap();
    }

    async fn scalar(&self, sql: &str) -> i64 {
        self.db
            .query_one(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "value")
            .unwrap()
    }

    async fn text(&self, sql: &str) -> String {
        self.db
            .query_one(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "value")
            .unwrap()
    }

    async fn snapshot(&self) -> Vec<String> {
        let mut result = Vec::new();
        for table in [
            "accounts",
            "account_details",
            "bills",
            "transfers",
            "recurring_investments",
            "investment_executions",
            "investment_sms_events",
            "installment_plans",
            "installment_items",
        ] {
            let columns = self
                .db
                .query_all(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("PRAGMA table_info(\"{table}\")"),
                ))
                .await
                .unwrap();
            let expression = columns
                .into_iter()
                .map(|column| {
                    let name: String = column.try_get("", "name").unwrap();
                    format!("quote(\"{name}\")")
                })
                .collect::<Vec<_>>()
                .join(" || '|' || ");
            let rows = self
                .db
                .query_all(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT {expression} AS value FROM \"{table}\" ORDER BY 1"),
                ))
                .await
                .unwrap();
            result.extend(rows.into_iter().map(|row| {
                let value: String = row.try_get("", "value").unwrap();
                format!("{table}:{value}")
            }));
        }
        result
    }

    async fn account(&self, kind: &str) -> i64 {
        let response = self
            .post(
                "/accounts",
                &[
                    ("name", "删除回归账户".into()),
                    ("account_kind", kind.into()),
                    ("currency", "CNY".into()),
                    ("card_number", "622200001234".into()),
                    ("account_username", String::new()),
                    ("credit_limit", "1000".into()),
                    ("billing_day", "1".into()),
                    ("note", String::new()),
                    ("transfer_fee_rate", "0".into()),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        self.scalar("SELECT MAX(id) AS value FROM accounts WHERE parent_id IS NULL")
            .await
    }

    // Only fixture setup writes offsets directly, including otherwise non-settable fund offsets.
    async fn set_offset(&self, account: i64, cents: i64) {
        self.sql(
            "UPDATE accounts SET balance_offset = ? WHERE id = ?",
            vec![
                crypto::encrypt_cents(&self.dek, cents).into(),
                account.into(),
            ],
        )
        .await;
    }

    async fn balance(&self, id: i64) -> String {
        let response = self
            .client
            .get(format!("{}/accounts/{id}/balance-summary", self.url))
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["balance"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn plan(&self, strategy: &str, next: &str) -> i64 {
        self.sql("INSERT INTO recurring_investments \
            (name, from_account_id, fund_account_id, amount, fee_rate_bps, strategy, \
            index_code, moving_average_days, start_date, next_trade_date, active, note, created_at) \
            VALUES (?, ?, ?, ?, ?, ?, '', 180, ?, ?, 1, ?, ?)", vec![
            crypto::encrypt(&self.dek, "删除回归计划".as_bytes()).into(),
            self.source.into(), self.fund.into(),
            crypto::encrypt_cents(&self.dek, 10_000).into(),
            crypto::encrypt_cents(&self.dek, 100).into(), strategy.into(),
            next.into(), next.into(), crypto::encrypt(&self.dek, b"").into(), CREATED.into(),
        ]).await;
        self.scalar("SELECT MAX(id) AS value FROM recurring_investments")
            .await
    }

    async fn bill(&self, account: i64, kind: &str, cents: i64) -> i64 {
        self.sql("INSERT INTO bills \
            (account_id, kind, amount, category, category_id, is_food, note, happened_at, created_at) \
            VALUES (?, ?, ?, ?, 0, 0, ?, ?, ?)", vec![
            account.into(), kind.into(), crypto::encrypt_cents(&self.dek, cents).into(),
            crypto::encrypt(&self.dek, "投资手续费".as_bytes()).into(),
            crypto::encrypt(&self.dek, b"fixture").into(), WHEN.into(), CREATED.into(),
        ]).await;
        self.scalar("SELECT MAX(id) AS value FROM bills").await
    }

    // Legacy fixture: fee ownership exists only on the execution, as before this change.
    async fn investment(
        &self,
        strategy: &str,
        amount: i64,
        fee: i64,
        fee_account: i64,
    ) -> Execution {
        let plan = self.plan(strategy, "2099-01-05").await;
        self.sql(
            "INSERT INTO transfers \
            (from_account_id, to_account_id, amount, to_amount, note, happened_at, created_at) \
            VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                self.source.into(),
                self.fund.into(),
                crypto::encrypt_cents(&self.dek, amount).into(),
                crypto::encrypt_cents(&self.dek, amount).into(),
                crypto::encrypt(&self.dek, b"fixture").into(),
                WHEN.into(),
                CREATED.into(),
            ],
        )
        .await;
        let transfer = self.scalar("SELECT MAX(id) AS value FROM transfers").await;
        let fee = if fee > 0 {
            Some(self.bill(fee_account, "expense", fee).await)
        } else {
            None
        };
        self.sql(
            "INSERT INTO investment_executions \
            (plan_id, trade_date, transfer_id, fee_bill_id, base_amount, multiplier_bps, strategy, \
            index_code, moving_average_days, index_close, moving_average, created_at) \
            VALUES (?, '2026-10-08', ?, ?, ?, ?, ?, '', 0, '', '', ?)",
            vec![
                plan.into(),
                transfer.into(),
                fee.into(),
                crypto::encrypt_cents(&self.dek, amount).into(),
                crypto::encrypt_cents(&self.dek, 10_000).into(),
                strategy.into(),
                CREATED.into(),
            ],
        )
        .await;
        Execution {
            plan,
            transfer,
            fee,
        }
    }

    async fn linked_audit(&self, execution: &Execution) {
        self.sql("INSERT INTO investment_sms_events \
            (event_hash, occurred_at, kind, status, plan_id, transfer_id, bill_id, sender, parsed, raw, detail, created_at) \
            VALUES ('preserved-audit-hash', ?, 'transfer_out', 'confirmed', ?, ?, ?, '', '', ?, ?, ?)", vec![
            CREATED.into(), execution.plan.into(), execution.transfer.into(), execution.fee.into(),
            crypto::encrypt(&self.dek, b"historical SMS").into(),
            crypto::encrypt(&self.dek, b"historically confirmed").into(), CREATED.into(),
        ]).await;
    }

    async fn transfer(&self, from: i64, to: i64, amount: &str) {
        let response = self
            .post(
                "/transfers",
                &[
                    ("from_account_id", from.to_string()),
                    ("to_account_id", to.to_string()),
                    ("amount", amount.into()),
                    ("fee_amount", "0".into()),
                    ("note", String::new()),
                    ("happened_at", "2026-10-08T07:00".into()),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn sms(&self, payload: &Value) -> Value {
        let response = self
            .client
            .post(format!("{}/api/sms", self.url))
            .bearer_auth(TOKEN)
            .json(payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn fixed_execution_saves_fee_ownership_and_deletion_restores_money_without_rewinding() {
    let app = TestApp::start().await;
    let today = (Utc::now().naive_utc() + Days::hours(8)).date();
    let mut trade_date = today;
    while matches!(trade_date.weekday(), Weekday::Sat | Weekday::Sun) {
        trade_date = trade_date.pred_opt().unwrap();
    }
    // 隔离真实休市安排，并关闭后续日期，避免测试跨北京时间午夜时多执行一天。
    app.sql("DELETE FROM market_closed_days", vec![]).await;
    for offset in 1..=7 {
        app.sql(
            "INSERT INTO market_closed_days (date, name, source, created_at) VALUES (?, 'controlled calendar', 'user', ?)",
            vec![(today + Days::days(offset)).to_string().into(), CREATED.into()],
        ).await;
    }
    let plan = app.plan("fixed", &trade_date.to_string()).await;
    let response = app.post("/investments/run-due", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["executed"], 1);
    let transfer = app.scalar("SELECT id AS value FROM transfers").await;
    let fee = app
        .scalar("SELECT fee_bill_id AS value FROM transfers")
        .await;
    assert_eq!(
        app.scalar("SELECT fee_bill_id AS value FROM investment_executions")
            .await,
        fee
    );
    let snapshot = app.text("SELECT fee_amount AS value FROM transfers").await;
    assert_eq!(crypto::decrypt_cents(&app.dek, &snapshot), 100);
    let rate = app
        .text("SELECT fee_rate_bps AS value FROM transfers")
        .await;
    assert_eq!(crypto::decrypt_cents(&app.dek, &rate), 100);
    let next = app
        .text(&format!(
            "SELECT next_trade_date AS value FROM recurring_investments WHERE id = {plan}"
        ))
        .await;
    assert_eq!(app.balance(app.source).await, "CNY 899.00");
    assert_eq!(app.balance(app.fund).await, "CNY 100.00");
    assert_eq!(
        app.post(
            &format!("/bills/{fee}/refund"),
            &[
                ("mode", "full".into()),
                ("happened_at", "2026-10-16T09:30".into())
            ],
        )
        .await
        .status(),
        StatusCode::OK
    );
    let refund = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    assert_eq!(app.balance(app.source).await, "CNY 900.00");
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM investment_executions")
            .await,
        1
    );
    assert_eq!(app.balance(app.fund).await, "CNY 100.00");
    assert_eq!(
        app.post(&format!("/bills/{refund}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.post(
            &format!("/transfers/{transfer}/delete"),
            &[("redirect_to", "/bills".into())]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM investment_executions")
            .await,
        0
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 0);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
        0
    );
    assert_eq!(app.balance(app.source).await, "CNY 1000.00");
    assert_eq!(app.balance(app.fund).await, "CNY 0.00");
    assert_eq!(
        app.text(&format!(
            "SELECT next_trade_date AS value FROM recurring_investments WHERE id = {plan}"
        ))
        .await,
        next
    );
    let rerun = app.post("/investments/run-due", &[]).await;
    assert_eq!(rerun.status(), StatusCode::OK);
    assert_eq!(rerun.json::<Value>().await.unwrap()["executed"], 0);
}

#[tokio::test]
async fn manual_legacy_fee_uses_actual_account_and_shared_links_are_refunded_only_once() {
    let app = TestApp::start().await;
    let execution = app.investment("manual", 10_000, 100, app.fund).await;
    app.sql(
        "UPDATE transfers SET fee_bill_id = ?, fee_amount = ? WHERE id = ?",
        vec![
            execution.fee.into(),
            crypto::encrypt_cents(&app.dek, 100).into(),
            execution.transfer.into(),
        ],
    )
    .await;
    // A zero-amount manual execution has no principal transfer and must survive.
    app.sql(
        "INSERT INTO investment_executions \
        (plan_id, trade_date, base_amount, multiplier_bps, strategy, index_code, \
        moving_average_days, index_close, moving_average, created_at) \
        VALUES (?, '2026-10-09', ?, ?, 'manual', '', 0, '', '', ?)",
        vec![
            execution.plan.into(),
            crypto::encrypt_cents(&app.dek, 0).into(),
            crypto::encrypt_cents(&app.dek, 10_000).into(),
            CREATED.into(),
        ],
    )
    .await;
    app.linked_audit(&execution).await;
    let detail = app
        .text("SELECT detail AS value FROM investment_sms_events")
        .await;
    assert_eq!(app.balance(app.source).await, "CNY 900.00");
    assert_eq!(app.balance(app.fund).await, "CNY 99.00");
    assert_eq!(
        app.post(&format!("/transfers/{}/delete", execution.transfer), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(app.source).await, "CNY 1000.00");
    assert_eq!(app.balance(app.fund).await, "CNY 0.00");
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 0);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM investment_executions WHERE transfer_id IS NULL")
            .await,
        1
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM investment_sms_events WHERE transfer_id IS NULL AND bill_id IS NULL AND status = 'confirmed' AND event_hash = 'preserved-audit-hash'").await, 1);
    assert_eq!(
        app.text("SELECT detail AS value FROM investment_sms_events")
            .await,
        detail
    );
    assert_eq!(
        app.text("SELECT next_trade_date AS value FROM recurring_investments")
            .await,
        "2099-01-05"
    );
}

#[tokio::test]
async fn insufficient_fund_and_late_transaction_failure_leave_every_record_unchanged() {
    let app = TestApp::start().await;
    let execution = app.investment("fixed", 10_000, 100, app.source).await;
    app.linked_audit(&execution).await;
    let target = app.account("other").await;
    app.transfer(app.fund, target, "1").await;
    let before = app.snapshot().await;
    assert_eq!(
        app.post(&format!("/transfers/{}/delete", execution.transfer), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.snapshot().await, before);
    assert_eq!(app.balance(app.source).await, "CNY 899.00");
    assert_eq!(app.balance(app.fund).await, "CNY 99.00");

    app.transfer(target, app.fund, "1").await;
    app.sql(
        &format!(
            "CREATE TRIGGER fail_principal_delete BEFORE DELETE ON transfers \
        WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced late transaction failure'); END",
            execution.transfer
        ),
        vec![],
    )
    .await;
    let before = app.snapshot().await;
    assert_eq!(
        app.post(&format!("/transfers/{}/delete", execution.transfer), &[])
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    // Execution and fee deletion, as well as both audit updates, happened before this trigger.
    assert_eq!(app.snapshot().await, before);
    assert_eq!(app.balance(app.source).await, "CNY 899.00");
    assert_eq!(app.balance(app.fund).await, "CNY 100.00");
}

#[tokio::test]
async fn credit_fee_account_ceiling_and_refund_overflow_are_atomic() {
    let app = TestApp::start().await;
    let credit = app.account("credit_service").await;
    let execution = app.investment("fixed", 10_000, 100, credit).await;
    app.bill(credit, "income", 100).await;
    assert_eq!(app.balance(credit).await, "CNY 0.00");
    let before = app.snapshot().await;
    assert_eq!(
        app.post(&format!("/transfers/{}/delete", execution.transfer), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.snapshot().await, before);

    let overflow = TestApp::start().await;
    overflow.set_offset(overflow.source, i64::MAX).await;
    let execution = overflow
        .investment("fixed", i64::MAX, 1, overflow.source)
        .await;
    overflow.bill(overflow.source, "income", 1).await;
    let before = overflow.snapshot().await;
    assert_eq!(
        overflow
            .post(&format!("/transfers/{}/delete", execution.transfer), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(overflow.snapshot().await, before);
}

#[tokio::test]
async fn deleting_legacy_plan_first_preserves_fee_ownership_atomically_for_later_deletion() {
    let app = TestApp::start().await;
    let execution = app.investment("fixed", 10_000, 100, app.source).await;
    app.sql(
        "CREATE TRIGGER fail_plan_delete BEFORE DELETE ON recurring_investments \
        BEGIN SELECT RAISE(ABORT, 'forced plan delete failure'); END",
        vec![],
    )
    .await;
    let before = app.snapshot().await;
    assert_eq!(
        app.post(&format!("/investments/{}/delete", execution.plan), &[])
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(app.snapshot().await, before);
    app.sql("DROP TRIGGER fail_plan_delete", vec![]).await;
    assert_eq!(
        app.post(&format!("/investments/{}/delete", execution.plan), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM investment_executions")
            .await,
        0
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM recurring_investments")
            .await,
        0
    );
    assert_eq!(
        app.scalar("SELECT fee_bill_id AS value FROM transfers")
            .await,
        execution.fee.unwrap()
    );
    assert_eq!(app.balance(app.source).await, "CNY 899.00");
    assert_eq!(
        app.post(&format!("/transfers/{}/delete", execution.transfer), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 0);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
        0
    );
    assert_eq!(app.balance(app.source).await, "CNY 1000.00");
    assert_eq!(app.balance(app.fund).await, "CNY 0.00");
}

#[tokio::test]
async fn sms_execution_without_fee_stays_audited_and_replay_or_pending_processing_cannot_recreate_it(
) {
    let app = TestApp::start().await;
    let plan = app.plan("sms", "2026-10-08").await;
    let payload = json!({
        "time": "2026-10-08T07:00:00Z", "sender": "95555",
        "raw": "【招商银行】您尾号1234的账户于10月08日执行「删除测试基金」的定投计划，扣款100.00元，活期余额900.00元。"
    });
    assert_eq!(app.sms(&payload).await["status"], "executed");
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 0);
    let audit = app.text("SELECT event_hash || ':' || status || ':' || detail AS value FROM investment_sms_events").await;
    let next = app
        .text(&format!(
            "SELECT next_trade_date AS value FROM recurring_investments WHERE id = {plan}"
        ))
        .await;
    let transfer = app.scalar("SELECT id AS value FROM transfers").await;
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.sms(&payload).await["status"], "duplicate");
    let response = app.post("/sms/process-pending", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["processed"], 0);
    assert_eq!(app.text("SELECT event_hash || ':' || status || ':' || detail AS value FROM investment_sms_events").await, audit);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
        0
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM investment_executions")
            .await,
        0
    );
    assert_eq!(
        app.text(&format!(
            "SELECT next_trade_date AS value FROM recurring_investments WHERE id = {plan}"
        ))
        .await,
        next
    );
    assert_eq!(app.balance(app.source).await, "CNY 1000.00");
    assert_eq!(app.balance(app.fund).await, "CNY 0.00");
}

#[tokio::test]
async fn installment_principal_still_requires_installment_revocation() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let category = app
        .text("SELECT name AS value FROM categories WHERE kind = 'expense' ORDER BY id LIMIT 1")
        .await;
    let category = crypto::decrypt_string(&app.dek, &category);
    let response = app
        .post(
            "/bills",
            &[
                ("account_id", credit.to_string()),
                ("kind", "expense".into()),
                ("amount", "120".into()),
                ("category", category),
                ("note", String::new()),
                ("happened_at", "2026-10-08T07:00".into()),
                ("use_installment", "true".into()),
                ("installment_term", "3".into()),
                ("installment_method", "equal_principal".into()),
                ("installment_annual_rate", "0".into()),
                ("installment_fee", "0".into()),
                ("installment_first_due_date", "2026-11-08".into()),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let item = app
        .scalar("SELECT MIN(id) AS value FROM installment_items")
        .await;
    assert_eq!(
        app.post(
            &format!("/installments/items/{item}/paid"),
            &[
                ("paid", "true".into()),
                ("repayment_account_id", app.source.to_string()),
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let transfer = app
        .scalar(&format!(
            "SELECT principal_transfer_id AS value FROM installment_items WHERE id = {item}"
        ))
        .await;
    let before = app.snapshot().await;
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.snapshot().await, before);
}
