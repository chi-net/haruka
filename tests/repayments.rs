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
        self.account_with_billing(kind, currency, 1).await
    }
    async fn account_with_billing(&self, kind: &str, currency: &str, billing_day: i32) -> i64 {
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
                    ("billing_day", billing_day.to_string()),
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
    async fn asset_account(&self, currency: &str) -> i64 {
        assert_eq!(
            self.post(
                "/accounts",
                &[
                    ("name", "攒钱回归账户".into()),
                    ("account_kind", "bank".into()),
                    ("currency", currency.into()),
                    ("card_number", "6222000000001234".into()),
                    ("account_username", String::new()),
                    ("credit_limit", "0".into()),
                    ("billing_day", "1".into()),
                    ("repayment_day", "20".into()),
                    ("note", String::new()),
                    ("transfer_fee_rate", "0".into()),
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
    async fn installment_bill(
        &self,
        account: i64,
        amount: &str,
        first_due: chrono::NaiveDate,
    ) -> i64 {
        assert_eq!(
            self.post(
                "/bills",
                &[
                    ("account_id", account.to_string()),
                    ("kind", "expense".into()),
                    ("amount", amount.into()),
                    ("category", "还款回归消费".into()),
                    ("note", "分期账单".into()),
                    (
                        "happened_at",
                        (chrono::Utc::now() - chrono::Duration::days(90))
                            .format("%Y-%m-%dT%H:%M")
                            .to_string()
                    ),
                    ("use_installment", "true".into()),
                    ("installment_term", "3".into()),
                    ("installment_method", "equal_principal".into()),
                    ("installment_annual_rate", "0".into()),
                    ("installment_fee", "0".into()),
                    ("installment_first_due_date", first_due.to_string()),
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

    // Historical income can make the estimate larger than the actual debt.
    // Never turn that estimate into a second, phantom loan.
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
                    (chrono::Utc::now() - chrono::Duration::minutes(1))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string()
                ),
            ]
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(app.options(other_credit).await["ordinary_cents"], "5000");
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

#[tokio::test]
async fn custom_repayment_pays_only_the_current_billing_cycle_installment() {
    use chrono::Datelike;

    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let today = chrono::Utc::now().date_naive();
    let card = app
        .account_with_billing("credit_card", "CNY", today.day().min(28) as i32)
        .await;
    app.force_balance(source, "5000").await;
    app.installment_bill(card, "1200", today + chrono::Duration::days(3))
        .await;

    let before = app.options(card).await;
    assert_eq!(before["plans"][0]["remaining_cents"], "120000");
    assert_eq!(before["plans"][0]["next_cents"], "40000");

    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::fields(source, card, "custom", "400")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 4600.00");
    assert_eq!(app.balance(card).await, "CNY -800.00");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        1
    );
    let after = app.options(card).await;
    assert_eq!(after["plans"][0]["remaining_cents"], "80000");
    assert_eq!(after["plans"][0]["next_cents"], "40000");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NULL")
            .await,
        2
    );
}

#[tokio::test]
async fn overpayment_pays_current_and_pending_installments_but_not_future() {
    use chrono::Datelike;

    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let today = chrono::Utc::now().date_naive();
    let card = app
        .account_with_billing("credit_card", "CNY", today.day().min(28) as i32)
        .await;
    app.force_balance(source, "5000").await;
    app.installment_bill(card, "1200", today + chrono::Duration::days(3))
        .await;

    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert!(dashboard.contains("未出账总额"));
    assert!(dashboard.contains("待出账分期"));
    assert!(dashboard.contains("CNY 400.00"));

    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::fields(source, card, "custom", "800")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 4200.00");
    assert_eq!(app.balance(card).await, "CNY -400.00");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        2
    );
    let after = app.options(card).await;
    assert_eq!(after["plans"][0]["remaining_cents"], "40000");
    assert_eq!(after["plans"][0]["next_cents"], "40000");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NULL")
            .await,
        1
    );

    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert!(dashboard.contains("未出账总额"));
    assert!(dashboard.contains("待出账分期"));
    assert!(dashboard.contains("CNY 400.00"));
    assert!(dashboard.contains("普通消费 CNY 0.00 · 待出账分期 CNY 0.00"));
}

#[tokio::test]
async fn stock_reconcile_prompts_previews_and_only_changes_after_confirmation() {
    use chrono::Datelike;

    let app = TestApp::start().await;
    let today = chrono::Utc::now().date_naive();
    let card = app
        .account_with_billing("credit_card", "CNY", today.day().min(28) as i32)
        .await;
    app.installment_bill(card, "1200", today + chrono::Duration::days(3))
        .await;
    assert_eq!(
        app.post(
            &format!("/accounts/{card}/balance"),
            &[("balance", "-286.42".into())]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );

    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert!(dashboard.contains("检测到历史分期数据可能需要核对"));
    assert!(dashboard.contains("/installments/reconcile"));

    let preview = app.get("/installments/reconcile").await;
    assert_eq!(preview.status(), StatusCode::OK);
    let before = preview.text().await.unwrap();
    assert!(before.contains("改前"));
    assert!(before.contains("改后"));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        0
    );
    assert_eq!(app.balance(card).await, "CNY -286.42");
    assert_eq!(
        app.post("/installments/reconcile/apply", &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        0
    );

    let applied = app
        .post(
            "/installments/reconcile/apply",
            &[("confirm", "true".into())],
        )
        .await;
    assert_eq!(applied.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        2
    );
    assert_eq!(app.balance(card).await, "CNY -286.42");
    assert_eq!(
        app.text("SELECT installment_reconcile_state AS value FROM meta WHERE id = 1")
            .await,
        "done"
    );

    let preview = app
        .get("/installments/reconcile")
        .await
        .text()
        .await
        .unwrap();
    assert!(preview.contains("没有发现需要迁移的历史分期"));
    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert!(!dashboard.contains("检测到历史分期数据可能需要核对"));
}

#[tokio::test]
async fn far_future_installment_is_not_in_the_unbilled_total() {
    let app = TestApp::start().await;
    let today = chrono::Utc::now().date_naive();
    let card = app.account_with_billing("credit_card", "CNY", 5).await;
    let first_due = today.checked_add_months(chrono::Months::new(3)).unwrap();
    app.installment_bill(card, "1200", first_due).await;

    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert!(dashboard.contains("未出账总额"));
    assert!(dashboard.contains("普通消费 CNY 0.00 · 待出账分期 CNY 0.00"));
    assert!(dashboard.contains("CNY 1200.00"));
}

#[tokio::test]
async fn installment_periods_can_be_edited_and_update_the_original_bill() {
    use chrono::Datelike;

    let app = TestApp::start().await;
    let today = chrono::Utc::now().date_naive();
    let card = app
        .account_with_billing("credit_card", "CNY", today.day().min(28) as i32)
        .await;
    app.installment_bill(card, "982.83", today + chrono::Duration::days(3))
        .await;
    let item1 = app
        .scalar("SELECT MIN(id) AS value FROM installment_items")
        .await;
    let item2 = app
        .scalar("SELECT MAX(id) AS value FROM installment_items WHERE sequence = 2")
        .await;
    let item3 = app
        .scalar("SELECT MAX(id) AS value FROM installment_items WHERE sequence = 3")
        .await;

    let detail = app.get("/installments/1").await.text().await.unwrap();
    assert!(detail.contains("已入账"));
    assert!(detail.contains("即将入账"));
    assert!(detail.contains("未入账"));
    assert!(detail.contains("修改金额"));

    assert_eq!(
        app.post(
            &format!("/installments/1/items/{item1}/amount"),
            &[("amount", "300.00".into())]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(card).await, "CNY -955.22");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM balance_adjustments")
            .await,
        0
    );

    assert_eq!(
        app.post(
            &format!("/installments/1/items/{item2}/amount"),
            &[("amount", "350.00".into())]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(card).await, "CNY -977.61");
    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert!(dashboard.contains("普通消费 CNY 0.00"));

    assert_eq!(
        app.post(
            &format!("/installments/1/items/{item3}/amount"),
            &[("amount", "400.00".into())]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(card).await, "CNY -1050.00");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM balance_adjustments")
            .await,
        0
    );

    let detail = app.get("/installments/1").await.text().await.unwrap();
    assert!(detail.contains("CNY 300.00"));
    assert!(detail.contains("CNY 350.00"));
    assert!(detail.contains("CNY 400.00"));
}

#[tokio::test]
async fn paid_installment_period_amount_can_be_edited_without_undo() {
    use chrono::Datelike;

    let app = TestApp::start().await;
    let source = app.account("other", "CNY").await;
    let today = chrono::Utc::now().date_naive();
    let card = app
        .account_with_billing("credit_card", "CNY", today.day().min(28) as i32)
        .await;
    app.force_balance(source, "5000").await;
    app.installment_bill(card, "982.83", today + chrono::Duration::days(3))
        .await;
    assert_eq!(
        app.post(
            "/repayments",
            &TestApp::fields(source, card, "custom", "327.61")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        1
    );
    let item = app
        .scalar("SELECT MIN(id) AS value FROM installment_items")
        .await;
    assert_eq!(app.balance(card).await, "CNY -655.22");

    let detail = app.get("/installments/1").await.text().await.unwrap();
    assert!(detail.contains("已还"));
    assert!(detail.contains("修改金额"));

    assert_eq!(
        app.post(
            &format!("/installments/1/items/{item}/amount"),
            &[("amount", "300.00".into())]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(card).await, "CNY -627.61");
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM installment_items WHERE paid_at IS NOT NULL")
            .await,
        1
    );
}

#[tokio::test]
async fn dashboard_derives_unbilled_from_debt_and_installments() {
    let app = TestApp::start().await;
    let card = app.account_with_billing("credit_card", "CNY", 11).await;
    app.installment_bill(
        card,
        "982.83",
        chrono::Utc::now().date_naive() + chrono::Duration::days(3),
    )
    .await;
    app.bill(card, "240.15").await;
    app.db
        .execute(Statement::from_string(
            DbBackend::Sqlite,
            "UPDATE installment_items SET paid_at = '2026-11-26T00:00:00+00:00' WHERE sequence <= 2"
                .to_owned(),
        ))
        .await
        .unwrap();

    let dashboard = app.get("/dashboard").await.text().await.unwrap();
    assert_eq!(app.balance(card).await, "CNY -1222.98");
    assert!(dashboard.contains("普通消费 CNY 895.37"));
    assert!(dashboard.contains("分期剩余本金"));
    assert!(dashboard.contains("CNY 327.61"));
}

#[tokio::test]
async fn rejects_future_and_pre_1970_bill_times() {
    let app = TestApp::start().await;
    let account = app.account_with_billing("credit_card", "CNY", 1).await;
    let base = [
        ("account_id", account.to_string()),
        ("kind", "expense".into()),
        ("amount", "1.00".into()),
        ("category", "餐饮".into()),
        ("note", String::new()),
    ];
    let future = (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let mut fields = base.to_vec();
    fields.push(("happened_at", future));
    let response = app.post("/bills", &fields).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let old = (chrono::NaiveDate::from_ymd_opt(1969, 12, 31)
        .unwrap()
        .and_hms_opt(23, 59, 0)
        .unwrap())
    .format("%Y-%m-%dT%H:%M")
    .to_string();
    let mut fields = base.to_vec();
    fields.push(("happened_at", old));
    let response = app.post("/bills", &fields).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let mut fields = base.to_vec();
    fields.push((
        "happened_at",
        (chrono::Utc::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M")
            .to_string(),
    ));
    let response = app.post("/bills", &fields).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn savings_requires_a_target_and_freezes_the_linked_account() {
    let app = TestApp::start().await;
    let source = app.asset_account("CNY").await;
    let savings = app.asset_account("CNY").await;
    app.force_balance(source, "100").await;

    let response = app
        .post(
            "/savings",
            &[
                ("name", "相机".into()),
                ("kind", "item".into()),
                ("target_amount", "80".into()),
                ("account_id", savings.to_string()),
                ("note", String::new()),
                ("auto_enabled", "false".into()),
                ("source_account_id", "0".into()),
                ("amount", String::new()),
                ("cycle", "month".into()),
                ("next_run_at", String::new()),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let project = app
        .scalar("SELECT MAX(id) AS value FROM savings_projects")
        .await;
    let page = app.get("/savings").await.text().await.unwrap();
    assert!(page.contains("相机"));
    let quick = app.get("/bills/new").await.text().await.unwrap();
    assert!(quick.contains("data-quick-mode=\"savings\""));
    assert!(quick.contains("相机"));
    assert!(page.contains("目标"));
    assert!(page.contains("CNY 80.00"));

    let deposited = app
        .post(
            "/savings/deposit",
            &[
                ("project_id", project.to_string()),
                ("from_account_id", source.to_string()),
                ("amount", "50".into()),
            ],
        )
        .await;
    assert_eq!(deposited.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.balance(savings).await, "CNY 50.00");

    let blocked = app
        .post(
            "/bills",
            &[
                ("account_id", savings.to_string()),
                ("kind", "expense".into()),
                ("amount", "10.00".into()),
                ("category", "餐饮".into()),
                ("note", String::new()),
                (
                    "happened_at",
                    (chrono::Utc::now() - chrono::Duration::minutes(1))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string(),
                ),
            ],
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.balance(savings).await, "CNY 50.00");

    let redeemed = app
        .post(
            &format!("/savings/{project}/redeem"),
            &[
                ("note", "已购买相机".into()),
                ("confirm_redeem", "true".into()),
            ],
        )
        .await;
    assert_eq!(redeemed.status(), StatusCode::SEE_OTHER);
    let page = app.get("/savings").await.text().await.unwrap();
    assert!(page.contains("已兑现，附属账户资金已全部解除冻结"));
    assert_eq!(app.balance(savings).await, "CNY 50.00");

    let spent = app
        .post(
            "/bills",
            &[
                ("account_id", savings.to_string()),
                ("kind", "expense".into()),
                ("amount", "10.00".into()),
                ("category", "餐饮".into()),
                ("note", String::new()),
                (
                    "happened_at",
                    (chrono::Utc::now() - chrono::Duration::minutes(1))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string(),
                ),
            ],
        )
        .await;
    assert_eq!(spent.status(), StatusCode::OK);
    assert_eq!(app.balance(savings).await, "CNY 40.00");
}
