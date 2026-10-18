//! Serve the programs built into this binary, each as a virtual object of its
//! name: `cargo build --release` embeds the programs, and the binary evaluates
//! them when it starts. Run it (PORT, by default 9080), then register it with
//! Restate:
//!
//! ```sh
//! target/release/quickjs-durable
//! curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
//! curl localhost:8080/counter/alice/run --json 20
//! ```
use restate_sdk::prelude::HttpServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let programs = quickjs_durable::embedded()?;
    let mut names: Vec<&str> = programs.names().collect();
    names.sort();
    let port = std::env::var("PORT").unwrap_or_else(|_| "9080".into());
    println!("Serving {} on port {port}", names.join(", "));
    HttpServer::new(quickjs_durable::endpoint(&programs).await?)
        .listen_and_serve(format!("0.0.0.0:{port}").parse()?)
        .await;
    Ok(())
}
