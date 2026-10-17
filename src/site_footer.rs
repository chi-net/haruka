use std::sync::{Arc, LazyLock, OnceLock, RwLock};

use axum::http::StatusCode;
use sea_orm::{sea_query::Expr, ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::{entity::preference, AppState};

/// Deliberately public, plaintext site metadata; never contains decrypted ledger data.
#[derive(Default, Deserialize)]
pub struct FooterConfig {
    pub footer_text: String,
    pub icp_record: String,
}

static CONFIG: OnceLock<RwLock<Arc<FooterConfig>>> = OnceLock::new();
// Templates can also be rendered outside the server startup path in unit tests.
static EMPTY: LazyLock<Arc<FooterConfig>> = LazyLock::new(|| Arc::new(FooterConfig::default()));
// Keep database commit order and cache publication order identical.
static SAVE_LOCK: Mutex<()> = Mutex::const_new(());

pub fn current() -> Arc<FooterConfig> {
    CONFIG
        .get()
        .map(|config| Arc::clone(&config.read().expect("页脚配置锁已损坏")))
        .unwrap_or_else(|| Arc::clone(&EMPTY))
}

pub async fn load(db: &sea_orm::DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    let (footer_text, icp_record) = preference::Entity::find_by_id(1)
        .select_only()
        .columns([
            preference::Column::FooterText,
            preference::Column::IcpRecord,
        ])
        .into_tuple::<(String, String)>()
        .one(db)
        .await?
        .ok_or_else(|| sea_orm::DbErr::Custom("默认设置不存在".into()))?;
    CONFIG
        .set(RwLock::new(Arc::new(FooterConfig {
            footer_text,
            icp_record,
        })))
        .map_err(|_| sea_orm::DbErr::Custom("页脚配置已经初始化".into()))
}

pub async fn save(state: &AppState, mut config: FooterConfig) -> Result<(), (StatusCode, String)> {
    let text = config.footer_text.trim();
    if text.chars().count() > 2000 {
        return Err((
            StatusCode::BAD_REQUEST,
            "页脚内容不能超过 2000 个字符".into(),
        ));
    }
    if text.len() != config.footer_text.len() {
        config.footer_text = text.to_owned();
    }
    let record = config.icp_record.trim();
    if record.chars().count() > 80 {
        return Err((
            StatusCode::BAD_REQUEST,
            "ICP备案号不能超过 80 个字符".into(),
        ));
    }
    if record.len() != config.icp_record.len() {
        config.icp_record = record.to_owned();
    }
    let cache = CONFIG.get().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "页脚配置尚未初始化".into(),
        )
    })?;
    let _guard = SAVE_LOCK.lock().await;
    let result = preference::Entity::update_many()
        .col_expr(
            preference::Column::FooterText,
            Expr::value(config.footer_text.clone()),
        )
        .col_expr(
            preference::Column::IcpRecord,
            Expr::value(config.icp_record.clone()),
        )
        .filter(preference::Column::Id.eq(1))
        .exec(&state.db)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if result.rows_affected != 1 {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "默认设置不存在".into()));
    }
    let config = Arc::new(config);
    *cache.write().expect("页脚配置锁已损坏") = config;
    Ok(())
}
