use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use tokio::task::JoinSet;

const PASSWORD: &str = "password-attempts-regression-only";
const WRONG_PASSWORD: &str = "not-the-master-password";
const BILL_NOTE: &str = "password-attempts-preserved-bill";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TestApp {
    child: Option<Child>,
    directory: TestDirectory,
    db: DatabaseConnection,
    client: Client,
    url: String,
    cookie: String,
    mnemonic: String,
}

impl TestApp {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = TestDirectory(std::env::temp_dir().join(format!(
            "haruka-password-attempts-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        )));
        std::fs::create_dir(&directory.0).unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.0.join("haruka.db").display()
        );
        let mut app = Self {
            child: None,
            directory,
            db: Database::connect(&database_url).await.unwrap(),
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap(),
            url: String::new(),
            cookie: String::new(),
            mnemonic: String::new(),
        };
        app.restart().await;
        let response = app
            .post(
                "/setup",
                &[("password", PASSWORD), ("confirm", PASSWORD)],
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        app.cookie = session_cookie(&response);
        app.mnemonic = recovery_phrase(&response.text().await.unwrap());
        assert_eq!(app.rounds().await, 0);
        app
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            child.wait().unwrap();
        }
    }

    async fn restart(&mut self) {
        self.stop();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            self.directory.0.join("haruka.db").display()
        );
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_haruka"))
                .args(["--listen", &address.to_string()])
                .env("DATABASE_URL", database_url)
                .env_remove("PASSKEY_ORIGIN")
                .env_remove("PASSKEY_RP_ID")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        self.url = format!("http://{address}");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.client.get(format!("{}/setup", self.url)).send().await {
                Ok(response) => {
                    assert!(matches!(
                        response.status(),
                        StatusCode::OK | StatusCode::SEE_OTHER
                    ));
                    break;
                }
                Err(error) => {
                    assert!(Instant::now() < deadline, "server startup failed: {error}");
                    assert!(
                        self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                        "server exited during startup"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    async fn post(&self, path: &str, fields: &[(&str, &str)], cookie: Option<&str>) -> Response {
        let request = self
            .client
            .post(format!("{}{path}", self.url))
            .header(header::ACCEPT, "application/json")
            .form(fields);
        let request = match cookie {
            Some(cookie) => request.header(header::COOKIE, cookie),
            None => request,
        };
        request.send().await.unwrap()
    }

    async fn unlock(&self, password: &str, cookie: Option<&str>) -> Response {
        self.post("/unlock", &[("password", password)], cookie)
            .await
    }

    async fn exec(&self, sql: &str) {
        self.db
            .execute(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
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

    async fn rounds(&self) -> i64 {
        self.scalar("SELECT failed_password_rounds AS value FROM meta WHERE id = 1")
            .await
    }

    async fn strings(&self, sql: &str) -> Vec<String> {
        self.db
            .query_all(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get("", "value").unwrap())
            .collect()
    }

    async fn encrypted_records(&self) -> Vec<String> {
        self.strings(
            "SELECT 'account:' || id || ':' || name || ':' || balance_offset || ':' || note AS value FROM accounts \
             UNION ALL SELECT 'bill:' || id || ':' || account_id || ':' || kind || ':' || amount || ':' || category || ':' || note || ':' || happened_at || ':' || created_at FROM bills \
             UNION ALL SELECT 'recovery:' || hex(dek_nonce) || ':' || hex(wrapped_dek) FROM recovery \
             ORDER BY value",
        )
        .await
    }

    async fn wrapped_password(&self) -> Vec<String> {
        self.strings("SELECT hex(salt) || ':' || hex(dek_nonce) || ':' || hex(wrapped_dek) AS value FROM meta")
            .await
    }

    async fn create_bill(&self) {
        let account = self.scalar("SELECT MIN(id) AS value FROM accounts").await;
        let account = account.to_string();
        let response = self
            .post(
                "/bills",
                &[
                    ("account_id", &account),
                    ("kind", "income"),
                    ("amount", "123.45"),
                    ("category", "工资"),
                    ("note", BILL_NOTE),
                    ("happened_at", "2026-10-10T08:00"),
                ],
                Some(&self.cookie),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(self.scalar("SELECT COUNT(*) AS value FROM bills").await, 1);
    }

    async fn assert_bill_access(&self, cookie: &str) {
        let response = self
            .client
            .get(format!("{}/bills", self.url))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains(BILL_NOTE));
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn session_cookie(response: &Response) -> String {
    response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn recovery_phrase(html: &str) -> String {
    let element = html.split("id=\"recovery-phrase\"").nth(1).unwrap();
    element
        .split_once('>')
        .unwrap()
        .1
        .split("</textarea>")
        .next()
        .unwrap()
        .trim()
        .to_owned()
}

async fn assert_incorrect(response: Response, recovery: bool) {
    assert_eq!(
        response.status(),
        if recovery {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::OK
        }
    );
    assert!(!response.headers().contains_key(header::SET_COOKIE));
}

fn assert_locked(response: &Response) {
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
}

#[tokio::test]
async fn login_and_recovery_share_failures_without_success_or_cookie_resets() {
    let mut app = TestApp::start().await;
    app.create_bill().await;
    let records = app.encrypted_records().await;
    let setup_cookie = app.cookie.clone();

    // An absent repair ceremony must fail before verifying the supplied password.
    for _ in 0..3 {
        let response = app
            .client
            .post(format!("{}/passkey/auth/repair", app.url))
            .json(&serde_json::json!({
                "flow_id": "nonexistent-regression-flow",
                "password": WRONG_PASSWORD,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    for failure in 1..=9 {
        let recovery = matches!(failure, 3 | 7);
        let response = if recovery {
            app.post(
                "/settings/recovery",
                &[("password", WRONG_PASSWORD)],
                Some(&app.cookie),
            )
            .await
        } else {
            let cookie = match failure % 3 {
                0 => Some(setup_cookie.as_str()),
                1 => None,
                _ => Some("sid=unrecognized-regression-cookie"),
            };
            app.unlock(WRONG_PASSWORD, cookie).await
        };
        assert_incorrect(response, recovery).await;
        assert_eq!(app.rounds().await, 0);
        if failure == 4 {
            let response = app.unlock(PASSWORD, None).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            app.cookie = session_cookie(&response);
        }
        if failure == 6 {
            let response = app
                .post(
                    "/settings/recovery",
                    &[("password", PASSWORD)],
                    Some(&app.cookie),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            app.mnemonic = recovery_phrase(&response.text().await.unwrap());
        }
    }
    assert_locked(&app.unlock(WRONG_PASSWORD, None).await);
    assert_eq!(app.rounds().await, 1);
    for cookie in [None, Some("sid=another-cookie"), Some(app.cookie.as_str())] {
        assert_locked(&app.unlock(PASSWORD, cookie).await);
    }
    assert_locked(
        &app.post(
            "/settings/recovery",
            &[("password", PASSWORD)],
            Some(&app.cookie),
        )
        .await,
    );
    assert_eq!(app.rounds().await, 1);
    app.assert_bill_access(&app.cookie).await;
    // Regenerating recovery deliberately changes its wrapper, but not the bills/accounts.
    let after = app.encrypted_records().await;
    assert_eq!(
        records
            .iter()
            .filter(|row| !row.starts_with("recovery:"))
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|row| !row.starts_with("recovery:"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn concurrent_failures_exhaust_one_global_round_exactly_once() {
    let app = TestApp::start().await;
    let mut requests = JoinSet::new();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    for request in 0..16 {
        let client = app.client.clone();
        let url = app.url.clone();
        let barrier = barrier.clone();
        requests.spawn(async move {
            barrier.wait().await;
            let mut builder = client
                .post(format!("{url}/unlock"))
                .form(&[("password", WRONG_PASSWORD)]);
            if request % 2 == 0 {
                builder = builder.header(header::COOKIE, format!("sid=concurrent-{request}"));
            }
            let response = builder.send().await.unwrap();
            let status = response.status();
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            status
        });
    }
    let mut incorrect = 0;
    let mut locked = 0;
    while let Some(result) = requests.join_next().await {
        let status = result.unwrap();
        match status {
            StatusCode::OK => incorrect += 1,
            StatusCode::TOO_MANY_REQUESTS => locked += 1,
            other => panic!("unexpected concurrent password result: {other}"),
        }
    }
    assert_eq!(incorrect, 9);
    assert_eq!(locked, 7);
    assert_eq!(app.rounds().await, 1);
    assert_locked(&app.unlock(PASSWORD, None).await);
    assert_locked(&app.unlock(WRONG_PASSWORD, None).await);
    assert_eq!(app.rounds().await, 1);
}

#[tokio::test]
async fn legacy_meta_migrates_and_three_rounds_preserve_data_until_manual_reset() {
    let mut app = TestApp::start().await;
    app.create_bill().await;
    let records = app.encrypted_records().await;
    let wrapper = app.wrapped_password().await;
    app.stop();
    app.exec("ALTER TABLE meta DROP COLUMN failed_password_rounds")
        .await;
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM pragma_table_info('meta') WHERE name = 'failed_password_rounds'")
            .await,
        0
    );
    app.restart().await;
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM pragma_table_info('meta') WHERE name = 'failed_password_rounds'")
            .await,
        1
    );
    assert_eq!(app.rounds().await, 0);

    for round in 1..=3 {
        if round > 1 {
            app.restart().await;
        }
        assert_eq!(app.rounds().await, round - 1);
        let response = app.unlock(PASSWORD, None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        app.cookie = session_cookie(&response);
        app.assert_bill_access(&app.cookie).await;
        for _ in 1..=9 {
            assert_incorrect(app.unlock(WRONG_PASSWORD, None).await, false).await;
        }
        assert_locked(&app.unlock(WRONG_PASSWORD, None).await);
        assert_locked(&app.unlock(PASSWORD, None).await);
        assert_eq!(app.rounds().await, round);
        app.assert_bill_access(&app.cookie).await;
        assert!(app.directory.0.join("haruka.db").is_file());
        assert_eq!(app.encrypted_records().await, records);
        assert_eq!(app.wrapped_password().await, wrapper);
    }

    app.restart().await;
    assert_eq!(app.rounds().await, 3);
    assert_locked(&app.unlock(PASSWORD, None).await);
    assert_locked(&app.unlock(WRONG_PASSWORD, None).await);
    assert_eq!(app.encrypted_records().await, records);
    assert_eq!(app.wrapped_password().await, wrapper);

    // Real mnemonic recovery still creates an unlocked session, without clearing the gate.
    let response = app
        .post(
            "/recover",
            &[
                ("mnemonic", &app.mnemonic),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
            ],
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    app.cookie = session_cookie(&response);
    assert_eq!(app.rounds().await, 3);
    assert_locked(&app.unlock(PASSWORD, None).await);
    app.assert_bill_access(&app.cookie).await;
    assert_eq!(app.encrypted_records().await, records);

    app.stop();
    app.exec("UPDATE meta SET failed_password_rounds = 0 WHERE id = 1")
        .await;
    app.restart().await;
    assert_eq!(app.rounds().await, 0);
    let response = app.unlock(PASSWORD, None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    app.cookie = session_cookie(&response);
    app.assert_bill_access(&app.cookie).await;
    assert!(app.directory.0.join("haruka.db").is_file());
    assert_eq!(app.encrypted_records().await, records);
}

#[tokio::test]
async fn failed_round_persistence_stays_closed_and_retries_without_double_counting() {
    let app = TestApp::start().await;
    app.create_bill().await;
    let records = app.encrypted_records().await;
    let wrapper = app.wrapped_password().await;
    for _ in 1..=9 {
        assert_incorrect(app.unlock(WRONG_PASSWORD, None).await, false).await;
    }
    app.exec(
        "CREATE TRIGGER reject_password_round BEFORE UPDATE OF failed_password_rounds ON meta \
         BEGIN SELECT RAISE(ABORT, 'regression simulated persistence failure'); END",
    )
    .await;
    let response = app.unlock(WRONG_PASSWORD, None).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert_eq!(app.rounds().await, 0);
    for password in [PASSWORD, WRONG_PASSWORD, PASSWORD] {
        let response = app.unlock(password, None).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(app.rounds().await, 0);
    }
    app.assert_bill_access(&app.cookie).await;
    app.exec("DROP TRIGGER reject_password_round").await;
    for password in [PASSWORD, WRONG_PASSWORD, PASSWORD] {
        assert_locked(&app.unlock(password, None).await);
        assert_eq!(app.rounds().await, 1);
    }
    assert_eq!(app.encrypted_records().await, records);
    assert_eq!(app.wrapped_password().await, wrapper);
    assert!(app.directory.0.join("haruka.db").is_file());
}
