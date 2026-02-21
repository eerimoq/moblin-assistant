use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use log::{debug, info};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{
    Authentication, ChatMessage, ChatMessagesRequest, HelloMessage, IdentifiedMessage,
    IdentifiedResult, IdentifyMessage, MessageToAssistant, MessageToStreamer, RequestData,
    RequestMessage, TwitchStartMessage, YouTubeStartMessage, API_VERSION,
};
use crate::utils::{random_string, AnyError};
use crate::{twitch, youtube, DisconnectedStreamer, Streamer, Streamers, WebsocketWriter};

pub struct StreamerState {
    me: Weak<Mutex<Self>>,
    writer: Option<WebsocketWriter>,
    request_id: i32,
    chat_message_id: i32,
    chat_messages: VecDeque<ChatMessage>,
    twitch_chat: Option<JoinHandle<()>>,
    youtube_chat: Option<JoinHandle<()>>,
}

impl StreamerState {
    pub fn new(writer: WebsocketWriter) -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                writer: Some(writer),
                request_id: 0,
                chat_message_id: 0,
                chat_messages: VecDeque::new(),
                twitch_chat: None,
                youtube_chat: None,
            })
        })
    }

    pub fn destroy_twitch_chat(&mut self) {
        self.twitch_chat = None
    }

    pub fn destroy_youtube_chat(&mut self) {
        self.youtube_chat = None
    }

    pub fn next_chat_message_id(&mut self) -> i32 {
        self.chat_message_id += 1;
        self.chat_message_id
    }

    pub async fn append_chat_message(&mut self, message: ChatMessage) {
        self.store_chat_message(message.clone());
        let request = MessageToStreamer::Request(RequestMessage {
            id: self.next_request_id(),
            data: RequestData::ChatMessages(ChatMessagesRequest {
                history: false,
                messages: vec![message],
            }),
        });
        self.send(request).await;
    }

    fn next_request_id(&mut self) -> i32 {
        self.request_id += 1;
        self.request_id
    }

    fn store_chat_message(&mut self, message: ChatMessage) {
        if self.chat_messages.len() >= 100 {
            self.chat_messages.pop_front();
        }
        self.chat_messages.push_back(message);
    }

    async fn send_chat_history(&mut self) {
        let request = MessageToStreamer::Request(RequestMessage {
            id: self.next_request_id(),
            data: RequestData::ChatMessages(ChatMessagesRequest {
                history: true,
                messages: self.chat_messages.clone().into(),
            }),
        });
        self.send(request).await;
    }

    async fn send(&self, message: MessageToStreamer) {
        let Some(ref writer) = self.writer else {
            return;
        };
        let Ok(encoded) = serde_json::to_string(&message) else {
            return;
        };
        writer.lock().await.send(Message::Text(encoded)).await.ok();
    }
}

pub struct StreamerConnection {
    me: Weak<Mutex<Self>>,
    password: String,
    peer_address: String,
    writer: WebsocketWriter,
    challenge: String,
    salt: String,
    identified: bool,
    streamer_id: Option<String>,
    streamers: Streamers,
    state: Option<Arc<Mutex<StreamerState>>>,
}

