mod cleanup;
mod protocol;
mod streamer;
mod twitch;
mod utils;
mod youtube;

use clap::Parser;
use futures_util::stream::SplitSink;
use log::info;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::cleanup::start_periodic_cleanup;
use crate::streamer::{handle_streamer, StreamerConnection, StreamerState};

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

enum Streamer {
    Connected(Weak<Mutex<StreamerConnection>>),
    Disconnected(DisconnectedStreamer),
}

struct DisconnectedStreamer {
    state: Arc<Mutex<StreamerState>>,
    time: Instant,
}

impl DisconnectedStreamer {
    pub fn new(state: Arc<Mutex<StreamerState>>) -> Self {
        Self {
            state,
            time: Instant::now(),
        }
    }
}

type Streamers = Arc<Mutex<HashMap<String, Streamer>>>;
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
    let streamers = Arc::new(Mutex::new(HashMap::new()));
    start_periodic_cleanup(streamers.clone());
    loop {
        let (stream, peer_address) = listener
            .accept()
            .await
            .expect("Failed to accept connection");
        tokio::spawn(handle_streamer(
            stream,
            peer_address.to_string(),
            args.password.clone(),
            streamers.clone(),
        ));
    }
}
