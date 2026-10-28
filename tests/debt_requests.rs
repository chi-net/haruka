use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, NaiveDateTime, Utc};
use reqwest::{header, Client, Response, StatusCode};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult, Statement};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// Reuse the actual encryption implementation only for constructing historical
// fixtures. All consumer operations below go through the running HTTP server.
#[allow(dead_code)]
#[path = "../src/crypto.rs"]
mod crypto;

const PASSWORD: &str = "debt-requests-regression-only";
const PAID_AT: &str = "2026-10-10T08:00";
const CARD: &str = "622200001234";

static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

struct App {
    child: Child,
    directory: PathBuf,
    db: DatabaseConnection,
    client: Client,
    url: String,
    cookie: String,
    dek: crypto::Dek,
}

impl App {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "haruka-debt-requests-test-{}-{nonce}-{sequence}",
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
            .timeout(Duration::from_secs(20))
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
            .form(&[("password", PASSWORD), ("confirm", PASSWORD)])
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
        let db = Database::connect(&database_url).await.unwrap();
        let meta = db
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
        let kek = crypto::derive_kek(PASSWORD, &salt);
        let dek = crypto::unwrap_dek(&nonce, &wrapped, kek.as_slice()).unwrap();
        Self {
            child,
            directory,
            db,
            client,
            url,
            cookie,
            dek,
        }
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

    async fn public_post(&self, path: &str, fields: &[(&str, String)]) -> Response {
        self.client
            .post(format!("{}{path}", self.url))
            .form(fields)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str, authenticated: bool) -> Response {
        let request = self.client.get(format!("{}{path}", self.url));
        let request = if authenticated {
            request.header(header::COOKIE, &self.cookie)
        } else {
            request
        };
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

    async fn exec(&self, sql: &str) {
        self.db
            .execute(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap();
    }

    async fn row(&self, id: i64) -> QueryResult {
        self.db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT * FROM debt_requests WHERE id = ?",
                [id.into()],
            ))
            .await
            .unwrap()
            .unwrap()
    }

    async fn counts(&self) -> (i64, i64) {
        (
            self.scalar("SELECT COUNT(*) AS value FROM debt_requests")
                .await,
            self.scalar("SELECT COUNT(*) AS value FROM debt_records")
                .await,
        )
    }

