mod crypto;
mod db;
mod entity;
mod handlers;

use std::sync::{Arc, RwLock};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use sea_orm::{DatabaseConnection, EntityTrait};

use crypto::Dek;
use entity::meta;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    pub dek: Arc<RwLock<Option<Dek>>>,
}

impl AppState {
    pub fn dek(&self) -> Result<Dek, (StatusCode, String)> {
        self.dek
            .read()
            .unwrap()
            .clone()
            .ok_or((StatusCode::UNAUTHORIZED, "未解锁".to_string()))
    }
}

async fn require_unlock(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.dek.read().unwrap().is_none() {
        let has_meta = meta::Entity::find_by_id(1)
            .one(&state.db)
            .await
            .ok()
            .flatten()
            .is_some();
        let target = if has_meta { "/unlock" } else { "/setup" };
        return Redirect::to(target).into_response();
    }
    next.run(req).await
}

#[tokio::main]
async fn main() {
    let db = db::init().await;
    let state = AppState {
        db,
        dek: Arc::new(RwLock::new(None)),
    };

    let protected = Router::new()
        .route("/", get(|| async { Redirect::to("/bills") }))
        .route("/accounts", get(handlers::accounts::list).post(handlers::accounts::create))
        .route("/accounts/new", get(handlers::accounts::new_form))
        .route(
            "/accounts/{id}/edit",
            get(handlers::accounts::edit_form).post(handlers::accounts::update),
        )
        .route("/accounts/{id}/delete", post(handlers::accounts::delete))
        .route("/bills", get(handlers::bills::list).post(handlers::bills::create))
        .route("/bills/new", get(handlers::bills::new_form))
        .route(
            "/bills/{id}/edit",
            get(handlers::bills::edit_form).post(handlers::bills::update),
        )
        .route("/bills/{id}/delete", post(handlers::bills::delete))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_unlock));

    let app = Router::new()
        .route("/setup", get(handlers::auth::setup_form).post(handlers::auth::setup))
        .route("/unlock", get(handlers::auth::unlock_form).post(handlers::auth::unlock))
        .merge(protected)
        .with_state(state);

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("端口绑定失败");
    println!("haruka 已启动: http://{addr}");
    axum::serve(listener, app).await.expect("服务器运行失败");
}
