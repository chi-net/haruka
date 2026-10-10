use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header, Client, Response, StatusCode};
use serde_json::{json, Value};

struct TestApp {
    child: Child,
    directory: PathBuf,
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
            "haruka-finance-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let child = Command::new(env!("CARGO_BIN_EXE_haruka"))
            .args(["--listen", &address.to_string()])
            .env(
                "DATABASE_URL",
                format!(
                    "sqlite://{}?mode=rwc",
                    directory.join("haruka.db").display()
                ),
            )
            .env("SMS_API_TOKEN", "isolated-finance-regression-token")
            .env_remove("PASSKEY_ORIGIN")
            .env_remove("PASSKEY_RP_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut app = Self {
            child,
            directory,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
            url: format!("http://{address}"),
            cookie: String::new(),
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
        let response = app
            .client
            .post(format!("{}/setup", app.url))
            .form(&[
                ("password", "finance-regression-only"),
                ("confirm", "finance-regression-only"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        app.cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
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

    async fn data(&self) -> Value {
        let response = self
            .client
            .get(format!("{}/finance", self.url))
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = response.text().await.unwrap();
        let element = &html[html.find("id=\"finance-data\"").unwrap()..];
        let content = &element[element.find('>').unwrap() + 1..];
        serde_json::from_str(content.split("</script>").next().unwrap()).unwrap()
    }

    async fn save(&self, data: &Value, config: &Value) -> Value {
        let response = self
            .post(
                "/finance/save",
                &[
                    ("revision", data["revision"].to_string()),
                    ("config_json", config.to_string()),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn generate(&self, data: &Value) -> Value {
        let response = self
            .post(
                "/finance/generate",
                &[
                    ("revision", data["revision"].to_string()),
                    (
                        "generation_token",
                        data["generation_token"].as_str().unwrap().to_owned(),
                    ),
                ],
            )
            .await;
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

fn plan_ids(data: &Value) -> Vec<i64> {
    data["linked_plans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plan| plan["plan_id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn paused_plans_resume_without_duplicates_and_deleted_accounts_remain_editable() {
    let app = TestApp::start().await;
    let initial = app.data().await;
    let source = initial["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["kind"] == "other")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let mut config = initial["config"].clone();
    config["manual_monthly_income"] = json!("4000.00");
    config["from_account_id"] = json!(source);
    let saved = app.save(&initial, &config).await;
    let generated = app.generate(&saved).await;
    let original_ids = plan_ids(&generated);
    assert_eq!(
        original_ids.len(),
        config["allocations"].as_array().unwrap().len()
    );

    config = generated["config"].clone();
    config["investment_bps"] = json!(0);
    let saved = app.save(&generated, &config).await;
    let paused = app.generate(&saved).await;
    assert_eq!(
        plan_ids(&paused),
        original_ids,
        "pausing must retain the plan bindings"
    );

    config = paused["config"].clone();
    config["investment_bps"] = json!(1000);
    let saved = app.save(&paused, &config).await;
    let resumed = app.generate(&saved).await;
    assert_eq!(
        plan_ids(&resumed),
        original_ids,
        "resuming must reuse plans, not create another set"
    );

    let response = app.post(&format!("/accounts/{source}/delete"), &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let recoverable = app.data().await;
    assert_eq!(recoverable["config"]["from_account_id"], json!(source));
    assert_eq!(
        recoverable["config"]["manual_monthly_income"],
        json!("4000.00")
    );
    assert!(
        recoverable["preview"].is_null(),
        "an unavailable account must block generation but not editing"
    );
    let response = app
        .post(
            "/finance/save",
            &[
                ("revision", recoverable["revision"].to_string()),
                ("config_json", recoverable["config"].to_string()),
            ],
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "invalid account references must not be saved"
    );
    let response = app
        .post(
            "/finance/generate",
            &[
                ("revision", recoverable["revision"].to_string()),
                (
                    "generation_token",
                    recoverable["generation_token"].as_str().unwrap().to_owned(),
                ),
            ],
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "GET recovery must not weaken write validation"
    );
    assert_eq!(app.data().await["revision"], recoverable["revision"]);
}
