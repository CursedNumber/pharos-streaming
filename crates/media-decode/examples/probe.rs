//! Manual probe check: `cargo run -p media-decode --example probe`.
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let caps = media_decode::capabilities();
    println!("\nchosen input args: {:?}", caps.input_args());
    println!("selected: {} — {}", caps.chosen().id, caps.selected_reason);
}
