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
            "haruka-credit-overpayment-{}-{nonce}-{}",
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
                ("password", "credit-overpayment-regression"),
                ("confirm", "credit-overpayment-regression"),
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
        app.create_category().await;
        app
    }
    async fn create_category(&self) {
        assert_eq!(
            self.post(
                "/categories",
                &[
                    ("kind", "expense".into()),
                    ("name", "溢缴款回归消费".into()),
                    ("is_food", "false".into()),
                ]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    async fn post(&self, path: &str, fields: &[(&str, String)]) -> Response {
        self.client
            .post(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .form(fields)
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
    async fn account(&self, kind: &str, credit_limit: &str) -> i64 {
        assert_eq!(
            self.post(
                "/accounts",
                &[
                    ("name", "溢缴款回归账户".into()),
                    ("account_kind", kind.into()),
                    ("currency", "CNY".into()),
                    ("card_number", "622200001111".into()),
                    ("account_username", String::new()),
                    ("credit_limit", credit_limit.into()),
                    ("billing_day", "1".into()),
                    ("repayment_day", String::new()),
                    ("note", String::new()),
                    ("transfer_fee_rate", String::new()),
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
    async fn balance_cents(&self, account: i64) -> String {
        let response = self
            .get(&format!("/accounts/{account}/balance-summary"))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["balance_cents"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn get(&self, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .unwrap()
    }
    async fn bill(&self, account: i64, amount: &str) -> Response {
        self.client
            .post(format!("{}/bills", self.url))
            .header(header::COOKIE, &self.cookie)
            .header(header::ACCEPT, "application/json")
            .form(&[
                ("account_id", account.to_string()),
                ("kind", "expense".into()),
                ("amount", amount.into()),
                ("category", "溢缴款回归消费".into()),
                ("note", "零额度卡支出回归".into()),
                (
                    "happened_at",
                    (chrono::Utc::now() - chrono::Duration::days(1))
                        .format("%Y-%m-%dT%H:%M")
                        .to_string(),
                ),
            ])
            .send()
            .await
            .unwrap()
    }
    async fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn zero_limit_credit_card_spends_overpayment_but_not_beyond() {
    let mut app = TestApp::start().await;
    let source = app.account("payment", "").await;
    app.force_balance(source, "1000").await;
    // 0 授信信用卡：还款形成的溢缴款可以按余额支出，但不能透支成负余额。
    let card = app.account("credit_card", "0").await;

    let repayment = app
        .post(
            "/repayments",
            &[
                ("mode", "custom".into()),
                ("from_account_id", source.to_string()),
                ("to_account_id", card.to_string()),
                ("amount", "50.63".into()),
                (
                    "happened_at",
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string(),
                ),
            ],
        )
        .await;
    assert_eq!(repayment.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.balance_cents(card).await, "5063");

    // 溢缴款范围内的支出必须成功（用户回归场景：余额 50.63，支出 30.34）。
    let first = app.bill(card, "30.34").await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(app.balance_cents(card).await, "2029");

    // 花完剩余溢缴款到正好 0 也允许。
    let exact = app.bill(card, "20.29").await;
    assert_eq!(exact.status(), StatusCode::OK);
    assert_eq!(app.balance_cents(card).await, "0");

    // 0 授信不允许负余额：再多花 0.01 必须拒绝并给出授信额度错误。
    let over = app.bill(card, "0.01").await;
    assert_eq!(over.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        over.json::<Value>().await.unwrap()["error"],
        "操作后将超过该账户的可用授信额度"
    );
    assert_eq!(app.balance_cents(card).await, "0");

    app.stop().await;
}

#[tokio::test]
async fn zero_limit_credit_card_accepts_repayment_overpayment_on_zero_balance() {
    let mut app = TestApp::start().await;
    let source = app.account("payment", "").await;
    app.force_balance(source, "100").await;
    let card = app.account("credit_card", "0").await;

    // 零欠款、零授信的信用卡允许直接存入溢缴款（自定义还款保留溢缴款）。
    let repayment = app
        .post(
            "/repayments",
            &[
                ("mode", "custom".into()),
                ("from_account_id", source.to_string()),
                ("to_account_id", card.to_string()),
                ("amount", "50.63".into()),
                (
                    "happened_at",
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string(),
                ),
            ],
        )
        .await;
    assert_eq!(repayment.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.balance_cents(card).await, "5063");

    app.stop().await;
}
