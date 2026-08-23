mod crypto;
mod db;
mod entity;
mod handlers;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sea_orm::{DatabaseConnection, EntityTrait};

use crypto::Dek;
use entity::meta;

const SESSION_COOKIE: &str = "haruka_session";

#[derive(Clone)]
pub struct SessionDek(pub Dek);

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    sessions: Arc<RwLock<HashMap<String, Dek>>>,
    pub balance_writes: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    pub fn create_session(&self, dek: &Dek) -> HeaderValue {
        let token = URL_SAFE_NO_PAD.encode(crypto::random_bytes::<32>());
        self.sessions
            .write()
            .unwrap()
            .insert(token.clone(), dek.clone());
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict"
        ))
        .expect("会话 Cookie 无效")
    }

    fn session_token(headers: &HeaderMap) -> Option<&str> {
        headers
            .get(header::COOKIE)?
            .to_str()
            .ok()?
            .split(';')
            .filter_map(|part| part.trim().split_once('='))
            .find_map(|(name, value)| (name == SESSION_COOKIE).then_some(value))
    }

    fn session_dek(&self, headers: &HeaderMap) -> Option<Dek> {
        let token = Self::session_token(headers)?;
        self.sessions.read().unwrap().get(token).cloned()
    }

    pub fn remove_session(&self, headers: &HeaderMap) {
        if let Some(token) = Self::session_token(headers) {
            self.sessions.write().unwrap().remove(token);
        }
    }

    pub fn clear_sessions(&self) {
        self.sessions.write().unwrap().clear();
    }

    pub fn clear_session_cookie() -> HeaderValue {
        HeaderValue::from_static("haruka_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
    }
}

async fn require_unlock(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if let Some(dek) = state.session_dek(req.headers()) {
        req.extensions_mut().insert(SessionDek(dek));
        return next.run(req).await;
    }

    let has_meta = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .ok()
        .flatten()
        .is_some();
    let target = if has_meta { "/unlock" } else { "/setup" };
    Redirect::to(target).into_response()
}

#[tokio::main]
async fn main() {
    let db = db::init().await;
    let state = AppState {
        db,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        balance_writes: Arc::new(tokio::sync::Mutex::new(())),
    };

    let protected = Router::new()
        .route("/", get(handlers::dashboard::redirect))
        .route("/dashboard", get(handlers::dashboard::show))
        .route(
            "/accounts",
            get(handlers::accounts::list).post(handlers::accounts::create),
        )
        .route("/accounts/new", get(handlers::accounts::new_form))
        .route(
            "/accounts/{id}/edit",
            get(handlers::accounts::edit_form).post(handlers::accounts::update),
        )
        .route(
            "/accounts/{id}/balance",
            get(handlers::accounts::balance_form).post(handlers::accounts::force_balance),
        )
        .route("/accounts/{id}/delete", post(handlers::accounts::delete))
        .route(
            "/bills",
            get(handlers::bills::list).post(handlers::bills::create),
        )
        .route("/bills/new", get(handlers::bills::new_form))
        .route(
            "/bills/{id}/edit",
            get(handlers::bills::edit_form).post(handlers::bills::update),
        )
        .route("/bills/{id}/delete", post(handlers::bills::delete))
        .route(
            "/subscriptions",
            get(handlers::subscriptions::list).post(handlers::subscriptions::create),
        )
        .route("/subscriptions/new", get(handlers::subscriptions::new_form))
        .route(
            "/subscriptions/{id}/edit",
            get(handlers::subscriptions::edit_form).post(handlers::subscriptions::update),
        )
        .route(
            "/subscriptions/{id}/expense",
            post(handlers::subscriptions::create_expense),
        )
        .route(
            "/subscriptions/{id}/delete",
            post(handlers::subscriptions::delete),
        )
        .route(
            "/transfers",
            get(handlers::dashboard::redirect).post(handlers::transfers::create),
        )
        .route("/transfers/{id}/delete", post(handlers::transfers::delete))
        .route(
            "/debts",
            get(handlers::dashboard::redirect).post(handlers::debts::create_record),
        )
        .route("/debts/{id}/delete", post(handlers::debts::delete_record))
        .route(
            "/debt-people",
            get(handlers::debts::people).post(handlers::debts::create_person),
        )
        .route("/debt-people/new", get(handlers::debts::new_person_form))
        .route(
            "/debt-people/{id}/edit",
            get(handlers::debts::edit_person_form).post(handlers::debts::update_person),
        )
        .route(
            "/debt-people/{id}/delete",
            post(handlers::debts::delete_person),
        )
        .route("/settings", get(handlers::settings::show))
        .route(
            "/settings/categories",
            post(handlers::settings::create_category),
        )
        .route(
            "/settings/categories/{id}/edit",
            get(handlers::settings::edit_category_form).post(handlers::settings::update_category),
        )
        .route(
            "/settings/categories/{id}/delete",
            post(handlers::settings::delete_category),
        )
        .route(
            "/settings/recovery",
            post(handlers::auth::generate_recovery),
        )
        .route("/lock", post(handlers::auth::lock))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_unlock,
        ));

    let app = Router::new()
        .route(
            "/setup",
            get(handlers::auth::setup_form).post(handlers::auth::setup),
        )
        .route(
            "/unlock",
            get(handlers::auth::unlock_form).post(handlers::auth::unlock),
        )
        .route(
            "/recover",
            get(handlers::auth::recover_form).post(handlers::auth::recover),
        )
        .merge(protected)
        .with_state(state)
        .layer(middleware::from_fn(handlers::render_server_error));

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("端口绑定失败");
    println!("haruka 已启动: http://{addr}");
    axum::serve(listener, app).await.expect("服务器运行失败");
}
