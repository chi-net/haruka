use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::Value;

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
            "haruka-transfer-fees-test-{}-{nonce}",
            std::process::id()
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
                ("password", "transfer-fees-regression-only"),
                ("confirm", "transfer-fees-regression-only"),
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
        Self {
            child,
            directory,
            db: Database::connect(&database_url).await.unwrap(),
            client,
            url,
            cookie,
        }
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

    async fn scalar(&self, sql: &str) -> i64 {
        self.db
            .query_one(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "value")
            .unwrap()
    }

    async fn account(&self, kind: &str, currency: &str, rate: &str) -> i64 {
        let response = self
            .post(
                "/accounts",
                &[
                    ("name", "手续费测试账户".into()),
                    ("account_kind", kind.into()),
                    ("currency", currency.into()),
                    ("card_number", String::new()),
                    ("account_username", String::new()),
                    ("credit_limit", "1000".into()),
                    ("billing_day", "1".into()),
                    ("note", String::new()),
                    ("transfer_fee_rate", rate.into()),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        self.scalar("SELECT MAX(id) AS value FROM accounts").await
    }

    async fn force_balance(&self, id: i64, balance: &str) {
        let response = self
            .post(
                &format!("/accounts/{id}/balance"),
                &[("balance", balance.into())],
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
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

    async fn transfer(
        &self,
        from: i64,
        to: i64,
        amount: &str,
        rate: &str,
        fee: &str,
        received: &str,
    ) -> Response {
        self.post(
            "/transfers",
            &[
                ("from_account_id", from.to_string()),
                ("to_account_id", to.to_string()),
                ("amount", amount.into()),
                ("to_amount", received.into()),
                ("fee_rate", rate.into()),
                ("fee_amount", fee.into()),
                ("note", String::new()),
                ("happened_at", "2026-10-10T08:00".into()),
            ],
        )
        .await
    }

    async fn counts(&self) -> (i64, i64) {
        (
            self.scalar("SELECT COUNT(*) AS value FROM transfers").await,
            self.scalar("SELECT COUNT(*) AS value FROM bills").await,
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
async fn fees_are_atomic_protected_and_revoked_with_the_transfer_or_target_account() {
    let app = TestApp::start().await;
    let source = app.account("other", "CNY", "0.1").await;
    let target = app.account("other", "CNY", "0").await;
    app.force_balance(source, "1000").await;

    assert_eq!(
        app.transfer(source, target, "1000", "", "", "")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (0, 0));
    assert_eq!(app.balance(source).await, "CNY 1000.00");
    assert_eq!(app.balance(target).await, "CNY 0.00");

    assert_eq!(
        app.transfer(source, target, "100", "", "", "")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 899.90");
    assert_eq!(app.balance(target).await, "CNY 100.00");
    assert_eq!(app.counts().await, (1, 1));
    let transfer = app.scalar("SELECT id AS value FROM transfers").await;
    let bill = app
        .scalar("SELECT fee_bill_id AS value FROM transfers")
        .await;
    let edit = app
        .client
        .get(format!("{}/bills/{bill}/edit", app.url))
        .header(header::COOKIE, &app.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(edit.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app.post(&format!("/bills/{bill}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(
            &format!("/bills/{bill}/edit"),
            &[
                ("account_id", source.to_string()),
                ("kind", "income".into()),
                ("amount", "99".into()),
                ("category", "工资".into()),
                ("note", String::new()),
                ("happened_at", "2026-10-10T08:00".into()),
            ]
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.balance(source).await, "CNY 899.90");

    app.force_balance(target, "0").await;
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (1, 1));
    assert_eq!(app.balance(source).await, "CNY 899.90");
    app.force_balance(target, "100").await;
    assert_eq!(
        app.post(&format!("/transfers/{transfer}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.counts().await, (0, 0));
    assert_eq!(app.balance(source).await, "CNY 1000.00");

    assert_eq!(
        app.transfer(source, target, "100", "", "0", "")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 900.00");
    assert_eq!(app.counts().await, (1, 0));
    assert_eq!(
        app.post(&format!("/accounts/{target}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(source).await, "CNY 1000.00");

    let target = app.account("other", "USD", "0").await;
    assert_eq!(
        app.transfer(source, target, "100", "3", "0.25", "12.34")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 899.75");
    assert_eq!(app.balance(target).await, "USD 12.34");
    assert_eq!(app.counts().await, (1, 1));
    assert_eq!(
        app.post(&format!("/accounts/{target}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(source).await, "CNY 1000.00");
    assert_eq!(app.counts().await, (0, 0));
}

#[tokio::test]
async fn credit_transfers_cannot_fund_fees_from_available_credit() {
    let app = TestApp::start().await;
    let source = app.account("credit_card", "CNY", "0.1").await;
    let target = app.account("other", "CNY", "0").await;
    app.force_balance(source, "1000").await;
    assert_eq!(
        app.transfer(source, target, "1000", "", "", "")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (0, 0));
    assert_eq!(app.balance(source).await, "CNY 1000.00");
    assert_eq!(
        app.transfer(source, target, "999", "", "", "")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(app.balance(source).await, "CNY 0.00");
    assert_eq!(app.balance(target).await, "CNY 999.00");
    let id = app.scalar("SELECT id AS value FROM transfers").await;
    assert_eq!(
        app.post(&format!("/transfers/{id}/delete"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(source).await, "CNY 1000.00");
    assert_eq!(app.balance(target).await, "CNY 0.00");
}
