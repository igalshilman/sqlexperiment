fn main() {
    let mode = if cfg!(feature = "full-scan") {
        "full-scan control"
    } else {
        "portable Wasm write barriers"
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "active_checkpoint_tracking": mode,
            "optional_linux_backend": quickjs_durable::tracking::probe()
        }))
        .unwrap()
    );
}
