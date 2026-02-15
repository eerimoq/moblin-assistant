use serde::{Deserialize, Serialize};

pub const API_VERSION: &str = "0.1";

#[derive(Debug, Serialize, Deserialize)]
pub struct Authentication {
    pub challenge: String,
    pub salt: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloMessage {
    pub api_version: String,
    pub authentication: Authentication,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IdentifiedResult {
    Ok {},
    WrongPassword {},
    AlreadyIdentified {},
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IdentifiedMessage {
    pub result: IdentifiedResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Platform {
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
pub struct RgbColor {
    pub red: i32,
    pub green: i32,
    pub blue: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatPostSegment {
    pub id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: i32,
    pub platform: Platform,
    pub message_id: Option<String>,
    pub display_name: Option<String>,
    pub user: Option<String>,
    pub user_id: Option<String>,
    pub user_color: Option<RgbColor>,
    pub user_badges: Vec<String>,
    pub segments: Vec<ChatPostSegment>,
    pub timestamp: String,
    pub is_action: bool,
    pub is_moderator: bool,
    pub is_subscriber: bool,
    pub is_owner: bool,
    pub bits: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatMessagesRequest {
    pub history: bool,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestData {
    ChatMessages(ChatMessagesRequest),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestMessage {
    pub id: i32,
    pub data: RequestData,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifyMessage {
    pub authentication: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TwitchStartMessage {
    pub channel_name: Option<String>,
    pub channel_id: String,
    pub access_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeStartMessage {
    pub video_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageToAssistant {
    Identify(IdentifyMessage),
    Ping(serde_json::Value),
    Event(serde_json::Value),
    TwitchStart(TwitchStartMessage),
    YouTubeStart(YouTubeStartMessage),
    Response(serde_json::Value),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageToStreamer {
    Hello(HelloMessage),
    Identified(IdentifiedMessage),
    Request(RequestMessage),
    Pong {},
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_identify_message() {
        let json = r#"{"identify": {"authentication": "abc123"}}"#;
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        match msg {
            MessageToAssistant::Identify(data) => {
                assert_eq!(data.authentication, "abc123");
            }
            _ => panic!("Expected Identify variant"),
        }
    }

    #[test]
    fn test_deserialize_ping_message() {
        let json = r#"{"ping": {}}"#;
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, MessageToAssistant::Ping(_)));
    }

    #[test]
    fn test_deserialize_event_message() {
        let json = r#"{"event": {"type": "someEvent"}}"#;
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, MessageToAssistant::Event(_)));
    }

    #[test]
    fn test_deserialize_twitch_start_message() {
        let json = r#"{"twitchStart": {"channelName": "mychannel", "channelId": "123", "accessToken": "encrypted"}}"#;
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        match msg {
            MessageToAssistant::TwitchStart(data) => {
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
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        match msg {
            MessageToAssistant::TwitchStart(data) => {
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
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, MessageToAssistant::Response(_)));
    }

    #[test]
    fn test_deserialize_unknown_message() {
        let json = r#"{"unknown": {}}"#;
        let result: Result<MessageToAssistant, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_youtube_start_message() {
        let json = r#"{"youTubeStart": {"videoId": "abc123"}}"#;
        let msg: MessageToAssistant = serde_json::from_str(json).unwrap();
        match msg {
            MessageToAssistant::YouTubeStart(data) => {
                assert_eq!(data.video_id, "abc123");
            }
            _ => panic!("Expected YouTubeStart variant"),
        }
    }

    #[test]
    fn test_serialize_hello_message() {
        let message = MessageToStreamer::Hello(HelloMessage {
            api_version: API_VERSION.to_string(),
            authentication: Authentication {
                challenge: "foo".to_string(),
                salt: "bar".to_string(),
            },
        });
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"hello":{"apiVersion":"0.1","authentication":{"challenge":"foo","salt":"bar"}}}"#
        );
    }

    #[test]
    fn test_serialize_pong_message() {
        let message = MessageToStreamer::Pong {};
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"pong":{}}"#);
    }
}
