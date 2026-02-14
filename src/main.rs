use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{Emote, ServerMessage};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

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
    #[allow(clippy::enum_variant_names)]
    OpenStreamingPlatform {},
    Twitch {},
    YouTube {},
    #[serde(rename = "dlive")]
    DLive {},
}

#[derive(Debug, Serialize, Deserialize)]
struct RgbColor {
    red: i32,
    green: i32,
    blue: i32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatPostSegment {
    id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
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
#[serde(rename_all = "camelCase")]
enum RequestData {
    ChatMessages(ChatMessagesRequest),
}

#[derive(Debug, Serialize, Deserialize)]
struct RequestMessage {
    id: i32,
    data: RequestData,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentifyData {
    authentication: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TwitchStartData {
    channel_name: Option<String>,
    channel_id: String,
    access_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum IncomingMessage {
    Identify(IdentifyData),
    Ping(serde_json::Value),
    Event(serde_json::Value),
    TwitchStart(TwitchStartData),
    Response(serde_json::Value),
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

    fn create_hello_message(&self) -> HelloMessage {
        HelloMessage {
            api_version: API_VERSION.to_string(),
            authentication: Authentication {
                challenge: self.challenge.clone(),
                salt: self.salt.clone(),
            },
        }
    }

    fn create_identified_message(&self, result: IdentifiedResult) -> IdentifiedMessage {
        IdentifiedMessage { result }
    }

    fn create_pong_message(&self) -> serde_json::Value {
        json!({
            "pong": {}
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

fn create_twitch_segments(message_text: &str, emotes: &[Emote]) -> Vec<ChatPostSegment> {
    let mut segments: Vec<ChatPostSegment> = Vec::new();
    let mut id: i32 = 0;
    let chars: Vec<char> = message_text.chars().collect();
    let mut sorted_emotes: Vec<&Emote> = emotes.iter().collect();
    sorted_emotes.sort_by_key(|e| e.char_range.start);
    let mut start_index: usize = 0;
    for emote in &sorted_emotes {
        if emote.char_range.start >= chars.len() {
            break;
        }
        let emote_end = emote.char_range.end.min(chars.len());
        if emote.char_range.start > start_index {
            let text_before: String = chars[start_index..emote.char_range.start].iter().collect();
            for word in text_before.split_whitespace() {
                segments.push(ChatPostSegment {
                    id,
                    text: Some(format!("{word} ")),
                    url: None,
                });
                id += 1;
            }
        }
        let emote_url = format!(
            "https://static-cdn.jtvnw.net/emoticons/v2/{}/default/dark/3.0",
            emote.id
        );
        segments.push(ChatPostSegment {
            id,
            text: None,
            url: Some(emote_url),
        });
        id += 1;
        // Empty text spacer segment after emote, matching Moblin's protocol
        segments.push(ChatPostSegment {
            id,
            text: Some(String::new()),
            url: None,
        });
        id += 1;
        start_index = emote_end;
    }
    if start_index < chars.len() {
        let remaining: String = chars[start_index..].iter().collect();
        for word in remaining.split_whitespace() {
            segments.push(ChatPostSegment {
                id,
                text: Some(format!("{word} ")),
                url: None,
            });
            id += 1;
        }
    }
    if segments.is_empty() {
        segments.push(ChatPostSegment {
            id,
            text: Some(message_text.to_string()),
            url: None,
        });
    }
    segments
}

async fn connect_twitch_irc(
    writer: WsWriter,
    assistant: Arc<Mutex<Assistant>>,
    channel_name: String,
    addr: String,
) {
    info!("[{addr}] Connecting to Twitch IRC for channel: {channel_name}");

    // Use anonymous credentials since we only need to read chat messages
    let credentials = StaticLoginCredentials::anonymous();
    let config = ClientConfig::new_simple(credentials);
    let (mut incoming_messages, client) =
        TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

    // twitch-irc crate expects channel names without '#' prefix, in lowercase
    let channel = channel_name.trim_start_matches('#').to_lowercase();
    if let Err(e) = client.join(channel.clone()) {
        error!("[{addr}] Failed to join Twitch channel '{channel}': {e}");
        return;
    }

    info!("[{addr}] Joined Twitch channel #{channel}");

    while let Some(message) = incoming_messages.recv().await {
        if let ServerMessage::Privmsg(msg) = message {
            let user_color = msg.name_color.map(|c| RgbColor {
                red: c.r as i32,
                green: c.g as i32,
                blue: c.b as i32,
            });

            let user_badges: Vec<String> = msg
                .badges
                .iter()
                .map(|b| format!("{}/{}", b.name, b.version))
                .collect();

            let is_moderator = msg.badges.iter().any(|b| b.name == "moderator");
            let is_subscriber = msg.badges.iter().any(|b| b.name == "subscriber");
            let is_owner = msg.badges.iter().any(|b| b.name == "broadcaster");

            let mut assistant = assistant.lock().await;
            let chat_message_id = assistant.next_chat_message_id();
            let request_id = assistant.next_id();
            drop(assistant);

            let chat_msg = ChatMessage {
                id: chat_message_id,
                platform: Platform::Twitch {},
                message_id: Some(msg.message_id.clone()),
                display_name: Some(msg.sender.name.clone()),
                user: Some(msg.sender.login.clone()),
                user_id: Some(msg.sender.id.clone()),
                user_color,
                user_badges,
                segments: create_twitch_segments(&msg.message_text, &msg.emotes),
                timestamp: msg.server_timestamp.to_rfc3339(),
                is_action: msg.is_action,
                is_moderator,
                is_subscriber,
                is_owner,
                bits: msg.bits.map(|b| b.to_string()),
            };

            let chat_request = RequestMessage {
                id: request_id,
                data: RequestData::ChatMessages(ChatMessagesRequest {
                    history: false,
                    messages: vec![chat_msg],
                }),
            };

            if let Ok(msg_str) = serde_json::to_string(&chat_request) {
                debug!("[{addr}] Forwarding Twitch chat message: {msg_str}");
                let mut write = writer.lock().await;
                if let Err(e) = write.send(Message::Text(msg_str)).await {
                    error!("[{addr}] Error forwarding chat message: {e}");
                    return;
                }
            }
        }
    }

    debug!("[{addr}] Twitch IRC connection ended");
}

async fn handle_identify_message(
    identify: &IdentifyData,
    writer: &WsWriter,
    assistant: &Arc<Mutex<Assistant>>,
    addr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("[{addr}] Processing identify message");
    let mut assistant = assistant.lock().await;
    let expected_hash = assistant.hash_password();

    let result = if assistant.identified {
        debug!("[{addr}] Streamer already identified");
        IdentifiedResult::AlreadyIdentified {}
    } else if identify.authentication == expected_hash {
        assistant.identified = true;
        info!("[{addr}] Streamer successfully identified");
        IdentifiedResult::Ok {}
    } else {
        error!("[{addr}] Wrong password from streamer");
        IdentifiedResult::WrongPassword {}
    };

    let response = assistant.create_identified_message(result);
    let is_identified = assistant.identified;
    drop(assistant);

    let mut write = writer.lock().await;
    write
        .send(Message::Text(
            serde_json::to_string(&response).expect("Failed to serialize identified response"),
        ))
        .await?;
    debug!("[{addr}] Sent identified response");
    drop(write);

    // If identified successfully, start processing Twitch messages
    if is_identified {
        debug!("[{addr}] Streamer identified, waiting for twitchStart message");
    }
    Ok(())
}

async fn handle_ping_message(
    writer: &WsWriter,
    assistant: &Arc<Mutex<Assistant>>,
    addr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let assistant = assistant.lock().await;
    let pong = assistant.create_pong_message();
    drop(assistant);

    let mut write = writer.lock().await;
    write
        .send(Message::Text(
            serde_json::to_string(&pong).expect("Failed to serialize pong message"),
        ))
        .await?;
    debug!("[{addr}] Sent pong response");
    Ok(())
}

async fn handle_event_message(addr: &str) {
    debug!("[{addr}] Received event from streamer");
}

async fn handle_twitch_start_message(
    twitch_start: &TwitchStartData,
    writer: &WsWriter,
    assistant: &Arc<Mutex<Assistant>>,
    addr: &str,
) {
    debug!("[{addr}] Received twitchStart message");

    if let Some(channel) = &twitch_start.channel_name {
        info!("[{addr}] Starting Twitch IRC connection for channel: {channel}");
        tokio::spawn(connect_twitch_irc(
            writer.clone(),
            assistant.clone(),
            channel.clone(),
            addr.to_string(),
        ));
    }
}

async fn handle_response_message(addr: &str) {
    debug!("[{addr}] Received response from streamer");
}

async fn is_identified(assistant: &Arc<Mutex<Assistant>>) -> bool {
    assistant.lock().await.identified
}

async fn handle_websocket_ping(
    data: Vec<u8>,
    writer: &WsWriter,
    addr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("[{addr}] Received WebSocket ping");
    let mut write = writer.lock().await;
    write.send(Message::Pong(data)).await?;
    debug!("[{addr}] Sent WebSocket pong");
    Ok(())
}

async fn handle_streamer_connection(
    stream: tokio::net::TcpStream,
    assistant: Arc<Mutex<Assistant>>,
) {
    let addr = stream
        .peer_addr()
        .expect("Failed to get peer address")
        .to_string();
    info!("[{addr}] New streamer connection");

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => {
            debug!("[{addr}] WebSocket handshake completed");
            ws
        }
        Err(e) => {
            error!("[{addr}] Error during WebSocket handshake: {e}");
            return;
        }
    };

    let (write, mut read) = ws_stream.split();
    let writer: WsWriter = Arc::new(Mutex::new(write));

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
            error!("[{addr}] Error sending hello: {e}");
            return;
        }
        debug!("[{addr}] Sent hello message with authentication challenge");
    }

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                error!("[{addr}] Error receiving message: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                debug!("[{addr}] Received message: {text}");

                let incoming: IncomingMessage = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("[{addr}] Error parsing JSON: {e}");
                        continue;
                    }
                };

                match &incoming {
                    IncomingMessage::Identify(identify) => {
                        if let Err(e) =
                            handle_identify_message(identify, &writer, &assistant, &addr).await
                        {
                            error!("[{addr}] Error handling identify message: {e}");
                            break;
                        }
                    }
                    IncomingMessage::Ping(_) => {
                        if let Err(e) = handle_ping_message(&writer, &assistant, &addr).await {
                            error!("[{addr}] Error handling ping message: {e}");
                            break;
                        }
                    }
                    IncomingMessage::Event(_) => {
                        if !is_identified(&assistant).await {
                            break;
                        }
                        handle_event_message(&addr).await;
                    }
                    IncomingMessage::TwitchStart(twitch_start) => {
                        if !is_identified(&assistant).await {
                            break;
                        }
                        handle_twitch_start_message(twitch_start, &writer, &assistant, &addr).await;
                    }
                    IncomingMessage::Response(_) => {
                        if !is_identified(&assistant).await {
                            break;
                        }
                        handle_response_message(&addr).await;
                    }
                }
            }
            Message::Ping(data) => {
                if let Err(e) = handle_websocket_ping(data, &writer, &addr).await {
                    error!("[{addr}] Error handling WebSocket ping: {e}");
                    break;
                }
            }
            Message::Close(_) => {
                debug!("[{addr}] Connection closed by streamer");
                break;
            }
            _ => {
                debug!("[{addr}] Received unhandled message type");
            }
        }
    }

    info!("[{addr}] Streamer disconnected");
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
        let assistant = Arc::new(Mutex::new(Assistant::new(args.password.clone())));
        tokio::spawn(handle_streamer_connection(stream, assistant));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_irc::message::Emote;

    #[test]
    fn test_no_emotes() {
        let segments = create_twitch_segments("hello world", &[]);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text.as_deref(), Some("hello "));
        assert!(segments[0].url.is_none());
        assert_eq!(segments[1].text.as_deref(), Some("world "));
        assert!(segments[1].url.is_none());
    }

    #[test]
    fn test_single_emote_only() {
        let emotes = vec![Emote {
            id: "25".to_string(),
            char_range: 0..5,
            code: "Kappa".to_string(),
        }];
        let segments = create_twitch_segments("Kappa", &emotes);
        assert_eq!(segments.len(), 2);
        assert!(segments[0].text.is_none());
        assert_eq!(
            segments[0].url.as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/3.0")
        );
        assert_eq!(segments[1].text.as_deref(), Some(""));
    }

    #[test]
    fn test_text_before_and_after_emote() {
        let emotes = vec![Emote {
            id: "25".to_string(),
            char_range: 6..11,
            code: "Kappa".to_string(),
        }];
        let segments = create_twitch_segments("hello Kappa world", &emotes);
        // "hello " -> text, Kappa -> url + spacer, " world" -> text
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].text.as_deref(), Some("hello "));
        assert!(segments[1].url.is_some());
        assert_eq!(segments[2].text.as_deref(), Some(""));
        assert_eq!(segments[3].text.as_deref(), Some("world "));
    }

    #[test]
    fn test_multiple_emotes() {
        // "Kappa Keepo Kappa"
        let emotes = vec![
            Emote {
                id: "25".to_string(),
                char_range: 0..5,
                code: "Kappa".to_string(),
            },
            Emote {
                id: "1902".to_string(),
                char_range: 6..11,
                code: "Keepo".to_string(),
            },
            Emote {
                id: "25".to_string(),
                char_range: 12..17,
                code: "Kappa".to_string(),
            },
        ];
        let segments = create_twitch_segments("Kappa Keepo Kappa", &emotes);
        assert_eq!(
            segments[0].url.as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/3.0")
        );
        assert_eq!(
            segments[2].url.as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/1902/default/dark/3.0")
        );
        assert_eq!(
            segments[4].url.as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/3.0")
        );
    }

    #[test]
    fn test_emote_with_unicode() {
        // "👉 <3 test" - emoji is one char, then space, then <3 emote
        let emotes = vec![Emote {
            id: "483".to_string(),
            char_range: 2..4,
            code: "<3".to_string(),
        }];
        let segments = create_twitch_segments("👉 <3 test", &emotes);
        assert_eq!(segments[0].text.as_deref(), Some("👉 "));
        assert!(segments[1].url.is_some());
        assert_eq!(segments[2].text.as_deref(), Some(""));
        assert_eq!(segments[3].text.as_deref(), Some("test "));
    }

    #[test]
    fn test_empty_message() {
        let segments = create_twitch_segments("", &[]);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text.as_deref(), Some(""));
    }

    #[test]
    fn test_emote_with_non_numeric_id() {
        let emotes = vec![Emote {
            id: "300196486_TK".to_string(),
            char_range: 0..8,
            code: "pajaM_TK".to_string(),
        }];
        let segments = create_twitch_segments("pajaM_TK", &emotes);
        assert_eq!(segments.len(), 2);
        assert_eq!(
            segments[0].url.as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/300196486_TK/default/dark/3.0")
        );
    }

    #[test]
    fn test_mixed_text_and_emotes() {
        let emotes = vec![
            Emote {
                id: "25".to_string(),
                char_range: 0..5,
                code: "Kappa".to_string(),
            },
            Emote {
                id: "1902".to_string(),
                char_range: 6..11,
                code: "Keepo".to_string(),
            },
            Emote {
                id: "25".to_string(),
                char_range: 12..17,
                code: "Kappa".to_string(),
            },
            Emote {
                id: "25".to_string(),
                char_range: 18..23,
                code: "Kappa".to_string(),
            },
        ];
        let segments = create_twitch_segments("Kappa Keepo Kappa Kappa test", &emotes);
        let url_count = segments.iter().filter(|s| s.url.is_some()).count();
        assert_eq!(url_count, 4);
        let last = segments.last().unwrap();
        assert_eq!(last.text.as_deref(), Some("test "));
    }

    #[test]
    fn test_deserialize_identify_message() {
        let json = r#"{"identify": {"authentication": "abc123"}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::Identify(data) => {
                assert_eq!(data.authentication, "abc123");
            }
            _ => panic!("Expected Identify variant"),
        }
    }

    #[test]
    fn test_deserialize_ping_message() {
        let json = r#"{"ping": {}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::Ping(_)));
    }

    #[test]
    fn test_deserialize_event_message() {
        let json = r#"{"event": {"type": "someEvent"}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::Event(_)));
    }

    #[test]
    fn test_deserialize_twitch_start_message() {
        let json = r#"{"twitchStart": {"channelName": "mychannel", "channelId": "123", "accessToken": "encrypted"}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::TwitchStart(data) => {
                assert_eq!(data.channel_name.as_deref(), Some("mychannel"));
                assert_eq!(data.channel_id, "123");
                assert_eq!(data.access_token, "encrypted");
            }
            _ => panic!("Expected TwitchStart variant"),
        }
    }

    #[test]
    fn test_deserialize_twitch_start_message_minimal() {
        let json = r#"{"twitchStart": {"channelId": "123", "accessToken": "encrypted"}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::TwitchStart(data) => {
                assert!(data.channel_name.is_none());
                assert_eq!(data.channel_id, "123");
                assert_eq!(data.access_token, "encrypted");
            }
            _ => panic!("Expected TwitchStart variant"),
        }
    }

    #[test]
    fn test_deserialize_response_message() {
        let json = r#"{"response": {"id": 1}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::Response(_)));
    }

    #[test]
    fn test_deserialize_unknown_message() {
        let json = r#"{"unknown": {}}"#;
        let result: Result<IncomingMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
