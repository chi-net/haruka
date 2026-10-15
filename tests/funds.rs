use std::{
    collections::HashMap,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use regex::Regex;
use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::Value;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
type Fields = HashMap<String, String>;

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
            "haruka-funds-test-{}-{nonce}-{}",
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
                ("password", "funds-regression-only"),
                ("confirm", "funds-regression-only"),
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

    async fn get(&self, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, fields: &Fields) -> Response {
        self.client
            .post(format!("{}{path}", self.url))
            .header(header::COOKIE, &self.cookie)
            .header(header::ACCEPT, "application/json")
            .form(fields)
            .send()
            .await
            .unwrap()
    }

    async fn execute(&self, sql: &str) {
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

    async fn account(&self, kind: &str, currency: &str) -> i64 {
        let response = self
            .post(
                "/accounts",
                &fields(&[
                    ("name", "校准测试分组"),
                    ("account_kind", kind),
                    ("currency", currency),
                    ("card_number", ""),
                    ("account_username", ""),
                    ("credit_limit", ""),
                    ("billing_day", "1"),
                    ("note", ""),
                    ("transfer_fee_rate", "0"),
                ]),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        self.scalar("SELECT MAX(id) AS value FROM accounts WHERE parent_id IS NULL")
            .await
    }

    async fn fund(&self, group: i64, name: &str) -> i64 {
        let response = self
            .post(
                &format!("/accounts/{group}/funds"),
                &fields(&[
                    ("name", name),
                    ("aliases", ""),
                    ("note", ""),
                    ("transfer_fee_rate", "0"),
                ]),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        self.scalar("SELECT MAX(id) AS value FROM accounts").await
    }

    async fn valuation(&self, path: &str) -> Fields {
        let response = self.get(path).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let html = response.text().await.unwrap();
        let input = Regex::new(r#"name="((?:baseline_|target_)[0-9]+)" value="([^"]*)""#).unwrap();
        input
            .captures_iter(&html)
            .map(|capture| (capture[1].to_owned(), capture[2].to_owned()))
            .collect()
    }

    async fn balance(&self, id: i64) -> String {
        let response = self.get(&format!("/accounts/{id}/balance-summary")).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["balance"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn fund_state(&self) -> Vec<(i64, String, Option<String>)> {
        self.db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, balance_offset, last_calibrated_at FROM accounts WHERE kind = 'investment_fund' ORDER BY id".to_owned(),
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.try_get("", "id").unwrap(),
                    row.try_get("", "balance_offset").unwrap(),
                    row.try_get("", "last_calibrated_at").unwrap(),
                )
            })
            .collect()
    }

    async fn audits(&self) -> i64 {
        self.scalar("SELECT COUNT(*) AS value FROM balance_adjustments")
            .await
    }

    async fn transfer(&self, from: i64, to: i64, amount: &str) {
        let response = self
            .post(
                "/transfers",
                &fields(&[
                    ("from_account_id", &from.to_string()),
                    ("to_account_id", &to.to_string()),
                    ("amount", amount),
                    ("to_amount", ""),
                    ("fee_rate", "0"),
                    ("fee_amount", "0"),
                    ("note", ""),
                    ("happened_at", "2026-10-10T08:00"),
                ]),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn rejects_without_writes(&self, path: &str, form: &Fields, status: StatusCode) {
        let before = self.fund_state().await;
        let audits = self.audits().await;
        assert_eq!(self.post(path, form).await.status(), status);
        assert_eq!(self.fund_state().await, before);
        assert_eq!(self.audits().await, audits);
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn fields(values: &[(&str, &str)]) -> Fields {
    values
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn target(form: &mut Fields, id: i64, value: &str) {
    *form.get_mut(&format!("target_{id}")).unwrap() = value.to_owned();
}

#[tokio::test]
async fn all_funds_are_unpaginated_and_cross_currency_calibration_review_and_zero_are_real() {
    let app = TestApp::start().await;
    let cny = app.account("investment", "CNY").await;
    let usd = app.account("investment", "USD").await;
    let first = app.fund(cny, "人民币基金").await;
    let second = app.fund(usd, "美元基金").await;
    let empty = app.fund(cny, "零持仓基金").await;
    // Clone valid encrypted zero-value rows only in this isolated fixture, beyond every page size.
    app.execute(&format!(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 205) INSERT INTO accounts (name, kind, currency, parent_id, balance_offset, transfer_fee_rate_bps, sms_names, note, created_at) SELECT a.name, a.kind, a.currency, a.parent_id, a.balance_offset, a.transfer_fee_rate_bps, a.sms_names, a.note, a.created_at FROM accounts a CROSS JOIN n WHERE a.id = {empty}"
    ))
    .await;
    let mut form = app.valuation("/funds/valuation?page=2&per_page=50").await;
    let count = app
        .scalar("SELECT COUNT(*) AS value FROM accounts WHERE kind = 'investment_fund'")
        .await;
    assert_eq!(form.len(), usize::try_from(count).unwrap() * 2);
    assert!(form.contains_key(&format!("target_{empty}")));
    target(&mut form, first, "12.34");
    target(&mut form, second, "56.78");
    let response = app.post("/funds/valuation", &form).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let saved_page = response.headers()[header::LOCATION].to_str().unwrap();
    let refreshed = app.valuation(saved_page).await;
    assert_eq!(refreshed[&format!("target_{first}")], "12.34");
    assert_eq!(refreshed[&format!("target_{second}")], "56.78");
    assert_eq!(app.balance(first).await, "CNY 12.34");
    assert_eq!(app.balance(second).await, "USD 56.78");
    assert_eq!(app.balance(empty).await, "CNY 0.00");
    assert_eq!(app.audits().await, 2);
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 0);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
        0
    );

    let group_path = format!("/accounts/{cny}/funds/valuation");
    let group_form = app.valuation(&group_path).await;
    assert!(group_form.contains_key(&format!("target_{first}")));
    assert!(!group_form.contains_key(&format!("target_{second}")));
    app.execute(&format!(
        "UPDATE accounts SET last_calibrated_at = '2000-01-01T00:00:00+00:00' WHERE id = {second}"
    ))
    .await;
    let before = app.fund_state().await;
    let mut reviewed = app.valuation("/funds/valuation").await;
    target(&mut reviewed, second, "");
    reviewed.insert(format!("reviewed_{second}"), "1".into());
    assert_eq!(
        app.post("/funds/valuation", &reviewed).await.status(),
        StatusCode::SEE_OTHER
    );
    let after = app.fund_state().await;
    let old = before.iter().find(|row| row.0 == second).unwrap();
    let new = after.iter().find(|row| row.0 == second).unwrap();
    assert_eq!(
        old.1, new.1,
        "reviewing must not rewrite the encrypted offset"
    );
    assert_ne!(old.2, new.2);
    assert_eq!(app.audits().await, 3);
    let ledger = app.get(&format!("/accounts/{second}")).await;
    assert_eq!(ledger.status(), StatusCode::OK);
    assert!(
        ledger
            .text()
            .await
            .unwrap()
            .contains("USD 56.78 → USD 56.78"),
        "the native account ledger must expose the zero-difference audit"
    );

    let mut cleared = app.valuation("/funds/valuation").await;
    target(&mut cleared, first, "0");
    assert_eq!(
        app.post("/funds/valuation", &cleared).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(first).await, "CNY 0.00");
    assert_eq!(app.audits().await, 4);
    let mut scoped = app.valuation(&group_path).await;
    target(&mut scoped, empty, "1.00");
    let response = app.post(&group_path, &scoped).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION].to_str().unwrap(),
        format!("/accounts/{cny}/funds")
    );
    assert_eq!(app.balance(empty).await, "CNY 1.00");
}

#[tokio::test]
async fn late_conflict_rolls_back_prior_funds_but_untouched_stale_rows_preserve_new_flow() {
    let app = TestApp::start().await;
    let cny = app.account("investment", "CNY").await;
    let other = app.account("investment", "CNY").await;
    let first = app.fund(cny, "先更新").await;
    let second = app.fund(other, "后冲突").await;
    let cash = app.account("other", "CNY").await;
    assert_eq!(
        app.post(
            &format!("/accounts/{cash}/balance"),
            &fields(&[("balance", "100")]),
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let mut original = app.valuation("/funds/valuation").await;
    target(&mut original, first, "100");
    target(&mut original, second, "100");
    assert_eq!(
        app.post("/funds/valuation", &original).await.status(),
        StatusCode::SEE_OTHER
    );
    let stale = app.valuation("/funds/valuation").await;
    app.transfer(cash, second, "10").await;
    let mut conflict = stale.clone();
    target(&mut conflict, first, "101");
    target(&mut conflict, second, "111");
    app.rejects_without_writes("/funds/valuation", &conflict, StatusCode::CONFLICT)
        .await;
    assert_eq!(app.balance(first).await, "CNY 100.00");
    assert_eq!(app.balance(second).await, "CNY 110.00");
    let mut unchanged = stale.clone();
    target(&mut unchanged, first, "101");
    assert_eq!(
        app.post("/funds/valuation", &unchanged).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(first).await, "CNY 101.00");
    assert_eq!(app.balance(second).await, "CNY 110.00");
    assert_eq!(app.audits().await, 4); // cash adjustment, two initial values, first's edit.
    let before = app.fund_state().await;
    let mut blank = stale;
    for value in blank
        .iter_mut()
        .filter_map(|(key, value)| key.starts_with("target_").then_some(value))
    {
        value.clear();
    }
    assert_eq!(
        app.post("/funds/valuation", &blank).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.fund_state().await, before);
    assert_eq!(app.audits().await, 4);
}

#[tokio::test]
async fn invalid_rows_and_group_or_offset_overflow_reject_the_entire_scope() {
    let app = TestApp::start().await;
    let cny = app.account("investment", "CNY").await;
    let usd = app.account("investment", "USD").await;
    let first = app.fund(cny, "首行").await;
    let later = app.fund(usd, "后行").await;
    let same_group = app.fund(cny, "同组边界").await;
    let cash = app.account("other", "CNY").await;
    let original = app.valuation("/funds/valuation").await;
    for invalid in ["-1", "0.001", "92233720368547758.08", "not-money"] {
        let mut form = original.clone();
        target(&mut form, first, "1");
        target(&mut form, later, invalid);
        app.rejects_without_writes("/funds/valuation", &form, StatusCode::BAD_REQUEST)
            .await;
    }
    for key in [
        format!("target_{cny}"),
        format!("baseline_{cash}"),
        "reviewed_9223372036854775808".into(),
        "target_999999999".into(),
        format!("target_0{later}"),
        "forged_field".into(),
    ] {
        let mut form = original.clone();
        target(&mut form, first, "1");
        form.insert(key, "0".into());
        app.rejects_without_writes("/funds/valuation", &form, StatusCode::BAD_REQUEST)
            .await;
    }
    for invalid in ["not-cents", "9223372036854775808"] {
        let mut form = original.clone();
        target(&mut form, first, "1");
        form.insert(format!("baseline_{later}"), invalid.into());
        app.rejects_without_writes("/funds/valuation", &form, StatusCode::BAD_REQUEST)
            .await;
    }
    let mut missing_baseline = original.clone();
    target(&mut missing_baseline, first, "1");
    missing_baseline.remove(&format!("baseline_{later}"));
    app.rejects_without_writes(
        "/funds/valuation",
        &missing_baseline,
        StatusCode::BAD_REQUEST,
    )
    .await;
    app.rejects_without_writes(
        &format!("/accounts/{cny}/funds/valuation"),
        &original,
        StatusCode::BAD_REQUEST,
    )
    .await;
    app.rejects_without_writes(
        "/accounts/999999999/funds/valuation",
        &Fields::new(),
        StatusCode::NOT_FOUND,
    )
    .await;
    app.rejects_without_writes(
        &format!("/accounts/{cash}/funds/valuation"),
        &Fields::new(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let deleted = app.fund(usd, "将被删除").await;
    let mut stale_deleted = app.valuation("/funds/valuation").await;
    assert_eq!(
        app.post(&format!("/funds/{deleted}/delete"), &Fields::new())
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    target(&mut stale_deleted, first, "1");
    app.rejects_without_writes("/funds/valuation", &stale_deleted, StatusCode::BAD_REQUEST)
        .await;

    let mut group_overflow = original.clone();
    target(&mut group_overflow, first, "92233720368547758.07");
    target(&mut group_overflow, same_group, "0.01");
    app.rejects_without_writes("/funds/valuation", &group_overflow, StatusCode::BAD_REQUEST)
        .await;
    let mut two_maxima = original;
    target(&mut two_maxima, first, "92233720368547758.07");
    target(&mut two_maxima, later, "92233720368547758.07");
    assert_eq!(
        app.post("/funds/valuation", &two_maxima).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(first).await, "CNY 92233720368547758.07");
    assert_eq!(app.balance(later).await, "USD 92233720368547758.07");
    let mut scoped_overflow = app
        .valuation(&format!("/accounts/{cny}/funds/valuation"))
        .await;
    target(&mut scoped_overflow, same_group, "0.01");
    app.rejects_without_writes(
        &format!("/accounts/{cny}/funds/valuation"),
        &scoped_overflow,
        StatusCode::BAD_REQUEST,
    )
    .await;
    app.transfer(first, cash, "92233720368547758.07").await;
    assert_eq!(app.balance(first).await, "CNY 0.00");
    let mut offset_overflow = app.valuation("/funds/valuation").await;
    target(&mut offset_overflow, first, "0.01");
    app.rejects_without_writes(
        "/funds/valuation",
        &offset_overflow,
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(app.balance(first).await, "CNY 0.00");
}
