//! 构建期嵌入的版本号（git 提交计数 + commit id）与远程更新检查。

/// 提交计数，例如 152。
pub const COMMIT_COUNT: &str = env!("HARUKA_GIT_COUNT");
/// 完整 commit id。
pub const COMMIT_SHA: &str = env!("HARUKA_GIT_SHA");

/// 展示用版本号：`v<提交计数>+<短 commit id>`。
pub fn display() -> String {
    let short = if COMMIT_SHA == "unknown" {
        COMMIT_SHA
    } else {
        &COMMIT_SHA[..COMMIT_SHA.len().min(12)]
    };
    format!("v{COMMIT_COUNT}+{short}")
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct UpdateStatus {
    pub current: String,
    /// 落后远程的提交数；0 表示已是最新。
    pub behind_by: Option<u64>,
    pub latest_sha: Option<String>,
    pub latest_message: Option<String>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
}

impl UpdateStatus {
    pub fn unavailable(reason: &str) -> Self {
        Self {
            current: display(),
            behind_by: None,
            latest_sha: None,
            latest_message: None,
            checked_at: None,
            error: Some(reason.into()),
        }
    }
}

const REPO: &str = "chi-net/haruka";
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

static LAST_CHECK: std::sync::LazyLock<
    tokio::sync::Mutex<Option<(std::time::Instant, UpdateStatus)>>,
> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

fn cache() -> &'static tokio::sync::Mutex<Option<(std::time::Instant, UpdateStatus)>> {
    &LAST_CHECK
}

/// 通过 GitHub compare API 比较本地 commit 与远程 main；一小时内的结果直接复用。
pub async fn check_updates(client: &reqwest::Client) -> UpdateStatus {
    if COMMIT_SHA == "unknown" {
        return UpdateStatus::unavailable("当前构建没有 git 版本信息，无法检查更新");
    }
    {
        let guard = cache().lock().await;
        if let Some((at, status)) = guard.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return status.clone();
            }
        }
    }
    let status = fetch_updates(client).await;
    *cache().lock().await = Some((std::time::Instant::now(), status.clone()));
    status
}

async fn fetch_updates(client: &reqwest::Client) -> UpdateStatus {
    let url = format!("https://api.github.com/repos/{REPO}/compare/{COMMIT_SHA}...main");
    let result = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "haruka-update-check")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await;
    let mut status = UpdateStatus {
        current: display(),
        behind_by: None,
        latest_sha: None,
        latest_message: None,
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        error: None,
    };
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            status.error = Some(format!("检查更新网络请求失败：{error}"));
            return status;
        }
    };
    if !response.status().is_success() {
        status.error = Some(format!("检查更新失败：GitHub 返回 {}", response.status()));
        return status;
    }
    let body: serde_json::Value = match response.json().await {
        Ok(body) => body,
        Err(error) => {
            status.error = Some(format!("检查更新响应无法解析：{error}"));
            return status;
        }
    };
    status.behind_by = body.get("behind_by").and_then(|v| v.as_u64());
    if let Some(commits) = body.get("commits").and_then(|v| v.as_array()) {
        if let Some(latest) = commits.last() {
            status.latest_sha = latest
                .get("sha")
                .and_then(|v| v.as_str())
                .map(|sha| sha[..sha.len().min(12)].to_string());
            status.latest_message = latest
                .get("commit")
                .and_then(|commit| commit.get("message"))
                .and_then(|v| v.as_str())
                .map(|message| message.lines().next().unwrap_or_default().to_string());
        }
    }
    if status.behind_by.is_none() {
        status.error = Some("检查更新响应缺少 behind_by 字段".into());
    }
    status
}
