mod protocol;
mod twitch;
mod youtube;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Weak};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use protocol::{
    Authentication, HelloMessage, IdentifiedMessage, IdentifiedResult, IdentifyMessage,
    MessageToAssistant, MessageToStreamer, TwitchStartMessage, YouTubeStartMessage, API_VERSION,
};

const DEFAULT_PORT: u16 = 2345;

pub type AnyError = Box<dyn std::error::Error + Send + Sync>;

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
    me: Weak<Mutex<Self>>,
    password: String,
    peer_address: String,
    challenge: String,
    salt: String,
    pub identified: bool,
    request_id: i32,
    chat_message_id: i32,
    writer: WsWriter,
}

impl Streamer {
    fn new(password: String, peer_address: String, writer: WsWriter) -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                password,
                peer_address,
                challenge: random_string(),
                salt: random_string(),
                identified: false,
                request_id: 0,
                chat_message_id: 0,
                writer,
            })
        })
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

    pub async fn send_hello(&mut self) {
        let hello = MessageToStreamer::Hello(HelloMessage {
            api_version: API_VERSION.to_string(),
            authentication: Authentication {
                challenge: self.challenge.clone(),
                salt: self.salt.clone(),
            },
        });
        let mut writer = self.writer.lock().await;
        if let Err(e) = writer
            .send(Message::Text(
                serde_json::to_string(&hello).expect("Failed to serialize hello message"),
            ))
            .await
        {
            error!("[{}] Error sending hello: {}", self.peer_address, e);
            return;
        }
        debug!(
            "[{}] Sent hello message with authentication challenge",
            self.peer_address
        );
    }

    pub async fn handle_message(&mut self, message: MessageToAssistant) -> Result<(), AnyError> {
        match message {
            MessageToAssistant::Identify(identify) => {
                self.handle_identify(identify).await?;
            }
            MessageToAssistant::Ping(_) => {
                self.handle_ping().await?;
            }
            MessageToAssistant::Event(_) => {
                self.handle_event().await;
            }
            MessageToAssistant::TwitchStart(twitch_start) => {
                self.handle_twitch_start(twitch_start).await;
            }
            MessageToAssistant::YouTubeStart(youtube_start) => {
                self.handle_youtube_start(youtube_start).await;
            }
            MessageToAssistant::Response(_) => {
                self.handle_response().await;
            }
        }
        Ok(())
    }

    pub async fn handle_identify(&mut self, identify: IdentifyMessage) -> Result<(), AnyError> {
        debug!("[{}] Processing identify message", self.peer_address);
        let expected_hash = self.hash_password();

        let result = if self.identified {
            debug!("[{}] Streamer already identified", self.peer_address);
            IdentifiedResult::AlreadyIdentified {}
        } else if identify.authentication == expected_hash {
            self.identified = true;
            info!("[{}] Streamer successfully identified", self.peer_address);
            IdentifiedResult::Ok {}
        } else {
            error!("[{}] Wrong password from streamer", self.peer_address);
            IdentifiedResult::WrongPassword {}
        };

        let identified = MessageToStreamer::Identified(IdentifiedMessage { result });
        self.writer
            .lock()
            .await
            .send(Message::Text(
                serde_json::to_string(&identified)
                    .expect("Failed to serialize identified response"),
            ))
            .await?;

        Ok(())
    }

    pub async fn handle_ping(&mut self) -> Result<(), AnyError> {
        let pong = MessageToStreamer::Pong {};
        let mut writer = self.writer.lock().await;
        writer
            .send(Message::Text(
                serde_json::to_string(&pong).expect("Failed to serialize pong message"),
            ))
            .await?;
        Ok(())
    }

    pub async fn handle_event(&mut self) {
        if !self.identified {
            return;
        }
        debug!("[{}] Received event from streamer", self.peer_address);
    }

    pub async fn handle_twitch_start(&mut self, twitch_start: TwitchStartMessage) {
        if !self.identified {
            return;
        }
        if let Some(channel_name) = &twitch_start.channel_name {
            info!(
                "[{}] Starting Twitch IRC connection for channel: {}",
                self.peer_address, channel_name
            );
            tokio::spawn(twitch::connect_twitch_irc(
                self.me.clone(),
                channel_name.to_lowercase(),
                self.peer_address.clone(),
            ));
        }
    }

    pub async fn handle_youtube_start(&mut self, youtube_start: YouTubeStartMessage) {
        if !self.identified {
            return;
        }
        info!(
            "[{}] Starting YouTube chat for video: {}",
            self.peer_address, youtube_start.video_id
        );
        tokio::spawn(youtube::connect_youtube_chat(
            self.me.clone(),
            youtube_start.video_id.clone(),
            self.peer_address.clone(),
        ));
    }

    pub async fn handle_response(&mut self) {
        if !self.identified {
            return;
        }
        debug!("[{}] Received response from streamer", self.peer_address);
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

async fn handle_websocket_ping(
    data: Vec<u8>,
    writer: &WsWriter,
    peer_address: &str,
) -> Result<(), AnyError> {
    debug!("[{peer_address}] Received WebSocket ping");
    let mut writer = writer.lock().await;
    writer.send(Message::Pong(data)).await?;
    debug!("[{peer_address}] Sent WebSocket pong");
    Ok(())
}

async fn handle_streamer_connection(stream: tokio::net::TcpStream, password: String) {
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
    let writer = Arc::new(Mutex::new(write));
    let streamer = Streamer::new(password, peer_address.clone(), writer.clone());
    streamer.lock().await.send_hello().await;

    while let Some(message) = read.next().await {
        let message = match message {
            Ok(message) => message,
            Err(e) => {
                info!("[{peer_address}] Error receiving message: {e}");
                break;
            }
        };

        match message {
            Message::Text(text) => {
                debug!("[{peer_address}] Received message: {text}");

                let message: MessageToAssistant = match serde_json::from_str(&text) {
                    Ok(message) => message,
                    Err(e) => {
                        error!("[{peer_address}] Error parsing JSON: {e}");
                        continue;
                    }
                };

                if let Err(e) = streamer.lock().await.handle_message(message).await {
                    error!("[{peer_address}] Error handling WebSocket message: {e}");
                    break;
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

    let address = format!("0.0.0.0:{}", args.port);
    info!("Starting server on {}", address);
    let listener = TcpListener::bind(address.clone())
        .await
        .expect("Failed to bind");
    info!("Server listening on {}", address);

    loop {
        let (stream, addr) = listener
            .accept()
            .await
            .expect("Failed to accept connection");
        debug!("Accepted TCP connection from {addr}");
        tokio::spawn(handle_streamer_connection(stream, args.password.clone()));
    }
}
