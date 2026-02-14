mod protocol;
mod twitch;
mod youtube;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use protocol::{
    Authentication, HelloMessage, IdentifiedResult, IdentifyData, IncomingMessage, OutgoingMessage,
    PongMessage, TwitchStartData, YouTubeStartData, API_VERSION, IdentifiedMessage,
};

const DEFAULT_PORT: u16 = 2345;

#[derive(Parser, Debug)]
#[command(name = "moblin-assistant")]
#[command(about = "Moblin remote control assistant server")]
struct Args {
    /// Password for authentication
    #[arg(short, long)]
    password: String,

    /// Port to listen on
    #[arg(short = 'P', long, default_value_t = DEFAULT_PORT)]
    port: u16,
}

pub(crate) struct Streamer {
    password: String,
    challenge: String,
    salt: String,
    pub identified: bool,
    request_id: i32,
    chat_message_id: i32,
}

impl Streamer {
    fn new(password: String) -> Self {
        Self {
            password,
            challenge: random_string(),
            salt: random_string(),
            identified: false,
            request_id: 0,
            chat_message_id: 0,
        }
    }

    fn hash_password(&self) -> String {
        // First hash: password + salt
        let concatenated = format!("{}{}", self.password, self.salt);
        let mut hasher = Sha256::new();
        hasher.update(concatenated.as_bytes());
        let hash1 = BASE64.encode(hasher.finalize());

        // Second hash: hash1 + challenge
        let concatenated = format!("{}{}", hash1, self.challenge);
        let mut hasher = Sha256::new();
        hasher.update(concatenated.as_bytes());
        BASE64.encode(hasher.finalize())
    }

    pub fn next_id(&mut self) -> i32 {
        self.request_id += 1;
        self.request_id
    }

    pub fn next_chat_message_id(&mut self) -> i32 {
        self.chat_message_id += 1;
        self.chat_message_id
    }

    fn create_hello_message(&self) -> OutgoingMessage {
        OutgoingMessage::Hello(HelloMessage {
            api_version: API_VERSION.to_string(),
            authentication: Authentication {
                challenge: self.challenge.clone(),
                salt: self.salt.clone(),
            },
        })
    }

    fn create_identified_message(&self, result: IdentifiedResult) -> OutgoingMessage {
        OutgoingMessage::Identified(IdentifiedMessage { result })
    }

    fn create_pong_message(&self) -> OutgoingMessage {
        OutgoingMessage::Pong(PongMessage {})
    }
}

fn random_string() -> String {
    use rand::Rng;
    let bytes: Vec<u8> = (0..64).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(bytes)
}

pub(crate) type WsWriter = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            Message,
        >,
    >,
>;

async fn handle_identify_message(
    identify: &IdentifyData,
    writer: &WsWriter,
    streamer: &Arc<Mutex<Streamer>>,
    peer_address: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("[{peer_address}] Processing identify message");
    let mut streamer = streamer.lock().await;
    let expected_hash = streamer.hash_password();

    let result = if streamer.identified {
        debug!("[{peer_address}] Streamer already identified");
        IdentifiedResult::AlreadyIdentified {}
    } else if identify.authentication == expected_hash {
        streamer.identified = true;
        info!("[{peer_address}] Streamer successfully identified");
        IdentifiedResult::Ok {}
    } else {
        error!("[{peer_address}] Wrong password from streamer");
        IdentifiedResult::WrongPassword {}
    };

    let identified = streamer.create_identified_message(result);
    let is_identified = streamer.identified;
    drop(streamer);

    let mut writer = writer.lock().await;
    writer
        .send(Message::Text(
            serde_json::to_string(&identified).expect("Failed to serialize identified response"),
        ))
        .await?;
    debug!("[{peer_address}] Sent identified response");
    drop(writer);

    // If identified successfully, start processing Twitch messages
    if is_identified {
        debug!("[{peer_address}] Streamer identified, waiting for twitchStart message");
    }
    Ok(())
}

async fn handle_ping_message(
    writer: &WsWriter,
    streamer: &Arc<Mutex<Streamer>>,
    peer_address: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let streamer = streamer.lock().await;
    let pong = streamer.create_pong_message();
    drop(streamer);

    let mut writer = writer.lock().await;
    writer
        .send(Message::Text(
            serde_json::to_string(&pong).expect("Failed to serialize pong message"),
        ))
        .await?;
    debug!("[{peer_address}] Sent pong response");
    Ok(())
}

async fn handle_event_message(peer_address: &str) {
    debug!("[{peer_address}] Received event from streamer");
}