impl StreamerConnection {
    pub fn new(
        password: String,
        peer_address: String,
        writer: WebsocketWriter,
        streamers: Streamers,
    ) -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                password,
                peer_address,
                writer: writer.clone(),
                challenge: random_string(),
                salt: random_string(),
                identified: false,
                state: Some(StreamerState::new(writer)),
                streamer_id: None,
                streamers,
            })
        })
    }

    fn hash_password(&self) -> String {
        let concatenated = format!("{}{}", self.password, self.salt);
        let mut hasher = Sha256::new();
        hasher.update(concatenated.as_bytes());
        let hash1 = BASE64.encode(hasher.finalize());
        let concatenated = format!("{}{}", hash1, self.challenge);
        let mut hasher = Sha256::new();
        hasher.update(concatenated.as_bytes());
        BASE64.encode(hasher.finalize())
    }

    async fn send_hello(&mut self) {
        let hello = MessageToStreamer::Hello(HelloMessage {
            api_version: API_VERSION.to_string(),
            authentication: Authentication {
                challenge: self.challenge.clone(),
                salt: self.salt.clone(),
            },
        });
        self.send(hello).await;
    }

    async fn send(&mut self, message: MessageToStreamer) {
        let Ok(encoded) = serde_json::to_string(&message) else {
            return;
        };
        self.writer
            .lock()
            .await
            .send(Message::Text(encoded))
            .await
            .ok();
    }

    async fn handle_message(&mut self, message: &str) -> Result<(), AnyError> {
        debug!("{}: Received message: {}", self.peer_address, message);
        match serde_json::from_str(message)? {
            MessageToAssistant::Identify(identify) => {
                self.handle_identify(identify).await;
            }
            MessageToAssistant::Ping(_) => {
                self.handle_ping().await;
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

    async fn handle_identify(&mut self, identify: IdentifyMessage) {
        if self.identified {
            self.handle_identify_already_identified().await;
        } else if identify.authentication == self.hash_password() {
            self.handle_identify_correct_password(identify).await;
        } else {
            self.handle_identify_wrong_password().await;
        };
    }

    async fn handle_identify_already_identified(&mut self) {
        debug!("{}: Streamer already identified", self.peer_address);
        self.send_identified(IdentifiedResult::AlreadyIdentified {})
            .await;
    }

    async fn handle_identify_correct_password(&mut self, identify: IdentifyMessage) {
        debug!("{}: Streamer identified", self.peer_address);
        self.identified = true;
        self.streamer_id = identify.streamer_id;
        if let Some(ref streamer_id) = self.streamer_id {
            let mut streamers = self.streamers.lock().await;
            if let Some(streamer) = streamers.remove(streamer_id) {
                match streamer {
                    Streamer::Connected(streamer) => {
                        if let Some(streamer) = streamer.upgrade() {
                            let mut streamer = streamer.lock().await;
                            self.state = streamer.state.take();
                            streamer.writer.lock().await.close().await.ok();
                        }
                    }
                    Streamer::Disconnected(streamer) => {
                        self.state = Some(streamer.state);
                    }
                }
            }
            if let Some(ref state) = self.state {
                state.lock().await.writer = Some(self.writer.clone());
            } else {
                self.state = Some(StreamerState::new(self.writer.clone()));
            }
            streamers.insert(streamer_id.clone(), Streamer::Connected(self.me.clone()));
        }
        self.send_identified(IdentifiedResult::Ok {}).await;
        if let Some(ref state) = self.state {
            state.lock().await.send_chat_history().await;
        }
    }

    async fn handle_identify_wrong_password(&mut self) {
        info!("{}: Wrong password from streamer", self.peer_address);
        self.send_identified(IdentifiedResult::WrongPassword {})
            .await;
    }

    async fn send_identified(&mut self, result: IdentifiedResult) {
        self.send(MessageToStreamer::Identified(IdentifiedMessage { result }))
            .await;
    }

    async fn handle_ping(&mut self) {
        self.send(MessageToStreamer::Pong {}).await;
    }

    async fn handle_event(&mut self) {
        if !self.identified {
            return;
        }
        debug!("{}: Received event from streamer", self.peer_address);
    }

    async fn handle_twitch_start(&mut self, twitch_start: TwitchStartMessage) {
        if !self.identified {
            return;
        }
        let Some(ref state_shared) = self.state else {
            return;
        };
        let mut state = state_shared.lock().await;
        if state.twitch_chat.is_some() {
            return;
        }
        let Some(channel_name) = &twitch_start.channel_name else {
            return;
        };
        state.twitch_chat = Some(tokio::spawn(twitch::setup_twitch_chat(
            state.me.clone(),
            channel_name.to_lowercase(),
        )));
    }

    async fn handle_youtube_start(&mut self, youtube_start: YouTubeStartMessage) {
        if !self.identified {
            return;
        }
        let Some(ref state_shared) = self.state else {
            return;
        };
        let mut state = state_shared.lock().await;
        if state.youtube_chat.is_some() {
            return;
        }
        state.youtube_chat = Some(tokio::spawn(youtube::setup_youtube_chat(
            state.me.clone(),
            youtube_start.video_id.clone(),
        )));
    }

    async fn handle_response(&mut self) {
        if !self.identified {
            return;
        }
        debug!("{}: Received response from streamer", self.peer_address);
    }

    async fn stop(&mut self) {
        if !self.identified {
            return;
        }
        let Some(ref streamer_id) = self.streamer_id else {
            return;
        };
        let mut streamers = self.streamers.lock().await;
        let Some(state) = self.state.take() else {
            return;
        };
        state.lock().await.writer = None;
        streamers.insert(
            streamer_id.clone(),
            Streamer::Disconnected(DisconnectedStreamer::new(state)),
        );
    }
}

pub async fn handle_streamer(
    stream: tokio::net::TcpStream,
    peer_address: String,
    password: String,
    streamers: Streamers,
) {
    let Ok(stream) = accept_async(stream).await else {
        return;
    };
    info!("{peer_address}: Streamer connected");
    let (writer, mut reader) = stream.split();
    let writer = Arc::new(Mutex::new(writer));
    let streamer =
        StreamerConnection::new(password, peer_address.clone(), writer.clone(), streamers);
    streamer.lock().await.send_hello().await;
    while let Some(Ok(message)) = reader.next().await {
        match message {
            Message::Text(text) => {
                if let Err(e) = streamer.lock().await.handle_message(&text).await {
                    info!("{peer_address}: Error handling text: {e}");
                    break;
                }
            }
            Message::Ping(data) => {
                if let Err(e) = handle_ping(data, &writer).await {
                    info!("{peer_address}: Error handling ping: {e}");
                    break;
                }
            }
            Message::Close(_) => {
                debug!("{peer_address}: Connection closed by streamer");
                break;
            }
            _ => {}
        }
    }
    streamer.lock().await.stop().await;
    info!("{peer_address}: Streamer disconnected");
}

async fn handle_ping(data: Vec<u8>, writer: &WebsocketWriter) -> Result<(), AnyError> {
    let mut writer = writer.lock().await;
    writer.send(Message::Pong(data)).await?;
    Ok(())
}
