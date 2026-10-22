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

    async fn installment_bill(&self, account: i64, private: bool) -> i64 {
        let mut fields = Self::bill_fields(account, "expense", "300", private);
        fields.extend([
            ("use_installment", "true".into()),
            ("installment_term", "3".into()),
            ("installment_method", "flat".into()),
            ("installment_annual_rate", "0".into()),
            ("installment_fee", "3".into()),
            ("installment_first_due_date", "2026-11-01".into()),
        ]);
        assert_eq!(
            self.post("/bills", &fields, None).await.status(),
            StatusCode::OK
        );
        self.scalar("SELECT MAX(id) AS value FROM bills").await
    }

    async fn installment_financial_snapshot(&self, bill: i64) -> String {
        let original = self.text(&format!("SELECT account_id || '|' || kind || '|' || amount || '|' || happened_at || '|' || created_at || '|' || is_private || '|' || coalesce(refund_of_bill_id, '') AS value FROM bills WHERE id = {bill}")).await;
        let plan = self.text(&format!("SELECT id || '|' || bill_id || '|' || account_id || '|' || term_months || '|' || method || '|' || annual_rate_bps || '|' || fee || '|' || first_due_date || '|' || created_at AS value FROM installment_plans WHERE bill_id = {bill}")).await;
        let items = self.text(&format!("SELECT group_concat(id || '|' || plan_id || '|' || sequence || '|' || due_date || '|' || principal || '|' || interest || '|' || fee || '|' || total || '|' || coalesce(paid_at, '') || '|' || coalesce(repayment_account_id, '') || '|' || coalesce(principal_transfer_id, '') || '|' || coalesce(charge_bill_id, '') || '|' || created_at, ';') AS value FROM (SELECT * FROM installment_items WHERE plan_id = (SELECT id FROM installment_plans WHERE bill_id = {bill}) ORDER BY sequence)")).await;
        format!("{original}\n{plan}\n{items}")
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
    let private_ledger = app
        .get("/privacy/bills", Some(token))
        .await
        .text()
        .await
        .unwrap();
    assert!(private_ledger.contains("已退款"));
    let stats = app
        .get(
            "/statistics?start_date=2026-10-01&end_date=2026-10-31",
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(stats.contains("+CNY 0.00"));
    assert!(stats.contains("-CNY 0.00"));
    assert!(!stats.contains("1 笔"));
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
async fn partial_refund_requires_exact_review_and_deletion_pauses_again() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let cash = app.account("other").await;
    app.force_balance(cash, "1000").await;
    let original = app.installment_bill(credit, false).await;
    let plan = app
        .scalar(&format!(
            "SELECT id AS value FROM installment_plans WHERE bill_id = {original}"
        ))
        .await;
    let first = app
        .scalar(&format!(
            "SELECT MIN(id) AS value FROM installment_items WHERE plan_id = {plan}"
        ))
        .await;
    let pay = [
        ("paid", "true".into()),
        ("repayment_account_id", cash.to_string()),
    ];
    assert_eq!(
        app.post(&format!("/installments/items/{first}/paid"), &pay, None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let paid_history = app.text(&format!("SELECT principal || '|' || paid_at || '|' || principal_transfer_id || '|' || charge_bill_id AS value FROM installment_items WHERE id = {first}")).await;
    assert_eq!(
        app.refund(original, "partial", "60", None).await.status(),
        StatusCode::OK
    );
    let refund = app
        .scalar("SELECT MAX(id) AS value FROM bills WHERE refund_of_bill_id IS NOT NULL")
        .await;
    let second = app.scalar(&format!("SELECT MIN(id) AS value FROM installment_items WHERE plan_id = {plan} AND paid_at IS NULL")).await;
    let third = app
        .scalar(&format!(
            "SELECT MAX(id) AS value FROM installment_items WHERE plan_id = {plan}"
        ))
        .await;
    let cash_before = app.balance(cash).await;
    let credit_before = app.balance(credit).await;
    assert_eq!(
        app.post(&format!("/installments/items/{second}/paid"), &pay, None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let schedule = serde_json::json!([
        {"id":second,"principal":"0","interest":"0","fee":"0","due_date":"2026-12-01"},
        {"id":third,"principal":"140","interest":"0","fee":"2","due_date":"2027-01-01"}
    ])
    .to_string();
    let review = [("refunded_cents", "6000".into()), ("schedule", schedule)];
    let mut stale = review.clone();
    stale[0].1 = "5999".into();
    assert_eq!(
        app.post(&format!("/installments/{plan}/review"), &stale, None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let duplicate = serde_json::json!([
        {"id":second,"principal":"70","interest":"0","fee":"1","due_date":"2026-12-01"},
        {"id":second,"principal":"70","interest":"0","fee":"1","due_date":"2027-01-01"}
    ])
    .to_string();
    assert_eq!(
        app.post(
            &format!("/installments/{plan}/review"),
            &[("refunded_cents", "6000".into()), ("schedule", duplicate)],
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(&format!("/installments/{plan}/review"), &review, None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.post(&format!("/installments/{plan}/review"), &review, None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(cash).await, cash_before);
    assert_eq!(app.balance(credit).await, credit_before);
    assert_eq!(app.text(&format!("SELECT principal || '|' || paid_at || '|' || principal_transfer_id || '|' || charge_bill_id AS value FROM installment_items WHERE id = {first}")).await, paid_history);
    for item in [second, third] {
        assert_eq!(
            app.post(&format!("/installments/items/{item}/paid"), &pay, None)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(app.balance(credit).await, "CNY 0.00");
    assert_eq!(app.scalar(&format!(
        "SELECT COUNT(*) AS value FROM installment_items WHERE id = {second} AND principal_transfer_id IS NULL AND charge_bill_id IS NULL AND paid_at IS NOT NULL"
    )).await, 1);
    let cash_after = app.balance(cash).await;
    let paid_items = app.text(&format!(
        "SELECT group_concat(id || '|' || principal || '|' || total || '|' || paid_at || '|' || coalesce(principal_transfer_id, '') || '|' || coalesce(charge_bill_id, ''), ';') AS value FROM (SELECT * FROM installment_items WHERE plan_id = {plan} AND paid_at IS NOT NULL ORDER BY id)"
    )).await;
    assert_eq!(
        app.post(&format!("/bills/{refund}/delete"), &[], None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.post(&format!("/installments/items/{second}/paid"), &pay, None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(cash).await, cash_after);
    assert_eq!(app.balance(credit).await, "CNY -60.00");
    for extra in [
        "2027-02-01,59,0,0",
        "2027-02-01,60.001,0,0",
        "invalid,60,0,0",
    ] {
        assert_eq!(
            app.post(
                &format!("/installments/{plan}/review"),
                &[
                    ("refunded_cents", "0".into()),
                    ("additional_schedule", extra.into())
                ],
                None,
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.scalar(&format!(
                "SELECT COUNT(*) AS value FROM installment_items WHERE plan_id = {plan}"
            ))
            .await,
            3
        );
    }
    let additional = [
        ("refunded_cents", "0".into()),
        ("additional_schedule", "2027-02-01,60,0,0".into()),
    ];
    assert_eq!(
        app.post(&format!("/installments/{plan}/review"), &additional, None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.text(&format!(
        "SELECT group_concat(id || '|' || principal || '|' || total || '|' || paid_at || '|' || coalesce(principal_transfer_id, '') || '|' || coalesce(charge_bill_id, ''), ';') AS value FROM (SELECT * FROM installment_items WHERE plan_id = {plan} AND paid_at IS NOT NULL ORDER BY id)"
    )).await, paid_items);
    assert_eq!(app.balance(cash).await, cash_after);
    assert_eq!(app.balance(credit).await, "CNY -60.00");
    let added = app
        .scalar(&format!(
            "SELECT id AS value FROM installment_items WHERE plan_id = {plan} AND paid_at IS NULL"
        ))
        .await;
    assert_eq!(
        app.post(&format!("/installments/items/{added}/paid"), &pay, None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(credit).await, "CNY 0.00");
    assert_eq!(app.balance(cash).await, "CNY 697.00");
}

#[tokio::test]
async fn refunds_stop_unpaid_installments_keep_history_and_protect_linked_fee_reversals() {
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
    let next = app
        .scalar("SELECT MIN(id) AS value FROM installment_items WHERE paid_at IS NULL")
        .await;
    let source_balance = app.balance(cash).await;
    assert_eq!(
        app.post(
            &format!("/installments/items/{next}/paid"),
            &[
                ("paid", "true".into()),
                ("repayment_account_id", cash.to_string())
            ],
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(cash).await, source_balance);
    assert_eq!(app.text(items_sql).await, items);
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

    let credit = app.account("other").await;
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

#[tokio::test]
async fn statistics_net_refunds_at_original_date_and_ignore_internal_transfers() {
    let app = TestApp::start().await;
    let source = app.account("other").await;
    let destination = app.account("other").await;
    app.force_balance(source, "1000").await;
    let original = app.bill(source, "expense", "100", false).await;
    app.execute(&format!(
        "UPDATE bills SET happened_at = '2026-09-10 12:00:00' WHERE id = {original}"
    ))
    .await;
    assert_eq!(
        app.refund(original, "partial", "40", None).await.status(),
        StatusCode::OK
    );
    let stats = app
        .get(
            "/statistics?start_date=2026-09-01&end_date=2026-09-30",
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(stats.contains("-CNY 60.00"));
    assert!(stats.contains("+CNY 0.00"));
    assert!(stats.contains("1 笔"));
    let ledger = app.get("/bills", None).await.text().await.unwrap();
    assert!(ledger.contains("部分退款"));
    assert_eq!(
        app.post(
            "/transfers",
            &[
                ("from_account_id", source.to_string()),
                ("to_account_id", destination.to_string()),
                ("amount", "200".into()),
                ("fee_amount", "0".into()),
                ("note", "实际内部转账".into()),
                ("happened_at", "2026-10-16T10:00".into()),
            ],
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    let refund_period = app
        .get(
            "/statistics?start_date=2026-10-01&end_date=2026-10-31",
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(refund_period.contains("+CNY 0.00"));
    assert!(refund_period.contains("-CNY 0.00"));
    assert_eq!(app.balance(source).await, "CNY 740.00");
    assert_eq!(app.balance(destination).await, "CNY 200.00");
    assert_eq!(
        app.refund(original, "full", "", None).await.status(),
        StatusCode::OK
    );
    let stats = app
        .get(
            "/statistics?start_date=2026-09-01&end_date=2026-09-30",
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(stats.contains("-CNY 0.00"));
    assert!(!stats.contains("1 笔"));
    let ledger = app.get("/bills", None).await.text().await.unwrap();
    assert!(ledger.contains("已退款"));
    let account = app
        .get(&format!("/accounts/{source}"), None)
        .await
        .text()
        .await
        .unwrap();
    assert!(account.contains("已退款"));
    assert_eq!(app.balance(source).await, "CNY 800.00");
    // A category called “refund income” does not make an ordinary income a refund.
    let mut fields = TestApp::bill_fields(source, "income", "25", false);
    for (name, value) in &mut fields {
        if *name == "category" {
            *value = "退款收入".into();
        }
    }
    assert_eq!(
        app.post("/bills", &fields, None).await.status(),
        StatusCode::OK
    );
    let stats = app
        .get(
            "/statistics?start_date=2026-10-01&end_date=2026-10-31",
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(stats.contains("+CNY 25.00"));
}

#[tokio::test]
async fn installment_metadata_edits_preserve_money_and_repayments_even_after_refunds() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let cash = app.account("other").await;
    app.force_balance(cash, "1000").await;
    let original = app.installment_bill(credit, false).await;
    let original_category = app
        .scalar(&format!(
            "SELECT category_id AS value FROM bills WHERE id = {original}"
        ))
        .await;
    let plan = app
        .scalar("SELECT MAX(id) AS value FROM installment_plans")
        .await;
    let item = app
        .scalar("SELECT MIN(id) AS value FROM installment_items")
        .await;
    assert_eq!(
        app.post(
            &format!("/installments/items/{item}/paid"),
            &[
                ("paid", "true".into()),
                ("repayment_account_id", cash.to_string())
            ],
            None,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.post(
            "/categories",
            &[
                ("kind", "expense".into()),
                ("name", "分期餐饮".into()),
                ("is_food", "true".into())
            ],
            None,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let food_category = app.scalar("SELECT MAX(id) AS value FROM categories").await;
    let detail = app
        .get(&format!("/installments/{plan}"), None)
        .await
        .text()
        .await
        .unwrap();
    assert!(detail.contains(&format!("/bills/{original}/edit")));
    let form = app.get(&format!("/bills/{original}/edit"), None).await;
    assert_eq!(form.status(), StatusCode::OK);
    let form = form.text().await.unwrap();
    assert!(form.contains("仅可修改分类与备注"));

    for (refund_mode, category, is_food) in [
        (None, "分期餐饮", 1),
        (Some("partial"), EXPENSE_CATEGORY, 0),
        (Some("full"), "分期餐饮", 1),
    ] {
        if let Some(mode) = refund_mode {
            assert_eq!(
                app.refund(original, mode, "30", None).await.status(),
                StatusCode::OK
            );
        }
        let financial = app.installment_financial_snapshot(original).await;
        let credit_balance = app.balance(credit).await;
        let cash_balance = app.balance(cash).await;
        let bill_count = app.scalar("SELECT COUNT(*) AS value FROM bills").await;
        let transfer_count = app.scalar("SELECT COUNT(*) AS value FROM transfers").await;
        let note = format!("分期元数据修改 {is_food} {}", refund_mode.unwrap_or("none"));
        let response = app
            .post(
                &format!("/bills/{original}/edit"),
                &[("category", category.into()), ("note", note.clone())],
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION].to_str().unwrap(),
            format!("/installments/{plan}")
        );
        assert_eq!(
            app.installment_financial_snapshot(original).await,
            financial
        );
        assert_eq!(app.balance(credit).await, credit_balance);
        assert_eq!(app.balance(cash).await, cash_balance);
        assert_eq!(
            app.scalar("SELECT COUNT(*) AS value FROM bills").await,
            bill_count
        );
        assert_eq!(
            app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
            transfer_count
        );
        assert_eq!(
            app.scalar(&format!(
                "SELECT is_food AS value FROM bills WHERE id = {original}"
            ))
            .await,
            is_food
        );
        assert_eq!(
            app.scalar(&format!(
                "SELECT category_id AS value FROM bills WHERE id = {original}"
            ))
            .await,
            if is_food == 1 {
                food_category
            } else {
                original_category
            }
        );
        let encrypted_note = app
            .text(&format!(
                "SELECT note AS value FROM bills WHERE id = {original}"
            ))
            .await;
        assert_ne!(encrypted_note, note);
        let detail = app
            .get(&format!("/installments/{plan}"), None)
            .await
            .text()
            .await
            .unwrap();
        assert!(detail.contains(category));
        assert!(detail.contains(&note));
    }

    let original_snapshot = app.original_snapshot(original).await;
    let financial = app.installment_financial_snapshot(original).await;
    for (field, value) in [
        ("account_id", cash.to_string()),
        ("kind", "income".into()),
        ("amount", "999.00".into()),
        ("happened_at", "2027-01-01T00:00".into()),
        ("is_private", "false".into()),
        ("use_installment", "false".into()),
        ("installment_term", "12".into()),
    ] {
        let response = app
            .post(
                &format!("/bills/{original}/edit"),
                &[
                    ("category", EXPENSE_CATEGORY.into()),
                    ("note", "不得保存".into()),
                    (field, value),
                ],
                None,
            )
            .await;
        assert!(response.status().is_client_error(), "{field} was accepted");
        assert_eq!(app.original_snapshot(original).await, original_snapshot);
        assert_eq!(
            app.installment_financial_snapshot(original).await,
            financial
        );
    }
    for category in [INCOME_CATEGORY, "不存在的分类"] {
        assert_eq!(
            app.post(
                &format!("/bills/{original}/edit"),
                &[("category", category.into()), ("note", "不得保存".into())],
                None,
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(app.original_snapshot(original).await, original_snapshot);
    }
    let fee = app
        .scalar(&format!(
            "SELECT charge_bill_id AS value FROM installment_items WHERE id = {item}"
        ))
        .await;
    assert_eq!(
        app.get(&format!("/bills/{fee}/edit"), None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(
            &format!("/bills/{fee}/edit"),
            &[
                ("category", EXPENSE_CATEGORY.into()),
                ("note", "不得保存".into())
            ],
            None,
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn private_installment_metadata_edits_require_privacy_access() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let original = app.installment_bill(credit, true).await;
    let financial = app.installment_financial_snapshot(original).await;
    let snapshot = app.original_snapshot(original).await;
    let fields = [
        ("category", EXPENSE_CATEGORY.into()),
        ("note", "仅隐私可见的分期备注".into()),
    ];
    assert_eq!(
        app.get(&format!("/bills/{original}/edit"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.post(&format!("/bills/{original}/edit"), &fields, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(app.original_snapshot(original).await, snapshot);
    let response = app
        .post(
            "/privacy/setup",
            &[
                ("password", "installment-private-password".into()),
                ("confirm", "installment-private-password".into()),
            ],
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: Value = response.json().await.unwrap();
    let token = payload["token"].as_str().unwrap();
    assert_eq!(
        app.get(&format!("/bills/{original}/edit"), Some(token))
            .await
            .status(),
        StatusCode::OK
    );
    let mut tampered = fields.to_vec();
    tampered.push(("is_private", "false".into()));
    assert_eq!(
        app.post(&format!("/bills/{original}/edit"), &tampered, Some(token))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.original_snapshot(original).await, snapshot);
    let response = app
        .post(&format!("/bills/{original}/edit"), &fields, Some(token))
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/privacy/bills");
    assert_eq!(
        app.installment_financial_snapshot(original).await,
        financial
    );
    assert_eq!(app.balance(credit).await, "CNY -300.00");
    let public = app.get("/bills", None).await.text().await.unwrap();
    assert!(!public.contains("仅隐私可见的分期备注"));
    let private = app
        .get("/privacy/bills", Some(token))
        .await
        .text()
        .await
        .unwrap();
    assert!(private.contains("仅隐私可见的分期备注"));
    let stored_note = app
        .text(&format!(
            "SELECT note AS value FROM bills WHERE id = {original}"
        ))
        .await;
    assert_ne!(stored_note, "仅隐私可见的分期备注");
    assert_eq!(
        app.post(&format!("/bills/{original}/edit"), &fields, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_returned_installments_preserves_all_paid_history_and_never_adds_refunds() {
    let app = TestApp::start().await;
    let cash = app.account("other").await;
    app.force_balance(cash, "3000").await;
    // All refunded legacy plans remain deletable: partial/full, reviewed/unreviewed,
    // and even plans whose installments have already been paid in full.
    for (mode, paid_count, reviewed) in [
        ("partial", 1, false),
        ("partial", 1, true),
        ("partial", 3, false),
        ("full", 1, false),
        ("full", 3, false),
    ] {
        let credit = app.account("credit_card").await;
        let original = app.installment_bill(credit, false).await;
        let plan = app
            .scalar(&format!(
                "SELECT id AS value FROM installment_plans WHERE bill_id = {original}"
            ))
            .await;
        for _ in 0..paid_count {
            let item = app.scalar(&format!(
                "SELECT MIN(id) AS value FROM installment_items WHERE plan_id = {plan} AND paid_at IS NULL"
            )).await;
            assert_eq!(
                app.post(
                    &format!("/installments/items/{item}/paid"),
                    &[
                        ("paid", "true".into()),
                        ("repayment_account_id", cash.to_string())
                    ],
                    None,
                )
                .await
                .status(),
                StatusCode::SEE_OTHER
            );
        }
        assert_eq!(
            app.refund(original, mode, "60", None).await.status(),
            StatusCode::OK
        );
        if reviewed {
            let second = app.scalar(&format!(
                "SELECT MIN(id) AS value FROM installment_items WHERE plan_id = {plan} AND paid_at IS NULL"
            )).await;
            let third = app
                .scalar(&format!(
                    "SELECT MAX(id) AS value FROM installment_items WHERE plan_id = {plan}"
                ))
                .await;
            let schedule = serde_json::json!([
                {"id":second,"principal":"70","interest":"0","fee":"1","due_date":"2026-12-01"},
                {"id":third,"principal":"70","interest":"0","fee":"1","due_date":"2027-01-01"}
            ])
            .to_string();
            assert_eq!(
                app.post(
                    &format!("/installments/{plan}/review"),
                    &[("refunded_cents", "6000".into()), ("schedule", schedule)],
                    None,
                )
                .await
                .status(),
                StatusCode::SEE_OTHER
            );
        }
        let financial = app.installment_financial_snapshot(original).await;
        let cash_before = app.balance(cash).await;
        let credit_before = app.balance(credit).await;
        let bills = app.scalar("SELECT COUNT(*) AS value FROM bills").await;
        let transfers = app.scalar("SELECT COUNT(*) AS value FROM transfers").await;
        let baseline = if mode == "full" { "30000" } else { "6000" };
        let delete = [("refunded_cents", baseline.into())];
        assert_eq!(
            app.post(&format!("/installments/{plan}/delete"), &delete, None,)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(app.scalar(&format!(
            "SELECT COUNT(*) AS value FROM installment_plans WHERE id = {plan} AND deleted_at IS NOT NULL"
        )).await, 1);
        assert_eq!(
            app.installment_financial_snapshot(original).await,
            financial
        );
        assert_eq!(app.balance(cash).await, cash_before);
        assert_eq!(app.balance(credit).await, credit_before);
        assert_eq!(
            app.scalar("SELECT COUNT(*) AS value FROM bills").await,
            bills
        );
        assert_eq!(
            app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
            transfers
        );
        assert_eq!(
            app.post(&format!("/installments/{plan}/delete"), &delete, None,)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            app.scalar("SELECT COUNT(*) AS value FROM bills").await,
            bills
        );
        assert_eq!(
            app.post(
                &format!("/installments/{plan}/review"),
                &[
                    ("refunded_cents", baseline.into()),
                    ("additional_schedule", "2027-02-01,60,0,0".into())
                ],
                None,
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        if paid_count < 3 {
            let unpaid = app.scalar(&format!(
                "SELECT MIN(id) AS value FROM installment_items WHERE plan_id = {plan} AND paid_at IS NULL"
            )).await;
            assert_eq!(
                app.post(
                    &format!("/installments/items/{unpaid}/paid"),
                    &[
                        ("paid", "true".into()),
                        ("repayment_account_id", cash.to_string())
                    ],
                    None,
                )
                .await
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let refund = app
            .scalar(&format!(
                "SELECT id AS value FROM bills WHERE refund_of_bill_id = {original}"
            ))
            .await;
        assert_eq!(
            app.post(&format!("/bills/{refund}/delete"), &[], None)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(app.scalar(&format!(
            "SELECT COUNT(*) AS value FROM installment_plans WHERE id = {plan} AND deleted_at IS NOT NULL"
        )).await, 1);
        assert_eq!(
            app.balance(credit).await,
            format!("CNY {}.00", -300 + 100 * paid_count)
        );
        // Replaying the original confirmation after undoing a refund must not
        // recreate it or reopen the deleted installment.
        assert_eq!(
            app.post(&format!("/installments/{plan}/delete"), &delete, None,)
                .await
                .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            app.scalar(&format!(
                "SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original}"
            ))
            .await,
            0
        );
        if mode == "full" && paid_count == 1 {
            let paid = app.scalar(&format!(
                "SELECT id AS value FROM installment_items WHERE plan_id = {plan} AND paid_at IS NOT NULL"
            )).await;
            let fee = app
                .scalar(&format!(
                    "SELECT charge_bill_id AS value FROM installment_items WHERE id = {paid}"
                ))
                .await;
            assert_eq!(
                app.refund(fee, "full", "", None).await.status(),
                StatusCode::OK
            );
            let fee_refund = app
                .scalar(&format!(
                    "SELECT id AS value FROM bills WHERE refund_of_bill_id = {fee}"
                ))
                .await;
            let history = app.installment_financial_snapshot(original).await;
            let balance = app.balance(cash).await;
            assert_eq!(
                app.post(
                    &format!("/installments/items/{paid}/paid"),
                    &[("paid", "false".into())],
                    None,
                )
                .await
                .status(),
                StatusCode::BAD_REQUEST
            );
            assert_eq!(app.installment_financial_snapshot(original).await, history);
            assert_eq!(app.balance(cash).await, balance);
            assert_eq!(
                app.post(&format!("/bills/{fee_refund}/delete"), &[], None)
                    .await
                    .status(),
                StatusCode::SEE_OTHER
            );
            assert_eq!(
                app.post(
                    &format!("/installments/items/{paid}/paid"),
                    &[("paid", "false".into())],
                    None,
                )
                .await
                .status(),
                StatusCode::SEE_OTHER
            );
            assert_eq!(app.balance(credit).await, "CNY -300.00");
            assert_eq!(app.scalar(&format!(
                "SELECT COUNT(*) AS value FROM installment_plans WHERE id = {plan} AND deleted_at IS NOT NULL"
            )).await, 1);
        }
    }
}

#[tokio::test]
async fn unrefunded_installment_deletion_requires_explicit_receipt_and_is_atomic() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let original = app.installment_bill(credit, false).await;
    let plan = app
        .scalar("SELECT MAX(id) AS value FROM installment_plans")
        .await;
    let financial = app.installment_financial_snapshot(original).await;
    let categories = app.scalar("SELECT COUNT(*) AS value FROM categories").await;
    for fields in [
        vec![("refunded_cents", "0".into())],
        vec![
            ("refunded_cents", "0".into()),
            ("treat_as_return", "false".into()),
        ],
        vec![
            ("refunded_cents", "1".into()),
            ("treat_as_return", "true".into()),
        ],
    ] {
        assert_eq!(
            app.post(&format!("/installments/{plan}/delete"), &fields, None)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.installment_financial_snapshot(original).await,
            financial
        );
        assert_eq!(app.balance(credit).await, "CNY -300.00");
        assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 1);
        assert_eq!(
            app.scalar(
                "SELECT COUNT(*) AS value FROM installment_plans WHERE deleted_at IS NOT NULL"
            )
            .await,
            0
        );
    }
    let fields = [
        ("refunded_cents", "0".into()),
        ("treat_as_return", "true".into()),
    ];
    app.execute("CREATE TRIGGER reject_plan_delete BEFORE UPDATE OF deleted_at ON installment_plans BEGIN SELECT RAISE(ABORT, 'forced deletion failure'); END").await;
    assert_eq!(
        app.post(&format!("/installments/{plan}/delete"), &fields, None)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 1);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM categories").await,
        categories
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_plans WHERE deleted_at IS NOT NULL")
            .await,
        0
    );
    assert_eq!(app.balance(credit).await, "CNY -300.00");
    app.execute("DROP TRIGGER reject_plan_delete").await;
    assert_eq!(
        app.post(&format!("/installments/{plan}/delete"), &fields, None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.installment_financial_snapshot(original).await,
        financial
    );
    assert_eq!(app.balance(credit).await, "CNY 0.00");
    assert_eq!(app.scalar(&format!(
        "SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original} AND account_id = {credit} AND kind = 'income' AND is_private = 0"
    )).await, 1);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_plans WHERE deleted_at IS NOT NULL")
            .await,
        1
    );
    assert_eq!(
        app.post(&format!("/installments/{plan}/delete"), &fields, None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 2);
    assert_eq!(app.balance(credit).await, "CNY 0.00");
}

#[tokio::test]
async fn changed_refund_baseline_blocks_deletion_and_service_positive_balance_rolls_back() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let original = app.installment_bill(credit, false).await;
    let plan = app
        .scalar("SELECT MAX(id) AS value FROM installment_plans")
        .await;
    assert_eq!(
        app.refund(original, "partial", "60", None).await.status(),
        StatusCode::OK
    );
    let financial = app.installment_financial_snapshot(original).await;
    for stale in ["0", "5999", "30000"] {
        assert_eq!(
            app.post(
                &format!("/installments/{plan}/delete"),
                &[
                    ("refunded_cents", stale.into()),
                    ("treat_as_return", "true".into())
                ],
                None,
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.installment_financial_snapshot(original).await,
            financial
        );
        assert_eq!(app.balance(credit).await, "CNY -240.00");
        assert_eq!(
            app.scalar(
                "SELECT COUNT(*) AS value FROM installment_plans WHERE deleted_at IS NOT NULL"
            )
            .await,
            0
        );
    }
    let service = app.account("credit_service").await;
    let original = app.installment_bill(service, false).await;
    let plan = app
        .scalar("SELECT MAX(id) AS value FROM installment_plans")
        .await;
    app.force_balance(service, "0").await;
    let bills = app.scalar("SELECT COUNT(*) AS value FROM bills").await;
    let categories = app.scalar("SELECT COUNT(*) AS value FROM categories").await;
    assert_eq!(
        app.post(
            &format!("/installments/{plan}/delete"),
            &[
                ("refunded_cents", "0".into()),
                ("treat_as_return", "true".into())
            ],
            None,
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(service).await, "CNY 0.00");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bills").await,
        bills
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM categories").await,
        categories
    );
    assert_eq!(
        app.scalar(&format!(
            "SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original}"
        ))
        .await,
        0
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_plans WHERE deleted_at IS NOT NULL")
            .await,
        0
    );
}

#[tokio::test]
async fn private_deletion_cannot_bypass_refund_access_but_existing_refunds_allow_cleanup() {
    let app = TestApp::start().await;
    let credit = app.account("credit_card").await;
    let original = app.installment_bill(credit, true).await;
    let plan = app
        .scalar("SELECT MAX(id) AS value FROM installment_plans")
        .await;
    let financial = app.installment_financial_snapshot(original).await;
    let fields = [
        ("refunded_cents", "0".into()),
        ("treat_as_return", "true".into()),
    ];
    for token in [None, Some("invalid-privacy-token")] {
        assert_eq!(
            app.post(&format!("/installments/{plan}/delete"), &fields, token)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.installment_financial_snapshot(original).await,
            financial
        );
        assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 1);
        assert_eq!(app.balance(credit).await, "CNY -300.00");
    }
    let setup = app
        .post(
            "/privacy/setup",
            &[
                ("password", "delete-private-password".into()),
                ("confirm", "delete-private-password".into()),
            ],
            None,
        )
        .await;
    assert_eq!(setup.status(), StatusCode::OK);
    let payload: Value = setup.json().await.unwrap();
    let token = payload["token"].as_str().unwrap();
    assert_eq!(
        app.post(
            &format!("/installments/{plan}/delete"),
            &fields,
            Some(token)
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.scalar(&format!(
        "SELECT COUNT(*) AS value FROM bills WHERE refund_of_bill_id = {original} AND is_private = 1 AND account_id = {credit}"
    )).await, 1);
    assert_eq!(app.balance(credit).await, "CNY 0.00");
    let original = app.installment_bill(credit, true).await;
    let plan = app
        .scalar("SELECT MAX(id) AS value FROM installment_plans")
        .await;
    assert_eq!(
        app.refund(original, "partial", "60", Some(token))
            .await
            .status(),
        StatusCode::OK
    );
    let bills = app.scalar("SELECT COUNT(*) AS value FROM bills").await;
    assert_eq!(
        app.post(
            &format!("/installments/{plan}/delete"),
            &[("refunded_cents", "6000".into())],
            None,
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bills").await,
        bills
    );
    assert_eq!(app.balance(credit).await, "CNY -240.00");
}
