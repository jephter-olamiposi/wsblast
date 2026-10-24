//! The `wsblast` command-line entry point. The command lives in the library
//! so the binary and the library share one engine.

#[tokio::main]
async fn main() {
    wsblast::__private::run_cli().await;
}