async fn handle_twitch_start_message(
    twitch_start: &TwitchStartData,
    writer: &WsWriter,
    streamer: &Arc<Mutex<Streamer>>,
    peer_address: &str,
) {
    if let Some(channel_name) = &twitch_start.channel_name {
        info!("[{peer_address}] Starting Twitch IRC connection for channel: {channel_name}");
        tokio::spawn(twitch::connect_twitch_irc(
            writer.clone(),
            streamer.clone(),
            channel_name.to_lowercase(),
            peer_address.to_string(),
        ));
    }
}

async fn handle_youtube_start_message(
    youtube_start: &YouTubeStartData,
    writer: &WsWriter,
    streamer: &Arc<Mutex<Streamer>>,
    peer_address: &str,
) {
    info!(
        "[{peer_address}] Starting YouTube chat for video: {}",
        youtube_start.video_id
    );
    tokio::spawn(youtube::connect_youtube_chat(
        writer.clone(),
        streamer.clone(),
        youtube_start.video_id.clone(),
        peer_address.to_string(),
    ));
}

async fn handle_response_message(peer_address: &str) {
    debug!("[{peer_address}] Received response from streamer");
}

async fn is_identified(streamer: &Arc<Mutex<Streamer>>) -> bool {
    streamer.lock().await.identified
}

async fn handle_websocket_ping(
    data: Vec<u8>,
    writer: &WsWriter,
    peer_address: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("[{peer_address}] Received WebSocket ping");
    let mut writer = writer.lock().await;
    writer.send(Message::Pong(data)).await?;
    debug!("[{peer_address}] Sent WebSocket pong");
    Ok(())
}

async fn handle_streamer_connection(stream: tokio::net::TcpStream, streamer: Arc<Mutex<Streamer>>) {
    let peer_address = stream
        .peer_addr()
        .expect("Failed to get peer address")
        .to_string();
    info!("[{peer_address}] New streamer connection");

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => {
            debug!("[{peer_address}] WebSocket handshake completed");
            ws
        }
        Err(e) => {
            error!("[{peer_address}] Error during WebSocket handshake: {e}");
            return;
        }
    };

    let (write, mut read) = ws_stream.split();
    let writer: WsWriter = Arc::new(Mutex::new(write));

    {
        let streamer = streamer.lock().await;
        let hello = streamer.create_hello_message();
        let mut writer = writer.lock().await;
        if let Err(e) = writer
            .send(Message::Text(
                serde_json::to_string(&hello).expect("Failed to serialize hello message"),
            ))
            .await
        {
            error!("[{peer_address}] Error sending hello: {e}");
            return;
        }
        debug!("[{peer_address}] Sent hello message with authentication challenge");
    }

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                error!("[{peer_address}] Error receiving message: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                debug!("[{peer_address}] Received message: {text}");

                let incoming: IncomingMessage = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("[{peer_address}] Error parsing JSON: {e}");
                        continue;
                    }
                };

                match &incoming {
                    IncomingMessage::Identify(identify) => {
                        if let Err(e) =
                            handle_identify_message(identify, &writer, &streamer, &peer_address)
                                .await
                        {
                            error!("[{peer_address}] Error handling identify message: {e}");
                            break;
                        }
                    }
                    IncomingMessage::Ping(_) => {
                        if let Err(e) = handle_ping_message(&writer, &streamer, &peer_address).await
                        {
                            error!("[{peer_address}] Error handling ping message: {e}");
                            break;
                        }
                    }
                    IncomingMessage::Event(_) => {
                        if !is_identified(&streamer).await {
                            break;
                        }
                        handle_event_message(&peer_address).await;
                    }
                    IncomingMessage::TwitchStart(twitch_start) => {
                        if !is_identified(&streamer).await {
                            break;
                        }
                        handle_twitch_start_message(
                            twitch_start,
                            &writer,
                            &streamer,
                            &peer_address,
                        )
                        .await;
                    }
                    IncomingMessage::YouTubeStart(youtube_start) => {
                        if !is_identified(&streamer).await {
                            break;
                        }
                        handle_youtube_start_message(
                            youtube_start,
                            &writer,
                            &streamer,
                            &peer_address,
                        )
                        .await;
                    }
                    IncomingMessage::Response(_) => {
                        if !is_identified(&streamer).await {
                            break;
                        }
                        handle_response_message(&peer_address).await;
                    }
                }
            }
            Message::Ping(data) => {
                if let Err(e) = handle_websocket_ping(data, &writer, &peer_address).await {
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

    info!("[{peer_address}] Streamer disconnected");
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let args = Args::parse();

    info!("Starting Moblin Assistant server on port {}", args.port);

    let listener = TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind");

    info!("Server listening on 0.0.0.0:{}", args.port);

    loop {
        let (stream, addr) = listener
            .accept()
            .await
            .expect("Failed to accept connection");
        debug!("Accepted TCP connection from {addr}");
        let streamer = Arc::new(Mutex::new(Streamer::new(args.password.clone())));
        tokio::spawn(handle_streamer_connection(stream, streamer));
    }
}
