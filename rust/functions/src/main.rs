//! Serve the programs built into this binary, each as a virtual object of its
//! name: `cargo build --release` embeds the programs, and the binary evaluates
//! them when it starts. Run it (PORT, by default 9080), then register it with
//! Restate:
//!
//! ```sh
//! target/release/functions
//! curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
//! curl localhost:8080/counter/alice/run --json 20
//! ```
use restate_sdk::prelude::HttpServer;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    give_back_freed_memory()?;
    functions::no_huge_pages()?;
    let programs = functions::embedded()?;
    let mut names: Vec<&str> = programs.names().collect();
    names.sort();
    let port = std::env::var("PORT").unwrap_or_else(|_| "9080".into());
    println!("Serving {} on port {port}", names.join(", "));
    HttpServer::new(functions::endpoint(&programs).await?)
        .listen_and_serve(format!("0.0.0.0:{port}").parse()?)
        .await;
    Ok(())
}

/// Give back to the system the memory that a burst of runs freed: every
/// 5 s, if what is in use fell by 64 MiB since its peak. glibc's allocator
/// keeps freed memory for reuse: after 800 Python runs at once, an idle
/// server held 1.3 GB, and 140 MB once trimmed.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn give_back_freed_memory() -> std::io::Result<()> {
    const FREED: usize = 64 << 20;
    std::thread::Builder::new()
        .name("trim".into())
        .spawn(|| {
            let mut peak = 0;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                // SAFETY: glibc's own functions, safe from any thread.
                let in_use = unsafe { libc::mallinfo2() }.uordblks;
                peak = peak.max(in_use);
                if peak - in_use > FREED {
                    unsafe { libc::malloc_trim(0) };
                    peak = in_use;
                }
            }
        })
        .map(drop)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn give_back_freed_memory() -> std::io::Result<()> {
    Ok(())
}
