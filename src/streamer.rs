use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::SinkExt;
use log::{debug, error, info};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{
    Authentication, ChatMessage, ChatMessagesRequest, HelloMessage, IdentifiedMessage,
    IdentifiedResult, IdentifyMessage, MessageToAssistant, MessageToStreamer, RequestData,
    RequestMessage, TwitchStartMessage, YouTubeStartMessage, API_VERSION,
};
use crate::utils::{random_string, AnyError};
use crate::{twitch, youtube, ActiveStreamers, DisconnectedStreamers, WebsocketWriter};

pub struct StreamerState {
    writer: WebsocketWriter,
    request_id: i32,
    chat_message_id: i32,
    chat_messages: VecDeque<ChatMessage>,
    pub twitch_running: bool,
    pub youtube_running: bool,
}

impl StreamerState {
    pub fn new(writer: WebsocketWriter) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            writer,
            request_id: 0,
            chat_message_id: 0,
            chat_messages: VecDeque::new(),
            twitch_running: false,
            youtube_running: false,
        }))
    }

    pub fn next_request_id(&mut self) -> i32 {
        self.request_id += 1;
        self.request_id
    }

    pub fn next_chat_message_id(&mut self) -> i32 {
        self.chat_message_id += 1;
        self.chat_message_id
    }

    pub fn store_chat_message(&mut self, message: ChatMessage) {
        if self.chat_messages.len() >= 100 {
            self.chat_messages.pop_front();
        }
        self.chat_messages.push_back(message);
    }

    pub async fn send_chat_history(&mut self) {
        let request = MessageToStreamer::Request(RequestMessage {
            id: self.next_request_id(),
            data: RequestData::ChatMessages(ChatMessagesRequest {
                history: true,
                messages: self.chat_messages.clone().into(),
            }),
        });
        if let Ok(encoded) = serde_json::to_string(&request) {
            self.send(Message::Text(encoded)).await;
        }
    }

    async fn send(&self, message: Message) {
        self.writer.lock().await.send(message).await.ok();
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
        if let Ok(encoded) = serde_json::to_string(&request) {
            self.send(Message::Text(encoded)).await;
        }
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
    disconnected_streamers: DisconnectedStreamers,
    active_streamers: ActiveStreamers,
    pub state: Option<Arc<Mutex<StreamerState>>>,
}

impl StreamerConnection {
    pub fn new(
        password: String,
        peer_address: String,
        writer: WebsocketWriter,
        disconnected_streamers: DisconnectedStreamers,
        active_streamers: ActiveStreamers,
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
                disconnected_streamers,
                active_streamers,
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

    pub async fn send_hello(&mut self) {
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
        if let Ok(encoded) = serde_json::to_string(&message) {
            self.writer
                .lock()
                .await
                .send(Message::Text(encoded))
                .await
                .ok();
        }
    }

    pub async fn handle_message(&mut self, message: &str) -> Result<(), AnyError> {
        debug!("[{}] Received message: {}", self.peer_address, message);
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

    pub async fn handle_identify(&mut self, identify: IdentifyMessage) {
        let result = if self.identified {
            self.handle_identify_already_identified()
        } else if identify.authentication == self.hash_password() {
            self.handle_identify_corrent_password(identify).await
        } else {
            self.handle_identify_wrong_password()
        };
        self.send(MessageToStreamer::Identified(IdentifiedMessage { result }))
            .await;
        if let Some(ref state) = self.state {
            state.lock().await.send_chat_history().await;
        }
    }

    fn handle_identify_already_identified(&self) -> IdentifiedResult {
        debug!("[{}] Streamer already identified", self.peer_address);
        IdentifiedResult::AlreadyIdentified {}
    }

    async fn handle_identify_corrent_password(
        &mut self,
        identify: IdentifyMessage,
    ) -> IdentifiedResult {
        info!("[{}] Streamer successfully identified", self.peer_address);
        self.identified = true;
        self.streamer_id = identify.streamer_id;
        if let Some(ref streamer_id) = self.streamer_id {
            if let Some(streamer) = self
                .active_streamers
                .lock()
                .await
                .remove(streamer_id)
                .and_then(|streamer| streamer.upgrade())
            {
                self.state = streamer.lock().await.state.take();
            } else if let Some((state, _)) =
                self.disconnected_streamers.lock().await.remove(streamer_id)
            {
                self.state = Some(state);
            }
            if let Some(ref state) = self.state {
                state.lock().await.writer = self.writer.clone();
            } else {
                self.state = Some(StreamerState::new(self.writer.clone()));
            }
            self.active_streamers
                .lock()
                .await
                .insert(streamer_id.clone(), self.me.clone());
        }
        IdentifiedResult::Ok {}
    }

    fn handle_identify_wrong_password(&self) -> IdentifiedResult {
        error!("[{}] Wrong password from streamer", self.peer_address);
        IdentifiedResult::WrongPassword {}
    }

    async fn handle_ping(&mut self) {
        self.send(MessageToStreamer::Pong {}).await;
    }

    async fn handle_event(&mut self) {
        if !self.identified {
            return;
        }
        debug!("[{}] Received event from streamer", self.peer_address);
    }

    async fn handle_twitch_start(&mut self, twitch_start: TwitchStartMessage) {
        if !self.identified {
            return;
        }
        let Some(ref state_shared) = self.state else {
            return;
        };
        let mut state = state_shared.lock().await;
        if state.twitch_running {
            info!(
                "[{}] Twitch connection already running, ignoring start",
                self.peer_address
            );
            return;
        }
        if let Some(channel_name) = &twitch_start.channel_name {
            info!(
                "[{}] Starting Twitch IRC connection for channel: {}",
                self.peer_address, channel_name
            );
            state.twitch_running = true;
            tokio::spawn(twitch::connect_twitch_irc(
                state_shared.clone(),
                channel_name.to_lowercase(),
                self.peer_address.clone(),
            ));
        }
    }

    async fn handle_youtube_start(&mut self, youtube_start: YouTubeStartMessage) {
        if !self.identified {
            return;
        }
        let Some(ref state_shared) = self.state else {
            return;
        };
        let mut state = state_shared.lock().await;
        if state.youtube_running {
            info!(
                "[{}] YouTube connection already running, ignoring start",
                self.peer_address
            );
            return;
        }
        info!(
            "[{}] Starting YouTube chat for video: {}",
            self.peer_address, youtube_start.video_id
        );
        state.youtube_running = true;
        tokio::spawn(youtube::connect_youtube_chat(
            state_shared.clone(),
            youtube_start.video_id.clone(),
            self.peer_address.clone(),
        ));
    }

    async fn handle_response(&mut self) {
        if !self.identified {
            return;
        }
        debug!("[{}] Received response from streamer", self.peer_address);
    }

    pub async fn stop(&mut self) {
        if !self.identified {
            return;
        }
        let Some(ref streamer_id) = self.streamer_id else {
            return;
        };
        self.active_streamers.lock().await.remove(streamer_id);
        let Some(ref state) = self.state else {
            return;
        };
        self.disconnected_streamers
            .lock()
            .await
            .insert(streamer_id.clone(), (state.clone(), Instant::now()));
    }
}
