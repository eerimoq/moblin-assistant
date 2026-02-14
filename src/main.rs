use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const API_VERSION: &str = "0.1";
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

#[derive(Debug, Serialize, Deserialize)]
struct Authentication {
    challenge: String,
    salt: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct HelloMessage {
    #[serde(rename = "apiVersion")]
    api_version: String,
    authentication: Authentication,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum IdentifiedResult {
    Ok {},
    WrongPassword {},
    AlreadyIdentified {},
}

#[derive(Debug, Serialize, Deserialize)]
struct IdentifiedMessage {
    result: IdentifiedResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Platform {
    Soop {},
    Kick {},
    OpenStreamingPlatform {},
    Twitch {},
    YouTube {},
    #[serde(rename = "dlive")]
    DLive {},
}

#[derive(Debug, Serialize, Deserialize)]
struct RgbColor {
    red: f64,
    green: f64,
    blue: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum ChatPostSegment {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "url")]
    Url { url: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    id: i32,
    platform: Platform,
    message_id: Option<String>,
    display_name: Option<String>,
    user: Option<String>,
    user_id: Option<String>,
    user_color: Option<RgbColor>,
    user_badges: Vec<String>,
    segments: Vec<ChatPostSegment>,
    timestamp: String,
    is_action: bool,
    is_moderator: bool,
    is_subscriber: bool,
    is_owner: bool,
    bits: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessagesRequest {
    history: bool,
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RequestMessage {
    id: i32,
    data: serde_json::Value,
}

struct Assistant {
    password: String,
    challenge: String,
    salt: String,
    identified: bool,
    request_id: i32,
    chat_message_id: i32,
}

impl Assistant {
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

    fn next_id(&mut self) -> i32 {
        self.request_id += 1;
        self.request_id
    }

    fn next_chat_message_id(&mut self) -> i32 {
        self.chat_message_id += 1;
        self.chat_message_id
    }

    fn create_hello_message(&self) -> serde_json::Value {
        json!({
            "hello": {
                "apiVersion": API_VERSION,
                "authentication": {
                    "challenge": self.challenge,
                    "salt": self.salt
                }
            }
        })
    }

    fn create_identified_message(&self, result: IdentifiedResult) -> serde_json::Value {
        json!({
            "identified": {
                "result": result
            }
        })
    }

    fn create_pong_message(&self) -> serde_json::Value {
        json!({
            "pong": {}
        })
    }

    fn create_chat_message_request(&mut self, message: &str) -> serde_json::Value {
        let chat_message = ChatMessage {
            id: self.next_chat_message_id(),
            platform: Platform::Twitch {},
            message_id: None,
            display_name: Some("Assistant".to_string()),
            user: Some("assistant".to_string()),
            user_id: Some("assistant".to_string()),
            user_color: Some(RgbColor {
                red: 0.0,
                green: 0.5,
                blue: 1.0,
            }),
            user_badges: vec![],
            segments: vec![ChatPostSegment::Text {
                text: message.to_string(),
            }],
            timestamp: chrono::Utc::now().to_rfc3339(),
            is_action: false,
            is_moderator: false,
            is_subscriber: false,
            is_owner: false,
            bits: None,
        };

        let request_id = self.next_id();
        json!({
            "request": {
                "id": request_id,
                "data": {
                    "chatMessages": {
                        "history": false,
                        "messages": [chat_message]
                    }
                }
            }
        })
    }
}

fn random_string() -> String {
    use rand::Rng;
    let bytes: Vec<u8> = (0..64).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(bytes)
}

type WsWriter = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            Message,
        >,
    >,
>;

fn log(addr: &str, msg: &str) {
    println!("[{}] [{}] {}", chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"), addr, msg);
}

fn log_error(addr: &str, msg: &str) {
    eprintln!("[{}] [{}] {}", chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"), addr, msg);
}

async fn send_delayed_chat_messages(
    writer: WsWriter,
    assistant: Arc<Mutex<Assistant>>,
    addr: String,
) {
    // Send first chat message after 2 seconds
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    {
        let mut assistant_locked = assistant.lock().await;
        let chat_request =
            assistant_locked.create_chat_message_request("Hello from Rust Moblin Assistant! 🎉");
        drop(assistant_locked);

        if let Ok(msg_str) = serde_json::to_string(&chat_request) {
            log(&addr, &format!("Sending chat message: {}", msg_str));
            let mut write = writer.lock().await;
            if let Err(e) = write.send(Message::Text(msg_str)).await {
                log_error(
                    &addr,
                    &format!("Error sending chat message, aborting delayed messages: {}", e),
                );
                return;
            }
        }
    }

    // Send second message after 3 more seconds
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    {
        let mut assistant_locked = assistant.lock().await;
        let chat_request =
            assistant_locked.create_chat_message_request("This is a second hardcoded message!");
        drop(assistant_locked);

        if let Ok(msg_str) = serde_json::to_string(&chat_request) {
            log(&addr, &format!("Sending second chat message: {}", msg_str));
            let mut write = writer.lock().await;
            if let Err(e) = write.send(Message::Text(msg_str)).await {
                log_error(&addr, &format!("Error sending second chat message: {}", e));
            }
        }
    }
}

async fn handle_streamer_connection(
    stream: tokio::net::TcpStream,
    assistant: Arc<Mutex<Assistant>>,
) {
    let addr = stream
        .peer_addr()
        .expect("Failed to get peer address")
        .to_string();
    log(&addr, "New streamer connection");

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => {
            log(&addr, "WebSocket handshake completed");
            ws
        }
        Err(e) => {
            log_error(&addr, &format!("Error during WebSocket handshake: {}", e));
            return;
        }
    };

    let (write, mut read) = ws_stream.split();
    let writer: WsWriter = Arc::new(Mutex::new(write));

    // Send hello message
    {
        let assistant = assistant.lock().await;
        let hello = assistant.create_hello_message();
        let mut write = writer.lock().await;
        if let Err(e) = write
            .send(Message::Text(
                serde_json::to_string(&hello).expect("Failed to serialize hello message"),
            ))
            .await
        {
            log_error(&addr, &format!("Error sending hello: {}", e));
            return;
        }
        log(&addr, "Sent hello message with authentication challenge");
    }

    // Handle incoming messages
    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                log_error(&addr, &format!("Error receiving message: {}", e));
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                log(&addr, &format!("Received message: {}", text));

                let json_msg: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        log_error(&addr, &format!("Error parsing JSON: {}", e));
                        continue;
                    }
                };

                // Handle identify message
                if let Some(identify) = json_msg.get("identify") {
                    log(&addr, "Processing identify message");
                    if let Some(auth) =
                        identify.get("authentication").and_then(|a| a.as_str())
                    {
                        let mut assistant_locked = assistant.lock().await;
                        let expected_hash = assistant_locked.hash_password();

                        let result = if assistant_locked.identified {
                            log(&addr, "Streamer already identified");
                            IdentifiedResult::AlreadyIdentified {}
                        } else if auth == expected_hash {
                            assistant_locked.identified = true;
                            log(&addr, "Streamer successfully identified");
                            IdentifiedResult::Ok {}
                        } else {
                            log_error(&addr, "Wrong password from streamer");
                            IdentifiedResult::WrongPassword {}
                        };

                        let response = assistant_locked.create_identified_message(result);
                        let is_identified = assistant_locked.identified;
                        drop(assistant_locked);

                        let mut write = writer.lock().await;
                        if let Err(e) = write.send(Message::Text(
                            serde_json::to_string(&response)
                                .expect("Failed to serialize identified response"),
                        ))
                        .await
                        {
                            log_error(&addr, &format!("Error sending identified: {}", e));
                            break;
                        }
                        log(&addr, "Sent identified response");
                        drop(write);

                        // If identified successfully, spawn a task to send delayed chat messages
                        // without blocking the message read loop
                        if is_identified {
                            log(&addr, "Spawning delayed chat message task");
                            tokio::spawn(send_delayed_chat_messages(
                                writer.clone(),
                                assistant.clone(),
                                addr.clone(),
                            ));
                        }
                    } else {
                        log_error(&addr, "Identify message missing authentication field");
                    }
                }

                // Handle ping message
                if json_msg.get("ping").is_some() {
                    let assistant_locked = assistant.lock().await;
                    let pong = assistant_locked.create_pong_message();
                    drop(assistant_locked);

                    let mut write = writer.lock().await;
                    if let Err(e) = write
                        .send(Message::Text(
                            serde_json::to_string(&pong)
                                .expect("Failed to serialize pong message"),
                        ))
                        .await
                    {
                        log_error(&addr, &format!("Error sending pong: {}", e));
                        break;
                    }
                    log(&addr, "Sent pong response");
                }

                // Handle event messages
                if json_msg.get("event").is_some() {
                    log(&addr, "Received event from streamer");
                }

                // Handle response messages
                if json_msg.get("response").is_some() {
                    log(&addr, "Received response from streamer");
                }
            }
            Message::Ping(data) => {
                log(&addr, "Received WebSocket ping");
                let mut write = writer.lock().await;
                if let Err(e) = write.send(Message::Pong(data)).await {
                    log_error(&addr, &format!("Error sending WebSocket pong: {}", e));
                    break;
                }
                log(&addr, "Sent WebSocket pong");
            }
            Message::Close(_) => {
                log(&addr, "Connection closed by streamer");
                break;
            }
            _ => {
                log(&addr, "Received unhandled message type");
            }
        }
    }

    log(&addr, "Streamer disconnected");
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    println!(
        "[{}] Starting Moblin Assistant server on port {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        args.port
    );

    let listener = TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind");

    println!(
        "[{}] Server listening on 0.0.0.0:{}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        args.port
    );

    loop {
        let (stream, addr) = listener.accept().await.expect("Failed to accept connection");
        println!(
            "[{}] Accepted TCP connection from {}",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
            addr
        );
        let assistant = Arc::new(Mutex::new(Assistant::new(args.password.clone())));
        tokio::spawn(handle_streamer_connection(stream, assistant));
    }
}
