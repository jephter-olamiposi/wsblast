//! Standalone WebSocket echo server for local benchmarking and integration verification.

use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::protocol::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr_str = env::var("ADDR").unwrap_or_else(|_| "127.0.0.1:9001".to_string());
    let delay_ms: u64 = env::var("DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let verbose = env::var("VERBOSE")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let addr: SocketAddr = addr_str.parse()?;
    let listener = TcpListener::bind(&addr).await?;

    println!(
        "{} wsblast echo server listening on {}",
        "[INFO]".green().bold(),
        format!("ws://{addr}").cyan().bold()
    );

    if delay_ms > 0 {
        println!(
            "{} Configured artificial delay: {}ms",
            "[INFO]".green().bold(),
            delay_ms.to_string().yellow()
        );
    }

    if verbose {
        println!(
            "{} Verbose per-frame logging enabled ({})",
            "[INFO]".green().bold(),
            "VERBOSE=1".green()
        );
    }

    let active_connections = Arc::new(AtomicUsize::new(0));
    let total_messages = Arc::new(AtomicU64::new(0));

    let stats_active = Arc::clone(&active_connections);
    let stats_total = Arc::clone(&total_messages);
    tokio::spawn(async move {
        let mut last_total = 0u64;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let active = stats_active.load(Ordering::Relaxed);
            let current_total = stats_total.load(Ordering::Relaxed);
            let delta = current_total.saturating_sub(last_total);
            last_total = current_total;

            if active > 0 || current_total > 0 {
                let rate_formatted = if delta >= 1_000_000 {
                    format!("{:.2}M msg/s", delta as f64 / 1_000_000.0)
                } else if delta >= 1_000 {
                    format!("{:.1}k msg/s", delta as f64 / 1_000.0)
                } else {
                    format!("{delta} msg/s")
                };

                println!(
                    "{} Active: {} | Total echoed: {} | Rate: {}",
                    "[STATS]".magenta().bold(),
                    active.to_string().cyan().bold(),
                    current_total.to_string().green(),
                    rate_formatted.yellow().bold()
                );
            }
        }
    });

    while let Ok((stream, peer)) = listener.accept().await {
        let active_ref = Arc::clone(&active_connections);
        let messages_ref = Arc::clone(&total_messages);

        tokio::spawn(async move {
            active_ref.fetch_add(1, Ordering::Relaxed);
            let current_active = active_ref.load(Ordering::Relaxed);

            println!(
                "{} Accepted connection from {} (active: {})",
                "[CONN]".blue().bold(),
                peer.to_string().cyan(),
                current_active.to_string().green()
            );

            let res = handle_connection(stream, peer, delay_ms, messages_ref, verbose).await;

            let remaining = active_ref.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
            match res {
                Ok(count) => {
                    println!(
                        "{} Closed connection from {} (processed {} frames | active: {})",
                        "[DISC]".yellow().bold(),
                        peer.to_string().cyan(),
                        count.to_string().green(),
                        remaining
                    );
                }
                Err(e) => {
                    eprintln!(
                        "{} Connection error with {}: {} (active: {})",
                        "[WARN]".red().bold(),
                        peer,
                        e,
                        remaining
                    );
                }
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    delay_ms: u64,
    total_messages: Arc<AtomicU64>,
    verbose: bool,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut reader) = ws_stream.split();
    let mut conn_messages = 0u64;

    while let Some(msg_result) = reader.next().await {
        let msg = msg_result?;
        match msg {
            Message::Text(text) => {
                conn_messages += 1;
                total_messages.fetch_add(1, Ordering::Relaxed);

                if verbose {
                    println!(
                        "{} Recv text from {}: {:.60}",
                        "[FRAME]".bright_black(),
                        peer,
                        text
                    );
                }

                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                sink.send(Message::Text(text)).await?;
            }
            Message::Binary(bin) => {
                conn_messages += 1;
                total_messages.fetch_add(1, Ordering::Relaxed);

                if verbose {
                    println!(
                        "{} Recv binary frame from {} ({} bytes)",
                        "[FRAME]".bright_black(),
                        peer,
                        bin.len()
                    );
                }

                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                sink.send(Message::Binary(bin)).await?;
            }
            Message::Ping(data) => {
                sink.send(Message::Pong(data)).await?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
            Message::Frame(_) => {}
        }
    }

    Ok(conn_messages)
}
