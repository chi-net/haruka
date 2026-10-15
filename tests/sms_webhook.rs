use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header, Client, RequestBuilder, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
use serde_json::{json, Value};

const TOKEN: &str = "isolated-sms-webhook-regression-token";
const PASSWORD: &str = "sms-webhook-regression-password";

static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

struct TestApp {
    child: Child,
    directory: PathBuf,
    database_url: String,
    client: Client,
    url: String,
}

impl TestApp {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "haruka-sms-webhook-{}-{nonce}-{sequence}",
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
            .env("SMS_API_TOKEN", TOKEN)
            .env_remove("PASSKEY_ORIGIN")
            .env_remove("PASSKEY_RP_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut app = Self {
            child,
            directory,
            database_url,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
            url: format!("http://{address}"),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match app.client.get(format!("{}/setup", app.url)).send().await {
                Ok(response) => {
                    assert_eq!(response.status(), StatusCode::OK);
                    break;
                }
                Err(error) => {
                    assert!(Instant::now() < deadline, "server startup failed: {error}");
                    assert!(
                        app.child.try_wait().unwrap().is_none(),
                        "server exited during startup"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        app
    }

    fn sms(&self) -> RequestBuilder {
        self.client.post(format!("{}/api/sms", self.url))
    }

    async fn setup(&self) -> String {
        let response = self
            .client
            .post(format!("{}/setup", self.url))
            .form(&[("password", PASSWORD), ("confirm", PASSWORD)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn payload(raw: &str) -> Value {
    json!({ "time": "2026-10-08T03:45:12Z", "raw": raw })
}

async fn response_json(response: Response, expected: StatusCode) -> Value {
    assert_eq!(response.status(), expected);
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    let content_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .map(|value| value.to_str().unwrap().parse::<usize>().unwrap());
    let body = response
        .bytes()
        .await
        .expect("HTTP response body was truncated");
    if let Some(content_length) = content_length {
        assert_eq!(body.len(), content_length);
    }
    let value: Value = serde_json::from_slice(&body).unwrap();
    if expected.is_client_error() || expected.is_server_error() {
        assert_eq!(value["ok"], false);
        assert_eq!(value["status"], expected.as_u16());
    }
    value
}

#[tokio::test]
async fn rejected_sms_requests_return_complete_json_instead_of_losing_the_connection() {
    let app = TestApp::start().await;
    let valid = payload("HTTP transport regression: unrecognized notification");

    // Shortcuts may omit Accept or use */*: an API extraction error must not become HTML.
    for content_type in [None, Some("text/plain")] {
        let mut request = app.sms().bearer_auth(TOKEN).body(valid.to_string());
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        response_json(
            request.send().await.unwrap(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
        .await;
    }
    let response = app
        .client
        .get(format!("{}/api/sms", app.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()[header::ALLOW], "POST");
    response_json(response, StatusCode::METHOD_NOT_ALLOWED).await;

    response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .header(header::CONTENT_TYPE, "application/json")
            .body("{")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    for invalid in [
        json!({ "time": "2026-10-08T03:45:12Z" }),
        json!({ "time": 7, "raw": "notice" }),
    ] {
        response_json(
            app.sms()
                .bearer_auth(TOKEN)
                .json(&invalid)
                .send()
                .await
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    for invalid in [
        json!({ "time": "2026-10-08 11:45", "raw": "notice" }),
        payload(" "),
        payload(&"a".repeat(4097)),
        json!({ "time": "2026-10-08T03:45:12Z", "raw": "notice", "sender": "95\n555" }),
    ] {
        response_json(
            app.sms()
                .bearer_auth(TOKEN)
                .json(&invalid)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .json(&payload(&"a".repeat(2 * 1024 * 1024)))
            .send()
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    for request in [
        app.sms().json(&valid),
        app.sms().bearer_auth("wrong-token").json(&valid),
        app.sms()
            .bearer_auth("wrong-token")
            .header("Authentication", format!("Bearer {TOKEN}"))
            .json(&valid),
    ] {
        response_json(request.send().await.unwrap(), StatusCode::UNAUTHORIZED).await;
    }
    let accepted = response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .json(&valid)
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    assert_eq!(accepted["status"], "queued");
    assert_eq!(accepted["pending"], 1);

    // A real storage error in this isolated database must also be a complete HTTP error.
    let db = Database::connect(&app.database_url).await.unwrap();
    db.execute(Statement::from_string(
        DbBackend::Sqlite,
        "DROP TABLE preferences".to_owned(),
    ))
    .await
    .unwrap();
    db.close().await.unwrap();
    response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .json(&valid)
            .send()
            .await
            .unwrap(),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
}

#[tokio::test]
async fn sms_authentication_queue_and_synchronous_unlocked_processing_keep_their_contract() {
    let app = TestApp::start().await;
    let queued = payload("HTTP regression queued notification");
    for request in [
        app.sms().bearer_auth(TOKEN).json(&queued),
        app.sms()
            .header("Authentication", format!("Bearer {TOKEN}"))
            .json(&queued),
    ] {
        let value = response_json(request.send().await.unwrap(), StatusCode::ACCEPTED).await;
        assert_eq!(value["ok"], true);
        assert_eq!(value["status"], "queued");
        assert_eq!(
            value["pending"], 1,
            "duplicate requests must not grow the queue"
        );
    }
    let cookie = app.setup().await;
    // /api/sms uses the active server session's DEK, not a cookie on the SMS request.
    // Unrecognized text is an application-level unmatched outcome, not a transport error.
    let synchronous = response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .json(&payload("HTTP regression synchronous notification"))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(synchronous["ok"], false);
    assert_eq!(synchronous["status"], "unmatched");
    assert_eq!(synchronous["pending"], 1);
    let processed = response_json(
        app.client
            .post(format!("{}/sms/process-pending", app.url))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    // The explicit retry also revisits the synchronous unmatched audit event.
    assert_eq!(processed["processed"], 2);
    for (amount, balance) in [
        ("79228162514264337593543950335", "900.00"),
        ("100.00", "79228162514264337593543950335"),
    ] {
        let overflow = json!({
            "time": "2026-10-08T12:30:00+08:00",
            "sender": "95555",
            "raw": format!("【招商银行】您账户1234于10月08日12:30在信用卡还款-张三快捷支付{amount}元，余额{balance}元。"),
        });
        let value = response_json(
            app.sms()
                .bearer_auth(TOKEN)
                .json(&overflow)
                .send()
                .await
                .expect("out-of-range SMS amounts must not close the connection"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(value["ok"], false);
        assert_eq!(value["status"], "unmatched");
        assert_eq!(value["pending"], 0);
    }
    let audit = app
        .client
        .get(format!("{}/sms", app.url))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(audit.status(), StatusCode::OK);
    let audit = audit.text().await.unwrap();
    assert!(audit.contains("HTTP regression queued notification"));
    assert!(audit.contains("HTTP regression synchronous notification"));

    let locked = app
        .client
        .post(format!("{}/lock", app.url))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(locked.status(), StatusCode::SEE_OTHER);
    for index in 0..100 {
        let value = response_json(
            app.sms()
                .bearer_auth(TOKEN)
                .json(&payload(&format!("HTTP regression queue boundary {index}")))
                .send()
                .await
                .unwrap(),
            StatusCode::ACCEPTED,
        )
        .await;
        assert_eq!(value["pending"], index + 1);
    }
    response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .json(&payload("HTTP regression queue overflow"))
            .send()
            .await
            .unwrap(),
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    let duplicate = response_json(
        app.sms()
            .bearer_auth(TOKEN)
            .json(&payload("HTTP regression queue boundary 0"))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    assert_eq!(duplicate["pending"], 100);
}
