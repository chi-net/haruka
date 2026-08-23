mod db;
mod entity;
mod handlers;

use axum::{
    response::Redirect,
    routing::{get, post},
    Router,
};

#[tokio::main]
async fn main() {
    let db = db::init().await;

    let app = Router::new()
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
        .with_state(db);

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("端口绑定失败");
    println!("haruka 已启动: http://{addr}");
    axum::serve(listener, app).await.expect("服务器运行失败");
}
