#![cfg(unix)]

use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, StatusCode};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct App {
    child: Child,
    directory: PathBuf,
    url: String,
}

impl App {
    async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "haruka-shutdown-test-{}-{nonce}-{}",
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
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let url = format!("http://{address}");
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if client.get(format!("{url}/setup")).send().await.is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "server startup failed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            child,
            directory,
            url,
        }
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
async fn sigterm_stops_the_server_cleanly() {
    let mut app = App::start().await;
    let response = Client::new()
        .get(format!("{}/setup", app.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let sent = Command::new("kill")
        .args(["-TERM".to_string(), app.child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success(), "failed to send SIGTERM");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = app.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "server did not shut down gracefully: {status:?}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "server did not stop after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
