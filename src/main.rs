use autometrics::autometrics;
use axum::{http::StatusCode, routing::get, Router};
use clap::Parser;
use config::LoggerExt;
use uuid::Uuid;

mod cli;
mod config;

#[tokio::main]
async fn main() {
    let config = cli::Config::try_from(cli::Args::parse()).unwrap();

    let logger = &config.logger;

    let _logger_guards = logger.init_logger().unwrap();
    tracing::info!("Logger: {logger:?}");

    let addr = config.address;
    tracing::info!("Listening on {addr}");
    let app: Router = Router::new().route("/hello", get(hello));
    let app = app.route(
        "/metrics",
        axum::routing::get(|| async {
            autometrics::prometheus_exporter::encode_to_string()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }),
    );

    axum::Server::bind(&config.address)
        .serve(app.into_make_service())
        .await
        .unwrap()
}

#[autometrics(objective = config::API_SLO)]
#[tracing::instrument]
pub async fn hello() -> &'static str {
    tracing::info!(counter.baz = 1);
    tracing::info!("Got a http hello request.");
    "Hello World!"
}
