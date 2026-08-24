mod crypto;
mod currency;
mod db;
mod entity;
mod handlers;

use std::{
    collections::HashMap,
    env, process,
    sync::{Arc, RwLock},
    time::Instant,
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
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, Url, Webauthn, WebauthnBuilder,
};

const SESSION_COOKIE: &str = "haruka_session";
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:3000";

fn port_addr(value: &str, source: &str) -> Result<String, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| format!("{source} 必须是 1 到 65535 之间的端口号"))?;
    if port == 0 {
        return Err(format!("{source} 不能为 0"));
    }
    Ok(format!("0.0.0.0:{port}"))
}

fn required_arg(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{option} 后面缺少值"))
}

fn print_usage() {
    println!(
        "haruka\n\n用法：\n  haruka [--listen <地址:端口> | --port <端口>]\n\n参数：\n  --listen <地址:端口>  精确指定监听地址，例如 127.0.0.1:3000\n  --port <端口>         监听 0.0.0.0:<端口>\n  -h, --help            显示帮助\n\n环境变量：\n  LISTEN_ADDR           精确指定监听地址\n  PORT                  监听 0.0.0.0:<端口>\n\n优先级：命令行参数 > LISTEN_ADDR > PORT > 127.0.0.1:3000"
    );
}

fn resolve_listen_addr() -> Result<String, String> {
    let mut listen = None;
    let mut port = None;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = Some(required_arg(&mut args, "--listen")?),
            "--port" => port = Some(required_arg(&mut args, "--port")?),
            "-h" | "--help" => {
                print_usage();
                process::exit(0);
            }
            _ if arg.starts_with("--listen=") => {
                let value = arg.trim_start_matches("--listen=").trim();
                if value.is_empty() {
                    return Err("--listen 后面缺少值".to_string());
                }
                listen = Some(value.to_string());
            }
            _ if arg.starts_with("--port=") => {
                let value = arg.trim_start_matches("--port=").trim();
                if value.is_empty() {
                    return Err("--port 后面缺少值".to_string());
                }
                port = Some(value.to_string());
            }
            _ => return Err(format!("未知参数：{arg}")),
        }
    }

    match (listen, port) {
        (Some(_), Some(_)) => Err("--listen 和 --port 不能同时使用".to_string()),
        (Some(addr), None) => Ok(addr),
        (None, Some(value)) => port_addr(&value, "--port"),
        (None, None) => match env::var("LISTEN_ADDR") {
            Ok(addr) if addr.trim().is_empty() => Err("LISTEN_ADDR 不能为空".to_string()),
            Ok(addr) => Ok(addr),
            Err(env::VarError::NotPresent) => match env::var("PORT") {
                Ok(value) => port_addr(value.trim(), "PORT"),
                Err(env::VarError::NotPresent) => Ok(DEFAULT_LISTEN_ADDR.to_string()),
                Err(env::VarError::NotUnicode(_)) => Err("PORT 不是有效文本".to_string()),
            },
            Err(env::VarError::NotUnicode(_)) => Err("LISTEN_ADDR 不是有效文本".to_string()),
        },
    }
}

fn default_passkey_origin(addr: &str) -> String {
    let port = addr
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<u16>().ok())
        .unwrap_or(3000);
    format!("http://localhost:{port}")
}