    async fn account(&self, currency: &str) -> i64 {
        let response = self
            .post(
                "/accounts",
                &[
                    ("name", "回归账户".into()),
                    ("account_kind", "bank".into()),
                    ("currency", currency.into()),
                    ("card_number", CARD.into()),
                    ("account_username", String::new()),
                    ("note", String::new()),
                    ("credit_limit", "0".into()),
                    ("billing_day", "1".into()),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        self.scalar("SELECT MAX(id) AS value FROM accounts").await
    }

    async fn person(&self) -> i64 {
        let response = self
            .post(
                "/debt-people",
                &[("name", "回归对象".into()), ("note", String::new())],
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        self.scalar("SELECT MAX(id) AS value FROM debt_people")
            .await
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
            .get(&format!("/accounts/{account}/balance-summary"), true)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().await.unwrap()["balance"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn create(
        &self,
        kind: &str,
        account: i64,
        person: i64,
        amount: &str,
        confirmation: Option<bool>,
        origin: Option<i64>,
    ) -> Response {
        let mut fields = vec![
            ("kind", kind.into()),
            ("account_id", account.to_string()),
            ("person_id", person.to_string()),
            ("amount", amount.into()),
            ("recipient_name", "回归收款户名".into()),
            ("note", "请求回归说明".into()),
            ("paid_at", PAID_AT.into()),
            (
                "expires_at",
                (Utc::now() + chrono::Duration::days(1))
                    .format("%Y-%m-%dT%H:%M")
                    .to_string(),
            ),
        ];
        if let Some(value) = confirmation {
            fields.push(("requires_confirmation", value.to_string()));
        }
        if let Some(value) = origin {
            fields.push(("origin_debt_record_id", value.to_string()));
        }
        self.post("/debt-requests", &fields).await
    }

    fn created_id(response: &Response) -> i64 {
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .strip_prefix("/debt-requests?created=")
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn book(
        &self,
        kind: &str,
        account: i64,
        person: i64,
        amount: &str,
        origin: Option<i64>,
    ) -> (i64, i64) {
        let response = self
            .create(kind, account, person, amount, Some(false), origin)
            .await;
        let id = Self::created_id(&response);
        (id, self.record_id(id).await)
    }

    async fn record_id(&self, id: i64) -> i64 {
        self.row(id)
            .await
            .try_get::<Option<i64>>("", "confirmed_debt_record_id")
            .unwrap()
            .unwrap()
    }

    async fn path(&self, id: i64) -> String {
        let cipher: String = self.row(id).await.try_get("", "token").unwrap();
        format!("/r/{}", crypto::decrypt_string(&self.dek, &cipher))
    }

    async fn submit(&self, id: i64, amount: &str) -> Response {
        self.public_post(
            &self.path(id).await,
            &[
                ("payer_name", "回归对象".into()),
                ("payer_contact", String::new()),
                ("payer_account", "5678".into()),
                ("amount", amount.into()),
                ("note", "对方实际收付声明".into()),
                ("paid_at", PAID_AT.into()),
            ],
        )
        .await
    }

    async fn confirm(&self, id: i64) -> Response {
        self.post(&format!("/debt-requests/{id}/confirm"), &[])
            .await
    }

    async fn settle(&self, id: i64, note: &str) -> Response {
        self.post(
            &format!("/debt-requests/{id}/settle"),
            &[("note", note.into())],
        )
        .await
    }

    async fn historical_record(&self, kind: &str, person: i64, account: i64, cents: i64) -> i64 {
        self.db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
            "INSERT INTO debt_records (person_id, account_id, kind, amount, note, happened_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![person.into(), account.into(), kind.into(), crypto::encrypt_cents(&self.dek, cents).into(),
                crypto::encrypt(&self.dek, b"historical fixture").into(),
                "2026-10-01 08:00:00".into(), "2026-10-01T08:00:00+00:00".into()]))
            .await.unwrap();
        self.scalar("SELECT MAX(id) AS value FROM debt_records")
            .await
    }

    async fn request_key(&self, id: i64) -> crypto::Dek {
        let row = self.row(id).await;
        let salt: Vec<u8> = row.try_get("", "salt").unwrap();
        let cipher: String = row.try_get("", "token").unwrap();
        let token = crypto::decrypt_string(&self.dek, &cipher);
        let bytes = URL_SAFE_NO_PAD.decode(token).unwrap();
        let mut material = b"haruka debt request key v1\0".to_vec();
        material.extend_from_slice(&bytes);
        crypto::derive_key(&material, &salt)
    }

    // Construct the old on-disk format, not a production migration: retain the
    // original v1 digest and old encrypted masking for compatibility assertions.
    async fn make_legacy_open(&self, id: i64) -> String {
        let row = self.row(id).await;
        let key = self.request_key(id).await;
        let snapshot: String = row.try_get("", "snapshot").unwrap();
        let mut snapshot: Value =
            serde_json::from_slice(&crypto::decrypt(&key, &snapshot).unwrap()).unwrap();
        snapshot["version"] = json!(1);
        snapshot["account_name"] = json!("回归账户");
        snapshot["identifier_masked"] = json!("•••• 1234");
        let snapshot = crypto::encrypt(&key, &serde_json::to_vec(&snapshot).unwrap());
        let token_hash: Vec<u8> = row.try_get("", "token_hash").unwrap();
        let account_id: i64 = row.try_get("", "account_id").unwrap();
        let person_id: Option<i64> = row.try_get("", "person_id").unwrap();
        let kind: String = row.try_get("", "kind").unwrap();
        let expires: NaiveDateTime = row.try_get("", "expires_at").unwrap();
        let created: DateTime<Utc> = row.try_get("", "created_at").unwrap();
        let hash = chain(&[
            b"created",
            &token_hash,
            &account_id.to_be_bytes(),
            &person_id.unwrap_or_default().to_be_bytes(),
            kind.as_bytes(),
            snapshot.as_bytes(),
            expires.format("%Y-%m-%dT%H:%M").to_string().as_bytes(),
            created.to_rfc3339().as_bytes(),
        ]);
        self.db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
            "UPDATE debt_requests SET proof_version = 1, requires_confirmation = 1, origin_debt_record_id = NULL, snapshot = ?, request_digest = ? WHERE id = ?",
            vec![snapshot.clone().into(), hash.into(), id.into()])).await.unwrap();
        snapshot
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn chain(parts: &[&[u8]]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(b"haruka debt request verification chain v1\0");
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().to_vec()
}

fn assert_private_proof_policy(response: &Response) {
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::PRAGMA], "no-cache");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(
        response.headers()["x-robots-tag"],
        "noindex, nofollow, noarchive"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let policy = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(policy.contains("default-src 'none'"));
    assert!(policy.contains("form-action 'self'"));
    assert!(policy.contains("frame-ancestors 'none'"));
    let styles = policy
        .split(';')
        .map(str::trim)
        .find(|directive| directive.starts_with("style-src "))
        .unwrap();
    assert!(styles.starts_with("style-src 'self' 'nonce-"));
    assert!(!styles.contains("'unsafe-inline'"));
}

#[tokio::test]
async fn unilateral_four_directions_bind_proof_and_cannot_become_counterparty_confirmation() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let person = app.person().await;
    let (borrow, borrowed) = app.book("borrow", account, person, "100", None).await;
    let (_, lent) = app.book("lend", account, person, "40", None).await;
    app.book("repayment_received", account, person, "15", Some(lent))
        .await;
    app.book("repayment_paid", account, person, "25", Some(borrowed))
        .await;
    assert_eq!(app.balance(account).await, "CNY 50.00");
    assert_eq!(app.counts().await, (4, 4));
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM debt_requests WHERE proof_version = 2 AND requires_confirmation = 0 AND status = 'recorded' AND confirmed_debt_record_id IS NOT NULL AND submission <> '' AND length(confirmation_digest) = 32 AND verified_at IS NULL").await, 4);
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM debt_records d LEFT JOIN debt_requests r ON r.confirmed_debt_record_id = d.id WHERE r.id IS NULL").await, 0);
    let path = app.path(borrow).await;
    let receipt = app
        .client
        .get(format!("{}{path}", app.url))
        .header(header::COOKIE, "haruka_time_zone=Asia%2FShanghai")
        .send()
        .await
        .unwrap();
    assert_private_proof_policy(&receipt);
    let receipt = receipt.text().await.unwrap();
    assert!(receipt.contains("单方记录，未经对方确认或认可"));
    assert!(receipt.contains("Asia/Shanghai"));
    assert!(receipt.contains("2026-10-10 08:00:00 UTC"));
    assert!(receipt.contains("2026-10-10 16:00:00 +08:00"));
    assert!(receipt.contains("CNY 100.00"));
    assert!(receipt.contains("无。此为账主单方记录"));
    assert!(receipt.contains("未经对方确认"));
    assert!(!receipt.contains("/verify\""));
    assert!(!receipt.contains(CARD));
    assert_eq!(app.confirm(borrow).await.status(), StatusCode::BAD_REQUEST);
    assert!(app
        .submit(borrow, "999")
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert!(app
        .public_post(&format!("{path}/verify"), &[])
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    for endpoint in [
        format!("/debts/{borrowed}/delete"),
        format!("/accounts/{account}/delete"),
        format!("/debt-people/{person}/delete"),
        format!("/debt-requests/{borrow}/delete"),
    ] {
        assert_eq!(
            app.post(&endpoint, &[]).await.status(),
            StatusCode::BAD_REQUEST,
            "{endpoint}"
        );
    }
    assert_eq!(
        app.post("/debts", &[("kind", "borrow".into())])
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(app.counts().await, (4, 4));
}

#[tokio::test]
async fn confirmation_waits_for_owner_and_public_submission_and_owner_confirmation_are_idempotent()
{
    let app = App::start().await;
    let account = app.account("CNY").await;
    // An omitted policy preserves the old default, including creating the person
    // only after the owner has verified the counterpart's submission.
    let response = app.create("borrow", account, 0, "100", None, None).await;
    let borrow = App::created_id(&response);
    assert_eq!(app.counts().await, (1, 0));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM debt_people")
            .await,
        0
    );
    assert_eq!(app.confirm(borrow).await.status(), StatusCode::BAD_REQUEST);
    let path = app.path(borrow).await;
    let public = app.get(&path, false).await.text().await.unwrap();
    assert!(public.contains("回归账户(1234)"));
    assert!(!public.contains(CARD));
    assert!(public.contains("未提交的借还请求（不是正式证明）"));
    let identifier = app.public_post(&format!("{path}/account"), &[]).await;
    assert_eq!(identifier.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(identifier.json::<Value>().await.unwrap()["value"], CARD);
    let submitted_receipt = app.submit(borrow, "100").await;
    assert_eq!(submitted_receipt.status(), StatusCode::OK);
    assert_private_proof_policy(&submitted_receipt);
    let submitted_receipt = submitted_receipt.text().await.unwrap();
    assert!(submitted_receipt.contains("尚未核实入账"));
    assert!(submitted_receipt.contains("账主尚未核实"));
    let submitted: Vec<u8> = app
        .row(borrow)
        .await
        .try_get("", "submission_digest")
        .unwrap();
    assert_eq!(
        app.submit(borrow, "999").await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.row(borrow)
            .await
            .try_get::<Vec<u8>>("", "submission_digest")
            .unwrap(),
        submitted
    );
    assert_eq!(app.counts().await, (1, 0));
    assert_eq!(app.balance(account).await, "CNY 0.00");
    let (first, second) = tokio::join!(app.confirm(borrow), app.confirm(borrow));
    assert_eq!(first.status(), StatusCode::SEE_OTHER);
    assert_eq!(second.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.counts().await, (1, 1));
    assert_eq!(app.balance(account).await, "CNY 100.00");
    let confirmed_receipt = app.get(&path, false).await;
    assert_private_proof_policy(&confirmed_receipt);
    let confirmed_receipt = confirmed_receipt.text().await.unwrap();
    assert!(confirmed_receipt.contains("账主已核实到账并入账"));
    assert!(confirmed_receipt.contains("CNY 100.00"));
    assert!(confirmed_receipt.contains("对方提交 / 确认收付"));
    assert!(confirmed_receipt.contains("账主核实入账"));
    let borrowed = app.record_id(borrow).await;
    let person = app
        .scalar(&format!(
            "SELECT person_id AS value FROM debt_records WHERE id = {borrowed}"
        ))
        .await;
    let mut lent = 0;
    for (kind, amount, origin, expected) in [
        ("lend", "40", None, "CNY 60.00"),
        ("repayment_paid", "25", Some(borrowed), "CNY 35.00"),
    ] {
        let response = app
            .create(kind, account, person, amount, Some(true), origin)
            .await;
        let id = App::created_id(&response);
        let payout_path = app.path(id).await;
        assert_eq!(
            app.public_post(&format!("{payout_path}/account"), &[])
                .await
                .json::<Value>()
                .await
                .unwrap()["ok"],
            false
        );
        let payout_snapshot: String = app.row(id).await.try_get("", "snapshot").unwrap();
        let payout_key = app.request_key(id).await;
        let payout_snapshot: Value =
            serde_json::from_slice(&crypto::decrypt(&payout_key, &payout_snapshot).unwrap())
                .unwrap();
        assert_eq!(payout_snapshot["identifier"], "");
        let before = app.balance(account).await;
        assert_eq!(app.submit(id, amount).await.status(), StatusCode::OK);
        assert_eq!(app.balance(account).await, before);
        assert_eq!(app.confirm(id).await.status(), StatusCode::SEE_OTHER);
        assert_eq!(app.confirm(id).await.status(), StatusCode::SEE_OTHER);
        assert_eq!(app.balance(account).await, expected);
        if kind == "lend" {
            lent = app.record_id(id).await;
        }
    }
    let response = app
        .create(
            "repayment_received",
            account,
            person,
            "15",
            Some(true),
            Some(lent),
        )
        .await;
    let repayment = App::created_id(&response);
    assert_eq!(app.submit(repayment, "15").await.status(), StatusCode::OK);
    assert_eq!(app.balance(account).await, "CNY 35.00");
    assert_eq!(app.confirm(repayment).await.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.balance(account).await, "CNY 50.00");
    assert_eq!(app.counts().await, (4, 4));
    // New repayments use /r POST followed by owner /confirm, never a public
    // funds-writing /verify shortcut.
    assert!(app
        .public_post(&format!("{}/verify", app.path(repayment).await), &[])
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM debt_requests WHERE verified_at IS NOT NULL")
            .await,
        0
    );
    let response = app
        .create("borrow", account, person, "1", Some(true), None)
        .await;
    let revoked = App::created_id(&response);
    assert_eq!(
        app.post(&format!("/debt-requests/{revoked}/revoke"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let revoked_path = app.path(revoked).await;
    let unavailable = app.get(&revoked_path, false).await;
    assert_eq!(unavailable.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(unavailable.headers()["referrer-policy"], "no-referrer");
    let unavailable = unavailable.text().await.unwrap();
    assert!(unavailable.contains("此分享不可用"));
    assert_eq!(
        app.public_post(&format!("{revoked_path}/account"), &[])
            .await
            .json::<Value>()
            .await
            .unwrap()["ok"],
        false
    );
    let response = app
        .create("borrow", account, person, "1", Some(true), None)
        .await;
    let expired = App::created_id(&response);
    app.exec(&format!(
        "UPDATE debt_requests SET expires_at = '2000-01-01 00:00:00' WHERE id = {expired}"
    ))
    .await;
    assert!(app
        .submit(expired, "1")
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM debt_records")
            .await,
        4
    );
}

#[tokio::test]
async fn repayments_enforce_original_direction_person_currency_and_concurrent_remaining_caps() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let usd = app.account("USD").await;
    let person = app.person().await;
    let stranger = app.person().await;
    let (borrow, origin) = app.book("borrow", account, person, "40", None).await;
    let (_, other_origin) = app.book("borrow", account, person, "80", None).await;
    let (_, usd_origin) = app.book("borrow", usd, person, "100", None).await;
    let (_, lend) = app.book("lend", account, person, "30", None).await;
    let before = app.counts().await;
    for (kind, target, who, amount, original) in [
        ("repayment_paid", account, person, "1", None),
        ("repayment_paid", account, person, "1", Some(999999)),
        ("repayment_paid", account, stranger, "1", Some(origin)),
        ("repayment_paid", account, person, "1", Some(lend)),
        ("repayment_received", account, person, "1", Some(origin)),
        ("repayment_paid", account, person, "1", Some(usd_origin)),
        ("repayment_paid", usd, person, "1", Some(origin)),
        ("repayment_paid", account, person, "40.01", Some(origin)),
        ("borrow", account, person, "1", Some(origin)),
    ] {
        assert_eq!(
            app.create(kind, target, who, amount, Some(false), original)
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{kind} {target} {who} {original:?}"
        );
    }
    assert_eq!(app.counts().await, before);
    // The old shortcut now produces a separate, origin-bound partial request.
    let response = app
        .post(
            &format!("/debt-requests/{borrow}/repayment"),
            &[
                ("account_id", account.to_string()),
                ("amount", "25".into()),
                ("repaid_at", PAID_AT.into()),
                ("note", String::new()),
                ("requires_confirmation", "false".into()),
            ],
        )
        .await;
    let partial = App::created_id(&response);
    assert_eq!(
        app.row(partial)
            .await
            .try_get::<Option<i64>>("", "origin_debt_record_id")
            .unwrap(),
        Some(origin)
    );
    let mut competing = Vec::new();
    for _ in 0..2 {
        let response = app
            .create(
                "repayment_paid",
                account,
                person,
                "15",
                Some(true),
                Some(origin),
            )
            .await;
        let id = App::created_id(&response);
        assert_eq!(app.submit(id, "15").await.status(), StatusCode::OK);
        competing.push(id);
    }
    let (a, b) = tokio::join!(app.confirm(competing[0]), app.confirm(competing[1]));
    let statuses = [a.status(), b.status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|&&status| status == StatusCode::SEE_OTHER)
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
    assert_eq!(app.scalar(&format!("SELECT COUNT(*) AS value FROM debt_requests WHERE origin_debt_record_id = {origin} AND confirmed_debt_record_id IS NOT NULL")).await, 2);
    assert_eq!(
        app.create(
            "repayment_paid",
            account,
            person,
            "0.01",
            Some(false),
            Some(origin)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    // Legacy unbound repayments also reduce the person+currency cap. Foreign
    // currency loans and outstanding lend obligations cannot offset that cap.
    app.historical_record("repayment_paid", person, account, 7000)
        .await;
    assert_eq!(
        app.create(
            "repayment_paid",
            account,
            person,
            "10.01",
            Some(false),
            Some(other_origin)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn policy_and_origin_tampering_and_insufficient_funds_cannot_leave_unbound_money() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let person = app.person().await;
    assert_eq!(
        app.create("lend", account, person, "10", Some(false), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (0, 0));
    let response = app
        .create("lend", account, person, "10", Some(true), None)
        .await;
    let lend = App::created_id(&response);
    assert_eq!(app.submit(lend, "10").await.status(), StatusCode::OK);
    assert_eq!(app.confirm(lend).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.counts().await, (1, 0));
    assert_eq!(
        app.row(lend).await.try_get::<String>("", "status").unwrap(),
        "submitted"
    );
    assert_eq!(app.balance(account).await, "CNY 0.00");
    app.exec(&format!(
        "UPDATE debt_requests SET requires_confirmation = 0 WHERE id = {lend}"
    ))
    .await;
    assert!(app
        .get(&app.path(lend).await, false)
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert_eq!(app.confirm(lend).await.status(), StatusCode::BAD_REQUEST);
    app.exec(&format!(
        "UPDATE debt_requests SET requires_confirmation = 1, proof_version = 1 WHERE id = {lend}"
    ))
    .await;
    assert_eq!(app.confirm(lend).await.status(), StatusCode::BAD_REQUEST);
    app.exec(&format!(
        "UPDATE debt_requests SET proof_version = 2 WHERE id = {lend}"
    ))
    .await;
    let (_, original) = app.book("borrow", account, person, "100", None).await;
    let (_, other) = app.book("borrow", account, person, "100", None).await;
    let response = app
        .create(
            "repayment_paid",
            account,
            person,
            "10",
            Some(true),
            Some(original),
        )
        .await;
    let repayment = App::created_id(&response);
    assert_eq!(app.submit(repayment, "10").await.status(), StatusCode::OK);
    app.exec(&format!(
        "UPDATE debt_requests SET origin_debt_record_id = {other} WHERE id = {repayment}"
    ))
    .await;
    assert_eq!(
        app.confirm(repayment).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(app
        .get(&app.path(repayment).await, false)
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    app.exec(&format!(
        "UPDATE debt_requests SET origin_debt_record_id = {original} WHERE id = {repayment}"
    ))
    .await;
    assert_eq!(app.confirm(repayment).await.status(), StatusCode::SEE_OTHER);
    let before = app.counts().await;
    // Failure after inserting the debt record still rolls back the entire
    // unilateral request+submission+record transaction.
    app.exec("CREATE TRIGGER reject_debt_request BEFORE INSERT ON debt_requests BEGIN SELECT RAISE(ABORT, 'regression request failure'); END").await;
    assert_eq!(
        app.create("borrow", account, person, "1", Some(false), None)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    app.exec("DROP TRIGGER reject_debt_request").await;
    assert_eq!(app.counts().await, before);
    assert_eq!(app.balance(account).await, "CNY 190.00");
}

#[tokio::test]
async fn legacy_digests_and_historical_public_verification_remain_readable_without_new_money() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let person = app.person().await;
    let response = app
        .create("borrow", account, person, "100", Some(true), None)
        .await;
    let borrow = App::created_id(&response);
    let old_cipher = app.make_legacy_open(borrow).await;
    let path = app.path(borrow).await;
    let page = app.get(&path, false).await.text().await.unwrap();
    assert!(page.contains("回归账户(1234)"));
    assert!(!page.contains("••••"));
    assert!(!page.contains(CARD));
    assert_eq!(
        app.row(borrow)
            .await
            .try_get::<String>("", "snapshot")
            .unwrap(),
        old_cipher
    );
    assert_eq!(app.submit(borrow, "100").await.status(), StatusCode::OK);
    assert_eq!(app.confirm(borrow).await.status(), StatusCode::SEE_OTHER);
    assert_eq!(app.confirm(borrow).await.status(), StatusCode::SEE_OTHER);
    let response = app
        .post(
            &format!("/debt-requests/{borrow}/repayment"),
            &[
                ("account_id", account.to_string()),
                ("amount", "25".into()),
                ("repaid_at", PAID_AT.into()),
                ("note", String::new()),
            ],
        )
        .await;
    let child = App::created_id(&response);
    assert!(app
        .row(child)
        .await
        .try_get::<bool>("", "requires_confirmation")
        .unwrap());
    assert_eq!(
        app.row(child)
            .await
            .try_get::<String>("", "status")
            .unwrap(),
        "open"
    );
    assert_eq!(
        app.row(child)
            .await
            .try_get::<Option<i64>>("", "origin_debt_record_id")
            .unwrap(),
        Some(app.record_id(borrow).await)
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM debt_records")
            .await,
        1
    );
    let repayment_record = app
        .historical_record("repayment_paid", person, account, 10000)
        .await;
    let key = app.request_key(borrow).await;
    let repayment = crypto::encrypt(
        &key,
        &serde_json::to_vec(&json!({
            "version": 1, "amount_cents": 10000, "repaid_at": PAID_AT, "note": "历史还款"
        }))
        .unwrap(),
    );
    let confirmed: Vec<u8> = app
        .row(borrow)
        .await
        .try_get("", "confirmation_digest")
        .unwrap();
    let submitted_at = Utc::now();
    let hash = chain(&[
        b"repayment-submitted",
        &confirmed,
        repayment.as_bytes(),
        &account.to_be_bytes(),
        &repayment_record.to_be_bytes(),
        submitted_at.to_rfc3339().as_bytes(),
    ]);
    app.db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "UPDATE debt_requests SET status = 'repayment_submitted', repayment_submission = ?, repayment_digest = ?, repayment_submitted_at = ?, repayment_account_id = ?, repayment_debt_record_id = ? WHERE id = ?",
        vec![repayment.into(), hash.into(), submitted_at.into(), account.into(), repayment_record.into(), borrow.into()]))
        .await.unwrap();
    let before = app.counts().await;
    let pending = app.get(&path, false).await;
    assert_private_proof_policy(&pending);
    let pending = pending.text().await.unwrap();
    assert!(pending.contains("等待对方确认"));
    assert!(pending.contains("不能视为对方已认可"));
    assert!(pending.contains("历史还款金额"));
    assert!(pending.contains("CNY 100.00"));
    assert_eq!(
        app.public_post(&format!("{path}/verify"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.public_post(&format!("{path}/verify"), &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let repaid = app.get(&path, false).await;
    assert_private_proof_policy(&repaid);
    let repaid = repaid.text().await.unwrap();
    assert!(repaid.contains("历史还款已由对方确认收到"));
    assert!(repaid.contains("对方确认收到历史还款"));
    assert_eq!(app.counts().await, before);
    assert_eq!(app.balance(account).await, "CNY 0.00");
    assert_eq!(
        app.row(borrow)
            .await
            .try_get::<String>("", "snapshot")
            .unwrap(),
        old_cipher
    );
    // Old standalone repayment requests have no origin ID. Preserve their
    // historical confirmation policy without fabricating an origin or signature.
    app.force_balance(account, "100").await;
    let old_lend = app.historical_record("lend", person, account, 5000).await;
    let response = app
        .create(
            "repayment_received",
            account,
            person,
            "10",
            Some(true),
            Some(old_lend),
        )
        .await;
    let old_repayment = App::created_id(&response);
    app.make_legacy_open(old_repayment).await;
    assert_eq!(
        app.submit(old_repayment, "10").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.confirm(old_repayment).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.confirm(old_repayment).await.status(),
        StatusCode::SEE_OTHER
    );
    assert!(app
        .row(old_repayment)
        .await
        .try_get::<Option<i64>>("", "origin_debt_record_id")
        .unwrap()
        .is_none());
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM debt_requests WHERE proof_version = 1")
            .await,
        2
    );
}

#[tokio::test]
async fn forced_settlement_closes_remaining_debt_without_faking_money_or_counterparty_consent() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let person = app.person().await;
    app.force_balance(account, "100").await;
    let (borrow, borrowed) = app.book("borrow", account, person, "100", None).await;
    let (paid, _) = app
        .book("repayment_paid", account, person, "40", Some(borrowed))
        .await;
    let (lend, lent) = app.book("lend", account, person, "50", None).await;
    app.book("repayment_received", account, person, "10", Some(lent))
        .await;
    let draft = App::created_id(
        &app.create(
            "repayment_paid",
            account,
            person,
            "1",
            Some(true),
            Some(borrowed),
        )
        .await,
    );
    assert_eq!(app.submit(draft, "1").await.status(), StatusCode::OK);
    let before = app.counts().await;
    let balance = app.balance(account).await;
    assert!(app
        .get("/debt-people/search?relationship=both", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    let unauthorized = app
        .public_post(&format!("/debt-requests/{borrow}/settle"), &[])
        .await;
    assert_eq!(unauthorized.status(), StatusCode::SEE_OTHER);
    assert_eq!(unauthorized.headers()[header::LOCATION], "/unlock");
    assert_eq!(app.settle(paid, "").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app.settle(draft, "").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.settle(borrow, &"长".repeat(1001)).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.settle(borrow, "  不再偿还的单方说明  ").await.status(),
        StatusCode::SEE_OTHER
    );
    let closed = app.row(borrow).await;
    let settled_amount: String = closed.try_get("", "settled_amount").unwrap();
    assert_eq!(crypto::decrypt_cents(&app.dek, &settled_amount), 6000);
    let settlement: String = closed.try_get("", "settlement").unwrap();
    let key = app.request_key(borrow).await;
    let declaration: Value =
        serde_json::from_slice(&crypto::decrypt(&key, &settlement).unwrap()).unwrap();
    assert_eq!(declaration["amount_cents"], 6000);
    assert_eq!(declaration["note"], "不再偿还的单方说明");
    let fingerprint: Vec<u8> = closed.try_get("", "settlement_digest").unwrap();
    let at: Option<DateTime<Utc>> = closed.try_get("", "settled_at").unwrap();
    assert!(at.is_some());
    assert_eq!(
        app.settle(borrow, "重试不应覆盖声明").await.status(),
        StatusCode::SEE_OTHER
    );
    let retry = app.row(borrow).await;
    assert_eq!(
        retry.try_get::<Vec<u8>>("", "settlement_digest").unwrap(),
        fingerprint
    );
    assert_eq!(
        retry
            .try_get::<Option<DateTime<Utc>>>("", "settled_at")
            .unwrap(),
        at
    );
    assert_eq!(
        retry.try_get::<String>("", "settlement").unwrap(),
        settlement
    );
    assert_eq!(app.counts().await, before);
    assert_eq!(app.balance(account).await, balance);
    assert_eq!(app.scalar("SELECT COUNT(*) AS value FROM bills").await, 0);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM transfers").await,
        0
    );
    assert!(!app
        .get("/debt-people/search?relationship=payable", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    assert!(app
        .get("/debt-people/search?relationship=receivable", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    let proof = app.get(&app.path(borrow).await, false).await;
    assert_private_proof_policy(&proof);
    let proof = proof.text().await.unwrap();
    assert!(proof.contains("CNY 60.00"));
    assert!(proof.contains("不再偿还的单方说明"));
    assert!(proof.contains("单方强制平账"));
    assert!(proof.contains("不代表对方已还款"));
    assert_eq!(app.confirm(draft).await.status(), StatusCode::BAD_REQUEST);
    assert!(app
        .get(&app.path(draft).await, false)
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert!(app
        .submit(draft, "1")
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert_eq!(
        app.create(
            "repayment_paid",
            account,
            person,
            "0.01",
            Some(false),
            Some(borrowed)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.settle(lend, "对方不再归还").await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        crypto::decrypt_cents(
            &app.dek,
            &app.row(lend)
                .await
                .try_get::<String>("", "settled_amount")
                .unwrap()
        ),
        4000
    );
    assert!(app
        .get("/debt-people/search?relationship=settled", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    assert_eq!(app.balance(account).await, balance);
    assert_eq!(app.counts().await, before);
    assert_eq!(
        app.post(&format!("/debt-requests/{borrow}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post(&format!("/debts/{borrowed}/delete"), &[])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    // Closed loans cannot absorb new debt or re-enable repayment against old origins.
    let (new_loan, new_origin) = app.book("borrow", account, person, "3", None).await;
    assert_eq!(
        app.create(
            "repayment_paid",
            account,
            person,
            "3.01",
            Some(false),
            Some(new_origin)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    app.book("repayment_paid", account, person, "3", Some(new_origin))
        .await;
    // 还款流水已全额抵消的借款允许以零金额单方结清，只追加终结标记，
    // 不重复扣减对象净额；零金额结清同样封死后续还款。
    assert_eq!(
        app.settle(new_loan, "").await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        crypto::decrypt_cents(
            &app.dek,
            &app
                .row(new_loan)
                .await
                .try_get::<String>("", "settled_amount")
                .unwrap()
        ),
        0
    );
    assert_eq!(
        app.create(
            "repayment_paid",
            account,
            person,
            "0.01",
            Some(false),
            Some(new_origin)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(app
        .get("/debt-people/search?relationship=settled", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    // A changed ciphertext invalidates the appended proof instead of silently
    // re-opening the debt or displaying a forged closure.
    app.exec(&format!(
        "UPDATE debt_requests SET settled_amount = settled_amount || 'x' WHERE id = {borrow}"
    ))
    .await;
    assert!(app
        .get(&app.path(borrow).await, false)
        .await
        .text()
        .await
        .unwrap()
        .contains("此分享不可用"));
    assert_eq!(
        app.get("/debt-people", true).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn forced_settlement_serializes_with_repayment_and_rolls_back_failed_storage() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let person = app.person().await;
    let (borrow, origin) = app.book("borrow", account, person, "100", None).await;
    app.exec("CREATE TRIGGER reject_settlement BEFORE UPDATE OF settled_at ON debt_requests WHEN NEW.settled_at IS NOT NULL BEGIN SELECT RAISE(ABORT, 'blocked closure'); END").await;
    assert_eq!(
        app.settle(borrow, "写入失败不能假结清").await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let failed = app.row(borrow).await;
    assert!(failed
        .try_get::<Option<DateTime<Utc>>>("", "settled_at")
        .unwrap()
        .is_none());
    assert!(failed
        .try_get::<String>("", "settled_amount")
        .unwrap()
        .is_empty());
    assert_eq!(app.balance(account).await, "CNY 100.00");
    app.exec("DROP TRIGGER reject_settlement").await;
    let repayment = App::created_id(
        &app.create(
            "repayment_paid",
            account,
            person,
            "30",
            Some(true),
            Some(origin),
        )
        .await,
    );
    assert_eq!(app.submit(repayment, "30").await.status(), StatusCode::OK);
    let (closure, confirmation) =
        tokio::join!(app.settle(borrow, "并发结清"), app.confirm(repayment));
    assert_eq!(closure.status(), StatusCode::SEE_OTHER);
    let (waived, balance, records) = match confirmation.status() {
        StatusCode::SEE_OTHER => (7000, "CNY 70.00", 2),
        StatusCode::BAD_REQUEST => (10000, "CNY 100.00", 1),
        other => panic!("unexpected concurrent confirmation status: {other}"),
    };
    let closed = app.row(borrow).await;
    assert_eq!(
        crypto::decrypt_cents(
            &app.dek,
            &closed.try_get::<String>("", "settled_amount").unwrap()
        ),
        waived
    );
    assert_eq!(app.balance(account).await, balance);
    assert_eq!(
        app.scalar("SELECT COUNT(*) AS value FROM debt_records")
            .await,
        records
    );
    assert!(app
        .get("/debt-people/search?relationship=settled", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    assert_eq!(app.confirm(repayment).await.status(), confirmation.status());
}

#[tokio::test]
async fn foreign_currency_settlement_nets_before_conversion_without_rounding_debt() {
    let app = App::start().await;
    let account = app.account("USD").await;
    let person = app.person().await;
    let today = chrono::Local::now().date_naive();
    app.db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "INSERT INTO exchange_rates (id, requested_date, rate_date, base_currency, quote_currency, rate, fetched_at) VALUES (?, ?, ?, 'USD', 'CNY', '1.5', ?)",
        vec![format!("{today}:USD:CNY").into(), today.into(), today.into(), Utc::now().into()]))
        .await.unwrap();
    let (loan, origin) = app.book("borrow", account, person, "0.03", None).await;
    app.book("repayment_paid", account, person, "0.01", Some(origin))
        .await;
    app.book("repayment_paid", account, person, "0.01", Some(origin))
        .await;
    assert_eq!(
        app.settle(loan, "剩余一美分").await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.balance(account).await, "USD 0.01");
    assert!(app
        .get("/debt-people/search?relationship=settled", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    assert!(!app
        .get("/debt-people/search?min_payable=0.01", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
}

#[tokio::test]
async fn forced_settlement_marks_zero_remaining_loan_closed() {
    let app = App::start().await;
    let account = app.account("CNY").await;
    let person = app.person().await;
    app.force_balance(account, "400").await;
    // 场景一：原借款已被关联还款全额入账抵消（用户在系统外另行归还）
    let (borrow, borrowed) = app.book("borrow", account, person, "308.08", None).await;
    app.book("repayment_paid", account, person, "308.08", Some(borrowed))
        .await;
    let response = app.settle(borrow, "已另行归还，单方结清").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let closed = app.row(borrow).await;
    assert_eq!(
        crypto::decrypt_cents(
            &app.dek,
            &closed.try_get::<String>("", "settled_amount").unwrap()
        ),
        0
    );
    assert!(closed
        .try_get::<Option<DateTime<Utc>>>("", "settled_at")
        .unwrap()
        .is_some());
    // 结清金额为零，不重复扣减对象净额
    assert!(!app
        .get("/debt-people/search?min_payable=0.01", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    assert!(app
        .get("/debt-people/search?relationship=settled", true)
        .await
        .text()
        .await
        .unwrap()
        .contains("回归对象"));
    // 重复平账幂等
    assert_eq!(
        app.settle(borrow, "重复不应覆盖").await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        crypto::decrypt_cents(
            &app.dek,
            &app.row(borrow)
                .await
                .try_get::<String>("", "settled_amount")
                .unwrap()
        ),
        0
    );
    // 结清后禁止继续对本笔还款
    assert_eq!(
        app.create("repayment_paid", account, person, "0.01", Some(false), Some(borrowed))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    // 场景二：对象级净额被历史遗留的无关联还款流水压平，本笔剩余同样允许平账
    let (borrow2, _) = app.book("borrow", account, person, "50", None).await;
    app.historical_record("repayment_paid", person, account, 5000)
        .await;
    let response = app.settle(borrow2, "净额已平").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        crypto::decrypt_cents(
            &app.dek,
            &app.row(borrow2)
                .await
                .try_get::<String>("", "settled_amount")
                .unwrap()
        ),
        0
    );
}
