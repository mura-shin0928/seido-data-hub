use anyhow::Context;
use sea_orm::{ConnectOptions, Database};

/// Lambda の外（ローカル）で待ち受けるアドレス。
const LOCAL_ADDR: &str = "127.0.0.1:3000";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL が無い")?;
    // Lambda のランタイムが立てる環境変数。あれば Lambda の中で動いている
    let on_lambda = std::env::var_os("AWS_LAMBDA_RUNTIME_API").is_some();

    let mut options = ConnectOptions::new(database_url);
    if on_lambda {
        // Lambda の1台は同時に1リクエストしか処理しないので、接続は1本で足りる。
        // 初期化時に張り、同じ台の次のリクエストで使い回す
        options.max_connections(1).min_connections(1);
    }
    options.sqlx_logging(false);
    let db = Database::connect(options)
        .await
        .context("DB に接続できない")?;
    let app = api::router(db);

    if on_lambda {
        lambda_http::run(app)
            .await
            .map_err(|err| anyhow::anyhow!(err))
    } else {
        let listener = tokio::net::TcpListener::bind(LOCAL_ADDR).await?;
        println!("http://{LOCAL_ADDR} で待ち受け中");
        axum::serve(listener, app).await?;
        Ok(())
    }
}
