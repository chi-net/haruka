use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::Value;
use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
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
            "haruka-repayments-{}-{nonce}-{}",
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
                ("password", "repayment-regression-only"),
                ("confirm", "repayment-regression-only"),
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
        assert_eq!(
            app.post(
                "/categories",
                &[("kind", "expense".into()), ("name", "还款回归消费".into())]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
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
    async fn get(&self, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .unwrap()
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
    async fn account(&self, kind: &str, currency: &str) -> i64 {
        assert_eq!(
            self.post(
                "/accounts",
                &[
                    ("name", "还款回归账户".into()),
                    ("account_kind", kind.into()),
                    ("currency", currency.into()),
                    ("card_number", String::new()),
                    ("account_username", String::new()),
                    ("credit_limit", "10000".into()),
                    ("billing_day", "1".into()),
                    ("repayment_day", "20".into()),
                    ("note", String::new()),
                    ("transfer_fee_rate", "2".into()),
                ]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        self.scalar("SELECT MAX(id) AS value FROM accounts").await
    }
    async fn force_balance(&self, account: i64, balance: &str) {
        assert_eq!(
            self.post(
                &format!("/accounts/{account}/balance"),
                &[("balance", balance.into())]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    async fn balance(&self, account: i64) -> String {
        let response = self
            .get(&format!("/accounts/{account}/balance-summary"))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["balance"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn bill(&self, account: i64, amount: &str) -> i64 {
        assert_eq!(
            self.post(
                "/bills",
                &[
                    ("account_id", account.to_string()),
                    ("kind", "expense".into()),
                    ("amount", amount.into()),
                    ("category", "还款回归消费".into()),
                    ("note", "原消费不能修改".into()),
                    (
                        "happened_at",
                        (chrono::Utc::now() - chrono::Duration::days(1))
                            .format("%Y-%m-%dT%H:%M")
                            .to_string()
                    ),
                ]
            )
            .await
            .status(),
            StatusCode::OK
        );
        self.scalar("SELECT MAX(id) AS value FROM bills").await
    }
    fn fields(from: i64, to: i64, mode: &str, amount: &str) -> Vec<(&'static str, String)> {
        vec![
            ("mode", mode.into()),
            ("from_account_id", from.to_string()),
            ("to_account_id", to.to_string()),
            ("amount", amount.into()),
            ("note", "真实还款".into()),
            (
                "happened_at",
                chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string(),
            ),
        ]
    }
    fn minimum(from: i64, to: i64, amount: &str, ordinary: &str) -> Vec<(&'static str, String)> {
        let mut fields = Self::fields(from, to, "minimum", amount);
        fields.extend([
            ("ordinary_cents", ordinary.into()),
            ("installment_term", "3".into()),
            ("installment_method", "equal_principal".into()),
            ("installment_annual_rate", "0".into()),
            ("installment_fee", "3".into()),
            (
                "installment_first_due_date",
                (chrono::Utc::now().date_naive() + chrono::Months::new(1)).to_string(),
            ),
        ]);
        fields
    }
    async fn options(&self, account: i64) -> Value {
        let response = self
            .get(&format!("/repayments/options?account_id={account}"))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("no-store"));
        response.json().await.unwrap()
    }
    async fn counts(&self) -> (i64, i64, i64) {
        (
            self.scalar("SELECT COUNT(*) AS value FROM transfers").await,
            self.scalar("SELECT COUNT(*) AS value FROM bills").await,
            self.scalar("SELECT COUNT(*) AS value FROM installment_plans")
                .await,
        )
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
async fn custom_minimum_and_concurrent_schedule_payments_keep_one_real_debt() {
    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let target = app.account("credit_card", "CNY").await;
    app.force_balance(source, "1000").await;
    let bill = app.bill(target, "300").await;
    let original = app.text(&format!("SELECT amount || '|' || note || '|' || happened_at || '|' || created_at AS value FROM bills WHERE id = {bill}")).await;
    assert_eq!(app.options(target).await["ordinary_cents"], "30000");
    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::fields(source, target, "custom", "30")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(app.counts().await, (1, 1, 0));
    assert_eq!(app.balance(source).await, "CNY 970.00");
    assert_eq!(app.balance(target).await, "CNY -270.00");
    let minimum = TestApp::minimum(source, target, "30", "27000");
    assert_eq!(
        app.post("/repayments", &minimum).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.post("/repayments", &minimum).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (2, 1, 1));
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM installment_plans WHERE bill_id IS NULL AND repayment_transfer_id IS NOT NULL AND converted_at IS NOT NULL").await, 1);
    assert_eq!(app.balance(source).await, "CNY 940.00");
    assert_eq!(app.balance(target).await, "CNY -240.00");
    let options = app.options(target).await;
    assert_eq!(options["ordinary_cents"], "0");
    assert_eq!(options["plans"][0]["remaining_cents"], "24300");
    let plan = options["plans"][0]["id"].as_i64().unwrap();
    let item = options["plans"][0]["next_item_id"].as_i64().unwrap();
    let minimum_transfer = app
        .scalar("SELECT repayment_transfer_id AS value FROM installment_plans")
        .await;
    assert_eq!(
        app.post(&format!("/transfers/{minimum_transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(&format!("/accounts/{source}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (2, 1, 1));
    assert_eq!(app.balance(source).await, "CNY 940.00");
    assert_eq!(app.balance(target).await, "CNY -240.00");
    let mut next = TestApp::fields(source, target, "installment_next", "");
    next.extend([("plan_id", plan.to_string()), ("item_id", item.to_string())]);
    let (first, repeated) = tokio::join!(
        app.post("/repayments", &next),
        app.post("/repayments", &next)
    );
    let statuses = [first.status(), repeated.status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|&&status| status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|&&status| status == StatusCode::BAD_REQUEST)
            .count(),
        1
    );
    assert_eq!(app.balance(source).await, "CNY 859.00");
    assert_eq!(app.balance(target).await, "CNY -160.00");
    assert_eq!(
        app.post(&format!("/installments/{plan}/cancel-conversion"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let options = app.options(target).await;
    assert_eq!(options["plans"][0]["remaining_cents"], "16200");
    let mut all = TestApp::fields(source, target, "installment_all", "");
    all.extend([
        ("plan_id", plan.to_string()),
        ("remaining_cents", "24300".into()),
    ]);
    assert_eq!(
        app.post("/repayments", &all).await.status(),
        StatusCode::BAD_REQUEST
    );
    all.iter_mut()
        .find(|(name, _)| *name == "remaining_cents")
        .unwrap()
        .1 = "16200".into();
    let (first, repeated) =
        tokio::join!(app.post("/repayments", &all), app.post("/repayments", &all));
    let statuses = [first.status(), repeated.status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|&&status| status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|&&status| status == StatusCode::BAD_REQUEST)
            .count(),
        1
    );
    assert_eq!(app.balance(source).await, "CNY 697.00");
    assert_eq!(app.balance(target).await, "CNY 0.00");
    assert_eq!(app.counts().await, (5, 4, 1));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        3
    );
    assert_eq!(app.options(target).await["ordinary_cents"], "0");
    assert_eq!(app.text(&format!("SELECT amount || '|' || note || '|' || happened_at || '|' || created_at AS value FROM bills WHERE id = {bill}")).await, original);
}

#[tokio::test]
async fn repayment_validation_and_insufficient_aggregate_payment_are_atomic() {
    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let foreign = app.account("other", "USD").await;
    let target = app.account("credit_service", "CNY").await;
    let other_credit = app.account("credit_card", "CNY").await;
    app.force_balance(source, "20").await;
    app.force_balance(foreign, "1000").await;
    app.bill(target, "300").await;
    for (from, to, amount) in [
        (source, target, "30"),
        (foreign, target, "10"),
        (other_credit, target, "10"),
        (source, source, "10"),
        (source, target, "301"),
    ] {
        assert_eq!(
            app.post("/repayments", &TestApp::fields(from, to, "custom", amount))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut stale = TestApp::minimum(source, target, "10", "29900");
    assert_eq!(
        app.post("/repayments", &stale).await.status(),
        StatusCode::BAD_REQUEST
    );
    stale
        .iter_mut()
        .find(|(name, _)| *name == "ordinary_cents")
        .unwrap()
        .1 = "30000".into();
    stale
        .iter_mut()
        .find(|(name, _)| *name == "happened_at")
        .unwrap()
        .1 = (chrono::Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    assert_eq!(
        app.post("/repayments", &stale).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (0, 1, 0));
    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::minimum(source, target, "10", "30000")
        )
        .await
        .status(),
        StatusCode::OK
    );
    let options = app.options(target).await;
    let plan = options["plans"][0]["id"].as_i64().unwrap();
    let mut all = TestApp::fields(source, target, "installment_all", "");
    all.extend([
        ("plan_id", plan.to_string()),
        (
            "remaining_cents",
            options["plans"][0]["remaining_cents"]
                .as_str()
                .unwrap()
                .into(),
        ),
    ]);
    assert_eq!(
        app.post("/repayments", &all).await.status(),
        StatusCode::BAD_REQUEST
    );
    let mut wrong_plan = TestApp::fields(source, other_credit, "installment_next", "");
    wrong_plan.extend([
        ("plan_id", plan.to_string()),
        (
            "item_id",
            options["plans"][0]["next_item_id"]
                .as_i64()
                .unwrap()
                .to_string(),
        ),
    ]);
    assert_eq!(
        app.post("/repayments", &wrong_plan).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (1, 1, 1));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        0
    );
    assert_eq!(app.balance(source).await, "CNY 10.00");
    assert_eq!(app.balance(target).await, "CNY -290.00");

    // Future income affects the real balance but not today's statement estimate.
    // Never turn an estimate larger than actual debt into a second, phantom loan.
    app.bill(other_credit, "300").await;
    assert_eq!(
        app.post(
            "/bills",
            &[
                ("account_id", other_credit.to_string()),
                ("kind", "income".into()),
                ("amount", "250".into()),
                ("category", "工资".into()),
                ("note", String::new()),
                (
                    "happened_at",
                    (chrono::Utc::now() + chrono::Duration::days(2))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string()
                ),
            ]
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(app.options(other_credit).await["ordinary_cents"], "30000");
    assert_eq!(app.balance(other_credit).await, "CNY -50.00");
    let before = app.counts().await;
    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::minimum(source, other_credit, "1", "30000")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, before);
    assert_eq!(app.balance(other_credit).await, "CNY -50.00");
    assert_eq!(app.balance(source).await, "CNY 10.00");
}

#[tokio::test]
async fn native_forms_and_unused_conversion_cancel_preserve_the_real_minimum_payment() {
    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let target = app.account("credit_card", "CNY").await;
    app.force_balance(source, "100").await;
    app.bill(target, "300").await;
    let response = app
        .client
        .post(format!("{}/repayments", app.url))
        .header(header::COOKIE, &app.cookie)
        .form(&TestApp::minimum(source, target, "30", "30000"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/dashboard");
    let options = app.options(target).await;
    let plan = options["plans"][0]["id"].as_i64().unwrap();
    let transfer = app
        .scalar("SELECT repayment_transfer_id AS value FROM installment_plans")
        .await;
    assert_eq!(
        app.post(&format!("/installments/{plan}/cancel-conversion"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.counts().await, (1, 1, 0));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items")
            .await,
        0
    );
    assert_eq!(app.balance(source).await, "CNY 70.00");
    assert_eq!(app.balance(target).await, "CNY -270.00");
    assert_eq!(app.options(target).await["ordinary_cents"], "27000");
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(source).await, "CNY 100.00");
    assert_eq!(app.balance(target).await, "CNY -300.00");
}

#[tokio::test]
async fn card_overpayment_is_real_cash_without_income_and_service_still_cannot_overpay() {
    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let card = app.account("credit_card", "CNY").await;
    let service = app.account("credit_service", "CNY").await;
    app.force_balance(source, "100").await;
    for amount in ["30", "20"] {
        assert_eq!(
            app.post(
                "/repayments",
                &TestApp::fields(source, card, "custom", amount)
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    assert_eq!(app.balance(source).await, "CNY 50.00");
    assert_eq!(app.balance(card).await, "CNY 50.00");
    assert_eq!(app.counts().await, (2, 0, 0));
    for (target, amount) in [(card, "51"), (service, "1")] {
        assert_eq!(
            app.post(
                "/repayments",
                &TestApp::fields(source, target, "custom", amount)
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        app.post("/repayments", &TestApp::minimum(source, card, "1", "0"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (2, 0, 0));
    app.bill(service, "25").await;
    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::fields(source, service, "custom", "26")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::fields(source, service, "custom", "25")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 25.00");
    assert_eq!(app.balance(card).await, "CNY 50.00");
    assert_eq!(app.balance(service).await, "CNY 0.00");
    assert_eq!(app.counts().await, (3, 1, 0));
}
