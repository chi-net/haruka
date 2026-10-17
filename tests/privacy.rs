use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::{json, Value};

#[allow(dead_code)]
#[path = "../src/crypto.rs"]
mod crypto;

struct App {
    child: Child,
    directory: PathBuf,
    db: DatabaseConnection,
    client: Client,
    url: String,
    cookie: String,
}

impl App {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("haruka-privacy-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.join("haruka.db").display()
        );
        // Exercise an upgrade database, not only a freshly generated bills schema.
        let legacy = Database::connect(&database_url).await.unwrap();
        legacy.execute(Statement::from_string(DbBackend::Sqlite,
            "CREATE TABLE bills (id INTEGER PRIMARY KEY AUTOINCREMENT, account_id INTEGER NOT NULL, kind TEXT NOT NULL, amount TEXT NOT NULL, category TEXT NOT NULL, category_id INTEGER, is_food BOOLEAN NOT NULL, note TEXT NOT NULL, happened_at TEXT NOT NULL, created_at TEXT NOT NULL)".to_owned()
        )).await.unwrap();
        legacy.close().await.unwrap();
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
                ("password", "main-regression-password"),
                ("confirm", "main-regression-password"),
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

    async fn post(&self, path: &str, fields: &[(&str, &str)], token: Option<&str>) -> Response {
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
            .header(header::COOKIE, &self.cookie);
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

    async fn bill(&self, note: &str, private: bool) -> i64 {
        let response = self
            .post(
                "/bills",
                &[
                    ("account_id", "1"),
                    ("kind", "expense"),
                    ("amount", "12.34"),
                    ("category", "购物"),
                    ("note", note),
                    ("happened_at", "2026-10-15T05:00"),
                    ("is_private", if private { "true" } else { "false" }),
                ],
                None,
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        self.scalar("SELECT MAX(id) AS value FROM bills").await
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn private_bills_require_tab_authority_and_keep_real_balances_without_metadata_leaks() {
    let app = App::start().await;
    assert_eq!(
        app.post("/accounts/1/balance", &[("balance", "1000")], None)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let private_id = app.bill("private-regression-secret-note", true).await;
    let public_id = app.bill("public-regression-note", false).await;
    for path in [
        "/bills",
        "/bills/search",
        "/bills/export.csv",
        "/bills/search/export.csv",
        "/dashboard",
        "/accounts/1",
        "/statistics",
        "/installments",
    ] {
        let response = app.get(path, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = response.text().await.unwrap();
        assert!(
            !text.contains("private-regression-secret-note"),
            "metadata leak: {path}"
        );
    }
    let ordinary = app.get("/bills", None).await.text().await.unwrap();
    assert!(ordinary.contains("public-regression-note"));
    assert!(!ordinary.contains(&format!("/bills/{private_id}/edit")));
    let balance: Value = app
        .get("/accounts/1/balance-summary", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(balance["balance"], "CNY 975.32");
    assert_eq!(
        app.get("/privacy/bills", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let navigation = app
        .client
        .get(format!("{}/privacy/bills", app.url))
        .header(header::COOKIE, &app.cookie)
        .header(header::ACCEPT, "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(navigation.status(), StatusCode::SEE_OTHER);
    assert_eq!(navigation.headers()[header::LOCATION], "/privacy");
    assert_eq!(
        app.get(&format!("/bills/{private_id}/edit"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.post(&format!("/bills/{private_id}/delete"), &[], None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 2);

    let setup = app
        .post(
            "/privacy/setup",
            &[
                ("password", "privacy-regression-password"),
                ("confirm", "privacy-regression-password"),
            ],
            None,
        )
        .await;
    assert_eq!(setup.status(), StatusCode::OK);
    assert!(setup.headers().get(header::SET_COOKIE).is_none());
    let payload: Value = setup.json().await.unwrap();
    let token = payload["token"].as_str().unwrap().to_owned();
    let private_view = app
        .get("/privacy/bills", Some(&token))
        .await
        .text()
        .await
        .unwrap();
    assert!(private_view.contains("private-regression-secret-note"));
    assert!(!private_view.contains("public-regression-note"));
    assert!(!app
        .get("/bills", Some(&token))
        .await
        .text()
        .await
        .unwrap()
        .contains("private-regression-secret-note"));
    assert_eq!(
        app.get(&format!("/bills/{private_id}/edit"), Some(&token))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.get(&format!("/bills/{private_id}/share"), Some(&token))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    // Main-app cookies do not carry privacy authority; a new main session cannot reuse another tab's grant.
    let other = app
        .client
        .post(format!("{}/unlock", app.url))
        .form(&[("password", "main-regression-password")])
        .send()
        .await
        .unwrap();
    let other_cookie = other.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let cross_session = app
        .client
        .get(format!("{}/privacy/bills", app.url))
        .header(header::COOKIE, other_cookie)
        .header("X-Haruka-Privacy", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(cross_session.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        app.get("/privacy/bills", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.post(
            "/privacy/unlock",
            &[("password", "main-regression-password")],
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.scalar("SELECT failed_password_rounds AS value FROM meta WHERE id=1")
            .await,
        0
    );

    assert_eq!(
        app.post(
            "/privacy/password",
            &[
                ("current_password", "privacy-regression-password"),
                ("password", "new-privacy-password"),
                ("confirm", "new-privacy-password")
            ],
            Some(&token)
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.get("/privacy/bills", Some(&token)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.post(
            "/privacy/unlock",
            &[("password", "privacy-regression-password")],
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let unlocked = app
        .post(
            "/privacy/unlock",
            &[("password", "new-privacy-password")],
            None,
        )
        .await;
    assert_eq!(unlocked.status(), StatusCode::OK);
    let payload: Value = unlocked.json().await.unwrap();
    let token = payload["token"].as_str().unwrap();
    assert_eq!(
        app.post("/privacy/lock", &[], Some(token)).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.get("/privacy/bills", Some(token)).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Converting a public bill must atomically revoke snapshots that could otherwise remain publicly readable.
    let expires = (chrono::Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let share = app
        .post(
            &format!("/bills/{public_id}/share"),
            &[("expires_at", &expires), ("password", ""), ("confirm", "")],
            None,
        )
        .await;
    assert_eq!(share.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bill_shares")
            .await,
        1
    );
    let update = app
        .post(
            &format!("/bills/{public_id}/edit"),
            &[
                ("account_id", "1"),
                ("kind", "expense"),
                ("amount", "12.34"),
                ("category", "购物"),
                ("note", "newly-private-note"),
                ("happened_at", "2026-10-15T05:00"),
                ("is_private", "true"),
            ],
            None,
        )
        .await;
    assert_eq!(update.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bill_shares")
            .await,
        0
    );
    assert_eq!(
        app.get(&format!("/bills/{public_id}/edit"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let batch = app.client.post(format!("{}/bills/batch", app.url)).header(header::COOKIE, &app.cookie).json(&json!({"entries":[
        {"account_id":"1","kind":"expense","amount":"1","category":"购物","note":"private-batch-note","happened_at":"2026-10-15T06:00","is_private":true},
        {"account_id":"1","kind":"expense","amount":"2","category":"购物","note":"public-batch-note","happened_at":"2026-10-15T06:00","is_private":false}
    ]})).send().await.unwrap();
    assert_eq!(batch.status(), StatusCode::OK);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM bills WHERE is_private=1")
            .await,
        3
    );
    let ordinary = app.get("/bills", None).await.text().await.unwrap();
    assert!(ordinary.contains("public-batch-note"));
    assert!(!ordinary.contains("private-batch-note"));
}

#[tokio::test]
async fn private_money_is_included_in_summaries_without_exposing_private_rows() {
    let app = App::start().await;
    for (kind, name, food) in [
        ("income", "公开收入", "false"),
        ("income", "private-income-category", "false"),
        ("expense", "公开非食品", "false"),
        ("expense", "private-food-category", "true"),
    ] {
        assert_eq!(
            app.post(
                "/categories",
                &[("kind", kind), ("name", name), ("is_food", food)],
                None,
            )
            .await
            .status(),
            StatusCode::SEE_OTHER,
        );
    }
    let happened_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string();
    for (kind, amount, category, note, private) in [
        ("income", "1000", "公开收入", "public-income", "false"),
        (
            "income",
            "25",
            "private-income-category",
            "private-income-note",
            "true",
        ),
        ("expense", "100", "公开非食品", "public-expense", "false"),
        (
            "expense",
            "50",
            "private-food-category",
            "private-food-note",
            "true",
        ),
    ] {
        assert_eq!(
            app.post(
                "/bills",
                &[
                    ("account_id", "1"),
                    ("kind", kind),
                    ("amount", amount),
                    ("category", category),
                    ("note", note),
                    ("happened_at", &happened_at),
                    ("is_private", private),
                ],
                None,
            )
            .await
            .status(),
            StatusCode::OK,
        );
    }
    for path in [
        "/bills?time_zone=UTC",
        "/bills/search?keyword=no-matching-note&time_zone=UTC",
    ] {
        let response = app.get(path, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = response.text().await.unwrap();
        assert!(html.contains("+CNY 1025.00"), "{path}");
        assert!(html.contains("-CNY 150.00"), "{path}");
        assert!(html.contains("33.3%"), "{path}");
        for secret in ["private-income-note", "private-food-note"] {
            assert!(!html.contains(secret), "{path}: {secret}");
        }
    }
    let stats = app.get("/statistics", None).await.text().await.unwrap();
    assert!(stats.contains("+CNY 1025.00"));
    assert!(stats.contains("-CNY 150.00"));
    assert!(stats.contains("33.3%"));
    assert!(!stats.contains("private-food-category"));
    assert!(!stats.contains("private-food-note"));
    let balance: Value = app
        .get("/accounts/1/balance-summary", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(balance["balance"], "CNY 875.00");
}

#[tokio::test]
async fn account_usernames_are_complete_but_cards_require_explicit_unlocked_access() {
    let app = App::start().await;
    let mut ids = Vec::new();
    for (kind, name, card, username) in [
        ("bank", "完整银行卡账户", "6225888812345678", ""),
        ("payment", "完整支付账户", "", "complete-account-user-1999"),
        ("payment", "单字符支付账户", "", "甲"),
    ] {
        let created = app
            .post(
                "/accounts",
                &[
                    ("name", name),
                    ("account_kind", kind),
                    ("currency", "CNY"),
                    ("card_number", card),
                    ("account_username", username),
                    ("note", ""),
                    ("credit_limit", "0"),
                    ("billing_day", "1"),
                ],
                None,
            )
            .await;
        assert_eq!(created.status(), StatusCode::SEE_OTHER);
        let id = app.scalar("SELECT MAX(id) AS value FROM accounts").await;
        ids.push(id);
        let response = app.get(&format!("/accounts/{id}"), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::PRAGMA], "no-cache");
        let html = response.text().await.unwrap();
        assert!(html.contains(name));
        let edit = app
            .get(&format!("/accounts/{id}/edit"), None)
            .await
            .text()
            .await
            .unwrap();
        let identifier = if card.is_empty() { username } else { "5678" };
        assert!(html.contains(identifier));
        assert!(edit.contains(identifier));
        for path in [
            "/accounts".to_owned(),
            "/bills/new".to_owned(),
            "/dashboard".to_owned(),
            format!("/accounts/{id}/statement"),
        ] {
            let response = app.get(&path, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let page = response.text().await.unwrap();
            assert!(page.contains(&format!("{name}({identifier})")), "{path}");
            if !card.is_empty() {
                assert!(!page.contains(card), "{path}");
            }
        }
        if !card.is_empty() {
            assert!(!html.contains(card));
            assert!(!edit.contains(card));
            let copied = app
                .post(&format!("/accounts/{id}/card-number"), &[], None)
                .await;
            assert_eq!(copied.status(), StatusCode::OK);
            assert_eq!(copied.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(copied.headers()[header::PRAGMA], "no-cache");
            assert_eq!(copied.json::<Value>().await.unwrap()["card_number"], card);
        } else {
            assert_eq!(
                app.post(&format!("/accounts/{id}/card-number"), &[], None)
                    .await
                    .status(),
                StatusCode::NOT_FOUND,
            );
        }
    }
    assert!(app.post("/lock", &[], None).await.status().is_redirection());
    for id in ids {
        let locked = app.get(&format!("/accounts/{id}"), None).await;
        assert!(locked.status().is_redirection());
        assert_eq!(locked.headers()[header::LOCATION], "/unlock");
        let card = app
            .post(&format!("/accounts/{id}/card-number"), &[], None)
            .await;
        assert!(card.status().is_redirection());
        assert_eq!(card.headers()[header::LOCATION], "/unlock");
    }
}

#[tokio::test]
async fn owner_name_stays_private_and_only_nonblank_submissions_update_encrypted_storage() {
    let app = App::start().await;
    let meta = app
        .db
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
    let kek = crypto::derive_kek("main-regression-password", &salt);
    let dek = crypto::unwrap_dek(&nonce, &wrapped, kek.as_slice()).unwrap();
    let original_name = "隐私姓名原值甲";
    let original_ciphertext = crypto::encrypt(&dek, original_name.as_bytes());
    app.db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE preferences SET owner_name = ? WHERE id = 1",
            [original_ciphertext.clone().into()],
        ))
        .await
        .unwrap();

    let response = app.get("/settings", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.text().await.unwrap().contains(original_name));

    for blank in ["", " \t\r\n\u{3000} "] {
        let saved = app
            .post("/settings/owner-name", &[("owner_name", blank)], None)
            .await;
        assert_eq!(saved.status(), StatusCode::SEE_OTHER);
        let stored: String = app
            .db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT owner_name FROM preferences WHERE id = 1".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "owner_name")
            .unwrap();
        assert_eq!(stored, original_ciphertext);
        assert_eq!(crypto::decrypt_string(&dek, &stored), original_name);
    }

    let new_name = "隐私姓名新值乙";
    let padded = format!(" \t{new_name}\u{3000} ");
    let saved = app
        .post("/settings/owner-name", &[("owner_name", &padded)], None)
        .await;
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    let updated: String = app
        .db
        .query_one(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT owner_name FROM preferences WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "owner_name")
        .unwrap();
    assert_ne!(updated, original_ciphertext);
    assert!(!updated.contains(new_name));
    assert_eq!(crypto::decrypt_string(&dek, &updated), new_name);
    let response = app.get("/settings", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = response.text().await.unwrap();
    assert!(!html.contains(original_name));
    assert!(!html.contains(new_name));
}
