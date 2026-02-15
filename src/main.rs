mod protocol;
mod streamer;
mod twitch;
mod utils;
mod youtube;

use clap::Parser;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, WebSocketStream};

use crate::streamer::{StreamerConnection, StreamerState};
use crate::utils::AnyError;

#[derive(Parser, Debug)]
#[command(name = "moblin-assistant")]
#[command(about = "Moblin Remote Control Assistant server")]
struct Args {
    /// Password for authentication
    #[arg(short, long)]
    password: String,

    /// Port to listen on
    #[arg(short = 'P', long, default_value_t = 2345)]
    port: u16,
}

type DisconnectedStreamers = Arc<Mutex<HashMap<String, (Arc<Mutex<StreamerState>>, Instant)>>>;
type ActiveStreamers = Arc<Mutex<HashMap<String, Weak<Mutex<StreamerConnection>>>>>;
type WebsocketWriter = Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>;

#[tokio::main]
async fn main() {
    env_logger::init();
    let args = Args::parse();

    let address = format!("0.0.0.0:{}", args.port);
    info!("Starting server on {}", address);
    let listener = TcpListener::bind(address.clone())
        .await
        .expect("Failed to bind");

    let disconnected_streamers: DisconnectedStreamers = Arc::new(Mutex::new(HashMap::new()));
    let active_streamers: ActiveStreamers = Arc::new(Mutex::new(HashMap::new()));
    start_cleanup_task(disconnected_streamers.clone());

    loop {
        let (stream, peer_address) = listener
            .accept()
            .await
            .expect("Failed to accept connection");
        tokio::spawn(handle_streamer(
            stream,
            peer_address.to_string(),
            args.password.clone(),
            disconnected_streamers.clone(),
            active_streamers.clone(),
        ));
    }
}

async fn handle_streamer(
    stream: tokio::net::TcpStream,
    peer_address: String,
    password: String,
    disconnected_streamers: DisconnectedStreamers,
    active_streamers: ActiveStreamers,
) {
    let Ok(stream) = accept_async(stream).await else {
        return;
    };

    info!("[{peer_address}] New streamer");

    let (writer, mut reader) = stream.split();
    let writer = Arc::new(Mutex::new(writer));
    let streamer = StreamerConnection::new(
        password,
        peer_address.clone(),
        writer.clone(),
        disconnected_streamers,
        active_streamers,
    );
    streamer.lock().await.send_hello().await;

    while let Some(Ok(message)) = reader.next().await {
        match message {
            Message::Text(text) => {
                if let Err(e) = streamer.lock().await.handle_message(&text).await {
                    error!("[{peer_address}] Error handling WebSocket text: {e}");
                    break;
                }
            }
            Message::Ping(data) => {
                if let Err(e) = handle_ping(data, &writer, &peer_address).await {
                    error!("[{peer_address}] Error handling WebSocket ping: {e}");
                    break;
                }
            }
            Message::Close(_) => {
                debug!("[{peer_address}] Connection closed by streamer");
                break;
            }
            _ => {
                debug!("[{peer_address}] Received unhandled message type");
            }
        }
    }

    streamer.lock().await.stop().await;
    info!("[{peer_address}] Streamer disconnected");
}

async fn handle_ping(
    data: Vec<u8>,
    writer: &WebsocketWriter,
    peer_address: &str,
) -> Result<(), AnyError> {
    debug!("[{peer_address}] Received WebSocket ping");
    let mut writer = writer.lock().await;
    writer.send(Message::Pong(data)).await?;
    debug!("[{peer_address}] Sent WebSocket pong");
    Ok(())
}

fn start_cleanup_task(disconnected_streamers: DisconnectedStreamers) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_hours(1)).await;
            disconnected_streamers
                .lock()
                .await
                .retain(|streamer_id, (_, saved_at)| {
                    let expired = saved_at.elapsed() >= std::time::Duration::from_hours(12);
                    if expired {
                        info!("Removing expired state for streamer_id={}", streamer_id);
                    }
                    !expired
                });
        }
    });
}
