use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
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
struct YouTubeStartData {
    video_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum IncomingMessage {
    Identify(IdentifyData),
    Ping(serde_json::Value),
    Event(serde_json::Value),
    TwitchStart(TwitchStartData),
    #[serde(rename = "youTubeStart")]
    YouTubeStart(YouTubeStartData),
    Response(serde_json::Value),
}

#[derive(Debug, Serialize, Deserialize)]
struct PongMessage {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum OutgoingMessage {
    Hello(HelloMessage),
    Identified(IdentifiedMessage),
    Pong(PongMessage),
    Request(RequestMessage),
}

struct Streamer {
    password: String,
    challenge: String,
    salt: String,
    identified: bool,
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

    fn next_id(&mut self) -> i32 {
        self.request_id += 1;
        self.request_id
    }

    fn next_chat_message_id(&mut self) -> i32 {
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

// YouTube Live Chat API structures

const YOUTUBE_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:124.0) Gecko/20100101 Firefox/124.0";

const YOUTUBE_MIN_POLL_DELAY_MS: u64 = 200;
const YOUTUBE_MAX_POLL_DELAY_MS: u64 = 3000;
const YOUTUBE_RECONNECT_DELAY_SECS: u64 = 5;

#[derive(Debug, Deserialize)]
struct YtThumbnail {
    url: String,
}

#[derive(Debug, Deserialize)]
struct YtImage {
    thumbnails: Vec<YtThumbnail>,
}

#[derive(Debug, Deserialize)]
struct YtEmoji {
    image: YtImage,
}

#[derive(Debug, Deserialize)]
struct YtRun {
    text: Option<String>,
    emoji: Option<YtEmoji>,
}

#[derive(Debug, Deserialize)]
struct YtMessage {
    runs: Vec<YtRun>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtAuthor {
    simple_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtAmount {
    #[allow(dead_code)]
    simple_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtBadgeIcon {
    icon_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct YtAuthorBadgeRenderer {
    icon: Option<YtBadgeIcon>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtAuthorBadge {
    live_chat_author_badge_renderer: Option<YtAuthorBadgeRenderer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtChatDescription {
    author_name: YtAuthor,
    message: Option<YtMessage>,
    #[allow(dead_code)]
    purchase_amount_text: Option<YtAmount>,
    header_subtext: Option<YtMessage>,
    author_badges: Option<Vec<YtAuthorBadge>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtAddChatItemActionItem {
    live_chat_text_message_renderer: Option<YtChatDescription>,
    live_chat_paid_message_renderer: Option<YtChatDescription>,
    live_chat_paid_sticker_renderer: Option<YtChatDescription>,
    live_chat_membership_item_renderer: Option<YtChatDescription>,
}

#[derive(Debug, Deserialize)]
struct YtAddChatItemAction {
    item: YtAddChatItemActionItem,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtAction {
    add_chat_item_action: Option<YtAddChatItemAction>,
}

#[derive(Debug, Deserialize)]
struct YtInvalidationContinuationData {
    continuation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtContinuations {
    invalidation_continuation_data: Option<YtInvalidationContinuationData>,
}

#[derive(Debug, Deserialize)]
struct YtLiveChatContinuation {
    continuations: Vec<YtContinuations>,
    actions: Option<Vec<YtAction>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtContinuationContents {
    live_chat_continuation: YtLiveChatContinuation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YtGetLiveChat {
    continuation_contents: YtContinuationContents,
}

fn create_youtube_segments(chat_description: &YtChatDescription) -> Vec<ChatPostSegment> {
    let mut segments: Vec<ChatPostSegment> = Vec::new();
    let mut id: i32 = 0;

    if let Some(header_subtext) = &chat_description.header_subtext {
        for run in &header_subtext.runs {
            if let Some(text) = &run.text {
                for word in text.split_whitespace() {
                    segments.push(ChatPostSegment {
                        id,
                        text: Some(format!("{word} ")),
                        url: None,
                    });
                    id += 1;
                }
            }
            if let Some(emoji) = &run.emoji {
                if let Some(thumbnail) = emoji.image.thumbnails.first() {
                    segments.push(ChatPostSegment {
                        id,
                        text: None,
                        url: Some(thumbnail.url.clone()),
                    });
                    id += 1;
                }
            }
        }
    }

    if let Some(message) = &chat_description.message {
        for run in &message.runs {
            if let Some(text) = &run.text {
                for word in text.split_whitespace() {
                    segments.push(ChatPostSegment {
                        id,
                        text: Some(format!("{word} ")),
                        url: None,
                    });
                    id += 1;
                }
            }
            if let Some(emoji) = &run.emoji {
                if let Some(thumbnail) = emoji.image.thumbnails.first() {
                    segments.push(ChatPostSegment {
                        id,
                        text: None,
                        url: Some(thumbnail.url.clone()),
                    });
                    id += 1;
                }
            }
        }
    }

    segments
}

fn is_youtube_owner(chat_description: &YtChatDescription) -> bool {
    chat_description
        .author_badges
        .as_ref()
        .map(|badges| {
            badges.iter().any(|b| {
                b.live_chat_author_badge_renderer
                    .as_ref()
                    .and_then(|r| r.icon.as_ref())
                    .and_then(|i| i.icon_type.as_ref())
                    .is_some_and(|t| t == "OWNER")
            })
        })
        .unwrap_or(false)
}

fn is_youtube_moderator(chat_description: &YtChatDescription) -> bool {
    chat_description
        .author_badges
        .as_ref()
        .map(|badges| {
            badges.iter().any(|b| {
                b.live_chat_author_badge_renderer
                    .as_ref()
                    .and_then(|r| r.icon.as_ref())
                    .and_then(|i| i.icon_type.as_ref())
                    .is_some_and(|t| t == "MODERATOR")
            })
        })
        .unwrap_or(false)
}

fn is_youtube_member(chat_description: &YtChatDescription) -> bool {
    chat_description
        .author_badges
        .as_ref()
        .map(|badges| {
            badges.iter().any(|b| {
                b.live_chat_author_badge_renderer
                    .as_ref()
                    .and_then(|r| r.icon.as_ref())
                    .and_then(|i| i.icon_type.as_ref())
                    .is_some_and(|t| t == "MEMBER")
            })
        })
        .unwrap_or(false)
}

async fn youtube_get_initial_continuation(
    client: &reqwest::Client,
    video_id: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("https://www.youtube.com/live_chat?is_popout=1&v={video_id}");
    let response = client
        .get(&url)
        .header("User-Agent", YOUTUBE_USER_AGENT)
        .header("Cookie", "CONSENT=YES+1")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("YouTube live chat page returned status {}", response.status()).into());
    }

    let body = response.text().await?;
    let re = regex::Regex::new(r#""continuation":"([^"]+)""#)?;
    let captures = re
        .captures(&body)
        .ok_or("No continuation token found in YouTube live chat page")?;
    Ok(captures[1].to_string())
}

async fn youtube_fetch_messages(
    client: &reqwest::Client,
    continuation: &str,
) -> Result<YtGetLiveChat, Box<dyn std::error::Error + Send + Sync>> {
    let url = "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat";
    let body = serde_json::json!({
        "context": {
            "client": {
                "clientName": "WEB",
                "clientVersion": "2.20210128.02.00"
            }
        },
        "continuation": continuation
    });

    let response = client
        .post(url)
        .header("User-Agent", YOUTUBE_USER_AGENT)
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("YouTube get_live_chat returned status {}", response.status()).into());
    }

    let data: YtGetLiveChat = response.json().await?;
    Ok(data)
}

async fn connect_youtube_chat(
    writer: WsWriter,
    streamer: Arc<Mutex<Streamer>>,
    video_id: String,
    peer_address: String,
) {
    info!("[{peer_address}] Starting YouTube chat for video: {video_id}");

    let client = reqwest::Client::new();

    loop {
        match youtube_chat_session(&client, &writer, &streamer, &video_id, &peer_address).await {
            Ok(()) => {
                debug!("[{peer_address}] YouTube chat session ended normally");
                break;
            }
            Err(e) => {
                error!("[{peer_address}] YouTube chat error: {e}, reconnecting in {YOUTUBE_RECONNECT_DELAY_SECS}s");
                tokio::time::sleep(tokio::time::Duration::from_secs(YOUTUBE_RECONNECT_DELAY_SECS))
                    .await;
            }
        }
    }

    debug!("[{peer_address}] YouTube chat connection ended");
}

async fn youtube_chat_session(
    client: &reqwest::Client,
    writer: &WsWriter,
    streamer: &Arc<Mutex<Streamer>>,
    video_id: &str,
    peer_address: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut continuation = youtube_get_initial_continuation(client, video_id).await?;
    info!("[{peer_address}] Got YouTube continuation token, starting message polling");

    let mut delay_ms = YOUTUBE_MAX_POLL_DELAY_MS;

    loop {
        let live_chat = youtube_fetch_messages(client, &continuation).await?;

        let mut message_count = 0;

        if let Some(actions) = &live_chat.continuation_contents.live_chat_continuation.actions {
            for action in actions {
                let item = match &action.add_chat_item_action {
                    Some(a) => &a.item,
                    None => continue,
                };

                let descriptions: Vec<&YtChatDescription> = [
                    item.live_chat_text_message_renderer.as_ref(),
                    item.live_chat_paid_message_renderer.as_ref(),
                    item.live_chat_paid_sticker_renderer.as_ref(),
                    item.live_chat_membership_item_renderer.as_ref(),
                ]
                .into_iter()
                .flatten()
                .collect();

                for chat_description in descriptions {
                    let segments = create_youtube_segments(chat_description);
                    if segments.is_empty() {
                        continue;
                    }

                    let is_owner = is_youtube_owner(chat_description);
                    let is_moderator = is_youtube_moderator(chat_description);
                    let is_subscriber = is_youtube_member(chat_description);
                    let display_name = chat_description.author_name.simple_text.clone();

                    let mut streamer_lock = streamer.lock().await;
                    let chat_message_id = streamer_lock.next_chat_message_id();
                    let request_id = streamer_lock.next_id();
                    drop(streamer_lock);

                    let chat_message = ChatMessage {
                        id: chat_message_id,
                        platform: Platform::YouTube {},
                        message_id: None,
                        display_name: Some(display_name.clone()),
                        user: Some(display_name),
                        user_id: None,
                        user_color: None,
                        user_badges: vec![],
                        segments,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        is_action: false,
                        is_moderator,
                        is_subscriber,
                        is_owner,
                        bits: None,
                    };

                    let request = OutgoingMessage::Request(RequestMessage {
                        id: request_id,
                        data: RequestData::ChatMessages(ChatMessagesRequest {
                            history: false,
                            messages: vec![chat_message],
                        }),
                    });

                    if let Ok(encoded) = serde_json::to_string(&request) {
                        debug!("[{peer_address}] Forwarding YouTube chat message: {encoded}");
                        let mut w = writer.lock().await;
                        if let Err(e) = w.send(Message::Text(encoded)).await {
                            error!("[{peer_address}] Error forwarding YouTube chat message: {e}");
                            return Ok(());
                        }
                    }

                    message_count += 1;
                }
            }
        }

        // Update continuation
        let new_continuation = live_chat
            .continuation_contents
            .live_chat_continuation
            .continuations
            .iter()
            .find_map(|c| {
                c.invalidation_continuation_data
                    .as_ref()
                    .map(|d| d.continuation.clone())
            })
            .ok_or("No continuation token in YouTube response")?;
        continuation = new_continuation;

        // Adaptive polling delay
        if message_count > 0 {
            delay_ms = delay_ms * 5 / message_count as u64;
        } else {
            delay_ms = YOUTUBE_MAX_POLL_DELAY_MS;
        }
        delay_ms = delay_ms.clamp(YOUTUBE_MIN_POLL_DELAY_MS, YOUTUBE_MAX_POLL_DELAY_MS);

        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
    }
}

async fn connect_twitch_irc(
    writer: WsWriter,
    streamer: Arc<Mutex<Streamer>>,
    channel_name: String,
    peer_address: String,
) {
    info!("[{peer_address}] Connecting to Twitch IRC for channel: {channel_name}");

    let credentials = StaticLoginCredentials::anonymous();
    let config = ClientConfig::new_simple(credentials);
    let (mut messages, client) =
        TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

    if let Err(e) = client.join(channel_name.clone()) {
        error!("[{peer_address}] Failed to join Twitch channel '{channel_name}': {e}");
        return;
    }

    info!("[{peer_address}] Joined Twitch channel #{channel_name}");

    while let Some(message) = messages.recv().await {
        if let ServerMessage::Privmsg(message) = message {
            let user_color = message.name_color.map(|c| RgbColor {
                red: c.r as i32,
                green: c.g as i32,
                blue: c.b as i32,
            });

            let user_badges: Vec<String> = message
                .badges
                .iter()
                .map(|b| format!("{}/{}", b.name, b.version))
                .collect();

            let is_moderator = message.badges.iter().any(|b| b.name == "moderator");
            let is_subscriber = message.badges.iter().any(|b| b.name == "subscriber");
            let is_owner = message.badges.iter().any(|b| b.name == "broadcaster");

            let mut streamer = streamer.lock().await;
            let chat_message_id = streamer.next_chat_message_id();
            let request_id = streamer.next_id();
            drop(streamer);

            let chat_message = ChatMessage {
                id: chat_message_id,
                platform: Platform::Twitch {},
                message_id: Some(message.message_id.clone()),
                display_name: Some(message.sender.name.clone()),
                user: Some(message.sender.login.clone()),
                user_id: Some(message.sender.id.clone()),
                user_color,
                user_badges,
                segments: create_twitch_segments(&message.message_text, &message.emotes),
                timestamp: message.server_timestamp.to_rfc3339(),
                is_action: message.is_action,
                is_moderator,
                is_subscriber,
                is_owner,
                bits: message.bits.map(|b| b.to_string()),
            };

            let request = OutgoingMessage::Request(RequestMessage {
                id: request_id,
                data: RequestData::ChatMessages(ChatMessagesRequest {
                    history: false,
                    messages: vec![chat_message],
                }),
            });

            if let Ok(encoded) = serde_json::to_string(&request) {
                debug!("[{peer_address}] Forwarding Twitch chat message: {encoded}");
                let mut writer = writer.lock().await;
                if let Err(e) = writer.send(Message::Text(encoded)).await {
                    error!("[{peer_address}] Error forwarding chat message: {e}");
                    return;
                }
            }
        }
    }

    debug!("[{peer_address}] Twitch IRC connection ended");
}

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
    debug!("[{peer_address}] Received twitchStart message");

    if let Some(channel_name) = &twitch_start.channel_name {
        info!("[{peer_address}] Starting Twitch IRC connection for channel: {channel_name}");
        tokio::spawn(connect_twitch_irc(
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
    debug!("[{peer_address}] Received youTubeStart message");

    info!(
        "[{peer_address}] Starting YouTube chat for video: {}",
        youtube_start.video_id
    );
    tokio::spawn(connect_youtube_chat(
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

    #[test]
    fn test_deserialize_youtube_start_message() {
        let json = r#"{"youTubeStart": {"videoId": "abc123"}}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::YouTubeStart(data) => {
                assert_eq!(data.video_id, "abc123");
            }
            _ => panic!("Expected YouTubeStart variant"),
        }
    }

    #[test]
    fn test_youtube_segments_text_only() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "TestUser".to_string(),
            },
            message: Some(YtMessage {
                runs: vec![YtRun {
                    text: Some("hello world".to_string()),
                    emoji: None,
                }],
            }),
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text.as_deref(), Some("hello "));
        assert_eq!(segments[1].text.as_deref(), Some("world "));
    }

    #[test]
    fn test_youtube_segments_with_emoji() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "TestUser".to_string(),
            },
            message: Some(YtMessage {
                runs: vec![
                    YtRun {
                        text: Some("hi ".to_string()),
                        emoji: None,
                    },
                    YtRun {
                        text: None,
                        emoji: Some(YtEmoji {
                            image: YtImage {
                                thumbnails: vec![YtThumbnail {
                                    url: "https://example.com/emoji.png".to_string(),
                                }],
                            },
                        }),
                    },
                    YtRun {
                        text: Some(" bye".to_string()),
                        emoji: None,
                    },
                ],
            }),
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text.as_deref(), Some("hi "));
        assert!(segments[0].url.is_none());
        assert!(segments[1].text.is_none());
        assert_eq!(
            segments[1].url.as_deref(),
            Some("https://example.com/emoji.png")
        );
        assert_eq!(segments[2].text.as_deref(), Some("bye "));
    }

    #[test]
    fn test_youtube_segments_empty_message() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "TestUser".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert!(segments.is_empty());
    }

    #[test]
    fn test_youtube_owner_detection() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "Owner".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: Some(vec![YtAuthorBadge {
                live_chat_author_badge_renderer: Some(YtAuthorBadgeRenderer {
                    icon: Some(YtBadgeIcon {
                        icon_type: Some("OWNER".to_string()),
                    }),
                }),
            }]),
        };
        assert!(is_youtube_owner(&desc));
        assert!(!is_youtube_moderator(&desc));
        assert!(!is_youtube_member(&desc));
    }

    #[test]
    fn test_youtube_moderator_detection() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "Mod".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: Some(vec![YtAuthorBadge {
                live_chat_author_badge_renderer: Some(YtAuthorBadgeRenderer {
                    icon: Some(YtBadgeIcon {
                        icon_type: Some("MODERATOR".to_string()),
                    }),
                }),
            }]),
        };
        assert!(!is_youtube_owner(&desc));
        assert!(is_youtube_moderator(&desc));
    }

