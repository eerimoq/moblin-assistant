use log::info;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{Emote, ServerMessage};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

use crate::protocol::{ChatMessage, ChatPostSegment, Platform, RgbColor};
use crate::streamer::StreamerState;

fn create_segments(message_text: &str, emotes: &[Emote]) -> Vec<ChatPostSegment> {
    let mut segments = Vec::new();
    let mut id = 0;
    let chars: Vec<char> = message_text.chars().collect();
    let mut sorted_emotes: Vec<&Emote> = emotes.iter().collect();
    sorted_emotes.sort_by_key(|e| e.char_range.start);
    let mut start_index = 0;
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

pub async fn setup_twitch_chat(streamer: Weak<Mutex<StreamerState>>, channel_name: String) {
    let credentials = StaticLoginCredentials::anonymous();
    let config = ClientConfig::new_simple(credentials);
    let (mut messages, client) =
        TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);
    match client.join(channel_name.clone()) {
        Ok(_) => {
            info!("Joined channel #{channel_name}");
            while let Some(message) = messages.recv().await {
                let Some(ref streamer) = streamer.upgrade() else {
                    break;
                };
                handle_message(streamer, message).await;
            }
            info!("Left channel #{channel_name}");
        }
        Err(e) => {
            info!("Failed to join channel #{channel_name}: {e}");
        }
    }
    if let Some(ref streamer) = streamer.upgrade() {
        streamer.lock().await.destroy_twitch_chat();
    }
}

async fn handle_message(streamer: &Arc<Mutex<StreamerState>>, message: ServerMessage) {
    let ServerMessage::Privmsg(message) = message else {
        return;
    };
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
    let segments = create_segments(&message.message_text, &message.emotes);
    let mut streamer = streamer.lock().await;
    let message = ChatMessage {
        id: streamer.next_chat_message_id(),
        platform: Platform::Twitch {},
        message_id: Some(message.message_id.clone()),
        display_name: Some(message.sender.name.clone()),
        user: Some(message.sender.login.clone()),
        user_id: Some(message.sender.id.clone()),
        user_color,
        user_badges,
        segments,
        timestamp: message.server_timestamp.to_rfc3339(),
        is_action: message.is_action,
        is_moderator,
        is_subscriber,
        is_owner,
        bits: message.bits.map(|bits| bits.to_string()),
    };
    streamer.append_chat_message(message).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_irc::message::Emote;

    #[test]
    fn test_no_emotes() {
        let segments = create_segments("hello world", &[]);
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
        let segments = create_segments("Kappa", &emotes);
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
        let segments = create_segments("hello Kappa world", &emotes);
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].text.as_deref(), Some("hello "));
        assert!(segments[1].url.is_some());
        assert_eq!(segments[2].text.as_deref(), Some(""));
        assert_eq!(segments[3].text.as_deref(), Some("world "));
    }

    #[test]
    fn test_multiple_emotes() {
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
        let segments = create_segments("Kappa Keepo Kappa", &emotes);
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
        let emotes = vec![Emote {
            id: "483".to_string(),
            char_range: 2..4,
            code: "<3".to_string(),
        }];
        let segments = create_segments("👉 <3 test", &emotes);
        assert_eq!(segments[0].text.as_deref(), Some("👉 "));
        assert!(segments[1].url.is_some());
        assert_eq!(segments[2].text.as_deref(), Some(""));
        assert_eq!(segments[3].text.as_deref(), Some("test "));
    }

    #[test]
    fn test_empty_message() {
        let segments = create_segments("", &[]);
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
        let segments = create_segments("pajaM_TK", &emotes);
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
        let segments = create_segments("Kappa Keepo Kappa Kappa test", &emotes);
        let url_count = segments.iter().filter(|s| s.url.is_some()).count();
        assert_eq!(url_count, 4);
        let last = segments.last().unwrap();
        assert_eq!(last.text.as_deref(), Some("test "));
    }
}
