//! Standalone WebSocket echo server for local benchmarking and integration verification.

use futures_util::{SinkExt, StreamExt};
use std::env;
use std::net::SocketAddr;
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

    let addr: SocketAddr = addr_str.parse()?;
    let listener = TcpListener::bind(&addr).await?;

    println!("wsblast echo server listening on ws://{addr}");
    if delay_ms > 0 {
        println!("Configured echo response delay: {delay_ms}ms");
    }

    while let Ok((stream, peer)) = listener.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer, delay_ms).await {
                eprintln!("Connection error with {peer}: {e}");
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    _peer: SocketAddr,
    delay_ms: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut reader) = ws_stream.split();

    while let Some(msg_result) = reader.next().await {
        let msg = msg_result?;
        match msg {
            Message::Text(text) => {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                sink.send(Message::Text(text)).await?;
            }
            Message::Binary(bin) => {
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

    Ok(())
}
