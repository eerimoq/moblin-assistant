use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Parser;
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
struct Platform {
    #[serde(rename = "type")]
    platform_type: String,
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
}

impl Assistant {
    fn new(password: String) -> Self {
        Self {
            password,
            challenge: random_string(),
            salt: random_string(),
            identified: false,
            request_id: 0,
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
            id: 1,
            platform: Platform {
                platform_type: "assistant".to_string(),
            },
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

async fn handle_streamer_connection(
    stream: tokio::net::TcpStream,
    assistant: Arc<Mutex<Assistant>>,
) {
    let addr = stream
        .peer_addr()
        .expect("Failed to get peer address")
        .to_string();
    println!("New streamer connection from: {}", addr);

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("Error during WebSocket handshake: {}", e);
            return;
        }
    };

    let (mut write, mut read) = ws_stream.split();
    use futures_util::{SinkExt, StreamExt};

    // Send hello message
    {
        let assistant = assistant.lock().await;
        let hello = assistant.create_hello_message();
        if let Err(e) = write
            .send(Message::Text(serde_json::to_string(&hello).unwrap()))
            .await
        {
            eprintln!("Error sending hello: {}", e);
            return;
        }
        println!("Sent hello message");
    }

    // Handle incoming messages
    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Error receiving message: {}", e);
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                println!("Received: {}", text);
                
                let json_msg: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Error parsing JSON: {}", e);
                        continue;
                    }
                };

                // Handle identify message
                if let Some(identify) = json_msg.get("identify") {
                    if let Some(auth) = identify.get("authentication").and_then(|a| a.as_str()) {
                        let mut assistant_locked = assistant.lock().await;
                        let expected_hash = assistant_locked.hash_password();
                        
                        let result = if assistant_locked.identified {
                            IdentifiedResult::AlreadyIdentified {}
                        } else if auth == expected_hash {
                            assistant_locked.identified = true;
                            println!("Streamer successfully identified");
                            IdentifiedResult::Ok {}
                        } else {
                            eprintln!("Wrong password from streamer");
                            IdentifiedResult::WrongPassword {}
                        };

                        let response = assistant_locked.create_identified_message(result);
                        if let Err(e) = write
                            .send(Message::Text(serde_json::to_string(&response).unwrap()))
                            .await
                        {
                            eprintln!("Error sending identified: {}", e);
                            break;
                        }

                        // If identified successfully, send hardcoded chat messages after a delay
                        if assistant_locked.identified {
                            // Send first chat message after 2 seconds
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                            let chat_request = assistant_locked.create_chat_message_request("Hello from Rust Moblin Assistant! 🎉");
                            if let Ok(msg_str) = serde_json::to_string(&chat_request) {
                                println!("Sending chat message: {}", msg_str);
                                if let Err(e) = write.send(Message::Text(msg_str)).await {
                                    eprintln!("Error sending chat message: {}", e);
                                    break;
                                }
                            }
                            
                            // Send second message after 3 more seconds
                            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                            let chat_request2 = assistant_locked.create_chat_message_request("This is a second hardcoded message!");
                            if let Ok(msg_str) = serde_json::to_string(&chat_request2) {
                                println!("Sending second chat message: {}", msg_str);
                                if let Err(e) = write.send(Message::Text(msg_str)).await {
                                    eprintln!("Error sending second chat message: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }

                // Handle ping message
                if json_msg.get("ping").is_some() {
                    let assistant_locked = assistant.lock().await;
                    let pong = assistant_locked.create_pong_message();
                    if let Err(e) = write
                        .send(Message::Text(serde_json::to_string(&pong).unwrap()))
                        .await
                    {
                        eprintln!("Error sending pong: {}", e);
                        break;
                    }
                    println!("Sent pong message");
                }

                // Handle event messages (just log them)
                if let Some(_event) = json_msg.get("event") {
                    println!("Received event from streamer");
                }

                // Handle response messages (just log them)
                if let Some(_response) = json_msg.get("response") {
                    println!("Received response from streamer");
                }
            }
            Message::Ping(data) => {
                if let Err(e) = write.send(Message::Pong(data)).await {
                    eprintln!("Error sending pong: {}", e);
                    break;
                }
            }
            Message::Close(_) => {
                println!("Connection closed by streamer");
                break;
            }
            _ => {}
        }
    }

    println!("Streamer disconnected: {}", addr);
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    println!("Starting Moblin Assistant server on port {}", args.port);
    println!("Password: {}", args.password);

    let listener = TcpListener::bind(format!("0.0.0.0:{}", args.port))
        .await
        .expect("Failed to bind");

    println!("Server listening on 0.0.0.0:{}", args.port);

    loop {
        let (stream, _) = listener.accept().await.expect("Failed to accept connection");
        let assistant = Arc::new(Mutex::new(Assistant::new(args.password.clone())));
        tokio::spawn(handle_streamer_connection(stream, assistant));
    }
}
