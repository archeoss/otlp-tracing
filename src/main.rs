use axum::{http::StatusCode, routing::get, Router, ServiceExt};
use clap::Parser;
use error_stack::ResultExt;
use config::LoggerExt;


mod config;
mod cli;

#[tokio::main]
async fn main() {
    let config = cli::Config::try_from(cli::Args::parse()).unwrap();

        let logger = &config.logger;

        let logger_guards = logger.init_logger().unwrap();
        tracing::info!("Logger: {logger:?}");

        let addr = config.address;
        tracing::info!("Listening on {addr}");
        let app: Router = Router::new()
        .route("/hello", get(hello));
        let app = 
        app
            .route(
                "/metrics",
                axum::routing::get(|| async {
                    autometrics::prometheus_exporter::encode_to_string()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
                }),
            );

        axum::Server::bind(&config.address)
            .serve(app.into_make_service())
            .await.unwrap()
}

#[tracing::instrument]
pub async fn hello() -> &'static str {
    tracing::info!("Got a http hello request.");
    "Hello World!"
}