    #[test]
    fn test_youtube_no_badges() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "User".to_string(),
            },
            message: None,
            purchase_amount_text: None,
            header_subtext: None,
            author_badges: None,
        };
        assert!(!is_youtube_owner(&desc));
        assert!(!is_youtube_moderator(&desc));
        assert!(!is_youtube_member(&desc));
    }

    #[test]
    fn test_youtube_segments_header_subtext() {
        let desc = YtChatDescription {
            author_name: YtAuthor {
                simple_text: "TestUser".to_string(),
            },
            message: Some(YtMessage {
                runs: vec![YtRun {
                    text: Some("main message".to_string()),
                    emoji: None,
                }],
            }),
            purchase_amount_text: None,
            header_subtext: Some(YtMessage {
                runs: vec![YtRun {
                    text: Some("header".to_string()),
                    emoji: None,
                }],
            }),
            author_badges: None,
        };
        let segments = create_youtube_segments(&desc);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text.as_deref(), Some("header "));
        assert_eq!(segments[1].text.as_deref(), Some("main "));
        assert_eq!(segments[2].text.as_deref(), Some("message "));
    }

    #[test]
    fn test_deserialize_youtube_live_chat_response() {
        let json = r#"{
            "continuationContents": {
                "liveChatContinuation": {
                    "continuations": [{
                        "invalidationContinuationData": {
                            "continuation": "next_token"
                        }
                    }],
                    "actions": [{
                        "addChatItemAction": {
                            "item": {
                                "liveChatTextMessageRenderer": {
                                    "authorName": {"simpleText": "TestUser"},
                                    "message": {
                                        "runs": [{"text": "Hello!"}]
                                    }
                                }
                            }
                        }
                    }]
                }
            }
        }"#;
        let data: YtGetLiveChat = serde_json::from_str(json).unwrap();
        let continuation = &data
            .continuation_contents
            .live_chat_continuation
            .continuations[0];
        assert_eq!(
            continuation
                .invalidation_continuation_data
                .as_ref()
                .unwrap()
                .continuation,
            "next_token"
        );
        let actions = data
            .continuation_contents
            .live_chat_continuation
            .actions
            .as_ref()
            .unwrap();
        assert_eq!(actions.len(), 1);
        let item = &actions[0].add_chat_item_action.as_ref().unwrap().item;
        let renderer = item.live_chat_text_message_renderer.as_ref().unwrap();
        assert_eq!(renderer.author_name.simple_text, "TestUser");
    }
}