#[derive(Clone)]
pub struct SessionDek(pub Dek);

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    sessions: Arc<RwLock<HashMap<String, Dek>>>,
    pub balance_writes: Arc<tokio::sync::Mutex<()>>,
    fx_client: reqwest::Client,
    fx_fetches: Arc<tokio::sync::Mutex<()>>,
    webauthn: Arc<Webauthn>,
    passkey_registrations:
        Arc<tokio::sync::Mutex<HashMap<String, (Instant, PasskeyRegistration, String)>>>,
    passkey_enrollments:
        Arc<tokio::sync::Mutex<HashMap<String, (Instant, PasskeyAuthentication, Passkey, String)>>>,
    passkey_authentications:
        Arc<tokio::sync::Mutex<HashMap<String, (Instant, PasskeyAuthentication)>>>,
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
    let addr = resolve_listen_addr().unwrap_or_else(|error| {
        eprintln!("启动参数错误：{error}\n");
        print_usage();
        process::exit(2);
    });
    let db = db::init().await;
    let passkey_origin =
        env::var("PASSKEY_ORIGIN").unwrap_or_else(|_| default_passkey_origin(&addr));
    let passkey_rp_id = env::var("PASSKEY_RP_ID").unwrap_or_else(|_| {
        Url::parse(&passkey_origin)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "localhost".to_string())
    });
    let origin = Url::parse(&passkey_origin).expect("PASSKEY_ORIGIN 不是有效 URL");
    let webauthn = WebauthnBuilder::new(&passkey_rp_id, &origin)
        .expect("Passkey RP 配置无效")
        .rp_name("haruka")
        .build()
        .expect("Passkey 配置无效");
    let state = AppState {
        db,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        balance_writes: Arc::new(tokio::sync::Mutex::new(())),
        fx_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .user_agent("haruka/0.1")
            .build()
            .expect("汇率客户端初始化失败"),
        fx_fetches: Arc::new(tokio::sync::Mutex::new(())),
        webauthn: Arc::new(webauthn),
        passkey_registrations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        passkey_enrollments: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        passkey_authentications: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    };

    let protected = Router::new()
        .route("/", get(handlers::dashboard::redirect))
        .route("/dashboard", get(handlers::dashboard::show))
        .route(
            "/accounts",
            get(handlers::accounts::list).post(handlers::accounts::create),
        )
        .route("/accounts/new", get(handlers::accounts::new_form))
        .route("/accounts/{id}", get(handlers::accounts::detail))
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
        .route("/bills/search", get(handlers::bills::advanced_search))
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
        .route(
            "/subscriptions/search",
            get(handlers::subscriptions::advanced_search),
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
        .route("/installments", get(handlers::installments::list))
        .route(
            "/installments/search",
            get(handlers::installments::advanced_search),
        )
        .route("/installments/{id}", get(handlers::installments::detail))
        .route(
            "/installments/items/{id}/paid",
            post(handlers::installments::set_paid),
        )
        .route(
            "/transfers",
            get(handlers::dashboard::redirect).post(handlers::transfers::create),
        )
        .route("/transfers/quote", get(handlers::transfers::quote))
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
        .route(
            "/debt-people/search",
            get(handlers::debts::advanced_people_search),
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
            "/settings/currency",
            post(handlers::settings::update_currency),
        )
        .route("/statistics", get(handlers::statistics::show))
        .route("/currency-converter", get(handlers::currencies::converter))
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
        .route(
            "/settings/passkeys/register/start",
            post(handlers::passkeys::start_registration),
        )
        .route(
            "/settings/passkeys/register/finish",
            post(handlers::passkeys::finish_registration),
        )
        .route(
            "/settings/passkeys/register/complete",
            post(handlers::passkeys::complete_registration),
        )
        .route(
            "/settings/passkeys/{id}/delete",
            post(handlers::passkeys::delete),
        )
        .route("/lock", post(handlers::auth::lock))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_unlock,
        ));

    let app = Router::new()
        .route("/static/app.css", get(handlers::stylesheet))
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
        .route(
            "/passkey/auth/start",
            post(handlers::passkeys::start_authentication),
        )
        .route(
            "/passkey/auth/finish",
            post(handlers::passkeys::finish_authentication),
        )
        .merge(protected)
        .with_state(state)
        .layer(middleware::from_fn(handlers::render_error_response));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|error| {
            eprintln!("无法监听 {addr}：{error}");
            process::exit(1);
        });
    println!("haruka 已启动，监听地址: {addr}");
    println!("Passkey 来源: {passkey_origin}（RP ID: {passkey_rp_id}）");
    axum::serve(listener, app).await.expect("服务器运行失败");
}
