use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::Value;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
const EXPENSE_CATEGORY: &str = "退款回归支出";
const INCOME_CATEGORY: &str = "退款回归收入";

struct TestApp {
    child: Child,
    directory: PathBuf,
    db: DatabaseConnection,
    client: Client,
    url: String,
    cookie: String,
}

impl TestApp {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "haruka-refunds-test-{}-{nonce}-{}",
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
            .env_remove("PASSKEY_ORIGIN")
            .env_remove("PASSKEY_RP_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
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
            .form(&[
                ("password", "refund-regression-only"),
                ("confirm", "refund-regression-only"),
            ])
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
        let app = Self {
            child,
            directory,
            db: Database::connect(&database_url).await.unwrap(),
            client,
            url,
            cookie,
        };
        for (kind, name) in [("expense", EXPENSE_CATEGORY), ("income", INCOME_CATEGORY)] {
            assert_eq!(
                app.post(
                    "/categories",
                    &[("kind", kind.into()), ("name", name.into())],
                    None
                )
                .await
                .status(),
                StatusCode::SEE_OTHER
            );
        }
        app
    }

    async fn post(&self, path: &str, fields: &[(&str, String)], token: Option<&str>) -> Response {
        let mut request = self
            .client
            .post(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .header(header::ACCEPT, "application/json")
            .form(fields);
        if let Some(token) = token {
            request = request.header("X-Haruka-Privacy", token);
        }
        request.send().await.unwrap()
    }

    async fn get(&self, path: &str, token: Option<&str>) -> Response {
        let mut request = self
            .client
            .get(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .header(header::ACCEPT, "application/json");
        if let Some(token) = token {
            request = request.header("X-Haruka-Privacy", token);
        }
        request.send().await.unwrap()
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

    async fn execute(&self, sql: &str) {
        self.db
            .execute(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap();
    }

    async fn account(&self, kind: &str) -> i64 {
        assert_eq!(
            self.post(
                "/accounts",
                &[
                    ("name", "退款测试账户".into()),
                    ("account_kind", kind.into()),
                    ("currency", "CNY".into()),
                    ("card_number", String::new()),
                    ("account_username", String::new()),
                    ("credit_limit", "1000".into()),
                    ("billing_day", "1".into()),
                    ("note", String::new()),
                ],
                None
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        self.scalar("SELECT MAX(id) AS value FROM accounts").await
    }

    async fn force_balance(&self, id: i64, balance: &str) {
        assert_eq!(
            self.post(
                &format!("/accounts/{id}/balance"),
                &[("balance", balance.into())],
                None
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }

    async fn balance(&self, id: i64) -> String {
        let response = self
            .get(&format!("/accounts/{id}/balance-summary"), None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["balance"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn bill_fields(
        account: i64,
        kind: &str,
        amount: &str,
        private: bool,
    ) -> Vec<(&'static str, String)> {
        vec![
            ("account_id", account.to_string()),
            ("kind", kind.into()),
            ("amount", amount.into()),
            (
                "category",
                if kind == "expense" {
                    EXPENSE_CATEGORY
                } else {
                    INCOME_CATEGORY
                }
                .into(),
            ),
            ("note", "原始备注不可改变".into()),
            ("happened_at", "2026-10-10T08:00".into()),
            ("is_private", private.to_string()),
        ]
    }

    async fn bill(&self, account: i64, kind: &str, amount: &str, private: bool) -> i64 {
        assert_eq!(
            self.post(
                "/bills",
                &Self::bill_fields(account, kind, amount, private),
                None
            )
            .await
            .status(),
            StatusCode::OK
        );
        self.scalar("SELECT MAX(id) AS value FROM bills").await
    }

    async fn refund(&self, id: i64, mode: &str, amount: &str, token: Option<&str>) -> Response {
        self.post(
            &format!("/bills/{id}/refund"),
            &[
                ("mode", mode.into()),
                ("amount", amount.into()),
                ("happened_at", "2026-10-16T09:30".into()),
                ("note", "退款备注".into()),
            ],
            token,
        )
        .await
    }

    async fn original_snapshot(&self, id: i64) -> String {
        self.text(&format!("SELECT account_id || '|' || kind || '|' || amount || '|' || category || '|' || category_id || '|' || note || '|' || happened_at || '|' || created_at || '|' || is_private AS value FROM bills WHERE id = {id}")).await
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
async fn partial_then_full_keeps_expense_and_supports_only_balance_safe_refund_deletion() {
    let app = TestApp::start().await;
    for kind in ["expense", "income"] {
        assert_eq!(
            app.post(
                "/categories",
                &[("kind", kind.into()), ("name", "退款收入".into())],
                None
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let refund_category = app.scalar("SELECT MAX(id) AS value FROM categories").await;
    let account = app.account("other").await;
    app.force_balance(account, "1000").await;
    let original = app.bill(account, "expense", "100", false).await;
    let snapshot = app.original_snapshot(original).await;
    assert_eq!(
        app.get(&format!("/bills/{original}/refund"), None)
            .await
            .status(),
        StatusCode::OK
    );
    for (amount, expected) in [("20", "CNY 920.00"), ("30", "CNY 950.00")] {
        assert_eq!(
            app.refund(original, "partial", amount, None).await.status(),
            StatusCode::OK
        );
        assert_eq!(app.balance(account).await, expected);
    }
    // Full mode ignores the stale client amount and uses the latest remaining 50.
    assert_eq!(
        app.refund(original, "full", "100", None).await.status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(account).await, "CNY 1000.00");
    assert_eq!(app.original_snapshot(original).await, snapshot);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bills WHERE kind = 'expense'")
            .await,
        1
    );
    assert_eq!(app.scalar(&format!("SELECT COUNT(*) AS value FROM bills WHERE kind = 'income' AND refund_of_bill_id = {original} AND account_id = {account} AND is_food = 0 AND is_private = 0")).await, 3);
    assert_eq!(app.scalar(&format!("SELECT COUNT(DISTINCT category_id) AS value FROM bills WHERE refund_of_bill_id = {original}")).await, 1);
    assert_eq!(app.scalar(&format!("SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original} AND category_id = {refund_category} AND strftime('%Y-%m-%dT%H:%M', happened_at) = '2026-10-16T09:30'")).await, 3);
    assert_eq!(app.scalar(&format!("SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original} AND (amount = '2000' OR category = '退款收入' OR note = '退款备注')")).await, 0);
    assert_eq!(
        app.refund(original, "full", "", None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.get(&format!("/bills/{original}/edit"), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(
            &format!("/bills/{original}/edit"),
            &TestApp::bill_fields(account, "expense", "90", false),
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(&format!("/bills/{original}/delete"), &[], None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let refund = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    assert_eq!(
        app.get(&format!("/bills/{refund}/edit"), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(
            &format!("/bills/{refund}/edit"),
            &TestApp::bill_fields(account, "income", "1", false),
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    app.force_balance(account, "0").await;
    assert_eq!(
        app.post(&format!("/bills/{refund}/delete"), &[], None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.scalar(&format!(
            "SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original}"
        ))
        .await,
        3
    );
    app.force_balance(account, "50").await;
    assert_eq!(
        app.post(&format!("/bills/{refund}/delete"), &[], None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(account).await, "CNY 0.00");
    assert_eq!(
        app.refund(original, "full", "", None).await.status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(account).await, "CNY 50.00");
    assert_eq!(app.original_snapshot(original).await, snapshot);
}

#[tokio::test]
async fn invalid_amounts_sources_and_concurrent_full_refunds_cannot_over_refund() {
    let app = TestApp::start().await;
    let account = app.account("other").await;
    app.force_balance(account, "100").await;
    let original = app.bill(account, "expense", "100", false).await;
    let category_count = app.scalar("SELECT COUNT(*) AS value FROM categories").await;
    for amount in [
        "0",
        "-1",
        "100.01",
        "0.001",
        "92233720368547758.08",
        "999999999999999999999999999999999",
        "not-money",
    ] {
        assert_eq!(
            app.refund(original, "partial", amount, None).await.status(),
            StatusCode::BAD_REQUEST,
            "amount: {amount}"
        );
    }
    assert_eq!(
        app.refund(original, "unknown", "1", None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(
            &format!("/bills/{original}/refund"),
            &[
                ("mode", "partial".into()),
                ("amount", "1".into()),
                ("happened_at", "invalid-time".into()),
            ],
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 1);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM categories").await,
        category_count
    );
    assert_eq!(app.balance(account).await, "CNY 0.00");
    let (first, second) = tokio::join!(
        app.refund(original, "full", "", None),
        app.refund(original, "full", "", None)
    );
    let mut statuses = [first.status().as_u16(), second.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 400]);
    assert_eq!(app.balance(account).await, "CNY 100.00");
    assert_eq!(
        app.scalar(&format!(
            "SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original}"
        ))
        .await,
        1
    );
    let income = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    assert_eq!(
        app.get(&format!("/bills/{income}/refund"), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.refund(income, "full", "", None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.refund(original, "partial", "0.01", None).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn credit_service_ceiling_overflow_and_failed_insert_leave_no_income_or_category() {
    let app = TestApp::start().await;
    let credit = app.account("credit_service").await;
    let original = app.bill(credit, "expense", "100", false).await;
    app.force_balance(credit, "0").await;
    let category_count = app.scalar("SELECT COUNT(*) AS value FROM categories").await;
    assert_eq!(
        app.refund(original, "full", "", None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(credit).await, "CNY 0.00");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM categories").await,
        category_count
    );

    let cash = app.account("other").await;
    app.force_balance(cash, "100").await;
    let cash_bill = app.bill(cash, "expense", "100", false).await;
    app.bill(cash, "income", "100", false).await;
    app.force_balance(cash, "92233720368547758.00").await;
    assert_eq!(
        app.refund(cash_bill, "partial", "1", None).await.status(),
        StatusCode::BAD_REQUEST
    );
    app.force_balance(cash, "0").await;
    // Exercise rollback after category insertion but before the bill is committed.
    app.execute("CREATE TRIGGER reject_refund_insert BEFORE INSERT ON bills WHEN NEW.refund_of_bill_id IS NOT NULL BEGIN SELECT RAISE(ABORT, 'refund rollback regression'); END").await;
    assert_eq!(
        app.refund(cash_bill, "full", "", None).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id IS NOT NULL")
            .await,
        0
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM categories").await,
        category_count
    );
    assert_eq!(app.balance(cash).await, "CNY 0.00");
    app.execute("DROP TRIGGER reject_refund_insert").await;
    assert_eq!(
        app.refund(cash_bill, "full", "", None).await.status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(cash).await, "CNY 100.00");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM categories").await,
        category_count + 1
    );
}

#[tokio::test]
async fn private_refunds_require_authorization_inherit_privacy_and_can_be_revoked() {
    let app = TestApp::start().await;
    let account = app.account("other").await;
    app.force_balance(account, "100").await;
    let original = app.bill(account, "expense", "100", true).await;
    assert_eq!(
        app.get(&format!("/bills/{original}/refund"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.refund(original, "full", "", None).await.status(),
        StatusCode::NOT_FOUND
    );
    let setup = app
        .post(
            "/privacy/setup",
            &[
                ("password", "refund-private-password".into()),
                ("confirm", "refund-private-password".into()),
            ],
            None,
        )
        .await;
    assert_eq!(setup.status(), StatusCode::OK);
    let payload: Value = setup.json().await.unwrap();
    let token = payload["token"].as_str().unwrap();
    assert_eq!(
        app.get(&format!("/bills/{original}/refund"), Some(token))
            .await
            .status(),
        StatusCode::OK
    );
    let response = app.refund(original, "full", "", Some(token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["redirect"],
        "/privacy/bills"
    );
    let refund = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    assert_eq!(
        app.scalar(&format!(
            "SELECT is_private AS value FROM bills WHERE id = {refund}"
        ))
        .await,
        1
    );
    assert_eq!(
        app.get(&format!("/bills/{refund}/edit"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.post(&format!("/bills/{refund}/delete"), &[], None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.post(&format!("/bills/{original}/delete"), &[], Some(token))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let public_ledger = app.get("/bills", None).await.text().await.unwrap();
    assert!(!public_ledger.contains("退款备注"));
    assert_eq!(
        app.post(&format!("/bills/{refund}/delete"), &[], Some(token))
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(account).await, "CNY 0.00");
    assert_eq!(
        app.post(&format!("/bills/{original}/delete"), &[], Some(token))
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn refunds_leave_installments_unchanged_and_protect_linked_fee_reversals() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let cash = app.account("other").await;
    app.force_balance(cash, "1000").await;
    let mut fields = TestApp::bill_fields(credit, "expense", "300", false);
    fields.extend([
        ("use_installment", "true".into()),
        ("installment_term", "3".into()),
        ("installment_method", "flat".into()),
        ("installment_annual_rate", "0".into()),
        ("installment_fee", "3".into()),
        ("installment_first_due_date", "2026-11-01".into()),
    ]);
    assert_eq!(
        app.post("/bills", &fields, None).await.status(),
        StatusCode::OK
    );
    let original = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    let item = app
        .scalar("SELECT MIN(id) AS value FROM installment_items")
        .await;
    assert_eq!(
        app.post(
            &format!("/installments/items/{item}/paid"),
            &[
                ("paid", "true".into()),
                ("repayment_account_id", cash.to_string()),
            ],
            None
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let fee = app
        .scalar(&format!(
            "SELECT charge_bill_id AS value FROM installment_items WHERE id = {item}"
        ))
        .await;
    let plan_sql = "SELECT bill_id || '|' || account_id || '|' || term_months || '|' || method || '|' || annual_rate_bps || '|' || fee || '|' || first_due_date AS value FROM installment_plans";
    let items_sql = "SELECT group_concat(id || '|' || due_date || '|' || principal || '|' || interest || '|' || fee || '|' || total || '|' || coalesce(paid_at, '') || '|' || coalesce(principal_transfer_id, '') || '|' || coalesce(charge_bill_id, ''), ';') AS value FROM (SELECT * FROM installment_items ORDER BY sequence)";
    let plan = app.text(plan_sql).await;
    let items = app.text(items_sql).await;
    let snapshot = app.original_snapshot(original).await;
    assert_eq!(
        app.refund(original, "full", "", None).await.status(),
        StatusCode::OK
    );
    assert_eq!(app.text(plan_sql).await, plan);
    assert_eq!(app.text(items_sql).await, items);
    assert_eq!(app.original_snapshot(original).await, snapshot);
    assert_eq!(
        app.refund(fee, "full", "", None).await.status(),
        StatusCode::OK
    );
    let fee_refund = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    assert_eq!(app.balance(cash).await, "CNY 900.00");
    assert_eq!(
        app.post(
            &format!("/installments/items/{item}/paid"),
            &[("paid", "false".into())],
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.text(items_sql).await, items);
    assert_eq!(
        app.post(&format!("/bills/{fee_refund}/delete"), &[], None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.post(
            &format!("/installments/items/{item}/paid"),
            &[("paid", "false".into())],
            None
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(cash).await, "CNY 1000.00");
    assert_eq!(app.balance(credit).await, "CNY 0.00");

    assert_eq!(
        app.post(
            "/transfers",
            &[
                ("from_account_id", cash.to_string()),
                ("to_account_id", credit.to_string()),
                ("amount", "10".into()),
                ("fee_amount", "1".into()),
                ("note", String::new()),
                ("happened_at", "2026-10-16T10:00".into()),
            ],
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    let transfer_fee = app.scalar("SELECT fee_bill_id AS value FROM transfers WHERE fee_bill_id IS NOT NULL ORDER BY id DESC LIMIT 1").await;
    assert_eq!(
        app.refund(transfer_fee, "full", "", None).await.status(),
        StatusCode::OK
    );
    let transfer_refund = app.scalar("SELECT MAX(id) AS value FROM bills").await;
    let transfer = app
        .scalar(&format!(
            "SELECT id AS value FROM transfers WHERE fee_bill_id = {transfer_fee}"
        ))
        .await;
    assert_eq!(app.balance(cash).await, "CNY 990.00");
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[], None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(cash).await, "CNY 990.00");
    assert_eq!(app.balance(credit).await, "CNY 10.00");
    assert_eq!(
        app.post(&format!("/bills/{transfer_refund}/delete"), &[], None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[], None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(cash).await, "CNY 1000.00");
    assert_eq!(app.balance(credit).await, "CNY 0.00");
}
