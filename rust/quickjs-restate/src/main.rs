//! Serve the durable JavaScript services: `cargo run --release` (PORT, by
//! default 9080), then register the endpoint with Restate:
//!
//! ```sh
//! curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
//! ```
use restate_sdk::prelude::HttpServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let port = std::env::var("PORT").unwrap_or_else(|_| "9080".into());
    HttpServer::new(quickjs_restate::endpoint()?)
        .listen_and_serve(format!("0.0.0.0:{port}").parse()?)
        .await;
    Ok(())
}
